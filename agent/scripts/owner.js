// The human owner's controls, signed with the owner's key.
// Usage:
//   npm run owner -- pending
//   npm run owner -- approve <id>
//   npm run owner -- reject <id> [reason]
//   npm run owner -- freeze [reason]        (owner or guardian)
//   npm run owner -- unfreeze
//   npm run owner -- limits <max_per_tx> <daily_limit> <approval_above>   (NEAR)
//   npm run owner -- contact <account>      add a trusted contact
//   npm run owner -- deny <account>
//   npm run owner -- rotate-key [allowance]  new agent key (old one revoked), e.g. when its gas allowance ran out
import { account, call, fmtNear, loadDeployment, loadKey, near, newKeyPair, saveKey, teraToGas } from "../src/near.js";
import { LeashWalletClient } from "../src/wallet.js";
import { color, detail, info, link, tag } from "../src/log.js";

const d = loadDeployment();
const [cmd, ...args] = process.argv.slice(2);
const owner = account(d.owner);
const w = new LeashWalletClient(d.wallet, owner);

const show = (r) => {
  if (r.decision) info(`${tag(r.decision.status)} #${r.decision.id}`), detail(r.decision.reasons);
  link("tx", r.url);
};

switch (cmd) {
  case "pending":
    for (const p of await w.pending()) {
      info(`#${p.id} ${p.action.type} ${fmtNear(p.action.amount || p.action.deposit)} -> ${p.action.receiver_id || p.action.contract_id} (by ${p.requested_by}, expires ${new Date(Number(p.expires_at_ms)).toISOString()})`);
      detail(p.reasons);
    }
    break;
  case "approve":
    show(await w.approve(args[0]));
    break;
  case "reject":
    show(await w.reject(args[0], args.slice(1).join(" ") || null));
    break;
  case "freeze":
    show(await w.freeze(args.join(" ") || null));
    break;
  case "unfreeze":
    show(await w.unfreeze());
    break;
  case "limits": {
    const [perTx, daily, approval] = args.map(Number);
    if (![perTx, daily, approval].every((x) => x >= 0)) throw new Error("usage: limits <max_per_tx> <daily_limit> <approval_above>");
    const p = await w.policy();
    p.limits.near = { max_per_tx: near(perTx).toString(), daily_limit: near(daily).toString(), approval_above: near(approval).toString() };
    show(await w.setPolicy(p));
    break;
  }
  case "contact":
    show(await call(owner, d.wallet, "add_contact", { account_id: args[0] }, { gas: teraToGas("30") }));
    break;
  case "deny":
    show(await call(owner, d.wallet, "deny", { account_id: args[0] }, { gas: teraToGas("30") }));
    break;
  case "rotate-key": {
    const old = loadKey(d.wallet, "agent");
    const kp = newKeyPair();
    const added = await call(owner, d.wallet, "add_agent_key", { public_key: kp.getPublicKey().toString(), label: "invoice-bot", allowance: near(Number(args[0] || 1)).toString() }, { gas: teraToGas("30") });
    link("added", added.url);
    saveKey(d.wallet, kp, "agent");
    const revoked = await call(owner, d.wallet, "revoke_agent_key", { public_key: old.public_key }, { gas: teraToGas("30") });
    link("revoked", revoked.url);
    info(`new agent key ${kp.getPublicKey().toString()}; run npm run publish-key to update the red-team page`);
    break;
  }
  default:
    console.log(color.gray("commands: pending | approve <id> | reject <id> [reason] | freeze [reason] | unfreeze | limits <tx> <daily> <approval> | contact <acct> | deny <acct> | rotate-key [allowance]"));
}
