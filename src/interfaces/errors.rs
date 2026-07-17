//! HTTP/API Error types for the interfaces layer.
//!
//! This module contains error types specific to the HTTP/API layer.
//! These errors handle the conversion from domain errors to HTTP responses.

use std::borrow::Cow;

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use crate::domain::errors::{DomainError, ErrorKind};

/// Error type for HTTP/API responses.
///
/// This struct represents errors that will be returned to HTTP clients.
/// It contains the HTTP status code, a user-friendly message, and an error
/// type identifier. `error_type` is a `Cow`: every built-in constructor and
/// the `DomainError` conversion use a `&'static str` from a closed set, so
/// the common 4xx path allocates nothing for it (benches/ROUND11.md §9).
#[derive(Debug)]
pub struct AppError {
    pub status_code: StatusCode,
    pub message: String,
    pub error_type: Cow<'static, str>,
}

/// JSON response structure for errors, borrowing from the `AppError` it
/// renders — `error` and `message` intentionally serialize the SAME string
/// for backwards compatibility (legacy handlers returned `{"error": …}`,
/// AppError returned `{"message": …}`); serializing one buffer twice
/// replaces the old per-response deep clone.
#[derive(Serialize)]
pub struct ErrorResponse<'a> {
    pub status: &'a str,
    pub error: &'a str,
    pub message: &'a str,
    pub error_type: &'a str,
}

impl AppError {
    /// Create a new AppError with custom status code, message and error type.
    pub fn new(
        status_code: StatusCode,
        message: impl Into<String>,
        error_type: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self {
            status_code,
            message: message.into(),
            error_type: error_type.into(),
        }
    }

    /// Create a 400 Bad Request error.
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message, "BadRequest")
    }

    /// Create a 401 Unauthorized error.
    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, message, "Unauthorized")
    }

    /// Create a 403 Forbidden error.
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, message, "Forbidden")
    }

    /// Create a 404 Not Found error.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message, "NotFound")
    }

    /// Create a 500 Internal Server Error.
    pub fn internal_error(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message, "InternalError")
    }

    /// Create a 405 Method Not Allowed error.
    pub fn method_not_allowed(message: impl Into<String>) -> Self {
        Self::new(StatusCode::METHOD_NOT_ALLOWED, message, "MethodNotAllowed")
    }

    /// Create a 409 Conflict error.
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, message, "Conflict")
    }

    /// Create a 423 Locked error (WebDAV).
    pub fn locked(message: impl Into<String>) -> Self {
        Self::new(StatusCode::LOCKED, message, "Locked")
    }

    /// Create a 415 Unsupported Media Type error.
    pub fn unsupported_media_type(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            message,
            "UnsupportedMediaType",
        )
    }

    /// Create a 412 Precondition Failed error.
    pub fn precondition_failed(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::PRECONDITION_FAILED,
            message,
            "PreconditionFailed",
        )
    }

    /// Create a 413 Payload Too Large error.
    pub fn payload_too_large(message: impl Into<String>) -> Self {
        Self::new(StatusCode::PAYLOAD_TOO_LARGE, message, "PayloadTooLarge")
    }

    /// Create a 507 Insufficient Storage error for an expired RFC 6578
    /// `sync-token` (client must discard local state and restart with a
    /// fresh initial sync). Distinct from `ErrorKind::QuotaExceeded`,
    /// which also maps to 507 but for actual storage-space exhaustion.
    pub fn sync_token_expired(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::INSUFFICIENT_STORAGE,
            message,
            "SyncTokenExpired",
        )
    }
}

impl From<DomainError> for AppError {
    fn from(err: DomainError) -> Self {
        let status_code = match err.kind {
            ErrorKind::NotFound => StatusCode::NOT_FOUND,
            ErrorKind::AlreadyExists => StatusCode::CONFLICT,
            ErrorKind::InvalidInput => StatusCode::BAD_REQUEST,
            ErrorKind::AccessDenied => StatusCode::FORBIDDEN,
            ErrorKind::Timeout => StatusCode::REQUEST_TIMEOUT,
            ErrorKind::InternalError => StatusCode::INTERNAL_SERVER_ERROR,
            ErrorKind::NotImplemented => StatusCode::NOT_IMPLEMENTED,
            ErrorKind::UnsupportedOperation => StatusCode::METHOD_NOT_ALLOWED,
            ErrorKind::DatabaseError => StatusCode::INTERNAL_SERVER_ERROR,
            ErrorKind::QuotaExceeded => StatusCode::INSUFFICIENT_STORAGE,
            ErrorKind::Conflict => StatusCode::CONFLICT,
            ErrorKind::PreconditionFailed => StatusCode::PRECONDITION_FAILED,
            ErrorKind::SyncTokenExpired => StatusCode::INSUFFICIENT_STORAGE,
        };

        Self {
            status_code,
            message: err.message,
            error_type: Cow::Borrowed(err.kind.as_str()),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.status_code;

        // Sanitize 500 Internal Server Error to prevent information leakage.
        // Log the full error server-side for debugging, return a generic
        // message to the client. Other status codes (including 5xx like
        // 501, 503, 507) keep their intentionally user-facing messages.
        let client_message: &str = if status == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!(
                error_type = %self.error_type,
                "Internal server error: {}",
                self.message
            );
            "An internal error occurred. Please try again later."
        } else {
            &self.message
        };

        let status_line = status.to_string();
        let error_response = ErrorResponse {
            status: &status_line,
            error: client_message,
            message: client_message,
            error_type: &self.error_type,
        };

        let body = Json(&error_response);
        (status, body).into_response()
    }
}
