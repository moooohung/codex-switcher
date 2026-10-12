//! Use synthetic credentials in isolated homes; never touch the user's login.
use codex_switcher_lib::{auth::*, types::*};
use std::{path::PathBuf, sync::Mutex};

static ENV_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn provider_plan_preferences_are_persisted_independently() {
    let _guard = ENV_LOCK.lock().unwrap();
    let sandbox = Sandbox::new();
    update_app_settings(|settings| {
        settings.auto_switch_enabled = false;
        settings.auto_switch_threshold_percent = 95.0;
        settings.auto_switch_plan_preference = PlanPreference::Pro;
        Ok(())
    })
    .unwrap();
    let codex_settings = std::fs::read(get_settings_file().unwrap()).unwrap();
    let claude = codex_switcher_lib::claude::storage::Manager {
        root: get_config_dir().unwrap().join("claude"),
        live_dir: sandbox.dir.join("claude-live"),
        live_config: sandbox.dir.join("claude-live/.claude.json"),
    };
    let mut settings = codex_switcher_lib::claude::ClaudeSettings::default();
    settings.auto_switch_enabled = true;
    settings.auto_switch_plan_preference = PlanPreference::Max;
    claude.save_settings(&settings).unwrap();
    assert_eq!(
        std::fs::read(get_settings_file().unwrap()).unwrap(),
        codex_settings
    );
    assert_eq!(
        load_app_settings().unwrap().auto_switch_plan_preference,
        PlanPreference::Pro
    );
    assert_eq!(
        load_app_settings().unwrap().auto_switch_threshold_percent,
        95.0
    );
    assert!(!load_app_settings().unwrap().auto_switch_enabled);
    assert_eq!(
        claude.settings().unwrap().auto_switch_plan_preference,
        PlanPreference::Max
    );
    let claude_bytes = std::fs::read(claude.root.join("settings.json")).unwrap();
    update_app_settings(|settings| {
        settings.auto_switch_plan_preference = PlanPreference::Plus;
        Ok(())
    })
    .unwrap();
    assert_eq!(
        std::fs::read(claude.root.join("settings.json")).unwrap(),
        claude_bytes
    );
    assert!(!sandbox.dir.join("claude-live/.credentials.json").exists());
}
struct Sandbox {
    dir: PathBuf,
    previous: Vec<(&'static str, Option<std::ffi::OsString>)>,
}
impl Sandbox {
    fn new() -> Self {
        let dir =
            std::env::temp_dir().join(format!("switcher-regression-{}", uuid::Uuid::new_v4()));
        let mut previous = Vec::new();
        for (key, path) in [
            ("CODEX_SWITCHER_HOME", dir.join("switcher")),
            ("CODEX_HOME", dir.join("codex")),
        ] {
            previous.push((key, std::env::var_os(key)));
            std::env::set_var(key, path);
        }
        Self { dir, previous }
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        for (key, old) in &self.previous {
            if let Some(value) = old {
                std::env::set_var(key, value);
            } else {
                std::env::remove_var(key);
            }
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn auth_contention_unchanged_transactions_do_not_trigger_file_watchers() {
    let _guard = ENV_LOCK.lock().unwrap();
    let _sandbox = Sandbox::new();
    let account = add_account(StoredAccount::new_api_key(
        "Synthetic".into(),
        "synthetic".into(),
    ))
    .unwrap();
    let path = get_accounts_file().unwrap();
    let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
    let contents = std::fs::read(&path).unwrap();
    let backup = std::fs::read(persistence::backup_path(&path)).ok();
    for _ in 0..10 {
        update_accounts(|_| Ok(())).unwrap();
        update_account_metadata(&account.id, None, None, None, None).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified,
            "a read-only token reconciliation must not trigger the accounts file watcher"
        );
    }
    assert_eq!(std::fs::read(&path).unwrap(), contents);
    assert_eq!(std::fs::read(persistence::backup_path(&path)).ok(), backup);
    update_account_metadata(&account.id, Some("Changed".into()), None, None, None).unwrap();
    assert_ne!(std::fs::read(&path).unwrap(), contents);
    assert_eq!(get_account(&account.id).unwrap().unwrap().name, "Changed");
}

#[test]
fn usage_snapshot_uses_live_tokens_without_writing_or_overwriting_pending_rotation() {
    let _guard = ENV_LOCK.lock().unwrap();
    let _sandbox = Sandbox::new();
    let payload = base64::Engine::encode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        r#"{"exp":1,"https://api.openai.com/auth":{"chatgpt_account_id":"synthetic-workspace"}}"#,
    );
    let account = add_account(StoredAccount::new_chatgpt(
        "Synthetic".into(),
        None,
        None,
        None,
        format!("header.{payload}.signature"),
        "old-access".into(),
        "old-refresh".into(),
        Some("synthetic-workspace".into()),
    ))
    .unwrap();
    switch_to_account(&account).unwrap();
    let mut live = account.clone();
    let AuthData::ChatGPT {
        access_token,
        refresh_token,
        ..
    } = &mut live.auth_data
    else {
        unreachable!()
    };
    *access_token = "live-access".into();
    *refresh_token = "live-refresh".into();
    // Simulate Codex rotating the credentials without updating the saved copy.
    switch_to_account(&live).unwrap();
    let before = std::fs::read(get_accounts_file().unwrap()).unwrap();
    let snapshot = chatgpt_account_for_usage(&account).unwrap();
    assert!(credentials_match_auth(
        &snapshot,
        &read_current_auth().unwrap().unwrap()
    ));
    assert_eq!(std::fs::read(get_accounts_file().unwrap()).unwrap(), before);
    let pending = update_account_chatgpt_tokens(
        &account.id,
        format!("header.{payload}.next"),
        "pending-access".into(),
        "pending-refresh".into(),
        Some("synthetic-workspace".into()),
        None,
        None,
        None,
    )
    .unwrap();
    let before = std::fs::read(get_accounts_file().unwrap()).unwrap();
    let snapshot = chatgpt_account_for_usage(&account).unwrap();
    assert!(snapshot.auth_sync_pending);
    assert_eq!(
        serde_json::to_value(snapshot.auth_data).unwrap(),
        serde_json::to_value(pending.auth_data).unwrap()
    );
    assert_eq!(std::fs::read(get_accounts_file().unwrap()).unwrap(), before);
}

#[test]
fn additions_deletion_and_stale_markers_follow_the_credential_file() {
    let _guard = ENV_LOCK.lock().unwrap();
    let _sandbox = Sandbox::new();
    let a = add_account(StoredAccount::new_api_key("A".into(), "synthetic-a".into())).unwrap();
    let b = add_account(StoredAccount::new_api_key("B".into(), "synthetic-b".into())).unwrap();
    assert_eq!(load_accounts().unwrap().active_account_id, None);
    switch_to_account(&a).unwrap();
    assert_eq!(
        load_accounts().unwrap().active_account_id,
        Some(a.id.clone())
    );
    let mut damaged_marker = load_accounts().unwrap();
    damaged_marker.active_account_id = Some(b.id.clone());
    persistence::atomic_write(
        &get_accounts_file().unwrap(),
        &serde_json::to_vec(&damaged_marker).unwrap(),
    )
    .unwrap();
    assert_eq!(
        load_accounts().unwrap().active_account_id,
        Some(a.id.clone())
    );
    switch_to_account(&b).unwrap();
    assert_eq!(
        load_accounts().unwrap().active_account_id,
        Some(b.id.clone())
    );
    remove_account(&b.id).unwrap();
    assert_eq!(load_accounts().unwrap().active_account_id, None);
    assert!(credentials_match_auth(
        &b,
        &read_current_auth().unwrap().unwrap()
    ));
    switch_to_account(&a).unwrap();
    std::fs::remove_file(get_codex_auth_file().unwrap()).unwrap();
    assert_eq!(load_accounts().unwrap().active_account_id, None);
    assert!(read_current_auth().unwrap().is_none());
}

#[test]
fn pending_rotation_survives_old_live_auth_and_clears_after_verified_write() {
    let _guard = ENV_LOCK.lock().unwrap();
    let _sandbox = Sandbox::new();
    let original = add_account(StoredAccount::new_chatgpt(
        "A".into(),
        None,
        None,
        None,
        "old-id".into(),
        "old-access".into(),
        "old-refresh".into(),
        Some("workspace-a".into()),
    ))
    .unwrap();
    switch_to_account(&original).unwrap();
    let rotated = update_account_chatgpt_tokens(
        &original.id,
        "new-id".into(),
        "new-access".into(),
        "new-refresh".into(),
        Some("workspace-a".into()),
        None,
        None,
        None,
    )
    .unwrap();
    let live = read_current_auth().unwrap().unwrap();
    update_accounts(|store| {
        assert!(!sync_active_account_tokens(store, &live));
        Ok(())
    })
    .unwrap();
    let saved = get_account(&original.id).unwrap().unwrap();
    assert!(saved.auth_sync_pending);
    assert!(!credentials_match_auth(&saved, &live));
    assert_eq!(
        serde_json::to_value(&saved.auth_data).unwrap(),
        serde_json::to_value(&rotated.auth_data).unwrap()
    );
    switch_to_account(&saved).unwrap();
    assert!(credentials_match_auth(
        &saved,
        &read_current_auth().unwrap().unwrap()
    ));
    assert!(
        !get_account(&original.id)
            .unwrap()
            .unwrap()
            .auth_sync_pending
    );
}

#[test]
fn transaction_child() {
    let Ok(worker) = std::env::var("SWITCHER_TEST_CHILD") else {
        return;
    };
    assert!(get_config_dir()
        .unwrap()
        .to_string_lossy()
        .contains("switcher-regression-"));
    for index in 0..12 {
        let a = add_account(StoredAccount::new_api_key(
            format!("worker-{worker}-{index}"),
            "synthetic".into(),
        ))
        .unwrap();
        update_account_metadata(
            &a.id,
            None,
            Some("synthetic@example.invalid".into()),
            None,
            None,
        )
        .unwrap();
    }
}

#[test]
fn independent_processes_do_not_lose_accounts_or_metadata() {
    let _guard = ENV_LOCK.lock().unwrap();
    let _sandbox = Sandbox::new();
    let executable = std::env::current_exe().unwrap();
    let children: Vec<_> = (0..4)
        .map(|worker| {
            std::process::Command::new(&executable)
                .args(["--exact", "transaction_child", "--test-threads=1"])
                .env("SWITCHER_TEST_CHILD", worker.to_string())
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    for mut child in children {
        assert!(child.wait().unwrap().success());
    }
    let store = load_accounts().unwrap();
    assert_eq!(store.accounts.len(), 48);
    assert!(store
        .accounts
        .iter()
        .all(|a| a.email.as_deref() == Some("synthetic@example.invalid")));
    assert_eq!(store.active_account_id, None);
}
