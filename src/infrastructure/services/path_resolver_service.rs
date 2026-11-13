//! Single-query WebDAV path resolver.
//!
//! Replaces the double-query pattern (`get_folder_by_path` + `get_file_by_path`)
//! with a single `UNION ALL` query that returns the first match.  PostgreSQL's
//! `Append` node short-circuits on `LIMIT 1`, so if the folder branch matches
//! the file branch is never executed.

use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

use crate::application::dtos::display_helpers::{ResourceCategory, category_for, intern_mime};
use crate::application::dtos::file_dto::FileDto;
use crate::application::dtos::folder_dto::FolderDto;
use crate::application::dtos::timestamp_helpers::epoch_to_datetime;
use crate::application::services::share_mount_service::ShareMountService;
use crate::common::errors::{DomainError, ErrorKind};
use crate::domain::entities::file::File;
use crate::domain::entities::folder::Folder;
use crate::domain::services::authorization::Subject;

/// Result of resolving a WebDAV path — either a folder or a file.
#[derive(Debug, Clone)]
pub enum ResolvedResource {
    Folder(FolderDto),
    File(FileDto),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdKind {
    Folder,
    File,
}

/// Where a folder or file id lives, without redirects or caller scoping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdAddress {
    pub kind: IdKind,
    pub drive_id: Uuid,
    /// Folder path; for a file its parent path joined with its name (name alone at the root).
    pub path: String,
    /// Folder row that mounts another folder (`mount_target_id` set).
    pub is_link: bool,
}

/// Where a DAV path lands after walking through at most one link (I3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLocation {
    pub drive_id: Uuid,
    pub db_path: String,
    /// Link row of the hop.
    pub via_link: Option<Uuid>,
    /// The path names the link row itself; `drive_id`/`db_path` are its target.
    pub at_link: bool,
}

/// Resolves a WebDAV path to a folder or file in a single SQL round-trip.
pub struct PathResolverService {
    pool: Arc<PgPool>,
    links: Option<Arc<ShareMountService>>,
}

impl PathResolverService {
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool, links: None }
    }

    pub fn with_links(mut self, links: Arc<ShareMountService>) -> Self {
        self.links = Some(links);
        self
    }

    /// Rebase `path` onto a link's target when a prefix of it is a link row (R0/R1 via
    /// `ShareMountService::resolve`; a link the caller may not see is `NotFound`).
    pub async fn locate(
        &self,
        caller: Uuid,
        path: &str,
        drive_id: Uuid,
    ) -> Result<ResolvedLocation, DomainError> {
        let unchanged = || ResolvedLocation {
            drive_id,
            db_path: path.to_owned(),
            via_link: None,
            at_link: false,
        };
        let Some(links) = &self.links else {
            return Ok(unchanged());
        };
        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if segments.len() < 2 {
            return Ok(unchanged());
        }
        let prefixes: Vec<String> = (2..=segments.len())
            .map(|n| segments[..n].join("/"))
            .collect();
        let row = sqlx::query_as::<_, (Uuid, String, String)>(
            "SELECT m.id, m.path, t.path
               FROM storage.folders m
               JOIN storage.folders t ON t.id = m.mount_target_id
              WHERE m.drive_id = $1 AND m.mount_target_id IS NOT NULL
                AND NOT m.is_trashed AND m.path = ANY($2)
              ORDER BY length(m.path) DESC
              LIMIT 1",
        )
        .bind(drive_id)
        .bind(&prefixes)
        .fetch_optional(&*self.pool)
        .await
        .map_err(|e| DomainError::internal_error("PathResolver", format!("locate: {e}")))?;
        let Some((link_id, link_path, target_path)) = row else {
            return Ok(unchanged());
        };
        let miss = || DomainError::not_found("Resource", path);
        let mount = match links
            .resolve(Subject::User(caller), &link_id.to_string())
            .await
        {
            Ok(Some(m)) => m,
            Ok(None) => return Err(miss()),
            Err(e) if e.kind == ErrorKind::NotFound => return Err(miss()),
            Err(e) => return Err(e),
        };
        let rest = segments[link_path.split('/').count()..].join("/");
        let db_path = if rest.is_empty() {
            target_path
        } else {
            format!("{target_path}/{rest}")
        };
        Ok(ResolvedLocation {
            drive_id: mount.target_drive_id,
            db_path,
            via_link: Some(link_id),
            at_link: rest.is_empty(),
        })
    }

    /// Look up a non-trashed folder or file by id in one query.
    pub async fn resolve_id(&self, id: Uuid) -> Result<Option<IdAddress>, DomainError> {
        let row = sqlx::query_as::<_, (bool, Uuid, String, bool)>(
            "(SELECT FALSE, drive_id, path, mount_target_id IS NOT NULL
                FROM storage.folders WHERE id = $1 AND NOT is_trashed)
             UNION ALL
             (SELECT TRUE, f.drive_id,
                     CASE WHEN fo.path IS NULL OR fo.path = '' THEN f.name
                          ELSE fo.path || '/' || f.name END,
                     FALSE
                FROM storage.files f
                LEFT JOIN storage.folders fo ON fo.id = f.folder_id
               WHERE f.id = $1 AND NOT f.is_trashed)
             LIMIT 1",
        )
        .bind(id)
        .fetch_optional(&*self.pool)
        .await
        .map_err(|e| DomainError::internal_error("PathResolver", format!("resolve_id: {e}")))?;
        Ok(row.map(|(is_file, drive_id, path, is_link)| IdAddress {
            kind: if is_file {
                IdKind::File
            } else {
                IdKind::Folder
            },
            drive_id,
            path,
            is_link,
        }))
    }

    /// Resolve `path` to a folder or file **within the given drive**.
    ///
    /// Filters on `fo.drive_id = $4` / `fi.drive_id = $4`. Callers
    /// pre-resolve which drive they're operating in — native WebDAV
    /// derives it from the caller's default drive
    /// (`resolve_drive_id_for_native_webdav`); NC WebDAV takes it from
    /// the URL-selected chroot (`chroot.drive_id`). Shared by both
    /// surfaces so the single-query UNION ALL optimisation lands
    /// consistently and no path lookup keys on the doomed
    /// `storage.{files,folders}.user_id` column.
    pub async fn resolve_path_in_drive(
        &self,
        path: &str,
        drive_id: Uuid,
    ) -> Result<ResolvedResource, DomainError> {
        let path = path.trim_start_matches('/').trim_end_matches('/');
        if path.is_empty() {
            return Err(DomainError::not_found("Resource", "empty path"));
        }

        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let filename = segments[segments.len() - 1];
        let folder_path = if segments.len() > 1 {
            segments[..segments.len() - 1].join("/")
        } else {
            String::new()
        };

        // Widened SELECT: also fetches `blob_hash` (for file ETag) and
        // `tree_modified_at` (for folder ETag). Both share the same
        // canonical formulas as the rest of the codebase — see
        // [`File::compute_etag`] and [`Folder::compute_etag`]. Without
        // these two extra columns the resolver used to emit empty
        // ETag strings, and NC's `If-Match` round-trips broke
        // (see the F6b regression on `test_nc_put_mkcol_blake3.sh`).
        let row = sqlx::query_as::<
            _,
            (
                String,         // resource_type
                String,         // id
                String,         // name
                String,         // path
                Option<String>, // parent_id
                Uuid,           // drive_id
                i64,            // created_at
                i64,            // content_modified_at → entity `modified_at`
                Option<i64>,    // size
                Option<String>, // mime_type
                Option<String>, // folder_id
                Option<String>, // blob_hash (files only)
                Option<i64>,    // tree_modified_at (folders only)
            ),
        >(
            r#"
            SELECT resource_type, id, name, path, parent_id, drive_id,
                   created_at, content_modified_at, size, mime_type, folder_id,
                   blob_hash, tree_modified_at
              FROM (
                SELECT 'folder'::text       AS resource_type,
                       fo.id::text,
                       fo.name,
                       fo.path,
                       fo.parent_id::text,
                       fo.drive_id,
                       EXTRACT(EPOCH FROM fo.created_at)::bigint AS created_at,
                       EXTRACT(EPOCH FROM fo.content_modified_at)::bigint AS content_modified_at,
                       NULL::bigint         AS size,
                       NULL::text           AS mime_type,
                       NULL::text           AS folder_id,
                       NULL::text           AS blob_hash,
                       EXTRACT(EPOCH FROM fo.tree_modified_at)::bigint AS tree_modified_at
                  FROM storage.folders fo
                 WHERE fo.path = $1 AND NOT fo.is_trashed
                   AND fo.drive_id = $4

                UNION ALL

                SELECT 'file'::text         AS resource_type,
                       fi.id::text,
                       fi.name,
                       CASE
                         WHEN fo.path IS NOT NULL AND fo.path != ''
                         THEN fo.path || '/' || fi.name
                         ELSE fi.name
                       END                  AS path,
                       NULL::text           AS parent_id,
                       fi.drive_id,
                       EXTRACT(EPOCH FROM fi.created_at)::bigint AS created_at,
                       EXTRACT(EPOCH FROM fi.content_modified_at)::bigint AS content_modified_at,
                       fi.size,
                       fi.mime_type,
                       fi.folder_id::text,
                       fi.blob_hash,
                       NULL::bigint         AS tree_modified_at
                  FROM storage.files fi
                  LEFT JOIN storage.folders fo ON fo.id = fi.folder_id
                 WHERE fi.name = $2
                   AND (
                         ($3 = '' AND fi.folder_id IS NULL)
                         OR fo.path = $3
                       )
                   AND NOT fi.is_trashed
                   AND fi.drive_id = $4
              ) sub
             LIMIT 1
            "#,
        )
        .bind(path) // $1
        .bind(filename) // $2
        .bind(&folder_path) // $3
        .bind(drive_id) // $4
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::internal_error("PathResolver", format!("resolve_in_drive: {e}")))?
        .ok_or_else(|| DomainError::not_found("Resource", path))?;

        let (
            resource_type,
            id,
            name,
            res_path,
            parent_id,
            drive_id,
            created_at,
            modified_at,
            size,
            mime_type,
            folder_id,
            blob_hash,
            tree_modified_at,
        ) = row;

        match resource_type.as_str() {
            "folder" => {
                let tree_mod = tree_modified_at.unwrap_or(modified_at) as u64;
                Ok(ResolvedResource::Folder(FolderDto {
                    etag: Folder::compute_etag(&id, tree_mod),
                    id,
                    name: name.clone(),
                    path: res_path,
                    parent_id,
                    drive_id,
                    created_at: epoch_to_datetime(created_at.max(0) as u64),
                    modified_at: epoch_to_datetime(modified_at.max(0) as u64),
                    tree_modified_at: epoch_to_datetime(tree_mod),
                    category: ResourceCategory::Folder,
                    // §14 provenance not selected by this resolver path —
                    // it's used for existence/type discrimination, not
                    // detailed DTO emission. Callers that need provenance
                    // reload through the repo.
                    created_by: None,
                    updated_by: None,
                    // Caller state flags not looked up here — the
                    // resolver is an internal utility that answers
                    // existence/type questions, not a wire emission
                    // path. Callers that emit to the SPA reload
                    // through the listing repo or the caller_flags
                    // helper.
                    is_favorite: false,
                    is_shared: false,
                    mount: None,
                }))
            }
            _ => {
                let mime = mime_type.unwrap_or_else(|| "application/octet-stream".to_string());
                let sz = size.unwrap_or(0) as u64;
                let hash = blob_hash.unwrap_or_default();
                let modified_at_u = modified_at as u64;
                let etag = File::compute_etag(&hash, modified_at_u);
                Ok(ResolvedResource::File(FileDto {
                    id,
                    name: name.clone(),
                    path: res_path,
                    size: sz,
                    mime_type: intern_mime(&mime),
                    folder_id,
                    created_at: epoch_to_datetime(created_at.max(0) as u64),
                    modified_at: epoch_to_datetime(modified_at_u),
                    category: category_for(&name, &mime),
                    content_hash: hash,
                    etag,
                    // §14 provenance not selected by this resolver path
                    created_by: None,
                    updated_by: None,
                    // Caller state flags not looked up here — see
                    // the folder branch above for rationale.
                    is_favorite: false,
                    is_shared: false,
                }))
            }
        }
    }

    /// Returns `true` if the resource at `path` belongs to `user_id`.
    /// Check whether `path` resolves to a folder or file within the
    /// given drive. Companion to `resolve_path_in_drive` — same scope
    /// filter, existence-only projection.
    pub async fn exists_in_drive(&self, path: &str, drive_id: Uuid) -> Result<bool, DomainError> {
        let path = path.trim_start_matches('/').trim_end_matches('/');
        if path.is_empty() {
            return Ok(false);
        }

        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let filename = segments[segments.len() - 1];
        let folder_path = if segments.len() > 1 {
            segments[..segments.len() - 1].join("/")
        } else {
            String::new()
        };

        let exists = sqlx::query_scalar::<_, bool>(
            r#"
            SELECT EXISTS(
              SELECT 1 FROM storage.folders
               WHERE path = $1 AND NOT is_trashed AND drive_id = $4
            ) OR EXISTS(
              SELECT 1
                FROM storage.files fi
                LEFT JOIN storage.folders fo ON fo.id = fi.folder_id
               WHERE fi.name = $2
                 AND (($3 = '' AND fi.folder_id IS NULL) OR fo.path = $3)
                 AND NOT fi.is_trashed
                 AND fi.drive_id = $4
            )
            "#,
        )
        .bind(path)
        .bind(filename)
        .bind(&folder_path)
        .bind(drive_id)
        .fetch_one(self.pool.as_ref())
        .await
        .map_err(|e| {
            DomainError::internal_error("PathResolver", format!("exists_in_drive: {e}"))
        })?;

        Ok(exists)
    }
}

#[cfg(all(test, integration_tests))]
mod resolve_id_tests {
    use super::*;
    use crate::infrastructure::repositories::pg::share_mount_pg_repository::trigger_tests::{
        make_user_with_drive, personal_root,
    };
    use crate::mount_it_support::fresh_db;

    async fn folder(pool: &PgPool, name: &str, parent: Uuid, drive: Uuid, by: Uuid) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO storage.folders (name, parent_id, drive_id, created_by, updated_by)
             VALUES ($1, $2, $3, $4, $4) RETURNING id",
        )
        .bind(name)
        .bind(parent)
        .bind(drive)
        .bind(by)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn file(pool: &PgPool, name: &str, parent: Uuid, drive: Uuid, by: Uuid) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO storage.files (name, folder_id, drive_id, created_by, updated_by, blob_hash)
             VALUES ($1, $2, $3, $4, $4, repeat('a', 64)) RETURNING id",
        )
        .bind(name)
        .bind(parent)
        .bind(drive)
        .bind(by)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn resolve_id_folder_file_trashed_and_link() {
        let (_c, pool) = fresh_db().await;
        let alice = make_user_with_drive(&pool, "alice").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let (a_drive, a_root) = personal_root(&pool, alice).await;
        let (b_drive, b_root) = personal_root(&pool, bob).await;
        let svc = PathResolverService::new(pool.clone());

        let reports = folder(&pool, "Reports", a_root, a_drive, alice).await;
        let root_name: String =
            sqlx::query_scalar("SELECT path FROM storage.folders WHERE id = $1")
                .bind(a_root)
                .fetch_one(&*pool)
                .await
                .unwrap();
        let reports_path = format!("{root_name}/Reports");

        let f = svc.resolve_id(reports).await.unwrap().unwrap();
        assert_eq!(
            f,
            IdAddress {
                kind: IdKind::Folder,
                drive_id: a_drive,
                path: reports_path.clone(),
                is_link: false
            }
        );

        let nested = file(&pool, "q1.txt", reports, a_drive, alice).await;
        let n = svc.resolve_id(nested).await.unwrap().unwrap();
        assert_eq!(n.kind, IdKind::File);
        assert_eq!(n.path, format!("{reports_path}/q1.txt"));

        let in_root = file(&pool, "loose.txt", a_root, a_drive, alice).await;
        let r = svc.resolve_id(in_root).await.unwrap().unwrap();
        assert_eq!(r.path, format!("{root_name}/loose.txt"));

        let link: Uuid = sqlx::query_scalar(
            "INSERT INTO storage.folders (name, parent_id, drive_id, created_by, updated_by, mount_target_id)
             VALUES ('Link', $1, $2, $3, $3, $4) RETURNING id",
        )
        .bind(b_root)
        .bind(b_drive)
        .bind(bob)
        .bind(reports)
        .fetch_one(&*pool)
        .await
        .unwrap();
        assert!(svc.resolve_id(link).await.unwrap().unwrap().is_link);

        sqlx::query("UPDATE storage.files SET is_trashed = TRUE WHERE id = $1")
            .bind(nested)
            .execute(&*pool)
            .await
            .unwrap();
        assert!(svc.resolve_id(nested).await.unwrap().is_none());
        assert!(svc.resolve_id(Uuid::new_v4()).await.unwrap().is_none());
    }
}

#[cfg(all(test, integration_tests))]
mod locate_tests {
    use super::*;
    use crate::application::ports::authorization_ports::AuthorizationEngine;
    use crate::application::services::share_mount_service::it::{engine, service_with_mode};
    use crate::common::config::LinksMode;
    use crate::domain::services::authorization::{Resource, Role, Subject};
    use crate::infrastructure::repositories::pg::share_mount_pg_repository::trigger_tests::{
        make_user_with_drive, personal_root,
    };
    use crate::mount_it_support::{fresh_db, provision_folder};

    async fn path_of(pool: &PgPool, id: Uuid) -> String {
        sqlx::query_scalar("SELECT path FROM storage.folders WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn locate_hops_through_a_link() {
        let (_c, pool) = fresh_db().await;
        let alice = provision_folder(&pool, "alice", "Reports").await;
        let bob = make_user_with_drive(&pool, "bob").await;
        let carol = make_user_with_drive(&pool, "carol").await;
        engine(&pool)
            .set_role(
                alice.owner_id,
                Subject::User(bob),
                Role::Viewer,
                Resource::Folder(alice.mount_folder_id),
                None,
            )
            .await
            .unwrap();
        let links = Arc::new(service_with_mode(
            &pool,
            engine(&pool),
            Some("Shares"),
            LinksMode::Auto,
        ));
        links.settle(bob, alice.mount_folder_id).await.unwrap();
        let (bob_drive, bob_root) = personal_root(&pool, bob).await;
        let link_id = links.repo.existing_mounts(bob_drive).await.unwrap()[0].mount_id;
        let link_path = path_of(&pool, link_id).await;
        let reports_path = path_of(&pool, alice.mount_folder_id).await;
        let svc = PathResolverService::new(pool.clone()).with_links(links.clone());

        let below = format!("{link_path}/q1.txt");
        let hop = svc.locate(bob, &below, bob_drive).await.unwrap();
        assert_eq!(
            hop,
            ResolvedLocation {
                drive_id: alice.drive_id,
                db_path: format!("{reports_path}/q1.txt"),
                via_link: Some(link_id),
                at_link: false,
            }
        );
        let at = svc.locate(bob, &link_path, bob_drive).await.unwrap();
        assert_eq!(
            (at.drive_id, at.db_path.as_str()),
            (alice.drive_id, reports_path.as_str())
        );
        assert!(at.at_link);

        let plain = format!("{}/Other/x.txt", path_of(&pool, bob_root).await);
        let p = svc.locate(bob, &plain, bob_drive).await.unwrap();
        assert_eq!(
            (p.drive_id, p.db_path, p.via_link),
            (bob_drive, plain, None)
        );

        let err = svc.locate(carol, &below, bob_drive).await.unwrap_err();
        let miss = svc
            .resolve_path_in_drive(&below, bob_drive)
            .await
            .unwrap_err();
        assert_eq!(
            (err.kind, err.to_string()),
            (miss.kind, miss.to_string()),
            "R0 denial reads as an ordinary miss"
        );

        let off = PathResolverService::new(pool.clone()).with_links(Arc::new(service_with_mode(
            &pool,
            engine(&pool),
            Some("Shares"),
            LinksMode::Off,
        )));
        let err = off.locate(bob, &below, bob_drive).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::NotFound, "links off");

        sqlx::query("UPDATE storage.folders SET is_trashed = TRUE WHERE id = $1")
            .bind(alice.mount_folder_id)
            .execute(&*pool)
            .await
            .unwrap();
        let err = svc.locate(bob, &below, bob_drive).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::NotFound, "trashed target");
    }
}
