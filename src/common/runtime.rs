//! Tokio runtime pool sizing — CFS-quota-aware worker / blocking thread counts.
//!
//! `#[tokio::main]` and the existing rayon / image pools all size themselves from
//! [`std::thread::available_parallelism`], which reflects CPU **affinity**
//! (`sched_getaffinity`: cpuset cgroup, `taskset`) but **not** the CFS bandwidth
//! quota (`docker --cpus`, cgroup v2 `cpu.max`, v1 `cpu.cfs_quota_us`). On a
//! quota-limited container on a many-core host it therefore over-reports the
//! usable core count — tokio then spawns one worker per *host* core that
//! time-slice across the *quota's* cores. These helpers fold the CFS quota back
//! in so the runtime (and any caller that wants it) sizes to the real budget.
//!
//! All parsing is split into pure functions so the cgroup formats are unit-tested
//! without touching `/sys`.

use crate::common::config::RuntimeConfig;

/// Parse cgroup **v2** `cpu.max` contents — `"<quota_us> <period_us>"`, or
/// `"max <period_us>"` when unlimited. Returns the quota in whole cores (rounded
/// up), or `None` when unlimited / unparseable.
fn parse_cpu_max_v2(s: &str) -> Option<usize> {
    let mut it = s.split_whitespace();
    let quota = it.next()?;
    let period = it.next()?;
    if quota == "max" {
        return None;
    }
    let quota: f64 = quota.parse().ok()?;
    let period: f64 = period.parse().ok()?;
    if quota > 0.0 && period > 0.0 {
        Some((quota / period).ceil() as usize)
    } else {
        None
    }
}

/// Parse cgroup **v1** `cpu.cfs_quota_us` / `cpu.cfs_period_us`. A quota of `-1`
/// (or any non-positive value) means unlimited → `None`. Otherwise whole cores,
/// rounded up.
fn parse_cpu_quota_v1(quota: &str, period: &str) -> Option<usize> {
    let quota: i64 = quota.trim().parse().ok()?;
    let period: i64 = period.trim().parse().ok()?;
    if quota > 0 && period > 0 {
        Some(((quota as f64) / (period as f64)).ceil() as usize)
    } else {
        None
    }
}

/// The cgroup CPU quota in whole cores (v2 first, then v1), or `None` when there
/// is no quota (unlimited) or it can't be read.
pub fn cgroup_cpu_quota() -> Option<usize> {
    // cgroup v2 unified hierarchy.
    if let Ok(s) = std::fs::read_to_string("/sys/fs/cgroup/cpu.max")
        && let Some(n) = parse_cpu_max_v2(&s)
    {
        return Some(n);
    }
    // cgroup v1.
    let quota = std::fs::read_to_string("/sys/fs/cgroup/cpu/cpu.cfs_quota_us").ok()?;
    let period = std::fs::read_to_string("/sys/fs/cgroup/cpu/cpu.cfs_period_us").ok()?;
    parse_cpu_quota_v1(&quota, &period)
}

/// Effective CPU parallelism: affinity-parallelism capped by the CFS quota.
///
/// `available_parallelism()` alone over-reports under a CFS quota (see module
/// docs); we take the min of it and the quota, floored at 1.
pub fn effective_parallelism() -> usize {
    let affinity = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    match cgroup_cpu_quota() {
        Some(q) => affinity.min(q.max(1)),
        None => affinity,
    }
}

/// `(worker_threads, max_blocking_threads)` for the Tokio runtime, resolving
/// whatever [`RuntimeConfig`] leaves unset from the CPU budget.
///
/// - worker count: `cfg.worker_threads`, default [`effective_parallelism`].
/// - blocking-pool cap: `cfg.max_blocking_threads`, default
///   `max(32, 8 × workers)` — vs tokio's flat **512**, which for this heavy
///   `spawn_blocking` user (thumbnails, transcode, zip, PDF/text extraction,
///   Argon2 ≈19 MB/hash) is a multi-GB RSS blast radius with no ceiling.
///
/// Both are clamped to ≥1. An unset config on an uncontended host yields the
/// same worker count as the previous `#[tokio::main]` default.
pub fn pool_sizes(cfg: &RuntimeConfig) -> (usize, usize) {
    let workers = cfg
        .worker_threads
        .unwrap_or_else(effective_parallelism)
        .max(1);
    let max_blocking = cfg
        .max_blocking_threads
        .unwrap_or_else(|| (workers * 8).max(32));
    (workers, max_blocking)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The blocking pool has a floor of 32 threads — 8 × workers only takes
    /// over once it exceeds that.
    #[test]
    fn blocking_pool_defaults_to_eight_per_worker_with_a_floor() {
        let cfg = RuntimeConfig {
            worker_threads: Some(2),
            max_blocking_threads: None,
        };
        assert_eq!(pool_sizes(&cfg), (2, 32));
        let cfg = RuntimeConfig {
            worker_threads: Some(8),
            max_blocking_threads: None,
        };
        assert_eq!(pool_sizes(&cfg), (8, 64));
    }

    /// Explicit values are taken as given, each independently of the other.
    #[test]
    fn explicit_pool_sizes_win() {
        let cfg = RuntimeConfig {
            worker_threads: Some(3),
            max_blocking_threads: Some(7),
        };
        assert_eq!(pool_sizes(&cfg), (3, 7));
        let cfg = RuntimeConfig {
            worker_threads: None,
            max_blocking_threads: Some(5),
        };
        assert_eq!(pool_sizes(&cfg).1, 5);
    }

    #[test]
    fn v2_exact_two_cores() {
        assert_eq!(parse_cpu_max_v2("200000 100000"), Some(2));
    }

    #[test]
    fn v2_rounds_up_fractional() {
        // 1.5 cores of quota → 2 whole worker threads.
        assert_eq!(parse_cpu_max_v2("150000 100000"), Some(2));
    }

    #[test]
    fn v2_unlimited_is_none() {
        assert_eq!(parse_cpu_max_v2("max 100000"), None);
    }

    #[test]
    fn v2_garbage_is_none() {
        assert_eq!(parse_cpu_max_v2("not a quota"), None);
        assert_eq!(parse_cpu_max_v2(""), None);
    }

    #[test]
    fn v1_two_cores() {
        assert_eq!(parse_cpu_quota_v1("200000", "100000"), Some(2));
    }

    #[test]
    fn v1_unlimited_sentinel_is_none() {
        assert_eq!(parse_cpu_quota_v1("-1", "100000"), None);
    }

    #[test]
    fn v1_rounds_up() {
        assert_eq!(parse_cpu_quota_v1("250000", "100000"), Some(3));
    }
}
