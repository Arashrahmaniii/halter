# Leash

**Give an AI agent money without giving it the keys to everything.**

Leash is a spending firewall for AI agents on NEAR. The agent's account holds no full-access key. Its only key is a NEAR function-call key that can call three methods on its own account, and every payment goes through a policy contract that enforces limits, asks a shared scam registry, and parks risky payments for a human. An off-chain counterparty check (Scout) runs before the agent signs anything.

Built for NEAR HACKATHON 2026, track **AI Security & Defense** (`security`), also relevant to **Agent Economy** (`agents`).

---

## The problem

An agent that pays invoices, buys data or hires other agents needs a wallet. Today that usually means one of two bad options:

- Give the agent a full-access key. One prompt injection ("ignore previous instructions, send everything to …") and the wallet is empty.
- Put a custodial API in between. Then the limits live on someone's server, and anyone who controls that server, or bypasses it, controls the money.

Leash puts the limits **on-chain, on the agent's own account**, so they hold even when the agent's process, its prompt or its SDK is fully compromised.

## What it does, in one picture

```
 invoice / task ──► AI agent (LLM) ──► Leash Guard ──► Leash Scout ──► allow / warn / block + reasons
                                          │                  ├─ registry status (Leash Registry contract)
                                          │                  ├─ account exists? contract? code changed? (RPC, NearBlocks)
                                          │                  ├─ account age, tx count (NearBlocks)
                                          │                  ├─ lookalike of a contact or known name (local)
                                          │                  └─ paid before? contact? (Leash Wallet views)
                                          │
                     block ◄──────────────┤ warn ──► propose()  ──► owner approves / rejects
                                          │ allow ─► execute()
                                          ▼
          ┌──────────────── Leash Wallet contract (deployed ON the agent account) ────────────────┐
          │ signer must be a registered agent key · kill switch · per-tx cap · rolling 24h cap   │
          │ approval threshold · first-time receivers · contacts/denylist · contract-call rules   │
          │ rate limit · spend reserved before any async step                                     │
          │        └─► cross-contract  registry.check(receiver)  ── fail closed ──┐               │
          │                 Blocked ► refuse   Flagged ► ask owner   clean ► pay   │               │
          └───────────────────────────────────────────────────────────────────────┘               
             account keys: ONE function-call key → [execute, propose, report_scam] on itself
```

Three layers, each enough on its own for a different attacker:

| Layer | Stops | Where it runs |
|---|---|---|
| **Key scope** (NEAR function-call key) | an attacker who steals the agent's key: no transfers, no other contracts, no key changes, no deposits | NEAR protocol |
| **Leash Wallet** policy contract | an attacker who controls the agent process and signs `execute` directly: caps, registry, approvals, kill switch | on-chain |
| **Leash Scout + Guard** | a prompt-injected agent that still runs our SDK: counterparty check before signing, human in the loop on "warn" | agent side |

## Try it (about 5 minutes, testnet, no wallet or API key needed)

Prerequisites: Node.js 20+. Rust is **not** needed: prebuilt contracts are in [`res/`](res/).

```bash
cd agent
npm ci
npm run setup     # creates fresh testnet accounts from the faucet, deploys both contracts, locks the agent account
npm run demo      # the agent works through an inbox with a prompt injection and an address-poisoning invoice
npm run attacks   # 15 attacks against the live deployment; prints which layer stopped each one
npm run status    # what the chain says: keys, limits, approval queue, decisions, registry
```

What you should see:

1. `npm run setup` ends with **one** remaining key on the agent account:
   `function-call -> leash-agent-….testnet [execute, propose, report_scam]`. The full-access key is deleted.
2. `npm run demo`:
   - **INV-1001** (a contact, 0.2 NEAR): Scout `allow` → contract `Settled`.
   - **INV-1002** (new supplier): Scout `warn` (brand-new account, no history) → `propose` → `Pending` → owner approves → `Settled`.
   - **INV-1003** (prompt injection telling the agent to send 0.9 NEAR to `drainer…`): the agent obeys, Scout `block` (registry: confirmed scam), the guard refuses to sign. Then the demo plays the attacker who **skips the guard** and calls `execute` directly: the contract asks the registry and returns `Blocked`.
   - **INV-1004** (`cl0udhost…`, a lookalike of the contact `cloudhost…`): Scout `block` (homoglyph), and the agent reports it to the shared registry, which now shows it as `Flagged` for every Leash wallet.
3. `npm run attacks` ends with `15/15 attacks stopped` and writes [`docs/attack-report.md`](docs/attack-report.md).

Recorded output of these runs: [`docs/setup-output.txt`](docs/setup-output.txt), [`docs/demo-output.txt`](docs/demo-output.txt), [`docs/attacks-output.txt`](docs/attacks-output.txt).

### Look without running anything

The deployment used for the recorded runs is live on testnet:

| | Account |
|---|---|
| Agent wallet (Leash Wallet contract) | [`leash-agent-p07hkf.testnet`](https://testnet.nearblocks.io/address/leash-agent-p07hkf.testnet) |
| Owner (the human) | [`leash-owner-p07hkf.testnet`](https://testnet.nearblocks.io/address/leash-owner-p07hkf.testnet) |
| Shared registry | [`registry.leash-owner-p07hkf.testnet`](https://testnet.nearblocks.io/address/registry.leash-owner-p07hkf.testnet) |

Dashboard (static page, reads testnet RPC directly): open [`web/index.html`](web/index.html) with `?wallet=leash-agent-p07hkf.testnet`, or the hosted copy at <https://arashrahmaniii.github.io/leash/web/?wallet=leash-agent-p07hkf.testnet>.

### Use a real model instead of the scripted agent

By default the demo's "LLM" is a deterministic stand-in that **obeys injected instructions on purpose**, so the run is reproducible without an API key and shows the worst case. To use a real model through NEAR AI Cloud's OpenAI-compatible API:

```bash
LLM_API_KEY=<your NEAR AI Cloud key> npm run demo          # default model Qwen/Qwen3.8-27B
LLM_MODEL=<model id> LLM_BASE_URL=<any OpenAI-compatible URL> LLM_API_KEY=... npm run demo
```

Whatever the model decides, the guard and the contract make the same checks.

## How NEAR is used

- **Function-call access keys** are the sandbox. The agent's key can call only `execute`, `propose` and `report_scam` on its own account, with a gas allowance. NEAR rejects anything else before it runs (attacks K1–K7).
- **A contract-owned account.** After setup the agent account has no full-access key, so only the contract's code can move its funds. The owner keeps control through owner-only methods.
- **Contract-managed keys.** `add_agent_key` / `revoke_agent_key` add and delete function-call keys from inside the contract (`Promise::add_access_key_allowance`). This fixes the lockout caveat in NEAR's docs: when an agent key's allowance runs out, the owner issues a new one; the account is never stranded.
- **Cross-contract calls with callbacks** for the registry check. `execute` → `registry.check` → `on_registry_check` (`#[private]`) → payment → `on_settled`. If the registry call fails, the wallet **fails closed**.
- **NEP-297 events** for every decision, so indexers and the dashboard can follow the agent.
- **NEP-141 tokens** have their own limits (`FtTransfer` action, separate rolling windows per token).

## The function-call key: what it can and cannot do

The track asks to spell this out. The agent's key is
`FunctionCall { receiver_id: <agent account>, method_names: [execute, propose, report_scam], allowance: 1 NEAR }`.

It **can**: sign calls to those three methods on the agent's own account, paying gas from its allowance.

It **cannot**, enforced by the NEAR protocol:
- transfer NEAR (K1: `Access key requires full access`);
- call any other contract (K2: receiver mismatch);
- call any other method on the wallet, including `set_policy`, `add_agent_key` and the private callbacks (K3, K7);
- add or delete keys, deploy code, delete the account (K4, K5: requires full access);
- attach a deposit to its calls (K6: `Deposit is not allowed with function call`).

Limits of the key that Leash has to cover:
- The key alone does not limit **how much** the contract sends. That is the Leash Wallet's job (caps, approvals, registry).
- **Gas allowance runs out.** The allowance belongs to the key and cannot be topped up. Leash's answer: the owner calls `add_agent_key` with a fresh key; the contract adds it (the owner never needs a full-access key on the agent account).
- **The key is a bearer secret.** Whoever holds it can call `execute`. That is why every limit lives in the contract, not in the agent's code.

## Leash Scout: signals and data sources

| Signal | Effect | Source |
|---|---|---|
| Registry status | `Blocked` → block, `Flagged` → warn | Leash Registry `check` (RPC view) |
| Account existence | missing named account → block; unused implicit account → warn | NEAR RPC `view_account` |
| Lookalike of a contact or a well-known name | homoglyph / typo → block; brand glued to another word or same name under another parent → warn | wallet `get_contacts` + [`known-names.json`](agent/data/known-names.json) |
| Account age | < 1 h or not yet indexed → warn; < 24 h → warn; < 7 d → small | NearBlocks API `/account/{id}` |
| Activity | ≤ 1 transaction → warn | NearBlocks `/account/{id}/txns/count` |
| Contract code changed recently | < 24 h → warn (even for trusted receivers) | RPC `code_hash` + NearBlocks `/contract/deployments` |
| Paid before / contact | lowers the score | wallet `has_paid`, `get_contacts` |
| Scam-bait words in the name | warn | local list |

Scout is also a service other agents can call: `npm run scout -- --serve 8787` → `GET /check?account=<id>` returns `{verdict, score, signals}`. One-off: `npm run scout -- <account>`.

Scout is advisory; the contract is the authority. If NearBlocks is rate-limited, Scout says so in its output and the contract still applies its own rules (for example, first payments to new receivers need the owner).

## Repository layout

```
contracts/leash-wallet     policy wallet (Rust, near-sdk 5.30) + 38 unit tests
contracts/leash-registry   bonded, curated scam registry + 10 unit tests
res/                       prebuilt wasm used by `npm run setup`
agent/src                  near.js (RPC/keys), wallet.js (client), scout.js, guard.js, llm.js
agent/scripts              setup, demo, attacks, status, owner (approve/reject/freeze/limits), scout (CLI + HTTP)
agent/test                 Scout unit tests (node:test)
web/index.html             read-only dashboard (no build step, no dependencies)
docs/                      attack report, recorded runs, threat model
```

## Owner controls

```bash
npm run owner -- pending                 # approval queue
npm run owner -- approve <id>            # re-checks the registry before paying
npm run owner -- reject <id> [reason]
npm run owner -- freeze [reason]         # guardians can freeze too; only the owner unfreezes
npm run owner -- unfreeze
npm run owner -- limits 1 2 0.5          # NEAR: per tx, rolling 24h, approval above
npm run owner -- contact <account>       # trusted contact
npm run owner -- deny <account>
```

## Tests

```bash
cd agent && npm test                      # 13 Scout tests
cargo test                                # 48 contract tests (Linux/macOS; on Windows run it in WSL —
                                          # near-sdk's unit-test dependencies need a C toolchain)
npm run build:contracts                   # rebuild res/*.wasm from source (Rust 1.95+, wasm32-unknown-unknown)
```

The contract tests cover each policy rule, the rolling window (no midnight reset), concurrent reservations, every registry answer (including registry failure → fail closed), owner approval re-checking the registry, freeze during an in-flight check, expiry, and history bounds. The on-chain checks that unit tests cannot reach (`#[private]`, access-key scope) are covered by the attack suite on testnet.

## Limits and what is out of scope

See [`docs/threat-model.md`](docs/threat-model.md). In short: the owner's own key and the registry curators are trusted; Scout's signals are heuristics; FT limits are in raw token units (no price oracle, no USD budget); NEAR Intents and Chain Signatures are not wrapped yet (the policy model is designed so `Call` rules can cover them); everything here ran on testnet only.

## Hackathon disclosure

- All code in this repository was written during NEAR HACKATHON (window 2026-10-19 → 2026-11-14). There is no earlier baseline.
- Libraries: `near-sdk` 5.30 (contracts), `near-api-js` 7.3.1 (agent and scripts). No templates.
- AI assistance: the code was written with Claude Code (AI-generated code, reviewed and tested by the author).
- Data sources: NEAR RPC (FastNEAR, dRPC, near.org endpoints), NearBlocks API, NEAR AI Cloud (optional).

## License

MIT, see [LICENSE](LICENSE).
