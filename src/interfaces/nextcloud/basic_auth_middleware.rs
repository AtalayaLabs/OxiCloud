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

/// Default-drive root folder id → `FolderDto`, 30 s TTL (benches/CHROOT-CACHE.md).
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

    // Canonicalise the whole Basic-Auth username to lowercase.
    //
    // Usernames are canonical (lowercase) in the DB post-migration
    // (`docs/plan/username-lowercase.md`), and NC / DAVX5 clients that
    // cached URLs from before the migration keep sending `Alice:pass`
    // — the server continues to accept that indefinitely by
    // lowercasing here.
    //
    // ASCII-only by `validate_username`'s charset check, so
    // `to_ascii_lowercase` is deterministic and locale-safe.
    let raw_username = raw_username.to_ascii_lowercase();

    let username: &str = raw_username.as_str();

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

            // ── Resolve chroot: the caller's default personal drive root ──
            use crate::application::ports::folder_ports::FolderUseCase;
            use crate::domain::repositories::drive_repository::DriveRepository;
            let chroot = match state
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
            };
            if chroot.is_none() {
                tracing::info!(
                    target: "audit",
                    event = "auth.nc_basic_rejected",
                    reason = "no_default_drive",
                    user_id = %user_id,
                    "👮🏻‍♂️ NC Basic auth rejected: default drive root not resolvable"
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
            tracing::info!(
                target: "audit",
                event = "auth.nc_basic_rejected",
                reason = "bad_credentials",
                username = %username,
                "👮🏻‍♂️ NC Basic auth rejected: bad credentials"
            );
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

    // Canonicalise the username to lowercase here too, so any caller
    // that reaches for `parse_basic_auth` directly (bypassing the
    // middleware wrapper) also sees the canonical form. Redundant with
    // the middleware's explicit `to_ascii_lowercase` on `raw_username`
    // — belt-and-braces to keep the invariant local to the parser too.
    // See `docs/plan/username-lowercase.md § 4. NextCloud DAV surface`.
    Some((user.to_ascii_lowercase(), pass.to_string()))
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
