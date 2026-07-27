//! In-memory login-exchange cache for OPAQUE aPAKE (Phase 1).
//!
//! OPAQUE login is a two-round exchange. KE1 arrives from the client
//! and produces a [`ServerLogin`] state that must survive until the
//! matching KE3 arrives (usually within a few hundred milliseconds).
//! This module holds that state between the two round-trips, keyed by
//! a random `exchange_id: Uuid` handed back to the client on KE1.
//!
//! ## Why in-memory
//!
//! Single-instance deployments (the current OxiCloud shape) can use a
//! process-local cache without correctness issues — KE1 and KE3 always
//! hit the same server. Multi-instance deployments (Phase 5+ if we go
//! there) would need to swap the backing to Redis or session-affinity
//! at the load balancer; the shape of this module (`store`/`take`)
//! stays identical, so the swap would be local.
//!
//! ## Why NOT put the state in a cookie
//!
//! The naive alternative — "cookie the ServerLogin state client-side" —
//! would leak the server's ephemeral private key material (the KE1
//! response bakes it in). Even if AEAD-wrapped with a server secret,
//! that AEAD key becomes another crown jewel to rotate. Server-side
//! storage keyed by a random opaque handle is simpler and correct.
//!
//! ## Single-use semantics
//!
//! An `exchange_id` MUST be consumed at most once. Two concurrent KE3s
//! with the same id would be either an accidental double-submit or a
//! replay attempt; either way, the second must fail. We use moka's
//! atomic [`Cache::remove`] (get-and-invalidate in one call — verified
//! in moka 0.12+ source) so there's no race window between "state
//! exists" and "state consumed".
//!
//! ## TTL
//!
//! 60s is the ceiling for a normal OPAQUE login round-trip (KE1
//! response → user typed nothing new → client computes KE3 → sends).
//! Beyond that, the state is stale — the client would have to start
//! over anyway. Bounded capacity (default 10k concurrent exchanges,
//! LRU-evicted) caps memory even under a burst / abuse pattern.

use std::sync::Arc;
use std::time::Duration;

use moka::sync::Cache;
use opaque_ke::ServerLogin;
use uuid::Uuid;

use crate::infrastructure::services::opaque_service::OxiCloudSuite;

/// Default lifetime of a login exchange between KE1 and KE3.
///
/// Chosen empirically to cover slow client CPUs (Argon2id on mobile
/// can take ~1s with production KSF params) plus a network buffer.
/// Extending beyond 60s is a security taste question; shortening
/// under ~15s starts failing legitimate slow clients.
pub const DEFAULT_TTL_SECS: u64 = 60;

/// Default maximum concurrent in-flight exchanges. Bounds memory in
/// case of an attack or bug that spams KE1 without ever sending KE3.
/// Each entry is roughly the size of a `ServerLogin<OxiCloudSuite>`
/// (~200 bytes with the Ristretto255 keypair + AKE state), so 10k
/// entries is a couple of MB total — well below "worry" territory.
pub const DEFAULT_MAX_INFLIGHT: u64 = 10_000;

/// Handle passed to the client on KE1 that the client must echo back
/// on KE3. Opaque random UUID — nothing about the server state is
/// derivable from it, so leaking it only enables a race that STILL
/// requires a valid KE3 payload to succeed (which requires the
/// correct passphrase).
pub type ExchangeId = Uuid;

/// Server-side state stashed between KE1 and KE3.
///
/// Holds the [`ServerLogin`] (opaque-ke handshake continuation) plus
/// the `user_id` KE1 resolved from the client's identifier — `Some`
/// for a real user, `None` for the anti-enum dummy branch (unknown
/// user or user without an envelope). KE3 uses this to know which
/// account to mint the session for on success; the dummy branch's
/// KE3 will fail the AKE check inside `ServerLogin::finish` and
/// never reach the session mint, so the `None` variant is a
/// belt-and-braces guard we never rely on for correctness.
///
/// `state` lives in a `Mutex<Option<_>>` because moka's `Cache::remove`
/// returns a *clone* of the stored `Arc` (not the original). That means
/// unwrapping the Arc to consume `ServerLogin` (which isn't `Clone`)
/// would race the briefly-lingering cache-side ref. `Mutex::lock().take()`
/// gives us ownership of the inner value regardless of how many Arc
/// clones exist.
struct StashedExchange {
    state: std::sync::Mutex<Option<ServerLogin<OxiCloudSuite>>>,
    user_id: Option<Uuid>,
}

/// Exchange state handed back to the KE3 handler — a plain owned
/// tuple so consumers work with the concrete types, not Arc<Mutex<_>>.
pub struct TakenExchange {
    pub state: ServerLogin<OxiCloudSuite>,
    pub user_id: Option<Uuid>,
}

/// In-memory cache holding server-side login state between KE1 and KE3.
#[derive(Clone)]
pub struct OpaqueLoginExchange {
    inner: Arc<Cache<ExchangeId, Arc<StashedExchange>>>,
}

impl OpaqueLoginExchange {
    /// Build a cache with production defaults ([`DEFAULT_TTL_SECS`],
    /// [`DEFAULT_MAX_INFLIGHT`]). Callers wanting tighter TTL for
    /// tests should use [`Self::with_params`].
    pub fn new() -> Self {
        Self::with_params(Duration::from_secs(DEFAULT_TTL_SECS), DEFAULT_MAX_INFLIGHT)
    }

    /// Build with explicit TTL + capacity. Used by tests to shrink the
    /// TTL so expiry paths can be exercised in milliseconds.
    pub fn with_params(ttl: Duration, max_capacity: u64) -> Self {
        let inner = Cache::builder()
            .time_to_live(ttl)
            .max_capacity(max_capacity)
            .build();
        Self {
            inner: Arc::new(inner),
        }
    }

    /// Stash a fresh exchange and return the handle to hand back to
    /// the client. The exchange_id is generated here so callers
    /// can't accidentally reuse one — every KE1 gets its own.
    pub fn store(&self, state: ServerLogin<OxiCloudSuite>, user_id: Option<Uuid>) -> ExchangeId {
        let id = Uuid::new_v4();
        self.inner.insert(
            id,
            Arc::new(StashedExchange {
                state: std::sync::Mutex::new(Some(state)),
                user_id,
            }),
        );
        id
    }

    /// Atomically consume the exchange for `exchange_id`. Returns
    /// `None` if the id is unknown, already consumed, or expired.
    /// Callers must treat those three cases identically (anti-enum):
    /// a KE3 with a bad id, a replay, and a timeout should all
    /// surface as the same `InvalidCredentials` shape to the client.
    ///
    /// Combines two atomicity guarantees:
    ///
    ///   * `moka::Cache::remove` is a single get-and-invalidate on
    ///     the cache side (verified in moka 0.12+).
    ///   * `Mutex::lock().take()` gives us ownership of the inner
    ///     `ServerLogin` even when moka returns a *clone* of the
    ///     stored `Arc` (which it does — `remove` yields a clone,
    ///     not the original), and prevents two concurrent takers
    ///     from both seeing `Some`.
    pub fn take(&self, exchange_id: ExchangeId) -> Option<TakenExchange> {
        let arc = self.inner.remove(&exchange_id)?;
        let state = arc.state.lock().ok()?.take()?;
        Some(TakenExchange {
            state,
            user_id: arc.user_id,
        })
    }

    /// Force runtime maintenance (LRU eviction + TTL sweep). Moka runs
    /// these opportunistically on `insert`/`get` too; test code calls
    /// this after fast-forwarding time so expiry assertions are
    /// deterministic without waiting on background threads.
    #[cfg(test)]
    fn run_pending_tasks(&self) {
        self.inner.run_pending_tasks();
    }
}

impl Default for OpaqueLoginExchange {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::config::OpaqueConfig;
    use crate::infrastructure::services::opaque_service::{
        OpaqueMode, OpaqueService, OxiCloudSuite,
    };
    use opaque_ke::{
        ClientLogin, ClientRegistration, ClientRegistrationFinishParameters,
        ServerLoginStartParameters, ServerRegistration,
    };
    use rand_core::OsRng;

    /// Build a real `ServerLogin<OxiCloudSuite>` to stash — the type
    /// is generic and can't be `Default`ed, so we drive a mini
    /// registration + KE1 to produce one. Slower than a fake, but
    /// this exercises the full crate type-plumb.
    fn build_server_login_state() -> ServerLogin<OxiCloudSuite> {
        let svc = OpaqueService::from_config(OpaqueConfig {
            mode: OpaqueMode::Migrate,
            server_setup_b64: Some(OpaqueService::generate_server_setup_b64()),
            ..OpaqueConfig::default()
        })
        .expect("build service");

        let ksf = argon2::Argon2::new(
            argon2::Algorithm::Argon2id,
            argon2::Version::V0x13,
            argon2::Params::new(8, 1, 1, None).unwrap(),
        );
        let mut rng = OsRng;
        let user = b"alice@example.com";
        let pass = b"pw";

        // Registration to prime the password file.
        let client_reg = ClientRegistration::<OxiCloudSuite>::start(&mut rng, pass).unwrap();
        let server_reg =
            ServerRegistration::<OxiCloudSuite>::start(svc.setup(), client_reg.message, user)
                .unwrap();
        let client_reg_finish = client_reg
            .state
            .finish(
                &mut rng,
                pass,
                server_reg.message,
                ClientRegistrationFinishParameters::new(
                    opaque_ke::Identifiers::default(),
                    Some(&ksf),
                ),
            )
            .unwrap();
        let password_file = ServerRegistration::<OxiCloudSuite>::finish(client_reg_finish.message);

        // KE1 to produce the ServerLogin state we want to stash.
        let client_login = ClientLogin::<OxiCloudSuite>::start(&mut rng, pass).unwrap();
        opaque_ke::ServerLogin::start(
            &mut rng,
            svc.setup(),
            Some(password_file),
            client_login.message,
            user,
            ServerLoginStartParameters::default(),
        )
        .unwrap()
        .state
    }

    #[test]
    fn store_and_take_round_trip_returns_the_same_state_once() {
        let cache = OpaqueLoginExchange::with_params(Duration::from_secs(60), 100);
        let state = build_server_login_state();

        let id = cache.store(state, Some(Uuid::new_v4()));
        // Second call after take must miss — single-use semantic.
        let taken = cache.take(id).expect("first take retrieves the state");
        assert!(taken.user_id.is_some(), "user_id round-trips through the stash");
        assert!(
            cache.take(id).is_none(),
            "second take must miss — exchange_id is single-use"
        );
    }

    #[test]
    fn unknown_id_returns_none() {
        let cache = OpaqueLoginExchange::new();
        assert!(cache.take(Uuid::new_v4()).is_none());
    }

    #[test]
    fn expired_state_is_evicted_and_take_returns_none() {
        // 100ms TTL so the test finishes fast without wall-clock sleep
        // beyond that. Moka's TTL is not perfectly wall-clock precise
        // (it runs pending tasks lazily), so `run_pending_tasks`
        // forces a deterministic sweep.
        let cache = OpaqueLoginExchange::with_params(Duration::from_millis(100), 100);
        let id = cache.store(build_server_login_state(), None);

        std::thread::sleep(Duration::from_millis(150));
        cache.run_pending_tasks();

        assert!(
            cache.take(id).is_none(),
            "state must be evicted after TTL — replay attempts past 60s must fail"
        );
    }

    #[test]
    fn store_yields_distinct_exchange_ids_per_call() {
        // Two KE1s from the same user MUST get different exchange_ids
        // — reusing one would enable a KE3 to consume the wrong
        // exchange's state.
        let cache = OpaqueLoginExchange::new();
        let a = cache.store(build_server_login_state(), None);
        let b = cache.store(build_server_login_state(), None);
        assert_ne!(a, b, "each store() must mint a fresh UUID");
    }

    #[test]
    fn dummy_branch_stash_carries_no_user_id() {
        // KE1 for an unknown user stashes `user_id: None` (anti-enum
        // — dummy branch). KE3 for the dummy will fail at AKE, but
        // the stash contract is: real user → Some(id), unknown → None.
        let cache = OpaqueLoginExchange::new();
        let id = cache.store(build_server_login_state(), None);
        let taken = cache.take(id).expect("take dummy stash");
        assert!(taken.user_id.is_none(), "dummy-branch stash carries no user_id");
    }
}
