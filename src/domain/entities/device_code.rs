//! Device Authorization Code entity (RFC 8628).
//!
//! Represents a pending or completed OAuth 2.0 Device Authorization Grant flow.

use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

/// Status of a device authorization flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceCodeStatus {
    /// Waiting for the user to authorize on the verification page.
    Pending,
    /// User approved — tokens are ready for the polling client.
    Authorized,
    /// User explicitly denied the request.
    Denied,
    /// The code expired before the user acted.
    Expired,
}

impl DeviceCodeStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Authorized => "authorized",
            Self::Denied => "denied",
            Self::Expired => "expired",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "authorized" => Some(Self::Authorized),
            "denied" => Some(Self::Denied),
            "expired" => Some(Self::Expired),
            _ => None,
        }
    }
}

impl std::fmt::Display for DeviceCodeStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Domain entity for a Device Authorization flow.
#[derive(Debug, Clone)]
pub struct DeviceCode {
    id: Uuid,
    device_code: String,
    user_code: String,
    client_name: String,
    scopes: String,
    status: DeviceCodeStatus,
    user_id: Option<Uuid>,
    access_token: Option<String>,
    refresh_token: Option<String>,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    expires_at: DateTime<Utc>,
    poll_interval_secs: i32,
    last_poll_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    authorized_at: Option<DateTime<Utc>>,
}

impl DeviceCode {
    /// Create a new pending device code flow.
    ///
    /// * `device_code` — opaque token for client polling (64 hex chars)
    /// * `user_code` — short human-readable code (e.g. "ABCD-1234")
    /// * `client_name` — display name of the requesting client
    /// * `scopes` — requested scopes (e.g. `oxicloud:files:rw`); see the
    ///   migration `20261126000000_credential_scopes_to_areas` on why these
    ///   are AREAS rather than protocols (e.g. "webdav,caldav,carddav")
    /// * `verification_uri` — URL the user must visit
    /// * `expires_in_secs` — TTL for the device code
    /// * `poll_interval_secs` — minimum polling interval
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        device_code: String,
        user_code: String,
        client_name: String,
        scopes: String,
        verification_uri: String,
        verification_uri_complete: Option<String>,
        expires_in_secs: i64,
        poll_interval_secs: i32,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            device_code,
            user_code,
            client_name,
            scopes,
            status: DeviceCodeStatus::Pending,
            user_id: None,
            access_token: None,
            refresh_token: None,
            verification_uri,
            verification_uri_complete,
            expires_at: now + Duration::seconds(expires_in_secs),
            poll_interval_secs,
            last_poll_at: None,
            created_at: now,
            authorized_at: None,
        }
    }

    /// Reconstruct from database row.
    #[allow(clippy::too_many_arguments)]
    pub fn from_raw(
        id: Uuid,
        device_code: String,
        user_code: String,
        client_name: String,
        scopes: String,
        status: DeviceCodeStatus,
        user_id: Option<Uuid>,
        access_token: Option<String>,
        refresh_token: Option<String>,
        verification_uri: String,
        verification_uri_complete: Option<String>,
        expires_at: DateTime<Utc>,
        poll_interval_secs: i32,
        last_poll_at: Option<DateTime<Utc>>,
        created_at: DateTime<Utc>,
        authorized_at: Option<DateTime<Utc>>,
    ) -> Self {
        Self {
            id,
            device_code,
            user_code,
            client_name,
            scopes,
            status,
            user_id,
            access_token,
            refresh_token,
            verification_uri,
            verification_uri_complete,
            expires_at,
            poll_interval_secs,
            last_poll_at,
            created_at,
            authorized_at,
        }
    }

    // ── Getters ──────────────────────────────────────────────────

    pub fn id(&self) -> Uuid {
        self.id
    }

    pub fn device_code(&self) -> &str {
        &self.device_code
    }

    pub fn user_code(&self) -> &str {
        &self.user_code
    }

    pub fn client_name(&self) -> &str {
        &self.client_name
    }

    pub fn scopes(&self) -> &str {
        &self.scopes
    }

    pub fn status(&self) -> DeviceCodeStatus {
        self.status
    }

    pub fn user_id(&self) -> Option<Uuid> {
        self.user_id
    }

    pub fn access_token(&self) -> Option<&str> {
        self.access_token.as_deref()
    }

    pub fn refresh_token(&self) -> Option<&str> {
        self.refresh_token.as_deref()
    }

    pub fn verification_uri(&self) -> &str {
        &self.verification_uri
    }

    pub fn verification_uri_complete(&self) -> Option<&str> {
        self.verification_uri_complete.as_deref()
    }

    pub fn expires_at(&self) -> DateTime<Utc> {
        self.expires_at
    }

    pub fn poll_interval_secs(&self) -> i32 {
        self.poll_interval_secs
    }

    pub fn last_poll_at(&self) -> Option<DateTime<Utc>> {
        self.last_poll_at
    }

    pub fn created_at(&self) -> DateTime<Utc> {
        self.created_at
    }

    pub fn authorized_at(&self) -> Option<DateTime<Utc>> {
        self.authorized_at
    }

    // ── Business logic ───────────────────────────────────────────

    /// Whether the device code has expired.
    pub fn is_expired(&self) -> bool {
        Utc::now() > self.expires_at
    }

    /// Seconds remaining until expiry (clamped to 0).
    pub fn seconds_remaining(&self) -> i64 {
        let remaining = (self.expires_at - Utc::now()).num_seconds();
        remaining.max(0)
    }

    /// Whether the client is polling too fast (within poll_interval_secs).
    pub fn is_polling_too_fast(&self) -> bool {
        if let Some(last) = self.last_poll_at {
            let elapsed = (Utc::now() - last).num_seconds();
            elapsed < self.poll_interval_secs as i64
        } else {
            false
        }
    }

    /// Record a poll attempt timestamp.
    pub fn record_poll(&mut self) {
        self.last_poll_at = Some(Utc::now());
    }

    /// Authorize this device code for a specific user, storing the tokens.
    pub fn authorize(&mut self, user_id: Uuid, access_token: String, refresh_token: String) {
        self.status = DeviceCodeStatus::Authorized;
        self.user_id = Some(user_id);
        self.access_token = Some(access_token);
        self.refresh_token = Some(refresh_token);
        self.authorized_at = Some(Utc::now());
    }

    /// Deny this device code.
    pub fn deny(&mut self) {
        self.status = DeviceCodeStatus::Denied;
    }

    /// Mark as expired.
    pub fn mark_expired(&mut self) {
        self.status = DeviceCodeStatus::Expired;
    }
}
