// Print what the chain says about the deployed Leash wallet: keys, policy, spend, queue, decisions.
// Usage: npm run status [-- --json]
import { accessKeys, fmtNear, loadDeployment, view } from "../src/near.js";
import { color, info, step, tag } from "../src/log.js";

const d = loadDeployment();
const [status, policy, pending, decisions, keys, entries] = await Promise.all([
  view(d.wallet, "get_status"),
  view(d.wallet, "get_policy"),
  view(d.wallet, "get_pending"),
  view(d.wallet, "get_decisions", { limit: 15 }),
  accessKeys(d.wallet),
  view(d.registry, "list_entries", { limit: 50 }),
]);

if (process.argv.includes("--json")) {
  console.log(JSON.stringify({ deployment: d, status, policy, pending, decisions, keys, registry: entries }, null, 2));
  process.exit(0);
}

step(`Wallet ${d.wallet}`);
info(`owner ${status.owner_id} · guardians ${status.guardians.join(", ") || "-"} · frozen ${status.frozen ? color.red("YES") : "no"} · balance ${fmtNear(status.balance)}`);

step("Keys on the account (from RPC, not from the contract)");
for (const k of keys) {
  const p = k.access_key.permission;
  info(
    p === "FullAccess"
      ? color.red(`${k.public_key}  FULL ACCESS`)
      : `${k.public_key}  function-call -> ${p.FunctionCall.receiver_id} [${p.FunctionCall.method_names.join(", ")}] allowance ${p.FunctionCall.allowance ? fmtNear(p.FunctionCall.allowance) : "unlimited"}`,
  );
}

step("Policy and spend (rolling 24h)");
for (const a of status.assets) {
  info(`${a.asset}: spent ${fmtNear(a.spent_24h)} of ${fmtNear(a.limits.daily_limit)} · per tx ≤ ${fmtNear(a.limits.max_per_tx)} · approval above ${fmtNear(a.limits.approval_above)}`);
}
info(`receivers: ${policy.receiver_mode}${policy.approve_new_receivers ? ", first payment to a new receiver needs approval" : ""} · rate ${status.actions_last_hour}/${status.max_actions_per_hour} per hour · registry ${policy.registry_id || "-"}${policy.block_flagged ? " (flagged = block)" : " (flagged = ask owner)"}`);

step(`Approval queue (${pending.length})`);
for (const p of pending) info(`#${p.id} ${p.action.type} ${fmtNear(p.action.amount || p.action.deposit)} -> ${p.action.receiver_id || p.action.contract_id}  ${color.gray(p.reasons.join("; "))}`);

step("Latest decisions");
for (const x of decisions) {
  const a = x.action;
  info(`#${String(x.id).padEnd(3)} ${tag(x.status)} ${a.type} ${fmtNear(a.amount || a.deposit || 0)} -> ${a.receiver_id || a.contract_id} ${color.gray(`${x.requested_by}${x.memo ? " · " + x.memo : ""}`)}`);
  if (x.reasons?.length) info(color.gray(`      ${x.reasons.join("; ")}`));
}

step(`Shared registry ${d.registry}`);
for (const [acct, e] of entries) info(`${acct}  ${tag(e.status)} reports ${e.total_reports}${e.review_reason ? color.gray(" · " + e.review_reason) : ""}`);
