//! Reopen only desktop apps observed immediately before a force close.
//! Launch targets stay in the backend; the frontend receives a one-use token.

use std::sync::Mutex;
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[derive(Debug, Clone, PartialEq, Eq)]
enum DesktopTarget {
    #[cfg(any(target_os = "macos", test))]
    MacBundle(String),
    #[cfg(any(windows, test))]
    WindowsExecutable(String),
    #[cfg(any(windows, test))]
    WindowsAppId(String),
}

pub(super) struct CapturedDesktop {
    pid: u32,
    target: DesktopTarget,
}

pub(super) fn desktop_pids(desktops: &[CapturedDesktop]) -> Vec<u32> {
    desktops.iter().map(|desktop| desktop.pid).collect()
}

struct ReopenTicket {
    token: String,
    created_at: Instant,
    targets: Vec<DesktopTarget>,
    recovery: Option<Recovery>,
}

pub(super) struct Recovery {
    #[cfg(windows)]
    directory: std::path::PathBuf,
    #[cfg(windows)]
    cancel_on_drop: bool,
    #[cfg(windows)]
    helper: Option<std::os::windows::io::OwnedHandle>,
}

#[cfg(windows)]
#[derive(serde::Deserialize)]
struct RecoveryReceipt {
    pid: u32,
    owner_pid: u32,
    receipt_id: String,
}

impl Recovery {
    pub(super) fn close_started(&mut self) {
        // After the first destructive action, a poisoned mutex, replaced ticket
        // or unwinding stack must not disarm the independent recovery helper.
        #[cfg(windows)]
        {
            self.cancel_on_drop = false;
        }
    }
}

#[cfg(windows)]
impl Drop for Recovery {
    fn drop(&mut self) {
        if self.cancel_on_drop {
            let _ = std::fs::write(self.directory.join("cancel"), b"");
        }
    }
}

// No credentials, account IDs or user command lines enter this recovery plan.
#[cfg(windows)]
fn recovery_target(target: &DesktopTarget) -> serde_json::Value {
    match target {
        DesktopTarget::WindowsExecutable(value) => serde_json::json!({"kind":"exe", "value":value}),
        DesktopTarget::WindowsAppId(value) => serde_json::json!({"kind":"app", "value":value}),
        #[cfg(test)]
        _ => unreachable!("Windows recovery requires Windows desktops"),
    }
}

pub(super) fn prepare_recovery(desktops: &[CapturedDesktop]) -> Result<Option<Recovery>, String> {
    #[cfg(windows)]
    if !desktops.is_empty() {
        let directory = crate::auth::get_config_dir()
            .map_err(|e| e.to_string())?
            .join("desktop-recovery")
            .join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
        let mut recovery = Recovery {
            directory,
            cancel_on_drop: true,
            helper: None,
        };
        let receipt_id = recovery
            .directory
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let plan = serde_json::json!({
            "owner_pid": std::process::id(),
            "receipt_id": receipt_id,
            "desktops": desktops.iter().map(|desktop| serde_json::json!({
                "pid": desktop.pid, "target": recovery_target(&desktop.target),
            })).collect::<Vec<_>>(),
        });
        crate::auth::persistence::atomic_write(
            &recovery.directory.join("plan.json"),
            &serde_json::to_vec(&plan).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        let script = recovery.directory.join("recover.ps1");
        std::fs::write(&script, WINDOWS_RECOVERY_SCRIPT).map_err(|e| e.to_string())?;
        // Ask the actual Explorer desktop to create the recovery process. A
        // locally created Shell.Application alone may launch from this relay.
        let shell = std::env::var_os("ProgramFiles")
            .map(std::path::PathBuf::from)
            .ok_or("ProgramFiles is unavailable; nothing was closed")?
            .join("PowerShell/7/pwsh.exe");
        let arguments = format!(
            "-NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File \"{}\" \"{}\"",
            script.display(),
            recovery.directory.display(),
        );
        let mut command = super::Command::new(&shell);
        command
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Sta",
                "-Command",
                WINDOWS_DESKTOP_BROKER_SCRIPT,
            ])
            .env("CODEX_SWITCHER_RECOVERY_SHELL", &shell)
            .env("CODEX_SWITCHER_RECOVERY_ARGUMENTS", arguments)
            .env("CODEX_SWITCHER_RECOVERY_DIRECTORY", &recovery.directory)
            .creation_flags(super::CREATE_NO_WINDOW)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut child = command.spawn().map_err(|e| {
            format!("Could not start Explorer recovery relay; nothing was closed: {e}")
        })?;
        let started = Instant::now();
        let mut relay_done = false;
        loop {
            if !relay_done {
                if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                    if !status.success() {
                        return Err(
                            "Explorer desktop recovery broker failed; nothing was closed".into(),
                        );
                    }
                    relay_done = true;
                }
            }
            if relay_done {
                if let Ok(bytes) = std::fs::read(recovery.directory.join("ready")) {
                    let receipt: RecoveryReceipt = serde_json::from_slice(&bytes).map_err(|e| {
                        format!("Invalid recovery receipt; nothing was closed: {e}")
                    })?;
                    if receipt.owner_pid != std::process::id()
                        || receipt.receipt_id != receipt_id
                        || receipt.pid == child.id()
                        || receipt.pid == std::process::id()
                    {
                        return Err("Recovery receipt does not identify the owned Explorer helper; nothing was closed".into());
                    }
                    recovery.helper = Some(verify_recovery_helper(receipt.pid, &shell)?);
                    return Ok(Some(recovery));
                }
            }
            if started.elapsed() >= Duration::from_secs(5) {
                return Err("Desktop recovery did not become ready; nothing was closed".into());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    #[cfg(not(windows))]
    let _ = desktops;
    Ok(None)
}

#[cfg(windows)]
const WINDOWS_DESKTOP_BROKER_SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
try {
  $shell = New-Object -ComObject Shell.Application
  $hwnd = 0
  $desktop = $shell.Windows().FindWindowSW(0, 0, 8, [ref]$hwnd, 1)
  if ($null -eq $desktop -or $null -eq $desktop.Document.Application) {
    throw 'Explorer desktop automation is unavailable'
  }
  $desktop.Document.Application.ShellExecute($env:CODEX_SWITCHER_RECOVERY_SHELL,
    $env:CODEX_SWITCHER_RECOVERY_ARGUMENTS, $env:CODEX_SWITCHER_RECOVERY_DIRECTORY, 'open', 0)
} catch { exit 1 }
"#;

#[cfg(windows)]
fn verify_recovery_helper(
    pid: u32,
    executable: &std::path::Path,
) -> Result<std::os::windows::io::OwnedHandle, String> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::{
        Foundation::STILL_ACTIVE,
        System::Threading::{
            GetExitCodeProcess, OpenProcess, QueryFullProcessImageNameW,
            PROCESS_QUERY_LIMITED_INFORMATION,
        },
    };
    unsafe {
        let raw = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if raw.is_null() {
            return Err("Could not open recovery helper; nothing was closed".into());
        }
        let handle = OwnedHandle::from_raw_handle(raw);
        let mut code = 0;
        let mut path = vec![0u16; 32768];
        let mut length = path.len() as u32;
        if GetExitCodeProcess(handle.as_raw_handle(), &mut code) == 0
            || code != STILL_ACTIVE as u32
            || QueryFullProcessImageNameW(handle.as_raw_handle(), 0, path.as_mut_ptr(), &mut length)
                == 0
            || super::normalize_windows_path(&String::from_utf16_lossy(&path[..length as usize]))
                != super::normalize_windows_path(&executable.to_string_lossy())
        {
            return Err("Recovery helper identity or lifetime changed; nothing was closed".into());
        }
        // This is a check of our newly brokered helper, never of Switcher or
        // arbitrary desktop jobs. Keep the handle so PID reuse cannot replace it.
        if super::windows_close::process_is_in_job(handle.as_raw_handle())
            .map_err(|e| format!("Could not inspect recovery helper; nothing was closed: {e}"))?
        {
            return Err("Explorer recovery helper remained in a job; nothing was closed".into());
        }
        Ok(handle)
    }
}

#[cfg(windows)]
const WINDOWS_RECOVERY_SCRIPT: &str = r#"
param([string]$Directory)
$ErrorActionPreference = 'Stop'
function Write-Result([string]$Status, [string]$Message) {
  @{ status = $Status; message = $Message } | ConvertTo-Json -Compress |
    Set-Content -LiteralPath (Join-Path $Directory 'result.json') -Encoding utf8
}
try {
  $plan = Get-Content -LiteralPath (Join-Path $Directory 'plan.json') -Raw | ConvertFrom-Json
  $owner = [Diagnostics.Process]::GetProcessById([int]$plan.owner_pid)
  $null = $owner.Handle # Retain original process identity across PID reuse.
  $desktops = @($plan.desktops | ForEach-Object {
    $process = $null
    try { $process = [Diagnostics.Process]::GetProcessById([int]$_.pid); $null = $process.Handle }
    catch { $process = $null }
    @{ target = $_.target; process = $process }
  })
  $receipt = @{ pid = $PID; owner_pid = $plan.owner_pid; receipt_id = $plan.receipt_id } | ConvertTo-Json -Compress
  [IO.File]::WriteAllText((Join-Path $Directory 'ready.staged'), $receipt)
  Move-Item -LiteralPath (Join-Path $Directory 'ready.staged') -Destination (Join-Path $Directory 'ready')
  $deadline = [DateTime]::UtcNow.AddSeconds(120)
  $request = Join-Path $Directory 'reopen.json'
  while ($true) {
    if (Test-Path -LiteralPath (Join-Path $Directory 'cancel')) { exit 0 }
    if (Test-Path -LiteralPath $request) {
      $targets = @(Get-Content -LiteralPath $request -Raw | ConvertFrom-Json)
      break
    }
    if ($owner.HasExited) { $targets = @($desktops | ForEach-Object { $_.target }); break }
    if ([DateTime]::UtcNow -ge $deadline) { throw 'Desktop recovery request expired' }
    Start-Sleep -Milliseconds 100
  }
  $launched = @{}
  foreach ($target in $targets) {
    $key = $target.kind + ':' + $target.value
    if ($launched.ContainsKey($key)) { continue }
    $matching = @($desktops | Where-Object {
      $_.target.kind -eq $target.kind -and $_.target.value -eq $target.value
    })
    if ($matching.Count -eq 0) { throw 'Recovery target was not captured' }
    # Partial close or an unrelated caller exit must never launch another copy
    # while any of this installation's captured desktop roots remain alive.
    $exitDeadline = [DateTime]::UtcNow.AddSeconds(5)
    while (@($matching | Where-Object { $null -ne $_.process -and -not $_.process.HasExited }).Count -gt 0 -and
           [DateTime]::UtcNow -lt $exitDeadline) { Start-Sleep -Milliseconds 100 }
    if (@($matching | Where-Object { $null -ne $_.process -and -not $_.process.HasExited }).Count -gt 0) { continue }
    if ($target.kind -eq 'app') {
      if ($target.value -notmatch '^OpenAI\.Codex[A-Za-z0-9._-]*_2p2nqsd0c76g0![A-Za-z0-9._-]+$') {
        throw 'Invalid Codex application identity'
      }
      Start-Process -FilePath ('shell:AppsFolder\' + $target.value)
    } elseif ($target.kind -eq 'exe') {
      Start-Process -FilePath $target.value -WorkingDirectory ([IO.Path]::GetDirectoryName($target.value))
    } else { throw 'Unsupported recovery target' }
    $launched[$key] = $true
  }
  Write-Result 'launched' '' # Backend separately confirms the exact desktop.
} catch { Write-Result 'failed' $_.Exception.Message; exit 1 }
"#;

static PENDING_REOPEN: Mutex<Option<ReopenTicket>> = Mutex::new(None);

#[derive(serde::Serialize)]
pub struct CodexReopenInfo {
    supported: bool,
    desktop_count: usize,
}

#[tauri::command]
pub async fn get_codex_reopen_info() -> Result<CodexReopenInfo, String> {
    tokio::task::spawn_blocking(|| {
        let (pids, _) = super::find_codex_processes().map_err(|e| e.to_string())?;
        let desktops = capture_desktops(&pids)?;
        Ok(CodexReopenInfo {
            supported: cfg!(any(target_os = "macos", windows)),
            desktop_count: desktops.len(),
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

pub(super) fn capture_desktops(pids: &[u32]) -> Result<Vec<CapturedDesktop>, String> {
    let mut desktops = Vec::new();

    #[cfg(target_os = "macos")]
    {
        let names = super::read_unix_process_names();
        for &pid in pids {
            let output = super::Command::new("ps")
                .args(["-p", &pid.to_string(), "-o", "command="])
                .output()
                .map_err(|e| e.to_string())?;
            if !output.status.success() {
                continue;
            }
            let command = String::from_utf8_lossy(&output.stdout);
            let name = names.get(&pid).map(String::as_str);
            if let Some(bundle) = mac_bundle_path(command.trim(), name) {
                if is_codex_bundle(&bundle) {
                    desktops.push(CapturedDesktop {
                        pid,
                        target: DesktopTarget::MacBundle(bundle),
                    });
                }
            }
        }
    }

    #[cfg(windows)]
    {
        for process in super::read_windows_codex_processes().map_err(|e| e.to_string())? {
            if !pids.contains(&process.process_id) {
                continue;
            }
            if let Some(path) = windows_desktop_executable(&process) {
                let target = if path.to_ascii_lowercase().contains("\\windowsapps\\") {
                    // Packaged apps require their exact registered identity, not
                    // whichever Codex happens to be first in the Start menu.
                    windows_app_id(&path).map(DesktopTarget::WindowsAppId)
                } else {
                    let executable = std::path::Path::new(&path);
                    let has_desktop_resources = executable.parent().is_some_and(|parent| {
                        parent.join("resources/app.asar").is_file()
                            || parent.join("resources/app").is_dir()
                    });
                    (executable.is_file() && has_desktop_resources)
                        .then_some(DesktopTarget::WindowsExecutable(path))
                };
                if let Some(target) = target {
                    desktops.push(CapturedDesktop {
                        pid: process.process_id,
                        target,
                    });
                }
            }
        }
    }

    #[cfg(not(any(target_os = "macos", windows)))]
    let _ = (pids, &mut desktops);
    Ok(desktops)
}

#[cfg(any(target_os = "macos", test))]
fn mac_bundle_path(command: &str, name: Option<&str>) -> Option<String> {
    let suffix = match name? {
        "ChatGPT" => "/ChatGPT.app/Contents/MacOS/ChatGPT",
        "Codex" => "/Codex.app/Contents/MacOS/Codex",
        _ => return None,
    };
    if !command.starts_with('/') {
        return None;
    }
    let index = command.find(suffix)?;
    if command[index + suffix.len()..]
        .chars()
        .next()
        .is_some_and(|c| !c.is_whitespace())
    {
        return None;
    }
    Some(command[..index + suffix.find("/Contents/")?].to_string())
}

#[cfg(target_os = "macos")]
fn is_codex_bundle(bundle: &str) -> bool {
    plist::Value::from_file(std::path::Path::new(bundle).join("Contents/Info.plist"))
        .ok()
        .and_then(|value| {
            value
                .as_dictionary()?
                .get("CFBundleIdentifier")?
                .as_string()
                .map(str::to_owned)
        })
        .is_some_and(|id| id == "com.openai.codex")
}

#[cfg(any(windows, test))]
fn windows_desktop_executable(process: &super::WindowsCodexProcess) -> Option<String> {
    if !super::is_windows_codex_root_process(process)
        || super::is_ide_plugin_process(&process.command_line.to_ascii_lowercase())
    {
        return None;
    }
    let path = if process.executable_path.trim().is_empty() {
        super::windows_command_executable_path(&process.command_line)?
    } else {
        process.executable_path.trim()
    };
    let path = path.replace('/', "\\");
    let bytes = path.as_bytes();
    // Only an absolute local executable path; never a CLI name or arguments.
    if bytes.len() < 3 || !bytes[0].is_ascii_alphabetic() || &bytes[1..3] != b":\\" {
        return None;
    }
    Some(path)
}

#[cfg(windows)]
fn windows_app_id(executable: &str) -> Option<String> {
    use std::os::windows::process::CommandExt;
    // Resolve the manifest application whose executable matches this process.
    // Passing the path via the environment avoids interpreting it as PowerShell.
    let script = r#"
$ErrorActionPreference = 'Stop'
$exe = $env:CODEX_SWITCHER_REOPEN_EXE
foreach ($pkg in (Get-AppxPackage -Name 'OpenAI.Codex*')) {
  if (-not $pkg.InstallLocation) { continue }
  foreach ($app in (Get-AppxPackageManifest -Package $pkg.PackageFullName).Package.Applications.Application) {
    if (-not $app.Executable -or -not $app.Id) { continue }
    $candidate = Join-Path $pkg.InstallLocation $app.Executable
    if ([string]::Equals($candidate, $exe, [StringComparison]::OrdinalIgnoreCase)) {
      Write-Output ($pkg.PackageFamilyName + '!' + $app.Id)
      exit 0
    }
  }
}
exit 1
"#;
    let output = super::Command::new("powershell.exe")
        .creation_flags(super::CREATE_NO_WINDOW)
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .env("CODEX_SWITCHER_REOPEN_EXE", executable)
        .output()
        .ok()?;
    let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (output.status.success() && valid_windows_app_id(&id)).then_some(id)
}

#[cfg(any(windows, test))]
fn valid_windows_app_id(id: &str) -> bool {
    id.to_ascii_lowercase().starts_with("openai.codex")
        && id.contains("_2p2nqsd0c76g0!")
        && id.split('!').count() == 2
        && !id.ends_with('!')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-!".contains(c))
}

fn closed_targets(desktops: Vec<CapturedDesktop>, killed_pids: &[u32]) -> Vec<DesktopTarget> {
    let mut targets = Vec::new();
    for desktop in desktops {
        if killed_pids.contains(&desktop.pid) && !targets.contains(&desktop.target) {
            targets.push(desktop.target);
        }
    }
    targets
}

pub(super) fn remember_closed_desktops(
    desktops: Vec<CapturedDesktop>,
    killed_pids: &[u32],
    recovery: Option<Recovery>,
) -> Option<String> {
    let targets = closed_targets(desktops, killed_pids);
    let mut pending = PENDING_REOPEN.lock().ok()?;
    *pending = None;
    if targets.is_empty() {
        return None;
    }
    let token = uuid::Uuid::new_v4().to_string();
    *pending = Some(ReopenTicket {
        token: token.clone(),
        created_at: Instant::now(),
        targets,
        recovery,
    });
    Some(token)
}

fn take_targets(pending: &mut Option<ReopenTicket>, token: &str) -> Result<ReopenTicket, String> {
    let valid = pending.as_ref().is_some_and(|ticket| {
        ticket.token == token && ticket.created_at.elapsed() < Duration::from_secs(120)
    });
    if !valid {
        return Err("The desktop reopen request expired or is no longer available".into());
    }
    Ok(pending.take().expect("validated ticket"))
}

#[tauri::command]
pub async fn reopen_closed_codex_desktop(token: String) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        // Keep the ticket available when another live session still blocks it.
        super::ensure_codex_not_running()?;
        let mut ticket = take_targets(
            &mut *PENDING_REOPEN.lock().map_err(|e| e.to_string())?,
            &token,
        )?;
        let expected = ticket.targets.clone();
        #[cfg(windows)]
        if let Some(recovery) = ticket.recovery.as_mut() {
            // The frontend/backend calls this only after the account write (or
            // its failure). Never race a still-running credential update.
            crate::auth::persistence::atomic_write(
                &recovery.directory.join("reopen.json"),
                &serde_json::to_vec(&ticket.targets.iter().map(recovery_target).collect::<Vec<_>>())
                    .map_err(|e| e.to_string())?,
            ).map_err(|e| e.to_string())?;
            recovery.cancel_on_drop = false;
        }
        for target in &ticket.targets {
            if ticket.recovery.is_none() && !launch_desktop(target) {
                return Err("Could not launch Codex desktop. The account change was not undone. Open Codex manually.".into());
            }
        }
        let confirmed = wait_for_desktops(
            &expected,
            Duration::from_secs(20),
            || {
                let (pids, _) = super::find_codex_processes().map_err(|e| e.to_string())?;
                Ok(capture_desktops(&pids)?.into_iter().map(|desktop| desktop.target).collect())
            },
        );
        if !confirmed {
            return Err("Codex desktop did not appear within 20 seconds. The account change was not undone. Check the app or open it manually.".into());
        }
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

fn wait_for_desktops(
    expected: &[DesktopTarget],
    timeout: Duration,
    mut inspect: impl FnMut() -> Result<Vec<DesktopTarget>, String>,
) -> bool {
    let started = Instant::now();
    loop {
        if let Ok(running) = inspect() {
            if expected.iter().all(|target| running.contains(target)) {
                return true;
            }
        }
        if started.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn launch_desktop(target: &DesktopTarget) -> bool {
    match target {
        #[cfg(target_os = "macos")]
        DesktopTarget::MacBundle(bundle) => {
            is_codex_bundle(bundle)
                && super::command_succeeds(super::Command::new("open").arg("-a").arg(bundle))
        }
        #[cfg(windows)]
        DesktopTarget::WindowsExecutable(path) => {
            super::spawn_windows_codex_exe(std::path::Path::new(path))
        }
        #[cfg(windows)]
        DesktopTarget::WindowsAppId(id) => {
            use std::os::windows::process::CommandExt;
            valid_windows_app_id(id) && super::command_succeeds(
                super::Command::new("powershell.exe")
                    .creation_flags(super::CREATE_NO_WINDOW)
                    .args(["-NoProfile", "-NonInteractive", "-Command",
                        "$ErrorActionPreference = 'Stop'; Start-Process ('shell:AppsFolder\\' + $env:CODEX_SWITCHER_REOPEN_APP_ID)"])
                    .env("CODEX_SWITCHER_REOPEN_APP_ID", id),
            )
        }
        #[allow(unreachable_patterns)]
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn dropping_recovery_only_cancels_before_close_started() {
        for close_started in [false, true] {
            let directory = std::env::temp_dir()
                .join(format!("switcher-recovery-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&directory).unwrap();
            let mut recovery = Recovery {
                directory: directory.clone(),
                cancel_on_drop: true,
                helper: None,
            };
            if close_started {
                recovery.close_started();
            }
            drop(recovery);
            assert_eq!(directory.join("cancel").exists(), !close_started);
            std::fs::remove_dir_all(directory).unwrap();
        }
        assert!(prepare_recovery(&[]).unwrap().is_none());
    }

    #[test]
    fn launch_confirmation_requires_the_exact_desktop_and_handles_detection_failure() {
        let expected = vec![DesktopTarget::MacBundle("/Applications/ChatGPT.app".into())];
        assert!(wait_for_desktops(&expected, Duration::ZERO, || Ok(
            expected.clone()
        )));
        assert!(!wait_for_desktops(&expected, Duration::ZERO, || Ok(vec![])));
        assert!(!wait_for_desktops(&expected, Duration::ZERO, || Err(
            "query failed".into()
        )));
        assert!(!wait_for_desktops(&expected, Duration::ZERO, || Ok(vec![
            DesktopTarget::MacBundle("/Other/ChatGPT.app".into())
        ])));
    }

    #[test]
    fn extracts_exact_macos_bundle_with_spaces_and_rejects_helpers() {
        assert_eq!(
            mac_bundle_path(
                "/Users/test/My Apps/ChatGPT.app/Contents/MacOS/ChatGPT --flag",
                Some("ChatGPT")
            ),
            Some("/Users/test/My Apps/ChatGPT.app".into()),
        );
        assert_eq!(
            mac_bundle_path(
                "/Applications/Codex.app/Contents/MacOS/Codex",
                Some("Codex")
            ),
            Some("/Applications/Codex.app".into())
        );
        assert_eq!(
            mac_bundle_path(
                "/Applications/ChatGPT.app/Contents/MacOS/ChatGPTHelper",
                Some("ChatGPT")
            ),
            None
        );
        assert_eq!(
            mac_bundle_path(
                "/Applications/ChatGPT.app/Contents/Resources/codex app-server",
                Some("codex")
            ),
            None
        );
        assert_eq!(mac_bundle_path("codex", Some("codex")), None);
    }

    fn windows_process(path: &str, command: &str) -> super::super::WindowsCodexProcess {
        super::super::WindowsCodexProcess {
            name: "ChatGPT.exe".into(),
            process_id: 100,
            parent_process_id: 1,
            executable_path: path.into(),
            command_line: command.into(),
            main_window_title: "Codex".into(),
        }
    }

    #[test]
    fn windows_package_requires_codex_identity_and_preserves_exact_path() {
        let path =
            r"C:\Program Files\WindowsApps\OpenAI.Codex_1.2.3.0_x64__2p2nqsd0c76g0\app\ChatGPT.exe";
        assert_eq!(
            windows_desktop_executable(&windows_process(path, "")),
            Some(path.into())
        );
        assert_eq!(
            windows_desktop_executable(&windows_process("", &format!("\"{path}\" --flag"))),
            Some(path.into())
        );
        assert_eq!(
            windows_desktop_executable(&windows_process(path, "--type=renderer")),
            None
        );
        assert_eq!(
            windows_desktop_executable(&windows_process(r"C:\Other\ChatGPT.exe", "")),
            None
        );
        assert_eq!(
            windows_desktop_executable(&windows_process(
                &path.replace("2p2nqsd0c76g0", "other"),
                ""
            )),
            None
        );
        let mut legacy = windows_process(r"C:\Users\test\Apps\Codex\Codex.exe", "");
        legacy.name = "Codex.exe".into();
        assert_eq!(
            windows_desktop_executable(&legacy),
            Some(legacy.executable_path.clone())
        );
        legacy.executable_path = r"C:\Apps\Codex\resources\codex.exe".into();
        assert_eq!(windows_desktop_executable(&legacy), None);
        legacy.executable_path = "codex.exe".into();
        assert_eq!(windows_desktop_executable(&legacy), None);
    }

    #[test]
    fn windows_app_id_rejects_other_apps_and_malformed_identifiers() {
        assert!(valid_windows_app_id("OpenAI.Codex_2p2nqsd0c76g0!App"));
        for id in [
            "OpenAI.ChatGPT_2p2nqsd0c76g0!App",
            "OpenAI.Codex_other!App",
            "OpenAI.Codex_2p2nqsd0c76g0!",
            "OpenAI.Codex_2p2nqsd0c76g0!App\n",
            "OpenAI.Codex_2p2nqsd0c76g0!App!Other",
        ] {
            assert!(!valid_windows_app_id(id));
        }
    }

    #[test]
    fn reopens_only_closed_roots_and_deduplicates_same_installation() {
        let target = DesktopTarget::MacBundle("/Applications/ChatGPT.app".into());
        let desktops = vec![
            CapturedDesktop {
                pid: 1,
                target: target.clone(),
            },
            CapturedDesktop {
                pid: 2,
                target: target.clone(),
            },
            CapturedDesktop {
                pid: 3,
                target: DesktopTarget::MacBundle("/Other/Codex.app".into()),
            },
        ];
        assert_eq!(closed_targets(desktops, &[1, 2]), vec![target]);
        assert!(closed_targets(vec![], &[1]).is_empty());
    }

    #[test]
    fn reopen_tickets_are_one_use_and_expire() {
        let mut ticket = Some(ReopenTicket {
            token: "token".into(),
            created_at: Instant::now(),
            targets: vec![DesktopTarget::WindowsAppId(
                "OpenAI.Codex_2p2nqsd0c76g0!App".into(),
            )],
            recovery: None,
        });
        assert!(take_targets(&mut ticket, "wrong").is_err());
        assert_eq!(take_targets(&mut ticket, "token").unwrap().targets.len(), 1);
        assert!(take_targets(&mut ticket, "token").is_err());
        let mut expired = Some(ReopenTicket {
            token: "expired".into(),
            created_at: Instant::now() - Duration::from_secs(121),
            targets: vec![DesktopTarget::WindowsExecutable("C:\\Codex.exe".into())],
            recovery: None,
        });
        assert!(take_targets(&mut expired, "expired").is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "Read-only inspection of live desktop processes; run explicitly"]
    fn inspect_live_desktop_targets() {
        let (pids, _) = super::super::find_codex_processes().unwrap();
        let desktops = capture_desktops(&pids).unwrap();
        for desktop in &desktops {
            println!("Desktop PID {}: {:?}", desktop.pid, desktop.target);
        }
        assert!(!desktops.is_empty(), "No running Codex desktop found");
    }
}
