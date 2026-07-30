pub mod cookie_auth;
pub mod deserializer;
pub mod handlers;
pub mod routes;
pub mod sized_json;

pub use routes::create_api_routes;
pub use routes::create_health_routes;
pub use routes::create_public_api_routes;

use utoipa::openapi::security::{Http, HttpAuthScheme, SecurityScheme};
use utoipa::{Modify, OpenApi};

use crate::application::dtos::contact_dto::{
    AddressDto, ContactDto, ContactGroupDto, EmailDto, PhoneDto,
};
use crate::application::dtos::drive_dto::{DriveDto, DriveKindDto};
use crate::application::dtos::favorites_dto::{
    BatchFavoritesResult, BatchFavoritesStats, FavoriteItemDto, FavoritesResourceItemDto,
};
use crate::application::dtos::file_dto::FileDto;
use crate::application::dtos::folder_dto::{
    AccessSourceDriveDto, AccessSourceDto, AccessSourceKind, AccessSourceSubjectDto,
    AccessSourceSubjectKind, CreateFolderDto, FolderAncestorDto, FolderAncestorsDto, FolderDto,
    FolderResourceItemDto, MoveFolderDto, RenameFolderDto,
};
use crate::application::dtos::folder_listing_dto::FolderListingDto;
use crate::application::dtos::grant_dto::{
    CreateGrantDto, GrantDto, OutgoingResourceItemDto, PermissionDto, ResourceContentDto,
    ResourceDto, ResourceTypeDto, RoleDto, SharedWithMeDto, SharedWithMeItemDto, SubjectDto,
    SubjectTypeDto, UpdateRoleDto,
};
use crate::application::dtos::i18n_dto::{
    LocaleDto, TranslationErrorDto, TranslationRequestDto, TranslationResponseDto,
};
use crate::application::dtos::pagination::{PaginationDto, PaginationRequestDto};
use crate::application::dtos::recent_dto::{RecentItemDto, RecentResourceItemDto};
use crate::application::dtos::search_dto::{
    SearchCriteriaDto, SearchFileResultDto, SearchFolderResultDto, SearchMeta, SearchResourceItem,
    SearchResourcesDto, SearchResultsDto, SearchSuggestionItem, SearchSuggestionsDto,
};
use crate::application::dtos::share_dto::{CreateShareDto, ShareDto, UpdateShareDto};
use crate::application::dtos::trash_dto::{
    DeletePermanentlyRequest, MoveToTrashRequest, RestoreFromTrashRequest, TrashResourceItemDto,
    TrashResourcesDto, TrashedItemDto,
};
use crate::application::dtos::user_dto::{
    AuthResponseDto, ChangePasswordDto, LoginDto, OidcExchangeDto, OidcProviderInfoDto,
    RefreshTokenDto, RegisterDto, SetupAdminDto, UserDto,
};
use crate::application::ports::chunked_upload_ports::{
    ChunkUploadResponseDto, CreateUploadResponseDto, UploadStatusResponseDto,
};
use crate::interfaces::api::handlers::auth_handler::SystemStatus;
use crate::interfaces::api::handlers::chunked_upload_handler::{
    CompleteUploadResponse, CreateUploadRequest,
};
use crate::interfaces::api::handlers::contacts_handler::{
    AddMemberRequest, AddressBookResponse, CreateAddressBookRequest, CreateContactRequest,
    GroupNameRequest, UpdateAddressBookRequest, UpdateContactRequest,
};
use crate::interfaces::api::handlers::dedup_handler::{HashCheckResponse, StatsResponse};
use crate::interfaces::api::handlers::file_handler::MoveFilePayload;

#[derive(OpenApi)]
#[openapi(
    modifiers(&SecurityAddon),
    paths(
        // Auth handlers (public, protected, OIDC)
        handlers::auth_handler::register,
        handlers::auth_handler::login,
        handlers::auth_handler::refresh_token,
        handlers::auth_handler::get_current_user,
        handlers::auth_handler::change_password,
        handlers::auth_handler::logout,
        handlers::auth_handler::setup_admin,
        handlers::auth_handler::get_system_status,
        handlers::auth_handler::oidc_providers,
        handlers::auth_handler::oidc_authorize,
        handlers::auth_handler::oidc_callback,
        handlers::auth_handler::oidc_exchange,
        // File handlers (free functions — see file_handler.rs for why)
        handlers::file_handler::list_files_query,
        handlers::file_handler::upload_file_with_thumbnails,
        handlers::file_handler::create_file_by_hash,
        handlers::delta_upload_handler::delta_negotiate,
        handlers::delta_upload_handler::delta_upload_chunks,
        handlers::delta_upload_handler::delta_commit,
        handlers::delta_upload_handler::delta_file_manifest,
        handlers::delta_upload_handler::delta_download_chunks,
        handlers::file_handler::download_file,
        handlers::file_handler::get_thumbnail,
        handlers::file_handler::upload_thumbnail,
        handlers::file_handler::get_file_metadata,
        handlers::file_handler::delete_file,
        handlers::file_handler::rename_file,
        handlers::file_handler::move_file_simple,
        // Folder handlers (free functions — see folder_handler.rs for why)
        handlers::folder_handler::create_folder,
        handlers::folder_handler::get_folder,
        handlers::folder_handler::get_folder_ancestors,
        handlers::folder_handler::list_root_folders,
        handlers::folder_handler::list_folder_resources,
        handlers::folder_handler::rename_folder,
        handlers::folder_handler::move_folder,
        handlers::folder_handler::delete_folder_with_trash,
        handlers::folder_handler::download_folder_zip,
        // Search handlers (free functions — see search_handler.rs for why)
        handlers::search_handler::search_resources,
        handlers::search_handler::suggest_files,
        handlers::search_handler::clear_search_cache,
        // i18n handlers (free functions — see i18n_handler.rs for why)
        handlers::i18n_handler::get_locales,
        handlers::i18n_handler::translate,
        handlers::i18n_handler::get_translations_by_locale,
        // Chunked upload handlers — all five are free functions (not impl methods) because
        // utoipa 5.4.0 cannot annotate methods on ChunkedUploadHandler; see handler file.
        handlers::chunked_upload_handler::create_upload,
        handlers::chunked_upload_handler::upload_chunk,
        handlers::chunked_upload_handler::get_upload_status,
        handlers::chunked_upload_handler::complete_upload,
        handlers::chunked_upload_handler::cancel_upload,
        // Dedup handlers — all free functions for the same utoipa reason as chunked uploads.
        handlers::dedup_handler::check_hash,
        handlers::dedup_handler::get_stats,
        handlers::dedup_handler::get_blob,
        handlers::dedup_handler::recalculate_stats,
        // Trash handlers (free functions)
        handlers::trash_handler::get_trash_resources,
        handlers::trash_handler::move_file_to_trash,
        handlers::trash_handler::move_folder_to_trash,
        handlers::trash_handler::restore_from_trash,
        handlers::trash_handler::delete_permanently,
        handlers::trash_handler::empty_trash,
        // Share handlers (free functions)
        handlers::share_handler::create_shared_link,
        handlers::share_handler::get_shared_link,
        handlers::share_handler::get_user_shares,
        handlers::share_handler::update_shared_link,
        handlers::share_handler::delete_shared_link,
        handlers::share_handler::access_shared_item,
        handlers::share_handler::verify_shared_item_password,
        handlers::share_handler::download_shared_file,
        handlers::share_handler::list_share_contents_root,
        handlers::share_handler::list_share_contents_subfolder,
        handlers::share_handler::download_share_file_in_folder,
        handlers::share_handler::download_share_zip_root,
        handlers::share_handler::download_share_zip_subfolder,
        // Favorites handlers (free functions)
        handlers::favorites_handler::list_favorites_resources,
        handlers::favorites_handler::add_favorite,
        handlers::favorites_handler::remove_favorite,
        handlers::favorites_handler::batch_add_favorites,
        // Recent handlers (free functions)
        handlers::recent_handler::list_recent_resources,
        handlers::recent_handler::record_item_access,
        handlers::recent_handler::remove_from_recent,
        handlers::recent_handler::clear_recent_items,
        // Photos handler (free function)
        handlers::photos_handler::list_photos,
        handlers::photos_handler::list_photos_geo,
        // Drive handler (free function)
        handlers::drive_handler::list_drives,
        // Batch handlers (free functions)
        handlers::batch_handler::move_files_batch,
        handlers::batch_handler::copy_files_batch,
        handlers::batch_handler::copy_folders_batch,
        handlers::batch_handler::delete_files_batch,
        handlers::batch_handler::get_files_batch,
        handlers::batch_handler::delete_folders_batch,
        handlers::batch_handler::create_folders_batch,
        handlers::batch_handler::get_folders_batch,
        handlers::batch_handler::move_folders_batch,
        handlers::batch_handler::trash_batch,
        handlers::batch_handler::download_batch_post,
        handlers::batch_handler::download_batch_querystring,
        // Music/playlist handlers (free functions)
        handlers::music_handler::create_playlist,
        handlers::music_handler::list_playlists,
        handlers::music_handler::get_playlist,
        handlers::music_handler::update_playlist,
        handlers::music_handler::delete_playlist,
        handlers::music_handler::list_playlist_tracks,
        handlers::music_handler::add_tracks,
        handlers::music_handler::remove_track,
        handlers::music_handler::reorder_tracks,
        handlers::music_handler::share_playlist,
        handlers::music_handler::remove_share,
        handlers::music_handler::get_playlist_shares,
        handlers::music_handler::get_audio_metadata,
        // Contacts / address-book handlers (free functions)
        handlers::contacts_handler::list_address_books,
        handlers::contacts_handler::create_address_book,
        handlers::contacts_handler::update_address_book,
        handlers::contacts_handler::delete_address_book,
        handlers::contacts_handler::list_contacts,
        handlers::contacts_handler::create_contact,
        handlers::contacts_handler::get_contact,
        handlers::contacts_handler::update_contact,
        handlers::contacts_handler::delete_contact,
        handlers::contacts_handler::list_groups,
        handlers::contacts_handler::create_group,
        handlers::contacts_handler::get_group,
        handlers::contacts_handler::update_group,
        handlers::contacts_handler::delete_group,
        handlers::contacts_handler::list_contacts_in_group,
        handlers::contacts_handler::add_contact_to_group,
        handlers::contacts_handler::remove_contact_from_group,
        // Admin handlers (pub free functions)
        handlers::admin_handler::get_dashboard_stats,
        handlers::admin_handler::list_users,
        handlers::admin_handler::get_user,
        handlers::admin_handler::create_user,
        handlers::admin_handler::delete_user,
        handlers::admin_handler::update_user_role,
        handlers::admin_handler::update_user_active,
        handlers::admin_handler::update_user_quota,
        handlers::admin_handler::reset_user_password,
        handlers::admin_handler::set_registration_setting,
        handlers::admin_handler::get_oidc_settings,
        handlers::admin_handler::save_oidc_settings,
        handlers::admin_handler::get_storage_settings,
        handlers::admin_handler::save_storage_settings,
        handlers::admin_handler::get_migration_status,
        handlers::admin_handler::start_migration,
        handlers::admin_handler::pause_migration,
        handlers::admin_handler::resume_migration,
        handlers::admin_handler::complete_migration,
        handlers::admin_handler::verify_migration,
        handlers::admin_handler::generate_encryption_key,
        // JobRegistry admin surface — production, always-on,
        // audit-logged. Retired the `/internal/trigger-*` handlers in
        // favour of `/api/admin/jobs/{name}/trigger` uniform surface
        // (docs/plan/job-registry.md §Cross-cutting).
        handlers::admin_handler::list_jobs,
        handlers::admin_handler::trigger_job,
        handlers::admin_handler::cancel_job,
        handlers::admin_handler::list_job_runs,
        handlers::admin_handler::get_job_run,
        // Grant / ReBAC handlers (free functions)
        handlers::grant_handler::create_grant,
        handlers::grant_handler::revoke_grant,
        handlers::grant_handler::set_role,
        handlers::grant_handler::list_incoming,
        handlers::grant_handler::list_shared_with_me,
        handlers::grant_handler::list_outgoing,
        handlers::grant_handler::list_my_shares,
        handlers::grant_handler::list_on_resource,
        // Subject-group handlers (ReBAC named groups) — free functions
        handlers::subject_group_handler::create_group,
        handlers::subject_group_handler::list_groups,
        handlers::subject_group_handler::search_groups,
        handlers::subject_group_handler::get_group,
        handlers::subject_group_handler::update_group,
        handlers::subject_group_handler::delete_group,
        handlers::subject_group_handler::list_members,
        handlers::subject_group_handler::add_member,
        handlers::subject_group_handler::remove_user_member,
        handlers::subject_group_handler::remove_group_member,
        handlers::subject_group_handler::list_effective_members,
    ),
    components(
        schemas(
            // Folder schemas
            FolderDto,
            CreateFolderDto,
            RenameFolderDto,
            MoveFolderDto,
            FolderListingDto,
            FolderResourceItemDto,
            ResourceContentDto,
            // Folder ancestor chain (breadcrumb endpoint)
            FolderAncestorsDto,
            FolderAncestorDto,
            AccessSourceDto,
            AccessSourceKind,
            AccessSourceDriveDto,
            AccessSourceSubjectDto,
            AccessSourceSubjectKind,
            // File schemas
            FileDto,
            // Delta-upload schemas
            crate::application::services::delta_upload_service::ChunkRef,
            crate::application::services::delta_upload_service::DeltaNegotiateRequest,
            crate::application::services::delta_upload_service::DeltaNegotiateResponse,
            crate::application::services::delta_upload_service::DeltaChunksResponse,
            crate::application::services::delta_upload_service::DeltaCommitRequest,
            handlers::delta_upload_handler::DeltaStillMissingResponse,
            handlers::delta_upload_handler::DeltaNotAvailableResponse,
            crate::application::services::delta_upload_service::DeltaManifestResponse,
            crate::application::services::delta_upload_service::DeltaDownloadRequest,
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
            SystemStatus,
            OidcProviderInfoDto,
            OidcExchangeDto,
            // Share schemas
            ShareDto,
            CreateShareDto,
            UpdateShareDto,
            // Trash schemas
            TrashedItemDto,
            TrashResourceItemDto,
            TrashResourcesDto,
            MoveToTrashRequest,
            RestoreFromTrashRequest,
            DeletePermanentlyRequest,
            // Search schemas — wire envelope shares the /*/resources
            // shape (SearchResourcesDto → items[] { resource_type,
            // resource, meta }). The internal SearchCriteriaDto /
            // SearchResultsDto types are still emitted so external
            // consumers browsing the OpenAPI doc can see the service-
            // layer shape referenced by other docs.
            SearchResourcesDto,
            SearchResourceItem,
            SearchMeta,
            SearchCriteriaDto,
            SearchResultsDto,
            SearchFileResultDto,
            SearchFolderResultDto,
            SearchSuggestionsDto,
            SearchSuggestionItem,
            // Favorites schemas
            FavoriteItemDto,
            FavoritesResourceItemDto,
            BatchFavoritesResult,
            BatchFavoritesStats,
            // Recent schemas
            RecentItemDto,
            RecentResourceItemDto,
            // i18n schemas
            LocaleDto,
            TranslationRequestDto,
            TranslationResponseDto,
            TranslationErrorDto,
            // Chunked upload schemas
            CreateUploadRequest,
            CompleteUploadResponse,
            CreateUploadResponseDto,
            ChunkUploadResponseDto,
            UploadStatusResponseDto,
            // Dedup schemas
            HashCheckResponse,
            StatsResponse,
            // Contacts / address-book schemas
            AddressBookResponse,
            CreateAddressBookRequest,
            UpdateAddressBookRequest,
            ContactDto,
            ContactGroupDto,
            EmailDto,
            PhoneDto,
            AddressDto,
            CreateContactRequest,
            UpdateContactRequest,
            GroupNameRequest,
            AddMemberRequest,
            // Grant / ReBAC schemas
            SubjectTypeDto,
            SubjectDto,
            ResourceTypeDto,
            ResourceDto,
            PermissionDto,
            RoleDto,
            CreateGrantDto,
            UpdateRoleDto,
            GrantDto,
            SharedWithMeDto,
            SharedWithMeItemDto,
            OutgoingResourceItemDto,
            // Drive schemas
            DriveDto,
            DriveKindDto,
            // Subject-group (ReBAC named groups) schemas
            handlers::subject_group_handler::CreateGroupRequest,
            handlers::subject_group_handler::UpdateGroupRequest,
            handlers::subject_group_handler::AddSubjectGroupMemberRequest,
            handlers::subject_group_handler::GroupDto,
            handlers::subject_group_handler::GroupListDto,
            handlers::subject_group_handler::GroupMemberDto,
        )
    ),
    tags(
        (name = "auth", description = "Authentication and session management endpoints"),
        (name = "files", description = "File management endpoints"),
        (name = "folders", description = "Folder management endpoints"),
        (name = "trash", description = "Trash / recycle bin endpoints"),
        (name = "search", description = "Search endpoints"),
        (name = "shares", description = "Shared links endpoints"),
        (name = "favorites", description = "Favorites management endpoints"),
        (name = "recent", description = "Recent items endpoints"),
        (name = "photos", description = "Photos timeline endpoints"),
        (name = "i18n", description = "Internationalisation endpoints"),
        (name = "uploads", description = "Chunked / resumable upload endpoints"),
        (name = "dedup", description = "Content deduplication endpoints"),
        (name = "batch", description = "Batch operation endpoints"),
        (name = "playlists", description = "Music playlist endpoints"),
        (name = "contacts", description = "Address books, contacts, and groups endpoints"),
        (name = "admin", description = "Admin management endpoints"),
        (name = "grants", description = "ReBAC grant management endpoints"),
        (name = "groups", description = "ReBAC subject-group management endpoints (named, nestable, root-owned)"),
    ),
    info(
        title = "OxiCloud API",
        version = env!("CARGO_PKG_VERSION"),
        description = "REST API for OxiCloud — self-hosted cloud storage, calendar & contacts",
        license(name = "MIT")
    )
)]
pub struct ApiDoc;

/// Injects the `bearerAuth` HTTP Bearer security scheme into the generated spec.
struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            "bearerAuth",
            SecurityScheme::Http(Http::new(HttpAuthScheme::Bearer)),
        );
    }
}

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
        // The old grouped list endpoints (/api/trash, /api/favorites, /api/recent)
        // were removed in favour of the normalized cursor-paginated /resources API.
        assert!(
            paths.paths.contains_key("/api/trash/resources"),
            "missing /api/trash/resources"
        );
        assert!(
            paths.paths.contains_key("/api/shares"),
            "missing /api/shares"
        );
        assert!(
            paths.paths.contains_key("/api/favorites/resources"),
            "missing /api/favorites/resources"
        );
        assert!(
            paths.paths.contains_key("/api/recent/resources"),
            "missing /api/recent/resources"
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
