use bytes::Bytes;
use chrono::Utc;
use futures::Stream;
use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

use crate::application::dtos::trash_dto::TrashedItemDto;
use crate::application::ports::storage_ports::{FileReadPort, FileWritePort};
use crate::application::ports::trash_ports::TrashUseCase;
use crate::common::errors::{DomainError, ErrorKind, Result};
use crate::domain::entities::file::File;
use crate::domain::entities::folder::Folder;
use crate::domain::entities::trashed_item::{TrashedItem, TrashedItemType};
use crate::domain::repositories::folder_repository::FolderRepository;
use crate::domain::repositories::trash_repository::TrashRepository;
use crate::domain::services::path_service::StoragePath;

/// Test-only service that mirrors `TrashService` logic but accepts generic repos,
/// allowing mock repositories to be injected in unit tests.
#[allow(dead_code)]
struct TrashServiceForTest<TR, FR, FW, FoR> {
    trash_repository: Arc<TR>,
    file_read_port: Arc<FR>,
    file_write_port: Arc<FW>,
    folder_storage_port: Arc<FoR>,
    retention_days: u32,
}

impl<TR, FR, FW, FoR> TrashServiceForTest<TR, FR, FW, FoR>
where
    TR: TrashRepository,
    FR: FileReadPort,
    FW: FileWritePort,
    FoR: FolderRepository,
{
    #[allow(dead_code)]
    fn new(
        trash_repository: Arc<TR>,
        file_read_port: Arc<FR>,
        file_write_port: Arc<FW>,
        folder_storage_port: Arc<FoR>,
        retention_days: u32,
    ) -> Self {
        Self {
            trash_repository,
            file_read_port,
            file_write_port,
            folder_storage_port,
            retention_days,
        }
    }
}

impl<TR, FR, FW, FoR> TrashUseCase for TrashServiceForTest<TR, FR, FW, FoR>
where
    TR: TrashRepository,
    FR: FileReadPort,
    FW: FileWritePort,
    FoR: FolderRepository,
{
    async fn get_trash_items(&self, user_id: Uuid) -> Result<Vec<TrashedItemDto>> {
        use crate::application::dtos::display_helpers::{
            category_for, icon_class_for, icon_special_class_for,
        };

        let items = self.trash_repository.get_trash_items(&user_id).await?;
        Ok(items
            .into_iter()
            .map(|item| {
                let days_until_deletion = item.days_until_deletion();

                // Determine display fields based on item type
                let (category, icon_class, icon_special_class) = match item.item_type() {
                    TrashedItemType::Folder => (
                        "Folder".to_string(),
                        "fas fa-folder".to_string(),
                        "folder-icon".to_string(),
                    ),
                    TrashedItemType::File => {
                        let name = item.name();
                        let category = category_for(name, "").to_string();
                        let icon_class = icon_class_for(name, "").to_string();
                        let icon_special_class = icon_special_class_for(name, "").to_string();
                        (category, icon_class, icon_special_class)
                    }
                };

                TrashedItemDto {
                    id: item.id().to_string(),
                    original_id: item.original_id().to_string(),
                    item_type: match item.item_type() {
                        TrashedItemType::File => "file".to_string(),
                        TrashedItemType::Folder => "folder".to_string(),
                    },
                    name: item.name().to_string(),
                    original_path: item.original_path().to_string(),
                    trashed_at: item.trashed_at(),
                    days_until_deletion,
                    category,
                    icon_class,
                    icon_special_class,
                }
            })
            .collect())
    }

    async fn move_to_trash(&self, item_id: &str, item_type: &str, user_id: Uuid) -> Result<()> {
        let item_uuid = Uuid::parse_str(item_id)
            .map_err(|e| DomainError::validation_error(format!("Invalid item ID: {}", e)))?;

        match item_type {
            "file" => {
                let file = self.file_read_port.get_file(item_id).await.map_err(|e| {
                    DomainError::new(
                        ErrorKind::NotFound,
                        "File",
                        format!("Error retrieving file {}: {}", item_id, e),
                    )
                })?;
                let original_path = file.storage_path().to_string();
                let trashed_item = TrashedItem::new(
                    item_uuid,
                    user_id,
                    TrashedItemType::File,
                    file.name().to_string(),
                    original_path,
                    self.retention_days,
                );
                self.trash_repository
                    .add_to_trash(&trashed_item)
                    .await
                    .map_err(|e| {
                        DomainError::internal_error(
                            "TrashRepository",
                            format!("Failed to add file to trash: {}", e),
                        )
                    })?;
                self.file_write_port
                    .move_to_trash(item_id)
                    .await
                    .map_err(|e| {
                        DomainError::new(
                            ErrorKind::InternalError,
                            "File",
                            format!("Error moving file {} to trash: {}", item_id, e),
                        )
                    })?;
                Ok(())
            }
            "folder" => {
                let folder = self
                    .folder_storage_port
                    .get_folder(item_id)
                    .await
                    .map_err(|e| {
                        DomainError::new(
                            ErrorKind::NotFound,
                            "Folder",
                            format!("Error retrieving folder {}: {}", item_id, e),
                        )
                    })?;
                let original_path = folder.storage_path().to_string();
                let trashed_item = TrashedItem::new(
                    item_uuid,
                    user_id,
                    TrashedItemType::Folder,
                    folder.name().to_string(),
                    original_path,
                    self.retention_days,
                );
                self.trash_repository
                    .add_to_trash(&trashed_item)
                    .await
                    .map_err(|e| {
                        DomainError::internal_error(
                            "TrashRepository",
                            format!("Failed to add folder to trash: {}", e),
                        )
                    })?;
                self.folder_storage_port
                    .move_to_trash(item_id)
                    .await
                    .map_err(|e| {
                        DomainError::new(
                            ErrorKind::InternalError,
                            "Folder",
                            format!("Error moving folder {} to trash: {}", item_id, e),
                        )
                    })?;
                Ok(())
            }
            _ => Err(DomainError::validation_error(format!(
                "Invalid item type: {}",
                item_type
            ))),
        }
    }

    async fn restore_item(&self, trash_id: &str, user_id: Uuid) -> Result<()> {
        let trash_uuid = Uuid::parse_str(trash_id)
            .map_err(|e| DomainError::validation_error(format!("Invalid trash ID: {}", e)))?;

        let item = self
            .trash_repository
            .get_trash_item(&trash_uuid, &user_id)
            .await?;
        match item {
            Some(item) => {
                match item.item_type() {
                    TrashedItemType::File => {
                        let file_id = item.original_id().to_string();
                        let original_path = item.original_path().to_string();
                        let result = self
                            .file_write_port
                            .restore_from_trash(&file_id, &original_path)
                            .await;
                        if let Err(e) = result
                            && !format!("{}", e).contains("not found")
                        {
                            return Err(DomainError::new(
                                ErrorKind::InternalError,
                                "File",
                                format!("Error restoring file {} from trash: {}", file_id, e),
                            ));
                        }
                    }
                    TrashedItemType::Folder => {
                        let folder_id = item.original_id().to_string();
                        let original_path = item.original_path().to_string();
                        let result = self
                            .folder_storage_port
                            .restore_from_trash(&folder_id, &original_path)
                            .await;
                        if let Err(e) = result
                            && !format!("{}", e).contains("not found")
                        {
                            return Err(DomainError::new(
                                ErrorKind::InternalError,
                                "Folder",
                                format!("Error restoring folder {} from trash: {}", folder_id, e),
                            ));
                        }
                    }
                }
                self.trash_repository
                    .restore_from_trash(&trash_uuid, &user_id)
                    .await
                    .map_err(|e| {
                        DomainError::new(
                            ErrorKind::InternalError,
                            "Trash",
                            format!("Error removing trash entry after restoration: {}", e),
                        )
                    })?;
                Ok(())
            }
            None => Ok(()),
        }
    }

    async fn delete_permanently(&self, trash_id: &str, user_id: Uuid) -> Result<()> {
        let trash_uuid = Uuid::parse_str(trash_id)
            .map_err(|e| DomainError::validation_error(format!("Invalid trash ID: {}", e)))?;

        let item = self
            .trash_repository
            .get_trash_item(&trash_uuid, &user_id)
            .await?;
        match item {
            Some(item) => {
                match item.item_type() {
                    TrashedItemType::File => {
                        let file_id = item.original_id().to_string();
                        let result = self.file_write_port.delete_file_permanently(&file_id).await;
                        if let Err(e) = result
                            && !format!("{}", e).contains("not found")
                        {
                            return Err(DomainError::new(
                                ErrorKind::InternalError,
                                "File",
                                format!("Error deleting file {} permanently: {}", file_id, e),
                            ));
                        }
                    }
                    TrashedItemType::Folder => {
                        let folder_id = item.original_id().to_string();
                        let result = self
                            .folder_storage_port
                            .delete_folder_permanently(&folder_id)
                            .await;
                        if let Err(e) = result
                            && !format!("{}", e).contains("not found")
                        {
                            return Err(DomainError::new(
                                ErrorKind::InternalError,
                                "Folder",
                                format!("Error deleting folder {} permanently: {}", folder_id, e),
                            ));
                        }
                    }
                }
                self.trash_repository
                    .delete_permanently(&trash_uuid, &user_id)
                    .await
                    .map_err(|e| {
                        DomainError::new(
                            ErrorKind::InternalError,
                            "Trash",
                            format!("Error removing trash entry: {}", e),
                        )
                    })?;
                Ok(())
            }
            None => Ok(()),
        }
    }

    async fn empty_trash(&self, user_id: Uuid) -> Result<()> {
        self.trash_repository.clear_trash(&user_id).await
    }
}

// Mock repositories for testing
#[allow(dead_code)]
struct MockTrashRepository {
    trash_items: Mutex<HashMap<Uuid, TrashedItem>>,
    /// Shared refs to the file/folder trashed maps so `clear_trash` can
    /// simulate the PG CASCADE + trigger behaviour.
    trashed_files: Arc<Mutex<HashMap<String, File>>>,
    trashed_folders: Arc<Mutex<HashMap<String, Folder>>>,
}

impl MockTrashRepository {
    #[allow(dead_code)]
    fn new(
        trashed_files: Arc<Mutex<HashMap<String, File>>>,
        trashed_folders: Arc<Mutex<HashMap<String, Folder>>>,
    ) -> Self {
        Self {
            trash_items: Mutex::new(HashMap::new()),
            trashed_files,
            trashed_folders,
        }
    }
}

impl TrashRepository for MockTrashRepository {
    async fn add_to_trash(&self, item: &TrashedItem) -> Result<()> {
        let mut items = self.trash_items.lock().unwrap();
        items.insert(item.id(), item.clone());
        Ok(())
    }

    async fn get_trash_items(&self, user_id: &Uuid) -> Result<Vec<TrashedItem>> {
        let items = self.trash_items.lock().unwrap();
        let user_items = items
            .values()
            .filter(|item| item.user_id() == *user_id)
            .cloned()
            .collect();
        Ok(user_items)
    }

    async fn get_trash_item(&self, id: &Uuid, user_id: &Uuid) -> Result<Option<TrashedItem>> {
        let items = self.trash_items.lock().unwrap();
        let item = items
            .get(id)
            .filter(|item| item.user_id() == *user_id)
            .cloned();
        Ok(item)
    }

    async fn restore_from_trash(&self, id: &Uuid, user_id: &Uuid) -> Result<()> {
        let mut items = self.trash_items.lock().unwrap();
        if let Some(item) = items.get(id)
            && item.user_id() == *user_id
        {
            items.remove(id);
        }
        Ok(())
    }

    async fn delete_permanently(&self, id: &Uuid, user_id: &Uuid) -> Result<()> {
        let mut items = self.trash_items.lock().unwrap();
        if let Some(item) = items.get(id)
            && item.user_id() == *user_id
        {
            items.remove(id);
        }
        Ok(())
    }

    async fn clear_trash(&self, user_id: &Uuid) -> Result<()> {
        let mut items = self.trash_items.lock().unwrap();
        items.retain(|_, item| item.user_id() != *user_id);
        // Simulate PG CASCADE: clear trashed file/folder storage too
        self.trashed_files.lock().unwrap().clear();
        self.trashed_folders.lock().unwrap().clear();
        Ok(())
    }

    async fn get_all_trashed_file_ids(&self, _user_id: &Uuid) -> Result<Vec<String>> {
        let files = self.trashed_files.lock().unwrap();
        Ok(files.keys().cloned().collect())
    }

    async fn delete_expired_bulk(&self) -> Result<(u64, u64)> {
        let mut items = self.trash_items.lock().unwrap();
        let now = Utc::now();
        let before = items.len() as u64;
        items.retain(|_, item| item.deletion_date() > now);
        let deleted = before - items.len() as u64;
        Ok((deleted, 0))
    }
}

#[allow(dead_code)]
struct MockFileRepository {
    files: Mutex<HashMap<String, File>>,
    trashed_files: Arc<Mutex<HashMap<String, File>>>,
}

impl MockFileRepository {
    #[allow(dead_code)]
    fn new(trashed_files: Arc<Mutex<HashMap<String, File>>>) -> Self {
        Self {
            files: Mutex::new(HashMap::new()),
            trashed_files,
        }
    }

    #[allow(dead_code)]
    fn add_test_file(&self, id: &str, name: &str, path: &str) {
        let file = File::new(
            id.to_string(),
            name.to_string(),
            StoragePath::from_string(path),
            100,
            "text/plain".to_string(),
            None,
        )
        .unwrap();

        let mut files = self.files.lock().unwrap();
        files.insert(id.to_string(), file);
    }
}

impl FileReadPort for MockFileRepository {
    async fn get_file(&self, id: &str) -> std::result::Result<File, DomainError> {
        let files = self.files.lock().unwrap();
        if let Some(file) = files.get(id) {
            Ok(file.clone())
        } else {
            Err(DomainError::not_found("File", id.to_string()))
        }
    }

    async fn get_file_or_trashed(&self, id: &str) -> std::prelude::v1::Result<File, DomainError> {
        let files = self.files.lock().unwrap();
        if let Some(file) = files.get(id) {
            Ok(file.clone())
        } else {
            Err(DomainError::not_found("File", id.to_string()))
        }
    }
    async fn list_files(
        &self,
        _folder_id: Option<&str>,
    ) -> std::result::Result<Vec<File>, DomainError> {
        Ok(vec![])
    }

    async fn get_file_stream(
        &self,
        _id: &str,
    ) -> std::result::Result<
        Box<dyn Stream<Item = std::result::Result<Bytes, std::io::Error>> + Send>,
        DomainError,
    > {
        unimplemented!()
    }

    async fn get_file_range_stream(
        &self,
        _id: &str,
        _start: u64,
        _end: Option<u64>,
    ) -> std::result::Result<
        Box<dyn Stream<Item = std::result::Result<Bytes, std::io::Error>> + Send>,
        DomainError,
    > {
        unimplemented!()
    }

    async fn get_file_path(&self, _id: &str) -> std::result::Result<StoragePath, DomainError> {
        unimplemented!()
    }

    async fn get_parent_folder_id(&self, _path: &str) -> std::result::Result<String, DomainError> {
        unimplemented!()
    }

    async fn get_folder_id_by_path(
        &self,
        _folder_path: &str,
    ) -> std::result::Result<String, DomainError> {
        unimplemented!()
    }

    async fn get_blob_hash(&self, _file_id: &str) -> std::result::Result<String, DomainError> {
        Ok(String::new())
    }

    async fn search_files_paginated(
        &self,
        _folder_id: Option<&str>,
        _criteria: &crate::application::dtos::search_dto::SearchCriteriaDto,
        _user_id: Uuid,
    ) -> std::result::Result<(Vec<File>, usize), DomainError> {
        Ok((Vec::new(), 0))
    }

    async fn count_files(
        &self,
        _folder_id: Option<&str>,
        _criteria: &crate::application::dtos::search_dto::SearchCriteriaDto,
        _user_id: Uuid,
    ) -> std::result::Result<usize, DomainError> {
        Ok(0)
    }

    async fn stream_files_in_subtree(
        &self,
        _folder_id: &str,
    ) -> std::result::Result<
        Pin<Box<dyn Stream<Item = std::result::Result<File, DomainError>> + Send>>,
        DomainError,
    > {
        Ok(Box::pin(futures::stream::empty()))
    }

    async fn get_file_for_owner(
        &self,
        id: &str,
        _owner_id: Uuid,
    ) -> std::result::Result<File, DomainError> {
        // In this mock, ignore ownership — trash tests don't focus on ownership
        self.get_file(id).await
    }
}

impl FileWritePort for MockFileRepository {
    async fn save_file_from_temp(
        &self,
        _name: String,
        _folder_id: Option<String>,
        _content_type: String,
        _temp_path: &std::path::Path,
        _size: u64,
        _pre_computed_hash: Option<String>,
    ) -> std::result::Result<File, DomainError> {
        unimplemented!()
    }

    async fn move_file(
        &self,
        _file_id: &str,
        _target_folder_id: Option<String>,
    ) -> std::result::Result<File, DomainError> {
        unimplemented!()
    }

    async fn rename_file(
        &self,
        _file_id: &str,
        _new_name: &str,
    ) -> std::result::Result<File, DomainError> {
        unimplemented!()
    }

    async fn delete_file(&self, _id: &str) -> std::result::Result<(), DomainError> {
        Ok(())
    }

    async fn update_file_content_from_temp(
        &self,
        _file_id: &str,
        _temp_path: &std::path::Path,
        _size: u64,
        _content_type: Option<String>,
        _pre_computed_hash: Option<String>,
        _modified_at: Option<i64>,
    ) -> std::result::Result<(String, i64), DomainError> {
        Ok((String::new(), 0))
    }

    async fn register_file_deferred(
        &self,
        _name: String,
        _folder_id: Option<String>,
        _content_type: String,
        _size: u64,
    ) -> std::result::Result<(File, PathBuf), DomainError> {
        unimplemented!()
    }

    async fn copy_file(
        &self,
        _file_id: &str,
        _target_folder_id: Option<String>,
    ) -> std::result::Result<File, DomainError> {
        unimplemented!()
    }

    async fn move_to_trash(&self, id: &str) -> std::result::Result<(), DomainError> {
        let mut files = self.files.lock().unwrap();
        let mut trashed = self.trashed_files.lock().unwrap();

        if let Some(file) = files.remove(id) {
            trashed.insert(id.to_string(), file);
            Ok(())
        } else {
            Err(DomainError::not_found("File", id.to_string()))
        }
    }

    async fn restore_from_trash(
        &self,
        id: &str,
        _original_path: &str,
    ) -> std::result::Result<(), DomainError> {
        let mut files = self.files.lock().unwrap();
        let mut trashed = self.trashed_files.lock().unwrap();

        if let Some(file) = trashed.remove(id) {
            files.insert(id.to_string(), file);
            Ok(())
        } else {
            Err(DomainError::not_found(
                "File",
                format!("File {} not found in trash", id),
            ))
        }
    }

    async fn delete_file_permanently(&self, id: &str) -> std::result::Result<(), DomainError> {
        let mut trashed = self.trashed_files.lock().unwrap();
        if trashed.remove(id).is_some() {
            Ok(())
        } else {
            Err(DomainError::not_found(
                "File",
                format!("File {} not found in trash", id),
            ))
        }
    }
}

#[allow(dead_code)]
struct MockFolderRepository {
    folders: Mutex<HashMap<String, Folder>>,
    trashed_folders: Arc<Mutex<HashMap<String, Folder>>>,
}

impl MockFolderRepository {
    #[allow(dead_code)]
    fn new(trashed_folders: Arc<Mutex<HashMap<String, Folder>>>) -> Self {
        Self {
            folders: Mutex::new(HashMap::new()),
            trashed_folders,
        }
    }

    #[allow(dead_code)]
    fn add_test_folder(&self, id: &str, name: &str, path: &str) {
        let folder = Folder::new(
            id.to_string(),
            name.to_string(),
            StoragePath::from_string(path),
            None,
        )
        .unwrap();

        let mut folders = self.folders.lock().unwrap();
        folders.insert(id.to_string(), folder);
    }
}

impl FolderRepository for MockFolderRepository {
    async fn create_folder(
        &self,
        _name: String,
        _parent_id: Option<String>,
    ) -> std::result::Result<Folder, DomainError> {
        unimplemented!()
    }

    async fn get_folder(&self, id: &str) -> std::result::Result<Folder, DomainError> {
        let folders = self.folders.lock().unwrap();
        if let Some(folder) = folders.get(id) {
            Ok(folder.clone())
        } else {
            Err(DomainError::not_found("Folder", id.to_string()))
        }
    }

    async fn get_folder_by_path(
        &self,
        _storage_path: &StoragePath,
    ) -> std::result::Result<Folder, DomainError> {
        unimplemented!()
    }

    async fn list_folders(
        &self,
        _parent_id: Option<&str>,
    ) -> std::result::Result<Vec<Folder>, DomainError> {
        Ok(vec![])
    }

    async fn list_folders_by_owner(
        &self,
        _parent_id: Option<&str>,
        _owner_id: Uuid,
    ) -> std::result::Result<Vec<Folder>, DomainError> {
        Ok(vec![])
    }

    async fn list_folders_paginated(
        &self,
        _parent_id: Option<&str>,
        _offset: usize,
        _limit: usize,
        _include_total: bool,
    ) -> std::result::Result<(Vec<Folder>, Option<usize>), DomainError> {
        Ok((vec![], Some(0)))
    }

    async fn list_folders_by_owner_paginated(
        &self,
        _parent_id: Option<&str>,
        _owner_id: Uuid,
        _offset: usize,
        _limit: usize,
        _include_total: bool,
    ) -> std::result::Result<(Vec<Folder>, Option<usize>), DomainError> {
        Ok((vec![], Some(0)))
    }

    async fn rename_folder(
        &self,
        _id: &str,
        _new_name: String,
    ) -> std::result::Result<Folder, DomainError> {
        unimplemented!()
    }

    async fn move_folder(
        &self,
        _id: &str,
        _new_parent_id: Option<&str>,
    ) -> std::result::Result<Folder, DomainError> {
        unimplemented!()
    }

    async fn delete_folder(&self, _id: &str) -> std::result::Result<(), DomainError> {
        Ok(())
    }

    async fn folder_exists(
        &self,
        _storage_path: &StoragePath,
    ) -> std::result::Result<bool, DomainError> {
        Ok(false)
    }

    async fn get_folder_path(&self, _id: &str) -> std::result::Result<StoragePath, DomainError> {
        Ok(StoragePath::from_string("/"))
    }

    async fn move_to_trash(&self, id: &str) -> std::result::Result<(), DomainError> {
        let mut folders = self.folders.lock().unwrap();
        let mut trashed = self.trashed_folders.lock().unwrap();

        if let Some(folder) = folders.remove(id) {
            trashed.insert(id.to_string(), folder);
            Ok(())
        } else {
            Err(DomainError::not_found("Folder", id.to_string()))
        }
    }

    async fn restore_from_trash(
        &self,
        id: &str,
        _original_path: &str,
    ) -> std::result::Result<(), DomainError> {
        let mut folders = self.folders.lock().unwrap();
        let mut trashed = self.trashed_folders.lock().unwrap();

        if let Some(folder) = trashed.remove(id) {
            folders.insert(id.to_string(), folder);
            Ok(())
        } else {
            Err(DomainError::not_found(
                "Folder",
                format!("Folder {} not found in trash", id),
            ))
        }
    }

    async fn delete_folder_permanently(&self, id: &str) -> std::result::Result<(), DomainError> {
        let mut trashed = self.trashed_folders.lock().unwrap();
        if trashed.remove(id).is_some() {
            Ok(())
        } else {
            Err(DomainError::not_found(
                "Folder",
                format!("Folder {} not found in trash", id),
            ))
        }
    }

    async fn create_home_folder(
        &self,
        _user_id: Uuid,
        _name: String,
    ) -> std::result::Result<Folder, DomainError> {
        Ok(Folder::default())
    }
}

#[cfg(integration_tests)]
mod tests {
    #[allow(unused_imports)]
    use super::*;
    #[allow(unused_imports)]
    use crate::application::ports::trash_ports::TrashUseCase;
    #[allow(unused_imports)]
    use crate::infrastructure::repositories::pg::file_blob_read_repository::FileBlobReadRepository;
    #[allow(unused_imports)]
    use crate::infrastructure::repositories::pg::file_blob_write_repository::FileBlobWriteRepository;

    #[tokio::test]
    async fn test_move_file_to_trash() {
        // Arrange
        let trashed_files = Arc::new(Mutex::new(HashMap::new()));
        let trashed_folders = Arc::new(Mutex::new(HashMap::new()));
        let trash_repo = Arc::new(MockTrashRepository::new(
            trashed_files.clone(),
            trashed_folders.clone(),
        ));
        let file_repo = Arc::new(MockFileRepository::new(trashed_files));
        let folder_repo = Arc::new(MockFolderRepository::new(trashed_folders));

        let service = TrashServiceForTest::new(
            trash_repo.clone(),
            file_repo.clone(),
            file_repo.clone(),
            folder_repo.clone(),
            30, // 30 days retention
        );

        let file_id = "550e8400-e29b-41d4-a716-446655440000";
        let user_id = "550e8400-e29b-41d4-a716-446655440001";
        let user_uuid = Uuid::parse_str(user_id).unwrap();

        // Add a test file to the repository
        file_repo.add_test_file(file_id, "test.txt", "/test/path/test.txt");

        // Act
        let result = service.move_to_trash(file_id, "file", user_uuid).await;

        // Assert
        assert!(result.is_ok(), "Moving file to trash failed: {:?}", result);

        // Verify the file is in trash
        let trash_items = trash_repo.get_trash_items(&user_uuid).await.unwrap();

        assert_eq!(
            trash_items.len(),
            1,
            "Should have exactly one item in trash"
        );
        let trash_item = &trash_items[0];

        assert_eq!(
            trash_item.original_id().to_string(),
            file_id,
            "Original ID should match file ID"
        );
        assert_eq!(
            trash_item.user_id().to_string(),
            user_id,
            "User ID should match"
        );
        assert_eq!(
            *trash_item.item_type(),
            TrashedItemType::File,
            "Item type should be File"
        );
        assert_eq!(trash_item.name(), "test.txt", "File name should match");

        // Verify file is moved in file repository
        let files = file_repo.files.lock().unwrap();
        let trashed_files = file_repo.trashed_files.lock().unwrap();

        assert!(
            files.get(file_id).is_none(),
            "File should no longer be in main storage"
        );
        assert!(
            trashed_files.get(file_id).is_some(),
            "File should be in trash storage"
        );
    }

    #[tokio::test]
    async fn test_move_folder_to_trash() {
        // Arrange
        let trashed_files = Arc::new(Mutex::new(HashMap::new()));
        let trashed_folders = Arc::new(Mutex::new(HashMap::new()));
        let trash_repo = Arc::new(MockTrashRepository::new(
            trashed_files.clone(),
            trashed_folders.clone(),
        ));
        let file_repo = Arc::new(MockFileRepository::new(trashed_files));
        let folder_repo = Arc::new(MockFolderRepository::new(trashed_folders));

        let service = TrashServiceForTest::new(
            trash_repo.clone(),
            file_repo.clone(),
            file_repo.clone(),
            folder_repo.clone(),
            30, // 30 days retention
        );

        let folder_id = "550e8400-e29b-41d4-a716-446655440002";
        let user_id = "550e8400-e29b-41d4-a716-446655440001";
        let user_uuid = Uuid::parse_str(user_id).unwrap();

        // Add a test folder to the repository
        folder_repo.add_test_folder(folder_id, "test_folder", "/test/path/test_folder");

        // Act
        let result = service.move_to_trash(folder_id, "folder", user_uuid).await;

        // Assert
        assert!(
            result.is_ok(),
            "Moving folder to trash failed: {:?}",
            result
        );

        // Verify the folder is in trash
        let trash_items = trash_repo.get_trash_items(&user_uuid).await.unwrap();

        assert_eq!(
            trash_items.len(),
            1,
            "Should have exactly one item in trash"
        );
        let trash_item = &trash_items[0];

        assert_eq!(
            trash_item.original_id().to_string(),
            folder_id,
            "Original ID should match folder ID"
        );
        assert_eq!(
            trash_item.user_id().to_string(),
            user_id,
            "User ID should match"
        );
        assert_eq!(
            *trash_item.item_type(),
            TrashedItemType::Folder,
            "Item type should be Folder"
        );
        assert_eq!(trash_item.name(), "test_folder", "Folder name should match");
    }

    #[tokio::test]
    async fn test_restore_file_from_trash() {
        // Arrange
        let trashed_files = Arc::new(Mutex::new(HashMap::new()));
        let trashed_folders = Arc::new(Mutex::new(HashMap::new()));
        let trash_repo = Arc::new(MockTrashRepository::new(
            trashed_files.clone(),
            trashed_folders.clone(),
        ));
        let file_repo = Arc::new(MockFileRepository::new(trashed_files));
        let folder_repo = Arc::new(MockFolderRepository::new(trashed_folders));

        let service = TrashServiceForTest::new(
            trash_repo.clone(),
            file_repo.clone(),
            file_repo.clone(),
            folder_repo.clone(),
            30, // 30 days retention
        );

        let file_id = "550e8400-e29b-41d4-a716-446655440000";
        let user_id = "550e8400-e29b-41d4-a716-446655440001";
        let user_uuid = Uuid::parse_str(user_id).unwrap();
        let file_path = "/test/path/test.txt";

        // Add a test file and move it to trash
        file_repo.add_test_file(file_id, "test.txt", file_path);
        service
            .move_to_trash(file_id, "file", user_uuid)
            .await
            .unwrap();

        // Get the trash item ID
        let trash_items = trash_repo.get_trash_items(&user_uuid).await.unwrap();
        let trash_id = trash_items[0].id().to_string();

        // Act
        let result = service.restore_item(&trash_id, user_uuid).await;

        // Assert
        assert!(
            result.is_ok(),
            "Restoring file from trash failed: {:?}",
            result
        );

        // Verify the file is restored in file repository
        {
            let files = file_repo.files.lock().unwrap();
            let trashed_files = file_repo.trashed_files.lock().unwrap();

            assert!(
                files.get(file_id).is_some(),
                "File should be back in main storage"
            );
            assert!(
                trashed_files.get(file_id).is_none(),
                "File should no longer be in trash storage"
            );
        }

        // Verify the trash item is removed
        let trash_items = trash_repo.get_trash_items(&user_uuid).await.unwrap();
        assert_eq!(
            trash_items.len(),
            0,
            "Trash should be empty after restoration"
        );
    }

    #[tokio::test]
    async fn test_delete_permanently() {
        // Arrange
        let trashed_files = Arc::new(Mutex::new(HashMap::new()));
        let trashed_folders = Arc::new(Mutex::new(HashMap::new()));
        let trash_repo = Arc::new(MockTrashRepository::new(
            trashed_files.clone(),
            trashed_folders.clone(),
        ));
        let file_repo = Arc::new(MockFileRepository::new(trashed_files));
        let folder_repo = Arc::new(MockFolderRepository::new(trashed_folders));

        let service = TrashServiceForTest::new(
            trash_repo.clone(),
            file_repo.clone(),
            file_repo.clone(),
            folder_repo.clone(),
            30, // 30 days retention
        );

        let file_id = "550e8400-e29b-41d4-a716-446655440000";
        let user_id = "550e8400-e29b-41d4-a716-446655440001";
        let user_uuid = Uuid::parse_str(user_id).unwrap();

        // Add a test file and move it to trash
        file_repo.add_test_file(file_id, "test.txt", "/test/path/test.txt");
        service
            .move_to_trash(file_id, "file", user_uuid)
            .await
            .unwrap();

        // Get the trash item ID
        let trash_items = trash_repo.get_trash_items(&user_uuid).await.unwrap();
        let trash_id = trash_items[0].id().to_string();

        // Act
        let result = service.delete_permanently(&trash_id, user_uuid).await;

        // Assert
        assert!(
            result.is_ok(),
            "Deleting file permanently failed: {:?}",
            result
        );

        // Verify the file is permanently deleted
        {
            let files = file_repo.files.lock().unwrap();
            let trashed_files = file_repo.trashed_files.lock().unwrap();

            assert!(
                files.get(file_id).is_none(),
                "File should not be in main storage"
            );
            assert!(
                trashed_files.get(file_id).is_none(),
                "File should not be in trash storage"
            );
        }

        // Verify the trash item is removed
        let trash_items = trash_repo.get_trash_items(&user_uuid).await.unwrap();
        assert_eq!(
            trash_items.len(),
            0,
            "Trash should be empty after permanent deletion"
        );
    }

    #[tokio::test]
    async fn test_empty_trash() {
        // Arrange
        let trashed_files = Arc::new(Mutex::new(HashMap::new()));
        let trashed_folders = Arc::new(Mutex::new(HashMap::new()));
        let trash_repo = Arc::new(MockTrashRepository::new(
            trashed_files.clone(),
            trashed_folders.clone(),
        ));
        let file_repo = Arc::new(MockFileRepository::new(trashed_files));
        let folder_repo = Arc::new(MockFolderRepository::new(trashed_folders));

        let service = TrashServiceForTest::new(
            trash_repo.clone(),
            file_repo.clone(),
            file_repo.clone(),
            folder_repo.clone(),
            30, // 30 days retention
        );

        let user_id = "550e8400-e29b-41d4-a716-446655440001";
        let user_uuid = Uuid::parse_str(user_id).unwrap();

        // Add multiple files and folders to trash
        let file_ids = [
            "550e8400-e29b-41d4-a716-446655440010",
            "550e8400-e29b-41d4-a716-446655440011",
        ];

        let folder_ids = [
            "550e8400-e29b-41d4-a716-446655440020",
            "550e8400-e29b-41d4-a716-446655440021",
        ];

        // Add test files and folders
        for (i, file_id) in file_ids.iter().enumerate() {
            file_repo.add_test_file(
                file_id,
                &format!("test{}.txt", i),
                &format!("/test/path/test{}.txt", i),
            );
            service
                .move_to_trash(file_id, "file", user_uuid)
                .await
                .unwrap();
        }

        for (i, folder_id) in folder_ids.iter().enumerate() {
            folder_repo.add_test_folder(
                folder_id,
                &format!("folder{}", i),
                &format!("/test/path/folder{}", i),
            );
            service
                .move_to_trash(folder_id, "folder", user_uuid)
                .await
                .unwrap();
        }

        // Verify items are in trash
        let trash_items = trash_repo.get_trash_items(&user_uuid).await.unwrap();
        assert_eq!(trash_items.len(), 4, "Should have 4 items in trash");

        // Act
        let result = service.empty_trash(user_uuid).await;

        // Assert
        assert!(result.is_ok(), "Emptying trash failed: {:?}", result);

        // Verify all items are permanently deleted
        for file_id in &file_ids {
            let files = file_repo.files.lock().unwrap();
            let trashed_files = file_repo.trashed_files.lock().unwrap();
            assert!(
                files.get(*file_id).is_none(),
                "File should not be in main storage"
            );
            assert!(
                trashed_files.get(*file_id).is_none(),
                "File should not be in trash storage"
            );
        }

        for folder_id in &folder_ids {
            let folders = folder_repo.folders.lock().unwrap();
            let trashed_folders = folder_repo.trashed_folders.lock().unwrap();
            assert!(
                folders.get(*folder_id).is_none(),
                "Folder should not be in main storage"
            );
            assert!(
                trashed_folders.get(*folder_id).is_none(),
                "Folder should not be in trash storage"
            );
        }

        // Verify the trash is empty
        let trash_items = trash_repo.get_trash_items(&user_uuid).await.unwrap();
        assert_eq!(trash_items.len(), 0, "Trash should be empty after emptying");
    }
}
