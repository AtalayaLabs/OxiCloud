use axum::Json;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

use crate::application::dtos::search_dto::SearchCriteriaDto;
use crate::application::ports::inbound::SearchUseCase;
use crate::application::ports::storage_ports::StorageUsagePort;
use crate::common::di::AppState;
use crate::interfaces::middleware::auth::AuthUser;

/// Build an OCS success response with the given statuscode and data.
fn ocs_ok(statuscode: u16, data: serde_json::Value) -> serde_json::Value {
    json!({
        "ocs": {
            "meta": { "status": "ok", "statuscode": statuscode, "message": "OK" },
            "data": data,
        }
    })
}

/// Build an OCS error response.
fn ocs_err(statuscode: u16, message: &str) -> serde_json::Value {
    json!({
        "ocs": {
            "meta": { "status": "failure", "statuscode": statuscode, "message": message },
            "data": {},
        }
    })
}

pub async fn handle_capabilities_v1(State(state): State<Arc<AppState>>) -> Response {
    tracing::debug!("[NC] capabilities v1 requested, returning payload");
    capabilities_response(&state, 1)
}

pub async fn handle_capabilities_v2(State(state): State<Arc<AppState>>) -> Response {
    tracing::debug!("[NC] capabilities v2 requested, returning payload");
    capabilities_response(&state, 2)
}

/// Pre-serialized capabilities bodies, `[v1, v2]`. The payload is
/// process-invariant (pure config: base URL + emulated NC version), yet
/// every desktop/mobile client polls it periodically — the old handler
/// re-built the ~40-node `json!` tree, re-read `OXICLOUD_BASE_URL` from
/// the environment and re-serialized on every poll. Now that work runs
/// once; a poll is a `Bytes` refcount bump.
static CAPABILITIES_BODIES: std::sync::OnceLock<[bytes::Bytes; 2]> = std::sync::OnceLock::new();

fn capabilities_response(state: &AppState, ocs_version: u8) -> Response {
    let bodies = CAPABILITIES_BODIES.get_or_init(|| {
        let base_url = state.core.config.base_url();
        let emulated = state.core.config.nextcloud.emulated_version;
        let version_string = state.core.config.nextcloud.version_string();
        [1u8, 2u8].map(|v| {
            bytes::Bytes::from(
                serde_json::to_vec(&capabilities_payload(
                    &base_url,
                    emulated,
                    &version_string,
                    v,
                ))
                .expect("static capabilities JSON serializes"),
            )
        })
    });
    let body = bodies[usize::from(ocs_version != 1)].clone();
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

pub async fn handle_user_info(
    State(state): State<Arc<AppState>>,
    session: crate::interfaces::nextcloud::session::SharedNcSession,
) -> Response {
    let quota: (i64, i64) = match state.storage_usage_service.as_ref() {
        Some(service) => match service.get_user_storage_info(session.user.id).await {
            Ok((used, total)) => (used, total),
            Err(_) => (0, 0),
        },
        None => (0, 0),
    };

    let free = quota.1.saturating_sub(quota.0);
    let relative = if quota.1 > 0 {
        (quota.0 as f64 / quota.1 as f64) * 100.0
    } else {
        0.0
    };

    // `id` MUST echo the raw wire username the client used at Basic
    // Auth time — NC desktop reads `data.id` from this endpoint and
    // splices it into every subsequent WebDAV path it builds
    // (`/remote.php/dav/files/{id}/…`). Returning the bare canonical
    // username on a `~{uuid}` session would make the client strip
    // the marker and revert to the home drive.
    //
    // Display fields stay short on the default drive (bare
    // username); on a marker session we render `username@<drive>`
    // using the resolved chroot's stored name, which is friendlier
    // than the raw UUID the wire form carries.
    let id = session.raw_username.clone();
    let displayname = if session.is_home() {
        session.user.username.to_string()
    } else {
        match session.chroot.as_ref() {
            Some(chroot) => format!("{}@{}", session.user.username, chroot.name),
            None => session.user.username.to_string(),
        }
    };

    Json(json!({
        "ocs": {
            "meta": { "status": "ok", "statuscode": 200, "message": "OK" },
            "data": {
                "enabled": true,
                "id": id,
                "display-name": displayname,
                "displayname": displayname,
                "email": session.user.email,
                "quota": {
                    "used": quota.0,
                    "total": quota.1,
                    "free": free,
                    "relative": relative
                }
            }
        }
    }))
    .into_response()
}

/// GET /ocs/v1.php/cloud/users/{userid}
pub async fn handle_user_provisioning_v1(
    state: State<Arc<AppState>>,
    path: Path<String>,
    user: AuthUser,
) -> Response {
    user_provisioning_response(state, path, user, 1).await
}

/// GET /ocs/v2.php/cloud/users/{userid}
pub async fn handle_user_provisioning_v2(
    state: State<Arc<AppState>>,
    path: Path<String>,
    user: AuthUser,
) -> Response {
    user_provisioning_response(state, path, user, 2).await
}

/// Returns user details in Nextcloud OCS provisioning API format.
/// Used by the Nextcloud mobile app to fetch the user profile screen.
async fn user_provisioning_response(
    State(state): State<Arc<AppState>>,
    Path(userid): Path<String>,
    user: AuthUser,
    ocs_version: u8,
) -> Response {
    let statuscode = if ocs_version == 1 { 100 } else { 200 };

    // AuthZ audit #11 (2026-07-12): the pre-fix path here rolled its
    // own gate ("caller is `userid`, else must be admin") and then
    // called bare `get_user_by_username` — bypassing every visibility
    // rule the id-keyed `/api/users/{id}` endpoint enforces. Cross-user
    // probes returned 403 (leaking existence via the differential vs a
    // genuine 404 for missing users); admins bypassed
    // `expose_system_users`; no audit line ever fired.
    //
    // Now routing through `get_user_profile_by_username_with_perms`,
    // which delegates to the same visibility engine as the REST
    // endpoint (self / shared-grant / expose_system_users / admin
    // paths, all audit-logged on denial). The OCS wire shape stays
    // `ocs_err(404, ...)` for every denied case — the NC client can't
    // tell "no such user" from "you can't see this user" from "you're
    // not admin" apart, which is the anti-enum invariant.
    let auth_service = match state.auth_service.as_ref() {
        Some(svc) => &svc.auth_application_service,
        None => {
            return Json(ocs_err(997, "Authentication not configured")).into_response();
        }
    };
    let Some(pool) = state.db_pool.as_ref() else {
        return Json(ocs_err(997, "Database pool not available")).into_response();
    };

    // Two-step lookup: (1) `get_user_profile_by_username_with_perms`
    // gates access via the same visibility engine the REST endpoint
    // uses; (2) if visibility passes, `get_user_with_derived_flags`
    // hydrates the OCS-specific fields (federation_kind / last_login_at
    // / active) that live on `FullUserDto` but not on the slim
    // `PublicUserDto` returned by the visibility gate. Second call is
    // ~1 DB round-trip on the maintenance pool; NC OCS provisioning is
    // not on any hot inner loop.
    let public = match auth_service
        .get_user_profile_by_username_with_perms(
            user.id,
            &userid,
            state.core.config.features.expose_system_users,
            pool,
        )
        .await
    {
        Ok(u) => u,
        Err(_) => {
            return Json(ocs_err(404, "User not found")).into_response();
        }
    };
    let target_id = match uuid::Uuid::parse_str(&public.id) {
        Ok(u) => u,
        Err(_) => {
            // Should be unreachable — PublicUserDto.id is always the
            // serialised form of a Uuid. Fail closed if this invariant
            // is ever violated.
            return Json(ocs_err(500, "Malformed user id")).into_response();
        }
    };
    let user_dto = match auth_service.get_user_with_derived_flags(target_id).await {
        Ok((user, flags)) => crate::application::dtos::user_dto::FullUserDto::build(user, flags),
        Err(_) => {
            // Visibility already passed above; a miss here would mean
            // the user was deleted between the two round-trips. Fall
            // back to the 404 shape (anti-enum invariant still holds).
            return Json(ocs_err(404, "User not found")).into_response();
        }
    };

    // Determine groups based on role
    let groups = if user_dto.user.role == "admin" {
        vec!["admin", "users"]
    } else {
        vec!["users"]
    };

    // Determine backend based on federation kind. Historically checked
    // `auth_provider.to_lowercase().contains("oidc")` which happened to
    // work when the DTO field held a display label containing "oidc"
    // (e.g. "OIDC-Google") — but broke silently when the label was
    // "MockSSO" or, post Phase B of the federation-identity rename, when
    // the field became an issuer URL that doesn't contain "oidc". The
    // kind field is the load-bearing signal.
    let backend = if user_dto.federation_kind.as_deref() == Some("oidc") {
        "OIDC"
    } else {
        "Database"
    };

    // Convert last_login_at to JS milliseconds
    let last_login = user_dto
        .last_login_at
        .map(|dt| dt.timestamp() * 1000)
        .unwrap_or(0);

    // Fetch quota from storage usage service
    let quota: (i64, i64) = match state.storage_usage_service.as_ref() {
        Some(service) => match service
            .get_user_storage_info(uuid::Uuid::parse_str(&user_dto.user.id).unwrap_or_default())
            .await
        {
            Ok((used, total)) => (used, total),
            Err(_) => (0, 0),
        },
        None => (0, 0),
    };

    let free = quota.1.saturating_sub(quota.0);
    let relative = if quota.1 > 0 {
        (quota.0 as f64 / quota.1 as f64) * 100.0
    } else {
        0.0
    };

    Json(json!({
        "ocs": {
            "meta": { "status": "ok", "statuscode": statuscode, "message": "OK" },
            "data": {
                "enabled": user_dto.active,
                "id": user_dto.user.username,
                "display-name": user_dto.user.username,
                "displayname": user_dto.user.username,
                "email": user_dto.user.email,
                "phone": "",
                "address": "",
                "website": "",
                "twitter": "",
                "groups": groups,
                "language": "en",
                "locale": "en_US",
                "backend": backend,
                "lastLogin": last_login,
                "quota": {
                    "used": quota.0,
                    "total": quota.1,
                    "free": free,
                    "relative": relative
                }
            }
        }
    }))
    .into_response()
}

pub async fn handle_revoke_apppassword(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    headers: axum::http::HeaderMap,
) -> Response {
    let nextcloud = match state.nextcloud.as_ref() {
        Some(nextcloud) => nextcloud,
        None => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };

    let app_password = match extract_basic_password(&headers) {
        Some(password) => password,
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };

    if let Err(e) = nextcloud
        .app_passwords
        .revoke_by_password(user.id, &app_password)
        .await
    {
        tracing::warn!("Failed to revoke app password for {}: {}", user.id, e);
    }

    Json(ocs_ok(200, json!({}))).into_response()
}

pub async fn handle_notifications_list() -> Response {
    Json(ocs_ok(200, json!([]))).into_response()
}

pub async fn handle_notifications_push() -> Response {
    Json(ocs_ok(200, json!({}))).into_response()
}

/// GET /ocs/v2.php/apps/recommendations/api/v1/recommendations
///
/// Returns recommended files. Stub that returns an empty list.
pub async fn handle_recommendations() -> Response {
    Json(json!({
        "ocs": {
            "meta": { "status": "ok", "statuscode": 200, "message": "OK" },
            "data": []
        }
    }))
    .into_response()
}

/// GET /ocs/v2.php/apps/files_sharing/api/v1/sharees?search={query}&itemType={type}
///
/// Returns matching users for the sharing autocomplete UI.
/// Even though sharing is disabled, the Nextcloud mobile app still calls
/// this endpoint and expects a well-formed OCS response rather than a 404.
pub async fn handle_sharees_search(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    axum::extract::Query(params): axum::extract::Query<ShareeSearchParams>,
) -> Response {
    let search = params.search.unwrap_or_default();
    if search.is_empty() {
        return sharees_response(vec![]).into_response();
    }

    let auth_service = match state.auth_service.as_ref() {
        Some(svc) => &svc.auth_application_service,
        None => return sharees_response(vec![]).into_response(),
    };

    // SQL-level ILIKE search with limit — avoids loading all users into
    // memory. Username-only projection: the wide `search_users` row drags
    // the up-to-512 KiB avatar `image` per matched user, per keystroke
    // (benches/ROUND12.md §1). NULL-username (email-only signup) rows are
    // already filtered by the service, preserving the old post-limit
    // filtering semantics.
    let usernames = auth_service
        .search_sharee_usernames(&search, 26)
        .await
        .unwrap_or_default();

    // Skip self (don't suggest sharing with yourself).
    let matches: Vec<serde_json::Value> = usernames
        .into_iter()
        .filter_map(|handle| {
            if handle.as_str() == &*user.username {
                return None;
            }
            Some(json!({
                "label": handle,
                "value": {
                    "shareType": 0,
                    "shareWith": handle,
                }
            }))
        })
        .take(25)
        .collect();

    sharees_response(matches).into_response()
}

#[derive(serde::Deserialize)]
pub struct ShareeSearchParams {
    search: Option<String>,
    #[serde(rename = "itemType")]
    #[allow(dead_code)]
    item_type: Option<String>,
    #[serde(rename = "perPage")]
    #[allow(dead_code)]
    per_page: Option<u32>,
}

fn sharees_response(users: Vec<serde_json::Value>) -> Json<serde_json::Value> {
    Json(json!({
        "ocs": {
            "meta": { "status": "ok", "statuscode": 200, "message": "OK" },
            "data": {
                "exact": { "users": [], "groups": [], "remotes": [] },
                "users": users,
                "groups": [],
                "remotes": []
            }
        }
    }))
}

/// GET /ocs/v2.php/search/providers
///
/// Returns the list of available Unified Search providers.
/// We only expose the "files" provider.
pub async fn handle_search_providers() -> Response {
    Json(json!({
        "ocs": {
            "meta": { "status": "ok", "statuscode": 200, "message": "OK" },
            "data": [
                {
                    "id": "files",
                    "appId": "files",
                    "name": "Files",
                    "icon": "/apps/files/img/app.svg",
                    "order": 5,
                    "filters": {},
                    "isPaginated": false
                }
            ]
        }
    }))
    .into_response()
}

/// GET /ocs/v2.php/search/providers/{provider_id}/search?term=…&limit=…&cursor=…
///
/// Executes a Unified Search query against the given provider.
/// Only the "files" provider is implemented; all others return empty results.
pub async fn handle_search(
    State(state): State<Arc<AppState>>,
    Path(provider_id): Path<String>,
    axum::extract::Query(params): axum::extract::Query<UnifiedSearchParams>,
    user: AuthUser,
) -> Response {
    // Only the "files" provider is supported
    if provider_id != "files" {
        return empty_search_response().into_response();
    }

    let search_service = match state.applications.search_service.as_ref() {
        Some(svc) => svc,
        None => return empty_search_response().into_response(),
    };

    let term = params.term.unwrap_or_default();
    if term.is_empty() {
        return empty_search_response().into_response();
    }

    let criteria = SearchCriteriaDto {
        name_contains: Some(term),
        recursive: true,
        limit: params.limit.unwrap_or(25),
        ..SearchCriteriaDto::default()
    };

    let results = match search_service.search(criteria, user.id).await {
        Ok(r) => r,
        Err(_) => return empty_search_response().into_response(),
    };

    let file_id_svc = state.nextcloud.as_ref().map(|n| &n.file_ids);

    // Pre-resolve numeric ids for every file result in a single batch query
    // (was one INSERT round-trip per result).
    let file_uuids: Vec<&str> = results.files.iter().map(|f| f.id.as_str()).collect();
    let file_id_map: HashMap<uuid::Uuid, i64> = match file_id_svc {
        Some(svc) => svc
            .get_or_create_file_ids(&file_uuids)
            .await
            .unwrap_or_default(),
        None => HashMap::new(),
    };

    let mut entries: Vec<serde_json::Value> = Vec::new();

    // Map file results.
    //
    // `strip_drive_root_segment` handles both default and secondary
    // drives — post-D0 the first path segment is the drive's root
    // folder name (`"Personal"` for D0-provisioned defaults, the
    // original sibling-root name for M2 backfilled secondaries).
    // Read-scope is upstream in `state.applications.search_service`;
    // this handler only formats display paths.
    for file in &results.files {
        let display_path =
            crate::interfaces::nextcloud::webdav_handler::strip_drive_root_segment(&file.path);
        let display_path = format!("/{}", display_path);

        let numeric_id =
            crate::interfaces::nextcloud::webdav_handler::nc_id_of(&file_id_map, &file.id);

        let thumbnail_url = match numeric_id {
            Some(nid) => format!("/index.php/core/preview?fileId={}&x=32&y=32", nid),
            None => String::new(),
        };
        let resource_url = match numeric_id {
            Some(nid) => format!("/f/{}", nid),
            None => String::new(),
        };

        entries.push(json!({
            "thumbnailUrl": thumbnail_url,
            "title": file.name,
            "subline": display_path,
            "resourceUrl": resource_url,
            "icon": "",
            "rounded": false
        }));
    }

    // Map folder results — same drive-agnostic strip as above.
    for folder in &results.folders {
        let display_path =
            crate::interfaces::nextcloud::webdav_handler::strip_drive_root_segment(&folder.path);
        let display_path = format!("/{}", display_path);

        entries.push(json!({
            "thumbnailUrl": "",
            "title": folder.name,
            "subline": display_path,
            "resourceUrl": "",
            "icon": "/apps/files/img/folder.svg",
            "rounded": false
        }));
    }

    Json(json!({
        "ocs": {
            "meta": { "status": "ok", "statuscode": 200, "message": "OK" },
            "data": {
                "name": "Files",
                "isPaginated": false,
                "entries": entries,
                "cursor": null
            }
        }
    }))
    .into_response()
}

#[derive(serde::Deserialize)]
pub struct UnifiedSearchParams {
    term: Option<String>,
    limit: Option<usize>,
    #[allow(dead_code)]
    cursor: Option<String>,
}

fn empty_search_response() -> Json<serde_json::Value> {
    Json(json!({
        "ocs": {
            "meta": { "status": "ok", "statuscode": 200, "message": "OK" },
            "data": {
                "name": "Files",
                "isPaginated": false,
                "entries": [],
                "cursor": null
            }
        }
    }))
}

/// Build the capabilities JSON tree from its three config inputs. Public
/// only under the `bench` feature caller path via
/// [`capabilities_payload_for_bench`]; production reaches it once through
/// the [`CAPABILITIES_BODIES`] init.
fn capabilities_payload(
    base_url: &str,
    emulated_version: (u32, u32, u32),
    version_string: &str,
    ocs_version: u8,
) -> serde_json::Value {
    let statuscode = if ocs_version == 1 { 100 } else { 200 };
    let (nc_major, nc_minor, nc_micro) = emulated_version;
    let nc_version_str = version_string;

    json!({
        "ocs": {
            "meta": {
                "status": "ok",
                "statuscode": statuscode,
                "message": "OK"
            },
            "data": {
                "version": {
                    "major": nc_major,
                    "minor": nc_minor,
                    "micro": nc_micro,
                    "string": nc_version_str,
                    "edition": "",
                    "extendedSupport": false
                },
                "capabilities": {
                    "core": {
                        "pollinterval": 60,
                        "webdav-root": "remote.php/dav",
                        "reference-api": false,
                        "reference-regex": ""
                    },
                    "files": {
                        "bigfilechunking": true,
                        "favorites": true,
                        "undelete": true,
                        "versioning": false
                    },
                    "dav": {
                        "chunking": "1.0"
                    },
                    "checksums": {
                        "preferredUploadType": "",
                        "supportedTypes": []
                    },
                    "files_sharing": {
                        "api_enabled": false,
                        "public": { "enabled": false },
                        "user": { "send_mail": false },
                        "resharing": false
                    },
                    "notifications": {
                        "ocs-endpoints": ["list", "get", "delete", "delete-all"]
                    },
                    "theming": {
                        "name": "OxiCloud",
                        "url": base_url,
                        "logo": format!("{}/logo.png", base_url),
                        "color": "#0082c9",
                        "color-text": "#ffffff",
                        "color-element": "#0082c9",
                        "color-element-bright": "#0082c9",
                        "color-element-dark": "#0082c9",
                        "background": "#0082c9",
                        "background-plain": true,
                        "background-default": true,
                        "logoheader": format!("{}/logo.png", base_url),
                        "favicon": format!("{}/favicon.ico", base_url)
                    }
                }
            }
        }
    })
}

/// Bench-only public wrapper (feature = "bench") over the private payload
/// builder so `examples/bench_capabilities_static.rs` can A/B the
/// rebuild-per-poll flow against the memoized bytes.
#[cfg(feature = "bench")]
pub fn capabilities_payload_for_bench(
    base_url: &str,
    emulated_version: (u32, u32, u32),
    version_string: &str,
    ocs_version: u8,
) -> serde_json::Value {
    capabilities_payload(base_url, emulated_version, version_string, ocs_version)
}

fn extract_basic_password(headers: &axum::http::HeaderMap) -> Option<String> {
    let value = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    super::basic_auth_middleware::parse_basic_auth(value).map(|(_, pass)| pass)
}
