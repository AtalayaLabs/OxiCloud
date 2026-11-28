//! App Password entity.
//!
//! Represents an application-specific password that clients (like DAVx⁵, Thunderbird)
//! can use with HTTP Basic Auth to access WebDAV/CalDAV/CardDAV endpoints without
//! requiring interactive OAuth flows.

use chrono::{DateTime, Utc};
use uuid::Uuid;

/// An application password created by a user for a specific client.
#[derive(Debug, Clone)]
pub struct AppPassword {
    /// Unique identifier.
    pub id: Uuid,
    /// Owner user ID.
    pub user_id: Uuid,
    /// Human-readable label chosen by the user (e.g. "DAVx5 on Pixel 8").
    pub label: String,
    /// Argon2 hash of the generated password token.
    ///
    /// The plain text token is only returned once at creation time.
    pub password_hash: String,
    /// First 8 characters of the plain text token, stored for display purposes
    /// so the user can identify which token is which.
    pub prefix: String,
    /// Scopes, space- or comma-separated (e.g. `oxicloud:files:rw`).
    /// AREA vocabulary since `20261126000000_credential_scopes_to_areas`;
    /// the protocol names it replaced named the same permission twice..
    pub scopes: String,
    /// When this app password was created.
    pub created_at: DateTime<Utc>,
    /// When this app password was last used for authentication.
    pub last_used_at: Option<DateTime<Utc>>,
    /// Optional expiry — `None` means never expires.
    pub expires_at: Option<DateTime<Utc>>,
    /// Whether this app password is active.
    pub active: bool,
}

impl AppPassword {
    /// Create a new app password entity.
    ///
    /// The caller is responsible for hashing the raw token and passing
    /// the hash and prefix.
    pub fn new(
        user_id: Uuid,
        label: String,
        password_hash: String,
        prefix: String,
        scopes: String,
        expires_at: Option<DateTime<Utc>>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            user_id,
            label,
            password_hash,
            prefix,
            scopes,
            created_at: Utc::now(),
            last_used_at: None,
            expires_at,
            active: true,
        }
    }

    /// Check whether this app password has expired.
    pub fn is_expired(&self) -> bool {
        if let Some(exp) = self.expires_at {
            Utc::now() >= exp
        } else {
            false
        }
    }

    /// Check whether this app password is usable (active and not expired).
    pub fn is_usable(&self) -> bool {
        self.active && !self.is_expired()
    }

    /// Check whether the given scope is granted by this app password.
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.split(',').any(|s| s.trim() == scope)
    }
}
