# Prompt-cache observations and opt-in maintenance

Enable **캐시 자동 유지** on an account card. The switch is independent for
each provider and account, defaults to off, and applies automatically to already
connected open sessions. The connection count is separate from cache-hit or
maintenance-success evidence. A separate account warmup does not maintain a
working conversation's prefix.

Interactive Claude Code sessions in a matching current-login or account-profile
directory can attach without restart, resume or a session choice. Switcher
installs settings-defined lifecycle hooks; Claude's settings watcher picks up
the change. The next ordinary user input registers that existing session.
When the conversation stops, an official `asyncRewake` hook waits for the
observed cache deadline and can wake the same idle conversation. Enabling the
option does not itself send a prompt or replay previous input. An already-idle
session with no subsequent user input remains unconnected.

Native Codex automatically follows the preference on an already-connected
managed transport. An external native CLI/app process with no reachable control
transport cannot have its request route replaced while running. This remains
an explicit connection limitation; Switcher does not silently resume or create
a replacement conversation. The secondary **세션 연결 도움말** retains manual
new-session and saved-conversation fallback controls.

## Continue an existing conversation

Use **기존 대화 이어가기**, select a saved conversation, then choose
**선택한 대화 이어가기**. Enable cache maintenance on that account before
opening it. The saved conversation UUID and working directory are retained;
native Codex also restores the saved model and reasoning effort. The adapter
uses official `codex resume` or Claude `--resume`, with no fork and no initial
prompt. A resumed conversation does not guarantee recovery of an expired
server cache; new observed requests establish maintenance evidence.

The original transcript is read only by Switcher. Account-profile Claude
history and matching current-login global Claude history are listed; native
Codex reads the configured local session store. Web-bridge history is disabled
to prevent an implicit model/provider change. History enumeration is bounded
and shows a limited recent list. Unknown metadata or missing working folders
are refused rather than guessing another conversation.

Close the existing session before resuming. Active Claude session markers,
open history writers on Windows and a cross-process resume lease prevent a
known running conversation from receiving a second Switcher writer. The
detached owner keeps its lease across GUI restarts. Switcher does not close an
external CLI to make a resume succeed.

## Claude Code

Managed interactive Claude sessions can use a local plugin with lifecycle hooks
and an official background monitor. Existing interactive sessions instead use
hot-reloaded settings hooks and a bounded `asyncRewake` timer. They share the
same per-session lease and account budget, so the routes cannot both schedule
a request for one session. Near the observed cache deadline, the adapter
requests a short acknowledgement in the same session. This consumes
subscription quota and adds a notification/reply to the conversation. It does
not type into the terminal, clear a draft, or press Escape.

The CLI controls notification delivery. Automatic live attachment and monitors are unavailable in `claude -p`
and may be unavailable with some provider or telemetry settings. The card waits
for a real monitor heartbeat rather than treating an installed plugin as a
connected session. Turning the account option off suppresses future pings even
if Claude leaves the plugin process running.

Official statusline TTL/expiry is preferred. A bounded transcript-tail fallback
uses a matching session, model and request usage timestamp. Polling never
extends the expiry; missing or ambiguous data remains unknown.
Only official scoped TTL evidence schedules automatic requests. Transcript
estimates are display-only. Claude requires a fresh, matching quota receipt;
busy conversations, background work, compaction, errors and expired caches
suppress maintenance. The account budget is 12 attempts per rolling 24 hours,
with a 12-hour limit since the last human input.

A `StopFailure` suspends maintenance for that response; it does not permanently
unregister a still-running CLI. A new human input clears the response failure,
invalidates the old cache anchor and still waits for that input's successful
main `Stop`. Old prompt completions cannot idle a newer input. For older ledgers
that latched a failure without its generation, an exact, newer, idle main Stop
can recover the same registered session. Attempt budgets and cancelled-request
attribution are preserved. The UI distinguishes a Claude response failure from
a maintenance request error.

## Native Codex

A new managed Codex CLI uses a private loopback Responses transport. Provider
configuration is passed to that process; the global Codex configuration, desktop
app and existing jobs are not rewritten. The transport observes the actual
rendered request, pins the selected account, and keeps the bounded request body
in memory only.

GPT-5.6 and later have an official `prompt_cache_options.prewarm` request that
prepares the same prefix without generating output. Automatic maintenance uses
that path; it must not fall back to an ordinary generation if the serving
endpoint rejects or ignores it. ChatGPT OAuth backend support is distinct from
the public API contract and must be confirmed from the actual response. Web
bridge browser sessions do not expose this transport.
The selected account stays pinned. One managed host per account holds a process
lease across UI restarts; fresh token-free metadata restores the card's state.
The native budget is 12 attempts per host lifetime and 12 hours without a new
foreground request, not an account-wide daily budget. Requests above 4 MiB still
stream normally but are not retained for maintenance. Unsupported or ambiguous
prewarm responses stop automatic requests for that host. Native OAuth provides
no authoritative expiry, so its countdown remains unknown while actual cache
usage and scheduling are shown separately.
Token refresh keeps the existing authentication-owner guard. If a running
Codex owns the active account's rotation, known-expired tokens are rejected
locally and the card waits for authentication; it does not rotate behind that
process or permanently disable maintenance. After the owner refreshes or the
user signs in again, a new foreground request can establish a fresh capture.

## Status and cost

An account's saved preference is independent of its current login and runtime.
When a shared Claude directory changes owners, the previous owner's historical
cache observation remains visible while its connection is shown as inactive.
The new owner's lease is never attributed to it. Missing authentication or a
damaged runtime ledger leaves the checkbox usable and reports a connection
problem; it does not authorize maintenance or manufacture a cache hit.

The card distinguishes a reservation, a sent request and observed cache reads.
HTTP success alone does not prove a cache hit. Unknown output-token usage is
shown as unknown, not zero. Deadlines are estimates for one observed session
and model, not an account-wide cache balance.

Maintenance is bounded by idle, busy, expiry, cancellation and request-budget
checks. It never executes generated tool calls. Immutable helper executables
let a managed session outlive a Switcher UI restart without locking the installed
Switcher executable on Windows.

Validation uses offline lifecycle fixtures and loopback mock transports. It
does not make a paid request or claim a measured live subscription cache hit.

References checked on 2026-10-11:

- [Claude plugin monitors](https://code.claude.com/docs/en/plugins/components#monitors)
- [Claude settings hooks and asyncRewake](https://code.claude.com/docs/en/hooks)
- [Claude statusline](https://code.claude.com/docs/en/statusline)
- [OpenAI prompt caching and prewarming](https://developers.openai.com/api/docs/guides/prompt-caching)
- [Codex custom-provider configuration](https://github.com/openai/codex/tree/main/codex-rs/model-provider-info)

Implementation patterns were researched in `demouo/claude-code-cache-keepalive`,
`ilia-pluzhnikov/claude-code-statusline`, and the CortexKit cache-keep adapters.
These projects are references, not proof that an external live session is
connected or that a particular endpoint supports prewarming.
