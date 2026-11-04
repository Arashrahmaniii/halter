// Client for a deployed Halter Wallet contract.
import { call, halterEvents, teraToGas, view } from "./near.js";

export const GAS = {
  execute: teraToGas("150"),
  propose: teraToGas("30"),
  report: teraToGas("60"),
  owner: teraToGas("150"),
};

export const transferAction = (receiverId, amountYocto) => ({
  type: "Transfer",
  receiver_id: receiverId,
  amount: String(amountYocto),
});

export const ftAction = (tokenId, receiverId, amount, memo = null) => ({
  type: "FtTransfer",
  token_id: tokenId,
  receiver_id: receiverId,
  amount: String(amount),
  memo,
});

export class HalterWalletClient {
  /**
   * @param walletId the agent account the contract is deployed on
   * @param signer   near-api-js Account used to sign (agent FC key, owner, or guardian)
   */
  constructor(walletId, signer) {
    this.walletId = walletId;
    this.signer = signer;
  }

  // ------------------------------------------------ agent methods

  async execute(action, memo = null) {
    const r = await call(this.signer, this.walletId, "execute", { action, memo }, { gas: GAS.execute });
    return withDecision(r);
  }

  async propose(action, reason, memo = null) {
    const r = await call(this.signer, this.walletId, "propose", { action, reason, memo }, { gas: GAS.propose });
    return withDecision(r);
  }

  async reportScam(accountId, category, evidence) {
    return call(this.signer, this.walletId, "report_scam", { account_id: accountId, category, evidence }, { gas: GAS.report });
  }

  // ------------------------------------------------ owner / guardian methods

  async approve(id) {
    return withDecision(await call(this.signer, this.walletId, "approve", { id: String(id) }, { gas: GAS.owner }));
  }

  async reject(id, reason) {
    return withDecision(await call(this.signer, this.walletId, "reject", { id: String(id), reason }, { gas: GAS.propose }));
  }

  async freeze(reason) {
    return call(this.signer, this.walletId, "freeze", { reason }, { gas: GAS.propose });
  }

  async unfreeze() {
    return call(this.signer, this.walletId, "unfreeze", {}, { gas: GAS.propose });
  }

  async setPolicy(policy) {
    return call(this.signer, this.walletId, "set_policy", { policy }, { gas: GAS.propose });
  }

  // ------------------------------------------------ views

  status() {
    return view(this.walletId, "get_status");
  }
  policy() {
    return view(this.walletId, "get_policy");
  }
  pending() {
    return view(this.walletId, "get_pending");
  }
  decisions(limit = 20) {
    return view(this.walletId, "get_decisions", { limit });
  }
  decision(id) {
    return view(this.walletId, "get_decision", { id: String(id) });
  }
  simulate(action) {
    return view(this.walletId, "simulate", { action });
  }
  contacts() {
    return view(this.walletId, "get_contacts");
  }
}

/** Attach the final on-chain decision (from the return value or the last decision event). */
function withDecision(r) {
  const events = halterEvents(r.logs).filter((e) => e.standard === "halter" && e.event === "decision");
  const last = events.at(-1)?.data?.[0];
  const outcome = r.value && typeof r.value === "object" && "status" in r.value ? r.value : undefined;
  return {
    ...r,
    decision: outcome
      ? { id: Number(outcome.id), status: outcome.status, reasons: outcome.reasons || [] }
      : last
        ? { id: Number(last.id), status: last.status, reasons: last.reasons || [] }
        : undefined,
    events,
  };
}
