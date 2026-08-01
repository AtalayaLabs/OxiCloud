//! Factory that builds a `BlobStorageBackend` from a `NamedStorageEntry`.
//!
//! Central to `docs/plan/storage-multi-entry.md`: the same function is
//! called by the boot path (to build the LIVE backend for the active
//! entry) and by the migration handler (to build a target backend for
//! any named entry). Keeping one factory means the encryption-decorator
//! wrapping decision is expressed exactly once — no chance of the
//! migration copy path silently omitting encryption while boot applies
//! it (or vice versa).
//!
//! What this factory does NOT do:
//! - Retry decorator — applied per-app-instance in `common/di.rs`
//!   because policy comes from `AppConfig.storage.retry`, not the
//!   entry. If per-entry retry becomes a need, add a
//!   `RetryConfig` field to `NamedStorageEntry` and move the
//!   wrapping in here.
//! - Cache decorator — same story: cache path/size are ambient
//!   `AppConfig.storage.cache` settings, not per-entry.
//!
//! So the returned backend is `base [+ encryption]` — the two layers
//! whose choice is tied to the entry itself. The caller stacks any
//! remaining decorators.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use sqlx::PgPool;

use crate::application::ports::blob_storage_ports::BlobStorageBackend;
use crate::common::config::{NamedStorageEntry, StorageBackendType};

/// Key in `auth.admin_settings` that holds the currently-active
/// storage entry's name. Single source of truth for runtime backend
/// selection (see `docs/plan/storage-multi-entry.md` §"One DB row").
pub const ACTIVE_BACKEND_NAME_KEY: &str = "storage.active_backend_name";

/// Result of [`resolve_active_entry`].
pub enum ActiveEntry<'a> {
    /// DB has an `active_backend_name` set AND that name matches an
    /// entry declared in the current env. Boot uses this entry.
    Explicit(&'a NamedStorageEntry),
    /// DB has NO `active_backend_name` set (fresh install, or the row
    /// was intentionally cleared). Caller falls back to a sensible
    /// default — typically the first entry in `_ENTRIES` order.
    Unset,
}

/// Look up the entry the app should boot with.
///
/// Returns:
/// - `Ok(ActiveEntry::Explicit(entry))` when DB has a value AND that
///   value names an entry in `entries`.
/// - `Ok(ActiveEntry::Unset)` when the DB row is absent (never
///   written). Caller decides the fallback.
/// - `Err(msg)` when the DB row IS set but the named entry is missing
///   from the current env (deploy drift — someone removed an entry
///   from `.env` or renamed it). The error message names the missing
///   entry, lists the available ones, and points at the
///   `oxicloud --select-storage <name>` repair flag. Boot must abort
///   on this — silently falling back to a different entry would move
///   the app's live backend without operator consent.
pub async fn resolve_active_entry<'a>(
    pool: &PgPool,
    entries: &'a [NamedStorageEntry],
) -> Result<ActiveEntry<'a>, String> {
    let stored: Option<String> =
        sqlx::query_scalar("SELECT value FROM auth.admin_settings WHERE key = $1")
            .bind(ACTIVE_BACKEND_NAME_KEY)
            .fetch_optional(pool)
            .await
            .map_err(|e| {
                format!("reading `{ACTIVE_BACKEND_NAME_KEY}` from auth.admin_settings failed: {e}")
            })?;

    match stored {
        None => Ok(ActiveEntry::Unset),
        Some(name) => match entries.iter().find(|e| e.name == name) {
            Some(entry) => Ok(ActiveEntry::Explicit(entry)),
            None => {
                let available = if entries.is_empty() {
                    "(none — no OXICLOUD_STORAGE_ENTRIES declared)".to_string()
                } else {
                    entries
                        .iter()
                        .map(|e| e.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                Err(format!(
                    "auth.admin_settings.storage.active_backend_name = `{name}`, but no entry \
                     with that name is declared in OXICLOUD_STORAGE_ENTRIES. Available: [{available}]. \
                     Either add `{name}` back to your .env, or repair the DB pointer with:\n    \
                     oxicloud --select-storage <one-of-the-available-names>"
                ))
            }
        },
    }
}

/// Build a `BlobStorageBackend` matching the given entry, with the
/// encryption decorator applied when the entry declares a key.
///
/// `local_storage_path_fallback` is the ambient `AppConfig.storage_path`
/// — used for a Local entry when `entry.root_dir` is `None`. Matches
/// the fallback rule documented in
/// `docs/plan/storage-multi-entry.md` §Legacy: per-entry `_ROOT_DIR`
/// falls back to `OXICLOUD_STORAGE_PATH` for Local entries when unset.
///
/// Panics with a targeted message on the two configuration errors that
/// slip past env-parse-time validation:
/// - S3 entry with `entry.s3 == None` — parser invariant violated.
/// - Encryption key that fails base64 / length validation — the parser
///   validates at env time, so hitting this means the entry was
///   constructed programmatically without going through
///   `parse_storage_entries`.
///
/// Both are boot-fatal and indicate a code (not config) bug, so
/// panic is the honest response.
pub fn build_entry_backend(
    entry: &NamedStorageEntry,
    local_storage_path_fallback: &Path,
) -> Arc<dyn BlobStorageBackend> {
    let base: Arc<dyn BlobStorageBackend> = match entry.backend {
        StorageBackendType::Local => {
            let path = entry
                .root_dir
                .as_ref()
                .map(PathBuf::from)
                .unwrap_or_else(|| local_storage_path_fallback.to_path_buf());
            Arc::new(
                crate::infrastructure::services::local_blob_backend::LocalBlobBackend::new(&path),
            )
        }
        StorageBackendType::S3 => {
            let s3 = entry.s3.as_ref().unwrap_or_else(|| {
                panic!(
                    "entry `{}` has backend=s3 but no s3 config — parser invariant violated",
                    entry.name
                )
            });
            Arc::new(crate::infrastructure::services::s3_blob_backend::S3BlobBackend::new(s3))
        }
        StorageBackendType::Azure => {
            let az = entry.azure.as_ref().unwrap_or_else(|| {
                panic!(
                    "entry `{}` has backend=azure but no azure config — parser invariant violated",
                    entry.name
                )
            });
            Arc::new(crate::infrastructure::services::azure_blob_backend::AzureBlobBackend::new(az))
        }
    };

    // Encryption decorator — presence-implies-enabled, per plan §Encryption.
    let Some(key_b64) = entry.encryption_key_base64.as_ref() else {
        return base;
    };
    use crate::infrastructure::services::encrypted_blob_backend::EncryptedBlobBackend;
    let key_bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, key_b64)
        .unwrap_or_else(|e| {
            panic!(
                "entry `{}` encryption key is not valid base64: {e} — parser was supposed to \
                 catch this at boot",
                entry.name
            )
        });
    let key: [u8; 32] = key_bytes.try_into().unwrap_or_else(|v: Vec<u8>| {
        panic!(
            "entry `{}` encryption key decoded to {} bytes; must be 32 — parser was supposed to \
             catch this at boot",
            entry.name,
            v.len()
        )
    });
    tracing::info!(
        "Storage entry `{}` encrypted with AES-256-GCM (key from env)",
        entry.name
    );
    Arc::new(EncryptedBlobBackend::new(base, &key))
}
