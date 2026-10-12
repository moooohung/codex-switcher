import { useEffect, useState } from "react";
import App from "../App";
import { ClaudeApp } from "./ClaudeApp";
import { applyTheme, readStoredTheme } from "../lib/theme";
import { CLAUDE_THEME_EVENT, CLAUDE_THEME_KEY } from "../lib/claude";

type Provider = "codex" | "claude";
export function ProviderTabs() {
  const [provider, setProvider] = useState<Provider>(() => localStorage.getItem("switcher-active-provider") === "claude" ? "claude" : "codex");
  useEffect(() => {
    const changed = (event: StorageEvent) => { if (event.key === "switcher-active-provider") setProvider(event.newValue === "claude" ? "claude" : "codex"); };
    window.addEventListener("storage", changed);
    return () => window.removeEventListener("storage", changed);
  }, []);
  useEffect(() => {
    const sync = () => {
      const theme = localStorage.getItem(CLAUDE_THEME_KEY) ?? "system";
      const claudeDark = theme === "dark" || (theme === "system" && matchMedia("(prefers-color-scheme: dark)").matches);
      applyTheme(provider === "claude" ? (claudeDark ? "dark" : "light") : readStoredTheme());
    };
    sync();
    const media = matchMedia("(prefers-color-scheme: dark)");
    media.addEventListener("change", sync);
    window.addEventListener(CLAUDE_THEME_EVENT, sync);
    return () => { media.removeEventListener("change", sync); window.removeEventListener(CLAUDE_THEME_EVENT, sync); };
  }, [provider]);
  const select = (next: Provider) => { setProvider(next); localStorage.setItem("switcher-active-provider", next); };
  return <>
    <nav aria-label="계정 서비스 선택" className="sticky top-0 z-50 border-b border-gray-200 bg-white dark:border-gray-800 dark:bg-gray-950">
      <div role="tablist" aria-label="계정 관리" className="mx-auto flex max-w-5xl gap-2 px-4 py-2">
        {(["codex", "claude"] as const).map((key) => <button key={key} id={`${key}-tab`} role="tab" aria-selected={provider === key} aria-controls={`${key}-panel`} tabIndex={provider === key ? 0 : -1}
          onClick={() => select(key)} onKeyDown={(event) => { if (["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key)) { event.preventDefault(); const next = event.key === "Home" ? "codex" : event.key === "End" ? "claude" : provider === "codex" ? "claude" : "codex"; select(next); document.getElementById(`${next}-tab`)?.focus(); } }}
          className={`rounded-lg px-5 py-2 text-sm font-semibold focus-visible:outline-2 focus-visible:outline-orange-500 ${provider === key ? "bg-orange-100 text-orange-900 dark:bg-orange-950 dark:text-orange-200" : "text-gray-500 hover:bg-gray-100 dark:hover:bg-gray-800"}`}>
          {key === "codex" ? "Codex" : "Claude Code"}
        </button>)}
      </div>
    </nav>
    <section id="codex-panel" role="tabpanel" aria-labelledby="codex-tab" hidden={provider !== "codex"}><App /></section>
    <section id="claude-panel" role="tabpanel" aria-labelledby="claude-tab" hidden={provider !== "claude"}><ClaudeApp /></section>
  </>;
}
