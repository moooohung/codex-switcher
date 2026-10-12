import { useEffect, useId, useRef, useState, type ReactNode } from "react";
import type { ClaudeSettings } from "../lib/claude";
import { planPreferences, type PlanPreference } from "../lib/planPreference";
import { normalizeClaudeWarmupTimes } from "./ClaudeAccountView";

export const claudeButtonClass = "px-4 py-2 text-sm font-medium rounded-lg bg-gray-100 dark:bg-gray-800 hover:bg-gray-200 dark:hover:bg-gray-700 text-gray-700 dark:text-gray-200 transition-colors disabled:opacity-50";
export const claudeFieldClass = "w-full rounded-lg border border-gray-300 dark:border-gray-700 bg-white dark:bg-gray-800 px-3 py-2 text-sm text-gray-900 dark:text-gray-100 disabled:opacity-50";

export function ClaudeDialog({ title, onClose, children, footer, error, busy = false }: {
  title: string; onClose: () => void; children: ReactNode; footer?: ReactNode; error?: string | null; busy?: boolean;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const titleId = useId();
  const closeRef = useRef(onClose);
  const busyRef = useRef(busy);
  useEffect(() => { closeRef.current = onClose; busyRef.current = busy; }, [onClose, busy]);
  useEffect(() => {
    const previous = document.activeElement as HTMLElement | null;
    const visible = () => ref.current && !ref.current.closest("[hidden]");
    const elements = () => Array.from(ref.current?.querySelectorAll<HTMLElement>("button:not(:disabled),input:not(:disabled),select:not(:disabled),textarea:not(:disabled),[tabindex='0']") ?? []);
    if (visible()) elements()[0]?.focus();
    const key = (event: KeyboardEvent) => {
      if (!visible()) return;
      if (event.key === "Escape" && !busyRef.current) { event.preventDefault(); closeRef.current(); }
      if (event.key === "Tab") {
        const list = elements(), first = list[0], last = list[list.length - 1];
        if (!list.length) { event.preventDefault(); ref.current?.focus(); }
        else if (event.shiftKey && document.activeElement === first) { event.preventDefault(); last?.focus(); }
        else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first?.focus(); }
      }
    };
    document.addEventListener("keydown", key);
    return () => { document.removeEventListener("keydown", key); if (visible() && previous?.isConnected) previous.focus(); };
  }, []);
  return <div className="fixed inset-0 bg-black/40 flex items-center justify-center z-[70]" onMouseDown={(event) => { if (event.target === event.currentTarget && !busy) onClose(); }}>
    <div ref={ref} role="dialog" aria-modal="true" aria-labelledby={titleId} tabIndex={-1} className="bg-white dark:bg-gray-900 border border-gray-200 dark:border-gray-700 rounded-2xl w-full max-w-md mx-4 shadow-xl">
      <div className="p-5 border-b border-gray-100 dark:border-gray-800"><h2 id={titleId} className="text-lg font-semibold text-gray-900 dark:text-gray-100">{title}</h2></div>
      <div className="p-5 space-y-3 max-h-[65vh] overflow-y-auto">{error && <p role="alert" className="text-sm text-red-600 dark:text-red-300">{error}</p>}{children}</div>
      <div className="flex justify-end gap-3 p-5 border-t border-gray-100 dark:border-gray-800">{footer}<button type="button" onClick={onClose} disabled={busy} className={claudeButtonClass}>{footer ? "Cancel" : "Done"}</button></div>
    </div>
  </div>;
}

export function ClaudeSettingsDialog({ initial, onSave, onClose, busy, error }: {
  initial: ClaudeSettings; onSave: (settings: ClaudeSettings) => Promise<void>; onClose: () => void; busy: boolean; error: string | null;
}) {
  const [settings, setSettings] = useState(initial);
  const [times, setTimes] = useState(initial.timed_warmup_times.join(", "));
  const [validationError, setValidationError] = useState<string | null>(null);
  const formId = useId();
  const update = <K extends keyof ClaudeSettings>(key: K, value: ClaudeSettings[K]) => setSettings((old) => ({ ...old, [key]: value }));
  return <ClaudeDialog title="Settings" onClose={onClose} busy={busy} error={validationError ?? error} footer={<button type="submit" form={formId} disabled={busy} className={claudeButtonClass}>{busy ? "Saving..." : "Save"}</button>}>
    <form id={formId} onSubmit={(event) => {
      event.preventDefault();
      const normalized = normalizeClaudeWarmupTimes(times);
      if (!normalized) { setValidationError("예약 시간은 HH:MM 형식으로 입력하세요 (00:00–23:59)."); return; }
      setValidationError(null); void onSave({ ...settings, timed_warmup_times: normalized });
    }} className="space-y-3">
      <p className="text-xs text-gray-500 dark:text-gray-400">Claude Code 계정과 설정에만 적용됩니다.</p>
      <label className="flex items-center justify-between gap-3 text-sm font-medium text-gray-900 dark:text-gray-100">Auto-switch on low quota (자동 불판 교체)<input type="checkbox" checked={settings.auto_switch_enabled} disabled={busy} onChange={(event) => update("auto_switch_enabled", event.target.checked)} className="h-4 w-4 accent-blue-600" /></label>
      <label className="block text-sm font-medium">자동 전환 우선 등급<select className={claudeFieldClass} value={settings.auto_switch_plan_preference ?? "any"} disabled={busy} onChange={(event) => update("auto_switch_plan_preference", event.target.value as PlanPreference)}>{planPreferences.claude.map(({ value, label }) => <option key={value} value={value}>{label}</option>)}</select></label>
      <p className="text-xs text-gray-500 dark:text-gray-400">최근 사용량이 확인된 우선 등급을 먼저 선택하고, 여유 계정이 없으면 다른 등급을 사용합니다.</p>
      <label className="block text-sm font-medium">전환 임계값 (Switch threshold)<input className={claudeFieldClass} type="number" min="1" max="100" step="0.5" required disabled={busy} value={settings.auto_switch_threshold_percent} onChange={(event) => update("auto_switch_threshold_percent", Number(event.target.value))} /></label>
      <div className="border-t border-gray-100 dark:border-gray-800" />
      <label className="flex items-center gap-2 text-sm"><input type="checkbox" checked={settings.close_before_switch} disabled={busy} onChange={(event) => update("close_before_switch", event.target.checked)} className="h-4 w-4 accent-orange-600" />전환 전에 Switcher에서 연 Claude 창 종료</label>
      <label className="flex items-center gap-2 text-sm"><input type="checkbox" checked={settings.reopen_after_switch} disabled={busy} onChange={(event) => update("reopen_after_switch", event.target.checked)} className="h-4 w-4 accent-orange-600" />전환 후 선택한 계정으로 새 Claude 창 열기</label>
      <p className="text-xs text-gray-500 dark:text-gray-400">Switcher 밖에서 실행한 대화는 유지됩니다.</p>
      <label className="flex items-center gap-2 text-sm"><input type="checkbox" checked={settings.auto_remote_control ?? false} disabled={busy} onChange={(event) => update("auto_remote_control", event.target.checked)} className="h-4 w-4 accent-blue-600" />계정 전환 뒤 열린 대화 리모트 컨트롤 자동 재연결</label>
      <p className="text-xs text-gray-500 dark:text-gray-400">새 대화는 시작 시 연결합니다. 관측된 Windows 터미널 대화는 계정 변경으로 연결이 끊기면 비어 있는 대기 프롬프트에서 재연결합니다. 작업·입력·승인 중이면 보류하며 최초 연결 동의는 직접 확인해야 합니다. 별도 계정으로 고정한 대화와 Claude Desktop의 Code 탭에는 적용되지 않습니다.</p>
      <div className="border-t border-gray-100 dark:border-gray-800" />
      <label className="flex items-center gap-2 text-sm"><input type="checkbox" checked={settings.capture_usage} disabled={busy} onChange={(event) => update("capture_usage", event.target.checked)} className="h-4 w-4 accent-blue-600" />공식 상태줄에서 사용량과 세션 통계 수집</label>
      <label className="flex items-center gap-2 text-sm"><input type="checkbox" checked={settings.warmup_all} disabled={busy} onChange={(event) => update("warmup_all", event.target.checked)} className="h-4 w-4 accent-emerald-600" />할당량 초기화 후 모든 계정 자동 워밍업</label>
      <p className="text-xs text-gray-500 dark:text-gray-400">워밍업은 Haiku 5.5·낮은 추론 강도·최대 출력 1토큰으로 고정되며 할당량을 사용합니다.</p>
      <label className="flex items-center gap-3 text-sm"><input type="checkbox" checked={settings.protect_prompt_cache ?? false} disabled={busy} onChange={(event) => update("protect_prompt_cache", event.target.checked)} className="h-4 w-4 accent-emerald-600" />프롬프트 캐시 보호 (1시간 TTL 요청)</label>
      <p className="text-xs text-gray-500 dark:text-gray-400">Switcher에서 새로 여는 Claude 대화와 워밍업에 적용합니다. 기존 캐시 설정을 우선하며, API·크레딧 사용 시 캐시 쓰기 비용이 늘 수 있습니다. 워밍업은 작업 대화를 재개하거나 모델을 바꾸지 않습니다. 별도 문맥의 워밍업으로 작업 캐시가 연장되지는 않습니다.</p>
      <label className="block text-sm font-medium">예약 워밍업 시간 (현지 시각, HH:MM)<input className={claudeFieldClass} value={times} disabled={busy} onChange={(event) => setTimes(event.target.value)} placeholder="09:00, 18:30" /></label>
      <label className="block text-sm font-medium">Claude 대화용 모델<select className={claudeFieldClass} value={settings.model} disabled={busy} onChange={(event) => update("model", event.target.value as ClaudeSettings["model"])}><option value="sonnet">Sonnet</option><option value="opus">Opus</option><option value="haiku">Haiku</option></select></label>
      <label className="block text-sm font-medium">Appearance<select className={claudeFieldClass} value={settings.theme} disabled={busy} onChange={(event) => update("theme", event.target.value as ClaudeSettings["theme"])}><option value="system">System</option><option value="light">Light</option><option value="dark">Dark</option></select></label>
    </form>
  </ClaudeDialog>;
}
