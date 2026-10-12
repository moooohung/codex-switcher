<p align="center">
  <img src="src-tauri/icons/logo.svg" alt="Codex Switcher" width="128" height="128">
</p>

<h1 align="center">Codex Switcher — Codex + Claude Code fork</h1>

This edition is maintained at [moooohung/codex-switcher](https://github.com/moooohung/codex-switcher), forked from [Lampese/codex-switcher](https://github.com/Lampese/codex-switcher). Original project attribution and third-party notices are retained.

It adds an independent Claude Code provider with profile-scoped quota polling, account switching, warmup, encrypted backup, and matching account cards. Optional cache maintenance and Windows Remote Control reconnection are documented in [cache maintenance](docs/prompt-cache-keepalive.md) and [Remote Control](docs/claude-remote-control.md).

Inactive accounts use their own saved OAuth credentials for quota polling. Cache preferences can be saved before a session is connected. Missing quota windows remain unknown; stale or revoked credentials do not become spare quota. A revoked refresh token still requires official sign-in. Settings, runtime registration, a confirmed connection and measured cache hits are displayed separately.

The fork disables the upstream in-app updater so an upstream package cannot replace these changes. Passing main commits produce versioned preview releases; stable releases require a matching version tag. A source commit does not imply that a release has passed its publication checks. Dependency update automation remains enabled.

[![CI](https://github.com/moooohung/codex-switcher/actions/workflows/pull-request-validation.yml/badge.svg)](https://github.com/moooohung/codex-switcher/actions/workflows/pull-request-validation.yml)
[![Build & Release](https://github.com/moooohung/codex-switcher/actions/workflows/build.yml/badge.svg)](https://github.com/moooohung/codex-switcher/actions/workflows/build.yml)

<p align="center">
  A Desktop Application for Managing Multiple OpenAI <a href="https://github.com/openai/codex">Codex</a> Accounts<br>
  Easily switch between accounts, monitor usage, schedule warm-ups, and stay in control of your quota
</p>

<div align="center">

[![](https://dcbadge.limes.pink/api/server/4QzyJTC3S)](https://discord.gg/4QzyJTC3S)

</div>

## Features

- **Multi-Account Management** – Add, rename, mask, import, export, and manage multiple Codex accounts in one place
- **Quick Switching** – Switch between accounts from the main window, native tray menu, or tray popup while preserving rotated ChatGPT sessions
- **Usage Stats** – View account usage stats for OAuth accounts, including lifetime tokens, daily buckets, streaks, activity insights, and top integrations
- **Manual Reset Credits** – See available manual reset credits beside each account plan badge, with the closest expiry highlighted as it approaches
- **Automatic Warm-Up** – Warm up one account or all accounts manually, after each 5-hour reset window, or at specific scheduled times of day
- **System Tray Controls** – Use the tray popup to switch accounts, inspect quota and active-account stats, refresh usage, open the main window, or quit the app
- **Tray Display Modes** – Choose between the app icon with session percentage, a text-only hourly/weekly percentage display, or a hidden tray icon
- **macOS Dock Control** – Keep Codex Switcher in the Dock or run it as a menu bar only app, with a first-close prompt and a tray fallback
- **Rate-Limit Monitoring** – View real-time 5-hour session and weekly usage, reset timing, credits, and subscription expiry
- **Blocked Switch Recovery** – Detect running Codex sessions and offer a force-close flow before retrying the account switch
- **Dual Login Mode** – Authenticate with ChatGPT OAuth or import existing `auth.json` files

## Installation

### Download a Release

Completed builds publish platform packages here. Main previews have a
`v<version>-ci.g<commit>` tag; stable releases use `v<version>`. If the required
checks or any platform build fails, no partial release is published:

[Fork releases](https://github.com/moooohung/codex-switcher/releases)

Choose the filename prefix for your platform:

- **macOS Apple Silicon:** `aarch64-apple-darwin--*.dmg`
- **macOS Intel:** `x86_64-apple-darwin--*.dmg`
- **Windows:** `x86_64-pc-windows-msvc--*.exe` or `*.msi`
- **Linux:** `x86_64-unknown-linux-gnu--*.deb`, `*.AppImage` or `*.rpm`

Each release includes SHA-256 checksums and per-platform provenance manifests.
Verify a downloaded package's GitHub build attestation with:

```bash
gh attestation verify <package> --repo moooohung/codex-switcher
```

> **macOS:** current release builds are not Apple-notarized. If macOS says the
> app is damaged, move it to `/Applications` and remove the quarantine flag:
>
> ```bash
> sudo xattr -dr com.apple.quarantine "/Applications/Codex Switcher.app"
> open "/Applications/Codex Switcher.app"
> ```

### Auto Updates

This fork uses manual updates from its own releases. In-app updates are disabled
until the fork has its own verified signing and update infrastructure.

### Build from Source

#### Prerequisites

- [Node.js](https://nodejs.org/) (26.11.1, matching the fork's CI)
- [pnpm](https://pnpm.io/) (12.11.2, matching the fork's CI)
- [Rust](https://rustup.rs/) (stable)

```bash
# Clone the repository
git clone https://github.com/moooohung/codex-switcher.git
cd codex-switcher

# Install dependencies
pnpm install --frozen-lockfile

# Run in development mode
pnpm tauri dev

# Build for production
pnpm tauri build
```

> **Windows:** the `pnpm tauri` script runs through a POSIX shell wrapper
> (`sh ./scripts/tauri.sh`) and will not work in PowerShell/CMD. Use the
> `tauri:win` script instead: `pnpm tauri:win dev` and `pnpm tauri:win build`.

The built application will be in `src-tauri/target/release/bundle/`.

### Run the Dashboard in a Browser

You can also serve the built dashboard over HTTP instead of opening the Tauri shell.

```bash
# Build the frontend and start the web server on 0.0.0.0:3210
pnpm lan
```

Optional environment variables:

- `CODEX_SWITCHER_WEB_HOST` to override the bind host
- `CODEX_SWITCHER_WEB_PORT` to override the port

The browser dashboard serves the same UI and backend actions through `/api/invoke/*`, which makes it usable over LAN, Tailscale, or a remote host tunnel when you expose the chosen port safely.

## Usage and Reset Credits

Codex Switcher shows two kinds of account usage information:

- **Rate limits** – the account card shows the current 5-hour and weekly limit
  windows, remaining percentage, reset timing, credit balance, and subscription
  expiry when available.
- **Usage Stats** – ChatGPT OAuth accounts can expand the **Usage
  Stats** panel to view stats such as lifetime tokens,
  today, last 7 days, last 30 days, streaks, longest task, token activity,
  reasoning/activity insights, and most-used integrations. The active account
  opens this panel by default; other accounts keep it collapsed until needed.
- **Manual reset credits** – OAuth accounts with available reset credits show a
  compact badge next to the plan badge. It includes the available count and the
  closest expiry date, hides zero-count results, and turns amber within 10 days
  or red within 3 days of expiry.

The tray popup also includes compact active-account stats for today and
the last 7 days, while keeping the normal rate-limit refresh flow separate.

## Safe Account Switching

ChatGPT can replace an OAuth refresh token after using it. Once replaced, the
older token may no longer be accepted. Before Codex Switcher writes another
account to `~/.codex/auth.json`, it now saves the latest tokens from the account
that is currently active. Switching back therefore restores the current session
instead of an older snapshot.

Token refreshes and account switches are serialized so a background refresh
cannot finish late and overwrite the account you just selected. Codex Switcher
also avoids refreshing the active account while Codex or ChatGPT is running;
close the running app before switching accounts.

If an older Codex Switcher version already saved an invalid refresh token, sign
in to that account again or remove and re-add it once. An invalidated token
cannot be recovered locally.

## macOS Dock and Menu Bar Mode

On macOS, Codex Switcher can either stay visible in the Dock or live only in the
menu bar. The first time you close the main window, the app asks which behavior
you want and lets you choose whether to show that prompt again.

You can change the same setting later from the tray popup or from the native
tray menu under **Dock Icon**. If you choose **Menu Bar Only**, the app keeps a
visible tray item so you can always reopen the main window or switch back to
Dock mode.

## Warm-Up

A warm-up sends one minimal request to an account so its current usage window
has activity before you need it.

- **Manual** – warm up a single or all accounts, from the main window or tray menu.
- **Automatic** – when enabled (per account or for all), the app tracks the
  5-hour window when available and warms it after each reset, as long as the
  weekly limit isn't exhausted. If only the weekly window is available, it
  warms once after the weekly reset and automatically returns to the 5-hour
  schedule if that window reappears.
- **Timed** – pick specific times of day (e.g. `08:00`, `13:00`, `18:00`) from
  the **Timed** control in the main window. At each time the app warms all
  accounts (skipping any whose weekly limit is exhausted), so you control when
  your 5-hour windows start instead of letting them drift.

Timed warm-up checks the schedule every 30 seconds, runs each configured minute
only once per day, and skips missed times if the machine was asleep instead of
warming accounts late.

On macOS you can keep the machine awake with the built-in `caffeinate` command,
which stops automatically when the app quits:

```bash
caffeinate -i -w "$(pgrep -x 'Codex Switcher')"
```

## Disclaimer

This tool is designed **exclusively for individuals who personally own multiple OpenAI/ChatGPT accounts**. It is intended to help users manage their own accounts more conveniently.

**This tool is NOT intended for:**

- Sharing accounts between multiple users
- Circumventing OpenAI's terms of service
- Any form of account pooling or credential sharing

By using this software, you agree that you are the rightful owner of all accounts you add to the application. The authors are not responsible for any misuse or violations of OpenAI's terms of service.

## GitHub Actions

The required `CI gate` aggregates frontend tests/build, serialized Rust tests on
Linux, Windows and macOS, workflow lint, CodeQL for TypeScript/JavaScript, Rust
and Actions, and dependency review on pull requests. It also runs weekly and
supports merge queues. Focused Linux startup and updater workflows are retained.
Dependabot checks npm, Cargo and action updates daily.

A successful main **push** CI starts a four-target build. Publication requires
successful CI for that exact commit, synchronized source versions, all four
platform bundles, verified attestations and uploaded asset digest readback.
Pull request CI never triggers a release. Superseded commits are skipped when
their source digest differs from the release workflow's signed digest. Stable
tags must match `package.json`, Tauri, Cargo and the lockfile. Builds use a
temporary preview version without changing source history.

Maintainers can run `Build & Release` on main after CI passes; leave `version`
empty for a preview or enter the exact matching stable tag. Completed tags are
idempotent; failed draft publications can be retried. Workflow tokens default
to read access, and only the final publisher receives release write access.
Actions are pinned to full commit SHAs. Cache and job time limits bound wasted
runs. Standard GitHub-hosted runners and public-repository security features
are used; no paid AI action, larger runner or signing service is required.

## Versioning

Use the version bump helper to keep app versions in sync across Tauri, Cargo, and the frontend.

```bash
# Exact version
pnpm version:bump 0.2.1

# Semver bumps
pnpm version:patch
pnpm version:minor
pnpm version:major

# Prepare a release commit and tag
# This prompts for a short release note and runs the version bump first.
pnpm release patch

# Prepare and push a release
# The tag stores the release note for the in-app update prompt.
pnpm release patch -- --push

# For non-interactive use, pass the note explicitly.
pnpm release patch -- --push --note "Fixed account switching issues"
```
