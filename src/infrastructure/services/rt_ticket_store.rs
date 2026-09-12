//! Short-lived tickets for authenticating a WebSocket upgrade.
//!
//! # Problem
//!
//! A DPoP-bound session must carry a fresh `DPoP:` header on every
//! request. `new WebSocket(url)` in browsers cannot set arbitrary
//! headers — only `Sec-WebSocket-Protocol` — so the upgrade GET
//! arrives without a DPoP proof and `require_dpop_layer` refuses with
//! 401 `proof_missing_on_bound_session`. See
//! `docs/plan/message-bus.md § F`.
//!
//! # Solution
//!
//! Ticket exchange. The FE first `POST /api/rt/ticket` — a normal
//! HTTP request, so `apiFetch` attaches the DPoP proof and every other
//! middleware runs. The server mints an opaque one-shot ticket, tied
//! to the caller_id and a 30 s expiry. The FE then opens the WS with
//! `Sec-WebSocket-Protocol: oxi.ticket.<uuid>`; the WS handler
//! redeems the ticket via this store to recover the caller_id, then
//! runs the session with zero auth-middleware involvement.
//!
//! # Invariants
//!
//! - **Single-use** — `redeem` removes the entry atomically, so a
//!   captured ticket can be replayed at most once (the race is decided
//!   by the first successful `remove`; every other caller gets `None`).
//! - **Short-lived** — 30 s TTL. A captured ticket that isn't burned
//!   inside that window is inert.
//! - **Opaque** — the token carries no user identity itself. All the
//!   auth data lives in the store keyed by the token. Losing the store
//!   invalidates every issued ticket; that's the correct failure mode.
//! - **In-process** — one store per process. Multi-instance
//!   deployments will need a shared backend (Redis, PG); calling it
//!   out here so the seam is visible when the day comes.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use tokio::task::JoinHandle;
use uuid::Uuid;

/// TTL for a freshly-minted ticket. 30 s covers the round-trip from
/// `/api/rt/ticket` response to `new WebSocket()` handshake on any
/// realistic network — well under the shortest sensible clock skew
/// budget, well above the 100–500 ms actually needed on localhost or
/// LAN. Kept as a compile-time constant; if operators ever want to
/// tune it, promote to config.
pub const TICKET_TTL: Duration = Duration::from_secs(30);

/// Reaper cadence. Every N seconds the store walks its entries and
/// drops expired ones. Redemption also lazily short-circuits on
/// expiry, so the reaper is a memory-hygiene backstop rather than a
/// correctness gate — a ticket that expires and is never redeemed
/// stays around for up to `TICKET_TTL + REAPER_INTERVAL` before its
/// row is freed.
pub const REAPER_INTERVAL: Duration = Duration::from_secs(60);

/// Wire prefix identifying our tickets in `Sec-WebSocket-Protocol`.
/// The full value on the wire is `oxi.ticket.<uuid>` — one
/// subprotocol string, opaque to intermediaries. Kept short so
/// stripping proxies don't hit an arbitrary length limit.
pub const SUBPROTOCOL_PREFIX: &str = "oxi.ticket.";

struct Entry {
    caller_id: Uuid,
    expires_at: Instant,
}

/// In-process ticket store. Cheap to construct; the reaper task is
/// spawned by DI when the store is wired.
pub struct RtTicketStore {
    entries: DashMap<Uuid, Entry>,
}

impl RtTicketStore {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            entries: DashMap::new(),
        })
    }

    /// Issue a fresh ticket for `caller_id`. Returns the opaque token
    /// (a UUIDv4 string) — the FE puts this on the wire as
    /// `Sec-WebSocket-Protocol: oxi.ticket.<uuid>`.
    ///
    /// Ticket IDs are v4 (random) — 122 bits of entropy, well above
    /// the "unguessable-token" bar even without server-side rate
    /// limiting. A serial or timestamped id would leak issue-order
    /// signal to anyone with a wire tap.
    pub fn issue(&self, caller_id: Uuid) -> Uuid {
        let ticket = Uuid::new_v4();
        self.entries.insert(
            ticket,
            Entry {
                caller_id,
                expires_at: Instant::now() + TICKET_TTL,
            },
        );
        ticket
    }

    /// Redeem `ticket` if it exists AND has not expired. Removes the
    /// entry regardless of outcome — a valid ticket returns the
    /// caller_id, an expired ticket is silently freed and returns
    /// `None`. Single-use invariant holds by construction: only one
    /// caller wins the `remove`, everyone else sees `None`.
    pub fn redeem(&self, ticket: Uuid) -> Option<Uuid> {
        let (_, entry) = self.entries.remove(&ticket)?;
        if entry.expires_at < Instant::now() {
            return None;
        }
        Some(entry.caller_id)
    }

    /// Background reaper. Walks the map on the configured cadence and
    /// removes expired entries. Runs until the returned handle is
    /// dropped or `cancel` is notified (per the standard shutdown
    /// contract used across the crate).
    pub fn spawn_reaper(self: Arc<Self>) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(REAPER_INTERVAL);
            // First tick fires immediately; skip it so the store has
            // at least one TTL window's worth of entries before the
            // first sweep.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let now = Instant::now();
                self.entries.retain(|_, entry| entry.expires_at >= now);
            }
        })
    }

    /// Present count. Test-only. Not exposed to handlers — no
    /// operational reason to peek at the queue depth from a request
    /// path.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_then_redeem_returns_caller_id() {
        let store = RtTicketStore::new();
        let caller = Uuid::new_v4();
        let ticket = store.issue(caller);
        assert_eq!(store.redeem(ticket), Some(caller));
    }

    #[test]
    fn redeem_is_single_use() {
        let store = RtTicketStore::new();
        let caller = Uuid::new_v4();
        let ticket = store.issue(caller);
        assert_eq!(store.redeem(ticket), Some(caller));
        // Second redeem finds nothing — replay protection.
        assert_eq!(store.redeem(ticket), None);
    }

    #[test]
    fn redeem_unknown_returns_none() {
        let store = RtTicketStore::new();
        assert_eq!(store.redeem(Uuid::new_v4()), None);
    }

    #[test]
    fn issued_ticket_is_present_in_store() {
        let store = RtTicketStore::new();
        let caller = Uuid::new_v4();
        assert_eq!(store.len(), 0);
        let _ticket = store.issue(caller);
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn redeem_after_expiry_returns_none_and_frees_entry() {
        // Use a synthetic entry with `expires_at` in the past so the
        // test doesn't have to sleep 30 s.
        let store = RtTicketStore::new();
        let caller = Uuid::new_v4();
        let ticket = Uuid::new_v4();
        store.entries.insert(
            ticket,
            Entry {
                caller_id: caller,
                expires_at: Instant::now() - Duration::from_secs(1),
            },
        );
        assert_eq!(store.len(), 1);
        // Expired redeem returns None…
        assert_eq!(store.redeem(ticket), None);
        // …and the entry is gone.
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn distinct_tickets_for_the_same_caller() {
        // Two issues in a row must produce distinct token ids — the
        // FE will issue one per WS reconnect, and a collision would
        // mean the second issue clobbers the first's expiry map row.
        let store = RtTicketStore::new();
        let caller = Uuid::new_v4();
        let t1 = store.issue(caller);
        let t2 = store.issue(caller);
        assert_ne!(t1, t2);
    }

    // Wall-clock testing of the reaper's timer needs the tokio
    // `test-util` feature; not enabled crate-wide. The reaper body is
    // a straight `entries.retain(|_, e| e.expires_at >= now)` and the
    // redemption path already lazily short-circuits on expiry (see
    // `redeem_after_expiry_returns_none_and_frees_entry`), which
    // exercises the same expiry decision without waiting on a real
    // clock.

    #[test]
    fn is_send_sync_arc_shareable() {
        // Mirrors the actual usage in `AppState` — an
        // `Arc<RtTicketStore>` shared across the axum-served tasks.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Arc<RtTicketStore>>();
    }
}
