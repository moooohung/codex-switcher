use rand::RngCore;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use anyhow::Context;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};
use tokio::runtime::Runtime;

use crate::commands::{
    add_account_from_auth_json_text, add_account_from_file, cancel_login, check_codex_processes,
    complete_login, delete_account, export_accounts_full_encrypted_bytes,
    export_accounts_slim_text, fetch_usage, get_account_usage_stats, get_active_account_info,
    get_masked_account_ids, import_accounts_full_encrypted_bytes, import_accounts_slim_text,
    kill_codex_processes, list_accounts, refresh_account_metadata, refresh_all_accounts_usage,
    rename_account, set_masked_account_ids, start_login, switch_account, warmup_account,
    warmup_all_accounts,
};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AccountIdArgs {
    #[serde(alias = "account_id")]
    account_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RenameAccountArgs {
    #[serde(alias = "account_id")]
    account_id: String,
    #[serde(alias = "new_name")]
    new_name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoginArgs {
    #[serde(alias = "account_name")]
    account_name: String,
}

#[derive(Debug, Deserialize)]
struct ImportSlimArgs {
    payload: String,
}

#[derive(Debug, Deserialize)]
struct MaskedIdsArgs {
    ids: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CloseCodexArgs {
    reopen_desktop: Option<bool>,
    force_close: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct UploadAuthJsonArgs {
    name: String,
    contents: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadEncryptedArgs {
    #[serde(default)]
    passphrase: String,
    #[serde(alias = "contents_base64")]
    contents_base64: String,
}

#[derive(Debug, Deserialize)]
struct FileImportArgs {
    path: String,
    name: String,
}

#[derive(Debug, Deserialize)]
struct BackupPasswordArgs {
    passphrase: String,
}

fn web_access_token() -> anyhow::Result<String> {
    if let Ok(token) = std::env::var("CODEX_SWITCHER_WEB_TOKEN") {
        anyhow::ensure!(
            token.len() >= 32,
            "CODEX_SWITCHER_WEB_TOKEN must have at least 32 characters"
        );
        return Ok(token);
    }
    let dir = crate::auth::get_config_dir()?;
    let _guard = crate::auth::persistence::lock_file(&dir.join("web-token.lock"))?;
    let path = dir.join("web-access-token");
    if path.exists() {
        let token = fs::read_to_string(&path)?.trim().to_string();
        anyhow::ensure!(token.len() >= 32, "Web access token file is invalid");
        return Ok(token);
    }
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let token = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    crate::auth::persistence::atomic_write(&path, token.as_bytes())?;
    println!("Web API access token saved in {}", path.display());
    Ok(token)
}

fn authorized(request: &Request, token: &str) -> bool {
    request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Authorization"))
        .and_then(|h| h.value.as_str().strip_prefix("Bearer "))
        .is_some_and(|value| {
            value.len() == token.len()
                && value
                    .bytes()
                    .zip(token.bytes())
                    .fold(0u8, |diff, (a, b)| diff | (a ^ b))
                    == 0
        })
}

pub fn run_lan_server(host: &str, port: u16) -> anyhow::Result<()> {
    let address = format!("{host}:{port}");
    let server = Server::http(&address)
        .map_err(|err| anyhow::anyhow!("Failed to bind HTTP server on {address}: {err}"))?;
    let runtime = Arc::new(Runtime::new().context("Failed to start async runtime")?);
    let token = Arc::new(web_access_token()?);
    let active_requests = Arc::new(AtomicUsize::new(0));
    let dist_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("dist");

    println!("Codex Switcher web server listening on http://{address}");
    println!("Serving static files from {}", dist_dir.display());

    for request in server.incoming_requests() {
        // A waiting OAuth completion must not block cancel_login on another request.
        if active_requests.fetch_add(1, Ordering::SeqCst) >= 16 {
            active_requests.fetch_sub(1, Ordering::SeqCst);
            let _ = respond_json(request, StatusCode(503), &json!({"error":"Server busy"}));
            continue;
        }
        let runtime = runtime.clone();
        let dist_dir = dist_dir.clone();
        let token = token.clone();
        let active_requests = active_requests.clone();
        std::thread::spawn(move || {
            if let Err(error) = handle_request(request, &runtime, &dist_dir, &token) {
                eprintln!("[web] request failed: {error:#}");
            }
            active_requests.fetch_sub(1, Ordering::SeqCst);
        });
    }

    Ok(())
}

fn handle_request(
    mut request: Request,
    runtime: &Runtime,
    dist_dir: &Path,
    token: &str,
) -> anyhow::Result<()> {
    let method = request.method().clone();
    let url = request.url().to_string();

    if url.starts_with("/api/") && !authorized(&request, token) {
        respond_json(
            request,
            StatusCode(401),
            &json!({"error":"Web API access token required"}),
        )?;
        return Ok(());
    }
    if method == Method::Get && url == "/api/health" {
        respond_json(request, StatusCode(200), &json!({ "ok": true }))?;
        return Ok(());
    }

    if method == Method::Post && url.starts_with("/api/invoke/") {
        let command = url.trim_start_matches("/api/invoke/");
        let payload = parse_request_json(&mut request)?;
        let result = runtime.block_on(invoke_web_command(command, payload));
        match result {
            Ok(value) => respond_json(request, StatusCode(200), &value)?,
            Err(error) => respond_json(request, StatusCode(400), &json!({ "error": error }))?,
        }
        return Ok(());
    }

    if method == Method::Get {
        serve_static(request, dist_dir, &url)?;
        return Ok(());
    }

    respond_text(
        request,
        StatusCode(405),
        "Method Not Allowed",
        "text/plain; charset=utf-8",
    )?;
    Ok(())
}

async fn invoke_web_command(command: &str, payload: Value) -> Result<Value, String> {
    match command {
        "open_prompt_cache_session" => {
            let provider = payload
                .get("provider")
                .and_then(Value::as_str)
                .ok_or("Missing cache provider")?;
            let args: AccountIdArgs = parse_args(payload.clone())?;
            let session_id = match payload
                .get("sessionId")
                .or_else(|| payload.get("session_id"))
            {
                None | Some(Value::Null) => None,
                Some(Value::String(id)) => Some(id.clone()),
                _ => return Err("Invalid session ID".into()),
            };
            crate::prompt_cache::open_prompt_cache_session(
                provider.to_owned(),
                args.account_id,
                session_id,
            )
            .await
        }
        "list_prompt_cache_sessions" => {
            let provider = payload
                .get("provider")
                .and_then(Value::as_str)
                .ok_or("Missing cache provider")?;
            let args: AccountIdArgs = parse_args(payload.clone())?;
            to_json(
                crate::prompt_cache_resume::list_prompt_cache_sessions(
                    provider.to_owned(),
                    args.account_id,
                )
                .await?,
            )
        }
        "get_prompt_cache_status" => {
            let args: AccountIdArgs = parse_args(payload.clone())?;
            let provider = payload
                .get("provider")
                .and_then(Value::as_str)
                .unwrap_or("codex");
            to_json(
                crate::prompt_cache::get_prompt_cache_status(provider.to_owned(), args.account_id)
                    .await?,
            )
        }
        "set_prompt_cache_auto_keepalive" => {
            let args: AccountIdArgs = parse_args(payload.clone())?;
            let provider = payload
                .get("provider")
                .and_then(Value::as_str)
                .ok_or("Missing provider")?;
            let enabled = payload
                .get("enabled")
                .and_then(Value::as_bool)
                .ok_or("Missing enabled flag")?;
            to_json(
                crate::prompt_cache::set_prompt_cache_auto_keepalive(
                    provider.to_owned(),
                    args.account_id,
                    enabled,
                )
                .await?,
            )
        }
        "claude_command" => {
            let action = payload
                .get("action")
                .and_then(Value::as_str)
                .ok_or("Missing Claude action")?;
            crate::claude::claude_command(action.to_owned(), payload.get("payload").cloned()).await
        }
        "list_accounts" => to_json(list_accounts().await?),
        "get_active_account_info" => to_json(get_active_account_info().await?),
        "add_account_from_file" => {
            let args: FileImportArgs = parse_args(payload)?;
            to_json(add_account_from_file(args.path, args.name).await?)
        }
        "add_account_from_auth_json_text" => {
            let args: UploadAuthJsonArgs = parse_args(payload)?;
            to_json(add_account_from_auth_json_text(args.name, args.contents).await?)
        }
        "get_usage" => {
            let args: AccountIdArgs = parse_args(payload)?;
            to_json(fetch_usage(&args.account_id).await?)
        }
        "get_account_usage_stats" => {
            let args: AccountIdArgs = parse_args(payload)?;
            to_json(get_account_usage_stats(args.account_id).await?)
        }
        "refresh_account_metadata" => {
            let args: AccountIdArgs = parse_args(payload)?;
            to_json(refresh_account_metadata(args.account_id).await?)
        }
        "refresh_all_accounts_usage" => to_json(refresh_all_accounts_usage().await?),
        "warmup_account" => {
            let args: AccountIdArgs = parse_args(payload)?;
            to_json(warmup_account(args.account_id).await?)
        }
        "warmup_all_accounts" => to_json(warmup_all_accounts().await?),
        "switch_account" => {
            let args: AccountIdArgs = parse_args(payload)?;
            to_json(switch_account(args.account_id).await?)
        }
        "delete_account" => {
            let args: AccountIdArgs = parse_args(payload)?;
            to_json(delete_account(args.account_id).await?)
        }
        "rename_account" => {
            let args: RenameAccountArgs = parse_args(payload)?;
            to_json(rename_account(args.account_id, args.new_name).await?)
        }
        "start_login" => {
            let args: LoginArgs = parse_args(payload)?;
            to_json(start_login(args.account_name).await?)
        }
        "complete_login" => to_json(complete_login().await?),
        "cancel_login" => to_json(cancel_login().await?),
        "export_accounts_slim_text" => to_json(export_accounts_slim_text().await?),
        "import_accounts_slim_text" => {
            let args: ImportSlimArgs = parse_args(payload)?;
            to_json(import_accounts_slim_text(args.payload).await?)
        }
        "export_accounts_full_encrypted_bytes" => {
            let args: BackupPasswordArgs = parse_args(payload)?;
            let encoded =
                STANDARD.encode(export_accounts_full_encrypted_bytes(args.passphrase).await?);
            to_json(encoded)
        }
        "import_accounts_full_encrypted_bytes" => {
            let args: UploadEncryptedArgs = parse_args(payload)?;
            let bytes = STANDARD
                .decode(args.contents_base64)
                .map_err(|error| format!("Failed to decode uploaded backup: {error}"))?;
            to_json(import_accounts_full_encrypted_bytes(bytes, args.passphrase).await?)
        }
        "get_masked_account_ids" => to_json(get_masked_account_ids().await?),
        "set_masked_account_ids" => {
            let args: MaskedIdsArgs = parse_args(payload)?;
            to_json(set_masked_account_ids(args.ids).await?)
        }
        "check_codex_processes" => to_json(check_codex_processes().await?),
        "kill_codex_processes" => {
            let args: CloseCodexArgs = parse_args(payload)?;
            to_json(kill_codex_processes(args.reopen_desktop, args.force_close).await?)
        }
        _ => Err(format!("Unsupported web command: {command}")),
    }
}

fn parse_request_json(request: &mut Request) -> anyhow::Result<Value> {
    let mut body = String::new();
    request
        .as_reader()
        .take(12 * 1024 * 1024 + 1)
        .read_to_string(&mut body)
        .context("Failed to read request body")?;

    anyhow::ensure!(body.len() <= 12 * 1024 * 1024, "Request body too large");
    if body.trim().is_empty() {
        return Ok(json!({}));
    }

    serde_json::from_str(&body).context("Failed to parse request JSON")
}

fn parse_args<T>(value: Value) -> Result<T, String>
where
    T: DeserializeOwned,
{
    serde_json::from_value(value).map_err(|error| format!("Invalid command payload: {error}"))
}

fn to_json<T>(value: T) -> Result<Value, String>
where
    T: serde::Serialize,
{
    serde_json::to_value(value).map_err(|error| format!("Failed to serialize response: {error}"))
}

fn serve_static(request: Request, dist_dir: &Path, url: &str) -> anyhow::Result<()> {
    let requested = if url == "/" {
        PathBuf::from("index.html")
    } else {
        sanitize_path(url)?
    };
    let candidate = dist_dir.join(&requested);

    if candidate.is_file() {
        return serve_file(request, candidate);
    }

    if requested.extension().is_some() {
        respond_text(
            request,
            StatusCode(404),
            "Not Found",
            "text/plain; charset=utf-8",
        )?;
        return Ok(());
    }

    serve_file(request, dist_dir.join("index.html"))
}

fn sanitize_path(url: &str) -> anyhow::Result<PathBuf> {
    let path = url.split('?').next().unwrap_or("/");
    let raw = path.trim_start_matches('/');
    let candidate = Path::new(raw);

    for component in candidate.components() {
        match component {
            Component::Normal(_) => {}
            _ => anyhow::bail!("Invalid request path"),
        }
    }

    Ok(candidate.to_path_buf())
}

fn serve_file(request: Request, path: PathBuf) -> anyhow::Result<()> {
    let data = fs::read(&path).with_context(|| format!("Failed to read {}", path.display()))?;
    let mime = mime_type_for_path(&path);
    let response = Response::from_data(data)
        .with_header(header("Content-Type", mime)?)
        .with_header(header("Cache-Control", "no-cache")?);
    request.respond(response)?;
    Ok(())
}

fn respond_json(request: Request, status: StatusCode, payload: &Value) -> anyhow::Result<()> {
    let response = Response::from_string(serde_json::to_string(payload)?)
        .with_status_code(status)
        .with_header(header("Content-Type", "application/json; charset=utf-8")?);
    request.respond(response)?;
    Ok(())
}

fn respond_text(
    request: Request,
    status: StatusCode,
    body: &str,
    content_type: &str,
) -> anyhow::Result<()> {
    let response = Response::from_string(body.to_string())
        .with_status_code(status)
        .with_header(header("Content-Type", content_type)?);
    request.respond(response)?;
    Ok(())
}

fn header(name: &str, value: &str) -> anyhow::Result<Header> {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).map_err(|_| {
        anyhow::anyhow!("Failed to create header {name}: invalid header value `{value}`")
    })
}

fn mime_type_for_path(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "css" => "text/css; charset=utf-8",
        "html" => "text/html; charset=utf-8",
        "ico" => "image/x-icon",
        "jpeg" | "jpg" => "image/jpeg",
        "js" => "text/javascript; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "png" => "image/png",
        "svg" => "image/svg+xml",
        "txt" => "text/plain; charset=utf-8",
        "webp" => "image/webp",
        _ => "application/octet-stream",
    }
}
