//! Standard types for cursor-based, sortable listing endpoints.
//!
//! All `GET` endpoints that return a collection **must** use these types so
//! that every listing is consistent for API consumers.
//!
//! # Quick start
//!
//! ```rust,ignore
//! // 1. Define a cursor for your endpoint
//! #[derive(Serialize, Deserialize)]
//! pub struct MyCursor { pub created_at: DateTime<Utc>, pub id: Uuid }
//! impl PageCursor for MyCursor {}          // encode/decode for free
//!
//! // 2. Compose the standard query params
//! #[derive(Deserialize, IntoParams)]
//! pub struct MyQuery {
//!     #[serde(flatten)]
//!     pub paging: CursorQuery,
//!     pub my_filter: Option<String>,      // endpoint-specific extras
//! }
//!
//! // 3. Return the standard envelope
//! async fn list_things(Query(q): Query<MyQuery>, …) -> Json<CursorListResponse<ThingDto>> {
//!     let limit  = q.paging.limit_clamped();
//!     let cursor = q.paging.decode_cursor::<MyCursor>();
//!     // fetch limit+1 rows …
//!     Json(CursorListResponse::from_oversized(rows, limit, |r| MyCursor { … }))
//! }
//! ```

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
// ToSchema is used on CursorQuery so it can be flattened into IntoParams structs

// ════════════════════════════════════════════════════════════════════════════
// PageCursor trait
// ════════════════════════════════════════════════════════════════════════════

/// Marker trait for opaque keyset-pagination cursors.
///
/// The default `encode` / `decode` implementations use
/// URL-safe base64url (no padding) over a JSON serialisation of `Self`.
/// Any struct that derives `Serialize + Deserialize` can implement this
/// with a bare `impl PageCursor for MyCursor {}`.
///
/// The encoding is intentionally opaque to API callers. Treat an
/// undecodable cursor as "start from the top" — never return an error.
pub trait PageCursor: Sized + Serialize + for<'de> Deserialize<'de> {
    /// Encode `self` as a URL-safe, no-padding base64url string.
    fn encode(&self) -> String {
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(self).unwrap_or_default())
    }

    /// Decode from a base64url string.  Returns `None` on any parse failure.
    fn decode(s: &str) -> Option<Self> {
        let bytes = URL_SAFE_NO_PAD.decode(s).ok()?;
        serde_json::from_slice(&bytes).ok()
    }
}

// ════════════════════════════════════════════════════════════════════════════
// CursorQuery — standard query params
// ════════════════════════════════════════════════════════════════════════════

/// Standard query parameters for cursor-based listing endpoints.
///
/// Use `CursorQuery` directly as the `Query<CursorQuery>` extractor when an
/// endpoint has no extra filter params.  When extra params are needed, declare
/// them in an endpoint-specific struct and **repeat** the three fields — Axum's
/// query extractor uses `serde_urlencoded` which does not support
/// `#[serde(flatten)]`.  Use `CursorQuery::default_limit()` for the default
/// and the helpers `limit_clamped()` / `decode_cursor()` by either calling
/// them on `CursorQuery` directly or re-implementing them inline:
///
/// ```rust,ignore
/// #[derive(Deserialize, IntoParams)]
/// pub struct MyQuery {
///     #[serde(default = "CursorQuery::default_limit")]
///     pub limit: u32,
///     pub cursor: Option<String>,
///     pub sort_by: Option<String>,
///     pub status: Option<String>,    // endpoint-specific
/// }
/// ```
#[derive(Debug, Deserialize, IntoParams, ToSchema)]
pub struct CursorQuery {
    /// Maximum items per page (1–200, default 50).
    #[serde(default = "CursorQuery::default_limit")]
    pub limit: u32,
    /// Opaque cursor from a previous response. Absent on the first page.
    pub cursor: Option<String>,
    /// Sort dimension.  Valid values are endpoint-defined (e.g. `"granted_at"`,
    /// `"name"`, `"granted_by"`).  Unknown values should return HTTP 400.
    pub sort_by: Option<String>,
}

impl CursorQuery {
    /// Default value for the `limit` field — exposed `pub` so endpoint-specific
    /// query structs can reference it in `#[serde(default = "CursorQuery::default_limit")]`.
    pub fn default_limit() -> u32 {
        50
    }

    /// Returns `limit` clamped to `[1, 200]`.
    pub fn limit_clamped(&self) -> usize {
        self.limit.clamp(1, 200) as usize
    }

    /// Decode the optional cursor string into type `C`.
    /// Returns `None` when no cursor is present or when decoding fails
    /// (invalid cursor → start from the top).
    pub fn decode_cursor<C: PageCursor>(&self) -> Option<C> {
        self.cursor.as_deref().and_then(C::decode)
    }
}

// ════════════════════════════════════════════════════════════════════════════
// CursorListResponse — standard response envelope
// ════════════════════════════════════════════════════════════════════════════

/// Standard response envelope for cursor-paginated listing endpoints.
///
/// `next_cursor` is omitted from the JSON when `None` (i.e. last page).
/// Callers must treat a missing `next_cursor` as end-of-results — never
/// include a `total` count (that would require an expensive `COUNT(*)`).
#[derive(Debug, Serialize, ToSchema)]
pub struct CursorListResponse<T: Serialize> {
    pub items: Vec<T>,
    /// Opaque cursor for the next page.  Absent when this is the last page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

impl<T: Serialize> CursorListResponse<T> {
    /// Build a response from an over-fetched slice (fetch `limit + 1` rows).
    ///
    /// If `items.len() > limit` a next page exists: `items` is truncated to
    /// `limit` and `cursor_fn` is called on the **last kept item** to produce
    /// the next cursor.  Otherwise `next_cursor` is `None`.
    pub fn from_oversized<C: PageCursor>(
        mut items: Vec<T>,
        limit: usize,
        cursor_fn: impl FnOnce(&T) -> C,
    ) -> Self {
        let next_cursor = if items.len() > limit {
            let c = cursor_fn(&items[limit - 1]);
            items.truncate(limit);
            Some(c.encode())
        } else {
            None
        };
        Self { items, next_cursor }
    }

    /// Build a response when the next cursor is already known (e.g. returned
    /// by a service layer that handles the `limit+1` logic internally).
    pub fn with_cursor(items: Vec<T>, next_cursor: Option<String>) -> Self {
        Self { items, next_cursor }
    }

    /// Compute a page-identity ETag for `If-None-Match` revalidation.
    ///
    /// Shared across every `/resources` endpoint (§2 of
    /// `docs/plan/photos-resources-migration.md`): the hash mixes the
    /// cursor input, limit, a per-endpoint freshness signal, the row
    /// count, and the next-page cursor so a page transitioning from
    /// "has next" to "last page" invalidates on its own.
    ///
    /// `fresh_signal` is endpoint-specific — the right thing to feed
    /// in differs by surface:
    ///
    /// - photos:     `max(media_sort_date)` across returned rows
    /// - favorites:  `max(favorited_at)`
    /// - recents:    `max(accessed_at)`
    /// - trash:      `max(trashed_at)`
    /// - folders:    `max(modified_at)` over the folder's children
    ///
    /// Empty-page semantics: callers pass `0` when the vec is empty.
    /// A `0` signal is still legitimate — the hash collapses to
    /// `(cursor, limit, 0, 0, None)`, which revalidates stably across
    /// a repeatedly-empty page (new-user onboarding, cleaned-out
    /// trash) and invalidates the moment the first row appears
    /// (`len == 1`, non-zero signal).
    ///
    /// Deliberately NOT a hash of the serialised body: the point is
    /// to short-circuit BEFORE serialisation, and the body shape is
    /// stable so cursor + limit + freshness is a correct identity
    /// for "nothing changed on this page". Strong-form syntactically
    /// (no `W/` prefix) — our deterministic `ORDER BY <axis>, id` on
    /// every listing makes the body byte-identical given the same
    /// inputs, so the client's HTTP cache can treat the ETag as
    /// strong without risk.
    pub fn weak_etag(&self, cursor_input: Option<&str>, limit: usize, fresh_signal: u64) -> String {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        cursor_input.hash(&mut hasher);
        limit.hash(&mut hasher);
        fresh_signal.hash(&mut hasher);
        self.items.len().hash(&mut hasher);
        self.next_cursor.hash(&mut hasher);
        format!("\"{:x}\"", hasher.finish())
    }
}
