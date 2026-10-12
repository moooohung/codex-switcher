//! Official startup policy plus the lifecycle/console adapter for existing TUIs.
//! https://code.claude.com/docs/en/remote-control#enable-remote-control-for-all-sessions
use super::storage::{same_identity, write_json, Manager};
use anyhow::{ensure, Context, Result};
use serde_json::Value;

fn merge(settings: &mut Value, choice: Option<bool>) -> Result<bool> {
    let Some(enabled) = choice else {
        return Ok(false);
    };
    let object = settings
        .as_object_mut()
        .context("Claude settings must be an object")?;
    if object
        .get("remoteControlAtStartup")
        .and_then(Value::as_bool)
        == Some(enabled)
    {
        return Ok(false);
    }
    object.insert("remoteControlAtStartup".into(), Value::Bool(enabled));
    Ok(true)
}

pub(super) fn apply(m: &Manager, id: &str, live: bool) -> Result<()> {
    let choice = m.settings()?.auto_remote_control;
    if choice.is_none() {
        return Ok(());
    }
    let account = m.account(id)?;
    let profile = m.profile(id)?;
    let dir = if live { &m.live_dir } else { &profile };
    // Same ordering as activation: credentials before settings. Check the
    // account again while locked; a delayed settings task cannot follow a swap.
    let _credentials = if live {
        Some(super::locks::CredentialsGuard::acquire(dir)?)
    } else {
        None
    };
    let _settings = crate::auth::persistence::lock_file(&dir.join("switcher-settings.lock"))?;
    if live {
        let config = Manager::read_config(&m.live_config)?;
        let credentials = Manager::read_config(&dir.join(".credentials.json"))?;
        let saved = Manager::read_config(&profile.join(".credentials.json"))?;
        ensure!(
            same_identity(&account, &config, &credentials, &saved),
            "Remote Control setting account changed"
        );
    }
    let path = dir.join("settings.json");
    let mut settings = Manager::read_config(&path)?;
    let mut changed = merge(&mut settings, choice)?;
    if live {
        changed |= super::remote_control_live::merge(&mut settings, choice == Some(true), dir)?;
    }
    if changed {
        write_json(&path, &settings)?;
    }
    Ok(())
}

pub(super) fn reconcile(m: &Manager) -> Result<()> {
    if m.settings()?.auto_remote_control.is_none() {
        return Ok(());
    }
    let store = m.store()?;
    for account in &store.accounts {
        apply(m, &account.id, false)?;
    }
    if let Some(active) = m.active_id(&store)? {
        apply(m, &active, true)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn startup_policy_is_explicit_idempotent_and_preserves_remote_control_restrictions() {
        let original = json!({"remoteControlAtStartup":false,"disableRemoteControl":true,
            "permissions":{"deny":["Bash(*)"]},"hooks":{"Stop":[]}});
        let mut settings = original.clone();
        assert!(!merge(&mut settings, None).unwrap());
        assert_eq!(settings, original);
        assert!(merge(&mut settings, Some(true)).unwrap());
        assert!(!merge(&mut settings, Some(true)).unwrap());
        assert_eq!(settings["disableRemoteControl"], true);
        assert_eq!(settings["hooks"], original["hooks"]);
        assert_eq!(settings["permissions"], original["permissions"]);
        assert!(merge(&mut settings, Some(false)).unwrap());
        assert_eq!(settings, original);
    }
    #[test]
    fn malformed_policy_input_is_not_repaired_or_overwritten() {
        for mut settings in [json!([]), json!(null)] {
            let before = settings.clone();
            assert!(merge(&mut settings, Some(true)).is_err());
            assert_eq!(settings, before);
        }
        let old: super::super::ClaudeSettings = serde_json::from_value(json!({})).unwrap();
        assert_eq!(old.auto_remote_control, None);
    }

    #[test]
    fn apply_preserves_auth_and_transcript_and_rejects_different_live_owner() {
        let root =
            std::env::temp_dir().join(format!("remote-control-test-{}", uuid::Uuid::new_v4()));
        let m = Manager {
            root: root.join("switcher/claude"),
            live_dir: root.join("live"),
            live_config: root.join("live/.claude.json"),
        };
        let mut choice = super::super::ClaudeSettings::default();
        choice.auto_remote_control = Some(true);
        m.save_settings(&choice).unwrap();
        let oauth = json!({"claudeAiOauth":{"accessToken":"synthetic","refreshToken":"synthetic-refresh",
            "expiresAt":4000000000000_i64}});
        let config = json!({"oauthAccount":{"accountUuid":"synthetic-owner"}});
        let original = json!({"model":"keep","disableRemoteControl":true,"hooks":{"Stop":[]}});
        let account = m.add("synthetic", &oauth, &config, &original).unwrap();
        let profile = m.profile(&account.id).unwrap();
        let transcript = profile.join("synthetic-session.jsonl");
        std::fs::write(&transcript, "unchanged synthetic conversation\n").unwrap();
        let paths = [
            profile.join(".credentials.json"),
            profile.join(".claude.json"),
            transcript,
        ];
        let before: Vec<_> = paths.iter().map(|p| std::fs::read(p).unwrap()).collect();
        apply(&m, &account.id, false).unwrap();
        let settings = profile.join("settings.json");
        let modified = std::fs::metadata(&settings).unwrap().modified().unwrap();
        apply(&m, &account.id, false).unwrap();
        assert_eq!(
            std::fs::metadata(&settings).unwrap().modified().unwrap(),
            modified
        );
        for (path, bytes) in paths.iter().zip(&before) {
            assert_eq!(&std::fs::read(path).unwrap(), bytes);
        }
        assert_eq!(
            Manager::read_config(&settings).unwrap()["disableRemoteControl"],
            true
        );
        write_json(
            &m.live_config,
            &json!({"oauthAccount":{"accountUuid":"other-owner"}}),
        )
        .unwrap();
        write_json(&m.live_dir.join(".credentials.json"), &oauth).unwrap();
        let live_settings = m.live_dir.join("settings.json");
        write_json(&live_settings, &original).unwrap();
        let live_before = std::fs::read(&live_settings).unwrap();
        assert!(apply(&m, &account.id, true).is_err());
        assert_eq!(std::fs::read(&live_settings).unwrap(), live_before);
        std::fs::remove_dir_all(root).unwrap();
    }
}
