//! Read-only history selection and a detached, single-writer resume owner.
use anyhow::{bail, ensure, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant, UNIX_EPOCH},
};

const WINDOW: usize = 256 * 1024;
const SCAN_FILES: usize = 256;
const INSPECT_FILES: usize = 96;
const LIST_SIZE: usize = 40;

#[derive(Clone, Debug)]
pub(crate) struct ResumeTarget {
    pub session_id: String,
    pub transcript: PathBuf,
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub effort: Option<String>,
}

#[derive(Clone, Serialize, Debug)]
pub struct SavedSession {
    pub id: String,
    pub title: String,
    pub cwd: String,
    pub model: Option<String>,
    pub updated_at: i64,
    pub resumable: bool,
    pub reason: Option<String>,
    pub source: String,
}

#[derive(Serialize)]
pub struct SavedSessions {
    pub sessions: Vec<SavedSession>,
    pub truncated: bool,
    pub reason: Option<String>,
}

struct Root {
    path: PathBuf,
    source: &'static str,
}
struct Candidate {
    row: SavedSession,
    target: ResumeTarget,
}

fn uuid(value: &str) -> bool {
    value.len() == 36 && uuid::Uuid::parse_str(value).is_ok()
}

fn read_object(path: &Path) -> Result<Value> {
    read_object_limit(path, 64 * 1024)
}

fn read_object_limit(path: &Path, limit: usize) -> Result<Value> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "Identity metadata is oversized");
    let data: Value = serde_json::from_slice(&bytes)?;
    ensure!(data.is_object(), "Identity metadata is invalid");
    Ok(data)
}

fn roots(provider: &str, account: &str) -> Result<Vec<Root>> {
    ensure!(uuid(account), "Invalid account ID");
    match provider {
        "codex" => {
            ensure!(
                crate::auth::get_account(account)?.is_some(),
                "Codex account not found"
            );
            let home = std::env::var_os("CODEX_HOME")
                .map(PathBuf::from)
                .unwrap_or(crate::claude::storage::user_home()?.join(".codex"));
            Ok(vec![Root {
                path: home.join("sessions"),
                source: "local_codex",
            }])
        }
        "claude" => {
            let manager = crate::claude::storage::Manager::current()?;
            let selected = manager.account(account)?;
            let profile = manager.profile(account)?;
            let mut result = vec![Root {
                path: profile.join("projects"),
                source: "account_profile",
            }];
            // Global Claude history is shared. Only expose it under the current,
            // explicitly matching login; never infer identity from an email.
            if let (Some(expected), Ok(live)) = (
                selected.account_uuid,
                read_object_limit(&manager.live_config, 8 * 1024 * 1024),
            ) {
                if live
                    .pointer("/oauthAccount/accountUuid")
                    .and_then(Value::as_str)
                    == Some(expected.as_str())
                {
                    result.push(Root {
                        path: manager.live_dir.join("projects"),
                        source: "local_claude",
                    });
                }
            }
            Ok(result)
        }
        _ => bail!("Unsupported cache provider"),
    }
}

fn reparse(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    false
}

fn trusted_root(path: &Path) -> bool {
    path.is_absolute()
        && path.ancestors().all(|ancestor| {
            fs::symlink_metadata(ancestor)
                .is_ok_and(|metadata| metadata.is_dir() && !reparse(&metadata))
        })
}

fn collect(
    path: &Path,
    depth: usize,
    paths: &mut Vec<PathBuf>,
    started: Instant,
    truncated: &mut bool,
) {
    if depth == 0 || paths.len() >= SCAN_FILES || started.elapsed() > Duration::from_secs(2) {
        *truncated = true;
        return;
    }
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    let mut entries: Vec<_> = entries.take(2049).filter_map(|entry| entry.ok()).collect();
    if entries.len() > 2048 {
        entries.truncate(2048);
        *truncated = true;
    }
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.file_name()));
    for entry in entries {
        if paths.len() >= SCAN_FILES || started.elapsed() > Duration::from_secs(2) {
            *truncated = true;
            break;
        }
        let Ok(metadata) = fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if reparse(&metadata) {
            continue;
        }
        if metadata.is_dir() {
            collect(&entry.path(), depth - 1, paths, started, truncated);
        } else if metadata.is_file() && entry.path().extension().is_some_and(|v| v == "jsonl") {
            paths.push(entry.path());
        }
    }
}

fn file_uuid(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let id = stem.get(stem.len().checked_sub(36)?..)?;
    uuid(id).then(|| id.to_owned())
}

fn windows_read_probe(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Refuse an already open writer. This temporary probe closes before
        // official resume, so it never prevents that CLI from writing history.
        options.share_mode(1);
    }
    options
        .open(path)
        .context("대화 기록이 다른 프로세스에서 사용 중이거나 읽을 수 없습니다.")
}

fn records(path: &Path) -> Result<(Vec<(Value, bool)>, i64)> {
    let mut file = windows_read_probe(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && !reparse(&metadata),
        "Invalid history file"
    );
    let updated = metadata.modified()?.duration_since(UNIX_EPOCH)?.as_secs() as i64;
    let mut head = Vec::new();
    (&mut file).take(WINDOW as u64).read_to_end(&mut head)?;
    let large = metadata.len() > WINDOW as u64;
    let mut chunks = vec![(head, false, !large)];
    if metadata.len() > WINDOW as u64 {
        let offset = metadata.len().saturating_sub(WINDOW as u64);
        file.seek(SeekFrom::Start(offset.saturating_sub(1)))?;
        let mut preceding = [0];
        file.read_exact(&mut preceding)?;
        file.seek(SeekFrom::Start(offset))?;
        let mut tail = Vec::new();
        (&mut file).take(WINDOW as u64).read_to_end(&mut tail)?;
        chunks.push((tail, offset > 0 && preceding[0] != b'\n', true));
    }
    let mut values = Vec::new();
    for (chunk, partial_start, recent) in chunks {
        for (index, line) in chunk.split(|b| *b == b'\n').enumerate() {
            if partial_start && index == 0 {
                continue;
            }
            if let Ok(value) = serde_json::from_slice::<Value>(line) {
                values.push((value, recent));
            }
        }
    }
    Ok((values, updated))
}

fn text(value: Option<&Value>, max: usize) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(|value| {
            value
                .chars()
                .filter(|c| !c.is_control())
                .take(max)
                .collect()
        })
}

fn parse(provider: &str, path: &Path, source: &str) -> Result<Candidate> {
    let id = file_uuid(path).context("Not a saved conversation UUID")?;
    let (records, updated_at) = records(path)?;
    let mut cwd = None;
    let mut model = None;
    let mut effort = None;
    let mut title = None;
    let mut identity = false;
    let mut recent_settings = false;
    let mut unsupported = None;
    for (record, recent) in records {
        if provider == "codex" {
            let payload = &record["payload"];
            match record["type"].as_str() {
                Some("session_meta") => {
                    ensure!(
                        payload["id"].as_str() == Some(id.as_str()),
                        "History UUID mismatch"
                    );
                    identity = true;
                    cwd = text(payload.get("cwd"), 32768).map(PathBuf::from);
                    if payload["model_provider"]
                        .as_str()
                        .is_some_and(|p| !matches!(p, "openai" | "switcher-cache"))
                    {
                        unsupported = Some("다른 모델 제공자의 대화입니다.".to_owned());
                    }
                }
                Some("turn_context") => {
                    model = text(payload.get("model"), 128);
                    recent_settings = recent && model.is_some();
                    effort = text(
                        payload
                            .get("effort")
                            .or_else(|| payload.get("reasoning_effort")),
                        16,
                    );
                    cwd = text(payload.get("cwd"), 32768).map(PathBuf::from).or(cwd);
                }
                Some("event_msg") if payload["type"] == "user_message" && title.is_none() => {
                    title = text(payload.get("message"), 100);
                }
                _ => {}
            }
        } else {
            if record["isSidechain"] == true {
                continue;
            }
            if let Some(session) = record["sessionId"].as_str() {
                ensure!(session == id, "History UUID mismatch");
                identity = true;
            }
            cwd = text(record.get("cwd"), 32768).map(PathBuf::from).or(cwd);
            if let Some(value) = text(record.pointer("/message/model"), 128).filter(|v| {
                v.starts_with("claude-")
                    || matches!(v.as_str(), "sonnet" | "opus" | "haiku" | "fable")
            }) {
                model = Some(value);
                recent_settings = recent;
            }
            if record["type"] == "custom-title" {
                title = text(record.get("customTitle"), 100).or(title);
            }
            if title.is_none() && record["type"] == "summary" {
                title = text(record.get("summary"), 100);
            }
        }
    }
    ensure!(identity, "History session identity is unavailable");
    let cwd = cwd.context("History working directory is unavailable")?;
    ensure!(cwd.is_absolute(), "Invalid saved working directory");
    if provider == "codex" {
        match model.as_deref() {
            Some(value) if value.starts_with("chatgpt-web/") => {
                unsupported = Some("웹 브리지 대화는 네이티브 CLI로 이어받지 않습니다.".into())
            }
            Some(value) if value.starts_with("gpt-") || value.starts_with("codex-") => {}
            _ => unsupported = Some("원래 네이티브 모델을 확인할 수 없습니다.".into()),
        }
        if effort.as_deref().is_some_and(|v| {
            !matches!(
                v,
                "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max" | "ultra"
            )
        }) {
            unsupported = Some("원래 reasoning effort를 확인할 수 없습니다.".into());
        }
    }
    if !cwd.is_dir() {
        unsupported = Some("원래 작업 폴더가 없습니다.".into());
    }
    if !recent_settings {
        unsupported = Some("최근 기록에서 원래 모델 설정을 확인할 수 없습니다.".into());
    }
    let row = SavedSession {
        title: title.unwrap_or_else(|| format!("대화 {}", &id[..8])),
        id: id.clone(),
        cwd: cwd.to_string_lossy().into_owned(),
        model: model.clone(),
        updated_at,
        resumable: unsupported.is_none(),
        reason: unsupported,
        source: source.into(),
    };
    Ok(Candidate {
        row,
        target: ResumeTarget {
            session_id: id,
            transcript: path.into(),
            cwd,
            model,
            effort,
        },
    })
}

fn lease_path(provider: &str, session: &str) -> Result<PathBuf> {
    ensure!(
        matches!(provider, "claude" | "codex") && uuid(session),
        "Invalid resume identity"
    );
    Ok(crate::auth::get_config_dir()?
        .join("cache-runtime/resume-leases")
        .join(format!("{provider}-{session}.lock")))
}

fn locked(provider: &str, session: &str) -> bool {
    let Ok(path) = lease_path(provider, session) else {
        return true;
    };
    locked_path(&path)
}

fn locked_path(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map(|file| file.try_lock().is_err())
        .unwrap_or(true)
}

fn acquire(provider: &str, session: &str) -> Result<File> {
    let path = lease_path(provider, session)?;
    acquire_path(&path)
}

fn acquire_path(path: &Path) -> Result<File> {
    fs::create_dir_all(path.parent().context("Missing lease parent")?)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    file.try_lock()
        .map_err(|_| anyhow::anyhow!("이미 실행 중인 대화입니다."))?;
    Ok(file)
}

#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, GetLastError},
        System::Threading::{OpenProcess, WaitForSingleObject},
    };
    let handle = unsafe { OpenProcess(0x00100000, 0, pid) };
    if handle.is_null() {
        return unsafe { GetLastError() } == 5;
    }
    let alive = unsafe { WaitForSingleObject(handle, 0) } == 258;
    unsafe {
        CloseHandle(handle);
    }
    alive
}
#[cfg(not(windows))]
fn process_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .is_ok_and(|status| status.success())
}

fn active_claude(roots: &[Root]) -> HashSet<String> {
    let mut active = HashSet::new();
    for root in roots {
        if !trusted_root(&root.path) {
            continue;
        }
        let Some(config) = root.path.parent() else {
            continue;
        };
        let Ok(entries) = fs::read_dir(config.join("sessions")) else {
            continue;
        };
        for (index, entry) in entries.take(1025).enumerate() {
            if index == 1024 {
                active.insert("unknown_active_identity".into());
                break;
            }
            let Ok(entry) = entry else {
                active.insert("unknown_active_identity".into());
                continue;
            };
            let path = entry.path();
            if path.extension().is_none_or(|v| v != "json") {
                continue;
            }
            let Some(pid) = path
                .file_stem()
                .and_then(|v| v.to_str())
                .and_then(|v| v.parse::<u32>().ok())
            else {
                continue;
            };
            if !process_alive(pid) {
                continue;
            }
            match read_object(&path) {
                Ok(data) => {
                    if let Some(id) = data
                        .get("sessionId")
                        .or_else(|| data.get("session_id"))
                        .and_then(Value::as_str)
                        .filter(|id| uuid(id))
                    {
                        active.insert(id.into());
                    } else {
                        active.insert("unknown_active_identity".into());
                    }
                }
                Err(_) => {
                    active.insert("unknown_active_identity".into());
                }
            }
        }
    }
    active
}

fn candidates(
    provider: &str,
    account: &str,
    own_session: Option<&str>,
) -> Result<(Vec<Candidate>, bool)> {
    let roots = roots(provider, account)?;
    let active = if provider == "claude" {
        active_claude(&roots)
    } else {
        HashSet::new()
    };
    let started = Instant::now();
    let mut truncated = false;
    let mut candidates = Vec::new();
    for root in &roots {
        if !trusted_root(&root.path) {
            continue;
        }
        let canonical = root.path.canonicalize()?;
        let mut paths = Vec::new();
        collect(
            &canonical,
            if provider == "codex" { 5 } else { 3 },
            &mut paths,
            started,
            &mut truncated,
        );
        paths.sort_by_key(|path| {
            std::cmp::Reverse(
                fs::metadata(path)
                    .and_then(|m| m.modified())
                    .unwrap_or(UNIX_EPOCH),
            )
        });
        if paths.len() > INSPECT_FILES {
            paths.truncate(INSPECT_FILES);
            truncated = true;
        }
        for path in paths {
            if started.elapsed() > Duration::from_secs(4) {
                truncated = true;
                break;
            }
            let Ok(actual) = path.canonicalize() else {
                continue;
            };
            if !actual.starts_with(&canonical) {
                continue;
            }
            if let Ok(mut candidate) = parse(provider, &actual, root.source) {
                if active.contains(&candidate.row.id)
                    || active.contains("unknown_active_identity")
                    || (own_session != Some(candidate.row.id.as_str())
                        && locked(provider, &candidate.row.id))
                {
                    candidate.row.resumable = false;
                    candidate.row.reason =
                        Some("실행 중인 대화입니다. 해당 세션을 종료한 뒤 이어가세요.".into());
                }
                candidates.push(candidate);
            }
        }
    }
    candidates.sort_by_key(|c| std::cmp::Reverse(c.row.updated_at));
    let mut ids = HashSet::new();
    candidates.retain(|c| ids.insert(c.row.id.clone()));
    Ok((candidates, truncated))
}

#[tauri::command]
pub async fn list_prompt_cache_sessions(
    provider: String,
    account_id: String,
) -> Result<SavedSessions, String> {
    tokio::task::spawn_blocking(move || {
        let (mut candidates, mut truncated) = candidates(&provider, &account_id, None)?;
        if candidates.len() > LIST_SIZE {
            truncated = true;
            candidates.truncate(LIST_SIZE);
        }
        Ok(SavedSessions {
            sessions: candidates.into_iter().map(|c| c.row).collect(),
            truncated,
            reason: Some(
                "선택한 계정으로 기존 대화를 이어갑니다. 진행 중인 창은 먼저 종료하세요.".into(),
            ),
        })
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e: anyhow::Error| e.to_string())
}

fn resolve(provider: &str, account: &str, session: &str, own_lease: bool) -> Result<ResumeTarget> {
    ensure!(uuid(session), "Invalid session ID");
    let (candidates, _) = candidates(provider, account, own_lease.then_some(session))?;
    let chosen = candidates
        .into_iter()
        .find(|c| c.row.id == session)
        .context("기존 대화가 목록에 없습니다. 새로 고침 후 다시 선택하세요.")?;
    ensure!(
        chosen.row.resumable,
        "{}",
        chosen
            .row
            .reason
            .unwrap_or_else(|| "이어갈 수 없는 대화입니다.".into())
    );
    Ok(chosen.target)
}

pub async fn launch(provider: &str, account: &str, session: &str) -> Result<Value> {
    let (provider, account, session) =
        (provider.to_owned(), account.to_owned(), session.to_owned());
    tokio::task::spawn_blocking(move || {
        resolve(&provider, &account, &session, false)?;
        let helper = crate::prompt_cache::helper_executable()?;
        let mut command = Command::new(helper);
        command
            .args(["--prompt-cache-resume", &provider, &account, &session])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let mut child = command.spawn().context("Could not start resume owner")?;
        let mut output = child
            .stdout
            .take()
            .context("Missing resume acknowledgement")?;
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let result = (|| -> Result<Value> {
                let mut line = Vec::new();
                loop {
                    let mut byte = [0];
                    output.read_exact(&mut byte)?;
                    if byte[0] == b'\n' {
                        break;
                    }
                    ensure!(line.len() < 4096, "Resume acknowledgement is oversized");
                    line.push(byte[0]);
                }
                Ok(serde_json::from_slice(&line)?)
            })();
            let _ = send.send(result);
        });
        // A timeout is ambiguous: never kill a possibly opened CLI or retry it.
        let reply = receive
            .recv_timeout(Duration::from_secs(25))
            .context("이어가기 확인이 늦어지고 있습니다. 열린 CLI와 세션 상태를 확인하세요.")??;
        if let Some(message) = reply["error"].as_str() {
            bail!("{message}");
        }
        ensure!(
            reply["started"] == true,
            "Resume owner did not confirm startup"
        );
        Ok(reply)
    })
    .await?
}

fn write_started(mut output: impl Write, provider: &str, session: &str) -> Result<()> {
    serde_json::to_writer(
        &mut output,
        &json!({"started":true,"resumed":true,"session_id":session,"provider":provider}),
    )?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

pub fn helper_entry(provider: &str, account: &str, session: &str) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let result = async {
            ensure!(crate::prompt_cache::preference(&crate::auth::get_config_dir()?, provider, account, None)?, "계정의 캐시 자동 유지 옵션을 먼저 켜 주세요.");
            let _lease = acquire(provider, session)?;
            let target = resolve(provider, account, session, true)?;
            match provider {
                "codex" => { crate::prompt_cache_codex::open_resume(account, &target).await?; }
                "claude" => { let manager = crate::claude::storage::Manager::current()?; crate::claude::resume_cache_session(&manager, account, &target).await?; }
                _ => bail!("Unsupported provider"),
            }
            // The GUI/acknowledgement pipe is not the resumed CLI's lifetime.
            // Keep its lease even if Switcher closes during startup delivery.
            let _ = write_started(std::io::stdout(), provider, session);
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let running = match provider {
                    "codex" => crate::prompt_cache_codex::managed_running(account),
                    "claude" => crate::claude::cache_session_running(account).await,
                    _ => Ok(false),
                };
                match running { Ok(false) => break, Ok(true) | Err(_) => {} }
            }
            Ok::<(), anyhow::Error>(())
        }.await;
        if result.is_err() {
            // No raw CLI/provider errors or transcript text in bootstrap output.
            serde_json::to_writer(std::io::stdout(), &json!({"error":"기존 대화를 열지 못했습니다. 실행 중인 세션·계정 인증·기록을 확인하세요."}))?;
            std::io::stdout().write_all(b"\n")?; std::io::stdout().flush()?;
        }
        result
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    const ID: &str = "11111111-1111-4111-8111-111111111111";
    fn temp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cache-resume-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        dir
    }
    fn native(dir: &Path, model: &str) -> PathBuf {
        let path = dir.join(format!("rollout-2026-10-11-{ID}.jsonl"));
        let values = [
            json!({"type":"session_meta","payload":{"id":ID,"cwd":dir,"model_provider":"openai"}}),
            json!({"type":"turn_context","payload":{"cwd":dir,"model":model,"effort":"xhigh"}}),
            json!({"type":"event_msg","payload":{"type":"user_message","message":"existing saved work"}}),
        ];
        fs::write(
            &path,
            values.iter().map(|v| format!("{v}\n")).collect::<String>(),
        )
        .unwrap();
        path
    }
    #[test]
    fn native_preserves_id_model_effort_cwd_and_bytes() {
        let dir = temp();
        let path = native(&dir, "gpt-6.1-sol");
        let before = fs::read(&path).unwrap();
        let result = parse("codex", &path, "local_codex").unwrap();
        assert_eq!(result.target.session_id, ID);
        assert_eq!(result.target.cwd, dir);
        assert_eq!(result.target.model.as_deref(), Some("gpt-6.1-sol"));
        assert_eq!(result.target.effort.as_deref(), Some("xhigh"));
        assert!(result.row.resumable);
        assert_eq!(fs::read(&path).unwrap(), before);
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn web_and_unknown_models_never_silently_become_native() {
        let dir = temp();
        for model in ["chatgpt-web/gpt-5.6-sol", "unknown"] {
            let path = native(&dir, model);
            assert!(!parse("codex", &path, "local_codex").unwrap().row.resumable);
        }
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn mismatched_uuid_missing_cwd_and_path_inputs_reject() {
        let dir = temp();
        let path = native(&dir, "gpt-6.1-sol");
        let source = fs::read_to_string(&path).unwrap();
        fs::write(
            &path,
            source.replace(ID, "22222222-2222-4222-8222-222222222222"),
        )
        .unwrap();
        assert!(parse("codex", &path, "local_codex").is_err());
        assert!(!uuid("../history"));
        assert!(!uuid("--last"));
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn claude_ignores_sidechain_model_and_restores_main_conversation() {
        let dir = temp();
        let path = dir.join(format!("{ID}.jsonl"));
        fs::write(&path, format!("{}\n{}\n{}\n", json!({"type":"assistant","sessionId":ID,"cwd":dir,"message":{"model":"claude-sonnet-5"}}), json!({"type":"assistant","sessionId":"other","isSidechain":true,"message":{"model":"haiku"}}), json!({"type":"custom-title","sessionId":ID,"customTitle":"Saved project"}))).unwrap();
        let before = fs::read(&path).unwrap();
        let candidate = parse("claude", &path, "account_profile").unwrap();
        assert_eq!(candidate.row.title, "Saved project");
        assert_eq!(candidate.target.model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(before, fs::read(&path).unwrap());
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn large_history_is_bounded_and_latest_tail_model_wins() {
        let dir = temp();
        let path = native(&dir, "gpt-5.6-sol");
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&vec![b'x'; 2 * WINDOW]).unwrap();
        file.write_all(
            format!(
                "\n{}\n",
                json!({"type":"turn_context","payload":{"model":"gpt-6.1-sol","effort":"high"}})
            )
            .as_bytes(),
        )
        .unwrap();
        drop(file);
        let candidate = parse("codex", &path, "local_codex").unwrap();
        assert_eq!(candidate.target.model.as_deref(), Some("gpt-6.1-sol"));
        assert_eq!(candidate.target.effort.as_deref(), Some("high"));
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn scan_skips_symlinks_and_bounds_file_count() {
        let dir = temp();
        for n in 0..SCAN_FILES + 1 {
            fs::write(dir.join(format!("{n:04}.jsonl")), b"{}\n").unwrap();
        }
        let mut paths = Vec::new();
        let mut truncated = false;
        collect(&dir, 3, &mut paths, Instant::now(), &mut truncated);
        assert_eq!(paths.len(), SCAN_FILES);
        assert!(truncated);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn resume_owner_lease_excludes_second_writer_and_releases_on_exit() {
        let dir = temp();
        let path = dir.join("resume-owner.lock");
        assert!(!locked_path(&path));
        let owner = acquire_path(&path).unwrap();
        assert!(locked_path(&path));
        assert!(acquire_path(&path).is_err());
        drop(owner);
        assert!(!locked_path(&path));
        drop(acquire_path(&path).unwrap());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn live_claude_session_marker_is_detected_without_touching_real_sessions() {
        let dir = temp();
        fs::create_dir(dir.join("projects")).unwrap();
        fs::create_dir(dir.join("sessions")).unwrap();
        let marker = dir
            .join("sessions")
            .join(format!("{}.json", std::process::id()));
        fs::write(&marker, format!("{}", json!({"sessionId":ID}))).unwrap();
        let roots = [Root {
            path: dir.join("projects"),
            source: "fixture",
        }];
        assert!(active_claude(&roots).contains(ID));
        fs::write(&marker, b"{partial").unwrap();
        assert!(active_claude(&roots).contains("unknown_active_identity"));
        fs::write(&marker, b"{}").unwrap();
        assert!(active_claude(&roots).contains("unknown_active_identity"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_unobserved_middle_context_never_restores_stale_native_settings() {
        let dir = temp();
        let path = native(&dir, "gpt-6.1-sol");
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&vec![b'\n'; WINDOW]).unwrap();
        writeln!(file, "{}", json!({"type":"turn_context","payload":{"model":"chatgpt-web/gpt-5.6-sol","effort":"high"}})).unwrap();
        file.write_all(&vec![b'\n'; WINDOW + 256]).unwrap();
        drop(file);
        assert!(!parse("codex", &path, "fixture").unwrap().row.resumable);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn exact_tail_line_boundary_keeps_the_first_complete_context() {
        let dir = temp();
        let path = native(&dir, "gpt-5.6-sol");
        let mut bytes = fs::read(&path).unwrap();
        bytes.resize(WINDOW, b'\n');
        let context = format!(
            "{}\n",
            json!({"type":"turn_context","payload":{"model":"gpt-6.1-sol","effort":"high"}})
        );
        bytes.extend_from_slice(context.as_bytes());
        bytes.resize(2 * WINDOW, b'\n');
        fs::write(&path, bytes).unwrap();
        let result = parse("codex", &path, "fixture").unwrap();
        assert!(result.row.resumable);
        assert_eq!(result.target.model.as_deref(), Some("gpt-6.1-sol"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn acknowledgement_pipe_failure_does_not_release_the_resume_owner() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
        }
        let dir = temp();
        let path = dir.join("owner.lock");
        let owner = acquire_path(&path).unwrap();
        assert!(write_started(Broken, "codex", ID).is_err());
        assert!(locked_path(&path));
        drop(owner);
        assert!(!locked_path(&path));
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn a_projects_root_junction_cannot_escape_its_account_anchor() {
        let dir = temp();
        let target = dir.join("other-account");
        fs::create_dir(&target).unwrap();
        let alias = dir.join("projects");
        let output = Command::new("cmd.exe")
            .args([
                "/d",
                "/c",
                "mklink",
                "/j",
                alias.to_str().unwrap(),
                target.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "Could not create isolated junction fixture"
        );
        assert!(!trusted_root(&alias));
        assert!(trusted_root(&target));
        fs::remove_dir(&alias).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn an_open_history_writer_prevents_resume_observation() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = temp();
        let path = native(&dir, "gpt-6.1-sol");
        let writer = OpenOptions::new()
            .append(true)
            .share_mode(3)
            .open(&path)
            .unwrap();
        assert!(parse("codex", &path, "local_codex").is_err());
        drop(writer);
        assert!(parse("codex", &path, "local_codex").is_ok());
        fs::remove_dir_all(dir).unwrap();
    }
}
