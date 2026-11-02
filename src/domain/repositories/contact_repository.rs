use std::result::Result;
use uuid::Uuid;

use crate::common::errors::DomainError;
use crate::domain::entities::contact::{Contact, ContactGroup};

pub type ContactRepositoryResult<T> = Result<T, DomainError>;

pub trait ContactRepository: Send + Sync + 'static {
    async fn create_contact(&self, contact: Contact) -> ContactRepositoryResult<Contact>;
    async fn update_contact(&self, contact: Contact) -> ContactRepositoryResult<Contact>;
    async fn delete_contact(&self, id: &Uuid) -> ContactRepositoryResult<()>;
    async fn get_contact_by_id(&self, id: &Uuid) -> ContactRepositoryResult<Option<Contact>>;
    async fn get_contact_by_uid(
        &self,
        address_book_id: &Uuid,
        uid: &str,
    ) -> ContactRepositoryResult<Option<Contact>>;
    /// Fetches the contacts matching any of the given vCard UIDs in one
    /// indexed query (`uid = ANY(...)`). Used by CardDAV multiget so a
    /// request for a handful of contacts never pays for the whole book.
    /// UIDs with no matching contact are silently absent from the result.
    async fn get_contacts_by_uids(
        &self,
        address_book_id: &Uuid,
        uids: &[String],
    ) -> ContactRepositoryResult<Vec<Contact>>;
    /// Cursor stream over every contact of the book in the listing
    /// order (`full_name, first_name, last_name`) — ONE scan+sort on
    /// the server; the streaming CardDAV emitters page over it.
    fn stream_contacts_by_book(
        &self,
        address_book_id: Uuid,
    ) -> futures::stream::BoxStream<'static, ContactRepositoryResult<Contact>>;

    async fn get_contacts_by_address_book(
        &self,
        address_book_id: &Uuid,
    ) -> ContactRepositoryResult<Vec<Contact>>;
    /// Same as [`Self::get_contacts_by_address_book`] but bounded by
    /// `LIMIT`/`OFFSET` for paginated listings.
    async fn get_contacts_by_address_book_paginated(
        &self,
        address_book_id: &Uuid,
        limit: i64,
        offset: i64,
    ) -> ContactRepositoryResult<Vec<Contact>>;
    async fn get_contacts_by_email(&self, email: &str) -> ContactRepositoryResult<Vec<Contact>>;
    async fn get_contacts_by_group(&self, group_id: &Uuid)
    -> ContactRepositoryResult<Vec<Contact>>;
    /// Trigram-indexed substring search over the contact's name, nickname,
    /// organisation, e-mail and phone fields, bounded by `LIMIT`/`OFFSET`
    /// so a REST search never materialises an entire address book.
    async fn search_contacts(
        &self,
        address_book_id: &Uuid,
        query: &str,
        limit: i64,
        offset: i64,
    ) -> ContactRepositoryResult<Vec<Contact>>;
}

pub trait ContactGroupRepository: Send + Sync + 'static {
    async fn create_group(&self, group: ContactGroup) -> ContactRepositoryResult<ContactGroup>;
    async fn update_group(&self, group: ContactGroup) -> ContactRepositoryResult<ContactGroup>;
    async fn delete_group(&self, id: &Uuid) -> ContactRepositoryResult<()>;
    async fn get_group_by_id(&self, id: &Uuid) -> ContactRepositoryResult<Option<ContactGroup>>;
    async fn get_groups_by_address_book(
        &self,
        address_book_id: &Uuid,
    ) -> ContactRepositoryResult<Vec<ContactGroup>>;
    async fn add_contact_to_group(
        &self,
        group_id: &Uuid,
        contact_id: &Uuid,
    ) -> ContactRepositoryResult<()>;
    async fn remove_contact_from_group(
        &self,
        group_id: &Uuid,
        contact_id: &Uuid,
    ) -> ContactRepositoryResult<()>;
    async fn get_contacts_in_group(&self, group_id: &Uuid)
    -> ContactRepositoryResult<Vec<Contact>>;
    /// Membership count only — for group summaries that don't need the
    /// contacts hydrated (each row carries the full vCard TEXT plus three
    /// JSONB arrays; counting must not pay for any of that).
    async fn count_contacts_in_group(&self, group_id: &Uuid) -> ContactRepositoryResult<i64>;
    async fn get_groups_for_contact(
        &self,
        contact_id: &Uuid,
    ) -> ContactRepositoryResult<Vec<ContactGroup>>;
}
