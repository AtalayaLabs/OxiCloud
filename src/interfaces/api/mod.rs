pub mod cookie_auth;
pub mod deserializer;
pub mod etag;
pub mod handlers;
pub mod policy_declarations;
pub mod policy_router;
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
use crate::application::dtos::content_metadata_dto::FileContentMetadataDto;
use crate::application::dtos::cursor::CursorListResponse;
use crate::application::dtos::display_helpers::ResourceCategory;
use crate::application::dtos::drive_dto::{DriveDto, DriveKindDto};
use crate::application::dtos::favorites_dto::{
    BatchFavoritesResult, BatchFavoritesStats, FavoriteItemDto, FavoritesOrderBy,
    FavoritesResourceItemDto,
};
use crate::application::dtos::file_dto::FileDto;
use crate::application::dtos::folder_dto::{
    AccessSourceDriveDto, AccessSourceDto, AccessSourceKind, AccessSourceSubjectDto,
    AccessSourceSubjectKind, CreateFolderDto, FolderAncestorDto, FolderAncestorsDto, FolderDto,
    FolderOrderBy, FolderResourceItemDto, MoveFolderDto, RenameFolderDto,
};
use crate::application::dtos::folder_listing_dto::FolderListingDto;
use crate::application::dtos::grant_dto::{
    CreateGrantDto, GrantDto, IncomingGrantOrderBy, OutgoingGrantOrderBy, OutgoingResourceItemDto,
    PermissionDto, ResourceContentDto, ResourceDto, ResourceTypeDto, RoleDto, SharedWithMeDto,
    SharedWithMeItemDto, SubjectDto, SubjectTypeDto, UpdateGrantRoleDto,
};
use crate::application::dtos::i18n_dto::{
    LocaleDto, TranslationErrorDto, TranslationRequestDto, TranslationResponseDto,
};
use crate::application::dtos::pagination::{PaginationDto, PaginationRequestDto};
use crate::application::dtos::photos_dto::{PhotoKind, PhotoOrderBy, PhotoResourceItemDto};
use crate::application::dtos::recent_changes_dto::{FileChangeItemDto, RecentChangesOrderBy};
use crate::application::dtos::recent_dto::{RecentItemDto, RecentOrderBy, RecentResourceItemDto};
use crate::application::dtos::search_dto::{
    SearchCriteriaDto, SearchFileResultDto, SearchFolderResultDto, SearchMeta, SearchOrderBy,
    SearchResourceItem, SearchResourcesDto, SearchResultsDto, SearchSuggestionItem,
    SearchSuggestionsDto, SearchTypeFilter,
};
use crate::application::dtos::share_dto::{CreateShareDto, ShareDto, UpdateShareDto};
use crate::application::dtos::trash_dto::{
    DeletePermanentlyRequest, MoveToTrashRequest, RestoreFromTrashRequest, TrashOrderBy,
    TrashResourceItemDto, TrashResourcesDto, TrashedItemDto,
};
use crate::application::dtos::upload_preflight_dto::{
    PreflightExistingFileDto, PreflightExistingFolderDto, PreflightKindConflictDto,
    UploadPreflightRequest, UploadPreflightResponse,
};
use crate::application::dtos::user_dto::{
    AuthResponseDto, ChangePasswordDto, LoginDto, OidcExchangeDto, PublicUserDto, RefreshTokenDto,
    RegisterDto, SetupAdminDto,
};
use crate::application::ports::chunked_upload_ports::{
    ChunkUploadResponseDto, CreateUploadResponseDto, UploadStatusResponseDto,
};
use crate::domain::services::name_conflict::ConflictPolicy;
use crate::interfaces::api::handlers::calendar_handler::{
    CalendarEventResponse, CalendarResponse, CreateCalendarRequest, SaveEventRequest,
    UpdateCalendarRequest,
};
use crate::interfaces::api::handlers::chunked_upload_handler::{
    CompleteUploadResponse, CreateUploadRequest,
};
use crate::interfaces::api::handlers::config_handler::{FeaturesDto, ServerConfigDto};
use crate::interfaces::api::handlers::contacts_handler::{
    AddContactToGroupRequest, AddressBookResponse, ContactGroupNameRequest,
    CreateAddressBookRequest, CreateContactRequest, UpdateAddressBookRequest, UpdateContactRequest,
};
use crate::interfaces::api::handlers::dedup_handler::{
    DedupStatsResponse, HashCheckAdminResponse, HashCheckResponse, HashSiblingDto,
};
use crate::interfaces::api::handlers::file_handler::MoveFilePayload;
use crate::interfaces::middleware::server_status::{HeaderPayload, ProgressHeader};

#[derive(OpenApi)]
#[openapi(
    modifiers(&SecurityAddon),
    paths(
        // WOPI — only the FRONTEND endpoint. `/wopi/*` is the protocol
        // surface the Office server calls with `?access_token=`; it has no
        // `CurrentCaller`, so it is deliberately absent from the spec and
        // therefore from the policy engine. See § Between PR 1 and PR 2 in
        // `docs/plan/scope-gates.md`.
        handlers::wopi_handler::get_editor_url,
        handlers::admin_appearance_handler::get_appearance,
        handlers::admin_appearance_handler::reload_themes,
        handlers::admin_appearance_handler::select_theme,
        // Auth handlers (public, protected, OIDC)
        handlers::auth_handler::register,
        handlers::auth_handler::login,
        handlers::auth_handler::refresh_token,
        handlers::auth_handler::get_current_user,
        handlers::auth_handler::change_password,
        handlers::auth_handler::logout,
        handlers::auth_handler::list_own_sessions,
        handlers::auth_handler::revoke_own_session,
        handlers::auth_handler::setup_admin,
        handlers::auth_handler::oidc_authorize,
        handlers::server_status_handler::get_server_status,
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
        handlers::auth_handler::update_user_image,
        handlers::auth_handler::upgrade_to_internal,
        // File handlers (free functions — see file_handler.rs for why)
        handlers::file_handler::list_files_query,
        handlers::file_handler::upload_file_with_thumbnails,
        handlers::file_handler::replace_file_content,
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
        handlers::folder_handler::upload_preflight,
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
        handlers::dedup_handler::admin_check_hash,
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
        handlers::recent_changes_handler::list_recent_changes,
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
        handlers::photos_handler::list_photos_resources,
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
        // Calendar handlers (REST surface for the SPA)
        handlers::calendar_handler::list_calendars,
        handlers::calendar_handler::create_calendar,
        handlers::calendar_handler::update_calendar,
        handlers::calendar_handler::delete_calendar,
        handlers::calendar_handler::list_events,
        handlers::calendar_handler::create_event,
        handlers::calendar_handler::update_event,
        handlers::calendar_handler::delete_event,
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
        handlers::admin_handler::get_backend_write_lock,
        handlers::admin_handler::acquire_backend_write_lock,
        handlers::admin_handler::release_backend_write_lock,
        handlers::admin_handler::list_ops_banners,
        handlers::admin_handler::create_ops_banner,
        handlers::admin_handler::update_ops_banner,
        handlers::admin_handler::delete_ops_banner,
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
        // Admin drive management — full CRUD on drives + membership,
        // distinct from the user-facing /api/drives surface (admin can
        // touch any drive; user can touch only those they're an owner
        // of).
        handlers::admin_handler::list_all_drives,
        handlers::admin_handler::get_drive_policy_defaults,
        handlers::admin_handler::get_drive_policy_drift,
        handlers::admin_handler::set_drive_policy_defaults,
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
        handlers::admin_handler::get_webhook_info,
        handlers::admin_handler::send_smtp_test,
        handlers::admin_handler::send_webhook_test,
        // Alerting policy — what gets sent, as opposed to the transport
        // endpoints above.
        handlers::admin_handler::get_notify_info,
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
        // Device-authorization grant (RFC 8628) — client flow + user
        // verification + device-management endpoints.
        handlers::device_auth_handler::device_authorize,
        handlers::device_auth_handler::device_token,
        handlers::device_auth_handler::device_verify_info,
        handlers::device_auth_handler::device_verify_action,
        handlers::device_auth_handler::list_devices,
        handlers::device_auth_handler::revoke_device,
        // App passwords — persistent credentials for DAV / NC-Basic clients
        // (DAVx5, Thunderbird, rclone, NC desktop). Self-service CRUD; the
        // plaintext secret is returned ONCE at creation and never again.
        handlers::app_password_handler::create_app_password,
        handlers::app_password_handler::list_app_passwords,
        handlers::app_password_handler::revoke_app_password,
    ),
    components(
        // ── DO NOT delete a registration just because a linter calls it
        //    "never used". ─────────────────────────────────────────────
        //
        // Redocly's `no-unused-components` flags every schema here that
        // no `#[utoipa::path]` response or request body `$ref`s — 22 of
        // them at the time of writing. `check-openapi-lint` keeps that
        // rule OFF precisely because 17 of the 22 must never be acted
        // on, so you will not see it from `just check-openapi`; you will
        // see it from any other OpenAPI linter pointed at this spec.
        // Either way, unreferenced is NOT the same as unused, and acting
        // on it naively breaks three things a linter cannot see:
        //
        //   * Generics. OpenAPI has none, so `CursorListResponse<T>` is
        //     emitted once per instantiation and utoipa INLINES the item
        //     type into each wrapper instead of `$ref`ing it. Every
        //     `*ResourceItemDto` and the bare `CursorListResponse` are
        //     reported unreferenced while being load-bearing — delete one
        //     and the paths collapse onto a single bare
        //     `CursorListResponse`. See its own doc comment.
        //   * TypeScript. `@hey-api/openapi-ts` emits a type for EVERY
        //     entry below, `$ref`d or not, and the SPA imports from
        //     `generated/openapi/types.gen` in ~57 places. Three of the
        //     flagged schemas are imported by
        //     `frontend/src/lib/api/types.ts` right now —
        //     `SessionSummaryDto`, `SharegrantedPayload`,
        //     `TrashResourceItemDto`. Removing them here fails
        //     `npm run check`, not this file's build.
        //   * AsyncAPI. The WS surface has its own spec and its own
        //     generated tree; a DTO shared between REST and WS would look
        //     orphaned here while carrying the WS contract.
        //
        // Some entries are also published deliberately with nothing
        // `$ref`ing them, so consumers can type-narrow a payload the
        // schema types as `serde_json::Value` — see the notifications
        // block below for the worked example.
        //
        // Before removing any entry, confirm BOTH come back empty:
        //
        //   grep -rn '\bTheDto\b' frontend/src --include='*.ts' \
        //        --include='*.svelte' | grep -v '/generated/'
        //   grep -n '"TheDto"' resources/gen/asyncapi.json
        //
        // Unreferenced in the spec AND unimported by either consumer is
        // the only safe signal — necessary but not sufficient, since a
        // generic artefact above passes both greps and must still stay.
        // The full six-class audit, and why the rule is off rather than
        // merely downgraded, is in `checks/openapi/redocly.yaml`.
        schemas(
            handlers::wopi_handler::EditorUrlResponse,
            crate::application::dtos::theme_dto::AppearanceDto,
            crate::application::dtos::theme_dto::ThemeDiagnosticDto,
            crate::application::dtos::theme_dto::SelectThemeDto,
            // Shared error body — every 4xx / 5xx response conforms to
            // this shape. Registered so the generated SDK and third-
            // party clients have a typed body to deserialize against
            // instead of `unknown`. See `interfaces::errors::ApiErrorBody`
            // for the known `error_type` values.
            crate::interfaces::errors::ApiErrorBody,
            // RFC 6749 § 5.2 / RFC 7009 OAuth error response — used by
            // endpoints federated IdPs call directly (OIDC Back-Channel
            // Logout today; future federated endpoints too). Keeps the
            // spec-required `{error, error_description}` shape separate
            // from the OxiCloud-flavoured `ApiErrorBody` — different
            // audience, different vocabulary.
            crate::interfaces::errors::OAuthErrorResponse,
            // Folder schemas
            FolderDto,
            FolderOrderBy,
            CreateFolderDto,
            ConflictPolicy,
            UploadPreflightRequest,
            UploadPreflightResponse,
            PreflightExistingFolderDto,
            PreflightExistingFileDto,
            PreflightKindConflictDto,
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
            crate::application::services::delta_upload_service::DeltaChunkRef,
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
            // Device-authorization grant (RFC 8628).
            crate::application::dtos::device_auth_dto::DeviceAuthorizeRequestDto,
            crate::application::dtos::device_auth_dto::DeviceAuthorizeResponseDto,
            crate::application::dtos::device_auth_dto::DeviceTokenRequestDto,
            crate::application::dtos::device_auth_dto::DeviceTokenSuccessDto,
            crate::application::dtos::device_auth_dto::DeviceTokenPendingDto,
            crate::application::dtos::device_auth_dto::DeviceVerifyRequestDto,
            crate::application::dtos::device_auth_dto::DeviceVerifyInfoDto,
            crate::application::dtos::device_auth_dto::DeviceVerifyActionResponseDto,
            crate::application::services::device_auth_service::DeviceInfoDto,
            // App-password management.
            crate::application::dtos::app_password_dto::CreateAppPasswordRequestDto,
            crate::application::dtos::app_password_dto::AppPasswordCreatedResponseDto,
            crate::application::dtos::app_password_dto::AppPasswordInstructions,
            crate::application::dtos::app_password_dto::AppPasswordSummaryDto,
            crate::application::dtos::app_password_dto::AppPasswordListResponseDto,
            crate::application::dtos::app_password_dto::AppPasswordRevokeResponseDto,
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
            handlers::notifications_handler::NotificationListResponseDto,
            handlers::notifications_handler::UnreadCountDto,
            handlers::notifications_handler::MarkAllReadResponseDto,
            // Imported by `frontend/src/lib/api/types.ts` — keep. Reported
            // as "never used" by the spec linter; see the guard at the top
            // of `components(`.
            crate::domain::entities::notification::SharegrantedPayload,
            HeaderPayload,
            ProgressHeader,
            OidcExchangeDto,
            // Admin sessions panel — wire shape for `/api/admin/sessions`.
            // `SessionOrigin` is the discriminated enum for the `origin`
            // field, so it must ship separately for consumers to type
            // the union.
            // Imported by `frontend/src/lib/api/types.ts` — keep. Reported
            // as "never used" because `/api/admin/sessions` documents
            // `body = Object`; pointing that response at this DTO is the
            // right fix, not deleting it.
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
            handlers::people_handler::RenamePersonRequest,
            handlers::people_handler::MergePeopleRequest,
            // Share schemas
            ShareDto,
            CreateShareDto,
            UpdateShareDto,
            // Trash schemas
            TrashedItemDto,
            TrashOrderBy,
            // Imported by `frontend/src/lib/api/types.ts` (re-exported as
            // `TrashResourceItem`) — keep, despite the linter calling it
            // unused. `CursorListResponse<TrashResourceItemDto>` below is
            // what the wire actually carries.
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
            SearchOrderBy,
            SearchTypeFilter,
            ResourceCategory,
            SearchResultsDto,
            SearchFileResultDto,
            SearchFolderResultDto,
            SearchSuggestionsDto,
            SearchSuggestionItem,
            // Favorites schemas
            FavoriteItemDto,
            FavoritesOrderBy,
            FavoritesResourceItemDto,
            BatchFavoritesResult,
            BatchFavoritesStats,
            // Recent schemas
            RecentItemDto,
            RecentOrderBy,
            RecentResourceItemDto,
            // Recent-changes schemas — an unregistered enum silently drops
            // from the spec, which is how a typed axis becomes an untyped
            // string for every SDK consumer.
            FileChangeItemDto,
            RecentChangesOrderBy,
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
            HashCheckAdminResponse,
            HashSiblingDto,
            DedupStatsResponse,
            // Photos — the two filter enums `PhotosResourcesQueryParams`
            // references via `Option<_>`. utoipa emits them as
            // `$ref`-s on the params; without registering them here
            // the FE's openapi-ts gen prints
            // "Skipping unresolvable $ref" and the generated SDK is
            // missing their TS types.
            PhotoKind,
            PhotoOrderBy,
            // The response item itself. Registering it was necessary but not
            // sufficient: the ENVELOPE also has to name which item it holds,
            // or the path still documents the wrong one. See the seven
            // envelope aliases registered below.
            PhotoResourceItemDto,
            // ── The seven listing envelopes ──────────────────────────────
            //
            // ⚠ One schema per instantiation, or six of seven endpoints are
            // documented as returning the WRONG item type.
            //
            // OpenAPI has no generics, so utoipa emits one schema per NAME.
            // A bare `ToSchema` on `CursorListResponse<T>` therefore
            // collapsed every instantiation into a single
            // `CursorListResponse` whose `items` was hard-wired to whichever
            // one it happened to resolve (`OutgoingResourceItemDto`) — and
            // every path `$ref`d that one, while the real container types sat
            // in `components.schemas` referenced by nothing.
            //
            // Spelled out with the item type, which is what makes utoipa emit
            // a distinct schema per endpoint. `#[aliases(..)]` on the struct
            // would be tidier but was removed in utoipa 5.
            CursorListResponse<FolderResourceItemDto>,
            CursorListResponse<FavoritesResourceItemDto>,
            CursorListResponse<RecentResourceItemDto>,
            CursorListResponse<FileChangeItemDto>,
            CursorListResponse<TrashResourceItemDto>,
            CursorListResponse<SharedWithMeItemDto>,
            CursorListResponse<OutgoingResourceItemDto>,
            CursorListResponse<PhotoResourceItemDto>,
            // `GET /api/files/{id}/metadata`, which was annotated
            // `body = Object` — so the generated SDK carried no type for it
            // and the SPA kept a hand-written `FileMetadata` with no drift
            // guard, the same shape of hole the comment above describes.
            FileContentMetadataDto,
            // Admin jobs registry — the response body of
            // `GET /api/admin/jobs`, previously `body = Object`.
            //
            // Registering these closes the same hole
            // `PhotoResourceItemDto` above closed: the path was in the
            // spec with an untyped body, the generated SDK carried no TS
            // type, and the frontend kept HAND-WRITTEN `JobSummary` /
            // `JobParam` with no drift guard. `JobParam.allowed` — the
            // field that decides whether the admin panel renders a
            // `<select>` or a free-text box — was invisible to the spec
            // on the day it shipped.
            crate::infrastructure::scheduler::JobSummary,
            crate::infrastructure::scheduler::JobParam,
            crate::infrastructure::scheduler::JobParamType,
            crate::infrastructure::scheduler::JobParamDefault,
            crate::infrastructure::scheduler::JobParamValue,
            crate::infrastructure::scheduler::JobOutcome,
            crate::infrastructure::scheduler::Mutates,
            crate::infrastructure::scheduler::ScheduledVia,
            crate::infrastructure::scheduler::PausedRunBrief,
            crate::infrastructure::scheduler::StartupTrigger,
            // `JobSummary.last_run_status`. Published so the panel keeps
            // an exhaustive switch over the six states instead of the
            // `string` an `Option<String>` field generated.
            crate::infrastructure::scheduler::RunStatus,
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
            ContactGroupNameRequest,
            AddContactToGroupRequest,
            // Calendar schemas
            CalendarResponse,
            CreateCalendarRequest,
            UpdateCalendarRequest,
            CalendarEventResponse,
            SaveEventRequest,
            // Grant / ReBAC schemas
            SubjectTypeDto,
            SubjectDto,
            ResourceTypeDto,
            ResourceDto,
            PermissionDto,
            RoleDto,
            CreateGrantDto,
            UpdateGrantRoleDto,
            GrantDto,
            IncomingGrantOrderBy,
            OutgoingGrantOrderBy,
            SharedWithMeDto,
            SharedWithMeItemDto,
            OutgoingResourceItemDto,
            // Drive schemas
            DriveDto,
            crate::domain::entities::drive::DrivePolicies,
            crate::application::services::drive_policy_defaults_service::PolicyDefaultsImpactDto,
            crate::application::services::drive_policy_defaults_service::PolicyKnobImpactDto,
            crate::application::services::drive_policy_defaults_service::WeakerDriveDto,
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
        (name = "calendars", description = "Calendars and events endpoints (SPA JSON surface)"),
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
/// The role FLOOR an operation requires: `internal`, `admin` or `owner`.
///
/// Separate from [`SCOPE_EXTENSION`] because they are different axes.
/// One key used to carry both — `role:admin` sat in `x-oxicloud-scope`
/// alongside `share:read`, so a consumer could not tell a role floor from
/// a credential scope from a principal kind. Three axes, three keys.
pub const MINROLE_EXTENSION: &str = "x-oxicloud-minrole";

/// The least privileged PRINCIPAL an operation admits, when it is not the
/// default authenticated user: `visitor` or `public`.
///
/// `share:read` lived in `x-oxicloud-scope` and was never a scope — "a
/// public-share visitor may reach this" is the principal axis.
pub const PRINCIPAL_EXTENSION: &str = "x-oxicloud-principal";

/// The credential SCOPE an operation requires, as `area:access` —
/// `files:ro`. Inert until PR 2 mints credentials carrying a scope set.
pub const SCOPE_EXTENSION: &str = "x-oxicloud-scope";

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
/// Hand-annotating each handler would put the published description and
/// the enforced rule in two places that can disagree; deriving one from
/// the other means they cannot. The policy stays the authority — an
/// extension here grants nothing.
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

/// The wire name of a scope area, as it appears in a credential's scope
/// set — lowercase, so `files:ro` rather than `Files:ReadOnly`.
fn area_name(scope: crate::application::ports::scope_ports::Scope) -> &'static str {
    use crate::application::ports::scope_ports::Scope as S;
    match scope {
        S::Files => "files",
        S::Calendar => "calendar",
        S::Contacts => "contacts",
        S::Photos => "photos",
        S::Music => "music",
        S::Sharing => "sharing",
        S::Account => "account",
        S::Security => "security",
    }
}

/// Operations whose `security(...)` annotation disagrees with the compiled
/// policy, as `(method, path, macro_says, policy_says)`.
///
/// **Do not write `security(("bearerAuth" = []))` in `#[utoipa::path]`.**
/// The engine derives it from the operation's policy — `public()` means no
/// credential, anything else means bearer — so a hand-written one can only
/// agree or be wrong.
///
/// A finding is therefore a stale annotation or a missing declaration.
/// Reported rather than fatal while the two are being reconciled.
pub fn audit_declared_security(
    openapi: &utoipa::openapi::OpenApi,
    registry: &crate::application::services::scope_table::PolicyRegistry,
) -> Vec<(String, String, &'static str, &'static str)> {
    use crate::application::ports::scope_ports::{MinPrincipal, Op};

    let mut findings = Vec::new();
    for (path, item) in openapi.paths.paths.iter() {
        let path_static: &'static str = Box::leak(path.clone().into_boxed_str());
        for (method, op) in [
            ("GET", item.get.as_ref()),
            ("PUT", item.put.as_ref()),
            ("POST", item.post.as_ref()),
            ("DELETE", item.delete.as_ref()),
            ("PATCH", item.patch.as_ref()),
            ("HEAD", item.head.as_ref()),
            ("OPTIONS", item.options.as_ref()),
            ("TRACE", item.trace.as_ref()),
        ]
        .into_iter()
        .filter_map(|(m, o)| o.map(|o| (m, o)))
        {
            let Ok(policy) = registry.compile_one(Op::Rest(method, path_static)) else {
                continue;
            };
            // An EMPTY array is "no credential required"; anything else
            // names a scheme.
            let macro_says = match op.security.as_deref() {
                Some([]) => "no auth",
                _ => "bearer",
            };
            let policy_says = if policy.principal == MinPrincipal::Public {
                "no auth"
            } else {
                "bearer"
            };
            if macro_says != policy_says {
                findings.push((method.to_owned(), path.clone(), macro_says, policy_says));
            }
        }
    }
    findings.sort();
    findings
}

/// Mark every operation from the COMPILED POLICY TABLE.
///
/// Called by the two places that produce a spec: `generate-openapi`, which
/// builds the service graph in memory to get a registry, and the server's
/// `/api/openapi.json`, which already has one installed. Both mark from
/// the same data the gate enforces, so the published description and the
/// enforced rule are one thing rather than two kept in agreement.
///
/// Per-operation, which is what a path prefix could not be: the table
/// knows `GET /api/files/{id}` is visitor-reachable while the `DELETE` on
/// the same path is not.
pub fn mark_policies(
    openapi: &mut utoipa::openapi::OpenApi,
    registry: &crate::application::services::scope_table::PolicyRegistry,
) {
    use crate::application::ports::scope_ports::{Access, MinPrincipal, MinRole, Op};

    const SHARE_NOTE: &str = "Reachable by a public-share VISITOR: someone holding an \
                              unlocked share link may call this, scoped to what that share \
                              grants. No account is required, but a share ring is.";
    const PUBLIC_NOTE: &str = "Reachable with NO credential at all.";
    const INTERNAL_NOTE: &str = "Requires an INTERNAL user. An external (grant-only) account \
                                 is refused: it is a guest invited to specific resources, and \
                                 this surface aggregates more than that invitation implied.";
    const ADMIN_NOTE: &str = "Requires the deployment administrator role; enforced by the \
                              compiled policy declared on the `/api/admin` nest.";
    const OWNER_NOTE: &str = "Requires the SERVER OWNER — stricter than admin. An \
                              administrator is refused.";

    for (path, item) in openapi.paths.paths.iter_mut() {
        // `Op` holds `&'static str` so the table allocates nothing per
        // request. A spec path is one of a fixed set and this runs ONCE
        // per process, so leaking is cheaper than threading a lifetime.
        let path_static: &'static str = Box::leak(path.clone().into_boxed_str());

        for (method, op) in [
            ("GET", item.get.as_mut()),
            ("PUT", item.put.as_mut()),
            ("POST", item.post.as_mut()),
            ("DELETE", item.delete.as_mut()),
            ("PATCH", item.patch.as_mut()),
            ("HEAD", item.head.as_mut()),
            ("OPTIONS", item.options.as_mut()),
            ("TRACE", item.trace.as_mut()),
        ]
        .into_iter()
        .filter_map(|(m, o)| o.map(|o| (m, o)))
        {
            let Ok(policy) = registry.compile_one(Op::Rest(method, path_static)) else {
                // Uncompilable declarations are a BOOT error; the spec is
                // not the place to report one.
                continue;
            };

            // The engine OWNS the security array, so no operation has to
            // (and must not) hand-write `security(...)`: a credential is
            // required unless the principal is Public.
            op.security = Some(if policy.principal == MinPrincipal::Public {
                // `[]`, not `[{}]`: an EMPTY array removes the
                // document-level requirement, while an array holding an
                // empty object means auth is OPTIONAL. Only the first
                // says "no credential needed".
                Vec::new()
            } else {
                vec![utoipa::openapi::security::SecurityRequirement::new::<
                    &str,
                    [&str; 0],
                    &str,
                >("bearerAuth", [])]
            });

            // One key per AXIS. A consumer can now tell "needs admin" from
            // "a share visitor may read this" from "the credential must
            // carry files:rw", which a single key could not express.
            let mut ext = utoipa::openapi::extensions::ExtensionsBuilder::new();
            let mut notes: Vec<&str> = Vec::new();

            if let Some(role) = policy.role {
                let name = match role {
                    MinRole::Internal => "internal",
                    MinRole::Admin => "admin",
                    MinRole::Owner => "owner",
                };
                ext = ext.add(MINROLE_EXTENSION, serde_json::json!(name));
                notes.push(match role {
                    MinRole::Internal => INTERNAL_NOTE,
                    MinRole::Admin => ADMIN_NOTE,
                    MinRole::Owner => OWNER_NOTE,
                });
            }

            // Only when it differs from the default (an authenticated
            // user) — marking every operation "principal: user" would be
            // noise on 270 of them.
            match policy.principal {
                MinPrincipal::Visitor => {
                    ext = ext.add(PRINCIPAL_EXTENSION, serde_json::json!("visitor"));
                    notes.push(SHARE_NOTE);
                }
                MinPrincipal::Public => {
                    ext = ext.add(PRINCIPAL_EXTENSION, serde_json::json!("public"));
                    notes.push(PUBLIC_NOTE);
                    // An EMPTY security array is OpenAPI's "no
                    // authentication required", and it overrides the
                    // document-level requirement. Without this the spec
                    // claims a bearer is needed where none is — which it
                    // does today for `/api/auth/login` and `/api/setup`,
                    // because nothing declares them `public()` yet and
                    // their compiled policy is therefore the default.
                    //
                    // So this is the mechanism that makes the published
                    // bearer requirement TRUE, and it starts working the
                    // moment those routes declare what they are.
                    // `[]`, not `[{}]`: an EMPTY array removes the
                    // document-level requirement, while an array holding
                    // an empty object means auth is OPTIONAL. Only the
                    // first says "no credential needed".
                    op.security = Some(Vec::new());
                }
                MinPrincipal::User => {}
            }

            if let Some((scope, access)) = policy.scope {
                let access = match access {
                    Access::ReadOnly => "ro",
                    Access::ReadWrite => "rw",
                };
                ext = ext.add(
                    SCOPE_EXTENSION,
                    serde_json::json!(format!("{}:{access}", area_name(scope))),
                );
            }

            if notes.is_empty() && policy.scope.is_none() {
                // No axis differs from the default, so no marker — but
                // the security array above is already set.
                continue;
            }
            op.extensions = Some(ext.build());

            let note = notes.join("\n\n");
            op.description = Some(match op.description.take() {
                Some(existing) if !existing.is_empty() => format!("{existing}\n\n{note}"),
                _ => note,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three axes land in three KEYS, and only when they differ from
    /// the default.
    ///
    /// Markers are derived from the compiled table, so there is no copy to
    /// drift from and nothing to compare. What IS worth pinning is that
    /// each axis gets its own key — one key used to carry all three.
    #[test]
    fn each_axis_marks_its_own_extension_key() {
        use crate::application::ports::scope_ports::{Condition, MinRole};
        use crate::application::services::scope_table::{PolicyRegistry, Site};

        let mut registry = PolicyRegistry::new();
        registry.declare(
            Site::RestPrefix("/api/admin"),
            vec![Condition::role(MinRole::Admin)],
        );
        registry.declare(
            Site::Exact(crate::application::ports::scope_ports::Op::Rest(
                "GET",
                "/api/files/{id}",
            )),
            vec![Condition::visitor_allowed()],
        );

        let mut openapi = ApiDoc::openapi();
        mark_policies(&mut openapi, &registry);
        let spec = serde_json::to_value(openapi).expect("spec serializes");

        let admin = &spec["paths"]["/api/admin/users"]["get"];
        assert_eq!(admin[MINROLE_EXTENSION], "admin", "a role floor is a ROLE");
        assert!(
            admin.get(PRINCIPAL_EXTENSION).is_none(),
            "an admin route states no principal — the default is implied"
        );

        let shared = &spec["paths"]["/api/files/{id}"]["get"];
        assert_eq!(
            shared[PRINCIPAL_EXTENSION], "visitor",
            "visitor-reachable is the PRINCIPAL axis, never a scope"
        );
        assert!(
            shared.get(MINROLE_EXTENSION).is_none(),
            "a visitor route states no role floor"
        );

        // A PUBLIC operation drops the bearer requirement, which is the
        // only honest way to document one: the spec claims a bearer on
        // `/api/auth/login` today purely because nothing declares it
        // public, so its compiled policy is the default.
        let mut public_reg = PolicyRegistry::new();
        public_reg.declare(
            Site::Exact(crate::application::ports::scope_ports::Op::Rest(
                "POST",
                "/api/auth/login",
            )),
            vec![Condition::public()],
        );
        let mut public_spec = ApiDoc::openapi();
        mark_policies(&mut public_spec, &public_reg);
        let public_spec = serde_json::to_value(public_spec).expect("serializes");
        let login = &public_spec["paths"]["/api/auth/login"]["post"];
        assert_eq!(
            login["security"],
            serde_json::json!([]),
            "an empty security array removes the bearer requirement; \
             `[{{}}]` would mean auth is merely OPTIONAL"
        );
        assert_eq!(login[PRINCIPAL_EXTENSION], "public");

        // And the sibling verb on that same path is untouched, which a
        // path-level marker could not express.
        let deleted = &spec["paths"]["/api/files/{id}"]["delete"];
        assert!(
            deleted.get(PRINCIPAL_EXTENSION).is_none(),
            "DELETE on an allowlisted path must NOT be marked visitor"
        );
    }
}
