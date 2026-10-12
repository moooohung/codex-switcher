//! Metrics for one Claude main-conversation session/model, never an account-wide cache.
//! Schema: https://code.claude.com/docs/en/statusline#prompt-cache-fields
//! `expires_at` is an explicit epoch-second deadline; polling never extends it.
use crate::auth::persistence::{atomic_write, lock_file};
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, fs::File, io::Read, path::Path};

/// Windows locks a running executable. Background session helpers use their
/// own immutable copy so a Switcher update never needs to kill a working CLI.
pub(crate) fn helper_executable() -> Result<std::path::PathBuf> {
    helper_from(
        &std::env::current_exe()?,
        &crate::auth::get_config_dir()?.join("cache-runtime"),
    )
}

fn helper_from(source: &Path, root: &Path) -> Result<std::path::PathBuf> {
    use sha2::{Digest, Sha256};
    let mut file = File::open(source)?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    let hash = format!("{:x}", digest.finalize());
    std::fs::create_dir_all(root)?;
    let _guard = lock_file(&root.join("runtime.lock"))?;
    let target = root.join(format!(
        "cache-helper-{hash}{}",
        if cfg!(windows) { ".exe" } else { "" }
    ));
    if !target.is_file() {
        let stage = root.join(format!(".helper-{}.tmp", uuid::Uuid::new_v4()));
        std::fs::copy(source, &stage)?;
        std::fs::OpenOptions::new()
            .write(true)
            .open(&stage)?
            .sync_all()?;
        std::fs::rename(&stage, &target)?;
    }
    // The filename is content addressed, but verify existing bytes too. Never
    // launch a stale or edited helper under a trusted version name.
    let mut verify = File::open(&target)?;
    let mut digest = Sha256::new();
    loop {
        let count = verify.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    ensure!(
        format!("{:x}", digest.finalize()) == hash,
        "Cache helper hash mismatch"
    );
    Ok(target)
}

const FILE: &str = "prompt-cache.json";
const MAX_BYTES: u64 = 8192;
const SOURCE: &str = "claude_statusline.prompt_cache.ttl";
const TRANSCRIPT_SOURCE: &str = "claude_transcript.usage.timestamp";
const MALFORMED: &str = "캐시 관측값이 올바르지 않아 남은 시간을 확인할 수 없습니다.";
const NO_EVIDENCE: &str =
    "이 세션·모델의 캐시 관측이 없습니다. 사용량 조회나 별도 워밍업으로 작업 캐시를 확인할 수는 없습니다.";
const NATIVE: &str = "이 계정의 관리된 Codex 세션에서 캐시를 아직 관측하지 못했습니다. 외부 세션이나 웹 브리지는 별도 워밍업으로 캐시를 확인하거나 연장할 수 없습니다.";
const SCOPE: &str = "Observed Claude main conversation only, scoped to this session and model; other sessions, models and warmups do not establish its cache lifetime.";
const KEEPALIVE_UNSUPPORTED: &str = "캐시 유지 세션을 열면 해당 세션의 문맥으로 유지 요청을 보냅니다. 외부에서 시작한 세션과 웹 브리지에는 적용되지 않습니다.";
const PREFERENCES: &str = "prompt-cache-preferences.json";
const MAX_PREFERENCES_BYTES: u64 = 65536;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PromptCacheStatus {
    pub state: String,
    pub observed_at: Option<i64>,
    pub model: Option<String>,
    pub session_id: Option<String>,
    pub cache_read_tokens: Option<i64>,
    pub cache_write_tokens: Option<i64>,
    pub uncached_input_tokens: Option<i64>,
    pub expires_at: Option<i64>,
    pub ttl_seconds: Option<i64>,
    pub ttl_source: Option<String>,
    pub reason: Option<String>,
    #[serde(default)]
    pub auto_keepalive_enabled: bool,
    /// Fresh, identity-verified live adapter connections; not cache success.
    #[serde(default)]
    pub auto_keepalive_connected_sessions: u32,
    /// Verified existing CLI processes; registration is not a timer connection.
    #[serde(default)]
    pub auto_keepalive_registered_sessions: u32,
    #[serde(default)]
    pub auto_keepalive_busy_sessions: u32,
    #[serde(default)]
    pub auto_keepalive_supported: bool,
    #[serde(default)]
    pub auto_keepalive_reason: String,
    #[serde(default)]
    pub auto_keepalive_state: String,
    #[serde(default)]
    pub auto_keepalive_next_at: Option<i64>,
    #[serde(default)]
    pub auto_keepalive_last_at: Option<i64>,
    #[serde(default)]
    pub auto_keepalive_requests: u64,
    #[serde(default)]
    pub auto_keepalive_cache_read_tokens: Option<i64>,
    #[serde(default)]
    pub auto_keepalive_output_tokens: Option<i64>,
}

pub(crate) fn unknown(reason: &str) -> PromptCacheStatus {
    PromptCacheStatus {
        state: "unknown".into(),
        observed_at: None,
        model: None,
        session_id: None,
        cache_read_tokens: None,
        cache_write_tokens: None,
        uncached_input_tokens: None,
        expires_at: None,
        ttl_seconds: None,
        ttl_source: None,
        reason: Some(reason.into()),
        auto_keepalive_enabled: false,
        auto_keepalive_connected_sessions: 0,
        auto_keepalive_registered_sessions: 0,
        auto_keepalive_busy_sessions: 0,
        auto_keepalive_supported: false,
        auto_keepalive_reason: KEEPALIVE_UNSUPPORTED.into(),
        auto_keepalive_state: "off".into(),
        auto_keepalive_next_at: None,
        auto_keepalive_last_at: None,
        auto_keepalive_requests: 0,
        auto_keepalive_cache_read_tokens: None,
        auto_keepalive_output_tokens: None,
    }
}

fn valid_id(value: &str, limit: usize) -> bool {
    !value.is_empty()
        && value.len() <= limit
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.:/".contains(&b))
}

fn identifier(value: Option<&Value>, limit: usize) -> Result<Option<String>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if valid_id(text, limit) => Ok(Some(text.clone())),
        _ => anyhow::bail!("Invalid cache observation identifier"),
    }
}

// `as_i64` deliberately rejects floats, strings, negatives and overflowing u64s.
fn number(value: Option<&Value>) -> Result<Option<i64>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let n = value.as_i64().filter(|n| *n >= 0);
            ensure!(n.is_some(), "Invalid cache observation integer");
            Ok(n)
        }
    }
}

fn observed_reason(status: &PromptCacheStatus, now: i64) -> String {
    let timing = match status.expires_at {
        Some(expiry) if expiry <= now => "Reported expiry has passed.",
        Some(_) if status.ttl_source.as_deref() == Some(TRANSCRIPT_SOURCE) => {
            "Expiry is estimated from the last matching transcript request."
        }
        Some(_) => "Expiry is explicitly reported by Claude Code.",
        None if status.ttl_seconds.is_some() => {
            "TTL is reported, but an explicit expiry is unavailable."
        }
        None => "TTL and expiry are unknown.",
    };
    format!("{SCOPE} {timing}")
}

fn validate(status: &PromptCacheStatus, now: i64) -> Result<()> {
    ensure!(
        matches!(status.state.as_str(), "observed" | "unknown"),
        "Invalid state"
    );
    ensure!(
        status.model.as_deref().is_none_or(|s| valid_id(s, 256)),
        "Invalid model"
    );
    ensure!(
        status
            .session_id
            .as_deref()
            .is_none_or(|s| valid_id(s, 128)),
        "Invalid session"
    );
    ensure!(
        status
            .reason
            .as_deref()
            .is_none_or(|s| s.len() <= 512 && !s.chars().any(char::is_control)),
        "Invalid reason"
    );
    for n in [
        status.cache_read_tokens,
        status.cache_write_tokens,
        status.uncached_input_tokens,
    ] {
        ensure!(n.is_none_or(|n| n >= 0), "Invalid token count");
    }
    ensure!(
        matches!(status.ttl_seconds, None | Some(300) | Some(3600)),
        "Invalid TTL"
    );
    ensure!(
        (status.ttl_seconds.is_none() && status.ttl_source.is_none())
            || (status.ttl_seconds.is_some()
                && matches!(
                    status.ttl_source.as_deref(),
                    Some(SOURCE) | Some(TRANSCRIPT_SOURCE)
                )),
        "Invalid TTL source"
    );
    ensure!(
        status.observed_at.is_none_or(|at| at >= 0 && at <= now),
        "Future observation"
    );
    if let Some(expiry) = status.expires_at {
        ensure!(expiry >= 0, "Invalid expiry");
        // Future expiry is legitimate; a request anchor in the future is not.
        let ttl = status.ttl_seconds.unwrap_or(3600);
        ensure!(
            expiry
                .checked_sub(ttl)
                .is_some_and(|at| at >= 0 && at <= now),
            "Invalid expiry anchor"
        );
    }
    if let Some(at) = status.observed_at {
        ensure!(
            status
                .expires_at
                .zip(status.ttl_seconds)
                .and_then(|(e, t)| e.checked_sub(t))
                == Some(at),
            "Invalid observation anchor"
        );
    }
    if status.state == "observed" {
        ensure!(
            status.model.is_some() && status.session_id.is_some(),
            "Unscoped observation"
        );
    } else {
        ensure!(
            status.observed_at.is_none()
                && status.expires_at.is_none()
                && status.ttl_seconds.is_none(),
            "Unknown cache lifetime"
        );
    }
    Ok(())
}

fn parse_claude(data: &Value, now: i64) -> Result<PromptCacheStatus> {
    ensure!(data.is_object(), "Invalid statusline object");
    let mut status = unknown(NO_EVIDENCE);
    status.session_id = identifier(data.get("session_id"), 128)?;
    if let Some(model) = data.get("model").filter(|v| !v.is_null()) {
        ensure!(model.is_object(), "Invalid model object");
        status.model = identifier(model.get("id"), 256)?;
    }
    if let Some(context) = data.get("context_window").filter(|v| !v.is_null()) {
        ensure!(context.is_object(), "Invalid context object");
        if let Some(usage) = context.get("current_usage").filter(|v| !v.is_null()) {
            ensure!(usage.is_object(), "Invalid current usage");
            status.cache_read_tokens = number(usage.get("cache_read_input_tokens"))?;
            status.cache_write_tokens = number(usage.get("cache_creation_input_tokens"))?;
            status.uncached_input_tokens = number(usage.get("input_tokens"))?;
        }
    }
    let positive = status.cache_read_tokens.is_some_and(|n| n > 0)
        || status.cache_write_tokens.is_some_and(|n| n > 0);
    let mut caching_observed = None;
    let mut ttl = None;
    let mut expiry = None;
    if let Some(cache) = data.get("prompt_cache").filter(|v| !v.is_null()) {
        ensure!(cache.is_object(), "Invalid prompt cache object");
        if let Some(value) = cache.get("caching_observed").filter(|v| !v.is_null()) {
            caching_observed = Some(
                value
                    .as_bool()
                    .ok_or_else(|| anyhow::anyhow!("Invalid caching observation"))?,
            );
        }
        ttl = match cache.get("ttl") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) if s == "5m" => Some(300),
            Some(Value::String(s)) if s == "1h" => Some(3600),
            _ => anyhow::bail!("Invalid official cache TTL"),
        };
        expiry = number(cache.get("expires_at"))?;
    }
    ensure!(
        !(positive && caching_observed == Some(false)),
        "Conflicting cache evidence"
    );
    // Validate explicit timing even when the payload has no cache evidence.
    if let Some(e) = expiry {
        ensure!(
            e.checked_sub(ttl.unwrap_or(3600))
                .is_some_and(|at| at >= 0 && at <= now),
            "Future cache request anchor"
        );
    }
    if status.model.is_some()
        && status.session_id.is_some()
        && (positive || caching_observed == Some(true))
    {
        status.state = "observed".into();
        status.ttl_seconds = ttl;
        let source = if data.pointer("/prompt_cache/source").and_then(Value::as_str)
            == Some(TRANSCRIPT_SOURCE)
        {
            TRANSCRIPT_SOURCE
        } else {
            SOURCE
        };
        status.ttl_source = ttl.map(|_| source.into());
        status.expires_at = expiry;
        // No receipt-time fallback: repeated statusline payloads keep this anchor.
        status.observed_at = if positive {
            expiry.zip(ttl).and_then(|(e, t)| e.checked_sub(t))
        } else {
            None
        };
        status.reason = Some(observed_reason(&status, now));
    }
    validate(&status, now)?;
    Ok(status)
}

/// The caller must first validate the selected account/profile identity.
/// Only these whitelisted metrics are persisted; no statusline payload is saved.
pub fn record_claude(profile: &Path, data: &Value) -> Result<()> {
    record_at(profile, data, chrono::Utc::now().timestamp())
}

fn record_at(profile: &Path, data: &Value, now: i64) -> Result<()> {
    let status = parse_claude(data, now).unwrap_or_else(|_| unknown(MALFORMED));
    let bytes = serde_json::to_vec_pretty(&status)?;
    let _lock = lock_file(&profile.join("prompt-cache.lock"))?;
    let path = profile.join(FILE);
    if std::fs::read(&path).ok().as_deref() != Some(bytes.as_slice()) {
        atomic_write(&path, &bytes)?;
    }
    Ok(())
}

pub(crate) fn read_status(profile: &Path, now: i64) -> PromptCacheStatus {
    let read = || -> Result<Option<PromptCacheStatus>> {
        let file = match File::open(profile.join(FILE)) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let mut bytes = Vec::new();
        file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() as u64 <= MAX_BYTES, "Oversized cache telemetry");
        let mut status: PromptCacheStatus = serde_json::from_slice(&bytes)?;
        validate(&status, now)?;
        if status.state == "observed" {
            status.reason = Some(observed_reason(&status, now));
        } else {
            // Never return arbitrary text from a malformed/edited local file.
            ensure!(
                matches!(
                    status.reason.as_deref(),
                    Some(NO_EVIDENCE) | Some(MALFORMED)
                ),
                "Invalid unknown reason"
            );
        }
        Ok(Some(status))
    };
    // No recovery reader: a bad primary must not resurrect an older .bak record.
    match read() {
        Ok(Some(status)) => status,
        Ok(None) => unknown("No prompt-cache telemetry recorded for this Claude profile."),
        Err(_) => unknown(MALFORMED),
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Preferences {
    version: u8,
    accounts: BTreeMap<String, BTreeMap<String, bool>>,
}

fn read_preferences(root: &Path) -> Result<Preferences> {
    let file = match File::open(root.join(PREFERENCES)) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Preferences {
                version: 1,
                accounts: BTreeMap::new(),
            });
        }
        Err(e) => return Err(e.into()),
    };
    let mut bytes = Vec::new();
    file.take(MAX_PREFERENCES_BYTES + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_PREFERENCES_BYTES,
        "Oversized cache preferences"
    );
    // Never restore an older .bak that could silently re-enable an option.
    let preferences: Preferences = serde_json::from_slice(&bytes)?;
    ensure!(
        preferences.version == 1,
        "Unsupported cache preferences version"
    );
    for (provider, accounts) in &preferences.accounts {
        ensure!(
            matches!(provider.as_str(), "codex" | "claude"),
            "Invalid cache provider"
        );
        ensure!(
            accounts.keys().all(|id| valid_id(id, 128)),
            "Invalid cache preference account"
        );
    }
    Ok(preferences)
}

pub(crate) fn preference(
    root: &Path,
    provider: &str,
    account_id: &str,
    enabled: Option<bool>,
) -> Result<bool> {
    ensure!(
        matches!(provider, "codex" | "claude") && valid_id(account_id, 128),
        "Invalid cache preference target"
    );
    let _guard = lock_file(&root.join("prompt-cache-preferences.lock"))?;
    let mut preferences = read_preferences(root)?;
    if let Some(enabled) = enabled {
        preferences
            .accounts
            .entry(provider.into())
            .or_default()
            .insert(account_id.into(), enabled);
        let bytes = serde_json::to_vec_pretty(&preferences)?;
        ensure!(
            bytes.len() as u64 <= MAX_PREFERENCES_BYTES,
            "Oversized cache preferences"
        );
        atomic_write(&root.join(PREFERENCES), &bytes)?;
    }
    Ok(preferences
        .accounts
        .get(provider)
        .and_then(|accounts| accounts.get(account_id))
        .copied()
        .unwrap_or(false))
}

fn status_for_account(
    provider: &str,
    account_id: &str,
    enabled: Option<bool>,
) -> Result<PromptCacheStatus, String> {
    if !valid_id(&account_id, 128) {
        return Err("Invalid account ID".into());
    }
    let mut status = match provider {
        "claude" => {
            let manager = crate::claude::storage::Manager::current().map_err(|e| e.to_string())?;
            manager.account(&account_id).map_err(|e| e.to_string())?;
            let profile = manager.profile(&account_id).map_err(|e| e.to_string())?;
            read_status(&profile, chrono::Utc::now().timestamp())
        }
        "codex" => {
            if crate::auth::get_account(&account_id)
                .map_err(|e| e.to_string())?
                .is_none()
            {
                return Err("Codex account not found".into());
            }
            unknown(NATIVE)
        }
        _ => return Err("Unsupported provider".into()),
    };
    let root = crate::auth::get_config_dir().map_err(|e| e.to_string())?;
    let mut installation_pending = false;
    if provider == "claude" {
        if let Some(enabled) = enabled {
            let _guard = lock_file(&root.join("prompt-cache-integration.lock"))
                .map_err(|_| "캐시 자동 연결 설정을 잠그지 못했습니다.".to_string())?;
            let manager = crate::claude::storage::Manager::current().map_err(|e| e.to_string())?;
            installation_pending = configure_claude_preference(&manager, account_id, enabled)
                .map_err(|_| "캐시 자동 유지 설정을 저장하지 못했습니다.".to_string())?;
        }
    }
    status.auto_keepalive_enabled = preference(
        &root,
        provider,
        account_id,
        if provider == "claude" { None } else { enabled },
    )
    .map_err(|_| "캐시 자동 유지 설정을 읽거나 저장하지 못했습니다.".to_string())?;
    if provider == "claude" {
        let manager = crate::claude::storage::Manager::current().map_err(|e| e.to_string())?;
        decorate_claude_status(&manager, account_id, &mut status);
        if installation_pending && status.auto_keepalive_enabled {
            unavailable_claude_runtime(&mut status);
        }
    } else {
        crate::prompt_cache_codex::status(account_id, &mut status)
            .map_err(|_| "캐시 유지 상태를 확인하지 못했습니다.".to_string())?;
    }
    // Claude's card understands the lifecycle gates. Do not collapse a failed
    // Claude response into a failed maintenance request or an expired/ended
    // session into a generic connection wait.
    status.auto_keepalive_state = display_state(provider, &status.auto_keepalive_state).into();
    Ok(status)
}

fn configure_claude_preference(
    manager: &crate::claude::storage::Manager,
    account_id: &str,
    enabled: bool,
) -> Result<bool> {
    manager.account(account_id)?;
    // Persist user intent independently of authentication/session availability.
    // Disabling immediately closes the request gate; enabling alone never
    // authorizes a request. Background reconciliation retries owned hooks.
    preference(
        manager
            .root
            .parent()
            .ok_or_else(|| anyhow::anyhow!("Missing settings root"))?,
        "claude",
        account_id,
        Some(enabled),
    )?;
    Ok(crate::prompt_cache_keepalive::install_for_account(manager, account_id, enabled).is_err())
}

fn unavailable_claude_runtime(status: &mut PromptCacheStatus) {
    status.auto_keepalive_supported = false;
    status.auto_keepalive_connected_sessions = 0;
    status.auto_keepalive_registered_sessions = 0;
    status.auto_keepalive_busy_sessions = 0;
    status.auto_keepalive_next_at = None;
    status.auto_keepalive_state = if status.auto_keepalive_enabled {
        "runtime_unavailable"
    } else {
        "disabled"
    }
    .into();
    status.auto_keepalive_reason =
        "설정은 저장되어 있습니다. 계정 인증 또는 세션 연결 기록을 확인하지 못해 자동 유지 상태는 확인할 수 없습니다.".into();
}

fn decorate_claude_status(
    manager: &crate::claude::storage::Manager,
    account_id: &str,
    status: &mut PromptCacheStatus,
) {
    // Runtime evidence is optional for viewing/toggling a saved preference.
    // A stale ledger, missing lease or an identity refusal must not erase the
    // account's checkbox or its historical cache observation. Emission keeps
    // its independent strict identity/quota/lease gates.
    let mut decorated = status.clone();
    if crate::prompt_cache_keepalive::decorate(manager, account_id, &mut decorated).is_ok() {
        *status = decorated;
    } else {
        unavailable_claude_runtime(status);
    }
}

/// Independent of the selected UI tab. Existing opt-ins are reattached after
/// a Switcher restart or a matching live-account change; this never starts a CLI.
pub(crate) fn start_live_session_detection() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| loop {
        let _ = refresh_live_integrations();
        std::thread::sleep(std::time::Duration::from_secs(30));
    });
}

fn refresh_live_integrations() -> Result<()> {
    let root = crate::auth::get_config_dir()?;
    let _guard = lock_file(&root.join("prompt-cache-integration.lock"))?;
    let preferences = read_preferences(&root)?;
    let Some(accounts) = preferences.accounts.get("claude") else {
        return Ok(());
    };
    let manager = crate::claude::storage::Manager::current()?;
    let store = manager.store()?;
    for account in store.accounts {
        if accounts.get(&account.id) == Some(&true) {
            // A broken profile must not prevent another account's attachment.
            let _ = crate::prompt_cache_keepalive::install_for_account(&manager, &account.id, true);
        }
    }
    Ok(())
}

fn display_state<'a>(provider: &str, state: &'a str) -> &'a str {
    if provider == "claude" {
        return state;
    }
    match state {
        "disabled" => "off",
        "monitor_unavailable"
        | "observation_required"
        | "human_input_required"
        | "ended"
        | "needs_managed_session"
        | "waiting_for_request"
        | "stopped" => "waiting_session",
        "ready" => "scheduled",
        "awaiting_response" | "verifying_prewarm" | "warming" => "pending",
        "failed" | "cancelled" | "quota_unavailable" | "authentication_required" => "error",
        "human_idle_limit" | "daily_budget" => "idle",
        other => other,
    }
}

/// Opens a new managed session or explicitly resumes selected saved history.
/// Resume validates ownership in a detached single-writer helper.
#[tauri::command]
pub async fn open_prompt_cache_session(
    provider: String,
    account_id: String,
    session_id: Option<String>,
) -> Result<Value, String> {
    if !valid_id(&account_id, 128) {
        return Err("Invalid account ID".into());
    }
    let root = crate::auth::get_config_dir().map_err(|e| e.to_string())?;
    if !preference(&root, &provider, &account_id, None).map_err(|e| e.to_string())? {
        return Err("먼저 이 계정의 캐시 자동 유지 옵션을 켜 주세요.".into());
    }
    if let Some(session) = session_id {
        return crate::prompt_cache_resume::launch(&provider, &account_id, &session)
            .await
            .map_err(|e| e.to_string());
    }
    match provider.as_str() {
        "codex" => crate::prompt_cache_codex::open(&account_id)
            .await
            .map_err(|e| e.to_string()),
        "claude" => {
            let manager = crate::claude::storage::Manager::current().map_err(|e| e.to_string())?;
            crate::claude::open_cache_session(&manager, &account_id)
                .await
                .map_err(|e| e.to_string())?;
            Ok(serde_json::json!({"started":true}))
        }
        _ => Err("Unsupported provider".into()),
    }
}

/// Helper processes run before Tauri/single-instance initialization. Their
/// stdout belongs to the Claude monitor or the private native proxy handshake.
pub fn handle_helper(arguments: &[String]) -> bool {
    let Some(flag) = arguments.get(1) else {
        return false;
    };
    if flag == "--prompt-cache-live-timer" {
        let result = arguments
            .get(2)
            .zip(arguments.get(3))
            .map(|(id, dir)| crate::prompt_cache_keepalive::live_timer_entry(id, dir));
        if matches!(result, Some(Ok(true))) {
            // Official asyncRewake uses exit 2 as a wake signal. Errors and
            // malformed arguments must never take this branch.
            eprintln!("{}", crate::prompt_cache_keepalive::NOTIFICATION);
            std::process::exit(2);
        }
        if result.is_none() || result.is_some_and(|r| r.is_err()) {
            eprintln!("Prompt-cache live timer stopped; see the account cache status.");
        }
        return true;
    }
    let result = match flag.as_str() {
        "--prompt-cache-live-hook" => arguments
            .get(2)
            .zip(arguments.get(3))
            .map(|(id, dir)| crate::prompt_cache_keepalive::live_hook_entry(id, dir)),
        "--prompt-cache-hook" => arguments
            .get(2)
            .zip(arguments.get(3))
            .map(|(id, dir)| crate::prompt_cache_keepalive::hook_entry(id, dir)),
        "--prompt-cache-monitor" => arguments
            .get(2)
            .zip(arguments.get(3))
            .zip(arguments.get(4))
            .map(|((id, dir), session)| {
                crate::prompt_cache_keepalive::monitor_entry(id, dir, session)
            }),
        "--prompt-cache-codex-host" => arguments
            .get(2)
            .map(|id| crate::prompt_cache_codex::host_entry(id)),
        "--prompt-cache-resume" => arguments
            .get(2)
            .zip(arguments.get(3))
            .zip(arguments.get(4))
            .map(|((provider, account), session)| {
                crate::prompt_cache_resume::helper_entry(provider, account, session)
            }),
        _ => return false,
    };
    if result.is_none() || result.is_some_and(|r| r.is_err()) {
        // Never print raw errors, bodies or tokens from a helper transport.
        eprintln!("Prompt-cache helper stopped; see the account cache status.");
    }
    true
}

#[tauri::command]
pub async fn get_prompt_cache_status(
    provider: String,
    account_id: String,
) -> Result<PromptCacheStatus, String> {
    tokio::task::spawn_blocking(move || status_for_account(&provider, &account_id, None))
        .await
        .map_err(|_| "Prompt-cache status worker failed".to_string())?
}

#[tauri::command]
pub async fn set_prompt_cache_auto_keepalive(
    provider: String,
    account_id: String,
    enabled: bool,
) -> Result<PromptCacheStatus, String> {
    tokio::task::spawn_blocking(move || status_for_account(&provider, &account_id, Some(enabled)))
        .await
        .map_err(|_| "Prompt-cache preference worker failed".to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    const NOW: i64 = 1_800_000_000;
    fn payload() -> Value {
        json!({"session_id":"fixture-session", "model":{"id":"claude-opus-fixture"},
            "context_window":{"current_usage":{"cache_read_input_tokens":120,
            "cache_creation_input_tokens":30,"input_tokens":7}}})
    }
    fn temp() -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("switcher-prompt-cache-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn claude_card_keeps_response_failure_and_other_gate_states_distinct() {
        for state in [
            "failed",
            "ended",
            "cancelled",
            "quota_unavailable",
            "human_idle_limit",
            "monitor_unavailable",
            "waiting_for_stop",
            "timer_waiting",
            "daily_budget",
            "busy",
            "scheduled",
            "awaiting_response",
        ] {
            assert_eq!(display_state("claude", state), state);
        }
        assert_eq!(display_state("codex", "failed"), "error");
        assert_eq!(
            display_state("codex", "monitor_unavailable"),
            "waiting_session"
        );
    }

    #[test]
    fn preference_toggle_persists_while_account_authentication_is_unavailable() {
        let root = temp();
        let manager = crate::claude::storage::Manager {
            root: root.join("claude"),
            live_dir: root.join("live"),
            live_config: root.join("live/.claude.json"),
        };
        let id = "e4a0bd94-6c81-49da-9167-82666e94513d";
        fs::create_dir_all(&manager.root).unwrap();
        let store = crate::claude::ClaudeStore {
            accounts: vec![crate::claude::ClaudeAccount {
                id: id.into(),
                name: "offline".into(),
                email: None,
                account_uuid: Some("a860f661-3385-494c-9fe2-d6185816506d".into()),
                plan_type: None,
                created_at: "offline".into(),
                last_used_at: None,
            }],
            ..Default::default()
        };
        fs::write(
            manager.root.join("accounts.json"),
            serde_json::to_vec(&store).unwrap(),
        )
        .unwrap();
        for option in [true, false, true] {
            assert!(configure_claude_preference(&manager, id, option).unwrap());
            assert_eq!(preference(&root, "claude", id, None).unwrap(), option);
        }
        assert!(configure_claude_preference(
            &manager,
            "a860f661-3385-494c-9fe2-d6185816506d",
            true
        )
        .is_err());
        assert!(!manager.live_dir.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn runtime_identity_failure_does_not_hide_account_cache_option_or_observation() {
        let root = temp();
        let manager = crate::claude::storage::Manager {
            root: root.join("claude"),
            live_dir: root.join("live"),
            live_config: root.join("live/.claude.json"),
        };
        let id = "e4a0bd94-6c81-49da-9167-82666e94513d";
        // Missing profile identity is deliberately not repaired by a status
        // read. Persisted preference remains usable and no adapter is claimed.
        for option in [false, true, false] {
            preference(&root, "claude", id, Some(option)).unwrap();
            let mut status = unknown("historical cache");
            status.auto_keepalive_enabled = preference(&root, "claude", id, None).unwrap();
            status.observed_at = Some(NOW);
            status.cache_read_tokens = Some(1200);
            status.expires_at = Some(NOW + 300);
            decorate_claude_status(&manager, id, &mut status);
            assert_eq!(status.auto_keepalive_enabled, option);
            assert_eq!(
                status.auto_keepalive_state,
                if option {
                    "runtime_unavailable"
                } else {
                    "disabled"
                }
            );
            assert!(!status.auto_keepalive_supported);
            assert_eq!(status.auto_keepalive_connected_sessions, 0);
            assert_eq!(status.auto_keepalive_next_at, None);
            assert_eq!(status.observed_at, Some(NOW));
            assert_eq!(status.cache_read_tokens, Some(1200));
            assert_eq!(status.expires_at, Some(NOW + 300));
        }
        assert!(!manager.root.exists());
        assert!(!manager.live_dir.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn helper_versions_are_immutable_and_tampered_primary_is_never_launched() {
        let dir = temp();
        let source = dir.join("synthetic-switcher.exe");
        let runtime = dir.join("runtime");
        fs::write(&source, b"version-one").unwrap();
        let one = helper_from(&source, &runtime).unwrap();
        assert_ne!(one, source);
        fs::write(&source, b"version-two").unwrap();
        let two = helper_from(&source, &runtime).unwrap();
        assert_ne!(one, two);
        assert_eq!(fs::read(&one).unwrap(), b"version-one");
        fs::write(&two, b"edited-after-copy").unwrap();
        assert!(helper_from(&source, &runtime).is_err());
        // Refusal must not silently replace a process' immutable runtime.
        assert_eq!(fs::read(&two).unwrap(), b"edited-after-copy");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn read_write_metrics_do_not_invent_ttl_or_persist_payload() {
        let dir = temp();
        let mut data = payload();
        data["prompt"] = json!("DO_NOT_PERSIST");
        data["transcript_path"] = json!("DO_NOT_PERSIST");
        data["credentials"] = json!({"token":"DO_NOT_PERSIST"});
        record_at(&dir, &data, NOW).unwrap();
        let status = read_status(&dir, NOW);
        assert_eq!(status.state, "observed");
        assert_eq!(
            (
                status.cache_read_tokens,
                status.cache_write_tokens,
                status.uncached_input_tokens
            ),
            (Some(120), Some(30), Some(7))
        );
        assert_eq!(
            (status.observed_at, status.expires_at, status.ttl_seconds),
            (None, None, None)
        );
        assert!(!fs::read_to_string(dir.join(FILE))
            .unwrap()
            .contains("DO_NOT_PERSIST"));
        assert_eq!(
            serde_json::to_value(&status)
                .unwrap()
                .as_object()
                .unwrap()
                .len(),
            23
        );
        assert_eq!(status.auto_keepalive_connected_sessions, 0);
        assert_eq!(status.auto_keepalive_registered_sessions, 0);
        assert_eq!(status.auto_keepalive_busy_sessions, 0);
        assert!(dir.join("prompt-cache.lock").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn explicit_official_ttl_and_expiry_and_legitimate_expiration() {
        for (ttl, seconds) in [("5m", 300), ("1h", 3600)] {
            let mut data = payload();
            data["prompt_cache"] = json!({"ttl":ttl,"expires_at":NOW+100,"caching_observed":true});
            let status = parse_claude(&data, NOW).unwrap();
            assert_eq!(status.ttl_seconds, Some(seconds));
            assert_eq!(status.observed_at, Some(NOW + 100 - seconds));
            assert_eq!(status.expires_at, Some(NOW + 100));
            let expired = parse_claude(&data, NOW + 101).unwrap();
            assert_eq!(expired.state, "observed");
            assert!(expired.reason.unwrap().contains("expiry has passed"));
        }
        let mut data = payload();
        data["prompt_cache"] = json!({"ttl":"1h","caching_observed":true});
        let status = parse_claude(&data, NOW).unwrap();
        assert_eq!(status.ttl_seconds, Some(3600));
        assert_eq!((status.observed_at, status.expires_at), (None, None));
    }

    #[test]
    fn malformed_counts_ids_and_future_request_anchors_are_unknown() {
        let dir = temp();
        for value in [json!(-1), json!(1.5), json!("3"), json!(u64::MAX)] {
            let mut data = payload();
            data["context_window"]["current_usage"]["cache_read_input_tokens"] = value;
            record_at(&dir, &data, NOW).unwrap();
            assert_eq!(read_status(&dir, NOW).state, "unknown");
        }
        for cache in [
            json!({"ttl":"30m"}),
            json!({"ttl":300}),
            json!({"ttl":"5m","expires_at":NOW+301}),
            json!({"expires_at":i64::MAX}),
            json!({"expires_at":-1}),
            json!({"caching_observed":"true"}),
        ] {
            let mut data = payload();
            data["prompt_cache"] = cache;
            assert!(parse_claude(&data, NOW).is_err());
        }
        let mut data = payload();
        data["session_id"] = json!("x".repeat(129));
        assert!(parse_claude(&data, NOW).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn repeated_payload_never_advances_expiry_or_observation() {
        let dir = temp();
        let mut data = payload();
        data["prompt_cache"] = json!({"ttl":"5m","expires_at":NOW+200,"caching_observed":true});
        record_at(&dir, &data, NOW).unwrap();
        let before = fs::read(dir.join(FILE)).unwrap();
        record_at(&dir, &data, NOW + 50).unwrap();
        assert_eq!(fs::read(dir.join(FILE)).unwrap(), before);
        record_at(&dir, &data, NOW + 201).unwrap();
        let status = read_status(&dir, NOW + 202);
        assert_eq!(
            (status.observed_at, status.expires_at),
            (Some(NOW - 100), Some(NOW + 200))
        );
        assert!(status.reason.unwrap().contains("expiry has passed"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn transcript_timing_retains_estimate_provenance_without_poll_extension() {
        let dir = temp();
        let mut data = payload();
        data["prompt_cache"] = json!({"ttl":"5m","expires_at":NOW+120,
            "caching_observed":true,"source":TRANSCRIPT_SOURCE});
        record_at(&dir, &data, NOW).unwrap();
        let first = read_status(&dir, NOW);
        record_at(&dir, &data, NOW + 60).unwrap();
        let later = read_status(&dir, NOW + 60);
        assert_eq!(first.observed_at, later.observed_at);
        assert_eq!(later.ttl_source.as_deref(), Some(TRANSCRIPT_SOURCE));
        assert!(later.reason.unwrap().contains("estimated"));
        // The official source does not inherit the fallback label.
        data["prompt_cache"]
            .as_object_mut()
            .unwrap()
            .remove("source");
        assert_eq!(
            parse_claude(&data, NOW).unwrap().ttl_source.as_deref(),
            Some(SOURCE)
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn quota_totals_and_unscoped_payloads_never_create_cache() {
        for data in [
            json!({"session_id":"fixture", "model":{"id":"opus"},
            "rate_limits":{"five_hour":{"used_percentage":10,"resets_at":NOW+300}},
            "context_window":{"total_input_tokens":2000}}),
            json!({"usage":{"cache_read_input_tokens":2000},"creator_account_id":"ambiguous"}),
            json!({"session_id":"fixture","prompt_cache":{"ttl":"1h","expires_at":NOW+100,"caching_observed":true}}),
        ] {
            let status = parse_claude(&data, NOW).unwrap();
            assert_eq!(status.state, "unknown");
            assert_eq!(
                (status.observed_at, status.expires_at, status.ttl_seconds),
                (None, None, None)
            );
        }
        assert_eq!(unknown(NATIVE).state, "unknown");
    }

    #[test]
    fn fresh_file_read_replaces_session_model_and_rejects_malformed_primary() {
        let dir = temp();
        assert_eq!(read_status(&dir, NOW).state, "unknown");
        record_at(&dir, &payload(), NOW).unwrap();
        let mut other = payload();
        other["session_id"] = json!("another-session");
        other["model"]["id"] = json!("claude-haiku-fixture");
        record_at(&dir, &other, NOW).unwrap();
        let status = read_status(&dir, NOW);
        assert_eq!(status.session_id.as_deref(), Some("another-session"));
        assert_eq!(status.model.as_deref(), Some("claude-haiku-fixture"));
        assert!(status
            .reason
            .unwrap()
            .contains("scoped to this session and model"));
        for bytes in [
            b"{bad".as_slice(),
            br#"{"state":"observed","expires_at":1e999}"#.as_slice(),
        ] {
            fs::write(dir.join(FILE), bytes).unwrap();
            assert_eq!(read_status(&dir, NOW).state, "unknown");
        }
        fs::write(dir.join(FILE), vec![b' '; MAX_BYTES as usize + 1]).unwrap();
        assert_eq!(read_status(&dir, NOW).state, "unknown");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn keepalive_preferences_are_provider_and_account_scoped_and_default_off() {
        let dir = temp();
        assert!(!preference(&dir, "codex", "same-id", None).unwrap());
        assert!(preference(&dir, "claude", "same-id", Some(true)).unwrap());
        assert!(!preference(&dir, "codex", "same-id", None).unwrap());
        assert!(!preference(&dir, "claude", "neighbor", None).unwrap());
        assert!(preference(&dir, "claude", "neighbor", Some(true)).unwrap());
        assert!(!preference(&dir, "claude", "same-id", Some(false)).unwrap());
        assert!(preference(&dir, "claude", "neighbor", None).unwrap());
        record_at(&dir, &payload(), NOW).unwrap();
        assert!(preference(&dir, "claude", "neighbor", None).unwrap());
        let status = read_status(&dir, NOW);
        assert!(!status.auto_keepalive_supported);
        assert!(!status.auto_keepalive_enabled);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn malformed_preferences_never_restore_backup_or_overwrite_primary() {
        let dir = temp();
        preference(&dir, "claude", "fixture", Some(true)).unwrap();
        fs::copy(
            dir.join(PREFERENCES),
            dir.join(format!("{PREFERENCES}.bak")),
        )
        .unwrap();
        for bytes in [
            b"{bad".to_vec(),
            vec![b' '; MAX_PREFERENCES_BYTES as usize + 1],
            br#"{"version":2,"accounts":{}}"#.to_vec(),
        ] {
            fs::write(dir.join(PREFERENCES), &bytes).unwrap();
            assert!(preference(&dir, "claude", "fixture", None).is_err());
            assert!(preference(&dir, "claude", "fixture", Some(false)).is_err());
            assert_eq!(fs::read(dir.join(PREFERENCES)).unwrap(), bytes);
        }
        assert!(preference(&dir, "unsupported", "fixture", Some(true)).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
