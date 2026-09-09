//! In-process `RealtimeBus` — one `broadcast::Sender` per active topic,
//! held in a [`DashMap`] keyed by [`Topic`]. Publish is fire-and-forget,
//! subscribe returns a `Stream` backed by [`BroadcastStream`].
//!
//! # Slow-subscriber policy
//!
//! `tokio::sync::broadcast` drops the oldest queued message when a
//! subscriber can't keep up (ring is bounded to [`BROADCAST_RING_CAPACITY`]).
//! When a subscriber's stream sees a `Lagged` marker, the WS handler kills
//! that session with a JSON-RPC `rt.revoked` notification (reason
//! `slow_consumer`) and lets the client reconnect + refetch. That policy
//! lives in the handler; this module just surfaces the `Lagged` variant.
//!
//! # Topic GC
//!
//! When the last subscriber of a topic drops, `broadcast::Sender::receiver_count`
//! falls to zero. New publishes on that topic still succeed (they hit the
//! now-orphaned sender), but the entry stays in the map. A background GC
//! task periodically sweeps entries whose `receiver_count == 0`. Kept
//! simple: no ref-count tracking, no watchdog — a sweep every
//! [`GC_INTERVAL`] is enough for our fan-out volume.

use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use futures::StreamExt;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;

use crate::application::ports::realtime_ports::{
    BusReplicator, BusStream, RealtimeBus, RealtimeEvent, Topic,
};

/// Per-topic ring-buffer size for slow subscribers. When a subscriber lags
/// past this, the broadcast channel starts dropping the oldest messages and
/// signals `Lagged`. Sized generously — fan-out volume per topic is low
/// (folder mutations, job step ticks) so pressure comes from a genuinely
/// dead consumer, not from a normal traffic spike.
pub const BROADCAST_RING_CAPACITY: usize = 256;

/// How often the GC task sweeps empty topics. Short enough that a burst of
/// short-lived subs (folder navigations) doesn't grow the map indefinitely,
/// long enough that GC overhead stays trivial.
pub const GC_INTERVAL: Duration = Duration::from_secs(60);

/// The in-process implementation of [`RealtimeBus`].
///
/// Callers hold `Arc<InProcessRealtimeBus>` (or `Arc<dyn RealtimeBus>`).
/// The struct owns its topic map and — when constructed via
/// [`InProcessRealtimeBus::with_replicator`] — an [`Arc<dyn BusReplicator>`]
/// that gets fed every local publish for outbound broker forwarding.
pub struct InProcessRealtimeBus {
    topics: DashMap<Topic, broadcast::Sender<RealtimeEvent>>,
    replicator: Arc<dyn BusReplicator>,
}

impl InProcessRealtimeBus {
    /// Construct with a replicator. In v1 that's a
    /// [`crate::application::ports::realtime_ports::NoopReplicator`]; when
    /// multi-instance ships, it becomes the pg-NOTIFY or broker impl.
    ///
    /// The GC task holds a [`Weak`] handle so it exits naturally when the
    /// last outer `Arc` drops — matches OxiCloud's DI convention that
    /// background tasks are dropped on runtime shutdown, no explicit
    /// signal needed.
    pub fn with_replicator(replicator: Arc<dyn BusReplicator>) -> Arc<Self> {
        let bus = Arc::new(Self {
            topics: DashMap::new(),
            replicator,
        });
        bus.spawn_gc();
        bus
    }

    /// Spawn the periodic GC task. Holds `Weak<Self>` so it does not keep
    /// the bus alive past the last outer `Arc` drop; the next
    /// upgrade-and-sweep call after that returns `None` and the loop
    /// exits.
    fn spawn_gc(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(GC_INTERVAL);
            // First tick fires immediately; skip it so we don't sweep an
            // empty map on startup.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                match weak.upgrade() {
                    Some(bus) => bus.gc_empty_topics(),
                    None => break,
                }
            }
        });
    }

    /// Remove topics whose broadcast sender has no live receivers. Called
    /// on the GC ticker.
    fn gc_empty_topics(&self) {
        self.topics
            .retain(|_topic, sender| sender.receiver_count() > 0);
    }

    /// For tests + observability: how many topics currently have a
    /// broadcast sender in the map.
    pub fn active_topic_count(&self) -> usize {
        self.topics.len()
    }

    /// Get-or-insert the broadcast sender for `topic`, returning a fresh
    /// receiver. Used by both `publish` (for the sender) and `subscribe`
    /// (for the receiver) — one code path for the map insert avoids a race
    /// where publish creates a sender concurrent subscribers miss.
    fn sender_for(&self, topic: &Topic) -> broadcast::Sender<RealtimeEvent> {
        self.topics
            .entry(*topic)
            .or_insert_with(|| broadcast::channel(BROADCAST_RING_CAPACITY).0)
            .clone()
    }
}

impl RealtimeBus for InProcessRealtimeBus {
    fn publish(&self, topic: &Topic, event: RealtimeEvent) {
        // Feed the replicator FIRST — if it were called after local fan-out,
        // an unwind on a broken subscriber could skip broker forwarding.
        // `on_local_publish` is a sync fire-and-forget contract; slow
        // replicators must background their I/O themselves.
        self.replicator.on_local_publish(topic, &event);

        // If nobody is subscribed, don't allocate a sender just to drop
        // its message. `broadcast::Sender::send` returns Err when there
        // are no receivers — cheaper still to short-circuit here.
        if let Some(sender) = self.topics.get(topic) {
            // `send` never blocks; it drops the oldest when the ring is
            // full, signalling `Lagged` on that subscriber's next recv.
            let _ = sender.send(event);
        }
        // Else: no active subs. Event is lost by design (see
        // `docs/plan/message-bus.md § Failure modes`).
    }

    fn subscribe(&self, topic: &Topic) -> BusStream {
        let receiver = self.sender_for(topic).subscribe();
        // `BroadcastStream` yields `Result<T, BroadcastStreamRecvError>`;
        // filter out the `Lagged` variant here and terminate the stream on
        // it so the WS handler sees a clean "the stream ended" signal
        // rather than having to match on the error. The handler is
        // responsible for emitting the `rt.revoked` notification with
        // `slow_consumer` reason on such a termination.
        let stream = BroadcastStream::new(receiver).take_while(|item| {
            let keep = item.is_ok();
            async move { keep }
        });
        Box::pin(stream.filter_map(|item| async move { item.ok() }))
    }
}

// ════════════════════════════════════════════════════════════════════════════
// Tests
// ════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::ports::realtime_ports::NoopReplicator;
    use futures::StreamExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::sync::Notify;
    use uuid::Uuid;

    fn make_bus() -> Arc<InProcessRealtimeBus> {
        InProcessRealtimeBus::with_replicator(Arc::new(NoopReplicator))
    }

    fn folder_topic() -> Topic {
        Topic::Folder(Uuid::new_v4())
    }

    fn file_created(parent_id: Uuid) -> RealtimeEvent {
        RealtimeEvent::FileCreated {
            file_id: Uuid::new_v4(),
            name: "a.txt".into(),
            parent_id,
            actor: Uuid::new_v4(),
        }
    }

    /// Positive fan-out: a subscriber to a topic receives an event
    /// published on that same topic.
    #[tokio::test]
    async fn subscriber_receives_publish_on_same_topic() {
        let bus = make_bus();
        let topic = folder_topic();
        let mut stream = bus.subscribe(&topic);

        // Give the subscriber a moment to install (broadcast::Sender::send
        // silently fails against a not-yet-installed receiver; the
        // subscribe() call above is synchronous but the receiver still
        // needs to be registered on the sender's side before publish).
        let parent = match topic {
            Topic::Folder(id) => id,
            _ => unreachable!(),
        };
        let event = file_created(parent);
        bus.publish(&topic, event.clone());

        let received = tokio::time::timeout(Duration::from_millis(200), stream.next())
            .await
            .expect("event should arrive within 200ms")
            .expect("stream must yield at least once");
        assert_eq!(received, event);
    }

    /// Topic isolation: a subscriber to folder A does not receive events
    /// published on folder B. This is the invariant the smoke test's
    /// Scenario 2 asserts end-to-end; verifying it in-unit here catches
    /// bugs early.
    #[tokio::test]
    async fn subscriber_does_not_receive_other_topic() {
        let bus = make_bus();
        let topic_a = folder_topic();
        let topic_b = folder_topic();
        assert_ne!(topic_a, topic_b);

        let mut stream_a = bus.subscribe(&topic_a);

        let parent_b = match topic_b {
            Topic::Folder(id) => id,
            _ => unreachable!(),
        };
        let event_b = file_created(parent_b);
        bus.publish(&topic_b, event_b);

        // The A subscriber must NOT see B's event. Poll with a short
        // timeout — if the isolation is broken we'll see the event; if
        // it holds we'll time out.
        let result = tokio::time::timeout(Duration::from_millis(100), stream_a.next()).await;
        assert!(
            result.is_err(),
            "subscriber to topic A must not observe events published on topic B \
             (got {:?})",
            result.ok().flatten()
        );
    }

    /// Multiple subscribers to the same topic all see each publish.
    #[tokio::test]
    async fn multi_subscriber_fanout() {
        let bus = make_bus();
        let topic = folder_topic();
        let mut s1 = bus.subscribe(&topic);
        let mut s2 = bus.subscribe(&topic);

        let parent = match topic {
            Topic::Folder(id) => id,
            _ => unreachable!(),
        };
        let event = file_created(parent);
        bus.publish(&topic, event.clone());

        let r1 = tokio::time::timeout(Duration::from_millis(200), s1.next())
            .await
            .unwrap()
            .unwrap();
        let r2 = tokio::time::timeout(Duration::from_millis(200), s2.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(r1, event);
        assert_eq!(r2, event);
    }

    /// Publishing with no subscribers is a no-op (does not panic, does not
    /// grow the map into an orphan-sender state we later have to sweep).
    #[tokio::test]
    async fn publish_with_no_subscribers_is_noop() {
        let bus = make_bus();
        let topic = folder_topic();
        let parent = match topic {
            Topic::Folder(id) => id,
            _ => unreachable!(),
        };
        bus.publish(&topic, file_created(parent));
        assert_eq!(
            bus.active_topic_count(),
            0,
            "publish without any subscribe must not insert into topics map"
        );
    }

    /// The replicator sees every publish, exactly once per publish.
    /// Locks in the "feed replicator FIRST" contract without asserting on
    /// broker semantics we don't control from unit-tests.
    #[tokio::test]
    async fn replicator_is_notified_on_publish() {
        struct CountingReplicator {
            count: AtomicUsize,
        }
        #[async_trait::async_trait]
        impl BusReplicator for CountingReplicator {
            fn on_local_publish(&self, _topic: &Topic, _event: &RealtimeEvent) {
                self.count.fetch_add(1, Ordering::SeqCst);
            }
            async fn run(
                self: Arc<Self>,
                shutdown: Arc<Notify>,
            ) -> Result<(), crate::common::errors::DomainError> {
                shutdown.notified().await;
                Ok(())
            }
        }

        let counter = Arc::new(CountingReplicator {
            count: AtomicUsize::new(0),
        });
        let bus = InProcessRealtimeBus::with_replicator(Arc::clone(&counter) as Arc<_>);
        let topic = folder_topic();
        let _sub = bus.subscribe(&topic);
        let parent = match topic {
            Topic::Folder(id) => id,
            _ => unreachable!(),
        };
        bus.publish(&topic, file_created(parent));
        bus.publish(&topic, file_created(parent));
        bus.publish(&topic, file_created(parent));
        assert_eq!(counter.count.load(Ordering::SeqCst), 3);
    }

    /// Dropping the last subscriber leaves the topic sender orphaned until
    /// the GC sweeps it. We don't wait for the timer here (that would make
    /// the test slow); instead we call `gc_empty_topics` directly to
    /// verify the sweep does what it promises.
    #[tokio::test]
    async fn gc_removes_empty_topics() {
        let bus = make_bus();
        let topic = folder_topic();
        {
            let _sub = bus.subscribe(&topic);
            assert_eq!(bus.active_topic_count(), 1);
        }
        // Subscriber dropped. Sender is still in the map, but receiver
        // count is zero — the sweep should reclaim it.
        bus.gc_empty_topics();
        assert_eq!(bus.active_topic_count(), 0);
    }
}
