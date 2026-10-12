use super::{
    storage::{same_identity, user_home, write_json, Manager, ReloginTarget},
    *,
};
use crate::prompt_cache_resume::ResumeTarget;
use anyhow::{ensure, Context, Result};
use chrono::Utc;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Stdio,
    sync::LazyLock,
};
use tokio::{
    process::{Child, Command},
    sync::Mutex,
    time::{timeout, Duration},
};

pub const CLAUDE_CODE_VERSION: &str = "2.1.296";
pub static OPERATION: Mutex<()> = Mutex::const_new(());
pub static WARMUP: Mutex<()> = Mutex::const_new(());
enum SessionChild {
    #[cfg(any(test, not(windows)))]
    Direct(Child),
    #[cfg(windows)]
    Console(super::pwsh_session::Session),
}
impl SessionChild {
    #[cfg(windows)]
    fn identity(&self) -> Result<super::running_count::Identity> {
        let (pid, created) = match self {
            #[cfg(test)]
            Self::Direct(child) => {
                let pid = child.id().context("Owned Claude process exited")?;
                (pid, super::remote_control_console::process(pid)?.created)
            }
            Self::Console(child) => child.identity()?,
        };
        Ok(super::running_count::Identity { pid, created })
    }
    fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>> {
        match self {
            #[cfg(any(test, not(windows)))]
            Self::Direct(child) => Ok(child.try_wait()?),
            #[cfg(windows)]
            Self::Console(child) => child.try_wait(),
        }
    }
    async fn kill(&mut self) -> Result<()> {
        match self {
            #[cfg(any(test, not(windows)))]
            Self::Direct(child) => Ok(child.kill().await?),
            #[cfg(windows)]
            Self::Console(child) => child.stop(),
        }
    }
}
static SESSIONS: LazyLock<Mutex<HashMap<String, SessionChild>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static LOGIN: Mutex<Option<Login>> = Mutex::const_new(None);
struct Login {
    child: Child,
    stage: LoginProfile,
    mode: LoginMode,
    started: std::time::Instant,
}
enum LoginMode {
    Add { name: String },
    Relogin { target: ReloginTarget },
}

/// Only a newly created, privately owned staging directory may be removed.
/// Never copy a target profile (or its sessions/settings) into this directory.
struct LoginProfile {
    parent: PathBuf,
    dir: PathBuf,
    owner: String,
}
impl LoginProfile {
    fn create(m: &Manager, id: &str) -> Result<Self> {
        // UUID validation also prevents an accidental path traversal in cleanup.
        m.profile(id)?;
        std::fs::create_dir_all(&m.root)?;
        let root = m.root.canonicalize()?;
        let parent = root.join("login-staging");
        std::fs::create_dir_all(&parent)?;
        ensure!(
            parent.canonicalize()? == parent,
            "Claude login staging directory was redirected"
        );
        let dir = parent.join(id);
        std::fs::create_dir(&dir)?;
        let stage = Self {
            parent,
            dir,
            owner: uuid::Uuid::new_v4().to_string(),
        };
        // This marker is private ownership evidence, never a credential.
        if let Err(error) = std::fs::write(stage.dir.join(".switcher-login-owner"), &stage.owner) {
            // No process has started and no children were created. Remove only
            // this just-created empty directory if writing the marker failed.
            let _ = std::fs::remove_dir(&stage.dir);
            return Err(error).context("Could not mark the private Claude login directory");
        }
        Ok(stage)
    }
    fn cleanup(&self) -> Result<()> {
        if !self.dir.try_exists()? {
            return Ok(());
        }
        let metadata = std::fs::symlink_metadata(&self.dir)?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "Refusing to remove a redirected Claude login directory"
        );
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            ensure!(
                metadata.file_attributes() & 0x400 == 0,
                "Refusing to remove a Claude login reparse point"
            );
        }
        ensure!(
            self.parent.canonicalize()? == self.parent
                && self.dir.canonicalize()? == self.dir
                && self.dir.parent() == Some(self.parent.as_path())
                && std::fs::read_to_string(self.dir.join(".switcher-login-owner"))? == self.owner,
            "Refusing to remove an unowned Claude login directory"
        );
        // remove_dir_all does not follow child symlinks. The absolute root was
        // checked above and can only be this login's private staging directory.
        std::fs::remove_dir_all(&self.dir)?;
        Ok(())
    }
}
impl Drop for LoginProfile {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

pub fn executable() -> Result<std::path::PathBuf> {
    // The official native installer can use HOME on Windows even when Claude's
    // auth status reports USERPROFILE for its configuration directory.
    let mut installed = vec![];
    for home in [std::env::var_os("HOME"), std::env::var_os("USERPROFILE")]
        .into_iter()
        .flatten()
    {
        let versions = std::path::PathBuf::from(home).join(".local/share/claude/versions");
        if let Ok(entries) = std::fs::read_dir(versions) {
            for entry in entries.flatten() {
                if let Some(version) = stable_version(&entry.file_name().to_string_lossy()) {
                    if entry.path().is_file() {
                        installed.push((version, entry.path()));
                    }
                }
            }
        }
    }
    if let Some((_, path)) = installed.into_iter().max_by_key(|(version, _)| *version) {
        return Ok(path);
    }
    let home = user_home()?;
    let name = if cfg!(windows) {
        "claude.exe"
    } else {
        "claude"
    };
    let native = home.join(".local/bin").join(name);
    if native.is_file() {
        return Ok(native);
    }
    for path in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        let exe = path.join(name);
        if exe.is_file() {
            return Ok(exe);
        }
    }
    anyhow::bail!("Claude Code is not installed. Install the official native CLI first.")
}
fn stable_version(value: &str) -> Option<[u32; 3]> {
    let parts = value.split('.').collect::<Vec<_>>();
    if parts.len() != 3
        || parts
            .iter()
            .any(|part| part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    Some([
        parts[0].parse().ok()?,
        parts[1].parse().ok()?,
        parts[2].parse().ok()?,
    ])
}
fn command(config_dir: &Path, visible: bool) -> Result<Command> {
    let mut c = Command::new(executable()?);
    c.env("CLAUDE_CONFIG_DIR", config_dir);
    // Subscription profiles must not accidentally use externally configured API billing.
    for key in [
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "ANTHROPIC_PROFILE",
        "ANTHROPIC_BASE_URL",
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_VERTEX",
        "CLAUDE_CODE_USE_FOUNDRY",
    ] {
        c.env_remove(key);
    }
    #[cfg(windows)]
    c.creation_flags(if visible { 0x00000010 } else { 0x08000000 });
    #[cfg(not(windows))]
    let _ = visible;
    Ok(c)
}
pub async fn auth_status(dir: &Path) -> Result<Value> {
    let output = timeout(
        Duration::from_secs(15),
        command(dir, false)?
            .args(["auth", "status"])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("Claude authentication check timed out")??;
    let status: Value = serde_json::from_slice(&output.stdout)
        .context("Claude did not return authentication status JSON")?;
    ensure!(
        output.status.success()
            && status["loggedIn"] == true
            && status["authMethod"] == "claude.ai",
        "Sign in with a Claude Code subscription account"
    );
    Ok(status)
}
pub async fn start_login(m: &Manager, name: String) -> Result<Value> {
    ensure!(
        !name.trim().is_empty() && name.chars().count() <= 100,
        "Enter an account name"
    );
    let mut pending = LOGIN.lock().await;
    ensure!(pending.is_none(), "A Claude login is already in progress");
    begin_login(m, &mut pending, LoginMode::Add { name })
}
pub async fn start_relogin(m: &Manager, id: &str) -> Result<Value> {
    let mut pending = LOGIN.lock().await;
    ensure!(pending.is_none(), "A Claude login is already in progress");
    let _operation = timeout(Duration::from_secs(15), OPERATION.lock())
        .await
        .context("Another Claude account operation is in progress")?;
    let manager = m.clone();
    let id = id.to_owned();
    let target = tokio::task::spawn_blocking(move || manager.relogin_target(&id)).await??;
    begin_login(m, &mut pending, LoginMode::Relogin { target })
}
fn begin_login(m: &Manager, pending: &mut Option<Login>, mode: LoginMode) -> Result<Value> {
    let id = uuid::Uuid::new_v4().to_string();
    let stage = LoginProfile::create(m, &id)?;
    // The official login command owns the browser flow and credential refresh,
    // with no inherited target/live credentials or project working directory.
    let child = command(&stage.dir, true)?
        .current_dir(&stage.dir)
        .args(["auth", "login", "--claudeai"])
        .kill_on_drop(true)
        .spawn()?;
    *pending = Some(Login {
        child,
        stage,
        mode,
        started: std::time::Instant::now(),
    });
    Ok(json!({"login_id":id,"message":"Claude Code 로그인 창과 브라우저에서 로그인을 완료하세요."}))
}
pub async fn complete_login(m: &Manager) -> Result<Value> {
    let mut pending = LOGIN.lock().await;
    // Take first: every terminal error (including try_wait/status/CAS failure)
    // releases LOGIN and drops only the privately owned staging directory.
    let mut login = pending.take().context("No Claude login is in progress")?;
    if login.started.elapsed() > Duration::from_secs(600) {
        let _ = login.child.kill().await;
        anyhow::bail!("Claude login timed out");
    }
    let exit = match login.child.try_wait() {
        Ok(exit) => exit,
        Err(error) => {
            let _ = login.child.kill().await;
            return Err(error).context("Could not check Claude login process");
        }
    };
    let Some(exit) = exit else {
        *pending = Some(login);
        return Ok(json!({"pending":true}));
    };
    ensure!(
        exit.success(),
        "Claude login did not complete. Retry the login."
    );
    auth_status(&login.stage.dir).await?;
    let credentials = Manager::read_config(&login.stage.dir.join(".credentials.json"))?;
    let config = Manager::read_config(&login.stage.dir.join(".claude.json"))?;
    let account = match login.mode {
        LoginMode::Add { name } => {
            let account = m.add(&name, &credentials, &config, &json!({}))?;
            if m.settings()?.capture_usage {
                install_bridge(m, &account.id, false)?;
            }
            account
        }
        LoginMode::Relogin { target } => {
            let _operation = timeout(Duration::from_secs(15), OPERATION.lock())
                .await
                .context("Another Claude account operation is in progress")?;
            let manager = m.clone();
            tokio::task::spawn_blocking(move || {
                manager.commit_relogin(&target, &credentials, &config)
            })
            .await??
        }
    };
    Ok(json!({"pending":false,"account_id":account.id}))
}
pub async fn cancel_login() -> Result<()> {
    if let Some(mut login) = LOGIN.lock().await.take() {
        let _ = login.child.kill().await;
    }
    Ok(())
}
pub async fn open(m: &Manager, id: &str) -> Result<()> {
    open_session(m, id, None).await
}

pub(crate) async fn resume_cache_session(
    m: &Manager,
    id: &str,
    target: &ResumeTarget,
) -> Result<()> {
    // The detached caller owns history/identity validation and the thread lease.
    open_session(m, id, Some(target)).await
}

async fn open_session(m: &Manager, id: &str, resume: Option<&ResumeTarget>) -> Result<()> {
    let account = m.account(id)?;
    let profile = m.profile(id)?;
    auth_status(&profile).await?;
    super::remote_control::apply(m, id, false)?;
    if m.settings()?.capture_usage {
        install_bridge(m, id, false)?;
    }
    let mut sessions = SESSIONS.lock().await;
    if let Some(child) = sessions.get_mut(id) {
        if child.try_wait()?.is_none() {
            anyhow::bail!("This Claude profile already has a Switcher-launched window");
        }
    }
    #[cfg(windows)]
    let child = {
        let settings = m.settings()?;
        let mut cmd = command(&profile, true)?;
        configure_cache_protection(&mut cmd, &profile, settings.protect_prompt_cache)?;
        // Always load the passive monitor for this managed interactive session,
        // so the account option can be enabled/disabled without restarting it.
        let plugin = crate::prompt_cache_keepalive::prepare(m, id, &profile)?;
        configure_interactive_session(&mut cmd, &plugin, &settings.model, resume)?;
        SessionChild::Console(super::pwsh_session::Session::launch(&cmd, &profile).await?)
    };
    #[cfg(not(windows))]
    let child = {
        anyhow::bail!(
            "Open this profile from a terminal with CLAUDE_CONFIG_DIR set to its profile directory"
        );
        #[allow(unreachable_code)]
        SessionChild::Direct(command(&profile, true)?.spawn()?)
    };
    sessions.insert(account.id, child);
    Ok(())
}

fn configure_interactive_session(
    cmd: &mut Command,
    plugin: &Path,
    new_model: &str,
    resume: Option<&ResumeTarget>,
) -> Result<()> {
    if let Some(target) = resume {
        ensure!(
            target.transcript.is_absolute() && target.cwd.is_absolute(),
            "Claude resume requires an absolute transcript path and working directory"
        );
        // Official --resume accepts an absolute .jsonl path. Without a fork it
        // retains the transcript's session_id, validated by the parent against
        // target.session_id. Do not mint an ID, copy history, or send a prompt.
        // https://code.claude.com/docs/en/cli-reference
        cmd.arg("--plugin-dir").arg(plugin);
        cmd.current_dir(&target.cwd)
            .arg("--resume")
            .arg(&target.transcript);
        if let Some(model) = &target.model {
            cmd.arg("--model").arg(model);
        }
        if let Some(effort) = &target.effort {
            cmd.arg("--effort").arg(effort);
        }
    } else {
        cmd.arg("--plugin-dir")
            .arg(plugin)
            .args(["--model", new_model]);
    }
    Ok(())
}

pub(crate) async fn cache_session_running(id: &str) -> Result<bool> {
    // Poll only this process's owned child. The detached helper keeps this map
    // alive independently of the GUI; the parent owns the cross-process lease.
    let mut sessions = SESSIONS.lock().await;
    match sessions.get_mut(id) {
        Some(child) => Ok(child.try_wait()?.is_none()),
        None => Ok(false),
    }
}
pub async fn close(id: &str) -> Result<bool> {
    if let Some(mut child) = SESSIONS.lock().await.remove(id) {
        if child.try_wait()?.is_none() {
            child.kill().await?;
            return Ok(true);
        }
    }
    Ok(false)
}
#[cfg(not(windows))]
async fn running() -> Result<Vec<String>> {
    let mut sessions = SESSIONS.lock().await;
    let mut active = vec![];
    for (id, child) in sessions.iter_mut() {
        if child.try_wait()?.is_none() {
            active.push(id.clone());
        }
    }
    Ok(active)
}
pub(super) struct RunningState {
    pub managed_running_ids: Vec<String>,
    pub count: usize,
    pub error: Option<String>,
    #[cfg(all(windows, test))]
    confirmed_processes: Vec<super::running_count::Identity>,
}
pub(super) async fn running_state(m: &Manager) -> Result<RunningState> {
    #[cfg(windows)]
    {
        let mut managed = vec![];
        {
            let mut sessions = SESSIONS.lock().await;
            for (id, child) in sessions.iter_mut() {
                if child.try_wait()?.is_none() {
                    managed.push((id.clone(), child.identity()?));
                }
            }
        }
        let observation = super::running_observation::discover(m);
        let discovered = observation.identities;
        let mut error = observation.error;
        let mut generations = std::collections::BTreeMap::<u32, Vec<u64>>::new();
        for identity in managed
            .iter()
            .map(|(_, identity)| identity)
            .chain(&discovered)
        {
            generations
                .entry(identity.pid)
                .or_default()
                .push(identity.created);
        }
        let (managed_running_ids, count, _confirmed_processes) =
            super::running_count::merge(&managed, &discovered, |pid| {
                match super::remote_control_console::process(pid) {
                    Ok(process) if super::interactive_image(&process.image) => {
                        Some(super::running_count::Identity {
                            pid,
                            created: process.created,
                        })
                    }
                    Ok(_) => None,
                    Err(_) => {
                        if generations[&pid].iter().any(|created| {
                            !super::remote_control_console::process_generation_ended(pid, *created)
                        }) {
                            error.get_or_insert_with(|| {
                                "일부 Claude 프로세스의 실행 여부를 확인할 수 없습니다.".into()
                            });
                        }
                        None
                    }
                }
            });
        Ok(RunningState {
            managed_running_ids,
            count,
            error,
            #[cfg(test)]
            confirmed_processes: _confirmed_processes,
        })
    }
    #[cfg(not(windows))]
    {
        let _ = m;
        let managed_running_ids = running().await?;
        Ok(RunningState {
            count: managed_running_ids.len(),
            managed_running_ids,
            error: Some("현재 플랫폼의 전체 Claude 실행 수는 아직 확인할 수 없습니다.".into()),
        })
    }
}
pub async fn activate(m: &Manager, id: &str) -> Result<Value> {
    activate_with_expectation(m, id, None).await
}
pub async fn activate_automatic(m: &Manager, id: &str, source: &str, force: bool) -> Result<Value> {
    activate_with_expectation(m, id, Some((source, force))).await
}
async fn activate_with_expectation(
    m: &Manager,
    id: &str,
    expected: Option<(&str, bool)>,
) -> Result<Value> {
    let _gate = timeout(Duration::from_secs(15), OPERATION.lock())
        .await
        .context("Another Claude account operation is in progress")?;
    let profile = m.profile(id)?;
    m.account(id)?;
    auth_status(&profile).await?;
    let manager = m.clone();
    let previous = tokio::task::spawn_blocking(move || manager.sync_active()).await??;
    let settings = m.settings()?;
    // Prepare credentials before closing a window. Never terminate user-owned CLI processes.
    let target = Manager::read_config(&profile.join(".credentials.json"))?;
    super::storage::validate_credentials(&target)?;
    let closed = if settings.close_before_switch {
        if let Some(old) = previous {
            close(&old).await?
        } else {
            false
        }
    } else {
        false
    };
    let manager = m.clone();
    let target_id = id.to_owned();
    let expectation = expected.map(|(source, force)| (source.to_owned(), force));
    tokio::task::spawn_blocking(move || {
        manager.activate_checked(&target_id, |locked| {
            if let Some((source, force)) = expectation {
                super::commands::validate_rotation(locked, &source, &target_id, force)?;
            }
            Ok(())
        })
    })
    .await??;
    super::remote_control::apply(m, id, false)?;
    super::remote_control::apply(m, id, true)?;
    if settings.capture_usage {
        install_bridge(m, id, true)?;
    }
    if settings.reopen_after_switch {
        open(m, id).await?;
    }
    Ok(
        json!({"account_id":id,"auth_file_verified":true,"closed_managed_window":closed,"opened_new_window":settings.reopen_after_switch,
            "remote_control_startup_enabled":settings.auto_remote_control==Some(true),
            "remote_control_existing_session_reconnect_queued":settings.auto_remote_control==Some(true),
            "message":if settings.auto_remote_control==Some(true) {
                "Claude 인증 파일을 적용했습니다. 새 대화는 시작 시 연결하고, 관측된 Windows 터미널 대화는 계정 변경 뒤 비어 있는 대기 프롬프트에서 /rc로 재연결합니다. 작업·입력·승인 중이면 보류하며 최초 연결 동의는 직접 확인해야 합니다."
            } else { "Claude 인증 파일을 적용했습니다. 기존 대화는 유지되며 새 세션부터 선택한 계정을 사용합니다." }}),
    )
}
pub fn install_bridge(m: &Manager, id: &str, live: bool) -> Result<()> {
    let account = m.account(id)?;
    let dir = if live {
        m.live_dir.clone()
    } else {
        m.profile(id)?
    };
    let _credentials = if live {
        Some(super::locks::CredentialsGuard::acquire(&dir)?)
    } else {
        None
    };
    let _lock = crate::auth::persistence::lock_file(&dir.join("switcher-settings.lock"))?;
    if live {
        let config = Manager::read_config(&m.live_config)?;
        let credentials = Manager::read_config(&dir.join(".credentials.json"))?;
        let saved = Manager::read_config(&m.profile(id)?.join(".credentials.json"))?;
        ensure!(
            same_identity(&account, &config, &credentials, &saved),
            "Claude statusline setting account changed"
        );
    }
    let path = dir.join("settings.json");
    let mut settings = Manager::read_config(&path)?;
    let before = settings.clone();
    let current = settings.get("statusLine").cloned().unwrap_or(Value::Null);
    let original = dir.join(".switcher-original-statusline.json");
    let owned = current["command"]
        .as_str()
        .is_some_and(|s| s.contains("--claude-statusline"));
    if !owned {
        write_json(&original, &json!({"statusLine":current}))?;
    }
    let executable = std::env::current_exe()?;
    ensure!(
        !executable.to_string_lossy().contains('"') && !dir.to_string_lossy().contains('"'),
        "Unsupported executable path"
    );
    let prefix = if cfg!(windows)
        && statusline_shell("")
            .as_std()
            .get_program()
            .to_string_lossy()
            .ends_with("pwsh.exe")
    {
        "& "
    } else {
        ""
    };
    let command = format!(
        "{prefix}\"{}\" --claude-statusline {} \"{}\"",
        executable.to_string_lossy().replace('\\', "/"),
        id,
        dir.to_string_lossy().replace('\\', "/")
    );
    let mut bridge = if current.is_object() {
        current
    } else {
        json!({})
    };
    bridge["type"] = json!("command");
    bridge["command"] = json!(command);
    settings["statusLine"] = bridge;
    // The always-on attachment task may revisit a live profile. Do not trigger
    // Claude's settings watcher, backups or I/O when the command is unchanged.
    if settings != before {
        write_json(&path, &settings)?;
    }
    Ok(())
}
pub fn remove_bridge(m: &Manager, id: &str, live: bool) -> Result<()> {
    let dir = if live {
        m.live_dir.clone()
    } else {
        m.profile(id)?
    };
    let _lock = crate::auth::persistence::lock_file(&dir.join("switcher-settings.lock"))?;
    let mut settings = Manager::read_config(&dir.join("settings.json"))?;
    if settings["statusLine"]["command"]
        .as_str()
        .is_some_and(|s| s.contains("--claude-statusline"))
    {
        let original = Manager::read_config(&dir.join(".switcher-original-statusline.json"))?;
        if original["statusLine"].is_null() {
            settings.as_object_mut().unwrap().remove("statusLine");
        } else {
            settings["statusLine"] = original["statusLine"].clone();
        }
        write_json(&dir.join("settings.json"), &settings)?;
    }
    Ok(())
}
pub fn usage_from_statusline(
    id: &str,
    plan: Option<String>,
    data: &Value,
) -> crate::types::UsageInfo {
    let rate = &data["rate_limits"];
    let percentage = |name: &str| {
        rate[name]["used_percentage"]
            .as_f64()
            .filter(|n| n.is_finite() && (0.0..=100.0).contains(n))
    };
    let reset = |name: &str| rate[name]["resets_at"].as_i64();
    crate::types::UsageInfo {
        account_id: id.into(),
        plan_type: plan,
        primary_used_percent: percentage("five_hour"),
        primary_window_minutes: percentage("five_hour").map(|_| 300),
        primary_resets_at: reset("five_hour"),
        secondary_used_percent: percentage("seven_day"),
        secondary_window_minutes: percentage("seven_day").map(|_| 10080),
        secondary_resets_at: reset("seven_day"),
        has_credits: None,
        unlimited_credits: None,
        credits_balance: None,
        error: None,
    }
}
pub fn collect(m: &Manager, id: &str, dir: &Path, data: &Value) -> Result<()> {
    let account = m.account(id)?;
    let profile = m.profile(id)?;
    ensure!(
        dir == profile || dir == m.live_dir,
        "Unrecognized Claude profile directory"
    );
    if !m.settings()?.capture_usage {
        return Ok(());
    }
    let config_path = if dir == m.live_dir {
        m.live_config.clone()
    } else {
        profile.join(".claude.json")
    };
    let config = Manager::read_config(&config_path)?;
    let credentials = Manager::read_config(&dir.join(".credentials.json"))?;
    let saved = Manager::read_config(&profile.join(".credentials.json"))?;
    ensure!(
        same_identity(&account, &config, &credentials, &saved),
        "Claude profile identity changed; reconnect the usage bridge"
    );
    // Cache observations have their own timestamps and never freshen quota.
    // Unsupported/partial statusline payloads must not break usage collection.
    let enriched = crate::prompt_cache_transcript::enrich_statusline(data);
    let _ = crate::prompt_cache::record_claude(&profile, &enriched);
    let cache = crate::prompt_cache::read_status(&profile, Utc::now().timestamp());
    let output_tokens = enriched
        .pointer("/context_window/current_usage/output_tokens")
        .and_then(Value::as_i64)
        .filter(|n| *n >= 0);
    let _ = crate::prompt_cache_keepalive::observe(m, id, dir, &cache, output_tokens);
    let _lock = crate::auth::persistence::lock_file(&profile.join("usage.lock"))?;
    let previous =
        crate::auth::persistence::read_json::<UsageReceipt>(&profile.join("usage.json"))?;
    if let Some(receipt) = &previous {
        super::monitor::remember_usage_sync(m, receipt)?;
    }
    let mut receipt = merge_statusline_receipt(
        id,
        account.plan_type,
        data,
        previous,
        Utc::now().timestamp(),
    );
    if let Some(session) = data["session_id"].as_str().filter(|s| s.len() <= 128) {
        let session_usage = SessionUsage {
            input_tokens: data
                .pointer("/context_window/total_input_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(0)
                .max(0),
            output_tokens: data
                .pointer("/context_window/total_output_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(0)
                .max(0),
            cost_usd: data
                .pointer("/cost/total_cost_usd")
                .and_then(Value::as_f64)
                .unwrap_or(0.0)
                .max(0.0),
            day: Utc::now().format("%Y-%m-%d").to_string(),
        };
        receipt.sessions.insert(session.into(), session_usage);
    }
    super::monitor::remember_usage_sync(m, &receipt)?;
    write_json(&profile.join("usage.json"), &receipt)
}

fn merge_statusline_receipt(
    id: &str,
    plan: Option<String>,
    data: &Value,
    previous: Option<UsageReceipt>,
    now: i64,
) -> UsageReceipt {
    let next = usage_from_statusline(id, plan, data);
    let mut receipt = previous.unwrap_or_else(|| UsageReceipt {
        account_id: id.into(),
        observed_at: 0,
        usage: next.clone(),
        sessions: Default::default(),
    });
    // Statusline fields are optional and disappear before/after API responses.
    // A missing window must not erase a known weekly limit or make it fresh.
    let loses_window = (receipt.usage.primary_used_percent.is_some()
        && next.primary_used_percent.is_none())
        || (receipt.usage.secondary_used_percent.is_some()
            && next.secondary_used_percent.is_none());
    let has_quota = next.primary_used_percent.is_some() || next.secondary_used_percent.is_some();
    if has_quota && !loses_window {
        receipt.usage = next;
        receipt.observed_at = now;
    }
    receipt
}
pub fn statusline_entry(id: &str, directory: &str) -> Result<()> {
    let manager = Manager::current()?;
    let dir = Path::new(directory);
    manager.account(id)?;
    ensure!(
        dir == manager.profile(id)? || dir == manager.live_dir,
        "Unrecognized Claude profile directory"
    );
    let mut bytes = vec![];
    std::io::stdin()
        .take(256 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 256 * 1024,
        "Claude statusline input is too large"
    );
    let data: Value = serde_json::from_slice(&bytes)?;
    let collected = collect(&manager, id, dir, &data);
    let original = Manager::read_config(&dir.join(".switcher-original-statusline.json"))?;
    if let Some(old_command) = original
        .pointer("/statusLine/command")
        .and_then(Value::as_str)
    {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let output = rt.block_on(async {
            let mut cmd = statusline_shell(old_command);
            #[cfg(windows)]
            cmd.creation_flags(0x08000000);
            cmd.stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            let mut child = cmd.spawn()?;
            if let Some(mut input) = child.stdin.take() {
                use tokio::io::AsyncWriteExt;
                input.write_all(&bytes).await?;
            }
            timeout(Duration::from_secs(2), child.wait_with_output())
                .await
                .context("Previous statusline timed out")?
                .map_err(anyhow::Error::from)
        });
        if let Ok(output) = output {
            std::io::stdout().write_all(&output.stdout)?;
        }
    } else {
        let usage = usage_from_statusline(id, None, &data);
        let mut fields = vec!["Claude".to_string()];
        if let Some(p) = usage.primary_used_percent {
            fields.push(format!("5h {:.0}%", 100.0 - p));
        }
        if let Some(p) = usage.secondary_used_percent {
            fields.push(format!("7d {:.0}%", 100.0 - p));
        }
        println!("{}", fields.join(" · "));
    }
    collected
}

fn statusline_shell(script: &str) -> Command {
    // Match the official CLI's host shell so existing Bash status lines retain
    // their quoting, pipes and environment expansion on native Windows.
    #[cfg(windows)]
    {
        let mut candidates = vec![];
        if let Some(path) = std::env::var_os("CLAUDE_CODE_GIT_BASH_PATH") {
            candidates.push(std::path::PathBuf::from(path));
        }
        for root in ["ProgramFiles", "LOCALAPPDATA"] {
            if let Some(path) = std::env::var_os(root) {
                candidates.push(std::path::PathBuf::from(path).join("Git/bin/bash.exe"));
            }
        }
        for path in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
            if path.join("git.exe").is_file() {
                if let Some(root) = path.parent() {
                    candidates.push(root.join("bin/bash.exe"));
                }
            }
        }
        if let Some(bash) = candidates.into_iter().find(|p| p.is_file()) {
            let mut cmd = Command::new(bash);
            cmd.args(["-c", script]);
            return cmd;
        }
        let mut cmd = Command::new("pwsh.exe");
        cmd.args(["-NoProfile", "-NonInteractive", "-Command", script]);
        cmd
    }
    #[cfg(not(windows))]
    {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", script]);
        cmd
    }
}
// Verified against the official model registry on 2026-10-10. Use an exact
// Haiku version so a user alias or the interactive model cannot raise costs.
const WARMUP_MODEL: &str = "claude-haiku-5-5";
const WARMUP_MAX_OUTPUT_TOKENS: &str = "1";

fn configure_cache_protection(cmd: &mut Command, dir: &Path, enabled: bool) -> Result<()> {
    if !enabled {
        return Ok(());
    }
    let settings = Manager::read_config(&dir.join("settings.json"))?;
    for (key, env) in [
        ("promptCacheTtl", "CLAUDE_CODE_PROMPT_CACHE_TTL"),
        (
            "subagentPromptCacheTtl",
            "CLAUDE_CODE_SUBAGENT_PROMPT_CACHE_TTL",
        ),
    ] {
        // Inherited forcing/disabling variables retain official precedence.
        // Respect an explicit TTL even for the isolated --setting-sources ""
        // warmup. No model, history, tools, or running process is modified.
        if std::env::var_os(env).is_none() {
            let ttl = settings[key]
                .as_str()
                .filter(|s| matches!(*s, "5m" | "1h"))
                .unwrap_or("1h");
            cmd.env(env, ttl);
        }
    }
    Ok(())
}

fn warmup_command(dir: &Path) -> Result<Command> {
    let mut cmd = command(dir, false)?;
    configure_warmup(&mut cmd, dir);
    Ok(cmd)
}

fn configure_warmup(cmd: &mut Command, dir: &Path) {
    cmd.current_dir(dir)
        .env("CLAUDE_CODE_MAX_OUTPUT_TOKENS", WARMUP_MAX_OUTPUT_TOKENS)
        .env("CLAUDE_CODE_EFFORT_LEVEL", "low")
        .env("CLAUDE_CODE_MAX_RETRIES", "0")
        .env("CLAUDE_CODE_DISABLE_TERMINAL_TITLE", "1")
        .env_remove("ANTHROPIC_MODEL")
        .env_remove("MAX_THINKING_TOKENS")
        .args([
            "-p",
            "X",
            "--model",
            WARMUP_MODEL,
            "--effort",
            "low",
            "--system-prompt",
            "Reply X.",
            // Unlike --bare, safe mode retains subscription authentication.
            "--safe-mode",
            "--output-format",
            "json",
            "--tools",
            "",
            "--strict-mcp-config",
            "--mcp-config",
            "{\"mcpServers\":{}}",
            "--disable-slash-commands",
            "--no-session-persistence",
            "--setting-sources",
            "",
            "--max-turns",
            "1",
            // The CLI retries truncated replies inside a turn. A budget below
            // one output token's list cost ends the loop after the first call.
            "--max-budget-usd",
            "0.000000001",
        ])
        .stdin(Stdio::null())
        .kill_on_drop(true);
}

pub async fn warmup(m: &Manager, id: &str) -> Result<()> {
    let _gate = timeout(Duration::from_secs(5), WARMUP.lock())
        .await
        .context("Another Claude warmup is in progress")?;
    m.account(id)?;
    let process_lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(m.root.join("warmup.lock"))?;
    process_lock
        .try_lock()
        .context("Another Switcher process is warming Claude")?;
    let dir = m.profile(id)?;
    auth_status(&dir).await?;
    let mut cmd = warmup_command(&dir)?;
    configure_cache_protection(&mut cmd, &dir, m.settings()?.protect_prompt_cache)?;
    let output = timeout(Duration::from_secs(90), cmd.output())
        .await
        .context("Claude warmup timed out")??;
    validate_warmup_output(&output)
}

fn validate_warmup_output(output: &std::process::Output) -> Result<()> {
    let result: Value =
        serde_json::from_slice(&output.stdout).context("Claude warmup did not return JSON")?;
    // Reaching our budget after a confirmed one-token model response is the
    // intended warmup stop. Authentication/quota/API failures remain failures.
    if result["subtype"] == "error_max_budget_usd"
        && result["terminal_reason"] == "budget_exhausted"
        && result["num_turns"] == 1
        && result["modelUsage"][WARMUP_MODEL]["outputTokens"] == 1
        && result["api_error"].is_null()
    {
        return Ok(());
    }
    ensure!(
        output.status.success() && result["is_error"] != true,
        "Claude warmup failed. Open the profile to check login and quota."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[tokio::test]
    #[ignore = "Parent-only passive live proof; requires explicit PID, creation time and artifact path"]
    async fn readonly_native_running_state_after_gui_restart() -> Result<()> {
        let pid = std::env::var("CLAUDE_RUNNING_EXPECTED_PID")?.parse::<u32>()?;
        let created = std::env::var("CLAUDE_RUNNING_EXPECTED_CREATED")?.parse::<u64>()?;
        let artifact = PathBuf::from(
            std::env::var_os("CLAUDE_RUNNING_READBACK_PATH")
                .context("Explicit readback artifact path is required")?,
        );
        ensure!(
            artifact.is_absolute() && artifact.parent().is_some_and(Path::is_dir),
            "Readback artifact needs an existing absolute directory"
        );
        let expected = super::super::running_count::Identity { pid, created };
        // A fresh test host has no owned children, just like a restarted GUI.
        // Never clear another test's map or open/close a CLI to simulate restart.
        let result: Result<RunningState> = async {
            ensure!(
                SESSIONS.lock().await.is_empty(),
                "Run this ignored test alone"
            );
            let before = super::super::remote_control_console::process(pid)?;
            ensure!(
                before.created == created && super::super::interactive_image(&before.image),
                "Expected native PID/creation identity does not match"
            );
            let manager = Manager::current()?;
            let state = running_state(&manager).await?;
            ensure!(
                super::super::remote_control_console::process(pid)? == before,
                "Native process changed during observation"
            );
            ensure!(
                SESSIONS.lock().await.is_empty(),
                "Owned sessions changed during observation"
            );
            Ok(state)
        }
        .await;
        let report = match &result {
            Ok(state) => json!({"source":"read_only_native_running_state_empty_owned_sessions",
                "expected_pid":pid,"expected_created":created.to_string(),
                "expected_process_counted":state.confirmed_processes.contains(&expected),
                "running_process_count":state.count,"running_process_error":state.error,
                "managed_running_ids":state.managed_running_ids,
                "confirmed_processes":state.confirmed_processes.iter().map(|identity|
                    json!({"pid":identity.pid,"created":identity.created.to_string()})).collect::<Vec<_>>(),
                "rc_connection_verified":false,"input_sent":false}),
            Err(error) => json!({"source":"read_only_native_running_state_empty_owned_sessions",
                "expected_pid":pid,"expected_created":created.to_string(),
                "expected_process_counted":false,"error":error.to_string(),
                "rc_connection_verified":false,"input_sent":false}),
        };
        // This explicitly selected evidence artifact is the only write.
        std::fs::write(&artifact, serde_json::to_vec_pretty(&report)?)?;
        println!("Claude running readback: {}", artifact.display());
        let state = result?;
        ensure!(
            state.managed_running_ids.is_empty()
                && state.confirmed_processes.contains(&expected)
                && state.count == state.confirmed_processes.len(),
            "Expected native process was not counted"
        );
        ensure!(
            state.error.is_none(),
            "Count observation was incomplete; inspect readback artifact"
        );
        Ok(())
    }

    fn cache_resume_fixture() -> (PathBuf, ResumeTarget, PathBuf) {
        let root = std::env::temp_dir().join(format!("claude-resume-{}", uuid::Uuid::new_v4()));
        let cwd = root.join("saved cwd 한글");
        let history = root.join("original profile/projects/saved-project");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&history).unwrap();
        let session_id = "550e8400-e29b-41d4-a716-446655440000".to_owned();
        let transcript = history.join(format!("{session_id}.jsonl"));
        std::fs::write(
            &transcript,
            format!("{{\"sessionId\":\"{session_id}\",\"type\":\"user\",\"message\":{{\"content\":\"preserved\"}}}}\n"),
        )
        .unwrap();
        let target = ResumeTarget {
            session_id,
            transcript,
            cwd,
            model: Some("saved-model".into()),
            effort: Some("high".into()),
        };
        let plugin = root.join("selected profile/passive plugin");
        (root, target, plugin)
    }

    fn cache_command_args(cmd: &Command) -> Vec<std::ffi::OsString> {
        cmd.as_std().get_args().map(|arg| arg.to_owned()).collect()
    }

    #[test]
    fn cache_new_session_keeps_existing_launch_arguments() {
        let mut cmd = Command::new("synthetic-cli");
        let plugin = Path::new("fixture-plugin");
        configure_interactive_session(&mut cmd, plugin, "new-model", None).unwrap();
        assert_eq!(
            cache_command_args(&cmd),
            ["--plugin-dir", "fixture-plugin", "--model", "new-model"]
                .map(std::ffi::OsString::from)
        );
        assert!(cmd.as_std().get_current_dir().is_none());
    }

    #[test]
    fn cache_resume_keeps_transcript_session_cwd_and_saved_options() {
        let (root, target, plugin) = cache_resume_fixture();
        let bytes = std::fs::read(&target.transcript).unwrap();
        let record: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(record["sessionId"], target.session_id);
        let mut cmd = Command::new("synthetic-cli");
        configure_interactive_session(&mut cmd, &plugin, "new-model", Some(&target)).unwrap();
        assert_eq!(cmd.as_std().get_current_dir(), Some(target.cwd.as_path()));
        // This complete argv allows neither a new ID/fork/continue nor an
        // automatic prompt, print mode, or history replay argument.
        assert_eq!(
            cache_command_args(&cmd),
            vec![
                "--plugin-dir".into(),
                plugin.into_os_string(),
                "--resume".into(),
                target.transcript.as_os_str().to_owned(),
                "--model".into(),
                "saved-model".into(),
                "--effort".into(),
                "high".into(),
            ]
        );
        assert_eq!(std::fs::read(&target.transcript).unwrap(), bytes);
        assert_eq!(
            target.transcript.file_stem().unwrap(),
            std::ffi::OsStr::new(&target.session_id)
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cache_resume_without_saved_options_does_not_force_new_session_defaults() {
        let (root, mut target, plugin) = cache_resume_fixture();
        target.model = None;
        target.effort = None;
        let mut cmd = Command::new("synthetic-cli");
        configure_interactive_session(&mut cmd, &plugin, "new-model", Some(&target)).unwrap();
        assert_eq!(
            cache_command_args(&cmd),
            vec![
                "--plugin-dir".into(),
                plugin.into_os_string(),
                "--resume".into(),
                target.transcript.into_os_string(),
            ]
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cache_resume_configuration_preserves_original_and_selected_profile_files() {
        let (root, target, plugin) = cache_resume_fixture();
        let original = root.join("original profile");
        let selected = root.join("selected profile");
        std::fs::create_dir_all(&selected).unwrap();
        let mut paths = vec![target.transcript.clone()];
        for profile in [&original, &selected] {
            for (name, bytes) in [
                (".credentials.json", b"{\"syntheticAuth\":true}".as_slice()),
                (".claude.json", b"{\"syntheticAccount\":true}".as_slice()),
                (
                    "settings.json",
                    b"{\"model\":\"profile-model\",\"promptCacheTtl\":\"5m\"}".as_slice(),
                ),
                (
                    "history.jsonl",
                    b"{\"display\":\"preserved history\"}\n".as_slice(),
                ),
            ] {
                let path = profile.join(name);
                std::fs::write(&path, bytes).unwrap();
                paths.push(path);
            }
        }
        let before: Vec<_> = paths
            .iter()
            .map(|path| std::fs::read(path).unwrap())
            .collect();
        let selected_entries = std::fs::read_dir(&selected).unwrap().count();
        for protected in [false, true] {
            let mut cmd = Command::new("synthetic-cli");
            cmd.env("CLAUDE_CONFIG_DIR", &selected);
            configure_cache_protection(&mut cmd, &selected, protected).unwrap();
            configure_interactive_session(&mut cmd, &plugin, "new-model", Some(&target)).unwrap();
            assert_eq!(
                cmd.as_std()
                    .get_envs()
                    .find(|(key, _)| *key == "CLAUDE_CONFIG_DIR")
                    .unwrap()
                    .1,
                Some(selected.as_os_str())
            );
            for (path, bytes) in paths.iter().zip(&before) {
                assert_eq!(&std::fs::read(path).unwrap(), bytes);
            }
            assert_eq!(
                std::fs::read_dir(&selected).unwrap().count(),
                selected_entries
            );
            assert_eq!(
                std::fs::read_dir(target.transcript.parent().unwrap())
                    .unwrap()
                    .count(),
                1
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cache_resume_rejects_relative_paths_without_configuring_a_command() {
        let (root, mut target, plugin) = cache_resume_fixture();
        let transcript = target.transcript.clone();
        let cwd = target.cwd.clone();
        for relative_transcript in [true, false] {
            target.transcript = if relative_transcript {
                PathBuf::from("history.jsonl")
            } else {
                transcript.clone()
            };
            target.cwd = if relative_transcript {
                cwd.clone()
            } else {
                PathBuf::from("project")
            };
            let mut cmd = Command::new("synthetic-cli");
            assert!(
                configure_interactive_session(&mut cmd, &plugin, "new-model", Some(&target))
                    .is_err()
            );
            assert!(cache_command_args(&cmd).is_empty());
            assert!(cmd.as_std().get_current_dir().is_none());
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "Local test-process fixture; run only by cache_session_running_polls_owned_child"]
    fn cache_session_child_fixture() {
        // EOF releases this test process. It never invokes a shell or provider.
        std::io::stderr().write_all(b"ready").unwrap();
        let _ = std::io::stdin().read(&mut [0_u8; 1]);
    }

    #[tokio::test]
    async fn cache_session_running_polls_owned_child() {
        use tokio::io::AsyncReadExt;

        let id = format!("resume-child-fixture-{}", uuid::Uuid::new_v4());
        assert!(!cache_session_running(&id).await.unwrap());
        // libtest names omit the crate prefix present in module_path!().
        let (_, module) = module_path!().split_once("::").unwrap();
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args([
            "--exact",
            &format!("{module}::cache_session_child_fixture"),
            "--ignored",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(0x08000000);
        let mut child = cmd.spawn().unwrap();
        let mut ready = [0_u8; 5];
        tokio::time::timeout(
            Duration::from_secs(10),
            child.stderr.as_mut().unwrap().read_exact(&mut ready),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(&ready, b"ready");
        SESSIONS
            .lock()
            .await
            .insert(id.clone(), SessionChild::Direct(child));
        assert!(cache_session_running(&id).await.unwrap());
        {
            let mut sessions = SESSIONS.lock().await;
            let SessionChild::Direct(child) = sessions.get_mut(&id).unwrap() else {
                panic!("test fixture must be Direct")
            };
            drop(child.stdin.take());
            assert!(child.wait().await.unwrap().success());
        }
        assert!(!cache_session_running(&id).await.unwrap());
        SESSIONS.lock().await.remove(&id);
        assert!(!cache_session_running(&id).await.unwrap());
    }

    fn login_fixture() -> (PathBuf, Manager, ClaudeAccount) {
        let root =
            std::env::temp_dir().join(format!("claude-login-offline-{}", uuid::Uuid::new_v4()));
        let m = Manager {
            root: root.join("switcher"),
            live_dir: root.join("live"),
            live_config: root.join("live/.claude.json"),
        };
        let credentials = json!({"claudeAiOauth":{"accessToken":"synthetic-old","refreshToken":"synthetic-refresh","expiresAt":1}});
        let config = json!({"oauthAccount":{"accountUuid":"synthetic-uuid"}});
        write_json(&m.live_dir.join(".credentials.json"), &credentials).unwrap();
        write_json(&m.live_config, &config).unwrap();
        let account = m
            .add(
                "Saved label",
                &credentials,
                &config,
                &json!({"keep":"settings"}),
            )
            .unwrap();
        (root, m, account)
    }
    #[test]
    fn prompt_cache_protection_is_opt_in_and_preserves_explicit_ttl_and_session_isolation() {
        let (root, m, a) = login_fixture();
        let dir = m.profile(&a.id).unwrap();
        write_json(
            &dir.join("settings.json"),
            &json!({"promptCacheTtl":"5m","subagentPromptCacheTtl":"1h"}),
        )
        .unwrap();
        let mut disabled = Command::new("synthetic-cli");
        configure_cache_protection(&mut disabled, &dir, false).unwrap();
        assert_eq!(disabled.as_std().get_envs().count(), 0);
        let mut enabled = Command::new("synthetic-cli");
        configure_warmup(&mut enabled, &dir);
        let args_before: Vec<_> = enabled.as_std().get_args().map(|s| s.to_owned()).collect();
        configure_cache_protection(&mut enabled, &dir, true).unwrap();
        for (name, expected) in [
            ("CLAUDE_CODE_PROMPT_CACHE_TTL", "5m"),
            ("CLAUDE_CODE_SUBAGENT_PROMPT_CACHE_TTL", "1h"),
        ] {
            // This helper inherits process overrides instead of replacing them.
            if std::env::var_os(name).is_none() {
                assert!(enabled.as_std().get_envs().any(
                    |(key, value)| key == name && value == Some(std::ffi::OsStr::new(expected))
                ));
            }
        }
        assert_eq!(
            enabled
                .as_std()
                .get_args()
                .map(|s| s.to_owned())
                .collect::<Vec<_>>(),
            args_before
        );
        assert!(args_before.contains(&std::ffi::OsString::from("--no-session-persistence")));
        assert!(!args_before.contains(&std::ffi::OsString::from("--resume")));
        // Fresh owned fixture contains no running sessions.
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn login_staging_is_fresh_and_cleanup_requires_ownership() {
        let (root, m, a) = login_fixture();
        let stage = LoginProfile::create(&m, &uuid::Uuid::new_v4().to_string()).unwrap();
        assert_ne!(stage.dir, m.profile(&a.id).unwrap());
        assert_ne!(stage.dir, m.live_dir);
        assert_eq!(std::fs::read_dir(&stage.dir).unwrap().count(), 1);
        let marker = stage.dir.join(".switcher-login-owner");
        std::fs::write(&marker, "replacement-owner").unwrap();
        assert!(stage.cleanup().is_err());
        assert!(stage.dir.is_dir());
        std::fs::write(&marker, &stage.owner).unwrap();
        let path = stage.dir.clone();
        drop(stage);
        assert!(!path.exists());
        assert!(m
            .profile(&a.id)
            .unwrap()
            .join(".credentials.json")
            .is_file());
        assert!(m.live_config.is_file());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn login_failure_timeout_and_cancel_release_pending_and_only_remove_staging() {
        let (root, m, a) = login_fixture();
        let paths = [
            m.root.join("accounts.json"),
            m.profile(&a.id).unwrap().join(".credentials.json"),
            m.profile(&a.id).unwrap().join(".claude.json"),
            m.profile(&a.id).unwrap().join("settings.json"),
            m.live_config.clone(),
            m.live_dir.join(".credentials.json"),
        ];
        let before: Vec<_> = paths.iter().map(|p| std::fs::read(p).unwrap()).collect();
        for failure in ["failed", "timedout", "cancelled"] {
            let stage = LoginProfile::create(&m, &uuid::Uuid::new_v4().to_string()).unwrap();
            let stage_path = stage.dir.clone();
            #[cfg(windows)]
            let mut mock = {
                let mut c = Command::new("cmd.exe");
                c.args(["/C", "exit /b 1"]).creation_flags(0x08000000);
                c
            };
            #[cfg(not(windows))]
            let mut mock = {
                let mut c = Command::new("sh");
                c.args(["-c", "exit 1"]);
                c
            };
            // An already-exited local stub cannot contact Claude or open auth.
            let mut child = mock
                .kill_on_drop(true)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            child.wait().await.unwrap();
            let started = if failure == "timedout" {
                std::time::Instant::now() - Duration::from_secs(601)
            } else {
                std::time::Instant::now()
            };
            *LOGIN.lock().await = Some(Login {
                child,
                stage,
                mode: LoginMode::Relogin {
                    target: m.relogin_target(&a.id).unwrap(),
                },
                started,
            });
            if failure == "cancelled" {
                cancel_login().await.unwrap();
            } else {
                let error = complete_login(&m).await.err().unwrap().to_string();
                assert!(error.contains(if failure == "timedout" {
                    "timed out"
                } else {
                    "did not complete"
                }));
            }
            assert!(LOGIN.lock().await.is_none());
            assert!(!stage_path.exists());
            for (path, bytes) in paths.iter().zip(&before) {
                assert_eq!(&std::fs::read(path).unwrap(), bytes);
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn cli_versions_sort_numerically_and_exclude_prereleases() {
        assert!(stable_version("2.1.296") > stable_version("2.1.75"));
        for invalid in [
            "2.1.297-beta",
            "2.1",
            "2.1.296.exe",
            "../claude",
            "+2.1.296",
        ] {
            assert!(stable_version(invalid).is_none());
        }
    }
    #[test]
    fn partial_statusline_does_not_erase_or_freshen_known_weekly_quota() {
        let original = UsageReceipt {
            account_id: "fixture".into(),
            observed_at: 100,
            usage: usage_from_statusline(
                "fixture",
                None,
                &json!({"rate_limits": {
                "five_hour": {"used_percentage":20,"resets_at":4000000000_i64},
                "seven_day": {"used_percentage":100,"resets_at":4000000000_i64}}}),
            ),
            sessions: Default::default(),
        };
        for data in [
            json!({}),
            json!({"rate_limits":{"five_hour":{"used_percentage":25,"resets_at":4000000000_i64}}}),
        ] {
            let retained =
                merge_statusline_receipt("fixture", None, &data, Some(original.clone()), 500);
            assert_eq!(retained.observed_at, 100);
            assert_eq!(retained.usage.secondary_used_percent, Some(100.0));
        }
        let refreshed = merge_statusline_receipt(
            "fixture",
            None,
            &json!({"rate_limits": {
            "five_hour":{"used_percentage":30,"resets_at":4000000000_i64},
            "seven_day":{"used_percentage":45,"resets_at":4000000000_i64}}}),
            Some(original),
            500,
        );
        assert_eq!(refreshed.observed_at, 500);
        assert_eq!(refreshed.usage.secondary_used_percent, Some(45.0));
    }
    #[test]
    fn warmup_always_uses_smallest_model_and_minimum_output() {
        let mut cmd = Command::new("claude");
        configure_warmup(&mut cmd, Path::new("fixture-profile"));
        let cmd = cmd.as_std();
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let flag = |name: &str| args[args.iter().position(|a| a == name).unwrap() + 1].clone();
        assert_eq!(flag("--model"), "claude-haiku-5-5");
        assert_eq!(flag("--effort"), "low");
        assert_eq!(flag("--max-turns"), "1");
        assert_eq!(flag("--max-budget-usd"), "0.000000001");
        assert_eq!(flag("--tools"), "");
        assert_eq!(flag("--mcp-config"), "{\"mcpServers\":{}}");
        assert!(args.iter().any(|a| a == "--safe-mode"));
        let env: HashMap<_, _> = cmd
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert_eq!(env["CLAUDE_CODE_MAX_OUTPUT_TOKENS"].as_deref(), Some("1"));
        assert_eq!(env["CLAUDE_CODE_EFFORT_LEVEL"].as_deref(), Some("low"));
        assert_eq!(env["CLAUDE_CODE_MAX_RETRIES"].as_deref(), Some("0"));
        assert_eq!(env["ANTHROPIC_MODEL"], None);
        assert_eq!(env["MAX_THINKING_TOKENS"], None);
    }

    #[test]
    fn warmup_budget_stop_requires_a_completed_minimal_model_response() {
        #[cfg(unix)]
        use std::os::unix::process::ExitStatusExt;
        #[cfg(windows)]
        use std::os::windows::process::ExitStatusExt;
        let completed = json!({"is_error":true,"subtype":"error_max_budget_usd","terminal_reason":"budget_exhausted","num_turns":1,"modelUsage":{WARMUP_MODEL:{"outputTokens":1}}});
        let validate = |value: &Value| {
            validate_warmup_output(&std::process::Output {
                status: std::process::ExitStatus::from_raw(1),
                stdout: serde_json::to_vec(value).unwrap(),
                stderr: Vec::new(),
            })
        };
        assert!(validate(&completed).is_ok());
        for error in ["authentication_failed", "rate_limit", "invalid_request"] {
            let mut failed = completed.clone();
            failed["api_error"] = json!(error);
            assert!(validate(&failed).is_err());
        }
        let mut no_response = completed.clone();
        no_response["modelUsage"][WARMUP_MODEL]["outputTokens"] = json!(0);
        assert!(validate(&no_response).is_err());
        let mut repeated = completed;
        repeated["num_turns"] = json!(2);
        assert!(validate(&repeated).is_err());
    }

    #[tokio::test]
    #[ignore = "Requires the installed official Claude Code CLI; uses only a local mock API"]
    async fn official_cli_sends_minimal_warmup_request() {
        for stop_reason in ["end_turn", "max_tokens"] {
            let dir =
                std::env::temp_dir().join(format!("claude-warmup-wire-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            write_json(
                &dir.join(".claude.json"),
                &json!({"hasCompletedOnboarding":true}),
            )
            .unwrap();
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let address = format!("http://{}", server.server_addr());
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let done = stop.clone();
            let handler = std::thread::spawn(move || {
                let mut messages = Vec::new();
                while !done.load(std::sync::atomic::Ordering::Relaxed) {
                    let Some(mut request) =
                        server.recv_timeout(Duration::from_millis(100)).unwrap()
                    else {
                        continue;
                    };
                    let mut body = String::new();
                    request.as_reader().read_to_string(&mut body).unwrap();
                    if !request.url().starts_with("/v1/messages") {
                        request
                            .respond(tiny_http::Response::from_string("{}").with_status_code(200))
                            .unwrap();
                        continue;
                    }
                    let data: Value = serde_json::from_str(&body).unwrap();
                    let events = [
                        (
                            "message_start",
                            json!({"type":"message_start","message":{"id":"msg_fixture","type":"message","role":"assistant","model":WARMUP_MODEL,"content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":1,"output_tokens":0}}}),
                        ),
                        (
                            "content_block_start",
                            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                        ),
                        (
                            "content_block_delta",
                            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"X"}}),
                        ),
                        (
                            "content_block_stop",
                            json!({"type":"content_block_stop","index":0}),
                        ),
                        (
                            "message_delta",
                            json!({"type":"message_delta","delta":{"stop_reason":stop_reason,"stop_sequence":null},"usage":{"output_tokens":1}}),
                        ),
                        ("message_stop", json!({"type":"message_stop"})),
                    ];
                    let stream = events
                        .into_iter()
                        .map(|(event, value)| format!("event: {event}\ndata: {value}\n\n"))
                        .collect::<String>();
                    messages.push(data);
                    request
                        .respond(
                            tiny_http::Response::from_string(stream).with_header(
                                tiny_http::Header::from_bytes("Content-Type", "text/event-stream")
                                    .unwrap(),
                            ),
                        )
                        .unwrap();
                }
                messages
            });
            let mut cmd = warmup_command(&dir).unwrap();
            // Synthetic credentials and a loopback endpoint only, never a real account.
            cmd.env("ANTHROPIC_API_KEY", "sk-ant-switcher-fixture")
                .env("ANTHROPIC_BASE_URL", address)
                .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
                .env("DISABLE_AUTOUPDATER", "1");
            let output = timeout(Duration::from_secs(40), cmd.output()).await;
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            let messages = handler.join().unwrap();
            std::fs::remove_dir_all(&dir).unwrap();
            let output = output.expect("Official CLI timed out").unwrap();
            validate_warmup_output(&output).unwrap_or_else(|error| {
                panic!(
                    "{error}; output: {}; requests: {}",
                    String::from_utf8_lossy(&output.stdout),
                    messages.len()
                )
            });
            assert_eq!(messages.len(), 1, "Unexpected additional model requests");
            assert_eq!(messages[0]["model"], WARMUP_MODEL);
            assert_eq!(messages[0]["max_tokens"], 1);
            assert_eq!(messages[0]["output_config"]["effort"], "low");
            assert!(messages[0]["tools"]
                .as_array()
                .is_none_or(|tools| tools.is_empty()));
            println!("Verified official CLI ({stop_reason}): Haiku 5.5, max_tokens=1, effort=low, one request, no tools, synthetic local API only");
        }
    }
    #[test]
    fn statusline_handles_missing_windows_without_inventing_quota() {
        let u = usage_from_statusline(
            "a",
            None,
            &json!({"rate_limits":{"seven_day":{"used_percentage":42.5,"resets_at":4000000000_i64}}}),
        );
        assert_eq!(u.primary_used_percent, None);
        assert_eq!(u.secondary_used_percent, Some(42.5));
        let r = UsageReceipt {
            account_id: "a".into(),
            observed_at: 100,
            usage: u,
            sessions: Default::default(),
        };
        assert!(!super::super::storage::eligible_usage(&r, 101));
        assert!(!super::super::storage::eligible_usage(&r, 401));
    }
    #[test]
    fn invalid_percentages_and_expired_windows_are_not_switch_candidates() {
        let u = usage_from_statusline(
            "a",
            None,
            &json!({"rate_limits":{"five_hour":{"used_percentage":101,"resets_at":4000000000_i64}}}),
        );
        assert_eq!(u.primary_used_percent, None);
        let u = usage_from_statusline(
            "a",
            None,
            &json!({"rate_limits":{"five_hour":{"used_percentage":10,"resets_at":10}}}),
        );
        assert!(!super::super::storage::eligible_usage(
            &UsageReceipt {
                account_id: "a".into(),
                observed_at: 50,
                usage: u,
                sessions: Default::default()
            },
            51
        ));
    }
    #[test]
    fn bridge_preserves_options_restores_output_and_rejects_mismatched_identity() {
        let root =
            std::env::temp_dir().join(format!("claude-bridge-test-{}", uuid::Uuid::new_v4()));
        let m = Manager {
            root: root.join("switcher/claude"),
            live_dir: root.join("live"),
            live_config: root.join("live/.claude.json"),
        };
        let credentials = json!({"claudeAiOauth":{"accessToken":"synthetic-test-access","refreshToken":"synthetic-test-refresh","expiresAt":4000000000000_i64,"subscriptionType":"max"}});
        let original = json!({"statusLine":{"type":"command","command":"echo preserved","padding":3,"refreshInterval":15},"model":"opus","hooks":{"keep":[]}});
        let a = m
            .add(
                "Fixture",
                &credentials,
                &json!({"oauthAccount":{"accountUuid":"fixture"}}),
                &original,
            )
            .unwrap();
        let profile = m.profile(&a.id).unwrap();
        install_bridge(&m, &a.id, false).unwrap();
        install_bridge(&m, &a.id, false).unwrap();
        let bridged = Manager::read_config(&profile.join("settings.json")).unwrap();
        assert_eq!(bridged["statusLine"]["padding"], 3);
        assert_eq!(bridged["model"], "opus");
        assert!(bridged["statusLine"]["command"]
            .as_str()
            .unwrap()
            .contains("--claude-statusline"));
        let data = json!({"session_id":"fixture-session","context_window":{"total_input_tokens":200,"total_output_tokens":30},"rate_limits":{"five_hour":{"used_percentage":50,"resets_at":4000000000_i64}}});
        collect(&m, &a.id, &profile, &data).unwrap();
        assert_eq!(
            m.receipt(&a.id).unwrap().unwrap().sessions["fixture-session"].input_tokens,
            200
        );
        let exported = m.backup().unwrap();
        assert_eq!(exported.accounts[0].settings, original);
        write_json(
            &profile.join(".claude.json"),
            &json!({"oauthAccount":{"accountUuid":"different"}}),
        )
        .unwrap();
        assert!(collect(&m, &a.id, &profile, &data).is_err());
        remove_bridge(&m, &a.id, false).unwrap();
        assert_eq!(
            Manager::read_config(&profile.join("settings.json")).unwrap(),
            original
        );
        let mut invalid = m.backup().unwrap();
        invalid.settings.model = "invalid".into();
        let account_bytes = std::fs::read(m.root.join("accounts.json")).unwrap();
        assert!(m.import_backup(invalid).is_err());
        assert_eq!(
            account_bytes,
            std::fs::read(m.root.join("accounts.json")).unwrap()
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
