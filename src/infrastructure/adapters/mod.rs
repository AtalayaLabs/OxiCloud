//! Infrastructure Adapters
//!
//! This module contains adapters that bridge the gap between domain repositories
//! and application ports. These adapters implement the application layer ports
//! using the infrastructure layer repositories.
//!
//! It also includes error adapters for converting infrastructure-specific errors
//! to domain errors, following Clean Architecture principles.

pub mod calendar_storage_adapter;
pub mod contact_storage_adapter;
pub mod error_adapters;
pub mod music_storage_adapter;
#[cfg(feature = "opencloudmesh")]
pub mod ocm_adapters;

pub use calendar_storage_adapter::CalendarStorageAdapter;
pub use contact_storage_adapter::ContactStorageAdapter;
pub use error_adapters::IntoDomainError;
pub use music_storage_adapter::MusicStorageAdapter;
