//! The single error type the bus surfaces.

/// Errors surfaced by the bus. Publishing only fails on serialization; receiving
/// surfaces broadcast lag and channel closure.
#[derive(Debug, thiserror::Error)]
pub enum AxonError {
    /// A typed event failed to serialize into (or deserialize out of) its envelope payload.
    #[error("event (de)serialization failed: {0}")]
    Serialize(#[from] serde_json::Error),
    /// The typed subscriber fell behind and skipped `n` events.
    #[error("subscriber lagged and dropped {0} events")]
    Lagged(u64),
    /// No senders remain for the channel; no further events will arrive.
    #[error("channel closed")]
    Closed,
}
