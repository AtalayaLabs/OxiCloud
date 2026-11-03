use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

// Re-export entity errors from the centralized module
pub use super::entity_errors::ShareError;

#[derive(Debug, Clone, PartialEq)]
pub struct Share {
    id: Uuid,
    item_id: String,
    item_name: Option<String>,
    item_type: ShareItemType,
    token: String,
    password_hash: Option<String>,
    /// Derived from `storage.role_grants.expires_at` — not stored on the share row.
    expires_at: Option<u64>,
    created_at: u64,
    created_by: Uuid,
    access_count: u64,
    /// `None` = unlimited. Enforced when a visitor downloads content
    /// through the link, not when the link is opened.
    max_downloads: Option<u32>,
    download_count: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ShareItemType {
    File,
    Folder,
}

impl Share {
    pub fn new(
        item_id: String,
        item_name: Option<String>,
        item_type: ShareItemType,
        created_by: Uuid,
        password_hash: Option<String>,
    ) -> Result<Self, ShareError> {
        if item_id.is_empty() {
            return Err(ShareError::ValidationError(
                "Item ID cannot be empty".to_string(),
            ));
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("Time went backwards")
            .as_secs();

        Ok(Self {
            id: Uuid::new_v4(),
            item_id,
            item_name,
            item_type,
            token: Uuid::new_v4().to_string(),
            password_hash,
            expires_at: None,
            created_at: now,
            created_by,
            access_count: 0,
            max_downloads: None,
            download_count: 0,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn from_raw(
        id: Uuid,
        item_id: String,
        item_name: Option<String>,
        item_type: ShareItemType,
        token: String,
        password_hash: Option<String>,
        expires_at: Option<u64>,
        created_at: u64,
        created_by: Uuid,
        access_count: u64,
        max_downloads: Option<u32>,
        download_count: u64,
    ) -> Self {
        Self {
            id,
            item_id,
            item_name,
            item_type,
            token,
            password_hash,
            expires_at,
            created_at,
            created_by,
            access_count,
            max_downloads,
            download_count,
        }
    }

    // ── Getters ──

    pub fn id(&self) -> Uuid {
        self.id
    }

    pub fn item_id(&self) -> &str {
        &self.item_id
    }

    pub fn item_name(&self) -> Option<&str> {
        self.item_name.as_deref()
    }

    pub fn item_type(&self) -> &ShareItemType {
        &self.item_type
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn expires_at(&self) -> Option<u64> {
        self.expires_at
    }

    pub fn created_at(&self) -> u64 {
        self.created_at
    }

    pub fn created_by(&self) -> Uuid {
        self.created_by
    }

    pub fn access_count(&self) -> u64 {
        self.access_count
    }

    pub fn max_downloads(&self) -> Option<u32> {
        self.max_downloads
    }

    pub fn download_count(&self) -> u64 {
        self.download_count
    }

    // ── Builder-style modifiers (immutable) ──

    pub fn with_max_downloads(mut self, max_downloads: Option<u32>) -> Self {
        self.max_downloads = max_downloads;
        self
    }

    pub fn with_download_count(mut self, download_count: u64) -> Self {
        self.download_count = download_count;
        self
    }

    pub fn with_password(mut self, password_hash: Option<String>) -> Self {
        self.password_hash = password_hash;
        self
    }

    pub fn with_token(mut self, token: String) -> Self {
        self.token = token;
        self
    }

    pub fn is_expired(&self) -> bool {
        if let Some(expires_at) = self.expires_at {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("Time went backwards")
                .as_secs();

            return expires_at <= now;
        }

        false
    }

    pub fn increment_access_count(mut self) -> Self {
        self.access_count += 1;
        self
    }

    /// Returns whether this share requires a password to access.
    pub fn has_password(&self) -> bool {
        self.password_hash.is_some()
    }

    /// Returns a reference to the password hash, if one is set.
    ///
    /// Password verification should be performed externally via PasswordHasherPort
    /// to keep cryptographic dependencies out of the domain layer.
    pub fn password_hash(&self) -> Option<&str> {
        self.password_hash.as_deref()
    }
}

impl std::fmt::Display for ShareItemType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ShareItemType::File => write!(f, "file"),
            ShareItemType::Folder => write!(f, "folder"),
        }
    }
}

impl TryFrom<&str> for ShareItemType {
    type Error = ShareError;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        // ASCII case-insensitive compare against the two literals instead of a
        // throwaway Unicode `to_lowercase()` String — byte-identical acceptance
        // for the ASCII targets "file"/"folder" (1 → 0 allocs/call).
        if s.eq_ignore_ascii_case("file") {
            Ok(ShareItemType::File)
        } else if s.eq_ignore_ascii_case("folder") {
            Ok(ShareItemType::Folder)
        } else {
            Err(ShareError::ValidationError(format!(
                "Invalid item type: {}",
                s
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_user_id() -> Uuid {
        Uuid::new_v4()
    }

    #[test]
    fn test_create_share() {
        let uid = test_user_id();
        let share = Share::new(
            "test_file_id".to_string(),
            None,
            ShareItemType::File,
            uid,
            None,
        )
        .unwrap();

        assert_eq!(share.item_id(), "test_file_id");
        assert_eq!(*share.item_type(), ShareItemType::File);
        assert_eq!(share.created_by(), uid);
        assert!(!share.has_password());
        assert!(share.expires_at().is_none());
        assert_eq!(share.access_count(), 0);
    }

    #[test]
    fn test_share_item_type_conversion() {
        assert_eq!(ShareItemType::File.to_string(), "file");
        assert_eq!(ShareItemType::Folder.to_string(), "folder");

        assert_eq!(
            ShareItemType::try_from("file").unwrap(),
            ShareItemType::File
        );
        assert_eq!(
            ShareItemType::try_from("folder").unwrap(),
            ShareItemType::Folder
        );
        assert_eq!(
            ShareItemType::try_from("FILE").unwrap(),
            ShareItemType::File
        );
        assert!(ShareItemType::try_from("invalid").is_err());
    }

    #[test]
    fn test_has_password_with_hash() {
        let share = Share::new(
            "test_file_id".to_string(),
            None,
            ShareItemType::File,
            test_user_id(),
            Some("some_hash_value".to_string()),
        )
        .unwrap();

        assert!(share.has_password());
        assert_eq!(share.password_hash(), Some("some_hash_value"));
    }

    #[test]
    fn test_has_password_without_hash() {
        let share = Share::new(
            "test_file_id".to_string(),
            None,
            ShareItemType::File,
            test_user_id(),
            None,
        )
        .unwrap();

        assert!(!share.has_password());
        assert_eq!(share.password_hash(), None);
    }
}
