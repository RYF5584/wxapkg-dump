// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! Server-sent event bus.
//!
//! Every operation publishes short progress and result lines here; a front-end
//! renders them in its log drawer. The bus is lossy by design: a slow or disconnected
//! client must never block a handler.

use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use futures_core::Stream;
use serde::Serialize;
use tokio::sync::broadcast;

/// Capacity of the in-memory ring the subscribers read from.
const CAPACITY: usize = 512;

/// One log line pushed to subscribers.
#[derive(Debug, Clone, Serialize)]
pub struct Event {
    /// Unix timestamp in seconds.
    pub ts: i64,
    /// `info`, `warn` or `error`.
    pub level: String,
    /// Message body.
    pub message: String,
}

/// Broadcasts events to every connected client.
#[derive(Clone)]
pub struct EventBus {
    sender: Arc<broadcast::Sender<Event>>,
    /// Latches on shutdown; subscribers read it to end their streams.
    closed: Arc<AtomicBool>,
}

impl std::fmt::Debug for EventBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventBus")
            .field("receivers", &self.sender.receiver_count())
            .field("closed", &self.closed.load(Ordering::SeqCst))
            .finish()
    }
}

impl EventBus {
    /// Creates an empty bus.
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(CAPACITY);
        Self {
            sender: Arc::new(sender),
            closed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Subscribes to the stream.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.sender.subscribe()
    }

    /// Marks the bus as finished.
    ///
    /// An SSE response never completes by itself, so a graceful shutdown would wait on it
    /// forever. Closing the bus lets each subscriber end its stream — but the flag is only
    /// observed when the stream is polled, so a caller must still publish one last line
    /// afterwards to wake any subscriber that is sitting idle.
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    /// The flag an SSE handler wraps its stream with.
    pub fn closed_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.closed)
    }

    /// Publishes a line. Returns `false` when nobody is listening.
    pub fn publish(&self, level: &str, message: impl Into<String>) -> bool {
        let event = Event {
            ts: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            level: level.to_owned(),
            message: message.into(),
        };
        tracing::debug!(target: "wxapkg-dump::events", level = %event.level, "{}", event.message);
        self.sender.send(event).is_ok()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

/// Ends the wrapped stream once the bus has been closed.
///
/// `filter_map` cannot express this: it skips an item rather than stopping, and the HTTP
/// body has to actually complete for the connection to close.
pub struct UntilClosed<S> {
    inner: S,
    closed: Arc<AtomicBool>,
}

impl<S> UntilClosed<S> {
    /// Wraps `inner` so it stops yielding as soon as `closed` is set.
    pub fn new(inner: S, closed: Arc<AtomicBool>) -> Self {
        Self { inner, closed }
    }
}

impl<S: Stream + Unpin> Stream for UntilClosed<S> {
    type Item = S::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.closed.load(Ordering::SeqCst) {
            return Poll::Ready(None);
        }
        Pin::new(&mut this.inner).poll_next(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_stream::wrappers::BroadcastStream;
    use tokio_stream::StreamExt as _;

    #[tokio::test]
    async fn events_reach_subscribers() {
        let bus = EventBus::new();
        let mut receiver = bus.subscribe();
        bus.publish("info", "hello");
        let event = receiver.recv().await.unwrap();
        assert_eq!(event.message, "hello");
        assert_eq!(event.level, "info");
    }

    #[tokio::test]
    async fn publishing_without_subscribers_is_lossless_for_the_server() {
        let bus = EventBus::new();
        assert!(!bus.publish("info", "nobody is listening"));
    }

    #[tokio::test]
    async fn closing_the_bus_ends_a_subscriber_stream() {
        let bus = EventBus::new();
        let mut stream = Box::pin(UntilClosed::new(
            BroadcastStream::new(bus.subscribe()),
            bus.closed_handle(),
        ));

        bus.publish("info", "before shutdown");
        assert!(
            stream.next().await.is_some(),
            "the stream must still deliver"
        );

        bus.close();
        bus.publish("info", "server is shutting down");
        assert!(
            stream.next().await.is_none(),
            "a closed bus must complete the stream so graceful shutdown can finish"
        );
    }
}
