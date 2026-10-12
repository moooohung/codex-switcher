// Local-only UI fixture. It never reads real credentials or invokes a CLI.
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { resolve, extname } from 'node:path';
const root = resolve(import.meta.dirname, '../dist');
const port = Number(process.env.SWITCHER_FIXTURE_PORT ?? 3217);
const now = () => Math.floor(Date.now() / 1000);
const cacheObservedAt = now() - 120;
// Fixed synthetic deadlines: polling and opening a mock session never renew them.
const cacheKeepaliveNextAt = now() + 60;
const cacheKeepaliveLastAt = now() - 30;
const keepaliveStates = new Set([
  'off', 'waiting_session', 'idle', 'busy', 'scheduled', 'pending', 'expired', 'error', 'unsupported',
  'disabled', 'monitor_unavailable', 'needs_managed_session', 'waiting_for_request', 'awaiting_prewarm',
  'prewarming', 'active', 'stopped', 'limit_reached', 'ready', 'observation_required', 'human_input_required',
  'human_idle_limit', 'awaiting_response', 'cancelled', 'failed', 'ended', 'quota_unavailable', 'daily_budget',
]);
const fixtureKeepaliveState = keepaliveStates.has(process.env.SWITCHER_FIXTURE_KEEPALIVE_STATE)
  ? process.env.SWITCHER_FIXTURE_KEEPALIVE_STATE : null;
const fixtureKeepaliveMeasured = process.env.SWITCHER_FIXTURE_KEEPALIVE_MEASURED === '1';
const settings = {
  auto_switch_enabled: true, auto_switch_threshold_percent: 99.5,
  auto_switch_plan_preference: 'any', close_before_switch: false,
  reopen_after_switch: false, capture_usage: true, warmup_all: false,
  warmup_account_ids: [], timed_warmup_times: [], theme: 'light', sort_by: 'reset', model: 'sonnet',
};
const account = (id, name, active, provider) => ({
  id, name, email: `${name.toLowerCase()}@example.invalid`, plan_type: provider === 'claude' ? 'max' : 'pro',
  subscription_expires_at: null, auth_mode: provider === 'claude' ? 'claude_ai' : 'chat_g_p_t',
  is_active: active, created_at: '2026-10-01T00:00:00Z', last_used_at: null,
  profile_directory: 'synthetic', usage_observed_at: now(),
  usage: { account_id: id, plan_type: provider === 'claude' ? 'max' : 'pro', primary_used_percent: active ? 30 : 5,
    primary_window_minutes: 300, primary_resets_at: now() + 14000,
    secondary_used_percent: active ? 45 : 10, secondary_window_minutes: 10080, secondary_resets_at: now() + 500000,
    has_credits: null, unlimited_credits: null, credits_balance: null, error: null },
});
const claude = [account('11111111-1111-4111-8111-111111111111', 'Alpha', true, 'claude'), account('22222222-2222-4222-8222-222222222222', 'Beta', false, 'claude')];
if (process.env.SWITCHER_FIXTURE_AUTH_ERROR === '1') {
  claude[1].usage.error = 'OAuth refresh HTTP 400 (invalid_grant; sign in again); HTTP retry at 1791629922; last known quota retained';
  claude[1].usage_observed_at = now() - 14 * 3600;
}
let loginAccountId = null;
const codex = [account('synthetic-codex-alpha', 'Alpha', true, 'codex'), account('synthetic-codex-beta', 'Beta', false, 'codex')];
let masked = [];
const calls = [];
const cacheKeepalivePreferences = new Map();
const savedCacheSessions = new Map([...claude, ...codex].map((entry) => {
  const provider = entry.id.startsWith('synthetic-codex-') ? 'codex' : 'claude';
  return [`${provider}:${entry.id}`, [
    { id: `${provider}:saved/${entry.id}?opaque=1`, title: `${provider === 'claude' ? 'Claude' : 'Codex'} ${entry.name} 저장된 작업`,
      cwd: 'C:/synthetic/saved-project', model: `${provider}-synthetic`, updated_at: cacheObservedAt,
      resumable: true, reason: null, source: `${provider}_synthetic_history` },
    { id: `${provider}:older/${entry.id}`, title: '<script>synthetic title</script>', cwd: 'C:/synthetic/older-project',
      model: null, updated_at: cacheObservedAt - 3600, resumable: true, reason: null, source: `${provider}_synthetic_history` },
    { id: `${provider}:busy/${entry.id}`, title: `${entry.name} 사용 중인 대화`, cwd: 'C:/synthetic/busy-project',
      model: `${provider}-synthetic`, updated_at: cacheObservedAt - 60, resumable: false, reason: 'session_busy', source: `${provider}_synthetic_history` },
  ]];
}));
const stats = (id) => ({ account_id: id, available: true, source: 'Synthetic UI fixture', generated_at: new Date().toISOString(), stats_as_of: '2026-10-10',
  summary: { lifetime_tokens: 230, peak_daily_tokens: 230, longest_task_seconds: null, current_streak_days: null, longest_streak_days: null },
  activity: { fast_mode_percent: null, reasoning_effort: null, reasoning_effort_percent: null, skills_explored: null, total_skills_used: null, total_threads: 1 },
  daily: [{ date: '2026-10-10', tokens: 230 }], top_invocations: [], reset_credits: null, error: null });
const snapshot = () => ({ accounts: claude, settings, masked_account_ids: masked, active_account_id: claude.find(a => a.is_active)?.id,
  cli_available: true, cli_latest_version: '2.1.296', live_directory: 'synthetic', managed_running_ids: [],
  monitor_status: { phase: 'monitoring', message: 'Claude 계정 사용량을 백그라운드에서 확인 중', checked_at: new Date().toISOString(), switched_at: null },
  warmup_status: { version: 1, last_checked_at: now(), accounts: Object.fromEntries(claude.map(a => [a.id, {
    phase: 'idle', last_checked_at: now(), last_attempt_at: null, last_success_at: null,
    next_retry_at: null, last_error: null, pending_tasks: 0,
  }])) },
});
function invoke(command, args) {
  calls.push({ command, action: args.action, provider: args.provider,
    accountId: args.payload?.accountId ?? args.accountId, ...('sessionId' in args ? { sessionId: args.sessionId } : {}) });
  if (command === 'get_prompt_cache_status' || command === 'set_prompt_cache_auto_keepalive') {
    const key = `${args.provider}:${args.accountId}`;
    if (command === 'set_prompt_cache_auto_keepalive') cacheKeepalivePreferences.set(key, args.enabled);
    const claudeKnown = args.provider === 'claude' && args.accountId === claude[0].id;
    // Synthetic connection evidence only; opening/polling never creates cache success.
    const connectedSessions = (args.provider === 'claude' && args.accountId === claude[0].id)
      || (args.provider === 'codex' && args.accountId === codex[0].id) ? 2 : 0;
    const enabled = cacheKeepalivePreferences.get(key) ?? (process.env.SWITCHER_FIXTURE_KEEPALIVE_ENABLED === '1');
    const state = !enabled ? 'off' : fixtureKeepaliveState ?? (claudeKnown ? 'scheduled' : 'waiting_session');
    const measured = fixtureKeepaliveMeasured && claudeKnown;
    const routeReason = state === 'pending' ? 'native_codex_route_pending'
      : state === 'error' ? '유지 요청을 처리하지 못했습니다.'
        : state === 'busy' ? 'session_busy'
          : state === 'unsupported' ? (args.provider === 'codex' ? 'codex_web_unsupported' : '이 Claude 세션은 유지 요청 경로를 지원하지 않습니다.')
            : args.provider === 'codex' ? 'native_codex_managed_transport'
              : claudeKnown ? 'claude_official_interactive_plugin_monitor' : 'waiting_session';
    return {state:claudeKnown?'observed':'unknown',observed_at:claudeKnown?cacheObservedAt:null,model:claudeKnown?'synthetic-model':null,session_id:claudeKnown?'synthetic-session':null,
      cache_read_tokens:claudeKnown?9000:null,cache_write_tokens:claudeKnown?1000:null,uncached_input_tokens:claudeKnown?0:null,
      expires_at:claudeKnown?cacheObservedAt+3600:null,ttl_seconds:claudeKnown?3600:null,ttl_source:claudeKnown?'claude_statusline':null,
      reason:claudeKnown?null:'계정별 캐시 만료시간을 확인할 수 없습니다.',
      auto_keepalive_enabled:enabled,
      auto_keepalive_connected_sessions:connectedSessions,
      auto_keepalive_supported:state !== 'unsupported' && state !== 'pending',
      auto_keepalive_state:state,
      auto_keepalive_reason:routeReason,
      auto_keepalive_next_at:['scheduled', 'ready', 'awaiting_prewarm', 'active'].includes(state) ? cacheKeepaliveNextAt : null,
      auto_keepalive_last_at:measured ? cacheKeepaliveLastAt : null,
      auto_keepalive_requests:measured ? 2 : 0,
      auto_keepalive_cache_read_tokens:measured ? 8192 : null,
      auto_keepalive_output_tokens:measured ? 3 : null};
  }
  if (command === 'list_prompt_cache_sessions') {
    if (process.env.SWITCHER_FIXTURE_SESSION_LIST_ERROR === '1') throw new Error('저장된 대화 목록을 읽지 못했습니다.');
    return { sessions: process.env.SWITCHER_FIXTURE_SESSION_LIST_EMPTY === '1'
      ? [] : savedCacheSessions.get(`${args.provider}:${args.accountId}`) ?? [],
      truncated: process.env.SWITCHER_FIXTURE_SESSION_LIST_TRUNCATED === '1', reason: null };
  }
  if (command === 'open_prompt_cache_session') {
    const enabled = cacheKeepalivePreferences.get(`${args.provider}:${args.accountId}`)
      ?? (process.env.SWITCHER_FIXTURE_KEEPALIVE_ENABLED === '1');
    if (!enabled) throw new Error('먼저 이 계정의 캐시 자동 유지 옵션을 켜 주세요.');
    if (process.env.SWITCHER_FIXTURE_KEEPALIVE_OPEN_ERROR === '1') throw new Error('이 대화가 사용 중이어서 이어갈 수 없습니다.');
    if ('sessionId' in args) {
      const session = savedCacheSessions.get(`${args.provider}:${args.accountId}`)?.find(item => item.id === args.sessionId);
      if (!session) throw new Error('저장된 대화를 찾지 못했습니다. 목록을 새로고침해 주세요.');
      if (!session.resumable) throw new Error('이 대화가 사용 중이어서 이어갈 수 없습니다.');
    }
    return { started: true };
  }
  if (command === 'claude_command') {
    const payload = args.payload ?? {};
    switch (args.action) {
      case 'snapshot': return snapshot();
      case 'start_relogin': loginAccountId = payload.accountId; return { login_id: 'synthetic-relogin' };
      case 'complete_login': {
        const target = claude.find(a => a.id === loginAccountId);
        if (!target) throw new Error('No synthetic login pending');
        target.usage.error = null; target.usage_observed_at = now();
        loginAccountId = null;
        return { pending: false, account_id: target.id, reauthenticated: true };
      }
      case 'cancel_login': loginAccountId = null; return null;
      case 'refresh': for (const a of claude) if (!payload.accountId || payload.accountId === a.id) a.usage_observed_at = now(); return snapshot();
      case 'settings': Object.assign(settings, payload.settings); return settings;
      case 'mask': masked = payload.ids; return null;
      case 'stats': return stats(payload.accountId);
      case 'switch': for (const a of claude) a.is_active = a.id === payload.accountId; return { message: 'Synthetic switch complete' };
      case 'warmup': return null;
      case 'warmup_all': return { total_accounts: claude.length, warmed_accounts: claude.length, failed_account_ids: [] };
      case 'auto_switch': return { switched: false, waiting: false };
      case 'rename': claude.find(a => a.id === payload.accountId).name = payload.newName; return null;
      case 'export_slim': return 'claude1.synthetic-ui-fixture';
      case 'delete': { const at = claude.findIndex(a => a.id === payload.accountId); if (at >= 0) claude.splice(at, 1); return null; }
      default: return null;
    }
  }
  switch (command) {
    case 'list_accounts': return codex;
    case 'get_usage': return codex.find(a => a.id === args.accountId)?.usage;
    case 'refresh_account_metadata': return codex.find(a => a.id === args.accountId);
    case 'get_account_usage_stats': return stats(args.accountId);
    case 'get_masked_account_ids': return [];
    case 'get_dock_display_mode': return 'show_in_dock';
    case 'check_codex_processes': return { count: 0, background_count: 0, can_switch: true, pids: [] };
    case 'get_display_settings': return { auto_switch_enabled: true, auto_switch_threshold_percent: 99.5, auto_switch_plan_preference: 'any', tray_display_mode: 'remaining' };
    case 'get_auto_switch_status': return { phase: 'monitoring', checked_at: new Date().toISOString(), message: '사용량 확인 중' };
    default: return null;
  }
}
const server = createServer(async (req, res) => {
  const path = new URL(req.url, `http://127.0.0.1:${port}`).pathname;
  const json = (value) => { res.setHeader('Content-Type', 'application/json'); res.end(JSON.stringify(value)); };
  try {
    if (path === '/api/health') return json({ ok: true });
    if (path === '/fixture/state') return json({ calls, snapshot: snapshot() });
    if (path.startsWith('/api/invoke/')) {
      let body = ''; for await (const chunk of req) body += chunk;
      return json(invoke(path.slice('/api/invoke/'.length), JSON.parse(body || '{}')));
    }
    const file = resolve(root, '.' + (path === '/' ? '/index.html' : path));
    if (!file.startsWith(root + '/') && !file.startsWith(root + '\\')) { res.statusCode = 403; return res.end(); }
    res.setHeader('Content-Type', { '.js': 'text/javascript', '.css': 'text/css', '.html': 'text/html', '.svg': 'image/svg+xml' }[extname(file)] ?? 'application/octet-stream');
    res.end(await readFile(file));
  } catch (error) { res.statusCode = 500; json({ error: error instanceof Error ? error.message : String(error) }); }
});
server.listen(port, '127.0.0.1', () => console.log(`Synthetic Switcher UI http://127.0.0.1:${server.address().port}`));
