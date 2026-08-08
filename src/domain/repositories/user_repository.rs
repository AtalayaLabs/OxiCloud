use crate::common::errors::DomainError;
use crate::domain::entities::user::{User, UserRole};
use chrono::{DateTime, Utc};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum UserRepositoryError {
    #[error("User not found: {0}")]
    NotFound(String),

    #[error("User already exists: {0}")]
    AlreadyExists(String),

    #[error("Database error: {0}")]
    DatabaseError(String),

    #[error("Validation error: {0}")]
    ValidationError(String),

    #[error("Timeout error: {0}")]
    Timeout(String),

    #[error("Operation not allowed: {0}")]
    OperationNotAllowed(String),
}

pub type UserRepositoryResult<T> = Result<T, UserRepositoryError>;

/// Narrow projection for user-directory tables that do not need secrets,
/// profile pictures, or the cross-device UI-preferences document.
///
/// The full [`User`] row intentionally carries all of those fields for account
/// detail and the system address book.  Reusing it for the paginated admin
/// table made PostgreSQL detoast and transfer an avatar of up to 512 KiB per
/// row, only for the handler to serialize it back to the browser where the
/// table never reads it.  Keeping the projection explicit prevents a future
/// full-row field from silently returning to that hot path.
#[derive(Debug, Clone)]
pub struct UserListEntry {
    pub id: Uuid,
    pub username: Option<String>,
    pub email: String,
    pub role: UserRole,
    pub storage_quota_bytes: i64,
    pub storage_used_bytes: i64,
    pub last_login_at: Option<DateTime<Utc>>,
    pub active: bool,
    pub federation_kind: Option<String>,
    pub federation_issuer: Option<String>,
    pub is_external: bool,
    /// TRUE when `auth.users.password_hash IS NOT NULL` — user has a
    /// server-verifiable password on file (legacy or admin-set).
    /// Distinct from `opaque_registered` (which is the zero-knowledge
    /// envelope): a fully-migrated user carries BOTH — password for
    /// the fallback / operator flows, envelope for the actual login.
    /// A user with `has_password = false AND !opaque_registered AND
    /// federation_issuer IS NULL` is passwordless — the only path in is
    /// via magic-link (or, for externals, whatever grant they hold).
    pub has_password: bool,
    /// TRUE when `auth.users.opaque_envelope IS NOT NULL` — the user
    /// has completed OPAQUE registration (typically via the Phase 2
    /// silent-migration hook after a successful legacy login). Surfaced
    /// on the admin user table so operators can see rollout progress
    /// per-user. Admin-only exposure — see `AdminUserSummaryDto`.
    pub opaque_registered: bool,
    /// TRUE when `auth.users.opaque_migrated_at IS NOT NULL` — the
    /// user has completed at least one successful OPAQUE login. Distinct
    /// from `opaque_registered` because a user can have an envelope on
    /// file without having actually logged in via OPAQUE yet (e.g.
    /// admin cleared the envelope, silent-migration hasn't re-run).
    pub opaque_migrated: bool,
}

// Conversion from UserRepositoryError to DomainError
impl From<UserRepositoryError> for DomainError {
    fn from(err: UserRepositoryError) -> Self {
        match err {
            UserRepositoryError::NotFound(msg) => DomainError::not_found("User", msg),
            UserRepositoryError::AlreadyExists(msg) => DomainError::already_exists("User", msg),
            UserRepositoryError::DatabaseError(msg) => DomainError::internal_error("Database", msg),
            UserRepositoryError::ValidationError(msg) => DomainError::validation_error(msg),
            UserRepositoryError::Timeout(msg) => DomainError::timeout("Database", msg),
            UserRepositoryError::OperationNotAllowed(msg) => {
                DomainError::access_denied("User", msg)
            }
        }
    }
}

pub trait UserRepository: Send + Sync + 'static {
    /// Creates a new user
    async fn create_user(&self, user: User) -> UserRepositoryResult<User>;

    /// Gets a user by ID
    async fn get_user_by_id(&self, id: Uuid) -> UserRepositoryResult<User>;

    /// Batch-loads a set of users by id, preserving no particular order
    /// and silently skipping ids that don't match any row. Caller is
    /// responsible for de-duplicating the input vec. Returns an empty
    /// vec when given an empty input. Used by group-recipient expansion
    /// in `RecipientNotificationService` to avoid N+1 queries.
    async fn get_users_by_ids(&self, ids: Vec<Uuid>) -> UserRepositoryResult<Vec<User>>;

    /// Gets a user by username
    async fn get_user_by_username(&self, username: &str) -> UserRepositoryResult<User>;

    /// Gets a user by email
    async fn get_user_by_email(&self, email: &str) -> UserRepositoryResult<User>;

    /// Updates an existing user
    async fn update_user(&self, user: User) -> UserRepositoryResult<User>;

    /// Updates only a user's storage usage
    async fn update_storage_usage(
        &self,
        user_id: Uuid,
        usage_bytes: i64,
    ) -> UserRepositoryResult<()>;

    /// Updates the last login date
    async fn update_last_login(&self, user_id: Uuid) -> UserRepositoryResult<()>;

    /// Lists users with pagination.
    ///
    /// `include_external` controls whether external (grant-only) users
    /// appear in the result. Default callers should pass `false` so
    /// external users stay invisible to internal-user surfaces (system
    /// address book autocomplete, sharee search, etc.). Only the admin
    /// management UI should request `true`.
    async fn list_users(
        &self,
        limit: i64,
        offset: i64,
        include_external: bool,
    ) -> UserRepositoryResult<Vec<User>>;

    /// Lists the columns needed by compact user-management tables.  Unlike
    /// [`Self::list_users`], this never fetches password hashes, OIDC subjects,
    /// avatars, names, locale state, or UI preferences.
    async fn list_user_summaries(
        &self,
        limit: i64,
        offset: i64,
        include_external: bool,
    ) -> UserRepositoryResult<Vec<UserListEntry>>;

    /// Searches users by username or email (SQL ILIKE) with a limit.
    /// See [`list_users`] for the meaning of `include_external`.
    async fn search_users(
        &self,
        query: &str,
        limit: i64,
        include_external: bool,
    ) -> UserRepositoryResult<Vec<User>>;

    /// Activates or deactivates a user
    async fn set_user_active_status(&self, user_id: Uuid, active: bool)
    -> UserRepositoryResult<()>;

    /// Changes a user's password
    async fn change_password(&self, user_id: Uuid, password_hash: &str)
    -> UserRepositoryResult<()>;

    /// Changes a user's role
    async fn change_role(&self, user_id: Uuid, role: UserRole) -> UserRepositoryResult<()>;

    /// Lists users by role (admin or user)
    async fn list_users_by_role(&self, role: &str) -> UserRepositoryResult<Vec<User>>;

    /// Counts users with a given role via a scalar `COUNT(*)` — no row
    /// hydration (benches/ROUND29.md §G).
    async fn count_users_by_role(&self, role: &str) -> UserRepositoryResult<i64>;

    /// Deletes a user
    async fn delete_user(&self, user_id: Uuid) -> UserRepositoryResult<()>;

    /// Finds a user by federation (issuer, subject) pair.
    async fn get_user_by_federation_subject(
        &self,
        issuer: &str,
        subject: &str,
    ) -> UserRepositoryResult<User>;

    /// Updates a user's storage quota
    async fn update_storage_quota(
        &self,
        user_id: Uuid,
        quota_bytes: i64,
    ) -> UserRepositoryResult<()>;

    /// Counts the total number of users
    async fn count_users(&self) -> UserRepositoryResult<i64>;

    /// Gets aggregated storage statistics
    async fn get_storage_stats(&self) -> UserRepositoryResult<StorageStats>;
}

/// Aggregated storage statistics
#[derive(Debug, Clone)]
pub struct StorageStats {
    pub total_users: i64,
    pub active_users: i64,
    pub total_quota_bytes: i64,
    pub total_used_bytes: i64,
    pub users_over_80_percent: i64,
    pub users_over_quota: i64,
}
