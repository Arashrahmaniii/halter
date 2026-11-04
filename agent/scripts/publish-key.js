// Publish the agent's function-call key for the red-team page (web/redteam.json).
//
// This is deliberate: Halter's claim is that the agent key cannot move money outside the owner's
// policy, so the key is safe to show. Testnet only. The owner key is never published.
//
// Usage: npm run publish-key
import fs from "node:fs";
import path from "node:path";
import { ROOT, loadDeployment, loadKey } from "../src/near.js";

const d = loadDeployment();
if (d.network !== "testnet") throw new Error("Refusing to publish a key outside testnet");
const k = loadKey(d.wallet, "agent");
const known = JSON.parse(fs.readFileSync(path.join(ROOT, "data", "known-names.json"), "utf8"));
const out = {
  _note: "Testnet agent key, published on purpose: it is a function-call key that can only call execute/propose/report_scam on the wallet. Try to steal the funds with it.",
  network: d.network,
  wallet: d.wallet,
  registry: d.registry,
  owner: d.owner,
  drainer: d.people.drainer,
  contacts: [d.people.cloudhost],
  default_target: d.people.drainer,
  people: d.people,
  known_names: known.testnet,
  agent_key: { public_key: k.public_key, private_key: k.private_key },
};
const file = path.resolve(ROOT, "..", "web", "redteam.json");
fs.writeFileSync(file, JSON.stringify(out, null, 2) + "\n");
console.log(`wrote ${file} (agent key ${k.public_key})`);
