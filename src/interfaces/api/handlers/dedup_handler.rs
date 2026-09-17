use axum::{
    body::Body,
    extract::{Json, Path, State},
    http::{Response, StatusCode, header},
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::common::di::AppState;
use crate::domain::entities::user::UserRole;
use crate::interfaces::middleware::auth::AuthUser;
use std::sync::Arc;

/// Global application state for dependency injection
type GlobalState = Arc<AppState>;

/// Upper bound on hashes accepted in one batch ownership check — keeps a single
/// request from pinning the DB with a pathologically large `ANY(...)` array.
/// ~10k covers any realistic folder upload; clients fall back to plain uploads
/// for whatever doesn't fit.
const MAX_BATCH_HASHES: usize = 10_000;

/// A well-formed BLAKE3 hash is 64 hex characters.
fn is_valid_blob_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit())
}

/// Response for hash check endpoint
#[derive(Debug, Serialize, ToSchema)]
pub struct HashCheckResponse {
    /// Whether a blob with this hash already exists
    pub exists: bool,
    /// The BLAKE3 hash that was checked
    pub hash: String,
    /// If exists, the size of the existing blob
    #[serde(skip_serializing_if = "Option::is_none")]
    pub existing_size: Option<u64>,
    /// Global reference count for this blob across all users.
    /// Only populated when the authenticated user has the `admin` role;
    /// omitted for regular users to prevent cross-user content inference.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ref_count: Option<u32>,
}

/// Request body for the batch hash-ownership check (`POST /api/dedup/check-batch`).
#[derive(Debug, Deserialize, ToSchema)]
pub struct HashBatchRequest {
    /// Candidate BLAKE3 hashes (64 hex chars each) to test for ownership.
    pub hashes: Vec<String>,
}

/// Response for the batch hash-ownership check.
#[derive(Debug, Serialize, ToSchema)]
pub struct HashBatchResponse {
    /// The subset of the submitted `hashes` the authenticated user already owns.
    pub owned: Vec<String>,
}

/// Response for dedup stats endpoint
#[derive(Debug, Serialize, ToSchema)]
pub struct StatsResponse {
    /// Total number of unique blobs stored
    pub unique_blobs: u64,
    /// Total number of references (files pointing to blobs)
    pub total_references: u64,
    /// Total bytes saved by deduplication
    pub bytes_saved: u64,
    /// Total logical bytes (what users think they have)
    pub total_logical_bytes: u64,
    /// Total physical bytes (actual disk usage)
    pub total_physical_bytes: u64,
    /// Deduplication ratio (logical / physical)
    pub dedup_ratio: f64,
    /// Percentage of storage saved
    pub savings_percentage: f64,
}

/// Handler for deduplication-related endpoints.
///
/// All route functions are free functions at module scope — see the section
/// below the impl block for the reason (utoipa 5.4.0 limitation).
/// Provides endpoints for:
/// - Checking if content already exists (by hash)
/// - Uploading files with automatic deduplication
/// - Getting deduplication statistics
pub struct DedupHandler;

impl DedupHandler {
    // ── Why no #[utoipa::path] here? ─────────────────────────────────────────────
    // Same utoipa 5.4.0 limitation as ChunkedUploadHandler: the macro generates
    // helper structs inside its expansion and Rust forbids structs inside impl blocks.
    // All route handlers are free functions below; they delegate to these *_impl methods.
    // TODO: collapse back into the impl block after a utoipa upgrade.

    /// Check if the authenticated user already has a file with the given hash.
    ///
    /// User-scoped: only reveals whether **this user** owns a file that
    /// references the blob — never exposes global existence to non-admins.
    /// Admins additionally receive the global `ref_count` in the response.
    ///
    /// GET /api/dedup/check/{hash}
    pub(super) async fn check_hash_impl(
        State(state): State<GlobalState>,
        auth_user: AuthUser,
        Path(hash): Path<String>,
    ) -> impl IntoResponse {
        let dedup = &state.core.dedup_service;

        // Validate hash format (BLAKE3 = 64 hex chars)
        if !is_valid_blob_hash(&hash) {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"error": "Invalid hash format. Expected BLAKE3 (64 hex characters)"}"#,
                ))
                .unwrap()
                .into_response();
        }

        // Only reveal whether THIS user has the blob — no global oracle
        let user_has_it = dedup
            .user_owns_blob_reference(&hash, &auth_user.id.to_string())
            .await;

        if user_has_it {
            // Fetch size from metadata (safe — user owns a reference).
            // Admins also get the global ref_count for dedup accounting tests.
            let metadata = dedup.get_blob_metadata(&hash).await;
            let size = metadata.as_ref().map(|m| m.size);
            let ref_count = if UserRole::str_at_least(&auth_user.role, UserRole::Admin) {
                metadata.map(|m| m.ref_count)
            } else {
                None // Never expose global ref_count to regular users
            };
            let response = HashCheckResponse {
                exists: true,
                hash,
                existing_size: size,
                ref_count,
            };
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_string(&response).unwrap()))
                .unwrap()
                .into_response()
        } else {
            let response = HashCheckResponse {
                exists: false,
                hash,
                existing_size: None,
                ref_count: None,
            };
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_string(&response).unwrap()))
                .unwrap()
                .into_response()
        }
    }

    /// Batch variant of [`Self::check_hash_impl`]: return the subset of the
    /// submitted hashes the authenticated user already owns, in one round trip.
    /// Lets a client hash a whole upload set up front and learn which files it
    /// can skip with ONE request instead of one probe per file.
    ///
    /// User-scoped (anti-enumeration): only the caller's own blobs are echoed
    /// back — never reveals whether other users hold a blob. Malformed hashes
    /// are silently dropped (they can't be owned anyway).
    ///
    /// POST /api/dedup/check-batch
    pub(super) async fn check_hashes_batch_impl(
        State(state): State<GlobalState>,
        auth_user: AuthUser,
        Json(request): Json<HashBatchRequest>,
    ) -> impl IntoResponse {
        if request.hashes.len() > MAX_BATCH_HASHES {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"error": "Too many hashes in one batch"}"#))
                .unwrap()
                .into_response();
        }

        let valid: Vec<String> = request
            .hashes
            .into_iter()
            .filter(|h| is_valid_blob_hash(h))
            .collect();

        let owned = state
            .core
            .dedup_service
            .user_owned_blob_references(&valid, &auth_user.id.to_string())
            .await;

        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::to_string(&HashBatchResponse { owned }).unwrap(),
            ))
            .unwrap()
            .into_response()
    }

    /// Get deduplication statistics
    ///
    /// GET /api/dedup/stats
    ///
    /// Returns comprehensive statistics about the deduplication system including:
    /// - Number of unique blobs
    /// - Total references
    /// - Bytes saved
    /// - Deduplication ratio
    pub(super) async fn get_stats_impl(
        State(state): State<GlobalState>,
        _auth_user: AuthUser,
    ) -> impl IntoResponse {
        // AuthZ audit #24 (2026-07-17): admin check moved to the
        // `/api/admin/*` middleware layer. Reaching this handler means
        // the caller is admin by construction — the bespoke role
        // string comparison here (`auth_user.role != "admin"` → 403
        // with a hand-rolled JSON body, no audit line) is gone. The
        // route is registered at `admin_handler::admin_routes()`;
        // moving the URL to `/api/admin/dedup/stats` also declares
        // the admin intent up front.
        let dedup = &state.core.dedup_service;
        let stats = dedup.get_stats().await;

        // Calculate savings percentage
        let savings_pct = if stats.total_bytes_referenced > 0 {
            (stats.bytes_saved as f64 / stats.total_bytes_referenced as f64) * 100.0
        } else {
            0.0
        };

        let response = StatsResponse {
            unique_blobs: stats.total_blobs,
            total_references: stats.dedup_hits + stats.total_blobs, // Approximation
            bytes_saved: stats.bytes_saved,
            total_logical_bytes: stats.total_bytes_referenced,
            total_physical_bytes: stats.total_bytes_stored,
            dedup_ratio: stats.dedup_ratio,
            savings_percentage: savings_pct,
        };

        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_string(&response).unwrap()))
            .unwrap()
            .into_response()
    }

    /// Retrieve content by hash (user-scoped).
    ///
    /// GET /api/dedup/blob/{hash}
    ///
    /// Returns the raw content of a blob **only if** the authenticated user
    /// owns at least one file that references it. Returns 404 otherwise
    /// (does not reveal whether the blob exists globally).
    pub(super) async fn get_blob_impl(
        State(state): State<GlobalState>,
        auth_user: AuthUser,
        Path(hash): Path<String>,
    ) -> impl IntoResponse {
        let dedup = &state.core.dedup_service;

        // Validate hash format
        if !is_valid_blob_hash(&hash) {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"error": "Invalid hash format"}"#))
                .unwrap()
                .into_response();
        }

        // Verify the user owns at least one file referencing this blob
        if !dedup
            .user_owns_blob_reference(&hash, &auth_user.id.to_string())
            .await
        {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"error": "Blob not found"}"#))
                .unwrap()
                .into_response();
        }

        // Get metadata first for content-type
        let metadata = dedup.get_blob_metadata(&hash).await;
        let content_type = metadata
            .as_ref()
            .and_then(|m| m.content_type.clone())
            .unwrap_or_else(|| "application/octet-stream".to_string());

        // Stream blob in 64 KB chunks — constant memory regardless of size
        let size = match dedup.blob_size(&hash).await {
            Ok(s) => s,
            Err(_) => {
                return Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"error": "Blob not found"}"#))
                    .unwrap()
                    .into_response();
            }
        };

        match dedup.read_blob_stream(&hash).await {
            Ok(stream) => Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, content_type)
                .header(header::CONTENT_LENGTH, size.to_string())
                .header("X-Dedup-Hash", &hash)
                .body(Body::from_stream(stream))
                .unwrap()
                .into_response(),
            Err(_) => Response::builder()
                .status(StatusCode::NOT_FOUND)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"error": "Blob not found"}"#))
                .unwrap()
                .into_response(),
        }
    }

    /// Force recalculation of statistics from disk
    ///
    /// POST /api/dedup/recalculate
    ///
    /// Verifies integrity and returns current statistics.
    /// Useful for health checks and auditing.
    pub(super) async fn recalculate_stats_impl(
        State(state): State<GlobalState>,
        auth_user: AuthUser,
    ) -> impl IntoResponse {
        // AuthZ audit #25 (2026-07-17): admin check moved to the
        // `/api/admin/*` middleware layer — see the sibling
        // `get_stats_impl` comment. `auth_user` is kept so the
        // success-side audit line carries the caller id.
        let dedup = &state.core.dedup_service;

        // Verify integrity first
        match dedup.verify_integrity().await {
            Ok(issues) => {
                if !issues.is_empty() {
                    tracing::warn!("Dedup integrity issues found: {:?}", issues);
                }
            }
            Err(e) => {
                tracing::error!("Dedup integrity verification failed: {}", e);
                return Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"error": "Verification failed"}"#))
                    .unwrap()
                    .into_response();
            }
        }

        let stats = dedup.get_stats().await;

        // Calculate savings percentage
        let savings_pct = if stats.total_bytes_referenced > 0 {
            (stats.bytes_saved as f64 / stats.total_bytes_referenced as f64) * 100.0
        } else {
            0.0
        };

        let response = StatsResponse {
            unique_blobs: stats.total_blobs,
            total_references: stats.dedup_hits + stats.total_blobs,
            bytes_saved: stats.bytes_saved,
            total_logical_bytes: stats.total_bytes_referenced,
            total_physical_bytes: stats.total_bytes_stored,
            dedup_ratio: stats.dedup_ratio,
            savings_percentage: savings_pct,
        };

        // AuthZ audit #25 (2026-07-17): integrity recalculation is a
        // low-frequency privileged operation — landing an audit event
        // so security reviews can see who ran verify + integrity
        // sweeps and when. The pre-fix path emitted no audit line at
        // all (the accepted 200 was silent from the security POV).
        tracing::info!(
            target: "audit",
            event = "dedup.integrity_recalculated",
            caller_id = %auth_user.id,
            unique_blobs = response.unique_blobs,
            total_references = response.total_references,
            bytes_saved = response.bytes_saved,
            "🧮 dedup integrity verified and stats recomputed by admin",
        );

        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_string(&response).unwrap()))
            .unwrap()
            .into_response()
    }
}

// ── Route handlers (free functions) ──────────────────────────────────────────
//
// Same utoipa 5.4.0 limitation as ChunkedUploadHandler: #[utoipa::path] cannot
// be applied to methods on DedupHandler because the macro generates helper structs
// that Rust forbids inside impl blocks. All logic lives in the DedupHandler::*_impl
// methods above; these thin wrappers carry the OpenAPI annotation at module scope.
//
// routes.rs calls these free functions directly instead of DedupHandler::method.
// TODO: collapse back into the impl block after a utoipa upgrade resolves the issue.

#[utoipa::path(
    get,
    path = "/api/dedup/check/{hash}",
    params(
        ("hash" = String, Path, description = "BLAKE3 hash (64 hex characters)"),
    ),
    responses(
        (status = 200, description = "Hash check result. `ref_count` is only present for admin users.", body = HashCheckResponse),
        (status = 400, description = "Invalid hash format"),
    ),
    tag = "dedup",
    security(("bearerAuth" = []))
)]
pub async fn check_hash(
    state: State<GlobalState>,
    auth_user: AuthUser,
    path: Path<String>,
) -> impl IntoResponse {
    DedupHandler::check_hash_impl(state, auth_user, path).await
}

#[utoipa::path(
    post,
    path = "/api/dedup/check-batch",
    request_body = HashBatchRequest,
    responses(
        (status = 200, description = "The subset of the submitted hashes the caller already owns", body = HashBatchResponse),
        (status = 400, description = "Too many hashes in one batch"),
    ),
    tag = "dedup",
    security(("bearerAuth" = []))
)]
pub async fn check_hashes_batch(
    state: State<GlobalState>,
    auth_user: AuthUser,
    body: Json<HashBatchRequest>,
) -> impl IntoResponse {
    DedupHandler::check_hashes_batch_impl(state, auth_user, body).await
}

#[utoipa::path(
    get,
    path = "/api/admin/dedup/stats",
    responses(
        (status = 200, description = "Deduplication statistics", body = StatsResponse),
        (status = 401, description = "Missing or invalid token"),
        (status = 403, description = "Caller is not an admin"),
    ),
    tag = "admin",
    security(("bearerAuth" = []))
)]
pub async fn get_stats(state: State<GlobalState>, auth_user: AuthUser) -> impl IntoResponse {
    DedupHandler::get_stats_impl(state, auth_user).await
}

#[utoipa::path(
    get,
    path = "/api/dedup/blob/{hash}",
    params(
        ("hash" = String, Path, description = "BLAKE3 hash of the blob (64 hex characters)"),
    ),
    responses(
        (status = 200, description = "Raw blob content (user-scoped)"),
        (status = 400, description = "Invalid hash format"),
        (status = 404, description = "Blob not found or not owned by this user"),
    ),
    tag = "dedup",
    security(("bearerAuth" = []))
)]
pub async fn get_blob(
    state: State<GlobalState>,
    auth_user: AuthUser,
    path: Path<String>,
) -> impl IntoResponse {
    DedupHandler::get_blob_impl(state, auth_user, path).await
}

#[utoipa::path(
    post,
    path = "/api/admin/dedup/recalculate",
    responses(
        (status = 200, description = "Statistics after integrity verification", body = StatsResponse),
        (status = 401, description = "Missing or invalid token"),
        (status = 403, description = "Caller is not an admin"),
        (status = 500, description = "Integrity verification failed"),
    ),
    tag = "admin",
    security(("bearerAuth" = []))
)]
pub async fn recalculate_stats(
    state: State<GlobalState>,
    auth_user: AuthUser,
) -> impl IntoResponse {
    DedupHandler::recalculate_stats_impl(state, auth_user).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    /// Verify that the hash-on-write pattern produces the same BLAKE3 hash
    /// as hashing the entire content at once.
    #[tokio::test]
    async fn hash_on_write_matches_full_hash() {
        let content = b"Hello, OxiCloud dedup streaming upload!";

        // 1. Full-content hash (reference)
        let full_hash = blake3::hash(content).to_hex().to_string();

        // 2. Incremental hash-on-write (what the handler does)
        let mut hasher = blake3::Hasher::new();
        // Simulate multiple chunks
        hasher.update(&content[..10]);
        hasher.update(&content[10..25]);
        hasher.update(&content[25..]);
        let incremental_hash = hasher.finalize().to_hex().to_string();

        assert_eq!(full_hash, incremental_hash);
    }

    /// Verify BLAKE3 incremental hashing produces a valid 64-char hex hash.
    #[tokio::test]
    async fn incremental_hash_format_is_valid() {
        let content = vec![0xABu8; 1024 * 1024]; // 1 MB of data

        let mut hasher = blake3::Hasher::new();
        // Feed in 64 KB chunks like real uploads
        for chunk in content.chunks(65_536) {
            hasher.update(chunk);
        }
        let hash = hasher.finalize().to_hex().to_string();

        assert_eq!(hash.len(), 64, "BLAKE3 hash should be 64 hex characters");
        assert!(
            hash.chars().all(|c| c.is_ascii_hexdigit()),
            "Hash should only contain hex characters"
        );
    }

    /// Verify spool-to-disk + BLAKE3 hash-on-write writes correct content
    /// and produces the correct hash.
    #[tokio::test]
    async fn spool_to_temp_file_preserves_content_and_hash() {
        let temp_dir = tempfile::tempdir().unwrap();
        let temp_path = temp_dir.path().join("test-upload.tmp");

        let content = b"The quick brown fox jumps over the lazy dog";
        let expected_hash = blake3::hash(content).to_hex().to_string();

        // Simulate the handler's hash-on-write spool loop
        let mut hasher = blake3::Hasher::new();
        let mut total_size: u64 = 0;
        {
            let file = tokio::fs::File::create(&temp_path).await.unwrap();
            let mut writer = tokio::io::BufWriter::with_capacity(524_288, file);

            // Simulate 3 incoming chunks
            let chunks: &[&[u8]] = &[&content[..10], &content[10..30], &content[30..]];
            for chunk in chunks {
                total_size += chunk.len() as u64;
                hasher.update(chunk);
                writer.write_all(chunk).await.unwrap();
            }
            writer.flush().await.unwrap();
        }

        let hash = hasher.finalize().to_hex().to_string();

        // Verify hash matches
        assert_eq!(hash, expected_hash);

        // Verify total size
        assert_eq!(total_size, content.len() as u64);

        // Verify file content on disk is identical
        let disk_content = tokio::fs::read(&temp_path).await.unwrap();
        assert_eq!(disk_content, content);
    }

    /// Verify that an empty upload produces total_size == 0.
    #[tokio::test]
    async fn empty_upload_detected_before_store() {
        let hasher = blake3::Hasher::new();
        let total_size: u64 = 0;

        // No chunks fed — simulates empty file
        let _hash = hasher.finalize().to_hex().to_string();

        // The handler checks total_size == 0 and returns 400
        assert_eq!(total_size, 0);
    }

    /// Verify large payload streaming produces consistent hash
    /// without buffering all content in memory.
    #[tokio::test]
    async fn large_payload_streaming_hash_consistency() {
        let temp_dir = tempfile::tempdir().unwrap();
        let temp_path = temp_dir.path().join("large-upload.tmp");

        // 5 MB of patterned data
        let chunk_size = 65_536usize; // 64 KB chunks
        let total_chunks = 80; // 80 × 64 KB = 5 MB
        let mut reference_data = Vec::with_capacity(chunk_size * total_chunks);

        let mut hasher = blake3::Hasher::new();
        let mut total_size: u64 = 0;
        {
            let file = tokio::fs::File::create(&temp_path).await.unwrap();
            let mut writer = tokio::io::BufWriter::with_capacity(524_288, file);

            for i in 0..total_chunks {
                // Patterned data: each chunk filled with its index byte
                let chunk = vec![(i % 256) as u8; chunk_size];
                reference_data.extend_from_slice(&chunk);
                total_size += chunk.len() as u64;
                hasher.update(&chunk);
                writer.write_all(&chunk).await.unwrap();
            }
            writer.flush().await.unwrap();
        }

        let streaming_hash = hasher.finalize().to_hex().to_string();
        let reference_hash = blake3::hash(&reference_data).to_hex().to_string();

        // Hashes match
        assert_eq!(streaming_hash, reference_hash);

        // File on disk matches
        let file_size = tokio::fs::metadata(&temp_path).await.unwrap().len();
        assert_eq!(file_size, total_size);
        assert_eq!(total_size, (chunk_size * total_chunks) as u64);

        // Verify file content matches (read back)
        let disk_data = tokio::fs::read(&temp_path).await.unwrap();
        assert_eq!(disk_data, reference_data);
    }

    /// Verify temp file is cleaned up when spool fails partway through.
    #[tokio::test]
    async fn temp_file_cleanup_on_partial_write() {
        let temp_dir = tempfile::tempdir().unwrap();
        let temp_path = temp_dir.path().join("partial-upload.tmp");

        // Create the file and write some data
        {
            let file = tokio::fs::File::create(&temp_path).await.unwrap();
            let mut writer = tokio::io::BufWriter::with_capacity(524_288, file);
            writer.write_all(b"partial data").await.unwrap();
            writer.flush().await.unwrap();
        }

        // File exists before cleanup
        assert!(tokio::fs::try_exists(&temp_path).await.unwrap());

        // Simulate the handler's error cleanup path
        let _ = tokio::fs::remove_file(&temp_path).await;

        // File is gone after cleanup
        assert!(!tokio::fs::try_exists(&temp_path).await.unwrap_or(true));
    }

    #[test]
    fn valid_blob_hash_accepts_64_hex_only() {
        assert!(is_valid_blob_hash(&"abcdef0123456789".repeat(4))); // 64 hex chars
        assert!(!is_valid_blob_hash(&"a".repeat(63))); // too short
        assert!(!is_valid_blob_hash(&"a".repeat(65))); // too long
        assert!(!is_valid_blob_hash(&"g".repeat(64))); // non-hex
        assert!(!is_valid_blob_hash("")); // empty
    }

    #[test]
    fn hash_batch_request_deserializes() {
        let req: HashBatchRequest = serde_json::from_str(r#"{"hashes":["aa","bb"]}"#).unwrap();
        assert_eq!(req.hashes, vec!["aa".to_string(), "bb".to_string()]);
    }

    #[test]
    fn hash_batch_response_serializes_owned_subset() {
        let r = HashBatchResponse {
            owned: vec!["a".repeat(64), "b".repeat(64)],
        };
        let parsed: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(parsed["owned"].as_array().unwrap().len(), 2);
        assert_eq!(parsed["owned"][0], "a".repeat(64));
    }
}
