pub mod admin_settings_service;
pub mod app_password_service;
pub mod auth_application_service;
pub mod batch_operations;
pub mod blob_lifecycle_service;
pub mod caldav_sync_collection_service;
pub mod calendar_service;
pub mod carddav_sync_collection_service;
pub mod contact_service;
pub mod delta_upload_service;
pub mod device_auth_service;
pub mod drive_management_service;
pub mod external_identity_service;
pub mod external_mount_router;
pub mod external_upload_service;
pub mod favorites_service;
pub mod file_lifecycle_service;
pub mod file_management_service;
pub mod file_retrieval_service;
pub mod file_upload_service;
pub mod file_use_case_factory;
pub mod folder_service;
pub mod i18n_application_service;
pub mod magic_link_invite_service;
pub mod mount_dto;
pub mod mount_registry;
pub mod music_service;
pub mod nextcloud_file_id_service;
pub mod nextcloud_login_flow_service;
pub mod people_service;
pub mod places_service;
pub mod recent_service;
pub mod recipient_notification_service;
pub mod search_service;
pub mod share_browse_service;
pub mod share_service;
pub mod storage_settings_service;
pub mod storage_usage_service;
pub mod subject_group_service;
pub mod sync_collection_engine;
pub mod trash_service;
pub mod user_lifecycle_service;
pub mod webdav_sync_collection_service;
pub mod wopi_lock_service;
pub mod wopi_token_service;

#[cfg(test)]
mod batch_operations_test;
#[cfg(test)]
mod trash_service_test;

// Re-exportar para facilitar acceso
pub use file_management_service::FileManagementService;
pub use file_retrieval_service::FileRetrievalService;
pub use file_upload_service::FileUploadService;
pub use file_use_case_factory::AppFileUseCaseFactory;
