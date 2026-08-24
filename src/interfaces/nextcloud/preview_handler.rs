//! Nextcloud-compatible preview/thumbnail endpoint.
//!
//! Maps Nextcloud preview requests to OxiCloud's thumbnail service.

use axum::{
    body::Body,
    extract::{Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use std::sync::Arc;

use crate::application::ports::authorization_ports::AuthorizationEngine;
use crate::application::ports::file_ports::FileRetrievalUseCase;
use crate::application::ports::storage_ports::FileReadPort;
use crate::application::ports::thumbnail_ports::{ThumbnailFormat, ThumbnailPort, ThumbnailSize};
use crate::common::di::AppState;
use crate::domain::services::authorization::{Permission, Resource, Subject};
use crate::interfaces::middleware::auth::AuthUser;
use uuid::Uuid;

#[derive(Debug, Deserialize)]
pub struct PreviewParams {
    #[serde(rename = "fileId")]
    file_id: String,
    x: Option<u32>,
    y: Option<u32>,
    #[serde(rename = "forceIcon")]
    force_icon: Option<u8>,
}

/// Handle Nextcloud preview requests.
///
/// Maps:
/// - `/index.php/core/preview?fileId=X` to thumbnail generation
/// - Size selection based on request dimensions and forceIcon param
pub async fn handle_preview(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Query(params): Query<PreviewParams>,
    req: axum::extract::Request,
) -> impl IntoResponse {
    // Parse the Nextcloud file ID — the NC app may append an instance suffix
    // (e.g. "00000326ocnca"), so strip non-digit characters first.
    let digit_end = params
        .file_id
        .as_bytes()
        .iter()
        .position(|b| !b.is_ascii_digit())
        .unwrap_or(params.file_id.len());
    let nc_file_id: i64 = match params.file_id[..digit_end].parse() {
        Ok(id) => id,
        Err(_) => {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from("Invalid file ID"))
                .unwrap();
        }
    };

    // Look up the OxiCloud file UUID from the Nextcloud ID
    let object_id = match state.nextcloud.as_ref() {
        Some(nc) => match nc.file_ids.get_oxicloud_id(nc_file_id).await {
            Ok(id) => id,
            Err(_) => {
                return Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .body(Body::from("File not found"))
                    .unwrap();
            }
        },
        None => {
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("Nextcloud integration not configured"))
                .unwrap();
        }
    };

    // Get file details
    let file = match state
        .applications
        .file_retrieval_service
        .get_file(&object_id)
        .await
    {
        Ok(file) => file,
        Err(_) => {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::from("File not found"))
                .unwrap();
        }
    };

    // Verify the authenticated user can Read this file. Anti-enum: any
    // AuthZ denial surfaces as 404 (same shape as "unknown file" above),
    // and the engine emits an `authz.denied` audit line internally.
    let file_uuid = match Uuid::parse_str(&file.id) {
        Ok(u) => u,
        Err(_) => {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::from("File not found"))
                .unwrap();
        }
    };
    if state
        .authorization
        .require(
            Subject::User(user.id),
            Permission::Read,
            Resource::File(file_uuid),
        )
        .await
        .is_err()
    {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("File not found"))
            .unwrap();
    }

    // Determine thumbnail size based on request params
    let thumb_size = if params.force_icon == Some(1) {
        ThumbnailSize::Icon
    } else {
        // Map requested dimensions to our thumbnail sizes
        let max_dim = params.x.unwrap_or(400).max(params.y.unwrap_or(400));
        if max_dim <= 150 {
            ThumbnailSize::Icon
        } else if max_dim <= 400 {
            ThumbnailSize::Preview
        } else {
            ThumbnailSize::Large
        }
    };

    // Conditional revalidation. NC clients revalidate gallery previews
    // constantly; this endpoint set an immutable ETag but never compared it,
    // so every revalidation re-ran the whole pipeline and re-shipped the body
    // (ROUND10). Authz already passed above; a 304 must never skip the Read
    // check.
    //
    // Keyed on the CONTENT hash, matching the REST thumbnail endpoint. A
    // thumbnail is a pure function of (source bytes, size), so that pair
    // identifies the response and `immutable` below is honest. Keying on the
    // object id meant replacing a file's content — which preserves the id —
    // left every client showing the old preview for up to a year, since
    // `immutable` suppresses revalidation entirely.
    //
    // This moves the blob-hash query ahead of the 304 rather than adding one:
    // the same lookup used to sit just below, on the path that renders.
    let blob_hash = match state
        .repositories
        .file_read_repository
        .get_blob_hash(&object_id)
        .await
    {
        Ok(hash) => hash,
        Err(_) => {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::from("File blob not found"))
                .unwrap();
        }
    };
    let etag = {
        let s = thumb_size.as_str();
        let mut e = String::with_capacity(9 + blob_hash.len() + s.len());
        e.push_str("\"thumb-");
        e.push_str(&blob_hash);
        e.push('-');
        e.push_str(s);
        e.push('"');
        e
    };
    if let Some(inm) = req.headers().get(header::IF_NONE_MATCH)
        && let Ok(client_etag) = inm.to_str()
        && (client_etag == etag || client_etag == "*")
    {
        return Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(header::CACHE_CONTROL, "public, max-age=31536000, immutable")
            .header(header::ETAG, etag)
            .body(Body::empty())
            .unwrap();
    }

    // Check if file is an image
    if !state
        .core
        .thumbnail_service
        .is_supported_image(&file.mime_type)
    {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("Preview not available for this file type"))
            .unwrap();
    }

    // `blob_hash` was resolved above to build the ETag.
    if let Some(data) = state
        .core
        .thumbnail_service
        // NextCloud clients don't advertise WebP and expect JPEG — pin to JPEG
        // (served from the shared lazy `.jpg` fallback).
        .get_cached_thumbnail(
            &object_id,
            Some(&blob_hash),
            thumb_size.into(),
            ThumbnailFormat::Jpeg,
            Some(&state.core.dedup_service),
        )
        .await
    {
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "image/jpeg")
            .header(header::CONTENT_LENGTH, data.len())
            .header(header::CACHE_CONTROL, "public, max-age=31536000, immutable")
            .header(header::ETAG, etag)
            .body(Body::from(data))
            .unwrap();
    }

    // Generate/get thumbnail — the blob is read inside the service once a
    // decode permit is held, so preview stampedes cannot stack source
    // images in RAM.
    match state
        .core
        .thumbnail_service
        .get_thumbnail_from_blob(
            &object_id,
            &blob_hash,
            thumb_size.into(),
            ThumbnailFormat::Jpeg,
            state.core.dedup_service.clone(),
        )
        .await
    {
        Ok(data) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "image/jpeg")
            .header(header::CONTENT_LENGTH, data.len())
            .header(header::CACHE_CONTROL, "public, max-age=31536000, immutable")
            .header(header::ETAG, etag)
            .body(Body::from(data))
            .unwrap(),
        Err(err) => {
            tracing::error!("Thumbnail generation failed for {}: {}", object_id, err);
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("Failed to generate thumbnail"))
                .unwrap()
        }
    }
}
