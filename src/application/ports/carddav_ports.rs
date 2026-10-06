use crate::application::dtos::address_book_dto::{
    AddressBookDto, CreateAddressBookDto, UpdateAddressBookDto,
};
use crate::application::dtos::contact_dto::{
    ContactDto, ContactGroupDto, CreateContactDto, CreateContactGroupDto, CreateContactVCardDto,
    GroupMembershipDto, UpdateContactDto, UpdateContactGroupDto,
};
use crate::common::errors::DomainError;
use crate::domain::entities::contact::{AddressBook, Contact, ContactGroup};
use uuid::Uuid;

pub type CardDavRepositoryError = DomainError;

/// Low-level storage port for CardDAV resources. Post-Round-3 the
/// port covers ONLY raw storage operations — everything that used
/// to be routed through it for sharing (`share_address_book`,
/// `unshare_address_book`, `get_address_book_shares`) or
/// scope-listing (`get_address_books_by_owner`,
/// `get_shared_address_books`) is gone. Access decisions live in
/// `AuthorizationEngine`; sharing state lives in
/// `storage.role_grants`. The service layer (`ContactService`) gates
/// each call, then reaches through this port for storage.
///
/// Symmetric with `CalendarStoragePort`. Implemented by
/// `ContactStorageAdapter` against Postgres today; a future backend
/// (external CardDAV, LDAP directory, in-memory test mock) would
/// implement the same trait and swap in via DI.
pub trait ContactStoragePort: Send + Sync + 'static {
    // ── Address books ────────────────────────────────────────────
    async fn create_address_book(
        &self,
        address_book: AddressBook,
    ) -> Result<AddressBook, DomainError>;
    async fn update_address_book(
        &self,
        address_book: AddressBook,
    ) -> Result<AddressBook, DomainError>;
    async fn delete_address_book(&self, id: &Uuid) -> Result<(), DomainError>;
    async fn get_address_book_by_id(&self, id: &Uuid) -> Result<Option<AddressBook>, DomainError>;

    /// Batch sibling of [`Self::get_address_book_by_id`]: hydrate a page
    /// of grant-derived ids in ONE storage round-trip. Missing rows drop
    /// out silently; ordering is not guaranteed.
    async fn get_address_books_by_ids(&self, ids: &[Uuid])
    -> Result<Vec<AddressBook>, DomainError>;

    // ── Contacts ─────────────────────────────────────────────────
    async fn create_contact(&self, contact: Contact) -> Result<Contact, DomainError>;
    async fn update_contact(&self, contact: Contact) -> Result<Contact, DomainError>;
    async fn delete_contact(&self, id: &Uuid) -> Result<(), DomainError>;
    async fn get_contact_by_id(&self, id: &Uuid) -> Result<Option<Contact>, DomainError>;
    /// Indexed single-row lookup by vCard UID within a specific book.
    async fn get_contact_by_uid(
        &self,
        address_book_id: &Uuid,
        uid: &str,
    ) -> Result<Option<Contact>, DomainError>;
    /// Indexed batch lookup by vCard UID within a specific book.
    async fn get_contacts_by_uids(
        &self,
        address_book_id: &Uuid,
        uids: &[String],
    ) -> Result<Vec<Contact>, DomainError>;
    async fn get_contacts_by_address_book(
        &self,
        address_book_id: &Uuid,
    ) -> Result<Vec<Contact>, DomainError>;
    /// Cursor stream over the book's contacts in listing order — feeds
    /// the streaming CardDAV emitters.
    fn stream_contacts_by_book(
        &self,
        address_book_id: Uuid,
    ) -> futures::stream::BoxStream<'static, Result<Contact, DomainError>>;
    async fn get_contacts_by_address_book_paginated(
        &self,
        address_book_id: &Uuid,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<Contact>, DomainError>;
    async fn search_contacts(
        &self,
        address_book_id: &Uuid,
        query: &str,
    ) -> Result<Vec<Contact>, DomainError>;

    // ── Contact groups ───────────────────────────────────────────
    async fn create_group(&self, group: ContactGroup) -> Result<ContactGroup, DomainError>;
    async fn update_group(&self, group: ContactGroup) -> Result<ContactGroup, DomainError>;
    async fn delete_group(&self, id: &Uuid) -> Result<(), DomainError>;
    async fn get_group_by_id(&self, id: &Uuid) -> Result<Option<ContactGroup>, DomainError>;
    async fn get_groups_by_address_book(
        &self,
        address_book_id: &Uuid,
    ) -> Result<Vec<ContactGroup>, DomainError>;

    // ── Group membership ─────────────────────────────────────────
    async fn add_contact_to_group(
        &self,
        group_id: &Uuid,
        contact_id: &Uuid,
    ) -> Result<(), DomainError>;
    async fn remove_contact_from_group(
        &self,
        group_id: &Uuid,
        contact_id: &Uuid,
    ) -> Result<(), DomainError>;
    async fn get_contacts_in_group(&self, group_id: &Uuid) -> Result<Vec<Contact>, DomainError>;
    /// Membership count without hydrating the contacts (vCard TEXT +
    /// 3 JSONB parses per row) — for group summary DTOs.
    async fn count_contacts_in_group(&self, group_id: &Uuid) -> Result<i64, DomainError>;
    async fn get_groups_for_contact(
        &self,
        contact_id: &Uuid,
    ) -> Result<Vec<ContactGroup>, DomainError>;
}

pub trait AddressBookUseCase: Send + Sync + 'static {
    // Address Book operations
    async fn create_address_book(
        &self,
        dto: CreateAddressBookDto,
    ) -> Result<AddressBookDto, DomainError>;
    async fn update_address_book(
        &self,
        address_book_id: &str,
        update: UpdateAddressBookDto,
    ) -> Result<AddressBookDto, DomainError>;
    async fn delete_address_book(
        &self,
        address_book_id: &str,
        user_id: Uuid,
    ) -> Result<(), DomainError>;
    async fn get_address_book(
        &self,
        address_book_id: &str,
        user_id: Uuid,
    ) -> Result<AddressBookDto, DomainError>;
    async fn list_user_address_books(
        &self,
        user_id: Uuid,
    ) -> Result<Vec<AddressBookDto>, DomainError>;
}

pub trait ContactUseCase: Send + Sync + 'static {
    // Contact operations
    async fn create_contact(&self, dto: CreateContactDto) -> Result<ContactDto, DomainError>;
    async fn create_contact_from_vcard(
        &self,
        dto: CreateContactVCardDto,
    ) -> Result<ContactDto, DomainError>;
    async fn update_contact(
        &self,
        contact_id: &str,
        update: UpdateContactDto,
    ) -> Result<ContactDto, DomainError>;
    async fn delete_contact(&self, contact_id: &str, user_id: Uuid) -> Result<(), DomainError>;
    async fn get_contact(&self, contact_id: &str, user_id: Uuid)
    -> Result<ContactDto, DomainError>;
    /// Resolve one contact by its vCard UID (the identifier CardDAV
    /// object resources are addressed by) with an indexed single-row
    /// lookup — instead of listing the whole address book (every row
    /// with its vCard + JSONB columns) and filtering client-side.
    /// `Ok(None)` when no contact with that UID exists in the book.
    async fn get_contact_by_uid(
        &self,
        address_book_id: &str,
        uid: &str,
        user_id: Uuid,
    ) -> Result<Option<ContactDto>, DomainError>;
    /// Resolve a batch of contacts by their vCard UIDs with a single
    /// indexed query (`uid = ANY(...)`) — the CardDAV multiget REPORT
    /// must use this instead of listing the whole address book and
    /// filtering client-side. UIDs without a matching contact are
    /// silently absent from the result.
    async fn get_contacts_by_uids(
        &self,
        address_book_id: &str,
        uids: &[String],
        user_id: Uuid,
    ) -> Result<Vec<ContactDto>, DomainError>;
    /// List contacts in an address book. `limit`/`offset` bound the
    /// result for paginated callers (REST API); `None` returns the full
    /// book, which the CardDAV listing/sync paths rely on.
    /// Streaming support: cursor over the book's contacts (same Read
    /// gate as [`Self::list_contacts`], checked once before the cursor
    /// opens).
    async fn stream_contacts_by_book(
        &self,
        address_book_id: &str,
        user_id: Uuid,
    ) -> Result<futures::stream::BoxStream<'static, Result<ContactDto, DomainError>>, DomainError>;

    async fn list_contacts(
        &self,
        address_book_id: &str,
        limit: Option<i64>,
        offset: Option<i64>,
        user_id: Uuid,
    ) -> Result<Vec<ContactDto>, DomainError>;
    async fn search_contacts(
        &self,
        address_book_id: &str,
        query: &str,
        user_id: Uuid,
    ) -> Result<Vec<ContactDto>, DomainError>;

    // Contact Group operations
    async fn create_group(
        &self,
        dto: CreateContactGroupDto,
    ) -> Result<ContactGroupDto, DomainError>;
    async fn update_group(
        &self,
        group_id: &str,
        update: UpdateContactGroupDto,
    ) -> Result<ContactGroupDto, DomainError>;
    async fn delete_group(&self, group_id: &str, user_id: Uuid) -> Result<(), DomainError>;
    async fn get_group(
        &self,
        group_id: &str,
        user_id: Uuid,
    ) -> Result<ContactGroupDto, DomainError>;
    async fn list_groups(
        &self,
        address_book_id: &str,
        user_id: Uuid,
    ) -> Result<Vec<ContactGroupDto>, DomainError>;

    // Group membership
    async fn add_contact_to_group(
        &self,
        dto: GroupMembershipDto,
        user_id: Uuid,
    ) -> Result<(), DomainError>;
    async fn remove_contact_from_group(
        &self,
        dto: GroupMembershipDto,
        user_id: Uuid,
    ) -> Result<(), DomainError>;
    async fn list_contacts_in_group(
        &self,
        group_id: &str,
        user_id: Uuid,
    ) -> Result<Vec<ContactDto>, DomainError>;
    async fn list_groups_for_contact(
        &self,
        contact_id: &str,
        user_id: Uuid,
    ) -> Result<Vec<ContactGroupDto>, DomainError>;

    // vCard operations
    async fn get_contact_vcard(
        &self,
        contact_id: &str,
        user_id: Uuid,
    ) -> Result<String, DomainError>;
    async fn get_contacts_as_vcards(
        &self,
        address_book_id: &str,
        user_id: Uuid,
    ) -> Result<Vec<(String, String)>, DomainError>;
}
