use super::*;
use crate::auth::persistence::{atomic_write, lock_file, read_json};
use anyhow::{ensure, Context, Result};
use chrono::Utc;
use serde_json::{json, Value};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Clone)]
pub struct Manager {
    pub root: PathBuf,
    pub live_dir: PathBuf,
    pub live_config: PathBuf,
}

/// The exact credential generation selected before an official CLI login starts.
/// Never use the broad import/add identity matching for an account repair.
pub(super) struct ReloginTarget {
    pub id: String,
    account_uuid: String,
    previous_oauth: Value,
}

fn config_uuid(config: &Value) -> Option<&str> {
    config
        .pointer("/oauthAccount/accountUuid")
        .and_then(Value::as_str)
        .filter(|uuid| !uuid.trim().is_empty())
}

/// A repair must fail closed on corrupt/missing identity or credential files.
/// The normal recovery reader can rewrite a .bak over a concurrent winner.
fn repair_object_file(path: &Path) -> Result<Value> {
    strict_object_file(path, 8 * 1024 * 1024)?
        .map(|(_, value)| value)
        .context("Claude repair file is missing")
}

// Observe only the primary, with a bounded read even if a writer grows it.
// Never use the recovering reader for credential/configuration observation.
fn strict_object_file(path: &Path, limit: u64) -> Result<Option<(Vec<u8>, Value)>> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= limit,
        "Invalid or oversized Claude JSON file"
    );
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= limit, "Claude JSON file is too large");
    let value: Value = serde_json::from_slice(&bytes)
        .context("Invalid Claude JSON; refusing backup restoration")?;
    ensure!(value.is_object(), "Claude JSON must be an object");
    Ok(Some((bytes, value)))
}

fn relogin_barrier(profile: &Path, uuid: Option<&str>) -> Result<Option<Vec<u8>>> {
    let path = profile.join("relogin-barrier.json");
    let Some((bytes, marker)) = strict_object_file(&path, 4096)? else {
        ensure!(
            !profile.join("relogin-barrier.json.bak").try_exists()?,
            "Claude relogin barrier primary is missing; refusing live credentials"
        );
        return Ok(None);
    };
    ensure!(
        marker["version"] == 1,
        "Invalid Claude relogin barrier version"
    );
    let expected = uuid
        .filter(|uuid| !uuid.trim().is_empty())
        .context("Claude relogin barrier requires an account UUID")?;
    ensure!(
        marker["account_uuid"].as_str() == Some(expected),
        "Claude relogin barrier UUID mismatch"
    );
    let generation = marker["generation"]
        .as_str()
        .context("Claude relogin barrier generation is missing")?;
    uuid::Uuid::parse_str(generation).context("Invalid Claude relogin barrier generation")?;
    Ok(Some(bytes))
}

fn clear_relogin_barrier(profile: &Path, uuid: Option<&str>, expected: &[u8]) -> Result<()> {
    ensure!(
        relogin_barrier(profile, uuid)?.as_deref() == Some(expected),
        "Claude relogin barrier changed during activation"
    );
    // Backup first: any cleanup failure must leave the primary barrier.
    for name in ["relogin-barrier.json.bak", "relogin-barrier.json"] {
        match fs::remove_file(profile.join(name)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .context("Claude activation succeeded; relogin barrier cleanup failed")
            }
        }
    }
    Ok(())
}

pub fn user_home() -> Result<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .context("Home directory is unavailable")
}

fn object_file(path: &Path) -> Result<Value> {
    Ok(strict_object_file(path, 8 * 1024 * 1024)?
        .map(|(_, value)| value)
        .unwrap_or_else(|| json!({})))
}
pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    if fs::read(path).ok().as_deref() != Some(bytes.as_slice()) {
        atomic_write(path, &bytes)?;
    }
    Ok(())
}
pub fn validate_credentials(value: &Value) -> Result<()> {
    let oauth = value
        .get("claudeAiOauth")
        .context("A Claude Code subscription login is required")?;
    ensure!(
        oauth
            .get("accessToken")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty()),
        "Claude access token is missing"
    );
    ensure!(
        oauth
            .get("refreshToken")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty()),
        "Claude refresh token is missing"
    );
    ensure!(
        oauth.get("expiresAt").and_then(Value::as_i64).is_some(),
        "Claude token expiry is missing"
    );
    Ok(())
}
pub fn same_identity(
    account: &ClaudeAccount,
    config: &Value,
    credentials: &Value,
    saved: &Value,
) -> bool {
    if let (Some(expected), Some(live)) = (
        &account.account_uuid,
        config
            .pointer("/oauthAccount/accountUuid")
            .and_then(Value::as_str),
    ) {
        return expected == live;
    }
    let a = credentials
        .pointer("/claudeAiOauth/accessToken")
        .and_then(Value::as_str);
    let b = saved
        .pointer("/claudeAiOauth/accessToken")
        .and_then(Value::as_str);
    a.is_some() && a == b
}

impl Manager {
    pub fn current() -> Result<Self> {
        let home = user_home()?;
        let live_dir = std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".claude"));
        let live_config = if std::env::var_os("CLAUDE_CONFIG_DIR").is_some() {
            live_dir.join(".claude.json")
        } else {
            home.join(".claude.json")
        };
        Ok(Self {
            root: crate::auth::get_config_dir()?.join("claude"),
            live_dir,
            live_config,
        })
    }
    pub fn profile(&self, id: &str) -> Result<PathBuf> {
        let uuid = uuid::Uuid::parse_str(id).context("Invalid Claude account ID")?;
        ensure!(uuid.to_string() == id, "Invalid Claude account ID");
        Ok(self.root.join("profiles").join(id))
    }
    /// Safe inside an accounts transaction: no store reads or nested locks.
    /// Protect the entire independent login lineage, including CLI rotations.
    pub(super) fn has_relogin_barrier(&self, id: &str, uuid: Option<&str>) -> Result<bool> {
        Ok(relogin_barrier(&self.profile(id)?, uuid)?.is_some())
    }
    pub fn transaction<T>(
        &self,
        operation: impl FnOnce(&mut ClaudeStore) -> Result<T>,
    ) -> Result<T> {
        let _lock = lock_file(&self.root.join("accounts.lock"))?;
        let path = self.root.join("accounts.json");
        let mut store = read_json::<ClaudeStore>(&path)?.unwrap_or_default();
        ensure!(
            store.provider == "claude" && store.version == 1,
            "Unsupported Claude account store"
        );
        let before = serde_json::to_vec(&store)?;
        let result = operation(&mut store)?;
        if before != serde_json::to_vec(&store)? {
            write_json(&path, &store)?;
        }
        Ok(result)
    }
    pub fn store(&self) -> Result<ClaudeStore> {
        self.transaction(|s| Ok(s.clone()))
    }
    pub fn account(&self, id: &str) -> Result<ClaudeAccount> {
        self.profile(id)?;
        self.store()?
            .accounts
            .into_iter()
            .find(|a| a.id == id)
            .context("Claude account not found")
    }
    // Async callers hold runtime::OPERATION and run these blocking methods on
    // spawn_blocking. Profile protocol guards precede all account/file locks.
    pub(super) fn relogin_target(&self, id: &str) -> Result<ReloginTarget> {
        let profile = self.profile(id)?;
        let _guard = super::locks::CredentialsGuard::acquire(&profile)?;
        self.transaction(|store| {
            let account = store
                .accounts
                .iter()
                .find(|a| a.id == id)
                .context("Claude account not found")?;
            let uuid = account
                .account_uuid
                .as_deref()
                .filter(|uuid| !uuid.trim().is_empty())
                .context(
                    "Claude account UUID is missing; cannot safely reauthenticate this account",
                )?;
            let _lock = lock_file(&profile.join("credentials.lock"))?;
            ensure!(
                config_uuid(&repair_object_file(&profile.join(".claude.json"))?) == Some(uuid),
                "Claude profile UUID does not match the selected account"
            );
            let previous = repair_object_file(&profile.join(".credentials.json"))?;
            let oauth = previous
                .get("claudeAiOauth")
                .filter(|oauth| oauth.is_object())
                .context("Claude OAuth credential object is missing")?;
            Ok(ReloginTarget {
                id: id.to_owned(),
                account_uuid: uuid.to_owned(),
                previous_oauth: oauth.clone(),
            })
        })
    }
    pub(super) fn commit_relogin(
        &self,
        target: &ReloginTarget,
        credentials: &Value,
        config: &Value,
    ) -> Result<ClaudeAccount> {
        validate_credentials(credentials)?;
        ensure!(
            config_uuid(config) == Some(target.account_uuid.as_str()),
            "Claude login UUID does not match the selected account; sign in to the same account"
        );
        ensure!(
            credentials["claudeAiOauth"]["accessToken"] != target.previous_oauth["accessToken"]
                || credentials["claudeAiOauth"]["refreshToken"]
                    != target.previous_oauth["refreshToken"],
            "Claude login returned unchanged credentials; retry the official login"
        );
        let profile = self.profile(&target.id)?;
        let _guard = super::locks::CredentialsGuard::acquire(&profile)?;
        // Same order as quota completion: usage -> accounts -> credentials -> poll.
        // Invalidate old HTTP reservations while usage.lock excludes finishers.
        let _usage = lock_file(&profile.join("usage.lock"))?;
        self.transaction(|store| {
            let account = store
                .accounts
                .iter()
                .find(|a| a.id == target.id)
                .context("Claude account was removed during login")?;
            ensure!(
                account.account_uuid.as_deref() == Some(target.account_uuid.as_str()),
                "Claude account UUID changed during login"
            );
            let _credentials = lock_file(&profile.join("credentials.lock"))?;
            ensure!(
                config_uuid(&repair_object_file(&profile.join(".claude.json"))?)
                    == Some(target.account_uuid.as_str()),
                "Claude profile UUID changed during login"
            );
            let path = profile.join(".credentials.json");
            let mut saved = repair_object_file(&path)?;
            ensure!(
                saved.get("claudeAiOauth") == Some(&target.previous_oauth),
                "Claude credentials changed during login; preserving the concurrent update"
            );
            let mut receipt: Option<UsageReceipt> = read_json(&profile.join("usage.json"))?;
            if let Some(receipt) = &receipt {
                ensure!(
                    receipt.account_id == target.id && receipt.usage.account_id == target.id,
                    "Claude receipt belongs to another account"
                );
            }
            // Preserve all unrelated credential keys, account metadata, config,
            // settings and sessions. In particular, never touch live auth here.
            saved["claudeAiOauth"] = credentials["claudeAiOauth"].clone();
            write_json(&path, &saved)?;
            ensure!(
                repair_object_file(&path)?["claudeAiOauth"] == credentials["claudeAiOauth"],
                "Claude credential read-back changed after reauthentication"
            );
            let barrier = json!({
                "version":1,"account_uuid":target.account_uuid,
                "generation":uuid::Uuid::new_v4().to_string()
            });
            let barrier_path = profile.join("relogin-barrier.json");
            write_json(&barrier_path, &barrier)
                .context("Claude credentials committed; relogin barrier write failed")?;
            ensure!(
                relogin_barrier(&profile, Some(&target.account_uuid))?
                    == Some(serde_json::to_vec_pretty(&barrier)?),
                "Claude relogin barrier read-back changed"
            );
            let _poll = lock_file(&profile.join("quota-poll.lock"))?;
            // A prior attempt ID must never restore an old invalid_grant after
            // repair. Missing generation is intentional: reserve fingerprints
            // the committed token pair before issuing the next normal poll.
            write_json(
                &profile.join("quota-poll.json"),
                &json!({
                    "account_uuid":target.account_uuid,
                    "last_attempt_at":0,"next_poll_at":0,"last_success_at":null,
                    "failures":0,"last_error":null,"retry_after_seconds":null,
                    "attempt_id":null,"in_flight_until":0,
                    "credential_generation":null,"rejected_credential_generation":null,
                    "authentication_required":false
                }),
            )
            .context("Claude credentials committed; quota reservation cleanup failed")?;
            // Never recover the invalidated reservation from its old backup.
            // The protocol guard also owns any pending rotating grant: its old
            // successor must not be replayed after this independent CLI login.
            for name in [
                "quota-poll.json.bak",
                "quota-token-pending.json",
                "quota-token-pending.json.bak",
            ] {
                match fs::remove_file(profile.join(name)) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error)
                            .context("Claude credentials committed; pending token cleanup failed")
                    }
                }
            }
            if let Some(receipt) = &mut receipt {
                receipt.usage.error = Some(
                    "Claude quota is stale after reauthentication; waiting for a normal poll"
                        .into(),
                );
                write_json(&profile.join("usage.json"), receipt)
                    .context("Claude credentials committed; quota receipt cleanup failed")?;
            }
            Ok(account.clone())
        })
    }
    pub fn settings(&self) -> Result<ClaudeSettings> {
        Ok(read_json::<ClaudeSettings>(&self.root.join("settings.json"))?.unwrap_or_default())
    }
    fn validate_settings(settings: &ClaudeSettings) -> Result<()> {
        ensure!(
            settings.auto_switch_threshold_percent.is_finite()
                && (1.0..=100.0).contains(&settings.auto_switch_threshold_percent),
            "Threshold must be between 1 and 100"
        );
        ensure!(
            ["light", "dark", "system"].contains(&settings.theme.as_str()),
            "Invalid Claude theme"
        );
        ensure!(
            ["reset", "usage", "name"].contains(&settings.sort_by.as_str()),
            "Invalid account sort order"
        );
        ensure!(
            ["sonnet", "opus", "haiku"].contains(&settings.model.as_str()),
            "Invalid Claude model"
        );
        for time in &settings.timed_warmup_times {
            let parts: Vec<_> = time.split(':').collect();
            ensure!(
                parts.len() == 2
                    && parts[0].len() == 2
                    && parts[1].len() == 2
                    && parts[0].parse::<u32>().is_ok_and(|n| n < 24)
                    && parts[1].parse::<u32>().is_ok_and(|n| n < 60),
                "Warmup time must be HH:MM"
            );
        }
        ensure!(
            !matches!(
                settings.auto_switch_plan_preference,
                PlanPreference::Plus | PlanPreference::Education | PlanPreference::Free
            ),
            "This plan preference is not supported for Claude Code"
        );
        Ok(())
    }
    pub fn save_settings(&self, settings: &ClaudeSettings) -> Result<()> {
        Self::validate_settings(settings)?;
        let _lock = lock_file(&self.root.join("settings.lock"))?;
        write_json(&self.root.join("settings.json"), settings)
    }
    pub fn active_id(&self, store: &ClaudeStore) -> Result<Option<String>> {
        let live = object_file(&self.live_dir.join(".credentials.json"))?;
        let config = object_file(&self.live_config)?;
        // A config splice can lag a credential swap. Never attribute tokens
        // known to another stored account to the UUID left in .claude.json.
        let token_owner = store
            .accounts
            .iter()
            .filter_map(|account| {
                let saved =
                    object_file(&self.profile(&account.id).ok()?.join(".credentials.json")).ok()?;
                (live
                    .pointer("/claudeAiOauth/accessToken")
                    .and_then(Value::as_str)
                    .is_some()
                    && live["claudeAiOauth"]["accessToken"]
                        == saved["claudeAiOauth"]["accessToken"])
                    .then(|| account.id.clone())
            })
            .collect::<Vec<_>>();
        let mut matches = vec![];
        for account in &store.accounts {
            if !token_owner.is_empty() && !token_owner.contains(&account.id) {
                continue;
            }
            let saved = object_file(&self.profile(&account.id)?.join(".credentials.json"))?;
            if same_identity(account, &config, &live, &saved) {
                matches.push(account.id.clone());
            }
        }
        Ok((matches.len() == 1).then(|| matches.remove(0)))
    }
    /// Keep refresh tokens rotated by the real CLI before the next account switch.
    pub fn sync_active(&self) -> Result<Option<String>> {
        let _live_guard = super::locks::CredentialsGuard::acquire(&self.live_dir)?;
        self.sync_active_locked()
    }
    fn sync_active_locked(&self) -> Result<Option<String>> {
        let store = self.store()?;
        let active = self.active_id(&store)?;
        if let Some(id) = &active {
            let profile = self.profile(id)?;
            let _profile_guard = super::locks::CredentialsGuard::acquire(&profile)?;
            let account = store
                .accounts
                .iter()
                .find(|account| &account.id == id)
                .context("Claude active account disappeared")?;
            if self.has_relogin_barrier(id, account.account_uuid.as_deref())? {
                return Ok(active);
            }
            let live = object_file(&self.live_dir.join(".credentials.json"))?;
            let _lock = lock_file(&profile.join("credentials.lock"))?;
            let mut saved = object_file(&profile.join(".credentials.json"))?;
            // A concurrently running isolated profile may have refreshed first.
            // Never roll its rotating refresh token back to an older live copy.
            let live_expiry = live
                .pointer("/claudeAiOauth/expiresAt")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let saved_expiry = saved
                .pointer("/claudeAiOauth/expiresAt")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            if live_expiry >= saved_expiry {
                saved["claudeAiOauth"] = live["claudeAiOauth"].clone();
            }
            write_json(&profile.join(".credentials.json"), &saved)?;
            let mut config = object_file(&profile.join(".claude.json"))?;
            config["oauthAccount"] = object_file(&self.live_config)?["oauthAccount"].clone();
            write_json(&profile.join(".claude.json"), &config)?;
        }
        Ok(active)
    }
    pub fn import_current(&self, name: &str) -> Result<ClaudeAccount> {
        let credentials = object_file(&self.live_dir.join(".credentials.json"))?;
        let config = object_file(&self.live_config)?;
        let mut settings = object_file(&self.live_dir.join("settings.json"))?;
        // A new login may share the live directory's previous account bridge.
        if settings["statusLine"]["command"]
            .as_str()
            .is_some_and(|s| s.contains("--claude-statusline"))
        {
            let original = object_file(&self.live_dir.join(".switcher-original-statusline.json"))?;
            if original["statusLine"].is_null() {
                settings.as_object_mut().unwrap().remove("statusLine");
            } else {
                settings["statusLine"] = original["statusLine"].clone();
            }
        }
        self.add(name, &credentials, &config, &settings)
    }
    pub fn add(
        &self,
        name: &str,
        credentials: &Value,
        config: &Value,
        settings: &Value,
    ) -> Result<ClaudeAccount> {
        validate_credentials(credentials)?;
        ensure!(
            config.is_object() && settings.is_object(),
            "Claude configuration and settings must be objects"
        );
        ensure!(
            !name.trim().is_empty() && name.chars().count() <= 100,
            "Account name must be 1 to 100 characters"
        );
        let account_uuid = config
            .pointer("/oauthAccount/accountUuid")
            .and_then(Value::as_str)
            .map(str::to_owned);
        self.transaction(|store| {
            let mut existing = None;
            for a in &store.accounts {
                if (account_uuid.is_some() && a.account_uuid == account_uuid)
                    || (account_uuid.is_none() && same_identity(a, config, credentials, &object_file(&self.profile(&a.id)?.join(".credentials.json"))?)) {
                    existing = Some(a.clone()); break;
                }
            }
            let mut account = existing.unwrap_or_else(|| ClaudeAccount {
                id: uuid::Uuid::new_v4().to_string(), name: name.trim().into(),
                email: config.pointer("/oauthAccount/emailAddress").and_then(Value::as_str).map(str::to_owned),
                account_uuid, plan_type: None, created_at: Utc::now().to_rfc3339(), last_used_at: None,
            });
            account.plan_type = credentials.pointer("/claudeAiOauth/subscriptionType").and_then(Value::as_str).map(str::to_owned);
            let dir = self.profile(&account.id)?;
            fs::create_dir_all(&dir)?;
            let saved = object_file(&dir.join(".credentials.json"))?;
            let saved_expiry = saved.pointer("/claudeAiOauth/expiresAt").and_then(Value::as_i64).unwrap_or(0);
            let imported_expiry = credentials.pointer("/claudeAiOauth/expiresAt").and_then(Value::as_i64).unwrap_or(0);
            if !self.has_relogin_barrier(&account.id, account.account_uuid.as_deref())? && imported_expiry >= saved_expiry { write_json(&dir.join(".credentials.json"), &json!({"claudeAiOauth": credentials["claudeAiOauth"]}))?; }
            write_json(&dir.join(".claude.json"), &json!({"oauthAccount":config["oauthAccount"],"hasCompletedOnboarding":true,"hasAvailableSubscription":true}))?;
            // Preserve existing per-profile settings on re-import, including the receipt bridge.
            if !dir.join("settings.json").exists() { write_json(&dir.join("settings.json"), settings)?; }
            if let Some(found) = store.accounts.iter_mut().find(|a| a.id == account.id) { *found = account.clone(); }
            else { store.accounts.push(account.clone()); }
            Ok(account)
        })
    }
    pub fn activate(&self, id: &str) -> Result<()> {
        self.activate_checked(id, |_| Ok(()))
    }
    pub fn activate_checked(
        &self,
        id: &str,
        check: impl FnOnce(&Self) -> Result<()>,
    ) -> Result<()> {
        let _lock = lock_file(&self.root.join("activation.lock"))?;
        let _live_guard = super::locks::CredentialsGuard::acquire(&self.live_dir)?;
        let _config_guard = super::locks::ConfigGuard::acquire(&self.live_config)?;
        let account = self.account(id)?;
        self.sync_active_locked()?;
        check(self)?;
        let target = self.profile(id)?;
        let _profile_guard = super::locks::CredentialsGuard::acquire(&target)?;
        let barrier = relogin_barrier(&target, account.account_uuid.as_deref())?;
        let credentials = object_file(&target.join(".credentials.json"))?;
        validate_credentials(&credentials)?;
        let config = object_file(&target.join(".claude.json"))?;
        if barrier.is_some() {
            ensure!(
                config_uuid(&config) == account.account_uuid.as_deref(),
                "Claude profile UUID changed before activation"
            );
        }
        let old_credentials = object_file(&self.live_dir.join(".credentials.json"))?;
        let old_config = object_file(&self.live_config)?;
        let mut new_credentials = old_credentials.clone();
        new_credentials["claudeAiOauth"] = credentials["claudeAiOauth"].clone();
        let mut new_config = old_config.clone();
        new_config["oauthAccount"] = config["oauthAccount"].clone();
        new_config["hasAvailableSubscription"] = json!(true);
        let result = (|| {
            write_json(&self.live_dir.join(".credentials.json"), &new_credentials)?;
            write_json(&self.live_config, &new_config)?;
            ensure!(
                object_file(&self.live_dir.join(".credentials.json"))?["claudeAiOauth"]
                    == new_credentials["claudeAiOauth"],
                "Claude credential read-back did not match the selected profile"
            );
            ensure!(
                same_identity(
                    &account,
                    &object_file(&self.live_config)?,
                    &object_file(&self.live_dir.join(".credentials.json"))?,
                    &credentials
                ),
                "Claude credential read-back did not match"
            );
            Ok(())
        })();
        if result.is_err() {
            write_json(&self.live_dir.join(".credentials.json"), &old_credentials)?;
            write_json(&self.live_config, &old_config)?;
        }
        result?;
        self.transaction(|store| {
            let selected = store
                .accounts
                .iter_mut()
                .find(|a| a.id == id)
                .context("Claude account was removed during activation")?;
            ensure!(
                selected.account_uuid == account.account_uuid,
                "Claude account UUID changed during activation"
            );
            selected.last_used_at = Some(Utc::now().to_rfc3339());
            Ok(())
        })?;
        // All writes and read-backs succeeded. Still hold live/config/profile
        // protocol guards; a changed marker or profile must keep its barrier.
        ensure!(
            relogin_barrier(&target, account.account_uuid.as_deref())? == barrier,
            "Claude relogin barrier changed during activation"
        );
        if let Some(expected_barrier) = barrier.as_deref() {
            ensure!(
                repair_object_file(&target.join(".credentials.json"))?["claudeAiOauth"]
                    == credentials["claudeAiOauth"]
                    && repair_object_file(&self.live_dir.join(".credentials.json"))?
                        ["claudeAiOauth"]
                        == credentials["claudeAiOauth"]
                    && repair_object_file(&target.join(".claude.json"))?["oauthAccount"]
                        == config["oauthAccount"]
                    && repair_object_file(&self.live_config)?["oauthAccount"]
                        == config["oauthAccount"],
                "Claude selected profile generation changed during activation"
            );
            clear_relogin_barrier(&target, account.account_uuid.as_deref(), expected_barrier)?;
        }
        Ok(())
    }
    pub fn receipt(&self, id: &str) -> Result<Option<UsageReceipt>> {
        self.account(id)?;
        read_json(&self.profile(id)?.join("usage.json"))
    }
    pub fn backup(&self) -> Result<ClaudeBackup> {
        self.sync_active()?;
        let mut accounts = vec![];
        for account in self.store()?.accounts {
            let p = self.profile(&account.id)?;
            let mut settings = object_file(&p.join("settings.json"))?;
            if settings["statusLine"]["command"]
                .as_str()
                .is_some_and(|s| s.contains("--claude-statusline"))
            {
                let original = object_file(&p.join(".switcher-original-statusline.json"))?;
                if original["statusLine"].is_null() {
                    settings.as_object_mut().unwrap().remove("statusLine");
                } else {
                    settings["statusLine"] = original["statusLine"].clone();
                }
            }
            accounts.push(BackupAccount {
                account,
                credentials: object_file(&p.join(".credentials.json"))?,
                config: object_file(&p.join(".claude.json"))?,
                settings,
            });
        }
        Ok(ClaudeBackup {
            provider: "claude".into(),
            version: 1,
            accounts,
            settings: self.settings()?,
        })
    }
    pub fn import_backup(
        &self,
        backup: ClaudeBackup,
    ) -> Result<crate::types::ImportAccountsSummary> {
        ensure!(
            backup.provider == "claude" && backup.version == 1,
            "This is not a Claude backup"
        );
        ensure!(backup.accounts.len() <= 500, "Too many accounts in backup");
        Self::validate_settings(&backup.settings)?;
        // Validate the entire payload before persisting any account.
        for entry in &backup.accounts {
            validate_credentials(&entry.credentials)?;
            ensure!(
                !entry.account.name.trim().is_empty() && entry.account.name.chars().count() <= 100,
                "Invalid account name"
            );
            ensure!(
                entry.config.is_object() && entry.settings.is_object(),
                "Invalid Claude profile settings"
            );
        }
        let total = backup.accounts.len();
        let before = self.store()?.accounts.len();
        let mut id_map = std::collections::HashMap::new();
        for entry in backup.accounts {
            let account = self.add(
                &entry.account.name,
                &entry.credentials,
                &entry.config,
                &entry.settings,
            )?;
            id_map.insert(entry.account.id, account.id);
        }
        let mut settings = backup.settings;
        settings.warmup_account_ids = settings
            .warmup_account_ids
            .iter()
            .filter_map(|id| id_map.get(id).cloned())
            .collect();
        self.save_settings(&settings)?;
        let imported = self.store()?.accounts.len() - before;
        Ok(crate::types::ImportAccountsSummary {
            total_in_payload: total,
            imported_count: imported,
            skipped_count: total - imported,
        })
    }
    pub fn delete(&self, id: &str) -> Result<()> {
        self.account(id)?;
        self.transaction(|s| {
            s.accounts.retain(|a| a.id != id);
            s.masked_account_ids.retain(|a| a != id);
            Ok(())
        })?;
        // Keep the profile as a recoverable archive; deleting an entry never logs out the real CLI.
        Ok(())
    }
    pub fn rename(&self, id: &str, name: &str) -> Result<()> {
        ensure!(
            !name.trim().is_empty() && name.chars().count() <= 100,
            "Invalid account name"
        );
        self.transaction(|s| {
            let a = s
                .accounts
                .iter_mut()
                .find(|a| a.id == id)
                .context("Claude account not found")?;
            a.name = name.trim().into();
            Ok(())
        })
    }
    pub fn masked(&self, ids: Vec<String>) -> Result<()> {
        self.transaction(|s| {
            ensure!(
                ids.iter().all(|id| s.accounts.iter().any(|a| a.id == *id)),
                "Unknown account in mask settings"
            );
            s.masked_account_ids = ids;
            Ok(())
        })
    }
    pub fn read_config(path: &Path) -> Result<Value> {
        object_file(path)
    }
}

pub fn eligible_usage(receipt: &UsageReceipt, now: i64) -> bool {
    if receipt.account_id != receipt.usage.account_id {
        return false;
    }
    if receipt.observed_at > now + 5
        || now - receipt.observed_at > 300
        || receipt.usage.error.is_some()
    {
        return false;
    }
    let windows = [
        (
            receipt.usage.primary_used_percent,
            receipt.usage.primary_resets_at,
        ),
        (
            receipt.usage.secondary_used_percent,
            receipt.usage.secondary_resets_at,
        ),
    ];
    // Both subscription windows must be known. Missing data is not spare
    // quota. A server-confirmed 0% with null reset is an unstarted window.
    windows.iter().all(|(p, r)| {
        p.is_some_and(|v| v.is_finite() && (0.0..=100.0).contains(&v))
            && (r.is_some_and(|t| t > now) || (p == &Some(0.0) && r.is_none()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Manager {
        let root =
            std::env::temp_dir().join(format!("claude-switcher-test-{}", uuid::Uuid::new_v4()));
        Manager {
            root: root.join("switcher/claude"),
            live_dir: root.join("live"),
            live_config: root.join("live/.claude.json"),
        }
    }
    fn credentials(token: &str) -> Value {
        json!({"claudeAiOauth":{"accessToken":token,"refreshToken":format!("r-{token}"),"expiresAt":4000000000000_i64,"subscriptionType":"max"}})
    }
    fn relogin_fixture() -> (Manager, ClaudeAccount) {
        let m = fixture();
        write_json(
            &m.live_config,
            &json!({"oauthAccount":{"accountUuid":"one"},"projects":{"keep":true}}),
        )
        .unwrap();
        write_json(&m.live_dir.join(".credentials.json"), &credentials("live")).unwrap();
        write_json(
            &m.live_dir.join("settings.json"),
            &json!({"liveSetting":true}),
        )
        .unwrap();
        m.save_settings(&ClaudeSettings::default()).unwrap();
        let a = m.add("Keep this label", &credentials("old"),
            &json!({"oauthAccount":{"accountUuid":"one","emailAddress":"original@example.test"}}),
            &json!({"model":"opus","statusLine":{"command":"custom-command"}})).unwrap();
        let p = m.profile(&a.id).unwrap();
        let mut saved = credentials("old");
        saved["mcpOAuth"] = json!({"keep":"target"});
        write_json(&p.join(".credentials.json"), &saved).unwrap();
        let receipt = UsageReceipt {
            account_id: a.id.clone(),
            observed_at: 123,
            usage: super::super::runtime::usage_from_statusline(
                &a.id,
                Some("max".into()),
                &json!({"rate_limits":{"five_hour":{"used_percentage":77,"resets_at":4000000000_i64},"seven_day":{"used_percentage":88,"resets_at":4000000000_i64}}}),
            ),
            sessions: [(
                "existing-session".into(),
                SessionUsage {
                    input_tokens: 123,
                    output_tokens: 456,
                    cost_usd: 0.25,
                    day: "2026-10-10".into(),
                },
            )]
            .into_iter()
            .collect(),
        };
        let mut receipt = receipt;
        receipt.usage.error = Some("old invalid_grant".into());
        write_json(&p.join("usage.json"), &receipt).unwrap();
        write_json(
            &p.join("quota-poll.json"),
            &json!({"account_uuid":"one","last_error":"old invalid_grant","next_poll_at":4000000000_i64,"attempt_id":"old-http-attempt","in_flight_until":4000000000_i64,"credential_generation":"old-generation","authentication_required":true}),
        )
        .unwrap();
        write_json(
            &p.join("quota-token-pending.json"),
            &json!({"old":"successor"}),
        )
        .unwrap();
        write_json(
            &p.join("quota-token-pending.json.bak"),
            &json!({"old":"backup"}),
        )
        .unwrap();
        fs::write(p.join("session.jsonl"), "synthetic saved conversation\n").unwrap();
        let neighbor = m
            .add(
                "Other account",
                &credentials("other"),
                &json!({"oauthAccount":{"accountUuid":"two"}}),
                &json!({}),
            )
            .unwrap();
        let other = m.profile(&neighbor.id).unwrap();
        write_json(
            &other.join("quota-token-pending.json"),
            &json!({"keep":"neighbor"}),
        )
        .unwrap();
        (m, a)
    }
    fn relogin_files(m: &Manager, a: &ClaudeAccount) -> Vec<(PathBuf, Vec<u8>)> {
        let p = m.profile(&a.id).unwrap();
        let mut paths = vec![
            m.root.join("accounts.json"),
            m.root.join("settings.json"),
            m.live_config.clone(),
            m.live_dir.join(".credentials.json"),
            m.live_dir.join("settings.json"),
            p.join(".claude.json"),
            p.join("settings.json"),
            p.join("session.jsonl"),
        ];
        for account in m.store().unwrap().accounts {
            if account.id != a.id {
                let other = m.profile(&account.id).unwrap();
                paths.extend([
                    other.join(".credentials.json"),
                    other.join(".claude.json"),
                    other.join("settings.json"),
                    other.join("quota-token-pending.json"),
                ]);
            }
        }
        paths
            .into_iter()
            .map(|p| {
                let bytes = fs::read(&p).unwrap();
                (p, bytes)
            })
            .collect()
    }
    fn assert_relogin_files_unchanged(files: &[(PathBuf, Vec<u8>)]) {
        for (path, bytes) in files {
            assert_eq!(
                &fs::read(path).unwrap(),
                bytes,
                "Changed {}",
                path.display()
            );
        }
    }
    #[test]
    fn relogin_barrier_blocks_greater_and_equal_old_live_expiry_through_profile_rotations() {
        for delta in [0_i64, 1000] {
            let (m, a) = relogin_fixture();
            let target = m.relogin_target(&a.id).unwrap();
            let mut live = credentials("old-live");
            live["claudeAiOauth"]["expiresAt"] = json!(4000000000000_i64 + delta);
            write_json(&m.live_dir.join(".credentials.json"), &live).unwrap();
            m.commit_relogin(
                &target,
                &credentials("fresh"),
                &json!({"oauthAccount":{"accountUuid":"one"}}),
            )
            .unwrap();
            let p = m.profile(&a.id).unwrap();
            let marker = fs::read(p.join("relogin-barrier.json")).unwrap();
            let marker_text = String::from_utf8(marker.clone()).unwrap();
            assert!(!marker_text.contains("fresh"));
            assert!(!marker_text.contains("accessToken"));
            for token in ["fresh", "profile-cli-rotation"] {
                // The marker is independent of the current token fingerprint.
                let rotated = credentials(token);
                write_json(&p.join(".credentials.json"), &rotated).unwrap();
                assert_eq!(m.sync_active().unwrap(), Some(a.id.clone()));
                m.import_current("Keep this label").unwrap();
                m.backup().unwrap();
                assert_eq!(
                    object_file(&p.join(".credentials.json")).unwrap()["claudeAiOauth"],
                    rotated["claudeAiOauth"]
                );
                assert_eq!(fs::read(p.join("relogin-barrier.json")).unwrap(), marker);
                // Quota snapshot invokes this helper under the accounts lock.
                assert!(m
                    .transaction(|_| m.has_relogin_barrier(&a.id, Some("one")))
                    .unwrap());
            }
            assert_eq!(
                object_file(&m.live_dir.join(".credentials.json")).unwrap(),
                live
            );
            fs::remove_dir_all(m.root.parent().unwrap().parent().unwrap()).unwrap();
        }
    }
    #[test]
    fn relogin_barrier_is_strict_bounded_uuid_checked_and_never_recovers_backup() {
        let m = fixture();
        let id = uuid::Uuid::new_v4().to_string();
        let p = m.profile(&id).unwrap();
        assert!(!m.has_relogin_barrier(&id, None).unwrap());
        assert!(!m.root.exists());
        fs::create_dir_all(&p).unwrap();
        let path = p.join("relogin-barrier.json");
        let marker =
            json!({"version":1,"account_uuid":"one","generation":uuid::Uuid::new_v4().to_string()});
        write_json(&path, &marker).unwrap();
        assert!(m.has_relogin_barrier(&id, Some("one")).unwrap());
        assert!(m.has_relogin_barrier(&id, Some("two")).is_err());
        assert!(m.has_relogin_barrier(&id, None).is_err());
        let backup = p.join("relogin-barrier.json.bak");
        let good = fs::read(&path).unwrap();
        fs::write(&backup, &good).unwrap();
        for bad in [b"{bad".to_vec(), b"[]".to_vec(), vec![b' '; 4097]] {
            fs::write(&path, &bad).unwrap();
            assert!(m.has_relogin_barrier(&id, Some("one")).is_err());
            assert_eq!(fs::read(&path).unwrap(), bad);
            assert_eq!(fs::read(&backup).unwrap(), good);
        }
        fs::remove_file(&path).unwrap();
        assert!(m.has_relogin_barrier(&id, Some("one")).is_err());
        assert!(!path.exists());
        assert!(m.has_relogin_barrier("../escape", Some("one")).is_err());
        fs::remove_dir_all(m.root.parent().unwrap().parent().unwrap()).unwrap();
    }
    #[test]
    fn relogin_barrier_survives_failed_activation_and_clears_after_exact_install() {
        let (m, a) = relogin_fixture();
        let target = m.relogin_target(&a.id).unwrap();
        m.commit_relogin(
            &target,
            &credentials("fresh"),
            &json!({"oauthAccount":{"accountUuid":"one"}}),
        )
        .unwrap();
        let p = m.profile(&a.id).unwrap();
        let path = p.join("relogin-barrier.json");
        let marker = fs::read(&path).unwrap();
        let backup = p.join("relogin-barrier.json.bak");
        fs::write(&backup, &marker).unwrap();
        let live_before = fs::read(m.live_dir.join(".credentials.json")).unwrap();
        assert!(m
            .activate_checked(&a.id, |_| anyhow::bail!("synthetic activation rejection"))
            .is_err());
        assert_eq!(fs::read(&path).unwrap(), marker);
        assert_eq!(fs::read(&backup).unwrap(), marker);
        assert_eq!(
            fs::read(m.live_dir.join(".credentials.json")).unwrap(),
            live_before
        );
        // Unreadable live configuration must retain the marker too.
        let config_before = fs::read(&m.live_config).unwrap();
        fs::remove_file(&m.live_config).unwrap();
        fs::create_dir(&m.live_config).unwrap();
        assert!(m.activate(&a.id).is_err());
        assert_eq!(fs::read(&path).unwrap(), marker);
        fs::remove_dir(&m.live_config).unwrap();
        fs::write(&m.live_config, config_before).unwrap();
        write_json(
            &p.join(".credentials.json"),
            &credentials("rotated-after-relogin"),
        )
        .unwrap();
        m.activate(&a.id).unwrap();
        assert!(!path.exists());
        assert!(!backup.exists());
        assert!(!m.has_relogin_barrier(&a.id, Some("one")).unwrap());
        assert_eq!(
            object_file(&m.live_dir.join(".credentials.json")).unwrap()["claudeAiOauth"],
            credentials("rotated-after-relogin")["claudeAiOauth"]
        );
        fs::remove_dir_all(m.root.parent().unwrap().parent().unwrap()).unwrap();
    }
    #[test]
    fn activation_cleanup_preserves_a_changed_marker_and_failed_backup_removal() {
        let (m, a) = relogin_fixture();
        let target = m.relogin_target(&a.id).unwrap();
        m.commit_relogin(
            &target,
            &credentials("fresh"),
            &json!({"oauthAccount":{"accountUuid":"one"}}),
        )
        .unwrap();
        let p = m.profile(&a.id).unwrap();
        let path = p.join("relogin-barrier.json");
        let old = fs::read(&path).unwrap();
        let mut marker = object_file(&path).unwrap();
        marker["generation"] = json!(uuid::Uuid::new_v4().to_string());
        write_json(&path, &marker).unwrap();
        let changed = fs::read(&path).unwrap();
        let backup = p.join("relogin-barrier.json.bak");
        assert!(clear_relogin_barrier(&p, Some("one"), &old).is_err());
        assert_eq!(fs::read(&path).unwrap(), changed);
        assert!(backup.exists());
        fs::remove_file(&backup).unwrap();
        fs::create_dir(&backup).unwrap();
        assert!(m.activate(&a.id).is_err());
        assert_eq!(fs::read(&path).unwrap(), changed);
        fs::remove_dir(&backup).unwrap();
        m.activate(&a.id).unwrap();
        assert!(!path.exists());
        fs::remove_dir_all(m.root.parent().unwrap().parent().unwrap()).unwrap();
    }
    #[test]
    fn relogin_installs_barrier_before_quota_cleanup_can_fail() {
        let (m, a) = relogin_fixture();
        let target = m.relogin_target(&a.id).unwrap();
        let p = m.profile(&a.id).unwrap();
        fs::create_dir(p.join("quota-poll.lock")).unwrap();
        assert!(m
            .commit_relogin(
                &target,
                &credentials("fresh"),
                &json!({"oauthAccount":{"accountUuid":"one"}})
            )
            .is_err());
        assert_eq!(
            object_file(&p.join(".credentials.json")).unwrap()["claudeAiOauth"],
            credentials("fresh")["claudeAiOauth"]
        );
        assert!(m.has_relogin_barrier(&a.id, Some("one")).unwrap());
        m.sync_active().unwrap();
        assert_eq!(
            object_file(&p.join(".credentials.json")).unwrap()["claudeAiOauth"],
            credentials("fresh")["claudeAiOauth"]
        );
        fs::remove_dir_all(m.root.parent().unwrap().parent().unwrap()).unwrap();
    }
    #[test]
    fn active_id_and_read_config_never_restore_malformed_primary_from_good_backup() {
        for location in [
            "live-credentials",
            "live-config",
            "profile-credentials",
            "profile-config",
        ] {
            let (m, a) = relogin_fixture();
            let p = m.profile(&a.id).unwrap();
            let path = match location {
                "live-credentials" => m.live_dir.join(".credentials.json"),
                "live-config" => m.live_config.clone(),
                "profile-credentials" => p.join(".credentials.json"),
                _ => p.join(".claude.json"),
            };
            let good = fs::read(&path).unwrap();
            let backup = crate::auth::persistence::backup_path(&path);
            fs::write(&backup, &good).unwrap();
            let bad = b"{malformed concurrent writer";
            fs::write(&path, bad).unwrap();
            assert!(Manager::read_config(&path).is_err());
            let result = m.active_id(&m.store().unwrap());
            if location != "profile-config" {
                assert!(result.is_err());
            }
            assert_eq!(fs::read(&path).unwrap(), bad);
            assert_eq!(fs::read(&backup).unwrap(), good);
            fs::remove_dir_all(m.root.parent().unwrap().parent().unwrap()).unwrap();
        }
    }
    #[test]
    fn relogin_same_uuid_merges_only_oauth_preserves_label_settings_sessions_and_live() {
        let (m, a) = relogin_fixture();
        let target = m.relogin_target(&a.id).unwrap();
        // Concurrent metadata and unrelated credential updates are preserved.
        m.rename(&a.id, "Renamed while login was open").unwrap();
        let p = m.profile(&a.id).unwrap();
        let mut current = object_file(&p.join(".credentials.json")).unwrap();
        current["anotherKey"] = json!({"concurrent":true});
        write_json(&p.join(".credentials.json"), &current).unwrap();
        let files = relogin_files(&m, &a);
        let before = m.receipt(&a.id).unwrap().unwrap();
        let mut fresh = credentials("fresh-official-cli");
        fresh["claudeAiOauth"]["subscriptionType"] = json!("pro");
        fresh["unrelatedStagedSecret"] = json!("must-not-copy");
        let result = m.commit_relogin(&target, &fresh,
            &json!({"oauthAccount":{"accountUuid":"one","emailAddress":"new@example.test"},"projects":{"mustNotCopy":true}})).unwrap();
        assert_eq!(result.id, a.id);
        assert_eq!(result.name, "Renamed while login was open");
        assert_eq!(result.plan_type, a.plan_type);
        assert_eq!(result.email, a.email);
        let saved = object_file(&p.join(".credentials.json")).unwrap();
        current["claudeAiOauth"] = fresh["claudeAiOauth"].clone();
        assert_eq!(saved, current);
        assert_relogin_files_unchanged(&files);
        assert_eq!(m.store().unwrap().accounts.len(), 2);
        let after = m.receipt(&a.id).unwrap().unwrap();
        assert_eq!(after.observed_at, before.observed_at);
        assert_eq!(
            serde_json::to_value(&after.sessions).unwrap(),
            serde_json::to_value(&before.sessions).unwrap()
        );
        let mut expected_usage = serde_json::to_value(&before.usage).unwrap();
        expected_usage["error"] = serde_json::to_value(&after.usage.error).unwrap();
        assert_eq!(serde_json::to_value(&after.usage).unwrap(), expected_usage);
        assert!(after.usage.error.unwrap().contains("stale"));
        assert!(!p.join("quota-token-pending.json").exists());
        assert!(!p.join("quota-token-pending.json.bak").exists());
        let poll = object_file(&p.join("quota-poll.json")).unwrap();
        assert_eq!(poll["account_uuid"], "one");
        assert!(poll["attempt_id"].is_null());
        assert!(poll["credential_generation"].is_null());
        assert!(poll["rejected_credential_generation"].is_null());
        assert_eq!(poll["authentication_required"], false);
        assert!(poll["last_error"].is_null());
        assert_eq!(poll["last_attempt_at"], 0);
        assert_eq!(poll["next_poll_at"], 0);
        assert_eq!(poll["in_flight_until"], 0);
        assert!(!p.join("quota-poll.json.bak").exists());
        fs::remove_dir_all(m.root.parent().unwrap().parent().unwrap()).unwrap();
    }
    #[test]
    fn relogin_wrong_or_missing_fresh_uuid_never_imports_or_changes_the_target() {
        let (m, a) = relogin_fixture();
        let target = m.relogin_target(&a.id).unwrap();
        let p = m.profile(&a.id).unwrap();
        let mut files = relogin_files(&m, &a);
        for name in [
            ".credentials.json",
            "usage.json",
            "quota-token-pending.json",
            "quota-token-pending.json.bak",
            "quota-poll.json",
        ] {
            let path = p.join(name);
            files.push((path.clone(), fs::read(path).unwrap()));
        }
        for config in [
            json!({"oauthAccount":{"accountUuid":"two"}}),
            json!({"oauthAccount":{"accountUuid":""}}),
            json!({"oauthAccount":{"accountUuid":" "}}),
            json!({"oauthAccount":{"accountUuid":null}}),
            json!({}),
            json!({"oauthAccount":{"accountUuid":1}}),
        ] {
            let error = m
                .commit_relogin(&target, &credentials("wrong"), &config)
                .err()
                .unwrap();
            assert!(error.to_string().contains("UUID"));
            assert_relogin_files_unchanged(&files);
        }
        assert!(m
            .commit_relogin(
                &target,
                &json!({"claudeAiOauth":{"accessToken":"bad"}}),
                &json!({"oauthAccount":{"accountUuid":"one"}})
            )
            .is_err());
        assert_relogin_files_unchanged(&files);
        fs::remove_dir_all(m.root.parent().unwrap().parent().unwrap()).unwrap();
    }
    #[test]
    fn relogin_concurrent_rotation_wins_even_with_the_same_expiry() {
        let (m, a) = relogin_fixture();
        let target = m.relogin_target(&a.id).unwrap();
        let p = m.profile(&a.id).unwrap();
        write_json(
            &p.join(".credentials.json"),
            &credentials("concurrent-winner"),
        )
        .unwrap();
        let before = fs::read(p.join(".credentials.json")).unwrap();
        let receipt = fs::read(p.join("usage.json")).unwrap();
        let pending = fs::read(p.join("quota-token-pending.json")).unwrap();
        let poll = fs::read(p.join("quota-poll.json")).unwrap();
        let files = relogin_files(&m, &a);
        let error = m
            .commit_relogin(
                &target,
                &credentials("stale-login"),
                &json!({"oauthAccount":{"accountUuid":"one"}}),
            )
            .err()
            .unwrap();
        assert!(error.to_string().contains("concurrent"));
        assert_eq!(fs::read(p.join(".credentials.json")).unwrap(), before);
        assert_eq!(fs::read(p.join("usage.json")).unwrap(), receipt);
        assert_eq!(
            fs::read(p.join("quota-token-pending.json")).unwrap(),
            pending
        );
        assert_eq!(fs::read(p.join("quota-poll.json")).unwrap(), poll);
        assert_relogin_files_unchanged(&files);
        fs::remove_dir_all(m.root.parent().unwrap().parent().unwrap()).unwrap();
    }
    #[test]
    fn relogin_deleted_or_changed_target_is_never_recreated_or_replaced() {
        for change in [
            "delete",
            "metadata-uuid",
            "profile-uuid",
            "credentials-deleted",
        ] {
            let (m, a) = relogin_fixture();
            let target = m.relogin_target(&a.id).unwrap();
            let p = m.profile(&a.id).unwrap();
            match change {
                "delete" => m.delete(&a.id).unwrap(),
                "metadata-uuid" => m
                    .transaction(|s| {
                        s.accounts
                            .iter_mut()
                            .find(|v| v.id == a.id)
                            .unwrap()
                            .account_uuid = Some("other".into());
                        Ok(())
                    })
                    .unwrap(),
                "profile-uuid" => write_json(
                    &p.join(".claude.json"),
                    &json!({"oauthAccount":{"accountUuid":"other"}}),
                )
                .unwrap(),
                _ => fs::remove_file(p.join(".credentials.json")).unwrap(),
            }
            let files = relogin_files(&m, &a);
            let credentials_before = fs::read(p.join(".credentials.json")).ok();
            let receipt_before = fs::read(p.join("usage.json")).unwrap();
            let poll_before = fs::read(p.join("quota-poll.json")).unwrap();
            assert!(m
                .commit_relogin(
                    &target,
                    &credentials("stale-login"),
                    &json!({"oauthAccount":{"accountUuid":"one"}})
                )
                .is_err());
            assert_relogin_files_unchanged(&files);
            assert_eq!(
                fs::read(p.join(".credentials.json")).ok(),
                credentials_before
            );
            assert_eq!(fs::read(p.join("usage.json")).unwrap(), receipt_before);
            assert_eq!(fs::read(p.join("quota-poll.json")).unwrap(), poll_before);
            if change == "delete" {
                assert!(m.account(&a.id).is_err());
            }
            fs::remove_dir_all(m.root.parent().unwrap().parent().unwrap()).unwrap();
        }
    }
    #[test]
    fn relogin_requires_nonempty_target_uuid_before_starting() {
        for uuid in [None, Some(""), Some(" ")] {
            let (m, a) = relogin_fixture();
            m.transaction(|s| {
                s.accounts
                    .iter_mut()
                    .find(|v| v.id == a.id)
                    .unwrap()
                    .account_uuid = uuid.map(str::to_owned);
                Ok(())
            })
            .unwrap();
            assert!(m
                .relogin_target(&a.id)
                .err()
                .unwrap()
                .to_string()
                .contains("UUID"));
            fs::remove_dir_all(m.root.parent().unwrap().parent().unwrap()).unwrap();
        }
    }
    #[test]
    fn relogin_identical_token_pair_does_not_clear_errors_reservations_or_pending() {
        let (m, a) = relogin_fixture();
        let target = m.relogin_target(&a.id).unwrap();
        let p = m.profile(&a.id).unwrap();
        let mut files = relogin_files(&m, &a);
        for name in [
            ".credentials.json",
            "usage.json",
            "quota-poll.json",
            "quota-token-pending.json",
            "quota-token-pending.json.bak",
        ] {
            let path = p.join(name);
            files.push((path.clone(), fs::read(path).unwrap()));
        }
        // Metadata/expiry changes alone do not establish a new token generation.
        let mut same = credentials("old");
        same["claudeAiOauth"]["expiresAt"] = json!(4000000010000_i64);
        same["claudeAiOauth"]["subscriptionType"] = json!("pro");
        let error = m
            .commit_relogin(
                &target,
                &same,
                &json!({"oauthAccount":{"accountUuid":"one"}}),
            )
            .err()
            .unwrap();
        assert!(error.to_string().contains("unchanged credentials"));
        assert_relogin_files_unchanged(&files);
        fs::remove_dir_all(m.root.parent().unwrap().parent().unwrap()).unwrap();
    }
    #[test]
    fn relogin_malformed_config_or_credentials_never_restore_an_older_backup() {
        for name in [".claude.json", ".credentials.json"] {
            let (m, a) = relogin_fixture();
            let target = m.relogin_target(&a.id).unwrap();
            let p = m.profile(&a.id).unwrap();
            let path = p.join(name);
            let backup = crate::auth::persistence::backup_path(&path);
            fs::write(&backup, fs::read(&path).unwrap()).unwrap();
            fs::write(&path, b"{malformed concurrent writer").unwrap();
            let mut files = relogin_files(&m, &a);
            for path in [
                backup,
                p.join(".credentials.json"),
                p.join("usage.json"),
                p.join("quota-poll.json"),
                p.join("quota-token-pending.json"),
            ] {
                files.push((path.clone(), fs::read(path).unwrap()));
            }
            assert!(m.relogin_target(&a.id).is_err());
            assert_relogin_files_unchanged(&files);
            assert!(m
                .commit_relogin(
                    &target,
                    &credentials("fresh"),
                    &json!({"oauthAccount":{"accountUuid":"one"}})
                )
                .is_err());
            assert_relogin_files_unchanged(&files);
            fs::remove_dir_all(m.root.parent().unwrap().parent().unwrap()).unwrap();
        }
    }
    #[test]
    fn stale_config_uuid_cannot_attribute_another_stored_accounts_tokens() {
        let m = fixture();
        write_json(
            &m.live_config,
            &json!({"oauthAccount":{"accountUuid":"one"}}),
        )
        .unwrap();
        write_json(&m.live_dir.join(".credentials.json"), &credentials("one")).unwrap();
        let one = m.import_current("First").unwrap();
        m.add(
            "Second",
            &credentials("two"),
            &json!({"oauthAccount":{"accountUuid":"two"}}),
            &json!({}),
        )
        .unwrap();
        write_json(&m.live_dir.join(".credentials.json"), &credentials("two")).unwrap();
        assert!(m.active_id(&m.store().unwrap()).unwrap().is_none());
        assert!(m.sync_active().unwrap().is_none());
        assert_eq!(
            object_file(&m.profile(&one.id).unwrap().join(".credentials.json")).unwrap()
                ["claudeAiOauth"]["accessToken"],
            "one"
        );
        std::fs::remove_dir_all(m.root.parent().unwrap().parent().unwrap()).unwrap();
    }
    #[test]
    fn rejected_final_activation_check_preserves_live_authentication() {
        let m = fixture();
        write_json(
            &m.live_config,
            &json!({"oauthAccount":{"accountUuid":"one"}}),
        )
        .unwrap();
        write_json(&m.live_dir.join(".credentials.json"), &credentials("one")).unwrap();
        m.import_current("First").unwrap();
        let target = m
            .add(
                "Second",
                &credentials("two"),
                &json!({"oauthAccount":{"accountUuid":"two"}}),
                &json!({}),
            )
            .unwrap();
        let before = fs::read(m.live_dir.join(".credentials.json")).unwrap();
        assert!(m
            .activate_checked(&target.id, |_| anyhow::bail!(
                "synthetic account changed during auth check"
            ))
            .is_err());
        assert_eq!(
            fs::read(m.live_dir.join(".credentials.json")).unwrap(),
            before
        );
        std::fs::remove_dir_all(m.root.parent().unwrap().parent().unwrap()).unwrap();
    }
    #[test]
    fn activation_preserves_unrelated_settings_and_rotated_tokens() {
        let m = fixture();
        write_json(&m.live_config,&json!({"oauthAccount":{"accountUuid":"one","emailAddress":"one@example.test"},"projects":{"project":true},"mcpServers":{"kept":true}})).unwrap();
        write_json(&m.live_dir.join(".credentials.json"), &credentials("old")).unwrap();
        let one = m.import_current("First").unwrap();
        let two = m
            .add(
                "Second",
                &credentials("second"),
                &json!({"oauthAccount":{"accountUuid":"two"}}),
                &json!({}),
            )
            .unwrap();
        let mut rotated = credentials("rotated");
        rotated["mcpOAuth"] = json!({"preserved":true});
        write_json(&m.live_dir.join(".credentials.json"), &rotated).unwrap();
        m.activate(&two.id).unwrap();
        let saved = object_file(&m.profile(&one.id).unwrap().join(".credentials.json")).unwrap();
        assert_eq!(saved["claudeAiOauth"]["accessToken"], "rotated");
        let live = object_file(&m.live_dir.join(".credentials.json")).unwrap();
        assert_eq!(live["mcpOAuth"]["preserved"], true);
        assert_eq!(
            object_file(&m.live_config).unwrap()["mcpServers"]["kept"],
            true
        );
        assert_eq!(m.active_id(&m.store().unwrap()).unwrap(), Some(two.id));
        assert!(!m.root.parent().unwrap().join("accounts.json").exists());
        std::fs::remove_dir_all(m.root.parent().unwrap().parent().unwrap()).unwrap();
    }
    #[test]
    fn rejects_traversal_cross_provider_backup_and_invalid_credentials() {
        let m = fixture();
        assert!(m.profile("../../.codex").is_err());
        assert!(m
            .add(
                "Bad",
                &json!({"tokens":{"access_token":"codex"}}),
                &json!({}),
                &json!({})
            )
            .is_err());
        assert!(m
            .import_backup(ClaudeBackup {
                provider: "codex".into(),
                version: 1,
                accounts: vec![],
                settings: ClaudeSettings::default()
            })
            .is_err());
        assert!(!m.root.exists());
    }
    #[test]
    fn settings_and_delete_never_change_live_authentication() {
        let m = fixture();
        write_json(
            &m.live_config,
            &json!({"oauthAccount":{"accountUuid":"one"}}),
        )
        .unwrap();
        write_json(&m.live_dir.join(".credentials.json"), &credentials("one")).unwrap();
        let a = m.import_current("First").unwrap();
        let before = fs::read(m.live_dir.join(".credentials.json")).unwrap();
        let mut settings = ClaudeSettings::default();
        settings.auto_switch_enabled = true;
        m.save_settings(&settings).unwrap();
        m.rename(&a.id, "Renamed").unwrap();
        m.masked(vec![a.id.clone()]).unwrap();
        m.delete(&a.id).unwrap();
        assert_eq!(
            before,
            fs::read(m.live_dir.join(".credentials.json")).unwrap()
        );
        assert!(m.store().unwrap().accounts.is_empty());
        assert!(m.settings().unwrap().auto_switch_enabled);
        std::fs::remove_dir_all(m.root.parent().unwrap().parent().unwrap()).unwrap();
    }
    #[test]
    fn newer_profile_refresh_is_not_overwritten_by_live_or_backup_copy() {
        let m = fixture();
        write_json(
            &m.live_config,
            &json!({"oauthAccount":{"accountUuid":"one"}}),
        )
        .unwrap();
        write_json(&m.live_dir.join(".credentials.json"), &credentials("old")).unwrap();
        let a = m.import_current("First").unwrap();
        let p = m.profile(&a.id).unwrap();
        let mut newer = credentials("newer");
        newer["claudeAiOauth"]["expiresAt"] = json!(4000000001000_i64);
        write_json(&p.join(".credentials.json"), &newer).unwrap();
        m.sync_active().unwrap();
        m.import_current("First").unwrap();
        assert_eq!(
            object_file(&p.join(".credentials.json")).unwrap()["claudeAiOauth"]["accessToken"],
            "newer"
        );
        std::fs::remove_dir_all(m.root.parent().unwrap().parent().unwrap()).unwrap();
    }
}
