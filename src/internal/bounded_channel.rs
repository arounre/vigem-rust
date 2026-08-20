//! Notification channel. Default: bounded, drop-oldest.
//! Unbounded via [`crate::NotificationBuffer::Unbounded`].

use std::collections::VecDeque;
use std::sync::mpsc::{RecvError, RecvTimeoutError, TryRecvError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Default capacity for bounded notification subscriptions.
pub(crate) const NOTIFICATION_CHANNEL_CAPACITY: usize = 4;

struct State<T> {
    buf: VecDeque<T>,
    /// `Some(n)`: drop-oldest when full; `None`: unbounded.
    capacity: Option<usize>,
    /// Live senders; when zero and buffer empty, `recv` disconnects.
    senders: usize,
    receiver_alive: bool,
    /// Dropped sends on a full bounded buffer.
    dropped: u64,
}

struct Shared<T> {
    state: Mutex<State<T>>,
    not_empty: Condvar,
}

pub(crate) struct NotifSender<T> {
    shared: Arc<Shared<T>>,
}

pub(crate) struct NotifReceiver<T> {
    shared: Arc<Shared<T>>,
}

/// Create a channel.
///
/// * `Some(capacity)` - at most `capacity` items (clamped to >= 1); drop-oldest when full.
/// * `None` - unbounded; never drops.
pub(crate) fn channel<T>(capacity: Option<usize>) -> (NotifSender<T>, NotifReceiver<T>) {
    let capacity = capacity.map(|c| c.max(1));
    let reserve = capacity.unwrap_or(0).min(64);
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            buf: VecDeque::with_capacity(reserve),
            capacity,
            senders: 1,
            receiver_alive: true,
            dropped: 0,
        }),
        not_empty: Condvar::new(),
    });
    (
        NotifSender {
            shared: Arc::clone(&shared),
        },
        NotifReceiver { shared },
    )
}

/// Clamps capacity to >= 1.
#[cfg(test)]
pub(crate) fn bounded<T>(capacity: usize) -> (NotifSender<T>, NotifReceiver<T>) {
    channel(Some(capacity))
}

/// Unbounded; never drops.
#[cfg(test)]
pub(crate) fn unbounded<T>() -> (NotifSender<T>, NotifReceiver<T>) {
    channel(None)
}

impl<T> NotifSender<T> {
    /// Drop-oldest when the bounded buffer is full.
    /// `Err(value)` if the receiver is gone.
    pub(crate) fn send(&self, value: T) -> Result<(), T> {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.receiver_alive {
            return Err(value);
        }
        if let Some(cap) = state.capacity
            && state.buf.len() >= cap
        {
            let _oldest = state.buf.pop_front();
            state.dropped = state.dropped.saturating_add(1);
        }
        state.buf.push_back(value);
        drop(state);
        self.shared.not_empty.notify_one();
        Ok(())
    }
}

impl<T> Drop for NotifSender<T> {
    fn drop(&mut self) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.senders = state.senders.saturating_sub(1);
        drop(state);
        // Wake a blocked `recv` so it can observe disconnect.
        self.shared.not_empty.notify_all();
    }
}

impl<T> NotifReceiver<T> {
    /// `None` if unbounded.
    pub(crate) fn capacity(&self) -> Option<usize> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .capacity
    }

    pub(crate) fn len(&self) -> usize {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .buf
            .len()
    }

    /// Drop-oldest discards (bounded only).
    pub(crate) fn dropped_count(&self) -> u64 {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .dropped
    }

    pub(crate) fn recv(&self) -> Result<T, RecvError> {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(value) = state.buf.pop_front() {
                return Ok(value);
            }
            if state.senders == 0 {
                return Err(RecvError);
            }
            state = self
                .shared
                .not_empty
                .wait(state)
                .unwrap_or_else(|e| e.into_inner());
        }
    }

    pub(crate) fn try_recv(&self) -> Result<T, TryRecvError> {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(value) = state.buf.pop_front() {
            return Ok(value);
        }
        if state.senders == 0 {
            Err(TryRecvError::Disconnected)
        } else {
            Err(TryRecvError::Empty)
        }
    }

    pub(crate) fn recv_timeout(&self, timeout: Duration) -> Result<T, RecvTimeoutError> {
        // checked_add: Instant + Duration::MAX panics (same as mpsc).
        let deadline = Instant::now().checked_add(timeout);
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(value) = state.buf.pop_front() {
                return Ok(value);
            }
            if state.senders == 0 {
                return Err(RecvTimeoutError::Disconnected);
            }
            let wait_for = match deadline {
                Some(d) => {
                    let now = Instant::now();
                    if now >= d {
                        return Err(RecvTimeoutError::Timeout);
                    }
                    d - now
                }
                // Overflow: wait in 1h chunks.
                None => Duration::from_secs(3600),
            };
            let (guard, _) = self
                .shared
                .not_empty
                .wait_timeout(state, wait_for)
                .unwrap_or_else(|e| e.into_inner());
            state = guard;
        }
    }

    pub(crate) fn iter(&self) -> NotifIter<'_, T> {
        NotifIter { rx: self }
    }
}

impl<T> Drop for NotifReceiver<T> {
    fn drop(&mut self) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.receiver_alive = false;
        state.buf.clear();
    }
}

pub(crate) struct NotifIter<'a, T> {
    rx: &'a NotifReceiver<T>,
}

impl<T> Iterator for NotifIter<'_, T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        self.rx.recv().ok()
    }
}

// Alias used by the public receiver wrapper.
pub(crate) type BoundedReceiver<T> = NotifReceiver<T>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn capacity_is_respected_under_flood() {
        let (tx, rx) = bounded::<u32>(4);
        for i in 0..10_000 {
            tx.send(i).expect("receiver alive");
        }
        assert_eq!(rx.len(), 4);
        assert_eq!(rx.capacity(), Some(4));
        assert_eq!(rx.dropped_count(), 10_000 - 4);

        let mut got = Vec::new();
        while let Ok(v) = rx.try_recv() {
            got.push(v);
        }
        assert_eq!(got, vec![9_996, 9_997, 9_998, 9_999]);

        // Unbounded counterpart: never drops under the same flood.
        let (tx, rx) = unbounded::<u32>();
        for i in 0..10_000 {
            tx.send(i).expect("receiver alive");
        }
        assert_eq!(rx.len(), 10_000);
        assert_eq!(rx.capacity(), None);
        assert_eq!(rx.dropped_count(), 0);
        assert_eq!(rx.recv().unwrap(), 0);
        assert_eq!(rx.recv().unwrap(), 1);
    }

    #[test]
    fn disconnect_semantics() {
        // Send fails after the receiver is dropped.
        let (tx, rx) = bounded::<u32>(4);
        drop(rx);
        assert_eq!(tx.send(1).err(), Some(1));

        // Empty channel with a live sender is Empty, not Disconnected.
        let (tx, rx) = bounded::<u32>(4);
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
        drop(tx);
        assert!(matches!(rx.recv(), Err(RecvError)));
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Disconnected)));

        // Remaining items drain after the sender drops, then disconnect.
        let (tx, rx) = bounded::<u32>(4);
        tx.send(7).unwrap();
        drop(tx);
        assert_eq!(rx.recv().unwrap(), 7);
        assert!(matches!(rx.recv(), Err(RecvError)));
    }

    /// Also: short timeout, and `Duration::MAX` waits until send.
    #[test]
    fn recv_timeout_with_max_duration_does_not_panic() {
        let (tx, rx) = bounded::<u32>(4);
        let err = rx
            .recv_timeout(Duration::from_millis(20))
            .expect_err("should time out");
        assert!(matches!(err, RecvTimeoutError::Timeout));

        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            tx.send(7).unwrap();
        });
        let value = rx
            .recv_timeout(Duration::MAX)
            .expect("Duration::MAX must wait, not panic");
        assert_eq!(value, 7);
        handle.join().unwrap();
    }

    /// Latest-wins at clamped capacity 1.
    #[test]
    fn zero_capacity_becomes_one() {
        let (tx, rx) = bounded::<u32>(0);
        assert_eq!(rx.capacity(), Some(1));
        tx.send(1).unwrap();
        tx.send(2).unwrap();
        assert_eq!(rx.recv().unwrap(), 2);
        assert_eq!(rx.dropped_count(), 1);
    }
}
