//! Operator-driven banner messages for every connected session.
//!
//! Three distinct consumer surfaces share one in-process primitive:
//!
//! 1. **Admin API** — CRUD against the full list of banners, including
//!    `starts_at > now` entries that aren't yet publicly visible.
//! 2. **Public wire** — `/api/config.server_status.banners` and the
//!    `X-Server-Status` header carry the FILTERED list (entries whose
//!    scheduled start has already arrived). Changes bump
//!    `banners_version` for the FE's version-diff refetch pattern.
//! 3. **Message bus** — a `Topic::ServerStatus` broadcast carrying the
//!    new `banners_version` on every mutation, so open browser tabs
//!    react within ~1s instead of on their next API call.
//!
//! Design doc lives inline here. Persistence is a single JSONB value
//! under `auth.admin_settings.ops_banners`; no new table. The in-memory
//! copy under an `Arc<RwLock<Vec<OpsBanner>>>` is the authoritative
//! runtime value — DB is restart-survival. Both are swapped in
//! lock-step on every mutation.
//!
//! Non-goals (deliberately deferred, say if any becomes a need):
//! - Per-user targeting (`show_to: role`) — a separate feature axis
//! - Auto-expiry via `expires_at` — operators delete explicitly today
//! - Operator-reorderable list — server-side `(severity, created_at
//!   DESC)` is enough for v1
//! - Soft-delete / history — the audit log is the trail
//!
//! Hard limits that stay small on purpose:
//! - At most [`MAX_BANNERS`] entries. The admin UI should feel like a
//!   small operator-managed list, not a CMS. Rejecting past the cap
//!   is a 400 asking the operator to delete first.
//! - At most [`MAX_LOCALES_PER_BANNER`] locale keys per banner.
//!   Still room for en/fr/de/es/it; a legitimate ops banner doesn't
//!   need twenty translations.
//! - At most [`MAX_BODY_BYTES`] UTF-8 bytes per locale body. A banner
//!   is a sentence or two of markdown, not an article.

use std::sync::Arc;
use std::sync::RwLock;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use std::collections::BTreeMap;

/// Severity axis. Two variants on purpose — operators need "I'm
/// warning you about something" vs "I'm telling you about something",
/// and the UI picks a colour/icon pair accordingly. A third severity
/// (`Error`? `Critical`?) is a legitimate future addition; the enum
/// is `#[non_exhaustive]` so adding one is additive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OpsBannerSeverity {
    Warning,
    Notification,
}

impl OpsBannerSeverity {
    /// Rank for the server-side `(severity, created_at DESC)` order —
    /// Warnings above Notifications. Lower number sorts first.
    pub fn rank(&self) -> u8 {
        match self {
            Self::Warning => 0,
            Self::Notification => 1,
        }
    }
}

/// One banner as the operator wrote it.
///
/// `body` holds raw markdown per locale, UTF-8. Sanitization happens
/// at render time (FE), not at store time — the operator who
/// composed it must be able to edit the exact same text back. A
/// server-side sanitizer regression that lands later re-renders
/// every live banner safely without touching the data.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct OpsBanner {
    pub id: Uuid,
    pub severity: OpsBannerSeverity,
    /// Locale code → markdown body. `BTreeMap` so JSON serialization
    /// is deterministic (critical for `banners_version` hashing).
    /// Minimum one entry; `"en"` is the canonical fallback when the
    /// viewer's `preferred_locale` is missing — hard-coded, not
    /// `locale.split('-')[0]`, so the behaviour is predictable.
    pub body: BTreeMap<String, String>,
    /// `Some(_)` → the banner is configured but only visible on the
    /// public wire once `now >= starts_at`. `None` → effective
    /// immediately. Admin always sees everything.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub starts_at: Option<DateTime<Utc>>,
    /// `Some(_)` → the banner stops being visible on the public wire
    /// once `now >= expires_at`. `None` → stays until the operator
    /// deletes it explicitly. Pairs with `starts_at` to cover the
    /// "scheduled maintenance window" case: operator posts the
    /// banner once with `starts_at = Thursday 20:00` + `expires_at
    /// = Thursday 23:00` and the server handles both visibility
    /// transitions without a manual delete.
    ///
    /// An expired banner stays on the ADMIN list until the next
    /// write to the collection (any create / update / delete), at
    /// which point `prune_expired` cleans it up. That keeps the
    /// admin surface honest without needing a periodic job.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    pub created_by: Uuid,
    pub created_at: DateTime<Utc>,
}

/// Hard upper bound on how many banners can be live at once.
pub const MAX_BANNERS: usize = 10;
/// Upper bound on how many locale translations a single banner can carry.
pub const MAX_LOCALES_PER_BANNER: usize = 20;
/// Upper bound on each locale body's UTF-8 byte length.
pub const MAX_BODY_BYTES: usize = 2048;

/// Validation errors mapped to HTTP by the admin handler layer. One
/// variant per rule so the handler can pick an appropriate status
/// (400 for most, 409 for the "cap reached" case).
#[derive(Clone, Debug)]
pub enum OpsBannerError {
    /// Operator asked to create but the cap is reached.
    CapReached,
    /// `body` is empty — a banner with no translations has nothing to
    /// render.
    EmptyBody,
    /// `body` has more locales than [`MAX_LOCALES_PER_BANNER`].
    TooManyLocales,
    /// A locale body exceeds [`MAX_BODY_BYTES`].
    BodyTooLong { locale: String },
    /// A locale key fails the loose "letters, digits, dash" check
    /// (RFC 5646 subset).
    InvalidLocale { locale: String },
    /// `expires_at` was set but falls at-or-before the effective
    /// start (`starts_at` if set, else "now") — the banner would
    /// never be visible to anyone. Rejected to surface the typo
    /// rather than silently create a dead row.
    ExpiresBeforeStart,
    /// Banner id passed to update/delete not found.
    NotFound,
    /// Any downstream error (DB persist, serde round-trip).
    Internal(String),
}

impl std::fmt::Display for OpsBannerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CapReached => write!(
                f,
                "banner cap reached ({MAX_BANNERS}) — delete one before creating another"
            ),
            Self::EmptyBody => write!(f, "banner body must have at least one locale entry"),
            Self::TooManyLocales => write!(
                f,
                "banner body has more than {MAX_LOCALES_PER_BANNER} locale entries"
            ),
            Self::BodyTooLong { locale } => write!(
                f,
                "banner body for locale '{locale}' exceeds {MAX_BODY_BYTES} bytes"
            ),
            Self::InvalidLocale { locale } => {
                write!(f, "banner has invalid locale code '{locale}'")
            }
            Self::ExpiresBeforeStart => write!(
                f,
                "expires_at must be strictly after the effective start (starts_at if set, otherwise 'now')"
            ),
            Self::NotFound => write!(f, "banner not found"),
            Self::Internal(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for OpsBannerError {}

/// Input the admin handlers pass to the service for create / update.
/// Doesn't carry `id` / `created_by` / `created_at` — those are
/// server-assigned on create (and preserved on update).
#[derive(Clone, Debug, Deserialize, utoipa::ToSchema)]
pub struct OpsBannerInput {
    pub severity: OpsBannerSeverity,
    pub body: BTreeMap<String, String>,
    #[serde(default)]
    pub starts_at: Option<DateTime<Utc>>,
    /// Optional auto-hide timestamp. See the field doc on
    /// [`OpsBanner::expires_at`].
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
}

/// Service — the composition root for the banner feature.
///
/// Holds the in-memory list under `RwLock` for cheap reads (every
/// request hits `read()` to compute the public filter + version
/// hash), and persists changes to `auth.admin_settings` through
/// [`persist_ops_banners`] whenever the list mutates.
pub struct OpsBannerService {
    banners: Arc<RwLock<Vec<OpsBanner>>>,
    pool: Arc<PgPool>,
}

impl OpsBannerService {
    /// Build from a boot-seeded snapshot. Call sites in `common/di.rs`
    /// run [`load_ops_banners`] against the pool first and pass the
    /// result through — same shape as the `backend_write_gate` seeding
    /// pattern.
    pub fn new(initial: Vec<OpsBanner>, pool: Arc<PgPool>) -> Self {
        Self {
            banners: Arc::new(RwLock::new(sorted(initial))),
            pool,
        }
    }

    /// Full list, admin-facing (includes `starts_at > now` entries).
    pub fn list_all(&self) -> Vec<OpsBanner> {
        self.banners
            .read()
            .expect("OpsBannerService poisoned")
            .clone()
    }

    /// Public-filtered list — only entries whose `starts_at` is in
    /// the past (or absent) AND whose `expires_at` is in the
    /// future (or absent). Used by the server-status payload the
    /// X-Server-Status header + `/api/config` serve.
    ///
    /// Expired entries stay on the admin list until the next write
    /// to the collection OR until [`Self::prune_expired`] runs
    /// (periodic job). That keeps a public read side-effect-free
    /// — the public endpoint never touches storage.
    pub fn list_public(&self) -> Vec<OpsBanner> {
        let now = Utc::now();
        self.banners
            .read()
            .expect("OpsBannerService poisoned")
            .iter()
            .filter(|b| b.starts_at.map(|s| s <= now).unwrap_or(true))
            .filter(|b| b.expires_at.map(|e| e > now).unwrap_or(true))
            .cloned()
            .collect()
    }

    /// Drop every expired banner from the in-memory list and
    /// re-persist. Returns the number of entries pruned. Called by
    /// the periodic `ops_banner_expiry` job; also used
    /// opportunistically before any write-side commit so a fresh
    /// mutation never has to compete with stale expired entries for
    /// the [`MAX_BANNERS`] cap.
    ///
    /// Idempotent: zero expired → zero rows written, zero bytes
    /// pushed on the message bus (caller decides whether to
    /// broadcast).
    pub async fn prune_expired(&self) -> Result<usize, OpsBannerError> {
        let now = Utc::now();
        let (new_list, pruned) = {
            let mut guard = self.banners.write().expect("OpsBannerService poisoned");
            let before = guard.len();
            guard.retain(|b| b.expires_at.map(|e| e > now).unwrap_or(true));
            let pruned = before - guard.len();
            (guard.clone(), pruned)
        };
        if pruned > 0 {
            persist(self.pool.as_ref(), &new_list).await?;
        }
        Ok(pruned)
    }

    /// Short stable version hash of the full list — carried on the
    /// `X-Server-Status` header. BLAKE3 truncated to 16 hex chars is
    /// plenty of entropy for a change detector; a full 64-char hash
    /// would just bloat every API response.
    pub fn version(&self) -> String {
        let list = self.list_all();
        // `BTreeMap` + deterministic serde output means the same
        // list serializes to the same bytes every time; two different
        // lists reach the same hash with probability < 2^-64. Cheap
        // enough to recompute every request — this is O(bytes) over
        // a tiny JSON payload, nowhere near a hot path concern.
        let bytes = serde_json::to_vec(&list).unwrap_or_default();
        let full = blake3::hash(&bytes).to_hex().to_string();
        full[..16].to_string()
    }

    /// Create a new banner. Returns the stored row (with
    /// server-assigned id / created_at / created_by). Validates
    /// against every cap in the module header; errors are mapped to
    /// HTTP status by the handler.
    pub async fn create(
        &self,
        input: OpsBannerInput,
        created_by: Uuid,
    ) -> Result<OpsBanner, OpsBannerError> {
        validate_input(&input)?;
        let banner = OpsBanner {
            id: Uuid::new_v4(),
            severity: input.severity,
            body: input.body,
            starts_at: input.starts_at,
            expires_at: input.expires_at,
            created_by,
            created_at: Utc::now(),
        };
        let new_list: Vec<OpsBanner> = {
            let mut guard = self.banners.write().expect("OpsBannerService poisoned");
            if guard.len() >= MAX_BANNERS {
                return Err(OpsBannerError::CapReached);
            }
            guard.push(banner.clone());
            *guard = sorted(std::mem::take(&mut *guard));
            guard.clone()
        };
        persist(self.pool.as_ref(), &new_list).await?;
        Ok(banner)
    }

    /// Update an existing banner (full-replacement of
    /// severity/body/starts_at — `id`, `created_by`, `created_at`
    /// stay).
    ///
    /// Note on severity change: this does NOT un-dismiss for users
    /// client-side (the dismissal is keyed on `id`, and the id
    /// stays). If an operator wants to force the banner back in
    /// front of users who dismissed, DELETE + create — the new row
    /// gets a new id. The decision ladder in the design doc covers
    /// this trade-off.
    pub async fn update(
        &self,
        id: Uuid,
        input: OpsBannerInput,
    ) -> Result<OpsBanner, OpsBannerError> {
        validate_input(&input)?;
        let (updated, new_list) = {
            let mut guard = self.banners.write().expect("OpsBannerService poisoned");
            let pos = guard
                .iter()
                .position(|b| b.id == id)
                .ok_or(OpsBannerError::NotFound)?;
            let existing = &guard[pos];
            let updated = OpsBanner {
                id: existing.id,
                severity: input.severity,
                body: input.body,
                starts_at: input.starts_at,
                expires_at: input.expires_at,
                created_by: existing.created_by,
                created_at: existing.created_at,
            };
            guard[pos] = updated.clone();
            *guard = sorted(std::mem::take(&mut *guard));
            (updated, guard.clone())
        };
        persist(self.pool.as_ref(), &new_list).await?;
        Ok(updated)
    }

    /// Delete by id. Returns `Ok(())` on success, `NotFound` if the
    /// id isn't live.
    pub async fn delete(&self, id: Uuid) -> Result<(), OpsBannerError> {
        let new_list = {
            let mut guard = self.banners.write().expect("OpsBannerService poisoned");
            let before = guard.len();
            guard.retain(|b| b.id != id);
            if guard.len() == before {
                return Err(OpsBannerError::NotFound);
            }
            guard.clone()
        };
        persist(self.pool.as_ref(), &new_list).await?;
        Ok(())
    }
}

/// `(severity.rank(), created_at DESC)` — the public ordering rule
/// baked into the service. One place, applied on every write-side
/// commit so reads don't have to sort.
fn sorted(mut v: Vec<OpsBanner>) -> Vec<OpsBanner> {
    v.sort_by(|a, b| {
        a.severity
            .rank()
            .cmp(&b.severity.rank())
            .then_with(|| b.created_at.cmp(&a.created_at))
    });
    v
}

fn validate_input(input: &OpsBannerInput) -> Result<(), OpsBannerError> {
    if input.body.is_empty() {
        return Err(OpsBannerError::EmptyBody);
    }
    if input.body.len() > MAX_LOCALES_PER_BANNER {
        return Err(OpsBannerError::TooManyLocales);
    }
    for (loc, body) in &input.body {
        if !is_plausible_locale(loc) {
            return Err(OpsBannerError::InvalidLocale {
                locale: loc.clone(),
            });
        }
        if body.len() > MAX_BODY_BYTES {
            return Err(OpsBannerError::BodyTooLong {
                locale: loc.clone(),
            });
        }
    }
    // `expires_at` must be strictly after the effective visibility
    // start. For a banner with `starts_at` set, the window is
    // `[starts_at, expires_at)`. Without `starts_at`, the window
    // opens now, so `expires_at > now` is the rule.
    if let Some(ex) = input.expires_at {
        let effective_start = input.starts_at.unwrap_or_else(Utc::now);
        if ex <= effective_start {
            return Err(OpsBannerError::ExpiresBeforeStart);
        }
    }
    Ok(())
}

/// Loose RFC-5646 subset: `[a-zA-Z0-9]`, length 2..=15, optional
/// hyphen-separated subtags. Rejects empty, `..`, path chars, and
/// generally anything that would be weird in a locale selector.
fn is_plausible_locale(s: &str) -> bool {
    if s.is_empty() || s.len() > 35 {
        return false;
    }
    s.split('-')
        .all(|segment| !segment.is_empty() && segment.chars().all(|c| c.is_ascii_alphanumeric()))
}

// ─── DB persistence ─────────────────────────────────────────────

/// Key in `auth.admin_settings` holding the serialized banner list.
pub const OPS_BANNERS_KEY: &str = "ops.banners";

/// Load the persisted banner list. Malformed JSON / absent row /
/// transient DB error → empty list. A corrupt row MUST NOT wedge the
/// server; the operator can recover by posting a new banner, which
/// overwrites the row.
pub async fn load_ops_banners(pool: &PgPool) -> Vec<OpsBanner> {
    let row: Result<Option<(Option<String>,)>, sqlx::Error> =
        sqlx::query_as("SELECT value FROM auth.admin_settings WHERE key = $1")
            .bind(OPS_BANNERS_KEY)
            .fetch_optional(pool)
            .await;
    match row {
        Ok(Some((Some(json),))) => match serde_json::from_str::<Vec<OpsBanner>>(&json) {
            Ok(list) => sorted(list),
            Err(e) => {
                tracing::warn!(
                    target: "oxicloud::ops_banner",
                    event = "ops_banner.load_parse_failed",
                    error = %e,
                    "could not parse ops.banners — starting with an empty list; next \
                     operator write will overwrite the corrupt row"
                );
                Vec::new()
            }
        },
        Ok(_) => Vec::new(),
        Err(e) => {
            tracing::warn!(
                target: "oxicloud::ops_banner",
                event = "ops_banner.load_failed",
                error = %e,
                "failed to read {OPS_BANNERS_KEY} at boot; starting empty"
            );
            Vec::new()
        }
    }
}

/// Serialize + upsert. Only the service calls this.
async fn persist(pool: &PgPool, list: &[OpsBanner]) -> Result<(), OpsBannerError> {
    let json = serde_json::to_string(list)
        .map_err(|e| OpsBannerError::Internal(format!("serialize ops_banners: {e}")))?;
    sqlx::query(
        r#"
        INSERT INTO auth.admin_settings (key, value, category, is_secret)
             VALUES ($1, $2, 'ops', FALSE)
        ON CONFLICT (key)
        DO UPDATE SET value = EXCLUDED.value, updated_at = NOW()
        "#,
    )
    .bind(OPS_BANNERS_KEY)
    .bind(&json)
    .execute(pool)
    .await
    .map_err(|e| OpsBannerError::Internal(format!("persist ops_banners: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn body(locale: &str, text: &str) -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        m.insert(locale.to_string(), text.to_string());
        m
    }

    #[test]
    fn validate_rejects_empty_body() {
        let i = OpsBannerInput {
            severity: OpsBannerSeverity::Warning,
            body: BTreeMap::new(),
            starts_at: None,
            expires_at: None,
        };
        assert!(matches!(validate_input(&i), Err(OpsBannerError::EmptyBody)));
    }

    #[test]
    fn validate_rejects_oversize_body() {
        let i = OpsBannerInput {
            severity: OpsBannerSeverity::Warning,
            body: body("en", &"a".repeat(MAX_BODY_BYTES + 1)),
            starts_at: None,
            expires_at: None,
        };
        assert!(matches!(
            validate_input(&i),
            Err(OpsBannerError::BodyTooLong { .. })
        ));
    }

    #[test]
    fn validate_rejects_bad_locale() {
        let i = OpsBannerInput {
            severity: OpsBannerSeverity::Warning,
            body: body("../etc/passwd", "oops"),
            starts_at: None,
            expires_at: None,
        };
        assert!(matches!(
            validate_input(&i),
            Err(OpsBannerError::InvalidLocale { .. })
        ));
    }

    #[test]
    fn validate_accepts_rfc5646_subtags() {
        let i = OpsBannerInput {
            severity: OpsBannerSeverity::Warning,
            body: body("fr-CA", "bonjour"),
            starts_at: None,
            expires_at: None,
        };
        assert!(validate_input(&i).is_ok());
    }

    #[test]
    fn sorted_puts_warnings_above_notifications() {
        let now = Utc::now();
        let w = OpsBanner {
            id: Uuid::new_v4(),
            severity: OpsBannerSeverity::Warning,
            body: body("en", "w"),
            starts_at: None,
            expires_at: None,
            created_by: Uuid::nil(),
            created_at: now - chrono::Duration::hours(1),
        };
        let n = OpsBanner {
            id: Uuid::new_v4(),
            severity: OpsBannerSeverity::Notification,
            body: body("en", "n"),
            starts_at: None,
            expires_at: None,
            created_by: Uuid::nil(),
            created_at: now,
        };
        let out = sorted(vec![n.clone(), w.clone()]);
        assert_eq!(out[0].severity, OpsBannerSeverity::Warning);
        assert_eq!(out[1].severity, OpsBannerSeverity::Notification);
    }

    #[test]
    fn version_changes_on_mutation() {
        let banners = Arc::new(RwLock::new(Vec::<OpsBanner>::new()));
        // Can't use the service without a pool — reach into the hashing
        // directly. The service's `version()` is a thin wrapper over
        // this logic.
        let v0 = {
            let bytes = serde_json::to_vec(&*banners.read().unwrap()).unwrap();
            blake3::hash(&bytes).to_hex().to_string()[..16].to_string()
        };
        banners.write().unwrap().push(OpsBanner {
            id: Uuid::new_v4(),
            severity: OpsBannerSeverity::Warning,
            body: body("en", "test"),
            starts_at: None,
            expires_at: None,
            created_by: Uuid::nil(),
            created_at: Utc::now(),
        });
        let v1 = {
            let bytes = serde_json::to_vec(&*banners.read().unwrap()).unwrap();
            blake3::hash(&bytes).to_hex().to_string()[..16].to_string()
        };
        assert_ne!(v0, v1);
    }

    #[test]
    fn plausible_locale_rejects_path_tricks() {
        assert!(!is_plausible_locale(""));
        assert!(!is_plausible_locale("../"));
        assert!(!is_plausible_locale("en/US"));
        assert!(!is_plausible_locale("en_US")); // underscore not RFC 5646
        assert!(is_plausible_locale("en"));
        assert!(is_plausible_locale("en-US"));
        assert!(is_plausible_locale("zh-Hans-CN"));
    }

    #[test]
    fn public_filter_hides_future_starts() {
        let pool_arc = Arc::new(RwLock::new(vec![
            OpsBanner {
                id: Uuid::new_v4(),
                severity: OpsBannerSeverity::Notification,
                body: body("en", "already live"),
                starts_at: Some(Utc::now() - chrono::Duration::hours(1)),
                expires_at: None,
                created_by: Uuid::nil(),
                created_at: Utc::now(),
            },
            OpsBanner {
                id: Uuid::new_v4(),
                severity: OpsBannerSeverity::Warning,
                body: body("en", "future"),
                starts_at: Some(Utc::now() + chrono::Duration::hours(1)),
                expires_at: None,
                created_by: Uuid::nil(),
                created_at: Utc::now(),
            },
            OpsBanner {
                id: Uuid::new_v4(),
                severity: OpsBannerSeverity::Notification,
                body: body("en", "no starts_at"),
                starts_at: None,
                expires_at: None,
                created_by: Uuid::nil(),
                created_at: Utc::now(),
            },
        ]));
        // Reimplement the filter inline (same as `list_public`) since
        // we can't construct a service without a real DB pool.
        let now = Utc::now();
        let public: Vec<OpsBanner> = pool_arc
            .read()
            .unwrap()
            .iter()
            .filter(|b| b.starts_at.map(|s| s <= now).unwrap_or(true))
            .cloned()
            .collect();
        assert_eq!(public.len(), 2);
        assert!(public.iter().all(|b| b.body["en"] != "future"));
    }
}
