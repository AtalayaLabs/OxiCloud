//! Builders that synthesize `FolderDto` / `FileDto` from a provider [`MountStat`].
//!
//! Mount entries have no `storage.folders`/`storage.files` row, so the normal
//! `FolderDto::from(Folder)` path doesn't apply. These helpers produce the same
//! DTO shape from a provider stat plus the mount config, with a synthetic `ext:`
//! id and a virtual etag. Shared by the folder/file services and the handlers.

use std::sync::Arc;

use uuid::Uuid;

use crate::application::dtos::display_helpers::{
    category_for, format_file_size, icon_class_for, icon_special_class_for,
};
use crate::application::dtos::file_dto::FileDto;
use crate::application::dtos::folder_dto::FolderDto;
use crate::application::ports::external_mount_ports::{MountEntry, MountStat};
use crate::application::services::mount_registry::MountConfig;
use crate::domain::services::external_mount_id::{
    encode_child_id, virtual_file_etag, virtual_folder_etag,
};

/// Final path segment of a node id (the display name).
fn node_name(node_id: &str) -> &str {
    node_id.rsplit('/').next().unwrap_or(node_id)
}

/// Emit the structured audit line for a mount mutation (per AGENTS.md). Every
/// write op (upload / mkdir / rename / delete / move) calls this.
pub fn audit_mount_write(action: &str, cfg: &MountConfig, caller_id: Uuid, node_id: &str) {
    tracing::info!(
        target: "audit",
        event = "external_mount.write",
        action,
        mount_id = %cfg.mount_id,
        caller_id = %caller_id,
        node_id = %node_id,
        reason = "external_mount_op",
        "👮🏻‍♂️ external mount mutation",
    );
}

/// The id-string of a mount entry's parent: the parent's `ext:` id, or the
/// mount-root folder UUID when the entry is a direct child of the root.
pub fn mount_parent_id(cfg: &MountConfig, node_id: &str) -> String {
    match node_id.rsplit_once('/') {
        Some((parent, _)) => encode_child_id(cfg.mount_id, parent),
        None => cfg.mount_id.to_string(),
    }
}

/// Build a `FolderDto` for a mount directory from its stat. `parent_id` is the
/// id-string of the containing directory (mount-root UUID or an `ext:` id).
pub fn mount_folder_dto(cfg: &MountConfig, parent_id: &str, stat: &MountStat) -> FolderDto {
    FolderDto {
        etag: virtual_folder_etag(stat.modified_at),
        id: encode_child_id(cfg.mount_id, stat.node_id.clone()),
        name: node_name(stat.node_id.as_str()).to_owned(),
        path: String::new(),
        parent_id: Some(parent_id.to_owned()),
        drive_id: cfg.drive_id,
        created_at: stat.created_at,
        modified_at: stat.modified_at,
        is_root: false,
        icon_class: Arc::from("fas fa-folder"),
        icon_special_class: Arc::from("folder-icon"),
        category: Arc::from("Folder"),
        created_by: Some(cfg.owner_id),
        updated_by: Some(cfg.owner_id),
        // Mount entries carry synthetic `ext:*` ids that don't exist in
        // `auth.user_favorites` or `storage.role_grants`, so both flags
        // are always false — they can't be favorited or grant-listed.
        is_favorite: false,
        is_shared: false,
        mount: None,
    }
}

/// Build a `FolderDto` from a directory listing entry. `parent_id` is the
/// id-string of the directory being listed.
pub fn mount_entry_folder_dto(cfg: &MountConfig, parent_id: &str, entry: &MountEntry) -> FolderDto {
    FolderDto {
        etag: virtual_folder_etag(entry.modified_at),
        id: encode_child_id(cfg.mount_id, entry.node_id.clone()),
        name: entry.name.clone(),
        path: String::new(),
        parent_id: Some(parent_id.to_owned()),
        drive_id: cfg.drive_id,
        created_at: entry.created_at,
        modified_at: entry.modified_at,
        is_root: false,
        icon_class: Arc::from("fas fa-folder"),
        icon_special_class: Arc::from("folder-icon"),
        category: Arc::from("Folder"),
        created_by: Some(cfg.owner_id),
        updated_by: Some(cfg.owner_id),
        is_favorite: false,
        is_shared: false,
        mount: None,
    }
}

/// Build a `FileDto` from a directory listing entry (mime sniffed from name).
pub fn mount_entry_file_dto(cfg: &MountConfig, parent_id: &str, entry: &MountEntry) -> FileDto {
    let name = entry.name.as_str();
    let mime = mime_guess::from_path(name)
        .first_or_octet_stream()
        .to_string();
    FileDto {
        id: encode_child_id(cfg.mount_id, entry.node_id.clone()),
        name: name.to_owned(),
        path: String::new(),
        size: entry.size,
        mime_type: Arc::from(mime.as_str()),
        folder_id: Some(parent_id.to_owned()),
        created_at: entry.created_at,
        modified_at: entry.modified_at,
        icon_class: Arc::from(icon_class_for(name, &mime)),
        icon_special_class: Arc::from(icon_special_class_for(name, &mime)),
        category: Arc::from(category_for(name, &mime)),
        size_formatted: format_file_size(entry.size),
        sort_date: None,
        content_hash: String::new(),
        etag: virtual_file_etag(entry.size, entry.modified_at),
        created_by: Some(cfg.owner_id),
        updated_by: Some(cfg.owner_id),
        is_favorite: false,
        is_shared: false,
    }
}

/// Build a `FileDto` for a mount file from its stat. Virtual files have no blob
/// hash (`content_hash` empty) and a size+mtime etag.
pub fn mount_file_dto(cfg: &MountConfig, parent_id: &str, stat: &MountStat) -> FileDto {
    let name = node_name(stat.node_id.as_str());
    let mime = stat.mime_type.as_str();
    FileDto {
        id: encode_child_id(cfg.mount_id, stat.node_id.clone()),
        name: name.to_owned(),
        path: String::new(),
        size: stat.size,
        mime_type: Arc::from(mime),
        folder_id: Some(parent_id.to_owned()),
        created_at: stat.created_at,
        modified_at: stat.modified_at,
        icon_class: Arc::from(icon_class_for(name, mime)),
        icon_special_class: Arc::from(icon_special_class_for(name, mime)),
        category: Arc::from(category_for(name, mime)),
        size_formatted: format_file_size(stat.size),
        sort_date: None,
        content_hash: String::new(),
        etag: virtual_file_etag(stat.size, stat.modified_at),
        created_by: Some(cfg.owner_id),
        updated_by: Some(cfg.owner_id),
        is_favorite: false,
        is_shared: false,
    }
}
