#![allow(unused_must_use)]

use super::*;
use near_sdk::test_utils::{get_logs, VMContextBuilder};
use near_sdk::{test_vm_config, testing_env, PromiseResult, RuntimeFeesConfig};

const WALLET: &str = "agent.owner.testnet";
const OWNER: &str = "owner.testnet";
const REGISTRY: &str = "registry.testnet";
const GUARDIAN: &str = "guardian.testnet";
const ALICE: &str = "alice.testnet"; // a contact
const T0_MS: u64 = 1_800_000_000_000;
const ONE_NEAR: u128 = 1_000_000_000_000_000_000_000_000;

fn acc(s: &str) -> AccountId {
    s.parse().unwrap()
}

fn agent_pk() -> PublicKey {
    "ed25519:6E8sCci9badyRkXb3JoRpBj5p8C6Tw41ELDZoiihKEtp".parse().unwrap()
}

fn rogue_pk() -> PublicKey {
    "ed25519:DcA2MzgpJbrUATQLLceocVckhhAqrkingax4oJ9kZ847".parse().unwrap()
}

fn ctx(predecessor: &str, pk: PublicKey, now_ms: u64, results: Vec<PromiseResult>) {
    let mut b = VMContextBuilder::new();
    b.current_account_id(acc(WALLET))
        .predecessor_account_id(acc(predecessor))
        .signer_account_id(acc(predecessor))
        .signer_account_pk(pk)
        .block_timestamp(now_ms * 1_000_000)
        .account_balance(NearToken::from_near(50))
        .prepaid_gas(Gas::from_tgas(300));
    testing_env!(b.build(), test_vm_config(), RuntimeFeesConfig::test(), Default::default(), results);
}

fn as_agent(now_ms: u64) {
    ctx(WALLET, agent_pk(), now_ms, vec![]);
}

fn as_owner(now_ms: u64) {
    ctx(OWNER, rogue_pk(), now_ms, vec![]);
}

fn as_callback(now_ms: u64, results: Vec<PromiseResult>) {
    ctx(WALLET, agent_pk(), now_ms, results);
}

fn milli(n: u128) -> U128 {
    U128(n * ONE_NEAR / 1000)
}

fn pay(to: &str, amount: U128) -> Action {
    Action::Transfer { receiver_id: acc(to), amount }
}

fn setup(with_registry: bool) -> LeashWallet {
    as_owner(T0_MS);
    let mut w = LeashWallet::new(
        acc(OWNER),
        None,
        with_registry.then(|| acc(REGISTRY)),
        Some(vec![acc(GUARDIAN)]),
        Some(vec![acc(ALICE)]),
    );
    w.add_agent_key(agent_pk(), "invoice-bot".into(), U128(ONE_NEAR));
    w
}

fn last_id(w: &LeashWallet) -> U64 {
    U64(w.get_status().next_id.0 - 1)
}

fn status_of(w: &LeashWallet) -> Status {
    w.get_decision(last_id(w)).unwrap().status
}

fn value(r: PromiseOrValue<Outcome>) -> Outcome {
    match r {
        PromiseOrValue::Value(o) => o,
        PromiseOrValue::Promise(_) => panic!("expected a value, got a promise"),
    }
}

fn is_promise(r: &PromiseOrValue<Outcome>) -> bool {
    matches!(r, PromiseOrValue::Promise(_))
}

fn spent_near(w: &LeashWallet) -> u128 {
    w.get_status().assets.iter().find(|a| a.asset == NEAR_ASSET).unwrap().spent_24h.0
}

// ------------------------------------------------------------------ who may call

#[test]
#[should_panic(expected = "not a registered agent key")]
fn rogue_key_on_the_wallet_cannot_execute() {
    let mut w = setup(false);
    ctx(WALLET, rogue_pk(), T0_MS, vec![]);
    w.execute(pay(ALICE, milli(100)), None);
}

#[test]
#[should_panic(expected = "must be signed by this wallet's agent key")]
fn other_accounts_cannot_execute() {
    let mut w = setup(false);
    ctx("mallory.testnet", agent_pk(), T0_MS, vec![]);
    w.execute(pay(ALICE, milli(100)), None);
}

#[test]
#[should_panic(expected = "Only the owner")]
fn agent_cannot_change_policy() {
    let mut w = setup(false);
    as_agent(T0_MS);
    let mut p = w.get_policy();
    p.max_actions_per_hour = 100;
    w.set_policy(p);
}

#[test]
#[should_panic(expected = "Only the owner")]
fn agent_cannot_add_keys() {
    let mut w = setup(false);
    as_agent(T0_MS);
    w.add_agent_key(rogue_pk(), "x".into(), U128(1));
}

#[test]
#[should_panic(expected = "greater than zero")]
fn zero_allowance_key_is_refused() {
    let mut w = setup(false);
    as_owner(T0_MS);
    w.add_agent_key(rogue_pk(), "x".into(), U128(0));
}

// ------------------------------------------------------------------ happy path

#[test]
fn small_payment_to_contact_is_dispatched_and_reserved() {
    let mut w = setup(false);
    as_agent(T0_MS);
    let r = w.execute(pay(ALICE, milli(200)), Some("invoice #1".into()));
    assert!(is_promise(&r));
    assert_eq!(status_of(&w), Status::Dispatched);
    assert_eq!(spent_near(&w), milli(200).0);
    assert!(get_logs().iter().any(|l| l.contains("\"event\":\"decision\"")));
}

#[test]
fn settled_receipt_marks_receiver_as_seen() {
    let mut w = setup(false);
    as_agent(T0_MS);
    w.execute(pay("bob.testnet", milli(100)), None); // new receiver -> pending
    assert_eq!(status_of(&w), Status::Pending);
    as_owner(T0_MS);
    w.approve(last_id(&w));
    as_callback(T0_MS, vec![PromiseResult::Successful(vec![])]);
    let o = w.on_settled(last_id(&w));
    assert_eq!(o.status, Status::Settled);
    assert!(w.has_paid(acc("bob.testnet")));
    // Second payment to bob no longer needs approval.
    as_agent(T0_MS + 1000);
    assert!(is_promise(&w.execute(pay("bob.testnet", milli(100)), None)));
}

#[test]
fn failed_receipt_releases_the_reservation() {
    let mut w = setup(false);
    as_agent(T0_MS);
    w.execute(pay(ALICE, milli(300)), None);
    assert_eq!(spent_near(&w), milli(300).0);
    as_callback(T0_MS, vec![PromiseResult::Failed]);
    let o = w.on_settled(last_id(&w));
    assert_eq!(o.status, Status::Failed);
    assert_eq!(spent_near(&w), 0);
}

// ------------------------------------------------------------------ hard limits

#[test]
fn above_per_tx_cap_is_blocked_without_spending() {
    let mut w = setup(false);
    as_agent(T0_MS);
    let o = value(w.execute(pay(ALICE, milli(1500)), None));
    assert_eq!(o.status, Status::Blocked);
    assert!(o.reasons[0].contains("per-transaction cap"));
    assert_eq!(spent_near(&w), 0);
}

#[test]
fn rolling_window_has_no_midnight_reset() {
    let mut w = setup(false);
    // Six payments of 0.5 NEAR use the whole 3 NEAR window.
    for i in 0..6 {
        as_agent(T0_MS + i * 60_000);
        assert!(is_promise(&w.execute(pay(ALICE, milli(500)), None)));
    }
    as_agent(T0_MS + 10 * 60_000);
    let o = value(w.execute(pay(ALICE, milli(100)), None));
    assert_eq!(o.status, Status::Blocked);
    assert!(o.reasons[0].contains("rolling 24h limit"));

    // 12 hours later: still inside the window.
    as_agent(T0_MS + 12 * HOUR_MS);
    assert_eq!(value(w.execute(pay(ALICE, milli(100)), None)).status, Status::Blocked);

    // 25 hours later: the old spend has rolled out.
    as_agent(T0_MS + 25 * HOUR_MS);
    assert!(is_promise(&w.execute(pay(ALICE, milli(100)), None)));
}

#[test]
fn rate_limit_counts_a_rolling_hour() {
    let mut w = setup(false);
    as_owner(T0_MS);
    let mut p = w.get_policy();
    p.max_actions_per_hour = 2;
    w.set_policy(p);
    as_agent(T0_MS);
    w.execute(pay(ALICE, milli(10)), None);
    as_agent(T0_MS + 1);
    w.execute(pay(ALICE, milli(10)), None);
    as_agent(T0_MS + 2);
    let o = value(w.execute(pay(ALICE, milli(10)), None));
    assert_eq!(o.status, Status::Blocked);
    assert!(o.reasons[0].contains("rate limit"));
    as_agent(T0_MS + HOUR_MS + 10);
    assert!(is_promise(&w.execute(pay(ALICE, milli(10)), None)));
}

#[test]
fn denylist_contacts_only_and_unknown_assets() {
    let mut w = setup(false);
    as_owner(T0_MS);
    w.deny(acc("drainer.testnet"));
    as_agent(T0_MS);
    let o = value(w.execute(pay("drainer.testnet", milli(10)), None));
    assert!(o.reasons.iter().any(|r| r.contains("denylist")));

    let ft = Action::FtTransfer {
        token_id: acc("usdc.testnet"),
        receiver_id: acc(ALICE),
        amount: U128(1_000_000),
        memo: None,
    };
    let o = value(w.execute(ft, None));
    assert!(o.reasons.iter().any(|r| r.contains("asset usdc.testnet is not allowed")));

    as_owner(T0_MS);
    let mut p = w.get_policy();
    p.receiver_mode = ReceiverMode::ContactsOnly;
    w.set_policy(p);
    as_agent(T0_MS);
    let o = value(w.execute(pay("stranger.testnet", milli(10)), None));
    assert!(o.reasons.iter().any(|r| r.contains("contacts-only")));
}

#[test]
fn ft_limits_apply_per_token() {
    let mut w = setup(false);
    as_owner(T0_MS);
    let mut p = w.get_policy();
    p.limits.insert(
        "usdc.testnet".into(),
        AssetLimits { max_per_tx: U128(50_000_000), daily_limit: U128(100_000_000), approval_above: U128(20_000_000) },
    );
    w.set_policy(p);
    let ft = |amount: u128| Action::FtTransfer {
        token_id: acc("usdc.testnet"),
        receiver_id: acc(ALICE),
        amount: U128(amount),
        memo: None,
    };
    as_agent(T0_MS);
    assert!(is_promise(&w.execute(ft(10_000_000), None)));
    assert_eq!(value(w.execute(ft(30_000_000), None)).status, Status::Pending);
    assert_eq!(value(w.execute(ft(60_000_000), None)).status, Status::Blocked);
    // NEAR spending is untouched by USDC spending.
    assert_eq!(spent_near(&w), 0);
}

#[test]
fn call_rules_gate_contract_calls() {
    let mut w = setup(false);
    as_owner(T0_MS);
    let mut p = w.get_policy();
    p.call_rules.push(CallRule {
        contract_id: acc("oracle.testnet"),
        methods: vec!["buy_report".into()],
        max_deposit: milli(100),
    });
    w.set_policy(p);
    w.add_contact(acc("oracle.testnet"));
    let call = |method: &str, deposit: U128, gas: u64| Action::Call {
        contract_id: acc("oracle.testnet"),
        method_name: method.into(),
        args: Base64VecU8(b"{}".to_vec()),
        deposit,
        gas: U64(Gas::from_tgas(gas).as_gas()),
    };
    as_agent(T0_MS);
    assert!(is_promise(&w.execute(call("buy_report", milli(50), 30), None)));
    let o = value(w.execute(call("withdraw_all", milli(0), 30), None));
    assert!(o.reasons.iter().any(|r| r.contains("method withdraw_all")));
    let o = value(w.execute(call("buy_report", milli(200), 30), None));
    assert!(o.reasons.iter().any(|r| r.contains("above the rule's cap")));
    let o = value(w.execute(call("buy_report", milli(10), 200), None));
    assert!(o.reasons.iter().any(|r| r.contains("100 Tgas")));
    let other = Action::Call {
        contract_id: acc("dex.testnet"),
        method_name: "swap".into(),
        args: Base64VecU8(vec![]),
        deposit: U128(0),
        gas: U64(Gas::from_tgas(30).as_gas()),
    };
    let o = value(w.execute(other, None));
    assert!(o.reasons.iter().any(|r| r.contains("calls to dex.testnet are not allowed")));
}

#[test]
fn wallet_cannot_call_itself_or_pay_the_registry() {
    let mut w = setup(true);
    as_owner(T0_MS);
    let mut p = w.get_policy();
    p.call_rules.push(CallRule {
        contract_id: acc(WALLET),
        methods: vec!["set_policy".into()],
        max_deposit: U128(0),
    });
    w.set_policy(p);
    as_agent(T0_MS);
    let self_call = Action::Call {
        contract_id: acc(WALLET),
        method_name: "set_policy".into(),
        args: Base64VecU8(vec![]),
        deposit: U128(0),
        gas: U64(Gas::from_tgas(10).as_gas()),
    };
    let o = value(w.execute(self_call, None));
    assert!(o.reasons.iter().any(|r| r.contains("cannot pay or call itself")));
    let o = value(w.execute(pay(REGISTRY, milli(10)), None));
    assert!(o.reasons.iter().any(|r| r.contains("not a payment target")));
}

// ------------------------------------------------------------------ approvals

#[test]
fn approval_threshold_parks_then_owner_approves() {
    let mut w = setup(false);
    as_agent(T0_MS);
    let o = value(w.execute(pay(ALICE, milli(800)), None));
    assert_eq!(o.status, Status::Pending);
    assert_eq!(w.get_pending().len(), 1);
    assert_eq!(spent_near(&w), 0);
    as_owner(T0_MS + 1000);
    assert!(is_promise(&w.approve(o.id)));
    assert_eq!(w.get_pending().len(), 0);
    assert_eq!(spent_near(&w), milli(800).0);
}

#[test]
fn owner_rejects() {
    let mut w = setup(false);
    as_agent(T0_MS);
    let o = value(w.execute(pay(ALICE, milli(800)), None));
    as_owner(T0_MS);
    assert_eq!(w.reject(o.id, Some("not on the invoice".into())).status, Status::Rejected);
    assert!(w.get_pending().is_empty());
}

#[test]
fn approval_after_expiry_does_not_pay() {
    let mut w = setup(false);
    as_agent(T0_MS);
    let o = value(w.execute(pay(ALICE, milli(800)), None));
    as_owner(T0_MS + 25 * HOUR_MS);
    assert_eq!(value(w.approve(o.id)).status, Status::Expired);
    assert_eq!(spent_near(&w), 0);
}

#[test]
fn approval_cannot_override_hard_limits() {
    let mut w = setup(false);
    as_agent(T0_MS);
    let o = value(w.execute(pay("newshop.testnet", milli(100)), None));
    assert_eq!(o.status, Status::Pending);
    as_owner(T0_MS);
    w.deny(acc("newshop.testnet"));
    let r = value(w.approve(o.id));
    assert_eq!(r.status, Status::Blocked);
    assert!(r.reasons.iter().any(|x| x.contains("denylist")));
}

#[test]
fn propose_parks_even_allowed_actions() {
    let mut w = setup(false);
    as_agent(T0_MS);
    let o = w.propose(pay(ALICE, milli(10)), "scout said warn: name looks like a lookalike".into(), None);
    assert_eq!(o.status, Status::Pending);
    assert!(o.reasons[0].starts_with("agent asked"));
    let o = w.propose(pay(ALICE, milli(5000)), "big".into(), None);
    assert_eq!(o.status, Status::Blocked);
}

// ------------------------------------------------------------------ kill switch

#[test]
fn guardian_freezes_and_only_owner_unfreezes() {
    let mut w = setup(false);
    ctx(GUARDIAN, rogue_pk(), T0_MS, vec![]);
    w.freeze(Some("anomaly".into()));
    as_agent(T0_MS);
    let o = value(w.execute(pay(ALICE, milli(10)), None));
    assert_eq!(o.reasons, vec!["wallet is frozen".to_string()]);
    as_owner(T0_MS);
    w.unfreeze();
    as_agent(T0_MS);
    assert!(is_promise(&w.execute(pay(ALICE, milli(10)), None)));
}

#[test]
#[should_panic(expected = "Only the owner")]
fn guardian_cannot_unfreeze() {
    let mut w = setup(false);
    ctx(GUARDIAN, rogue_pk(), T0_MS, vec![]);
    w.freeze(None);
    w.unfreeze();
}

#[test]
#[should_panic(expected = "Only the owner or a guardian")]
fn strangers_cannot_freeze() {
    let mut w = setup(false);
    ctx("mallory.testnet", rogue_pk(), T0_MS, vec![]);
    w.freeze(None);
}

// ------------------------------------------------------------------ registry

fn registry_says(status: &str, open: u32) -> Result<RegistryVerdict, PromiseError> {
    Ok(RegistryVerdict { status: status.into(), open_reports: open })
}

#[test]
fn registry_blocked_refuses_and_releases() {
    let mut w = setup(true);
    as_agent(T0_MS);
    assert!(is_promise(&w.execute(pay(ALICE, milli(300)), None)));
    assert_eq!(spent_near(&w), milli(300).0);
    as_callback(T0_MS, vec![]);
    let o = value(w.on_registry_check(last_id(&w), registry_says("Blocked", 0)));
    assert_eq!(o.status, Status::Blocked);
    assert!(o.reasons[0].contains("confirmed scam"));
    assert_eq!(spent_near(&w), 0);
}

#[test]
fn registry_flagged_goes_to_the_owner() {
    let mut w = setup(true);
    as_agent(T0_MS);
    w.execute(pay(ALICE, milli(300)), None);
    as_callback(T0_MS, vec![]);
    let o = value(w.on_registry_check(last_id(&w), registry_says("Flagged", 2)));
    assert_eq!(o.status, Status::Pending);
    assert!(o.reasons[0].contains("2 open scam report"));
    assert_eq!(w.get_pending().len(), 1);
    assert_eq!(spent_near(&w), 0);
}

#[test]
fn registry_flagged_blocks_when_policy_says_so() {
    let mut w = setup(true);
    as_owner(T0_MS);
    let mut p = w.get_policy();
    p.block_flagged = true;
    w.set_policy(p);
    as_agent(T0_MS);
    w.execute(pay(ALICE, milli(300)), None);
    as_callback(T0_MS, vec![]);
    assert_eq!(value(w.on_registry_check(last_id(&w), registry_says("Flagged", 1))).status, Status::Blocked);
}

#[test]
fn registry_unknown_dispatches() {
    let mut w = setup(true);
    as_agent(T0_MS);
    w.execute(pay(ALICE, milli(300)), None);
    as_callback(T0_MS, vec![]);
    assert!(is_promise(&w.on_registry_check(last_id(&w), registry_says("Unknown", 0))));
    assert_eq!(spent_near(&w), milli(300).0);
}

#[test]
fn registry_failure_fails_closed() {
    let mut w = setup(true);
    as_agent(T0_MS);
    w.execute(pay(ALICE, milli(300)), None);
    as_callback(T0_MS, vec![]);
    let o = value(w.on_registry_check(last_id(&w), Err(PromiseError::Failed)));
    assert_eq!(o.status, Status::Blocked);
    assert!(o.reasons[0].contains("failing closed"));
    assert_eq!(spent_near(&w), 0);
}

#[test]
fn payment_needing_approval_is_checked_against_the_registry_first() {
    // Regression: a compromised agent skipping the guard must not be able to park a payment to
    // a confirmed scam account in the owner's approval queue.
    let mut w = setup(true);
    as_agent(T0_MS);
    let r = w.execute(pay("drainer.testnet", milli(900)), None);
    assert!(is_promise(&r), "registry check runs before parking");
    assert_eq!(status_of(&w), Status::Checking);
    as_callback(T0_MS, vec![]);
    let o = value(w.on_registry_check(last_id(&w), registry_says("Blocked", 0)));
    assert_eq!(o.status, Status::Blocked);
    assert!(w.get_pending().is_empty());
    assert_eq!(spent_near(&w), 0);

    // A clean receiver that needs approval is parked after the check.
    as_agent(T0_MS);
    w.execute(pay("newshop.testnet", milli(100)), None);
    as_callback(T0_MS, vec![]);
    let o = value(w.on_registry_check(last_id(&w), registry_says("Unknown", 0)));
    assert_eq!(o.status, Status::Pending);
    assert!(o.reasons.iter().any(|r| r.contains("first payment")));
    assert_eq!(spent_near(&w), 0, "nothing is reserved while waiting for the owner");
}

#[test]
fn owner_approval_rechecks_the_registry() {
    let mut w = setup(true);
    as_agent(T0_MS);
    w.execute(pay("newshop.testnet", milli(100)), None);
    as_callback(T0_MS, vec![]);
    let id = value(w.on_registry_check(last_id(&w), registry_says("Unknown", 0))).id;

    // The receiver gets confirmed as a scam while the payment waits; approval must not pay it.
    as_owner(T0_MS + 60_000);
    assert!(is_promise(&w.approve(id)));
    assert_eq!(spent_near(&w), milli(100).0);
    as_callback(T0_MS + 60_000, vec![]);
    let o = value(w.on_registry_check(id, registry_says("Blocked", 1)));
    assert_eq!(o.status, Status::Blocked);
    assert_eq!(spent_near(&w), 0);
}

#[test]
fn owner_approval_overrides_an_unreviewed_flag() {
    let mut w = setup(true);
    as_agent(T0_MS);
    w.execute(pay(ALICE, milli(300)), None);
    as_callback(T0_MS, vec![]);
    let id = value(w.on_registry_check(last_id(&w), registry_says("Flagged", 1))).id;
    as_owner(T0_MS);
    w.approve(id);
    as_callback(T0_MS, vec![]);
    assert!(is_promise(&w.on_registry_check(id, registry_says("Flagged", 1))));
    assert_eq!(w.get_decision(id).unwrap().status, Status::Dispatched);
}

#[test]
fn freeze_during_registry_check_stops_payment() {
    let mut w = setup(true);
    as_agent(T0_MS);
    w.execute(pay(ALICE, milli(300)), None);
    as_owner(T0_MS);
    w.freeze(None);
    as_callback(T0_MS, vec![]);
    assert_eq!(value(w.on_registry_check(last_id(&w), registry_says("Unknown", 0))).status, Status::Blocked);
}

// `#[private]` is enforced by the wasm entry point, so it is covered by the testnet attack suite
// (agent/attacks), not by unit tests that call the Rust method directly.

#[test]
fn concurrent_requests_cannot_both_squeeze_under_the_limit() {
    // Both requests are reserved before either registry check returns.
    let mut w = setup(true);
    as_owner(T0_MS);
    let mut p = w.get_policy();
    p.limits.get_mut(NEAR_ASSET).unwrap().daily_limit = milli(1000);
    p.limits.get_mut(NEAR_ASSET).unwrap().max_per_tx = milli(700);
    p.limits.get_mut(NEAR_ASSET).unwrap().approval_above = milli(700);
    w.set_policy(p);
    as_agent(T0_MS);
    assert!(is_promise(&w.execute(pay(ALICE, milli(600)), None)));
    let o = value(w.execute(pay(ALICE, milli(600)), None));
    assert_eq!(o.status, Status::Blocked);
}

// ------------------------------------------------------------------ misc

#[test]
fn simulate_matches_execute() {
    let mut w = setup(false);
    as_agent(T0_MS);
    assert_eq!(w.simulate(pay(ALICE, milli(10))).status, Status::Dispatched);
    assert_eq!(w.simulate(pay("new.testnet", milli(10))).status, Status::Pending);
    assert_eq!(w.simulate(pay(ALICE, milli(5000))).status, Status::Blocked);
    w.execute(pay(ALICE, milli(10)), None);
}

#[test]
fn history_is_newest_first_and_bounded() {
    let mut w = setup(false);
    for i in 0..(HISTORY_LEN + 5) {
        as_agent(T0_MS + i);
        w.execute(pay(ALICE, milli(5000)), None); // blocked, does not count towards rate limit
    }
    let d = w.get_decisions(Some(200));
    assert_eq!(d.len(), HISTORY_LEN as usize);
    assert_eq!(d[0].id.0, HISTORY_LEN + 4);
    assert!(w.get_decision(U64(0)).is_none());
}

#[test]
fn report_scam_adds_to_denylist_and_counts_the_bond() {
    let mut w = setup(true);
    as_agent(T0_MS);
    w.report_scam(acc("drainer.testnet"), "Drainer".into(), "tx:abc".into());
    assert!(w.get_denylist().contains(&acc("drainer.testnet")));
    assert_eq!(spent_near(&w), REPORT_BOND.as_yoctonear());
}

#[test]
fn amounts_are_human_readable() {
    assert_eq!(fmt_amount(NEAR_ASSET, ONE_NEAR), "1 NEAR");
    assert_eq!(fmt_amount(NEAR_ASSET, ONE_NEAR * 3 / 2), "1.5 NEAR");
    assert_eq!(fmt_amount(NEAR_ASSET, milli(450).0), "0.45 NEAR");
    assert_eq!(fmt_amount("usdc.testnet", 1_000_000), "1000000 usdc.testnet");
}

#[test]
fn json_shape_of_actions_is_tagged() {
    let a: Action = serde_json::from_str(
        r#"{"type":"Transfer","receiver_id":"alice.testnet","amount":"1000"}"#,
    )
    .unwrap();
    assert_eq!(a, pay(ALICE, U128(1000)));
}

// ------------------------------------------------------------------ self-audit regressions

#[test]
fn proposals_count_against_rate_limit_and_queue_cap() {
    let mut w = setup(false);
    as_owner(T0_MS);
    let mut p = w.get_policy();
    p.max_actions_per_hour = 3;
    w.set_policy(p);
    for i in 0..3 {
        as_agent(T0_MS + i);
        assert_eq!(w.propose(pay(ALICE, milli(1)), "x".into(), None).status, Status::Pending);
    }
    as_agent(T0_MS + 10);
    let o = w.propose(pay(ALICE, milli(1)), "x".into(), None);
    assert_eq!(o.status, Status::Blocked);
    assert!(o.reasons[0].contains("rate limit"));

    // Queue cap: with a generous rate limit the queue still stops at MAX_PENDING.
    as_owner(T0_MS + 2 * HOUR_MS);
    let mut p = w.get_policy();
    p.max_actions_per_hour = 120;
    w.set_policy(p);
    for i in 0..(MAX_PENDING as u64) {
        as_agent(T0_MS + 2 * HOUR_MS + i);
        w.propose(pay(ALICE, milli(1)), "x".into(), None);
    }
    assert_eq!(w.get_pending().len() as u32, MAX_PENDING);
    as_agent(T0_MS + 2 * HOUR_MS + 100);
    let o = w.propose(pay(ALICE, milli(1)), "x".into(), None);
    assert!(o.reasons.iter().any(|r| r.contains("too many pending")));
    assert_eq!(w.get_pending().len() as u32, MAX_PENDING);
}

#[test]
fn expired_approvals_leave_the_queue() {
    let mut w = setup(false);
    as_agent(T0_MS);
    let first = value(w.execute(pay("newshop.testnet", milli(10)), None)).id;
    assert_eq!(w.get_pending().len(), 1);
    as_agent(T0_MS + 25 * HOUR_MS);
    w.execute(pay(ALICE, milli(10)), None);
    assert!(w.get_pending().is_empty());
    assert_eq!(w.get_decision(first).unwrap().status, Status::Expired);
}

#[test]
#[should_panic(expected = "Call arguments too large")]
fn oversized_call_args_are_refused_before_storage() {
    let mut w = setup(false);
    as_agent(T0_MS);
    w.execute(
        Action::Call {
            contract_id: acc("x.testnet"),
            method_name: "m".into(),
            args: Base64VecU8(vec![0u8; 50_000]),
            deposit: U128(0),
            gas: U64(Gas::from_tgas(10).as_gas()),
        },
        None,
    );
}

#[test]
#[should_panic(expected = "Memo too long")]
fn oversized_ft_memo_is_refused() {
    let mut w = setup(false);
    as_agent(T0_MS);
    w.execute(
        Action::FtTransfer { token_id: acc("usdc.testnet"), receiver_id: acc(ALICE), amount: U128(1), memo: Some("m".repeat(5000)) },
        None,
    );
}

#[test]
fn agent_reports_cannot_denylist_contacts() {
    let mut w = setup(true);
    as_agent(T0_MS);
    w.report_scam(acc(ALICE), "Other".into(), "e".into());
    assert!(!w.get_denylist().contains(&acc(ALICE)));
    w.report_scam(acc("evil.testnet"), "Drainer".into(), "e".into());
    assert!(w.get_denylist().contains(&acc("evil.testnet")));
}

#[test]
fn ownership_moves_in_two_steps() {
    let mut w = setup(false);
    as_owner(T0_MS);
    w.propose_owner(acc("newowner.testnet"));
    assert_eq!(w.get_status().owner_id, acc(OWNER));
    ctx("newowner.testnet", rogue_pk(), T0_MS, vec![]);
    w.accept_owner();
    assert_eq!(w.get_status().owner_id, acc("newowner.testnet"));
}

#[test]
#[should_panic(expected = "Only the proposed owner")]
fn strangers_cannot_accept_ownership() {
    let mut w = setup(false);
    as_owner(T0_MS);
    w.propose_owner(acc("newowner.testnet"));
    ctx("mallory.testnet", rogue_pk(), T0_MS, vec![]);
    w.accept_owner();
}
