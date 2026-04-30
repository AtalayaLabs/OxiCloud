use chrono::Utc;
use sqlx::{PgPool, Row, types::Uuid};
use std::sync::Arc;

use crate::common::errors::DomainError;
use crate::domain::entities::contact::AddressBook;
use crate::domain::repositories::address_book_repository::{
    AddressBookRepository, AddressBookRepositoryResult,
};

pub struct AddressBookPgRepository {
    pool: Arc<PgPool>,
}

impl AddressBookPgRepository {
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool }
    }
}

impl AddressBookRepository for AddressBookPgRepository {
    async fn create_address_book(
        &self,
        address_book: AddressBook,
    ) -> AddressBookRepositoryResult<AddressBook> {
        let row = sqlx::query(
            r#"
            INSERT INTO carddav.address_books (id, name, owner_id, description, color, is_public, created_at, updated_at)
            VALUES ($1, $2, $3::uuid, $4, $5, $6, $7, $8)
            RETURNING id, name, owner_id, description, color, is_public, created_at, updated_at
            "#
        )
        .bind(address_book.id())
        .bind(address_book.name())
        .bind(address_book.owner_id())
        .bind(address_book.description())
        .bind(address_book.color())
        .bind(address_book.is_public())
        .bind(address_book.created_at())
        .bind(address_book.updated_at())
        .fetch_one(&*self.pool)
        .await
        .map_err(|e| DomainError::database_error(format!("Failed to create address book: {}", e)))?;

        Ok(AddressBook::from_raw(
            row.get("id"),
            row.get("name"),
            row.get("owner_id"),
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
        let now = Utc::now();
        let row = sqlx::query(
            r#"
            UPDATE carddav.address_books
            SET name = $1, description = $2, color = $3, is_public = $4, updated_at = $5
            WHERE id = $6
            RETURNING id, name, owner_id, description, color, is_public, created_at, updated_at
            "#,
        )
        .bind(address_book.name())
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

        Ok(AddressBook::from_raw(
            row.get("id"),
            row.get("name"),
            row.get("owner_id"),
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
            AddressBook::from_raw(
                row.get("id"),
                row.get("name"),
                row.get("owner_id"),
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
                AddressBook::from_raw(
                    row.get("id"),
                    row.get("name"),
                    row.get("owner_id"),
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

    async fn get_shared_address_books(
        &self,
        user_id: Uuid,
    ) -> AddressBookRepositoryResult<Vec<AddressBook>> {
        let rows = sqlx::query(
            r#"
            SELECT a.id, a.name, a.owner_id, a.description, a.color, a.is_public, a.created_at, a.updated_at
            FROM carddav.address_books a
            INNER JOIN carddav.address_book_shares s ON a.id = s.address_book_id
            WHERE s.user_id = $1
            ORDER BY a.name
            "#
        )
        .bind(user_id)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| DomainError::database_error(format!("Failed to get shared address books: {}", e)))?;

        let result = rows
            .into_iter()
            .map(|row| {
                AddressBook::from_raw(
                    row.get("id"),
                    row.get("name"),
                    row.get("owner_id"),
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
                AddressBook::from_raw(
                    row.get("id"),
                    row.get("name"),
                    row.get("owner_id"),
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

    async fn share_address_book(
        &self,
        address_book_id: &Uuid,
        user_id: Uuid,
        can_write: bool,
    ) -> AddressBookRepositoryResult<()> {
        sqlx::query(
            r#"
            INSERT INTO carddav.address_book_shares (address_book_id, user_id, can_write)
            VALUES ($1, $2, $3)
            ON CONFLICT (address_book_id, user_id) DO UPDATE SET can_write = $3
            "#,
        )
        .bind(address_book_id)
        .bind(user_id)
        .bind(can_write)
        .execute(&*self.pool)
        .await
        .map_err(|e| DomainError::database_error(format!("Failed to share address book: {}", e)))?;

        Ok(())
    }

    async fn unshare_address_book(
        &self,
        address_book_id: &Uuid,
        user_id: Uuid,
    ) -> AddressBookRepositoryResult<()> {
        sqlx::query(
            r#"
            DELETE FROM carddav.address_book_shares
            WHERE address_book_id = $1 AND user_id = $2
            "#,
        )
        .bind(address_book_id)
        .bind(user_id)
        .execute(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("Failed to unshare address book: {}", e))
        })?;

        Ok(())
    }

    async fn get_address_book_shares(
        &self,
        address_book_id: &Uuid,
    ) -> AddressBookRepositoryResult<Vec<(String, bool)>> {
        let rows = sqlx::query(
            r#"
            SELECT user_id, can_write
            FROM carddav.address_book_shares
            WHERE address_book_id = $1
            ORDER BY user_id
            "#,
        )
        .bind(address_book_id)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("Failed to get address book shares: {}", e))
        })?;

        let result = rows
            .into_iter()
            .map(|row| (row.get("user_id"), row.get("can_write")))
            .collect();

        Ok(result)
    }
}
