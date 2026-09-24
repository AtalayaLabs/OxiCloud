//! Collaborative document sessions — Phase A C1.
//!
//! One long-lived Tokio task per open file that hosts a `yrs::Doc`. The
//! service is the registry that spawns / looks up / shuts down these
//! actors. WebSocket binary-frame routing and bus fan-out land in C2
//! (`interfaces/api/handlers/rt_ws.rs`) — this file's scope is:
//!
//! - Actor lifecycle (spawn on first attach, idle-GC after last socket)
//! - CRDT state management (load-or-seed, apply updates, compact snapshots)
//! - Debounced flush-to-blob (currently gated on a [`DocContentWriter`] port;
//!   the actual `FileManagementService` bridge lands in a follow-up)
//!
//! # Actor model
//!
//! `yrs::Doc` is not `Send + Sync` — the correct pattern is one owning
//! task per doc, with a Tokio channel inbox. Every operation on the doc
//! (apply an update, extract text for flush, serialise a snapshot)
//! flows through the actor's `run` loop; concurrent WS handlers post
//! messages and await replies via oneshot channels. This eliminates any
//! locking on the `Doc` itself.
//!
//! ```text
//! WS handler ──ApplyUpdate──▶ inbox ──▶  ┌──────────────────────┐
//! WS handler ──AttachSocket─▶ inbox ──▶  │ CollabSession::run   │
//! WS handler ──DetachSocket─▶ inbox ──▶  │ (owns yrs::Doc,      │
//!                                        │ HashSet<SocketId>,   │
//! GC scan   ──MaybeIdleShut─▶ inbox ──▶  │ update counter, …)  │
//!                                        └──────────────────────┘
//! ```
//!
//! # C1 scope boundaries
//!
//! - **In scope:** actor lifecycle, `yrs::Doc` load-or-seed, apply-update,
//!   snapshot compaction after N updates, unit tests over the CRDT
//!   convergence.
//! - **Out of scope (C2):** bus fan-out of `MessageBusEvent::CrdtUpdate`,
//!   binary-frame routing in the WS handler, `Topic::Collab` variant on the
//!   port.
//! - **Out of scope (C7):** real flush-to-blob wiring (a `DocContentWriter`
//!   port is declared; the concrete `FileManagementService` bridge waits
//!   for the write-content API surface — see
//!   [`docs/plan/markdown-collab.md`] § Flush-to-blob).
//! - **Out of scope (later):** WebDAV out-of-band write eviction, oversized-
//!   file refusal at attach (`max_doc_bytes`), malformed-frame handling.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use tokio::sync::{broadcast, mpsc, oneshot};
use uuid::Uuid;
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{Doc, GetString, ReadTxn, StateVector, Text, Transact, Update};

use crate::application::ports::collab_ports::{
    CollabAuthzGate, DocContentReader, DocContentWriter, DocSessionRepository, StoredDocSession,
};
use crate::common::errors::{DomainError, ErrorKind};
use crate::domain::services::authorization::Permission;

/// Opaque socket identifier — matches the WS handler's per-connection
/// id. C1 doesn't use it beyond ref-counting (`HashSet<SocketId>`);
/// C2's fan-out uses it to skip the sender's own socket when
/// broadcasting an update.
pub type SocketId = u64;

/// One fan-out item on a session's outbox — a wire-kind byte plus the
/// payload bytes ready for `encode_binary_frame`. Three kinds ride
/// the channel:
///
///   * `kind::UPDATE` (0x01) — Yjs update blob; server-authoritative
///     CRDT apply already happened, this fires the broadcast for
///     other sockets to catch up.
///   * `kind::AWARENESS` (0x02) — presence bytes (cursor position,
///     user handle, colour). NOT persisted; not applied to the CRDT.
///     Fan-out only.
///   * [`INTERNAL_KIND_EVICTED`] (0xFE) — server-only control signal
///     for "this session is going away". Payload is a UTF-8 reason
///     string (`resource_deleted`, `external_write`, …). The WS
///     forwarder catches this kind BEFORE `encode_binary_frame` —
///     it never reaches the wire as binary; instead the forwarder
///     synthesizes an `rt.revoked` text frame and unwinds.
///
/// Sharing one channel keeps ordering natural — a peer's UPDATE and
/// the AWARENESS bump that reflects the caret motion arrive on the
/// receiver in the same order the actor accepted them.
pub type CollabBroadcast = (u8, Vec<u8>);

/// Server-only kind marker for "session evicted" control messages
/// on the outbox. Deliberately outside the [`kind::*`] wire vocabulary
/// so a stray publish can never reach a client as a valid binary
/// frame — the WS forwarder catches this value and turns it into a
/// JSON `rt.revoked` notification instead.
pub const INTERNAL_KIND_EVICTED: u8 = 0xFE;

/// Configuration knobs. Defaults track the plan doc's § Limits &
/// guardrails table. Wired from `AppConfig::collab` in the DI layer
/// (not yet — C1 lands with defaults only).
#[derive(Debug, Clone, Copy)]
pub struct CollabLimits {
    /// Compact the CRDT snapshot after this many updates layered on
    /// top of the persisted `state`. Higher = cheaper writes, more
    /// expensive reconnect catch-up. Plan default: 200.
    pub snapshot_after_updates: i32,
    /// Capacity of the per-actor broadcast channel that fans updates
    /// out to attached sockets. A slow subscriber that falls this many
    /// updates behind is dropped by the broadcast layer (with a
    /// `RecvError::Lagged` on the receiver side, which the WS
    /// forwarder handles by terminating the socket — clean recovery
    /// via reconnect + sync-step-1 catches the client up). Sized for
    /// bursts a few hundred keystrokes deep; defaults to 256.
    pub broadcast_capacity: usize,
    /// Debounced flush-to-blob: fire when no update has arrived for
    /// this long. Bounds "how quiet does the doc need to be before
    /// we materialise CRDT → blob" — matches Y-Sweet's default and
    /// the plan spec. Default 15 s.
    pub debounce_idle: Duration,
    /// Debounced flush-to-blob: fire regardless of activity when this
    /// long has elapsed since the FIRST dirty update. Bounds
    /// staleness for non-collab consumers (WebDAV, download, search
    /// index) on a doc that stays continuously edited. Default 60 s.
    pub debounce_max: Duration,
    /// How often the actor wakes to check whether a debounce
    /// threshold has been crossed. Not user-tunable in principle;
    /// exists so tests can shrink it to millisecond scales without
    /// waiting on the default. Default 1 s.
    pub debounce_tick: Duration,
    /// Ceiling on the actor's encoded Yjs state size, in bytes.
    /// Enforced before every `apply_update`: if applying the
    /// incoming update WOULD push the running total over this cap,
    /// the update is rejected with [`CollabError::DocTooLarge`].
    ///
    /// The running total is a conservative over-estimate — it sums
    /// each applied update's raw byte length (which the CRDT can
    /// then internally compress on snapshot). A snapshot resets the
    /// estimate to the compacted state's real encoded size, so
    /// long-running sessions self-correct after each compaction.
    ///
    /// Default 10 MB. Rationale: text collaboration on markdown /
    /// code is well under this; a caller trying to grow the doc
    /// past 10 MB is either a bug (loop uploading via UPDATE) or
    /// a resource-exhaustion attempt. Both cases should be refused
    /// at the wire, not applied. When operators need bigger docs
    /// (say a legitimate long-form workflow), override via
    /// `OXICLOUD_COLLAB_MAX_DOC_BYTES` in the env.
    pub max_doc_bytes: usize,
}

impl Default for CollabLimits {
    fn default() -> Self {
        Self {
            snapshot_after_updates: 200,
            broadcast_capacity: 256,
            debounce_idle: Duration::from_secs(15),
            debounce_max: Duration::from_secs(60),
            debounce_tick: Duration::from_secs(1),
            max_doc_bytes: 10 * 1024 * 1024,
        }
    }
}

/// Errors surfaced to WS handlers when a session interaction fails.
/// Callers translate these into wire-level responses (JSON-RPC error
/// object for control-plane failures; WS close frames for session-fatal
/// classes).
#[derive(Debug, thiserror::Error)]
pub enum CollabError {
    #[error("session actor is gone (shut down mid-request)")]
    SessionGone,
    #[error("CRDT decode failed: {0}")]
    BadUpdate(String),
    #[error("storage error: {0}")]
    Storage(#[from] DomainError),
    /// The caller passed the socket-level auth but lacks the required
    /// permission on the file for this specific frame class (write
    /// vs read). The WS handler closes the socket with a
    /// `collab.write_denied` / `collab.read_denied` audit line — same
    /// shape as protocol-violation, since a client that legitimately
    /// only has Read shouldn't be sending UPDATE frames at all.
    #[error("authz denied: {permission} on file {file_id}")]
    AuthzDenied {
        permission: &'static str,
        file_id: Uuid,
    },
    /// Applying the incoming update would push the actor's running
    /// encoded-doc-bytes total over [`CollabLimits::max_doc_bytes`].
    /// The WS binary router turns this into a graceful `rt.write_denied`
    /// with `reason: "doc_too_large"` and keeps the socket alive —
    /// symmetric with the AuthzDenied write-path treatment
    /// introduced in the graceful-write-denied slice.
    #[error("doc size cap ({limit_bytes} B) exceeded on file {file_id}")]
    DocTooLarge { file_id: Uuid, limit_bytes: usize },
}

// ════════════════════════════════════════════════════════════════════════════
// Actor inbox
// ════════════════════════════════════════════════════════════════════════════

/// Messages the actor accepts on its inbox channel. Every mutation to
/// the owned `yrs::Doc` flows through here; the actor's `run` loop is
/// the only writer.
enum SessionMsg {
    AttachSocket {
        socket_id: SocketId,
        reply: oneshot::Sender<()>,
    },
    DetachSocket {
        socket_id: SocketId,
        remaining_sockets: oneshot::Sender<usize>,
    },
    ApplyUpdate {
        caller_id: Uuid,
        bytes: Vec<u8>,
        reply: oneshot::Sender<Result<(), CollabError>>,
    },
    /// Broadcast an awareness (presence) blob to other subscribers.
    /// No CRDT apply, no debouncer arm — just a fan-out. Reply is
    /// `()`: awareness has no failure mode beyond "actor is gone",
    /// which surfaces via the reply-drop path like every other
    /// message here.
    ApplyAwareness {
        bytes: Vec<u8>,
        reply: oneshot::Sender<()>,
    },
    /// Force a flush of the CRDT text to the file's blob. Called
    /// automatically by the debouncer's tick branch when idle/max
    /// thresholds are crossed; also exposed on the public handle for
    /// tests, idle-GC, and any future admin/explicit-save endpoint.
    /// Reply is `Ok(true)` if a write happened, `Ok(false)` if the
    /// hash matched and the call short-circuited.
    FlushToBlob {
        reply: oneshot::Sender<Result<bool, CollabError>>,
    },
    /// Extract the current CRDT text as UTF-8 bytes. C1 tests use this
    /// to assert convergence; C2's flush loop uses it before writing
    /// to a blob.
    GetText {
        reply: oneshot::Sender<String>,
    },
    /// Force-serialise the current doc to a snapshot and persist. The
    /// actor calls this itself when `updates_since_snapshot` crosses
    /// the threshold; external callers use it for tests + graceful
    /// shutdown.
    Snapshot {
        reply: oneshot::Sender<Result<(), CollabError>>,
    },
    /// Yjs sync-step-1: the client sends its state vector; the server
    /// replies with the diff (sync-step-2) that brings the client up
    /// to date. Called on every attach and on reconnect. The reply
    /// payload is the raw bytes clients pass to `Y.applyUpdate` — no
    /// additional framing here (the WS handler wraps it in a `0x03`
    /// binary frame per the wire spec).
    SyncStep1 {
        client_state_vector: Vec<u8>,
        reply: oneshot::Sender<Result<Vec<u8>, CollabError>>,
    },
    /// Return a fresh `broadcast::Receiver` on the actor's update
    /// outbox. Every subsequent `ApplyUpdate` re-broadcasts its raw
    /// bytes to every live receiver; the WS handler forwards those
    /// bytes as `0x01` binary frames to the socket it forwards for.
    /// Yjs semantics are idempotent — a sender that receives its own
    /// update back applies it with no visible effect — so the fan-out
    /// deliberately does NOT filter by origin. Simpler wire, one
    /// fewer piece of per-socket state, and matches the y-websocket
    /// reference server's behaviour.
    SubscribeUpdates {
        reply: oneshot::Sender<broadcast::Receiver<CollabBroadcast>>,
    },
    /// Publish an [`INTERNAL_KIND_EVICTED`] control message on the
    /// outbox, then break the actor loop. Ordering matters: the
    /// eviction bytes hit the broadcast channel BEFORE `RecvError::Closed`
    /// fires on any forwarder, so every attached socket sees the
    /// `rt.revoked` signal before its channel drops. Emitted from
    /// [`CollabSessionService::evict_sessions_for_file`] on external
    /// invalidation events (file deleted, out-of-band write, admin
    /// forced-eject) — the actor's own idle-GC shutdown path uses
    /// plain [`SessionMsg::Shutdown`] without an eviction signal,
    /// because idle sockets don't need a UI banner.
    Evict {
        reason: &'static str,
    },
    Shutdown,
}

/// The per-file actor handle held in [`CollabSessionService`]'s
/// registry. Cheap to clone (just an mpsc sender + a couple of Arcs).
#[derive(Clone)]
pub struct CollabSession {
    inbox: mpsc::Sender<SessionMsg>,
}

impl CollabSession {
    pub async fn attach_socket(&self, socket_id: SocketId) -> Result<(), CollabError> {
        let (tx, rx) = oneshot::channel();
        self.inbox
            .send(SessionMsg::AttachSocket {
                socket_id,
                reply: tx,
            })
            .await
            .map_err(|_| CollabError::SessionGone)?;
        rx.await.map_err(|_| CollabError::SessionGone)
    }

    /// Detach a socket. Returns the number of sockets still attached
    /// after the removal — 0 means the caller can request idle-GC.
    pub async fn detach_socket(&self, socket_id: SocketId) -> Result<usize, CollabError> {
        let (tx, rx) = oneshot::channel();
        self.inbox
            .send(SessionMsg::DetachSocket {
                socket_id,
                remaining_sockets: tx,
            })
            .await
            .map_err(|_| CollabError::SessionGone)?;
        rx.await.map_err(|_| CollabError::SessionGone)
    }

    /// Broadcast an awareness (presence) blob. Fire-and-await the
    /// actor's confirmation that it enqueued the fan-out. Doesn't
    /// touch the CRDT.
    pub async fn apply_awareness(&self, bytes: Vec<u8>) -> Result<(), CollabError> {
        let (tx, rx) = oneshot::channel();
        self.inbox
            .send(SessionMsg::ApplyAwareness { bytes, reply: tx })
            .await
            .map_err(|_| CollabError::SessionGone)?;
        rx.await.map_err(|_| CollabError::SessionGone)?;
        Ok(())
    }

    pub async fn apply_update(&self, caller_id: Uuid, bytes: Vec<u8>) -> Result<(), CollabError> {
        let (tx, rx) = oneshot::channel();
        self.inbox
            .send(SessionMsg::ApplyUpdate {
                caller_id,
                bytes,
                reply: tx,
            })
            .await
            .map_err(|_| CollabError::SessionGone)?;
        rx.await.map_err(|_| CollabError::SessionGone)?
    }

    /// Flush the CRDT text to the file's blob. Idempotent — a call
    /// with unchanged content since the previous flush short-circuits
    /// via `last_flushed_content_hash`. Reply is `Ok(true)` on a
    /// real write, `Ok(false)` on the short-circuit path.
    ///
    /// The actor's debouncer calls this automatically; external
    /// callers use it for tests, idle-GC, and admin/explicit-save
    /// paths.
    pub async fn flush_to_blob(&self) -> Result<bool, CollabError> {
        let (tx, rx) = oneshot::channel();
        self.inbox
            .send(SessionMsg::FlushToBlob { reply: tx })
            .await
            .map_err(|_| CollabError::SessionGone)?;
        rx.await.map_err(|_| CollabError::SessionGone)?
    }

    pub async fn get_text(&self) -> Result<String, CollabError> {
        let (tx, rx) = oneshot::channel();
        self.inbox
            .send(SessionMsg::GetText { reply: tx })
            .await
            .map_err(|_| CollabError::SessionGone)?;
        rx.await.map_err(|_| CollabError::SessionGone)
    }

    pub async fn force_snapshot(&self) -> Result<(), CollabError> {
        let (tx, rx) = oneshot::channel();
        self.inbox
            .send(SessionMsg::Snapshot { reply: tx })
            .await
            .map_err(|_| CollabError::SessionGone)?;
        rx.await.map_err(|_| CollabError::SessionGone)?
    }

    /// Handle a client's sync-step-1 (state vector) by returning the
    /// sync-step-2 diff bytes the client needs to catch up. The WS
    /// handler wraps the reply in a `0x03` binary frame per the wire
    /// spec.
    ///
    /// If the client's state vector fails to decode, returns
    /// [`CollabError::BadUpdate`] — the caller closes the socket with
    /// a protocol-violation reason (WS 1002).
    pub async fn sync_step_1(&self, client_state_vector: Vec<u8>) -> Result<Vec<u8>, CollabError> {
        let (tx, rx) = oneshot::channel();
        self.inbox
            .send(SessionMsg::SyncStep1 {
                client_state_vector,
                reply: tx,
            })
            .await
            .map_err(|_| CollabError::SessionGone)?;
        rx.await.map_err(|_| CollabError::SessionGone)?
    }

    pub async fn shutdown(&self) {
        // Best-effort — the actor may already be gone.
        let _ = self.inbox.send(SessionMsg::Shutdown).await;
    }

    /// Publish an eviction control signal on the outbox and terminate
    /// the actor. Every attached forwarder receives the signal via
    /// its `subscribe_updates` channel before the channel closes, so
    /// downstream sockets can synthesize a distinct `rt.revoked` frame
    /// (see [`INTERNAL_KIND_EVICTED`]). Best-effort — a lost actor is
    /// already gone as far as the client is concerned; the next
    /// subscribe attempt gets a fresh session with fresh seeding, or
    /// a denial if the underlying file no longer exists.
    pub async fn evict(&self, reason: &'static str) {
        let _ = self.inbox.send(SessionMsg::Evict { reason }).await;
    }

    /// Subscribe to this session's update stream. Each `apply_update`
    /// re-broadcasts its raw bytes to every live receiver; the WS
    /// handler wraps each broadcast in a `0x01` binary frame and
    /// forwards it to its socket. Callers must poll the receiver
    /// continuously — a receiver that falls `broadcast_capacity`
    /// updates behind starts returning `RecvError::Lagged`; the
    /// forwarder recovers by tearing down the socket, and the client
    /// reconnects and catches up via sync-step-1.
    pub async fn subscribe_updates(
        &self,
    ) -> Result<broadcast::Receiver<CollabBroadcast>, CollabError> {
        let (tx, rx) = oneshot::channel();
        self.inbox
            .send(SessionMsg::SubscribeUpdates { reply: tx })
            .await
            .map_err(|_| CollabError::SessionGone)?;
        rx.await.map_err(|_| CollabError::SessionGone)
    }
}

// ════════════════════════════════════════════════════════════════════════════
// Actor state — the per-file task's owned mutable state
// ════════════════════════════════════════════════════════════════════════════

/// Well-known Yjs root type name for the doc's text content. Kept
/// stable across the codebase — clients must use the exact same key
/// or their `Y.Text` handle will point at a different (empty) type.
const ROOT_TEXT_NAME: &str = "content";

struct ActorState {
    file_id: Uuid,
    doc: Doc,
    /// Blob hash this actor believes the file row points at: observed
    /// at attach, then replaced with each successful flush's return.
    ///
    /// Passed as the flush's `expected_blob_hash`, which makes the
    /// write a compare-and-swap. `None` means the baseline could not
    /// be established, and the flush falls back to writing blind
    /// rather than refusing to save.
    last_known_blob_hash: Option<String>,
    attached: HashSet<SocketId>,
    updates_since_snapshot: i32,
    limits: CollabLimits,
    repo: Arc<dyn DocSessionRepository>,
    /// Fan-out channel for applied updates. Kept alive by the actor for
    /// the actor's lifetime; each successful `apply_update` publishes
    /// the raw update bytes here. `SubscribeUpdates` messages hand out
    /// fresh receivers on demand. Publishing to a channel with zero
    /// live receivers is not an error (`broadcast::Sender::send` just
    /// returns `Err(SendError)` which we silently drop) — the CRDT
    /// state stays authoritative on the server regardless of whether
    /// anyone is listening.
    outbox: broadcast::Sender<CollabBroadcast>,
    /// Flush-to-blob writer. Called by the debouncer with the last
    /// human writer's `caller_id` — that stamps §14 provenance on the
    /// underlying `update_file_content_with_blob` call.
    writer: Arc<dyn DocContentWriter>,
    /// The most recent human whose UPDATE frame reached
    /// `apply_update`. `None` until the first `0x01` is applied. Used
    /// as the flush's `caller_id` — the last actual editor gets the
    /// audit / recent-items credit. Grant revocation between write
    /// and flush is out of scope: the update already went into the
    /// CRDT under authorization at write time.
    last_writer_id: Option<Uuid>,
    /// `Instant` at which the doc went from clean to dirty. `Some`
    /// while unflushed edits exist; cleared to `None` on successful
    /// flush. Bounds the "how long since the FIRST dirty edit"
    /// deadline via `limits.debounce_max`.
    first_dirty_at: Option<Instant>,
    /// `Instant` of the most recent `apply_update`. `Some` while
    /// unflushed edits exist; cleared to `None` on successful flush.
    /// Bounds the "how quiet has the doc been" deadline via
    /// `limits.debounce_idle`.
    last_dirty_at: Option<Instant>,
    /// In-memory mirror of `collab.doc_sessions.last_flushed_content_hash`
    /// — the content hash of the last successful blob write. Used to
    /// short-circuit no-op flushes (CRDT text unchanged since the
    /// previous materialisation). Loaded from the row on attach;
    /// stamped after each successful flush.
    last_flushed_content_hash: Option<String>,
    /// Running estimate of the encoded doc size in bytes. Compared
    /// against [`CollabLimits::max_doc_bytes`] before each apply to
    /// refuse updates that would push the doc over the cap.
    ///
    /// Conservative over-estimate — sums each applied update's raw
    /// bytes (which the CRDT can internally compress on snapshot).
    /// Reset to the real compacted size after every `snapshot()` so
    /// long-running sessions self-correct.
    doc_bytes: usize,
}

impl ActorState {
    /// Load the persisted snapshot into the actor's `Doc`, OR seed a
    /// fresh doc from `seed_content` (the current file blob text) on
    /// first-ever attach. Also stamps an initial snapshot row via the
    /// repo so subsequent attaches follow the load-path.
    async fn load_or_seed(
        file_id: Uuid,
        repo: Arc<dyn DocSessionRepository>,
        writer: Arc<dyn DocContentWriter>,
        seed_content: Option<Vec<u8>>,
        baseline_blob_hash: Option<String>,
        limits: CollabLimits,
    ) -> Result<Self, CollabError> {
        let doc = Doc::new();
        let existing = repo.load(file_id).await?;
        let last_flushed_content_hash = existing
            .as_ref()
            .and_then(|r| r.last_flushed_content_hash.clone());
        let updates_since_snapshot = match existing {
            Some(StoredDocSession {
                state,
                updates_since_snapshot,
                ..
            }) => {
                // Restore from the persisted snapshot. Any updates
                // layered on top since the last compaction are already
                // folded into `state` — the snapshot IS the whole
                // history collapsed.
                let update = Update::decode_v1(&state)
                    .map_err(|e| CollabError::BadUpdate(format!("load state: {e}")))?;
                doc.transact_mut()
                    .apply_update(update)
                    .map_err(|e| CollabError::BadUpdate(format!("apply loaded state: {e}")))?;
                updates_since_snapshot
            }
            None => {
                // First-ever attach — seed the CRDT with the current
                // file text. Empty `Vec<u8>` for a brand-new file
                // reads as an empty document, which is correct.
                let content = seed_content.unwrap_or_default();
                let text = String::from_utf8(content).map_err(|e| {
                    CollabError::Storage(DomainError::new(
                        ErrorKind::InvalidInput,
                        "collab",
                        format!("seed content is not valid UTF-8: {e}"),
                    ))
                })?;
                {
                    let text_ref = doc.get_or_insert_text(ROOT_TEXT_NAME);
                    let mut txn = doc.transact_mut();
                    text_ref.insert(&mut txn, 0, &text);
                }
                // Persist the seeded snapshot so a second attach hits
                // the load-path and gets the same starting point.
                let snapshot = doc
                    .transact()
                    .encode_state_as_update_v1(&StateVector::default());
                let sv = doc.transact().state_vector().encode_v1();
                repo.save_snapshot(file_id, &snapshot, &sv).await?;
                0
            }
        };
        let (outbox, _) = broadcast::channel(limits.broadcast_capacity);
        // Seed the `doc_bytes` running estimate from the compacted
        // state — whether we just loaded a snapshot from the repo or
        // seeded fresh from the blob, the encoded state's length is
        // the current doc size.
        let doc_bytes = doc
            .transact()
            .encode_state_as_update_v1(&StateVector::default())
            .len();
        Ok(Self {
            file_id,
            doc,
            last_known_blob_hash: baseline_blob_hash,
            attached: HashSet::new(),
            updates_since_snapshot,
            limits,
            repo,
            outbox,
            writer,
            last_writer_id: None,
            first_dirty_at: None,
            last_dirty_at: None,
            last_flushed_content_hash,
            doc_bytes,
        })
    }

    fn text(&self) -> String {
        let text_ref = self.doc.get_or_insert_text(ROOT_TEXT_NAME);
        let txn = self.doc.transact();
        text_ref.get_string(&txn)
    }

    /// Apply an incoming Yjs update. Errors on malformed bytes; a
    /// successful apply bumps the update counter and MAY trigger a
    /// snapshot compaction. After a successful apply, the raw update
    /// bytes are broadcast on `outbox` so every subscribed WS forwarder
    /// can push them to its socket — this is the fan-out path.
    /// `send` errors when no receivers are live; that's expected on a
    /// solo session and silently dropped, since the authoritative
    /// state is already on the actor's `Doc`.
    ///
    /// Also arms the debouncer: `caller_id` is remembered as
    /// `last_writer_id` for the eventual flush, and the dirty
    /// timestamps advance so the tick branch of `run_actor` can decide
    /// whether the idle / max thresholds have been crossed.
    async fn apply_update(&mut self, caller_id: Uuid, bytes: Vec<u8>) -> Result<(), CollabError> {
        // Doc-size cap. `doc_bytes` is a running over-estimate that
        // gets reset to the compacted size after every snapshot, so
        // the check is conservative rather than paranoid. Reject
        // BEFORE apply so the offending update never enters the CRDT
        // — Yjs has no rollback in `yrs`, and applying-then-refusing
        // would leave the doc permanently over the cap.
        if self.doc_bytes.saturating_add(bytes.len()) > self.limits.max_doc_bytes {
            return Err(CollabError::DocTooLarge {
                file_id: self.file_id,
                limit_bytes: self.limits.max_doc_bytes,
            });
        }
        let update = Update::decode_v1(&bytes)
            .map_err(|e| CollabError::BadUpdate(format!("decode: {e}")))?;
        {
            let mut txn = self.doc.transact_mut();
            txn.apply_update(update)
                .map_err(|e| CollabError::BadUpdate(format!("apply: {e}")))?;
        }
        // Advance the running estimate now that the apply committed.
        // The next snapshot will recompute this to the compacted size,
        // so growth stays bounded by real doc size, not by the sum of
        // historical updates.
        self.doc_bytes = self.doc_bytes.saturating_add(bytes.len());
        self.repo.touch_after_update(self.file_id).await?;
        self.updates_since_snapshot += 1;
        if self.updates_since_snapshot >= self.limits.snapshot_after_updates {
            self.snapshot().await?;
        }
        let _ = self.outbox.send((
            crate::application::services::collab_wire::kind::UPDATE,
            bytes,
        ));
        // Debouncer bookkeeping. `first_dirty_at` sticks on the FIRST
        // update after a clean state; `last_dirty_at` advances on
        // every update. Together they drive the tick-branch decision.
        let now = Instant::now();
        if self.first_dirty_at.is_none() {
            self.first_dirty_at = Some(now);
        }
        self.last_dirty_at = Some(now);
        self.last_writer_id = Some(caller_id);
        Ok(())
    }

    /// Fan out an awareness (presence) blob to other subscribers.
    /// Unlike [`Self::apply_update`], the actor's `Doc` is untouched
    /// — awareness is transient state (cursor position, user handle,
    /// colour). The debouncer isn't armed either; a cursor bump
    /// shouldn't extend the flush deadline. Publishing to a channel
    /// with zero live receivers is not an error (`Err(SendError)`
    /// silently dropped) — solo sessions never see their own
    /// awareness back, which is fine.
    fn broadcast_awareness(&self, bytes: Vec<u8>) {
        let _ = self.outbox.send((
            crate::application::services::collab_wire::kind::AWARENESS,
            bytes,
        ));
    }

    /// Serialise the current doc as one update-blob + state-vector and
    /// persist. Resets `updates_since_snapshot` to 0. Called
    /// automatically when the update counter hits threshold; also
    /// exposed to callers for graceful shutdown and tests.
    async fn snapshot(&mut self) -> Result<(), CollabError> {
        let snapshot = self
            .doc
            .transact()
            .encode_state_as_update_v1(&StateVector::default());
        let sv = self.doc.transact().state_vector().encode_v1();
        // Reset the running size estimate to the compacted doc's
        // real encoded length. `doc_bytes` was inflating with each
        // apply's raw bytes; the CRDT internally deduplicates on
        // snapshot so the compacted state is (usually much) smaller.
        // Keeps the cap check anchored to reality on long sessions.
        self.doc_bytes = snapshot.len();
        self.repo
            .save_snapshot(self.file_id, &snapshot, &sv)
            .await?;
        self.updates_since_snapshot = 0;
        Ok(())
    }

    /// Yjs sync-step-1 handler: given the client's state vector, encode
    /// the diff that brings the client up to date. Read-only on the
    /// server doc — no persistence side-effects.
    fn sync_step_1(&self, client_sv_bytes: &[u8]) -> Result<Vec<u8>, CollabError> {
        let sv = StateVector::decode_v1(client_sv_bytes)
            .map_err(|e| CollabError::BadUpdate(format!("state-vector decode: {e}")))?;
        Ok(self.doc.transact().encode_state_as_update_v1(&sv))
    }

    /// Return `true` when the debouncer's tick branch should fire a
    /// flush. Two conditions, either is enough:
    ///
    ///   * `now - last_dirty_at ≥ debounce_idle` — the doc has been
    ///     quiet long enough (idle threshold).
    ///   * `now - first_dirty_at ≥ debounce_max` — the doc has been
    ///     continuously dirty long enough (max age; bounds staleness
    ///     for non-collab consumers on a doc that never quiets down).
    ///
    /// A clean doc (`first_dirty_at.is_none()`) is never due.
    fn flush_due(&self, now: Instant) -> bool {
        let (Some(first), Some(last)) = (self.first_dirty_at, self.last_dirty_at) else {
            return false;
        };
        now.duration_since(last) >= self.limits.debounce_idle
            || now.duration_since(first) >= self.limits.debounce_max
    }

    /// Flush the CRDT text to the file's blob. Idempotent: a call with
    /// unchanged CRDT text since the last flush is a no-op. Returns
    /// `Ok(true)` if a write actually happened, `Ok(false)` if the
    /// hash matched and the call short-circuited.
    ///
    /// **Sequence** (mirrors `docs/plan/markdown-collab.md § Backend
    /// step 4`):
    ///   1. Force a snapshot so any in-memory-only updates hit
    ///      `collab.doc_sessions.state` BEFORE the blob write. Prevents
    ///      "blob newer than persisted CRDT" on a crash between step 3
    ///      and the next snapshot tick.
    ///   2. Extract `Y.Text` UTF-8 bytes. Compute BLAKE3 (via the dedup
    ///      pipeline inside the writer — no separate hash here).
    ///   3. Short-circuit if the CRDT text matches
    ///      `last_flushed_content_hash` (idempotency guard for the
    ///      "same content, timer fired anyway" case).
    ///   4. Delegate to `self.writer.write_content(caller_id, file_id,
    ///      bytes)` — the writer ingests via dedup, swaps the file's
    ///      blob, invalidates the content cache, fires the lifecycle
    ///      hook. Returns the new content hash.
    ///   5. Stamp `last_flushed_content_hash` (both in the DB via
    ///      `repo.record_flush` and in memory) so the next tick's
    ///      short-circuit works. Reset dirty timestamps.
    async fn flush_to_blob(&mut self) -> Result<bool, CollabError> {
        // Nothing to flush if nobody's written since attach — the
        // blob already matches the seed content.
        let Some(caller_id) = self.last_writer_id else {
            return Ok(false);
        };

        // 1. Force snapshot so CRDT state is durable before we touch
        //    the blob. Cheap when there's nothing new to snapshot.
        self.snapshot().await?;

        // 2. Extract text.
        let text = self.text();
        let bytes = text.into_bytes();

        // 3. Idempotency short-circuit. We compute the hash the same
        //    way the dedup pipeline would (BLAKE3 of the whole
        //    stream) via a peek at the dedup service — but that
        //    would double the cost. Simpler: compute BLAKE3 here and
        //    compare. If the writer's internal hashing disagrees
        //    with ours, dedup will still ref-count correctly; the
        //    only cost is a stale short-circuit-miss. Same tool the
        //    upload path uses (blake3 crate).
        let content_hash = blake3::hash(&bytes).to_hex().to_string();
        if self.last_flushed_content_hash.as_deref() == Some(content_hash.as_str()) {
            // Unchanged since last flush. Reset the debounce clock so
            // we don't re-tick on the same clean state.
            self.first_dirty_at = None;
            self.last_dirty_at = None;
            return Ok(false);
        }

        // 4. Delegate the blob write. The writer's returned hash may
        //    differ from `content_hash` (raw BLAKE3 of the text) —
        //    for a chunked file it's the manifest hash, not the raw
        //    content hash. We use OUR content_hash as the
        //    short-circuit key so the compare on next tick stays
        //    consistent regardless of chunking geometry; the writer's
        //    return is verified but not tracked.
        //    `last_known_blob_hash` rides along as the precondition:
        //    the swap lands only if the row still points at the blob
        //    this CRDT was derived from.
        let writer_hash = match self
            .writer
            .write_content(
                caller_id,
                self.file_id,
                bytes,
                self.last_known_blob_hash.clone(),
            )
            .await
        {
            Ok(h) => h,
            Err(e) if e.kind == ErrorKind::PreconditionFailed => {
                // Somebody wrote this file from outside the session —
                // WebDAV PUT, WOPI, a re-upload — after we seeded. The
                // blob on disk is NOT what our CRDT was derived from,
                // so writing our text would destroy their content
                // wholesale. Refuse, and leave the row exactly as the
                // external writer left it.
                //
                // Deliberately NOT retried with a refreshed baseline:
                // that is just the blind overwrite again, one round
                // trip later. `CollabEvictLifecycleHook` is already in
                // flight for this same write and will tear the session
                // down; every client then re-attaches and re-seeds
                // from the new blob.
                //
                // The dirty marks are left standing on purpose. They
                // are the record that this actor holds unflushed text,
                // and clearing them here would let the session go on
                // looking clean while its edits exist nowhere but RAM.
                tracing::warn!(
                    target: "audit",
                    event = "collab.flush_conflict",
                    reason = "external_write",
                    caller_id = %caller_id,
                    file_id = %self.file_id,
                    expected_blob_hash = ?self.last_known_blob_hash,
                    "👮🏻‍♂️ collab flush refused: file changed outside the session; not overwriting",
                );
                return Err(CollabError::Storage(e));
            }
            Err(e) => return Err(CollabError::Storage(e)),
        };

        // Carry the new blob hash forward as the next flush's
        // precondition, keeping the compare-and-swap chain unbroken.
        // Distinct from `content_hash` below: for a chunked file this
        // is the manifest hash, and it is what the file row actually
        // stores, so it is the only value a CAS can be built on.
        self.last_known_blob_hash = Some(writer_hash);

        // 5. Stamp DB + in-memory mirror WITH `content_hash` (the
        //    raw BLAKE3 of the text we just wrote). This matches
        //    what `last_flushed_content_hash` is designed to hold
        //    per the migration comment: "content hash of the last
        //    successful flush-to-blob write." Order matters: DB
        //    first, so a crash between DB stamp and in-memory update
        //    loses only the short-circuit optimisation on next boot.
        self.repo.record_flush(self.file_id, &content_hash).await?;
        self.last_flushed_content_hash = Some(content_hash);
        self.first_dirty_at = None;
        self.last_dirty_at = None;
        Ok(true)
    }
}

/// Run one session actor's message loop until every sender drops OR
/// a `Shutdown` message arrives. Called by [`CollabSessionService`]'s
/// spawn helper. Errors on messages surface via each message's own
/// reply channel — the loop itself doesn't propagate them.
///
/// The loop `select!`s over three sources:
///   * `inbox.recv()` — the primary event source (attach, apply,
///     sync, subscribe, shutdown).
///   * `tick.tick()` — the debouncer heartbeat (default 1 s,
///     configurable via `CollabLimits::debounce_tick`). On every
///     tick we check `state.flush_due(now)` and self-invoke
///     `flush_to_blob()` inline when it returns true. Inline (not
///     via a self-message) so the flush observes the current state
///     and no ApplyUpdate can interleave between the "due" check
///     and the flush proper.
///   * (implicit) shutdown — a `Shutdown` message OR the last
///     sender dropping breaks the loop cleanly.
async fn run_actor(mut state: ActorState, mut inbox: mpsc::Receiver<SessionMsg>) {
    let mut tick = tokio::time::interval(state.limits.debounce_tick);
    // Coalesce backlog if the runtime pauses under heavy load rather
    // than firing a burst of catch-up ticks when it recovers.
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Discard the immediate first tick — the actor just spawned; a
    // freshly-seeded doc has no dirty bytes and would no-op anyway,
    // but the extra tick would waste a syscall on every actor birth.
    tick.tick().await;

    loop {
        tokio::select! {
            // biased: process control-plane messages before the tick
            // branch. A burst of applies + one flush deadline should
            // apply all updates first, then flush the accumulated
            // state — not flush mid-burst and re-arm.
            biased;

            msg = inbox.recv() => {
                let Some(msg) = msg else { break };
                match msg {
                    SessionMsg::AttachSocket { socket_id, reply } => {
                        state.attached.insert(socket_id);
                        let _ = reply.send(());
                    }
                    SessionMsg::DetachSocket {
                        socket_id,
                        remaining_sockets,
                    } => {
                        state.attached.remove(&socket_id);
                        let _ = remaining_sockets.send(state.attached.len());
                    }
                    SessionMsg::ApplyUpdate { caller_id, bytes, reply } => {
                        let result = state.apply_update(caller_id, bytes).await;
                        let _ = reply.send(result);
                    }
                    SessionMsg::ApplyAwareness { bytes, reply } => {
                        state.broadcast_awareness(bytes);
                        let _ = reply.send(());
                    }
                    SessionMsg::GetText { reply } => {
                        let _ = reply.send(state.text());
                    }
                    SessionMsg::Snapshot { reply } => {
                        let _ = reply.send(state.snapshot().await);
                    }
                    SessionMsg::SyncStep1 {
                        client_state_vector,
                        reply,
                    } => {
                        let _ = reply.send(state.sync_step_1(&client_state_vector));
                    }
                    SessionMsg::SubscribeUpdates { reply } => {
                        // Hand out a fresh receiver on the actor's outbox.
                        // `broadcast::Sender::subscribe` produces a NEW receiver
                        // that only sees updates from this point forward —
                        // catch-up is Yjs's problem, not the broadcast
                        // channel's (clients issue sync-step-1 on attach).
                        let _ = reply.send(state.outbox.subscribe());
                    }
                    SessionMsg::FlushToBlob { reply } => {
                        let result = state.flush_to_blob().await;
                        let _ = reply.send(result);
                    }
                    SessionMsg::Evict { reason } => {
                        // Best-effort broadcast — solo sessions (no
                        // live receivers) return `Err(SendError)` which
                        // is fine: nobody to notify. The kind byte is
                        // deliberately server-only ([`INTERNAL_KIND_EVICTED`]);
                        // the WS forwarder catches it and synthesizes
                        // an `rt.revoked` text frame rather than
                        // forwarding a bogus binary kind to clients.
                        let _ = state.outbox.send((
                            INTERNAL_KIND_EVICTED,
                            reason.as_bytes().to_vec(),
                        ));
                        break;
                    }
                    SessionMsg::Shutdown => break,
                }
            }

            _ = tick.tick() => {
                if state.flush_due(Instant::now()) {
                    // Fire-and-log. A flush failure (blob write blip,
                    // DB unavailability) leaves dirty_at set so the
                    // NEXT tick retries — no data loss, just deferred
                    // materialization. The audit line goes to the
                    // `oxicloud::collab` target so operators can spot
                    // sustained failures without an audit-channel
                    // false positive on every tick.
                    if let Err(e) = state.flush_to_blob().await {
                        // Non-recoverable classes shut the actor down
                        // rather than retry forever. Today the main
                        // one is "file row disappeared" — the DELETE
                        // cascade dropped `collab.doc_sessions` while
                        // this actor kept editing in-memory. The plan
                        // eviction path (AuthzChanged file-scoped) will
                        // pre-empt this once wired; until then, a
                        // NotFound stops the retry loop cleanly.
                        let terminal = matches!(
                            &e,
                            CollabError::Storage(de) if de.kind == crate::common::errors::ErrorKind::NotFound
                        );
                        // An external write replaced the blob. Our text
                        // can NEVER reach this file now — the baseline
                        // will not match again for the life of this
                        // actor, so every later tick would fail
                        // identically while the doc keeps accepting
                        // keystrokes it cannot persist.
                        //
                        // Stop immediately and say why. `flush_due`
                        // only fires on a dirty doc, so reaching here
                        // always means there IS unsaved work — which is
                        // exactly what separates this from the plain
                        // `external_write` eviction the lifecycle hook
                        // raises. Clients seeing this reason must
                        // preserve their buffer before re-attaching.
                        if matches!(
                            &e,
                            CollabError::Storage(de) if de.kind == crate::common::errors::ErrorKind::PreconditionFailed
                        ) {
                            tracing::warn!(
                                target: "audit",
                                event = "collab.session_conflicted",
                                reason = "external_write_conflict",
                                file_id = %state.file_id,
                                attached_sockets = state.attached.len(),
                                "👮🏻‍♂️ collab session ended with unflushed edits: file changed outside the session",
                            );
                            let _ = state.outbox.send((
                                INTERNAL_KIND_EVICTED,
                                b"external_write_conflict".to_vec(),
                            ));
                            break;
                        }
                        tracing::warn!(
                            target: "oxicloud::collab",
                            file_id = %state.file_id,
                            error = %e,
                            terminal,
                            "🧵 debounced flush failed{}",
                            if terminal { " (terminal — shutting actor down)" } else { "; will retry on next tick" },
                        );
                        if terminal {
                            break;
                        }
                    }
                }
            }
        }
    }
}

// ════════════════════════════════════════════════════════════════════════════
// Service registry
// ════════════════════════════════════════════════════════════════════════════

/// Registry of live per-file collab actors. Held in `AppState` when
/// the collab feature is enabled; each `attach_file` call either
/// returns an existing actor or spawns a fresh one seeded from the
/// blob.
pub struct CollabSessionService {
    sessions: DashMap<Uuid, CollabSession>,
    repo: Arc<dyn DocSessionRepository>,
    reader: Arc<dyn DocContentReader>,
    #[allow(dead_code)] // C7 wires flush-to-blob
    writer: Arc<dyn DocContentWriter>,
    /// Per-frame AuthZ gate — `handle_binary_frame` calls
    /// `authz.require(caller, file_id, <perm>)` before applying
    /// UPDATE frames (Update permission) or replying to SYNC frames
    /// (Read permission). The concrete engine has its own decision
    /// cache, so this stays O(1) per keystroke after the first check
    /// per (caller, file) pair; no per-service cache needed.
    authz: Arc<dyn CollabAuthzGate>,
    limits: CollabLimits,
}

impl CollabSessionService {
    pub fn new(
        repo: Arc<dyn DocSessionRepository>,
        reader: Arc<dyn DocContentReader>,
        writer: Arc<dyn DocContentWriter>,
        authz: Arc<dyn CollabAuthzGate>,
        limits: CollabLimits,
    ) -> Self {
        Self {
            sessions: DashMap::new(),
            repo,
            reader,
            writer,
            authz,
            limits,
        }
    }

    /// Return the actor for `file_id`, spawning one if none exists.
    /// The `caller_id` is passed through to `DocContentReader` when
    /// seeding a fresh doc so the read is audited as "collab on
    /// behalf of user X".
    pub async fn attach_file(
        &self,
        caller_id: Uuid,
        file_id: Uuid,
    ) -> Result<CollabSession, CollabError> {
        // Fast path: session already spawned — clone the handle.
        if let Some(existing) = self.sessions.get(&file_id) {
            return Ok(existing.clone());
        }
        // Slow path: load persisted state OR seed from the blob.
        // Read seed content BEFORE holding the DashMap slot so the
        // network call doesn't block other file-ids' attaches.
        let seed = match self.repo.load(file_id).await? {
            Some(_) => None, // load-path — actor reads from repo itself
            None => Some(self.reader.read_content(caller_id, file_id).await?),
        };
        // The blob this actor's CRDT corresponds to, as of right now.
        // Baseline for the flush's compare-and-swap — read on EVERY
        // attach, not just the seeding one: on the load-path the
        // snapshot may have been persisted long ago while the blob
        // moved on, and an actor respawning after an idle GC must
        // compare against today's blob, not the one it last wrote.
        // Best-effort — a lookup failure leaves the baseline unknown
        // and the flush writes blind, exactly as it did before.
        let baseline_blob_hash = match self.reader.current_blob_hash(caller_id, file_id).await {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(
                    target: "oxicloud::collab",
                    file_id = %file_id,
                    error = %e,
                    "could not read baseline blob hash — flush will not be able to detect an external write",
                );
                None
            }
        };
        let state = ActorState::load_or_seed(
            file_id,
            self.repo.clone(),
            self.writer.clone(),
            seed,
            baseline_blob_hash,
            self.limits,
        )
        .await?;
        let (tx, rx) = mpsc::channel::<SessionMsg>(64);
        tokio::spawn(run_actor(state, rx));
        let session = CollabSession { inbox: tx };
        // Race: another task may have spawned first. entry() collapses
        // the two to whichever landed first; the other's tokio::spawn
        // simply exits when its inbox drops.
        Ok(self.sessions.entry(file_id).or_insert(session).clone())
    }

    /// Test / graceful-shutdown helper. Drops the session's handle
    /// from the registry AND signals shutdown; the actor's `run` loop
    /// exits after any in-flight message completes.
    pub async fn shutdown(&self, file_id: Uuid) {
        if let Some((_, session)) = self.sessions.remove(&file_id) {
            session.shutdown().await;
        }
    }

    /// Number of live actors. Test/introspection only.
    pub fn live_session_count(&self) -> usize {
        self.sessions.len()
    }

    /// Evict the live collaborative session (if any) attached to
    /// `file_id`, notifying every attached socket via a control
    /// message on the actor's outbox. The WS forwarder translates
    /// that message into an `rt.revoked` JSON frame — the client
    /// sees a distinct terminal state (`SyncState::disconnected`
    /// with the given reason).
    ///
    /// Called from the file-mutation paths that invalidate an
    /// in-memory CRDT view:
    ///
    ///   * **`resource_deleted`** — file is being trashed / permanently
    ///     deleted. Continuing to edit would be pointless, and the
    ///     `collab.doc_sessions` row is about to cascade-delete via
    ///     the FK (see migration `20261029000001`).
    ///   * **`external_write`** — an out-of-band write (WebDAV PUT,
    ///     WOPI overwrite, REST upload replacing the file contents)
    ///     replaced the file's blob under the CRDT's feet; the actor's
    ///     doc is now stale relative to the on-disk truth. Re-attach
    ///     forces a fresh seed. (Wired on a follow-up slice.)
    ///
    /// Safe to call speculatively — a no-op if no session is live.
    /// Idempotent — repeat calls with the same reason drop cleanly.
    pub async fn evict_sessions_for_file(&self, file_id: Uuid, reason: &'static str) {
        if let Some((_, session)) = self.sessions.remove(&file_id) {
            session.evict(reason).await;
            tracing::info!(
                target: "audit",
                event = "collab.session_evicted",
                reason = reason,
                file_id = %file_id,
                "🚫 collab session evicted",
            );
        }
    }

    /// Idle-GC sweep: reap `collab.doc_sessions` rows whose
    /// `last_activity_at` is older than `older_than`, and drop the
    /// live actor (if any) for each. Called on a schedule by the
    /// `collab_idle_gc` job (see `docs/plan/markdown-collab.md
    /// § Backend step 5`). Returns the number of rows swept.
    ///
    /// For each stale file_id, the sequence is:
    ///
    ///   1. If a live actor exists in the registry, ask it to
    ///      `flush_to_blob` — belt-and-braces vs the periodic flush,
    ///      so nothing dirty escapes the GC. Idempotent (short-circuits
    ///      on unchanged content hash).
    ///   2. Ask the actor to `shutdown` — the run loop breaks after
    ///      any in-flight message finishes.
    ///   3. Remove the row from `collab.doc_sessions` so a subsequent
    ///      attach re-seeds fresh from the (now flushed) blob.
    ///
    /// A failure at any step logs and moves on to the next row — the
    /// sweep is best-effort. The next tick retries whatever this
    /// tick missed.
    pub async fn gc_stale(
        &self,
        older_than: chrono::DateTime<chrono::Utc>,
        limit: i64,
    ) -> Result<usize, CollabError> {
        let stale = self.repo.list_stale(older_than, limit).await?;
        let count = stale.len();
        for file_id in stale {
            // Step 1: final flush if we have a live actor.
            if let Some(session_entry) = self.sessions.get(&file_id) {
                let session = session_entry.clone();
                drop(session_entry); // release DashMap read guard before await
                if let Err(e) = session.flush_to_blob().await {
                    tracing::warn!(
                        target: "oxicloud::collab",
                        file_id = %file_id,
                        error = %e,
                        "🧹 idle-GC: final flush failed; deleting anyway",
                    );
                }
            }
            // Step 2: drop the actor (best-effort).
            self.shutdown(file_id).await;
            // Step 3: remove the DB row so the next attach re-seeds
            //         fresh from the blob (which now carries the CRDT text).
            if let Err(e) = self.repo.delete(file_id).await {
                tracing::warn!(
                    target: "oxicloud::collab",
                    file_id = %file_id,
                    error = %e,
                    "🧹 idle-GC: delete failed; will retry on next tick",
                );
            }
        }
        Ok(count)
    }

    /// Route an incoming binary frame from the WS handler to the right
    /// per-file actor. The caller must have already passed the
    /// subscribe-time `Read` gate on `Topic::Collab(file_id)`; this
    /// method assumes that check succeeded.
    ///
    /// Return value:
    /// - `Ok(Some(reply_frame))` — a frame the caller must send back
    ///   on this socket (sync-step-2 reply to a client's sync-step-1).
    /// - `Ok(None)` — accepted, nothing to reply on this socket.
    ///   For a `0x01` UPDATE the bytes are ALSO broadcast on the
    ///   actor's outbox; every WS forwarder subscribed via
    ///   [`CollabSession::subscribe_updates`] will push the same
    ///   bytes to its socket as a `0x01` frame.
    /// - `Err(CollabError)` — protocol violation or CRDT-apply error;
    ///   the caller closes the socket with a `collab.protocol_violation`
    ///   audit line + WS close 1002.
    ///
    /// **Per-frame AuthZ:**
    /// - `0x01 UPDATE`  → `Permission::Update` (mutates the doc)
    /// - `0x03 SYNC`    → `Permission::Read`   (reveals doc content)
    /// - `0x02 AWARENESS` → NOT gated (presence-only: cursor position,
    ///   user handle, colour — does not leak doc content and does not
    ///   mutate state, so a viewer who cleared the subscribe-time
    ///   Read gate is allowed to show their cursor).
    ///
    /// Read is defensively re-checked on SYNC even though it was
    /// already gated at subscribe time: the WS handler accepts binary
    /// frames without requiring a prior subscribe, so a caller with a
    /// valid JWT but no Read grant could otherwise send SYNC and
    /// receive doc content.
    pub async fn handle_binary_frame(
        &self,
        caller_id: Uuid,
        frame: crate::application::services::collab_wire::BinaryFrame,
    ) -> Result<Option<Vec<u8>>, CollabError> {
        use crate::application::services::collab_wire::kind;
        let file_id = frame.file_id;
        match frame.kind {
            kind::UPDATE => {
                self.require_perm(caller_id, file_id, Permission::Update, "update")
                    .await?;
                let session = self.attach_file(caller_id, file_id).await?;
                session.apply_update(caller_id, frame.payload).await?;
                Ok(None)
            }
            kind::AWARENESS => {
                // Presence-only; not persisted, not applied to the
                // CRDT, and does not leak doc bytes. Subscribe-time
                // Read gate is sufficient — a viewer's cursor is
                // legitimate. Fan out to every other subscribed
                // socket so peers render each other's cursors.
                let session = self.attach_file(caller_id, file_id).await?;
                session.apply_awareness(frame.payload).await?;
                Ok(None)
            }
            kind::SYNC => {
                self.require_perm(caller_id, file_id, Permission::Read, "read")
                    .await?;
                let session = self.attach_file(caller_id, file_id).await?;
                // Client → server: sync-step-1 (state vector). Reply
                // with sync-step-2 (diff) bytes; the WS handler wraps
                // them in another 0x03 binary frame.
                let reply_payload = session.sync_step_1(frame.payload).await?;
                Ok(Some(reply_payload))
            }
            other => Err(CollabError::BadUpdate(format!(
                "unknown frame kind 0x{other:02x} (routed past parser?)"
            ))),
        }
    }

    /// Internal AuthZ helper for `handle_binary_frame`. Wraps
    /// `AuthorizationEngine::require` so a denial surfaces as
    /// [`CollabError::AuthzDenied`] rather than the engine's raw
    /// `DomainError`, which the WS handler would otherwise have to
    /// pattern-match on. The engine's own `authz.denied` audit line
    /// fires from inside `require`; the collab-side audit line
    /// (`collab.write_denied` / `collab.read_denied`) fires from the
    /// WS handler on `AuthzDenied`, so both sides of the denial trail
    /// are captured without duplication.
    async fn require_perm(
        &self,
        caller_id: Uuid,
        file_id: Uuid,
        permission: Permission,
        label: &'static str,
    ) -> Result<(), CollabError> {
        match self.authz.require(caller_id, file_id, permission).await {
            Ok(()) => Ok(()),
            Err(_) => Err(CollabError::AuthzDenied {
                permission: label,
                file_id,
            }),
        }
    }
}

// Yield helpers for tests + external callers that need to wait for
// the actor to drain a message. `tokio::task::yield_now()` alone
// isn't enough; the actor may be mid-`await` on the repo. This helper
// keeps that intent centralised.
#[allow(dead_code)]
pub(crate) async fn drain_tick() {
    tokio::time::sleep(Duration::from_millis(1)).await;
}

// ════════════════════════════════════════════════════════════════════════════
// Tests
// ════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use chrono::Utc;
    use std::sync::Mutex;
    use tokio::sync::RwLock;

    // ── In-memory repo stub ────────────────────────────────────────────

    struct MemRepo {
        rows: RwLock<std::collections::HashMap<Uuid, StoredDocSession>>,
    }
    impl MemRepo {
        fn new() -> Self {
            Self {
                rows: RwLock::new(Default::default()),
            }
        }
    }
    #[async_trait]
    impl DocSessionRepository for MemRepo {
        async fn load(&self, file_id: Uuid) -> Result<Option<StoredDocSession>, DomainError> {
            Ok(self.rows.read().await.get(&file_id).cloned())
        }
        async fn save_snapshot(
            &self,
            file_id: Uuid,
            state: &[u8],
            state_vector: &[u8],
        ) -> Result<(), DomainError> {
            let now = Utc::now();
            let mut rows = self.rows.write().await;
            let existing = rows.remove(&file_id);
            rows.insert(
                file_id,
                StoredDocSession {
                    file_id,
                    state: state.to_vec(),
                    state_vector: state_vector.to_vec(),
                    updates_since_snapshot: 0,
                    last_flushed_content_hash: existing
                        .as_ref()
                        .and_then(|r| r.last_flushed_content_hash.clone()),
                    last_flushed_at: existing.as_ref().and_then(|r| r.last_flushed_at),
                    last_activity_at: now,
                    created_at: existing.map(|r| r.created_at).unwrap_or(now),
                },
            );
            Ok(())
        }
        async fn touch_after_update(&self, file_id: Uuid) -> Result<(), DomainError> {
            let mut rows = self.rows.write().await;
            if let Some(r) = rows.get_mut(&file_id) {
                r.updates_since_snapshot += 1;
                r.last_activity_at = Utc::now();
            }
            Ok(())
        }
        async fn record_flush(&self, file_id: Uuid, content_hash: &str) -> Result<(), DomainError> {
            let mut rows = self.rows.write().await;
            if let Some(r) = rows.get_mut(&file_id) {
                r.last_flushed_content_hash = Some(content_hash.into());
                r.last_flushed_at = Some(Utc::now());
            }
            Ok(())
        }
        async fn delete(&self, file_id: Uuid) -> Result<(), DomainError> {
            self.rows.write().await.remove(&file_id);
            Ok(())
        }
        async fn list_stale(
            &self,
            older_than: chrono::DateTime<chrono::Utc>,
            limit: i64,
        ) -> Result<Vec<Uuid>, DomainError> {
            let rows = self.rows.read().await;
            let mut stale: Vec<(chrono::DateTime<chrono::Utc>, Uuid)> = rows
                .iter()
                .filter(|(_, r)| r.last_activity_at < older_than)
                .map(|(id, r)| (r.last_activity_at, *id))
                .collect();
            stale.sort_by_key(|(t, _)| *t);
            Ok(stale
                .into_iter()
                .take(limit as usize)
                .map(|(_, id)| id)
                .collect())
        }
    }

    // ── Stub reader / writer ──────────────────────────────────────────

    struct StubReader {
        content: Mutex<Vec<u8>>,
        /// Baseline the actor reads at attach and uses as its first
        /// flush precondition. `None` models "hash unknown", which
        /// deliberately falls back to a blind write.
        blob_hash: Mutex<Option<String>>,
    }
    #[async_trait]
    impl DocContentReader for StubReader {
        async fn read_content(
            &self,
            _caller_id: Uuid,
            _file_id: Uuid,
        ) -> Result<Vec<u8>, DomainError> {
            Ok(self.content.lock().unwrap().clone())
        }

        async fn current_blob_hash(
            &self,
            _caller_id: Uuid,
            _file_id: Uuid,
        ) -> Result<Option<String>, DomainError> {
            Ok(self.blob_hash.lock().unwrap().clone())
        }
    }

    struct StubWriter;
    #[async_trait]
    impl DocContentWriter for StubWriter {
        async fn write_content(
            &self,
            _caller_id: Uuid,
            _file_id: Uuid,
            _content: Vec<u8>,
            _expected_blob_hash: Option<String>,
        ) -> Result<String, DomainError> {
            Ok("b3-stub".into())
        }
    }

    /// Writer that models the repository's compare-and-swap: it holds
    /// the blob hash the "file row" currently points at, and refuses
    /// any write whose precondition no longer matches.
    struct CasWriter {
        /// What the row points at. `external_write` moves it, standing
        /// in for a WebDAV PUT / WOPI / re-upload landing mid-session.
        current: Mutex<Option<String>>,
        /// Preconditions seen, so a test can prove one was actually
        /// sent rather than the write being blind.
        seen_preconditions: Mutex<Vec<Option<String>>>,
        writes: Mutex<usize>,
    }

    impl CasWriter {
        fn new(initial: &str) -> Self {
            Self {
                current: Mutex::new(Some(initial.to_string())),
                seen_preconditions: Mutex::new(Vec::new()),
                writes: Mutex::new(0),
            }
        }
        /// Simulate a writer outside the collab session replacing the
        /// blob.
        fn external_write(&self, new_hash: &str) {
            *self.current.lock().unwrap() = Some(new_hash.to_string());
        }
        fn write_count(&self) -> usize {
            *self.writes.lock().unwrap()
        }
        fn current_hash(&self) -> Option<String> {
            self.current.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl DocContentWriter for CasWriter {
        async fn write_content(
            &self,
            _caller_id: Uuid,
            _file_id: Uuid,
            content: Vec<u8>,
            expected_blob_hash: Option<String>,
        ) -> Result<String, DomainError> {
            self.seen_preconditions
                .lock()
                .unwrap()
                .push(expected_blob_hash.clone());
            let mut current = self.current.lock().unwrap();
            if let Some(expected) = expected_blob_hash.as_deref()
                && current.as_deref() != Some(expected)
            {
                return Err(DomainError::new(
                    ErrorKind::PreconditionFailed,
                    "collab-test",
                    "blob hash moved",
                ));
            }
            let new_hash = format!("b3-collab-{}", blake3::hash(&content).to_hex());
            *current = Some(new_hash.clone());
            *self.writes.lock().unwrap() += 1;
            Ok(new_hash)
        }
    }

    /// Writer stub that records every `write_content` invocation so the
    /// debouncer test can assert on call count + bytes + caller_id.
    /// Returns a distinct hash per call so the actor's in-memory
    /// `last_flushed_content_hash` mirror stays honest across
    /// successive flushes.
    #[derive(Default)]
    struct RecordingWriter {
        writes: Mutex<Vec<(Uuid, Uuid, Vec<u8>)>>,
    }
    #[async_trait]
    impl DocContentWriter for RecordingWriter {
        async fn write_content(
            &self,
            caller_id: Uuid,
            file_id: Uuid,
            content: Vec<u8>,
            _expected_blob_hash: Option<String>,
        ) -> Result<String, DomainError> {
            let mut writes = self.writes.lock().unwrap();
            let hash = format!("b3-recorded-{}", writes.len());
            writes.push((caller_id, file_id, content));
            Ok(hash)
        }
    }

    // ── Stub authz gate ──────────────────────────────────────────
    //
    // Three shapes used by the tests:
    //   * `AllowAll`  — every check permitted; used by legacy tests
    //     that pre-date the gate and don't care about AuthZ.
    //   * `DenyAll`   — every check denied; used to prove the gate is
    //     actually consulted.
    //   * `ReadOnly`  — Read allowed, everything else denied; models
    //     a Viewer role for the mixed permission tests.

    struct AllowAll;
    #[async_trait]
    impl CollabAuthzGate for AllowAll {
        async fn require(
            &self,
            _caller_id: Uuid,
            _file_id: Uuid,
            _permission: Permission,
        ) -> Result<(), DomainError> {
            Ok(())
        }
    }

    struct DenyAll;
    #[async_trait]
    impl CollabAuthzGate for DenyAll {
        async fn require(
            &self,
            _caller_id: Uuid,
            _file_id: Uuid,
            _permission: Permission,
        ) -> Result<(), DomainError> {
            Err(DomainError::new(
                ErrorKind::AccessDenied,
                "collab-test",
                "deny-all gate".to_string(),
            ))
        }
    }

    struct ReadOnly;
    #[async_trait]
    impl CollabAuthzGate for ReadOnly {
        async fn require(
            &self,
            _caller_id: Uuid,
            _file_id: Uuid,
            permission: Permission,
        ) -> Result<(), DomainError> {
            if permission == Permission::Read {
                Ok(())
            } else {
                Err(DomainError::new(
                    ErrorKind::AccessDenied,
                    "collab-test",
                    format!("read-only gate: {permission:?}"),
                ))
            }
        }
    }

    fn service_with_seed(seed: &str, limits: CollabLimits) -> Arc<CollabSessionService> {
        service_with_seed_and_gate(seed, limits, Arc::new(AllowAll))
    }

    fn service_with_seed_and_gate(
        seed: &str,
        limits: CollabLimits,
        authz: Arc<dyn CollabAuthzGate>,
    ) -> Arc<CollabSessionService> {
        Arc::new(CollabSessionService::new(
            Arc::new(MemRepo::new()),
            Arc::new(StubReader {
                content: Mutex::new(seed.as_bytes().to_vec()),
                blob_hash: Mutex::new(None),
            }),
            Arc::new(StubWriter),
            authz,
            limits,
        ))
    }

    #[tokio::test]
    async fn first_attach_seeds_doc_from_blob_content() {
        let svc = service_with_seed("# Hello\nworld", CollabLimits::default());
        let session = svc.attach_file(Uuid::nil(), Uuid::new_v4()).await.unwrap();
        let text = session.get_text().await.unwrap();
        assert_eq!(text, "# Hello\nworld");
    }

    #[tokio::test]
    async fn two_clients_apply_updates_and_converge() {
        // Simulate two "clients" as two independent yrs::Doc instances
        // that talk to each other via the server-side session actor.
        // Convergence means: after every party has seen every update,
        // both clients + the server-side doc all show the same text.
        let svc = service_with_seed("", CollabLimits::default());
        let file_id = Uuid::new_v4();
        let session = svc.attach_file(Uuid::nil(), file_id).await.unwrap();

        // Client A appends "Hello ".
        let doc_a = Doc::new();
        {
            let ta = doc_a.get_or_insert_text(ROOT_TEXT_NAME);
            let mut tx = doc_a.transact_mut();
            ta.insert(&mut tx, 0, "Hello ");
        }
        let update_a = doc_a
            .transact()
            .encode_state_as_update_v1(&StateVector::default());
        session
            .apply_update(Uuid::nil(), update_a.clone())
            .await
            .unwrap();

        // Client B (independent doc) appends "world".
        let doc_b = Doc::new();
        // B first sees A's update so its insert offset lines up
        // deterministically with A's — real Yjs clients get sync-step-2
        // on attach; this simulates that.
        {
            let update = Update::decode_v1(&update_a).unwrap();
            doc_b.transact_mut().apply_update(update).unwrap();
        }
        {
            let tb = doc_b.get_or_insert_text(ROOT_TEXT_NAME);
            let mut tx = doc_b.transact_mut();
            tb.insert(&mut tx, 6, "world");
        }
        // Send B's delta (what B added on top of what it received).
        let update_b = doc_b.transact().encode_state_as_update_v1(
            &StateVector::decode_v1(&doc_a.transact().state_vector().encode_v1()).unwrap(),
        );
        session.apply_update(Uuid::nil(), update_b).await.unwrap();

        let server_text = session.get_text().await.unwrap();
        assert_eq!(server_text, "Hello world");
    }

    #[tokio::test]
    async fn snapshot_compaction_triggers_at_threshold() {
        // Threshold 3 so we don't have to fire 200 updates in a test.
        let limits = CollabLimits {
            snapshot_after_updates: 3,
            ..CollabLimits::default()
        };
        let svc = service_with_seed("", limits);
        let file_id = Uuid::new_v4();
        let session = svc.attach_file(Uuid::nil(), file_id).await.unwrap();

        // ONE client Doc that evolves across three iterations. On
        // each iteration we insert a character locally, then encode
        // the DELTA (state-as-update against the server's last seen
        // state vector) and send it. That's what a real Yjs client
        // does — send only what the server hasn't seen yet.
        //
        // IMPORTANT: never open a read txn while a write txn is
        // live on the same Doc — yrs locks a per-Doc RwLock and
        // nested reader-under-writer (or the reverse — reader
        // temporary evaluated as an argument to a call that already
        // took the writer) deadlocks with no timeout. Each txn
        // below lives in its own tight scope, and encoding is done
        // OUTSIDE any active mutation.
        let client = Doc::new();
        let mut server_sv = StateVector::default();
        for ch in ['a', 'b', 'c'] {
            // Local edit inside its own scope so the mut txn drops
            // BEFORE we open a read txn for encoding below.
            {
                let text = client.get_or_insert_text(ROOT_TEXT_NAME);
                let mut txn = client.transact_mut();
                let cur_len = text.get_string(&txn).chars().count() as u32;
                text.insert(&mut txn, cur_len, &ch.to_string());
            }
            // Encode the delta: what the server hasn't seen yet.
            let delta = client.transact().encode_state_as_update_v1(&server_sv);
            session.apply_update(Uuid::nil(), delta).await.unwrap();
            // Mirror the server's state vector locally so the next
            // delta is minimal.
            server_sv = client.transact().state_vector();
        }

        // Convergence: the server-side doc must show "abc" — same
        // string the client sees locally. Also proves compaction
        // fired (the actor's apply_update snapshots inline at
        // threshold), because get_text returning "abc" after 3
        // deltas would fail if the third apply had deadlocked.
        let text = session.get_text().await.unwrap();
        assert_eq!(text, "abc");
    }

    #[tokio::test]
    async fn attach_is_idempotent_across_calls() {
        let svc = service_with_seed("seed", CollabLimits::default());
        let file_id = Uuid::new_v4();
        let s1 = svc.attach_file(Uuid::nil(), file_id).await.unwrap();
        let s2 = svc.attach_file(Uuid::nil(), file_id).await.unwrap();
        // Second attach must return the SAME actor — apply_update on
        // s1 must be visible via s2.
        let d = Doc::new();
        {
            let tr = d.get_or_insert_text(ROOT_TEXT_NAME);
            tr.insert(&mut d.transact_mut(), 0, "seed");
            tr.insert(&mut d.transact_mut(), 4, " + more");
        }
        let update = d
            .transact()
            .encode_state_as_update_v1(&StateVector::default());
        s1.apply_update(Uuid::nil(), update).await.unwrap();
        let via_s2 = s2.get_text().await.unwrap();
        assert!(via_s2.contains("seed"));
        assert!(via_s2.contains("more"));
        assert_eq!(svc.live_session_count(), 1);
    }

    #[tokio::test]
    async fn shutdown_drops_the_session_from_the_registry() {
        let svc = service_with_seed("", CollabLimits::default());
        let file_id = Uuid::new_v4();
        let _ = svc.attach_file(Uuid::nil(), file_id).await.unwrap();
        assert_eq!(svc.live_session_count(), 1);
        svc.shutdown(file_id).await;
        assert_eq!(svc.live_session_count(), 0);
    }

    // ── Binary-frame router (C2 slice 2) ──────────────────────────────

    use crate::application::services::collab_wire::{BinaryFrame, kind};

    #[tokio::test]
    async fn handle_binary_frame_update_applies_to_the_actor() {
        let svc = service_with_seed("", CollabLimits::default());
        let file_id = Uuid::new_v4();

        // Build a valid Yjs update on a client-side Doc.
        let client = Doc::new();
        {
            let text = client.get_or_insert_text(ROOT_TEXT_NAME);
            let mut txn = client.transact_mut();
            text.insert(&mut txn, 0, "hello");
        }
        let update_bytes = client
            .transact()
            .encode_state_as_update_v1(&StateVector::default());

        // Route as a 0x01 frame — router should apply + return None.
        let frame = BinaryFrame {
            kind: kind::UPDATE,
            file_id,
            payload: update_bytes,
        };
        let reply = svc.handle_binary_frame(Uuid::nil(), frame).await.unwrap();
        assert_eq!(reply, None, "update frames don't reply on this socket");

        // Server-side actor now reflects the client's edit.
        let session = svc.attach_file(Uuid::nil(), file_id).await.unwrap();
        assert_eq!(session.get_text().await.unwrap(), "hello");
    }

    #[tokio::test]
    async fn handle_binary_frame_awareness_is_a_no_op_on_server_state() {
        let svc = service_with_seed("seed", CollabLimits::default());
        let file_id = Uuid::new_v4();

        // 0x02 payload is opaque presence bytes; we don't decode.
        let frame = BinaryFrame {
            kind: kind::AWARENESS,
            file_id,
            payload: vec![0xAA, 0xBB, 0xCC],
        };
        let reply = svc.handle_binary_frame(Uuid::nil(), frame).await.unwrap();
        assert_eq!(reply, None);

        // Doc text unchanged — awareness is presence, not content.
        let session = svc.attach_file(Uuid::nil(), file_id).await.unwrap();
        assert_eq!(session.get_text().await.unwrap(), "seed");
    }

    #[tokio::test]
    async fn handle_binary_frame_sync_replies_with_diff_that_catches_client_up() {
        let svc = service_with_seed("hello world", CollabLimits::default());
        let file_id = Uuid::new_v4();

        // Client's state vector is empty — it wants everything.
        let empty_sv = StateVector::default().encode_v1();
        let frame = BinaryFrame {
            kind: kind::SYNC,
            file_id,
            payload: empty_sv,
        };
        let reply = svc
            .handle_binary_frame(Uuid::nil(), frame)
            .await
            .unwrap()
            .expect("SYNC frames reply with sync-step-2 bytes");

        // Applying the reply on a fresh client Doc reproduces the
        // server text — the whole point of sync-step-2.
        let client = Doc::new();
        {
            let update = Update::decode_v1(&reply).unwrap();
            client.transact_mut().apply_update(update).unwrap();
        }
        let text_on_client = {
            let text = client.get_or_insert_text(ROOT_TEXT_NAME);
            text.get_string(&client.transact())
        };
        assert_eq!(text_on_client, "hello world");
    }

    #[tokio::test]
    async fn handle_binary_frame_sync_with_current_state_vector_returns_empty_diff() {
        // Regression guard: a client whose state vector already
        // matches the server MUST get an empty(-ish) sync-step-2, not
        // a resend of the full doc. Yjs handles this internally by
        // returning a minimal update; the encoded bytes may be a
        // handful (framing only, no ops).
        let svc = service_with_seed("hello", CollabLimits::default());
        let file_id = Uuid::new_v4();

        // Seed a client, get it in sync, then re-issue sync-step-1.
        let client = Doc::new();
        // Client learns about the server's content first.
        let empty_sv = StateVector::default().encode_v1();
        let first_reply = svc
            .handle_binary_frame(
                Uuid::nil(),
                BinaryFrame {
                    kind: kind::SYNC,
                    file_id,
                    payload: empty_sv,
                },
            )
            .await
            .unwrap()
            .unwrap();
        {
            let update = Update::decode_v1(&first_reply).unwrap();
            client.transact_mut().apply_update(update).unwrap();
        }
        // Now client's SV should match the server's.
        let client_sv_now = client.transact().state_vector().encode_v1();
        let second_reply = svc
            .handle_binary_frame(
                Uuid::nil(),
                BinaryFrame {
                    kind: kind::SYNC,
                    file_id,
                    payload: client_sv_now,
                },
            )
            .await
            .unwrap()
            .unwrap();
        // The diff must NOT include the original insert — Yjs encodes
        // "no work" as a very small blob. Reasonable heuristic:
        // considerably smaller than the first (which contained the
        // full content) is enough proof.
        assert!(
            second_reply.len() < first_reply.len(),
            "in-sync client's diff ({} B) must be smaller than the initial catch-up ({} B)",
            second_reply.len(),
            first_reply.len()
        );
    }

    #[tokio::test]
    async fn handle_binary_frame_malformed_update_bytes_error() {
        let svc = service_with_seed("", CollabLimits::default());
        let file_id = Uuid::new_v4();
        let frame = BinaryFrame {
            kind: kind::UPDATE,
            file_id,
            payload: vec![0xFF, 0xFF, 0xFF],
        };
        match svc.handle_binary_frame(Uuid::nil(), frame).await {
            Err(CollabError::BadUpdate(_)) => {} // expected
            other => panic!("expected BadUpdate, got {:?}", other),
        }
    }

    // ── Fan-out via broadcast outbox (C2 slice 3) ──────────────────────
    //
    // These tests exercise the actor-side of fan-out — the WS handler's
    // forwarder task lives in `rt_ws.rs` and is covered by the api-test
    // scenario S18. Here we assert on the service surface:
    //   * `subscribe_updates` hands out a receiver, and a subsequent
    //     `apply_update` reaches it verbatim (same bytes).
    //   * Multiple concurrent subscribers each see the same update
    //     — the fan-out is a broadcast, not a queue.
    //   * A receiver taken BEFORE an update sees it; a receiver taken
    //     AFTER an update does NOT see the past one (broadcast::Sender
    //     semantics — the receiver's stream starts at "now").

    #[tokio::test]
    async fn subscribe_updates_receives_bytes_from_a_subsequent_apply() {
        let svc = service_with_seed("", CollabLimits::default());
        let file_id = Uuid::new_v4();
        let session = svc.attach_file(Uuid::nil(), file_id).await.unwrap();
        let mut rx = session.subscribe_updates().await.unwrap();

        // Build a valid Yjs update on a client-side Doc.
        let client = Doc::new();
        {
            let text = client.get_or_insert_text(ROOT_TEXT_NAME);
            let mut txn = client.transact_mut();
            text.insert(&mut txn, 0, "fan-out");
        }
        let update_bytes = client
            .transact()
            .encode_state_as_update_v1(&StateVector::default());

        session
            .apply_update(Uuid::nil(), update_bytes.clone())
            .await
            .unwrap();

        // The receiver sees the same bytes we applied, tagged as an
        // UPDATE frame (0x01) on the shared (kind, bytes) channel.
        let received = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("broadcast delivery within 1s")
            .expect("no lag / no close");
        assert_eq!(
            received.0,
            crate::application::services::collab_wire::kind::UPDATE
        );
        assert_eq!(received.1, update_bytes);
    }

    #[tokio::test]
    async fn subscribe_updates_broadcasts_to_every_live_receiver() {
        let svc = service_with_seed("", CollabLimits::default());
        let file_id = Uuid::new_v4();
        let session = svc.attach_file(Uuid::nil(), file_id).await.unwrap();

        let mut rx1 = session.subscribe_updates().await.unwrap();
        let mut rx2 = session.subscribe_updates().await.unwrap();
        let mut rx3 = session.subscribe_updates().await.unwrap();

        let client = Doc::new();
        {
            let text = client.get_or_insert_text(ROOT_TEXT_NAME);
            let mut txn = client.transact_mut();
            text.insert(&mut txn, 0, "hi");
        }
        let update_bytes = client
            .transact()
            .encode_state_as_update_v1(&StateVector::default());
        session
            .apply_update(Uuid::nil(), update_bytes.clone())
            .await
            .unwrap();

        for rx in [&mut rx1, &mut rx2, &mut rx3] {
            let received = tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .expect("broadcast delivery within 1s")
                .expect("no lag / no close");
            assert_eq!(
                received.0,
                crate::application::services::collab_wire::kind::UPDATE
            );
            assert_eq!(received.1, update_bytes);
        }
    }

    #[tokio::test]
    async fn subscribe_updates_does_not_replay_past_updates() {
        // Newcomer subscribes AFTER an update was applied — must NOT
        // see it. Yjs sync-step-1 is how a late joiner catches up on
        // history, NOT the broadcast channel; keeping these disjoint
        // avoids double-application on reconnect.
        let svc = service_with_seed("", CollabLimits::default());
        let file_id = Uuid::new_v4();
        let session = svc.attach_file(Uuid::nil(), file_id).await.unwrap();

        let client = Doc::new();
        {
            let text = client.get_or_insert_text(ROOT_TEXT_NAME);
            let mut txn = client.transact_mut();
            text.insert(&mut txn, 0, "before");
        }
        let past_bytes = client
            .transact()
            .encode_state_as_update_v1(&StateVector::default());
        session.apply_update(Uuid::nil(), past_bytes).await.unwrap();

        // NOW subscribe. Should time out on the past update.
        let mut rx = session.subscribe_updates().await.unwrap();
        match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
            Err(_elapsed) => {} // expected: no replay
            Ok(other) => panic!("expected timeout, got {other:?}"),
        }
    }

    // ── Per-frame AuthZ gate (C2 slice 4) ──────────────────────────────
    //
    // These tests assert on `handle_binary_frame`'s behaviour with a
    // stub gate that permits/denies per Permission. Real end-to-end
    // enforcement (across the WS wire) lives in the api-test S19; the
    // service-level tests here guard the dispatch and error shape.

    #[tokio::test]
    async fn update_frame_denied_when_gate_rejects_update() {
        // ReadOnly gate ⇒ Permission::Update is refused → UPDATE frame
        // must return AuthzDenied and NOT reach the actor's Doc.
        let svc = service_with_seed_and_gate("", CollabLimits::default(), Arc::new(ReadOnly));
        let file_id = Uuid::new_v4();

        // Build a legit Yjs update — the point is that the gate rejects
        // BEFORE decode/apply, so even a well-formed update is refused.
        let client = Doc::new();
        {
            let text = client.get_or_insert_text(ROOT_TEXT_NAME);
            let mut txn = client.transact_mut();
            text.insert(&mut txn, 0, "unauthorized write");
        }
        let update_bytes = client
            .transact()
            .encode_state_as_update_v1(&StateVector::default());

        let frame = BinaryFrame {
            kind: kind::UPDATE,
            file_id,
            payload: update_bytes,
        };
        match svc.handle_binary_frame(Uuid::nil(), frame).await {
            Err(CollabError::AuthzDenied {
                permission: "update",
                file_id: fid,
            }) if fid == file_id => {}
            other => {
                panic!("expected AuthzDenied{{permission:\"update\", file_id}}, got {other:?}")
            }
        }

        // Doc must be untouched — an authorized caller reading via SYNC
        // sees the seed (empty) text, not "unauthorized write".
        let session = svc.attach_file(Uuid::nil(), file_id).await.unwrap();
        assert_eq!(session.get_text().await.unwrap(), "");
    }

    #[tokio::test]
    async fn sync_frame_denied_when_gate_rejects_read() {
        // DenyAll gate ⇒ even Read is refused. A caller with a valid
        // socket but no Read grant must NOT be able to pull doc content
        // via SYNC (defense-in-depth: subscribe-time Read gate is the
        // primary check, this closes the "binary bypass" gap).
        let svc = service_with_seed_and_gate(
            "secret content",
            CollabLimits::default(),
            Arc::new(DenyAll),
        );
        let file_id = Uuid::new_v4();

        let empty_sv = StateVector::default().encode_v1();
        let frame = BinaryFrame {
            kind: kind::SYNC,
            file_id,
            payload: empty_sv,
        };
        match svc.handle_binary_frame(Uuid::nil(), frame).await {
            Err(CollabError::AuthzDenied {
                permission: "read",
                file_id: fid,
            }) if fid == file_id => {}
            other => panic!("expected AuthzDenied{{permission:\"read\", file_id}}, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn awareness_frame_bypasses_the_gate() {
        // AWARENESS is presence-only (cursor position, user handle);
        // it neither mutates state nor leaks content. Subscribe-time
        // Read is sufficient — deny-all here proves the per-frame gate
        // isn't consulted on 0x02.
        let svc = service_with_seed_and_gate("", CollabLimits::default(), Arc::new(DenyAll));
        let file_id = Uuid::new_v4();

        let frame = BinaryFrame {
            kind: kind::AWARENESS,
            file_id,
            payload: vec![0xAA, 0xBB, 0xCC],
        };
        let reply = svc.handle_binary_frame(Uuid::nil(), frame).await.unwrap();
        assert_eq!(reply, None, "awareness has no per-socket reply");
    }

    // ── Debounced flush-to-blob (C7 slice 2) ───────────────────────────
    //
    // These tests assert the actor's tick branch fires flushes on the
    // configured schedule. Sub-second thresholds keep the wall-clock
    // cost trivial. The `RecordingWriter` captures every call so we
    // can assert on invocations count, caller_id (§14 provenance),
    // and payload bytes.

    fn service_with_recording_writer(
        seed: &str,
        limits: CollabLimits,
    ) -> (Arc<CollabSessionService>, Arc<RecordingWriter>) {
        let writer = Arc::new(RecordingWriter::default());
        let svc = Arc::new(CollabSessionService::new(
            Arc::new(MemRepo::new()),
            Arc::new(StubReader {
                content: Mutex::new(seed.as_bytes().to_vec()),
                blob_hash: Mutex::new(None),
            }),
            writer.clone(),
            Arc::new(AllowAll),
            limits,
        ));
        (svc, writer)
    }

    // ── External-write conflict (compare-and-swap on flush) ───────────
    //
    // A file can be replaced from outside the collab session — WebDAV
    // PUT, WOPI PutFile, a re-upload. `CollabEvictLifecycleHook` tears
    // the session down when that happens, but it dispatches through
    // `tokio::spawn`: the external write returns while eviction is
    // still queued, and the actor's debounce tick can fire in that
    // gap. The flush would then materialise a CRDT seeded from the
    // PRE-write content straight over the new blob — silent data loss,
    // no error, no audit line.
    //
    // These drive `flush_to_blob` directly rather than through the
    // debouncer, so they assert the guard itself and not a race.

    fn service_with_cas_writer(
        seed: &str,
        baseline: &str,
    ) -> (Arc<CollabSessionService>, Arc<CasWriter>) {
        let writer = Arc::new(CasWriter::new(baseline));
        let svc = Arc::new(CollabSessionService::new(
            Arc::new(MemRepo::new()),
            Arc::new(StubReader {
                content: Mutex::new(seed.as_bytes().to_vec()),
                blob_hash: Mutex::new(Some(baseline.to_string())),
            }),
            writer.clone(),
            Arc::new(AllowAll),
            CollabLimits::default(),
        ));
        (svc, writer)
    }

    /// Make the session dirty so a flush has something to write.
    async fn dirty(session: &CollabSession, caller: Uuid, text: &str) {
        let client = Doc::new();
        {
            let t = client.get_or_insert_text(ROOT_TEXT_NAME);
            let mut txn = client.transact_mut();
            t.insert(&mut txn, 0, text);
        }
        let update = client
            .transact()
            .encode_state_as_update_v1(&StateVector::default());
        session.apply_update(caller, update).await.unwrap();
    }

    #[tokio::test]
    async fn flush_passes_the_baseline_blob_hash_as_precondition() {
        let (svc, writer) = service_with_cas_writer("hello", "blob-v1");
        let file_id = Uuid::new_v4();
        let caller = Uuid::new_v4();
        let session = svc.attach_file(caller, file_id).await.unwrap();
        dirty(&session, caller, " world").await;

        assert!(session.flush_to_blob().await.unwrap());

        // Without a precondition the write is blind and the conflict
        // test below could never fail — this is the positive control.
        let seen = writer.seen_preconditions.lock().unwrap().clone();
        assert_eq!(seen, vec![Some("blob-v1".to_string())]);
        assert_eq!(writer.write_count(), 1);
    }

    #[tokio::test]
    async fn flush_refuses_to_overwrite_a_write_from_outside_the_session() {
        let (svc, writer) = service_with_cas_writer("hello", "blob-v1");
        let file_id = Uuid::new_v4();
        let caller = Uuid::new_v4();
        let session = svc.attach_file(caller, file_id).await.unwrap();
        dirty(&session, caller, " world").await;

        // Somebody PUTs the file while the session holds unflushed text.
        writer.external_write("blob-from-webdav");

        let err = session.flush_to_blob().await.unwrap_err();
        assert!(
            matches!(&err, CollabError::Storage(e) if e.kind == ErrorKind::PreconditionFailed),
            "expected a precondition failure, got {err:?}",
        );

        // The point of the whole exercise: their bytes are still there.
        assert_eq!(writer.write_count(), 0, "nothing may be written");
        assert_eq!(writer.current_hash().as_deref(), Some("blob-from-webdav"));
    }

    #[tokio::test]
    async fn flush_chains_the_precondition_across_successive_writes() {
        let (svc, writer) = service_with_cas_writer("hello", "blob-v1");
        let file_id = Uuid::new_v4();
        let caller = Uuid::new_v4();
        let session = svc.attach_file(caller, file_id).await.unwrap();

        dirty(&session, caller, "one").await;
        assert!(session.flush_to_blob().await.unwrap());
        let after_first = writer.current_hash().unwrap();

        dirty(&session, caller, "two").await;
        assert!(
            session.flush_to_blob().await.unwrap(),
            "second flush must succeed — the actor's own write is not a conflict",
        );

        // The second precondition must be the hash the FIRST write
        // returned. Keeping the stale baseline would make every flush
        // after the first fail against the session's own work.
        let seen = writer.seen_preconditions.lock().unwrap().clone();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[1], Some(after_first));
    }

    #[tokio::test]
    async fn flush_writes_blind_when_no_baseline_could_be_established() {
        // `current_blob_hash` returning None models a file row with no
        // blob yet. Refusing to save there would be worse than the
        // race we are guarding: the user's text would have nowhere to
        // go at all.
        let writer = Arc::new(CasWriter::new("blob-v1"));
        let svc = Arc::new(CollabSessionService::new(
            Arc::new(MemRepo::new()),
            Arc::new(StubReader {
                content: Mutex::new(b"hello".to_vec()),
                blob_hash: Mutex::new(None),
            }),
            writer.clone(),
            Arc::new(AllowAll),
            CollabLimits::default(),
        ));
        let file_id = Uuid::new_v4();
        let caller = Uuid::new_v4();
        let session = svc.attach_file(caller, file_id).await.unwrap();
        dirty(&session, caller, " world").await;

        assert!(session.flush_to_blob().await.unwrap());
        assert_eq!(
            writer.seen_preconditions.lock().unwrap().clone(),
            vec![None]
        );
    }

    #[tokio::test]
    async fn conflicted_flush_evicts_with_a_reason_that_says_work_is_unsaved() {
        let limits = CollabLimits {
            debounce_idle: Duration::from_millis(50),
            debounce_max: Duration::from_millis(500),
            debounce_tick: Duration::from_millis(20),
            ..CollabLimits::default()
        };
        let writer = Arc::new(CasWriter::new("blob-v1"));
        let svc = Arc::new(CollabSessionService::new(
            Arc::new(MemRepo::new()),
            Arc::new(StubReader {
                content: Mutex::new(b"hello".to_vec()),
                blob_hash: Mutex::new(Some("blob-v1".to_string())),
            }),
            writer.clone(),
            Arc::new(AllowAll),
            limits,
        ));
        let file_id = Uuid::new_v4();
        let caller = Uuid::new_v4();
        let session = svc.attach_file(caller, file_id).await.unwrap();
        let mut rx = session.subscribe_updates().await.unwrap();

        dirty(&session, caller, " world").await;
        writer.external_write("blob-from-webdav");

        // Drain until the control frame: the dirtying update is
        // broadcast first and is not what this test is about.
        let reason = loop {
            let (kind, payload) = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .expect("a control frame within 2s")
                .expect("channel open until the actor drops");
            if kind == INTERNAL_KIND_EVICTED {
                break String::from_utf8(payload).unwrap();
            }
        };

        // Distinct from the lifecycle hook's plain `external_write`:
        // this one promises there IS unflushed text, which is what
        // tells the client to preserve its buffer as a conflict copy
        // instead of silently reloading.
        assert_eq!(reason, "external_write_conflict");
        assert_eq!(writer.write_count(), 0);
        assert_eq!(writer.current_hash().as_deref(), Some("blob-from-webdav"));
    }

    #[tokio::test]
    async fn debouncer_fires_after_idle_threshold() {
        // 50 ms idle, 500 ms max, 20 ms tick — the idle path should
        // fire first, ~50 ms after the last apply.
        let limits = CollabLimits {
            debounce_idle: Duration::from_millis(50),
            debounce_max: Duration::from_millis(500),
            debounce_tick: Duration::from_millis(20),
            ..CollabLimits::default()
        };
        let (svc, writer) = service_with_recording_writer("", limits);
        let file_id = Uuid::new_v4();
        let caller = Uuid::new_v4();
        let session = svc.attach_file(caller, file_id).await.unwrap();

        // Apply one update.
        let client = Doc::new();
        {
            let text = client.get_or_insert_text(ROOT_TEXT_NAME);
            let mut txn = client.transact_mut();
            text.insert(&mut txn, 0, "flush me");
        }
        let update = client
            .transact()
            .encode_state_as_update_v1(&StateVector::default());
        session.apply_update(caller, update).await.unwrap();

        // Wait past idle threshold + one tick — give the actor time to
        // observe and fire the flush.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Recording writer received exactly one write with the caller
        // and the UTF-8 text bytes.
        let writes = writer.writes.lock().unwrap();
        assert_eq!(writes.len(), 1, "expected one flush, got {}", writes.len());
        assert_eq!(writes[0].0, caller, "§14 caller_id must match last writer");
        assert_eq!(writes[0].1, file_id);
        assert_eq!(&writes[0].2, b"flush me");
    }

    #[tokio::test]
    async fn debouncer_short_circuits_on_unchanged_content() {
        // Two flushes back-to-back with no new updates between: the
        // second must be a no-op via `last_flushed_content_hash`.
        let limits = CollabLimits {
            debounce_idle: Duration::from_millis(30),
            debounce_max: Duration::from_millis(500),
            debounce_tick: Duration::from_millis(10),
            ..CollabLimits::default()
        };
        let (svc, writer) = service_with_recording_writer("", limits);
        let file_id = Uuid::new_v4();
        let caller = Uuid::new_v4();
        let session = svc.attach_file(caller, file_id).await.unwrap();

        let client = Doc::new();
        {
            let text = client.get_or_insert_text(ROOT_TEXT_NAME);
            let mut txn = client.transact_mut();
            text.insert(&mut txn, 0, "once");
        }
        let update = client
            .transact()
            .encode_state_as_update_v1(&StateVector::default());
        session.apply_update(caller, update).await.unwrap();

        // Explicit flush + wait for the tick's idempotent flush.
        let first = session.flush_to_blob().await.unwrap();
        assert!(first, "first flush wrote");
        tokio::time::sleep(Duration::from_millis(150)).await;
        // Explicit second flush after settling.
        let second = session.flush_to_blob().await.unwrap();
        assert!(!second, "second flush was a no-op (hash unchanged)");

        let writes = writer.writes.lock().unwrap();
        assert_eq!(
            writes.len(),
            1,
            "only one write reached the writer (short-circuit worked)"
        );
    }

    #[tokio::test]
    async fn debouncer_fires_by_max_deadline_under_continuous_edits() {
        // Constant edits every 20 ms: idle threshold never trips
        // (edits keep pushing last_dirty_at forward). The max
        // deadline MUST fire on its own — this is what bounds
        // staleness on a never-quiet doc.
        let limits = CollabLimits {
            debounce_idle: Duration::from_secs(60), // idle path can't win
            debounce_max: Duration::from_millis(150), // max path wins
            debounce_tick: Duration::from_millis(20),
            ..CollabLimits::default()
        };
        let (svc, writer) = service_with_recording_writer("", limits);
        let file_id = Uuid::new_v4();
        let caller = Uuid::new_v4();
        let session = svc.attach_file(caller, file_id).await.unwrap();

        // Fire small deltas from ONE Doc so state vectors line up
        // across the sequence (see snapshot_compaction_triggers_at_threshold
        // for the pattern's rationale).
        let client = Doc::new();
        let mut server_sv = StateVector::default();
        for ch in ['a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j'] {
            {
                let text = client.get_or_insert_text(ROOT_TEXT_NAME);
                let mut txn = client.transact_mut();
                let cur_len = text.get_string(&txn).chars().count() as u32;
                text.insert(&mut txn, cur_len, &ch.to_string());
            }
            let delta = client.transact().encode_state_as_update_v1(&server_sv);
            session.apply_update(caller, delta).await.unwrap();
            server_sv = client.transact().state_vector();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // 10 edits × 20 ms = 200 ms of continuous editing. Max
        // deadline (150 ms) should have fired at least once mid-run;
        // wait a couple more ticks to make sure the tick branch saw
        // its `flush_due` return true.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let writes = writer.writes.lock().unwrap();
        assert!(
            !writes.is_empty(),
            "max-deadline flush never fired under continuous edits"
        );
    }

    // ── Awareness fan-out (C6) ─────────────────────────────────────────
    //
    // AWARENESS frames must ride the shared broadcast channel so peer
    // WS forwarders push them to their sockets. They must NOT touch
    // the CRDT (`get_text()` unchanged), must NOT arm the debouncer
    // (writer stays quiet), and must NOT require Update permission
    // (subscribe-time Read is sufficient for presence).

    #[tokio::test]
    async fn awareness_broadcasts_to_subscribers_without_touching_the_doc() {
        let (svc, writer) = service_with_recording_writer("", CollabLimits::default());
        let file_id = Uuid::new_v4();
        let session = svc.attach_file(Uuid::nil(), file_id).await.unwrap();
        let mut rx = session.subscribe_updates().await.unwrap();

        // Opaque presence bytes — the actor doesn't decode.
        let presence = vec![0xAA, 0xBB, 0xCC, 0xDD];
        session.apply_awareness(presence.clone()).await.unwrap();

        // Receiver sees the same bytes, tagged as AWARENESS (0x02).
        let received = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("broadcast delivery within 1s")
            .expect("no lag / no close");
        assert_eq!(
            received.0,
            crate::application::services::collab_wire::kind::AWARENESS
        );
        assert_eq!(received.1, presence);

        // Doc text unchanged — awareness is presence, not content.
        assert_eq!(session.get_text().await.unwrap(), "");

        // No flush fired — awareness must not arm the debouncer.
        // (No wait here; the debouncer's tick interval is 1s by
        // default and the test doesn't sleep past it.)
        assert!(writer.writes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn awareness_frame_via_binary_router_broadcasts() {
        // End-to-end through `handle_binary_frame` — the WS handler's
        // real entry point — so we assert the AWARENESS branch calls
        // `apply_awareness` and the broadcast lands on peer receivers.
        // DenyAll gate proves that awareness is NOT gated on Update
        // (only subscribe-time Read is, and we don't exercise it here
        // because the actor is already attached).
        let svc = service_with_seed_and_gate("", CollabLimits::default(), Arc::new(DenyAll));
        let file_id = Uuid::new_v4();
        let session = svc.attach_file(Uuid::nil(), file_id).await.unwrap();
        let mut rx = session.subscribe_updates().await.unwrap();

        let presence = vec![0x11, 0x22, 0x33];
        let frame = BinaryFrame {
            kind: kind::AWARENESS,
            file_id,
            payload: presence.clone(),
        };
        let reply = svc.handle_binary_frame(Uuid::nil(), frame).await.unwrap();
        assert_eq!(reply, None, "awareness has no per-socket reply");

        let received = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("broadcast delivery within 1s")
            .expect("no lag / no close");
        assert_eq!(
            received.0,
            crate::application::services::collab_wire::kind::AWARENESS
        );
        assert_eq!(received.1, presence);
    }

    // Eviction control message: `evict_sessions_for_file` puts one
    // `(INTERNAL_KIND_EVICTED, reason_bytes)` tuple on the outbox
    // BEFORE the actor drops (and the channel closes with
    // `RecvError::Closed`). This wire ordering is what the WS forwarder
    // depends on to translate the eviction into an `rt.revoked` text
    // frame — a broken order would look silent to the client.
    #[tokio::test]
    async fn evict_sessions_for_file_broadcasts_control_then_closes() {
        let (svc, _writer) = service_with_recording_writer("", CollabLimits::default());
        let file_id = Uuid::new_v4();
        let session = svc.attach_file(Uuid::nil(), file_id).await.unwrap();
        let mut rx = session.subscribe_updates().await.unwrap();

        svc.evict_sessions_for_file(file_id, "resource_deleted")
            .await;

        // First recv: the eviction control tuple.
        let received = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("eviction delivery within 1s")
            .expect("no lag / no close");
        assert_eq!(received.0, INTERNAL_KIND_EVICTED);
        assert_eq!(&received.1[..], b"resource_deleted");

        // Second recv: the channel closes as the actor drops.
        // `RecvError::Closed` is the wire signal for "actor gone".
        let closed = tokio::time::timeout(Duration::from_secs(1), rx.recv()).await;
        assert!(
            matches!(
                closed,
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed))
            ),
            "expected RecvError::Closed after eviction, got {closed:?}",
        );

        // Registry no longer knows about this file — a fresh attach
        // spawns a new actor with a fresh outbox, i.e. clients that
        // re-subscribe post-eviction see the current on-disk state
        // (or a denial, if the file was concurrently deleted).
        assert_eq!(svc.live_session_count(), 0);
    }

    #[tokio::test]
    async fn evict_sessions_for_file_is_a_no_op_when_no_session_attached() {
        // A speculative evict on a file with no live actor must be
        // safe — the delete path calls it unconditionally rather than
        // pre-checking, so this covers the common "delete of a file
        // no one had open" case.
        let (svc, _writer) = service_with_recording_writer("", CollabLimits::default());
        let file_id = Uuid::new_v4();
        svc.evict_sessions_for_file(file_id, "resource_deleted")
            .await;
        assert_eq!(svc.live_session_count(), 0);
    }

    // Doc-size cap enforcement: an UPDATE that would push the actor
    // over `max_doc_bytes` MUST be refused with `DocTooLarge` BEFORE
    // it enters the CRDT — Yjs has no rollback in `yrs`, so an
    // apply-then-refuse would leave the doc permanently over the cap.
    #[tokio::test]
    async fn apply_update_rejects_when_over_max_doc_bytes() {
        // Cap so tight that even the seeded fresh doc fits with room
        // for a modest first update, but a payload of a few hundred
        // bytes trips the check. Yjs update encoding has ~30 B of
        // framing overhead per insert, so a 500-char string encodes
        // to ~530-ish bytes and blows past a 200 B cap.
        let limits = CollabLimits {
            max_doc_bytes: 200,
            ..CollabLimits::default()
        };
        let svc = service_with_seed("", limits);
        let file_id = Uuid::new_v4();
        let session = svc.attach_file(Uuid::nil(), file_id).await.unwrap();

        // Build a big update on a client Doc so the wire delta is
        // real Yjs-encoded bytes, not synthetic. 500 chars is
        // overkill vs 200 B cap.
        let client = Doc::new();
        {
            let text = client.get_or_insert_text(ROOT_TEXT_NAME);
            let mut txn = client.transact_mut();
            text.insert(&mut txn, 0, &"x".repeat(500));
        }
        let big_delta = client
            .transact()
            .encode_state_as_update_v1(&StateVector::default());
        assert!(
            big_delta.len() > 200,
            "test wants an update bigger than the cap; got {} B",
            big_delta.len()
        );

        let err = session
            .apply_update(Uuid::nil(), big_delta)
            .await
            .expect_err("over-cap update must be rejected");
        match err {
            CollabError::DocTooLarge {
                file_id: reported_id,
                limit_bytes,
            } => {
                assert_eq!(reported_id, file_id);
                assert_eq!(limit_bytes, 200);
            }
            other => panic!("expected DocTooLarge, got {other:?}"),
        }

        // Server doc must be unchanged — the reject-before-apply
        // ordering is the whole safety property. If the update had
        // slipped through, `get_text` would return 500 x's.
        assert_eq!(session.get_text().await.unwrap(), "");
    }

    #[tokio::test]
    async fn apply_update_within_cap_succeeds() {
        // Mirror of the previous test: same tight cap, but the update
        // is small enough that `doc_bytes + update.len()` stays under
        // the cap. Sanity-checks that the check doesn't over-reject
        // on legitimately-sized edits at boot.
        let limits = CollabLimits {
            max_doc_bytes: 200,
            ..CollabLimits::default()
        };
        let svc = service_with_seed("", limits);
        let file_id = Uuid::new_v4();
        let session = svc.attach_file(Uuid::nil(), file_id).await.unwrap();

        let client = Doc::new();
        {
            let text = client.get_or_insert_text(ROOT_TEXT_NAME);
            let mut txn = client.transact_mut();
            text.insert(&mut txn, 0, "hi");
        }
        let small_delta = client
            .transact()
            .encode_state_as_update_v1(&StateVector::default());
        session
            .apply_update(Uuid::nil(), small_delta)
            .await
            .expect("under-cap update must apply");
        assert_eq!(session.get_text().await.unwrap(), "hi");
    }
}
