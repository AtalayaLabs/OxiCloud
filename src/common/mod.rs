pub mod config;
pub mod config_file;
pub mod di;
pub mod errors;
pub mod fmt;
pub mod locale;
pub mod migration_progress;
pub mod mime_detect;
pub mod runtime;
pub mod spec_tag;
// Tier-1 posture pin for the native-only TLS strategy. The file carries
// no runtime code — only `#[cfg(test)]` modules — but it must be in the
// module tree for `cargo test` to pick it up. Hidden behind `cfg(test)`
// so release builds don't carry a dead module.
pub mod stubs;
pub mod text;
#[cfg(test)]
pub mod tls_posture;
pub mod username_migration;
