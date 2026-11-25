pub mod authorization;
pub mod email_normalize;
pub mod external_mount_id;
pub mod i18n_service;
pub mod name_conflict;
pub mod path_service;

// NOTE: auth_service has been moved to infrastructure/services/jwt_service.rs
// The functionality is now exposed through application/ports/auth_ports.rs (TokenServicePort)
