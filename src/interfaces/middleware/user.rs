//! Caller-id-based user guards.
//!
//! All guards in this module take `(auth, caller_id) → Result<(), AppError>`
//! so handlers compose them uniformly as one-liners. They assume the
//! caller has already been authenticated by the
//! [`AuthUser`](super::auth::AuthUser) extractor, and pull the current
//! user flags via `AuthApplicationService::get_user_flags` — a
//! lightweight, short-TTL-cached lookup (no `image` column) — so role /
//! external-flag changes take effect within seconds without waiting
//! for token rotation, while the hot DAV paths stop paying one full-row
//! DB fetch per request.
//!
//! ```ignore
//! let caller_id = auth_user.id;
//! require_internal_user(&auth, caller_id).await?;
//! require_admin_user(&auth, caller_id).await?;
//! ```
//!
//! Future role-based guards (e.g. `require_active_user`) should follow
//! the same shape so they slot in next to these without ceremony.
//!
//! For the legacy header-based admin guard (`require_admin`), see
//! [`super::admin`] — that variant exists because some handlers take
//! `headers: HeaderMap` directly instead of `AuthUser`.

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use smol_str::SmolStr;
use std::sync::Arc;
use uuid::Uuid;

use crate::application::services::auth_application_service::AuthApplicationService;
use crate::common::di::AppState;
use crate::domain::entities::user::{UserFlags, UserRole};
use crate::domain::errors::{DomainError, ErrorKind};
use crate::interfaces::errors::AppError;
use crate::interfaces::middleware::auth::CurrentUser;

/// Require the caller to be an internal user. Returns `Ok(())` for
/// internal callers, `Err(403)` for externals.
///
/// External users authenticate via magic-link / OIDC-only / OCM and
/// exist solely to interact with resources they were explicitly
/// granted. They have no business enumerating the user directory, the
/// address book, subject groups, or any other instance-wide listing —
/// this guard locks them out of those surfaces.
///
/// DB lookup errors fall back to `Ok(())` so a transient outage doesn't
/// lock everyone out — this guard is defense in depth. The canonical
/// filter is at the service / repository layer (`include_external =
/// false` on `list_users`, the visibility rule in `get_user_profile`,
/// etc.); this helper just opts a surface in to "internal only" with
/// one extra line.
///
/// The 403 status is honest (not 404 stealth) because the caller's own
/// `is_external` flag is not a secret to themselves — the UI already
/// surfaces "you came in through a magic link".
pub async fn require_internal_user(
    auth: &AuthApplicationService,
    caller_id: Uuid,
) -> Result<(), AppError> {
    match auth.get_user_flags(caller_id).await {
        Ok(flags) if flags.is_external => Err(AppError::new(
            StatusCode::FORBIDDEN,
            "External users cannot access this endpoint",
            "Forbidden",
        )),
        _ => Ok(()),
    }
}

/// Require the caller to hold the admin role. Returns `Ok(())` for
/// admins, `Err(403)` otherwise.
///
/// The check pulls the role from the user record (not from JWT
/// claims) so a role change takes effect within the flags-cache TTL —
/// or immediately when changed through `change_user_role`, which
/// invalidates the entry — without waiting for token rotation. Mirrors
/// [`require_internal_user`]'s shape so handlers compose either of
/// them as a one-liner via `?`.
///
/// Use this in handlers that already have an
/// [`AuthUser`](super::auth::AuthUser) extractor (and thus a validated
/// `caller_id`); use the legacy [`super::admin::require_admin`] variant
/// when the handler signature is `headers: HeaderMap` instead.
pub async fn require_admin_user(
    auth: &AuthApplicationService,
    caller_id: Uuid,
) -> Result<(), AppError> {
    let flags = auth
        .get_user_flags(caller_id)
        .await
        .map_err(AppError::from)?;

    if flags.role != UserRole::Admin {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "Admin access required",
            "Forbidden",
        ));
    }
    Ok(())
}

/// Outcome of re-checking a token-authenticated caller against the live
/// user record (see [`resolve_live_role`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveRole {
    /// The account exists and is active. Carries the caller's *current*
    /// role string (`"admin"` / `"user"`), which is authoritative and
    /// supersedes the — possibly stale — JWT `role` claim. `SmolStr` so the
    /// per-request render of the (≤23-byte) role never heap-allocates.
    Active(SmolStr),
    /// The account is deactivated or deleted: the request must be rejected
    /// even though its token is still cryptographically valid.
    Revoked,
}

/// Re-validate a caller carried by a still-valid token against the live
/// user record, so deactivation, deletion and role changes take effect
/// within [`USER_FLAGS_CACHE_TTL`](crate::application::services::auth_application_service)
/// instead of waiting for the token to expire (access 1 h / refresh 7 d by
/// default).
///
/// JWT claims — `role` included — are frozen at login. Without this check a
/// demoted admin keeps admin power, and a disabled or deleted account keeps
/// full access, until its token expires. Returning the *current* role lets
/// every caller stop trusting `claims.role`.
///
/// Cost: the short-TTL-cached `get_user_flags` (no `image` column), so ~one
/// tiny indexed query per user per cache-TTL window; admin role/active
/// changes invalidate the entry eagerly for immediate effect.
///
/// Availability stance mirrors [`require_internal_user`]: a *transient*
/// lookup failure fails OPEN with the claim role (a DB blip must not lock
/// every authenticated user out, and login/refresh already enforce `active`
/// at the canonical layer). A *missing* row (`NotFound`) is a definitive
/// revocation and fails CLOSED.
pub async fn resolve_live_role(
    auth: &AuthApplicationService,
    user_id: Uuid,
    claim_role: &str,
) -> LiveRole {
    decide_live_role(auth.get_user_flags(user_id).await, user_id, claim_role)
}

/// Pure decision core of [`resolve_live_role`], split out so the
/// allow/revoke/fail-open policy is unit-testable without a service or DB.
fn decide_live_role(
    flags: Result<UserFlags, DomainError>,
    user_id: Uuid,
    claim_role: &str,
) -> LiveRole {
    match flags {
        Ok(flags) if flags.active => LiveRole::Active(SmolStr::new_static(flags.role.as_str())),
        Ok(_) => {
            audit_token_revoked(user_id, "deactivated");
            LiveRole::Revoked
        }
        // The user row is gone — a definitive revocation; fail closed.
        Err(e) if matches!(e.kind, ErrorKind::NotFound) => {
            audit_token_revoked(user_id, "deleted");
            LiveRole::Revoked
        }
        // Transient lookup failure (DB blip): fail open on the claim role so
        // a momentary outage doesn't 401 every authenticated user at once.
        Err(e) => {
            tracing::warn!(
                user_id = %user_id,
                error = %e,
                "live-user re-check failed transiently; allowing request on the JWT claim role (fail-open)"
            );
            LiveRole::Active(SmolStr::new(claim_role))
        }
    }
}

/// Audit a request rejected because the token outlived the account's access
/// (deactivation or deletion). Anti-enumeration is not a concern — the
/// subject is the caller's own account.
fn audit_token_revoked(user_id: Uuid, reason: &'static str) {
    tracing::info!(
        target: "audit",
        event = "auth.token_revoked",
        reason = reason,
        caller_id = %user_id,
        "👮🏻‍♂️ valid token presented for an account that is no longer active — rejected"
    );
}

/// Axum middleware layer that blocks external users from a whole route
/// subtree. Apply via `.layer(from_fn_with_state(state, require_internal_user_layer))`
/// on the protocol nests (CalDAV / CardDAV / WebDAV) that have no
/// semantic meaning for externals — they own no calendars, no address
/// books, no home folder.
///
/// Must run AFTER the auth middleware so `CurrentUser` is in the
/// request extensions; in tower order that means the auth layer is
/// added LAST (outermost). If the layer fires on an unauthenticated
/// path (no `CurrentUser` populated), it simply passes through — the
/// inner handler is then responsible for the 401, and we don't blanket-
/// 403 traffic the auth layer would have rejected anyway.
///
/// Emits an `authz.external_user_blocked` audit event on rejection so
/// operators can spot which surfaces externals are probing.
pub async fn require_internal_user_layer(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let caller_id = request
        .extensions()
        .get::<Arc<CurrentUser>>()
        .map(|cu| cu.id);

    let (Some(caller_id), Some(svc)) = (
        caller_id,
        state
            .auth_service
            .as_ref()
            .map(|s| &*s.auth_application_service),
    ) else {
        // No auth populated, or auth disabled globally — pass through.
        return next.run(request).await;
    };

    if let Err(err) = require_internal_user(svc, caller_id).await {
        let path = request.uri().path().to_owned();
        tracing::info!(
            target: "audit",
            event = "authz.external_user_blocked",
            reason = "internal_only_surface",
            caller_id = %caller_id,
            path = %path,
            "👮🏻‍♂️ External user blocked from internal-only route subtree"
        );
        return err.into_response();
    }

    next.run(request).await
}

/// Endpoints the gate lets through even when
/// `force_password_change_at_next_login` is TRUE — the caller needs
/// them to complete the mandatory reset:
///
///   * `GET  /api/auth/me`             — the SPA must be able to read
///     the flag (that's what tells it to enter mandatory-mode).
///   * `PUT  /api/auth/change-password` — the way OUT of the state.
///   * `POST /api/auth/logout`         — bailing out is always allowed.
///
/// `/api/auth/refresh` is not on this list because refresh is mounted
/// on a rate-limited public path that doesn't carry a `CurrentUser` at
/// middleware time; the gate never fires on it. If refresh ever moves
/// under the gate, add `(&Method::POST, "/api/auth/refresh")` here.
fn is_password_change_pending_allowlisted(
    method: &axum::http::Method,
    path: &str,
) -> bool {
    use axum::http::Method;
    matches!(
        (method, path),
        (&Method::GET, "/api/auth/me")
            | (&Method::PUT, "/api/auth/change-password")
            | (&Method::POST, "/api/auth/logout")
    )
}

/// Axum middleware layer that blocks EVERY authenticated request when
/// the caller's `force_password_change_at_next_login` flag is TRUE —
/// EXCEPT the small allowlist above ([`is_password_change_pending_allowlisted`]).
/// Mounted on all authenticated `/api/*` subtrees so an admin-set
/// temp password cannot be used to hit files / WebDAV / CalDAV / etc.
/// via any non-SPA client.
///
/// The flag is read from the cached `UserFlags` (same cache the role /
/// external guards use — see [`require_internal_user`]), so this adds
/// no DB round-trip on the hot path. `admin_reset_password` and
/// `change_password` both invalidate the entry eagerly so the gate
/// lifts within one request round-trip.
///
/// Response shape on refusal: `403 { error_type: "PasswordChangeRequired" }`.
/// The SPA reads that error_type on any subsequent request that leaks
/// past its own nav guard (mid-navigation refresh, stale tab, …) and
/// bounces the user back to the change-password screen. Non-SPA
/// clients (WebDAV sync, mobile app, curl) get the same 403 — that's
/// intentional; they need to log in via the SPA once to complete the
/// reset before other clients work again.
///
/// Must run AFTER the auth middleware. On unauthenticated paths (no
/// `CurrentUser` populated) this is a pass-through — the inner
/// handler / auth layer will produce the 401.
pub async fn require_no_password_change_pending_layer(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    // Cheap path check FIRST — allowlisted endpoints never even hit
    // the flag lookup. Keeps the /me polling path (which the SPA hits
    // as part of every session-probe) from doing the cache lookup on
    // every call, and makes the allowlist trivially auditable in one
    // place (see `is_password_change_pending_allowlisted`).
    //
    // MUST use `OriginalUri` — axum's `.nest("/api/auth", …)` strips
    // the prefix so `request.uri().path()` returns `/me` inside the
    // nested router, not `/api/auth/me`. The allowlist is defined
    // against the operator-visible full URL, so we need the pre-strip
    // path. `OriginalUri` is set on the request extensions by axum
    // whenever a nest strips a prefix; falls back to the current path
    // when this middleware is layered on a top-level (non-nested)
    // router (defense in depth).
    let full_path = request
        .extensions()
        .get::<axum::extract::OriginalUri>()
        .map(|uri| uri.0.path().to_owned())
        .unwrap_or_else(|| request.uri().path().to_owned());
    if is_password_change_pending_allowlisted(request.method(), &full_path) {
        return next.run(request).await;
    }

    let caller_id = request
        .extensions()
        .get::<Arc<CurrentUser>>()
        .map(|cu| cu.id);

    let (Some(caller_id), Some(svc)) = (
        caller_id,
        state
            .auth_service
            .as_ref()
            .map(|s| &*s.auth_application_service),
    ) else {
        return next.run(request).await;
    };

    // Cached lookup — no DB hit on the hot path. Fail-open on repo
    // error (the same posture as require_internal_user_layer above):
    // a transient DB blip must not lock every user out of every API,
    // and the SPA-side nav guard is a defense-in-depth backstop.
    let flag = match svc.get_user_flags(caller_id).await {
        Ok(f) => f.force_password_change,
        Err(_) => false,
    };
    if flag {
        // Log the operator-visible full path (not the nest-stripped
        // one). `full_path` was computed above via `OriginalUri`.
        tracing::info!(
            target: "audit",
            event = "auth.password_change_required_blocked",
            reason = "force_password_change_pending",
            caller_id = %caller_id,
            path = %full_path,
            "👮🏻‍♂️ Blocked API access — user must change admin-set temp password first"
        );
        return AppError::new(
            StatusCode::FORBIDDEN,
            "Password change required before accessing this endpoint",
            "PasswordChangeRequired",
        )
        .into_response();
    }

    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags(role: UserRole, active: bool) -> UserFlags {
        UserFlags {
            role,
            is_external: false,
            active,
            force_password_change: false,
        }
    }

    #[test]
    fn active_admin_yields_current_admin_role() {
        let live = decide_live_role(Ok(flags(UserRole::Admin, true)), Uuid::nil(), "user");
        // The live record wins over the (stale) claim — a freshly promoted
        // user is admin even though their token still says "user".
        assert_eq!(live, LiveRole::Active(SmolStr::new_static("admin")));
    }

    #[test]
    fn active_user_yields_current_user_role() {
        // A demoted admin: token claim still "admin", live record "user".
        let live = decide_live_role(Ok(flags(UserRole::User, true)), Uuid::nil(), "admin");
        assert_eq!(live, LiveRole::Active(SmolStr::new_static("user")));
    }

    #[test]
    fn deactivated_account_is_revoked() {
        let live = decide_live_role(Ok(flags(UserRole::Admin, false)), Uuid::nil(), "admin");
        assert_eq!(live, LiveRole::Revoked);
    }

    #[test]
    fn deleted_account_not_found_is_revoked() {
        let err = DomainError::new(ErrorKind::NotFound, "User", "no such user");
        let live = decide_live_role(Err(err), Uuid::nil(), "admin");
        assert_eq!(live, LiveRole::Revoked);
    }

    #[test]
    fn transient_error_fails_open_on_claim_role() {
        // A DB blip must not lock everyone out: allow on the claim role.
        let err = DomainError::new(ErrorKind::InternalError, "User", "connection reset");
        let live = decide_live_role(Err(err), Uuid::nil(), "admin");
        assert_eq!(live, LiveRole::Active(SmolStr::new_static("admin")));
    }
}
