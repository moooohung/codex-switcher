import assert from "node:assert/strict";
import test from "node:test";
import type { ClaudeAccount } from "../src/lib/claude.ts";
import { backendClaudeSort, CLAUDE_ACCOUNT_SEARCH_THRESHOLD, CLAUDE_REAUTHENTICATION_MESSAGE, claudeAccountForCard, claudeAccountRequiresReauthentication, completedClaudeLoginAccountId, defaultClaudeSort, matchesClaudeAccount, normalizeClaudeWarmupTimes, requiresClaudeReauthentication, sortClaudeAccounts, validClaudeBackupPassword } from "../src/components/ClaudeAccountView.ts";

function account(name: string, reset: number | null, used: number | null, expiry: string | null = null): ClaudeAccount {
  return {
    id: name, name, email: `${name}@example.invalid`, auth_mode: "claude_ai", is_active: false,
    plan_type: "max", subscription_expires_at: expiry, created_at: "2026-10-10", last_used_at: null,
    profile_directory: "synthetic", usage_observed_at: null,
    usage: { account_id: name, plan_type: "max", primary_resets_at: reset, primary_used_percent: used,
      primary_window_minutes: 300, secondary_resets_at: null, secondary_used_percent: null,
      secondary_window_minutes: 10080, has_credits: null, unlimited_credits: null, credits_balance: null, error: null },
  };
}
const names = (accounts: ClaudeAccount[]) => accounts.map((value) => value.name);

test("invalid grants and explicit sign-in-again errors require user reauthentication", () => {
  for (const error of [
    "OAuth refresh HTTP 400 (invalid_grant; sign in again); HTTP retry at 1791547200; last known quota retained",
    '{"error":"invalid_grant","error_description":"Refresh token expired"}',
    "INVALID_GRANT",
    "Sign in again",
    "Please SIGN\nIN\tAGAIN to continue.",
    "sign-in-again",
  ]) assert.equal(requiresClaudeReauthentication(error), true, error);
});

test("transient network and generic HTTP errors are not classified as expired authentication", () => {
  for (const error of [
    null, undefined, "", "OAuth refresh HTTP 400", "OAuth refresh HTTP 401", "HTTP 401 Unauthorized",
    "OAuth refresh HTTP 403 (invalid_client)", "OAuth usage HTTP 429; HTTP retry at 1791547200",
    "OAuth HTTP transport error", "Network error: connection reset", "Request timed out", "HTTP 503",
    "invalid_grant_retry", "not_invalid_grant", "Sign in with a Claude Code subscription account",
  ]) assert.equal(requiresClaudeReauthentication(error), false, String(error));
});

test("expired authentication is explained in Korean without changing raw errors or observed quota", () => {
  const value = account("expired", 100, 37);
  const raw = "OAuth refresh HTTP400 invalid_grant; HTTP retry at 1791547200";
  value.usage!.error = raw;
  const original = structuredClone(value);
  assert.equal(claudeAccountRequiresReauthentication(value), true);
  const card = claudeAccountForCard(value);
  assert.notEqual(card, value);
  assert.notEqual(card.usage, value.usage);
  assert.equal(card.usage!.error, CLAUDE_REAUTHENTICATION_MESSAGE);
  assert.match(card.usage!.error!, /인증이 만료/);
  assert.match(card.usage!.error!, /재로그인/);
  assert.doesNotMatch(card.usage!.error!, /invalid_grant|1791547200/);
  assert.deepEqual({ ...card.usage, error: raw }, original.usage);
  assert.deepEqual(value, original);
});

test("unknown quota stays unknown and other account errors remain untouched", () => {
  const value = account("unknown", null, null);
  value.usage!.error = "Sign in again";
  const card = claudeAccountForCard(value);
  assert.equal(card.usage!.primary_used_percent, null);
  assert.equal(card.usage!.secondary_used_percent, null);
  assert.equal(card.usage!.primary_resets_at, null);
  assert.equal(card.usage!.credits_balance, null);
  for (const error of [null, "OAuth usage HTTP 401", "Network request timed out"]) {
    value.usage!.error = error;
    assert.equal(claudeAccountRequiresReauthentication(value), false);
    assert.equal(claudeAccountForCard(value), value);
  }
  value.usage = undefined;
  assert.equal(claudeAccountRequiresReauthentication(value), false);
  assert.equal(claudeAccountForCard(value), value);
});

test("login completion requires explicit backend success and the same expected account ID", () => {
  assert.equal(completedClaudeLoginAccountId({ pending: false, account_id: "expected", refreshed: true }, "expected"), "expected");
  assert.equal(completedClaudeLoginAccountId({ pending: false, account_id: "new-account" }, null), "new-account");
  for (const result of [
    null, undefined, false, {}, { pending: true, account_id: "expected" },
    { pending: false }, { pending: false, account_id: "" }, { pending: false, account_id: "  " },
    { pending: false, account_id: "different" }, { pending: false, account_id: 123 },
    { pending: "false", account_id: "expected" }, { account_id: "expected" },
  ]) assert.equal(completedClaudeLoginAccountId(result, "expected"), null, JSON.stringify(result));
});

test("reset directions keep unobserved accounts last and never mutate the snapshot", () => {
  const input = [account("missing", null, null), account("late", 200, 10), account("early", 100, 80)];
  assert.deepEqual(names(sortClaudeAccounts(input, "deadline_asc")), ["early", "late", "missing"]);
  assert.deepEqual(names(sortClaudeAccounts(input, "deadline_desc")), ["late", "early", "missing"]);
  assert.deepEqual(names(input), ["missing", "late", "early"]);
});

test("remaining directions use primary quota and place unknown quota last", () => {
  const input = [account("missing", null, null), account("low", 100, 90), account("high", 200, 10)];
  assert.deepEqual(names(sortClaudeAccounts(input, "remaining_desc")), ["high", "low", "missing"]);
  assert.deepEqual(names(sortClaudeAccounts(input, "remaining_asc")), ["low", "high", "missing"]);
});

test("secondary quota is the fallback when primary quota is unavailable", () => {
  const secondary = account("secondary", null, null);
  secondary.usage!.secondary_resets_at = 50;
  secondary.usage!.secondary_used_percent = 5;
  const primary = account("primary", 100, 10);
  assert.deepEqual(names(sortClaudeAccounts([primary, secondary], "deadline_asc")), ["secondary", "primary"]);
  assert.deepEqual(names(sortClaudeAccounts([primary, secondary], "remaining_desc")), ["secondary", "primary"]);
});

test("expiry sorting handles malformed and missing dates without inventing data", () => {
  const input = [account("unknown", 50, 20), account("malformed", 100, 20, "invalid"), account("late", 100, 20, "2026-12-01"), account("early", 100, 20, "2026-11-01")];
  assert.deepEqual(names(sortClaudeAccounts(input, "subscription_asc")), ["early", "late", "unknown", "malformed"]);
  assert.deepEqual(names(sortClaudeAccounts(input, "subscription_desc")), ["late", "early", "unknown", "malformed"]);
});

test("equal deadlines break ties by remaining quota then name", () => {
  const input = [account("zeta", 100, 50), account("alpha", 100, 50), account("spare", 100, 10)];
  assert.deepEqual(names(sortClaudeAccounts(input, "deadline_asc")), ["spare", "alpha", "zeta"]);
});

test("search normalizes whitespace and case, and accepts missing email", () => {
  const value = account("Alpha", 100, 10);
  assert.equal(CLAUDE_ACCOUNT_SEARCH_THRESHOLD, 8);
  assert.equal(matchesClaudeAccount(value, "  ALPHA  "), true);
  assert.equal(matchesClaudeAccount(value, "EXAMPLE.INVALID"), true);
  value.email = null;
  assert.equal(matchesClaudeAccount(value, ""), true);
  assert.equal(matchesClaudeAccount(value, "other"), false);
});

test("UI sort directions stay within the backend's three accepted sort values", () => {
  assert.equal(backendClaudeSort("remaining_asc"), "usage");
  assert.equal(backendClaudeSort("deadline_desc"), "reset");
  assert.equal(backendClaudeSort("subscription_desc"), "reset");
  assert.equal(backendClaudeSort("name"), "name");
  assert.equal(defaultClaudeSort("usage"), "remaining_desc");
  assert.equal(defaultClaudeSort("name"), "name");
});

test("timer input rejects impossible times and deduplicates valid schedules", () => {
  assert.equal(normalizeClaudeWarmupTimes("24:00, 09:00"), null);
  assert.equal(normalizeClaudeWarmupTimes("9:00"), null);
  assert.deepEqual(normalizeClaudeWarmupTimes("18:30, 09:00 18:30"), ["09:00", "18:30"]);
  assert.deepEqual(normalizeClaudeWarmupTimes(""), []);
});

test("encrypted export requires matching passwords with twelve Unicode characters", () => {
  const elevenEmoji = "😀".repeat(11);
  const twelveEmoji = "😀".repeat(12);
  assert.equal(validClaudeBackupPassword(elevenEmoji, elevenEmoji), false);
  assert.equal(validClaudeBackupPassword(twelveEmoji, twelveEmoji), true);
  assert.equal(validClaudeBackupPassword("abcdefghijkl", "abcdefghijkX"), false);
});
