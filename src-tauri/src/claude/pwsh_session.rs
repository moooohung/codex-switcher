//! Interactive sessions have their own console, never the detached helper's pipes.
//! The pwsh window stays open on CLI errors; the lease follows the CLI, not pwsh.
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
    process::ExitStatus,
};
use tokio::{
    process::Command,
    time::{sleep, Duration, Instant},
};
use windows_sys::Win32::{Foundation::*, System::Threading::*};

const SCRIPT: &str = r#"param([Parameter(Mandatory=$true)][string]$Spec)
$ErrorActionPreference = 'Stop'
$state = Join-Path (Split-Path -LiteralPath $Spec) 'state.json'
function Write-State($value) {
    $tmp = "$state.tmp"
    [IO.File]::WriteAllText($tmp, ($value | ConvertTo-Json -Compress))
    [IO.File]::Move($tmp, $state, $true)
}
try {
    $launch = Get-Content -LiteralPath $Spec -Raw | ConvertFrom-Json
    if ($launch.cwd) { Set-Location -LiteralPath $launch.cwd }
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $launch.exe
    $info.UseShellExecute = $false
    $info.WorkingDirectory = (Get-Location).Path
    # Inherit this real console. Never redirect the interactive CLI to pipes.
    foreach ($arg in $launch.args) { $info.ArgumentList.Add([string]$arg) }
    if (Test-Path -LiteralPath (Join-Path (Split-Path -LiteralPath $Spec) 'cancel')) { throw 'Launch cancelled' }
    $cli = [Diagnostics.Process]::Start($info)
    Write-State @{phase='started';pid=$cli.Id;created=$cli.StartTime.ToUniversalTime().ToFileTimeUtc()}
    while (-not $cli.WaitForExit(100)) {
        if (Test-Path -LiteralPath (Join-Path (Split-Path -LiteralPath $Spec) 'cancel')) {
            $cli.Kill()
            $cli.WaitForExit()
        }
    }
    Write-State @{phase='exited';pid=$cli.Id;exit_code=$cli.ExitCode}
    Write-Host "`nClaude Code exited (code $($cli.ExitCode)). This pwsh window remains open." -ForegroundColor Yellow
} catch {
    Write-State @{phase='failed'}
    Write-Host "Claude Code could not start: $($_.Exception.Message)" -ForegroundColor Red
}
"#;

pub(super) struct Session {
    cli: Process,
    // Retain a handle for fixture verification, not as the history lease owner.
    _shell: Process,
}
unsafe impl Send for Session {}
struct Process(HANDLE);
// A Windows process handle can move between executor threads. Drop only closes
// our handle; it never terminates a user process.
unsafe impl Send for Process {}
struct StartupGuard(Option<PathBuf>);
impl Drop for StartupGuard {
    fn drop(&mut self) {
        let Some(directory) = self.0.take() else {
            return;
        };
        let _ = std::fs::write(directory.join("cancel"), "cancelled");
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if std::fs::read(directory.join("state.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                .is_some_and(|state| matches!(state["phase"].as_str(), Some("exited" | "failed")))
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
impl Process {
    fn identity(&self) -> Result<(u32, u64)> {
        let pid = unsafe { GetProcessId(self.0) };
        let (mut created, mut exited, mut kernel, mut user) = unsafe {
            (
                std::mem::zeroed(),
                std::mem::zeroed(),
                std::mem::zeroed(),
                std::mem::zeroed(),
            )
        };
        ensure!(
            pid != 0
                && unsafe {
                    GetProcessTimes(self.0, &mut created, &mut exited, &mut kernel, &mut user)
                } != 0,
            "Could not identify console process"
        );
        Ok((
            pid,
            (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime),
        ))
    }
    fn try_wait(&self) -> Result<Option<ExitStatus>> {
        use std::os::windows::process::ExitStatusExt;
        let waited = unsafe { WaitForSingleObject(self.0, 0) };
        if waited == WAIT_TIMEOUT {
            return Ok(None);
        }
        ensure!(
            waited == WAIT_OBJECT_0,
            "Could not monitor the owned Claude process"
        );
        let mut code = 0;
        ensure!(
            unsafe { GetExitCodeProcess(self.0, &mut code) } != 0,
            "Could not read Claude exit code"
        );
        Ok(Some(ExitStatus::from_raw(code)))
    }
    fn stop(&self) -> Result<()> {
        if self.try_wait()?.is_none() {
            ensure!(
                unsafe { TerminateProcess(self.0, 1) } != 0,
                "Could not close the owned Claude process"
            );
        }
        Ok(())
    }
    fn attach(pid: u32, expected: u64) -> Result<Self> {
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE,
                0,
                pid,
            )
        };
        ensure!(
            !handle.is_null(),
            "Claude exited before startup could be confirmed; see the pwsh window"
        );
        let process = Self(handle);
        let (mut created, mut exited, mut kernel, mut user) = unsafe {
            (
                std::mem::zeroed(),
                std::mem::zeroed(),
                std::mem::zeroed(),
                std::mem::zeroed(),
            )
        };
        ensure!(
            unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) }
                != 0,
            "Could not identify the owned Claude process"
        );
        let stamp = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
        ensure!(stamp == expected, "Claude startup process identity changed");
        Ok(process)
    }
}

fn pwsh() -> Result<PathBuf> {
    let base = std::env::var_os("ProgramFiles")
        .context("PowerShell 7 installation directory unavailable")?;
    let installed = PathBuf::from(base).join("PowerShell/7/pwsh.exe");
    ensure!(
        installed.is_file(),
        "PowerShell 7 (pwsh) is required to open Claude sessions"
    );
    Ok(installed)
}

impl Session {
    pub(super) async fn launch(cmd: &Command, profile: &Path) -> Result<Self> {
        Self::launch_with(cmd, profile, &pwsh()?).await
    }
    async fn launch_with(cmd: &Command, profile: &Path, shell: &Path) -> Result<Self> {
        let command = cmd.as_std();
        let directory = profile
            .join(".switcher-console-launches")
            .join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&directory)?;
        let script = directory.join("launch.ps1");
        let spec = directory.join("launch.json");
        let receipt = directory.join("state.json");
        let exe = PathBuf::from(command.get_program());
        ensure!(
            exe.is_absolute() && exe.is_file(),
            "Claude executable must be an installed absolute path"
        );
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let cwd = command
            .get_current_dir()
            .map(Path::to_path_buf)
            .unwrap_or(std::env::current_dir()?);
        std::fs::write(&script, SCRIPT)?;
        // Contains paths/argv only. Authentication remains in the profile, never here.
        std::fs::write(
            &spec,
            serde_json::to_vec(&json!({"exe":exe,"args":args,"cwd":cwd}))?,
        )?;
        let mut startup_guard = StartupGuard(Some(directory.clone()));
        let shell = spawn_console(
            shell,
            &[
                "-NoLogo".into(),
                "-NoProfile".into(),
                "-NoExit".into(),
                "-ExecutionPolicy".into(),
                "Bypass".into(),
                "-File".into(),
                script.into_os_string(),
                "-Spec".into(),
                spec.into_os_string(),
            ],
            command,
            &cwd,
        )?;
        let (pid, created) = shell.identity()?;
        std::fs::write(
            directory.join("console.json"),
            serde_json::to_vec(&json!({"pid":pid,"created":created}))?,
        )?;
        let deadline = Instant::now() + Duration::from_secs(10);
        let cli = loop {
            if let Ok(bytes) = std::fs::read(&receipt) {
                if let Ok(state) = serde_json::from_slice::<Value>(&bytes) {
                    match state["phase"].as_str() {
                        Some("started") => {
                            let pid = state["pid"]
                                .as_u64()
                                .and_then(|pid| u32::try_from(pid).ok())
                                .context("Invalid Claude startup PID")?;
                            let stamp = state["created"]
                                .as_u64()
                                .context("Invalid Claude startup identity")?;
                            break Process::attach(pid, stamp)?;
                        }
                        Some("exited" | "failed") => anyhow::bail!(
                            "Claude exited during startup; the pwsh window contains the error"
                        ),
                        _ => {}
                    }
                }
            }
            ensure!(
                shell.try_wait()?.is_none(),
                "The pwsh window closed before Claude started"
            );
            ensure!(
                Instant::now() < deadline,
                "Claude startup was not confirmed; inspect the pwsh window before retrying"
            );
            sleep(Duration::from_millis(100)).await;
        };
        sleep(Duration::from_millis(1200)).await;
        ensure!(
            cli.try_wait()?.is_none(),
            "Claude exited during startup; the pwsh window contains the error"
        );
        startup_guard.0.take();
        Ok(Self { cli, _shell: shell })
    }
    pub(super) fn try_wait(&self) -> Result<Option<ExitStatus>> {
        self.cli.try_wait()
    }
    pub(super) fn identity(&self) -> Result<(u32, u64)> {
        self.cli.identity()
    }
    pub(super) fn stop(&self) -> Result<()> {
        self.cli.stop()
    }
}

fn quote(value: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    let mut result = vec![34];
    let mut slashes = 0;
    for ch in value.encode_wide() {
        if ch == 92 {
            slashes += 1;
            continue;
        }
        result.extend(std::iter::repeat_n(
            92,
            if ch == 34 { slashes * 2 + 1 } else { slashes },
        ));
        slashes = 0;
        result.push(ch);
    }
    result.extend(std::iter::repeat_n(92, slashes * 2));
    result.push(34);
    result
}
fn spawn_console(
    path: &Path,
    args: &[OsString],
    configured: &std::process::Command,
    cwd: &Path,
) -> Result<Process> {
    use std::os::windows::ffi::OsStrExt;
    let application = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let mut argv = quote(path.as_os_str());
    for arg in args {
        argv.push(32);
        argv.extend(quote(arg));
    }
    argv.push(0);
    let mut environment = std::env::vars_os()
        .map(|(key, value)| (key.to_string_lossy().to_ascii_uppercase(), (key, value)))
        .collect::<BTreeMap<_, _>>();
    for (key, value) in configured.get_envs() {
        let index = key.to_string_lossy().to_ascii_uppercase();
        if let Some(value) = value {
            environment.insert(index, (key.to_os_string(), value.to_os_string()));
        } else {
            environment.remove(&index);
        }
    }
    let mut env = Vec::new();
    for (_, (key, value)) in environment {
        env.extend(key.encode_wide());
        env.push(61);
        env.extend(value.encode_wide());
        env.push(0);
    }
    env.push(0);
    let cwd = cwd
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
    startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    // No STARTF_USESTDHANDLES and no handle inheritance: the hidden helper's
    // stdin=NUL/stdout=ACK pipe cannot become the interactive session's TTY.
    let mut information: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let result = unsafe {
        CreateProcessW(
            application.as_ptr(),
            argv.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            CREATE_NEW_CONSOLE | CREATE_UNICODE_ENVIRONMENT,
            env.as_ptr().cast(),
            cwd.as_ptr(),
            &startup,
            &mut information,
        )
    };
    if result == 0 {
        return Err(std::io::Error::last_os_error()).context("Could not open the pwsh console");
    }
    unsafe {
        CloseHandle(information.hThread);
    }
    Ok(Process(information.hProcess))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "Owned subprocess fixture, invoked by pwsh_launch_has_console_and_preserves_argv"]
    fn cli_fixture() {
        use windows_sys::Win32::System::Console::*;
        let mut modes = [0; 3];
        let valid = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE]
            .into_iter()
            .zip(modes.iter_mut())
            .all(|(kind, mode)| unsafe { GetConsoleMode(GetStdHandle(kind), mode) != 0 });
        let output = std::env::var_os("SWITCHER_CONSOLE_FIXTURE").unwrap();
        std::fs::write(output, serde_json::to_vec(&json!({"console":valid,"cwd":std::env::current_dir().unwrap(),"args":std::env::args().collect::<Vec<_>>(),"marker":std::env::var("SWITCHER_CONSOLE_MARKER").ok()})).unwrap()).unwrap();
        // Stay alive until this test's parent removes its private marker.
        let stop = std::env::var_os("SWITCHER_CONSOLE_STOP").unwrap();
        while Path::new(&stop).exists() {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    #[tokio::test]
    async fn pwsh_launch_has_console_and_preserves_argv() {
        let root =
            std::env::temp_dir().join(format!("switcher pwsh 한글 '$()' {}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let output = root.join("fixture.json");
        let stop = root.join("running");
        std::fs::write(&stop, "owned").unwrap();
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args([
            "--exact",
            "claude::pwsh_session::tests::cli_fixture",
            "--ignored",
            "--nocapture",
        ])
        .current_dir(&root)
        .env("SWITCHER_CONSOLE_FIXTURE", &output)
        .env("SWITCHER_CONSOLE_STOP", &stop)
        .env("SWITCHER_CONSOLE_MARKER", "spaces 한글 ' \" $()")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped());
        let session = Session::launch(&cmd, &root).await.unwrap();
        let observed: Value = serde_json::from_slice(&std::fs::read(&output).unwrap()).unwrap();
        assert_eq!(observed["console"], true);
        // Hosted Windows runners may report TEMP using an 8.3 alias while
        // pwsh resolves the long name. Compare directory identity, not spelling.
        assert_eq!(
            Path::new(observed["cwd"].as_str().unwrap())
                .canonicalize()
                .unwrap(),
            root.canonicalize().unwrap()
        );
        assert_eq!(observed["marker"], "spaces 한글 ' \" $()");
        assert_eq!(observed["args"][1], "--exact");
        assert_eq!(
            observed["args"][2],
            "claude::pwsh_session::tests::cli_fixture"
        );
        assert!(session.try_wait().unwrap().is_none());
        std::fs::remove_file(stop).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while session.try_wait().unwrap().is_none() && Instant::now() < deadline {
            sleep(Duration::from_millis(50)).await;
        }
        assert!(session.try_wait().unwrap().unwrap().success());
        assert!(
            session._shell.try_wait().unwrap().is_none(),
            "pwsh should remain for diagnosis"
        );
        session._shell.stop().unwrap();
        // Only this UUID-owned temp fixture is removed, after its processes exit.
        std::fs::remove_dir_all(&root).unwrap();
    }
    #[tokio::test]
    async fn early_cli_exit_is_failure_not_successful_launch() {
        let root =
            std::env::temp_dir().join(format!("switcher-pwsh-failure-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let mut cmd = Command::new(pwsh().unwrap());
        cmd.args(["-NoProfile", "-Command", "exit 17"]);
        let error = Session::launch(&cmd, &root)
            .await
            .err()
            .expect("early exit must reject startup");
        assert!(error.to_string().contains("startup"));
        // The diagnostic shell remains open on failure. Close only the fixture
        // process whose exact PID + creation stamp is in this private receipt.
        let launch_dir = std::fs::read_dir(root.join(".switcher-console-launches"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let receipt: Value =
            serde_json::from_slice(&std::fs::read(launch_dir.join("console.json")).unwrap())
                .unwrap();
        let shell = Process::attach(
            receipt["pid"].as_u64().unwrap() as u32,
            receipt["created"].as_u64().unwrap(),
        )
        .unwrap();
        assert!(shell.try_wait().unwrap().is_none());
        shell.stop().unwrap();
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn cancelled_startup_closes_only_owned_cli_and_retains_diagnostics() {
        let root =
            std::env::temp_dir().join(format!("switcher-pwsh-cancel-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let output = root.join("fixture.json");
        let stop = root.join("running");
        std::fs::write(&stop, "owned").unwrap();
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args([
            "--exact",
            "claude::pwsh_session::tests::cli_fixture",
            "--ignored",
            "--nocapture",
        ])
        .env("SWITCHER_CONSOLE_FIXTURE", &output)
        .env("SWITCHER_CONSOLE_STOP", &stop);
        let profile = root.clone();
        let task = tokio::spawn(async move { Session::launch(&cmd, &profile).await });
        let deadline = Instant::now() + Duration::from_secs(10);
        while !output.exists() && Instant::now() < deadline {
            sleep(Duration::from_millis(20)).await;
        }
        assert!(
            output.exists(),
            "owned fixture must start before cancellation"
        );
        task.abort();
        assert!(matches!(task.await, Err(error) if error.is_cancelled()));
        let directory = std::fs::read_dir(root.join(".switcher-console-launches"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let state: Value =
            serde_json::from_slice(&std::fs::read(directory.join("state.json")).unwrap()).unwrap();
        assert_eq!(
            state["phase"], "exited",
            "cancellation must end the CLI before releasing the lease"
        );
        let receipt: Value =
            serde_json::from_slice(&std::fs::read(directory.join("console.json")).unwrap())
                .unwrap();
        let shell = Process::attach(
            receipt["pid"].as_u64().unwrap() as u32,
            receipt["created"].as_u64().unwrap(),
        )
        .unwrap();
        assert!(shell.try_wait().unwrap().is_none());
        shell.stop().unwrap();
        std::fs::remove_dir_all(&root).unwrap();
    }
}
