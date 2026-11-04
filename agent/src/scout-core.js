// Halter Scout: counterparty check for agents that pay.
//
// Before an agent sends funds or calls a contract it asks Scout about the receiver. Scout
// gathers facts from public sources, scores them, and answers allow / warn / block with reasons.
//
// Signals and where they come from:
//   registry      Halter Registry contract (shared, evidence-backed blocklist)   -> RPC view `check`
//   existence     NEAR RPC `view_account`                                          -> RPC
//   lookalike     receiver vs the wallet's contacts and a list of known names     -> local
//   age           account creation time                                           -> NearBlocks API
//   activity      transaction count                                               -> NearBlocks API
//   code change   contract deployments (is it a contract, did its code change?)   -> RPC + NearBlocks
//   history       has this wallet paid the receiver before?                       -> RPC view `has_paid`
//   contact       is the receiver in the wallet's on-chain contact list?           -> RPC view `get_contacts`
//   name          scam-bait words in the account name                              -> local
// Pure core: no Node or near-api-js imports, so the same file runs in the browser (web/redteam.html).
// Data access is passed in as `sources`: { viewAccount(id), view(contract, method, args), nearblocks(path) }.

const HOUR = 3_600_000;
const EMPTY_CODE = "11111111111111111111111111111111";

export const SCAM_WORDS = [
  "refund", "support", "airdrop", "claim", "giveaway", "bonus", "reward", "verify", "validate",
  "recover", "unlock", "official", "helpdesk", "walletconnect", "rectify", "migrate", "migration",
  "security-update", "kyc", "urgent",
];

// Characters and pairs commonly swapped to imitate a trusted name.
const CONFUSABLE_PAIRS = [["rn", "m"], ["vv", "w"], ["cl", "d"]];
const CONFUSABLE_CHARS = { 0: "o", 1: "l", 3: "e", 4: "a", 5: "s", 7: "t", 8: "b", 9: "g", i: "l", "|": "l", $: "s" };

/** Collapse an account id to a "skeleton" so homoglyph variants compare equal. */
export function skeleton(id) {
  let s = String(id).toLowerCase();
  for (const [a, b] of CONFUSABLE_PAIRS) s = s.split(a).join(b);
  s = [...s].map((c) => CONFUSABLE_CHARS[c] ?? c).join("");
  s = s.replace(/[-_]/g, "");
  s = s.replace(/(.)\1+/g, "$1"); // "goooogle" -> "gogle"
  return s;
}

export function levenshtein(a, b) {
  const m = a.length;
  const n = b.length;
  if (!m) return n;
  if (!n) return m;
  let prev = Array.from({ length: n + 1 }, (_, j) => j);
  for (let i = 1; i <= m; i++) {
    const cur = [i];
    for (let j = 1; j <= n; j++) {
      cur[j] = Math.min(prev[j] + 1, cur[j - 1] + 1, prev[j - 1] + (a[i - 1] === b[j - 1] ? 0 : 1));
    }
    prev = cur;
  }
  return prev[n];
}

function splitId(id) {
  const parts = String(id).split(".");
  return { first: parts[0], parent: parts.slice(1).join(".") };
}

/**
 * Does `receiver` imitate one of the `trusted` names?
 * Returns { target, kind } or null. kind: "homoglyph" | "typo" | "impersonation".
 */
export function lookalike(receiver, trusted) {
  const r = String(receiver).toLowerCase();
  if (trusted.some((t) => String(t).toLowerCase() === r)) return null; // it IS a trusted name
  const rs = splitId(r);
  for (const t0 of trusted) {
    const t = String(t0).toLowerCase();
    if (!t || t === r) continue;
    const ts = splitId(t);
    if (skeleton(r) === skeleton(t)) return { target: t0, kind: "homoglyph" };
    // Same parent namespace (or both top-level), first label one or two edits away.
    if (rs.parent === ts.parent && ts.first.length >= 5) {
      const d = levenshtein(rs.first, ts.first);
      if (d > 0 && d <= (ts.first.length >= 8 ? 2 : 1)) return { target: t0, kind: "typo" };
    }
    // Trusted brand glued to another word: "cloudhost-billing", "support_usdc".
    if (ts.first.length >= 4 && rs.first !== ts.first) {
      const esc = ts.first.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
      if (new RegExp(`(^${esc}[-_])|([-_]${esc}$)`).test(rs.first)) return { target: t0, kind: "impersonation" };
    }
    // Same name under a different parent: "usdc.scam.testnet" vs trusted "usdc.fakes.testnet",
    // or a trusted full id stuffed into another namespace: "wrap.testnet.evil.testnet".
    if (ts.parent && rs.parent !== ts.parent && (rs.first === ts.first || r.startsWith(t + "."))) {
      return { target: t0, kind: "impersonation" };
    }
  }
  return null;
}

/** NearBlocks client with a short cache and one retry on the free tier's rate limit. */
export function makeNearblocks({ apiBase, apiKey } = {}) {
  const cache = new Map();
  return async function nearblocks(pathAndQuery) {
    const url = `${apiBase}${pathAndQuery}`;
    const hit = cache.get(url);
    if (hit && Date.now() - hit.at < 60_000) return hit.value;
    const headers = { accept: "application/json" };
    if (apiKey) headers.authorization = `Bearer ${apiKey}`;
    for (let attempt = 0; attempt < 2; attempt++) {
      const ctrl = new AbortController();
      const timer = setTimeout(() => ctrl.abort(), 6000);
      try {
        const res = await fetch(url, { signal: ctrl.signal, headers });
        const body = await res.json().catch(() => ({}));
        const limited = res.status === 429 || /exceeded your API request limit/i.test(body?.message || "");
        if (limited) {
          if (attempt === 0) {
            await new Promise((r) => setTimeout(r, 2500));
            continue;
          }
          throw new Error("NearBlocks rate limit");
        }
        if (!res.ok) throw new Error(`NearBlocks HTTP ${res.status}`);
        cache.set(url, { at: Date.now(), value: body });
        return body;
      } finally {
        clearTimeout(timer);
      }
    }
    throw new Error("NearBlocks unavailable");
  };
}

/** Gather raw facts about a receiver. Every source is optional; failures are recorded. */
export async function gatherFacts(receiver, { walletId, registryId, sources }) {
  const { view, viewAccount, nearblocks } = sources;
  const facts = { receiver, errors: {} };
  const tasks = [];

  tasks.push(
    viewAccount(receiver)
      .then((a) => {
        facts.exists = !!a;
        if (a) {
          facts.balance = a.amount;
          facts.isContract = a.code_hash && a.code_hash !== EMPTY_CODE;
          facts.codeHash = a.code_hash;
        }
      })
      .catch((e) => (facts.errors.rpc = String(e.message || e))),
  );

  if (registryId) {
    tasks.push(
      view(registryId, "check", { account_id: receiver })
        .then((v) => (facts.registry = v))
        .catch((e) => (facts.errors.registry = String(e.message || e))),
    );
  }

  if (walletId) {
    tasks.push(
      view(walletId, "has_paid", { account_id: receiver })
        .then((v) => (facts.paidBefore = !!v))
        .catch((e) => (facts.errors.history = String(e.message || e))),
    );
    tasks.push(
      view(walletId, "get_contacts", {})
        .then((v) => (facts.contacts = v || []))
        .catch((e) => (facts.errors.contacts = String(e.message || e))),
    );
  }

  const enc = encodeURIComponent(receiver);
  tasks.push(
    nearblocks(`/account/${enc}`)
      .then((j) => {
        const a = j?.account?.[0];
        facts.indexed = !!a;
        const ts = a?.created?.block_timestamp;
        if (ts) facts.createdAtMs = Math.floor(Number(ts) / 1e6);
      })
      .catch((e) => (facts.errors.nearblocks = String(e.message || e))),
  );
  tasks.push(
    nearblocks(`/account/${enc}/txns/count`)
      .then((j) => (facts.txCount = Number(j?.txns?.[0]?.count ?? NaN)))
      .catch((e) => (facts.errors.nearblocksTxns = String(e.message || e))),
  );

  await Promise.all(tasks);

  // Code history only matters for contracts; skip the extra indexer call otherwise.
  if (facts.isContract) {
    await nearblocks(`/account/${enc}/contract/deployments`)
      .then((j) => {
        const ds = j?.deployments || [];
        facts.deployments = ds.length;
        const last = ds.map((d) => Number(d.block_timestamp)).filter(Number.isFinite).sort((a, b) => b - a)[0];
        if (last) facts.lastDeployMs = Math.floor(last / 1e6);
      })
      .catch((e) => (facts.errors.nearblocksDeploys = String(e.message || e)));
  }
  return facts;
}

const ago = (ms) => (ms < HOUR ? `${Math.max(1, Math.round(ms / 60000))} min` : ms < 48 * HOUR ? `${Math.round(ms / HOUR)} h` : `${Math.round(ms / (24 * HOUR))} days`);

/**
 * Score facts into a verdict. Pure function: same facts in, same answer out.
 * @returns {{verdict: "allow"|"warn"|"block", score: number, signals: Array}}
 */
export function score(facts, { knownNames = [], now = Date.now(), warnAt = 50 } = {}) {
  const signals = [];
  const add = (id, level, points, detail, source) => signals.push({ id, level, points, detail, source });
  const r = facts.receiver;
  const isImplicit = /^[0-9a-f]{64}$/.test(r) || /^0x[0-9a-f]{40}$/.test(r);
  const contacts = facts.contacts || [];
  const trustedByUser = contacts.includes(r) || facts.paidBefore;

  // Shared registry.
  const reg = facts.registry;
  if (reg?.status === "Blocked") add("registry", "block", 100, reg.total_reports ? `confirmed scam account in the shared registry (${reg.total_reports} report(s))` : "confirmed scam account in the shared registry (blocked by a curator)", "Halter Registry");
  else if (reg?.status === "Flagged") add("registry", "warn", 60, `${reg.open_reports} open scam report(s) in the shared registry`, "Halter Registry");
  else if (facts.errors?.registry) add("registry", "warn", 30, "registry unreachable", "Halter Registry");

  // Existence.
  if (facts.exists === false) {
    if (isImplicit) add("existence", "warn", 50, "implicit account has never been used (a typo here loses funds)", "NEAR RPC");
    else add("existence", "block", 100, "account does not exist: the payment would fail or be lost", "NEAR RPC");
  }

  // Lookalikes of names the user trusts.
  const la = lookalike(r, [...contacts, ...knownNames]);
  if (la) {
    if (la.kind === "impersonation") add("lookalike", "warn", 50, `name imitates trusted ${la.target}`, "contacts + known names");
    else add("lookalike", "block", 100, `${la.kind} of trusted ${la.target} (address poisoning pattern)`, "contacts + known names");
  }

  // Trust from the user's own history.
  if (contacts.includes(r)) add("contact", "trust", -40, "receiver is in the wallet's contact list", "Halter Wallet");
  if (facts.paidBefore) add("history", "trust", -30, "this wallet has paid the receiver before", "Halter Wallet");

  // Age and activity (soft signals; ignored for receivers the user already trusts).
  if (facts.exists && !trustedByUser) {
    if (facts.createdAtMs) {
      const age = now - facts.createdAtMs;
      if (age < HOUR) add("age", "warn", 50, `account created ${ago(age)} ago`, "NearBlocks");
      else if (age < 24 * HOUR) add("age", "warn", 35, `account created ${ago(age)} ago`, "NearBlocks");
      else if (age < 7 * 24 * HOUR) add("age", "info", 15, `account created ${ago(age)} ago`, "NearBlocks");
      else add("age", "info", 0, `account is ${ago(age)} old`, "NearBlocks");
    } else if (facts.indexed === false || (facts.indexed && !facts.createdAtMs)) {
      add("age", "warn", 35, "creation time not indexed yet (very new account)", "NearBlocks");
    } else if (facts.errors?.nearblocks) {
      add("age", "warn", 25, "cannot verify account age (indexer unreachable or rate-limited)", "NearBlocks");
    }
    if (Number.isFinite(facts.txCount)) {
      if (facts.txCount <= 1) add("activity", "warn", 20, `${facts.txCount} transaction(s) on record`, "NearBlocks");
      else if (facts.txCount <= 5) add("activity", "info", 10, `${facts.txCount} transactions on record`, "NearBlocks");
    }
  }

  // Contracts and code changes (never ignored: a trusted contract can be upgraded into a drainer).
  if (facts.isContract) {
    if (facts.lastDeployMs) {
      const age = now - facts.lastDeployMs;
      if (age < 24 * HOUR) add("code", "warn", 50, `contract code changed ${ago(age)} ago`, "NearBlocks");
      else if (age < 7 * 24 * HOUR) add("code", "warn", 25, `contract code changed ${ago(age)} ago`, "NearBlocks");
      else add("code", "info", 0, `contract code unchanged for ${ago(age)}`, "NearBlocks");
    } else {
      add("code", "info", 5, "receiver is a contract", "NEAR RPC");
    }
  }

  // Bait words.
  const words = SCAM_WORDS.filter((w) => r.split(".")[0].includes(w));
  if (words.length) add("name", "warn", Math.min(50, 25 * words.length), `name uses scam-bait words: ${words.join(", ")}`, "local");

  const hard = signals.some((s) => s.level === "block");
  const total = signals.reduce((a, s) => a + s.points, 0);
  const verdict = hard ? "block" : total >= warnAt ? "warn" : "allow";
  return { verdict, score: Math.max(0, total), signals };
}

/** One line per signal, for logs. */
export function explain(report) {
  return report.signals
    .filter((s) => s.level !== "info" || s.points > 0)
    .map((s) => `${s.level.toUpperCase().padEnd(5)} ${s.id}: ${s.detail} [${s.source}]`);
}
