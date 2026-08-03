//! User-lifecycle hook port.
//!
//! Observer notified by [`AuthApplicationService`] when a user transitions
//! through one of four lifecycle events: created, login, logout, deleted.
//! Register concrete impls with [`UserLifecycleService`] during DI wiring;
//! the dispatcher fans out each event to every registered hook.
//!
//! Each impl owns ONE concern. Folder service owns home-folder provisioning,
//! authz engine owns its cache invalidation, audit service owns the audit
//! trail, etc. New services plug in by registering a hook; the dispatcher
//! itself never gains domain knowledge.
//!
//! # Convention: explicit no-ops
//!
//! Every implementor **must** provide all four methods — use an explicit
//! one-liner `Ok(())` for events the implementor does not care about. This
//! forces conscious acknowledgement of every lifecycle event rather than
//! silent omission via trait defaults. Mirrors the [`FileLifecycleHook`]
//! convention at `application/ports/file_lifecycle.rs`.
//!
//! # Convention: async + per-event semantics
//!
//! Unlike [`FileLifecycleHook`] (sync fire-and-forget), user-lifecycle
//! events are async because some require synchronous semantics:
//! provisioning must finish before the session token is returned;
//! deletion cleanup must commit atomically with the user DELETE.
//!
//! Per-event failure model (encoded in the dispatcher, not the trait):
//!
//! | Event              | Awaited? | On `Err`?                              |
//! |--------------------|----------|----------------------------------------|
//! | `on_user_created`  | yes      | log-and-continue (retry on next login) |
//! | `on_user_login`    | yes      | log-and-continue (idempotent retry)    |
//! | `on_user_logout`   | no       | fire-and-forget (spawned), error logged|
//! | `on_user_deleted`  | yes (in tx) | abort the transaction (Err propagates) |
//!
//! # Tips for hook implementors
//!
//! 1. **First-ever login detection.** `on_user_login` fires after
//!    credentials validate but **before** `user.register_login()` is
//!    called for this session. So `user.last_login_at().is_none()` is a
//!    reliable "this is the first login since account creation" signal —
//!    useful for welcome emails, one-shot default-resource seeding,
//!    "complete your profile" prompts.
//!
//! 2. **External-user short-circuit.** Every hook that provisions or
//!    manages user-owned resources (folders, calendars, address books)
//!    should start with `if user.is_external() { return Ok(()); }`.
//!    External users are grant-only — they don't own storage. The
//!    `is_external` flag lands in PR 2 of this work; until then, treat
//!    every user as internal.
//!
//! 3. **Idempotency is mandatory.** `on_user_login` fires on every
//!    successful authentication. A hook that creates a resource must
//!    first check whether the resource already exists. Same for cache
//!    invalidation, audit deduplication, etc. The `on_user_login`
//!    safety-net only works if hooks no-op when their work is already
//!    done.
//!
//! 4. **External → internal conversion needs no special event.** When
//!    admin converts an external user to internal (`UPDATE auth.users
//!    SET is_external = FALSE`), the user's next login fires
//!    `on_user_login` with the new flag value. Idempotent hooks see
//!    `!is_external` + missing resources → provision. No
//!    `on_user_converted` method needed.
//!
//! 5. **Per-session logout firing.** When a flow revokes multiple
//!    sessions (e.g. `revoke_all_user_sessions` on password change),
//!    today the dispatcher fires `on_user_logout` ONCE per logical
//!    revoke-call. PR 4's `SessionRevocationLifecycleHook` will refine
//!    this to once-per-session for proper audit granularity. Hooks must
//!    therefore accept N redundant calls with the same reason — keep
//!    them idempotent and side-effect-free.
//!
//! 6. **Failure swallowing on create/login.** If your hook returns
//!    `Err`, the user is still created / logged in; only your hook's
//!    effect is delayed. Log enough detail via `tracing::error!` that a
//!    subsequent investigation can identify the user and retry
//!    manually. The `on_user_login` safety-net will retry on the next
//!    successful authentication.
//!
//! 7. **`on_user_deleted` runs inside the delete transaction.** The
//!    user row still exists when the hook fires; the dispatcher commits
//!    only after every hook returns `Ok(())`. Returning `Err` aborts
//!    the whole transaction — including the user DELETE itself.
//!    Implementors get `tx: &mut sqlx::Transaction<'_, Postgres>` so
//!    cleanup queries land in the same tx (e.g. session revocation
//!    with audit trail before FK CASCADE wipes the rows). Be
//!    conservative about returning `Err`: an abort means the admin's
//!    delete operation fails, leaving the user intact.
//!
//! 8. **Hook order is registration order.** The DI factory at
//!    [`AppServiceFactory`] determines the firing sequence. If two hooks
//!    have an ordering dependency (e.g. home-folder must exist before
//!    default-calendar can be seeded inside it), the dependent hook
//!    registers AFTER the producer. Document the convention in the DI
//!    block where order matters.

use async_trait::async_trait;

use crate::common::errors::DomainError;
use crate::domain::entities::user::User;

/// Reason a user session is ending. Hooks that don't care about the cause
/// (e.g. cache invalidation) ignore the value; audit-style hooks branch on
/// it to emit distinguishable events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogoutReason {
    /// User clicked logout. Single-session.
    UserInitiated,
    /// Session TTL hit. Single-session.
    SessionExpired,
    /// Admin invoked single-session revocation (e.g. "log out other
    /// devices"). Today this fires from `logout_all` and from individual
    /// admin endpoints if/when they exist.
    AdminRevoked,
    /// `user.active` flipped to `FALSE` → all sessions revoked. Fires once
    /// per logical revoke-call today (see tip #5).
    AccountDisabled,
    /// Password was changed → sibling sessions invalidated to force re-login
    /// with the new password.
    PasswordChanged,
    /// Refresh-token reuse detected by the session-family guard. Entire
    /// family revoked because the rotation was probably stolen.
    TokenReused,
    /// OIDC Back-Channel Logout — the IdP notified us that a session
    /// ended on its side (user logged out on another RP, or admin
    /// revoked the SSO session). Session(s) revoked without any user
    /// action on OxiCloud itself.
    IdpNotification,
}

/// How aggressively `on_user_deleted` cleanup should run. Today both
/// variants are equivalent (only `AuditLifecycleHook` exists, and it logs
/// regardless). The split exists so PR 4's `PersonalDriveLifecycleHook` can
/// trash on `AdminDelete` but hard-delete on `GdprPurge`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeletionMode {
    /// Admin deletes a user through the UI. Resources move to trash for
    /// the retention window; recoverable.
    AdminDelete,
    /// GDPR right-to-erasure sweeper. Hard-delete everything; not
    /// recoverable. (No sweeper is wired today; the variant is reserved.)
    GdprPurge,
}

/// Observer for user-lifecycle events. See module-level docstring for the
/// convention, semantics, and 8 tips for implementors.
///
/// `#[async_trait]` is required to make the trait `dyn`-compatible —
/// the dispatcher holds `Arc<dyn UserLifecycleHook>`. Without it,
/// native `async fn in trait` returns an opaque type that has no vtable
/// representation. The same crate (`async-trait` 0.1.x) is used by other
/// async ecosystem deps and was already transitively in Cargo.lock.
#[async_trait]
pub trait UserLifecycleHook: Send + Sync {
    /// Short identifier used in tracing / error logs. Example: `"home_folder"`,
    /// `"audit"`, `"authz_cache"`.
    fn name(&self) -> &'static str;

    /// Fires once after INSERT into `auth.users` succeeds, regardless of
    /// the creation path (self-register, admin-create, OIDC JIT, future
    /// magic-link bootstrap).
    ///
    /// The dispatcher logs `Err` and continues — the user is still
    /// created and the next `on_user_login` will run an idempotent retry.
    async fn on_user_created(&self, user: &User) -> Result<(), DomainError>;

    /// Fires after every successful authentication, BEFORE the user's
    /// `last_login_at` is updated for this session and BEFORE the session
    /// token is returned to the caller.
    ///
    /// **Idempotency is mandatory** — this fires on every login, not just
    /// the first. Hooks that provision must check whether their resource
    /// already exists before creating it. See tip #3 in the module
    /// docstring.
    ///
    /// `user.last_login_at().is_none()` distinguishes the first-ever
    /// login from subsequent ones. See tip #1.
    async fn on_user_login(&self, user: &User) -> Result<(), DomainError>;

    /// Fires on session termination. `reason` lets hooks distinguish
    /// causes — audit cares; cache invalidation usually doesn't.
    ///
    /// Spawned by the dispatcher — `Err` is logged but never propagates.
    /// The HTTP response shouldn't wait for downstream cache flushes.
    async fn on_user_logout(&self, user: &User, reason: LogoutReason) -> Result<(), DomainError>;

    /// Fires inside the `delete_user_admin` transaction, BEFORE the
    /// `DELETE FROM auth.users` row removal. The user row still exists
    /// at this point; `user.id()` is safe to reference in queries on
    /// the same `tx`. Returning `Err` rolls back the transaction —
    /// the user is NOT deleted and the admin's request fails. See
    /// tip #7 in the module docstring.
    async fn on_user_deleted(
        &self,
        user: &User,
        mode: DeletionMode,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<(), DomainError>;

    /// Fires after `AuthApplicationService::upgrade_to_internal`
    /// successfully persists `is_external = false` on the user row —
    /// the external → internal conversion path. The `user` argument
    /// reflects the POST-upgrade state (`is_external() == false`,
    /// `storage_quota_bytes > 0`, `password_hash` maybe stamped).
    ///
    /// Load-bearing implementations:
    ///   * `PersonalDriveLifecycleHook` → provisions the home drive
    ///     (would have short-circuited on `on_user_created` because
    ///     the user was external at creation).
    ///   * `AuditLifecycleHook` → emits `event="auth.user_upgraded"`.
    ///
    /// Default: no-op. Hooks that don't care about upgrade don't need
    /// to opt in — this keeps the trait extension backwards-compatible
    /// with existing implementations. Do NOT reuse `on_user_created`
    /// for this event: hooks that observe `last_login_at().is_none()`
    /// as "first ever" or that clean up magic-link tokens
    /// (`ExternalIdentityLifecycleHook`) would mis-fire.
    ///
    /// Idempotency: fires exactly once per successful upgrade transition
    /// (guarded by `is_external` toggling). A retried upgrade after a
    /// crash would hit the `AlreadyInternal` guard in the service and
    /// this hook wouldn't fire again — so hooks may assume "first
    /// upgrade" semantics.
    async fn on_upgraded_to_internal(&self, _user: &User) -> Result<(), DomainError> {
        Ok(())
    }
}
