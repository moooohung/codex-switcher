export type PromptCacheProvider = "codex" | "claude";

export interface PromptCacheSession {
  /** Opaque backend ID; pass it back unchanged, never interpret it as a path. */
  id: string;
  title: string;
  cwd: string;
  model: string | null;
  updated_at: number;
  resumable: boolean;
  reason: string | null;
  source: string;
}

export interface PromptCacheSessionList {
  sessions: PromptCacheSession[];
  truncated: boolean;
  reason: string | null;
}

export interface PromptCacheStatus {
  state: "observed" | "unknown";
  /** Epoch seconds of the actual request, never the status poll. */
  observed_at: number | null;
  model: string | null;
  session_id: string | null;
  cache_read_tokens: number | null;
  cache_write_tokens: number | null;
  uncached_input_tokens: number | null;
  expires_at: number | null;
  ttl_seconds: number | null;
  ttl_source: string | null;
  reason: string | null;
  auto_keepalive_enabled?: boolean;
  /** Living adapters with a fresh leased heartbeat, independent of cache/keepalive success. */
  auto_keepalive_connected_sessions?: number;
  /** Verified living registrations; a registration does not establish a timer connection. */
  auto_keepalive_registered_sessions?: number;
  /** Verified registered sessions currently processing a response. */
  auto_keepalive_busy_sessions?: number;
  auto_keepalive_supported?: boolean;
  auto_keepalive_reason?: string;
  auto_keepalive_state?: string;
  /** Keepalive timestamps are epoch seconds, independently of cache observations. */
  auto_keepalive_next_at?: number | null;
  auto_keepalive_last_at?: number | null;
  auto_keepalive_requests?: number;
  auto_keepalive_cache_read_tokens?: number | null;
  auto_keepalive_output_tokens?: number | null;
}

export interface PromptCacheObservation {
  observedAt: number | null;
  cacheReadTokens: number | null;
  cacheWriteTokens: number | null;
  uncachedInputTokens: number | null;
  hitPercent: number | null;
}

export interface PromptCacheTtl {
  expiresAt: number;
  remainingSeconds: number;
  remainingPercent: number;
}

export const PROMPT_CACHE_WARMUP_NOTICE =
  "워밍업은 별도 문맥으로 실행되며 작업 캐시를 연장하지 않습니다.";

export const PROMPT_CACHE_AUTO_KEEPALIVE_NOTICE =
  "자동 유지 상태와 토큰은 서버가 보고한 값입니다. 요청 전송이나 예약만으로 캐시 유지 성공을 보장하지 않으며, 미측정 출력은 0으로 표시하지 않습니다.";

const CLAUDE_KEEPALIVE_NOTICE =
  "공식 인터랙티브 훅 설치가 확인된 Claude는 일반 사용자 입력으로 이미 열린 세션을 등록하고, 자동 유지 옵션이 켜져 있고 유지 조건이 충족되면 해당 입력에 대한 응답 완료를 확인한 뒤 유지 타이머를 연결합니다. 세션 선택·재개나 새 세션·재시작은 필요하지 않습니다. 같은 작업 세션의 유지 요청은 짧은 cachekeep 알림으로 대화 기록에 표시되며 한도를 소비합니다. 설치 확인은 실제 연결 수나 캐시 유지 성공의 증거가 아닙니다.";
const CODEX_KEEPALIVE_NOTICE =
  "Native Codex는 연결된 열린 관리 CLI 세션에 자동 유지를 적용하며 다시 선택할 필요가 없습니다. 최초 연결이 필요하면 도움말에서 선택한 기존 대화를 관리 CLI 세션에서 이어가거나 새 세션을 열어 로컬 캡처 전송 경로를 사용합니다. 기존 대화는 기록과 작업 폴더를 이어 사용합니다. 기존 앱·작업·전역 설정은 변경하지 않습니다. 무출력 예열은 지원되는 GPT 5.6+ 모델에 한정되며 실제 출력·캐시 읽기는 측정값으로 확인합니다. Codex 웹 경로는 지원하지 않습니다.";

const KEEPALIVE_LABELS = {
  off: "꺼짐",
  waiting_session: "세션 연결 대기",
  idle: "요청 대기",
  busy: "대화 사용 중",
  waiting_for_stop: "응답 완료 대기",
  timer_waiting: "유지 타이머 연결 대기",
  scheduled: "요청 예약",
  pending: "요청 준비 대기",
  expired: "캐시 만료",
  error: "유지 요청 오류",
  unsupported: "현재 경로 미지원",
  // Current managed adapters also report these more specific lifecycle states.
  disabled: "자동 유지 중단",
  monitor_unavailable: "모니터 연결 대기",
  needs_managed_session: "관리 세션 연결 필요",
  waiting_for_request: "첫 요청 대기",
  awaiting_prewarm: "예열 지원 확인 대기",
  prewarming: "예열 요청 중",
  active: "유지 요청 대기",
  stopped: "관리 세션 종료",
  limit_reached: "자동 요청 한도 도달",
  ready: "유지 요청 대기",
  observation_required: "새 캐시 관측 대기",
  human_input_required: "사용자 입력 대기",
  human_idle_limit: "유휴 제한으로 중단",
  awaiting_response: "유지 응답 대기",
  cancelled: "유지 요청 취소",
  failed: "Claude 응답 오류 · 복구 대기",
  ended: "세션 종료",
  quota_unavailable: "사용량·한도 확인 필요",
  account_inactive: "계정 세션 연결 대기",
  runtime_unavailable: "설정 저장됨 · 연결 확인 필요",
  daily_budget: "일일 요청 한도 도달",
  unknown: "상태 확인 불가",
} as const;

type KeepaliveState = keyof typeof KEEPALIVE_LABELS;

const KEEPALIVE_REASONS: Record<string, string> = {
  codex_web_unsupported: "Codex 웹 경로는 지원하지 않습니다.",
  native_codex_route_pending: "Native Codex 유지 요청 경로를 분석 중입니다.",
  model_unsupported: "무출력 예열은 지원되는 GPT 5.6+ 모델에 한정됩니다.",
  waiting_session: "연결된 관리 작업 세션을 기다립니다.",
  session_busy: "대화가 사용 중이어서 유지 요청을 기다립니다.",
  waiting_for_stop: "등록된 Claude 세션의 해당 입력에 대한 응답 완료를 기다립니다.",
  timer_waiting: "등록된 유휴 Claude 세션의 유지 타이머 연결을 기다립니다.",
  claude_official_interactive_plugin_monitor: "공식 인터랙티브 플러그인 모니터가 같은 Claude 세션의 유지 요청을 관리합니다.",
  claude_official_live_hooks: "공식 Claude 훅 설정은 실행 중에도 반영됩니다. 일반 사용자 입력으로 이미 열린 Claude 세션을 등록하고, 자동 유지 옵션이 켜져 있고 유지 조건이 충족되면 해당 입력에 대한 응답 완료를 확인한 뒤 유지 타이머를 연결합니다. 세션 선택·재개나 새 세션·재시작은 필요하지 않습니다.",
  native_codex_managed_transport: "기존 대화 이어가기 또는 새 관리 Codex CLI 세션의 로컬 캡처 전송 경로를 사용합니다.",
};

const SESSION_REASONS: Record<string, string> = {
  session_busy: "이 대화가 사용 중입니다. 사용 중인 세션을 종료한 뒤 다시 시도해 주세요.",
  session_in_use: "이 대화가 사용 중입니다. 사용 중인 세션을 종료한 뒤 다시 시도해 주세요.",
  session_not_found: "저장된 대화를 찾지 못했습니다. 목록을 새로고침해 주세요.",
  session_not_resumable: "이 대화는 현재 이어갈 수 없습니다. 다른 대화를 선택해 주세요.",
  invalid_session_id: "대화 ID를 확인하지 못했습니다. 목록을 새로고침해 주세요.",
  account_mismatch: "이 계정에 연결된 대화가 아닙니다. 목록을 새로고침해 주세요.",
  auto_keepalive_disabled: "먼저 이 계정의 캐시 자동 유지 옵션을 켜 주세요.",
  keepalive_disabled: "먼저 이 계정의 캐시 자동 유지 옵션을 켜 주세요.",
  codex_web_unsupported: "Codex 웹 경로는 기존 대화 이어가기를 지원하지 않습니다.",
  no_saved_sessions: "저장된 대화가 없습니다.",
};

export function getPromptCacheSessionReason(reason: unknown): string | undefined {
  const safe = getPromptCacheReason(typeof reason === "string" ? reason : undefined);
  if (!safe) return undefined;
  if (Object.prototype.hasOwnProperty.call(SESSION_REASONS, safe)) return SESSION_REASONS[safe];
  if (/\b(?:exception|panic|traceback|bearer|api[_-]?key|access[_-]?token|refresh[_-]?token)\b/i.test(safe)) return undefined;
  // Preserve human explanations; unfamiliar machine codes are not useful UI text.
  return /[가-힣]|\s/.test(safe) ? safe : undefined;
}

export function getPromptCacheSessionError(error: unknown, fallback: string): string {
  return getPromptCacheSessionReason(error instanceof Error ? error.message : error) ?? fallback;
}

function getKeepaliveReason(reason: string | undefined, outputTokens: number | null): string | undefined {
  const safe = getPromptCacheReason(reason);
  if (!safe) return undefined;
  // Translate route reasons without displaying exception text, paths or old blanket claims.
  if (Object.prototype.hasOwnProperty.call(KEEPALIVE_REASONS, safe)) return KEEPALIVE_REASONS[safe];
  if (/현재 CLI.*(?:미지원|지원하지|지원.*않|보낼 수 없)|zero.output|cache.success|캐시.*성공/i.test(safe)) return undefined;
  if (/\b(?:error|exception|panic|traceback|bearer|api[_-]?key|access[_-]?token|refresh[_-]?token)\b/i.test(safe)) return undefined;
  if (/출력\s*0|무출력.*(?:성공|완료|확인했습니다)/.test(safe) && outputTokens !== 0) {
    return "예열 결과는 실제 출력·캐시 읽기 측정값으로 확인합니다. 입력 캐시 비용은 발생할 수 있습니다.";
  }
  // Plain Korean backend explanations are already user-facing; unfamiliar codes stay unknown.
  return /[가-힣]/.test(safe) ? safe : undefined;
}

function keepaliveTimestamp(value: unknown): number | null {
  return typeof value === "number" && Number.isSafeInteger(value) && value > 0 && value <= 8_640_000_000_000
    ? value : null;
}

function sessionCount(value: unknown): number | null {
  const count = tokenCount(value);
  return count !== null && count <= 0xffff_ffff ? count : null;
}

export function getPromptCacheAutoKeepalive(
  status: PromptCacheStatus | null | undefined,
  nowSeconds: number = Date.now() / 1000,
  provider?: PromptCacheProvider,
) {
  const enabled = status?.auto_keepalive_enabled === true;
  const supported = status?.auto_keepalive_supported === true;
  const settingKnown = typeof status?.auto_keepalive_enabled === "boolean";
  // A cached observation, saved history row or successful request is not a live connection.
  const connectedSessions = tokenCount(status?.auto_keepalive_connected_sessions);
  const registeredSessions = sessionCount(status?.auto_keepalive_registered_sessions);
  const reportedBusySessions = sessionCount(status?.auto_keepalive_busy_sessions);
  const busySessions = registeredSessions !== null && reportedBusySessions !== null && reportedBusySessions > registeredSessions
    ? null : reportedBusySessions;
  const reportedState = status?.auto_keepalive_state;
  const state: KeepaliveState = !settingKnown ? "unknown" : !enabled ? "off"
    : reportedState && Object.prototype.hasOwnProperty.call(KEEPALIVE_LABELS, reportedState)
      ? reportedState as KeepaliveState : "unknown";
  const outputTokens = tokenCount(status?.auto_keepalive_output_tokens);
  const reason = getKeepaliveReason(status?.auto_keepalive_reason, outputTokens);
  let registeredNotice: string | null = null;
  if (provider === "claude" && registeredSessions !== null && registeredSessions > 0) {
    const detected = `감지된 열린 Claude 세션 ${registeredSessions}개`;
    if (!settingKnown) registeredNotice = `${detected} · 자동 유지 설정 확인 불가`;
    else if (!enabled) registeredNotice = `${detected} · 자동 유지 꺼짐`;
    else if (reportedState === "unsupported" || status?.auto_keepalive_supported === false) registeredNotice = `${detected} · 현재 경로 미지원`;
    else if (!supported) registeredNotice = `${detected} · 자동 유지 지원 확인 필요`;
    else if (["idle", "pending", "error", "expired", "disabled", "stopped", "ended", "failed", "cancelled",
      "human_idle_limit", "human_input_required", "observation_required", "awaiting_response", "quota_unavailable",
      "daily_budget", "limit_reached"].includes(state)) {
      const detail = status?.auto_keepalive_reason === "claude_official_live_hooks" ? undefined : reason;
      registeredNotice = `${detected} · ${KEEPALIVE_LABELS[state]}${detail ? ` · ${detail}` : ""}`;
    }
    else if (connectedSessions === null) registeredNotice = `${detected} · 유지 타이머 연결 상태 확인 불가`;
    else if (["busy", "waiting_for_stop", "timer_waiting"].includes(state) && busySessions !== null && busySessions > 0) registeredNotice = `${detected} · 작업 중 ${busySessions}개 · 응답 완료 후 자동 유지 대기`;
    else if (reportedState === "waiting_for_stop" || reportedState === "busy") registeredNotice = `${detected} · 응답 완료 후 자동 유지 대기`;
    else if (state === "timer_waiting") registeredNotice = `${detected} · 유지 타이머 연결 대기`;
    else registeredNotice = `${detected} · 자동 유지 상태 확인 필요`;
  }
  const waitingForClaudeInput = provider === "claude" && enabled && supported && connectedSessions === 0
    && status?.auto_keepalive_reason === "claude_official_live_hooks"
    && status.auto_keepalive_state !== "unsupported";
  const connectionNotice = connectedSessions !== null && connectedSessions > 0
    ? enabled ? `연결된 열린 세션 ${connectedSessions}개에 자동 적용 · 다시 선택할 필요 없음`
      : settingKnown ? `연결된 열린 세션 ${connectedSessions}개 · 옵션을 켜면 자동 적용`
        : `연결된 열린 세션 ${connectedSessions}개 · 자동 유지 설정 확인 불가`
    : registeredNotice ?? (waitingForClaudeInput ? "열린 Claude 세션 등록 대기 · 일반 사용자 입력으로 세션을 등록합니다."
      : "열린 세션 연결 대기 · 외부에서 실행한 세션은 최초 연결이 필요합니다.");
  const hasSchedule = ["scheduled", "ready", "awaiting_prewarm", "active"].includes(state);
  const nextAt = hasSchedule ? keepaliveTimestamp(status?.auto_keepalive_next_at) : null;
  const reportedLastAt = keepaliveTimestamp(status?.auto_keepalive_last_at);
  const lastAt = positiveNumber(nowSeconds) && reportedLastAt !== null && reportedLastAt <= nowSeconds
    ? reportedLastAt : null;
  const requests = tokenCount(status?.auto_keepalive_requests);
  const cacheReadTokens = tokenCount(status?.auto_keepalive_cache_read_tokens);
  const nextRequestLabel = !hasSchedule ? null
    : nextAt === null || !positiveNumber(nowSeconds) ? "다음 요청 시각 미확인"
      : nextAt > nowSeconds ? `다음 요청 ${formatPromptCacheDuration(nextAt - nowSeconds)} 후`
        : "다음 요청 · 실행 대기";
  return {
    enabled,
    connectedSessions,
    registeredSessions,
    busySessions,
    connectionNotice,
    supported,
    state,
    label: !settingKnown ? "설정 확인 불가" : KEEPALIVE_LABELS[state],
    tooltip: [
      provider === "claude" ? CLAUDE_KEEPALIVE_NOTICE : provider === "codex" ? CODEX_KEEPALIVE_NOTICE : null,
      reason ?? (status?.auto_keepalive_reason ? "서버의 유지 경로 사유를 확인할 수 없습니다." : null),
      PROMPT_CACHE_AUTO_KEEPALIVE_NOTICE,
    ].filter(Boolean).join(" "),
    nextAt,
    nextRequestLabel,
    lastAt,
    requests,
    cacheReadTokens,
    outputTokens,
    hasUsage: (requests !== null && requests > 0) || lastAt !== null || cacheReadTokens !== null || outputTokens !== null,
  };
}

export function getPromptCacheReason(reason: string | null | undefined): string | undefined {
  if (typeof reason !== "string") return undefined;
  const text = reason.trim();
  // Reasons are backend explanations, never raw exceptions or file paths.
  if (
    !text || text.length > 240 ||
    /[\\<>\u0000-\u001f\u007f]|:\/\/|[a-z]:\/|(?:^|[\s"'(])(?:~?\/|\.\.?\/)/i.test(text)
  ) return undefined;
  return text;
}

function positiveNumber(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value) && value > 0;
}

function tokenCount(value: unknown): number | null {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0
    ? value
    : null;
}

export function getPromptCacheObservation(
  status: PromptCacheStatus | null | undefined,
  nowSeconds: number,
): PromptCacheObservation | null {
  if (
    status?.state !== "observed" ||
    !positiveNumber(nowSeconds) ||
    (status.observed_at !== null && status.observed_at !== undefined &&
      (!positiveNumber(status.observed_at) || status.observed_at > nowSeconds))
  ) return null;

  const cacheReadTokens = tokenCount(status.cache_read_tokens);
  const cacheWriteTokens = tokenCount(status.cache_write_tokens);
  const uncachedInputTokens = tokenCount(status.uncached_input_tokens);
  let hitPercent: number | null = null;
  // Missing counters are unknown, not zero. Cache writes are input, not hits.
  if (cacheReadTokens !== null && cacheWriteTokens !== null && uncachedInputTokens !== null) {
    const total = cacheReadTokens + cacheWriteTokens + uncachedInputTokens;
    if (Number.isSafeInteger(total) && total > 0) {
      hitPercent = Math.min(100, Math.max(0, (cacheReadTokens / total) * 100));
    }
  }

  return {
    observedAt: status.observed_at ?? null,
    cacheReadTokens,
    cacheWriteTokens,
    uncachedInputTokens,
    hitPercent,
  };
}

export function getPromptCacheTtl(
  status: PromptCacheStatus | null | undefined,
  nowSeconds: number,
): PromptCacheTtl | null {
  const observation = getPromptCacheObservation(status, nowSeconds);
  if (
    !observation ||
    !status ||
    !positiveNumber(observation.observedAt) ||
    !((observation.cacheReadTokens ?? 0) > 0 || (observation.cacheWriteTokens ?? 0) > 0) ||
    !positiveNumber(status.ttl_seconds) ||
    !positiveNumber(status.expires_at) ||
    status.expires_at <= observation.observedAt
  ) return null;

  const requestExpiry = observation.observedAt + status.ttl_seconds;
  if (!Number.isFinite(requestExpiry)) return null;
  // Even inconsistent backend expiry data cannot extend the request's TTL.
  const expiresAt = Math.min(status.expires_at, requestExpiry);
  const remainingSeconds = Math.max(0, Math.min(status.ttl_seconds, expiresAt - nowSeconds));
  return {
    expiresAt,
    remainingSeconds,
    remainingPercent: Math.min(100, Math.max(0, (remainingSeconds / status.ttl_seconds) * 100)),
  };
}

export function formatPromptCacheDuration(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds < 0) return "확인 불가";
  const wholeSeconds = Math.ceil(seconds);
  if (wholeSeconds < 60) return `${wholeSeconds}초`;
  if (wholeSeconds < 3600) return `${Math.floor(wholeSeconds / 60)}분 ${wholeSeconds % 60}초`;
  return `${Math.floor(wholeSeconds / 3600)}시간 ${Math.floor((wholeSeconds % 3600) / 60)}분`;
}

export function formatPromptCacheScope(
  model: string | null | undefined,
  sessionId: string | null | undefined,
): string {
  // Reject paths/URLs rather than displaying even a shortened path or tooltip.
  const safeModel = typeof model === "string" && model === model.trim() && /^[a-zA-Z0-9][a-zA-Z0-9._-]{0,63}$/.test(model)
    ? model
    : "확인 불가";
  let safeSession = "확인 불가";
  if (typeof sessionId === "string" && sessionId === sessionId.trim() && /^[a-zA-Z0-9_-]{1,128}$/.test(sessionId)) {
    safeSession = sessionId.length > 12
      ? `${sessionId.slice(0, 4)}…${sessionId.slice(-4)}`
      : sessionId.length > 5
        ? `${sessionId.slice(0, 2)}…${sessionId.slice(-2)}`
        : "••••";
  }
  return `모델 ${safeModel} · 세션 ${safeSession}`;
}
