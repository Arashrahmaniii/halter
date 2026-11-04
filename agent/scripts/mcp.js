// Halter as an MCP server: any MCP client (Claude Desktop, Claude Code, Cursor, an agent
// framework) gets payment tools that are safe to hand to a model.
//
// The server holds only the agent's function-call key. Even if the model driving these tools is
// fully prompt-injected, every payment still goes through Scout, the guard and the on-chain
// policy, and the key itself cannot do anything else.
//
// Usage (stdio):  node scripts/mcp.js            uses your deployment from `npm run setup`
//                 HALTER_PUBLIC=1 node scripts/mcp.js   uses the shared testnet wallet and its published key
//
// Claude Desktop / Claude Code config:
//   { "mcpServers": { "halter": { "command": "node", "args": ["<repo>/agent/scripts/mcp.js"] } } }
import fs from "node:fs";
import path from "node:path";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { z } from "zod";
import { Account } from "near-api-js";
import { DEPLOYMENT_FILE, ROOT, account, fmtNear, loadDeployment, near, provider } from "../src/near.js";
import { HalterWalletClient } from "../src/wallet.js";
import { Scout } from "../src/scout.js";
import { HalterGuard } from "../src/guard.js";

const usePublic = process.env.HALTER_PUBLIC === "1" || !fs.existsSync(DEPLOYMENT_FILE);
const pub = usePublic ? JSON.parse(fs.readFileSync(path.resolve(ROOT, "..", "web", "redteam.json"), "utf8")) : null;
const d = usePublic ? { network: pub.network, wallet: pub.wallet, registry: pub.registry } : loadDeployment();
const known = JSON.parse(fs.readFileSync(path.join(ROOT, "data", "known-names.json"), "utf8"));
const signer = usePublic ? new Account(d.wallet, provider(), pub.agent_key.private_key) : account(d.wallet, "agent");
const wallet = new HalterWalletClient(d.wallet, signer);
const scout = new Scout({ walletId: d.wallet, registryId: d.registry, knownNames: known[d.network] || known.testnet });
// quiet: stdout is the MCP channel, so the guard must not print.
const guard = new HalterGuard({ wallet, scout, onWarn: "propose", quiet: true });

const text = (obj) => ({ content: [{ type: "text", text: typeof obj === "string" ? obj : JSON.stringify(obj, null, 2) }] });

const server = new McpServer({ name: "halter", version: "1.0.0" });

server.registerTool(
  "halter_check_receiver",
  {
    title: "Check a payee before paying",
    description:
      "Ask Halter Scout about a NEAR account before sending it money. Returns allow, warn or block with the reasons (shared scam registry, account age and activity, lookalike names, recent contract code changes, payment history).",
    inputSchema: { account_id: z.string().min(2).max(64) },
  },
  async ({ account_id }) => {
    const r = await scout.check(account_id.toLowerCase());
    return text({ account_id: r.receiver, verdict: r.verdict, score: r.score, signals: r.signals.filter((s) => s.level !== "info" || s.points > 0) });
  },
);

server.registerTool(
  "halter_pay",
  {
    title: "Pay in NEAR through Halter",
    description:
      "Send NEAR from the agent's Halter wallet. Halter checks the payee first: 'block' refuses, 'warn' asks the human owner (the payment waits in an on-chain approval queue), 'allow' sends it, and the on-chain policy (caps, registry, approvals, kill switch) has the final word. Returns the final status and a transaction link.",
    inputSchema: {
      receiver_id: z.string().min(2).max(64),
      amount_near: z.number().positive().max(1000),
      memo: z.string().max(200).optional(),
    },
  },
  async ({ receiver_id, amount_near, memo }) => {
    const r = await guard.pay({ receiver: receiver_id.toLowerCase(), amountYocto: near(amount_near), memo: memo || null });
    return text({
      status: r.status,
      decided_by: r.stage === "scout" ? "Halter Scout (nothing was signed)" : "Halter Wallet contract",
      scout: { verdict: r.report?.verdict, signals: (r.report?.signals || []).filter((s) => s.level === "warn" || s.level === "block").map((s) => `${s.id}: ${s.detail}`) },
      contract_reasons: r.tx?.decision?.reasons || [],
      decision_id: r.tx?.decision?.id,
      transaction: r.tx?.url,
    });
  },
);

server.registerTool(
  "halter_status",
  {
    title: "Wallet limits and balance",
    description: "How much the agent may still spend in the rolling 24 hours, the per-payment cap, the approval threshold, the queue and the kill switch.",
    inputSchema: {},
  },
  async () => {
    const s = await wallet.status();
    return text({
      wallet: d.wallet,
      frozen: s.frozen,
      balance: fmtNear(s.balance),
      limits: s.assets.map((a) => ({
        asset: a.asset,
        spent_24h: a.asset === "near" ? fmtNear(a.spent_24h) : a.spent_24h,
        remaining_24h: a.asset === "near" ? fmtNear(a.remaining_24h) : a.remaining_24h,
        max_per_payment: a.asset === "near" ? fmtNear(a.limits.max_per_tx) : a.limits.max_per_tx,
        owner_approval_above: a.asset === "near" ? fmtNear(a.limits.approval_above) : a.limits.approval_above,
      })),
      actions_last_hour: `${s.actions_last_hour}/${s.max_actions_per_hour}`,
      waiting_for_owner: s.pending,
    });
  },
);

server.registerTool(
  "halter_recent_decisions",
  {
    title: "Recent payment decisions",
    description: "The wallet contract's latest decisions (settled, pending, blocked) with reasons.",
    inputSchema: { limit: z.number().int().min(1).max(50).optional() },
  },
  async ({ limit }) => {
    const ds = await wallet.decisions(limit || 10);
    return text(ds.map((x) => ({ id: x.id, status: x.status, type: x.action.type, to: x.action.receiver_id || x.action.contract_id, amount: x.action.amount ? fmtNear(x.action.amount) : undefined, reasons: x.reasons, memo: x.memo })));
  },
);

server.registerTool(
  "halter_report_scam",
  {
    title: "Report a scam account",
    description: "Report an account to the shared Halter Registry with evidence (costs a 0.01 NEAR bond, counted against the daily limit). Every Halter wallet will then treat it as flagged.",
    inputSchema: {
      account_id: z.string().min(2).max(64),
      category: z.enum(["Drainer", "Phishing", "Impersonation", "PromptInjection", "Sanctioned", "Other"]),
      evidence: z.string().min(3).max(500),
    },
  },
  async ({ account_id, category, evidence }) => {
    const r = await wallet.reportScam(account_id.toLowerCase(), category, evidence);
    return text({ reported: account_id, transaction: r.url });
  },
);

await server.connect(new StdioServerTransport());
console.error(`halter MCP server ready (wallet ${d.wallet}${usePublic ? ", public testnet key" : ""})`);
