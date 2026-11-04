import { test } from "node:test";
import assert from "node:assert/strict";
import { lookalike, score, skeleton, levenshtein } from "../src/scout.js";

const NOW = Date.parse("2026-11-04T12:00:00Z");
const H = 3_600_000;

const base = (over = {}) => ({
  receiver: "shop.testnet",
  exists: true,
  indexed: true,
  createdAtMs: NOW - 400 * 24 * H,
  txCount: 300,
  isContract: false,
  contacts: [],
  paidBefore: false,
  registry: { status: "Unknown", open_reports: 0, total_reports: 0 },
  errors: {},
  ...over,
});

test("skeleton folds homoglyphs", () => {
  assert.equal(skeleton("cl0udhost.testnet"), skeleton("cloudhost.testnet"));
  assert.equal(skeleton("paypa1.near"), skeleton("paypal.near"));
  assert.equal(skeleton("rnicrosoft.near"), skeleton("microsoft.near"));
  assert.notEqual(skeleton("alice.near"), skeleton("bob.near"));
});

test("levenshtein", () => {
  assert.equal(levenshtein("kitten", "sitting"), 3);
  assert.equal(levenshtein("abc", "abc"), 0);
});

test("lookalike kinds", () => {
  const trusted = ["cloudhost.owner.testnet", "usdc.fakes.testnet"];
  assert.equal(lookalike("cl0udhost.owner.testnet", trusted)?.kind, "homoglyph");
  assert.equal(lookalike("cloudhots.owner.testnet", trusted)?.kind, "typo");
  assert.equal(lookalike("cloudhost-billing.owner.testnet", trusted)?.kind, "impersonation");
  assert.equal(lookalike("usdc.scam.testnet", trusted)?.kind, "impersonation");
  assert.equal(lookalike("cloudhost.owner.testnet", trusted), null, "exact match is not a lookalike");
  assert.equal(lookalike("weather.owner.testnet", trusted), null);
});

test("old, active, unflagged account is allowed", () => {
  assert.equal(score(base(), { now: NOW }).verdict, "allow");
});

test("registry Blocked always blocks, even for contacts", () => {
  const r = score(base({ contacts: ["shop.testnet"], registry: { status: "Blocked", open_reports: 0, total_reports: 3 } }), { now: NOW });
  assert.equal(r.verdict, "block");
  assert.ok(r.signals.some((s) => s.id === "registry" && s.level === "block"));
});

test("registry Flagged warns", () => {
  const r = score(base({ registry: { status: "Flagged", open_reports: 2, total_reports: 2 } }), { now: NOW });
  assert.equal(r.verdict, "warn");
});

test("brand-new account with no history warns", () => {
  const r = score(base({ createdAtMs: NOW - 10 * 60_000, txCount: 1 }), { now: NOW });
  assert.equal(r.verdict, "warn");
  assert.ok(r.signals.some((s) => s.id === "age"));
});

test("new account that is a contact is allowed", () => {
  const r = score(base({ createdAtMs: NOW - 10 * 60_000, txCount: 1, contacts: ["shop.testnet"] }), { now: NOW });
  assert.equal(r.verdict, "allow");
});

test("nonexistent named account blocks, unused implicit account warns", () => {
  assert.equal(score(base({ exists: false }), { now: NOW }).verdict, "block");
  const implicit = "a".repeat(64);
  assert.equal(score(base({ receiver: implicit, exists: false }), { now: NOW }).verdict, "warn");
});

test("lookalike of a contact blocks", () => {
  const r = score(base({ receiver: "cl0udhost.owner.testnet", contacts: ["cloudhost.owner.testnet"] }), { now: NOW });
  assert.equal(r.verdict, "block");
});

test("recent code change on a contract warns even if paid before", () => {
  const r = score(base({ isContract: true, lastDeployMs: NOW - 2 * H, paidBefore: true }), { now: NOW });
  assert.equal(r.verdict, "allow", "trust offsets one soft code warning");
  const r2 = score(base({ isContract: true, lastDeployMs: NOW - 2 * H }), { now: NOW });
  assert.equal(r2.verdict, "warn");
});

test("scam-bait name plus young account warns", () => {
  const r = score(base({ receiver: "support-refunds.testnet", createdAtMs: NOW - 30 * H }), { now: NOW });
  assert.equal(r.verdict, "warn");
  assert.ok(r.signals.some((s) => s.id === "name"));
});

test("unreachable registry is not silently ignored", () => {
  const r = score(base({ registry: undefined, errors: { registry: "timeout" } }), { now: NOW });
  assert.ok(r.signals.some((s) => s.id === "registry" && s.level === "warn"));
});
