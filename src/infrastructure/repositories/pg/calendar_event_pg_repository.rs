use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row, types::Uuid};
use std::sync::Arc;

use crate::common::errors::DomainError;
use crate::domain::entities::calendar_event::CalendarEvent;
use crate::domain::repositories::calendar_event_repository::{
    CalendarEventRepository, CalendarEventRepositoryResult,
};

pub struct CalendarEventPgRepository {
    pool: Arc<PgPool>,
}

impl CalendarEventPgRepository {
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool }
    }

    /// Shared row → entity mapping (the inline shape every listing
    /// method uses, factored for the cursor stream).
    fn row_to_event(row: &sqlx::postgres::PgRow) -> CalendarEventRepositoryResult<CalendarEvent> {
        let mut event = CalendarEvent::with_id(
            row.get("id"),
            row.get("calendar_id"),
            row.get("summary"),
            row.get::<Option<String>, _>("description"),
            row.get::<Option<String>, _>("location"),
            row.get("start_time"),
            row.get("end_time"),
            row.get("all_day"),
            row.get::<Option<String>, _>("rrule"),
            row.get("ical_uid"),
            row.get("ical_data"),
            row.get("created_at"),
            row.get("updated_at"),
        )
        .map_err(|e| {
            DomainError::database_error(format!("Error creating calendar event: {}", e))
        })?;
        event.set_recurrence_id(row.get::<Option<DateTime<Utc>>, _>("recurrence_id"));
        Ok(event)
    }
}

impl CalendarEventRepository for CalendarEventPgRepository {
    async fn create_event(
        &self,
        event: CalendarEvent,
    ) -> CalendarEventRepositoryResult<CalendarEvent> {
        // This method would need a full implementation that builds the CalendarEvent
        // from the query result, using constructor methods
        // For this demonstration, we return the same event

        sqlx::query(
            r#"
            INSERT INTO caldav.calendar_events (
                id, calendar_id, summary, description, location, start_time, end_time,
                all_day, rrule, created_at, updated_at, ical_uid, ical_data,
                recurrence_id
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
            "#,
        )
        .bind(event.id())
        .bind(event.calendar_id())
        .bind(event.summary())
        .bind(event.description())
        .bind(event.location())
        .bind(event.start_time())
        .bind(event.end_time())
        .bind(event.all_day())
        .bind(event.rrule())
        .bind(event.created_at())
        .bind(event.updated_at())
        .bind(event.ical_uid())
        .bind(event.ical_data())
        // NULL on masters, non-NULL on exception overrides — see the
        // `20260913000001_calendar_events_recurrence_id.sql` migration
        // and `docs/architecture/rebac-authorization.md` follow-up doc.
        .bind(event.recurrence_id().copied())
        .execute(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("Failed to create calendar event: {}", e))
        })?;

        // We return the same event instead of a result
        Ok(event)
    }

    async fn update_event(
        &self,
        event: CalendarEvent,
    ) -> CalendarEventRepositoryResult<CalendarEvent> {
        let now = Utc::now();

        sqlx::query(
            r#"
            UPDATE caldav.calendar_events
            SET summary = $1,
                description = $2,
                location = $3,
                start_time = $4,
                end_time = $5,
                all_day = $6,
                rrule = $7,
                ical_data = $8,
                recurrence_id = $9,
                updated_at = $10
            WHERE id = $11
            "#,
        )
        .bind(event.summary())
        .bind(event.description())
        .bind(event.location())
        .bind(event.start_time())
        .bind(event.end_time())
        .bind(event.all_day())
        .bind(event.rrule())
        .bind(event.ical_data())
        .bind(event.recurrence_id().copied())
        .bind(now)
        .bind(event.id())
        .execute(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("Failed to update calendar event: {}", e))
        })?;

        // In a full implementation, we would retrieve the updated event
        // For simplicity, we return the same event we received
        Ok(event)
    }

    async fn delete_event(&self, id: &Uuid) -> CalendarEventRepositoryResult<()> {
        sqlx::query(
            r#"
            DELETE FROM caldav.calendar_events
            WHERE id = $1
            "#,
        )
        .bind(id)
        .execute(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("Failed to delete calendar event: {}", e))
        })?;

        Ok(())
    }

    async fn get_events_in_time_range(
        &self,
        calendar_id: &Uuid,
        start: &DateTime<Utc>,
        end: &DateTime<Utc>,
    ) -> CalendarEventRepositoryResult<Vec<CalendarEvent>> {
        let rows = sqlx::query(
            r#"
            SELECT
                id, calendar_id, summary, description, location,
                start_time, end_time, all_day, rrule,
                created_at, updated_at, ical_uid, ical_data, recurrence_id
            FROM caldav.calendar_events
            WHERE calendar_id = $1
              AND (
                  (start_time >= $2 AND start_time < $3) OR
                  (end_time > $2 AND end_time <= $3) OR
                  (start_time <= $2 AND end_time >= $3) OR
                  -- A recurring master hosts occurrences long after its
                  -- own DTEND. Filtering it on `end_time >= $2` dropped
                  -- a series from every window past its first instance
                  -- (the SPA's month view then showed nothing when the
                  -- user navigated forward). `start_time < $3` is the
                  -- only bound that is safe: UNTIL/COUNT are expanded
                  -- client-side and cannot be evaluated in SQL.
                  (rrule IS NOT NULL AND start_time < $3)
              )
            ORDER BY start_time
            "#,
        )
        .bind(calendar_id)
        .bind(start)
        .bind(end)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("Failed to get events in time range: {}", e))
        })?;

        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            let mut event = CalendarEvent::with_id(
                row.get("id"),
                row.get("calendar_id"),
                row.get("summary"),
                row.get::<Option<String>, _>("description"),
                row.get::<Option<String>, _>("location"),
                row.get("start_time"),
                row.get("end_time"),
                row.get("all_day"),
                row.get::<Option<String>, _>("rrule"),
                row.get("ical_uid"),
                row.get("ical_data"),
                row.get("created_at"),
                row.get("updated_at"),
            )
            .map_err(|e| {
                DomainError::database_error(format!("Error creating calendar event: {}", e))
            })?;
            // Rehydrate the RECURRENCE-ID after entity construction —
            // `with_id` initialises to `None` because the field predates
            // the rest of the constructor signature (#528). Keeping
            // `with_id` unchanged avoids ripple-changing every caller.
            event.set_recurrence_id(row.get::<Option<DateTime<Utc>>, _>("recurrence_id"));
            events.push(event);
        }

        Ok(events)
    }

    async fn find_calendar_id_by_event_id(&self, id: &Uuid) -> CalendarEventRepositoryResult<Uuid> {
        sqlx::query_scalar("SELECT calendar_id FROM caldav.calendar_events WHERE id = $1")
            .bind(id)
            .fetch_optional(&*self.pool)
            .await
            .map_err(|e| {
                DomainError::database_error(format!("Failed to get event calendar id: {}", e))
            })?
            .ok_or_else(|| DomainError::not_found("Calendar Event", id.to_string()))
    }

    async fn find_event_by_id(&self, id: &Uuid) -> CalendarEventRepositoryResult<CalendarEvent> {
        let row = sqlx::query(
            r#"
            SELECT
                id, calendar_id, summary, description, location,
                start_time, end_time, all_day, rrule,
                created_at, updated_at, ical_uid, ical_data, recurrence_id
            FROM caldav.calendar_events
            WHERE id = $1
            "#,
        )
        .bind(id)
        .fetch_optional(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("Failed to get calendar event by id: {}", e))
        })?
        .ok_or_else(|| DomainError::not_found("Calendar Event", id.to_string()))?;

        let mut event = CalendarEvent::with_id(
            row.get("id"),
            row.get("calendar_id"),
            row.get("summary"),
            row.get::<Option<String>, _>("description"),
            row.get::<Option<String>, _>("location"),
            row.get("start_time"),
            row.get("end_time"),
            row.get("all_day"),
            row.get::<Option<String>, _>("rrule"),
            row.get("ical_uid"),
            row.get("ical_data"),
            row.get("created_at"),
            row.get("updated_at"),
        )
        .map_err(|e| {
            DomainError::database_error(format!("Error creating calendar event: {}", e))
        })?;
        event.set_recurrence_id(row.get::<Option<DateTime<Utc>>, _>("recurrence_id"));

        Ok(event)
    }

    async fn list_events_by_calendar(
        &self,
        calendar_id: &Uuid,
    ) -> CalendarEventRepositoryResult<Vec<CalendarEvent>> {
        let rows = sqlx::query(
            r#"
            SELECT
                id, calendar_id, summary, description, location,
                start_time, end_time, all_day, rrule,
                created_at, updated_at, ical_uid, ical_data, recurrence_id
            FROM caldav.calendar_events
            WHERE calendar_id = $1
            ORDER BY start_time
            "#,
        )
        .bind(calendar_id)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("Failed to get events by calendar: {}", e))
        })?;

        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            let mut event = CalendarEvent::with_id(
                row.get("id"),
                row.get("calendar_id"),
                row.get("summary"),
                row.get::<Option<String>, _>("description"),
                row.get::<Option<String>, _>("location"),
                row.get("start_time"),
                row.get("end_time"),
                row.get("all_day"),
                row.get::<Option<String>, _>("rrule"),
                row.get("ical_uid"),
                row.get("ical_data"),
                row.get("created_at"),
                row.get("updated_at"),
            )
            .map_err(|e| {
                DomainError::database_error(format!("Error creating calendar event: {}", e))
            })?;
            event.set_recurrence_id(row.get::<Option<DateTime<Utc>>, _>("recurrence_id"));
            events.push(event);
        }

        Ok(events)
    }

    async fn find_events_by_summary(
        &self,
        calendar_id: &Uuid,
        summary: &str,
    ) -> CalendarEventRepositoryResult<Vec<CalendarEvent>> {
        let search_pattern = super::like_escape(summary);

        let rows = sqlx::query(
            r#"
            SELECT
                id, calendar_id, summary, description, location,
                start_time, end_time, all_day, rrule,
                created_at, updated_at, ical_uid, ical_data, recurrence_id
            FROM caldav.calendar_events
            WHERE calendar_id = $1 AND summary ILIKE $2
            ORDER BY start_time
            "#,
        )
        .bind(calendar_id)
        .bind(&search_pattern)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("Failed to find events by summary: {}", e))
        })?;

        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            let mut event = CalendarEvent::with_id(
                row.get("id"),
                row.get("calendar_id"),
                row.get("summary"),
                row.get::<Option<String>, _>("description"),
                row.get::<Option<String>, _>("location"),
                row.get("start_time"),
                row.get("end_time"),
                row.get("all_day"),
                row.get::<Option<String>, _>("rrule"),
                row.get("ical_uid"),
                row.get("ical_data"),
                row.get("created_at"),
                row.get("updated_at"),
            )
            .map_err(|e| {
                DomainError::database_error(format!("Error creating calendar event: {}", e))
            })?;
            event.set_recurrence_id(row.get::<Option<DateTime<Utc>>, _>("recurrence_id"));
            events.push(event);
        }

        Ok(events)
    }

    async fn find_event_by_ical_uid(
        &self,
        calendar_id: &Uuid,
        ical_uid: &str,
    ) -> CalendarEventRepositoryResult<Option<CalendarEvent>> {
        // Phase 2 note: this method looks up "an event with this UID"
        // — the SELECT still isn't filtered on `recurrence_id IS NULL`
        // because the phase-3 handler routing (which will distinguish
        // master vs. exception override at PUT time) is where the
        // filter actually needs to live. For phase 2 the invariant is
        // enforced only at INSERT time via the two partial unique
        // indexes; reads see whatever's there.
        let row_opt = sqlx::query(
            r#"
            SELECT
                id, calendar_id, summary, description, location,
                start_time, end_time, all_day, rrule,
                created_at, updated_at, ical_uid, ical_data, recurrence_id
            FROM caldav.calendar_events
            WHERE calendar_id = $1 AND ical_uid = $2 AND recurrence_id IS NULL
            "#,
        )
        .bind(calendar_id)
        .bind(ical_uid)
        .fetch_optional(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("Failed to get calendar event by UID: {}", e))
        })?;

        match row_opt {
            Some(row) => {
                let mut event = CalendarEvent::with_id(
                    row.get("id"),
                    row.get("calendar_id"),
                    row.get("summary"),
                    row.get::<Option<String>, _>("description"),
                    row.get::<Option<String>, _>("location"),
                    row.get("start_time"),
                    row.get("end_time"),
                    row.get("all_day"),
                    row.get::<Option<String>, _>("rrule"),
                    row.get("ical_uid"),
                    row.get("ical_data"),
                    row.get("created_at"),
                    row.get("updated_at"),
                )
                .map_err(|e| {
                    DomainError::database_error(format!("Error creating calendar event: {}", e))
                })?;
                event.set_recurrence_id(row.get::<Option<DateTime<Utc>>, _>("recurrence_id"));
                Ok(Some(event))
            }
            None => Ok(None),
        }
    }

    async fn find_event_by_ical_uid_and_recurrence_id(
        &self,
        calendar_id: &Uuid,
        ical_uid: &str,
        recurrence_id: &DateTime<Utc>,
    ) -> CalendarEventRepositoryResult<Option<CalendarEvent>> {
        // Uses idx_calendar_events_exception_unique — the partial
        // unique index on (calendar_id, ical_uid, recurrence_id)
        // WHERE recurrence_id IS NOT NULL — for the exact-match seek.
        let row_opt = sqlx::query(
            r#"
            SELECT
                id, calendar_id, summary, description, location,
                start_time, end_time, all_day, rrule,
                created_at, updated_at, ical_uid, ical_data, recurrence_id
            FROM caldav.calendar_events
            WHERE calendar_id = $1
              AND ical_uid = $2
              AND recurrence_id = $3
            "#,
        )
        .bind(calendar_id)
        .bind(ical_uid)
        .bind(recurrence_id)
        .fetch_optional(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!(
                "Failed to get calendar event exception by UID+RECURRENCE-ID: {}",
                e
            ))
        })?;

        match row_opt {
            Some(row) => {
                let mut event = CalendarEvent::with_id(
                    row.get("id"),
                    row.get("calendar_id"),
                    row.get("summary"),
                    row.get::<Option<String>, _>("description"),
                    row.get::<Option<String>, _>("location"),
                    row.get("start_time"),
                    row.get("end_time"),
                    row.get("all_day"),
                    row.get::<Option<String>, _>("rrule"),
                    row.get("ical_uid"),
                    row.get("ical_data"),
                    row.get("created_at"),
                    row.get("updated_at"),
                )
                .map_err(|e| {
                    DomainError::database_error(format!("Error creating calendar event: {}", e))
                })?;
                event.set_recurrence_id(row.get::<Option<DateTime<Utc>>, _>("recurrence_id"));
                Ok(Some(event))
            }
            None => Ok(None),
        }
    }

    async fn find_events_by_ical_uids(
        &self,
        calendar_id: &Uuid,
        ical_uids: &[String],
    ) -> CalendarEventRepositoryResult<Vec<CalendarEvent>> {
        // Batch UID lookup returns ALL rows for the given UIDs, both
        // masters and exception overrides. Callers that want just
        // masters filter downstream. Same phase-2 policy as the
        // single-UID variant — read-side filtering is a phase-3
        // concern; the DB unique indexes are what guarantee at most
        // one master + N distinct exceptions per (calendar, UID).
        let rows = sqlx::query(
            r#"
            SELECT
                id, calendar_id, summary, description, location,
                start_time, end_time, all_day, rrule,
                created_at, updated_at, ical_uid, ical_data, recurrence_id
            FROM caldav.calendar_events
            WHERE calendar_id = $1 AND ical_uid = ANY($2)
            ORDER BY start_time
            "#,
        )
        .bind(calendar_id)
        .bind(ical_uids)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("Failed to get calendar events by UIDs: {}", e))
        })?;

        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            let mut event = CalendarEvent::with_id(
                row.get("id"),
                row.get("calendar_id"),
                row.get("summary"),
                row.get::<Option<String>, _>("description"),
                row.get::<Option<String>, _>("location"),
                row.get("start_time"),
                row.get("end_time"),
                row.get("all_day"),
                row.get::<Option<String>, _>("rrule"),
                row.get("ical_uid"),
                row.get("ical_data"),
                row.get("created_at"),
                row.get("updated_at"),
            )
            .map_err(|e| {
                DomainError::database_error(format!("Error creating calendar event: {}", e))
            })?;
            event.set_recurrence_id(row.get::<Option<DateTime<Utc>>, _>("recurrence_id"));
            events.push(event);
        }

        Ok(events)
    }

    async fn count_events_in_calendar(
        &self,
        calendar_id: &Uuid,
    ) -> CalendarEventRepositoryResult<i64> {
        let row = sqlx::query(
            r#"
            SELECT COUNT(*) as count
            FROM caldav.calendar_events
            WHERE calendar_id = $1
            "#,
        )
        .bind(calendar_id)
        .fetch_one(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("Failed to count events in calendar: {}", e))
        })?;

        Ok(row.get::<i64, _>("count"))
    }

    async fn delete_all_events_in_calendar(
        &self,
        calendar_id: &Uuid,
    ) -> CalendarEventRepositoryResult<i64> {
        let result = sqlx::query(
            r#"
            DELETE FROM caldav.calendar_events
            WHERE calendar_id = $1
            "#,
        )
        .bind(calendar_id)
        .execute(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("Failed to delete all events in calendar: {}", e))
        })?;

        Ok(result.rows_affected() as i64)
    }

    fn stream_events_uid_order(
        &self,
        calendar_id: Uuid,
    ) -> futures::stream::BoxStream<'static, CalendarEventRepositoryResult<CalendarEvent>> {
        // ONE ordered scan for the whole calendar, served through a PG
        // cursor (`fetch`) so only a window of rows is in flight. The
        // window function puts every UID's rows adjacent, bundles
        // ordered by first occurrence — exactly the first-appearance
        // order the buffered `ORDER BY start_time` listing produced
        // after grouping — with the master row first inside each UID.
        //
        // The first streaming shape hydrated pages via
        // `ical_uid = ANY(page)`: ~20 µs per index descent made the
        // total wall 3-4x the buffered single scan (measured in
        // benches/ROUND5.md). This keeps the buffered path's one
        // scan+sort while bounding memory to a page.
        let pool = self.pool.clone();
        let stream: futures::stream::BoxStream<
            'static,
            CalendarEventRepositoryResult<CalendarEvent>,
        > = Box::pin(async_stream::try_stream! {
            let mut conn = pool.acquire().await.map_err(|e| {
                DomainError::database_error(format!("Failed to acquire connection: {}", e))
            })?;
            let mut rows = sqlx::query(
                r#"
                SELECT
                    id, calendar_id, summary, description, location,
                    start_time, end_time, all_day, rrule,
                    created_at, updated_at, ical_uid, ical_data, recurrence_id
                FROM caldav.calendar_events
                WHERE calendar_id = $1
                ORDER BY MIN(start_time) OVER (PARTITION BY ical_uid),
                         ical_uid,
                         (recurrence_id IS NOT NULL),
                         start_time
                "#,
            )
            .bind(calendar_id)
            .fetch(&mut *conn);

            use futures::TryStreamExt;
            while let Some(row) = rows.try_next().await.map_err(|e| {
                DomainError::database_error(format!("Failed to stream events: {}", e))
            })? {
                yield Self::row_to_event(&row)?;
            }
        });
        stream
    }

    async fn list_events_by_calendar_paginated(
        &self,
        calendar_id: &Uuid,
        limit: i64,
        offset: i64,
    ) -> CalendarEventRepositoryResult<Vec<CalendarEvent>> {
        let rows = sqlx::query(
            r#"
            SELECT 
                id, calendar_id, summary, description, location, 
                start_time, end_time, all_day, rrule, 
                created_at, updated_at, ical_uid, ical_data
            FROM caldav.calendar_events
            WHERE calendar_id = $1
            ORDER BY start_time
            LIMIT $2 OFFSET $3
            "#,
        )
        .bind(calendar_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!(
                "Failed to get paginated events by calendar: {}",
                e
            ))
        })?;

        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            let event = CalendarEvent::with_id(
                row.get("id"),
                row.get("calendar_id"),
                row.get("summary"),
                row.get::<Option<String>, _>("description"),
                row.get::<Option<String>, _>("location"),
                row.get("start_time"),
                row.get("end_time"),
                row.get("all_day"),
                row.get::<Option<String>, _>("rrule"),
                row.get("ical_uid"),
                row.get("ical_data"),
                row.get("created_at"),
                row.get("updated_at"),
            )
            .map_err(|e| {
                DomainError::database_error(format!("Error creating calendar event: {}", e))
            })?;
            events.push(event);
        }

        Ok(events)
    }

    async fn find_recurring_events_in_range(
        &self,
        calendar_id: &Uuid,
        start: &DateTime<Utc>,
        end: &DateTime<Utc>,
    ) -> CalendarEventRepositoryResult<Vec<CalendarEvent>> {
        let rows = sqlx::query(
            r#"
            SELECT 
                id, calendar_id, summary, description, location, 
                start_time, end_time, all_day, rrule, 
                created_at, updated_at, ical_uid, ical_data
            FROM caldav.calendar_events
            WHERE calendar_id = $1 
              AND rrule IS NOT NULL
              AND end_time >= $2
              AND start_time <= $3
            ORDER BY start_time
            "#,
        )
        .bind(calendar_id)
        .bind(start)
        .bind(end)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("Failed to find recurring events in range: {}", e))
        })?;

        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            let event = CalendarEvent::with_id(
                row.get("id"),
                row.get("calendar_id"),
                row.get("summary"),
                row.get::<Option<String>, _>("description"),
                row.get::<Option<String>, _>("location"),
                row.get("start_time"),
                row.get("end_time"),
                row.get("all_day"),
                row.get::<Option<String>, _>("rrule"),
                row.get("ical_uid"),
                row.get("ical_data"),
                row.get("created_at"),
                row.get("updated_at"),
            )
            .map_err(|e| {
                DomainError::database_error(format!("Error creating calendar event: {}", e))
            })?;
            events.push(event);
        }

        Ok(events)
    }
}
