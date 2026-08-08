use std::sync::Arc;
use uuid::Uuid;

use crate::application::dtos::settings_dto::{
    OidcSettingsDto, OidcTestResultDto, SaveOidcSettingsDto, TestOidcConnectionDto,
};
use crate::application::services::auth_application_service::AuthApplicationService;
use crate::common::config::OidcConfig;
use crate::common::errors::{DomainError, ErrorKind};
use crate::domain::repositories::settings_repository::SettingsRepository;
use crate::infrastructure::repositories::pg::SettingsPgRepository;
use crate::infrastructure::services::oidc_service::OidcService;

/// Admin settings service — manages platform configuration in the database.
///
/// Configuration priority: **env vars > DB settings > defaults**.
/// Supports hot-reloading OIDC configuration without server restart.
pub struct AdminSettingsService {
    settings_repo: Arc<SettingsPgRepository>,
    env_oidc_config: OidcConfig,
    auth_app_service: Arc<AuthApplicationService>,
    server_base_url: String,
}

impl AdminSettingsService {
    pub fn new(
        settings_repo: Arc<SettingsPgRepository>,
        env_oidc_config: OidcConfig,
        auth_app_service: Arc<AuthApplicationService>,
        server_base_url: String,
    ) -> Self {
        Self {
            settings_repo,
            env_oidc_config,
            auth_app_service,
            server_base_url,
        }
    }

    /// Auto-generated OIDC callback URL
    fn callback_url(&self) -> String {
        let base = self.server_base_url.trim_end_matches('/');
        format!("{}/api/auth/oidc/callback", base)
    }

    /// Detect which OIDC fields are overridden by environment variables
    fn get_env_overrides(&self) -> Vec<String> {
        let mut out = Vec::new();
        let vars = [
            ("OXICLOUD_OIDC_ENABLED", "enabled"),
            ("OXICLOUD_OIDC_ISSUER_URL", "issuer_url"),
            ("OXICLOUD_OIDC_CLIENT_ID", "client_id"),
            ("OXICLOUD_OIDC_CLIENT_SECRET", "client_secret"),
            ("OXICLOUD_OIDC_SCOPES", "scopes"),
            ("OXICLOUD_OIDC_AUTO_PROVISION", "auto_provision"),
            (
                "OXICLOUD_OIDC_AUTO_LINK_EMAIL_MATCH",
                "auto_link_email_match",
            ),
            ("OXICLOUD_OIDC_ADMIN_GROUPS", "admin_groups"),
            (
                "OXICLOUD_OIDC_DISABLE_PASSWORD_LOGIN",
                "disable_password_login",
            ),
            ("OXICLOUD_OIDC_PROVIDER_NAME", "provider_name"),
        ];
        for (env_key, field_name) in &vars {
            if std::env::var(env_key).is_ok() {
                out.push(field_name.to_string());
            }
        }
        out
    }

    /// Apply environment variable overrides on top of a config
    fn apply_env_overrides(&self, config: &mut OidcConfig) {
        let e = &self.env_oidc_config;
        if std::env::var("OXICLOUD_OIDC_ENABLED").is_ok() {
            config.enabled = e.enabled;
        }
        if std::env::var("OXICLOUD_OIDC_ISSUER_URL").is_ok() {
            config.issuer_url = e.issuer_url.clone();
        }
        if std::env::var("OXICLOUD_OIDC_CLIENT_ID").is_ok() {
            config.client_id = e.client_id.clone();
        }
        if std::env::var("OXICLOUD_OIDC_CLIENT_SECRET").is_ok() {
            config.client_secret = e.client_secret.clone();
        }
        if std::env::var("OXICLOUD_OIDC_SCOPES").is_ok() {
            config.scopes = e.scopes.clone();
        }
        if std::env::var("OXICLOUD_OIDC_REDIRECT_URI").is_ok() {
            config.redirect_uri = e.redirect_uri.clone();
        }
        if std::env::var("OXICLOUD_OIDC_FRONTEND_URL").is_ok() {
            config.frontend_url = e.frontend_url.clone();
        }
        if std::env::var("OXICLOUD_OIDC_AUTO_PROVISION").is_ok() {
            config.auto_provision = e.auto_provision;
        }
        if std::env::var("OXICLOUD_OIDC_AUTO_LINK_EMAIL_MATCH").is_ok() {
            config.auto_link_email_match = e.auto_link_email_match;
        }
        if std::env::var("OXICLOUD_OIDC_ADMIN_GROUPS").is_ok() {
            config.admin_groups = e.admin_groups.clone();
        }
        if std::env::var("OXICLOUD_OIDC_DISABLE_PASSWORD_LOGIN").is_ok() {
            config.disable_password_login = e.disable_password_login;
        }
        if std::env::var("OXICLOUD_OIDC_PROVIDER_NAME").is_ok() {
            config.provider_name = e.provider_name.clone();
        }
    }

    /// Load the effective OIDC config: DB settings + env var overrides + defaults.
    pub async fn load_effective_oidc_config(&self) -> Result<OidcConfig, DomainError> {
        let db = self.settings_repo.get_by_category("oidc").await?;
        let d = OidcConfig::default();

        let mut config = OidcConfig {
            enabled: db
                .get("oidc.enabled")
                .and_then(|v| v.parse().ok())
                .unwrap_or(d.enabled),
            issuer_url: db.get("oidc.issuer_url").cloned().unwrap_or(d.issuer_url),
            client_id: db.get("oidc.client_id").cloned().unwrap_or(d.client_id),
            client_secret: db
                .get("oidc.client_secret")
                .cloned()
                .unwrap_or(d.client_secret),
            redirect_uri: self.callback_url(),
            scopes: db.get("oidc.scopes").cloned().unwrap_or(d.scopes),
            frontend_url: self.server_base_url.clone(),
            auto_provision: db
                .get("oidc.auto_provision")
                .and_then(|v| v.parse().ok())
                .unwrap_or(d.auto_provision),
            admin_groups: db
                .get("oidc.admin_groups")
                .cloned()
                .unwrap_or(d.admin_groups),
            disable_password_login: db
                .get("oidc.disable_password_login")
                .and_then(|v| v.parse().ok())
                .unwrap_or(d.disable_password_login),
            provider_name: db
                .get("oidc.provider_name")
                .cloned()
                .unwrap_or(d.provider_name),
            auto_link_email_match: db
                .get("oidc.auto_link_email_match")
                .and_then(|v| v.parse().ok())
                .unwrap_or(d.auto_link_email_match),
        };

        // Env vars override DB
        self.apply_env_overrides(&mut config);
        Ok(config)
    }

    /// Get OIDC settings for display in admin UI (secrets masked).
    pub async fn get_oidc_settings(&self) -> Result<OidcSettingsDto, DomainError> {
        let db = self.settings_repo.get_by_category("oidc").await?;

        let has_secret = db
            .get("oidc.client_secret")
            .map(|s| !s.is_empty())
            .unwrap_or(false)
            || std::env::var("OXICLOUD_OIDC_CLIENT_SECRET")
                .map(|s| !s.is_empty())
                .unwrap_or(false);

        // Load effective config with env var overrides applied
        let effective = self.load_effective_oidc_config().await?;

        Ok(OidcSettingsDto {
            enabled: effective.enabled,
            issuer_url: effective.issuer_url,
            client_id: effective.client_id,
            client_secret_set: has_secret,
            scopes: effective.scopes,
            auto_provision: effective.auto_provision,
            admin_groups: effective.admin_groups,
            disable_password_login: effective.disable_password_login,
            provider_name: effective.provider_name,
            callback_url: effective.redirect_uri.clone(),
            env_overrides: self.get_env_overrides(),
        })
    }

    /// Save OIDC settings to DB and hot-reload the OIDC service.
    pub async fn save_oidc_settings(
        &self,
        dto: SaveOidcSettingsDto,
        updated_by: Uuid,
    ) -> Result<(), DomainError> {
        let cat = "oidc";
        let by = Some(updated_by);

        self.settings_repo
            .set("oidc.enabled", &dto.enabled.to_string(), cat, false, by)
            .await?;
        self.settings_repo
            .set("oidc.issuer_url", &dto.issuer_url, cat, false, by)
            .await?;
        self.settings_repo
            .set("oidc.client_id", &dto.client_id, cat, false, by)
            .await?;

        if let Some(ref secret) = dto.client_secret
            && !secret.is_empty()
        {
            self.settings_repo
                .set("oidc.client_secret", secret, cat, true, by)
                .await?;
        }
        if let Some(ref v) = dto.scopes {
            self.settings_repo
                .set("oidc.scopes", v, cat, false, by)
                .await?;
        }
        if let Some(v) = dto.auto_provision {
            self.settings_repo
                .set("oidc.auto_provision", &v.to_string(), cat, false, by)
                .await?;
        }
        if let Some(ref v) = dto.admin_groups {
            self.settings_repo
                .set("oidc.admin_groups", v, cat, false, by)
                .await?;
        }
        if let Some(v) = dto.disable_password_login {
            self.settings_repo
                .set(
                    "oidc.disable_password_login",
                    &v.to_string(),
                    cat,
                    false,
                    by,
                )
                .await?;
        }
        if let Some(ref v) = dto.provider_name {
            self.settings_repo
                .set("oidc.provider_name", v, cat, false, by)
                .await?;
        }

        // Hot-reload OIDC service
        let eff = self.load_effective_oidc_config().await?;
        if eff.enabled
            && !eff.issuer_url.is_empty()
            && !eff.client_id.is_empty()
            && !eff.client_secret.is_empty()
        {
            let svc = Arc::new(OidcService::new(eff.clone()));
            self.auth_app_service.reload_oidc(svc, eff);
            tracing::info!("OIDC service hot-reloaded with new configuration");
        } else if !eff.enabled {
            self.auth_app_service.disable_oidc();
            tracing::info!("OIDC service disabled via admin panel");
        }

        Ok(())
    }

    /// Test OIDC connection by fetching the discovery document.
    pub async fn test_oidc_connection(
        &self,
        dto: TestOidcConnectionDto,
    ) -> Result<OidcTestResultDto, DomainError> {
        let issuer = dto.issuer_url.trim_end_matches('/');
        let discovery_url = format!("{}/.well-known/openid-configuration", issuer);

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| {
                DomainError::new(
                    ErrorKind::InternalError,
                    "OIDC",
                    format!("HTTP client error: {}", e),
                )
            })?;

        let resp = match client.get(&discovery_url).send().await {
            Ok(r) => r,
            Err(e) => {
                return Ok(OidcTestResultDto {
                    success: false,
                    message: format!(
                        "Cannot reach the OIDC provider: {}. Check your Issuer URL.",
                        e
                    ),
                    issuer: None,
                    authorization_endpoint: None,
                    token_endpoint: None,
                    userinfo_endpoint: None,
                    provider_name_suggestion: None,
                });
            }
        };

        if !resp.status().is_success() {
            return Ok(OidcTestResultDto {
                success: false,
                message: format!(
                    "OIDC discovery returned HTTP {} — the Issuer URL may be incorrect.",
                    resp.status()
                ),
                issuer: None,
                authorization_endpoint: None,
                token_endpoint: None,
                userinfo_endpoint: None,
                provider_name_suggestion: None,
            });
        }

        #[derive(serde::Deserialize)]
        struct Discovery {
            issuer: Option<String>,
            authorization_endpoint: Option<String>,
            token_endpoint: Option<String>,
            userinfo_endpoint: Option<String>,
        }

        let disc: Discovery = match resp.json().await {
            Ok(d) => d,
            Err(e) => {
                return Ok(OidcTestResultDto {
                    success: false,
                    message: format!("Invalid discovery document: {}", e),
                    issuer: None,
                    authorization_endpoint: None,
                    token_endpoint: None,
                    userinfo_endpoint: None,
                    provider_name_suggestion: None,
                });
            }
        };

        // Suggest provider name from hostname
        let suggestion = issuer
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .split('/')
            .next()
            .and_then(|host| {
                let parts: Vec<&str> = host.split('.').collect();
                let name = if parts.len() >= 2 { parts[0] } else { host };
                let mut c = name.chars();
                c.next().map(|f| f.to_uppercase().to_string() + c.as_str())
            });

        Ok(OidcTestResultDto {
            success: true,
            message: "OIDC provider is reachable and returned a valid discovery document.".into(),
            issuer: disc.issuer,
            authorization_endpoint: disc.authorization_endpoint,
            token_endpoint: disc.token_endpoint,
            userinfo_endpoint: disc.userinfo_endpoint,
            provider_name_suggestion: suggestion,
        })
    }

    // ========================================================================
    // System Initialization
    // ========================================================================

    /// Check if the system has been initialized (first admin created).
    /// Returns `true` if the `system_initialized` flag is set to `"true"` in the DB.
    pub async fn is_system_initialized(&self) -> bool {
        match self.settings_repo.get("system_initialized").await {
            Ok(Some(val)) => val == "true",
            _ => false, // fail-closed: if DB error or key absent, system is NOT initialized
        }
    }

    /// Mark the system as initialized after the first admin is created.
    pub async fn mark_system_initialized(&self, admin_user_id: Uuid) -> Result<(), DomainError> {
        self.settings_repo
            .set(
                "system_initialized",
                "true",
                "system",
                false,
                Some(admin_user_id),
            )
            .await
    }

    /// Atomically try to claim system initialization.
    ///
    /// Returns `Ok(true)` if this call was the one that marked the system as
    /// initialized (the caller "won" the race), or `Ok(false)` if another
    /// request already did it.  This eliminates the race-condition window
    /// between `is_system_initialized()` and `mark_system_initialized()`.
    pub async fn try_claim_initialization(&self, admin_user_id: Uuid) -> Result<bool, DomainError> {
        self.settings_repo
            .try_claim_initialization(admin_user_id)
            .await
    }

    // ========================================================================
    // Registration Control
    // ========================================================================

    /// Check if public self-registration is enabled.
    /// Priority: env var `OXICLOUD_DISABLE_REGISTRATION` > DB setting > default (true).
    pub async fn get_registration_enabled(&self) -> bool {
        // Env var override takes priority
        if let Ok(val) = std::env::var("OXICLOUD_DISABLE_REGISTRATION") {
            return !matches!(val.to_lowercase().as_str(), "true" | "1" | "yes");
        }
        // Check DB setting
        match self.settings_repo.get("registration_enabled").await {
            Ok(Some(val)) => val == "true",
            _ => true, // default: enabled
        }
    }

    /// Enable or disable public self-registration.
    pub async fn set_registration_enabled(
        &self,
        enabled: bool,
        updated_by: Uuid,
    ) -> Result<(), DomainError> {
        self.settings_repo
            .set(
                "registration_enabled",
                if enabled { "true" } else { "false" },
                "general",
                false,
                Some(updated_by),
            )
            .await
    }
}
