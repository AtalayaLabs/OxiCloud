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
    decide_internal_user(auth.get_user_flags(caller_id).await, caller_id)
}

/// Pure decision half of [`require_internal_user`].
///
/// Split out for the same reason as [`decide_live_role`] below: the policy
/// is worth unit-testing without a live auth service, and this gate guards
/// all three DAV surfaces.
fn decide_internal_user(
    flags: Result<UserFlags, DomainError>,
    caller_id: Uuid,
) -> Result<(), AppError> {
    match flags {
        Ok(flags) if flags.is_external => Err(AppError::new(
            StatusCode::FORBIDDEN,
            "External users cannot access this endpoint",
            "Forbidden",
        )),
        Ok(_) => Ok(()),
        // These three arms were once a blanket `_ => Ok(())`, on the only
        // middleware guarding `/webdav`, `/caldav` and `/carddav` — so any
        // lookup failure admitted the caller, and a principal with no
        // `auth.users` row (an anonymous share session) walked straight
        // through the "internal users only" gate.
        //
        // `NotFound` is a denial: a definitive answer, and the only error
        // this gate may act on. The rest split on whether a retry could
        // change the answer, because admitting and denying BOTH answer a
        // question the lookup could not. 503 says "ask again", which is
        // what is true; 403 would make an SPA show a permissions error or
        // bounce to login instead of retrying.
        //
        // Failing closed costs little availability here: during a database
        // outage the request was going to fail at its next query anyway,
        // so admitting only let a caller into a system that could not
        // serve them.
        Err(e) if matches!(e.kind, ErrorKind::NotFound) => {
            tracing::info!(
                target: "audit",
                event = "authz.denied",
                reason = "no_user_row",
                caller_id = %caller_id,
                "👮🏻‍♂️ principal has no user record — refused an internal-user endpoint",
            );
            Err(AppError::new(
                StatusCode::FORBIDDEN,
                "This endpoint requires a user account",
                "Forbidden",
            ))
        }
        Err(e) if e.is_transient() => {
            tracing::warn!(
                target: "audit",
                event = "authz.evaluation_failed",
                reason = "transient_backend",
                caller_id = %caller_id,
                error = %e,
                "👮🏻‍♂️ internal-user gate could not evaluate — answered 503, neither admitted nor denied",
            );
            Err(indeterminate_app_error(true))
        }
        // Not transient and not `NotFound`: a decode failure, a schema
        // mismatch — a bug rather than a blip, so retrying will not help
        // and saying "retry" would be a lie. 500 and an audit line.
        Err(e) => {
            tracing::error!(
                target: "audit",
                event = "authz.evaluation_failed",
                reason = "lookup_failed",
                caller_id = %caller_id,
                error = %e,
                "👮🏻‍♂️ internal-user gate could not evaluate — permanent lookup failure",
            );
            Err(indeterminate_app_error(false))
        }
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

    if !flags.role.at_least(UserRole::Admin) {
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
    /// The live record could not be read, so the caller's role is
    /// **unknown** — which is not the same as revoked, and the
    /// distinction is load-bearing at two call sites. The cookie branch
    /// of the auth middleware turns `Revoked` into a 401 and a redirect
    /// to `/login`; folding "cannot tell" into it would log every
    /// browser user out during a database blip. The old behaviour here
    /// was the opposite mistake — fail OPEN on the frozen JWT claim
    /// role, so a demoted admin kept admin power for as long as the
    /// lookup kept failing.
    ///
    /// `retryable` separates a blip (answer 503, ask again) from a
    /// permanent lookup failure (answer 500). Telling a client to retry
    /// a schema mismatch is a lie it acts on, which is why this is a
    /// field rather than one collapsed "unavailable" case.
    Indeterminate { retryable: bool },
}

/// The HTTP answer for a [`LiveRole::Indeterminate`] outcome.
///
/// One home for the choice, because four call sites face it and a
/// status code picked four times gets picked differently the fourth
/// time. `503` carries `Retry-After` semantics for the SPA's fetch
/// interceptor; `500` tells it not to bother.
pub fn indeterminate_app_error(retryable: bool) -> AppError {
    if retryable {
        AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "Temporarily unable to verify your account; please retry",
            "ServiceUnavailable",
        )
    } else {
        AppError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Unable to verify your account",
            "InternalError",
        )
    }
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
    if let Some(live) = anonymous_live_role(claim_role) {
        return live;
    }
    decide_live_role(auth.get_user_flags(user_id).await, user_id)
}

/// `Some` when the claim is an anonymous public-share session, which must
/// skip the live re-check entirely.
///
/// Such a session has no `auth.users` row by design, so the re-check would
/// look one up, find nothing, and report the token as revoked — every share
/// request 401ing.
///
/// Skipping is safe because the re-check exists to catch deactivation,
/// deletion and DEMOTION, and none applies here: there is no account to
/// deactivate, and `anonymous` is already the floor of the role order, so a
/// stale claim cannot be an over-privileged one. The claim itself is inside
/// a signed JWT, so it cannot be forged — and forging it would only ever
/// *reduce* what the bearer can reach.
///
/// The staleness this leaves is bounded elsewhere — by the session's own
/// short TTL and by share revocation — not by the user-flags cache.
fn anonymous_live_role(claim_role: &str) -> Option<LiveRole> {
    UserRole::from_session(claim_role)
        .filter(|r| r.is_anonymous())
        .map(|r| LiveRole::Active(SmolStr::new_static(r.as_str())))
}

/// Pure decision core of [`resolve_live_role`], split out so the
/// allow/revoke/indeterminate policy is unit-testable without a service
/// or DB.
///
/// Takes no claim role: the live record is the only authority here. The
/// parameter used to exist solely to be returned when the lookup failed
/// — the fail-open path — and removing it is what makes a frozen claim
/// unable to influence the outcome at all. `resolve_live_role` still
/// reads the claim, but only to spot an anonymous share session that has
/// no user row to re-check.
fn decide_live_role(flags: Result<UserFlags, DomainError>, user_id: Uuid) -> LiveRole {
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
        // The lookup failed, so the role is unknown. This arm used to
        // fail OPEN on `claim_role` — the frozen JWT claim — which meant a
        // demoted admin kept admin power for as long as the lookup kept
        // failing, and the one thing this re-check exists to catch was
        // the one thing it stopped catching under load.
        //
        // Reporting "unknown" rather than guessing lets each caller answer
        // honestly: 503 for a blip, 500 for a permanent failure, and — at
        // the cookie branch — neither of those is a 401, so a blip no
        // longer logs every browser session out.
        Err(e) => {
            let retryable = e.is_transient();
            tracing::warn!(
                target: "audit",
                event = "auth.live_role_indeterminate",
                reason = if retryable { "transient_backend" } else { "lookup_failed" },
                caller_id = %user_id,
                error = %e,
                "👮🏻‍♂️ live-role re-check could not evaluate — neither honoured nor revoked",
            );
            LiveRole::Indeterminate { retryable }
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
fn is_password_change_pending_allowlisted(method: &axum::http::Method, path: &str) -> bool {
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

    let current_user = request.extensions().get::<Arc<CurrentUser>>();

    // An anonymous public-share visitor has no account, so no password can be
    // pending a forced change. Skipping is not only correctness: without it
    // every such request looks up a `visitor_id` matching no row, which misses
    // the flags cache and reaches the database — on the highest-frequency GET
    // in the app (one per thumbnail tile).
    if current_user.is_some_and(|cu| cu.is_anonymous()) {
        return next.run(request).await;
    }

    let caller_id = current_user.map(|cu| cu.id);

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
        let live = decide_live_role(Ok(flags(UserRole::Admin, true)), Uuid::nil());
        // The live record wins over the (stale) claim — a freshly promoted
        // user is admin even though their token still says "user".
        assert_eq!(live, LiveRole::Active(SmolStr::new_static("admin")));
    }

    #[test]
    fn active_user_yields_current_user_role() {
        // A demoted admin: token claim still "admin", live record "user".
        let live = decide_live_role(Ok(flags(UserRole::User, true)), Uuid::nil());
        assert_eq!(live, LiveRole::Active(SmolStr::new_static("user")));
    }

    #[test]
    fn deactivated_account_is_revoked() {
        let live = decide_live_role(Ok(flags(UserRole::Admin, false)), Uuid::nil());
        assert_eq!(live, LiveRole::Revoked);
    }

    #[test]
    fn deleted_account_not_found_is_revoked() {
        let err = DomainError::new(ErrorKind::NotFound, "User", "no such user");
        let live = decide_live_role(Err(err), Uuid::nil());
        assert_eq!(live, LiveRole::Revoked);
    }

    /// Replaces `transient_error_fails_open_on_claim_role`, which pinned
    /// the fail-open arm: a failed lookup returned `Active(claim_role)`,
    /// so a demoted admin kept admin power for as long as the lookup kept
    /// failing — the one thing this re-check exists to catch.
    #[test]
    fn transient_error_is_indeterminate_and_retryable() {
        let err = DomainError::transient_backend("Database", "connection reset");
        assert_eq!(
            decide_live_role(Err(err), Uuid::nil()),
            LiveRole::Indeterminate { retryable: true },
        );
    }

    /// A permanent failure is still indeterminate, but not retryable, so
    /// callers answer 500 rather than inviting a retry that cannot help.
    #[test]
    fn permanent_error_is_indeterminate_and_not_retryable() {
        let err = DomainError::new(ErrorKind::InternalError, "User", "column missing");
        assert_eq!(
            decide_live_role(Err(err), Uuid::nil()),
            LiveRole::Indeterminate { retryable: false },
        );
    }

    /// The property the whole variant exists for: a failed lookup never
    /// yields a ROLE. `Active(claim_role)` was the fail-open, and
    /// `Revoked` would be the opposite mistake — the cookie branch turns
    /// `Revoked` into a 401 and a redirect to `/login`, so collapsing
    /// "cannot tell" into it would log out every browser session during a
    /// blip.
    #[test]
    fn a_failed_lookup_never_yields_a_role_nor_a_revocation() {
        for kind in [
            ErrorKind::InternalError,
            ErrorKind::TransientBackend,
            ErrorKind::Timeout,
            ErrorKind::DatabaseError,
            ErrorKind::AccessDenied,
        ] {
            let live = decide_live_role(Err(DomainError::new(kind, "User", "x")), Uuid::nil());
            assert!(
                matches!(live, LiveRole::Indeterminate { .. }),
                "{kind:?} must be indeterminate, got {live:?}",
            );
        }
    }

    /// `NotFound` stays a revocation — the account is genuinely gone, and
    /// that IS the definitive answer the cookie branch should turn into a
    /// 401. Pinned next to the above so the boundary between them is
    /// visible in one place.
    #[test]
    fn not_found_stays_a_revocation_not_indeterminate() {
        let err = DomainError::new(ErrorKind::NotFound, "User", "no such user");
        assert_eq!(decide_live_role(Err(err), Uuid::nil()), LiveRole::Revoked);
    }

    /// The status split, pinned on the shared helper the four call sites
    /// use — so a change there cannot quietly turn a blip into a 500 or a
    /// permanent fault into "please retry".
    #[test]
    fn indeterminate_renders_503_when_retryable_and_500_otherwise() {
        assert_eq!(
            indeterminate_app_error(true).status_code,
            StatusCode::SERVICE_UNAVAILABLE,
        );
        assert_eq!(
            indeterminate_app_error(false).status_code,
            StatusCode::INTERNAL_SERVER_ERROR,
        );
    }

    /// An anonymous session skips the live re-check — there is no user row
    /// to re-check against, and without this every share request 401s.
    #[test]
    fn anonymous_claim_skips_the_live_recheck() {
        assert_eq!(
            anonymous_live_role("anonymous"),
            Some(LiveRole::Active(SmolStr::new_static("anonymous"))),
        );
    }

    /// Every other claim must still go through the DB re-check. If this
    /// ever returned `Some`, a demoted admin or a deleted account would
    /// keep its token's frozen privileges until expiry — the exact thing
    /// `resolve_live_role` exists to prevent.
    #[test]
    fn only_anonymous_skips_the_live_recheck() {
        for claim in ["user", "admin", "", "Anonymous", "anonymous ", "root"] {
            assert_eq!(
                anonymous_live_role(claim),
                None,
                "claim {claim:?} must not bypass the live re-check",
            );
        }
    }

    #[test]
    fn internal_user_gate_admits_an_internal_account() {
        assert!(decide_internal_user(Ok(flags(UserRole::User, true)), Uuid::nil()).is_ok());
    }

    #[test]
    fn internal_user_gate_refuses_an_external_account() {
        let mut f = flags(UserRole::User, true);
        f.is_external = true;
        assert!(decide_internal_user(Ok(f), Uuid::nil()).is_err());
    }

    /// The regression this gate existed to have. A caller with no
    /// `auth.users` row used to fall into a blanket `_ => Ok(())` and be
    /// ADMITTED — and this is the only middleware guarding `/webdav`,
    /// `/caldav` and `/carddav`.
    #[test]
    fn internal_user_gate_refuses_a_principal_with_no_user_row() {
        let err = DomainError::new(ErrorKind::NotFound, "User", "no such user");
        assert!(
            decide_internal_user(Err(err), Uuid::nil()).is_err(),
            "a principal with no user record must not pass the internal-user gate",
        );
    }

    /// Replaces `internal_user_gate_stays_open_on_a_transient_error`,
    /// which pinned the fail-open arm. Admitting and denying both *answer*
    /// a question the lookup could not; 503 says "ask again", which is
    /// what is true. 403 would be the other wrong answer — it makes an
    /// SPA show a permissions error or bounce to login instead of
    /// retrying.
    #[test]
    fn internal_user_gate_answers_503_on_a_transient_error() {
        let err = DomainError::transient_backend("Database", "connection reset");
        let e = decide_internal_user(Err(err), Uuid::nil())
            .expect_err("a transient lookup failure must not admit");
        assert_eq!(e.status_code, StatusCode::SERVICE_UNAVAILABLE);
    }

    /// A permanent lookup failure is 500, not 503. Telling a client to
    /// retry a decode failure or a schema mismatch is a lie it acts on,
    /// and `DomainError::is_transient` defaults to "not retryable" for
    /// exactly this reason.
    #[test]
    fn internal_user_gate_answers_500_on_a_permanent_error() {
        let err = DomainError::new(ErrorKind::InternalError, "User", "column missing");
        let e = decide_internal_user(Err(err), Uuid::nil())
            .expect_err("a permanent lookup failure must not admit");
        assert_eq!(e.status_code, StatusCode::INTERNAL_SERVER_ERROR);
    }

    /// The property that matters more than either status code, and the
    /// one a future refactor is most likely to undo: **no error admits.**
    /// The arm these replace was `Err(_) => Ok(())` on the only
    /// middleware guarding `/webdav`, `/caldav` and `/carddav`, so any
    /// lookup failure walked straight through the gate.
    #[test]
    fn internal_user_gate_never_admits_on_any_error() {
        for kind in [
            ErrorKind::NotFound,
            ErrorKind::InternalError,
            ErrorKind::TransientBackend,
            ErrorKind::Timeout,
            ErrorKind::DatabaseError,
            ErrorKind::AccessDenied,
        ] {
            let err = DomainError::new(kind, "User", "lookup failed");
            assert!(
                decide_internal_user(Err(err), Uuid::nil()).is_err(),
                "{kind:?} must not admit the caller",
            );
        }
    }
}
