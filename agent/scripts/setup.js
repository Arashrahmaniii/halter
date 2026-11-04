// One-time testnet setup:
//   owner (human) account, agent wallet account, registry, guardian, demo counterparties.
// Then the agent account is LOCKED: its full-access key is deleted, so the only key left is a
// function-call key that can call execute / propose / report_scam on itself.
//
// Usage: npm run setup            (creates fresh accounts from the testnet faucet)
//        npm run setup -- --force (replace an existing .halter/deployment.json)
import fs from "node:fs";
import path from "node:path";
import { Account, actions } from "near-api-js";
import {
  DEPLOYMENT_FILE,
  NETWORK,
  ROOT,
  accessKeys,
  account,
  call,
  near,
  newKeyPair,
  provider,
  saveDeployment,
  saveKey,
  sleep,
  teraToGas,
  txUrl,
  viewAccount,
} from "../src/near.js";
import { color, info, link, step } from "../src/log.js";

// Prebuilt contracts are committed in res/ so judges do not need Rust; `npm run build:contracts`
// rebuilds them from source and refreshes res/.
const WASM_DIR = path.resolve(ROOT, "..", "res");
const WALLET_WASM = path.join(WASM_DIR, "halter_wallet.wasm");
const REGISTRY_WASM = path.join(WASM_DIR, "halter_registry.wasm");

const force = process.argv.includes("--force");
if (fs.existsSync(DEPLOYMENT_FILE) && !force) {
  console.log(`Deployment already exists at ${DEPLOYMENT_FILE}. Use --force to create a new one.`);
  process.exit(0);
}
for (const f of [WALLET_WASM, REGISTRY_WASM]) {
  if (!fs.existsSync(f)) {
    console.error(`Missing ${f}. Build the contracts first: npm run build:contracts`);
    process.exit(1);
  }
}

const rand = Math.random().toString(36).slice(2, 8);
const OWNER = process.env.HALTER_OWNER || `halter-owner-${rand}.testnet`;
const AGENT = process.env.HALTER_AGENT || `halter-agent-${rand}.testnet`;
const sub = (name) => `${name}.${OWNER}`;

async function faucetAccount(accountId) {
  const kp = newKeyPair();
  const res = await fetch(NETWORK.faucetUrl, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ newAccountId: accountId, newAccountPublicKey: kp.getPublicKey().toString() }),
  });
  const body = await res.text();
  if (!res.ok) throw new Error(`Faucet refused ${accountId}: HTTP ${res.status} ${body.slice(0, 200)}`);
  saveKey(accountId, kp);
  for (let i = 0; i < 20; i++) {
    if (await viewAccount(accountId)) return kp;
    await sleep(1500);
  }
  throw new Error(`Faucet accepted ${accountId} but the account did not appear`);
}

async function subAccount(parent, prefix, amountNear) {
  const kp = newKeyPair();
  const id = `${prefix}.${parent.accountId}`;
  const r = await parent.createSubAccount({ accountOrPrefix: prefix, publicKey: kp.getPublicKey().toString(), nearToTransfer: near(amountNear) });
  saveKey(id, kp);
  return { id, hash: r?.transaction?.hash };
}

async function deployAndInit(acct, wasmFile, initArgs) {
  const code = new Uint8Array(fs.readFileSync(wasmFile));
  const r = await acct.signAndSendTransaction({
    receiverId: acct.accountId,
    actions: [actions.deployContract(code), actions.functionCall("new", initArgs, teraToGas("100"), 0n)],
  });
  return r?.transaction?.hash;
}

const yocto = (n) => near(n).toString();

async function main() {
  step("1. Accounts from the testnet faucet");
  info(`owner (the human): ${color.bold(OWNER)}`);
  await faucetAccount(OWNER);
  info(`agent wallet:      ${color.bold(AGENT)}`);
  const agentFak = await faucetAccount(AGENT);
  const owner = account(OWNER);
  const agentFull = new Account(AGENT, provider(), agentFak.toString());

  step("2. Registry, guardian and demo counterparties (sub-accounts of the owner)");
  const registry = await subAccount(owner, "registry", 3);
  info(`registry:  ${registry.id}`);
  const guardian = await subAccount(owner, "guardian", 0.2);
  info(`guardian:  ${guardian.id}`);
  const people = {};
  for (const [name, role] of [
    ["cloudhost", "contact: the company's hosting provider"],
    ["dataapi", "new legitimate supplier"],
    ["cl0udhost", "lookalike of cloudhost (address poisoning)"],
    ["drainer", "known scam account, blocked in the registry"],
    ["support-refunds", "fresh scam-bait account, unknown to the registry"],
  ]) {
    const a = await subAccount(owner, name, 0.05);
    people[name] = a.id;
    info(`${name.padEnd(16)} ${a.id}  ${color.gray(role)}`);
  }

  step("3. Deploy the registry and seed one confirmed scam account");
  const regAcct = account(registry.id);
  link("deploy", txUrl(await deployAndInit(regAcct, REGISTRY_WASM, { owner: OWNER, curators: [] })));
  const blocked = await call(owner, registry.id, "block", {
    account_id: people.drainer,
    reason: "demo seed: stands in for a drainer from a public incident report",
  });
  info(`registry.block(${people.drainer})`);
  link("tx", blocked.url);

  step("4. Deploy the Halter wallet on the agent account");
  const policy = {
    limits: {
      near: { max_per_tx: yocto(1), daily_limit: yocto(2), approval_above: yocto(0.5) },
    },
    receiver_mode: "Any",
    approve_new_receivers: true,
    max_actions_per_hour: 20,
    registry_id: registry.id,
    block_flagged: false,
    call_rules: [],
    pending_ttl_ms: String(24 * 3600 * 1000),
  };
  link(
    "deploy",
    txUrl(
      await deployAndInit(agentFull, WALLET_WASM, {
        owner_id: OWNER,
        policy,
        registry_id: registry.id,
        guardians: [guardian.id],
        contacts: [people.cloudhost],
      }),
    ),
  );
  info("policy: 1 NEAR per tx, 2 NEAR per rolling 24h, owner approval above 0.5 NEAR and for first-time receivers");

  step("5. Owner gives the agent a scoped function-call key");
  const agentKey = newKeyPair();
  saveKey(AGENT, agentKey, "agent");
  const added = await call(owner, AGENT, "add_agent_key", {
    public_key: agentKey.getPublicKey().toString(),
    label: "invoice-bot",
    allowance: yocto(1),
  });
  info(`key ${agentKey.getPublicKey().toString().slice(0, 20)}… may call only execute/propose/report_scam on ${AGENT}, gas budget 1 NEAR`);
  link("tx", added.url);

  step("6. Lock the agent account: delete its full-access key");
  const del = await agentFull.deleteKey(agentFak.getPublicKey().toString());
  link("tx", del?.transaction?.hash && txUrl(del.transaction.hash));
  await sleep(1500);
  const keys = await accessKeys(AGENT);
  for (const k of keys) {
    const perm = k.access_key.permission;
    const desc =
      perm === "FullAccess"
        ? color.red("FULL ACCESS")
        : `function-call -> ${perm.FunctionCall.receiver_id} [${perm.FunctionCall.method_names.join(", ")}]`;
    info(`remaining key ${k.public_key.slice(0, 20)}…  ${desc}`);
  }
  if (keys.some((k) => k.access_key.permission === "FullAccess")) throw new Error("Agent account still has a full-access key");
  fs.rmSync(path.join(ROOT, ".halter", "keys", `${AGENT}.full-access.json`), { force: true });

  saveDeployment({
    network: NETWORK.networkId,
    createdAt: new Date().toISOString(),
    owner: OWNER,
    wallet: AGENT,
    registry: registry.id,
    guardian: guardian.id,
    people,
  });
  step("Done");
  info(`deployment saved to ${DEPLOYMENT_FILE}`);
  info(`next: ${color.bold("npm run demo")}  then  ${color.bold("npm run attacks")}`);
}

main().catch((e) => {
  console.error(color.red("setup failed:"), e?.message || e);
  process.exit(1);
});
