//! Thin administrative JSON boundary; live authorization belongs to ThemeService.
use crate::{
    application::dtos::theme_dto::{AppearanceDto, SelectThemeDto},
    common::di::AppState,
    interfaces::{errors::AppError, middleware::auth::AuthUser},
};
use axum::{
    Json,
    extract::State,
    routing::{get, post},
};
use std::sync::Arc;

use crate::interfaces::api::policy_router::GatedRouter;

pub fn routes() -> GatedRouter<Arc<AppState>> {
    GatedRouter::new()
        .route(
            "/settings/appearance",
            get(get_appearance).put(select_theme),
        )
        .route("/settings/appearance/reload", post(reload_themes))
}
#[utoipa::path(get, path = "/api/admin/settings/appearance", tag = "admin",
    responses((status = 200, body = AppearanceDto), (status = 401, description = "Authentication required"), (status = 403, description = "Administrator required")),
    security(("bearerAuth" = [])))]
pub async fn get_appearance(
    State(state): State<Arc<AppState>>,
    caller: AuthUser,
) -> Result<Json<AppearanceDto>, AppError> {
    let auth = state
        .auth_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Authentication unavailable"))?;
    Ok(Json(
        state
            .theme_service
            .get_appearance_with_perms(
                caller.id,
                auth.auth_application_service.as_ref(),
                state.authorization.as_ref(),
            )
            .await?,
    ))
}
#[utoipa::path(put, path = "/api/admin/settings/appearance", tag = "admin", request_body = SelectThemeDto,
    responses((status = 200, body = AppearanceDto), (status = 400, description = "InvalidInput"), (status = 401, description = "Authentication required"), (status = 403, description = "Administrator required"), (status = 500, description = "Persistence failed")),
    security(("bearerAuth" = [])))]
pub async fn select_theme(
    State(state): State<Arc<AppState>>,
    caller: AuthUser,
    Json(request): Json<SelectThemeDto>,
) -> Result<Json<AppearanceDto>, AppError> {
    let auth = state
        .auth_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Authentication unavailable"))?;
    Ok(Json(
        state
            .theme_service
            .select_theme_with_perms(
                caller.id,
                request.theme_id,
                auth.auth_application_service.as_ref(),
                state.authorization.as_ref(),
            )
            .await?,
    ))
}

#[utoipa::path(post, path = "/api/admin/settings/appearance/reload", tag = "admin",
    responses((status = 200, body = AppearanceDto), (status = 401, description = "Authentication required"), (status = 403, description = "Administrator required"), (status = 400, description = "Invalid active theme"), (status = 500, description = "Catalogue reload failed")), security(("bearerAuth" = [])))]
pub async fn reload_themes(
    State(state): State<Arc<AppState>>,
    caller: AuthUser,
) -> Result<Json<AppearanceDto>, AppError> {
    let auth = state
        .auth_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Authentication unavailable"))?;
    Ok(Json(
        state
            .theme_service
            .reload_with_perms(
                caller.id,
                auth.auth_application_service.as_ref(),
                state.authorization.as_ref(),
            )
            .await?,
    ))
}

#[cfg(test)]
mod tests {
    use utoipa::OpenApi;
    #[test]
    fn appearance_openapi_declares_the_enforced_admin_scope() {
        // The markers are applied by `mark_policies` from the compiled
        // table, not by the `ApiDoc` derive — so a spec straight from
        // `openapi()` carries none. Marked here with the one declaration
        // that matters, which keeps this a test of THIS operation's
        // documentation rather than of production wiring.
        let mut openapi = crate::interfaces::api::ApiDoc::openapi();
        let mut registry = crate::application::services::scope_table::PolicyRegistry::new();
        registry.declare(
            crate::application::services::scope_table::Site::RestPrefix("/api/admin"),
            vec![crate::application::ports::scope_ports::Condition::role(
                crate::application::ports::scope_ports::MinRole::Admin,
            )],
        );
        crate::interfaces::api::mark_policies(&mut openapi, &registry);
        let spec = serde_json::to_value(openapi).unwrap();
        let path = &spec["paths"]["/api/admin/settings/appearance"];
        for verb in ["get", "put"] {
            // Regression pin: HTTP bearer scopes must stay empty; the admin
            // marker is DERIVED from the compiled policy, so it cannot
            // disagree with what the gate enforces.
            assert_eq!(
                path[verb]["security"][0]["bearerAuth"],
                serde_json::json!([])
            );
            // `minrole`, not `scope`: a role floor and a credential scope
            // are different axes, and one key used to carry both.
            assert_eq!(
                path[verb][crate::interfaces::api::MINROLE_EXTENSION],
                "admin"
            );
            assert!(path[verb]["responses"]["200"]["content"]["application/json"].is_object());
        }
        assert!(path["put"]["requestBody"].is_object());
    }
}
