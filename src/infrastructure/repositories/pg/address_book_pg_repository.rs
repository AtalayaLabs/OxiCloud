use chrono::Utc;
use sqlx::{PgPool, Row, types::Uuid};
use std::sync::Arc;

use crate::common::errors::DomainError;
use crate::domain::entities::contact::AddressBook;
use crate::domain::repositories::address_book_repository::{
    AddressBookRepository, AddressBookRepositoryResult,
};
use crate::domain::services::path_service::normalize_storage_name;

pub struct AddressBookPgRepository {
    pool: Arc<PgPool>,
}

impl AddressBookPgRepository {
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool }
    }

    /// `EXISTS` short-circuit for the login provisioning hook — the old
    /// `get_address_books_by_owner(..).is_empty()` hydrated every owned
    /// `AddressBook` row on EVERY login just to test emptiness (the ROUND9
    /// §7 COUNT→EXISTS pattern; benches/ROUND13.md §Q2).
    pub async fn has_owned_address_book(&self, owner_id: Uuid) -> Result<bool, DomainError> {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM carddav.address_books WHERE owner_id = $1)",
        )
        .bind(owner_id)
        .fetch_one(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("Failed to probe owned address books: {}", e))
        })?;
        Ok(exists)
    }
}

impl AddressBookRepository for AddressBookPgRepository {
    async fn create_address_book(
        &self,
        address_book: AddressBook,
    ) -> AddressBookRepositoryResult<AddressBook> {
        // NFC-normalize the caller-supplied display name at the last
        // touch before bind. Same choke-point pattern as the storage.*
        // repos — the entity constructor's normalization is bypassed by
        // every real production path (`AddressBook::from_raw`
        // reconstructs from DB bytes; `AddressBook::new` goes through
        // an inbound DTO that may or may not have been touched).
        // Enforcing here means every carddav write surface — DAV
        // `MKCOL`, REST create — lands in NFC regardless.
        let normalized_name = normalize_storage_name(address_book.name());
        let row = sqlx::query(
            r#"
            INSERT INTO carddav.address_books (id, name, owner_id, description, color, is_public, created_at, updated_at)
            VALUES ($1, $2, $3::uuid, $4, $5, $6, $7, $8)
            RETURNING id, name, owner_id, description, color, is_public, created_at, updated_at
            "#
        )
        .bind(address_book.id())
        .bind(&normalized_name)
        .bind(address_book.owner_id())
        .bind(address_book.description())
        .bind(address_book.color())
        .bind(address_book.is_public())
        .bind(address_book.created_at())
        .bind(address_book.updated_at())
        .fetch_one(&*self.pool)
        .await
        .map_err(|e| DomainError::database_error(format!("Failed to create address book: {}", e)))?;

        let owner_id: Uuid = row.get("owner_id");
        Ok(AddressBook::from_raw(
            row.get("id"),
            row.get("name"),
            owner_id.to_string(),
            row.get("description"),
            row.get("color"),
            row.get("is_public"),
            row.get("created_at"),
            row.get("updated_at"),
        ))
    }

    async fn update_address_book(
        &self,
        address_book: AddressBook,
    ) -> AddressBookRepositoryResult<AddressBook> {
        // NFC-normalize on rename — see `create_address_book` for the why.
        let normalized_name = normalize_storage_name(address_book.name());
        let now = Utc::now();
        let row = sqlx::query(
            r#"
            UPDATE carddav.address_books
            SET name = $1, description = $2, color = $3, is_public = $4, updated_at = $5
            WHERE id = $6
            RETURNING id, name, owner_id, description, color, is_public, created_at, updated_at
            "#,
        )
        .bind(&normalized_name)
        .bind(address_book.description())
        .bind(address_book.color())
        .bind(address_book.is_public())
        .bind(now)
        .bind(address_book.id())
        .fetch_one(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("Failed to update address book: {}", e))
        })?;

        let owner_id: Uuid = row.get("owner_id");
        Ok(AddressBook::from_raw(
            row.get("id"),
            row.get("name"),
            owner_id.to_string(),
            row.get("description"),
            row.get("color"),
            row.get("is_public"),
            row.get("created_at"),
            row.get("updated_at"),
        ))
    }

    async fn delete_address_book(&self, id: &Uuid) -> AddressBookRepositoryResult<()> {
        sqlx::query(
            r#"
            DELETE FROM carddav.address_books
            WHERE id = $1
            "#,
        )
        .bind(id)
        .execute(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("Failed to delete address book: {}", e))
        })?;

        Ok(())
    }

    async fn get_address_books_by_ids(
        &self,
        ids: &[Uuid],
    ) -> AddressBookRepositoryResult<Vec<AddressBook>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            r#"
            SELECT id, name, owner_id, description, color, is_public, created_at, updated_at
            FROM carddav.address_books
            WHERE id = ANY($1)
            "#,
        )
        .bind(ids)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("Failed to get address books by ids: {}", e))
        })?;

        Ok(rows
            .iter()
            .map(|row| {
                let owner_id: Uuid = row.get("owner_id");
                AddressBook::from_raw(
                    row.get("id"),
                    row.get("name"),
                    owner_id.to_string(),
                    row.get("description"),
                    row.get("color"),
                    row.get("is_public"),
                    row.get("created_at"),
                    row.get("updated_at"),
                )
            })
            .collect())
    }

    async fn get_address_book_by_id(
        &self,
        id: &Uuid,
    ) -> AddressBookRepositoryResult<Option<AddressBook>> {
        let maybe_row = sqlx::query(
            r#"
            SELECT id, name, owner_id, description, color, is_public, created_at, updated_at
            FROM carddav.address_books
            WHERE id = $1
            "#,
        )
        .bind(id)
        .fetch_optional(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("Failed to get address book by id: {}", e))
        })?;

        let result = maybe_row.map(|row| {
            let owner_id: Uuid = row.get("owner_id");
            AddressBook::from_raw(
                row.get("id"),
                row.get("name"),
                owner_id.to_string(),
                row.get("description"),
                row.get("color"),
                row.get("is_public"),
                row.get("created_at"),
                row.get("updated_at"),
            )
        });

        Ok(result)
    }

    async fn get_address_books_by_owner(
        &self,
        owner_id: Uuid,
    ) -> AddressBookRepositoryResult<Vec<AddressBook>> {
        let rows = sqlx::query(
            r#"
            SELECT id, name, owner_id, description, color, is_public, created_at, updated_at
            FROM carddav.address_books
            WHERE owner_id = $1
            ORDER BY name
            "#,
        )
        .bind(owner_id)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("Failed to get address books by owner: {}", e))
        })?;

        let result = rows
            .into_iter()
            .map(|row| {
                let owner_id: Uuid = row.get("owner_id");
                AddressBook::from_raw(
                    row.get("id"),
                    row.get("name"),
                    owner_id.to_string(),
                    row.get("description"),
                    row.get("color"),
                    row.get("is_public"),
                    row.get("created_at"),
                    row.get("updated_at"),
                )
            })
            .collect();

        Ok(result)
    }

    async fn get_public_address_books(&self) -> AddressBookRepositoryResult<Vec<AddressBook>> {
        let rows = sqlx::query(
            r#"
            SELECT id, name, owner_id, description, color, is_public, created_at, updated_at
            FROM carddav.address_books
            WHERE is_public = true
            ORDER BY name
            "#,
        )
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("Failed to get public address books: {}", e))
        })?;

        let result = rows
            .into_iter()
            .map(|row| {
                let owner_id: Uuid = row.get("owner_id");
                AddressBook::from_raw(
                    row.get("id"),
                    row.get("name"),
                    owner_id.to_string(),
                    row.get("description"),
                    row.get("color"),
                    row.get("is_public"),
                    row.get("created_at"),
                    row.get("updated_at"),
                )
            })
            .collect();

        Ok(result)
    }
}
