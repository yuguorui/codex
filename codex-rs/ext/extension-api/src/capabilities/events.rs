use codex_protocol::ThreadId;
use codex_protocol::protocol::Event;
use std::future::Future;
use std::pin::Pin;

/// Result of attempting host-owned delivery for an extension event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtensionEventDelivery {
    /// The host observed delivery and can durably acknowledge this identity.
    Acknowledged { idempotency_key: String },
    /// Delivery was not observed. Retrying the same identity is safe.
    Retryable { idempotency_key: String },
}

/// Completion returned when an extension event must observe host delivery.
pub type ExtensionEventDeliveryFuture<'a> =
    Pin<Box<dyn Future<Output = ExtensionEventDelivery> + Send + 'a>>;

/// Host availability notification used to retry durable extension deliveries.
pub type ExtensionEventAvailabilityFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

/// Extension warning with an explicit thread target and optional turn correlation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionWarning {
    /// Stable host-owned thread identifier used for delivery.
    pub thread_id: String,
    /// Stable host-owned turn identifier when the warning arose in a turn callback.
    pub turn_id: Option<String>,
    /// Concise warning message for the user.
    pub message: String,
}

/// Host-provided fire-and-forget sink for extension-generated events.
///
/// Extensions construct protocol events with the correlation id appropriate for
/// the callback they are handling, then leave persistence, ordering, transport
/// fanout, and logging decisions to the host.
pub trait ExtensionEventSink: Send + Sync {
    /// Queue one protocol event for host-owned delivery.
    fn emit(&self, event: Event);

    /// Deliver one protocol event and report whether the host observed delivery.
    ///
    /// Extensions use this when later state transitions depend on the host
    /// accepting and delivering the event. The returned idempotency key can be
    /// persisted with the extension state so an unacknowledged event can be
    /// retried after restart.
    fn emit_and_wait(&self, event: Event) -> ExtensionEventDeliveryFuture<'_> {
        let idempotency_key = event.id.clone();
        self.emit(event);
        Box::pin(std::future::ready(ExtensionEventDelivery::Retryable {
            idempotency_key,
        }))
    }

    /// Wait for a new eligible subscriber after a retryable delivery.
    ///
    /// Hosts that expose subscriber lifecycle state return `Some`; other hosts
    /// leave durable retry to process restoration.
    fn wait_for_delivery_availability(
        &self,
        _thread_id: ThreadId,
    ) -> Option<ExtensionEventAvailabilityFuture<'_>> {
        None
    }

    /// Queue one warning for host-owned delivery.
    ///
    /// Implementations must use [`ExtensionWarning::thread_id`] for routing. The optional
    /// [`ExtensionWarning::turn_id`] is correlation metadata and does not identify a thread.
    fn emit_warning(&self, warning: ExtensionWarning);
}

/// Event sink used when the host does not expose extension event emission.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopExtensionEventSink;

impl ExtensionEventSink for NoopExtensionEventSink {
    fn emit(&self, _event: Event) {}

    fn emit_warning(&self, _warning: ExtensionWarning) {}
}
