//! REST handlers for the ReBAC grant management endpoints.
//!
//! All endpoints under `/api/grants`. The authenticated caller is taken from
//! the `AuthUser` extractor. Authorization for sharing operations is enforced
//! via `authz.require(caller, Share, resource)` — handlers never embed their
//! own checks (see CLAUDE.md § Authorization).

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{error, warn};
use utoipa::IntoParams;
use uuid::Uuid;

use crate::application::dtos::cursor::PageCursor;
use crate::application::dtos::drive_dto::DriveDto;
use crate::application::dtos::grant_dto::{
    CreateGrantDto, CreateGrantResponseDto, GrantDto, MySharesDto, NotifyOutcomeSetDto,
    OutgoingResourceGrantDto, OutgoingResourceItemDto, ResourceContentDto, ResourceDto,
    ResourceTypeDto, SharedWithMeDto, SharedWithMeItemDto, SharedWithMeQuery, SubjectDto,
    SubjectInputDto, UpdateRoleDto, role_from_permissions,
};
use crate::application::ports::authorization_ports::AuthorizationEngine;
use crate::application::services::recipient_notification_service::NotifyTrigger;
use crate::common::di::AppState;
#[allow(unused_imports)]
use crate::common::errors::DomainError;
use crate::domain::repositories::drive_repository::DriveRepository;
use crate::domain::services::authorization::{
    GrantCursor, IncomingGrantSummary, OutgoingResourceSummary, Permission, Resource, ResourceKind,
    Role, Subject,
};
use crate::interfaces::errors::AppError;
use crate::interfaces::middleware::auth::AuthUser;

type AppStateRef = Arc<AppState>;

// ════════════════════════════════════════════════════════════════════════════
// POST /api/grants
// ════════════════════════════════════════════════════════════════════════════

/// Share a resource with someone — kicks off the **social flow** (invite +
/// notification + lazy external-user provisioning if subject is an email).
///
/// **Use this when:** the recipient may not exist yet, hasn't been told,
/// or this is the initial grant. The handler:
/// - resolves `subject` (User / Group / Token / Email — email lazily creates
///   an external user via `MagicLinkInviteService::resolve_or_create_recipient`),
/// - writes one role-grant row via `authz.set_role` (`ON CONFLICT UPDATE`),
/// - sends a share-notification email (magic-link arm for externals, plain
///   notification for internal users — both honour `notify_on_share` opt-out
///   and per-recipient rate limits).
///
/// **Compare with `PUT /api/grants/role`:** that endpoint is the *silent*
/// admin-style role change for an already-known subject; it skips the
/// invitation and notification side-effects entirely. Both write through
/// the same idempotent UPSERT, so the only operational difference is the
/// social flow attached here.
///
/// **Drive resources** (`resource.type == "drive"`) are routed through
/// `DriveManagementService.set_member_role` internally — the
/// personal-drive guard and shared-drive last-owner protection apply
/// no matter which endpoint creates the grant.
#[utoipa::path(
    post,
    path = "/api/grants",
    request_body = CreateGrantDto,
    responses(
        (status = 201, description = "Grant(s) created", body = CreateGrantResponseDto),
        (status = 400, description = "Invalid input (both/neither of permissions+role provided)"),
        (status = 404, description = "Resource not found OR caller lacks Share permission"),
        (status = 405, description = "Drive resource: personal drives have immutable membership"),
    ),
    security(("bearerAuth" = [])),
    tag = "grants"
)]
pub async fn create_grant(
    State(state): State<AppStateRef>,
    auth_user: AuthUser,
    Json(dto): Json<CreateGrantDto>,
) -> impl IntoResponse {
    let authz = &state.authorization;
    let caller_id = auth_user.id;

    let role: Role = dto.role.into();
    let resource: Resource = dto.resource.into();
    let expires_at = dto.expires_at;

    // Caller must have Share on the resource (owners pass via short-circuit).
    if let Err(e) = authz
        .require(Subject::User(caller_id), Permission::Share, resource)
        .await
    {
        return AppError::from(e).into_response();
    }

    // D5: load the resource's owning drive policies in one round-trip
    // and gate `forbid_external_sharing` (early refusal for email
    // subjects below + late refusal for resolved external users further
    // down). `forbid_sharing` (the next D5 policy) will read the same
    // fetched bag — see `docs/plan/drive.md` §8.
    let drive_policies = match resource {
        Resource::File(id) => state.drive_repo.get_policies_for_file(id).await,
        Resource::Folder(id) => state.drive_repo.get_policies_for_folder(id).await,
        Resource::Drive(id) => state
            .drive_repo
            .get_by_id(id)
            .await
            .map(|d| d.drive.typed_policies()),
        // Calendars, address books and playlists live outside the
        // drive hierarchy (top-level per user), so no drive-level
        // policy gates apply. If per-resource policies ever ship for
        // these kinds, they'll live on the resource itself, not on a
        // drive; the default-empty bag is the right no-op here.
        Resource::Calendar(_) | Resource::AddressBook(_) | Resource::Playlist(_) => {
            Ok(crate::domain::entities::drive::DrivePolicies::default())
        }
    };
    let drive_policies = match drive_policies {
        Ok(p) => p,
        Err(e) => {
            return AppError::internal_error(format!("drive policy lookup: {e:?}")).into_response();
        }
    };

    // D5 — `forbid_sharing`: refuses per-resource grants on
    // File / Folder when the drive's policy is on. Drive-resource
    // grants intentionally bypass this gate — they're drive
    // membership, not per-resource sharing (§8 semantic carve-out).
    if !matches!(resource, Resource::Drive(_))
        && let Err(e) =
            drive_policies.refuse_sharing(crate::domain::entities::drive::SharingGateContext {
                caller_id,
                resource_type: resource.type_str(),
                resource_id: resource.id(),
            })
    {
        return AppError::from(e).into_response();
    }

    // D5 — `forbid_public_links`: Token subjects on `POST /api/grants`
    // create exactly the anonymous-link grant that this policy is meant
    // to block — the canonical surface is `share_service::create_shared_link`
    // but the same kind of grant can be minted here by passing
    // `subject.type=token`. Use the same shared gate so the refusal
    // shape stays in lockstep with the share-handler path.
    if matches!(&dto.subject, SubjectInputDto::Token { .. })
        && let Err(e) = drive_policies.refuse_public_links(
            crate::domain::entities::drive::PublicLinkGateContext {
                caller_id,
                item_type: resource.type_str(),
                item_id: resource.id(),
            },
        )
    {
        return AppError::from(e).into_response();
    }

    // D5 — `forbid_external_sharing` (early): when the caller is sharing
    // by email or OpenCloudMesh, refuse BEFORE `resolve_or_create_recipient`
    // runs so this policy never side-effects a fresh external-user row. Existing
    // external users are caught by the late check below.
    if drive_policies.forbid_external_sharing
        && (matches!(&dto.subject, SubjectInputDto::Email { .. })
            | matches!(&dto.subject, SubjectInputDto::OpenCloudMesh { .. }))
    {
        tracing::info!(
            target: "audit",
            event = "grant.rejected",
            reason = "forbid_external_sharing",
            stage = "early_email",
            caller_id = %caller_id,
            resource_type = resource.type_str(),
            resource_id = %resource.id(),
            "👮🏻‍♂️ email-grant refused: drive policy forbid_external_sharing",
        );
        return AppError::from(DomainError::operation_not_supported(
            "Grant",
            "This drive does not allow external sharing.",
        ))
        .into_response();
    }

    // Resolve the subject. For the email variant this lazily provisions
    // an external user (or reuses an existing match) and remembers the
    // resolved User so the invitation email can be sent after the grant
    // rows land.
    // FIXME: clone to make ocm send share match work below
    let (subject, invite_recipient) = match dto.subject.clone() {
        SubjectInputDto::User { id } => (Subject::User(id), None),
        SubjectInputDto::Group { id } => (Subject::Group(id), None),
        SubjectInputDto::Token { id } => (Subject::Token(id), None),
        SubjectInputDto::Email { email } => {
            let Some(invite_svc) = state.magic_link_invite_service.as_ref() else {
                return AppError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Magic-link invitations are not configured on this server \
                     (set OXICLOUD_SMTP_HOST in .env to enable)",
                    "ServiceUnavailable",
                )
                .into_response();
            };
            // PR 12 — per-sharer ceiling: 50 email-invitations / hour
            // per caller. Hitting the cap returns 429 because the
            // caller is authenticated and rate-limit visibility leaks
            // nothing they don't already know about their own
            // behaviour.
            if state
                .email_invite_rate_limiter
                .check_and_increment(&caller_id.to_string())
                .is_err()
            {
                tracing::warn!(
                    target: "audit",
                    event = "grants.email_invite",
                    reason = "rate_limited",
                    caller_id = %caller_id,
                    "Per-sharer email-invite rate limit exceeded"
                );
                return crate::interfaces::middleware::rate_limit::too_many_requests(
                    state.email_invite_rate_limiter.retry_after(),
                );
            }
            // PR C: pass the inviter id so resolve_or_create_recipient
            // can inherit their preferred_locale onto a freshly-
            // provisioned external user (best-effort; lookup failure
            // just leaves the new row's locale NULL, no hard error).
            match invite_svc
                .resolve_or_create_recipient(&email, Some(caller_id))
                .await
            {
                Ok(user) => (Subject::User(user.id()), Some(user)),
                Err(e) => return AppError::from(e).into_response(),
            }
        }
        SubjectInputDto::OpenCloudMesh { ref share_with } => {
            let Some(ocm_svc) = state.opencloudmesh_service.as_ref() else {
                return AppError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "OpenCloudMesh is not configured on this server",
                    "ServiceUnavailable",
                )
                .into_response();
            };
            match ocm_svc
                .resolve_or_create_recipient(&share_with)
                .await
            {
                Ok(user) => (Subject::User(user.id()), Some(user)),
                Err(e) => return AppError::from(e).into_response(),
            }
        }
    };

    // D5 — `forbid_external_sharing` (late) for File/Folder ONLY:
    // catches the case where the subject resolved to a pre-existing
    // external user. The early check above only fires for email-input;
    // this one closes the user-by-id loophole.
    //
    // Drive resources are deliberately skipped here — they route through
    // `set_member_role` below, which runs the SAME gate
    // (`DrivePolicies::refuse_external_sharing`) at the service layer.
    // That one service-layer check also covers `POST /api/drives/{id}/members`
    // and its PATCH sibling, where no grant_handler runs. Checking
    // again here for Drive would duplicate the user-flags lookup.
    //
    // `invite_recipient` carries the User entity when we just came from
    // the email path — read its `is_external` flag instead of a
    // redundant lookup; otherwise probe via `get_user_flags`.
    if drive_policies.forbid_external_sharing
        && !matches!(resource, Resource::Drive(_))
        && let Subject::User(uid) = subject
    {
        let is_external = if let Some(user) = invite_recipient.as_ref() {
            user.is_external()
        } else if let Some(auth_svc) = state.auth_service.as_ref() {
            match auth_svc.auth_application_service.get_user_flags(uid).await {
                Ok(flags) => flags.is_external,
                Err(e) => {
                    return AppError::internal_error(format!("user flags lookup: {e:?}"))
                        .into_response();
                }
            }
        } else {
            false
        };
        if let Err(e) = drive_policies.refuse_external_sharing(
            subject,
            is_external,
            crate::domain::entities::drive::ExternalSharingGateContext {
                caller_id,
                stage: "late_user",
                drive_id: None,
                resource_type: Some(resource.type_str()),
                resource_id: Some(resource.id()),
            },
        ) {
            return AppError::from(e).into_response();
        }
    }

    // Single role row in `storage.role_grants`. `ON CONFLICT UPDATE` in
    // the engine makes repeated POSTs with the same (subject, resource)
    // a role refresh, matching the PATCH-style semantics callers expect.
    //
    // Drive resources are routed through `DriveManagementService` so the
    // personal-drive guard and shared-drive last-owner protection apply
    // — the same guards that gate `/api/drives/{id}/members`. Defense in
    // depth: a caller can't bypass them by hitting `/api/grants` directly.
    let grant = if let Resource::Drive(drive_id) = resource {
        match state
            .drive_management_service
            .set_member_role(caller_id, false, drive_id, subject, role, expires_at)
            .await
        {
            Ok(g) => g,
            Err(err) => return AppError::from(err).into_response(),
        }
    } else {
        match authz
            .set_role(caller_id, subject, role, resource, expires_at)
            .await
        {
            Ok(g) => g,
            Err(err) => {
                error!("set_role write failed: {err}");
                return AppError::from(err).into_response();
            }
        }
    };
    let grants = vec![GrantDto::from(grant)];

    tracing::info!(
        target: "audit",
        event = "role_grant.created",
        caller_id = %caller_id,
        subject_type = subject.type_str(),
        subject_id = %subject.id(),
        resource_type = resource.type_str(),
        resource_id = %resource.id(),
        role = role.as_str(),
        expires_at = ?expires_at,
        "🤝 grant created with role '{}'", role.as_str(),
    );

    // PR N1 — route the post-grant notification through the unified
    // RecipientNotificationService. Handles user/group/token subjects
    // uniformly (Token subjects return an empty outcome set); applies
    // per-(granter, recipient) coalesce + per-recipient hard rate
    // limit; dispatches the magic-link arm (delegating to
    // MagicLinkInviteService::issue_invitation) for eligible externals
    // and the plain-notification arm for internal users; honours the
    // per-user `notify_on_share` opt-out and the operator-level
    // `OXICLOUD_NOTIFY_INTERNAL_USERS_ON_SHARE` flag. SMTP failures
    // remain non-fatal — the grant rows are already in place and the
    // service captures every per-recipient result as a NotifyOutcome
    // rather than an Err.
    //
    // For the email-resolved subject variant we already loaded the
    // recipient `User` above for the lazy-provision side effect; the
    // notification service re-resolves the same id, which is cheap and
    // keeps the entry-point signature uniform across subject types.
    let _ = invite_recipient; // value used only for its side effect above

    #[cfg(feature = "opencloudmesh")]
    {
        match (dto.subject, invite_recipient) {
            (SubjectInputDto::OpenCloudMesh { .. }, Some(invite_recipient)) => {
                let Some(ocm_svc) = state.opencloudmesh_service.as_ref() else {
                    return AppError::new(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "OpenCloudMesh is not configured on this server",
                        "ServiceUnavailable",
                    )
                    .into_response();
                };
                match ocm_svc
                    .send_share(invite_recipient, caller_id, &resource)
                    .await
                {
                    Ok(user) => (Subject::User(user.id()), Some(user)),
                    Err(e) => return AppError::from(e).into_response(),
                };
            }
            _ => {},
        }
    };

    // Load the granter as a full `User` entity — the notification
    // service uses display fields (`username`, `given/family_name`) for
    // the inviter label in the email body. Failure here means the JWT
    // claims correspond to a user row that has since been deleted; we
    // return the grants without a notification rather than rolling back.
    let notification = match (
        state.recipient_notification_service.as_ref(),
        state.auth_service.as_ref(),
    ) {
        (Some(svc), Some(auth_svc)) => {
            match auth_svc
                .auth_application_service
                .get_user_entity(caller_id)
                .await
            {
                Ok(granter) => match svc
                    .send_share_notification(
                        &granter,
                        subject,
                        resource,
                        NotifyTrigger::GrantCreated,
                    )
                    .await
                {
                    Ok(set) => set.to_dto(),
                    Err(e) => {
                        warn!(
                            "notification dispatch failed for grant action by {}: {}",
                            caller_id, e
                        );
                        NotifyOutcomeSetDto::empty()
                    }
                },
                Err(e) => {
                    warn!(
                        "granter {} user-row load failed; skipping notification: {}",
                        caller_id, e
                    );
                    NotifyOutcomeSetDto::empty()
                }
            }
        }
        _ => NotifyOutcomeSetDto::empty(),
    };

    (
        StatusCode::CREATED,
        Json(CreateGrantResponseDto {
            grants,
            notification,
        }),
    )
        .into_response()
}

// ════════════════════════════════════════════════════════════════════════════
// DELETE /api/grants/{id}
// ════════════════════════════════════════════════════════════════════════════

#[utoipa::path(
    delete,
    path = "/api/grants/{id}",
    params(("id" = String, Path, description = "Grant UUID")),
    responses(
        (status = 204, description = "Grant revoked (or did not exist)"),
        (status = 404, description = "Caller lacks Share permission on the underlying resource"),
    ),
    security(("bearerAuth" = [])),
    tag = "grants"
)]
pub async fn revoke_grant(
    State(state): State<AppStateRef>,
    auth_user: AuthUser,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let authz = &state.authorization;
    let caller_id = auth_user.id;
    let grant_id = match Uuid::parse_str(&id) {
        Ok(u) => u,
        Err(_) => return AppError::not_found(format!("Grant {id} not found")).into_response(),
    };

    // Look up the grant to find the subject, resource, and granter.
    // `find_grant_full_by_id` returns the subject too — needed for the
    // `clear_role` dual-write below (role_grants is keyed by (subject,
    // resource), not by access_grants id).
    let (subject, resource, granter) = match authz.find_grant_full_by_id(grant_id).await {
        Ok(Some(triple)) => triple,
        Ok(None) => return StatusCode::NO_CONTENT.into_response(), // idempotent
        Err(e) => return AppError::from(e).into_response(),
    };

    // Drive resources are routed through `DriveManagementService` so the
    // personal-drive guard and shared-drive last-owner protection apply.
    // Note: the "granter can always revoke" shortcut DOES NOT apply for
    // drive grants — drive membership mutations always require current
    // Manage on the drive, even if you granted the row originally and
    // were later demoted.
    if let Resource::Drive(drive_id) = resource {
        // Also revoke the legacy access_grants row for the dual-write
        // window — `remove_member` handles the role_grants side.
        if let Err(e) = authz.revoke(grant_id).await {
            return AppError::from(e).into_response();
        }
        if let Err(e) = state
            .drive_management_service
            .remove_member(caller_id, false, drive_id, subject)
            .await
        {
            return AppError::from(e).into_response();
        }
    } else {
        // Caller is authorized if they are the granter OR have Share on the resource.
        if granter != caller_id
            && let Err(e) = authz
                .require(Subject::User(caller_id), Permission::Share, resource)
                .await
        {
            return AppError::from(e).into_response();
        }

        if let Err(e) = authz.revoke(grant_id).await {
            return AppError::from(e).into_response();
        }

        // D-Prep dual-write: clear the role_grants row for this (subject,
        // resource). Idempotent — succeeds whether or not the row existed.
        //
        // Today's API revokes one access_grants row by id; the role_grants
        // row models the WHOLE (subject, resource) cluster. Calling clear_role
        // here effectively revokes the WHOLE role assignment in role_grants,
        // even if other per-permission access_grants rows remain. This is the
        // correct semantics for the eventual cleanup-PR model (role_grants is
        // role-keyed; once access_grants goes away, "revoke" means "drop the
        // role"). During the dual-write window the two tables can drift
        // briefly if a caller revokes only some permissions of a role, but
        // the engine still reads access_grants so behaviour is unchanged.
        if let Err(e) = authz.clear_role(subject, resource).await {
            return AppError::from(e).into_response();
        }
    }

    tracing::info!(
        target: "audit",
        event = "role_grant.revoked",
        caller_id = %caller_id,
        grant_id = %grant_id,
        subject_type = subject.type_str(),
        subject_id = %subject.id(),
        resource_type = resource.type_str(),
        resource_id = %resource.id(),
        granter_id = %granter,
        self_revoke = (granter == caller_id),
        "🗑️ grant revoked",
    );
    StatusCode::NO_CONTENT.into_response()
}

// ════════════════════════════════════════════════════════════════════════════
// POST /api/grants/{id}/notify — manual share-notification resend
// ════════════════════════════════════════════════════════════════════════════

#[utoipa::path(
    post,
    path = "/api/grants/{id}/notify",
    params(("id" = String, Path, description = "Grant UUID")),
    responses(
        (status = 204, description = "Notification(s) dispatched"),
        (status = 200, description = "Mixed outcome (some recipients coalesced / not-applicable); body carries the full NotifyOutcomeSet", body = NotifyOutcomeSetDto),
        (status = 404, description = "Grant not found OR caller is not the granter"),
        (status = 409, description = "Token subject (use the existing /magic/v1/{token}/resend channel)"),
        (status = 429, description = "Per-recipient hard rate limit exceeded"),
    ),
    security(("bearerAuth" = [])),
    tag = "grants"
)]
pub async fn notify_grant_recipient(
    State(state): State<AppStateRef>,
    auth_user: AuthUser,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let authz = &state.authorization;
    let caller_id = auth_user.id;

    let grant_id = match Uuid::parse_str(&id) {
        Ok(u) => u,
        Err(_) => return AppError::not_found(format!("Grant {id} not found")).into_response(),
    };

    // Load the grant. Anti-enumeration: missing AND not-owner both
    // surface as 404 to the caller; only the audit row carries the
    // real reason. Mirrors `revoke_grant`'s precedent.
    let (subject, resource, granter_id) = match authz.find_grant_full_by_id(grant_id).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            tracing::info!(
                target: "audit",
                event = "grant.notify_skipped",
                reason = "grant_not_found",
                caller_id = %caller_id,
                grant_id = %grant_id,
                "🤫 manual notify rejected: grant {} not found",
                grant_id,
            );
            return AppError::not_found(format!("Grant {grant_id} not found")).into_response();
        }
        Err(e) => return AppError::from(e).into_response(),
    };

    if granter_id != caller_id {
        tracing::info!(
            target: "audit",
            event = "grant.notify_skipped",
            reason = "not_owner",
            caller_id = %caller_id,
            grant_id = %grant_id,
            actual_granter = %granter_id,
            "🤫 manual notify rejected: caller {} is not the granter of {}",
            caller_id,
            grant_id,
        );
        return AppError::not_found(format!("Grant {grant_id} not found")).into_response();
    }

    // Token subjects can't be notified — the link share has no human
    // recipient to email. Map to 409 so the frontend can hide the menu
    // item for these as defense-in-depth (the v1 UI already does this
    // client-side; this is the server-side enforcement).
    if matches!(subject, Subject::Token(_)) {
        return AppError::new(
            StatusCode::CONFLICT,
            "Cannot notify a link-share recipient — token shares have no email channel",
            "subject_is_token",
        )
        .into_response();
    }

    // Load the granter entity (we are the granter; needed for the
    // notification email body's "Alice shared X with you" salutation).
    let Some(auth_svc) = state.auth_service.as_ref() else {
        return AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "Authentication subsystem not available",
            "ServiceUnavailable",
        )
        .into_response();
    };
    let granter = match auth_svc
        .auth_application_service
        .get_user_entity(caller_id)
        .await
    {
        Ok(u) => u,
        Err(e) => return AppError::from(e).into_response(),
    };

    let Some(svc) = state.recipient_notification_service.as_ref() else {
        return AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "Notification service is not configured on this server \
             (set OXICLOUD_SMTP_HOST in .env to enable)",
            "ServiceUnavailable",
        )
        .into_response();
    };

    let outcome_set = match svc
        .send_share_notification(&granter, subject, resource, NotifyTrigger::ManualResend)
        .await
    {
        Ok(s) => s,
        Err(e) => return AppError::from(e).into_response(),
    };

    let dto = outcome_set.to_dto();

    // HTTP mapping per the plan:
    //   - empty outcomes (Token subject — already 409'd above; defense
    //     in depth) → 409
    //   - every outcome is Sent → 204 No Content
    //   - all RateLimited (no Sent) → 429 with the longest Retry-After
    //   - mixed → 200 with the full body
    if dto.outcomes.is_empty() {
        return AppError::new(
            StatusCode::CONFLICT,
            "Grant has no notifiable recipients",
            "subject_is_token",
        )
        .into_response();
    }

    let any_sent = dto.outcomes.iter().any(|o| {
        matches!(
            o,
            crate::application::dtos::grant_dto::NotifyOutcomeDto::Sent { .. }
        )
    });
    let max_retry_after = dto
        .outcomes
        .iter()
        .filter_map(|o| match o {
            crate::application::dtos::grant_dto::NotifyOutcomeDto::RateLimited {
                retry_after_secs,
            } => Some(*retry_after_secs),
            _ => None,
        })
        .max();
    let all_sent = dto.outcomes.iter().all(|o| {
        matches!(
            o,
            crate::application::dtos::grant_dto::NotifyOutcomeDto::Sent { .. }
        )
    });

    if all_sent {
        return StatusCode::NO_CONTENT.into_response();
    }
    if !any_sent && let Some(secs) = max_retry_after {
        return crate::interfaces::middleware::rate_limit::too_many_requests(secs as u64);
    }
    (StatusCode::OK, Json(dto)).into_response()
}

// ════════════════════════════════════════════════════════════════════════════
// PUT /api/grants/role
// ════════════════════════════════════════════════════════════════════════════

/// Change an existing member's role on a resource — **silent**, no
/// invitation, no notification.
///
/// **Use this when:** the subject is already known (user/group/token has
/// an existing grant or you've already informed them out-of-band) and you
/// just want to bump their role. Typical use is the management UI's
/// "Viewer → Editor" dropdown — you don't want a fresh share email
/// firing on every dropdown change.
///
/// **Compare with `POST /api/grants`:** that endpoint kicks off the
/// invitation + share-notification social flow and accepts an email-shaped
/// subject for lazy external-user provisioning. This endpoint is the
/// admin-style update — concrete subjects only, no side effects beyond
/// the role row itself.
///
/// Both endpoints write through the same idempotent UPSERT
/// (`authz.set_role`, unique on `(subject, resource)`), so they are
/// indistinguishable in their effect on the role-grants table — the
/// difference is purely the social side-effects attached to `POST`.
///
/// **Drive resources** (`resource.type == "drive"`) are routed through
/// `DriveManagementService.set_member_role` internally — the
/// personal-drive guard and shared-drive last-owner protection apply.
#[utoipa::path(
    put,
    path = "/api/grants/role",
    request_body = UpdateRoleDto,
    responses(
        (status = 200, description = "Role applied; returns the new full grant set", body = Vec<GrantDto>),
        (status = 400, description = "Drive resource: shared-drive last-owner demotion refused"),
        (status = 404, description = "Resource not found or caller lacks Share"),
        (status = 405, description = "Drive resource: personal drives have immutable membership"),
    ),
    security(("bearerAuth" = [])),
    tag = "grants"
)]
pub async fn set_role(
    State(state): State<AppStateRef>,
    auth_user: AuthUser,
    Json(dto): Json<UpdateRoleDto>,
) -> impl IntoResponse {
    let authz = &state.authorization;
    let caller_id = auth_user.id;
    let subject: Subject = dto.subject.into();
    let resource: Resource = dto.resource.into();
    let role: Role = dto.role.into();
    let expires_at = dto.expires_at;

    // Caller must have Share on the resource.
    if let Err(e) = authz
        .require(Subject::User(caller_id), Permission::Share, resource)
        .await
    {
        return AppError::from(e).into_response();
    }

    // Atomic role refresh. UNIQUE on (subject, resource) + ON CONFLICT
    // UPDATE in `set_role` turns this into a single UPSERT — no diff,
    // no race window. Returns the resulting role row.
    //
    // Drive resources are routed through `DriveManagementService` so the
    // personal-drive guard and shared-drive last-owner protection apply.
    // See `create_grant` for the same delegation rationale.
    let grant = if let Resource::Drive(drive_id) = resource {
        match state
            .drive_management_service
            .set_member_role(caller_id, false, drive_id, subject, role, expires_at)
            .await
        {
            Ok(g) => g,
            Err(e) => return AppError::from(e).into_response(),
        }
    } else {
        match authz
            .set_role(caller_id, subject, role, resource, expires_at)
            .await
        {
            Ok(g) => g,
            Err(e) => return AppError::from(e).into_response(),
        }
    };

    tracing::info!(
        target: "audit",
        event = "role_grant.role_set",
        caller_id = %caller_id,
        subject_type = subject.type_str(),
        subject_id = %subject.id(),
        resource_type = resource.type_str(),
        resource_id = %resource.id(),
        role = role.as_str(),
        expires_at = ?expires_at,
        "🔁 role set to '{}'", role.as_str(),
    );
    (StatusCode::OK, Json(vec![GrantDto::from(grant)])).into_response()
}

// ════════════════════════════════════════════════════════════════════════════
// GET /api/grants/incoming
// ════════════════════════════════════════════════════════════════════════════

#[utoipa::path(
    get,
    path = "/api/grants/incoming",
    responses(
        (status = 200, description = "Direct role grants targeting the caller", body = Vec<GrantDto>),
    ),
    security(("bearerAuth" = [])),
    tag = "grants"
)]
pub async fn list_incoming(
    State(state): State<AppStateRef>,
    auth_user: AuthUser,
) -> impl IntoResponse {
    let caller_id = auth_user.id;
    match state
        .authorization
        .list_incoming_grants(Subject::User(caller_id))
        .await
    {
        Ok(grants) => {
            let dtos: Vec<GrantDto> = grants.into_iter().map(Into::into).collect();
            (StatusCode::OK, Json(dtos)).into_response()
        }
        Err(e) => AppError::from(e).into_response(),
    }
}

// ════════════════════════════════════════════════════════════════════════════
// GET /api/grants/incoming/resources
// ════════════════════════════════════════════════════════════════════════════

#[utoipa::path(
    get,
    path = "/api/grants/incoming/resources",
    params(SharedWithMeQuery),
    responses(
        (status = 200,
         description = "Cursor-paginated resources shared with the caller. \
                        Each item carries the full file or folder details plus \
                        aggregated permissions. `next_cursor` is absent on the \
                        last page.",
         body = SharedWithMeDto),
    ),
    security(("bearerAuth" = [])),
    tag = "grants"
)]
pub async fn list_shared_with_me(
    State(state): State<AppStateRef>,
    auth_user: AuthUser,
    Query(q): Query<SharedWithMeQuery>,
) -> impl IntoResponse {
    let caller_id = auth_user.id;
    let subject = Subject::User(caller_id);

    // Parse resource_types filter (unknown values silently ignored).
    let kinds: Vec<ResourceKind> = q
        .resource_types
        .as_deref()
        .map(|s| {
            s.split(',')
                .filter_map(|t| ResourceKind::parse(t.trim()))
                .collect()
        })
        .unwrap_or_default();

    // Clamp limit to 1–200.
    let limit = q.limit_clamped() as u32;

    // Validate sort_by (defaults to "granted_at").
    let sort_by = q.sort_by.as_deref().unwrap_or("granted_at");
    if !matches!(sort_by, "granted_at" | "granted_by" | "name" | "type") {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "invalid sort_by; valid values: granted_at, granted_by, name, type"})),
        )
            .into_response();
    }

    let reverse = q.reverse;

    // Decode cursor — discard it when the sort dimension or direction changed
    // to avoid keyset confusion across sort modes.
    let cursor = q
        .decode_cursor::<GrantCursor>()
        .filter(|c| c.sort_by == sort_by && c.reverse == reverse);

    // Fetch paged summaries from the ACL engine.
    let (summaries, next_cursor) = match state
        .authorization
        .list_incoming_resources_paged(subject, &kinds, limit, cursor, sort_by, reverse)
        .await
    {
        Ok(r) => r,
        Err(e) => return AppError::from(e).into_response(),
    };

    // Split summaries by resource kind for parallel resolution.
    let file_summaries: Vec<&IncomingGrantSummary> = summaries
        .iter()
        .filter(|s| matches!(s.resource_type, ResourceKind::File))
        .collect();
    let folder_summaries: Vec<&IncomingGrantSummary> = summaries
        .iter()
        .filter(|s| matches!(s.resource_type, ResourceKind::Folder))
        .collect();
    let drive_summaries: Vec<&IncomingGrantSummary> = summaries
        .iter()
        .filter(|s| matches!(s.resource_type, ResourceKind::Drive))
        .collect();

    let file_service = &state.applications.file_retrieval_service;
    let folder_service = &state.applications.folder_service_concrete;

    // Pre-compute ID strings to avoid temporaries inside async closures.
    let file_ids: Vec<String> = file_summaries
        .iter()
        .map(|s| s.resource_id.to_string())
        .collect();
    let folder_ids: Vec<String> = folder_summaries
        .iter()
        .map(|s| s.resource_id.to_string())
        .collect();
    let drive_ids: Vec<Uuid> = drive_summaries.iter().map(|s| s.resource_id).collect();

    // Resolve resource details in three batch queries (was one per id via
    // join_all, which could fan out to ~limit concurrent pooled connections
    // and starve the primary pool). Missing ids — stale grants whose resource
    // was deleted before the cascade trigger fired — drop out of the maps.
    let (file_list, folder_list, drive_list) = tokio::join!(
        file_service.get_files_by_ids(&file_ids),
        folder_service.get_folders_by_ids(&folder_ids),
        state.drive_repo.get_by_ids(&drive_ids)
    );
    let file_map: HashMap<String, _> = match file_list {
        Ok(files) => files.into_iter().map(|f| (f.id.clone(), f)).collect(),
        Err(e) => return AppError::from(e).into_response(),
    };
    let folder_map: HashMap<String, _> = match folder_list {
        Ok(folders) => folders.into_iter().map(|f| (f.id.clone(), f)).collect(),
        Err(e) => return AppError::from(e).into_response(),
    };
    let drive_map: HashMap<Uuid, DriveDto> = match drive_list {
        Ok(drives) => drives
            .into_iter()
            .map(|d| (d.drive.id, DriveDto::from(d)))
            .collect(),
        Err(e) => {
            return AppError::internal_error(format!("Failed to batch-resolve drives: {e:?}"))
                .into_response();
        }
    };

    // Build the unified item list in original grant order (newest first),
    // looking each resolved resource up by id.
    let mut items: Vec<SharedWithMeItemDto> = Vec::with_capacity(summaries.len());

    // Enrich caller flags on every returned resource DTO. Incoming
    // grants pages are typically small (10-50 items), so N sequential
    // helper calls is acceptable; the folded-into-SQL treatment
    // photos got is overkill here. Follow-up path if this grows
    // hot: same LATERAL EXISTS shape in `get_files_by_ids` /
    // `get_folders_by_ids`.
    for summary in &summaries {
        let rid = summary.resource_id.to_string();
        match summary.resource_type {
            ResourceKind::File => match file_map.get(&rid) {
                Some(file_dto) => {
                    let mut dto = file_dto.clone().without_hierarchy_info();
                    crate::interfaces::api::handlers::caller_flags::enrich_file_flags(
                        &state, &mut dto, caller_id,
                    )
                    .await;
                    items.push(SharedWithMeItemDto {
                        resource_type: ResourceTypeDto::File,
                        permissions: summary.permissions.iter().map(|p| (*p).into()).collect(),
                        granted_at: summary.granted_at,
                        granted_by: summary.granted_by,
                        resource: ResourceContentDto::File(dto),
                    });
                }
                None => warn!(
                    "Skipping stale file grant for resource_id={}: not found",
                    summary.resource_id
                ),
            },
            ResourceKind::Folder => match folder_map.get(&rid) {
                Some(folder_dto) => {
                    let mut dto = folder_dto.clone().without_hierarchy_info();
                    crate::interfaces::api::handlers::caller_flags::enrich_folder_flags(
                        &state, &mut dto, caller_id,
                    )
                    .await;
                    items.push(SharedWithMeItemDto {
                        resource_type: ResourceTypeDto::Folder,
                        permissions: summary.permissions.iter().map(|p| (*p).into()).collect(),
                        granted_at: summary.granted_at,
                        granted_by: summary.granted_by,
                        resource: ResourceContentDto::Folder(dto),
                    });
                }
                None => warn!(
                    "Skipping stale folder grant for resource_id={}: not found",
                    summary.resource_id
                ),
            },
            ResourceKind::Drive => match drive_map.get(&summary.resource_id) {
                Some(drive_dto) => {
                    items.push(SharedWithMeItemDto {
                        resource_type: ResourceTypeDto::Drive,
                        permissions: summary.permissions.iter().map(|p| (*p).into()).collect(),
                        granted_at: summary.granted_at,
                        granted_by: summary.granted_by,
                        resource: ResourceContentDto::Drive(drive_dto.clone()),
                    });
                }
                None => warn!(
                    "Skipping stale drive grant for resource_id={}: not found",
                    summary.resource_id
                ),
            },
        }
    }

    (
        StatusCode::OK,
        Json(SharedWithMeDto::with_cursor(
            items,
            next_cursor.map(|c| c.encode()),
        )),
    )
        .into_response()
}

// ════════════════════════════════════════════════════════════════════════════
// GET /api/grants/outgoing
// ════════════════════════════════════════════════════════════════════════════

#[utoipa::path(
    get,
    path = "/api/grants/outgoing",
    responses(
        (status = 200, description = "Grants the caller has created", body = Vec<GrantDto>),
    ),
    security(("bearerAuth" = [])),
    tag = "grants"
)]
pub async fn list_outgoing(
    State(state): State<AppStateRef>,
    auth_user: AuthUser,
) -> impl IntoResponse {
    let caller_id = auth_user.id;
    match state.authorization.list_outgoing_grants(caller_id).await {
        Ok(grants) => {
            let dtos: Vec<GrantDto> = grants.into_iter().map(Into::into).collect();
            (StatusCode::OK, Json(dtos)).into_response()
        }
        Err(e) => AppError::from(e).into_response(),
    }
}

// ════════════════════════════════════════════════════════════════════════════
// GET /api/grants?resource_type=...&resource_id=...
// (list grants on a specific resource — requires Share on it)
// ════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Deserialize, IntoParams)]
pub struct OnResourceQuery {
    pub resource_type: ResourceTypeDto,
    pub resource_id: Uuid,
}

#[utoipa::path(
    get,
    path = "/api/grants",
    params(OnResourceQuery),
    responses(
        (status = 200, description = "Grants on the specified resource", body = Vec<GrantDto>),
        (status = 404, description = "Resource not found or caller lacks Share"),
    ),
    security(("bearerAuth" = [])),
    tag = "grants"
)]
pub async fn list_on_resource(
    State(state): State<AppStateRef>,
    auth_user: AuthUser,
    Query(q): Query<OnResourceQuery>,
) -> impl IntoResponse {
    let authz = &state.authorization;
    let caller_id = auth_user.id;
    let resource: Resource = ResourceDto {
        kind: q.resource_type,
        id: q.resource_id,
    }
    .into();

    if let Err(e) = authz
        .require(Subject::User(caller_id), Permission::Share, resource)
        .await
    {
        return AppError::from(e).into_response();
    }

    match authz.list_grants_on_resource(resource).await {
        Ok(grants) => {
            let dtos: Vec<GrantDto> = grants.into_iter().map(Into::into).collect();
            (StatusCode::OK, Json(dtos)).into_response()
        }
        Err(e) => AppError::from(e).into_response(),
    }
}

// ════════════════════════════════════════════════════════════════════════════
// GET /api/grants/outgoing/resources
// ════════════════════════════════════════════════════════════════════════════

#[utoipa::path(
    get,
    path = "/api/grants/outgoing/resources",
    params(SharedWithMeQuery),
    responses(
        (status = 200,
         description = "Cursor-paginated resources the caller has shared with others. \
                        Each item carries the full resource details plus all subjects \
                        (users and tokens) the resource was shared with. \
                        `next_cursor` is absent on the last page.",
         body = MySharesDto),
    ),
    security(("bearerAuth" = [])),
    tag = "grants"
)]
pub async fn list_my_shares(
    State(state): State<AppStateRef>,
    auth_user: AuthUser,
    Query(q): Query<SharedWithMeQuery>,
) -> impl IntoResponse {
    let caller_id = auth_user.id;

    let limit = q.limit_clamped() as u32;

    let sort_by = q.sort_by.as_deref().unwrap_or("first_shared_at");
    if !matches!(
        sort_by,
        "first_shared_at" | "name" | "type" | "subject" | "role"
    ) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "invalid sort_by; valid values: first_shared_at, name, type, subject, role"})),
        )
            .into_response();
    }

    let reverse = q.reverse;

    let cursor = q
        .decode_cursor::<GrantCursor>()
        .filter(|c| c.sort_by == sort_by && c.reverse == reverse);

    let (summaries, next_cursor) = match state
        .authorization
        .list_outgoing_resources_paged(caller_id, limit, cursor, sort_by, reverse)
        .await
    {
        Ok(r) => r,
        Err(e) => return AppError::from(e).into_response(),
    };

    let file_service = &state.applications.file_retrieval_service;
    let folder_service = &state.applications.folder_service_concrete;

    // Split summaries by resource kind for parallel resolution.
    let file_summaries: Vec<&OutgoingResourceSummary> = summaries
        .iter()
        .filter(|s| matches!(s.resource_type, ResourceKind::File))
        .collect();
    let folder_summaries: Vec<&OutgoingResourceSummary> = summaries
        .iter()
        .filter(|s| matches!(s.resource_type, ResourceKind::Folder))
        .collect();
    let drive_summaries: Vec<&OutgoingResourceSummary> = summaries
        .iter()
        .filter(|s| matches!(s.resource_type, ResourceKind::Drive))
        .collect();

    let file_ids: Vec<String> = file_summaries
        .iter()
        .map(|s| s.resource_id.to_string())
        .collect();
    let folder_ids: Vec<String> = folder_summaries
        .iter()
        .map(|s| s.resource_id.to_string())
        .collect();
    let drive_ids: Vec<Uuid> = drive_summaries.iter().map(|s| s.resource_id).collect();

    // Three batch queries instead of one get_* per id (see list_shared_with_me).
    let (file_list, folder_list, drive_list) = tokio::join!(
        file_service.get_files_by_ids(&file_ids),
        folder_service.get_folders_by_ids(&folder_ids),
        state.drive_repo.get_by_ids(&drive_ids)
    );
    let file_map: HashMap<String, _> = match file_list {
        Ok(files) => files.into_iter().map(|f| (f.id.clone(), f)).collect(),
        Err(e) => return AppError::from(e).into_response(),
    };
    let folder_map: HashMap<String, _> = match folder_list {
        Ok(folders) => folders.into_iter().map(|f| (f.id.clone(), f)).collect(),
        Err(e) => return AppError::from(e).into_response(),
    };
    let drive_map: HashMap<Uuid, DriveDto> = match drive_list {
        Ok(drives) => drives
            .into_iter()
            .map(|d| (d.drive.id, DriveDto::from(d)))
            .collect(),
        Err(e) => {
            return AppError::internal_error(format!("Failed to batch-resolve drives: {e:?}"))
                .into_response();
        }
    };

    let mut items: Vec<OutgoingResourceItemDto> = Vec::with_capacity(summaries.len());

    for summary in &summaries {
        let grants: Vec<OutgoingResourceGrantDto> = summary
            .grants
            .iter()
            .map(|g| OutgoingResourceGrantDto {
                grant_id: g.grant_id,
                subject_type: g.subject_type.clone(),
                subject_id: g.subject_id,
                subject_display: g.subject_display.clone(),
                role: role_from_permissions(&g.permissions).to_owned(),
                granted_at: g.granted_at,
                expires_at: g.expires_at,
                has_password: g.has_password,
                is_external: g.is_external,
            })
            .collect();

        let rid = summary.resource_id.to_string();
        match summary.resource_type {
            ResourceKind::File => match file_map.get(&rid) {
                Some(file_dto) => {
                    // Caller is the granter — they had share-access to the
                    // resource, so the containing hierarchy is already known
                    // to them. Keep `path` (unlike list_shared_with_me).
                    let mut dto = file_dto.clone();
                    // is_shared: TRUE by construction on this feed.
                    // is_favorite: real EXISTS via the shared helper.
                    crate::interfaces::api::handlers::caller_flags::enrich_file_flags(
                        &state, &mut dto, caller_id,
                    )
                    .await;
                    dto.is_shared = true;
                    items.push(OutgoingResourceItemDto {
                        resource_type: ResourceTypeDto::File,
                        first_shared_at: summary.first_shared_at,
                        resource: ResourceContentDto::File(dto),
                        grants,
                    });
                }
                None => warn!(
                    "Skipping stale outgoing file grant for resource_id={}: not found",
                    summary.resource_id
                ),
            },
            ResourceKind::Folder => match folder_map.get(&rid) {
                Some(folder_dto) => {
                    let mut dto = folder_dto.clone();
                    crate::interfaces::api::handlers::caller_flags::enrich_folder_flags(
                        &state, &mut dto, caller_id,
                    )
                    .await;
                    dto.is_shared = true;
                    items.push(OutgoingResourceItemDto {
                        resource_type: ResourceTypeDto::Folder,
                        first_shared_at: summary.first_shared_at,
                        resource: ResourceContentDto::Folder(dto),
                        grants,
                    });
                }
                None => warn!(
                    "Skipping stale outgoing folder grant for resource_id={}: not found",
                    summary.resource_id
                ),
            },
            ResourceKind::Drive => match drive_map.get(&summary.resource_id) {
                Some(drive_dto) => {
                    items.push(OutgoingResourceItemDto {
                        resource_type: ResourceTypeDto::Drive,
                        first_shared_at: summary.first_shared_at,
                        resource: ResourceContentDto::Drive(drive_dto.clone()),
                        grants,
                    });
                }
                None => warn!(
                    "Skipping stale outgoing drive grant for resource_id={}: not found",
                    summary.resource_id
                ),
            },
        }
    }

    (
        StatusCode::OK,
        Json(MySharesDto::with_cursor(
            items,
            next_cursor.map(|c| c.encode()),
        )),
    )
        .into_response()
}

// Silence unused-import warnings for SubjectDto when only certain endpoints
// touch it directly.
#[allow(dead_code)]
fn _ensure_subject_dto_compiles(_: SubjectDto) {}
