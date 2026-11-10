//! Global selection is persisted before a single immutable public snapshot moves.
//! Reads never contend with the SQL mutation lock. Files are loaded at boot or on an explicit administrator refresh.
use crate::{
    application::{
        dtos::theme_dto::AppearanceDto,
        ports::{
            authorization_ports::{AdminCallerPort, AuthorizationEngine},
            theme_ports::{PreparedTheme, ThemeCatalogue, ThemeCatalogueLoader},
        },
    },
    common::errors::DomainError,
    domain::{entities::theme::ThemeId, repositories::settings_repository::SettingsRepository},
};
use arc_swap::ArcSwap;
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;
const KEY: &str = "appearance.active_theme";

struct Snapshot {
    catalogue: Arc<ThemeCatalogue>,
    theme: Arc<PreparedTheme>,
    selected: Option<String>,
    warning: Option<String>,
}
pub struct ThemeService<S: SettingsRepository> {
    repository: Arc<S>,
    loader: Option<Arc<dyn ThemeCatalogueLoader + Send + Sync>>,
    default_id: String,
    force_default: bool,
    snapshot: ArcSwap<Snapshot>,
    mutation: Mutex<()>,
}
impl<S: SettingsRepository> ThemeService<S> {
    pub async fn new(
        repository: Arc<S>,
        catalogue: ThemeCatalogue,
        default_id: String,
    ) -> Result<Self, DomainError> {
        let catalogue = Arc::new(catalogue);
        if !catalogue.themes.contains_key(&default_id) {
            return Err(DomainError::validation_error(
                "Configured default theme is unavailable",
            ));
        }
        let selected = repository.get(KEY).await?;
        let snapshot = Self::resolve(catalogue, &default_id, selected)?;
        if snapshot.warning.is_some() {
            tracing::warn!(
                event = "appearance.fallback",
                "Selected theme unavailable; serving default without changing saved selection"
            );
        }
        Ok(Self {
            repository,
            loader: None,
            force_default: false,
            default_id,
            snapshot: ArcSwap::from_pointee(snapshot),
            mutation: Mutex::new(()),
        })
    }
    fn resolve(
        catalogue: Arc<ThemeCatalogue>,
        default_id: &str,
        selected: Option<String>,
    ) -> Result<Snapshot, DomainError> {
        let requested = selected.as_deref().unwrap_or(default_id);
        let (active, warning) = if catalogue.themes.contains_key(requested) {
            (requested, None)
        } else {
            ("default", Some("selected_theme_unavailable".to_owned()))
        };
        let theme = catalogue.themes.get(active).cloned().ok_or_else(|| {
            DomainError::internal_error("Theme", "Built-in default theme is missing")
        })?;
        Ok(Snapshot {
            catalogue,
            theme,
            selected,
            warning,
        })
    }
    /// Recovery mode preserves the saved choice but never serves operator CSS.
    pub fn with_forced_default(mut self, enabled: bool) -> Self {
        self.force_default = enabled;
        if enabled {
            self.default_id = "default".into();
            let current = self.snapshot.load_full();
            let forced = self
                .resolve_snapshot(current.catalogue.clone(), current.selected.clone())
                .expect("built-in default was validated during construction");
            self.snapshot.store(Arc::new(forced));
            tracing::warn!(
                event = "appearance.forced_default",
                "Serving built-in theme; saved selection is preserved"
            );
        }
        self
    }
    fn resolve_snapshot(
        &self,
        catalogue: Arc<ThemeCatalogue>,
        selected: Option<String>,
    ) -> Result<Snapshot, DomainError> {
        if self.force_default {
            let mut snapshot = Self::resolve(catalogue, "default", None)?;
            snapshot.selected = selected;
            snapshot.warning = Some("forced_default".into());
            Ok(snapshot)
        } else {
            Self::resolve(catalogue, &self.default_id, selected)
        }
    }
    pub fn with_loader(mut self, loader: Arc<dyn ThemeCatalogueLoader + Send + Sync>) -> Self {
        self.loader = Some(loader);
        self
    }
    pub async fn reload_with_perms<C: AdminCallerPort, A: AuthorizationEngine>(
        self: &Arc<Self>,
        caller_id: Uuid,
        users: &C,
        authorization: &A,
    ) -> Result<AppearanceDto, DomainError> {
        users.require_admin_caller(authorization, caller_id).await?;
        let service = self.clone();
        tokio::spawn(async move {
            let appearance = service.reload().await?;
            tracing::info!(target: "audit", event = "appearance.reloaded", reason = "admin_refresh", %caller_id, "Theme catalogue reloaded");
            Ok(appearance)
        }).await.map_err(|e| DomainError::internal_error("Theme", e.to_string()))?
    }
    async fn reload(&self) -> Result<AppearanceDto, DomainError> {
        let _guard = self.mutation.lock().await;
        let loader = self
            .loader
            .clone()
            .ok_or_else(|| DomainError::internal_error("Theme", "Theme loader unavailable"))?;
        let catalogue = tokio::task::spawn_blocking(move || loader.load())
            .await
            .map_err(|e| DomainError::internal_error("Theme", e.to_string()))??;
        if !catalogue.themes.contains_key(&self.default_id) {
            return Err(DomainError::validation_error(
                "Configured default theme is unavailable",
            ));
        }
        let previous = self.active();
        if catalogue
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.theme_id.as_deref() == Some(previous.id.as_str()))
        {
            return Err(DomainError::validation_error(
                "Active theme is invalid or unreadable; previous catalogue retained",
            ));
        }
        let selected = self.repository.get(KEY).await?;
        let snapshot = self.resolve_snapshot(Arc::new(catalogue), selected)?;
        self.snapshot.store(Arc::new(snapshot));
        Ok(self.appearance())
    }

    pub fn active(&self) -> Arc<PreparedTheme> {
        self.snapshot.load().theme.clone()
    }
    fn appearance(&self) -> AppearanceDto {
        let snapshot = self.snapshot.load();
        AppearanceDto {
            available_themes: snapshot.catalogue.themes.keys().cloned().collect(),
            default_theme_id: self.default_id.clone(),
            selected_theme_id: snapshot.selected.clone(),
            active_theme_id: snapshot.theme.id.clone(),
            warning: snapshot.warning.clone(),
            theme_diagnostics: snapshot.catalogue.diagnostics.clone(),
        }
    }
    pub async fn get_appearance_with_perms<C: AdminCallerPort, A: AuthorizationEngine>(
        &self,
        caller_id: Uuid,
        users: &C,
        authorization: &A,
    ) -> Result<AppearanceDto, DomainError> {
        users.require_admin_caller(authorization, caller_id).await?;
        Ok(self.appearance())
    }
    pub async fn select_theme_with_perms<C: AdminCallerPort, A: AuthorizationEngine>(
        self: &Arc<Self>,
        caller_id: Uuid,
        id: String,
        users: &C,
        authorization: &A,
    ) -> Result<AppearanceDto, DomainError> {
        users.require_admin_caller(authorization, caller_id).await?;
        if ThemeId::parse(&id).is_err() || !self.snapshot.load().catalogue.themes.contains_key(&id)
        {
            tracing::info!(target: "audit", event = "appearance.selection_rejected", reason = "theme_unavailable", %caller_id, "Theme selection rejected");
            return Err(DomainError::validation_error(
                "Theme identifier is invalid or unavailable",
            ));
        }
        // The task owns the critical section even if the HTTP future is dropped.
        let service = self.clone();
        tokio::spawn(async move { service.select(caller_id, id).await })
            .await
            .map_err(|_| DomainError::internal_error("Theme", "Theme selection task failed"))?
    }
    async fn select(&self, caller_id: Uuid, id: String) -> Result<AppearanceDto, DomainError> {
        let _guard = self.mutation.lock().await;
        if self.force_default {
            tracing::info!(target: "audit", event = "appearance.selection_rejected", reason = "forced_default", %caller_id, "Theme selection disabled by recovery configuration");
            return Err(DomainError::validation_error(
                "Theme selection is disabled while recovery mode is active",
            ));
        }
        if !self.snapshot.load().catalogue.themes.contains_key(&id) {
            tracing::info!(target: "audit", event = "appearance.selection_rejected", reason = "theme_unavailable", %caller_id, "Theme removed during selection");
            return Err(DomainError::validation_error("Theme is unavailable"));
        }
        let saved = self.repository.get(KEY).await?;
        let next =
            self.resolve_snapshot(self.snapshot.load().catalogue.clone(), Some(id.clone()))?;
        if saved.as_deref() == Some(&id) {
            self.snapshot.store(Arc::new(next));
            return Ok(self.appearance());
        }
        if let Err(error) = self
            .repository
            .set(KEY, &id, "appearance", false, Some(caller_id))
            .await
        {
            tracing::error!(event = "appearance.persistence_failed", %caller_id, "Theme write failed; database outcome may be indeterminate, previous published snapshot retained");
            return Err(error);
        }
        self.snapshot.store(Arc::new(next));
        tracing::info!(target: "audit", event = "appearance.changed", reason = "admin_selection", %caller_id, theme_id = %id, "Global theme selection changed");
        Ok(self.appearance())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        application::ports::theme_ports::ThemeCatalogue,
        domain::entities::user::{UserFlags, UserRole},
        infrastructure::services::pg_acl_engine::PgAclEngine,
    };
    use std::{
        collections::HashMap,
        sync::{
            Mutex as StdMutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
    };
    #[derive(Default)]
    struct Repository {
        saved: StdMutex<Option<String>>,
        by: StdMutex<Option<Uuid>>,
        fail: AtomicBool,
        fail_read: AtomicBool,
        commit_then_fail: AtomicBool,
        reads: AtomicUsize,
        writes: AtomicUsize,
        pause: AtomicBool,
        started: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }
    impl SettingsRepository for Repository {
        async fn get(&self, _: &str) -> Result<Option<String>, DomainError> {
            if self.fail_read.load(Ordering::Relaxed) {
                return Err(DomainError::internal_error("Theme", "read failed"));
            }
            self.reads.fetch_add(1, Ordering::Relaxed);
            Ok(self.saved.lock().unwrap().clone())
        }
        async fn get_by_category(&self, _: &str) -> Result<HashMap<String, String>, DomainError> {
            unimplemented!()
        }
        async fn set(
            &self,
            _: &str,
            value: &str,
            category: &str,
            secret: bool,
            by: Option<Uuid>,
        ) -> Result<(), DomainError> {
            assert_eq!(category, "appearance");
            assert!(!secret);
            if self.pause.load(Ordering::Relaxed) {
                self.started.notify_one();
                self.release.notified().await;
            }
            if self.fail.load(Ordering::Relaxed) {
                return Err(DomainError::internal_error("Theme", "write failed"));
            }
            self.writes.fetch_add(1, Ordering::Relaxed);
            *self.saved.lock().unwrap() = Some(value.into());
            *self.by.lock().unwrap() = by;
            if self.commit_then_fail.load(Ordering::Relaxed) {
                return Err(DomainError::internal_error(
                    "Theme",
                    "Commit acknowledgement lost",
                ));
            }
            Ok(())
        }
        async fn delete(&self, _: &str) -> Result<(), DomainError> {
            unimplemented!()
        }
    }
    struct Caller(UserFlags);
    impl AdminCallerPort for Caller {
        async fn admin_caller_flags(&self, _: Uuid) -> Result<UserFlags, DomainError> {
            Ok(self.0)
        }
    }
    fn admin() -> Caller {
        Caller(UserFlags {
            role: UserRole::Admin,
            is_external: false,
            active: true,
            force_password_change: false,
        })
    }
    fn catalogue() -> ThemeCatalogue {
        ThemeCatalogue {
            themes: ["default", "sober", "blue"]
                .into_iter()
                .map(|id| {
                    (
                        id.into(),
                        Arc::new(
                            crate::infrastructure::services::theme_assets::prepare_theme(
                                id.into(),
                                format!(
                                    ":root {{ --color-accent: {}; }}",
                                    if id == "sober" { "red" } else { "blue" }
                                ),
                            ),
                        ),
                    )
                })
                .collect(),
            diagnostics: Vec::new(),
        }
    }
    async fn service(repo: Arc<Repository>, default: &str) -> Arc<ThemeService<Repository>> {
        Arc::new(
            ThemeService::new(repo, catalogue(), default.into())
                .await
                .unwrap(),
        )
    }
    #[tokio::test]
    async fn public_routes_are_cached_and_respect_subpath_and_missing_assets() {
        use axum::{
            Router,
            body::{Body, to_bytes},
            http::Request,
            response::Html,
        };
        use tower::ServiceExt;
        let repo = Arc::new(Repository::default());
        let svc = service(repo.clone(), "default").await;
        let reads = repo.reads.load(Ordering::Relaxed);
        // Regression pin: registering /themes separately from the nested /
        // route panicked at boot. Exercise the production mount, including
        // the bare namespace and its slash form, beneath a deployment prefix.
        let routes: Router = crate::interfaces::web::theme::mount(Router::new(), svc);
        let app: Router = Router::new()
            .nest("/cloud", routes)
            .fallback(|| async { Html("SPA shell") })
            .layer(tower_http::compression::CompressionLayer::new());
        for method in ["GET", "HEAD", "GET"] {
            let res = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri("/cloud/themes/active.css")
                        .header("Accept-Encoding", "gzip")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), axum::http::StatusCode::OK);
            assert_eq!(res.headers()["Content-Encoding"], "gzip");
            let body = to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
            assert_eq!(body.is_empty(), method == "HEAD");
        }
        for path in [
            "/cloud/themes",
            "/cloud/themes/",
            "/cloud/themes/missing.css",
        ] {
            let res = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(res.status(), axum::http::StatusCode::NOT_FOUND, "{path}");
        }
        assert_eq!(repo.reads.load(Ordering::Relaxed), reads);
    }
    #[tokio::test]
    async fn refresh_discovers_changes_and_keeps_the_snapshot_on_io_failure() {
        let root = tempfile::tempdir().unwrap();
        let loader = Arc::new(crate::infrastructure::services::theme_loader::ThemeLoader {
            config: crate::common::config::ThemesConfig {
                path: root.path().to_owned(),
                path_explicit: true,
                default_theme: "default".into(),
                force_default: false,
            },
        });
        let repo = Arc::new(Repository::default());
        let svc = Arc::new(
            ThemeService::new(repo.clone(), loader.load().unwrap(), "default".into())
                .await
                .unwrap()
                .with_loader(loader),
        );
        let authz = PgAclEngine::new_stub();
        let caller = Uuid::new_v4();
        std::fs::create_dir(root.path().join("new")).unwrap();
        let file = root.path().join("new/theme.css");
        std::fs::write(&file, ":root { --color-accent: #123456; }").unwrap();
        let denied = Caller(UserFlags {
            role: UserRole::User,
            ..admin().0
        });
        assert!(
            svc.reload_with_perms(caller, &denied, &authz)
                .await
                .is_err()
        );
        assert!(!svc.appearance().available_themes.contains(&"new".into()));
        assert!(
            svc.reload_with_perms(caller, &admin(), &authz)
                .await
                .unwrap()
                .available_themes
                .contains(&"new".into())
        );
        svc.select_theme_with_perms(caller, "new".into(), &admin(), &authz)
            .await
            .unwrap();
        let etag = svc.active().etag.clone();
        std::fs::write(&file, ":root { --color-accent: #abcdef; }").unwrap();
        svc.reload_with_perms(caller, &admin(), &authz)
            .await
            .unwrap();
        assert_ne!(etag, svc.active().etag);
        // Regression pin: a failed rescan must retain both catalogue and CSS,
        // rather than publishing a partial or empty replacement.
        let prior = svc.active();
        std::fs::write(&file, [0xff]).unwrap();
        assert!(
            svc.reload_with_perms(caller, &admin(), &authz)
                .await
                .is_err()
        );
        assert!(Arc::ptr_eq(&prior, &svc.active()));
        std::fs::remove_dir_all(root.path()).unwrap();
        assert!(
            svc.reload_with_perms(caller, &admin(), &authz)
                .await
                .is_err()
        );
        assert!(Arc::ptr_eq(&prior, &svc.active()));
        assert_eq!(svc.appearance().selected_theme_id.as_deref(), Some("new"));
    }
    #[tokio::test]
    async fn database_read_errors_are_not_missing_settings() {
        let repo = Arc::new(Repository::default());
        repo.fail_read.store(true, Ordering::Relaxed);
        assert!(
            ThemeService::new(repo, catalogue(), "default".into())
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn uncertain_commit_and_retry_do_not_publish_a_failed_selection() {
        // Regression pin: PostgreSQL can commit even when its acknowledgement
        // is lost. Keep serving the previous snapshot until a confirmed retry.
        let repo = Arc::new(Repository::default());
        let svc = service(repo.clone(), "default").await;
        repo.commit_then_fail.store(true, Ordering::Relaxed);
        let previous = svc.active();
        let caller = Uuid::new_v4();
        let authz = PgAclEngine::new_stub();
        assert!(
            svc.select_theme_with_perms(caller, "sober".into(), &admin(), &authz)
                .await
                .is_err()
        );
        assert!(Arc::ptr_eq(&previous, &svc.active()));
        assert_eq!(repo.saved.lock().unwrap().as_deref(), Some("sober"));
        repo.commit_then_fail.store(false, Ordering::Relaxed);
        svc.select_theme_with_perms(caller, "sober".into(), &admin(), &authz)
            .await
            .unwrap();
        assert_eq!(svc.active().id, "sober");
        assert_eq!(repo.writes.load(Ordering::Relaxed), 1);
    }
    #[tokio::test]
    async fn failed_selection_does_not_publish_an_unrelated_database_choice() {
        // Regression pin: the old pre-write reconciliation published a different
        // saved choice even when the requested mutation subsequently failed.
        let repo = Arc::new(Repository::default());
        let svc = service(repo.clone(), "default").await;
        *repo.saved.lock().unwrap() = Some("blue".into());
        repo.fail.store(true, Ordering::Relaxed);
        let previous = svc.active();
        assert!(
            svc.select_theme_with_perms(
                Uuid::new_v4(),
                "sober".into(),
                &admin(),
                &PgAclEngine::new_stub()
            )
            .await
            .is_err()
        );
        assert!(Arc::ptr_eq(&previous, &svc.active()));
    }
    #[tokio::test]
    async fn recovery_preserves_the_saved_choice_and_disables_selection() {
        let repo = Arc::new(Repository::default());
        *repo.saved.lock().unwrap() = Some("sober".into());
        let svc = Arc::new(
            ThemeService::new(repo.clone(), catalogue(), "default".into())
                .await
                .unwrap()
                .with_forced_default(true),
        );
        assert_eq!(svc.active().id, "default");
        assert_eq!(svc.appearance().warning.as_deref(), Some("forced_default"));
        assert_eq!(svc.appearance().selected_theme_id.as_deref(), Some("sober"));
        assert!(
            svc.select_theme_with_perms(
                Uuid::new_v4(),
                "blue".into(),
                &admin(),
                &PgAclEngine::new_stub()
            )
            .await
            .is_err()
        );
        assert_eq!(repo.saved.lock().unwrap().as_deref(), Some("sober"));
    }
    #[tokio::test]
    async fn priority_fallback_and_restart_preserve_saved_choice() {
        let repo = Arc::new(Repository::default());
        let svc = service(repo.clone(), "sober").await;
        assert_eq!(svc.active().id, "sober");
        let caller = Uuid::new_v4();
        let authz = PgAclEngine::new_stub();
        svc.select_theme_with_perms(caller, "blue".into(), &admin(), &authz)
            .await
            .unwrap();
        assert_eq!(*repo.by.lock().unwrap(), Some(caller));
        assert_eq!(service(repo.clone(), "sober").await.active().id, "blue");
        *repo.saved.lock().unwrap() = Some("removed".into());
        let fallback = service(repo.clone(), "sober").await;
        assert_eq!(fallback.active().id, "default");
        assert_eq!(
            fallback.appearance().warning.as_deref(),
            Some("selected_theme_unavailable")
        );
        assert_eq!(repo.saved.lock().unwrap().as_deref(), Some("removed"));
        assert!(
            ThemeService::new(repo, catalogue(), "missing".into())
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn permissions_and_failed_changes_preserve_public_bytes() {
        let repo = Arc::new(Repository::default());
        let svc = service(repo.clone(), "default").await;
        let authz = PgAclEngine::new_stub();
        let caller = Uuid::new_v4();
        for flags in [
            UserFlags {
                role: UserRole::User,
                ..admin().0
            },
            UserFlags {
                is_external: true,
                ..admin().0
            },
            UserFlags {
                active: false,
                ..admin().0
            },
        ] {
            let user = Caller(flags);
            assert!(
                svc.get_appearance_with_perms(caller, &user, &authz)
                    .await
                    .is_err()
            );
            assert!(
                svc.select_theme_with_perms(caller, "sober".into(), &user, &authz)
                    .await
                    .is_err()
            );
        }
        assert!(
            svc.select_theme_with_perms(caller, "unknown".into(), &admin(), &authz)
                .await
                .is_err()
        );
        let previous = svc.active();
        repo.fail.store(true, Ordering::Relaxed);
        assert!(
            svc.select_theme_with_perms(caller, "sober".into(), &admin(), &authz)
                .await
                .is_err()
        );
        assert!(Arc::ptr_eq(&previous, &svc.active()));
        let reads = repo.reads.load(Ordering::Relaxed);
        for _ in 0..100 {
            svc.active();
        }
        assert_eq!(repo.reads.load(Ordering::Relaxed), reads);
    }
    #[tokio::test]
    async fn concurrent_changes_and_repetition_agree_with_storage() {
        let repo = Arc::new(Repository::default());
        let svc = service(repo.clone(), "default").await;
        let authz = PgAclEngine::new_stub();
        let user = admin();
        let caller = Uuid::new_v4();
        let (a, b) = tokio::join!(
            svc.select_theme_with_perms(caller, "sober".into(), &user, &authz),
            svc.select_theme_with_perms(caller, "blue".into(), &user, &authz)
        );
        a.unwrap();
        b.unwrap();
        let active = svc.active().id.clone();
        assert_eq!(repo.saved.lock().unwrap().as_deref(), Some(active.as_str()));
        let writes = repo.writes.load(Ordering::Relaxed);
        svc.select_theme_with_perms(caller, active, &user, &authz)
            .await
            .unwrap();
        assert_eq!(repo.writes.load(Ordering::Relaxed), writes);
    }
    #[tokio::test]
    async fn abandoned_http_future_does_not_abandon_the_committed_selection() {
        // Regression pin: request cancellation during SQL must not leave the DB
        // on one theme and the in-memory public stylesheet on another.
        let repo = Arc::new(Repository::default());
        repo.pause.store(true, Ordering::Relaxed);
        let svc = service(repo.clone(), "default").await;
        let task_svc = svc.clone();
        let request = tokio::spawn(async move {
            task_svc
                .select_theme_with_perms(
                    Uuid::new_v4(),
                    "sober".into(),
                    &admin(),
                    &PgAclEngine::new_stub(),
                )
                .await
        });
        repo.started.notified().await;
        assert_eq!(svc.active().id, "default");
        request.abort();
        repo.release.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while svc.active().id != "sober" {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(repo.saved.lock().unwrap().as_deref(), Some("sober"));
    }
}

#[cfg(all(test, integration_tests))]
mod integration_tests {
    use super::*;
    use crate::infrastructure::repositories::pg::SettingsPgRepository;
    use std::collections::HashMap;
    const TEST_KEY: &str = "rust-test-theme-selection";
    // An isolated settings key exercises the real repository without changing
    // the global selection used by concurrently running integration scenarios.
    struct IsolatedSettings(SettingsPgRepository);
    impl SettingsRepository for IsolatedSettings {
        async fn get(&self, _: &str) -> Result<Option<String>, DomainError> {
            self.0.get(TEST_KEY).await
        }
        async fn get_by_category(
            &self,
            category: &str,
        ) -> Result<HashMap<String, String>, DomainError> {
            self.0.get_by_category(category).await
        }
        async fn set(
            &self,
            _: &str,
            value: &str,
            category: &str,
            secret: bool,
            by: Option<Uuid>,
        ) -> Result<(), DomainError> {
            self.0.set(TEST_KEY, value, category, secret, by).await
        }
        async fn delete(&self, _: &str) -> Result<(), DomainError> {
            self.0.delete(TEST_KEY).await
        }
    }
    #[tokio::test]
    async fn theme_selection_survives_service_reconstruction_in_postgres() {
        let pool = Arc::new(
            sqlx::postgres::PgPoolOptions::new()
                .max_connections(2)
                .connect(&crate::integration_test_support::test_db_url())
                .await
                .unwrap(),
        );
        let user = Uuid::parse_str("9899b1ea-3264-4668-b048-288503ec2039").unwrap();
        sqlx::query("DELETE FROM auth.admin_settings WHERE key = $1")
            .bind(TEST_KEY)
            .execute(pool.as_ref())
            .await
            .unwrap();
        sqlx::query("DELETE FROM auth.users WHERE id = $1")
            .bind(user)
            .execute(pool.as_ref())
            .await
            .unwrap();
        sqlx::query("INSERT INTO auth.users (id, username, email, password_hash, role) VALUES ($1, 'rust-test-theme-admin', 'rust-test-theme-admin@example.com', '', 'admin')").bind(user).execute(pool.as_ref()).await.unwrap();
        let repo = Arc::new(IsolatedSettings(SettingsPgRepository::new(pool.clone())));
        let catalogue = || ThemeCatalogue {
            themes: ["default", "sober"]
                .into_iter()
                .map(|id| {
                    (
                        id.into(),
                        Arc::new(
                            crate::infrastructure::services::theme_assets::prepare_theme(
                                id.into(),
                                ":root {}".into(),
                            ),
                        ),
                    )
                })
                .collect(),
            diagnostics: Vec::new(),
        };
        let service = ThemeService::new(repo.clone(), catalogue(), "default".into())
            .await
            .unwrap();
        service.select(user, "sober".into()).await.unwrap();
        let rebuilt = ThemeService::new(repo.clone(), catalogue(), "default".into())
            .await
            .unwrap();
        assert_eq!(rebuilt.active().id, "sober");
        let updated_by: Option<Uuid> =
            sqlx::query_scalar("SELECT updated_by FROM auth.admin_settings WHERE key = $1")
                .bind(TEST_KEY)
                .fetch_one(pool.as_ref())
                .await
                .unwrap();
        assert_eq!(updated_by, Some(user));
        repo.delete(TEST_KEY).await.unwrap();
        sqlx::query("DELETE FROM auth.users WHERE id = $1")
            .bind(user)
            .execute(pool.as_ref())
            .await
            .unwrap();
    }
}
