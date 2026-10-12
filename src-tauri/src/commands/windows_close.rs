//! A Codex process tree can contain Switcher and user programs launched by a
//! task. Only known Codex executables are eligible for individual termination.
use std::collections::HashSet;

use super::{normalize_windows_path, WindowsCodexProcess};

fn is_switcher(process: &WindowsCodexProcess) -> bool {
    let name = process.name.to_ascii_lowercase();
    name == "codex-switcher.exe"
        || name == "codex-web.exe"
        || normalize_windows_path(&process.executable_path).contains("\\codex switcher\\")
}

pub(super) fn plan_close(
    roots: &[u32],
    processes: &[WindowsCodexProcess],
    caller_pid: u32,
) -> Result<Vec<WindowsCodexProcess>, String> {
    let mut protected: HashSet<_> = processes
        .iter()
        .filter(|p| p.process_id == caller_pid || is_switcher(p))
        .map(|p| p.process_id)
        .collect();
    protected.insert(caller_pid);
    // Everything Switcher owns, including its webviews and close command, stays
    // alive. Do not rely on a root being excluded from /T: /T is never used.
    loop {
        let before = protected.len();
        for process in processes {
            // A separately launched Codex desktop remains closable even when
            // Switcher opened it. Never inherit protection into a chosen root.
            if protected.contains(&process.parent_process_id)
                && !roots.contains(&process.process_id)
            {
                protected.insert(process.process_id);
            }
        }
        if protected.len() == before {
            break;
        }
    }
    let mut targets = Vec::new();
    let mut seen = HashSet::new();
    for root_pid in roots {
        let root = processes
            .iter()
            .find(|p| p.process_id == *root_pid)
            .ok_or("Codex 종료 대상이 변경되었습니다. 다시 확인해 주세요")?;
        if protected.contains(root_pid) || !super::is_windows_codex_root_process(root) {
            return Err("Switcher 또는 Switcher가 소유한 프로세스는 종료하지 않습니다".into());
        }
        if root.executable_path.is_empty() {
            return Err("Codex 실행 파일을 확인할 수 없어 종료를 취소했습니다".into());
        }
        let mut queue = vec![*root_pid];
        let mut descendants = HashSet::new();
        while let Some(parent) = queue.pop() {
            for process in processes.iter().filter(|p| p.parent_process_id == parent) {
                if protected.contains(&process.process_id)
                    || !descendants.insert(process.process_id)
                {
                    continue;
                }
                queue.push(process.process_id);
            }
        }
        let root_path = normalize_windows_path(&root.executable_path);
        let root_dir = root_path
            .rsplit_once('\\')
            .map(|(dir, _)| dir)
            .unwrap_or("");
        for process in processes
            .iter()
            .filter(|p| descendants.contains(&p.process_id))
        {
            let path = normalize_windows_path(&process.executable_path);
            let name = process.name.to_ascii_lowercase();
            let command = process.command_line.to_ascii_lowercase();
            let electron_helper = path == root_path && command.contains("--type=");
            let bundled_backend = name == "codex.exe"
                && command.contains("app-server")
                && (path == format!("{root_dir}\\resources\\codex.exe")
                    || is_codex_runtime_path(&path));
            if !protected.contains(&process.process_id)
                && (electron_helper || bundled_backend)
                && seen.insert(process.process_id)
            {
                targets.push(process.clone());
            }
        }
        if seen.insert(root.process_id) {
            targets.push(root.clone());
        }
    }
    Ok(targets)
}

fn is_codex_runtime_path(path: &str) -> bool {
    let Some((prefix, _)) = path.rsplit_once("\\openai\\codex\\bin\\") else {
        return false;
    };
    if !prefix.ends_with("\\appdata\\local") {
        return false;
    }
    let tail = path.rsplit_once("\\openai\\codex\\bin\\").unwrap().1;
    let Some((version, filename)) = tail.split_once('\\') else {
        return false;
    };
    !version.is_empty() && !version.contains(['/', '\\']) && filename == "codex.exe"
}

pub(super) fn taskkill_args(pid: u32, force: bool) -> Vec<String> {
    let mut args = Vec::new();
    if force {
        args.push("/F".into());
    }
    args.extend(["/PID".into(), pid.to_string()]);
    args
}

/// PID-only termination still closes jobs owned by a Codex ancestor. A running
/// Switcher cannot leave an inherited job, so refuse before sending any close.
pub(super) fn ensure_independent_lifetime(
    targets: &[WindowsCodexProcess],
    processes: &[WindowsCodexProcess],
    caller_pid: u32,
) -> Result<(), String> {
    let mut visited = HashSet::new();
    let mut pid = caller_pid;
    while visited.insert(pid) {
        let Some(process) = processes.iter().find(|p| p.process_id == pid) else {
            break;
        };
        pid = process.parent_process_id;
        if targets.iter().any(|target| target.process_id == pid) {
            return Err("Codex is an ancestor of Switcher and may own its Windows job. Nothing was closed. Relaunch Switcher from the Start menu before switching accounts.".into());
        }
    }
    Ok(())
}

#[cfg(windows)]
pub(super) fn process_is_in_job(handle: *mut std::ffi::c_void) -> std::io::Result<bool> {
    // IsProcessInJob is an SDK export gated behind System_JobObjects in the
    // existing windows-sys pin. Match its ABI without changing the manifest.
    #[link(name = "kernel32")]
    extern "system" {
        fn IsProcessInJob(
            process: *mut std::ffi::c_void,
            job: *mut std::ffi::c_void,
            result: *mut i32,
        ) -> i32;
    }
    let mut in_job = 0;
    if unsafe { IsProcessInJob(handle, std::ptr::null_mut(), &mut in_job) } == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(in_job != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const CODEX: &str = r"C:\Program Files\WindowsApps\OpenAI.Codex_26.1002.7124.0_x64__2p2nqsd0c76g0\app\ChatGPT.exe";
    fn p(pid: u32, parent: u32, path: &str, args: &str) -> WindowsCodexProcess {
        WindowsCodexProcess {
            name: path.rsplit('\\').next().unwrap().into(),
            process_id: pid,
            parent_process_id: parent,
            executable_path: path.into(),
            command_line: format!("\"{path}\" {args}"),
            main_window_title: String::new(),
        }
    }

    #[test]
    fn switcher_launched_inside_codex_survives_with_its_helpers() {
        let processes = vec![
            p(10, 1, CODEX, ""),
            p(11, 10, CODEX, "--type=renderer"),
            p(
                20,
                10,
                r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
                "",
            ),
            p(
                30,
                20,
                r"C:\Users\test\AppData\Local\Codex Switcher\codex-switcher.exe",
                "",
            ),
            p(31, 30, r"C:\Runtime\msedgewebview2.exe", "--type=renderer"),
            p(32, 30, CODEX, "--type=renderer"),
            p(
                40,
                10,
                r"C:\Users\test\AppData\Local\OpenAI\Codex\bin\abc\codex.exe",
                "app-server",
            ),
            p(41, 40, r"C:\Windows\System32\cmd.exe", ""),
            p(
                50,
                41,
                r"C:\Users\test\AppData\Local\Codex Switcher\codex-web.exe",
                "",
            ),
            p(60, 10, r"C:\Work\python.exe", "user_task.py"),
        ];
        let target_ids: Vec<_> = plan_close(&[10], &processes, 30)
            .unwrap()
            .iter()
            .map(|p| p.process_id)
            .collect();
        assert_eq!(target_ids, vec![11, 40, 10]);
        for protected in [20, 30, 31, 32, 41, 50, 60] {
            assert!(!target_ids.contains(&protected));
        }
    }

    #[test]
    fn sibling_switcher_and_other_installation_are_not_selected() {
        let processes = vec![
            p(10, 1, CODEX, ""),
            p(11, 10, CODEX, "--type=gpu-process"),
            p(12, 10, r"C:\Other\ChatGPT.exe", "--type=renderer"),
            p(13, 10, r"C:\Temp\codex.exe", "app-server"),
            p(20, 1, r"C:\Apps\codex-switcher.exe", ""),
            p(21, 20, CODEX, "--type=renderer"),
        ];
        assert_eq!(
            plan_close(&[10], &processes, 20)
                .unwrap()
                .iter()
                .map(|p| p.process_id)
                .collect::<Vec<_>>(),
            vec![11, 10]
        );
    }

    #[test]
    fn desktop_opened_by_switcher_is_closable_but_switcher_is_not() {
        let processes = vec![
            p(10, 20, CODEX, ""),
            p(20, 1, r"C:\Apps\codex-switcher.exe", ""),
        ];
        assert_eq!(plan_close(&[10], &processes, 20).unwrap()[0].process_id, 10);
        assert!(plan_close(&[20], &processes, 20).is_err());
        assert!(plan_close(&[99], &processes, 20).is_err());
    }

    #[test]
    fn termination_never_requests_an_entire_process_tree() {
        assert_eq!(taskkill_args(123, false), vec!["/PID", "123"]);
        assert_eq!(taskkill_args(123, true), vec!["/F", "/PID", "123"]);
        for force in [false, true] {
            assert!(!taskkill_args(123, force)
                .iter()
                .any(|a| a.eq_ignore_ascii_case("/T")));
        }
    }

    #[test]
    fn inherited_lifetime_blocks_desktop_and_backend_ancestors() {
        let processes = vec![
            p(10, 1, CODEX, ""),
            p(11, 10, r"C:\Apps\Codex\resources\codex.exe", "app-server"),
            p(20, 11, r"C:\Windows\System32\cmd.exe", ""),
            p(30, 20, r"C:\Apps\codex-switcher.exe", ""),
        ];
        for target in [&processes[0], &processes[1]] {
            assert!(ensure_independent_lifetime(&[target.clone()], &processes, 30).is_err());
        }
        assert!(ensure_independent_lifetime(&[processes[0].clone()], &processes, 99).is_ok());
        let direct_child = vec![
            p(10, 30, CODEX, ""),
            p(30, 1, r"C:\Apps\codex-switcher.exe", ""),
        ];
        assert!(ensure_independent_lifetime(&[direct_child[0].clone()], &direct_child, 30).is_ok());
    }

    #[test]
    fn lifetime_check_terminates_on_a_cyclic_snapshot() {
        let processes = vec![
            p(20, 30, r"C:\Windows\System32\cmd.exe", ""),
            p(30, 20, r"C:\Apps\codex-switcher.exe", ""),
        ];
        assert!(ensure_independent_lifetime(&[], &processes, 30).is_ok());
    }

    #[test]
    fn explorer_parent_does_not_block_a_separately_launched_desktop() {
        // Job ownership is unknown and cannot be inferred from IsProcessInJob.
        // Only a selected Codex target in Switcher's parent chain blocks close.
        let processes = vec![
            p(10, 20, CODEX, ""),
            p(20, 1, r"C:\Windows\explorer.exe", ""),
            p(30, 20, r"C:\Apps\codex-switcher.exe", ""),
        ];
        assert!(ensure_independent_lifetime(&[processes[0].clone()], &processes, 30).is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn windows_close_fixture_child() {
        let Ok(role) = std::env::var("SWITCHER_CLOSE_FIXTURE_ROLE") else {
            return;
        };
        let dir = std::path::PathBuf::from(std::env::var_os("SWITCHER_CLOSE_FIXTURE_DIR").unwrap());
        assert!(dir.starts_with(std::env::temp_dir()));
        assert!(std::env::current_exe().unwrap().starts_with(&dir));
        let children = if role == "root" {
            let switcher = spawn_fixture(&dir.join("codex-switcher.exe"), &dir, "switcher", false);
            let backend = spawn_fixture(
                &dir.join(r"Users\test\AppData\Local\OpenAI\Codex\bin\fixture\codex.exe"),
                &dir,
                "backend",
                true,
            );
            let user_task = spawn_fixture(&dir.join("user-task.exe"), &dir, "task", false);
            std::fs::write(
                dir.join("ready.json"),
                serde_json::to_vec(&vec![
                    std::process::id(),
                    switcher.id(),
                    backend.id(),
                    user_task.id(),
                ])
                .unwrap(),
            )
            .unwrap();
            vec![switcher, backend, user_task]
        } else {
            Vec::new()
        };
        // Bound fixture lifetime even if its parent test fails before cleanup.
        std::thread::sleep(std::time::Duration::from_secs(30));
        for mut child in children {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    #[cfg(windows)]
    fn spawn_fixture(
        path: &std::path::Path,
        dir: &std::path::Path,
        role: &str,
        backend: bool,
    ) -> std::process::Child {
        use std::os::windows::process::CommandExt;
        let mut command = std::process::Command::new(path);
        command
            .args([
                "--exact",
                "commands::process::windows_close::tests::windows_close_fixture_child",
                "--test-threads=1",
            ])
            .env("SWITCHER_CLOSE_FIXTURE_DIR", dir)
            .env("SWITCHER_CLOSE_FIXTURE_ROLE", role)
            .creation_flags(super::super::CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if backend {
            command.args(["--skip", "app-server"]);
        }
        command.spawn().unwrap()
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "Terminates only isolated copied test executables; run explicitly"]
    fn closes_fixture_codex_but_preserves_descendant_switcher_and_user_task() {
        for force in [false, true] {
            let dir = std::env::temp_dir()
                .join(format!("switcher-close-fixture-{}", uuid::Uuid::new_v4()));
            let executable = std::env::current_exe().unwrap();
            for relative in [
                "Codex.exe",
                "codex-switcher.exe",
                "user-task.exe",
                r"Users\test\AppData\Local\OpenAI\Codex\bin\fixture\codex.exe",
            ] {
                let path = dir.join(relative);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::copy(&executable, &path).unwrap();
            }
            let mut root = spawn_fixture(&dir.join("Codex.exe"), &dir, "root", false);
            let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let ids: Vec<u32> = loop {
                if let Ok(bytes) = std::fs::read(dir.join("ready.json")) {
                    if let Ok(ids) = serde_json::from_slice(&bytes) {
                        break ids;
                    }
                }
                assert!(
                    std::time::Instant::now() < until,
                    "fixture did not become ready"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            };
            assert_eq!(ids[0], root.id());
            let processes = super::super::read_windows_processes(false).unwrap();
            let plan = plan_close(&[root.id()], &processes, std::process::id()).unwrap();
            let targets: Vec<_> = plan.iter().map(|p| p.process_id).collect();
            assert_eq!(targets.len(), 2);
            assert!(targets.contains(&ids[0]) && targets.contains(&ids[2]));
            assert!(!targets.contains(&ids[1]) && !targets.contains(&ids[3]));
            let mut wrong = plan[0].clone();
            wrong.executable_path.push_str(".not-the-same-process");
            assert!(!super::super::close_windows_process(&wrong, true));
            if !force {
                // Hidden console fixtures refuse a window-close request. It
                // must fail without silently escalating to force.
                let target = plan.iter().find(|p| p.process_id == root.id()).unwrap();
                assert!(!super::super::close_windows_process(target, false));
                for pid in &ids {
                    assert!(super::super::process_exists(*pid));
                }
            }
            for target in &plan {
                assert!(super::super::close_windows_process(target, true));
            }
            assert!(!super::super::process_exists(ids[0]));
            assert!(!super::super::process_exists(ids[2]));
            assert!(
                super::super::process_exists(ids[1]),
                "fixture Switcher was terminated"
            );
            assert!(
                super::super::process_exists(ids[3]),
                "fixture user task was terminated"
            );
            // Cleanup only the two surviving fixture processes, verified against
            // the snapshot and this unique temporary directory.
            for pid in [ids[1], ids[3]] {
                let process = processes.iter().find(|p| p.process_id == pid).unwrap();
                assert!(std::path::Path::new(&process.executable_path).starts_with(&dir));
                assert!(super::super::close_windows_process(process, true));
            }
            let _ = root.wait();
            std::fs::remove_dir_all(&dir).unwrap();
            println!("PASS: graceful_attempt={}, forced Codex and backend close; descendant Switcher and user task stayed alive", !force);
        }
    }
}
