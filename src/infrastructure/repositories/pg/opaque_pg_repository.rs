//! PostgreSQL repository for OPAQUE aPAKE envelopes.
//!
//! Backs [`OpaqueRepositoryPort`] against the three OPAQUE columns on
//! `auth.users` introduced by migration `20260930000002_auth_opaque.sql`:
//!
//!   * `opaque_envelope BYTEA` — the serialised registration blob,
//!   * `opaque_ciphersuite_version SMALLINT` — the bound suite version,
//!   * `opaque_registered_at TIMESTAMPTZ` — first-registration timestamp,
//!
//! plus the co-located `force_password_change_at_next_login BOOLEAN`
//! toggled by [`clear_registration`].
//!
//! No caching. OPAQUE reads happen at most once per login (the
//! server hands the envelope to `ServerLogin::start` and that's it),
//! so the added cache-invalidation complexity would earn nothing. If
//! that ever changes (batch-endpoint use), the moka pattern in
//! `login_lockout_service` is the shape to reach for.

use std::sync::Arc;

use async_trait::async_trait;
use sqlx::PgPool;
use uuid::Uuid;

use crate::application::ports::opaque_ports::{OpaqueRepositoryPort, StoredEnvelope};
use crate::common::errors::{DomainError, Result};

pub struct OpaquePgRepository {
    pool: Arc<PgPool>,
}

impl OpaquePgRepository {
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool }
    }

    fn pool(&self) -> &PgPool {
        &self.pool
    }
}

#[async_trait]
impl OpaqueRepositoryPort for OpaquePgRepository {
    async fn write_registration(
        &self,
        user_id: Uuid,
        envelope: &[u8],
        ciphersuite_version: i16,
    ) -> Result<()> {
        // COALESCE preserves the first-registration timestamp across
        // re-registrations (password change → new envelope, same
        // registered_at). The alternative — always stamping `NOW()`
        // — would erase the operational signal "when did this user
        // first join OPAQUE," which the migration dashboard reads.
        let res = sqlx::query(
            r#"
            UPDATE auth.users
               SET opaque_envelope            = $2,
                   opaque_ciphersuite_version = $3,
                   opaque_registered_at       = COALESCE(opaque_registered_at, NOW())
             WHERE id = $1
            "#,
        )
        .bind(user_id)
        .bind(envelope)
        .bind(ciphersuite_version)
        .execute(self.pool())
        .await
        .map_err(|e| DomainError::internal_error("OpaquePg", format!("write_registration: {e}")))?;

        if res.rows_affected() == 0 {
            // No matching user id — caller expected the user to exist.
            // We surface this as NotFound rather than swallowing so the
            // handler layer can 404 anti-enum consistently.
            return Err(DomainError::not_found("User", user_id.to_string()));
        }
        Ok(())
    }

    async fn read_registration(&self, user_id: Uuid) -> Result<Option<StoredEnvelope>> {
        // `try_get` on the envelope column returns None when the row
        // exists but the column is NULL (the Phase 0 default for every
        // pre-migration account). A missing row propagates as NotFound
        // via the same anti-enum path as `write_registration`.
        let row = sqlx::query_as::<_, EnvelopeRow>(
            r#"
            SELECT opaque_envelope,
                   opaque_ciphersuite_version,
                   opaque_registered_at
              FROM auth.users
             WHERE id = $1
            "#,
        )
        .bind(user_id)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| DomainError::internal_error("OpaquePg", format!("read_registration: {e}")))?;

        let Some(row) = row else {
            return Err(DomainError::not_found("User", user_id.to_string()));
        };

        // All three columns are NULL together (they're set atomically by
        // `write_registration`). Any partial-NULL is a schema-drift
        // symptom — return None with a warn so ops can catch it.
        match (row.envelope, row.ciphersuite_version, row.registered_at) {
            (Some(env), Some(ver), Some(at)) => Ok(Some(StoredEnvelope {
                envelope: env,
                ciphersuite_version: ver,
                registered_at: at,
            })),
            (None, None, None) => Ok(None),
            (env, ver, at) => {
                tracing::warn!(
                    target: "oxicloud::opaque",
                    user_id = %user_id,
                    envelope_set = env.is_some(),
                    version_set = ver.is_some(),
                    registered_at_set = at.is_some(),
                    "OPAQUE columns partial-NULL — treating as unregistered. \
                     This shouldn't happen; check for a broken migration."
                );
                Ok(None)
            }
        }
    }

    async fn mark_migrated(&self, user_id: Uuid) -> Result<()> {
        // COALESCE preserves the first-migration timestamp — same
        // pattern as write_registration preserves opaque_registered_at.
        // Ops dashboards read this to answer "what fraction of users
        // have completed the OPAQUE cutover", so overwriting on every
        // login would erase the signal.
        let res = sqlx::query(
            r#"
            UPDATE auth.users
               SET opaque_migrated_at = COALESCE(opaque_migrated_at, NOW())
             WHERE id = $1
            "#,
        )
        .bind(user_id)
        .execute(self.pool())
        .await
        .map_err(|e| DomainError::internal_error("OpaquePg", format!("mark_migrated: {e}")))?;

        if res.rows_affected() == 0 {
            return Err(DomainError::not_found("User", user_id.to_string()));
        }
        Ok(())
    }

    async fn is_migrated(&self, user_id: Uuid) -> Result<bool> {
        // Cheap presence check on the partial index
        // `idx_users_opaque_migrated`. `fetch_optional` returning `None`
        // covers both "no such user" and "user exists but not migrated";
        // Phase 4's gate collapses both to `false` (anti-enum — the
        // wrong-password branch upstream has already covered the
        // user-lookup miss).
        let row: Option<(Option<chrono::DateTime<chrono::Utc>>,)> = sqlx::query_as(
            r#"
            SELECT opaque_migrated_at
              FROM auth.users
             WHERE id = $1
            "#,
        )
        .bind(user_id)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| DomainError::internal_error("OpaquePg", format!("is_migrated: {e}")))?;
        Ok(row.and_then(|(t,)| t).is_some())
    }

    async fn clear_registration(&self, user_id: Uuid) -> Result<()> {
        // One UPDATE writes both the envelope invalidation AND the
        // force-change flag — matches the atomicity we promise in the
        // port doc, avoids drift between two separate writes.
        let res = sqlx::query(
            r#"
            UPDATE auth.users
               SET opaque_envelope                     = NULL,
                   opaque_ciphersuite_version          = NULL,
                   opaque_registered_at                = NULL,
                   opaque_migrated_at                  = NULL,
                   force_password_change_at_next_login = TRUE
             WHERE id = $1
            "#,
        )
        .bind(user_id)
        .execute(self.pool())
        .await
        .map_err(|e| DomainError::internal_error("OpaquePg", format!("clear_registration: {e}")))?;

        if res.rows_affected() == 0 {
            return Err(DomainError::not_found("User", user_id.to_string()));
        }
        Ok(())
    }
}

#[derive(sqlx::FromRow)]
struct EnvelopeRow {
    #[sqlx(rename = "opaque_envelope")]
    envelope: Option<Vec<u8>>,
    #[sqlx(rename = "opaque_ciphersuite_version")]
    ciphersuite_version: Option<i16>,
    #[sqlx(rename = "opaque_registered_at")]
    registered_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[cfg(integration_tests)]
#[allow(dead_code)]
mod integration_tests {
    use super::*;
    use crate::integration_test_support::{ensure_clean_test_db, test_db_url};
    use sqlx::postgres::PgPoolOptions;

    async fn test_repo() -> OpaquePgRepository {
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(&test_db_url())
            .await
            .expect("connect to integration-test PostgreSQL");
        ensure_clean_test_db(&pool).await;
        OpaquePgRepository::new(Arc::new(pool))
    }

    /// Hermetic per-test user seed. `project_test_fixture_self_seeding`
    /// memo: don't couple to `init-test-schema.sh`'s implicit admin —
    /// mint a fresh user with a unique email so parallel tests don't
    /// stomp each other. Returns the new user's id.
    async fn seed_user(repo: &OpaquePgRepository, email: &str) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query(
            r#"
            INSERT INTO auth.users (
                id, username, email, password_hash, role,
                storage_quota_bytes, storage_used_bytes,
                created_at, updated_at, active
            ) VALUES (
                $1, NULL, $2, NULL, 'user'::auth.userrole,
                0, 0, NOW(), NOW(), TRUE
            )
            "#,
        )
        .bind(id)
        .bind(email)
        .execute(repo.pool())
        .await
        .expect("seed test user");
        id
    }

    #[tokio::test]
    async fn write_then_read_round_trips_envelope_and_version() {
        let repo = test_repo().await;
        let user = seed_user(
            &repo,
            &format!("opaque-rt-{}@example.invalid", Uuid::new_v4()),
        )
        .await;

        assert!(
            repo.read_registration(user)
                .await
                .expect("read pre")
                .is_none(),
            "seed user must start with no envelope"
        );

        let payload = b"envelope-v1-bytes".to_vec();
        repo.write_registration(user, &payload, 1)
            .await
            .expect("write");

        let stored = repo
            .read_registration(user)
            .await
            .expect("read post")
            .expect("envelope now present");
        assert_eq!(stored.envelope, payload);
        assert_eq!(stored.ciphersuite_version, 1);
    }

    #[tokio::test]
    async fn re_registration_preserves_registered_at_but_swaps_envelope() {
        // Password change / silent-migration re-mint: fresh envelope
        // bytes + potentially a new suite version, but the
        // first-registration timestamp must stay stable (ops dashboard
        // reads it to know when the user joined OPAQUE).
        let repo = test_repo().await;
        let user = seed_user(
            &repo,
            &format!("opaque-rereg-{}@example.invalid", Uuid::new_v4()),
        )
        .await;

        repo.write_registration(user, b"first-envelope", 1)
            .await
            .expect("first write");
        let first = repo.read_registration(user).await.unwrap().unwrap();

        // Tiny sleep so a bug that overwrites registered_at with NOW()
        // would produce a measurably different timestamp.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        repo.write_registration(user, b"second-envelope", 1)
            .await
            .expect("second write");
        let second = repo.read_registration(user).await.unwrap().unwrap();

        assert_eq!(second.envelope, b"second-envelope");
        assert_eq!(
            second.registered_at, first.registered_at,
            "registered_at must be preserved across re-registration (COALESCE guard)"
        );
    }

    #[tokio::test]
    async fn clear_registration_nulls_columns_and_sets_force_change_flag() {
        let repo = test_repo().await;
        let user = seed_user(
            &repo,
            &format!("opaque-clr-{}@example.invalid", Uuid::new_v4()),
        )
        .await;

        repo.write_registration(user, b"envelope", 1)
            .await
            .expect("prime with envelope");
        repo.clear_registration(user)
            .await
            .expect("clear registration");

        // Envelope columns are back to NULL.
        assert!(
            repo.read_registration(user).await.unwrap().is_none(),
            "clear must NULL the envelope"
        );

        // Force-change flag is TRUE — this is the load-bearing behaviour
        // that keeps admin-set passwords temporary.
        let flag: (bool,) = sqlx::query_as(
            "SELECT force_password_change_at_next_login FROM auth.users WHERE id = $1",
        )
        .bind(user)
        .fetch_one(repo.pool())
        .await
        .expect("read force-change flag");
        assert!(flag.0, "clear must set force_password_change_at_next_login");
    }

    /// End-to-end proof: run the FULL OPAQUE register handshake
    /// client-side against the real ciphersuite, persist the resulting
    /// envelope through this repository, read it back on a fresh
    /// connection, and use those bytes to complete a real login
    /// handshake. This is the load-bearing test for Phase 1 Step 3 —
    /// it proves the shape the register endpoints will land on:
    ///
    ///   client_register.start
    ///     → ServerRegistration::start (server-side, produces response)
    ///   client_register.finish
    ///     → ServerRegistration::finish → serialize → repo.write
    ///   repo.read
    ///     → ServerRegistration::deserialize
    ///     → ServerLogin::start (with the stored password_file)
    ///   client_login.finish → ServerLogin::finish
    ///     → session_key matches
    ///
    /// If any of these steps drift (envelope shape change, ciphersuite
    /// mismatch, serialisation format regression), this test catches
    /// it — without needing to spin up an HTTP server.
    #[tokio::test]
    async fn envelope_persists_across_register_and_serves_a_matching_login() {
        use crate::infrastructure::services::opaque_service::OxiCloudSuite;
        use opaque_ke::{
            ClientLogin, ClientLoginFinishParameters, ClientRegistration,
            ClientRegistrationFinishParameters, ServerLogin, ServerLoginStartParameters,
            ServerRegistration, ServerSetup,
        };
        use rand_core::OsRng;

        let repo = test_repo().await;
        let user = seed_user(
            &repo,
            &format!("opaque-e2e-{}@example.invalid", Uuid::new_v4()),
        )
        .await;

        // Fresh server setup for this test only — mirrors what the
        // DI factory would load from OXICLOUD_OPAQUE_SERVER_SETUP.
        let mut server_rng = OsRng;
        let server_setup = ServerSetup::<OxiCloudSuite>::new(&mut server_rng);

        // Fast KSF so the test finishes in ms rather than seconds.
        let ksf = argon2::Argon2::new(
            argon2::Algorithm::Argon2id,
            argon2::Version::V0x13,
            argon2::Params::new(8, 1, 1, None).unwrap(),
        );
        let mut client_rng = OsRng;
        let user_bytes = user.as_bytes();
        let passphrase = b"correct horse battery staple";

        // ── REGISTER — same shape the /register/{start,finish} handlers run ─
        let client_reg = ClientRegistration::<OxiCloudSuite>::start(&mut client_rng, passphrase)
            .expect("client_register.start");
        let server_reg = ServerRegistration::<OxiCloudSuite>::start(
            &server_setup,
            client_reg.message,
            user_bytes,
        )
        .expect("server_register.start");
        let client_reg_finish = client_reg
            .state
            .finish(
                &mut client_rng,
                passphrase,
                server_reg.message,
                ClientRegistrationFinishParameters::new(
                    opaque_ke::Identifiers::default(),
                    Some(&ksf),
                ),
            )
            .expect("client_register.finish");
        let password_file = ServerRegistration::<OxiCloudSuite>::finish(client_reg_finish.message);

        // Persist through the repo — this is exactly what `register/finish`
        // will do at the handler layer. `.serialize()` returns a
        // `GenericArray`; convert to `Vec<u8>` at the boundary so
        // downstream comparisons stay simple.
        let envelope_bytes: Vec<u8> = password_file.serialize().to_vec();
        repo.write_registration(user, &envelope_bytes, 1)
            .await
            .expect("persist envelope");

        // ── LOGIN — reads the envelope back the way `login/ke1` will ─────
        let stored = repo
            .read_registration(user)
            .await
            .expect("read")
            .expect("envelope present after write");
        assert_eq!(stored.ciphersuite_version, 1);
        assert_eq!(
            stored.envelope, envelope_bytes,
            "stored bytes must round-trip verbatim"
        );

        let password_file_back = ServerRegistration::<OxiCloudSuite>::deserialize(&stored.envelope)
            .expect("deserialize stored envelope");

        let client_login = ClientLogin::<OxiCloudSuite>::start(&mut client_rng, passphrase)
            .expect("client_login.start");
        let server_login = ServerLogin::start(
            &mut server_rng,
            &server_setup,
            Some(password_file_back),
            client_login.message,
            user_bytes,
            ServerLoginStartParameters::default(),
        )
        .expect("server_login.start (with stored envelope)");
        let client_login_finish = client_login
            .state
            .finish(
                passphrase,
                server_login.message,
                ClientLoginFinishParameters::new(
                    None,
                    opaque_ke::Identifiers::default(),
                    Some(&ksf),
                ),
            )
            .expect("client_login.finish");
        let server_login_finish = server_login
            .state
            .finish(client_login_finish.message)
            .expect("server_login.finish");

        assert_eq!(
            client_login_finish.session_key.as_slice(),
            server_login_finish.session_key.as_slice(),
            "session keys must match after a full register → persist → login round trip"
        );
    }

    /// Mark_migrated is idempotent — a second call preserves the
    /// first-migration timestamp, matching the ops-dashboard contract
    /// ("when did this user first complete OPAQUE?").
    #[tokio::test]
    async fn mark_migrated_stamps_once_and_is_idempotent() {
        let repo = test_repo().await;
        let user = seed_user(
            &repo,
            &format!("opaque-mig-{}@example.invalid", Uuid::new_v4()),
        )
        .await;

        // Read the initial NULL state
        let initial: (Option<chrono::DateTime<chrono::Utc>>,) =
            sqlx::query_as("SELECT opaque_migrated_at FROM auth.users WHERE id = $1")
                .bind(user)
                .fetch_one(repo.pool())
                .await
                .unwrap();
        assert!(
            initial.0.is_none(),
            "new user starts with no opaque_migrated_at"
        );

        repo.mark_migrated(user).await.expect("first mark");
        let first: (Option<chrono::DateTime<chrono::Utc>>,) =
            sqlx::query_as("SELECT opaque_migrated_at FROM auth.users WHERE id = $1")
                .bind(user)
                .fetch_one(repo.pool())
                .await
                .unwrap();
        let first_ts = first.0.expect("timestamp set after first mark");

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        repo.mark_migrated(user).await.expect("second mark");
        let second: (Option<chrono::DateTime<chrono::Utc>>,) =
            sqlx::query_as("SELECT opaque_migrated_at FROM auth.users WHERE id = $1")
                .bind(user)
                .fetch_one(repo.pool())
                .await
                .unwrap();
        assert_eq!(
            second.0.unwrap(),
            first_ts,
            "second mark_migrated preserves the first timestamp (COALESCE)"
        );
    }

    /// `is_migrated` returns false until `mark_migrated` stamps
    /// `opaque_migrated_at`, then flips to true. `clear_registration`
    /// re-opens the fallback by NULL-ing the column. This is the
    /// exact state machine the Phase 4 legacy-login gate reads.
    #[tokio::test]
    async fn is_migrated_tracks_mark_and_clear_state_transitions() {
        let repo = test_repo().await;
        let user = seed_user(
            &repo,
            &format!("opaque-ism-{}@example.invalid", Uuid::new_v4()),
        )
        .await;

        // Fresh user: no envelope, no migration mark.
        assert!(
            !repo.is_migrated(user).await.unwrap(),
            "fresh user must not be marked migrated"
        );

        // Mark migrated — should flip the read to true. The service-
        // level gate refuses legacy login from this point onward.
        repo.mark_migrated(user).await.expect("mark migrated");
        assert!(
            repo.is_migrated(user).await.unwrap(),
            "user must be marked migrated after mark_migrated"
        );

        // Admin password reset (clear_registration) MUST re-open the
        // legacy fallback by NULL-ing opaque_migrated_at — otherwise
        // an admin-reset user would be locked out of their own account
        // (no envelope, but Phase 4 gate still refuses legacy).
        repo.clear_registration(user)
            .await
            .expect("clear registration");
        assert!(
            !repo.is_migrated(user).await.unwrap(),
            "clear_registration must NULL opaque_migrated_at to re-open the legacy fallback"
        );
    }

    /// Missing user reads as `false` (anti-enum). The service-layer
    /// gate must not distinguish "user gone" from "user not migrated"
    /// — the upstream user lookup + password check already covered
    /// the "unknown identifier" branch.
    #[tokio::test]
    async fn is_migrated_returns_false_for_missing_user() {
        let repo = test_repo().await;
        let ghost = Uuid::new_v4();
        assert!(
            !repo.is_migrated(ghost).await.unwrap(),
            "missing user must read as not-migrated (anti-enum)"
        );
    }

    #[tokio::test]
    async fn missing_user_surfaces_notfound_on_write_and_read_and_clear() {
        let repo = test_repo().await;
        let ghost = Uuid::new_v4();

        for err in [
            repo.write_registration(ghost, b"whatever", 1)
                .await
                .unwrap_err(),
            repo.read_registration(ghost).await.unwrap_err(),
            repo.clear_registration(ghost).await.unwrap_err(),
            repo.mark_migrated(ghost).await.unwrap_err(),
        ] {
            assert_eq!(
                err.kind,
                crate::common::errors::ErrorKind::NotFound,
                "missing-user path must surface as NotFound (anti-enum)"
            );
        }
    }
}
