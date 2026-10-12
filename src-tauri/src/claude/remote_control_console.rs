//! Compatibility transport for an existing Windows console, in an isolated
//! helper only. No foreground activation, clipboard, mouse, process termination,
//! input flushing, draft-clearing key or consent response is used.
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, thread, time::Duration};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, FILETIME, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    },
    Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING},
    System::{Console::*, Threading::*},
};

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
struct Attached;
impl Drop for Attached {
    fn drop(&mut self) {
        unsafe {
            FreeConsole();
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Process {
    pub pid: u32,
    pub created: u64,
    pub image: PathBuf,
}
pub(super) fn process(pid: u32) -> Result<Process> {
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            pid,
        )
    };
    ensure!(!handle.is_null(), "Claude process is inaccessible");
    let handle = Handle(handle);
    ensure!(
        unsafe { WaitForSingleObject(handle.0, 0) } == 258,
        "Claude process ended"
    );
    let mut created = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    ensure!(
        unsafe { GetProcessTimes(handle.0, &mut created, &mut exit, &mut kernel, &mut user) } != 0,
        "Claude creation time unavailable"
    );
    let mut image = vec![0_u16; 32768];
    let mut length = image.len() as u32;
    ensure!(
        unsafe { QueryFullProcessImageNameW(handle.0, 0, image.as_mut_ptr(), &mut length) } != 0,
        "Claude executable path unavailable"
    );
    Ok(Process {
        pid,
        created: (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime),
        image: String::from_utf16(&image[..length as usize])?.into(),
    })
}

// Registry publication may be truncated before a creation time can be read.
// Only OS-confirmed absence/exit allows its stale record to be ignored. An
// inaccessible process or a live recycled PID remains inconclusive here.
pub(super) fn process_absent(pid: u32) -> bool {
    if pid <= 1 {
        return false;
    }
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            pid,
        )
    };
    if handle.is_null() {
        return std::io::Error::last_os_error().raw_os_error() == Some(87);
    }
    let handle = Handle(handle);
    unsafe { WaitForSingleObject(handle.0, 0) == 0 }
}

pub(super) fn process_generation_ended(pid: u32, expected: u64) -> bool {
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            pid,
        )
    };
    if handle.is_null() {
        // Invalid PID proves absence. Access denied and other failures do not.
        return std::io::Error::last_os_error().raw_os_error() == Some(87);
    }
    let handle = Handle(handle);
    match unsafe { WaitForSingleObject(handle.0, 0) } {
        0 => true,
        258 => {
            let mut created = FILETIME::default();
            let mut exit = FILETIME::default();
            let mut kernel = FILETIME::default();
            let mut user = FILETIME::default();
            (unsafe { GetProcessTimes(handle.0, &mut created, &mut exit, &mut kernel, &mut user) })
                != 0
                && ((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
                    != expected
        }
        _ => false,
    }
}

fn console(name: &str) -> Result<Handle> {
    let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    ensure!(handle != INVALID_HANDLE_VALUE, "Console access unavailable");
    Ok(Handle(handle))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Screen {
    pub lines: Vec<String>,
    pub cursor_x: i16,
    pub cursor_row: usize,
    pub pending: u32,
}
impl Screen {
    pub(super) fn connected(&self) -> bool {
        // Status indicators only; prose/history cannot count as connection proof.
        self.lines
            .iter()
            .skip(self.cursor_row.saturating_sub(1))
            .take(8)
            .any(|line| {
                line.contains("/rc active")
                    || line.trim() == "/rc"
                    || line.contains("? for shortcuts")
                        && line.split_whitespace().any(|part| part == "/rc")
            })
    }
    pub(super) fn owner_changed(&self) -> bool {
        let text = self.lines.join(" ").to_lowercase();
        (text.contains("account") || text.contains("organization"))
            && text.contains("changed")
            && text.contains("remote control")
            && (text.contains("/remote-control") || text.contains("/rc"))
    }
    pub(super) fn empty_prompt(&self) -> bool {
        if self.pending != 0 || self.cursor_row == 0 {
            return false;
        }
        let Some(line) = self.lines.get(self.cursor_row) else {
            return false;
        };
        // Exact native TUI prompt and cursor, not a shell prompt or history quote.
        let prefix = line.trim_start();
        let Some(rest) = prefix.strip_prefix('❯') else {
            return false;
        };
        if !rest.trim().is_empty() || self.cursor_x < 1 || self.cursor_x > 4 {
            return false;
        }
        let rule = |line: &str| line.chars().filter(|c| matches!(c, '─' | '━')).count() >= 12;
        if !rule(&self.lines[self.cursor_row - 1])
            || !self.lines.get(self.cursor_row + 1).is_some_and(|l| rule(l))
        {
            return false;
        }
        let footer = self
            .lines
            .iter()
            .skip(self.cursor_row + 2)
            .take(8)
            .cloned()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        footer.contains("? for shortcuts")
            && ![
                "esc to interrupt",
                "esc to cancel",
                "enter to confirm",
                "enter to select",
                "allow",
                "permission",
                "yes/no",
                "(y/n)",
                "do you want",
                "trust this",
            ]
            .iter()
            .any(|s| footer.contains(s))
    }
}

fn screen(input: &Handle, output: &Handle) -> Result<Screen> {
    let mut info = CONSOLE_SCREEN_BUFFER_INFO::default();
    ensure!(
        unsafe { GetConsoleScreenBufferInfo(output.0, &mut info) } != 0,
        "Console screen unavailable"
    );
    let width = info.dwSize.X;
    ensure!((80..=512).contains(&width), "Unsupported console width");
    let start = info
        .srWindow
        .Top
        .max(info.dwCursorPosition.Y.saturating_sub(120));
    let end = info
        .srWindow
        .Bottom
        .min(info.dwCursorPosition.Y.saturating_add(8));
    ensure!(
        start <= info.dwCursorPosition.Y && info.dwCursorPosition.Y <= end,
        "Cursor is outside the viewport"
    );
    let mut lines = Vec::new();
    for row in start..=end {
        // Character counts are not cell counts for CJK/wide glyphs. Read the
        // cell grid instead; ReadConsoleOutputCharacterW can return fewer UTF-16
        // characters than the buffer width on a perfectly valid Korean row.
        let mut cells = vec![CHAR_INFO::default(); width as usize];
        let mut region = SMALL_RECT {
            Left: 0,
            Top: row,
            Right: width - 1,
            Bottom: row,
        };
        ensure!(
            unsafe {
                ReadConsoleOutputW(
                    output.0,
                    cells.as_mut_ptr(),
                    COORD { X: width, Y: 1 },
                    COORD { X: 0, Y: 0 },
                    &mut region,
                )
            } != 0
                && region.Left == 0
                && region.Right == width - 1
                && region.Top == row
                && region.Bottom == row,
            "Console read incomplete"
        );
        let chars: Vec<_> = cells
            .iter()
            .filter(|c| c.Attributes & COMMON_LVB_TRAILING_BYTE as u16 == 0)
            .map(|c| {
                let v = unsafe { c.Char.UnicodeChar };
                if v == 0 {
                    32
                } else {
                    v
                }
            })
            .collect();
        lines.push(String::from_utf16_lossy(&chars).trim_end().into());
    }
    let mut pending = 0;
    ensure!(
        unsafe { GetNumberOfConsoleInputEvents(input.0, &mut pending) } != 0,
        "Console input state unavailable"
    );
    Ok(Screen {
        lines,
        cursor_x: info.dwCursorPosition.X,
        cursor_row: (info.dwCursorPosition.Y - start) as usize,
        pending,
    })
}

fn exclusive(expected: &Process) -> Result<()> {
    ensure!(
        process(expected.pid)? == *expected,
        "Claude PID was replaced"
    );
    let mut pids = [0_u32; 64];
    let count = unsafe { GetConsoleProcessList(pids.as_mut_ptr(), pids.len() as u32) } as usize;
    ensure!(
        count > 0 && count <= pids.len() && pids[..count].contains(&expected.pid),
        "Console membership changed"
    );
    for &pid in &pids[..count] {
        if pid == expected.pid || pid == std::process::id() {
            continue;
        }
        let p = process(pid)?;
        let name = p
            .image
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_lowercase();
        ensure!(
            [
                "pwsh.exe",
                "powershell.exe",
                "cmd.exe",
                "conhost.exe",
                "openconsole.exe"
            ]
            .contains(&name.as_str()),
            "Another console reader may consume the command"
        );
    }
    Ok(())
}

/// Read-only local diagnostics; never invokes the command delivery function.
pub(super) fn inspect(pid: u32) -> Result<serde_json::Value> {
    let expected = process(pid)?;
    let name = expected
        .image
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    ensure!(
        name.eq_ignore_ascii_case("claude.exe")
            || name.split('.').count() == 3
                && name
                    .split('.')
                    .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())),
        "Not a native Claude CLI"
    );
    unsafe {
        FreeConsole();
    }
    ensure!(
        unsafe { AttachConsole(pid) } != 0,
        "Claude console cannot be attached"
    );
    let _attached = Attached;
    let input = console("CONIN$")?;
    let output = console("CONOUT$")?;
    let exclusive_console = exclusive(&expected).is_ok();
    let s = screen(&input, &output)?;
    let line = s.lines.get(s.cursor_row).map(String::as_str).unwrap_or("");
    let native = line.trim_start().starts_with('❯');
    let rest = line.trim_start().strip_prefix('❯').unwrap_or("");
    Ok(
        serde_json::json!({"pid":pid,"created":expected.created,"connected_indicator":s.connected(),
        "owner_changed_notice":s.owner_changed(),"empty_prompt":s.empty_prompt(),"cursor_x":s.cursor_x,
        "cursor_on_native_prompt":native,"composer_chars":rest.trim().chars().count(),"pending_events":s.pending,
        "native_prompt_rows":s.lines.iter().filter(|l|l.trim_start().starts_with('❯')).count(),
        "idle_hint_rows":s.lines.iter().filter(|l|l.contains("? for shortcuts")).count(),
        "rows_read":s.lines.len(),
        "idle_hint":s.lines.iter().skip(s.cursor_row+2).take(8).any(|l|l.contains("? for shortcuts")),
            "single_cli_console":exclusive_console,"input_sent":false}),
    )
}

/// The caller revalidates identity/ledger under locks in `before_send` and marks
/// the attempt BEFORE the single write. An ambiguous/partial write is never replayed.
pub(super) fn visit(
    expected: &Process,
    allow_send: bool,
    previously_connected: bool,
    disconnect_seen: bool,
    mut before_send: impl FnMut() -> Result<()>,
) -> Result<&'static str> {
    ensure!(
        process(expected.pid)? == *expected,
        "Claude process changed"
    );
    // Only the short-lived helper calls this. Never detach the GUI/web host.
    unsafe {
        FreeConsole();
    }
    ensure!(
        unsafe { AttachConsole(expected.pid) } != 0,
        "Claude console cannot be attached"
    );
    let _attached = Attached;
    let input = console("CONIN$")?;
    let output = console("CONOUT$")?;
    exclusive(expected)?;
    let first = screen(&input, &output)?;
    if first.connected() {
        return Ok("connected");
    }
    if !allow_send {
        if previously_connected && first.owner_changed() {
            return Ok("observed_owner_changed");
        }
        return Ok(if first.empty_prompt() {
            "observed_disconnected"
        } else {
            "observed"
        });
    }
    // Never interpret a hidden/missing indicator as proof of disconnection.
    // Even a prior connection requires the account-change notification.
    // Conversation text can quote a disconnect notice. It is never sufficient
    // without a recorded active connection under the PREVIOUS account.
    if !previously_connected {
        return Ok("waiting_for_previous_connection_proof");
    }
    if !disconnect_seen && !first.owner_changed() {
        return Ok("waiting_for_disconnect_evidence");
    }
    if !first.empty_prompt() {
        return Ok("waiting_for_empty_prompt");
    }
    thread::sleep(Duration::from_millis(650));
    let second = screen(&input, &output)?;
    if first != second || !second.empty_prompt() {
        return Ok("waiting_for_stable_prompt");
    }
    exclusive(expected)?;
    before_send()?;
    // Check again after disk/identity work; no input buffer is ever cleared.
    ensure!(
        second == screen(&input, &output)?,
        "Console changed before command delivery"
    );
    let records = key_records("/rc\r");
    let mut written = 0;
    ensure!(
        unsafe {
            WriteConsoleInputW(
                input.0,
                records.as_ptr(),
                records.len() as u32,
                &mut written,
            )
        } != 0
            && written == records.len() as u32,
        "Command delivery is uncertain; not replayed"
    );
    Ok("requested")
}

fn key_records(text: &str) -> Vec<INPUT_RECORD> {
    text.encode_utf16()
        .flat_map(|character| {
            [1, 0].map(|down| INPUT_RECORD {
                EventType: KEY_EVENT as u16,
                Event: INPUT_RECORD_0 {
                    KeyEvent: KEY_EVENT_RECORD {
                        bKeyDown: down,
                        wRepeatCount: 1,
                        wVirtualKeyCode: if character == 13 { 13 } else { 0 },
                        wVirtualScanCode: 0,
                        uChar: KEY_EVENT_RECORD_0 {
                            UnicodeChar: character,
                        },
                        dwControlKeyState: 0,
                    },
                },
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn empty() -> Screen {
        Screen {
            lines: vec![
                "Remote Control stopped: account changed. Run /rc".into(),
                "───────────────────────────────────────".into(),
                "❯ ".into(),
                "───────────────────────────────────────".into(),
                "? for shortcuts".into(),
            ],
            cursor_row: 2,
            cursor_x: 2,
            pending: 0,
        }
    }
    #[test]
    fn prompt_requires_empty_native_composer_and_idle_footer() {
        let s = empty();
        assert!(s.empty_prompt());
        assert!(s.owner_changed());
        for draft in [
            "❯ unfinished draft",
            "PS C:\\>",
            "❯ /login",
            "❯ [pasted text]",
        ] {
            let mut s = empty();
            s.lines[2] = draft.into();
            assert!(!s.empty_prompt());
        }
        for footer in [
            "esc to interrupt",
            "? for shortcuts · esc to cancel",
            "? for shortcuts · Allow once",
            "? for shortcuts · Do you want to connect?",
        ] {
            let mut s = empty();
            s.lines[4] = footer.into();
            assert!(!s.empty_prompt());
        }
        let mut s = empty();
        s.pending = 1;
        assert!(!s.empty_prompt());
        s = empty();
        s.cursor_x = 50;
        assert!(!s.empty_prompt());
        s = empty();
        s.lines[3] = "continuation draft".into();
        assert!(!s.empty_prompt());
    }
    #[test]
    fn ordinary_history_and_unrelated_errors_are_not_connection_proof() {
        let mut s = empty();
        s.lines[0] = "assistant said /rc active".into();
        assert!(!s.connected());
        s.lines[4] = "? for shortcuts · /rc active".into();
        assert!(s.connected());
        s.lines[0] = "Remote Control network failure. Try again.".into();
        assert!(!s.owner_changed());
    }
    #[test]
    fn only_official_slash_command_is_written_without_draft_clearing_keys() {
        let records = key_records("/rc\r");
        assert_eq!(records.len(), 8);
        let text: Vec<_> = records
            .iter()
            .step_by(2)
            .map(|r| unsafe { r.Event.KeyEvent.uChar.UnicodeChar })
            .collect();
        assert_eq!(String::from_utf16(&text).unwrap(), "/rc\r");
        assert!(records
            .iter()
            .all(|r| unsafe { r.Event.KeyEvent.dwControlKeyState == 0 }));
    }

    // Real Win32 input/output transport in PRIVATE invisible consoles. These
    // hosts never launch Claude, authenticate, call a model or touch a user pane.
    const HOST: &str = "claude::remote_control_console::tests::private_console_host";
    const AGENT: &str = "claude::remote_control_console::tests::private_console_agent";
    #[test]
    #[ignore = "internal fixture host, launched only by private_console_transport"]
    fn private_console_host() {
        use std::{
            fs,
            path::Path,
            time::{Duration, Instant},
        };
        let Ok(root) = std::env::var("SWITCHER_RC_FIXTURE") else {
            return;
        };
        let root = Path::new(&root);
        let scenario = fs::read_to_string(root.join("scenario")).unwrap();
        let output = console("CONOUT$").unwrap();
        let input = console("CONIN$").unwrap();
        let lines = [
            if scenario == "korean_empty" || scenario == "korean_draft" {
                "한국어 대화 · Remote Control stopped: account changed. Run /remote-control"
            } else if scenario == "scrolled_notice" {
                "Later tool output; disconnect notice scrolled away"
            } else {
                "Remote Control stopped: signed-in account changed. Run /remote-control"
            },
            "────────────────────────────────────────────────────────────────────────────────",
            if scenario == "korean_draft" {
                "❯ 작성 중인 한글 초안"
            } else if scenario == "draft" {
                "❯ unfinished user draft"
            } else {
                "❯ "
            },
            "────────────────────────────────────────────────────────────────────────────────",
            match scenario.as_str() {
                "busy" => "? for shortcuts · esc to interrupt",
                "approval" => "Do you want to connect? (y/n)",
                "connected" => "? for shortcuts · /rc active",
                _ => "? for shortcuts",
            },
        ];
        for (row, text) in lines.iter().enumerate() {
            let chars: Vec<_> = text.encode_utf16().collect();
            let mut written = 0;
            assert_ne!(
                unsafe {
                    WriteConsoleOutputCharacterW(
                        output.0,
                        chars.as_ptr(),
                        chars.len() as u32,
                        COORD {
                            X: 0,
                            Y: row as i16,
                        },
                        &mut written,
                    )
                },
                0
            );
        }
        if scenario == "korean_empty" || scenario == "korean_draft" {
            // Exercise actual wide-cell attributes, rather than merely UTF-16
            // text in one cell. The trailing cell must not duplicate the glyph.
            let mut cells = [CHAR_INFO::default(); 4];
            for (index, character) in ['한', '한', '글', '글'].into_iter().enumerate() {
                cells[index].Char.UnicodeChar = character as u16;
                cells[index].Attributes = 7 | if index % 2 == 0 {
                    COMMON_LVB_LEADING_BYTE as u16
                } else {
                    COMMON_LVB_TRAILING_BYTE as u16
                };
            }
            let mut region = SMALL_RECT {
                Left: 0,
                Top: 5,
                Right: 3,
                Bottom: 5,
            };
            assert_ne!(
                unsafe {
                    WriteConsoleOutputW(
                        output.0,
                        cells.as_ptr(),
                        COORD { X: 4, Y: 1 },
                        COORD { X: 0, Y: 0 },
                        &mut region,
                    )
                },
                0
            );
        }
        assert_ne!(
            unsafe { SetConsoleCursorPosition(output.0, COORD { X: 2, Y: 2 }) },
            0
        );
        fs::write(root.join("ready"), "ready").unwrap();
        let mut received = vec![];
        let until = Instant::now() + Duration::from_secs(4);
        while Instant::now() < until {
            let mut pending = 0;
            assert_ne!(
                unsafe { GetNumberOfConsoleInputEvents(input.0, &mut pending) },
                0
            );
            if pending > 0 {
                let mut records = [INPUT_RECORD::default(); 32];
                let mut count = 0;
                assert_ne!(
                    unsafe { ReadConsoleInputW(input.0, records.as_mut_ptr(), 32, &mut count) },
                    0
                );
                for r in &records[..count as usize] {
                    if r.EventType == KEY_EVENT as u16 && unsafe { r.Event.KeyEvent.bKeyDown } != 0
                    {
                        received.push(unsafe { r.Event.KeyEvent.uChar.UnicodeChar });
                    }
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
        fs::write(
            root.join("received"),
            String::from_utf16(&received).unwrap(),
        )
        .unwrap();
    }
    #[test]
    #[ignore = "internal fixture agent, launched only by private_console_transport"]
    fn private_console_agent() {
        use std::{fs, path::Path};
        let Ok(root) = std::env::var("SWITCHER_RC_FIXTURE") else {
            return;
        };
        let root = Path::new(&root);
        let expected: Process =
            serde_json::from_slice(&fs::read(root.join("process.json")).unwrap()).unwrap();
        let scenario = fs::read_to_string(root.join("scenario")).unwrap();
        if scenario == "korean_empty" || scenario == "korean_draft" {
            unsafe {
                FreeConsole();
            }
            assert_ne!(unsafe { AttachConsole(expected.pid) }, 0);
            let attached = Attached;
            let s = screen(&console("CONIN$").unwrap(), &console("CONOUT$").unwrap()).unwrap();
            assert!(s.lines.iter().any(|l| l == "한글"));
            assert!(s.lines.iter().any(|l| l.contains("한국어 대화")));
            drop(attached);
        }
        let phase = visit(
            &expected,
            true,
            scenario != "unobserved",
            scenario == "scrolled_notice",
            || {
                ensure!(scenario != "identity_changed", "Synthetic owner changed");
                Ok(())
            },
        )
        .unwrap_or("unavailable_or_changed");
        fs::write(root.join("result"), phase).unwrap();
    }
    #[test]
    fn private_console_transport() {
        use std::{
            fs,
            os::windows::process::CommandExt,
            process::{Child, Command, Stdio},
            time::Instant,
        };
        struct Owned(Child);
        impl Drop for Owned {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let exe = std::env::current_exe().unwrap();
        let root =
            std::env::temp_dir().join(format!("switcher-rc-console-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        for (scenario, phase, text) in [
            ("empty", "requested", "/rc\r"),
            ("korean_empty", "requested", "/rc\r"),
            ("korean_draft", "waiting_for_empty_prompt", ""),
            ("draft", "waiting_for_empty_prompt", ""),
            ("busy", "waiting_for_empty_prompt", ""),
            ("approval", "waiting_for_empty_prompt", ""),
            ("connected", "connected", ""),
            ("pid_changed", "unavailable_or_changed", ""),
            ("identity_changed", "unavailable_or_changed", ""),
            ("unobserved", "waiting_for_previous_connection_proof", ""),
            ("scrolled_notice", "requested", "/rc\r"),
        ] {
            let dir = root.join(scenario);
            fs::create_dir(&dir).unwrap();
            fs::write(dir.join("scenario"), scenario).unwrap();
            let mut host = Owned(
                Command::new(&exe)
                    .args(["--exact", HOST, "--ignored", "--nocapture"])
                    .env("SWITCHER_RC_FIXTURE", &dir)
                    .creation_flags(0x08000000)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap(),
            );
            let until = Instant::now() + Duration::from_secs(5);
            while !dir.join("ready").is_file() && Instant::now() < until {
                thread::sleep(Duration::from_millis(20));
            }
            assert!(
                dir.join("ready").is_file(),
                "private fixture did not become ready: {scenario}"
            );
            let mut expected = process(host.0.id()).unwrap();
            if scenario == "pid_changed" {
                expected.created += 1;
            }
            fs::write(
                dir.join("process.json"),
                serde_json::to_vec(&expected).unwrap(),
            )
            .unwrap();
            let mut agent = Owned(
                Command::new(&exe)
                    .args(["--exact", AGENT, "--ignored", "--nocapture"])
                    .env("SWITCHER_RC_FIXTURE", &dir)
                    .creation_flags(0x08000000)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap(),
            );
            let until = Instant::now() + Duration::from_secs(6);
            while agent.0.try_wait().unwrap().is_none() && Instant::now() < until {
                thread::sleep(Duration::from_millis(20));
            }
            assert_eq!(
                fs::read_to_string(dir.join("result")).unwrap(),
                phase,
                "scenario {scenario}"
            );
            while host.0.try_wait().unwrap().is_none() && Instant::now() < until {
                thread::sleep(Duration::from_millis(20));
            }
            assert_eq!(
                fs::read_to_string(dir.join("received")).unwrap(),
                text,
                "scenario {scenario}"
            );
        }
        fs::remove_dir_all(root).unwrap();
    }
}
