//! Bridges the [`UserLifecycleHook`] fan-out to the plugin runtime.
//!
//! `UserLifecycleService` already notifies hooks on user create/login/logout/
//! delete. This adapter turns the *login* event into a `user.login` plugin
//! event. It references only the [`PluginDispatchPort`] trait (not Extism), so
//! it is always compiled and the Extism dependency stays in the infrastructure
//! layer.
//!
//! Privacy note: the `user.login` payload is deliberately minimal — an opaque
//! `user_id` plus two non-identifying booleans (`first_login`, `is_external`).
//! It carries no email or username, so no PII reaches untrusted plugins in M0.
//! When the permissions system lands, richer fields (email, username) can be
//! added back behind a granted permission.

use std::sync::Arc;

use async_trait::async_trait;
use uuid::Uuid;

use crate::application::ports::plugin_ports::{EVENT_USER_LOGIN, PluginDispatchPort, PluginEvent};
use crate::application::ports::user_lifecycle::{DeletionMode, LogoutReason, UserLifecycleHook};
use crate::common::errors::DomainError;
use crate::domain::entities::user::User;

/// Lifecycle hook that forwards successful logins to subscribed plugins.
pub struct PluginUserLifecycleHook {
    dispatch: Arc<dyn PluginDispatchPort>,
}

impl PluginUserLifecycleHook {
    pub fn new(dispatch: Arc<dyn PluginDispatchPort>) -> Self {
        Self { dispatch }
    }
}

#[async_trait]
impl UserLifecycleHook for PluginUserLifecycleHook {
    fn name(&self) -> &'static str {
        "plugins"
    }

    async fn on_user_login(&self, user: &User) -> Result<(), DomainError> {
        if self.dispatch.has_subscribers(EVENT_USER_LOGIN) {
            self.dispatch.dispatch(PluginEvent {
                name: EVENT_USER_LOGIN,
                user_id: Some(user.id().to_string()),
                invocation_id: Uuid::new_v4().to_string(),
                payload: serde_json::json!({
                    "user_id": user.id().to_string(),
                    "first_login": user.last_login_at().is_none(),
                    "is_external": user.is_external(),
                }),
            });
        }
        // Returns immediately — dispatch is fire-and-forget (the runtime runs the
        // plugin on the blocking pool), so login latency is unaffected.
        Ok(())
    }

    // M0 emits only `user.login`. The trait forces an explicit decision on the
    // other three events; they are deliberate no-ops (reserved for future events
    // like `user.created` / `user.deleted`).
    async fn on_user_created(&self, _user: &User) -> Result<(), DomainError> {
        Ok(())
    }

    async fn on_user_logout(&self, _user: &User, _reason: LogoutReason) -> Result<(), DomainError> {
        Ok(())
    }

    async fn on_user_deleted(
        &self,
        _user: &User,
        _mode: DeletionMode,
        _tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<(), DomainError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::domain::entities::user::UserRole;

    /// Records dispatched events so the test can assert the bridge built the
    /// right `user.login` event without a runtime or DB.
    #[derive(Default)]
    struct RecordingDispatch {
        events: Mutex<Vec<PluginEvent>>,
    }

    impl PluginDispatchPort for RecordingDispatch {
        fn dispatch(&self, event: PluginEvent) {
            self.events.lock().unwrap().push(event);
        }
        fn has_subscribers(&self, _event: &str) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn on_user_login_dispatches_user_login_event() {
        let recorder = Arc::new(RecordingDispatch::default());
        let hook = PluginUserLifecycleHook::new(recorder.clone());

        let user = User::new(
            "alice@example.com".to_string(),
            Some("alice".to_string()),
            None,
            None, // federation_kind
            None, // federation_issuer
            None, // federation_subject
            UserRole::User,
            0,
            false,
        )
        .unwrap();

        hook.on_user_login(&user).await.unwrap();

        let events = recorder.events.lock().unwrap();
        assert_eq!(events.len(), 1, "exactly one event dispatched");
        let ev = &events[0];
        assert_eq!(ev.name, EVENT_USER_LOGIN);
        assert_eq!(ev.user_id.as_deref(), Some(user.id().to_string().as_str()));
        assert_eq!(ev.payload["user_id"], user.id().to_string());
        assert_eq!(ev.payload["first_login"], true); // last_login_at is None
        assert_eq!(ev.payload["is_external"], false);
        // Minimal payload: no PII fields.
        assert!(ev.payload.get("email").is_none(), "must not leak email");
        assert!(
            ev.payload.get("username").is_none(),
            "must not leak username"
        );
    }

    #[tokio::test]
    async fn skips_dispatch_when_no_subscribers() {
        struct NoSubscribers;
        impl PluginDispatchPort for NoSubscribers {
            fn dispatch(&self, _event: PluginEvent) {
                panic!("must not dispatch when nothing subscribes");
            }
            fn has_subscribers(&self, _event: &str) -> bool {
                false
            }
        }
        let hook = PluginUserLifecycleHook::new(Arc::new(NoSubscribers));
        let user = User::new(
            "bob@example.com".to_string(),
            None,
            None,
            None, // federation_kind
            None, // federation_issuer
            None, // federation_subject
            UserRole::User,
            0,
            false,
        )
        .unwrap();
        hook.on_user_login(&user).await.unwrap();
    }
}
