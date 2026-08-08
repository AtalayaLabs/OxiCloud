//! Domain errors
//!
//! This module contains domain-specific error types.
//! DomainError is the base error used throughout the domain layer.

use std::error::Error as StdError;
use std::fmt::{Display, Formatter, Result as FmtResult};
use thiserror::Error;

/// Common Result type for the domain with DomainError as the standard error
pub type Result<T> = std::result::Result<T, DomainError>;

/// Domain error types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Entity not found
    NotFound,
    /// Entity already exists
    AlreadyExists,
    /// Invalid input or failed validation
    InvalidInput,
    /// Access or permissions error
    AccessDenied,
    /// Timeout expired
    Timeout,
    /// Internal system error
    InternalError,
    /// Functionality not implemented
    NotImplemented,
    /// Unsupported operation
    UnsupportedOperation,
    /// Database error
    DatabaseError,
    /// Storage quota exceeded
    QuotaExceeded,
    /// State conflict — the request is well-formed and permitted, but
    /// the resource is in a state that refuses it (e.g. "drive must
    /// be empty before delete"). Maps to HTTP 409. Distinct from
    /// `AlreadyExists` (which is a uniqueness violation) so audit
    /// readers can tell them apart.
    Conflict,
    /// RFC 7232 precondition failure — a caller-supplied conditional
    /// (If-Match, or an internal compare-and-swap standing in for one)
    /// did not hold against the resource's current state. Maps to
    /// HTTP 412. Distinct from `Conflict` (409): this is specifically
    /// "the state you thought you were writing against has moved."
    PreconditionFailed,
    /// A `sync-token` (RFC 6578) predates the server's change-log
    /// retention window — the client must discard local state and
    /// restart with a fresh initial sync. Maps to HTTP 507, distinct
    /// from `QuotaExceeded` (also 507, but for storage-space
    /// exhaustion) so audit readers can tell the two 507 causes apart.
    SyncTokenExpired,
}

impl ErrorKind {
    /// Stable human-readable name; `Display` delegates here so the two can
    /// never drift. Being `&'static` it lets the HTTP error path borrow the
    /// value instead of allocating per response (benches/ROUND11.md §9).
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrorKind::NotFound => "Not Found",
            ErrorKind::AlreadyExists => "Already Exists",
            ErrorKind::InvalidInput => "Invalid Input",
            ErrorKind::AccessDenied => "Access Denied",
            ErrorKind::Timeout => "Timeout",
            ErrorKind::InternalError => "Internal Error",
            ErrorKind::NotImplemented => "Not Implemented",
            ErrorKind::UnsupportedOperation => "Unsupported Operation",
            ErrorKind::DatabaseError => "Database Error",
            ErrorKind::QuotaExceeded => "Quota Exceeded",
            ErrorKind::Conflict => "Conflict",
            ErrorKind::PreconditionFailed => "Precondition Failed",
            ErrorKind::SyncTokenExpired => "Sync Token Expired",
        }
    }
}

impl Display for ErrorKind {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.write_str(self.as_str())
    }
}

/// Base domain error that provides detailed context
#[derive(Error, Debug)]
#[error("{kind}: {message}")]
pub struct DomainError {
    /// Error type
    pub kind: ErrorKind,
    /// Affected entity type (e.g.: "File", "Folder")
    pub entity_type: &'static str,
    /// Entity identifier if available
    pub entity_id: Option<String>,
    /// Descriptive error message
    pub message: String,
    /// Source error (optional)
    #[source]
    pub source: Option<Box<dyn StdError + Send + Sync>>,
}

impl DomainError {
    /// Creates a new domain error
    pub fn new<S: Into<String>>(kind: ErrorKind, entity_type: &'static str, message: S) -> Self {
        Self {
            kind,
            entity_type,
            entity_id: None,
            message: message.into(),
            source: None,
        }
    }

    /// Creates an entity not found error
    pub fn not_found<S: Into<String>>(entity_type: &'static str, entity_id: S) -> Self {
        let id = entity_id.into();
        // Message first, then move the id — the old `Some(id.clone())`
        // paid an extra allocation on every 404 construction.
        let message = format!("{} not found: {}", entity_type, id);
        Self {
            kind: ErrorKind::NotFound,
            entity_type,
            entity_id: Some(id),
            message,
            source: None,
        }
    }

    /// Creates an entity already exists error
    pub fn already_exists<S: Into<String>>(entity_type: &'static str, entity_id: S) -> Self {
        let id = entity_id.into();
        let message = format!("{} already exists: {}", entity_type, id);
        Self {
            kind: ErrorKind::AlreadyExists,
            entity_type,
            entity_id: Some(id),
            message,
            source: None,
        }
    }

    /// Creates an error for unsupported operations
    pub fn operation_not_supported<S: Into<String>>(entity_type: &'static str, message: S) -> Self {
        Self::new(ErrorKind::UnsupportedOperation, entity_type, message)
    }

    /// Creates a timeout error
    pub fn timeout<S: Into<String>>(entity_type: &'static str, message: S) -> Self {
        Self {
            kind: ErrorKind::Timeout,
            entity_type,
            entity_id: None,
            message: message.into(),
            source: None,
        }
    }

    /// Creates an internal error
    pub fn internal_error<S: Into<String>>(entity_type: &'static str, message: S) -> Self {
        Self {
            kind: ErrorKind::InternalError,
            entity_type,
            entity_id: None,
            message: message.into(),
            source: None,
        }
    }

    /// Creates an access denied error
    pub fn access_denied<S: Into<String>>(entity_type: &'static str, message: S) -> Self {
        Self {
            kind: ErrorKind::AccessDenied,
            entity_type,
            entity_id: None,
            message: message.into(),
            source: None,
        }
    }

    /// Alias for access_denied to maintain compatibility
    pub fn unauthorized<S: Into<String>>(message: S) -> Self {
        Self {
            kind: ErrorKind::AccessDenied,
            entity_type: "Authorization",
            entity_id: None,
            message: message.into(),
            source: None,
        }
    }

    /// Creates a database error
    pub fn database_error<S: Into<String>>(message: S) -> Self {
        Self {
            kind: ErrorKind::DatabaseError,
            entity_type: "Database",
            entity_id: None,
            message: message.into(),
            source: None,
        }
    }

    /// Creates a storage quota exceeded error
    pub fn quota_exceeded<S: Into<String>>(message: S) -> Self {
        Self {
            kind: ErrorKind::QuotaExceeded,
            entity_type: "Storage",
            entity_id: None,
            message: message.into(),
            source: None,
        }
    }

    /// Creates a precondition-failed error (RFC 7232 / CAS mismatch)
    pub fn precondition_failed<S: Into<String>>(entity_type: &'static str, message: S) -> Self {
        Self {
            kind: ErrorKind::PreconditionFailed,
            entity_type,
            entity_id: None,
            message: message.into(),
            source: None,
        }
    }

    /// Creates a sync-token-expired error (RFC 6578 §3.6 — client's
    /// token predates the change-log retention window).
    pub fn sync_token_expired<S: Into<String>>(entity_type: &'static str, message: S) -> Self {
        Self {
            kind: ErrorKind::SyncTokenExpired,
            entity_type,
            entity_id: None,
            message: message.into(),
            source: None,
        }
    }

    /// Creates a validation error
    pub fn validation_error<S: Into<String>>(message: S) -> Self {
        Self {
            kind: ErrorKind::InvalidInput,
            entity_type: "Validation",
            entity_id: None,
            message: message.into(),
            source: None,
        }
    }

    /// Creates a not implemented error
    pub fn not_implemented<S: Into<String>>(entity_type: &'static str, message: S) -> Self {
        Self {
            kind: ErrorKind::NotImplemented,
            entity_type,
            entity_id: None,
            message: message.into(),
            source: None,
        }
    }

    /// Sets the entity ID
    pub fn with_id<S: Into<String>>(mut self, entity_id: S) -> Self {
        self.entity_id = Some(entity_id.into());
        self
    }

    /// Sets the source error
    pub fn with_source<E: StdError + Send + Sync + 'static>(mut self, source: E) -> Self {
        self.source = Some(Box::new(source));
        self
    }
}

/// Trait for adding context to errors
pub trait ErrorContext<T, E> {
    fn with_context<C, F>(self, context: F) -> std::result::Result<T, DomainError>
    where
        C: Into<String>,
        F: FnOnce() -> C;

    fn with_error_kind(
        self,
        kind: ErrorKind,
        entity_type: &'static str,
    ) -> std::result::Result<T, DomainError>;
}

impl<T, E: StdError + Send + Sync + 'static> ErrorContext<T, E> for std::result::Result<T, E> {
    fn with_context<C, F>(self, context: F) -> std::result::Result<T, DomainError>
    where
        C: Into<String>,
        F: FnOnce() -> C,
    {
        self.map_err(|e| DomainError {
            kind: ErrorKind::InternalError,
            entity_type: "Unknown",
            entity_id: None,
            message: context().into(),
            source: Some(Box::new(e)),
        })
    }

    fn with_error_kind(
        self,
        kind: ErrorKind,
        entity_type: &'static str,
    ) -> std::result::Result<T, DomainError> {
        self.map_err(|e| DomainError {
            kind,
            entity_type,
            entity_id: None,
            message: format!("{}", e),
            source: Some(Box::new(e)),
        })
    }
}

// From implementations for standard errors (without external infrastructure dependencies)
impl From<std::io::Error> for DomainError {
    fn from(err: std::io::Error) -> Self {
        DomainError {
            kind: ErrorKind::InternalError,
            entity_type: "IO",
            entity_id: None,
            message: format!("{}", err),
            source: Some(Box::new(err)),
        }
    }
}

impl From<uuid::Error> for DomainError {
    fn from(err: uuid::Error) -> Self {
        DomainError {
            kind: ErrorKind::InvalidInput,
            entity_type: "UUID",
            entity_id: None,
            message: format!("{}", err),
            source: Some(Box::new(err)),
        }
    }
}
