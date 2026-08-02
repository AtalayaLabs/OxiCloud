use sqlx::PgPool;
use std::sync::Arc;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

use crate::application::services::auth_application_service::AuthApplicationService;
use crate::application::services::user_lifecycle_service::UserLifecycleService;
use crate::common::config::AppConfig;
use crate::common::di::AuthServices;
use crate::domain::repositories::magic_link_token_repository::MagicLinkTokenRepository;
use crate::infrastructure::repositories::pg::MagicLinkTokenPgRepository;
use crate::infrastructure::repositories::{SessionPgRepository, UserPgRepository};
use crate::infrastructure::services::jwt_service::JwtTokenService;
use crate::infrastructure::services::oidc_service::OidcService;
use crate::infrastructure::services::password_hasher::Argon2PasswordHasher;

pub async fn create_auth_services(
    config: &AppConfig,
    pool: Arc<PgPool>,
    user_lifecycle: Arc<UserLifecycleService>,
) -> Result<AuthServices> {
    // Create JWT token service (TokenServicePort implementation)
    let token_service: Arc<JwtTokenService> = Arc::new(JwtTokenService::new(
        config.auth.jwt_secret.clone(),
        config.auth.access_token_expiry_secs,
        config.auth.refresh_token_expiry_secs,
    ));

    // Create password hashing service with configured Argon2id parameters
    let password_hasher = Arc::new(Argon2PasswordHasher::new(
        config.auth.hash_memory_cost,
        config.auth.hash_time_cost,
        config.auth.hash_parallelism,
    ));

    // Create PostgreSQL repositories
    let user_repository = Arc::new(UserPgRepository::new(pool.clone()));
    let session_repository = Arc::new(SessionPgRepository::new(pool.clone()));

    // Create authentication application service
    let mut auth_app_service = AuthApplicationService::new(
        user_repository,
        session_repository,
        password_hasher,
        token_service.clone(),
        config.storage_path.clone(),
    );

    // Wire the user-lifecycle dispatcher. Home-folder provisioning is
    // now handled by PersonalDriveLifecycleHook (registered on the
    // dispatcher in DI) — AuthApplicationService no longer needs a
    // direct FolderService dependency for that path.
    auth_app_service = auth_app_service.with_user_lifecycle(user_lifecycle);

    // Wire the auth-method allowlist + email-verification requirement so
    // login / magic-link / register handlers consult a single snapshot
    // rather than reaching into the app config on every call.
    auth_app_service = auth_app_service.with_auth_policy(
        config.auth.allowed_auth_methods.clone(),
        config.auth.auth_policies.clone(),
        config.auth.require_verified_email,
    );

    // Wire the magic-link token repo. Enables `GET /magic/v1/{token}`
    // and the future `POST /api/auth/magic-link/send` endpoint to mint
    // and consume tokens. The repo is unconditional (it's just SQL on
    // an empty table when the feature is dormant); the feature kill
    // switch lives in `config.magic_link.allow_external_users`, checked
    // by the issuance side, not by the redemption side.
    let magic_link_repo: Arc<dyn MagicLinkTokenRepository> =
        Arc::new(MagicLinkTokenPgRepository::new(pool.clone()));
    auth_app_service = auth_app_service.with_magic_link_repo(magic_link_repo);

    // Configure OIDC service if enabled
    if config.oidc.enabled {
        tracing::info!(
            "Initializing OIDC service (provider: {}, issuer: {})",
            config.oidc.provider_name,
            config.oidc.issuer_url
        );

        let oidc_service = Arc::new(OidcService::new(config.oidc.clone()));
        auth_app_service = auth_app_service.with_oidc(oidc_service, config.oidc.clone());

        if config.oidc.disable_password_login {
            tracing::warn!("Password login is DISABLED — only OIDC authentication is allowed");
        }
    }

    // Package service in Arc
    let auth_application_service = Arc::new(auth_app_service);

    // Account lockout service — in-memory brute-force protection
    let login_lockout = Arc::new(
        crate::infrastructure::services::login_lockout_service::LoginLockoutService::new(
            config.auth.rate_limit.lockout_max_failures,
            config.auth.rate_limit.lockout_duration_secs,
            100_000, // Track up to 100k accounts concurrently
        ),
    );
    tracing::info!(
        "Login lockout service initialized: max {} failures, {}s lockout",
        config.auth.rate_limit.lockout_max_failures,
        config.auth.rate_limit.lockout_duration_secs,
    );

    Ok(AuthServices {
        token_service,
        auth_application_service,
        login_lockout,
    })
}
