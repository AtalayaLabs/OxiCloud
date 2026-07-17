//! Application-layer orchestration for CardDAV `sync-collection` REPORT
//! (RFC 6578) — resolves the durable change log
//! (`ContactSyncChangeRepository`) into `SyncDelta<ContactDto>`, enforcing
//! the same authz gate every other `_with_perms` method uses before
//! touching the collection.
//!
//! Kept as its own service (mirroring `WebdavSyncCollectionService`)
//! rather than folded into `ContactService`, per the WebDAV
//! sync-collection structure — even though a CardDAV collection is
//! homogeneous (contacts only) like CalDAV's, this keeps the
//! sync-collection resolution path — change-log read, expiry check,
//! per-row resolve-or-degrade — isolated from `ContactService`'s
//! authz-gated use-case surface, with its own narrow constructor instead
//! of growing `ContactService::new`'s parameter list.

use std::sync::Arc;

use uuid::Uuid;

use crate::application::dtos::contact_dto::ContactDto;
use crate::application::ports::authorization_ports::AuthorizationEngine;
use crate::application::ports::change_log_port::{SyncChange, SyncDelta};
use crate::common::errors::DomainError;
use crate::domain::entities::sync_token::SyncToken;
use crate::domain::repositories::contact_repository::ContactRepository;
use crate::domain::repositories::contact_sync_change_repository::{
    ContactSyncChangeRepository, SyncChangeKind,
};
use crate::domain::services::authorization::{Permission, Resource, Subject};
use crate::infrastructure::repositories::pg::ContactPgRepository;
use crate::infrastructure::repositories::pg::contact_sync_change_pg_repository::ContactSyncChangePgRepository;
use crate::infrastructure::services::pg_acl_engine::PgAclEngine;

pub struct CarddavSyncCollectionService {
    change_log: Arc<ContactSyncChangePgRepository>,
    contact_storage: Arc<ContactPgRepository>,
    authz: Arc<PgAclEngine>,
}

impl CarddavSyncCollectionService {
    pub fn new(
        change_log: Arc<ContactSyncChangePgRepository>,
        contact_storage: Arc<ContactPgRepository>,
        authz: Arc<PgAclEngine>,
    ) -> Self {
        Self {
            change_log,
            contact_storage,
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
        collection_address_book_id: Uuid,
        caller_id: Uuid,
    ) -> Result<SyncToken, DomainError> {
        self.authz
            .require(
                Subject::User(caller_id),
                Permission::Read,
                Resource::AddressBook(collection_address_book_id),
            )
            .await?;

        let seq = self
            .change_log
            .current_seq(collection_address_book_id)
            .await?;
        Ok(SyncToken::mint(collection_address_book_id, seq))
    }

    /// Resolves the delta for `collection_address_book_id` since
    /// `since_token`. See `WebdavSyncCollectionService::list_changes_with_perms`
    /// for the shared shape and expiry semantics.
    pub async fn list_changes_with_perms(
        &self,
        collection_address_book_id: Uuid,
        since_token: Option<SyncToken>,
        caller_id: Uuid,
    ) -> Result<SyncDelta<ContactDto>, DomainError> {
        self.authz
            .require(
                Subject::User(caller_id),
                Permission::Read,
                Resource::AddressBook(collection_address_book_id),
            )
            .await?;

        if let Some(token) = since_token
            && self.change_log.is_seq_expired(token.seq()).await?
        {
            return Err(DomainError::sync_token_expired(
                "ContactSyncChange",
                format!(
                    "sync-token seq {} for address book {} predates the retention window",
                    token.seq(),
                    collection_address_book_id
                ),
            ));
        }

        let since_seq = since_token.map(|t| t.seq());
        let (rows, new_seq) = self
            .change_log
            .changes_since(collection_address_book_id, since_seq)
            .await?;

        let mut changes = Vec::with_capacity(rows.len());
        for row in rows {
            match row.kind {
                SyncChangeKind::Deleted => {
                    changes.push(SyncChange::Deleted {
                        member_id: row.member_id,
                        href_hint: format!("{}.vcf", row.uid),
                        is_collection: false,
                    });
                }
                SyncChangeKind::Created | SyncChangeKind::Updated => {
                    // The member may have been hard-deleted after this row
                    // was logged but before we resolved it — the
                    // deleted-branch will have (or will soon) log its own
                    // tombstone; degrade this row to a Deleted entry with
                    // the last-known uid rather than surfacing a lookup
                    // error to the client.
                    match self.contact_storage.get_contact_by_id(&row.member_id).await {
                        Ok(Some(contact)) => {
                            changes.push(SyncChange::Upserted(ContactDto::from(contact)));
                        }
                        _ => {
                            changes.push(SyncChange::Deleted {
                                member_id: row.member_id,
                                href_hint: format!("{}.vcf", row.uid),
                                is_collection: false,
                            });
                        }
                    }
                }
            }
        }

        Ok(SyncDelta {
            changes,
            new_token: SyncToken::mint(collection_address_book_id, new_seq),
        })
    }
}
