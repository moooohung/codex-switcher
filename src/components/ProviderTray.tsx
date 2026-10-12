import { useEffect, useState } from "react";
import TrayMenu from "../TrayMenu";
import { invokeBackend, isTauriRuntime } from "../lib/platform";
import { invokeClaude, CLAUDE_THEME_KEY, type ClaudeSnapshot } from "../lib/claude";
import { applyTheme, readStoredTheme } from "../lib/theme";

export function ProviderTray() {
  const [provider, setProvider] = useState(() => localStorage.getItem("switcher-active-provider") === "claude" ? "claude" : "codex");
  const [snapshot, setSnapshot] = useState<ClaudeSnapshot | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    const sync = (event: StorageEvent) => { if (event.key === "switcher-active-provider") setProvider(event.newValue === "claude" ? "claude" : "codex"); };
    window.addEventListener("storage", sync);
    return () => window.removeEventListener("storage", sync);
  }, []);
  useEffect(() => {
    if (provider !== "claude") { applyTheme(readStoredTheme()); return; }
    const theme = localStorage.getItem(CLAUDE_THEME_KEY) ?? "system";
    applyTheme(theme === "dark" || (theme === "system" && matchMedia("(prefers-color-scheme: dark)").matches) ? "dark" : "light");
    let disposed = false;
    const refresh = async () => { try { const next = await invokeClaude<ClaudeSnapshot>("snapshot"); if (!disposed) setSnapshot(next); } catch (err) { if (!disposed) setError(String(err)); } };
    void refresh(); const timer = window.setInterval(() => void refresh(), 5000);
    return () => { disposed = true; window.clearInterval(timer); };
  }, [provider]);
  const select = (value: string) => { setProvider(value); localStorage.setItem("switcher-active-provider", value); };
  const open = async (settings = false) => {
    localStorage.setItem("switcher-active-provider", "claude");
    await invokeBackend("open_main_window");
    if (settings && isTauriRuntime()) { const { emit } = await import("@tauri-apps/api/event"); await emit("claude-settings-requested"); }
  };
  return <div className="bg-white text-gray-900 dark:bg-gray-900 dark:text-gray-100">
    <nav aria-label="트레이 계정 서비스" className="flex gap-2 border-b border-gray-200 px-3 py-2 dark:border-gray-700">{["codex", "claude"].map((key) => <button key={key} aria-pressed={provider === key} className={`rounded px-3 py-1 text-xs ${provider === key ? "bg-orange-100 text-orange-800 dark:bg-orange-950 dark:text-orange-200" : "text-gray-500"}`} onClick={() => select(key)}>{key === "codex" ? "Codex" : "Claude"}</button>)}</nav>
    <div hidden={provider !== "codex"}><TrayMenu /></div>
    {provider === "claude" && <div className="max-h-[370px] overflow-auto p-3 text-xs">
      <div className="mb-3 flex justify-between"><strong>Claude Code</strong><button disabled={busy} onClick={() => void open(true)}>설정 ↗</button></div>
      {error && <p role="alert" className="mb-2 text-red-500">{error}</p>}
      {!snapshot?.accounts.length && <button onClick={() => void open()}>계정 추가하기 ↗</button>}
      {snapshot?.accounts.map((account) => <button key={account.id} disabled={busy} className={`mb-2 w-full rounded-lg border p-3 text-left disabled:opacity-50 ${account.is_active ? "border-emerald-400" : "border-gray-200 dark:border-gray-700"}`} onClick={async () => { setBusy(true); setError(null); try { await invokeClaude("switch", { accountId: account.id }); setSnapshot(await invokeClaude("snapshot")); } catch (err) { setError(String(err)); } finally { setBusy(false); } }}>
        <span className={snapshot.masked_account_ids.includes(account.id) ? "blur-sm" : ""}>{account.name}</span>{account.is_active && " ✓ 인증 적용"}
        <div className="mt-1 text-gray-500">{account.usage?.primary_used_percent != null && `5h ${Math.round(100 - account.usage.primary_used_percent)}% 남음`}{account.usage?.secondary_used_percent != null && ` · 7d ${Math.round(100 - account.usage.secondary_used_percent)}% 남음`}{account.usage?.error && " · 사용량 관측 대기"}</div>
      </button>)}
      <p className="mt-2 text-gray-500">{snapshot?.monitor_status.message}</p><button className="mt-3" onClick={() => void open()}>Claude 계정 관리 열기 ↗</button>
    </div>}
  </div>;
}
