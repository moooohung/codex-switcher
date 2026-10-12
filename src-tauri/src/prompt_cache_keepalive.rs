//! Same-conversation keepalive through official interactive monitors or live hooks.
//!
//! Integration: prepare before `claude --plugin-dir`, dispatch the native flags
//! before Tauri, observe only after validated main-session collection, decorate
//! after reading preferences. Never launch Claude, authenticate, or rotate here.
//! Docs checked with Context7 /websites/code_claude on 2026-10-11:
//! https://code.claude.com/docs/en/plugins/components#monitors
//! https://code.claude.com/docs/en/plugins/manifest-reference#monitors
//! https://code.claude.com/docs/en/hooks#exec-form-and-shell-form
//!
//! A queued notification is an attempt, not evidence of a response or extension.
//! One atomic, bounded account ledger holds independent session/model records
//! and the shared rolling 24h budget. Missing/corrupt primaries never recover a
//! backup. Terminal sessions are retained at least 24h; only ended, unleased
//! records whose pending budget has aged out can be reclaimed at the cap.
use crate::{claude::storage::Manager, prompt_cache::PromptCacheStatus};
use anyhow::{ensure, Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions, TryLockError},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::OnceLock,
    thread,
    time::{Duration, Instant, SystemTime},
};

const PLUGIN: &str = ".switcher-cache-plugin";
const LEDGER: &str = "ledger.json";
const MAX_FILE: u64 = 256 * 1024;
const MAX_INPUT: u64 = 1024 * 1024;
const MAX_SESSIONS: usize = 64;
const MAX_REQUESTS: usize = 12;
const DAY: i64 = 86_400;
const HUMAN_IDLE: i64 = 12 * 3600;
const HEARTBEAT: i64 = 15;
pub const NOTIFICATION: &str = "Cache keepalive: reply with ok only; no tools or project work.";
const TTL_SOURCE: &str = "claude_statusline.prompt_cache.ttl";
const LIVE_HOOK: &str = "--prompt-cache-live-hook";
const LIVE_TIMER: &str = "--prompt-cache-live-timer";
const LIVE_TIMEOUT: u64 = 3660;
const LIVE_PARENT_PROBE_SECONDS: u64 = 6;
const LIVE_LEDGER_WAIT_SECONDS: u64 = 6;
const LIVE_HOOK_TIMEOUT: u64 = 20;
const LIVE_TIMER_SECONDS: u64 = 3600;
const LIVE_START_GRACE: u64 = 10;
const LIVE_EVENTS: [&str; 6] = [
    "SessionStart",
    "UserPromptSubmit",
    "Stop",
    "StopFailure",
    "PreCompact",
    "SessionEnd",
];

struct CachedHelper {
    path: PathBuf,
    len: u64,
    modified: SystemTime,
}
static LIVE_HELPER: OnceLock<CachedHelper> = OnceLock::new();

// A verified content-addressed copy is cached for this running binary only.
// Background account reconciliation must not hash/copy the executable every 30s.
fn cached_helper() -> Result<&'static Path> {
    if LIVE_HELPER.get().is_none() {
        let path = crate::prompt_cache::helper_executable()?;
        let metadata = fs::metadata(&path)?;
        let _ = LIVE_HELPER.set(CachedHelper {
            path,
            len: metadata.len(),
            modified: metadata.modified()?,
        });
    }
    let helper = LIVE_HELPER.get().context("Missing verified cache helper")?;
    ordinary(&helper.path)?;
    let metadata = fs::metadata(&helper.path)?;
    ensure!(
        metadata.is_file()
            && metadata.len() == helper.len
            && metadata.modified()? == helper.modified,
        "Verified cache helper changed"
    );
    Ok(&helper.path)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Anchor {
    model: String,
    at: i64,
    expiry: i64,
    ttl: i64,
    cache_read_tokens: Option<i64>,
    output_tokens: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pending {
    queued_at: i64,
    previous: Anchor,
    human_generation: u64,
    stop_at: Option<i64>,
    cancelled: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Session {
    directory: PathBuf,
    started_at: i64,
    human_at: Option<i64>,
    human_generation: u64,
    invalidated_at: Option<i64>,
    compacted_at: Option<i64>,
    busy: bool,
    idle_at: Option<i64>,
    failed: bool,
    // A response failure belongs to a human-input generation, not the lifetime
    // of a resumed interactive process. None also reads pre-fix ledgers.
    #[serde(default)]
    failed_generation: Option<u64>,
    ended: bool,
    ended_at: Option<i64>,
    heartbeat_at: Option<i64>,
    monitor: Option<String>,
    anchor: Option<Anchor>,
    pending: Option<Pending>,
    requests: u64,
    last_at: Option<i64>,
    last_anchor_at: Option<i64>,
    last_model: Option<String>,
    cache_read_tokens: Option<i64>,
    output_tokens: Option<i64>,
    // Only direct lifecycle events set these. An old ledger remains readable.
    #[serde(default)]
    live_prompt_id: Option<String>,
    #[serde(default)]
    live_stop_prompt_id: Option<String>,
    #[serde(default)]
    live_event_created_at: Option<u64>,
    #[serde(default)]
    live_process_created: Option<u64>,
    #[serde(default)]
    live_pid: Option<u32>,
}

impl Session {
    fn new(directory: PathBuf, now: i64) -> Self {
        Self {
            directory,
            started_at: now,
            human_at: None,
            human_generation: 0,
            invalidated_at: Some(now),
            compacted_at: None,
            busy: true,
            idle_at: None,
            failed: false,
            failed_generation: None,
            ended: false,
            ended_at: None,
            heartbeat_at: None,
            monitor: None,
            anchor: None,
            pending: None,
            requests: 0,
            last_at: None,
            last_anchor_at: None,
            last_model: None,
            cache_read_tokens: None,
            output_tokens: None,
            live_prompt_id: None,
            live_stop_prompt_id: None,
            live_event_created_at: None,
            live_process_created: None,
            live_pid: None,
        }
    }

    fn cancel(&mut self) {
        if let Some(pending) = &mut self.pending {
            pending.cancelled = true;
        }
    }

    fn confirm(&mut self, now: i64) {
        let (Some(pending), Some(anchor)) = (&self.pending, &self.anchor) else {
            return;
        };
        if !pending.cancelled
            && !self.failed
            && !self.ended
            && !self.busy
            && pending.human_generation == self.human_generation
            && pending.stop_at.is_some_and(|at| at >= pending.queued_at)
            && anchor.model == pending.previous.model
            && anchor.at > pending.previous.at
            && anchor.at >= pending.queued_at
            && anchor.expiry > pending.previous.expiry
            && anchor.expiry > now
        {
            self.last_at = Some(anchor.at);
            self.last_anchor_at = Some(anchor.at);
            self.last_model = Some(anchor.model.clone());
            self.cache_read_tokens = anchor.cache_read_tokens;
            self.output_tokens = anchor.output_tokens;
            self.pending = None;
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    version: u8,
    account_id: String,
    account_uuid: String,
    clock: i64,
    attempts: Vec<i64>,
    sessions: BTreeMap<String, Session>,
}

struct Identity {
    profile: PathBuf,
    directory: PathBuf,
    uuid: String,
}

fn uuid(value: &str) -> Result<()> {
    ensure!(
        uuid::Uuid::parse_str(value)?.to_string() == value,
        "Noncanonical UUID"
    );
    Ok(())
}

fn ordinary(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                !metadata.file_type().is_symlink(),
                "Redirected keepalive path"
            );
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                ensure!(
                    metadata.file_attributes() & 0x400 == 0,
                    "Redirected keepalive path"
                );
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

fn read_bytes(mut reader: impl Read, limit: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.by_ref().take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= limit, "Oversized keepalive input");
    Ok(bytes)
}

fn primary<T: DeserializeOwned>(path: &Path, limit: u64) -> Result<Option<T>> {
    ordinary(path)?;
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    ensure!(file.metadata()?.is_file(), "Expected ordinary JSON file");
    Ok(Some(serde_json::from_slice(&read_bytes(file, limit)?)?))
}

fn required<T: DeserializeOwned>(path: &Path, limit: u64) -> Result<T> {
    primary(path, limit)?.context("Required keepalive evidence is missing")
}

// Read primary account/config files directly: Manager::store/receipt may restore
// .bak files. This observer must never rewrite authentication or quota evidence.
fn identity(manager: &Manager, id: &str, dir: &Path) -> Result<Identity> {
    uuid(id)?;
    let profile = manager.profile(id)?;
    ensure!(
        dir == profile || dir == manager.live_dir,
        "Unrecognized keepalive directory"
    );
    ordinary(&profile)?;
    ordinary(&profile.join(".claude.json"))?;
    let store: crate::claude::ClaudeStore =
        required(&manager.root.join("accounts.json"), MAX_INPUT)?;
    ensure!(
        store.provider == "claude" && store.version == 1,
        "Invalid Claude store"
    );
    let mut accounts = store.accounts.iter().filter(|a| a.id == id);
    let account = accounts.next().context("Keepalive account was removed")?;
    ensure!(
        accounts.next().is_none(),
        "Duplicate keepalive account identity"
    );
    let expected = account
        .account_uuid
        .as_deref()
        .context("Missing Claude account UUID")?;
    uuid(expected)?;
    ensure!(
        store
            .accounts
            .iter()
            .filter(|a| a.account_uuid.as_deref() == Some(expected))
            .count()
            == 1,
        "Duplicate Claude account UUID"
    );
    let saved: Value = required(&profile.join(".claude.json"), MAX_INPUT)?;
    ensure!(
        saved
            .pointer("/oauthAccount/accountUuid")
            .and_then(Value::as_str)
            == Some(expected),
        "Claude profile UUID mismatch"
    );
    let config = if dir == profile {
        profile.join(".claude.json")
    } else {
        manager.live_config.clone()
    };
    let live: Value = required(&config, MAX_INPUT)?;
    ensure!(
        live.pointer("/oauthAccount/accountUuid")
            .and_then(Value::as_str)
            == Some(expected),
        "Claude live UUID mismatch"
    );
    for name in ["relogin-barrier.json", "relogin-barrier.json.bak"] {
        ensure!(
            !profile.join(name).try_exists()?,
            "Claude login lineage is pending"
        );
    }
    // Check live token attribution without persisting or emitting token material.
    if dir != profile {
        let saved: Value = required(&profile.join(".credentials.json"), MAX_INPUT)?;
        let live: Value = required(&dir.join(".credentials.json"), MAX_INPUT)?;
        for key in ["accessToken", "refreshToken"] {
            let token = saved["claudeAiOauth"][key]
                .as_str()
                .filter(|v| !v.is_empty());
            ensure!(
                token.is_some() && token == live["claudeAiOauth"][key].as_str(),
                "Claude live credential attribution mismatch"
            );
        }
    }
    let canonical = profile.canonicalize()?;
    ensure!(
        canonical == manager.root.canonicalize()?.join("profiles").join(id),
        "Redirected Claude profile directory"
    );
    Ok(Identity {
        profile,
        directory: dir.canonicalize()?,
        uuid: expected.into(),
    })
}

fn state_dir(profile: &Path) -> Result<PathBuf> {
    let plugin = profile.join(PLUGIN);
    ordinary(&plugin)?;
    let state = plugin.join("state");
    ordinary(&state)?;
    ensure!(state.is_dir(), "Keepalive plugin has not been prepared");
    Ok(state)
}

fn open_lock(path: &Path) -> Result<File> {
    ordinary(path)?;
    Ok(OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?)
}

fn ledger_lock(state: &Path) -> Result<File> {
    let lock = open_lock(&state.join("ledger.lock"))?;
    lock.lock()?;
    Ok(lock)
}

fn validate_anchor(anchor: &Anchor, now: i64) -> Result<()> {
    ensure!(
        !anchor.model.is_empty()
            && anchor.model.len() <= 256
            && anchor
                .model
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.:/".contains(&b)),
        "Invalid model"
    );
    ensure!(
        matches!(anchor.ttl, 300 | 3600)
            && anchor.at >= 0
            && anchor.at <= now
            && anchor.at.checked_add(anchor.ttl) == Some(anchor.expiry),
        "Invalid cache anchor"
    );
    ensure!(
        anchor.cache_read_tokens.is_none_or(|n| n >= 0)
            && anchor.output_tokens.is_none_or(|n| n >= 0),
        "Invalid measured tokens"
    );
    Ok(())
}

fn status_matches(
    session: &Session,
    session_id: &str,
    status: &PromptCacheStatus,
    now: i64,
) -> bool {
    status.session_id.as_deref() == Some(session_id)
        && from_status(status, None, now).is_ok()
        && session.anchor.as_ref().is_some_and(|anchor| {
            status.model.as_deref() == Some(anchor.model.as_str())
                && status.observed_at == Some(anchor.at)
                && status.expires_at == Some(anchor.expiry)
                && status.ttl_seconds == Some(anchor.ttl)
        })
}

fn load(state: &Path, id: &str, expected: &str, now: i64) -> Result<Ledger> {
    let ledger: Ledger = required(&state.join(LEDGER), MAX_FILE)?;
    ensure!(
        ledger.version == 1 && ledger.account_id == id && ledger.account_uuid == expected,
        "Keepalive ledger identity mismatch"
    );
    ensure!(
        ledger.clock >= 0 && ledger.clock <= now,
        "Keepalive clock moved backwards"
    );
    ensure!(
        ledger.attempts.len() <= MAX_REQUESTS && ledger.sessions.len() <= MAX_SESSIONS,
        "Keepalive ledger bounds exceeded"
    );
    ensure!(
        ledger
            .attempts
            .iter()
            .all(|at| *at >= 0 && *at <= ledger.clock)
            && ledger.attempts.windows(2).all(|pair| pair[0] <= pair[1]),
        "Invalid keepalive budget"
    );
    for (id, session) in &ledger.sessions {
        uuid(id)?;
        ensure!(session.directory.is_absolute(), "Invalid session directory");
        for at in [
            Some(session.started_at),
            session.human_at,
            session.invalidated_at,
            session.compacted_at,
            session.ended_at,
            session.heartbeat_at,
            session.last_at,
            session.last_anchor_at,
            session.idle_at,
        ] {
            ensure!(
                at.is_none_or(|at| at >= 0 && at <= ledger.clock),
                "Invalid session timestamp"
            );
        }
        if let Some(monitor) = &session.monitor {
            uuid(monitor)?;
        }
        for prompt in [&session.live_prompt_id, &session.live_stop_prompt_id]
            .into_iter()
            .flatten()
        {
            uuid(prompt)?;
        }
        ensure!(
            !session.ended || session.ended_at.is_some(),
            "Missing terminal timestamp"
        );
        ensure!(
            session.live_process_created.is_none_or(|at| at > 0),
            "Invalid live process identity"
        );
        if let Some(model) = &session.last_model {
            ensure!(
                !model.is_empty()
                    && model.len() <= 256
                    && model
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_.:/".contains(&b)),
                "Invalid confirmed model"
            );
        }
        if let Some(anchor) = &session.anchor {
            validate_anchor(anchor, now)?;
        }
        if let Some(pending) = &session.pending {
            validate_anchor(&pending.previous, now)?;
            ensure!(
                pending.queued_at >= 0
                    && pending.queued_at <= ledger.clock
                    && pending.human_generation == session.human_generation
                    && pending
                        .stop_at
                        .is_none_or(|at| at >= pending.queued_at && at <= ledger.clock),
                "Invalid pending keepalive request"
            );
        }
        ensure!(
            session.cache_read_tokens.is_none_or(|n| n >= 0)
                && session.output_tokens.is_none_or(|n| n >= 0),
            "Invalid keepalive metrics"
        );
    }
    Ok(ledger)
}

fn save(state: &Path, ledger: &Ledger) -> Result<()> {
    let bytes = serde_json::to_vec(ledger)?;
    ensure!(bytes.len() as u64 <= MAX_FILE, "Keepalive ledger is full");
    ordinary(&state.join(LEDGER))?;
    ordinary(&state.join("ledger.json.bak"))?;
    crate::auth::persistence::atomic_write(&state.join(LEDGER), &bytes)
}

fn enabled(manager: &Manager, id: &str) -> Result<bool> {
    crate::prompt_cache::preference(
        manager.root.parent().context("Missing Switcher root")?,
        "claude",
        id,
        None,
    )
}

// Git Bash is required by the official Monitor tool on native Windows. The
// native helper needs no interpreter dependency; only the monitor command uses
// the shell provided by Claude itself. Single-quote literals defeat expansion.
fn shell_quote(value: &str) -> Result<String> {
    ensure!(!value.chars().any(char::is_control), "Invalid command path");
    Ok(format!("'{}'", value.replace('\'', "'\\''")))
}

fn plugin_documents(helper: &Path, id: &str, dir: &Path) -> Result<(Value, Value)> {
    let helper = helper.to_str().context("Non-Unicode helper path")?;
    let directory = dir.to_str().context("Non-Unicode Claude directory")?;
    let command = format!(
        "{} --prompt-cache-monitor {} {} \"${{CLAUDE_SESSION_ID}}\"",
        shell_quote(&helper.replace('\\', "/"))?,
        shell_quote(id)?,
        shell_quote(&directory.replace('\\', "/"))?
    );
    let manifest = json!({
        "name": format!("switcher-cache-{id}"), "version": "1.0.0",
        "description": "Opt-in same-session prompt cache keepalive",
        "experimental": {"monitors": [{"name":"cache-keepalive", "command":command,
            "description":"Same-session cache keepalive", "when":"always"}]}
    });
    let mut hooks = serde_json::Map::new();
    for event in [
        "SessionStart",
        "UserPromptSubmit",
        "Stop",
        "StopFailure",
        "PreCompact",
        "SessionEnd",
    ] {
        hooks.insert(
            event.into(),
            json!([{"hooks":[{"type":"command", "command":helper,
            "args":["--prompt-cache-hook",id,directory], "timeout":10}]}]),
        );
    }
    Ok((manifest, json!({"hooks":hooks})))
}

/// Creates only owned plugin files. The parent attaches the returned directory
/// with --plugin-dir when opening the managed interactive Claude session.
pub fn prepare(manager: &Manager, id: &str, dir: &Path) -> Result<PathBuf> {
    prepare_with_helper(manager, id, dir, cached_helper()?)
}

fn prepare_with_helper(manager: &Manager, id: &str, dir: &Path, helper: &Path) -> Result<PathBuf> {
    let who = identity(manager, id, dir)?;
    let plugin = who.profile.join(PLUGIN);
    ordinary(&plugin)?;
    for sub in [".claude-plugin", "hooks", "state"] {
        ordinary(&plugin.join(sub))?;
        fs::create_dir_all(plugin.join(sub))?;
    }
    let state = state_dir(&who.profile)?;
    let _guard = ledger_lock(&state)?;
    let now = chrono::Utc::now().timestamp();
    if !state.join(LEDGER).try_exists()? {
        ensure!(
            !state.join("ledger.json.bak").try_exists()?,
            "Missing keepalive ledger primary"
        );
        save(
            &state,
            &Ledger {
                version: 1,
                account_id: id.into(),
                account_uuid: who.uuid.clone(),
                clock: now,
                attempts: vec![],
                sessions: BTreeMap::new(),
            },
        )?;
    } else {
        load(&state, id, &who.uuid, now)?;
    }
    let (manifest, hooks) = plugin_documents(helper, id, dir)?;
    for (path, value) in [
        (plugin.join(".claude-plugin/plugin.json"), manifest),
        (plugin.join("hooks/hooks.json"), hooks),
    ] {
        ordinary(&path)?;
        let mut backup = path.as_os_str().to_os_string();
        backup.push(".bak");
        ordinary(Path::new(&backup))?;
        if primary::<Value>(&path, MAX_INPUT)?.as_ref() != Some(&value) {
            crate::auth::persistence::atomic_write(&path, &serde_json::to_vec_pretty(&value)?)?;
        }
    }
    Ok(plugin)
}

fn session_directory(manager: &Manager, id: &str, canonical: &Path) -> Result<PathBuf> {
    for known in [manager.profile(id)?, manager.live_dir.clone()] {
        ordinary(&known)?;
        if known.try_exists()? && known.canonicalize()? == canonical {
            return Ok(known);
        }
    }
    anyhow::bail!("Unrecognized persisted session directory")
}

fn live_group(helper: &Path, id: &str, dir: &Path, timer: bool) -> Result<Value> {
    let command = helper.to_str().context("Non-Unicode helper path")?;
    let directory = dir.to_str().context("Non-Unicode live directory")?;
    Ok(if timer {
        json!({"hooks":[{"type":"command","command":command,
            "args":[LIVE_TIMER,id,directory],"asyncRewake":true,"timeout":LIVE_TIMEOUT}]})
    } else {
        json!({"hooks":[{"type":"command","command":command,
            "args":[LIVE_HOOK,id,directory],"async":true,"timeout":LIVE_HOOK_TIMEOUT}]})
    })
}

fn owned_live_group(group: &Value, id: &str, dir: &Path, helper_root: &Path, timer: bool) -> bool {
    let Some(command) = group.pointer("/hooks/0/command").and_then(Value::as_str) else {
        return false;
    };
    let helper = Path::new(command);
    let Some(name) = helper.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let hash = name.strip_prefix("cache-helper-").and_then(|n| {
        if cfg!(windows) {
            n.strip_suffix(".exe")
        } else {
            Some(n)
        }
    });
    helper.parent() == Some(helper_root)
        && hash.is_some_and(|h| {
            h.len() == 64
                && h.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
        && live_group(helper, id, dir, timer).is_ok_and(|mut expected| {
            if expected == *group {
                return true;
            }
            // Migrate only the complete generated lifecycle group from the
            // earlier ten-second budget. Preserve every other user edit.
            if !timer {
                expected["hooks"][0]["timeout"] = json!(10);
                return expected == *group;
            }
            false
        })
}

fn remove_previous_live_bindings(settings: &mut Value, id: &str, dir: &Path, helper_root: &Path) {
    for event in LIVE_EVENTS {
        let Some(groups) = settings
            .pointer_mut(&format!("/hooks/{event}"))
            .and_then(Value::as_array_mut)
        else {
            continue;
        };
        groups.retain(|group| {
            let Some(previous) = group.pointer("/hooks/0/args/1").and_then(Value::as_str) else {
                return true;
            };
            previous == id
                || uuid(previous).is_err()
                || !(owned_live_group(group, previous, dir, helper_root, false)
                    || (event == "Stop"
                        && owned_live_group(group, previous, dir, helper_root, true)))
        });
    }
}

// Recognize entire generated groups exactly, including the immutable helper root.
// A user-edited group or a group containing another hook is never removed.
fn merge_live_settings(
    settings: &mut Value,
    id: &str,
    dir: &Path,
    helper_root: &Path,
    helper: Option<&Path>,
) -> Result<()> {
    let mut next = settings.clone();
    let object = next
        .as_object_mut()
        .context("Claude settings must be an object")?;
    if !object.contains_key("hooks") && helper.is_none() {
        return Ok(());
    }
    let hooks = object
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("Claude hooks must be an object")?;
    for event in LIVE_EVENTS {
        let mut removed = false;
        if let Some(groups) = hooks.get_mut(event) {
            let groups = groups
                .as_array_mut()
                .context("Claude hook event must be an array")?;
            groups.retain(|group| {
                let owned = owned_live_group(group, id, dir, helper_root, false)
                    || (event == "Stop" && owned_live_group(group, id, dir, helper_root, true));
                removed |= owned;
                !owned
            });
            if removed && groups.is_empty() {
                hooks.remove(event);
            }
        }
        if let Some(helper) = helper {
            let groups = hooks
                .entry(event)
                .or_insert_with(|| json!([]))
                .as_array_mut()
                .context("Claude hook event must be an array")?;
            groups.push(live_group(helper, id, dir, false)?);
            if event == "Stop" {
                groups.push(live_group(helper, id, dir, true)?);
            }
        }
    }
    if hooks.is_empty() {
        object.remove("hooks");
    }
    *settings = next;
    Ok(())
}

/// Missing/different live credentials are an ordinary skip; corrupt JSON, I/O
/// errors and invalid private profile evidence remain errors, never a match.
pub fn live_account_matches(manager: &Manager, id: &str) -> Result<bool> {
    let profile = manager.profile(id)?;
    let who = identity(manager, id, &profile)?;
    ordinary(&manager.live_dir)?;
    if !manager.live_dir.try_exists()? {
        return Ok(false);
    }
    let Some(config) = primary::<Value>(&manager.live_config, MAX_INPUT)? else {
        return Ok(false);
    };
    if config
        .pointer("/oauthAccount/accountUuid")
        .and_then(Value::as_str)
        != Some(who.uuid.as_str())
    {
        return Ok(false);
    }
    let saved: Value = required(&profile.join(".credentials.json"), MAX_INPUT)?;
    let Some(live) = primary::<Value>(&manager.live_dir.join(".credentials.json"), MAX_INPUT)?
    else {
        return Ok(false);
    };
    for key in ["accessToken", "refreshToken"] {
        let expected = saved["claudeAiOauth"][key]
            .as_str()
            .filter(|s| !s.is_empty());
        if expected.is_none() || expected != live["claudeAiOauth"][key].as_str() {
            return Ok(false);
        }
    }
    identity(manager, id, &manager.live_dir)?;
    Ok(true)
}

/// Reconcile only this account's private profile and its strictly matching live
/// directory. No CLI, login or model request is started by this wrapper.
pub fn install_for_account(manager: &Manager, id: &str, enabled: bool) -> Result<()> {
    let profile = manager.profile(id)?;
    identity(manager, id, &profile)?;
    if enabled {
        crate::claude::install_cache_observation(manager, id, false)?;
    }
    install_live_hooks(manager, id, &profile, enabled)?;
    if live_account_matches(manager, id)?
        && manager.live_dir.canonicalize()? != profile.canonicalize()?
    {
        if enabled {
            crate::claude::install_cache_observation(manager, id, true)?;
        }
        install_live_hooks(manager, id, &manager.live_dir, enabled)?;
    }
    Ok(())
}

/// Merge/remove only exact owned groups, under runtime::install_bridge's lock.
/// This is called by the parent integration; it is not an attachment API.
pub fn install_live_hooks(manager: &Manager, id: &str, dir: &Path, enabled: bool) -> Result<()> {
    let who = identity(manager, id, dir)?;
    // Activation uses credentials -> settings ordering. Keep a live settings
    // write on the same account generation throughout, including revalidation.
    let _credentials = if dir == manager.live_dir {
        Some(crate::claude::LiveSettingsCredentialsGuard::acquire(dir)?)
    } else {
        None
    };
    let lock = dir.join("switcher-settings.lock");
    ordinary(&lock)?;
    let _guard = crate::auth::persistence::lock_file(&lock)?;
    // Revalidate after taking the settings lock; never apply to a different account.
    identity(manager, id, dir)?;
    let path = dir.join("settings.json");
    let previous = primary::<Value>(&path, MAX_INPUT)?.unwrap_or_else(|| json!({}));
    let helper = if enabled {
        Some(cached_helper()?)
    } else {
        None
    };
    let helper_root = manager
        .root
        .parent()
        .context("Missing Switcher root")?
        .join("cache-runtime");
    let mut next = previous.clone();
    if enabled && dir == manager.live_dir {
        // This shared directory has exactly one verified current owner. Remove
        // only our complete generated groups from earlier account generations.
        remove_previous_live_bindings(&mut next, id, dir, &helper_root);
    }
    merge_live_settings(&mut next, id, dir, &helper_root, helper)?;
    if enabled {
        let state = who.profile.join(PLUGIN).join("state");
        if state.is_dir() && state.join(LEDGER).try_exists()? {
            let state = state_dir(&who.profile)?;
            let _ledger = ledger_lock(&state)?;
            load(&state, id, &who.uuid, chrono::Utc::now().timestamp())?;
        } else {
            // Keep plugin documents scoped to the private managed profile. Live
            // hooks share only its ledger/helper and never load the plugin.
            prepare_with_helper(manager, id, &who.profile, helper.unwrap())?;
        }
    }
    if next != previous {
        ordinary(&path.with_file_name("settings.json.bak"))?;
        crate::auth::persistence::atomic_write(&path, &serde_json::to_vec_pretty(&next)?)?;
    }
    Ok(())
}

#[derive(Deserialize)]
struct Hook {
    hook_event_name: String,
    session_id: String,
    #[serde(default)]
    agent_id: Option<Value>,
    #[serde(default)]
    stop_hook_active: bool,
    #[serde(default)]
    background_tasks: Option<Vec<Value>>,
    #[serde(default)]
    session_crons: Option<Vec<Value>>,
    #[serde(default)]
    prompt_id: Option<String>,
    // Read transiently to exclude this adapter's own reminder; never persisted.
    #[serde(default)]
    prompt: Option<String>,
}

fn live_runtime_markers(
    hook: &Hook,
    entrypoint: Option<&str>,
    attended: Option<&str>,
    session: Option<&str>,
    child: Option<&str>,
    claudecode: Option<&str>,
) -> bool {
    entrypoint == Some("cli")
        && attended == Some("1")
        && child == Some("1")
        && claudecode == Some("1")
        && session == Some(hook.session_id.as_str())
        && hook.agent_id.as_ref().is_none_or(Value::is_null)
}

fn interactive_cli_args(arguments: &[String]) -> bool {
    !arguments.is_empty()
        && !arguments.iter().skip(1).any(|arg| {
            arg == "--print"
                || arg.starts_with("--print=")
                || (arg.starts_with('-') && !arg.starts_with("--") && arg.contains('p'))
                || matches!(
                    arg.as_str(),
                    "--bg"
                        | "--background"
                        | "--cloud"
                        | "--desktop"
                        | "--input-format"
                        | "--output-format"
                        | "--environment"
                )
                || arg.starts_with("--input-format=")
                || arg.starts_with("--output-format=")
                || arg.starts_with("--bg=")
                || arg.starts_with("--background=")
        })
}

#[cfg(windows)]
fn windows_arguments(command: &str) -> Option<Vec<String>> {
    use std::ffi::c_void;
    #[link(name = "shell32")]
    unsafe extern "system" {
        fn CommandLineToArgvW(command: *const u16, count: *mut i32) -> *mut *mut u16;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LocalFree(memory: *mut c_void) -> *mut c_void;
    }
    if command.contains('\0') || command.len() > 32768 {
        return None;
    }
    let wide: Vec<u16> = command.encode_utf16().chain(Some(0)).collect();
    let mut count = 0;
    // Windows owns this allocation; no process or configuration is modified.
    unsafe {
        let argv = CommandLineToArgvW(wide.as_ptr(), &mut count);
        if argv.is_null() {
            return None;
        }
        let result = if (1..=4096).contains(&count) {
            let mut result = Vec::new();
            for pointer in std::slice::from_raw_parts(argv, count as usize) {
                let mut len = 0;
                while *pointer.add(len) != 0 {
                    len += 1;
                }
                result.push(String::from_utf16_lossy(std::slice::from_raw_parts(
                    *pointer, len,
                )));
            }
            Some(result)
        } else {
            None
        };
        LocalFree(argv.cast());
        result
    }
}

// There is no documented interactive boolean in hook input. Require both the
// 2.1.296 runtime's markers AND the actual exec-form helper parent's invocation.
// Native source: UVe sets session/attended/PID; T maps headless cli to sdk-cli.
// Missing, inaccessible, non-Windows or ambiguous evidence always skips a wake.
#[cfg(windows)]
pub(crate) fn interactive_parent() -> Result<Option<u64>> {
    use std::{
        os::windows::process::CommandExt,
        process::{Command, Stdio},
    };
    let Some(pid) = std::env::var("CLAUDE_PID")
        .ok()
        .and_then(|p| p.parse::<u32>().ok())
        .filter(|p| *p > 0 && *p != std::process::id())
    else {
        return Ok(None);
    };
    let shell = std::env::var_os("ProgramFiles")
        .map(PathBuf::from)
        .map(|p| p.join("PowerShell/7/pwsh.exe"))
        .filter(|p| p.is_file())
        // OS-bundled management shell is a hard Windows compatibility fallback.
        .or_else(|| {
            std::env::var_os("SystemRoot")
                .map(PathBuf::from)
                .map(|p| p.join("System32/WindowsPowerShell/v1.0/powershell.exe"))
                .filter(|p| p.is_file())
        });
    let Some(shell) = shell else {
        return Ok(None);
    };
    let script = format!(
        "$ErrorActionPreference='Stop'; $h=Get-CimInstance Win32_Process -Filter 'ProcessId={child}'; \
         if($null -eq $h -or $h.ParentProcessId -ne {pid}){{exit 0}}; \
         $p=Get-CimInstance Win32_Process -Filter 'ProcessId={pid}'; \
         if($null -eq $p -or ($p.Name -ne 'claude.exe' -and $p.Name -notmatch '^[0-9]+[.][0-9]+[.][0-9]+$') -or $p.CommandLine.Length -gt 32768){{exit 0}}; \
         @{{pid=$p.ProcessId;command=$p.CommandLine;created=[string]$h.CreationDate.ToFileTimeUtc()}} | ConvertTo-Json -Compress",
        child=std::process::id());
    let mut child = match Command::new(shell)
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &script,
        ])
        .creation_flags(0x08000000)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return Ok(None),
    };
    let output = child.stdout.take().context("Missing parent probe stdout")?;
    let reader = thread::spawn(move || read_bytes(output, MAX_INPUT));
    // Cold PowerShell/CIM startup on a busy host can exceed three seconds.
    // Leave room for the bounded ledger wait within the lifecycle-hook budget.
    // No guard is
    // bypassed and only this helper's own read-only probe may be terminated.
    let deadline = Instant::now() + Duration::from_secs(LIVE_PARENT_PROBE_SECONDS);
    let success = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            _ => {
                // This is only our just-created read-only diagnostic child.
                let _ = child.kill();
                let _ = child.wait();
                return Ok(None);
            }
        }
    };
    let bytes = match reader.join() {
        Ok(Ok(bytes)) if success => bytes,
        _ => return Ok(None),
    };
    let proof: Value = match serde_json::from_slice(&bytes) {
        Ok(proof) => proof,
        Err(_) => return Ok(None),
    };
    if proof["pid"].as_u64() != Some(u64::from(pid)) {
        return Ok(None);
    }
    let Some(command) = proof["command"].as_str() else {
        return Ok(None);
    };
    if !windows_arguments(command).is_some_and(|args| interactive_cli_args(&args)) {
        return Ok(None);
    }
    Ok(proof["created"]
        .as_str()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|t| *t > 0))
}

#[cfg(not(windows))]
pub(crate) fn interactive_parent() -> Result<Option<u64>> {
    Ok(None)
}

fn direct_environment(manager: &Manager, dir: &Path, configured: Option<&Path>) -> Result<()> {
    let canonical = dir.canonicalize()?;
    if let Some(configured) = configured {
        ensure!(
            configured.canonicalize()? == canonical,
            "Claude config environment mismatch"
        );
    } else {
        ensure!(
            manager.live_dir.canonicalize()? == canonical,
            "Implicit config is allowed only for the exact live directory"
        );
    }
    // Direct settings hooks intentionally do not require CLAUDE_PLUGIN_ROOT.
    Ok(())
}

fn live_hook_at(
    ledger: &mut Ledger,
    directory: &Path,
    hook: &Hook,
    now: i64,
    created_at: u64,
) -> Result<bool> {
    if hook.agent_id.as_ref().is_some_and(|v| !v.is_null()) {
        return Ok(false);
    }
    ensure!(
        LIVE_EVENTS.contains(&hook.hook_event_name.as_str()),
        "Unexpected direct hook event"
    );
    uuid(&hook.session_id)?;
    ensure!(
        created_at > 0,
        "Missing lifecycle process creation evidence"
    );
    if let Some(prompt) = &hook.prompt_id {
        uuid(prompt)?;
    }
    if hook.hook_event_name == "UserPromptSubmit"
        && hook
            .prompt
            .as_ref()
            .is_some_and(|p| p.contains(NOTIFICATION))
    {
        return Ok(false);
    }
    if !ledger.sessions.contains_key(&hook.session_id) {
        if !matches!(
            hook.hook_event_name.as_str(),
            "UserPromptSubmit" | "SessionStart"
        ) {
            return Ok(false);
        }
        ensure!(
            ledger.sessions.len() < MAX_SESSIONS,
            "Keepalive session ledger is full"
        );
        ledger
            .sessions
            .insert(hook.session_id.clone(), Session::new(directory.into(), now));
    }
    let session = &ledger.sessions[&hook.session_id];
    ensure!(
        session.directory == directory,
        "Live session directory mismatch"
    );
    if session.ended {
        return Ok(false);
    }
    // Async delivery may be reversed. The actual exec-form child creation time
    // is read from Win32_Process, not supplied by settings or inferred from Stop.
    if session
        .live_event_created_at
        .is_some_and(|at| at >= created_at)
    {
        return Ok(false);
    }
    if hook.hook_event_name == "UserPromptSubmit"
        && hook.prompt_id.is_some()
        && session.live_prompt_id == hook.prompt_id
    {
        return Ok(false);
    }
    // A delayed completion from an older prompt must not mark the current
    // response idle, clear its failure, or give its timer a Stop proof.
    if matches!(hook.hook_event_name.as_str(), "Stop" | "StopFailure")
        && session.live_prompt_id.is_some()
        && (hook.prompt_id.is_some() || hook.hook_event_name == "Stop")
        && hook.prompt_id != session.live_prompt_id
    {
        return Ok(false);
    }
    // Old ledgers latched `failed` across subsequent human inputs. Only a real,
    // newer successful main Stop for the exact currently registered prompt can
    // recover that legacy state. Cancelled requests and account budgets survive.
    if hook.hook_event_name == "Stop"
        && session.failed
        && session.failed_generation.is_none()
        && session.human_generation > 0
        && session.live_prompt_id.is_some()
        && hook.prompt_id == session.live_prompt_id
        && !hook.stop_hook_active
        && hook.background_tasks.as_ref().is_none_or(Vec::is_empty)
        && hook.session_crons.as_ref().is_none_or(Vec::is_empty)
    {
        ledger.sessions.get_mut(&hook.session_id).unwrap().failed = false;
    }
    hook_at(ledger, directory, hook, now)?;
    let session = ledger.sessions.get_mut(&hook.session_id).unwrap();
    session.live_event_created_at = Some(created_at);
    match hook.hook_event_name.as_str() {
        "UserPromptSubmit" => {
            session.live_prompt_id = hook.prompt_id.clone();
            session.live_stop_prompt_id = None;
        }
        "Stop" => {
            session.live_stop_prompt_id =
                if !session.busy && hook.prompt_id == session.live_prompt_id {
                    hook.prompt_id.clone()
                } else {
                    None
                };
        }
        _ => {
            session.live_stop_prompt_id = None;
        }
    }
    Ok(true)
}

fn live_entry_context(
    hook: &Hook,
    directory: &str,
    id: &str,
) -> Result<Option<(Manager, Identity, u64, u64)>> {
    let vars: Vec<Option<String>> = [
        "CLAUDE_CODE_ENTRYPOINT",
        "CLAUDE_CODE_SESSION_ATTENDED",
        "CLAUDE_CODE_SESSION_ID",
        "CLAUDE_CODE_CHILD_SESSION",
        "CLAUDECODE",
    ]
    .iter()
    .map(|key| std::env::var(key).ok())
    .collect();
    if !live_runtime_markers(
        hook,
        vars[0].as_deref(),
        vars[1].as_deref(),
        vars[2].as_deref(),
        vars[3].as_deref(),
        vars[4].as_deref(),
    ) {
        return Ok(None);
    }
    let Some(created_at) = interactive_parent()? else {
        return Ok(None);
    };
    #[cfg(windows)]
    let process_created = crate::claude::interactive_process_created()?;
    #[cfg(not(windows))]
    let process_created = 0;
    let manager = Manager::current()?;
    let dir = Path::new(directory);
    let configured = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from);
    direct_environment(&manager, dir, configured.as_deref())?;
    let who = identity(&manager, id, dir)?;
    Ok(Some((manager, who, created_at, process_created)))
}

// A saved conversation UUID survives `--resume`. A terminal record may be
// rebound only by a verified new interactive process and a start/human event,
// after its prior monitor releases the lease. Account attempt history survives.
fn bind_live_process(
    ledger: &mut Ledger,
    directory: &Path,
    hook: &Hook,
    process_created: u64,
    event_created: u64,
    leased: bool,
    old_process_ended: bool,
) -> Result<bool> {
    ensure!(process_created > 0, "Missing live process identity");
    if let Some(old) = ledger.sessions.get(&hook.session_id) {
        if old.ended || old_process_ended {
            if !matches!(
                hook.hook_event_name.as_str(),
                "SessionStart" | "UserPromptSubmit"
            ) || leased
                || old.live_process_created == Some(process_created)
                || old
                    .live_event_created_at
                    .is_some_and(|at| at >= event_created)
            {
                return Ok(false);
            }
            let mut next = Session::new(directory.into(), ledger.clock);
            next.human_generation = old.human_generation;
            next.requests = old.requests;
            next.last_at = old.last_at;
            next.last_anchor_at = old.last_anchor_at;
            next.last_model = old.last_model.clone();
            next.cache_read_tokens = old.cache_read_tokens;
            next.output_tokens = old.output_tokens;
            next.live_process_created = Some(process_created);
            ledger.sessions.insert(hook.session_id.clone(), next);
        } else {
            ensure!(
                old.directory == directory
                    && old
                        .live_process_created
                        .is_none_or(|at| at == process_created),
                "Another process owns this keepalive conversation"
            );
        }
    }
    Ok(true)
}

fn apply_live_hook(
    ledger: &mut Ledger,
    directory: &Path,
    hook: &Hook,
    now: i64,
    pid: u32,
    process_created: u64,
    event_created: u64,
    leased: bool,
    old_process_ended: bool,
) -> Result<bool> {
    if hook.agent_id.as_ref().is_some_and(|v| !v.is_null())
        || (hook.hook_event_name == "UserPromptSubmit"
            && hook
                .prompt
                .as_ref()
                .is_some_and(|p| p.contains(NOTIFICATION)))
    {
        return Ok(false);
    }
    if !bind_live_process(
        ledger,
        directory,
        hook,
        process_created,
        event_created,
        leased,
        old_process_ended,
    )? {
        return Ok(false);
    }
    if !live_hook_at(ledger, directory, hook, now, event_created)? {
        return Ok(false);
    }
    let session = ledger.sessions.get_mut(&hook.session_id).unwrap();
    session.live_process_created = Some(process_created);
    session.live_pid = Some(pid);
    Ok(true)
}

// A pre-fix failure could survive many newer inputs. A verified duplicate main
// submit may recover that legacy latch when a successful cache-bearing request
// is newer than every recorded lifecycle event. This is progress proof only:
// leave the conversation busy and require a subsequent exact Stop for a timer.
fn recover_legacy_progress(
    session: &mut Session,
    status: &crate::prompt_cache::PromptCacheStatus,
    now: i64,
    pid: u32,
    process_created: u64,
    event_created: u64,
    hook: &Hook,
) -> bool {
    const FILETIME_EPOCH_SECONDS: u64 = 11_644_473_600;
    let Some(previous_event) = session.live_event_created_at else {
        return false;
    };
    let Some(previous_second) = (previous_event / 10_000_000).checked_sub(FILETIME_EPOCH_SECONDS)
    else {
        return false;
    };
    if !session.failed
        || session.failed_generation.is_some()
        || session.ended
        || !session.busy
        || session.human_generation == 0
        || session.human_at.is_none()
        || session.live_pid != Some(pid)
        || session.live_process_created != Some(process_created)
        || event_created <= previous_event
        || hook.hook_event_name != "UserPromptSubmit"
        || session.live_prompt_id.is_none()
        || hook.prompt_id != session.live_prompt_id
        || hook.agent_id.as_ref().is_some_and(|v| !v.is_null())
        || hook
            .prompt
            .as_ref()
            .is_some_and(|p| p.contains(NOTIFICATION))
        || status.session_id.as_deref() != Some(hook.session_id.as_str())
    {
        return false;
    }
    let Ok(anchor) = from_status(status, None, now) else {
        return false;
    };
    if anchor.at > now
        || anchor.at <= previous_second as i64
        || now - anchor.at > HEARTBEAT
        || !after_invalidation(session, anchor.at)
    {
        return false;
    }
    session.failed = false;
    session.live_event_created_at = Some(event_created);
    session.idle_at = None;
    session.anchor = None;
    session.live_stop_prompt_id = None;
    session.cancel();
    true
}

/// Silent, async lifecycle helper. Unknown sessions can start only from a real
/// SessionStart/UserPromptSubmit event; Stop alone never supplies human evidence.
pub fn live_hook_entry(id: &str, directory: &str) -> Result<()> {
    let hook: Hook = serde_json::from_slice(&read_bytes(std::io::stdin().lock(), MAX_INPUT)?)?;
    let result = record_live_hook(id, directory, &hook);
    // Never save hook bodies, prompts, credentials or arbitrary error text.
    // A bounded receipt distinguishes a dispatched helper from installation.
    if let Ok(manager) = Manager::current() {
        if let Ok(profile) = manager.profile(id) {
            if let Ok(state) = state_dir(&profile) {
                let outcome = match &result {
                    Ok(value) => *value,
                    Err(_) => "rejected",
                };
                let _ = crate::auth::persistence::atomic_write(
                    &state.join("last-live-hook.json"),
                    &serde_json::to_vec(&json!({
                        "checked_at":chrono::Utc::now().timestamp(),
                        "event":if LIVE_EVENTS.contains(&hook.hook_event_name.as_str()) { Some(hook.hook_event_name.as_str()) } else { None },
                        "session_id":if uuid(&hook.session_id).is_ok() { Some(hook.session_id.as_str()) } else { None },
                        "helper_revision":std::env::current_exe().ok().and_then(|path| {
                            let name = path.file_name()?.to_str()?;
                            let revision = name.strip_prefix("cache-helper-")?.strip_suffix(".exe")?;
                            (revision.len() == 64 && revision.bytes().all(|b| b.is_ascii_hexdigit()))
                                .then(|| revision.to_ascii_lowercase())
                        }),
                        "outcome":outcome
                    }))?,
                );
            }
        }
    }
    result.map(|_| ())
}

fn record_live_hook(id: &str, directory: &str, hook: &Hook) -> Result<&'static str> {
    let Some((manager, who, created_at, process_created)) =
        live_entry_context(hook, directory, id)?
    else {
        return Ok(
            if live_runtime_markers(
                hook,
                std::env::var("CLAUDE_CODE_ENTRYPOINT").ok().as_deref(),
                std::env::var("CLAUDE_CODE_SESSION_ATTENDED")
                    .ok()
                    .as_deref(),
                std::env::var("CLAUDE_CODE_SESSION_ID").ok().as_deref(),
                std::env::var("CLAUDE_CODE_CHILD_SESSION").ok().as_deref(),
                std::env::var("CLAUDECODE").ok().as_deref(),
            ) {
                "parent_probe_unconfirmed"
            } else {
                "runtime_markers_unconfirmed"
            },
        );
    };
    let state = state_dir(&who.profile)?;
    let pid: u32 = std::env::var("CLAUDE_PID")?.parse()?;
    let deadline = Instant::now() + Duration::from_secs(LIVE_LEDGER_WAIT_SECONDS);
    loop {
        let Some(_guard) = try_ledger_lock(&state)? else {
            if Instant::now() >= deadline {
                return Ok("ledger_busy");
            }
            thread::sleep(Duration::from_millis(100));
            continue;
        };
        let fresh = identity(&manager, id, Path::new(directory))?;
        ensure!(
            fresh.uuid == who.uuid && fresh.directory == who.directory,
            "Live hook account changed while waiting"
        );
        let now = chrono::Utc::now().timestamp();
        let mut ledger = load(&state, id, &who.uuid, now)?;
        ledger.clock = now;
        let old = ledger.sessions.get(&hook.session_id);
        #[cfg(windows)]
        let old_process_ended = old
            .and_then(|session| session.live_pid.zip(session.live_process_created))
            .is_some_and(|(old_pid, created)| {
                crate::claude::interactive_process_ended(old_pid, created)
            });
        #[cfg(not(windows))]
        let old_process_ended = false;
        let leased = lease_live(&state, &hook.session_id)?;
        if leased
            && old.is_some_and(|s| s.ended || old_process_ended)
            && matches!(
                hook.hook_event_name.as_str(),
                "SessionStart" | "UserPromptSubmit"
            )
        {
            if Instant::now() >= deadline {
                return Ok("previous_monitor_busy");
            }
            drop(_guard);
            thread::sleep(Duration::from_millis(100));
            continue;
        }
        if !ledger.sessions.contains_key(&hook.session_id)
            && matches!(
                hook.hook_event_name.as_str(),
                "SessionStart" | "UserPromptSubmit"
            )
        {
            reclaim(&state, &mut ledger, now)?;
        }
        let applied = apply_live_hook(
            &mut ledger,
            &who.directory,
            hook,
            now,
            pid,
            process_created,
            created_at,
            leased,
            old_process_ended,
        )?;
        let recovered = !applied
            && ledger.sessions.get_mut(&hook.session_id).is_some_and(|s| {
                recover_legacy_progress(
                    s,
                    &crate::prompt_cache::read_status(&who.profile, now),
                    now,
                    pid,
                    process_created,
                    created_at,
                    hook,
                )
            });
        ledger.clock = now;
        save(&state, &ledger)?;
        return Ok(if recovered {
            "recovered_legacy_progress"
        } else if applied {
            "registered"
        } else {
            "ignored_event"
        });
    }
}

fn hook_at(ledger: &mut Ledger, directory: &Path, hook: &Hook, now: i64) -> Result<()> {
    // agent_type may name a custom MAIN agent; only agent_id denotes a child.
    if hook.agent_id.as_ref().is_some_and(|v| !v.is_null()) {
        return Ok(());
    }
    uuid(&hook.session_id)?;
    if hook.hook_event_name == "SessionStart" && !ledger.sessions.contains_key(&hook.session_id) {
        ensure!(
            ledger.sessions.len() < MAX_SESSIONS,
            "Keepalive session ledger is full"
        );
        ledger
            .sessions
            .insert(hook.session_id.clone(), Session::new(directory.into(), now));
    }
    let session = ledger
        .sessions
        .get_mut(&hook.session_id)
        .context("Unknown keepalive session")?;
    ensure!(
        session.directory == directory,
        "Keepalive session directory mismatch"
    );
    if session.ended {
        return Ok(());
    }
    match hook.hook_event_name.as_str() {
        "SessionStart" => {
            // Reload/resume never resets budget, pending attempts or terminal failure.
            session.busy = true;
            session.idle_at = None;
            session.anchor = None;
            session.invalidated_at = Some(now);
            session.cancel();
        }
        "UserPromptSubmit" => {
            session.human_generation = session
                .human_generation
                .checked_add(1)
                .context("Human counter overflow")?;
            session.human_at = Some(now);
            session.failed = false;
            session.failed_generation = None;
            session.invalidated_at = Some(now);
            session.busy = true;
            session.idle_at = None;
            session.anchor = None;
            session.pending = None;
        }
        "Stop" => {
            session.busy = hook.stop_hook_active
                || hook
                    .background_tasks
                    .as_ref()
                    .is_some_and(|v| !v.is_empty())
                || hook.session_crons.as_ref().is_some_and(|v| !v.is_empty());
            if !session.busy {
                session.idle_at = Some(now);
                if let Some(pending) = &mut session.pending {
                    pending.stop_at = Some(now);
                }
                session.confirm(now);
            } else {
                session.idle_at = None;
            }
        }
        "PreCompact" => {
            session.busy = true;
            session.idle_at = None;
            session.invalidated_at = Some(now);
            session.compacted_at = Some(now);
            session.anchor = None;
            session.cancel();
        }
        "StopFailure" => {
            session.failed = true;
            session.failed_generation = Some(session.human_generation);
            session.busy = true;
            session.cancel();
        }
        "SessionEnd" => {
            session.ended = true;
            session.ended_at = Some(now);
            session.busy = true;
            session.cancel();
        }
        _ => anyhow::bail!("Unexpected keepalive hook event"),
    }
    ledger.clock = now;
    Ok(())
}

fn entry_environment(manager: &Manager, id: &str, dir: &Path, session: Option<&str>) -> Result<()> {
    let config =
        std::env::var_os("CLAUDE_CONFIG_DIR").context("Missing managed Claude config directory")?;
    ensure!(
        Path::new(&config).canonicalize()? == dir.canonicalize()?,
        "Claude config environment mismatch"
    );
    let plugin =
        std::env::var_os("CLAUDE_PLUGIN_ROOT").context("Missing official plugin environment")?;
    ensure!(
        Path::new(&plugin).canonicalize()? == manager.profile(id)?.join(PLUGIN).canonicalize()?,
        "Claude plugin environment mismatch"
    );
    if let Some(session) = session {
        uuid(session)?;
        ensure!(
            std::env::var("CLAUDE_SESSION_ID").ok().as_deref() == Some(session),
            "Missing or mismatched official monitor session UUID"
        );
    }
    Ok(())
}

/// Success is deliberately silent: no hook output can inject a prompt or block Stop.
pub fn hook_entry(id: &str, directory: &str) -> Result<()> {
    let hook: Hook = serde_json::from_slice(&read_bytes(std::io::stdin().lock(), MAX_INPUT)?)?;
    if hook.agent_id.as_ref().is_some_and(|v| !v.is_null()) {
        return Ok(());
    }
    let manager = Manager::current()?;
    let dir = Path::new(directory);
    entry_environment(&manager, id, dir, None)?;
    let who = identity(&manager, id, dir)?;
    let state = state_dir(&who.profile)?;
    let _guard = ledger_lock(&state)?;
    let now = chrono::Utc::now().timestamp();
    let mut ledger = load(&state, id, &who.uuid, now)?;
    if hook.hook_event_name == "SessionStart" && !ledger.sessions.contains_key(&hook.session_id) {
        reclaim(&state, &mut ledger, now)?;
    }
    hook_at(&mut ledger, &who.directory, &hook, now)?;
    save(&state, &ledger)
}

fn from_status(status: &PromptCacheStatus, output_tokens: Option<i64>, now: i64) -> Result<Anchor> {
    ensure!(
        status.state == "observed" && status.ttl_source.as_deref() == Some(TTL_SOURCE),
        "No official scoped TTL observation"
    );
    uuid(
        status
            .session_id
            .as_deref()
            .context("Missing cache session")?,
    )?;
    let anchor = Anchor {
        model: status.model.clone().context("Missing cache model")?,
        at: status.observed_at.context("Missing cache request anchor")?,
        expiry: status.expires_at.context("Missing explicit cache expiry")?,
        ttl: status.ttl_seconds.context("Missing official TTL")?,
        cache_read_tokens: status.cache_read_tokens,
        output_tokens,
    };
    validate_anchor(&anchor, now)?;
    ensure!(
        status.cache_read_tokens.is_some_and(|n| n > 0)
            || status.cache_write_tokens.is_some_and(|n| n > 0),
        "No measured cache usage"
    );
    Ok(anchor)
}

fn after_invalidation(session: &Session, at: i64) -> bool {
    // Seconds are the official precision. A human turn's anchor may equal its
    // submit hook's second; scheduling still requires a subsequent main Stop.
    // A compaction's equal-second prefix is ambiguous and never accepted.
    !session.compacted_at.is_some_and(|compact| at <= compact)
        && session.invalidated_at.is_none_or(|invalidated| {
            at > invalidated || (at == invalidated && session.human_at == Some(at))
        })
}

fn observe_at(session: &mut Session, anchor: Option<Anchor>, now: i64) {
    if session.ended || session.failed {
        return;
    }
    let Some(anchor) = anchor else {
        session.anchor = None;
        session.cancel();
        return;
    };
    if !after_invalidation(session, anchor.at) {
        return;
    }
    if let Some(previous) = session.anchor.clone() {
        if anchor.at < previous.at {
            return;
        }
        if anchor.model != previous.model {
            session.cancel();
            if anchor.at <= previous.at {
                session.anchor = None;
                session.invalidated_at = Some(now);
                return;
            }
        } else if anchor.at == previous.at
            && (anchor.expiry != previous.expiry || anchor.ttl != previous.ttl)
        {
            session.anchor = None;
            session.cancel();
            return;
        }
    }
    if session
        .pending
        .as_ref()
        .is_some_and(|p| p.previous.model != anchor.model)
    {
        session.cancel();
    }
    if session.last_anchor_at == Some(anchor.at)
        && session.last_model.as_deref() == Some(anchor.model.as_str())
        && session
            .anchor
            .as_ref()
            .is_some_and(|a| a.model == anchor.model)
    {
        // An enriched observation may provide output usage after Stop, but may
        // never manufacture a new success timestamp or a new TTL anchor.
        if anchor.output_tokens.is_some() {
            session.output_tokens = anchor.output_tokens;
        }
    }
    // A new request observed after the last Stop removes that idle proof.
    // Repeated statusline callbacks can never set busy=false.
    if session.idle_at.is_none_or(|stopped| anchor.at > stopped) {
        session.busy = true;
    }
    session.anchor = Some(anchor);
    session.confirm(now);
}

/// Only whitelisted request metadata survives collection. Output tokens are the
/// measured current request's tokens, never a guess or a transcript saved here.
pub fn observe(
    manager: &Manager,
    id: &str,
    dir: &Path,
    status: &PromptCacheStatus,
    output_tokens: Option<i64>,
) -> Result<()> {
    let who = identity(manager, id, dir)?;
    let state = state_dir(&who.profile)?;
    let _guard = ledger_lock(&state)?;
    let now = chrono::Utc::now().timestamp();
    let mut ledger = load(&state, id, &who.uuid, now)?;
    let Some(session_id) = status.session_id.as_deref() else {
        return Ok(());
    };
    uuid(session_id)?;
    let Some(session) = ledger.sessions.get_mut(session_id) else {
        return Ok(());
    };
    ensure!(
        session.directory == who.directory,
        "Cache session directory mismatch"
    );
    observe_at(session, from_status(status, output_tokens, now).ok(), now);
    ledger.clock = now;
    save(&state, &ledger)
}

fn quota(manager: &Manager, id: &str, expected: &str, now: i64) -> Result<bool> {
    let profile = manager.profile(id)?;
    if let Some(poll) = primary::<Value>(&profile.join("quota-poll.json"), MAX_FILE)? {
        ensure!(poll.is_object(), "Invalid quota evidence");
        ensure!(
            poll.get("account_uuid").and_then(Value::as_str) == Some(expected),
            "Quota UUID mismatch"
        );
        if poll
            .get("authentication_required")
            .is_some_and(|v| v != &Value::Bool(false))
            || poll.get("last_error").is_some_and(|v| !v.is_null())
        {
            return Ok(false);
        }
    }
    let Some(receipt) =
        primary::<crate::claude::UsageReceipt>(&profile.join("usage.json"), MAX_INPUT)?
    else {
        return Ok(false);
    };
    Ok(receipt.account_id == id
        && receipt.observed_at >= 0
        && receipt.observed_at <= now
        && crate::claude::storage::eligible_usage(&receipt, now)
        && receipt
            .usage
            .primary_used_percent
            .is_some_and(|p| p < 100.0)
        && receipt
            .usage
            .secondary_used_percent
            .is_some_and(|p| p < 100.0))
}

#[derive(Debug, PartialEq, Eq)]
struct Gate {
    state: &'static str,
    next_at: Option<i64>,
}

fn gate(
    session: &Session,
    attempts: &[i64],
    enabled: bool,
    live: bool,
    quota: bool,
    now: i64,
) -> Gate {
    let blocked = |state| Gate {
        state,
        next_at: None,
    };
    if !enabled {
        return blocked("disabled");
    }
    if session.ended {
        return blocked("ended");
    }
    if session.failed {
        return blocked("failed");
    }
    if !live
        || !session
            .heartbeat_at
            .is_some_and(|at| at <= now && now - at <= HEARTBEAT)
    {
        return blocked("monitor_unavailable");
    }
    let Some(human) = session.human_at else {
        return blocked("human_input_required");
    };
    if human > now || now - human >= HUMAN_IDLE {
        return blocked("human_idle_limit");
    }
    if let Some(pending) = &session.pending {
        return blocked(if pending.cancelled {
            "cancelled"
        } else {
            "awaiting_response"
        });
    }
    if session.busy || session.idle_at.is_none() || session.idle_at.is_some_and(|stop| stop < human)
    {
        return blocked("busy");
    }
    if !quota {
        return blocked("quota_unavailable");
    }
    let Some(anchor) = &session.anchor else {
        return blocked("observation_required");
    };
    if validate_anchor(anchor, now).is_err() || !after_invalidation(session, anchor.at) {
        return blocked("observation_required");
    }
    if anchor.expiry <= now {
        return blocked("expired");
    }
    if attempts.iter().filter(|at| **at > now - DAY).count() >= MAX_REQUESTS {
        return blocked("daily_budget");
    }
    let next_at = anchor.expiry - if anchor.ttl == 300 { 60 } else { 300 };
    Gate {
        state: if now >= next_at { "ready" } else { "scheduled" },
        next_at: Some(next_at),
    }
}

fn queue(ledger: &mut Ledger, session_id: &str, now: i64) -> Result<()> {
    ledger.attempts.retain(|at| *at > now - DAY);
    ensure!(
        ledger.attempts.len() < MAX_REQUESTS,
        "Keepalive daily budget exhausted"
    );
    let session = ledger
        .sessions
        .get_mut(session_id)
        .context("Unknown monitor session")?;
    ensure!(
        session.pending.is_none(),
        "Keepalive request already pending"
    );
    let previous = session
        .anchor
        .clone()
        .context("No keepalive cache anchor")?;
    session.requests = session
        .requests
        .checked_add(1)
        .context("Keepalive request counter overflow")?;
    session.pending = Some(Pending {
        queued_at: now,
        previous,
        human_generation: session.human_generation,
        stop_at: None,
        cancelled: false,
    });
    session.busy = true;
    ledger.attempts.push(now);
    ledger.clock = now;
    Ok(())
}

fn lease_path(state: &Path, session: &str) -> Result<PathBuf> {
    uuid(session)?;
    Ok(state.join(format!("monitor-{session}.lock")))
}

fn lease_live(state: &Path, session: &str) -> Result<bool> {
    let path = lease_path(state, session)?;
    if !path.try_exists()? {
        return Ok(false);
    }
    let file = open_lock(&path)?;
    match file.try_lock() {
        Ok(()) => Ok(false),
        Err(TryLockError::WouldBlock) => Ok(true),
        Err(TryLockError::Error(error)) => Err(error.into()),
    }
}

fn try_ledger_lock(state: &Path) -> Result<Option<File>> {
    let file = open_lock(&state.join("ledger.lock"))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(error)) => Err(error.into()),
    }
}

fn try_session_lease(state: &Path, session_id: &str) -> Result<Option<File>> {
    let file = open_lock(&lease_path(state, session_id)?)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(error)) => Err(error.into()),
    }
}

fn live_hooks_installed(manager: &Manager, id: &str, dir: &Path, helper: &Path) -> Result<bool> {
    let Some(settings) = primary::<Value>(&dir.join("settings.json"), MAX_INPUT)? else {
        return Ok(false);
    };
    let root = manager
        .root
        .parent()
        .context("Missing Switcher root")?
        .join("cache-runtime");
    let lifecycle = live_group(helper, id, dir, false)?;
    let timer = live_group(helper, id, dir, true)?;
    Ok(LIVE_EVENTS.iter().all(|event| {
        settings["hooks"][*event].as_array().is_some_and(|groups| {
            groups
                .iter()
                .any(|g| *g == lifecycle && owned_live_group(g, id, dir, &root, false))
                && (*event != "Stop"
                    || groups
                        .iter()
                        .any(|g| *g == timer && owned_live_group(g, id, dir, &root, true)))
        })
    }))
}

// Pure observation: inspect an installed helper; never create or hash one from
// a status getter. Installation is capability evidence, not a live connection.
fn live_hook_capability(manager: &Manager, id: &str, dir: &Path) -> Result<bool> {
    let Some(settings) = primary::<Value>(&dir.join("settings.json"), MAX_INPUT)? else {
        return Ok(false);
    };
    let Some(groups) = settings.pointer("/hooks/Stop").and_then(Value::as_array) else {
        return Ok(false);
    };
    let root = manager
        .root
        .parent()
        .context("Missing Switcher root")?
        .join("cache-runtime");
    for group in groups {
        if !owned_live_group(group, id, dir, &root, true) {
            continue;
        }
        let helper = Path::new(group["hooks"][0]["command"].as_str().unwrap());
        ordinary(helper)?;
        if helper.is_file() && live_hooks_installed(manager, id, dir, helper)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn timer_scope(session: &Session, directory: &Path, prompt: &str, generation: Option<u64>) -> bool {
    session.directory == directory
        && !session.ended
        && !session.failed
        && session.human_at.is_some()
        && session.live_prompt_id.as_deref() == Some(prompt)
        && session.live_stop_prompt_id.as_deref() == Some(prompt)
        && generation.is_none_or(|g| g == session.human_generation)
}

/// Official Stop asyncRewake helper. Only Ok(true) may be translated by the
/// parent dispatch into NOTIFICATION on stderr and exit 2. Err/Ok(false) must
/// exit 0, silently: a diagnostic error is never model feedback.
pub fn live_timer_entry(id: &str, directory: &str) -> Result<bool> {
    let hook: Hook = serde_json::from_slice(&read_bytes(std::io::stdin().lock(), MAX_INPUT)?)?;
    if hook.hook_event_name != "Stop" || hook.stop_hook_active {
        return Ok(false);
    }
    let Some(prompt) = hook.prompt_id.as_deref() else {
        return Ok(false);
    };
    uuid(prompt)?;
    let Some((manager, who, created_at, process_created)) =
        live_entry_context(&hook, directory, id)?
    else {
        return Ok(false);
    };
    let dir = Path::new(directory);
    let pid: u32 = std::env::var("CLAUDE_PID")?.parse()?;
    let state = state_dir(&who.profile)?;
    let helper = std::env::current_exe()?;
    let started = Instant::now();
    let deadline = started + Duration::from_secs(LIVE_TIMER_SECONDS);
    let start_deadline = started + Duration::from_secs(LIVE_START_GRACE);
    let monitor = uuid::Uuid::new_v4().to_string();
    let mut lease: Option<File> = None;
    let mut generation = None;
    while Instant::now() < deadline {
        let Some(guard) = try_ledger_lock(&state)? else {
            thread::sleep(Duration::from_millis(100));
            continue;
        };
        let now = chrono::Utc::now().timestamp();
        let fresh = identity(&manager, id, dir)?;
        ensure!(
            fresh.uuid == who.uuid && fresh.directory == who.directory,
            "Live timer identity changed"
        );
        if !enabled(&manager, id)? || !live_hooks_installed(&manager, id, dir, &helper)? {
            return Ok(false);
        }
        let mut ledger = load(&state, id, &who.uuid, now)?;
        let scoped = ledger.sessions.get(&hook.session_id).is_some_and(|s| {
            timer_scope(s, &who.directory, prompt, generation)
                && !s.busy
                && s.live_process_created == Some(process_created)
                && s.live_pid == Some(pid)
        });
        if !scoped {
            // Lifecycle and timer helpers run concurrently. Wait for the exact
            // Stop, but never register from Stop or revive an older generation.
            if generation.is_some() || Instant::now() >= start_deadline {
                return Ok(false);
            }
            drop(guard);
            thread::sleep(Duration::from_millis(100));
            continue;
        }
        if lease.is_none() {
            lease = try_session_lease(&state, &hook.session_id)?;
            if lease.is_none() {
                // A stale timer may be releasing its lease after a new prompt.
                // A plugin monitor holding the same lease prevents duplicates.
                if Instant::now() >= start_deadline {
                    return Ok(false);
                }
                drop(guard);
                thread::sleep(Duration::from_millis(100));
                continue;
            }
            generation = Some(ledger.sessions[&hook.session_id].human_generation);
        }
        let status = crate::prompt_cache::read_status(&who.profile, now);
        let session = ledger.sessions.get_mut(&hook.session_id).unwrap();
        // Only a successfully leased, registered, exactly scoped timer can
        // publish a heartbeat. Repeated reads never change request timestamps.
        session.heartbeat_at = Some(now);
        session.monitor = Some(monitor.clone());
        if status.session_id.as_deref() == Some(hook.session_id.as_str()) {
            observe_at(session, from_status(&status, None, now).ok(), now);
        }
        let decision = gate(
            session,
            &ledger.attempts,
            true,
            true,
            quota(&manager, id, &who.uuid, now)?,
            now,
        );
        let ready =
            decision.state == "ready" && status_matches(session, &hook.session_id, &status, now);
        ledger.clock = now;
        if ready {
            // Recheck the actual owning invocation before reserving anything.
            if interactive_parent()? != Some(created_at) {
                return Ok(false);
            }
            let at = chrono::Utc::now().timestamp();
            let fresh = identity(&manager, id, dir)?;
            let status = crate::prompt_cache::read_status(&who.profile, at);
            let current = &ledger.sessions[&hook.session_id];
            if fresh.uuid != who.uuid
                || fresh.directory != who.directory
                || !timer_scope(current, &who.directory, prompt, generation)
                || !enabled(&manager, id)?
                || !live_hooks_installed(&manager, id, dir, &helper)?
                || !status_matches(current, &hook.session_id, &status, at)
                || gate(
                    current,
                    &ledger.attempts,
                    true,
                    true,
                    quota(&manager, id, &who.uuid, at)?,
                    at,
                )
                .state
                    != "ready"
            {
                return Ok(false);
            }
            let previous = current.clone();
            let attempts = ledger.attempts.clone();
            queue(&mut ledger, &hook.session_id, at)?;
            save(&state, &ledger)?;
            // The durable write may cross expiry/settings changes. No output
            // has escaped yet, so a failed final check can safely undo only our
            // reservation while still holding the account transaction lock.
            let allowed = (|| -> Result<bool> {
                let final_identity = identity(&manager, id, dir)?;
                let final_at = chrono::Utc::now().timestamp();
                let current = &ledger.sessions[&hook.session_id];
                let status = crate::prompt_cache::read_status(&who.profile, final_at);
                Ok(final_at >= at
                    && final_identity.uuid == who.uuid
                    && final_identity.directory == who.directory
                    && timer_scope(current, &who.directory, prompt, generation)
                    && current.monitor.as_deref() == Some(monitor.as_str())
                    && enabled(&manager, id)?
                    && live_hooks_installed(&manager, id, dir, &helper)?
                    && quota(&manager, id, &who.uuid, final_at)?
                    && status_matches(current, &hook.session_id, &status, final_at)
                    && current.anchor.as_ref().is_some_and(|a| a.expiry > final_at)
                    && current
                        .human_at
                        .is_some_and(|h| h <= final_at && final_at - h < HUMAN_IDLE))
            })();
            if matches!(allowed, Ok(true)) {
                return Ok(true);
            }
            ledger.sessions.insert(hook.session_id.clone(), previous);
            ledger.attempts = attempts;
            ledger.clock = chrono::Utc::now().timestamp().max(ledger.clock);
            save(&state, &ledger)?;
            return allowed;
        }
        save(&state, &ledger)?;
        if !matches!(
            decision.state,
            "ready" | "scheduled" | "observation_required" | "quota_unavailable"
        ) {
            return Ok(false);
        }
        drop(guard);
        // Keep cancellation/heartbeats bounded without polling the CLI or API.
        thread::sleep(Duration::from_secs(5));
    }
    Ok(false)
}

fn fresh_connection(session: &Session, leased: bool, now: i64) -> bool {
    leased
        && !session.ended
        && !session.failed
        && session.monitor.is_some()
        && session
            .heartbeat_at
            .is_some_and(|at| at <= now && now - at <= HEARTBEAT)
}

fn registered_live_process(session: &Session) -> bool {
    #[cfg(windows)]
    {
        !session.ended
            && session
                .live_pid
                .zip(session.live_process_created)
                .is_some_and(|(pid, created)| {
                    crate::claude::interactive_process_matches(pid, created)
                })
    }
    #[cfg(not(windows))]
    {
        let _ = session;
        false
    }
}

#[derive(Default, Debug, PartialEq, Eq)]
struct SessionCounts {
    registered: u32,
    busy: u32,
    connected: u32,
}

impl SessionCounts {
    fn include(&mut self, session: &Session, process_verified: bool, connected: bool) {
        if session.ended {
            return;
        }
        if process_verified || connected {
            self.registered += 1;
            if session.busy {
                self.busy += 1;
            }
        }
        if connected && !session.failed {
            self.connected += 1;
        }
    }
}

// Display state only. The emission gate and leased timer admission are unchanged.
fn awaiting_timer_state(
    session: &Session,
    registered: bool,
    connected: bool,
    now: i64,
) -> Option<&'static str> {
    if !registered || connected || session.ended || session.failed {
        return None;
    }
    let Some(human) = session.human_at else {
        return Some("human_input_required");
    };
    if human > now {
        return Some("human_input_required");
    }
    if now - human >= HUMAN_IDLE {
        return Some("human_idle_limit");
    }
    if let Some(pending) = &session.pending {
        return Some(if pending.cancelled {
            "cancelled"
        } else {
            "awaiting_response"
        });
    }
    if session.busy || session.idle_at.is_none_or(|at| at < human) {
        return Some("busy");
    }
    if session.live_prompt_id.is_none() || session.live_prompt_id != session.live_stop_prompt_id {
        return Some("waiting_for_stop");
    }
    Some("timer_waiting")
}

fn session_counts(
    manager: &Manager,
    id: &str,
    state: &Path,
    ledger: &Ledger,
    now: i64,
) -> Result<SessionCounts> {
    let mut counts = SessionCounts::default();
    for (session_id, session) in &ledger.sessions {
        let process_verified = registered_live_process(session);
        let possible_connection = fresh_connection(session, true, now);
        if !process_verified && !possible_connection {
            continue;
        }
        let Ok(dir) = session_directory(manager, id, &session.directory) else {
            continue;
        };
        let Ok(who) = identity(manager, id, &dir) else {
            continue;
        };
        if who.uuid == ledger.account_uuid && who.directory == session.directory {
            let connected = possible_connection
                && fresh_connection(session, lease_live(state, session_id)?, now);
            counts.include(session, process_verified, connected);
        }
    }
    Ok(counts)
}

fn reclaimable(session: &Session, now: i64) -> bool {
    session.ended
        && session.ended_at.is_some_and(|at| at <= now - DAY)
        && session
            .pending
            .as_ref()
            .is_none_or(|p| p.cancelled && p.queued_at <= now - DAY)
}

fn reclaim(state: &Path, ledger: &mut Ledger, now: i64) -> Result<()> {
    let mut old = Vec::new();
    for (id, session) in &ledger.sessions {
        if reclaimable(session, now) && !lease_live(state, id)? {
            old.push(id.clone());
        }
    }
    for id in old {
        let path = lease_path(state, &id)?;
        ordinary(&path)?;
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        ledger.sessions.remove(&id);
    }
    // The budget remains independent of record reclamation and UTC midnight.
    ledger.attempts.retain(|at| *at > now - DAY);
    Ok(())
}

/// Blocks in the official monitor subprocess, independent of Switcher tabs.
/// Exactly one leased monitor per session; preferences are reread every 5s.
pub fn monitor_entry(id: &str, directory: &str, session_id: &str) -> Result<()> {
    let manager = Manager::current()?;
    let dir = Path::new(directory);
    entry_environment(&manager, id, dir, Some(session_id))?;
    let who = identity(&manager, id, dir)?;
    let state = state_dir(&who.profile)?;
    // Only SessionStart can register sessions. Wait briefly for its startup race.
    let mut registered = false;
    for _ in 0..6 {
        let _guard = ledger_lock(&state)?;
        let ledger = load(&state, id, &who.uuid, chrono::Utc::now().timestamp())?;
        if ledger.sessions.contains_key(session_id) {
            registered = true;
            break;
        }
        drop(_guard);
        thread::sleep(Duration::from_secs(5));
    }
    ensure!(registered, "Official SessionStart hook was not observed");
    let lease = open_lock(&lease_path(&state, session_id)?)?;
    match lease.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(()),
        Err(TryLockError::Error(error)) => return Err(error.into()),
    }
    let generation = uuid::Uuid::new_v4().to_string();
    loop {
        let _guard = ledger_lock(&state)?;
        let now = chrono::Utc::now().timestamp();
        let fresh = identity(&manager, id, dir)?;
        ensure!(
            fresh.uuid == who.uuid && fresh.directory == who.directory,
            "Monitor identity changed"
        );
        let mut ledger = load(&state, id, &who.uuid, now)?;
        let option = enabled(&manager, id)?;
        let session = ledger
            .sessions
            .get_mut(session_id)
            .context("Monitor session removed")?;
        ensure!(
            session.directory == who.directory,
            "Monitor session directory changed"
        );
        if session.ended || session.failed {
            return Ok(());
        }
        session.heartbeat_at = Some(now);
        session.monitor = Some(generation.clone());
        if !option {
            session.cancel();
        }
        // read_status retains the original request time, never the polling time.
        let status = crate::prompt_cache::read_status(&who.profile, now);
        let matches = status_matches(session, session_id, &status, now);
        let fresh_quota = quota(&manager, id, &who.uuid, now)?;
        let decision = gate(session, &ledger.attempts, option, true, fresh_quota, now);
        ledger.clock = now;
        let mut emit = matches && decision.state == "ready";
        if emit {
            // Final identity, preference, quota and file evidence check while
            // holding the account ledger lock, immediately before reserving.
            let final_identity = identity(&manager, id, dir)?;
            ensure!(
                final_identity.uuid == who.uuid && final_identity.directory == who.directory,
                "Pre-emission identity changed"
            );
            let before = chrono::Utc::now().timestamp();
            let pre_status = crate::prompt_cache::read_status(&who.profile, before);
            let option = enabled(&manager, id)?;
            let quota = quota(&manager, id, &who.uuid, before)?;
            emit = status_matches(
                &ledger.sessions[session_id],
                session_id,
                &pre_status,
                before,
            ) && gate(
                &ledger.sessions[session_id],
                &ledger.attempts,
                option,
                true,
                quota,
                before,
            )
            .state
                == "ready";
            if emit {
                queue(&mut ledger, session_id, before)?;
            }
        }
        // Persist reservation BEFORE stdout: crash/pipe failure burns an attempt
        // and leaves pending, so neither restart nor another monitor retries it.
        save(&state, &ledger)?;
        if emit {
            // Durable replacement can cross an expiry or a setting/account
            // change. Recheck at the actual emission boundary; never refund a
            // reservation whose stdout delivery might otherwise be ambiguous.
            let fresh = identity(&manager, id, dir)?;
            let at = chrono::Utc::now().timestamp();
            let session = &ledger.sessions[session_id];
            let status = crate::prompt_cache::read_status(&who.profile, at);
            let allowed = fresh.uuid == who.uuid
                && fresh.directory == who.directory
                && at >= ledger.clock
                && enabled(&manager, id)?
                && quota(&manager, id, &who.uuid, at)?
                && status_matches(session, session_id, &status, at)
                && session.anchor.as_ref().is_some_and(|a| a.expiry > at)
                && session
                    .human_at
                    .is_some_and(|human| human <= at && at - human < HUMAN_IDLE);
            if allowed {
                let mut stdout = std::io::stdout().lock();
                writeln!(stdout, "{NOTIFICATION}")?;
                stdout.flush()?;
            } else {
                ledger.sessions.get_mut(session_id).unwrap().cancel();
                ledger.clock = at.max(ledger.clock);
                save(&state, &ledger)?;
            }
        }
        drop(_guard);
        thread::sleep(Duration::from_secs(5));
    }
}

/// Adds adapter state without changing cache timing or manufacturing support.
pub fn decorate(manager: &Manager, id: &str, status: &mut PromptCacheStatus) -> Result<()> {
    status.auto_keepalive_supported = false;
    status.auto_keepalive_connected_sessions = 0;
    status.auto_keepalive_registered_sessions = 0;
    status.auto_keepalive_busy_sessions = 0;
    status.auto_keepalive_state = "monitor_unavailable".into();
    status.auto_keepalive_next_at = None;
    status.auto_keepalive_last_at = None;
    status.auto_keepalive_requests = 0;
    status.auto_keepalive_cache_read_tokens = None;
    status.auto_keepalive_output_tokens = None;
    status.auto_keepalive_reason = "관리된 대화형 세션의 모니터 연결이 필요합니다.".into();
    let option = enabled(manager, id)?;
    status.auto_keepalive_enabled = option;
    if !option {
        status.auto_keepalive_state = "disabled".into();
    }
    let profile = manager.profile(id)?;
    let who = identity(manager, id, &profile)?;
    let capability = live_hook_capability(manager, id, &profile)?
        || (live_account_matches(manager, id)?
            && live_hook_capability(manager, id, &manager.live_dir)?);
    if capability {
        status.auto_keepalive_supported = true;
        status.auto_keepalive_reason = "claude_official_live_hooks".into();
        if option {
            status.auto_keepalive_state = "awaiting_connection".into();
        }
    }
    if !profile.join(PLUGIN).join("state").is_dir() {
        return Ok(());
    }
    let state = state_dir(&profile)?;
    let _guard = ledger_lock(&state)?;
    let now = chrono::Utc::now().timestamp();
    // Account UUID is checked again through the session's exact directory.
    let expected = who.uuid.as_str();
    let ledger = load(&state, id, expected, now)?;
    let counts = session_counts(manager, id, &state, &ledger, now)?;
    status.auto_keepalive_connected_sessions = counts.connected;
    status.auto_keepalive_registered_sessions = counts.registered;
    status.auto_keepalive_busy_sessions = counts.busy;
    let Some(session_id) = status.session_id.as_deref() else {
        return Ok(());
    };
    uuid(session_id)?;
    let Some(session) = ledger.sessions.get(session_id) else {
        return Ok(());
    };
    // canonicalize is used for persisted comparison; identity accepts only the
    // manager's original profile/live path, never arbitrary paths from a ledger.
    let dir = session_directory(manager, id, &session.directory)?;
    if dir == manager.live_dir && !live_account_matches(manager, id)? {
        // History in the shared directory belongs to its previous owner. Keep
        // this account's observation and preference readable without treating
        // the new owner's process or timer as this account's connection.
        if option {
            status.auto_keepalive_state = "account_inactive".into();
            status.auto_keepalive_reason =
                "이 계정의 이전 세션은 다른 계정으로 전환됐습니다. 설정은 저장되어 있으며 이 계정의 세션이 연결되면 적용됩니다.".into();
        }
        return Ok(());
    }
    identity(manager, id, &dir)?;
    let scoped = status_matches(session, session_id, status, now);
    let live = lease_live(&state, session_id)?
        && session.monitor.is_some()
        && session
            .heartbeat_at
            .is_some_and(|at| at <= now && now - at <= HEARTBEAT);
    status.auto_keepalive_supported = capability || fresh_connection(session, live, now);
    let decision = gate(
        session,
        &ledger.attempts,
        option,
        live,
        quota(manager, id, expected, now)?,
        now,
    );
    status.auto_keepalive_state = if decision.state == "monitor_unavailable" {
        awaiting_timer_state(session, registered_live_process(session), live, now)
            .unwrap_or(decision.state)
            .into()
    } else if !scoped && matches!(decision.state, "ready" | "scheduled") {
        "observation_required".into()
    } else {
        decision.state.into()
    };
    status.auto_keepalive_next_at = if scoped { decision.next_at } else { None };
    status.auto_keepalive_requests = session.requests;
    if scoped && session.last_model.as_deref() == status.model.as_deref() {
        status.auto_keepalive_last_at = session.last_at;
        status.auto_keepalive_cache_read_tokens = session.cache_read_tokens;
        status.auto_keepalive_output_tokens = session.output_tokens;
    }
    status.auto_keepalive_reason = match status.auto_keepalive_state.as_str() {
        "monitor_unavailable" if capability && status.auto_keepalive_connected_sessions == 0 => {
            "claude_official_live_hooks"
        }
        "disabled" => "자동 캐시 유지가 꺼져 있습니다.",
        "human_input_required" => "이 세션에서 사람의 입력이 확인되어야 합니다.",
        "human_idle_limit" => "사람 입력 없이 12시간이 지나 자동 요청을 중단했습니다.",
        "busy" => "대화나 백그라운드 작업이 진행 중입니다.",
        "waiting_for_stop" => "사용자 입력은 등록됐으며 해당 응답의 완료 이벤트를 기다립니다.",
        "timer_waiting" => "세션과 응답 완료는 확인됐으며 유지 타이머 연결을 기다립니다.",
        "awaiting_response" => "유지 알림을 보냈으며 새 캐시 관측과 응답 완료를 기다립니다.",
        "cancelled" => "유지 요청이 취소되었습니다. 새 사람 입력이 필요합니다.",
        "failed" => "Claude 응답 오류 뒤 자동 유지를 보류했습니다. 새 사용자 입력과 정상 응답 완료가 확인되면 같은 세션에서 복구합니다.",
        "ended" => "이 세션은 종료되었습니다.",
        "quota_unavailable" => "신선한 사용량 증거가 없거나 인증 오류·한도 소진이 관측되었습니다.",
        "daily_budget" => "이 계정의 최근 24시간 자동 요청 한도 12회에 도달했습니다.",
        "expired" => "관측된 캐시가 만료되어 유지 요청을 보내지 않습니다.",
        "scheduled" | "ready" => "이 세션·모델의 관측된 만료시간 직전에 유지 알림을 보냅니다.",
        "observation_required" => "이 세션·모델의 새 캐시 관측이 필요합니다.",
        _ => "관리된 대화형 세션의 모니터 연결이 필요합니다.",
    }
    .into();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    const ID: &str = "e4a0bd94-6c81-49da-9167-82666e94513d";
    const ACCOUNT: &str = "a860f661-3385-494c-9fe2-d6185816506d";
    const NOW: i64 = 2_000_000_000;

    fn anchor(at: i64, ttl: i64) -> Anchor {
        Anchor {
            model: "claude-sonnet-4-6".into(),
            at,
            expiry: at + ttl,
            ttl,
            cache_read_tokens: Some(1200),
            output_tokens: Some(1),
        }
    }
    fn session() -> Session {
        let mut s = Session::new(std::env::temp_dir(), NOW - 1000);
        s.human_at = Some(NOW - 400);
        s.busy = false;
        s.idle_at = Some(NOW - 1);
        s.heartbeat_at = Some(NOW);
        s.anchor = Some(anchor(NOW - 240, 300));
        s
    }
    fn ledger() -> Ledger {
        Ledger {
            version: 1,
            account_id: ID.into(),
            account_uuid: ACCOUNT.into(),
            clock: NOW,
            attempts: vec![],
            sessions: BTreeMap::from([(ID.into(), session())]),
        }
    }
    fn hook(event: &str) -> Hook {
        serde_json::from_value(json!({"session_id":ID,"hook_event_name":event})).unwrap()
    }

    #[test]
    fn timing_is_original_anchor_and_never_pings_expired_cache() {
        let s = session();
        assert_eq!(gate(&s, &[], true, true, true, NOW).state, "ready");
        assert_eq!(
            gate(&s, &[], true, true, true, NOW - 1).state,
            "monitor_unavailable"
        );
        let mut s = s;
        s.heartbeat_at = Some(NOW + 60);
        assert_eq!(gate(&s, &[], true, true, true, NOW + 60).state, "expired");
        s.anchor = Some(anchor(NOW, 3600));
        s.heartbeat_at = Some(NOW + 3299);
        assert_eq!(
            gate(&s, &[], true, true, true, NOW + 3299).next_at,
            Some(NOW + 3300)
        );
        assert_eq!(
            gate(&s, &[], true, true, true, NOW + 3299).state,
            "scheduled"
        );
        s.heartbeat_at = Some(NOW + 3300);
        assert_eq!(gate(&s, &[], true, true, true, NOW + 3300).state, "ready");
    }

    #[test]
    fn every_eligibility_gate_fails_closed() {
        let cases: Vec<(&str, Box<dyn Fn(&mut Session)>)> = vec![
            ("human_input_required", Box::new(|s| s.human_at = None)),
            (
                "human_idle_limit",
                Box::new(|s| s.human_at = Some(NOW - HUMAN_IDLE)),
            ),
            ("busy", Box::new(|s| s.busy = true)),
            ("busy", Box::new(|s| s.idle_at = None)),
            ("ended", Box::new(|s| s.ended = true)),
            ("failed", Box::new(|s| s.failed = true)),
            (
                "monitor_unavailable",
                Box::new(|s| s.heartbeat_at = Some(NOW - HEARTBEAT - 1)),
            ),
            ("observation_required", Box::new(|s| s.anchor = None)),
            (
                "observation_required",
                Box::new(|s| s.anchor.as_mut().unwrap().ttl = 301),
            ),
        ];
        for (expected, mutate) in cases {
            let mut s = session();
            mutate(&mut s);
            assert_eq!(gate(&s, &[], true, true, true, NOW).state, expected);
        }
        let s = session();
        assert_eq!(gate(&s, &[], false, true, true, NOW).state, "disabled");
        assert_eq!(
            gate(&s, &[], true, false, true, NOW).state,
            "monitor_unavailable"
        );
        assert_eq!(
            gate(&s, &[], true, true, false, NOW).state,
            "quota_unavailable"
        );
    }

    #[test]
    fn queued_is_not_success_and_repeated_observation_does_not_retry() {
        let mut l = ledger();
        queue(&mut l, ID, NOW).unwrap();
        assert!(queue(&mut l, ID, NOW).is_err());
        let s = l.sessions.get_mut(ID).unwrap();
        s.busy = false;
        s.idle_at = Some(NOW);
        s.pending.as_mut().unwrap().stop_at = Some(NOW);
        observe_at(s, Some(anchor(NOW - 240, 300)), NOW);
        assert_eq!(s.last_at, None);
        assert_eq!(s.requests, 1);
        assert_eq!(
            gate(s, &l.attempts, true, true, true, NOW).state,
            "awaiting_response"
        );
        s.idle_at = Some(NOW + 2);
        s.pending.as_mut().unwrap().stop_at = Some(NOW + 2);
        observe_at(s, Some(anchor(NOW + 1, 300)), NOW + 2);
        assert!(s.pending.is_none());
        assert_eq!(s.last_at, Some(NOW + 1));
        assert_eq!(s.output_tokens, Some(1));
        assert_eq!(s.cache_read_tokens, Some(1200));
    }

    #[test]
    fn response_before_stop_waits_and_new_human_cancels_attribution() {
        let mut l = ledger();
        queue(&mut l, ID, NOW).unwrap();
        observe_at(
            l.sessions.get_mut(ID).unwrap(),
            Some(anchor(NOW + 1, 300)),
            NOW + 1,
        );
        assert_eq!(l.sessions[ID].last_at, None);
        let dir = l.sessions[ID].directory.clone();
        hook_at(&mut l, &dir, &hook("Stop"), NOW + 2).unwrap();
        assert_eq!(l.sessions[ID].last_at, Some(NOW + 1));
        let mut l = ledger();
        queue(&mut l, ID, NOW).unwrap();
        hook_at(&mut l, &dir, &hook("UserPromptSubmit"), NOW + 1).unwrap();
        observe_at(
            l.sessions.get_mut(ID).unwrap(),
            Some(anchor(NOW + 2, 300)),
            NOW + 2,
        );
        hook_at(&mut l, &dir, &hook("Stop"), NOW + 3).unwrap();
        assert_eq!(l.sessions[ID].last_at, None);
    }

    #[test]
    fn compaction_failure_end_and_subagents_cannot_confirm() {
        for event in ["PreCompact", "StopFailure", "SessionEnd"] {
            let mut l = ledger();
            queue(&mut l, ID, NOW).unwrap();
            let dir = l.sessions[ID].directory.clone();
            hook_at(&mut l, &dir, &hook(event), NOW + 1).unwrap();
            observe_at(
                l.sessions.get_mut(ID).unwrap(),
                Some(anchor(NOW + 2, 300)),
                NOW + 2,
            );
            hook_at(&mut l, &dir, &hook("Stop"), NOW + 3).unwrap();
            assert_eq!(l.sessions[ID].last_at, None, "{event}");
            hook_at(&mut l, &dir, &hook("SessionStart"), NOW + 4).unwrap();
            if event == "SessionEnd" {
                assert!(l.sessions[ID].ended);
            }
            if event == "StopFailure" {
                assert!(l.sessions[ID].failed);
            }
        }
        let mut l = ledger();
        l.sessions.get_mut(ID).unwrap().busy = true;
        let mut h = hook("Stop");
        h.agent_id = Some(json!("child"));
        hook_at(&mut l, &std::env::temp_dir(), &h, NOW).unwrap();
        assert!(l.sessions[ID].busy);
    }

    #[test]
    fn failed_response_recovers_on_new_human_but_requires_its_successful_stop() {
        let mut l = ledger();
        let dir = l.sessions[ID].directory.clone();
        let mut human = hook("UserPromptSubmit");
        human.prompt_id = Some(ID.into());
        live_hook_at(&mut l, &dir, &human, NOW, 10).unwrap();
        let mut failure = hook("StopFailure");
        failure.prompt_id = human.prompt_id.clone();
        live_hook_at(&mut l, &dir, &failure, NOW + 1, 20).unwrap();
        assert_eq!(l.sessions[ID].failed_generation, Some(1));
        assert_eq!(
            gate(&l.sessions[ID], &[], true, true, true, NOW + 1).state,
            "failed"
        );
        // Redelivery and a Stop for the failed input never authorize a wake.
        assert!(!live_hook_at(&mut l, &dir, &human, NOW + 2, 30).unwrap());
        let mut stop = hook("Stop");
        stop.prompt_id = human.prompt_id.clone();
        live_hook_at(&mut l, &dir, &stop, NOW + 2, 40).unwrap();
        assert!(l.sessions[ID].failed);
        assert!(!timer_scope(&l.sessions[ID], &dir, ID, Some(1)));
        l.attempts = vec![NOW - 10];
        l.sessions.get_mut(ID).unwrap().requests = 1;
        human.prompt_id = Some(ACCOUNT.into());
        live_hook_at(&mut l, &dir, &human, NOW + 3, 50).unwrap();
        assert!(!l.sessions[ID].failed);
        assert_eq!(l.sessions[ID].failed_generation, None);
        assert_eq!(l.attempts, vec![NOW - 10]);
        assert_eq!(l.sessions[ID].requests, 1);
        assert!(!live_hook_at(&mut l, &dir, &stop, NOW + 4, 60).unwrap());
        assert!(!live_hook_at(&mut l, &dir, &failure, NOW + 4, 70).unwrap());
        observe_at(
            l.sessions.get_mut(ID).unwrap(),
            Some(anchor(NOW + 4, 300)),
            NOW + 4,
        );
        assert_eq!(
            gate(&l.sessions[ID], &l.attempts, true, true, true, NOW + 4).state,
            "busy"
        );
        stop.prompt_id = human.prompt_id.clone();
        live_hook_at(&mut l, &dir, &stop, NOW + 5, 80).unwrap();
        assert!(timer_scope(&l.sessions[ID], &dir, ACCOUNT, Some(2)));
        let s = l.sessions.get_mut(ID).unwrap();
        s.heartbeat_at = Some(NOW + 5);
        assert_eq!(
            gate(s, &l.attempts, true, true, true, NOW + 5).state,
            "scheduled"
        );
        s.heartbeat_at = Some(NOW + 244);
        assert_eq!(
            gate(s, &l.attempts, true, true, true, NOW + 244).state,
            "ready"
        );
    }

    #[test]
    fn legacy_failed_live_session_recovers_only_on_exact_new_idle_main_stop() {
        let mut l = ledger();
        let dir = l.sessions[ID].directory.clone();
        let s = l.sessions.get_mut(ID).unwrap();
        s.failed = true;
        s.failed_generation = None;
        s.live_prompt_id = Some(ACCOUNT.into());
        s.live_event_created_at = Some(10);
        s.human_generation = 16;
        s.human_at = Some(NOW);
        s.busy = true;
        s.idle_at = None;
        let mut stop = hook("Stop");
        stop.prompt_id = Some(ID.into());
        assert!(!live_hook_at(&mut l, &dir, &stop, NOW + 1, 20).unwrap());
        assert!(l.sessions[ID].failed);
        stop.prompt_id = Some(ACCOUNT.into());
        stop.stop_hook_active = true;
        live_hook_at(&mut l, &dir, &stop, NOW + 1, 30).unwrap();
        assert!(l.sessions[ID].failed);
        stop.stop_hook_active = false;
        stop.agent_id = Some(json!("child"));
        assert!(!live_hook_at(&mut l, &dir, &stop, NOW + 2, 40).unwrap());
        stop.agent_id = None;
        stop.background_tasks = Some(vec![json!({"id":"running"})]);
        live_hook_at(&mut l, &dir, &stop, NOW + 2, 50).unwrap();
        assert!(l.sessions[ID].failed);
        stop.background_tasks = None;
        live_hook_at(&mut l, &dir, &stop, NOW + 3, 60).unwrap();
        assert!(!l.sessions[ID].failed);
        assert!(timer_scope(&l.sessions[ID], &dir, ACCOUNT, Some(16)));
        assert_eq!(l.sessions[ID].last_at, None);
        assert!(l.sessions[ID].anchor.is_some());
        // Ended sessions are never revived by this compatibility repair.
        l.sessions.get_mut(ID).unwrap().ended = true;
        l.sessions.get_mut(ID).unwrap().failed = true;
        assert!(!live_hook_at(&mut l, &dir, &stop, NOW + 4, 70).unwrap());
        assert!(l.sessions[ID].failed);
    }

    #[test]
    fn legacy_progress_clears_latch_without_idling_emitting_or_forgiving_attempts() {
        let event = (NOW as u64 + 11_644_473_600) * 10_000_000;
        let mut s = session();
        s.failed = true;
        s.human_generation = 16;
        s.busy = true;
        s.live_pid = Some(66088);
        s.live_process_created = Some(123);
        s.live_prompt_id = Some(ACCOUNT.into());
        s.live_event_created_at = Some(event);
        s.idle_at = None;
        let mut h = hook("UserPromptSubmit");
        h.prompt_id = Some(ACCOUNT.into());
        let mut status = crate::prompt_cache::unknown("synthetic");
        status.state = "observed".into();
        status.session_id = Some(ID.into());
        status.model = Some("claude-opus-5-5".into());
        status.observed_at = Some(NOW + 1);
        status.expires_at = Some(NOW + 3601);
        status.ttl_seconds = Some(3600);
        status.ttl_source = Some(TTL_SOURCE.into());
        status.cache_read_tokens = Some(1200);
        s.requests = 2;
        for case in 0..8 {
            let mut rejected = s.clone();
            let mut stale = status.clone();
            let mut input = hook("UserPromptSubmit");
            input.prompt_id = Some(ACCOUNT.into());
            match case {
                0 => rejected.failed_generation = Some(16),
                1 => rejected.ended = true,
                2 => stale.observed_at = Some(NOW),
                3 => stale.session_id = Some(ACCOUNT.into()),
                4 => input.prompt_id = Some(ID.into()),
                5 => input.agent_id = Some(json!("child")),
                6 => stale.observed_at = Some(NOW + 99),
                _ => rejected.live_process_created = Some(999),
            }
            assert!(
                !recover_legacy_progress(
                    &mut rejected,
                    &stale,
                    NOW + 2,
                    66088,
                    123,
                    event + 20_000_000,
                    &input
                ),
                "case {case}"
            );
            assert!(rejected.failed);
        }
        assert!(recover_legacy_progress(
            &mut s,
            &status,
            NOW + 2,
            66088,
            123,
            event + 20_000_000,
            &h
        ));
        assert!(!s.failed);
        assert!(s.busy);
        assert_eq!(s.human_generation, 16);
        assert_eq!(s.requests, 2);
        assert_eq!(s.last_at, None);
        assert!(s.anchor.is_none());
        assert_eq!(s.live_stop_prompt_id, None);
        assert!(!timer_scope(&s, &s.directory, ACCOUNT, Some(16)));
        assert_eq!(gate(&s, &[], true, true, true, NOW + 2).state, "busy");
        assert_eq!(s.live_event_created_at, Some(event + 20_000_000));
        let directory = s.directory.clone();
        let mut recovered = ledger();
        recovered.sessions.insert(ID.into(), s);
        for kind in ["Stop", "StopFailure"] {
            let mut delayed = hook(kind);
            delayed.prompt_id = Some(ACCOUNT.into());
            assert!(!live_hook_at(
                &mut recovered,
                &directory,
                &delayed,
                NOW + 3,
                event + 10_000_000
            )
            .unwrap());
            let unchanged = &recovered.sessions[ID];
            assert!(unchanged.busy);
            assert!(!unchanged.failed);
            assert_eq!(unchanged.idle_at, None);
            assert_eq!(unchanged.live_stop_prompt_id, None);
            assert_eq!(unchanged.requests, 2);
        }
    }

    #[test]
    fn budgets_are_shared_across_sessions_and_midnight() {
        let mut l = ledger();
        l.attempts = vec![NOW - 10; MAX_REQUESTS];
        assert_eq!(
            gate(&l.sessions[ID], &l.attempts, true, true, true, NOW).state,
            "daily_budget"
        );
        assert!(queue(&mut l, ID, NOW).is_err());
        l.sessions.insert(ACCOUNT.into(), session());
        l.attempts = vec![NOW - 10; MAX_REQUESTS - 1];
        queue(&mut l, ID, NOW).unwrap();
        assert!(queue(&mut l, ACCOUNT, NOW).is_err());
        l.sessions.get_mut(ID).unwrap().pending = None;
        l.attempts = vec![NOW - DAY; MAX_REQUESTS];
        queue(&mut l, ID, NOW).unwrap();
        assert_eq!(l.attempts, vec![NOW]);
    }

    #[test]
    fn plugin_uses_exec_hooks_immutable_helper_and_quoted_monitor() {
        let (manifest, hooks) = plugin_documents(
            Path::new("C:/a b/helper.exe"),
            ID,
            Path::new("C:/profile's $data"),
        )
        .unwrap();
        let command = manifest["experimental"]["monitors"][0]["command"]
            .as_str()
            .unwrap();
        assert!(command.contains("'C:/a b/helper.exe'"));
        assert!(command.contains("profile'\\''s $data"));
        assert!(command.ends_with("\"${CLAUDE_SESSION_ID}\""));
        assert_eq!(manifest["experimental"]["monitors"][0]["when"], "always");
        for entries in hooks["hooks"].as_object().unwrap().values() {
            assert_eq!(entries[0]["hooks"][0]["command"], "C:/a b/helper.exe");
            assert_eq!(entries[0]["hooks"][0]["args"][0], "--prompt-cache-hook");
        }
        assert!(shell_quote("bad\npath").is_err());
    }

    #[test]
    fn stdin_is_bounded_and_sensitive_fields_are_discarded() {
        assert!(read_bytes(&b"12345"[..], 4).is_err());
        let hook: Hook = serde_json::from_value(json!({"hook_event_name":"UserPromptSubmit",
            "session_id":ID,"prompt":"SECRET PROMPT","last_assistant_message":"SECRET RESPONSE",
            "credentials":{"accessToken":"SECRET TOKEN"}}))
        .unwrap();
        let mut l = ledger();
        hook_at(&mut l, &std::env::temp_dir(), &hook, NOW).unwrap();
        let text = serde_json::to_string(&l).unwrap();
        assert!(!text.contains("SECRET"));
        let value = serde_json::to_value(&l).unwrap();
        for field in ["prompt", "last_assistant_message", "credentials"] {
            assert!(value["sessions"][ID].get(field).is_none());
        }
    }

    #[test]
    fn live_settings_merge_is_idempotent_preserves_foreign_hooks_and_removes_only_owned_groups() {
        let root = Path::new("C:/synthetic/cache-runtime");
        let helper = root.join(format!(
            "cache-helper-{}{}",
            "a".repeat(64),
            if cfg!(windows) { ".exe" } else { "" }
        ));
        let dir = Path::new("C:/synthetic/profile");
        let foreign = json!({"hooks":[{"type":"command","command":"user command","timeout":5}]});
        let original = json!({"model":"keep", "permissions":{"deny":["Bash(*)"]},
            "hooks":{"Stop":[foreign.clone()],"CustomEvent":[foreign.clone()]}});
        let mut settings = original.clone();
        merge_live_settings(&mut settings, ID, dir, root, Some(&helper)).unwrap();
        let installed = settings.clone();
        merge_live_settings(&mut settings, ID, dir, root, Some(&helper)).unwrap();
        assert_eq!(settings, installed);
        assert_eq!(settings["hooks"]["Stop"][0], foreign);
        assert_eq!(
            settings["hooks"]["Stop"][2]["hooks"][0]["asyncRewake"],
            true
        );
        assert_eq!(settings["model"], original["model"]);
        merge_live_settings(&mut settings, ID, dir, root, None).unwrap();
        assert_eq!(settings, original);
        // A group edited by the user, or belonging to another identity, survives.
        let mut edited = live_group(&helper, ID, dir, true).unwrap();
        edited["hooks"][0]["timeout"] = json!(7);
        settings["hooks"]["Stop"]
            .as_array_mut()
            .unwrap()
            .push(edited.clone());
        merge_live_settings(&mut settings, ACCOUNT, dir, root, None).unwrap();
        assert_eq!(settings["hooks"]["Stop"][1], edited);
    }

    #[test]
    fn lifecycle_budget_replaces_exact_legacy_groups_without_touching_user_edits() {
        assert!(LIVE_HOOK_TIMEOUT > LIVE_PARENT_PROBE_SECONDS + LIVE_LEDGER_WAIT_SECONDS + 2);
        let dir = Path::new("C:/profile");
        let root = Path::new("C:/cache-runtime");
        let helper = root.join(format!(
            "cache-helper-{}{}",
            "a".repeat(64),
            if cfg!(windows) { ".exe" } else { "" }
        ));
        let mut legacy = live_group(&helper, ID, dir, false).unwrap();
        legacy["hooks"][0]["timeout"] = json!(10);
        assert!(owned_live_group(&legacy, ID, dir, root, false));
        let mut edited = legacy.clone();
        edited["hooks"][0]["timeout"] = json!(11);
        assert!(!owned_live_group(&edited, ID, dir, root, false));
        let mut settings = json!({"hooks":{"Stop":[legacy, edited.clone()]}});
        merge_live_settings(&mut settings, ID, dir, root, Some(&helper)).unwrap();
        assert_eq!(settings["hooks"]["Stop"].as_array().unwrap().len(), 3);
        assert_eq!(settings["hooks"]["Stop"][0], edited);
        assert_eq!(
            settings["hooks"]["Stop"][1]["hooks"][0]["timeout"],
            LIVE_HOOK_TIMEOUT
        );
        let once = settings.clone();
        merge_live_settings(&mut settings, ID, dir, root, Some(&helper)).unwrap();
        assert_eq!(settings, once);
    }

    #[test]
    fn malformed_live_settings_are_never_partially_rewritten() {
        for mut settings in [json!([]), json!({"hooks":[]}), json!({"hooks":{"Stop":{}}})] {
            let before = settings.clone();
            assert!(merge_live_settings(
                &mut settings,
                ID,
                Path::new("C:/profile"),
                Path::new("C:/cache-runtime"),
                Some(Path::new("C:/helper.exe"))
            )
            .is_err());
            assert_eq!(settings, before);
        }
    }

    #[test]
    fn shared_live_migration_removes_only_exact_previous_account_groups() {
        let dir = Path::new("C:/shared-live");
        let root = Path::new("C:/cache-runtime");
        let helper = root.join(format!(
            "cache-helper-{}{}",
            "a".repeat(64),
            if cfg!(windows) { ".exe" } else { "" }
        ));
        let foreign = json!({"hooks":[{"type":"command","command":"security-check"}]});
        let mut settings =
            json!({"permissions":{"deny":["Bash(*)"]},"hooks":{"Stop":[foreign.clone()]}});
        merge_live_settings(&mut settings, ACCOUNT, dir, root, Some(&helper)).unwrap();
        let mut edited = live_group(&helper, ACCOUNT, dir, true).unwrap();
        edited["hooks"][0]["timeout"] = json!(9);
        settings["hooks"]["Stop"]
            .as_array_mut()
            .unwrap()
            .push(edited.clone());
        remove_previous_live_bindings(&mut settings, ID, dir, root);
        merge_live_settings(&mut settings, ID, dir, root, Some(&helper)).unwrap();
        assert_eq!(settings["hooks"]["Stop"][0], foreign);
        assert_eq!(settings["hooks"]["Stop"][1], edited);
        assert_eq!(settings["permissions"]["deny"], json!(["Bash(*)"]));
        for event in LIVE_EVENTS {
            let groups = settings["hooks"][event].as_array().unwrap();
            assert!(!groups
                .iter()
                .any(|g| owned_live_group(g, ACCOUNT, dir, root, false)
                    || owned_live_group(g, ACCOUNT, dir, root, true)));
            assert!(groups
                .iter()
                .any(|g| owned_live_group(g, ID, dir, root, false)));
        }
        let before = settings.clone();
        remove_previous_live_bindings(&mut settings, ID, dir, root);
        merge_live_settings(&mut settings, ID, dir, root, Some(&helper)).unwrap();
        assert_eq!(settings, before);
    }

    #[test]
    fn live_attachment_requires_human_input_and_matching_stop_generation() {
        let dir = std::env::temp_dir();
        let mut l = ledger();
        l.sessions.clear();
        live_hook_at(&mut l, &dir, &hook("Stop"), NOW, 10).unwrap();
        assert!(l.sessions.is_empty());
        let mut human = hook("UserPromptSubmit");
        human.prompt_id = Some(ACCOUNT.into());
        human.prompt = Some("synthetic human input".into());
        live_hook_at(&mut l, &dir, &human, NOW, 20).unwrap();
        assert_eq!(l.sessions[ID].human_generation, 1);
        let mut stop = hook("Stop");
        stop.prompt_id = Some(ACCOUNT.into());
        live_hook_at(&mut l, &dir, &stop, NOW + 1, 30).unwrap();
        assert!(timer_scope(&l.sessions[ID], &dir, ACCOUNT, Some(1)));
        assert!(!timer_scope(&l.sessions[ID], &dir, ID, Some(1)));
        // Late old delivery and duplicate prompt events cannot cancel or revive it.
        live_hook_at(&mut l, &dir, &human, NOW + 2, 20).unwrap();
        live_hook_at(&mut l, &dir, &human, NOW + 2, 40).unwrap();
        assert_eq!(l.sessions[ID].human_generation, 1);
        human.prompt_id = Some(ID.into());
        live_hook_at(&mut l, &dir, &human, NOW + 3, 50).unwrap();
        assert!(!timer_scope(&l.sessions[ID], &dir, ACCOUNT, Some(1)));
        assert!(l.sessions[ID].busy);
    }

    #[test]
    fn resumed_ended_conversation_rebinds_without_refunding_account_budget() {
        let mut l = ledger();
        l.attempts = vec![NOW - 10];
        let old = l.sessions.get_mut(ID).unwrap();
        old.ended = true;
        old.ended_at = Some(NOW - 1);
        old.live_process_created = Some(100);
        old.live_event_created_at = Some(110);
        old.requests = 3;
        old.human_generation = 2;
        let dir = std::env::temp_dir().join("shared-live");
        let mut human = hook("UserPromptSubmit");
        human.prompt_id = Some(ACCOUNT.into());
        bind_live_process(&mut l, &dir, &human, 200, 210, false, false).unwrap();
        live_hook_at(&mut l, &dir, &human, NOW, 210).unwrap();
        let s = &l.sessions[ID];
        assert!(!s.ended);
        assert_eq!(s.directory, dir);
        assert_eq!(s.live_process_created, Some(200));
        assert_eq!(s.human_generation, 3);
        assert_eq!(s.requests, 3);
        assert_eq!(l.attempts, [NOW - 10]);
        assert!(s.anchor.is_none());
        assert!(s.monitor.is_none());
        assert!(s.heartbeat_at.is_none());
        assert!(s.busy);
        // An old process's late Stop cannot terminate the resumed conversation.
        assert!(bind_live_process(&mut l, &dir, &hook("Stop"), 100, 220, false, false).is_err());
    }

    #[test]
    fn resume_requires_new_process_start_or_human_and_released_lease() {
        for (event, process, time, leased) in [
            ("Stop", 200, 210, false),
            ("SessionStart", 100, 210, false),
            ("UserPromptSubmit", 200, 90, false),
            ("SessionStart", 200, 210, true),
        ] {
            let mut l = ledger();
            let old = l.sessions.get_mut(ID).unwrap();
            old.ended = true;
            old.ended_at = Some(NOW - 1);
            old.live_process_created = Some(100);
            old.live_event_created_at = Some(110);
            let dir = old.directory.clone();
            bind_live_process(&mut l, &dir, &hook(event), process, time, leased, false).unwrap();
            assert!(l.sessions[ID].ended, "{event}/{process}/{time}/{leased}");
        }
        let mut l = ledger();
        l.sessions.get_mut(ID).unwrap().live_process_created = Some(100);
        let dir = l.sessions[ID].directory.clone();
        assert!(
            bind_live_process(&mut l, &dir, &hook("SessionStart"), 200, 210, false, false).is_err()
        );
    }

    #[test]
    fn legacy_ended_record_can_attach_to_verified_resumed_live_process() {
        let mut l = ledger();
        let old = l.sessions.get_mut(ID).unwrap();
        old.ended = true;
        old.ended_at = Some(NOW - 1);
        old.live_process_created = None;
        let dir = std::env::temp_dir().join("shared-live");
        bind_live_process(&mut l, &dir, &hook("SessionStart"), 200, 210, false, false).unwrap();
        live_hook_at(&mut l, &dir, &hook("SessionStart"), NOW, 210).unwrap();
        assert!(!l.sessions[ID].ended);
        assert_eq!(l.sessions[ID].live_process_created, Some(200));
        assert_eq!(l.sessions[ID].human_at, None);
    }

    #[test]
    fn missing_session_end_requires_demonstrably_gone_parent_and_free_lease() {
        let dir = std::env::temp_dir().join("live-resume");
        for gone in [false, true] {
            let mut l = ledger();
            l.attempts = vec![NOW - 1];
            l.sessions.get_mut(ID).unwrap().live_process_created = Some(100);
            l.sessions.get_mut(ID).unwrap().live_pid = Some(1234);
            let result =
                bind_live_process(&mut l, &dir, &hook("SessionStart"), 200, 210, false, gone);
            assert_eq!(result.is_ok(), gone);
            if gone {
                assert_eq!(l.sessions[ID].live_process_created, Some(200));
                assert_eq!(l.attempts, [NOW - 1]);
            }
        }
        let mut l = ledger();
        l.sessions.get_mut(ID).unwrap().live_process_created = Some(100);
        let old = l.sessions[ID].directory.clone();
        bind_live_process(&mut l, &dir, &hook("SessionStart"), 200, 210, true, true).unwrap();
        assert_eq!(l.sessions[ID].directory, old);
        assert_eq!(l.sessions[ID].live_process_created, Some(100));
    }

    #[test]
    fn rejected_takeover_cannot_inherit_or_poison_prior_human_cache_state() {
        for event in ["Stop", "StopFailure", "PreCompact", "SessionEnd"] {
            for leased in [false, true] {
                let mut l = ledger();
                let old = l.sessions.get_mut(ID).unwrap();
                old.live_pid = Some(1234);
                old.live_process_created = Some(100);
                old.live_event_created_at = Some(110);
                let dir = old.directory.clone();
                let before = serde_json::to_value(&l).unwrap();
                assert!(!apply_live_hook(
                    &mut l,
                    &dir,
                    &hook(event),
                    NOW,
                    4321,
                    200,
                    210,
                    leased,
                    true,
                )
                .unwrap());
                assert_eq!(
                    serde_json::to_value(&l).unwrap(),
                    before,
                    "{event}/{leased}"
                );
            }
        }
    }

    #[test]
    fn applied_terminal_event_is_reported_once_without_rebinding() {
        let mut l = ledger();
        let old = l.sessions.get_mut(ID).unwrap();
        old.live_pid = Some(1234);
        old.live_process_created = Some(100);
        old.live_event_created_at = Some(110);
        let dir = old.directory.clone();
        assert!(apply_live_hook(
            &mut l,
            &dir,
            &hook("SessionEnd"),
            NOW,
            1234,
            100,
            120,
            false,
            false,
        )
        .unwrap());
        assert!(l.sessions[ID].ended);
        assert_eq!(l.sessions[ID].live_process_created, Some(100));
        assert!(!apply_live_hook(
            &mut l,
            &dir,
            &hook("SessionEnd"),
            NOW + 1,
            1234,
            100,
            130,
            false,
            false,
        )
        .unwrap());
        assert_eq!(l.sessions[ID].live_event_created_at, Some(120));
    }

    #[test]
    fn live_reminders_subagents_and_headless_invocations_never_bootstrap() {
        let mut l = ledger();
        l.sessions.clear();
        let mut human = hook("UserPromptSubmit");
        human.prompt = Some(NOTIFICATION.into());
        live_hook_at(&mut l, &std::env::temp_dir(), &human, NOW, 1).unwrap();
        human.prompt = Some("human".into());
        human.agent_id = Some(json!("subagent"));
        live_hook_at(&mut l, &std::env::temp_dir(), &human, NOW, 2).unwrap();
        assert!(l.sessions.is_empty());
        human.agent_id = None;
        assert!(live_runtime_markers(
            &human,
            Some("cli"),
            Some("1"),
            Some(ID),
            Some("1"),
            Some("1")
        ));
        for entrypoint in [None, Some("sdk-cli"), Some("remote")] {
            assert!(!live_runtime_markers(
                &human,
                entrypoint,
                Some("1"),
                Some(ID),
                Some("1"),
                Some("1")
            ));
        }
        assert!(!live_runtime_markers(
            &human,
            Some("cli"),
            None,
            Some(ID),
            Some("1"),
            Some("1")
        ));
        assert!(!live_runtime_markers(
            &human,
            Some("cli"),
            Some("1"),
            Some(ACCOUNT),
            Some("1"),
            Some("1")
        ));
        assert!(interactive_cli_args(&[
            "claude.exe".into(),
            "--resume".into(),
            ID.into()
        ]));
        for arg in [
            "-p",
            "-ap",
            "--print",
            "--print=x",
            "--bg",
            "--background",
            "--output-format=json",
        ] {
            assert!(
                !interactive_cli_args(&["claude.exe".into(), arg.into()]),
                "{arg}"
            );
        }
    }

    #[test]
    fn inactive_account_shared_history_keeps_preference_without_adopting_live_owner() {
        let root = std::env::temp_dir().join(format!("inactive-cache-{}", uuid::Uuid::new_v4()));
        let m = Manager {
            root: root.join("switcher/claude"),
            live_dir: root.join("live"),
            live_config: root.join("live/.claude.json"),
        };
        let profile = m.profile(ID).unwrap();
        let state = profile.join(PLUGIN).join("state");
        fs::create_dir_all(&state).unwrap();
        fs::create_dir_all(&m.live_dir).unwrap();
        fs::write(
            m.root.join("accounts.json"),
            serde_json::to_vec(&crate::claude::ClaudeStore {
                accounts: vec![crate::claude::ClaudeAccount {
                    id: ID.into(),
                    name: "offline".into(),
                    email: None,
                    account_uuid: Some(ACCOUNT.into()),
                    plan_type: None,
                    created_at: "offline".into(),
                    last_used_at: None,
                }],
                ..Default::default()
            })
            .unwrap(),
        )
        .unwrap();
        fs::write(
            profile.join(".claude.json"),
            serde_json::to_vec(&json!({
                "oauthAccount":{"accountUuid":ACCOUNT}
            }))
            .unwrap(),
        )
        .unwrap();
        let foreign = serde_json::to_vec(&json!({"oauthAccount":{"accountUuid":ID}})).unwrap();
        fs::write(&m.live_config, &foreign).unwrap();
        let now = chrono::Utc::now().timestamp();
        let mut l = ledger();
        l.clock = now;
        let s = l.sessions.get_mut(ID).unwrap();
        *s = Session::new(m.live_dir.canonicalize().unwrap(), now);
        s.heartbeat_at = Some(now);
        s.monitor = Some(ACCOUNT.into());
        save(&state, &l).unwrap();
        let lease = try_session_lease(&state, ID).unwrap().unwrap();
        let ledger_before = fs::read(state.join("ledger.json")).unwrap();
        for option in [true, false, true] {
            crate::prompt_cache::preference(m.root.parent().unwrap(), "claude", ID, Some(option))
                .unwrap();
            let mut status = crate::prompt_cache::unknown("historical observation");
            status.session_id = Some(ID.into());
            status.expires_at = Some(now + 300);
            decorate(&m, ID, &mut status).unwrap();
            assert_eq!(status.auto_keepalive_enabled, option);
            assert_eq!(
                status.auto_keepalive_state,
                if option {
                    "account_inactive"
                } else {
                    "disabled"
                }
            );
            assert_eq!(status.auto_keepalive_connected_sessions, 0);
            assert_eq!(status.auto_keepalive_next_at, None);
            assert_eq!(status.expires_at, Some(now + 300));
            assert!(identity(&m, ID, &m.live_dir).is_err());
        }
        assert_eq!(fs::read(&m.live_config).unwrap(), foreign);
        assert_eq!(fs::read(state.join("ledger.json")).unwrap(), ledger_before);
        drop(lease);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn live_connection_count_requires_identity_lease_and_fresh_heartbeat_without_cache_observation()
    {
        let root = std::env::temp_dir().join(format!("live-cache-{}", uuid::Uuid::new_v4()));
        let m = Manager {
            root: root.join("switcher/claude"),
            live_dir: root.join("live"),
            live_config: root.join("live/.claude.json"),
        };
        let profile = m.profile(ID).unwrap();
        let state = profile.join(PLUGIN).join("state");
        fs::create_dir_all(&state).unwrap();
        fs::create_dir_all(&m.live_dir).unwrap();
        let account = crate::claude::ClaudeAccount {
            id: ID.into(),
            name: "synthetic".into(),
            email: None,
            account_uuid: Some(ACCOUNT.into()),
            plan_type: None,
            created_at: "synthetic".into(),
            last_used_at: None,
        };
        fs::write(
            m.root.join("accounts.json"),
            serde_json::to_vec(&crate::claude::ClaudeStore {
                accounts: vec![account],
                ..Default::default()
            })
            .unwrap(),
        )
        .unwrap();
        fs::write(
            profile.join(".claude.json"),
            serde_json::to_vec(&json!({"oauthAccount":{"accountUuid":ACCOUNT}})).unwrap(),
        )
        .unwrap();
        let now = chrono::Utc::now().timestamp();
        let mut l = ledger();
        l.clock = now;
        let s = l.sessions.get_mut(ID).unwrap();
        *s = Session::new(profile.canonicalize().unwrap(), now);
        s.heartbeat_at = Some(now);
        s.monitor = Some(ACCOUNT.into());
        save(&state, &l).unwrap();
        let mut status = crate::prompt_cache::unknown("no cache observation");
        decorate(&m, ID, &mut status).unwrap();
        assert_eq!(status.auto_keepalive_connected_sessions, 0);
        let lease = try_session_lease(&state, ID).unwrap().unwrap();
        decorate(&m, ID, &mut status).unwrap();
        assert_eq!(status.auto_keepalive_connected_sessions, 1);
        assert_eq!(status.state, "unknown");
        assert_eq!(status.expires_at, None);
        l.sessions.get_mut(ID).unwrap().heartbeat_at = Some(now - HEARTBEAT - 1);
        save(&state, &l).unwrap();
        decorate(&m, ID, &mut status).unwrap();
        assert_eq!(status.auto_keepalive_connected_sessions, 0);
        drop(lease);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn same_second_human_anchor_requires_stop_but_compaction_equality_is_ambiguous() {
        let mut l = ledger();
        let dir = l.sessions[ID].directory.clone();
        hook_at(&mut l, &dir, &hook("UserPromptSubmit"), NOW).unwrap();
        observe_at(l.sessions.get_mut(ID).unwrap(), Some(anchor(NOW, 300)), NOW);
        assert!(l.sessions[ID].anchor.is_some());
        assert_eq!(
            gate(&l.sessions[ID], &[], true, true, true, NOW).state,
            "busy"
        );
        hook_at(&mut l, &dir, &hook("Stop"), NOW).unwrap();
        assert_eq!(
            gate(&l.sessions[ID], &[], true, true, true, NOW).state,
            "scheduled"
        );
        hook_at(&mut l, &dir, &hook("PreCompact"), NOW).unwrap();
        observe_at(l.sessions.get_mut(ID).unwrap(), Some(anchor(NOW, 300)), NOW);
        hook_at(&mut l, &dir, &hook("Stop"), NOW).unwrap();
        assert!(l.sessions[ID].anchor.is_none());
        assert_eq!(
            gate(&l.sessions[ID], &[], true, true, true, NOW).state,
            "observation_required"
        );
    }

    #[test]
    fn busy_registered_process_is_not_a_timer_connection_or_emission_permission() {
        let mut s = session();
        s.busy = true;
        s.idle_at = None;
        let mut counts = SessionCounts::default();
        counts.include(&s, true, false);
        assert_eq!(
            counts,
            SessionCounts {
                registered: 1,
                busy: 1,
                connected: 0
            }
        );
        assert_eq!(awaiting_timer_state(&s, true, false, NOW), Some("busy"));
        assert_eq!(
            gate(&s, &[], true, false, true, NOW).state,
            "monitor_unavailable"
        );
        assert_eq!(gate(&s, &[], true, true, true, NOW).state, "busy");
        s.failed = true;
        counts.include(&s, true, true);
        assert_eq!(counts.registered, 2);
        assert_eq!(counts.connected, 0);
        assert_eq!(awaiting_timer_state(&s, true, false, NOW), None);
    }

    #[test]
    fn registered_idle_display_needs_matching_stop_and_never_fabricates_schedule() {
        let mut s = session();
        s.live_prompt_id = Some(ID.into());
        assert_eq!(
            awaiting_timer_state(&s, true, false, NOW),
            Some("waiting_for_stop")
        );
        s.live_stop_prompt_id = Some(ACCOUNT.into());
        assert_eq!(
            awaiting_timer_state(&s, true, false, NOW),
            Some("waiting_for_stop")
        );
        s.live_stop_prompt_id = Some(ID.into());
        assert_eq!(
            awaiting_timer_state(&s, true, false, NOW),
            Some("timer_waiting")
        );
        assert_eq!(awaiting_timer_state(&s, false, false, NOW), None);
        assert_eq!(awaiting_timer_state(&s, true, true, NOW), None);
        assert_eq!(gate(&s, &[], true, false, true, NOW).next_at, None);
        s.ended = true;
        assert_eq!(awaiting_timer_state(&s, true, false, NOW), None);
        let mut counts = SessionCounts::default();
        counts.include(&s, true, true);
        assert_eq!(counts, SessionCounts::default());
    }

    #[test]
    fn registered_without_timer_still_reports_cancelled_and_human_idle_limits() {
        let mut s = session();
        s.live_prompt_id = Some(ID.into());
        s.live_stop_prompt_id = Some(ID.into());
        s.pending = Some(Pending {
            queued_at: NOW - 1,
            previous: anchor(NOW - 240, 300),
            human_generation: s.human_generation,
            stop_at: None,
            cancelled: true,
        });
        assert_eq!(
            awaiting_timer_state(&s, true, false, NOW),
            Some("cancelled")
        );
        s.pending.as_mut().unwrap().cancelled = false;
        assert_eq!(
            awaiting_timer_state(&s, true, false, NOW),
            Some("awaiting_response")
        );
        s.human_at = Some(NOW - HUMAN_IDLE);
        assert_eq!(
            awaiting_timer_state(&s, true, false, NOW),
            Some("human_idle_limit")
        );
    }

    #[test]
    fn cancelled_or_different_model_response_is_not_success() {
        for different_model in [false, true] {
            let mut l = ledger();
            queue(&mut l, ID, NOW).unwrap();
            let s = l.sessions.get_mut(ID).unwrap();
            let mut response = anchor(NOW + 1, 300);
            if different_model {
                response.model = "claude-opus-4-6".into();
            } else {
                s.cancel();
            }
            observe_at(s, Some(response), NOW + 1);
            s.busy = false;
            s.idle_at = Some(NOW + 2);
            s.pending.as_mut().unwrap().stop_at = Some(NOW + 2);
            s.confirm(NOW + 2);
            assert_eq!(s.last_at, None);
            assert_eq!(s.requests, 1);
            assert_eq!(
                gate(s, &l.attempts, true, true, true, NOW + 2).state,
                "cancelled"
            );
            assert_eq!(
                gate(s, &l.attempts, false, true, true, NOW + 2).state,
                "disabled"
            );
        }
    }

    #[test]
    fn missing_output_measurement_remains_optional_after_confirmed_response() {
        let mut l = ledger();
        queue(&mut l, ID, NOW).unwrap();
        let mut response = anchor(NOW + 1, 300);
        response.output_tokens = None;
        observe_at(l.sessions.get_mut(ID).unwrap(), Some(response), NOW + 1);
        let dir = l.sessions[ID].directory.clone();
        hook_at(&mut l, &dir, &hook("Stop"), NOW + 2).unwrap();
        assert_eq!(l.sessions[ID].last_at, Some(NOW + 1));
        assert_eq!(l.sessions[ID].output_tokens, None);
    }

    #[test]
    fn reclaim_only_old_fully_ended_unleased_records_and_preserve_account_budget() {
        let root =
            std::env::temp_dir().join(format!("keepalive-lease-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let mut l = ledger();
        l.attempts = vec![NOW - 10];
        let s = l.sessions.get_mut(ID).unwrap();
        assert!(!reclaimable(s, NOW));
        s.ended = true;
        s.ended_at = Some(NOW - DAY + 1);
        assert!(!reclaimable(s, NOW));
        s.ended_at = Some(NOW - DAY);
        assert!(reclaimable(s, NOW));
        s.pending = Some(Pending {
            queued_at: NOW - DAY - 1,
            previous: anchor(NOW - DAY - 300, 300),
            human_generation: s.human_generation,
            stop_at: None,
            cancelled: false,
        });
        assert!(!reclaimable(s, NOW));
        s.cancel();
        assert!(reclaimable(s, NOW));
        let path = lease_path(&root, ID).unwrap();
        let lease = open_lock(&path).unwrap();
        lease.try_lock().unwrap();
        assert!(lease_live(&root, ID).unwrap());
        reclaim(&root, &mut l, NOW).unwrap();
        assert!(l.sessions.contains_key(ID));
        drop(lease);
        assert!(!lease_live(&root, ID).unwrap());
        reclaim(&root, &mut l, NOW).unwrap();
        assert!(l.sessions.is_empty());
        assert_eq!(l.attempts, vec![NOW - 10]);
        assert!(!path.exists());
        fs::remove_dir(root).unwrap();
    }
}

#[cfg(test)]
mod hook_schema_tests {
    use super::*;

    const ID: &str = "a3426688-2937-4bba-b39c-e8fbbe7f0d8f";

    // The command/args contract implicated by the 2.1.296 SettingsError;
    // deliberately not a replacement for the private CLI's full validator.
    #[derive(Deserialize)]
    struct ExecHook {
        command: String,
        args: Vec<String>,
    }

    fn helper(root: &Path) -> PathBuf {
        root.join(format!(
            "cache-helper-{}{}",
            "a".repeat(64),
            if cfg!(windows) { ".exe" } else { "" }
        ))
    }

    #[test]
    fn plugin_and_live_keepalive_hooks_satisfy_exec_schema_with_literal_paths() {
        let root = Path::new("C:/synthetic path/$data's `literal`/cache-runtime");
        let helper = helper(root);
        let dir = Path::new("C:/synthetic path/profile's $data `literal`");
        let (_, plugin) = plugin_documents(&helper, ID, dir).unwrap();
        assert_eq!(
            plugin["hooks"].as_object().unwrap().len(),
            LIVE_EVENTS.len()
        );
        for event in LIVE_EVENTS {
            let parsed: ExecHook =
                serde_json::from_value(plugin["hooks"][event][0]["hooks"][0].clone()).unwrap();
            assert_eq!(parsed.command, helper.to_str().unwrap(), "{event}");
            assert_eq!(
                parsed.args,
                vec!["--prompt-cache-hook", ID, dir.to_str().unwrap()],
                "{event}"
            );
        }
        let mut settings = json!({});
        merge_live_settings(&mut settings, ID, dir, root, Some(&helper)).unwrap();
        for event in LIVE_EVENTS {
            let groups = settings["hooks"][event].as_array().unwrap();
            assert_eq!(groups.len(), if event == "Stop" { 2 } else { 1 });
            for (index, group) in groups.iter().enumerate() {
                let hook = &group["hooks"][0];
                let parsed: ExecHook = serde_json::from_value(hook.clone()).unwrap();
                let timer = event == "Stop" && index == 1;
                assert_eq!(parsed.command, helper.to_str().unwrap(), "{event}");
                assert_eq!(
                    parsed.args,
                    vec![
                        if timer { LIVE_TIMER } else { LIVE_HOOK },
                        ID,
                        dir.to_str().unwrap()
                    ],
                    "{event}"
                );
                assert_eq!(hook["type"], "command");
                assert_eq!(hook[if timer { "asyncRewake" } else { "async" }], true);
                assert_eq!(
                    hook["timeout"],
                    if timer {
                        LIVE_TIMEOUT
                    } else {
                        LIVE_HOOK_TIMEOUT
                    }
                );
            }
        }
    }

    #[test]
    fn keepalive_reconciliation_preserves_user_permissions_security_and_remote_hooks() {
        let root = Path::new("C:/synthetic/cache-runtime");
        let helper = helper(root);
        let dir = Path::new("C:/synthetic/live");
        let user_permission = json!({"matcher":"Bash","hooks":[{"type":"command","command":"userPermissionRequest"}]});
        let security =
            json!({"matcher":"*","hooks":[{"type":"command","command":"security-check"}]});
        let remote = json!({"hooks":[{"type":"command","command":helper,"args":["--claude-rc-hook",dir],"async":true,"timeout":10}]});
        let legacy_remote = json!({"hooks":[{"type":"command","command":{"exe":helper,"args":["--claude-rc-hook",dir]},"async":true,"timeout":10}]});
        let original = json!({"permissions":{"deny":["Bash(*)"]},"disableRemoteControl":true,
            "hooks":{"PermissionRequest":[user_permission],"PreToolUse":[security.clone()],
                "Stop":[security,remote,legacy_remote]}});
        let mut settings = original.clone();
        merge_live_settings(&mut settings, ID, dir, root, Some(&helper)).unwrap();
        let installed = settings.clone();
        merge_live_settings(&mut settings, ID, dir, root, Some(&helper)).unwrap();
        assert_eq!(settings, installed);
        assert_eq!(
            settings["hooks"]["PermissionRequest"],
            original["hooks"]["PermissionRequest"]
        );
        assert_eq!(
            settings["hooks"]["PreToolUse"],
            original["hooks"]["PreToolUse"]
        );
        assert_eq!(
            &settings["hooks"]["Stop"].as_array().unwrap()[..3],
            original["hooks"]["Stop"].as_array().unwrap()
        );
        assert_eq!(settings["permissions"], original["permissions"]);
        assert_eq!(
            settings["disableRemoteControl"],
            original["disableRemoteControl"]
        );
        merge_live_settings(&mut settings, ID, dir, root, None).unwrap();
        assert_eq!(settings, original);
        merge_live_settings(&mut settings, ID, dir, root, None).unwrap();
        assert_eq!(settings, original);
    }
}
