//! OAuth adds credentials; switching uses the same guarded flow as other accounts.
use crate::auth::oauth_server::{start_oauth_login, wait_for_oauth_login, OAuthLoginResult};
use crate::auth::{add_account, load_accounts};
use crate::types::{AccountInfo, OAuthLoginInfo};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use tokio::sync::oneshot;
struct PendingOAuth {
    generation: u64,
    rx: Option<oneshot::Receiver<anyhow::Result<OAuthLoginResult>>>,
    cancelled: Arc<AtomicBool>,
}
static PENDING_OAUTH: Mutex<Option<PendingOAuth>> = Mutex::new(None);
static GENERATION: AtomicU64 = AtomicU64::new(0);
#[tauri::command]
pub async fn start_login(account_name: String) -> Result<OAuthLoginInfo, String> {
    let generation = {
        let mut pending = PENDING_OAUTH.lock().unwrap();
        if let Some(previous) = pending.take() {
            previous.cancelled.store(true, Ordering::SeqCst);
        }
        GENERATION.fetch_add(1, Ordering::SeqCst) + 1
    };
    let (info, rx, cancelled) = start_oauth_login(account_name.trim().to_string())
        .await
        .map_err(|e| e.to_string())?;
    let mut pending = PENDING_OAUTH.lock().unwrap();
    if GENERATION.load(Ordering::SeqCst) != generation {
        cancelled.store(true, Ordering::SeqCst);
        return Err("OAuth login was cancelled".into());
    }
    *pending = Some(PendingOAuth {
        generation,
        rx: Some(rx),
        cancelled,
    });
    Ok(info)
}
#[tauri::command]
pub async fn complete_login() -> Result<AccountInfo, String> {
    let (generation, rx, cancelled) = {
        let mut guard = PENDING_OAUTH.lock().unwrap();
        let pending = guard.as_mut().ok_or("No pending OAuth login")?;
        (
            pending.generation,
            pending
                .rx
                .take()
                .ok_or("OAuth completion already in progress")?,
            pending.cancelled.clone(),
        )
    };
    let result = wait_for_oauth_login(rx).await;
    let mut pending = PENDING_OAUTH.lock().unwrap();
    if cancelled.load(Ordering::SeqCst)
        || !pending.as_ref().is_some_and(|p| p.generation == generation)
    {
        return Err("OAuth login was cancelled".into());
    }
    *pending = None;
    let account = result.map_err(|e| e.to_string())?;
    // Cancellation and commit share a mutex; cancelled flows cannot add accounts.
    let stored = add_account(account).map_err(|e| e.to_string())?;
    let store = load_accounts().map_err(|e| e.to_string())?;
    Ok(AccountInfo::from_stored(
        &stored,
        store.active_account_id.as_deref(),
    ))
}
#[tauri::command]
pub async fn cancel_login() -> Result<(), String> {
    let mut pending = PENDING_OAUTH.lock().unwrap();
    GENERATION.fetch_add(1, Ordering::SeqCst);
    if let Some(flow) = pending.take() {
        flow.cancelled.store(true, Ordering::SeqCst);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancel_during_completion_rejects_a_late_success_without_touching_storage() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let (tx, rx) = oneshot::channel();
            let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
            let cancelled = Arc::new(AtomicBool::new(false));
            *PENDING_OAUTH.lock().unwrap() = Some(PendingOAuth {
                generation,
                rx: Some(rx),
                cancelled: cancelled.clone(),
            });
            let completion = tokio::spawn(complete_login());
            loop {
                if PENDING_OAUTH
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|p| p.rx.is_none())
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
            cancel_login().await.unwrap();
            assert!(cancelled.load(Ordering::SeqCst));
            let account =
                crate::types::StoredAccount::new_api_key("late".into(), "synthetic".into());
            assert!(tx.send(Ok(OAuthLoginResult { account })).is_ok());
            assert!(completion.await.unwrap().unwrap_err().contains("cancelled"));
        });
    }
}
