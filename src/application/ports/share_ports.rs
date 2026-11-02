use uuid::Uuid;

use crate::{
    application::dtos::{
        pagination::PaginatedResponseDto,
        share_dto::{CreateShareDto, ShareDto, UpdateShareDto},
    },
    common::errors::DomainError,
    domain::entities::share::ShareItemType,
};

pub trait ShareUseCase: Send + Sync + 'static {
    /// Create a new shared link for a file or folder
    async fn create_shared_link(
        &self,
        user_id: Uuid,
        dto: CreateShareDto,
    ) -> Result<ShareDto, DomainError>;

    /// Get a shared link by its ID (ownership-verified)
    async fn get_shared_link(&self, id: Uuid, requester_id: Uuid) -> Result<ShareDto, DomainError>;

    /// Get a shared link by its token (for access by non-users)
    async fn get_shared_link_by_token(&self, token: &str) -> Result<ShareDto, DomainError>;

    /// Get all shared links for a specific item (ownership-verified)
    async fn get_shared_links_for_item(
        &self,
        item_id: &str,
        item_type: &ShareItemType,
        requester_id: Uuid,
    ) -> Result<Vec<ShareDto>, DomainError>;

    /// Update a shared link (ownership-verified)
    async fn update_shared_link(
        &self,
        id: Uuid,
        requester_id: Uuid,
        dto: UpdateShareDto,
    ) -> Result<ShareDto, DomainError>;

    /// Delete a shared link (ownership-verified)
    async fn delete_shared_link(&self, id: Uuid, requester_id: Uuid) -> Result<(), DomainError>;

    /// Get all shared links created by a specific user
    async fn get_user_shared_links(
        &self,
        user_id: Uuid,
        page: usize,
        per_page: usize,
    ) -> Result<PaginatedResponseDto<ShareDto>, DomainError>;

    /// Verify a password for a password-protected shared link.
    /// On success, returns the full share metadata (`ShareDto`).
    /// On failure (wrong password), returns `AccessDenied`.
    async fn verify_shared_link_password(
        &self,
        token: &str,
        password: &str,
    ) -> Result<ShareDto, DomainError>;

    /// Register an access to a shared link
    async fn register_shared_link_access(&self, token: &str) -> Result<(), DomainError>;
}

pub trait ShareStoragePort: Send + Sync + 'static {
    async fn save_share(
        &self,
        share: &crate::domain::entities::share::Share,
    ) -> Result<crate::domain::entities::share::Share, DomainError>;

    async fn find_share_by_token(
        &self,
        token: &str,
    ) -> Result<crate::domain::entities::share::Share, DomainError>;

    /// Find a share by ID only if it belongs to the given user.
    /// Returns `NotFound` if the share doesn't exist OR belongs to another user
    /// (prevents share-ID enumeration).
    async fn find_share_by_id_for_user(
        &self,
        id: Uuid,
        user_id: Uuid,
    ) -> Result<crate::domain::entities::share::Share, DomainError>;

    /// Delete a share only if it belongs to the given user.
    async fn delete_share_for_user(&self, id: Uuid, user_id: Uuid) -> Result<(), DomainError>;

    /// Find shares for a specific item that belong to the given user.
    async fn find_shares_by_item_for_user(
        &self,
        item_id: &str,
        item_type: &ShareItemType,
        user_id: Uuid,
    ) -> Result<Vec<crate::domain::entities::share::Share>, DomainError>;

    async fn update_share(
        &self,
        share: &crate::domain::entities::share::Share,
    ) -> Result<crate::domain::entities::share::Share, DomainError>;

    /// Atomically bump a link's access counter (public share landing).
    /// Returns the number of rows updated — 0 means "no live share for
    /// this token" (missing OR expired).
    ///
    /// The default is the legacy read-modify-write (kept for test mocks);
    /// `SharePgRepository` overrides it with a single `UPDATE … SET
    /// access_count = access_count + 1`, replacing 2 correlated-subquery
    /// round-trips per anonymous visit with 1 and removing the lost-update
    /// race between concurrent visitors (benches/SHARE-ACCESS.md).
    async fn increment_access_count(&self, token: &str) -> Result<u64, DomainError> {
        let share = match self.find_share_by_token(token).await {
            Ok(s) => s,
            Err(e) if e.kind == crate::common::errors::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e),
        };
        if share.is_expired() {
            return Ok(0);
        }
        self.update_share(&share.increment_access_count()).await?;
        Ok(1)
    }

    /// Consume one download from this link's allowance.
    ///
    /// `false` means the allowance is exhausted, or no such share exists.
    /// The check and the increment must be one atomic step.
    async fn consume_download(&self, share_id: Uuid) -> Result<bool, DomainError>;

    async fn find_shares_by_user(
        &self,
        user_id: Uuid,
        offset: usize,
        limit: usize,
    ) -> Result<(Vec<crate::domain::entities::share::Share>, usize), DomainError>;
}
