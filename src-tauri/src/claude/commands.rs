use super::{
    runtime,
    storage::{eligible_usage, write_json, Manager},
    *,
};
use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use chrono::Utc;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        LazyLock,
    },
};
use tokio::sync::Mutex;

static MONITOR_STARTED: AtomicBool = AtomicBool::new(false);
static MONITOR: Mutex<()> = Mutex::const_new(());
static STATUS: LazyLock<Mutex<MonitorStatus>> = LazyLock::new(|| {
    Mutex::new(MonitorStatus {
        phase: "disabled".into(),
        message: "Claude 자동 전환 꺼짐".into(),
        ..Default::default()
    })
});

fn text<'a>(payload: &'a Value, key: &str) -> Result<&'a str> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("Missing {key}"))
}
fn account_usage(m: &Manager, a: &ClaudeAccount) -> (crate::types::UsageInfo, Option<i64>) {
    // A card read neither repairs a corrupt receipt from .bak nor lets one
    // unrelated account's receipt make the complete inventory unavailable.
    let receipt = (|| -> Result<Option<UsageReceipt>> {
        use std::io::Read;
        let path = m.profile(&a.id)?.join("usage.json");
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let limit = 8 * 1024 * 1024;
        anyhow::ensure!(
            file.metadata()?.is_file() && file.metadata()?.len() <= limit,
            "Invalid usage file"
        );
        let mut bytes = Vec::new();
        file.take(limit + 1).read_to_end(&mut bytes)?;
        anyhow::ensure!(bytes.len() as u64 <= limit, "Oversized usage file");
        let receipt: UsageReceipt = serde_json::from_slice(&bytes)?;
        anyhow::ensure!(
            receipt.account_id == a.id && receipt.usage.account_id == a.id,
            "Usage receipt belongs to another account"
        );
        anyhow::ensure!(
            receipt.observed_at <= Utc::now().timestamp() + 5,
            "Future usage observation"
        );
        Ok(Some(receipt))
    })();
    let receipt = match receipt {
        Ok(receipt) => receipt,
        Err(_) => return (crate::types::UsageInfo::error(a.id.clone(),
            "이 계정의 사용량 기록을 읽지 못했습니다. 새로고침하거나 인증 상태를 확인해 주세요.".into()), None),
    };
    let observed = receipt.as_ref().map(|r| r.observed_at);
    let mut usage = receipt.map(|r| r.usage).unwrap_or_else(|| {
        crate::types::UsageInfo::error(
            a.id.clone(),
            "Claude 계정 사용량을 조회하는 중입니다. 새로고침으로 다시 확인할 수 있습니다.".into(),
        )
    });
    usage.plan_type = a.plan_type.clone();
    if let Some(at) = observed {
        if Utc::now().timestamp() - at > 300 {
            let stale = "5분 이상 지난 관측값입니다. 자동 전환 판단에는 사용하지 않습니다.";
            usage.error = Some(match usage.error.take() {
                Some(error) => format!("{error} · {stale}"),
                None => stale.into(),
            });
        }
    }
    (usage, observed)
}
fn snapshot(m: &Manager) -> Result<Value> {
    let store = m.store()?;
    let active = m.active_id(&store)?;
    let mut accounts = vec![];
    for a in &store.accounts {
        let (usage, observed) = account_usage(m, a);
        accounts.push(json!({"id":a.id,"name":a.name,"email":a.email,"plan_type":a.plan_type,"subscription_expires_at":null,"auth_mode":"claude_ai","is_active":active.as_deref()==Some(a.id.as_str()),"auth_sync_pending":m.has_relogin_barrier(&a.id,a.account_uuid.as_deref())?,"created_at":a.created_at,"last_used_at":a.last_used_at,"usage":usage,"usage_observed_at":observed,"profile_directory":m.profile(&a.id)?}));
    }
    Ok(
        json!({"accounts":accounts,"masked_account_ids":store.masked_account_ids,"settings":m.settings()?,"cli_available":runtime::executable().is_ok(),"cli_latest_version":runtime::CLAUDE_CODE_VERSION,"active_account_id":active,"live_directory":m.live_dir,"warmup_status":monitor::warmup_status(m)?,"remote_control_status":super::remote_control_live::status_for_ui(m)}),
    )
}
fn stats(m: &Manager, id: &str) -> Result<Value> {
    let receipt = m.receipt(id)?;
    let mut daily = BTreeMap::<String, i64>::new();
    let mut sessions = 0;
    if let Some(r) = &receipt {
        sessions = r.sessions.len();
        for session in r.sessions.values() {
            *daily.entry(session.day.clone()).or_default() +=
                session.input_tokens + session.output_tokens;
        }
    }
    let total: i64 = daily.values().sum();
    let peak = daily.values().copied().max();
    Ok(
        json!({"account_id":id,"available":receipt.is_some(),"source":"Claude Code statusline snapshots; connected sessions only, grouped by last observed day","generated_at":receipt.as_ref().and_then(|r|chrono::DateTime::from_timestamp(r.observed_at,0)).map(|t|t.to_rfc3339()),"stats_as_of":daily.keys().next_back(),"summary":{"lifetime_tokens":if sessions>0{Some(total)}else{None},"peak_daily_tokens":peak,"longest_task_seconds":null,"current_streak_days":null,"longest_streak_days":null},"activity":{"fast_mode_percent":null,"reasoning_effort":null,"reasoning_effort_percent":null,"skills_explored":null,"total_skills_used":null,"total_threads":sessions},"daily":daily.into_iter().map(|(date,tokens)|json!({"date":date,"tokens":tokens})).collect::<Vec<_>>(),"top_invocations":[],"reset_credits":null,"error":if receipt.is_none(){Some("사용량 연결 후 수집한 Claude 세션 통계가 여기에 표시됩니다.")}else{None}}),
    )
}
async fn snapshot_with_runtime(m: &Manager) -> Result<Value> {
    let mut snapshot = snapshot(m)?;
    let running = runtime::running_state(m).await?;
    snapshot["monitor_status"] = serde_json::to_value(STATUS.lock().await.clone())?;
    snapshot["managed_running_ids"] = json!(running.managed_running_ids);
    snapshot["running_process_count"] = json!(running.count);
    snapshot["running_process_error"] = json!(running.error);
    Ok(snapshot)
}
pub fn choose_candidate(
    receipts: &[UsageReceipt],
    active: &str,
    threshold: f64,
    now: i64,
    preference: PlanPreference,
) -> Option<String> {
    receipts
        .iter()
        .filter(|r| {
            r.account_id != active
                && eligible_usage(r, now)
                && r.usage.primary_used_percent.unwrap_or(0.0) < threshold
                && r.usage.secondary_used_percent.unwrap_or(0.0) < threshold
        })
        .min_by(|a, b| {
            (!preference.matches(a.usage.plan_type.as_deref()))
                .cmp(&(!preference.matches(b.usage.plan_type.as_deref())))
                .then_with(|| {
                    a.usage
                        .primary_used_percent
                        .unwrap_or(0.0)
                        .max(a.usage.secondary_used_percent.unwrap_or(0.0))
                        .total_cmp(
                            &b.usage
                                .primary_used_percent
                                .unwrap_or(0.0)
                                .max(b.usage.secondary_used_percent.unwrap_or(0.0)),
                        )
                })
        })
        .map(|r| r.account_id.clone())
}
async fn status(m: &Manager, status: MonitorStatus) {
    let _ = write_json(&m.root.join("auto-switch-status.json"), &status);
    *STATUS.lock().await = status;
}
pub(super) fn validate_rotation(
    m: &Manager,
    source: &str,
    target: &str,
    force: bool,
) -> Result<()> {
    let settings = m.settings()?;
    anyhow::ensure!(
        force || settings.auto_switch_enabled,
        "Claude 자동 전환이 해제되어 전환을 취소했습니다."
    );
    let store = m.store()?;
    anyhow::ensure!(
        m.active_id(&store)?.as_deref() == Some(source),
        "현재 Claude 계정이 변경되어 자동 전환을 다시 확인합니다."
    );
    let now = Utc::now().timestamp();
    let receipts: Vec<_> = store
        .accounts
        .into_iter()
        .filter_map(|a| {
            let mut r = m.receipt(&a.id).ok().flatten()?;
            r.usage.plan_type = a.plan_type;
            Some(r)
        })
        .collect();
    let exhausted = receipts
        .iter()
        .find(|r| r.account_id == source)
        .filter(|r| eligible_usage(r, now))
        .is_some_and(|r| {
            r.usage
                .primary_used_percent
                .is_some_and(|v| v >= settings.auto_switch_threshold_percent)
                || r.usage
                    .secondary_used_percent
                    .is_some_and(|v| v >= settings.auto_switch_threshold_percent)
        });
    anyhow::ensure!(
        force || exhausted,
        "Claude 계정의 최신 할당량으로 자동 전환을 다시 확인합니다."
    );
    anyhow::ensure!(
        choose_candidate(
            &receipts,
            source,
            settings.auto_switch_threshold_percent,
            now,
            settings.auto_switch_plan_preference
        )
        .as_deref()
            == Some(target),
        "Claude 전환 후보의 사용량 또는 우선 등급이 변경되어 다시 확인합니다."
    );
    Ok(())
}
async fn rotate(m: &Manager, force: bool) -> Result<Value> {
    let _gate = MONITOR.lock().await;
    let settings = m.settings()?;
    let process_lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(m.root.join("auto-switch.lock"))?;
    if process_lock.try_lock().is_err() {
        return Ok(json!({"switched":false,"waiting":true}));
    }
    if !settings.auto_switch_enabled && !force {
        return Ok(json!({"switched":false}));
    }
    let store = m.store()?;
    let Some(active) = m.active_id(&store)? else {
        anyhow::bail!("현재 Claude 인증과 일치하는 저장 계정이 없습니다.");
    };
    let now = Utc::now().timestamp();
    let mut receipts = vec![];
    for a in &store.accounts {
        if let Some(mut r) = m.receipt(&a.id)? {
            r.usage.plan_type = a.plan_type.clone();
            receipts.push(r);
        }
    }
    let exhausted = receipts
        .iter()
        .find(|r| r.account_id == active)
        .filter(|r| eligible_usage(r, now))
        .is_some_and(|r| {
            r.usage
                .primary_used_percent
                .is_some_and(|p| p >= settings.auto_switch_threshold_percent)
                || r.usage
                    .secondary_used_percent
                    .is_some_and(|p| p >= settings.auto_switch_threshold_percent)
        });
    if !exhausted && !force {
        status(
            m,
            MonitorStatus {
                phase: "monitoring".into(),
                message: "Claude 계정 사용량을 백그라운드에서 확인 중".into(),
                ..Default::default()
            },
        )
        .await;
        return Ok(json!({"switched":false}));
    }
    let Some(target) = choose_candidate(
        &receipts,
        &active,
        settings.auto_switch_threshold_percent,
        now,
        settings.auto_switch_plan_preference,
    ) else {
        status(
            m,
            MonitorStatus {
                phase: "waiting".into(),
                message: "최근 5분 안에 여유 할당량이 확인된 다른 Claude 계정을 기다립니다.".into(),
                source_id: Some(active),
                ..Default::default()
            },
        )
        .await;
        return Ok(json!({"switched":false,"waiting":true}));
    };
    status(
        m,
        MonitorStatus {
            phase: "switching".into(),
            message: "Claude 계정 전환 중".into(),
            source_id: Some(active.clone()),
            target_id: Some(target.clone()),
            ..Default::default()
        },
    )
    .await;
    let result = runtime::activate_automatic(m, &target, &active, force).await?;
    status(
        m,
        MonitorStatus {
            phase: "applied".into(),
            message: "Claude 인증 파일 적용 완료 · 새 세션부터 사용".into(),
            source_id: Some(active),
            target_id: Some(target),
            switched_at: Some(Utc::now().to_rfc3339()),
        },
    )
    .await;
    Ok(json!({"switched":true,"result":result}))
}
async fn dispatch(action: &str, payload: Value) -> Result<Value> {
    let m = Manager::current()?;
    match action {
        "refresh" => {
            if let Some(id) = payload.get("accountId").and_then(Value::as_str) {
                quota::refresh_account(&m, id, true).await?;
            } else {
                quota::refresh_all(&m, true).await?;
            }
            snapshot_with_runtime(&m).await
        }
        "snapshot" => snapshot_with_runtime(&m).await,
        "import_current" => {
            runtime::auth_status(&m.live_dir).await?;
            let a = m.import_current(text(&payload, "name")?)?;
            if m.settings()?.capture_usage {
                runtime::install_bridge(&m, &a.id, false)?;
                runtime::install_bridge(&m, &a.id, true)?;
            }
            Ok(json!({"account_id":a.id}))
        }
        "import_credentials" => {
            let source: Value = serde_json::from_str(text(&payload, "contents")?)?;
            let credentials = source.get("credentials").unwrap_or(&source);
            let a = m.add(
                text(&payload, "name")?,
                credentials,
                source.get("config").unwrap_or(&json!({})),
                source.get("settings").unwrap_or(&json!({})),
            )?;
            if m.settings()?.capture_usage {
                runtime::install_bridge(&m, &a.id, false)?;
            }
            Ok(json!({"account_id":a.id}))
        }
        "start_login" => runtime::start_login(&m, text(&payload, "name")?.into()).await,
        "start_relogin" => runtime::start_relogin(&m, text(&payload, "accountId")?).await,
        "complete_login" => runtime::complete_login(&m).await,
        "cancel_login" => {
            runtime::cancel_login().await?;
            Ok(Value::Null)
        }
        "switch" => runtime::activate(&m, text(&payload, "accountId")?).await,
        "delete" => {
            let id = text(&payload, "accountId")?;
            let _gate = runtime::OPERATION.lock().await;
            if m.sync_active()?.as_deref() == Some(id) {
                runtime::remove_bridge(&m, id, true)?;
            }
            runtime::remove_bridge(&m, id, false)?;
            m.delete(id)?;
            Ok(Value::Null)
        }
        "rename" => {
            m.rename(text(&payload, "accountId")?, text(&payload, "newName")?)?;
            Ok(Value::Null)
        }
        "mask" => {
            m.masked(serde_json::from_value(payload["ids"].clone())?)?;
            Ok(Value::Null)
        }
        "settings" => {
            let settings: ClaudeSettings = serde_json::from_value(payload["settings"].clone())?;
            m.save_settings(&settings)?;
            super::remote_control::reconcile(&m)?;
            for account in m.store()?.accounts {
                if settings.capture_usage {
                    runtime::install_bridge(&m, &account.id, false)?;
                } else {
                    runtime::remove_bridge(&m, &account.id, false)?;
                }
            }
            if let Some(active) = m.sync_active()? {
                if settings.capture_usage {
                    runtime::install_bridge(&m, &active, true)?;
                } else {
                    runtime::remove_bridge(&m, &active, true)?;
                }
            }
            Ok(serde_json::to_value(settings)?)
        }
        "stats" => stats(&m, text(&payload, "accountId")?),
        "open" => {
            let id = text(&payload, "accountId")?;
            runtime::open(&m, id).await?;
            Ok(Value::Null)
        }
        "close" => {
            m.account(text(&payload, "accountId")?)?;
            Ok(json!({"closed":runtime::close(text(&payload,"accountId")?).await?}))
        }
        "warmup" => {
            let id = text(&payload, "accountId")?;
            runtime::warmup(&m, id).await?;
            monitor::record_manual_warmup(&m, id)?;
            let _ = quota::refresh_account(&m, id, false).await;
            Ok(Value::Null)
        }
        "warmup_all" => {
            let accounts = m.store()?.accounts;
            let mut failed = vec![];
            for a in &accounts {
                match runtime::warmup(&m, &a.id).await {
                    Ok(()) => {
                        monitor::record_manual_warmup(&m, &a.id)?;
                        let _ = quota::refresh_account(&m, &a.id, false).await;
                    }
                    Err(_) => failed.push(a.id.clone()),
                }
            }
            Ok(
                json!({"total_accounts":accounts.len(),"warmed_accounts":accounts.len()-failed.len(),"failed_account_ids":failed}),
            )
        }
        "auto_switch" => rotate(&m, true).await,
        "export_slim" => Ok(json!(format!(
            "claude1.{}",
            STANDARD.encode(serde_json::to_vec(&m.backup()?)?)
        ))),
        "import_slim" => {
            let body = text(&payload, "contents")?
                .strip_prefix("claude1.")
                .context("This is not a Claude account backup")?;
            let bytes = STANDARD.decode(body)?;
            let backup: ClaudeBackup = serde_json::from_slice(&bytes)?;
            let summary = m.import_backup(backup)?;
            for a in m.store()?.accounts {
                if m.settings()?.capture_usage {
                    runtime::install_bridge(&m, &a.id, false)?;
                }
            }
            Ok(serde_json::to_value(summary)?)
        }
        "export_encrypted" => {
            let bytes = crate::commands::account::encode_full_encrypted_store(
                &m.backup()?,
                text(&payload, "passphrase")?,
            )?;
            Ok(json!(STANDARD.encode(bytes)))
        }
        "import_encrypted" => {
            let bytes = STANDARD.decode(text(&payload, "contentsBase64")?)?;
            let backup: ClaudeBackup = crate::commands::account::decode_full_encrypted_payload(
                &bytes,
                text(&payload, "passphrase")?,
            )?;
            let summary = m.import_backup(backup)?;
            for a in m.store()?.accounts {
                if m.settings()?.capture_usage {
                    runtime::install_bridge(&m, &a.id, false)?;
                }
            }
            Ok(serde_json::to_value(summary)?)
        }
        "export_encrypted_file" => {
            let bytes = crate::commands::account::encode_full_encrypted_store(
                &m.backup()?,
                text(&payload, "passphrase")?,
            )?;
            std::fs::write(text(&payload, "path")?, bytes)?;
            Ok(Value::Null)
        }
        "import_encrypted_file" => {
            let path = text(&payload, "path")?;
            anyhow::ensure!(
                std::fs::metadata(path)?.len() <= 8 * 1024 * 1024,
                "Backup file is too large"
            );
            let bytes = std::fs::read(path)?;
            let backup: ClaudeBackup = crate::commands::account::decode_full_encrypted_payload(
                &bytes,
                text(&payload, "passphrase")?,
            )?;
            let summary = m.import_backup(backup)?;
            for a in m.store()?.accounts {
                if m.settings()?.capture_usage {
                    runtime::install_bridge(&m, &a.id, false)?;
                }
            }
            Ok(serde_json::to_value(summary)?)
        }
        _ => anyhow::bail!("Unsupported Claude action: {action}"),
    }
}
#[tauri::command]
pub async fn claude_command(
    action: String,
    payload: Option<Value>,
) -> std::result::Result<Value, String> {
    let payload = payload.unwrap_or_else(|| json!({}));
    if serde_json::to_vec(&payload)
        .map_err(|e| e.to_string())?
        .len()
        > 8 * 1024 * 1024
    {
        return Err("Claude request is too large".into());
    }
    dispatch(&action, payload)
        .await
        .map_err(|e| format!("{e:#}"))
}
pub fn start_monitor() {
    if MONITOR_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    crate::prompt_cache::start_live_session_detection();
    std::thread::spawn(|| {
        if let Ok(manager) = Manager::current() {
            let result = super::remote_control::reconcile(&manager);
            let _ = write_json(
                &manager.root.join("remote-control-startup-policy.json"),
                &json!({"checked_at":Utc::now().timestamp(),"applied":result.is_ok(),
                    "existing_session_reconnect":"official_rc_on_verified_idle_windows_console",
                    "error":result.err().map(|e|e.to_string())}),
            );
        }
    });
    super::remote_control_live::start();
    monitor::start_warmup_monitor();
    tauri::async_runtime::spawn(async {
        loop {
            if let Ok(m) = Manager::current() {
                // This task belongs to the backend, including tray-only and web
                // hosts. Warmups have a separate task and never stop polling.
                if let Err(error) = quota::refresh_all(&m, false).await {
                    status(
                        &m,
                        MonitorStatus {
                            phase: "waiting".into(),
                            message: error.to_string(),
                            ..Default::default()
                        },
                    )
                    .await;
                }
                if let Ok(settings) = m.settings() {
                    if settings.auto_switch_enabled {
                        if let Err(e) = rotate(&m, false).await {
                            status(
                                &m,
                                MonitorStatus {
                                    phase: "waiting".into(),
                                    message: e.to_string(),
                                    ..Default::default()
                                },
                            )
                            .await;
                        }
                    } else {
                        status(
                            &m,
                            MonitorStatus {
                                phase: "disabled".into(),
                                message: "Claude 자동 전환 꺼짐".into(),
                                ..Default::default()
                            },
                        )
                        .await;
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn inventory_usage_isolated_from_corrupt_foreign_and_future_account_receipts() {
        let root = std::env::temp_dir().join(format!("card-usage-{}", uuid::Uuid::new_v4()));
        let m = Manager {
            root: root.join("claude"),
            live_dir: root.join("live"),
            live_config: root.join("live/.claude.json"),
        };
        let a = ClaudeAccount {
            id: uuid::Uuid::new_v4().to_string(),
            name: "synthetic".into(),
            email: None,
            account_uuid: None,
            plan_type: None,
            created_at: "offline".into(),
            last_used_at: None,
        };
        let p = m.profile(&a.id).unwrap();
        std::fs::create_dir_all(&p).unwrap();
        let now = Utc::now().timestamp();
        let good = receipt(&a.id, 42.0, Some(21.0), now);
        let backup = serde_json::to_vec(&good).unwrap();
        std::fs::write(p.join("usage.json.bak"), &backup).unwrap();
        let mut foreign = good.clone();
        foreign.usage.account_id = uuid::Uuid::new_v4().to_string();
        let mut future = good.clone();
        future.observed_at = now + 3600;
        for bytes in [
            b"{broken".to_vec(),
            serde_json::to_vec(&foreign).unwrap(),
            serde_json::to_vec(&future).unwrap(),
        ] {
            std::fs::write(p.join("usage.json"), &bytes).unwrap();
            let (usage, at) = account_usage(&m, &a);
            assert!(usage.error.is_some());
            assert_eq!(at, None);
            assert_eq!(usage.primary_used_percent, None);
            assert_eq!(std::fs::read(p.join("usage.json")).unwrap(), bytes);
        }
        std::fs::write(p.join("usage.json"), &backup).unwrap();
        let (usage, at) = account_usage(&m, &a);
        assert!(usage.error.is_none());
        assert_eq!(at, Some(now));
        assert_eq!(usage.primary_used_percent, Some(42.0));
        assert_eq!(std::fs::read(p.join("usage.json.bak")).unwrap(), backup);
        std::fs::remove_dir_all(root).unwrap();
    }
    fn receipt(id: &str, p: f64, weekly: Option<f64>, at: i64) -> UsageReceipt {
        UsageReceipt {
            account_id: id.into(),
            observed_at: at,
            usage: runtime::usage_from_statusline(
                id,
                None,
                &json!({"rate_limits":{"five_hour":{"used_percentage":p,"resets_at":5000},"seven_day":{"used_percentage":weekly,"resets_at":10000}}}),
            ),
            sessions: Default::default(),
        }
    }
    #[test]
    fn rotation_excludes_exhausted_stale_and_current_accounts() {
        let receipts = vec![
            receipt("active", 100.0, Some(10.0), 1000),
            receipt("stale", 0.0, Some(0.0), 0),
            receipt("weekly-exhausted", 5.0, Some(100.0), 1000),
            receipt("healthy", 12.0, Some(64.0), 1000),
        ];
        assert_eq!(
            choose_candidate(&receipts, "active", 99.5, 1001, PlanPreference::Any),
            Some("healthy".into())
        );
        assert_eq!(
            choose_candidate(&receipts, "healthy", 99.5, 1001, PlanPreference::Any),
            None
        );
    }
    #[test]
    fn rotation_requires_both_windows_but_accepts_observed_idle_windows() {
        let mut alternate = receipt("alternate", 0.0, Some(0.0), 1000);
        alternate.usage.primary_resets_at = None;
        alternate.usage.secondary_resets_at = None;
        assert_eq!(
            choose_candidate(
                &[alternate.clone()],
                "active",
                99.5,
                1001,
                PlanPreference::Any
            ),
            Some("alternate".into())
        );
        alternate.usage.secondary_used_percent = None;
        assert_eq!(
            choose_candidate(
                &[alternate.clone()],
                "active",
                99.5,
                1001,
                PlanPreference::Any
            ),
            None
        );
        alternate.usage.secondary_used_percent = Some(100.0);
        alternate.usage.secondary_resets_at = Some(5000);
        assert_eq!(
            choose_candidate(&[alternate], "active", 99.5, 1001, PlanPreference::Any),
            None
        );
    }
    #[test]
    fn encrypted_claude_backup_cannot_be_imported_as_codex() {
        let backup = ClaudeBackup {
            provider: "claude".into(),
            version: 1,
            accounts: vec![],
            settings: ClaudeSettings::default(),
        };
        let bytes = crate::commands::account::encode_full_encrypted_store(
            &backup,
            "a-strong-test-password",
        )
        .unwrap();
        let restored: ClaudeBackup = crate::commands::account::decode_full_encrypted_payload(
            &bytes,
            "a-strong-test-password",
        )
        .unwrap();
        assert_eq!(restored.provider, "claude");
        let codex =
            crate::commands::account::decode_full_encrypted_store(&bytes, "a-strong-test-password");
        // Codex's strict account records reject Claude payloads; the provider tag remains mandatory on the Claude side.
        assert!(codex.is_err());
    }
    #[test]
    fn claude_plan_preference_yields_to_exhaustion_and_stale_observations() {
        let mut max = receipt("max", 75.0, Some(65.0), 1000);
        max.usage.plan_type = Some("max_20x".into());
        let mut pro = receipt("pro", 1.0, Some(5.0), 1000);
        pro.usage.plan_type = Some("pro".into());
        assert_eq!(
            choose_candidate(
                &[pro.clone(), max.clone()],
                "active",
                99.5,
                1001,
                PlanPreference::Max
            ),
            Some("max".into())
        );
        max.usage.secondary_used_percent = Some(100.0);
        assert_eq!(
            choose_candidate(
                &[max.clone(), pro.clone()],
                "active",
                99.5,
                1001,
                PlanPreference::Max
            ),
            Some("pro".into())
        );
        max.usage.secondary_used_percent = Some(10.0);
        max.observed_at = 0;
        assert_eq!(
            choose_candidate(&[max, pro], "active", 99.5, 1001, PlanPreference::Max),
            Some("pro".into())
        );
        let old: ClaudeSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(old.auto_switch_plan_preference, PlanPreference::Any);
    }
}
