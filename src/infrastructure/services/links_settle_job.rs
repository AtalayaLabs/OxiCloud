//! Drains `storage.link_events` by settling every (recipient, target) pair an event names (docs/plan/links.md § When links are created).

use std::sync::Arc;

use async_trait::async_trait;
use uuid::Uuid;

use crate::application::services::share_mount_service::{SettleOutcome, ShareMountService};
use crate::common::errors::DomainError;
use crate::domain::services::authorization::Subject;
use crate::infrastructure::repositories::pg::{
    LinkEvent, LinkEventsPgRepository, ShareMountPgRepository,
};
use crate::infrastructure::scheduler::{JobHandler, JobOutcome, JobRunArgs, Mutates};

pub const LINKS_SETTLE_JOB_NAME: &str = "links_settle";

pub struct LinksSettleJob {
    svc: Arc<ShareMountService>,
    repo: Arc<ShareMountPgRepository>,
    events: Arc<LinkEventsPgRepository>,
    max_events_per_run: usize,
    checkpoint_every: usize,
}

#[derive(Default)]
struct Tally {
    pairs: u64,
    created: u64,
    deleted: u64,
    name_exhausted: u64,
}

impl LinksSettleJob {
    pub fn new(
        svc: Arc<ShareMountService>,
        repo: Arc<ShareMountPgRepository>,
        events: Arc<LinkEventsPgRepository>,
    ) -> Self {
        Self {
            svc,
            repo,
            events,
            max_events_per_run: 1000,
            checkpoint_every: 100,
        }
    }

    fn bad_event(ev: &LinkEvent) -> DomainError {
        DomainError::internal_error(
            "LinksSettle",
            format!(
                "event {}: unknown {}/{}",
                ev.id, ev.subject_type, ev.resource_type
            ),
        )
    }

    /// (recipients sorted by uuid, targets) of one event
    pub async fn expand(&self, ev: &LinkEvent) -> Result<(Vec<Uuid>, Vec<Uuid>), DomainError> {
        let subject = match ev.subject_type.as_str() {
            "user" => Subject::User(ev.subject_id),
            "group" => Subject::Group(ev.subject_id),
            _ => return Err(Self::bad_event(ev)),
        };
        let targets = match ev.resource_type.as_str() {
            "folder" | "drive" => {
                match self
                    .repo
                    .target_of(&ev.resource_type, ev.resource_id)
                    .await?
                {
                    Some(t) => vec![t],
                    None => return Ok((Vec::new(), Vec::new())),
                }
            }
            "group" => {
                let groups = self.repo.group_and_ancestors(ev.resource_id).await?;
                self.repo.grant_targets_of_groups(&groups).await?
            }
            _ => return Err(Self::bad_event(ev)),
        };
        let mut recipients = self.svc.users_of(subject).await?;
        recipients.sort_unstable();
        recipients.dedup();
        Ok((recipients, targets))
    }

    /// settles every pair after `ev.cursor`, checkpointing; Ok(pairs settled)
    pub async fn process(&self, ev: &LinkEvent) -> Result<u64, DomainError> {
        let mut tally = Tally::default();
        self.process_into(ev, &mut tally).await?;
        Ok(tally.pairs)
    }

    async fn process_into(&self, ev: &LinkEvent, tally: &mut Tally) -> Result<(), DomainError> {
        let (recipients, targets) = self.expand(ev).await?;
        let membership = ev.resource_type == "group";
        let mut since_checkpoint = 0;
        let mut last_done: Option<Uuid> = None;
        for recipient in recipients
            .into_iter()
            .filter(|r| ev.cursor.is_none_or(|c| *r > c))
        {
            if let Err(e) = self
                .settle_recipient(recipient, &targets, membership, tally)
                .await
            {
                if since_checkpoint > 0
                    && let Some(done) = last_done
                {
                    let _ = self.events.checkpoint(ev.id, done).await;
                }
                return Err(e);
            }
            last_done = Some(recipient);
            since_checkpoint += 1;
            if since_checkpoint >= self.checkpoint_every {
                self.events.checkpoint(ev.id, recipient).await?;
                since_checkpoint = 0;
            }
        }
        Ok(())
    }

    async fn settle_recipient(
        &self,
        recipient: Uuid,
        targets: &[Uuid],
        membership: bool,
        tally: &mut Tally,
    ) -> Result<(), DomainError> {
        let mut own;
        let targets = if membership {
            own = self.repo.link_and_decline_targets(recipient).await?;
            own.extend_from_slice(targets);
            own.sort_unstable();
            own.dedup();
            &own[..]
        } else {
            targets
        };
        for &target in targets {
            match self.svc.settle(recipient, target).await? {
                SettleOutcome::Created(_) => tally.created += 1,
                SettleOutcome::Deleted => tally.deleted += 1,
                SettleOutcome::NameExhausted => tally.name_exhausted += 1,
                SettleOutcome::Unchanged | SettleOutcome::Skipped(_) => {}
            }
            tally.pairs += 1;
        }
        Ok(())
    }
}

#[async_trait]
impl JobHandler for LinksSettleJob {
    fn name(&self) -> &str {
        LINKS_SETTLE_JOB_NAME
    }

    fn description(&self) -> &'static str {
        "Creates and removes share links for the (recipient, target) pairs named by queued grant and group-membership events."
    }

    fn mutates(&self) -> Mutates {
        Mutates::Always
    }

    async fn run(&self, _args: &JobRunArgs) -> JobOutcome {
        let mut tally = Tally::default();
        let (mut done, mut attempted, mut failed) = (0u64, 0usize, 0u64);
        let mut after = 0i64;
        let mut last_error: Option<String> = None;
        while attempted < self.max_events_per_run {
            let ev = match self.events.next_after(after).await {
                Ok(Some(ev)) => ev,
                Ok(None) => break,
                Err(e) => {
                    failed += 1;
                    last_error = Some(format!("next: {e}"));
                    break;
                }
            };
            after = ev.id;
            attempted += 1;
            let settled = match self.process_into(&ev, &mut tally).await {
                Ok(()) => self.events.delete(ev.id).await,
                Err(e) => Err(e),
            };
            match settled {
                Ok(()) => done += 1,
                Err(e) => {
                    tracing::error!(
                        target: "oxicloud::links",
                        event_id = ev.id,
                        subject_type = %ev.subject_type,
                        subject_id = %ev.subject_id,
                        resource_type = %ev.resource_type,
                        resource_id = %ev.resource_id,
                        error = %e,
                        "link event left for retry",
                    );
                    failed += 1;
                    last_error = Some(format!("event {}: {e}", ev.id));
                }
            }
        }
        let pending = self.events.pending().await.ok();
        if attempted > 0 || failed > 0 {
            tracing::info!(
                target: "oxicloud::links",
                events = done,
                failed,
                pairs = tally.pairs,
                created = tally.created,
                deleted = tally.deleted,
                name_exhausted = tally.name_exhausted,
                pending = ?pending,
                "links settled",
            );
        }
        match last_error {
            Some(msg) => JobOutcome::err(format!("links_settle: {failed} failed, last {msg}")),
            None => JobOutcome::ok_with(
                tally.pairs,
                serde_json::json!({ "events": done, "pending": pending }),
            ),
        }
    }

    fn overlapping_ticks_expected(&self) -> bool {
        true
    }

    fn quiet_when_idle(&self) -> bool {
        true
    }
}

#[cfg(all(test, integration_tests))]
pub(crate) mod it {
    use super::*;
    use crate::application::services::subject_group_service::SubjectGroupService;
    use crate::common::config::LinksMode;
    use crate::infrastructure::repositories::pg::share_mount_pg_repository::trigger_tests::{
        make_shared_drive, make_user_with_drive, personal_root,
    };
    use crate::infrastructure::repositories::pg::{
        DrivePgRepository, FileBlobReadRepository, FolderDbRepository, SubjectGroupPgRepository,
        UserPgRepository,
    };
    use crate::infrastructure::services::pg_acl_engine::PgAclEngine;
    use crate::mount_it_support::{fresh_db, provision_folder};
    use sqlx::PgPool;
    use std::sync::atomic::AtomicBool;

    pub(crate) struct Fx {
        pub(crate) pool: Arc<PgPool>,
        pub(crate) job: LinksSettleJob,
        pub(crate) repo: Arc<ShareMountPgRepository>,
        pub(crate) events: Arc<LinkEventsPgRepository>,
        groups: Arc<SubjectGroupService>,
    }

    pub(crate) fn fixture(pool: &Arc<PgPool>) -> Fx {
        let engine = Arc::new(PgAclEngine::new(
            pool.clone(),
            Arc::new(FolderDbRepository::new(pool.clone())),
            Arc::new(FileBlobReadRepository::new_stub()),
            Arc::new(SubjectGroupPgRepository::new(pool.clone())),
            Arc::new(AtomicBool::new(false)),
        ));
        let repo = Arc::new(ShareMountPgRepository::new(pool.clone()));
        let svc = Arc::new(ShareMountService::new(
            repo.clone(),
            Arc::new(DrivePgRepository::new(pool.clone())),
            engine.clone(),
            Arc::new(FolderDbRepository::new(pool.clone())),
            None,
            LinksMode::Auto,
        ));
        let groups = Arc::new(SubjectGroupService::new(
            Arc::new(SubjectGroupPgRepository::new(pool.clone())),
            pool.clone(),
            Arc::new(UserPgRepository::new(pool.clone())),
            engine,
            Arc::new(DrivePgRepository::new(pool.clone())),
        ));
        svc.set_group_service(groups.clone());
        let events = Arc::new(LinkEventsPgRepository::new(pool.clone()));
        Fx {
            pool: pool.clone(),
            job: LinksSettleJob::new(svc, repo.clone(), events.clone()),
            repo,
            events,
            groups,
        }
    }

    impl Fx {
        pub(crate) async fn clear_events(&self) {
            sqlx::query("DELETE FROM storage.link_events")
                .execute(&*self.pool)
                .await
                .unwrap();
        }

        pub(crate) async fn run_ok(&self) -> u64 {
            match self.job.run(&JobRunArgs::default()).await {
                JobOutcome::Ok { count, .. } => count,
                other => panic!("run failed: {other:?}"),
            }
        }

        pub(crate) async fn linked(&self, user: Uuid, target: Uuid) -> bool {
            let (drive, _) = personal_root(&self.pool, user).await;
            self.repo.mount_of(drive, target).await.unwrap().is_some()
        }
    }

    async fn make_group(pool: &PgPool, name: &str) -> Uuid {
        sqlx::query_scalar("INSERT INTO auth.subject_groups (name) VALUES ($1) RETURNING id")
            .bind(name)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn add_user(pool: &PgPool, group: Uuid, user: Uuid) {
        sqlx::query(
            "INSERT INTO auth.subject_group_members (group_id, member_user_id, added_by) VALUES ($1, $2, $2)",
        )
        .bind(group)
        .bind(user)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn add_group(pool: &PgPool, parent: Uuid, child: Uuid, by: Uuid) {
        sqlx::query(
            "INSERT INTO auth.subject_group_members (group_id, member_group_id, added_by) VALUES ($1, $2, $3)",
        )
        .bind(parent)
        .bind(child)
        .bind(by)
        .execute(pool)
        .await
        .unwrap();
    }

    pub(crate) async fn grant(
        pool: &PgPool,
        subject_type: &str,
        subject: Uuid,
        resource_type: &str,
        resource: Uuid,
        by: Uuid,
    ) {
        sqlx::query(
            "INSERT INTO storage.role_grants (subject_type, subject_id, resource_type, resource_id, role, granted_by)
             VALUES ($1, $2, $3, $4, 'viewer', $5)",
        )
        .bind(subject_type)
        .bind(subject)
        .bind(resource_type)
        .bind(resource)
        .bind(by)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn plain_folder(pool: &PgPool, name: &str, parent: Uuid, drive: Uuid, by: Uuid) {
        sqlx::query(
            "INSERT INTO storage.folders (name, parent_id, drive_id, created_by, updated_by) VALUES ($1, $2, $3, $4, $4)",
        )
        .bind(name)
        .bind(parent)
        .bind(drive)
        .bind(by)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn drain_creates_links_for_user_and_group_grants() {
        let (_c, pool) = fresh_db().await;
        let fx = fixture(&pool);
        let alice = provision_folder(&pool, "alice", "F").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let carol = make_user_with_drive(&pool, "carol").await;
        let (d, d_root) = make_shared_drive(&pool, "D", alice.owner_id).await;
        let g = make_group(&pool, "g").await;
        add_user(&pool, g, bob).await;
        let f = alice.mount_folder_id;
        grant(&pool, "group", g, "folder", f, alice.owner_id).await;
        grant(&pool, "user", carol, "drive", d, alice.owner_id).await;
        fx.run_ok().await;
        assert!(fx.linked(bob, f).await);
        assert!(fx.linked(carol, d_root).await);
        assert_eq!(fx.events.pending().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn joining_a_group_links_its_grants_and_ancestors_grants() {
        let (_c, pool) = fresh_db().await;
        let fx = fixture(&pool);
        let alice = provision_folder(&pool, "alice", "F").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let (d, d_root) = make_shared_drive(&pool, "D", alice.owner_id).await;
        let (top, g, h) = (
            make_group(&pool, "top").await,
            make_group(&pool, "g").await,
            make_group(&pool, "h").await,
        );
        add_group(&pool, top, g, alice.owner_id).await;
        add_group(&pool, g, h, alice.owner_id).await;
        let f = alice.mount_folder_id;
        grant(&pool, "group", g, "folder", f, alice.owner_id).await;
        grant(&pool, "group", top, "drive", d, alice.owner_id).await;
        fx.run_ok().await;
        add_user(&pool, h, bob).await;
        assert_eq!(fx.events.pending().await.unwrap(), 1);
        fx.run_ok().await;
        assert!(fx.linked(bob, f).await);
        assert!(fx.linked(bob, d_root).await);
    }

    #[tokio::test]
    async fn grant_then_immediate_revoke_leaves_no_link() {
        let (_c, pool) = fresh_db().await;
        let fx = fixture(&pool);
        let alice = provision_folder(&pool, "alice", "F").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let f = alice.mount_folder_id;
        fx.clear_events().await;
        grant(&pool, "user", bob, "folder", f, alice.owner_id).await;
        sqlx::query("DELETE FROM storage.role_grants WHERE resource_id = $1 AND subject_id = $2")
            .bind(f)
            .bind(bob)
            .execute(&*pool)
            .await
            .unwrap();
        assert_eq!(fx.events.pending().await.unwrap(), 2);
        fx.run_ok().await;
        assert!(!fx.linked(bob, f).await);
        assert_eq!(fx.events.pending().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn resume_from_cursor_skips_settled_recipients() {
        let (_c, pool) = fresh_db().await;
        let fx = fixture(&pool);
        let alice = provision_folder(&pool, "alice", "F").await;
        let g = make_group(&pool, "g").await;
        let mut members = Vec::new();
        for i in 0..5 {
            let u = make_user_with_drive(&pool, &format!("u{i}")).await;
            add_user(&pool, g, u).await;
            members.push(u);
        }
        members.sort_unstable();
        let f = alice.mount_folder_id;
        fx.clear_events().await;
        grant(&pool, "group", g, "folder", f, alice.owner_id).await;
        let ev = fx.events.next().await.unwrap().unwrap();
        fx.events.checkpoint(ev.id, members[2]).await.unwrap();
        let ev = fx.events.next().await.unwrap().unwrap();
        assert_eq!(fx.job.process(&ev).await.unwrap(), 2);
        for (i, m) in members.iter().enumerate() {
            assert_eq!(fx.linked(*m, f).await, i > 2, "member {i}");
        }
    }

    #[tokio::test]
    async fn name_exhaustion_is_recorded_not_fatal() {
        let (_c, pool) = fresh_db().await;
        let fx = fixture(&pool);
        let alice = provision_folder(&pool, "alice", "X").await;
        let g = make_group(&pool, "g").await;
        let mut members = Vec::new();
        for i in 0..3 {
            let u = make_user_with_drive(&pool, &format!("u{i}")).await;
            add_user(&pool, g, u).await;
            members.push(u);
        }
        let full = members[1];
        let (full_drive, full_root) = personal_root(&pool, full).await;
        plain_folder(&pool, "X", full_root, full_drive, full).await;
        for n in 2..=100 {
            plain_folder(&pool, &format!("X ({n})"), full_root, full_drive, full).await;
        }
        let x = alice.mount_folder_id;
        fx.clear_events().await;
        grant(&pool, "group", g, "folder", x, alice.owner_id).await;
        fx.run_ok().await;
        assert!(!fx.linked(full, x).await);
        assert!(fx.linked(members[0], x).await);
        assert!(fx.linked(members[2], x).await);
        assert_eq!(fx.events.pending().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn membership_event_for_group_with_no_grants_is_deleted() {
        let (_c, pool) = fresh_db().await;
        let fx = fixture(&pool);
        let bob = make_user_with_drive(&pool, "bob").await;
        let g = make_group(&pool, "g").await;
        fx.clear_events().await;
        add_user(&pool, g, bob).await;
        assert_eq!(fx.events.pending().await.unwrap(), 1);
        assert_eq!(fx.run_ok().await, 0);
        assert_eq!(fx.events.pending().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn grant_event_for_vanished_folder_is_deleted() {
        let (_c, pool) = fresh_db().await;
        let fx = fixture(&pool);
        fx.clear_events().await;
        fx.events
            .enqueue("user", Uuid::new_v4(), "folder", Uuid::new_v4())
            .await
            .unwrap();
        assert_eq!(fx.run_ok().await, 0);
        assert_eq!(fx.events.pending().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn deleting_a_group_drops_its_members_links_and_declines() {
        let (_c, pool) = fresh_db().await;
        let fx = fixture(&pool);
        let alice = provision_folder(&pool, "alice", "F").await;
        let (_, other_root) = make_shared_drive(&pool, "E", alice.owner_id).await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let g = make_group(&pool, "g").await;
        add_user(&pool, g, bob).await;
        let f = alice.mount_folder_id;
        grant(&pool, "group", g, "folder", f, alice.owner_id).await;
        fx.run_ok().await;
        assert!(fx.linked(bob, f).await);
        fx.repo.insert_decline(bob, other_root).await.unwrap();
        fx.groups.delete(g, alice.owner_id).await.unwrap();
        fx.run_ok().await;
        assert!(!fx.linked(bob, f).await);
        assert!(!fx.repo.is_declined(bob, other_root).await.unwrap());
        assert_eq!(fx.events.pending().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn settle_error_keeps_the_event_and_fails_the_run() {
        let (_c, pool) = fresh_db().await;
        let fx = fixture(&pool);
        fx.clear_events().await;
        fx.events
            .enqueue("user", Uuid::new_v4(), "group", Uuid::new_v4())
            .await
            .unwrap();
        sqlx::query("ALTER TABLE storage.share_mount_declines RENAME TO smd_gone")
            .execute(&*pool)
            .await
            .unwrap();
        let out = fx.job.run(&JobRunArgs::default()).await;
        assert!(matches!(out, JobOutcome::Err { .. }), "{out:?}");
        assert_eq!(fx.events.pending().await.unwrap(), 1);
        assert_eq!(fx.events.next().await.unwrap().unwrap().cursor, None);
    }

    async fn fail_links_for(pool: &PgPool, user: Uuid) {
        sqlx::raw_sql(&format!(
            "CREATE FUNCTION storage.test_fail_link() RETURNS trigger AS $$ BEGIN \
               IF NEW.created_by = '{user}' AND NEW.mount_target_id IS NOT NULL THEN RAISE EXCEPTION 'injected'; END IF; \
               RETURN NEW; END $$ LANGUAGE plpgsql; \
             CREATE TRIGGER test_fail_link BEFORE INSERT ON storage.folders \
               FOR EACH ROW EXECUTE FUNCTION storage.test_fail_link();"
        ))
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn failed_event_does_not_block_later_events() {
        let (_c, pool) = fresh_db().await;
        let fx = fixture(&pool);
        let alice = provision_folder(&pool, "alice", "F").await;
        let bad = make_user_with_drive(&pool, "bad").await;
        let good = make_user_with_drive(&pool, "good").await;
        let f = alice.mount_folder_id;
        fail_links_for(&pool, bad).await;
        fx.clear_events().await;
        grant(&pool, "user", bad, "folder", f, alice.owner_id).await;
        grant(&pool, "user", good, "folder", f, alice.owner_id).await;
        let out = fx.job.run(&JobRunArgs::default()).await;
        assert!(matches!(out, JobOutcome::Err { .. }), "{out:?}");
        assert!(fx.linked(good, f).await);
        let left = fx.events.next().await.unwrap().unwrap();
        assert_eq!((left.subject_id, left.cursor), (bad, None));
        assert_eq!(fx.events.pending().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn failure_keeps_cursor_at_last_settled_recipient() {
        let (_c, pool) = fresh_db().await;
        let mut fx = fixture(&pool);
        fx.job.checkpoint_every = 2;
        let alice = provision_folder(&pool, "alice", "F").await;
        let g = make_group(&pool, "g").await;
        let mut members = Vec::new();
        for i in 0..5 {
            let u = make_user_with_drive(&pool, &format!("u{i}")).await;
            add_user(&pool, g, u).await;
            members.push(u);
        }
        members.sort_unstable();
        fail_links_for(&pool, members[3]).await;
        let f = alice.mount_folder_id;
        fx.clear_events().await;
        grant(&pool, "group", g, "folder", f, alice.owner_id).await;
        let out = fx.job.run(&JobRunArgs::default()).await;
        assert!(matches!(out, JobOutcome::Err { .. }), "{out:?}");
        assert_eq!(
            fx.events.next().await.unwrap().unwrap().cursor,
            Some(members[2])
        );
        for (i, m) in members.iter().enumerate() {
            assert_eq!(fx.linked(*m, f).await, i < 3, "member {i}");
        }
    }
}
