//! # Lifecycle
//!
//! ```text
//! Client::new_*_target()
//!     .plug()             -> TargetHandle<T, Pending>   // plugged, not yet ready
//!     .wait_for_ready()   -> TargetHandle<T, Ready>     // can update / notifications
//! ```
//!
//! You can skip the wait with [`assume_ready`](TargetHandle::assume_ready) if you
//! accept that early reports may do nothing until the host sees the pad.
//!
//! # Who unplugs a pad
//!
//! A pad leaves the bus when any of these happens:
//! - the **last** [`TargetHandle`] clone is dropped
//! - you call [`unplug`](TargetHandle::unplug) or [`Client::unplug_all`]
//! - the **last** [`Client`] clone is dropped
//!
//! Extra clones are just refs; dropping a non-last handle does not touch the bus.
//! The last handle drop and last client drop both unplug **on that thread**, wait
//! on the driver, swallow unplug errors, and do not panic on a poisoned mutex.
//! Last-client drop unplugs remaining pads one by one (on the order of
//! `max_targets` × a few seconds if the bus is wedged). Call
//! [`unplug`](TargetHandle::unplug) at a time you can block.
//!
//! # Sharing
//!
//! [`Client`] and [`TargetHandle`] are `Send + Sync` and cheap to clone. Clone
//! the handle to share one pad across threads; concurrent `update`s run one at
//! a time. [`NotificationReceiver`] is `Send`; move it onto the thread that
//! reads events.

use std::{
    fmt,
    marker::PhantomData,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

#[cfg(feature = "ds4")]
use crate::controller::ds4::{Ds4Notification, Ds4OutputBuffer, Ds4Report, Ds4ReportEx};
#[cfg(feature = "x360")]
use crate::controller::x360::{X360Notification, X360Report};

use crate::{
    client::{Client, ClientInner},
    error::{BusError, ClientError, Operation, RecvError, RecvTimeoutError, TryRecvError},
    internal::{
        bounded_channel::{self, BoundedReceiver},
        bus::{Bus, NotificationWorkerHandle},
        notification_hub::{Fanout, NotificationHub, SubscriberToken},
        overlapped::OverlappedCall,
    },
};

#[inline]
fn is_retryable_ready_error(err: &ClientError) -> bool {
    matches!(err, ClientError::Bus(BusError::TargetNotReady { .. }))
}

/// Default queue size for rumble / LED (and similar) notifications (currently `4`).
///
/// Used by [`NotificationBuffer::default`] and `register_notification()`. When
/// the queue is full, the **oldest** event is dropped so a slow reader does not
/// block new ones.
///
/// Sized for "latest state wins" consumers such as a game loop. To keep every
/// event, use [`NotificationBuffer::Unbounded`], drain promptly, and check
/// [`NotificationReceiver::dropped_count`].
pub const NOTIFICATION_CHANNEL_CAPACITY: usize = bounded_channel::NOTIFICATION_CHANNEL_CAPACITY;

/// How host feedback (rumble, LEDs, ...) is buffered for a subscription.
///
/// Pass to `register_notification_with` / `register_notification_raw_buffer_with`.
/// The default is a small bounded queue that drops the oldest event when full,
/// a good fit for live games.
///
/// # Examples
/// ```
/// use vigem_rust::{NotificationBuffer, NOTIFICATION_CHANNEL_CAPACITY};
///
/// assert_eq!(
///     NotificationBuffer::default(),
///     NotificationBuffer::Bounded {
///         capacity: NOTIFICATION_CHANNEL_CAPACITY
///     }
/// );
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum NotificationBuffer {
    /// Keep at most `capacity` events; drop the oldest when full.
    ///
    /// `capacity == 0` is treated as `1`.
    Bounded {
        /// Max events waiting to be read.
        capacity: usize,
    },
    /// Never drop events. Memory grows if you do not read them.
    ///
    /// Prefer this only for short captures, or when a dedicated reader always
    /// drains the receiver.
    Unbounded,
}

impl Default for NotificationBuffer {
    fn default() -> Self {
        Self::Bounded {
            capacity: NOTIFICATION_CHANNEL_CAPACITY,
        }
    }
}

impl NotificationBuffer {
    pub(crate) fn to_channel_capacity(self) -> Option<usize> {
        match self {
            Self::Bounded { capacity } => Some(capacity.max(1)),
            Self::Unbounded => None,
        }
    }
}

const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(3);

const READY_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Neutral submits in a row before minting [`Ready`].
/// One success can still be followed by [`BusError::TargetNotReady`] under multi-plug.
const READY_CONSECUTIVE_PROBES: u32 = 3;

/// Marker type: the pad is plugged, but not yet ready for input.
///
/// Returned by [`plug`](TargetBuilder::plug). Call
/// [`wait_for_ready`](TargetHandle::wait_for_ready) (preferred) or
/// [`assume_ready`](TargetHandle::assume_ready) before `update` or notifications.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pending;

/// Marker type: this handle can send input and register notifications.
///
/// This is the default type parameter on [`TargetHandle`], so
/// `TargetHandle<Xbox360>` means a ready pad.
///
/// Readiness is not permanent. `wait_for_ready` only checks that a few neutral
/// reports were accepted. If you plug many pads at once, a later `update` can
/// still return [`BusError::TargetNotReady`] briefly. Treat that as retryable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ready;

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TargetType {
    #[cfg(feature = "x360")]
    Xbox360 = 0,
    #[cfg(feature = "ds4")]
    DualShock4 = 2,
}

impl TargetType {
    #[inline]
    // (vendor_id, product_id)
    pub(crate) fn get_identifiers(&self) -> (u16, u16) {
        match *self {
            #[cfg(feature = "x360")]
            TargetType::Xbox360 => (0x045E, 0x028E),
            #[cfg(feature = "ds4")]
            TargetType::DualShock4 => (0x054C, 0x05C4),
        }
    }
}

/// Live virtual target in the client map.
///
/// Identity is `(serial_no, generation)`; serials recycle after unplug.
#[derive(Debug)]
pub(crate) struct Target {
    pub(crate) kind: TargetType,
    pub(crate) serial_no: u32,
    /// Monotonic id at plug; never reused.
    pub(crate) generation: u64,
    pub(crate) vendor_id: u16,
    pub(crate) product_id: u16,
    /// Cleared under the client mutex on unplug / client drop.
    /// Hot paths read with `Acquire` and skip the client mutex.
    pub(crate) alive: Arc<AtomicBool>,
}

/// Zero-sized marker for a virtual Xbox 360 pad (`TargetHandle<Xbox360>`).
///
/// This type has no methods of its own; drive the pad through [`TargetHandle`].
/// Sealed [`Device`] impl; not user-implementable.
#[cfg(feature = "x360")]
#[cfg_attr(docsrs, doc(cfg(feature = "x360")))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Xbox360;

/// Zero-sized marker for a virtual DualShock 4 pad (`TargetHandle<DualShock4>`).
///
/// This type has no methods of its own; drive the pad through [`TargetHandle`].
/// Sealed [`Device`] impl; not user-implementable.
#[cfg(feature = "ds4")]
#[cfg_attr(docsrs, doc(cfg(feature = "ds4")))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DualShock4;

/// Crate-private supertrait: seals [`Device`] and hides `Slots`.
///
/// `Slots` cannot live on the public trait: a public associated type cannot
/// name the `pub(crate)` slot structs (E0446). Keeping it here is what makes
/// each pad store only its own hubs.
mod sealed {
    pub(crate) trait DeviceSlots {
        type Slots: Default + Send + Sync + 'static;
        const KIND: super::TargetType;
    }
}

/// Device kinds a [`TargetHandle`] can be parameterized by.
///
/// Implemented only by the feature-gated `Xbox360` and `DualShock4` markers;
/// sealed, so downstream crates cannot add device kinds.
#[allow(private_bounds)]
pub trait Device: sealed::DeviceSlots + 'static {}

/// Per-pad slots for shared notification hubs (one worker per kind).
///
/// Exactly one [`TargetHandleInner`] exists per `(serial_no, generation)`, so
/// these slots are per-pad, not per-handle.
#[cfg(feature = "x360")]
#[derive(Default)]
pub(crate) struct Xbox360Slots {
    pub(crate) x360: Mutex<Weak<NotificationHub<X360Notification>>>,
}

/// Per-pad slots for shared notification hubs (one worker per kind).
///
/// Exactly one [`TargetHandleInner`] exists per `(serial_no, generation)`, so
/// these slots are per-pad, not per-handle.
#[cfg(feature = "ds4")]
#[derive(Default)]
pub(crate) struct Ds4Slots {
    pub(crate) ds4: Mutex<Weak<NotificationHub<Ds4Notification>>>,
    pub(crate) ds4_raw: Mutex<Weak<NotificationHub<Ds4OutputBuffer>>>,
}

#[cfg(feature = "x360")]
impl sealed::DeviceSlots for Xbox360 {
    type Slots = Xbox360Slots;
    const KIND: TargetType = TargetType::Xbox360;
}
#[cfg(feature = "x360")]
impl Device for Xbox360 {}

#[cfg(feature = "ds4")]
impl sealed::DeviceSlots for DualShock4 {
    type Slots = Ds4Slots;
    const KIND: TargetType = TargetType::DualShock4;
}
#[cfg(feature = "ds4")]
impl Device for DualShock4 {}

/// Explicit test device so `()` is not treated as a pad kind.
///
/// `Slots = ()` is also a compile-time check that a zero-slot device is legal.
#[cfg(test)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct TestPad;

#[cfg(test)]
impl sealed::DeviceSlots for TestPad {
    type Slots = ();
    #[cfg(feature = "x360")]
    const KIND: TargetType = TargetType::Xbox360;
    #[cfg(all(not(feature = "x360"), feature = "ds4"))]
    const KIND: TargetType = TargetType::DualShock4;
}
#[cfg(test)]
impl Device for TestPad {}

/// Shared state for one pad. Every [`TargetHandle`] clone holds an `Arc` of this.
///
/// Generic over device kind `T`, not readiness `S`. `Pending` -> `Ready` only
/// retags the outer handle so we can reuse this `Arc`; `Drop` unplugs the pad.
struct TargetHandleInner<T: Device> {
    serial_no: u32,
    generation: u64,
    /// Shared with the matching [`Target`] in the client map.
    alive: Arc<AtomicBool>,
    bus: Bus,
    client_inner: Weak<Mutex<ClientInner>>,
    /// Cached `OVERLAPPED` + event, created on first submit (`update`,
    /// `update_ex`, or a ready probe) so each call does not `CreateEventW`.
    ///
    /// Mutex: cloned handles share this call; Windows allows one IRP per event.
    submit: Mutex<Option<OverlappedCall>>,
    notifications: <T as sealed::DeviceSlots>::Slots,
}

impl<T: Device> Drop for TargetHandleInner<T> {
    fn drop(&mut self) {
        // Unplug only if this generation is still live.
        let Some(inner_arc) = self.client_inner.upgrade() else {
            self.alive.store(false, Ordering::Release);
            return;
        };
        // Reserve serial across the unplug IOCTL so concurrent plug cannot reuse it.
        // Recover from poison; unplug outside the lock (slow IOCTL).
        if let Some(guard) = crate::client::begin_unplug_recover(
            &inner_arc,
            self.serial_no,
            self.generation,
            &self.alive,
        ) {
            let _ = self.bus.unplug(self.serial_no);
            drop(guard);
        }
    }
}

/// A plugged-in virtual controller you drive from your code.
///
/// - `T`: [`Device`] marker (`Xbox360` or `DualShock4`, feature-gated)
/// - `S`: readiness ([`Pending`] or [`Ready`]; defaults to [`Ready`], so
///   `TargetHandle<Xbox360>` means a pad you can drive)
///
/// # Getting a ready handle
///
/// ```no_run
/// # use vigem_rust::Client;
/// # use std::error::Error;
/// # fn main() -> Result<(), Box<dyn Error>> {
/// # let client = Client::connect()?;
#[cfg_attr(
    feature = "x360",
    doc = "let pad = client.new_x360_target().plug()?.wait_for_ready()?;\n// pad: TargetHandle<Xbox360>  (Ready is the default)"
)]
#[cfg_attr(
    all(feature = "ds4", not(feature = "x360")),
    doc = "let pad = client.new_ds4_target().plug()?.wait_for_ready()?;\n// pad: TargetHandle<DualShock4>  (Ready is the default)"
)]
/// # let _ = pad;
/// # Ok(())
/// # }
/// ```
///
/// `plug` returns a [`Pending`] handle. Call `wait_for_ready` (or
/// [`assume_ready`](Self::assume_ready)) to get a [`Ready`] handle, which unlocks
/// `update` and `register_notification`.
///
/// # Threads and drop
///
/// `TargetHandle` is `Send + Sync` and cheap to clone (`Arc`). A clone is the
/// same pad; concurrent `update`s run one at a time.
///
/// The pad stays connected until the **last** handle clone is dropped, you call
/// [`unplug`](Self::unplug), or the last owning [`Client`] is dropped. The last
/// handle unplugs **on that thread** and waits on the driver. Drop swallows
/// unplug errors and does not panic if the client mutex is poisoned. See
/// [who unplugs a pad](crate::target#who-unplugs-a-pad).
///
/// After unplug, further use returns [`ClientError::TargetDoesNotExist`]. After
/// the last `Client` is gone, [`is_attached`](Self::is_attached) returns
/// [`ClientError::ClientNoLongerExists`] instead.
pub struct TargetHandle<T: Device, S = Ready> {
    inner: Arc<TargetHandleInner<T>>,
    _state: PhantomData<fn() -> S>,
}

/// Last path segment of `type_name` (`vigem_rust::target::Xbox360` -> `Xbox360`).
fn type_name_leaf<T: ?Sized>() -> &'static str {
    let name = std::any::type_name::<T>();
    name.rsplit("::").next().unwrap_or(name)
}

impl<T: Device, S> fmt::Debug for TargetHandle<T, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TargetHandle")
            .field("device", &type_name_leaf::<T>())
            .field("state", &type_name_leaf::<S>())
            .field("serial_no", &self.inner.serial_no)
            .field("generation", &self.inner.generation)
            .field("alive", &self.inner.alive.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl<T: Device, S> Clone for TargetHandle<T, S> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            _state: PhantomData,
        }
    }
}

impl<T: Device, S> TargetHandle<T, S> {
    pub(crate) fn new(
        serial_no: u32,
        generation: u64,
        alive: Arc<AtomicBool>,
        bus: Bus,
        client_inner: Weak<Mutex<ClientInner>>,
    ) -> Self {
        Self {
            inner: Arc::new(TargetHandleInner {
                serial_no,
                generation,
                alive,
                bus,
                client_inner,
                submit: Mutex::new(None),
                notifications: Default::default(),
            }),
            _state: PhantomData,
        }
    }

    /// Run `f` with the per-handle cached [`OverlappedCall`] (created on first use).
    ///
    /// Concurrent callers serialize on the mutex. A drain-timeout poisoned call is
    /// dropped (kernel-owned event/`OVERLAPPED` already leaked by design) and
    /// replaced so later `update`s are not wedged forever. Mutex poison is recovered
    /// here (cached call only); the client map mutex still maps poison to
    /// [`ClientError::Poisoned`].
    fn with_submit_call<R>(
        &self,
        f: impl FnOnce(&mut OverlappedCall) -> Result<R, BusError>,
    ) -> Result<R, ClientError> {
        let mut guard = self.inner.submit.lock().unwrap_or_else(|e| e.into_inner());
        if guard.as_ref().is_some_and(OverlappedCall::is_poisoned) {
            let _ = guard.take();
        }
        if guard.is_none() {
            let call = OverlappedCall::new(Arc::clone(&self.inner.bus.inner)).map_err(|e| {
                BusError::from_ioctl(Operation::CREATE_EVENT_W, Some(self.inner.serial_no), e)
            })?;
            *guard = Some(call);
        }
        f(guard.as_mut().expect("submit call just inserted")).map_err(Into::into)
    }

    /// Precondition form of [`Self::is_attached`]: `Ok(false)` becomes
    /// [`ClientError::TargetDoesNotExist`] so callers can `?`.
    fn assert_live(&self) -> Result<(), ClientError> {
        if self.is_attached()? {
            Ok(())
        } else {
            Err(ClientError::TargetDoesNotExist {
                serial_no: self.inner.serial_no,
            })
        }
    }

    /// Acquire-load of the liveness flag (hot path; no client mutex).
    #[inline]
    fn assert_alive_hot(&self) -> Result<(), ClientError> {
        if self.inner.alive.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(ClientError::TargetDoesNotExist {
                serial_no: self.inner.serial_no,
            })
        }
    }

    /// Whether this handle still refers to a pad that is plugged in.
    ///
    /// Returns `Ok(false)` after unplug (including drop of the last handle clone).
    /// That is different from "the client is gone" (see Errors).
    ///
    /// # Errors
    ///
    /// - [`ClientError::ClientNoLongerExists`]: the owning client was dropped
    /// - [`ClientError::Poisoned`]: another thread panicked while holding client state
    pub fn is_attached(&self) -> Result<bool, ClientError> {
        if let Some(inner_arc) = self.inner.client_inner.upgrade() {
            let inner = inner_arc.lock().map_err(|_| ClientError::Poisoned)?;
            Ok(inner
                .targets
                .get(&self.inner.serial_no)
                .is_some_and(|t| t.generation == self.inner.generation))
        } else {
            Err(ClientError::ClientNoLongerExists)
        }
    }

    /// Unplug this pad now, even if other handle clones still exist.
    ///
    /// After this, `update` and notifications on this handle and its clones fail
    /// with [`ClientError::TargetDoesNotExist`].
    ///
    /// # Errors
    ///
    /// - [`ClientError::ClientNoLongerExists`]: the client was already dropped
    /// - [`ClientError::TargetDoesNotExist`]: already unplugged
    /// - [`ClientError::Bus`]: the driver failed the unplug request
    pub fn unplug(&self) -> Result<(), ClientError> {
        let Some(inner_arc) = self.inner.client_inner.upgrade() else {
            return Err(ClientError::ClientNoLongerExists);
        };
        let Some(guard) = crate::client::begin_unplug(
            &inner_arc,
            self.inner.serial_no,
            self.inner.generation,
            &self.inner.alive,
        )?
        else {
            return Err(ClientError::TargetDoesNotExist {
                serial_no: self.inner.serial_no,
            });
        };
        let result = self.inner.bus.unplug(self.inner.serial_no);
        drop(guard);
        result.map_err(Into::into)
    }

    /// Bus slot id assigned when this pad was plugged (`1..=`[`crate::SERIAL_SCAN_LIMIT`]).
    ///
    /// Not a USB serial string. Values are from a machine-wide pool and can be
    /// reused after unplug. See [serials](crate::client#serials-and-other-processes).
    #[inline]
    pub fn serial_no(&self) -> u32 {
        self.inner.serial_no
    }
}

impl<T: Device> TargetHandle<T, Pending> {
    /// Treat the pad as ready without waiting.
    ///
    /// Prefer [`wait_for_ready`](TargetHandle::wait_for_ready) in normal apps.
    /// Use this only when you want to skip the wait and accept that early
    /// `update`s may do nothing until the host has seen the controller.
    ///
    /// # Choosing
    ///
    /// | Call | Blocks | Guarantee |
    /// |---|---|---|
    /// | [`wait_for_ready`](TargetHandle::wait_for_ready) | up to ~3 s | pad accepted several reports in a row |
    /// | `wait_for_ready_timeout(Duration::ZERO)` | one quick check | pad accepted one report |
    /// | `assume_ready` | no | none; early `update`s may be ignored |
    #[must_use]
    pub fn assume_ready(self) -> TargetHandle<T, Ready> {
        self.into_ready()
    }

    /// Move the shared identity into a `Ready` handle without dropping it.
    fn into_ready(self) -> TargetHandle<T, Ready> {
        TargetHandle {
            inner: self.inner,
            _state: PhantomData,
        }
    }

    /// Clone the shared identity into a `Ready` handle; `self` stays `Pending`.
    fn as_ready(&self) -> TargetHandle<T, Ready> {
        TargetHandle {
            inner: Arc::clone(&self.inner),
            _state: PhantomData,
        }
    }

    /// Retry `probe` until it succeeds enough times, or the wait gives up.
    ///
    /// `timeout == 0` is one probe: `Ok` -> Ready, `TargetNotReady` ->
    /// [`ClientError::ReadyTimeout`]. Non-zero needs [`READY_CONSECUTIVE_PROBES`]
    /// consecutive `Ok`s; any `TargetNotReady` resets the streak. The deadline
    /// is checked only after a retryable error (`checked_add` overflow = no
    /// deadline). Successes do not sleep. The first miss is retried immediately;
    /// later misses back off by [`READY_POLL_INTERVAL`].
    ///
    /// Takes `&self` so a timeout does not drop this handle (and therefore does
    /// not unplug the pad). Success returns a `Ready` clone of the same identity.
    ///
    /// `probe` receives `&self` so it can reuse the per-handle cached
    /// [`OverlappedCall`] (via [`Self::with_submit_call`]) instead of
    /// `CreateEventW` on every poll.
    fn wait_with_probe(
        &self,
        timeout: Duration,
        mut probe: impl FnMut(&Self) -> Result<(), ClientError>,
    ) -> Result<TargetHandle<T, Ready>, ClientError> {
        self.assert_live()?;

        let need = if timeout.is_zero() {
            1
        } else {
            READY_CONSECUTIVE_PROBES
        };

        // First probe before the deadline. Zero-timeout returns here (`need == 1`).
        let mut consecutive = match probe(self) {
            Ok(()) => {
                if need == 1 {
                    return Ok(self.as_ready());
                }
                1u32
            }
            Err(e) if timeout.is_zero() && is_retryable_ready_error(&e) => {
                return Err(ClientError::ReadyTimeout { timeout });
            }
            Err(e) if is_retryable_ready_error(&e) => 0u32,
            Err(e) => return Err(e),
        };

        // Overflow (near `Duration::MAX`) means no deadline, same as
        // `mpsc::Receiver::recv_timeout`.
        let deadline = Instant::now().checked_add(timeout);
        loop {
            self.assert_live()?;
            match probe(self) {
                Ok(()) => {
                    consecutive = consecutive.saturating_add(1);
                    if consecutive >= need {
                        return Ok(self.as_ready());
                    }
                    // No sleep on success; the happy path is consecutive submits.
                }
                Err(e)
                    if is_retryable_ready_error(&e)
                        && deadline.is_none_or(|d| Instant::now() < d) =>
                {
                    consecutive = 0;
                    thread::sleep(READY_POLL_INTERVAL);
                }
                Err(e) if is_retryable_ready_error(&e) => {
                    return Err(ClientError::ReadyTimeout { timeout });
                }
                Err(e) => return Err(e),
            }
        }
    }
}

#[cfg(feature = "x360")]
#[cfg_attr(docsrs, doc(cfg(feature = "x360")))]
impl TargetHandle<Xbox360, Pending> {
    /// Wait until the pad accepts input (default timeout: 3 seconds).
    ///
    /// Sends neutral reports until several succeed in a row, then returns a
    /// [`Ready`] handle. Call this after `plug` before `update` or
    /// `register_notification`.
    ///
    /// Takes `&self`: a timeout does **not** drop this handle or unplug the pad.
    /// Bind the pending handle if you want to retry or call
    /// [`assume_ready`](Self::assume_ready) afterward.
    ///
    /// # Errors
    ///
    /// - [`ClientError::ReadyTimeout`]: timed out while the pad was still warming up
    /// - [`ClientError::Bus`] / [`ClientError::TargetDoesNotExist`]: hard failures
    ///
    /// After success, `update` should work in normal use. If you plug many pads
    /// at once, a brief [`BusError::TargetNotReady`] on a later `update` is still
    /// possible. A short retry is fine.
    pub fn wait_for_ready(&self) -> Result<TargetHandle<Xbox360, Ready>, ClientError> {
        self.wait_for_ready_timeout(DEFAULT_READY_TIMEOUT)
    }

    /// Same as [`wait_for_ready`](Self::wait_for_ready), with your own timeout.
    ///
    /// | `timeout` | Behavior |
    /// |-----------|----------|
    /// | `> 0` | Wait until several neutral reports succeed in a row, or fail with [`ReadyTimeout`](ClientError::ReadyTimeout) |
    /// | `0` | One probe only (fast, weaker check) |
    ///
    /// Takes `&self`; see [`wait_for_ready`](Self::wait_for_ready) for recovery.
    ///
    /// # Errors
    ///
    /// Same as [`wait_for_ready`](Self::wait_for_ready).
    pub fn wait_for_ready_timeout(
        &self,
        timeout: Duration,
    ) -> Result<TargetHandle<Xbox360, Ready>, ClientError> {
        let report = X360Report::default();
        self.wait_with_probe(timeout, |this| {
            this.with_submit_call(|call| {
                this.inner
                    .bus
                    .update_x360_on(call, this.inner.serial_no, &report)
            })
        })
    }
}

#[cfg(feature = "ds4")]
#[cfg_attr(docsrs, doc(cfg(feature = "ds4")))]
impl TargetHandle<DualShock4, Pending> {
    /// Wait until the pad accepts input (default timeout: 3 seconds).
    ///
    /// Same contract as the Xbox 360
    /// [`wait_for_ready`](TargetHandle::wait_for_ready): probe with neutral
    /// [`Ds4Report`]s. DS4 often rejects input briefly after plug.
    ///
    /// Takes `&self`: a timeout does **not** drop this handle or unplug the pad.
    ///
    /// # Errors
    ///
    /// Same as the Xbox 360 variant: [`ClientError::ReadyTimeout`] on timeout;
    /// [`ClientError::Bus`] / [`ClientError::TargetDoesNotExist`] on hard failure.
    pub fn wait_for_ready(&self) -> Result<TargetHandle<DualShock4, Ready>, ClientError> {
        self.wait_for_ready_timeout(DEFAULT_READY_TIMEOUT)
    }

    /// Same as [`wait_for_ready`](Self::wait_for_ready), with your own timeout.
    ///
    /// `timeout == 0` is a single probe. Non-zero waits for several successes in a
    /// row. After success, a brief [`BusError::TargetNotReady`] on a later `update`
    /// is still possible when plugging many pads at once.
    ///
    /// Takes `&self`; see [`wait_for_ready`](Self::wait_for_ready) for recovery.
    ///
    /// # Errors
    ///
    /// Same as [`wait_for_ready`](Self::wait_for_ready).
    pub fn wait_for_ready_timeout(
        &self,
        timeout: Duration,
    ) -> Result<TargetHandle<DualShock4, Ready>, ClientError> {
        let report = Ds4Report::default();
        self.wait_with_probe(timeout, |this| {
            this.with_submit_call(|call| {
                this.inner
                    .bus
                    .update_ds4_on(call, this.inner.serial_no, &report)
            })
        })
    }
}

/// Incoming host feedback for one pad (rumble, LEDs, lightbar, ...).
///
/// Created by [`TargetHandle::register_notification`] (Xbox 360 or DualShock 4)
/// or the DS4 raw-buffer variant.
/// Method names match [`std::sync::mpsc::Receiver`]
/// ([`recv`](Self::recv), [`try_recv`](Self::try_recv),
/// [`recv_timeout`](Self::recv_timeout), [`iter`](Self::iter)),
/// but each call is a single [`Result`]: `Ok(n)` is host feedback.
///
/// ```
/// # use vigem_rust::NotificationReceiver;
/// fn take_until_disconnect<N>(rx: &NotificationReceiver<N>) {
///     while let Ok(_n) = rx.recv() {}
/// }
/// ```
///
/// # Threads and drop
///
/// `NotificationReceiver` is `Send`. Move it onto the thread that reads events
/// (`recv` takes `&self`). Dropping the **last** receiver stops the worker.
///
/// # Multiple subscribers
///
/// You can register more than once on the same pad. Every subscriber receives
/// every event. Each subscription has its own [`NotificationBuffer`]. One can
/// be unbounded while another is `Bounded { capacity: 1 }`. A slow reader only
/// drops events on its own queue (see [`dropped_count`](Self::dropped_count)).
///
/// # Lifetime
///
/// Delivery starts with the first subscription and stops when the **last**
/// receiver is dropped, when the pad is unplugged, or when the client is dropped.
/// That also means you will not keep receiving another pad's data if the serial
/// is reused later.
///
/// A driver failure arrives as [`RecvError::Bus`] (and the matching try/timeout
/// variants). It is often terminal; remaining subscribers see the same error,
/// then [`RecvError::Disconnected`].
pub struct NotificationReceiver<N> {
    /// Drops first so this subscription is removed before the shared hub tears down.
    _sub: SubscriberToken<N>,
    /// Keeps shared delivery alive while any receiver still exists.
    _hub: Arc<NotificationHub<N>>,
    rx: BoundedReceiver<Result<N, BusError>>,
}

impl<N> fmt::Debug for NotificationReceiver<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NotificationReceiver")
            .field("len", &self.rx.len())
            .field("capacity", &self.rx.capacity())
            .field("dropped", &self.rx.dropped_count())
            .finish_non_exhaustive()
    }
}

impl<N> NotificationReceiver<N> {
    /// Block until the next event, or until delivery ends.
    ///
    /// # Errors
    ///
    /// - [`RecvError::Disconnected`]: the worker stopped (pad unplugged, client
    ///   dropped, last receiver dropped, or a fatal bus error closed the hub)
    /// - [`RecvError::Bus`]: a driver failure while listening for feedback
    pub fn recv(&self) -> Result<N, RecvError> {
        match self.rx.recv() {
            Ok(Ok(n)) => Ok(n),
            Ok(Err(e)) => Err(RecvError::Bus(e)),
            Err(_) => Err(RecvError::Disconnected),
        }
    }

    /// Return immediately with an event, empty, or disconnected.
    ///
    /// # Errors
    ///
    /// - [`TryRecvError::Empty`]: nothing waiting
    /// - [`TryRecvError::Disconnected`]: the worker stopped (pad unplugged,
    ///   client dropped, last receiver dropped, or a fatal bus error closed
    ///   the hub)
    /// - [`TryRecvError::Bus`]: a driver failure while listening for feedback
    pub fn try_recv(&self) -> Result<N, TryRecvError> {
        match self.rx.try_recv() {
            Ok(Ok(n)) => Ok(n),
            Ok(Err(e)) => Err(TryRecvError::Bus(e)),
            Err(mpsc::TryRecvError::Empty) => Err(TryRecvError::Empty),
            Err(mpsc::TryRecvError::Disconnected) => Err(TryRecvError::Disconnected),
        }
    }

    /// Like [`recv`](Self::recv), but give up after `timeout`.
    ///
    /// # Errors
    ///
    /// - [`RecvTimeoutError::Timeout`]: `timeout` elapsed with no event
    /// - [`RecvTimeoutError::Disconnected`]: the worker stopped (pad unplugged,
    ///   client dropped, last receiver dropped, or a fatal bus error closed
    ///   the hub)
    /// - [`RecvTimeoutError::Bus`]: a driver failure while listening for feedback
    pub fn recv_timeout(&self, timeout: Duration) -> Result<N, RecvTimeoutError> {
        match self.rx.recv_timeout(timeout) {
            Ok(Ok(n)) => Ok(n),
            Ok(Err(e)) => Err(RecvTimeoutError::Bus(e)),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(RecvTimeoutError::Timeout),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(RecvTimeoutError::Disconnected),
        }
    }

    /// Blocking iterator of events until delivery ends.
    ///
    /// Yields `Ok(notification)` or `Err(bus)` until the worker disconnects
    /// (then the iterator ends). Same items as [`recv`](Self::recv), with
    /// disconnect as `None` instead of [`RecvError::Disconnected`].
    pub fn iter(&self) -> impl Iterator<Item = Result<N, BusError>> + '_ {
        self.rx.iter()
    }

    /// Max queue length for a bounded buffer, or `None` if unbounded.
    #[must_use]
    pub fn capacity(&self) -> Option<usize> {
        self.rx.capacity()
    }

    /// Whether this subscription uses a bounded (drop-oldest) buffer.
    #[must_use]
    pub fn is_bounded(&self) -> bool {
        self.rx.capacity().is_some()
    }

    /// How many events are waiting to be read.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rx.len()
    }

    /// Whether the queue is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rx.len() == 0
    }

    /// How many events were discarded because the bounded queue was full.
    ///
    /// Always `0` for [`NotificationBuffer::Unbounded`].
    #[must_use]
    pub fn dropped_count(&self) -> u64 {
        self.rx.dropped_count()
    }
}

/// Join an existing shared worker or spawn a fresh one for this (pad, kind).
///
/// Holds the slot lock for the whole call so two threads cannot race two
/// workers onto one pad. Spawning only waits on the worker's startup ack
/// (`CreateEventW`), which never touches the slot.
fn subscribe_or_spawn<N, F>(
    slot: &Mutex<Weak<NotificationHub<N>>>,
    buffer: NotificationBuffer,
    start: F,
) -> Result<NotificationReceiver<N>, ClientError>
where
    N: Clone + Send + 'static,
    F: FnOnce(Arc<Fanout<N>>) -> Result<NotificationWorkerHandle, BusError>,
{
    let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());

    if let Some(hub) = guard.upgrade()
        && let Some((id, rx)) = hub.fanout.subscribe(buffer)
    {
        return Ok(NotificationReceiver {
            _sub: SubscriberToken::new(Arc::clone(&hub.fanout), id),
            _hub: Arc::clone(&hub),
            rx,
        });
    }

    // Subscribe before `start` so an immediate worker completion cannot close
    // an empty fan-out (that panics on `expect` below when re-registering).
    let fanout = Arc::new(Fanout::new());
    let (id, rx) = fanout
        .subscribe(buffer)
        .expect("fresh fanout must accept subscribers");
    let worker = start(Arc::clone(&fanout))?;
    let hub = Arc::new(NotificationHub::new(Arc::clone(&fanout), worker));
    *guard = Arc::downgrade(&hub);
    Ok(NotificationReceiver {
        _sub: SubscriberToken::new(fanout, id),
        _hub: hub,
        rx,
    })
}

#[cfg(feature = "x360")]
#[cfg_attr(docsrs, doc(cfg(feature = "x360")))]
impl TargetHandle<Xbox360, Ready> {
    /// XInput-style user index reported by the driver.
    ///
    /// Often stays `0` even after a game assigns a player. For the player LED
    /// number shown on a real pad, prefer
    /// [`register_notification`](Self::register_notification) and
    /// [`X360Notification::led_number`].
    ///
    /// # Errors
    ///
    /// - [`ClientError::TargetDoesNotExist`]: this pad was unplugged
    /// - [`ClientError::ClientNoLongerExists`]: the owning client was dropped
    /// - [`ClientError::Poisoned`]: another thread panicked while holding client state
    /// - [`ClientError::Bus`]: driver failure
    pub fn user_index(&self) -> Result<u32, ClientError> {
        self.assert_live()?;
        let index = self.inner.bus.get_x360_user_index(self.inner.serial_no)?;
        Ok(index)
    }

    /// Alias for [`user_index`](Self::user_index).
    ///
    /// # Errors
    ///
    /// Same as [`user_index`](Self::user_index).
    #[deprecated(since = "0.2.0", note = "renamed to `user_index`")]
    #[inline]
    pub fn get_user_index(&self) -> Result<u32, ClientError> {
        self.user_index()
    }

    /// Listen for rumble and player-LED updates from the host (default buffer).
    ///
    /// You can register more than once; every subscriber gets every event.
    /// Delivery stops when the last [`NotificationReceiver`] is dropped, or when
    /// you unplug this pad / drop the client. The default queue is small and
    /// drops the oldest event when full; see
    /// [`register_notification_with`](Self::register_notification_with) to change that.
    ///
    /// # Errors
    ///
    /// - [`ClientError::TargetDoesNotExist`]: this pad was unplugged
    /// - [`ClientError::ClientNoLongerExists`]: the owning client was dropped
    /// - [`ClientError::Poisoned`]: another thread panicked while holding client state
    /// - [`ClientError::Bus`]: failed to start listening for host feedback
    ///
    /// # Example
    /// ```no_run
    /// # use vigem_rust::Client;
    /// # use std::error::Error;
    /// # fn main() -> Result<(), Box<dyn Error>> {
    /// # let client = Client::connect()?;
    /// # let x360 = client.new_x360_target().plug()?.wait_for_ready()?;
    /// let receiver = x360.register_notification()?;
    /// if let Ok(n) = receiver.try_recv() {
    ///     println!("LED {}  rumble L={} R={}", n.led_number, n.large_motor, n.small_motor);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn register_notification(
        &self,
    ) -> Result<NotificationReceiver<X360Notification>, ClientError> {
        self.register_notification_with(NotificationBuffer::default())
    }

    /// Same as [`register_notification`](Self::register_notification), with an
    /// explicit [`NotificationBuffer`]. Each subscriber has its own buffer; a
    /// slow reader only drops its own events.
    ///
    /// # Errors
    ///
    /// Same as [`register_notification`](Self::register_notification).
    pub fn register_notification_with(
        &self,
        buffer: NotificationBuffer,
    ) -> Result<NotificationReceiver<X360Notification>, ClientError> {
        self.assert_live()?;
        let bus = self.inner.bus.clone();
        let serial = self.inner.serial_no;
        let alive = Arc::clone(&self.inner.alive);
        subscribe_or_spawn(&self.inner.notifications.x360, buffer, move |fanout| {
            bus.start_x360_notification_thread(serial, alive, fanout)
        })
    }

    /// Send the current button/axis state to the host.
    ///
    /// Build an [`X360Report`], then call this each frame (or whenever input
    /// changes). Concurrent `update` calls on the same pad run one at a time.
    ///
    /// # Errors
    ///
    /// - [`ClientError::TargetDoesNotExist`]: this pad was unplugged
    /// - [`ClientError::Bus`]: driver failure; [`BusError::TargetNotReady`] is
    ///   transient and worth a short retry
    ///
    /// # Example
    /// ```no_run
    /// # use vigem_rust::{Client, X360Report, X360Button};
    /// # use std::error::Error;
    /// # fn main() -> Result<(), Box<dyn Error>> {
    /// # let client = Client::connect()?;
    /// # let x360 = client.new_x360_target().plug()?.wait_for_ready()?;
    /// let mut report = X360Report::default();
    /// report.buttons = X360Button::A | X360Button::START;
    /// report.thumb_lx = 16384;
    /// x360.update(&report)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn update(&self, report: &X360Report) -> Result<(), ClientError> {
        self.assert_alive_hot()?;
        self.with_submit_call(|call| {
            self.inner
                .bus
                .update_x360_on(call, self.inner.serial_no, report)
        })
    }
}

#[cfg(feature = "ds4")]
#[cfg_attr(docsrs, doc(cfg(feature = "ds4")))]
impl TargetHandle<DualShock4, Ready> {
    /// Listen for rumble and lightbar commands from the host (default buffer).
    ///
    /// You can register more than once; every subscriber gets every event. Drop
    /// the last receiver or unplug the pad to stop. For the full 64-byte host
    /// output report, use
    /// [`register_notification_raw_buffer`](Self::register_notification_raw_buffer).
    ///
    /// **Note:** `register_notification` and `register_notification_raw_buffer`
    /// are separate streams. Prefer one kind per pad. Using both at once may
    /// split host output between them depending on the driver version.
    ///
    /// # Errors
    ///
    /// - [`ClientError::TargetDoesNotExist`]: this pad was unplugged
    /// - [`ClientError::ClientNoLongerExists`]: the owning client was dropped
    /// - [`ClientError::Poisoned`]: another thread panicked while holding client state
    /// - [`ClientError::Bus`]: failed to start listening for host feedback
    ///
    /// # Example
    /// ```no_run
    /// # use vigem_rust::Client;
    /// # use std::error::Error;
    /// # fn main() -> Result<(), Box<dyn Error>> {
    /// # let client = Client::connect()?;
    /// # let ds4 = client.new_ds4_target().plug()?.wait_for_ready()?;
    /// let receiver = ds4.register_notification()?;
    /// if let Ok(n) = receiver.try_recv() {
    ///     println!("lightbar {:?}", n.lightbar);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn register_notification(
        &self,
    ) -> Result<NotificationReceiver<Ds4Notification>, ClientError> {
        self.register_notification_with(NotificationBuffer::default())
    }

    /// Same as [`register_notification`](Self::register_notification), with an
    /// explicit [`NotificationBuffer`]. Each subscriber has its own buffer.
    ///
    /// # Errors
    ///
    /// Same as [`register_notification`](Self::register_notification).
    pub fn register_notification_with(
        &self,
        buffer: NotificationBuffer,
    ) -> Result<NotificationReceiver<Ds4Notification>, ClientError> {
        self.assert_live()?;
        let bus = self.inner.bus.clone();
        let serial = self.inner.serial_no;
        let alive = Arc::clone(&self.inner.alive);
        subscribe_or_spawn(&self.inner.notifications.ds4, buffer, move |fanout| {
            bus.start_ds4_notification_thread(serial, alive, fanout)
        })
    }

    /// Listen for raw 64-byte DS4 output reports from the host (default buffer).
    ///
    /// Use this when you need more than rumble and lightbar. For the common case,
    /// prefer [`register_notification`](Self::register_notification). Delivery
    /// stops when the last receiver is dropped or this pad is unplugged.
    ///
    /// See [`register_notification`](Self::register_notification) for the note
    /// about using both kinds on one pad.
    ///
    /// # Errors
    ///
    /// - [`ClientError::TargetDoesNotExist`]: this pad was unplugged
    /// - [`ClientError::ClientNoLongerExists`]: the owning client was dropped
    /// - [`ClientError::Poisoned`]: another thread panicked while holding client state
    /// - [`ClientError::Bus`]: failed to start listening for host feedback
    pub fn register_notification_raw_buffer(
        &self,
    ) -> Result<NotificationReceiver<Ds4OutputBuffer>, ClientError> {
        self.register_notification_raw_buffer_with(NotificationBuffer::default())
    }

    /// Same as [`register_notification_raw_buffer`](Self::register_notification_raw_buffer),
    /// with an explicit [`NotificationBuffer`].
    ///
    /// # Errors
    ///
    /// Same as [`register_notification_raw_buffer`](Self::register_notification_raw_buffer).
    pub fn register_notification_raw_buffer_with(
        &self,
        buffer: NotificationBuffer,
    ) -> Result<NotificationReceiver<Ds4OutputBuffer>, ClientError> {
        self.assert_live()?;
        let bus = self.inner.bus.clone();
        let serial = self.inner.serial_no;
        let alive = Arc::clone(&self.inner.alive);
        subscribe_or_spawn(&self.inner.notifications.ds4_raw, buffer, move |fanout| {
            bus.start_ds4_output_thread(serial, alive, fanout)
        })
    }

    /// Send a standard DS4 input report (buttons, sticks, triggers).
    ///
    /// For gyro / accel / touchpad, use [`update_ex`](Self::update_ex) with
    /// [`Ds4ReportEx`].
    ///
    /// # Errors
    ///
    /// - [`ClientError::TargetDoesNotExist`]: this pad was unplugged
    /// - [`ClientError::Bus`]: driver failure; [`BusError::TargetNotReady`] is
    ///   transient and worth a short retry
    ///
    /// # Example
    /// ```no_run
    /// # use vigem_rust::{Client, Ds4Report, Ds4ButtonFlags, Ds4Dpad};
    /// # use std::error::Error;
    /// # fn main() -> Result<(), Box<dyn Error>> {
    /// # let client = Client::connect()?;
    /// # let ds4 = client.new_ds4_target().plug()?.wait_for_ready()?;
    /// let mut report = Ds4Report::default();
    /// report.buttons |= Ds4ButtonFlags::CROSS;
    /// report.set_dpad(Ds4Dpad::South);
    /// report.trigger_right = 255;
    /// report.buttons |= Ds4ButtonFlags::TRIGGER_RIGHT; // digital bit too
    /// ds4.update(&report)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn update(&self, report: &Ds4Report) -> Result<(), ClientError> {
        self.assert_alive_hot()?;
        self.with_submit_call(|call| {
            self.inner
                .bus
                .update_ds4_on(call, self.inner.serial_no, report)
        })
    }

    /// Send an extended DS4 report (standard inputs plus motion and touchpad).
    ///
    /// Needs a ViGEmBus build that supports extended DS4 reports. Older drivers
    /// may fail with [`BusError::Ioctl`]. Prefer a current
    /// [ViGEmBus](https://vigem.org) install if you need touch or motion.
    ///
    /// Concurrent submits on the same pad run one at a time.
    ///
    /// # Errors
    ///
    /// - [`ClientError::TargetDoesNotExist`]: this pad was unplugged
    /// - [`ClientError::Bus`]: driver failure; [`BusError::TargetNotReady`] is
    ///   transient and worth a short retry; [`BusError::Ioctl`] if the driver
    ///   lacks extended-report support
    ///
    /// # Example
    /// ```no_run
    /// # use vigem_rust::{Client, Ds4ReportEx};
    /// # use std::error::Error;
    /// # fn main() -> Result<(), Box<dyn Error>> {
    /// # let client = Client::connect()?;
    /// # let ds4 = client.new_ds4_target().plug()?.wait_for_ready()?;
    /// let mut report_ex = Ds4ReportEx::default();
    /// report_ex.gyro_x = 12345;
    /// report_ex.thumb_lx = 200;
    /// ds4.update_ex(&report_ex)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn update_ex(&self, report: &Ds4ReportEx) -> Result<(), ClientError> {
        self.assert_alive_hot()?;
        self.with_submit_call(|call| {
            self.inner
                .bus
                .update_ds4_ex_on(call, self.inner.serial_no, report)
        })
    }
}

/// Builds and plugs a virtual controller.
///
/// Obtained from `Client::new_x360_target` / `Client::new_ds4_target` (feature-gated).
/// Optionally set USB vendor/product ids, then call [`plug`](TargetBuilder::plug).
#[derive(Debug)]
#[must_use = "the pad is not created until you call `.plug()`"]
pub struct TargetBuilder<'a, T: Device> {
    client: &'a Client,
    vid: Option<u16>,
    pid: Option<u16>,
    _marker: PhantomData<T>,
}

impl<'a, T: Device> TargetBuilder<'a, T> {
    #[inline]
    pub(crate) fn new(client: &'a Client) -> Self {
        Self {
            client,
            vid: None,
            pid: None,
            _marker: PhantomData,
        }
    }

    /// USB vendor ID reported to the host.
    ///
    /// Defaults: `0x045E` (Microsoft) for Xbox 360, `0x054C` (Sony) for DualShock 4.
    /// Most apps should leave this alone: games and anti-cheat often expect the
    /// real vendor id.
    #[inline]
    pub fn with_vid(mut self, vid: u16) -> Self {
        self.vid = Some(vid);
        self
    }

    /// USB product ID reported to the host.
    ///
    /// Defaults: `0x028E` (Xbox 360 Controller), `0x05C4` (DualShock 4).
    #[inline]
    pub fn with_pid(mut self, pid: u16) -> Self {
        self.pid = Some(pid);
        self
    }

    /// Create the virtual pad on the bus.
    ///
    /// Returns a [`Pending`] handle. Call
    /// [`wait_for_ready`](TargetHandle::wait_for_ready) before sending input.
    ///
    /// # Errors
    ///
    /// - [`ClientError::NoFreeSlot`]: this client already owns `max_targets` pads
    /// - [`ClientError::BusSerialsExhausted`]: every serial in the search window is
    ///   taken (including by other processes)
    /// - [`ClientError::Bus`]: other driver failures
    /// - [`ClientError::Poisoned`]: another thread panicked while holding client state
    ///
    /// # Example
    /// ```no_run
    /// # use vigem_rust::Client;
    /// # use std::error::Error;
    /// # fn main() -> Result<(), Box<dyn Error>> {
    /// # let client = Client::connect()?;
    #[cfg_attr(
        feature = "x360",
        doc = "let pad = client.new_x360_target().plug()?.wait_for_ready()?;"
    )]
    #[cfg_attr(
        all(feature = "ds4", not(feature = "x360")),
        doc = "let pad = client.new_ds4_target().plug()?.wait_for_ready()?;"
    )]
    /// # let _ = pad;
    /// # Ok(())
    /// # }
    /// ```
    pub fn plug(self) -> Result<TargetHandle<T, Pending>, ClientError> {
        let kind = <T as sealed::DeviceSlots>::KIND;
        let (default_vid, default_pid) = kind.get_identifiers();
        let spec = crate::client::TargetSpec {
            kind,
            vendor_id: self.vid.unwrap_or(default_vid),
            product_id: self.pid.unwrap_or(default_pid),
        };
        self.client.plugin_internal(spec)
    }

    /// Alias for [`plug`](Self::plug).
    ///
    /// # Errors
    ///
    /// Same as [`plug`](Self::plug).
    #[deprecated(since = "0.2.0", note = "renamed to `plug`")]
    #[inline]
    pub fn plugin(self) -> Result<TargetHandle<T, Pending>, ClientError> {
        self.plug()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{Client, TargetSpec};
    use std::cell::RefCell;
    use std::time::{Duration, Instant};

    fn test_spec() -> TargetSpec {
        #[cfg(feature = "x360")]
        let kind = TargetType::Xbox360;
        #[cfg(all(not(feature = "x360"), feature = "ds4"))]
        let kind = TargetType::DualShock4;

        TargetSpec {
            kind,
            vendor_id: 0,
            product_id: 0,
        }
    }

    fn pending_pad(client: &Client) -> TargetHandle<TestPad, Pending> {
        client
            .plugin_internal_with::<TestPad, _>(test_spec(), |_bus, _target, _serial| Ok(()))
            .expect("plug")
    }

    #[test]
    fn probe_streak_requires_three_consecutive_oks() {
        {
            let client = Client::null_for_tests(4);
            let handle = pending_pad(&client);
            let probes = RefCell::new(0u32);

            let ready = handle
                .wait_with_probe(Duration::from_secs(1), |_this| {
                    *probes.borrow_mut() += 1;
                    Ok(())
                })
                .expect("ready");

            assert_eq!(*probes.borrow(), READY_CONSECUTIVE_PROBES);
            let _ = ready;
        }

        // NotReady mid-streak resets; need three consecutive OKs after (5 probes total).
        {
            let client = Client::null_for_tests(4);
            let handle = pending_pad(&client);
            let serial = handle.serial_no();
            let script = RefCell::new(
                [
                    Ok(()),
                    Err(ClientError::Bus(BusError::TargetNotReady {
                        serial_no: serial,
                    })),
                    Ok(()),
                    Ok(()),
                    Ok(()),
                ]
                .into_iter(),
            );
            let probes = RefCell::new(0u32);

            let ready = handle
                .wait_with_probe(Duration::from_secs(1), |_this| {
                    *probes.borrow_mut() += 1;
                    script
                        .borrow_mut()
                        .next()
                        .expect("script exhausted before wait finished")
                })
                .expect("ready after streak reset");

            assert_eq!(*probes.borrow(), 5);
            let _ = ready;
        }
    }

    #[test]
    fn zero_timeout_probes_exactly_once() {
        {
            let client = Client::null_for_tests(4);
            let handle = pending_pad(&client);
            let probes = RefCell::new(0u32);

            let ready = handle
                .wait_with_probe(Duration::ZERO, |_this| {
                    *probes.borrow_mut() += 1;
                    Ok(())
                })
                .expect("ready");

            assert_eq!(*probes.borrow(), 1);
            let _ = ready;
        }

        // TargetNotReady on the single probe -> ReadyTimeout (no retry).
        {
            let client = Client::null_for_tests(4);
            let handle = pending_pad(&client);
            let serial = handle.serial_no();
            let probes = RefCell::new(0u32);

            let err = handle
                .wait_with_probe(Duration::ZERO, |_this| {
                    *probes.borrow_mut() += 1;
                    Err(ClientError::Bus(BusError::TargetNotReady {
                        serial_no: serial,
                    }))
                })
                .expect_err("zero-timeout TargetNotReady must surface as ReadyTimeout");

            assert!(matches!(
                err,
                ClientError::ReadyTimeout { timeout } if timeout.is_zero()
            ));
            assert_eq!(err.to_string(), "wait_for_ready timed out after 0ns");
            assert_eq!(*probes.borrow(), 1);
            assert!(
                handle.is_attached().expect("client still live"),
                "wait timeout must leave the pending handle attached"
            );
            let _ready = handle.assume_ready();
        }
    }

    #[test]
    fn wait_with_probe_non_retryable_propagates_immediately() {
        let client = Client::null_for_tests(4);
        let handle = pending_pad(&client);
        let probes = RefCell::new(0u32);

        let err = handle
            .wait_with_probe(Duration::from_secs(1), |_this| {
                *probes.borrow_mut() += 1;
                Err(ClientError::Bus(BusError::AccessDenied {
                    operation: Operation::XUSB_SUBMIT_REPORT,
                    serial_no: Some(1),
                }))
            })
            .expect_err("AccessDenied must not be retried as ReadyTimeout");

        assert!(matches!(
            err,
            ClientError::Bus(BusError::AccessDenied { .. })
        ));
        assert_eq!(*probes.borrow(), 1);
        assert!(
            handle.is_attached().expect("client still live"),
            "a hard probe error must leave the pending handle attached"
        );
    }

    #[test]
    fn nonzero_timeout_times_out_as_ready_timeout() {
        let client = Client::null_for_tests(4);
        let handle = pending_pad(&client);
        let serial = handle.serial_no();
        let probes = RefCell::new(0u32);
        let timeout = Duration::from_millis(50);

        let started = Instant::now();
        let err = handle
            .wait_with_probe(timeout, |_this| {
                *probes.borrow_mut() += 1;
                Err(ClientError::Bus(BusError::TargetNotReady {
                    serial_no: serial,
                }))
            })
            .expect_err("non-zero timeout that stays TargetNotReady must surface as ReadyTimeout");
        let elapsed = started.elapsed();

        assert!(
            matches!(err, ClientError::ReadyTimeout { timeout: t } if t == timeout),
            "timeout must be ClientError::ReadyTimeout, not the inner BusError; got {err:?}"
        );
        assert_eq!(
            err.to_string(),
            format!("wait_for_ready timed out after {timeout:?}")
        );
        assert!(
            *probes.borrow() >= 2,
            "first miss must be retried immediately; probes={}",
            *probes.borrow()
        );
        assert!(
            elapsed >= timeout,
            "deadline must be honoured, elapsed={elapsed:?}"
        );
        assert!(
            handle.is_attached().expect("client still live"),
            "wait timeout must leave the pending handle attached"
        );
        assert_eq!(handle.serial_no(), serial);
        let ready = handle.assume_ready();
        assert!(
            ready.is_attached().expect("client still live"),
            "assume_ready after timeout must still yield a live handle"
        );
    }

    #[test]
    fn wait_with_probe_aborts_when_pad_unplugged_mid_wait() {
        let client = Client::null_for_tests(4);
        let handle = pending_pad(&client);
        let serial = handle.serial_no();
        let probes = RefCell::new(0u32);

        let started = Instant::now();
        let err = handle
            .wait_with_probe(Duration::from_secs(30), |this| {
                *probes.borrow_mut() += 1;
                if *probes.borrow() == 2 {
                    let _ = this.unplug();
                }
                Err(ClientError::Bus(BusError::TargetNotReady {
                    serial_no: serial,
                }))
            })
            .expect_err("unplug mid-wait must abort instead of spinning to the deadline");
        let elapsed = started.elapsed();

        assert!(
            matches!(err, ClientError::TargetDoesNotExist { serial_no } if serial_no == serial),
            "got {err:?}"
        );
        assert_eq!(
            *probes.borrow(),
            2,
            "no probe after the pad is gone; probes={}",
            *probes.borrow()
        );
        assert!(
            elapsed < Duration::from_secs(1),
            "must not wait out the 30s deadline, elapsed={elapsed:?}"
        );
        assert!(matches!(handle.is_attached(), Ok(false)));
    }

    const fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn handle_and_inner_are_send_sync() {
        // Public handles are covered by tests/api_surface.rs; this pins the inner.
        assert_send_sync::<TargetHandleInner<TestPad>>();
        assert_send_sync::<TargetHandle<TestPad, Ready>>();
        assert_send_sync::<TargetHandle<TestPad, Pending>>();
        #[cfg(feature = "x360")]
        assert_send_sync::<TargetHandleInner<Xbox360>>();
        #[cfg(feature = "ds4")]
        assert_send_sync::<TargetHandleInner<DualShock4>>();
    }

    #[test]
    fn device_kind_matches_marker() {
        #[cfg(feature = "x360")]
        {
            assert_eq!(<Xbox360 as sealed::DeviceSlots>::KIND, TargetType::Xbox360);
            // ViGEmBus VIGEM_TARGET_TYPE: XBOX360 = 0.
            assert_eq!(TargetType::Xbox360 as u32, 0);
            assert_eq!(TargetType::Xbox360.get_identifiers(), (0x045E, 0x028E));
        }
        #[cfg(feature = "ds4")]
        {
            assert_eq!(
                <DualShock4 as sealed::DeviceSlots>::KIND,
                TargetType::DualShock4
            );
            // ViGEmBus VIGEM_TARGET_TYPE: DUALSHOCK4 = 2 (1 is unused).
            assert_eq!(TargetType::DualShock4 as u32, 2);
            assert_eq!(TargetType::DualShock4.get_identifiers(), (0x054C, 0x05C4));
        }
    }

    #[test]
    #[cfg(all(feature = "x360", feature = "ds4"))]
    fn slot_structs_store_only_this_device_hubs() {
        use std::mem::size_of;

        assert_eq!(
            size_of::<Xbox360Slots>(),
            size_of::<Mutex<Weak<NotificationHub<X360Notification>>>>()
        );
        assert_eq!(
            size_of::<Ds4Slots>(),
            size_of::<(
                Mutex<Weak<NotificationHub<Ds4Notification>>>,
                Mutex<Weak<NotificationHub<Ds4OutputBuffer>>>,
            )>()
        );
        assert!(
            size_of::<TargetHandleInner<Xbox360>>() <= size_of::<TargetHandleInner<DualShock4>>()
        );
        assert!(size_of::<TargetHandleInner<TestPad>>() < size_of::<TargetHandleInner<Xbox360>>());
    }

    #[test]
    #[cfg(feature = "x360")]
    fn reregister_after_worker_closed_fanout_spawns_new_hub() {
        let client = Client::null_for_tests(4);
        let pad = client
            .plugin_internal_with::<Xbox360, _>(test_spec(), |_, _, _| Ok(()))
            .expect("plug")
            .assume_ready();

        let rx1 = pad.register_notification().expect("first register");

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match rx1.recv_timeout(Duration::from_millis(500)) {
                Err(RecvTimeoutError::Disconnected | RecvTimeoutError::Bus(_)) => break,
                Err(RecvTimeoutError::Timeout) | Ok(_) => {
                    assert!(
                        Instant::now() < deadline,
                        "rx1 did not disconnect within 5s (null-bus worker should exit immediately)"
                    );
                }
            }
        }

        // rx1 still alive: hub Arc stays up while fan-out is closed.
        let rx2 = pad
            .register_notification()
            .expect("re-register after worker closed must not panic");

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match rx2.recv_timeout(Duration::from_millis(500)) {
                Err(RecvTimeoutError::Disconnected | RecvTimeoutError::Bus(_)) => break,
                Err(RecvTimeoutError::Timeout) | Ok(_) => {
                    assert!(
                        Instant::now() < deadline,
                        "rx2 did not disconnect within 5s"
                    );
                }
            }
        }
    }

    #[test]
    #[cfg(feature = "x360")]
    fn notification_receiver_exposes_configured_buffer() {
        let client = Client::null_for_tests(4);
        let pad = client
            .plugin_internal_with::<Xbox360, _>(test_spec(), |_, _, _| Ok(()))
            .expect("plug")
            .assume_ready();

        let bounded = pad
            .register_notification_with(NotificationBuffer::Bounded { capacity: 1 })
            .expect("bounded");
        assert_eq!(bounded.capacity(), Some(1));
        assert!(bounded.is_bounded());

        let unbounded = pad
            .register_notification_with(NotificationBuffer::Unbounded)
            .expect("unbounded");
        assert_eq!(unbounded.capacity(), None);
        assert!(!unbounded.is_bounded());

        let default = pad.register_notification().expect("default");
        assert_eq!(default.capacity(), Some(NOTIFICATION_CHANNEL_CAPACITY));
    }
}
