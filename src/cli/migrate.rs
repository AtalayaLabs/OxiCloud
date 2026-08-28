//! `migrate` subcommand domain — one-time data migrations.
//!
//! Sqlx schema migrations run automatically at boot via
//! `sqlx::migrate!()` — this domain is reserved for **data** migrations
//! that need explicit operator invocation (data-loss ambiguity, long
//! runtime, or historical schema-drift cleanup).
//!
//! Currently ships one action: `nfc-filenames` — cleans up NFD/NFC
//! filename collisions in databases populated before the June 2026
//! write-time fix at `src/domain/services/path_service.rs::normalize_storage_name`
//! (called from `src/infrastructure/repositories/pg/file_blob_read_repository.rs`
//! during file operations). New installs never need this migration;
//! only pre-June-2026 databases do.
//!
//! Previously lived in a standalone `migrate-nfc-filenames` binary
//! before the v0.9.0 CLI/server merge — see docs/plan/bundled-binary.md § 1b.
//! The 149-line body of `main()` moved here as `run_nfc_filenames()`
//! with `env::args()` parsing replaced by clap.
//!
//! Future removal target: v1.0. Databases upgraded through v0.9.0
//! will have run this migration (or been unaffected because they were
//! post-fix installs); by v1.0 no user should still need it. Drop
//! the `NfcFilenames` variant + this module's `run_nfc_filenames()`
//! function together at that point.

use std::env;

use chrono::{DateTime, Utc};
use clap::Subcommand;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::domain::services::path_service::normalize_storage_name;

#[derive(Subcommand)]
pub enum Action {
    /// NFC-normalize storage.files.name across the instance.
    ///
    /// Historical cleanup for databases populated before June 2026.
    /// New installs (post-`normalize_storage_name` write-time fix)
    /// never need this — file operations already write NFC form.
    ///
    /// Collision handling:
    /// * No collision → UPDATE row name to NFC.
    /// * Same blob content → trash the newer row.
    /// * Different content → rename the newer to `{name}.duplicate[-N]`.
    ///
    /// In all collision cases, the surviving (older) row's name is
    /// also normalized to NFC.
    NfcFilenames {
        /// Print what would change without touching the DB.
        #[arg(long)]
        dry_run: bool,
    },
}

pub async fn run(action: Action) -> u8 {
    match action {
        Action::NfcFilenames { dry_run } => run_nfc_filenames(dry_run).await,
    }
}

#[derive(Debug, Clone)]
struct FileRow {
    id: Uuid,
    folder_id: Option<Uuid>,
    user_id: Uuid,
    name: String,
    blob_hash: String,
    created_at: DateTime<Utc>,
}

#[derive(Default)]
struct Stats {
    scanned: u64,
    already_nfc: u64,
    normalized_in_place: u64,
    deduped_same_content: u64,
    renamed_duplicate: u64,
}

async fn run_nfc_filenames(dry_run: bool) -> u8 {
    let database_url = match env::var("DATABASE_URL") {
        Ok(v) => v,
        Err(_) => {
            eprintln!("migrate nfc-filenames: DATABASE_URL not set");
            return 2;
        }
    };

    let pool = match PgPool::connect(&database_url).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("migrate nfc-filenames: failed to connect to database: {e}");
            return 1;
        }
    };

    println!(
        "=== NFC filename migration ({}) ===",
        if dry_run {
            "DRY RUN — no writes"
        } else {
            "EXECUTING"
        }
    );
    println!();

    let rows = match load_non_trashed_files(&pool).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("migrate nfc-filenames: initial scan failed: {e}");
            return 1;
        }
    };
    println!("Loaded {} non-trashed file rows", rows.len());
    println!();

    let mut stats = Stats {
        scanned: rows.len() as u64,
        ..Default::default()
    };

    for row in &rows {
        let nfc_name = normalize_storage_name(&row.name);
        if nfc_name == row.name {
            stats.already_nfc += 1;
            continue;
        }

        // Row is in non-NFC form. Look for a collision in the same
        // (folder_id, user_id) scope, including rows that may also
        // be non-NFC but happen to normalize to the same NFC value.
        let collision = match find_collision(&pool, row, &nfc_name).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!(
                    "migrate nfc-filenames: collision query failed for {}: {e}",
                    row.id
                );
                return 1;
            }
        };

        match collision {
            None => {
                println!(
                    "NORMALIZE  {}  user={}  '{}' → '{}'",
                    row.id, row.user_id, row.name, nfc_name
                );
                if !dry_run
                    && let Err(e) = sqlx::query("UPDATE storage.files SET name = $1 WHERE id = $2")
                        .bind(&nfc_name)
                        .bind(row.id)
                        .execute(&pool)
                        .await
                {
                    eprintln!("migrate nfc-filenames: rename failed for {}: {e}", row.id);
                    return 1;
                }
                stats.normalized_in_place += 1;
            }
            Some(other) => {
                // Pick winner/loser by `created_at` — older wins.
                let (older, newer) = if row.created_at <= other.created_at {
                    (row, &other)
                } else {
                    (&other, row)
                };

                if older.blob_hash == newer.blob_hash {
                    // Same content → trash the newer; promote older's
                    // name to NFC if it isn't already.
                    println!(
                        "DEDUP      newer={} (trash, same blob)  older={}  user={}  hash={}",
                        newer.id,
                        older.id,
                        older.user_id,
                        &older.blob_hash[..16.min(older.blob_hash.len())]
                    );
                    if !dry_run {
                        if let Err(e) = sqlx::query(
                            "UPDATE storage.files
                                SET is_trashed = TRUE,
                                    trashed_at = NOW()
                              WHERE id = $1",
                        )
                        .bind(newer.id)
                        .execute(&pool)
                        .await
                        {
                            eprintln!("migrate nfc-filenames: trash failed for {}: {e}", newer.id);
                            return 1;
                        }
                        if let Err(e) = normalize_survivor_name(&pool, older, &nfc_name).await {
                            eprintln!(
                                "migrate nfc-filenames: survivor rename failed for {}: {e}",
                                older.id
                            );
                            return 1;
                        }
                    }
                    stats.deduped_same_content += 1;
                } else {
                    // Different content → rename newer to a free
                    // `{nfc_name}.duplicate[-N]`; promote older to NFC.
                    let disambiguated = match find_free_duplicate_name(&pool, newer, &nfc_name)
                        .await
                    {
                        Ok(n) => n,
                        Err(e) => {
                            eprintln!(
                                "migrate nfc-filenames: duplicate-name search failed for {}: {e}",
                                newer.id
                            );
                            return 1;
                        }
                    };
                    println!(
                        "RENAME     newer={} (different blob)  older={}  '{}' → '{}'",
                        newer.id, older.id, newer.name, disambiguated
                    );
                    if !dry_run {
                        if let Err(e) =
                            sqlx::query("UPDATE storage.files SET name = $1 WHERE id = $2")
                                .bind(&disambiguated)
                                .bind(newer.id)
                                .execute(&pool)
                                .await
                        {
                            eprintln!(
                                "migrate nfc-filenames: disambiguation rename failed for {}: {e}",
                                newer.id
                            );
                            return 1;
                        }
                        if let Err(e) = normalize_survivor_name(&pool, older, &nfc_name).await {
                            eprintln!(
                                "migrate nfc-filenames: survivor rename failed for {}: {e}",
                                older.id
                            );
                            return 1;
                        }
                    }
                    stats.renamed_duplicate += 1;
                }
            }
        }
    }

    println!();
    println!("=== Summary ===");
    println!("  scanned                            : {}", stats.scanned);
    println!(
        "  already in NFC                     : {}",
        stats.already_nfc
    );
    println!(
        "  normalized in place (no collision) : {}",
        stats.normalized_in_place
    );
    println!(
        "  dedup-trashed (same content)       : {}",
        stats.deduped_same_content
    );
    println!(
        "  renamed to .duplicate              : {}",
        stats.renamed_duplicate
    );
    if dry_run {
        println!();
        println!("DRY RUN — no rows were written. Re-run without --dry-run to apply.");
    }

    0
}

async fn load_non_trashed_files(pool: &PgPool) -> Result<Vec<FileRow>, Box<dyn std::error::Error>> {
    let raw = sqlx::query(
        "SELECT id, folder_id, user_id, name, blob_hash, created_at
           FROM storage.files
          WHERE NOT is_trashed
          ORDER BY created_at",
    )
    .fetch_all(pool)
    .await?;

    let mut out = Vec::with_capacity(raw.len());
    for r in raw {
        out.push(FileRow {
            id: r.try_get("id")?,
            folder_id: r.try_get("folder_id")?,
            user_id: r.try_get("user_id")?,
            name: r.try_get("name")?,
            blob_hash: r.try_get("blob_hash")?,
            created_at: r.try_get("created_at")?,
        });
    }
    Ok(out)
}

/// Looks for a row in the same `(folder_id, user_id)` scope whose
/// CURRENT name equals `nfc_name`, excluding the row being processed.
/// The other row may itself be in non-NFC form whose normalized
/// representation happens to differ from `nfc_name`; the collision
/// check is intentionally based on stored bytes (matching the
/// UNIQUE-index semantics that this migration is repairing).
async fn find_collision(
    pool: &PgPool,
    row: &FileRow,
    nfc_name: &str,
) -> Result<Option<FileRow>, Box<dyn std::error::Error>> {
    let result = sqlx::query(
        "SELECT id, folder_id, user_id, name, blob_hash, created_at
           FROM storage.files
          WHERE name = $1
            AND user_id = $2
            AND ($3::uuid IS NULL AND folder_id IS NULL
                 OR folder_id = $3::uuid)
            AND id <> $4
            AND NOT is_trashed
          LIMIT 1",
    )
    .bind(nfc_name)
    .bind(row.user_id)
    .bind(row.folder_id)
    .bind(row.id)
    .fetch_optional(pool)
    .await?;

    Ok(result.map(|r| FileRow {
        id: r.get("id"),
        folder_id: r.get("folder_id"),
        user_id: r.get("user_id"),
        name: r.get("name"),
        blob_hash: r.get("blob_hash"),
        created_at: r.get("created_at"),
    }))
}

/// Finds a free name in the form `{nfc_name}.duplicate` or
/// `{nfc_name}.duplicate-N` for `N >= 1`, scoped to the row's
/// `(folder_id, user_id)`. Returns the first candidate that does
/// not currently exist as a non-trashed row.
async fn find_free_duplicate_name(
    pool: &PgPool,
    row: &FileRow,
    nfc_name: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut suffix: u32 = 0;
    loop {
        let candidate = if suffix == 0 {
            format!("{}.duplicate", nfc_name)
        } else {
            format!("{}.duplicate-{}", nfc_name, suffix)
        };

        let taken: bool = sqlx::query_scalar(
            "SELECT EXISTS(
                SELECT 1 FROM storage.files
                 WHERE name = $1
                   AND user_id = $2
                   AND ($3::uuid IS NULL AND folder_id IS NULL
                        OR folder_id = $3::uuid)
                   AND id <> $4
                   AND NOT is_trashed)",
        )
        .bind(&candidate)
        .bind(row.user_id)
        .bind(row.folder_id)
        .bind(row.id)
        .fetch_one(pool)
        .await?;

        if !taken {
            return Ok(candidate);
        }
        suffix = suffix.saturating_add(1);
        // Safety bound — should never trigger under realistic data.
        if suffix > 10_000 {
            return Err(format!(
                "Exhausted .duplicate-N suffixes for '{}' in scope (user={}, folder_id={:?})",
                nfc_name, row.user_id, row.folder_id
            )
            .into());
        }
    }
}

/// If the surviving (older) row's stored name is not yet in NFC,
/// UPDATE it now that the collision has been resolved.
async fn normalize_survivor_name(
    pool: &PgPool,
    survivor: &FileRow,
    nfc_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if survivor.name == nfc_name {
        return Ok(());
    }
    sqlx::query("UPDATE storage.files SET name = $1 WHERE id = $2")
        .bind(nfc_name)
        .bind(survivor.id)
        .execute(pool)
        .await?;
    Ok(())
}
