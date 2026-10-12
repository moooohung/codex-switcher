//! Durable automatic warmups, independently of quota polling and rotation.
//!
//! Parent integration: start `start_warmup_monitor()` once; call
//! `remember_usage()` before replacing a quota receipt, and call
//! `record_manual_warmup()` only AFTER a successful manual runtime warmup.
//! Neither receipt history nor this module invents a reset for an idle window.
use super::{runtime, storage::Manager, ClaudeSettings, UsageReceipt};
use crate::auth::persistence::lock_file;
use anyhow::{ensure, Context, Result};
use chrono::{Local, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions},
    future::Future,
    path::Path,
    pin::Pin,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

const STATE_FILE: &str = "warmup-monitor.json";
const STATE_LOCK: &str = "warmup-monitor-state.lock";
// Deliberately distinct from runtime's warmup.lock. Never take that lock here.
const EXECUTION_LOCK: &str = "warmup-monitor-execution.lock";
const WEEKLY_LIMIT: f64 = 99.5;
const RECEIPT_MAX_AGE: i64 = 300;
const LEASE_SECONDS: i64 = 300;
static STARTED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Reason {
    PrimaryReset { reset: i64 },
    WeeklyReset { reset: i64 },
    IdlePrimary { generation: u64 },
    Timed { day: String, time: String },
}
impl Reason {
    fn key(&self) -> String {
        match self {
            Self::PrimaryReset { reset } => format!("primary:{reset}"),
            Self::WeeklyReset { reset } => format!("weekly:{reset}"),
            Self::IdlePrimary { generation } => format!("idle:{generation}"),
            Self::Timed { day, time } => format!("timed:{day}:{time}"),
        }
    }
    fn enabled(&self, settings: &ClaudeSettings, id: &str, slot: &LocalSlot) -> bool {
        match self {
            Self::Timed { day, time } => {
                day == &slot.day && settings.timed_warmup_times.contains(time)
            }
            _ => settings.warmup_all || settings.warmup_account_ids.iter().any(|v| v == id),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Lease {
    token: String,
    claimed_at: i64,
    expires_at: i64,
    // A reserved lease can safely be retried after a crash; an executing one
    // cannot, because the model response and a local disk commit aren't atomic.
    started_at: Option<i64>,
    reasons: BTreeMap<String, Reason>,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct AccountLedger {
    primary_windows: BTreeSet<i64>,
    weekly_windows: BTreeSet<i64>,
    last_observed_at: Option<i64>,
    current_primary_reset: Option<i64>,
    last_checked_at: Option<i64>,
    primary_window_seconds: Option<i64>,
    weekly_window_seconds: Option<i64>,
    primary_idle: bool,
    idle_generation: u64,
    queued: BTreeMap<String, Reason>,
    successes: BTreeMap<String, i64>,
    // An interrupted call stays visibly unresolved, not falsely successful or
    // automatically repeated after restart. A later manual success can settle it.
    uncertain: BTreeMap<String, Reason>,
    lease: Option<Lease>,
    failures: u32,
    next_retry_at: i64,
    last_attempt_at: Option<i64>,
    last_success_at: Option<i64>,
    last_error: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Ledger {
    version: u32,
    #[serde(default)]
    last_checked_at: Option<i64>,
    accounts: BTreeMap<String, AccountLedger>,
}
impl Default for Ledger {
    fn default() -> Self {
        Self {
            version: 1,
            last_checked_at: None,
            accounts: BTreeMap::new(),
        }
    }
}

#[derive(Clone)]
struct LocalSlot {
    day: String,
    time: String,
}
impl LocalSlot {
    fn now() -> Self {
        let now = Local::now();
        Self {
            day: now.format("%Y-%m-%d").to_string(),
            time: now.format("%H:%M").to_string(),
        }
    }
}

struct Snapshot {
    settings: ClaudeSettings,
    receipts: BTreeMap<String, Option<UsageReceipt>>,
}
impl Snapshot {
    fn read(m: &Manager) -> Result<Self> {
        let settings = m.settings()?;
        let receipts = m
            .store()?
            .accounts
            .into_iter()
            .map(|a| {
                // One unreadable account must not stall all other accounts.
                let receipt = m.receipt(&a.id).ok().flatten();
                (a.id, receipt)
            })
            .collect();
        Ok(Self { settings, receipts })
    }
}

fn valid_percent(p: Option<f64>) -> bool {
    p.is_some_and(|v| v.is_finite() && (0.0..=100.0).contains(&v))
}

fn safe_usage(r: &UsageReceipt, id: &str, now: i64) -> bool {
    if r.account_id != id
        || r.usage.account_id != id
        || r.usage.error.is_some()
        || r.observed_at > now.saturating_add(5)
        || now.saturating_sub(r.observed_at) > RECEIPT_MAX_AGE
        || !valid_percent(r.usage.primary_used_percent)
        || !valid_percent(r.usage.secondary_used_percent)
        || r.usage.secondary_used_percent.unwrap_or(100.0) >= WEEKLY_LIMIT
    {
        return false;
    }
    // null is legitimate only for an explicitly observed 0%, unstarted window.
    let known = |p: Option<f64>, reset: Option<i64>| match reset {
        Some(reset) => reset > 0,
        None => p == Some(0.0),
    };
    known(r.usage.primary_used_percent, r.usage.primary_resets_at)
        && known(r.usage.secondary_used_percent, r.usage.secondary_resets_at)
        // Do not replay a missed reset into a current window already in use.
        && (r.usage.primary_used_percent == Some(0.0)
            || r.usage.primary_resets_at.is_some_and(|reset| reset <= now))
}

impl AccountLedger {
    fn observe(&mut self, r: &UsageReceipt, now: i64) {
        if r.account_id != r.usage.account_id
            || r.usage.error.is_some()
            || r.observed_at > now.saturating_add(5)
        {
            return;
        }
        // Keep BOTH old and new resets; older observations can add history but
        // cannot roll the current idle generation backwards.
        for (p, reset, windows) in [
            (
                r.usage.primary_used_percent,
                r.usage.primary_resets_at,
                &mut self.primary_windows,
            ),
            (
                r.usage.secondary_used_percent,
                r.usage.secondary_resets_at,
                &mut self.weekly_windows,
            ),
        ] {
            if valid_percent(p) {
                if let Some(reset) = reset.filter(|v| *v > 0) {
                    windows.insert(reset);
                }
            }
        }
        if self.last_observed_at.is_some_and(|at| r.observed_at < at)
            || !valid_percent(r.usage.primary_used_percent)
        {
            return;
        }
        let seconds = |minutes: Option<i64>| {
            minutes
                .filter(|minutes| (1..=43200).contains(minutes))
                .map(|minutes| minutes * 60)
        };
        if let Some(seconds) = seconds(r.usage.primary_window_minutes) {
            self.primary_window_seconds = Some(seconds);
        }
        if let Some(seconds) = seconds(r.usage.secondary_window_minutes) {
            self.weekly_window_seconds = Some(seconds);
        }
        let idle = r.usage.primary_used_percent == Some(0.0) && r.usage.primary_resets_at.is_none();
        // A delayed idle observation from before a known window's end cannot
        // create another bootstrap generation (no per-second disk watermark).
        if idle
            && self
                .current_primary_reset
                .is_some_and(|reset| r.observed_at < reset)
        {
            return;
        }
        if idle && !self.primary_idle {
            self.idle_generation = self.idle_generation.saturating_add(1);
        }
        // Missing/nonzero-with-null reset is unknown, not a new idle generation.
        if (idle || r.usage.primary_resets_at.is_some_and(|reset| reset > 0))
            && (self.last_observed_at.is_none()
                || self.primary_idle != idle
                || self.current_primary_reset != r.usage.primary_resets_at)
        {
            self.primary_idle = idle;
            self.current_primary_reset = r.usage.primary_resets_at;
            self.last_observed_at = Some(r.observed_at);
        }
    }

    fn due(&self, now: i64) -> Vec<Reason> {
        let mut reasons = Vec::new();
        if let Some(reset) = self.primary_windows.range(..=now).next_back() {
            reasons.push(Reason::PrimaryReset { reset: *reset });
        }
        if let Some(reset) = self.weekly_windows.range(..=now).next_back() {
            reasons.push(Reason::WeeklyReset { reset: *reset });
        }
        if self.primary_idle && self.idle_generation > 0 {
            reasons.push(Reason::IdlePrimary {
                generation: self.idle_generation,
            });
        }
        reasons
            .into_iter()
            .filter(|r| self.relevant(r, now))
            .collect()
    }

    fn relevant(&self, reason: &Reason, now: i64) -> bool {
        match reason {
            Reason::PrimaryReset { reset } => {
                now.saturating_sub(*reset) <= self.primary_window_seconds.unwrap_or(18000)
                    && self.primary_windows.range(..=now).next_back() == Some(reset)
            }
            Reason::WeeklyReset { reset } => {
                now.saturating_sub(*reset) <= self.weekly_window_seconds.unwrap_or(604800)
                    && self.weekly_windows.range(..=now).next_back() == Some(reset)
            }
            Reason::IdlePrimary { generation } => {
                self.primary_idle && *generation == self.idle_generation
            }
            Reason::Timed { .. } => true,
        }
    }

    fn enqueue(&mut self, reason: Reason) {
        let key = reason.key();
        if !self.successes.contains_key(&key)
            && !self.uncertain.contains_key(&key)
            && !self
                .lease
                .as_ref()
                .is_some_and(|lease| lease.reasons.contains_key(&key))
        {
            self.queued.insert(key, reason);
        }
    }

    fn recover_orphan(&mut self) {
        // Only called while the OS execution lock is owned. Expiry alone is NOT
        // permission to steal a lease from an execution still holding the lock.
        if let Some(lease) = self.lease.take() {
            if lease.started_at.is_some() {
                for (key, reason) in lease.reasons {
                    if !self.successes.contains_key(&key) {
                        self.uncertain.insert(key, reason);
                    }
                }
                self.last_error =
                    Some("Interrupted warmup: outcome unknown; automatic replay suppressed".into());
            } else {
                for (key, reason) in lease.reasons {
                    if !self.successes.contains_key(&key) {
                        self.queued.insert(key, reason);
                    }
                }
            }
        }
    }

    fn succeed(&mut self, reasons: BTreeMap<String, Reason>, now: i64) {
        for key in reasons.into_keys() {
            self.queued.remove(&key);
            self.uncertain.remove(&key);
            self.successes.insert(key, now);
        }
        self.failures = 0;
        self.next_retry_at = 0;
        self.last_error = None;
        self.last_success_at = Some(now);
    }
}

fn reconcile(state: &mut Ledger, snapshot: &Snapshot, now: i64, slot: &LocalSlot) {
    state.last_checked_at = Some(now);
    for (id, account) in &mut state.accounts {
        account.queued.retain(|_, reason| {
            snapshot.receipts.contains_key(id) && reason.enabled(&snapshot.settings, id, slot)
        });
    }
    for (id, receipt) in &snapshot.receipts {
        let account = state.accounts.entry(id.clone()).or_default();
        account.last_checked_at = Some(now);
        if let Some(receipt) = receipt.as_ref().filter(|r| r.account_id == *id) {
            account.observe(receipt, now);
        }
        let queue = std::mem::take(&mut account.queued);
        account.queued = queue
            .into_iter()
            .filter(|(_, reason)| account.relevant(reason, now))
            .collect();
        for reason in account.due(now) {
            if reason.enabled(&snapshot.settings, id, slot) {
                account.enqueue(reason);
            }
        }
        for time in &snapshot.settings.timed_warmup_times {
            if valid_time(time) && time <= &slot.time {
                // Catch a missed tick within today; identity is per account/slot.
                account.enqueue(Reason::Timed {
                    day: slot.day.clone(),
                    time: time.clone(),
                });
            }
        }
    }
}

fn valid_time(time: &str) -> bool {
    let bytes = time.as_bytes();
    bytes.len() == 5
        && bytes[2] == b':'
        && bytes[..2]
            .iter()
            .chain(bytes[3..].iter())
            .all(u8::is_ascii_digit)
        && time[..2].parse::<u8>().is_ok_and(|h| h < 24)
        && time[3..].parse::<u8>().is_ok_and(|m| m < 60)
}

fn transaction<T>(root: &Path, f: impl FnOnce(&mut Ledger) -> Result<T>) -> Result<T> {
    // This short lock is NEVER retained across a CLI await. Call from the
    // blocking pool on async paths; quota/history writes remain independent.
    let _guard = lock_file(&root.join(STATE_LOCK))?;
    // Fail closed on invalid state; recovering an older backup could replay a
    // confirmed success. Durable atomic replacement already protects writes.
    let path = root.join(STATE_FILE);
    let mut state: Ledger = if path.exists() {
        serde_json::from_slice(&std::fs::read(&path)?)
            .context("Invalid warmup ledger; automatic replay refused")?
    } else {
        Ledger::default()
    };
    ensure!(state.version == 1, "Unsupported warmup ledger version");
    let before = serde_json::to_vec(&state)?;
    let result = f(&mut state)?;
    if !path.exists() || before != serde_json::to_vec(&state)? {
        super::storage::write_json(&path, &state)?;
    }
    Ok(result)
}

fn execution_lock(root: &Path) -> Result<Option<File>> {
    std::fs::create_dir_all(root)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join(EXECUTION_LOCK))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

struct Claim {
    id: String,
    token: String,
    // Own this OS lock through runtime await AND the durable result commit.
    _execution: File,
}

fn tick(m: &Manager, now: i64, slot: &LocalSlot) -> Result<Option<Claim>> {
    let snapshot = Snapshot::read(m)?;
    let execution = execution_lock(&m.root)?;
    transaction(&m.root, |state| {
        if execution.is_some() {
            for account in state.accounts.values_mut() {
                account.recover_orphan();
            }
        }
        reconcile(state, &snapshot, now, slot);
        if execution.is_none() {
            return Ok(None);
        }
        let candidate = state
            .accounts
            .iter()
            .filter(|(id, a)| {
                !a.queued.is_empty()
                    && a.next_retry_at <= now
                    && snapshot
                        .receipts
                        .get(*id)
                        .and_then(Option::as_ref)
                        .is_some_and(|r| safe_usage(r, id, now))
            })
            // Failed accounts don't monopolize the worker. Never-attempted ones
            // precede previously attempted ones, independently of account order.
            .min_by_key(|(_, a)| a.last_attempt_at)
            .map(|(id, _)| id.clone());
        let Some(id) = candidate else {
            return Ok(None);
        };
        let account = state.accounts.get_mut(&id).unwrap();
        let token = uuid::Uuid::new_v4().to_string();
        account.lease = Some(Lease {
            token: token.clone(),
            claimed_at: now,
            expires_at: now.saturating_add(LEASE_SECONDS),
            started_at: None,
            reasons: std::mem::take(&mut account.queued),
        });
        Ok(Some((id, token)))
    })
    .map(|claim| {
        claim.map(|(id, token)| Claim {
            id,
            token,
            _execution: execution.unwrap(),
        })
    })
}

fn begin(m: &Manager, claim: &Claim, now: i64, slot: &LocalSlot) -> Result<bool> {
    let snapshot = Snapshot::read(m)?;
    transaction(&m.root, |state| {
        reconcile(state, &snapshot, now, slot);
        let account = state
            .accounts
            .get_mut(&claim.id)
            .context("Missing warmup account ledger")?;
        let relevant: BTreeSet<_> = account
            .lease
            .as_ref()
            .context("Missing warmup lease")?
            .reasons
            .iter()
            .filter(|(_, r)| account.relevant(r, now))
            .map(|(k, _)| k.clone())
            .collect();
        let successes = &account.successes;
        let lease = account.lease.as_mut().context("Missing warmup lease")?;
        ensure!(lease.token == claim.token, "Warmup lease changed");
        lease.reasons.retain(|key, reason| {
            relevant.contains(key)
                && !successes.contains_key(key)
                && reason.enabled(&snapshot.settings, &claim.id, slot)
        });
        let safe = snapshot
            .receipts
            .get(&claim.id)
            .and_then(Option::as_ref)
            .is_some_and(|r| safe_usage(r, &claim.id, now));
        if lease.reasons.is_empty() || !safe {
            let lease = account.lease.take().unwrap();
            account.queued.extend(lease.reasons);
            return Ok(false);
        }
        lease.started_at = Some(now);
        lease.expires_at = now.saturating_add(LEASE_SECONDS);
        account.last_attempt_at = Some(now);
        Ok(true)
    })
}

fn finish(
    m: &Manager,
    claim: &Claim,
    now: i64,
    slot: &LocalSlot,
    error: Option<String>,
) -> Result<()> {
    let settings = error.as_ref().and_then(|_| m.settings().ok());
    transaction(&m.root, |state| {
        let account = state
            .accounts
            .get_mut(&claim.id)
            .context("Missing warmup account ledger")?;
        ensure!(
            account
                .lease
                .as_ref()
                .is_some_and(|lease| lease.token == claim.token),
            "Warmup lease changed"
        );
        let lease = account.lease.take().unwrap();
        if let Some(error) = error {
            for (key, reason) in lease.reasons {
                if !account.successes.contains_key(&key)
                    && account.relevant(&reason, now)
                    && settings
                        .as_ref()
                        .is_some_and(|s| reason.enabled(s, &claim.id, slot))
                {
                    account.queued.insert(key, reason);
                }
            }
            account.failures = account.failures.saturating_add(1);
            let delay = 30_i64
                .saturating_mul(1_i64 << account.failures.saturating_sub(1).min(7))
                .min(3600);
            account.next_retry_at = now.saturating_add(delay);
            account.last_error = Some(error);
        } else {
            account.succeed(lease.reasons, now);
        }
        Ok(())
    })
}

trait Executor: Send + Sync {
    fn execute<'a>(
        &'a self,
        m: &'a Manager,
        id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;
}
struct RuntimeExecutor;
impl Executor for RuntimeExecutor {
    fn execute<'a>(
        &'a self,
        m: &'a Manager,
        id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        // Keep the fixed minimal model/token/budget policy in runtime.rs.
        Box::pin(runtime::warmup(m, id))
    }
}

async fn execute<E: Executor>(m: Manager, claim: Claim, executor: &E) -> Result<()> {
    execute_with_clock(m, claim, executor, || {
        (Utc::now().timestamp(), LocalSlot::now())
    })
    .await
}

async fn execute_with_clock<E: Executor>(
    m: Manager,
    claim: Claim,
    executor: &E,
    clock: impl Fn() -> (i64, LocalSlot),
) -> Result<()> {
    let m_begin = m.clone();
    let (started_at, slot) = clock();
    // Move the claim, including its execution lock, through each blocking
    // operation. No gap opens between a lease commit and runtime completion.
    let (claim, run) = tokio::task::spawn_blocking(move || {
        let run = begin(&m_begin, &claim, started_at, &slot)?;
        Ok::<_, anyhow::Error>((claim, run))
    })
    .await??;
    if !run {
        return Ok(());
    }
    let result = executor.execute(&m, &claim.id).await;
    let error = result.err().map(|e| format!("{e:#}"));
    let (finished_at, slot) = clock();
    tokio::task::spawn_blocking(move || {
        let result = finish(&m, &claim, finished_at, &slot, error);
        drop(claim);
        result
    })
    .await??;
    Ok(())
}

/// Remember a receipt BEFORE HTTP refresh replaces it. No CLI/network work.
/// Await this hook before writing the new receipt; an error must not be ignored
/// if the caller needs durable missed-reset coverage.
pub async fn remember_usage(m: &Manager, receipt: &UsageReceipt) -> Result<()> {
    let m = m.clone();
    let receipt = receipt.clone();
    tokio::task::spawn_blocking(move || remember_usage_sync(&m, &receipt)).await?
}

/// Synchronous equivalent for runtime::collect. This takes only the short
/// ledger lock, never the execution lock or runtime's warmup.lock.
pub fn remember_usage_sync(m: &Manager, receipt: &UsageReceipt) -> Result<()> {
    transaction(&m.root, |state| {
        state
            .accounts
            .entry(receipt.account_id.clone())
            .or_default()
            .observe(&receipt, Utc::now().timestamp());
        Ok(())
    })
}

/// Read-only UI snapshot; null timestamps stay null. This does not start work.
pub fn warmup_status(m: &Manager) -> Result<Value> {
    let _guard = lock_file(&m.root.join(STATE_LOCK))?;
    let path = m.root.join(STATE_FILE);
    let state: Ledger = if path.exists() {
        serde_json::from_slice(&std::fs::read(path)?).context("Invalid warmup ledger")?
    } else {
        Ledger::default()
    };
    ensure!(state.version == 1, "Unsupported warmup ledger version");
    let now = Utc::now().timestamp();
    let accounts: BTreeMap<_, _> = state
        .accounts
        .iter()
        .map(|(id, a)| {
            let phase = if a.lease.is_some() {
                "running"
            } else if !a.uncertain.is_empty() {
                "uncertain"
            } else if !a.queued.is_empty() && a.next_retry_at > now {
                "backoff"
            } else if !a.queued.is_empty() {
                "pending"
            } else {
                "idle"
            };
            (
                id,
                json!({"phase":phase,"pending_tasks":a.queued.len(),
            "uncertain_tasks":a.uncertain.len(),"successful_tasks":a.successes.len(),
            "last_checked_at":a.last_checked_at,"last_observed_at":a.last_observed_at,
            "last_attempt_at":a.last_attempt_at,"last_success_at":a.last_success_at,
            "failures":a.failures,"next_retry_at":(a.next_retry_at>0).then_some(a.next_retry_at),
            "last_error":a.last_error,"lease":a.lease}),
            )
        })
        .collect();
    Ok(json!({"version":state.version,"last_checked_at":state.last_checked_at,"accounts":accounts}))
}

/// Call only after runtime::warmup returned Ok for this account. A manual
/// success settles currently due work, including disabled or uncertain work,
/// without stamping a global day or pre-crediting any future reset.
pub fn record_manual_warmup(m: &Manager, id: &str) -> Result<()> {
    let snapshot = Snapshot::read(m)?;
    ensure!(
        snapshot.receipts.contains_key(id),
        "Claude account not found"
    );
    let now = Utc::now().timestamp();
    let slot = LocalSlot::now();
    transaction(&m.root, |state| {
        reconcile(state, &snapshot, now, &slot);
        let account = state.accounts.entry(id.to_owned()).or_default();
        let mut reasons: BTreeMap<_, _> =
            account.due(now).into_iter().map(|r| (r.key(), r)).collect();
        reasons.extend(account.queued.clone());
        reasons.extend(account.uncertain.clone());
        // The automatic caller may have acquired its lease AFTER the manual
        // call finished. Never use that lease as evidence of manual success.
        reasons.retain(|key, reason| {
            !account
                .lease
                .as_ref()
                .is_some_and(|lease| lease.reasons.contains_key(key))
                && match reason {
                    Reason::PrimaryReset { reset } | Reason::WeeklyReset { reset } => *reset <= now,
                    _ => true,
                }
        });
        account.succeed(reasons, now);
        Ok(())
    })
}

/// Start one independent task with a 10-second planning tick and a separate
/// asynchronous worker. A 90-second CLI await never delays quota or planning.
pub fn start_warmup_monitor() {
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    tauri::async_runtime::spawn(async {
        let mut interval = tokio::time::interval(Duration::from_secs(10));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut worker: Option<tokio::task::JoinHandle<Result<()>>> = None;
        loop {
            interval.tick().await;
            if worker.as_ref().is_some_and(|w| w.is_finished()) {
                if let Some(handle) = worker.take() {
                    match handle.await {
                        Ok(Ok(())) => {}
                        result => eprintln!("Claude warmup worker: {result:?}"),
                    }
                }
            }
            let result = tokio::task::spawn_blocking(|| {
                let m = Manager::current()?;
                let claim = tick(&m, Utc::now().timestamp(), &LocalSlot::now())?;
                Ok::<_, anyhow::Error>((m, claim))
            })
            .await;
            match result {
                Ok(Ok((m, Some(claim)))) => {
                    worker = Some(tokio::spawn(async move {
                        execute(m, claim, &RuntimeExecutor).await
                    }));
                }
                Ok(Ok((_, None))) => {}
                Ok(Err(error)) => eprintln!("Claude warmup monitor: {error:#}"),
                Err(error) => eprintln!("Claude warmup planner: {error}"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::super::{storage::write_json, ClaudeAccount};
    use super::*;
    use std::{
        path::PathBuf,
        sync::{atomic::AtomicUsize, Arc, Mutex},
    };

    const NOW: i64 = 1_000_000;
    struct Fixture {
        base: PathBuf,
        m: Manager,
        ids: Vec<String>,
    }
    impl Fixture {
        fn new(count: usize) -> Self {
            let base =
                std::env::temp_dir().join(format!("claude-monitor-test-{}", uuid::Uuid::new_v4()));
            let m = Manager {
                root: base.join("store"),
                live_dir: base.join("live"),
                live_config: base.join("live.json"),
            };
            let ids: Vec<_> = (0..count)
                .map(|_| uuid::Uuid::new_v4().to_string())
                .collect();
            m.transaction(|s| {
                for id in &ids {
                    s.accounts.push(ClaudeAccount {
                        id: id.clone(),
                        name: "Fixture".into(),
                        email: None,
                        account_uuid: None,
                        plan_type: Some("pro".into()),
                        created_at: "fixture".into(),
                        last_used_at: None,
                    });
                }
                Ok(())
            })
            .unwrap();
            m.save_settings(&ClaudeSettings {
                warmup_all: true,
                ..Default::default()
            })
            .unwrap();
            let f = Self { base, m, ids };
            for i in 0..count {
                f.receipt(i, 0.0, None, Some(5.0), Some(NOW + 604800), NOW);
            }
            f
        }
        fn receipt(
            &self,
            i: usize,
            primary: f64,
            reset: Option<i64>,
            weekly: Option<f64>,
            weekly_reset: Option<i64>,
            at: i64,
        ) -> UsageReceipt {
            let id = &self.ids[i];
            let mut usage = crate::types::UsageInfo::error(id.clone(), "fixture".into());
            usage.error = None;
            usage.primary_used_percent = Some(primary);
            usage.primary_resets_at = reset;
            usage.primary_window_minutes = Some(300);
            usage.secondary_used_percent = weekly;
            usage.secondary_resets_at = weekly_reset;
            usage.secondary_window_minutes = Some(10080);
            let r = UsageReceipt {
                account_id: id.clone(),
                observed_at: at,
                usage,
                sessions: Default::default(),
            };
            self.write_receipt(i, &r);
            r
        }
        fn write_receipt(&self, i: usize, r: &UsageReceipt) {
            write_json(&self.m.profile(&self.ids[i]).unwrap().join("usage.json"), r).unwrap();
        }
        fn state(&self) -> Ledger {
            serde_json::from_slice(&std::fs::read(self.m.root.join(STATE_FILE)).unwrap()).unwrap()
        }
        fn claim(&self, now: i64) -> Option<Claim> {
            tick(&self.m, now, &slot()).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }
    fn slot() -> LocalSlot {
        LocalSlot {
            day: "2026-10-10".into(),
            time: "10:01".into(),
        }
    }

    #[derive(Default)]
    struct FakeExecutor {
        calls: Mutex<Vec<String>>,
        failures_left: AtomicUsize,
    }
    impl Executor for FakeExecutor {
        fn execute<'a>(
            &'a self,
            m: &'a Manager,
            id: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
            Box::pin(async move {
                let state: Ledger =
                    serde_json::from_slice(&std::fs::read(m.root.join(STATE_FILE))?)?;
                let a = &state.accounts[id];
                assert!(a.lease.as_ref().unwrap().started_at.is_some());
                assert!(a
                    .lease
                    .as_ref()
                    .unwrap()
                    .reasons
                    .keys()
                    .all(|k| !a.successes.contains_key(k)));
                self.calls.lock().unwrap().push(id.to_owned());
                if self
                    .failures_left
                    .try_update(Ordering::SeqCst, Ordering::SeqCst, |v| v.checked_sub(1))
                    .is_ok()
                {
                    anyhow::bail!("injected failure");
                }
                Ok(())
            })
        }
    }
    async fn run(f: &Fixture, claim: Claim, executor: &impl Executor, now: i64) {
        execute_with_clock(f.m.clone(), claim, executor, || (now, slot()))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn idle_null_reset_bootstraps_once_and_survives_restart() {
        let f = Fixture::new(1);
        let executor = FakeExecutor::default();
        let claim = f.claim(NOW).unwrap();
        let before = f.state();
        let a = &before.accounts[&f.ids[0]];
        assert!(a.primary_windows.is_empty());
        assert!(a.successes.is_empty());
        assert_eq!(a.lease.as_ref().unwrap().reasons.len(), 1);
        run(&f, claim, &executor, NOW).await;
        assert!(tick(&f.m.clone(), NOW + 10, &slot()).unwrap().is_none());
        assert_eq!(executor.calls.lock().unwrap().len(), 1);
        assert_eq!(f.state().accounts[&f.ids[0]].successes["idle:1"], NOW);
    }

    #[tokio::test]
    async fn primary_and_weekly_resets_coalesce_with_missed_tick_over_120_seconds() {
        let f = Fixture::new(1);
        f.receipt(0, 0.0, Some(NOW - 1000), Some(1.0), Some(NOW - 500), NOW);
        let executor = FakeExecutor::default();
        let claim = f.claim(NOW).unwrap();
        assert_eq!(
            f.state().accounts[&f.ids[0]]
                .lease
                .as_ref()
                .unwrap()
                .reasons
                .len(),
            2
        );
        run(&f, claim, &executor, NOW).await;
        assert_eq!(f.state().accounts[&f.ids[0]].successes.len(), 2);
        assert!(f.claim(NOW + 10).is_none());
        assert_eq!(executor.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn previous_reset_is_remembered_before_http_overwrites_receipt() {
        let f = Fixture::new(1);
        let previous = f.receipt(
            0,
            90.0,
            Some(NOW - 1000),
            Some(8.0),
            Some(NOW + 50000),
            NOW - 1001,
        );
        remember_usage(&f.m, &previous).await.unwrap();
        f.receipt(0, 0.0, Some(NOW + 18000), Some(8.0), Some(NOW + 50000), NOW);
        let executor = FakeExecutor::default();
        run(&f, f.claim(NOW).unwrap(), &executor, NOW).await;
        let state = f.state();
        let a = &state.accounts[&f.ids[0]];
        assert_eq!(a.primary_windows.len(), 2);
        assert!(a.successes.contains_key(&format!("primary:{}", NOW - 1000)));
        assert!(!a
            .successes
            .contains_key(&format!("primary:{}", NOW + 18000)));
        assert!(f.claim(NOW + 10).is_none());
    }

    #[test]
    fn sync_history_hook_does_not_execute_or_queue() {
        let f = Fixture::new(1);
        let r = f.receipt(0, 50.0, Some(NOW - 5), Some(5.0), Some(NOW + 1000), NOW);
        remember_usage_sync(&f.m, &r).unwrap();
        let a = &f.state().accounts[&f.ids[0]];
        assert!(a.primary_windows.contains(&(NOW - 5)));
        assert!(a.queued.is_empty() && a.lease.is_none() && a.successes.is_empty());
    }

    #[test]
    fn unknown_error_full_weekly_and_stale_usage_never_execute() {
        let f = Fixture::new(1);
        let baseline = f.m.receipt(&f.ids[0]).unwrap().unwrap();
        for case in 0..10 {
            let mut r = baseline.clone();
            match case {
                0 => r.usage.secondary_used_percent = None,
                1 => r.usage.secondary_used_percent = Some(99.5),
                2 => r.usage.secondary_used_percent = Some(100.0),
                3 => r.usage.error = Some("usage HTTP failed".into()),
                4 => r.usage.primary_used_percent = None,
                5 => r.usage.primary_used_percent = Some(3.0), // null reset with nonzero
                6 => r.usage.secondary_resets_at = None,       // null reset with nonzero
                7 => r.observed_at = NOW - 301,
                8 => r.observed_at = NOW + 6,
                9 => r.usage.account_id = "another-account".into(),
                _ => unreachable!(),
            }
            f.write_receipt(0, &r);
            assert!(f.claim(NOW).is_none(), "case {case}");
            assert!(f.state().accounts[&f.ids[0]].successes.is_empty());
        }
        let mut r = baseline;
        r.usage.secondary_used_percent = Some(f64::NAN);
        assert!(!safe_usage(&r, &f.ids[0], NOW));
        r.usage.secondary_used_percent = Some(f64::INFINITY);
        assert!(!safe_usage(&r, &f.ids[0], NOW));
    }

    #[tokio::test]
    async fn explicitly_zero_weekly_with_null_reset_is_known_not_unknown() {
        let f = Fixture::new(1);
        f.receipt(0, 0.0, None, Some(0.0), None, NOW);
        let executor = FakeExecutor::default();
        run(&f, f.claim(NOW).unwrap(), &executor, NOW).await;
        assert!(f.state().accounts[&f.ids[0]].weekly_windows.is_empty());
        assert_eq!(executor.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn disabled_settings_cancel_queued_and_reserved_work() {
        let f = Fixture::new(1);
        let claim = f.claim(NOW).unwrap();
        drop(claim); // Reserved, no model call made.
        f.m.save_settings(&ClaudeSettings::default()).unwrap();
        assert!(f.claim(NOW + 1).is_none());
        let a = &f.state().accounts[&f.ids[0]];
        assert!(a.queued.is_empty() && a.lease.is_none() && a.successes.is_empty());
    }

    #[tokio::test]
    async fn disabled_after_claim_is_rechecked_before_executor() {
        let f = Fixture::new(1);
        let claim = f.claim(NOW).unwrap();
        f.m.save_settings(&ClaudeSettings::default()).unwrap();
        let executor = FakeExecutor::default();
        run(&f, claim, &executor, NOW).await;
        assert!(executor.calls.lock().unwrap().is_empty());
        assert!(f.state().accounts[&f.ids[0]].lease.is_none());
    }

    #[tokio::test]
    async fn timed_failures_retry_per_account_not_global_day_and_survive_restart() {
        let f = Fixture::new(2);
        f.m.save_settings(&ClaudeSettings {
            timed_warmup_times: vec!["10:00".into()],
            ..Default::default()
        })
        .unwrap();
        let executor = FakeExecutor {
            failures_left: AtomicUsize::new(1),
            ..Default::default()
        };
        let failed = f.claim(NOW).unwrap();
        let failed_id = failed.id.clone();
        run(&f, failed, &executor, NOW).await;
        let state = f.state();
        assert!(state.accounts[&failed_id].successes.is_empty());
        assert_eq!(state.accounts[&failed_id].next_retry_at, NOW + 30);
        let other = f.claim(NOW + 1).unwrap();
        assert_ne!(other.id, failed_id);
        run(&f, other, &executor, NOW + 1).await;
        assert!(tick(&f.m.clone(), NOW + 29, &slot()).unwrap().is_none());
        let retry = tick(&f.m.clone(), NOW + 30, &slot()).unwrap().unwrap();
        assert_eq!(retry.id, failed_id);
        run(&f, retry, &executor, NOW + 30).await;
        assert!(f.claim(NOW + 31).is_none());
        assert_eq!(executor.calls.lock().unwrap().len(), 3);
        assert!(f.state().accounts.values().all(|a| a.successes.len() == 1));
    }

    #[tokio::test]
    async fn failure_backoff_grows_and_success_resets_it() {
        let f = Fixture::new(1);
        let executor = FakeExecutor {
            failures_left: AtomicUsize::new(2),
            ..Default::default()
        };
        run(&f, f.claim(NOW).unwrap(), &executor, NOW).await;
        assert!(f.claim(NOW + 29).is_none());
        run(&f, f.claim(NOW + 30).unwrap(), &executor, NOW + 30).await;
        assert_eq!(f.state().accounts[&f.ids[0]].next_retry_at, NOW + 90);
        assert!(f.claim(NOW + 89).is_none());
        run(&f, f.claim(NOW + 90).unwrap(), &executor, NOW + 90).await;
        let a = &f.state().accounts[&f.ids[0]];
        assert_eq!(a.failures, 0);
        assert_eq!(a.next_retry_at, 0);
        assert!(a.last_error.is_none());
    }

    #[test]
    fn reserved_orphan_is_retryable_but_executing_orphan_is_not_replayed() {
        let f = Fixture::new(1);
        drop(f.claim(NOW).unwrap());
        let claim = f.claim(NOW + 1).unwrap(); // No expiry wait required: OS owner is gone.
        assert!(begin(&f.m, &claim, NOW + 1, &slot()).unwrap());
        drop(claim); // Simulate crash across a potentially completed model call.
        assert!(f.claim(NOW + 2).is_none());
        let a = &f.state().accounts[&f.ids[0]];
        assert_eq!(a.uncertain.len(), 1);
        assert!(a.successes.is_empty() && a.lease.is_none());
        assert!(f.claim(NOW + 3).is_none());
    }

    #[test]
    fn very_old_or_superseded_resets_and_current_busy_windows_do_not_execute() {
        let f = Fixture::new(1);
        let old = f.receipt(
            0,
            80.0,
            Some(NOW - 18001),
            Some(8.0),
            Some(NOW - 604801),
            NOW - 20000,
        );
        remember_usage_sync(&f.m, &old).unwrap();
        f.receipt(0, 0.0, Some(NOW + 18000), Some(8.0), Some(NOW + 50000), NOW);
        assert!(f.claim(NOW).is_none());
        let recent = f.receipt(
            0,
            80.0,
            Some(NOW - 5),
            Some(8.0),
            Some(NOW + 50000),
            NOW - 6,
        );
        remember_usage_sync(&f.m, &recent).unwrap();
        f.receipt(0, 7.0, Some(NOW + 18000), Some(8.0), Some(NOW + 50000), NOW);
        assert!(f.claim(NOW).is_none());
        // Only the most recent missed reset becomes due once usage is zero.
        f.receipt(
            0,
            0.0,
            Some(NOW + 18000),
            Some(8.0),
            Some(NOW + 50000),
            NOW + 1,
        );
        let claim = f.claim(NOW + 1).unwrap();
        let a = &f.state().accounts[&f.ids[0]];
        assert_eq!(a.lease.as_ref().unwrap().reasons.len(), 1);
        assert!(a
            .lease
            .as_ref()
            .unwrap()
            .reasons
            .contains_key(&format!("primary:{}", NOW - 5)));
        drop(claim);
    }

    #[test]
    fn corrupt_primary_ledger_fails_closed_instead_of_replaying_backup() {
        let f = Fixture::new(1);
        drop(f.claim(NOW).unwrap());
        std::fs::write(f.m.root.join(STATE_FILE), b"\0\0").unwrap();
        assert!(tick(&f.m, NOW + 1, &slot()).is_err());
    }

    #[test]
    fn manual_success_settles_pending_and_uncertain_without_future_credit() {
        let f = Fixture::new(1);
        let now = Utc::now().timestamp();
        f.receipt(0, 0.0, None, Some(5.0), Some(now + 604800), now);
        let claim = f.claim(now).unwrap();
        assert!(begin(&f.m, &claim, now, &slot()).unwrap());
        drop(claim);
        assert!(f.claim(now).is_none());
        record_manual_warmup(&f.m, &f.ids[0]).unwrap();
        assert!(f.claim(now).is_none());
        let a = &f.state().accounts[&f.ids[0]];
        assert!(a.uncertain.is_empty());
        assert_eq!(a.successes.len(), 1);
        assert!(!a
            .successes
            .contains_key(&format!("weekly:{}", now + 604800)));
    }

    #[test]
    fn manual_hook_does_not_credit_a_new_automatic_lease_or_future_reset() {
        let f = Fixture::new(1);
        let now = Utc::now().timestamp();
        let receipt = f.receipt(0, 0.0, Some(now - 10), Some(5.0), Some(now + 604800), now);
        remember_usage_sync(&f.m, &receipt).unwrap();
        let claim = f.claim(now).unwrap();
        assert!(begin(&f.m, &claim, now, &slot()).unwrap());
        f.receipt(
            0,
            0.0,
            Some(now + 18000),
            Some(5.0),
            Some(now + 604800),
            now,
        );
        record_manual_warmup(&f.m, &f.ids[0]).unwrap();
        let a = &f.state().accounts[&f.ids[0]];
        assert!(a.successes.is_empty());
        assert!(a.lease.as_ref().unwrap().started_at.is_some());
        assert!(!a
            .successes
            .contains_key(&format!("primary:{}", now + 18000)));
        // Only the automatic executor's actual success settles its lease.
        finish(&f.m, &claim, now, &slot(), None).unwrap();
        assert_eq!(f.state().accounts[&f.ids[0]].successes.len(), 1);
    }

    #[test]
    fn unchanged_history_is_not_rewritten_for_each_usage_receipt() {
        let f = Fixture::new(1);
        let receipt = f.m.receipt(&f.ids[0]).unwrap().unwrap();
        remember_usage_sync(&f.m, &receipt).unwrap();
        let path = f.m.root.join(STATE_FILE);
        let before = std::fs::read(&path).unwrap();
        let mut newer = receipt;
        newer.observed_at += 1;
        remember_usage_sync(&f.m, &newer).unwrap();
        newer.observed_at += 1;
        remember_usage_sync(&f.m, &newer).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(!crate::auth::persistence::backup_path(&path).exists());
    }

    #[test]
    fn unknown_manual_receipt_cannot_seed_or_replace_window_history() {
        let f = Fixture::new(1);
        let now = Utc::now().timestamp();
        let known = f.receipt(
            0,
            5.0,
            Some(now + 18000),
            Some(8.0),
            Some(now + 604800),
            now,
        );
        remember_usage_sync(&f.m, &known).unwrap();
        let mut unknown = known;
        unknown.usage = crate::types::UsageInfo::error(f.ids[0].clone(), "unknown".into());
        unknown.observed_at += 1;
        f.write_receipt(0, &unknown);
        record_manual_warmup(&f.m, &f.ids[0]).unwrap();
        let a = &f.state().accounts[&f.ids[0]];
        assert_eq!(a.idle_generation, 0);
        assert!(!a.primary_idle);
        assert_eq!(a.primary_windows, BTreeSet::from([now + 18000]));
        assert!(a.successes.is_empty());
        assert!(a.last_success_at.is_some());
    }

    #[test]
    fn delayed_idle_observation_cannot_reseed_an_unexpired_known_window() {
        let f = Fixture::new(1);
        let mut a = AccountLedger::default();
        let mut receipt = f.m.receipt(&f.ids[0]).unwrap().unwrap();
        a.observe(&receipt, NOW);
        assert_eq!(a.idle_generation, 1);
        receipt.observed_at = NOW + 10;
        receipt.usage.primary_used_percent = Some(1.0);
        receipt.usage.primary_resets_at = Some(NOW + 18000);
        a.observe(&receipt, NOW + 10);
        receipt.observed_at = NOW + 11;
        receipt.usage.primary_used_percent = Some(0.0);
        receipt.usage.primary_resets_at = None;
        a.observe(&receipt, NOW + 11);
        assert!(!a.primary_idle);
        assert_eq!(a.idle_generation, 1);
    }

    #[test]
    fn failure_after_disabling_does_not_restore_cancelled_queue() {
        let f = Fixture::new(1);
        let claim = f.claim(NOW).unwrap();
        assert!(begin(&f.m, &claim, NOW, &slot()).unwrap());
        f.m.save_settings(&ClaudeSettings::default()).unwrap();
        assert!(f.claim(NOW + 1).is_none());
        finish(
            &f.m,
            &claim,
            NOW + 1,
            &slot(),
            Some("injected failure".into()),
        )
        .unwrap();
        let a = &f.state().accounts[&f.ids[0]];
        assert!(a.queued.is_empty());
        assert!(a.successes.is_empty());
        assert_eq!(a.failures, 1);
    }

    #[test]
    fn status_is_read_only_and_reports_null_unobserved_times() {
        let f = Fixture::new(1);
        assert!(warmup_status(&f.m).unwrap()["last_checked_at"].is_null());
        assert!(!f.m.root.join(STATE_FILE).exists());
        let claim = f.claim(NOW).unwrap();
        let status = warmup_status(&f.m).unwrap();
        let a = &status["accounts"][&f.ids[0]];
        assert_eq!(a["phase"], "running");
        assert!(a["last_success_at"].is_null());
        assert_eq!(a["lease"]["claimed_at"], NOW);
        drop(claim);
    }

    struct HeldExecutor {
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }
    impl Executor for HeldExecutor {
        fn execute<'a>(
            &'a self,
            _: &'a Manager,
            _: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
            Box::pin(async move {
                self.entered.notify_one();
                self.release.notified().await;
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn lock_is_held_across_await_without_blocking_planning_quota_or_other_process() {
        let f = Fixture::new(1);
        let executor = Arc::new(HeldExecutor {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        let claim = f.claim(NOW).unwrap();
        let m = f.m.clone();
        let exec = executor.clone();
        let worker = tokio::spawn(async move {
            execute_with_clock(m, claim, exec.as_ref(), || (NOW, slot())).await
        });
        tokio::time::timeout(Duration::from_secs(5), executor.entered.notified())
            .await
            .unwrap();
        assert!(execution_lock(&f.m.root).unwrap().is_none());
        // Even an expired lease cannot override an execution holding its lock.
        f.receipt(
            0,
            0.0,
            None,
            Some(5.0),
            Some(NOW + 604800),
            NOW + LEASE_SECONDS + 1,
        );
        let m = f.m.clone();
        let planned = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::task::spawn_blocking(move || tick(&m, NOW + LEASE_SECONDS + 1, &slot())),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
        assert!(planned.is_none());
        assert!(f.m.receipt(&f.ids[0]).unwrap().is_some());
        let output = tokio::time::timeout(
            Duration::from_secs(10),
            tokio::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "claude::monitor::tests::execution_lock_child",
                    "--ignored",
                    "--nocapture",
                ])
                .env("CLAUDE_MONITOR_TEST_ROOT", &f.m.root)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("test result: ok. 1 passed"));
        assert!(f.state().accounts[&f.ids[0]].successes.is_empty());
        f.m.save_settings(&ClaudeSettings::default()).unwrap();
        assert!(f.claim(NOW + LEASE_SECONDS + 1).is_none());
        assert!(f.state().accounts[&f.ids[0]].queued.is_empty());
        executor.release.notify_one();
        worker.await.unwrap().unwrap();
        assert!(execution_lock(&f.m.root).unwrap().is_some());
        assert_eq!(f.state().accounts[&f.ids[0]].successes.len(), 1);
    }

    #[test]
    #[ignore = "Invoked only by the cross-process fixture with a temporary ledger root"]
    fn execution_lock_child() {
        let root =
            PathBuf::from(std::env::var_os("CLAUDE_MONITOR_TEST_ROOT").expect("fixture root"));
        assert!(execution_lock(&root).unwrap().is_none());
    }
}
