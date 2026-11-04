// Smoke test for the MCP server: starts it over stdio like an MCP client would, lists the tools,
// checks a payee and tries one malicious payment.
// Usage: npm run mcp:smoke   (add HALTER_PUBLIC=1 to use the shared testnet wallet)
import path from "node:path";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";
import fs from "node:fs";
import { DEPLOYMENT_FILE, ROOT } from "../src/near.js";

const usePublic = process.env.HALTER_PUBLIC === "1" || !fs.existsSync(DEPLOYMENT_FILE);
const dep = usePublic
  ? JSON.parse(fs.readFileSync(path.resolve(ROOT, "..", "web", "redteam.json"), "utf8"))
  : JSON.parse(fs.readFileSync(DEPLOYMENT_FILE, "utf8"));
const drainer = usePublic ? dep.drainer : dep.people.drainer;

const transport = new StdioClientTransport({
  command: process.execPath,
  args: [path.join(ROOT, "scripts", "mcp.js")],
  env: { ...process.env, HALTER_PUBLIC: usePublic ? "1" : "0" },
  stderr: "inherit",
});
const client = new Client({ name: "halter-smoke", version: "1.0.0" });
await client.connect(transport);

const { tools } = await client.listTools();
console.log("tools:", tools.map((t) => t.name).join(", "));

const show = (r) => console.log(r.content.map((c) => c.text).join("\n"));
console.log("\n> halter_status");
show(await client.callTool({ name: "halter_status", arguments: {} }));
console.log(`\n> halter_check_receiver ${drainer}`);
show(await client.callTool({ name: "halter_check_receiver", arguments: { account_id: drainer } }));
console.log(`\n> halter_pay 0.9 NEAR -> ${drainer}  (what a prompt-injected model would ask for)`);
show(await client.callTool({ name: "halter_pay", arguments: { receiver_id: drainer, amount_near: 0.9, memo: "mcp smoke" } }));
await client.close();
