//! Delta-upload protocol endpoints — "upload only what changed".
//!
//! Wire surface of [`DeltaUploadService`]; see that module (and
//! `docs/delta-upload-protocol.md`) for the protocol and its security
//! model. These handlers only authenticate, rate-limit and translate
//! the wire formats — every decision lives in the application service.
//!
//! Chunk-frame wire format (`PUT /api/files/delta/chunks`): the body is a
//! sequence of `[u32 big-endian length][length bytes]` frames, one per
//! chunk, with `Content-Type: application/octet-stream`. Frames are capped
//! at the CDC maximum chunk size (1 MiB) and the whole request at the same
//! per-request ceiling as resumable chunk PUTs (`chunk_max_bytes`).

use axum::{
    Json,
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use bytes::{Buf, Bytes, BytesMut};
use futures::{Stream, TryStreamExt};
use std::sync::Arc;
use tokio_stream::StreamExt;

use crate::application::services::delta_upload_service::{
    DeltaChunksResponse, DeltaCommitOutcome, DeltaCommitRequest, DeltaDownloadOutcome,
    DeltaDownloadRequest, DeltaManifestResponse, DeltaNegotiateRequest, DeltaNegotiateResponse,
};
use crate::common::di::AppState;
use crate::common::errors::DomainError;
use crate::infrastructure::services::dedup_service::CDC_MAX_CHUNK;
use crate::interfaces::errors::AppError;
use crate::interfaces::middleware::auth::AuthUser;
use http_body_util::BodyStream;
use serde::Serialize;
use utoipa::ToSchema;

/// 409 body of `POST /api/files/delta/commit` when chunks vanished between
/// negotiate and commit (GC race) or were never claimable: the client
/// uploads exactly these hashes and retries the same commit.
#[derive(Debug, Serialize, ToSchema)]
pub struct DeltaStillMissingResponse {
    pub still_missing: Vec<String>,
}

/// 404 body of `POST /api/files/delta/download` when some requested chunks
/// are not reachable through the caller's files (or don't exist — the two
/// are deliberately indistinguishable).
#[derive(Debug, Serialize, ToSchema)]
pub struct DeltaNotAvailableResponse {
    pub not_available: Vec<String>,
}

/// Per-caller flood guard shared by the delta endpoints.
fn check_rate_limit(state: &Arc<AppState>, auth_user: &AuthUser) -> Result<(), AppError> {
    if state
        .delta_upload_rate_limiter
        .check_and_increment(&auth_user.id.to_string())
        .is_err()
    {
        tracing::info!(
            target: "audit",
            event = "delta_upload.rejected",
            reason = "rate_limited",
            caller_id = %auth_user.id,
            "👮🏻‍♂️ Delta upload rejected: per-caller rate limit exceeded",
        );
        return Err(AppError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many delta-upload requests; please retry shortly",
            "RateLimited",
        ));
    }
    Ok(())
}

/// Parse a `[u32 BE length][bytes]` frame sequence from the request body.
///
/// Streaming: peak RAM is one frame (≤ 1 MiB) plus one HTTP frame,
/// regardless of how many chunks the request carries. `max_total` bounds
/// the whole request body.
fn parse_chunk_frames(
    body: Body,
    max_total: usize,
) -> impl Stream<Item = Result<Bytes, DomainError>> + Send {
    async_stream::try_stream! {
        let mut body_stream = BodyStream::new(body);
        let mut buf = BytesMut::new();
        let mut expecting: Option<usize> = None;
        let mut total: usize = 0;

        loop {
            // Drain every complete frame already buffered.
            loop {
                match expecting {
                    None => {
                        if buf.len() < 4 {
                            break;
                        }
                        let len =
                            u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
                        buf.advance(4);
                        if len == 0 || len > CDC_MAX_CHUNK {
                            Err(DomainError::validation_error(format!(
                                "Chunk frame of {len} bytes out of bounds (1 ..= {CDC_MAX_CHUNK})"
                            )))?;
                        }
                        expecting = Some(len);
                    }
                    Some(len) => {
                        if buf.len() < len {
                            break;
                        }
                        let frame = buf.split_to(len).freeze();
                        expecting = None;
                        yield frame;
                    }
                }
            }

            match body_stream.next().await {
                Some(Ok(http_frame)) => {
                    if let Some(data) = http_frame.data_ref() {
                        total += data.len();
                        if total > max_total {
                            Err(DomainError::validation_error(format!(
                                "Request body exceeds the {max_total}-byte per-request cap; \
                                 split the chunk upload into several requests"
                            )))?;
                        }
                        buf.extend_from_slice(data);
                    }
                }
                Some(Err(e)) => {
                    Err(DomainError::validation_error(format!(
                        "Failed to read request body: {e}"
                    )))?;
                }
                None => {
                    if expecting.is_some() || !buf.is_empty() {
                        Err(DomainError::validation_error(
                            "Truncated chunk frame at end of body",
                        ))?;
                    }
                    break;
                }
            }
        }
    }
}

#[utoipa::path(
    post,
    path = "/api/files/delta/negotiate",
    request_body = DeltaNegotiateRequest,
    responses(
        (status = 200, description = "Chunks the caller must upload (the rest are claimable without bytes)", body = DeltaNegotiateResponse),
        (status = 400, description = "Malformed chunk list (hash format, size bounds, count ceiling)"),
        (status = 429, description = "Rate limited"),
    ),
    security(("bearerAuth" = [])),
    tag = "delta-upload"
)]
pub async fn delta_negotiate(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    Json(request): Json<DeltaNegotiateRequest>,
) -> Result<impl IntoResponse, AppError> {
    check_rate_limit(&state, &auth_user)?;
    let response = state
        .applications
        .delta_upload_service
        .negotiate_with_perms(auth_user.id, &request)
        .await
        .map_err(AppError::from)?;
    Ok(Json(response))
}

#[utoipa::path(
    put,
    path = "/api/files/delta/chunks",
    request_body(content_type = "application/octet-stream",
        description = "Sequence of [u32 BE length][bytes] frames, one per chunk (each ≤ 1 MiB)"),
    responses(
        (status = 200, description = "Server-computed identity of every received frame, in order", body = DeltaChunksResponse),
        (status = 400, description = "Malformed framing, oversized frame, or oversized request"),
        (status = 429, description = "Rate limited"),
    ),
    security(("bearerAuth" = [])),
    tag = "delta-upload"
)]
pub async fn delta_upload_chunks(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    body: Body,
) -> Result<impl IntoResponse, AppError> {
    check_rate_limit(&state, &auth_user)?;
    let frames = parse_chunk_frames(body, state.core.config.storage.chunk_max_bytes);
    let response = state
        .applications
        .delta_upload_service
        .receive_chunks(frames)
        .await
        .map_err(AppError::from)?;
    Ok(Json(response))
}

#[utoipa::path(
    post,
    path = "/api/files/delta/commit",
    request_body = DeltaCommitRequest,
    responses(
        (status = 201, description = "File created from the committed chunk sequence", body = crate::application::dtos::file_dto::FileDto),
        (status = 200, description = "Existing file's content replaced", body = crate::application::dtos::file_dto::FileDto),
        (status = 400, description = "Malformed request, or the declared file_hash does not match the chunk sequence"),
        (status = 404, description = "Target folder/file not found or not accessible"),
        (status = 409, description = "Chunks not claimable — upload them and retry", body = DeltaStillMissingResponse),
        (status = 429, description = "Rate limited"),
        (status = 507, description = "Storage quota exceeded"),
    ),
    security(("bearerAuth" = [])),
    tag = "delta-upload"
)]
pub async fn delta_commit(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    Json(request): Json<DeltaCommitRequest>,
) -> Result<Response, AppError> {
    check_rate_limit(&state, &auth_user)?;
    let outcome = state
        .applications
        .delta_upload_service
        .commit_with_perms(auth_user.id, request)
        .await
        .map_err(AppError::from)?;
    Ok(match outcome {
        DeltaCommitOutcome::Done { mut file, created } => {
            let status = if created {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            };
            crate::interfaces::api::handlers::caller_flags::enrich_file_flags(
                &state,
                &mut file,
                crate::domain::services::authorization::Subject::User(auth_user.id),
            )
            .await;
            (status, Json(file)).into_response()
        }
        DeltaCommitOutcome::StillMissing(still_missing) => (
            StatusCode::CONFLICT,
            Json(DeltaStillMissingResponse { still_missing }),
        )
            .into_response(),
    })
}

#[utoipa::path(
    get,
    path = "/api/files/{id}/manifest",
    params(("id" = String, Path, description = "File ID")),
    responses(
        (status = 200, description = "Chunk recipe of the file (immutable per file_hash; served with ETag = file_hash)", body = DeltaManifestResponse),
        (status = 304, description = "Not modified (If-None-Match matched the current file_hash)"),
        (status = 404, description = "File not found, not accessible, or not owned by the caller"),
        (status = 429, description = "Rate limited"),
    ),
    security(("bearerAuth" = [])),
    tag = "delta-upload"
)]
pub async fn delta_file_manifest(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    Path(file_id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    check_rate_limit(&state, &auth_user)?;
    let manifest = state
        .applications
        .delta_upload_service
        .file_manifest_with_perms(auth_user.id, &file_id)
        .await
        .map_err(AppError::from)?;

    // A manifest is immutable for a given file_hash, so the hash IS the
    // strong validator: sync clients polling a file revalidate for free.
    let etag = format!("\"{}\"", manifest.file_hash);
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|inm| inm == etag)
    {
        return Ok(Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(header::ETAG, etag)
            .body(Body::empty())
            .unwrap());
    }
    Ok((
        StatusCode::OK,
        [
            (header::ETAG, etag),
            (header::CACHE_CONTROL, "private, no-cache".to_string()),
        ],
        Json(manifest),
    )
        .into_response())
}

#[utoipa::path(
    post,
    path = "/api/files/delta/download",
    request_body = DeltaDownloadRequest,
    responses(
        (status = 200, description = "Requested chunks as [u32 BE length][bytes] frames, in request order",
            content_type = "application/octet-stream"),
        (status = 400, description = "Malformed hashes, duplicates, or batch above the per-request ceiling"),
        (status = 404, description = "Some chunks are not available to this caller", body = DeltaNotAvailableResponse),
        (status = 429, description = "Rate limited"),
    ),
    security(("bearerAuth" = [])),
    tag = "delta-upload"
)]
pub async fn delta_download_chunks(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    Json(request): Json<DeltaDownloadRequest>,
) -> Result<Response, AppError> {
    check_rate_limit(&state, &auth_user)?;
    let service = state.applications.delta_upload_service.clone();
    let outcome = service
        .authorize_chunk_download_with_perms(auth_user.id, &request)
        .await
        .map_err(AppError::from)?;

    let ordered = match outcome {
        DeltaDownloadOutcome::NotAvailable(not_available) => {
            return Ok((
                StatusCode::NOT_FOUND,
                Json(DeltaNotAvailableResponse { not_available }),
            )
                .into_response());
        }
        DeltaDownloadOutcome::Ready(ordered) => ordered,
    };
    let total: u64 = ordered.iter().map(|(_, s)| 4 + s).sum();

    // Stream the frames: 4-byte length headers come from the (entitled)
    // index sizes; bytes stream straight from the blob backend. Peak RAM
    // is bounded by `read_prefetch` open streams (their first frame),
    // independent of batch size.
    //
    // `buffered(read_prefetch)` overlaps the NEXT chunk's open with the
    // current chunk's drain — the same combinator/tuning as the main CDC
    // download path (benches/BLOB-PREFETCH.md). The old per-chunk await
    // paid every open's full round-trip serially: on an object-store
    // backend a 64-chunk batch at ~30 ms first-byte cost ~1.9 s of pure
    // latency. Frames still arrive strictly in request order.
    let prefetch = service.read_prefetch().max(1);
    let svc = service.clone();
    // `futures::StreamExt` spelled out — this handler imports
    // `tokio_stream::StreamExt`, whose `map` adapter lacks `buffered`.
    let opened = futures::StreamExt::map(futures::stream::iter(ordered), move |(hash, size)| {
        let svc = svc.clone();
        async move {
            let chunk = svc
                .chunk_stream(&hash)
                .await
                .map_err(std::io::Error::other)?;
            let header = futures::stream::once(async move {
                Ok::<Bytes, std::io::Error>(Bytes::copy_from_slice(&(size as u32).to_be_bytes()))
            });
            Ok::<_, std::io::Error>(futures::StreamExt::chain(header, chunk))
        }
    });
    let body_stream: std::pin::Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>> =
        Box::pin(TryStreamExt::try_flatten(futures::StreamExt::buffered(
            opened, prefetch,
        )));
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, total.to_string())
        .body(Body::from_stream(body_stream))
        .unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode frames the way a client would.
    fn encode(frames: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for f in frames {
            out.extend_from_slice(&(f.len() as u32).to_be_bytes());
            out.extend_from_slice(f);
        }
        out
    }

    async fn collect(body: Body, max_total: usize) -> Result<Vec<Bytes>, DomainError> {
        let stream = parse_chunk_frames(body, max_total);
        futures::pin_mut!(stream);
        let mut out = Vec::new();
        while let Some(item) = stream.next().await {
            out.push(item?);
        }
        Ok(out)
    }

    #[tokio::test]
    async fn roundtrips_frames_in_order() {
        let wire = encode(&[b"first", b"second chunk", &[0xAB; 1000]]);
        let frames = collect(Body::from(wire), usize::MAX).await.unwrap();
        assert_eq!(frames.len(), 3);
        assert_eq!(&frames[0][..], b"first");
        assert_eq!(&frames[1][..], b"second chunk");
        assert_eq!(frames[2].len(), 1000);
    }

    #[tokio::test]
    async fn empty_body_yields_no_frames() {
        let frames = collect(Body::empty(), usize::MAX).await.unwrap();
        assert!(frames.is_empty());
    }

    #[tokio::test]
    async fn rejects_zero_length_frame() {
        let wire = encode(&[b""]);
        assert!(collect(Body::from(wire), usize::MAX).await.is_err());
    }

    #[tokio::test]
    async fn rejects_frame_above_cdc_max() {
        let mut wire = Vec::new();
        wire.extend_from_slice(&((CDC_MAX_CHUNK as u32) + 1).to_be_bytes());
        // Header alone is enough — the length is rejected before any data.
        assert!(collect(Body::from(wire), usize::MAX).await.is_err());
    }

    #[tokio::test]
    async fn rejects_truncated_frame() {
        let mut wire = encode(&[b"complete"]);
        wire.extend_from_slice(&10u32.to_be_bytes());
        wire.extend_from_slice(b"only5"); // promises 10, delivers 5
        assert!(collect(Body::from(wire), usize::MAX).await.is_err());
    }

    #[tokio::test]
    async fn rejects_request_above_total_cap() {
        let wire = encode(&[&[0u8; 600], &[1u8; 600]]);
        assert!(collect(Body::from(wire), 1000).await.is_err());
    }

    #[tokio::test]
    async fn accepts_frame_exactly_at_cdc_max() {
        let big = vec![7u8; CDC_MAX_CHUNK];
        let wire = encode(&[&big]);
        let frames = collect(Body::from(wire), usize::MAX).await.unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].len(), CDC_MAX_CHUNK);
    }
}
