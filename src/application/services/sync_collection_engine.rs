//! Shared engine behind CalDAV's and CardDAV's `sync-collection` services
//! — both resolve a homogeneous change log into `SyncDelta<M>` with
//! identical control flow (authz gate → expiry check → changes_since →
//! per-row resolve-or-degrade). WebDAV's service stays hand-rolled
//! (`WebdavSyncCollectionService`, heterogeneous folder/file members,
//! drive/path resolution) — not a fit here, per that service's doc comment.

use std::sync::Arc;

use uuid::Uuid;

use crate::application::ports::authorization_ports::AuthorizationEngine;
use crate::application::ports::change_log_port::{SyncChange, SyncDelta};
use crate::common::errors::DomainError;
use crate::domain::entities::sync_token::SyncToken;
use crate::domain::repositories::sync_change_log_repository::{
    SyncChangeKind, SyncChangeLogRepository,
};
use crate::domain::services::authorization::{Permission, Resource, Subject};
use crate::infrastructure::services::pg_acl_engine::PgAclEngine;

/// Per-domain hook resolving a `Created`/`Updated` row's member id back to
/// its current DTO. Returns `None` on any lookup miss (hard-deleted/purged
/// race, or a swallowed error) — the engine degrades that to a `Deleted`
/// entry rather than surfacing a lookup error to the client.
pub trait SyncMemberResolver: Send + Sync + 'static {
    type Member;
    async fn resolve(&self, member_id: Uuid) -> Option<Self::Member>;
}

pub struct SyncCollectionEngine<L, R>
where
    L: SyncChangeLogRepository,
    R: SyncMemberResolver,
{
    change_log: Arc<L>,
    resolver: R,
    authz: Arc<PgAclEngine>,
    /// `Resource::Calendar` / `Resource::AddressBook` are themselves valid
    /// `fn(Uuid) -> Resource` values (tuple-variant constructors) — no
    /// closure needed.
    resource_of: fn(Uuid) -> Resource,
    /// Turns a row's `label` into the `href_hint` a `Deleted` entry
    /// carries (`"{label}.ics"` / `"{label}.vcf"`).
    label_to_href_hint: fn(&str) -> String,
    /// `entity_type` passed to `DomainError::sync_token_expired`.
    error_tag: &'static str,
}

impl<L, R> SyncCollectionEngine<L, R>
where
    L: SyncChangeLogRepository,
    R: SyncMemberResolver,
{
    pub fn new(
        change_log: Arc<L>,
        resolver: R,
        authz: Arc<PgAclEngine>,
        resource_of: fn(Uuid) -> Resource,
        label_to_href_hint: fn(&str) -> String,
        error_tag: &'static str,
    ) -> Self {
        Self {
            change_log,
            resolver,
            authz,
            resource_of,
            label_to_href_hint,
            error_tag,
        }
    }

    /// Mints the token an **initial** sync response should hand back
    /// (empty/absent client `sync-token`). Cheaper than routing through
    /// `list_changes_with_perms`, which would also walk the collection's
    /// full change history only to discard it.
    pub async fn mint_initial_token(
        &self,
        collection_id: Uuid,
        caller_id: Uuid,
    ) -> Result<SyncToken, DomainError> {
        self.authz
            .require(
                Subject::User(caller_id),
                Permission::Read,
                (self.resource_of)(collection_id),
            )
            .await?;

        let seq = self.change_log.current_seq(collection_id).await?;
        Ok(SyncToken::mint(collection_id, seq))
    }

    /// Resolves the delta for `collection_id` since `since_token`.
    ///
    /// Returns `Err(ErrorKind::SyncTokenExpired)` (→ HTTP 507) when
    /// `since_token` predates the retention watermark — the caller must
    /// discard local state and restart with a fresh initial sync.
    pub async fn list_changes_with_perms(
        &self,
        collection_id: Uuid,
        since_token: Option<SyncToken>,
        caller_id: Uuid,
    ) -> Result<SyncDelta<R::Member>, DomainError> {
        self.authz
            .require(
                Subject::User(caller_id),
                Permission::Read,
                (self.resource_of)(collection_id),
            )
            .await?;

        if let Some(token) = since_token
            && self.change_log.is_seq_expired(token.seq()).await?
        {
            return Err(DomainError::sync_token_expired(
                self.error_tag,
                format!(
                    "sync-token seq {} for collection {} predates the retention window",
                    token.seq(),
                    collection_id
                ),
            ));
        }

        let since_seq = since_token.map(|t| t.seq());
        let (rows, new_seq) = self
            .change_log
            .changes_since(collection_id, since_seq)
            .await?;

        let mut changes = Vec::with_capacity(rows.len());
        for row in rows {
            match row.kind {
                SyncChangeKind::Deleted => {
                    changes.push(SyncChange::Deleted {
                        member_id: row.member_id,
                        href_hint: (self.label_to_href_hint)(&row.label),
                        is_collection: false,
                    });
                }
                SyncChangeKind::Created | SyncChangeKind::Updated => {
                    match self.resolver.resolve(row.member_id).await {
                        Some(member) => changes.push(SyncChange::Upserted(member)),
                        None => changes.push(SyncChange::Deleted {
                            member_id: row.member_id,
                            href_hint: (self.label_to_href_hint)(&row.label),
                            is_collection: false,
                        }),
                    }
                }
            }
        }

        Ok(SyncDelta {
            changes,
            new_token: SyncToken::mint(collection_id, new_seq),
        })
    }
}
