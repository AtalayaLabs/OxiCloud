use std::result::Result;
use uuid::Uuid;

use crate::common::errors::DomainError;
use crate::domain::entities::contact::AddressBook;

pub type AddressBookRepositoryResult<T> = Result<T, DomainError>;

/// Repository interface for AddressBook entity operations.
///
/// Post-Round-3, access-control state lives in `storage.role_grants`.
/// The pre-Round-3 methods that read/wrote `carddav.address_book_shares`
/// (`get_shared_address_books`, `share_address_book`,
/// `unshare_address_book`, `get_address_book_shares`) have been removed
/// from this trait, and the backing table was dropped in
/// `20260906000002_drop_legacy_share_tables.sql`.
pub trait AddressBookRepository: Send + Sync + 'static {
    async fn create_address_book(
        &self,
        address_book: AddressBook,
    ) -> AddressBookRepositoryResult<AddressBook>;
    async fn update_address_book(
        &self,
        address_book: AddressBook,
    ) -> AddressBookRepositoryResult<AddressBook>;
    async fn delete_address_book(&self, id: &Uuid) -> AddressBookRepositoryResult<()>;
    /// Batch sibling of `get_address_book_by_id`: one `= ANY($1)`
    /// round-trip for a page of grant-derived ids. Missing ids drop
    /// out; ordering is not guaranteed.
    async fn get_address_books_by_ids(
        &self,
        ids: &[Uuid],
    ) -> AddressBookRepositoryResult<Vec<AddressBook>>;

    async fn get_address_book_by_id(
        &self,
        id: &Uuid,
    ) -> AddressBookRepositoryResult<Option<AddressBook>>;
    /// Direct owner enumeration — same semantics as the calendar
    /// counterpart. The service layer prefers
    /// `authz.list_incoming_grants`, but internal maintenance paths
    /// keep the owner-only lookup available.
    async fn get_address_books_by_owner(
        &self,
        owner_id: Uuid,
    ) -> AddressBookRepositoryResult<Vec<AddressBook>>;
}
