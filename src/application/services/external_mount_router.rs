//! Classifies file/folder ids into native vs. external-mount handling.
//!
//! This is the single, cheap hook the service layer calls before any
//! `Uuid::parse_str`, so synthetic `ext:` ids and mount-root UUIDs branch to the
//! provider while everything else flows to the PostgreSQL repositories unchanged.

use std::sync::Arc;

use uuid::Uuid;

use crate::application::services::mount_registry::{MountConfig, MountRegistry};
use crate::domain::services::authorization::Resource;
use crate::domain::services::external_mount_id::{NodeId, is_external_id, parse_child_id};

/// The result of classifying an id.
pub enum ResolvedId {
    /// Plain native resource (UUID not registered as a mount root, or an
    /// unrecognized id). Handle exactly as today.
    Regular,
    /// A real UUID that IS a mount root. Listing/metadata branch to the provider;
    /// the row itself still exists natively.
    MountRoot { cfg: Arc<MountConfig> },
    /// A synthetic id addressing an entry inside a mount.
    MountChild {
        cfg: Arc<MountConfig>,
        node_id: NodeId,
    },
}

/// Thin, cloneable classifier over the mount registry.
#[derive(Clone)]
pub struct MountRouter {
    registry: Arc<MountRegistry>,
}

impl MountRouter {
    /// Construct from the shared registry.
    pub fn new(registry: Arc<MountRegistry>) -> Self {
        Self { registry }
    }

    /// Borrow the underlying registry (for path-based resolution / admin reload).
    pub fn registry(&self) -> &Arc<MountRegistry> {
        &self.registry
    }

    /// Fast path: are there no mounts at all? Lets callers skip classification.
    pub fn is_empty(&self) -> bool {
        self.registry.is_empty()
    }

    /// Classify an id. Never parses a provider `node_id` — only the envelope.
    pub fn classify(&self, id: &str) -> ResolvedId {
        if is_external_id(id) {
            if let Some(child) = parse_child_id(id)
                && let Some(cfg) = self.registry.get(&child.mount_id)
            {
                return ResolvedId::MountChild {
                    cfg,
                    node_id: child.node_id,
                };
            }
            // Malformed or dangling `ext:` id — fall through to Regular so it
            // surfaces a clean NotFound downstream rather than hitting the repos.
            return ResolvedId::Regular;
        }
        if let Ok(uuid) = Uuid::parse_str(id)
            && let Some(cfg) = self.registry.get(&uuid)
        {
            return ResolvedId::MountRoot { cfg };
        }
        ResolvedId::Regular
    }

    /// The AuthZ resource guarding a *folder* id that may be native or
    /// mount-addressed. Everything inside a mount is gated by the mount-root
    /// folder grant, so both the root UUID and `ext:` children collapse onto
    /// `Resource::Folder(cfg.mount_id)`; a native id parses to its own UUID.
    /// `None` when the id is neither — callers answer NotFound (never 500),
    /// which is also the anti-enumeration shape `FolderService::folder_resource`
    /// uses.
    ///
    /// Added for #794: the DAV handlers ran `Uuid::parse_str(&dto.id)` on a
    /// resolved DTO *before* the permission check, so every `ext:` id — i.e.
    /// every path below a mount root — turned into a 404.
    pub fn folder_authz_resource(&self, id: &str) -> Option<Resource> {
        match self.classify(id) {
            ResolvedId::MountRoot { cfg } | ResolvedId::MountChild { cfg, .. } => {
                Some(Resource::Folder(cfg.mount_id))
            }
            ResolvedId::Regular => Uuid::parse_str(id).ok().map(Resource::Folder),
        }
    }

    /// File counterpart of [`Self::folder_authz_resource`]: a native file id
    /// becomes `Resource::File`, an `ext:` child collapses onto the mount-root
    /// folder. A mount-root UUID is a directory and never a file → `None`.
    pub fn file_authz_resource(&self, id: &str) -> Option<Resource> {
        match self.classify(id) {
            ResolvedId::MountChild { cfg, .. } => Some(Resource::Folder(cfg.mount_id)),
            ResolvedId::MountRoot { .. } => None,
            ResolvedId::Regular => Uuid::parse_str(id).ok().map(Resource::File),
        }
    }

    /// `ext:` id → `(mount config, provider node)`; `None` for native ids and
    /// mount roots. The DAV GET/HEAD paths use this to stream a mount file
    /// through its provider instead of the blob store.
    pub fn mount_child(&self, id: &str) -> Option<(Arc<MountConfig>, NodeId)> {
        match self.classify(id) {
            ResolvedId::MountChild { cfg, node_id } => Some((cfg, node_id)),
            _ => None,
        }
    }

    /// True when `id` addresses anything inside a mount (root or child).
    pub fn is_mount_id(&self, id: &str) -> bool {
        matches!(
            self.classify(id),
            ResolvedId::MountRoot { .. } | ResolvedId::MountChild { .. }
        )
    }

    /// Path-based lookup for the protocol surfaces (WebDAV / NextCloud): does
    /// `internal_path` descend into a mount within `drive_id`? Returns the mount
    /// config plus the remainder relpath (empty when the path IS the mount root).
    pub fn find_path(
        &self,
        drive_id: uuid::Uuid,
        internal_path: &str,
    ) -> Option<(Arc<MountConfig>, String)> {
        self.registry.find_mount_for_path(drive_id, internal_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::ports::external_mount_ports::{
        ExternalMountRecord, ExternalMountRepositoryPort,
    };
    use crate::domain::errors::DomainError;
    use crate::domain::services::external_mount_id::encode_child_id;
    use crate::infrastructure::services::mount_provider_factory::DefaultMountProviderFactory;
    use async_trait::async_trait;
    use tempfile::TempDir;

    struct FakeRepo(Vec<ExternalMountRecord>);
    #[async_trait]
    impl ExternalMountRepositoryPort for FakeRepo {
        async fn list_all(&self) -> Result<Vec<ExternalMountRecord>, DomainError> {
            Ok(self.0.clone())
        }
    }

    async fn router_with_mount(mount_id: Uuid, dir: &TempDir) -> MountRouter {
        let repo = FakeRepo(vec![ExternalMountRecord {
            mount_folder_id: mount_id,
            kind: "local_fs".to_string(),
            config: serde_json::json!({ "path": dir.path().to_str().unwrap() }),
            name: "M".to_string(),
            owner_id: Uuid::new_v4(),
            read_only: false,
            drive_id: Uuid::new_v4(),
            mount_path: "Personal/M".to_string(),
        }]);
        let reg = Arc::new(MountRegistry::empty());
        reg.reload(&repo, &DefaultMountProviderFactory::new()).await;
        MountRouter::new(reg)
    }

    #[test]
    fn empty_registry_classifies_everything_regular() {
        let router = MountRouter::new(Arc::new(MountRegistry::empty()));
        assert!(router.is_empty());
        assert!(matches!(
            router.classify(&Uuid::new_v4().to_string()),
            ResolvedId::Regular
        ));
        assert!(matches!(
            router.classify("ext:deadbeef:dG9rZW4"),
            ResolvedId::Regular
        ));
        assert!(matches!(router.classify("garbage"), ResolvedId::Regular));
    }

    #[tokio::test]
    async fn classifies_mount_root_uuid() {
        let dir = TempDir::new().unwrap();
        let mount_id = Uuid::new_v4();
        let router = router_with_mount(mount_id, &dir).await;

        match router.classify(&mount_id.to_string()) {
            ResolvedId::MountRoot { cfg } => assert_eq!(cfg.mount_id, mount_id),
            _ => panic!("expected MountRoot"),
        }
        assert!(router.is_mount_id(&mount_id.to_string()));
    }

    #[tokio::test]
    async fn classifies_ext_child_id() {
        let dir = TempDir::new().unwrap();
        let mount_id = Uuid::new_v4();
        let router = router_with_mount(mount_id, &dir).await;

        let child = encode_child_id(mount_id, "docs/a.txt");
        match router.classify(&child) {
            ResolvedId::MountChild { cfg, node_id } => {
                assert_eq!(cfg.mount_id, mount_id);
                assert_eq!(node_id.as_str(), "docs/a.txt");
            }
            _ => panic!("expected MountChild"),
        }
        assert!(router.is_mount_id(&child));
    }

    /// #794 regression pin: an `ext:` child id must authorize against the
    /// mount-root folder instead of failing a UUID parse (which the handlers
    /// turned into a 404 for every path below a mount root). The root UUID
    /// itself resolves to the same resource; a native UUID keeps its own.
    #[tokio::test]
    async fn authz_resources_collapse_mount_ids_onto_mount_root() {
        let dir = TempDir::new().unwrap();
        let mount_id = Uuid::new_v4();
        let router = router_with_mount(mount_id, &dir).await;

        let child = encode_child_id(mount_id, "docs/a.txt");
        assert_eq!(
            router.folder_authz_resource(&child),
            Some(Resource::Folder(mount_id))
        );
        assert_eq!(
            router.file_authz_resource(&child),
            Some(Resource::Folder(mount_id))
        );
        assert_eq!(
            router.folder_authz_resource(&mount_id.to_string()),
            Some(Resource::Folder(mount_id))
        );
        // A mount root is a directory, never a file.
        assert_eq!(router.file_authz_resource(&mount_id.to_string()), None);

        let native = Uuid::new_v4();
        assert_eq!(
            router.folder_authz_resource(&native.to_string()),
            Some(Resource::Folder(native))
        );
        assert_eq!(
            router.file_authz_resource(&native.to_string()),
            Some(Resource::File(native))
        );
        assert_eq!(router.folder_authz_resource("garbage"), None);
        assert_eq!(router.file_authz_resource("garbage"), None);

        let (cfg, node) = router.mount_child(&child).expect("mount child");
        assert_eq!(cfg.mount_id, mount_id);
        assert_eq!(node.as_str(), "docs/a.txt");
        assert!(router.mount_child(&mount_id.to_string()).is_none());
        assert!(router.mount_child(&native.to_string()).is_none());
    }

    #[tokio::test]
    async fn ext_id_for_unregistered_mount_is_regular() {
        let dir = TempDir::new().unwrap();
        let mount_id = Uuid::new_v4();
        let router = router_with_mount(mount_id, &dir).await;

        // A well-formed ext: id but for a DIFFERENT (unknown) mount → Regular,
        // so it 404s downstream rather than hitting the repos.
        let dangling = encode_child_id(Uuid::new_v4(), "x");
        assert!(matches!(router.classify(&dangling), ResolvedId::Regular));

        // A plain (non-mount) UUID is also Regular.
        assert!(matches!(
            router.classify(&Uuid::new_v4().to_string()),
            ResolvedId::Regular
        ));
    }
}
