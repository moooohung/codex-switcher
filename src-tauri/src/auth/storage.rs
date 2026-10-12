//! Account storage: all read-modify-write operations share a cross-process lock.
use super::persistence::{atomic_write, lock_file, read_json};
use crate::types::{
    parse_chatgpt_id_token_claims, AccountsStore, AppSettings, AuthData, AuthDotJson, StoredAccount,
};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::path::PathBuf;

pub fn account_matches_auth(account: &StoredAccount, auth: &AuthDotJson) -> bool {
    match (&account.auth_data, &auth.tokens, &auth.openai_api_key) {
        (AuthData::ApiKey { key }, _, Some(live)) => !key.is_empty() && key == live,
        (
            AuthData::ChatGPT {
                id_token,
                access_token,
                account_id,
                ..
            },
            Some(live),
            None,
        ) => {
            if id_token == &live.id_token && access_token == &live.access_token {
                return true;
            }
            let stored = parse_chatgpt_id_token_claims(id_token);
            let current = parse_chatgpt_id_token_claims(&live.id_token);
            let stored_id = stored.account_id.or_else(|| account_id.clone());
            let current_id = current.account_id.or_else(|| live.account_id.clone());
            stored_id.is_some()
                && stored_id == current_id
                && match (
                    stored.email.or_else(|| account.email.clone()),
                    current.email,
                ) {
                    (Some(a), Some(b)) => a.eq_ignore_ascii_case(&b),
                    _ => true,
                }
        }
        _ => false,
    }
}

pub fn credentials_match_auth(account: &StoredAccount, auth: &AuthDotJson) -> bool {
    match (&account.auth_data, &auth.tokens, &auth.openai_api_key) {
        (AuthData::ApiKey { key }, None, Some(live)) => key == live,
        (
            AuthData::ChatGPT {
                id_token,
                access_token,
                refresh_token,
                account_id,
            },
            Some(live),
            None,
        ) => {
            id_token == &live.id_token
                && access_token == &live.access_token
                && refresh_token == &live.refresh_token
                && account_id == &live.account_id
        }
        _ => false,
    }
}

/// The marker is a cache; the credential file is the source of truth.
pub fn reconcile_active_identity(store: &mut AccountsStore, auth: Option<&AuthDotJson>) {
    store.active_account_id = auth.and_then(|auth| {
        // Exact installed credentials outrank a stale marker, even when saved
        // aliases share a workspace. Ambiguous identity-only matches stay unknown.
        let exact: Vec<_> = store
            .accounts
            .iter()
            .filter(|a| credentials_match_auth(a, auth))
            .collect();
        if !exact.is_empty() {
            return exact
                .iter()
                .find(|a| store.active_account_id.as_deref() == Some(a.id.as_str()))
                .or_else(|| exact.first())
                .map(|a| a.id.clone());
        }
        let matches: Vec<_> = store
            .accounts
            .iter()
            .filter(|a| account_matches_auth(a, auth))
            .collect();
        (matches.len() == 1).then(|| matches[0].id.clone())
    });
}
pub fn sync_active_account_tokens(store: &mut AccountsStore, auth: &AuthDotJson) -> bool {
    let Some(active_id) = store.active_account_id.as_deref() else {
        return false;
    };
    let Some(tokens) = auth.tokens.as_ref() else {
        return false;
    };
    let Some(account) = store
        .accounts
        .iter_mut()
        .find(|account| account.id == active_id)
    else {
        return false;
    };
    if account.auth_sync_pending {
        return false;
    }
    let AuthData::ChatGPT {
        id_token,
        access_token,
        refresh_token,
        account_id,
    } = &mut account.auth_data
    else {
        return false;
    };

    let stored_account_id = parse_chatgpt_id_token_claims(id_token)
        .account_id
        .or_else(|| account_id.clone());
    let current_account_id = parse_chatgpt_id_token_claims(&tokens.id_token)
        .account_id
        .or_else(|| tokens.account_id.clone());
    let (Some(stored_account_id), Some(current_account_id)) =
        (stored_account_id, current_account_id)
    else {
        return false;
    };
    if stored_account_id != current_account_id {
        return false;
    }

    let changed = *id_token != tokens.id_token
        || *access_token != tokens.access_token
        || *refresh_token != tokens.refresh_token
        || account_id.as_ref() != Some(&current_account_id);
    if !changed {
        return false;
    }

    id_token.clone_from(&tokens.id_token);
    access_token.clone_from(&tokens.access_token);
    refresh_token.clone_from(&tokens.refresh_token);
    *account_id = Some(current_account_id);
    true
}

pub fn get_config_dir() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("CODEX_SWITCHER_HOME") {
        return Ok(PathBuf::from(path));
    }
    Ok(dirs::home_dir()
        .context("Could not find home directory")?
        .join(".codex-switcher"))
}
pub fn get_accounts_file() -> Result<PathBuf> {
    Ok(get_config_dir()?.join("accounts.json"))
}
pub fn get_settings_file() -> Result<PathBuf> {
    Ok(get_config_dir()?.join("settings.json"))
}

fn read_store_locked() -> Result<AccountsStore> {
    let mut store = read_json::<AccountsStore>(&get_accounts_file()?)?.unwrap_or_default();
    let previous_active = store.active_account_id.clone();
    // Invalid auth must not prevent choosing a replacement credential in the UI.
    let auth = super::read_current_auth().ok().flatten();
    reconcile_active_identity(&mut store, auth.as_ref());
    if store.active_account_id != previous_active {
        atomic_write(&get_accounts_file()?, &serde_json::to_vec_pretty(&store)?)?;
    }
    Ok(store)
}

pub fn load_accounts() -> Result<AccountsStore> {
    let _guard = lock_file(&get_config_dir()?.join("accounts.lock"))?;
    read_store_locked()
}

pub fn update_accounts<T>(update: impl FnOnce(&mut AccountsStore) -> Result<T>) -> Result<T> {
    let _guard = lock_file(&get_config_dir()?.join("accounts.lock"))?;
    let mut store = read_store_locked()?;
    let before = serde_json::to_vec_pretty(&store)?;
    let result = update(&mut store)?;
    let auth = super::read_current_auth().ok().flatten();
    reconcile_active_identity(&mut store, auth.as_ref());
    let after = serde_json::to_vec_pretty(&store)?;
    // An unchanged token/metadata read must not re-trigger the file watcher,
    // which otherwise reloads tray statistics and queues more auth operations.
    if after != before {
        atomic_write(&get_accounts_file()?, &after)?;
    }
    Ok(result)
}

pub fn load_app_settings() -> Result<AppSettings> {
    let _guard = lock_file(&get_config_dir()?.join("settings.lock"))?;
    Ok(read_json::<AppSettings>(&get_settings_file()?)?.unwrap_or_default())
}
pub fn update_app_settings<T>(update: impl FnOnce(&mut AppSettings) -> Result<T>) -> Result<T> {
    let _guard = lock_file(&get_config_dir()?.join("settings.lock"))?;
    let mut settings = read_json::<AppSettings>(&get_settings_file()?)?.unwrap_or_default();
    let result = update(&mut settings)?;
    atomic_write(
        &get_settings_file()?,
        &serde_json::to_vec_pretty(&settings)?,
    )?;
    Ok(result)
}

pub fn add_account(account: StoredAccount) -> Result<StoredAccount> {
    update_accounts(|store| {
        if store.accounts.iter().any(|a| a.name == account.name) {
            anyhow::bail!("An account with name '{}' already exists", account.name);
        }
        store.accounts.push(account.clone());
        Ok(account)
    })
}
pub fn remove_account(account_id: &str) -> Result<()> {
    update_accounts(|store| {
        let before = store.accounts.len();
        store.accounts.retain(|a| a.id != account_id);
        if before == store.accounts.len() {
            anyhow::bail!("Account not found: {account_id}");
        }
        store.masked_account_ids.retain(|id| id != account_id);
        if store.active_account_id.as_deref() == Some(account_id) {
            store.active_account_id = None;
        }
        Ok(())
    })
}
pub fn set_active_account(account_id: &str) -> Result<()> {
    update_accounts(|store| {
        let account = store
            .accounts
            .iter()
            .find(|a| a.id == account_id)
            .context("Account not found")?;
        let auth = super::read_current_auth()?.context("No active credential file")?;
        if !account_matches_auth(account, &auth) {
            anyhow::bail!("Credential file does not match the selected account");
        }
        store.active_account_id = Some(account_id.into());
        Ok(())
    })
}
pub fn get_account(account_id: &str) -> Result<Option<StoredAccount>> {
    Ok(load_accounts()?
        .accounts
        .into_iter()
        .find(|a| a.id == account_id))
}
pub fn get_active_account() -> Result<Option<StoredAccount>> {
    let store = load_accounts()?;
    Ok(store
        .accounts
        .into_iter()
        .find(|a| Some(&a.id) == store.active_account_id.as_ref()))
}
pub fn touch_account(account_id: &str) -> Result<()> {
    update_accounts(|store| {
        if let Some(account) = store.accounts.iter_mut().find(|a| a.id == account_id) {
            account.last_used_at = Some(Utc::now());
        }
        Ok(())
    })
}
pub fn mark_auth_synced(account: &StoredAccount) -> Result<()> {
    update_accounts(|store| {
        if let Some(stored) = store.accounts.iter_mut().find(|a| a.id == account.id) {
            if serde_json::to_value(&stored.auth_data)? == serde_json::to_value(&account.auth_data)?
            {
                stored.auth_sync_pending = false;
            }
        }
        Ok(())
    })
}
pub fn update_account_metadata(
    account_id: &str,
    name: Option<String>,
    email: Option<String>,
    plan_type: Option<String>,
    subscription_expires_at: Option<Option<DateTime<Utc>>>,
) -> Result<StoredAccount> {
    update_accounts(|store| {
        if let Some(ref name) = name {
            if store
                .accounts
                .iter()
                .any(|a| a.id != account_id && &a.name == name)
            {
                anyhow::bail!("Account name already exists");
            }
        }
        let account = store
            .accounts
            .iter_mut()
            .find(|a| a.id == account_id)
            .context("Account not found")?;
        if let Some(name) = name {
            account.name = name;
        }
        if let Some(email) = email {
            account.email = Some(email);
        }
        if let Some(plan) = plan_type {
            account.plan_type = Some(plan);
        }
        if let Some(expiry) = subscription_expires_at {
            account.subscription_expires_at = expiry;
        }
        Ok(account.clone())
    })
}
pub fn update_account_chatgpt_tokens(
    account_id: &str,
    id_token: String,
    access_token: String,
    refresh_token: String,
    chatgpt_account_id: Option<String>,
    email: Option<String>,
    plan_type: Option<String>,
    subscription_expires_at: Option<DateTime<Utc>>,
) -> Result<StoredAccount> {
    update_accounts(|store| {
        let account = store
            .accounts
            .iter_mut()
            .find(|a| a.id == account_id)
            .context("Account not found")?;
        match &mut account.auth_data {
            AuthData::ChatGPT {
                id_token: id,
                access_token: access,
                refresh_token: refresh,
                account_id: workspace,
            } => {
                *id = id_token;
                *access = access_token;
                *refresh = refresh_token;
                if chatgpt_account_id.is_some() {
                    *workspace = chatgpt_account_id;
                }
            }
            _ => anyhow::bail!("Cannot update OAuth tokens for an API key account"),
        }
        account.auth_sync_pending = true;
        if let Some(email) = email {
            account.email = Some(email);
        }
        if let Some(plan) = plan_type {
            account.plan_type = Some(plan);
        }
        if let Some(expiry) = subscription_expires_at {
            account.subscription_expires_at = Some(expiry);
        }
        Ok(account.clone())
    })
}
pub fn get_masked_account_ids() -> Result<Vec<String>> {
    Ok(load_accounts()?.masked_account_ids)
}
pub fn set_masked_account_ids(ids: Vec<String>) -> Result<()> {
    update_accounts(|store| {
        store.masked_account_ids = ids
            .into_iter()
            .filter(|id| store.accounts.iter().any(|a| &a.id == id))
            .collect();
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::{reconcile_active_identity, sync_active_account_tokens};
    use crate::types::{AccountsStore, AuthData, AuthDotJson, StoredAccount, TokenData};
    use base64::Engine;

    fn account(name: &str, account_id: &str, suffix: &str) -> StoredAccount {
        StoredAccount::new_chatgpt(
            name.into(),
            None,
            None,
            None,
            format!("id-{suffix}"),
            format!("access-{suffix}"),
            format!("refresh-{suffix}"),
            Some(account_id.into()),
        )
    }

    #[test]
    fn exact_file_credentials_outrank_a_stale_marker_for_shared_workspaces() {
        let a = account("A", "shared", "a");
        let b = account("B", "shared", "b");
        let id = b.id.clone();
        let mut store = AccountsStore {
            active_account_id: Some(a.id.clone()),
            accounts: vec![a, b],
            ..AccountsStore::default()
        };
        reconcile_active_identity(&mut store, Some(&auth("shared", "b")));
        assert_eq!(store.active_account_id, Some(id));
        reconcile_active_identity(&mut store, Some(&auth("shared", "unknown-person")));
        assert_eq!(store.active_account_id, None);
    }

    fn auth(account_id: &str, suffix: &str) -> AuthDotJson {
        AuthDotJson {
            openai_api_key: None,
            tokens: Some(TokenData {
                id_token: format!("id-{suffix}"),
                access_token: format!("access-{suffix}"),
                refresh_token: format!("refresh-{suffix}"),
                account_id: Some(account_id.into()),
            }),
            last_refresh: None,
        }
    }

    fn refresh_token(account: &StoredAccount) -> &str {
        match &account.auth_data {
            AuthData::ChatGPT { refresh_token, .. } => refresh_token,
            AuthData::ApiKey { .. } => panic!("expected ChatGPT account"),
        }
    }

    fn id_token_with_account_id(account_id: &str, suffix: &str) -> String {
        let payload =
            format!(r#"{{"https://api.openai.com/auth":{{"chatgpt_account_id":"{account_id}"}}}}"#);
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload);
        format!("header.{encoded}.{suffix}")
    }

    #[test]
    fn preserves_rotated_tokens_before_switching_away_and_back() {
        let account_a = account("A", "workspace-a", "a1");
        let account_a_id = account_a.id.clone();
        let account_b = account("B", "workspace-b", "b1");
        let account_b_id = account_b.id.clone();
        let mut store = AccountsStore {
            accounts: vec![account_a, account_b],
            active_account_id: Some(account_a_id.clone()),
            ..AccountsStore::default()
        };

        assert!(!sync_active_account_tokens(
            &mut store,
            &auth("workspace-b", "wrong-account")
        ));
        assert_eq!(refresh_token(&store.accounts[0]), "refresh-a1");

        let mut auth_without_top_level_id = auth("workspace-b", "missing-id");
        let tokens = auth_without_top_level_id.tokens.as_mut().unwrap();
        tokens.id_token = id_token_with_account_id("workspace-b", "signature");
        tokens.account_id = None;
        assert!(!sync_active_account_tokens(
            &mut store,
            &auth_without_top_level_id
        ));
        assert_eq!(refresh_token(&store.accounts[0]), "refresh-a1");

        let mut auth_without_identity = auth("workspace-a", "unknown");
        auth_without_identity.tokens.as_mut().unwrap().account_id = None;
        assert!(!sync_active_account_tokens(
            &mut store,
            &auth_without_identity
        ));
        assert_eq!(refresh_token(&store.accounts[0]), "refresh-a1");

        assert!(sync_active_account_tokens(
            &mut store,
            &auth("workspace-a", "a2")
        ));
        store.active_account_id = Some(account_b_id);

        let restored_a = store
            .accounts
            .iter()
            .find(|account| account.id == account_a_id)
            .unwrap();
        let AuthData::ChatGPT { refresh_token, .. } = &restored_a.auth_data else {
            panic!("expected ChatGPT account");
        };
        assert_eq!(refresh_token, "refresh-a2");
    }

    #[test]
    fn rejects_live_tokens_when_stored_account_identity_is_unknown() {
        let mut account = account("A", "workspace-a", "a1");
        let account_id = account.id.clone();
        let AuthData::ChatGPT {
            id_token,
            account_id: chatgpt_account_id,
            ..
        } = &mut account.auth_data
        else {
            panic!("expected ChatGPT account");
        };
        *id_token = "opaque-id-token".into();
        *chatgpt_account_id = None;

        let mut store = AccountsStore {
            accounts: vec![account],
            active_account_id: Some(account_id),
            ..AccountsStore::default()
        };

        assert!(!sync_active_account_tokens(
            &mut store,
            &auth("workspace-a", "a2")
        ));
        assert_eq!(refresh_token(&store.accounts[0]), "refresh-a1");
    }

    #[test]
    fn derives_stored_identity_from_id_token_and_backfills_account_id() {
        let mut account = account("A", "workspace-a", "a1");
        let account_id = account.id.clone();
        let AuthData::ChatGPT {
            id_token,
            account_id: chatgpt_account_id,
            ..
        } = &mut account.auth_data
        else {
            panic!("expected ChatGPT account");
        };
        *id_token = id_token_with_account_id("workspace-a", "stored");
        *chatgpt_account_id = None;

        let mut store = AccountsStore {
            accounts: vec![account],
            active_account_id: Some(account_id),
            ..AccountsStore::default()
        };

        assert!(sync_active_account_tokens(
            &mut store,
            &auth("workspace-a", "a2")
        ));
        let AuthData::ChatGPT { account_id, .. } = &store.accounts[0].auth_data else {
            panic!("expected ChatGPT account");
        };
        assert_eq!(account_id.as_deref(), Some("workspace-a"));
        assert_eq!(refresh_token(&store.accounts[0]), "refresh-a2");
    }
}
