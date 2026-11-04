// Halter Scout as a CLI or a small HTTP service other agents can call.
// Usage:
//   npm run scout -- <account_id>                one-off check (uses the local deployment if any)
//   npm run scout -- --serve [port]              GET /check?account=<id>[&wallet=<id>&registry=<id>]
import fs from "node:fs";
import http from "node:http";
import path from "node:path";
import { DEPLOYMENT_FILE, ROOT } from "../src/near.js";
import { Scout, explain } from "../src/scout.js";
import { color, detail, info, tag } from "../src/log.js";

const known = JSON.parse(fs.readFileSync(path.join(ROOT, "data", "known-names.json"), "utf8"));
const dep = fs.existsSync(DEPLOYMENT_FILE) ? JSON.parse(fs.readFileSync(DEPLOYMENT_FILE, "utf8")) : {};
const knownNames = known[dep.network || "testnet"] || known.testnet;
const args = process.argv.slice(2);

if (args[0] === "--serve") {
  const port = Number(args[1] || 8787);
  http
    .createServer(async (req, res) => {
      const url = new URL(req.url, "http://localhost");
      if (url.pathname !== "/check") {
        res.writeHead(404, { "content-type": "application/json" }).end(JSON.stringify({ error: "GET /check?account=<id>" }));
        return;
      }
      const accountId = url.searchParams.get("account");
      if (!accountId || !/^[a-z0-9._-]{2,64}$/.test(accountId)) {
        res.writeHead(400, { "content-type": "application/json" }).end(JSON.stringify({ error: "bad account id" }));
        return;
      }
      try {
        const scout = new Scout({
          walletId: url.searchParams.get("wallet") || dep.wallet,
          registryId: url.searchParams.get("registry") || dep.registry,
          knownNames,
        });
        const r = await scout.check(accountId);
        res.writeHead(200, { "content-type": "application/json" }).end(JSON.stringify({ account: accountId, verdict: r.verdict, score: r.score, signals: r.signals, checkedAt: r.checkedAt }));
      } catch (e) {
        res.writeHead(502, { "content-type": "application/json" }).end(JSON.stringify({ error: String(e.message || e) }));
      }
    })
    .listen(port, () => console.log(`Halter Scout listening on http://localhost:${port}/check?account=<id>`));
} else if (args[0]) {
  const scout = new Scout({ walletId: dep.wallet, registryId: dep.registry, knownNames });
  const r = await scout.check(args[0]);
  info(`${color.bold(args[0])} ${tag(r.verdict)} score ${r.score}`);
  detail(explain(r));
  if (Object.keys(r.facts.errors).length) detail(Object.entries(r.facts.errors).map(([k, v]) => `source ${k} unavailable: ${v}`));
} else {
  console.log("usage: npm run scout -- <account_id>   |   npm run scout -- --serve [port]");
}
