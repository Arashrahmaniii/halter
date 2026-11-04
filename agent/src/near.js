// Thin helpers over near-api-js v7: network config, accounts, keys, tx results.
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import {
  Account,
  FailoverRpcProvider,
  JsonRpcProvider,
  KeyPair,
  getTransactionLastResult,
  teraToGas,
  nearToYocto,
  yoctoToNear,
} from "near-api-js";

export { teraToGas, nearToYocto, yoctoToNear, KeyPair };

const here = path.dirname(fileURLToPath(import.meta.url));
export const ROOT = path.resolve(here, "..");
export const STATE_DIR = path.resolve(process.env.HALTER_STATE_DIR || path.join(ROOT, ".halter"));
export const KEYS_DIR = path.join(STATE_DIR, "keys");
export const DEPLOYMENT_FILE = path.join(STATE_DIR, "deployment.json");

export const NETWORK = {
  networkId: "testnet",
  rpcUrls: (process.env.NEAR_RPC_URLS || "https://test.rpc.fastnear.com,https://near-testnet.drpc.org,https://rpc.testnet.near.org")
    .split(",")
    .map((s) => s.trim())
    .filter(Boolean),
  faucetUrl: "https://helper.nearprotocol.com/account",
  nearblocksApi: "https://api-testnet.nearblocks.io/v1",
  explorer: "https://testnet.nearblocks.io",
};

let _provider;
export function provider() {
  if (!_provider) {
    const list = NETWORK.rpcUrls.map((url) => new JsonRpcProvider({ url }));
    _provider = list.length > 1 ? new FailoverRpcProvider(list) : list[0];
  }
  return _provider;
}

export const txUrl = (hash) => `${NETWORK.explorer}/txns/${hash}`;
export const accountUrl = (id) => `${NETWORK.explorer}/address/${id}`;

// ------------------------------------------------------------------ keys

export function newKeyPair() {
  return KeyPair.fromRandom("ed25519");
}

export function saveKey(accountId, keyPair, label = "full-access") {
  fs.mkdirSync(KEYS_DIR, { recursive: true });
  const file = path.join(KEYS_DIR, `${accountId}.${label}.json`);
  fs.writeFileSync(
    file,
    JSON.stringify(
      { account_id: accountId, label, public_key: keyPair.getPublicKey().toString(), private_key: keyPair.toString() },
      null,
      2,
    ),
    { mode: 0o600 },
  );
  return file;
}

export function loadKey(accountId, label = "full-access") {
  const file = path.join(KEYS_DIR, `${accountId}.${label}.json`);
  if (!fs.existsSync(file)) throw new Error(`No ${label} key for ${accountId} in ${KEYS_DIR}. Run: npm run setup`);
  return JSON.parse(fs.readFileSync(file, "utf8"));
}

export function account(accountId, label = "full-access", { singleRpc = false } = {}) {
  const k = loadKey(accountId, label);
  // A single provider surfaces the exact rejection reason; the failover provider retries
  // invalid transactions on every endpoint and reports only "retries exceeded".
  const p = singleRpc ? new JsonRpcProvider({ url: NETWORK.rpcUrls[0] }) : provider();
  return new Account(accountId, p, k.private_key);
}

export function readonlyAccount(accountId) {
  return new Account(accountId, provider());
}

// ------------------------------------------------------------------ deployment state

export function loadDeployment() {
  if (!fs.existsSync(DEPLOYMENT_FILE)) {
    throw new Error(`No deployment found at ${DEPLOYMENT_FILE}. Run: npm run setup`);
  }
  return JSON.parse(fs.readFileSync(DEPLOYMENT_FILE, "utf8"));
}

export function saveDeployment(d) {
  fs.mkdirSync(STATE_DIR, { recursive: true });
  fs.writeFileSync(DEPLOYMENT_FILE, JSON.stringify(d, null, 2));
}

// ------------------------------------------------------------------ RPC helpers

export async function view(contractId, method, args = {}) {
  return provider().callFunction({ contractId, method, args });
}

export async function viewAccount(accountId) {
  try {
    return await provider().viewAccount({ accountId });
  } catch (e) {
    if (e?.constructor?.name === "AccountDoesNotExistError" || /does not exist|UNKNOWN_ACCOUNT/i.test(String(e?.message || e))) return null;
    throw e;
  }
}

export async function accessKeys(accountId) {
  const r = await provider().viewAccessKeyList({ accountId });
  return r.keys || [];
}

/** Sign and send a function call; return { hash, value, logs, outcome }. Throws on tx failure. */
export async function call(acct, contractId, methodName, args = {}, { gas = teraToGas("100"), deposit = 0n } = {}) {
  const outcome = await acct.callFunctionRaw({ contractId, methodName, args, gas, deposit });
  return summarize(outcome);
}

export function summarize(outcome) {
  const hash = outcome?.transaction?.hash || outcome?.transaction_outcome?.id;
  let value;
  try {
    value = getTransactionLastResult(outcome);
  } catch {
    value = undefined;
  }
  const logs = [];
  for (const r of [outcome?.transaction_outcome, ...(outcome?.receipts_outcome || [])]) {
    for (const l of r?.outcome?.logs || []) logs.push(l);
  }
  const failure = findFailure(outcome);
  return { hash, url: hash ? txUrl(hash) : undefined, value, logs, failure, outcome };
}

function findFailure(outcome) {
  const s = outcome?.status;
  if (s && typeof s === "object" && s.Failure) return s.Failure;
  for (const r of outcome?.receipts_outcome || []) {
    const st = r?.outcome?.status;
    if (st && typeof st === "object" && st.Failure) return st.Failure;
  }
  return undefined;
}

/** Halter decision events from a tx's logs. */
export function halterEvents(logs) {
  return logs
    .filter((l) => l.startsWith("EVENT_JSON:"))
    .map((l) => {
      try {
        return JSON.parse(l.slice("EVENT_JSON:".length));
      } catch {
        return null;
      }
    })
    .filter(Boolean);
}

export function near(n) {
  return BigInt(nearToYocto(String(n)));
}

export function fmtNear(yocto) {
  const s = yoctoToNear(BigInt(yocto ?? 0).toString());
  const n = Number(s);
  return `${Number.isFinite(n) ? +n.toFixed(4) : s} NEAR`;
}

export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** Short, human-readable message for an RPC / execution error. */
export function errorText(e) {
  const raw = e?.message || String(e);
  const m = raw.match(/"(MethodNameMismatch|ReceiverMismatch|DepositWithFunctionCall|RequiresFullAccess|NotEnoughAllowance|InvalidAccessKeyError)"/);
  if (m) return m[1];
  const typed = e?.type || e?.cause?.name;
  return (typed ? `${typed}: ` : "") + raw.split("\n")[0].slice(0, 300);
}
