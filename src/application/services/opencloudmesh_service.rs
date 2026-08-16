use std::{collections::HashSet, str::FromStr, sync::Arc};

use axum::http::Uri;
use ocm_drivers::protocols::webdav::Webdav;
use ocm_server_axum::{
    discover,
    drivers::{protocols::Protocol, resources::Resource as OcmResource},
    http_client::ReqwestClient,
    send_share,
    share::SendShareError,
    types::common::OcmAddress,
};

use crate::{
    application::{
        ports::{
            authorization_ports::AuthorizationEngine, share_ports::ShareStoragePort,
            storage_ports::FileReadPort,
        },
        services::{
            share_service::ShareServiceError, user_lifecycle_service::UserLifecycleService,
        },
    },
    common::errors::{DomainError, ErrorKind},
    domain::{
        entities::{
            file::File,
            folder::Folder,
            share::{Share, ShareItemType},
            user::{User, UserRole},
        },
        repositories::user_repository::{UserRepository, UserRepositoryError},
        services::authorization::{Permission, Resource, Role, Subject},
    },
    infrastructure::{
        repositories::{
            FileBlobReadRepository, FolderDbRepository, UserPgRepository, pg::SharePgRepository,
        },
        services::pg_acl_engine::PgAclEngine,
    },
};

use crate::domain::repositories::folder_repository::FolderRepository;

pub struct OpenCloudMeshService {
    user_storage: Arc<UserPgRepository>,
    authorization: Arc<PgAclEngine>,
    folder_repository: Arc<FolderDbRepository>,
    file_repository: Arc<FileBlobReadRepository>,
    share_repository: Arc<SharePgRepository>,
    // magic_link_repo: Arc<dyn MagicLinkTokenRepository>,
    user_lifecycle: Arc<UserLifecycleService>,
    public_base_url: String,
    client: ReqwestClient,
    protocols: Arc<Vec<Box<dyn Protocol>>>,
}

impl OpenCloudMeshService {
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        user_storage: Arc<UserPgRepository>,
        authorization: Arc<PgAclEngine>,
        folder_repository: Arc<FolderDbRepository>,
        file_repository: Arc<FileBlobReadRepository>,
        share_repository: Arc<SharePgRepository>,
        user_lifecycle: Arc<UserLifecycleService>,
        public_base_url: String,
    ) -> Self {
        //FIXME: this should not be async, is there already some http client available?
        let client = ReqwestClient::new(None, true).await;
        let protocols: Arc<Vec<Box<dyn Protocol>>> =
            Arc::new(vec![Box::new(Webdav::new("/dav".parse().unwrap()))]);
        Self {
            user_storage,
            authorization,
            folder_repository,
            file_repository,
            share_repository,
            user_lifecycle,
            public_base_url,
            client,
            protocols,
        }
    }

    pub async fn resolve_or_create_recipient(
        &self,
        share_with: &str,
    ) -> Result<User, DomainError> {
        let share_with = OcmAddress::from_str(share_with)
            .map_err(|e| DomainError::new(ErrorKind::InvalidInput, "OCM Address", e))?;

        // TODO: Is it actually necessary to create an external user? or would it be more efficient
        // to just create a token or a contact? Also, we have no way to know if the user exists
        // without sending a share, the OcmAddress is NOT an email and should not be stored in
        // place of an email. the share_with might represent a group on the receiving server
        match UserRepository::get_user_by_email(&*self.user_storage, share_with.as_ref()).await {
            Ok(user) => Ok(user),
            Err(UserRepositoryError::NotFound(_)) => {
                self.create_external_user(&share_with.as_ref()).await
            }
            Err(e) => Err(DomainError::from(e)),
        }
    }

    pub async fn send_share(
        &self,
        recipient: User,
        inviter_id: uuid::Uuid,
        resource: &Resource,
    ) -> Result<Share, DomainError> {
        let share_with = OcmAddress::from_str(recipient.email())
            .map_err(|e| DomainError::new(ErrorKind::InvalidInput, "OCM Address", e))?;

        let receiving_server_url =
            Uri::try_from(share_with.get_server_url()).expect("Invalid share_with!");

        let share_item_type = match resource {
            Resource::Folder(_) => Ok(ShareItemType::Folder),
            Resource::File(_) => Ok(ShareItemType::File),
            _ => {
                tracing::info!(
                    target: "audit",
                    event = "ocm.share_suppressed",
                    reason = "resource_kind_unsupported",
                    user_id = %inviter_id,
                    resource_kind = %resource.type_str(),
                    "OCM share suppressed: {} resources aren't shareable via OpenCloudMesh",
                    resource.type_str(),
                );
                Err(DomainError::operation_not_supported(
                    "OCM Share",
                    format!(
                        "{} resources aren't shareable via OpenCloudMesh",
                        resource.type_str()
                    ),
                ))
            }
        }?;

        let resource_id = resource.id().to_string();
        let ocm_resource = self.get_resource(&resource_id, &share_item_type).await?;

        // AuthZ: only callers with `Share` on the resource may
        // send a federated share. Without this gate, an
        // ex-Viewer who kept a guessed UUID could launder a
        // temporary read into a
        // permanent federated share that survives their own grant
        // revocation. `Permission::Share` is bundled with the
        // `owner` and `editor` role_grants only. `require` returns
        // `not_found` on denial (anti-enum, matches the shape used
        // by every other share route). See `docs/plan/authz_audit/`.
        self.authorization
            .require(
                Subject::User(inviter_id),
                Permission::Share,
                resource.to_owned(),
            )
            .await?;

        let receiving_server = discover(&self.client, &receiving_server_url)
            .await
            .map_err(|e| {
                DomainError::new(
                    ErrorKind::InternalError,
                    "OCM Discovery",
                    format!(
                        "Failed to discovery receiving OCM Server at {receiving_server_url}: {:?}",
                        e
                    ),
                )
            })?;

        let share = Share::new(
            resource_id,
            Some(ocm_resource.name().to_string()),
            share_item_type,
            inviter_id,
            None,
        )
        .map_err(|e| ShareServiceError::Validation(e.to_string()))?;

        // TODO wire actual grant permissions
        let permissions = HashSet::from([ocm_server_axum::drivers::protocols::Permission::Read]);

        let (new_ocm_share, creation_response) = send_share(
            &self.client,
            share.id().to_string(),
            // FIXME: this is ugly
            format!(
                "{inviter_id}@{}",
                self.public_base_url.split_once("://").unwrap().1
            )
            .try_into()
            .unwrap(),
            &receiving_server,
            share_with.clone(),
            &ocm_resource,
            &permissions,
            self.protocols.as_slice(),
        )
        .await
        .map_err(|e| {
            // TODO: proper logging
            tracing::error!("{:?}", e);
            e
        })?;

        if let Some(recipient_name) = creation_response.recipient_display_name {
            if let Ok(mut user) =
                UserRepository::get_user_by_email(&*self.user_storage, share_with.as_ref()).await
            {
                user.set_given_name(Some(recipient_name));
                // Setting the displayname of the recipient is nice to have, so accept failure
                let _ = UserRepository::update_user(&*self.user_storage, user).await;
            }
        };

        let share = if let Some(shared_secret) = new_ocm_share
            .protocol
            .webdav
            .and_then(|webdav| webdav.shared_secret)
        {
            share.with_token(shared_secret)
        } else {
            share
        };

        // TODO: ocm shares should probably be stored in a separate repo?
        let saved_share = self
            .share_repository
            .save_share(&share)
            .await
            .map_err(|e| ShareServiceError::Repository(e.to_string()))?;

        // FIXME: This is potentially wrong
        self.authorization
            .set_role(
                recipient.id(),
                Subject::Token(saved_share.id()),
                Role::Viewer,
                resource.to_owned(),
                None,
            )
            .await
            .map_err(|e| ShareServiceError::Repository(e.to_string()))?;

        Ok(saved_share)
    }

    /// Verifies that the item to share exists
    async fn get_resource(
        &self,
        item_id: &str,
        item_type: &ShareItemType,
    ) -> Result<OcmFsResource, ShareServiceError> {
        match item_type {
            ShareItemType::File => {
                Ok(self
                    .file_repository
                    .get_file(item_id) // Using the correct method from the FileStoragePort trait
                    .await
                    .map_err(|_| {
                        ShareServiceError::ItemNotFound(format!(
                            "File with ID {} not found",
                            item_id
                        ))
                    })?
                    .into())
            }
            ShareItemType::Folder => {
                Ok(self
                    .folder_repository
                    .get_folder(item_id) // Using the correct method from the FolderStoragePort trait
                    .await
                    .map_err(|_| {
                        ShareServiceError::ItemNotFound(format!(
                            "Folder with ID {} not found",
                            item_id
                        ))
                    })?
                    .into())
            }
        }
    }

    /// Lazy provisioning path. Runs the two policy guards (kill switch
    /// and per-domain allowlist) before touching the DB.
    ///
    async fn create_external_user(&self, normalised_email: &str) -> Result<User, DomainError> {
        // External users are created without a username or password.
        // `password_hash IS NULL` is the canonical no-password marker.
        //
        // federation_kind stays None in Phase A of the federation-identity
        // rename — magic-link externals get their `federation_kind` stamp
        // in a future PR when the invite handler is refactored to opt into
        // the composable federation model. Behaviour is unchanged today.
        let user = User::new(
            normalised_email.to_string(),
            None,
            None,
            None, // federation_kind
            None, // federation_issuer
            None, // federation_subject
            UserRole::User,
            0,
            true,
        )
        .map_err(|e| {
            DomainError::new(
                ErrorKind::InvalidInput,
                "OCM Share",
                format!("invalid external user data: {}", e),
            )
        })?;
        let saved = UserRepository::create_user(&*self.user_storage, user)
            .await
            .map_err(DomainError::from)?;

        // Fire the user-lifecycle dispatcher — `on_user_created` lights
        // up audit + future external-identity provenance bookkeeping.
        // Errors are logged-and-continued by the dispatcher's
        // `dispatch_created` per the lifecycle contract.
        self.user_lifecycle.dispatch_created(&saved).await;

        Ok(saved)
    }
}

#[derive(Clone, Debug)]
enum OcmFsResource {
    File(File),
    Folder(Folder),
}

impl From<File> for OcmFsResource {
    fn from(value: File) -> Self {
        OcmFsResource::File(value)
    }
}

impl From<Folder> for OcmFsResource {
    fn from(value: Folder) -> Self {
        OcmFsResource::Folder(value)
    }
}

impl OcmResource for OcmFsResource {
    const RESOURCE_TYPE: &str = "file";

    fn uri(&self) -> &str {
        match self {
            OcmFsResource::File(file) => file.id(),
            OcmFsResource::Folder(folder) => folder.id(),
        }
    }

    fn name(&self) -> &str {
        match self {
            OcmFsResource::File(file) => file.name(),
            OcmFsResource::Folder(folder) => folder.name(),
        }
    }
}

impl From<SendShareError> for DomainError {
    fn from(value: SendShareError) -> Self {
        match value {
            SendShareError::InvalidOcmEndpoint(invalid_uri) => {
                DomainError::validation_error(format!("Invalid OCM Endpoint: {invalid_uri}"))
            }
            SendShareError::RecievingServerNotEnabled => {
                DomainError::validation_error("Receiving OCM Server is not enabled")
            }
            SendShareError::VersionCompatiblity(e) => {
                DomainError::validation_error(format!("Incompatible OCM Version: {}", e))
            }
            SendShareError::UnfullfilledCriterium(e) => DomainError::validation_error(format!(
                "Can't fullfill a criterium of the receiving OCM Server: {}",
                e
            )),
            SendShareError::RequestError(e) => DomainError::internal_error("OCM", e),
            SendShareError::StoringShareFailed(_) => {
                DomainError::internal_error("OCM Share", "Failed to store OCM Share")
            }
            SendShareError::InvalidShareWith(e) => {
                DomainError::validation_error(format!("Invalid OCM Recipient: {e}"))
            }
            SendShareError::InvalidSender() => DomainError::validation_error("Invalid OCM sender"),
            SendShareError::UnsupportedShare => DomainError::validation_error(
                "Receiving Server does not support receiving this resource via any supported protocol",
            ),
        }
    }
}
