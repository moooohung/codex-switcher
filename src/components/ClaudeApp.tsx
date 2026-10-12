import { useCallback, useEffect, useMemo, useRef, useState, type MouseEvent } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { AccountCard } from "./AccountCard";
import { exportFullBackupFile, importFullBackupFile, isTauriRuntime } from "../lib/platform";
import { formatClaudeRemoteControlStatus, formatClaudeRunningStatus, invokeClaude, CLAUDE_THEME_EVENT, CLAUDE_THEME_KEY, type ClaudeSettings, type ClaudeSnapshot } from "../lib/claude";
import type { ImportAccountsSummary, WarmupSummary } from "../types";

import { ClaudeDialog as Dialog, ClaudeSettingsDialog, claudeButtonClass as button, claudeFieldClass as field } from "./ClaudeDialogs";
import { backendClaudeSort, CLAUDE_REAUTHENTICATION_MESSAGE, CLAUDE_SORT_KEY, claudeAccountForCard, claudeAccountRequiresReauthentication, claudeSortOptions, completedClaudeLoginAccountId, defaultClaudeSort, isClaudeAccountSort, matchesClaudeAccount, sortClaudeAccounts, validClaudeBackupPassword, type ClaudeAccountSort } from "./ClaudeAccountView";

const CLAUDE_LOGIN_TIMEOUT_MS = 10 * 60 * 1000;

export function ClaudeApp() {
  const [snapshot, setSnapshot] = useState<ClaudeSnapshot | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [remoteControlNow, setRemoteControlNow] = useState(() => Math.floor(Date.now() / 1000));
  const remoteControlEnabled = snapshot?.settings.auto_remote_control === true;
  const remoteControl = formatClaudeRemoteControlStatus(snapshot?.remote_control_status, remoteControlNow);
  const [operation, setOperation] = useState<{ action: string; accountId?: string } | null>(null);
  const [isAccountSearchOpen, setIsAccountSearchOpen] = useState(false);
  const [isActionsMenuOpen, setIsActionsMenuOpen] = useState(false);
  const [isNavMenuOpen, setIsNavMenuOpen] = useState(false);
  const [isTimedWarmupOpen, setIsTimedWarmupOpen] = useState(false);
  const [timedWarmupDraft, setTimedWarmupDraft] = useState("");
  const [isWindowMaximized, setIsWindowMaximized] = useState(false);
  const [confirmation, setConfirmation] = useState("");
  const [configCopied, setConfigCopied] = useState(false);
  const [windowAccountId, setWindowAccountId] = useState("");
  const [otherAccountsSort, setOtherAccountsSort] = useState<ClaudeAccountSort>(() => { try { const stored = localStorage.getItem(CLAUDE_SORT_KEY); return isClaudeAccountSort(stored) ? stored : "deadline_asc"; } catch { return "deadline_asc"; } });
  const actionsMenuRef = useRef<HTMLDivElement>(null);
  const navMenuRef = useRef<HTMLDivElement>(null);
  const rootRef = useRef<HTMLDivElement>(null);
  const snapshotRef = useRef<ClaudeSnapshot | null>(null);
  const loadPromiseRef = useRef<Promise<void> | null>(null);
  const sortInitialized = useRef(false);
  const pausedTimedWarmupTimes = useRef<string[]>([]);
  const busyRef = useRef(false);
  const loginAttemptRef = useRef(0);
  const loginSessionRef = useRef(false);
  const [modal, setModal] = useState<"add" | "reauthenticate" | "settings" | "export" | "import" | "encrypted-export" | "encrypted-import" | "windows" | null>(null);
  const [reauthenticateAccountId, setReauthenticateAccountId] = useState<string | null>(null);
  const [accountName, setAccountName] = useState("");
  const [contents, setContents] = useState("");
  const [password, setPassword] = useState("");
  const [loginPending, setLoginPending] = useState(false);
  const [query, setQuery] = useState("");
  const [deleteId, setDeleteId] = useState<string | null>(null);
  const load = useCallback((): Promise<void> => {
    if (loadPromiseRef.current) return loadPromiseRef.current;
    const pending = invokeClaude<ClaudeSnapshot>("snapshot").then((next) => {
      snapshotRef.current = next; setSnapshot(next); setLoadError(null);
      try {
        if (localStorage.getItem(CLAUDE_THEME_KEY) !== next.settings.theme) { localStorage.setItem(CLAUDE_THEME_KEY, next.settings.theme); window.dispatchEvent(new Event(CLAUDE_THEME_EVENT)); }
        if (!sortInitialized.current) { const stored = localStorage.getItem(CLAUDE_SORT_KEY); setOtherAccountsSort(isClaudeAccountSort(stored) ? stored : defaultClaudeSort(next.settings.sort_by)); }
      } catch { /* Storage restrictions do not prevent viewing accounts. */ }
      sortInitialized.current = true;
    }).catch((err) => { setLoadError(err instanceof Error ? err.message : String(err)); throw err; })
      .finally(() => { loadPromiseRef.current = null; setLoading(false); });
    loadPromiseRef.current = pending; return pending;
  }, []);
  useEffect(() => { void load().catch(() => {}); const timer = window.setInterval(() => { if (!busyRef.current) void load().catch(() => {}); }, 5000); return () => window.clearInterval(timer); }, [load]);
  useEffect(() => {
    if (!remoteControlEnabled) return;
    setRemoteControlNow(Math.floor(Date.now() / 1000));
    const timer = window.setInterval(() => setRemoteControlNow(Math.floor(Date.now() / 1000)), 1000);
    return () => window.clearInterval(timer);
  }, [remoteControlEnabled]);
  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | undefined;
    if (isTauriRuntime()) void import("@tauri-apps/api/event").then(async ({ listen }) => {
      const stop = await listen("claude-settings-requested", () => setModal("settings"));
      if (disposed) stop(); else unlisten = stop;
    }).catch((err) => { if (!disposed) setError(String(err)); });
    return () => { disposed = true; unlisten?.(); };
  }, []);
  const perform = useCallback(async <T,>(job: () => Promise<T>, message?: string): Promise<T | undefined> => {
    if (busyRef.current) return;
    busyRef.current = true; setBusy(true); setError(null); setNotice(null);
    try {
      await loadPromiseRef.current?.catch(() => {});
      const value = await job();
      // A cached-read failure belongs to loadError and clears on the next successful load.
      try { await load(); } catch { return undefined; }
      if (message) setNotice(message);
      return value;
    }
    catch (err) { setError(err instanceof Error ? err.message : String(err)); return undefined; }
    finally { busyRef.current = false; setBusy(false); }
  }, [load]);
  useEffect(() => {
    if (!loginPending) return;
    let stopped = false;
    let checking = false;
    const attempt = loginAttemptRef.current;
    const current = () => !stopped && loginAttemptRef.current === attempt;
    const stopTimers = () => { window.clearInterval(timer); window.clearTimeout(timeout); };
    const timer = window.setInterval(async () => {
      if (!current() || checking) return;
      checking = true;
      try {
        const result = await invokeClaude<{ pending?: boolean; account_id?: string } | null>("complete_login");
        if (!current() || result?.pending === true) return;
        if (!completedClaudeLoginAccountId(result, reauthenticateAccountId)) {
          throw new Error(reauthenticateAccountId ? "대상 계정의 인증 갱신을 확인하지 못했습니다. 취소한 뒤 같은 계정으로 다시 로그인해 주세요." : "로그인한 계정을 확인하지 못했습니다. 취소한 뒤 다시 로그인해 주세요.");
        }
        window.clearInterval(timer);
        loginSessionRef.current = false;
        // A snapshot requested before login finished may still contain the expired credentials' error.
        await loadPromiseRef.current?.catch(() => {});
        if (!current()) return;
        await load();
        if (!current()) return;
        stopTimers();
        setLoginPending(false); setModal(null); setContents(""); setReauthenticateAccountId(null);
        setNotice(reauthenticateAccountId ? "Claude 인증을 갱신했습니다." : "Claude 계정을 추가했습니다.");
      }
      catch (err) { if (current()) { stopTimers(); setError(err instanceof Error ? err.message : String(err)); setLoginPending(false); } }
      finally { checking = false; }
    }, 2000);
    const timeout = window.setTimeout(() => {
      if (!current()) return;
      loginAttemptRef.current += 1; stopTimers(); setLoginPending(false);
      setError("Claude 로그인 대기 시간이 지났습니다. 취소하거나 다시 시도해 주세요.");
    }, CLAUDE_LOGIN_TIMEOUT_MS);
    return () => { stopped = true; stopTimers(); };
  }, [loginPending, reauthenticateAccountId, load]);
  const closeModal = useCallback(() => {
    if (busyRef.current) return;
    loginAttemptRef.current += 1; setLoginPending(false);
    const close = () => { setModal(null); setReauthenticateAccountId(null); setPassword(""); setConfirmation(""); setContents(""); setConfigCopied(false); };
    if (!loginSessionRef.current) { close(); return; }
    busyRef.current = true; setBusy(true);
    void invokeClaude("cancel_login").then(() => { loginSessionRef.current = false; close(); })
      .catch((err) => setError(err instanceof Error ? err.message : String(err)))
      .finally(() => { busyRef.current = false; setBusy(false); });
  }, []);
  const save = useCallback(async (settings: ClaudeSettings) => { await invokeClaude("settings", { settings }); return true; }, []);
  const accounts = snapshot?.accounts ?? [];
  const activeAccount = accounts.find((account) => account.is_active);
  const otherAccounts = accounts.filter((account) => !account.is_active);
  const isAccountSearchEnabled = accounts.length >= 8;
  const normalizedQuery = isAccountSearchEnabled ? query.trim().toLowerCase() : "";
  const visibleOtherAccounts = useMemo(() => sortClaudeAccounts(otherAccounts, otherAccountsSort).filter((account) => matchesClaudeAccount(account, normalizedQuery)), [snapshot, otherAccountsSort, normalizedQuery]);
  const maskedAccounts = new Set(snapshot?.masked_account_ids ?? []);
  const reauthenticateAccount = accounts.find((account) => account.id === reauthenticateAccountId);
  const reauthenticateAccountLabel = reauthenticateAccount
    ? maskedAccounts.has(reauthenticateAccount.id) ? `숨김 계정 ${accounts.indexOf(reauthenticateAccount) + 1}` : reauthenticateAccount.name
    : "선택한 계정";
  const allMasked = accounts.length > 0 && accounts.every((account) => maskedAccounts.has(account.id));
  const runningIds = snapshot?.managed_running_ids ?? [];
  const processInfo = formatClaudeRunningStatus(snapshot, Boolean(loadError));
  const hasRunningProcesses = (processInfo?.count ?? 0) > 0 && !loadError;
  const appWindow = isTauriRuntime() ? getCurrentWindow() : null;
  const isMacOs = /(Mac|iPhone|iPod|iPad)/i.test(navigator.userAgent);
  const isRefreshing = operation?.action === "refresh";
  const isWarmingAll = operation?.action === "warmup_all";
  const isOpeningClaude = operation?.action === "open";
  const isForceClosingClaude = busy;
  const isCompletingForceClose = busy;
  const switchingId = busy ? operation?.accountId ?? "busy" : null;
  const autoWarmupAllEnabled = snapshot?.settings.warmup_all ?? false;
  const timedWarmupTimes = snapshot?.settings.timed_warmup_times ?? [];
  const timedWarmupEnabled = timedWarmupTimes.length > 0;
  const headerAutoWarmupLabel = autoWarmupAllEnabled ? "on" : "off";
  const timedWarmupLabel = timedWarmupEnabled ? timedWarmupTimes.length + " times" : "off";
  const themeMode = snapshot?.settings.theme ?? "system";
  const isExportingSlim = operation?.action === "export_slim";
  const isImportingSlim = operation?.action === "import_slim";
  const isExportingFull = operation?.action === "export_encrypted";
  const isImportingFull = operation?.action === "import_encrypted";
  const handleTitlebarDrag = (event: MouseEvent<HTMLDivElement>) => { if (event.button === 0) void appWindow?.startDragging().catch((err) => setError(String(err))); };
  const handleTitlebarDoubleClick = () => { void appWindow?.toggleMaximize().catch((err) => setError(String(err))); };
  const runAction = async <T,>(action: string, payload: Record<string, unknown> = {}, message?: string) => {
    if (busyRef.current) return undefined;
    setOperation({ action, accountId: typeof payload.accountId === "string" ? payload.accountId : undefined });
    try { return await perform(async () => ({ value: await invokeClaude<T>(action, payload) }), message); }
    finally { setOperation(null); }
  };
  const updateSettings = (patch: Partial<ClaudeSettings>) => {
    const current = snapshotRef.current;
    return current ? runAction("settings", { settings: { ...current.settings, ...patch } }) : Promise.resolve(undefined);
  };
  const handleRefresh = () => { void runAction("refresh", {}, "Usage refreshed successfully"); };
  const handleWarmupAll = async () => { const result = await runAction<WarmupSummary>("warmup_all"); if (result) setNotice(result.value.warmed_accounts + "/" + result.value.total_accounts + " 계정 워밍업 완료"); };
  const handleOpenClaudeApp = () => { if (activeAccount) void runAction("open", { accountId: activeAccount.id }, "선택한 Claude 계정으로 새 창을 열었습니다."); };
  const setAutoWarmupAllEnabled = () => { void updateSettings({ warmup_all: !autoWarmupAllEnabled }); };
  const setThemeMode = () => { void updateSettings({ theme: themeMode === "dark" ? "light" : "dark" }); };
  const handleRemoveTimedWarmupTime = (time: string) => { void updateSettings({ timed_warmup_times: timedWarmupTimes.filter((item) => item !== time) }); };
  const toggleTimedWarmup = (enabled: boolean) => {
    if (!enabled) pausedTimedWarmupTimes.current = [...timedWarmupTimes];
    void updateSettings({ timed_warmup_times: enabled ? pausedTimedWarmupTimes.current : [] });
  };
  const handleAddTimedWarmupTime = () => { if (/^([01]\d|2[0-3]):[0-5]\d$/.test(timedWarmupDraft)) void updateSettings({ timed_warmup_times: [...new Set([...timedWarmupTimes, timedWarmupDraft])].sort() }).then((result) => { if (result) setTimedWarmupDraft(""); }); };
  const setIsSettingsOpen = () => { setError(null); setModal("settings"); };
  const setIsAddModalOpen = () => { if (loginSessionRef.current) return; setError(null); setReauthenticateAccountId(null); setAccountName(""); setContents(""); setModal("add"); };
  const handleExportSlimText = () => showExport();
  const openImportSlimTextModal = () => { setError(null); setContents(""); setModal("import"); };
  const handleExportFullFile = () => openBackup("encrypted-export");
  const handleImportFullFile = () => openBackup("encrypted-import");
  const toggleMaskAll = () => { void mask(); };
  const openWindows = () => { setWindowAccountId(activeAccount?.id ?? accounts[0]?.id ?? ""); setModal("windows"); };
  useEffect(() => { if (!isAccountSearchEnabled) { setQuery(""); setIsAccountSearchOpen(false); } }, [isAccountSearchEnabled]);
  useEffect(() => {
    if (!notice) return;
    const timer = window.setTimeout(() => setNotice(null), 3000); return () => window.clearTimeout(timer);
  }, [notice]);
  useEffect(() => {
    if (!isActionsMenuOpen && !isNavMenuOpen && !isTimedWarmupOpen) return;
    const visible = () => !rootRef.current?.closest("[hidden]");
    const outside = (event: globalThis.MouseEvent) => { if (!visible()) return; if (!actionsMenuRef.current?.contains(event.target as Node)) setIsActionsMenuOpen(false); if (!navMenuRef.current?.contains(event.target as Node)) { setIsNavMenuOpen(false); setIsTimedWarmupOpen(false); } };
    const key = (event: KeyboardEvent) => { if (visible() && event.key === "Escape") { setIsActionsMenuOpen(false); setIsNavMenuOpen(false); setIsTimedWarmupOpen(false); } };
    document.addEventListener("mousedown", outside); document.addEventListener("keydown", key);
    return () => { document.removeEventListener("mousedown", outside); document.removeEventListener("keydown", key); };
  }, [isActionsMenuOpen, isNavMenuOpen, isTimedWarmupOpen]);
  useEffect(() => {
    if (!isTauriRuntime()) return;
    let disposed = false; let stop: (() => void) | undefined;
    const check = () => { void getCurrentWindow().isMaximized().then((value) => { if (!disposed) setIsWindowMaximized(value); }).catch(() => {}); };
    check(); void getCurrentWindow().onResized(check).then((unlisten) => { if (disposed) unlisten(); else stop = unlisten; }).catch(() => {});
    return () => { disposed = true; stop?.(); };
  }, []);
  const masked = snapshot?.masked_account_ids ?? [];
  const mask = async (id?: string) => { if (!snapshot) return; const ids = id ? masked.includes(id) ? masked.filter((x) => x !== id) : [...masked, id] : snapshot.accounts.every((account) => masked.includes(account.id)) ? [] : snapshot.accounts.map((a) => a.id); await runAction("mask", { ids }); };
  const showExport = () => void runAction<string>("export_slim").then((result) => { if (result) { setContents(result.value); setConfigCopied(false); setModal("export"); } });
  const openBackup = (next: "encrypted-export" | "encrypted-import") => { setPassword(""); setConfirmation(""); setError(null); setModal(next); };
  const backup = async () => {
    if (modal === "encrypted-export" && !validClaudeBackupPassword(password, confirmation)) return;
    const result = await perform(async () => modal === "encrypted-export" ? exportFullBackupFile(password, "claude") : importFullBackupFile(password, "claude"));
    if (result) { setModal(null); setPassword(""); setConfirmation(""); setNotice("Claude 암호화 백업 처리를 완료했습니다."); }
  };
  const startLogin = async (targetAccountId: string | null) => {
    if (busyRef.current || loginPending) return;
    if (!snapshotRef.current?.cli_available) { setError("공식 Claude Code CLI를 먼저 설치해 주세요."); return; }
    if (targetAccountId && !snapshotRef.current.accounts.some((account) => account.id === targetAccountId)) { setError("대상 계정을 찾을 수 없습니다. 계정 목록을 새로고침해 주세요."); return; }
    busyRef.current = true; setBusy(true); setError(null); setNotice(null);
    const attempt = ++loginAttemptRef.current;
    try {
      await loadPromiseRef.current?.catch(() => {});
      if (loginSessionRef.current) { await invokeClaude("cancel_login"); loginSessionRef.current = false; }
      await invokeClaude(targetAccountId ? "start_relogin" : "start_login", targetAccountId ? { accountId: targetAccountId } : { name: accountName || "현재 Claude 계정" });
      loginSessionRef.current = true;
      if (loginAttemptRef.current === attempt) setLoginPending(true);
    }
    catch (err) { setError(err instanceof Error ? err.message : String(err)); }
    finally { busyRef.current = false; setBusy(false); }
  };
  const add = async (action: "import_current" | "start_login" | "import_credentials") => {
    if (action === "start_login") { await startLogin(null); return; }
    if (loginSessionRef.current) return;
    const result = await perform(() => invokeClaude(action, { name: accountName || "현재 Claude 계정", contents }));
    if (result !== undefined) { setModal(null); setContents(""); setNotice("Claude 계정을 추가했습니다."); }
  };
  const renderCard = (account: ClaudeSnapshot["accounts"][number]) => {
    const requiresLogin = claudeAccountRequiresReauthentication(account);
    const status = snapshot?.warmup_status?.accounts?.[account.id];
    const warming = isWarmingAll || (operation?.action === "warmup" && operation.accountId === account.id) || status?.phase === "running";
    const autoEnabled = Boolean(snapshot?.settings.warmup_all || snapshot?.settings.warmup_account_ids.includes(account.id));
    return <AccountCard key={account.id} provider="claude" usageObservedAt={account.usage_observed_at} account={{ ...claudeAccountForCard(account), usageLoading: account.usageLoading || (isRefreshing && (!operation.accountId || operation.accountId === account.id)) }}
      switching={operation?.action === "switch" && operation.accountId === account.id} switchDisabled={busy || loginPending || requiresLogin} warmingUp={warming} masked={masked.includes(account.id)}
      onReauthenticate={requiresLogin ? () => { if (busyRef.current || loginSessionRef.current) return; setError(null); setNotice(null); setReauthenticateAccountId(account.id); setModal("reauthenticate"); } : undefined}
      reauthenticateDisabled={busy || loginPending || !snapshot?.cli_available}
      onSwitch={() => { if (requiresLogin) { setError(CLAUDE_REAUTHENTICATION_MESSAGE); return; } if (loginSessionRef.current) { setError("Claude 로그인을 완료하거나 취소한 뒤 계정을 전환해 주세요."); return; } void runAction<{ message: string }>("switch", { accountId: account.id }).then((result) => { if (result) setNotice(result.value.message); }); }}
      onWarmup={async () => { if (requiresLogin) { setError(CLAUDE_REAUTHENTICATION_MESSAGE); return; } if (loginSessionRef.current) { setError("Claude 로그인을 완료하거나 취소한 뒤 워밍업해 주세요."); return; } await runAction("warmup", { accountId: account.id }, "Claude 워밍업 완료"); }} onDelete={() => setDeleteId(account.id)}
      onRefresh={async () => { if (!await runAction("refresh", { accountId: account.id })) throw new Error("Claude 사용량 새로고침 실패"); }}
      onRename={async (newName) => { if (!await runAction("rename", { accountId: account.id, newName })) throw new Error("Claude 계정 이름 변경 실패"); }} onToggleMask={() => { void mask(account.id); }}
      autoWarmupEnabled={autoEnabled} autoWarmupManagedByAll={snapshot?.settings.warmup_all} autoWarmupLabel={warming ? "Warming..." : autoEnabled ? "on" : "off"}
      onToggleAutoWarmup={() => { if (!snapshot) return; const ids = snapshot.settings.warmup_account_ids; void updateSettings({ warmup_account_ids: ids.includes(account.id) ? ids.filter((id) => id !== account.id) : [...ids, account.id] }); }} />;
  };
  return <div ref={rootRef} className="min-h-screen bg-gray-50 text-gray-900 dark:bg-gray-950 dark:text-gray-100">
      <header className="sticky top-0 z-40 border-b border-gray-200 bg-white dark:border-gray-800 dark:bg-gray-900">
        <div className="flex h-9 items-center bg-white px-3 dark:bg-gray-900">
          <div
            onMouseDown={handleTitlebarDrag}
            onDoubleClick={handleTitlebarDoubleClick}
            className={`h-full flex-1 select-none cursor-default ${isMacOs ? "ml-18 mr-2" : "mr-3"}`}
          />
          {isTauriRuntime() && !isMacOs && (
            <div className="flex items-center gap-1">
              <button
                onClick={() => {
                  void appWindow?.minimize();
                }}
                className="flex h-8 w-8 items-center justify-center rounded-md text-gray-500 transition-colors hover:bg-gray-100 hover:text-gray-900 dark:text-gray-400 dark:hover:bg-gray-800 dark:hover:text-gray-100"
                title="Minimize"
              >
                <svg className="h-4 w-4" viewBox="0 0 24 24" fill="none" stroke="currentColor">
                  <path d="M5 12h14" strokeWidth="2" strokeLinecap="round" />
                </svg>
              </button>
              <button
                onClick={() => {
                  void appWindow?.toggleMaximize();
                }}
                className="flex h-8 w-8 items-center justify-center rounded-md text-gray-500 transition-colors hover:bg-gray-100 hover:text-gray-900 dark:text-gray-400 dark:hover:bg-gray-800 dark:hover:text-gray-100"
                title={isWindowMaximized ? "Restore" : "Maximize"}
              >
                {isWindowMaximized ? (
                  <svg className="h-4 w-4" viewBox="0 0 24 24" fill="none" stroke="currentColor">
                    <path d="M9 9h10v10H9z" strokeWidth="2" />
                    <path d="M5 15V5h10" strokeWidth="2" strokeLinecap="round" />
                  </svg>
                ) : (
                  <svg className="h-4 w-4" viewBox="0 0 24 24" fill="none" stroke="currentColor">
                    <rect x="5" y="5" width="14" height="14" strokeWidth="2" />
                  </svg>
                )}
              </button>
              <button
                onClick={() => {
                  void appWindow?.close();
                }}
                className="flex h-8 w-8 items-center justify-center rounded-md text-gray-500 transition-colors hover:bg-red-500 hover:text-white dark:text-gray-400 dark:hover:bg-red-500 dark:hover:text-white"
                title="Close"
              >
                <svg className="h-4 w-4" viewBox="0 0 24 24" fill="none" stroke="currentColor">
                  <path d="M6 6l12 12M18 6L6 18" strokeWidth="2" strokeLinecap="round" />
                </svg>
              </button>
            </div>
          )}
        </div>

        <div className="max-w-5xl mx-auto px-6 py-4">
          <div className="mb-3 rounded-lg border border-gray-200 dark:border-gray-700 px-3 py-2 text-sm" role="status" title={snapshot?.monitor_status.message}>
            <span>인증 파일 계정: {activeAccount ? (maskedAccounts.has(activeAccount.id) ? "숨김" : activeAccount.name) : "확인할 수 없음"}</span>
            {activeAccount?.auth_sync_pending && <span className="ml-3 text-amber-600">갱신된 인증 정보 적용 대기 · 다시 적용을 눌러 주세요</span>}
            <span className="ml-3 text-gray-500">{hasRunningProcesses ? "Claude 실행 중 · 앱 내부 로그인 계정 미확인" : "Claude 실행 여부와 인증 파일을 각각 확인합니다."}</span>
            {(isTauriRuntime() || snapshot?.monitor_status.phase === "failed" || snapshot?.monitor_status.phase === "waiting") && <div className={`mt-1 text-xs ${snapshot?.monitor_status.phase === "failed" || snapshot?.monitor_status.phase === "waiting" ? "text-amber-600 dark:text-amber-400" : "text-gray-500 dark:text-gray-400"}`} role="status">{snapshot?.monitor_status.message || (loading ? "Claude 상태를 불러오는 중…" : "Claude 자동 전환 꺼짐")}</div>}
          </div>
          <div className="grid grid-cols-1 gap-3 md:grid-cols-[minmax(0,1fr)_max-content] md:items-center md:gap-4">
            {remoteControlEnabled && <div className="md:col-span-2 text-xs text-gray-500 dark:text-gray-400" role="status">
              <p>{remoteControl.summary}</p>
              {loadError && <p>RC 상태 새로고침에 실패했습니다. 표시된 수치는 마지막 보고값입니다.</p>}
              {remoteControl.details.map((detail, index) => <p key={index}>{detail}</p>)}
              {!loadError && remoteControl.retryNotice && <p>{remoteControl.retryNotice}</p>}
            </div>}
            <div className="flex items-center gap-3 min-w-0 flex-1">
              <div className="min-w-0">
                <div className="flex items-center gap-2 flex-wrap">
                  <h1 className="text-xl font-bold text-gray-900 dark:text-gray-100 tracking-tight">
                    Claude Code Switcher
                  </h1>
                  {processInfo && (
                    <div className="inline-flex items-center gap-1">
                      <span
                        className={`inline-flex items-center gap-1 px-2 py-0.5 rounded-md text-xs border ${hasRunningProcesses || !processInfo.complete
                            ? "bg-amber-50 text-amber-700 border-amber-200 dark:bg-amber-900/30 dark:text-amber-300 dark:border-amber-700"
                            : "bg-green-50 text-green-700 border-green-200 dark:bg-green-900/30 dark:text-green-300 dark:border-green-700"
                          }`}
                      >
                        <span
                          className={`inline-block w-1.5 h-1.5 rounded-full ${hasRunningProcesses || !processInfo.complete ? "bg-amber-500" : "bg-green-500"
                            }`}
                        ></span>
                        <span>
                          {processInfo.label}
                        </span>
                      </span>
                      {runningIds.length > 0 && (
                        <button
                          onClick={() => {
                            
                            openWindows();
                          }}
                          disabled={isForceClosingClaude}
                          className="inline-flex items-center rounded-md border border-red-200 bg-red-50 px-2 py-0.5 text-xs font-medium text-red-700 transition-colors hover:bg-red-100 disabled:opacity-50 dark:border-red-800 dark:bg-red-900/20 dark:text-red-300 dark:hover:bg-red-900/30"
                          title="Close Switcher-managed Claude windows"
                        >
                          Close
                        </button>
                      )}
                    </div>
                  )}
                  {isTauriRuntime() && processInfo?.complete && processInfo.count === 0 && (
                    <button
                      onClick={handleOpenClaudeApp}
                      disabled={isOpeningClaude || isCompletingForceClose || switchingId !== null}
                      className="inline-flex items-center rounded-md border border-green-200 bg-green-50 px-2 py-0.5 text-xs font-medium text-green-700 transition-colors hover:bg-green-100 disabled:opacity-50 dark:border-green-800 dark:bg-green-900/20 dark:text-green-300 dark:hover:bg-green-900/30"
                      title="Open Claude app"
                    >
                      {isOpeningClaude ? "Opening..." : "Open Claude"}
                    </button>
                  )}
                </div>
              </div>
            </div>

            <div className="flex flex-wrap items-center gap-2 shrink-0 md:ml-4 md:w-max md:flex-nowrap md:justify-end">
              <button
                onClick={toggleMaskAll}
                className="flex h-10 w-10 items-center justify-center rounded-lg bg-gray-100 text-gray-700 transition-colors hover:bg-gray-200 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700 shrink-0"
                title={allMasked ? "Show all account names and emails" : "Hide all account names and emails"}
              >
                {allMasked ? (
                  <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                    <path
                      strokeLinecap="round"
                      strokeLinejoin="round"
                      strokeWidth={2}
                      d="M13.875 18.825A10.05 10.05 0 0112 19c-4.478 0-8.268-2.943-9.543-7a9.97 9.97 0 011.563-3.029m5.858.908a3 3 0 114.243 4.243M9.878 9.878l4.242 4.242M9.88 9.88l-3.29-3.29m7.532 7.532l3.29 3.29M3 3l3.59 3.59m0 0A9.953 9.953 0 0112 5c4.478 0 8.268 2.943 9.543 7a10.025 10.025 0 01-4.132 5.411m0 0L21 21"
                    />
                  </svg>
                ) : (
                  <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                    <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M15 12a3 3 0 11-6 0 3 3 0 016 0z" />
                    <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M2.458 12C3.732 7.943 7.523 5 12 5c4.478 0 8.268 2.943 9.542 7-1.274 4.057-5.064 7-9.542 7-4.477 0-8.268-2.943-9.542-7z" />
                  </svg>
                )}
              </button>
              <button
                onClick={handleRefresh}
                disabled={isRefreshing}
                className="flex h-10 w-10 items-center justify-center rounded-lg bg-gray-100 text-gray-700 transition-colors hover:bg-gray-200 disabled:opacity-50 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700 shrink-0"
                title={isRefreshing ? "Refreshing all usage" : "Refresh all usage"}
              >
                <span className={isRefreshing ? "animate-spin inline-block" : ""}>↻</span>
              </button>
              <button
                onClick={() => void handleWarmupAll()}
                disabled={isWarmingAll || accounts.length === 0}
                className={`flex h-10 w-10 items-center justify-center rounded-lg transition-colors disabled:opacity-50 shrink-0 ${
                  isWarmingAll
                    ? "bg-amber-100 text-amber-500 dark:bg-amber-900/30 dark:text-amber-300"
                    : "bg-amber-50 text-amber-700 hover:bg-amber-100 dark:bg-amber-900/20 dark:text-amber-300 dark:hover:bg-amber-900/40"
                }`}
                title={isWarmingAll ? "Warming up all accounts" : "Warm up all accounts"}
              >
                <span className={isWarmingAll ? "animate-pulse" : ""}>⚡</span>
              </button>
              {isAccountSearchEnabled && (
                <button
                  onClick={() => {
                    if (isAccountSearchOpen) {
                      setQuery("");
                    }
                    setIsAccountSearchOpen((prev) => !prev);
                  }}
                  className={`flex h-10 w-10 items-center justify-center rounded-lg transition-colors shrink-0 ${
                    isAccountSearchOpen
                      ? "bg-gray-900 text-white hover:bg-gray-800 dark:bg-black dark:text-white dark:hover:bg-neutral-900"
                      : "bg-gray-100 text-gray-700 hover:bg-gray-200 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"
                  }`}
                  title={isAccountSearchOpen ? "Hide account search" : "Search accounts"}
                >
                  <svg className="h-4 w-4" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
                    <circle cx="11" cy="11" r="7" />
                    <path d="m20 20-3.5-3.5" strokeLinecap="round" />
                  </svg>
                </button>
              )}

              <div className="relative" ref={navMenuRef}>
                <button
                  onClick={() => {
                    setIsTimedWarmupOpen(false);
                    setIsActionsMenuOpen(false); setIsNavMenuOpen((prev) => !prev);
                  }}
                  className={`flex h-10 w-10 items-center justify-center rounded-lg transition-colors shrink-0 ${
                    isNavMenuOpen
                      ? "bg-gray-900 text-white hover:bg-gray-800 dark:bg-black dark:text-white dark:hover:bg-neutral-900"
                      : "bg-gray-100 text-gray-700 hover:bg-gray-200 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"
                  }`}
                  title="Menu"
                >
                  <svg className="h-4 w-4" viewBox="0 0 24 24" fill="currentColor">
                    <circle cx="12" cy="5" r="1.6" />
                    <circle cx="12" cy="12" r="1.6" />
                    <circle cx="12" cy="19" r="1.6" />
                  </svg>
                </button>
                {isNavMenuOpen && (
                  <div className="absolute right-0 z-50 mt-2 w-64 rounded-xl border border-gray-200 bg-white p-2 text-gray-700 shadow-xl dark:border-neutral-800 dark:bg-black dark:text-white">
                    <button
                      onClick={() => {
                        setIsNavMenuOpen(false);
                        setIsSettingsOpen();
                      }}
                      className="flex w-full items-center justify-between gap-2 rounded-lg px-3 py-2 text-left text-sm transition-colors hover:bg-gray-100 dark:text-white dark:hover:bg-neutral-900"
                    >
                      Settings
                    </button>
                    <button
                      onClick={() => {
                        setIsNavMenuOpen(false);
                        setAutoWarmupAllEnabled();
                      }}
                      disabled={accounts.length === 0}
                      className="flex w-full items-center justify-between gap-2 rounded-lg px-3 py-2 text-left text-sm transition-colors hover:bg-gray-100 disabled:opacity-50 dark:text-white dark:hover:bg-neutral-900"
                    >
                      <span>Auto Warm Up</span>
                      <span
                        className={`rounded-md px-1.5 py-0.5 text-[11px] font-medium ${
                          autoWarmupAllEnabled
                            ? "bg-emerald-50 text-emerald-700 dark:bg-emerald-900/30 dark:text-emerald-300"
                            : "bg-gray-100 text-gray-500 dark:bg-gray-800 dark:text-gray-400"
                        }`}
                      >
                        {headerAutoWarmupLabel}
                      </span>
                    </button>
                    <button
                      onClick={() => {
                        setIsNavMenuOpen(false);
                        setIsTimedWarmupOpen((prev) => !prev);
                      }}
                      className="flex w-full items-center justify-between gap-2 rounded-lg px-3 py-2 text-left text-sm transition-colors hover:bg-gray-100 dark:text-white dark:hover:bg-neutral-900"
                    >
                      <span>Timer</span>
                      <span
                        className={`rounded-md px-1.5 py-0.5 text-[11px] font-medium ${
                          timedWarmupEnabled
                            ? "bg-emerald-50 text-emerald-700 dark:bg-emerald-900/30 dark:text-emerald-300"
                            : "bg-gray-100 text-gray-500 dark:bg-gray-800 dark:text-gray-400"
                        }`}
                      >
                        {timedWarmupLabel}
                      </span>
                    </button>
                    <button
                      onClick={() => {
                        setIsNavMenuOpen(false);
                        setThemeMode();
                      }}
                      className="flex w-full items-center justify-between gap-2 rounded-lg px-3 py-2 text-left text-sm transition-colors hover:bg-gray-100 dark:text-white dark:hover:bg-neutral-900"
                    >
                      <span>Appearance</span>
                      <span className="text-[11px] text-gray-400 dark:text-gray-500">
                        {themeMode === "dark" ? "☾ Dark" : themeMode === "light" ? "☀ Light" : "System"}
                      </span>
                    </button>
                    <div className="my-1 border-t border-gray-100 dark:border-neutral-800" />
                    <button className="flex w-full items-center justify-between gap-2 rounded-lg px-3 py-2 text-left text-sm transition-colors hover:bg-gray-100 disabled:opacity-50 dark:text-white dark:hover:bg-neutral-900" disabled={busy || !accounts.length} onClick={() => { setIsNavMenuOpen(false); void runAction<{ switched: boolean }>("auto_switch").then((result) => { if (result) setNotice(result.value.switched ? "Claude 자동 선택과 인증 적용을 완료했습니다." : "최근 사용량이 확인된 전환 후보를 기다립니다."); }); }}>여유 계정으로 전환</button>
                    <button className="flex w-full items-center justify-between gap-2 rounded-lg px-3 py-2 text-left text-sm transition-colors hover:bg-gray-100 dark:text-white dark:hover:bg-neutral-900" disabled={!accounts.length} onClick={() => { setIsNavMenuOpen(false); openWindows(); }}>Claude windows</button>
                  </div>
                )}
                {isTimedWarmupOpen && (
                  <div className="absolute right-0 z-20 mt-2 w-64 rounded-lg border border-gray-200 bg-white p-3 shadow-lg dark:border-gray-700 dark:bg-gray-900">
                    <label className="flex items-center justify-between text-sm font-medium text-gray-800 dark:text-gray-100">
                      <span>Timed warm-up</span>
                      <input type="checkbox" checked={timedWarmupEnabled} disabled={busy || (!timedWarmupEnabled && !pausedTimedWarmupTimes.current.length)} onChange={(event) => toggleTimedWarmup(event.target.checked)} className="h-4 w-4 accent-emerald-600" title={!timedWarmupEnabled && !pausedTimedWarmupTimes.current.length ? "Add a time to enable timed warm-up" : undefined} />
                    </label>
                    <div className="mt-3 space-y-1">
                      {timedWarmupTimes.length === 0 ? (
                        <p className="text-xs italic text-gray-400 dark:text-gray-500">
                          No times added yet.
                        </p>
                      ) : (
                        timedWarmupTimes.map((time) => (
                          <div
                            key={time}
                            className="flex items-center justify-between rounded-md bg-gray-50 px-2 py-1 text-sm dark:bg-gray-800"
                          >
                            <span className="font-mono text-gray-800 dark:text-gray-100">
                              {time}
                            </span>
                            <button
                              onClick={() => handleRemoveTimedWarmupTime(time)}
                              className="text-gray-400 transition-colors hover:text-red-500"
                              title={`Remove ${time}`}
                            >
                              ✕
                            </button>
                          </div>
                        ))
                      )}
                    </div>

                    <div className="mt-3 flex items-center gap-2">
                      <input
                        type="time"
                        value={timedWarmupDraft}
                        onChange={(e) => setTimedWarmupDraft(e.target.value)}
                        onKeyDown={(e) => {
                          if (e.key === "Enter") handleAddTimedWarmupTime();
                        }}
                        className="h-8 flex-1 rounded-md border border-gray-300 bg-white px-2 text-sm text-gray-800 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-100"
                      />
                      <button
                        onClick={handleAddTimedWarmupTime}
                        disabled={!timedWarmupDraft}
                        className="h-8 rounded-md bg-gray-900 px-3 text-xs font-semibold text-white transition-colors hover:bg-gray-800 disabled:opacity-50 dark:bg-black dark:hover:bg-neutral-900"
                      >
                        Add
                      </button>
                    </div>
                  </div>
                )}
              </div>
              <div className="relative" ref={actionsMenuRef}>
                <button
                  onClick={() => { setIsNavMenuOpen(false); setIsTimedWarmupOpen(false); setIsActionsMenuOpen((prev) => !prev); }}
                  className="h-10 px-4 py-2 text-sm font-medium rounded-lg bg-gray-900 text-white transition-colors hover:bg-gray-800 dark:bg-black dark:hover:bg-neutral-900 shrink-0 whitespace-nowrap"
                >
                  Account ▾
                </button>
                {isActionsMenuOpen && (
                  <div className="absolute right-0 z-50 mt-2 w-56 rounded-xl border border-gray-200 bg-white p-2 text-gray-700 shadow-xl dark:border-neutral-800 dark:bg-black dark:text-white">
                    <button
                      onClick={() => {
                        setIsActionsMenuOpen(false);
                        setIsAddModalOpen();
                      }}
                      className="w-full rounded-lg px-3 py-2 text-left text-sm transition-colors hover:bg-gray-100 dark:text-white dark:hover:bg-neutral-900"
                    >
                      + Add Account
                    </button>
                    <button
                      onClick={() => {
                        setIsActionsMenuOpen(false);
                        void handleExportSlimText();
                      }}
                      disabled={isExportingSlim}
                      className="w-full rounded-lg px-3 py-2 text-left text-sm transition-colors hover:bg-gray-100 disabled:opacity-50 dark:text-white dark:hover:bg-neutral-900"
                    >
                      {isExportingSlim ? "Exporting..." : "Export Slim Text"}
                    </button>
                    <button
                      onClick={() => {
                        setIsActionsMenuOpen(false);
                        openImportSlimTextModal();
                      }}
                      disabled={isImportingSlim}
                      className="w-full rounded-lg px-3 py-2 text-left text-sm transition-colors hover:bg-gray-100 disabled:opacity-50 dark:text-white dark:hover:bg-neutral-900"
                    >
                      {isImportingSlim ? "Importing..." : "Import Slim Text"}
                    </button>
                    <button
                      onClick={() => {
                        setIsActionsMenuOpen(false);
                        void handleExportFullFile();
                      }}
                      disabled={isExportingFull}
                      className="w-full rounded-lg px-3 py-2 text-left text-sm transition-colors hover:bg-gray-100 disabled:opacity-50 dark:text-white dark:hover:bg-neutral-900"
                    >
                      {isExportingFull ? "Exporting..." : "Export Full Encrypted File"}
                    </button>
                    <button
                      onClick={() => {
                        setIsActionsMenuOpen(false);
                        void handleImportFullFile();
                      }}
                      disabled={isImportingFull}
                      className="w-full rounded-lg px-3 py-2 text-left text-sm transition-colors hover:bg-gray-100 disabled:opacity-50 dark:text-white dark:hover:bg-neutral-900"
                    >
                      {isImportingFull ? "Importing..." : "Import Full Encrypted File"}
                    </button>
                  </div>
                )}
              </div>
            </div>
          </div>
        </div>
      </header>

    <main className="max-w-5xl mx-auto px-6 pt-4 pb-8">
      {snapshot && (error || loadError) && <p role="alert" className="mb-4 text-sm text-red-600 dark:text-red-300">{error || loadError}</p>}
      {snapshot && !snapshot.cli_available && <p role="alert" className="mb-4 text-sm text-amber-600 dark:text-amber-300">공식 Claude Code CLI를 먼저 설치해 주세요.</p>}
      {loading && !snapshot ? <div className="flex flex-col items-center justify-center py-20"><div className="animate-spin h-10 w-10 border-2 border-gray-900 dark:border-gray-100 border-t-transparent rounded-full mb-4" /><p className="text-gray-500 dark:text-gray-400">Loading accounts...</p></div>
      : !snapshot && loadError ? <div className="text-center py-20" role="alert"><div className="text-red-600 dark:text-red-300 mb-2">Failed to load accounts</div><p className="text-sm text-gray-500 dark:text-gray-400">{loadError}</p></div>
      : !accounts.length ? <div className="text-center py-20"><div className="h-16 w-16 rounded-2xl bg-gray-100 dark:bg-gray-800 flex items-center justify-center mx-auto mb-4"><span className="text-3xl">👤</span></div><h2 className="text-xl font-semibold text-gray-900 dark:text-gray-100 mb-2">No accounts yet</h2><p className="text-gray-500 dark:text-gray-400 mb-6">Add your first Claude account to get started</p><button onClick={setIsAddModalOpen} className="px-6 py-3 text-sm font-medium rounded-lg bg-gray-900 hover:bg-gray-800 dark:bg-gray-100 dark:hover:bg-gray-200 text-white dark:text-gray-900 transition-colors">Add Account</button></div>
      : <div className="space-y-4">
        {normalizedQuery && (!activeAccount || !matchesClaudeAccount(activeAccount, normalizedQuery)) && !visibleOtherAccounts.length && <div className="rounded-2xl border border-dashed border-gray-300 px-6 py-12 text-center dark:border-gray-700"><h2 className="text-base font-semibold text-gray-900 dark:text-gray-100">No matching accounts</h2><p className="mt-1 text-sm text-gray-500 dark:text-gray-400">Try a different account name or email address.</p></div>}
        {isAccountSearchEnabled && isAccountSearchOpen && <div className="relative w-full"><span className="pointer-events-none absolute inset-y-0 left-3 flex items-center text-gray-400 dark:text-gray-500"><svg className="h-4 w-4" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2"><circle cx="11" cy="11" r="7" /><path d="m20 20-3.5-3.5" strokeLinecap="round" /></svg></span><input type="search" value={query} onChange={(event) => setQuery(event.target.value)} placeholder="Search accounts by name or email" aria-label="Search accounts" autoFocus className="w-full rounded-xl border border-gray-300 bg-white py-2.5 pl-10 pr-10 text-sm text-gray-900 shadow-sm transition-colors placeholder:text-gray-400 focus:border-gray-400 focus:outline-none focus:ring-2 focus:ring-gray-200 dark:border-gray-700 dark:bg-gray-900 dark:text-gray-100 dark:placeholder:text-gray-500 dark:focus:border-gray-600 dark:focus:ring-gray-800" />{query && <button onClick={() => setQuery("")} aria-label="Clear account search" className="absolute inset-y-0 right-2 flex items-center px-2 text-gray-400 transition-colors hover:text-gray-700 dark:text-gray-500 dark:hover:text-gray-200">✕</button>}</div>}
        {activeAccount && matchesClaudeAccount(activeAccount, normalizedQuery) && <section><h2 className="text-sm font-medium text-gray-500 dark:text-gray-400 uppercase tracking-wider mb-4">인증 파일에 적용된 계정</h2>{renderCard(activeAccount)}</section>}
        {visibleOtherAccounts.length > 0 && <section><div className="flex items-center justify-between gap-3 mb-4"><h2 className="text-sm font-medium text-gray-500 dark:text-gray-400 uppercase tracking-wider">Other Accounts ({normalizedQuery ? visibleOtherAccounts.length + " of " + otherAccounts.length : otherAccounts.length})</h2><div className="flex items-center gap-2"><label htmlFor="claude-other-accounts-sort" className="text-xs text-gray-500 dark:text-gray-400">Sort</label><div className="relative"><select id="claude-other-accounts-sort" value={otherAccountsSort} disabled={busy} onChange={(event) => { const next = event.target.value; if (!isClaudeAccountSort(next)) return; setOtherAccountsSort(next); try { localStorage.setItem(CLAUDE_SORT_KEY, next); } catch {} void updateSettings({ sort_by: backendClaudeSort(next) }); }} className="appearance-none font-sans text-xs sm:text-sm font-medium pl-3 pr-9 py-2 rounded-xl border border-gray-300 dark:border-gray-700 bg-gradient-to-b from-white to-gray-50 dark:from-gray-900 dark:to-gray-800 text-gray-700 dark:text-gray-200 shadow-sm hover:border-gray-400 dark:hover:border-gray-600 hover:shadow focus:outline-none focus:ring-2 focus:ring-gray-300 dark:focus:ring-gray-600 focus:border-gray-400 dark:focus:border-gray-600 transition-all">{claudeSortOptions.map(([value, label]) => <option key={value} value={value} disabled={value.startsWith("subscription") && !otherAccounts.some((account) => account.subscription_expires_at)}>{label}</option>)}</select><span className="pointer-events-none absolute inset-y-0 right-3 flex items-center text-gray-500 dark:text-gray-400"><svg className="h-4 w-4" viewBox="0 0 20 20" fill="none" stroke="currentColor" strokeWidth="2"><path d="M6 8l4 4 4-4" strokeLinecap="round" strokeLinejoin="round" /></svg></span></div></div></div><div className="grid grid-cols-1 md:grid-cols-2 gap-4">{visibleOtherAccounts.map(renderCard)}</div></section>}
      </div>}
    </main>
    {notice && <div role="status" className="fixed bottom-6 left-1/2 -translate-x-1/2 px-4 py-3 bg-green-600 text-white rounded-lg shadow-lg text-sm flex items-center gap-2"><span>✓</span>{notice}</div>}
    {modal === "settings" && snapshot && <ClaudeSettingsDialog initial={snapshot.settings} busy={busy} error={error} onClose={closeModal} onSave={async (settings) => { const result = await perform(async () => { await save(settings); return true; }); if (result) closeModal(); }} />}
    {modal === "reauthenticate" && <Dialog title="Claude 다시 로그인" onClose={closeModal} busy={busy} error={error} footer={<button className={button} disabled={busy || loginPending || !snapshot?.cli_available || !reauthenticateAccount} onClick={() => void startLogin(reauthenticateAccountId)}>{loginPending ? "로그인 완료 대기 중…" : "공식 Claude Code에서 로그인"}</button>}>
      <p className="text-sm">인증을 갱신할 계정: <strong className="break-all">{reauthenticateAccountLabel}</strong></p>
      {reauthenticateAccount?.email && !maskedAccounts.has(reauthenticateAccount.id) && <p className="break-all text-sm text-gray-500 dark:text-gray-400">{reauthenticateAccount.email}</p>}
      <p className="text-sm">공식 Claude Code 로그인 창과 브라우저에서 위 계정으로 직접 로그인해 주세요. 다른 계정으로 로그인하면 갱신할 수 없습니다.</p>
      <p className="text-xs text-gray-500 dark:text-gray-400">이 계정의 인증만 갱신합니다. 현재 적용된 계정은 바뀌지 않습니다.</p>
      {loginPending && <p role="status" className="text-sm">공식 로그인 완료를 기다리고 있습니다. 대상 계정 확인과 목록 새로고침이 끝나면 결과를 알려드립니다.</p>}
    </Dialog>}
    {modal === "add" && <Dialog title="Claude 계정 추가" onClose={closeModal} busy={busy} error={error}><div className="space-y-4 text-sm"><label className="block">계정 이름<input className={field} value={accountName} onChange={(event) => setAccountName(event.target.value)} maxLength={100} /></label><div className="flex flex-wrap gap-2"><button className={button} disabled={busy || loginPending} onClick={() => void add("import_current")}>현재 로그인 계정 가져오기</button><button className={button} disabled={busy || loginPending || !accountName.trim()} onClick={() => void add("start_login")}>다른 계정으로 로그인</button></div>{loginPending && <p role="status">공식 Claude 로그인 창과 브라우저에서 로그인하세요. 완료되면 자동으로 계정을 추가합니다.</p>}<label className="block">Claude 인증 JSON 파일<input className={field} type="file" accept=".json,application/json" disabled={busy || loginPending} onChange={(event) => { const file = event.target.files?.[0]; if (file) { if (file.size > 8 * 1024 * 1024) setError("인증 파일이 너무 큽니다."); else void file.text().then(setContents).catch((err) => setError(String(err))); } }} /></label><button className={button} disabled={busy || loginPending || !contents || !accountName.trim()} onClick={() => void add("import_credentials")}>파일에서 추가</button><p className="text-xs text-gray-500">Claude Code의 .credentials.json 또는 Claude 내보내기 파일을 사용하세요. 계정 추가는 Codex 로그인에 영향을 주지 않습니다.</p></div></Dialog>}
    {(modal === "export" || modal === "import") && <Dialog title={modal === "export" ? "Claude 계정 텍스트 내보내기" : "Claude 계정 텍스트 가져오기"} onClose={closeModal} busy={busy} error={error}><p className="mb-3 text-sm text-amber-700 dark:text-amber-300">이 텍스트에는 Claude 로그인 정보가 들어 있습니다. 개인 백업으로 보관하세요.</p><textarea className={`${field} min-h-48 font-mono text-xs`} value={contents} readOnly={modal === "export"} onChange={(event) => setContents(event.target.value)} maxLength={8 * 1024 * 1024} aria-label="Claude 계정 백업 텍스트" /><div className="mt-3 flex justify-end">{modal === "export" ? <><button className={button} onClick={() => { void navigator.clipboard.writeText(contents).then(() => setConfigCopied(true)).catch((err) => setError(String(err))); }}>{configCopied ? "Copied!" : "Copy Text"}</button><button className={button} onClick={() => { const bytes = new Blob([contents], { type: "text/plain" }); const url = URL.createObjectURL(bytes); const a = document.createElement("a"); a.href = url; a.download = "claude-accounts.txt"; a.click(); window.setTimeout(() => URL.revokeObjectURL(url), 1000); }}>파일 저장</button></> : <button className={button} disabled={busy || !contents} onClick={() => void perform(async () => { const summary = await invokeClaude<ImportAccountsSummary>("import_slim", { contents }); setNotice(`${summary.imported_count}개 Claude 계정 추가, ${summary.skipped_count}개 갱신/중복`); setModal(null); setContents(""); })}>가져오기</button>}</div></Dialog>}
    {(modal === "encrypted-export" || modal === "encrypted-import") && <Dialog title={modal === "encrypted-export" ? "Claude 암호화 백업" : "Claude 암호화 복원"} onClose={closeModal} busy={busy} error={error}><form onSubmit={(event) => { event.preventDefault(); void backup(); }}><label>백업 비밀번호<input className={field} type="password" autoComplete="new-password" value={password} onChange={(event) => setPassword(event.target.value)} disabled={busy} /></label>{modal === "encrypted-export" && <label className="block mt-3">암호 확인<input className={field} type="password" autoComplete="new-password" value={confirmation} onChange={(event) => setConfirmation(event.target.value)} disabled={busy} /></label>}<p className="my-3 text-xs text-gray-500">Claude 계정과 Claude 설정만 처리합니다. 내보내기는 비밀번호 12자 이상이 필요합니다.</p><button className={button} type="submit" disabled={busy || (modal === "encrypted-export" && !validClaudeBackupPassword(password, confirmation))}>{busy ? "처리 중…" : "파일 선택 후 진행"}</button></form></Dialog>}
    {modal === "windows" && <Dialog title="Claude windows" onClose={closeModal} busy={busy} error={error}><label className="block text-sm font-medium">Account<select className={field} value={windowAccountId} disabled={busy} onChange={(event) => setWindowAccountId(event.target.value)}>{accounts.map((account, index) => <option key={account.id} value={account.id}>{masked.includes(account.id) ? "숨김 " + (index + 1) : account.name}</option>)}</select></label><div className="flex gap-2"><button className={button} disabled={busy || !windowAccountId || !snapshot?.cli_available} onClick={() => { void runAction("open", { accountId: windowAccountId }, "선택한 Claude 계정으로 새 창을 열었습니다."); }}>이 계정으로 Claude 열기</button><button className={button} disabled={busy || !runningIds.includes(windowAccountId)} onClick={() => { void runAction("close", { accountId: windowAccountId }, "Switcher에서 연 Claude 창을 닫았습니다."); }}>Claude 창 닫기</button></div></Dialog>}
    {deleteId && <Dialog title="Claude 계정 제거" onClose={() => { if (!busyRef.current) setDeleteId(null); }} busy={busy} error={error}><p className="mb-4 text-sm">이 계정을 관리 목록에서 제거합니다. 현재 실행 중인 Claude 대화는 유지됩니다.</p><button className={button} disabled={busy} onClick={() => void perform(async () => { await invokeClaude("delete", { accountId: deleteId }); setDeleteId(null); })}>목록에서 제거</button></Dialog>}
  </div>;
}
