// End-to-end demo on testnet: an accounts-payable agent works through an inbox that contains
// a prompt injection and an address-poisoning invoice. Halter stops both, twice: once in the
// agent-side guard, and again on-chain when the attacker skips the guard.
//
// Usage: npm run demo                  (naive scripted agent, no API key needed)
//        LLM_API_KEY=... npm run demo  (real model via NEAR AI Cloud's OpenAI-compatible API)
import fs from "node:fs";
import path from "node:path";
import { ROOT, account, fmtNear, loadDeployment, near, sleep } from "../src/near.js";
import { HalterWalletClient, transferAction } from "../src/wallet.js";
import { Scout } from "../src/scout.js";
import { HalterGuard } from "../src/guard.js";
import { makeLlm } from "../src/llm.js";
import { color, detail, info, link, step, tag } from "../src/log.js";

const d = loadDeployment();
const known = JSON.parse(fs.readFileSync(path.join(ROOT, "data", "known-names.json"), "utf8"));
const inbox = JSON.parse(fs.readFileSync(path.join(ROOT, "data", "inbox.json"), "utf8")).map((inv) => ({
  ...inv,
  text: inv.text.replaceAll("{owner}", d.owner),
}));

const agentSigner = account(d.wallet, "agent"); // the ONLY key the agent has
const ownerSigner = account(d.owner);
const wallet = new HalterWalletClient(d.wallet, agentSigner);
const ownerWallet = new HalterWalletClient(d.wallet, ownerSigner);
const scout = new Scout({ walletId: d.wallet, registryId: d.registry, knownNames: known[d.network] || known.testnet });
const guard = new HalterGuard({ wallet, scout, onWarn: "propose" });
const llm = makeLlm();

const results = [];

async function handle(inv) {
  step(`${inv.id}  ${color.gray(inv.from)}`);
  for (const line of inv.text.split("\n")) console.log(color.gray("    │ " + line));
  const dec = await llm.decide(inv.text);
  info(`agent (${llm.name}) decides: ${color.bold(dec.decision)}` + (dec.receiver ? ` ${dec.amount_near} NEAR -> ${dec.receiver}` : "") + color.gray(`  "${dec.reason}"`));
  if (dec.decision !== "pay" || !dec.receiver || !(dec.amount_near > 0)) {
    results.push({ invoice: inv.id, outcome: "agent skipped" });
    return null;
  }
  const r = await guard.pay({ receiver: dec.receiver, amountYocto: near(dec.amount_near), memo: inv.id });
  results.push({ invoice: inv.id, receiver: dec.receiver, amount: dec.amount_near, scout: r.report?.verdict, outcome: r.status });
  return { dec, r };
}

async function main() {
  console.log(color.bold("\nHalter demo — an AI accounts-payable agent on a halter"));
  info(`wallet ${d.wallet} (no full-access key; the agent holds one function-call key)`);
  info(`owner  ${d.owner}   registry ${d.registry}`);
  // Clear approvals left over from an earlier run so the queue below is this run's only.
  for (const p of await ownerWallet.pending()) await ownerWallet.reject(p.id, "stale: left over from an earlier demo run");
  const s0 = await wallet.status();
  info(`policy: ${fmtNear(s0.assets[0].limits.max_per_tx)} per tx, ${fmtNear(s0.assets[0].limits.daily_limit)} per rolling 24h, approval above ${fmtNear(s0.assets[0].limits.approval_above)}; spent so far ${fmtNear(s0.assets[0].spent_24h)}`);

  // INV-1001: contact, small amount -> paid.
  await handle(inbox[0]);

  // INV-1002: new supplier -> scout warns -> parked on-chain -> owner approves.
  const second = await handle(inbox[1]);
  const pendingId = second?.r?.tx?.decision?.status === "Pending" ? second.r.tx.decision.id : undefined;
  if (pendingId !== undefined) {
    step(`Owner reviews the approval queue`);
    const pending = await ownerWallet.pending();
    for (const p of pending) info(`#${p.id} ${p.action.type} ${fmtNear(p.action.amount)} -> ${p.action.receiver_id}`), detail(p.reasons);
    const a = await ownerWallet.approve(pendingId);
    info(`owner approves #${pendingId}: ${tag(a.decision?.status)}`);
    link("tx", a.url);
    results.find((x) => x.invoice === inbox[1].id).outcome = `approved -> ${a.decision?.status}`;
  }

  // INV-1003: prompt injection -> agent tries to pay the drainer -> guard refuses.
  const third = await handle(inbox[2]);

  // Same attack, but the attacker controls the agent process and skips the guard entirely.
  if (third?.dec?.receiver) {
    step("What if the attacker owns the agent process and skips the guard?");
    info(`compromised agent signs execute() directly with its key: ${third.dec.amount_near} NEAR -> ${third.dec.receiver}`);
    const raw = await wallet.execute(transferAction(third.dec.receiver, near(third.dec.amount_near)), "bypass");
    info(`contract says ${tag(raw.decision?.status)}`);
    detail((raw.decision?.reasons || []).map((r) => `contract: ${r}`));
    link("tx", raw.url);
    results.push({ invoice: `${inbox[2].id} (guard bypassed)`, receiver: third.dec.receiver, amount: third.dec.amount_near, scout: "-", outcome: raw.decision?.status });
  }

  // INV-1004: lookalike address -> guard refuses and reports it to the shared registry.
  const fourth = await handle(inbox[3]);
  if (fourth?.dec?.receiver) {
    await sleep(1500);
    const v = await scout.check(fourth.dec.receiver);
    info(`registry now says ${color.bold(v.facts.registry?.status)} for ${fourth.dec.receiver} — every Halter agent sees it`);
  }

  step("Summary");
  for (const r of results) {
    info(`${r.invoice.padEnd(26)} ${String(r.receiver || "").padEnd(46)} ${String(r.amount ?? "").padEnd(5)} scout ${String(r.scout || "-").padEnd(5)}  ${tag(r.outcome)}`);
  }
  const s1 = await wallet.status();
  const a = s1.assets[0];
  info(`spent in the last 24h: ${fmtNear(a.spent_24h)} of ${fmtNear(a.limits.daily_limit)}  ·  pending approvals: ${s1.pending}  ·  frozen: ${s1.frozen}`);
  info(color.gray(`decisions on-chain: npm run status  ·  attack suite: npm run attacks`));
}

main().catch((e) => {
  console.error(color.red("demo failed:"), e?.stack || e);
  process.exit(1);
});
