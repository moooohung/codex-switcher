import type { ClaudeAccount, ClaudeSettings } from "../lib/claude";

export const CLAUDE_ACCOUNT_SEARCH_THRESHOLD = 8;
export const CLAUDE_SORT_KEY = "claude-switcher-other-accounts-sort";
export const claudeSortOptions = [
  ["deadline_asc", "Reset: earliest to latest"],
  ["deadline_desc", "Reset: latest to earliest"],
  ["remaining_desc", "% remaining: highest to lowest"],
  ["remaining_asc", "% remaining: lowest to highest"],
  ["subscription_asc", "Expiry: earliest to latest"],
  ["subscription_desc", "Expiry: latest to earliest"],
  ["name", "Name: A to Z"],
] as const;
export type ClaudeAccountSort = typeof claudeSortOptions[number][0];

export const CLAUDE_REAUTHENTICATION_MESSAGE = "Claude 인증이 만료되었거나 해제되었습니다. ‘재로그인’으로 공식 Claude Code에서 이 계정에 로그인해 주세요. 인증을 갱신할 때까지 계정 전환과 워밍업을 사용할 수 없습니다.";

export function requiresClaudeReauthentication(error: string | null | undefined): boolean {
  return typeof error === "string" && (/(?:^|[^a-z0-9_])invalid_grant(?:$|[^a-z0-9_])/i.test(error)
    || /\bsign[\s-]+in[\s-]+again\b/i.test(error));
}

export function claudeAccountRequiresReauthentication(account: ClaudeAccount): boolean {
  return requiresClaudeReauthentication(account.usage?.error);
}

export function claudeAccountForCard(account: ClaudeAccount): ClaudeAccount {
  if (!account.usage || !claudeAccountRequiresReauthentication(account)) return account;
  return { ...account, usage: { ...account.usage, error: CLAUDE_REAUTHENTICATION_MESSAGE } };
}

export function completedClaudeLoginAccountId(result: unknown, expectedAccountId: string | null): string | null {
  if (!result || typeof result !== "object" || !("pending" in result) || result.pending !== false
    || !("account_id" in result) || typeof result.account_id !== "string" || !result.account_id.trim()) return null;
  return expectedAccountId === null || result.account_id === expectedAccountId ? result.account_id : null;
}

export function isClaudeAccountSort(value: unknown): value is ClaudeAccountSort {
  return claudeSortOptions.some(([key]) => key === value);
}

export function backendClaudeSort(sort: ClaudeAccountSort): ClaudeSettings["sort_by"] {
  return sort === "name" ? "name" : sort.startsWith("remaining") ? "usage" : "reset";
}

export function defaultClaudeSort(sort: ClaudeSettings["sort_by"]): ClaudeAccountSort {
  return sort === "name" ? "name" : sort === "usage" ? "remaining_desc" : "deadline_asc";
}

export function matchesClaudeAccount(account: ClaudeAccount, query: string): boolean {
  const normalized = query.trim().toLowerCase();
  return !normalized || account.name.toLowerCase().includes(normalized)
    || account.email?.toLowerCase().includes(normalized) === true;
}

function compareOptional(a: number | null, b: number | null, descending = false): number {
  if (a === null) return b === null ? 0 : 1;
  if (b === null) return -1;
  return descending ? b - a : a - b;
}

export function sortClaudeAccounts(accounts: readonly ClaudeAccount[], sort: ClaudeAccountSort): ClaudeAccount[] {
  const deadline = (account: ClaudeAccount) => account.usage?.primary_resets_at ?? account.usage?.secondary_resets_at ?? null;
  const remaining = (account: ClaudeAccount) => {
    const used = account.usage?.primary_used_percent ?? account.usage?.secondary_used_percent;
    return used == null ? null : Math.max(0, 100 - used);
  };
  const expiry = (account: ClaudeAccount) => {
    const timestamp = account.subscription_expires_at ? Date.parse(account.subscription_expires_at) : NaN;
    return Number.isFinite(timestamp) ? timestamp : null;
  };
  return [...accounts].sort((a, b) => {
    if (sort === "name") return a.name.localeCompare(b.name);
    if (sort.startsWith("subscription")) {
      const difference = compareOptional(expiry(a), expiry(b), sort === "subscription_desc");
      if (difference) return difference;
    }
    if (sort.startsWith("remaining")) {
      const difference = compareOptional(remaining(a), remaining(b), sort === "remaining_desc");
      if (difference) return difference;
    }
    const difference = compareOptional(deadline(a), deadline(b), sort === "deadline_desc");
    return difference || compareOptional(remaining(a), remaining(b), true) || a.name.localeCompare(b.name);
  });
}

export function normalizeClaudeWarmupTimes(value: string): string[] | null {
  const times = value.split(/[\s,]+/).filter(Boolean);
  if (times.some((time) => !/^([01]\d|2[0-3]):[0-5]\d$/.test(time))) return null;
  return [...new Set(times)].sort();
}

export function validClaudeBackupPassword(password: string, confirmation: string): boolean {
  return Array.from(password).length >= 12 && password === confirmation;
}
