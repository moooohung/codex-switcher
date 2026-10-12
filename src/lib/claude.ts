import { invokeBackend } from "./platform.ts";
import type { AccountWithUsage } from "../types";
import type { PlanPreference } from "./planPreference";

export interface ClaudeSettings {
  auto_switch_enabled: boolean;
  auto_switch_threshold_percent: number;
  auto_switch_plan_preference: PlanPreference;
  close_before_switch: boolean;
  reopen_after_switch: boolean;
  capture_usage: boolean;
  protect_prompt_cache?: boolean;
  auto_remote_control?: boolean | null;
  warmup_all: boolean;
  warmup_account_ids: string[];
  timed_warmup_times: string[];
  theme: "light" | "dark" | "system";
  sort_by: "reset" | "usage" | "name";
  model: "sonnet" | "opus" | "haiku";
}
export interface ClaudeAccount extends AccountWithUsage {
  profile_directory: string;
  usage_observed_at: number | null;
}
export interface ClaudeRemoteControlStatus {
  checked_at: number;
  registered: number;
  connected: number;
  requested: number;
  waiting: number;
  runtime_bound?: number;
  waiting_reasons?: Record<string, number>;
  retry_at?: number | null;
  transport: string;
  error: string | null;
}
export interface ClaudeSnapshot {
  accounts: ClaudeAccount[];
  settings: ClaudeSettings;
  masked_account_ids: string[];
  active_account_id: string | null;
  cli_available: boolean;
  cli_latest_version: string;
  live_directory: string;
  managed_running_ids: string[];
  running_process_count?: number;
  running_process_error?: string | null;
  monitor_status: { phase: string; message: string; switched_at: string | null };
  remote_control_status?: ClaudeRemoteControlStatus;
  warmup_status?: {
    version: number;
    last_checked_at: number | null;
    accounts: Record<string, {
      phase: "idle" | "running" | "pending" | "backoff" | "uncertain";
      last_checked_at: number | null;
      last_attempt_at: number | null;
      last_success_at: number | null;
      next_retry_at: number | null;
      last_error: string | null;
      pending_tasks: number;
    }>;
  };
}
export function invokeClaude<T>(action: string, payload: Record<string, unknown> = {}): Promise<T> {
  return invokeBackend<T>("claude_command", { action, payload });
}
export const CLAUDE_THEME_EVENT = "claude-theme-changed";
export const CLAUDE_THEME_KEY = "claude-switcher-theme";

export interface ClaudeRunningPresentation {
  count: number | null;
  complete: boolean;
  label: string;
}

export function formatClaudeRunningStatus(
  snapshot: Pick<ClaudeSnapshot, "running_process_count" | "running_process_error"> | null,
  refreshFailed = false,
): ClaudeRunningPresentation | null {
  if (!snapshot) return null;
  const reported = snapshot.running_process_count;
  const count = typeof reported === "number" && Number.isSafeInteger(reported) && reported >= 0
    ? reported : null;
  const complete = count !== null && !snapshot.running_process_error && !refreshFailed;
  const label = count === null ? "Claude running: 전체 확인 대기"
    : refreshFailed ? `${count} Claude running · 마지막 관측값`
    : snapshot.running_process_error ? count > 0
      ? `${count} Claude running · 일부 확인 실패` : "Claude running: 확인 실패"
    : `${count} Claude running`;
  return { count, complete, label };
}

const CLAUDE_RC_WAITING_MESSAGES: Record<string, string> = {
  busy: "응답·작업 진행 중입니다. 응답이 완료된 뒤 재연결 상태를 확인합니다.",
  shell: "셸 화면입니다. 정상 Claude 입력창으로 돌아올 때까지 기다립니다.",
  permission_required: "Claude의 권한·사용자 확인을 기다립니다. 확인 화면이 있으면 직접 처리해 주세요.",
  response_failed: "응답이 실패했습니다. Claude의 오류를 확인하고 정상 입력창으로 돌아와 주세요.",
  waiting_for_empty_prompt: "빈 Claude 입력창 확인 대기 · 작성 중인 입력은 유지합니다.",
  waiting_for_stable_prompt: "Claude 입력창이 안정될 때까지 재연결을 보류합니다.",
  waiting_for_disconnect_evidence: "이전 계정의 RC 연결 해제를 아직 확인하지 못했습니다.",
  waiting_for_auth_transition: "Claude의 계정 변경 반영을 기다립니다. 새 계정 연결은 아직 확인하지 못했습니다.",
  initial_activation_required: "최초 RC 활성화·동의 확인이 필요합니다. Claude에서 RC 이용 자격과 최초 동의를 직접 확인해 주세요.",
  awaiting_connection_confirmation: "RC 연결 결과 확인을 기다립니다.",
  unavailable_or_changed: "CLI 상태에 접근할 수 없거나 실행 정보가 변경되어 연결을 확인할 수 없습니다.",
  policy_restricted: "조직·프로젝트 정책으로 RC가 제한됩니다. 해당 정책과 이용 자격을 확인해 주세요.",
  connection_unverified: "RC 연결을 아직 확인하지 못했습니다. 런타임 등록만으로 연결 완료를 판단하지 않습니다.",
};

export interface ClaudeRemoteControlPresentation {
  summary: string;
  details: string[];
  retryNotice: string | null;
}

// Only backend-confirmed connections contribute to connected; a timer never does.
export function formatClaudeRemoteControlStatus(
  status: ClaudeRemoteControlStatus | null | undefined,
  nowSeconds: number,
): ClaudeRemoteControlPresentation {
  if (!status) return {
    summary: "리모트 컨트롤: 상태 확인 대기",
    details: ["RC 상태 정보를 아직 받지 못했습니다."],
    retryNotice: null,
  };
  const summary = `리모트 컨트롤: 실행 세션 ${status.registered} · 연결 확인 ${status.connected} · 요청 후 확인 대기 ${status.requested} · 대기/보류 ${status.waiting}`
    + (status.runtime_bound === undefined ? "" : ` · 런타임 연결 등록 ${status.runtime_bound} (접속 확인 별도)`);
  const details: string[] = [];
  const stale = status.checked_at > 0 && nowSeconds - status.checked_at > 30;
  if (status.error) details.push("RC 상태 확인 오류입니다. 표시된 수치는 마지막 보고값입니다.");
  else if (!(status.checked_at > 0)) details.push("RC 모니터의 첫 상태 확인을 기다립니다.");
  else if (stale) details.push("최근 30초 동안 RC 상태가 갱신되지 않았습니다. 표시된 수치는 마지막 관측값입니다.");
  else if (status.registered === 0) details.push("감지된 Claude CLI 세션이 없습니다.");
  const reasons = Object.entries(status.waiting_reasons ?? {}).filter(([, count]) => Number.isInteger(count) && count > 0);
  for (const [reason, count] of reasons) {
    const message = Object.prototype.hasOwnProperty.call(CLAUDE_RC_WAITING_MESSAGES, reason)
      ? CLAUDE_RC_WAITING_MESSAGES[reason]
      : "대기 사유의 세부 내용을 아직 확인하지 못했습니다.";
    details.push(`${message} (${count}개)`);
  }
  if (status.waiting > 0 && reasons.length === 0) details.push("대기 사유를 아직 확인하지 못했습니다.");
  const retryAt = status.retry_at;
  let retryNotice: string | null = null;
  if (!status.error && !stale && status.checked_at > 0 && typeof retryAt === "number" && Number.isFinite(retryAt) && retryAt > 0) {
    const seconds = Math.ceil(retryAt - nowSeconds);
    retryNotice = seconds > 0
      ? `자동 재확인 ${seconds}초 후 (횟수 제한)`
      : "자동 재확인 예정 시각이 지났습니다. 결과 갱신을 기다립니다.";
  }
  return { summary, details, retryNotice };
}

export function orderClaudeAccounts(accounts: ClaudeAccount[], sort: ClaudeSettings["sort_by"]): ClaudeAccount[] {
  return [...accounts].sort((a, b) => {
    if (a.is_active !== b.is_active) return a.is_active ? -1 : 1;
    if (sort === "name") return a.name.localeCompare(b.name);
    if (sort === "usage") {
      const score = (account: ClaudeAccount) => Math.max(account.usage?.primary_used_percent ?? 101, account.usage?.secondary_used_percent ?? 0);
      const difference = score(a) - score(b);
      if (difference) return difference;
    } else {
      const reset = (account: ClaudeAccount) => Math.min(account.usage?.primary_resets_at ?? Infinity, account.usage?.secondary_resets_at ?? Infinity);
      const difference = reset(a) - reset(b);
      if (Number.isFinite(difference) && difference) return difference;
    }
    return a.name.localeCompare(b.name);
  });
}
