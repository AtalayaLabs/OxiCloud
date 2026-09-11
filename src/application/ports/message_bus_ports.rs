//! Message-bus port — the seam every service publishes through and every WS
//! session subscribes on.
//!
//! # Design (see `docs/plan/message-bus.md`)
//!
//! - [`MessageBus`] is the **local-facing** trait: services publish, the WS
//!   handler subscribes. It never involves the network.
//! - [`BusReplicator`] is the OPTIONAL seam that mirrors local publishes to
//!   and from a broker (pg `LISTEN/NOTIFY`, RabbitMQ, NATS). Callers see only
//!   [`MessageBus`]; a real replicator plugs into the in-process impl without
//!   touching consumers. Day-1 impl is [`NoopReplicator`].
//!
//! # MVP scope
//!
//! Ships the smallest slice that lets the smoke test verify a folder
//! subscription receives file/folder-created events and rejects subscribes
//! to folders the caller can't `Read`:
//!
//! - Topics: [`Topic::Folder`] and [`Topic::UserAuthz`]
//! - Events: [`MessageBusEvent::FileCreated`], [`MessageBusEvent::FileRenamed`],
//!   [`MessageBusEvent::FileMoved`], [`MessageBusEvent::FileDeleted`],
//!   [`MessageBusEvent::FolderCreated`], [`MessageBusEvent::FolderRenamed`],
//!   [`MessageBusEvent::FolderMoved`], [`MessageBusEvent::FolderDeleted`]
//!
//! Adding a variant is a one-line change plus a match arm in `to_wire_key` /
//! `parse` / `required_perm`. Other topics (`file:{id}`, `job:{id}`,
//! `collab:{id}`, `user:{u}:notifications`, …) land with their producers in
//! Phase-A follow-ups.
//!
//! # Wire protocol
//!
//! JSON-RPC 2.0 for control + events (text frames), Yjs sync protocol for
//! CRDT (binary frames). This module owns the JSON-RPC error-code
//! vocabulary; see [`error_code`].

use std::pin::Pin;
use std::sync::Arc;

use futures::Stream;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use uuid::Uuid;

use crate::common::errors::DomainError;

// ════════════════════════════════════════════════════════════════════════════
// Topic — a typed key on the bus
// ════════════════════════════════════════════════════════════════════════════

/// A topic on the message bus. Typed enum, not a string — prevents typos
/// and gives exhaustive matching in the AuthZ dispatch and the wire encoder.
///
/// Encodes to a stable dotted wire key that maps naturally onto RabbitMQ
/// topic-exchange routing keys or NATS subjects when the [`BusReplicator`]
/// seam is filled in later.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Topic {
    /// A folder's mutation stream — file/subfolder created/deleted/renamed/
    /// moved in or out. Consumed by the folder view for live refresh.
    Folder(Uuid),

    /// A user's private authz-change channel. The WS handler will auto-
    /// subscribe the caller and evict stale subs when its events fire once
    /// the eviction wiring lands (Phase-A follow-up).
    UserAuthz(Uuid),

    /// A named background job's run lifecycle — start / progress /
    /// end. Consumed by the admin job dashboard so operators who
    /// trigger a long-running job (backend migration, thumb import…)
    /// can navigate to other admin pages without losing progress
    /// visibility. AuthZ: **admin-only** (Class 3 role-scoped).
    /// Non-admins get `topic_forbidden` — indistinguishable on the
    /// wire from an unknown topic. Job names are stable
    /// scheduler-registered strings (e.g. `backend_migration`,
    /// `thumb_derived_import`); the topic string is `job:<name>`.
    Job(String),
}

impl Topic {
    /// Stable dotted wire form used by the JSON-RPC control frames and any
    /// future broker routing keys. Reverse of [`Topic::parse`].
    pub fn to_wire_key(&self) -> String {
        match self {
            Topic::Folder(id) => format!("folder:{id}"),
            Topic::UserAuthz(id) => format!("user:{id}:authz"),
            Topic::Job(name) => format!("job:{name}"),
        }
    }

    /// Parse a wire-form topic string. Rejects unknown shapes with a stable
    /// error kind so the WS handler can respond with a JSON-RPC error object
    /// (`topic_forbidden` for unknown topic shapes, `no_read` for known
    /// shapes the caller can't reach — the latter after the AuthZ check).
    pub fn parse(s: &str) -> Result<Self, ParseTopicErr> {
        if let Some(rest) = s.strip_prefix("folder:") {
            let id = Uuid::parse_str(rest).map_err(|_| ParseTopicErr::BadUuid)?;
            return Ok(Topic::Folder(id));
        }
        if let Some(rest) = s.strip_prefix("user:")
            && let Some((id_str, "authz")) = rest.rsplit_once(':')
        {
            let id = Uuid::parse_str(id_str).map_err(|_| ParseTopicErr::BadUuid)?;
            return Ok(Topic::UserAuthz(id));
        }
        if let Some(name) = s.strip_prefix("job:") {
            // Job names are scheduler-registered short slugs — see
            // `infrastructure/scheduler/registry.rs`. Validate here
            // only that the name is non-empty and consists of
            // `[a-z0-9_-]` chars — reject anything else as
            // `Unknown` (indistinguishable to the caller from a
            // topic shape we've never heard of).
            if !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
            {
                return Ok(Topic::Job(name.to_string()));
            }
            return Err(ParseTopicErr::Unknown);
        }
        Err(ParseTopicErr::Unknown)
    }

    /// Which permission check the WS handler must run before allowing a
    /// subscribe. Three classes per plan (see
    /// `docs/plan/message-bus.md § AuthZ model`):
    ///
    /// - Resource-scoped: default `Read` on the resource (Phase-B adds
    ///   `Share`/`Comment` for the stricter topics).
    /// - Identity-scoped: `caller_id == subject_uuid`. No admin bypass.
    /// - Role-scoped / bespoke: not represented in this MVP.
    pub fn required_perm(&self) -> AuthzCheck {
        match self {
            Topic::Folder(id) => AuthzCheck::ResourceRead {
                resource: BusResource::Folder(*id),
            },
            Topic::UserAuthz(id) => AuthzCheck::IdentityMatch { user_id: *id },
            Topic::Job(_) => AuthzCheck::RoleAdmin,
        }
    }
}

/// Parse failure for a wire-form topic string. Kept small — the WS handler
/// maps every variant to `topic_forbidden` on the wire (both a bad UUID and
/// an unknown shape are indistinguishable from the caller's perspective).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseTopicErr {
    /// The prefix was recognized but the UUID inside didn't parse.
    BadUuid,
    /// The topic string didn't match any known shape (typo, or a topic
    /// that isn't in this MVP).
    Unknown,
}

// ════════════════════════════════════════════════════════════════════════════
// AuthzCheck — the gate class the WS handler dispatches on
// ════════════════════════════════════════════════════════════════════════════

/// Resource kinds the bus knows how to gate on. Deliberately a small closed
/// enum, not the full `domain::authorization::Resource` — the bus does not
/// need every resource type in the domain, and keeping this separate avoids
/// dragging domain-shaped churn into the port.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BusResource {
    Folder(Uuid),
    // File(Uuid), Drive(Uuid), Calendar(Uuid), AddressBook(Uuid) land with
    // their topic variants.
}

/// The check the WS handler must run at subscribe time. Split into the three
/// classes described in `docs/plan/message-bus.md § AuthZ model`, so a new
/// topic variant with a new gate shape is a compile error at the dispatch
/// site rather than a runtime "unhandled" bug.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthzCheck {
    /// Class 1 — Resource-scoped, default gate is Read on the resource.
    /// Extend to `ResourceShare`/`ResourceComment` when the Phase-B topics
    /// (`file:{id}:shares`, `file:{id}:comments`) land.
    ResourceRead { resource: BusResource },

    /// Class 2 — Identity-scoped. `caller_id` must equal `user_id`.
    /// No admin bypass — privacy is a hard rule.
    IdentityMatch { user_id: Uuid },

    /// Class 3 — Role-scoped. Caller must hold the admin role. Used
    /// by `Topic::Job(_)` today; future `admin:*` topics land here.
    /// Non-admin subscriber gets `topic_forbidden` on the wire —
    /// same anti-enum shape as unknown-topic denial.
    RoleAdmin,
}

// ════════════════════════════════════════════════════════════════════════════
// MessageBusEvent — the payload
// ════════════════════════════════════════════════════════════════════════════

/// A fact that has just become true. Emitted by services AFTER commit,
/// never inside a DB transaction — a rollback would otherwise fan out a
/// lie.
///
/// Payloads are **thin facts** (ids + actor + verb): the client refetches
/// details via REST when it needs them. This keeps the AuthZ surface small
/// (thin payloads can't leak fields the caller couldn't already read via
/// REST) and keeps events well under the ~8 KB pg NOTIFY cap when the
/// `PgListenReplicator` seam is filled in later.
///
/// Wire form uses `#[serde(tag = "event", rename_all = "snake_case")]`;
/// discriminator strings are the JSON-RPC notification `event` field. New
/// denial cause / new event = new variant, never repurpose an existing one,
/// per project convention.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum MessageBusEvent {
    /// A file was created inside `parent_id`.
    FileCreated {
        file_id: Uuid,
        name: String,
        parent_id: Uuid,
        actor: Uuid,
    },
    /// A file was renamed. `parent_id` unchanged — same folder.
    FileRenamed {
        file_id: Uuid,
        old_name: String,
        new_name: String,
        parent_id: Uuid,
        actor: Uuid,
    },
    /// A file was moved between folders. Fanned out on BOTH the source
    /// and destination folder topics — subscribers to either see the
    /// event once. `from` / `to` are the folder UUIDs; a move
    /// involving a drive root would be `Option<Uuid>` in a future
    /// variant, but MVP mutations all address a real folder.
    FileMoved {
        file_id: Uuid,
        name: String,
        from: Uuid,
        to: Uuid,
        actor: Uuid,
    },
    /// A file was deleted (trashed OR permanently removed — the wire
    /// doesn't distinguish, and clients treat both as "disappears from
    /// the folder view"). `parent_id` is the folder the file used to
    /// live in — snapshotted before the delete since the row may be
    /// gone by publish time.
    FileDeleted {
        file_id: Uuid,
        parent_id: Uuid,
        actor: Uuid,
    },
    /// A sub-folder was created inside `parent_id`.
    FolderCreated {
        folder_id: Uuid,
        name: String,
        parent_id: Uuid,
        actor: Uuid,
    },
    /// A folder was renamed. `parent_id` unchanged.
    FolderRenamed {
        folder_id: Uuid,
        old_name: String,
        new_name: String,
        parent_id: Uuid,
        actor: Uuid,
    },
    /// A folder was moved between parents. Fanned out on BOTH source
    /// and destination folder topics.
    FolderMoved {
        folder_id: Uuid,
        name: String,
        from: Uuid,
        to: Uuid,
        actor: Uuid,
    },
    /// A folder was deleted (trashed or permanent — see `FileDeleted`
    /// for the same wire-collapse rationale).
    FolderDeleted {
        folder_id: Uuid,
        parent_id: Uuid,
        actor: Uuid,
    },
    /// A user's authorization changed — publishes on
    /// [`Topic::UserAuthz`]. The WS handler auto-subscribes each
    /// session to its own `user:{caller}:authz` topic; on receipt it
    /// walks the session's active subscriptions and evicts any whose
    /// resource is in `affected_folders`, emitting a `rt.revoked`
    /// notification per evicted topic.
    ///
    /// MVP carries folder UUIDs only (the only resource-scoped topic
    /// that ships in Phase A). When file/drive/calendar topics land,
    /// the payload extends with additional resource classes — see the
    /// plan's Phase-B roadmap.
    AuthzChanged { affected_folders: Vec<Uuid> },

    /// A background job's run started. Published on
    /// [`Topic::Job`]. `started_at` is server wall-clock (RFC 3339
    /// serialised by serde). Admin dashboard's job-list view uses
    /// this to flip a row from "idle" to "running" without a
    /// polling round-trip.
    JobRunStarted {
        name: String,
        started_at: chrono::DateTime<chrono::Utc>,
        actor: Uuid,
    },

    /// A background job made progress. Published at most every
    /// 3 seconds per job (throttled at the publish site — see
    /// scheduler engine). `step` / `total` populate an operator-
    /// facing progress bar; `message` is a one-line free-form
    /// status. All three are optional because different jobs have
    /// different progress semantics (some know the total up front,
    /// some don't; some can render a step count, some just have a
    /// running status message).
    JobRunProgress {
        name: String,
        step: Option<u64>,
        total: Option<u64>,
        message: Option<String>,
    },

    /// A background job's run ended. `success = true` for a normal
    /// completion; `false` for failure / cancelled / paused with
    /// unhandled outcome. `reason` populates the "click for
    /// details" flow on the admin dashboard: the notification (Slice
    /// E) will link to `/admin/jobs/<name>` on the `false` branch,
    /// where the full outcome and paused-run state live.
    ///
    /// Deliberately NOT a rich outcome enum — the admin panel is one
    /// click away and holds the full detail; the bus event just
    /// needs to say "done, ok or not". Adding a new outcome nuance
    /// server-side does NOT churn the wire.
    JobRunEnded {
        name: String,
        success: bool,
        reason: Option<String>,
        ended_at: chrono::DateTime<chrono::Utc>,
    },
}

// ════════════════════════════════════════════════════════════════════════════
// JSON-RPC 2.0 error codes — stable, never repurpose
// ════════════════════════════════════════════════════════════════════════════

/// JSON-RPC 2.0 `error.code` values used on the WS wire. Follows the spec's
/// "server-defined" range `-32000` to `-32099` for our application-defined
/// codes; the standard `-326xx` envelope codes are re-exported here too so
/// the WS handler has one place to reach for.
///
/// See `docs/plan/message-bus.md § JSON-RPC error codes` for the
/// wire-`message`/audit-`reason` mapping.
pub mod error_code {
    /// Resource-scoped topic, caller lacks Read (or resource doesn't exist —
    /// indistinguishable to caller by design). Anti-enum invariant.
    pub const NO_READ: i32 = -32001;

    /// Resource-scoped topic requiring `Share`, caller has Read but not
    /// Share. Applies to `file:{id}:shares` (Phase B).
    pub const NO_SHARE: i32 = -32002;

    /// Resource-scoped topic requiring `Comment` (`file:{id}:comments`
    /// Phase B).
    pub const NO_COMMENT: i32 = -32003;

    /// Identity-scoped mismatch OR unknown/malformed topic. Same wire code
    /// regardless of whether the target user exists — anti-enum.
    pub const TOPIC_FORBIDDEN: i32 = -32004;

    /// Per-connection sub cap hit.
    pub const SUB_LIMIT: i32 = -32005;

    /// Subscribe-frame token bucket exhausted.
    pub const RATE_LIMITED: i32 = -32006;

    /// CRDT edit frame from a caller without `Edit`. Emitted as an
    /// `rt.write_denied` notification (not tied to a request `id`).
    pub const NO_EDIT: i32 = -32007;

    // ────────────────────── JSON-RPC 2.0 standard codes ─────────────────────
    // Re-exported so the WS handler doesn't reach for two constant lists.

    /// Server-side failure the client should retry.
    pub const INTERNAL_ERROR: i32 = -32603;

    /// Malformed JSON-RPC envelope (missing `method`, wrong `jsonrpc`
    /// version).
    pub const INVALID_REQUEST: i32 = -32600;

    /// Method outside the `rt.*` allowlist.
    pub const METHOD_NOT_FOUND: i32 = -32601;

    /// Method known but `params` shape wrong (missing `topic`, unparseable).
    pub const INVALID_PARAMS: i32 = -32602;
}

// ════════════════════════════════════════════════════════════════════════════
// MessageBus — the port
// ════════════════════════════════════════════════════════════════════════════

/// The local-facing message bus. Fire-and-forget publish, stream subscribe.
///
/// `publish` is intentionally synchronous — services must not `await` under
/// a DB transaction (a slow subscriber could hold the tx open) and services
/// should not care whether fan-out is happening in a background task or not.
///
/// `subscribe` returns a `Stream` so the impl can change (broadcast, mpsc,
/// pg listener) without churn at the consumer.
pub trait MessageBus: Send + Sync + 'static {
    /// Fan an event out to every current subscriber of `topic`. Never
    /// blocks; slow subscribers are dropped by the impl (they'll reconnect
    /// and refetch).
    fn publish(&self, topic: &Topic, event: MessageBusEvent);

    /// Subscribe to `topic`. The returned stream yields events until the
    /// subscriber is dropped or the impl kicks it out (e.g. for lagging
    /// too far behind).
    fn subscribe(&self, topic: &Topic) -> BusStream;
}

/// Boxed stream returned by [`MessageBus::subscribe`]. Aliased so
/// consumers don't need to spell out the `Pin<Box<...>>` shape.
pub type BusStream = Pin<Box<dyn Stream<Item = MessageBusEvent> + Send>>;

// ════════════════════════════════════════════════════════════════════════════
// BusReplicator — the multi-instance seam (day-1 noop)
// ════════════════════════════════════════════════════════════════════════════

/// Cross-instance replicator. Sits BESIDE [`MessageBus`], not in front of
/// it — the bus does the local fan-out; the replicator forwards outbound
/// publishes to the broker (pg NOTIFY, RabbitMQ, NATS) and injects inbound
/// broker messages back into the local bus.
///
/// V1 ships [`NoopReplicator`]. The trait is declared today so wiring the
/// day the second impl arrives is drop-in.
#[async_trait::async_trait]
pub trait BusReplicator: Send + Sync + 'static {
    /// Called by the local bus for every publish. Fire-and-forget — must not
    /// block or await; forwarding to the broker happens on a background task
    /// owned by the impl.
    fn on_local_publish(&self, topic: &Topic, event: &MessageBusEvent);

    /// Long-running consumer task: reads remote messages and re-publishes
    /// locally. Returns when `shutdown` is notified — DI calls
    /// `shutdown.notify_one()` on graceful shutdown.
    ///
    /// **Shutdown semantics:** use `Notify::notify_one` (not
    /// `notify_waiters`) at the signalling site: `notify_one` stores a
    /// permit if no waiter is currently parked, so signal-before-park is
    /// safe. `notify_waiters` silently drops signals sent before parking
    /// and creates a race. This constrains the impl to a single-waiter
    /// shutdown handle; multi-task replicators must spin their own
    /// `CancellationToken`-style fan-out internally.
    async fn run(self: Arc<Self>, shutdown: Arc<Notify>) -> Result<(), DomainError>;
}

/// Day-1 replicator: does nothing. Wired unconditionally so callers hold
/// `Arc<dyn BusReplicator>` uniformly. Swapped for a real impl when
/// multi-instance deployment matters.
#[derive(Default)]
pub struct NoopReplicator;

#[async_trait::async_trait]
impl BusReplicator for NoopReplicator {
    fn on_local_publish(&self, _topic: &Topic, _event: &MessageBusEvent) {
        // Intentionally empty. Local fan-out already happened in the bus.
    }

    async fn run(self: Arc<Self>, shutdown: Arc<Notify>) -> Result<(), DomainError> {
        // Park until shutdown so the DI-managed handle stays alive with the
        // same lifecycle as a future real replicator.
        shutdown.notified().await;
        Ok(())
    }
}

// ════════════════════════════════════════════════════════════════════════════
// Tests
// ════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_topic_roundtrip() {
        let id = Uuid::new_v4();
        let t = Topic::Folder(id);
        let wire = t.to_wire_key();
        assert_eq!(wire, format!("folder:{id}"));
        assert_eq!(Topic::parse(&wire).unwrap(), t);
    }

    #[test]
    fn user_authz_topic_roundtrip() {
        let id = Uuid::new_v4();
        let t = Topic::UserAuthz(id);
        let wire = t.to_wire_key();
        assert_eq!(wire, format!("user:{id}:authz"));
        assert_eq!(Topic::parse(&wire).unwrap(), t);
    }

    #[test]
    fn job_topic_roundtrip() {
        let t = Topic::Job("backend_migration".to_string());
        let wire = t.to_wire_key();
        assert_eq!(wire, "job:backend_migration");
        assert_eq!(Topic::parse(&wire).unwrap(), t);
    }

    #[test]
    fn job_topic_rejects_bad_name_chars() {
        // Job names come from the scheduler registry — a stable
        // `[a-z0-9_-]` alphabet. Anything else is `Unknown` (same
        // wire response as an unrecognised topic shape).
        assert_eq!(Topic::parse("job:"), Err(ParseTopicErr::Unknown));
        assert_eq!(Topic::parse("job:UPPER"), Err(ParseTopicErr::Unknown));
        assert_eq!(Topic::parse("job:with.dot"), Err(ParseTopicErr::Unknown));
        assert_eq!(Topic::parse("job:with space"), Err(ParseTopicErr::Unknown));
    }

    #[test]
    fn required_perm_job_is_role_admin() {
        assert_eq!(
            Topic::Job("thumb_derived_import".to_string()).required_perm(),
            AuthzCheck::RoleAdmin
        );
    }

    #[test]
    fn parse_rejects_bad_uuid() {
        assert_eq!(
            Topic::parse("folder:not-a-uuid"),
            Err(ParseTopicErr::BadUuid)
        );
    }

    #[test]
    fn parse_rejects_unknown_shape() {
        assert_eq!(Topic::parse(""), Err(ParseTopicErr::Unknown));
        assert_eq!(Topic::parse("unknown:x"), Err(ParseTopicErr::Unknown));
        assert_eq!(
            Topic::parse(&format!("user:{}", Uuid::new_v4())),
            Err(ParseTopicErr::Unknown),
            "user:<uuid> without :authz suffix is not a known topic in MVP"
        );
    }

    #[test]
    fn required_perm_folder_is_resource_read() {
        let id = Uuid::new_v4();
        assert_eq!(
            Topic::Folder(id).required_perm(),
            AuthzCheck::ResourceRead {
                resource: BusResource::Folder(id)
            }
        );
    }

    #[test]
    fn required_perm_user_authz_is_identity_match() {
        let id = Uuid::new_v4();
        assert_eq!(
            Topic::UserAuthz(id).required_perm(),
            AuthzCheck::IdentityMatch { user_id: id }
        );
    }

    #[test]
    fn event_serializes_with_snake_case_discriminator() {
        // The `#[serde(tag = "event")]` shape is the WS wire contract for
        // the `rt.event` JSON-RPC notification's `params.event` field. Pin
        // every variant's discriminator with a snapshot so an accidental
        // rename fails the test instead of silently breaking clients —
        // the AsyncAPI spec's `event` enum mirrors these exact strings.
        let cases: &[(MessageBusEvent, &str)] = &[
            (
                MessageBusEvent::FileCreated {
                    file_id: Uuid::nil(),
                    name: "notes.md".into(),
                    parent_id: Uuid::nil(),
                    actor: Uuid::nil(),
                },
                "file_created",
            ),
            (
                MessageBusEvent::FileRenamed {
                    file_id: Uuid::nil(),
                    old_name: "a.md".into(),
                    new_name: "b.md".into(),
                    parent_id: Uuid::nil(),
                    actor: Uuid::nil(),
                },
                "file_renamed",
            ),
            (
                MessageBusEvent::FileMoved {
                    file_id: Uuid::nil(),
                    name: "a.md".into(),
                    from: Uuid::nil(),
                    to: Uuid::nil(),
                    actor: Uuid::nil(),
                },
                "file_moved",
            ),
            (
                MessageBusEvent::FileDeleted {
                    file_id: Uuid::nil(),
                    parent_id: Uuid::nil(),
                    actor: Uuid::nil(),
                },
                "file_deleted",
            ),
            (
                MessageBusEvent::FolderCreated {
                    folder_id: Uuid::nil(),
                    name: "docs".into(),
                    parent_id: Uuid::nil(),
                    actor: Uuid::nil(),
                },
                "folder_created",
            ),
            (
                MessageBusEvent::FolderRenamed {
                    folder_id: Uuid::nil(),
                    old_name: "old".into(),
                    new_name: "new".into(),
                    parent_id: Uuid::nil(),
                    actor: Uuid::nil(),
                },
                "folder_renamed",
            ),
            (
                MessageBusEvent::FolderMoved {
                    folder_id: Uuid::nil(),
                    name: "docs".into(),
                    from: Uuid::nil(),
                    to: Uuid::nil(),
                    actor: Uuid::nil(),
                },
                "folder_moved",
            ),
            (
                MessageBusEvent::FolderDeleted {
                    folder_id: Uuid::nil(),
                    parent_id: Uuid::nil(),
                    actor: Uuid::nil(),
                },
                "folder_deleted",
            ),
            (
                MessageBusEvent::AuthzChanged {
                    affected_folders: vec![Uuid::nil()],
                },
                "authz_changed",
            ),
            (
                MessageBusEvent::JobRunStarted {
                    name: "backend_migration".into(),
                    started_at: chrono::DateTime::<chrono::Utc>::from_timestamp(0, 0).unwrap(),
                    actor: Uuid::nil(),
                },
                "job_run_started",
            ),
            (
                MessageBusEvent::JobRunProgress {
                    name: "backend_migration".into(),
                    step: Some(10),
                    total: Some(100),
                    message: Some("phase 2".into()),
                },
                "job_run_progress",
            ),
            (
                MessageBusEvent::JobRunEnded {
                    name: "backend_migration".into(),
                    success: true,
                    reason: None,
                    ended_at: chrono::DateTime::<chrono::Utc>::from_timestamp(0, 0).unwrap(),
                },
                "job_run_ended",
            ),
        ];
        for (ev, expected) in cases {
            let json = serde_json::to_value(ev).unwrap();
            assert_eq!(
                json["event"], *expected,
                "wire discriminator mismatch for {ev:?}"
            );
        }
    }

    #[test]
    fn event_roundtrip() {
        let file_id = Uuid::new_v4();
        let parent_id = Uuid::new_v4();
        let actor = Uuid::new_v4();
        let original = MessageBusEvent::FileCreated {
            file_id,
            name: "a.txt".into(),
            parent_id,
            actor,
        };
        let json = serde_json::to_string(&original).unwrap();
        let decoded: MessageBusEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn error_codes_stay_in_the_defined_ranges() {
        // Application-defined codes live in the JSON-RPC "server-defined"
        // range -32000..=-32099. Standard envelope codes live in
        // -32700..=-32600. A refactor that moves a value out of its range
        // is a wire-break — pin it here.
        for code in [
            error_code::NO_READ,
            error_code::NO_SHARE,
            error_code::NO_COMMENT,
            error_code::TOPIC_FORBIDDEN,
            error_code::SUB_LIMIT,
            error_code::RATE_LIMITED,
            error_code::NO_EDIT,
        ] {
            assert!(
                (-32099..=-32000).contains(&code),
                "app-defined code {code} outside -32099..=-32000"
            );
        }
        for code in [
            error_code::INTERNAL_ERROR,
            error_code::INVALID_REQUEST,
            error_code::METHOD_NOT_FOUND,
            error_code::INVALID_PARAMS,
        ] {
            assert!(
                (-32700..=-32600).contains(&code),
                "standard code {code} outside -32700..=-32600"
            );
        }
    }

    #[tokio::test]
    async fn noop_replicator_parks_until_notified() {
        let repl = Arc::new(NoopReplicator);
        let shutdown = Arc::new(Notify::new());
        let handle = tokio::spawn({
            let repl = Arc::clone(&repl);
            let shutdown = Arc::clone(&shutdown);
            async move { BusReplicator::run(repl, shutdown).await }
        });
        // on_local_publish is a no-op that should not panic or spawn work.
        repl.on_local_publish(
            &Topic::Folder(Uuid::nil()),
            &MessageBusEvent::FileCreated {
                file_id: Uuid::nil(),
                name: "x".into(),
                parent_id: Uuid::nil(),
                actor: Uuid::nil(),
            },
        );
        // `notify_one` (not `notify_waiters`) so the signal survives if the
        // spawned task hasn't yet reached `.notified().await` — permit
        // queues instead of being dropped. See BusReplicator docs.
        shutdown.notify_one();
        handle.await.unwrap().unwrap();
    }
}
