//! Enqueues a `link_events` row per grant that expired since the stored watermark (docs/plan/links.md § Expiry sweep).

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::application::services::share_mount_service::ShareMountService;
use crate::domain::repositories::settings_repository::SettingsRepository;
use crate::infrastructure::repositories::pg::LinkEventsPgRepository;
use crate::infrastructure::scheduler::{JobHandler, JobOutcome, JobRunArgs, Mutates};

pub const LINKS_EXPIRY_SWEEP_JOB_NAME: &str = "links_expiry_sweep";
const WATERMARK_KEY: &str = "links.expiry_watermark";

pub struct LinksExpirySweepJob<S: SettingsRepository> {
    events: Arc<LinkEventsPgRepository>,
    settings: Arc<S>,
    svc: Option<Arc<ShareMountService>>,
}

impl<S: SettingsRepository> LinksExpirySweepJob<S> {
    pub fn new(
        events: Arc<LinkEventsPgRepository>,
        settings: Arc<S>,
        svc: Option<Arc<ShareMountService>>,
    ) -> Self {
        Self {
            events,
            settings,
            svc,
        }
    }

    async fn sweep(&self) -> Result<u64, String> {
        let from = match self.settings.get(WATERMARK_KEY).await {
            Ok(Some(v)) => match DateTime::parse_from_rfc3339(&v) {
                Ok(t) => Some(t.with_timezone(&Utc)),
                Err(e) => {
                    tracing::warn!(
                        target: "oxicloud::links",
                        value = %v,
                        error = %e,
                        "unparsable expiry watermark; using the default window",
                    );
                    None
                }
            },
            Ok(None) => None,
            Err(e) => return Err(format!("read watermark: {e}")),
        };
        let (n, to) = self
            .events
            .enqueue_expired_since(from)
            .await
            .map_err(|e| format!("enqueue: {e}"))?;
        self.settings
            .set(WATERMARK_KEY, &to.to_rfc3339(), "links", false, None)
            .await
            .map_err(|e| format!("write watermark: {e}"))?;
        Ok(n)
    }
}

#[async_trait]
impl<S: SettingsRepository> JobHandler for LinksExpirySweepJob<S> {
    fn name(&self) -> &str {
        LINKS_EXPIRY_SWEEP_JOB_NAME
    }

    fn description(&self) -> &'static str {
        "Queues link events for grants that expired since the last sweep so their links are removed."
    }

    fn mutates(&self) -> Mutates {
        Mutates::Always
    }

    async fn run(&self, _args: &JobRunArgs) -> JobOutcome {
        match self.sweep().await {
            Ok(n) => {
                if n > 0 {
                    tracing::info!(target: "oxicloud::links", enqueued = n, "expired grants queued");
                    if let Some(svc) = &self.svc {
                        svc.poke();
                    }
                }
                JobOutcome::ok(n)
            }
            Err(msg) => JobOutcome::err(format!("links_expiry_sweep: {msg}")),
        }
    }
}

#[cfg(all(test, integration_tests))]
mod it {
    use super::*;
    use crate::infrastructure::repositories::pg::SettingsPgRepository;
    use crate::infrastructure::repositories::pg::share_mount_pg_repository::trigger_tests::make_user_with_drive;
    use crate::infrastructure::services::links_settle_job::it::{Fx, fixture, grant};
    use crate::mount_it_support::{fresh_db, provision_folder};
    use sqlx::PgPool;
    use uuid::Uuid;

    fn sweep_job(fx: &Fx) -> LinksExpirySweepJob<SettingsPgRepository> {
        LinksExpirySweepJob::new(
            fx.events.clone(),
            Arc::new(SettingsPgRepository::new(fx.pool.clone())),
            None,
        )
    }

    async fn set_expiry(pool: &PgPool, user: Uuid, folder: Uuid, sql_interval: &str) {
        sqlx::query(&format!(
            "UPDATE storage.role_grants SET expires_at = now() + interval '{sql_interval}' \
              WHERE subject_id = $1 AND resource_id = $2"
        ))
        .bind(user)
        .bind(folder)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn run_count(job: &LinksExpirySweepJob<SettingsPgRepository>) -> u64 {
        match job.run(&JobRunArgs::default()).await {
            JobOutcome::Ok { count, .. } => count,
            other => panic!("sweep failed: {other:?}"),
        }
    }

    #[tokio::test]
    async fn sweep_enqueues_only_grants_expired_since_watermark_and_advances_it() {
        let (_c, pool) = fresh_db().await;
        let fx = fixture(&pool);
        let alice = provision_folder(&pool, "alice", "F").await;
        let f = alice.mount_folder_id;
        let (u1, u2, u3) = (
            make_user_with_drive(&pool, "u1").await,
            make_user_with_drive(&pool, "u2").await,
            make_user_with_drive(&pool, "u3").await,
        );
        for u in [u1, u2, u3] {
            grant(&pool, "user", u, "folder", f, alice.owner_id).await;
        }
        set_expiry(&pool, u1, f, "-1 hour").await;
        set_expiry(&pool, u2, f, "1 hour").await;
        set_expiry(&pool, u3, f, "-40 days").await;
        fx.clear_events().await;
        let job = sweep_job(&fx);
        assert_eq!(run_count(&job).await, 1);
        assert_eq!(fx.events.pending().await.unwrap(), 1);
        assert_eq!(fx.events.next().await.unwrap().unwrap().subject_id, u1);
        assert_eq!(run_count(&job).await, 0);
        assert_eq!(fx.events.pending().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn sweep_then_settle_deletes_link_but_not_a_decline_of_a_live_grant() {
        let (_c, pool) = fresh_db().await;
        let fx = fixture(&pool);
        let alice = provision_folder(&pool, "alice", "F").await;
        let f = alice.mount_folder_id;
        let bob = make_user_with_drive(&pool, "bob").await;
        grant(&pool, "user", bob, "folder", f, alice.owner_id).await;
        fx.run_ok().await;
        assert!(fx.linked(bob, f).await);
        sqlx::query(
            "UPDATE storage.role_grants SET expires_at = now() + interval '1 second' WHERE subject_id = $1",
        )
        .bind(bob)
        .execute(&*pool)
        .await
        .unwrap();
        fx.run_ok().await;
        assert!(fx.linked(bob, f).await);
        sqlx::query(
            "UPDATE storage.role_grants SET expires_at = now() - interval '1 hour' WHERE subject_id = $1",
        )
        .bind(bob)
        .execute(&*pool)
        .await
        .unwrap();
        fx.run_ok().await;
        assert!(!fx.linked(bob, f).await);

        // a decline of a still-live grant survives the sweep
        let carol = make_user_with_drive(&pool, "carol").await;
        grant(&pool, "user", carol, "folder", f, alice.owner_id).await;
        fx.run_ok().await;
        fx.repo.insert_decline(carol, f).await.unwrap();
        let job = sweep_job(&fx);
        run_count(&job).await;
        fx.run_ok().await;
        assert!(fx.repo.is_declined(carol, f).await.unwrap());
    }

    #[tokio::test]
    async fn renewal_after_sweep_links_again() {
        let (_c, pool) = fresh_db().await;
        let fx = fixture(&pool);
        let alice = provision_folder(&pool, "alice", "F").await;
        let f = alice.mount_folder_id;
        let bob = make_user_with_drive(&pool, "bob").await;
        grant(&pool, "user", bob, "folder", f, alice.owner_id).await;
        set_expiry(&pool, bob, f, "-1 hour").await;
        fx.run_ok().await;
        assert!(!fx.linked(bob, f).await);
        let job = sweep_job(&fx);
        assert_eq!(run_count(&job).await, 1);
        fx.run_ok().await;
        assert!(!fx.linked(bob, f).await);
        set_expiry(&pool, bob, f, "1 day").await;
        fx.run_ok().await;
        assert!(fx.linked(bob, f).await);
    }

    #[tokio::test]
    async fn sweep_failure_keeps_watermark_and_fails() {
        let (_c, pool) = fresh_db().await;
        let fx = fixture(&pool);
        let job = sweep_job(&fx);
        sqlx::query("ALTER TABLE storage.link_events RENAME TO le_gone")
            .execute(&*pool)
            .await
            .unwrap();
        let out = job.run(&JobRunArgs::default()).await;
        assert!(matches!(out, JobOutcome::Err { .. }), "{out:?}");
        let wm = SettingsPgRepository::new(pool.clone())
            .get(WATERMARK_KEY)
            .await
            .unwrap();
        assert_eq!(wm, None);
    }

    #[tokio::test]
    async fn sweep_uses_database_clock_for_window_and_watermark() {
        let (_c, pool) = fresh_db().await;
        let fx = fixture(&pool);
        let alice = provision_folder(&pool, "alice", "F").await;
        let f = alice.mount_folder_id;
        let bob = make_user_with_drive(&pool, "bob").await;
        grant(&pool, "user", bob, "folder", f, alice.owner_id).await;
        sqlx::query("UPDATE storage.role_grants SET expires_at = now() + interval '1500 milliseconds' WHERE subject_id = $1 AND resource_id = $2")
            .bind(bob)
            .bind(f)
            .execute(&*pool)
            .await
            .unwrap();
        fx.clear_events().await;
        let job = sweep_job(&fx);
        assert_eq!(run_count(&job).await, 0);
        let wm: String = SettingsPgRepository::new(pool.clone())
            .get(WATERMARK_KEY)
            .await
            .unwrap()
            .unwrap();
        let wm = DateTime::parse_from_rfc3339(&wm)
            .unwrap()
            .with_timezone(&Utc);
        let db_now: DateTime<Utc> = sqlx::query_scalar("SELECT now()")
            .fetch_one(&*pool)
            .await
            .unwrap();
        assert!(wm <= db_now);
        tokio::time::sleep(std::time::Duration::from_millis(1800)).await;
        assert_eq!(run_count(&job).await, 1);
        assert_eq!(fx.events.pending().await.unwrap(), 1);
    }
}
