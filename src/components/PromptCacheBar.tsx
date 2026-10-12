import { useEffect, useRef, useState } from "react";
import { invokeBackend } from "../lib/platform";
import {
  formatPromptCacheDuration,
  formatPromptCacheScope,
  getPromptCacheAutoKeepalive,
  getPromptCacheObservation,
  getPromptCacheReason,
  getPromptCacheSessionError,
  getPromptCacheSessionReason,
  getPromptCacheTtl,
  PROMPT_CACHE_WARMUP_NOTICE,
} from "../lib/promptCache";
import type { PromptCacheProvider, PromptCacheSession, PromptCacheSessionList, PromptCacheStatus } from "../lib/promptCache";

interface PromptCacheBarProps {
  provider: PromptCacheProvider;
  accountId: string;
}

interface CacheSnapshot extends PromptCacheBarProps {
  status: PromptCacheStatus | null;
}

interface SessionChooser extends PromptCacheBarProps {
  result: PromptCacheSessionList | null;
  selectedId: string | null;
}

interface RequestScope extends PromptCacheBarProps {
  active: boolean;
  busy: boolean;
  revision: number;
}

interface CacheMutation extends PromptCacheBarProps {
  busy: boolean;
  error: string | null;
  action?: "preference" | "open_session" | "list_sessions";
}

const STATUS_POLL_MS = 30_000;
const COUNTDOWN_TICK_MS = 10_000;

function formatTokens(tokens: number | null): string {
  return tokens === null ? "확인 불가" : tokens.toLocaleString();
}

export function PromptCacheBar({ provider, accountId }: PromptCacheBarProps) {
  const [snapshot, setSnapshot] = useState<CacheSnapshot | null>(null);
  const [nowSeconds, setNowSeconds] = useState(() => Date.now() / 1000);
  const requestScope = useRef<RequestScope | null>(null);
  const [mutation, setMutation] = useState<CacheMutation | null>(null);
  const [sessionChooser, setSessionChooser] = useState<SessionChooser | null>(null);

  useEffect(() => {
    if (!accountId.trim()) return;
    let disposed = false;
    let inFlight = false;
    const scope: RequestScope = { provider, accountId, active: true, busy: false, revision: 0 };
    requestScope.current = scope;
    setMutation(null);
    setSessionChooser(null);
    setNowSeconds(Date.now() / 1000);

    const readStatus = async () => {
      if (disposed || inFlight || scope.busy) return;
      inFlight = true;
      const revision = scope.revision;
      try {
        // This reads existing observations; never send a model/warmup request.
        const status = await invokeBackend<PromptCacheStatus>("get_prompt_cache_status", {
          provider,
          accountId,
        });
        if (!disposed && revision === scope.revision) {
          setNowSeconds(Date.now() / 1000);
          setSnapshot({ provider, accountId, status });
          // Keep an action refusal visible until the user retries an action.
          setMutation((current) => current?.error && current.action
            ? current : { provider, accountId, busy: false, error: null });
        }
      } catch {
        if (!disposed && revision === scope.revision) {
          setMutation({ provider, accountId, busy: false, error: "캐시 상태를 불러오지 못했습니다." });
        }
      } finally {
        inFlight = false;
      }
    };

    void readStatus();
    const poll = window.setInterval(() => void readStatus(), STATUS_POLL_MS);
    const countdown = window.setInterval(() => setNowSeconds(Date.now() / 1000), COUNTDOWN_TICK_MS);
    return () => {
      disposed = true;
      scope.active = false;
      window.clearInterval(poll);
      window.clearInterval(countdown);
    };
  }, [provider, accountId]);

  // Hide the previous account synchronously, before the next effect runs.
  const status = snapshot?.provider === provider && snapshot.accountId === accountId
    ? snapshot.status
    : null;
  const currentMutation = mutation?.provider === provider && mutation.accountId === accountId
    ? mutation : null;
  const currentChooser = sessionChooser?.provider === provider && sessionChooser.accountId === accountId
    ? sessionChooser : null;
  const selectedSession = currentChooser?.selectedId === null ? undefined
    : currentChooser?.result?.sessions.find((session) => session.id === currentChooser.selectedId && session.resumable);
  const autoKeepalive = getPromptCacheAutoKeepalive(status, nowSeconds, provider);
  const keepaliveLabel = `${autoKeepalive.label}${currentMutation?.busy
    ? currentMutation.action === "open_session" ? " · 세션 여는 중…"
      : currentMutation.action === "list_sessions" ? " · 대화 목록 불러오는 중…" : " · 설정 저장 중…"
    : ""}`;

  const setAutoKeepalive = async (enabled: boolean) => {
    const scope = requestScope.current;
    if (!status || !scope?.active || scope.busy || scope.provider !== provider || scope.accountId !== accountId) return;
    scope.busy = true;
    // Invalidate reads started before the preference write.
    scope.revision += 1;
    setMutation({ provider, accountId, busy: true, error: null, action: "preference" });
    try {
      const next = await invokeBackend<PromptCacheStatus>("set_prompt_cache_auto_keepalive", {
        provider, accountId, enabled,
      });
      if (scope.active && requestScope.current === scope) {
        setSnapshot({ provider, accountId, status: next });
        setNowSeconds(Date.now() / 1000);
      }
    } catch {
      if (scope.active && requestScope.current === scope) {
        setMutation({ provider, accountId, busy: false, error: "설정을 저장하지 못했습니다. 다시 시도해 주세요." });
      }
      return;
    } finally {
      scope.busy = false;
    }
    if (scope.active && requestScope.current === scope) {
      setMutation({ provider, accountId, busy: false, error: null });
    }
  };

  const listSessions = async () => {
    const scope = requestScope.current;
    if (!accountId.trim() || !scope?.active || scope.busy ||
      scope.provider !== provider || scope.accountId !== accountId) return;
    scope.busy = true;
    scope.revision += 1;
    const revision = scope.revision;
    // Refreshing always requires a fresh explicit choice, never the first row.
    setSessionChooser({ provider, accountId, result: null, selectedId: null });
    setMutation({ provider, accountId, busy: true, error: null, action: "list_sessions" });
    try {
      const result = await invokeBackend<PromptCacheSessionList>("list_prompt_cache_sessions", { provider, accountId });
      if (scope.active && requestScope.current === scope && revision === scope.revision) {
        setSessionChooser({ provider, accountId, result, selectedId: null });
        setMutation({ provider, accountId, busy: false, error: null });
      }
    } catch (error) {
      if (scope.active && requestScope.current === scope && revision === scope.revision) {
        setMutation({ provider, accountId, busy: false, action: "list_sessions",
          error: getPromptCacheSessionError(error, "저장된 대화 목록을 불러오지 못했습니다. 다시 시도해 주세요.") });
      }
    } finally {
      scope.busy = false;
    }
  };

  const openKeepaliveSession = async (session?: PromptCacheSession) => {
    const scope = requestScope.current;
    if (!autoKeepalive.enabled || !scope?.active || scope.busy ||
      scope.provider !== provider || scope.accountId !== accountId) return;
    if (session && (!session.resumable || selectedSession !== session)) return;
    scope.busy = true;
    scope.revision += 1;
    setMutation({ provider, accountId, busy: true, error: null, action: "open_session" });
    let opened = false;
    try {
      await invokeBackend<unknown>("open_prompt_cache_session", {
        provider, accountId, ...(session ? { sessionId: session.id } : {}),
      });
      opened = true;
      if (!scope.active || requestScope.current !== scope) return;
      // Starting a route does not establish a cache hit or successful keepalive.
      const next = await invokeBackend<PromptCacheStatus>("get_prompt_cache_status", { provider, accountId });
      if (scope.active && requestScope.current === scope) {
        setSnapshot({ provider, accountId, status: next });
        setNowSeconds(Date.now() / 1000);
        setMutation({ provider, accountId, busy: false, error: null });
      }
    } catch (error) {
      if (scope.active && requestScope.current === scope) {
        setMutation({ provider, accountId, busy: false, action: "open_session", error: opened
          ? "세션 열기 요청 후 상태를 불러오지 못했습니다."
          : getPromptCacheSessionError(error, session
            ? "선택한 대화를 이어가지 못했습니다. 목록을 새로고침한 뒤 다시 시도해 주세요."
            : "캐시 유지 세션을 열지 못했습니다. 다시 시도해 주세요.") });
      }
    } finally {
      scope.busy = false;
    }
  };
  const observation = getPromptCacheObservation(status, nowSeconds);
  const ttl = getPromptCacheTtl(status, nowSeconds);
  const label = !observation
    ? "프롬프트 캐시 · 관측 없음"
    : !ttl
      ? "프롬프트 캐시 · 만료시간 확인 불가"
      : ttl.remainingSeconds > 0
        ? "프롬프트 캐시 · 예상 잔여 시간"
        : "프롬프트 캐시 · 예상 만료";
  const remainingLabel = ttl ? formatPromptCacheDuration(ttl.remainingSeconds) : null;
  const colorClass = ttl && ttl.remainingPercent <= 10
    ? "bg-red-500"
    : ttl && ttl.remainingPercent <= 30
      ? "bg-amber-500"
      : "bg-emerald-500";

  return (
    <div className="space-y-2">
      <div className="space-y-1">
        <div className="flex flex-wrap justify-between gap-x-2 text-xs text-gray-500 dark:text-gray-400">
          <span title={!ttl ? getPromptCacheReason(status?.reason) : undefined}>{label}</span>
          {remainingLabel !== null && <span>{remainingLabel} (추정)</span>}
        </div>
        {ttl ? (
          <div
            className="h-1.5 bg-gray-100 dark:bg-gray-800 rounded-full overflow-hidden"
            role="progressbar"
            aria-label="프롬프트 캐시 예상 잔여 시간"
            aria-valuemin={0}
            aria-valuemax={100}
            aria-valuenow={ttl.remainingPercent}
            aria-valuetext={`예상 잔여 ${remainingLabel} (마지막 요청 기준 추정)`}
          >
            <div
              className={`h-full transition-all duration-300 ${colorClass}`}
              style={{ width: `${ttl.remainingPercent}%` }}
            />
          </div>
        ) : (
          <div className="h-1.5 bg-gray-100 dark:bg-gray-800 rounded-full overflow-hidden" aria-hidden="true" />
        )}
      </div>
      {observation && status && (
        <div className="space-y-1 text-xs text-gray-500 dark:text-gray-400">
          <div className="flex flex-wrap justify-between gap-x-2">
            <span title={observation.observedAt === null ? undefined : new Date(observation.observedAt * 1000).toLocaleString()}>
              {observation.observedAt === null ? "요청 시각 확인 불가" : `마지막 요청 ${new Date(observation.observedAt * 1000).toLocaleTimeString()}`}
            </span>
            <span title="마지막 요청의 캐시 읽기 / 전체 입력 토큰 비율이며, 현재 재사용을 보장하지 않습니다.">
              적중률 {observation.hitPercent === null ? "확인 불가" : `${observation.hitPercent.toFixed(1)}%`}
            </span>
          </div>
          <div>
            캐시 읽기 {formatTokens(observation.cacheReadTokens)} · 쓰기 {formatTokens(observation.cacheWriteTokens)} · 비캐시 {formatTokens(observation.uncachedInputTokens)} 토큰
          </div>
          <div className="break-words">{formatPromptCacheScope(status.model, status.session_id)}</div>
        </div>
      )}
      <div className="text-xs text-gray-500 dark:text-gray-400">
        <div className="flex flex-wrap items-center justify-between gap-2" title={autoKeepalive.tooltip}>
          <label className="flex items-center gap-1.5">
            <input
              type="checkbox"
              checked={autoKeepalive.enabled}
              disabled={!accountId.trim() || typeof status?.auto_keepalive_enabled !== "boolean" || currentMutation?.busy}
              onChange={(event) => void setAutoKeepalive(event.target.checked)}
            />
            캐시 자동 유지
          </label>
          <span role="status" tabIndex={0} title={autoKeepalive.tooltip} aria-label={`${keepaliveLabel}. ${autoKeepalive.tooltip}`}>
            {keepaliveLabel}
          </span>
        </div>
        <div role="status" title="서버가 확인한 살아 있는 어댑터 연결 수입니다. 캐시 적중·예열 완료·만료시간을 보장하지 않습니다.">
          {autoKeepalive.connectionNotice}
        </div>
        <details key={`${provider}:${accountId}`} className="mt-1">
          <summary className="cursor-pointer">세션 연결 도움말</summary>
          <div className="mt-2 space-y-2">
            <p>연결된 열린 세션은 옵션을 켜면 자동 적용되며 다시 선택할 필요가 없습니다.</p>
            <p>외부에서 실행한 세션은 최초 연결이 필요합니다. 연결 어댑터를 지원하지 않는 실행 중인 외부 프로세스에는 자동으로 연결할 수 없습니다.</p>
            <p>아래 버튼은 새 관리 세션을 열거나 저장된 대화를 이어갑니다. 실행 중인 외부 세션에 접속하는 기능은 아닙니다.</p>
            <div className="flex flex-wrap gap-x-3 gap-y-1">
              {autoKeepalive.enabled && (
                <button
                  type="button"
                  className="text-xs underline cursor-pointer disabled:opacity-50 disabled:cursor-wait"
                  title={autoKeepalive.tooltip}
                  disabled={!accountId.trim() || currentMutation?.busy}
                  onClick={() => void openKeepaliveSession()}
                >
                  {currentMutation?.busy && currentMutation.action === "open_session" ? "세션 여는 중…" : "캐시 유지 세션 열기"}
                </button>
              )}
              <button
                type="button"
                className="text-xs underline cursor-pointer disabled:opacity-50 disabled:cursor-wait"
                disabled={!accountId.trim() || currentMutation?.busy}
                aria-expanded={currentChooser !== null}
                onClick={() => currentChooser ? setSessionChooser(null) : void listSessions()}
              >
                기존 대화 이어가기
              </button>
            </div>
            {currentChooser && (
              <div className="mt-2 space-y-2 rounded border border-gray-200 dark:border-gray-700 p-2">
                <div className="flex items-center justify-between gap-2">
                  <span>이어갈 대화를 직접 선택해 주세요.</span>
                  <button
                    type="button"
                    className="underline disabled:opacity-50"
                    disabled={currentMutation?.busy}
                    onClick={() => void listSessions()}
                  >
                    목록 새로고침
                  </button>
                </div>
                {currentMutation?.busy && currentMutation.action === "list_sessions" && (
                  <div role="status">저장된 대화를 불러오는 중…</div>
                )}
                {currentChooser.result && (
                  <>
                    {currentChooser.result.sessions.length === 0 && <div>저장된 대화가 없습니다.</div>}
                    {currentChooser.result.reason && (
                      <div>{getPromptCacheSessionReason(currentChooser.result.reason) ?? "대화 목록의 일부 정보를 확인하지 못했습니다."}</div>
                    )}
                    {currentChooser.result.truncated && <div>일부 저장된 대화만 표시되었습니다.</div>}
                    {currentChooser.result.sessions.length > 0 && (
                      <fieldset className="max-h-64 space-y-2 overflow-y-auto" disabled={currentMutation?.busy}>
                        <legend className="sr-only">이어갈 기존 대화</legend>
                        {currentChooser.result.sessions.map((session) => (
                          <label key={session.id} className="flex items-start gap-2 break-words">
                            <input
                              type="radio"
                              name={`prompt-cache-session-${provider}-${accountId}`}
                              value={session.id}
                              checked={currentChooser.selectedId === session.id}
                              disabled={!session.resumable || currentMutation?.busy}
                              onChange={() => {
                                if (!session.resumable || requestScope.current?.busy) return;
                                setSessionChooser((current) => current?.provider === provider && current.accountId === accountId
                                  && current.result?.sessions.includes(session) ? { ...current, selectedId: session.id } : current);
                              }}
                            />
                            <span className="min-w-0 space-y-0.5">
                              <span className="block text-gray-700 dark:text-gray-200">{session.title.trim() || "제목 없는 대화"}</span>
                              <span className="block">{session.cwd || "작업 폴더 확인 불가"}</span>
                              <span className="block">
                                {session.model ?? "모델 확인 불가"}
                                {Number.isFinite(session.updated_at) && session.updated_at > 0 && (
                                  <span>{` · ${new Date(session.updated_at * 1000).toLocaleString()}`}</span>
                                )}
                              </span>
                              {!session.resumable && (
                                <span className="block">{getPromptCacheSessionReason(session.reason) ?? "이 대화는 현재 이어갈 수 없습니다."}</span>
                              )}
                            </span>
                          </label>
                        ))}
                      </fieldset>
                    )}
                  </>
                )}
                <div>선택한 대화 기록과 작업 폴더를 이어 사용합니다.</div>
                {!autoKeepalive.enabled && <div>대화를 열려면 이 계정의 캐시 자동 유지를 켜 주세요.</div>}
                <button
                  type="button"
                  className="underline disabled:opacity-50 disabled:cursor-not-allowed"
                  disabled={!selectedSession || !autoKeepalive.enabled || currentMutation?.busy}
                  onClick={() => { if (selectedSession) void openKeepaliveSession(selectedSession); }}
                >
                  {currentMutation?.busy && currentMutation.action === "open_session" ? "세션 여는 중…" : "선택한 대화 이어가기"}
                </button>
              </div>
            )}
          </div>
        </details>
        {autoKeepalive.nextRequestLabel && (
          <div title={autoKeepalive.nextAt === null ? undefined : new Date(autoKeepalive.nextAt * 1000).toLocaleString()}>
            {autoKeepalive.nextRequestLabel}
          </div>
        )}
        {autoKeepalive.hasUsage && (
          <div className="space-y-1" title="서버가 보고한 유지 요청 횟수와 토큰입니다. 미측정 값은 0이 아니며, 이후 요청의 캐시 적중을 보장하지 않습니다.">
            <div>
              유지 요청 {autoKeepalive.requests === null ? "횟수 미확인" : `${formatTokens(autoKeepalive.requests)}회`}
              {autoKeepalive.lastAt !== null && (
                <span title={new Date(autoKeepalive.lastAt * 1000).toLocaleString()}>
                  {` · 마지막 ${new Date(autoKeepalive.lastAt * 1000).toLocaleTimeString()}`}
                </span>
              )}
            </div>
            <div>
              캐시 읽기 {autoKeepalive.cacheReadTokens === null ? "미측정" : formatTokens(autoKeepalive.cacheReadTokens)} · 출력 {autoKeepalive.outputTokens === null ? "미측정" : formatTokens(autoKeepalive.outputTokens)} 토큰
            </div>
          </div>
        )}
        {currentMutation?.error && <div role="alert" className="text-red-500">{currentMutation.error}</div>}
        <span className="cursor-help" tabIndex={0} title={PROMPT_CACHE_WARMUP_NOTICE} aria-label={PROMPT_CACHE_WARMUP_NOTICE}>
          워밍업 안내
        </span>
      </div>
    </div>
  );
}
