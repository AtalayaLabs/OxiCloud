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
    /// A dependency failed in a way that may clear on its own — an HTTP
    /// 5xx or 429 from object storage, a connection reset, a DNS
    /// failure.
    ///
    /// Distinct from [`ErrorKind::InternalError`] because the engine has
    /// to tell "the provider is down" from "this data is wrong": the
    /// first is worth retrying and then pausing so an operator can
    /// resume, the second is terminal. Flattening both into
    /// `InternalError` is what forced `RetryBlobBackend` to classify by
    /// string-matching `Display` output — fragile in exactly the way
    /// that turns an SDK's cosmetic reformat into a silent behaviour
    /// change.
    ///
    /// **Set it deliberately, at the point where the status code is
    /// still visible** — the port wrapping the SDK error. By the time an
    /// error reaches the engine, the code survives only inside a
    /// formatted string.
    ///
    /// Not a promise that a retry succeeds. A deterministic 500 (Azurite
    /// answering the CRC64 ranged GET) is a permanent fault wearing a
    /// retryable status code, which no status-based taxonomy can get
    /// right — the bounded attempt cap is the safety net for exactly
    /// that. See `docs/plan/jobs-handling-recoverable-error.md`.
    TransientBackend,
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
            // Wire value — the SPA switches on `error_type`, so this
            // string is a contract. Additive here; nothing keys off it
            // yet.
            ErrorKind::TransientBackend => "Transient Backend",
            ErrorKind::InternalError => "Internal Error",
            ErrorKind::NotImplemented => "Not Implemented",
            ErrorKind::UnsupportedOperation => "Unsupported Operation",
            ErrorKind::DatabaseError => "Database Error",
            ErrorKind::QuotaExceeded => "Quota Exceeded",
            ErrorKind::Conflict => "Conflict",
            ErrorKind::PreconditionFailed => "Precondition Failed",
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

    /// A dependency failed in a way that may clear on its own. See
    /// [`ErrorKind::TransientBackend`] for what qualifies and why the
    /// classification belongs at the port rather than downstream.
    pub fn transient_backend<S: Into<String>>(entity_type: &'static str, message: S) -> Self {
        Self {
            kind: ErrorKind::TransientBackend,
            entity_type,
            entity_id: None,
            message: message.into(),
            source: None,
        }
    }

    /// Whether retrying this operation could plausibly succeed.
    ///
    /// The single place that answers the question, so a retry decorator
    /// and the job engine cannot disagree about the same error — they
    /// did while the answer was `Display` string-matching in one of
    /// them and nothing in the other.
    ///
    /// `Timeout` is included because it is transient by construction;
    /// everything else must say so explicitly via
    /// [`ErrorKind::TransientBackend`]. Defaulting to "not retryable" is
    /// the safe direction: a missed retry surfaces as a visible failure,
    /// whereas retrying a permanent fault burns attempts and, in the
    /// job engine, holds `migration_readonly` while it does.
    pub fn is_transient(&self) -> bool {
        matches!(self.kind, ErrorKind::Timeout | ErrorKind::TransientBackend)
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

#[cfg(test)]
mod transient_tests {
    use super::*;

    /// The retry decorator and the job engine both branch on this, so
    /// the set has to be deliberate rather than incidental.
    #[test]
    fn only_timeout_and_transient_backend_are_retryable() {
        assert!(DomainError::transient_backend("S3", "503").is_transient());
        assert!(DomainError::timeout("S3", "read timed out").is_transient());

        // Everything else defaults to permanent. Retrying a genuine
        // fault burns attempts and, in the job engine, holds
        // `migration_readonly` while it does — so the default has to be
        // "no".
        for e in [
            DomainError::internal_error("S3", "decode failed"),
            DomainError::new(ErrorKind::NotFound, "Blob", "missing"),
            DomainError::new(ErrorKind::AccessDenied, "S3", "bad credentials"),
            DomainError::new(ErrorKind::InvalidInput, "S3", "malformed key"),
            DomainError::new(ErrorKind::UnsupportedOperation, "S3", "no enumeration"),
        ] {
            assert!(
                !e.is_transient(),
                "{:?} must not be retryable by default",
                e.kind
            );
        }
    }

    /// `error_type` is a wire contract the SPA switches on, so this
    /// string is not free to churn.
    #[test]
    fn transient_backend_has_a_stable_wire_name() {
        assert_eq!(ErrorKind::TransientBackend.as_str(), "Transient Backend");
    }
}
