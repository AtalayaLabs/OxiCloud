//! Shared classification of `sqlx::Error` transience.
//!
//! One home for the question "could this database error clear on its
//! own?", because six repository modules each map `sqlx::Error` to their
//! own error type (`map_sqlx_error`, `map_sqlx_err`, `db_err`) and a
//! predicate duplicated six ways drifts six ways.
//!
//! Why it matters beyond tidiness: every mapper currently funnels pool
//! timeouts, dropped connections and `08006` into its catch-all
//! "database error" arm, which becomes `ErrorKind::InternalError` and a
//! 500. A caller then cannot tell "your request is malformed" from "ask
//! me again in a second" — and an auth gate reading a user row cannot
//! tell a definitive answer from no answer at all, which is how
//! `require_internal_user` ended up choosing between admitting on any
//! error and denying every user during a blip. See
//! `docs/plan/scope-gates.md` § The two fail-open arms.
//!
//! The classification lives here, at the port, while the backend's own
//! error type is still visible — the same reason
//! [`DomainError::transient_backend`] documents for the blob backends.
//! Downstream, the SQLSTATE survives only inside a formatted string.
//!
//! [`DomainError::transient_backend`]: crate::domain::errors::DomainError::transient_backend

/// Whether a retry of the same statement could plausibly succeed.
///
/// Deliberately conservative, mirroring
/// [`DomainError::is_transient`](crate::domain::errors::DomainError::is_transient):
/// defaulting to "permanent" makes a missed retry a visible failure,
/// whereas calling a permanent fault transient burns retry budget and,
/// in the job engine, holds `migration_readonly` while it does.
///
/// Note what is **not** here. `PoolClosed` is shutdown, not a blip;
/// `RowNotFound` is a definitive answer and the one error an auth gate
/// may act on; a constraint violation is the caller's fault however
/// often it is retried.
pub fn is_transient(err: &sqlx::Error) -> bool {
    match err {
        // Connection-level: the statement never reached a healthy
        // server, or the link died mid-flight.
        sqlx::Error::Io(_) | sqlx::Error::Tls(_) => true,
        // The pool could not hand out a connection in time. Usually
        // load, occasionally a stuck transaction — both clear.
        sqlx::Error::PoolTimedOut => true,
        // The background worker died; the next call reconnects.
        sqlx::Error::WorkerCrashed => true,
        sqlx::Error::Database(db) => db.code().is_some_and(|c| is_transient_sqlstate(&c)),
        _ => false,
    }
}

/// SQLSTATE codes worth retrying, per the PostgreSQL error-code table.
///
/// Grouped by why they clear, because the list is otherwise impossible
/// to review:
///
/// - **Class 08 — connection exception.** Every member means "this
///   connection is gone", including `08006` (`connection_failure`) and
///   `08P01` (`protocol_violation`, which sqlx also raises on a
///   half-closed socket).
/// - **57P01/02/03 — the server is going away or not yet ready.** A
///   rolling restart, a crash recovery, or a standby still replaying.
/// - **57014 — `query_canceled`.** The statement-timeout path. The query
///   was killed, not answered, so the data is unknown rather than absent
///   — exactly the distinction a gate must not collapse.
/// - **40001 / 40P01 — serialization failure and deadlock.** Postgres
///   explicitly asks the client to retry these.
/// - **55P03 — `lock_not_available`.** Someone else held the row; they
///   will let go.
/// - **53300 — `too_many_connections`.** Capacity, not correctness.
///   `53100` (disk full) is left out on purpose: it needs an operator,
///   and retrying it just hammers a full disk.
fn is_transient_sqlstate(code: &str) -> bool {
    if let Some(rest) = code.strip_prefix("08") {
        // Class 08 in full — any five-character code in the class.
        return rest.len() == 3;
    }
    matches!(
        code,
        "57P01" | "57P02" | "57P03" | "57014" | "40001" | "40P01" | "55P03" | "53300"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_class_is_transient_in_full() {
        for code in [
            "08000", "08003", "08006", "08001", "08004", "08007", "08P01",
        ] {
            assert!(
                is_transient_sqlstate(code),
                "{code} (class 08) must be transient"
            );
        }
    }

    #[test]
    fn server_going_away_is_transient() {
        for code in ["57P01", "57P02", "57P03"] {
            assert!(is_transient_sqlstate(code), "{code} must be transient");
        }
    }

    /// `57014` is the statement-timeout cancel. It reads as "no answer",
    /// never "no row" — the whole point of the classifier for an auth
    /// gate, which may act on a definitive `RowNotFound` and must not
    /// act on this.
    #[test]
    fn query_canceled_is_transient() {
        assert!(is_transient_sqlstate("57014"));
    }

    #[test]
    fn contention_is_transient() {
        for code in ["40001", "40P01", "55P03", "53300"] {
            assert!(is_transient_sqlstate(code), "{code} must be transient");
        }
    }

    /// The conservative direction, pinned so a later "make retries more
    /// aggressive" change has to argue with a test. A unique violation
    /// is the caller's fault, and a full disk needs an operator — both
    /// would otherwise burn the retry budget to reach the same failure.
    #[test]
    fn caller_faults_and_operator_faults_are_permanent() {
        for code in ["23505", "23503", "23502", "42601", "42P01", "53100"] {
            assert!(!is_transient_sqlstate(code), "{code} must NOT be transient");
        }
    }

    /// `08` as a prefix is not enough — the class test must require a
    /// full five-character code, or a future `0812345` typo classifies
    /// as a connection failure.
    #[test]
    fn class_08_requires_a_five_character_code() {
        assert!(!is_transient_sqlstate("08"));
        assert!(!is_transient_sqlstate("0800"));
        assert!(!is_transient_sqlstate("0800012"));
    }

    #[test]
    fn row_not_found_is_definitive_not_transient() {
        assert!(!is_transient(&sqlx::Error::RowNotFound));
    }

    /// Shutdown is not a blip: retrying a closed pool cannot succeed,
    /// and treating it as transient would make a graceful shutdown look
    /// like an outage to retry through.
    #[test]
    fn pool_closed_is_not_transient() {
        assert!(!is_transient(&sqlx::Error::PoolClosed));
    }

    #[test]
    fn pool_timeout_and_worker_crash_are_transient() {
        assert!(is_transient(&sqlx::Error::PoolTimedOut));
        assert!(is_transient(&sqlx::Error::WorkerCrashed));
    }

    #[test]
    fn io_errors_are_transient() {
        let io = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset by peer");
        assert!(is_transient(&sqlx::Error::Io(io)));
    }
}
