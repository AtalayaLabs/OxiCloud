//! PostgreSQL repository for the People (faces) feature.
//!
//! Embeddings are stored as `BYTEA` (512 × little-endian `f32`); there is no
//! pgvector dependency. Similarity search / clustering is done in-app over the
//! decoded vectors (see `PeopleService`).

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::application::ports::face_ports::FaceRepository;
use crate::common::errors::DomainError;
use crate::domain::entities::face::{BoundingBox, Face, FaceBox, Person};

/// Row shape for `faces.faces` selects (avoids `clippy::type_complexity`).
type FaceRow = (
    Uuid,           // id
    Uuid,           // file_id
    Uuid,           // user_id
    Option<Uuid>,   // person_id
    Vec<f32>,       // bbox (REAL[])
    f32,            // det_score
    Option<f32>,    // quality
    Vec<u8>,        // embedding (BYTEA)
    Option<String>, // blob_hash
    DateTime<Utc>,  // created_at
);

type PersonRow = (
    Uuid,           // id
    Uuid,           // user_id
    Option<String>, // display_name
    Option<Uuid>,   // cover_face_id
    bool,           // is_hidden
    DateTime<Utc>,  // created_at
);

fn embedding_to_bytes(e: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(e.len() * 4);
    for v in e {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

fn bytes_to_embedding(b: &[u8]) -> Vec<f32> {
    // `as_chunks::<4>` (stable since Rust 1.88) hands back `&[[u8; 4]]`
    // typed at the array level, so the closure gets a `&[u8; 4]` and
    // the `[c[0], c[1], c[2], c[3]]` array-copy dance from the old
    // `chunks_exact(4)` shape collapses to a plain deref. Any trailing
    // bytes that aren't a multiple of 4 land in `.1` and are dropped
    // — same semantics as `chunks_exact` which iterated only the
    // aligned prefix.
    b.as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes(*c))
        .collect()
}

fn row_to_face(r: FaceRow) -> Face {
    let (
        id,
        file_id,
        user_id,
        person_id,
        bbox,
        det_score,
        quality,
        embedding,
        blob_hash,
        created_at,
    ) = r;
    Face {
        id,
        file_id,
        user_id,
        person_id,
        bbox: BoundingBox::from_slice(&bbox),
        det_score,
        quality,
        embedding: bytes_to_embedding(&embedding),
        blob_hash,
        created_at,
    }
}

fn row_to_person(r: PersonRow) -> Person {
    let (id, user_id, display_name, cover_face_id, is_hidden, created_at) = r;
    Person {
        id,
        user_id,
        display_name,
        cover_face_id,
        is_hidden,
        created_at,
    }
}

fn db_err(ctx: &'static str, e: sqlx::Error) -> DomainError {
    DomainError::internal_error("FacePg", format!("{ctx}: {e}"))
}

const FACE_COLS: &str =
    "id, file_id, user_id, person_id, bbox, det_score, quality, embedding, blob_hash, created_at";
const PERSON_COLS: &str = "id, user_id, display_name, cover_face_id, is_hidden, created_at";

pub struct FacePgRepository {
    pool: Arc<PgPool>,
}

impl FacePgRepository {
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl FaceRepository for FacePgRepository {
    async fn save_faces(&self, faces: &[Face]) -> Result<(), DomainError> {
        if faces.is_empty() {
            return Ok(());
        }
        // One multi-row INSERT over parallel UNNEST arrays instead of one
        // round-trip per face — a group photo yields many faces per indexed
        // image. The `bbox` float4[] can't ride an array-of-arrays through
        // unnest (PG flattens), so its 4 components travel as 4 parallel
        // arrays and are reassembled server-side. A single statement is
        // atomic on its own; the per-row transaction wrapper is gone.
        let n = faces.len();
        let mut ids = Vec::with_capacity(n);
        let mut file_ids = Vec::with_capacity(n);
        let mut user_ids = Vec::with_capacity(n);
        let mut person_ids: Vec<Option<Uuid>> = Vec::with_capacity(n);
        let (mut bx, mut by, mut bw, mut bh) = (
            Vec::with_capacity(n),
            Vec::with_capacity(n),
            Vec::with_capacity(n),
            Vec::with_capacity(n),
        );
        let mut det_scores = Vec::with_capacity(n);
        let mut qualities: Vec<Option<f32>> = Vec::with_capacity(n);
        let mut embeddings = Vec::with_capacity(n);
        let mut blob_hashes: Vec<Option<&str>> = Vec::with_capacity(n);
        for f in faces {
            ids.push(f.id);
            file_ids.push(f.file_id);
            user_ids.push(f.user_id);
            person_ids.push(f.person_id);
            bx.push(f.bbox.x);
            by.push(f.bbox.y);
            bw.push(f.bbox.w);
            bh.push(f.bbox.h);
            det_scores.push(f.det_score);
            qualities.push(f.quality);
            embeddings.push(embedding_to_bytes(&f.embedding));
            blob_hashes.push(f.blob_hash.as_deref());
        }
        sqlx::query(
            r#"
            INSERT INTO faces.faces
                (id, file_id, user_id, person_id, bbox, det_score, quality, embedding, blob_hash)
            SELECT t.id, t.file_id, t.user_id, t.person_id,
                   ARRAY[t.bx, t.by, t.bw, t.bh]::real[],
                   t.det_score, t.quality, t.embedding, t.blob_hash
              FROM unnest($1::uuid[], $2::uuid[], $3::uuid[], $4::uuid[],
                          $5::real[], $6::real[], $7::real[], $8::real[],
                          $9::real[], $10::real[], $11::bytea[], $12::text[])
                   AS t(id, file_id, user_id, person_id,
                        bx, by, bw, bh, det_score, quality, embedding, blob_hash)
            "#,
        )
        .bind(&ids)
        .bind(&file_ids)
        .bind(&user_ids)
        .bind(&person_ids)
        .bind(&bx)
        .bind(&by)
        .bind(&bw)
        .bind(&bh)
        .bind(&det_scores)
        .bind(&qualities)
        .bind(&embeddings)
        .bind(&blob_hashes)
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| db_err("save_faces", e))?;
        Ok(())
    }

    async fn face_boxes_for_file(
        &self,
        file_id: Uuid,
        user_id: Uuid,
    ) -> Result<Vec<FaceBox>, DomainError> {
        // Narrow projection: the lightbox needs only (id, person_id, bbox), so
        // the 2 KiB embedding BYTEA + 6 unused columns stay in the DB and the
        // caller filter runs in SQL (idx_faces_file drives it) rather than in
        // Rust after a full-row fetch. See benches/ROUND14.md §Q1.
        let rows: Vec<(Uuid, Option<Uuid>, Vec<f32>)> = sqlx::query_as(
            "SELECT id, person_id, bbox FROM faces.faces WHERE file_id = $1 AND user_id = $2",
        )
        .bind(file_id)
        .bind(user_id)
        .fetch_all(self.pool.as_ref())
        .await
        .map_err(|e| db_err("face_boxes_for_file", e))?;
        Ok(rows
            .into_iter()
            .map(|(id, person_id, bbox)| FaceBox {
                id,
                person_id,
                bbox: BoundingBox::from_slice(&bbox),
            })
            .collect())
    }

    async fn delete_faces_for_file(&self, file_id: Uuid) -> Result<(), DomainError> {
        sqlx::query("DELETE FROM faces.faces WHERE file_id = $1")
            .bind(file_id)
            .execute(self.pool.as_ref())
            .await
            .map_err(|e| db_err("delete_faces_for_file", e))?;
        Ok(())
    }

    async fn faces_for_user(&self, user_id: Uuid) -> Result<Vec<Face>, DomainError> {
        let sql = format!("SELECT {FACE_COLS} FROM faces.faces WHERE user_id = $1");
        let rows: Vec<FaceRow> = sqlx::query_as(&sql)
            .bind(user_id)
            .fetch_all(self.pool.as_ref())
            .await
            .map_err(|e| db_err("faces_for_user", e))?;
        Ok(rows.into_iter().map(row_to_face).collect())
    }

    async fn faces_for_blob(
        &self,
        user_id: Uuid,
        blob_hash: &str,
    ) -> Result<Vec<Face>, DomainError> {
        let sql =
            format!("SELECT {FACE_COLS} FROM faces.faces WHERE user_id = $1 AND blob_hash = $2");
        let rows: Vec<FaceRow> = sqlx::query_as(&sql)
            .bind(user_id)
            .bind(blob_hash)
            .fetch_all(self.pool.as_ref())
            .await
            .map_err(|e| db_err("faces_for_blob", e))?;
        Ok(rows.into_iter().map(row_to_face).collect())
    }

    async fn person_face_stats(&self, user_id: Uuid) -> Result<Vec<(Uuid, i64)>, DomainError> {
        // Grouped COUNT — the People tab only needs per-person counts, so
        // this replaces a full faces_for_user scan that shipped a 2 KiB
        // embedding BYTEA per row (benches/PEOPLE-LIST.md).
        let rows: Vec<(Uuid, i64)> = sqlx::query_as(
            "SELECT person_id, COUNT(*) FROM faces.faces
              WHERE user_id = $1 AND person_id IS NOT NULL
              GROUP BY person_id",
        )
        .bind(user_id)
        .fetch_all(self.pool.as_ref())
        .await
        .map_err(|e| db_err("person_face_stats", e))?;
        Ok(rows)
    }

    async fn file_ids_for_faces(
        &self,
        user_id: Uuid,
        face_ids: &[Uuid],
    ) -> Result<std::collections::HashMap<Uuid, Uuid>, DomainError> {
        if face_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let rows: Vec<(Uuid, Uuid)> = sqlx::query_as(
            "SELECT id, file_id FROM faces.faces WHERE user_id = $1 AND id = ANY($2)",
        )
        .bind(user_id)
        .bind(face_ids)
        .fetch_all(self.pool.as_ref())
        .await
        .map_err(|e| db_err("file_ids_for_faces", e))?;
        Ok(rows.into_iter().collect())
    }

    async fn reassign_person_faces(
        &self,
        user_id: Uuid,
        from: Uuid,
        into: Uuid,
    ) -> Result<u64, DomainError> {
        let result = sqlx::query(
            "UPDATE faces.faces SET person_id = $3
              WHERE user_id = $1 AND person_id = $2",
        )
        .bind(user_id)
        .bind(from)
        .bind(into)
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| db_err("reassign_person_faces", e))?;
        Ok(result.rows_affected())
    }

    async fn assign_person(
        &self,
        face_id: Uuid,
        person_id: Option<Uuid>,
    ) -> Result<(), DomainError> {
        sqlx::query("UPDATE faces.faces SET person_id = $2 WHERE id = $1")
            .bind(face_id)
            .bind(person_id)
            .execute(self.pool.as_ref())
            .await
            .map_err(|e| db_err("assign_person", e))?;
        Ok(())
    }

    async fn assign_person_batch(
        &self,
        assignments: &[(Uuid, Option<Uuid>)],
    ) -> Result<(), DomainError> {
        if assignments.is_empty() {
            return Ok(());
        }
        let (face_ids, person_ids): (Vec<Uuid>, Vec<Option<Uuid>>) =
            assignments.iter().cloned().unzip();
        sqlx::query(
            "UPDATE faces.faces f SET person_id = u.pid
               FROM (SELECT unnest($1::uuid[]) AS fid, unnest($2::uuid[]) AS pid) u
              WHERE f.id = u.fid",
        )
        .bind(&face_ids)
        .bind(&person_ids)
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| db_err("assign_person_batch", e))?;
        Ok(())
    }

    async fn create_person(&self, person: &Person) -> Result<(), DomainError> {
        sqlx::query(
            r#"
            INSERT INTO faces.persons (id, user_id, display_name, cover_face_id, is_hidden)
            VALUES ($1, $2, $3, $4, $5)
            "#,
        )
        .bind(person.id)
        .bind(person.user_id)
        .bind(person.display_name.as_deref())
        .bind(person.cover_face_id)
        .bind(person.is_hidden)
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| db_err("create_person", e))?;
        Ok(())
    }

    async fn persons_for_user(&self, user_id: Uuid) -> Result<Vec<Person>, DomainError> {
        let sql = format!(
            "SELECT {PERSON_COLS} FROM faces.persons WHERE user_id = $1 ORDER BY created_at"
        );
        let rows: Vec<PersonRow> = sqlx::query_as(&sql)
            .bind(user_id)
            .fetch_all(self.pool.as_ref())
            .await
            .map_err(|e| db_err("persons_for_user", e))?;
        Ok(rows.into_iter().map(row_to_person).collect())
    }

    async fn rename_person(
        &self,
        user_id: Uuid,
        person_id: Uuid,
        name: Option<String>,
    ) -> Result<(), DomainError> {
        sqlx::query(
            "UPDATE faces.persons SET display_name = $3, updated_at = now() WHERE id = $2 AND user_id = $1",
        )
        .bind(user_id)
        .bind(person_id)
        .bind(name)
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| db_err("rename_person", e))?;
        Ok(())
    }

    async fn set_person_cover(
        &self,
        person_id: Uuid,
        cover_face_id: Uuid,
    ) -> Result<(), DomainError> {
        sqlx::query(
            "UPDATE faces.persons SET cover_face_id = $2, updated_at = now() WHERE id = $1",
        )
        .bind(person_id)
        .bind(cover_face_id)
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| db_err("set_person_cover", e))?;
        Ok(())
    }

    async fn set_person_hidden(
        &self,
        user_id: Uuid,
        person_id: Uuid,
        hidden: bool,
    ) -> Result<(), DomainError> {
        sqlx::query(
            "UPDATE faces.persons SET is_hidden = $3, updated_at = now() WHERE id = $2 AND user_id = $1",
        )
        .bind(user_id)
        .bind(person_id)
        .bind(hidden)
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| db_err("set_person_hidden", e))?;
        Ok(())
    }

    async fn files_for_person(
        &self,
        user_id: Uuid,
        person_id: Uuid,
    ) -> Result<Vec<Uuid>, DomainError> {
        let rows: Vec<(Uuid,)> = sqlx::query_as(
            r#"
            SELECT file_id
              FROM faces.faces
             WHERE user_id = $1 AND person_id = $2
             GROUP BY file_id
             ORDER BY max(created_at) DESC
            "#,
        )
        .bind(user_id)
        .bind(person_id)
        .fetch_all(self.pool.as_ref())
        .await
        .map_err(|e| db_err("files_for_person", e))?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    async fn delete_all_for_user(&self, user_id: Uuid) -> Result<(), DomainError> {
        let mut tx = self.pool.begin().await.map_err(|e| db_err("begin", e))?;
        sqlx::query("DELETE FROM faces.faces WHERE user_id = $1")
            .bind(user_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| db_err("delete_all_faces", e))?;
        sqlx::query("DELETE FROM faces.persons WHERE user_id = $1")
            .bind(user_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| db_err("delete_all_persons", e))?;
        tx.commit().await.map_err(|e| db_err("commit", e))?;
        Ok(())
    }
}
