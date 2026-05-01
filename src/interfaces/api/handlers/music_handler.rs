use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::Deserialize;

use crate::application::dtos::playlist_dto::{
    AddTracksDto, CreatePlaylistDto, PlaylistQueryDto, ReorderTracksDto, SharePlaylistDto,
    UpdatePlaylistDto,
};
use crate::application::ports::music_ports::MusicUseCase;
use crate::application::services::music_service::MusicService;
use crate::interfaces::errors::AppError;
use crate::interfaces::middleware::auth::AuthUser;

#[derive(Debug, Deserialize)]
pub struct PaginationQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[utoipa::path(
    post,
    path = "/api/playlists",
    responses(
        (status = 201, description = "Playlist created"),
        (status = 400, description = "Bad request"),
        (status = 401, description = "Unauthorized")
    ),
    tag = "playlists"
)]
pub async fn create_playlist(
    State(music_service): State<Arc<MusicService>>,
    auth_user: AuthUser,
    Json(dto): Json<CreatePlaylistDto>,
) -> impl IntoResponse {
    match music_service.create_playlist(dto, auth_user.id).await {
        Ok(playlist) => (StatusCode::CREATED, Json(playlist)).into_response(),
        Err(err) => AppError::from(err).into_response(),
    }
}

#[utoipa::path(
    get,
    path = "/api/playlists/{playlist_id}",
    params(("playlist_id" = String, Path, description = "Playlist ID")),
    responses(
        (status = 200, description = "Playlist details"),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Playlist not found")
    ),
    tag = "playlists"
)]
pub async fn get_playlist(
    State(music_service): State<Arc<MusicService>>,
    auth_user: AuthUser,
    Path(playlist_id): Path<String>,
) -> impl IntoResponse {
    match music_service.get_playlist(&playlist_id, auth_user.id).await {
        Ok(playlist) => (StatusCode::OK, Json(playlist)).into_response(),
        Err(err) => AppError::from(err).into_response(),
    }
}

#[utoipa::path(
    get,
    path = "/api/playlists",
    responses(
        (status = 200, description = "List of playlists"),
        (status = 401, description = "Unauthorized")
    ),
    tag = "playlists"
)]
pub async fn list_playlists(
    State(music_service): State<Arc<MusicService>>,
    auth_user: AuthUser,
    Query(query): Query<PlaylistQueryDto>,
) -> impl IntoResponse {
    match music_service.list_playlists(query, auth_user.id).await {
        Ok(playlists) => (StatusCode::OK, Json(playlists)).into_response(),
        Err(err) => AppError::from(err).into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct IncludeSharedQuery {
    pub include_shared: Option<bool>,
    pub include_public: Option<bool>,
}

#[utoipa::path(
    put,
    path = "/api/playlists/{playlist_id}",
    params(("playlist_id" = String, Path, description = "Playlist ID")),
    responses(
        (status = 200, description = "Playlist updated"),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Playlist not found")
    ),
    tag = "playlists"
)]
pub async fn update_playlist(
    State(music_service): State<Arc<MusicService>>,
    auth_user: AuthUser,
    Path(playlist_id): Path<String>,
    Json(dto): Json<UpdatePlaylistDto>,
) -> impl IntoResponse {
    match music_service
        .update_playlist(&playlist_id, dto, auth_user.id)
        .await
    {
        Ok(playlist) => (StatusCode::OK, Json(playlist)).into_response(),
        Err(err) => AppError::from(err).into_response(),
    }
}

#[utoipa::path(
    delete,
    path = "/api/playlists/{playlist_id}",
    params(("playlist_id" = String, Path, description = "Playlist ID")),
    responses(
        (status = 204, description = "Playlist deleted"),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Playlist not found")
    ),
    tag = "playlists"
)]
pub async fn delete_playlist(
    State(music_service): State<Arc<MusicService>>,
    auth_user: AuthUser,
    Path(playlist_id): Path<String>,
) -> impl IntoResponse {
    match music_service
        .delete_playlist(&playlist_id, auth_user.id)
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => AppError::from(err).into_response(),
    }
}

#[utoipa::path(
    post,
    path = "/api/playlists/{playlist_id}/tracks",
    params(("playlist_id" = String, Path, description = "Playlist ID")),
    responses(
        (status = 201, description = "Tracks added"),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Playlist not found")
    ),
    tag = "playlists"
)]
pub async fn add_tracks(
    State(music_service): State<Arc<MusicService>>,
    auth_user: AuthUser,
    Path(playlist_id): Path<String>,
    Json(dto): Json<AddTracksDto>,
) -> impl IntoResponse {
    match music_service
        .add_tracks(&playlist_id, dto, auth_user.id)
        .await
    {
        Ok(tracks) => (StatusCode::CREATED, Json(tracks)).into_response(),
        Err(err) => AppError::from(err).into_response(),
    }
}

#[utoipa::path(
    delete,
    path = "/api/playlists/{playlist_id}/tracks/{file_id}",
    params(
        ("playlist_id" = String, Path, description = "Playlist ID"),
        ("file_id" = String, Path, description = "File ID to remove")
    ),
    responses(
        (status = 204, description = "Track removed"),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Playlist or track not found")
    ),
    tag = "playlists"
)]
pub async fn remove_track(
    State(music_service): State<Arc<MusicService>>,
    auth_user: AuthUser,
    Path((playlist_id, file_id)): Path<(String, String)>,
) -> impl IntoResponse {
    match music_service
        .remove_track(&playlist_id, &file_id, auth_user.id)
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => AppError::from(err).into_response(),
    }
}

#[utoipa::path(
    put,
    path = "/api/playlists/{playlist_id}/reorder",
    params(("playlist_id" = String, Path, description = "Playlist ID")),
    responses(
        (status = 204, description = "Tracks reordered"),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Playlist not found")
    ),
    tag = "playlists"
)]
pub async fn reorder_tracks(
    State(music_service): State<Arc<MusicService>>,
    auth_user: AuthUser,
    Path(playlist_id): Path<String>,
    Json(dto): Json<ReorderTracksDto>,
) -> impl IntoResponse {
    match music_service
        .reorder_tracks(&playlist_id, dto, auth_user.id)
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => AppError::from(err).into_response(),
    }
}

#[utoipa::path(
    get,
    path = "/api/playlists/{playlist_id}/tracks",
    params(("playlist_id" = String, Path, description = "Playlist ID")),
    responses(
        (status = 200, description = "List of playlist tracks"),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Playlist not found")
    ),
    tag = "playlists"
)]
pub async fn list_playlist_tracks(
    State(music_service): State<Arc<MusicService>>,
    auth_user: AuthUser,
    Path(playlist_id): Path<String>,
) -> impl IntoResponse {
    match music_service
        .list_playlist_tracks(&playlist_id, auth_user.id)
        .await
    {
        Ok(tracks) => (StatusCode::OK, Json(tracks)).into_response(),
        Err(err) => AppError::from(err).into_response(),
    }
}

#[utoipa::path(
    post,
    path = "/api/playlists/{playlist_id}/share",
    params(("playlist_id" = String, Path, description = "Playlist ID")),
    responses(
        (status = 204, description = "Playlist shared"),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Playlist not found")
    ),
    tag = "playlists"
)]
pub async fn share_playlist(
    State(music_service): State<Arc<MusicService>>,
    auth_user: AuthUser,
    Path(playlist_id): Path<String>,
    Json(dto): Json<SharePlaylistDto>,
) -> impl IntoResponse {
    match music_service
        .share_playlist(&playlist_id, dto, auth_user.id)
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => AppError::from(err).into_response(),
    }
}

#[utoipa::path(
    delete,
    path = "/api/playlists/{playlist_id}/share/{user_id}",
    params(
        ("playlist_id" = String, Path, description = "Playlist ID"),
        ("user_id" = String, Path, description = "User ID to remove share")
    ),
    responses(
        (status = 204, description = "Share removed"),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Playlist or share not found")
    ),
    tag = "playlists"
)]
pub async fn remove_share(
    State(music_service): State<Arc<MusicService>>,
    auth_user: AuthUser,
    Path((playlist_id, user_id)): Path<(String, String)>,
) -> impl IntoResponse {
    match music_service
        .remove_share(&playlist_id, &user_id, auth_user.id)
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => AppError::from(err).into_response(),
    }
}

#[utoipa::path(
    get,
    path = "/api/playlists/{playlist_id}/shares",
    params(("playlist_id" = String, Path, description = "Playlist ID")),
    responses(
        (status = 200, description = "List of playlist shares"),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Playlist not found")
    ),
    tag = "playlists"
)]
pub async fn get_playlist_shares(
    State(music_service): State<Arc<MusicService>>,
    auth_user: AuthUser,
    Path(playlist_id): Path<String>,
) -> impl IntoResponse {
    match music_service
        .get_playlist_shares(&playlist_id, auth_user.id)
        .await
    {
        Ok(shares) => (StatusCode::OK, Json(shares)).into_response(),
        Err(err) => AppError::from(err).into_response(),
    }
}

#[utoipa::path(
    get,
    path = "/api/playlists/audio-metadata/{file_id}",
    params(("file_id" = String, Path, description = "Audio file ID")),
    responses(
        (status = 200, description = "Audio metadata"),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "File not found")
    ),
    tag = "playlists"
)]
pub async fn get_audio_metadata(
    State(music_service): State<Arc<MusicService>>,
    auth_user: AuthUser,
    Path(file_id): Path<String>,
) -> impl IntoResponse {
    match music_service
        .get_audio_metadata(&file_id, auth_user.id)
        .await
    {
        Ok(metadata) => (StatusCode::OK, Json(metadata)).into_response(),
        Err(err) => AppError::from(err).into_response(),
    }
}
