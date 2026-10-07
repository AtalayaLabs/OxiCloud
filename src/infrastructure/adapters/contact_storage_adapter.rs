//! Contact Storage Adapter
//!
//! Implements [`ContactStoragePort`] using the three PostgreSQL
//! repositories (`AddressBookPgRepository`, `ContactPgRepository`,
//! `ContactGroupPgRepository`).
//!
//! **Pure storage port.** No access-control logic, no sharing state,
//! no owner-vs-shared listing carve-outs — every method is plain
//! delegation to a repository. Access decisions live in
//! `AuthorizationEngine`; sharing state lives in
//! `storage.role_grants`. The service layer (`ContactService`) gates
//! each call before reaching through this port.
//!
//! Symmetric with `CalendarStorageAdapter`. Post-Round-3 the
//! pre-existing 1000-line adapter that mixed the use-case impls +
//! bespoke `check_address_book_access` was deleted; this file
//! recreates a much smaller storage-only version.

use std::sync::Arc;
use uuid::Uuid;

use crate::application::ports::carddav_ports::ContactStoragePort;
use crate::common::errors::DomainError;
use crate::domain::entities::contact::{AddressBook, Contact, ContactGroup};
use crate::domain::repositories::address_book_repository::AddressBookRepository;
use crate::domain::repositories::contact_repository::{ContactGroupRepository, ContactRepository};
use crate::infrastructure::repositories::pg::{
    AddressBookPgRepository, ContactGroupPgRepository, ContactPgRepository,
};

/// Storage-port adapter bundling the three CardDAV PG repositories.
///
/// Wired in DI once; passed to `ContactService` which layers authz
/// on top and exposes the `AddressBookUseCase` / `ContactUseCase`
/// trait impls the HTTP handlers consume.
pub struct ContactStorageAdapter {
    address_book_repository: Arc<AddressBookPgRepository>,
    contact_repository: Arc<ContactPgRepository>,
    contact_group_repository: Arc<ContactGroupPgRepository>,
}

impl ContactStorageAdapter {
    pub fn new(
        address_book_repository: Arc<AddressBookPgRepository>,
        contact_repository: Arc<ContactPgRepository>,
        contact_group_repository: Arc<ContactGroupPgRepository>,
    ) -> Self {
        Self {
            address_book_repository,
            contact_repository,
            contact_group_repository,
        }
    }
}

impl ContactStoragePort for ContactStorageAdapter {
    // ── Address books ────────────────────────────────────────────

    async fn create_address_book(
        &self,
        address_book: AddressBook,
    ) -> Result<AddressBook, DomainError> {
        self.address_book_repository
            .create_address_book(address_book)
            .await
    }

    async fn update_address_book(
        &self,
        address_book: AddressBook,
    ) -> Result<AddressBook, DomainError> {
        self.address_book_repository
            .update_address_book(address_book)
            .await
    }

    async fn delete_address_book(&self, id: &Uuid) -> Result<(), DomainError> {
        self.address_book_repository.delete_address_book(id).await
    }

    async fn get_address_book_by_id(&self, id: &Uuid) -> Result<Option<AddressBook>, DomainError> {
        self.address_book_repository
            .get_address_book_by_id(id)
            .await
    }

    async fn get_address_books_by_ids(
        &self,
        ids: &[Uuid],
    ) -> Result<Vec<AddressBook>, DomainError> {
        self.address_book_repository
            .get_address_books_by_ids(ids)
            .await
    }

    // ── Contacts ─────────────────────────────────────────────────

    async fn create_contact(&self, contact: Contact) -> Result<Contact, DomainError> {
        self.contact_repository.create_contact(contact).await
    }

    async fn update_contact(&self, contact: Contact) -> Result<Contact, DomainError> {
        self.contact_repository.update_contact(contact).await
    }

    async fn delete_contact(&self, id: &Uuid) -> Result<(), DomainError> {
        self.contact_repository.delete_contact(id).await
    }

    async fn get_contact_by_id(&self, id: &Uuid) -> Result<Option<Contact>, DomainError> {
        self.contact_repository.get_contact_by_id(id).await
    }

    async fn get_contact_by_uid(
        &self,
        address_book_id: &Uuid,
        uid: &str,
    ) -> Result<Option<Contact>, DomainError> {
        self.contact_repository
            .get_contact_by_uid(address_book_id, uid)
            .await
    }

    async fn get_contacts_by_uids(
        &self,
        address_book_id: &Uuid,
        uids: &[String],
    ) -> Result<Vec<Contact>, DomainError> {
        self.contact_repository
            .get_contacts_by_uids(address_book_id, uids)
            .await
    }

    async fn get_contacts_by_address_book(
        &self,
        address_book_id: &Uuid,
    ) -> Result<Vec<Contact>, DomainError> {
        self.contact_repository
            .get_contacts_by_address_book(address_book_id)
            .await
    }

    fn stream_contacts_by_book(
        &self,
        address_book_id: Uuid,
    ) -> futures::stream::BoxStream<'static, Result<Contact, DomainError>> {
        self.contact_repository
            .stream_contacts_by_book(address_book_id)
    }

    async fn get_contacts_by_address_book_paginated(
        &self,
        address_book_id: &Uuid,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<Contact>, DomainError> {
        self.contact_repository
            .get_contacts_by_address_book_paginated(address_book_id, limit, offset)
            .await
    }

    async fn search_contacts(
        &self,
        address_book_id: &Uuid,
        query: &str,
    ) -> Result<Vec<Contact>, DomainError> {
        self.contact_repository
            .search_contacts(address_book_id, query)
            .await
    }

    // ── Contact groups ───────────────────────────────────────────

    async fn create_group(&self, group: ContactGroup) -> Result<ContactGroup, DomainError> {
        self.contact_group_repository.create_group(group).await
    }

    async fn update_group(&self, group: ContactGroup) -> Result<ContactGroup, DomainError> {
        self.contact_group_repository.update_group(group).await
    }

    async fn delete_group(&self, id: &Uuid) -> Result<(), DomainError> {
        self.contact_group_repository.delete_group(id).await
    }

    async fn get_group_by_id(&self, id: &Uuid) -> Result<Option<ContactGroup>, DomainError> {
        self.contact_group_repository.get_group_by_id(id).await
    }

    async fn get_groups_by_address_book(
        &self,
        address_book_id: &Uuid,
    ) -> Result<Vec<ContactGroup>, DomainError> {
        self.contact_group_repository
            .get_groups_by_address_book(address_book_id)
            .await
    }

    // ── Group membership ─────────────────────────────────────────

    async fn add_contact_to_group(
        &self,
        group_id: &Uuid,
        contact_id: &Uuid,
    ) -> Result<(), DomainError> {
        self.contact_group_repository
            .add_contact_to_group(group_id, contact_id)
            .await
    }

    async fn remove_contact_from_group(
        &self,
        group_id: &Uuid,
        contact_id: &Uuid,
    ) -> Result<(), DomainError> {
        self.contact_group_repository
            .remove_contact_from_group(group_id, contact_id)
            .await
    }

    async fn get_contacts_in_group(&self, group_id: &Uuid) -> Result<Vec<Contact>, DomainError> {
        self.contact_group_repository
            .get_contacts_in_group(group_id)
            .await
    }

    async fn count_contacts_in_group(&self, group_id: &Uuid) -> Result<i64, DomainError> {
        self.contact_group_repository
            .count_contacts_in_group(group_id)
            .await
    }

    async fn get_groups_for_contact(
        &self,
        contact_id: &Uuid,
    ) -> Result<Vec<ContactGroup>, DomainError> {
        self.contact_group_repository
            .get_groups_for_contact(contact_id)
            .await
    }
}
