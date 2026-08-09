use axum::{
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use base64::Engine;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use crate::application::dtos::folder_dto::FolderDto;
use crate::common::di::AppState;
use crate::interfaces::middleware::auth::CurrentUser;

/// Markerless-chroot cache: default-drive root folder id → `FolderDto`.
///
/// This middleware wraps EVERY protected NextCloud route (DAV files,
/// per-chunk uploads, trashbin, previews, avatars, OCS polls). With the
/// app-password verification already cached, the chroot resolution was the
/// last per-request DB work: `find_default_for_user` (now cached in
/// `DrivePgRepository`) plus this folder-by-PK fetch. A desktop sync run
/// issues hundreds of these per minute for a value that changes only on a
/// root-folder rename — the 30 s TTL bounds that staleness (mirrors
/// `drive_role_cache` / the default-drive cache; measured in
/// `benches/CHROOT-CACHE.md`).
///
/// Only the MARKERLESS branch is cached: it targets the caller's own
/// default drive root, so no per-request authorization decision is being
/// skipped. The drive-marker branch keeps its `get_folder_with_perms`
/// check on every request.
// `Arc<FolderDto>` values: a hit hands back a refcount bump instead of a
// deep clone of the DTO's ~5 owned Strings (moka's `get` clones `V`), and
// the same `Arc` then rides inside `NcSession` for the whole request.
static NC_CHROOT_CACHE: LazyLock<moka::sync::Cache<uuid::Uuid, Arc<FolderDto>>> =
    LazyLock::new(|| {
        moka::sync::Cache::builder()
            .max_capacity(100_000)
            .time_to_live(Duration::from_secs(30))
            .build()
    });

#[derive(Debug, thiserror::Error)]
pub enum NextcloudAuthError {
    #[error("Unauthorized")]
    Unauthorized,
    #[error("Nextcloud services unavailable")]
    ServiceUnavailable,
    #[error("Internal error: {0}")]
    Internal(String),
}

impl IntoResponse for NextcloudAuthError {
    fn into_response(self) -> Response {
        match self {
            NextcloudAuthError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, "Basic realm=\"OxiCloud\"")],
                "Unauthorized",
            )
                .into_response(),
            NextcloudAuthError::ServiceUnavailable => {
                (StatusCode::SERVICE_UNAVAILABLE, "Nextcloud unavailable").into_response()
            }
            NextcloudAuthError::Internal(_) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response()
            }
        }
    }
}

pub async fn basic_auth_middleware(
    State(state): State<Arc<AppState>>,
    mut request: Request,
    next: Next,
) -> Result<Response, NextcloudAuthError> {
    tracing::debug!("[NC] {} {}", request.method(), request.uri());

    // Borrow the Authorization header directly rather than cloning the whole
    // HeaderMap per NC sync request; the borrow ends at `parse_basic_auth`
    // below, before any request mutation (benches/ROUND14.md §A4).
    let auth_header = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| {
            tracing::warn!(
                "[NC] 401 no auth header: {} {}",
                request.method(),
                request.uri()
            );
            NextcloudAuthError::Unauthorized
        })?;

    let (raw_username, password) =
        parse_basic_auth(auth_header).ok_or(NextcloudAuthError::Unauthorized)?;

    // ── Multi-drive composite-username parse ────────────────────────
    // POC wire shape: `{username}~{drive_marker}` may appear in the
    // Basic Auth header. `~` was chosen because it needs no URL
    // encoding and doesn't collide with UUID hyphens. The marker
    // after `~` is a chroot SELECTOR (handled by `NcSession` via the
    // URL `{user}` segment), NOT an auth credential — the password
    // is verified against the username PREFIX. The middleware just
    // peels the prefix off so the app-password lookup uses the
    // canonical name. When no `~` is present, the request is a
    // plain single-drive ("home") NC sync.
    //
    // Reject `name~` (empty marker) and `~marker` (empty username)
    // at the auth boundary rather than treating them as "missing
    // marker" — they are unambiguous typos that would otherwise
    // silently fall into a different code path.
    // Borrow the prefix / marker out of the already-owned `raw_username`
    // (`split_once` yields `&str` slices) instead of allocating a duplicate
    // `String` per request — `username` is only ever passed by reference, and
    // `raw_username` outlives every use before it moves into `NcSession`
    // (benches/ROUND29.md §E).
    let (username, drive_marker): (&str, Option<&str>) = match raw_username.split_once('~') {
        Some(("", _)) => {
            tracing::warn!(
                "[NC] 401 malformed composite username (empty prefix): {}",
                raw_username
            );
            return Err(NextcloudAuthError::Unauthorized);
        }
        Some((_, "")) => {
            tracing::warn!(
                "[NC] 401 malformed composite username (empty marker): {}",
                raw_username
            );
            return Err(NextcloudAuthError::Unauthorized);
        }
        Some((u, m)) => (u, Some(m)),
        None => (raw_username.as_str(), None),
    };

    // Check account lockout before attempting password verification (saves CPU).
    // The lockout is per (account, IP), see #323 for rationale.
    let client_ip = crate::interfaces::middleware::rate_limit::extract_client_ip(&request);
    if let Some(auth_svc) = state.auth_service.as_ref()
        && let Err(secs) = auth_svc.login_lockout.check(username, &client_ip)
    {
        tracing::warn!(
            username = %username,
            client_ip = %client_ip,
            lockout_remaining_secs = secs,
            "[NC] Account locked, too many failed attempts from this IP"
        );
        return Err(NextcloudAuthError::Unauthorized);
    }

    let nextcloud = state
        .nextcloud
        .as_ref()
        .ok_or(NextcloudAuthError::ServiceUnavailable)?;

    match nextcloud
        .app_passwords
        .verify_basic_auth(username, &password)
        .await
    {
        Ok((user_id, uname, email, role)) => {
            // Reset lockout counter on success
            if let Some(auth_svc) = state.auth_service.as_ref() {
                auth_svc.login_lockout.record_success(username, &client_ip);
            }
            // External users must never authenticate against the NC
            // surface — that whole subtree (WebDAV files, uploads,
            // trashbin, OCS user info, sharees autocomplete, etc.) has
            // no semantic meaning for a magic-link-only principal, and
            // an app password would be a persistent credential
            // bypassing the magic-link-eligibility rule. POST
            // /api/auth/app-passwords also gates externals upfront;
            // this is the belt-and-braces check in case one slipped
            // through (e.g. user later flipped to is_external).
            if let Some(auth_svc) = state.auth_service.as_ref()
                && let Ok(flags) = auth_svc
                    .auth_application_service
                    .get_user_flags(user_id)
                    .await
                && flags.is_external
            {
                tracing::info!(
                    target: "audit",
                    event = "auth.nc_basic_rejected",
                    reason = "external_user",
                    user_id = %user_id,
                    "👮🏻‍♂️ External user attempted NC Basic auth — rejected"
                );
                return Err(NextcloudAuthError::Unauthorized);
            }
            // Populate the deferred `user_id` field on the request
            // tracing span (declared in `middleware/trace_span.rs::ClientIpMakeSpan`).
            // Mirrors what `interfaces/middleware/auth.rs` does for the
            // JWT path so the two auth surfaces produce log lines with
            // the same structured shape — without this, every NC
            // request would appear in the logs with `user_id=-`,
            // making it harder to correlate WebDAV / OCS activity to
            // a specific principal.
            // `field::display` renders lazily into the subscriber's buffer —
            // no per-request `to_string` (mirrors the JWT path since ROUND5).
            tracing::Span::current().record("user_id", tracing::field::display(user_id));
            // One shared identity: the same `Arc` serves the
            // `Arc<CurrentUser>` extension AND `NcSession.user` (the old
            // code built the struct, cloned it for the extension, then
            // moved the original — 2-3 String allocs per request).
            // Nextcloud clients are always unbound — they authenticate
            // with app passwords via Basic Auth, no WebCrypto, no DPoP.
            // Middleware exempts unbound sessions per Gate 9 design.
            let current_user = Arc::new(CurrentUser {
                id: user_id,
                username: uname,
                email,
                role,
                dpop_jkt: None,
            });

            // ── Resolve chroot from the Basic Auth drive marker ─────
            // No marker → caller's default personal drive's root folder
            // (post-D0 every internal user has one — provisioned by the
            // lifecycle hook via the atomic four-write transaction in
            // §3 of docs/plan/drive.md). With a marker →
            // `get_folder_with_perms` enforces per-folder access (404
            // anti-enumeration on miss / no-read). Today this is the
            // sole chroot source; tomorrow it'll come from the
            // app-password row instead.
            //
            // Pre-D0 this lookup name-matched `"My Folder - <username>"`
            // against the user's root folders; that broke after the
            // wrapper was renamed to `"Personal"` and shared across all
            // users — name-matching was the wrong axis. The drive lookup
            // is the right one: name-independent, secondary-drive-safe.
            use crate::application::ports::folder_ports::FolderUseCase;
            use crate::domain::repositories::drive_repository::DriveRepository;
            let chroot = match drive_marker {
                None => {
                    match state
                        .drive_repo
                        .find_default_for_user(current_user.id)
                        .await
                    {
                        Ok(drive_with_name) => {
                            let root_id = drive_with_name.drive.root_folder_id;
                            match NC_CHROOT_CACHE.get(&root_id) {
                                Some(cached) => Some(cached),
                                None => {
                                    let fetched = state
                                        .applications
                                        .folder_service
                                        .get_folder(&root_id.to_string())
                                        .await
                                        .ok()
                                        .map(Arc::new);
                                    if let Some(f) = &fetched {
                                        NC_CHROOT_CACHE.insert(root_id, Arc::clone(f));
                                    }
                                    fetched
                                }
                            }
                        }
                        Err(_) => None,
                    }
                }
                Some(folder_id) => state
                    .applications
                    .folder_service
                    .get_folder_with_perms(folder_id, current_user.id)
                    .await
                    .ok()
                    .map(Arc::new),
            };
            if chroot.is_none() {
                tracing::warn!(
                    "[NC] 404 chroot not resolvable: user={} marker={:?}",
                    current_user.username,
                    drive_marker
                );
                return Err(NextcloudAuthError::Unauthorized);
            }

            // Record from the local before it moves into the session —
            // the old code re-read the just-inserted extension and paid a
            // `to_string` for the span value.
            if let Some(c) = &chroot {
                tracing::Span::current().record("chroot_id", tracing::field::display(&c.id));
            }
            request.extensions_mut().insert(Arc::clone(&current_user));
            request.extensions_mut().insert(Arc::new(
                crate::interfaces::nextcloud::session::NcSession {
                    user: current_user,
                    raw_username,
                    chroot,
                },
            ));
            Ok(next.run(request).await)
        }
        Err(_) => {
            // Record failed attempt for lockout tracking
            if let Some(auth_svc) = state.auth_service.as_ref() {
                auth_svc.login_lockout.record_failure(username, &client_ip);
            }
            Err(NextcloudAuthError::Unauthorized)
        }
    }
}

/// Parse a `Basic` Authorization header into `(username, password)`.
pub fn parse_basic_auth(header_value: &str) -> Option<(String, String)> {
    let mut parts = header_value.splitn(2, ' ');
    let scheme = parts.next()?.trim();
    let encoded = parts.next()?.trim();

    if !scheme.eq_ignore_ascii_case("Basic") {
        return None;
    }

    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    let (user, pass) = decoded.split_once(':')?;

    Some((user.to_string(), pass.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_valid_basic_auth() {
        let encoded = base64::engine::general_purpose::STANDARD.encode("alice:secret123");
        let header = format!("Basic {}", encoded);
        let (user, pass) = parse_basic_auth(&header).expect("should parse");
        assert_eq!(user, "alice");
        assert_eq!(pass, "secret123");
    }

    #[test]
    fn test_parse_basic_auth_with_colon_in_password() {
        let encoded = base64::engine::general_purpose::STANDARD.encode("user:pass:with:colons");
        let header = format!("Basic {}", encoded);
        let (user, pass) = parse_basic_auth(&header).expect("should parse");
        assert_eq!(user, "user");
        assert_eq!(pass, "pass:with:colons");
    }

    #[test]
    fn test_parse_basic_auth_bearer_scheme_rejected() {
        let encoded = base64::engine::general_purpose::STANDARD.encode("user:pass");
        let header = format!("Bearer {}", encoded);
        assert!(parse_basic_auth(&header).is_none());
    }

    #[test]
    fn test_parse_basic_auth_missing_colon() {
        let encoded = base64::engine::general_purpose::STANDARD.encode("nocolon");
        let header = format!("Basic {}", encoded);
        assert!(parse_basic_auth(&header).is_none());
    }

    #[test]
    fn test_parse_basic_auth_invalid_base64() {
        assert!(parse_basic_auth("Basic not-valid-base64!!!").is_none());
    }

    #[test]
    fn test_parse_basic_auth_case_insensitive_scheme() {
        let encoded = base64::engine::general_purpose::STANDARD.encode("user:pass");
        let header = format!("BASIC {}", encoded);
        let result = parse_basic_auth(&header);
        assert!(result.is_some());
    }
}
