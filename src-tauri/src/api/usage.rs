//! Usage API client for fetching rate limits and credits

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use futures::{stream, StreamExt};
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION, USER_AGENT},
    StatusCode,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;

use crate::auth::{
    chatgpt_account_for_usage, ensure_chatgpt_tokens_fresh, refresh_chatgpt_tokens,
    refresh_chatgpt_tokens_for_usage,
};
use crate::types::{
    AuthData, CreditStatusDetails, RateLimitDetails, RateLimitStatusPayload, RateLimitWindow,
    StoredAccount, UsageInfo,
};

const CHATGPT_BACKEND_API: &str = "https://chatgpt.com/backend-api";
const CHATGPT_ACCOUNTS_CHECK_API: &str =
    "https://chatgpt.com/backend-api/accounts/check/v4-2023-04-27";
const CHATGPT_CODEX_RESPONSES_API: &str = "https://chatgpt.com/backend-api/codex/responses";
const OPENAI_API: &str = "https://api.openai.com/v1";
const CHATGPT_ORIGIN: &str = "https://chatgpt.com";

/// A browser-like User-Agent to avoid Cloudflare bot detection.
/// Matches a recent Chrome on Windows, the same platform Codex CLI runs from.
const BROWSER_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
     AppleWebKit/537.36 (KHTML, like Gecko) \
     Chrome/136.0.0.0 Safari/537.36";
const SESSION_WINDOW_SECONDS: i32 = 5 * 60 * 60;
const WEEKLY_WINDOW_SECONDS: i32 = 7 * 24 * 60 * 60;

#[derive(Debug, Clone)]
pub struct ChatGptAccountMetadata {
    pub plan_type: Option<String>,
    pub subscription_expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize)]
struct AccountsCheckResponse {
    #[serde(default)]
    accounts: HashMap<String, AccountsCheckEntry>,
}

#[derive(Debug, Deserialize)]
struct AccountsCheckEntry {
    #[serde(default)]
    account: Option<AccountsCheckAccount>,
    #[serde(default)]
    entitlement: Option<AccountsCheckEntitlement>,
}

#[derive(Debug, Deserialize)]
struct AccountsCheckAccount {
    #[serde(default)]
    plan_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AccountsCheckEntitlement {
    #[serde(default)]
    expires_at: Option<DateTime<Utc>>,
}

/// Get usage information for an account
pub async fn get_account_usage(account: &StoredAccount) -> Result<UsageInfo> {
    match &account.auth_data {
        AuthData::ApiKey { .. } => Ok(UsageInfo {
            account_id: account.id.clone(),
            plan_type: Some("api_key".to_string()),
            primary_used_percent: None,
            primary_window_minutes: None,
            primary_resets_at: None,
            secondary_used_percent: None,
            secondary_window_minutes: None,
            secondary_resets_at: None,
            has_credits: None,
            unlimited_credits: None,
            credits_balance: None,
            error: Some("Usage info not available for API key accounts".to_string()),
        }),
        AuthData::ChatGPT { .. } => get_usage_with_chatgpt_auth(account).await,
    }
}

/// Send a minimal authenticated request to warm up account traffic paths.
pub async fn warmup_account(account: &StoredAccount) -> Result<()> {
    match &account.auth_data {
        AuthData::ApiKey { key } => warmup_with_api_key(key).await,
        AuthData::ChatGPT { .. } => warmup_with_chatgpt_auth(account).await,
    }
}

pub async fn fetch_chatgpt_account_metadata(
    account: &StoredAccount,
) -> Result<ChatGptAccountMetadata> {
    let (access_token, chatgpt_account_id) = extract_chatgpt_auth(account)?;
    let response =
        send_chatgpt_get_request(CHATGPT_ACCOUNTS_CHECK_API, access_token, chatgpt_account_id)
            .await?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        if status == StatusCode::FORBIDDEN {
            anyhow::bail!(
                "Accounts check API returned 403 Forbidden. \
                 The request was likely blocked by Cloudflare bot detection. \
                 This is a transient network issue — please try again in a moment."
            );
        }
        anyhow::bail!("Accounts check API error: {status} - {body}");
    }

    let payload: AccountsCheckResponse = response
        .json()
        .await
        .context("Failed to parse accounts check response")?;

    let selected_entry = chatgpt_account_id
        .and_then(|account_id| payload.accounts.get(account_id))
        .or_else(|| payload.accounts.get("default"))
        .or_else(|| payload.accounts.values().next())
        .context("Accounts check response did not include an account entry")?;

    Ok(ChatGptAccountMetadata {
        plan_type: selected_entry
            .account
            .as_ref()
            .and_then(|account| account.plan_type.clone()),
        subscription_expires_at: selected_entry
            .entitlement
            .as_ref()
            .and_then(|entitlement| entitlement.expires_at),
    })
}

async fn get_usage_with_chatgpt_auth(account: &StoredAccount) -> Result<UsageInfo> {
    get_usage_with_chatgpt_auth_at(account, &format!("{CHATGPT_BACKEND_API}/wham/usage")).await
}

async fn get_usage_with_chatgpt_auth_at(account: &StoredAccount, url: &str) -> Result<UsageInfo> {
    let fresh_account = chatgpt_account_for_usage(account)?;
    let (access_token, chatgpt_account_id) = extract_chatgpt_auth(&fresh_account)?;

    let response = send_chatgpt_get_request(url, access_token, chatgpt_account_id).await?;

    // 401 means the token is genuinely expired — refresh and retry once.
    // 403 is a Cloudflare challenge or permissions error; refreshing the token
    // would burn the refresh token unnecessarily (refresh_token_reused error).
    if response.status() == StatusCode::UNAUTHORIZED {
        println!(
            "[Usage] Unauthorized for account {}, refreshing token and retrying once",
            fresh_account.name
        );
        let refreshed_account = refresh_chatgpt_tokens_for_usage(&fresh_account).await?;
        let (retry_token, retry_account_id) = extract_chatgpt_auth(&refreshed_account)?;
        let retry_response = send_chatgpt_get_request(url, retry_token, retry_account_id).await?;
        return parse_usage_response(
            &refreshed_account.id,
            &refreshed_account.name,
            retry_response,
        )
        .await;
    }

    parse_usage_response(&fresh_account.id, &fresh_account.name, response).await
}

async fn parse_usage_response(
    account_id: &str,
    account_name: &str,
    response: reqwest::Response,
) -> Result<UsageInfo> {
    let status = response.status();

    if !status.is_success() {
        return Ok(UsageInfo::error(
            account_id.to_string(),
            format!("API error: {status}"),
        ));
    }

    let body_text = response
        .text()
        .await
        .context("Failed to read response body")?;

    let payload: RateLimitStatusPayload =
        serde_json::from_str(&body_text).context("Failed to parse usage response")?;

    let usage = convert_payload_to_usage_info(account_id, payload);
    println!("[Usage] Refreshed account: {account_name}");

    Ok(usage)
}

async fn warmup_with_chatgpt_auth(account: &StoredAccount) -> Result<()> {
    let fresh_account = ensure_chatgpt_tokens_fresh(account).await?;
    let (access_token, chatgpt_account_id) = extract_chatgpt_auth(&fresh_account)?;

    let mut response = send_chatgpt_warmup_request(access_token, chatgpt_account_id, true).await?;

    // Only refresh tokens on 401 (genuinely expired). A 403 is a Cloudflare
    // challenge and does not indicate stale tokens — refreshing on 403 burns
    // the refresh token and causes a refresh_token_reused error on the next call.
    if response.status() == StatusCode::UNAUTHORIZED {
        println!(
            "[Warmup] Unauthorized for account {}, refreshing token and retrying once",
            fresh_account.name
        );
        let refreshed_account = refresh_chatgpt_tokens(&fresh_account).await?;
        let (retry_token, retry_account_id) = extract_chatgpt_auth(&refreshed_account)?;
        response = send_chatgpt_warmup_request(retry_token, retry_account_id, true).await?;
    }

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        println!("[Warmup] ChatGPT warm-up error response: {body}");
        anyhow::bail!("ChatGPT warm-up failed with status {status}");
    }

    let body = response.text().await.unwrap_or_default();
    log_warmup_response("ChatGPT", &body, true);

    Ok(())
}

async fn warmup_with_api_key(api_key: &str) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;
    let payload = build_warmup_payload(false, true);
    let response = client
        .post(format!("{OPENAI_API}/responses"))
        .header(USER_AGENT, BROWSER_USER_AGENT)
        .header(AUTHORIZATION, format!("Bearer {api_key}"))
        .json(&payload)
        .send()
        .await
        .context("Failed to send API key warm-up request")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        println!("[Warmup] API key warm-up error response: {body}");
        anyhow::bail!("API key warm-up failed with status {status}");
    }

    let body = response.text().await.unwrap_or_default();
    log_warmup_response("API key", &body, false);

    Ok(())
}

fn build_warmup_payload(stream: bool, include_max_output_tokens: bool) -> serde_json::Value {
    let mut payload = json!({
        "model": "gpt-6-luna",
        "instructions": "You are Codex.",
        "input": [
            {
                "type": "message",
                "role": "user",
                "content": [
                    {
                        "type": "input_text",
                        "text": "Thanks"
                    }
                ]
            }
        ],
        "tools": [],
        "tool_choice": "auto",
        "parallel_tool_calls": false,
        "reasoning": {
            "effort": "low"
        },
        "store": false,
        "stream": stream
    });

    if include_max_output_tokens {
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("max_output_tokens".to_string(), json!(1));
        }
    }

    payload
}

fn build_chatgpt_headers(
    access_token: &str,
    chatgpt_account_id: Option<&str>,
) -> Result<HeaderMap> {
    use reqwest::header::{ACCEPT, ACCEPT_LANGUAGE, ORIGIN, REFERER};

    let mut headers = HeaderMap::new();

    // Use a real browser User-Agent so Cloudflare does not flag the request as
    // a bot.  The codex-cli/1.0.0 string was the previous value and it
    // reliably triggers the CF "Enable JavaScript and cookies" challenge.
    headers.insert(USER_AGENT, HeaderValue::from_static(BROWSER_USER_AGENT));

    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {access_token}")).context("Invalid access token")?,
    );

    // Browser-like headers that Cloudflare uses to distinguish real browsers
    // from automated HTTP clients.
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/json, text/plain, */*"),
    );
    headers.insert(ACCEPT_LANGUAGE, HeaderValue::from_static("en-US,en;q=0.9"));
    headers.insert(ORIGIN, HeaderValue::from_static(CHATGPT_ORIGIN));
    headers.insert(REFERER, HeaderValue::from_static(CHATGPT_ORIGIN));

    // Fetch metadata headers sent by the browser
    if let Ok(name) = HeaderName::from_bytes(b"sec-fetch-dest") {
        headers.insert(name, HeaderValue::from_static("empty"));
    }
    if let Ok(name) = HeaderName::from_bytes(b"sec-fetch-mode") {
        headers.insert(name, HeaderValue::from_static("cors"));
    }
    if let Ok(name) = HeaderName::from_bytes(b"sec-fetch-site") {
        headers.insert(name, HeaderValue::from_static("same-origin"));
    }

    if let Some(acc_id) = chatgpt_account_id {
        if let Ok(header_name) = HeaderName::from_bytes(b"chatgpt-account-id") {
            if let Ok(header_value) = HeaderValue::from_str(acc_id) {
                headers.insert(header_name, header_value);
            }
        }
    }

    Ok(headers)
}

fn extract_chatgpt_auth(account: &StoredAccount) -> Result<(&str, Option<&str>)> {
    match &account.auth_data {
        AuthData::ChatGPT {
            access_token,
            account_id,
            ..
        } => Ok((access_token.as_str(), account_id.as_deref())),
        AuthData::ApiKey { .. } => anyhow::bail!("Account is not using ChatGPT OAuth"),
    }
}

async fn send_chatgpt_get_request(
    url: &str,
    access_token: &str,
    chatgpt_account_id: Option<&str>,
) -> Result<reqwest::Response> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;
    let headers = build_chatgpt_headers(access_token, chatgpt_account_id)?;

    client
        .get(url)
        .headers(headers)
        .send()
        .await
        .with_context(|| format!("Failed to send GET request to {url}"))
}

async fn send_chatgpt_warmup_request(
    access_token: &str,
    chatgpt_account_id: Option<&str>,
    stream: bool,
) -> Result<reqwest::Response> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;
    let headers = build_chatgpt_headers(access_token, chatgpt_account_id)?;
    let payload = build_warmup_payload(stream, false);

    client
        .post(CHATGPT_CODEX_RESPONSES_API)
        .headers(headers)
        .json(&payload)
        .send()
        .await
        .context("Failed to send ChatGPT warm-up request")
}

#[cfg(test)]
mod usage_lock_regression {
    use super::*;
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct IsolatedHome {
        path: std::path::PathBuf,
        previous: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }
    impl IsolatedHome {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("switcher-usage-{}", uuid::Uuid::new_v4()));
            let mut previous = Vec::new();
            for (key, suffix) in [("CODEX_HOME", "codex"), ("CODEX_SWITCHER_HOME", "switcher")] {
                previous.push((key, std::env::var_os(key)));
                std::env::set_var(key, path.join(suffix));
            }
            Self { path, previous }
        }
    }
    impl Drop for IsolatedHome {
        fn drop(&mut self) {
            for (key, value) in &self.previous {
                if let Some(value) = value {
                    std::env::set_var(key, value);
                } else {
                    std::env::remove_var(key);
                }
            }
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn id_token(account: &str) -> String {
        let json = serde_json::json!({"exp":1,"https://api.openai.com/auth":{"chatgpt_account_id":account}});
        format!(
            "header.{}.signature",
            URL_SAFE_NO_PAD.encode(json.to_string())
        )
    }

    #[tokio::test]
    async fn auth_contention_valid_tokens_do_not_join_the_mutation_queue() {
        let _sandbox = IsolatedHome::new();
        // Synthetic JWT with exp=4000000000; no OAuth or external request.
        let token = "header.eyJleHAiOjQwMDAwMDAwMDB9.signature";
        let account = crate::auth::add_account(StoredAccount::new_chatgpt(
            "Synthetic".into(),
            None,
            None,
            None,
            token.into(),
            token.into(),
            "synthetic-refresh".into(),
            Some("synthetic-workspace".into()),
        ))
        .unwrap();
        crate::auth::switch_to_account(&account).unwrap();
        let path = crate::auth::get_accounts_file().unwrap();
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let _operation = crate::auth::AUTH_OPERATION_LOCK.lock().await;
        let _file_operation = crate::auth::lock_auth_operations().await.unwrap();
        let current = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            crate::auth::ensure_chatgpt_tokens_fresh(&account),
        )
        .await
        .expect("valid tokens must not queue behind a manual switch or unrelated refresh")
        .unwrap();
        assert_eq!(current.id, account.id);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified
        );
        // Codex may rotate tokens outside the Switcher's mutation gate. The
        // fast path must still save that replacement, respecting pending sync.
        let mut live = crate::auth::read_current_auth().unwrap().unwrap();
        live.tokens.as_mut().unwrap().refresh_token = "native-rotated-refresh".into();
        let live_path = crate::auth::get_codex_auth_file().unwrap();
        crate::auth::persistence::atomic_write(&live_path, &serde_json::to_vec(&live).unwrap())
            .unwrap();
        let current = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            crate::auth::ensure_chatgpt_tokens_fresh(&account),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(crate::auth::credentials_match_auth(&current, &live));
        assert!(crate::auth::credentials_match_auth(
            &crate::auth::get_account(&account.id).unwrap().unwrap(),
            &live
        ));
    }

    #[tokio::test]
    async fn exhausted_usage_does_not_wait_for_refresh_or_switch_locks() {
        let _sandbox = IsolatedHome::new();
        let account = crate::auth::add_account(StoredAccount::new_chatgpt(
            "Synthetic Plus".into(),
            None,
            None,
            None,
            id_token("synthetic-plus"),
            "synthetic-access".into(),
            "synthetic-refresh".into(),
            Some("synthetic-plus".into()),
        ))
        .unwrap();
        crate::auth::switch_to_account(&account).unwrap();
        let stored_before = std::fs::read(crate::auth::get_accounts_file().unwrap()).unwrap();
        let auth_before = std::fs::read(crate::auth::get_codex_auth_file().unwrap()).unwrap();
        // Both the desktop mutex and another process's file lock remain held.
        let _operation = crate::auth::AUTH_OPERATION_LOCK.lock().await;
        let _file_operation = crate::auth::lock_auth_operations().await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/usage", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 8192];
            let size = socket.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..size]).to_lowercase();
            assert!(request.contains("authorization: bearer synthetic-access"));
            let body = r#"{"plan_type":"plus","rate_limit":{"allowed":false,"limit_reached":true,"primary_window":{"used_percent":100,"limit_window_seconds":18000},"secondary_window":{"used_percent":67,"limit_window_seconds":604800}}}"#;
            let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            get_usage_with_chatgpt_auth_at(&account, &url),
        )
        .await;
        if result.is_err() {
            server.abort();
        }
        let usage = result
            .expect("quota detection must not wait behind token refresh or manual switch")
            .unwrap();
        server.await.unwrap();
        assert_eq!(usage.primary_used_percent, Some(100.0));
        assert_eq!(crate::auto_switch::quota_score(&usage).unwrap(), 100.0);
        assert_eq!(
            std::fs::read(crate::auth::get_accounts_file().unwrap()).unwrap(),
            stored_before
        );
        assert_eq!(
            std::fs::read(crate::auth::get_codex_auth_file().unwrap()).unwrap(),
            auth_before
        );
        drop(_file_operation);
        drop(_operation);
        // Continue from the actual HTTP observation through the production
        // policy into a verified credential-file write in this isolated home.
        let pro = crate::auth::add_account(StoredAccount::new_chatgpt(
            "Synthetic Pro".into(),
            None,
            None,
            None,
            id_token("synthetic-pro"),
            "synthetic-pro-access".into(),
            "synthetic-pro-refresh".into(),
            Some("synthetic-pro".into()),
        ))
        .unwrap();
        let pro_id = pro.id.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/usage", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 8192];
            let size = socket.read(&mut request).await.unwrap();
            assert!(String::from_utf8_lossy(&request[..size])
                .to_lowercase()
                .contains("authorization: bearer synthetic-pro-access"));
            let body = r#"{"plan_type":"prolite","rate_limit":{"allowed":true,"limit_reached":false,"primary_window":{"used_percent":53,"limit_window_seconds":604800}}}"#;
            let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let result = crate::auto_switch::evaluate_rotation(
            &account,
            &[account.clone(), pro],
            &usage,
            98.0,
            |candidate| {
                let url = url.clone();
                async move {
                    get_usage_with_chatgpt_auth_at(&candidate, &url)
                        .await
                        .map_err(|e| e.to_string())
                }
            },
            |_, target| async move {
                crate::auth::switch_to_account(&target).map_err(|e| e.to_string())?;
                Ok(crate::commands::account::AutoSwitchOutcome {
                    from: "Synthetic Plus".into(),
                    to: target.name,
                    desktop_status: "isolated_no_desktop".into(),
                    warning: None,
                })
            },
        )
        .await
        .unwrap();
        server.await.unwrap();
        assert!(matches!(
            result,
            crate::auto_switch::RotationResult::Switched(_)
        ));
        let store = crate::auth::load_accounts().unwrap();
        assert_eq!(store.active_account_id, Some(pro_id.clone()));
        assert!(crate::auth::credentials_match_auth(
            store.accounts.iter().find(|a| a.id == pro_id).unwrap(),
            &crate::auth::read_current_auth().unwrap().unwrap()
        ));
    }
}

fn log_warmup_response(source: &str, body: &str, is_sse: bool) {
    if body.trim().is_empty() {
        println!("[Warmup] {source} warm-up response was empty");
        return;
    }

    let preview = truncate_text(body, 300);
    println!("[Warmup] {source} warm-up response preview: {preview}");

    let extracted = if is_sse {
        extract_text_from_sse(body)
    } else {
        extract_text_from_json(body)
    };

    if let Some(message) = extracted {
        let message_preview = truncate_text(&message, 200);
        println!("[Warmup] {source} warm-up message: {message_preview}");
    }
}

fn truncate_text(text: &str, max_len: usize) -> String {
    if text.len() <= max_len {
        return text.to_string();
    }
    let mut boundary = max_len;
    while !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    let mut out = text[..boundary].to_string();
    out.push_str("...");
    out
}

#[test]
fn truncates_unicode_without_splitting_a_character() {
    assert_eq!(truncate_text("한글🙂abc", 1), "...");
    assert_eq!(truncate_text("한글🙂abc", 7), "한글...");
    assert_eq!(truncate_text("éclair", 2), "é...");
    assert_eq!(truncate_text("한글", 6), "한글");
}

fn extract_text_from_sse(body: &str) -> Option<String> {
    let mut last_text: Option<String> = None;
    for line in body.lines() {
        let line = line.trim();
        if !line.starts_with("data:") {
            continue;
        }
        let data = line.trim_start_matches("data:").trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(data) {
            if let Some(text) = extract_last_text_from_value(&value) {
                last_text = Some(text);
            }
        }
    }
    last_text.filter(|text| !text.trim().is_empty())
}

fn extract_text_from_json(body: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(body).ok()?;
    extract_last_text_from_value(&value)
}

fn extract_last_text_from_value(value: &Value) -> Option<String> {
    let mut last: Option<String> = None;
    collect_last_text(value, &mut last);
    last
}

fn collect_last_text(value: &Value, last: &mut Option<String>) {
    match value {
        Value::Object(map) => {
            for (key, val) in map {
                if matches!(key.as_str(), "text" | "delta" | "output_text") {
                    if let Value::String(text) = val {
                        if !text.is_empty() {
                            *last = Some(text.clone());
                        }
                    }
                }
                collect_last_text(val, last);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_last_text(item, last);
            }
        }
        _ => {}
    }
}

/// Convert API response to UsageInfo
fn convert_payload_to_usage_info(account_id: &str, payload: RateLimitStatusPayload) -> UsageInfo {
    let (primary, secondary) = extract_rate_limits(payload.rate_limit);
    let credits = extract_credits(payload.credits);

    UsageInfo {
        account_id: account_id.to_string(),
        plan_type: Some(payload.plan_type),
        primary_used_percent: primary.as_ref().map(|w| w.used_percent),
        primary_window_minutes: primary
            .as_ref()
            .and_then(|w| w.limit_window_seconds)
            .map(|s| (i64::from(s) + 59) / 60),
        primary_resets_at: primary.as_ref().and_then(|w| w.reset_at),
        secondary_used_percent: secondary.as_ref().map(|w| w.used_percent),
        secondary_window_minutes: secondary
            .as_ref()
            .and_then(|w| w.limit_window_seconds)
            .map(|s| (i64::from(s) + 59) / 60),
        secondary_resets_at: secondary.as_ref().and_then(|w| w.reset_at),
        has_credits: credits.as_ref().map(|c| c.has_credits),
        unlimited_credits: credits.as_ref().map(|c| c.unlimited),
        credits_balance: credits.and_then(|c| c.balance),
        error: None,
    }
}

fn extract_rate_limits(
    rate_limit: Option<RateLimitDetails>,
) -> (Option<RateLimitWindow>, Option<RateLimitWindow>) {
    let Some(details) = rate_limit else {
        return (None, None);
    };

    match (details.primary_window, details.secondary_window) {
        // The backend can temporarily omit the 5-hour window and promote the
        // weekly window into primary_window. Keep UsageInfo semantic so every
        // consumer still treats primary as session and secondary as weekly.
        (Some(primary), None) if is_weekly_window(&primary) => (None, Some(primary)),
        (None, Some(secondary)) if is_session_window(&secondary) => (Some(secondary), None),
        (Some(primary), Some(secondary))
            if is_weekly_window(&primary) && is_session_window(&secondary) =>
        {
            (Some(secondary), Some(primary))
        }
        (primary, secondary) => (primary, secondary),
    }
}

fn is_session_window(window: &RateLimitWindow) -> bool {
    window.limit_window_seconds == Some(SESSION_WINDOW_SECONDS)
}

fn is_weekly_window(window: &RateLimitWindow) -> bool {
    window.limit_window_seconds == Some(WEEKLY_WINDOW_SECONDS)
}

fn extract_credits(credits: Option<CreditStatusDetails>) -> Option<CreditStatusDetails> {
    credits
}

/// Refresh all account usage
pub async fn refresh_all_usage(accounts: &[StoredAccount]) -> Vec<UsageInfo> {
    let concurrency = accounts.len().min(10).max(1);
    let results: Vec<UsageInfo> = stream::iter(accounts.iter().cloned())
        .map(|account| async move {
            match get_account_usage(&account).await {
                Ok(info) => info,
                Err(e) => {
                    println!("[Usage] Error for {}: {}", account.name, e);
                    UsageInfo::error(account.id.clone(), e.to_string())
                }
            }
        })
        .buffer_unordered(concurrency)
        .collect()
        .await;

    results
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rate_limit_window(used_percent: f64, window_seconds: i32) -> RateLimitWindow {
        RateLimitWindow {
            used_percent,
            limit_window_seconds: Some(window_seconds),
            reset_at: Some(1_800_000_000),
        }
    }

    #[test]
    fn keeps_legacy_session_and_weekly_windows_in_place() {
        let (session, weekly) = extract_rate_limits(Some(RateLimitDetails {
            primary_window: Some(rate_limit_window(27.0, SESSION_WINDOW_SECONDS)),
            secondary_window: Some(rate_limit_window(82.0, WEEKLY_WINDOW_SECONDS)),
        }));

        assert_eq!(
            session.and_then(|window| window.limit_window_seconds),
            Some(SESSION_WINDOW_SECONDS)
        );
        assert_eq!(
            weekly.and_then(|window| window.limit_window_seconds),
            Some(WEEKLY_WINDOW_SECONDS)
        );
    }

    #[test]
    fn moves_weekly_only_primary_window_to_weekly_slot() {
        let (session, weekly) = extract_rate_limits(Some(RateLimitDetails {
            primary_window: Some(rate_limit_window(35.0, WEEKLY_WINDOW_SECONDS)),
            secondary_window: None,
        }));

        assert!(session.is_none());
        assert_eq!(weekly.map(|window| window.used_percent), Some(35.0));
    }

    #[test]
    fn restores_semantic_order_if_backend_reverses_windows() {
        let (session, weekly) = extract_rate_limits(Some(RateLimitDetails {
            primary_window: Some(rate_limit_window(82.0, WEEKLY_WINDOW_SECONDS)),
            secondary_window: Some(rate_limit_window(27.0, SESSION_WINDOW_SECONDS)),
        }));

        assert_eq!(session.map(|window| window.used_percent), Some(27.0));
        assert_eq!(weekly.map(|window| window.used_percent), Some(82.0));
    }

    #[test]
    fn preserves_unknown_windows_by_backend_position() {
        let (primary, secondary) = extract_rate_limits(Some(RateLimitDetails {
            primary_window: Some(rate_limit_window(11.0, 60 * 60)),
            secondary_window: Some(rate_limit_window(22.0, 30 * 24 * 60 * 60)),
        }));

        assert_eq!(primary.map(|window| window.used_percent), Some(11.0));
        assert_eq!(secondary.map(|window| window.used_percent), Some(22.0));
    }
}
