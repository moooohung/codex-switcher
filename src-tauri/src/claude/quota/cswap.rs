//! Read-only cswap successor discovery and identity-verified credential adoption.
//! Scheduling/backoff belongs to the parent. No refresh grants, cswap writes,
//! account switches, logins, or credential-bearing diagnostics happen here.

use super::super::storage::validate_credentials;
use super::{
    access, credential_fingerprint, persist_rotation, raw, response_uuid, runtime, snapshot,
    CredentialsGuard, Failure, Http, LiveSnapshot, Manager, Snapshot,
};
use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use chrono::Utc;
use serde_json::Value;
use std::{
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::time::{timeout_at, Instant};

const MAX_JSON: usize = 256 * 1024;
const MAX_ENCODED: usize = MAX_JSON.div_ceil(3) * 4;
const EXPIRY_MARGIN_MS: i64 = 300_000;
const GUARD_BUDGET: Duration = Duration::from_secs(15);

// Deliberately no Debug: both the parsed value and captured files contain secrets.
#[derive(Clone)]
pub(super) struct Candidate {
    pub(super) credentials: Value,
    pub(super) fingerprint: String,
    source: Source,
}

#[derive(Clone)]
struct Source {
    config_path: PathBuf,
    encoded_path: PathBuf,
    config: Vec<u8>,
    encoded: Vec<u8>,
}

fn bounded(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let file = File::open(path).map_err(|_| anyhow::anyhow!("Cannot read cswap backup"))?;
    let meta = file
        .metadata()
        .map_err(|_| anyhow::anyhow!("Cannot inspect cswap backup"))?;
    ensure!(
        meta.is_file() && meta.len() <= limit as u64,
        "Invalid cswap backup size"
    );
    // Bound the read too: a file may grow after the metadata check.
    let mut bytes = Vec::new();
    file.take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow::anyhow!("Cannot read cswap backup"))?;
    ensure!(bytes.len() <= limit, "Invalid cswap backup size");
    Ok(bytes)
}

fn json(bytes: &[u8]) -> Result<Value> {
    let text = std::str::from_utf8(bytes).context("Invalid cswap backup UTF-8")?;
    let value: Value =
        serde_json::from_str(text).map_err(|_| anyhow::anyhow!("Invalid cswap backup JSON"))?;
    ensure!(value.is_object(), "Invalid cswap backup object");
    Ok(value)
}

fn decode(encoded: &[u8]) -> Result<Value> {
    ensure!(encoded.len() <= MAX_ENCODED, "Invalid cswap backup size");
    let text = std::str::from_utf8(encoded).context("Invalid cswap backup UTF-8")?;
    let bytes = STANDARD
        .decode(text.trim_matches(|c: char| c.is_ascii_whitespace()))
        .map_err(|_| anyhow::anyhow!("Invalid cswap backup base64"))?;
    ensure!(bytes.len() <= MAX_JSON, "Invalid cswap backup size");
    let value = json(&bytes)?;
    validate_credentials(&value)
        .map_err(|_| anyhow::anyhow!("Invalid cswap backup credentials"))?;
    Ok(value)
}

fn saved_uuid(old: &Snapshot) -> Option<&str> {
    old.account
        .account_uuid
        .as_deref()
        .filter(|s| !s.trim().is_empty())
}

fn expiry(credentials: &Value) -> Option<i64> {
    credentials
        .pointer("/claudeAiOauth/expiresAt")
        .and_then(Value::as_i64)
}

fn near_expiry(old: &Snapshot, now: i64) -> bool {
    saved_uuid(old).is_some()
        && expiry(&old.credentials).is_some_and(|n| n <= now.saturating_add(EXPIRY_MARGIN_MS))
}

fn successor(credentials: &Value, old: &Value, now: i64) -> bool {
    let (Some(next), Some(previous)) = (expiry(credentials), expiry(old)) else {
        return false;
    };
    next > now.saturating_add(EXPIRY_MARGIN_MS)
        && next > previous
        && ["accessToken", "refreshToken"].iter().all(|field| {
            let next = credentials["claudeAiOauth"][*field].as_str();
            next.is_some_and(|s| !s.trim().is_empty())
        })
        && ["accessToken", "refreshToken"].iter().any(|field| {
            credentials["claudeAiOauth"][*field].as_str() != old["claudeAiOauth"][*field].as_str()
        })
}

impl Candidate {
    fn confirm_value(&self, uuid: &str) -> Result<()> {
        ensure!(
            json(&self.source.config)?
                .pointer("/oauthAccount/accountUuid")
                .and_then(Value::as_str)
                == Some(uuid),
            "Cswap backup account UUID changed"
        );
        ensure!(
            decode(&self.source.encoded)? == self.credentials,
            "Cswap candidate changed"
        );
        ensure!(
            credential_fingerprint(&self.credentials)
                .map_err(|_| anyhow::anyhow!("Invalid cswap candidate credentials"))?
                == self.fingerprint,
            "Cswap candidate changed"
        );
        Ok(())
    }

    fn confirm_source(&self, uuid: &str) -> Result<()> {
        self.confirm_value(uuid)?;
        ensure!(
            bounded(&self.source.config_path, MAX_JSON)? == self.source.config
                && bounded(&self.source.encoded_path, MAX_ENCODED)? == self.source.encoded,
            "Cswap backup changed during adoption; retry deferred"
        );
        Ok(())
    }
}

pub(super) fn candidate(m: &Manager, old: &Snapshot) -> Result<Option<Candidate>> {
    // Production home must never be consulted by any unit-test caller, including
    // the parent's polling tests. Fixtures explicitly call candidate_at instead.
    #[cfg(test)]
    {
        let _ = (m, old);
        Ok(None)
    }
    #[cfg(not(test))]
    {
        if !near_expiry(old, Utc::now().timestamp_millis()) {
            return Ok(None);
        }
        let root = super::super::storage::user_home()?.join(".claude-swap-backup");
        candidate_at(m, old, &root)
    }
}

/// Explicit isolated-root entry point, also usable by the parent's adoption
/// fixtures. Candidate literals cannot omit the source bytes needed for CAS.
pub(super) fn candidate_at(_m: &Manager, old: &Snapshot, root: &Path) -> Result<Option<Candidate>> {
    let now = Utc::now().timestamp_millis();
    if !near_expiry(old, now) {
        return Ok(None);
    }
    let uuid = saved_uuid(old).context("Saved Claude account UUID is missing")?;
    let entries = match fs::read_dir(root.join("configs")) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => anyhow::bail!("Cannot inspect cswap backup directory"),
    };
    let mut freshest: Option<Candidate> = None;
    for entry in entries {
        let entry = entry.map_err(|_| anyhow::anyhow!("Cannot inspect cswap backup entry"))?;
        let name = entry.file_name();
        let Some(stem) = name
            .to_str()
            .and_then(|name| name.strip_prefix(".claude-config-"))
            .and_then(|name| name.strip_suffix(".json"))
        else {
            continue;
        };
        let Some((slot, label)) = stem.split_once('-') else {
            continue;
        };
        if slot.is_empty() || !slot.bytes().all(|b| b.is_ascii_digit()) || label.trim().is_empty() {
            continue;
        }
        // Ignore malformed/unreadable individual backups; they must not poison
        // another account or conceal a valid successor in a different slot.
        let captured = (|| -> Result<Option<Candidate>> {
            let config_path = entry.path();
            let config = bounded(&config_path, MAX_JSON)?;
            if json(&config)?
                .pointer("/oauthAccount/accountUuid")
                .and_then(Value::as_str)
                != Some(uuid)
            {
                return Ok(None);
            }
            let encoded_path = root.join("credentials").join(format!(".creds-{stem}.enc"));
            let encoded = bounded(&encoded_path, MAX_ENCODED)?;
            let credentials = decode(&encoded)?;
            if !successor(&credentials, &old.credentials, now) {
                return Ok(None);
            }
            let candidate = Candidate {
                fingerprint: credential_fingerprint(&credentials)
                    .map_err(|_| anyhow::anyhow!("Invalid cswap candidate credentials"))?,
                credentials,
                source: Source {
                    config_path,
                    encoded_path,
                    config,
                    encoded,
                },
            };
            // Compare the complete pair after parsing/capture, not just token or
            // file size/mtime. cswap may replace either file independently.
            candidate.confirm_source(uuid)?;
            Ok(Some(candidate))
        })();
        let Ok(Some(next)) = captured else { continue };
        let replace = freshest.as_ref().is_none_or(|previous| {
            expiry(&next.credentials) > expiry(&previous.credentials)
                || (expiry(&next.credentials) == expiry(&previous.credentials)
                    && next.source.config_path < previous.source.config_path)
        });
        if replace {
            freshest = Some(next);
        }
    }
    Ok(freshest)
}

async fn verify(http: &Http, credentials: &Value, uuid: &str) -> std::result::Result<(), Failure> {
    let token = access(credentials).map_err(|_| Failure::local("Invalid adoption credentials"))?;
    let identity = http.identity(token).await?;
    if response_uuid(&identity).map_err(|_| Failure::local("Malformed adoption account UUID"))?
        != Some(uuid)
    {
        return Err(Failure::local(
            "Adoption token account UUID does not match the saved account",
        ));
    }
    Ok(())
}

pub(super) async fn adopt(
    m: &Manager,
    old: &Snapshot,
    candidate: &Candidate,
    http: &Http,
) -> std::result::Result<Snapshot, Failure> {
    let uuid =
        saved_uuid(old).ok_or_else(|| Failure::local("Saved Claude account UUID is missing"))?;
    candidate
        .confirm_value(uuid)
        .map_err(|_| Failure::local("Invalid cswap adoption candidate"))?;
    if !successor(
        &candidate.credentials,
        &old.credentials,
        Utc::now().timestamp_millis(),
    ) {
        return Err(Failure::local(
            "Cswap adoption candidate is no longer fresh",
        ));
    }
    // No operation/protocol/file locks are held across this GET.
    verify(http, &candidate.credentials, uuid).await?;
    commit_confirmed(m, old, &candidate.credentials, Some(candidate)).await
}

pub(super) async fn adopt_live(
    m: &Manager,
    old: &Snapshot,
    http: &Http,
) -> std::result::Result<Snapshot, Failure> {
    if old.live.is_none() || old.credentials["claudeAiOauth"] == old.saved_oauth {
        return Ok(Snapshot {
            account: old.account.clone(),
            saved_oauth: old.saved_oauth.clone(),
            credentials: old.credentials.clone(),
            live: old.live.clone(),
            receipt: old.receipt.clone(),
        });
    }
    let uuid =
        saved_uuid(old).ok_or_else(|| Failure::local("Saved Claude account UUID is missing"))?;
    verify(http, &old.credentials, uuid).await?;
    commit_confirmed(m, old, &old.credentials, None).await
}

fn remaining(deadline: Instant) -> Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    ensure!(!remaining.is_zero(), "Claude adoption guard budget reached");
    // CredentialsGuard has TWO locks with a per-lock timeout.
    Ok(remaining / 2)
}

fn live_unchanged(m: &Manager, live: &LiveSnapshot) -> Result<bool> {
    Ok(
        raw(&m.live_dir.join(".credentials.json"))?.as_deref() == Some(live.credentials.as_slice())
            && raw(&m.live_config)?.as_deref() == Some(live.config.as_slice()),
    )
}

/// Call only while holding OPERATION and live -> profile CredentialsGuards,
/// after a successful credential CAS/read-back. Preserve any pending rotation
/// that still refers to the durable generation or cannot be safely classified.
/// The parent can reuse this for another guarded external-successor install.
pub(super) fn clear_superseded_pending(m: &Manager, confirmed: &Snapshot) -> Result<()> {
    ensure!(
        saved_uuid(confirmed).is_some(),
        "Saved Claude account UUID is missing"
    );
    let profile = m.profile(&confirmed.account.id)?;
    let paths = [
        profile.join("quota-token-pending.json"),
        profile.join("quota-token-pending.json.bak"),
    ];
    let mut captured = Vec::new();
    for path in paths {
        let Some(bytes) =
            raw(&path).map_err(|_| anyhow::anyhow!("Cannot inspect pending OAuth rotation"))?
        else {
            continue;
        };
        let pending: super::PendingRotation = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("Cannot classify pending OAuth rotation"))?;
        validate_credentials(&pending.credentials)
            .map_err(|_| anyhow::anyhow!("Cannot classify pending OAuth rotation"))?;
        validate_credentials(&serde_json::json!({"claudeAiOauth": pending.previous_oauth}))
            .map_err(|_| anyhow::anyhow!("Cannot classify pending OAuth rotation"))?;
        if pending.account_uuid != confirmed.account.account_uuid
            || pending.previous_oauth == confirmed.saved_oauth
            || pending.credentials["claudeAiOauth"] == confirmed.saved_oauth
        {
            // All existing copies must be superseded before deleting either;
            // never erase a current-generation recovery copy or staged reauth.
            return Ok(());
        }
        captured.push((path, bytes));
    }
    if captured.is_empty() {
        return Ok(());
    }
    let current = snapshot(m, &confirmed.account.id)
        .map_err(|_| anyhow::anyhow!("Cannot recheck pending OAuth rotation generation"))?;
    ensure!(
        current.account.account_uuid == confirmed.account.account_uuid
            && current.saved_oauth == confirmed.saved_oauth,
        "Claude credential generation changed before pending cleanup"
    );
    for (path, bytes) in &captured {
        ensure!(
            raw(path)
                .map_err(|_| anyhow::anyhow!("Cannot recheck pending OAuth rotation"))?
                .as_deref()
                == Some(bytes.as_slice()),
            "Pending OAuth rotation changed before cleanup"
        );
    }
    // Remove recovery first, so a later corrupt primary cannot resurrect it.
    for (path, _) in captured.into_iter().rev() {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => anyhow::bail!("Cannot clear superseded pending OAuth rotation"),
        }
    }
    Ok(())
}

async fn commit_confirmed(
    m: &Manager,
    old: &Snapshot,
    credentials: &Value,
    candidate: Option<&Candidate>,
) -> std::result::Result<Snapshot, Failure> {
    let deadline = Instant::now() + GUARD_BUDGET;
    let operation = timeout_at(deadline, runtime::OPERATION.lock())
        .await
        .map_err(|_| Failure::local("Claude activation is busy; adoption deferred"))?;
    let live_dir = m.live_dir.clone();
    let profile = m
        .profile(&old.account.id)
        .map_err(|_| Failure::local("Cannot locate Claude adoption profile"))?;
    // Move OPERATION into the acquisition worker: a timed-out spawn_blocking
    // task cannot be aborted after starting. It only acquires/releases guards;
    // it can NEVER install credentials after the caller reports a timeout.
    let guards = timeout_at(
        deadline,
        tokio::task::spawn_blocking(move || -> Result<_> {
            let live = CredentialsGuard::acquire_with_timeout(&live_dir, remaining(deadline)?)?;
            let profile = CredentialsGuard::acquire_with_timeout(&profile, remaining(deadline)?)?;
            ensure!(
                Instant::now() < deadline,
                "Claude adoption guard budget reached"
            );
            Ok((operation, live, profile))
        }),
    )
    .await
    .map_err(|_| Failure::local("Claude adoption guard budget reached"))?
    .map_err(|_| Failure::local("Claude adoption guard task failed"))?
    .map_err(|_| Failure::local("Claude credentials are busy; adoption deferred"))?;
    let m = m.clone();
    let id = old.account.id.clone();
    let uuid = old.account.account_uuid.clone();
    let baseline = old.saved_oauth.clone();
    let effective = old.credentials["claudeAiOauth"].clone();
    let original_live = old.live.clone();
    let credentials = credentials.clone();
    let candidate = candidate.cloned();
    // Once a commit starts, await its result: cancelling a blocking file write
    // via timeout would permit an unobserved late commit. The 15s deadline bounds
    // OPERATION + protocol-guard acquisition and is checked again before writes.
    tokio::task::spawn_blocking(move || -> std::result::Result<Snapshot, Failure> {
        let (_operation, _live_guard, _profile_guard) = guards;
        let mut current = snapshot(&m, &id)
            .map_err(|_| Failure::local("Cannot recheck Claude adoption credentials"))?;
        if current.account.account_uuid != uuid {
            return Err(Failure::local(
                "Claude account UUID changed during adoption",
            ));
        }
        // Any concurrent saved/effective OAuth is the winner, including an
        // already-installed exact successor. Never overwrite staged reauth.
        if current.saved_oauth != baseline || current.credentials["claudeAiOauth"] != effective {
            return Ok(current);
        }
        if let Some(candidate) = &candidate {
            candidate
                .confirm_source(uuid.as_deref().unwrap_or_default())
                .map_err(|_| {
                    Failure::local("Cswap backup changed during adoption; retry deferred")
                })?;
            if !successor(
                &credentials,
                &current.credentials,
                Utc::now().timestamp_millis(),
            ) {
                return Err(Failure::local(
                    "Cswap adoption candidate is no longer fresh",
                ));
            }
            // Retain the ORIGINAL live CAS, never recapture permission to
            // overwrite a live file that changed during the identity request.
            current.live = original_live;
        } else {
            let stable = match &original_live {
                Some(live) => live_unchanged(&m, live)
                    .map_err(|_| Failure::local("Cannot recheck live adoption credentials"))?,
                None => false,
            };
            if !stable {
                return Ok(current);
            }
            // Live adoption persists the verified successor to PROFILE ONLY.
            current.live = None;
        }
        if Instant::now() >= deadline {
            return Err(Failure::local("Claude adoption guard budget reached"));
        }
        persist_rotation(&m, &mut current, &credentials)
            .map_err(|_| Failure::local("Claude credential adoption could not be persisted"))?;
        clear_superseded_pending(&m, &current).map_err(|_| {
            Failure::local("Could not clear superseded pending OAuth rotation after adoption")
        })?;
        Ok(current)
    })
    .await
    .map_err(|_| Failure::local("Claude adoption commit task failed"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{
        sync::{
            atomic::{AtomicBool, AtomicU64, Ordering},
            Arc, Mutex,
        },
        thread,
    };

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        root: PathBuf,
        manager: Manager,
        old: Snapshot,
        now: i64,
    }

    impl Fixture {
        fn new() -> Self {
            let now = Utc::now().timestamp_millis();
            let root = std::env::temp_dir().join(format!(
                "switcher-cswap-{}-{now}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(root.join("configs")).unwrap();
            fs::create_dir_all(root.join("credentials")).unwrap();
            let credentials = credentials("synthetic-old", "synthetic-old-refresh", now - 1);
            Self {
                manager: Manager {
                    root: root.join("store"),
                    live_dir: root.join("live"),
                    live_config: root.join("live.json"),
                },
                old: Snapshot {
                    account: super::super::super::ClaudeAccount {
                        id: "00000000-0000-4000-8000-000000000001".into(),
                        name: "synthetic".into(),
                        email: None,
                        account_uuid: Some("synthetic-account".into()),
                        plan_type: None,
                        created_at: String::new(),
                        last_used_at: None,
                    },
                    saved_oauth: credentials["claudeAiOauth"].clone(),
                    credentials,
                    live: None,
                    receipt: None,
                },
                root,
                now,
            }
        }

        fn backup(&self, stem: &str, uuid: &str, credentials: &Value) -> (PathBuf, PathBuf) {
            let config = self
                .root
                .join("configs")
                .join(format!(".claude-config-{stem}.json"));
            let encoded = self
                .root
                .join("credentials")
                .join(format!(".creds-{stem}.enc"));
            fs::write(
                &config,
                serde_json::to_vec(&json!({"oauthAccount":{"accountUuid":uuid}})).unwrap(),
            )
            .unwrap();
            fs::write(
                &encoded,
                STANDARD.encode(serde_json::to_vec(credentials).unwrap()),
            )
            .unwrap();
            (config, encoded)
        }

        fn load(&self) -> Result<Option<Candidate>> {
            candidate_at(&self.manager, &self.old, &self.root)
        }

        fn stored(mut self, active: bool) -> Self {
            // Construct a real Manager store entirely inside this fixture. The
            // parent's sibling tests/Fixture is private; no HOME/auth import is
            // needed to exercise snapshot/persist_rotation through the sidecar.
            let config =
                json!({"oauthAccount":{"accountUuid":"synthetic-account"},"configOnly":"keep"});
            let store = super::super::super::ClaudeStore {
                accounts: vec![self.old.account.clone()],
                ..Default::default()
            };
            put(
                &self.manager.root.join("accounts.json"),
                &serde_json::to_value(store).unwrap(),
            );
            let mut saved = self.old.credentials.clone();
            saved["profileOnly"] = json!("keep-profile");
            put(&self.profile_credentials(), &saved);
            put(&self.profile().join(".claude.json"), &config);
            if active {
                let mut live = self.old.credentials.clone();
                live["liveOnly"] = json!("keep-live");
                put(&self.live_credentials(), &live);
                put(&self.manager.live_config, &config);
            }
            self.old = snapshot(&self.manager, &self.old.account.id).unwrap();
            assert_eq!(self.old.live.is_some(), active);
            self
        }

        fn profile(&self) -> PathBuf {
            self.manager.profile(&self.old.account.id).unwrap()
        }

        fn profile_credentials(&self) -> PathBuf {
            self.profile().join(".credentials.json")
        }

        fn live_credentials(&self) -> PathBuf {
            self.manager.live_dir.join(".credentials.json")
        }

        fn live_successor(&mut self) -> Value {
            let mut live = credentials(
                "synthetic-cli-new",
                "synthetic-cli-refresh",
                self.now + 900_000,
            );
            live["liveOnly"] = json!("keep-cli-live");
            put(&self.live_credentials(), &live);
            self.old = snapshot(&self.manager, &self.old.account.id).unwrap();
            assert!(self.old.live.is_some());
            assert!(self.old.saved_oauth != self.old.credentials["claudeAiOauth"]);
            live
        }

        fn external_successor(&self) -> Candidate {
            let mut value = credentials(
                "synthetic-cswap-new",
                "synthetic-cswap-refresh",
                self.now + 600_000,
            );
            value["backupOnly"] = json!("must-not-replace-profile-metadata");
            self.backup("1-match", "synthetic-account", &value);
            self.load().unwrap().unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let resolved = self.root.canonicalize().unwrap();
            let temporary = std::env::temp_dir().canonicalize().unwrap();
            assert_eq!(resolved.parent(), Some(temporary.as_path()));
            assert!(resolved
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("switcher-cswap-"));
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn put(path: &Path, value: &Value) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
    }

    fn read(path: &Path) -> Value {
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    fn identity() -> Value {
        json!({"account":{"uuid":"synthetic-account"}})
    }

    fn pending_document(f: &Fixture, previous_oauth: &Value) -> Value {
        json!({
            "account_uuid": f.old.account.account_uuid,
            "previous_oauth": previous_oauth,
            "credentials": credentials("synthetic-abandoned", "synthetic-abandoned-refresh", f.now + 600_000),
            "live": null,
        })
    }

    // Record only method/path and a comparison result, never bearer values.
    struct ObservedRequest {
        method: String,
        path: String,
        authorization_matches: bool,
    }

    struct Loopback {
        base: String,
        stop: Arc<AtomicBool>,
        worker: Option<thread::JoinHandle<()>>,
        requests: Arc<Mutex<Vec<ObservedRequest>>>,
    }

    impl Loopback {
        fn new(expected_access: &str, mut respond: impl FnMut() -> Value + Send + 'static) -> Self {
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let base = format!("http://{}", server.server_addr());
            let expected_authorization = format!("Bearer {expected_access}");
            let stop = Arc::new(AtomicBool::new(false));
            let flag = stop.clone();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let recorded = requests.clone();
            let worker = thread::spawn(move || {
                while !flag.load(Ordering::SeqCst) {
                    let Some(request) = server.recv_timeout(Duration::from_millis(30)).unwrap()
                    else {
                        continue;
                    };
                    recorded.lock().unwrap().push(ObservedRequest {
                        method: request.method().as_str().into(),
                        path: request.url().into(),
                        authorization_matches: request.headers().iter().any(|header| {
                            header.field.equiv("Authorization")
                                && header.value.as_str() == expected_authorization.as_str()
                        }),
                    });
                    // Hooks deliberately emulate a non-cooperative file writer
                    // while the identity GET is in progress, before its reply.
                    let response = tiny_http::Response::from_string(respond().to_string())
                        .with_status_code(200);
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
            let url = reqwest::Url::parse(&self.base).unwrap();
            assert!(url
                .host_str()
                .unwrap()
                .parse::<std::net::IpAddr>()
                .unwrap()
                .is_loopback());
            Http {
                client: Http::client(false, Duration::from_secs(2)).unwrap(),
                profile: format!("{}/profile", self.base),
                usage: format!("{}/unused-usage", self.base),
                token: format!("{}/forbidden-refresh", self.base),
            }
        }

        fn assert_profile_gets(&self, count: usize) {
            let requests = self.requests.lock().unwrap();
            assert_eq!(requests.len(), count);
            assert!(requests.iter().all(|request| request.method == "GET"
                && request.path == "/profile"
                && request.authorization_matches));
        }
    }

    impl Drop for Loopback {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(worker) = self.worker.take() {
                worker.join().unwrap();
            }
        }
    }

    fn credentials(access: &str, refresh: &str, expires: i64) -> Value {
        json!({"claudeAiOauth":{"accessToken":access,"refreshToken":refresh,"expiresAt":expires},"preserved":true})
    }

    #[test]
    fn matching_identity_selects_freshest_and_reads_only() {
        let f = Fixture::new();
        f.backup(
            "1-first",
            "synthetic-account",
            &credentials("first", "first-refresh", f.now + 600_000),
        );
        let expected = credentials("freshest", "freshest-refresh", f.now + 900_000);
        let (config, encoded) = f.backup("2-label-with-dashes", "synthetic-account", &expected);
        let before = (fs::read(&config).unwrap(), fs::read(&encoded).unwrap());
        let candidate = f.load().unwrap().unwrap();
        assert!(candidate.credentials == expected);
        assert_eq!(
            candidate.fingerprint,
            credential_fingerprint(&expected).unwrap()
        );
        assert!(fs::read(config).unwrap() == before.0);
        assert!(fs::read(encoded).unwrap() == before.1);
        assert!(!f.manager.root.exists());
        assert!(!f.manager.live_dir.exists());
    }

    #[test]
    fn foreign_or_nonexact_identity_is_ignored() {
        let f = Fixture::new();
        for (slot, uuid) in [
            ("1-foreign", "foreign"),
            ("2-case", "Synthetic-account"),
            ("3-space", "synthetic-account "),
            ("4-empty", ""),
        ] {
            f.backup(
                slot,
                uuid,
                &credentials("new", "new-refresh", f.now + 600_000),
            );
        }
        assert!(f.load().unwrap().is_none());
    }

    #[test]
    fn unknown_or_empty_saved_identity_is_ignored() {
        let mut f = Fixture::new();
        f.backup(
            "1-match",
            "synthetic-account",
            &credentials("new", "new-refresh", f.now + 600_000),
        );
        for uuid in [None, Some(String::new()), Some("  ".into())] {
            f.old.account.account_uuid = uuid;
            assert!(f.load().unwrap().is_none());
        }
    }

    #[test]
    fn good_saved_credentials_do_not_scan_and_test_entry_never_uses_home() {
        let mut f = Fixture::new();
        f.old.credentials["claudeAiOauth"]["expiresAt"] = json!(f.now + 900_000);
        // A regular file at configs would fail read_dir if the loader scanned.
        fs::remove_dir(f.root.join("configs")).unwrap();
        fs::write(f.root.join("configs"), b"must not scan").unwrap();
        assert!(f.load().unwrap().is_none());
        f.old.credentials["claudeAiOauth"]["expiresAt"] = json!(f.now - 1);
        assert!(candidate(&f.manager, &f.old).unwrap().is_none());
    }

    #[test]
    fn stale_or_unchanged_tokens_are_ignored() {
        let f = Fixture::new();
        for (stem, access, refresh, expiry) in [
            ("1-expired", "new", "new-refresh", f.now - 2),
            ("2-margin", "new", "new-refresh", f.now + 300_000),
            (
                "3-unchanged",
                "synthetic-old",
                "synthetic-old-refresh",
                f.now + 600_000,
            ),
            ("4-empty-access", "", "new-refresh", f.now + 600_000),
            ("5-empty-refresh", "new", "", f.now + 600_000),
        ] {
            f.backup(
                stem,
                "synthetic-account",
                &credentials(access, refresh, expiry),
            );
        }
        assert!(f.load().unwrap().is_none());
        assert!(!successor(
            &credentials("a", "r", 600_000),
            &credentials("old", "old-r", 600_000),
            0
        ));
        assert!(!successor(
            &credentials("a", "r", 300_000),
            &credentials("old", "old-r", -1),
            0
        ));
    }

    #[test]
    fn malformed_and_oversized_backups_do_not_hide_valid_candidate() {
        let f = Fixture::new();
        let value = credentials("synthetic-new", "synthetic-new-refresh", f.now + 900_000);
        let (_, encoded) = f.backup("1-bad-base64", "synthetic-account", &value);
        fs::write(encoded, b"secret-fixture-invalid-base64").unwrap();
        let (_, encoded) = f.backup("2-bad-utf8", "synthetic-account", &value);
        fs::write(encoded, STANDARD.encode([0xff, 0xfe])).unwrap();
        let (_, encoded) = f.backup("3-bad-json", "synthetic-account", &value);
        fs::write(encoded, STANDARD.encode(b"{secret-fixture-malformed")).unwrap();
        let (config, _) = f.backup("4-config-size", "synthetic-account", &value);
        fs::write(config, vec![b' '; MAX_JSON + 1]).unwrap();
        let (_, encoded) = f.backup("5-encoded-size", "synthetic-account", &value);
        fs::write(encoded, vec![b'A'; MAX_ENCODED + 1]).unwrap();
        let (_, encoded) = f.backup("6-decoded-size", "synthetic-account", &value);
        fs::write(encoded, STANDARD.encode(vec![b' '; MAX_JSON + 1])).unwrap();
        let (_, encoded) = f.backup("7-invalid-credentials", "synthetic-account", &value);
        fs::write(encoded, STANDARD.encode(b"{}")).unwrap();
        assert!(f.load().unwrap().is_none());
        f.backup("8-valid", "synthetic-account", &value);
        assert!(f.load().unwrap().unwrap().credentials == value);
    }

    #[test]
    fn complete_source_bytes_are_rechecked_even_if_oauth_is_unchanged() {
        for change_config in [true, false] {
            let f = Fixture::new();
            let value = credentials("new", "new-refresh", f.now + 600_000);
            let (config, encoded) = f.backup("1-match", "synthetic-account", &value);
            let candidate = f.load().unwrap().unwrap();
            candidate.confirm_source("synthetic-account").unwrap();
            let path = if change_config { config } else { encoded };
            let mut bytes = fs::read(&path).unwrap();
            bytes.push(b'\n');
            fs::write(path, bytes).unwrap();
            assert!(candidate.confirm_source("synthetic-account").is_err());
        }
    }

    #[test]
    fn a_successor_can_retain_either_nonempty_token() {
        for (access, refresh) in [
            ("new-access", "synthetic-old-refresh"),
            ("synthetic-old", "new-refresh"),
        ] {
            let f = Fixture::new();
            let expected = credentials(access, refresh, f.now + 600_000);
            f.backup("1-one-token-changed", "synthetic-account", &expected);
            assert!(f.load().unwrap().unwrap().credentials == expected);
        }
    }

    #[tokio::test]
    async fn same_uuid_identity_get_adopts_without_refresh_or_metadata_replacement() {
        for active in [false, true] {
            let f = Fixture::new().stored(active);
            let candidate = f.external_successor();
            let accounts_before = fs::read(f.manager.root.join("accounts.json")).unwrap();
            let config_before = fs::read(f.profile().join(".claude.json")).unwrap();
            let profile = f.profile_credentials();
            let profile_before = fs::read(&profile).unwrap();
            let live_dir = f.manager.live_dir.clone();
            let profile_dir = f.profile();
            let server = Loopback::new("synthetic-cswap-new", move || {
                // Identity verification happens before any adoption write or
                // protocol guard; only this fixture's paths are inspected.
                assert!(fs::read(&profile).unwrap() == profile_before);
                assert!(!live_dir.join(".oauth_refresh.lock").exists());
                assert!(!profile_dir.join(".oauth_refresh.lock").exists());
                identity()
            });
            let result = adopt(&f.manager, &f.old, &candidate, &server.http())
                .await
                .unwrap();
            assert!(result.saved_oauth == candidate.credentials["claudeAiOauth"]);
            let saved = read(&f.profile_credentials());
            assert!(saved["claudeAiOauth"] == candidate.credentials["claudeAiOauth"]);
            assert_eq!(saved["profileOnly"], "keep-profile");
            assert!(saved.get("backupOnly").is_none());
            assert!(fs::read(f.manager.root.join("accounts.json")).unwrap() == accounts_before);
            assert!(fs::read(f.profile().join(".claude.json")).unwrap() == config_before);
            if active {
                let live = read(&f.live_credentials());
                assert!(live["claudeAiOauth"] == candidate.credentials["claudeAiOauth"]);
                assert_eq!(live["liveOnly"], "keep-live");
                assert!(live.get("backupOnly").is_none());
            } else {
                assert!(!f.live_credentials().exists());
            }
            server.assert_profile_gets(1);
        }
    }

    #[tokio::test]
    async fn changed_access_with_stable_refresh_is_server_verified_and_adopted() {
        let f = Fixture::new().stored(false);
        let value = credentials(
            "synthetic-new-access-only",
            "synthetic-old-refresh",
            f.now + 600_000,
        );
        f.backup("1-stable-refresh", "synthetic-account", &value);
        let candidate = f.load().unwrap().unwrap();
        let server = Loopback::new("synthetic-new-access-only", identity);
        let result = adopt(&f.manager, &f.old, &candidate, &server.http())
            .await
            .unwrap();
        assert!(result.saved_oauth == value["claudeAiOauth"]);
        assert!(read(&f.profile_credentials())["claudeAiOauth"] == value["claudeAiOauth"]);
        server.assert_profile_gets(1);
    }

    #[tokio::test]
    async fn foreign_server_uuid_cannot_write_profile_or_live_credentials() {
        let f = Fixture::new().stored(true);
        let candidate = f.external_successor();
        let pending = f.profile().join("quota-token-pending.json");
        let backup = f.profile().join("quota-token-pending.json.bak");
        let document = pending_document(&f, &f.old.saved_oauth);
        put(&pending, &document);
        put(&backup, &document);
        let pending_before = fs::read(&pending).unwrap();
        let backup_before = fs::read(&backup).unwrap();
        let profile_before = fs::read(f.profile_credentials()).unwrap();
        let live_before = fs::read(f.live_credentials()).unwrap();
        let config_before = fs::read(&f.manager.live_config).unwrap();
        let server = Loopback::new(
            "synthetic-cswap-new",
            || json!({"account":{"uuid":"foreign-account"}}),
        );
        let error = adopt(&f.manager, &f.old, &candidate, &server.http())
            .await
            .err()
            .unwrap();
        assert!(error.message.contains("UUID"));
        assert!(fs::read(f.profile_credentials()).unwrap() == profile_before);
        assert!(fs::read(f.live_credentials()).unwrap() == live_before);
        assert!(fs::read(&f.manager.live_config).unwrap() == config_before);
        assert!(fs::read(pending).unwrap() == pending_before);
        assert!(fs::read(backup).unwrap() == backup_before);
        assert!(!f.profile().join(".credentials.json.bak").exists());
        server.assert_profile_gets(1);
    }

    #[tokio::test]
    async fn cswap_source_drift_during_identity_get_prevents_all_credential_writes() {
        for change_config in [true, false] {
            let f = Fixture::new().stored(true);
            let candidate = f.external_successor();
            let profile_before = fs::read(f.profile_credentials()).unwrap();
            let live_before = fs::read(f.live_credentials()).unwrap();
            let changed = if change_config {
                candidate.source.config_path.clone()
            } else {
                candidate.source.encoded_path.clone()
            };
            let server = Loopback::new("synthetic-cswap-new", move || {
                let mut bytes = fs::read(&changed).unwrap();
                // Still decodes to the same JSON: the full source must match.
                bytes.push(b'\n');
                fs::write(&changed, bytes).unwrap();
                identity()
            });
            let error = adopt(&f.manager, &f.old, &candidate, &server.http())
                .await
                .err()
                .unwrap();
            assert!(error.message.contains("backup changed"));
            assert!(fs::read(f.profile_credentials()).unwrap() == profile_before);
            assert!(fs::read(f.live_credentials()).unwrap() == live_before);
            assert!(!f.profile().join(".credentials.json.bak").exists());
            server.assert_profile_gets(1);
        }
    }

    #[tokio::test]
    async fn concurrent_profile_repair_or_already_installed_candidate_wins_adoption() {
        for exact_candidate in [false, true] {
            let f = Fixture::new().stored(true);
            let candidate = f.external_successor();
            let mut winner = if exact_candidate {
                candidate.credentials.clone()
            } else {
                credentials(
                    "synthetic-repaired",
                    "synthetic-repaired-refresh",
                    f.now + 1_200_000,
                )
            };
            winner["profileOnly"] = json!("concurrent-repair-metadata");
            let installed = winner.clone();
            let profile = f.profile_credentials();
            let live_before = fs::read(f.live_credentials()).unwrap();
            let server = Loopback::new("synthetic-cswap-new", move || {
                put(&profile, &installed);
                identity()
            });
            let result = adopt(&f.manager, &f.old, &candidate, &server.http())
                .await
                .unwrap();
            assert!(result.saved_oauth == winner["claudeAiOauth"]);
            assert!(read(&f.profile_credentials()) == winner);
            assert!(fs::read(f.live_credentials()).unwrap() == live_before);
            assert!(!f.profile().join(".credentials.json.bak").exists());
            server.assert_profile_gets(1);
        }
    }

    #[tokio::test]
    async fn concurrent_live_cli_successor_wins_cswap_adoption() {
        let f = Fixture::new().stored(true);
        let candidate = f.external_successor();
        let profile_before = fs::read(f.profile_credentials()).unwrap();
        let winner = credentials(
            "synthetic-cli-winner",
            "synthetic-cli-winner-refresh",
            f.now + 1_200_000,
        );
        let installed = winner.clone();
        let live = f.live_credentials();
        let server = Loopback::new("synthetic-cswap-new", move || {
            put(&live, &installed);
            identity()
        });
        let result = adopt(&f.manager, &f.old, &candidate, &server.http())
            .await
            .unwrap();
        assert!(result.credentials["claudeAiOauth"] == winner["claudeAiOauth"]);
        assert!(fs::read(f.profile_credentials()).unwrap() == profile_before);
        assert!(read(&f.live_credentials()) == winner);
        server.assert_profile_gets(1);
    }

    #[tokio::test]
    async fn live_successor_is_verified_and_persisted_only_to_profile_once() {
        let mut f = Fixture::new().stored(true);
        let successor = f.live_successor();
        let live_before = fs::read(f.live_credentials()).unwrap();
        let config_before = fs::read(&f.manager.live_config).unwrap();
        let server = Loopback::new("synthetic-cli-new", identity);
        let result = adopt_live(&f.manager, &f.old, &server.http())
            .await
            .unwrap();
        assert!(result.saved_oauth == successor["claudeAiOauth"]);
        let saved = read(&f.profile_credentials());
        assert!(saved["claudeAiOauth"] == successor["claudeAiOauth"]);
        assert_eq!(saved["profileOnly"], "keep-profile");
        assert!(saved.get("liveOnly").is_none());
        assert!(fs::read(f.live_credentials()).unwrap() == live_before);
        assert!(fs::read(&f.manager.live_config).unwrap() == config_before);
        assert!(!f.manager.live_dir.join(".credentials.json.bak").exists());
        let persisted = snapshot(&f.manager, &f.old.account.id).unwrap();
        let unchanged = adopt_live(&f.manager, &persisted, &server.http())
            .await
            .unwrap();
        assert!(unchanged.saved_oauth == result.saved_oauth);
        server.assert_profile_gets(1);
    }

    #[tokio::test]
    async fn live_adoption_with_foreign_server_identity_cannot_change_saved_or_live_bytes() {
        let mut f = Fixture::new().stored(true);
        f.live_successor();
        let profile_before = fs::read(f.profile_credentials()).unwrap();
        let live_before = fs::read(f.live_credentials()).unwrap();
        let server = Loopback::new(
            "synthetic-cli-new",
            || json!({"account":{"uuid":"foreign-account"}}),
        );
        let error = adopt_live(&f.manager, &f.old, &server.http())
            .await
            .err()
            .unwrap();
        assert!(error.message.contains("UUID"));
        assert!(fs::read(f.profile_credentials()).unwrap() == profile_before);
        assert!(fs::read(f.live_credentials()).unwrap() == live_before);
        server.assert_profile_gets(1);
    }

    #[tokio::test]
    async fn changed_complete_live_files_prevent_stale_profile_adoption() {
        for change_config in [false, true] {
            let mut f = Fixture::new().stored(true);
            f.live_successor();
            let profile_before = fs::read(f.profile_credentials()).unwrap();
            let changed = if change_config {
                f.manager.live_config.clone()
            } else {
                f.live_credentials()
            };
            let mut expected = fs::read(&changed).unwrap();
            expected.push(b'\n');
            let written = expected.clone();
            let path = changed.clone();
            let server = Loopback::new("synthetic-cli-new", move || {
                fs::write(&path, &written).unwrap();
                identity()
            });
            let result = adopt_live(&f.manager, &f.old, &server.http())
                .await
                .unwrap();
            assert!(result.saved_oauth == f.old.saved_oauth);
            assert!(fs::read(f.profile_credentials()).unwrap() == profile_before);
            assert!(fs::read(changed).unwrap() == expected);
            assert!(!f.profile().join(".credentials.json.bak").exists());
            server.assert_profile_gets(1);
        }
    }

    #[tokio::test]
    async fn live_adoption_preserves_concurrent_reauth_and_exact_durable_successor_metadata() {
        for exact_successor in [false, true] {
            let mut f = Fixture::new().stored(true);
            let successor = f.live_successor();
            let mut winner = if exact_successor {
                successor
            } else {
                credentials(
                    "synthetic-reauth-winner",
                    "synthetic-reauth-refresh",
                    f.now + 1_200_000,
                )
            };
            winner["profileOnly"] = json!("new-reauth-metadata");
            let installed = winner.clone();
            let profile = f.profile_credentials();
            let live_before = fs::read(f.live_credentials()).unwrap();
            let server = Loopback::new("synthetic-cli-new", move || {
                put(&profile, &installed);
                identity()
            });
            let result = adopt_live(&f.manager, &f.old, &server.http())
                .await
                .unwrap();
            assert!(result.saved_oauth == winner["claudeAiOauth"]);
            assert!(read(&f.profile_credentials()) == winner);
            assert!(fs::read(f.live_credentials()).unwrap() == live_before);
            assert!(!f.profile().join(".credentials.json.bak").exists());
            server.assert_profile_gets(1);
        }
    }

    #[tokio::test]
    async fn successful_external_and_live_adoption_clear_superseded_pending_and_recovery() {
        for live_adoption in [false, true] {
            let mut f = Fixture::new().stored(true);
            let candidate = if live_adoption {
                f.live_successor();
                None
            } else {
                Some(f.external_successor())
            };
            let pending = f.profile().join("quota-token-pending.json");
            let recovery = f.profile().join("quota-token-pending.json.bak");
            let document = pending_document(&f, &f.old.saved_oauth);
            put(&pending, &document);
            put(&recovery, &document);
            let expected_access = if live_adoption {
                "synthetic-cli-new"
            } else {
                "synthetic-cswap-new"
            };
            let server = Loopback::new(expected_access, identity);
            let result = if let Some(candidate) = candidate {
                adopt(&f.manager, &f.old, &candidate, &server.http())
                    .await
                    .unwrap()
            } else {
                adopt_live(&f.manager, &f.old, &server.http())
                    .await
                    .unwrap()
            };
            assert!(read(&f.profile_credentials())["claudeAiOauth"] == result.saved_oauth);
            assert!(!pending.exists());
            assert!(!recovery.exists());
            server.assert_profile_gets(1);
        }
    }

    #[tokio::test]
    async fn adoption_preserves_pending_pair_if_either_copy_belongs_to_durable_generation() {
        for current_primary in [false, true] {
            let f = Fixture::new().stored(false);
            let candidate = f.external_successor();
            let superseded = pending_document(&f, &f.old.saved_oauth);
            let current = pending_document(&f, &candidate.credentials["claudeAiOauth"]);
            let pending = f.profile().join("quota-token-pending.json");
            let recovery = f.profile().join("quota-token-pending.json.bak");
            put(
                &pending,
                if current_primary {
                    &current
                } else {
                    &superseded
                },
            );
            put(
                &recovery,
                if current_primary {
                    &superseded
                } else {
                    &current
                },
            );
            let pending_before = fs::read(&pending).unwrap();
            let recovery_before = fs::read(&recovery).unwrap();
            let server = Loopback::new("synthetic-cswap-new", identity);
            let result = adopt(&f.manager, &f.old, &candidate, &server.http())
                .await
                .unwrap();
            assert!(result.saved_oauth == candidate.credentials["claudeAiOauth"]);
            assert!(fs::read(pending).unwrap() == pending_before);
            assert!(fs::read(recovery).unwrap() == recovery_before);
            server.assert_profile_gets(1);
        }
    }

    #[test]
    fn parse_failures_never_include_secret_input() {
        let marker = "synthetic-secret-do-not-log";
        for bytes in [
            marker.as_bytes().to_vec(),
            STANDARD.encode(format!("{{{marker}")).into_bytes(),
        ] {
            let error = decode(&bytes).err().unwrap().to_string();
            assert!(!error.contains(marker));
        }
        let error = json(format!("{{{marker}").as_bytes())
            .unwrap_err()
            .to_string();
        assert!(!error.contains(marker));
    }
}
