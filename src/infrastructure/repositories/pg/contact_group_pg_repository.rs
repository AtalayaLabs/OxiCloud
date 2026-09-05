use chrono::Utc;
use sqlx::{PgPool, Row, types::Uuid};
use std::sync::Arc;

use super::contact_persistence_dto::{
    AddressPersistenceDto, EmailPersistenceDto, PhonePersistenceDto, addresses_from_persistence,
    emails_from_persistence, phones_from_persistence,
};
use crate::common::errors::{DomainError, ErrorKind};
use crate::domain::entities::contact::{Contact, ContactGroup};
use crate::domain::repositories::contact_repository::{
    ContactGroupRepository, ContactRepositoryResult,
};
use crate::domain::services::path_service::normalize_storage_name;

pub struct ContactGroupPgRepository {
    pool: Arc<PgPool>,
}

impl ContactGroupPgRepository {
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool }
    }
}

impl ContactGroupRepository for ContactGroupPgRepository {
    async fn create_group(&self, group: ContactGroup) -> ContactRepositoryResult<ContactGroup> {
        // NFC-normalize at the last touch before bind — same choke-point
        // pattern as the storage.* / caldav.* / carddav.address_books
        // repos. Group display names on macOS Contacts sync as NFD
        // (Address Book pushes decomposed forms in vCard KIND=group);
        // NC-desktop / Thunderbird would then miss the group on their
        // NFC-normalized lookup.
        let normalized_name = normalize_storage_name(group.name());
        sqlx::query(
            "INSERT INTO carddav.contact_groups (id, address_book_id, name, created_at, updated_at) VALUES ($1, $2, $3, $4, $5)"
        )
        .bind(group.id())
        .bind(group.address_book_id())
        .bind(&normalized_name)
        .bind(group.created_at())
        .bind(group.updated_at())
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::new(ErrorKind::InternalError, "ContactGroup", format!("Failed to create group: {}", e)))?;

        Ok(group)
    }

    async fn update_group(&self, group: ContactGroup) -> ContactRepositoryResult<ContactGroup> {
        // NFC-normalize on rename — see `create_group` for the why.
        let normalized_name = normalize_storage_name(group.name());
        sqlx::query("UPDATE carddav.contact_groups SET name = $1, updated_at = $2 WHERE id = $3")
            .bind(&normalized_name)
            .bind(Utc::now())
            .bind(group.id())
            .execute(self.pool.as_ref())
            .await
            .map_err(|e| {
                DomainError::new(
                    ErrorKind::InternalError,
                    "ContactGroup",
                    format!("Failed to update group: {}", e),
                )
            })?;

        Ok(group)
    }

    async fn delete_group(&self, id: &Uuid) -> ContactRepositoryResult<()> {
        // Delete memberships first
        sqlx::query("DELETE FROM carddav.group_memberships WHERE group_id = $1")
            .bind(id)
            .execute(self.pool.as_ref())
            .await
            .map_err(|e| {
                DomainError::new(
                    ErrorKind::InternalError,
                    "ContactGroup",
                    format!("Failed to delete group memberships: {}", e),
                )
            })?;

        sqlx::query("DELETE FROM carddav.contact_groups WHERE id = $1")
            .bind(id)
            .execute(self.pool.as_ref())
            .await
            .map_err(|e| {
                DomainError::new(
                    ErrorKind::InternalError,
                    "ContactGroup",
                    format!("Failed to delete group: {}", e),
                )
            })?;

        Ok(())
    }

    async fn get_group_by_id(&self, id: &Uuid) -> ContactRepositoryResult<Option<ContactGroup>> {
        let row = sqlx::query(
            "SELECT id, address_book_id, name, created_at, updated_at FROM carddav.contact_groups WHERE id = $1"
        )
        .bind(id)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::new(ErrorKind::InternalError, "ContactGroup", format!("Failed to get group: {}", e)))?;

        match row {
            Some(row) => {
                let group = ContactGroup::from_raw(
                    row.get::<Uuid, _>("id"),
                    row.get::<Uuid, _>("address_book_id"),
                    row.get::<String, _>("name"),
                    row.get("created_at"),
                    row.get("updated_at"),
                );
                Ok(Some(group))
            }
            None => Ok(None),
        }
    }

    async fn get_groups_by_address_book(
        &self,
        address_book_id: &Uuid,
    ) -> ContactRepositoryResult<Vec<ContactGroup>> {
        let rows = sqlx::query(
            "SELECT id, address_book_id, name, created_at, updated_at FROM carddav.contact_groups WHERE address_book_id = $1 ORDER BY name"
        )
        .bind(address_book_id)
        .fetch_all(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::new(ErrorKind::InternalError, "ContactGroup", format!("Failed to list groups: {}", e)))?;

        Ok(rows
            .into_iter()
            .map(|row| {
                ContactGroup::from_raw(
                    row.get::<Uuid, _>("id"),
                    row.get::<Uuid, _>("address_book_id"),
                    row.get::<String, _>("name"),
                    row.get("created_at"),
                    row.get("updated_at"),
                )
            })
            .collect())
    }

    async fn add_contact_to_group(
        &self,
        group_id: &Uuid,
        contact_id: &Uuid,
    ) -> ContactRepositoryResult<()> {
        sqlx::query(
            "INSERT INTO carddav.group_memberships (group_id, contact_id) VALUES ($1, $2) ON CONFLICT DO NOTHING"
        )
        .bind(group_id)
        .bind(contact_id)
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::new(ErrorKind::InternalError, "ContactGroup", format!("Failed to add contact to group: {}", e)))?;

        Ok(())
    }

    async fn remove_contact_from_group(
        &self,
        group_id: &Uuid,
        contact_id: &Uuid,
    ) -> ContactRepositoryResult<()> {
        sqlx::query(
            "DELETE FROM carddav.group_memberships WHERE group_id = $1 AND contact_id = $2",
        )
        .bind(group_id)
        .bind(contact_id)
        .execute(self.pool.as_ref())
        .await
        .map_err(|e| {
            DomainError::new(
                ErrorKind::InternalError,
                "ContactGroup",
                format!("Failed to remove contact from group: {}", e),
            )
        })?;

        Ok(())
    }

    async fn count_contacts_in_group(&self, group_id: &Uuid) -> ContactRepositoryResult<i64> {
        sqlx::query_scalar("SELECT COUNT(*) FROM carddav.group_memberships WHERE group_id = $1")
            .bind(group_id)
            .fetch_one(self.pool.as_ref())
            .await
            .map_err(|e| {
                DomainError::new(
                    ErrorKind::InternalError,
                    "ContactGroup",
                    format!("Failed to count contacts in group: {}", e),
                )
            })
    }

    async fn get_contacts_in_group(
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
            INNER JOIN carddav.group_memberships gm ON c.id = gm.contact_id
            WHERE gm.group_id = $1
            ORDER BY c.full_name, c.first_name, c.last_name
            "#,
        )
        .bind(group_id)
        .fetch_all(self.pool.as_ref())
        .await
        .map_err(|e| {
            DomainError::new(
                ErrorKind::InternalError,
                "ContactGroup",
                format!("Failed to get contacts in group: {}", e),
            )
        })?;

        let mut contacts = Vec::with_capacity(rows.len());
        for row in &rows {
            // Typed `Json<T>` decode (one `from_slice` pass) instead of the
            // `Value` DOM + `from_value` re-walk — the contact_pg_repository
            // §J1 fix applied to this inlined sibling. Byte-identical result,
            // 3 fewer throwaway DOMs per contact. (benches/ROUND23.md §J1)
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

            contacts.push(Contact::from_raw(
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
                // vcard column intentionally NOT selected — the sole live caller
                // (`list_contacts_in_group`) maps to `ContactDto`, which has no
                // vcard field, so fetching the multi-KB serialized vCard (with an
                // embedded base64 PHOTO) only to drop it wastes bandwidth + a
                // per-row String. Mirrors `row_to_contact_lite` (benches/ROUND29.md
                // §F / ROUND25 §Q2, applied to the LIVE group method this time).
                String::new(),
                row.get("etag"),
                row.get("created_at"),
                row.get("updated_at"),
            ));
        }
        Ok(contacts)
    }

    async fn get_groups_for_contact(
        &self,
        contact_id: &Uuid,
    ) -> ContactRepositoryResult<Vec<ContactGroup>> {
        let rows = sqlx::query(
            "SELECT g.id, g.address_book_id, g.name, g.created_at, g.updated_at FROM carddav.contact_groups g INNER JOIN carddav.group_memberships gm ON g.id = gm.group_id WHERE gm.contact_id = $1 ORDER BY g.name"
        )
        .bind(contact_id)
        .fetch_all(self.pool.as_ref())
        .await
        .map_err(|e| DomainError::new(ErrorKind::InternalError, "ContactGroup", format!("Failed to get groups for contact: {}", e)))?;

        Ok(rows
            .into_iter()
            .map(|row| {
                ContactGroup::from_raw(
                    row.get::<Uuid, _>("id"),
                    row.get::<Uuid, _>("address_book_id"),
                    row.get::<String, _>("name"),
                    row.get("created_at"),
                    row.get("updated_at"),
                )
            })
            .collect())
    }
}
