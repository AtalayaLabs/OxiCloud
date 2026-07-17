//! Application-layer orchestration for WebDAV `sync-collection` REPORT
//! (RFC 6578) — resolves the durable change log
//! (`FolderSyncChangeRepository`) into `SyncDelta<WebdavSyncMember>`,
//! enforcing the same authz gate every other `_with_perms` method uses
//! before touching the collection.
//!
//! Kept as its own service (rather than folded into `FolderService`/
//! `FileRetrievalService`) because it composes both — a WebDAV collection
//! mixes file and folder members — without changing either service's
//! constructor or existing call sites.

use std::sync::Arc;

use uuid::Uuid;

use crate::application::dtos::file_dto::FileDto;
use crate::application::dtos::folder_dto::FolderDto;
use crate::application::ports::authorization_ports::AuthorizationEngine;
use crate::application::ports::change_log_port::{SyncChange, SyncDelta};
use crate::application::ports::storage_ports::FileReadPort;
use crate::common::errors::DomainError;
use crate::domain::entities::sync_token::SyncToken;
use crate::domain::repositories::folder_repository::FolderRepository;
use crate::domain::repositories::folder_sync_change_repository::{
    FolderSyncChangeRepository, SyncChangeKind, SyncMemberType,
};
use crate::domain::services::authorization::{Permission, Resource, Subject};
use crate::infrastructure::repositories::pg::file_blob_read_repository::FileBlobReadRepository;
use crate::infrastructure::repositories::pg::folder_db_repository::FolderDbRepository;
use crate::infrastructure::repositories::pg::folder_sync_change_pg_repository::FolderSyncChangePgRepository;
use crate::infrastructure::services::pg_acl_engine::PgAclEngine;

/// A WebDAV collection member as it should be rendered in a
/// `sync-collection` response — a folder-membership change log mixes
/// both resource types, unlike CalDAV/CardDAV's homogeneous collections.
#[derive(Debug, Clone)]
pub enum WebdavSyncMember {
    Folder(FolderDto),
    File(FileDto),
}

pub struct WebdavSyncCollectionService {
    change_log: Arc<FolderSyncChangePgRepository>,
    folder_storage: Arc<FolderDbRepository>,
    file_read: Arc<FileBlobReadRepository>,
    authz: Arc<PgAclEngine>,
}

impl WebdavSyncCollectionService {
    pub fn new(
        change_log: Arc<FolderSyncChangePgRepository>,
        folder_storage: Arc<FolderDbRepository>,
        file_read: Arc<FileBlobReadRepository>,
        authz: Arc<PgAclEngine>,
    ) -> Self {
        Self {
            change_log,
            folder_storage,
            file_read,
            authz,
        }
    }

    /// Mints the token an **initial** sync response should hand back
    /// (empty/absent client `sync-token` — the caller renders a full
    /// listing itself and just needs a cursor for the client's *next*
    /// poll). Cheaper than routing through `list_changes_with_perms`,
    /// which would also walk the collection's full change history only
    /// to discard it.
    pub async fn mint_initial_token(
        &self,
        collection_folder_id: Uuid,
        caller_id: Uuid,
    ) -> Result<SyncToken, DomainError> {
        self.authz
            .require(
                Subject::User(caller_id),
                Permission::Read,
                Resource::Folder(collection_folder_id),
            )
            .await?;

        let seq = self.change_log.current_seq(collection_folder_id).await?;
        Ok(SyncToken::mint(collection_folder_id, seq))
    }

    /// Resolves the delta for `collection_folder_id` since `since_token`
    /// (`None` for an initial sync — still walks the log with `since = 0`
    /// so `new_token` is correctly minted, but callers doing a true
    /// initial sync should render a full listing instead of this delta,
    /// matching RFC 6578 §3.7's "empty sync-token" semantics).
    ///
    /// Returns `Err(ErrorKind::SyncTokenExpired)` (→ HTTP 507) when
    /// `since_token` predates the retention watermark — the caller must
    /// discard local state and restart with a fresh initial sync.
    pub async fn list_changes_with_perms(
        &self,
        collection_folder_id: Uuid,
        since_token: Option<SyncToken>,
        caller_id: Uuid,
    ) -> Result<SyncDelta<WebdavSyncMember>, DomainError> {
        self.authz
            .require(
                Subject::User(caller_id),
                Permission::Read,
                Resource::Folder(collection_folder_id),
            )
            .await?;

        if let Some(token) = since_token
            && self.change_log.is_seq_expired(token.seq()).await?
        {
            return Err(DomainError::sync_token_expired(
                "FolderSyncChange",
                format!(
                    "sync-token seq {} for collection {} predates the retention window",
                    token.seq(),
                    collection_folder_id
                ),
            ));
        }

        let since_seq = since_token.map(|t| t.seq());
        let (rows, new_seq) = self
            .change_log
            .changes_since(collection_folder_id, since_seq)
            .await?;

        let mut changes = Vec::with_capacity(rows.len());
        for row in rows {
            match row.kind {
                SyncChangeKind::Deleted => {
                    changes.push(SyncChange::Deleted {
                        member_id: row.member_id,
                        href_hint: row.href_name,
                        is_collection: matches!(row.member_type, SyncMemberType::Folder),
                    });
                }
                SyncChangeKind::Created | SyncChangeKind::Updated => {
                    let member_id = row.member_id.to_string();
                    // The member may have been hard-deleted/purged after
                    // this row was logged but before we resolved it — the
                    // deleted-branch will have (or will soon) log its own
                    // tombstone; degrade this row to a Deleted entry with
                    // the last-known name rather than surfacing a lookup
                    // error to the client.
                    let resolved = match row.member_type {
                        SyncMemberType::Folder => self
                            .folder_storage
                            .get_folder(&member_id)
                            .await
                            .map(|f| WebdavSyncMember::Folder(FolderDto::from(f)))
                            .ok(),
                        SyncMemberType::File => self
                            .file_read
                            .get_file(&member_id)
                            .await
                            .map(|f| WebdavSyncMember::File(FileDto::from(f)))
                            .ok(),
                    };
                    match resolved {
                        Some(member) => changes.push(SyncChange::Upserted(member)),
                        None => changes.push(SyncChange::Deleted {
                            member_id: row.member_id,
                            is_collection: matches!(row.member_type, SyncMemberType::Folder),
                            href_hint: row.href_name,
                        }),
                    }
                }
            }
        }

        Ok(SyncDelta {
            changes,
            new_token: SyncToken::mint(collection_folder_id, new_seq),
        })
    }
}
