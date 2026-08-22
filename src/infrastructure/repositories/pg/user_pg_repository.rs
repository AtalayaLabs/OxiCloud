use futures::future::BoxFuture;
use sqlx::{PgPool, Row};
use std::sync::Arc;
use uuid::Uuid;

use crate::application::ports::auth_ports::UserStoragePort;
use crate::common::errors::DomainError;
use crate::domain::entities::user::{User, UserFlags, UserRole};
use crate::domain::repositories::user_repository::{
    StorageStats, UserRepository, UserRepositoryError, UserRepositoryResult,
};
use crate::infrastructure::repositories::pg::transaction_utils::with_transaction;

// Implement From<sqlx::Error> for UserRepositoryError to allow automatic conversions
impl From<sqlx::Error> for UserRepositoryError {
    fn from(err: sqlx::Error) -> Self {
        UserPgRepository::map_sqlx_error(err)
    }
}

pub struct UserPgRepository {
    pool: Arc<PgPool>,
}

impl UserPgRepository {
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool }
    }

    /// Borrowed access to the connection pool. Exposed so callers can
    /// open transactions that span this repo and other repos / hooks
    /// (e.g. `AuthApplicationService::delete_user_admin` opening a tx
    /// that wraps the lifecycle dispatcher + the DELETE).
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    // Helper method to map SQL errors to domain errors
    pub fn map_sqlx_error(err: sqlx::Error) -> UserRepositoryError {
        match err {
            sqlx::Error::RowNotFound => UserRepositoryError::NotFound("User not found".to_string()),
            sqlx::Error::Database(db_err) => {
                if db_err.code().is_some_and(|code| code == "23505") {
                    // PostgreSQL uniqueness violation code
                    UserRepositoryError::AlreadyExists("User or email already exists".to_string())
                } else {
                    UserRepositoryError::DatabaseError(format!("Database error: {}", db_err))
                }
            }
            _ => UserRepositoryError::DatabaseError(format!("Database error: {}", err)),
        }
    }

    /// Fetch only the authorization-relevant flags of a user. Not part of
    /// the `UserRepository` trait — called directly from
    /// `AuthApplicationService::get_user_flags`.
    ///
    /// Deliberately selects three tiny columns instead of the full row:
    /// the full-row SELECT includes `image` (a data URI of up to 512 KiB),
    /// which per-request middleware guards were paying on every WebDAV /
    /// CalDAV / CardDAV request just to read `is_external` or `role`.
    pub async fn get_user_flags(&self, id: Uuid) -> UserRepositoryResult<UserFlags> {
        let row = sqlx::query(
            r#"
            SELECT role::text as role_text,
                   is_external,
                   active,
                   force_password_change_at_next_login
              FROM auth.users
             WHERE id = $1
            "#,
        )
        .bind(id)
        .fetch_one(&*self.pool)
        .await
        .map_err(Self::map_sqlx_error)?;

        let role_str: Option<String> = row.try_get("role_text").unwrap_or(None);
        let role = match role_str.as_deref() {
            Some("admin") => UserRole::Admin,
            _ => UserRole::User,
        };

        Ok(UserFlags {
            role,
            is_external: row.get("is_external"),
            active: row.get("active"),
            force_password_change: row.get("force_password_change_at_next_login"),
        })
    }

    /// Fetch only `(storage_used_bytes, storage_quota_bytes)`. Not part of
    /// the `UserRepository` trait — called from `StorageUsageService`.
    ///
    /// Same rationale as [`Self::get_user_flags`]: the full-row SELECT drags
    /// `image` (a data URI of up to 512 KiB), `password_hash`,
    /// `ui_preferences`, … across the wire, and the quota path runs on every
    /// folder PROPFIND and every upload quota check just to read two i64s.
    /// Measured in `benches/QUOTA-PATH.md`.
    pub async fn get_storage_usage(&self, id: Uuid) -> UserRepositoryResult<(i64, i64)> {
        let row = sqlx::query(
            r#"
            SELECT storage_used_bytes, storage_quota_bytes
            FROM auth.users
            WHERE id = $1
            "#,
        )
        .bind(id)
        .fetch_one(&*self.pool)
        .await
        .map_err(Self::map_sqlx_error)?;

        Ok((
            row.get("storage_used_bytes"),
            row.get("storage_quota_bytes"),
        ))
    }

    /// Read `force_password_change_at_next_login`. Written TRUE by the
    /// admin password-reset flow (via `OpaquePgRepository::clear_registration`,
    /// which sets it alongside the envelope invalidation in one UPDATE)
    /// and by admin-side `set_user_password`. Cleared on a successful
    /// user-initiated `change_password`.
    ///
    /// Reads via a single-column SELECT to avoid dragging the full row
    /// (with its up-to-512 KiB `image`) on every login-response mint.
    /// Returns `false` for missing users so the login path — which has
    /// already resolved the user by id — treats a lost race the same
    /// as "flag not set" rather than surfacing a 5xx.
    pub async fn is_force_password_change(&self, id: Uuid) -> UserRepositoryResult<bool> {
        let row: Option<(bool,)> = sqlx::query_as(
            r#"
            SELECT force_password_change_at_next_login
              FROM auth.users
             WHERE id = $1
            "#,
        )
        .bind(id)
        .fetch_optional(&*self.pool)
        .await
        .map_err(Self::map_sqlx_error)?;
        Ok(row.map(|(v,)| v).unwrap_or(false))
    }

    /// Clear `force_password_change_at_next_login`. Called by the
    /// change-password flow on success so a legitimate self-service
    /// password rotation lifts the admin-set "temporary" marker in
    /// one round-trip.
    ///
    /// Deliberately does NOT gate on the current value — flipping FALSE
    /// to FALSE is a no-op at the row level. That keeps the caller from
    /// needing a read-modify-write.
    pub async fn clear_force_password_change(&self, id: Uuid) -> UserRepositoryResult<()> {
        sqlx::query(
            r#"
            UPDATE auth.users
               SET force_password_change_at_next_login = FALSE
             WHERE id = $1
            "#,
        )
        .bind(id)
        .execute(&*self.pool)
        .await
        .map_err(Self::map_sqlx_error)?;
        Ok(())
    }

    /// Set `force_password_change_at_next_login = TRUE`. Used by
    /// admin-initiated password reset when the OPAQUE substrate is NOT
    /// wired. When it IS wired, callers should prefer
    /// `OpaquePgRepository::clear_registration` which does the same
    /// flag flip AND invalidates the OPAQUE envelope in one UPDATE
    /// (see the port doc on `clear_registration` for the atomicity
    /// contract). This method exists so OPAQUE-off deployments still
    /// get the "admin's temp password prompts change on next login"
    /// behaviour without having to depend on the OPAQUE code path.
    pub async fn set_force_password_change(&self, id: Uuid) -> UserRepositoryResult<()> {
        sqlx::query(
            r#"
            UPDATE auth.users
               SET force_password_change_at_next_login = TRUE
             WHERE id = $1
            "#,
        )
        .bind(id)
        .execute(&*self.pool)
        .await
        .map_err(Self::map_sqlx_error)?;
        Ok(())
    }

    /// Updates a user's profile image (URL or data URI). Not part of the
    /// `UserRepository` trait — called directly from `AuthApplicationService`.
    pub async fn update_image(
        &self,
        user_id: Uuid,
        image: Option<String>,
    ) -> UserRepositoryResult<()> {
        sqlx::query(
            r#"
            UPDATE auth.users
            SET image = $2, updated_at = NOW()
            WHERE id = $1
            "#,
        )
        .bind(user_id)
        .bind(&image)
        .execute(&*self.pool)
        .await
        .map_err(Self::map_sqlx_error)?;
        Ok(())
    }

    /// Shallow-merge a partial UI-preferences patch into
    /// `ui_preferences`. The Postgres `||` operator merges top-level
    /// keys — `{"a":1,"b":2} || {"b":3,"c":4}` → `{"a":1,"b":3,"c":4}`,
    /// which is exactly the semantic PATCH callers want: a partial
    /// write only touches the keys it mentions, so a preference set on
    /// one device isn't wiped by a partial write from another.
    ///
    /// `jsonb_strip_nulls` removes any key whose incoming value is
    /// null, giving callers a documented delete-a-key path (`PATCH
    /// {"foo": null}` clears `foo`). Nested nulls inside a value
    /// object survive — we only strip at the top level via the merge
    /// result.
    ///
    /// Not part of the `UserRepository` trait — called directly from
    /// `AuthApplicationService::update_profile`. Bumps `updated_at`
    /// so the standard "when did this row change" audits stay useful.
    ///
    /// The CHECK constraints
    /// (`users_ui_preferences_is_object` + `_size_cap`) enforce shape
    /// and cap at the schema layer; a violating patch surfaces as an
    /// sqlx error and returns to the handler as 400.
    pub async fn update_ui_preferences(
        &self,
        user_id: Uuid,
        patch: &serde_json::Value,
    ) -> UserRepositoryResult<()> {
        sqlx::query(
            r#"
            UPDATE auth.users
            SET ui_preferences = jsonb_strip_nulls(ui_preferences || $2::jsonb),
                updated_at = NOW()
            WHERE id = $1
            "#,
        )
        .bind(user_id)
        .bind(patch)
        .execute(&*self.pool)
        .await
        .map_err(Self::map_sqlx_error)?;
        Ok(())
    }
}

impl UserRepository for UserPgRepository {
    /// Creates a new user using a transaction
    async fn create_user(&self, user: User) -> UserRepositoryResult<User> {
        // Create a copy of the user for the closure
        let user_clone = user.clone();

        with_transaction(&self.pool, "create_user", |tx| {
            // We need to move the closure into a BoxFuture to return inside
            // the with_transaction call
            Box::pin(async move {
                // Use getters to extract the values
                // Convert user.role() to string to pass it as plain text
                let role_str = user_clone.role().to_string();

                // Modify the SQL to do an explicit cast to the auth.userrole type
                // `image` is included here (was missing pre-fix); without
                // it a JIT-provisioned OIDC user landed in the row with
                // a NULL profile picture even when the IdP's `picture`
                // claim was non-empty. `update_user` already wrote the
                // column so existing-user re-logins worked, but the
                // first-time INSERT silently dropped it — surfaced by
                // tests/oidc/oidc.hurl Step 6 asserting on `$.image`.
                let _result = sqlx::query(
                    r#"
                        INSERT INTO auth.users (
                            id, username, email, password_hash, role,
                            storage_quota_bytes, storage_used_bytes,
                            created_at, updated_at, last_login_at, active,
                            federation_kind, federation_issuer, federation_subject,
                            image, is_external,
                            given_name, family_name, email_verified_at,
                            preferred_locale, notify_on_share, ui_preferences
                        ) VALUES (
                            $1, $2, $3, $4, $5::auth.userrole, $6, $7, $8, $9, $10, $11,
                            $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22
                        )
                        RETURNING *
                        "#,
                )
                .bind(user_clone.id())
                .bind(user_clone.username())
                .bind(user_clone.email())
                .bind(user_clone.password_hash())
                .bind(&role_str) // Convert to string but with explicit cast in SQL
                .bind(user_clone.storage_quota_bytes())
                .bind(user_clone.storage_used_bytes())
                .bind(user_clone.created_at())
                .bind(user_clone.updated_at())
                .bind(user_clone.last_login_at())
                .bind(user_clone.is_active())
                .bind(user_clone.federation_kind().map(|k| k.as_str()))
                .bind(user_clone.federation_issuer())
                .bind(user_clone.federation_subject())
                .bind(user_clone.image())
                .bind(user_clone.is_external())
                .bind(user_clone.given_name())
                .bind(user_clone.family_name())
                .bind(user_clone.email_verified_at())
                .bind(user_clone.preferred_locale())
                .bind(user_clone.notify_on_share())
                // ui_preferences bind: always a JSON object. `User::new`
                // initialises the bag to `{}`; ownership stays with the
                // repo for shallow-merge writes via `update_ui_preferences`.
                .bind(user_clone.ui_preferences())
                .execute(&mut **tx)
                .await
                .map_err(Self::map_sqlx_error)?;

                // We could perform additional operations here,
                // such as configuring permissions, roles, etc.

                Ok(user_clone)
            }) as BoxFuture<'_, UserRepositoryResult<User>>
        })
        .await?;

        Ok(user) // Return the original user for simplicity
    }

    /// Gets a user by ID
    async fn get_user_by_id(&self, id: Uuid) -> UserRepositoryResult<User> {
        let row = sqlx::query(
            r#"
            SELECT
                id, username, email, password_hash, role::text as role_text,
                storage_quota_bytes, storage_used_bytes,
                created_at, updated_at, last_login_at, active,
                federation_kind, federation_issuer, federation_subject, image, is_external,
                given_name, family_name, email_verified_at, preferred_locale, notify_on_share,
                ui_preferences
            FROM auth.users
            WHERE id = $1
            "#,
        )
        .bind(id)
        .fetch_one(&*self.pool)
        .await
        .map_err(Self::map_sqlx_error)?;

        // Convert role string to UserRole enum
        let role_str: Option<String> = row.try_get("role_text").unwrap_or(None);
        let role = match role_str.as_deref() {
            Some("admin") => UserRole::Admin,
            _ => UserRole::User,
        };

        Ok(User::from_data_full(
            row.get("id"),
            row.get("username"),
            row.get("email"),
            row.get("password_hash"),
            role,
            row.get("storage_quota_bytes"),
            row.get("storage_used_bytes"),
            row.get("created_at"),
            row.get("updated_at"),
            row.get("last_login_at"),
            row.get("active"),
            row.get::<Option<String>, _>("federation_kind")
                .as_deref()
                .and_then(crate::domain::entities::user::FederationKind::parse),
            row.get("federation_issuer"),
            row.get("federation_subject"),
            row.get("image"),
            row.get("is_external"),
            row.get("given_name"),
            row.get("family_name"),
            row.get("email_verified_at"),
            row.get("preferred_locale"),
            row.get("notify_on_share"),
            row.get::<serde_json::Value, _>("ui_preferences"),
        ))
    }

    async fn get_user_with_derived_flags(
        &self,
        id: Uuid,
    ) -> UserRepositoryResult<(
        User,
        crate::domain::repositories::user_repository::UserDerivedFlags,
    )> {
        // Same column set as `get_user_by_id` plus the three IS-NOT-NULL
        // derivations for auth-capability flags AND the EXISTS scalar
        // for `is_online`. The `interval` argument is bound as `$2`
        // (seconds, `ONLINE_WINDOW.as_secs_f64()`) via
        // `make_interval(secs => $2)` — same pattern as
        // `session_liveness_gauges.rs`. Partial index
        // `idx_sessions_last_seen_at WHERE revoked = FALSE` covers the
        // EXISTS scan, so per-row cost is ~μs.
        let row = sqlx::query(
            r#"
            SELECT
                id, username, email, password_hash, role::text as role_text,
                storage_quota_bytes, storage_used_bytes,
                created_at, updated_at, last_login_at, active,
                federation_kind, federation_issuer, federation_subject, image, is_external,
                given_name, family_name, email_verified_at, preferred_locale, notify_on_share,
                ui_preferences,
                (password_hash IS NOT NULL)       AS has_password,
                (opaque_envelope IS NOT NULL)     AS opaque_registered,
                (opaque_migrated_at IS NOT NULL)  AS opaque_migrated,
                EXISTS (
                    SELECT 1 FROM auth.sessions s
                     WHERE s.user_id = auth.users.id
                       AND s.revoked = FALSE
                       AND s.last_seen_at > NOW() - make_interval(secs => $2)
                )                                 AS is_online
            FROM auth.users
            WHERE id = $1
            "#,
        )
        .bind(id)
        .bind(crate::application::dtos::session_dto::ONLINE_WINDOW.as_secs_f64())
        .fetch_one(&*self.pool)
        .await
        .map_err(Self::map_sqlx_error)?;

        let role_str: Option<String> = row.try_get("role_text").unwrap_or(None);
        let role = match role_str.as_deref() {
            Some("admin") => UserRole::Admin,
            _ => UserRole::User,
        };

        let user = User::from_data_full(
            row.get("id"),
            row.get("username"),
            row.get("email"),
            row.get("password_hash"),
            role,
            row.get("storage_quota_bytes"),
            row.get("storage_used_bytes"),
            row.get("created_at"),
            row.get("updated_at"),
            row.get("last_login_at"),
            row.get("active"),
            row.get::<Option<String>, _>("federation_kind")
                .as_deref()
                .and_then(crate::domain::entities::user::FederationKind::parse),
            row.get("federation_issuer"),
            row.get("federation_subject"),
            row.get("image"),
            row.get("is_external"),
            row.get("given_name"),
            row.get("family_name"),
            row.get("email_verified_at"),
            row.get("preferred_locale"),
            row.get("notify_on_share"),
            row.get::<serde_json::Value, _>("ui_preferences"),
        );
        let flags = crate::domain::repositories::user_repository::UserDerivedFlags {
            has_password: row.get("has_password"),
            opaque_registered: row.get("opaque_registered"),
            opaque_migrated: row.get("opaque_migrated"),
            is_online: row.get("is_online"),
        };
        Ok((user, flags))
    }

    /// Gets a user by username
    async fn get_user_by_username(&self, username: &str) -> UserRepositoryResult<User> {
        let row = sqlx::query(
            r#"
            SELECT
                id, username, email, password_hash, role::text as role_text,
                storage_quota_bytes, storage_used_bytes,
                created_at, updated_at, last_login_at, active,
                federation_kind, federation_issuer, federation_subject, image, is_external,
                given_name, family_name, email_verified_at, preferred_locale, notify_on_share,
                ui_preferences
            FROM auth.users
            WHERE username = $1
            "#,
        )
        .bind(username)
        .fetch_one(&*self.pool)
        .await
        .map_err(Self::map_sqlx_error)?;

        // Convert role string to UserRole enum
        let role_str: Option<String> = row.try_get("role_text").unwrap_or(None);
        let role = match role_str.as_deref() {
            Some("admin") => UserRole::Admin,
            _ => UserRole::User,
        };

        Ok(User::from_data_full(
            row.get("id"),
            row.get("username"),
            row.get("email"),
            row.get("password_hash"),
            role,
            row.get("storage_quota_bytes"),
            row.get("storage_used_bytes"),
            row.get("created_at"),
            row.get("updated_at"),
            row.get("last_login_at"),
            row.get("active"),
            row.get::<Option<String>, _>("federation_kind")
                .as_deref()
                .and_then(crate::domain::entities::user::FederationKind::parse),
            row.get("federation_issuer"),
            row.get("federation_subject"),
            row.get("image"),
            row.get("is_external"),
            row.get("given_name"),
            row.get("family_name"),
            row.get("email_verified_at"),
            row.get("preferred_locale"),
            row.get("notify_on_share"),
            row.get::<serde_json::Value, _>("ui_preferences"),
        ))
    }

    /// Gets a user by email
    async fn get_user_by_email(&self, email: &str) -> UserRepositoryResult<User> {
        let row = sqlx::query(
            r#"
            SELECT
                id, username, email, password_hash, role::text as role_text,
                storage_quota_bytes, storage_used_bytes,
                created_at, updated_at, last_login_at, active,
                federation_kind, federation_issuer, federation_subject, image, is_external,
                given_name, family_name, email_verified_at, preferred_locale, notify_on_share,
                ui_preferences
            FROM auth.users
            WHERE email = $1
            "#,
        )
        .bind(email)
        .fetch_one(&*self.pool)
        .await
        .map_err(Self::map_sqlx_error)?;

        // Convert role string to UserRole enum
        let role_str: Option<String> = row.try_get("role_text").unwrap_or(None);
        let role = match role_str.as_deref() {
            Some("admin") => UserRole::Admin,
            _ => UserRole::User,
        };

        Ok(User::from_data_full(
            row.get("id"),
            row.get("username"),
            row.get("email"),
            row.get("password_hash"),
            role,
            row.get("storage_quota_bytes"),
            row.get("storage_used_bytes"),
            row.get("created_at"),
            row.get("updated_at"),
            row.get("last_login_at"),
            row.get("active"),
            row.get::<Option<String>, _>("federation_kind")
                .as_deref()
                .and_then(crate::domain::entities::user::FederationKind::parse),
            row.get("federation_issuer"),
            row.get("federation_subject"),
            row.get("image"),
            row.get("is_external"),
            row.get("given_name"),
            row.get("family_name"),
            row.get("email_verified_at"),
            row.get("preferred_locale"),
            row.get("notify_on_share"),
            row.get::<serde_json::Value, _>("ui_preferences"),
        ))
    }

    /// Returns every user whose email normalizes to `normalized_email`.
    ///
    /// Looks up against `auth.users.identity_lookup_email`, a stored
    /// GENERATED column populated by PostgreSQL from the same
    /// normalization `common::text::normalize_email_for_link` applies
    /// on the caller side. See migration
    /// 20261011000000_users_normalized_email_index.sql — the b-tree
    /// index on that column makes this an O(log n) probe.
    async fn list_users_by_normalized_email(
        &self,
        normalized_email: &str,
    ) -> UserRepositoryResult<Vec<User>> {
        let rows = sqlx::query(
            r#"
            SELECT
                id, username, email, password_hash, role::text as role_text,
                storage_quota_bytes, storage_used_bytes,
                created_at, updated_at, last_login_at, active,
                federation_kind, federation_issuer, federation_subject, image, is_external,
                given_name, family_name, email_verified_at, preferred_locale, notify_on_share,
                ui_preferences
            FROM auth.users
            WHERE identity_lookup_email = $1
            "#,
        )
        .bind(normalized_email)
        .fetch_all(&*self.pool)
        .await
        .map_err(Self::map_sqlx_error)?;

        let users = rows
            .into_iter()
            .map(|row| {
                let role_str: Option<String> = row.try_get("role_text").unwrap_or(None);
                let role = match role_str.as_deref() {
                    Some("admin") => UserRole::Admin,
                    _ => UserRole::User,
                };
                User::from_data_full(
                    row.get("id"),
                    row.get("username"),
                    row.get("email"),
                    row.get("password_hash"),
                    role,
                    row.get("storage_quota_bytes"),
                    row.get("storage_used_bytes"),
                    row.get("created_at"),
                    row.get("updated_at"),
                    row.get("last_login_at"),
                    row.get("active"),
                    row.get::<Option<String>, _>("federation_kind")
                        .as_deref()
                        .and_then(crate::domain::entities::user::FederationKind::parse),
                    row.get("federation_issuer"),
                    row.get("federation_subject"),
                    row.get("image"),
                    row.get("is_external"),
                    row.get("given_name"),
                    row.get("family_name"),
                    row.get("email_verified_at"),
                    row.get("preferred_locale"),
                    row.get("notify_on_share"),
                    row.get::<serde_json::Value, _>("ui_preferences"),
                )
            })
            .collect();

        Ok(users)
    }

    /// Batch loads users by id in one query (avoids N+1 for group-
    /// recipient expansion). Missing ids are silently skipped — the
    /// caller treats absent rows as "no such recipient", same as
    /// `get_user_by_id` returning `NotFound` for a single lookup.
    ///
    /// Notification-recipient projection: the up-to-512 KiB avatar `image`
    /// and the `ui_preferences` JSONB are NOT hydrated (both come back as
    /// `None`/`Null`) — the sole caller
    /// (`RecipientNotificationService`) reads only the email/eligibility
    /// fields, and a group fan-out of M members otherwise detoasted +
    /// shipped + parsed M avatars purely to discard them (the ROUND12 §Q1
    /// avatar-narrowing pattern; benches/ROUND13.md §Q1). If a future
    /// caller needs the avatar, add a wide sibling rather than widening
    /// this one back.
    async fn get_users_by_ids(&self, ids: Vec<Uuid>) -> UserRepositoryResult<Vec<User>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        let rows = sqlx::query(
            r#"
            SELECT
                id, username, email, password_hash, role::text as role_text,
                storage_quota_bytes, storage_used_bytes,
                created_at, updated_at, last_login_at, active,
                federation_kind, federation_issuer, federation_subject, is_external,
                given_name, family_name, email_verified_at, preferred_locale, notify_on_share
            FROM auth.users
            WHERE id = ANY($1)
            "#,
        )
        .bind(&ids)
        .fetch_all(&*self.pool)
        .await
        .map_err(Self::map_sqlx_error)?;

        Ok(rows
            .into_iter()
            .map(|row| {
                let role_str: Option<String> = row.try_get("role_text").unwrap_or(None);
                let role = match role_str.as_deref() {
                    Some("admin") => UserRole::Admin,
                    _ => UserRole::User,
                };

                User::from_data_full(
                    row.get("id"),
                    row.get("username"),
                    row.get("email"),
                    row.get("password_hash"),
                    role,
                    row.get("storage_quota_bytes"),
                    row.get("storage_used_bytes"),
                    row.get("created_at"),
                    row.get("updated_at"),
                    row.get("last_login_at"),
                    row.get("active"),
                    row.get::<Option<String>, _>("federation_kind")
                        .as_deref()
                        .and_then(crate::domain::entities::user::FederationKind::parse),
                    row.get("federation_issuer"),
                    row.get("federation_subject"),
                    None, // image — not projected (notification-recipient path)
                    row.get("is_external"),
                    row.get("given_name"),
                    row.get("family_name"),
                    row.get("email_verified_at"),
                    row.get("preferred_locale"),
                    row.get("notify_on_share"),
                    serde_json::Value::Null, // ui_preferences — not projected
                )
            })
            .collect())
    }

    /// Updates an existing user using a transaction
    async fn update_user(&self, user: User) -> UserRepositoryResult<User> {
        // Create a copy of the user for the closure
        let user_clone = user.clone();

        with_transaction(&self.pool, "update_user", |tx| {
            Box::pin(async move {
                // Update the user
                sqlx::query(
                    r#"
                        UPDATE auth.users
                        SET
                            username = $2,
                            email = $3,
                            password_hash = $4,
                            role = $5::auth.userrole,
                            storage_quota_bytes = $6,
                            storage_used_bytes = $7,
                            updated_at = $8,
                            last_login_at = $9,
                            active = $10,
                            image = $11,
                            given_name = $12,
                            family_name = $13,
                            email_verified_at = $14,
                            preferred_locale = $15,
                            notify_on_share = $16,
                            -- Include `is_external` so the external →
                            -- internal upgrade path
                            -- (`AuthApplicationService::upgrade_to_internal`)
                            -- can flip this flag. Previously omitted
                            -- because no code path mutated it after
                            -- creation. The DB CHECK
                            -- `users_external_no_storage`
                            -- (`is_external=false OR quota=0`) is
                            -- satisfied by the upgrade because it
                            -- writes both fields in the same UPDATE:
                            -- `is_external=false, quota>0`.
                            is_external = $17
                        WHERE id = $1
                        "#,
                )
                .bind(user_clone.id())
                .bind(user_clone.username())
                .bind(user_clone.email())
                .bind(user_clone.password_hash())
                .bind(user_clone.role().to_string())
                .bind(user_clone.storage_quota_bytes())
                .bind(user_clone.storage_used_bytes())
                .bind(user_clone.updated_at())
                .bind(user_clone.last_login_at())
                .bind(user_clone.is_active())
                .bind(user_clone.image())
                .bind(user_clone.given_name())
                .bind(user_clone.family_name())
                .bind(user_clone.email_verified_at())
                .bind(user_clone.preferred_locale())
                .bind(user_clone.notify_on_share())
                .bind(user_clone.is_external())
                .execute(&mut **tx)
                .await
                .map_err(Self::map_sqlx_error)?;

                // We could perform additional operations here inside
                // the same transaction, such as updating permissions, etc.

                Ok(user_clone)
            }) as BoxFuture<'_, UserRepositoryResult<User>>
        })
        .await?;

        Ok(user)
    }

    /// Updates only the storage usage of a user.
    ///
    /// The `IS DISTINCT FROM` guard makes this a no-op when the value is
    /// unchanged — which is the common case for the periodic reconciliation
    /// sweep — so it produces no dead tuple and no WAL when nothing changed.
    async fn update_storage_usage(
        &self,
        user_id: Uuid,
        usage_bytes: i64,
    ) -> UserRepositoryResult<()> {
        sqlx::query(
            r#"
            UPDATE auth.users
            SET
                storage_used_bytes = $2,
                updated_at = NOW()
            WHERE id = $1 AND storage_used_bytes IS DISTINCT FROM $2
            "#,
        )
        .bind(user_id)
        .bind(usage_bytes)
        .execute(&*self.pool)
        .await
        .map_err(Self::map_sqlx_error)?;

        Ok(())
    }

    /// Updates the last login date
    async fn update_last_login(&self, user_id: Uuid) -> UserRepositoryResult<()> {
        sqlx::query(
            r#"
            UPDATE auth.users
            SET 
                last_login_at = NOW(),
                updated_at = NOW()
            WHERE id = $1
            "#,
        )
        .bind(user_id)
        .execute(&*self.pool)
        .await
        .map_err(Self::map_sqlx_error)?;

        Ok(())
    }

    /// Lists users with pagination
    async fn list_users(
        &self,
        limit: i64,
        offset: i64,
        include_external: bool,
    ) -> UserRepositoryResult<Vec<User>> {
        let rows = sqlx::query(
            r#"
            SELECT
                id, username, email, password_hash, role::text as role_text,
                storage_quota_bytes, storage_used_bytes,
                created_at, updated_at, last_login_at, active,
                federation_kind, federation_issuer, federation_subject, image, is_external,
                given_name, family_name, email_verified_at, preferred_locale, notify_on_share,
                ui_preferences
            FROM auth.users
            WHERE ($3 OR is_external = FALSE)
            ORDER BY created_at DESC, id DESC
            LIMIT $1 OFFSET $2
            "#,
        )
        .bind(limit)
        .bind(offset)
        .bind(include_external)
        .fetch_all(&*self.pool)
        .await
        .map_err(Self::map_sqlx_error)?;

        let users = rows
            .into_iter()
            .map(|row| {
                // Convert role string to UserRole enum for each row
                let role_str: Option<String> = row.try_get("role_text").unwrap_or(None);
                let role = match role_str.as_deref() {
                    Some("admin") => UserRole::Admin,
                    _ => UserRole::User,
                };

                User::from_data_full(
                    row.get("id"),
                    row.get("username"),
                    row.get("email"),
                    row.get("password_hash"),
                    role,
                    row.get("storage_quota_bytes"),
                    row.get("storage_used_bytes"),
                    row.get("created_at"),
                    row.get("updated_at"),
                    row.get("last_login_at"),
                    row.get("active"),
                    row.get::<Option<String>, _>("federation_kind")
                        .as_deref()
                        .and_then(crate::domain::entities::user::FederationKind::parse),
                    row.get("federation_issuer"),
                    row.get("federation_subject"),
                    row.get("image"),
                    row.get("is_external"),
                    row.get("given_name"),
                    row.get("family_name"),
                    row.get("email_verified_at"),
                    row.get("preferred_locale"),
                    row.get("notify_on_share"),
                    row.get::<serde_json::Value, _>("ui_preferences"),
                )
            })
            .collect();

        Ok(users)
    }

    async fn list_users_with_derived_flags(
        &self,
        limit: i64,
        offset: i64,
        include_external: bool,
    ) -> UserRepositoryResult<
        Vec<(
            User,
            crate::domain::repositories::user_repository::UserDerivedFlags,
        )>,
    > {
        // Full `User` column set (matches `get_user_by_id`) + the four
        // derived booleans (IS-NOT-NULL for auth capability, EXISTS for
        // `is_online`) in one SELECT. Same rationale as the single-user
        // `get_user_with_derived_flags` variant. Widened over the older
        // `list_user_summaries` projection because the FE now consumes
        // the full user profile from these rows (killing the per-row
        // `/api/users/{id}` fetch the admin table used to fire for
        // avatars — see docs/plan/userdto-refactor.md § N+1).
        //
        // `interval` bound as `$4` seconds
        // (`ONLINE_WINDOW.as_secs_f64()`), same pattern as
        // `session_liveness_gauges.rs` and `get_user_with_derived_flags`.
        let rows = sqlx::query(
            r#"
            SELECT
                id, username, email, password_hash, role::text as role_text,
                storage_quota_bytes, storage_used_bytes,
                created_at, updated_at, last_login_at, active,
                federation_kind, federation_issuer, federation_subject, image, is_external,
                given_name, family_name, email_verified_at, preferred_locale, notify_on_share,
                ui_preferences,
                (password_hash IS NOT NULL)       AS has_password_flag,
                (opaque_envelope IS NOT NULL)     AS opaque_registered,
                (opaque_migrated_at IS NOT NULL)  AS opaque_migrated,
                EXISTS (
                    SELECT 1 FROM auth.sessions s
                     WHERE s.user_id = auth.users.id
                       AND s.revoked = FALSE
                       AND s.last_seen_at > NOW() - make_interval(secs => $4)
                )                                 AS is_online
            FROM auth.users
            WHERE ($3 OR is_external = FALSE)
            ORDER BY created_at DESC, id DESC
            LIMIT $1 OFFSET $2
            "#,
        )
        .bind(limit)
        .bind(offset)
        .bind(include_external)
        .bind(crate::application::dtos::session_dto::ONLINE_WINDOW.as_secs_f64())
        .fetch_all(&*self.pool)
        .await
        .map_err(Self::map_sqlx_error)?;

        // Note: the `has_password_flag` alias avoids colliding with the
        // `password_hash` column selected above (the tuple destructure
        // in `list_user_summaries` uses a shorter projection so it
        // could reuse the raw `has_password` alias; here we keep both).
        Ok(rows
            .into_iter()
            .map(|row| {
                let role_str: Option<String> = row.try_get("role_text").unwrap_or(None);
                let role = match role_str.as_deref() {
                    Some("admin") => UserRole::Admin,
                    _ => UserRole::User,
                };
                let user = User::from_data_full(
                    row.get("id"),
                    row.get("username"),
                    row.get("email"),
                    row.get("password_hash"),
                    role,
                    row.get("storage_quota_bytes"),
                    row.get("storage_used_bytes"),
                    row.get("created_at"),
                    row.get("updated_at"),
                    row.get("last_login_at"),
                    row.get("active"),
                    row.get::<Option<String>, _>("federation_kind")
                        .as_deref()
                        .and_then(crate::domain::entities::user::FederationKind::parse),
                    row.get("federation_issuer"),
                    row.get("federation_subject"),
                    row.get("image"),
                    row.get("is_external"),
                    row.get("given_name"),
                    row.get("family_name"),
                    row.get("email_verified_at"),
                    row.get("preferred_locale"),
                    row.get("notify_on_share"),
                    row.get::<serde_json::Value, _>("ui_preferences"),
                );
                let flags = crate::domain::repositories::user_repository::UserDerivedFlags {
                    has_password: row.get("has_password_flag"),
                    opaque_registered: row.get("opaque_registered"),
                    opaque_migrated: row.get("opaque_migrated"),
                    is_online: row.get("is_online"),
                };
                (user, flags)
            })
            .collect())
    }

    async fn search_users(
        &self,
        query: &str,
        limit: i64,
        include_external: bool,
    ) -> UserRepositoryResult<Vec<User>> {
        let pattern = format!("%{}%", query);
        let rows = sqlx::query(
            r#"
            SELECT
                id, username, email, password_hash, role::text as role_text,
                storage_quota_bytes, storage_used_bytes,
                created_at, updated_at, last_login_at, active,
                federation_kind, federation_issuer, federation_subject, image, is_external,
                given_name, family_name, email_verified_at, preferred_locale, notify_on_share,
                ui_preferences
            FROM auth.users
            WHERE (username ILIKE $1 OR email ILIKE $1)
              AND ($3 OR is_external = FALSE)
            ORDER BY username
            LIMIT $2
            "#,
        )
        .bind(&pattern)
        .bind(limit)
        .bind(include_external)
        .fetch_all(&*self.pool)
        .await
        .map_err(Self::map_sqlx_error)?;

        let users = rows
            .into_iter()
            .map(|row| {
                let role_str: Option<String> = row.try_get("role_text").unwrap_or(None);
                let role = match role_str.as_deref() {
                    Some("admin") => UserRole::Admin,
                    _ => UserRole::User,
                };

                User::from_data_full(
                    row.get("id"),
                    row.get("username"),
                    row.get("email"),
                    row.get("password_hash"),
                    role,
                    row.get("storage_quota_bytes"),
                    row.get("storage_used_bytes"),
                    row.get("created_at"),
                    row.get("updated_at"),
                    row.get("last_login_at"),
                    row.get("active"),
                    row.get::<Option<String>, _>("federation_kind")
                        .as_deref()
                        .and_then(crate::domain::entities::user::FederationKind::parse),
                    row.get("federation_issuer"),
                    row.get("federation_subject"),
                    row.get("image"),
                    row.get("is_external"),
                    row.get("given_name"),
                    row.get("family_name"),
                    row.get("email_verified_at"),
                    row.get("preferred_locale"),
                    row.get("notify_on_share"),
                    row.get::<serde_json::Value, _>("ui_preferences"),
                )
            })
            .collect();

        Ok(users)
    }

    /// Activates or deactivates a user
    async fn set_user_active_status(
        &self,
        user_id: Uuid,
        active: bool,
    ) -> UserRepositoryResult<()> {
        sqlx::query(
            r#"
            UPDATE auth.users
            SET 
                active = $2,
                updated_at = NOW()
            WHERE id = $1
            "#,
        )
        .bind(user_id)
        .bind(active)
        .execute(&*self.pool)
        .await
        .map_err(Self::map_sqlx_error)?;

        Ok(())
    }

    /// Changes a user's password
    async fn change_password(
        &self,
        user_id: Uuid,
        password_hash: &str,
    ) -> UserRepositoryResult<()> {
        sqlx::query(
            r#"
            UPDATE auth.users
            SET 
                password_hash = $2,
                updated_at = NOW()
            WHERE id = $1
            "#,
        )
        .bind(user_id)
        .bind(password_hash)
        .execute(&*self.pool)
        .await
        .map_err(Self::map_sqlx_error)?;

        Ok(())
    }

    /// Changes a user's role
    async fn change_role(&self, user_id: Uuid, role: UserRole) -> UserRepositoryResult<()> {
        // Convert the role to string for the binding
        let role_str = role.to_string();

        sqlx::query(
            r#"
            UPDATE auth.users
            SET 
                role = $2::auth.userrole,
                updated_at = NOW()
            WHERE id = $1
            "#,
        )
        .bind(user_id)
        .bind(&role_str)
        .execute(&*self.pool)
        .await
        .map_err(Self::map_sqlx_error)?;

        Ok(())
    }

    /// Counts users by role with a scalar `COUNT(*)` — no row hydration.
    async fn count_users_by_role(&self, role: &str) -> UserRepositoryResult<i64> {
        sqlx::query_scalar("SELECT COUNT(*) FROM auth.users WHERE role::text = $1")
            .bind(role)
            .fetch_one(&*self.pool)
            .await
            .map_err(Self::map_sqlx_error)
    }

    /// Lists users by role
    async fn list_users_by_role(&self, role: &str) -> UserRepositoryResult<Vec<User>> {
        let rows = sqlx::query(
            r#"
            SELECT
                id, username, email, password_hash, role::text as role_text,
                storage_quota_bytes, storage_used_bytes,
                created_at, updated_at, last_login_at, active,
                federation_kind, federation_issuer, federation_subject, image, is_external,
                given_name, family_name, email_verified_at, preferred_locale, notify_on_share,
                ui_preferences
            FROM auth.users
            WHERE role::text = $1
            ORDER BY created_at DESC
            "#,
        )
        .bind(role)
        .fetch_all(&*self.pool)
        .await
        .map_err(Self::map_sqlx_error)?;

        let users = rows
            .into_iter()
            .map(|row| {
                // Convert role string to UserRole enum for each row
                let role_str: Option<String> = row.try_get("role_text").unwrap_or(None);
                let role = match role_str.as_deref() {
                    Some("admin") => UserRole::Admin,
                    _ => UserRole::User,
                };

                User::from_data_full(
                    row.get("id"),
                    row.get("username"),
                    row.get("email"),
                    row.get("password_hash"),
                    role,
                    row.get("storage_quota_bytes"),
                    row.get("storage_used_bytes"),
                    row.get("created_at"),
                    row.get("updated_at"),
                    row.get("last_login_at"),
                    row.get("active"),
                    row.get::<Option<String>, _>("federation_kind")
                        .as_deref()
                        .and_then(crate::domain::entities::user::FederationKind::parse),
                    row.get("federation_issuer"),
                    row.get("federation_subject"),
                    row.get("image"),
                    row.get("is_external"),
                    row.get("given_name"),
                    row.get("family_name"),
                    row.get("email_verified_at"),
                    row.get("preferred_locale"),
                    row.get("notify_on_share"),
                    row.get::<serde_json::Value, _>("ui_preferences"),
                )
            })
            .collect();

        Ok(users)
    }

    /// Deletes a user
    async fn delete_user(&self, user_id: Uuid) -> UserRepositoryResult<()> {
        sqlx::query(
            r#"
            DELETE FROM auth.users
            WHERE id = $1
            "#,
        )
        .bind(user_id)
        .execute(&*self.pool)
        .await
        .map_err(Self::map_sqlx_error)?;

        Ok(())
    }

    /// Finds a user by OIDC provider + subject pair
    async fn get_user_by_federation_subject(
        &self,
        provider: &str,
        subject: &str,
    ) -> UserRepositoryResult<User> {
        let row = sqlx::query(
            r#"
            SELECT
                id, username, email, password_hash, role::text as role_text,
                storage_quota_bytes, storage_used_bytes,
                created_at, updated_at, last_login_at, active,
                federation_kind, federation_issuer, federation_subject, image, is_external,
                given_name, family_name, email_verified_at, preferred_locale, notify_on_share,
                ui_preferences
            FROM auth.users
            WHERE federation_issuer = $1 AND federation_subject = $2
            "#,
        )
        .bind(provider)
        .bind(subject)
        .fetch_one(&*self.pool)
        .await
        .map_err(Self::map_sqlx_error)?;

        let role_str: Option<String> = row.try_get("role_text").unwrap_or(None);
        let role = match role_str.as_deref() {
            Some("admin") => UserRole::Admin,
            _ => UserRole::User,
        };

        Ok(User::from_data_full(
            row.get("id"),
            row.get("username"),
            row.get("email"),
            row.get("password_hash"),
            role,
            row.get("storage_quota_bytes"),
            row.get("storage_used_bytes"),
            row.get("created_at"),
            row.get("updated_at"),
            row.get("last_login_at"),
            row.get("active"),
            row.get::<Option<String>, _>("federation_kind")
                .as_deref()
                .and_then(crate::domain::entities::user::FederationKind::parse),
            row.get("federation_issuer"),
            row.get("federation_subject"),
            row.get("image"),
            row.get("is_external"),
            row.get("given_name"),
            row.get("family_name"),
            row.get("email_verified_at"),
            row.get("preferred_locale"),
            row.get("notify_on_share"),
            row.get::<serde_json::Value, _>("ui_preferences"),
        ))
    }

    /// Updates a user's storage quota
    async fn update_storage_quota(
        &self,
        user_id: Uuid,
        quota_bytes: i64,
    ) -> UserRepositoryResult<()> {
        sqlx::query(
            r#"
            UPDATE auth.users
            SET 
                storage_quota_bytes = $2,
                updated_at = NOW()
            WHERE id = $1
            "#,
        )
        .bind(user_id)
        .bind(quota_bytes)
        .execute(&*self.pool)
        .await
        .map_err(Self::map_sqlx_error)?;

        Ok(())
    }

    /// Counts the total number of users
    async fn count_users(&self) -> UserRepositoryResult<i64> {
        let row = sqlx::query("SELECT COUNT(*) as count FROM auth.users")
            .fetch_one(&*self.pool)
            .await
            .map_err(Self::map_sqlx_error)?;

        let count: i64 = row.get("count");
        Ok(count)
    }

    /// Gets aggregated storage statistics
    async fn get_storage_stats(&self) -> UserRepositoryResult<StorageStats> {
        let row = sqlx::query(
            r#"
            SELECT
                COUNT(*) as total_users,
                COUNT(*) FILTER (WHERE active = true) as active_users,
                COALESCE(SUM(storage_quota_bytes), 0) as total_quota_bytes,
                COALESCE(SUM(storage_used_bytes), 0) as total_used_bytes,
                COUNT(*) FILTER (WHERE storage_quota_bytes > 0 AND storage_used_bytes > storage_quota_bytes * 0.8) as users_over_80_percent,
                COUNT(*) FILTER (WHERE storage_quota_bytes > 0 AND storage_used_bytes > storage_quota_bytes) as users_over_quota
            FROM auth.users
            "#
        )
        .fetch_one(&*self.pool)
        .await
        .map_err(Self::map_sqlx_error)?;

        Ok(StorageStats {
            total_users: row.get("total_users"),
            active_users: row.get("active_users"),
            total_quota_bytes: row.get("total_quota_bytes"),
            total_used_bytes: row.get("total_used_bytes"),
            users_over_80_percent: row.get("users_over_80_percent"),
            users_over_quota: row.get("users_over_quota"),
        })
    }
}

// Storage port implementation for the application layer
impl UserStoragePort for UserPgRepository {
    async fn create_user(&self, user: User) -> Result<User, DomainError> {
        UserRepository::create_user(self, user)
            .await
            .map_err(DomainError::from)
    }

    async fn get_user_by_id(&self, id: Uuid) -> Result<User, DomainError> {
        UserRepository::get_user_by_id(self, id)
            .await
            .map_err(DomainError::from)
    }

    async fn get_user_with_derived_flags(
        &self,
        id: Uuid,
    ) -> Result<
        (
            User,
            crate::domain::repositories::user_repository::UserDerivedFlags,
        ),
        DomainError,
    > {
        UserRepository::get_user_with_derived_flags(self, id)
            .await
            .map_err(DomainError::from)
    }

    async fn get_users_by_ids(&self, ids: Vec<Uuid>) -> Result<Vec<User>, DomainError> {
        UserRepository::get_users_by_ids(self, ids)
            .await
            .map_err(DomainError::from)
    }

    async fn get_user_by_username(&self, username: &str) -> Result<User, DomainError> {
        UserRepository::get_user_by_username(self, username)
            .await
            .map_err(DomainError::from)
    }

    async fn get_user_by_email(&self, email: &str) -> Result<User, DomainError> {
        UserRepository::get_user_by_email(self, email)
            .await
            .map_err(DomainError::from)
    }

    async fn list_users_by_normalized_email(
        &self,
        normalized_email: &str,
    ) -> Result<Vec<User>, DomainError> {
        UserRepository::list_users_by_normalized_email(self, normalized_email)
            .await
            .map_err(DomainError::from)
    }

    async fn update_user(&self, user: User) -> Result<User, DomainError> {
        UserRepository::update_user(self, user)
            .await
            .map_err(DomainError::from)
    }

    async fn update_storage_usage(
        &self,
        user_id: Uuid,
        usage_bytes: i64,
    ) -> Result<(), DomainError> {
        UserRepository::update_storage_usage(self, user_id, usage_bytes)
            .await
            .map_err(DomainError::from)
    }

    async fn list_users(
        &self,
        limit: i64,
        offset: i64,
        include_external: bool,
    ) -> Result<Vec<User>, DomainError> {
        UserRepository::list_users(self, limit, offset, include_external)
            .await
            .map_err(DomainError::from)
    }

    async fn list_users_with_derived_flags(
        &self,
        limit: i64,
        offset: i64,
        include_external: bool,
    ) -> Result<
        Vec<(
            User,
            crate::domain::repositories::user_repository::UserDerivedFlags,
        )>,
        DomainError,
    > {
        UserRepository::list_users_with_derived_flags(self, limit, offset, include_external)
            .await
            .map_err(DomainError::from)
    }

    async fn search_users(
        &self,
        query: &str,
        limit: i64,
        include_external: bool,
    ) -> Result<Vec<User>, DomainError> {
        UserRepository::search_users(self, query, limit, include_external)
            .await
            .map_err(DomainError::from)
    }

    async fn search_usernames(
        &self,
        query: &str,
        limit: i64,
        include_external: bool,
    ) -> Result<Vec<Option<String>>, DomainError> {
        // Same predicate / order / limit as `search_users`, username-only
        // projection — the sharee autocomplete path reads nothing else, and
        // the wide row drags the avatar `image` per matched user.
        let pattern = format!("%{}%", query);
        let rows = sqlx::query(
            r#"
            SELECT username
            FROM auth.users
            WHERE (username ILIKE $1 OR email ILIKE $1)
              AND ($3 OR is_external = FALSE)
            ORDER BY username
            LIMIT $2
            "#,
        )
        .bind(&pattern)
        .bind(limit)
        .bind(include_external)
        .fetch_all(&*self.pool)
        .await
        .map_err(Self::map_sqlx_error)
        .map_err(DomainError::from)?;
        Ok(rows.into_iter().map(|row| row.get("username")).collect())
    }

    async fn mark_email_verified(&self, user_id: Uuid) -> Result<(), DomainError> {
        // SQL twin of `User::mark_email_verified` — stamps once, keeps the
        // first timestamp, and touches only the two columns involved.
        sqlx::query(
            r#"
            UPDATE auth.users
            SET email_verified_at = NOW(), updated_at = NOW()
            WHERE id = $1 AND email_verified_at IS NULL
            "#,
        )
        .bind(user_id)
        .execute(&*self.pool)
        .await
        .map_err(Self::map_sqlx_error)
        .map_err(DomainError::from)?;
        Ok(())
    }

    async fn sync_oidc_login_profile(
        &self,
        user_id: Uuid,
        image: Option<&str>,
    ) -> Result<(), DomainError> {
        // `IS DISTINCT FROM` guard (the `update_storage_usage` pattern): the
        // common repeat-login case — same IdP avatar, already verified —
        // writes nothing at all (no dead tuple, no WAL).
        sqlx::query(
            r#"
            UPDATE auth.users
            SET image = $2,
                email_verified_at = COALESCE(email_verified_at, NOW()),
                updated_at = NOW()
            WHERE id = $1
              AND (image IS DISTINCT FROM $2 OR email_verified_at IS NULL)
            "#,
        )
        .bind(user_id)
        .bind(image)
        .execute(&*self.pool)
        .await
        .map_err(Self::map_sqlx_error)
        .map_err(DomainError::from)?;
        Ok(())
    }

    async fn rebind_federation_issuer(
        &self,
        user_id: Uuid,
        new_issuer: &str,
    ) -> Result<(), DomainError> {
        // Same `IS DISTINCT FROM` guard as sync_oidc_login_profile: this
        // fires on every OIDC login, so the common already-migrated case
        // must be a zero-write no-op. Only actually flips the column
        // when the stored value is stale (legacy display label vs the
        // real issuer URL from the id_token's `iss` claim).
        sqlx::query(
            r#"
            UPDATE auth.users
            SET federation_issuer = $2,
                updated_at = NOW()
            WHERE id = $1
              AND federation_issuer IS DISTINCT FROM $2
            "#,
        )
        .bind(user_id)
        .bind(new_issuer)
        .execute(&*self.pool)
        .await
        .map_err(Self::map_sqlx_error)
        .map_err(DomainError::from)?;
        Ok(())
    }

    async fn link_federation_identity(
        &self,
        user_id: Uuid,
        kind: &str,
        issuer: &str,
        subject: &str,
    ) -> Result<(), DomainError> {
        // Guarded UPDATE: only proceed when the row currently has NO
        // federation identity. Prevents accidental identity overwrite —
        // callers wanting to replace an existing link must go through
        // unlink first. Silent no-op on already-linked rows is WRONG
        // because it would swallow the intent; instead we return an
        // error the app service translates to `already_linked`.
        //
        // Uniqueness enforcement lives on `idx_users_federation`
        // (UNIQUE(kind, issuer, subject) WHERE federation_kind IS NOT
        // NULL). If this triple is already bound to a DIFFERENT user,
        // the UPDATE succeeds row-count = 0 (the WHERE constrains us to
        // rows for THIS user_id) — but the following INSERT-shaped
        // UPDATE approach doesn't trigger the unique index; we rely on
        // the app service having pre-checked via
        // `get_user_by_federation_subject`. If that pre-check races
        // with a concurrent link (rare), the second call surfaces
        // `AlreadyExists` from sqlx via `map_sqlx_error`.
        let result = sqlx::query(
            r#"
            UPDATE auth.users
               SET federation_kind    = $2,
                   federation_issuer  = $3,
                   federation_subject = $4,
                   updated_at         = NOW()
             WHERE id = $1
               AND federation_kind IS NULL
            "#,
        )
        .bind(user_id)
        .bind(kind)
        .bind(issuer)
        .bind(subject)
        .execute(&*self.pool)
        .await
        .map_err(Self::map_sqlx_error)
        .map_err(DomainError::from)?;

        if result.rows_affected() == 0 {
            // Either the user doesn't exist OR they already have a
            // federation identity attached. The app service should have
            // already validated user existence + link state; being here
            // usually means a concurrent link race.
            return Err(DomainError::already_exists(
                "User",
                "user is already linked to a federation identity",
            ));
        }
        Ok(())
    }

    async fn is_opaque_registered(&self, user_id: Uuid) -> Result<bool, DomainError> {
        // Scalar `IS NOT NULL` check — the envelope is a few hundred
        // bytes of ciphertext; we don't want to fetch it just to
        // examine presence. `fetch_optional` returns None if the user
        // doesn't exist (caller treats missing as "not registered").
        let row: Option<(bool,)> = sqlx::query_as(
            r#"
            SELECT (opaque_envelope IS NOT NULL)
              FROM auth.users
             WHERE id = $1
            "#,
        )
        .bind(user_id)
        .fetch_optional(&*self.pool)
        .await
        .map_err(Self::map_sqlx_error)
        .map_err(DomainError::from)?;
        Ok(row.map(|(v,)| v).unwrap_or(false))
    }

    async fn unlink_federation_identity(&self, user_id: Uuid) -> Result<(), DomainError> {
        // Idempotent: unlinking an already-unlinked user is a zero-row
        // UPDATE. App service's `no_alternative_auth` refusal guard
        // runs BEFORE this — the DB layer just moves the columns.
        sqlx::query(
            r#"
            UPDATE auth.users
               SET federation_kind    = NULL,
                   federation_issuer  = NULL,
                   federation_subject = NULL,
                   updated_at         = NOW()
             WHERE id = $1
            "#,
        )
        .bind(user_id)
        .execute(&*self.pool)
        .await
        .map_err(Self::map_sqlx_error)
        .map_err(DomainError::from)?;
        Ok(())
    }

    async fn list_users_by_role(&self, role: &str) -> Result<Vec<User>, DomainError> {
        UserRepository::list_users_by_role(self, role)
            .await
            .map_err(DomainError::from)
    }

    async fn count_users_by_role(&self, role: &str) -> Result<i64, DomainError> {
        UserRepository::count_users_by_role(self, role)
            .await
            .map_err(DomainError::from)
    }

    async fn delete_user(&self, user_id: Uuid) -> Result<(), DomainError> {
        UserRepository::delete_user(self, user_id)
            .await
            .map_err(DomainError::from)
    }

    async fn change_password(&self, user_id: Uuid, password_hash: &str) -> Result<(), DomainError> {
        UserRepository::change_password(self, user_id, password_hash)
            .await
            .map_err(DomainError::from)
    }

    async fn get_user_by_federation_subject(
        &self,
        provider: &str,
        subject: &str,
    ) -> Result<User, DomainError> {
        UserRepository::get_user_by_federation_subject(self, provider, subject)
            .await
            .map_err(DomainError::from)
    }

    async fn set_user_active_status(&self, user_id: Uuid, active: bool) -> Result<(), DomainError> {
        UserRepository::set_user_active_status(self, user_id, active)
            .await
            .map_err(DomainError::from)
    }

    async fn change_role(&self, user_id: Uuid, role: &str) -> Result<(), DomainError> {
        let user_role = match role {
            "admin" => UserRole::Admin,
            _ => UserRole::User,
        };
        UserRepository::change_role(self, user_id, user_role)
            .await
            .map_err(DomainError::from)
    }

    async fn update_storage_quota(
        &self,
        user_id: Uuid,
        quota_bytes: i64,
    ) -> Result<(), DomainError> {
        UserRepository::update_storage_quota(self, user_id, quota_bytes)
            .await
            .map_err(DomainError::from)
    }

    async fn count_users(&self) -> Result<i64, DomainError> {
        UserRepository::count_users(self)
            .await
            .map_err(DomainError::from)
    }
}

#[cfg(integration_tests)]
#[allow(dead_code)]
mod integration_tests {
    use super::*;
    use crate::integration_test_support::{ensure_clean_test_db, test_db_url};
    use sqlx::postgres::PgPoolOptions;

    async fn test_repo() -> UserPgRepository {
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(&test_db_url())
            .await
            .expect("connect to integration-test PostgreSQL");
        ensure_clean_test_db(&pool).await;
        UserPgRepository::new(Arc::new(pool))
    }

    async fn insert_summary_fixture(
        repo: &UserPgRepository,
        id: Uuid,
        username: Option<&str>,
        email: &str,
        role: &str,
        is_external: bool,
    ) {
        sqlx::query(
            r#"
            INSERT INTO auth.users (
                id, username, email, password_hash, role,
                storage_quota_bytes, storage_used_bytes,
                created_at, updated_at, last_login_at, active,
                federation_issuer, is_external
            ) VALUES (
                $1, $2, $3, NULL, $4::auth.userrole,
                $5, 0,
                '9999-12-31 23:59:59+00', '9999-12-31 23:59:59+00', NULL, TRUE,
                $6, $7
            )
            "#,
        )
        .bind(id)
        .bind(username)
        .bind(email)
        .bind(role)
        .bind(if is_external {
            0_i64
        } else {
            10_737_418_240_i64
        })
        .bind(is_external.then_some("integration-idp"))
        .bind(is_external)
        .execute(repo.pool.as_ref())
        .await
        .expect("insert compact-list fixture");
    }

    #[tokio::test]
    async fn compact_listing_maps_narrow_columns_and_stably_breaks_timestamp_ties() {
        let repo = test_repo().await;
        sqlx::query("DELETE FROM auth.users WHERE email LIKE 'perf-summary-%@example.invalid'")
            .execute(repo.pool.as_ref())
            .await
            .expect("clean stale compact-list fixtures");
        let mut ids = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
        ids.sort_unstable_by(|left, right| right.cmp(left));
        let username_a = format!("perf-summary-a-{}", ids[0]);
        let username_b = format!("perf-summary-b-{}", ids[2]);

        insert_summary_fixture(
            &repo,
            ids[0],
            Some(&username_a),
            &format!("perf-summary-{}@example.invalid", ids[0]),
            "admin",
            false,
        )
        .await;
        insert_summary_fixture(
            &repo,
            ids[1],
            None,
            &format!("perf-summary-{}@example.invalid", ids[1]),
            "user",
            true,
        )
        .await;
        insert_summary_fixture(
            &repo,
            ids[2],
            Some(&username_b),
            &format!("perf-summary-{}@example.invalid", ids[2]),
            "user",
            false,
        )
        .await;

        // Migrated from the (now-deleted) `list_user_summaries` +
        // `UserListEntry` to `list_users_with_derived_flags`, which
        // returns `Vec<(User, UserDerivedFlags)>`. Field checks read
        // through the `User` accessors instead of struct-field access.
        let page = UserRepository::list_users_with_derived_flags(&repo, 3, 0, true)
            .await
            .expect("compact projection query must decode");
        assert_eq!(page.iter().map(|(u, _)| u.id()).collect::<Vec<_>>(), ids);
        assert_eq!(page[0].0.username(), Some(username_a.as_str()));
        assert_eq!(page[0].0.role(), UserRole::Admin);
        assert_eq!(page[0].0.storage_quota_bytes(), 10_737_418_240);
        assert_eq!(page[1].0.username(), None);
        assert!(page[1].0.is_external());
        assert_eq!(page[1].0.federation_issuer(), Some("integration-idp"));

        let internal = UserRepository::list_users_with_derived_flags(&repo, 10, 0, false)
            .await
            .expect("internal compact projection query must decode");
        assert!(internal.iter().any(|(u, _)| u.id() == ids[0]));
        assert!(internal.iter().any(|(u, _)| u.id() == ids[2]));
        assert!(!internal.iter().any(|(u, _)| u.id() == ids[1]));

        sqlx::query("DELETE FROM auth.users WHERE id = ANY($1)")
            .bind(ids.as_slice())
            .execute(repo.pool.as_ref())
            .await
            .expect("clean compact-list fixtures");
    }
}
