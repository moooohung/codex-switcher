//! Subscription quota polling; this module never runs Claude or switches accounts.
//!
//! `quota-poll.json` owns HTTP scheduling, independently of statusline timestamps.
//! Successes wait 180s; force only bypasses that interval after a 10s serve floor.
//! Errors (including 429) retain measured quota and its original observation time.
//! Force never bypasses error backoff or an outstanding request reservation.
//! Other tools such as cswap do not share this scheduling file or request budget.
//!
//! Ordinary GETs hold no credential/file locks. Rotating refresh POSTs hold the
//! CLI-compatible CredentialsGuard (live first, then profile); accounts/usage
//! file locks and runtime::OPERATION are never held across HTTP. Do not call
//! sync_active under that guard: it acquires the same live guard itself.
use super::{
    locks::CredentialsGuard,
    monitor, runtime,
    storage::{write_json, Manager},
    ClaudeAccount, UsageReceipt,
};
use crate::{
    auth::persistence::{lock_file, read_json},
    types::UsageInfo,
};
use anyhow::{ensure, Context, Result};
use chrono::{DateTime, Utc};
use reqwest::{
    header::{HeaderMap, RETRY_AFTER},
    Client, Response, StatusCode,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, path::Path, time::Duration};
use tokio::time::{timeout, Instant};

mod cswap;

const POLL_INTERVAL: i64 = 180;
const FORCE_FLOOR: i64 = 10;
const ATTEMPT_LEASE: i64 = 120;
const MAX_RETRY_AFTER: i64 = 86_400;
const MAX_BODY: usize = 256 * 1024;
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const BETA: &str = "oauth-2025-04-20";

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct PollState {
    account_uuid: Option<String>,
    last_attempt_at: i64,
    next_poll_at: i64,
    last_success_at: Option<i64>,
    failures: u32,
    last_error: Option<String>,
    retry_after_seconds: Option<i64>,
    attempt_id: Option<String>,
    in_flight_until: i64,
    credential_generation: Option<String>,
    rejected_credential_generation: Option<String>,
    authentication_required: bool,
}

fn credential_fingerprint(credentials: &Value) -> Result<String> {
    let oauth = oauth(credentials)?;
    let mut digest = Sha256::new();
    for field in ["accessToken", "refreshToken"] {
        let token = oauth[field]
            .as_str()
            .context("Claude OAuth token is missing")?;
        digest.update((token.len() as u64).to_be_bytes());
        digest.update(token.as_bytes());
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn authentication_required(error: &str) -> bool {
    error.contains("invalid_grant")
}

struct Http {
    client: Client,
    usage: String,
    token: String,
    profile: String,
}
impl Http {
    fn client(https_only: bool, budget: Duration) -> Result<Client> {
        let builder = Client::builder()
            .connect_timeout(Duration::from_secs(3).min(budget))
            .read_timeout(Duration::from_secs(5).min(budget))
            .timeout(budget)
            .https_only(https_only)
            .redirect(reqwest::redirect::Policy::none())
            // Replaying a rotating refresh grant is unsafe, even on a protocol error.
            .retry(reqwest::retry::never())
            .user_agent(concat!("codex-switcher/", env!("CARGO_PKG_VERSION")));
        // Production follows configured proxies. Fixture traffic must stay local.
        Ok(if https_only {
            builder
        } else {
            builder.no_proxy()
        }
        .build()?)
    }
    fn production() -> Result<Self> {
        #[cfg(test)]
        anyhow::bail!("Live OAuth HTTP is disabled in unit tests; use loopback fixtures");
        #[cfg(not(test))]
        Ok(Self {
            client: Self::client(true, Duration::from_secs(8))?,
            usage: "https://api.anthropic.com/api/oauth/usage".into(),
            token: "https://platform.claude.com/v1/oauth/token".into(),
            profile: "https://api.anthropic.com/api/oauth/profile".into(),
        })
    }
    async fn usage(&self, token: &str) -> std::result::Result<Value, Failure> {
        let response = self
            .client
            .get(&self.usage)
            .bearer_auth(token)
            .header("anthropic-beta", BETA)
            .send()
            .await
            .map_err(Failure::network)?;
        response_json(response, "usage").await
    }
    async fn rotate(&self, token: &str) -> std::result::Result<Value, Failure> {
        let response = self
            .client
            .post(&self.token)
            .json(&json!({
                "grant_type":"refresh_token", "refresh_token":token, "client_id":CLIENT_ID
            }))
            .send()
            .await
            .map_err(Failure::network)?;
        response_json(response, "refresh").await
    }
    async fn identity(&self, token: &str) -> std::result::Result<Value, Failure> {
        let response = self
            .client
            .get(&self.profile)
            .bearer_auth(token)
            .send()
            .await
            .map_err(Failure::network)?;
        response_json(response, "profile").await
    }
}

#[derive(Debug)]
struct Failure {
    message: String,
    status: Option<StatusCode>,
    retry_after: Option<i64>,
}
impl Failure {
    fn local(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            status: None,
            retry_after: None,
        }
    }
    fn network(error: reqwest::Error) -> Self {
        // Never surface URLs, response bodies, request headers or credential values.
        Self::local(if error.is_timeout() {
            "OAuth HTTP timeout"
        } else {
            "OAuth HTTP transport error"
        })
    }
}
fn retry_after(headers: &HeaderMap, now: i64) -> Option<i64> {
    let raw = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    let delay = if raw.bytes().all(|c| c.is_ascii_digit()) && !raw.is_empty() {
        raw.parse::<u64>()
            .unwrap_or(u64::MAX)
            .min(MAX_RETRY_AFTER as u64) as i64
    } else {
        DateTime::parse_from_rfc2822(raw)
            .ok()?
            .timestamp()
            .saturating_sub(now)
            .max(0)
    };
    Some(delay.min(MAX_RETRY_AFTER))
}
async fn body(mut response: Response) -> std::result::Result<Vec<u8>, Failure> {
    if response
        .content_length()
        .is_some_and(|n| n > MAX_BODY as u64)
    {
        return Err(Failure::local("OAuth HTTP response is too large"));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(Failure::network)? {
        if chunk.len() > MAX_BODY.saturating_sub(bytes.len()) {
            return Err(Failure::local("OAuth HTTP response is too large"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
async fn response_json(response: Response, endpoint: &str) -> std::result::Result<Value, Failure> {
    let status = response.status();
    if status != StatusCode::OK {
        let retry_after = retry_after(response.headers(), Utc::now().timestamp());
        let mut message = format!("OAuth {endpoint} HTTP {}", status.as_u16());
        if endpoint == "refresh" && matches!(status.as_u16(), 400 | 401 | 403) {
            if let Ok(bytes) = body(response).await {
                if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                    match value["error"].as_str() {
                        Some("invalid_grant") => {
                            message.push_str(" (invalid_grant; sign in again)")
                        }
                        Some("invalid_client") => message.push_str(" (invalid_client)"),
                        _ => {}
                    }
                }
            }
        }
        return Err(Failure {
            message,
            status: Some(status),
            retry_after,
        });
    }
    serde_json::from_slice(&body(response).await?)
        .map_err(|_| Failure::local(format!("OAuth {endpoint} returned malformed JSON")))
}

fn window(data: &Value, name: &str) -> Result<(f64, Option<i64>)> {
    // A missing/null whole window is UNKNOWN, not zero. cswap's build_usage_result
    // likewise skips null windows. An explicit utilization=0/reset=null is idle.
    let value = data
        .get(name)
        .filter(|v| v.is_object())
        .with_context(|| format!("Missing {name} quota window"))?;
    let percent = value
        .get("utilization")
        .and_then(Value::as_f64)
        .context("Missing quota utilization")?;
    ensure!(
        percent.is_finite() && (0.0..=100.0).contains(&percent),
        "Invalid quota utilization"
    );
    let reset = value.get("resets_at").context("Missing quota reset")?;
    let reset = if reset.is_null() {
        ensure!(percent == 0.0, "Nonzero quota has no reset");
        None
    } else {
        let parsed = DateTime::parse_from_rfc3339(reset.as_str().context("Invalid quota reset")?)
            .context("Invalid RFC3339 quota reset")?
            .timestamp();
        ensure!(
            parsed > Utc::now().timestamp(),
            "Quota reset is already stale"
        );
        Some(parsed)
    };
    Ok((percent, reset))
}
fn parse_usage(account: &ClaudeAccount, data: &Value) -> Result<UsageInfo> {
    let (primary, primary_reset) = window(data, "five_hour")?;
    let (secondary, secondary_reset) = window(data, "seven_day")?;
    let mut usage = UsageInfo::error(account.id.clone(), String::new());
    usage.plan_type = account.plan_type.clone();
    usage.primary_used_percent = Some(primary);
    usage.primary_window_minutes = Some(300);
    usage.primary_resets_at = primary_reset;
    usage.secondary_used_percent = Some(secondary);
    usage.secondary_window_minutes = Some(10_080);
    usage.secondary_resets_at = secondary_reset;
    usage.error = None;
    Ok(usage)
}

fn object(path: &Path) -> Result<Value> {
    let Some(bytes) = raw(path)? else {
        return Ok(json!({}));
    };
    let value: Value =
        serde_json::from_slice(&bytes).context("Invalid Claude credential/configuration JSON")?;
    ensure!(
        value.is_object(),
        "Claude credential/configuration must be an object"
    );
    Ok(value)
}
fn raw(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
        Ok(meta) => {
            ensure!(
                meta.len() <= 8 * 1024 * 1024,
                "Claude credential/configuration is too large"
            );
            Ok(Some(fs::read(path)?))
        }
    }
}
fn uuid_from_config(config: &Value) -> Option<&str> {
    config
        .pointer("/oauthAccount/accountUuid")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
}
fn guard_identity(account: &ClaudeAccount, config: &Value) -> Result<()> {
    if let Some(uuid) = &account.account_uuid {
        ensure!(
            uuid_from_config(config) == Some(uuid.as_str()),
            "Claude profile account UUID changed"
        );
    }
    Ok(())
}
fn oauth(value: &Value) -> Result<&Value> {
    super::storage::validate_credentials(value)?;
    Ok(&value["claudeAiOauth"])
}
fn access(value: &Value) -> Result<&str> {
    oauth(value)?["accessToken"]
        .as_str()
        .context("Missing Claude access token")
}

#[derive(Clone, Serialize, Deserialize)]
struct LiveSnapshot {
    credentials: Vec<u8>,
    config: Vec<u8>,
}
struct Snapshot {
    account: ClaudeAccount,
    saved_oauth: Value,
    credentials: Value,
    live: Option<LiveSnapshot>,
    receipt: Option<UsageReceipt>,
}
fn active_target(m: &Manager, store: &super::ClaudeStore, target: &ClaudeAccount) -> bool {
    if let Ok(active) = m.active_id(store) {
        return active.as_deref() == Some(target.id.as_str());
    }
    // An unreadable unrelated profile must not abort every account's poll or
    // hide a healthy active CLI rotation. Ignore only those unrelated reads.
    let Ok(config) = object(&m.live_config) else {
        return false;
    };
    let Ok(live) = object(&m.live_dir.join(".credentials.json")) else {
        return false;
    };
    let Ok(token) = access(&live) else {
        return false;
    };
    if guard_identity(target, &config).is_err() {
        return false;
    }
    let owners = store
        .accounts
        .iter()
        .filter_map(|account| {
            let saved = object(&m.profile(&account.id).ok()?.join(".credentials.json")).ok()?;
            (access(&saved).ok() == Some(token)).then_some(account.id.as_str())
        })
        .collect::<Vec<_>>();
    if !owners.is_empty() {
        return owners.len() == 1 && owners[0] == target.id;
    }
    target.account_uuid.as_deref().is_some_and(|uuid| {
        uuid_from_config(&config) == Some(uuid)
            && store
                .accounts
                .iter()
                .filter(|a| a.account_uuid.as_deref() == Some(uuid))
                .count()
                == 1
    })
}
fn snapshot(m: &Manager, id: &str) -> Result<Snapshot> {
    m.transaction(|store| {
        let account = store
            .accounts
            .iter()
            .find(|a| a.id == id)
            .cloned()
            .context("Claude account not found")?;
        let p = m.profile(id)?;
        guard_identity(&account, &object(&p.join(".claude.json"))?)?;
        let saved = object(&p.join(".credentials.json"))?;
        let saved_oauth = oauth(&saved)?.clone();
        let mut credentials = saved;
        let mut live = None;
        // active_id also rejects credentials known to belong to a different saved
        // account. UUID config alone is deliberately not sufficient here.
        if !m.has_relogin_barrier(id, account.account_uuid.as_deref())?
            && active_target(m, store, &account)
        {
            if let (Some(bytes), Some(config_bytes)) = (
                raw(&m.live_dir.join(".credentials.json"))?,
                raw(&m.live_config)?,
            ) {
                let live_credentials: Value = serde_json::from_slice(&bytes)?;
                let live_config: Value = serde_json::from_slice(&config_bytes)?;
                guard_identity(&account, &live_config)?;
                let live_oauth = oauth(&live_credentials)?;
                if live_oauth["expiresAt"].as_i64() > saved_oauth["expiresAt"].as_i64() {
                    credentials["claudeAiOauth"] = live_oauth.clone();
                }
                if live_oauth == &credentials["claudeAiOauth"]
                    && raw(&m.live_dir.join(".credentials.json"))?.as_deref()
                        == Some(bytes.as_slice())
                    && raw(&m.live_config)?.as_deref() == Some(config_bytes.as_slice())
                {
                    live = Some(LiveSnapshot {
                        credentials: bytes,
                        config: config_bytes,
                    });
                }
            }
        }
        Ok(Snapshot {
            account,
            saved_oauth,
            credentials,
            live,
            receipt: read_json(&p.join("usage.json"))?,
        })
    })
}

#[derive(Serialize, Deserialize)]
struct PendingRotation {
    account_uuid: Option<String>,
    previous_oauth: Value,
    credentials: Value,
    live: Option<LiveSnapshot>,
}
fn rotated(snapshot: &Snapshot, response: &Value) -> Result<Value> {
    ensure!(response.is_object(), "Malformed OAuth refresh response");
    let token = response["access_token"]
        .as_str()
        .filter(|s| !s.trim().is_empty() && s.len() <= 65_536)
        .context("Missing refreshed access token")?;
    let seconds = response["expires_in"]
        .as_i64()
        .filter(|n| (1..=31_622_400).contains(n))
        .context("Invalid refreshed expiry")?;
    let mut result = snapshot.credentials.clone();
    result["claudeAiOauth"]["accessToken"] = json!(token);
    result["claudeAiOauth"]["expiresAt"] = json!(Utc::now()
        .timestamp_millis()
        .checked_add(seconds * 1000)
        .context("Refreshed expiry overflow")?);
    if let Some(refresh) = response.get("refresh_token") {
        let refresh = refresh
            .as_str()
            .filter(|s| !s.trim().is_empty() && s.len() <= 65_536)
            .context("Invalid refreshed refresh token")?;
        result["claudeAiOauth"]["refreshToken"] = json!(refresh);
    }
    if let Some(scope) = response.get("scope") {
        let scope = scope.as_str().context("Invalid refreshed scope")?;
        result["claudeAiOauth"]["scopes"] = json!(scope.split_whitespace().collect::<Vec<_>>());
    }
    oauth(&result)?;
    Ok(result)
}
fn response_uuid(value: &Value) -> Result<Option<&str>> {
    let Some(account) = value.get("account") else {
        return Ok(None);
    };
    let uuid = account
        .get("uuid")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .context("Malformed OAuth account UUID")?;
    Ok(Some(uuid))
}
fn persist_rotation(m: &Manager, snapshot: &mut Snapshot, credentials: &Value) -> Result<()> {
    m.transaction(|store| {
        let current = store
            .accounts
            .iter()
            .find(|a| a.id == snapshot.account.id)
            .context("Claude account was removed during refresh")?;
        ensure!(
            current.account_uuid == snapshot.account.account_uuid,
            "Claude account UUID changed during refresh"
        );
        let p = m.profile(&current.id)?;
        let _lock = lock_file(&p.join("credentials.lock"))?;
        guard_identity(current, &object(&p.join(".claude.json"))?)?;
        let mut saved = object(&p.join(".credentials.json"))?;
        ensure!(
            oauth(&saved)? == &snapshot.saved_oauth,
            "Claude credentials changed during refresh; refusing stale token rotation"
        );
        saved["claudeAiOauth"] = credentials["claudeAiOauth"].clone();
        write_json(&p.join(".credentials.json"), &saved)?;
        ensure!(
            oauth(&object(&p.join(".credentials.json"))?)? == &credentials["claudeAiOauth"],
            "Claude profile credential read-back failed"
        );
        // Guards prevent cooperative CLI/activation writes. Compare both complete
        // live files again so an unrelated/non-cooperative writer is never undone.
        if let Some(live) = &snapshot.live {
            if active_target(m, store, current)
                && raw(&m.live_dir.join(".credentials.json"))?.as_deref()
                    == Some(live.credentials.as_slice())
                && raw(&m.live_config)?.as_deref() == Some(live.config.as_slice())
            {
                let mut value: Value = serde_json::from_slice(&live.credentials)?;
                value["claudeAiOauth"] = credentials["claudeAiOauth"].clone();
                write_json(&m.live_dir.join(".credentials.json"), &value)?;
                ensure!(
                    oauth(&object(&m.live_dir.join(".credentials.json"))?)?
                        == &credentials["claudeAiOauth"],
                    "Claude live credential read-back failed"
                );
            }
        }
        Ok(())
    })?;
    snapshot.saved_oauth = credentials["claudeAiOauth"].clone();
    snapshot.credentials = credentials.clone();
    Ok(())
}
async fn refresh_token(
    m: &Manager,
    old: &Snapshot,
    http: &Http,
) -> std::result::Result<Snapshot, Failure> {
    let operation = timeout(Duration::from_secs(15), runtime::OPERATION.lock())
        .await
        .map_err(|_| Failure::local("Claude activation is busy; refresh deferred"))?;
    let live = m.live_dir.clone();
    let profile = m
        .profile(&old.account.id)
        .map_err(|e| Failure::local(e.to_string()))?;
    // Always live FIRST, including inactive profiles: this also covers activation
    // between the initial snapshot and the refresh grant. Acquisition is blocking.
    let guards = tokio::task::spawn_blocking(move || -> Result<_> {
        let live_guard = CredentialsGuard::acquire(&live)?;
        let profile_guard = CredentialsGuard::acquire(&profile)?;
        Ok((live_guard, profile_guard))
    })
    .await
    .map_err(|_| Failure::local("Claude credential guard task failed"))?
    .map_err(|e| Failure::local(e.to_string()))?;
    let mut current = snapshot(m, &old.account.id).map_err(|e| Failure::local(e.to_string()))?;
    drop(operation);
    if current
        .account
        .account_uuid
        .as_ref()
        .is_none_or(|uuid| uuid.trim().is_empty())
    {
        return Err(Failure::local(
            "Claude account UUID is unknown; re-import the subscription login before refreshing",
        ));
    }
    let pending_path = m
        .profile(&old.account.id)
        .map_err(|e| Failure::local(e.to_string()))?
        .join("quota-token-pending.json");
    let pending: Option<PendingRotation> =
        read_json(&pending_path).map_err(|e| Failure::local(e.to_string()))?;
    let candidate = if let Some(pending) = pending {
        if pending.account_uuid != current.account.account_uuid {
            return Err(Failure::local(
                "Pending OAuth rotation belongs to an older credential generation",
            ));
        }
        if pending.previous_oauth != current.saved_oauth
            && pending.credentials["claudeAiOauth"] != current.saved_oauth
        {
            // A CLI or a partially completed commit has already installed a
            // successor. Never replay the predecessor, or overwrite that winner.
            if current.credentials["claudeAiOauth"]["expiresAt"]
                .as_i64()
                .is_some_and(|n| n > Utc::now().timestamp_millis() + 300_000)
            {
                fs::remove_file(&pending_path).map_err(|_| {
                    Failure::local("Could not clear superseded pending OAuth rotation")
                })?;
                drop(guards);
                return Ok(current);
            }
            return Err(Failure::local(
                "Pending OAuth rotation belongs to an older credential generation",
            ));
        }
        // Retain the ORIGINAL before/after live CAS across identity-query failure.
        // persist_rotation will skip live if either complete file has changed.
        current.live = pending.live;
        pending.credentials
    } else {
        // A real CLI may have won the refresh while this caller acquired guards.
        if access(&current.credentials).ok() != access(&old.credentials).ok()
            && current.credentials["claudeAiOauth"]["expiresAt"]
                .as_i64()
                .is_some_and(|n| n > Utc::now().timestamp_millis() + 300_000)
        {
            drop(guards);
            return Ok(current);
        }
        let refresh = oauth(&current.credentials).map_err(|e| Failure::local(e.to_string()))?
            ["refreshToken"]
            .as_str()
            .ok_or_else(|| Failure::local("Claude refresh token is missing"))?;
        let response = http.rotate(refresh).await?;
        let candidate = rotated(&current, &response).map_err(|e| Failure::local(e.to_string()))?;
        // Keep the successor durably before any identity request. If that request
        // times out, a retry verifies this successor rather than spending its
        // already-consumed predecessor again. Never install an unverified token.
        write_json(
            &pending_path,
            &PendingRotation {
                account_uuid: current.account.account_uuid.clone(),
                previous_oauth: current.saved_oauth.clone(),
                credentials: candidate.clone(),
                live: current.live.clone(),
            },
        )
        .map_err(|_| {
            Failure::local(
                "Could not preserve rotated OAuth successor; sign in again before retrying",
            )
        })?;
        if let Some(uuid) = response_uuid(&response).map_err(|e| Failure::local(e.to_string()))? {
            if current.account.account_uuid.as_deref() != Some(uuid) {
                return Err(Failure::local(
                    "Refreshed token account UUID does not match the saved account",
                ));
            }
            persist_rotation(m, &mut current, &candidate)
                .map_err(|e| Failure::local(e.to_string()))?;
            fs::remove_file(&pending_path)
                .map_err(|_| Failure::local("Could not clear pending OAuth rotation"))?;
            drop(guards);
            return Ok(current);
        }
        candidate
    };
    let identity = http
        .identity(access(&candidate).map_err(|e| Failure::local(e.to_string()))?)
        .await?;
    let uuid = response_uuid(&identity)
        .map_err(|e| Failure::local(e.to_string()))?
        .ok_or_else(|| Failure::local("OAuth profile returned no account UUID"))?;
    if current.account.account_uuid.as_deref() != Some(uuid) {
        return Err(Failure::local(
            "Refreshed token account UUID does not match the saved account",
        ));
    }
    persist_rotation(m, &mut current, &candidate).map_err(|e| Failure::local(e.to_string()))?;
    fs::remove_file(&pending_path)
        .map_err(|_| Failure::local("Could not clear pending OAuth rotation"))?;
    drop(guards);
    Ok(current)
}

fn reserve(m: &Manager, account: &ClaudeAccount, force: bool) -> Result<(PollState, bool)> {
    // Read identity/credentials before poll.lock, preserving accounts -> poll
    // lock order. A rejected grant stays paused until a real token generation
    // changes, including a newer cswap backup; elapsed time cannot repair it.
    let current = snapshot(m, &account.id)?;
    let external = cswap::candidate(m, &current)?;
    let current_fingerprint = credential_fingerprint(&current.credentials)?;
    reserve_generation(
        m,
        account,
        force,
        &current_fingerprint,
        external.as_ref().map(|c| c.fingerprint.as_str()),
    )
}
fn reserve_generation(
    m: &Manager,
    account: &ClaudeAccount,
    force: bool,
    current_fingerprint: &str,
    external_fingerprint: Option<&str>,
) -> Result<(PollState, bool)> {
    let p = m.profile(&account.id)?;
    let generation = format!(
        "{current_fingerprint}:{}",
        external_fingerprint.unwrap_or("")
    );
    let _lock = lock_file(&p.join("quota-poll.lock"))?;
    let mut state: PollState = read_json(&p.join("quota-poll.json"))?.unwrap_or_default();
    if state.account_uuid != account.account_uuid {
        state = PollState::default();
    }
    state.account_uuid = account.account_uuid.clone();
    let authentication_paused = state.authentication_required
        || state
            .last_error
            .as_deref()
            .is_some_and(authentication_required);
    // Candidate appearance/disappearance is scheduling information, not a new
    // credential. Remember the actual rejected pair even if it was adopted
    // during this attempt, after the reservation fingerprint was captured.
    let rejected = state
        .rejected_credential_generation
        .as_deref()
        .unwrap_or(current_fingerprint);
    // The external candidate is adopted before any usage GET/refresh. A changed
    // profile cannot justify replaying that same rejected candidate.
    let authentication_changed = external_fingerprint.unwrap_or(current_fingerprint) != rejected;
    if authentication_paused && state.rejected_credential_generation.is_none() {
        // Legacy reservation generations predate adoption and do not identify
        // the pair actually rejected. Do not guess from that scheduling key.
        state.rejected_credential_generation = Some(current_fingerprint.to_owned());
    }
    let now = Utc::now().timestamp();
    // Don't release another process's in-flight reservation. The rejected pair,
    // not the pre-adoption scheduling key, determines whether auth has changed.
    if now >= state.in_flight_until {
        if authentication_paused && authentication_changed {
            state.last_attempt_at = 0;
            state.next_poll_at = 0;
            state.failures = 0;
            state.last_error = None;
            state.retry_after_seconds = None;
            state.authentication_required = false;
            state.rejected_credential_generation = None;
        }
        state.credential_generation = Some(generation);
    }
    // Migrate prior poll files which stored invalid_grant as timed backoff.
    state.authentication_required |= state
        .last_error
        .as_deref()
        .is_some_and(authentication_required);
    let blocked = state.authentication_required
        || now < state.in_flight_until
        || now < state.last_attempt_at.saturating_add(FORCE_FLOOR)
        || (now < state.next_poll_at && (!force || state.failures > 0));
    if blocked {
        write_json(&p.join("quota-poll.json"), &state)?;
        return Ok((state, false));
    }
    state.last_attempt_at = now;
    state.in_flight_until = now.saturating_add(ATTEMPT_LEASE);
    state.attempt_id = Some(uuid::Uuid::new_v4().to_string());
    write_json(&p.join("quota-poll.json"), &state)?;
    Ok((state, true))
}
fn cached(m: &Manager, account: &ClaudeAccount, state: &PollState) -> Result<UsageInfo> {
    let receipt = m.receipt(&account.id)?;
    if let Some(r) = &receipt {
        ensure!(
            r.account_id == account.id && r.usage.account_id == account.id,
            "Claude receipt belongs to another account"
        );
    }
    let mut usage = receipt.map(|r| r.usage).unwrap_or_else(|| {
        UsageInfo::error(
            account.id.clone(),
            "Claude quota is unknown; no successful observation yet".into(),
        )
    });
    usage.plan_type = account.plan_type.clone();
    if let Some(error) = &state.last_error {
        // A later statusline may have cleared UsageInfo.error. HTTP error metadata
        // remains authoritative for this poller until its next successful GET.
        usage.error = Some(if state.authentication_required {
            "Claude authentication expired (invalid_grant; sign in again); automatic retries paused until credentials change".into()
        } else {
            format!(
                "{error}; HTTP retry at {}; retained quota may come from statusline",
                state.next_poll_at
            )
        });
    }
    Ok(usage)
}
async fn cached_with_error(
    m: &Manager,
    account: &ClaudeAccount,
    state: &PollState,
) -> Result<UsageInfo> {
    if state.last_error.is_none() {
        return cached(m, account, state);
    }
    let m = m.clone();
    let account = account.clone();
    tokio::task::spawn_blocking(move || {
        let p = m.profile(&account.id)?;
        let _lock = lock_file(&p.join("usage.lock"))?;
        // Re-read after acquiring the receipt lock: a successful HTTP completion
        // may have cleared an older error while this cached caller was waiting.
        let state: PollState = read_json(&p.join("quota-poll.json"))?.unwrap_or_default();
        ensure!(
            state.account_uuid == account.account_uuid,
            "Claude HTTP poll identity changed"
        );
        let usage = cached(&m, &account, &state)?;
        // UI commands discard refresh return values and read usage.json. Reapply
        // the durable HTTP error even if a full statusline just cleared it.
        m.transaction(|store| {
            let current = store
                .accounts
                .iter()
                .find(|a| a.id == account.id)
                .context("Claude account not found")?;
            ensure!(
                current.account_uuid == account.account_uuid,
                "Claude account UUID changed"
            );
            if let Some(mut receipt) = read_json::<UsageReceipt>(&p.join("usage.json"))? {
                receipt.usage.error = usage.error.clone();
                write_json(&p.join("usage.json"), &receipt)?;
            }
            Ok(usage)
        })
    })
    .await?
}
async fn finish(
    m: &Manager,
    account: &ClaudeAccount,
    attempt: &PollState,
    baseline: Option<&UsageReceipt>,
    expected_oauth: Option<&Value>,
    result: std::result::Result<UsageInfo, Failure>,
) -> Result<UsageInfo> {
    let m = m.clone();
    let account = account.clone();
    let attempt = attempt.clone();
    let baseline = baseline.cloned();
    let expected_oauth = expected_oauth.cloned();
    tokio::task::spawn_blocking(move || {
        finish_sync(
            &m,
            &account,
            &attempt,
            baseline.as_ref(),
            expected_oauth.as_ref(),
            result,
        )
    })
    .await?
}
fn finish_sync(
    m: &Manager,
    account: &ClaudeAccount,
    attempt: &PollState,
    baseline: Option<&UsageReceipt>,
    expected_oauth: Option<&Value>,
    result: std::result::Result<UsageInfo, Failure>,
) -> Result<UsageInfo> {
    let p = m.profile(&account.id)?;
    // History and receipt replacement are one blocking local operation. No await
    // or HTTP holds usage.lock; remember the exact receipt we are replacing.
    let _usage_lock = lock_file(&p.join("usage.lock"))?;
    let previous: Option<UsageReceipt> = read_json(&p.join("usage.json"))?;
    if let Some(receipt) = &previous {
        ensure!(
            receipt.account_id == account.id && receipt.usage.account_id == account.id,
            "Claude receipt belongs to another account"
        );
        monitor::remember_usage_sync(m, receipt)?;
    }
    m.transaction(|store| {
        let current = store
            .accounts
            .iter()
            .find(|a| a.id == account.id)
            .context("Claude account removed during quota poll")?;
        ensure!(
            current.account_uuid == account.account_uuid,
            "Claude account UUID changed during quota poll"
        );
        guard_identity(current, &object(&p.join(".claude.json"))?)?;
        if let Some(expected) = expected_oauth {
            ensure!(oauth(&object(&p.join(".credentials.json"))?)? == expected,
                "Claude credential generation changed before quota completion");
        }
        let _poll_lock = lock_file(&p.join("quota-poll.lock"))?;
        let mut state: PollState = read_json(&p.join("quota-poll.json"))?
            .context("Claude HTTP poll reservation disappeared")?;
        ensure!(
            state.attempt_id == attempt.attempt_id && state.account_uuid == account.account_uuid,
            "Claude HTTP poll reservation changed"
        );
        let now = Utc::now().timestamp();
        let mut receipt = previous.clone().unwrap_or_else(|| UsageReceipt {
            account_id: account.id.clone(),
            observed_at: 0,
            usage: UsageInfo::error(account.id.clone(), "Claude quota is unknown".into()),
            sessions: BTreeMap::new(),
        });
        match result {
            Ok(usage) => {
                // Prefer a complete, newer statusline measurement that arrived
                // during HTTP, preserving its own timestamp and all sessions.
                let newer = previous.as_ref().is_some_and(|r| {
                    r.observed_at >= attempt.last_attempt_at
                        && super::storage::eligible_usage(r, now)
                        && baseline.is_none_or(|b| {
                            b.observed_at != r.observed_at
                                || serde_json::to_value(&b.usage).ok()
                                    != serde_json::to_value(&r.usage).ok()
                        })
                });
                if !newer {
                    receipt.usage = usage;
                    receipt.observed_at = now;
                }
                state.failures = 0;
                state.authentication_required = false;
                state.rejected_credential_generation = None;
                state.last_error = None;
                state.retry_after_seconds = None;
                state.last_success_at = Some(now);
                state.next_poll_at = now.saturating_add(POLL_INTERVAL);
            }
            Err(error) => {
                state.authentication_required = authentication_required(&error.message);
                state.rejected_credential_generation = if state.authentication_required {
                    expected_oauth.map(|oauth| credential_fingerprint(&json!({"claudeAiOauth":oauth}))).transpose()?
                } else { None };
                state.failures = state.failures.saturating_add(1);
                let backoff =
                    (30_i64 * (1_i64 << state.failures.saturating_sub(1).min(7))).min(3600);
                let delay = error
                    .retry_after
                    .unwrap_or(0)
                    .max(backoff)
                    .min(MAX_RETRY_AFTER);
                state.last_error = Some(error.message.clone());
                state.retry_after_seconds = error.retry_after;
                state.next_poll_at = now.saturating_add(delay);
                receipt.usage.error = Some(if state.authentication_required {
                    "Claude authentication expired (invalid_grant; sign in again); automatic retries paused until credentials change".into()
                } else { format!(
                    "{}; HTTP retry at {}; last known quota retained",
                    error.message, state.next_poll_at
                ) });
                // No observation timestamp, utilization or session totals change.
            }
        }
        state.attempt_id = None;
        state.in_flight_until = 0;
        write_json(&p.join("quota-poll.json"), &state)?;
        write_json(&p.join("usage.json"), &receipt)?;
        Ok(receipt.usage)
    })
}
async fn refresh_account_with(
    m: &Manager,
    id: &str,
    force: bool,
    http: &Http,
) -> Result<UsageInfo> {
    let account = m.account(id)?;
    let (attempt, reserved) = reserve(m, &account, force)?;
    if !reserved {
        return cached_with_error(m, &account, &attempt).await;
    }
    let operation = timeout(Duration::from_secs(15), runtime::OPERATION.lock()).await;
    let initial = match operation {
        Ok(_guard) => snapshot(m, id),
        Err(_) => Err(anyhow::anyhow!(
            "Claude activation is busy; quota poll deferred"
        )),
    };
    let mut snapshot = match initial {
        Ok(s) => s,
        Err(error) => {
            return finish(
                m,
                &account,
                &attempt,
                None,
                None,
                Err(Failure::local(error.to_string())),
            )
            .await
        }
    };
    let baseline = snapshot.receipt.clone();
    let result = async {
        snapshot = cswap::adopt_live(m, &snapshot, http).await?;
        // cswap may have refreshed this inactive account since it was imported.
        // Read, verify server UUID and CAS-adopt without switching the live account.
        if let Some(candidate) =
            cswap::candidate(m, &snapshot).map_err(|e| Failure::local(e.to_string()))?
        {
            snapshot = cswap::adopt(m, &snapshot, &candidate, http).await?;
        }
        let expired = snapshot.credentials["claudeAiOauth"]["expiresAt"]
            .as_i64()
            .is_none_or(|n| n <= Utc::now().timestamp_millis() + 300_000);
        let mut refreshed = false;
        if expired
            || m.profile(id)
                .map_err(|e| Failure::local(e.to_string()))?
                .join("quota-token-pending.json")
                .exists()
        {
            snapshot = refresh_token(m, &snapshot, http).await?;
            refreshed = true;
        }
        let token = access(&snapshot.credentials).map_err(|e| Failure::local(e.to_string()))?;
        let data = match http.usage(token).await {
            Err(e) if e.status == Some(StatusCode::UNAUTHORIZED) && !refreshed => {
                snapshot = refresh_token(m, &snapshot, http).await?;
                http.usage(
                    access(&snapshot.credentials).map_err(|e| Failure::local(e.to_string()))?,
                )
                .await?
            }
            other => other?,
        };
        // Reject even valid-looking measurements if credentials were replaced.
        let current = object(
            &m.profile(id)
                .map_err(|e| Failure::local(e.to_string()))?
                .join(".credentials.json"),
        )
        .map_err(|e| Failure::local(e.to_string()))?;
        if oauth(&current).map_err(|e| Failure::local(e.to_string()))? != &snapshot.saved_oauth
            && oauth(&current).map_err(|e| Failure::local(e.to_string()))?
                != &snapshot.credentials["claudeAiOauth"]
        {
            return Err(Failure::local(
                "Claude credential generation changed during quota GET",
            ));
        }
        parse_usage(&snapshot.account, &data).map_err(|e| Failure::local(e.to_string()))
    }
    .await;
    finish(
        m,
        &account,
        &attempt,
        baseline.as_ref(),
        Some(&snapshot.credentials["claudeAiOauth"]),
        result,
    )
    .await
}

/// Poll one account. HTTP/auth/schema failures return Ok(last-known UsageInfo
/// with error set); storage/identity/history failures return Err. This API never
/// substitutes zero for absent measurements. Force respects Retry-After/backoff.
pub async fn refresh_account(m: &Manager, id: &str, force: bool) -> Result<UsageInfo> {
    refresh_account_with(m, id, force, &Http::production()?).await
}
async fn refresh_all_with(m: &Manager, force: bool, http: &Http) -> Result<Vec<UsageInfo>> {
    let accounts = m.store()?.accounts;
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut result = Vec::with_capacity(accounts.len());
    for account in accounts {
        // Never cancel a potentially consumed rotating grant at the batch deadline.
        // Start no more HTTP after 60s; finish the one bounded account in progress.
        let usage = if Instant::now() >= deadline {
            let mut u = cached(m, &account, &PollState::default())
                .unwrap_or_else(|e| UsageInfo::error(account.id.clone(), e.to_string()));
            u.error = Some("Claude HTTP poll batch budget reached; retained quota is stale".into());
            u
        } else {
            refresh_account_with(m, &account.id, force, http)
                .await
                .unwrap_or_else(|e| {
                    let mut u = cached(m, &account, &PollState::default())
                        .unwrap_or_else(|_| UsageInfo::error(account.id.clone(), e.to_string()));
                    u.error = Some(e.to_string());
                    u
                })
        };
        result.push(usage);
    }
    Ok(result)
}
/// Sequential bounded polling. An individual account failure never aborts the
/// remaining inventory. Only failure to read the inventory/client returns Err.
pub async fn refresh_all(m: &Manager, force: bool) -> Result<Vec<UsageInfo>> {
    refresh_all_with(m, force, &Http::production()?).await
}

#[cfg(test)]
mod tests {
    use super::super::SessionUsage;
    use super::*;
    use std::{
        io::Read,
        path::PathBuf,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Mutex,
        },
        thread,
    };

    struct Fixture {
        directory: PathBuf,
        manager: Manager,
    }
    impl Fixture {
        fn new() -> Self {
            let directory = std::env::temp_dir()
                .join(format!("switcher-quota-fixture-{}", uuid::Uuid::new_v4()));
            let manager = Manager {
                root: directory.join("profiles-store"),
                live_dir: directory.join("synthetic-live"),
                live_config: directory.join("synthetic-live/.claude.json"),
            };
            Self { directory, manager }
        }
        fn account(&self, token: &str, expired: bool, live: bool) -> ClaudeAccount {
            let config = json!({"oauthAccount":{"accountUuid":uuid::Uuid::new_v4().to_string()},"preserved":true});
            let credentials = credentials(token, expired);
            let account = self
                .manager
                .add(token, &credentials, &config, &json!({}))
                .unwrap();
            if live {
                // All auth paths are under the fixture directory, never HOME.
                write_json(&self.manager.live_config, &config).unwrap();
                write_json(
                    &self.manager.live_dir.join(".credentials.json"),
                    &credentials,
                )
                .unwrap();
            }
            account
        }
        fn receipt(
            &self,
            account: &ClaudeAccount,
            primary: f64,
            weekly: f64,
            at: i64,
        ) -> UsageReceipt {
            let mut receipt = UsageReceipt {
                account_id: account.id.clone(),
                observed_at: at,
                usage: parse_usage(account, &payload(primary, weekly)).unwrap(),
                sessions: BTreeMap::new(),
            };
            receipt.sessions.insert(
                "synthetic-session".into(),
                SessionUsage {
                    input_tokens: 123,
                    output_tokens: 45,
                    cost_usd: 0.03,
                    day: "2026-10-10".into(),
                },
            );
            write_json(
                &self
                    .manager
                    .profile(&account.id)
                    .unwrap()
                    .join("usage.json"),
                &receipt,
            )
            .unwrap();
            receipt
        }
        fn due(&self, account: &ClaudeAccount) {
            let path = self
                .manager
                .profile(&account.id)
                .unwrap()
                .join("quota-poll.json");
            let mut state: PollState = read_json(&path).unwrap().unwrap();
            state.last_attempt_at = Utc::now().timestamp() - FORCE_FLOOR - 1;
            state.next_poll_at = Utc::now().timestamp() - 1;
            state.in_flight_until = 0;
            write_json(&path, &state).unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            assert!(self.directory.starts_with(std::env::temp_dir()));
            assert!(self
                .directory
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("switcher-quota-fixture-"));
            let _ = fs::remove_dir_all(&self.directory);
        }
    }
    fn credentials(token: &str, expired: bool) -> Value {
        json!({"claudeAiOauth":{
            "accessToken":token,"refreshToken":format!("refresh-{token}"),
            "expiresAt":Utc::now().timestamp_millis() + if expired { -1000 } else { 3_600_000 },
            "subscriptionType":"max","scopes":["user:inference","user:profile"]
        },"unrelated":"retained"})
    }
    fn payload(primary: f64, weekly: f64) -> Value {
        json!({"five_hour":{"utilization":primary,"resets_at":"2099-01-01T12:00:00+09:00"},
            "seven_day":{"utilization":weekly,"resets_at":"2099-01-07T03:00:00Z"}})
    }
    fn token_response(account: &ClaudeAccount) -> Value {
        json!({"access_token":"synthetic-rotated","refresh_token":"synthetic-successor","expires_in":3600,
            "scope":"user:inference user:profile","account":{"uuid":account.account_uuid}})
    }
    struct Reply {
        status: u16,
        headers: Vec<(&'static str, String)>,
        body: String,
    }
    impl Reply {
        fn json(status: u16, value: Value) -> Self {
            Self {
                status,
                headers: vec![],
                body: value.to_string(),
            }
        }
        fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
            self.headers.push((name, value.into()));
            self
        }
    }
    struct Server {
        base: String,
        stop: Arc<AtomicBool>,
        worker: Option<thread::JoinHandle<()>>,
        requests: Arc<Mutex<Vec<Value>>>,
    }
    impl Server {
        fn new(mut respond: impl FnMut(&Value) -> Reply + Send + 'static) -> Self {
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let base = format!("http://{}", server.server_addr());
            let stop = Arc::new(AtomicBool::new(false));
            let flag = stop.clone();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let recorded = requests.clone();
            let worker = thread::spawn(move || {
                while !flag.load(Ordering::SeqCst) {
                    let Some(mut request) = server.recv_timeout(Duration::from_millis(30)).unwrap()
                    else {
                        continue;
                    };
                    let headers = request
                        .headers()
                        .iter()
                        .map(|h| {
                            (
                                h.field.as_str().to_string().to_ascii_lowercase(),
                                Value::String(h.value.to_string()),
                            )
                        })
                        .collect::<serde_json::Map<_, _>>();
                    let mut bytes = String::new();
                    request
                        .as_reader()
                        .take(MAX_BODY as u64)
                        .read_to_string(&mut bytes)
                        .unwrap();
                    let info = json!({"method":request.method().as_str(),"path":request.url(),"headers":headers,
                        "body":serde_json::from_str::<Value>(&bytes).unwrap_or(Value::Null)});
                    recorded.lock().unwrap().push(info.clone());
                    let reply = respond(&info);
                    let mut response =
                        tiny_http::Response::from_string(reply.body).with_status_code(reply.status);
                    for (name, value) in reply.headers {
                        response.add_header(tiny_http::Header::from_bytes(name, value).unwrap());
                    }
                    let _ = request.respond(response);
                }
            });
            Self {
                base,
                stop,
                worker: Some(worker),
                requests,
            }
        }
        fn http(&self) -> Http {
            self.http_timeout(Duration::from_secs(2))
        }
        fn http_timeout(&self, budget: Duration) -> Http {
            assert!(reqwest::Url::parse(&self.base)
                .unwrap()
                .host_str()
                .unwrap()
                .parse::<std::net::IpAddr>()
                .unwrap()
                .is_loopback());
            Http {
                client: Http::client(false, budget).unwrap(),
                usage: format!("{}/usage", self.base),
                token: format!("{}/token", self.base),
                profile: format!("{}/profile", self.base),
            }
        }
        fn count(&self) -> usize {
            self.requests.lock().unwrap().len()
        }
    }
    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(worker) = self.worker.take() {
                worker.join().unwrap();
            }
        }
    }
    fn file_unlocked(path: &Path) {
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        assert!(
            file.try_lock().is_ok(),
            "Unexpected HTTP-time OS lock: {}",
            path.display()
        );
    }

    #[tokio::test]
    async fn repaired_pair_resumes_even_when_pre_adoption_scheduling_generation_is_unchanged() {
        for in_flight in [false, true] {
            let f = Fixture::new();
            let a = f.account("repaired-scheduled-pair", false, false);
            let before = f.receipt(&a, 45.0, 65.0, Utc::now().timestamp() - 10);
            let current = snapshot(&f.manager, &a.id).unwrap();
            let fingerprint = credential_fingerprint(&current.credentials).unwrap();
            let p = f.manager.profile(&a.id).unwrap();
            let now = Utc::now().timestamp();
            let state = PollState {
                account_uuid: a.account_uuid.clone(),
                credential_generation: Some(format!("{fingerprint}:")),
                rejected_credential_generation: Some(
                    credential_fingerprint(&credentials("rejected-after-reservation", true))
                        .unwrap(),
                ),
                authentication_required: true,
                last_error: Some("OAuth refresh HTTP 400 (invalid_grant; sign in again)".into()),
                failures: 3,
                last_attempt_at: now,
                next_poll_at: now + 3600,
                attempt_id: Some("other-process-attempt".into()),
                in_flight_until: if in_flight { now + ATTEMPT_LEASE } else { 0 },
                ..PollState::default()
            };
            write_json(&p.join("quota-poll.json"), &state).unwrap();
            let server = Server::new(|r| {
                assert_eq!(r["method"], "GET");
                assert_eq!(r["path"], "/usage");
                assert_eq!(
                    r["headers"]["authorization"],
                    "Bearer repaired-scheduled-pair"
                );
                Reply::json(200, payload(12.0, 34.0))
            });
            let result = refresh_account_with(&f.manager, &a.id, true, &server.http())
                .await
                .unwrap();
            if in_flight {
                assert!(result.error.unwrap().contains("invalid_grant"));
                assert_eq!(server.count(), 0, "An active lease must not be stolen");
                let held: PollState = read_json(&p.join("quota-poll.json")).unwrap().unwrap();
                assert!(held.authentication_required);
                assert_eq!(held.attempt_id, state.attempt_id);
                assert_eq!(held.in_flight_until, state.in_flight_until);
                assert_eq!(held.next_poll_at, state.next_poll_at);
                f.due(&a);
                let result = refresh_account_with(&f.manager, &a.id, false, &server.http())
                    .await
                    .unwrap();
                assert!(result.error.is_none());
            } else {
                assert!(result.error.is_none());
            }
            assert_eq!(server.count(), 1);
            let receipt = f.manager.receipt(&a.id).unwrap().unwrap();
            assert_eq!(receipt.usage.primary_used_percent, Some(12.0));
            assert_eq!(receipt.usage.secondary_used_percent, Some(34.0));
            assert_eq!(receipt.sessions.len(), before.sessions.len());
            assert_eq!(receipt.sessions["synthetic-session"].input_tokens, 123);
            let poll: PollState = read_json(&p.join("quota-poll.json")).unwrap().unwrap();
            assert!(!poll.authentication_required);
            assert!(poll.rejected_credential_generation.is_none());
        }
    }

    #[test]
    fn changed_profile_cannot_resume_the_same_rejected_external_candidate() {
        let f = Fixture::new();
        let a = f.account("pre-adoption-profile", true, false);
        let current = snapshot(&f.manager, &a.id).unwrap();
        let fingerprint = credential_fingerprint(&current.credentials).unwrap();
        let rejected = credential_fingerprint(&credentials("rejected-external", false)).unwrap();
        let p = f.manager.profile(&a.id).unwrap();
        let state = PollState {
            account_uuid: a.account_uuid.clone(),
            credential_generation: Some(format!("{fingerprint}:{rejected}")),
            rejected_credential_generation: Some(rejected.clone()),
            authentication_required: true,
            last_error: Some("invalid_grant".into()),
            next_poll_at: Utc::now().timestamp() + 3600,
            ..PollState::default()
        };
        write_json(&p.join("quota-poll.json"), &state).unwrap();
        for force in [false, true] {
            let (paused, reserved) =
                reserve_generation(&f.manager, &a, force, &fingerprint, Some(&rejected)).unwrap();
            assert!(
                !reserved,
                "The selected rejected backup must not be replayed"
            );
            assert!(paused.authentication_required);
            assert_eq!(
                paused.rejected_credential_generation.as_deref(),
                Some(rejected.as_str())
            );
        }
        // A different fresh backup can recover authentication immediately.
        let successor = credential_fingerprint(&credentials("new-external", false)).unwrap();
        let (resumed, reserved) =
            reserve_generation(&f.manager, &a, false, &fingerprint, Some(&successor)).unwrap();
        assert!(reserved);
        assert!(!resumed.authentication_required);
    }

    #[tokio::test]
    async fn inactive_get_updates_its_receipt_without_active_identity_or_http_time_locks() {
        let f = Fixture::new();
        let active = f.account("healthy-active", false, true);
        let inactive = f.account("healthy-inactive", false, false);
        let active_receipt = f.receipt(&active, 70.0, 80.0, Utc::now().timestamp());
        let inactive_receipt = f.receipt(&inactive, 10.0, 20.0, Utc::now().timestamp() - 400);
        let active_profile = f.manager.profile(&active.id).unwrap();
        let inactive_profile = f.manager.profile(&inactive.id).unwrap();
        let paths = [
            f.manager.live_config.clone(),
            f.manager.live_dir.join(".credentials.json"),
            active_profile.join(".credentials.json"),
            inactive_profile.join(".credentials.json"),
            active_profile.join("usage.json"),
        ];
        let before = paths
            .iter()
            .map(|p| fs::read(p).unwrap())
            .collect::<Vec<_>>();
        let m = f.manager.clone();
        let profile = inactive_profile.clone();
        let server = Server::new(move |r| {
            assert_eq!(r["method"], "GET");
            assert_eq!(r["path"], "/usage");
            assert_eq!(r["headers"]["authorization"], "Bearer healthy-inactive");
            assert!(runtime::OPERATION.try_lock().is_ok());
            file_unlocked(&m.root.join("accounts.lock"));
            for name in ["usage.lock", "quota-poll.lock", "credentials.lock"] {
                file_unlocked(&profile.join(name));
            }
            assert!(!m.live_dir.join(".oauth_refresh.lock").exists());
            assert!(!profile.join(".oauth_refresh.lock").exists());
            Reply::json(200, payload(15.0, 25.0))
        });
        let result = refresh_account_with(&f.manager, &inactive.id, false, &server.http())
            .await
            .unwrap();
        assert!(result.error.is_none());
        assert_eq!(result.primary_used_percent, Some(15.0));
        assert_eq!(
            server.count(),
            1,
            "No refresh POST or identity GET is needed"
        );
        for (path, bytes) in paths.iter().zip(before) {
            assert!(fs::read(path).unwrap() == bytes);
        }
        let receipt = f.manager.receipt(&inactive.id).unwrap().unwrap();
        assert!(receipt.observed_at > inactive_receipt.observed_at);
        assert_eq!(receipt.sessions["synthetic-session"].input_tokens, 123);
        assert_eq!(
            f.manager.receipt(&active.id).unwrap().unwrap().observed_at,
            active_receipt.observed_at
        );
        assert_eq!(
            f.manager.active_id(&f.manager.store().unwrap()).unwrap(),
            Some(active.id)
        );
    }

    #[tokio::test]
    async fn invalid_grant_pauses_until_credentials_change_even_after_backoff_and_force() {
        let f = Fixture::new();
        let a = f.account("rejected", true, false);
        let before = f.receipt(&a, 45.0, 65.0, Utc::now().timestamp() - 10);
        let server = Server::new(|r| match r["path"].as_str().unwrap() {
            "/token" => Reply::json(400, json!({"error":"invalid_grant"})),
            "/usage" => Reply::json(200, payload(12.0, 34.0)),
            _ => panic!("Unexpected request"),
        });
        let failed = refresh_account_with(&f.manager, &a.id, false, &server.http())
            .await
            .unwrap();
        assert!(failed.error.unwrap().contains("automatic retries paused"));
        assert_eq!(server.count(), 1);
        f.due(&a);
        let cached = refresh_account_with(&f.manager, &a.id, true, &server.http())
            .await
            .unwrap();
        assert!(cached.error.unwrap().contains("invalid_grant"));
        assert_eq!(server.count(), 1, "A rejected grant must not be replayed");
        let receipt = f.manager.receipt(&a.id).unwrap().unwrap();
        assert_eq!(receipt.observed_at, before.observed_at);
        assert_eq!(receipt.usage.primary_used_percent, Some(45.0));
        assert_eq!(
            serde_json::to_value(receipt.sessions).unwrap(),
            serde_json::to_value(before.sessions).unwrap()
        );
        let p = f.manager.profile(&a.id).unwrap();
        write_json(
            &p.join(".credentials.json"),
            &credentials("repaired", false),
        )
        .unwrap();
        let repaired = refresh_account_with(&f.manager, &a.id, true, &server.http())
            .await
            .unwrap();
        assert!(repaired.error.is_none());
        assert_eq!(repaired.primary_used_percent, Some(12.0));
        assert_eq!(server.count(), 2);
        let poll: PollState = read_json(&p.join("quota-poll.json")).unwrap().unwrap();
        assert!(!poll.authentication_required);
        let serialized = serde_json::to_string(&poll).unwrap();
        assert!(!serialized.contains("refresh-repaired"));
        assert!(!serialized.contains("refresh-rejected"));
    }

    #[tokio::test]
    async fn adopted_pair_rejection_stays_paused_when_candidate_disappears() {
        let f = Fixture::new();
        let a = f.account("predecessor", true, false);
        let before = f.receipt(&a, 45.0, 65.0, Utc::now().timestamp() - 10);
        let (attempt, reserved) = reserve(&f.manager, &a, false).unwrap();
        assert!(reserved);
        let successor = credentials("adopted-successor", false);
        let p = f.manager.profile(&a.id).unwrap();
        write_json(&p.join(".credentials.json"), &successor).unwrap();
        let mut reserved_state = attempt.clone();
        reserved_state.credential_generation = Some(format!(
            "predecessor:{}",
            credential_fingerprint(&successor).unwrap()
        ));
        write_json(&p.join("quota-poll.json"), &reserved_state).unwrap();
        finish_sync(
            &f.manager,
            &a,
            &attempt,
            Some(&before),
            Some(&successor["claudeAiOauth"]),
            Err(Failure::local("invalid_grant")),
        )
        .unwrap();
        f.due(&a);
        let (paused, reserved) = reserve(&f.manager, &a, true).unwrap();
        assert!(
            !reserved,
            "Candidate disappearance cannot resurrect the rejected adopted pair"
        );
        assert!(paused.authentication_required);
        assert_eq!(
            paused.rejected_credential_generation,
            Some(credential_fingerprint(&successor).unwrap())
        );
    }
    #[tokio::test]
    async fn credential_change_does_not_bypass_server_rate_limit_backoff() {
        let f = Fixture::new();
        let a = f.account("limited", false, false);
        let server = Server::new(|_| Reply::json(429, json!({})).header("Retry-After", "600"));
        refresh_account_with(&f.manager, &a.id, false, &server.http())
            .await
            .unwrap();
        let p = f.manager.profile(&a.id).unwrap();
        write_json(&p.join(".credentials.json"), &credentials("changed", false)).unwrap();
        refresh_account_with(&f.manager, &a.id, true, &server.http())
            .await
            .unwrap();
        assert_eq!(server.count(), 1);
        let poll: PollState = read_json(&p.join("quota-poll.json")).unwrap().unwrap();
        assert!(poll.next_poll_at >= Utc::now().timestamp() + 590);
    }

    #[test]
    fn rfc3339_percentages_and_explicit_idle_windows_are_validated() {
        let f = Fixture::new();
        let account = f.account("parser", false, false);
        let parsed = parse_usage(&account, &payload(56.25, 21.0)).unwrap();
        assert_eq!(parsed.primary_used_percent, Some(56.25));
        assert_eq!(
            parsed.primary_resets_at,
            Some(
                DateTime::parse_from_rfc3339("2099-01-01T03:00:00Z")
                    .unwrap()
                    .timestamp()
            )
        );
        let idle = parse_usage(&account, &json!({"five_hour":{"utilization":0,"resets_at":null},"seven_day":{"utilization":0,"resets_at":null}})).unwrap();
        assert_eq!(idle.primary_used_percent, Some(0.0));
        assert_eq!(idle.primary_resets_at, None);
        for bad in [
            json!({}),
            json!({"five_hour":null,"seven_day":null}),
            json!({"five_hour":{"utilization":0,"resets_at":null}}),
        ] {
            assert!(parse_usage(&account, &bad).is_err());
        }
        for invalid in [
            json!({"utilization":true,"resets_at":null}),
            json!({"utilization":-1,"resets_at":null}),
            json!({"utilization":101,"resets_at":"2099-01-01T00:00:00Z"}),
            json!({"utilization":1,"resets_at":null}),
            json!({"utilization":0}),
            json!({"utilization":0,"resets_at":123}),
            json!({"utilization":1,"resets_at":"2000-01-01T00:00:00Z"}),
            json!({"utilization":1,"resets_at":"2099-01-01 00:00:00"}),
        ] {
            let mut value = payload(1.0, 2.0);
            value["five_hour"] = invalid;
            assert!(parse_usage(&account, &value).is_err());
        }
    }
    #[test]
    fn retry_after_supports_seconds_dates_and_bounded_extremes() {
        let now = 1_700_000_000;
        for (raw, expected) in [
            ("0".to_string(), Some(0)),
            ("120".into(), Some(120)),
            (
                "999999999999999999999999999999".into(),
                Some(MAX_RETRY_AFTER),
            ),
            ("NaN".into(), None),
            ("-4".into(), None),
            (
                DateTime::<Utc>::from_timestamp(now + 90, 0)
                    .unwrap()
                    .to_rfc2822(),
                Some(90),
            ),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(RETRY_AFTER, raw.parse().unwrap());
            assert_eq!(retry_after(&headers, now), expected);
        }
        assert!(
            Http::production().is_err(),
            "Unit tests must never access production OAuth"
        );
    }
    #[tokio::test]
    async fn fresh_statusline_does_not_starve_http_and_sessions_and_reset_history_survive() {
        let f = Fixture::new();
        let a = f.account("fresh", false, true);
        let before = f.receipt(&a, 56.0, 21.0, Utc::now().timestamp());
        let server = Server::new(|r| {
            assert_eq!(r["headers"]["anthropic-beta"], BETA);
            Reply::json(200, payload(60.0, 22.0))
        });
        let result = refresh_account_with(&f.manager, &a.id, false, &server.http())
            .await
            .unwrap();
        assert_eq!(result.primary_used_percent, Some(60.0));
        assert_eq!(server.count(), 1);
        let receipt = f.manager.receipt(&a.id).unwrap().unwrap();
        assert_eq!(receipt.sessions["synthetic-session"].input_tokens, 123);
        let ledger = object(&f.manager.root.join("warmup-monitor.json")).unwrap();
        assert!(ledger["accounts"][&a.id]["primary_windows"]
            .as_array()
            .unwrap()
            .contains(&json!(before.usage.primary_resets_at.unwrap())));
        let poll: PollState = read_json(&f.manager.profile(&a.id).unwrap().join("quota-poll.json"))
            .unwrap()
            .unwrap();
        assert_eq!(poll.next_poll_at - poll.last_success_at.unwrap(), 180);
        f.receipt(&a, 61.0, 22.0, Utc::now().timestamp());
        refresh_account_with(&f.manager, &a.id, false, &server.http())
            .await
            .unwrap();
        refresh_account_with(&f.manager, &a.id, true, &server.http())
            .await
            .unwrap();
        assert_eq!(
            server.count(),
            1,
            "Force must respect the short serve floor"
        );
    }
    #[tokio::test]
    async fn rate_limit_preserves_stale_exhausted_alternate_and_survives_statusline_rewrite() {
        let f = Fixture::new();
        let a = f.account("alternate", false, false);
        let before = f.receipt(&a, 0.0, 100.0, Utc::now().timestamp() - 43_966);
        let server = Server::new(|_| {
            Reply::json(429, json!({"error":"fixture"})).header("Retry-After", "7200")
        });
        let result = refresh_account_with(&f.manager, &a.id, true, &server.http())
            .await
            .unwrap();
        assert_eq!(result.primary_used_percent, Some(0.0));
        assert_eq!(result.secondary_used_percent, Some(100.0));
        assert!(result.error.as_ref().unwrap().contains("429"));
        assert_eq!(
            f.manager.receipt(&a.id).unwrap().unwrap().observed_at,
            before.observed_at
        );
        assert!(!super::super::storage::eligible_usage(
            &f.manager.receipt(&a.id).unwrap().unwrap(),
            Utc::now().timestamp()
        ));
        let rewritten = f.receipt(&a, 56.0, 21.0, Utc::now().timestamp());
        let result = refresh_account_with(&f.manager, &a.id, true, &server.http())
            .await
            .unwrap();
        assert!(result.error.unwrap().contains("429"));
        assert_eq!(server.count(), 1);
        let after = f.manager.receipt(&a.id).unwrap().unwrap();
        assert!(after.usage.error.unwrap().contains("429"));
        assert_eq!(after.observed_at, rewritten.observed_at);
        let poll: PollState = read_json(&f.manager.profile(&a.id).unwrap().join("quota-poll.json"))
            .unwrap()
            .unwrap();
        assert_eq!(poll.retry_after_seconds, Some(7200));
        assert!(poll.next_poll_at >= Utc::now().timestamp() + 7198);
    }
    #[tokio::test]
    async fn malformed_http_response_and_missing_windows_never_become_zero_quota() {
        for response in [
            "not JSON".to_string(),
            json!({}).to_string(),
            json!({"five_hour":{"utilization":0,"resets_at":null},"seven_day":null}).to_string(),
        ] {
            let f = Fixture::new();
            let a = f.account("malformed", false, false);
            let old = f.receipt(&a, 30.0, 90.0, Utc::now().timestamp() - 10);
            let server = Server::new(move |_| Reply {
                status: 200,
                headers: vec![],
                body: response.clone(),
            });
            let result = refresh_account_with(&f.manager, &a.id, false, &server.http())
                .await
                .unwrap();
            assert_eq!(result.secondary_used_percent, Some(90.0));
            assert!(result.error.is_some());
            assert_eq!(
                f.manager.receipt(&a.id).unwrap().unwrap().observed_at,
                old.observed_at
            );
        }
    }
    #[tokio::test]
    async fn redirects_are_not_followed_and_response_size_and_time_are_bounded() {
        for mode in 0..3 {
            let f = Fixture::new();
            let a = f.account("bounded", false, false);
            f.receipt(&a, 30.0, 40.0, Utc::now().timestamp());
            let server = Server::new(move |_| match mode {
                0 => Reply::json(302, json!({}))
                    .header("Location", "http://127.0.0.1:1/credential-leak"),
                1 => Reply {
                    status: 200,
                    headers: vec![],
                    body: "x".repeat(MAX_BODY + 1),
                },
                _ => {
                    thread::sleep(Duration::from_millis(250));
                    Reply::json(200, payload(1.0, 2.0))
                }
            });
            let started = Instant::now();
            let result = refresh_account_with(
                &f.manager,
                &a.id,
                false,
                &server.http_timeout(Duration::from_millis(100)),
            )
            .await
            .unwrap();
            assert!(result.error.is_some());
            assert_eq!(result.primary_used_percent, Some(30.0));
            assert_eq!(server.count(), 1);
            assert!(started.elapsed() < Duration::from_secs(2));
        }
    }
    #[tokio::test]
    async fn sequential_inventory_continues_after_independent_http_and_storage_failures() {
        let f = Fixture::new();
        let bad = f.account("bad", false, false);
        let missing = f.account("missing", false, false);
        let good = f.account("good", false, false);
        f.receipt(&bad, 70.0, 80.0, Utc::now().timestamp());
        fs::remove_file(
            f.manager
                .profile(&missing.id)
                .unwrap()
                .join(".credentials.json"),
        )
        .unwrap();
        let server = Server::new(|r| {
            if r["headers"]["authorization"] == "Bearer bad" {
                Reply::json(500, json!({}))
            } else {
                Reply::json(200, payload(12.0, 13.0))
            }
        });
        let result = refresh_all_with(&f.manager, false, &server.http())
            .await
            .unwrap();
        assert_eq!(result.len(), 3);
        assert!(result[0].error.is_some());
        assert!(result[1].error.is_some());
        assert!(result[2].error.is_none());
        assert_eq!(result[0].secondary_used_percent, Some(80.0));
        assert_eq!(result[2].account_id, good.id);
        assert_eq!(server.count(), 2);
        f.due(&bad);
        refresh_account_with(&f.manager, &bad.id, false, &server.http())
            .await
            .unwrap();
        let poll: PollState =
            read_json(&f.manager.profile(&bad.id).unwrap().join("quota-poll.json"))
                .unwrap()
                .unwrap();
        assert_eq!(poll.failures, 2);
        assert!(poll.next_poll_at - Utc::now().timestamp() >= 59);
    }
    #[tokio::test]
    async fn inactive_expired_account_refreshes_without_overwriting_current_login() {
        let f = Fixture::new();
        let active = f.account("current-owner", false, true);
        let inactive = f.account("expired-alternate", true, false);
        let profile = f.manager.profile(&inactive.id).unwrap();
        let live_before = fs::read(f.manager.live_dir.join(".credentials.json")).unwrap();
        let config_before = fs::read(&f.manager.live_config).unwrap();
        let response = token_response(&inactive);
        let server = Server::new(move |r| match r["path"].as_str().unwrap() {
            "/token" => Reply::json(200, response.clone()),
            _ => Reply::json(200, payload(31.0, 64.0)),
        });
        let result = refresh_account_with(&f.manager, &inactive.id, false, &server.http())
            .await
            .unwrap();
        assert!(result.error.is_none());
        assert_eq!(result.primary_used_percent, Some(31.0));
        assert_eq!(server.count(), 2);
        assert_eq!(
            fs::read(f.manager.live_dir.join(".credentials.json")).unwrap(),
            live_before
        );
        assert_eq!(fs::read(&f.manager.live_config).unwrap(), config_before);
        assert_eq!(
            f.manager.active_id(&f.manager.store().unwrap()).unwrap(),
            Some(active.id)
        );
        assert_eq!(
            object(&profile.join(".credentials.json")).unwrap()["claudeAiOauth"]["refreshToken"],
            "synthetic-successor"
        );
    }

    #[tokio::test]
    async fn expired_active_refresh_holds_protocol_guards_and_cas_persists_both_synthetic_files() {
        let f = Fixture::new();
        let a = f.account("expired", true, true);
        let response = token_response(&a);
        let m = f.manager.clone();
        let id = a.id.clone();
        let server = Server::new(move |r| {
            let p = m.profile(&id).unwrap();
            file_unlocked(&m.root.join("accounts.lock"));
            file_unlocked(&p.join("credentials.lock"));
            file_unlocked(&p.join("usage.lock"));
            if r["path"] == "/token" {
                assert!(m.live_dir.join(".oauth_refresh.lock").is_dir());
                assert!(p.join(".oauth_refresh.lock").is_dir());
                assert!(m.live_dir.with_file_name("synthetic-live.lock").is_dir());
                assert_eq!(r["body"]["client_id"], CLIENT_ID);
                assert_eq!(r["body"]["refresh_token"], "refresh-expired");
                Reply::json(200, response.clone())
            } else {
                assert_eq!(r["headers"]["authorization"], "Bearer synthetic-rotated");
                assert!(!m.live_dir.join(".oauth_refresh.lock").exists());
                assert!(!p.join(".oauth_refresh.lock").exists());
                Reply::json(200, payload(2.0, 3.0))
            }
        });
        let result = refresh_account_with(&f.manager, &a.id, false, &server.http())
            .await
            .unwrap();
        assert!(result.error.is_none());
        assert_eq!(server.count(), 2);
        let p = f.manager.profile(&a.id).unwrap();
        let saved = object(&p.join(".credentials.json")).unwrap();
        assert_eq!(
            saved["claudeAiOauth"]["refreshToken"],
            "synthetic-successor"
        );
        let live = object(&f.manager.live_dir.join(".credentials.json")).unwrap();
        assert_eq!(saved["claudeAiOauth"], live["claudeAiOauth"]);
        assert_eq!(live["unrelated"], "retained");
        assert!(!p.join("quota-token-pending.json").exists());
    }
    #[tokio::test]
    async fn unauthorized_get_refreshes_once_and_profile_uuid_is_checked_before_install() {
        let f = Fixture::new();
        let a = f.account("unauthorized", false, false);
        let uuid = a.account_uuid.clone();
        let server = Server::new(move |r| match r["path"].as_str().unwrap() {
            "/token" => Reply::json(
                200,
                json!({"access_token":"new","refresh_token":"new-refresh","expires_in":3600}),
            ),
            "/profile" => Reply::json(200, json!({"account":{"uuid":uuid}})),
            _ if r["headers"]["authorization"] == "Bearer unauthorized" => {
                Reply::json(401, json!({}))
            }
            _ => Reply::json(200, payload(3.0, 4.0)),
        });
        let result = refresh_account_with(&f.manager, &a.id, false, &server.http())
            .await
            .unwrap();
        assert!(result.error.is_none());
        assert_eq!(server.count(), 4);
        assert_eq!(
            server
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r["path"] == "/token")
                .count(),
            1
        );
    }
    #[tokio::test]
    async fn wrong_refresh_account_uuid_cannot_replace_any_credentials() {
        let f = Fixture::new();
        let a = f.account("wrong-id", true, true);
        let profile = f.manager.profile(&a.id).unwrap().join(".credentials.json");
        let before = fs::read(&profile).unwrap();
        let live_before = fs::read(f.manager.live_dir.join(".credentials.json")).unwrap();
        let server = Server::new(|_| {
            Reply::json(
                200,
                json!({"access_token":"foreign","refresh_token":"foreign-refresh","expires_in":3600,"account":{"uuid":"foreign-account"}}),
            )
        });
        let result = refresh_account_with(&f.manager, &a.id, false, &server.http())
            .await
            .unwrap();
        assert!(result.error.unwrap().contains("UUID"));
        assert_eq!(fs::read(profile).unwrap(), before);
        assert_eq!(
            fs::read(f.manager.live_dir.join(".credentials.json")).unwrap(),
            live_before
        );
        assert_eq!(server.count(), 1);
    }
    #[tokio::test]
    async fn concurrent_profile_rotation_wins_cas_and_live_changes_are_not_undone() {
        for change_profile in [true, false] {
            let f = Fixture::new();
            let a = f.account("cas", true, true);
            let m = f.manager.clone();
            let id = a.id.clone();
            let response = token_response(&a);
            let winner = credentials("concurrent-winner", false);
            let win = winner.clone();
            let server = Server::new(move |r| {
                if r["path"] == "/token" {
                    let path = if change_profile {
                        m.profile(&id).unwrap().join(".credentials.json")
                    } else {
                        m.live_dir.join(".credentials.json")
                    };
                    // Deliberately non-cooperative synthetic writer exercises CAS.
                    write_json(&path, &win).unwrap();
                    Reply::json(200, response.clone())
                } else {
                    Reply::json(200, payload(4.0, 5.0))
                }
            });
            let result = refresh_account_with(&f.manager, &a.id, false, &server.http()).await;
            let live = object(&f.manager.live_dir.join(".credentials.json")).unwrap();
            if change_profile {
                // A newer saved grant invalidates the entire old completion,
                // including its error receipt and retry state.
                assert!(result
                    .unwrap_err()
                    .to_string()
                    .contains("credential generation changed"));
                assert_eq!(
                    object(&f.manager.profile(&a.id).unwrap().join(".credentials.json")).unwrap()
                        ["claudeAiOauth"],
                    winner["claudeAiOauth"]
                );
                assert_eq!(live["claudeAiOauth"]["accessToken"], "cas");
            } else {
                assert!(result.unwrap().error.is_none());
                assert_eq!(live, winner);
            }
        }
    }
    #[tokio::test]
    async fn pending_successor_is_verified_without_replaying_consumed_refresh_token() {
        let f = Fixture::new();
        let a = f.account("pending", true, true);
        let uuid = a.account_uuid.clone();
        let mut profile_calls = 0;
        let server = Server::new(move |r| match r["path"].as_str().unwrap() {
            "/token" => Reply::json(
                200,
                json!({"access_token":"pending-new","refresh_token":"pending-refresh","expires_in":3600}),
            ),
            "/profile" => {
                profile_calls += 1;
                if profile_calls == 1 {
                    Reply::json(503, json!({}))
                } else {
                    Reply::json(200, json!({"account":{"uuid":uuid}}))
                }
            }
            _ => Reply::json(200, payload(7.0, 8.0)),
        });
        let first = refresh_account_with(&f.manager, &a.id, false, &server.http())
            .await
            .unwrap();
        assert!(first.error.is_some());
        assert_eq!(
            object(&f.manager.live_dir.join(".credentials.json")).unwrap()["claudeAiOauth"]
                ["accessToken"],
            "pending"
        );
        f.due(&a);
        let second = refresh_account_with(&f.manager, &a.id, false, &server.http())
            .await
            .unwrap();
        assert!(second.error.is_none());
        assert_eq!(
            server
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r["path"] == "/token")
                .count(),
            1
        );
        assert_eq!(
            object(&f.manager.live_dir.join(".credentials.json")).unwrap()["claudeAiOauth"]
                ["accessToken"],
            "pending-new"
        );
    }
    #[tokio::test]
    async fn newer_live_credentials_supply_get_without_independent_active_refresh() {
        let f = Fixture::new();
        let a = f.account("saved-old", true, true);
        let fresh = credentials("legitimate-cli-rotation", false);
        write_json(&f.manager.live_dir.join(".credentials.json"), &fresh).unwrap();
        let uuid = a.account_uuid.clone();
        let manager = f.manager.clone();
        let server = Server::new(move |r| {
            if r["path"] == "/profile" {
                return Reply::json(200, json!({"account":{"uuid":uuid}}));
            }
            assert_eq!(r["path"], "/usage");
            assert_eq!(
                r["headers"]["authorization"],
                "Bearer legitimate-cli-rotation"
            );
            // Concurrent synchronization of the exact effective grant must not
            // reject this successful GET or bring the consumed predecessor back.
            manager.sync_active().unwrap();
            Reply::json(200, payload(9.0, 10.0))
        });
        let result = refresh_account_with(&f.manager, &a.id, false, &server.http())
            .await
            .unwrap();
        assert!(result.error.is_none());
        assert_eq!(server.count(), 2);
        assert_eq!(
            object(&f.manager.live_dir.join(".credentials.json")).unwrap(),
            fresh
        );
        assert_eq!(
            object(&f.manager.profile(&a.id).unwrap().join(".credentials.json")).unwrap()
                ["claudeAiOauth"],
            fresh["claudeAiOauth"]
        );
    }
    #[tokio::test]
    async fn corrupt_neighbor_profile_cannot_starve_a_healthy_active_account() {
        let f = Fixture::new();
        let a = f.account("healthy", false, true);
        let broken = f.account("corrupt", false, false);
        fs::write(
            f.manager
                .profile(&broken.id)
                .unwrap()
                .join(".credentials.json"),
            b"malformed",
        )
        .unwrap();
        let server = Server::new(|r| {
            assert_eq!(r["headers"]["authorization"], "Bearer healthy");
            Reply::json(200, payload(10.0, 11.0))
        });
        let result = refresh_all_with(&f.manager, false, &server.http())
            .await
            .unwrap();
        assert!(result[0].error.is_none());
        assert!(result[1].error.is_some());
        assert_eq!(server.count(), 1);
        assert_eq!(result[0].account_id, a.id);
    }
    #[tokio::test]
    async fn pending_profile_commit_repairs_unchanged_live_without_second_refresh_post() {
        let f = Fixture::new();
        let a = f.account("partial-commit", true, true);
        let before = snapshot(&f.manager, &a.id).unwrap();
        let candidate = rotated(&before, &token_response(&a)).unwrap();
        let p = f.manager.profile(&a.id).unwrap();
        write_json(
            &p.join("quota-token-pending.json"),
            &PendingRotation {
                account_uuid: a.account_uuid.clone(),
                previous_oauth: before.saved_oauth,
                credentials: candidate.clone(),
                live: before.live,
            },
        )
        .unwrap();
        write_json(&p.join(".credentials.json"), &candidate).unwrap();
        let uuid = a.account_uuid.clone();
        let server = Server::new(move |r| match r["path"].as_str().unwrap() {
            "/profile" => Reply::json(200, json!({"account":{"uuid":uuid}})),
            "/usage" => Reply::json(200, payload(2.0, 3.0)),
            _ => panic!("A consumed predecessor must never be replayed"),
        });
        let result = refresh_account_with(&f.manager, &a.id, false, &server.http())
            .await
            .unwrap();
        assert!(result.error.is_none());
        assert_eq!(server.count(), 2);
        assert_eq!(
            object(&f.manager.live_dir.join(".credentials.json")).unwrap()["claudeAiOauth"],
            candidate["claudeAiOauth"]
        );
    }
}
