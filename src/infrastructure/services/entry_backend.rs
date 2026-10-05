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

/// Key in `auth.admin_settings` that holds the persistent-across-restart
/// migration-readonly flag. See
/// `docs/plan/storage-multi-entry.md` §"Read-only mode reuses the
/// existing AuthZ short-circuit". Value is `"true"` or `"false"`
/// (plain text; the settings table stores strings).
///
/// Kept alive as a backward-compat seed for the typed holder below —
/// when the holder row is absent at boot but this bool reads `"true"`,
/// the gate reseeds as `Migration { source: "legacy", target: "legacy" }`
/// so an operator mid-migration across the upgrade still sees writes
/// refused and the UI banner surfaces the (coarse) reason. New writes
/// now go to [`BACKEND_WRITE_LOCK_HOLDER_KEY`] alongside this bool so
/// a restart recovers both "is held" AND "why".
pub const MIGRATION_READONLY_KEY: &str = "storage.migration_readonly";

/// Key in `auth.admin_settings` that holds the typed
/// [`BackendWriteLockReason`] as JSON — the gate's authoritative
/// restart-survival storage. `NULL` / absent / malformed → gate is
/// not held (and the legacy bool above provides the coarse-grained
/// fallback for pre-upgrade state).
pub const BACKEND_WRITE_LOCK_HOLDER_KEY: &str = "storage.backend_write_lock_holder";

/// Read the persisted `migration_readonly` flag from `admin_settings`.
/// Absent row / parse failure / DB error all resolve to `false` — the
/// safer default when we can't determine the intent, since a false
/// value only means "writes allowed by AuthZ" not "migration is
/// running." Called once at boot to seed the in-memory `AtomicBool`.
pub async fn load_migration_readonly(pool: &PgPool) -> bool {
    let row: Result<Option<(Option<String>,)>, sqlx::Error> =
        sqlx::query_as("SELECT value FROM auth.admin_settings WHERE key = $1")
            .bind(MIGRATION_READONLY_KEY)
            .fetch_optional(pool)
            .await;
    match row {
        Ok(Some((Some(v),))) => matches!(v.to_lowercase().as_str(), "true" | "1"),
        Ok(_) => false,
        Err(e) => {
            tracing::warn!(
                target: "oxicloud::scheduler",
                event = "storage.migration_readonly.load_failed",
                error = %e,
                "failed to read {MIGRATION_READONLY_KEY} at boot; defaulting to false"
            );
            false
        }
    }
}

/// Persist the `migration_readonly` flag. Idempotent — upserts the
/// `admin_settings` row. Called by the cutover state machine (slice 5)
/// when a migration starts (set true) or completes cleanly across a
/// restart (set false via the boot clear rule). Handler / trigger
/// callers should also update the in-memory `AtomicBool` alongside
/// this call to keep the two in sync.
pub async fn persist_migration_readonly(pool: &PgPool, value: bool) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO auth.admin_settings (key, value, category, is_secret)
             VALUES ($1, $2, 'storage', FALSE)
        ON CONFLICT (key)
        DO UPDATE SET value = EXCLUDED.value, updated_at = NOW()
        "#,
    )
    .bind(MIGRATION_READONLY_KEY)
    .bind(if value { "true" } else { "false" })
    .execute(pool)
    .await?;
    Ok(())
}

/// Read the persisted typed [`BackendWriteLockReason`] from
/// `admin_settings`. Returns `None` for the three "not held" cases
/// (absent row, NULL value, malformed JSON) — all collapse to
/// "writes allowed". A malformed row MUST NOT wedge the server:
/// a corrupt reason is at worst a lost banner-caption, not a
/// permanent read-only state.
///
/// Called once at boot. Takes an optional legacy bool as a
/// fallback: if the typed row is absent but
/// `legacy_migration_readonly = true`, synthesize a
/// `Migration { source: "legacy", target: "legacy" }` so an
/// operator mid-migration-upgrade still sees writes refused — the
/// exact failure the typed storage is being added to prevent.
pub async fn load_backend_write_lock_holder(
    pool: &PgPool,
    legacy_migration_readonly: bool,
) -> Option<crate::application::services::backend_write_gate::BackendWriteLockReason> {
    use crate::application::services::backend_write_gate::BackendWriteLockReason;
    let row: Result<Option<(Option<String>,)>, sqlx::Error> =
        sqlx::query_as("SELECT value FROM auth.admin_settings WHERE key = $1")
            .bind(BACKEND_WRITE_LOCK_HOLDER_KEY)
            .fetch_optional(pool)
            .await;
    match row {
        Ok(Some((Some(json),))) => match serde_json::from_str::<BackendWriteLockReason>(&json) {
            Ok(reason) => Some(reason),
            Err(e) => {
                tracing::warn!(
                    target: "oxicloud::scheduler",
                    event = "storage.backend_write_lock.parse_failed",
                    error = %e,
                    raw = %json,
                    "could not parse backend_write_lock_holder — treating gate as free; \
                     the legacy migration_readonly bool is the only fallback"
                );
                legacy_fallback(legacy_migration_readonly)
            }
        },
        Ok(_) => legacy_fallback(legacy_migration_readonly),
        Err(e) => {
            tracing::warn!(
                target: "oxicloud::scheduler",
                event = "storage.backend_write_lock.load_failed",
                error = %e,
                "failed to read {BACKEND_WRITE_LOCK_HOLDER_KEY} at boot; falling back to the \
                 legacy migration_readonly bool"
            );
            legacy_fallback(legacy_migration_readonly)
        }
    }
}

fn legacy_fallback(
    legacy_migration_readonly: bool,
) -> Option<crate::application::services::backend_write_gate::BackendWriteLockReason> {
    use crate::application::services::backend_write_gate::BackendWriteLockReason;
    if legacy_migration_readonly {
        tracing::info!(
            target: "oxicloud::scheduler",
            event = "storage.backend_write_lock.legacy_bool_seeded",
            "no typed backend_write_lock_holder row, but migration_readonly=true — gate \
             reseeded as Migration{{source=\"legacy\",target=\"legacy\"}}; the banner will \
             update once the current operation checkpoints with its real pair of names"
        );
        Some(BackendWriteLockReason::Migration {
            source: "legacy".to_string(),
            target: "legacy".to_string(),
        })
    } else {
        None
    }
}

/// Persist the typed [`BackendWriteLockReason`] holder. `None`
/// writes a SQL NULL (gate is free); `Some(reason)` serializes to
/// JSON. Keep the legacy `MIGRATION_READONLY_KEY` bool in sync via
/// [`persist_migration_readonly`] — the AuthZ fast-path reader
/// doesn't speak JSON yet and reseeds from the bool on reboot.
pub async fn persist_backend_write_lock_holder(
    pool: &PgPool,
    holder: Option<&crate::application::services::backend_write_gate::BackendWriteLockReason>,
) -> Result<(), sqlx::Error> {
    let payload = match holder {
        Some(reason) => Some(serde_json::to_string(reason).map_err(|e| {
            // sqlx::Error has no "serialization" variant; wrap in Protocol which
            // the handler's error logging treats the same as any DB error.
            sqlx::Error::Protocol(format!("serialize backend_write_lock_holder: {e}"))
        })?),
        None => None,
    };
    sqlx::query(
        r#"
        INSERT INTO auth.admin_settings (key, value, category, is_secret)
             VALUES ($1, $2, 'storage', FALSE)
        ON CONFLICT (key)
        DO UPDATE SET value = EXCLUDED.value, updated_at = NOW()
        "#,
    )
    .bind(BACKEND_WRITE_LOCK_HOLDER_KEY)
    .bind(payload.as_deref())
    .execute(pool)
    .await?;
    Ok(())
}

/// Persist the `active_backend_name` pointer. Called by the migration
/// handler on `RunOutcome::Completed` to flip the runtime backend to
/// the just-migrated target entry. The next boot reads this via
/// `resolve_active_entry` and picks the new entry for the LIVE
/// backend; before the restart the process is still on the OLD
/// backend (that's what the `migration_readonly` gate is protecting).
/// Idempotent UPSERT.
pub async fn persist_active_backend_name(pool: &PgPool, name: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO auth.admin_settings (key, value, category, is_secret)
             VALUES ($1, $2, 'storage', FALSE)
        ON CONFLICT (key)
        DO UPDATE SET value = EXCLUDED.value, updated_at = NOW()
        "#,
    )
    .bind(ACTIVE_BACKEND_NAME_KEY)
    .bind(name)
    .execute(pool)
    .await?;
    Ok(())
}

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
                     oxicloud storage select <one-of-the-available-names>"
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
/// Typed variant of [`build_entry_backend`] — returns the concrete
/// [`EncryptedBlobBackend`] wrapper so callers that need K3's
/// introspection API (`read_and_classify`, `head_format`, …) can hit
/// it directly without a downcast.
///
/// Same construction path as `build_entry_backend`; the trait-object
/// version delegates through this. Preferred for job handlers
/// (`backend_rotate`) that need typed access. The trait-object
/// version stays for the DI hot-path where the caller only needs
/// the generic `BlobStorageBackend` contract.
pub fn build_entry_backend_typed(
    entry: &NamedStorageEntry,
    local_storage_path_fallback: &Path,
) -> Arc<crate::infrastructure::services::encrypted_blob_backend::EncryptedBlobBackend> {
    let base = build_base_backend(entry, local_storage_path_fallback);
    let pairs = entry.encryption.clone().unwrap_or_default();
    let mode = match entry.head_cipher() {
        Some(crate::common::config::CipherKind::AesGcm256) => "encrypted-v1",
        _ => "plaintext-v1",
    };
    tracing::info!(
        "Storage entry `{}` — {} wrapper (pairs: {})",
        entry.name,
        mode,
        pairs.len()
    );
    Arc::new(
        crate::infrastructure::services::encrypted_blob_backend::EncryptedBlobBackend::new(
            base, pairs,
        ),
    )
}

/// Construct just the raw backend for the entry (no wrapper). Split
/// out of [`build_entry_backend`] so the typed variant can share the
/// switch on backend type without duplicating panic messages.
fn build_base_backend(
    entry: &NamedStorageEntry,
    local_storage_path_fallback: &Path,
) -> Arc<dyn BlobStorageBackend> {
    match entry.backend {
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
    }
}

pub fn build_entry_backend(
    entry: &NamedStorageEntry,
    local_storage_path_fallback: &Path,
) -> Arc<dyn BlobStorageBackend> {
    build_entry_backend_typed(entry, local_storage_path_fallback)
}
