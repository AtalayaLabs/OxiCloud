//! `migrate` subcommand domain — one-time data migrations.
//!
//! Sqlx schema migrations run automatically at boot via
//! `sqlx::migrate!()` — this domain is reserved for **data** migrations
//! that need explicit operator invocation (data-loss ambiguity, long
//! runtime, or historical schema-drift cleanup).
//!
//! Currently ships one action: `nfc-filenames` — cleans up NFD/NFC
//! name collisions in databases populated before the write-time fix
//! landed at the repository layer (see
//! `folder_db_repository::create_folder`, `file_blob_write_repository`
//! ingest paths, `drive_pg_repository::create_shared_drive_atomic`).
//! New installs get NFC on every ingest and never accumulate drift.
//!
//! Covers BOTH `storage.files.name` and `storage.folders.name`. The
//! folder pass was added 2026-09-04 in response to
//! AtalayaLabs/OxiCloud#706 (macOS Finder folder upload landed NFD;
//! the file-only migrate did nothing for the reporter). Folders have
//! no `blob_hash`, so the collision branch is "older keeps NFC name,
//! newer becomes `.duplicate[-N]`" only — no dedup-by-trash arm,
//! because trashing a folder strands its subtree.
//!
//! Previously lived in a standalone `migrate-nfc-filenames` binary
//! before the v0.9.0 CLI/server merge — see docs/plan/bundled-binary.md § 1b.
//! The 149-line body of `main()` moved here as `run_nfc_filenames()`
//! with `env::args()` parsing replaced by clap.
//!
//! **Retention: indefinite.** An earlier version of this doc set a
//! "future removal target: v1.0" — retracted 2026-09-04 for three
//! reasons:
//!
//!   1. The pre-2026-09 write-side normalization was DEAD CODE
//!      (invariants at `File::new` / `Folder::new_folder` entity
//!      constructors that the create path bypassed), so every
//!      OxiCloud version shipped before that date accumulated NFD
//!      content and has a real remediation need. Many self-hosters
//!      won't upgrade for months.
//!   2. Prior versions of THIS migrate command referenced the D7-
//!      dropped `user_id` column and errored on first run, so users
//!      who tried to apply it never got anywhere. The 2026-09-04 fix
//!      makes it work again — but re-applying to instances that were
//!      "already migrated" (they weren't) is now the only remediation
//!      path for their historical NFD content.
//!   3. Post-fix installs run it as a no-op (all `already_nfc`), so
//!      the cost of shipping it forever is zero and the safety it
//!      offers for late-upgraders is real.

use std::env;

use chrono::{DateTime, Utc};
use clap::Subcommand;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::domain::services::path_service::normalize_storage_name;

#[derive(Subcommand)]
pub enum Action {
    /// NFC-normalize `storage.files.name` AND `storage.folders.name`
    /// across the instance.
    ///
    /// Historical cleanup for databases with rows written before the
    /// repo-level write-time normalization landed (see module doc for
    /// the exact repo methods). Post-fix installs run this as a
    /// harmless no-op — every row reports `already_nfc`.
    ///
    /// Collision handling (files):
    /// * No collision → UPDATE row name to NFC.
    /// * Same blob content → trash the newer row.
    /// * Different content → rename the newer to `{name}.duplicate[-N]`.
    ///
    /// Collision handling (folders):
    /// * No collision → UPDATE row name to NFC.
    /// * Collision → rename the newer to `{name}.duplicate[-N]`; the
    ///   dedup-by-trash arm from the file path is deliberately absent
    ///   because trashing a folder strands its subtree.
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
    /// §14 provenance — the user who created the row. Pre-D7 this
    /// lived on `user_id`; post-D7 it's `created_by` and `user_id`
    /// no longer exists. Not part of the collision scope (the DB
    /// unique index is `(folder_id, name) WHERE NOT is_trashed` —
    /// no user column in it), but surfaced in the log lines so an
    /// operator triaging a large migration output can spot rows
    /// owned by a specific principal without a separate query.
    created_by: Option<Uuid>,
    name: String,
    blob_hash: String,
    created_at: DateTime<Utc>,
}

/// Structural sibling of [`FileRow`] for `storage.folders`. Folders
/// have no `blob_hash` — there is no "same content dedup" branch on
/// collision, only "keep older, rename newer to .duplicate". Added to
/// close the AtalayaLabs/OxiCloud#706 recovery gap: pre-fix DBs with
/// NFD-named folders (macOS Finder / NC desktop upload from macOS)
/// were unreachable via NFC-normalizing clients, and the file-only
/// migrate did nothing for them.
#[derive(Debug, Clone)]
struct FolderRow {
    id: Uuid,
    parent_id: Option<Uuid>,
    /// §14 provenance — see [`FileRow::created_by`].
    created_by: Option<Uuid>,
    name: String,
    created_at: DateTime<Utc>,
}

#[derive(Default)]
struct Stats {
    scanned: u64,
    already_nfc: u64,
    normalized_in_place: u64,
    deduped_same_content: u64,
    renamed_duplicate: u64,
    // Folder stats — deliberately separate so operators reading the
    // summary see "X files, Y folders" instead of one blended count
    // that hides the fact that a run touched both scopes.
    folders_scanned: u64,
    folders_already_nfc: u64,
    folders_normalized_in_place: u64,
    folders_renamed_duplicate: u64,
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
        // folder scope (the DB's unique-index scope for storage.files —
        // `(folder_id, name) WHERE NOT is_trashed`), including rows
        // that may also be non-NFC but happen to normalize to the same
        // NFC value. Pre-D7 this scope included user_id; the column
        // has since been dropped (`docs/plan/drive.md` §D7), so the
        // scope now matches today's unique constraint verbatim.
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
                    "NORMALIZE  file={}  folder={:?}  created_by={:?}  '{}' ({}B) → '{}' ({}B)",
                    row.id,
                    row.folder_id,
                    row.created_by,
                    row.name,
                    row.name.len(),
                    nfc_name,
                    nfc_name.len(),
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
                        "DEDUP      newer={} (trash, same blob)  older={}  folder={:?}  created_by={:?}  hash={}",
                        newer.id,
                        older.id,
                        older.folder_id,
                        older.created_by,
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
                        "RENAME     newer={} (different blob)  older={}  created_by={:?}  '{}' ({}B) → '{}' ({}B)",
                        newer.id,
                        older.id,
                        newer.created_by,
                        newer.name,
                        newer.name.len(),
                        disambiguated,
                        disambiguated.len(),
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

    // Second pass: folders. Same shape as the file loop but no dedup
    // branch (folders have no `blob_hash`). Added to close
    // AtalayaLabs/OxiCloud#706 — a reported macOS-Finder folder upload
    // with an NFD name was unreachable via NFC-normalizing clients and
    // this migration was the operator's documented recovery path.
    if let Err(code) = run_folders(&pool, dry_run, &mut stats).await {
        return code;
    }

    println!();
    println!("=== Summary ===");
    println!("  --- storage.files ---");
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
    println!("  --- storage.folders ---");
    println!(
        "  scanned                            : {}",
        stats.folders_scanned
    );
    println!(
        "  already in NFC                     : {}",
        stats.folders_already_nfc
    );
    println!(
        "  normalized in place (no collision) : {}",
        stats.folders_normalized_in_place
    );
    println!(
        "  renamed to .duplicate              : {}",
        stats.folders_renamed_duplicate
    );
    if dry_run {
        println!();
        println!("DRY RUN — no rows were written. Re-run without --dry-run to apply.");
    }

    0
}

async fn load_non_trashed_files(pool: &PgPool) -> Result<Vec<FileRow>, Box<dyn std::error::Error>> {
    let raw = sqlx::query(
        "SELECT id, folder_id, created_by, name, blob_hash, created_at
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
            created_by: r.try_get("created_by")?,
            name: r.try_get("name")?,
            blob_hash: r.try_get("blob_hash")?,
            created_at: r.try_get("created_at")?,
        });
    }
    Ok(out)
}

/// Sibling of [`load_non_trashed_files`] for folders. Ordered by
/// `created_at` so the older-wins tiebreak on collisions is
/// deterministic. Excludes trashed rows for the same reason as the
/// file scan: DB unique index is partial (`WHERE NOT is_trashed`),
/// and trashed rows will never conflict with live ones.
async fn load_non_trashed_folders(
    pool: &PgPool,
) -> Result<Vec<FolderRow>, Box<dyn std::error::Error>> {
    let raw = sqlx::query(
        "SELECT id, parent_id, created_by, name, created_at
           FROM storage.folders
          WHERE NOT is_trashed
          ORDER BY created_at",
    )
    .fetch_all(pool)
    .await?;

    let mut out = Vec::with_capacity(raw.len());
    for r in raw {
        out.push(FolderRow {
            id: r.try_get("id")?,
            parent_id: r.try_get("parent_id")?,
            created_by: r.try_get("created_by")?,
            name: r.try_get("name")?,
            created_at: r.try_get("created_at")?,
        });
    }
    Ok(out)
}

/// Looks for a file in the same folder scope whose CURRENT name
/// equals `nfc_name`, excluding the row being processed. The other
/// row may itself be in non-NFC form whose normalized representation
/// happens to differ from `nfc_name`; the collision check is
/// intentionally based on stored bytes (matching the UNIQUE-index
/// semantics that this migration is repairing).
async fn find_collision(
    pool: &PgPool,
    row: &FileRow,
    nfc_name: &str,
) -> Result<Option<FileRow>, Box<dyn std::error::Error>> {
    let result = sqlx::query(
        "SELECT id, folder_id, created_by, name, blob_hash, created_at
           FROM storage.files
          WHERE name = $1
            AND ($2::uuid IS NULL AND folder_id IS NULL
                 OR folder_id = $2::uuid)
            AND id <> $3
            AND NOT is_trashed
          LIMIT 1",
    )
    .bind(nfc_name)
    .bind(row.folder_id)
    .bind(row.id)
    .fetch_optional(pool)
    .await?;

    Ok(result.map(|r| FileRow {
        id: r.get("id"),
        folder_id: r.get("folder_id"),
        created_by: r.get("created_by"),
        name: r.get("name"),
        blob_hash: r.get("blob_hash"),
        created_at: r.get("created_at"),
    }))
}

/// Folder-side sibling of [`find_collision`]. Same shape but keyed on
/// `parent_id` — the natural uniqueness scope for `storage.folders`.
async fn find_folder_collision(
    pool: &PgPool,
    row: &FolderRow,
    nfc_name: &str,
) -> Result<Option<FolderRow>, Box<dyn std::error::Error>> {
    let result = sqlx::query(
        "SELECT id, parent_id, created_by, name, created_at
           FROM storage.folders
          WHERE name = $1
            AND ($2::uuid IS NULL AND parent_id IS NULL
                 OR parent_id = $2::uuid)
            AND id <> $3
            AND NOT is_trashed
          LIMIT 1",
    )
    .bind(nfc_name)
    .bind(row.parent_id)
    .bind(row.id)
    .fetch_optional(pool)
    .await?;

    Ok(result.map(|r| FolderRow {
        id: r.get("id"),
        parent_id: r.get("parent_id"),
        created_by: r.get("created_by"),
        name: r.get("name"),
        created_at: r.get("created_at"),
    }))
}

/// Finds a free name in the form `{nfc_name}.duplicate` or
/// `{nfc_name}.duplicate-N` for `N >= 1`, scoped to the row's
/// folder. Returns the first candidate that does not currently
/// exist as a non-trashed row.
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
                   AND ($2::uuid IS NULL AND folder_id IS NULL
                        OR folder_id = $2::uuid)
                   AND id <> $3
                   AND NOT is_trashed)",
        )
        .bind(&candidate)
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
                "Exhausted .duplicate-N suffixes for '{}' in scope (folder_id={:?})",
                nfc_name, row.folder_id
            )
            .into());
        }
    }
}

/// Folder-side sibling. Same shape as [`find_free_duplicate_name`]
/// but keyed on `parent_id`.
async fn find_free_folder_duplicate_name(
    pool: &PgPool,
    row: &FolderRow,
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
                SELECT 1 FROM storage.folders
                 WHERE name = $1
                   AND ($2::uuid IS NULL AND parent_id IS NULL
                        OR parent_id = $2::uuid)
                   AND id <> $3
                   AND NOT is_trashed)",
        )
        .bind(&candidate)
        .bind(row.parent_id)
        .bind(row.id)
        .fetch_one(pool)
        .await?;

        if !taken {
            return Ok(candidate);
        }
        suffix = suffix.saturating_add(1);
        if suffix > 10_000 {
            return Err(format!(
                "Exhausted .duplicate-N suffixes for '{}' in scope (parent_id={:?})",
                nfc_name, row.parent_id
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

/// Folder-side sibling of [`normalize_survivor_name`]. If the older
/// folder we kept was itself in non-NFC form, promote it to the NFC
/// name we just picked as canonical.
async fn normalize_folder_survivor_name(
    pool: &PgPool,
    survivor: &FolderRow,
    nfc_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if survivor.name == nfc_name {
        return Ok(());
    }
    sqlx::query("UPDATE storage.folders SET name = $1 WHERE id = $2")
        .bind(nfc_name)
        .bind(survivor.id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Process every non-trashed folder, mirroring the file loop's shape.
/// Folders have no `blob_hash` so the "same content → dedup" branch is
/// absent: on collision the older folder wins its NFC name, the newer
/// gets renamed to `{nfc_name}.duplicate[-N]`. Never trashes a folder
/// — trashing would strand its subtree, and we cannot know without
/// inspection whether the newer folder was a broken second attempt
/// or an intentional sibling containing different files. Renaming is
/// the conservative choice.
async fn run_folders(pool: &PgPool, dry_run: bool, stats: &mut Stats) -> Result<(), u8> {
    let rows = match load_non_trashed_folders(pool).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("migrate nfc-filenames: folder scan failed: {e}");
            return Err(1);
        }
    };
    println!("Loaded {} non-trashed folder rows", rows.len());
    println!();

    stats.folders_scanned = rows.len() as u64;

    for row in &rows {
        let nfc_name = normalize_storage_name(&row.name);
        if nfc_name == row.name {
            stats.folders_already_nfc += 1;
            continue;
        }

        let collision = match find_folder_collision(pool, row, &nfc_name).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!(
                    "migrate nfc-filenames: folder collision query failed for {}: {e}",
                    row.id
                );
                return Err(1);
            }
        };

        match collision {
            None => {
                println!(
                    "NORMALIZE  folder={}  parent={:?}  created_by={:?}  '{}' ({}B) → '{}' ({}B)",
                    row.id,
                    row.parent_id,
                    row.created_by,
                    row.name,
                    row.name.len(),
                    nfc_name,
                    nfc_name.len(),
                );
                if !dry_run
                    && let Err(e) =
                        sqlx::query("UPDATE storage.folders SET name = $1 WHERE id = $2")
                            .bind(&nfc_name)
                            .bind(row.id)
                            .execute(pool)
                            .await
                {
                    eprintln!(
                        "migrate nfc-filenames: folder rename failed for {}: {e}",
                        row.id
                    );
                    return Err(1);
                }
                stats.folders_normalized_in_place += 1;
            }
            Some(other) => {
                // Older wins the canonical NFC slot; newer gets a
                // `.duplicate[-N]` suffix. No dedup branch here — see
                // the doc comment above.
                let (older, newer) = if row.created_at <= other.created_at {
                    (row, &other)
                } else {
                    (&other, row)
                };

                let disambiguated = match find_free_folder_duplicate_name(pool, newer, &nfc_name)
                    .await
                {
                    Ok(n) => n,
                    Err(e) => {
                        eprintln!(
                            "migrate nfc-filenames: folder duplicate-name search failed for {}: {e}",
                            newer.id
                        );
                        return Err(1);
                    }
                };
                println!(
                    "RENAME     folder-newer={}  older={}  created_by={:?}  '{}' ({}B) → '{}' ({}B)",
                    newer.id,
                    older.id,
                    newer.created_by,
                    newer.name,
                    newer.name.len(),
                    disambiguated,
                    disambiguated.len(),
                );
                if !dry_run {
                    if let Err(e) =
                        sqlx::query("UPDATE storage.folders SET name = $1 WHERE id = $2")
                            .bind(&disambiguated)
                            .bind(newer.id)
                            .execute(pool)
                            .await
                    {
                        eprintln!(
                            "migrate nfc-filenames: folder disambiguation rename failed for {}: {e}",
                            newer.id
                        );
                        return Err(1);
                    }
                    if let Err(e) = normalize_folder_survivor_name(pool, older, &nfc_name).await {
                        eprintln!(
                            "migrate nfc-filenames: folder survivor rename failed for {}: {e}",
                            older.id
                        );
                        return Err(1);
                    }
                }
                stats.folders_renamed_duplicate += 1;
            }
        }
    }
    Ok(())
}
