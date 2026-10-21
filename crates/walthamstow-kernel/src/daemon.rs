//! Copyright (c) Scott A Dixon
//!
//! The engine daemon's lifecycle, whatever product it runs: the bus a product's events reach
//! their consumers through, the tasks the daemon runs on its own behalf, and the signal
//! that stops them.

use std::sync::{Arc, Mutex, PoisonError, RwLock};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

/// How many events each channel holds for a receiver that falls behind.
const EVENT_CAPACITY: usize = 512;

/// The channel an event reaches besides the complete stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// The state and activation events a transport forwards to clients.
    Transport,
    /// Runtime-view and runtime-accounting changes.
    Observability,
}

/// An event a product publishes on the bus.
pub trait BusEvent: Clone + Send + 'static {
    /// The channel the event reaches besides the complete stream.
    fn route(&self) -> Route;
}

/// A listener that hears each event as it is published.
type Listener<E> = Arc<dyn Fn(&E) + Send + Sync>;

/// The channels a product's events reach their consumers through.
///
/// Every published event reaches the complete stream, which in-process consumers and
/// tests read. Each also reaches the channel its [`Route`] names, so high-volume
/// observability traffic cannot evict the state and activation events a consumer waits
/// for. Listeners hear every published event on the publishing thread, in the order that
/// thread published them.
pub struct EventBus<E> {
    all: broadcast::Sender<E>,
    transport: broadcast::Sender<E>,
    observability: broadcast::Sender<E>,
    listeners: RwLock<Vec<Listener<E>>>,
}

impl<E> std::fmt::Debug for EventBus<E> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("EventBus").finish_non_exhaustive()
    }
}

impl<E: BusEvent> Default for EventBus<E> {
    fn default() -> Self {
        Self::new()
    }
}

impl<E: BusEvent> EventBus<E> {
    pub fn new() -> Self {
        Self {
            all: broadcast::channel(EVENT_CAPACITY).0,
            transport: broadcast::channel(EVENT_CAPACITY).0,
            observability: broadcast::channel(EVENT_CAPACITY).0,
            listeners: RwLock::new(Vec::new()),
        }
    }

    /// Adds `listener`, which hears every event published from now on, on the
    /// publishing thread, so it hands work off rather than doing it.
    pub fn listen(&self, listener: impl Fn(&E) + Send + Sync + 'static) {
        self.listeners
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Arc::new(listener));
    }

    /// Tells every listener of `event`, then publishes it to the complete stream and to
    /// the channel its route names. An event no receiver is listening for is dropped.
    pub fn publish(&self, event: E) {
        let listeners = self
            .listeners
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        for listener in listeners {
            listener(&event);
        }
        let _ = self.all.send(event.clone());
        let _ = match event.route() {
            Route::Observability => self.observability.send(event),
            Route::Transport => self.transport.send(event),
        };
    }

    /// Publishes to the transport alone, for work that outlives a borrow of the bus.
    pub fn transport_publisher(&self) -> TransportPublisher<E> {
        TransportPublisher(self.transport.clone())
    }

    /// A receiver of every published event.
    pub fn subscribe(&self) -> broadcast::Receiver<E> {
        self.all.subscribe()
    }

    /// A receiver of the events routed to the transport.
    pub fn subscribe_transport(&self) -> broadcast::Receiver<E> {
        self.transport.subscribe()
    }

    /// A receiver of the events routed to observability.
    pub fn subscribe_observability(&self) -> broadcast::Receiver<E> {
        self.observability.subscribe()
    }
}

/// Publishes events a transport forwards to clients and the complete stream does not
/// carry.
#[derive(Debug, Clone)]
pub struct TransportPublisher<E>(broadcast::Sender<E>);

impl<E> TransportPublisher<E> {
    /// An event no client is listening for is dropped.
    pub fn publish(&self, event: E) {
        let _ = self.0.send(event);
    }
}

/// The tasks the daemon runs on its own behalf, and the signal that stops them.
#[derive(Debug, Default)]
pub struct Daemon {
    shutdown: CancellationToken,
    tasks: TaskTracker,
    /// Held while a task is admitted and while stopping begins, so no task is admitted
    /// once it has.
    admission: Mutex<()>,
}

impl Daemon {
    pub fn new() -> Self {
        Self::default()
    }

    /// Signals the daemon's tasks, and all work holding its shutdown token, to stop.
    /// Signalling again does nothing more.
    pub fn shutdown(&self) {
        self.shutdown.cancel();
    }

    /// Cancelled when the daemon shuts down. Work takes a child of it, or watches it.
    pub fn shutdown_token(&self) -> &CancellationToken {
        &self.shutdown
    }

    /// Completes when the daemon begins shutting down.
    pub async fn shutting_down(&self) {
        self.shutdown.cancelled().await;
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutdown.is_cancelled()
    }

    /// Runs `task` on the daemon's behalf, so [`stop`](Self::stop) waits for it. A task
    /// offered after shutdown is signalled still runs, and sees the signal; once `stop`
    /// has begun it is refused, and `None` is returned.
    pub fn spawn<F>(&self, task: F) -> Option<tokio::task::JoinHandle<F::Output>>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let _admission = self
            .admission
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if self.tasks.is_closed() {
            return None;
        }
        Some(self.tasks.spawn(task))
    }

    /// Shuts down, then waits for every task the daemon runs on its own behalf. No task
    /// is admitted once it has begun.
    pub async fn stop(&self) {
        {
            let _admission = self
                .admission
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            self.shutdown.cancel();
            self.tasks.close();
        }
        self.tasks.wait().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[derive(Debug, Clone, PartialEq)]
    enum Event {
        State(&'static str),
        Runtime,
    }

    impl BusEvent for Event {
        fn route(&self) -> Route {
            match self {
                Self::State(_) => Route::Transport,
                Self::Runtime => Route::Observability,
            }
        }
    }

    #[test]
    fn an_event_reaches_the_complete_stream_and_the_channel_its_route_names() {
        let bus = EventBus::new();
        let (mut all, mut transport, mut observability) = (
            bus.subscribe(),
            bus.subscribe_transport(),
            bus.subscribe_observability(),
        );
        let state = Event::State("changed");

        bus.publish(state.clone());
        bus.publish(Event::Runtime);

        assert_eq!(all.try_recv().ok(), Some(state.clone()));
        assert_eq!(all.try_recv().ok(), Some(Event::Runtime));
        assert_eq!(transport.try_recv().ok(), Some(state));
        assert!(
            transport.try_recv().is_err(),
            "observability events stay off the transport"
        );
        assert_eq!(observability.try_recv().ok(), Some(Event::Runtime));
        assert!(
            observability.try_recv().is_err(),
            "transport events stay off observability"
        );
    }

    #[test]
    fn a_listener_hears_every_event_before_any_channel_carries_it() {
        let bus = EventBus::new();
        let mut all = bus.subscribe();
        let heard = Arc::new(std::sync::Mutex::new(Vec::new()));
        let record = heard.clone();
        bus.listen(move |event: &Event| record.lock().unwrap().push(event.clone()));

        bus.publish(Event::State("first"));
        bus.publish(Event::Runtime);

        assert_eq!(
            *heard.lock().unwrap(),
            vec![Event::State("first"), Event::Runtime]
        );
        assert_eq!(all.try_recv().ok(), Some(Event::State("first")));
    }

    #[test]
    fn the_transport_publisher_reaches_the_transport_alone() {
        let bus = EventBus::new();
        let (mut all, mut transport) = (bus.subscribe(), bus.subscribe_transport());
        let state = Event::State("forwarded");

        bus.transport_publisher().publish(state.clone());

        assert_eq!(transport.try_recv().ok(), Some(state));
        assert!(all.try_recv().is_err());
    }

    #[test]
    fn publishing_with_no_receiver_is_not_an_error() {
        let bus = EventBus::new();
        bus.publish(Event::State("unheard"));
        bus.transport_publisher().publish(Event::State("unheard"));
    }

    #[tokio::test]
    async fn stopping_waits_for_every_task_the_daemon_spawned() {
        let daemon = Daemon::new();
        let finished = Arc::new(AtomicBool::new(false));
        let token = daemon.shutdown_token().child_token();
        let task_finished = finished.clone();
        daemon
            .spawn(async move {
                token.cancelled().await;
                task_finished.store(true, Ordering::SeqCst);
            })
            .expect("a running daemon admits a task");

        assert!(!daemon.is_shutting_down());
        daemon.stop().await;

        assert!(daemon.is_shutting_down());
        assert!(
            finished.load(Ordering::SeqCst),
            "stop returned before its task finished"
        );
        daemon.shutting_down().await;
    }

    #[tokio::test]
    async fn a_task_offered_after_the_signal_runs_and_one_offered_once_stopping_is_refused() {
        let daemon = Daemon::new();
        let signalled = Arc::new(AtomicBool::new(false));
        daemon.shutdown();
        let task_signalled = signalled.clone();
        let token = daemon.shutdown_token().clone();
        daemon
            .spawn(async move {
                token.cancelled().await;
                task_signalled.store(true, Ordering::SeqCst);
            })
            .expect("a signalled daemon still admits a task until it stops");
        daemon.stop().await;
        assert!(
            signalled.load(Ordering::SeqCst),
            "stop waits for a task admitted after the signal"
        );

        let late = Arc::new(AtomicBool::new(false));
        let task_late = late.clone();
        assert!(daemon
            .spawn(async move { task_late.store(true, Ordering::SeqCst) })
            .is_none());
        assert!(!late.load(Ordering::SeqCst));
    }
}
