//! Outbound port for OPAQUE aPAKE envelope persistence.
//!
//! The registration record (encrypted "envelope" blob) and its
//! metadata live in three columns on `auth.users` — introduced by the
//! Phase 0 migration (`20261001000002_auth_opaque.sql`). This trait
//! wraps the row-level access so:
//!
//!   * the OPAQUE handlers (Phase 1+) depend on a small, mockable
//!     interface rather than a `PgPool`,
//!   * unit tests can drive envelope reads/writes without a live DB,
//!   * a future E2EE-phase migration can slot in per-device bridges
//!     alongside this trait without disturbing the OPAQUE auth path.
//!
//! The trait is deliberately narrow — only what the OPAQUE
//! registration + login flows need. Anything else that touches
//! `auth.users` still goes through [`UserStoragePort`].
//!
//! ## `clear_registration` and `force_password_change_at_next_login`
//!
//! When an operator resets a user's password (Phase 4+ admin flow), we
//! need to invalidate the existing OPAQUE envelope AND force the user
//! to pick a new passphrase on their next login — otherwise the
//! admin-set password becomes a durable credential. [`clear_registration`]
//! does both in one round-trip: NULLs the four OPAQUE columns AND
//! sets `force_password_change_at_next_login = TRUE`. Individual
//! callers should NOT set that flag independently to avoid drift
//! between the two writes.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::common::errors::Result;

/// Server-stored OPAQUE registration record for one user.
///
/// Rebuilt on every read from three columns: the envelope blob, the
/// ciphersuite version it was minted under, and the first-registration
/// timestamp. `opaque_migrated_at` is intentionally NOT here — it's a
/// login-time signal, not registration state.
#[derive(Debug, Clone)]
pub struct StoredEnvelope {
    /// Serialised `opaque_ke::RegistrationUpload` payload. Server-opaque;
    /// only the client with the correct passphrase can use it to complete
    /// the login handshake.
    pub envelope: Vec<u8>,
    /// The ciphersuite version this envelope was minted under. Handlers
    /// compare against
    /// [`OpaqueService::ciphersuite_version`](crate::infrastructure::services::opaque_service::OpaqueService::ciphersuite_version)
    /// and refuse login with a specific `error_type` when they diverge
    /// — the client must re-register under the current suite.
    pub ciphersuite_version: i16,
    /// When this account first minted an OPAQUE envelope. Preserved
    /// across re-registrations (password changes) via a NULL check in
    /// [`OpaqueRepositoryPort::write_registration`].
    pub registered_at: DateTime<Utc>,
    /// Client-side Argon2id KSF parameters the CLIENT declared at
    /// register time. `None` when the envelope predates the
    /// per-envelope-KSF migration (`20261005000000`) — callers fall
    /// back to the server's current `OpaqueConfig::ksf_*` in that
    /// case. See the migration file for the "why per-envelope"
    /// rationale.
    pub ksf: Option<StoredKsf>,
}

/// Client-declared Argon2id parameters carried alongside an OPAQUE
/// envelope. All three move as an atomic set (populated together at
/// `register_finish`, nulled together at `clear_registration`).
#[derive(Debug, Clone, Copy)]
pub struct StoredKsf {
    /// Argon2id memory cost in KiB.
    pub memory_kib: u32,
    /// Argon2id iteration count.
    pub iterations: u32,
    /// Argon2id parallelism (lanes).
    pub parallelism: u32,
}

/// Secondary (outbound) port for OPAQUE envelope persistence.
///
/// Concrete impl lives in
/// [`crate::infrastructure::repositories::pg::opaque_pg_repository`].
#[cfg_attr(feature = "test_utils", mockall::automock)]
#[async_trait]
pub trait OpaqueRepositoryPort: Send + Sync + 'static {
    /// Write (or overwrite) the OPAQUE registration for `user_id`.
    ///
    /// Idempotent w.r.t. `opaque_registered_at`: the first-registration
    /// timestamp is preserved across re-registrations. Only the
    /// envelope + ciphersuite_version + KSF params rotate on password
    /// change.
    ///
    /// `ksf` carries the Argon2id parameters the CLIENT used at
    /// register time (declared in the register/finish request). Stored
    /// per-envelope so future changes to the server's
    /// `OpaqueConfig::ksf_*` do not invalidate this envelope — the
    /// lookup endpoint returns these values and the client uses them
    /// on the login handshake.
    ///
    /// Does NOT touch `opaque_migrated_at` — that's flipped by the
    /// login endpoint after the first successful OPAQUE handshake.
    async fn write_registration(
        &self,
        user_id: Uuid,
        envelope: &[u8],
        ciphersuite_version: i16,
        ksf: StoredKsf,
    ) -> Result<()>;

    /// Read the current envelope for `user_id`. Returns `None` when
    /// the user has no OPAQUE registration (Phase 0 default, or
    /// account was cleared by the admin reset flow).
    async fn read_registration(&self, user_id: Uuid) -> Result<Option<StoredEnvelope>>;

    /// Invalidate the OPAQUE registration for `user_id` and stamp the
    /// force-change-at-next-login flag in one transaction. Used by
    /// admin-side password reset (Phase 4+) — see the module-level
    /// note above for why the flag is co-located with the clear.
    ///
    /// Idempotent: clearing an already-empty registration is a no-op
    /// on the envelope columns but STILL sets the force-change flag
    /// (that's the point of the admin call).
    async fn clear_registration(&self, user_id: Uuid) -> Result<()>;

    /// Invalidate the OPAQUE envelope for `user_id` WITHOUT touching
    /// `force_password_change_at_next_login`. Used by the self-service
    /// `change_password` path: the user just proved and rotated their
    /// legacy password, so the OLD envelope (bound to the OLD
    /// passphrase) MUST go, but no forced-change prompt is needed on
    /// the next login (the user did just change it themselves).
    ///
    /// Distinct from [`clear_registration`], which co-flips
    /// `force_password_change` because that path represents an admin
    /// override — the user did NOT choose the new value, so they must
    /// pick their own on next login. Change-password is the inverse:
    /// user chose the value, no re-choice needed.
    ///
    /// Also used by `oxicloud-cli opaque reset --user X` for KSF
    /// rotation recovery — same "envelope stale, don't touch other
    /// state" semantics.
    ///
    /// Idempotent: nulling already-null columns is a no-op.
    async fn clear_envelope_only(&self, user_id: Uuid) -> Result<()>;

    /// Stamp `opaque_migrated_at` on `user_id` if it isn't set yet.
    /// Called by the login-KE3 handler after a successful OPAQUE
    /// handshake — the presence of this timestamp is the Phase 3+
    /// signal that legacy `POST /api/auth/login` should refuse for
    /// this user.
    ///
    /// Idempotent (COALESCE preserves the first-migration timestamp
    /// so a later login doesn't rewrite the operational signal).
    async fn mark_migrated(&self, user_id: Uuid) -> Result<()>;

    /// True iff `user_id` has completed at least one successful
    /// OPAQUE login (i.e. `opaque_migrated_at IS NOT NULL`). Read
    /// by the legacy login gate in Phase 4 to refuse password
    /// authentication for users who've already proven OPAQUE
    /// capability — the admin-reset path re-opens legacy by
    /// NULL-ing this column via [`clear_registration`], so the
    /// state is coherent without a separate carve-out.
    ///
    /// Returns `false` for missing users (anti-enum: the legacy
    /// gate must not distinguish "user gone" from "user not
    /// migrated" — the wrong-password branch already covered the
    /// user-lookup miss upstream).
    async fn is_migrated(&self, user_id: Uuid) -> Result<bool>;
}
