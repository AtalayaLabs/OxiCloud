//! Pre-sized JSON responses for listing endpoints.
//!
//! `axum::Json` serializes into a `BytesMut::with_capacity(128)` — a 500-row
//! listing grows that seed through ~11 doubling reallocations, memcpy-ing
//! ~1.3× the payload on every hot listing response (files, folder
//! resources, photos timeline, search). `sized_json` serializes into one
//! right-sized `Vec` instead: 2 allocations total and no copy chain
//! (benches/ROUND12.md §M1, 1.40x / −11 allocs on a 500-row page).
//!
//! The per-row estimates are calibrated against the serialized DTOs (a
//! realistic `FileDto` row measures ~380 B). Underestimates cost one extra
//! doubling — still far better than the 128-byte seed; overestimates waste
//! transient capacity only (the buffer is freed after the response).

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use serde::Serialize;

/// Serialized size estimate for one file/folder row (FileDto ≈ 380 B).
pub const EST_ROW_BYTES: usize = 384;

/// Serialized size estimate for one wrapped resource row
/// (FolderResourcesDto / PhotoResourceItemDto items carry a FileDto
/// plus wrapper fields).
pub const EST_WRAPPED_ROW_BYTES: usize = 448;

/// Serialize `value` into a single pre-sized buffer and wrap it as an
/// `application/json` response — drop-in for `Json(value).into_response()`
/// (byte-identical body, gated in `bench_round12_micro` §1), minus the
/// doubling-realloc chain.
pub fn sized_json<T: Serialize>(estimated_bytes: usize, value: &T) -> Response {
    let mut buf = Vec::with_capacity(estimated_bytes.max(128));
    match serde_json::to_writer(&mut buf, value) {
        Ok(()) => (
            StatusCode::OK,
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            )],
            Bytes::from(buf),
        )
            .into_response(),
        // Mirror axum's Json error arm: 500 + plain-text serializer error.
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            )],
            err.to_string(),
        )
            .into_response(),
    }
}
