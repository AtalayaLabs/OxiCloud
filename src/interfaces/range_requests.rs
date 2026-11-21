//! Conditional-request (`If-None-Match` → 304) and `Range` (→ 206/416)
//! helpers shared by the native WebDAV and NextCloud GET handlers.
//!
//! Mount-style WebDAV clients (rclone, davfs2, Finder, video players)
//! read by ranges; without 206 support every seek or resume transfers
//! the whole file. The REST download handler keeps its own richer
//! variant in `file_handler.rs` (Content-Disposition, compression,
//! permission-scoped streams) but follows the same header semantics.

use axum::body::Body;
use axum::http::{HeaderMap, Response, StatusCode, header};
use http_range_header::parse_range_header;
use std::sync::Arc;

use crate::application::dtos::file_dto::FileDto;
use crate::application::ports::file_ports::RangeContent;
use crate::application::services::file_retrieval_service::FileRetrievalService;

/// `If-None-Match` short-circuit: returns a `304 Not Modified` response
/// when the client already holds the current representation. `etag` must
/// be the quoted form the GET would emit (same comparison as the REST
/// download endpoint: exact match or `*`).
pub fn not_modified_response(headers: &HeaderMap, etag: &str) -> Option<Response<Body>> {
    let client_etag = headers.get(header::IF_NONE_MATCH)?.to_str().ok()?;
    if client_etag == etag || client_etag == "*" {
        return Some(
            Response::builder()
                .status(StatusCode::NOT_MODIFIED)
                .header(header::ETAG, etag)
                .body(Body::empty())
                .unwrap(),
        );
    }
    None
}

/// `Range` short-circuit for a streaming download.
///
/// Returns `Some(206)` with the requested byte range, `Some(416)` when
/// the range cannot be satisfied, or `None` when no (parseable) range
/// was requested or the range stream could not be created — callers
/// fall through to the full-body response, mirroring the REST handler.
///
/// The caller has already resolved access to the file (path-resolver /
/// ownership), so this uses the unscoped range stream — same contract
/// as the `get_file_stream` call in the surrounding handlers.
pub async fn range_response(
    headers: &HeaderMap,
    file: &FileDto,
    etag: &str,
    retrieval: &Arc<FileRetrievalService>,
) -> Option<Response<Body>> {
    let range_str = headers.get(header::RANGE)?.to_str().ok()?;
    let ranges = parse_range_header(range_str).ok()?;

    let valid_ranges = match ranges.validate(file.size) {
        Ok(v) => v,
        Err(_) => {
            return Some(
                Response::builder()
                    .status(StatusCode::RANGE_NOT_SATISFIABLE)
                    .header(header::CONTENT_RANGE, format!("bytes */{}", file.size))
                    .body(Body::empty())
                    .unwrap(),
            );
        }
    };

    let range = valid_ranges.first()?;
    let start = *range.start();
    let end = *range.end();
    let range_length = end - start + 1;

    // Cache-aware: sub-threshold files already in the RAM content cache are
    // answered with a zero-copy Bytes slice — no PG, no disk (benches/RANGE-CACHE.md).
    match retrieval
        .get_file_range_preloaded(file, start, Some(end + 1))
        .await
    {
        Ok(content) => {
            let body = match content {
                RangeContent::Bytes(b) => Body::from(b),
                RangeContent::Stream(s) => Body::from_stream(Box::into_pin(s)),
            };
            let builder = crate::interfaces::user_content::apply_dav_user_content_headers(
                Response::builder()
                    .status(StatusCode::PARTIAL_CONTENT)
                    .header(header::CONTENT_TYPE, &*file.mime_type)
                    .header(header::CONTENT_LENGTH, range_length)
                    .header(
                        header::CONTENT_RANGE,
                        format!("bytes {}-{}/{}", start, end, file.size),
                    )
                    .header(header::ACCEPT_RANGES, "bytes")
                    .header(header::ETAG, etag),
                &file.mime_type,
            );
            Some(builder.body(body).unwrap())
        }
        Err(err) => {
            tracing::error!("Error creating range stream: {}", err);
            None // fall through to the full download
        }
    }
}
