//! Account management Tauri commands

use crate::auth::{
    add_account, create_chatgpt_account_from_refresh_token, ensure_chatgpt_tokens_fresh_locked,
    import_from_auth_json, import_from_auth_json_contents, load_accounts, read_current_auth,
    remove_account, set_active_account, switch_to_account, sync_active_account_tokens,
    touch_account, update_accounts,
};
use crate::types::{AccountInfo, AccountsStore, AuthData, ImportAccountsSummary, StoredAccount};

use super::process::ensure_codex_not_running;

use anyhow::Context;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chacha20poly1305::{
    aead::{Aead, KeyInit},
    XChaCha20Poly1305, XNonce,
};
use flate2::{read::ZlibDecoder, write::ZlibEncoder, Compression};
use futures::{stream, StreamExt};
use pbkdf2::pbkdf2_hmac;
use rand::RngCore;
use sha2::Sha256;
use std::collections::HashSet;
use std::fs;
use std::io::{Read, Write};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

const SLIM_EXPORT_PREFIX: &str = "css1.";
const SLIM_FORMAT_VERSION: u8 = 1;
const SLIM_AUTH_API_KEY: u8 = 0;
const SLIM_AUTH_CHATGPT: u8 = 1;

const FULL_FILE_MAGIC: &[u8; 4] = b"CSWF";
const FULL_FILE_VERSION: u8 = 2;
const FULL_SALT_LEN: usize = 16;
const FULL_NONCE_LEN: usize = 24;
const FULL_KDF_ITERATIONS: u32 = 210_000;
const LEGACY_FULL_PASSPHRASE: &str = "gT7kQ9mV2xN4pL8sR1dH6zW3cB5yF0uJ_aE7nK2tP9vM4rX1";

const MAX_IMPORT_JSON_BYTES: u64 = 2 * 1024 * 1024;
const MAX_IMPORT_FILE_BYTES: u64 = 8 * 1024 * 1024;
const SLIM_IMPORT_CONCURRENCY: usize = 6;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct SlimPayload {
    #[serde(rename = "v")]
    version: u8,
    #[serde(rename = "a", skip_serializing_if = "Option::is_none")]
    active_name: Option<String>,
    #[serde(rename = "c")]
    accounts: Vec<SlimAccountPayload>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct SlimAccountPayload {
    #[serde(rename = "n")]
    name: String,
    #[serde(rename = "t")]
    auth_type: u8,
    #[serde(rename = "k", skip_serializing_if = "Option::is_none")]
    api_key: Option<String>,
    #[serde(rename = "r", skip_serializing_if = "Option::is_none")]
    refresh_token: Option<String>,
}

/// List all accounts with their info
#[tauri::command]
pub async fn list_accounts() -> Result<Vec<AccountInfo>, String> {
    let store = load_accounts().map_err(|e| e.to_string())?;
    let active_id = store.active_account_id.as_deref();

    let accounts: Vec<AccountInfo> = store
        .accounts
        .iter()
        .map(|a| {
            let mut info = AccountInfo::from_stored(a, active_id);
            super::usage::apply_cached_account_metadata(&mut info);
            info
        })
        .collect();

    Ok(accounts)
}

/// Get the currently active account
#[tauri::command]
pub async fn get_active_account_info() -> Result<Option<AccountInfo>, String> {
    let store = load_accounts().map_err(|e| e.to_string())?;
    let active_id = store.active_account_id.as_deref();

    if let Some(active) = store
        .accounts
        .iter()
        .find(|a| active_id == Some(a.id.as_str()))
    {
        let mut info = AccountInfo::from_stored(active, active_id);
        super::usage::apply_cached_account_metadata(&mut info);
        Ok(Some(info))
    } else {
        Ok(None)
    }
}

/// Add an account from an auth.json file
#[tauri::command]
pub async fn add_account_from_file(path: String, name: String) -> Result<AccountInfo, String> {
    // Import from the file
    let account = import_from_auth_json(&path, name).map_err(|e| e.to_string())?;

    // Add to storage
    let stored = add_account(account).map_err(|e| e.to_string())?;

    let store = load_accounts().map_err(|e| e.to_string())?;
    let active_id = store.active_account_id.as_deref();

    Ok(AccountInfo::from_stored(&stored, active_id))
}

/// Add an account from uploaded auth.json contents.
pub async fn add_account_from_auth_json_text(
    name: String,
    contents: String,
) -> Result<AccountInfo, String> {
    let account = import_from_auth_json_contents(&contents, name).map_err(|e| e.to_string())?;
    let stored = add_account(account).map_err(|e| e.to_string())?;

    let store = load_accounts().map_err(|e| e.to_string())?;
    let active_id = store.active_account_id.as_deref();

    Ok(AccountInfo::from_stored(&stored, active_id))
}

/// Switch to a different account
#[tauri::command]
pub async fn switch_account(account_id: String) -> Result<(), String> {
    switch_account_by_id(&account_id).await
}

pub async fn switch_account_by_id(account_id: &str) -> Result<(), String> {
    let _auth_guard = crate::auth::lock_auth_operation("manual_switch", account_id).await;
    let _file_guard = crate::auth::lock_auth_operations()
        .await
        .map_err(|e| e.to_string())?;
    switch_account_locked(account_id).await
}

async fn switch_account_locked(account_id: &str) -> Result<(), String> {
    let store = update_accounts(|store| {
        if let Ok(Some(auth)) = read_current_auth() {
            sync_active_account_tokens(store, &auth);
        }
        Ok(store.clone())
    })
    .map_err(|e| e.to_string())?;
    let target = store
        .accounts
        .iter()
        .find(|a| a.id == account_id)
        .ok_or_else(|| format!("Account not found: {account_id}"))?;
    if let Ok(Some(auth)) = read_current_auth() {
        if crate::auth::credentials_match_auth(target, &auth) {
            return Ok(());
        }
    }
    ensure_codex_not_running()?;
    let account = ensure_chatgpt_tokens_fresh_locked(target)
        .await
        .map_err(|e| e.to_string())?;
    switch_to_account(&account).map_err(|e| e.to_string())?;
    set_active_account(account_id).map_err(|e| e.to_string())?;
    touch_account(account_id).map_err(|e| e.to_string())?;
    run_post_switch_hook(&account);
    Ok(())
}

fn synchronized_export_store() -> anyhow::Result<AccountsStore> {
    update_accounts(|store| {
        if let Some(auth) = read_current_auth()? {
            sync_active_account_tokens(store, &auth);
        }
        Ok(store.clone())
    })
}

fn merge_import(imported: AccountsStore) -> Result<ImportAccountsSummary, String> {
    update_accounts(|current| {
        let (merged, summary) = merge_accounts_store(current.clone(), imported);
        *current = merged;
        Ok(summary)
    })
    .map_err(|e| e.to_string())
}

/// Execute generic post-switch hooks located in ~/.codex-switcher/hooks/
pub(crate) fn run_post_switch_hook(account: &StoredAccount) {
    let Ok(config_dir) = crate::auth::storage::get_config_dir() else {
        return;
    };
    let hooks_dir = config_dir.join("hooks");
    if !hooks_dir.is_dir() {
        return;
    }

    let account_id = &account.id;
    let name = &account.name;
    let email = account.email.as_deref().unwrap_or(name);
    let plan = account.plan_type.as_deref().unwrap_or("unknown");

    #[cfg(windows)]
    {
        let cmd_hook = hooks_dir.join("post-switch.cmd");
        let bat_hook = hooks_dir.join("post-switch.bat");
        let ps1_hook = hooks_dir.join("post-switch.ps1");

        let (executable, args) = if cmd_hook.is_file() {
            (
                "cmd.exe",
                vec![
                    "/c".to_string(),
                    cmd_hook.to_string_lossy().to_string(),
                    account_id.clone(),
                    name.clone(),
                    email.to_string(),
                ],
            )
        } else if bat_hook.is_file() {
            (
                "cmd.exe",
                vec![
                    "/c".to_string(),
                    bat_hook.to_string_lossy().to_string(),
                    account_id.clone(),
                    name.clone(),
                    email.to_string(),
                ],
            )
        } else if ps1_hook.is_file() {
            (
                "powershell.exe",
                vec![
                    "-NoProfile".to_string(),
                    "-ExecutionPolicy".to_string(),
                    "Bypass".to_string(),
                    "-File".to_string(),
                    ps1_hook.to_string_lossy().to_string(),
                    account_id.clone(),
                    name.clone(),
                    email.to_string(),
                ],
            )
        } else {
            return;
        };

        let mut cmd = std::process::Command::new(executable);
        cmd.args(&args)
            .creation_flags(CREATE_NO_WINDOW)
            .env("CODEX_SWITCHER_ACCOUNT_ID", account_id)
            .env("CODEX_SWITCHER_ACCOUNT_NAME", name)
            .env("CODEX_SWITCHER_ACCOUNT_EMAIL", email)
            .env("CODEX_SWITCHER_PLAN_TYPE", plan);

        let _ = cmd.spawn();
    }

    #[cfg(not(windows))]
    {
        let sh_hook = hooks_dir.join("post-switch.sh");
        let hook = if sh_hook.is_file() {
            sh_hook
        } else {
            hooks_dir.join("post-switch")
        };

        if hook.is_file() {
            let _ = std::process::Command::new(&hook)
                .args([account_id, name, email])
                .env("CODEX_SWITCHER_ACCOUNT_ID", account_id)
                .env("CODEX_SWITCHER_ACCOUNT_NAME", name)
                .env("CODEX_SWITCHER_ACCOUNT_EMAIL", email)
                .env("CODEX_SWITCHER_PLAN_TYPE", plan)
                .spawn();
        }
    }
}

/// Remove an account
#[tauri::command]
pub async fn delete_account(account_id: String) -> Result<(), String> {
    remove_account(&account_id).map_err(|e| e.to_string())?;
    Ok(())
}

/// Rename an account
#[tauri::command]
pub async fn rename_account(account_id: String, new_name: String) -> Result<(), String> {
    crate::auth::storage::update_account_metadata(&account_id, Some(new_name), None, None, None)
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Export minimal account config as a compact text string.
/// For ChatGPT accounts, only refresh token is exported.
#[tauri::command]
pub async fn export_accounts_slim_text() -> Result<String, String> {
    let store = synchronized_export_store().map_err(|e| e.to_string())?;
    encode_slim_payload_from_store(&store).map_err(|e| e.to_string())
}

/// Import minimal account config from a compact text string, skipping existing accounts.
#[tauri::command]
pub async fn import_accounts_slim_text(payload: String) -> Result<ImportAccountsSummary, String> {
    let slim_payload = decode_slim_payload(&payload).map_err(|e| format!("{e:#}"))?;
    let total_in_payload = slim_payload.accounts.len();

    let current = load_accounts().map_err(|e| e.to_string())?;
    let existing_names: HashSet<String> = current.accounts.iter().map(|a| a.name.clone()).collect();

    let imported = build_store_from_slim_payload(slim_payload, &existing_names)
        .await
        .map_err(|e| {
            format!(
                "{e:#}\nHint: Slim import needs network access to refresh ChatGPT tokens. You can use Full encrypted file import when offline."
            )
        })?;
    validate_imported_store(&imported).map_err(|e| format!("{e:#}"))?;

    let summary = merge_import(imported)?;
    Ok(ImportAccountsSummary {
        total_in_payload,
        imported_count: summary.imported_count,
        skipped_count: total_in_payload.saturating_sub(summary.imported_count),
    })
}

/// Export full account config as an encrypted file.
#[tauri::command]
pub async fn export_accounts_full_encrypted_file(
    path: String,
    passphrase: String,
) -> Result<(), String> {
    let store = synchronized_export_store().map_err(|e| e.to_string())?;
    let encrypted = encode_full_encrypted_store(&store, &passphrase).map_err(|e| e.to_string())?;
    write_encrypted_file(&path, &encrypted).map_err(|e| e.to_string())?;
    Ok(())
}

/// Export full account config as encrypted bytes for browser clients.
pub async fn export_accounts_full_encrypted_bytes(passphrase: String) -> Result<Vec<u8>, String> {
    let store = synchronized_export_store().map_err(|e| e.to_string())?;
    encode_full_encrypted_store(&store, &passphrase).map_err(|e| e.to_string())
}

/// Import full account config from an encrypted file, skipping existing accounts.
#[tauri::command]
pub async fn import_accounts_full_encrypted_file(
    path: String,
    passphrase: String,
) -> Result<ImportAccountsSummary, String> {
    let encrypted = read_encrypted_file(&path).map_err(|e| e.to_string())?;
    let imported =
        decode_full_encrypted_store(&encrypted, &passphrase).map_err(|e| e.to_string())?;
    validate_imported_store(&imported).map_err(|e| e.to_string())?;

    let summary = merge_import(imported)?;
    Ok(summary)
}

/// Import full account config from encrypted bytes uploaded through the browser UI.
pub async fn import_accounts_full_encrypted_bytes(
    bytes: Vec<u8>,
    passphrase: String,
) -> Result<ImportAccountsSummary, String> {
    let imported = decode_full_encrypted_store(&bytes, &passphrase).map_err(|e| e.to_string())?;
    validate_imported_store(&imported).map_err(|e| e.to_string())?;

    let summary = merge_import(imported)?;
    Ok(summary)
}

fn encode_slim_payload_from_store(store: &AccountsStore) -> anyhow::Result<String> {
    let active_name = store.active_account_id.as_ref().and_then(|active_id| {
        store
            .accounts
            .iter()
            .find(|account| account.id == *active_id)
            .map(|account| account.name.clone())
    });

    let slim_accounts = store
        .accounts
        .iter()
        .map(|account| match &account.auth_data {
            AuthData::ApiKey { key } => SlimAccountPayload {
                name: account.name.clone(),
                auth_type: SLIM_AUTH_API_KEY,
                api_key: Some(key.clone()),
                refresh_token: None,
            },
            AuthData::ChatGPT { refresh_token, .. } => SlimAccountPayload {
                name: account.name.clone(),
                auth_type: SLIM_AUTH_CHATGPT,
                api_key: None,
                refresh_token: Some(refresh_token.clone()),
            },
        })
        .collect();

    let payload = SlimPayload {
        version: SLIM_FORMAT_VERSION,
        active_name,
        accounts: slim_accounts,
    };

    let json = serde_json::to_vec(&payload).context("Failed to serialize slim payload")?;
    let compressed = compress_bytes(&json).context("Failed to compress slim payload")?;

    Ok(format!(
        "{SLIM_EXPORT_PREFIX}{}",
        URL_SAFE_NO_PAD.encode(compressed)
    ))
}

fn decode_slim_payload(payload: &str) -> anyhow::Result<SlimPayload> {
    let normalized: String = payload.chars().filter(|c| !c.is_whitespace()).collect();
    if normalized.is_empty() {
        anyhow::bail!("Import string is empty");
    }

    let encoded = normalized
        .strip_prefix(SLIM_EXPORT_PREFIX)
        .unwrap_or(&normalized);

    let compressed = URL_SAFE_NO_PAD
        .decode(encoded)
        .context("Invalid slim import string (base64 decode failed)")?;

    let decompressed = decompress_bytes_with_limit(&compressed, MAX_IMPORT_JSON_BYTES)
        .context("Invalid slim import string (decompression failed)")?;

    let parsed: SlimPayload = serde_json::from_slice(&decompressed)
        .context("Invalid slim import string (JSON parse failed)")?;

    validate_slim_payload(&parsed)?;
    Ok(parsed)
}

fn validate_slim_payload(payload: &SlimPayload) -> anyhow::Result<()> {
    if payload.version != SLIM_FORMAT_VERSION {
        anyhow::bail!("Unsupported slim payload version: {}", payload.version);
    }

    let mut names = HashSet::new();

    for account in &payload.accounts {
        if account.name.trim().is_empty() {
            anyhow::bail!("Slim import contains an account with empty name");
        }

        if !names.insert(account.name.clone()) {
            anyhow::bail!(
                "Slim import contains duplicate account name: {}",
                account.name
            );
        }

        match account.auth_type {
            SLIM_AUTH_API_KEY => {
                if account
                    .api_key
                    .as_ref()
                    .map_or(true, |key| key.trim().is_empty())
                {
                    anyhow::bail!("API key is missing for account {}", account.name);
                }
            }
            SLIM_AUTH_CHATGPT => {
                if account
                    .refresh_token
                    .as_ref()
                    .map_or(true, |token| token.trim().is_empty())
                {
                    anyhow::bail!("Refresh token is missing for account {}", account.name);
                }
            }
            _ => {
                anyhow::bail!(
                    "Unsupported auth type {} for account {}",
                    account.auth_type,
                    account.name
                );
            }
        }
    }

    if let Some(active_name) = &payload.active_name {
        if !names.contains(active_name) {
            anyhow::bail!("Slim import references missing active account: {active_name}");
        }
    }

    Ok(())
}

async fn build_store_from_slim_payload(
    payload: SlimPayload,
    existing_names: &HashSet<String>,
) -> anyhow::Result<AccountsStore> {
    let active_name = payload.active_name;
    let import_candidates: Vec<SlimAccountPayload> = payload
        .accounts
        .into_iter()
        .filter(|entry| !existing_names.contains(&entry.name))
        .collect();

    let accounts = restore_slim_accounts(import_candidates).await?;
    let mut active_account_id = None;

    if let Some(active) = active_name {
        active_account_id = accounts
            .iter()
            .find(|account| account.name == active)
            .map(|account| account.id.clone());
    }

    if active_account_id.is_none() {
        active_account_id = accounts.first().map(|a| a.id.clone());
    }

    Ok(AccountsStore {
        version: 1,
        accounts,
        active_account_id,
        masked_account_ids: Vec::new(),
    })
}

async fn restore_slim_accounts(
    entries: Vec<SlimAccountPayload>,
) -> anyhow::Result<Vec<StoredAccount>> {
    if entries.is_empty() {
        return Ok(Vec::new());
    }

    let mut restored = Vec::with_capacity(entries.len());
    let mut tasks = stream::iter(entries.into_iter().map(|entry| async move {
        let account_name = entry.name;
        let account = match entry.auth_type {
            SLIM_AUTH_API_KEY => StoredAccount::new_api_key(
                account_name.clone(),
                entry.api_key.context("API key payload is missing")?,
            ),
            SLIM_AUTH_CHATGPT => {
                let refresh_token = entry
                    .refresh_token
                    .context("Refresh token payload is missing")?;
                create_chatgpt_account_from_refresh_token(account_name.clone(), refresh_token)
                    .await
                    .with_context(|| {
                        format!(
                            "Failed to restore ChatGPT account `{account_name}` from refresh token"
                        )
                    })?
            }
            _ => anyhow::bail!("Unsupported auth type in slim payload"),
        };
        Ok::<StoredAccount, anyhow::Error>(account)
    }))
    .buffered(SLIM_IMPORT_CONCURRENCY);

    while let Some(account_result) = tasks.next().await {
        restored.push(account_result?);
    }

    Ok(restored)
}

pub(crate) fn encode_full_encrypted_store<T: serde::Serialize>(
    store: &T,
    passphrase: &str,
) -> anyhow::Result<Vec<u8>> {
    if passphrase.chars().count() < 12 {
        anyhow::bail!("Use a backup password of at least 12 characters");
    }
    let json = serde_json::to_vec(store).context("Failed to serialize account store")?;
    let compressed = compress_bytes(&json).context("Failed to compress account store")?;

    let mut salt = [0u8; FULL_SALT_LEN];
    rand::rng().fill_bytes(&mut salt);

    let mut nonce = [0u8; FULL_NONCE_LEN];
    rand::rng().fill_bytes(&mut nonce);

    let key = derive_encryption_key(passphrase, &salt);
    let cipher = XChaCha20Poly1305::new((&key).into());
    let ciphertext = cipher
        .encrypt(XNonce::from_slice(&nonce), compressed.as_slice())
        .map_err(|_| anyhow::anyhow!("Failed to encrypt account store"))?;

    let mut out = Vec::with_capacity(4 + 1 + FULL_SALT_LEN + FULL_NONCE_LEN + ciphertext.len());
    out.extend_from_slice(FULL_FILE_MAGIC);
    out.push(FULL_FILE_VERSION);
    out.extend_from_slice(&salt);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ciphertext);

    Ok(out)
}

pub(crate) fn decode_full_encrypted_payload<T: serde::de::DeserializeOwned>(
    file_bytes: &[u8],
    passphrase: &str,
) -> anyhow::Result<T> {
    if file_bytes.len() as u64 > MAX_IMPORT_FILE_BYTES {
        anyhow::bail!("Encrypted file is too large");
    }

    let header_len = 4 + 1 + FULL_SALT_LEN + FULL_NONCE_LEN;
    if file_bytes.len() <= header_len {
        anyhow::bail!("Encrypted file is invalid or truncated");
    }

    if &file_bytes[..4] != FULL_FILE_MAGIC {
        anyhow::bail!("Encrypted file header is invalid");
    }

    let version = file_bytes[4];
    if version != 1 && version != FULL_FILE_VERSION {
        anyhow::bail!("Unsupported encrypted file version: {version}");
    }

    let salt_start = 5;
    let nonce_start = salt_start + FULL_SALT_LEN;
    let ciphertext_start = nonce_start + FULL_NONCE_LEN;

    let salt = &file_bytes[salt_start..nonce_start];
    let nonce = &file_bytes[nonce_start..ciphertext_start];
    let ciphertext = &file_bytes[ciphertext_start..];

    let password = if version == 1 {
        LEGACY_FULL_PASSPHRASE
    } else {
        passphrase
    };
    let key = derive_encryption_key(password, salt);
    let cipher = XChaCha20Poly1305::new((&key).into());
    let compressed = cipher
        .decrypt(XNonce::from_slice(nonce), ciphertext)
        .map_err(|_| {
            anyhow::anyhow!("Failed to decrypt file (wrong passphrase or corrupted file)")
        })?;

    let json = decompress_bytes_with_limit(&compressed, MAX_IMPORT_JSON_BYTES)
        .context("Failed to decompress decrypted payload")?;

    let store: T =
        serde_json::from_slice(&json).context("Failed to parse decrypted account payload")?;

    Ok(store)
}

fn derive_encryption_key(passphrase: &str, salt: &[u8]) -> [u8; 32] {
    let mut key = [0u8; 32];
    pbkdf2_hmac::<Sha256>(passphrase.as_bytes(), salt, FULL_KDF_ITERATIONS, &mut key);
    key
}

pub(crate) fn decode_full_encrypted_store(
    file_bytes: &[u8],
    passphrase: &str,
) -> anyhow::Result<AccountsStore> {
    let value: serde_json::Value = decode_full_encrypted_payload(file_bytes, passphrase)?;
    anyhow::ensure!(
        value.get("provider").is_none(),
        "This backup belongs to another provider"
    );
    Ok(serde_json::from_value(value)?)
}

fn compress_bytes(input: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(input)?;
    encoder.finish().context("Failed to finalize compression")
}

fn decompress_bytes_with_limit(input: &[u8], max_bytes: u64) -> anyhow::Result<Vec<u8>> {
    let mut decoder = ZlibDecoder::new(input);
    let mut limited = decoder.by_ref().take(max_bytes + 1);
    let mut decompressed = Vec::new();
    limited.read_to_end(&mut decompressed)?;

    if decompressed.len() as u64 > max_bytes {
        anyhow::bail!("Import data is too large");
    }

    Ok(decompressed)
}

fn write_encrypted_file(path: &str, bytes: &[u8]) -> anyhow::Result<()> {
    fs::write(path, bytes).with_context(|| format!("Failed to write file: {path}"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("Failed to set file permissions: {path}"))?;
    }

    Ok(())
}

fn read_encrypted_file(path: &str) -> anyhow::Result<Vec<u8>> {
    let metadata =
        fs::metadata(path).with_context(|| format!("Failed to read file metadata: {path}"))?;
    if metadata.len() > MAX_IMPORT_FILE_BYTES {
        anyhow::bail!("Encrypted file is too large");
    }

    fs::read(path).with_context(|| format!("Failed to read file: {path}"))
}

fn validate_imported_store(store: &AccountsStore) -> anyhow::Result<()> {
    let mut ids = HashSet::new();
    let mut names = HashSet::new();

    for account in &store.accounts {
        if account.id.trim().is_empty() {
            anyhow::bail!("Import contains an account with empty id");
        }
        if account.name.trim().is_empty() {
            anyhow::bail!("Import contains an account with empty name");
        }
        if !ids.insert(account.id.clone()) {
            anyhow::bail!("Import contains duplicate account id: {}", account.id);
        }
        if !names.insert(account.name.clone()) {
            anyhow::bail!("Import contains duplicate account name: {}", account.name);
        }
    }

    if let Some(active_id) = &store.active_account_id {
        if !ids.contains(active_id) {
            anyhow::bail!("Import references a missing active account: {active_id}");
        }
    }

    Ok(())
}

fn merge_accounts_store(
    mut current: AccountsStore,
    imported: AccountsStore,
) -> (AccountsStore, ImportAccountsSummary) {
    let imported_version = imported.version;

    let total_in_payload = imported.accounts.len();
    let mut imported_count = 0usize;
    let mut existing_ids: HashSet<String> = current.accounts.iter().map(|a| a.id.clone()).collect();
    let mut existing_names: HashSet<String> =
        current.accounts.iter().map(|a| a.name.clone()).collect();

    for account in imported.accounts {
        if existing_ids.contains(&account.id) || existing_names.contains(&account.name) {
            continue;
        }
        existing_ids.insert(account.id.clone());
        existing_names.insert(account.name.clone());
        current.accounts.push(account);
        imported_count += 1;
    }

    current.version = current.version.max(imported_version).max(1);

    // Importing a backup must never imply that its credentials were installed.
    if !current
        .active_account_id
        .as_ref()
        .is_some_and(|id| current.accounts.iter().any(|a| &a.id == id))
    {
        current.active_account_id = None;
    }

    (
        current,
        ImportAccountsSummary {
            total_in_payload,
            imported_count,
            skipped_count: total_in_payload.saturating_sub(imported_count),
        },
    )
}

/// Get the list of masked account IDs
#[tauri::command]
pub async fn get_masked_account_ids() -> Result<Vec<String>, String> {
    crate::auth::storage::get_masked_account_ids().map_err(|e| e.to_string())
}

/// Set the list of masked account IDs
#[tauri::command]
pub async fn set_masked_account_ids(ids: Vec<String>) -> Result<(), String> {
    crate::auth::storage::set_masked_account_ids(ids).map_err(|e| e.to_string())
}

/// Automatically use a verified available account when the active quota is low.
/// Reports credential installation and desktop reopening as separate outcomes.
pub async fn auto_switch_to_best_account(
    threshold_percent: f64,
    observed: Option<crate::types::UsageInfo>,
    cached: std::collections::HashMap<String, crate::types::UsageInfo>,
) -> Result<crate::auto_switch::RotationResult, String> {
    let Ok(_automatic_guard) = AUTO_SWITCH_LOCK.try_lock() else {
        return Err("다른 자동 전환 작업이 진행 중입니다".into());
    };
    let store = load_accounts().map_err(|e| e.to_string())?;
    let active = store
        .accounts
        .iter()
        .find(|a| store.active_account_id.as_deref() == Some(a.id.as_str()))
        .ok_or("현재 인증 파일의 계정을 확인할 수 없습니다")?;
    let active_usage = match observed {
        Some(usage) => usage,
        None => crate::api::usage::get_account_usage(active)
            .await
            .map_err(|e| e.to_string())?,
    };
    let from_name = active.name.clone();
    let observed_at = std::time::Instant::now();
    let preference = crate::auth::load_app_settings()
        .map_err(|e| e.to_string())?
        .auto_switch_plan_preference;
    let result = crate::auto_switch::evaluate_rotation_with_preference(
        active,
        &store.accounts,
        &active_usage,
        threshold_percent,
        preference,
        |candidate| {
            let known = cached.get(&candidate.id).cloned();
            let source = active.id.clone();
            async move {
                crate::auto_switch::record_event(
                    "candidate_query_started",
                    Some(&source),
                    Some(&candidate.id),
                    None,
                    None,
                );
                if let Some(usage) = known.filter(|u| crate::auto_switch::quota_score(u).is_ok()) {
                    crate::auto_switch::record_event(
                        "candidate_cached",
                        Some(&source),
                        Some(&candidate.id),
                        None,
                        Some(&usage),
                    );
                    return Ok(usage);
                }
                let started = std::time::Instant::now();
                let result = crate::api::usage::get_account_usage(&candidate)
                    .await
                    .map_err(|e| e.to_string());
                crate::auto_switch::record_event(
                    "candidate_query_finished",
                    Some(&source),
                    Some(&candidate.id),
                    Some(started.elapsed()),
                    result.as_ref().ok(),
                );
                result
            }
        },
        |active_id, target| activate_auto_switch(active_id, from_name, target, observed_at),
    )
    .await;
    crate::auto_switch::defer_auth_busy(result)
}

async fn activate_auto_switch(
    active_id: String,
    from_name: String,
    target: StoredAccount,
    observed_at: std::time::Instant,
) -> Result<AutoSwitchOutcome, String> {
    let target_id = target.id.clone();
    let to_name = target.name.clone();
    // Serialize the final decision with manual switches and token refreshes.
    crate::auto_switch::record_event(
        "auth_lock_wait",
        Some(&active_id),
        Some(&target_id),
        Some(observed_at.elapsed()),
        None,
    );
    let _auth_guard = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        crate::auth::lock_auth_operation("auto_switch", &active_id),
    )
    .await
    .map_err(|_| crate::auto_switch::AUTH_BUSY_RETRY.to_string())?;
    crate::auto_switch::record_event(
        "auth_file_lock_wait",
        Some(&active_id),
        Some(&target_id),
        Some(observed_at.elapsed()),
        None,
    );
    let _file_guard = crate::auth::lock_auth_operations()
        .await
        .map_err(|e| e.to_string())?;
    crate::auto_switch::record_event(
        "auth_lock_acquired",
        Some(&active_id),
        Some(&target_id),
        Some(observed_at.elapsed()),
        None,
    );
    validate_auto_decision(&active_id, observed_at)?;
    crate::auto_switch::record_event(
        "target_credentials_preparing",
        Some(&active_id),
        Some(&target_id),
        Some(observed_at.elapsed()),
        None,
    );
    // Finish any target token rotation before closing the user's desktop.
    // switch_account_locked will re-read these persisted credentials afterward.
    ensure_chatgpt_tokens_fresh_locked(&target)
        .await
        .map_err(|e| e.to_string())?;
    crate::auto_switch::record_event(
        "target_credentials_ready",
        Some(&active_id),
        Some(&target_id),
        Some(observed_at.elapsed()),
        None,
    );
    if let Err(error) = validate_auto_decision(&active_id, observed_at) {
        crate::auto_switch::record_event(
            "decision_cancelled",
            Some(&active_id),
            Some(&target_id),
            Some(observed_at.elapsed()),
            None,
        );
        return Err(error);
    }
    crate::auto_switch::record_event(
        "desktop_close_started",
        Some(&active_id),
        Some(&target_id),
        Some(observed_at.elapsed()),
        None,
    );
    let (pids, _) = crate::commands::process::find_codex_processes().map_err(|e| e.to_string())?;
    let reopen_token = if !pids.is_empty() {
        // Preserve automatic desktop rotation, but only close roots whose exact
        // desktop identity was captured and can be reopened.
        let result = crate::commands::process::kill_codex_processes(Some(true), Some(true)).await?;
        if !result.failed_pids.is_empty() {
            if let Some(token) = result.reopen_token {
                let _ = crate::commands::reopen_closed_codex_desktop(token).await;
            }
            return Err("Codex did not close; credentials were not switched. Close the remaining Codex processes and retry.".into());
        }
        result.reopen_token
    } else {
        None
    };
    crate::auto_switch::record_event(
        "desktop_closed",
        Some(&active_id),
        Some(&target_id),
        Some(observed_at.elapsed()),
        None,
    );
    if let Err(error) = switch_account_locked(&target_id).await {
        let recovery = if let Some(token) = reopen_token {
            crate::commands::reopen_closed_codex_desktop(token)
                .await
                .err()
        } else {
            None
        };
        return Err(format!(
            "{error}{}",
            recovery
                .map(|e| format!("; reopening failed: {e}"))
                .unwrap_or_default()
        ));
    }
    crate::auto_switch::record_event(
        "credentials_installed",
        Some(&active_id),
        Some(&target_id),
        Some(observed_at.elapsed()),
        None,
    );
    let (desktop_status, warning) = if let Some(token) = reopen_token {
        match crate::commands::reopen_closed_codex_desktop(token).await {
            Ok(()) => ("reopened".to_string(), None),
            Err(error) => ("reopen_failed".to_string(), Some(error)),
        }
    } else {
        ("not_running".to_string(), None)
    };
    crate::auto_switch::record_event(
        &desktop_status,
        Some(&active_id),
        Some(&target_id),
        Some(observed_at.elapsed()),
        None,
    );
    Ok(AutoSwitchOutcome {
        from: from_name,
        to: to_name,
        desktop_status,
        warning,
    })
}

static AUTO_SWITCH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn validate_auto_decision(active_id: &str, observed_at: std::time::Instant) -> Result<(), String> {
    if !crate::auth::load_app_settings()
        .map_err(|e| e.to_string())?
        .auto_switch_enabled
    {
        return Err("자동 전환이 꺼져 있어 전환을 취소했습니다".into());
    }
    if observed_at.elapsed() > crate::auto_switch::USAGE_MAX_AGE {
        return Err("사용량 확인 이후 시간이 지나 다시 조회합니다".into());
    }
    if load_accounts()
        .map_err(|e| e.to_string())?
        .active_account_id
        .as_deref()
        != Some(active_id)
    {
        crate::auto_switch::record_event(
            "cancelled_account_changed",
            Some(active_id),
            None,
            Some(observed_at.elapsed()),
            None,
        );
        return Err("자동 전환 판단 중 현재 계정이 변경되어 다시 확인합니다".into());
    }
    Ok(())
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AutoSwitchOutcome {
    pub from: String,
    pub to: String,
    pub desktop_status: String,
    pub warning: Option<String>,
}

/// Trigger an immediate check and auto-switch if quota is low (called from UI or testing)
#[tauri::command]
pub async fn trigger_auto_switch() -> Result<Option<String>, String> {
    let settings = crate::auth::load_app_settings().unwrap_or_default();
    if !settings.auto_switch_enabled {
        return Ok(None);
    }
    match auto_switch_to_best_account(
        settings.auto_switch_threshold_percent,
        None,
        Default::default(),
    )
    .await?
    {
        crate::auto_switch::RotationResult::Switched(outcome) => Ok(Some(format!(
            "Credential file changed from {} to {}; desktop: {}{}",
            outcome.from,
            outcome.to,
            outcome.desktop_status,
            outcome
                .warning
                .map(|e| format!("; {e}"))
                .unwrap_or_default()
        ))),
        crate::auto_switch::RotationResult::Healthy => Ok(None),
        crate::auto_switch::RotationResult::Deferred(reason) => Err(reason),
        crate::auto_switch::RotationResult::NoCandidate(reason) => Err(reason),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn encrypted_backups_require_a_user_password_and_reject_wrong_passwords() {
        let mut store = AccountsStore::default();
        store.accounts.push(StoredAccount::new_api_key(
            "synthetic".into(),
            "fake".into(),
        ));
        assert!(encode_full_encrypted_store(&store, "short").is_err());
        let bytes = encode_full_encrypted_store(&store, "correct-test-password").unwrap();
        assert_eq!(bytes[4], 2);
        assert!(decode_full_encrypted_store(&bytes, "wrong-test-password").is_err());
        assert_eq!(
            decode_full_encrypted_store(&bytes, "correct-test-password")
                .unwrap()
                .accounts
                .len(),
            1
        );
        // Existing v1 backups remain importable, but exports always use v2.
        let mut legacy = encode_full_encrypted_store(&store, LEGACY_FULL_PASSPHRASE).unwrap();
        legacy[4] = 1;
        assert_eq!(
            decode_full_encrypted_store(&legacy, "")
                .unwrap()
                .accounts
                .len(),
            1
        );
    }
    #[test]
    fn importing_does_not_select_an_uninstalled_account_or_drop_current_changes() {
        let mut current = AccountsStore::default();
        current.accounts.push(StoredAccount::new_api_key(
            "already-added".into(),
            "fake-a".into(),
        ));
        let mut imported = AccountsStore::default();
        let account = StoredAccount::new_api_key("imported".into(), "fake-b".into());
        imported.active_account_id = Some(account.id.clone());
        imported.accounts.push(account);
        let (merged, summary) = merge_accounts_store(current, imported);
        assert_eq!(merged.accounts.len(), 2);
        assert_eq!(summary.imported_count, 1);
        assert!(merged.active_account_id.is_none());
    }
}
