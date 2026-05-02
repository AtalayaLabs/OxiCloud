//! Account lockout service — blocks login for an account after N consecutive
//! failed attempts.
//!
//! Uses a `moka` TTL cache so that:
//! * Failed-attempt counters automatically expire after the lockout window.
//! * No database writes are needed — this is **in-memory** and therefore
//!   per-instance.  If OxiCloud is deployed behind a load balancer with
//!   multiple replicas, a sticky-session or shared Redis store would be
//!   needed for cross-instance coordination (out of scope for v1).
//!
//! Typical flow:
//! 1. **Before password verification** → call [`LoginLockoutService::check`].
//!    If the account is locked, return `403` immediately without touching
//!    Argon2 (saves CPU).
//! 2. **After failed verification** → call [`LoginLockoutService::record_failure`].
//! 3. **After successful login** → call [`LoginLockoutService::record_success`]
//!    to reset the counter.

use moka::sync::Cache;
use std::time::Duration;

/// Tracks consecutive failures for a single username.
#[derive(Clone, Debug)]
struct FailureRecord {
    /// Number of consecutive failed attempts.
    count: u32,
}

/// In-memory account lockout tracker.
#[derive(Clone)]
pub struct LoginLockoutService {
    /// Maps `username -> FailureRecord`.  TTL = lockout window.
    cache: Cache<String, FailureRecord>,
    /// Maximum consecutive failures before the account is temporarily locked.
    max_failures: u32,
    /// How long the lockout lasts (seconds).
    lockout_secs: u64,
}

impl LoginLockoutService {
    /// Create a new lockout service.
    ///
    /// * `max_failures`  — e.g. `5` (lock after 5 bad passwords)
    /// * `lockout_secs`  — e.g. `900` (15-minute lockout)
    /// * `max_accounts`  — upper bound on tracked accounts (evicts LRU)
    pub fn new(max_failures: u32, lockout_secs: u64, max_accounts: u64) -> Self {
        let cache = Cache::builder()
            .time_to_live(Duration::from_secs(lockout_secs))
            .max_capacity(max_accounts)
            .build();
        Self {
            cache,
            max_failures,
            lockout_secs,
        }
    }

    /// Build the cache key from the (lowercased) username and the client IP.
    ///
    /// The IP is part of the key so that an attacker flooding bad passwords
    /// from one address cannot lock a legitimate user out of the same account
    /// from a different address (issue #323). When the caller cannot resolve
    /// a real IP — e.g. `OXICLOUD_TRUST_PROXY_HEADERS=false` and the peer
    /// address isn't available — `client_ip` should be a non-empty constant
    /// like `"unknown"`; in that pathological case we fall back to
    /// account-scoped lockout, which is no worse than the previous
    /// behaviour.
    fn key(username: &str, client_ip: &str) -> String {
        // `|` is not valid in either a username or an IP literal so it makes
        // the username/ip boundary unambiguous.
        format!("{}|{}", username.to_lowercase(), client_ip)
    }

    /// Check whether the (account, IP) pair is currently locked.
    ///
    /// Returns `Ok(())` if the user may attempt login, or
    /// `Err(remaining_secs)` with the *approximate* remaining lockout time.
    pub fn check(&self, username: &str, client_ip: &str) -> Result<(), u64> {
        if let Some(rec) = self.cache.get(&Self::key(username, client_ip))
            && rec.count >= self.max_failures
        {
            // The entry exists and is over the threshold.  Because moka
            // evicts at TTL we know the lockout window has not yet elapsed.
            return Err(self.lockout_secs);
        }
        Ok(())
    }

    /// Record a failed login attempt.  Returns the new failure count.
    pub fn record_failure(&self, username: &str, client_ip: &str) -> u32 {
        let key = Self::key(username, client_ip);
        let new_count = self.cache.get(&key).map(|r| r.count + 1).unwrap_or(1);
        self.cache
            .insert(key.clone(), FailureRecord { count: new_count });

        if new_count >= self.max_failures {
            tracing::warn!(
                username = %username,
                client_ip = %client_ip,
                attempts = new_count,
                lockout_secs = self.lockout_secs,
                "Account temporarily locked after {} consecutive failed login attempts from this IP",
                new_count,
            );
        }
        new_count
    }

    /// Record a successful login — resets the failure counter for this
    /// (account, IP) pair so the user isn't penalised for stray earlier
    /// failures from the same address.
    pub fn record_success(&self, username: &str, client_ip: &str) {
        self.cache.invalidate(&Self::key(username, client_ip));
    }

    /// Maximum failures before lockout (used to inform callers / error messages).
    pub fn max_failures(&self) -> u32 {
        self.max_failures
    }

    /// Lockout duration in seconds.
    pub fn lockout_secs(&self) -> u64 {
        self.lockout_secs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IP1: &str = "1.1.1.1";
    const IP2: &str = "2.2.2.2";

    #[test]
    fn allows_login_under_threshold() {
        let svc = LoginLockoutService::new(3, 60, 100);
        assert!(svc.check("alice", IP1).is_ok());
        svc.record_failure("alice", IP1);
        svc.record_failure("alice", IP1);
        // 2 failures — still under threshold
        assert!(svc.check("alice", IP1).is_ok());
    }

    #[test]
    fn locks_after_threshold() {
        let svc = LoginLockoutService::new(3, 60, 100);
        svc.record_failure("bob", IP1);
        svc.record_failure("bob", IP1);
        svc.record_failure("bob", IP1);
        assert!(svc.check("bob", IP1).is_err());
    }

    #[test]
    fn resets_on_success() {
        let svc = LoginLockoutService::new(3, 60, 100);
        svc.record_failure("carol", IP1);
        svc.record_failure("carol", IP1);
        svc.record_success("carol", IP1);
        // Counter reset — should be allowed again
        assert!(svc.check("carol", IP1).is_ok());
        svc.record_failure("carol", IP1); // starts over at 1
        assert!(svc.check("carol", IP1).is_ok());
    }

    #[test]
    fn case_insensitive() {
        let svc = LoginLockoutService::new(2, 60, 100);
        svc.record_failure("Dave", IP1);
        svc.record_failure("dave", IP1);
        assert!(svc.check("DAVE", IP1).is_err());
    }

    /// Regression test for #323: flooding bad passwords from one IP must
    /// NOT lock the account out for legitimate users coming from a
    /// different IP.
    #[test]
    fn does_not_lock_out_other_ips_for_same_account() {
        let svc = LoginLockoutService::new(3, 60, 100);

        // Attacker hammers the account from IP1 until it locks for that IP.
        for _ in 0..3 {
            svc.record_failure("admin", IP1);
        }
        assert!(
            svc.check("admin", IP1).is_err(),
            "attacker IP must be locked"
        );

        // A legitimate user coming from IP2 must still be allowed to try.
        assert!(
            svc.check("admin", IP2).is_ok(),
            "second IP must not inherit the lockout — that's the #323 DOS"
        );
    }

    /// A successful login on one IP must clear *that* IP's counter only —
    /// it should NOT silently absolve a separate, ongoing brute-force from
    /// a different IP against the same account.
    #[test]
    fn success_resets_only_the_acting_ip() {
        let svc = LoginLockoutService::new(3, 60, 100);

        for _ in 0..3 {
            svc.record_failure("admin", IP1);
        }
        // Genuine login from IP2 succeeds; should reset IP2 counter (which
        // is already 0 here) but leave IP1's lockout intact.
        svc.record_success("admin", IP2);

        assert!(
            svc.check("admin", IP1).is_err(),
            "IP1 must remain locked after IP2's success"
        );
    }
}
