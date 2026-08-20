//! # Typical flow
//!
//! ```no_run
//! use vigem_rust::{Client, ClientError};
//!
//! fn main() -> Result<(), ClientError> {
//!     let client = Client::connect()?;
//!     # #[cfg(feature = "x360")]
//!     let pad = client.new_x360_target().plug()?.wait_for_ready()?;
//!     # #[cfg(all(feature = "ds4", not(feature = "x360")))]
//!     # let pad = client.new_ds4_target().plug()?.wait_for_ready()?;
//!     // ... use pad ...
//!     # let _ = pad;
//!     Ok(())
//! }
//! ```
//!
//! Most of the time you only need [`Client::connect`]. Use [`Client::builder`] when you
//! want to cap how many virtual pads this client will own (see
//! [`ClientBuilder::max_targets`]).
//!
//! # Serials and other processes
//!
//! Each virtual pad gets a **serial number** from a pool shared by every process
//! using ViGEmBus on the machine (`1..=`[`SERIAL_SCAN_LIMIT`]).
//! [`max_targets`](ClientBuilder::max_targets) only limits how many pads **this
//! client** may own. It does not reserve serials for you.
//!
//! When you plug a pad, the crate picks a free serial (starting from a random
//! offset so concurrent apps do not all race on serial 1). If a serial is already
//! taken, including by another process, it simply tries the next one.
//!
//! ## Finding free serials
//!
//! By default ([`SerialDiscovery::Auto`]), an uncontested plug is a single
//! attempt. If that serial is busy, the crate checks which serials Windows
//! currently shows as in use and uses that to skip taken ones on the remaining
//! tries. Use [`SerialDiscovery::Eager`] to check first every time, or
//! [`SerialDiscovery::Off`] to rely only on retry.
//!
//! That occupancy snapshot can be slightly stale (Windows may lag a just-plugged
//! pad, and another app can race you). Retry still handles real conflicts under
//! every policy. This client's pads are [`Client::live_serials`]; the machine-wide
//! snapshot (other processes included) is [`Client::occupied_serials`].
//!
//! Plug can still fail with [`ClientError::BusSerialsExhausted`] when every
//! serial in the window is already taken, even if this client owns nothing.
//! Exceeding your own quota fails with [`ClientError::NoFreeSlot`] without
//! contacting the driver.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::error::{BusError, ClientError, Operation};
use crate::internal::bus::Bus;
#[cfg(feature = "ds4")]
use crate::target::DualShock4;
#[cfg(feature = "x360")]
use crate::target::Xbox360;

use crate::target::{Device, Pending, Target, TargetBuilder, TargetHandle, TargetType};

/// How many serial numbers this crate will try when plugging a pad.
///
/// Machine-wide pool, not this client's ownership cap. See the
/// [module docs](crate::client#serials-and-other-processes).
//
// Maintainer note: ViGEmBus (busenum.cpp) maps a duplicate serial to
// STATUS_INVALID_PARAMETER and leaks the PDO on that path. Keep this limit
// small; do not raise it toward the upstream client's 65535 scan without
// accounting for that kernel leak cost.
pub const SERIAL_SCAN_LIMIT: u32 = 64;

/// Default maximum virtual controllers per [`Client`] (16).
///
/// Per-client cap only; see [`ClientBuilder::max_targets`]. Windows XInput
/// still only exposes four user slots, so typical games see at most four Xbox
/// 360 pads. DualShock 4 is not limited by XInput.
pub const DEFAULT_MAX_TARGETS: u32 = 16;

/// Highest value [`ClientBuilder::max_targets`] accepts (same as [`SERIAL_SCAN_LIMIT`]).
///
/// A larger cap could never be filled. [`ClientError::InvalidMaxTargets`] if you pass more.
pub const MAX_TARGETS_LIMIT: u32 = SERIAL_SCAN_LIMIT;

const _: () = assert!(MAX_TARGETS_LIMIT <= SERIAL_SCAN_LIMIT && SERIAL_SCAN_LIMIT > 0);

/// How a [`Client`] looks up which serials are already taken before (or while)
/// plugging a pad.
///
/// Serials are shared machine-wide, so other processes' pads count too. Every
/// policy still retries if a serial turns out busy: the lookup is a hint, not a
/// reservation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum SerialDiscovery {
    /// Try a serial first; only look up occupancy after the first conflict.
    ///
    /// Cheap when nothing else is using the bus; learns which slots are taken
    /// once contention shows up. **Default.**
    #[default]
    Auto,
    /// Look up occupied serials before every plug.
    ///
    /// Adds a short Windows device-list check (typically a few milliseconds) and
    /// avoids most rejected attempts. Prefer this when plugging many pads on a
    /// busy machine.
    Eager,
    /// Skip the occupancy check; use randomised start and retry only.
    ///
    /// Use this when device enumeration is restricted, or to measure Auto/Eager
    /// against retry-only plugging.
    Off,
}

#[inline]
fn resolve_max_targets(configured: Option<u32>) -> Result<u32, ClientError> {
    let max_targets = configured.unwrap_or(DEFAULT_MAX_TARGETS);
    if max_targets == 0 || max_targets > MAX_TARGETS_LIMIT {
        return Err(ClientError::InvalidMaxTargets {
            requested: max_targets,
            max_allowed: MAX_TARGETS_LIMIT,
        });
    }
    Ok(max_targets)
}

pub(crate) struct ClientInner {
    pub(crate) bus: Bus,
    pub(crate) targets: HashMap<u32, Target>,
    /// Serials held while a plug or unplug IOCTL runs without the client mutex.
    /// Prevents `find_free_serial` from reusing a serial mid-operation.
    reserved: HashSet<u32>,
    /// Monotonic generation counter; never reused.
    pub(crate) next_gen: u64,
    max_targets: u32,
    /// Serial the next scan starts from (`1..=SERIAL_SCAN_LIMIT`), advanced past
    /// each successful plug. Randomised per client so independent processes do
    /// not all collide on serial 1.
    scan_cursor: u32,
    /// Plug attempts holding a quota slot but not yet in `targets`.
    /// Counted against `max_targets` so concurrent plugs cannot over-admit.
    inflight_plugs: u32,
    serial_discovery: SerialDiscovery,
    /// Count of driver-rejected plug attempts (slot conflicts). Each rejection
    /// also leaks a small amount of kernel memory in ViGEmBus. Use this to
    /// verify discovery is working (`0` under Eager after occupancy settled).
    plug_conflicts: u64,
    discovery_walks: u64,
    /// Test-only: when `Some`, used as the discovery snapshot instead of a real
    /// `PnP` walk (so unit tests can force a full mask / inject occupancy).
    #[cfg(test)]
    test_discovery_snapshot: Option<HashSet<u32>>,
}

/// Random starting serial in `1..=SERIAL_SCAN_LIMIT`.
///
/// Uses `RandomState`, which std seeds from system randomness per process and
/// varies per instance, enough to spread scan starts across processes. Not
/// cryptographic and not required to be.
fn random_scan_start() -> u32 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};

    let raw = RandomState::new().build_hasher().finish();
    let offset = u32::try_from(raw % u64::from(SERIAL_SCAN_LIMIT)).unwrap_or(0);
    offset + 1
}

/// Holds `serial_no` in [`ClientInner::reserved`] until dropped (unplug IOCTL window).
pub(crate) struct UnplugSerialGuard {
    client: Arc<Mutex<ClientInner>>,
    serial_no: u32,
}

impl Drop for UnplugSerialGuard {
    fn drop(&mut self) {
        lock_client_recover(&self.client)
            .reserved
            .remove(&self.serial_no);
    }
}

/// If `(serial_no, generation)` is live: clear `alive`, remove from the map, reserve
/// the serial, and return a guard that clears the reservation on drop.
///
/// Caller must run `bus.unplug` while the guard is alive, then drop the guard
/// (including on unplug failure) so the serial becomes free again.
pub(crate) fn begin_unplug(
    client: &Arc<Mutex<ClientInner>>,
    serial_no: u32,
    generation: u64,
    alive: &AtomicBool,
) -> Result<Option<UnplugSerialGuard>, ClientError> {
    let mut inner = lock_client(client)?;
    Ok(begin_unplug_locked(
        client, &mut inner, serial_no, generation, alive,
    ))
}

/// Same as [`begin_unplug`], recovering from a poisoned client mutex (Drop paths).
pub(crate) fn begin_unplug_recover(
    client: &Arc<Mutex<ClientInner>>,
    serial_no: u32,
    generation: u64,
    alive: &AtomicBool,
) -> Option<UnplugSerialGuard> {
    let mut inner = lock_client_recover(client);
    begin_unplug_locked(client, &mut inner, serial_no, generation, alive)
}

fn begin_unplug_locked(
    client: &Arc<Mutex<ClientInner>>,
    inner: &mut ClientInner,
    serial_no: u32,
    generation: u64,
    alive: &AtomicBool,
) -> Option<UnplugSerialGuard> {
    let matches = inner
        .targets
        .get(&serial_no)
        .is_some_and(|t| t.generation == generation);
    if !matches {
        return None;
    }
    alive.store(false, Ordering::Release);
    inner.targets.remove(&serial_no);
    inner.reserved.insert(serial_no);
    Some(UnplugSerialGuard {
        client: Arc::clone(client),
        serial_no,
    })
}

/// Target description before serial assignment in [`Client::plugin_internal`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct TargetSpec {
    pub(crate) kind: TargetType,
    pub(crate) vendor_id: u16,
    pub(crate) product_id: u16,
}

/// Unplugs `serial_no` on drop if still armed (plug succeeded, map insert failed).
struct UnplugOnDrop {
    bus: Bus,
    serial_no: u32,
    armed: bool,
}

impl Drop for UnplugOnDrop {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.bus.unplug(self.serial_no);
        }
    }
}

/// Holds one quota slot in [`ClientInner::inflight_plugs`] for the duration of a
/// plug attempt sequence. Released on drop, or handed over to `targets` on success.
struct PlugQuotaGuard {
    client: Arc<Mutex<ClientInner>>,
    active: bool,
}

impl PlugQuotaGuard {
    /// Release while the caller already holds the lock (success path, so the
    /// slot transfers to `targets` atomically with the map insert).
    fn release_locked(&mut self, inner: &mut ClientInner) {
        if self.active {
            debug_assert!(
                inner.inflight_plugs > 0,
                "PlugQuotaGuard release with zero inflight_plugs"
            );
            inner.inflight_plugs = inner.inflight_plugs.saturating_sub(1);
            self.active = false;
        }
    }
}

impl Drop for PlugQuotaGuard {
    fn drop(&mut self) {
        if self.active {
            let mut inner = lock_client_recover(&self.client);
            debug_assert!(
                inner.inflight_plugs > 0,
                "PlugQuotaGuard drop with zero inflight_plugs"
            );
            inner.inflight_plugs = inner.inflight_plugs.saturating_sub(1);
        }
    }
}

/// Lock the client mutex; poison -> [`ClientError::Poisoned`].
#[inline]
fn lock_client(mutex: &Mutex<ClientInner>) -> Result<MutexGuard<'_, ClientInner>, ClientError> {
    mutex.lock().map_err(|_| ClientError::Poisoned)
}

/// Lock the client mutex, recovering from poison (Drop / cleanup; used by target drop).
#[inline]
pub(crate) fn lock_client_recover(mutex: &Mutex<ClientInner>) -> MutexGuard<'_, ClientInner> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

#[inline]
fn next_generation(inner: &mut ClientInner) -> u64 {
    let generation = inner.next_gen;
    inner.next_gen = inner.next_gen.wrapping_add(1);
    if inner.next_gen == 0 {
        inner.next_gen = 1;
    }
    generation
}

/// First free serial at or after `start`, wrapping within
/// `1..=SERIAL_SCAN_LIMIT`. Skips serials this client holds live or reserved,
/// and any serials reported by `PnP` discovery when `discovered` is `Some`.
///
/// The range is the driver-wide serial window, **not** `max_targets`. The
/// quota is enforced separately in `plugin_internal_with`.
#[inline]
fn find_free_serial(
    inner: &ClientInner,
    start: u32,
    discovered: Option<&HashSet<u32>>,
) -> Option<u32> {
    let n = SERIAL_SCAN_LIMIT;
    debug_assert!((1..=n).contains(&start), "scan_cursor out of range");
    (0..n).map(|i| ((start - 1 + i) % n) + 1).find(|s| {
        !inner.targets.contains_key(s)
            && !inner.reserved.contains(s)
            && !discovered.is_some_and(|d| d.contains(s))
    })
}

#[inline]
const fn next_cursor(serial_no: u32) -> u32 {
    (serial_no % SERIAL_SCAN_LIMIT) + 1
}

/// `true` if a plug error means "that serial is taken" and the scan should advance.
///
/// Scoped to [`Operation::PLUGIN_TARGET`]. Includes `ERROR_INVALID_PARAMETER`,
/// which is what ViGEmBus returns for a duplicate serial (`Bus_PlugInDevice`
/// remaps `STATUS_OBJECT_NAME_EXISTS`). On any other operation that code means a
/// malformed request and is **not** retryable.
pub(crate) fn is_retryable_plug_error(err: &BusError) -> bool {
    match err {
        BusError::Ioctl {
            operation, source, ..
        } if *operation == Operation::PLUGIN_TARGET => source.is_plug_serial_conflict(),
        _ => false,
    }
}

/// Connection to the ViGEmBus driver and the virtual controllers you create on it.
///
/// # Creating a client
///
/// - [`Client::connect`][]: defaults (up to [`DEFAULT_MAX_TARGETS`] pads)
/// - [`Client::builder`][]: set [`max_targets`](ClientBuilder::max_targets) first
///
/// # Controllers
///
/// Use `new_x360_target` / `new_ds4_target` (feature-gated), then
/// `.plug()?.wait_for_ready()?` to get a usable [`TargetHandle`].
///
/// How serials are chosen is covered in the [module docs](crate::client#serials-and-other-processes).
///
/// # Threads and drop
///
/// `Client` is `Send + Sync` and cheap to clone (`Arc`). All clones share the
/// same connection and the same pads. To drive one pad from several threads,
/// clone the [`TargetHandle`], not the `Client`. Concurrent `update`s on the
/// same pad run one at a time.
///
/// A pad leaves the bus when its **last** [`TargetHandle`] is dropped, when you
/// call [`unplug`](crate::TargetHandle::unplug) / [`unplug_all`](Self::unplug_all),
/// or when the **last** `Client` clone is dropped. See the
/// [target module](crate::target#who-unplugs-a-pad).
///
/// The last handle or last client unplugs **on that thread** and waits on the
/// driver. Drop swallows unplug errors and does not panic if the client mutex
/// is poisoned.
///
/// Last-client drop unplugs remaining pads one by one. If the bus is wedged,
/// that is on the order of [`max_targets`](Self::max_targets) × a few seconds.
/// Prefer an explicit [`unplug`](crate::TargetHandle::unplug) on a frame or
/// async thread.
///
/// Getters such as [`max_targets`](Self::max_targets) always return a value,
/// even if another thread panicked while holding client state.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Mutex<ClientInner>>,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.inner.lock() {
            Ok(inner) => f
                .debug_struct("Client")
                .field("max_targets", &inner.max_targets)
                .field("target_count", &inner.targets.len())
                .field("serial_discovery", &inner.serial_discovery)
                .finish_non_exhaustive(),
            Err(_) => f
                .debug_struct("Client")
                .field("poisoned", &true)
                .finish_non_exhaustive(),
        }
    }
}

/// Optional settings before opening a [`Client`].
///
/// Start with [`Client::builder`]. Most apps only set
/// [`max_targets`](Self::max_targets), then call [`connect`](Self::connect).
#[derive(Debug, Clone, Default)]
#[must_use = "the client is not opened until you call `.connect()`"]
pub struct ClientBuilder {
    max_targets: Option<u32>,
    serial_discovery: SerialDiscovery,
}

impl ClientBuilder {
    #[inline]
    fn new() -> Self {
        Self {
            max_targets: None,
            serial_discovery: SerialDiscovery::default(),
        }
    }

    /// How many virtual controllers this client may own at once (`1..=`[`MAX_TARGETS_LIMIT`]).
    ///
    /// Default when unset: [`DEFAULT_MAX_TARGETS`] (16). `0` or a value above the
    /// limit fails [`connect`](Self::connect) with [`ClientError::InvalidMaxTargets`].
    ///
    /// Hitting this cap returns [`ClientError::NoFreeSlot`] without talking to the
    /// driver. It does not reserve serials. If the whole search window is taken,
    /// plug returns [`ClientError::BusSerialsExhausted`] instead. See
    /// [serials](crate::client#serials-and-other-processes).
    #[inline]
    pub fn max_targets(mut self, count: u32) -> Self {
        self.max_targets = Some(count);
        self
    }

    /// How this client looks up which serials are already taken when plugging.
    ///
    /// Default: [`SerialDiscovery::Auto`]. See that enum for the trade-offs.
    /// A failed lookup never fails the plug. The client falls back to retry.
    #[inline]
    pub fn serial_discovery(mut self, mode: SerialDiscovery) -> Self {
        self.serial_discovery = mode;
        self
    }

    /// Open ViGEmBus and return a [`Client`].
    ///
    /// # Errors
    ///
    /// - [`ClientError::InvalidMaxTargets`]: `max_targets` was `0` or above the limit
    /// - [`ClientError::Bus`]: driver missing, access denied, version mismatch, etc.
    ///   (see [`BusError`])
    pub fn connect(self) -> Result<Client, ClientError> {
        let max_targets = resolve_max_targets(self.max_targets)?;
        let bus = Bus::connect()?;
        let inner = ClientInner {
            bus,
            targets: HashMap::new(),
            reserved: HashSet::new(),
            next_gen: 1,
            max_targets,
            scan_cursor: random_scan_start(),
            inflight_plugs: 0,
            serial_discovery: self.serial_discovery,
            plug_conflicts: 0,
            discovery_walks: 0,
            #[cfg(test)]
            test_discovery_snapshot: None,
        };

        Ok(Client {
            inner: Arc::new(Mutex::new(inner)),
        })
    }
}

impl Client {
    /// Start a builder (e.g. set a custom pad limit) then call
    /// [`ClientBuilder::connect`].
    ///
    /// # Example
    /// ```no_run
    /// # use std::error::Error;
    /// # fn main() -> Result<(), Box<dyn Error>> {
    /// use vigem_rust::Client;
    /// let client = Client::builder().max_targets(4).connect()?;
    /// # let _ = client;
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    pub fn builder() -> ClientBuilder {
        ClientBuilder::new()
    }

    /// Connect to ViGEmBus with default settings.
    ///
    /// Equivalent to `Client::builder().connect()`. Requires the
    /// [ViGEmBus](https://vigem.org) driver installed and running.
    ///
    /// Missing or stopped driver -> [`BusError::BusNotFound`]. An incompatible
    /// driver is [`BusError::VersionMismatch`]. Used with the last official
    /// Nefarius builds, 1.21.442 and 1.22.0 (same driver).
    ///
    /// # Errors
    ///
    /// Same as [`ClientBuilder::connect`].
    ///
    /// # Example
    /// ```no_run
    /// # use std::error::Error;
    /// # fn main() -> Result<(), Box<dyn Error>> {
    /// use vigem_rust::Client;
    /// let client = Client::connect()?;
    /// # let _ = client;
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    pub fn connect() -> Result<Self, ClientError> {
        Self::builder().connect()
    }

    /// Maximum pads this client may own (set at connect time).
    ///
    /// See [`ClientBuilder::max_targets`].
    #[inline]
    pub fn max_targets(&self) -> u32 {
        lock_client_recover(&self.inner).max_targets
    }

    /// How many controllers this client currently has plugged.
    ///
    /// A snapshot that may change right after the call.
    #[inline]
    pub fn target_count(&self) -> usize {
        lock_client_recover(&self.inner).targets.len()
    }

    /// Serial numbers of this client's plugged controllers, sorted ascending.
    ///
    /// Bus slot ids (`1..=`[`SERIAL_SCAN_LIMIT`]), not USB serial strings, and only
    /// pads owned by **this** client. Reads client state only (no Windows
    /// enumeration); authoritative for this process. Snapshot; may change right
    /// after the call.
    ///
    /// Returned as a [`BTreeSet`] so it composes with
    /// [`occupied_serials`](Self::occupied_serials) (`occupied.difference(&live)`).
    /// See [serials](crate::client#serials-and-other-processes).
    pub fn live_serials(&self) -> BTreeSet<u32> {
        lock_client_recover(&self.inner)
            .targets
            .keys()
            .copied()
            .collect()
    }

    /// Serial-discovery policy set at connect time.
    #[inline]
    pub fn serial_discovery(&self) -> SerialDiscovery {
        lock_client_recover(&self.inner).serial_discovery
    }

    /// Serials currently in use on the bus, **including pads from other processes**.
    ///
    /// Walks Windows' device list, so a just-plugged pad (including this client's)
    /// may not appear for a moment. Use as a diagnostic snapshot or for your own
    /// placement logic, not as a hard reservation. This client's own pads are
    /// [`live_serials`](Self::live_serials).
    ///
    /// # Errors
    ///
    /// [`ClientError::Bus`] with [`BusError::Enumeration`] if occupied serials
    /// could not be listed. Returns an empty set, not an error, when nothing
    /// is plugged.
    pub fn occupied_serials(&self) -> Result<BTreeSet<u32>, ClientError> {
        let devnode = {
            let inner = lock_client(&self.inner)?;
            inner.bus.devnode_id_cloned()
        };
        let Some(id) = devnode else {
            return Err(ClientError::Bus(BusError::Enumeration {
                source: crate::error::Win32Error::from_win32(crate::error::ERROR_FILE_NOT_FOUND),
            }));
        };
        let set = crate::internal::devnode::occupied_serials(&id)?;
        {
            let mut inner = lock_client_recover(&self.inner);
            inner.discovery_walks = inner.discovery_walks.saturating_add(1);
        }
        Ok(set.into_iter().collect())
    }

    /// How many plug attempts hit a taken serial since connect.
    ///
    /// Unit-test helper for verifying discovery under contention.
    #[cfg(test)]
    #[inline]
    pub(crate) fn plug_conflict_count(&self) -> u64 {
        lock_client_recover(&self.inner).plug_conflicts
    }

    /// How many times this client successfully listed occupied serials.
    ///
    /// Counts both [`occupied_serials`](Self::occupied_serials) and plug-time
    /// discovery (`Auto` / `Eager`). Unit-test helper.
    #[cfg(test)]
    #[inline]
    pub(crate) fn discovery_walk_count(&self) -> u64 {
        lock_client_recover(&self.inner).discovery_walks
    }

    /// Unplug every virtual pad owned by this client.
    ///
    /// The client stays connected; you can plug new pads afterward. Existing
    /// [`TargetHandle`]s for the removed pads stop working
    /// (`update` / notifications return [`ClientError::TargetDoesNotExist`];
    /// [`is_attached`](crate::TargetHandle::is_attached) reports `Ok(false)`).
    ///
    /// Handles stop accepting input as soon as each pad is removed, even if a
    /// later driver unplug fails. Pads are unplugged one after another.
    ///
    /// # Errors
    ///
    /// - [`ClientError::Poisoned`]: another thread panicked while holding client state
    /// - [`ClientError::Bus`]: the first driver unplug failure (remaining pads
    ///   are still attempted)
    ///
    /// Returns `Ok(())` if there were no pads.
    pub fn unplug_all(&self) -> Result<(), ClientError> {
        // Snapshot under the lock; do not hold it across unplug IOCTLs.
        let (bus, snapshot): (Bus, Vec<(u32, u64, Arc<AtomicBool>)>) = {
            let inner = lock_client(&self.inner)?;
            let bus = inner.bus.clone();
            let snapshot = inner
                .targets
                .values()
                .map(|t| (t.serial_no, t.generation, Arc::clone(&t.alive)))
                .collect();
            (bus, snapshot)
        };

        let mut first_err: Option<BusError> = None;
        for (serial_no, generation, alive) in snapshot {
            let Some(guard) = begin_unplug(&self.inner, serial_no, generation, &alive)? else {
                // Already gone (concurrent unplug / handle drop).
                continue;
            };
            if let Err(e) = bus.unplug(serial_no)
                && first_err.is_none()
            {
                first_err = Some(e);
            }
            drop(guard);
        }

        match first_err {
            Some(e) => Err(ClientError::Bus(e)),
            None => Ok(()),
        }
    }

    /// Start building a virtual Xbox 360 controller.
    ///
    /// Chain optional USB ids, then plug and wait until it can accept input:
    ///
    /// ```no_run
    /// # use vigem_rust::Client;
    /// # use std::error::Error;
    /// # fn main() -> Result<(), Box<dyn Error>> {
    /// # let client = Client::connect()?;
    /// let pad = client
    ///     .new_x360_target()
    ///     .with_vid(0x045E) // optional; defaults are fine for most apps
    ///     .with_pid(0x028E)
    ///     .plug()?
    ///     .wait_for_ready()?;
    /// # let _ = pad;
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    #[cfg(feature = "x360")]
    #[cfg_attr(docsrs, doc(cfg(feature = "x360")))]
    pub fn new_x360_target(&self) -> TargetBuilder<'_, Xbox360> {
        TargetBuilder::new(self)
    }

    /// Start building a virtual DualShock 4 controller.
    ///
    /// Same pattern as Xbox 360: optional VID/PID, then `plug` + `wait_for_ready`.
    ///
    /// ```no_run
    /// # use vigem_rust::Client;
    /// # use std::error::Error;
    /// # fn main() -> Result<(), Box<dyn Error>> {
    /// # let client = Client::connect()?;
    /// let pad = client.new_ds4_target().plug()?.wait_for_ready()?;
    /// # let _ = pad;
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    #[cfg(feature = "ds4")]
    #[cfg_attr(docsrs, doc(cfg(feature = "ds4")))]
    pub fn new_ds4_target(&self) -> TargetBuilder<'_, DualShock4> {
        TargetBuilder::new(self)
    }

    /// Plug `spec` on the next free serial in the scan window.
    ///
    /// Mutex is not held across `bus.plug`. Slot conflicts on
    /// `VIGEM_PLUGIN_TARGET` try the next serial; other bus errors return as
    /// [`ClientError::Bus`]. If every serial in [`SERIAL_SCAN_LIMIT`] fails with
    /// a slot conflict, returns [`ClientError::BusSerialsExhausted`]. Local
    /// quota exhaustion returns [`ClientError::NoFreeSlot`] with no IOCTL.
    pub(crate) fn plugin_internal<T: Device>(
        &self,
        spec: TargetSpec,
    ) -> Result<TargetHandle<T, Pending>, ClientError> {
        self.plugin_internal_with(spec, |bus, target, serial_no| bus.plug(target, serial_no))
    }

    /// Same as [`Self::plugin_internal`], with an injectable plug for tests.
    pub(crate) fn plugin_internal_with<T: Device, F>(
        &self,
        spec: TargetSpec,
        mut plug: F,
    ) -> Result<TargetHandle<T, Pending>, ClientError>
    where
        F: FnMut(&Bus, &Target, u32) -> Result<(), BusError>,
    {
        let mut last_retryable: Option<BusError> = None;
        // Serials whose plug attempt failed retryably. Kept in `inner.reserved` so
        // `find_free_serial` advances; released on every exit path.
        let mut burned: Vec<u32> = Vec::new();
        let mut quota: Option<PlugQuotaGuard> = None;

        let (discovery, devnode) = {
            let inner = lock_client(&self.inner)?;
            (inner.serial_discovery, inner.bus.devnode_id_cloned())
        };

        // Optional occupancy snapshot from `PnP` (or a test inject). Walk at most
        // once per plug call; never cached across plugs.
        let mut discovered: Option<HashSet<u32>> = None;
        // Distinct from `discovered.is_none()`: after a full-window bypass we
        // clear the snapshot but must not walk again.
        let mut discovery_attempted = false;
        // When discovery masks the entire window, ignore it once and let the
        // driver adjudicate - keeps `BusSerialsExhausted` tied to a real error.
        let mut discovery_bypassed = false;

        if discovery == SerialDiscovery::Eager {
            discovery_attempted = true;
            discovered = self.try_discover(devnode.as_deref());
        }

        loop {
            let (serial_no, generation, bus) = {
                let mut inner = lock_client(&self.inner)?;

                if quota.is_none() {
                    let used = u32::try_from(inner.targets.len())
                        .unwrap_or(u32::MAX)
                        .saturating_add(inner.inflight_plugs);
                    if used >= inner.max_targets {
                        let max_targets = inner.max_targets;
                        for s in burned.drain(..) {
                            inner.reserved.remove(&s);
                        }
                        drop(inner);
                        return Err(ClientError::NoFreeSlot { max_targets });
                    }
                    inner.inflight_plugs += 1;
                    quota = Some(PlugQuotaGuard {
                        client: Arc::clone(&self.inner),
                        active: true,
                    });
                }

                match find_free_serial(&inner, inner.scan_cursor, discovered.as_ref()) {
                    Some(serial_no) => {
                        let generation = next_generation(&mut inner);
                        inner.reserved.insert(serial_no);
                        let bus = inner.bus.clone();
                        (serial_no, generation, bus)
                    }
                    None if discovered.is_some() && !discovery_bypassed => {
                        // Snapshot may be stale/over-broad. Drop it once and retry
                        // the scan so a free serial is still offered to the driver.
                        drop(inner);
                        discovered = None;
                        discovery_bypassed = true;
                        continue;
                    }
                    None => {
                        let max_targets = inner.max_targets;
                        for s in burned.drain(..) {
                            inner.reserved.remove(&s);
                        }
                        drop(inner);

                        if let Some(err) = last_retryable.take() {
                            return Err(ClientError::BusSerialsExhausted {
                                scanned: SERIAL_SCAN_LIMIT,
                                source: err,
                            });
                        }
                        // No plug attempted: every serial in the window is held by
                        // this client (live or mid-unplug). Quota allowed more, but
                        // the window is full.
                        return Err(ClientError::NoFreeSlot { max_targets });
                    }
                }
            };

            // `alive` stays false until map insert.
            let mut target = Target {
                kind: spec.kind,
                serial_no,
                generation,
                vendor_id: spec.vendor_id,
                product_id: spec.product_id,
                alive: Arc::new(AtomicBool::new(false)),
            };

            match plug(&bus, &target, serial_no) {
                Ok(()) => {
                    let mut unplug_guard = UnplugOnDrop {
                        bus: bus.clone(),
                        serial_no,
                        armed: true,
                    };

                    let alive = Arc::new(AtomicBool::new(true));
                    target.alive = Arc::clone(&alive);

                    {
                        let mut inner = match lock_client(&self.inner) {
                            Ok(g) => g,
                            Err(e) => {
                                // Guard stays armed; clear reservation under recover lock.
                                let mut inner = lock_client_recover(&self.inner);
                                inner.reserved.remove(&serial_no);
                                for s in burned.drain(..) {
                                    inner.reserved.remove(&s);
                                }
                                drop(inner);
                                return Err(e);
                            }
                        };
                        debug_assert!(
                            !inner.targets.contains_key(&serial_no),
                            "reserved serial must not already be live"
                        );
                        inner.reserved.remove(&serial_no);
                        for s in burned.drain(..) {
                            inner.reserved.remove(&s);
                        }
                        inner.targets.insert(serial_no, target);
                        inner.scan_cursor = next_cursor(serial_no);
                        if let Some(g) = quota.as_mut() {
                            g.release_locked(&mut inner);
                        }
                    }

                    unplug_guard.armed = false;

                    return Ok(TargetHandle::new(
                        serial_no,
                        generation,
                        alive,
                        bus,
                        Arc::downgrade(&self.inner),
                    ));
                }
                Err(e) if is_retryable_plug_error(&e) => {
                    burned.push(serial_no);
                    last_retryable = Some(e);
                    {
                        let mut inner = lock_client_recover(&self.inner);
                        inner.plug_conflicts = inner.plug_conflicts.saturating_add(1);
                    }
                    // First conflict under Auto: learn real occupancy once so the
                    // remaining attempts are informed rather than blind.
                    // Gate on `discovery_attempted`, not `discovered.is_none()`, so
                    // a later full-window bypass cannot trigger a second walk.
                    if discovery == SerialDiscovery::Auto && !discovery_attempted {
                        discovery_attempted = true;
                        discovered = self.try_discover(devnode.as_deref());
                    }
                }
                Err(e) => {
                    let mut inner = lock_client_recover(&self.inner);
                    inner.reserved.remove(&serial_no);
                    for s in burned.drain(..) {
                        inner.reserved.remove(&s);
                    }
                    drop(inner);
                    return Err(ClientError::Bus(e));
                }
            }
        }
    }

    /// Run one occupancy walk, or the test inject. Both count toward
    /// `discovery_walks`. A failed walk or a missing devnode returns `None`
    /// without incrementing, degrading this plug to `SerialDiscovery::Off`.
    fn try_discover(&self, devnode: Option<&[u16]>) -> Option<HashSet<u32>> {
        #[cfg(test)]
        {
            let inject = lock_client_recover(&self.inner)
                .test_discovery_snapshot
                .clone();
            if let Some(set) = inject {
                let mut inner = lock_client_recover(&self.inner);
                inner.discovery_walks = inner.discovery_walks.saturating_add(1);
                return Some(set);
            }
        }
        let id = devnode?;
        match crate::internal::devnode::occupied_serials(id) {
            Ok(set) => {
                let mut inner = lock_client_recover(&self.inner);
                inner.discovery_walks = inner.discovery_walks.saturating_add(1);
                Some(set)
            }
            // Swallow: discovery is advisory; plug continues with randomised retry.
            Err(_) => None,
        }
    }
}

impl Drop for ClientInner {
    fn drop(&mut self) {
        // Best-effort unplug; clear `alive` first for hot-path handles.
        // Each unplug is teardown-bounded (~1 s + up to ~2 s drain); targets run
        // serially, so worst case is roughly max_targets x ~3 s if the bus is wedged.
        for target in self.targets.values() {
            target.alive.store(false, Ordering::Release);
            let _ = self.bus.unplug(target.serial_no);
        }
        self.targets.clear();
        self.reserved.clear();
    }
}

#[cfg(test)]
impl Client {
    /// Driver-free client with a null bus handle (unit tests only).
    ///
    /// Pins [`ClientInner::scan_cursor`] to `1` so serial assignment stays
    /// deterministic across unit tests.
    pub(crate) fn null_for_tests(max_targets: u32) -> Self {
        Self::null_for_tests_with_cursor(max_targets, 1)
    }

    /// Like [`Self::null_for_tests`], but with an explicit scan cursor
    /// (`1..=SERIAL_SCAN_LIMIT`) for wraparound / start-offset tests.
    pub(crate) fn null_for_tests_with_cursor(max_targets: u32, scan_cursor: u32) -> Self {
        use crate::internal::overlapped::BusInner;
        use windows::Win32::Foundation::HANDLE;

        debug_assert!(
            (1..=SERIAL_SCAN_LIMIT).contains(&scan_cursor),
            "scan_cursor out of range"
        );

        Self {
            inner: Arc::new(Mutex::new(ClientInner {
                bus: Bus {
                    inner: Arc::new(BusInner::new(HANDLE::default())),
                },
                targets: HashMap::new(),
                reserved: HashSet::new(),
                next_gen: 1,
                max_targets,
                scan_cursor,
                inflight_plugs: 0,
                serial_discovery: SerialDiscovery::Off,
                plug_conflicts: 0,
                discovery_walks: 0,
                test_discovery_snapshot: None,
            })),
        }
    }

    /// Install a fake discovery snapshot for unit tests (used instead of `PnP`).
    pub(crate) fn set_test_discovery_snapshot(&self, serials: HashSet<u32>) {
        lock_client_recover(&self.inner).test_discovery_snapshot = Some(serials);
    }

    /// Force a specific serial-discovery policy on a test client.
    pub(crate) fn set_serial_discovery(&self, mode: SerialDiscovery) {
        lock_client_recover(&self.inner).serial_discovery = mode;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Win32Error;
    use crate::target::TestPad;

    // Mirror the Win32 codes used by `Win32Error::is_plug_serial_conflict` / classification.
    const ERROR_ACCESS_DENIED: u32 = 5;
    const ERROR_INVALID_PARAMETER: u32 = 87;
    const ERROR_BUSY: u32 = 170;
    const ERROR_ALREADY_EXISTS: u32 = 183;
    const ERROR_GEN_FAILURE: u32 = 31;

    fn ioctl_err(operation: Operation, code: u32) -> BusError {
        BusError::classify_ioctl(operation, Some(1), Win32Error::from_win32(code))
    }

    fn test_target(serial_no: u32) -> Target {
        use crate::target::TargetType;

        #[cfg(feature = "x360")]
        let kind = TargetType::Xbox360;
        #[cfg(all(not(feature = "x360"), feature = "ds4"))]
        let kind = TargetType::DualShock4;

        Target {
            kind,
            serial_no,
            generation: 1,
            vendor_id: 0,
            product_id: 0,
            alive: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Build a driver-free `ClientInner` for unit tests (null bus handle).
    ///
    /// Pins `scan_cursor` to `1` and `inflight_plugs` to `0` for determinism.
    fn test_client_inner(live: &[u32], reserved: &[u32], max_targets: u32) -> ClientInner {
        use crate::internal::overlapped::BusInner;
        use windows::Win32::Foundation::HANDLE;

        ClientInner {
            bus: Bus {
                inner: Arc::new(BusInner::new(HANDLE::default())),
            },
            targets: live.iter().map(|&s| (s, test_target(s))).collect(),
            reserved: reserved.iter().copied().collect(),
            next_gen: 1,
            max_targets,
            scan_cursor: 1,
            inflight_plugs: 0,
            serial_discovery: SerialDiscovery::Off,
            plug_conflicts: 0,
            discovery_walks: 0,
            test_discovery_snapshot: None,
        }
    }

    fn test_client(max_targets: u32) -> Client {
        Client::null_for_tests(max_targets)
    }

    fn test_spec() -> TargetSpec {
        use crate::target::TargetType;

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

    fn reserved_is_empty(client: &Client) -> bool {
        client.inner.lock().unwrap().reserved.is_empty()
    }

    fn target_count(client: &Client) -> usize {
        client.inner.lock().unwrap().targets.len()
    }

    fn inflight_plugs(client: &Client) -> u32 {
        client.inner.lock().unwrap().inflight_plugs
    }

    #[test]
    fn client_debug_omits_internal_counters() {
        let client = test_client(4);
        let dbg = format!("{client:?}");
        assert!(
            dbg.contains("max_targets: 4")
                && dbg.contains("target_count: 0")
                && dbg.contains("serial_discovery: Off"),
            "got {dbg}"
        );
        for leaked in [
            "scan_cursor",
            "inflight_plugs",
            "plug_conflicts",
            "discovery_walks",
            "reserved",
        ] {
            assert!(!dbg.contains(leaked), "Debug must not leak {leaked}: {dbg}");
        }
    }

    #[test]
    fn retryable_plug_error_matrix() {
        // Retryable: slot conflicts on PLUGIN_TARGET only.
        assert!(is_retryable_plug_error(&ioctl_err(
            Operation::PLUGIN_TARGET,
            ERROR_BUSY
        )));
        assert!(is_retryable_plug_error(&ioctl_err(
            Operation::PLUGIN_TARGET,
            ERROR_ALREADY_EXISTS
        )));
        // ViGEmBus: occupied serial -> STATUS_INVALID_PARAMETER (Win32 87).
        assert!(is_retryable_plug_error(&ioctl_err(
            Operation::PLUGIN_TARGET,
            ERROR_INVALID_PARAMETER
        )));

        // Non-retryable: wrong operation, wrong code, or non-Ioctl variants.
        assert!(!is_retryable_plug_error(&ioctl_err(
            Operation::WAIT_DEVICE_READY,
            ERROR_BUSY
        )));
        assert!(!is_retryable_plug_error(&ioctl_err(
            Operation::WAIT_DEVICE_READY,
            ERROR_INVALID_PARAMETER
        )));
        assert!(!is_retryable_plug_error(&ioctl_err(
            Operation::PLUGIN_TARGET,
            ERROR_ACCESS_DENIED
        )));
        assert!(matches!(
            ioctl_err(Operation::PLUGIN_TARGET, ERROR_ACCESS_DENIED),
            BusError::AccessDenied { .. }
        ));
        assert!(!is_retryable_plug_error(&ioctl_err(
            Operation::PLUGIN_TARGET,
            ERROR_GEN_FAILURE
        )));
        assert!(!is_retryable_plug_error(&BusError::BusNotFound {
            source: None
        }));
        assert!(!is_retryable_plug_error(&BusError::VersionMismatch {
            requested: 0x0001
        }));
        assert!(!is_retryable_plug_error(&BusError::DeviceGone {
            operation: Operation::PLUGIN_TARGET,
            serial_no: Some(1),
        }));
    }

    /// Client Arc refcount controls pad lifetime: clones keep pads alive; the
    /// last drop tears them down. Also pins hot-path (`update`) vs slow-path
    /// (`is_attached`) error distinction after client death.
    #[test]
    fn clone_refcount_controls_pad_lifetime() {
        // --- One clone remains: pad stays live ---
        {
            let client = test_client(4);
            let handle = client
                .plugin_internal_with::<TestPad, _>(test_spec(), |_bus, _target, _serial| Ok(()))
                .expect("plug");
            let serial = handle.serial_no();

            let kept = client.clone();
            drop(client);

            assert!(
                matches!(handle.is_attached(), Ok(true)),
                "pad must stay attached while a Client clone remains: {:?}",
                handle.is_attached()
            );
            assert!(
                kept.inner
                    .lock()
                    .unwrap()
                    .targets
                    .get(&serial)
                    .expect("pad still in map")
                    .alive
                    .load(Ordering::Acquire),
                "update-path alive flag must stay set while a Client clone remains"
            );
            drop(handle);
            drop(kept);
        }

        // --- Last clone drops: pad detaches; hot vs slow error paths differ ---
        {
            let client = test_client(4);

            // Typed handle so we can exercise the hot-path `update` contract below.
            #[cfg(feature = "x360")]
            let handle = {
                use crate::target::Xbox360;
                client
                    .plugin_internal_with::<Xbox360, _>(
                        test_spec(),
                        |_bus, _target, _serial| Ok(()),
                    )
                    .expect("plug")
                    .assume_ready()
            };
            #[cfg(all(not(feature = "x360"), feature = "ds4"))]
            let handle = {
                use crate::target::DualShock4;
                client
                    .plugin_internal_with::<DualShock4, _>(test_spec(), |_bus, _target, _serial| {
                        Ok(())
                    })
                    .expect("plug")
                    .assume_ready()
            };

            drop(client);

            // Slow path still sees a missing client (Weak upgrade fails).
            assert!(matches!(
                handle.is_attached(),
                Err(ClientError::ClientNoLongerExists)
            ));

            // Hot path only loads `alive`, which `ClientInner::drop` cleared first -
            // so `update` reports TargetDoesNotExist, not ClientNoLongerExists.
            #[cfg(feature = "x360")]
            {
                use crate::controller::x360::X360Report;
                assert!(matches!(
                    handle.update(&X360Report::default()),
                    Err(ClientError::TargetDoesNotExist { .. })
                ));
            }
            #[cfg(all(not(feature = "x360"), feature = "ds4"))]
            {
                use crate::controller::ds4::Ds4Report;
                assert!(matches!(
                    handle.update(&Ds4Report::default()),
                    Err(ClientError::TargetDoesNotExist { .. })
                ));
            }
        }
    }

    /// `unplug_all` clears the map and invalidates handles; client stays usable.
    #[test]
    fn unplug_all_clears_targets_and_invalidates_handles() {
        let client = test_client(4);

        #[cfg(feature = "x360")]
        let (h1, h2) = {
            use crate::target::Xbox360;
            let h1 = client
                .plugin_internal_with::<Xbox360, _>(test_spec(), |_bus, _t, _s| Ok(()))
                .expect("plug 1")
                .assume_ready();
            let h2 = client
                .plugin_internal_with::<Xbox360, _>(test_spec(), |_bus, _t, _s| Ok(()))
                .expect("plug 2")
                .assume_ready();
            (h1, h2)
        };
        #[cfg(all(not(feature = "x360"), feature = "ds4"))]
        let (h1, h2) = {
            use crate::target::DualShock4;
            let h1 = client
                .plugin_internal_with::<DualShock4, _>(test_spec(), |_bus, _t, _s| Ok(()))
                .expect("plug 1")
                .assume_ready();
            let h2 = client
                .plugin_internal_with::<DualShock4, _>(test_spec(), |_bus, _t, _s| Ok(()))
                .expect("plug 2")
                .assume_ready();
            (h1, h2)
        };

        assert_eq!(client.target_count(), 2);

        // Null bus handle: driver unplug fails, but map/`alive` must still clear.
        let err = client
            .unplug_all()
            .expect_err("null bus must surface the first driver unplug failure");
        assert!(matches!(err, ClientError::Bus(_)), "got {err:?}");

        assert_eq!(client.target_count(), 0);
        assert!(
            client.unplug_all().is_ok(),
            "unplug_all with no pads must be Ok"
        );
        assert!(reserved_is_empty(&client));
        assert!(matches!(h1.is_attached(), Ok(false)));
        assert!(matches!(h2.is_attached(), Ok(false)));

        #[cfg(feature = "x360")]
        {
            use crate::controller::x360::X360Report;
            assert!(matches!(
                h1.update(&X360Report::default()),
                Err(ClientError::TargetDoesNotExist { .. })
            ));
        }
        #[cfg(all(not(feature = "x360"), feature = "ds4"))]
        {
            use crate::controller::ds4::Ds4Report;
            assert!(matches!(
                h1.update(&Ds4Report::default()),
                Err(ClientError::TargetDoesNotExist { .. })
            ));
        }

        let _again = client
            .plugin_internal_with::<TestPad, _>(test_spec(), |_bus, _t, _s| Ok(()))
            .expect("plug after unplug_all");
        assert_eq!(client.target_count(), 1);
    }

    /// `max_targets` is rejected before `Bus::connect`, including on a machine
    /// with no driver.
    #[test]
    fn invalid_max_targets_rejected_before_touching_driver() {
        assert_eq!(
            resolve_max_targets(None).expect("default"),
            DEFAULT_MAX_TARGETS
        );
        assert_eq!(resolve_max_targets(Some(1)).expect("min"), 1);
        assert_eq!(
            resolve_max_targets(Some(MAX_TARGETS_LIMIT)).expect("limit"),
            MAX_TARGETS_LIMIT
        );
        assert!(matches!(
            resolve_max_targets(Some(0)),
            Err(ClientError::InvalidMaxTargets {
                requested: 0,
                max_allowed: MAX_TARGETS_LIMIT
            })
        ));
        assert!(matches!(
            resolve_max_targets(Some(MAX_TARGETS_LIMIT + 1)),
            Err(ClientError::InvalidMaxTargets {
                requested,
                max_allowed: MAX_TARGETS_LIMIT
            }) if requested == MAX_TARGETS_LIMIT + 1
        ));

        assert!(matches!(
            Client::builder().max_targets(0).connect(),
            Err(ClientError::InvalidMaxTargets { .. })
        ));
        assert!(
            !matches!(
                Client::builder().max_targets(MAX_TARGETS_LIMIT).connect(),
                Err(ClientError::InvalidMaxTargets { .. })
            ),
            "a valid cap is never rejected as InvalidMaxTargets"
        );
    }

    /// Concurrent plugs must never hand out the same serial across the unlocked
    /// plug window (`reserved` is the race fence).
    #[test]
    fn concurrent_plugin_assigns_unique_serials() {
        let client = test_client(16);
        let threads: Vec<_> = (0..16)
            .map(|_| {
                let c = client.clone();
                std::thread::spawn(move || {
                    c.plugin_internal_with::<TestPad, _>(test_spec(), |_bus, _target, _serial| {
                        // Widen the unlocked plug window so a missing reservation races.
                        std::thread::sleep(std::time::Duration::from_millis(2));
                        Ok(())
                    })
                })
            })
            .collect();

        let pads: Vec<_> = threads
            .into_iter()
            .map(|h| h.join().expect("no panic").expect("plug"))
            .collect();

        let mut serials: Vec<u32> = pads.iter().map(|h| h.serial_no()).collect();
        serials.sort_unstable();
        let unique = {
            let mut v = serials.clone();
            v.dedup();
            v.len()
        };
        assert_eq!(
            unique,
            serials.len(),
            "duplicate serial handed out: {serials:?}"
        );
        assert_eq!(serials, (1..=16).collect::<Vec<_>>());
        assert!(reserved_is_empty(&client));
        assert_eq!(target_count(&client), 16);
        drop(pads);
    }

    /// Slot conflicts advance serials; a later free slot succeeds.
    ///
    /// Includes `ERROR_INVALID_PARAMETER` (ViGEmBus: serial already in use).
    #[test]
    fn plugin_advances_past_retryable_slot_conflicts() {
        let client = test_client(4);
        let attempted: Mutex<Vec<u32>> = Mutex::new(Vec::new());

        let handle = client
            .plugin_internal_with::<TestPad, _>(test_spec(), |_bus, _target, serial| {
                attempted.lock().unwrap().push(serial);
                if serial < 4 {
                    // Mix driver-realistic conflict codes.
                    let code = if serial == 1 {
                        ERROR_INVALID_PARAMETER
                    } else if serial == 2 {
                        ERROR_ALREADY_EXISTS
                    } else {
                        ERROR_BUSY
                    };
                    Err(ioctl_err(Operation::PLUGIN_TARGET, code))
                } else {
                    Ok(())
                }
            })
            .expect("serial 4 should plug");

        assert_eq!(handle.serial_no(), 4);
        assert_eq!(attempted.into_inner().unwrap(), vec![1, 2, 3, 4]);
        assert!(reserved_is_empty(&client));
        assert_eq!(target_count(&client), 1);
        assert_eq!(inflight_plugs(&client), 0);
        // Drop handle so ClientInner::drop does not double-unplug during cleanup.
        drop(handle);
    }

    /// Exhausting the full scan window with only retryable errors returns
    /// [`ClientError::BusSerialsExhausted`] (not the local quota error).
    #[test]
    fn plugin_all_retryable_returns_bus_serials_exhausted() {
        let client = test_client(4);
        let attempted: Mutex<Vec<u32>> = Mutex::new(Vec::new());

        let result =
            client.plugin_internal_with::<TestPad, _>(test_spec(), |_bus, _target, serial| {
                attempted.lock().unwrap().push(serial);
                // Distinct last code so we can assert it is the one returned.
                let code = if serial == SERIAL_SCAN_LIMIT {
                    ERROR_BUSY
                } else {
                    ERROR_ALREADY_EXISTS
                };
                Err(ioctl_err(Operation::PLUGIN_TARGET, code))
            });

        assert_eq!(
            attempted.into_inner().unwrap(),
            (1..=SERIAL_SCAN_LIMIT).collect::<Vec<_>>()
        );
        match result {
            Err(ClientError::BusSerialsExhausted { scanned, source }) => {
                assert_eq!(scanned, SERIAL_SCAN_LIMIT);
                match source {
                    BusError::Ioctl { source, .. } => {
                        assert_eq!(source.win32_code(), Some(ERROR_BUSY));
                    }
                    other => panic!("expected last source Ioctl, got {other:?}"),
                }
            }
            other => panic!("expected BusSerialsExhausted, got {other:?}"),
        }
        assert!(reserved_is_empty(&client));
        assert_eq!(target_count(&client), 0);
        assert_eq!(inflight_plugs(&client), 0);
    }

    #[test]
    fn plugin_non_retryable_error_propagates_and_clears_reserved() {
        let client = test_client(4);

        let result =
            client.plugin_internal_with::<TestPad, _>(test_spec(), |_bus, _target, serial| {
                if serial == 1 {
                    Err(ioctl_err(Operation::PLUGIN_TARGET, ERROR_ALREADY_EXISTS))
                } else {
                    Err(ioctl_err(Operation::PLUGIN_TARGET, ERROR_ACCESS_DENIED))
                }
            });

        match result {
            Err(ClientError::Bus(BusError::AccessDenied { .. })) => {}
            other => panic!("expected AccessDenied, got {other:?}"),
        }
        assert!(reserved_is_empty(&client));
        assert_eq!(target_count(&client), 0);
        assert_eq!(inflight_plugs(&client), 0);
    }

    /// Same driver serial, new generation: a stale handle must not look attached
    /// and must not pass the hot `alive` gate used by `update`.
    ///
    /// Forces recycle by masking every serial except `7` via the Eager discovery
    /// inject (live ViGEmBus cannot reliably fill the 64-wide window).
    #[test]
    #[cfg(feature = "x360")]
    fn stale_handle_cannot_drive_recycled_serial() {
        use crate::controller::x360::X360Report;
        use crate::target::Xbox360;

        const SERIAL: u32 = 7;

        let client = Client::null_for_tests(2);
        client.set_serial_discovery(SerialDiscovery::Eager);
        let mut mask: HashSet<u32> = (1..=SERIAL_SCAN_LIMIT).collect();
        mask.remove(&SERIAL);
        client.set_test_discovery_snapshot(mask);

        let a = client
            .plugin_internal_with::<Xbox360, _>(test_spec(), |_bus, _t, serial| {
                assert_eq!(serial, SERIAL, "discovery mask must force serial {SERIAL}");
                Ok(())
            })
            .expect("plugin A")
            .assume_ready();
        let a_stale = a.clone();
        assert_eq!(a.serial_no(), SERIAL);
        assert!(a.is_attached().expect("client live"));

        // Null bus: unplug IOCTL fails, but map/`alive` still clear (see unplug_all).
        let _ = a.unplug();
        assert!(
            !a_stale.is_attached().expect("client live"),
            "stale must detach immediately on unplug"
        );

        let b = client
            .plugin_internal_with::<Xbox360, _>(test_spec(), |_bus, _t, serial| {
                assert_eq!(serial, SERIAL, "masked window must recycle serial {SERIAL}");
                Ok(())
            })
            .expect("plugin B on recycled serial")
            .assume_ready();
        assert_eq!(b.serial_no(), SERIAL, "B must reuse A's serial");
        assert!(b.is_attached().expect("client live"));

        assert!(
            !a_stale.is_attached().expect("client live"),
            "stale must stay detached after serial recycle (generation mismatch)"
        );

        let err = a_stale
            .update(&X360Report::default())
            .expect_err("stale handle must not update recycled serial");
        assert!(
            matches!(
                err,
                ClientError::TargetDoesNotExist {
                    serial_no: s
                } if s == SERIAL
            ),
            "expected TargetDoesNotExist {{ serial_no: {SERIAL} }}, got {err:?}"
        );

        // Do not call `b.update`: null bus handle cannot complete submit IOCTL.
        // Attachment + generation identity is what this test owns.
        let _ = b.unplug();
    }

    #[test]
    fn quota_blocks_before_any_ioctl() {
        let client = test_client(1);
        let first = client
            .plugin_internal_with::<TestPad, _>(test_spec(), |_bus, _target, _serial| Ok(()))
            .expect("first pad");

        let result =
            client.plugin_internal_with::<TestPad, _>(test_spec(), |_bus, _target, _serial| {
                panic!("plug IOCTL must not run when local quota is full");
            });

        assert!(
            matches!(result, Err(ClientError::NoFreeSlot { max_targets: 1 })),
            "got {result:?}"
        );
        assert_eq!(inflight_plugs(&client), 0);
        drop(first);
    }

    /// Concurrent plugs must not over-admit past `max_targets`.
    #[test]
    fn concurrent_plugin_respects_quota_under_contention() {
        use std::thread;
        use std::time::Duration;

        let client = Arc::new(test_client(4));
        let mut handles = Vec::new();

        for _ in 0..16 {
            let c = Arc::clone(&client);
            handles.push(thread::spawn(move || {
                c.plugin_internal_with::<TestPad, _>(test_spec(), |_bus, _target, _serial| {
                    // Widen the race window between quota check and map insert.
                    thread::sleep(Duration::from_millis(1));
                    Ok(())
                })
            }));
        }

        let mut ok = 0usize;
        let mut no_slot = 0usize;
        let mut pads = Vec::new();
        for h in handles {
            match h.join().expect("no panic") {
                Ok(pad) => {
                    ok += 1;
                    pads.push(pad);
                }
                Err(ClientError::NoFreeSlot { max_targets: 4 }) => no_slot += 1,
                Err(other) => panic!("unexpected error: {other:?}"),
            }
        }

        assert_eq!(ok, 4, "exactly four plugs should succeed");
        assert_eq!(no_slot, 12, "remaining threads should hit quota");
        assert_eq!(target_count(&client), 4);
        assert_eq!(inflight_plugs(&client), 0);
        assert!(reserved_is_empty(&client));
        drop(pads);
    }

    /// Allocator table: live / reserved / discovered occupancy and wrap past cursor.
    #[test]
    fn find_free_serial_skips_live_reserved_and_discovered() {
        // start=1: live [1,3] + reserved [2] -> next free is 4.
        assert_eq!(
            find_free_serial(&test_client_inner(&[1, 3], &[2], 4), 1, None),
            Some(4)
        );
        // Full window live -> None (scan range is SERIAL_SCAN_LIMIT, not max_targets).
        let full: Vec<u32> = (1..=SERIAL_SCAN_LIMIT).collect();
        assert_eq!(
            find_free_serial(&test_client_inner(&full, &[], 4), 1, None),
            None
        );
        // Reserved-but-not-live must be skipped - the plug/promote window.
        assert_eq!(
            find_free_serial(&test_client_inner(&[], &[1], 4), 1, None),
            Some(2)
        );
        // Only the first four live; max_targets no longer bounds the scan.
        assert_eq!(
            find_free_serial(&test_client_inner(&[1, 2, 3, 4], &[], 4), 1, None),
            Some(5)
        );
        // Discovered occupancy is skipped when present.
        let discovered: HashSet<u32> = [1u32, 2, 3].into_iter().collect();
        assert_eq!(
            find_free_serial(&test_client_inner(&[], &[], 4), 1, Some(&discovered)),
            Some(4)
        );
        // `None` discovery is a no-op (first free is 1).
        assert_eq!(
            find_free_serial(&test_client_inner(&[], &[], 4), 1, None),
            Some(1)
        );

        // Wrap past high cursor: low serials free after wrap.
        let mut inner = test_client_inner(&[63, 64], &[], 4);
        inner.scan_cursor = 63;
        assert_eq!(find_free_serial(&inner, 63, None), Some(1));
        assert_eq!(find_free_serial(&inner, 64, None), Some(1));

        // With 1 also taken, wrap lands on 2.
        let inner = test_client_inner(&[1, 63, 64], &[], 4);
        assert_eq!(find_free_serial(&inner, 63, None), Some(2));
    }

    /// Eager walks once per plug and never attempts masked serials (inject).
    #[test]
    fn eager_walks_once_per_plug() {
        let client = test_client(4);
        client.set_serial_discovery(SerialDiscovery::Eager);
        // Only 10 and 11 free - two plugs must take both, with one walk each.
        let mut mask: HashSet<u32> = (1..=SERIAL_SCAN_LIMIT).collect();
        mask.remove(&10);
        mask.remove(&11);
        client.set_test_discovery_snapshot(mask);

        let mut attempts: Vec<u32> = Vec::new();
        let a = client
            .plugin_internal_with::<TestPad, _>(test_spec(), |_bus, _t, serial| {
                attempts.push(serial);
                Ok(())
            })
            .expect("plug A");
        let b = client
            .plugin_internal_with::<TestPad, _>(test_spec(), |_bus, _t, serial| {
                attempts.push(serial);
                Ok(())
            })
            .expect("plug B");

        assert_eq!(attempts.len(), 2, "exactly one driver attempt per plug");
        assert!(
            attempts.iter().all(|s| *s == 10 || *s == 11),
            "Eager must only try free serials from the snapshot, got {attempts:?}"
        );
        assert_ne!(a.serial_no(), b.serial_no());
        assert_eq!(client.plug_conflict_count(), 0);
        assert_eq!(
            client.discovery_walk_count(),
            2,
            "Eager must walk once per plug call"
        );
        drop(a);
        drop(b);
    }

    /// Auto walks only after the first conflict: uncontended plugs pay zero
    /// enumeration cost; after a reject, occupancy is learned and skips apply.
    #[test]
    fn auto_walks_only_after_conflict() {
        // --- Uncontended: no walk ---
        // Inject is installed so that *if* Auto wrongly called `try_discover`,
        // `discovery_walk_count` would become 1 and this assertion would fail.
        {
            let client = test_client(4);
            client.set_serial_discovery(SerialDiscovery::Auto);
            client.set_test_discovery_snapshot([1u32, 2, 3].into_iter().collect());
            let _ = client
                .plugin_internal_with::<TestPad, _>(test_spec(), |_bus, _t, _s| Ok(()))
                .expect("plug");
            assert_eq!(
                client.discovery_walk_count(),
                0,
                "Auto must not call try_discover on an uncontended plug"
            );
            assert_eq!(client.plug_conflict_count(), 0);
        }

        // --- First conflict triggers one walk; known-taken serials are skipped ---
        {
            let client = test_client(4);
            client.set_serial_discovery(SerialDiscovery::Auto);
            // After the first reject, inject says 1..=3 are taken -> next try is 4.
            client.set_test_discovery_snapshot([1u32, 2, 3].into_iter().collect());

            let mut attempts: Vec<u32> = Vec::new();
            let handle = client
                .plugin_internal_with::<TestPad, _>(test_spec(), |_bus, _t, serial| {
                    attempts.push(serial);
                    if serial < 4 {
                        Err(BusError::Ioctl {
                            operation: Operation::PLUGIN_TARGET,
                            serial_no: Some(serial),
                            source: Win32Error::from_win32(87),
                        })
                    } else {
                        Ok(())
                    }
                })
                .expect("plug");
            assert_eq!(handle.serial_no(), 4);
            assert_eq!(attempts, vec![1, 4]);
            assert_eq!(client.plug_conflict_count(), 1);
            assert_eq!(
                client.discovery_walk_count(),
                1,
                "Auto must walk exactly once per plug call"
            );
            drop(handle);
        }
    }

    /// Full-window discovery mask is bypassed (must still issue a driver attempt),
    /// and the `discovery_attempted` latch prevents a second Auto walk after bypass.
    #[test]
    fn discovery_bypassed_when_window_fully_masked() {
        // --- Eager: full mask -> one bypass attempt, not sourceless exhaustion ---
        {
            let client = test_client(4);
            client.set_serial_discovery(SerialDiscovery::Eager);
            let full: HashSet<u32> = (1..=SERIAL_SCAN_LIMIT).collect();
            client.set_test_discovery_snapshot(full);

            let mut attempts = 0u32;
            let handle = client
                .plugin_internal_with::<TestPad, _>(test_spec(), |_bus, _target, serial| {
                    attempts += 1;
                    assert!(
                        (1..=SERIAL_SCAN_LIMIT).contains(&serial),
                        "serial out of window: {serial}"
                    );
                    Ok(())
                })
                .expect("plug must succeed after discovery bypass");
            assert_eq!(
                attempts, 1,
                "exactly one plug attempt after full-mask bypass"
            );
            assert_eq!(client.plug_conflict_count(), 0);
            // Inject counts as one walk; bypass must not trigger another.
            assert_eq!(client.discovery_walk_count(), 1);
            drop(handle);
        }

        // --- Auto: at most one occupancy walk per plug call, even when a
        // full-window snapshot is bypassed and further conflicts arrive.
        //
        // Without `discovery_attempted`, the sequence conflict -> full snapshot ->
        // bypass -> `discovered = None` -> next conflict would re-fire Auto and
        // walk again.
        {
            let client = test_client(4);
            client.set_serial_discovery(SerialDiscovery::Auto);
            let full: HashSet<u32> = (1..=SERIAL_SCAN_LIMIT).collect();
            client.set_test_discovery_snapshot(full);

            let mut attempts = 0u32;
            let handle = client
                .plugin_internal_with::<TestPad, _>(test_spec(), |_bus, _t, serial| {
                    attempts += 1;
                    // 1st: conflict -> Auto walks (full mask) -> find_free empty -> bypass.
                    // 2nd: another conflict after bypass (would re-walk without the gate).
                    // 3rd: success.
                    if attempts <= 2 {
                        Err(BusError::Ioctl {
                            operation: Operation::PLUGIN_TARGET,
                            serial_no: Some(serial),
                            source: Win32Error::from_win32(87),
                        })
                    } else {
                        Ok(())
                    }
                })
                .expect("plug after bypass");
            assert_eq!(attempts, 3, "conflict, conflict-after-bypass, success");
            assert_eq!(
                client.discovery_walk_count(),
                1,
                "bypass must not allow a second Auto walk"
            );
            assert_eq!(client.plug_conflict_count(), 2);
            drop(handle);
        }
    }

    /// Unplug removes the map entry but keeps the serial reserved until the guard
    /// drops; generation mismatch yields no guard and leaves state intact.
    #[test]
    fn begin_unplug_reserves_serial_until_guard_drops() {
        let serial_no = 1u32;
        let generation = 1u64;
        let alive = Arc::new(AtomicBool::new(true));

        let mut inner = test_client_inner(&[serial_no], &[], 4);
        inner.targets.get_mut(&serial_no).unwrap().alive = Arc::clone(&alive);
        inner.targets.get_mut(&serial_no).unwrap().generation = generation;
        let client = Arc::new(Mutex::new(inner));

        let guard = begin_unplug(&client, serial_no, generation, &alive)
            .expect("lock")
            .expect("live target");

        assert!(!alive.load(Ordering::Acquire));
        {
            let g = client.lock().unwrap();
            assert!(!g.targets.contains_key(&serial_no));
            assert!(g.reserved.contains(&serial_no));
            // Concurrent plug must not reuse serial mid-unplug.
            assert_eq!(find_free_serial(&g, 1, None), Some(2));
        }

        drop(guard);
        {
            let g = client.lock().unwrap();
            assert!(!g.reserved.contains(&serial_no));
            assert_eq!(find_free_serial(&g, 1, None), Some(1));
        }

        // Generation mismatch: no guard, no mutation.
        let alive2 = Arc::new(AtomicBool::new(true));
        let mut inner2 = test_client_inner(&[serial_no], &[], 4);
        inner2.targets.get_mut(&serial_no).unwrap().generation = 1;
        let client2 = Arc::new(Mutex::new(inner2));

        let guard2 = begin_unplug(&client2, serial_no, 99, &alive2).expect("lock");
        assert!(guard2.is_none());
        assert!(alive2.load(Ordering::Acquire));
        assert!(client2.lock().unwrap().targets.contains_key(&serial_no));
        assert!(client2.lock().unwrap().reserved.is_empty());
    }

    /// Poisoned client mutex: handle drop still clears map/`alive` (null bus handle).
    #[test]
    fn target_handle_drop_recovers_from_poisoned_client_mutex() {
        use crate::target::{Ready, TargetHandle};
        use std::panic::{AssertUnwindSafe, catch_unwind};

        let serial_no = 1u32;
        let generation = 1u64;
        let alive = Arc::new(AtomicBool::new(true));

        let mut inner = test_client_inner(&[serial_no], &[], 16);
        inner.targets.get_mut(&serial_no).unwrap().alive = Arc::clone(&alive);
        inner.next_gen = generation + 1;
        let bus = inner.bus.clone();
        let client_inner = Arc::new(Mutex::new(inner));

        let handle: TargetHandle<TestPad, Ready> = TargetHandle::new(
            serial_no,
            generation,
            Arc::clone(&alive),
            bus,
            Arc::downgrade(&client_inner),
        );

        let poison_result = catch_unwind(AssertUnwindSafe(|| {
            let _guard = client_inner.lock().unwrap();
            panic!("intentional client mutex poison");
        }));
        assert!(poison_result.is_err());
        assert!(
            client_inner.is_poisoned(),
            "client mutex must be poisoned before handle drop"
        );

        drop(handle);

        let inner = lock_client_recover(&client_inner);
        assert!(
            inner.targets.is_empty(),
            "poisoned drop must still remove the target from the client map"
        );
        assert!(
            !alive.load(Ordering::Acquire),
            "poisoned drop must mark the target dead"
        );
    }
}
