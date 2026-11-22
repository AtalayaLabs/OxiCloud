//! Proof that an account passed every per-login account check.
//!
//! Lives in the domain so that [`Session::new`] can REQUIRE it. That is
//! the whole point: a session row cannot be constructed anywhere in the
//! codebase without one, so a future login door that forgets the
//! account checks does not compile rather than shipping a bypass.
//!
//! [`Session::new`]: crate::domain::entities::session::Session::new

use crate::domain::entities::user::User;
use uuid::Uuid;

/// An account that may hold a session.
///
/// Constructible only by
/// [`check_account_admission`](crate::application::services::auth_application_service::check_account_admission),
/// because the fields are private and this module exposes no
/// constructor — so obtaining one is proof the checks ran. The
/// application layer owns the rules (they depend on deployment config);
/// the domain owns the *fact* that they passed.
///
/// It is NOT proof of a verified credential. The caller must already
/// have authenticated the principal. This answers the separate
/// question: *may this account have a session at all?*
///
/// Why it carries the whole `User` rather than an id: the session mint
/// needs the entity anyway (lifecycle hooks, `register_login`, token
/// claims), and splitting them would let a caller pair one user's proof
/// with another user's entity. Holding both together makes that
/// unrepresentable.
#[derive(Debug)]
pub struct AdmittedCaller {
    user: User,
    origin: &'static str,
}

impl AdmittedCaller {
    /// Record an admission. Crate-visible on purpose: callers outside
    /// the admission function have no business asserting this, and the
    /// doc on the type explains why.
    pub(crate) fn new(user: User, origin: &'static str) -> Self {
        Self { user, origin }
    }

    pub fn user(&self) -> &User {
        &self.user
    }

    pub fn user_id(&self) -> Uuid {
        self.user.id()
    }

    /// Which door admitted this caller — `"password"`, `"opaque"`,
    /// `"device"`, … Audit-only.
    pub fn origin(&self) -> &'static str {
        self.origin
    }

    /// Take the entity back out, for the mint that needs to mutate it
    /// (`register_login`) before persisting.
    pub fn into_user(self) -> User {
        self.user
    }

    /// Fabricate an admission WITHOUT running any check.
    ///
    /// Gated to test builds only, and deliberately NOT behind
    /// `test_utils`: that feature is part of `--features dev`, which
    /// would make the bypass reachable in a binary an operator could
    /// run. `integration_tests` is opt-in and not in `dev`, so it is
    /// safe to include.
    #[cfg(any(test, feature = "integration_tests"))]
    pub fn admitted_for_test(user: User) -> Self {
        Self::new(user, "test")
    }
}
