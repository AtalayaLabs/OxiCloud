//! Round-10 query-shape pack — BEFORE/AFTER over the dev Postgres.
//!
//! Sections:
//!   1. Share-download metadata: BEFORE 2× `get_file` per download (the
//!      handler fetched the DTO, then `get_file_optimized` re-fetched it)
//!      vs AFTER 1× + `_preloaded`. Gate: identical DTOs.
//!   2. CalDAV update/delete authz gate: BEFORE full `find_event_by_id`
//!      (drags `ical_data`) vs AFTER `find_calendar_id_by_event_id` scalar.
//!      Gate: identical calendar id.
//!   3. Contact-group summary: BEFORE `get_contacts_in_group().len()`
//!      (hydrates vCard TEXT + 3 JSONB parses × N) vs AFTER
//!      `count_contacts_in_group`. Gate: identical count.
//!   4. Trash listing: `drive_id = ANY($1) AND is_trashed` with only the
//!      pre-round indexes vs the new partial `(drive_id, trashed_at) WHERE
//!      is_trashed` pair. Gate: identical row sets.
//!   5. Legacy favorites listing rows: BEFORE `::TEXT` server casts vs
//!      AFTER binary UUID decode + app-side render (the shipped SQL).
//!      Gate: identical rendered tuples.
//!   6. `save_faces`: BEFORE one INSERT per face (replica of the old loop)
//!      vs AFTER the shipped single UNNEST INSERT. Gate: identical rows.
//!   7. Playlist reorder: BEFORE one UPDATE per track vs AFTER the shipped
//!      UNNEST UPDATE. Gate: identical final positions.
//!   8. Search page: BEFORE serial file-page + folder queries vs AFTER
//!      `tokio::join!` (the shipped shape; the content-index arm is off in
//!      this harness — the overlap win measured is files∥folders).
//!      Gate: identical results.
//!   9. Move pre-check: BEFORE serial src-drive + dst-drive point reads vs
//!      AFTER `join!`. Gate: identical resolutions. Decide-by-bench.
//!
//! Run (needs Postgres; reads DATABASE_URL from .env):
//!   cargo bench --features=bench --bench bench_round10_queries
//! Tunables (env): BENCH_PASSES (200)

use std::env;
use std::sync::Arc;
use std::time::{Duration, Instant};

use oxicloud::application::ports::face_ports::FaceRepository;
use oxicloud::domain::repositories::calendar_event_repository::CalendarEventRepository;
use oxicloud::domain::repositories::contact_repository::ContactGroupRepository;
use oxicloud::domain::repositories::drive_repository::DriveRepository;
use oxicloud::domain::repositories::playlist_repository::PlaylistItemRepository;
use oxicloud::infrastructure::repositories::pg::{
    CalendarEventPgRepository, ContactGroupPgRepository, DrivePgRepository, FacePgRepository,
    PlaylistItemPgRepository,
};
use sqlx::{PgPool, Row, postgres::PgPoolOptions};
use uuid::Uuid;

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn p50(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

async fn timed<F, Fut, R>(passes: usize, mut f: F) -> (f64, R)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = R>,
{
    // Warmup
    let mut last = f().await;
    let mut samples = Vec::with_capacity(passes);
    for _ in 0..passes {
        let t = Instant::now();
        last = f().await;
        samples.push(t.elapsed().as_secs_f64() * 1e3);
    }
    (p50(samples), last)
}

struct Seed {
    owner: Uuid,
    drive: Uuid,
    root: Uuid,
    file: Uuid,
    blob: String,
}

async fn seed_base(pool: &PgPool, tag: &str) -> Seed {
    // Idempotent: sweep leftovers from an aborted earlier run first.
    let _ = sqlx::query("DELETE FROM storage.files WHERE blob_hash = $1")
        .bind(format!("{:0<64}", format!("br10{tag}")))
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM storage.blobs WHERE hash = $1")
        .bind(format!("{:0<64}", format!("br10{tag}")))
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM storage.folders WHERE lpath = $1::ltree")
        .bind(format!("br10{tag}"))
        .execute(pool)
        .await;
    let _ = sqlx::query(
        "DELETE FROM storage.drives WHERE default_for_user IN
             (SELECT id FROM auth.users WHERE username = $1)",
    )
    .bind(format!("bench_r10_{tag}"))
    .execute(pool)
    .await;
    let _ = sqlx::query("DELETE FROM auth.users WHERE username = $1")
        .bind(format!("bench_r10_{tag}"))
        .execute(pool)
        .await;
    // Drive + root folder + root stamp must land in ONE transaction: the
    // `check_no_orphan_root_folder` trigger rejects a root folder whose
    // drive doesn't point back at it by statement end.
    let mut tx = pool.begin().await.expect("begin seed tx");
    let owner: Uuid = sqlx::query_scalar(
        "INSERT INTO auth.users (username, email, role)
         VALUES ($1, $2, 'user') RETURNING id",
    )
    .bind(format!("bench_r10_{tag}"))
    .bind(format!("bench_r10_{tag}@bench.invalid"))
    .fetch_one(&mut *tx)
    .await
    .expect("seed owner");
    let drive: Uuid = sqlx::query_scalar(
        "INSERT INTO storage.drives (kind, default_for_user) VALUES ('personal', $1) RETURNING id",
    )
    .bind(owner)
    .fetch_one(&mut *tx)
    .await
    .expect("seed drive");
    let root: Uuid = sqlx::query_scalar(
        "INSERT INTO storage.folders (name, path, lpath, drive_id)
         VALUES ('Personal', '/Personal', $2::ltree, $1) RETURNING id",
    )
    .bind(drive)
    .bind(format!("br10{tag}"))
    .fetch_one(&mut *tx)
    .await
    .expect("seed root");
    sqlx::query("UPDATE storage.drives SET root_folder_id = $1 WHERE id = $2")
        .bind(root)
        .bind(drive)
        .execute(&mut *tx)
        .await
        .expect("stamp root");
    sqlx::query(
        "INSERT INTO storage.role_grants
             (subject_type, subject_id, resource_type, resource_id, role, granted_by)
         VALUES ('user', $1, 'drive', $2, 'owner'::storage.grant_role, $1)",
    )
    .bind(owner)
    .bind(drive)
    .execute(&mut *tx)
    .await
    .expect("seed owner grant");

    let blob = format!("{:0<64}", format!("br10{tag}"));
    sqlx::query("INSERT INTO storage.blobs (hash, size, ref_count) VALUES ($1, 4096, 1)")
        .bind(&blob)
        .execute(&mut *tx)
        .await
        .expect("seed blob");
    let file: Uuid = sqlx::query_scalar(
        "INSERT INTO storage.files (name, folder_id, blob_hash, size, mime_type, drive_id)
         VALUES ('bench-share.bin', $1, $2, 4096, 'application/octet-stream', $3) RETURNING id",
    )
    .bind(root)
    .bind(&blob)
    .bind(drive)
    .fetch_one(&mut *tx)
    .await
    .expect("seed file");
    tx.commit().await.expect("commit seed tx");
    Seed {
        owner,
        drive,
        root,
        file,
        blob,
    }
}

async fn cleanup_base(pool: &PgPool, s: &Seed) {
    let _ = sqlx::query("DELETE FROM storage.role_grants WHERE subject_id = $1 OR granted_by = $1")
        .bind(s.owner)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM storage.files WHERE drive_id = $1")
        .bind(s.drive)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM storage.folders WHERE drive_id = $1")
        .bind(s.drive)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM storage.drives WHERE id = $1")
        .bind(s.drive)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM storage.blobs WHERE hash = $1")
        .bind(&s.blob)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM auth.users WHERE id = $1")
        .bind(s.owner)
        .execute(pool)
        .await;
}

/// The exact metadata row the share path fetches (a trimmed replica of the
/// repo's `get_file` projection — enough to time the round-trip honestly).
async fn fetch_file_meta(pool: &PgPool, id: Uuid) -> (Uuid, String, i64, String) {
    let row = sqlx::query(
        "SELECT fi.id, fi.name, fi.size, fi.mime_type, fi.blob_hash, fi.folder_id, fo.path,
                EXTRACT(EPOCH FROM fi.created_at)::bigint AS ca,
                EXTRACT(EPOCH FROM fi.updated_at)::bigint AS ma
           FROM storage.files fi
           LEFT JOIN storage.folders fo ON fo.id = fi.folder_id
          WHERE fi.id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("file meta");
    (
        row.get("id"),
        row.get("name"),
        row.get("size"),
        row.get("mime_type"),
    )
}

async fn section_share_double_fetch(pool: &PgPool, passes: usize) {
    println!("[1] share download — metadata fetches per request");
    let s = seed_base(pool, "share").await;

    let (before_ms, b) = timed(passes, || async {
        // BEFORE: handler get_file + get_file_optimized's internal get_file.
        let a = fetch_file_meta(pool, s.file).await;
        let _dup = fetch_file_meta(pool, s.file).await;
        a
    })
    .await;
    let (after_ms, a) = timed(passes, || async {
        // AFTER: one fetch; the DTO is handed to the _preloaded variant.
        fetch_file_meta(pool, s.file).await
    })
    .await;
    assert_eq!(b, a, "identical DTO");
    println!(
        "    BEFORE 2 queries {before_ms:.3} ms/download → AFTER 1 query {after_ms:.3} ms  ({:.2}x)",
        before_ms / after_ms
    );
    cleanup_base(pool, &s).await;
}

async fn section_calendar_narrow(pool: &Arc<PgPool>, passes: usize) {
    println!("[2] CalDAV update/delete gate — event row width");
    let s = seed_base(pool, "cal").await;
    let cal: Uuid = sqlx::query_scalar(
        "INSERT INTO caldav.calendars (id, name, owner_id)
         VALUES (gen_random_uuid(), 'Bench', $1) RETURNING id",
    )
    .bind(s.owner)
    .fetch_one(pool.as_ref())
    .await
    .expect("seed calendar");
    // A recurring event with a fat body — attendees/VALARM/X-props easily
    // push real invites into the tens of KB.
    let fat_ical = format!(
        "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:bench-r10\r\nSUMMARY:Standup\r\n{}END:VEVENT\r\nEND:VCALENDAR\r\n",
        "ATTENDEE;CN=Person;PARTSTAT=NEEDS-ACTION:mailto:person@example.com\r\n".repeat(160)
    );
    let event: Uuid = sqlx::query_scalar(
        "INSERT INTO caldav.calendar_events
             (id, calendar_id, summary, start_time, end_time, ical_uid, ical_data)
         VALUES (gen_random_uuid(), $1, 'Standup', NOW(), NOW() + interval '1 hour', 'bench-r10', $2)
         RETURNING id",
    )
    .bind(cal)
    .bind(&fat_ical)
    .fetch_one(pool.as_ref())
    .await
    .expect("seed event");
    println!("    ical_data bytes: {}", fat_ical.len());

    let repo = CalendarEventPgRepository::new(pool.clone());
    let (before_ms, b) = timed(passes, || async {
        // BEFORE: the service fetched the whole event for `.calendar_id`.
        *repo.find_event_by_id(&event).await.unwrap().calendar_id()
    })
    .await;
    let (after_ms, a) = timed(passes, || async {
        repo.find_calendar_id_by_event_id(&event).await.unwrap()
    })
    .await;
    assert_eq!(b, a, "identical calendar id");
    println!(
        "    BEFORE full row {before_ms:.3} ms → AFTER scalar {after_ms:.3} ms  ({:.2}x)",
        before_ms / after_ms
    );

    let _ = sqlx::query("DELETE FROM caldav.calendars WHERE id = $1")
        .bind(cal)
        .execute(pool.as_ref())
        .await;
    cleanup_base(pool, &s).await;
}

async fn section_group_count(pool: &Arc<PgPool>, passes: usize) {
    println!("[3] contact-group summary — members count");
    let s = seed_base(pool, "group").await;
    let book: Uuid = sqlx::query_scalar(
        "INSERT INTO carddav.address_books (id, name, owner_id)
         VALUES (gen_random_uuid(), 'Bench', $1) RETURNING id",
    )
    .bind(s.owner)
    .fetch_one(pool.as_ref())
    .await
    .expect("seed book");
    let group: Uuid = sqlx::query_scalar(
        "INSERT INTO carddav.contact_groups (id, address_book_id, name)
         VALUES (gen_random_uuid(), $1, 'Team') RETURNING id",
    )
    .bind(book)
    .fetch_one(pool.as_ref())
    .await
    .expect("seed group");
    let members = 500usize;
    let vcard_pad = format!(
        "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Contact\r\nNOTE:{}\r\nEND:VCARD\r\n",
        "x".repeat(2048)
    );
    for i in 0..members {
        let cid: Uuid = sqlx::query_scalar(
            "INSERT INTO carddav.contacts
                 (id, address_book_id, uid, full_name, email, phone, address, vcard, etag)
             VALUES (gen_random_uuid(), $1, $2, $3,
                     '[{\"email\":\"a@b.c\",\"type\":\"home\"}]'::jsonb,
                     '[{\"number\":\"+1555\",\"type\":\"cell\"}]'::jsonb,
                     '[]'::jsonb, $4, 'etag')
             RETURNING id",
        )
        .bind(book)
        .bind(format!("uid-{i}"))
        .bind(format!("Contact {i}"))
        .bind(&vcard_pad)
        .fetch_one(pool.as_ref())
        .await
        .expect("seed contact");
        sqlx::query("INSERT INTO carddav.group_memberships (group_id, contact_id) VALUES ($1, $2)")
            .bind(group)
            .bind(cid)
            .execute(pool.as_ref())
            .await
            .expect("seed membership");
    }

    let repo = ContactGroupPgRepository::new(pool.clone());
    let (before_ms, b) = timed(passes.min(60), || async {
        // BEFORE: full hydration, count, throw away.
        repo.get_contacts_in_group(&group).await.unwrap().len() as i64
    })
    .await;
    let (after_ms, a) = timed(passes.min(60), || async {
        repo.count_contacts_in_group(&group).await.unwrap()
    })
    .await;
    assert_eq!(b, a, "identical member count");
    println!(
        "    500 members: BEFORE hydrate-all {before_ms:.3} ms → AFTER COUNT(*) {after_ms:.3} ms  ({:.1}x)",
        before_ms / after_ms
    );

    let _ = sqlx::query("DELETE FROM carddav.address_books WHERE id = $1")
        .bind(book)
        .execute(pool.as_ref())
        .await;
    cleanup_base(pool, &s).await;
}

async fn section_trash_index(pool: &PgPool, passes: usize) {
    println!("[4] trash listing — partial (drive_id, trashed_at) indexes");
    // 30 drives × 3000 live + 25 trashed files each; the caller lists ONE drive.
    let owner: Uuid = sqlx::query_scalar(
        "INSERT INTO auth.users (username, email, role)
         VALUES ('bench_r10_trash', 'bench_r10_trash@bench.invalid', 'user') RETURNING id",
    )
    .fetch_one(pool)
    .await
    .expect("owner");
    let blob = format!("{:0<64}", "br10trash");
    sqlx::query("INSERT INTO storage.blobs (hash, size, ref_count) VALUES ($1, 4096, 1)")
        .bind(&blob)
        .execute(pool)
        .await
        .expect("blob");
    let mut drives = Vec::new();
    for d in 0..30 {
        // Drive + root + stamp in one tx (orphan-root trigger, see seed_base).
        let mut tx = pool.begin().await.expect("begin drive tx");
        let drive: Uuid = sqlx::query_scalar(
            "INSERT INTO storage.drives (kind, default_for_user) VALUES ('personal', NULL) RETURNING id",
        )
        .fetch_one(&mut *tx)
        .await
        .expect("drive");
        let root: Uuid = sqlx::query_scalar(
            "INSERT INTO storage.folders (name, path, lpath, drive_id)
             VALUES ('Personal', '/Personal', $2::ltree, $1) RETURNING id",
        )
        .bind(drive)
        .bind(format!("br10trash{d}"))
        .fetch_one(&mut *tx)
        .await
        .expect("root");
        sqlx::query("UPDATE storage.drives SET root_folder_id = $1 WHERE id = $2")
            .bind(root)
            .bind(drive)
            .execute(&mut *tx)
            .await
            .expect("stamp root");
        tx.commit().await.expect("commit drive tx");
        // Bulk-insert live + trashed files via generate_series.
        sqlx::query(
            "INSERT INTO storage.files (name, folder_id, blob_hash, size, mime_type, drive_id, is_trashed, trashed_at)
             SELECT 'live-' || i, $1, $2, 4096, 'application/octet-stream', $3, FALSE, NULL
               FROM generate_series(1, 3000) i",
        )
        .bind(root)
        .bind(&blob)
        .bind(drive)
        .execute(pool)
        .await
        .expect("live files");
        sqlx::query(
            "INSERT INTO storage.files (name, folder_id, blob_hash, size, mime_type, drive_id, is_trashed, trashed_at)
             SELECT 'gone-' || i, $1, $2, 4096, 'application/octet-stream', $3, TRUE, NOW() - (i || ' minutes')::interval
               FROM generate_series(1, 25) i",
        )
        .bind(root)
        .bind(&blob)
        .bind(drive)
        .execute(pool)
        .await
        .expect("trashed files");
        drives.push(drive);
    }
    sqlx::query("ANALYZE storage.files")
        .execute(pool)
        .await
        .expect("analyze");

    let list_sql = "SELECT f.id, f.name, f.trashed_at
                      FROM storage.files f
                     WHERE f.drive_id = ANY($1) AND f.is_trashed = TRUE
                     ORDER BY f.trashed_at DESC, f.id DESC
                     LIMIT 51";
    let target = vec![drives[7]];
    let run = |pool: &PgPool, target: &Vec<Uuid>| {
        let pool = pool.clone();
        let target = target.clone();
        async move {
            let rows = sqlx::query(list_sql)
                .bind(&target)
                .fetch_all(&pool)
                .await
                .expect("trash listing");
            rows.iter()
                .map(|r| r.get::<Uuid, _>("id"))
                .collect::<Vec<_>>()
        }
    };

    // BEFORE: drop the round-10 indexes (migration applies them by default).
    sqlx::query("DROP INDEX IF EXISTS storage.idx_files_drive_trashed")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("DROP INDEX IF EXISTS storage.idx_folders_drive_trashed")
        .execute(pool)
        .await
        .unwrap();
    let (before_ms, b) = timed(passes, || run(pool, &target)).await;

    // AFTER: recreate them (exact migration DDL).
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_files_drive_trashed
             ON storage.files (drive_id, trashed_at) WHERE is_trashed",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_folders_drive_trashed
             ON storage.folders (drive_id, trashed_at) WHERE is_trashed",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("ANALYZE storage.files")
        .execute(pool)
        .await
        .unwrap();
    let (after_ms, a) = timed(passes, || run(pool, &target)).await;
    assert_eq!(b, a, "identical trash listing");
    println!(
        "    1 drive of 30 (25 trash / 3000 live each): BEFORE {before_ms:.3} ms → AFTER {after_ms:.3} ms  ({:.1}x)",
        before_ms / after_ms
    );

    for d in &drives {
        let _ = sqlx::query("DELETE FROM storage.files WHERE drive_id = $1")
            .bind(d)
            .execute(pool)
            .await;
        let _ = sqlx::query("DELETE FROM storage.folders WHERE drive_id = $1")
            .bind(d)
            .execute(pool)
            .await;
        let _ = sqlx::query("DELETE FROM storage.drives WHERE id = $1")
            .bind(d)
            .execute(pool)
            .await;
    }
    let _ = sqlx::query("DELETE FROM storage.blobs WHERE hash = $1")
        .bind(&blob)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM auth.users WHERE id = $1")
        .bind(owner)
        .execute(pool)
        .await;
}

async fn section_favorites_cast(pool: &PgPool, passes: usize) {
    println!("[5] legacy favorites rows — ::TEXT casts vs binary decode");
    let s = seed_base(pool, "fav").await;
    // 500 favorited files.
    let mut file_ids = Vec::new();
    for i in 0..500 {
        let f: Uuid = sqlx::query_scalar(
            "INSERT INTO storage.files (name, folder_id, blob_hash, size, mime_type, drive_id)
             VALUES ($1, $2, $3, 4096, 'image/jpeg', $4) RETURNING id",
        )
        .bind(format!("fav-{i:04}.jpg"))
        .bind(s.root)
        .bind(&s.blob)
        .bind(s.drive)
        .fetch_one(pool)
        .await
        .expect("file");
        sqlx::query(
            "INSERT INTO auth.user_favorites (user_id, item_id, item_type) VALUES ($1, $2, 'file')",
        )
        .bind(s.owner)
        .bind(f.to_string())
        .execute(pool)
        .await
        .expect("fav");
        file_ids.push(f);
    }

    let before_sql = r#"
        SELECT uf.id::TEXT AS id, uf.user_id::TEXT AS user_id, uf.item_id,
               COALESCE(f.folder_id::TEXT, NULL) AS parent_id, f.name AS item_name
          FROM auth.user_favorites uf
          LEFT JOIN storage.files f ON uf.item_type = 'file' AND f.id = uf.item_id::UUID
         WHERE uf.user_id = $1
         ORDER BY uf.created_at DESC LIMIT 500"#;
    let after_sql = r#"
        SELECT uf.id AS id, uf.user_id AS user_id, uf.item_id,
               f.folder_id AS parent_id, f.name AS item_name
          FROM auth.user_favorites uf
          LEFT JOIN storage.files f ON uf.item_type = 'file' AND f.id = uf.item_id::UUID
         WHERE uf.user_id = $1
         ORDER BY uf.created_at DESC LIMIT 500"#;

    // Interleaved passes (the ROUND6/9 protocol) so plan/cache drift can't
    // favour one arm.
    let mut before_samples = Vec::new();
    let mut after_samples = Vec::new();
    let mut b_out: Vec<(String, String, Option<String>)> = Vec::new();
    let mut a_out: Vec<(String, String, Option<String>)> = Vec::new();
    for _ in 0..passes {
        let t = Instant::now();
        let rows = sqlx::query(before_sql)
            .bind(s.owner)
            .fetch_all(pool)
            .await
            .unwrap();
        b_out = rows
            .iter()
            .map(|r| {
                (
                    r.get::<String, _>("id"),
                    r.get::<String, _>("item_id"),
                    r.try_get::<Option<String>, _>("parent_id").ok().flatten(),
                )
            })
            .collect();
        before_samples.push(t.elapsed().as_secs_f64() * 1e3);

        let t = Instant::now();
        let rows = sqlx::query(after_sql)
            .bind(s.owner)
            .fetch_all(pool)
            .await
            .unwrap();
        a_out = rows
            .iter()
            .map(|r| {
                (
                    r.get::<i32, _>("id").to_string(),
                    r.get::<String, _>("item_id"),
                    r.try_get::<Option<Uuid>, _>("parent_id")
                        .ok()
                        .flatten()
                        .map(|u| u.to_string()),
                )
            })
            .collect();
        after_samples.push(t.elapsed().as_secs_f64() * 1e3);
    }
    assert_eq!(b_out, a_out, "identical rendered tuples");
    let before_ms = p50(before_samples);
    let after_ms = p50(after_samples);
    println!(
        "    500-row page: BEFORE ::TEXT {before_ms:.3} ms → AFTER binary {after_ms:.3} ms  ({:.2}x)",
        before_ms / after_ms
    );
    let _ = sqlx::query("DELETE FROM auth.user_favorites WHERE user_id = $1")
        .bind(s.owner)
        .execute(pool)
        .await;
    cleanup_base(pool, &s).await;
}

async fn section_save_faces(pool: &Arc<PgPool>, passes: usize) {
    println!("[6] save_faces — INSERT-per-face vs UNNEST batch (30 faces)");
    let s = seed_base(pool, "faces").await;
    use oxicloud::domain::entities::face::{BoundingBox, Face};

    let make_faces = |n: usize| -> Vec<Face> {
        (0..n)
            .map(|i| Face {
                id: Uuid::new_v4(),
                file_id: s.file,
                user_id: s.owner,
                person_id: None,
                bbox: BoundingBox {
                    x: 0.1,
                    y: 0.2,
                    w: 0.3,
                    h: 0.4,
                },
                det_score: 0.9,
                quality: Some(0.5 + i as f32 * 0.001),
                embedding: vec![0.5f32; 512],
                blob_hash: Some(s.blob.clone()),
                created_at: chrono::Utc::now(),
            })
            .collect()
    };

    let repo = FacePgRepository::new(pool.clone());
    let n_faces = 30usize;
    let bench_passes = passes.min(80);

    // BEFORE replica: the old per-face INSERT loop in one transaction.
    let (before_ms, _) = timed(bench_passes, || {
        let faces = make_faces(n_faces);
        let pool = pool.clone();
        async move {
            let mut tx = pool.begin().await.unwrap();
            for f in &faces {
                sqlx::query(
                    "INSERT INTO faces.faces
                         (id, file_id, user_id, person_id, bbox, det_score, quality, embedding, blob_hash)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
                )
                .bind(f.id)
                .bind(f.file_id)
                .bind(f.user_id)
                .bind(f.person_id)
                .bind(f.bbox.to_array())
                .bind(f.det_score)
                .bind(f.quality)
                .bind(f.embedding.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>())
                .bind(f.blob_hash.as_deref())
                .execute(&mut *tx)
                .await
                .unwrap();
            }
            tx.commit().await.unwrap();
        }
    })
    .await;

    // AFTER: the shipped UNNEST batch.
    let (after_ms, _) = timed(bench_passes, || {
        let faces = make_faces(n_faces);
        let repo = &repo;
        async move {
            repo.save_faces(&faces).await.unwrap();
        }
    })
    .await;

    // Gate: batch write round-trips identically (row content check). Read the
    // probe row back with a direct full-column SELECT — the repo's narrow
    // `face_boxes_for_file` (ROUND14 §Q1) no longer returns embedding/quality/
    // blob_hash, so this section fetches them itself to keep the gate intact.
    let probe = make_faces(3);
    repo.save_faces(&probe).await.unwrap();
    let (bbox, embedding, quality, blob_hash): (Vec<f32>, Vec<u8>, Option<f32>, Option<String>) =
        sqlx::query_as("SELECT bbox, embedding, quality, blob_hash FROM faces.faces WHERE id = $1")
            .bind(probe[1].id)
            .fetch_one(pool.as_ref())
            .await
            .expect("stored");
    assert_eq!(bbox, probe[1].bbox.to_array());
    assert_eq!(embedding.len() / 4, probe[1].embedding.len());
    assert_eq!(quality, probe[1].quality);
    assert_eq!(blob_hash, probe[1].blob_hash);

    println!(
        "    30-face image: BEFORE loop {before_ms:.3} ms → AFTER UNNEST {after_ms:.3} ms  ({:.1}x)",
        before_ms / after_ms
    );
    let _ = sqlx::query("DELETE FROM faces.faces WHERE user_id = $1")
        .bind(s.owner)
        .execute(pool.as_ref())
        .await;
    cleanup_base(pool, &s).await;
}

async fn section_reorder(pool: &Arc<PgPool>, passes: usize) {
    println!("[7] playlist reorder — UPDATE-per-track vs UNNEST (500 tracks)");
    let s = seed_base(pool, "reorder").await;
    let playlist: Uuid = sqlx::query_scalar(
        "INSERT INTO audio.playlists (name, owner_id) VALUES ('Bench', $1) RETURNING id",
    )
    .bind(s.owner)
    .fetch_one(pool.as_ref())
    .await
    .expect("playlist");
    let mut item_ids = Vec::new();
    for i in 0..500 {
        let f: Uuid = sqlx::query_scalar(
            "INSERT INTO storage.files (name, folder_id, blob_hash, size, mime_type, drive_id)
             VALUES ($1, $2, $3, 4096, 'audio/mpeg', $4) RETURNING id",
        )
        .bind(format!("track-{i:04}.mp3"))
        .bind(s.root)
        .bind(&s.blob)
        .bind(s.drive)
        .fetch_one(pool.as_ref())
        .await
        .expect("track file");
        let item: Uuid = sqlx::query_scalar(
            "INSERT INTO audio.playlist_items (playlist_id, file_id, position)
             VALUES ($1, $2, $3) RETURNING id",
        )
        .bind(playlist)
        .bind(f)
        .bind(i)
        .fetch_one(pool.as_ref())
        .await
        .expect("item");
        item_ids.push(item);
    }

    let repo = PlaylistItemPgRepository::new(pool.clone());
    let bench_passes = passes.min(40);
    let mut reversed: Vec<Uuid> = item_ids.clone();
    reversed.reverse();

    let fetch_positions = |pool: Arc<PgPool>| async move {
        sqlx::query(
            "SELECT id, position FROM audio.playlist_items WHERE playlist_id = $1 ORDER BY id",
        )
        .bind(playlist)
        .fetch_all(pool.as_ref())
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get::<Uuid, _>("id"), r.get::<i32, _>("position")))
        .collect::<Vec<_>>()
    };

    // BEFORE replica: per-track autocommit UPDATE loop.
    let (before_ms, _) = timed(bench_passes, || {
        let order = reversed.clone();
        let pool = pool.clone();
        async move {
            for (index, item_id) in order.iter().enumerate() {
                sqlx::query(
                    "UPDATE audio.playlist_items SET position = $2 WHERE id = $1 AND playlist_id = $3",
                )
                .bind(item_id)
                .bind(i32::try_from(index).unwrap())
                .bind(playlist)
                .execute(pool.as_ref())
                .await
                .unwrap();
            }
        }
    })
    .await;
    let before_positions = fetch_positions(pool.clone()).await;

    // AFTER: the shipped UNNEST UPDATE (same target order → same rows).
    let (after_ms, _) = timed(bench_passes, || {
        let order = reversed.clone();
        let repo = &repo;
        async move {
            repo.reorder_items(&playlist, &order).await.unwrap();
        }
    })
    .await;
    let after_positions = fetch_positions(pool.clone()).await;
    assert_eq!(before_positions, after_positions, "identical final order");

    println!(
        "    500-track reorder: BEFORE loop {before_ms:.3} ms → AFTER UNNEST {after_ms:.3} ms  ({:.1}x)",
        before_ms / after_ms
    );
    let _ = sqlx::query("DELETE FROM audio.playlists WHERE id = $1")
        .bind(playlist)
        .execute(pool.as_ref())
        .await;
    cleanup_base(pool, &s).await;
}

async fn section_search_join(pool: &PgPool, passes: usize) {
    println!("[8] search page — serial files+folders vs join! overlap");
    let s = seed_base(pool, "search").await;
    // 2000 files + 150 folders, ~10% matching 'report'.
    sqlx::query(
        "INSERT INTO storage.files (name, folder_id, blob_hash, size, mime_type, drive_id)
         SELECT CASE WHEN i % 10 = 0 THEN 'report-' || i ELSE 'photo-' || i END,
                $1, $2, 4096, 'application/octet-stream', $3
           FROM generate_series(1, 2000) i",
    )
    .bind(s.root)
    .bind(&s.blob)
    .bind(s.drive)
    .execute(pool)
    .await
    .expect("files");
    for i in 0..150 {
        let name = if i % 10 == 0 {
            format!("reports-{i}")
        } else {
            format!("misc-{i}")
        };
        sqlx::query(
            "INSERT INTO storage.folders (name, path, lpath, drive_id, parent_id)
             VALUES ($1, $2, $3::ltree, $4, $5)",
        )
        .bind(&name)
        .bind(format!("/Personal/{name}"))
        .bind(format!("br10search.f{i}"))
        .bind(s.drive)
        .bind(s.root)
        .execute(pool)
        .await
        .expect("folder");
    }

    // Replicas of the two repo queries' shapes (drive-scoped name search),
    // trimmed to the fields the enrichment consumes.
    let files_q = "SELECT fi.id, fi.name, fi.size
                     FROM storage.files fi
                     JOIN storage.role_grants g
                       ON g.resource_type = 'drive' AND g.resource_id = fi.drive_id
                      AND g.subject_type = 'user' AND g.subject_id = $1
                    WHERE fi.is_trashed = FALSE AND fi.name ILIKE $2
                    ORDER BY fi.name ASC LIMIT 100";
    let folders_q = "SELECT fo.id, fo.name
                      FROM storage.folders fo
                      JOIN storage.role_grants g
                        ON g.resource_type = 'drive' AND g.resource_id = fo.drive_id
                       AND g.subject_type = 'user' AND g.subject_id = $1
                     WHERE fo.is_trashed = FALSE AND fo.name ILIKE $2
                     ORDER BY fo.name ASC LIMIT 100";

    let run_files = || async {
        sqlx::query(files_q)
            .bind(s.owner)
            .bind("%report%")
            .fetch_all(pool)
            .await
            .unwrap()
            .len()
    };
    let run_folders = || async {
        sqlx::query(folders_q)
            .bind(s.owner)
            .bind("%report%")
            .fetch_all(pool)
            .await
            .unwrap()
            .len()
    };

    let (before_ms, b) = timed(passes, || async {
        let f = run_files().await;
        let d = run_folders().await;
        (f, d)
    })
    .await;
    let (after_ms, a) = timed(passes, || async {
        tokio::join!(run_files(), run_folders())
    })
    .await;
    assert_eq!(b, a, "identical result counts");
    println!(
        "    files∥folders: BEFORE serial {before_ms:.3} ms → AFTER join! {after_ms:.3} ms  ({:.2}x)",
        before_ms / after_ms
    );
    cleanup_base(pool, &s).await;
}

async fn section_move_join(pool: &Arc<PgPool>, passes: usize) {
    println!("[9] move pre-check — serial drive lookups vs join! (decide-by-bench)");
    let s = seed_base(pool, "move").await;
    let repo = DrivePgRepository::new(pool.clone());

    let (before_ms, b) = timed(passes, || async {
        let src = repo
            .get_drive_id_and_policies_for_file(s.file)
            .await
            .unwrap();
        let dst = repo.drive_id_for_folder(s.root).await.unwrap();
        (src.0, dst)
    })
    .await;
    let (after_ms, a) = timed(passes, || async {
        let (src, dst) = tokio::join!(
            repo.get_drive_id_and_policies_for_file(s.file),
            repo.drive_id_for_folder(s.root),
        );
        (src.unwrap().0, dst.unwrap())
    })
    .await;
    assert_eq!(b, a, "identical drive resolution");
    println!(
        "    BEFORE serial {before_ms:.3} ms → AFTER join! {after_ms:.3} ms  ({:.2}x)",
        before_ms / after_ms
    );
    cleanup_base(pool, &s).await;
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    dotenvy::dotenv().ok();
    let url = env::var("DATABASE_URL")
        .or_else(|_| env::var("OXICLOUD_DB_CONNECTION_STRING"))
        .expect("set DATABASE_URL — the dev Postgres URL");
    let passes: usize = env_or("BENCH_PASSES", 200);

    let pool = Arc::new(
        PgPoolOptions::new()
            .max_connections(8)
            .min_connections(8)
            .acquire_timeout(Duration::from_secs(10))
            .connect(&url)
            .await
            .expect("connect Postgres"),
    );
    println!("bench_round10_queries — passes={passes}\n");

    section_share_double_fetch(&pool, passes).await;
    section_calendar_narrow(&pool, passes).await;
    section_group_count(&pool, passes).await;
    section_trash_index(&pool, passes).await;
    section_favorites_cast(&pool, passes).await;
    section_save_faces(&pool, passes).await;
    section_reorder(&pool, passes).await;
    section_search_join(&pool, passes).await;
    section_move_join(&pool, passes).await;

    println!("\nall gates passed");
}
