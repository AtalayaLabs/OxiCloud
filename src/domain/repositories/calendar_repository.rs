use crate::common::errors::DomainError;
use crate::domain::entities::calendar::Calendar;
use uuid::Uuid;

pub type CalendarRepositoryResult<T> = Result<T, DomainError>;

/// Repository interface for Calendar entity operations.
///
/// Post-Round-3, access-control state lives in `storage.role_grants` —
/// the pre-Round-3 methods that read/wrote `caldav.calendar_shares`
/// (`list_calendars_shared_with_user`, `user_has_calendar_access`,
/// `share_calendar`, `remove_calendar_sharing`, `get_calendar_shares`)
/// have been removed from this trait, and the backing table was dropped
/// in `20260906000002_drop_legacy_share_tables.sql`.
pub trait CalendarRepository: Send + Sync + 'static {
    /// Creates a new calendar
    async fn create_calendar(&self, calendar: Calendar) -> CalendarRepositoryResult<Calendar>;

    /// Updates an existing calendar
    async fn update_calendar(&self, calendar: Calendar) -> CalendarRepositoryResult<Calendar>;

    /// Deletes a calendar by ID
    async fn delete_calendar(&self, id: &Uuid) -> CalendarRepositoryResult<()>;

    /// Finds a calendar by its ID
    async fn find_calendar_by_id(&self, id: &Uuid) -> CalendarRepositoryResult<Calendar>;

    /// Batch sibling of [`Self::find_calendar_by_id`]: one `= ANY($1)`
    /// round-trip for a page of grant-derived ids. Missing ids drop out
    /// (no per-id NotFound), matching the listing carve-out for
    /// deleted/trashed races. Ordering is not guaranteed.
    async fn find_calendars_by_ids(&self, ids: &[Uuid]) -> CalendarRepositoryResult<Vec<Calendar>>;

    /// Lists all calendars owned by a specific user. Post-Round-3 the
    /// service layer prefers `authz.list_incoming_grants` (surfaces
    /// owned + shared in one union), but this direct lookup remains
    /// available for internal maintenance / migration paths that need
    /// owner-only enumeration without going through the engine.
    async fn list_calendars_by_owner(
        &self,
        owner_id: Uuid,
    ) -> CalendarRepositoryResult<Vec<Calendar>>;

    /// Finds a calendar by name and owner
    async fn find_calendar_by_name_and_owner(
        &self,
        name: &str,
        owner_id: Uuid,
    ) -> CalendarRepositoryResult<Calendar>;

    /// Gets a custom property for a calendar
    async fn get_calendar_property(
        &self,
        calendar_id: &Uuid,
        property_name: &str,
    ) -> CalendarRepositoryResult<Option<String>>;

    /// Sets a custom property for a calendar
    async fn set_calendar_property(
        &self,
        calendar_id: &Uuid,
        property_name: &str,
        property_value: &str,
    ) -> CalendarRepositoryResult<()>;

    /// Removes a custom property from a calendar
    async fn remove_calendar_property(
        &self,
        calendar_id: &Uuid,
        property_name: &str,
    ) -> CalendarRepositoryResult<()>;

    /// Gets all custom properties for a calendar
    async fn get_calendar_properties(
        &self,
        calendar_id: &Uuid,
    ) -> CalendarRepositoryResult<std::collections::HashMap<String, String>>;
}
