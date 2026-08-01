//! PostgreSQL-backed implementation of `AuthorizationEngine`.
//!
//! Stores grants in `storage.role_grants` (one role per (subject, resource)
//! pair; the role's permission bundle is expanded in code via
//! `Role::expand()`). Cascading is resolved at check time via PostgreSQL
//! `ltree` `@>` (ancestor-of) on `storage.folders.lpath`, using the
//! existing GiST index for O(log N) traversal.
//!
//! Owner is implicit — `storage.folders.user_id` / `storage.files.user_id`
//! are checked first via dedicated helpers; if the caller is the owner, no
//! SQL against `role_grants` happens.
//!
//! ## Lifecycle cleanup
//!
//! In v1, cleanup of grant rows when a resource or subject is permanently
//! deleted is enforced by **DB triggers** (`trg_cleanup_grants_*` in the
//! migration). The application layer does not call `revoke_all_for_*`
//! explicitly today — the triggers are the canonical path because they
//! also catch bulk SQL maintenance, admin scripts, and any code path that
//! bypasses the service layer.
//!
//! The `revoke_all_for_resource` / `revoke_all_for_subject` methods exist
//! on the trait for future use cases:
//! - **Caching** (planned) — a `CachedAuthorizationEngine` decorator needs
//!   to see the invalidation event at the engine boundary, not just at the
//!   SQL level. When caching lands, services will start calling these
//!   methods explicitly before/around delete operations.
//! - **Alternate engines** (OpenFGA, future) — engines that don't share a
//!   DB transaction with the resource table need an explicit signal to
//!   delete their tuples.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;
use uuid::Uuid;

use moka::future::Cache;
use sqlx::PgPool;
use tokio::sync::oneshot;

use crate::application::ports::authorization_ports::AuthorizationEngine;
use crate::common::errors::DomainError;
use crate::domain::entities::drive::DrivePolicies;
use crate::domain::entities::subject_group::INTERNAL_GROUP_ID;
use crate::domain::repositories::subject_group_repository::SubjectGroupRepository;
use crate::domain::services::authorization::{
    Grant, GrantCursor, IncomingGrantSummary, OutgoingGrantEntry, OutgoingResourceSummary,
    Permission, Resource, ResourceKind, Role, Subject, roles_implying,
};
use crate::infrastructure::repositories::pg::SubjectGroupPgRepository;
use crate::infrastructure::repositories::pg::file_blob_read_repository::FileBlobReadRepository;
use crate::infrastructure::repositories::pg::folder_db_repository::FolderDbRepository;

/// Per-call counters surfaced through `tracing::debug!` for performance
/// observability: cache hit-rate, SQL traffic, transitive expansion size.
///
/// Sub-microsecond cost when debug logging is off (one atomic write per
/// increment, no allocation, no formatting).
#[derive(Default)]
struct QueryCounters {
    cache_hit: AtomicU32,
    sql_queries: AtomicU32,
    expanded_groups: AtomicU32,
}

/// Defensive upper bound on the number of grant rows the *unbounded* list
/// methods (`list_incoming_grants`, `list_grants_on_resource`) will pull into
/// memory. These back management surfaces ("Manage sharing", "Shared with
/// me"), not the hot `require()` path, so a single resource or subject
/// realistically accumulates orders of magnitude fewer grants than this.
///
/// We fetch `MAX_GRANT_ROWS + 1` and *reject* when the cap is exceeded rather
/// than silently truncating: `apply_role` computes an add/remove diff from the
/// returned set, so a partial list would be acted on as if complete. Hitting
/// the cap signals pathological data and is surfaced to operators via audit.
const MAX_GRANT_ROWS: i64 = 10_000;

/// `owner_cache` bound: entries are tiny (Resource + Uuid). 100k ≈ a few MB.
const OWNER_CACHE_CAPACITY: u64 = 100_000;
/// `owner_cache` TTL. A resource's owner is immutable, so the only staleness is
/// a hard-deleted resource briefly resolving to its former owner — harmless
/// (see `owner_cache` field doc), hence a generous TTL for a high hit rate.
const OWNER_CACHE_TTL: Duration = Duration::from_secs(300);

/// `drive_role_cache` bound: entries are `((Subject, Uuid), Option<Role>)` —
/// a few tens of bytes each. 100k accommodates ~5–10 drives per active user
/// with comfortable headroom.
const DRIVE_ROLE_CACHE_CAPACITY: u64 = 100_000;
/// `drive_role_cache` TTL. Membership mutations on a drive explicitly invalidate
/// affected entries (see `invalidate_drive_role_cache_for_drive`), so the TTL
/// is mainly a safety net for paths that skip explicit invalidation. Short
/// enough that any oversight self-heals in <1 minute.
const DRIVE_ROLE_CACHE_TTL: Duration = Duration::from_secs(30);

/// `drive_policies_cache` bound: entries are `(Uuid, DrivePolicies)` — a
/// handful of bools per drive. 100k is generous headroom for the drive
/// population of any realistic deployment.
const DRIVE_POLICIES_CACHE_CAPACITY: u64 = 100_000;
/// `drive_policies_cache` TTL. Policy mutations explicitly invalidate
/// (see `invalidate_drive_policies_cache_for_drive`) so the TTL is the
/// self-heal net for edge cases (direct SQL PATCH by an operator, migration
/// backfill). Short enough that a manually-flipped `read_only` becomes
/// effective within a minute on the hot path.
const DRIVE_POLICIES_CACHE_TTL: Duration = Duration::from_secs(30);

/// `cascade_grant_cache` bound: entries are
/// `((Subject, Resource, Permission), bool)` — a few tens of bytes each. A
/// shared photo album is one folder grant serving hundreds of file checks, so
/// 100k comfortably covers the working set of active shared-resource viewers.
const CASCADE_GRANT_CACHE_CAPACITY: u64 = 100_000;
/// `cascade_grant_cache` TTL. Direct grant mutations on the file/folder
/// (`set_role` / `clear_role`) explicitly invalidate the whole cache, so the
/// TTL is the self-heal net for the *indirect* paths — a group-membership
/// change, a resource move, or a grant's `expires_at` passing — exactly as
/// `drive_role_cache` leans on its TTL for group changes "rather than a deep
/// invalidation tree". Short enough that any such change takes effect in <1 min.
const CASCADE_GRANT_CACHE_TTL: Duration = Duration::from_secs(30);

/// `direct_grant_cache` bound/TTL: memoises the Calendar / AddressBook /
/// Playlist `role_grants` point decision — the only `check()` arms that had
/// NO result cache, re-run on every CalDAV/CardDAV/music request by clients
/// that poll continuously. Same invalidation contract as
/// `cascade_grant_cache`: grant writes on these resource types flush the
/// whole cache; group/expiry churn self-heals within the TTL
/// (benches/ROUND11.md §Q2).
const DIRECT_GRANT_CACHE_CAPACITY: u64 = 100_000;
const DIRECT_GRANT_CACHE_TTL: Duration = Duration::from_secs(30);

/// `file_parent_cache` bound/TTL: `file_id → Option<folder_id>` point rows
/// (~50 B each) resolved on the file-cascade path so an N-file album pays
/// ONE folder-cascade query instead of N (ROUND9). Parentage changes only
/// on move — an indirect path the cascade cache already self-heals via TTL,
/// so the same 30 s window applies (grant writes don't alter parentage and
/// need no flush here).
const FILE_PARENT_CACHE_CAPACITY: u64 = 100_000;
const FILE_PARENT_CACHE_TTL: Duration = Duration::from_secs(30);

pub struct PgAclEngine {
    pool: Arc<PgPool>,
    folder_repo: Arc<FolderDbRepository>,
    file_repo: Arc<FileBlobReadRepository>,
    /// Group repository — `None` only in test stubs that don't exercise authz.
    group_repo: Option<Arc<SubjectGroupPgRepository>>,
    /// Memoise `user_id → transitive group set` for 30 s. Bounded to 50 000
    /// entries; eviction is LRU + TTL. Stale by up to TTL after a membership
    /// change — acceptable trade-off (see plan, "Cache TTL behaviour").
    user_groups_cache: Cache<Uuid, Arc<HashSet<Uuid>>>,
    /// Memoise `resource → owner UUID`. The owner column is immutable, so the
    /// owner-short-circuit (the common case: a user touching their own files)
    /// no longer issues a PK query on every authorization check — just the first
    /// per resource within the TTL. **Safe**: this can never grant a non-owner
    /// access (a different caller's `owner == uid` test fails against the cached
    /// *real* owner), and a hard-deleted resource that briefly short-circuits as
    /// owned simply fails later at execution with NotFound.
    ///
    /// Post-D0 this caches `resource → drive_id` instead (the legacy `owner_*`
    /// rename was avoided to minimise field-name churn in the dual-write
    /// window). The drive precheck queries this for every File / Folder check.
    owner_cache: Cache<Resource, Uuid>,

    /// Memoise `(subject, drive_id) → Option<Role>`, the strongest role the
    /// subject holds on a drive (direct + group-mediated, collapsed). Drives
    /// the permission-floor precheck in `check_inner` — on a cache hit the
    /// entire drive-grant lookup resolves in-memory, returning the steady
    /// state to "0 SQL queries per authz check for callers touching their
    /// own drive content" (matching the legacy owner short-circuit).
    ///
    /// **Invalidation**: explicit on every membership mutation
    /// (`set_role` / `clear_role` with `Resource::Drive`) — drops every
    /// entry whose drive_id matches. Group-membership changes are caught by
    /// the short TTL rather than a deep invalidation tree.
    ///
    /// **Safety**: cache only widens authorization between mutations; the
    /// 30 s TTL bounds how long a revoked grant can still appear effective
    /// for a non-explicit invalidation path. Explicit paths (D2's
    /// `DriveManagementService`, the grant handler's revoke path) hit the
    /// invalidator inline.
    drive_role_cache: Cache<(Subject, Uuid), Option<Role>>,

    /// Memoise `drive_id → DrivePolicies` (the typed view of the JSONB
    /// `storage.drives.policies` column). Read on every mutating authz
    /// check on a resource that lives in a drive (File/Folder/Drive) to
    /// gate the `read_only` freeze.
    ///
    /// Subject-independent — policies are the same for every caller, so a
    /// single entry per drive covers the whole tenant. Kept separate from
    /// `drive_role_cache` (subject-keyed) so policy changes only flush this
    /// cache, and membership changes only flush that one.
    ///
    /// **Invalidation**: explicit on every `DriveManagementService::update_policies`
    /// call — a policy PATCH invalidates the entry before the response
    /// returns, so the next check sees the fresh values. Short 30 s TTL
    /// as the self-heal net for direct-SQL edits and migration backfills.
    drive_policies_cache: Cache<Uuid, DrivePolicies>,

    /// Memoise the File/Folder **grant-cascade** decision
    /// `(subject, resource, permission) → bool` — the result of the
    /// `role_grants` + folder-ancestor (`lpath @>`) cascade that
    /// `check_inner` falls through to when the drive-role precheck doesn't
    /// cover the caller. This is the per-request query a shared-album
    /// recipient (a grant on the containing folder, no drive membership) pays
    /// for **every thumbnail** — and browsers revalidate immutable thumbnails
    /// constantly, so the same `(subject, file, Read)` decision is recomputed
    /// again and again. Cached here it costs one query then in-memory hits.
    ///
    /// Only reached AFTER the drive-role precheck fails, so a caller who is a
    /// drive member short-circuits above and never populates a (possibly
    /// negative) entry here — a later drive grant can't be shadowed by a stale
    /// cascade `false`.
    ///
    /// **Invalidation**: explicit `invalidate_all` on every File/Folder
    /// `set_role` / `clear_role` (the direct share/revoke path — infrequent
    /// relative to thumbnail reads, so a full flush is cheap and keeps
    /// revocation immediate). The indirect paths — group-membership changes,
    /// resource moves that change ancestry, grant `expires_at` expiry — are
    /// caught by the 30 s TTL, matching `drive_role_cache`'s documented
    /// convention.
    ///
    /// **Safety**: the check still runs on every request (the ordering is
    /// unchanged — authz is never skipped); only its *result* is memoised, and
    /// only positively-or-negatively for at most the TTL. A revoke via
    /// `clear_role` flushes immediately; anything missed self-heals in ≤30 s.
    cascade_grant_cache: Cache<(Subject, Resource, Permission), bool>,

    /// Memoised Calendar/AddressBook/Playlist direct-grant decision (the
    /// top-level resources with no cascade parent). See
    /// `DIRECT_GRANT_CACHE_CAPACITY` for the contract.
    direct_grant_cache: Cache<(Subject, Resource, Permission), bool>,
    /// `file_id → Option<parent folder_id>` memo for the file-cascade
    /// decomposition (see `cascade_grant_cached`): resolving the parent lets
    /// a whole folder's files share ONE folder-cascade decision, so a shared
    /// album's first view runs one ltree query instead of one per file.
    /// Grant writes don't affect parentage — only the TTL applies (moves are
    /// an indirect path, same self-heal contract as `cascade_grant_cache`).
    file_parent_cache: Cache<Uuid, Option<Uuid>>,
    /// Natural-batching collector for cold `file_parent_cache` misses — the
    /// ROUND9 §10 deferred item. A shared N-photo album's cold first view
    /// arrives as N near-simultaneous thumbnail requests, each missing the
    /// parent memo and each paying a point `SELECT folder_id`.
    ///
    /// Leader-runs-inline shape: an idle miss marks itself leader (one
    /// mutex op) and runs its point query exactly as before — the
    /// SEQUENTIAL path gains no hop, no task, no extra latency (a
    /// channel-task variant benchmarked at ~66 µs/miss of pure overhead and
    /// was rejected). Misses arriving while a leader is in flight park a
    /// oneshot in this queue; the leader drains them into ONE `= ANY($1)`
    /// batch after its own query, so a K-wide herd collapses to ~2 queries.
    /// If a leader future is dropped mid-flight, its guard wakes every
    /// parked waiter to retry (and re-elect); waiters that exhaust retries
    /// fall back to the inline point query — strictly additive.
    parent_batch: Arc<std::sync::Mutex<Option<Vec<ParentWaiter>>>>,
    /// Total parent-resolution queries actually issued (point + batches) —
    /// exposed via [`Self::parent_query_count`] for benches/operators.
    parent_queries: Arc<AtomicU64>,
    /// Global "server is in migration read-only mode" flag. When
    /// `true`, `check_inner` short-circuits every write-adjacent
    /// permission (`Create`/`Update`/`Delete`/`Share`/`Comment`/`Manage`)
    /// with a `Denied` decision — same reason as the per-drive
    /// `read_only` gate below, but scoped to the whole process rather
    /// than a specific drive. Backed by
    /// `admin_settings.storage.migration_readonly` so it survives
    /// restart (see `docs/plan/storage-multi-entry.md` §"Read-only
    /// mode"). Shared as `Arc<AtomicBool>` with `AppState` so the
    /// cutover state machine (slice 5) can flip it without needing
    /// to reach into the engine.
    migration_readonly: Arc<std::sync::atomic::AtomicBool>,
}

/// One parked parent-resolution request: file id + reply slot. A dropped
/// sender (leader cancelled) is the retry signal. Errors are shared behind
/// `Arc` because `DomainError` carries a non-clonable source chain (same
/// convention as the Basic-auth single-flight).
type ParentWaiter = (
    Uuid,
    oneshot::Sender<Result<Option<Uuid>, Arc<DomainError>>>,
);

/// Upper bound on ids drained into one `= ANY` parent batch. A browser herd
/// is O(100); this only guards pathological queue growth.
const PARENT_BATCH_MAX: usize = 256;

/// How many times a parked waiter re-runs the elect-or-park protocol after
/// a leader vanished before giving up and querying inline itself.
const PARENT_WAIT_RETRIES: usize = 3;

/// RAII release of parent-resolution leadership. If the leader future is
/// dropped at an await point (client disconnect cancels the request), this
/// clears the in-flight marker and drops every parked waiter's sender —
/// their `oneshot` recv errors and they re-run the election, so a vanished
/// leader can never strand the queue.
struct ParentLeaderGuard<'a> {
    engine: &'a PgAclEngine,
}

impl Drop for ParentLeaderGuard<'_> {
    fn drop(&mut self) {
        let mut slot = match self.engine.parent_batch.lock() {
            Ok(s) => s,
            Err(poisoned) => poisoned.into_inner(),
        };
        *slot = None;
    }
}

impl PgAclEngine {
    pub fn new(
        pool: Arc<PgPool>,
        folder_repo: Arc<FolderDbRepository>,
        file_repo: Arc<FileBlobReadRepository>,
        group_repo: Arc<SubjectGroupPgRepository>,
        migration_readonly: Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self {
            pool,
            folder_repo,
            file_repo,
            group_repo: Some(group_repo),
            migration_readonly,
            user_groups_cache: Cache::builder()
                .max_capacity(50_000)
                .time_to_live(Duration::from_secs(30))
                .build(),
            owner_cache: Cache::builder()
                .max_capacity(OWNER_CACHE_CAPACITY)
                .time_to_live(OWNER_CACHE_TTL)
                .build(),
            drive_role_cache: Cache::builder()
                // `invalidate_entries_if` is the cleanup hook used by
                // `invalidate_drive_role_cache_for_drive`. moka returns
                // `Err(InvalidationClosuresDisabled)` from that call unless
                // this opt-in is set on the builder, so without it the
                // bulk invalidation silently no-ops and a freshly-promoted
                // member keeps their stale role for the full TTL.
                .support_invalidation_closures()
                .max_capacity(DRIVE_ROLE_CACHE_CAPACITY)
                .time_to_live(DRIVE_ROLE_CACHE_TTL)
                .build(),
            drive_policies_cache: Cache::builder()
                .max_capacity(DRIVE_POLICIES_CACHE_CAPACITY)
                .time_to_live(DRIVE_POLICIES_CACHE_TTL)
                .build(),
            cascade_grant_cache: Cache::builder()
                .max_capacity(CASCADE_GRANT_CACHE_CAPACITY)
                .time_to_live(CASCADE_GRANT_CACHE_TTL)
                .build(),
            direct_grant_cache: Cache::builder()
                .max_capacity(DIRECT_GRANT_CACHE_CAPACITY)
                .time_to_live(DIRECT_GRANT_CACHE_TTL)
                .build(),
            file_parent_cache: Cache::builder()
                .max_capacity(FILE_PARENT_CACHE_CAPACITY)
                .time_to_live(FILE_PARENT_CACHE_TTL)
                .build(),
            parent_batch: Arc::new(std::sync::Mutex::new(None)),
            parent_queries: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Subset of `resource_ids` the caller has shared — i.e. has any outgoing
    /// role grant on (a `user`/`group` grant or a `token` grant, the latter
    /// being a public link). One batched query, mirroring the membership the
    /// `/grants/outgoing/resources` endpoint exposes; used to stamp "shared"
    /// badges onto a folder listing without a per-navigation grants fetch.
    pub async fn shared_resource_ids(
        &self,
        granted_by: Uuid,
        resource_ids: &[Uuid],
    ) -> Result<HashSet<Uuid>, DomainError> {
        if resource_ids.is_empty() {
            return Ok(HashSet::new());
        }
        let rows: Vec<(Uuid,)> = sqlx::query_as(
            r#"
            SELECT DISTINCT resource_id
              FROM storage.role_grants
             WHERE granted_by = $1
               AND resource_id = ANY($2)
            "#,
        )
        .bind(granted_by)
        .bind(resource_ids)
        .fetch_all(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("PgAcl", format!("shared_resource_ids: {e}")))?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    /// Creates a stub instance for tests that need to construct services
    /// without a real PostgreSQL pool. Connecting to the lazy pool will
    /// fail at runtime — only safe in tests that exercise types, not actual
    /// authz queries.
    ///
    /// Visible under both `cfg(test)` (the standard unit-test build) and
    /// `cfg(integration_tests)` (the gated-by-RUSTFLAGS integration
    /// build). The `SubjectGroupService` integration tests construct the
    /// service with a stub engine, since they only exercise the engine's
    /// in-memory cache invalidation calls — never its SQL paths.
    #[cfg(any(test, integration_tests))]
    pub fn new_stub() -> Self {
        let pool = sqlx::pool::PoolOptions::<sqlx::Postgres>::new()
            .max_connections(1)
            .connect_lazy("postgres://invalid:5432/none")
            .unwrap();
        Self {
            pool: Arc::new(pool),
            folder_repo: Arc::new(FolderDbRepository::new_stub()),
            file_repo: Arc::new(FileBlobReadRepository::new_stub()),
            group_repo: None,
            user_groups_cache: Cache::builder()
                .max_capacity(1)
                .time_to_live(Duration::from_secs(1))
                .build(),
            owner_cache: Cache::builder()
                .max_capacity(1)
                .time_to_live(Duration::from_secs(1))
                .build(),
            drive_role_cache: Cache::builder()
                .support_invalidation_closures()
                .max_capacity(1)
                .time_to_live(Duration::from_secs(1))
                .build(),
            drive_policies_cache: Cache::builder()
                .max_capacity(1)
                .time_to_live(Duration::from_secs(1))
                .build(),
            cascade_grant_cache: Cache::builder()
                .max_capacity(1)
                .time_to_live(Duration::from_secs(1))
                .build(),
            direct_grant_cache: Cache::builder()
                .max_capacity(1)
                .time_to_live(Duration::from_secs(1))
                .build(),
            file_parent_cache: Cache::builder()
                .max_capacity(1)
                .time_to_live(Duration::from_secs(1))
                .build(),
            parent_batch: Arc::new(std::sync::Mutex::new(None)),
            parent_queries: Arc::new(AtomicU64::new(0)),
            migration_readonly: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Drop the cached transitive-group expansion for one user, forcing
    /// the next `expand_user(uid)` to walk the recursive CTE again.
    ///
    /// Called by [`AuthzCacheLifecycleHook`] on `on_user_logout` /
    /// `on_user_deleted` so a re-login (or a re-created account with the
    /// same id) doesn't observe stale memberships during the 30 s TTL
    /// window. Cheap — moka's `invalidate` is a single concurrent-map op.
    pub async fn invalidate_user_groups_cache(&self, user_id: Uuid) {
        self.user_groups_cache.invalidate(&user_id).await;
    }

    /// Drop every `drive_role_cache` entry whose key targets `drive_id`.
    /// Called after every membership mutation on the drive (set_role /
    /// clear_role / revoke when the resource is a Drive) so the next authz
    /// check sees the fresh role rather than a TTL-bounded stale view.
    ///
    /// Uses moka's predicate-based eviction — entries are marked for
    /// removal asynchronously by the maintenance task; subsequent `get`
    /// calls observe the eviction. Requires
    /// `support_invalidation_closures()` on the cache builder (see the
    /// `drive_role_cache` initialiser above), otherwise moka returns
    /// `InvalidationClosuresDisabled` and the mutation silently leaves
    /// stale role rows in cache for the full TTL.
    /// Drop the cached `DrivePolicies` entry for one drive. Called by
    /// `DriveManagementService::update_policies` after every JSONB PATCH so
    /// the next mutating authz check sees the fresh `read_only` flag and
    /// other policy values without waiting for the TTL. Single-entry
    /// invalidate is a cheap concurrent-map op.
    pub async fn invalidate_drive_policies_cache_for_drive(&self, drive_id: Uuid) {
        self.drive_policies_cache.invalidate(&drive_id).await;
    }

    pub async fn invalidate_drive_role_cache_for_drive(&self, drive_id: Uuid) {
        // `invalidate_entries_if` rejects predicates returning errors —
        // simple Fn(K, V) -> bool. We capture `drive_id` by value (Copy)
        // and match against the second tuple component.
        //
        // The result is `Err` only when the cache was built without
        // `support_invalidation_closures()` — a wiring bug, not a runtime
        // condition the caller can recover from. We log+continue rather
        // than panic because the consequence is a 30 s staleness window
        // on cached role entries, not a correctness bug at write time.
        if let Err(err) = self
            .drive_role_cache
            .invalidate_entries_if(move |key, _v| key.1 == drive_id)
        {
            tracing::error!(
                target: "oxicloud::authz",
                event = "authz.cache_invalidation_failed",
                cache = "drive_role_cache",
                drive_id = %drive_id,
                error = %err,
                "drive_role_cache cannot be bulk-invalidated — \
                 cache builder is missing support_invalidation_closures()",
            );
        }
    }

    /// Drop the `owner_cache` entry for `resource`. Called after any
    /// operation that changes which drive a file/folder belongs to —
    /// the pre-D6 comment on `owner_cache` ("a resource's owner is
    /// immutable") stopped being true when cross-drive MOVE landed.
    ///
    /// Without this call, admin (or any other role holder) on the
    /// destination drive gets `authz.denied` when acting on the moved
    /// resource: the cached (stale) `Resource → src_drive_id` lookup
    /// steers the drive-role precheck at `check_inner` toward the
    /// SOURCE drive where the caller has no role, and the fallback
    /// per-resource cascade doesn't cover drive-level grants. TTL
    /// backstops eventually (5 min), but every write path that MOVEs
    /// content across drives MUST invalidate here so authz observes
    /// the new drive on the next check.
    pub async fn invalidate_owner_cache_for_resource(&self, resource: Resource) {
        self.owner_cache.invalidate(&resource).await;
    }

    /// Bulk cousin of [`Self::invalidate_owner_cache_for_resource`] —
    /// clears the entire `owner_cache`. Called by folder cross-drive
    /// MOVE where the moved subtree's descendants each carry their
    /// own stale entry, and we don't (yet) walk the subtree to
    /// invalidate them individually. The cache repopulates lazily on
    /// next access; the overhead is a single JOIN per file/folder
    /// touched in the following minute or two, versus a stale-authz
    /// bug that returned `NotFound` for legitimate Delete.
    pub async fn invalidate_owner_cache_all(&self) {
        self.owner_cache.invalidate_all();
    }

    /// Flush the entire `cascade_grant_cache`. Called on every File/Folder
    /// `set_role` / `clear_role` — the direct share/revoke path. A resource
    /// grant can widen (or, via ancestry, narrow) the cascade decision for an
    /// unbounded set of descendant files, and the cache is keyed by the
    /// decision — not the grant — so we can't target the affected entries
    /// without walking the subtree. A full flush is correct and cheap here:
    /// grant mutations are rare next to the thumbnail reads the cache serves,
    /// and it keeps a revoke immediate. Indirect changes (group membership,
    /// resource moves, grant expiry) are left to the 30 s TTL, mirroring
    /// `drive_role_cache`.
    pub async fn invalidate_cascade_grant_cache_all(&self) {
        self.cascade_grant_cache.invalidate_all();
    }

    /// Sibling of [`Self::invalidate_drive_role_cache_for_drive`] keyed by
    /// subject rather than drive. Used by the user-deleted lifecycle hook
    /// to reap every cached "user X → drive Y = role R" entry after the
    /// user row (and its DB-cascade-cleared role_grants) is gone. Without
    /// this call the entry lingers until TTL; in practice auth rejection
    /// on the deleted user's tokens fires first, but leaving stale
    /// authorisation rows in the cache is poor hygiene and would surface
    /// as an issue if a session survived (e.g. long-lived Basic Auth via
    /// app password) or if a same-uuid user were ever recreated.
    pub async fn invalidate_drive_role_cache_for_subject(&self, subject: Subject) {
        if let Err(err) = self
            .drive_role_cache
            .invalidate_entries_if(move |key, _v| key.0 == subject)
        {
            tracing::error!(
                target: "oxicloud::authz",
                event = "authz.cache_invalidation_failed",
                cache = "drive_role_cache",
                subject = ?subject,
                error = %err,
                "drive_role_cache cannot be bulk-invalidated by subject — \
                 cache builder is missing support_invalidation_closures()",
            );
        }
    }

    /// Expand a user subject into the set of subject UUIDs that should match
    /// in `access_grants`: the user's own UUID, every group the user is
    /// transitively a member of, and (for internal users only) the implicit
    /// `INTERNAL_GROUP_ID`.
    ///
    /// External users (`auth.users.is_external = TRUE`) do NOT belong to
    /// the Internal virtual group — they are grant-only recipients whose
    /// access is determined exclusively by explicit grants on their
    /// `user_id` or on subject groups they were explicitly added to.
    /// `SubjectGroupService::add_member` rejects externals, so the only
    /// path by which an external user reaches a resource is via a
    /// `subject_type='user'` grant.
    ///
    /// This is the **only** place transitive membership is walked. A future
    /// closure-table swap-in (Option 3 in the design doc) replaces just the
    /// `repo.groups_for_user` call below — every caller stays unchanged.
    async fn expand_user(
        &self,
        user_id: Uuid,
        counters: &QueryCounters,
    ) -> Result<Arc<HashSet<Uuid>>, DomainError> {
        if let Some(cached) = self.user_groups_cache.get(&user_id).await {
            counters.cache_hit.store(1, Ordering::Relaxed);
            counters
                .expanded_groups
                .store(cached.len() as u32, Ordering::Relaxed);
            return Ok(cached);
        }

        let mut set: HashSet<Uuid> = HashSet::new();
        set.insert(user_id);

        // Look up `is_external` for the caller — external users do not
        // belong to the Internal virtual group. Unknown user (no row) is
        // treated as external to fail closed: a deleted or bogus user_id
        // must not gain implicit Internal membership.
        //
        // The `is_external` point read and the recursive groups CTE are
        // independent — `join!` overlaps their round-trips on every cold
        // expansion instead of paying them serially (benches/ROUND11.md
        // §Q3; the ROUND9/10 pattern).
        counters.sql_queries.fetch_add(1, Ordering::Relaxed);
        let is_external_fut = async {
            sqlx::query_scalar::<_, bool>("SELECT is_external FROM auth.users WHERE id = $1")
                .bind(user_id)
                .fetch_optional(self.pool.as_ref())
                .await
                .map_err(|e| {
                    DomainError::internal_error("PgAcl", format!("lookup is_external: {e}"))
                })
                .map(|row| row.unwrap_or(true))
        };
        let groups_fut = async {
            match &self.group_repo {
                Some(repo) => {
                    counters.sql_queries.fetch_add(1, Ordering::Relaxed);
                    repo.groups_for_user(user_id).await.map(Some).map_err(|e| {
                        DomainError::internal_error("PgAcl", format!("groups_for_user: {e}"))
                    })
                }
                None => Ok(None),
            }
        };
        let (is_external, direct) = tokio::join!(is_external_fut, groups_fut);
        if !is_external? {
            set.insert(INTERNAL_GROUP_ID);
        }
        if let Some(direct) = direct? {
            set.extend(direct);
        }

        counters
            .expanded_groups
            .store(set.len() as u32, Ordering::Relaxed);
        let arc = Arc::new(set);
        self.user_groups_cache.insert(user_id, arc.clone()).await;
        Ok(arc)
    }

    /// Expand a caller's `Subject` into the `(subject_types, subject_ids)`
    /// pair that should be matched in `storage.role_grants`. For User
    /// callers this is `(["user","group"], [uid, …transitive groups, INTERNAL])`;
    /// for any non-user subject (Token / External / Group as direct caller)
    /// it's a single-element pair with no cascade.
    ///
    /// Shared by `check_inner` (permission decision) and the
    /// `list_incoming_*` queries ("Shared with me") so that any folder/file
    /// the user can `read` via a group grant also appears in their incoming
    /// listing. Shares the `expand_user` Moka cache, so the listing call
    /// right after a permission check is a cache hit.
    async fn subject_match_set(
        &self,
        subject: Subject,
        counters: &QueryCounters,
    ) -> Result<(Vec<&'static str>, Vec<Uuid>), DomainError> {
        match subject {
            Subject::User(uid) => {
                let expanded = self.expand_user(uid, counters).await?;
                Ok((vec!["user", "group"], expanded.iter().copied().collect()))
            }
            _ => Ok((vec![subject.type_str()], vec![subject.id()])),
        }
    }

    /// Public wrapper around `subject_match_set` for callers that need
    /// the expanded `(subject_types, subject_ids)` pair without invoking
    /// the engine's full `check`/`require` pipeline.
    ///
    /// **Retained for legacy callers only** — new listing queries embed
    /// the `storage.caller_group_ids` PostgreSQL function inline (see
    /// migration `20260901000002_caller_group_ids_function.sql`) and
    /// take a bare `caller_id: Uuid` instead of the pre-expanded arrays.
    /// The engine's Moka cache still backs the fast path for per-request
    /// AuthZ decisions (`check_inner`, `drive_role_cache`) where the
    /// same subject is looked up repeatedly.
    pub async fn expand_subject_for_listing(
        &self,
        subject: Subject,
    ) -> Result<(Vec<&'static str>, Vec<Uuid>), DomainError> {
        let counters = QueryCounters::default();
        self.subject_match_set(subject, &counters).await
    }

    /// Drive lookup with memoisation. Hits the DB only on a cache miss; the
    /// result is cached because a resource's `drive_id` is immutable in the
    /// current model (cross-drive moves arrive in D6 and will need cache
    /// invalidation at that point). `NotFound` is propagated, not cached.
    ///
    /// **Why this replaces the legacy `owner_of_cached`**: the old short-
    /// circuit was `caller_id == resource.user_id`. Post-D0 ownership is
    /// modelled through `role_grants` on the resource's drive — a caller
    /// with any qualifying role on the drive automatically satisfies the
    /// check (per `drive.md §5`, drive role is the baseline floor for
    /// every resource in the drive). The lookup shape is identical
    /// (Resource → Uuid), so we keep the same cache infrastructure.
    async fn drive_of_cached(
        &self,
        resource: Resource,
        counters: &QueryCounters,
    ) -> Result<Uuid, DomainError> {
        if let Some(drive_id) = self.owner_cache.get(&resource).await {
            return Ok(drive_id);
        }
        counters.sql_queries.fetch_add(1, Ordering::Relaxed);
        let drive_id = self.drive_of(resource).await?;
        self.owner_cache.insert(resource, drive_id).await;
        Ok(drive_id)
    }

    /// Returns the `drive_id` for a File / Folder. Drives don't have a parent
    /// drive — this returns `NotFound` for `Resource::Drive` and the caller
    /// must not invoke it on Drive resources.
    ///
    /// `Resource::Calendar`, `Resource::AddressBook` and
    /// `Resource::Playlist` are top-level per user with no drive
    /// ancestor; they also return `NotFound` and the engine
    /// short-circuits to a direct `role_grants` lookup (no drive
    /// precheck applies).
    async fn drive_of(&self, resource: Resource) -> Result<Uuid, DomainError> {
        match resource {
            Resource::Folder(id) => self.folder_repo.get_folder_drive_id(&id.to_string()).await,
            Resource::File(id) => self.file_repo.get_file_drive_id(&id.to_string()).await,
            Resource::Drive(_)
            | Resource::Calendar(_)
            | Resource::AddressBook(_)
            | Resource::Playlist(_) => Err(DomainError::not_found(
                resource.type_str(),
                resource.id().to_string(),
            )),
        }
    }

    /// Convert a `Permission` into the array of role strings whose bundle
    /// includes it — bound as `ANY($N::storage.grant_role[])` so the
    /// ENUM-typed `role` column compares without an implicit text cast.
    ///
    /// This is the inverse of `Role::expand()`, precomputed via
    /// `grant_dto::roles_implying()`. The mapping is small and static (≤5
    /// roles per permission today); resolving it in code keeps the SQL
    /// path simple and lets us add new roles without touching every
    /// query site.
    fn roles_implying_strings(permission: Permission) -> Vec<&'static str> {
        roles_implying(permission)
            .iter()
            .map(|r| r.as_str())
            .collect()
    }

    /// Cascading check for folders: is there a grant on any ancestor folder
    /// (including the target itself) for any of the given subject IDs and
    /// any of the given subject types?
    ///
    /// `subject_types` is `["user", "group"]` when the caller is a User
    /// (so we match both their own grants and their group-mediated grants),
    /// or a single-element slice for Token / External / Group-direct callers.
    /// `subject_ids` is the expanded set returned by `expand_user` (or a
    /// single-element vec for non-user callers).
    ///
    /// Reads `storage.role_grants` (1 row per role assignment); a permission
    /// filter `g.permission = $3` becomes `g.role = ANY($3::storage.grant_role[])` where
    /// the array is the set of roles whose bundle includes the requested
    /// permission — see `roles_implying()`.
    ///
    /// Uses the GiST index on `storage.folders.lpath` for O(log N) cascade.
    /// Direct grant lookup with no cascade — used for top-level
    /// resources whose ACL lives entirely on their own row
    /// (`Resource::Calendar`, `Resource::AddressBook`). Same
    /// role-array + subject-set shape as the cascade helpers so a
    /// caller's group memberships still resolve, but no ltree /
    /// folder ancestry / drive precheck applies. Calendars and
    /// address books have no parent to inherit from.
    async fn direct_grant_exists(
        &self,
        subject_types: &[&str],
        subject_ids: &[Uuid],
        permission: Permission,
        resource_type: &'static str,
        resource_id: Uuid,
        counters: &QueryCounters,
    ) -> Result<bool, DomainError> {
        counters.sql_queries.fetch_add(1, Ordering::Relaxed);
        let roles = Self::roles_implying_strings(permission);
        let exists: Option<i32> = sqlx::query_scalar(
            r#"
            SELECT 1
              FROM storage.role_grants g
             WHERE g.subject_type  = ANY($1)
               AND g.subject_id    = ANY($2)
               AND g.role          = ANY($3::storage.grant_role[])
               AND g.resource_type = $4
               AND g.resource_id   = $5
               AND (g.expires_at IS NULL OR g.expires_at > NOW())
             LIMIT 1
            "#,
        )
        .bind(subject_types)
        .bind(subject_ids)
        .bind(&roles)
        .bind(resource_type)
        .bind(resource_id)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("PgAcl", format!("direct grant: {e}")))?;

        Ok(exists.is_some())
    }

    async fn folder_cascade_grant_exists(
        &self,
        subject_types: &[&str],
        subject_ids: &[Uuid],
        permission: Permission,
        folder_id: Uuid,
        counters: &QueryCounters,
    ) -> Result<bool, DomainError> {
        counters.sql_queries.fetch_add(1, Ordering::Relaxed);
        let roles = Self::roles_implying_strings(permission);
        let exists: Option<i32> = sqlx::query_scalar(
            r#"
            SELECT 1
              FROM storage.role_grants g
              JOIN storage.folders gf ON gf.id = g.resource_id
             WHERE g.subject_type  = ANY($1)
               AND g.subject_id    = ANY($2)
               AND g.role          = ANY($3::storage.grant_role[])
               AND g.resource_type = 'folder'
               AND (g.expires_at IS NULL OR g.expires_at > NOW())
               AND gf.lpath @> (SELECT lpath FROM storage.folders WHERE id = $4)
             LIMIT 1
            "#,
        )
        .bind(subject_types)
        .bind(subject_ids)
        .bind(&roles)
        .bind(folder_id)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("PgAcl", format!("folder cascade: {e}")))?;

        Ok(exists.is_some())
    }

    /// Direct file grant only — the first branch of the historical file
    /// cascade UNION, split out so `cascade_grant_cached` can amortize the
    /// ancestor-folder branch per FOLDER (see the `Resource::File` arm).
    /// A plain indexed `role_grants` point lookup, no ltree join.
    async fn file_direct_grant_exists(
        &self,
        subject_types: &[&str],
        subject_ids: &[Uuid],
        permission: Permission,
        file_id: Uuid,
        counters: &QueryCounters,
    ) -> Result<bool, DomainError> {
        counters.sql_queries.fetch_add(1, Ordering::Relaxed);
        let roles = Self::roles_implying_strings(permission);
        let exists: Option<i32> = sqlx::query_scalar(
            r#"
            SELECT 1
              FROM storage.role_grants
             WHERE subject_type = ANY($1)
               AND subject_id   = ANY($2)
               AND role         = ANY($3::storage.grant_role[])
               AND resource_type = 'file' AND resource_id = $4
               AND (expires_at IS NULL OR expires_at > NOW())
             LIMIT 1
            "#,
        )
        .bind(subject_types)
        .bind(subject_ids)
        .bind(&roles)
        .bind(file_id)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("PgAcl", format!("file direct grant: {e}")))?;

        Ok(exists.is_some())
    }

    /// Memoised `file_id → Option<parent folder_id>` read backing the
    /// file-cascade decomposition. `None` covers both a missing row and a
    /// NULL `folder_id` — in either case only the direct-file-grant branch
    /// can match (mirroring the historical UNION's `folder_id IS NOT NULL`
    /// guard).
    ///
    /// Cold misses run the leader-inline batching protocol (see
    /// `parent_batch`): an idle miss queries inline exactly as before;
    /// misses concurrent with an in-flight leader park and are answered by
    /// the leader's single `= ANY` charity batch.
    async fn file_parent_folder_cached(
        &self,
        file_id: Uuid,
        counters: &QueryCounters,
    ) -> Result<Option<Uuid>, DomainError> {
        if let Some(parent) = self.file_parent_cache.get(&file_id).await {
            return Ok(parent);
        }
        counters.sql_queries.fetch_add(1, Ordering::Relaxed);

        enum Elect {
            Lead,
            Park(oneshot::Receiver<Result<Option<Uuid>, Arc<DomainError>>>),
            Overflow,
        }
        for _ in 0..=PARENT_WAIT_RETRIES {
            // Elect-or-park. The guard lives only inside this block — the
            // decision is acted on AFTER it drops, so no lock is ever held
            // across an await (and the handler futures stay `Send`).
            let outcome = {
                let mut slot = self.parent_batch.lock().expect("parent_batch poisoned");
                match slot.as_mut() {
                    // A leader is in flight — park a oneshot in its queue.
                    Some(queue) if queue.len() < PARENT_BATCH_MAX => {
                        let (tx, rx) = oneshot::channel();
                        queue.push((file_id, tx));
                        Elect::Park(rx)
                    }
                    // Queue full — behave as if idle contention: inline below.
                    Some(_) => Elect::Overflow,
                    // Idle — become the leader.
                    None => {
                        *slot = Some(Vec::new());
                        Elect::Lead
                    }
                }
            };

            match outcome {
                Elect::Lead => return self.parent_leader_resolve(file_id).await,
                Elect::Park(rx) => match rx.await {
                    Ok(Ok(parent)) => return Ok(parent),
                    Ok(Err(shared)) => {
                        return Err(DomainError::new(
                            shared.kind,
                            shared.entity_type,
                            shared.message.clone(),
                        ));
                    }
                    // Leader vanished (cancelled mid-flight) — retry the
                    // election; a fresh leader (possibly us) takes over.
                    Err(_) => continue,
                },
                // Queue overflow: don't wait — resolve inline.
                Elect::Overflow => break,
            }
        }

        // Retries exhausted or queue overflow: the historical inline read.
        self.parent_queries.fetch_add(1, Ordering::Relaxed);
        let parent = Self::query_parent_point(&self.pool, file_id).await?;
        self.file_parent_cache.insert(file_id, parent).await;
        Ok(parent)
    }

    /// Leader half of the parent-resolution protocol: run own point query
    /// inline (the exact pre-round-10 cost), then serve everything that
    /// parked during it with ONE `= ANY` batch. A second wave arriving
    /// during the charity batch is handed to a detached drainer task so the
    /// leader's own response is never delayed by more than one batch.
    ///
    /// Cancellation-safe: `ParentLeaderGuard` releases leadership on drop
    /// and wakes parked waiters (their `oneshot` senders drop → they retry
    /// and re-elect).
    async fn parent_leader_resolve(&self, file_id: Uuid) -> Result<Option<Uuid>, DomainError> {
        let guard = ParentLeaderGuard { engine: self };

        self.parent_queries.fetch_add(1, Ordering::Relaxed);
        let own = Self::query_parent_point(&self.pool, file_id).await;
        if let Ok(parent) = &own {
            self.file_parent_cache.insert(file_id, *parent).await;
        }

        // Take the first charity wave (leave `Some(vec![])` so later
        // arrivals keep parking while the batch runs).
        let wave = {
            let mut slot = self.parent_batch.lock().expect("parent_batch poisoned");
            match slot.as_mut() {
                Some(queue) if !queue.is_empty() => std::mem::take(queue),
                _ => Vec::new(),
            }
        };
        if !wave.is_empty() {
            self.parent_queries.fetch_add(1, Ordering::Relaxed);
            Self::serve_parent_wave(&self.pool, &self.file_parent_cache, wave).await;
        }

        // Release leadership — or, if a second wave parked during the
        // charity batch, hand leadership to a detached drainer so the
        // leader's own response isn't delayed further. The drainer loops:
        // it keeps the slot marked in-flight (newer misses keep parking)
        // and only clears it when the queue drains empty.
        let second_wave = {
            let mut slot = self.parent_batch.lock().expect("parent_batch poisoned");
            match slot.as_mut() {
                Some(queue) if !queue.is_empty() => Some(std::mem::take(queue)),
                _ => {
                    *slot = None; // idle again
                    None
                }
            }
        };
        std::mem::forget(guard); // leadership released or handed to the drainer
        if let Some(first) = second_wave {
            let engine = self.clone_batch_handles();
            tokio::spawn(async move {
                let mut wave = first;
                loop {
                    engine.2.fetch_add(1, Ordering::Relaxed);
                    Self::serve_parent_wave(&engine.0, &engine.1, wave).await;
                    let mut slot = match engine.3.lock() {
                        Ok(s) => s,
                        Err(p) => p.into_inner(),
                    };
                    match slot.as_mut() {
                        Some(queue) if !queue.is_empty() => {
                            wave = std::mem::take(queue);
                        }
                        _ => {
                            *slot = None;
                            break;
                        }
                    }
                }
            });
        }
        own
    }

    /// The `Arc`'d handles the detached drainer needs (pool, memo cache,
    /// query counter, queue slot). Cloned individually because the drainer
    /// outlives this call and the engine isn't guaranteed to sit behind an
    /// `Arc` here.
    #[allow(clippy::type_complexity)]
    fn clone_batch_handles(
        &self,
    ) -> (
        Arc<PgPool>,
        Cache<Uuid, Option<Uuid>>,
        Arc<AtomicU64>,
        Arc<std::sync::Mutex<Option<Vec<ParentWaiter>>>>,
    ) {
        (
            Arc::clone(&self.pool),
            self.file_parent_cache.clone(),
            Arc::clone(&self.parent_queries),
            Arc::clone(&self.parent_batch),
        )
    }

    /// Resolve one file's parent with the point query (shared by the
    /// leader's own read and the no-batching fallback).
    async fn query_parent_point(pool: &PgPool, file_id: Uuid) -> Result<Option<Uuid>, DomainError> {
        let parent: Option<Option<Uuid>> =
            sqlx::query_scalar("SELECT folder_id FROM storage.files WHERE id = $1")
                .bind(file_id)
                .fetch_optional(pool)
                .await
                .map_err(|e| DomainError::internal_error("PgAcl", format!("file parent: {e}")))?;
        Ok(parent.flatten())
    }

    /// Serve a parked wave with one `= ANY` query: memoise every id
    /// (requested-but-absent rows memoise as `None`, matching the point
    /// read) and answer every oneshot. On error the shared failure is
    /// fanned out instead.
    async fn serve_parent_wave(
        pool: &PgPool,
        cache: &Cache<Uuid, Option<Uuid>>,
        wave: Vec<ParentWaiter>,
    ) {
        let mut ids: Vec<Uuid> = Vec::with_capacity(wave.len());
        for (id, _) in &wave {
            if !ids.contains(id) {
                ids.push(*id);
            }
        }
        let fetched: Result<Vec<(Uuid, Option<Uuid>)>, sqlx::Error> =
            sqlx::query_as("SELECT id, folder_id FROM storage.files WHERE id = ANY($1)")
                .bind(&ids)
                .fetch_all(pool)
                .await;
        match fetched {
            Ok(rows) => {
                let mut by_id: std::collections::HashMap<Uuid, Option<Uuid>> =
                    rows.into_iter().collect();
                for id in &ids {
                    by_id.entry(*id).or_insert(None);
                }
                for (id, parent) in &by_id {
                    cache.insert(*id, *parent).await;
                }
                for (id, reply) in wave {
                    let parent = by_id.get(&id).copied().unwrap_or(None);
                    let _ = reply.send(Ok(parent));
                }
            }
            Err(e) => {
                let shared = Arc::new(DomainError::internal_error(
                    "PgAcl",
                    format!("file parent batch: {e}"),
                ));
                for (_, reply) in wave {
                    let _ = reply.send(Err(Arc::clone(&shared)));
                }
            }
        }
    }

    /// Total parent-resolution queries actually issued (point + `= ANY`
    /// batches). With batching, this is ≤ the number of cold misses — the
    /// gap is the herd-collapse win. Exposed for benches and operators.
    pub fn parent_query_count(&self) -> u64 {
        self.parent_queries.load(Ordering::Relaxed)
    }

    /// Cache-aware wrapper over the File/Folder grant cascade. Serves the
    /// memoised `(subject, resource, permission)` decision when warm; on a
    /// miss it expands the subject set (itself cached) and runs the matching
    /// cascade query, then stores the result. Only invoked after the drive-role
    /// precheck fails, so it never caches a decision a drive grant would have
    /// satisfied — a later drive grant short-circuits above this cache.
    ///
    /// **File decomposition (ROUND9).** The historical file query was one
    /// UNION: `direct file grant ∨ grant on any ancestor of the parent
    /// folder` — one ltree join per file, so a shared N-photo album's FIRST
    /// view ran N near-identical ancestor queries (round 8 memoised only the
    /// per-file result, covering revalidation). The arm now resolves the
    /// file's parent (memoised point read) and recurses into the FOLDER arm
    /// for the ancestor half — one ltree query per folder, shared by every
    /// sibling — falling back to the direct-file-grant lookup only when the
    /// folder half denies. The decomposition is exactly the UNION split in
    /// two: no decision changes, including the parentless edge (the UNION's
    /// `folder_id IS NOT NULL` guard ≡ the direct-only fallback).
    ///
    /// The result is a pure function of the subject's group expansion + the
    /// resource's grants + folder ancestry; `invalidate_cascade_grant_cache_all`
    /// (on File/Folder grant writes — it holds file AND folder decisions in
    /// the same map) and the 30 s TTL (indirect changes, incl. moves for the
    /// parent memo) keep it fresh. See the `cascade_grant_cache` field doc.
    /// Cached wrapper for the Calendar/AddressBook/Playlist direct-grant
    /// decision (`try_get_with`: a cold herd on one key coalesces into ONE
    /// loader run, the ROUND10 single-flight pattern; loader errors are
    /// never cached). `resource_type` is the `role_grants.resource_type`
    /// discriminant for `resource`.
    async fn direct_grant_cached(
        &self,
        subject: Subject,
        resource: Resource,
        permission: Permission,
        resource_type: &'static str,
        id: Uuid,
        counters: &QueryCounters,
    ) -> Result<bool, DomainError> {
        if let Some(allowed) = self
            .direct_grant_cache
            .get(&(subject, resource, permission))
            .await
        {
            counters.cache_hit.fetch_add(1, Ordering::Relaxed);
            return Ok(allowed);
        }
        self.direct_grant_cache
            .try_get_with((subject, resource, permission), async {
                let (subject_types, subject_ids) =
                    self.subject_match_set(subject, counters).await?;
                self.direct_grant_exists(
                    &subject_types,
                    &subject_ids,
                    permission,
                    resource_type,
                    id,
                    counters,
                )
                .await
            })
            .await
            .map_err(|e: Arc<DomainError>| {
                DomainError::internal_error("PgAcl", format!("direct grant load: {e}"))
            })
    }

    async fn cascade_grant_cached(
        &self,
        subject: Subject,
        resource: Resource,
        permission: Permission,
        counters: &QueryCounters,
    ) -> Result<bool, DomainError> {
        if let Some(allowed) = self
            .cascade_grant_cache
            .get(&(subject, resource, permission))
            .await
        {
            counters.cache_hit.fetch_add(1, Ordering::Relaxed);
            return Ok(allowed);
        }
        // Only File/Folder reach this helper (see `check_inner`); keep the
        // defensive arm OUTSIDE the loader so it stays uncached, as before.
        if !matches!(resource, Resource::Folder(_) | Resource::File(_)) {
            return Ok(false);
        }
        // `try_get_with`: a cold herd on the same key — every photo of an
        // album recursing into the SAME folder decision at once — coalesces
        // into ONE loader run. The old get→compute→insert let K concurrent
        // misses each run the ltree query (ROUND10; the ROUND3 auth-herd
        // pattern). moka never caches loader errors, preserving the
        // historical error semantics.
        self.cascade_grant_cache
            .try_get_with((subject, resource, permission), async {
                match resource {
                    Resource::Folder(id) => {
                        let (subject_types, subject_ids) =
                            self.subject_match_set(subject, counters).await?;
                        self.folder_cascade_grant_exists(
                            &subject_types,
                            &subject_ids,
                            permission,
                            id,
                            counters,
                        )
                        .await
                    }
                    Resource::File(id) => {
                        // Ancestor half first — amortized to one query per
                        // FOLDER via the recursive Folder arm (its own cache
                        // entry + its own single-flight).
                        let folder_allowed =
                            match self.file_parent_folder_cached(id, counters).await? {
                                Some(parent) => {
                                    Box::pin(self.cascade_grant_cached(
                                        subject,
                                        Resource::Folder(parent),
                                        permission,
                                        counters,
                                    ))
                                    .await?
                                }
                                None => false,
                            };
                        if folder_allowed {
                            Ok(true)
                        } else {
                            let (subject_types, subject_ids) =
                                self.subject_match_set(subject, counters).await?;
                            self.file_direct_grant_exists(
                                &subject_types,
                                &subject_ids,
                                permission,
                                id,
                                counters,
                            )
                            .await
                        }
                    }
                    _ => unreachable!("guarded above"),
                }
            })
            .await
            .map_err(|e: Arc<DomainError>| {
                DomainError::new(e.kind, e.entity_type, e.message.clone())
            })
    }

    /// Cached resolution of `(subject, drive_id) → Option<Role>` — the
    /// strongest role the subject holds on the drive (direct + transitive
    /// group grants collapsed). `None` means no qualifying grant; cached
    /// negatively to avoid re-querying on repeated denials within the TTL.
    ///
    /// This is the cache-aware backbone of the permission-floor precheck.
    /// `Role::expand()` translates the returned role into its permission
    /// bundle; callers ask `role.expand().contains(&permission)` to decide.
    async fn caller_role_on_drive_cached(
        &self,
        subject: Subject,
        drive_id: Uuid,
        counters: &QueryCounters,
    ) -> Result<Option<Role>, DomainError> {
        if let Some(cached) = self.drive_role_cache.get(&(subject, drive_id)).await {
            return Ok(cached);
        }
        // Expand subject for the role lookup. `subject_match_set` is itself
        // cached (30 s TTL); the steady state on `drive_role_cache` miss is
        // one in-memory expansion + one indexed SQL query.
        let (subject_types, subject_ids) = self.subject_match_set(subject, counters).await?;
        counters.sql_queries.fetch_add(1, Ordering::Relaxed);
        let role_str: Option<String> = sqlx::query_scalar(
            r#"
            SELECT MIN(g.role)::text
              FROM storage.role_grants g
             WHERE g.subject_type  = ANY($1)
               AND g.subject_id    = ANY($2)
               AND g.resource_type = 'drive'
               AND g.resource_id   = $3
               AND (g.expires_at IS NULL OR g.expires_at > NOW())
            "#,
        )
        .bind(subject_types)
        .bind(subject_ids)
        .bind(drive_id)
        .fetch_one(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("PgAcl", format!("drive role lookup: {e}")))?;

        let role = role_str.as_deref().and_then(Role::parse);
        self.drive_role_cache
            .insert((subject, drive_id), role)
            .await;
        Ok(role)
    }

    /// Fetch a drive's typed `DrivePolicies`, going through `drive_policies_cache`
    /// (30 s TTL, explicit invalidation on policy PATCH). Malformed JSONB
    /// falls back to the all-false default — consistent with
    /// `DrivePolicies::from_value` — so enforcement can't panic on legacy
    /// or partial data.
    async fn drive_policies_cached(
        &self,
        drive_id: Uuid,
        counters: &QueryCounters,
    ) -> Result<DrivePolicies, DomainError> {
        if let Some(cached) = self.drive_policies_cache.get(&drive_id).await {
            counters.cache_hit.fetch_add(1, Ordering::Relaxed);
            return Ok(cached);
        }
        counters.sql_queries.fetch_add(1, Ordering::Relaxed);
        let row: Option<(serde_json::Value,)> =
            sqlx::query_as("SELECT policies FROM storage.drives WHERE id = $1")
                .bind(drive_id)
                .fetch_optional(self.pool.as_ref())
                .await
                .map_err(|e| {
                    DomainError::internal_error("PgAcl", format!("policies lookup: {e}"))
                })?;
        // Missing drive: cache the default (all-false). Anti-enum handled by
        // the caller — a missing drive returns NotFound at the resource-resolve
        // step upstream; here we just make sure the cache doesn't panic-loop
        // if the read happens post-drive-delete.
        let policies = row
            .map(|(v,)| DrivePolicies::from_value(&v))
            .unwrap_or_default();
        self.drive_policies_cache
            .insert(drive_id, policies.clone())
            .await;
        Ok(policies)
    }

    /// Every permission except `Read` mutates persistent state on a
    /// drive-scoped resource and is therefore refused when the drive is
    /// `read_only=true`:
    ///
    /// - `Create` / `Update` / `Delete` — the obvious file/folder mutations.
    /// - `Share` — persists a new `role_grants` row.
    /// - `Comment` — adds user-generated content (reserved feature).
    /// - `Manage` — mutates drive-level membership (add/remove/promote
    ///   members) on `Resource::Drive`.
    ///
    /// **Admin escape hatch does NOT rely on this gate.** Un-freezing a
    /// drive goes through `PATCH /api/drives/{id}/policies`, which is
    /// admin-only via `admin_guard` at the handler layer — it never
    /// enters `authz.require`. So blocking `Manage` here doesn't lock
    /// admins out; it locks OWNERS out of membership mutation while the
    /// freeze holds, which is exactly the legal-hold guarantee.
    ///
    /// Only `Read` passes: members can still list, download, and PROPFIND
    /// the drive's contents.
    fn read_only_gate_applies(p: Permission) -> bool {
        !matches!(p, Permission::Read)
    }

    /// Look up a single role grant by id, returning the actors a revoke /
    /// notify handler needs to make a decision without a second round-trip.
    /// Returns `(subject, resource, granted_by)` or `None` if no such row.
    pub async fn find_grant_full_by_id(
        &self,
        grant_id: Uuid,
    ) -> Result<Option<(Subject, Resource, Uuid)>, DomainError> {
        let row: Option<(String, Uuid, String, Uuid, Uuid)> = sqlx::query_as(
            "SELECT subject_type, subject_id, resource_type, resource_id, granted_by \
             FROM storage.role_grants WHERE id = $1",
        )
        .bind(grant_id)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("PgAcl", format!("find_grant_full_by_id: {e}")))?;

        let Some((st, sid, rt, rid, granter)) = row else {
            return Ok(None);
        };
        let subject = Subject::from_parts(&st, sid)
            .ok_or_else(|| DomainError::internal_error("PgAcl", "unknown subject_type"))?;
        let resource = Resource::from_parts(&rt, rid)
            .ok_or_else(|| DomainError::internal_error("PgAcl", "unknown resource_type"))?;
        Ok(Some((subject, resource, granter)))
    }

    /// Row type for `storage.role_grants` SELECTs:
    /// (id, subject_type, subject_id, resource_type, resource_id, role, granted_by, granted_at, expires_at).
    ///
    /// Builds a single role-keyed `Grant` per row. `Grant` is role-keyed
    /// since the D-Prep cleanup PR — every listing method returns role
    /// rows directly; bundle expansion to per-permission Grants no longer
    /// happens here. Callers that need the permission set use
    /// `grant.role.expand()` at the call site.
    #[allow(clippy::type_complexity)]
    fn row_to_grant(
        row: (
            Uuid,
            String,
            Uuid,
            String,
            Uuid,
            String,
            Uuid,
            chrono::DateTime<chrono::Utc>,
            Option<chrono::DateTime<chrono::Utc>>,
        ),
    ) -> Result<Grant, DomainError> {
        let subject = Subject::from_parts(&row.1, row.2)
            .ok_or_else(|| DomainError::internal_error("PgAcl", "unknown subject_type"))?;
        let resource = Resource::from_parts(&row.3, row.4)
            .ok_or_else(|| DomainError::internal_error("PgAcl", "unknown resource_type"))?;
        let role = Role::parse(&row.5)
            .ok_or_else(|| DomainError::internal_error("PgAcl", "unknown role"))?;
        Ok(Grant {
            id: row.0,
            subject,
            resource,
            role,
            granted_by: row.6,
            granted_at: row.7,
            expires_at: row.8,
        })
    }

    /// Reject an over-cap grant listing rather than returning a truncated set.
    /// The unbounded list methods fetch `MAX_GRANT_ROWS + 1` and pass the row
    /// count here; callers diff against the full result, so silently dropping
    /// rows would corrupt that diff. Emits an audit line before failing so the
    /// pathological resource/subject is visible to operators.
    fn guard_grant_row_cap(returned: usize, op: &str) -> Result<(), DomainError> {
        if returned as i64 > MAX_GRANT_ROWS {
            tracing::info!(
                target: "audit",
                event = "authz.grant_list_rejected",
                reason = "over_row_cap",
                op,
                cap = MAX_GRANT_ROWS,
                "👮🏻‍♂️ grant listing exceeded the row safety cap; refusing to return a partial set",
            );
            return Err(DomainError::internal_error(
                "PgAcl",
                format!("{op}: too many grants (cap {})", MAX_GRANT_ROWS),
            ));
        }
        Ok(())
    }

    /// The actual permission decision. Wrapped by `check()` which adds
    /// per-call instrumentation.
    async fn check_inner(
        &self,
        subject: Subject,
        permission: Permission,
        resource: Resource,
        counters: &QueryCounters,
    ) -> Result<bool, DomainError> {
        // Global migration-readonly short-circuit. Applies to every
        // resource type — no drive lookup, no per-resource state. When
        // the server is in migration read-only mode, every mutating
        // permission is refused with an audit line naming the specific
        // `migration_readonly` reason so operators filtering the audit
        // stream can distinguish it from per-drive freezes. Reads pass
        // (browsers, downloads, PROPFIND all keep working — same as the
        // per-drive gate). Admin operations don't reach `check_inner`
        // — they go through `admin_guard` middleware which bypasses
        // authz entirely, so the admin can still exit the mode, cancel
        // the migration, restart the server, etc.
        //
        // See `docs/plan/storage-multi-entry.md` §"Read-only mode".
        if Self::read_only_gate_applies(permission)
            && self
                .migration_readonly
                .load(std::sync::atomic::Ordering::Relaxed)
        {
            tracing::info!(
                target: "audit",
                event = "authz.denied",
                reason = "migration_readonly",
                subject_type = subject.type_str(),
                subject_id = %subject.id(),
                permission = permission.as_str(),
                resource_type = resource.type_str(),
                resource_id = %resource.id(),
                "🚧 mutation refused: server is in storage-migration read-only mode",
            );
            return Ok(false);
        }

        // Drive-membership precheck for File/Folder. A role on the resource's
        // drive is the baseline floor (`drive.md §5`): the caller passes any
        // permission check the role bundle covers. Replaces the legacy
        // `caller_id == resource.user_id` owner short-circuit — for a user's
        // own personal drive the lifecycle hook seeds an Owner row, so the
        // common case (touching your own files) is **0 SQL queries** after
        // the first hit on `drive_role_cache` (cached `(subject, drive) →
        // Role`, 30 s TTL with explicit invalidation on membership writes).
        if matches!(resource, Resource::Folder(_) | Resource::File(_)) {
            let drive_id = match self.drive_of_cached(resource, counters).await {
                Ok(d) => d,
                Err(e) if e.kind == crate::common::errors::ErrorKind::NotFound => {
                    // Resource doesn't exist — no permission. `require`
                    // converts the `false` back to NotFound at its layer.
                    return Ok(false);
                }
                Err(e) => return Err(e),
            };
            // Read-only drive freeze — every mutating permission on any
            // resource in this drive is refused, regardless of the caller's
            // role. Compliance-grade guarantee: paired with the background-
            // job SQL filters, no state on this drive changes until the
            // policy is flipped. See `docs/plan/drive.md` §8 (`read_only`).
            //
            // Anti-enumeration: emit an audit line with the specific
            // `drive_read_only` reason, then return `false`. The generic
            // `authz.denied` line at `require` also fires — operators
            // filter on the specific event to find freeze-caused denials.
            if Self::read_only_gate_applies(permission)
                && self
                    .drive_policies_cached(drive_id, counters)
                    .await?
                    .read_only
            {
                tracing::info!(
                    target: "audit",
                    event = "authz.denied",
                    reason = "drive_read_only",
                    subject_type = subject.type_str(),
                    subject_id = %subject.id(),
                    permission = permission.as_str(),
                    resource_type = resource.type_str(),
                    resource_id = %resource.id(),
                    drive_id = %drive_id,
                    "🧊 mutation refused: drive is read-only",
                );
                return Ok(false);
            }
            if let Some(role) = self
                .caller_role_on_drive_cached(subject, drive_id, counters)
                .await?
                && role.expand().contains(&permission)
            {
                return Ok(true);
            }
            // Drive precheck didn't match — fall through to per-resource
            // grant + folder-ancestor cascade (existing behaviour, untouched).
        }

        match resource {
            // File/Folder dispatch falls through to the cascade query, now
            // memoised: a shared-album recipient (folder grant, no drive
            // membership) reaches this per thumbnail, and browsers revalidate
            // thumbnails constantly, so the same decision is recomputed over
            // and over. `cascade_grant_cached` serves it from memory after the
            // first query; the check is unchanged (never skipped), only cached.
            Resource::Folder(_) | Resource::File(_) => {
                self.cascade_grant_cached(subject, resource, permission, counters)
                    .await
            }
            Resource::Drive(id) => {
                // Same read_only gate as the File/Folder branch: a frozen
                // drive refuses every mutating permission (Create / Update /
                // Delete / Share) targeting the drive resource itself.
                // Manage stays permitted so admins can toggle the policy
                // back off; Read stays permitted so members can still list.
                if Self::read_only_gate_applies(permission)
                    && self.drive_policies_cached(id, counters).await?.read_only
                {
                    tracing::info!(
                        target: "audit",
                        event = "authz.denied",
                        reason = "drive_read_only",
                        subject_type = subject.type_str(),
                        subject_id = %subject.id(),
                        permission = permission.as_str(),
                        resource_type = "drive",
                        resource_id = %id,
                        drive_id = %id,
                        "🧊 mutation refused: drive is read-only",
                    );
                    return Ok(false);
                }
                // Same cache-aware path the precheck uses — keeps the
                // single-source-of-truth for drive role resolution and
                // benefits identically from `drive_role_cache`.
                Ok(self
                    .caller_role_on_drive_cached(subject, id, counters)
                    .await?
                    .is_some_and(|r| r.expand().contains(&permission)))
            }
            // Top-level resources with no cascade parent — the ACL
            // lives entirely on their own `role_grants` rows. Owner is
            // an explicit grant seeded at MKCALENDAR / address-book
            // create time (Round 3 phase 2 migration), so the common
            // "owner accessing their own calendar" case is one SQL
            // round-trip — no drive_role_cache short-circuit (no
            // drive), no cascade.
            Resource::Calendar(id) => {
                self.direct_grant_cached(subject, resource, permission, "calendar", id, counters)
                    .await
            }
            Resource::AddressBook(id) => {
                self.direct_grant_cached(
                    subject,
                    resource,
                    permission,
                    "address_book",
                    id,
                    counters,
                )
                .await
            }
            Resource::Playlist(id) => {
                self.direct_grant_cached(subject, resource, permission, "playlist", id, counters)
                    .await
            }
        }
    }
}

impl AuthorizationEngine for PgAclEngine {
    async fn check(
        &self,
        subject: Subject,
        permission: Permission,
        resource: Resource,
    ) -> Result<bool, DomainError> {
        let start = std::time::Instant::now();
        let counters = QueryCounters::default();

        let result = self
            .check_inner(subject, permission, resource, &counters)
            .await;

        // Single structured debug line per check. No-op when subscriber
        // filter is at INFO or above. See plan, "Debug instrumentation".
        tracing::debug!(
            target: "oxicloud::authz",
            event = "authz.check",
            subject = %subject,
            permission = %permission,
            resource = %resource,
            allowed = result.as_ref().copied().unwrap_or(false),
            duration_us = start.elapsed().as_micros() as u64,
            cache_hit = counters.cache_hit.load(Ordering::Relaxed) > 0,
            sql_queries = counters.sql_queries.load(Ordering::Relaxed),
            expanded_groups = counters.expanded_groups.load(Ordering::Relaxed),
        );

        result
    }

    /// Batched Read check over a page of file ids (see the trait docs).
    ///
    /// Decision-equivalent to looping `check`: (1) resolve every file's
    /// drive in one `= ANY($1)` query (same rows as N ×
    /// `get_file_drive_id`; absent ids decide `false` exactly like the
    /// per-file `NotFound` path), (2) evaluate the drive-role floor once
    /// per distinct drive through the same `drive_role_cache`, (3) send
    /// only the drive-floor misses through the full per-file cascade —
    /// preserving per-file grant resolution. `Read` is never gated by the
    /// read-only drive freeze, so skipping that branch changes nothing.
    async fn check_files_read_batch(
        &self,
        subject: Subject,
        file_ids: &[Uuid],
    ) -> Result<std::collections::HashSet<Uuid>, DomainError> {
        use std::collections::{HashMap, HashSet};
        let start = std::time::Instant::now();
        let counters = QueryCounters::default();

        counters.sql_queries.fetch_add(1, Ordering::Relaxed);
        let pairs = self.file_repo.get_file_drive_ids(file_ids).await?;

        // Prime the resource→drive cache — later single checks on these
        // files (download, share) skip their point lookup too.
        for (file_id, drive_id) in &pairs {
            self.owner_cache
                .insert(Resource::File(*file_id), *drive_id)
                .await;
        }

        let mut drive_readable: HashMap<Uuid, bool> = HashMap::new();
        for (_, drive_id) in &pairs {
            if !drive_readable.contains_key(drive_id) {
                let ok = self
                    .caller_role_on_drive_cached(subject, *drive_id, &counters)
                    .await?
                    .is_some_and(|role| role.expand().contains(&Permission::Read));
                drive_readable.insert(*drive_id, ok);
            }
        }

        let mut allowed: HashSet<Uuid> = HashSet::with_capacity(pairs.len());
        for (file_id, drive_id) in &pairs {
            if drive_readable.get(drive_id).copied().unwrap_or(false) {
                allowed.insert(*file_id);
            } else if self
                .check_inner(
                    subject,
                    Permission::Read,
                    Resource::File(*file_id),
                    &counters,
                )
                .await?
            {
                // Per-file / folder-cascade grant inside a drive the caller
                // has no role on — rare, but must keep resolving.
                allowed.insert(*file_id);
            }
        }

        tracing::debug!(
            target: "oxicloud::authz",
            event = "authz.check_files_read_batch",
            subject = %subject,
            files = file_ids.len(),
            allowed = allowed.len(),
            duration_us = start.elapsed().as_micros() as u64,
            sql_queries = counters.sql_queries.load(Ordering::Relaxed),
        );
        Ok(allowed)
    }

    async fn list_incoming_grants(&self, subject: Subject) -> Result<Vec<Grant>, DomainError> {
        let counters = QueryCounters::default();
        let (subject_types, subject_ids) = self.subject_match_set(subject, &counters).await?;

        // `ORDER BY role ASC` exploits the `storage.grant_role` ENUM
        // declared as `(owner, editor, contributor, commenter, viewer)`,
        // so the sort order matches the UX requirement ("Owner > Editor
        // > Contributor > Commenter > Viewer") without a per-row CASE.
        let rows = sqlx::query_as::<
            _,
            (
                Uuid,
                String,
                Uuid,
                String,
                Uuid,
                String,
                Uuid,
                chrono::DateTime<chrono::Utc>,
                Option<chrono::DateTime<chrono::Utc>>,
            ),
        >(
            r#"
            SELECT id, subject_type, subject_id, resource_type, resource_id,
                   role::text, granted_by, granted_at, expires_at
              FROM storage.role_grants
             WHERE subject_type = ANY($1)
               AND subject_id   = ANY($2)
             ORDER BY role ASC, granted_at DESC
             LIMIT $3
            "#,
        )
        .bind(&subject_types)
        .bind(&subject_ids)
        .bind(MAX_GRANT_ROWS + 1)
        .fetch_all(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("PgAcl", format!("list incoming: {e}")))?;

        Self::guard_grant_row_cap(rows.len(), "list_incoming_grants")?;
        rows.into_iter().map(Self::row_to_grant).collect()
    }

    async fn list_incoming_resources_paged(
        &self,
        subject: Subject,
        kinds: &[ResourceKind],
        limit: u32,
        cursor: Option<GrantCursor>,
        sort_by: &str,
        reverse: bool,
    ) -> Result<(Vec<IncomingGrantSummary>, Option<GrantCursor>), DomainError> {
        // ── Common setup ──────────────────────────────────────────────────────
        let kind_strs: Option<Vec<&str>> = if kinds.is_empty() {
            None
        } else {
            Some(kinds.iter().map(|k| k.as_str()).collect())
        };
        let fetch_limit = (limit as i64) + 1;

        // Unified row type — the last two columns carry the sort key when present,
        // NULL otherwise.  This lets every sort mode share a single query_as call.
        //   0 resource_type  String
        //   1 resource_id    Uuid
        //   2 roles          Vec<String>  — every distinct role granting access to this
        //                                   resource (post-D-Prep). Expanded to permissions
        //                                   in `IncomingGrantSummary` via `Role::expand()`.
        //                                   Multiple entries possible when a user has both
        //                                   a direct grant and a group-mediated grant on
        //                                   the same resource.
        //   3 granted_at     DateTime<Utc>
        //   4 granted_by     Uuid
        //   5 sort_str       Option<String>  — resource_name (name/type) or owner_name (granted_by)
        //   6 sort_int       Option<i64>     — category_order (type) or file size in bytes (size)
        type Row = (
            String,
            Uuid,
            Vec<String>,
            chrono::DateTime<chrono::Utc>,
            Uuid,
            Option<String>,
            Option<i64>,
        );

        // Extract all cursor fields up-front; each branch uses the subset it needs.
        // Fixed parameter positions used in all SQL variants:
        //   $4 = cursor_str  (resource_name / owner_name)
        //   $5 = cursor_int  (type_order)
        //   $6 = cursor_at   (granted_at)
        //   $7 = cursor_id   (resource_id)
        //   $8 = fetch_limit
        let cursor_str = cursor.as_ref().and_then(|c| c.resource_name.clone());
        let cursor_int = cursor.as_ref().and_then(|c| c.sort_int);
        let cursor_at = cursor.as_ref().map(|c| c.granted_at);
        let cursor_id = cursor.as_ref().map(|c| c.resource_id);

        // ── agg CTE (identical in all branches) ───────────────────────────────
        // `subject_type`/`subject_id` are arrays here: for a User caller this
        // is `(["user","group"], [uid, …transitive groups, INTERNAL])` so the
        // listing includes every resource the user can reach via a group
        // grant (matching what `check()` allows). See `subject_match_set`.
        //
        // Post-D-Prep this reads `storage.role_grants` and aggregates the
        // ENUM-typed `role` column into a text array. Multiple roles can
        // appear per resource when the caller reaches it via both a direct
        // grant and a group-mediated grant — the union of role bundles
        // produces the displayed permission set in Rust below.
        const AGG: &str = r#"agg AS (
            SELECT
                resource_type,
                resource_id,
                array_agg(DISTINCT role::text ORDER BY role::text) AS roles,
                MIN(granted_at)                                    AS granted_at,
                (array_agg(granted_by ORDER BY granted_at))[1]    AS granted_by
            FROM storage.role_grants
            WHERE subject_type = ANY($1)
              AND subject_id   = ANY($2)
              AND ($3::text[] IS NULL OR resource_type = ANY($3))
            GROUP BY resource_type, resource_id
        )"#;

        // ── Build sort-specific SQL fragments ─────────────────────────────────
        // "name" and "type" share the same LEFT JOINs; only sort_int_expr,
        // the cursor WHERE condition, and ORDER BY differ.
        // Each branch emits two variants selected by `reverse`.
        let sql = match sort_by {
            "name" | "type" => {
                let sort_int_expr = if sort_by == "type" {
                    "CASE WHEN agg.resource_type = 'folder' THEN 0 ELSE fi.category_order::bigint END"
                } else {
                    "NULL::bigint"
                };
                // Normal vs reversed keyset + ORDER BY.
                let (where_clause, order_clause) = if sort_by == "type" {
                    if reverse {
                        (
                            r#"(  $5::integer IS NULL
                               OR sort_int < $5
                               OR (sort_int = $5 AND LOWER(sort_str) < $4)
                               OR (sort_int = $5 AND LOWER(sort_str) = $4 AND resource_id < $7::uuid))"#,
                            "sort_int DESC, LOWER(sort_str) DESC, resource_id DESC",
                        )
                    } else {
                        (
                            r#"(  $5::integer IS NULL
                               OR sort_int > $5
                               OR (sort_int = $5 AND LOWER(sort_str) > $4)
                               OR (sort_int = $5 AND LOWER(sort_str) = $4 AND resource_id > $7::uuid))"#,
                            "sort_int ASC, LOWER(sort_str) ASC, resource_id ASC",
                        )
                    }
                } else if reverse {
                    (
                        r#"(  $4::text IS NULL
                           OR LOWER(sort_str) < $4
                           OR (LOWER(sort_str) = $4 AND resource_id < $7::uuid))"#,
                        "LOWER(sort_str) DESC, resource_id DESC",
                    )
                } else {
                    (
                        r#"(  $4::text IS NULL
                           OR LOWER(sort_str) > $4
                           OR (LOWER(sort_str) = $4 AND resource_id > $7::uuid))"#,
                        "LOWER(sort_str) ASC, resource_id ASC",
                    )
                };
                format!(
                    r#"WITH {AGG},
                    named AS (
                        SELECT agg.*,
                            COALESCE(
                                CASE WHEN agg.resource_type = 'folder' THEN f.name  END,
                                CASE WHEN agg.resource_type = 'file'   THEN fi.name END
                            ) AS sort_str,
                            {sort_int_expr} AS sort_int
                        FROM agg
                        LEFT JOIN storage.folders f  ON f.id  = agg.resource_id AND agg.resource_type = 'folder'
                        LEFT JOIN storage.files   fi ON fi.id = agg.resource_id AND agg.resource_type = 'file'
                    )
                    SELECT resource_type, resource_id, roles, granted_at, granted_by, sort_str, sort_int
                    FROM named
                    WHERE {where_clause}
                    ORDER BY {order_clause}
                    LIMIT $8"#
                )
            }
            "granted_by" => {
                // Joins auth.users to sort alphabetically by username.
                // Cursor encodes (owner_name=$4, granted_at=$6, resource_id=$7).
                let (where_clause, order_clause) = if reverse {
                    (
                        r#"(  $4::text IS NULL
                          OR sort_str < $4
                          OR (sort_str = $4 AND (
                                  $6::timestamptz IS NULL
                               OR granted_at > $6
                               OR (granted_at = $6 AND resource_id > $7::uuid))))"#,
                        "sort_str DESC, granted_at ASC, resource_id ASC",
                    )
                } else {
                    (
                        r#"(  $4::text IS NULL
                          OR sort_str > $4
                          OR (sort_str = $4 AND (
                                  $6::timestamptz IS NULL
                               OR granted_at < $6
                               OR (granted_at = $6 AND resource_id < $7::uuid))))"#,
                        "sort_str ASC, granted_at DESC, resource_id DESC",
                    )
                };
                format!(
                    r#"WITH {AGG},
                    owner_named AS (
                        SELECT agg.*,
                            LOWER(u.username) AS sort_str,
                            NULL::bigint AS sort_int
                        FROM agg
                        LEFT JOIN auth.users u ON u.id = agg.granted_by
                    )
                    SELECT resource_type, resource_id, roles, granted_at, granted_by, sort_str, sort_int
                    FROM owner_named
                    WHERE {where_clause}
                    ORDER BY {order_clause}
                    LIMIT $8"#
                )
            }
            _ => {
                // Default: sort by grant date.
                // Normal = DESC (newest first); reversed = ASC (oldest first).
                // Cursor encodes (granted_at=$6, resource_id=$7); $4/$5 unused.
                let (where_clause, order_clause) = if reverse {
                    (
                        r#"(  $6::timestamptz IS NULL
                          OR granted_at > $6
                          OR (granted_at = $6 AND resource_id > $7::uuid))"#,
                        "granted_at ASC, resource_id ASC",
                    )
                } else {
                    (
                        r#"(  $6::timestamptz IS NULL
                          OR granted_at < $6
                          OR (granted_at = $6 AND resource_id < $7::uuid))"#,
                        "granted_at DESC, resource_id DESC",
                    )
                };
                format!(
                    r#"WITH {AGG}
                    SELECT resource_type, resource_id, roles, granted_at, granted_by,
                           NULL::text   AS sort_str,
                           NULL::bigint AS sort_int
                    FROM agg
                    WHERE {where_clause}
                    ORDER BY {order_clause}
                    LIMIT $8"#
                )
            }
        };

        // Expand the caller so group-mediated grants surface in the listing,
        // mirroring `check()`. Shares the Moka cache (`expand_user`).
        let counters = QueryCounters::default();
        let (subject_types, subject_ids) = self.subject_match_set(subject, &counters).await?;

        // ── Execute — uniform 8 binds for every sort mode ─────────────────────
        let mut rows: Vec<Row> = sqlx::query_as::<_, Row>(&sql)
            .bind(&subject_types) // $1
            .bind(&subject_ids) // $2
            .bind(&kind_strs) // $3
            .bind(&cursor_str) // $4 sort_str cursor
            .bind(cursor_int) // $5 sort_int cursor
            .bind(cursor_at) // $6 granted_at cursor
            .bind(cursor_id) // $7 resource_id cursor
            .bind(fetch_limit) // $8
            .fetch_all(self.pool.as_ref())
            .await
            .map_err(|e| {
                DomainError::internal_error(
                    "PgAcl",
                    format!("list_incoming_resources_paged ({sort_by}): {e}"),
                )
            })?;

        // ── Pagination ────────────────────────────────────────────────────────
        let has_next = rows.len() > limit as usize;
        rows.truncate(limit as usize);

        let next_cursor = if has_next {
            rows.last().map(|r| {
                let sort_str_lc = r.5.as_deref().map(str::to_lowercase);
                match sort_by {
                    "name" => GrantCursor {
                        sort_by: "name".to_owned(),
                        granted_at: r.3,
                        resource_id: r.1,
                        resource_name: sort_str_lc,
                        sort_int: None,
                        reverse,
                    },
                    "type" => GrantCursor {
                        sort_by: "type".to_owned(),
                        granted_at: r.3,
                        resource_id: r.1,
                        resource_name: sort_str_lc,
                        sort_int: r.6,
                        reverse,
                    },
                    "granted_by" => GrantCursor {
                        sort_by: "granted_by".to_owned(),
                        granted_at: r.3,
                        resource_id: r.1,
                        resource_name: r.5.clone(), // already lowercased by SQL
                        sort_int: None,
                        reverse,
                    },
                    _ => GrantCursor {
                        sort_by: "granted_at".to_owned(),
                        granted_at: r.3,
                        resource_id: r.1,
                        resource_name: None,
                        sort_int: None,
                        reverse,
                    },
                }
            })
        } else {
            None
        };

        // ── Convert rows to domain summaries ──────────────────────────────────
        // Post-D-Prep: the SQL aggregate produces a `roles` text array. We
        // expand each role's bundle and union them — direct grants and
        // group-mediated grants on the same resource collapse to a single
        // deduplicated permission set, matching the pre-pivot behaviour.
        let summaries = rows
            .into_iter()
            .filter_map(|(rt, rid, roles_str, granted_at, granted_by, _, _)| {
                let resource_type = ResourceKind::parse(&rt)?;
                let mut permissions: Vec<Permission> = roles_str
                    .into_iter()
                    .filter_map(|s| Role::parse(&s))
                    .flat_map(|r| r.expand().iter().copied())
                    .collect();
                permissions.sort_by_key(|p| p.as_str());
                permissions.dedup();
                Some(IncomingGrantSummary {
                    resource_type,
                    resource_id: rid,
                    permissions,
                    granted_at,
                    granted_by,
                })
            })
            .collect();

        Ok((summaries, next_cursor))
    }

    async fn list_grants_on_resource(&self, resource: Resource) -> Result<Vec<Grant>, DomainError> {
        // Pivoted to `storage.role_grants` (see `list_incoming_grants`).
        // Each role row expands to N permission-keyed `Grant` rows via
        // `role_row_to_grants` until the public `Grant` shape becomes
        // role-keyed.
        //
        // `ORDER BY role ASC` exploits the `storage.grant_role` ENUM's
        // declaration order (owner first → viewer last) so the share
        // dialog's "who has access" list shows strongest grants on top.
        let rows = sqlx::query_as::<
            _,
            (
                Uuid,
                String,
                Uuid,
                String,
                Uuid,
                String,
                Uuid,
                chrono::DateTime<chrono::Utc>,
                Option<chrono::DateTime<chrono::Utc>>,
            ),
        >(
            r#"
            SELECT id, subject_type, subject_id, resource_type, resource_id,
                   role::text, granted_by, granted_at, expires_at
              FROM storage.role_grants
             WHERE resource_type = $1
               AND resource_id   = $2
             ORDER BY role ASC, granted_at DESC
             LIMIT $3
            "#,
        )
        .bind(resource.type_str())
        .bind(resource.id())
        .bind(MAX_GRANT_ROWS + 1)
        .fetch_all(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("PgAcl", format!("list on resource: {e}")))?;

        Self::guard_grant_row_cap(rows.len(), "list_grants_on_resource")?;
        rows.into_iter().map(Self::row_to_grant).collect()
    }

    async fn list_outgoing_resources_paged(
        &self,
        granted_by: Uuid,
        limit: u32,
        cursor: Option<GrantCursor>,
        sort_by: &str,
        reverse: bool,
    ) -> Result<(Vec<OutgoingResourceSummary>, Option<GrantCursor>), DomainError> {
        let fetch_limit = (limit as i64) + 1;

        // Row shape — post-D-Prep, one row per (resource, subject) since
        // `storage.role_grants` carries exactly one role per pair (UNIQUE
        // constraint). Permission bundles are expanded in the row consumer
        // via `Role::expand()`.
        // Columns:
        //   0  resource_type   String
        //   1  resource_id     Uuid
        //   2  first_shared_at DateTime<Utc>   — MIN(granted_at) across resource
        //   3  subject_type    String
        //   4  subject_id      Uuid
        //   5  subject_display String          — username or share item_name
        //   6  grant_id        Uuid
        //   7  granted_at      DateTime<Utc>   — this (subject, role) row
        //   8  expires_at      Option<DateTime<Utc>>
        //   9  role            String          — `grant_role` ENUM as text
        //  10  sort_str        Option<String>
        //  11  sort_int        Option<i64>
        //  12  has_password    bool            — token: shares.password_hash IS NOT NULL
        //  13  is_external     bool            — user: auth.users.is_external (PR N2);
        //                                       FALSE for token/group subjects.
        type Row = (
            String,
            Uuid,
            chrono::DateTime<chrono::Utc>,
            String,
            Uuid,
            String,
            Uuid,
            chrono::DateTime<chrono::Utc>,
            Option<chrono::DateTime<chrono::Utc>>,
            String,
            Option<String>,
            Option<i64>,
            bool,
            bool,
        );

        let cursor_str = cursor.as_ref().and_then(|c| c.resource_name.clone());
        let cursor_int = cursor.as_ref().and_then(|c| c.sort_int);
        let cursor_at = cursor.as_ref().map(|c| c.granted_at);
        let cursor_id = cursor.as_ref().map(|c| c.resource_id);

        // ── Resource-page CTE (one row per resource, cursor-paginated) ─────────
        // We page on resources (by first_shared_at + resource_id) so that the
        // limit/cursor semantics are consistent with the incoming endpoint.
        // All grants for each paged resource are then retrieved in the same query.
        //
        // $1 = granted_by
        // $2 = cursor_str   (resource_name for name/type, owner_name for granted_by)
        // $3 = cursor_int   (category_order for type, size for size)
        // $4 = cursor_at    (first_shared_at)
        // $5 = cursor_id    (resource_id)
        // $6 = fetch_limit
        let sql = match sort_by {
            "name" | "type" => {
                let sort_int_expr = if sort_by == "type" {
                    "CASE WHEN ag.resource_type = 'folder' THEN 0 ELSE fi.category_order::bigint END"
                } else {
                    "NULL::bigint"
                };
                let (page_where, page_order) = if sort_by == "type" {
                    if reverse {
                        (
                            r#"(  $3::integer IS NULL
                               OR sort_int < $3
                               OR (sort_int = $3 AND LOWER(sort_str) < $2)
                               OR (sort_int = $3 AND LOWER(sort_str) = $2 AND resource_id < $5::uuid))"#,
                            "sort_int DESC, LOWER(sort_str) DESC, resource_id DESC",
                        )
                    } else {
                        (
                            r#"(  $3::integer IS NULL
                               OR sort_int > $3
                               OR (sort_int = $3 AND LOWER(sort_str) > $2)
                               OR (sort_int = $3 AND LOWER(sort_str) = $2 AND resource_id > $5::uuid))"#,
                            "sort_int ASC, LOWER(sort_str) ASC, resource_id ASC",
                        )
                    }
                } else if reverse {
                    (
                        r#"(  $2::text IS NULL
                           OR LOWER(sort_str) < $2
                           OR (LOWER(sort_str) = $2 AND resource_id < $5::uuid))"#,
                        "LOWER(sort_str) DESC, resource_id DESC",
                    )
                } else {
                    (
                        r#"(  $2::text IS NULL
                           OR LOWER(sort_str) > $2
                           OR (LOWER(sort_str) = $2 AND resource_id > $5::uuid))"#,
                        "LOWER(sort_str) ASC, resource_id ASC",
                    )
                };
                format!(
                    r#"WITH resource_page AS (
                        SELECT ag.resource_type, ag.resource_id, MIN(ag.granted_at) AS first_shared_at,
                               COALESCE(
                                   CASE WHEN ag.resource_type = 'folder' THEN f.name  END,
                                   CASE WHEN ag.resource_type = 'file'   THEN fi.name END
                               ) AS sort_str,
                               {sort_int_expr} AS sort_int
                        FROM storage.role_grants ag
                        LEFT JOIN storage.folders f  ON f.id  = ag.resource_id AND ag.resource_type = 'folder'
                        LEFT JOIN storage.files   fi ON fi.id = ag.resource_id AND ag.resource_type = 'file'
                        WHERE ag.granted_by = $1
                        GROUP BY ag.resource_type, ag.resource_id, f.name, fi.name, fi.category_order
                    ),
                    rp AS (
                        SELECT * FROM resource_page
                        WHERE {page_where}
                        ORDER BY {page_order}
                        LIMIT $6
                    )
                    SELECT ag.resource_type, ag.resource_id, rp.first_shared_at,
                           ag.subject_type, ag.subject_id,
                           COALESCE(u.username, u.email, sg.name::text, sh.item_name, fi.name, fld.name, ag.subject_id::text) AS subject_display,
                           ag.id AS grant_id, ag.granted_at, ag.expires_at, ag.role::text AS role,
                           rp.sort_str, rp.sort_int,
                           (sh.password_hash IS NOT NULL) AS has_password,
                           COALESCE(u.is_external, FALSE) AS is_external
                    FROM rp
                    JOIN storage.role_grants ag
                      ON ag.resource_type = rp.resource_type AND ag.resource_id = rp.resource_id
                     AND ag.granted_by = $1
                    LEFT JOIN auth.users u   ON ag.subject_type = 'user'  AND u.id   = ag.subject_id
                    LEFT JOIN auth.subject_groups sg ON ag.subject_type = 'group' AND sg.id = ag.subject_id
                    LEFT JOIN storage.shares sh  ON ag.subject_type = 'token' AND sh.id  = ag.subject_id
                    LEFT JOIN storage.files fi   ON ag.subject_type = 'token' AND ag.resource_type = 'file'   AND fi.id  = ag.resource_id
                    LEFT JOIN storage.folders fld ON ag.subject_type = 'token' AND ag.resource_type = 'folder' AND fld.id = ag.resource_id
                    -- Per-resource grant ordering: groups → users → password-protected
                    -- links → public links (matches the "Shared with" subject sort).
                    -- Resource ordering comes from {page_order}; the CASE only
                    -- breaks ties within one resource.
                    ORDER BY {page_order},
                             CASE
                                 WHEN ag.subject_type = 'group' THEN 0
                                 WHEN ag.subject_type = 'user' THEN 1
                                 WHEN ag.subject_type = 'token' AND sh.password_hash IS NOT NULL THEN 2
                                 ELSE 3
                             END ASC,
                             LOWER(COALESCE(u.username, u.email, sg.name::text, sh.item_name, ag.subject_id::text)) ASC,
                             ag.granted_at"#
                )
            }
            "subject" => {
                // Page on (subject_type_order, subject_display, resource_id) triples so
                // every swimlane is always contiguous across cursor pages.
                //
                // subject_type_order: 0 = group, 1 = user, 2 = token with password,
                //                     3 = token without password
                // — picked so the My Shares "Shared with" view naturally renders the
                // higher-trust principals (groups, then named users) above the
                // lower-trust ones (anonymous link tokens).
                //
                // Cursor encodes: sort_int = subject_type_order, resource_name = LOWER(subject_display),
                // resource_id = last resource_id.
                let (page_where, page_order) = if reverse {
                    (
                        r#"(  $3::bigint IS NULL
                          OR sort_int < $3
                          OR (sort_int = $3 AND LOWER(subject_display) < $2)
                          OR (sort_int = $3 AND LOWER(subject_display) = $2 AND resource_id < $5::uuid))"#,
                        "sort_int DESC, LOWER(subject_display) DESC, resource_id DESC",
                    )
                } else {
                    (
                        r#"(  $3::bigint IS NULL
                          OR sort_int > $3
                          OR (sort_int = $3 AND LOWER(subject_display) > $2)
                          OR (sort_int = $3 AND LOWER(subject_display) = $2 AND resource_id > $5::uuid))"#,
                        "sort_int ASC, LOWER(subject_display) ASC, resource_id ASC",
                    )
                };
                format!(
                    r#"WITH pairs AS (
                        SELECT
                            ag.resource_type,
                            ag.resource_id,
                            ag.subject_type,
                            ag.subject_id,
                            MAX(COALESCE(u.username, u.email, sg.name::text, sh.item_name, ag.subject_id::text)) AS subject_display,
                            BOOL_OR(sh.password_hash IS NOT NULL) AS has_password,
                            COALESCE(BOOL_OR(u.is_external), FALSE) AS is_external,
                            MAX(CASE
                                WHEN ag.subject_type = 'group' THEN 0
                                WHEN ag.subject_type = 'user' THEN 1
                                WHEN ag.subject_type = 'token' AND sh.password_hash IS NOT NULL THEN 2
                                ELSE 3
                            END)::bigint AS sort_int,
                            MIN(ag.granted_at) AS first_granted_at
                        FROM storage.role_grants ag
                        LEFT JOIN auth.users u
                               ON ag.subject_type = 'user' AND u.id = ag.subject_id
                        LEFT JOIN auth.subject_groups sg
                               ON ag.subject_type = 'group' AND sg.id = ag.subject_id
                        LEFT JOIN storage.shares sh
                               ON ag.subject_type = 'token' AND sh.id = ag.subject_id
                        LEFT JOIN storage.files fi
                               ON ag.subject_type = 'token' AND ag.resource_type = 'file' AND fi.id = ag.resource_id
                        LEFT JOIN storage.folders fld
                               ON ag.subject_type = 'token' AND ag.resource_type = 'folder' AND fld.id = ag.resource_id
                        WHERE ag.granted_by = $1
                          AND (ag.expires_at IS NULL OR ag.expires_at > NOW())
                        GROUP BY ag.resource_type, ag.resource_id, ag.subject_type, ag.subject_id
                    ),
                    rp AS (
                        SELECT * FROM pairs
                        WHERE {page_where}
                        ORDER BY {page_order}
                        LIMIT $6
                    )
                    SELECT
                        ag.resource_type,
                        ag.resource_id,
                        rp.first_granted_at    AS first_shared_at,
                        ag.subject_type,
                        ag.subject_id,
                        rp.subject_display,
                        ag.id                  AS grant_id,
                        ag.granted_at,
                        ag.expires_at,
                        ag.role::text          AS role,
                        LOWER(rp.subject_display) AS sort_str,
                        rp.sort_int,
                        rp.has_password,
                        rp.is_external
                    FROM rp
                    JOIN storage.role_grants ag
                      ON ag.resource_type = rp.resource_type
                     AND ag.resource_id   = rp.resource_id
                     AND ag.subject_type  = rp.subject_type
                     AND ag.subject_id    = rp.subject_id
                     AND ag.granted_by    = $1
                     AND (ag.expires_at IS NULL OR ag.expires_at > NOW())
                    ORDER BY {page_order}"#
                )
            }
            "role" => {
                // Page on (role_order, subject_display, resource_id) triples so that all
                // of one person's grants within a role are contiguous — enabling aggregation
                // ("Bob on Folder A, Folder B") to work correctly across cursor pages.
                //
                // role_order matches the `storage.grant_role` ENUM declaration
                // order (strongest first) via `array_position`, so
                // `sort_int ASC` matches the UX requirement: 1 = owner,
                // 2 = editor, 3 = contributor, 4 = commenter, 5 = viewer.
                // 1-based because `array_position` is.
                // Cursor: sort_int=role_order, resource_name=LOWER(subject_display), resource_id
                let (page_where, page_order) = if reverse {
                    (
                        r#"(  $3::bigint IS NULL
                          OR sort_int < $3
                          OR (sort_int = $3 AND LOWER(subject_display) < $2)
                          OR (sort_int = $3 AND LOWER(subject_display) = $2 AND resource_id < $5::uuid))"#,
                        "sort_int DESC, LOWER(subject_display) DESC, resource_id DESC",
                    )
                } else {
                    (
                        r#"(  $3::bigint IS NULL
                          OR sort_int > $3
                          OR (sort_int = $3 AND LOWER(subject_display) > $2)
                          OR (sort_int = $3 AND LOWER(subject_display) = $2 AND resource_id > $5::uuid))"#,
                        "sort_int ASC, LOWER(subject_display) ASC, resource_id ASC",
                    )
                };
                format!(
                    r#"WITH pairs AS (
                        SELECT
                            ag.resource_type,
                            ag.resource_id,
                            ag.subject_type,
                            ag.subject_id,
                            MAX(COALESCE(u.username, u.email, sh.item_name, ag.subject_id::text)) AS subject_display,
                            BOOL_OR(sh.password_hash IS NOT NULL) AS has_password,
                            COALESCE(BOOL_OR(u.is_external), FALSE) AS is_external,
                            -- One role per (resource, subject) post-D-Prep
                            -- (UNIQUE constraint on role_grants), so MAX
                            -- returns that single row's role. `array_position`
                            -- against the ENUM's declaration order produces a
                            -- 1-based rank: owner=1 → viewer=5. Strength
                            -- ordering tracks the ENUM declaration — adding
                            -- a new role between owner and viewer doesn't
                            -- need a parallel CASE update here.
                            array_position(
                                enum_range(NULL::storage.grant_role),
                                MAX(ag.role)
                            )::bigint AS sort_int,
                            MIN(ag.granted_at) AS first_granted_at
                        FROM storage.role_grants ag
                        LEFT JOIN auth.users u
                               ON ag.subject_type = 'user' AND u.id = ag.subject_id
                        LEFT JOIN storage.shares sh
                               ON ag.subject_type = 'token' AND sh.id = ag.subject_id
                        LEFT JOIN storage.files fi
                               ON ag.subject_type = 'token' AND ag.resource_type = 'file' AND fi.id = ag.resource_id
                        LEFT JOIN storage.folders fld
                               ON ag.subject_type = 'token' AND ag.resource_type = 'folder' AND fld.id = ag.resource_id
                        WHERE ag.granted_by = $1
                          AND (ag.expires_at IS NULL OR ag.expires_at > NOW())
                        GROUP BY ag.resource_type, ag.resource_id, ag.subject_type, ag.subject_id
                    ),
                    rp AS (
                        SELECT * FROM pairs
                        WHERE {page_where}
                        ORDER BY {page_order}
                        LIMIT $6
                    )
                    SELECT
                        ag.resource_type,
                        ag.resource_id,
                        rp.first_granted_at    AS first_shared_at,
                        ag.subject_type,
                        ag.subject_id,
                        rp.subject_display,
                        ag.id                  AS grant_id,
                        ag.granted_at,
                        ag.expires_at,
                        ag.role::text          AS role,
                        LOWER(rp.subject_display) AS sort_str,
                        rp.sort_int,
                        rp.has_password,
                        rp.is_external
                    FROM rp
                    JOIN storage.role_grants ag
                      ON ag.resource_type = rp.resource_type
                     AND ag.resource_id   = rp.resource_id
                     AND ag.subject_type  = rp.subject_type
                     AND ag.subject_id    = rp.subject_id
                     AND ag.granted_by    = $1
                     AND (ag.expires_at IS NULL OR ag.expires_at > NOW())
                    ORDER BY {page_order}"#
                )
            }
            _ => {
                // Default: sort by first_shared_at DESC (newest resource shared first).
                let (page_where, page_order) = if reverse {
                    (
                        r#"(  $4::timestamptz IS NULL
                          OR first_shared_at > $4
                          OR (first_shared_at = $4 AND resource_id > $5::uuid))"#,
                        "first_shared_at ASC, resource_id ASC",
                    )
                } else {
                    (
                        r#"(  $4::timestamptz IS NULL
                          OR first_shared_at < $4
                          OR (first_shared_at = $4 AND resource_id < $5::uuid))"#,
                        "first_shared_at DESC, resource_id DESC",
                    )
                };
                format!(
                    r#"WITH resource_page AS (
                        SELECT resource_type, resource_id, MIN(granted_at) AS first_shared_at,
                               NULL::text   AS sort_str,
                               NULL::bigint AS sort_int
                        FROM storage.role_grants
                        WHERE granted_by = $1
                        GROUP BY resource_type, resource_id
                    ),
                    rp AS (
                        SELECT * FROM resource_page
                        WHERE {page_where}
                        ORDER BY {page_order}
                        LIMIT $6
                    )
                    SELECT ag.resource_type, ag.resource_id, rp.first_shared_at,
                           ag.subject_type, ag.subject_id,
                           COALESCE(u.username, u.email, sh.item_name, fi.name, fld.name, ag.subject_id::text) AS subject_display,
                           ag.id AS grant_id, ag.granted_at, ag.expires_at, ag.role::text AS role,
                           NULL::text AS sort_str, NULL::bigint AS sort_int,
                           (sh.password_hash IS NOT NULL) AS has_password,
                           COALESCE(u.is_external, FALSE) AS is_external
                    FROM rp
                    JOIN storage.role_grants ag
                      ON ag.resource_type = rp.resource_type AND ag.resource_id = rp.resource_id
                     AND ag.granted_by = $1
                    LEFT JOIN auth.users u    ON ag.subject_type = 'user'  AND u.id  = ag.subject_id
                    LEFT JOIN storage.shares sh   ON ag.subject_type = 'token' AND sh.id  = ag.subject_id
                    LEFT JOIN storage.files fi    ON ag.subject_type = 'token' AND ag.resource_type = 'file'   AND fi.id  = ag.resource_id
                    LEFT JOIN storage.folders fld ON ag.subject_type = 'token' AND ag.resource_type = 'folder' AND fld.id = ag.resource_id
                    ORDER BY {page_order}, ag.subject_id, ag.granted_at"#
                )
            }
        };

        let rows: Vec<Row> = sqlx::query_as::<_, Row>(&sql)
            .bind(granted_by) // $1
            .bind(&cursor_str) // $2 sort_str cursor
            .bind(cursor_int) // $3 sort_int cursor
            .bind(cursor_at) // $4 first_shared_at cursor
            .bind(cursor_id) // $5 resource_id cursor
            .bind(fetch_limit) // $6
            .fetch_all(self.pool.as_ref())
            .await
            .map_err(|e| {
                DomainError::internal_error(
                    "PgAcl",
                    format!("list_outgoing_resources_paged ({sort_by}): {e}"),
                )
            })?;

        // ── Subject / Role sorts: page on (resource_id, subject_id) pairs ───────
        // Each pair becomes one OutgoingResourceSummary with exactly one grant,
        // preserving the SQL-ordered swimlane sequence across cursor pages.
        if matches!(sort_by, "subject" | "role") {
            let mut seen_pairs: Vec<(Uuid, Uuid)> = Vec::new();
            let mut seen_pair_set: std::collections::HashSet<(Uuid, Uuid)> =
                std::collections::HashSet::new();
            for r in &rows {
                if seen_pair_set.insert((r.1, r.4)) {
                    seen_pairs.push((r.1, r.4));
                }
            }
            let has_next = seen_pairs.len() > limit as usize;
            seen_pairs.truncate(limit as usize);
            let keep: std::collections::HashSet<(Uuid, Uuid)> =
                seen_pairs.iter().copied().collect();

            let last_row = rows.iter().rfind(|r| keep.contains(&(r.1, r.4)));
            let next_cursor = if has_next {
                last_row.map(|r| {
                    let resource_name = r.10.clone(); // LOWER(subject_display) for both subject and role sort
                    GrantCursor {
                        sort_by: sort_by.to_owned(),
                        granted_at: r.2,
                        resource_id: r.1,
                        resource_name,
                        sort_int: r.11,
                        reverse,
                    }
                })
            } else {
                None
            };

            // Group rows: (resource_id, subject_id) → OutgoingGrantEntry.
            let mut entry_map: std::collections::HashMap<
                (Uuid, Uuid),
                (ResourceKind, OutgoingGrantEntry),
            > = std::collections::HashMap::new();
            for r in rows.into_iter().filter(|r| keep.contains(&(r.1, r.4))) {
                let (
                    rt_str,
                    resource_id,
                    _first_shared_at,
                    subj_type,
                    subj_id,
                    subj_display,
                    grant_id,
                    granted_at,
                    expires_at,
                    role_str,
                    _,
                    _,
                    has_password,
                    is_external,
                ) = r;
                let Some(resource_type) = ResourceKind::parse(&rt_str) else {
                    continue;
                };
                let Some(role) = Role::parse(&role_str) else {
                    continue;
                };
                let key = (resource_id, subj_id);
                let (_, entry) = entry_map.entry(key).or_insert_with(|| {
                    (
                        resource_type,
                        OutgoingGrantEntry {
                            grant_id,
                            subject_type: subj_type.clone(),
                            subject_id: subj_id,
                            subject_display: subj_display.clone(),
                            permissions: Vec::new(),
                            granted_at,
                            expires_at,
                            has_password,
                            is_external,
                        },
                    )
                });
                for &perm in role.expand() {
                    if !entry.permissions.contains(&perm) {
                        entry.permissions.push(perm);
                    }
                }
            }

            let summaries: Vec<OutgoingResourceSummary> = seen_pairs
                .into_iter()
                .filter_map(|(rid, sid)| {
                    let (resource_type, grant) = entry_map.remove(&(rid, sid))?;
                    Some(OutgoingResourceSummary {
                        resource_type,
                        resource_id: rid,
                        first_shared_at: grant.granted_at,
                        grants: vec![grant],
                    })
                })
                .collect();

            return Ok((summaries, next_cursor));
        }

        // ── All other sorts: page on distinct resource_ids ────────────────────
        let mut seen_resources: Vec<Uuid> = Vec::new();
        let mut seen_set: std::collections::HashSet<Uuid> = std::collections::HashSet::new();
        for r in &rows {
            if seen_set.insert(r.1) {
                seen_resources.push(r.1);
            }
        }

        let has_next = seen_resources.len() > limit as usize;
        seen_resources.truncate(limit as usize);
        let keep: std::collections::HashSet<Uuid> = seen_resources.iter().copied().collect();

        let last_row = rows.iter().rfind(|r| keep.contains(&r.1));
        let next_cursor = if has_next {
            last_row.map(|r| {
                let sort_str_lc = r.10.as_deref().map(str::to_lowercase);
                match sort_by {
                    "name" => GrantCursor {
                        sort_by: "name".to_owned(),
                        granted_at: r.2,
                        resource_id: r.1,
                        resource_name: sort_str_lc,
                        sort_int: None,
                        reverse,
                    },
                    "type" => GrantCursor {
                        sort_by: "type".to_owned(),
                        granted_at: r.2,
                        resource_id: r.1,
                        resource_name: sort_str_lc,
                        sort_int: r.11,
                        reverse,
                    },
                    _ => GrantCursor {
                        sort_by: "first_shared_at".to_owned(),
                        granted_at: r.2,
                        resource_id: r.1,
                        resource_name: None,
                        sort_int: None,
                        reverse,
                    },
                }
            })
        } else {
            None
        };

        // Group flat rows by resource_id → (ResourceKind, first_shared_at, subjects).
        type ResourceEntry = (
            ResourceKind,
            chrono::DateTime<chrono::Utc>,
            std::collections::HashMap<Uuid, OutgoingGrantEntry>,
        );
        let mut resource_map: std::collections::HashMap<Uuid, ResourceEntry> =
            std::collections::HashMap::new();

        for r in rows.into_iter().filter(|r| keep.contains(&r.1)) {
            let (
                rt_str,
                resource_id,
                first_shared_at,
                subj_type,
                subj_id,
                subj_display,
                grant_id,
                granted_at,
                expires_at,
                role_str,
                _,
                _,
                has_password,
                is_external,
            ) = r;
            let Some(resource_type) = ResourceKind::parse(&rt_str) else {
                continue;
            };
            let Some(role) = Role::parse(&role_str) else {
                continue;
            };

            let (_, _, subj_map) = resource_map.entry(resource_id).or_insert_with(|| {
                (
                    resource_type,
                    first_shared_at,
                    std::collections::HashMap::new(),
                )
            });
            let entry = subj_map
                .entry(subj_id)
                .or_insert_with(|| OutgoingGrantEntry {
                    grant_id,
                    subject_type: subj_type.clone(),
                    subject_id: subj_id,
                    subject_display: subj_display.clone(),
                    permissions: Vec::new(),
                    granted_at,
                    expires_at,
                    has_password,
                    is_external,
                });
            for &perm in role.expand() {
                if !entry.permissions.contains(&perm) {
                    entry.permissions.push(perm);
                }
            }
        }

        let summaries: Vec<OutgoingResourceSummary> = seen_resources
            .into_iter()
            .filter_map(|rid| {
                let (resource_type, first_shared_at, subj_map) = resource_map.remove(&rid)?;
                let mut grants: Vec<OutgoingGrantEntry> = subj_map.into_values().collect();
                // Per-resource subject ordering (matches the subject-sort
                // branch's SQL CASE):
                //   0 = group, 1 = user, 2 = token-with-password, 3 = token,
                //   4 = external.
                // Alphabetical tiebreak by display name. This intentionally
                // ignores role/permission tier — the share dialog renders
                // role as a separate pill; ordering by subject type is the
                // UX contract.
                let subject_rank = |e: &OutgoingGrantEntry| -> u8 {
                    match e.subject_type.as_str() {
                        "group" => 0,
                        "user" => 1,
                        "token" if e.has_password => 2,
                        "token" => 3,
                        _ => 4,
                    }
                };
                grants.sort_by(|a, b| {
                    subject_rank(a).cmp(&subject_rank(b)).then_with(|| {
                        a.subject_display
                            .to_lowercase()
                            .cmp(&b.subject_display.to_lowercase())
                    })
                });
                Some(OutgoingResourceSummary {
                    resource_type,
                    resource_id: rid,
                    first_shared_at,
                    grants,
                })
            })
            .collect();

        Ok((summaries, next_cursor))
    }

    async fn list_outgoing_grants(&self, granted_by: Uuid) -> Result<Vec<Grant>, DomainError> {
        // Pivoted to `storage.role_grants` (see `list_incoming_grants`).
        // Group membership doesn't apply on the outgoing side — we
        // filter by `granted_by` directly. Bundle expansion still
        // happens at read time via `role_row_to_grants` until the
        // public `Grant` shape becomes role-keyed.
        let rows = sqlx::query_as::<
            _,
            (
                Uuid,
                String,
                Uuid,
                String,
                Uuid,
                String,
                Uuid,
                chrono::DateTime<chrono::Utc>,
                Option<chrono::DateTime<chrono::Utc>>,
            ),
        >(
            r#"
            SELECT id, subject_type, subject_id, resource_type, resource_id,
                   role::text, granted_by, granted_at, expires_at
              FROM storage.role_grants
             WHERE granted_by = $1
             ORDER BY role ASC, granted_at DESC
            "#,
        )
        .bind(granted_by)
        .fetch_all(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("PgAcl", format!("list outgoing: {e}")))?;

        rows.into_iter().map(Self::row_to_grant).collect()
    }

    async fn set_expiry_for_subject(
        &self,
        subject: Subject,
        expires_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<(), DomainError> {
        sqlx::query(
            "UPDATE storage.role_grants SET expires_at = $3 \
             WHERE subject_type = $1 AND subject_id = $2",
        )
        .bind(subject.type_str())
        .bind(subject.id())
        .bind(expires_at)
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| {
            DomainError::internal_error("PgAcl", format!("set_expiry_for_subject: {e}"))
        })?;
        Ok(())
    }

    async fn purge_expired_grants(&self, grace_days: u32) -> Result<u64, DomainError> {
        // Uses the partial index `idx_role_grants_expires_at` (migration
        // 20260730000000), which covers `WHERE expires_at IS NOT NULL`
        // — so this DELETE only touches indexed rows even when the
        // `role_grants` table has tens of millions of permanent grants.
        //
        // Grace days is bound as bigint and multiplied into an
        // interval — parameterised, no injection surface. u32 → i64
        // is loss-free.
        let result = sqlx::query(
            "DELETE FROM storage.role_grants \
              WHERE expires_at IS NOT NULL \
                AND expires_at < NOW() - ($1::bigint * INTERVAL '1 day')",
        )
        .bind(grace_days as i64)
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("PgAcl", format!("purge_expired_grants: {e}")))?;
        Ok(result.rows_affected())
    }

    async fn revoke(&self, grant_id: Uuid) -> Result<(), DomainError> {
        sqlx::query("DELETE FROM storage.role_grants WHERE id = $1")
            .bind(grant_id)
            .execute(self.pool.as_ref())
            .await
            .map_err(|e| DomainError::internal_error("PgAcl", format!("revoke: {e}")))?;
        Ok(())
    }

    async fn revoke_all_for_resource(&self, resource: Resource) -> Result<usize, DomainError> {
        let result = sqlx::query(
            "DELETE FROM storage.role_grants WHERE resource_type = $1 AND resource_id = $2",
        )
        .bind(resource.type_str())
        .bind(resource.id())
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("PgAcl", format!("revoke for resource: {e}")))?;

        Ok(result.rows_affected() as usize)
    }

    async fn revoke_all_for_subject(&self, subject: Subject) -> Result<usize, DomainError> {
        let result = sqlx::query(
            "DELETE FROM storage.role_grants WHERE subject_type = $1 AND subject_id = $2",
        )
        .bind(subject.type_str())
        .bind(subject.id())
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("PgAcl", format!("revoke for subject: {e}")))?;

        Ok(result.rows_affected() as usize)
    }

    // ── D-Prep role_grants writes ──────────────────────────────────────────

    async fn set_role(
        &self,
        granted_by: Uuid,
        subject: Subject,
        role: Role,
        resource: Resource,
        expires_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<Grant, DomainError> {
        let row = sqlx::query_as::<
            _,
            (
                Uuid,
                String,
                Uuid,
                String,
                Uuid,
                String,
                Uuid,
                chrono::DateTime<chrono::Utc>,
                Option<chrono::DateTime<chrono::Utc>>,
            ),
        >(
            r#"
            INSERT INTO storage.role_grants
                (subject_type, subject_id, resource_type, resource_id,
                 role, granted_by, expires_at)
            VALUES ($1, $2, $3, $4, $5::storage.grant_role, $6, $7)
            ON CONFLICT (subject_type, subject_id, resource_type, resource_id)
            DO UPDATE SET role       = EXCLUDED.role,
                          expires_at = EXCLUDED.expires_at,
                          granted_by = EXCLUDED.granted_by
            RETURNING id, subject_type, subject_id, resource_type, resource_id,
                      role::text, granted_by, granted_at, expires_at
            "#,
        )
        .bind(subject.type_str())
        .bind(subject.id())
        .bind(resource.type_str())
        .bind(resource.id())
        .bind(role.as_str())
        .bind(granted_by)
        .bind(expires_at)
        .fetch_one(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("PgAcl", format!("set_role: {e}")))?;

        // Membership write on a drive — drop every cached
        // `(subject, drive_id)` entry pointing at this drive so the next
        // authz check resolves against the fresh role.
        if let Resource::Drive(drive_id) = resource {
            self.invalidate_drive_role_cache_for_drive(drive_id).await;
        }
        // File/Folder grant write — a new share can widen the cascade
        // decision for descendant files; flush the cascade cache so the next
        // thumbnail/read check sees it immediately.
        if matches!(resource, Resource::File(_) | Resource::Folder(_)) {
            self.invalidate_cascade_grant_cache_all().await;
        }
        // Same immediacy contract for the memoised top-level decisions.
        if matches!(
            resource,
            Resource::Calendar(_) | Resource::AddressBook(_) | Resource::Playlist(_)
        ) {
            self.direct_grant_cache.invalidate_all();
        }

        Self::row_to_grant(row)
    }

    async fn clear_role(&self, subject: Subject, resource: Resource) -> Result<(), DomainError> {
        sqlx::query(
            "DELETE FROM storage.role_grants \
             WHERE subject_type = $1 AND subject_id = $2 \
               AND resource_type = $3 AND resource_id = $4",
        )
        .bind(subject.type_str())
        .bind(subject.id())
        .bind(resource.type_str())
        .bind(resource.id())
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("PgAcl", format!("clear_role: {e}")))?;

        // Symmetric with `set_role` — drop cached drive-role entries on
        // membership revocation so a viewer who just got removed doesn't
        // keep passing the precheck for up to 30 s.
        if let Resource::Drive(drive_id) = resource {
            self.invalidate_drive_role_cache_for_drive(drive_id).await;
        }
        // Revoking a File/Folder share must stop passing the cascade check
        // now, not in ≤30 s — flush the cascade cache (see `set_role`).
        if matches!(resource, Resource::File(_) | Resource::Folder(_)) {
            self.invalidate_cascade_grant_cache_all().await;
        }
        // A revoked calendar/address-book/playlist grant must fail the next
        // check now, not in ≤30 s (see `set_role`).
        if matches!(
            resource,
            Resource::Calendar(_) | Resource::AddressBook(_) | Resource::Playlist(_)
        ) {
            self.direct_grant_cache.invalidate_all();
        }

        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// AuthzCacheLifecycleHook
//
// Owns invalidation of the `user_groups_cache` Moka entry when a user's
// state changes in ways that affect transitive-group expansion (logout
// — so a re-login with new group memberships doesn't observe a stale
// expansion during the 30 s TTL window; delete — so a re-created
// account with the same id doesn't inherit the old cached value).
//
// Lives in this file (not under a centralised `lifecycle/` directory)
// because the authz engine owns its own cache invariants. See the
// "owner-located convention" note in
// `docs/architecture/user-lifecycle.md`.
// ─────────────────────────────────────────────────────────────────────────────

use async_trait::async_trait;

use crate::application::ports::user_lifecycle::{DeletionMode, LogoutReason, UserLifecycleHook};
use crate::domain::entities::user::User;

/// Lifecycle hook: drops the `user_groups_cache` entry for one user on
/// logout / deletion so the next authz check rebuilds it from current
/// `subject_group_members` rows.
pub struct AuthzCacheLifecycleHook {
    engine: Arc<PgAclEngine>,
}

impl AuthzCacheLifecycleHook {
    pub fn new(engine: Arc<PgAclEngine>) -> Self {
        Self { engine }
    }
}

#[async_trait]
impl UserLifecycleHook for AuthzCacheLifecycleHook {
    fn name(&self) -> &'static str {
        "authz_cache"
    }

    async fn on_user_created(&self, _user: &User) -> Result<(), DomainError> {
        // New user can't have a stale cache entry (no prior `expand_user`
        // call has produced one). Explicit no-op per the trait convention.
        Ok(())
    }

    async fn on_user_login(&self, _user: &User) -> Result<(), DomainError> {
        // Login doesn't change group membership; the cache (if present)
        // is still correct.
        Ok(())
    }

    async fn on_user_logout(&self, user: &User, _reason: LogoutReason) -> Result<(), DomainError> {
        self.engine.invalidate_user_groups_cache(user.id()).await;
        Ok(())
    }

    async fn on_user_deleted(
        &self,
        user: &User,
        _mode: DeletionMode,
        _tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<(), DomainError> {
        // No DB writes here — just memory invalidation. `_tx` is
        // intentionally ignored. The DB cascade
        // (`trg_cleanup_role_grants_user`) already dropped every
        // role_grants row for this subject; we mirror that cleanup on
        // both authz caches:
        //   1. `user_groups_cache` — recomputed group expansion.
        //   2. `drive_role_cache` — cached "user X → drive Y = role R"
        //      entries seeded by prior authz checks. Without this
        //      the deleted user's role stays visible in-process for
        //      up to the cache TTL (~30 s).
        self.engine.invalidate_user_groups_cache(user.id()).await;
        self.engine
            .invalidate_drive_role_cache_for_subject(Subject::User(user.id()))
            .await;
        Ok(())
    }
}
