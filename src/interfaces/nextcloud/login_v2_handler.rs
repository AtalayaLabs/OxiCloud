use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Json, Redirect, Response},
};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

use crate::common::di::AppState;
use crate::common::errors::DomainError;
use crate::interfaces::middleware::auth::CurrentUser;

pub async fn handle_login_initiate(State(state): State<Arc<AppState>>) -> Response {
    let nextcloud = match state.nextcloud.as_ref() {
        Some(nextcloud) => nextcloud,
        None => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };

    let base_url = state.core.config.base_url();
    let flow = match nextcloud.login_flow.initiate(&base_url) {
        Ok(flow) => flow,
        Err(_) => {
            tracing::warn!("Login Flow v2: too many pending flows, rejecting");
            return StatusCode::TOO_MANY_REQUESTS.into_response();
        }
    };

    tracing::info!(
        base_url = %base_url,
        login_url = %flow.login_url,
        poll_endpoint = %flow.poll_endpoint,
        "Login Flow v2 initiated"
    );

    Json(json!({
        "poll": {
            "token": flow.poll_token,
            "endpoint": flow.poll_endpoint,
        },
        "login": flow.login_url,
    }))
    .into_response()
}

pub async fn handle_login_poll(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    body: String,
) -> Response {
    let nextcloud = match state.nextcloud.as_ref() {
        Some(nextcloud) => nextcloud,
        None => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };

    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("(none)");

    tracing::debug!(
        body = %body,
        content_type = %content_type,
        query_has_token = query.contains_key("token"),
        "Login Flow v2 poll request"
    );

    // Try to extract token from multiple sources:
    // 1. Form-encoded body (token=xxx)
    // 2. JSON body ({"token": "xxx"})
    // 3. Query parameter (?token=xxx)
    let token = parse_form_value(&body, "token")
        .or_else(|| {
            serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("token")?.as_str().map(String::from))
        })
        .or_else(|| query.get("token").cloned());

    let token = match token {
        Some(token) => token,
        None => {
            tracing::warn!(
                body = %body,
                content_type = %content_type,
                "Login Flow v2 poll: could not extract token from body, JSON, or query"
            );
            return StatusCode::BAD_REQUEST.into_response();
        }
    };

    match nextcloud.login_flow.poll(&token) {
        Some(result) => {
            tracing::info!(
                login_name = %result.login_name,
                server = %result.server,
                "Login Flow v2 poll: returning completed credentials"
            );
            Json(json!({
                "server": result.server,
                "loginName": result.login_name,
                "appPassword": result.app_password,
            }))
            .into_response()
        }
        None => {
            tracing::debug!("Login Flow v2 poll: not yet completed");
            StatusCode::NOT_FOUND.into_response()
        }
    }
}

pub async fn handle_login_page(
    State(state): State<Arc<AppState>>,
    Path(token): Path<String>,
) -> Response {
    let nextcloud = match state.nextcloud.as_ref() {
        Some(nextcloud) => nextcloud,
        None => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };

    if !nextcloud.login_flow.flow_exists(&token) {
        return StatusCode::NOT_FOUND.into_response();
    }

    // The grant-access page is now owned by the SvelteKit SPA at
    // /nextcloud/login; redirect the client's browser there with the flow
    // token. The page POSTs back to /login/v2/flow/{token} (handle_login_submit).
    Redirect::to(&format!(
        "{}/nextcloud/login?token={token}",
        crate::common::config::server_base_path()
    ))
    .into_response()
}

pub async fn handle_login_submit(
    State(state): State<Arc<AppState>>,
    Path(token): Path<String>,
    body: String,
) -> Response {
    let nextcloud = match state.nextcloud.as_ref() {
        Some(nextcloud) => nextcloud,
        None => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };

    let params = parse_form(&body);
    let username = match params.get("user") {
        Some(value) if !value.is_empty() => value,
        _ => return StatusCode::BAD_REQUEST.into_response(),
    };
    let password = match params.get("password") {
        Some(value) if !value.is_empty() => value,
        _ => return StatusCode::BAD_REQUEST.into_response(),
    };

    let auth = match state.auth_service.as_ref() {
        Some(auth) => auth,
        None => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };

    let current_user = match auth
        .auth_application_service
        .verify_credentials(username, password)
        .await
    {
        Ok(user) => user,
        Err(e) => return login_failed_response(e),
    };

    complete_flow(
        &state,
        &nextcloud.login_flow,
        &token,
        &current_user,
        "Nextcloud",
    )
    .await
}

/// Complete an OIDC-authenticated NC Login Flow v2.
///
/// Called from the OIDC callback when the state carried an `nc_flow_token`.
/// NC clients pick up credentials via the poll endpoint (backchannel), so no
/// `nc://` frontchannel URL is emitted.
pub async fn handle_oidc_login_completion(
    state: &Arc<AppState>,
    token: &str,
    user_id: uuid::Uuid,
    username: &str,
) -> Response {
    let nextcloud = match state.nextcloud.as_ref() {
        Some(nc) => nc,
        None => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };

    let auth = match state.auth_service.as_ref() {
        Some(a) => a,
        None => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };

    // Full user record — needed to build the `CurrentUser` the shared
    // helpers expect (email + role in particular). We already have the
    // username from the OIDC claims, but not the rest.
    let user_dto = match auth.auth_application_service.get_user_by_id(user_id).await {
        Ok(u) => u,
        Err(e) => {
            tracing::error!(error = %e, %user_id, user = %username, "OIDC+NC: failed to fetch user by id");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let current_user = CurrentUser {
        id: user_id,
        username: std::sync::Arc::from(username),
        email: std::sync::Arc::from(user_dto.email.as_str()),
        role: smol_str::SmolStr::new(user_dto.role.as_str()),
        // NC login-flow-v2 mints an app password — no browser, no
        // WebCrypto, always unbound. DPoP middleware exempts.
        dpop_jkt: None,
    };

    complete_flow(
        state,
        &nextcloud.login_flow,
        token,
        &current_user,
        "Nextcloud (OIDC)",
    )
    .await
}

/// Mint an app password, complete the flow, and redirect to the success page.
async fn complete_flow(
    state: &Arc<AppState>,
    login_flow: &crate::application::services::nextcloud_login_flow_service::NextcloudLoginFlowService,
    token: &str,
    user: &CurrentUser,
    // Persisted verbatim as `auth.app_passwords.label`. Callers pass
    // `"Nextcloud"` for the password path and `"Nextcloud (OIDC)"` for
    // the OIDC path so operators can distinguish provenance from the
    // audit log alone.
    label: &str,
) -> Response {
    let nextcloud = match state.nextcloud.as_ref() {
        Some(nc) => nc,
        None => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };

    let app_password = match nextcloud.app_passwords.create_nc(user.id, label).await {
        Ok((_id, password)) => password,
        Err(e) => {
            tracing::error!(error = %e, user = %user.username, "Login Flow v2: failed to create app password");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let login_name = user.username.to_string();

    let base_url = state.core.config.base_url();
    let completed = login_flow.complete(token, &login_name, &base_url, &app_password);

    if completed {
        tracing::info!(
            user = %user.username,
            login_name = %login_name,
            base_url = %base_url,
            "Login Flow v2: flow completed successfully"
        );
        // Redirect the browser to a visible success page. NC clients
        // that use the LFv2 poll endpoint (the standard pattern) have
        // already received the credentials server-to-server through
        // `login_flow.complete()` above — they don't need any browser
        // hand-off.
        //
        // We deliberately do NOT redirect to `nc://login/…` here:
        //   1. Plain browsers can't follow it, so the tab looks stuck.
        //   2. NC desktop clients that pick it up while their poll
        //      has already succeeded try to complete the flow a
        //      second time, which fails validation ("Impossible de
        //      valider la requête") — the poll session is fine, the
        //      dialog is spurious noise.
        //
        // If a client ever needs a frontchannel `nc://` handoff
        // (older NC releases, mobile), reintroduce the URL as a
        // client-side-only fragment (`#target=…`) and add a manual
        // "Open Nextcloud" fallback on the success page. Keep the
        // credentials out of the query string either way — the query
        // string reaches server access logs.
        axum::response::Redirect::to(&format!(
            "{}/nextcloud/success",
            crate::common::config::server_base_path()
        ))
        .into_response()
    } else {
        tracing::error!(
            user = %user.username,
            "Login Flow v2: complete() returned false — flow token not found"
        );
        axum::response::Redirect::to(&format!(
            "{}/nextcloud/error?type=session-expired",
            crate::common::config::server_base_path()
        ))
        .into_response()
    }
}

/// GET /login/v2/flow/{token}/oidc — Start an OIDC authorization flow that is
/// tied to a Nextcloud Login Flow v2 session.  After successful IdP
/// authentication the regular `/api/auth/oidc/callback` endpoint will detect
/// the NC flow token and complete the Nextcloud login instead of issuing
/// internal JWTs.
pub async fn handle_login_oidc(
    State(state): State<Arc<AppState>>,
    Path(token): Path<String>,
) -> Response {
    // Verify Nextcloud services are configured
    let nextcloud = match state.nextcloud.as_ref() {
        Some(nc) => nc,
        None => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };

    // Verify the NC login flow token exists
    if !nextcloud.login_flow.flow_exists(&token) {
        return axum::response::Redirect::to(&format!(
            "{}/nextcloud/error?type=session-expired",
            crate::common::config::server_base_path()
        ))
        .into_response();
    }

    // Verify auth + OIDC are configured and enabled
    let auth = match state.auth_service.as_ref() {
        Some(auth) => auth,
        None => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };

    if !auth.auth_application_service.oidc_enabled() {
        tracing::warn!("OIDC login requested on NC login page but OIDC is not enabled");
        return StatusCode::NOT_FOUND.into_response();
    }

    // Prepare an OIDC authorize flow that carries the NC flow token
    match auth
        .auth_application_service
        .prepare_oidc_authorize_for_nextcloud(&token)
        .await
    {
        Ok(authorize_url) => {
            tracing::info!("OIDC authorize redirect for Nextcloud Login Flow v2");
            axum::response::Redirect::temporary(&authorize_url).into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to prepare OIDC authorize for NC login");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

fn login_failed_response(_err: DomainError) -> Response {
    axum::response::Redirect::to(&format!(
        "{}/nextcloud/error?type=invalid-credentials",
        crate::common::config::server_base_path()
    ))
    .into_response()
}

fn parse_form(body: &str) -> HashMap<String, String> {
    body.split('&')
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            let key = urlencoding::decode(key).ok()?.to_string();
            let value = urlencoding::decode(value).ok()?.to_string();
            Some((key, value))
        })
        .collect()
}

fn parse_form_value(body: &str, key: &str) -> Option<String> {
    parse_form(body).remove(key)
}
