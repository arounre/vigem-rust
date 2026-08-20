//! One background IRP per (pad, kind); each subscriber has its own queue.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::error::BusError;
use crate::internal::bounded_channel::{self, NotifReceiver, NotifSender};
use crate::internal::bus::NotificationWorkerHandle;
use crate::target::NotificationBuffer;

struct Subscriber<N> {
    id: u64,
    tx: NotifSender<Result<N, BusError>>,
}

/// Clones each notification to every live subscriber.
pub(crate) struct Fanout<N> {
    subs: Mutex<Vec<Subscriber<N>>>,
    next_id: AtomicU64,
    /// Worker exited; `subscribe` returns `None` after this.
    closed: AtomicBool,
}

impl<N> Fanout<N> {
    pub(crate) fn new() -> Self {
        Self {
            subs: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            closed: AtomicBool::new(false),
        }
    }

    /// `None` if closing; caller must start a new hub.
    pub(crate) fn subscribe(
        &self,
        buffer: NotificationBuffer,
    ) -> Option<(u64, NotifReceiver<Result<N, BusError>>)> {
        if self.closed.load(Ordering::Acquire) {
            return None;
        }
        let (tx, rx) = bounded_channel::channel(buffer.to_channel_capacity());
        let mut subs = self.subs.lock().unwrap_or_else(|e| e.into_inner());
        // Re-check under the lock; never attach after `close`.
        if self.closed.load(Ordering::Acquire) {
            return None;
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        subs.push(Subscriber { id, tx });
        Some((id, rx))
    }

    pub(crate) fn unsubscribe(&self, id: u64) {
        let mut subs = self.subs.lock().unwrap_or_else(|e| e.into_inner());
        subs.retain(|s| s.id != id);
    }

    /// Disconnect current receivers and reject new subscribers.
    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Release);
        let mut subs = self.subs.lock().unwrap_or_else(|e| e.into_inner());
        subs.clear();
    }
}

impl<N: Clone> Fanout<N> {
    /// Deliver to every subscriber. `false` => nobody left, worker should stop.
    pub(crate) fn deliver(&self, item: Result<N, BusError>) -> bool {
        let mut subs = self.subs.lock().unwrap_or_else(|e| e.into_inner());
        subs.retain(|s| s.tx.send(item.clone()).is_ok());
        !subs.is_empty()
    }
}

/// Shared worker + fan-out for one (pad, kind).
///
/// `worker` is in a `Mutex` so this type is `Sync` (`JoinHandle` is not).
pub(crate) struct NotificationHub<N> {
    pub(crate) fanout: Arc<Fanout<N>>,
    worker: Mutex<Option<NotificationWorkerHandle>>,
}

impl<N> NotificationHub<N> {
    pub(crate) fn new(fanout: Arc<Fanout<N>>, worker: NotificationWorkerHandle) -> Self {
        Self {
            fanout,
            worker: Mutex::new(Some(worker)),
        }
    }
}

impl<N> Drop for NotificationHub<N> {
    fn drop(&mut self) {
        // Dropping the handle cancels the IRP and joins the worker.
        let worker = self
            .worker
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        drop(worker);
        // Defensive; the worker also closes on exit.
        self.fanout.close();
    }
}

/// Removes one subscriber from the fan-out on drop.
pub(crate) struct SubscriberToken<N> {
    fanout: Arc<Fanout<N>>,
    id: u64,
}

impl<N> SubscriberToken<N> {
    pub(crate) fn new(fanout: Arc<Fanout<N>>, id: u64) -> Self {
        Self { fanout, id }
    }
}

impl<N> Drop for SubscriberToken<N> {
    fn drop(&mut self) {
        self.fanout.unsubscribe(self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{RecvTimeoutError, TryRecvError};
    use std::time::Duration;

    #[test]
    fn fanout_respects_per_subscriber_capacity() {
        let fanout = Fanout::<u32>::new();
        let (_id_b, rx_b) = fanout
            .subscribe(NotificationBuffer::Bounded { capacity: 1 })
            .expect("open");
        let (_id_u, rx_u) = fanout
            .subscribe(NotificationBuffer::Unbounded)
            .expect("open");

        for i in 0..10 {
            assert!(fanout.deliver(Ok(i)));
        }

        assert_eq!(rx_b.len(), 1);
        assert_eq!(rx_b.dropped_count(), 9);
        assert_eq!(rx_b.try_recv().unwrap().expect("ok"), 9);

        assert_eq!(rx_u.len(), 10);
        assert_eq!(rx_u.dropped_count(), 0);
        for i in 0..10 {
            assert_eq!(rx_u.try_recv().unwrap().expect("ok"), i);
        }
    }

    #[test]
    fn fanout_drops_dead_subscribers() {
        let fanout = Fanout::<u32>::new();
        let (_id1, rx1) = fanout
            .subscribe(NotificationBuffer::Unbounded)
            .expect("open");
        let (_id2, rx2) = fanout
            .subscribe(NotificationBuffer::Unbounded)
            .expect("open");

        drop(rx1);
        assert!(fanout.deliver(Ok(1)), "one live subscriber remains");
        assert_eq!(rx2.try_recv().unwrap().expect("ok"), 1);

        drop(rx2);
        assert!(
            !fanout.deliver(Ok(2)),
            "deliver must return false when the last subscriber is gone"
        );
    }

    #[test]
    fn close_semantics() {
        let fanout = Fanout::<u32>::new();
        let (_id, rx) = fanout
            .subscribe(NotificationBuffer::Unbounded)
            .expect("open");
        fanout.close();
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Disconnected)));
        assert!(matches!(
            rx.recv_timeout(Duration::from_millis(10)),
            Err(RecvTimeoutError::Disconnected)
        ));
        assert!(fanout.subscribe(NotificationBuffer::default()).is_none());
    }

    #[test]
    fn unsubscribe_token_removes_exactly_one() {
        let fanout = Arc::new(Fanout::<u32>::new());
        let (id1, rx1) = fanout
            .subscribe(NotificationBuffer::Unbounded)
            .expect("open");
        let (_id2, rx2) = fanout
            .subscribe(NotificationBuffer::Unbounded)
            .expect("open");

        let token = SubscriberToken::new(Arc::clone(&fanout), id1);
        drop(token);

        assert!(fanout.deliver(Ok(42)));
        assert!(matches!(rx1.try_recv(), Err(TryRecvError::Disconnected)));
        assert_eq!(rx2.try_recv().unwrap().expect("ok"), 42);
    }
}
