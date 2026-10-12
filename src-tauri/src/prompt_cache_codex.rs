//! A private transport for an explicitly launched native Codex CLI session.
//!
//! Parent wiring: export this module; dispatch `--prompt-cache-codex-host <id>`
//! to `host_entry`; call `open` from an explicit UI action and `status` when
//! building account telemetry. `open_resume` accepts only the parent's validated
//! native history target; the official CLI owns resumption and history writes.
//! The immutable helper owns a process HANDLE, not the GUI parent's lifetime.
//! Prompt bodies, affinity headers and proxy credentials never go to disk.
//! Prewarm is an experiment on the OAuth route: never fall back to generation.
//! https://developers.openai.com/api/docs/guides/prompt-caching
use crate::prompt_cache_resume::ResumeTarget;
use anyhow::{bail, ensure, Context, Result};
use reqwest::{header::HeaderMap, Client};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap},
    io::{Read, Write},
    net::{SocketAddr, TcpStream as BlockingStream},
    path::PathBuf,
    process::{Command, Stdio},
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{watch, Semaphore},
};

const RESPONSES: &str = "https://chatgpt.com/backend-api/codex/responses";
const MODELS: &str = "https://chatgpt.com/backend-api/codex/models";
const TOKEN_ENV: &str = "CODEX_SWITCHER_CACHE_TOKEN";
// At most one foreground body, one captured body and one warm body: <=12 MiB
// of retained/request byte buffers, leaving room under the 16 MiB byte budget.
// This is a payload budget, not a claim that the process RSS is below 16 MiB.
const MAX_BODY: usize = 4 * 1024 * 1024;
const MAX_HEADERS: usize = 16 * 1024;
const MAX_FRAME: usize = 64 * 1024;
const TTL: u64 = 1800;
const WARM_AFTER: u64 = 1500;
const MAX_IDLE: u64 = 12 * 3600;
// A lifetime cap is stricter than a daily cap for this managed session. Opening
// another session is a new explicit action; this is not an account-wide ledger.
const MAX_WARMS: u64 = 12;
const WARM_DEADLINE: u64 = 20;
const AFFINITY: &[&str] = &[
    "session-id",
    "thread-id",
    "x-session-affinity",
    "x-codex-turn-state",
    "x-codex-routing-hint",
    "x-codex-turn-metadata",
    "x-codex-window-id",
    "x-codex-beta-features",
    "x-openai-internal-codex-responses-lite",
    "originator",
    "user-agent",
    "version",
    "openai-beta",
    "x-parent-session-id",
    "x-client-request-id",
    "x-openai-subagent",
];

#[derive(Clone, Serialize, Deserialize)]
struct Endpoint {
    port: u16,
    token: String,
}
#[derive(Clone)]
struct ManagedHost {
    endpoint: Endpoint,
    pid: u32,
    created: u64,
}
static HOSTS: OnceLock<Mutex<HashMap<String, ManagedHost>>> = OnceLock::new();
fn hosts() -> &'static Mutex<HashMap<String, ManagedHost>> {
    HOSTS.get_or_init(|| Mutex::new(HashMap::new()))
}
fn now() -> i64 {
    chrono::Utc::now().timestamp()
}
fn valid_account(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
}
fn enabled(id: &str) -> Result<bool> {
    crate::prompt_cache::preference(&crate::auth::get_config_dir()?, "codex", id, None)
}
fn selected_identity(id: &str) -> Result<String> {
    let account = crate::auth::get_account(id)?.context("Selected account no longer exists")?;
    match account.auth_data {
        crate::types::AuthData::ChatGPT { account_id: Some(identity), .. } if !identity.is_empty() => Ok(identity),
        _ => bail!("Managed native cache sessions require a ChatGPT OAuth account with an account identity"),
    }
}

/// Explicit action only. Starts a hidden immutable helper and a NEW console.
/// Neither argv nor the local HTTP transport receives the account's OAuth token.
pub async fn open(account_id: &str) -> Result<Value> {
    ensure!(valid_account(account_id), "Invalid account ID");
    let id = account_id.to_owned();
    tokio::task::spawn_blocking(move || open_blocking(&id, None)).await?
}

/// The parent owns history selection, writer validation and the thread lease.
/// Pass no prompt: opening the console must wait for the user's next input.
pub(crate) async fn open_resume(account_id: &str, target: &ResumeTarget) -> Result<Value> {
    ensure!(valid_account(account_id), "Invalid account ID");
    validate_resume_target(target)?;
    let id = account_id.to_owned();
    // The parent contract does not require ResumeTarget to implement Clone.
    let target = ResumeTarget {
        session_id: target.session_id.clone(),
        transcript: target.transcript.clone(),
        cwd: target.cwd.clone(),
        model: target.model.clone(),
        effort: target.effort.clone(),
    };
    tokio::task::spawn_blocking(move || open_blocking(&id, Some(&target))).await?
}

fn open_blocking(id: &str, target: Option<&ResumeTarget>) -> Result<Value> {
    if let Some(target) = target {
        validate_resume_target(target)?;
    }
    ensure!(
        enabled(id)?,
        "Enable cache keepalive for this account before opening a managed session"
    );
    selected_identity(id)?;
    drop(account_lease(id)?); // Existing host lease survives GUI restarts.
    let mut registry = hosts()
        .lock()
        .map_err(|_| anyhow::anyhow!("Managed session registry unavailable"))?;
    if let Some(host) = registry.get(id) {
        if admin_status(&host.endpoint).is_ok() {
            bail!("This account already has a managed session in this Switcher process");
        }
    }
    let native = native_executable()?;
    let helper = crate::prompt_cache::helper_executable()?;
    let mut command = Command::new(helper);
    command
        .args(["--prompt-cache-codex-host", id])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let mut host = command
        .spawn()
        .context("Could not start managed cache host")?;
    let startup = (|| -> Result<ManagedHost> {
        let stdout = host.stdout.take().context("Missing host bootstrap pipe")?;
        let (line, stdout) = bounded_line(stdout, Duration::from_secs(10))?;
        let endpoint: Endpoint = serde_json::from_slice(&line).context("Invalid host bootstrap")?;
        ensure!(
            endpoint.port != 0
                && endpoint.token.len() == 64
                && endpoint.token.bytes().all(|b| b.is_ascii_hexdigit()),
            "Invalid host bootstrap"
        );
        // CreateProcess explicitly assigns a new console, inherits NO handles,
        // and uses its own environment block. Bootstrap pipes cannot leak in.
        let host_created = NativeProcess::process_stamp(host.id())?;
        let mut cli = NativeProcess::launch(&native, &endpoint, target)?;
        let ownership = json!({"pid": cli.pid(), "created": cli.created()?});
        let result = (|| -> Result<()> {
            let mut stdin = host.stdin.take().context("Missing host ownership pipe")?;
            serde_json::to_writer(&mut stdin, &ownership)?;
            stdin.write_all(b"\n")?;
            stdin.flush()?;
            drop(stdin);
            let (ack, _) = bounded_line(stdout, Duration::from_secs(5))?;
            ensure!(
                serde_json::from_slice::<Value>(&ack)?.get("ready") == Some(&Value::Bool(true)),
                "Host did not accept CLI ownership"
            );
            Ok(())
        })();
        if result.is_err() {
            cli.cancel_startup();
        }
        result?;
        Ok(ManagedHost {
            endpoint,
            pid: host.id(),
            created: host_created,
        })
    })();
    match startup {
        Ok(managed) => {
            registry.insert(id.to_owned(), managed);
            // Dropping Child detaches; closing/replacing the GUI does not kill
            // the immutable host or the independently created native console.
            let mut opened =
                json!({"opened":true,"provider":"codex","managed":true,"prewarm_verified":false});
            if let Some(target) = target {
                opened["resumed"] = json!(true);
                opened["session_id"] = json!(target.session_id);
            }
            Ok(opened)
        }
        Err(error) => {
            let _ = host.kill(); // Only this newly created, failed bootstrap.
            let _ = host.wait();
            Err(error)
        }
    }
}

fn bounded_line<R: Read + Send + 'static>(
    mut reader: R,
    timeout: Duration,
) -> Result<(Vec<u8>, R)> {
    let (send, receive) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result = (|| -> std::io::Result<Vec<u8>> {
            let mut line = Vec::new();
            loop {
                let mut byte = [0];
                reader.read_exact(&mut byte)?;
                if byte[0] == b'\n' {
                    return Ok(line);
                }
                if line.len() == 1024 {
                    return Err(std::io::Error::other("Bootstrap limit"));
                }
                line.push(byte[0]);
            }
        })();
        let _ = send.send((result, reader));
    });
    let (line, reader) = receive
        .recv_timeout(timeout)
        .context("Host bootstrap timed out")?;
    Ok((line?, reader))
}

fn native_executable() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        let appdata = std::env::var_os("APPDATA").context("APPDATA unavailable")?;
        let path = PathBuf::from(appdata).join("npm/node_modules/@openai/codex/node_modules/@openai/codex-win32-x64/vendor/x86_64-pc-windows-msvc/bin/codex.exe");
        ensure!(
            path.is_file(),
            "Stable npm native Codex binary is not installed"
        );
        // Never search PATH: its first codex.cmd may be the XSwap auth wrapper.
        Ok(path)
    }
    #[cfg(not(windows))]
    {
        bail!("Managed native console launch is currently supported on Windows only")
    }
}

fn validate_resume_target(target: &ResumeTarget) -> Result<()> {
    ensure!(
        target.session_id.len() == 36 && uuid::Uuid::parse_str(&target.session_id).is_ok(),
        "Resume requires an explicit session UUID"
    );
    ensure!(
        target.cwd.is_absolute(),
        "Saved resume directory must be absolute"
    );
    let cwd = target
        .cwd
        .to_str()
        .context("Saved resume directory is not valid Unicode")?;
    ensure!(
        !cwd.chars().any(char::is_control),
        "Invalid saved resume directory"
    );
    if let Some(model) = &target.model {
        ensure!(
            !model.is_empty()
                && model.len() <= 256
                && model.as_bytes()[0].is_ascii_alphanumeric()
                && model
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.:/".contains(&b)),
            "Invalid saved native model"
        );
    }
    if let Some(effort) = &target.effort {
        ensure!(
            matches!(
                effort.as_str(),
                "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max" | "ultra"
            ),
            "Invalid saved reasoning effort"
        );
    }
    Ok(())
}

fn launch_arguments(endpoint: &Endpoint, target: Option<&ResumeTarget>) -> Result<Vec<String>> {
    let mut arguments = vec![
        "-c".into(), "model_provider=\"switcher-cache\"".into(),
        "-c".into(), format!("model_providers.switcher-cache={{name=\"Switcher managed cache\",base_url=\"http://127.0.0.1:{}/v1\",wire_api=\"responses\",env_key=\"{}\",requires_openai_auth=false,supports_websockets=false,request_max_retries=0,stream_max_retries=0}}", endpoint.port, TOKEN_ENV),
    ];
    if let Some(target) = target {
        validate_resume_target(target)?;
        arguments.extend(["--cd".into(), target.cwd.to_str().unwrap().to_owned()]);
        // Explicit saved overrides prevent user/project config from selecting a
        // different model or effort. Missing values remain absent, never defaulted.
        if let Some(model) = &target.model {
            arguments.extend(["--model".into(), model.clone()]);
        }
        if let Some(effort) = &target.effort {
            arguments.extend(["-c".into(), format!("model_reasoning_effort=\"{effort}\"")]);
        }
        // Include exec histories, with the exact UUID as the only positional
        // argument. Never add a prompt, --last, a transcript path, or a fork.
        arguments.extend([
            "resume".into(),
            "--include-non-interactive".into(),
            target.session_id.clone(),
        ]);
    }
    Ok(arguments)
}

#[cfg(windows)]
struct NativeProcess {
    handle: windows_sys::Win32::Foundation::HANDLE,
    process_id: u32,
}
#[cfg(windows)]
impl NativeProcess {
    fn process_stamp(pid: u32) -> Result<u64> {
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        ensure!(!handle.is_null(), "Could not identify cache host");
        Self {
            handle,
            process_id: pid,
        }
        .created()
    }
    fn launch(
        path: &std::path::Path,
        endpoint: &Endpoint,
        target: Option<&ResumeTarget>,
    ) -> Result<Self> {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::{Foundation::CloseHandle, System::Threading::*};
        let application: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut argv = vec![path.to_string_lossy().into_owned()];
        argv.extend(launch_arguments(endpoint, target)?);
        let mut command: Vec<u16> = argv
            .iter()
            .map(|arg| windows_quote(arg))
            .collect::<Vec<_>>()
            .join(" ")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let directory: Option<Vec<u16>> = target.map(|target| {
            target
                .cwd
                .as_os_str()
                .encode_wide()
                .chain(Some(0))
                .collect()
        });
        let mut environment: BTreeMap<String, (std::ffi::OsString, std::ffi::OsString)> =
            std::env::vars_os()
                .filter(|(name, _)| {
                    !matches!(
                        name.to_string_lossy().to_ascii_uppercase().as_str(),
                        "OPENAI_API_KEY" | "OPENAI_BASE_URL" | "CODEX_SWITCHER_CACHE_TOKEN"
                    )
                })
                .map(|(name, value)| (name.to_string_lossy().to_ascii_uppercase(), (name, value)))
                .collect();
        environment.insert(
            TOKEN_ENV.into(),
            (TOKEN_ENV.into(), endpoint.token.clone().into()),
        );
        let mut env = Vec::<u16>::new();
        for (_, (name, value)) in environment {
            env.extend(name.encode_wide());
            env.push(b'=' as u16);
            env.extend(value.encode_wide());
            env.push(0);
        }
        env.push(0);
        let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
        startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        // STARTF_USESTDHANDLES is deliberately absent. Windows gives the child
        // the new console's handles instead of the helper's private pipes.
        let mut information: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        let success = unsafe {
            CreateProcessW(
                application.as_ptr(),
                command.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                CREATE_NEW_CONSOLE | CREATE_UNICODE_ENVIRONMENT,
                env.as_ptr().cast(),
                directory
                    .as_ref()
                    .map_or(std::ptr::null(), |cwd| cwd.as_ptr()),
                &startup,
                &mut information,
            )
        };
        ensure!(success != 0, "Could not create a native Codex console");
        unsafe {
            CloseHandle(information.hThread);
        }
        Ok(Self {
            handle: information.hProcess,
            process_id: information.dwProcessId,
        })
    }
    fn attach(pid: u32, created: u64) -> Result<Self> {
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
        };
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                0,
                pid,
            )
        };
        ensure!(!handle.is_null(), "Could not monitor managed CLI");
        let process = Self {
            handle,
            process_id: pid,
        };
        ensure!(
            process.created()? == created,
            "Managed CLI process identity changed"
        );
        Ok(process)
    }
    fn running(pid: u32, created: u64) -> Result<bool> {
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
        };
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                0,
                pid,
            )
        };
        if handle.is_null() {
            let error = std::io::Error::last_os_error();
            // An exited PID is normal; access/query failures must not release
            // the parent's thread lease as if the owned host had exited.
            if error.raw_os_error()
                == Some(windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER as i32)
            {
                return Ok(false);
            }
            return Err(error).context("Could not query managed host lifetime");
        }
        let process = Self {
            handle,
            process_id: pid,
        };
        Ok(process.created()? == created && process.alive())
    }
    fn created(&self) -> Result<u64> {
        use windows_sys::Win32::{Foundation::FILETIME, System::Threading::GetProcessTimes};
        let mut created: FILETIME = unsafe { std::mem::zeroed() };
        let mut exited: FILETIME = unsafe { std::mem::zeroed() };
        let mut kernel: FILETIME = unsafe { std::mem::zeroed() };
        let mut user: FILETIME = unsafe { std::mem::zeroed() };
        let ok = unsafe {
            GetProcessTimes(
                self.handle,
                &mut created,
                &mut exited,
                &mut kernel,
                &mut user,
            )
        };
        ensure!(ok != 0, "Could not identify managed CLI");
        Ok((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
    }
    fn alive(&self) -> bool {
        unsafe { windows_sys::Win32::System::Threading::WaitForSingleObject(self.handle, 0) == 258 }
    }
    fn pid(&self) -> u32 {
        self.process_id
    }
    fn cancel_startup(&mut self) {
        // Called only when handing this newly created process to its host fails.
        unsafe {
            windows_sys::Win32::System::Threading::TerminateProcess(self.handle, 1);
        }
    }
}
#[cfg(windows)]
impl Drop for NativeProcess {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}
#[cfg(not(windows))]
struct NativeProcess;
#[cfg(not(windows))]
impl NativeProcess {
    fn process_stamp(_: u32) -> Result<u64> {
        bail!("Windows only")
    }
    fn launch(_: &std::path::Path, _: &Endpoint, _: Option<&ResumeTarget>) -> Result<Self> {
        bail!("Windows only")
    }
    fn running(_: u32, _: u64) -> Result<bool> {
        bail!("Windows only")
    }
    fn attach(_: u32, _: u64) -> Result<Self> {
        bail!("Windows only")
    }
    fn created(&self) -> Result<u64> {
        bail!("Windows only")
    }
    fn alive(&self) -> bool {
        false
    }
    fn pid(&self) -> u32 {
        0
    }
    fn cancel_startup(&mut self) {}
}
fn windows_quote(arg: &str) -> String {
    let mut quoted = String::from("\"");
    let mut slashes = 0;
    for ch in arg.chars() {
        if ch == '\\' {
            slashes += 1;
            continue;
        }
        if ch == '"' {
            quoted.extend(std::iter::repeat_n('\\', slashes * 2 + 1));
        } else {
            quoted.extend(std::iter::repeat_n('\\', slashes));
        }
        slashes = 0;
        quoted.push(ch);
    }
    quoted.extend(std::iter::repeat_n('\\', slashes * 2));
    quoted.push('"');
    quoted
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Usage {
    input: Option<i64>,
    cached: Option<i64>,
    written: Option<i64>,
    output: Option<i64>,
}
impl Usage {
    fn read(value: &Value) -> Self {
        fn count(value: Option<&Value>) -> Option<i64> {
            value.and_then(Value::as_i64).filter(|n| *n >= 0)
        }
        Self {
            input: count(value.get("input_tokens")),
            output: count(value.get("output_tokens")),
            cached: count(value.pointer("/input_tokens_details/cached_tokens")),
            written: count(value.pointer("/input_tokens_details/cache_write_tokens")),
        }
    }
    fn cached(&self) -> bool {
        self.cached.unwrap_or(0) > 0 || self.written.unwrap_or(0) > 0
    }
}
struct Capture {
    body: Vec<u8>,
    headers: HeaderMap,
    model: String,
    session: String,
    thread: String,
    anchor: Instant,
    anchor_at: i64,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct View {
    state: String,
    reason: String,
    supported: bool,
    next_at: Option<i64>,
    last_at: Option<i64>,
    requests: u64,
    warm: Usage,
    foreground: Usage,
    model: Option<String>,
    session: Option<String>,
    observed_at: Option<i64>,
}
struct State {
    view: View,
    capture: Option<Arc<Capture>>,
    busy: usize,
    generation: u64,
    warming: bool,
    disabled: bool,
    opted_in: bool,
    last_user: Instant,
}
struct Shared {
    state: Mutex<State>,
    cancel: watch::Sender<u64>,
    forward: Arc<Semaphore>,
    account: String,
    identity: String,
    client: Client,
}
impl Shared {
    fn foreground(self: &Arc<Self>) -> Foreground {
        let mut state = self.state.lock().unwrap();
        state.generation = state.generation.wrapping_add(1);
        let generation = state.generation;
        state.busy += 1;
        state.capture = None;
        state.last_user = Instant::now();
        state.view.next_at = None;
        state.view.state = "busy".into();
        self.cancel.send_replace(generation);
        Foreground {
            shared: self.clone(),
            generation,
        }
    }
    fn stop_auto(&self, reason: &str) {
        let mut state = self.state.lock().unwrap();
        state.disabled = true;
        state.capture = None;
        state.view.supported = false;
        state.view.next_at = None;
        state.view.state = "disabled".into();
        state.view.reason = reason.into();
    }
}
struct Foreground {
    shared: Arc<Shared>,
    generation: u64,
}
impl Drop for Foreground {
    fn drop(&mut self) {
        let mut state = self.shared.state.lock().unwrap();
        state.busy = state.busy.saturating_sub(1);
        if state.busy == 0 && state.view.state == "busy" {
            state.view.state = if state.disabled {
                "disabled"
            } else if !state.opted_in {
                "off"
            } else {
                "waiting_for_request"
            }
            .into();
            if !state.disabled {
                state.view.reason = "원래 CLI 요청은 전달했습니다. 전체 인라인 본문·실제 캐시 사용량이 확인된 요청만 자동 유지합니다.".into();
            }
        }
    }
}

/// main must dispatch this BEFORE initializing Tauri. Only a bounded bootstrap
/// JSON line and ready acknowledgement are written to stdout (never prompts).
pub fn host_entry(account_id: &str) -> Result<()> {
    ensure!(
        valid_account(account_id) && enabled(account_id)?,
        "Account opt-in required"
    );
    let identity = selected_identity(account_id)?;
    let _lease = account_lease(account_id)?;
    let host_created = NativeProcess::process_stamp(std::process::id())?;
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    listener.set_nonblocking(true)?;
    let endpoint = Endpoint {
        port: listener.local_addr()?.port(),
        token: format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        ),
    };
    serde_json::to_writer(std::io::stdout(), &endpoint)?;
    std::io::stdout().write_all(b"\n")?;
    std::io::stdout().flush()?;
    let (ownership, _) = bounded_line(std::io::stdin(), Duration::from_secs(20))?;
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Ownership {
        pid: u32,
        created: u64,
    }
    let ownership: Ownership = serde_json::from_slice(&ownership)?;
    let cli = NativeProcess::attach(ownership.pid, ownership.created)?;
    ensure!(cli.alive(), "Managed CLI already exited");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let client = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .build()?;
    let (cancel, _) = watch::channel(0);
    let shared = Arc::new(Shared { account: account_id.into(), identity, client, cancel, forward: Arc::new(Semaphore::new(1)),
        state: Mutex::new(State { view: View { state: "waiting_for_request".into(), reason: "새 CLI의 전체 요청을 기다립니다. OAuth prewarm 지원은 아직 확인되지 않았습니다.".into(), ..View::default() },
            capture: None, busy: 0, generation: 0, warming: false, disabled: false, opted_in: true, last_user: Instant::now() }) });
    std::io::stdout().write_all(b"{\"ready\":true}\n")?;
    std::io::stdout().flush()?;
    runtime.block_on(async move {
        let listener = TcpListener::from_std(listener)?;
        let connections = Arc::new(Semaphore::new(8));
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    if !cli.alive() { shared.cancel.send_modify(|n| *n = n.wrapping_add(1)); break; }
                    schedule(&shared).await;
                    let _ = write_metadata(&shared, host_created);
                }
                incoming = listener.accept() => {
                    let (stream, peer) = incoming?;
                    if !peer.ip().is_loopback() { continue; }
                    if let Ok(permit) = connections.clone().try_acquire_owned() {
                        let shared = shared.clone(); let token = endpoint.token.clone();
                        tokio::spawn(async move { let _permit = permit; let _ = serve(stream, &token, shared).await; });
                    }
                }
            }
        }
        Ok(())
    })
}

/// Lifetime polling for the parent's dedicated resume helper. Owned hosts take
/// precedence over disk metadata, including when their HTTP endpoint is busy.
pub(crate) fn managed_running(account_id: &str) -> Result<bool> {
    ensure!(valid_account(account_id), "Invalid account ID");
    let owned = hosts()
        .lock()
        .map_err(|_| anyhow::anyhow!("Managed session registry unavailable"))?
        .get(account_id)
        .cloned();
    if let Some(host) = owned {
        // Retain ownership even after exit: metadata for a replacement host
        // must not extend this dedicated helper's per-thread lease.
        return managed_host_running(&host);
    }
    Ok(read_metadata(account_id).is_ok())
}

fn managed_host_running(host: &ManagedHost) -> Result<bool> {
    if admin_status(&host.endpoint).is_ok() {
        // Cache state (including off/disabled/expired) is not process lifetime.
        return Ok(true);
    }
    NativeProcess::running(host.pid, host.created)
}

/// GUI restart reads only fresh metadata with a matching process identity.
/// The reusable private Endpoint bearer is never persisted.
pub fn status(account_id: &str, status: &mut crate::prompt_cache::PromptCacheStatus) -> Result<()> {
    status.auto_keepalive_connected_sessions = 0;
    let endpoint = hosts()
        .lock()
        .map_err(|_| anyhow::anyhow!("Managed session registry unavailable"))?
        .get(account_id)
        .cloned();
    let live = endpoint
        .as_ref()
        .and_then(|host| admin_status(&host.endpoint).ok());
    if live.is_none() && endpoint.is_some() {
        hosts()
            .lock()
            .map_err(|_| anyhow::anyhow!("Managed session registry unavailable"))?
            .remove(account_id);
    }
    let Some(view) = live.or_else(|| read_metadata(account_id).ok()) else {
        status.auto_keepalive_state = if status.auto_keepalive_enabled {
            "needs_managed_session"
        } else {
            "off"
        }
        .into();
        status.auto_keepalive_supported = false;
        status.auto_keepalive_reason =
            "연결된 열린 Codex 세션이 없습니다. 외부 Codex의 전송 경로는 실행 중 자동 연결을 지원하지 않아 연결 도움말이 필요합니다.".into();
        return Ok(());
    };
    status.auto_keepalive_supported = view.supported;
    // An authenticated live host response or fresh process-verified metadata
    // proves one route. Neither proves prewarm support or a cache hit.
    status.auto_keepalive_connected_sessions = u32::from(view.state != "stopped");
    status.auto_keepalive_state = view.state;
    status.auto_keepalive_reason = view.reason;
    status.auto_keepalive_next_at = view.next_at;
    status.auto_keepalive_last_at = view.last_at;
    status.auto_keepalive_requests = view.requests;
    status.auto_keepalive_cache_read_tokens = view.warm.cached;
    status.auto_keepalive_output_tokens = view.warm.output;
    status.model = view.model;
    status.session_id = view.session;
    status.cache_read_tokens = view.foreground.cached;
    status.cache_write_tokens = view.foreground.written;
    status.uncached_input_tokens = view
        .foreground
        .input
        .zip(view.foreground.cached)
        .zip(view.foreground.written)
        .and_then(|((input, cached), written)| input.checked_sub(cached)?.checked_sub(written))
        .filter(|n| *n >= 0);
    // OAuth does not report a cache expiry. Do not manufacture an official
    // expiry/TTL from the public API's 30-minute contract or status polling.
    if view.observed_at.is_some() {
        status.state = "observed".into();
        status.observed_at = view.observed_at;
    }
    status.reason = Some(
        "연결된 관리 CLI에서 관측한 실제 사용량입니다. OAuth 캐시 만료시각은 미확인입니다.".into(),
    );
    Ok(())
}
fn account_lease(id: &str) -> Result<std::fs::File> {
    ensure!(valid_account(id), "Invalid account ID");
    let path = crate::auth::get_config_dir()?.join(format!("native-cache-{id}.lock"));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    file.try_lock().map_err(|_| {
        anyhow::anyhow!("이 계정에는 이미 관리 CLI 세션이 있습니다. 기존 세션을 계속 사용하세요.")
    })?;
    Ok(file)
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Metadata {
    version: u8,
    account: String,
    identity: String,
    host_pid: u32,
    host_created: u64,
    heartbeat: i64,
    view: View,
}
fn write_metadata(shared: &Shared, host_created: u64) -> Result<()> {
    let metadata = Metadata {
        version: 1,
        account: shared.account.clone(),
        identity: shared.identity.clone(),
        host_pid: std::process::id(),
        host_created,
        heartbeat: now(),
        view: shared.state.lock().unwrap().view.clone(),
    };
    let bytes = serde_json::to_vec(&metadata)?;
    ensure!(bytes.len() <= MAX_FRAME, "Metadata limit");
    let root = crate::auth::get_config_dir()?;
    let temporary = root.join(format!(
        ".native-cache-{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));
    let result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(
            &temporary,
            root.join(format!("native-cache-{}.json", shared.account)),
        )?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}
fn read_metadata(id: &str) -> Result<View> {
    ensure!(valid_account(id), "Invalid account ID");
    // Strictly primary-only. A .bak cannot resurrect an old host or budget.
    let file = std::fs::File::open(
        crate::auth::get_config_dir()?.join(format!("native-cache-{id}.json")),
    )?;
    let mut bytes = Vec::new();
    file.take((MAX_FRAME + 1) as u64).read_to_end(&mut bytes)?;
    let metadata = parse_metadata(&bytes, id, &selected_identity(id)?, now())?;
    let process = NativeProcess::attach(metadata.host_pid, metadata.host_created)?;
    ensure!(process.alive(), "Cache host exited");
    Ok(metadata.view)
}
fn parse_metadata(bytes: &[u8], account: &str, identity: &str, at: i64) -> Result<Metadata> {
    ensure!(bytes.len() <= MAX_FRAME, "Metadata limit");
    let metadata: Metadata = serde_json::from_slice(bytes)?;
    ensure!(
        metadata.version == 1 && metadata.account == account && metadata.identity == identity,
        "Metadata identity mismatch"
    );
    ensure!(
        metadata.heartbeat <= at && at.saturating_sub(metadata.heartbeat) <= 15,
        "Stale host heartbeat"
    );
    ensure!(
        metadata.host_pid > 0 && metadata.host_created > 0 && metadata.view.requests <= MAX_WARMS,
        "Invalid host metadata"
    );
    ensure!(
        matches!(
            metadata.view.state.as_str(),
            "waiting_for_request"
                | "busy"
                | "awaiting_prewarm"
                | "prewarming"
                | "active"
                | "disabled"
                | "off"
                | "limit_reached"
                | "expired"
                | "authentication_required"
        ),
        "Invalid host state"
    );
    ensure!(
        metadata.view.reason.len() <= 512 && !metadata.view.reason.chars().any(char::is_control),
        "Invalid host reason"
    );
    for value in [&metadata.view.model, &metadata.view.session] {
        ensure!(
            value.as_ref().is_none_or(|s| !s.is_empty()
                && s.len() <= 256
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.:/".contains(&b))),
            "Invalid host identifier"
        );
    }
    for usage in [&metadata.view.foreground, &metadata.view.warm] {
        ensure!(
            [usage.input, usage.output, usage.cached, usage.written]
                .iter()
                .all(|n| n.is_none_or(|n| n >= 0)),
            "Invalid usage metadata"
        );
    }
    Ok(metadata)
}
fn admin_status(endpoint: &Endpoint) -> Result<View> {
    let address: SocketAddr = ([127, 0, 0, 1], endpoint.port).into();
    let mut stream = BlockingStream::connect_timeout(&address, Duration::from_millis(500))?;
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
    write!(stream, "GET /status HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n", endpoint.port, endpoint.token)?;
    let mut bytes = Vec::new();
    stream
        .take((MAX_FRAME + 1) as u64)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_FRAME && bytes.starts_with(b"HTTP/1.1 200 "),
        "Invalid host status"
    );
    let end = bytes
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .context("Invalid host status")?;
    Ok(serde_json::from_slice(&bytes[end + 4..])?)
}

struct Head {
    method: String,
    route: String,
    headers: BTreeMap<String, String>,
    length: usize,
    body: Vec<u8>,
}
async fn read_head(stream: &mut TcpStream, token: &str) -> Result<Head> {
    let mut buffer = Vec::new();
    let end = loop {
        if let Some(end) = buffer.windows(4).position(|part| part == b"\r\n\r\n") {
            break end;
        }
        ensure!(buffer.len() <= MAX_HEADERS, "Header limit");
        let mut bytes = [0; 1024];
        let n = stream.read(&mut bytes).await?;
        ensure!(n > 0, "Incomplete headers");
        buffer.extend_from_slice(&bytes[..n]);
    };
    ensure!(end <= MAX_HEADERS, "Header limit");
    let text = std::str::from_utf8(&buffer[..end])?;
    let mut lines = text.split("\r\n");
    let parts: Vec<_> = lines.next().unwrap_or_default().split(' ').collect();
    ensure!(
        parts.len() == 3 && parts[2] == "HTTP/1.1",
        "Invalid HTTP request"
    );
    let (method, route) = (parts[0].to_owned(), parts[1].to_owned());
    let mut headers = BTreeMap::new();
    for line in lines {
        let (name, value) = line.split_once(':').context("Invalid header")?;
        ensure!(
            !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            "Invalid header"
        );
        let value = value.trim();
        ensure!(!value.chars().any(char::is_control), "Invalid header");
        ensure!(
            headers
                .insert(name.to_ascii_lowercase(), value.to_owned())
                .is_none(),
            "Duplicate header"
        );
    }
    ensure!(
        headers
            .get("authorization")
            .is_some_and(|value| token_matches(value, token)),
        "Unauthorized"
    );
    ensure!(
        !headers.contains_key("transfer-encoding") && !headers.contains_key("content-encoding"),
        "Encoded requests unsupported"
    );
    let length = headers
        .get("content-length")
        .map(|v| v.parse::<usize>())
        .transpose()?
        .unwrap_or(0);
    // MAX_BODY limits capture only; large foreground uploads are streamed.
    let body = buffer[end + 4..].to_vec();
    ensure!(body.len() <= length, "Pipelined request rejected");
    Ok(Head {
        method,
        route,
        headers,
        length,
        body,
    })
}
fn token_matches(value: &str, token: &str) -> bool {
    let Some(value) = value.strip_prefix("Bearer ") else {
        return false;
    };
    if value.len() != token.len() {
        return false;
    }
    value
        .bytes()
        .zip(token.bytes())
        .fold(0u8, |diff, (a, b)| diff | (a ^ b))
        == 0
}
async fn small_response(
    stream: &mut (impl AsyncWriteExt + Unpin),
    code: u16,
    bytes: &[u8],
) -> Result<()> {
    let headers = format!("HTTP/1.1 {code} Result\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", bytes.len());
    stream.write_all(headers.as_bytes()).await?;
    stream.write_all(bytes).await?;
    stream.shutdown().await?;
    Ok(())
}
// Uploads above the capture limit still pass through in bounded chunks. The
// currently unified reqwest 0.13.5 feature set includes `stream`; keep it enabled.
type Peer = Arc<tokio::sync::Mutex<tokio::net::tcp::OwnedReadHalf>>;
async fn disconnected(peer: Peer, mut uploaded: watch::Receiver<bool>) {
    while !*uploaded.borrow() {
        if uploaded.changed().await.is_err() {
            return;
        }
    }
    // After consuming the request, EOF (or unsupported pipelining) means this
    // response no longer has a live downstream owner. Drop upstream immediately.
    let mut byte = [0u8];
    let _ = peer.lock().await.read(&mut byte).await;
}
fn streaming_body(
    initial: Vec<u8>,
    remaining: usize,
    peer: Peer,
    uploaded: watch::Sender<bool>,
) -> reqwest::Body {
    let state = (Some(initial), remaining, peer, uploaded);
    reqwest::Body::wrap_stream(futures::stream::unfold(
        state,
        |(mut initial, mut remaining, peer, uploaded)| async move {
            if let Some(bytes) = initial.take().filter(|bytes| !bytes.is_empty()) {
                if remaining == 0 {
                    uploaded.send_replace(true);
                }
                return Some((
                    Ok::<Vec<u8>, std::io::Error>(bytes),
                    (initial, remaining, peer, uploaded),
                ));
            }
            if remaining == 0 {
                uploaded.send_replace(true);
                return None;
            }
            let mut bytes = vec![0; remaining.min(MAX_FRAME)];
            let result =
                tokio::time::timeout(Duration::from_secs(30), peer.lock().await.read(&mut bytes))
                    .await;
            let result = match result {
                Ok(Ok(n)) if n > 0 => {
                    remaining -= n;
                    bytes.truncate(n);
                    if remaining == 0 {
                        uploaded.send_replace(true);
                    }
                    Ok(bytes)
                }
                Ok(Ok(_)) => Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "Upload ended",
                )),
                Ok(Err(error)) => Err(error),
                Err(_) => Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "Upload timed out",
                )),
            };
            if result.is_err() {
                remaining = 0;
                uploaded.send_replace(true);
            }
            Some((result, (initial, remaining, peer, uploaded)))
        },
    ))
}
async fn serve(mut stream: TcpStream, token: &str, shared: Arc<Shared>) -> Result<()> {
    let mut head =
        match tokio::time::timeout(Duration::from_secs(10), read_head(&mut stream, token)).await {
            Ok(Ok(head)) => head,
            _ => {
                return small_response(
                    &mut stream,
                    400,
                    b"{\"error\":\"invalid private proxy request\"}",
                )
                .await;
            }
        };
    if head.method == "GET" && head.route == "/status" && head.length == 0 {
        let bytes = serde_json::to_vec(&shared.state.lock().unwrap().view)?;
        return small_response(&mut stream, 200, &bytes).await;
    }
    let Some((upstream, models)) = route_kind(&head.method, &head.route) else {
        return small_response(&mut stream, 404, b"{\"error\":\"route unavailable\"}").await;
    };
    let foreground = (!models).then(|| shared.foreground());
    let _slot = shared.forward.clone().acquire_owned().await?;
    let (reader, mut writer) = stream.into_split();
    let peer = Arc::new(tokio::sync::Mutex::new(reader));
    let (uploaded, upload_done) = watch::channel(false);
    let departed = disconnected(peer.clone(), upload_done);
    let work = async {
        let started = Instant::now();
        let started_at = now();
        let (body, template) = if head.length <= MAX_BODY {
            let received = head.body.len();
            head.body.resize(head.length, 0);
            tokio::time::timeout(
                Duration::from_secs(30),
                peer.lock().await.read_exact(&mut head.body[received..]),
            )
            .await??;
            uploaded.send_replace(true);
            let template = if !models && head.route == "/v1/responses" {
                capture(&head.body, &head.headers, started, started_at).ok()
            } else {
                None
            };
            (
                reqwest::Body::from(std::mem::take(&mut head.body)),
                template,
            )
        } else {
            let remaining = head.length - head.body.len();
            (
                streaming_body(
                    std::mem::take(&mut head.body),
                    remaining,
                    peer.clone(),
                    uploaded.clone(),
                ),
                None,
            )
        };
        let auth = match tokio::time::timeout(
            Duration::from_secs(30),
            fresh_auth(&shared.account, &shared.identity),
        )
        .await
        {
            Ok(Ok(auth)) => auth,
            _ => {
                authentication_wait(&shared);
                return small_response(
                    &mut writer,
                    502,
                    b"{\"error\":\"selected account authentication unavailable\"}",
                )
                .await;
            }
        };
        let mut request = if models {
            shared.client.get(upstream)
        } else {
            shared
                .client
                .post(upstream)
                .body(body)
                .header("content-length", head.length)
        };
        if models {
            let version = head
                .route
                .strip_prefix("/v1/models?client_version=")
                .unwrap_or("0.162.1");
            request = request.query(&[("client_version", version)]);
        }
        request = request
            .headers(forward_headers(&head.headers))
            .bearer_auth(auth.0)
            .header("chatgpt-account-id", auth.1)
            .header("accept-encoding", "identity");
        if !models {
            request = request
                .header("content-type", "application/json")
                .header("accept", "text/event-stream");
        }
        let response = match tokio::time::timeout(Duration::from_secs(60), request.send()).await {
            Ok(Ok(response)) => response,
            _ => {
                return small_response(&mut writer, 502, b"{\"error\":\"upstream unavailable\"}")
                    .await
            }
        };
        let successful = response.status().is_success();
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            authentication_wait(&shared);
        }
        let observation = relay(&mut writer, response).await?;
        if !models && observation.completed {
            let mut state = shared.state.lock().unwrap();
            let model = observation
                .model
                .or_else(|| template.as_ref().map(|capture| capture.model.clone()));
            if state.view.model != model {
                state.view.supported = false;
            }
            state.view.foreground = observation.usage.clone();
            state.view.model = model;
            state.view.session = head
                .headers
                .get("thread-id")
                .filter(|s| s.len() <= 128)
                .cloned();
            state.view.observed_at = Some(started_at); // Including zero-hit receipts.
            if let (Some(guard), Some(template)) = (foreground.as_ref(), template) {
                if successful
                    && observation.usage.cached()
                    && state.generation == guard.generation
                    && state.busy == 1
                    && state.opted_in
                    && !state.disabled
                {
                    state.view.state = "awaiting_prewarm".into();
                    state.view.reason="전체 요청을 캡처했습니다. OAuth의 출력 없는 prewarm 지원은 첫 완료 응답으로 확인합니다.".into();
                    state.view.next_at = Some(started_at + WARM_AFTER as i64);
                    state.capture = Some(Arc::new(template));
                }
            }
        }
        Ok(())
    };
    tokio::select! { biased; _=departed=>Ok(()), result=work=>result }
}
fn route_kind(method: &str, route: &str) -> Option<(&'static str, bool)> {
    match (method, route) {
        ("POST", "/v1/responses") => Some((RESPONSES, false)),
        ("POST", "/v1/responses/compact") => Some((
            "https://chatgpt.com/backend-api/codex/responses/compact",
            false,
        )),
        // Do not concatenate a client URL/query with an upstream URL.
        ("GET", path)
            if path == "/v1/models"
                || path
                    .strip_prefix("/v1/models?client_version=")
                    .is_some_and(|v| {
                        !v.is_empty()
                            && v.len() <= 64
                            && v.bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
                    }) =>
        {
            Some((MODELS, true))
        }
        _ => None,
    }
}
fn forward_headers(headers: &BTreeMap<String, String>) -> HeaderMap {
    let mut result = HeaderMap::new();
    for name in AFFINITY {
        if let Some(value) = headers.get(*name) {
            if let (Ok(name), Ok(value)) = (
                reqwest::header::HeaderName::from_bytes(name.as_bytes()),
                reqwest::header::HeaderValue::from_str(value),
            ) {
                result.insert(name, value);
            }
        }
    }
    result
}
async fn fresh_auth(id: &str, identity: &str) -> Result<(String, String)> {
    let id = id.to_owned();
    let identity = identity.to_owned();
    // Own any token refresh to completion even if a warm request is preempted.
    // The existing owner guard may defer active-account refresh to Codex. Do
    // not bypass it or send a known-expired credential as if it were fresh.
    tokio::spawn(async move {
        let account = crate::auth::get_account(&id)?.context("Account removed")?;
        let account = crate::auth::ensure_chatgpt_tokens_fresh(&account).await?;
        ensure!(account.id == id, "Account changed");
        match account.auth_data {
            crate::types::AuthData::ChatGPT {
                access_token,
                account_id: Some(account_id),
                ..
            } if account_id == identity && usable_access(&access_token, now()) => {
                Ok((access_token, account_id))
            }
            _ => bail!("Account identity changed"),
        }
    })
    .await?
}

fn usable_access(token: &str, at: i64) -> bool {
    use base64::Engine;
    if token.is_empty() || token.len() > MAX_HEADERS {
        return false;
    }
    let mut parts = token.split('.');
    let _header = parts.next();
    let payload = parts.next();
    if parts.next().is_none() || parts.next().is_some() {
        return true; // Opaque tokens have no locally authoritative expiry.
    }
    let expiry = payload
        .and_then(|s| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(s)
                .ok()
        })
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|value| value.get("exp").and_then(Value::as_i64));
    expiry.is_none_or(|expiry| expiry > at.saturating_add(60))
}

#[derive(Debug)]
struct AuthenticationUnavailable;
impl std::fmt::Display for AuthenticationUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Managed account authentication unavailable")
    }
}
impl std::error::Error for AuthenticationUnavailable {}

fn authentication_wait(shared: &Shared) {
    let mut state = shared.state.lock().unwrap();
    state.capture = None;
    state.view.supported = false;
    state.view.next_at = None;
    state.view.state = "authentication_required".into();
    state.view.reason = "계정 인증의 갱신을 기다립니다. 실행 중인 Codex의 인증 소유권을 유지하며, 인증이 복구된 뒤 이 CLI에서 새 요청을 보내면 다시 관측합니다.".into();
}
fn modern_model(model: &str) -> bool {
    let Some(version) = model.strip_prefix("gpt-") else {
        return false;
    };
    let version = version.split('-').next().unwrap_or_default();
    let mut numbers = version.split('.');
    let major = numbers.next().and_then(|s| s.parse::<u32>().ok());
    let minor = match numbers.next() {
        None => 0,
        Some(s) => match s.parse::<u32>() {
            Ok(n) => n,
            Err(_) => return false,
        },
    };
    numbers.next().is_none() && major.is_some_and(|major| major > 5 || (major == 5 && minor >= 6))
}
fn full_inline(body: &Value) -> bool {
    if !body.is_object()
        || ["previous_response_id", "conversation", "conversation_id"]
            .iter()
            .any(|key| body.get(*key).is_some())
    {
        return false;
    }
    let Some(input) = body
        .get("input")
        .and_then(Value::as_array)
        .filter(|input| !input.is_empty())
    else {
        return false;
    };
    fn inline(value: &Value) -> bool {
        match value {
            Value::Array(values) => values.iter().all(inline),
            Value::Object(values) => {
                !values
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| matches!(kind, "item_reference" | "reference"))
                    && !values.contains_key("file_id")
                    && !values.contains_key("image_url")
                    && !values.contains_key("audio_url")
                    && values.values().all(inline)
            }
            _ => true,
        }
    }
    input.iter().all(|item| item.is_object() && inline(item))
}
fn safe_tools(body: &Value) -> bool {
    fn safe(tool: &Value) -> bool {
        match tool.get("type").and_then(Value::as_str) {
            Some("function" | "custom") => true,
            Some("namespace") => tool
                .get("tools")
                .and_then(Value::as_array)
                .is_some_and(|tools| tools.iter().all(safe)),
            _ => false,
        }
    }
    body.get("tools")
        .is_none_or(|tools| tools.as_array().is_some_and(|tools| tools.iter().all(safe)))
}
fn capture(
    raw: &[u8],
    headers: &BTreeMap<String, String>,
    anchor: Instant,
    anchor_at: i64,
) -> Result<Capture> {
    ensure!(
        raw.len() <= MAX_BODY
            && !headers.contains_key("x-parent-session-id")
            && !headers.contains_key("x-openai-subagent"),
        "Capture unavailable"
    );
    let body: Value = serde_json::from_slice(raw)?;
    ensure!(
        full_inline(&body) && safe_tools(&body),
        "Full inline stateless input and client tools required"
    );
    ensure!(
        body.get("store") == Some(&Value::Bool(false))
            && body.get("stream") == Some(&Value::Bool(true)),
        "Only stateless streaming requests are eligible"
    );
    crate::prompt_cache_body::prewarm_body(raw)?; // Reject duplicate control fields without reserializing the prompt.
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| modern_model(model))
        .context("Prewarm model unsupported")?;
    let session = headers
        .get("session-id")
        .filter(|s| !s.is_empty() && s.len() <= 256)
        .context("Missing session affinity")?;
    let thread = headers
        .get("thread-id")
        .filter(|s| !s.is_empty() && s.len() <= 128)
        .context("Missing thread identity")?;
    Ok(Capture {
        body: raw.to_vec(),
        headers: forward_headers(headers),
        model: model.into(),
        session: session.clone(),
        thread: thread.clone(),
        anchor,
        anchor_at,
    })
}
fn prewarm_body(raw: &[u8]) -> Result<Vec<u8>> {
    let body: Value = serde_json::from_slice(raw)?;
    ensure!(
        full_inline(&body) && safe_tools(&body),
        "Unsafe prewarm input"
    );
    ensure!(
        body.get("store") == Some(&Value::Bool(false))
            && body.get("stream") == Some(&Value::Bool(true)),
        "Stateless SSE required"
    );
    let result = crate::prompt_cache_body::prewarm_body(raw)?;
    ensure!(result.len() <= MAX_BODY, "Prewarm body limit");
    Ok(result)
}
#[derive(Default)]
struct Observation {
    model: Option<String>,
    usage: Usage,
    completed: bool,
    empty_output: bool,
    output_seen: bool,
    invalid: bool,
}
impl Observation {
    fn event(&mut self, event: &Value, strict: bool) {
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        let response = event.get("response").unwrap_or(event);
        if response
            .get("output")
            .and_then(Value::as_array)
            .is_some_and(|items| !items.is_empty())
        {
            self.output_seen = true;
        }
        if kind.starts_with("response.output")
            || kind.contains(".delta")
            || kind.contains("function_call")
            || kind.contains("tool_call")
        {
            self.output_seen = true;
        }
        let terminal = matches!(kind, "response.completed" | "response.done")
            || (kind.is_empty()
                && response.get("status").and_then(Value::as_str) == Some("completed"));
        if terminal {
            self.model = response
                .get("model")
                .and_then(Value::as_str)
                .filter(|s| {
                    s.len() <= 256
                        && s.bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"-_.:/".contains(&b))
                })
                .map(str::to_owned);
            self.completed = true;
            self.empty_output = response
                .get("output")
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty);
            self.usage = Usage::read(&response["usage"]);
        } else if strict
            && !matches!(
                kind,
                "response.created" | "response.in_progress" | "response.queued"
            )
        {
            self.invalid = true;
        }
        if matches!(kind, "error" | "response.failed" | "response.incomplete")
            || response.get("error").is_some_and(|v| !v.is_null())
        {
            self.invalid = true;
        }
    }
    fn confirmed_prewarm(&self) -> bool {
        self.completed
            && self.empty_output
            && !self.output_seen
            && !self.invalid
            && self.usage.output == Some(0)
            && self.usage.input.is_some()
            && self.usage.cached.is_some()
            && self.usage.cached()
    }
}
struct Frames {
    pending: Vec<u8>,
    data: Vec<u8>,
    observation: Observation,
    strict: bool,
}
impl Frames {
    fn new(strict: bool) -> Self {
        Self {
            pending: Vec::new(),
            data: Vec::new(),
            observation: Observation::default(),
            strict,
        }
    }
    fn feed(&mut self, bytes: &[u8]) -> Result<()> {
        for byte in bytes {
            ensure!(self.pending.len() < MAX_FRAME, "SSE line limit");
            self.pending.push(*byte);
            if *byte != b'\n' {
                continue;
            }
            let line = self.pending.strip_suffix(b"\n").unwrap_or(&self.pending);
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if line.is_empty() {
                if !self.data.is_empty() && self.data != b"[DONE]" {
                    let event: Value = serde_json::from_slice(&self.data)?;
                    self.observation.event(&event, self.strict);
                }
                self.data.clear();
            } else if let Some(data) = line.strip_prefix(b"data:") {
                let data = data.strip_prefix(b" ").unwrap_or(data);
                ensure!(
                    self.data.len() + data.len() + 1 <= MAX_FRAME,
                    "SSE event limit"
                );
                if !self.data.is_empty() {
                    self.data.push(b'\n');
                }
                self.data.extend_from_slice(data);
            }
            self.pending.clear();
            if self.strict && (self.observation.output_seen || self.observation.invalid) {
                bail!("Prewarm returned output or an unsupported event");
            }
        }
        Ok(())
    }
}
async fn relay(
    stream: &mut (impl AsyncWriteExt + Unpin),
    mut response: reqwest::Response,
) -> Result<Observation> {
    let code = response.status().as_u16();
    let sse = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/event-stream"));
    let mut headers =
        format!("HTTP/1.1 {code} Upstream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n");
    for (name, value) in response.headers() {
        if matches!(
            name.as_str(),
            "connection"
                | "transfer-encoding"
                | "content-length"
                | "trailer"
                | "keep-alive"
                | "proxy-authenticate"
                | "proxy-authorization"
                | "te"
                | "upgrade"
        ) {
            continue;
        }
        if let Ok(value) = value.to_str() {
            headers.push_str(name.as_str());
            headers.push_str(": ");
            headers.push_str(value);
            headers.push_str("\r\n");
        }
    }
    headers.push_str("\r\n");
    stream.write_all(headers.as_bytes()).await?;
    let mut frames = Frames::new(false);
    let mut telemetry_ok = true;
    let mut json_body = Vec::new();
    loop {
        let chunk = tokio::time::timeout(Duration::from_secs(600), response.chunk()).await??;
        let Some(chunk) = chunk else {
            break;
        };
        if chunk.is_empty() {
            continue;
        }
        if telemetry_ok {
            if sse {
                if frames.feed(&chunk).is_err() {
                    telemetry_ok = false;
                }
            } else if json_body.len() + chunk.len() <= MAX_FRAME {
                json_body.extend_from_slice(&chunk);
            } else {
                telemetry_ok = false;
            }
        }
        let prefix = format!("{:x}\r\n", chunk.len());
        tokio::time::timeout(Duration::from_secs(10), async {
            stream.write_all(prefix.as_bytes()).await?;
            stream.write_all(&chunk).await?;
            stream.write_all(b"\r\n").await
        })
        .await??;
    }
    stream.write_all(b"0\r\n\r\n").await?;
    stream.shutdown().await?;
    if !telemetry_ok {
        return Ok(Observation::default());
    }
    if !sse {
        if let Ok(body) = serde_json::from_slice::<Value>(&json_body) {
            frames.observation.event(&body, false);
        }
    }
    Ok(frames.observation)
}

async fn schedule(shared: &Arc<Shared>) {
    let id = shared.account.clone();
    let opted_in = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::task::spawn_blocking(move || enabled(&id)),
    )
    .await;
    let opted_in = matches!(opted_in, Ok(Ok(Ok(true))));
    let mut state = shared.state.lock().unwrap();
    if !opted_in {
        state.opted_in = false;
        state.capture = None;
        state.view.next_at = None;
        state.view.state = "off".into();
        state.view.reason = "자동 유지를 껐습니다. 원래 CLI 요청은 계속 전달합니다.".into();
        shared.cancel.send_modify(|n| *n = n.wrapping_add(1));
        return;
    }
    state.opted_in = true;
    if state.disabled || state.busy > 0 || state.warming {
        return;
    }
    if state.view.requests >= MAX_WARMS || state.last_user.elapsed().as_secs() >= MAX_IDLE {
        state.capture = None;
        state.view.next_at = None;
        state.view.state = "limit_reached".into();
        state.view.reason =
            "이 관리 세션의 유지 요청 12회 또는 유휴 12시간 제한에 도달했습니다.".into();
        return;
    }
    let Some(capture) = state.capture.clone() else {
        return;
    };
    if capture.anchor.elapsed().as_secs() >= TTL {
        state.capture = None;
        state.view.next_at = None;
        state.view.state = "expired".into();
        state.view.reason = "보수적 30분 예상 기간을 지나 유지 요청을 보내지 않습니다.".into();
        return;
    }
    if capture.anchor.elapsed().as_secs() < WARM_AFTER {
        return;
    }
    let generation = state.generation;
    state.warming = true;
    state.view.state = "prewarming".into();
    let cancellation = shared.cancel.subscribe();
    drop(state);
    let shared = shared.clone();
    tokio::spawn(async move {
        warm(shared, capture, generation, cancellation).await;
    });
}
async fn warm(
    shared: Arc<Shared>,
    capture: Arc<Capture>,
    generation: u64,
    mut cancellation: watch::Receiver<u64>,
) {
    let warm_started = Instant::now();
    let warm_started_at = now();
    let request = async {
        let body = prewarm_body(&capture.body)?;
        let auth = fresh_auth(&shared.account, &shared.identity)
            .await
            .map_err(|_| AuthenticationUnavailable)?;
        {
            let mut state = shared.state.lock().unwrap();
            ensure!(
                state.generation == generation
                    && state.busy == 0
                    && state.opted_in
                    && !state.disabled
                    && capture.anchor.elapsed().as_secs() < TTL,
                "Warm preempted"
            );
            state.view.requests += 1;
            state.view.last_at = Some(now());
            // Until a terminal usage receipt arrives, cost/output is unknown.
            state.view.warm = Usage::default();
        }
        let mut response = shared
            .client
            .post(RESPONSES)
            .headers(capture.headers.clone())
            .bearer_auth(auth.0)
            .header("chatgpt-account-id", auth.1)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .header("accept-encoding", "identity")
            .body(body)
            .send()
            .await?;
        ensure!(response.status().is_success(), "Prewarm rejected");
        let sse = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/event-stream"));
        let mut frames = Frames::new(true);
        let mut json_body = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if sse {
                frames.feed(&chunk)?;
                if frames.observation.completed {
                    break;
                }
            } else {
                ensure!(
                    json_body.len() + chunk.len() <= MAX_FRAME,
                    "Prewarm response limit"
                );
                json_body.extend_from_slice(&chunk);
            }
        }
        if !sse {
            frames
                .observation
                .event(&serde_json::from_slice::<Value>(&json_body)?, true);
        }
        ensure!(
            frames.observation.confirmed_prewarm(),
            "Prewarm zero-output cache usage not confirmed"
        );
        Ok::<Usage, anyhow::Error>(frames.observation.usage)
    };
    let result = tokio::select! { biased;
        _ = cancellation.changed() => None,
        result = tokio::time::timeout(Duration::from_secs(WARM_DEADLINE), request) => Some(result),
    };
    let mut state = shared.state.lock().unwrap();
    state.warming = false;
    if state.generation != generation || !state.opted_in {
        return;
    }
    match result {
        Some(Ok(Ok(usage))) => {
            state.view.warm = usage;
            state.view.supported = true;
            state.view.state = "active".into();
            state.view.reason = "이 세션의 prewarm 완료와 출력 0을 확인했습니다. 입력 캐시 비용은 발생할 수 있으며 만료는 API 계약 기준의 추정입니다.".into();
            let refreshed = Capture {
                body: capture.body.clone(),
                headers: capture.headers.clone(),
                model: capture.model.clone(),
                session: capture.session.clone(),
                thread: capture.thread.clone(),
                anchor: warm_started,
                anchor_at: warm_started_at,
            };
            state.view.next_at = Some(refreshed.anchor_at + WARM_AFTER as i64);
            state.capture = Some(Arc::new(refreshed));
        }
        None => {
            state.capture = None;
            state.view.next_at = None;
        }
        Some(Ok(Err(error))) if error.is::<AuthenticationUnavailable>() => {
            drop(state);
            authentication_wait(&shared);
        }
        _ => {
            drop(state);
            shared.stop_auto("출력 없는 prewarm을 확인하지 못해 이 호스트의 자동 유지를 중단했습니다. 생성 요청으로 대체하지 않으며 원래 CLI는 계속 사용할 수 있습니다.");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn body() -> Value {
        json!({"model":"gpt-6.1-sol","instructions":"fixed","input":[{"role":"user","content":[{"type":"input_text","text":"private prompt"}]}],"tools":[{"type":"function","name":"local_tool","parameters":{"type":"object"}}],"prompt_cache_key":"original-key","prompt_cache_options":{"ttl":"30m","mode":"implicit"},"store":false,"stream":true,"reasoning":{"effort":"high"},"max_output_tokens":32})
    }
    #[test]
    fn replay_changes_only_documented_non_prefix_options() {
        let original = body();
        let raw = serde_json::to_vec(&original).unwrap();
        let mut expected = original.clone();
        expected["prompt_cache_options"]["prewarm"] = json!(true);
        expected["store"] = json!(false);
        expected["stream"] = json!(true);
        assert_eq!(
            serde_json::from_slice::<Value>(&prewarm_body(&raw).unwrap()).unwrap(),
            expected
        );
        assert_eq!(original["input"][0]["content"][0]["text"], "private prompt");
    }
    #[test]
    fn raw_replay_preserves_nested_order_escapes_whitespace_and_all_other_bytes() {
        let raw=br#" {"store":false, "stream":true,"input":[{"z":"a\\b\"},[","a":2}],"tools":[{"name":"f","type":"function","parameters":{"z":1,"a":2}}],"model":"gpt-6.1-sol", "prompt_cache_options" : {"ttl":"30m", "prewarm" : false},"reasoning":{"z":1,"a":2}} "#;
        let expected = String::from_utf8(raw.to_vec()).unwrap().replacen(
            "\"prewarm\" : false",
            "\"prewarm\" : true",
            1,
        );
        assert_eq!(prewarm_body(raw).unwrap(), expected.as_bytes());
        for options in [
            "",
            ",\"prompt_cache_options\":{}",
            ",\"prompt_cache_options\":{ \"ttl\":\"30m\" }",
        ] {
            let raw=format!("{{\"stream\":true,\"store\":false,\"model\":\"gpt-6.1-sol\",\"input\":[{{\"z\":1,\"a\":2}}]{options}}}");
            let warm = prewarm_body(raw.as_bytes()).unwrap();
            assert!(String::from_utf8_lossy(&warm).contains("{\"z\":1,\"a\":2}"));
            assert_eq!(
                serde_json::from_slice::<Value>(&warm).unwrap()["prompt_cache_options"]["prewarm"],
                true
            );
        }
        for options in [
            ",\"prompt_cache_options\":{},\"prompt_cache_options\":{}",
            ",\"prompt_cache_options\":{\"prewarm\":false,\"prewarm\":true}",
            ",\"prompt_cache_options\":{\"prewarm\":false,\"prew\\u0061rm\":true}",
        ] {
            let raw=format!("{{\"stream\":true,\"store\":false,\"input\":[{{\"role\":\"user\",\"content\":\"hi\"}}]{options}}}");
            assert!(prewarm_body(raw.as_bytes()).is_err());
        }
    }
    #[test]
    fn stateful_partial_hosted_and_external_inputs_are_never_replayed() {
        for field in ["previous_response_id", "conversation", "conversation_id"] {
            let mut b = body();
            b[field] = Value::Null;
            assert!(prewarm_body(&serde_json::to_vec(&b).unwrap()).is_err());
        }
        for input in [
            json!([]),
            json!("text"),
            json!([{"type":"item_reference","id":"item"}]),
            json!([{"role":"user","content":[{"type":"input_file","file_id":"file"}]}]),
        ] {
            let mut b = body();
            b["input"] = input;
            assert!(prewarm_body(&serde_json::to_vec(&b).unwrap()).is_err());
        }
        let mut b = body();
        b["tools"] = json!([{"type":"web_search"}]);
        assert!(prewarm_body(&serde_json::to_vec(&b).unwrap()).is_err());
    }
    fn terminal(output: Value, tokens: Value) -> Value {
        json!({"type":"response.completed","response":{"status":"completed","output":output,"usage":{"input_tokens":2000,"output_tokens":tokens,"input_tokens_details":{"cached_tokens":1920,"cache_write_tokens":0}}}})
    }
    #[test]
    fn zero_output_needs_empty_items_explicit_zero_usage_and_cache_evidence() {
        let mut observation = Observation::default();
        observation.event(&terminal(json!([]), json!(0)), true);
        assert!(observation.confirmed_prewarm());
        for event in [
            terminal(json!([{"type":"reasoning"}]), json!(0)),
            terminal(json!([]), Value::Null),
            terminal(json!([]), json!(1)),
            json!({"type":"response.completed","response":{"output":[],"usage":{"output_tokens":0}}}),
        ] {
            let mut observation = Observation::default();
            observation.event(&event, true);
            assert!(!observation.confirmed_prewarm());
        }
    }
    #[test]
    fn frames_are_bounded_and_abort_immediately_on_generated_output() {
        let mut frames = Frames::new(true);
        assert!(frames
            .feed(b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"x\"}\r\n\r\n")
            .is_err());
        let mut frames = Frames::new(true);
        assert!(frames.feed(&vec![b'x'; MAX_FRAME + 1]).is_err());
        let mut frames = Frames::new(true);
        let event = format!("data: {}\n\n", terminal(json!([]), json!(0)));
        for chunk in event.as_bytes().chunks(3) {
            frames.feed(chunk).unwrap();
        }
        assert!(frames.observation.confirmed_prewarm());
    }
    #[test]
    fn private_auth_routes_and_launch_are_constrained() {
        assert!(token_matches("Bearer secret", "secret"));
        assert!(!token_matches("Bearer oauth", "secret"));
        assert!(route_kind("POST", "http://evil/responses").is_none());
        assert!(route_kind("POST", "/v1/responses?url=https://evil").is_none());
        let args = launch_arguments(
            &Endpoint {
                port: 1234,
                token: "must-not-be-in-argv".into(),
            },
            None,
        )
        .unwrap()
        .join(" ");
        assert!(!args.contains("must-not-be-in-argv"));
        assert!(!args.contains("resume"));
        assert!(args.contains("requires_openai_auth=false"));
        assert!(args.contains("supports_websockets=false"));
        assert!(modern_model("gpt-5.6-sol"));
        assert!(!modern_model("gpt-5.5"));
        assert!(!modern_model("chatgpt-web/gpt-6.1-sol"));
        assert_eq!(windows_quote("a\\\"b\\"), "\"a\\\\\\\"b\\\\\"");
    }

    fn resume_fixture() -> ResumeTarget {
        let root = PathBuf::from(if cfg!(windows) { r"C:\" } else { "/" });
        ResumeTarget {
            session_id: "01a11d1e-5b36-72b2-b20a-4df7db240797".into(),
            transcript: root.join("Native history").join("rollout.jsonl"),
            cwd: root.join("Saved workspace").join("project with spaces"),
            model: Some("gpt-5.6-sol".into()),
            effort: Some("xhigh".into()),
        }
    }

    fn launch_fixture() -> Endpoint {
        Endpoint {
            port: 1234,
            token: "must-not-be-in-argv".into(),
        }
    }

    #[test]
    fn new_launch_argv_remains_provider_overrides_only() {
        assert_eq!(launch_arguments(&launch_fixture(), None).unwrap(), vec![
            "-c",
            "model_provider=\"switcher-cache\"",
            "-c",
            "model_providers.switcher-cache={name=\"Switcher managed cache\",base_url=\"http://127.0.0.1:1234/v1\",wire_api=\"responses\",env_key=\"CODEX_SWITCHER_CACHE_TOKEN\",requires_openai_auth=false,supports_websockets=false,request_max_retries=0,stream_max_retries=0}",
        ]);
    }

    #[test]
    fn resume_argv_preserves_uuid_cwd_model_and_effort_without_a_prompt() {
        let target = resume_fixture();
        let endpoint = launch_fixture();
        let base = launch_arguments(&endpoint, None).unwrap();
        let args = launch_arguments(&endpoint, Some(&target)).unwrap();
        assert_eq!(&args[..base.len()], base);
        assert_eq!(
            &args[base.len()..],
            &[
                "--cd",
                target.cwd.to_str().unwrap(),
                "--model",
                "gpt-5.6-sol",
                "-c",
                "model_reasoning_effort=\"xhigh\"",
                "resume",
                "--include-non-interactive",
                target.session_id.as_str(),
            ]
        );
        assert_eq!(
            args.iter().filter(|arg| *arg == &target.session_id).count(),
            1
        );
        assert!(!args.join(" ").contains(&endpoint.token));
        assert!(!args.join(" ").contains(target.transcript.to_str().unwrap()));
        // Complete argv equality above also excludes prompts, exec, fork,
        // --last, auth commands and history-import arguments.
    }

    #[test]
    fn resume_argv_never_invents_missing_model_or_effort() {
        let mut target = resume_fixture();
        let endpoint = launch_fixture();
        let base = launch_arguments(&endpoint, None).unwrap();
        for (model, effort, saved) in [
            (None, None, Vec::<&str>::new()),
            (Some("gpt-6.1-sol"), None, vec!["--model", "gpt-6.1-sol"]),
            (
                None,
                Some("low"),
                vec!["-c", "model_reasoning_effort=\"low\""],
            ),
        ] {
            target.model = model.map(str::to_owned);
            target.effort = effort.map(str::to_owned);
            let args = launch_arguments(&endpoint, Some(&target)).unwrap();
            let mut expected = vec!["--cd", target.cwd.to_str().unwrap()];
            expected.extend(saved);
            expected.extend(["resume", "--include-non-interactive", &target.session_id]);
            assert_eq!(&args[base.len()..], expected);
        }
        for effort in [
            "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
        ] {
            target.effort = Some(effort.into());
            let args = launch_arguments(&endpoint, Some(&target)).unwrap();
            assert!(args.contains(&format!("model_reasoning_effort=\"{effort}\"")));
        }
    }

    #[test]
    fn resume_argv_rejects_invalid_saved_arguments_without_falling_back() {
        let endpoint = launch_fixture();
        for id in [
            "",
            "--last",
            "session name",
            "01a11d1e5b3672b2b20a4df7db240797",
            "01a11d1e-5b36-72b2-b20a-4df7db240797 prompt",
        ] {
            let mut target = resume_fixture();
            target.session_id = id.into();
            assert!(launch_arguments(&endpoint, Some(&target)).is_err());
        }
        for model in [
            "",
            "--model",
            "gpt-6.1-sol\n",
            "gpt-6.1-sol\0",
            "gpt-6.1-sol\"",
            "gpt-6.1-sol injected",
        ] {
            let mut target = resume_fixture();
            target.model = Some(model.into());
            assert!(launch_arguments(&endpoint, Some(&target)).is_err());
        }
        for effort in ["", "default", "HIGH", "high\"", "high\n", "high injected"] {
            let mut target = resume_fixture();
            target.effort = Some(effort.into());
            assert!(launch_arguments(&endpoint, Some(&target)).is_err());
        }
        let valid_cwd = resume_fixture().cwd.to_string_lossy().into_owned();
        for cwd in [
            "relative workspace".to_owned(),
            format!("{valid_cwd}\0"),
            format!("{valid_cwd}\n"),
        ] {
            let mut target = resume_fixture();
            target.cwd = PathBuf::from(cwd);
            assert!(launch_arguments(&endpoint, Some(&target)).is_err());
        }
        let mut target = resume_fixture();
        target.session_id.make_ascii_uppercase();
        assert_eq!(
            launch_arguments(&endpoint, Some(&target)).unwrap().last(),
            Some(&target.session_id)
        );
    }

    #[test]
    fn winquote_preserves_spaces_unicode_quotes_and_trailing_backslashes() {
        for (argument, expected) in [
            ("", "\"\""),
            (
                r"C:\Program Files\Codex\codex.exe",
                r#""C:\Program Files\Codex\codex.exe""#,
            ),
            (r"C:\Saved workspace\", r#""C:\Saved workspace\\""#),
            (r"C:\저장된 작업\", r#""C:\저장된 작업\\""#),
            ("a\\\"b\\", "\"a\\\\\\\"b\\\\\""),
        ] {
            assert_eq!(windows_quote(argument), expected);
        }
    }

    #[cfg(windows)]
    #[test]
    fn resume_winquote_roundtrips_the_complete_native_command_line() {
        // Decode through Windows itself without starting a CLI or console.
        #[link(name = "shell32")]
        extern "system" {
            fn CommandLineToArgvW(command: *const u16, count: *mut i32) -> *mut *mut u16;
        }
        #[link(name = "kernel32")]
        extern "system" {
            fn LocalFree(memory: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
        }
        let mut target = resume_fixture();
        target.cwd = PathBuf::from(r"C:\저장된 workspace\");
        let mut args = vec![r"C:\Program Files\Codex\codex.exe".to_owned()];
        args.extend(launch_arguments(&launch_fixture(), Some(&target)).unwrap());
        args.extend([String::new(), "a\\\"b\\".into()]); // Decoder edge cases only.
        let command: Vec<u16> = args
            .iter()
            .map(|arg| windows_quote(arg))
            .collect::<Vec<_>>()
            .join(" ")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut count = 0;
        let decoded = unsafe { CommandLineToArgvW(command.as_ptr(), &mut count) };
        assert!(!decoded.is_null());
        let actual: Vec<String> = unsafe { std::slice::from_raw_parts(decoded, count as usize) }
            .iter()
            .map(|&arg| {
                let mut length = 0;
                while unsafe { *arg.add(length) } != 0 {
                    length += 1;
                }
                String::from_utf16(unsafe { std::slice::from_raw_parts(arg, length) }).unwrap()
            })
            .collect();
        unsafe {
            LocalFree(decoded.cast());
        }
        assert_eq!(actual, args);
    }

    #[cfg(windows)]
    #[test]
    fn owned_host_poll_survives_endpoint_failure_and_checks_process_identity() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let endpoint = Endpoint {
            port: listener.local_addr().unwrap().port(),
            token: "fixture".into(),
        };
        drop(listener); // No HTTP status endpoint, no CLI or auth activity.
        let pid = std::process::id();
        let created = NativeProcess::process_stamp(pid).unwrap();
        let mut host = ManagedHost {
            endpoint,
            pid,
            created,
        };
        assert!(managed_host_running(&host).unwrap());
        host.created += 1;
        assert!(!managed_host_running(&host).unwrap());
    }

    #[test]
    fn owned_host_poll_counts_disabled_cache_as_a_live_host() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let endpoint = Endpoint {
            port: listener.local_addr().unwrap().port(),
            token: "fixture".into(),
        };
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                socket.read_exact(&mut byte).unwrap();
                head.push(byte[0]);
            }
            assert!(String::from_utf8(head)
                .unwrap()
                .contains("Authorization: Bearer fixture\r\n"));
            let body = serde_json::to_vec(&View {
                state: "disabled".into(),
                ..View::default()
            })
            .unwrap();
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            socket.write_all(&body).unwrap();
        });
        let host = ManagedHost {
            endpoint,
            pid: 0,
            created: 0,
        };
        assert!(managed_host_running(&host).unwrap());
        server.join().unwrap();
    }
    #[tokio::test]
    async fn offline_socket_requires_local_bearer_and_passes_raw_body() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let client = tokio::spawn(async move {
            let mut s = TcpStream::connect(address).await.unwrap();
            s.write_all(b"POST /v1/responses HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer fixture\r\nContent-Length: 2\r\n\r\n{}").await.unwrap();
        });
        let (mut stream, _) = listener.accept().await.unwrap();
        let head = read_head(&mut stream, "fixture").await.unwrap();
        assert_eq!(head.body, b"{}");
        client.await.unwrap();
    }

    #[tokio::test]
    async fn large_foreground_upload_streams_beyond_capture_budget() {
        let length = MAX_BODY + MAX_FRAME + 17;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let sender = tokio::spawn(async move {
            let mut socket = TcpStream::connect(address).await.unwrap();
            socket.write_all(format!("POST /v1/responses HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer fixture\r\nContent-Length: {length}\r\n\r\n").as_bytes()).await.unwrap();
            let chunk = [b'x'; MAX_FRAME];
            let mut remaining = length;
            while remaining > 0 {
                let n = remaining.min(chunk.len());
                socket.write_all(&chunk[..n]).await.unwrap();
                remaining -= n;
            }
        });
        let (mut source, _) = listener.accept().await.unwrap();
        let head = read_head(&mut source, "fixture").await.unwrap();
        assert_eq!(head.length, length);
        assert!(head.body.len() <= 1024);
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_address = target.local_addr().unwrap();
        let received = tokio::spawn(async move {
            let (mut socket, _) = target.accept().await.unwrap();
            let head = read_head(&mut socket, "fixture").await.unwrap();
            assert_eq!(head.length, length);
            assert!(head.body.iter().all(|b| *b == b'x'));
            let mut count = head.body.len();
            let mut buffer = [0; MAX_FRAME];
            while count < length {
                let n = socket.read(&mut buffer).await.unwrap();
                assert!(n > 0);
                assert!(buffer[..n].iter().all(|b| *b == b'x'));
                count += n;
            }
            assert_eq!(count, length);
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
        });
        let (reader, _writer) = source.into_split();
        let (uploaded, done) = watch::channel(false);
        let remaining = head.length - head.body.len();
        let body = streaming_body(
            head.body,
            remaining,
            Arc::new(tokio::sync::Mutex::new(reader)),
            uploaded,
        );
        let response = Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .post(format!("http://{target_address}/v1/responses"))
            .bearer_auth("fixture")
            .header("content-length", length)
            .body(body)
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        assert!(*done.borrow());
        sender.await.unwrap();
        received.await.unwrap();
    }

    #[tokio::test]
    async fn silent_upstream_is_cancelled_when_downstream_leaves() {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = upstream.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = upstream.accept().await.unwrap();
            let _ = read_head(&mut socket, "fixture").await.unwrap();
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n").await.unwrap();
            std::future::pending::<()>().await;
        });
        let response = Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://{address}/"))
            .bearer_auth("fixture")
            .send()
            .await
            .unwrap();
        let downstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut client = TcpStream::connect(downstream.local_addr().unwrap())
            .await
            .unwrap();
        let (socket, _) = downstream.accept().await.unwrap();
        let (reader, mut writer) = socket.into_split();
        let (_uploaded, done) = watch::channel(true);
        let slots = Arc::new(Semaphore::new(1));
        let slot = slots.clone().acquire_owned().await.unwrap();
        let forwarding = tokio::spawn(async move {
            let _slot = slot;
            tokio::select! {
                _ = disconnected(Arc::new(tokio::sync::Mutex::new(reader)), done) => (),
                _ = relay(&mut writer, response) => panic!("silent upstream unexpectedly completed"),
            }
        });
        let mut header = [0; 512];
        assert!(client.read(&mut header).await.unwrap() > 0);
        drop(client);
        tokio::time::timeout(Duration::from_secs(2), forwarding)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(slots.available_permits(), 1);
        server.abort();
    }

    #[tokio::test]
    async fn relay_preserves_sticky_turn_state_and_observes_cold_usage() {
        let event = format!(
            "data: {}\n\n",
            json!({"type":"response.completed","response":{
                "status":"completed","model":"gpt-6.1-sol","output":[],
                "usage":{"input_tokens":100,"output_tokens":2,"input_tokens_details":{"cached_tokens":0}}
            }})
        );
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = upstream.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = upstream.accept().await.unwrap();
            let _ = read_head(&mut socket, "fixture").await.unwrap();
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nx-codex-turn-state: fixture-sticky\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{event}", event.len()).as_bytes()).await.unwrap();
        });
        let response = Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://{address}/"))
            .bearer_auth("fixture")
            .send()
            .await
            .unwrap();
        let downstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = TcpStream::connect(downstream.local_addr().unwrap())
            .await
            .unwrap();
        let (mut socket, _) = downstream.accept().await.unwrap();
        let receiver = tokio::spawn(async move {
            let mut client = client;
            let mut bytes = Vec::new();
            client.read_to_end(&mut bytes).await.unwrap();
            String::from_utf8(bytes).unwrap()
        });
        let observed = relay(&mut socket, response).await.unwrap();
        assert!(receiver
            .await
            .unwrap()
            .contains("x-codex-turn-state: fixture-sticky\r\n"));
        assert!(observed.completed);
        assert_eq!(observed.model.as_deref(), Some("gpt-6.1-sol"));
        assert_eq!(observed.usage.cached, Some(0));
        assert!(!observed.usage.cached());
        server.await.unwrap();
    }

    #[test]
    fn known_expired_access_is_not_forwarded_and_owner_wait_is_recoverable() {
        use base64::Engine;
        let jwt = |expiry| {
            format!(
                "header.{}.signature",
                base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .encode(format!("{{\"exp\":{expiry}}}"))
            )
        };
        assert!(!usable_access(&jwt(1000), 1000));
        assert!(!usable_access(&jwt(1060), 1000));
        assert!(usable_access(&jwt(1061), 1000));
        assert!(usable_access("opaque-fixture-token", 1000));
        assert!(!usable_access("", 1000));
        let (cancel, _) = watch::channel(0);
        let shared = Arc::new(Shared {
            account: "fixture".into(),
            identity: "identity".into(),
            client: Client::new(),
            cancel,
            forward: Arc::new(Semaphore::new(1)),
            state: Mutex::new(State {
                view: View::default(),
                capture: None,
                busy: 0,
                generation: 0,
                warming: false,
                disabled: false,
                opted_in: true,
                last_user: Instant::now(),
            }),
        });
        authentication_wait(&shared);
        assert_eq!(
            shared.state.lock().unwrap().view.state,
            "authentication_required"
        );
        assert!(!shared.state.lock().unwrap().disabled);
        let foreground = shared.foreground();
        assert_eq!(shared.state.lock().unwrap().view.state, "busy");
        drop(foreground);
        assert_eq!(
            shared.state.lock().unwrap().view.state,
            "waiting_for_request"
        );
    }

    #[test]
    fn restart_metadata_is_bounded_identity_scoped_and_token_free() {
        let mut metadata = Metadata {
            version: 1,
            account: "fixture".into(),
            identity: "identity".into(),
            host_pid: 1,
            host_created: 1,
            heartbeat: 1000,
            view: View {
                state: "active".into(),
                requests: MAX_WARMS,
                ..View::default()
            },
        };
        let valid = serde_json::to_vec(&metadata).unwrap();
        assert!(parse_metadata(&valid, "fixture", "identity", 1001).is_ok());
        assert!(parse_metadata(&valid, "neighbor", "identity", 1001).is_err());
        assert!(parse_metadata(&valid, "fixture", "rotated", 1001).is_err());
        assert!(parse_metadata(&valid, "fixture", "identity", 1016).is_err());
        assert!(parse_metadata(&valid, "fixture", "identity", 999).is_err());
        let mut unexpected: Value = serde_json::from_slice(&valid).unwrap();
        unexpected["token"] = json!("must-never-persist");
        assert!(parse_metadata(
            &serde_json::to_vec(&unexpected).unwrap(),
            "fixture",
            "identity",
            1001
        )
        .is_err());
        metadata.view.requests += 1;
        assert!(parse_metadata(
            &serde_json::to_vec(&metadata).unwrap(),
            "fixture",
            "identity",
            1001
        )
        .is_err());
    }
}
