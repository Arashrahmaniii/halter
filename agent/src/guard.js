// Leash Guard: the agent-side wrapper. The agent never talks to the wallet directly; it asks
// the guard to pay, and the guard
//   1. asks Scout about the receiver,
//   2. refuses to sign on "block" (and reports clear scams to the shared registry),
//   3. asks a human on "warn" (on-chain approval queue, or an interactive prompt),
//   4. signs `execute` with the agent's function-call key on "allow".
// The on-chain policy still has the last word: even an allowed payment can come back
// Pending or Blocked from the contract.
import readline from "node:readline/promises";
import { explain } from "./scout.js";
import { transferAction } from "./wallet.js";
import { fmtNear } from "./near.js";
import { color, detail, info, link, tag } from "./log.js";

export class LeashGuard {
  /**
   * @param {object} o
   * @param {import('./wallet.js').LeashWalletClient} o.wallet  wallet client signed by the agent key
   * @param {import('./scout.js').Scout} o.scout
   * @param {"propose"|"prompt"|"refuse"} [o.onWarn]  how to involve the human on "warn"
   * @param {boolean} [o.autoReport]  report lookalikes to the shared registry
   */
  constructor({ wallet, scout, onWarn = "propose", autoReport = true, quiet = false }) {
    this.wallet = wallet;
    this.scout = scout;
    this.onWarn = onWarn;
    this.autoReport = autoReport;
    this.quiet = quiet;
  }

  log(...a) {
    if (!this.quiet) info(...a);
  }

  /** Pay native NEAR. Returns { status, stage, report?, tx? }. */
  async pay({ receiver, amountYocto, memo = null, reason = "" }) {
    const action = transferAction(receiver, amountYocto);
    this.log(color.bold(`pay ${fmtNear(amountYocto)} -> ${receiver}`) + (memo ? color.gray(`  (${memo})`) : ""));

    const report = await this.scout.check(receiver);
    this.log(`scout ${tag(report.verdict)} score ${report.score}`);
    if (!this.quiet) detail(explain(report));

    if (report.verdict === "block") {
      this.log(color.red("guard refuses to sign."));
      if (this.autoReport) await this.maybeReport(receiver, report);
      return { status: "refused", stage: "scout", report };
    }

    if (report.verdict === "warn") {
      const why = report.signals
        .filter((s) => s.level === "warn" || s.level === "block")
        .map((s) => `${s.id}: ${s.detail}`)
        .join("; ")
        .slice(0, 240);
      if (this.onWarn === "refuse") return { status: "refused", stage: "scout-warn", report };
      if (this.onWarn === "prompt") {
        const ok = await askHuman(`Scout warns about ${receiver}: ${why}\nPay ${fmtNear(amountYocto)} anyway? [y/N] `);
        if (!ok) return { status: "refused", stage: "human", report };
      } else {
        const tx = await this.wallet.propose(action, `scout warn: ${why}`, memo);
        this.log(`on-chain ${tag(tx.decision?.status)} id ${tx.decision?.id} — waiting for the owner`);
        link("tx", tx.url);
        return { status: tx.decision?.status, stage: "propose", report, tx };
      }
    }

    const tx = await this.wallet.execute(action, memo);
    this.log(`on-chain ${tag(tx.decision?.status)} id ${tx.decision?.id}`);
    if (tx.decision?.reasons?.length && !this.quiet) detail(tx.decision.reasons.map((r) => `contract: ${r}`));
    link("tx", tx.url);
    return { status: tx.decision?.status, stage: "execute", report, tx };
  }

  async maybeReport(receiver, report) {
    const la = report.signals.find((s) => s.id === "lookalike" && s.level === "block");
    const already = report.facts?.registry?.status === "Blocked" || report.facts?.registry?.status === "Flagged";
    if (!la || already || report.facts?.exists === false) return;
    try {
      const r = await this.wallet.reportScam(receiver, "Impersonation", `leash-scout: ${la.detail}`);
      this.log(color.magenta(`reported ${receiver} to the shared registry`) + color.gray(" (others' agents will now see it flagged)"));
      link("tx", r.url);
    } catch (e) {
      this.log(color.gray(`could not report: ${String(e.message || e).slice(0, 120)}`));
    }
  }
}

async function askHuman(question) {
  if (!process.stdin.isTTY) return false;
  const rl = readline.createInterface({ input: process.stdin, output: process.stdout });
  try {
    return /^y(es)?$/i.test((await rl.question(question)).trim());
  } finally {
    rl.close();
  }
}
