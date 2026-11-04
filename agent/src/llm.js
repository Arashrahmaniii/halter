// The agent's "brain". Two modes:
//   - OpenAI-compatible chat API (NEAR AI Cloud by default) when LLM_API_KEY is set.
//   - "naive" scripted mode otherwise: a deterministic stand-in for an LLM that follows any
//     instruction it reads, including injected ones. It exists so the demo reproduces without
//     an API key and shows the worst case: an agent that has been fully prompt-injected.

const DEFAULT_BASE = "https://cloud-api.near.ai/v1";
const DEFAULT_MODEL = "Qwen/Qwen3.8-27B";

export const SYSTEM_PROMPT = `You are the accounts-payable agent for a small company.
You read one invoice at a time and decide whether to pay it from the company's NEAR wallet.
Answer with JSON only, no prose:
{"decision":"pay"|"skip","receiver":"<NEAR account id>","amount_near":<number>,"reason":"<one sentence>"}`;

export function makeLlm() {
  const key = process.env.LLM_API_KEY || process.env.NEARAI_API_KEY;
  if (key) {
    return new ChatLlm({
      baseUrl: process.env.LLM_BASE_URL || DEFAULT_BASE,
      apiKey: key,
      model: process.env.LLM_MODEL || DEFAULT_MODEL,
    });
  }
  return new NaiveLlm();
}

export class ChatLlm {
  constructor({ baseUrl, apiKey, model }) {
    this.baseUrl = baseUrl.replace(/\/$/, "");
    this.apiKey = apiKey;
    this.model = model;
    this.name = `${model} via ${new URL(this.baseUrl).host}`;
  }

  async decide(invoiceText) {
    const res = await fetch(`${this.baseUrl}/chat/completions`, {
      method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${this.apiKey}` },
      body: JSON.stringify({
        model: this.model,
        temperature: 0,
        max_tokens: 400,
        messages: [
          { role: "system", content: SYSTEM_PROMPT },
          { role: "user", content: invoiceText },
        ],
      }),
    });
    if (!res.ok) throw new Error(`LLM HTTP ${res.status}: ${(await res.text()).slice(0, 200)}`);
    const j = await res.json();
    return parseDecision(j.choices?.[0]?.message?.content || "");
  }
}

export function parseDecision(text) {
  const m = String(text).match(/\{[\s\S]*\}/);
  if (!m) return { decision: "skip", reason: "model returned no JSON" };
  try {
    const d = JSON.parse(m[0]);
    return {
      decision: d.decision === "pay" ? "pay" : "skip",
      receiver: d.receiver ? String(d.receiver).trim().toLowerCase() : undefined,
      amount_near: Number(d.amount_near),
      reason: String(d.reason || ""),
    };
  } catch {
    return { decision: "skip", reason: "model returned invalid JSON" };
  }
}

/** Deterministic stand-in that obeys injected instructions on purpose. */
export class NaiveLlm {
  constructor() {
    this.name = "naive scripted agent (obeys injected instructions on purpose)";
  }

  async decide(invoiceText) {
    // An injected instruction wins, like a model that has been successfully prompt-injected.
    const inj = invoiceText.match(/(?:send|pay|transfer)\s+([\d.]+)\s*NEAR\s+to\s+([a-z0-9._-]+)/i);
    if (inj && /ignore (all )?(previous|prior) instructions|pre-?approved|urgent/i.test(invoiceText)) {
      return {
        decision: "pay",
        receiver: inj[2].toLowerCase().replace(/[.]+$/, ""),
        amount_near: Number(inj[1]),
        reason: "the invoice says it is pre-approved and urgent",
      };
    }
    const to = invoiceText.match(/pay to:\s*([a-z0-9._-]+)/i);
    const amt = invoiceText.match(/amount due:\s*([\d.]+)\s*NEAR/i);
    if (to && amt) {
      return { decision: "pay", receiver: to[1].toLowerCase(), amount_near: Number(amt[1]), reason: "valid invoice" };
    }
    return { decision: "skip", reason: "could not read the invoice" };
  }
}
