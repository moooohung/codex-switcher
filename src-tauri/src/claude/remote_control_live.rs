//! Reconnect the same running TUI using the official /rc command. Lifecycle
//! hooks prove PID/session/config ownership; a separate console helper verifies
//! a stable empty idle prompt before sending. No model turn or history writer.
use super::storage::{write_json, Manager};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        OnceLock,
    },
    time::Duration,
};

const HOOK: &str = "--claude-rc-hook";
const CONSOLE: &str = "--claude-rc-console";
const INSPECT: &str = "--claude-rc-inspect";
const SCAN: &str = "--claude-rc-scan";
const EVENTS: [&str; 8] = [
    "SessionStart",
    "UserPromptSubmit",
    "Stop",
    "StopFailure",
    "PermissionRequest",
    "PreCompact",
    "SessionEnd",
    "ConfigChange",
];
const LIMIT: u64 = 256 * 1024;
const IDLE_PROBE_LIMIT: u8 = 3;
static STARTED: AtomicBool = AtomicBool::new(false);
static HELPER: OnceLock<PathBuf> = OnceLock::new();

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Session {
    session_id: String,
    pid: u32,
    created: u64,
    image: PathBuf,
    directory: PathBuf,
    cwd: PathBuf,
    observed_owner: String,
    event_created: u64,
    phase: String,
    observed_at: i64,
    connected_owner: Option<String>,
    #[serde(default)]
    disconnected_generation: Option<String>,
    sent_generation: Option<String>,
    result: String,
    next_check: i64,
    #[serde(default)]
    runtime_bridge_session: Option<String>,
    #[serde(default)]
    runtime_updated_at: i64,
    #[serde(default)]
    bridge_owner: Option<String>,
    #[serde(default)]
    probed_generation: Option<String>,
    #[serde(default)]
    probed_event_created: u64,
    #[serde(default)]
    confirmation_checks: u8,
    #[serde(default)]
    idle_probe_checks: u8,
    #[serde(default)]
    probed_runtime_bridge: Option<String>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    owner: Option<String>,
    generation: String,
    sessions: BTreeMap<String, Session>,
    checked_at: i64,
    error: Option<String>,
}
#[derive(Deserialize)]
struct HookInput {
    hook_event_name: String,
    session_id: String,
    cwd: PathBuf,
    #[serde(default)]
    agent_id: Option<Value>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    key: String,
    generation: String,
    owner: String,
    allow_send: bool,
    session: Session,
}

fn read<T: serde::de::DeserializeOwned>(path: &Path, limit: u64) -> Result<T> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink() && metadata.len() <= limit,
        "Invalid Remote Control state file"
    );
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            metadata.file_attributes() & 0x400 == 0,
            "Remote Control state was redirected"
        );
    }
    let mut bytes = vec![];
    fs::File::open(path)?
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "Remote Control input exceeds limit"
    );
    Ok(serde_json::from_slice(&bytes)?)
}
fn directory(m: &Manager) -> Result<PathBuf> {
    let dir = m.root.join("remote-control");
    fs::create_dir_all(&dir)?;
    let root = m.root.canonicalize()?;
    ensure!(
        dir.canonicalize()? == root.join("remote-control"),
        "Remote Control directory was redirected"
    );
    Ok(dir)
}
fn load(dir: &Path) -> Result<Ledger> {
    let path = dir.join("sessions.json");
    if !path.try_exists()? {
        ensure!(
            !dir.join("sessions.json.bak").try_exists()?,
            "Remote Control primary is missing"
        );
        return Ok(Ledger::default());
    }
    let ledger: Ledger = read(&path, LIMIT)?;
    ensure!(
        ledger.sessions.len() <= 64,
        "Too many Remote Control sessions"
    );
    Ok(ledger)
}
fn save(dir: &Path, ledger: &Ledger) -> Result<()> {
    crate::auth::persistence::atomic_write(
        &dir.join("sessions.json"),
        &serde_json::to_vec_pretty(ledger)?,
    )
}
fn live_owner(m: &Manager) -> Result<String> {
    let store = m.store()?;
    let id = m
        .active_id(&store)?
        .context("No verified live Claude account")?;
    let account = m.account(&id)?;
    let config = Manager::read_config(&m.live_config)?;
    let expected = account
        .account_uuid
        .context("Remote Control requires a saved account UUID")?;
    ensure!(
        config
            .pointer("/oauthAccount/accountUuid")
            .and_then(Value::as_str)
            == Some(&expected),
        "Live Claude owner changed"
    );
    Ok(expected)
}
fn owner(ledger: &mut Ledger, current: &str) {
    if ledger.owner.as_deref() != Some(current) {
        ledger.owner = Some(current.into());
        ledger.generation = uuid::Uuid::new_v4().to_string();
        // Keep old confirmed ownership: it proves which sessions actually had RC.
        for session in ledger.sessions.values_mut() {
            session.next_check = 0;
        }
    }
}
fn helper() -> Result<&'static Path> {
    if HELPER.get().is_none() {
        let _ = HELPER.set(crate::prompt_cache::helper_executable()?);
    }
    HELPER
        .get()
        .map(PathBuf::as_path)
        .context("Remote Control helper unavailable")
}
fn group(helper: &Path, dir: &Path) -> Value {
    // Claude exec form uses a string executable and a sibling argument vector.
    // https://code.claude.com/docs/en/hooks#exec-form-and-shell-form
    json!({"hooks":[{"type":"command","command":helper,"args":[HOOK,dir],"async":true,"timeout":10}]})
}
fn owned_group(group: &Value, dir: &Path) -> bool {
    let command = &group["hooks"][0]["command"];
    let Some(executable) = command.as_str().or_else(|| command["exe"].as_str()) else {
        return false;
    };
    let helper = Path::new(executable);
    let Some(name) = helper.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let hash = name.strip_prefix("cache-helper-").and_then(|name| {
        if cfg!(windows) {
            name.strip_suffix(".exe")
        } else {
            Some(name)
        }
    });
    if helper
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        != Some("cache-runtime")
        || !hash.is_some_and(|hash| {
            hash.len() == 64
                && hash
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
    {
        return false;
    }
    // Compare the whole generated group, in either its current or legacy form.
    // Edited matchers/options and groups containing user security hooks survive.
    group == &self::group(helper, dir)
        || group
            == &json!({"hooks":[{"type":"command","command":{"exe":helper,"args":[HOOK,dir]},"async":true,"timeout":10}]})
}
pub(super) fn merge_hooks(settings: &mut Value, helper: Option<&Path>, dir: &Path) -> Result<bool> {
    // Publish only after every affected event has been validated.
    let mut next = settings.clone();
    let object = next
        .as_object_mut()
        .context("Claude settings must be an object")?;
    if !object.contains_key("hooks") && helper.is_none() {
        return Ok(false);
    }
    let hooks = object
        .entry("hooks")
        .or_insert(json!({}))
        .as_object_mut()
        .context("Claude hooks must be an object")?;
    let mut removed_owned = false;
    for event in EVENTS {
        if let Some(groups) = hooks.get_mut(event) {
            let groups = groups
                .as_array_mut()
                .context("Claude hook groups must be arrays")?;
            let before = groups.len();
            groups.retain(|g| !owned_group(g, dir));
            removed_owned |= groups.len() != before;
            if groups.len() != before && groups.is_empty() {
                hooks.remove(event);
            }
        }
        if let Some(helper) = helper {
            let groups = hooks
                .entry(event)
                .or_insert(json!([]))
                .as_array_mut()
                .context("Claude hook groups must be arrays")?;
            groups.push(group(helper, dir));
        }
    }
    if removed_owned && hooks.is_empty() {
        object.remove("hooks");
    }
    let changed = *settings != next;
    *settings = next;
    Ok(changed)
}

/// Identity and credentials are already guarded by remote_control::apply.
pub(super) fn merge(settings: &mut Value, enabled: bool, dir: &Path) -> Result<bool> {
    merge_hooks(settings, if enabled { Some(helper()?) } else { None }, dir)
}

fn register(ledger: &mut Ledger, hook: &HookInput, mut record: Session) -> Result<()> {
    let key = format!("{}-{}", record.pid, record.created);
    if let Some(old) = ledger.sessions.get(&key) {
        ensure!(
            old.directory == record.directory && old.image == record.image,
            "Claude process/session binding changed"
        );
        if old.session_id != record.session_id {
            // A genuine CLI /resume or /clear can retain its PID. Only the
            // runtime's main-session start/human prompt may rebind it; old RC
            // ownership and send reservations never transfer to another chat.
            ensure!(
                ["SessionStart", "UserPromptSubmit"].contains(&hook.hook_event_name.as_str())
                    && record.event_created > old.event_created,
                "Claude session binding changed"
            );
        } else {
            if old.event_created >= record.event_created {
                return Ok(());
            }
            record.connected_owner = old.connected_owner.clone();
            record.disconnected_generation = old.disconnected_generation.clone();
            record.sent_generation = old.sent_generation.clone();
            record.result = old.result.clone();
            record.bridge_owner = old.bridge_owner.clone();
            record.runtime_bridge_session = old.runtime_bridge_session.clone();
            record.runtime_updated_at = old.runtime_updated_at;
            record.probed_generation = old.probed_generation.clone();
            record.probed_event_created = old.probed_event_created;
            record.confirmation_checks = old.confirmation_checks;
            record.idle_probe_checks = old.idle_probe_checks;
            record.probed_runtime_bridge = old.probed_runtime_bridge.clone();
        }
    } else {
        ensure!(
            ledger.sessions.len() < 64,
            "Remote Control session registry is full"
        );
    }
    record.phase = match hook.hook_event_name.as_str() {
        "Stop" => "idle",
        "SessionEnd" => "ended",
        "PermissionRequest" => "waiting",
        "StopFailure" => "failed",
        "UserPromptSubmit" | "PreCompact" => "busy",
        _ => "unknown", // SessionStart/ConfigChange alone cannot prove idle.
    }
    .into();
    ledger.sessions.insert(key, record);
    Ok(())
}

// Claude publishes this metadata for every running CLI, including sessions
// started before our hooks were installed. No console, input, transcript or
// messaging authentication key is read to discover a session.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeSession {
    pid: u32,
    session_id: String,
    cwd: PathBuf,
    started_at: i64,
    proc_start: String,
    kind: String,
    entrypoint: String,
    pid_domain: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    updated_at: i64,
    #[serde(default)]
    bridge_session_id: Option<String>,
    #[serde(default)]
    spare: bool,
    #[serde(default)]
    parked_job_id: Option<String>,
}

#[cfg(windows)]
fn native_image(image: &Path) -> bool {
    [std::env::var_os("USERPROFILE"), std::env::var_os("HOME")]
        .into_iter()
        .flatten()
        .any(|home| {
            let base = PathBuf::from(home).join(".local");
            if image == base.join("bin/claude.exe") {
                return true;
            }
            image.parent() == Some(base.join("share/claude/versions").as_path())
                && image.file_name().and_then(|s| s.to_str()).is_some_and(|s| {
                    s.split('.').count() == 3
                        && s.split('.')
                            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
                })
        })
}

#[cfg(windows)]
fn runtime_record(
    v: RuntimeSession,
    expected_pid: u32,
    p: super::remote_control_console::Process,
    live: &Path,
    current: &str,
    now: i64,
) -> Result<Session> {
    ensure!(
        v.pid == expected_pid && p.pid == v.pid && v.pid > 1,
        "Runtime session PID mismatch"
    );
    ensure!(
        v.kind == "interactive"
            && v.entrypoint == "cli"
            && v.pid_domain == "win32:maka"
            && !v.spare
            && v.parked_job_id.is_none(),
        "Not a shared interactive Claude CLI"
    );
    uuid::Uuid::parse_str(&v.session_id)?;
    ensure!(
        v.proc_start.parse::<u64>()? == p.created,
        "Runtime session PID was recycled"
    );
    ensure!(
        v.cwd.is_absolute() && v.started_at > 0 && v.started_at <= now * 1000 + 30_000,
        "Invalid runtime session metadata"
    );
    let phase = match v.status.as_str() {
        "idle" => "idle",
        "busy" => "busy",
        "waiting" => "waiting",
        "shell" => "shell",
        _ => "unknown",
    };
    Ok(Session {
        session_id: v.session_id,
        pid: v.pid,
        created: p.created,
        image: p.image,
        directory: live.into(),
        cwd: v.cwd,
        observed_owner: current.into(),
        event_created: ((v.updated_at.max(v.started_at) as u64) * 10_000)
            .saturating_add(116444736000000000),
        phase: phase.into(),
        observed_at: now,
        connected_owner: None,
        disconnected_generation: None,
        sent_generation: None,
        result: "runtime_registered".into(),
        next_check: 0,
        runtime_bridge_session: v.bridge_session_id,
        runtime_updated_at: v.updated_at,
        bridge_owner: None,
        probed_generation: None,
        probed_event_created: 0,
        confirmation_checks: 0,
        idle_probe_checks: 0,
        probed_runtime_bridge: None,
    })
}

#[cfg(windows)]
fn discover(m: &Manager, ledger: &mut Ledger, current: &str, now: i64) -> Result<()> {
    let live = m.live_dir.canonicalize()?;
    let dir = live.join("sessions");
    if !dir.try_exists()? {
        return Ok(());
    }
    ensure!(
        dir.canonicalize()? == dir,
        "Claude runtime registry was redirected"
    );
    // Bound enumeration and reads. Foreign/stale/headless entries are ignored;
    // one in-place JSON publication in progress cannot break every session.
    for entry in fs::read_dir(&dir)?.take(512).flatten() {
        let name = entry.file_name();
        let Some(stem) = name.to_str().and_then(|s| s.strip_suffix(".json")) else {
            continue;
        };
        let Ok(pid) = stem.parse::<u32>() else {
            continue;
        };
        if pid.to_string() != stem {
            continue;
        }
        let path = entry.path();
        let Ok(v) = read::<RuntimeSession>(&path, 16 * 1024) else {
            continue;
        };
        let Ok(p) = super::remote_control_console::process(pid) else {
            continue;
        };
        if !native_image(&p.image) {
            continue;
        }
        let Ok(record) = runtime_record(v, pid, p.clone(), &live, current, now) else {
            continue;
        };
        let Ok(second) = read::<RuntimeSession>(&path, 16 * 1024) else {
            continue;
        };
        if second.session_id != record.session_id
            || second.proc_start.parse::<u64>().ok() != Some(record.created)
            || second.cwd != record.cwd
            || second.updated_at != record.runtime_updated_at
            || second.bridge_session_id != record.runtime_bridge_session
            || !super::remote_control_console::process(pid).is_ok_and(|again| again == p)
        {
            continue;
        }
        let key = format!("{}-{}", pid, record.created);
        let phase = record.phase.clone();
        let bridge = record.runtime_bridge_session.clone();
        let updated = record.runtime_updated_at;
        let event_created = record.event_created;
        // register() replaces newer lifecycle records; retain the prior native
        // binding timestamp to distinguish a real removal from an old read.
        let old_runtime_updated = ledger
            .sessions
            .get(&key)
            .map_or(0, |s| s.runtime_updated_at);
        if updated < old_runtime_updated {
            continue;
        }
        let old_bridge = ledger
            .sessions
            .get(&key)
            .and_then(|s| s.runtime_bridge_session.clone());
        let hook = HookInput {
            hook_event_name: "SessionStart".into(),
            session_id: record.session_id.clone(),
            cwd: record.cwd.clone(),
            agent_id: None,
        };
        register(ledger, &hook, record)?;
        if let Some(s) = ledger.sessions.get_mut(&key) {
            // Runtime status is authoritative for discovery, never proof of an
            // empty composer or a connected transport. A bridge ID is a binding,
            // not permission to switch owners or to report reconnect success.
            if bridge.is_none() && (updated > old_runtime_updated || old_bridge.is_some()) {
                revoke_current_connection(s, current);
            }
            if bridge.is_some() && bridge != old_bridge {
                // A newly published binding is still unconfirmed transport,
                // but old waiting reasons/ownership must not cause an /rc toggle.
                s.bridge_owner = Some(current.into());
                if s.sent_generation.as_deref() != Some(&ledger.generation) {
                    s.result = "runtime_bound_unverified".into();
                }
            } else if bridge.is_some() && s.bridge_owner.is_none() {
                s.bridge_owner = Some(current.into());
            } else if bridge.is_none() && s.bridge_owner.as_deref() == Some(current) {
                s.bridge_owner = None;
            }
            s.runtime_bridge_session = bridge;
            s.runtime_updated_at = updated;
            if event_created >= s.event_created {
                s.phase = phase;
            }
            s.observed_at = now;
        }
    }
    Ok(())
}

fn revoke_current_connection(s: &mut Session, current: &str) {
    if s.connected_owner.as_deref() == Some(current) {
        s.connected_owner = None;
        if s.result == "connected" {
            s.result = "observed_disconnected".into();
        }
    }
    // Preserve previous-account binding evidence needed by the reconnect gate,
    // but never retain current-account proof after the vendor removes its ID.
    if s.bridge_owner.as_deref() == Some(current) {
        s.bridge_owner = None;
    }
}

#[cfg(windows)]
fn hook_entry(configured: &str) -> Result<()> {
    let mut bytes = vec![];
    std::io::stdin()
        .lock()
        .take(LIMIT + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= LIMIT,
        "Remote Control hook input too large"
    );
    let hook: HookInput = serde_json::from_slice(&bytes)?;
    uuid::Uuid::parse_str(&hook.session_id)?;
    if !EVENTS.contains(&hook.hook_event_name.as_str())
        || hook.agent_id.as_ref().is_some_and(|v| !v.is_null())
    {
        return Ok(());
    }
    for (key, value) in [
        ("CLAUDE_CODE_ENTRYPOINT", "cli"),
        ("CLAUDE_CODE_SESSION_ATTENDED", "1"),
        ("CLAUDE_CODE_CHILD_SESSION", "1"),
        ("CLAUDECODE", "1"),
        ("CLAUDE_CODE_SESSION_ID", hook.session_id.as_str()),
    ] {
        if std::env::var(key).ok().as_deref() != Some(value) {
            return Ok(());
        }
    }
    if [
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "ANTHROPIC_BASE_URL",
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_VERTEX",
        "CLAUDE_CODE_USE_FOUNDRY",
    ]
    .iter()
    .any(|k| std::env::var_os(k).is_some())
    {
        return Ok(());
    }
    let Some(event_created) = crate::prompt_cache_keepalive::interactive_parent()? else {
        return Ok(());
    };
    let pid: u32 = std::env::var("CLAUDE_PID")?.parse()?;
    let process = super::remote_control_console::process(pid)?;
    let m = Manager::current()?;
    if m.settings()?.auto_remote_control != Some(true) {
        return Ok(());
    }
    let live = m.live_dir.canonicalize()?;
    ensure!(
        Path::new(configured).canonicalize()? == live,
        "Remote Control config binding changed"
    );
    if let Some(config) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        if PathBuf::from(config).canonicalize()? != live {
            return Ok(());
        }
    }
    ensure!(
        hook.cwd.is_absolute(),
        "Remote Control requires the exact working directory"
    );
    let _credentials = super::locks::CredentialsGuard::acquire_with_timeout(
        &m.live_dir,
        Duration::from_millis(500),
    )?;
    let current = live_owner(&m)?;
    let dir = directory(&m)?;
    let _lock = crate::auth::persistence::lock_file(&dir.join("sessions.lock"))?;
    let mut ledger = load(&dir)?;
    owner(&mut ledger, &current);
    let now = chrono::Utc::now().timestamp();
    let record = Session {
        session_id: hook.session_id.clone(),
        pid,
        created: process.created,
        image: process.image,
        directory: live,
        cwd: hook.cwd.clone(),
        observed_owner: current,
        event_created,
        phase: String::new(),
        observed_at: now,
        connected_owner: None,
        disconnected_generation: None,
        sent_generation: None,
        result: "observing".into(),
        next_check: 0,
        runtime_bridge_session: None,
        runtime_updated_at: 0,
        bridge_owner: None,
        probed_generation: None,
        probed_event_created: 0,
        confirmation_checks: 0,
        idle_probe_checks: 0,
        probed_runtime_bridge: None,
    };
    register(&mut ledger, &hook, record)?;
    save(&dir, &ledger)
}

fn policy(m: &Manager, session: &Session) -> Result<()> {
    ensure!(
        m.settings()?.auto_remote_control == Some(true),
        "Automatic Remote Control is disabled"
    );
    for path in [
        m.live_dir.join("settings.json"),
        session.cwd.join(".claude/settings.json"),
        session.cwd.join(".claude/settings.local.json"),
    ] {
        if !path.try_exists()? {
            continue;
        }
        let v: Value = read(&path, 1024 * 1024)?;
        ensure!(
            v.is_object()
                && v["disableRemoteControl"] != true
                && v["remoteControlAtStartup"] != false,
            "Remote Control is restricted in user/project settings"
        );
    }
    // Managed policies, trusted-device approval and first-run consent are left
    // to Claude itself. The adapter never answers any modal or changes policy.
    Ok(())
}

#[cfg(windows)]
fn console_entry(request_path: &Path) -> Result<()> {
    let m = Manager::current()?;
    let dir = directory(&m)?;
    let request_dir = dir.join("requests");
    ensure!(
        request_path
            .parent()
            .context("Missing request parent")?
            .canonicalize()?
            == request_dir.canonicalize()?,
        "Unowned console request"
    );
    uuid::Uuid::parse_str(
        request_path
            .file_stem()
            .and_then(|s| s.to_str())
            .context("Missing request ID")?,
    )?;
    let request: Request = read(request_path, LIMIT)?;
    let _credentials = super::locks::CredentialsGuard::acquire_with_timeout(
        &m.live_dir,
        Duration::from_millis(500),
    )?;
    ensure!(
        live_owner(&m)? == request.owner
            && request.session.directory == m.live_dir.canonicalize()?,
        "Requested Claude owner changed"
    );
    let permitted = policy(&m, &request.session).is_ok();
    let validate = |ledger: &Ledger| -> Result<()> {
        ensure!(
            ledger.owner.as_deref() == Some(&request.owner)
                && ledger.generation == request.generation,
            "Remote Control generation changed"
        );
        let session = ledger
            .sessions
            .get(&request.key)
            .context("Claude session is no longer registered")?;
        ensure!(
            session.created == request.session.created
                && session.pid == request.session.pid
                && session.image == request.session.image
                && session.directory == request.session.directory
                && session.cwd == request.session.cwd
                && session.session_id == request.session.session_id
                && session.event_created == request.session.event_created
                && session.runtime_bridge_session == request.session.runtime_bridge_session
                && session.runtime_updated_at == request.session.runtime_updated_at
                && session.phase != "ended",
            "Claude lifecycle changed"
        );
        Ok(())
    };
    {
        let _lock = crate::auth::persistence::lock_file(&dir.join("sessions.lock"))?;
        let mut ledger = load(&dir)?;
        validate(&ledger)?;
        if !permitted {
            let session = ledger.sessions.get_mut(&request.key).unwrap();
            session.result = "policy_restricted".into();
            session.next_check = chrono::Utc::now().timestamp() + 30;
            save(&dir, &ledger)?;
            return Ok(());
        }
    }
    let expected = super::remote_control_console::Process {
        pid: request.session.pid,
        created: request.session.created,
        image: request.session.image.clone(),
    };
    let current_binding = request.session.runtime_bridge_session.is_some()
        && request.session.bridge_owner.as_deref() == Some(&request.owner);
    let previously_connected = !current_binding
        && (request
            .session
            .connected_owner
            .as_ref()
            .is_some_and(|o| o != &request.owner)
            || request
                .session
                .bridge_owner
                .as_ref()
                .is_some_and(|o| o != &request.owner));
    let result = super::remote_control_console::visit(
        &expected,
        request.allow_send,
        previously_connected,
        request.session.disconnected_generation.as_deref() == Some(&request.generation),
        || {
            ensure!(
                live_owner(&m)? == request.owner,
                "Live owner changed before command delivery"
            );
            policy(&m, &request.session)?;
            delivery_runtime(&m, &request.session)?;
            let _lock = crate::auth::persistence::lock_file(&dir.join("sessions.lock"))?;
            let mut ledger = load(&dir)?;
            validate(&ledger)?;
            let session = ledger.sessions.get_mut(&request.key).unwrap();
            ensure!(
                ["idle", "unknown"].contains(&session.phase.as_str())
                    && session.runtime_bridge_session.is_none()
                    && session.sent_generation.as_deref() != Some(&request.generation),
                "Claude is not idle or command was already attempted"
            );
            session.sent_generation = Some(request.generation.clone());
            session.confirmation_checks = 0;
            session.result = "delivery_pending".into();
            save(&dir, &ledger)
        },
    );
    let phase = result.unwrap_or("unavailable_or_changed");
    let phase = if phase == "connected"
        && previously_connected
        && request.session.sent_generation.as_deref() != Some(&request.generation)
    {
        "waiting_for_auth_transition"
    } else {
        phase
    };
    let _lock = crate::auth::persistence::lock_file(&dir.join("sessions.lock"))?;
    let mut ledger = load(&dir)?;
    if validate(&ledger).is_ok() {
        if let Some(session) = ledger.sessions.get_mut(&request.key) {
            session.result = phase.into();
            if phase == "connected" {
                session.connected_owner = Some(request.owner.clone());
                session.bridge_owner = Some(request.owner);
                session.disconnected_generation = None;
            } else if phase == "observed_owner_changed" {
                session.disconnected_generation = Some(request.generation.clone());
            } else if phase == "observed_disconnected"
                && session.connected_owner.as_deref() == Some(&request.owner)
            {
                // Respect a session that the user disconnected before switching.
                session.connected_owner = None;
            }
            session.next_check =
                chrono::Utc::now().timestamp() + if phase == "connected" { 30 } else { 10 };
        }
        save(&dir, &ledger)?;
    }
    Ok(())
}

#[cfg(windows)]
fn check_delivery_runtime(v: &RuntimeSession, s: &Session) -> Result<()> {
    ensure!(
        v.pid == s.pid
            && v.proc_start.parse::<u64>()? == s.created
            && v.session_id == s.session_id
            && v.cwd == s.cwd
            && v.kind == "interactive"
            && v.entrypoint == "cli"
            && v.pid_domain == "win32:maka"
            && !v.spare
            && v.parked_job_id.is_none(),
        "Claude native session changed before Remote Control delivery"
    );
    ensure!(
        v.bridge_session_id.is_none(),
        "Claude already has a native Remote Control binding"
    );
    ensure!(
        !["busy", "waiting", "shell"].contains(&v.status.as_str()),
        "Claude is not at an idle input before Remote Control delivery"
    );
    Ok(())
}
#[cfg(windows)]
fn delivery_runtime(m: &Manager, s: &Session) -> Result<()> {
    let dir = m.live_dir.canonicalize()?.join("sessions");
    ensure!(
        dir.canonicalize()? == dir,
        "Claude runtime registry redirected"
    );
    let path = dir.join(format!("{}.json", s.pid));
    // The native binding can update without a new hook/runtime timestamp.
    // Check the actual bounded metadata twice just before reserving input.
    for _ in 0..2 {
        check_delivery_runtime(&read::<RuntimeSession>(&path, 16 * 1024)?, s)?;
    }
    Ok(())
}

pub fn handle_helper(args: &[String]) -> bool {
    let Some(flag) = args
        .get(1)
        .map(String::as_str)
        .filter(|f| [HOOK, CONSOLE, INSPECT, SCAN].contains(f))
    else {
        return false;
    };
    #[cfg(windows)]
    {
        let result = match (flag, args.get(2)) {
            (HOOK, Some(dir)) => hook_entry(dir),
            (CONSOLE, Some(request)) => console_entry(Path::new(request)),
            (SCAN, Some(output)) => (|| {
                let m = Manager::current()?;
                let _credentials = super::locks::CredentialsGuard::acquire_with_timeout(
                    &m.live_dir,
                    Duration::from_millis(500),
                )?;
                let dir = directory(&m)?;
                let _lock = crate::auth::persistence::lock_file(&dir.join("sessions.lock"))?;
                let mut ledger = load(&dir)?;
                let current = live_owner(&m)?;
                owner(&mut ledger, &current);
                discover(&m, &mut ledger, &current, chrono::Utc::now().timestamp())?;
                ledger.checked_at = chrono::Utc::now().timestamp();
                save(&dir, &ledger)?;
                write_json(
                    Path::new(output),
                    &json!({"registered":ledger.sessions.len(),"input_sent":false,"console_attached":false}),
                )
            })(),
            (INSPECT, Some(pid)) => (|| {
                let output = args.get(3).context("Missing diagnostic output path")?;
                let value = match super::remote_control_console::inspect(pid.parse()?) {
                    Ok(value) => value,
                    Err(error) => json!({"pid":pid,"input_sent":false,"error":error.to_string()}),
                };
                crate::auth::persistence::atomic_write(
                    Path::new(output),
                    &serde_json::to_vec_pretty(&value)?,
                )
            })(),
            _ => Err(anyhow::anyhow!("Missing Remote Control helper argument")),
        };
        // Never emit hook JSON, prompt text, tokens or console contents.
        // AttachConsole changes standard handles. Never print after visiting a
        // user console, including on error; diagnostics use owned files only.
        let _ = result;
    }
    #[cfg(not(windows))]
    {
        let _ = flag;
    }
    true
}

#[cfg(windows)]
fn tick(m: &Manager) -> Result<()> {
    use std::{
        os::windows::process::CommandExt,
        process::{Command, Stdio},
        time::Instant,
    };
    let dir = directory(m)?;
    // GUI and LAN hosts may both run. Only one processes console requests;
    // hooks still use their separate short ledger lock and never wait on this.
    let monitor = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join("monitor.lock"))?;
    match monitor.try_lock() {
        Ok(()) => (),
        Err(std::fs::TryLockError::WouldBlock) => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    let enabled = m.settings()?.auto_remote_control == Some(true);
    let current = live_owner(m)?;
    let now = chrono::Utc::now().timestamp();
    let tasks = {
        let _lock = crate::auth::persistence::lock_file(&dir.join("sessions.lock"))?;
        let mut ledger = load(&dir)?;
        owner(&mut ledger, &current);
        ledger.checked_at = now;
        ledger.error = None;
        discover(m, &mut ledger, &current, now)?;
        // Never restore retired records or a backup over a new process generation.
        for s in ledger.sessions.values_mut() {
            if !super::remote_control_console::process(s.pid)
                .is_ok_and(|p| p.created == s.created && p.image == s.image)
            {
                s.phase = "ended".into();
                s.result = "process_ended_or_changed".into();
            }
        }
        ledger
            .sessions
            .retain(|_, s| s.phase != "ended" || now - s.observed_at < 86400);
        let tasks: Vec<_> = if enabled {
            ledger
                .sessions
                .iter()
                .filter(|(_, s)| s.phase != "ended" && s.next_check <= now)
                // Never attach for ordinary periodic observation. Probe once
                // after an account/lifecycle change when an existing RC binding
                // needs reconnect, with at most three post-send confirmations.
                .filter(|(_, s)| demand_probe(s, &ledger.generation, &current))
                .take(4)
                .map(|(key, s)| Request {
                    key: key.clone(),
                    generation: ledger.generation.clone(),
                    owner: current.clone(),
                    allow_send: ["idle", "unknown"].contains(&s.phase.as_str())
                        && s.runtime_bridge_session.is_none()
                        && s.sent_generation.as_deref() != Some(&ledger.generation)
                        && (s.connected_owner.as_ref().is_some_and(|o| o != &current)
                            || s.bridge_owner.as_ref().is_some_and(|o| o != &current)),
                    session: s.clone(),
                })
                .collect()
        } else {
            vec![]
        };
        for task in &tasks {
            if let Some(s) = ledger.sessions.get_mut(&task.key) {
                reserve_probe(s, &ledger.generation);
            }
        }
        save(&dir, &ledger)?;
        tasks
    };
    let requests = dir.join("requests");
    fs::create_dir_all(&requests)?;
    ensure!(
        requests.canonicalize()? == dir.canonicalize()?.join("requests"),
        "Console request directory redirected"
    );
    for task in tasks {
        let path = requests.join(format!("{}.json", uuid::Uuid::new_v4()));
        // Owned temporary file contains metadata only, never prompt/auth data.
        crate::auth::persistence::atomic_write(&path, &serde_json::to_vec(&task)?)?;
        let mut child = Command::new(helper()?)
            .arg(CONSOLE)
            .arg(&path)
            .creation_flags(0x08000000)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if child.try_wait()?.is_some() {
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let _ = fs::remove_file(&path);
        let _lock = crate::auth::persistence::lock_file(&dir.join("sessions.lock"))?;
        let mut ledger = load(&dir)?;
        if let Some(s) = ledger.sessions.get_mut(&task.key) {
            // Bound inaccessible consoles as well; do not spin on helper errors.
            if s.next_check <= now {
                s.next_check = now + 30;
            }
        }
        save(&dir, &ledger)?;
    }
    Ok(())
}
fn demand_probe(s: &Session, generation: &str, current: &str) -> bool {
    if ["busy", "waiting", "shell", "failed", "ended"].contains(&s.phase.as_str())
        || s.result == "connected" && s.connected_owner.as_deref() == Some(current)
    {
        return false;
    }
    if s.runtime_bridge_session.is_some() && s.probed_runtime_bridge != s.runtime_bridge_session {
        // Even after the three timed post-send checks, a genuinely new native
        // binding gets one read-only observation. The send reservation survives.
        return true;
    }
    if s.sent_generation.as_deref() == Some(generation) {
        return s.confirmation_checks < 3;
    }
    if s.runtime_bridge_session.is_some() {
        // A binding publication/owner change admits one read-only confirmation
        // once idle. Never toggle an existing native binding or attach every poll.
        return s.probed_generation.as_deref() != Some(generation)
            || s.probed_runtime_bridge != s.runtime_bridge_session;
    }
    let previous_owner = s.connected_owner.as_ref().is_some_and(|o| o != current)
        || s.bridge_owner.as_ref().is_some_and(|o| o != current);
    previous_owner
        && (s.probed_generation.as_deref() != Some(generation)
            || s.probed_runtime_bridge.is_some()
            || s.probed_event_created < s.event_created
            // Stop runs before the TUI necessarily renders its idle composer.
            // Permit two delayed rechecks for this exact idle lifecycle only.
            // No new account/connection or uncertain write can be retried here.
            || s.phase == "idle"
                && ["waiting_for_empty_prompt", "waiting_for_stable_prompt"]
                    .contains(&s.result.as_str())
                && s.idle_probe_checks < IDLE_PROBE_LIMIT)
}
fn reserve_probe(s: &mut Session, generation: &str) {
    if s.sent_generation.as_deref() == Some(generation) {
        s.confirmation_checks = s.confirmation_checks.saturating_add(1);
    } else if s.probed_generation.as_deref() == Some(generation)
        && s.probed_event_created == s.event_created
    {
        s.idle_probe_checks = s.idle_probe_checks.saturating_add(1);
    } else {
        s.idle_probe_checks = 1;
    }
    s.probed_generation = Some(generation.into());
    s.probed_event_created = s.event_created;
    s.probed_runtime_bridge = s.runtime_bridge_session.clone();
}
pub(super) fn start() {
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    #[cfg(windows)]
    std::thread::spawn(|| loop {
        if let Ok(m) = Manager::current() {
            if let Err(error) = tick(&m) {
                let _ = write_json(
                    &m.root.join("remote-control-live-error.json"),
                    &json!({"checked_at":chrono::Utc::now().timestamp(),"error":error.to_string()}),
                );
            }
        }
        std::thread::sleep(Duration::from_secs(5));
    });
}
pub(super) fn status(m: &Manager) -> Result<Value> {
    let ledger = load(&directory(m)?)?;
    Ok(status_value(
        &ledger,
        chrono::Utc::now().timestamp(),
        m.settings()?.auto_remote_control == Some(true),
    ))
}
fn waiting_reason<'a>(s: &'a Session, ledger: &Ledger) -> Option<&'a str> {
    if s.phase == "ended" || s.result == "connected" && s.connected_owner == ledger.owner {
        return None;
    }
    match s.phase.as_str() {
        "busy" => return Some("busy"),
        "shell" => return Some("shell"),
        "waiting" => return Some("permission_required"),
        "failed" => return Some("response_failed"),
        _ => (),
    }
    if s.sent_generation.as_deref() == Some(&ledger.generation) {
        return Some("awaiting_connection_confirmation");
    }
    if s.result.starts_with("waiting_")
        || ["unavailable_or_changed", "policy_restricted"].contains(&s.result.as_str())
    {
        return Some(&s.result);
    }
    Some(if s.connected_owner.is_none() && s.bridge_owner.is_none() {
        "initial_activation_required"
    } else {
        "connection_unverified"
    })
}
fn status_value(ledger: &Ledger, now: i64, enabled: bool) -> Value {
    let counts = |predicate: &dyn Fn(&Session) -> bool| {
        ledger
            .sessions
            .values()
            .filter(|s| s.phase != "ended" && predicate(s))
            .count()
    };
    let mut reasons: BTreeMap<&str, usize> = BTreeMap::new();
    for s in ledger.sessions.values() {
        if let Some(reason) = waiting_reason(s, ledger) {
            *reasons.entry(reason).or_default() += 1;
        }
    }
    let retry_at = enabled
        .then_some(ledger.owner.as_deref())
        .flatten()
        .and_then(|current| {
            ledger
                .sessions
                .values()
                .filter(|s| demand_probe(s, &ledger.generation, current))
                .map(|s| s.next_check.max(now))
                .min()
        });
    json!({"checked_at":ledger.checked_at,"registered":counts(&|_|true),
        "connected":counts(&|s|s.result=="connected" && s.connected_owner==ledger.owner),
        "requested":counts(&|s|s.sent_generation.as_deref()==Some(&ledger.generation) && s.result!="connected"),
        "waiting":reasons.values().sum::<usize>(),
        "runtime_bound":counts(&|s|s.runtime_bridge_session.is_some()),
        "waiting_reasons":reasons,"retry_at":retry_at,
        "transport":"runtime_registry_demand_console_rc","error":ledger.error})
}
pub(super) fn status_for_ui(m: &Manager) -> Value {
    status(m).unwrap_or_else(|error|json!({"checked_at":0,"registered":0,"connected":0,
        "requested":0,"waiting":0,"runtime_bound":0,"waiting_reasons":{},"retry_at":null,"transport":"runtime_registry_demand_console_rc","error":error.to_string()}))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn session() -> Session {
        Session {
            session_id: uuid::Uuid::new_v4().to_string(),
            pid: 100,
            created: 10,
            image: "C:/synthetic/claude.exe".into(),
            directory: "C:/synthetic/live".into(),
            cwd: "C:/synthetic/cwd".into(),
            observed_owner: "owner-a".into(),
            event_created: 10,
            phase: "idle".into(),
            observed_at: 100,
            connected_owner: Some("owner-a".into()),
            disconnected_generation: None,
            sent_generation: None,
            result: "connected".into(),
            next_check: 200,
            runtime_bridge_session: None,
            runtime_updated_at: 0,
            bridge_owner: None,
            probed_generation: None,
            probed_event_created: 0,
            confirmation_checks: 0,
            idle_probe_checks: 0,
            probed_runtime_bridge: None,
        }
    }
    #[cfg(windows)]
    fn runtime_value() -> Value {
        json!({"pid":100,"sessionId":"a3426688-2937-4bba-b39c-e8fbbe7f0d8f", "cwd":"C:/synthetic/cwd",
          "startedAt":90000,"procStart":"10","kind":"interactive","entrypoint":"cli", "pidDomain":"win32:maka",
          "status":"busy","updatedAt":99000,"bridgeSessionId":"session_synthetic"})
    }
    #[cfg(windows)]
    fn parse_runtime(v: Value) -> Result<Session> {
        runtime_record(
            serde_json::from_value(v)?,
            100,
            super::super::remote_control_console::Process {
                pid: 100,
                created: 10,
                image: "C:/synthetic/claude.exe".into(),
            },
            Path::new("C:/synthetic/live"),
            "owner-a",
            100,
        )
    }
    #[test]
    #[cfg(windows)]
    fn running_session_discovers_without_hook_and_does_not_fabricate_connection() {
        let s = parse_runtime(runtime_value()).unwrap();
        assert_eq!(s.phase, "busy");
        assert_eq!(
            s.runtime_bridge_session.as_deref(),
            Some("session_synthetic")
        );
        assert!(s.connected_owner.is_none());
        assert!(s.sent_generation.is_none());
    }
    #[test]
    #[cfg(windows)]
    fn runtime_discovery_rejects_headless_foreign_recycled_and_invalid_identity() {
        for (field, value) in [
            ("pid", json!(101)),
            ("sessionId", json!("bad")),
            ("procStart", json!("11")),
            ("kind", json!("bg")),
            ("entrypoint", json!("sdk")),
            ("pidDomain", json!("posix")),
            ("cwd", json!("relative")),
            ("startedAt", json!(200000)),
            ("spare", json!(true)),
            ("parkedJobId", json!("job")),
        ] {
            let mut v = runtime_value();
            v[field] = value;
            assert!(parse_runtime(v).is_err(), "field {field}");
        }
    }
    #[test]
    fn hook_merge_is_idempotent_and_preserves_foreign_hooks_and_policy() {
        let dir = Path::new("C:/synthetic/live");
        let exe = PathBuf::from(format!(
            "C:/synthetic/cache-runtime/cache-helper-{}{}",
            "a".repeat(64),
            if cfg!(windows) { ".exe" } else { "" }
        ));
        let foreign =
            json!({"matcher":"*","hooks":[{"type":"command","command":"existing-command"}]});
        let mut settings = json!({"disableRemoteControl":true,"hooks":{"Stop":[foreign.clone()]},"permissions":{"deny":["Bash(*)"]}});
        assert!(merge_hooks(&mut settings, Some(&exe), dir).unwrap());
        assert!(!merge_hooks(&mut settings, Some(&exe), dir).unwrap());
        assert_eq!(settings["hooks"]["Stop"][0], foreign);
        assert_eq!(settings["disableRemoteControl"], true);
        assert_eq!(settings["hooks"]["Stop"].as_array().unwrap().len(), 2);
        assert!(merge_hooks(&mut settings, None, dir).unwrap());
        assert_eq!(settings["hooks"]["Stop"], json!([foreign]));
        assert!(!merge_hooks(&mut settings, None, dir).unwrap());
    }
    #[test]
    fn stale_stop_cannot_overwrite_new_prompt_or_send_attempt() {
        let mut l = Ledger::default();
        let s = session();
        let hook = |event: &str| HookInput {
            hook_event_name: event.into(),
            session_id: s.session_id.clone(),
            cwd: s.cwd.clone(),
            agent_id: None,
        };
        register(&mut l, &hook("Stop"), s.clone()).unwrap();
        let mut newer = s.clone();
        newer.event_created = 30;
        register(&mut l, &hook("UserPromptSubmit"), newer).unwrap();
        register(&mut l, &hook("Stop"), s.clone()).unwrap();
        assert_eq!(l.sessions["100-10"].phase, "busy");
        let mut bad = s.clone();
        bad.session_id = uuid::Uuid::new_v4().to_string();
        assert!(register(&mut l, &hook("Stop"), bad).is_err());
        let mut next = s.clone();
        next.created = 20;
        register(&mut l, &hook("Stop"), next).unwrap();
        assert_eq!(l.sessions.len(), 2);
    }
    #[test]
    fn real_main_session_rebind_does_not_transfer_old_connection_or_send() {
        let mut l = Ledger::default();
        let s = session();
        let initial = HookInput {
            hook_event_name: "Stop".into(),
            session_id: s.session_id.clone(),
            cwd: s.cwd.clone(),
            agent_id: None,
        };
        register(&mut l, &initial, s.clone()).unwrap();
        let mut next = s;
        next.session_id = uuid::Uuid::new_v4().to_string();
        next.event_created = 20;
        next.connected_owner = None;
        next.sent_generation = None;
        let resumed = HookInput {
            hook_event_name: "SessionStart".into(),
            session_id: next.session_id.clone(),
            cwd: next.cwd.clone(),
            agent_id: None,
        };
        register(&mut l, &resumed, next.clone()).unwrap();
        assert_eq!(l.sessions["100-10"].session_id, next.session_id);
        assert!(l.sessions["100-10"].connected_owner.is_none());
        assert!(l.sessions["100-10"].sent_generation.is_none());
    }
    #[test]
    fn account_generation_changes_only_on_owner_change_and_retains_connection_proof() {
        let mut l = Ledger::default();
        l.sessions.insert("100-10".into(), session());
        owner(&mut l, "owner-a");
        let first = l.generation.clone();
        owner(&mut l, "owner-a");
        assert_eq!(l.generation, first);
        owner(&mut l, "owner-b");
        assert_ne!(l.generation, first);
        assert_eq!(
            l.sessions["100-10"].connected_owner.as_deref(),
            Some("owner-a")
        );
        assert_eq!(l.sessions["100-10"].next_check, 0);
    }
    #[test]
    fn runtime_observation_never_attaches_and_busy_work_defers_reconnect() {
        let mut s = session();
        s.connected_owner = None;
        s.bridge_owner = Some("owner-a".into());
        assert!(!demand_probe(&s, "generation-a", "owner-a"));
        s.phase = "busy".into();
        assert!(!demand_probe(&s, "generation-b", "owner-b"));
        s.phase = "waiting".into();
        assert!(!demand_probe(&s, "generation-b", "owner-b"));
        s.phase = "idle".into();
        assert!(demand_probe(&s, "generation-b", "owner-b"));
        s.probed_generation = Some("generation-b".into());
        s.probed_event_created = s.event_created;
        assert!(!demand_probe(&s, "generation-b", "owner-b"));
        s.event_created += 1;
        assert!(demand_probe(&s, "generation-b", "owner-b"));
        s.sent_generation = Some("generation-b".into());
        s.confirmation_checks = 2;
        assert!(demand_probe(&s, "generation-b", "owner-b"));
        s.confirmation_checks = 3;
        assert!(!demand_probe(&s, "generation-b", "owner-b"));
    }
    #[test]
    fn idle_stop_before_tui_render_gets_bounded_rechecks_without_resending() {
        let mut s = session();
        // The old implementation stamped this event before its first probe and
        // never revisited it once Stop had fired ahead of the empty TUI prompt.
        for n in 1..=IDLE_PROBE_LIMIT {
            assert!(demand_probe(&s, "generation-b", "owner-b"));
            reserve_probe(&mut s, "generation-b");
            assert_eq!(s.idle_probe_checks, n);
            s.result = "waiting_for_empty_prompt".into();
            assert!(s.sent_generation.is_none());
            assert_eq!(s.connected_owner.as_deref(), Some("owner-a"));
        }
        assert!(!demand_probe(&s, "generation-b", "owner-b"));
        // A genuinely newer Stop renews the observation budget, not the send.
        s.event_created += 1;
        assert!(demand_probe(&s, "generation-b", "owner-b"));
        reserve_probe(&mut s, "generation-b");
        assert_eq!(s.idle_probe_checks, 1);
        s.result = "waiting_for_stable_prompt".into();
        assert!(demand_probe(&s, "generation-b", "owner-b"));
        s.sent_generation = Some("generation-b".into());
        s.confirmation_checks = 3;
        s.event_created += 1;
        assert!(!demand_probe(&s, "generation-b", "owner-b"));
    }
    #[test]
    fn idle_rechecks_never_probe_work_shell_approvals_or_ambiguous_errors() {
        let mut s = session();
        reserve_probe(&mut s, "generation-b");
        s.result = "waiting_for_empty_prompt".into();
        for phase in ["busy", "waiting", "shell", "failed", "ended", "unknown"] {
            s.phase = phase.into();
            assert!(!demand_probe(&s, "generation-b", "owner-b"), "{phase}");
        }
        s.phase = "idle".into();
        for result in [
            "unavailable_or_changed",
            "waiting_for_disconnect_evidence",
            "policy_restricted",
            "waiting_for_auth_transition",
        ] {
            s.result = result.into();
            assert!(!demand_probe(&s, "generation-b", "owner-b"), "{result}");
        }
        s.result = "waiting_for_empty_prompt".into();
        assert!(!demand_probe(&s, "generation-b", "owner-a"));
        s.connected_owner = None;
        assert!(!demand_probe(&s, "generation-b", "owner-b"));
        // Newer lifecycle is still blocked if its native state proves shell.
        s.connected_owner = Some("owner-a".into());
        s.event_created += 1;
        s.phase = "shell".into();
        assert!(!demand_probe(&s, "generation-b", "owner-b"));
    }
    #[test]
    #[cfg(windows)]
    fn runtime_shell_is_distinct_and_failure_hook_cannot_authorize_reconnect() {
        let mut v = runtime_value();
        v["status"] = json!("shell");
        assert_eq!(parse_runtime(v).unwrap().phase, "shell");
        let s = session();
        let hook = HookInput {
            hook_event_name: "StopFailure".into(),
            session_id: s.session_id.clone(),
            cwd: s.cwd.clone(),
            agent_id: None,
        };
        let mut ledger = Ledger::default();
        register(&mut ledger, &hook, s).unwrap();
        assert_eq!(ledger.sessions["100-10"].phase, "failed");
        assert!(!demand_probe(
            &ledger.sessions["100-10"],
            "generation-b",
            "owner-b"
        ));
    }
    #[test]
    fn status_reports_real_blockers_and_only_schedules_admitted_rechecks() {
        let mut l = Ledger {
            owner: Some("owner-b".into()),
            generation: "generation-b".into(),
            ..Ledger::default()
        };
        let mut s = session();
        s.result = "waiting_for_empty_prompt".into();
        s.next_check = 110;
        reserve_probe(&mut s, &l.generation);
        l.sessions.insert("100-10".into(), s);
        let v = status_value(&l, 100, true);
        assert_eq!(v["connected"], 0);
        assert_eq!(v["requested"], 0);
        assert_eq!(v["waiting_reasons"]["waiting_for_empty_prompt"], 1);
        assert_eq!(v["retry_at"], 110);
        assert!(status_value(&l, 100, false)["retry_at"].is_null());
        for (phase, reason) in [
            ("busy", "busy"),
            ("shell", "shell"),
            ("waiting", "permission_required"),
            ("failed", "response_failed"),
        ] {
            l.sessions.get_mut("100-10").unwrap().phase = phase.into();
            let v = status_value(&l, 100, true);
            assert_eq!(v["waiting_reasons"][reason], 1);
            assert!(v["retry_at"].is_null());
        }
        let s = l.sessions.get_mut("100-10").unwrap();
        s.phase = "idle".into();
        s.connected_owner = None;
        s.result = "runtime_registered".into();
        assert_eq!(
            status_value(&l, 100, true)["waiting_reasons"]["initial_activation_required"],
            1
        );
        let s = l.sessions.get_mut("100-10").unwrap();
        s.connected_owner = l.owner.clone();
        s.result = "connected".into();
        let v = status_value(&l, 100, true);
        assert_eq!(v["connected"], 1);
        assert_eq!(v["waiting"], 0);
    }
    #[test]
    fn removed_current_runtime_binding_revokes_proof_but_never_rearms_input() {
        let mut s = session();
        s.result = "connected".into();
        s.bridge_owner = Some("owner-a".into());
        s.sent_generation = Some("generation-a".into());
        revoke_current_connection(&mut s, "owner-a");
        assert!(s.connected_owner.is_none());
        assert!(s.bridge_owner.is_none());
        assert_eq!(s.result, "observed_disconnected");
        assert_eq!(s.sent_generation.as_deref(), Some("generation-a"));
        let mut prior = session();
        prior.bridge_owner = Some("owner-a".into());
        revoke_current_connection(&mut prior, "owner-b");
        assert_eq!(prior.connected_owner.as_deref(), Some("owner-a"));
        assert_eq!(prior.bridge_owner.as_deref(), Some("owner-a"));
        // Upgrade keeps a legacy ledger readable with no invented retry budget.
        let mut legacy = serde_json::to_value(prior).unwrap();
        legacy.as_object_mut().unwrap().remove("idle_probe_checks");
        assert_eq!(
            serde_json::from_value::<Session>(legacy)
                .unwrap()
                .idle_probe_checks,
            0
        );
    }
    #[test]
    fn hook_preserves_native_observation_and_binding_probe_is_not_periodic() {
        let mut s = session();
        s.runtime_bridge_session = Some("native-old".into());
        s.runtime_updated_at = 1000;
        let hook = HookInput {
            hook_event_name: "UserPromptSubmit".into(),
            session_id: s.session_id.clone(),
            cwd: s.cwd.clone(),
            agent_id: None,
        };
        let mut l = Ledger::default();
        register(&mut l, &hook, s.clone()).unwrap();
        let mut next = s.clone();
        next.event_created += 1;
        next.runtime_updated_at = 0;
        next.runtime_bridge_session = None;
        register(&mut l, &hook, next).unwrap();
        let s = l.sessions.get_mut("100-10").unwrap();
        assert_eq!(s.runtime_updated_at, 1000);
        assert_eq!(s.runtime_bridge_session.as_deref(), Some("native-old"));
        assert!(!demand_probe(s, "generation-b", "owner-b")); // busy
        s.phase = "idle".into();
        assert!(demand_probe(s, "generation-b", "owner-b"));
        reserve_probe(s, "generation-b");
        assert!(!demand_probe(s, "generation-b", "owner-b"));
        s.event_created += 1;
        assert!(!demand_probe(s, "generation-b", "owner-b"));
        s.runtime_bridge_session = Some("native-new".into());
        assert!(demand_probe(s, "generation-b", "owner-b"));
        reserve_probe(s, "generation-b");
        assert!(!demand_probe(s, "generation-b", "owner-b"));
        s.runtime_bridge_session = None;
        assert!(demand_probe(s, "generation-b", "owner-b"));
        assert!(s.sent_generation.is_none());
    }
    #[test]
    #[cfg(windows)]
    fn native_binding_publication_between_observation_and_delivery_blocks_toggle() {
        let mut s = session();
        s.session_id = "a3426688-2937-4bba-b39c-e8fbbe7f0d8f".into();
        let mut v = runtime_value();
        v["status"] = json!("idle");
        v["bridgeSessionId"] = Value::Null;
        let parse = |v| serde_json::from_value::<RuntimeSession>(v).unwrap();
        assert!(check_delivery_runtime(&parse(v.clone()), &s).is_ok());
        // Binding changes do not require updatedAt to change in the real registry.
        let old_updated = v["updatedAt"].clone();
        v["bridgeSessionId"] = json!("native-new");
        assert_eq!(v["updatedAt"], old_updated);
        assert!(check_delivery_runtime(&parse(v.clone()), &s).is_err());
        v["bridgeSessionId"] = Value::Null;
        for status in ["shell", "waiting", "busy"] {
            v["status"] = json!(status);
            assert!(check_delivery_runtime(&parse(v.clone()), &s).is_err());
        }
        v["status"] = json!("idle");
        v["procStart"] = json!("11");
        assert!(check_delivery_runtime(&parse(v), &s).is_err());
        assert!(s.sent_generation.is_none());
    }
    #[test]
    fn new_binding_after_confirmation_budget_gets_one_read_only_probe() {
        let mut s = session();
        s.sent_generation = Some("generation-b".into());
        s.confirmation_checks = 3;
        s.result = "requested".into();
        s.runtime_bridge_session = Some("native-new".into());
        assert!(demand_probe(&s, "generation-b", "owner-b"));
        reserve_probe(&mut s, "generation-b");
        assert!(!demand_probe(&s, "generation-b", "owner-b"));
        assert_eq!(s.sent_generation.as_deref(), Some("generation-b"));
        s.event_created += 1;
        assert!(!demand_probe(&s, "generation-b", "owner-b"));
        s.runtime_bridge_session = Some("native-newer".into());
        assert!(demand_probe(&s, "generation-b", "owner-b"));
        s.phase = "busy".into();
        assert!(!demand_probe(&s, "generation-b", "owner-b"));
    }
    #[test]
    fn corrupt_primary_never_restores_backup_or_fabricates_sessions() {
        let dir = std::env::temp_dir().join(format!("rc-ledger-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("sessions.json.bak"), "{}").unwrap();
        assert!(load(&dir).is_err());
        fs::write(dir.join("sessions.json"), "invalid").unwrap();
        assert!(load(&dir).is_err());
        assert_eq!(
            fs::read_to_string(dir.join("sessions.json")).unwrap(),
            "invalid"
        );
        fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod hook_schema_tests {
    use super::*;

    // Focused offline contract for the 2.1.296 failure: a command must be a
    // string and its exec arguments must be a sibling array of strings.
    // This is not the private CLI's complete settings validator.
    #[derive(Deserialize)]
    struct ExecHook {
        command: String,
        args: Vec<String>,
    }

    fn helper(hash: char) -> PathBuf {
        PathBuf::from(format!(
            "C:/synthetic path/$data's `literal`/cache-runtime/cache-helper-{}{}",
            hash.to_string().repeat(64),
            if cfg!(windows) { ".exe" } else { "" }
        ))
    }

    fn legacy(helper: &Path, dir: &Path) -> Value {
        json!({"hooks":[{"type":"command","command":{"exe":helper,"args":[HOOK,dir]},"async":true,"timeout":10}]})
    }

    #[test]
    fn all_remote_events_emit_string_commands_and_literal_exec_arguments() {
        let helper = helper('a');
        let dir = Path::new("C:/synthetic path/profile's $data `literal`");
        let mut settings = json!({});
        merge_hooks(&mut settings, Some(&helper), dir).unwrap();
        assert_eq!(settings["hooks"].as_object().unwrap().len(), EVENTS.len());
        for event in EVENTS {
            let groups = settings["hooks"][event].as_array().unwrap();
            assert_eq!(groups.len(), 1, "{event}");
            let hook = &groups[0]["hooks"][0];
            let parsed: ExecHook = serde_json::from_value(hook.clone()).unwrap();
            assert_eq!(parsed.command, helper.to_str().unwrap(), "{event}");
            assert_eq!(parsed.args, vec![HOOK, dir.to_str().unwrap()], "{event}");
            assert_eq!(hook["type"], "command");
            assert_eq!(hook["async"], true);
            assert_eq!(hook["timeout"], 10);
            assert!(
                serde_json::from_value::<ExecHook>(legacy(&helper, dir)["hooks"][0].clone())
                    .is_err()
            );
        }
    }

    #[test]
    fn exact_legacy_groups_migrate_and_deduplicate_without_changing_user_policy() {
        let dir = Path::new("C:/synthetic/live");
        let old_helper = helper('a');
        let new_helper = helper('b');
        let user_permission = json!({"matcher":"Bash","hooks":[{"type":"command","command":"userPermissionRequest","timeout":5}]});
        let security = json!({"matcher":"*","hooks":[{"type":"command","command":"security-check","timeout":20}]});
        let mut original = json!({"disableRemoteControl":true,"permissions":{"deny":["Bash(*)"]},
            "hooks":{"PermissionRequest":[user_permission.clone()],"PreToolUse":[security.clone()],"CustomEvent":[security.clone()]}});
        for event in EVENTS {
            if original["hooks"].get(event).is_none() {
                original["hooks"][event] = json!([security.clone()]);
            }
        }
        let mut settings = original.clone();
        for event in EVENTS {
            settings["hooks"][event].as_array_mut().unwrap().extend([
                legacy(&old_helper, dir),
                legacy(&old_helper, dir),
                group(&old_helper, dir),
            ]);
        }
        assert!(merge_hooks(&mut settings, Some(&new_helper), dir).unwrap());
        for event in EVENTS {
            assert_eq!(settings["hooks"][event][0], original["hooks"][event][0]);
            assert_eq!(
                settings["hooks"][event],
                json!([original["hooks"][event][0], group(&new_helper, dir)])
            );
        }
        assert_eq!(
            settings["hooks"]["PreToolUse"],
            original["hooks"]["PreToolUse"]
        );
        assert_eq!(
            settings["hooks"]["CustomEvent"],
            original["hooks"]["CustomEvent"]
        );
        assert_eq!(settings["permissions"], original["permissions"]);
        assert_eq!(
            settings["disableRemoteControl"],
            original["disableRemoteControl"]
        );
        let migrated = settings.clone();
        assert!(!merge_hooks(&mut settings, Some(&new_helper), dir).unwrap());
        assert_eq!(settings, migrated);
        assert!(merge_hooks(&mut settings, None, dir).unwrap());
        assert_eq!(settings, original);
        assert!(!merge_hooks(&mut settings, None, dir).unwrap());
    }

    #[test]
    fn edited_mixed_and_foreign_groups_are_never_migrated_or_removed() {
        let helper = helper('a');
        let dir = Path::new("C:/synthetic/live");
        let security = json!({"type":"command","command":"security-check"});
        let mut foreign = vec![];
        for base in [group(&helper, dir), legacy(&helper, dir)] {
            for (key, value) in [
                ("timeout", json!(11)),
                ("async", json!(false)),
                ("shell", json!("powershell")),
                ("type", json!("prompt")),
            ] {
                let mut edited = base.clone();
                edited["hooks"][0][key] = value;
                foreign.push(edited);
            }
            let mut matcher = base.clone();
            matcher["matcher"] = json!("Bash");
            foreign.push(matcher);
            let mut mixed = base;
            mixed["hooks"]
                .as_array_mut()
                .unwrap()
                .push(security.clone());
            foreign.push(mixed);
        }
        let mut extra_arg = legacy(&helper, dir);
        extra_arg["hooks"][0]["command"]["args"]
            .as_array_mut()
            .unwrap()
            .push(json!("user-option"));
        foreign.extend([
            extra_arg,
            legacy(&helper, Path::new("C:/other/live")),
            group(&helper, Path::new("C:/other/live")),
            legacy(Path::new("C:/user-security.exe"), dir),
            group(Path::new("C:/user-security.exe"), dir),
            json!({"hooks":[{"type":"command","command":{"exe":null,"args":[HOOK,dir]},"async":true,"timeout":10}]}),
        ]);
        for candidate in &foreign {
            assert!(!owned_group(candidate, dir), "{candidate}");
        }
        let original = json!({"hooks":{"PermissionRequest":foreign.clone()}});
        let mut settings = original.clone();
        assert!(!merge_hooks(&mut settings, None, dir).unwrap());
        assert_eq!(settings, original);
        for mut empty in [json!({}), json!({"hooks":{}}), json!({"hooks":{"Stop":[]}})] {
            let before = empty.clone();
            assert!(!merge_hooks(&mut empty, None, dir).unwrap());
            assert_eq!(empty, before);
        }
        assert!(merge_hooks(&mut settings, Some(&helper), dir).unwrap());
        assert!(merge_hooks(&mut settings, None, dir).unwrap());
        assert_eq!(settings, original);
    }

    #[test]
    fn disabling_exact_legacy_hooks_is_idempotent_and_does_not_leave_empty_events() {
        let helper = helper('a');
        let dir = Path::new("C:/synthetic/live");
        let original = json!({"model":"keep"});
        let mut settings = original.clone();
        settings["hooks"] = json!({});
        for event in EVENTS {
            settings["hooks"][event] = json!([legacy(&helper, dir)]);
        }
        assert!(merge_hooks(&mut settings, None, dir).unwrap());
        assert_eq!(settings, original);
        assert!(!merge_hooks(&mut settings, None, dir).unwrap());
        assert_eq!(settings, original);
    }

    #[test]
    fn invalid_event_shapes_do_not_publish_a_partial_migration() {
        let helper = helper('a');
        let dir = Path::new("C:/synthetic/live");
        for initial in [
            json!([]),
            json!({"hooks":[]}),
            json!({"hooks":{"SessionStart":[legacy(&helper, dir)],"ConfigChange":{}}}),
        ] {
            for enabled in [false, true] {
                let mut settings = initial.clone();
                assert!(merge_hooks(
                    &mut settings,
                    if enabled { Some(&helper) } else { None },
                    dir
                )
                .is_err());
                assert_eq!(settings, initial);
            }
        }
    }
}
