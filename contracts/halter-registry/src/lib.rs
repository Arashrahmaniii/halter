//! Halter Registry: a shared, evidence-backed blocklist for paying agents.
//!
//! Anyone (a human, an agent's Halter wallet, a monitoring bot) can `report` an account with
//! evidence. Reports are bonded with a small deposit so spam costs money. Curators review the
//! evidence and `confirm` (-> Blocked) or `clear` (-> Cleared) the account.
//!
//! Halter wallets call `check` cross-contract before every outgoing payment:
//! - `Blocked`  -> the wallet refuses the payment on-chain.
//! - `Flagged`  -> the wallet parks the payment for human approval.
//! - `Unknown` / `Cleared` -> no registry objection.

use near_sdk::store::{IterableMap, IterableSet, LookupMap};
use near_sdk::{env, near, require, AccountId, BorshStorageKey, NearToken, PanicOnDefault, Promise};

/// Bond attached to every report. Returned when a curator confirms the report, kept by the
/// registry (spam fee) when the account is cleared.
pub const REPORT_BOND: NearToken = NearToken::from_millinear(10); // 0.01 NEAR
pub const MAX_EVIDENCE_LEN: usize = 512;
pub const MAX_NOTE_LEN: usize = 280;
pub const MAX_REPORTS_PER_ACCOUNT: u32 = 50;

#[near(serializers = [borsh])]
#[derive(BorshStorageKey)]
enum StorageKey {
    Curators,
    Entries,
    Reports,
}

#[near(serializers = [json, borsh])]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// No reports.
    Unknown,
    /// At least one open report, not reviewed yet.
    Flagged,
    /// A curator confirmed the evidence.
    Blocked,
    /// A curator reviewed the reports and found the account legitimate.
    Cleared,
}

#[near(serializers = [json, borsh])]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    Drainer,
    Phishing,
    Impersonation,
    PromptInjection,
    Sanctioned,
    Other,
}

#[near(serializers = [json, borsh])]
#[derive(Clone, Debug)]
pub struct Report {
    pub reporter: AccountId,
    pub category: Category,
    /// Link or transaction hash backing the claim.
    pub evidence: String,
    pub note: String,
    pub created_at_ms: u64,
    pub bond: NearToken,
}

#[near(serializers = [json, borsh])]
#[derive(Clone, Debug)]
pub struct Entry {
    pub status: Status,
    pub open_reports: u32,
    pub total_reports: u32,
    pub reviewed_by: Option<AccountId>,
    pub review_reason: Option<String>,
    pub updated_at_ms: u64,
}

/// What `check` returns. Kept small and stable: Halter wallets decode it in a callback.
#[near(serializers = [json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    pub account_id: AccountId,
    pub status: Status,
    pub open_reports: u32,
    pub total_reports: u32,
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Registry {
    owner: AccountId,
    curators: IterableSet<AccountId>,
    entries: IterableMap<AccountId, Entry>,
    reports: LookupMap<AccountId, Vec<Report>>,
}

fn empty_entry(now: u64) -> Entry {
    Entry {
        status: Status::Unknown,
        open_reports: 0,
        total_reports: 0,
        reviewed_by: None,
        review_reason: None,
        updated_at_ms: now,
    }
}

#[near]
impl Registry {
    #[init]
    pub fn new(owner: AccountId, curators: Vec<AccountId>) -> Self {
        let mut set = IterableSet::new(StorageKey::Curators);
        set.insert(owner.clone());
        for c in curators {
            set.insert(c);
        }
        Self {
            owner,
            curators: set,
            entries: IterableMap::new(StorageKey::Entries),
            reports: LookupMap::new(StorageKey::Reports),
        }
    }

    // ---------------------------------------------------------------- reporting

    /// Report an account. Attach at least `REPORT_BOND` (0.01 NEAR).
    #[payable]
    pub fn report(
        &mut self,
        account_id: AccountId,
        category: Category,
        evidence: String,
        note: Option<String>,
    ) -> Verdict {
        let bond = env::attached_deposit();
        require!(bond >= REPORT_BOND, "Attach at least 0.01 NEAR as a report bond");
        require!(!evidence.trim().is_empty(), "Evidence is required");
        require!(evidence.len() <= MAX_EVIDENCE_LEN, "Evidence too long");
        let note = note.unwrap_or_default();
        require!(note.len() <= MAX_NOTE_LEN, "Note too long");
        let reporter = env::predecessor_account_id();
        require!(reporter != account_id, "Cannot report yourself");

        let now = env::block_timestamp_ms();
        let mut list = self.reports.get(&account_id).cloned().unwrap_or_default();
        require!(
            (list.len() as u32) < MAX_REPORTS_PER_ACCOUNT,
            "Report limit reached for this account; a curator must review it"
        );
        require!(
            !list.iter().any(|r| r.reporter == reporter),
            "You already reported this account"
        );
        list.push(Report {
            reporter: reporter.clone(),
            category,
            evidence: evidence.clone(),
            note,
            created_at_ms: now,
            bond,
        });
        self.reports.insert(account_id.clone(), list);

        let mut entry = self.entries.get(&account_id).cloned().unwrap_or_else(|| empty_entry(now));
        entry.open_reports += 1;
        entry.total_reports += 1;
        // A cleared account that gets new reports goes back to Flagged. A blocked one stays blocked.
        if entry.status != Status::Blocked {
            entry.status = Status::Flagged;
        }
        entry.updated_at_ms = now;
        self.entries.insert(account_id.clone(), entry);

        emit(
            "report",
            near_sdk::serde_json::json!({
                "account_id": account_id,
                "reporter": reporter,
                "category": category,
                "evidence": evidence,
            }),
        );
        self.check(account_id)
    }

    // ---------------------------------------------------------------- curation

    /// Confirm the reports: the account becomes Blocked and reporters get their bonds back.
    pub fn confirm(&mut self, account_id: AccountId, reason: String) -> Verdict {
        self.assert_curator();
        self.review(account_id.clone(), Status::Blocked, reason, true);
        self.check(account_id)
    }

    /// Clear the account: it becomes Cleared and the open bonds are kept as a spam fee.
    pub fn clear(&mut self, account_id: AccountId, reason: String) -> Verdict {
        self.assert_curator();
        self.review(account_id.clone(), Status::Cleared, reason, false);
        self.check(account_id)
    }

    /// Curators can block an account directly (for example from a public hack post-mortem).
    pub fn block(&mut self, account_id: AccountId, reason: String) -> Verdict {
        self.assert_curator();
        self.review(account_id.clone(), Status::Blocked, reason, true);
        self.check(account_id)
    }

    pub fn add_curator(&mut self, account_id: AccountId) {
        self.assert_owner();
        self.curators.insert(account_id);
    }

    pub fn remove_curator(&mut self, account_id: AccountId) {
        self.assert_owner();
        require!(account_id != self.owner, "Owner is always a curator");
        self.curators.remove(&account_id);
    }

    // ---------------------------------------------------------------- views

    /// The verdict Halter wallets consume. Never panics.
    pub fn check(&self, account_id: AccountId) -> Verdict {
        let e = self.entries.get(&account_id).cloned().unwrap_or_else(|| empty_entry(0));
        Verdict {
            account_id,
            status: e.status,
            open_reports: e.open_reports,
            total_reports: e.total_reports,
        }
    }

    pub fn get_entry(&self, account_id: AccountId) -> Option<Entry> {
        self.entries.get(&account_id).cloned()
    }

    pub fn get_reports(&self, account_id: AccountId) -> Vec<Report> {
        self.reports.get(&account_id).cloned().unwrap_or_default()
    }

    /// Paginated list of every account the registry knows about.
    pub fn list_entries(&self, from_index: Option<u32>, limit: Option<u32>) -> Vec<(AccountId, Entry)> {
        let from = from_index.unwrap_or(0) as usize;
        let limit = limit.unwrap_or(50).min(100) as usize;
        self.entries
            .iter()
            .skip(from)
            .take(limit)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    pub fn get_curators(&self) -> Vec<AccountId> {
        self.curators.iter().cloned().collect()
    }

    pub fn get_owner(&self) -> AccountId {
        self.owner.clone()
    }

    // ---------------------------------------------------------------- internals

    fn review(&mut self, account_id: AccountId, status: Status, reason: String, refund: bool) {
        require!(reason.len() <= MAX_NOTE_LEN, "Reason too long");
        let now = env::block_timestamp_ms();
        let curator = env::predecessor_account_id();
        let mut entry = self.entries.get(&account_id).cloned().unwrap_or_else(|| empty_entry(now));
        if refund && entry.open_reports > 0 {
            if let Some(list) = self.reports.get(&account_id) {
                let start = list.len().saturating_sub(entry.open_reports as usize);
                for r in &list[start..] {
                    Promise::new(r.reporter.clone()).transfer(r.bond).detach();
                }
            }
        }
        entry.status = status;
        entry.open_reports = 0;
        entry.reviewed_by = Some(curator.clone());
        entry.review_reason = Some(reason.clone());
        entry.updated_at_ms = now;
        self.entries.insert(account_id.clone(), entry);
        emit(
            "review",
            near_sdk::serde_json::json!({
                "account_id": account_id,
                "status": status,
                "curator": curator,
                "reason": reason,
            }),
        );
    }

    fn assert_owner(&self) {
        require!(env::predecessor_account_id() == self.owner, "Only the owner");
    }

    fn assert_curator(&self) {
        require!(
            self.curators.contains(&env::predecessor_account_id()),
            "Only a curator"
        );
    }
}

/// NEP-297 event log.
fn emit(event: &str, data: near_sdk::serde_json::Value) {
    env::log_str(&format!(
        "EVENT_JSON:{}",
        near_sdk::serde_json::json!({
            "standard": "halter-registry",
            "version": "1.0.0",
            "event": event,
            "data": [data],
        })
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use near_sdk::test_utils::VMContextBuilder;
    use near_sdk::testing_env;

    fn acc(s: &str) -> AccountId {
        s.parse().unwrap()
    }

    fn ctx(predecessor: &str, deposit: NearToken) {
        let mut b = VMContextBuilder::new();
        b.current_account_id(acc("registry.testnet"))
            .predecessor_account_id(acc(predecessor))
            .signer_account_id(acc(predecessor))
            .attached_deposit(deposit)
            .block_timestamp(1_700_000_000_000_000_000);
        testing_env!(b.build());
    }

    fn setup() -> Registry {
        ctx("owner.testnet", NearToken::from_yoctonear(0));
        Registry::new(acc("owner.testnet"), vec![acc("curator.testnet")])
    }

    #[test]
    fn unknown_by_default() {
        let r = setup();
        assert_eq!(r.check(acc("bob.testnet")).status, Status::Unknown);
    }

    #[test]
    fn report_flags_then_confirm_blocks() {
        let mut r = setup();
        ctx("alice.testnet", REPORT_BOND);
        let v = r.report(acc("drainer.testnet"), Category::Drainer, "tx:abc".into(), None);
        assert_eq!(v.status, Status::Flagged);
        assert_eq!(v.open_reports, 1);

        ctx("curator.testnet", NearToken::from_yoctonear(0));
        let v = r.confirm(acc("drainer.testnet"), "verified drain tx".into());
        assert_eq!(v.status, Status::Blocked);
        assert_eq!(v.open_reports, 0);
        assert_eq!(v.total_reports, 1);
    }

    #[test]
    fn clear_then_new_report_reflags() {
        let mut r = setup();
        ctx("alice.testnet", REPORT_BOND);
        r.report(acc("shop.testnet"), Category::Phishing, "https://x".into(), None);
        ctx("owner.testnet", NearToken::from_yoctonear(0));
        assert_eq!(r.clear(acc("shop.testnet"), "legit".into()).status, Status::Cleared);
        ctx("carol.testnet", REPORT_BOND);
        assert_eq!(
            r.report(acc("shop.testnet"), Category::Phishing, "https://y".into(), None).status,
            Status::Flagged
        );
    }

    #[test]
    fn blocked_stays_blocked_on_new_report() {
        let mut r = setup();
        ctx("owner.testnet", NearToken::from_yoctonear(0));
        r.block(acc("evil.testnet"), "known drainer".into());
        ctx("alice.testnet", REPORT_BOND);
        assert_eq!(
            r.report(acc("evil.testnet"), Category::Drainer, "tx:1".into(), None).status,
            Status::Blocked
        );
    }

    #[test]
    #[should_panic(expected = "report bond")]
    fn report_needs_bond() {
        let mut r = setup();
        ctx("alice.testnet", NearToken::from_yoctonear(1));
        r.report(acc("x.testnet"), Category::Other, "e".into(), None);
    }

    #[test]
    #[should_panic(expected = "already reported")]
    fn one_report_per_reporter() {
        let mut r = setup();
        ctx("alice.testnet", REPORT_BOND);
        r.report(acc("x.testnet"), Category::Other, "e".into(), None);
        r.report(acc("x.testnet"), Category::Other, "e2".into(), None);
    }

    #[test]
    #[should_panic(expected = "Evidence is required")]
    fn evidence_required() {
        let mut r = setup();
        ctx("alice.testnet", REPORT_BOND);
        r.report(acc("x.testnet"), Category::Other, "  ".into(), None);
    }

    #[test]
    #[should_panic(expected = "Only a curator")]
    fn non_curator_cannot_confirm() {
        let mut r = setup();
        ctx("mallory.testnet", NearToken::from_yoctonear(0));
        r.confirm(acc("x.testnet"), "no".into());
    }

    #[test]
    #[should_panic(expected = "Only the owner")]
    fn only_owner_adds_curators() {
        let mut r = setup();
        ctx("curator.testnet", NearToken::from_yoctonear(0));
        r.add_curator(acc("mallory.testnet"));
    }

    #[test]
    fn list_entries_paginates() {
        let mut r = setup();
        for i in 0..5 {
            ctx(&format!("rep{i}.testnet"), REPORT_BOND);
            r.report(acc(&format!("bad{i}.testnet")), Category::Other, "e".into(), None);
        }
        assert_eq!(r.list_entries(None, Some(3)).len(), 3);
        assert_eq!(r.list_entries(Some(3), Some(10)).len(), 2);
    }
}
