#![allow(async_fn_in_trait)]

// Export the main project modules
pub mod application;
pub mod common;
pub mod domain;
pub mod infrastructure;
pub mod interfaces;

// Operator-tools subcommand tree, dispatched from `src/main.rs` when
// the first positional arg matches a known domain (`opaque`, `migrate`).
// Previously lived in a standalone `oxicloud-cli` binary; folded in so
// the release tarball ships one executable — see
// docs/plan/bundled-binary.md § Deliverable 1b.
pub mod cli;

// Test-only helpers for #[cfg(integration_tests)] modules across the
// crate (shared pool URL guard + pre-suite cleanup OnceCell).
#[cfg(integration_tests)]
pub mod integration_test_support;

// Shared testcontainers-backed harness for external-mount integration tests.
// Gated on `test` too because it links the `testcontainers` dev-dependency,
// which is only available to test targets (not the plain lib build).
#[cfg(all(test, integration_tests))]
mod mount_it_support;

// Phase 0 perf-benchmark support: deterministic image corpus generation/loading
// shared by `benches/thumbnails.rs` and `examples/bench_thumbnails_mem.rs`.
// Gated behind the `bench` feature so it adds nothing to normal builds.
#[cfg(feature = "bench")]
pub mod bench_support;

// Common public re-exports
pub use application::services::folder_service::FolderService;
pub use application::services::i18n_application_service::I18nApplicationService;
pub use domain::services::path_service::StoragePath;
pub use infrastructure::services::path_service::PathService;
