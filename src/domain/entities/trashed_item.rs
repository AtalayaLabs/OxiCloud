use chrono::{DateTime, Utc};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq)]
pub enum TrashedItemType {
    File,
    Folder,
}

/// Owned decomposition of a [`TrashedItem`] (see
/// [`TrashedItem::into_parts`]).
pub struct TrashedItemParts {
    pub id: Uuid,
    pub original_id: Uuid,
    pub item_type: TrashedItemType,
    pub name: String,
    pub original_path: String,
    pub trashed_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct TrashedItem {
    id: Uuid,
    original_id: Uuid,
    user_id: Uuid,
    item_type: TrashedItemType,
    name: String,
    original_path: String,
    trashed_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
}

impl TrashedItem {
    pub fn new(
        original_id: Uuid,
        user_id: Uuid,
        item_type: TrashedItemType,
        name: String,
        original_path: String,
        retention_days: u32,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            original_id,
            user_id,
            item_type,
            name,
            original_path,
            trashed_at: now,
            expires_at: now + chrono::Duration::days(retention_days as i64),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn from_raw(
        id: Uuid,
        original_id: Uuid,
        user_id: Uuid,
        item_type: TrashedItemType,
        name: String,
        original_path: String,
        trashed_at: DateTime<Utc>,
        expires_at: DateTime<Utc>,
    ) -> Self {
        Self {
            id,
            original_id,
            user_id,
            item_type,
            name,
            original_path,
            trashed_at,
            expires_at,
        }
    }

    // ── Getters ──

    pub fn id(&self) -> Uuid {
        self.id
    }

    pub fn original_id(&self) -> Uuid {
        self.original_id
    }

    pub fn user_id(&self) -> Uuid {
        self.user_id
    }

    pub fn item_type(&self) -> &TrashedItemType {
        &self.item_type
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn original_path(&self) -> &str {
        &self.original_path
    }

    pub fn trashed_at(&self) -> DateTime<Utc> {
        self.trashed_at
    }

    pub fn expires_at(&self) -> DateTime<Utc> {
        self.expires_at
    }

    /// Decompose into owned parts for DTO conversion — moves `name` /
    /// `original_path` instead of the getter clones `to_dto` used to make
    /// per trash row (benches/ROUND11.md; the File/Folder/Contact pattern).
    pub fn into_parts(self) -> TrashedItemParts {
        TrashedItemParts {
            id: self.id,
            original_id: self.original_id,
            item_type: self.item_type,
            name: self.name,
            original_path: self.original_path,
            trashed_at: self.trashed_at,
        }
    }

    pub fn days_until_deletion(&self) -> i64 {
        let now = Utc::now();
        (self.expires_at - now).num_days().max(0)
    }
}
