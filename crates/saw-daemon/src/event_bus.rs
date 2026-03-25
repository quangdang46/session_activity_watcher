use saw_core::AgentEvent;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::{RecvError, TryRecvError};

pub const EVENT_BUS_CAPACITY: usize = 1000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EventBusMetrics {
    pub events_published: u64,
    pub events_dropped: u64,
}

#[derive(Default)]
struct MetricsInner {
    events_published: AtomicU64,
    events_dropped: AtomicU64,
}

#[derive(Clone)]
pub struct EventBus {
    sender: broadcast::Sender<AgentEvent>,
    metrics: Arc<MetricsInner>,
}

pub struct Receiver<T: Clone> {
    inner: broadcast::Receiver<T>,
    metrics: Arc<MetricsInner>,
}

impl EventBus {
    pub fn new() -> Self {
        Self::with_capacity(EVENT_BUS_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        assert!(capacity > 0, "event bus capacity must be greater than zero");

        let (sender, _) = broadcast::channel(capacity);
        Self {
            sender,
            metrics: Arc::new(MetricsInner::default()),
        }
    }

    pub fn subscribe(&self) -> Receiver<AgentEvent> {
        Receiver::new(self.sender.subscribe(), Arc::clone(&self.metrics))
    }

    pub fn publish(&self, event: AgentEvent) {
        self.metrics
            .events_published
            .fetch_add(1, Ordering::Relaxed);
        let _ = self.sender.send(event);
    }

    pub fn metrics(&self) -> EventBusMetrics {
        EventBusMetrics {
            events_published: self.events_published(),
            events_dropped: self.events_dropped(),
        }
    }

    pub fn events_published(&self) -> u64 {
        self.metrics.events_published.load(Ordering::Relaxed)
    }

    pub fn events_dropped(&self) -> u64 {
        self.metrics.events_dropped.load(Ordering::Relaxed)
    }

    pub fn receiver_count(&self) -> usize {
        self.sender.receiver_count()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Clone> Receiver<T> {
    fn new(inner: broadcast::Receiver<T>, metrics: Arc<MetricsInner>) -> Self {
        Self { inner, metrics }
    }

    pub async fn recv(&mut self) -> Result<T, RecvError> {
        loop {
            match self.inner.recv().await {
                Ok(event) => return Ok(event),
                Err(RecvError::Lagged(skipped)) => self.record_lag(skipped),
                Err(RecvError::Closed) => return Err(RecvError::Closed),
            }
        }
    }

    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        loop {
            match self.inner.try_recv() {
                Ok(event) => return Ok(event),
                Err(TryRecvError::Lagged(skipped)) => self.record_lag(skipped),
                Err(err) => return Err(err),
            }
        }
    }

    pub fn resubscribe(&self) -> Self {
        Self::new(self.inner.resubscribe(), Arc::clone(&self.metrics))
    }

    fn record_lag(&self, skipped: u64) {
        if skipped == 0 {
            return;
        }

        self.metrics
            .events_dropped
            .fetch_add(skipped, Ordering::Relaxed);
        log::warn!("event bus dropped {skipped} events for a lagging subscriber");
    }
}

impl<T: Clone> Deref for Receiver<T> {
    type Target = broadcast::Receiver<T>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<T: Clone> DerefMut for Receiver<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::{EventBus, EventBusMetrics, EVENT_BUS_CAPACITY};
    use chrono::Utc;
    use saw_core::AgentEvent;

    fn session_started(session_id: impl Into<String>) -> AgentEvent {
        AgentEvent::SessionStart {
            timestamp: Utc::now(),
            session_id: session_id.into(),
        }
    }

    #[tokio::test]
    async fn publishes_the_same_event_to_multiple_subscribers() {
        let bus = EventBus::new();
        let mut first = bus.subscribe();
        let mut second = bus.subscribe();
        let event = session_started("ses-shared");

        bus.publish(event.clone());

        assert_eq!(first.recv().await.unwrap(), event);
        assert_eq!(second.recv().await.unwrap(), event);
        assert_eq!(
            bus.metrics(),
            EventBusMetrics {
                events_published: 1,
                events_dropped: 0,
            }
        );
    }

    #[tokio::test]
    async fn lagging_subscribers_drop_oldest_events_and_increment_metrics() {
        let bus = EventBus::with_capacity(3);
        let mut slow = bus.subscribe();
        let events: Vec<_> = (0..5)
            .map(|index| session_started(format!("ses-{index}")))
            .collect();

        for event in events.iter().cloned() {
            bus.publish(event);
        }

        let skipped = match slow.inner.recv().await {
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => skipped,
            other => panic!("expected lagged receiver, got {other:?}"),
        };
        slow.record_lag(skipped);

        assert_eq!(slow.inner.recv().await.unwrap(), events[skipped as usize]);
        assert_eq!(
            bus.metrics(),
            EventBusMetrics {
                events_published: 5,
                events_dropped: skipped,
            }
        );
    }

    #[tokio::test]
    async fn default_capacity_is_1000_events() {
        assert_eq!(EVENT_BUS_CAPACITY, 1000);

        let bus = EventBus::new();
        let mut slow = bus.subscribe();
        let events: Vec<_> = (0..(EVENT_BUS_CAPACITY * 2) + 5)
            .map(|index| session_started(format!("ses-{index}")))
            .collect();

        for event in events.iter().cloned() {
            bus.publish(event);
        }

        let skipped = match slow.inner.recv().await {
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => skipped,
            other => panic!("expected lagged receiver, got {other:?}"),
        };
        slow.record_lag(skipped);

        assert_eq!(slow.inner.recv().await.unwrap(), events[skipped as usize]);
        assert_eq!(bus.events_dropped(), skipped);
        assert_eq!(
            bus.events_published(),
            ((EVENT_BUS_CAPACITY * 2) + 5) as u64
        );
    }
}
