//! App Password application service.
//!
//! Orchestrates creation, verification, listing, and revocation of
//! application-specific passwords for DAV clients.

use crate::application::dtos::app_password_dto::*;
use crate::application::ports::auth_ports::{
    AppPasswordStoragePort, PasswordHasherPort, UserStoragePort,
};
use crate::application::ports::scope_ports::{CredentialKind, ScopeSet};
use crate::common::errors::{DomainError, ErrorKind};
use crate::domain::entities::app_password::AppPassword;
use crate::infrastructure::repositories::pg::AppPasswordPgRepository;
use crate::infrastructure::repositories::pg::UserPgRepository;
use crate::infrastructure::services::password_hasher::Argon2PasswordHasher;
use chrono::{Duration, Utc};
use moka::future::Cache;
use rand_core::RngCore;
use std::sync::Arc;
use std::time::Duration as StdDuration;
use uuid::Uuid;

/// App password token length (32 random alphanumeric chars after prefix).
const TOKEN_LENGTH: usize = 32;
/// Prefix for all app password tokens (makes them easily identifiable).
const TOKEN_PREFIX: &str = "oxicloud-";

// ── Nextcloud-format app password constants ──
const NC_APP_PASSWORD_GROUPS: usize = 5;
const NC_APP_PASSWORD_GROUP_LEN: usize = 5;
const NC_PREFIX_LEN: usize = 8;

/// TTL for cached Basic Auth verification results.
///
/// DAV sync clients (Nautilus, Windows Explorer, Apple Calendar, …) poll
/// continuously, and every cache miss costs a full Argon2id verification
/// (~50–100 ms of CPU) plus two DB round-trips. A 30 s TTL re-paid that
/// cost every 30 s per client; 5 min cuts it ~10× under steady sync load.
///
/// Security envelope of this window:
/// - **Revocation is immediate**: `revoke()` calls `invalidate_entries_if`
///   on this cache for the user, so a revoked password never survives in
///   cache regardless of TTL.
/// - **Expiry / deactivation are bounded by the TTL**: `expires_at` and
///   `user.is_active()` are only re-checked on a cache *miss* (the DB
///   query filters them), so an app password that expires — or a user
///   deactivated via `set_user_active` — may keep authenticating from
///   cache for at most this long. 5 min is comparable to a typical JWT
///   access-token lifetime, so the grace window is consistent across
///   auth surfaces. Lengthen with care; shorten if a tighter bound on
///   post-deactivation access is required.
const BASIC_AUTH_CACHE_TTL_SECS: u64 = 300;

/// Maximum number of cached Basic Auth verifications.
/// Each entry is ~160 bytes (32-byte key + 4 small strings), so 10 000
/// entries ≈ 1.6 MB — negligible compared to other in-memory caches.
const BASIC_AUTH_CACHE_MAX_ENTRIES: u64 = 10_000;

/// Cached identity returned after a successful Basic Auth verification.
///
/// `Arc<str>` fields: moka's `get` clones the value, so with owned
/// `String`s every warm Basic-auth request (all DAV traffic) paid string
/// copies just to read the cached identity. Now a hit is refcount bumps.
///
/// **No role.** It was cached here and DROPPED by both callers, each with
/// a comment saying why: this cache has a 300 s TTL, so a role read from
/// it can name one the user no longer holds, and the live value is
/// re-resolved per request from the 30 s flags cache. Carrying it was 24
/// bytes of staleness that only invited a third caller to trust it.
/// **Scopes ARE cached, unlike the role above, and the difference is
/// mutability.** A role changes under a live credential — an admin
/// demotes a user and the 300 s TTL would keep serving the old one. An
/// app password's scopes are fixed at mint: the three endpoints are
/// create, list and revoke, with nothing that edits an existing row's
/// `scopes` column. So a cached copy cannot name access the credential
/// no longer has.
///
/// If a scope-editing endpoint is ever added, that stops being true and
/// this field must come out of the cache — the narrowing would not take
/// effect for up to 300 s, which is exactly the staleness the role note
/// above rejects.
#[derive(Clone)]
struct CachedBasicAuthResult {
    user_id: Uuid,
    username: Arc<str>,
    email: Arc<str>,
    scopes: ScopeSet,
}

pub struct AppPasswordService {
    repo: Arc<AppPasswordPgRepository>,
    hasher: Arc<Argon2PasswordHasher>,
    user_repo: Arc<UserPgRepository>,
    base_url: String,
    /// `OXICLOUD_REQUIRE_VERIFIED_EMAIL`, snapshotted as the other
    /// auth services do. An app password mints no session but every
    /// request with one IS a login, so this door owes the same checks.
    require_verified_email: bool,

    /// In-memory cache of successful Basic Auth verifications.
    ///
    /// **Key**: `blake3(username + ":" + password)` — the plain-text password
    /// is never stored; only a cryptographic hash is kept as lookup key.
    ///
    /// **Value**: the authenticated identity plus the credential's SCOPE
    /// set — `user_id`, `username`, `email`, `scopes`. No `role`: see
    /// [`CachedBasicAuthResult`] for why that one is re-read live and the
    /// scopes are not.
    ///
    /// **Eviction**: TTL-based (see `BASIC_AUTH_CACHE_TTL_SECS`) +
    /// capacity-based (10 000 entries). Failed verifications are *never*
    /// cached, so brute-force attackers always pay the full Argon2id cost.
    ///
    /// **NOT evicted on revoke**, which is a gap rather than a decision:
    /// a revoked app password keeps authenticating for up to the TTL. The
    /// key is `blake3(username:password)` and `revoke` holds only the row
    /// id — the plaintext is never stored, so the key cannot be computed
    /// to target it. Closing it means either `invalidate_all()` on any
    /// revoke (one extra Argon2 for every live Basic client, on a rare
    /// human action) or a side index from app-password id to cache key.
    auth_cache: Cache<[u8; 32], CachedBasicAuthResult>,
}

impl AppPasswordService {
    pub fn new(
        repo: Arc<AppPasswordPgRepository>,
        hasher: Arc<Argon2PasswordHasher>,
        user_repo: Arc<UserPgRepository>,
        base_url: String,
        require_verified_email: bool,
    ) -> Self {
        let auth_cache = Cache::builder()
            .max_capacity(BASIC_AUTH_CACHE_MAX_ENTRIES)
            .time_to_live(StdDuration::from_secs(BASIC_AUTH_CACHE_TTL_SECS))
            .build();

        tracing::info!(
            "AppPasswordService Basic Auth cache initialized: TTL={}s, max={} entries",
            BASIC_AUTH_CACHE_TTL_SECS,
            BASIC_AUTH_CACHE_MAX_ENTRIES,
        );

        Self {
            repo,
            hasher,
            user_repo,
            base_url,
            auth_cache,
            require_verified_email,
        }
    }

    /// Generate a random app password token using cryptographic RNG.
    fn generate_token() -> String {
        use rand_core::{OsRng, RngCore};

        let charset: &[u8] = b"abcdefghijklmnopqrstuvwxyz\
                                ABCDEFGHIJKLMNOPQRSTUVWXYZ\
                                0123456789";
        let mut rng_bytes = [0u8; TOKEN_LENGTH];
        OsRng.fill_bytes(&mut rng_bytes);

        let random_part: String = rng_bytes
            .iter()
            .map(|&b| {
                let idx = (b as usize) % charset.len();
                charset[idx] as char
            })
            .collect();
        format!("{}{}", TOKEN_PREFIX, random_part)
    }

    /// Create a new app password for the given user.
    ///
    /// Returns the response DTO that includes the plain-text password (shown only once).
    pub async fn create(
        &self,
        user_id: Uuid,
        request: CreateAppPasswordRequestDto,
    ) -> Result<AppPasswordCreatedResponseDto, DomainError> {
        // Validate label
        let label = request.label.trim().to_string();
        if label.is_empty() || label.len() > 255 {
            return Err(DomainError::validation_error(
                "Label must be 1-255 characters",
            ));
        }

        // Fetch user for the username (needed for Basic Auth instructions)
        let user = self.user_repo.get_user_by_id(user_id).await?;
        let username = user.username().unwrap_or("").to_string();

        // Generate the plain-text token
        let plain_token = Self::generate_token();
        let prefix = plain_token[..TOKEN_PREFIX.len() + 8].to_string();

        // Hash the token for storage
        let password_hash = self.hasher.hash_password(&plain_token).await?;

        // Calculate expiration
        let expires_at = request
            .expires_in_days
            .map(|days| Utc::now() + Duration::days(days as i64));

        // Narrow at the STORE — see the note in `device_auth_service`.
        // The credential list reads this row back, so capping here makes
        // that output correct for free; the creation response below is
        // the one site that echoes rather than reads.
        let granted = CredentialKind::AppPassword
            .effective(Some(&request.scopes))
            .map_err(|e| {
                DomainError::new(
                    ErrorKind::InvalidInput,
                    "AppPassword",
                    format!("invalid scope: {e}"),
                )
            })?;

        // Create entity
        let app_password = AppPassword::new(
            user_id,
            label.clone(),
            password_hash,
            prefix.clone(),
            granted.to_wire(),
            expires_at,
        );

        let saved = self.repo.create(app_password).await?;

        let expires_str = saved.expires_at.map(|dt| dt.to_rfc3339());

        let curl_example = format!(
            "curl -u '{}:{}' -X PROPFIND {}/webdav/",
            username, plain_token, self.base_url
        );

        Ok(AppPasswordCreatedResponseDto {
            id: saved.id.to_string(),
            label,
            password: plain_token,
            username: username.clone(),
            // The EFFECTIVE set, not the request. RFC 6749 §5.1 makes
            // `scope` required in the response whenever it differs from
            // what was asked for — and echoing the request would tell a
            // client it holds `security` when it holds three areas,
            // which it would discover only as a 403 much later. Silent
            // narrowing is defensible; unreported narrowing is not.
            scopes: granted.to_wire(),
            expires_at: expires_str,
            instructions: AppPasswordInstructions {
                davx5: format!(
                    "In DAVx⁵, add account with base URL: {}/webdav/\n\
                     Username: {}\n\
                     Password: (the token shown above)",
                    self.base_url, username
                ),
                thunderbird: format!(
                    "In Thunderbird CalDAV/CardDAV:\n\
                     URL: {}/caldav/ or {}/carddav/\n\
                     Username: {}\n\
                     Password: (the token shown above)",
                    self.base_url, self.base_url, username
                ),
                rclone: format!(
                    "rclone config:\n\
                     type = webdav\n\
                     url = {}/webdav/\n\
                     vendor = other\n\
                     user = {}\n\
                     pass = (the token shown above, use 'rclone obscure' to encode)",
                    self.base_url, username
                ),
                curl_example,
            },
        })
    }

    /// List all app passwords for a user (excludes plain-text passwords).
    pub async fn list(&self, user_id: Uuid) -> Result<AppPasswordListResponseDto, DomainError> {
        let passwords = self.repo.list_by_user(user_id).await?;
        let total = passwords.len();

        let app_passwords = passwords
            .into_iter()
            .map(|ap| {
                let is_active = ap.active && !ap.is_expired();
                AppPasswordSummaryDto {
                    id: ap.id.to_string(),
                    label: ap.label,
                    prefix: format!("{}...", ap.prefix),
                    scopes: ap.scopes,
                    created_at: ap.created_at.to_rfc3339(),
                    last_used_at: ap.last_used_at.map(|dt| dt.to_rfc3339()),
                    expires_at: ap.expires_at.map(|dt| dt.to_rfc3339()),
                    active: is_active,
                }
            })
            .collect();

        Ok(AppPasswordListResponseDto {
            app_passwords,
            total,
        })
    }

    /// Revoke (soft-delete) an app password. Verifies ownership.
    ///
    /// Also invalidates **all** cached Basic Auth entries for the owning user
    /// so that the revocation takes effect immediately (instead of waiting
    /// Drop every cached Basic-auth verification.
    ///
    /// Called whenever a credential stops being valid: an app password
    /// revoked or deleted, or the account's password changed. Without it
    /// a revoked app password keeps authenticating for up to
    /// `BASIC_AUTH_CACHE_TTL_SECS` (300 s), because a cache hit never
    /// reaches the row that says `active = FALSE`. Revocation is the
    /// action a user takes when a credential LEAKS, so a five-minute
    /// grace is the wrong five minutes.
    ///
    /// **All of it, not one entry**, and that is forced rather than lazy:
    /// the key is `blake3(username:password)` over the PLAINTEXT, which
    /// the server never stores. Holding only a row id, there is no way to
    /// compute the key to target. The alternative is a side index from
    /// app-password id to cache key, which buys precision at the cost of
    /// a second structure to keep consistent — for an operation a human
    /// performs by hand, once.
    ///
    /// The cost is one extra Argon2id verification for each live Basic
    /// client, on their next request. Revocations are rare; wrongly
    /// authenticating a revoked credential is not a trade worth making
    /// for that.
    pub fn invalidate_auth_cache(&self) {
        self.auth_cache.invalidate_all();
        tracing::info!(
            target: "audit",
            event = "app_password.auth_cache_invalidated",
            reason = "credential_changed",
            "👮🏻‍♂️ Basic-auth cache dropped — a revoked or changed \
             credential must stop working now, not in 300 s",
        );
    }

    /// up to `BASIC_AUTH_CACHE_TTL_SECS`).
    pub async fn revoke(
        &self,
        user_id: Uuid,
        id: Uuid,
    ) -> Result<AppPasswordRevokeResponseDto, DomainError> {
        // Ownership enforced at SQL level (WHERE user_id = $2).
        // The get_by_id pre-check gives a clear error message when
        // the password doesn't belong to the caller.
        let ap = self.repo.get_by_id(id).await?;
        if ap.user_id != user_id {
            return Err(DomainError::unauthorized(
                "You can only revoke your own app passwords",
            ));
        }
        self.repo.revoke(id, user_id).await?;
        // The row now says `active = FALSE`, but a cache hit never reads
        // the row — so without this the credential keeps working for the
        // rest of its 300 s window.
        self.invalidate_auth_cache();

        // Invalidate all cached auth entries for this user so the
        // revocation is effective immediately.
        let uid = user_id;
        self.auth_cache
            .invalidate_entries_if(move |_key, val| val.user_id == uid)
            .ok();

        tracing::debug!(
            "Revoked app password {} — auth cache entries for user {} invalidated",
            id,
            user_id
        );

        Ok(AppPasswordRevokeResponseDto {
            status: "revoked".to_string(),
            id: id.to_string(),
        })
    }

    /// Verify username + app password for HTTP Basic Auth.
    ///
    /// Returns `(user_id, username, email, role)` on success.
    ///
    /// Handles both `oxicloud-` format and Nextcloud format (`XXXXX-XXXXX-...`)
    /// passwords. Uses prefix-based DB lookup to minimize Argon2id attempts.
    ///
    /// Successful verifications are cached for `BASIC_AUTH_CACHE_TTL_SECS`
    /// keyed by `blake3(username:password)`.  Failed verifications are
    /// **never** cached, preserving the full Argon2id cost as a brute-force
    /// deterrent.
    pub async fn verify_basic_auth(
        &self,
        username: &str,
        password: &str,
    ) -> Result<(Uuid, Arc<str>, Arc<str>, ScopeSet), DomainError> {
        // ── 1. Compute cache key = blake3("username:password") ────────
        // Stream the parts into an incremental hasher instead of
        // `blake3::hash(format!("{username}:{password}").as_bytes())` — the
        // `format!` heap-allocated one throw-away `String` per request (this
        // runs before the cache lookup, so even cache hits paid it), and DAV
        // sync clients hammer Basic auth on every request. Byte-identical key:
        // blake3 is a stream hash, so `hash(a || ":" || b)` == feeding the same
        // bytes in order (benches/ROUND19.md §M1).
        let cache_key: [u8; 32] = {
            let mut h = blake3::Hasher::new();
            h.update(username.as_bytes());
            h.update(b":");
            h.update(password.as_bytes());
            h.finalize().into()
        };

        // ── 2. Single-flight cache lookup ─────────────────────────────
        // Concurrent misses on the same credential coalesce into ONE
        // full verification: DAV sync clients hold 4-8 parallel
        // connections, so an expiring cache entry used to fan out into
        // K simultaneous Argon2id runs (~100-300 ms CPU + 64 MiB RAM
        // apiece) every TTL — a recurring p99 spike on every DAV
        // surface (8 -> 1 verifications, benches/AUTH-HERD.md).
        // `try_get_with` caches only `Ok` results, so failed
        // verifications are still never cached, preserving the full
        // Argon2id cost as a brute-force deterrent.
        let result = self
            .auth_cache
            .try_get_with(
                cache_key,
                self.verify_basic_auth_uncached(username, password),
            )
            .await
            .map_err(
                |e: std::sync::Arc<DomainError>| match std::sync::Arc::try_unwrap(e) {
                    Ok(err) => err,
                    // Another coalesced waiter still holds the Arc — rebuild
                    // an equivalent error (the source chain isn't clonable).
                    Err(shared) => {
                        DomainError::new(shared.kind, shared.entity_type, shared.message.clone())
                    }
                },
            )?;
        Ok((result.user_id, result.username, result.email, result.scopes))
    }

    /// The uncached Basic Auth slow path: user lookup, prefix-scoped
    /// candidate fetch, Argon2id verification. Runs at most once per
    /// credential per TTL — `verify_basic_auth` coalesces concurrent
    /// callers onto a single in-flight instance of this future.
    async fn verify_basic_auth_uncached(
        &self,
        username: &str,
        password: &str,
    ) -> Result<CachedBasicAuthResult, DomainError> {
        let user = self
            .user_repo
            .get_user_by_username(username)
            .await
            .map_err(|_| DomainError::unauthorized("Invalid username or app password"))?;

        if !user.is_active() {
            return Err(DomainError::unauthorized(
                "Invalid username or app password",
            ));
        }

        // Determine the password form and prefix for DB lookup.
        // oxicloud- format: use raw password, prefix = first 17 chars
        // NC format: normalize (strip dashes/whitespace, uppercase), prefix = first 8 chars
        let (verify_password, prefix) = if password.starts_with(TOKEN_PREFIX) {
            let pfx = password
                .get(..TOKEN_PREFIX.len() + 8)
                .unwrap_or(password)
                .to_string();
            (password.to_string(), pfx)
        } else {
            let norm = nc_normalize_password(password);
            match nc_token_prefix(&norm) {
                Ok(pfx) => (norm, pfx),
                Err(_) => {
                    return Err(DomainError::unauthorized(
                        "Invalid username or app password",
                    ));
                }
            }
        };

        // Use prefix-based lookup for efficiency (fewer Argon2id attempts)
        let candidates = self
            .repo
            .get_active_by_user_prefix(user.id(), &prefix)
            .await?;

        if candidates.is_empty() {
            return Err(DomainError::unauthorized(
                "Invalid username or app password",
            ));
        }

        for ap in &candidates {
            if let Ok(true) = self
                .hasher
                .verify_password(&verify_password, &ap.password_hash)
                .await
            {
                // The credential is proven, so run the full account
                // admission — the same rule every login door uses, so a
                // rule added to it reaches this door too. An app password
                // has no login phase of its own (every request is one),
                // which is exactly why this was the door that got
                // forgotten: the `is_active` pre-filter above looks like
                // the account check, and the email half was missing.
                //
                // The real reason goes to the audit log while the caller
                // still gets the generic "invalid username or app
                // password" below — the anti-enumeration pattern in
                // AGENTS.md. A DAV client could not act on "email not
                // verified" anyway.
                //
                // NOTE: the caller caches this result for
                // `BASIC_AUTH_CACHE_TTL`, so a deactivation or a policy
                // change takes effect within that window rather than
                // instantly — the same bound as the user-flags cache.
                if crate::application::services::auth_application_service::check_account_admission(
                    &user,
                    self.require_verified_email,
                    "app_password",
                )
                .is_err()
                {
                    return Err(DomainError::unauthorized(
                        "Invalid username or app password",
                    ));
                }

                let _ = self.repo.touch_last_used(ap.id).await;

                // Caching happens in `verify_basic_auth`: `try_get_with`
                // stores this value under the blake3 key on return.
                return Ok(CachedBasicAuthResult {
                    scopes: ScopeSet::from_credential(Some(&ap.scopes)),
                    user_id: user.id(),
                    username: Arc::from(user.username().unwrap_or("")),
                    email: Arc::from(user.email()),
                });
            }
        }

        Err(DomainError::unauthorized(
            "Invalid username or app password",
        ))
    }

    // ========================================================================
    // Nextcloud-format app password methods
    // ========================================================================

    /// Create a Nextcloud-format app password (`XXXXX-XXXXX-XXXXX-XXXXX-XXXXX`).
    ///
    /// Returns `(id, plain_password)`.
    pub async fn create_nc(
        &self,
        user_id: Uuid,
        label: &str,
    ) -> Result<(Uuid, String), DomainError> {
        let password = generate_nc_app_password();
        let normalized = nc_normalize_password(&password);
        let prefix = nc_token_prefix(&normalized)?;
        let hash = self.hasher.hash_password(&normalized).await?;

        let ap = AppPassword::new(
            user_id,
            label.to_string(),
            hash,
            prefix,
            "all".to_string(),
            None,
        );

        let saved = self.repo.create(ap).await?;
        Ok((saved.id, password))
    }

    /// Revoke an app password by matching the raw password value.
    /// Scoped to the authenticated user (fixes I3 — no global prefix search).
    pub async fn revoke_by_password(
        &self,
        user_id: Uuid,
        password: &str,
    ) -> Result<(), DomainError> {
        let normalized = nc_normalize_password(password);
        let prefix = match nc_token_prefix(&normalized) {
            Ok(pfx) => pfx,
            Err(_) => return Ok(()),
        };

        let candidates = self
            .repo
            .get_active_by_user_prefix(user_id, &prefix)
            .await?;

        for ap in candidates {
            if let Ok(true) = self
                .hasher
                .verify_password(&normalized, &ap.password_hash)
                .await
            {
                self.repo.revoke(ap.id, user_id).await?;
                self.invalidate_auth_cache();

                // Invalidate cache for this user
                let uid = user_id;
                self.auth_cache
                    .invalidate_entries_if(move |_key, val| val.user_id == uid)
                    .ok();
                break;
            }
        }

        Ok(())
    }

    /// List app passwords for a user (simple summary for NC UI).
    pub async fn list_nc(&self, user_id: Uuid) -> Result<Vec<AppPassword>, DomainError> {
        self.repo.list_by_user(user_id).await
    }

    /// Delete an app password by ID, scoped to the owning user.
    pub async fn delete_by_user(&self, id: Uuid, user_id: Uuid) -> Result<(), DomainError> {
        let deleted = self.repo.delete_by_user_and_id(id, user_id).await?;
        if deleted {
            // A DELETED row cannot be re-read at all, so a surviving
            // cache entry is strictly worse here than for a revoke.
            self.invalidate_auth_cache();
        }
        if !deleted {
            return Err(DomainError::new(
                ErrorKind::NotFound,
                "AppPassword",
                "App password not found",
            ));
        }
        Ok(())
    }
}

// ============================================================================
// Nextcloud app password helpers (module-private)
// ============================================================================

/// Generate a Nextcloud-format app password: `XXXXX-XXXXX-XXXXX-XXXXX-XXXXX`
/// using rejection sampling to avoid modulo bias.
fn generate_nc_app_password() -> String {
    let mut rng = rand_core::OsRng;
    let chars = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let len = chars.len() as u32; // 36
    let mut groups = Vec::with_capacity(NC_APP_PASSWORD_GROUPS);

    for _ in 0..NC_APP_PASSWORD_GROUPS {
        let mut group = String::with_capacity(NC_APP_PASSWORD_GROUP_LEN);
        for _ in 0..NC_APP_PASSWORD_GROUP_LEN {
            let threshold = u32::MAX - (u32::MAX % len);
            let idx = loop {
                let val = rng.next_u32();
                if val < threshold {
                    break (val % len) as usize;
                }
            };
            group.push(chars[idx] as char);
        }
        groups.push(group);
    }

    groups.join("-")
}

/// Normalize a Nextcloud-format password: strip dashes/whitespace, uppercase.
fn nc_normalize_password(password: &str) -> String {
    password
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

/// Extract the first 8 characters as the token prefix for DB lookup.
fn nc_token_prefix(normalized: &str) -> Result<String, DomainError> {
    if normalized.len() < NC_PREFIX_LEN {
        return Err(DomainError::new(
            ErrorKind::InvalidInput,
            "AppPassword",
            "App password too short",
        ));
    }
    Ok(normalized[..NC_PREFIX_LEN].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_nc_app_password_format() {
        let password = generate_nc_app_password();
        let groups: Vec<&str> = password.split('-').collect();
        assert_eq!(groups.len(), NC_APP_PASSWORD_GROUPS);
        for group in &groups {
            assert_eq!(group.len(), NC_APP_PASSWORD_GROUP_LEN);
            assert!(group.chars().all(|c| c.is_ascii_alphanumeric()));
        }
    }

    #[test]
    fn test_nc_normalize_password_strips_dashes_and_whitespace() {
        assert_eq!(
            nc_normalize_password("AB12C-DE34F-GH56I"),
            "AB12CDE34FGH56I"
        );
    }

    #[test]
    fn test_nc_normalize_password_uppercases() {
        assert_eq!(nc_normalize_password("abc-def"), "ABCDEF");
    }

    #[test]
    fn test_nc_token_prefix_extracts_first_8_chars() {
        assert_eq!(nc_token_prefix("ABCDEFGHIJKLMNOP").unwrap(), "ABCDEFGH");
    }

    #[test]
    fn test_nc_token_prefix_too_short() {
        assert!(nc_token_prefix("SHORT").is_err());
    }

    #[test]
    fn test_generated_nc_password_produces_valid_prefix() {
        let password = generate_nc_app_password();
        let normalized = nc_normalize_password(&password);
        let prefix = nc_token_prefix(&normalized);
        assert!(prefix.is_ok());
        assert_eq!(prefix.unwrap().len(), NC_PREFIX_LEN);
    }
}
