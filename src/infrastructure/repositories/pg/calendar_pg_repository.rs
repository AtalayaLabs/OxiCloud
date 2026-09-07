use chrono::Utc;
use sqlx::{PgPool, Row, types::Uuid};
use std::sync::Arc;

use crate::common::errors::DomainError;
use crate::domain::entities::calendar::Calendar;
use crate::domain::repositories::calendar_repository::{
    CalendarRepository, CalendarRepositoryResult,
};
use crate::domain::services::path_service::normalize_storage_name;

pub struct CalendarPgRepository {
    pool: Arc<PgPool>,
}

impl CalendarPgRepository {
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool }
    }

    /// `EXISTS` short-circuit for the login provisioning hook, which only
    /// needs to know whether the user owns ANY calendar. The old
    /// `list_calendars_by_owner(..).is_empty()` hydrated every owned
    /// `Calendar` row (8 cols incl. description/color TEXT) on EVERY login
    /// just to test emptiness — the ROUND9 §7 `Drive::is_empty` COUNT→EXISTS
    /// pattern (benches/ROUND13.md §Q2).
    pub async fn has_owned_calendar(&self, owner_id: Uuid) -> CalendarRepositoryResult<bool> {
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM caldav.calendars WHERE owner_id = $1)")
                .bind(owner_id)
                .fetch_one(&*self.pool)
                .await
                .map_err(|e| {
                    DomainError::database_error(format!("Failed to probe owned calendars: {}", e))
                })?;
        Ok(exists)
    }
}

impl CalendarRepository for CalendarPgRepository {
    async fn create_calendar(&self, calendar: Calendar) -> CalendarRepositoryResult<Calendar> {
        // NFC-normalize the caller-supplied display name at the last
        // touch before bind — same choke-point pattern the storage.files
        // / storage.folders repos use (see docs/plan/nfc-normalization.md
        // / migrate.rs module doc). macOS CalDAV clients emit NFD in the
        // display-name field just as Finder does in the filename field;
        // NC-desktop / Thunderbird would then miss the calendar on their
        // NFC-normalized lookup path. Same class of bug as
        // AtalayaLabs/OxiCloud#706, different table.
        let normalized_name = normalize_storage_name(calendar.name());
        let row = sqlx::query(
            r#"
            INSERT INTO caldav.calendars (id, name, owner_id, description, color, is_public, created_at, updated_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            RETURNING id, name, owner_id, description, color, is_public, created_at, updated_at
            "#
        )
        .bind(calendar.id())
        .bind(&normalized_name)
        .bind(calendar.owner_id())
        .bind(calendar.description())
        .bind(calendar.color())
        .bind(false) // is_public doesn't exist as a field
        .bind(calendar.created_at())
        .bind(calendar.updated_at())
        .fetch_one(&*self.pool)
        .await
        .map_err(|e| DomainError::database_error(format!("Failed to create calendar: {}", e)))?;

        // Build the Calendar object using its with_id constructor
        let result = Calendar::with_id(
            row.get("id"),
            row.get("name"),
            row.get("owner_id"),
            row.get("description"),
            row.get("color"),
            row.get("created_at"),
            row.get("updated_at"),
        )
        .map_err(|e| {
            DomainError::database_error(format!("Failed to create calendar object: {}", e))
        })?;

        Ok(result)
    }

    async fn update_calendar(&self, calendar: Calendar) -> CalendarRepositoryResult<Calendar> {
        // NFC-normalize on rename — see `create_calendar` for the why.
        let normalized_name = normalize_storage_name(calendar.name());
        let now = Utc::now();
        let row = sqlx::query(
            r#"
            UPDATE caldav.calendars
            SET name = $1, description = $2, color = $3, is_public = $4, updated_at = $5
            WHERE id = $6
            RETURNING id, name, owner_id, description, color, is_public, created_at, updated_at
            "#,
        )
        .bind(&normalized_name)
        .bind(calendar.description())
        .bind(calendar.color())
        .bind(false) // is_public doesn't exist as a field
        .bind(now)
        .bind(calendar.id())
        .fetch_one(&*self.pool)
        .await
        .map_err(|e| DomainError::database_error(format!("Failed to update calendar: {}", e)))?;

        // Build the Calendar object using its with_id constructor
        let result = Calendar::with_id(
            row.get("id"),
            row.get("name"),
            row.get("owner_id"),
            row.get("description"),
            row.get("color"),
            row.get("created_at"),
            row.get("updated_at"),
        )
        .map_err(|e| {
            DomainError::database_error(format!("Failed to create calendar object: {}", e))
        })?;

        Ok(result)
    }

    async fn delete_calendar(&self, id: &Uuid) -> CalendarRepositoryResult<()> {
        sqlx::query(
            r#"
            DELETE FROM caldav.calendars
            WHERE id = $1
            "#,
        )
        .bind(id)
        .execute(&*self.pool)
        .await
        .map_err(|e| DomainError::database_error(format!("Failed to delete calendar: {}", e)))?;

        Ok(())
    }

    async fn find_calendar_by_id(&self, id: &Uuid) -> CalendarRepositoryResult<Calendar> {
        let row = sqlx::query(
            r#"
            SELECT id, name, owner_id, description, color, is_public, created_at, updated_at
            FROM caldav.calendars
            WHERE id = $1
            "#,
        )
        .bind(id)
        .fetch_optional(&*self.pool)
        .await
        .map_err(|e| DomainError::database_error(format!("Failed to get calendar by id: {}", e)))?
        .ok_or_else(|| DomainError::not_found("Calendar", id.to_string()))?;

        let calendar = Calendar::with_id(
            row.get("id"),
            row.get("name"),
            row.get("owner_id"),
            row.get("description"),
            row.get("color"),
            row.get("created_at"),
            row.get("updated_at"),
        )
        .map_err(|e| {
            DomainError::database_error(format!("Failed to create calendar object: {}", e))
        })?;

        Ok(calendar)
    }

    async fn find_calendars_by_ids(&self, ids: &[Uuid]) -> CalendarRepositoryResult<Vec<Calendar>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            r#"
            SELECT id, name, owner_id, description, color, is_public, created_at, updated_at
            FROM caldav.calendars
            WHERE id = ANY($1)
            "#,
        )
        .bind(ids)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("Failed to get calendars by ids: {}", e))
        })?;

        rows.iter()
            .map(|row| {
                Calendar::with_id(
                    row.get("id"),
                    row.get("name"),
                    row.get("owner_id"),
                    row.get("description"),
                    row.get("color"),
                    row.get("created_at"),
                    row.get("updated_at"),
                )
                .map_err(|e| {
                    DomainError::database_error(format!("Failed to create calendar object: {}", e))
                })
            })
            .collect()
    }

    async fn list_calendars_by_owner(
        &self,
        owner_id: Uuid,
    ) -> CalendarRepositoryResult<Vec<Calendar>> {
        let rows = sqlx::query(
            r#"
            SELECT id, name, owner_id, description, color, is_public, created_at, updated_at
            FROM caldav.calendars
            WHERE owner_id = $1
            ORDER BY name
            "#,
        )
        .bind(owner_id)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("Failed to get calendars by owner: {}", e))
        })?;

        let mut calendars = Vec::with_capacity(rows.len());
        for row in rows {
            let calendar = Calendar::with_id(
                row.get("id"),
                row.get("name"),
                row.get("owner_id"),
                row.get("description"),
                row.get("color"),
                row.get("created_at"),
                row.get("updated_at"),
            )
            .map_err(|e| {
                DomainError::database_error(format!("Failed to create calendar object: {}", e))
            })?;
            calendars.push(calendar);
        }

        Ok(calendars)
    }

    async fn find_calendar_by_name_and_owner(
        &self,
        name: &str,
        owner_id: Uuid,
    ) -> CalendarRepositoryResult<Calendar> {
        let row = sqlx::query(
            r#"
            SELECT id, name, owner_id, description, color, is_public, created_at, updated_at
            FROM caldav.calendars
            WHERE name = $1 AND owner_id = $2
            "#,
        )
        .bind(name)
        .bind(owner_id)
        .fetch_optional(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("Failed to find calendar by name and owner: {}", e))
        })?
        .ok_or_else(|| {
            DomainError::not_found("Calendar", format!("{} (owned by {})", name, owner_id))
        })?;

        let calendar = Calendar::with_id(
            row.get("id"),
            row.get("name"),
            row.get("owner_id"),
            row.get("description"),
            row.get("color"),
            row.get("created_at"),
            row.get("updated_at"),
        )
        .map_err(|e| {
            DomainError::database_error(format!("Failed to create calendar object: {}", e))
        })?;

        Ok(calendar)
    }

    async fn list_public_calendars(
        &self,
        limit: i64,
        offset: i64,
    ) -> CalendarRepositoryResult<Vec<Calendar>> {
        let rows = sqlx::query(
            r#"
            SELECT id, name, owner_id, description, color, is_public, created_at, updated_at
            FROM caldav.calendars
            WHERE is_public = true
            ORDER BY name
            LIMIT $1 OFFSET $2
            "#,
        )
        .bind(limit)
        .bind(offset)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("Failed to get public calendars: {}", e))
        })?;

        let mut calendars = Vec::with_capacity(rows.len());
        for row in rows {
            let calendar = Calendar::with_id(
                row.get("id"),
                row.get("name"),
                row.get("owner_id"),
                row.get("description"),
                row.get("color"),
                row.get("created_at"),
                row.get("updated_at"),
            )
            .map_err(|e| {
                DomainError::database_error(format!("Failed to create calendar object: {}", e))
            })?;
            calendars.push(calendar);
        }

        Ok(calendars)
    }

    async fn get_calendar_property(
        &self,
        calendar_id: &Uuid,
        property_name: &str,
    ) -> CalendarRepositoryResult<Option<String>> {
        let row = sqlx::query(
            r#"
            SELECT value
            FROM caldav.calendar_properties
            WHERE calendar_id = $1 AND name = $2
            "#,
        )
        .bind(calendar_id)
        .bind(property_name)
        .fetch_optional(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("Failed to get calendar property: {}", e))
        })?;

        Ok(row.map(|r| r.get("value")))
    }

    async fn set_calendar_property(
        &self,
        calendar_id: &Uuid,
        property_name: &str,
        property_value: &str,
    ) -> CalendarRepositoryResult<()> {
        sqlx::query(
            r#"
            INSERT INTO caldav.calendar_properties (calendar_id, name, value)
            VALUES ($1, $2, $3)
            ON CONFLICT (calendar_id, name) DO UPDATE SET value = $3
            "#,
        )
        .bind(calendar_id)
        .bind(property_name)
        .bind(property_value)
        .execute(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("Failed to set calendar property: {}", e))
        })?;

        Ok(())
    }

    async fn remove_calendar_property(
        &self,
        calendar_id: &Uuid,
        property_name: &str,
    ) -> CalendarRepositoryResult<()> {
        sqlx::query(
            r#"
            DELETE FROM caldav.calendar_properties
            WHERE calendar_id = $1 AND name = $2
            "#,
        )
        .bind(calendar_id)
        .bind(property_name)
        .execute(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("Failed to remove calendar property: {}", e))
        })?;

        Ok(())
    }

    async fn get_calendar_properties(
        &self,
        calendar_id: &Uuid,
    ) -> CalendarRepositoryResult<std::collections::HashMap<String, String>> {
        let rows = sqlx::query(
            r#"
            SELECT name, value
            FROM caldav.calendar_properties
            WHERE calendar_id = $1
            "#,
        )
        .bind(calendar_id)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("Failed to get calendar properties: {}", e))
        })?;

        let mut properties = std::collections::HashMap::with_capacity(rows.len());
        for row in rows {
            properties.insert(row.get("name"), row.get("value"));
        }

        Ok(properties)
    }
}
