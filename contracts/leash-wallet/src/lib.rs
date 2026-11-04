//! Leash Wallet: a policy-enforcing NEAR account for AI agents.
//!
//! Deploy this contract on the agent's own account, give the agent a function-call key that may
//! only call `execute` / `propose` / `report_scam` on this account, and delete the account's
//! full-access key. From then on the agent can move value in exactly one way: through
//! `execute`, which applies the owner's policy on-chain before any token leaves:
//!
//! * per-asset caps (per transaction, rolling 24h window, approval threshold)
//! * receiver rules (allowlist mode, local denylist, human approval for first-time receivers)
//! * contract-call allowlist (contract + method + max deposit + max gas)
//! * rate limit (actions per rolling hour)
//! * a shared scam registry checked cross-contract before every payment (fail-closed)
//! * a kill switch the owner or any guardian can pull
//!
//! Payments that need a human are parked as pending actions the owner approves or rejects.
//! Every decision (allowed, pending, blocked, settled, failed) is logged as a NEP-297 event and
//! kept in a short on-chain history.

use std::collections::BTreeMap;

use near_sdk::json_types::{Base64VecU8, U128, U64};
use near_sdk::serde_json::{self, json};
use near_sdk::store::{IterableMap, IterableSet, LookupMap, LookupSet};
use near_sdk::{
    env, near, require, AccountId, Allowance, BorshStorageKey, Gas, NearToken, PanicOnDefault,
    Promise, PromiseError, PromiseOrValue, PublicKey,
};

pub const NEAR_ASSET: &str = "near";
pub const AGENT_METHODS: &str = "execute,propose,report_scam";

const HOUR_MS: u64 = 3_600_000;
const WINDOW_HOURS: u64 = 24;
const HISTORY_LEN: u64 = 100;
const MAX_PENDING: u32 = 20;
const MAX_MEMO_LEN: usize = 256;
const MAX_ACTIONS_PER_HOUR_CAP: u32 = 120;
const DEFAULT_PENDING_TTL_MS: u64 = 24 * HOUR_MS;

const GAS_REGISTRY_CHECK: Gas = Gas::from_tgas(10);
const GAS_FT_TRANSFER: Gas = Gas::from_tgas(15);
const GAS_REPORT: Gas = Gas::from_tgas(15);
const GAS_ON_SETTLED: Gas = Gas::from_tgas(10);
const GAS_CALLBACK_OVERHEAD: Gas = Gas::from_tgas(15);
pub const MAX_CALL_GAS: Gas = Gas::from_tgas(100);
const REPORT_BOND: NearToken = NearToken::from_millinear(10);
const ONE_YOCTO: NearToken = NearToken::from_yoctonear(1);

// ====================================================================== types

#[near(serializers = [borsh])]
#[derive(BorshStorageKey)]
enum StorageKey {
    Guardians,
    AgentKeys,
    Contacts,
    Denylist,
    Seen,
    Spend,
    Pending,
    InFlight,
    History,
}

/// Something the agent wants to do with the wallet's funds.
#[near(serializers = [json, borsh])]
#[serde(tag = "type")]
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Send native NEAR.
    Transfer { receiver_id: AccountId, amount: U128 },
    /// Send a NEP-141 token (the token contract must have a limit in the policy).
    FtTransfer {
        token_id: AccountId,
        receiver_id: AccountId,
        amount: U128,
        memo: Option<String>,
    },
    /// Call a contract that the policy allowlists, optionally with a NEAR deposit.
    Call {
        contract_id: AccountId,
        method_name: String,
        args: Base64VecU8,
        deposit: U128,
        gas: U64,
    },
}

impl Action {
    /// The account that ends up receiving value or the call.
    pub fn counterparty(&self) -> &AccountId {
        match self {
            Action::Transfer { receiver_id, .. } => receiver_id,
            Action::FtTransfer { receiver_id, .. } => receiver_id,
            Action::Call { contract_id, .. } => contract_id,
        }
    }

    /// The asset key used for limits: "near" or the token contract id.
    pub fn asset(&self) -> String {
        match self {
            Action::FtTransfer { token_id, .. } => token_id.to_string(),
            _ => NEAR_ASSET.to_string(),
        }
    }

    pub fn amount(&self) -> u128 {
        match self {
            Action::Transfer { amount, .. } => amount.0,
            Action::FtTransfer { amount, .. } => amount.0,
            Action::Call { deposit, .. } => deposit.0,
        }
    }

    fn dispatch_gas(&self) -> Gas {
        match self {
            Action::Transfer { .. } => Gas::from_tgas(0),
            Action::FtTransfer { .. } => GAS_FT_TRANSFER,
            Action::Call { gas, .. } => Gas::from_gas(gas.0),
        }
    }
}

#[near(serializers = [json, borsh])]
#[derive(Clone, Debug, PartialEq)]
pub struct AssetLimits {
    /// Hard cap for a single action. Above it the agent is refused.
    pub max_per_tx: U128,
    /// Hard cap for the rolling 24h window (hourly buckets, no midnight reset).
    pub daily_limit: U128,
    /// Actions above this amount wait for the owner's approval.
    pub approval_above: U128,
}

#[near(serializers = [json, borsh])]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ReceiverMode {
    /// Any receiver that passes the other checks.
    Any,
    /// Only accounts in the contact list.
    ContactsOnly,
}

#[near(serializers = [json, borsh])]
#[derive(Clone, Debug, PartialEq)]
pub struct CallRule {
    pub contract_id: AccountId,
    /// Allowed method names. Empty means no method is allowed.
    pub methods: Vec<String>,
    pub max_deposit: U128,
}

#[near(serializers = [json, borsh])]
#[derive(Clone, Debug, PartialEq)]
pub struct Policy {
    /// Limits per asset ("near" or a NEP-141 contract id). Assets not listed are refused.
    pub limits: BTreeMap<String, AssetLimits>,
    pub receiver_mode: ReceiverMode,
    /// First payment to an account that is neither a contact nor paid before needs approval.
    pub approve_new_receivers: bool,
    pub max_actions_per_hour: u32,
    /// Shared scam registry to consult before every payment.
    pub registry_id: Option<AccountId>,
    /// Registry "Flagged" (open, unreviewed reports): block instead of asking the owner.
    pub block_flagged: bool,
    pub call_rules: Vec<CallRule>,
    pub pending_ttl_ms: U64,
}

#[near(serializers = [json, borsh])]
#[derive(Clone, Debug)]
pub struct AgentKey {
    pub label: String,
    pub added_at_ms: U64,
}

#[near(serializers = [json, borsh])]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Status {
    /// Refused by policy or registry. Nothing was sent.
    Blocked,
    /// Waiting for the owner.
    Pending,
    /// Waiting for the shared registry's answer.
    Checking,
    /// Sent; waiting for the receipt result.
    Dispatched,
    /// The transfer or call succeeded.
    Settled,
    /// The transfer or call failed; the reservation was released.
    Failed,
    /// The owner rejected the pending action.
    Rejected,
    /// The pending action expired before the owner decided.
    Expired,
}

#[near(serializers = [json, borsh])]
#[derive(Clone, Debug)]
pub struct Decision {
    pub id: U64,
    pub action: Action,
    pub status: Status,
    pub reasons: Vec<String>,
    /// Which agent key (label) asked, or "owner".
    pub requested_by: String,
    pub memo: Option<String>,
    pub created_at_ms: U64,
    pub updated_at_ms: U64,
}

/// Returned by `execute`, `propose`, `approve` and the callbacks.
#[near(serializers = [json])]
#[derive(Clone, Debug, PartialEq)]
pub struct Outcome {
    pub id: U64,
    pub status: Status,
    pub reasons: Vec<String>,
}

#[near(serializers = [json, borsh])]
#[derive(Clone, Debug)]
pub struct Pending {
    pub id: U64,
    pub action: Action,
    pub reasons: Vec<String>,
    pub requested_by: String,
    pub memo: Option<String>,
    pub created_at_ms: U64,
    pub expires_at_ms: U64,
}

/// An action between `execute`/`approve` and its final status.
#[near(serializers = [borsh])]
#[derive(Clone, Debug)]
struct InFlight {
    action: Action,
    hour: u64,
    /// Counted against the window and rate limit (released if it does not go out).
    reserved: bool,
    /// Policy reasons that require the owner, decided before the registry check.
    approval: Vec<String>,
    owner_approved: bool,
}

/// Rolling 24h spend in hourly buckets.
#[near(serializers = [borsh])]
#[derive(Clone, Debug, Default)]
struct SpendWindow {
    buckets: Vec<u128>,
    last_hour: u64,
}

impl SpendWindow {
    fn roll(&mut self, now_hour: u64) {
        if self.buckets.len() != WINDOW_HOURS as usize {
            self.buckets = vec![0; WINDOW_HOURS as usize];
            self.last_hour = now_hour;
            return;
        }
        if now_hour <= self.last_hour {
            return;
        }
        if now_hour - self.last_hour >= WINDOW_HOURS {
            self.buckets.iter_mut().for_each(|b| *b = 0);
        } else {
            for h in (self.last_hour + 1)..=now_hour {
                self.buckets[(h % WINDOW_HOURS) as usize] = 0;
            }
        }
        self.last_hour = now_hour;
    }

    /// Total within the window ending at `now_hour`, without mutating.
    fn total_at(&self, now_hour: u64) -> u128 {
        if self.buckets.len() != WINDOW_HOURS as usize {
            return 0;
        }
        (0..WINDOW_HOURS)
            .filter_map(|age| {
                let h = now_hour.checked_sub(age)?;
                // Bucket for hour h is valid if h is within the stored window.
                if h > self.last_hour || self.last_hour - h >= WINDOW_HOURS {
                    return None;
                }
                Some(self.buckets[(h % WINDOW_HOURS) as usize])
            })
            .sum()
    }

    fn add(&mut self, now_hour: u64, amount: u128) {
        self.roll(now_hour);
        let b = &mut self.buckets[(now_hour % WINDOW_HOURS) as usize];
        *b = b.saturating_add(amount);
    }

    /// Release a reservation made at `hour`, if it is still inside the window.
    fn release(&mut self, now_hour: u64, hour: u64, amount: u128) {
        self.roll(now_hour);
        if hour <= now_hour && now_hour - hour < WINDOW_HOURS {
            let b = &mut self.buckets[(hour % WINDOW_HOURS) as usize];
            *b = b.saturating_sub(amount);
        }
    }
}

/// The subset of the registry's `check` response that the wallet needs.
#[near(serializers = [json])]
#[derive(Clone, Debug)]
pub struct RegistryVerdict {
    pub status: String,
    pub open_reports: u32,
}

#[near(serializers = [json])]
#[derive(Clone, Debug)]
pub struct AssetStatus {
    pub asset: String,
    pub spent_24h: U128,
    pub remaining_24h: U128,
    pub limits: AssetLimits,
}

#[near(serializers = [json])]
#[derive(Clone, Debug)]
pub struct WalletStatus {
    pub owner_id: AccountId,
    pub frozen: bool,
    pub balance: U128,
    pub assets: Vec<AssetStatus>,
    pub actions_last_hour: u32,
    pub max_actions_per_hour: u32,
    pub pending: u32,
    pub agent_keys: Vec<(PublicKey, AgentKey)>,
    pub guardians: Vec<AccountId>,
    pub next_id: U64,
}

/// Result of the pure policy evaluation.
#[derive(Debug, PartialEq)]
enum Verdict {
    Allow,
    NeedsApproval(Vec<String>),
    Block(Vec<String>),
}

// ====================================================================== contract

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct LeashWallet {
    owner_id: AccountId,
    guardians: IterableSet<AccountId>,
    policy: Policy,
    frozen: bool,
    agent_keys: IterableMap<PublicKey, AgentKey>,
    contacts: IterableSet<AccountId>,
    denylist: IterableSet<AccountId>,
    seen: LookupSet<AccountId>,
    spend: LookupMap<String, SpendWindow>,
    recent_actions: Vec<u64>,
    pending: IterableMap<u64, Pending>,
    in_flight: LookupMap<u64, InFlight>,
    history: LookupMap<u64, Decision>,
    next_id: u64,
}

pub fn default_policy(registry_id: Option<AccountId>) -> Policy {
    let mut limits = BTreeMap::new();
    limits.insert(
        NEAR_ASSET.to_string(),
        AssetLimits {
            max_per_tx: U128(NearToken::from_near(1).as_yoctonear()),
            daily_limit: U128(NearToken::from_near(3).as_yoctonear()),
            approval_above: U128(NearToken::from_millinear(500).as_yoctonear()),
        },
    );
    Policy {
        limits,
        receiver_mode: ReceiverMode::Any,
        approve_new_receivers: true,
        max_actions_per_hour: 30,
        registry_id,
        block_flagged: false,
        call_rules: vec![],
        pending_ttl_ms: U64(DEFAULT_PENDING_TTL_MS),
    }
}

#[near]
impl LeashWallet {
    #[init]
    pub fn new(
        owner_id: AccountId,
        policy: Option<Policy>,
        registry_id: Option<AccountId>,
        guardians: Option<Vec<AccountId>>,
        contacts: Option<Vec<AccountId>>,
    ) -> Self {
        let policy = policy.unwrap_or_else(|| default_policy(registry_id));
        validate_policy(&policy);
        let mut g = IterableSet::new(StorageKey::Guardians);
        for a in guardians.unwrap_or_default() {
            g.insert(a);
        }
        let mut c = IterableSet::new(StorageKey::Contacts);
        for a in contacts.unwrap_or_default() {
            c.insert(a);
        }
        Self {
            owner_id,
            guardians: g,
            policy,
            frozen: false,
            agent_keys: IterableMap::new(StorageKey::AgentKeys),
            contacts: c,
            denylist: IterableSet::new(StorageKey::Denylist),
            seen: LookupSet::new(StorageKey::Seen),
            spend: LookupMap::new(StorageKey::Spend),
            recent_actions: vec![],
            pending: IterableMap::new(StorageKey::Pending),
            in_flight: LookupMap::new(StorageKey::InFlight),
            history: LookupMap::new(StorageKey::History),
            next_id: 0,
        }
    }

    // ================================================================== agent API

    /// The agent's only way to move value. Signed with a registered function-call key.
    pub fn execute(&mut self, action: Action, memo: Option<String>) -> PromiseOrValue<Outcome> {
        let who = self.assert_agent();
        check_memo(&memo);
        let id = self.take_id();

        let verdict = self.evaluate(&action, true);
        let registry = self.policy.registry_id.clone();
        match (verdict, registry) {
            (Verdict::Block(reasons), _) => PromiseOrValue::Value(self.record_blocked(id, action, reasons, who, memo)),
            (Verdict::NeedsApproval(reasons), None) => PromiseOrValue::Value(self.park(id, action, reasons, who, memo)),
            // Even a payment that will wait for the owner is checked against the registry first,
            // so the owner is never asked to approve a payment to a confirmed scam account.
            (Verdict::NeedsApproval(reasons), Some(registry)) => {
                self.record(id, &action, Status::Checking, reasons.clone(), &who, &memo);
                self.track(id, &action, false, reasons, false);
                self.check_registry(id, registry, &action)
            }
            (Verdict::Allow, None) => {
                self.track(id, &action, true, vec![], false);
                self.record(id, &action, Status::Dispatched, vec![], &who, &memo);
                PromiseOrValue::Promise(self.dispatch(id, &action))
            }
            (Verdict::Allow, Some(registry)) => {
                self.track(id, &action, true, vec![], false);
                self.record(id, &action, Status::Checking, vec![], &who, &memo);
                self.check_registry(id, registry, &action)
            }
        }
    }

    /// The agent asks the owner to approve an action (for example after its own counterparty
    /// check returned "warn"). Hard limits still apply: a blocked action is not parked.
    pub fn propose(&mut self, action: Action, reason: String, memo: Option<String>) -> Outcome {
        let who = self.assert_agent();
        check_memo(&memo);
        require!(reason.len() <= MAX_MEMO_LEN, "Reason too long");
        let id = self.take_id();
        match self.evaluate(&action, false) {
            Verdict::Block(reasons) => self.record_blocked(id, action, reasons, who, memo),
            Verdict::NeedsApproval(mut reasons) => {
                reasons.insert(0, format!("agent asked: {reason}"));
                self.park(id, action, reasons, who, memo)
            }
            Verdict::Allow => self.park(id, action, vec![format!("agent asked: {reason}")], who, memo),
        }
    }

    /// The agent reports a scam account to the shared registry. The 0.01 NEAR bond counts
    /// against the NEAR daily limit and the rate limit.
    pub fn report_scam(&mut self, account_id: AccountId, category: String, evidence: String) -> Promise {
        self.assert_agent();
        require!(!self.frozen, "Wallet is frozen");
        let registry = self.policy.registry_id.clone().expect("No registry configured");
        let probe = Action::Transfer {
            receiver_id: registry.clone(),
            amount: U128(REPORT_BOND.as_yoctonear()),
        };
        let now_hour = now_hour();
        let mut reasons = vec![];
        self.check_amount_limits(&probe, now_hour, &mut reasons);
        self.check_rate(&mut reasons);
        require!(reasons.is_empty(), reasons.join("; "));
        self.add_spend(NEAR_ASSET, now_hour, REPORT_BOND.as_yoctonear());
        self.recent_actions.push(env::block_timestamp_ms());
        self.denylist.insert(account_id.clone());
        emit("report", json!({ "account_id": account_id, "category": category, "evidence": evidence }));
        Promise::new(registry).function_call(
            "report",
            serde_json::to_vec(&json!({
                "account_id": account_id,
                "category": category,
                "evidence": evidence,
                "note": format!("reported by agent wallet {}", env::current_account_id()),
            }))
            .unwrap(),
            REPORT_BOND,
            GAS_REPORT,
        )
    }

    // ================================================================== callbacks

    #[private]
    pub fn on_registry_check(
        &mut self,
        id: U64,
        #[callback_result] verdict: Result<RegistryVerdict, PromiseError>,
    ) -> PromiseOrValue<Outcome> {
        let id = id.0;
        let flight = self.in_flight.get(&id).cloned().expect("Unknown in-flight action");
        let cp = flight.action.counterparty().clone();

        // 1. Hard stops: kill switch, registry down (fail closed), confirmed scam.
        let stop = if self.frozen {
            Some("wallet was frozen while the registry check ran".to_string())
        } else {
            match &verdict {
                Err(_) => Some("registry unavailable: failing closed".to_string()),
                Ok(v) if v.status == "Blocked" => Some(format!("registry: {cp} is a confirmed scam account")),
                Ok(v) if v.status == "Flagged" && self.policy.block_flagged && !flight.owner_approved => {
                    Some(format!("registry: {cp} has {} open scam report(s)", v.open_reports))
                }
                _ => None,
            }
        };
        if let Some(reason) = stop {
            self.release(id);
            return PromiseOrValue::Value(self.update(id, Status::Blocked, vec![reason]));
        }

        // 2. Things a human must look at, unless the owner already approved this action.
        let mut approval = flight.approval.clone();
        if let Ok(v) = &verdict {
            if v.status == "Flagged" {
                approval.insert(0, format!("registry: {cp} has {} open scam report(s), needs your review", v.open_reports));
            }
        }
        if !approval.is_empty() && !flight.owner_approved {
            self.release(id);
            let d = self.history.get(&id).cloned().expect("Unknown decision");
            return PromiseOrValue::Value(self.park(id, flight.action, approval, d.requested_by, d.memo));
        }

        // 3. Clear: send it.
        self.update(id, Status::Dispatched, vec![]);
        PromiseOrValue::Promise(self.dispatch(id, &flight.action))
    }

    #[private]
    pub fn on_settled(&mut self, id: U64) -> Outcome {
        let id = id.0;
        let ok = env::promise_results_count() == 1
            && match env::promise_result_checked(0, 16 * 1024) {
                Ok(_) => true,
                Err(PromiseError::TooLong(_)) => true,
                Err(_) => false,
            };
        let flight = self.in_flight.remove(&id);
        if ok {
            if let Some(f) = flight {
                self.seen.insert(f.action.counterparty().clone());
            }
            self.update(id, Status::Settled, vec![])
        } else {
            if let Some(f) = flight {
                if f.reserved {
                    self.release_spend(&f.action, f.hour);
                }
            }
            self.update(id, Status::Failed, vec!["receipt failed; reservation released".into()])
        }
    }

    // ================================================================== owner API

    /// Approve a pending action. Owner approval overrides the approval threshold, the
    /// first-time-receiver rule and an unreviewed registry flag. It never overrides the kill
    /// switch, the denylist, the asset list, the per-transaction cap or a registry "Blocked"
    /// verdict, which is re-checked at approval time. (The owner can still pay anyone with
    /// `owner_execute`, deliberately and outside the agent's flow.)
    pub fn approve(&mut self, id: U64) -> PromiseOrValue<Outcome> {
        self.assert_owner();
        require!(!self.frozen, "Wallet is frozen");
        let p = self.pending.remove(&id.0).expect("No such pending action");
        if env::block_timestamp_ms() > p.expires_at_ms.0 {
            return PromiseOrValue::Value(self.update(id.0, Status::Expired, vec!["approval came after expiry".into()]));
        }
        let mut reasons = vec![];
        self.check_hard_limits(&p.action, &mut reasons);
        if !reasons.is_empty() {
            return PromiseOrValue::Value(self.update(id.0, Status::Blocked, reasons));
        }
        emit("approved", json!({ "id": id, "by": self.owner_id }));
        self.track(id.0, &p.action, true, vec![], true);
        match self.policy.registry_id.clone() {
            // Re-check: the receiver may have been confirmed as a scam while the action waited.
            Some(registry) => {
                self.update(id.0, Status::Checking, vec!["approved by owner".into()]);
                self.check_registry(id.0, registry, &p.action)
            }
            None => {
                self.update(id.0, Status::Dispatched, vec!["approved by owner".into()]);
                PromiseOrValue::Promise(self.dispatch(id.0, &p.action))
            }
        }
    }

    pub fn reject(&mut self, id: U64, reason: Option<String>) -> Outcome {
        self.assert_owner();
        self.pending.remove(&id.0).expect("No such pending action");
        self.update(id.0, Status::Rejected, vec![reason.unwrap_or_else(|| "rejected by owner".into())])
    }

    /// Kill switch. The owner or any guardian can pull it.
    pub fn freeze(&mut self, reason: Option<String>) {
        let who = env::predecessor_account_id();
        require!(who == self.owner_id || self.guardians.contains(&who), "Only the owner or a guardian");
        self.frozen = true;
        emit("frozen", json!({ "by": who, "reason": reason }));
    }

    pub fn unfreeze(&mut self) {
        self.assert_owner();
        self.frozen = false;
        emit("unfrozen", json!({ "by": self.owner_id }));
    }

    pub fn set_policy(&mut self, policy: Policy) {
        self.assert_owner();
        validate_policy(&policy);
        self.policy = policy;
        emit("policy_updated", json!({ "policy": self.policy }));
    }

    /// Give an agent a function-call key that can only call this wallet's agent methods.
    /// `allowance` is the key's gas budget in yoctoNEAR and must be > 0.
    pub fn add_agent_key(&mut self, public_key: PublicKey, label: String, allowance: U128) -> Promise {
        self.assert_owner();
        require!(label.len() <= 64, "Label too long");
        let allowance = Allowance::limited(NearToken::from_yoctonear(allowance.0))
            .expect("Allowance must be greater than zero");
        self.agent_keys.insert(
            public_key.clone(),
            AgentKey { label: label.clone(), added_at_ms: U64(env::block_timestamp_ms()) },
        );
        emit("agent_key_added", json!({ "public_key": public_key, "label": label }));
        Promise::new(env::current_account_id()).add_access_key_allowance(
            public_key,
            allowance,
            env::current_account_id(),
            AGENT_METHODS,
        )
    }

    pub fn revoke_agent_key(&mut self, public_key: PublicKey) -> Promise {
        self.assert_owner();
        self.agent_keys.remove(&public_key).expect("Not an agent key");
        emit("agent_key_revoked", json!({ "public_key": public_key }));
        Promise::new(env::current_account_id()).delete_key(public_key)
    }

    pub fn add_guardian(&mut self, account_id: AccountId) {
        self.assert_owner();
        self.guardians.insert(account_id);
    }

    pub fn remove_guardian(&mut self, account_id: AccountId) {
        self.assert_owner();
        self.guardians.remove(&account_id);
    }

    pub fn add_contact(&mut self, account_id: AccountId) {
        self.assert_owner();
        self.contacts.insert(account_id);
    }

    pub fn remove_contact(&mut self, account_id: AccountId) {
        self.assert_owner();
        self.contacts.remove(&account_id);
    }

    pub fn deny(&mut self, account_id: AccountId) {
        self.assert_owner();
        self.denylist.insert(account_id);
    }

    pub fn undeny(&mut self, account_id: AccountId) {
        self.assert_owner();
        self.denylist.remove(&account_id);
    }

    /// The owner acts directly, without policy (for example to fund a storage deposit or to
    /// pull funds out of a wallet whose full-access key was deleted).
    pub fn owner_execute(&mut self, action: Action) -> Promise {
        self.assert_owner();
        let id = self.take_id();
        self.record(id, &action, Status::Dispatched, vec!["owner action".into()], "owner", &None);
        self.dispatch(id, &action)
    }

    pub fn set_owner(&mut self, owner_id: AccountId) {
        self.assert_owner();
        emit("owner_changed", json!({ "from": self.owner_id, "to": owner_id }));
        self.owner_id = owner_id;
    }

    // ================================================================== views

    /// What would happen to this action right now, before the registry check.
    pub fn simulate(&self, action: Action) -> Outcome {
        let (status, reasons) = match self.evaluate(&action, true) {
            Verdict::Allow => (Status::Dispatched, vec![]),
            Verdict::NeedsApproval(r) => (Status::Pending, r),
            Verdict::Block(r) => (Status::Blocked, r),
        };
        Outcome { id: U64(self.next_id), status, reasons }
    }

    pub fn get_status(&self) -> WalletStatus {
        let now_hour = now_hour();
        let assets = self
            .policy
            .limits
            .iter()
            .map(|(asset, l)| {
                let spent = self.spent(asset, now_hour);
                AssetStatus {
                    asset: asset.clone(),
                    spent_24h: U128(spent),
                    remaining_24h: U128(l.daily_limit.0.saturating_sub(spent)),
                    limits: l.clone(),
                }
            })
            .collect();
        WalletStatus {
            owner_id: self.owner_id.clone(),
            frozen: self.frozen,
            balance: U128(env::account_balance().as_yoctonear()),
            assets,
            actions_last_hour: self.actions_last_hour(),
            max_actions_per_hour: self.policy.max_actions_per_hour,
            pending: self.pending.len(),
            agent_keys: self.agent_keys.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            guardians: self.guardians.iter().cloned().collect(),
            next_id: U64(self.next_id),
        }
    }

    pub fn get_policy(&self) -> Policy {
        self.policy.clone()
    }

    pub fn get_pending(&self) -> Vec<Pending> {
        self.pending.values().cloned().collect()
    }

    /// Newest first.
    pub fn get_decisions(&self, limit: Option<u32>) -> Vec<Decision> {
        let limit = limit.unwrap_or(20).min(HISTORY_LEN as u32) as u64;
        (0..self.next_id)
            .rev()
            .take(limit as usize)
            .filter_map(|id| self.history.get(&id).cloned())
            .collect()
    }

    pub fn get_decision(&self, id: U64) -> Option<Decision> {
        self.history.get(&id.0).cloned()
    }

    pub fn get_contacts(&self) -> Vec<AccountId> {
        self.contacts.iter().cloned().collect()
    }

    pub fn get_denylist(&self) -> Vec<AccountId> {
        self.denylist.iter().cloned().collect()
    }

    pub fn has_paid(&self, account_id: AccountId) -> bool {
        self.seen.contains(&account_id)
    }

    // ================================================================== internals

    fn assert_owner(&self) {
        require!(env::predecessor_account_id() == self.owner_id, "Only the owner");
    }

    /// The caller must be this account, signed with a registered agent key.
    fn assert_agent(&self) -> String {
        require!(
            env::predecessor_account_id() == env::current_account_id(),
            "Agent methods must be signed by this wallet's agent key"
        );
        let pk = env::signer_account_pk();
        match self.agent_keys.get(&pk) {
            Some(k) => k.label.clone(),
            None => env::panic_str("Signer key is not a registered agent key"),
        }
    }

    fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        if id >= HISTORY_LEN {
            let old = id - HISTORY_LEN;
            if !self.pending.contains_key(&old) && !self.in_flight.contains_key(&old) {
                self.history.remove(&old);
            }
        }
        id
    }

    /// Pure policy check. `count_new` is false for `propose`, where the agent already asks.
    fn evaluate(&self, action: &Action, count_new: bool) -> Verdict {
        let mut block = vec![];
        if self.frozen {
            return Verdict::Block(vec!["wallet is frozen".into()]);
        }
        self.check_hard_limits(action, &mut block);
        self.check_amount_limits(action, now_hour(), &mut block);
        self.check_rate(&mut block);
        if !block.is_empty() {
            return Verdict::Block(block);
        }

        let mut approval = vec![];
        let asset = action.asset();
        if let Some(l) = self.policy.limits.get(&asset) {
            if action.amount() > l.approval_above.0 {
                approval.push(format!(
                    "{} is above the approval threshold of {}",
                    fmt_amount(&asset, action.amount()),
                    fmt_amount(&asset, l.approval_above.0)
                ));
            }
        }
        let cp = action.counterparty();
        if count_new
            && self.policy.approve_new_receivers
            && !self.contacts.contains(cp)
            && !self.seen.contains(cp)
        {
            approval.push(format!("first payment to {cp}"));
        }
        if self.pending.len() >= MAX_PENDING && !approval.is_empty() {
            return Verdict::Block(vec!["too many pending approvals".into()]);
        }
        if approval.is_empty() {
            Verdict::Allow
        } else {
            Verdict::NeedsApproval(approval)
        }
    }

    /// Checks that even an owner approval cannot override.
    fn check_hard_limits(&self, action: &Action, out: &mut Vec<String>) {
        let me = env::current_account_id();
        let cp = action.counterparty();
        if cp == &me {
            out.push("the wallet cannot pay or call itself".into());
        }
        if Some(cp) == self.policy.registry_id.as_ref() {
            out.push("the registry is not a payment target".into());
        }
        if self.denylist.contains(cp) {
            out.push(format!("{cp} is on this wallet's denylist"));
        }
        if self.policy.receiver_mode == ReceiverMode::ContactsOnly && !self.contacts.contains(cp) {
            out.push(format!("{cp} is not in the contact list (contacts-only mode)"));
        }
        let asset = action.asset();
        match self.policy.limits.get(&asset) {
            None => out.push(format!("asset {asset} is not allowed by the policy")),
            Some(l) => {
                if action.amount() > l.max_per_tx.0 {
                    out.push(format!(
                        "{} is above the per-transaction cap of {}",
                        fmt_amount(&asset, action.amount()),
                        fmt_amount(&asset, l.max_per_tx.0)
                    ));
                }
            }
        }
        match action {
            Action::Transfer { amount, .. } | Action::FtTransfer { amount, .. } => {
                if amount.0 == 0 {
                    out.push("amount must be greater than zero".into());
                }
            }
            Action::Call { contract_id, method_name, deposit, gas, .. } => {
                if gas.0 > MAX_CALL_GAS.as_gas() {
                    out.push("call gas above the 100 Tgas cap".into());
                }
                match self.policy.call_rules.iter().find(|r| &r.contract_id == contract_id) {
                    None => out.push(format!("calls to {contract_id} are not allowed")),
                    Some(rule) => {
                        if !rule.methods.iter().any(|m| m == method_name) {
                            out.push(format!("method {method_name} on {contract_id} is not allowed"));
                        }
                        if deposit.0 > rule.max_deposit.0 {
                            out.push(format!(
                                "deposit {} is above the rule's cap of {}",
                                fmt_amount(NEAR_ASSET, deposit.0),
                                fmt_amount(NEAR_ASSET, rule.max_deposit.0)
                            ));
                        }
                    }
                }
            }
        }
        if let Action::FtTransfer { token_id, .. } = action {
            if token_id == &me {
                out.push("invalid token".into());
            }
        }
    }

    fn check_amount_limits(&self, action: &Action, now_hour: u64, out: &mut Vec<String>) {
        let asset = action.asset();
        if let Some(l) = self.policy.limits.get(&asset) {
            let spent = self.spent(&asset, now_hour);
            if spent.saturating_add(action.amount()) > l.daily_limit.0 {
                out.push(format!(
                    "rolling 24h limit: spent {} of {}, this action asks {}",
                    fmt_amount(&asset, spent),
                    fmt_amount(&asset, l.daily_limit.0),
                    fmt_amount(&asset, action.amount())
                ));
            }
        }
    }

    fn check_rate(&self, out: &mut Vec<String>) {
        if self.actions_last_hour() >= self.policy.max_actions_per_hour {
            out.push(format!(
                "rate limit: {} actions in the last hour",
                self.policy.max_actions_per_hour
            ));
        }
    }

    fn actions_last_hour(&self) -> u32 {
        let cutoff = env::block_timestamp_ms().saturating_sub(HOUR_MS);
        self.recent_actions.iter().filter(|t| **t > cutoff).count() as u32
    }

    fn spent(&self, asset: &str, now_hour: u64) -> u128 {
        self.spend.get(asset).map(|w| w.total_at(now_hour)).unwrap_or(0)
    }

    fn add_spend(&mut self, asset: &str, now_hour: u64, amount: u128) {
        let mut w = self.spend.get(asset).cloned().unwrap_or_default();
        w.add(now_hour, amount);
        self.spend.insert(asset.to_string(), w);
    }

    fn release_spend(&mut self, action: &Action, hour: u64) {
        let asset = action.asset();
        let mut w = self.spend.get(&asset).cloned().unwrap_or_default();
        w.release(now_hour(), hour, action.amount());
        self.spend.insert(asset, w);
    }

    /// Track an action across the async registry check and settlement.
    ///
    /// With `reserve`, the action is counted against the window and the rate limit *now*,
    /// before anything async happens, so two concurrent requests cannot both squeeze under the
    /// same limit. Actions that will wait for the owner are tracked without a reservation.
    fn track(&mut self, id: u64, action: &Action, reserve: bool, approval: Vec<String>, owner_approved: bool) {
        let hour = now_hour();
        if reserve {
            self.add_spend(&action.asset(), hour, action.amount());
            let cutoff = env::block_timestamp_ms().saturating_sub(HOUR_MS);
            self.recent_actions.retain(|t| *t > cutoff);
            self.recent_actions.push(env::block_timestamp_ms());
            if self.recent_actions.len() > MAX_ACTIONS_PER_HOUR_CAP as usize {
                self.recent_actions.remove(0);
            }
        }
        self.in_flight.insert(
            id,
            InFlight { action: action.clone(), hour, reserved: reserve, approval, owner_approved },
        );
    }

    fn release(&mut self, id: u64) {
        if let Some(f) = self.in_flight.remove(&id) {
            if f.reserved {
                self.release_spend(&f.action, f.hour);
            }
        }
    }

    fn check_registry(&mut self, id: u64, registry: AccountId, action: &Action) -> PromiseOrValue<Outcome> {
        let callback_gas = GAS_CALLBACK_OVERHEAD
            .saturating_add(action.dispatch_gas())
            .saturating_add(GAS_ON_SETTLED);
        require!(
            env::prepaid_gas().as_gas()
                >= GAS_REGISTRY_CHECK.as_gas() + callback_gas.as_gas() + Gas::from_tgas(15).as_gas(),
            "Attach more gas: the registry check and the payment need about 60-150 Tgas"
        );
        let check = Promise::new(registry).function_call(
            "check",
            serde_json::to_vec(&json!({ "account_id": action.counterparty() })).unwrap(),
            NearToken::from_yoctonear(0),
            GAS_REGISTRY_CHECK,
        );
        PromiseOrValue::Promise(
            check.then(
                Self::ext(env::current_account_id())
                    .with_static_gas(callback_gas)
                    .on_registry_check(U64(id)),
            ),
        )
    }

    fn dispatch(&mut self, id: u64, action: &Action) -> Promise {
        if !self.in_flight.contains_key(&id) {
            // owner_execute: not reserved against the window, but tracked for settlement.
            self.track(id, action, false, vec![], true);
        }
        let p = match action {
            Action::Transfer { receiver_id, amount } => {
                Promise::new(receiver_id.clone()).transfer(NearToken::from_yoctonear(amount.0))
            }
            Action::FtTransfer { token_id, receiver_id, amount, memo } => {
                Promise::new(token_id.clone()).function_call(
                    "ft_transfer",
                    serde_json::to_vec(&json!({
                        "receiver_id": receiver_id,
                        "amount": amount,
                        "memo": memo,
                    }))
                    .unwrap(),
                    ONE_YOCTO,
                    GAS_FT_TRANSFER,
                )
            }
            Action::Call { contract_id, method_name, args, deposit, gas } => {
                Promise::new(contract_id.clone()).function_call(
                    method_name.clone(),
                    args.0.clone(),
                    NearToken::from_yoctonear(deposit.0),
                    Gas::from_gas(gas.0),
                )
            }
        };
        p.then(
            Self::ext(env::current_account_id())
                .with_static_gas(GAS_ON_SETTLED)
                .on_settled(U64(id)),
        )
    }

    fn record(&mut self, id: u64, action: &Action, status: Status, reasons: Vec<String>, who: &str, memo: &Option<String>) {
        let now = U64(env::block_timestamp_ms());
        let d = Decision {
            id: U64(id),
            action: action.clone(),
            status,
            reasons,
            requested_by: who.to_string(),
            memo: memo.clone(),
            created_at_ms: now,
            updated_at_ms: now,
        };
        emit_decision(&d);
        self.history.insert(id, d);
    }

    fn update(&mut self, id: u64, status: Status, reasons: Vec<String>) -> Outcome {
        let mut d = self.history.get(&id).cloned().expect("Unknown decision");
        d.status = status;
        if !reasons.is_empty() {
            d.reasons = reasons.clone();
        }
        d.updated_at_ms = U64(env::block_timestamp_ms());
        emit_decision(&d);
        let out = Outcome { id: U64(id), status, reasons: d.reasons.clone() };
        self.history.insert(id, d);
        out
    }

    fn record_blocked(&mut self, id: u64, action: Action, reasons: Vec<String>, who: String, memo: Option<String>) -> Outcome {
        self.record(id, &action, Status::Blocked, reasons.clone(), &who, &memo);
        Outcome { id: U64(id), status: Status::Blocked, reasons }
    }

    fn park(&mut self, id: u64, action: Action, reasons: Vec<String>, who: String, memo: Option<String>) -> Outcome {
        let now = env::block_timestamp_ms();
        self.record(id, &action, Status::Pending, reasons.clone(), &who, &memo);
        self.pending.insert(
            id,
            Pending {
                id: U64(id),
                action,
                reasons: reasons.clone(),
                requested_by: who,
                memo,
                created_at_ms: U64(now),
                expires_at_ms: U64(now + self.policy.pending_ttl_ms.0),
            },
        );
        Outcome { id: U64(id), status: Status::Pending, reasons }
    }
}

/// "1.25 NEAR" for native NEAR, raw units plus the token id for NEP-141 tokens.
pub fn fmt_amount(asset: &str, amount: u128) -> String {
    if asset != NEAR_ASSET {
        return format!("{amount} {asset}");
    }
    const ONE: u128 = 1_000_000_000_000_000_000_000_000;
    let whole = amount / ONE;
    let frac = (amount % ONE) / 100_000_000_000_000_000_000; // 4 decimals
    if frac == 0 {
        format!("{whole} NEAR")
    } else {
        let f = format!("{frac:04}");
        format!("{whole}.{} NEAR", f.trim_end_matches('0'))
    }
}

fn now_hour() -> u64 {
    env::block_timestamp_ms() / HOUR_MS
}

fn check_memo(memo: &Option<String>) {
    if let Some(m) = memo {
        require!(m.len() <= MAX_MEMO_LEN, "Memo too long");
    }
}

fn validate_policy(p: &Policy) {
    require!(
        p.max_actions_per_hour > 0 && p.max_actions_per_hour <= MAX_ACTIONS_PER_HOUR_CAP,
        "max_actions_per_hour must be between 1 and 120"
    );
    for (asset, l) in &p.limits {
        require!(l.max_per_tx.0 <= l.daily_limit.0, format!("{asset}: max_per_tx above daily_limit"));
        if asset != NEAR_ASSET {
            require!(asset.parse::<AccountId>().is_ok(), format!("{asset}: not a token account id"));
        }
    }
    for r in &p.call_rules {
        require!(r.methods.len() <= 32, "Too many methods in a call rule");
    }
    require!(p.call_rules.len() <= 32, "Too many call rules");
    require!(p.pending_ttl_ms.0 >= 60_000, "pending_ttl_ms must be at least one minute");
}

fn emit(event: &str, data: serde_json::Value) {
    env::log_str(&format!(
        "EVENT_JSON:{}",
        json!({ "standard": "leash", "version": "1.0.0", "event": event, "data": [data] })
    ));
}

fn emit_decision(d: &Decision) {
    emit("decision", serde_json::to_value(d).unwrap());
}

#[cfg(test)]
mod tests;
