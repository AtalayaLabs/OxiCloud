pub mod cookie_auth;
pub mod deserializer;
pub mod handlers;
pub mod routes;

pub use routes::create_api_routes;
pub use routes::create_public_api_routes;

use utoipa::OpenApi;

use crate::application::dtos::favorites_dto::{
    BatchFavoritesResult, BatchFavoritesStats, FavoriteItemDto,
};
use crate::application::dtos::file_dto::FileDto;
use crate::application::dtos::folder_dto::{
    CreateFolderDto, FolderDto, MoveFolderDto, RenameFolderDto,
};
use crate::application::dtos::folder_listing_dto::FolderListingDto;
use crate::application::dtos::pagination::{PaginationDto, PaginationRequestDto};
use crate::application::dtos::recent_dto::RecentItemDto;
use crate::application::dtos::search_dto::{
    SearchCriteriaDto, SearchFileResultDto, SearchFolderResultDto, SearchResultsDto,
    SearchSuggestionItem, SearchSuggestionsDto,
};
use crate::application::dtos::share_dto::{
    CreateShareDto, ShareDto, SharePermissionsDto, UpdateShareDto,
};
use crate::application::dtos::trash_dto::{
    DeletePermanentlyRequest, MoveToTrashRequest, RestoreFromTrashRequest, TrashedItemDto,
};
use crate::application::dtos::user_dto::{
    AuthResponseDto, ChangePasswordDto, LoginDto, RefreshTokenDto, RegisterDto, SetupAdminDto,
    UserDto,
};
use crate::interfaces::api::handlers::file_handler::MoveFilePayload;

#[derive(OpenApi)]
#[openapi(
    paths(
        handlers::trash_handler::get_trash_items,
        handlers::trash_handler::move_file_to_trash,
        handlers::trash_handler::move_folder_to_trash,
        handlers::trash_handler::restore_from_trash,
        handlers::trash_handler::delete_permanently,
        handlers::trash_handler::empty_trash,
        handlers::share_handler::create_shared_link,
        handlers::share_handler::get_shared_link,
        handlers::share_handler::get_user_shares,
        handlers::share_handler::update_shared_link,
        handlers::share_handler::delete_shared_link,
        handlers::share_handler::access_shared_item,
        handlers::share_handler::verify_shared_item_password,
        handlers::favorites_handler::get_favorites,
        handlers::favorites_handler::add_favorite,
        handlers::favorites_handler::remove_favorite,
        handlers::favorites_handler::batch_add_favorites,
        handlers::recent_handler::get_recent_items,
        handlers::recent_handler::record_item_access,
        handlers::recent_handler::remove_from_recent,
        handlers::recent_handler::clear_recent_items,
    ),
    components(
        schemas(
            // Folder schemas
            FolderDto,
            CreateFolderDto,
            RenameFolderDto,
            MoveFolderDto,
            FolderListingDto,
            // File schemas
            FileDto,
            MoveFilePayload,
            PaginationDto,
            PaginationRequestDto,
            // User / Auth schemas
            UserDto,
            LoginDto,
            RegisterDto,
            SetupAdminDto,
            AuthResponseDto,
            ChangePasswordDto,
            RefreshTokenDto,
            // Share schemas
            ShareDto,
            SharePermissionsDto,
            CreateShareDto,
            UpdateShareDto,
            // Trash schemas
            TrashedItemDto,
            MoveToTrashRequest,
            RestoreFromTrashRequest,
            DeletePermanentlyRequest,
            // Search schemas
            SearchCriteriaDto,
            SearchResultsDto,
            SearchFileResultDto,
            SearchFolderResultDto,
            SearchSuggestionsDto,
            SearchSuggestionItem,
            // Favorites schemas
            FavoriteItemDto,
            BatchFavoritesResult,
            BatchFavoritesStats,
            // Recent schemas
            RecentItemDto,
        )
    ),
    tags(
        (name = "folders", description = "Folder management endpoints"),
        (name = "files", description = "File management endpoints"),
        (name = "trash", description = "Trash / recycle bin endpoints"),
        (name = "search", description = "Search endpoints"),
        (name = "shares", description = "Shared links endpoints"),
        (name = "favorites", description = "Favorites management endpoints"),
        (name = "recent", description = "Recent items endpoints"),
    ),
    info(
        title = "OxiCloud API",
        version = env!("CARGO_PKG_VERSION"),
        description = "REST API for OxiCloud — self-hosted cloud storage, calendar & contacts",
        license(name = "MIT")
    )
)]
pub struct ApiDoc;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openapi_spec_is_valid_and_has_expected_structure() {
        let spec = ApiDoc::openapi();

        assert_eq!(spec.info.title, "OxiCloud API");
        assert!(!spec.info.version.is_empty());

        let paths = &spec.paths;
        assert!(
            paths.paths.len() >= 10,
            "expected at least 10 paths, got {}",
            paths.paths.len()
        );
        assert!(paths.paths.contains_key("/api/trash"), "missing /api/trash");
        assert!(
            paths.paths.contains_key("/api/shares"),
            "missing /api/shares"
        );
        assert!(
            paths.paths.contains_key("/api/favorites"),
            "missing /api/favorites"
        );
        assert!(
            paths.paths.contains_key("/api/recent"),
            "missing /api/recent"
        );

        let schemas = &spec
            .components
            .as_ref()
            .expect("components missing")
            .schemas;
        assert!(
            schemas.len() >= 25,
            "expected at least 25 schemas, got {}",
            schemas.len()
        );
        for name in [
            "FileDto",
            "FolderDto",
            "ShareDto",
            "TrashedItemDto",
            "UserDto",
        ] {
            assert!(schemas.contains_key(name), "missing schema: {name}");
        }

        let json = serde_json::to_string(&spec).expect("spec should serialise to JSON");
        assert!(json.len() > 1000, "spec JSON suspiciously small");
    }
}
