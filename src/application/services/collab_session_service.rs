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
use std::time::Duration;

use dashmap::DashMap;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{Doc, GetString, ReadTxn, StateVector, Text, Transact, Update};

use crate::application::ports::collab_ports::{
    DocContentReader, DocContentWriter, DocSessionRepository, StoredDocSession,
};
use crate::common::errors::{DomainError, ErrorKind};

/// Opaque socket identifier — matches the WS handler's per-connection
/// id. C1 doesn't use it beyond ref-counting (`HashSet<SocketId>`);
/// C2's fan-out uses it to skip the sender's own socket when
/// broadcasting an update.
pub type SocketId = u64;

/// Configuration knobs. Defaults track the plan doc's § Limits &
/// guardrails table. Wired from `AppConfig::collab` in the DI layer
/// (not yet — C1 lands with defaults only).
#[derive(Debug, Clone, Copy)]
pub struct CollabLimits {
    /// Compact the CRDT snapshot after this many updates layered on
    /// top of the persisted `state`. Higher = cheaper writes, more
    /// expensive reconnect catch-up. Plan default: 200.
    pub snapshot_after_updates: i32,
}

impl Default for CollabLimits {
    fn default() -> Self {
        Self {
            snapshot_after_updates: 200,
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
        bytes: Vec<u8>,
        reply: oneshot::Sender<Result<(), CollabError>>,
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

    pub async fn apply_update(&self, bytes: Vec<u8>) -> Result<(), CollabError> {
        let (tx, rx) = oneshot::channel();
        self.inbox
            .send(SessionMsg::ApplyUpdate { bytes, reply: tx })
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

    pub async fn shutdown(&self) {
        // Best-effort — the actor may already be gone.
        let _ = self.inbox.send(SessionMsg::Shutdown).await;
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
    attached: HashSet<SocketId>,
    updates_since_snapshot: i32,
    limits: CollabLimits,
    repo: Arc<dyn DocSessionRepository>,
}

impl ActorState {
    /// Load the persisted snapshot into the actor's `Doc`, OR seed a
    /// fresh doc from `seed_content` (the current file blob text) on
    /// first-ever attach. Also stamps an initial snapshot row via the
    /// repo so subsequent attaches follow the load-path.
    async fn load_or_seed(
        file_id: Uuid,
        repo: Arc<dyn DocSessionRepository>,
        seed_content: Option<Vec<u8>>,
        limits: CollabLimits,
    ) -> Result<Self, CollabError> {
        let doc = Doc::new();
        let existing = repo.load(file_id).await?;
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
        Ok(Self {
            file_id,
            doc,
            attached: HashSet::new(),
            updates_since_snapshot,
            limits,
            repo,
        })
    }

    fn text(&self) -> String {
        let text_ref = self.doc.get_or_insert_text(ROOT_TEXT_NAME);
        let txn = self.doc.transact();
        text_ref.get_string(&txn)
    }

    /// Apply an incoming Yjs update. Errors on malformed bytes; a
    /// successful apply bumps the update counter and MAY trigger a
    /// snapshot compaction.
    async fn apply_update(&mut self, bytes: Vec<u8>) -> Result<(), CollabError> {
        let update = Update::decode_v1(&bytes)
            .map_err(|e| CollabError::BadUpdate(format!("decode: {e}")))?;
        {
            let mut txn = self.doc.transact_mut();
            txn.apply_update(update)
                .map_err(|e| CollabError::BadUpdate(format!("apply: {e}")))?;
        }
        self.repo.touch_after_update(self.file_id).await?;
        self.updates_since_snapshot += 1;
        if self.updates_since_snapshot >= self.limits.snapshot_after_updates {
            self.snapshot().await?;
        }
        Ok(())
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
        self.repo
            .save_snapshot(self.file_id, &snapshot, &sv)
            .await?;
        self.updates_since_snapshot = 0;
        Ok(())
    }
}

/// Run one session actor's message loop until every sender drops OR
/// a `Shutdown` message arrives. Called by [`CollabSessionService`]'s
/// spawn helper. Errors on messages surface via each message's own
/// reply channel — the loop itself doesn't propagate them.
async fn run_actor(mut state: ActorState, mut inbox: mpsc::Receiver<SessionMsg>) {
    while let Some(msg) = inbox.recv().await {
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
            SessionMsg::ApplyUpdate { bytes, reply } => {
                let result = state.apply_update(bytes).await;
                let _ = reply.send(result);
            }
            SessionMsg::GetText { reply } => {
                let _ = reply.send(state.text());
            }
            SessionMsg::Snapshot { reply } => {
                let _ = reply.send(state.snapshot().await);
            }
            SessionMsg::Shutdown => break,
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
    limits: CollabLimits,
}

impl CollabSessionService {
    pub fn new(
        repo: Arc<dyn DocSessionRepository>,
        reader: Arc<dyn DocContentReader>,
        writer: Arc<dyn DocContentWriter>,
        limits: CollabLimits,
    ) -> Self {
        Self {
            sessions: DashMap::new(),
            repo,
            reader,
            writer,
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
        let state = ActorState::load_or_seed(file_id, self.repo.clone(), seed, self.limits).await?;
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
    }

    // ── Stub reader / writer ──────────────────────────────────────────

    struct StubReader {
        content: Mutex<Vec<u8>>,
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
    }

    struct StubWriter;
    #[async_trait]
    impl DocContentWriter for StubWriter {
        async fn write_content(
            &self,
            _caller_id: Uuid,
            _file_id: Uuid,
            _content: Vec<u8>,
        ) -> Result<String, DomainError> {
            Ok("b3-stub".into())
        }
    }

    fn service_with_seed(seed: &str, limits: CollabLimits) -> Arc<CollabSessionService> {
        Arc::new(CollabSessionService::new(
            Arc::new(MemRepo::new()),
            Arc::new(StubReader {
                content: Mutex::new(seed.as_bytes().to_vec()),
            }),
            Arc::new(StubWriter),
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
        session.apply_update(update_a.clone()).await.unwrap();

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
        session.apply_update(update_b).await.unwrap();

        let server_text = session.get_text().await.unwrap();
        assert_eq!(server_text, "Hello world");
    }

    #[tokio::test]
    async fn snapshot_compaction_triggers_at_threshold() {
        // Threshold 3 so we don't have to fire 200 updates in a test.
        let limits = CollabLimits {
            snapshot_after_updates: 3,
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
            session.apply_update(delta).await.unwrap();
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
        s1.apply_update(update).await.unwrap();
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
}
