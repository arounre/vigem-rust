//! Public APIs return [`ClientError`]. When the problem is the driver or a pad,
//! match the nested [`BusError`]. [`NotificationReceiver`]
//! uses [`RecvError`] / [`TryRecvError`] / [`RecvTimeoutError`].
//!
//! # What to do when things fail
//!
//! | Error | Typical cause | Recovery |
//! |-------|---------------|----------|
//! | [`BusError::BusNotFound`] | ViGEmBus not installed or not started | Install / start the driver ([vigem.org](https://vigem.org)) |
//! | [`BusError::VersionMismatch`] | Installed ViGEmBus is not compatible | Install a current Nefarius build (1.21.442 / 1.22.0) |
//! | [`BusError::AccessDenied`] | No permission to open the bus | Run elevated or fix device permissions |
//! | [`BusError::DeviceGone`] | Driver removed, or pad no longer exists | Drop handles; reconnect if the bus died |
//! | [`BusError::TargetNotReady`] | Pad still coming up after plug | Retry `update` shortly (or use `wait_for_ready`) |
//! | [`ClientError::ReadyTimeout`] | `wait_for_ready` timed out | Call again on the same pending handle, or `assume_ready` |
//! | [`ClientError::NoFreeSlot`] | This client is at its own `max_targets` | Unplug one of your pads, or connect with a higher `max_targets` |
//! | [`ClientError::BusSerialsExhausted`] | All scanned serials taken, possibly by other processes | Unplug pads anywhere on the machine, or retry later |
//! | [`ClientError::TargetDoesNotExist`] | Already unplugged | Stop using that handle |
//! | [`RecvError::Disconnected`] | Notification worker stopped | Stop reading that receiver |
//! | [`RecvError::Bus`] | Driver failed while listening | Treat as terminal for that subscription |
//!
//! Prefer these variants for recovery. Use [`Win32Error`] /
//! [`BusError::win32_source`] only when logging raw Windows status codes.

use core::fmt;
use std::time::Duration;

use thiserror::Error;

// Win32 codes as raw `u32` so this module does not name `windows` types in the public API.
pub(crate) const ERROR_FILE_NOT_FOUND: u32 = 2;
const ERROR_ACCESS_DENIED: u32 = 5;
// `STATUS_DEVICE_DOES_NOT_EXIST` -> Win32 55; ViGEm unknown / never-plugged serial.
const ERROR_DEV_NOT_EXIST: u32 = 55;
// Duplicate serial on `VIGEM_PLUGIN_TARGET` (`STATUS_OBJECT_NAME_EXISTS` remapped in
// `Bus_PlugInDevice`). Also a `Size` mismatch on other ops, so only a slot conflict on plugin.
const ERROR_INVALID_PARAMETER: u32 = 87;
const ERROR_BUSY: u32 = 170;
const ERROR_ALREADY_EXISTS: u32 = 183;
const ERROR_NO_MORE_ITEMS: u32 = 259;
const ERROR_NO_SUCH_DEVICE: u32 = 433;
const ERROR_OPERATION_ABORTED: u32 = 995;
const ERROR_DEVICE_NOT_CONNECTED: u32 = 1167;

// `FACILITY_WIN32` (`HRESULT_FROM_WIN32` sets severity + facility 7).
const FACILITY_WIN32: i32 = 7;
const HRESULT_FACILITY_MASK: i32 = 0x07FF_0000;
const HRESULT_CODE_MASK: i32 = 0x0000_FFFF;
const HRESULT_SEVERITY_FAIL: i32 = 1i32 << 31;

/// A Windows status code kept as a plain integer.
///
/// Use [`win32_code`](Self::win32_code) or [`hresult`](Self::hresult) when logging.
/// Display includes the system message when available.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Win32Error(i32);

impl Win32Error {
    /// Build from a raw `HRESULT` value.
    #[inline]
    #[must_use]
    pub const fn from_hresult(hr: i32) -> Self {
        Self(hr)
    }

    /// Build from a classic Win32 error code (e.g. `5` for access denied).
    #[inline]
    #[must_use]
    pub const fn from_win32(code: u32) -> Self {
        let hr = if (code as i32) <= 0 {
            code as i32
        } else {
            ((code as i32) & HRESULT_CODE_MASK) | (FACILITY_WIN32 << 16) | HRESULT_SEVERITY_FAIL
        };
        Self(hr)
    }

    /// The full `HRESULT` as an `i32`.
    #[inline]
    #[must_use]
    pub const fn hresult(self) -> i32 {
        self.0
    }

    /// The classic Win32 error code, or `None` if this is not a Win32-style HRESULT.
    #[inline]
    #[must_use]
    pub const fn win32_code(self) -> Option<u32> {
        let hr = self.0;
        // NTSTATUS-style successes are non-negative; only failure HRESULTs with
        // FACILITY_WIN32 carry a Win32 code in the low 16 bits.
        if hr < 0 && ((hr & HRESULT_FACILITY_MASK) >> 16) == FACILITY_WIN32 {
            Some((hr & HRESULT_CODE_MASK) as u32)
        } else {
            None
        }
    }

    /// `ERROR_OPERATION_ABORTED` (drop-driven cancel).
    #[inline]
    #[must_use]
    pub(crate) const fn is_operation_aborted(self) -> bool {
        matches!(self.win32_code(), Some(ERROR_OPERATION_ABORTED))
    }

    /// Narrow slot-conflict codes: `ERROR_BUSY` and `ERROR_ALREADY_EXISTS`.
    ///
    /// Deliberately excludes `ERROR_INVALID_PARAMETER`, which ViGEmBus returns
    /// for a duplicate serial but which means a malformed request on other
    /// operations. Use [`Self::is_plug_serial_conflict`] to classify
    /// `VIGEM_PLUGIN_TARGET`.
    #[inline]
    #[must_use]
    pub(crate) const fn is_slot_conflict(self) -> bool {
        matches!(
            self.win32_code(),
            Some(ERROR_BUSY) | Some(ERROR_ALREADY_EXISTS)
        )
    }

    /// Codes that mean "this serial is taken" on `VIGEM_PLUGIN_TARGET`.
    ///
    /// Superset of [`Self::is_slot_conflict`]: adds `ERROR_INVALID_PARAMETER`,
    /// which is what ViGEmBus actually returns for a duplicate serial. Only
    /// valid for `VIGEM_PLUGIN_TARGET`. On other operations that code means a
    /// malformed request.
    #[inline]
    #[must_use]
    pub(crate) const fn is_plug_serial_conflict(self) -> bool {
        self.is_slot_conflict() || matches!(self.win32_code(), Some(ERROR_INVALID_PARAMETER))
    }

    /// Convert to [`windows::core::Error`].
    ///
    /// Puts `windows::core::Error` in this crate's public API, so a major bump of
    /// the `windows` crate can break callers of this method.
    #[cfg(feature = "windows-error")]
    #[cfg_attr(docsrs, doc(cfg(feature = "windows-error")))]
    #[must_use]
    pub fn to_windows_error(self) -> windows::core::Error {
        windows::core::Error::from_hresult(windows::core::HRESULT(self.0))
    }
}

impl fmt::Display for Win32Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        #[cfg(windows)]
        {
            let message =
                windows::core::Error::from_hresult(windows::core::HRESULT(self.0)).message();
            if !message.is_empty() {
                return write!(f, "{message} (0x{:08X})", self.0 as u32);
            }
        }
        if let Some(code) = self.win32_code() {
            write!(f, "Win32 error {code} (HRESULT 0x{:08X})", self.0 as u32)
        } else {
            write!(f, "HRESULT 0x{:08X}", self.0 as u32)
        }
    }
}

impl std::error::Error for Win32Error {}

#[cfg(windows)]
impl From<&windows::core::Error> for Win32Error {
    #[inline]
    fn from(err: &windows::core::Error) -> Self {
        Self::from_hresult(err.code().0)
    }
}

#[cfg(windows)]
impl From<windows::core::Error> for Win32Error {
    #[inline]
    fn from(err: windows::core::Error) -> Self {
        Self::from_hresult(err.code().0)
    }
}

/// High-level step an error came from.
///
/// Compare against the constants (e.g. `err.operation() == Some(Operation::PLUGIN_TARGET)`).
/// [`as_str`](Self::as_str) / [`Display`](fmt::Display) are stable for logging;
/// new constants may appear in minor releases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Operation(&'static str);

impl Operation {
    /// Named step not covered by the associated constants (logs, tests, newer driver).
    #[must_use]
    pub const fn from_static(name: &'static str) -> Self {
        Self(name)
    }

    /// Stable step name for logs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.0
    }

    /// Plug a virtual pad.
    pub const PLUGIN_TARGET: Self = Self("VIGEM_PLUGIN_TARGET");
    /// Unplug a virtual pad.
    pub const UNPLUG_TARGET: Self = Self("VIGEM_UNPLUG_TARGET");
    /// Wait after plug until the pad is ready for input.
    pub const WAIT_DEVICE_READY: Self = Self("VIGEM_WAIT_DEVICE_READY");
    /// Version check at connect.
    pub const CHECK_VERSION: Self = Self("VIGEM_CHECK_VERSION");
    /// Opening the ViGEmBus device at connect.
    pub const OPEN_BUS: Self = Self("OPEN_BUS");
    /// Send an Xbox 360 input report.
    pub const XUSB_SUBMIT_REPORT: Self = Self("XUSB_SUBMIT_REPORT");
    /// Send a DualShock 4 input report.
    pub const DS4_SUBMIT_REPORT: Self = Self("DS4_SUBMIT_REPORT");
    /// Send an extended DualShock 4 input report.
    pub const DS4_SUBMIT_REPORT_EX: Self = Self("DS4_SUBMIT_REPORT_EX");
    /// Query the Xbox 360 XInput user index.
    pub const XUSB_GET_USER_INDEX: Self = Self("XUSB_GET_USER_INDEX");
    /// Wait for Xbox 360 rumble / LED feedback.
    pub const XUSB_REQUEST_NOTIFICATION: Self = Self("XUSB_REQUEST_NOTIFICATION");
    /// Wait for DualShock 4 rumble / lightbar feedback.
    pub const DS4_REQUEST_NOTIFICATION: Self = Self("DS4_REQUEST_NOTIFICATION");
    /// Wait for a raw DualShock 4 host output report.
    pub const DS4_AWAIT_OUTPUT_AVAILABLE: Self = Self("DS4_AWAIT_OUTPUT_AVAILABLE");
    /// Create a local waitable event used for async I/O.
    pub const CREATE_EVENT_W: Self = Self("CreateEventW");
}

impl fmt::Display for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

#[inline]
fn serial_label(serial_no: Option<u32>) -> impl fmt::Display {
    struct Label(Option<u32>);
    impl fmt::Display for Label {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self.0 {
                Some(n) => write!(f, "serial {n}"),
                None => f.write_str("bus-wide"),
            }
        }
    }
    Label(serial_no)
}

/// Problems talking to the ViGEmBus driver or a virtual pad on it.
///
/// Usually wrapped in [`ClientError::Bus`].
#[derive(Debug, Clone, Error)]
#[non_exhaustive]
pub enum BusError {
    /// The bus or pad is gone (driver stopped, device removed, or serial gone).
    ///
    /// Do not retry forever on the same handle; if the whole bus died, create a
    /// new [`Client`](crate::Client).
    #[error(
        "ViGEmBus device gone during `{operation}` ({})",
        serial_label(*serial_no)
    )]
    DeviceGone {
        /// Step that failed.
        operation: Operation,
        /// Pad serial, or `None` for bus-wide ops.
        serial_no: Option<u32>,
    },

    /// Windows denied access to ViGEmBus (permissions).
    #[error(
        "ViGEmBus access denied for `{operation}` ({})",
        serial_label(*serial_no)
    )]
    AccessDenied {
        /// Step that failed.
        operation: Operation,
        /// Pad serial, or `None` for bus-wide ops.
        serial_no: Option<u32>,
    },

    /// The pad is not accepting input yet (usually right after plug).
    ///
    /// Retryable. [`wait_for_ready`](crate::TargetHandle::wait_for_ready) covers
    /// most of this window; a later `update` after plugging many pads at once
    /// may still see this briefly.
    #[error("target {serial_no} is not ready to accept reports")]
    TargetNotReady {
        /// Serial of the pad that rejected the report.
        serial_no: u32,
    },

    /// A driver call failed with a status we do not classify further.
    ///
    /// See [`source`](Self::Ioctl::source) / [`win32_source`](Self::win32_source)
    /// for the raw Windows status.
    #[error(
        "ViGEmBus IOCTL `{operation}` failed ({}): {source}",
        serial_label(*serial_no)
    )]
    Ioctl {
        /// Step that failed.
        operation: Operation,
        /// Pad serial, or `None` for bus-wide ops.
        serial_no: Option<u32>,
        /// Underlying Windows status.
        #[source]
        source: Win32Error,
    },

    /// Finding ViGEmBus, or listing which serials are in use, failed.
    ///
    /// Seen at connect and from [`Client::occupied_serials`](crate::Client::occupied_serials).
    /// Plug-time serial lookup swallows this and retries; it does not fail the plug.
    #[error("device enumeration failed: {source}")]
    Enumeration {
        /// Underlying Windows status.
        #[source]
        source: Win32Error,
    },

    /// Installed ViGEmBus is not compatible with this crate.
    ///
    /// Update the driver (or this crate) so the versions match. Used with the
    /// last official Nefarius builds, 1.21.442 and 1.22.0 (same driver).
    #[error("ViGEmBus driver version mismatch (client requested {requested:#06x})")]
    VersionMismatch {
        /// Interface version this crate requested.
        requested: u32,
    },

    /// No usable ViGEmBus device was found.
    ///
    /// Typical first-run failure: the driver is not installed or not started.
    /// Install from [vigem.org](https://vigem.org).
    #[error("ViGEmBus driver not found; install it from https://vigem.org")]
    BusNotFound {
        /// Last open or discovery error, when one was recorded.
        #[source]
        source: Option<Win32Error>,
    },
}

impl BusError {
    #[inline]
    pub(crate) fn from_ioctl(
        operation: Operation,
        serial_no: Option<u32>,
        source: windows::core::Error,
    ) -> Self {
        Self::classify_ioctl(operation, serial_no, Win32Error::from(source))
    }

    /// Same classification as [`from_ioctl`](Self::from_ioctl) (for tests).
    #[inline]
    pub(crate) fn classify_ioctl(
        operation: Operation,
        serial_no: Option<u32>,
        source: Win32Error,
    ) -> Self {
        match source.win32_code() {
            Some(ERROR_DEV_NOT_EXIST)
            | Some(ERROR_NO_SUCH_DEVICE)
            | Some(ERROR_DEVICE_NOT_CONNECTED) => Self::DeviceGone {
                operation,
                serial_no,
            },
            // FILE_NOT_FOUND on an open bus handle -> device gone (not CreateEventW).
            Some(ERROR_FILE_NOT_FOUND) if is_established_handle_op(operation) => Self::DeviceGone {
                operation,
                serial_no,
            },
            Some(ERROR_ACCESS_DENIED) => Self::AccessDenied {
                operation,
                serial_no,
            },
            Some(ERROR_NO_MORE_ITEMS) if is_submit_report_op(operation) => {
                if let Some(serial_no) = serial_no {
                    return Self::TargetNotReady { serial_no };
                }
                Self::Ioctl {
                    operation,
                    serial_no,
                    source,
                }
            }
            _ => Self::Ioctl {
                operation,
                serial_no,
                source,
            },
        }
    }

    #[inline]
    pub(crate) fn enumeration(source: windows::core::Error) -> Self {
        Self::Enumeration {
            source: Win32Error::from(source),
        }
    }

    /// Step that failed, if this variant records one.
    #[inline]
    #[must_use]
    pub fn operation(&self) -> Option<Operation> {
        match self {
            Self::DeviceGone { operation, .. }
            | Self::AccessDenied { operation, .. }
            | Self::Ioctl { operation, .. } => Some(*operation),
            Self::TargetNotReady { .. }
            | Self::Enumeration { .. }
            | Self::VersionMismatch { .. }
            | Self::BusNotFound { .. } => None,
        }
    }

    /// Raw Windows status, if this variant keeps one (`Ioctl`, `Enumeration`,
    /// sometimes `BusNotFound`).
    #[inline]
    #[must_use]
    pub fn win32_source(&self) -> Option<Win32Error> {
        match self {
            Self::Ioctl { source, .. } | Self::Enumeration { source } => Some(*source),
            Self::BusNotFound { source } => *source,
            Self::DeviceGone { .. }
            | Self::AccessDenied { .. }
            | Self::TargetNotReady { .. }
            | Self::VersionMismatch { .. } => None,
        }
    }

    /// [`win32_source`](Self::win32_source) as [`windows::core::Error`].
    ///
    /// See [`Win32Error::to_windows_error`] for the public-API stability note.
    #[cfg(feature = "windows-error")]
    #[cfg_attr(docsrs, doc(cfg(feature = "windows-error")))]
    #[must_use]
    pub fn as_windows_error(&self) -> Option<windows::core::Error> {
        self.win32_source().map(Win32Error::to_windows_error)
    }
}

// Ops on an already-open bus handle (not local `CreateEventW`).
#[inline]
fn is_established_handle_op(operation: Operation) -> bool {
    operation != Operation::CREATE_EVENT_W
}

// Submit ops that may return `ERROR_NO_MORE_ITEMS` while the HID child comes up.
#[inline]
fn is_submit_report_op(operation: Operation) -> bool {
    operation == Operation::XUSB_SUBMIT_REPORT
        || operation == Operation::DS4_SUBMIT_REPORT
        || operation == Operation::DS4_SUBMIT_REPORT_EX
}

/// Errors from client and controller APIs (`connect`, `plug`, `update`, ...).
///
/// Most driver failures are [`ClientError::Bus`]. Plug distinguishes
/// [`NoFreeSlot`](Self::NoFreeSlot) (this client's cap) from
/// [`BusSerialsExhausted`](Self::BusSerialsExhausted) (machine-wide serial window).
#[derive(Debug, Clone, Error)]
#[non_exhaustive]
pub enum ClientError {
    /// Driver or bus failure. Display forwards to the inner [`BusError`].
    ///
    /// [`Error::source`](std::error::Error::source) is the nested [`Win32Error`]
    /// when the variant keeps one, not the `BusError` itself.
    #[error(transparent)]
    Bus(#[from] BusError),

    /// This client already has as many pads as it is allowed to own
    /// ([`max_targets`](crate::ClientBuilder::max_targets)), not a machine-wide
    /// shortage ([`BusSerialsExhausted`](Self::BusSerialsExhausted)).
    #[error("no free target slot (max_targets = {max_targets})")]
    NoFreeSlot {
        /// The `max_targets` limit that was in effect.
        max_targets: u32,
    },

    /// Every serial in the search window is already in use, including by other processes.
    ///
    /// Can happen even when this client owns nothing. [`source`](Self::BusSerialsExhausted::source)
    /// is the last driver rejection seen while scanning. See
    /// [serials](crate::client#serials-and-other-processes).
    #[error(
        "no free ViGEmBus serial after scanning {scanned} serials (serials are shared driver-wide)"
    )]
    BusSerialsExhausted {
        /// How many serials were tried before giving up.
        scanned: u32,
        /// The last driver error seen while scanning.
        #[source]
        source: BusError,
    },

    /// Invalid `max_targets` (zero or above the crate limit).
    ///
    /// Returned from [`ClientBuilder::connect`](crate::ClientBuilder::connect)
    /// before the driver is contacted.
    #[error("max_targets must be between 1 and {max_allowed} inclusive (got {requested})")]
    InvalidMaxTargets {
        /// Value you requested.
        requested: u32,
        /// Highest value this crate accepts ([`MAX_TARGETS_LIMIT`](crate::MAX_TARGETS_LIMIT)).
        max_allowed: u32,
    },

    /// This handle's pad is no longer plugged in.
    #[error("target {serial_no} is no longer plugged in")]
    TargetDoesNotExist {
        /// Serial that was expected to still be live.
        serial_no: u32,
    },

    /// The [`Client`](crate::Client) that owned this pad was dropped.
    #[error("the owning Client has been dropped")]
    ClientNoLongerExists,

    /// [`wait_for_ready`](crate::TargetHandle::wait_for_ready) timed out after
    /// [`timeout`](Self::ReadyTimeout::timeout).
    ///
    /// Distinct from a transient `update` failure ([`BusError::TargetNotReady`]).
    /// Call again on the same pending handle, or [`assume_ready`](crate::TargetHandle::assume_ready)
    /// if you accept that early `update`s may do nothing. A timeout does not unplug
    /// the pad.
    #[error("wait_for_ready timed out after {timeout:?}")]
    ReadyTimeout {
        /// How long `wait_for_ready` waited before giving up.
        timeout: Duration,
    },

    /// Client state was left unusable after a panic on another thread.
    ///
    /// Drop the client and connect again.
    #[error("client internal state is poisoned (a previous operation panicked)")]
    Poisoned,
}

/// Failure from [`NotificationReceiver::recv`](crate::NotificationReceiver::recv).
///
/// `while let Ok(n) = rx.recv()` drains until the worker stops.
#[derive(Debug, Clone, Error)]
#[non_exhaustive]
pub enum RecvError {
    /// The worker stopped (unplug, client drop, last receiver dropped, or a
    /// fatal bus error closed the hub).
    #[error("notification worker disconnected")]
    Disconnected,

    /// Driver failure while listening for host feedback.
    ///
    /// Often terminal for this subscription; remaining subscribers see the same
    /// error, then [`Disconnected`](Self::Disconnected).
    #[error(transparent)]
    Bus(#[from] BusError),
}

/// Failure from [`NotificationReceiver::try_recv`](crate::NotificationReceiver::try_recv).
#[derive(Debug, Clone, Error)]
#[non_exhaustive]
pub enum TryRecvError {
    /// No event is waiting.
    #[error("no notification available")]
    Empty,

    /// The worker stopped (unplug, client drop, last receiver dropped, or a
    /// fatal bus error closed the hub).
    #[error("notification worker disconnected")]
    Disconnected,

    /// Driver failure while listening for host feedback.
    #[error(transparent)]
    Bus(#[from] BusError),
}

/// Failure from [`NotificationReceiver::recv_timeout`](crate::NotificationReceiver::recv_timeout).
#[derive(Debug, Clone, Error)]
#[non_exhaustive]
pub enum RecvTimeoutError {
    /// `timeout` elapsed with no event.
    #[error("timed out waiting for a notification")]
    Timeout,

    /// The worker stopped (unplug, client drop, last receiver dropped, or a
    /// fatal bus error closed the hub).
    #[error("notification worker disconnected")]
    Disconnected,

    /// Driver failure while listening for host feedback.
    #[error(transparent)]
    Bus(#[from] BusError),
}

impl From<RecvError> for TryRecvError {
    fn from(err: RecvError) -> Self {
        match err {
            RecvError::Disconnected => Self::Disconnected,
            RecvError::Bus(e) => Self::Bus(e),
        }
    }
}

impl From<RecvError> for RecvTimeoutError {
    fn from(err: RecvError) -> Self {
        match err {
            RecvError::Disconnected => Self::Disconnected,
            RecvError::Bus(e) => Self::Bus(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_ioctl_table() {
        let e = Win32Error::from_win32(ERROR_ACCESS_DENIED);
        assert_eq!(e.win32_code(), Some(ERROR_ACCESS_DENIED));
        // HRESULT_FROM_WIN32(5) = 0x80070005
        assert_eq!(e.hresult() as u32, 0x8007_0005);

        for code in [
            ERROR_DEV_NOT_EXIST,
            ERROR_NO_SUCH_DEVICE,
            ERROR_DEVICE_NOT_CONNECTED,
            ERROR_FILE_NOT_FOUND,
        ] {
            let err = BusError::classify_ioctl(
                Operation::XUSB_SUBMIT_REPORT,
                Some(1),
                Win32Error::from_win32(code),
            );
            assert!(
                matches!(
                    err,
                    BusError::DeviceGone {
                        operation,
                        serial_no: Some(1)
                    } if operation == Operation::XUSB_SUBMIT_REPORT
                ),
                "code {code} => {err:?}"
            );
            let msg = err.to_string();
            assert!(
                msg.contains("serial 1") && !msg.contains("Some("),
                "Display must not show Debug Option: {msg}"
            );
        }

        // FILE_NOT_FOUND on CreateEventW is not DeviceGone.
        let err = BusError::classify_ioctl(
            Operation::CREATE_EVENT_W,
            Some(1),
            Win32Error::from_win32(ERROR_FILE_NOT_FOUND),
        );
        assert!(
            matches!(
                err,
                BusError::Ioctl {
                    operation,
                    ..
                } if operation == Operation::CREATE_EVENT_W
            ),
            "got {err:?}"
        );

        let err = BusError::classify_ioctl(
            Operation::PLUGIN_TARGET,
            Some(2),
            Win32Error::from_win32(ERROR_ACCESS_DENIED),
        );
        assert!(matches!(
            err,
            BusError::AccessDenied {
                operation,
                serial_no: Some(2)
            } if operation == Operation::PLUGIN_TARGET
        ));
        let msg = err.to_string();
        assert!(
            msg.contains("serial 2") && !msg.contains("Some("),
            "Display must not show Debug Option: {msg}"
        );

        let ready = BusError::classify_ioctl(
            Operation::XUSB_SUBMIT_REPORT,
            Some(3),
            Win32Error::from_win32(ERROR_NO_MORE_ITEMS),
        );
        assert!(matches!(ready, BusError::TargetNotReady { serial_no: 3 }));

        let not_submit = BusError::classify_ioctl(
            Operation::PLUGIN_TARGET,
            Some(3),
            Win32Error::from_win32(ERROR_NO_MORE_ITEMS),
        );
        assert!(
            matches!(not_submit, BusError::Ioctl { .. }),
            "non-submit must stay Ioctl, got {not_submit:?}"
        );
    }

    #[test]
    fn slot_conflict_is_narrow_plug_conflict_includes_invalid_parameter() {
        let busy = Win32Error::from_win32(ERROR_BUSY);
        let exists = Win32Error::from_win32(ERROR_ALREADY_EXISTS);
        let invalid = Win32Error::from_win32(ERROR_INVALID_PARAMETER);
        let denied = Win32Error::from_win32(ERROR_ACCESS_DENIED);

        assert!(busy.is_slot_conflict());
        assert!(exists.is_slot_conflict());
        assert!(!invalid.is_slot_conflict());
        assert!(!denied.is_slot_conflict());

        assert!(busy.is_plug_serial_conflict());
        assert!(exists.is_plug_serial_conflict());
        assert!(invalid.is_plug_serial_conflict());
        assert!(!denied.is_plug_serial_conflict());
    }

    #[test]
    fn error_display_and_source_chain() {
        let source = BusError::Ioctl {
            operation: Operation::PLUGIN_TARGET,
            serial_no: Some(7),
            source: Win32Error::from_win32(ERROR_INVALID_PARAMETER),
        };
        let source_msg = source.to_string();
        let err = ClientError::BusSerialsExhausted {
            scanned: 64,
            source,
        };
        let msg = err.to_string();
        assert!(
            msg.contains("64") && msg.contains("shared driver-wide"),
            "got {msg}"
        );
        let chained = std::error::Error::source(&err)
            .expect("BusSerialsExhausted must expose source")
            .to_string();
        assert_eq!(chained, source_msg);

        let e = BusError::AccessDenied {
            operation: Operation::from_static("CreateFileW"),
            serial_no: None,
        };
        let msg = e.to_string();
        assert!(
            msg.contains("bus-wide") && !msg.contains("None"),
            "got {msg}"
        );

        let bus = BusError::AccessDenied {
            operation: Operation::from_static("CreateFileW"),
            serial_no: None,
        };
        let bus_msg = bus.to_string();
        let client = ClientError::from(bus);
        assert_eq!(
            client.to_string(),
            bus_msg,
            "ClientError::Bus must forward Display to BusError"
        );
        assert!(
            client.to_string().contains("access denied"),
            "got {:?}",
            client.to_string()
        );
        // Regression: Display must not collapse to a generic "bus error".
        assert_ne!(client.to_string(), "bus error");

        // `#[error(transparent)]` also skips BusError in `source()`.
        let client = ClientError::from(BusError::Ioctl {
            operation: Operation::XUSB_SUBMIT_REPORT,
            serial_no: Some(1),
            source: Win32Error::from_win32(5),
        });
        assert!(
            std::error::Error::source(&client)
                .and_then(|s| s.downcast_ref::<Win32Error>())
                .is_some(),
            "ClientError::Bus source() must skip BusError and yield its Win32Error"
        );
    }

    #[test]
    fn notification_recv_error_display_and_from() {
        assert_eq!(
            RecvError::Disconnected.to_string(),
            "notification worker disconnected"
        );
        assert_eq!(TryRecvError::Empty.to_string(), "no notification available");
        assert_eq!(
            RecvTimeoutError::Timeout.to_string(),
            "timed out waiting for a notification"
        );

        let bus = BusError::TargetNotReady { serial_no: 3 };
        let bus_msg = bus.to_string();
        let recv = RecvError::from(bus.clone());
        assert_eq!(
            recv.to_string(),
            bus_msg,
            "RecvError::Bus must forward Display to BusError"
        );

        let try_err = TryRecvError::from(RecvError::Disconnected);
        assert!(matches!(try_err, TryRecvError::Disconnected));
        let try_bus = TryRecvError::from(RecvError::from(bus.clone()));
        assert!(matches!(
            try_bus,
            TryRecvError::Bus(BusError::TargetNotReady { serial_no: 3 })
        ));

        let timeout_err = RecvTimeoutError::from(RecvError::Disconnected);
        assert!(matches!(timeout_err, RecvTimeoutError::Disconnected));
        let timeout_bus = RecvTimeoutError::from(RecvError::from(bus));
        assert!(matches!(
            timeout_bus,
            RecvTimeoutError::Bus(BusError::TargetNotReady { serial_no: 3 })
        ));
    }

    #[test]
    fn ready_timeout_display_includes_duration() {
        let err = ClientError::ReadyTimeout {
            timeout: Duration::from_secs(3),
        };
        assert_eq!(err.to_string(), "wait_for_ready timed out after 3s");

        let bus = BusError::TargetNotReady { serial_no: 1 };
        assert_ne!(
            err.to_string(),
            bus.to_string(),
            "ReadyTimeout must not look like TargetNotReady"
        );
    }
}
