# Threat model

## Assets

- The NEAR and NEP-141 tokens held by the agent account.
- The integrity of the policy (limits, contacts, registry, call rules).
- The shared registry's verdicts, which many wallets rely on.

## Actors and what they control

| Actor | Trusted? | Controls |
|---|---|---|
| Owner | yes | owner account key; can change policy, approve, withdraw (`owner_execute`) |
| Guardian | partly | can only `freeze` |
| Agent process / LLM | **no** | the agent's function-call key, the prompt, the SDK |
| Counterparties | no | their own accounts and contracts |
| Registry reporters | no | can file bonded reports (0.01 NEAR each) |
| Registry curators | yes | `confirm` / `clear` / `block` |

## Attacks considered and the control that stops each

| Attack | Control | Evidence |
|---|---|---|
| Stolen agent key used for a plain transfer, other contracts, key changes, deposits, account deletion | NEAR function-call key scope | attack suite K1–K7 |
| Forged registry callback | `#[private]` on callbacks | K8 |
| Prompt-injected payment to a known scam account, with or without the SDK | registry check inside `execute`, fail closed | demo INV-1003, P1 |
| Address poisoning / lookalike receiver | Scout lookalike check; contract: first payment needs owner; reported accounts are denylisted and flagged | demo INV-1004, P2 |
| Fresh scam account | Scout age/activity/name signals → owner approval | P3 |
| Single large payment | per-transaction cap | P4 |
| Drain through many small payments | rolling 24 h window with hourly buckets (no reset at midnight), spend reserved before async steps | P5, unit tests |
| Agent changes its own policy through a `Call` action | the wallet refuses to call itself; call rules are an allowlist | P6 |
| Ongoing incident | kill switch (owner or guardian), checked again inside the async callback | P7, unit tests |
| Owner unknowingly approves a payment to an account confirmed as a scam after the request | `approve` re-runs the registry check | unit test `owner_approval_rechecks_the_registry` |
| Two concurrent requests both fitting under the limit | reservation at `execute` time | unit test `concurrent_requests_cannot_both_squeeze_under_the_limit` |
| Registry down or returning garbage | wallet blocks (fail closed) | unit test `registry_failure_fails_closed` |
| Gas-allowance exhaustion locks the agent out | owner adds a new key through the contract | `add_agent_key` |

## Out of scope / known limits

- **Owner compromise.** Whoever holds the owner key controls the wallet. Use a hardware wallet or a multisig (for example a Sputnik DAO or Trezu account) as the owner.
- **Curator trust.** A malicious curator can block a legitimate account (denial of payment) or clear a scam. Reports are bonded and reviews are public events; a DAO-run curator set is the natural next step.
- **Scout is heuristic.** It can miss a scam that looks like an old, active, unflagged account, and it can warn on legitimate new suppliers. The contract's first-payment approval is the backstop.
- **Token amounts** are raw units per token; there is no price oracle or USD budget.
- **Allowed contract calls** are only as safe as the contracts the owner allowlists. A `Call` rule to a DEX or to NEAR Intents should be narrow (method and deposit).
- **NEAR Intents and Chain Signatures** are not wrapped yet. The `Call` rule model is how they would be: allow `intents.near` / `v1.signer` with specific methods and deposits, and let the policy cap the deposit.
- **Testnet only.** Nothing here has been audited or deployed to mainnet.
