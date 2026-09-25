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
    PublicUserDto, RefreshTokenDto, RegisterDto, SetupAdminDto,
};
use crate::application::ports::chunked_upload_ports::{
    ChunkUploadResponseDto, CreateUploadResponseDto, UploadStatusResponseDto,
};
use crate::interfaces::api::handlers::auth_handler::SystemStatus;
use crate::interfaces::api::handlers::chunked_upload_handler::{
    CompleteUploadResponse, CreateUploadRequest,
};
use crate::interfaces::api::handlers::config_handler::{FeaturesDto, ServerConfigDto};
use crate::interfaces::api::handlers::contacts_handler::{
    AddMemberRequest, AddressBookResponse, CreateAddressBookRequest, CreateContactRequest,
    GroupNameRequest, UpdateAddressBookRequest, UpdateContactRequest,
};
use crate::interfaces::api::handlers::dedup_handler::{HashCheckResponse, StatsResponse};
use crate::interfaces::api::handlers::file_handler::MoveFilePayload;
use crate::interfaces::middleware::server_status::{HeaderPayload, ProgressHeader};

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
        handlers::auth_handler::oidc_backchannel_logout,
        handlers::auth_handler::oidc_link_start,
        handlers::auth_handler::oidc_unlink,
        // DPoP post-redirect bind (Gate 3), magic-link SEND (the
        // outbound half of the passwordless flow — /magic/v1/{token}
        // redemption is a browser redirect, not an API endpoint),
        // profile edit (PATCH — the read is via /me), and the
        // external-user upgrade path.
        handlers::auth_handler::dpop_bind,
        handlers::auth_handler::send_magic_link,
        handlers::auth_handler::update_profile,
        handlers::auth_handler::upgrade_to_internal,
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
        handlers::dedup_handler::check_hashes_batch,
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
        handlers::trash_handler::empty_trash_for_drive,
        // Share handlers (free functions)
        handlers::share_handler::create_shared_link,
        handlers::share_handler::get_shared_link,
        handlers::share_handler::get_user_shares,
        handlers::share_handler::update_shared_link,
        handlers::share_handler::delete_shared_link,
        handlers::share_handler::access_shared_item,
        handlers::share_handler::verify_shared_item_password,
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
        // Notifications (Slice E — bell + retention)
        handlers::notifications_handler::list_notifications,
        handlers::notifications_handler::unread_count,
        handlers::notifications_handler::mark_read,
        handlers::notifications_handler::mark_all_read,
        handlers::notifications_handler::delete_notification,
        // Photos handler (free function)
        handlers::photos_handler::list_photos,
        handlers::photos_handler::list_photos_geo,
        // People / face-clustering handlers — mounted only when
        // `OXICLOUD_ENABLE_FACES` is on; each handler is defensive
        // (`disabled()` returns 404 otherwise). All work is strictly
        // caller-scoped by `PeopleService`.
        handlers::people_handler::list_people,
        handlers::people_handler::person_photos,
        handlers::people_handler::rename_person,
        handlers::people_handler::merge_people,
        handlers::people_handler::recluster,
        handlers::people_handler::delete_all,
        handlers::people_handler::faces_for_file,
        // Drive handler (free function)
        handlers::drive_handler::list_drives,
        handlers::drive_handler::delete_drive,
        handlers::drive_handler::list_drive_members,
        handlers::drive_handler::add_drive_member,
        handlers::drive_handler::update_drive_member,
        handlers::drive_handler::remove_drive_member,
        handlers::drive_handler::update_drive_policies,
        handlers::drive_handler::update_drive_quota,
        // Users handler — public profile lookup (auth-required, but
        // returns the callee's public view, not `/me`'s self-view).
        handlers::users_handler::get_user_profile,
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
        // handlers::admin_handler::verify_migration retired in
        // slice 7 — superseded by `blobs_consistency?storage=<name>`.
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
        handlers::admin_handler::pause_job,
        handlers::admin_handler::list_job_run_findings,
        handlers::admin_handler::purge_job_runs,
        // Admin drive management — full CRUD on drives + membership,
        // distinct from the user-facing /api/drives surface (admin can
        // touch any drive; user can touch only those they're an owner
        // of).
        handlers::admin_handler::list_all_drives,
        handlers::admin_handler::delete_drive_admin,
        handlers::admin_handler::list_drive_members_admin,
        handlers::admin_handler::add_drive_member_admin,
        handlers::admin_handler::update_drive_member_admin,
        handlers::admin_handler::remove_drive_member_admin,
        // Admin SMTP diagnostics + backend rotation + user promotion.
        // The two SMTP fns were pub-only-for-utoipa (private in-router
        // helpers before this branch); see the handler for the
        // read-only vs test-send semantics.
        handlers::admin_handler::get_smtp_info,
        handlers::admin_handler::send_smtp_test,
        handlers::admin_handler::trigger_backend_rotate,
        handlers::admin_handler::admin_promote_external_to_internal,
        handlers::admin_handler::transfer_ownership,
        // Admin sessions panel — list + revoke. Function names lack
        // the `_admin_` suffix; the `/api/admin/` prefix comes from
        // the router mount, not the handler name.
        handlers::admin_handler::list_sessions,
        handlers::admin_handler::revoke_session,
        // OPAQUE aPAKE endpoints — full register + login handshake
        // (`docs/plan/opaque-only.md`). Public routes; the register/*
        // pair is session-authenticated (the SPA already has a
        // legacy-login bearer before it enrolls an envelope, per the
        // Phase 2 silent-migration flow), the login/* triplet is not
        // (they ARE the login).
        handlers::opaque_auth_handler::opaque_params,
        handlers::opaque_auth_handler::register_start,
        handlers::opaque_auth_handler::register_finish,
        handlers::opaque_auth_handler::login_lookup,
        handlers::opaque_auth_handler::login_ke1,
        handlers::opaque_auth_handler::login_ke3,
        // Grant / ReBAC handlers (free functions)
        handlers::grant_handler::create_grant,
        handlers::grant_handler::revoke_grant,
        handlers::grant_handler::set_role,
        handlers::grant_handler::list_incoming,
        handlers::grant_handler::list_shared_with_me,
        handlers::grant_handler::list_outgoing,
        handlers::grant_handler::list_my_shares,
        handlers::grant_handler::list_on_resource,
        handlers::grant_handler::notify_grant_recipient,
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
        // Public server-config discovery.
        handlers::config_handler::get_config,
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
            PublicUserDto,
            LoginDto,
            RegisterDto,
            SetupAdminDto,
            AuthResponseDto,
            ChangePasswordDto,
            RefreshTokenDto,
            SystemStatus,
            // Public server-config discovery — `GET /api/config`.
            ServerConfigDto,
            FeaturesDto,
            // Notifications (Slice E) — bell REST DTOs + per-kind
            // typed payload structs. The generic `NotificationDto.payload`
            // stays `serde_json::Value` on the schema; each per-kind
            // struct (e.g. `SharegrantedPayload`) is registered here
            // so FE consumers can type-narrow on `kind`. Adding a
            // new kind = one more entry here + one Rust struct.
            handlers::notifications_handler::NotificationDto,
            handlers::notifications_handler::ListResponseDto,
            handlers::notifications_handler::UnreadCountDto,
            handlers::notifications_handler::MarkAllReadResponseDto,
            crate::domain::entities::notification::SharegrantedPayload,
            HeaderPayload,
            ProgressHeader,
            OidcProviderInfoDto,
            OidcExchangeDto,
            // Admin sessions panel — wire shape for `/api/admin/sessions`.
            // `SessionOrigin` is the discriminated enum for the `origin`
            // field, so it must ship separately for consumers to type
            // the union.
            crate::application::dtos::session_dto::SessionSummaryDto,
            crate::domain::entities::session::SessionOrigin,
            // OPAQUE aPAKE — request/response shapes for every step in
            // the register + login handshake. Base64url-wrapped OPRF /
            // AKE payloads; see docs/plan/opaque-only.md for the wire
            // grammar. `OpaqueParamsResponse` is the /params publish.
            handlers::opaque_auth_handler::OpaqueRegisterStartDto,
            handlers::opaque_auth_handler::OpaqueRegisterStartResponse,
            handlers::opaque_auth_handler::OpaqueRegisterFinishDto,
            handlers::opaque_auth_handler::OpaqueLookupDto,
            handlers::opaque_auth_handler::OpaqueLookupResponse,
            handlers::opaque_auth_handler::OpaqueLoginKe1Dto,
            handlers::opaque_auth_handler::OpaqueLoginKe1Response,
            handlers::opaque_auth_handler::OpaqueLoginKe3Dto,
            handlers::opaque_auth_handler::OpaqueParamsResponse,
            // People / face-clustering — response DTOs published by the
            // /api/people/* endpoints (list_people, faces_for_file), plus
            // the two request bodies (rename, merge). Face indexing pipeline
            // is documented in `face_indexing_service` + `onnx_face_analyzer`.
            crate::application::dtos::people_dto::PersonDto,
            crate::application::dtos::people_dto::FaceBoxDto,
            handlers::people_handler::RenameBody,
            handlers::people_handler::MergeBody,
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
        // OXICLOUD_TAG (bare tag, no `git describe` suffix), NOT
        // OXICLOUD_VERSION. This spec is regenerated into a committed
        // JSON file under resources/gen/ and checked by CI drift on
        // every PR; using the full version would produce a different
        // hash on every developer's local checkout (each with its own
        // `git describe` output) and false-positive the check on
        // every PR. See build.rs → strip_describe_suffix().
        title = "OxiCloud API",
        version = env!("OXICLOUD_TAG"),
        description = "REST API for OxiCloud — self-hosted cloud storage, calendar & contacts",
        license(name = "MIT")
    )
)]
pub struct ApiDoc;

/// The vendor extension marking an operation reachable with only a
/// public-share token. `share:read` names a CAPABILITY, not a caller: the set
/// of reads a share link permits.
pub const SCOPE_EXTENSION: &str = "x-oxicloud-scope";
const SHARE_READ_SCOPE: &str = "share:read";

/// Prefix of the admin-only nest. `require_admin` wraps exactly this subtree
/// (`routes.rs`), so the path prefix IS the gate — which is what lets the
/// marker below be derived rather than hand-maintained across 40 handlers.
const ADMIN_PREFIX: &str = "/api/admin";
const ADMIN_SCOPE: &str = "role:admin";

/// Injects the `bearerAuth` HTTP Bearer security scheme into the generated
/// spec, and marks the operations a public-share visitor may reach.
///
/// ## Why an extension and not a scope
///
/// The obvious place for `share:read` is the scopes array on each operation's
/// security requirement. It is the wrong place: OpenAPI defines that array
/// only for `oauth2` and `openIdConnect` schemes and requires it to be EMPTY
/// for every other type. `bearerAuth` is `http`/bearer, so scopes there make
/// the document non-conformant — which is why Swagger UI renders nothing for
/// them. Vendor extensions are the sanctioned way to carry information the
/// spec has no field for, so the marker is `x-oxicloud-scope` and the scopes
/// array stays empty. A sentence is appended to the operation's description
/// as well, because that is the part Swagger UI actually shows a human.
///
/// ## Why it is generated, not annotated
///
/// The marked set is read straight from `ANONYMOUS_ALLOWLIST` — the constant
/// the runtime gate itself uses. Hand-annotating each handler would put the
/// published description and the enforced rule in two places that can
/// disagree; deriving one from the other means they cannot. The gate stays
/// the authority: an extension here grants nothing.
struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            "bearerAuth",
            SecurityScheme::Http(Http::new(HttpAuthScheme::Bearer)),
        );

        // GET only, matching `anonymous_may_reach`: a method mounted later on
        // an already-listed path does not inherit share access, and the spec
        // must not imply it does.
        for path in crate::interfaces::middleware::anonymous_allowlist::ANONYMOUS_ALLOWLIST {
            let Some(item) = openapi.paths.paths.get_mut(*path) else {
                // An allowlisted route with no `#[utoipa::path]` — undocumented
                // rather than wrong. `share_read_marks_exactly_the_allowlist`
                // fails on it, which is the nudge to document it.
                continue;
            };
            let Some(op) = item.get.as_mut() else {
                continue;
            };
            op.extensions = Some(
                utoipa::openapi::extensions::ExtensionsBuilder::new()
                    .add(SCOPE_EXTENSION, serde_json::json!(SHARE_READ_SCOPE))
                    .build(),
            );
            let note = "Reachable with a public-share token (`share:read`): a visitor \
                        holding an unlocked share link may call this, scoped to what \
                        that share grants.";
            op.description = Some(match op.description.take() {
                Some(existing) if !existing.is_empty() => format!("{existing}\n\n{note}"),
                _ => note.to_owned(),
            });
        }

        // The admin nest. Derived from the path prefix for the same reason:
        // `require_admin` wraps exactly this subtree, so the prefix is the
        // gate, and marking 40 handlers by hand would be 40 chances to forget.
        //
        // EVERY method, not just GET — unlike the share marker. A share
        // visitor may only read, so a `PUT` on an allowlisted path is a
        // different decision; admin is a property of the whole subtree.
        let admin_note = "Requires the deployment administrator role \
                          (`role:admin`); enforced by the `require_admin` layer \
                          on the `/api/admin` nest.";
        for (path, item) in openapi.paths.paths.iter_mut() {
            if !path.starts_with(ADMIN_PREFIX) {
                continue;
            }
            for op in [
                item.get.as_mut(),
                item.put.as_mut(),
                item.post.as_mut(),
                item.delete.as_mut(),
                item.patch.as_mut(),
                item.head.as_mut(),
                item.options.as_mut(),
                item.trace.as_mut(),
            ]
            .into_iter()
            .flatten()
            {
                op.extensions = Some(
                    utoipa::openapi::extensions::ExtensionsBuilder::new()
                        .add(SCOPE_EXTENSION, serde_json::json!(ADMIN_SCOPE))
                        .build(),
                );
                op.description = Some(match op.description.take() {
                    Some(existing) if !existing.is_empty() => {
                        format!("{existing}\n\n{admin_note}")
                    }
                    _ => admin_note.to_owned(),
                });
            }
        }
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
            "PublicUserDto",
        ] {
            assert!(schemas.contains_key(name), "missing schema: {name}");
        }

        let json = serde_json::to_string(&spec).expect("spec should serialise to JSON");
        assert!(json.len() > 1000, "spec JSON suspiciously small");
    }

    /// The spec marks exactly the allowlisted routes — checked in BOTH
    /// directions.
    ///
    /// The marker is generated FROM `ANONYMOUS_ALLOWLIST`, so this cannot
    /// catch a human forgetting an annotation; it is a regression guard on the
    /// generation. It still earns its place: it fails if an allowlisted route
    /// carries no `#[utoipa::path]` (silently skipped by the injector, leaving
    /// the spec understating what a share link reaches), and it fails if the
    /// injector ever marks something the gate does not allow — which would
    /// promise visitors an endpoint that 403s and, worse, invite someone to
    /// "fix" the mismatch by widening the allowlist.
    ///
    /// The allowlist is the authority. This asserts the published description
    /// matches it.
    #[test]
    fn scope_markers_match_their_gates() {
        use crate::interfaces::middleware::anonymous_allowlist::ANONYMOUS_ALLOWLIST;
        use std::collections::BTreeSet;

        // Walked as serialised JSON rather than through utoipa's types: this
        // is the document consumers actually read, and it does not break when
        // `PathItem`'s shape changes between utoipa releases.
        let spec = serde_json::to_value(ApiDoc::openapi()).expect("spec should serialise");
        let paths = spec["paths"].as_object().expect("spec has no paths object");

        let declared: BTreeSet<&str> = paths
            .iter()
            .filter(|(_, item)| {
                item.as_object().is_some_and(|methods| {
                    methods
                        .values()
                        .any(|op| op[SCOPE_EXTENSION] == serde_json::json!(SHARE_READ_SCOPE))
                })
            })
            .map(|(path, _)| path.as_str())
            .collect();

        // Scopes belong to oauth2/openIdConnect schemes only; on an `http`
        // scheme the array must stay empty or the document is non-conformant
        // (and Swagger UI silently drops it). Pinned so nobody reintroduces
        // the marker there.
        for (path, item) in paths {
            for (method, op) in item.as_object().into_iter().flatten() {
                for req in op["security"].as_array().into_iter().flatten() {
                    for (scheme, scopes) in req.as_object().into_iter().flatten() {
                        assert!(
                            scopes.as_array().is_none_or(|s| s.is_empty()),
                            "{method} {path}: `{scheme}` is an http scheme, \
                             so its scopes array must be empty (got {scopes})"
                        );
                    }
                }
            }
        }

        // The admin half of the same contract. `require_admin` wraps exactly
        // the `/api/admin` nest, so prefix and marker must agree both ways: an
        // unmarked admin path understates the requirement, and a marked path
        // outside the nest claims a gate that is not there — the more
        // dangerous direction, since it reads as "already protected".
        let admin_marked: BTreeSet<&str> = paths
            .iter()
            .filter(|(_, item)| {
                item.as_object().is_some_and(|methods| {
                    methods
                        .values()
                        .any(|op| op[SCOPE_EXTENSION] == serde_json::json!(ADMIN_SCOPE))
                })
            })
            .map(|(path, _)| path.as_str())
            .collect();
        let admin_nested: BTreeSet<&str> = paths
            .keys()
            .map(String::as_str)
            .filter(|p| p.starts_with(ADMIN_PREFIX))
            .collect();
        assert_eq!(
            admin_marked,
            admin_nested,
            "`{ADMIN_SCOPE}` has drifted from the `{ADMIN_PREFIX}` nest.\n\
             marked but not nested: {:?}\n\
             nested but unmarked: {:?}",
            admin_marked.difference(&admin_nested).collect::<Vec<_>>(),
            admin_nested.difference(&admin_marked).collect::<Vec<_>>(),
        );

        let allowlisted: BTreeSet<&str> = ANONYMOUS_ALLOWLIST.iter().copied().collect();

        assert_eq!(
            declared,
            allowlisted,
            "`{SCOPE_EXTENSION}: {SHARE_READ_SCOPE}` in the OpenAPI spec has \
             drifted from ANONYMOUS_ALLOWLIST.\n\
             marked only in the spec: {:?}\n\
             allowlisted but unmarked (missing a `#[utoipa::path]`?): {:?}",
            declared.difference(&allowlisted).collect::<Vec<_>>(),
            allowlisted.difference(&declared).collect::<Vec<_>>(),
        );
    }
}
