use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rand_core::RngCore;
use uuid::Uuid;

/// Maximum number of concurrent pending login flows to prevent memory exhaustion.
const MAX_PENDING_FLOWS: usize = 1000;

#[derive(Debug, Clone)]
pub struct LoginFlowInfo {
    pub poll_token: String,
    pub poll_endpoint: String,
    pub login_url: String,
}

#[derive(Debug)]
pub enum LoginFlowError {
    TooManyPendingFlows,
}

#[derive(Debug, Clone)]
pub struct LoginResult {
    pub server: String,
    pub login_name: String,
    pub app_password: String,
}

#[derive(Debug)]
struct PendingFlow {
    created_at: Instant,
    poll_token: String,
    /// Set after the user authenticates on the login page **and** has more
    /// than one root drive — the flow is paused until the user picks a
    /// drive on the picker page. Consumed by `take_pending_user` when the
    /// picker submission arrives, so the second step is single-use even
    /// if the flow token leaks. `None` for single-drive accounts (legacy
    /// path goes straight to `completed`).
    pending_user_id: Option<Uuid>,
    /// App-password label to persist when this multi-drive flow finally
    /// completes. Stashed by `resolve_drive_or_complete` (login_v2_handler)
    /// alongside `pending_user_id` so `handle_drive_pick` can preserve
    /// provenance (`"Nextcloud"` vs `"Nextcloud (OIDC)"`) across the
    /// picker round-trip. Consumed by `take_pending_app_password_label`.
    pending_app_password_label: Option<String>,
    completed: Option<LoginResult>,
}

#[derive(Default)]
struct FlowState {
    flows: HashMap<String, PendingFlow>,
    poll_to_flow: HashMap<String, String>,
}

#[derive(Clone)]
pub struct NextcloudLoginFlowService {
    ttl: Duration,
    /// Uses `std::sync::Mutex` (not `tokio::sync::Mutex`) because the lock is
    /// never held across an `.await` point — all operations are synchronous
    /// HashMap lookups/inserts. This avoids the overhead of an async mutex.
    /// **Constraint:** Do not add `.await` calls inside any `self.state.lock()` scope.
    state: Arc<Mutex<FlowState>>,
}

impl NextcloudLoginFlowService {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            state: Arc::new(Mutex::new(FlowState::default())),
        }
    }

    pub fn new_stub() -> Self {
        Self::new(Duration::from_secs(600))
    }

    pub fn initiate(&self, base_url: &str) -> Result<LoginFlowInfo, LoginFlowError> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        prune_expired(&mut state, self.ttl);

        if state.flows.len() >= MAX_PENDING_FLOWS {
            return Err(LoginFlowError::TooManyPendingFlows);
        }

        let poll_token = random_hex(64);
        let flow_token = random_hex(48);

        state
            .poll_to_flow
            .insert(poll_token.clone(), flow_token.clone());
        state.flows.insert(
            flow_token.clone(),
            PendingFlow {
                created_at: Instant::now(),
                poll_token: poll_token.clone(),
                pending_user_id: None,
                pending_app_password_label: None,
                completed: None,
            },
        );

        Ok(LoginFlowInfo {
            poll_token: poll_token.clone(),
            poll_endpoint: format!("{}/login/v2/poll", base_url.trim_end_matches('/')),
            login_url: format!(
                "{}/login/v2/flow/{}",
                base_url.trim_end_matches('/'),
                flow_token
            ),
        })
    }

    pub fn flow_exists(&self, flow_token: &str) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        prune_expired(&mut state, self.ttl);
        state.flows.contains_key(flow_token)
    }

    /// Stash a verified user_id on the flow so a follow-up drive-pick
    /// request can prove "this browser just authenticated" without
    /// asking for the password again. Returns `false` if the flow
    /// token is unknown or expired.
    ///
    /// Only used on multi-drive accounts — single-drive logins go
    /// straight to [`complete`](Self::complete).
    pub fn mark_awaiting_drive(&self, flow_token: &str, user_id: Uuid) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        prune_expired(&mut state, self.ttl);
        match state.flows.get_mut(flow_token) {
            Some(pending) => {
                pending.pending_user_id = Some(user_id);
                true
            }
            None => false,
        }
    }

    /// Consume the stashed user_id (single-use). Returns the user_id
    /// when the flow is in "awaiting drive choice" state, or `None`
    /// when the flow is unknown, expired, or was never marked. Single-
    /// use semantics make this safe even if the flow token leaks: the
    /// second drive-pick attempt finds nothing to consume.
    pub fn take_pending_user(&self, flow_token: &str) -> Option<Uuid> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        prune_expired(&mut state, self.ttl);
        state
            .flows
            .get_mut(flow_token)
            .and_then(|pending| pending.pending_user_id.take())
    }

    /// Stash the app-password label to use when the flow eventually
    /// completes via `handle_drive_pick`. Called alongside
    /// `mark_awaiting_drive` so the multi-drive round-trip preserves
    /// the provenance string passed in at the auth step
    /// (`"Nextcloud"` for password login, `"Nextcloud (OIDC)"` for OIDC).
    /// Silently no-ops when the flow token is unknown or expired —
    /// the earlier `mark_awaiting_drive` on the same token is the
    /// authoritative "exists?" signal so we don't need to log again.
    pub fn set_pending_app_password_label(&self, flow_token: &str, label: &str) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        prune_expired(&mut state, self.ttl);
        if let Some(pending) = state.flows.get_mut(flow_token) {
            pending.pending_app_password_label = Some(label.to_string());
        }
    }

    /// Consume the stashed app-password label (single-use). Returns
    /// `None` when the flow was never marked, was password-shortcut
    /// (single-drive), or the token is unknown / expired — the caller
    /// falls back to a sensible default in that case.
    pub fn take_pending_app_password_label(&self, flow_token: &str) -> Option<String> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        prune_expired(&mut state, self.ttl);
        state
            .flows
            .get_mut(flow_token)
            .and_then(|pending| pending.pending_app_password_label.take())
    }

    pub fn complete(
        &self,
        flow_token: &str,
        username: &str,
        server: &str,
        app_password: &str,
    ) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        prune_expired(&mut state, self.ttl);

        let pending = match state.flows.get_mut(flow_token) {
            Some(pending) => pending,
            None => return false,
        };

        pending.completed = Some(LoginResult {
            server: server.to_string(),
            login_name: username.to_string(),
            app_password: app_password.to_string(),
        });

        true
    }

    pub fn poll(&self, poll_token: &str) -> Option<LoginResult> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        prune_expired(&mut state, self.ttl);

        let flow_token = state.poll_to_flow.get(poll_token).cloned()?;
        let pending = state.flows.get_mut(&flow_token)?;

        if let Some(result) = pending.completed.take() {
            state.poll_to_flow.remove(poll_token);
            state.flows.remove(&flow_token);
            Some(result)
        } else {
            None
        }
    }
}

fn prune_expired(state: &mut FlowState, ttl: Duration) {
    let now = Instant::now();
    let expired: Vec<String> = state
        .flows
        .iter()
        .filter(|(_, flow)| now.duration_since(flow.created_at) > ttl)
        .map(|(token, _)| token.clone())
        .collect();

    for flow_token in expired {
        if let Some(flow) = state.flows.remove(&flow_token) {
            state.poll_to_flow.remove(&flow.poll_token);
        }
    }
}

fn random_hex(len: usize) -> String {
    let mut bytes = vec![0u8; len.div_ceil(2)];
    rand_core::OsRng.fill_bytes(&mut bytes);
    let mut out = hex::encode(bytes);
    out.truncate(len);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service() -> NextcloudLoginFlowService {
        NextcloudLoginFlowService::new(Duration::from_secs(600))
    }

    #[test]
    fn test_initiate_returns_valid_tokens() {
        let svc = service();
        let info = svc.initiate("https://cloud.example.com").unwrap();

        assert!(!info.poll_token.is_empty());
        assert!(
            info.login_url
                .starts_with("https://cloud.example.com/login/v2/flow/")
        );
        assert_eq!(
            info.poll_endpoint,
            "https://cloud.example.com/login/v2/poll"
        );
    }

    #[test]
    fn test_flow_exists_after_initiate() {
        let svc = service();
        let info = svc.initiate("https://cloud.example.com").unwrap();

        // Extract flow token from login URL.
        let flow_token = info.login_url.rsplit('/').next().unwrap();
        assert!(svc.flow_exists(flow_token));
    }

    #[test]
    fn test_flow_not_found_for_unknown_token() {
        let svc = service();
        assert!(!svc.flow_exists("nonexistent-token"));
    }

    #[test]
    fn test_poll_returns_none_before_completion() {
        let svc = service();
        let info = svc.initiate("https://cloud.example.com").unwrap();
        assert!(svc.poll(&info.poll_token).is_none());
    }

    #[test]
    fn test_complete_and_poll_full_sequence() {
        let svc = service();
        let info = svc.initiate("https://cloud.example.com").unwrap();
        let flow_token = info.login_url.rsplit('/').next().unwrap();

        // Complete the flow.
        let completed = svc.complete(
            flow_token,
            "alice",
            "https://cloud.example.com",
            "APP-PASS-12345",
        );
        assert!(completed);

        // Poll should return the result exactly once.
        let result = svc.poll(&info.poll_token).expect("should return result");
        assert_eq!(result.login_name, "alice");
        assert_eq!(result.server, "https://cloud.example.com");
        assert_eq!(result.app_password, "APP-PASS-12345");

        // Second poll should return None (consumed).
        assert!(svc.poll(&info.poll_token).is_none());
    }

    #[test]
    fn test_complete_unknown_flow_returns_false() {
        let svc = service();
        assert!(!svc.complete("nonexistent", "alice", "https://x.com", "pass"));
    }

    #[test]
    fn test_expired_flows_are_pruned() {
        let svc = NextcloudLoginFlowService::new(Duration::from_millis(1));
        let info = svc.initiate("https://cloud.example.com").unwrap();
        let flow_token = info.login_url.rsplit('/').next().unwrap();

        // Wait for expiry.
        std::thread::sleep(Duration::from_millis(10));

        assert!(!svc.flow_exists(flow_token));
        assert!(svc.poll(&info.poll_token).is_none());
    }

    #[test]
    fn test_mark_awaiting_drive_then_take_pending_user() {
        let svc = service();
        let info = svc.initiate("https://cloud.example.com").unwrap();
        let flow_token = info.login_url.rsplit('/').next().unwrap();
        let uid = Uuid::new_v4();

        assert!(svc.mark_awaiting_drive(flow_token, uid));
        // First take consumes the slot.
        assert_eq!(svc.take_pending_user(flow_token), Some(uid));
        // Second take must return None (single-use).
        assert_eq!(svc.take_pending_user(flow_token), None);
    }

    #[test]
    fn test_mark_awaiting_drive_unknown_flow_returns_false() {
        let svc = service();
        assert!(!svc.mark_awaiting_drive("nonexistent", Uuid::new_v4()));
    }

    #[test]
    fn test_take_pending_user_without_mark_returns_none() {
        let svc = service();
        let info = svc.initiate("https://cloud.example.com").unwrap();
        let flow_token = info.login_url.rsplit('/').next().unwrap();
        // Flow exists but mark_awaiting_drive was never called.
        assert_eq!(svc.take_pending_user(flow_token), None);
    }

    #[test]
    fn test_max_pending_flows_cap() {
        let svc = NextcloudLoginFlowService::new(Duration::from_secs(600));
        for _ in 0..MAX_PENDING_FLOWS {
            svc.initiate("https://cloud.example.com").unwrap();
        }
        // The next initiate should fail
        assert!(svc.initiate("https://cloud.example.com").is_err());
    }
}
