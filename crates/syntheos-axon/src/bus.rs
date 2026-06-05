//! The in-process event bus: channel registry, publish, subscribe, typed helpers.

use std::collections::HashMap;
use std::sync::RwLock;

use syntheos_contracts::{AxonEnvelope, PrincipalId, TenantId, TypedEvent};
use tokio::sync::broadcast::{self, Receiver, Sender};

use crate::error::AxonError;
use crate::typed::TypedReceiver;

/// Default per-channel ring-buffer capacity.
const DEFAULT_CAPACITY: usize = 1024;

/// The in-process event bus. Share it as `Arc<AxonBus>`; channels are created
/// lazily on first publish or subscribe and keyed by channel string.
pub struct AxonBus {
    /// Channel registry. Read-locked on the hot path; write-locked only when a
    /// channel string is first seen. `broadcast` send/subscribe never `.await`,
    /// so a std (sync) lock is sufficient.
    channels: RwLock<HashMap<String, Sender<AxonEnvelope>>>,
    /// Fixed per-channel ring-buffer capacity, set once at construction. Must be >= 1.
    capacity: usize,
}

impl AxonBus {
    /// Create a bus with the default per-channel capacity (1024).
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// Create a bus with an explicit per-channel ring-buffer capacity.
    ///
    /// `capacity` must be at least 1 (the underlying broadcast channel rejects 0).
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            channels: RwLock::new(HashMap::new()),
            capacity,
        }
    }

    /// Resolve (or lazily create) the sender for `channel`.
    fn sender_for(&self, channel: &str) -> Sender<AxonEnvelope> {
        // Hot path: a read lock is enough once the channel exists.
        {
            let map = self.channels.read().unwrap_or_else(|e| e.into_inner());
            if let Some(sender) = map.get(channel) {
                return sender.clone();
            }
        }
        // Miss: take the write lock and insert, guarding against a race where
        // another thread created it between the read and the write.
        let mut map = self.channels.write().unwrap_or_else(|e| e.into_inner());
        map.entry(channel.to_string())
            .or_insert_with(|| broadcast::channel(self.capacity).0)
            .clone()
    }

    /// Publish a pre-built envelope to its `channel`. Returns the number of live
    /// subscribers it reached (0 if none -- not an error).
    pub fn publish(&self, env: &AxonEnvelope) -> usize {
        // `send` errors only when there are zero receivers; that is a reach of 0.
        self.sender_for(&env.channel).send(env.clone()).unwrap_or(0)
    }

    /// Subscribe to all envelopes on `channel`. Lazily creates the channel.
    pub fn subscribe(&self, channel: &str) -> Receiver<AxonEnvelope> {
        self.sender_for(channel).subscribe()
    }

    /// Build an envelope from a typed event (via [`TypedEvent::to_envelope`]) and
    /// publish it to `E::CHANNEL`. Returns subscribers reached.
    pub fn publish_event<E: TypedEvent>(
        &self,
        event: &E,
        tenant: TenantId,
        principal: PrincipalId,
    ) -> Result<usize, AxonError> {
        let env = event.to_envelope(tenant, principal)?;
        Ok(self.publish(&env))
    }

    /// Subscribe to `E::CHANNEL` and receive only `E::KIND` events, deserialized
    /// into `E`. Other kinds on the same channel are filtered out.
    pub fn subscribe_typed<E: TypedEvent>(&self) -> TypedReceiver<E> {
        TypedReceiver::new(self.subscribe(E::CHANNEL))
    }
}

impl Default for AxonBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use syntheos_contracts::{EventId, Timestamp};

    /// Test event on the `tool` channel.
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct ToolInvoked {
        tool: String,
    }
    impl TypedEvent for ToolInvoked {
        const CHANNEL: &'static str = "tool";
        const KIND: &'static str = "tool.invoked";
    }

    /// A second event type on the SAME channel, different kind -- for filtering.
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct ToolCompleted {
        tool: String,
    }
    impl TypedEvent for ToolCompleted {
        const CHANNEL: &'static str = "tool";
        const KIND: &'static str = "tool.completed";
    }

    /// Build a raw envelope with a fixed payload for the raw-path tests.
    fn make_env(channel: &str, kind: &str) -> AxonEnvelope {
        AxonEnvelope {
            id: EventId::new(),
            channel: channel.to_string(),
            kind: kind.to_string(),
            tenant: TenantId::new(),
            principal: PrincipalId::new(),
            occurred_at: Timestamp::now(),
            payload: serde_json::json!({ "ok": true }),
        }
    }

    #[tokio::test]
    async fn raw_round_trip() {
        let bus = AxonBus::new();
        let mut rx = bus.subscribe("tool");
        let env = make_env("tool", "tool.invoked");
        assert_eq!(bus.publish(&env), 1);
        let got = rx.recv().await.expect("receives the envelope");
        assert_eq!(got, env);
    }

    #[tokio::test]
    async fn typed_round_trip() {
        let bus = AxonBus::new();
        let mut rx = bus.subscribe_typed::<ToolInvoked>();
        bus.publish_event(&ToolInvoked { tool: "kleos".into() }, TenantId::new(), PrincipalId::new())
            .expect("publishes");
        let got = rx.recv().await.expect("receives the typed event");
        assert_eq!(got, ToolInvoked { tool: "kleos".into() });
    }

    #[tokio::test]
    async fn fanout_reaches_all_subscribers() {
        let bus = AxonBus::new();
        let mut rxs: Vec<_> = (0..3).map(|_| bus.subscribe("tool")).collect();
        let env = make_env("tool", "tool.invoked");
        assert_eq!(bus.publish(&env), 3);
        for rx in &mut rxs {
            assert_eq!(rx.recv().await.expect("each subscriber receives"), env);
        }
    }

    #[tokio::test]
    async fn typed_filters_by_kind() {
        let bus = AxonBus::new();
        let mut rx = bus.subscribe_typed::<ToolInvoked>();
        let (t, p) = (TenantId::new(), PrincipalId::new());
        // Same channel, wrong kind first -- it must be skipped.
        bus.publish_event(&ToolCompleted { tool: "skip".into() }, t, p).expect("publish completed");
        bus.publish_event(&ToolInvoked { tool: "keep".into() }, t, p).expect("publish invoked");
        let got = rx.recv().await.expect("receives only the matching kind");
        assert_eq!(got, ToolInvoked { tool: "keep".into() });
    }

    #[test]
    fn publish_without_subscribers_returns_zero() {
        let bus = AxonBus::new();
        let env = make_env("tool", "tool.invoked");
        assert_eq!(bus.publish(&env), 0);
    }

    #[tokio::test]
    async fn slow_subscriber_lags() {
        // Capacity 2, but publish 5 before reading -> the subscriber falls behind.
        let bus = AxonBus::with_capacity(2);
        let mut rx = bus.subscribe_typed::<ToolInvoked>();
        let (t, p) = (TenantId::new(), PrincipalId::new());
        for i in 0..5 {
            bus.publish_event(&ToolInvoked { tool: i.to_string() }, t, p).expect("publish");
        }
        match rx.recv().await {
            Err(AxonError::Lagged(_)) => {}
            other => panic!("expected Lagged, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn closed_when_senders_dropped() {
        let bus = AxonBus::new();
        let mut rx = bus.subscribe_typed::<ToolInvoked>();
        // Dropping the bus drops the only Sender for the channel.
        drop(bus);
        match rx.recv().await {
            Err(AxonError::Closed) => {}
            other => panic!("expected Closed, got {other:?}"),
        }
    }
}
