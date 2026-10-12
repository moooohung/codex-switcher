//! Authentication module

pub mod oauth_server;
pub mod persistence;
pub mod storage;
pub mod switcher;
pub mod token_refresh;

// Credential mutations share one ordered gate; valid-token reads bypass it.
pub(crate) static AUTH_OPERATION_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub(crate) struct AuthOperationGuard {
    _guard: tokio::sync::MutexGuard<'static, ()>,
    kind: &'static str,
    account: String,
    started: std::time::Instant,
}

/// Name real auth work so a release build can identify which job owns the gate.
pub(crate) async fn lock_auth_operation(kind: &'static str, account: &str) -> AuthOperationGuard {
    let queued = std::time::Instant::now();
    crate::auto_switch::record_event(
        &format!("auth_job_queued_{kind}"),
        Some(account),
        None,
        None,
        None,
    );
    let guard = AUTH_OPERATION_LOCK.lock().await;
    crate::auto_switch::record_event(
        &format!("auth_job_started_{kind}"),
        Some(account),
        None,
        Some(queued.elapsed()),
        None,
    );
    AuthOperationGuard {
        _guard: guard,
        kind,
        account: account.to_owned(),
        started: std::time::Instant::now(),
    }
}

impl Drop for AuthOperationGuard {
    fn drop(&mut self) {
        crate::auto_switch::record_event(
            &format!("auth_job_finished_{}", self.kind),
            Some(&self.account),
            None,
            Some(self.started.elapsed()),
            None,
        );
    }
}

/// Coordinate refresh/switch operations across the desktop and web processes.
pub(crate) async fn lock_auth_operations() -> anyhow::Result<std::fs::File> {
    let path = storage::get_config_dir()?.join("auth-operations.lock");
    tokio::task::spawn_blocking(move || persistence::lock_file(&path)).await?
}

pub use oauth_server::*;
pub use storage::*;
pub use switcher::*;
pub use token_refresh::*;
