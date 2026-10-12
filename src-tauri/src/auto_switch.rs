//! Automatic rotation policy shared by background and UI usage observations.
use std::{
    collections::HashMap,
    future::Future,
    time::{Duration, Instant},
};

use crate::{
    commands::account::AutoSwitchOutcome,
    types::{AuthData, PlanPreference, StoredAccount, UsageInfo},
};
use futures::{stream::FuturesUnordered, StreamExt};

pub const USAGE_MAX_AGE: Duration = Duration::from_secs(90);
pub const RETRY_INTERVAL: Duration = Duration::from_secs(30);
pub const AUTH_BUSY_RETRY: &str = "다른 인증 작업이 진행 중이어서 잠시 후 다시 확인합니다";

pub fn quota_score(usage: &UsageInfo) -> Result<f64, String> {
    if let Some(error) = &usage.error {
        return Err(error.clone());
    }
    let windows: Vec<_> = [usage.primary_used_percent, usage.secondary_used_percent]
        .into_iter()
        .flatten()
        .collect();
    if windows.is_empty()
        || windows
            .iter()
            .any(|p| !p.is_finite() || !(0.0..=100.0).contains(p))
    {
        return Err("사용량 한도를 확인할 수 없습니다".into());
    }
    Ok(windows.into_iter().fold(0.0, f64::max))
}

#[derive(Debug)]
pub enum RotationResult {
    Healthy,
    Deferred(String),
    NoCandidate(String),
    Switched(AutoSwitchOutcome),
}

pub(crate) fn defer_auth_busy(
    result: Result<RotationResult, String>,
) -> Result<RotationResult, String> {
    match result {
        Err(error) if error == AUTH_BUSY_RETRY => Ok(RotationResult::Deferred(error)),
        other => other,
    }
}

/// The supplied active observation is authoritative for this check. Do not make
/// a second network request that can discard an already observed exhausted quota.
pub async fn evaluate_rotation<F, FF, S, SF>(
    active: &StoredAccount,
    accounts: &[StoredAccount],
    active_usage: &UsageInfo,
    threshold: f64,
    fetch: F,
    activate: S,
) -> Result<RotationResult, String>
where
    F: FnMut(StoredAccount) -> FF,
    FF: Future<Output = Result<UsageInfo, String>>,
    S: FnOnce(String, StoredAccount) -> SF,
    SF: Future<Output = Result<AutoSwitchOutcome, String>>,
{
    evaluate_rotation_with_preference(
        active,
        accounts,
        active_usage,
        threshold,
        PlanPreference::Any,
        fetch,
        activate,
    )
    .await
}

pub async fn evaluate_rotation_with_preference<F, FF, S, SF>(
    active: &StoredAccount,
    accounts: &[StoredAccount],
    active_usage: &UsageInfo,
    threshold: f64,
    preference: PlanPreference,
    mut fetch: F,
    activate: S,
) -> Result<RotationResult, String>
where
    F: FnMut(StoredAccount) -> FF,
    FF: Future<Output = Result<UsageInfo, String>>,
    S: FnOnce(String, StoredAccount) -> SF,
    SF: Future<Output = Result<AutoSwitchOutcome, String>>,
{
    if active_usage.account_id != active.id {
        return Err("사용량을 조회한 계정과 현재 계정이 다릅니다".into());
    }
    if !threshold.is_finite() || !(50.0..=99.0).contains(&threshold) {
        return Err("자동 전환 임계값이 올바르지 않습니다".into());
    }
    if quota_score(active_usage)? < threshold {
        return Ok(RotationResult::Healthy);
    }
    let mut unavailable = 0;
    let mut exhausted = 0;
    let mut last_error = None;
    let mut pending = FuturesUnordered::new();
    let mut preferred_pending = 0;
    let mut fallback: Option<(StoredAccount, f64)> = None;
    for candidate in accounts
        .iter()
        .filter(|a| a.id != active.id && matches!(a.auth_data, AuthData::ChatGPT { .. }))
    {
        let query = fetch(candidate.clone());
        let candidate = candidate.clone();
        let preferred = preference.matches(candidate.plan_type.as_deref());
        preferred_pending += usize::from(preferred);
        pending.push(async move {
            let result = tokio::time::timeout(Duration::from_secs(15), query)
                .await
                .map_err(|_| "후보 계정 사용량 조회 시간 초과".to_string())
                .and_then(|result| result);
            (candidate, preferred, result)
        });
    }
    // Query in parallel. Hold a ready fallback only while preferred accounts
    // are being checked (bounded by the same 15-second timeout).
    while let Some((mut candidate, was_preferred, result)) = pending.next().await {
        preferred_pending -= usize::from(was_preferred);
        let result = result.and_then(|usage| {
            if usage.account_id != candidate.id {
                return Err("후보 계정의 사용량이 일치하지 않습니다".into());
            }
            let score = quota_score(&usage)?;
            if usage.plan_type.is_some() {
                candidate.plan_type = usage.plan_type;
            }
            Ok(score)
        });
        match result {
            Ok(score) if score < threshold => {
                if preference.matches(candidate.plan_type.as_deref()) {
                    drop(pending);
                    return activate(active.id.clone(), candidate)
                        .await
                        .map(RotationResult::Switched);
                }
                if fallback.as_ref().is_none_or(|(_, best)| score < *best) {
                    fallback = Some((candidate, score));
                }
            }
            Ok(_) => exhausted += 1,
            Err(error) => {
                unavailable += 1;
                last_error = Some(error);
            }
        }
        if preferred_pending == 0 {
            if let Some((candidate, _)) = fallback.take() {
                drop(pending);
                return activate(active.id.clone(), candidate)
                    .await
                    .map(RotationResult::Switched);
            }
        }
    }
    let detail = last_error
        .map(|e| format!(" · 조회 오류: {e}"))
        .unwrap_or_default();
    Ok(RotationResult::NoCandidate(format!(
            "전환 가능한 계정 없음 · 한도 소진 {exhausted}개 · 사용량 확인 실패 {unavailable}개{detail} · 30초 후 재확인"
    )))
}

/// Private, durable stage receipts; account IDs and percentages only, no tokens.
pub(crate) fn record_event(
    stage: &str,
    active: Option<&str>,
    target: Option<&str>,
    elapsed: Option<Duration>,
    usage: Option<&UsageInfo>,
) {
    use std::io::Write;
    let write = || -> anyhow::Result<()> {
        let dir = crate::auth::get_config_dir()?;
        let _guard = crate::auth::persistence::lock_file(&dir.join("auto-events.lock"))?;
        let now = chrono::Utc::now();
        let path = dir.join(format!(
            "auto-switch-events-{}.jsonl",
            now.format("%Y-%m-%d")
        ));
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        let event = serde_json::json!({"at":now,"stage":stage,"active":active,"target":target,"process_id":std::process::id(),
            "elapsed_ms":elapsed.map(|d|d.as_millis()),
            "primary":usage.and_then(|u|u.primary_used_percent),
            "secondary":usage.and_then(|u|u.secondary_used_percent),
            "usage_error":usage.map(|u|u.error.is_some())});
        serde_json::to_writer(&mut file, &event)?;
        file.write_all(b"\n")?;
        file.sync_data()?;
        Ok(())
    };
    if let Err(error) = write() {
        eprintln!("Could not save automatic switch event: {error}");
    }
}

#[derive(Default)]
pub struct UsageMonitor {
    cache: HashMap<String, (UsageInfo, Instant)>,
    last_attempt: Option<(String, Instant)>,
}

impl UsageMonitor {
    pub fn observe(&mut self, usage: UsageInfo, now: Instant) {
        self.cache.insert(usage.account_id.clone(), (usage, now));
    }

    pub fn fresh_usage(&self, id: &str, now: Instant) -> Option<UsageInfo> {
        self.cache
            .get(id)
            .filter(|(_, at)| now.saturating_duration_since(*at) <= USAGE_MAX_AGE)
            .map(|(usage, _)| usage.clone())
    }

    pub fn fresh_candidates(&self, now: Instant) -> HashMap<String, UsageInfo> {
        self.cache
            .keys()
            .filter_map(|id| self.fresh_usage(id, now).map(|u| (id.clone(), u)))
            .collect()
    }

    pub fn start_attempt(&mut self, active_id: &str, now: Instant) -> bool {
        if self.last_attempt.as_ref().is_some_and(|(id, at)| {
            id == active_id && now.saturating_duration_since(*at) < RETRY_INTERVAL
        }) {
            return false;
        }
        self.last_attempt = Some((active_id.into(), now));
        true
    }
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct AutoSwitchStatus {
    pub checked_at: Option<chrono::DateTime<chrono::Utc>>,
    pub phase: String,
    pub message: String,
    pub active_account_id: Option<String>,
    pub primary_used_percent: Option<f64>,
    pub secondary_used_percent: Option<f64>,
    pub last_attempt_at: Option<chrono::DateTime<chrono::Utc>>,
    pub last_attempt_result: Option<String>,
    pub last_attempt_message: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, future::ready};

    fn account(name: &str) -> StoredAccount {
        StoredAccount::new_chatgpt(
            name.into(),
            None,
            None,
            None,
            "id".into(),
            "access".into(),
            "refresh".into(),
            None,
        )
    }
    fn usage(account: &StoredAccount, session: Option<f64>, weekly: Option<f64>) -> UsageInfo {
        let mut u = UsageInfo::error(account.id.clone(), String::new());
        u.error = None;
        u.primary_used_percent = session;
        u.secondary_used_percent = weekly;
        u
    }

    #[tokio::test]
    async fn ready_pro_switches_while_another_candidate_is_stalled() {
        let plus = account("Exhausted Plus");
        let stalled = account("Stalled Plus");
        let pro = account("Weekly-only Pro");
        let pro_id = pro.id.clone();
        let result = tokio::time::timeout(
            Duration::from_millis(200),
            evaluate_rotation(
                &plus,
                &[stalled, pro],
                &usage(&plus, Some(100.0), Some(79.0)),
                98.0,
                |candidate| {
                    let is_pro = candidate.id == pro_id;
                    async move {
                        if !is_pro {
                            std::future::pending::<()>().await;
                        }
                        Ok(usage(&candidate, None, Some(53.0)))
                    }
                },
                |_, target| async move {
                    Ok(AutoSwitchOutcome {
                        from: "Plus".into(),
                        to: target.name,
                        desktop_status: "mock_reopened".into(),
                        warning: None,
                    })
                },
            ),
        )
        .await
        .expect("a stalled candidate must not prevent available Pro rotation")
        .unwrap();
        assert!(
            matches!(result, RotationResult::Switched(outcome) if outcome.to == "Weekly-only Pro")
        );
    }

    #[tokio::test]
    async fn exhausted_plus_rotates_to_weekly_only_pro_using_the_observed_usage() {
        let plus = account("Plus");
        let other_plus = account("Other Plus");
        let pro = account("Prolite");
        let switched = Cell::new(false);
        let observed = usage(&plus, Some(100.0), Some(52.0));
        let result = evaluate_rotation(
            &plus,
            &[plus.clone(), other_plus.clone(), pro.clone()],
            &observed,
            98.0,
            |candidate| {
                assert_ne!(
                    candidate.id, plus.id,
                    "active quota must not be fetched a second time"
                );
                ready(Ok(if candidate.id == pro.id {
                    usage(&pro, None, Some(17.0))
                } else {
                    usage(&other_plus, Some(100.0), Some(63.0))
                }))
            },
            |source, target| {
                assert_eq!(source, plus.id);
                assert_eq!(target.id, pro.id);
                switched.set(true);
                ready(Ok(AutoSwitchOutcome {
                    from: plus.name.clone(),
                    to: target.name,
                    desktop_status: "mock_reopened".into(),
                    warning: None,
                }))
            },
        )
        .await
        .unwrap();
        assert!(switched.get());
        assert!(matches!(result, RotationResult::Switched(_)));
    }

    #[tokio::test]
    async fn weekly_exhaustion_also_rotates_but_unknown_or_failed_candidates_do_not() {
        let active = account("Active");
        let unknown = account("Unknown");
        let failed = account("Failed");
        let result = evaluate_rotation(
            &active,
            &[unknown.clone(), failed.clone()],
            &usage(&active, Some(20.0), Some(100.0)),
            98.0,
            |candidate| {
                ready(if candidate.id == failed.id {
                    Err("HTTP 403".into())
                } else {
                    Ok(usage(&unknown, None, None))
                })
            },
            |_, _| async { panic!("unverified candidates must not be activated") },
        )
        .await
        .unwrap();
        assert!(
            matches!(result, RotationResult::NoCandidate(reason) if reason.contains("확인 실패 2개"))
        );
    }

    #[tokio::test]
    async fn healthy_manual_pro_switch_does_not_trigger_or_fetch_candidates() {
        let pro = account("Prolite");
        let result = evaluate_rotation(
            &pro,
            &[account("Plus")],
            &usage(&pro, None, Some(17.0)),
            98.0,
            |_| async { panic!("healthy active quota must not fetch candidates") },
            |_, _| async { panic!("healthy active quota must not switch") },
        )
        .await
        .unwrap();
        assert!(matches!(result, RotationResult::Healthy));
    }

    #[tokio::test]
    async fn stale_identity_and_activation_failures_are_reported() {
        let active = account("Active");
        let pro = account("Pro");
        assert!(evaluate_rotation(
            &active,
            &[],
            &usage(&pro, None, Some(17.0)),
            98.0,
            |_| ready(Err("unused".into())),
            |_, _| ready(Err("unused".into()))
        )
        .await
        .is_err());
        let error = evaluate_rotation(
            &active,
            &[pro.clone()],
            &usage(&active, Some(100.0), None),
            98.0,
            |_| ready(Ok(usage(&pro, None, Some(17.0)))),
            |_, _| ready(Err("desktop close failed".into())),
        )
        .await
        .unwrap_err();
        assert_eq!(error, "desktop close failed");
    }

    #[test]
    fn retries_are_per_account_and_observations_expire() {
        let mut monitor = UsageMonitor::default();
        let a = account("A");
        let now = Instant::now();
        monitor.observe(usage(&a, Some(100.0), None), now);
        assert!(monitor.fresh_usage(&a.id, now).is_some());
        assert!(monitor
            .fresh_usage(&a.id, now + USAGE_MAX_AGE + Duration::from_secs(1))
            .is_none());
        assert!(monitor.start_attempt(&a.id, now));
        assert!(!monitor.start_attempt(&a.id, now + Duration::from_secs(29)));
        assert!(monitor.start_attempt("new-active", now + Duration::from_secs(29)));
        assert!(monitor.start_attempt(&a.id, now + Duration::from_secs(30)));
    }

    #[test]
    fn authentication_queue_busy_is_retryable_but_real_failures_are_preserved() {
        assert!(matches!(
            defer_auth_busy(Err(AUTH_BUSY_RETRY.into())).unwrap(),
            RotationResult::Deferred(_)
        ));
        assert_eq!(
            defer_auth_busy(Err("credential write failed".into())).unwrap_err(),
            "credential write failed"
        );
        assert!(matches!(
            defer_auth_busy(Ok(RotationResult::Healthy)).unwrap(),
            RotationResult::Healthy
        ));
    }

    #[test]
    fn errors_and_invalid_percentages_never_count_as_available_quota() {
        let a = account("A");
        for value in [f64::NAN, f64::INFINITY, -1.0, 101.0] {
            assert!(quota_score(&usage(&a, Some(value), None)).is_err());
        }
        let mut u = usage(&a, Some(0.0), Some(0.0));
        u.error = Some("HTTP 401".into());
        assert!(quota_score(&u).is_err());
    }

    #[tokio::test]
    async fn preferred_plan_wins_even_when_another_grade_is_ready_first() {
        let active = account("Active");
        let mut plus = account("Ready Plus");
        plus.plan_type = Some("plus".into());
        let mut pro = account("Preferred Prolite");
        pro.plan_type = Some("Prolite".into());
        let result = evaluate_rotation_with_preference(
            &active,
            &[plus, pro],
            &usage(&active, Some(100.0), None),
            98.0,
            PlanPreference::Pro,
            |candidate| async move {
                if candidate.plan_type.as_deref() == Some("Prolite") {
                    tokio::time::sleep(Duration::from_millis(30)).await;
                }
                Ok(usage(&candidate, Some(20.0), Some(30.0)))
            },
            |_, target| async move {
                Ok(AutoSwitchOutcome {
                    from: "Active".into(),
                    to: target.name,
                    desktop_status: "fixture".into(),
                    warning: None,
                })
            },
        )
        .await
        .unwrap();
        assert!(
            matches!(result, RotationResult::Switched(outcome) if outcome.to == "Preferred Prolite")
        );
    }

    #[tokio::test]
    async fn preferred_exhausted_or_failed_accounts_fall_back_without_selecting_unknown_quota() {
        for failure in [false, true] {
            let active = account("Active");
            let mut pro = account("Pro");
            pro.plan_type = Some("pro".into());
            let mut plus = account("Plus fallback");
            plus.plan_type = Some("plus".into());
            let result = evaluate_rotation_with_preference(
                &active,
                &[pro, plus],
                &usage(&active, None, Some(100.0)),
                98.0,
                PlanPreference::Pro,
                |candidate| async move {
                    if candidate.plan_type.as_deref() == Some("pro") {
                        if failure {
                            Err("HTTP 401".into())
                        } else {
                            Ok(usage(&candidate, Some(100.0), Some(12.0)))
                        }
                    } else {
                        Ok(usage(&candidate, None, Some(25.0)))
                    }
                },
                |_, target| async move {
                    Ok(AutoSwitchOutcome {
                        from: "Active".into(),
                        to: target.name,
                        desktop_status: "fixture".into(),
                        warning: None,
                    })
                },
            )
            .await
            .unwrap();
            assert!(
                matches!(result, RotationResult::Switched(outcome) if outcome.to == "Plus fallback")
            );
        }
    }

    #[test]
    fn plan_aliases_and_existing_settings_keep_their_meaning() {
        assert!(PlanPreference::Pro.matches(Some("Prolite")));
        assert!(PlanPreference::Max.matches(Some("max_20x")));
        assert!(PlanPreference::Team.matches(Some("business")));
        assert!(!PlanPreference::Pro.matches(Some("plus")));
        assert!(!PlanPreference::Pro.matches(None));
        let old: crate::types::AppSettings = serde_json::from_str(
            r#"{"auto_switch_enabled":false,"auto_switch_threshold_percent":95}"#,
        )
        .unwrap();
        assert_eq!(old.auto_switch_plan_preference, PlanPreference::Any);
        assert!(!old.auto_switch_enabled);
        assert_eq!(old.auto_switch_threshold_percent, 95.0);
        assert!(serde_json::from_str::<crate::types::AppSettings>(
            r#"{"auto_switch_plan_preference":"typo"}"#
        )
        .is_err());
    }
}
