//! WebDAV lock store backed by Moka (in-memory cache with per-entry TTL).
//!
//! Each lock expires automatically at its own RFC 4918 `Timeout`, enforced
//! by Moka's [`Expiry`](moka::Expiry) policy. There are **no background
//! tasks and no per-lock timers** — Office clients refresh locks
//! constantly, and spawning a `sleep` future per acquire/refresh used to
//! leave thousands of orphaned timers pinned in the runtime. Two caches are
//! maintained:
//!
//! - `by_path`  : path → `LockEntry`  (source of truth; precise per-lock TTL)
//! - `by_token` : token → path        (reverse index for UNLOCK / refresh)
//!
//! `by_path` carries the exact per-lock TTL via `Expiry`; `by_token` keeps a
//! 24 h backstop TTL. A reverse-index entry that outlives its lock is
//! harmless: every lookup resolves through `by_path`, which is
//! authoritative, so an expired lock reads as absent even before its token
//! mapping is evicted.

use std::sync::Arc;
use std::time::{Duration, Instant};

use subtle::ConstantTimeEq;

use crate::application::adapters::webdav_adapter::{LockInfo, LockScope};

/// Constant-time equality for lock tokens. Same rationale as
/// `webdav_handler::ct_str_eq` — see that helper's doc-comment.
///
/// The two callsites in this file (`refresh` at :169, `release`
/// at :191) are already gated by `self.by_token.get(token)?`, so
/// the attacker CANNOT reach these checks without already having
/// presented a valid token — the practical timing-attack surface is
/// nil. Kept constant-time for defense-in-depth consistency across
/// every token comparison in the WebDAV surface, so a future
/// auditor doesn't have to re-derive "this one is safe because…"
/// for each individual callsite.
#[inline]
fn ct_str_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.as_bytes().ct_eq(b.as_bytes()).into()
}

/// Default lock timeout when the client does not specify one (RFC 4918 §10.7).
const DEFAULT_LOCK_TIMEOUT_SECS: u64 = 1800; // 30 minutes

/// Absolute maximum TTL a client may request.
const MAX_LOCK_TIMEOUT_SECS: u64 = 86_400; // 24 hours

/// A stored lock entry.
#[derive(Clone, Debug)]
pub struct LockEntry {
    pub info: LockInfo,
    pub path: String,
    /// The user who acquired the lock. `None` for entries seeded by
    /// unit tests or refresh paths that don't carry a caller (the
    /// refresh flow rebuilds from the existing entry without a new
    /// caller context, so we preserve whatever was there). RFC 4918
    /// §9.11's "MUST be requested by the owner" rule for UNLOCK is
    /// enforced by comparing this against the caller in
    /// `handle_unlock`.
    pub caller_user_id: Option<uuid::Uuid>,
}

/// Per-entry expiration policy for the `by_path` cache.
///
/// Moka calls this on insert (create) and re-insert (update, i.e. refresh)
/// to derive each lock's TTL from its own `Timeout` header — replacing the
/// old "global TTL + one spawned timer per lock" scheme. Reads do not
/// extend the lock (the default `expire_after_read` leaves the remaining
/// duration untouched).
struct LockExpiry;

impl moka::Expiry<String, LockEntry> for LockExpiry {
    fn expire_after_create(
        &self,
        _path: &String,
        entry: &LockEntry,
        _created_at: Instant,
    ) -> Option<Duration> {
        Some(WebDavLockStore::parse_timeout(
            entry.info.timeout.as_deref(),
        ))
    }

    fn expire_after_update(
        &self,
        _path: &String,
        entry: &LockEntry,
        _updated_at: Instant,
        _remaining: Option<Duration>,
    ) -> Option<Duration> {
        Some(WebDavLockStore::parse_timeout(
            entry.info.timeout.as_deref(),
        ))
    }
}

/// In-memory WebDAV lock store with automatic TTL-based expiration.
///
/// Uses Moka's `sync::Cache` — lock-free (sharded) reads, bounded size,
/// and per-entry TTL via `policy::Expiry`.
pub struct WebDavLockStore {
    /// path → `LockEntry`
    by_path: moka::sync::Cache<String, LockEntry>,
    /// token → path (reverse index)
    by_token: moka::sync::Cache<String, String>,
}

impl WebDavLockStore {
    /// Create a new lock store.
    ///
    /// * `max_capacity` — upper bound on simultaneous locks (evicts LRU on overflow).
    pub fn new(max_capacity: u64) -> Self {
        // `by_path` is the source of truth: each lock expires at its own
        // `Timeout` via the `LockExpiry` policy (no spawned timers).
        let by_path = moka::sync::Cache::builder()
            .max_capacity(max_capacity)
            .expire_after(LockExpiry)
            .build();

        // `by_token` is a reverse index; a 24 h backstop TTL bounds any
        // mapping that outlives its lock. Lookups resolve through `by_path`,
        // so a lingering entry here never resurrects an expired lock.
        let by_token = moka::sync::Cache::builder()
            .max_capacity(max_capacity)
            .time_to_live(Duration::from_secs(MAX_LOCK_TIMEOUT_SECS))
            .build();

        Self { by_path, by_token }
    }

    // ── Public API ──────────────────────────────────────────────

    /// Attempt to acquire a lock on `path`.
    ///
    /// Returns `Ok(LockEntry)` on success, or `Err(existing)` when:
    /// - The existing lock is exclusive (blocks any new lock), or
    /// - The new lock is exclusive and any lock already exists (RFC 4918 §7.8).
    #[allow(clippy::result_large_err)]
    pub fn acquire(
        &self,
        path: &str,
        info: LockInfo,
        caller_user_id: Option<uuid::Uuid>,
    ) -> Result<LockEntry, LockEntry> {
        if let Some(existing) = self.by_path.get(path) {
            // Exclusive existing lock → blocks everything.
            // New exclusive lock → blocked by any existing lock (shared or exclusive).
            if existing.info.scope == LockScope::Exclusive || info.scope == LockScope::Exclusive {
                return Err(existing);
            }
            // Both shared: keep the first holder as the enforcement sentinel in
            // `by_path` so releasing a secondary holder cannot clear the lock.
            // Register the new token only in the reverse index so UNLOCK works.
            let entry = LockEntry {
                info,
                path: path.to_owned(),
                caller_user_id,
            };
            self.by_token
                .insert(entry.info.token.clone(), path.to_owned());
            return Ok(entry);
        }

        let entry = LockEntry {
            info,
            path: path.to_owned(),
            caller_user_id,
        };

        // `LockExpiry` derives the TTL from `entry.info.timeout` on insert —
        // no spawned timer needed.
        self.by_path.insert(path.to_owned(), entry.clone());
        self.by_token
            .insert(entry.info.token.clone(), path.to_owned());

        Ok(entry)
    }

    /// Refresh an existing lock (extend its timeout).
    ///
    /// Returns `Some(LockEntry)` with updated timeout, or `None` if the token
    /// is unknown (expired or never existed).
    pub fn refresh(&self, token: &str, new_timeout: Option<&str>) -> Option<LockEntry> {
        let path = self.by_token.get(token)?;
        let mut entry = self.by_path.get(&path)?;

        if !ct_str_eq(&entry.info.token, token) {
            return None; // token mismatch — lock was replaced
        }

        let ttl = Self::parse_timeout(new_timeout.or(entry.info.timeout.as_deref()));
        // Normalize the stored timeout so `LockExpiry` recomputes the new TTL
        // from it on re-insert (Moka fires `expire_after_update`).
        entry.info.timeout = Some(format!("Second-{}", ttl.as_secs()));

        self.by_path.insert(path.clone(), entry.clone());
        self.by_token.insert(token.to_owned(), path.clone());

        Some(entry)
    }

    /// Release a lock by its token.
    ///
    /// Returns `true` if the lock existed and was removed.
    pub fn release(&self, token: &str) -> bool {
        if let Some(path) = self.by_token.get(token) {
            // Only remove from by_path if the token still matches
            if let Some(entry) = self.by_path.get(&path)
                && ct_str_eq(&entry.info.token, token)
            {
                self.by_path.invalidate(&path);
            }
            self.by_token.invalidate(token);
            true
        } else {
            false
        }
    }

    /// Look up a lock by resource path.
    pub fn get_by_path(&self, path: &str) -> Option<LockEntry> {
        self.by_path.get(path)
    }

    /// Look up a lock by token.
    pub fn get_by_token(&self, token: &str) -> Option<LockEntry> {
        let path = self.by_token.get(token)?;
        self.by_path.get(&path)
    }

    // ── Helpers ─────────────────────────────────────────────────

    /// Parse a WebDAV `Timeout` header value into a [`Duration`].
    ///
    /// Accepted formats (RFC 4918 §10.7):
    /// - `Second-NNN`
    /// - `Infinite`  (clamped to `MAX_LOCK_TIMEOUT_SECS`)
    /// - Comma-separated list (first value wins)
    fn parse_timeout(header: Option<&str>) -> Duration {
        let raw = match header {
            Some(v) if !v.is_empty() => v,
            _ => return Duration::from_secs(DEFAULT_LOCK_TIMEOUT_SECS),
        };

        // Take the first value in a comma-separated list
        let first = raw.split(',').next().unwrap_or(raw).trim();

        if first.eq_ignore_ascii_case("Infinite") {
            return Duration::from_secs(MAX_LOCK_TIMEOUT_SECS);
        }

        if let Some(secs_str) = first.strip_prefix("Second-")
            && let Ok(secs) = secs_str.trim().parse::<u64>()
        {
            return Duration::from_secs(secs.min(MAX_LOCK_TIMEOUT_SECS));
        }

        Duration::from_secs(DEFAULT_LOCK_TIMEOUT_SECS)
    }
}

/// Create a shared lock store wrapped in `Arc` for embedding in `AppState`.
pub fn create_webdav_lock_store() -> Arc<WebDavLockStore> {
    // 10 000 simultaneous locks should be more than enough; Moka evicts LRU
    // if the cap is reached, so stale entries are cleaned automatically.
    Arc::new(WebDavLockStore::new(10_000))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::adapters::webdav_adapter::LockType;
    use moka::Expiry;

    fn lock_info(token: &str, timeout: Option<&str>, scope: LockScope) -> LockInfo {
        LockInfo {
            token: token.to_owned(),
            owner: Some("tester".to_owned()),
            depth: "0".to_owned(),
            timeout: timeout.map(str::to_owned),
            scope,
            type_: LockType::Write,
        }
    }

    fn entry(token: &str, timeout: Option<&str>) -> LockEntry {
        LockEntry {
            info: lock_info(token, timeout, LockScope::Exclusive),
            path: "/file.txt".to_owned(),
            caller_user_id: None,
        }
    }

    #[test]
    fn expiry_uses_per_entry_timeout() {
        let now = Instant::now();
        let key = "/file.txt".to_owned();

        // Explicit Second-NNN → that exact duration.
        let e = entry("t", Some("Second-300"));
        assert_eq!(
            LockExpiry.expire_after_create(&key, &e, now),
            Some(Duration::from_secs(300))
        );
        // Refresh path (update) recomputes from the (normalized) timeout.
        assert_eq!(
            LockExpiry.expire_after_update(&key, &e, now, None),
            Some(Duration::from_secs(300))
        );
    }

    #[test]
    fn expiry_clamps_infinite_and_defaults_none() {
        let now = Instant::now();
        let key = "/file.txt".to_owned();

        let infinite = entry("t", Some("Infinite"));
        assert_eq!(
            LockExpiry.expire_after_create(&key, &infinite, now),
            Some(Duration::from_secs(MAX_LOCK_TIMEOUT_SECS))
        );

        let none = entry("t", None);
        assert_eq!(
            LockExpiry.expire_after_create(&key, &none, now),
            Some(Duration::from_secs(DEFAULT_LOCK_TIMEOUT_SECS))
        );

        // Over-large requests are clamped to the maximum.
        let huge = entry("t", Some("Second-999999999"));
        assert_eq!(
            LockExpiry.expire_after_create(&key, &huge, now),
            Some(Duration::from_secs(MAX_LOCK_TIMEOUT_SECS))
        );
    }

    #[test]
    fn acquire_get_release_roundtrip() {
        let store = WebDavLockStore::new(16);
        let info = lock_info("urn:token-1", Some("Second-600"), LockScope::Exclusive);

        let acquired = store.acquire("/a.txt", info, None).expect("acquire");
        assert_eq!(acquired.info.token, "urn:token-1");

        // Resolvable by both indexes.
        assert_eq!(
            store.get_by_path("/a.txt").map(|e| e.info.token.clone()),
            Some("urn:token-1".to_owned())
        );
        assert_eq!(
            store.get_by_token("urn:token-1").map(|e| e.path.clone()),
            Some("/a.txt".to_owned())
        );

        assert!(store.release("urn:token-1"));
        assert!(store.get_by_path("/a.txt").is_none());
        assert!(store.get_by_token("urn:token-1").is_none());
        // Releasing an unknown token reports nothing removed.
        assert!(!store.release("urn:token-1"));
    }

    #[test]
    fn exclusive_lock_conflicts() {
        let store = WebDavLockStore::new(16);
        store
            .acquire(
                "/a.txt",
                lock_info("urn:token-1", Some("Second-600"), LockScope::Exclusive),
                None,
            )
            .expect("first acquire");

        let conflict = store.acquire(
            "/a.txt",
            lock_info("urn:token-2", Some("Second-600"), LockScope::Exclusive),
            None,
        );
        assert!(conflict.is_err());
        // The original holder is returned so the caller can report it.
        assert_eq!(conflict.unwrap_err().info.token, "urn:token-1");
    }

    #[test]
    fn refresh_normalizes_timeout_and_keeps_lock() {
        let store = WebDavLockStore::new(16);
        store
            .acquire(
                "/a.txt",
                lock_info("urn:token-1", Some("Infinite"), LockScope::Exclusive),
                None,
            )
            .expect("acquire");

        let refreshed = store
            .refresh("urn:token-1", Some("Second-120"))
            .expect("refresh");
        assert_eq!(refreshed.info.timeout.as_deref(), Some("Second-120"));
        // Still present and still addressable by token.
        assert!(store.get_by_token("urn:token-1").is_some());

        // Refreshing an unknown token yields None.
        assert!(store.refresh("urn:unknown", Some("Second-120")).is_none());
    }
}
