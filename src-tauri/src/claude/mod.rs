//! Claude Code account profiles. No Codex credentials or settings are used here.
pub mod commands;
mod locks;
mod monitor;
#[cfg(windows)]
mod pwsh_session;
mod quota;
mod remote_control;
#[cfg(windows)]
mod remote_control_console;
mod remote_control_live;
#[cfg(windows)]
mod running_count;
#[cfg(windows)]
mod running_observation;
mod runtime;
pub mod storage;
pub use commands::{claude_command, start_monitor};
pub(crate) use locks::CredentialsGuard as LiveSettingsCredentialsGuard;
pub use remote_control_live::handle_helper as handle_remote_control_helper;
pub(crate) use runtime::install_bridge as install_cache_observation;
pub use runtime::open as open_cache_session;
pub use runtime::statusline_entry;
pub(crate) use runtime::{cache_session_running, resume_cache_session};

#[cfg(windows)]
pub(crate) fn interactive_process_created() -> anyhow::Result<u64> {
    let pid = std::env::var("CLAUDE_PID")?.parse()?;
    Ok(remote_control_console::process(pid)?.created)
}

#[cfg(windows)]
pub(crate) fn interactive_process_ended(pid: u32, created: u64) -> bool {
    remote_control_console::process_generation_ended(pid, created)
}

#[cfg(windows)]
pub(crate) fn interactive_process_matches(pid: u32, created: u64) -> bool {
    remote_control_console::process(pid)
        .is_ok_and(|process| process.created == created && interactive_image(&process.image))
}

#[cfg(windows)]
fn interactive_image(image: &std::path::Path) -> bool {
    let Some(name) = image.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name.eq_ignore_ascii_case("claude.exe")
        || (image
            .parent()
            .is_some_and(|parent| parent.ends_with(".local/share/claude/versions"))
            && name.split('.').count() == 3
            && name
                .split('.')
                .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit())))
}

#[cfg(all(windows, test))]
mod cache_process_image_tests {
    #[test]
    fn supports_native_install_alias_and_versioned_executable_without_arbitrary_version_names() {
        use std::path::Path;
        for path in [
            "C:/fixture/.local/bin/claude.exe",
            "C:/fixture/.local/share/claude/versions/2.1.296",
        ] {
            assert!(super::interactive_image(Path::new(path)), "{path}");
        }
        for path in [
            "C:/fixture/2.1.296",
            "C:/fixture/.local/share/claude/versions/2.1.296-beta",
            "C:/fixture/.local/share/claude/versions/2.1.296.exe",
            "C:/fixture/node.exe",
        ] {
            assert!(!super::interactive_image(Path::new(path)), "{path}");
        }
        assert!(!super::interactive_process_matches(std::process::id(), 1));
    }
}

use crate::types::{PlanPreference, UsageInfo};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Serialize, Deserialize)]
pub struct ClaudeAccount {
    pub id: String,
    pub name: String,
    pub email: Option<String>,
    pub account_uuid: Option<String>,
    pub plan_type: Option<String>,
    pub created_at: String,
    pub last_used_at: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ClaudeStore {
    pub provider: String,
    pub version: u32,
    pub accounts: Vec<ClaudeAccount>,
    pub masked_account_ids: Vec<String>,
}
impl Default for ClaudeStore {
    fn default() -> Self {
        Self {
            provider: "claude".into(),
            version: 1,
            accounts: vec![],
            masked_account_ids: vec![],
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ClaudeSettings {
    pub auto_switch_enabled: bool,
    pub auto_switch_threshold_percent: f64,
    pub auto_switch_plan_preference: PlanPreference,
    pub reopen_after_switch: bool,
    pub close_before_switch: bool,
    pub capture_usage: bool,
    pub protect_prompt_cache: bool,
    /// None preserves the user's existing official Claude setting.
    pub auto_remote_control: Option<bool>,
    pub warmup_all: bool,
    pub warmup_account_ids: Vec<String>,
    pub timed_warmup_times: Vec<String>,
    pub theme: String,
    pub sort_by: String,
    pub model: String,
}
impl Default for ClaudeSettings {
    fn default() -> Self {
        Self {
            auto_switch_enabled: false,
            auto_switch_threshold_percent: 99.5,
            auto_switch_plan_preference: PlanPreference::Any,
            reopen_after_switch: false,
            close_before_switch: false,
            capture_usage: true,
            protect_prompt_cache: false,
            auto_remote_control: None,
            warmup_all: false,
            warmup_account_ids: vec![],
            timed_warmup_times: vec![],
            theme: "system".into(),
            sort_by: "reset".into(),
            model: "sonnet".into(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Default)]
pub struct SessionUsage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_usd: f64,
    pub day: String,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct UsageReceipt {
    pub account_id: String,
    pub observed_at: i64,
    pub usage: UsageInfo,
    #[serde(default)]
    pub sessions: BTreeMap<String, SessionUsage>,
}

#[derive(Serialize, Deserialize)]
pub struct BackupAccount {
    pub account: ClaudeAccount,
    pub credentials: serde_json::Value,
    pub config: serde_json::Value,
    pub settings: serde_json::Value,
}
#[derive(Serialize, Deserialize)]
pub struct ClaudeBackup {
    pub provider: String,
    pub version: u32,
    pub accounts: Vec<BackupAccount>,
    pub settings: ClaudeSettings,
}

#[derive(Clone, Serialize, Deserialize, Default)]
pub struct MonitorStatus {
    pub phase: String,
    pub message: String,
    pub source_id: Option<String>,
    pub target_id: Option<String>,
    pub switched_at: Option<String>,
}
