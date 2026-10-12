import { useEffect, useState } from "react";
import { invokeBackend, isTauriRuntime } from "../lib/platform";

interface MonitorStatus {
  checked_at: string | null;
  phase: string;
  message: string;
  last_attempt_at: string | null;
  last_attempt_result: string | null;
  last_attempt_message: string | null;
}

export function AutoSwitchStatus() {
  const [status, setStatus] = useState<MonitorStatus | null>(null);
  const [readError, setReadError] = useState(false);
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    if (!isTauriRuntime()) return;
    let disposed = false;
    let stop: (() => void) | undefined;
    const refresh = async () => {
      try {
        const next = await invokeBackend<MonitorStatus>("get_auto_switch_status");
        if (!disposed) { setStatus(next); setReadError(false); setNow(Date.now()); }
      } catch {
        if (!disposed) { setReadError(true); setNow(Date.now()); }
      }
    };
    void refresh();
    const timer = window.setInterval(() => void refresh(), 5000);
    void import("@tauri-apps/api/event").then(async ({ listen }) => {
      const unlisten = await listen<MonitorStatus>("auto-switch-status-changed", ({ payload }) => {
        if (!disposed) { setStatus(payload); setReadError(false); setNow(Date.now()); }
      });
      if (disposed) unlisten(); else stop = unlisten;
    }).catch(() => {});
    return () => { disposed = true; window.clearInterval(timer); stop?.(); };
  }, []);

  if (!isTauriRuntime()) return null;
  const age = status?.checked_at ? now - Date.parse(status.checked_at) : Infinity;
  const stale = !!status?.checked_at && age > 120_000;
  const failed = readError || stale || status?.phase === "failed" || status?.phase === "waiting";
  const message = readError ? "자동 전환 상태 조회 실패" : stale
    ? "자동 전환 감시 응답 대기 · 마지막 확인이 2분 이상 지났습니다"
    : status?.message || "자동 전환 감시 시작 중";
  const lastFailure = status?.last_attempt_result === "failed" && status.phase !== "failed";
  return (
    <div className={`mt-1 text-xs ${failed ? "text-amber-600 dark:text-amber-400" : "text-gray-500 dark:text-gray-400"}`} role="status">
      <span>{message}</span>
      {status?.checked_at && <span className="ml-2">확인 {new Date(status.checked_at).toLocaleTimeString()}</span>}
      {lastFailure && <div>최근 자동 전환 실패: {status.last_attempt_message} ({status.last_attempt_at && new Date(status.last_attempt_at).toLocaleTimeString()})</div>}
    </div>
  );
}
