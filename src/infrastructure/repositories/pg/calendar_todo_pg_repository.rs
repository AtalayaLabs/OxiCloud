use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row, types::Uuid};
use std::sync::Arc;

use crate::common::errors::DomainError;
use crate::domain::entities::calendar_todo::CalendarTodo;
use crate::domain::repositories::calendar_todo_repository::{
    CalendarTodoRepository, CalendarTodoRepositoryResult,
};

/// PostgreSQL implementation of [`CalendarTodoRepository`] (#754).
///
/// Mirrors `CalendarEventPgRepository` shape-for-shape: same
/// master/exception UID routing, same delete-then-insert upsert
/// compatibility with the partial unique indexes, same cursor stream
/// ordering. The SELECT column list below is the single source for
/// every query in this file.
pub struct CalendarTodoPgRepository {
    pool: Arc<PgPool>,
}

/// The full column list every row-hydrating query shares — keep in
/// sync with `row_to_todo`. A macro (not a const) so `concat!` can
/// splice it into each query at compile time with zero allocation.
macro_rules! todo_columns {
    () => {
        "id, calendar_id, summary, description, location, status, \
         percent_complete, priority, start_time, due_time, completed_at, \
         all_day, rrule, ical_uid, ical_data, recurrence_id, \
         created_at, updated_at"
    };
}

impl CalendarTodoPgRepository {
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool }
    }

    /// Shared row → entity mapping.
    fn row_to_todo(row: &sqlx::postgres::PgRow) -> CalendarTodoRepositoryResult<CalendarTodo> {
        let mut todo = CalendarTodo::with_id(
            row.get("id"),
            row.get("calendar_id"),
            row.get::<Option<String>, _>("summary"),
            row.get::<Option<String>, _>("description"),
            row.get::<Option<String>, _>("location"),
            row.get::<Option<String>, _>("status"),
            row.get::<Option<i16>, _>("percent_complete"),
            row.get::<Option<i16>, _>("priority"),
            row.get::<Option<DateTime<Utc>>, _>("start_time"),
            row.get::<Option<DateTime<Utc>>, _>("due_time"),
            row.get::<Option<DateTime<Utc>>, _>("completed_at"),
            row.get("all_day"),
            row.get::<Option<String>, _>("rrule"),
            row.get("ical_uid"),
            row.get("ical_data"),
            row.get("created_at"),
            row.get("updated_at"),
        )
        .map_err(|e| DomainError::database_error(format!("Error creating calendar todo: {}", e)))?;
        // Rehydrate the RECURRENCE-ID after entity construction —
        // `with_id` initialises to `None`, mirroring the events path
        // (#528).
        todo.set_recurrence_id(row.get::<Option<DateTime<Utc>>, _>("recurrence_id"));
        Ok(todo)
    }

    /// Hydrate a `fetch_all` result set into entities.
    fn rows_to_todos(
        rows: Vec<sqlx::postgres::PgRow>,
    ) -> CalendarTodoRepositoryResult<Vec<CalendarTodo>> {
        let mut todos = Vec::with_capacity(rows.len());
        for row in rows {
            todos.push(Self::row_to_todo(&row)?);
        }
        Ok(todos)
    }
}

impl CalendarTodoRepository for CalendarTodoPgRepository {
    async fn create_todo(&self, todo: CalendarTodo) -> CalendarTodoRepositoryResult<CalendarTodo> {
        sqlx::query(
            r#"
            INSERT INTO caldav.calendar_todos (
                id, calendar_id, summary, description, location, status,
                percent_complete, priority, start_time, due_time, completed_at,
                all_day, rrule, ical_uid, ical_data, recurrence_id,
                created_at, updated_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18)
            "#,
        )
        .bind(todo.id())
        .bind(todo.calendar_id())
        .bind(todo.summary())
        .bind(todo.description())
        .bind(todo.location())
        .bind(todo.status())
        .bind(todo.percent_complete())
        .bind(todo.priority())
        .bind(todo.start_time())
        .bind(todo.due_time())
        .bind(todo.completed_at())
        .bind(todo.all_day())
        .bind(todo.rrule())
        .bind(todo.ical_uid())
        .bind(todo.ical_data())
        .bind(todo.recurrence_id().copied())
        .bind(todo.created_at())
        .bind(todo.updated_at())
        .execute(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("Failed to create calendar todo: {}", e))
        })?;

        Ok(todo)
    }

    async fn delete_todo(&self, id: &Uuid) -> CalendarTodoRepositoryResult<()> {
        sqlx::query(
            r#"
            DELETE FROM caldav.calendar_todos
            WHERE id = $1
            "#,
        )
        .bind(id)
        .execute(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("Failed to delete calendar todo: {}", e))
        })?;

        Ok(())
    }

    async fn find_calendar_id_by_todo_id(&self, id: &Uuid) -> CalendarTodoRepositoryResult<Uuid> {
        sqlx::query_scalar("SELECT calendar_id FROM caldav.calendar_todos WHERE id = $1")
            .bind(id)
            .fetch_optional(&*self.pool)
            .await
            .map_err(|e| {
                DomainError::database_error(format!("Failed to get todo calendar id: {}", e))
            })?
            .ok_or_else(|| DomainError::not_found("Calendar Todo", id.to_string()))
    }

    async fn find_todo_by_ical_uid(
        &self,
        calendar_id: &Uuid,
        ical_uid: &str,
    ) -> CalendarTodoRepositoryResult<Option<CalendarTodo>> {
        let row_opt = sqlx::query(concat!(
            "SELECT ",
            todo_columns!(),
            " FROM caldav.calendar_todos \
             WHERE calendar_id = $1 AND ical_uid = $2 AND recurrence_id IS NULL"
        ))
        .bind(calendar_id)
        .bind(ical_uid)
        .fetch_optional(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("Failed to get calendar todo by UID: {}", e))
        })?;

        match row_opt {
            Some(row) => Ok(Some(Self::row_to_todo(&row)?)),
            None => Ok(None),
        }
    }

    async fn find_todo_by_ical_uid_and_recurrence_id(
        &self,
        calendar_id: &Uuid,
        ical_uid: &str,
        recurrence_id: &DateTime<Utc>,
    ) -> CalendarTodoRepositoryResult<Option<CalendarTodo>> {
        // Uses idx_calendar_todos_exception_unique for the exact-match
        // seek (same shape as the events lookup).
        let row_opt = sqlx::query(concat!(
            "SELECT ",
            todo_columns!(),
            " FROM caldav.calendar_todos \
             WHERE calendar_id = $1 AND ical_uid = $2 AND recurrence_id = $3"
        ))
        .bind(calendar_id)
        .bind(ical_uid)
        .bind(recurrence_id)
        .fetch_optional(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!(
                "Failed to get calendar todo exception by UID+RECURRENCE-ID: {}",
                e
            ))
        })?;

        match row_opt {
            Some(row) => Ok(Some(Self::row_to_todo(&row)?)),
            None => Ok(None),
        }
    }

    async fn find_todos_by_ical_uids(
        &self,
        calendar_id: &Uuid,
        ical_uids: &[String],
    ) -> CalendarTodoRepositoryResult<Vec<CalendarTodo>> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            todo_columns!(),
            " FROM caldav.calendar_todos \
             WHERE calendar_id = $1 AND ical_uid = ANY($2) \
             ORDER BY COALESCE(start_time, due_time, created_at)"
        ))
        .bind(calendar_id)
        .bind(ical_uids)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("Failed to get calendar todos by UIDs: {}", e))
        })?;

        Self::rows_to_todos(rows)
    }

    async fn get_todos_in_time_range(
        &self,
        calendar_id: &Uuid,
        start: &DateTime<Utc>,
        end: &DateTime<Utc>,
    ) -> CalendarTodoRepositoryResult<Vec<CalendarTodo>> {
        // RFC 4791 §9.9 time-range overlap for VTODO — the pragmatic
        // rule set below matches what mainstream servers (SabreDAV
        // lineage) apply; each arm is annotated with the component
        // shape it covers. Tasks with NO datelike property at all
        // (only UID+DTSTAMP) never match a time-range — correct per
        // the RFC (they match only an unfiltered query).
        let rows = sqlx::query(concat!(
            "SELECT ",
            todo_columns!(),
            " FROM caldav.calendar_todos \
             WHERE calendar_id = $1 \
               AND ( \
                   /* (1) DTSTART + DUE: interval overlap of \
                      [DTSTART, DUE] with the window. */ \
                   (start_time IS NOT NULL AND due_time IS NOT NULL \
                       AND start_time < $3 AND due_time >= $2) \
                   /* (2) DTSTART only: DTSTART falls in the window, or \
                      the task is still OPEN and started before the \
                      window ends (ongoing-task semantics). */ \
                   OR (start_time IS NOT NULL AND due_time IS NULL \
                       AND start_time < $3 \
                       AND (start_time >= $2 \
                            OR (completed_at IS NULL \
                                AND status IS DISTINCT FROM 'COMPLETED' \
                                AND status IS DISTINCT FROM 'CANCELLED'))) \
                   /* (3) DUE only: the due instant falls in the window. */ \
                   OR (start_time IS NULL AND due_time IS NOT NULL \
                       AND due_time >= $2 AND due_time < $3) \
                   /* (4) Neither DTSTART nor DUE: COMPLETED within the \
                      window, else CREATED within the window. */ \
                   OR (start_time IS NULL AND due_time IS NULL \
                       AND ((completed_at IS NOT NULL \
                             AND completed_at >= $2 AND completed_at < $3) \
                            OR (completed_at IS NULL \
                                AND created_at >= $2 AND created_at < $3))) \
                   /* (5) Recurring master passthrough — same policy as \
                      events: the master row rides along when its own \
                      marker reaches into the window; expansion stays \
                      client-side. */ \
                   OR (rrule IS NOT NULL \
                       AND COALESCE(due_time, start_time, created_at) >= $2) \
               ) \
             ORDER BY COALESCE(start_time, due_time, created_at)"
        ))
        .bind(calendar_id)
        .bind(start)
        .bind(end)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("Failed to get todos in time range: {}", e))
        })?;

        Self::rows_to_todos(rows)
    }

    fn stream_todos_uid_order(
        &self,
        calendar_id: Uuid,
    ) -> futures::stream::BoxStream<'static, CalendarTodoRepositoryResult<CalendarTodo>> {
        // Same window-function ordering as the events stream: every
        // UID's rows adjacent, bundles in first-appearance order,
        // master first inside each UID. Tasks without any datelike
        // property sort on created_at via COALESCE.
        let pool = self.pool.clone();
        let stream: futures::stream::BoxStream<
            'static,
            CalendarTodoRepositoryResult<CalendarTodo>,
        > = Box::pin(async_stream::try_stream! {
            let mut conn = pool.acquire().await.map_err(|e| {
                DomainError::database_error(format!("Failed to acquire connection: {}", e))
            })?;
            let mut rows = sqlx::query(concat!(
                "SELECT ", todo_columns!(), " FROM caldav.calendar_todos \
                 WHERE calendar_id = $1 \
                 ORDER BY MIN(COALESCE(start_time, due_time, created_at)) \
                              OVER (PARTITION BY ical_uid), \
                          ical_uid, \
                          (recurrence_id IS NOT NULL), \
                          COALESCE(start_time, due_time, created_at)"
            ))
            .bind(calendar_id)
            .fetch(&mut *conn);

            use futures::TryStreamExt;
            while let Some(row) = rows.try_next().await.map_err(|e| {
                DomainError::database_error(format!("Failed to stream todos: {}", e))
            })? {
                yield Self::row_to_todo(&row)?;
            }
        });
        stream
    }

    async fn delete_all_todos_in_calendar(
        &self,
        calendar_id: &Uuid,
    ) -> CalendarTodoRepositoryResult<i64> {
        let result = sqlx::query(
            r#"
            DELETE FROM caldav.calendar_todos
            WHERE calendar_id = $1
            "#,
        )
        .bind(calendar_id)
        .execute(&*self.pool)
        .await
        .map_err(|e| {
            DomainError::database_error(format!("Failed to delete all todos in calendar: {}", e))
        })?;

        Ok(result.rows_affected() as i64)
    }
}
