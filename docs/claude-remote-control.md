# Claude Remote Control after account switching

`auto_remote_control` enables the official `remoteControlAtStartup` setting
and automatic reconnection of observed, already-running Windows CLI sessions.
It operates in the backend even when another Switcher tab is selected. It does
not send a model prompt, fork/resume history, kill Claude, clear a draft, take
mouse focus, or automatically answer a permission/first-connection dialog.

The previous startup-only implementation could not reconnect an existing TUI.
Claude Code intentionally disconnects the previous Remote Control owner when
the signed-in account changes. Its supported reconnection action is `/rc`.

## Existing sessions

The backend scans Claude's existing `sessions/<pid>.json` registry on startup
and every five seconds. Sessions opened before installation register without
a new message or picker. It verifies native executable location, PID/creation
time, session UUID, working directory and Windows PID domain, excluding stale,
headless, parked and foreign entries. It never reads messaging keys or rewrites
Claude registry files. A runtime RC binding alone is not connection verification.
Same-session hooks retain the native binding history and timestamp. A newly
published binding admits one read-only confirmation once the session is idle;
it does not admit `/rc` input, which could toggle an existing connection off.
Immediately before an input reservation, bounded native metadata is read twice
to reject a newly published binding, changed process/session or ongoing work.

Official exec-form lifecycle hooks also register the exact interactive parent PID,
process creation time, session UUID, working directory and configuration
directory. Hooks reject headless/background/subagent and custom API sessions.
ConfigChange allows an already-open CLI to become observed after settings
reload; otherwise its next lifecycle event registers it. No session picker or
model turn is needed. A delayed Stop older than a new prompt is ignored.

A durable per-owner generation and a cross-host monitor lease coordinate the
GUI and LAN backends. Ordinary observation never attaches a console. A demand
helper probes after an account/lifecycle change needing reconnection, deferring
busy work and permission waits. An unchanged failed probe is not repeatedly
retried; an idle `Stop` can precede the TUI's empty composer, so that exact
account/lifecycle permits at most three probes, ten seconds apart, for an empty
or stable prompt. Busy work, shell mode, failed responses and permission waits
never admit those rechecks. A newer lifecycle renews the observation budget;
an attempted input is never sent again in the same account generation.
After delivery at most three confirmation probes run. It requires a
recorded active connection or native RC binding under the previous account, the
account-change/disconnect notification, a native empty composer,
an idle footer, no queued input and two stable observations. Immediately before
delivery it rechecks identity, lifecycle, process creation time, console members
and the screen. It writes only `/rc` + Enter as one input batch. Work, typing,
approval dialogs, restricted project settings, inaccessible/narrow consoles and
ambiguous bindings defer the action. A sent/uncertain attempt is not repeated
in the same generation.

The UI separates processes, native RC bindings, requests and the observed
`/rc active` indicator, including Claude's shortened `/rc` footer. The old
account's still-visible indicator during auth transition cannot count as the
new owner's connection. No console contents, tokens or prompts are persisted.
Dead/reused PIDs retire. Corrupt primary state is not restored from a backup
and cannot break the main account-list UI.
Waiting reasons distinguish active work, shell mode, permissions, failed
responses, the empty composer, policy restrictions, initial activation and
unconfirmed connection. A scheduled recheck is reported only when the backend
admits it; neither registration nor user input implies a live RC connection.

The Windows console API is a compatibility adapter for already-running TUIs:
those processes do not expose a retrospective ConPTY input pipe. It is confined
to short-lived helpers. A keystroke in the final interval between the last screen
check and input write remains a race; uncertain delivery is never replayed.

## Limits and verification

This supports interactive CLI sessions using the shared live Claude config.
Separately pinned account profiles retain their account and startup policy.
Claude Desktop Code chats and terminals without an attachable Win32 console
are not controlled. Organization/project policy, trusted devices and first-run
consent remain Claude's responsibility.

Verified offline: policy/hook merge, preservation/idempotence, stale-event and
account-generation handling, corruption behavior, and actual Win32 transport
in private invisible consoles. Eleven cases cover successful `/rc`, draft,
busy, approval, already-connected, reused PID, changed identity and an
unobserved connection, and a notice that scrolled away while work continued.
They also cover Korean wide-cell text and preservation of a Korean draft.
An observed disconnect is retained for that account generation only.
Conversation prose alone never authorizes delivery.
Tests do not switch a live account, send model requests, interact with a user
terminal or prove that a remote session appears on a phone. A live cross-account
Remote Control connection is a separate validation, recorded separately in the
installation report.

The 2026-10-11 blank-terminal investigation did not reproduce blanking or mode
changes in 16 private ConPTY attach/read/detach cases. The experiment did not
test the actual Rust helper/Bun/Claude or Windows Terminal renderer and cannot
establish the cause of the user's blank pane. Ambiguous screen/draft states stay
pending; preserve the running CLI.

Sources checked on 2026-10-11; implementation is original, without copied code:

- Official contract: https://code.claude.com/docs/en/remote-control
- Exec-form lifecycle hooks: https://code.claude.com/docs/en/hooks
- Desktop account-switch issue: https://github.com/anthropics/claude-code/issues/96049
- Terminal command routing: https://github.com/pereljon/claude-mux
- Hook observation and targeted control: https://github.com/sverrirsig/claude-control
- Console compatibility: https://learn.microsoft.com/en-us/windows/console/writeconsoleinput
