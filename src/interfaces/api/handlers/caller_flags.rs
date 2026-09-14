//! Shared enrichment helpers that populate the `is_favorite` and
//! `is_shared` wire-contract flags on `FileDto` / `FolderDto` before
//! Json emission.
//!
//! The two functions live here so single-item handlers across
//! `folder_handler`, `file_handler`, `delta_upload_handler`,
//! `photos_handler`, etc. all go through the same path — one place
//! to change if the enrichment strategy ever moves (e.g. batch
//! lookups, background prefetch).
//!
//! Every handler that returns a `FileDto` or `FolderDto` to the SPA
//! MUST call one of these helpers. Handlers that emit only to
//! WebDAV / NextCloud DAV surfaces (which drop these fields via the
//! XML property serializer) can skip enrichment — the default `false`
//! is never observable on those wires.

use std::sync::Arc;

use uuid::Uuid;

use crate::application::dtos::file_dto::FileDto;
use crate::application::dtos::folder_dto::FolderDto;
use crate::common::di::AppState as GlobalAppState;
use crate::domain::services::authorization::Subject;

/// Both flags are caller-relative, and a public-share visitor is not a
/// caller either of them can describe.
///
/// `is_favorite` is per-user state a visitor has none of. `is_shared` is
/// worse: it is a **subject-less** EXISTS over `storage.shares`, so it would
/// answer "is this separately shared with anyone at all" — telling a visitor
/// which items inside the share the owner has also published elsewhere. That
/// is the disclosure `docs/plan/rationalize-publicshare.md` §Phase 2 names.
///
/// Taking a `Subject` rather than a `Uuid` is what makes this unforgettable:
/// a token caller has no `user_id`, so every present and future callsite gets
/// the `false` default without having to remember the rule. Passing
/// `Subject::User(id)` at a callsite that genuinely has a user is a no-op.
fn caller_user_for_flags(caller: Subject) -> Option<Uuid> {
    caller.user_id()
}

/// Populate the `is_favorite` + `is_shared` flags on a `FolderDto`.
///
/// Silently leaves the flags at their default `false` when the
/// favorites service isn't wired (feature-off), when the resource
/// id doesn't parse as a UUID, or when the caller is not a user
/// (see [`caller_user_for_flags`]) — the DTO stays valid on the wire
/// and the misleading-`false` window closes as soon as the next
/// listing refetch runs.
pub async fn enrich_folder_flags(
    state: &Arc<GlobalAppState>,
    dto: &mut FolderDto,
    caller: Subject,
) {
    let Some(caller_id) = caller_user_for_flags(caller) else {
        return;
    };
    let Some(favs) = state.favorites_service.as_ref() else {
        return;
    };
    let Ok(resource_id) = Uuid::parse_str(&dto.id) else {
        return;
    };
    if let Ok((fav, shr)) = favs.caller_flags(caller_id, "folder", resource_id).await {
        dto.is_favorite = fav;
        dto.is_shared = shr;
    }
}

/// File counterpart of [`enrich_folder_flags`] — see that doc.
pub async fn enrich_file_flags(state: &Arc<GlobalAppState>, dto: &mut FileDto, caller: Subject) {
    let Some(caller_id) = caller_user_for_flags(caller) else {
        return;
    };
    let Some(favs) = state.favorites_service.as_ref() else {
        return;
    };
    let Ok(resource_id) = Uuid::parse_str(&dto.id) else {
        return;
    };
    if let Ok((fav, shr)) = favs.caller_flags(caller_id, "file", resource_id).await {
        dto.is_favorite = fav;
        dto.is_shared = shr;
    }
}

/// Batch variant: enrich every `FileDto` in a slice with per-item
/// `caller_flags`. Runs the lookups sequentially — for the bulk
/// endpoints (`get_files_by_ids`, `photos_handler`) this is one
/// round trip per item; if that becomes hot on a large fetch, the
/// callsite can be replaced with a single SQL query returning the
/// pairs. Kept simple for now; the DTO is `&mut`, no clones.
pub async fn enrich_file_flags_batch(
    state: &Arc<GlobalAppState>,
    dtos: &mut [FileDto],
    caller: Subject,
) {
    for dto in dtos.iter_mut() {
        enrich_file_flags(state, dto, caller).await;
    }
}

/// Batch variant for folders — mirror of [`enrich_file_flags_batch`].
pub async fn enrich_folder_flags_batch(
    state: &Arc<GlobalAppState>,
    dtos: &mut [FolderDto],
    caller: Subject,
) {
    for dto in dtos.iter_mut() {
        enrich_folder_flags(state, dto, caller).await;
    }
}
