// Halter Scout for Node: wires the pure scoring core to NEAR RPC and the NearBlocks API.
// The scoring rules live in scout-core.js (shared with the browser red-team page).
import { NETWORK, view, viewAccount } from "./near.js";
import { explain, gatherFacts, levenshtein, lookalike, makeNearblocks, score, skeleton, SCAM_WORDS } from "./scout-core.js";

export { explain, gatherFacts, levenshtein, lookalike, score, skeleton, SCAM_WORDS };

const nearblocks = makeNearblocks({ apiBase: NETWORK.nearblocksApi, apiKey: process.env.NEARBLOCKS_API_KEY });

export class Scout {
  constructor({ walletId, registryId, knownNames = [] } = {}) {
    this.walletId = walletId;
    this.registryId = registryId;
    this.knownNames = knownNames;
  }

  async check(receiver, extra = {}) {
    const facts = await gatherFacts(receiver, {
      walletId: this.walletId,
      registryId: this.registryId,
      sources: { view, viewAccount, nearblocks },
    });
    const result = score(facts, { knownNames: this.knownNames, ...extra });
    return { receiver, ...result, facts, checkedAt: new Date().toISOString() };
  }
}
