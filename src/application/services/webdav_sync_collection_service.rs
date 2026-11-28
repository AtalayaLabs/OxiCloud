//! Application-layer orchestration for WebDAV `sync-collection` REPORT
//! (RFC 6578) — wraps `SyncCollectionEngine` over the
//! `storage.folder_sync_changes` log and owns the whole per-request flow
//! (initial listing or delta), so the plain-WebDAV and
//! NextCloud handlers only resolve the path and render hrefs.
//!
//! Kept as its own service (rather than folded into `FolderService`/
//! `FileRetrievalService`) because it composes both — a WebDAV collection
//! mixes file and folder members — without changing either service's
//! constructor or existing call sites.

use std::collections::HashMap;
use std::sync::Arc;

use uuid::Uuid;

use crate::application::dtos::file_dto::FileDto;
use crate::application::dtos::folder_dto::FolderDto;
use crate::application::ports::authorization_ports::AuthorizationEngine;
use crate::application::ports::change_log_port::SyncChange;
use crate::application::ports::file_ports::FileRetrievalUseCase;
use crate::application::ports::folder_ports::FolderUseCase;
use crate::application::services::file_retrieval_service::FileRetrievalService;
use crate::application::services::folder_service::FolderService;
use crate::application::services::sync_collection_engine::{
    SyncCollectionEngine, SyncMemberResolver,
};
use crate::common::errors::DomainError;
use crate::domain::entities::sync_token::SyncToken;
use crate::domain::repositories::sync_change_log_repository::{
    SyncChangeLogRepository, SyncChangeRow,
};
use crate::domain::services::authorization::Resource;
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

/// A member removed since the client's token, rendered as an RFC 6578
/// §3.7 404 sub-response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletedMember {
    /// Last-known leaf name, unencoded, no trailing slash.
    pub name: String,
    /// A subfolder: its href takes a trailing slash.
    pub is_collection: bool,
}

/// What a folder `sync-collection` REPORT renders: the live members (all
/// of them on an initial sync, only the changed ones on a delta), the
/// removed ones, and the token for the client's next poll.
#[derive(Debug, Clone)]
pub struct FolderSyncView {
    pub subfolders: Vec<FolderDto>,
    pub files: Vec<FileDto>,
    pub deleted: Vec<DeletedMember>,
    pub token: SyncToken,
}

/// Reads a WebDAV collection's members, both ways a sync needs them: by id
/// for a delta's changed rows, and as a full listing for an initial sync.
pub trait WebdavMemberSource: SyncMemberResolver<Member = WebdavSyncMember> + Clone {
    /// The folder's direct subfolders and files, permission-filtered for
    /// `caller_id`.
    async fn list_members(
        &self,
        folder_id: &str,
        caller_id: Uuid,
    ) -> Result<(Vec<FolderDto>, Vec<FileDto>), DomainError>;
}

/// Production member source over the PG repositories and the listing
/// services. Resolves a delta with two batch queries, one per member type;
/// trashed members drop out of both (`NOT is_trashed`), so a member trashed
/// after its row was logged is reported as deleted.
#[derive(Clone)]
pub struct PgWebdavMembers {
    folder_storage: Arc<FolderDbRepository>,
    file_read: Arc<FileBlobReadRepository>,
    folder_service: Arc<FolderService>,
    file_retrieval: Arc<FileRetrievalService>,
}

impl PgWebdavMembers {
    pub fn new(
        folder_storage: Arc<FolderDbRepository>,
        file_read: Arc<FileBlobReadRepository>,
        folder_service: Arc<FolderService>,
        file_retrieval: Arc<FileRetrievalService>,
    ) -> Self {
        Self {
            folder_storage,
            file_read,
            folder_service,
            file_retrieval,
        }
    }
}

impl SyncMemberResolver for PgWebdavMembers {
    type Member = WebdavSyncMember;

    async fn resolve_batch(
        &self,
        _collection_id: Uuid,
        rows: &[&SyncChangeRow],
    ) -> Result<HashMap<Uuid, WebdavSyncMember>, DomainError> {
        let (folder_rows, file_rows): (Vec<_>, Vec<_>) = rows.iter().partition(|r| r.is_collection);
        let ids = |rows: Vec<&&SyncChangeRow>| -> Vec<String> {
            rows.iter().map(|r| r.member_id.to_string()).collect()
        };
        let (folder_ids, file_ids) = (ids(folder_rows), ids(file_rows));

        let mut members = HashMap::with_capacity(rows.len());
        if !folder_ids.is_empty() {
            for folder in self.folder_storage.get_folders_by_ids(&folder_ids).await? {
                let dto = FolderDto::from(folder);
                members.insert(dto.id.parse()?, WebdavSyncMember::Folder(dto));
            }
        }
        if !file_ids.is_empty() {
            for file in self.file_read.get_files_by_ids(&file_ids).await? {
                let dto = FileDto::from(file);
                members.insert(dto.id.parse()?, WebdavSyncMember::File(dto));
            }
        }
        Ok(members)
    }
}

impl WebdavMemberSource for PgWebdavMembers {
    async fn list_members(
        &self,
        folder_id: &str,
        caller_id: Uuid,
    ) -> Result<(Vec<FolderDto>, Vec<FileDto>), DomainError> {
        let subfolders = self
            .folder_service
            .list_folders_with_perms(Some(folder_id), caller_id)
            .await?;
        let files = self
            .file_retrieval
            .list_files_with_perms(Some(folder_id), caller_id)
            .await?;
        Ok((subfolders, files))
    }
}

pub struct WebdavSyncCollectionService<
    L = FolderSyncChangePgRepository,
    S = PgWebdavMembers,
    A = PgAclEngine,
> where
    L: SyncChangeLogRepository,
    S: WebdavMemberSource,
    A: AuthorizationEngine,
{
    engine: SyncCollectionEngine<L, S, A>,
    members: S,
}

impl<L, S, A> WebdavSyncCollectionService<L, S, A>
where
    L: SyncChangeLogRepository,
    S: WebdavMemberSource,
    A: AuthorizationEngine,
{
    pub fn new(change_log: Arc<L>, members: S, authz: Arc<A>) -> Self {
        Self {
            engine: SyncCollectionEngine::new(
                change_log,
                members.clone(),
                authz,
                Resource::Folder,
                // The leaf name is the href segment as-is; the handler
                // percent-encodes it and appends `/` for collections.
                |label| label.to_owned(),
                "FolderSyncChange",
            ),
            members,
        }
    }

    /// Answers one folder `sync-collection` REPORT at sync-level 1 (the
    /// handler has already enforced RFC 6578's Depth and sync-level rules).
    ///
    /// - No `since_token`: an initial sync — every current member
    ///   (RFC 6578 §3.4).
    /// - A `since_token`: only what changed since it, or
    ///   `SyncTokenExpired` if the log can no longer answer it (see
    ///   `SyncCollectionEngine::list_changes_with_perms`).
    ///
    /// Authz is the engine's Read check on the folder, plus the listing
    /// services' own checks on an initial sync.
    pub async fn sync_folder_with_perms(
        &self,
        folder_id: Uuid,
        since_token: Option<SyncToken>,
        caller_id: Uuid,
    ) -> Result<FolderSyncView, DomainError> {
        let Some(since) = since_token else {
            // Mint BEFORE listing. A change landing between the two is then
            // both in the listing and after the token, and the next poll
            // replays it — harmless. The other order covers that change
            // with the token while leaving it out of the listing, and the
            // client never sees it.
            let token = self.engine.mint_initial_token(folder_id, caller_id).await?;
            let (subfolders, files) = self
                .members
                .list_members(&folder_id.to_string(), caller_id)
                .await?;
            return Ok(FolderSyncView {
                subfolders,
                files,
                deleted: Vec::new(),
                token,
            });
        };

        let delta = self
            .engine
            .list_changes_with_perms(folder_id, Some(since), caller_id)
            .await?;
        let mut view = FolderSyncView::empty(delta.new_token);
        for change in delta.changes {
            match change {
                SyncChange::Upserted(WebdavSyncMember::Folder(f)) => view.subfolders.push(f),
                SyncChange::Upserted(WebdavSyncMember::File(f)) => view.files.push(f),
                SyncChange::Deleted {
                    href_hint,
                    is_collection,
                    ..
                } => view.deleted.push(DeletedMember {
                    name: href_hint,
                    is_collection,
                }),
            }
        }
        Ok(view)
    }
}

impl FolderSyncView {
    fn empty(token: SyncToken) -> Self {
        Self {
            subfolders: Vec::new(),
            files: Vec::new(),
            deleted: Vec::new(),
            token,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::ports::authorization_ports::test_support::FakeEngine;
    use crate::domain::entities::sync_token::SyncCursor;
    use crate::domain::repositories::sync_change_log_repository::SyncChangeKind;
    use crate::domain::services::authorization::Subject;
    use chrono::{DateTime, Utc};
    use std::sync::Mutex;

    const HEAD: SyncCursor = SyncCursor {
        seq: 5,
        xmin: 3,
        xmax: 4,
        xip: Vec::new(),
    };

    /// Shared call log, so the test can see the order of side effects.
    type Calls = Arc<Mutex<Vec<&'static str>>>;

    struct Log {
        rows: Vec<SyncChangeRow>,
        calls: Calls,
    }

    impl SyncChangeLogRepository for Log {
        async fn changes_since(
            &self,
            _: Uuid,
            _: Option<&SyncCursor>,
        ) -> Result<(Vec<SyncChangeRow>, SyncCursor), DomainError> {
            self.calls.lock().unwrap().push("changes_since");
            Ok((self.rows.clone(), HEAD))
        }
        async fn is_cursor_expired(&self, _: Uuid, _: &SyncCursor) -> Result<bool, DomainError> {
            Ok(false)
        }
        async fn current_cursor(&self, _: Uuid) -> Result<SyncCursor, DomainError> {
            self.calls.lock().unwrap().push("mint");
            Ok(HEAD)
        }
        async fn delete_expired_before(&self, _: DateTime<Utc>) -> Result<u64, DomainError> {
            unimplemented!()
        }
        async fn enforce_row_cap(&self, _: u32) -> Result<u64, DomainError> {
            unimplemented!()
        }
    }

    /// A folder whose members are all gone: listing is empty and no
    /// changed row resolves.
    #[derive(Clone)]
    struct EmptyFolder(Calls);

    impl SyncMemberResolver for EmptyFolder {
        type Member = WebdavSyncMember;
        async fn resolve_batch(
            &self,
            _: Uuid,
            _: &[&SyncChangeRow],
        ) -> Result<HashMap<Uuid, WebdavSyncMember>, DomainError> {
            self.0.lock().unwrap().push("resolve");
            Ok(HashMap::new())
        }
    }

    impl WebdavMemberSource for EmptyFolder {
        async fn list_members(
            &self,
            _: &str,
            _: Uuid,
        ) -> Result<(Vec<FolderDto>, Vec<FileDto>), DomainError> {
            self.0.lock().unwrap().push("list");
            Ok((Vec::new(), Vec::new()))
        }
    }

    fn service(
        rows: Vec<SyncChangeRow>,
        caller: Uuid,
    ) -> (
        WebdavSyncCollectionService<Log, EmptyFolder, FakeEngine>,
        Calls,
    ) {
        let calls = Calls::default();
        let svc = WebdavSyncCollectionService::new(
            Arc::new(Log {
                rows,
                calls: calls.clone(),
            }),
            EmptyFolder(calls.clone()),
            Arc::new(FakeEngine::granting(vec![Subject::User(caller)])),
        );
        (svc, calls)
    }

    fn row(name: &str, kind: SyncChangeKind, is_collection: bool) -> SyncChangeRow {
        SyncChangeRow {
            member_id: Uuid::new_v4(),
            label: name.to_owned(),
            kind,
            is_collection,
        }
    }

    /// Regression: both sync-collection handlers listed the folder and
    /// minted the token afterwards. A change landing in between was covered
    /// by the token but missing from the listing, so the client never saw
    /// it. The token must be minted before the listing is read.
    #[tokio::test]
    async fn an_initial_sync_mints_the_token_before_listing() {
        let (caller, folder) = (Uuid::new_v4(), Uuid::new_v4());
        let (svc, calls) = service(Vec::new(), caller);
        svc.sync_folder_with_perms(folder, None, caller)
            .await
            .unwrap();
        assert_eq!(*calls.lock().unwrap(), vec!["mint", "list"]);
    }

    /// A delta's removals keep their name and collection flag, including a
    /// changed row whose member vanished before it could be resolved.
    #[tokio::test]
    async fn a_delta_reports_removed_members_with_their_kind() {
        let (caller, folder) = (Uuid::new_v4(), Uuid::new_v4());
        let (svc, _) = service(
            vec![
                row("old-dir", SyncChangeKind::Deleted, true),
                row("vanished.txt", SyncChangeKind::Updated, false),
            ],
            caller,
        );
        let view = svc
            .sync_folder_with_perms(
                folder,
                Some(SyncToken::mint(folder, SyncCursor { seq: 1, ..HEAD })),
                caller,
            )
            .await
            .unwrap();
        assert_eq!(
            view.deleted,
            vec![
                DeletedMember {
                    name: "old-dir".to_owned(),
                    is_collection: true
                },
                DeletedMember {
                    name: "vanished.txt".to_owned(),
                    is_collection: false
                },
            ]
        );
    }
}
