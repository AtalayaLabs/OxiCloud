use chrono::Utc;
use sqlx::{PgPool, Row, types::Uuid};
use std::sync::Arc;

use super::contact_persistence_dto::{
    AddressPersistenceDto, EmailPersistenceDto, PhonePersistenceDto, addresses_from_persistence,
    addresses_to_persistence, emails_from_persistence, emails_to_persistence,
    phones_from_persistence, phones_to_persistence,
};
use crate::common::errors::DomainError;
use crate::domain::entities::contact::Contact;
use crate::domain::repositories::contact_repository::{ContactRepository, ContactRepositoryResult};

pub struct ContactPgRepository {
    pool: Arc<PgPool>,
}

impl ContactPgRepository {
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool }
    }

    /// Maps a database row to a Contact domain entity (reads the `vcard` column).
    fn row_to_contact(row: &sqlx::postgres::PgRow) -> Result<Contact, DomainError> {
        Self::row_to_contact_with_vcard(row, row.get("vcard"))
    }

    /// Maps a row whose SELECT omitted the `vcard` column — used by the REST
    /// listings (paginated / search / by-group) whose `ContactDto` drops vcard
    /// anyway, so the multi-KB vCard TEXT (which can embed a base64 PHOTO) is
    /// never SELECTed, shipped over the wire, or allocated (benches/ROUND25.md
    /// §Q2). The domain `Contact` keeps an empty vcard; these paths never
    /// re-emit it. Do NOT use for CardDAV sync / whole-book export, which need
    /// the round-trip vCard.
    fn row_to_contact_lite(row: &sqlx::postgres::PgRow) -> Result<Contact, DomainError> {
        Self::row_to_contact_with_vcard(row, String::new())
    }

    /// Shared row → `Contact` mapper; `vcard` is supplied by the caller so the
    /// TEXT column can be omitted from listings that don't consume it.
    fn row_to_contact_with_vcard(
        row: &sqlx::postgres::PgRow,
        vcard: String,
    ) -> Result<Contact, DomainError> {
        // Decode each JSONB column straight into its typed Vec via
        // `sqlx::types::Json<T>` (a single `serde_json::from_slice` pass over
        // the raw JSONB bytes) instead of `row.get::<serde_json::Value>` +
        // `serde_json::from_value`, which built a throwaway `Value` DOM per
        // column and then walked it a SECOND time to produce the typed Vec —
        // 3 discarded DOMs per contact row on every list / multiget / CardDAV
        // sync. `try_get` preserves the exact malformed-shape fallback (the old
        // `from_value(...).unwrap_or_default()`; a bare `row.get` would panic on
        // a decode error); the columns are `JSONB NOT NULL DEFAULT '[]'`, so SQL
        // NULL never occurs. (benches/ROUND23.md §J1)
        let emails = row
            .try_get::<sqlx::types::Json<Vec<EmailPersistenceDto>>, _>("email")
            .map(|j| emails_from_persistence(j.0))
            .unwrap_or_default();
        let phones = row
            .try_get::<sqlx::types::Json<Vec<PhonePersistenceDto>>, _>("phone")
            .map(|j| phones_from_persistence(j.0))
            .unwrap_or_default();
        let addresses = row
            .try_get::<sqlx::types::Json<Vec<AddressPersistenceDto>>, _>("address")
            .map(|j| addresses_from_persistence(j.0))
            .unwrap_or_default();

        Ok(Contact::from_raw(
            row.get("id"),
            row.get("address_book_id"),
            row.get("uid"),
            row.get::<Option<String>, _>("full_name"),
            row.get::<Option<String>, _>("first_name"),
            row.get::<Option<String>, _>("last_name"),
            row.get::<Option<String>, _>("nickname"),
            emails,
            phones,
            addresses,
            row.get::<Option<String>, _>("organization"),
            row.get::<Option<String>, _>("title"),
            row.get::<Option<String>, _>("notes"),
            row.get::<Option<String>, _>("photo_url"),
            row.get("birthday"),
            row.get("anniversary"),
            vcard,
            row.get("etag"),
            row.get("created_at"),
            row.get("updated_at"),
        ))
    }
}

impl ContactRepository for ContactPgRepository {
    async fn create_contact(&self, contact: Contact) -> ContactRepositoryResult<Contact> {
        // Convert domain entities to persistence DTOs for JSONB serialization
        let email_dtos = emails_to_persistence(contact.email());
        let phone_dtos = phones_to_persistence(contact.phone());
        let address_dtos = addresses_to_persistence(contact.address());

        let row = sqlx::query(
            r#"
            INSERT INTO carddav.contacts (
                id, address_book_id, uid, full_name, first_name, last_name, nickname,
                email, phone, address, organization, title, notes, photo_url,
                birthday, anniversary, vcard, etag, created_at, updated_at
            )
            VALUES (
                $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14,
                $15, $16, $17, $18, $19, $20
            )
            RETURNING 
                id, address_book_id, uid, full_name, first_name, last_name, nickname,
                email, phone, address, organization, title, notes, photo_url,
                birthday, anniversary, vcard, etag, created_at, updated_at
            "#,
        )
        .bind(contact.id())
        .bind(contact.address_book_id())
        .bind(contact.uid())
        .bind(contact.full_name_owned())
        .bind(contact.first_name_owned())
        .bind(contact.last_name_owned())
        .bind(contact.nickname_owned())
        .bind(sqlx::types::Json(&email_dtos))
        .bind(sqlx::types::Json(&phone_dtos))
        .bind(sqlx::types::Json(&address_dtos))
        .bind(contact.organization_owned())
        .bind(contact.title_owned())
        .bind(contact.notes_owned())
        .bind(contact.photo_url_owned())
        .bind(contact.birthday().copied())
        .bind(contact.anniversary().copied())
        .bind(contact.vcard())
        .bind(contact.etag())
        .bind(*contact.created_at())
        .bind(*contact.updated_at())
        .fetch_one(&*self.pool)
        .await
        .map_err(|e| DomainError::database_error(format!("Failed to create contact: {}", e)))?;

        Self::row_to_contact(&row)
    }

    async fn update_contact(&self, contact: Contact) -> ContactRepositoryResult<Contact> {
        let now = Utc::now();
        // Convert domain entities to persistence DTOs for JSONB serialization
        let email_dtos = emails_to_persistence(contact.email());
        let phone_dtos = phones_to_persistence(contact.phone());
        let address_dtos = addresses_to_persistence(contact.address());

        // Create a clone of the contact with the updated timestamp
        let mut updated_contact = contact.clone();
        updated_contact.set_updated_at(now);

        let row = sqlx::query(
            r#"
            UPDATE carddav.contacts
            SET 
                full_name = $1,
                first_name = $2,
                last_name = $3,
                nickname = $4,
                email = $5,
                phone = $6,
                address = $7,
                organization = $8,
                title = $9,
                notes = $10,
                photo_url = $11,
                birthday = $12,
                anniversary = $13,
                vcard = $14,
                etag = $15,
                updated_at = $16
            WHERE id = $17
            RETURNING 
                id, address_book_id, uid, full_name, first_name, last_name, nickname,
                email, phone, address, organization, title, notes, photo_url,
                birthday, anniversary, vcard, etag, created_at, updated_at
            "#,
        )
        .bind(updated_contact.full_name_owned())
        .bind(updated_contact.first_name_owned())
        .bind(updated_contact.last_name_owned())
        .bind(updated_contact.nickname_owned())
        .bind(sqlx::types::Json(&email_dtos))
        .bind(sqlx::types::Json(&phone_dtos))
        .bind(sqlx::types::Json(&address_dtos))
        .bind(updated_contact.organization_owned())
        .bind(updated_contact.title_owned())
        .bind(updated_contact.notes_owned())
        .bind(updated_contact.photo_url_owned())
        .bind(updated_contact.birthday().copied())
        .bind(updated_contact.anniversary().copied())
        .bind(updated_contact.vcard())
        .bind(updated_contact.etag())
        .bind(now)
        .bind(updated_contact.id())
        .fetch_one(&*self.pool)
        .await
        .map_err(|e| DomainError::database_error(format!("Failed to update contact: {}", e)))?;

        Self::row_to_contact(&row)
    }

    async fn delete_contact(&self, id: &Uuid) -> ContactRepositoryResult<()> {
        sqlx::query(
            r#"
            DELETE FROM carddav.contacts
            WHERE id = $1
            "#,
        )
        .bind(id)
        .execute(&*self.pool)
        .await
        .map_err(|e| DomainError::database_error(format!("Failed to delete contact: {}", e)))?;

        Ok(())
    }

    async fn get_contact_by_id(&self, id: &Uuid) -> ContactRepositoryResult<Option<Contact>> {
        let row_opt = sqlx::query(
            r#"
            SELECT 
                id, address_book_id, uid, full_name, first_name, last_name, nickname,
                email, phone, address, organization, title, notes, photo_url,
                birthday, anniversary, vcard, etag, created_at, updated_at
            FROM carddav.contacts
            WHERE id = $1
            "#,
        )
        .bind(id)
        .fetch_optional(&*self.pool)
        .await
        .map_err(|e| DomainError::database_error(format!("Failed to get contact by id: {}", e)))?;

        match row_opt {
            Some(row) => Ok(Some(Self::row_to_contact(&row)?)),
            None => Ok(None),
        }
    }

    async fn get_contact_by_uid(
        &self,
        address_book_id: &Uuid,
        uid: &str,
    ) -> ContactRepositoryResult<Option<Contact>> {
        let row_opt = sqlx::query(
            r#"
            SELECT 
                id, address_book_id, uid, full_name, first_name, last_name, nickname,
                email, phone, address, organization, title, notes, photo_url,
                birthday, anniversary, vcard, etag, created_at, updated_at
            FROM carddav.contacts
            WHERE address_book_id = $1 AND uid = $2
            "#,
        )
        .bind(address_book_id)
        .bind(uid)
        .fetch_optional(&*self.pool)
        .await
        .map_err(|e| DomainError::database_error(format!("Failed to get contact by uid: {}", e)))?;

        match row_opt {
            Some(row) => Ok(Some(Self::row_to_contact(&row)?)),
            None => Ok(None),
        }
    }

    async fn get_contacts_by_uids(
        &self,
        address_book_id: &Uuid,
        uids: &[String],
    ) -> ContactRepositoryResult<Vec<Contact>> {
        let rows = sqlx::query(
            r#"
            SELECT
                id, address_book_id, uid, full_name, first_name, last_name, nickname,
                email, phone, address, organization, title, notes, photo_url,
                birthday, anniversary, vcard, etag, created_at, updated_at
            FROM carddav.contacts
            WHERE address_book_id = $1 AND uid = ANY($2)
            ORDER BY full_name, first_name, last_name
            "#,
        )
        .bind(address_book_id)
        .bind(uids)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("Failed to get contacts by uids: {}", e))
        })?;

        let mut contacts = Vec::with_capacity(rows.len());
        for row in &rows {
            contacts.push(Self::row_to_contact(row)?);
        }
        Ok(contacts)
    }

    fn stream_contacts_by_book(
        &self,
        address_book_id: Uuid,
    ) -> futures::stream::BoxStream<'static, ContactRepositoryResult<Contact>> {
        // ONE ordered scan served through a PG cursor — the CardDAV
        // multistatus emitters page over this stream so only a page of
        // contacts is resident (same design as the CalDAV round-5
        // cursor; contacts have no master/exception bundling, so pages
        // can cut anywhere).
        let pool = self.pool.clone();
        let stream: futures::stream::BoxStream<'static, ContactRepositoryResult<Contact>> =
            Box::pin(async_stream::try_stream! {
                let mut conn = pool.acquire().await.map_err(|e| {
                    DomainError::database_error(format!("Failed to acquire connection: {}", e))
                })?;
                let mut rows = sqlx::query(
                    r#"
                    SELECT
                        id, address_book_id, uid, full_name, first_name, last_name, nickname,
                        email, phone, address, organization, title, notes, photo_url,
                        birthday, anniversary, vcard, etag, created_at, updated_at
                    FROM carddav.contacts
                    WHERE address_book_id = $1
                    ORDER BY full_name, first_name, last_name
                    "#,
                )
                .bind(address_book_id)
                .fetch(&mut *conn);

                use futures::TryStreamExt;
                while let Some(row) = rows.try_next().await.map_err(|e| {
                    DomainError::database_error(format!("Failed to stream contacts: {}", e))
                })? {
                    yield Self::row_to_contact(&row)?;
                }
            });
        stream
    }

    async fn get_contacts_by_address_book(
        &self,
        address_book_id: &Uuid,
    ) -> ContactRepositoryResult<Vec<Contact>> {
        let rows = sqlx::query(
            r#"
            SELECT
                id, address_book_id, uid, full_name, first_name, last_name, nickname,
                email, phone, address, organization, title, notes, photo_url,
                birthday, anniversary, vcard, etag, created_at, updated_at
            FROM carddav.contacts
            WHERE address_book_id = $1
            ORDER BY full_name, first_name, last_name
            "#,
        )
        .bind(address_book_id)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("Failed to get contacts by address book: {}", e))
        })?;

        let mut contacts = Vec::with_capacity(rows.len());
        for row in &rows {
            contacts.push(Self::row_to_contact(row)?);
        }
        Ok(contacts)
    }

    async fn get_contacts_by_address_book_paginated(
        &self,
        address_book_id: &Uuid,
        limit: i64,
        offset: i64,
    ) -> ContactRepositoryResult<Vec<Contact>> {
        let rows = sqlx::query(
            r#"
            SELECT
                id, address_book_id, uid, full_name, first_name, last_name, nickname,
                email, phone, address, organization, title, notes, photo_url,
                birthday, anniversary, etag, created_at, updated_at
            FROM carddav.contacts
            WHERE address_book_id = $1
            ORDER BY full_name, first_name, last_name
            LIMIT $2 OFFSET $3
            "#,
        )
        .bind(address_book_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!(
                "Failed to get contacts by address book (paginated): {}",
                e
            ))
        })?;

        let mut contacts = Vec::with_capacity(rows.len());
        for row in &rows {
            contacts.push(Self::row_to_contact_lite(row)?);
        }
        Ok(contacts)
    }

    async fn get_contacts_by_email(&self, email: &str) -> ContactRepositoryResult<Vec<Contact>> {
        let search_pattern = super::like_escape(email);

        let rows = sqlx::query(
            r#"
            SELECT 
                id, address_book_id, uid, full_name, first_name, last_name, nickname,
                email, phone, address, organization, title, notes, photo_url,
                birthday, anniversary, vcard, etag, created_at, updated_at
            FROM carddav.contacts
            WHERE email::text ILIKE $1
            ORDER BY full_name, first_name, last_name
            "#,
        )
        .bind(&search_pattern)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("Failed to get contacts by email: {}", e))
        })?;

        let mut contacts = Vec::with_capacity(rows.len());
        for row in &rows {
            contacts.push(Self::row_to_contact(row)?);
        }
        Ok(contacts)
    }

    async fn get_contacts_by_group(
        &self,
        group_id: &Uuid,
    ) -> ContactRepositoryResult<Vec<Contact>> {
        let rows = sqlx::query(
            r#"
            SELECT 
                c.id, c.address_book_id, c.uid, c.full_name, c.first_name, c.last_name, c.nickname,
                c.email, c.phone, c.address, c.organization, c.title, c.notes, c.photo_url,
                c.birthday, c.anniversary, c.etag, c.created_at, c.updated_at
            FROM carddav.contacts c
            INNER JOIN carddav.group_memberships m ON c.id = m.contact_id
            WHERE m.group_id = $1
            ORDER BY c.full_name, c.first_name, c.last_name
            "#,
        )
        .bind(group_id)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("Failed to get contacts by group: {}", e))
        })?;

        let mut contacts = Vec::with_capacity(rows.len());
        for row in &rows {
            contacts.push(Self::row_to_contact_lite(row)?);
        }
        Ok(contacts)
    }

    async fn search_contacts(
        &self,
        address_book_id: &Uuid,
        query: &str,
        limit: i64,
        offset: i64,
    ) -> ContactRepositoryResult<Vec<Contact>> {
        let search_pattern = super::like_escape(query);

        let rows = sqlx::query(
            r#"
            SELECT
                id, address_book_id, uid, full_name, first_name, last_name, nickname,
                email, phone, address, organization, title, notes, photo_url,
                birthday, anniversary, etag, created_at, updated_at
            FROM carddav.contacts
            WHERE address_book_id = $1
              AND (
                  full_name ILIKE $2
                  OR first_name ILIKE $2
                  OR last_name ILIKE $2
                  OR nickname ILIKE $2
                  OR email::text ILIKE $2
                  OR phone::text ILIKE $2
                  OR organization ILIKE $2
              )
            ORDER BY full_name, first_name, last_name
            LIMIT $3 OFFSET $4
            "#,
        )
        .bind(address_book_id)
        .bind(&search_pattern)
        .bind(limit)
        .bind(offset)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| DomainError::database_error(format!("Failed to search contacts: {}", e)))?;

        let mut contacts = Vec::with_capacity(rows.len());
        for row in &rows {
            contacts.push(Self::row_to_contact_lite(row)?);
        }
        Ok(contacts)
    }
}
