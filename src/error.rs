//! Public APIs return [`ClientError`]. When the problem is the driver or a pad,
//! match the nested [`BusError`]. [`NotificationReceiver`](crate::NotificationReceiver)
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
//! | [`BusError::TargetNotReady`] | Host had no read waiting; the report was dropped (most often right after plug) | Send that report again if it must arrive; see the variant docs |
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
const ERROR_INVALID_FUNCTION: u32 = 1;
pub(crate) const ERROR_FILE_NOT_FOUND: u32 = 2;
const ERROR_ACCESS_DENIED: u32 = 5;
// `STATUS_NOT_SUPPORTED`: the driver's answer to a `CHECK_VERSION` it rejects.
const ERROR_NOT_SUPPORTED: u32 = 50;
// `STATUS_DEVICE_DOES_NOT_EXIST` -> Win32 55; ViGEm unknown / never-plugged serial.
const ERROR_DEV_NOT_EXIST: u32 = 55;
// Duplicate serial on `VIGEM_PLUGIN_TARGET` (`STATUS_OBJECT_NAME_EXISTS` remapped in
// `Bus_PlugInDevice`). Also a `Size` mismatch on other ops, so only a slot conflict on plugin.
const ERROR_INVALID_PARAMETER: u32 = 87;
const ERROR_BUSY: u32 = 170;
const ERROR_ALREADY_EXISTS: u32 = 183;
const ERROR_NO_MORE_ITEMS: u32 = 259;
const ERROR_NO_SUCH_DEVICE: u32 = 433;
// Win32 form of `STATUS_INVALID_DEVICE_OBJECT_PARAMETER`. `IOCTL_XUSB_GET_USER_INDEX` fails
// with it until the host has sent the pad an LED packet that assigns XInput slot 0..=3.
#[cfg(feature = "x360")]
pub(crate) const ERROR_INVALID_DEVICE_OBJECT_PARAMETER: u32 = 650;
// `STATUS_DEVICE_HARDWARE_ERROR`: the driver's 1 s `WAIT_DEVICE_READY` ran out.
const ERROR_DEVICE_HARDWARE_ERROR: u32 = 483;
const ERROR_OPERATION_ABORTED: u32 = 995;
const ERROR_DEVICE_NOT_CONNECTED: u32 = 1167;
// Never from the driver: `OverlappedCall`'s own bounded wait gave up.
const ERROR_TIMEOUT: u32 = 1460;

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

    /// Whether a `CHECK_VERSION` failure is the driver rejecting our version.
    ///
    /// The driver reports a mismatch only as `STATUS_NOT_SUPPORTED`
    /// (`ERROR_NOT_SUPPORTED`); any other failure (timeout, device gone, malformed
    /// request) is not a version problem.
    #[inline]
    #[must_use]
    pub(crate) const fn is_version_mismatch(self) -> bool {
        matches!(self.win32_code(), Some(ERROR_NOT_SUPPORTED))
    }

    /// `WAIT_DEVICE_READY` failed because the driver does not implement it
    /// (older drivers).
    #[inline]
    #[must_use]
    pub(crate) const fn is_wait_device_ready_unsupported(self) -> bool {
        matches!(
            self.win32_code(),
            Some(ERROR_INVALID_PARAMETER | ERROR_NOT_SUPPORTED | ERROR_INVALID_FUNCTION)
        )
    }

    /// ViGEmBus waits at most 1 s for the host to start a new pad, then completes
    /// `WAIT_DEVICE_READY` with `STATUS_DEVICE_HARDWARE_ERROR`
    /// (`ERROR_DEVICE_HARDWARE_ERROR`, 483). The pad still exists and may become
    /// usable once the host catches up (for example on a slow or busy system).
    #[inline]
    #[must_use]
    pub(crate) const fn is_wait_device_ready_driver_timeout(self) -> bool {
        matches!(self.win32_code(), Some(ERROR_DEVICE_HARDWARE_ERROR))
    }

    /// Our own bounded wait in `OverlappedCall` gave up (`ERROR_TIMEOUT`). The
    /// driver never reports this code, and the IRP may still have succeeded.
    #[inline]
    #[must_use]
    pub(crate) const fn is_local_ioctl_timeout(self) -> bool {
        matches!(self.win32_code(), Some(ERROR_TIMEOUT))
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

    /// The report was **not delivered**: the host had no read waiting for this pad.
    ///
    /// ViGEmBus does not queue input reports. It hands each one straight to a
    /// read the host's input stack has pending, and if none is pending at that
    /// moment it drops the report and returns this error. This is most likely
    /// right after plug, before the host starts reading (which
    /// [`wait_for_ready`](crate::TargetHandle::wait_for_ready) waits out), but it
    /// can happen at any time if reports arrive faster than the host asks for
    /// them. Nothing is retried for you.
    ///
    /// `Ok` from `update` means the opposite: the report reached the host. (On
    /// Xbox 360, a report identical to the previous one is accepted without
    /// being sent again, since the host already has that state.)
    ///
    /// What to do depends on the report:
    ///
    /// - **A frame loop that sends the full state every frame** can ignore a
    ///   miss. The next frame carries the same state.
    /// - **A report that must arrive**, such as a button release or the last
    ///   report before you stop sending, has to be sent again. Otherwise the
    ///   host keeps the previous state (a stuck button).
    ///
    /// ```
    /// use std::{thread, time::Duration};
    /// use vigem_rust::{BusError, ClientError};
    ///
    /// /// Resend until the driver accepts the report, for about 50 ms at most.
    /// fn send_until_delivered(
    ///     mut send: impl FnMut() -> Result<(), ClientError>,
    /// ) -> Result<(), ClientError> {
    ///     let mut tries = 0;
    ///     loop {
    ///         match send() {
    ///             Err(ClientError::Bus(BusError::TargetNotReady { .. })) if tries < 50 => {
    ///                 tries += 1;
    ///                 thread::sleep(Duration::from_millis(1));
    ///             }
    ///             other => return other,
    ///         }
    ///     }
    /// }
    ///
    /// // send_until_delivered(|| pad.update(&release))?;
    /// ```
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

    /// The host has given this Xbox 360 pad an XInput slot, but the driver did
    /// not return its number.
    ///
    // The Xbox 360 items linked below only exist with the `x360` feature, so
    // name them as plain code when it is off (the link would not resolve).
    #[cfg_attr(
        feature = "x360",
        doc = "Only from [`user_index`](crate::TargetHandle::user_index). Current"
    )]
    #[cfg_attr(not(feature = "x360"), doc = "Only from `user_index`. Current")]
    /// ViGEmBus drivers (measured on 1.21.442) answer the query without any
    /// data, so the number cannot be read this way. It is not a fault and
    /// retrying does not help: read
    #[cfg_attr(
        feature = "x360",
        doc = "[`X360Notification::led_number`](crate::X360Notification::led_number)"
    )]
    #[cfg_attr(not(feature = "x360"), doc = "`X360Notification::led_number`")]
    /// from [`register_notification`](crate::TargetHandle::register_notification)
    /// instead.
    #[error("target {serial_no} has an XInput user index, but the driver did not return it")]
    UserIndexUnavailable {
        /// Serial of the pad that was queried.
        serial_no: u32,
    },
}

impl BusError {
    /// `true` if a plug error means "that serial is taken" and the scan should advance.
    ///
    /// Scoped to [`Operation::PLUGIN_TARGET`]. Includes `ERROR_INVALID_PARAMETER`,
    /// which is what ViGEmBus returns for a duplicate serial (`Bus_PlugInDevice`
    /// remaps `STATUS_OBJECT_NAME_EXISTS`). On any other operation that code means a
    /// malformed request and is **not** retryable.
    #[inline]
    pub(crate) fn is_retryable_plug_error(&self) -> bool {
        match self {
            Self::Ioctl {
                operation, source, ..
            } if *operation == Operation::PLUGIN_TARGET => source.is_plug_serial_conflict(),
            _ => false,
        }
    }

    /// Fatal for a notification subscription: the bus device is gone (not a
    /// drop-driven `ERROR_OPERATION_ABORTED`).
    #[inline]
    pub(crate) fn is_fatal_notification_error(&self) -> bool {
        matches!(self, Self::DeviceGone { .. })
    }

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
            | Self::UserIndexUnavailable { .. }
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
            | Self::UserIndexUnavailable { .. }
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

/// Which errors mean "the pad is still coming up" to `wait_for_ready`.
///
/// Both waits check this client's own liveness flag every round, so a real
/// unplug still ends them, and if the driver still says `DeviceGone` when the
/// deadline passes the wait returns that error instead of `ReadyTimeout`.
/// Every other failure (access denied, unclassified IOCTL errors, ...) is a
/// hard error for `wait_for_ready`.
impl ClientError {
    /// The DS4 submit probe failed only because the host has no read waiting
    /// for the pad yet.
    #[cfg_attr(not(feature = "ds4"), allow(dead_code))]
    #[inline]
    pub(crate) fn is_ds4_submit_pending(&self) -> bool {
        matches!(self, Self::Bus(BusError::TargetNotReady { .. }))
    }

    /// The Xbox 360 user-index query failed only because the pad is still coming up:
    ///
    /// - `ERROR_INVALID_DEVICE_OBJECT_PARAMETER`: the host has not assigned a slot yet.
    /// - `DeviceGone`: for about the first millisecond after `PLUGIN_TARGET` the
    ///   driver does not know the serial yet. Without `WAIT_DEVICE_READY`, `plug`
    ///   returns inside that window.
    #[cfg(feature = "x360")]
    #[inline]
    pub(crate) fn is_user_index_pending(&self) -> bool {
        match self {
            Self::Bus(BusError::DeviceGone { operation, .. }) => {
                *operation == Operation::XUSB_GET_USER_INDEX
            }
            Self::Bus(BusError::Ioctl {
                operation, source, ..
            }) => {
                *operation == Operation::XUSB_GET_USER_INDEX
                    && source.win32_code() == Some(ERROR_INVALID_DEVICE_OBJECT_PARAMETER)
            }
            _ => false,
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
