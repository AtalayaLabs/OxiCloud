use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::application::dtos::timestamp_helpers::{
    deserialize_flex_datetime_option, epoch_to_datetime,
};
use crate::domain::entities::share::Share;

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ShareDto {
    pub id: String,
    pub item_id: String,
    pub item_name: Option<String>,
    pub item_type: String,
    pub token: String,
    pub url: String,
    pub has_password: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    /// Creation timestamp (RFC 3339 UTC).
    pub created_at: DateTime<Utc>,
    pub created_by: String,
    pub access_count: u64,
    /// `None` = unlimited.
    pub max_downloads: Option<u32>,
    pub download_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CreateShareDto {
    pub item_id: String,
    pub item_name: Option<String>,
    pub item_type: String,
    pub password: Option<String>,
    /// Inbound request accepts an integer epoch second OR an RFC 3339
    /// string — see `timestamp_helpers::deserialize_flex_datetime_option`.
    /// Outbound serialization is always RFC 3339.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_flex_datetime_option"
    )]
    pub expires_at: Option<DateTime<Utc>>,
    /// Optional cap on content downloads. Omit or `null` for unlimited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_downloads: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UpdateShareDto {
    pub password: Option<String>,
    /// See [`CreateShareDto::expires_at`] for the lenient wire contract.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_flex_datetime_option"
    )]
    pub expires_at: Option<DateTime<Utc>>,
    /// Absent leaves the cap alone; `null` removes it; a number sets it.
    // A double `Option` is required: plain `Option<u32>` maps absent and
    // `null` onto the same `None`, making "unlimited" unreachable.
    #[serde(default, deserialize_with = "present_option")]
    pub max_downloads: Option<Option<u32>>,
}

fn present_option<'de, T, D>(deserializer: D) -> Result<Option<T>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    T::deserialize(deserializer).map(Some)
}

impl ShareDto {
    pub fn from_entity(share: &Share, base_url: &str) -> Self {
        let url = format!("{}/s/{}", base_url, share.token());

        Self {
            id: share.id().to_string(),
            item_id: share.item_id().to_string(),
            item_name: share.item_name().map(|s| s.to_string()),
            item_type: share.item_type().to_string(),
            token: share.token().to_string(),
            url,
            has_password: share.has_password(),
            expires_at: share.expires_at().map(epoch_to_datetime),
            created_at: epoch_to_datetime(share.created_at()),
            created_by: share.created_by().to_string(),
            access_count: share.access_count(),
            max_downloads: share.max_downloads(),
            download_count: share.download_count(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::UpdateShareDto;

    #[test]
    fn update_share_dto_keeps_absent_null_and_number_apart() {
        let absent: UpdateShareDto = serde_json::from_str("{}").unwrap();
        assert_eq!(absent.max_downloads, None);

        let cleared: UpdateShareDto = serde_json::from_str(r#"{"max_downloads":null}"#).unwrap();
        assert_eq!(cleared.max_downloads, Some(None));

        let set: UpdateShareDto = serde_json::from_str(r#"{"max_downloads":5}"#).unwrap();
        assert_eq!(set.max_downloads, Some(Some(5)));
    }
}
