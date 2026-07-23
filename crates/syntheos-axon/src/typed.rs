//! A typed view over a raw channel subscription.

use std::marker::PhantomData;

use serde::de::DeserializeOwned;
use syntheos_contracts::{AxonEnvelope, TypedEvent};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::broadcast::Receiver;

use crate::error::AxonError;

/// A typed view over a channel subscription. Yields only events whose `kind`
/// matches `E::KIND`, deserialized into `E`. Non-matching kinds are skipped.
pub struct TypedReceiver<E> {
    /// The underlying raw broadcast receiver for `E::CHANNEL`.
    inner: Receiver<AxonEnvelope>,
    /// Zero-sized marker tying this receiver to its event type.
    _marker: PhantomData<E>,
}

/// Constructs typed receivers from raw channel subscriptions.
impl<E> TypedReceiver<E> {
    /// Wrap a raw broadcast receiver as a typed receiver for `E`.
    pub(crate) fn new(inner: Receiver<AxonEnvelope>) -> Self {
        Self {
            inner,
            _marker: PhantomData,
        }
    }
}

/// Receives and deserializes events matching the requested typed-event contract.
impl<E> TypedReceiver<E>
where
    E: TypedEvent + DeserializeOwned,
{
    /// Await the next matching event. Skips envelopes whose `kind != E::KIND`.
    ///
    /// Returns [`AxonError::Lagged`] if the subscriber fell behind and dropped events,
    /// [`AxonError::Closed`] when no senders remain, and [`AxonError::Serialize`] if a
    /// matching envelope's payload fails to deserialize into `E`.
    pub async fn recv(&mut self) -> Result<E, AxonError> {
        loop {
            match self.inner.recv().await {
                Ok(env) if env.kind == E::KIND => {
                    return serde_json::from_value(env.payload).map_err(AxonError::from);
                }
                // A different kind on the same channel -- skip it and keep waiting.
                Ok(_) => continue,
                Err(RecvError::Lagged(n)) => return Err(AxonError::Lagged(n)),
                Err(RecvError::Closed) => return Err(AxonError::Closed),
            }
        }
    }
}
