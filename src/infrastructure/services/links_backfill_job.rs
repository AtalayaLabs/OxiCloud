//! `links_backfill` (off -> auto switch, repair) and `links_drop` (leaving auto for good); docs/plan/links.md § Mode changes.

use std::sync::Arc;

use async_trait::async_trait;

use crate::application::services::share_mount_service::ShareMountService;
use crate::common::config::LinksMode;
use crate::domain::repositories::settings_repository::SettingsRepository;
use crate::infrastructure::repositories::pg::{LinkEventsPgRepository, ShareMountPgRepository};
use crate::infrastructure::scheduler::{JobHandler, JobOutcome, JobRunArgs, Mutates};

pub const LINKS_BACKFILL_JOB_NAME: &str = "links_backfill";
pub const LINKS_DROP_JOB_NAME: &str = "links_drop";
pub const MODE_APPLIED_KEY: &str = "links.mode_applied";

pub struct LinksBackfillJob<S: SettingsRepository> {
    events: Arc<LinkEventsPgRepository>,
    settings: Arc<S>,
    mode: LinksMode,
    svc: Option<Arc<ShareMountService>>,
}

impl<S: SettingsRepository> LinksBackfillJob<S> {
    pub fn new(
        events: Arc<LinkEventsPgRepository>,
        settings: Arc<S>,
        mode: LinksMode,
        svc: Option<Arc<ShareMountService>>,
    ) -> Self {
        Self {
            events,
            settings,
            mode,
            svc,
        }
    }
}

#[async_trait]
impl<S: SettingsRepository> JobHandler for LinksBackfillJob<S> {
    fn name(&self) -> &str {
        LINKS_BACKFILL_JOB_NAME
    }

    fn description(&self) -> &'static str {
        "Queues a link event for every current grant so each recipient gets a link (run on switching links to auto; also the repair action)."
    }

    fn mutates(&self) -> Mutates {
        Mutates::Always
    }

    async fn run(&self, _args: &JobRunArgs) -> JobOutcome {
        if self.mode != LinksMode::Auto {
            return JobOutcome::err("links_backfill requires OXICLOUD_LINKS_MODE=auto");
        }
        let n = match self.events.enqueue_all_current_grants().await {
            Ok(n) => n,
            Err(e) => return JobOutcome::err(format!("links_backfill: enqueue: {e}")),
        };
        if let Err(e) = self
            .settings
            .set(MODE_APPLIED_KEY, self.mode.as_str(), "links", false, None)
            .await
        {
            return JobOutcome::err(format!("links_backfill: record mode_applied: {e}"));
        }
        tracing::info!(target: "oxicloud::links", enqueued = n, "link backfill queued");
        if n > 0
            && let Some(svc) = &self.svc
        {
            svc.poke();
        }
        JobOutcome::ok(n)
    }
}

pub struct LinksDropJob {
    repo: Arc<ShareMountPgRepository>,
    mode: LinksMode,
}

impl LinksDropJob {
    pub fn new(repo: Arc<ShareMountPgRepository>, mode: LinksMode) -> Self {
        Self { repo, mode }
    }
}

#[async_trait]
impl JobHandler for LinksDropJob {
    fn name(&self) -> &str {
        LINKS_DROP_JOB_NAME
    }

    fn description(&self) -> &'static str {
        "Deletes ALL share links and ALL link declines; only runs with OXICLOUD_LINKS_MODE=off."
    }

    fn mutates(&self) -> Mutates {
        Mutates::Always
    }

    async fn run(&self, _args: &JobRunArgs) -> JobOutcome {
        if self.mode != LinksMode::Off {
            return JobOutcome::err("links_drop requires OXICLOUD_LINKS_MODE=off");
        }
        match self.repo.drop_all_links_and_declines().await {
            Ok((links, declines)) => {
                tracing::info!(
                    target: "audit",
                    event = "links.dropped",
                    links = links,
                    declines = declines,
                    "🗑️ all share links and declines dropped",
                );
                JobOutcome::ok(links + declines)
            }
            Err(e) => JobOutcome::err(format!("links_drop: {e}")),
        }
    }
}

#[cfg(all(test, integration_tests))]
mod it {
    use super::*;
    use crate::infrastructure::repositories::pg::SettingsPgRepository;
    use crate::infrastructure::repositories::pg::share_mount_pg_repository::trigger_tests::{
        make_shared_drive, make_user_with_drive,
    };
    use crate::infrastructure::services::links_settle_job::it::{Fx, fixture, grant};
    use crate::mount_it_support::{fresh_db, provision_folder};

    fn backfill(fx: &Fx, mode: LinksMode) -> LinksBackfillJob<SettingsPgRepository> {
        LinksBackfillJob::new(
            fx.events.clone(),
            Arc::new(SettingsPgRepository::new(fx.pool.clone())),
            mode,
            None,
        )
    }

    async fn run_count<J: JobHandler>(job: &J) -> u64 {
        match job.run(&JobRunArgs::default()).await {
            JobOutcome::Ok { count, .. } => count,
            other => panic!("job failed: {other:?}"),
        }
    }

    #[tokio::test]
    async fn backfill_enqueues_every_current_grant_and_marks_mode_applied() {
        let (_c, pool) = fresh_db().await;
        let fx = fixture(&pool);
        let alice = provision_folder(&pool, "alice", "F").await;
        let f = alice.mount_folder_id;
        let (u1, u2, u3) = (
            make_user_with_drive(&pool, "u1").await,
            make_user_with_drive(&pool, "u2").await,
            make_user_with_drive(&pool, "u3").await,
        );
        let (drive, _) = make_shared_drive(&pool, "Team", alice.owner_id).await;
        grant(&pool, "user", u1, "folder", f, alice.owner_id).await;
        grant(&pool, "user", u2, "folder", f, alice.owner_id).await;
        grant(&pool, "user", u3, "drive", drive, alice.owner_id).await;
        sqlx::query(
            "UPDATE storage.role_grants SET expires_at = now() - interval '1 hour' \
              WHERE subject_id = $1 AND resource_id = $2",
        )
        .bind(u2)
        .bind(f)
        .execute(&*pool)
        .await
        .unwrap();
        let grants: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM storage.role_grants WHERE subject_type IN ('user','group') \
               AND resource_type IN ('folder','drive') AND (expires_at IS NULL OR expires_at > now())",
        )
        .fetch_one(&*pool)
        .await
        .unwrap();
        fx.clear_events().await;
        let job = backfill(&fx, LinksMode::Auto);
        assert_eq!(run_count(&job).await as i64, grants);
        assert_eq!(fx.events.pending().await.unwrap() as i64, grants);
        let applied = SettingsPgRepository::new(pool.clone())
            .get(MODE_APPLIED_KEY)
            .await
            .unwrap();
        assert_eq!(applied.as_deref(), Some("auto"));
    }

    #[tokio::test]
    async fn backfill_refuses_in_off_and_records_nothing() {
        let (_c, pool) = fresh_db().await;
        let fx = fixture(&pool);
        let out = backfill(&fx, LinksMode::Off)
            .run(&JobRunArgs::default())
            .await;
        assert!(matches!(out, JobOutcome::Err { .. }), "{out:?}");
        let applied = SettingsPgRepository::new(pool.clone())
            .get(MODE_APPLIED_KEY)
            .await
            .unwrap();
        assert_eq!(applied, None);
    }

    #[tokio::test]
    async fn backfill_is_idempotent() {
        let (_c, pool) = fresh_db().await;
        let fx = fixture(&pool);
        let alice = provision_folder(&pool, "alice", "F").await;
        let f = alice.mount_folder_id;
        let bob = make_user_with_drive(&pool, "bob").await;
        let carol = make_user_with_drive(&pool, "carol").await;
        grant(&pool, "user", bob, "folder", f, alice.owner_id).await;
        grant(&pool, "user", carol, "folder", f, alice.owner_id).await;
        fx.repo.insert_decline(carol, f).await.unwrap();
        fx.run_ok().await;
        let job = backfill(&fx, LinksMode::Auto);
        for _ in 0..2 {
            run_count(&job).await;
            fx.run_ok().await;
        }
        let links: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM storage.folders WHERE mount_target_id = $1")
                .bind(f)
                .fetch_one(&*pool)
                .await
                .unwrap();
        assert_eq!(links, 1);
        assert!(fx.linked(bob, f).await);
        assert!(!fx.linked(carol, f).await);
        assert!(fx.repo.is_declined(carol, f).await.unwrap());
    }

    #[tokio::test]
    async fn drop_refuses_in_auto_and_deletes_in_off() {
        let (_c, pool) = fresh_db().await;
        let fx = fixture(&pool);
        let alice = provision_folder(&pool, "alice", "F").await;
        let f = alice.mount_folder_id;
        let bob = make_user_with_drive(&pool, "bob").await;
        let carol = make_user_with_drive(&pool, "carol").await;
        grant(&pool, "user", bob, "folder", f, alice.owner_id).await;
        fx.run_ok().await;
        fx.repo.insert_decline(carol, f).await.unwrap();
        assert!(fx.linked(bob, f).await);

        let auto = LinksDropJob::new(fx.repo.clone(), LinksMode::Auto);
        let out = auto.run(&JobRunArgs::default()).await;
        assert!(matches!(out, JobOutcome::Err { .. }), "{out:?}");
        assert!(fx.linked(bob, f).await);

        let off = LinksDropJob::new(fx.repo.clone(), LinksMode::Off);
        assert_eq!(run_count(&off).await, 2);
        assert!(!fx.linked(bob, f).await);
        assert!(!fx.repo.is_declined(carol, f).await.unwrap());
        let real: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM storage.folders WHERE id = $1 AND mount_target_id IS NULL",
        )
        .bind(f)
        .fetch_one(&*pool)
        .await
        .unwrap();
        assert_eq!(real, 1);
    }
}
