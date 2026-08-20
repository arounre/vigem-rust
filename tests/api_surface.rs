//! Public surface: crate-root nameability, re-exports, and Send/Sync checks.

#![cfg(windows)]

use vigem_rust::BusError as CrateRootBusError;
use vigem_rust::ClientError;
use vigem_rust::error::BusError as ErrorModuleBusError;

const fn assert_send<T: Send>() {}
const fn assert_send_sync<T: Send + Sync>() {}

/// Proves `BusError` is nameable and the module/crate-root paths are the same type.
fn takes(e: ErrorModuleBusError) -> CrateRootBusError {
    e
}

/// Proves `ClientError` is nameable and can wrap a `BusError`.
fn wraps_bus(e: BusError) -> ClientError {
    ClientError::Bus(e)
}

// Alias so the helper above can use the short name without import ambiguity.
use vigem_rust::BusError;

/// Types in public signatures are importable from the crate root.
///
/// Also covers constructibility of classified `BusError` / `ClientError` variants
/// and `Win32Error` as an `Error` type - one compile-time surface test is enough.
#[test]
fn public_types_are_nameable_at_crate_root() {
    use vigem_rust::{
        BusError, Client, ClientBuilder, ClientError, DEFAULT_MAX_TARGETS, Device,
        MAX_TARGETS_LIMIT, NOTIFICATION_CHANNEL_CAPACITY, NotificationBuffer, NotificationReceiver,
        Operation, Pending, Ready, RecvError, RecvTimeoutError, SERIAL_SCAN_LIMIT, SerialDiscovery,
        TargetBuilder, TargetHandle, TryRecvError, Win32Error,
    };

    // `std::error::Error` for the public error types.
    fn needs_error<T: std::error::Error>() {}
    needs_error::<Win32Error>();
    needs_error::<BusError>();
    needs_error::<ClientError>();
    needs_error::<RecvError>();
    needs_error::<TryRecvError>();
    needs_error::<RecvTimeoutError>();

    // Crate-root vs error-module path identity, and ClientError::Bus wrapping.
    let e = BusError::BusNotFound { source: None };
    let e = takes(e);
    let msg = e.to_string();
    assert!(
        msg.contains("vigem.org"),
        "BusNotFound display should mention the install URL, got: {msg}"
    );
    let _ = wraps_bus(e);

    // Every classified BusError variant must be constructible by name.
    let sample = Win32Error::from_win32(5); // ERROR_ACCESS_DENIED
    let _ = BusError::DeviceGone {
        operation: Operation::from_static("XUSB_SUBMIT_REPORT"),
        serial_no: Some(1),
    };
    let _ = BusError::AccessDenied {
        operation: Operation::from_static("VIGEM_PLUGIN_TARGET"),
        serial_no: None,
    };
    let _ = BusError::TargetNotReady { serial_no: 2 };
    let _ = BusError::Ioctl {
        operation: Operation::from_static("CreateEventW"),
        serial_no: Some(1),
        source: sample,
    };
    let _ = BusError::Enumeration { source: sample };
    let _ = BusError::VersionMismatch { requested: 0x0001 };
    let _ = BusError::BusNotFound {
        source: Some(sample),
    };
    assert_eq!(sample.win32_code(), Some(5));
    assert_eq!(sample.hresult() as u32, 0x8007_0005);
    assert_eq!(
        BusError::DeviceGone {
            operation: Operation::from_static("XUSB_SUBMIT_REPORT"),
            serial_no: Some(1),
        }
        .operation()
        .map(|o| o.as_str()),
        Some("XUSB_SUBMIT_REPORT")
    );

    // Every ClientError variant must be constructible by name.
    let _ = ClientError::NoFreeSlot { max_targets: 16 };
    let _ = ClientError::BusSerialsExhausted {
        scanned: 64,
        source: BusError::Ioctl {
            operation: Operation::from_static("VIGEM_PLUGIN_TARGET"),
            serial_no: Some(1),
            source: Win32Error::from_win32(87),
        },
    };
    let _ = ClientError::InvalidMaxTargets {
        requested: 0,
        max_allowed: MAX_TARGETS_LIMIT,
    };
    let _ = ClientError::TargetDoesNotExist { serial_no: 1 };
    let _ = ClientError::ClientNoLongerExists;
    let _ = ClientError::ReadyTimeout {
        timeout: std::time::Duration::from_secs(3),
    };
    let _ = ClientError::Poisoned;
    let _ = ClientError::Bus(BusError::VersionMismatch { requested: 0x0001 });
    let _ = ClientError::Bus(BusError::TargetNotReady { serial_no: 1 });
    let _ = ClientError::Bus(BusError::DeviceGone {
        operation: Operation::from_static("DS4_SUBMIT_REPORT"),
        serial_no: Some(1),
    });
    let _ = ClientError::Bus(BusError::AccessDenied {
        operation: Operation::from_static("VIGEM_PLUGIN_TARGET"),
        serial_no: Some(1),
    });

    let _ = NOTIFICATION_CHANNEL_CAPACITY;
    let _ = DEFAULT_MAX_TARGETS;
    let _ = MAX_TARGETS_LIMIT;
    let _ = SERIAL_SCAN_LIMIT;
    const {
        assert!(MAX_TARGETS_LIMIT <= SERIAL_SCAN_LIMIT);
    }
    let _: NotificationBuffer = NotificationBuffer::default();
    // Nameability only - behavioural default is tested in client unit tests.
    let _ = SerialDiscovery::Auto;
    let _ = SerialDiscovery::Eager;
    let _ = SerialDiscovery::Off;

    let _ = NotificationBuffer::Bounded {
        capacity: NOTIFICATION_CHANNEL_CAPACITY,
    };
    let _ = NotificationBuffer::Unbounded;
    let _ = std::any::type_name::<Client>();
    let _ = std::any::type_name::<ClientBuilder>();
    let _ = std::any::type_name::<ClientError>();
    let _ = std::any::type_name::<BusError>();
    let _ = std::any::type_name::<Operation>();
    let _ = std::any::type_name::<Win32Error>();
    let _ = std::any::type_name::<SerialDiscovery>();
    let _ = Operation::from_static("CreateEventW");
    let _ = std::any::type_name::<Pending>();
    let _ = std::any::type_name::<Ready>();
    let _ = std::any::type_name::<NotificationReceiver<()>>();
    let _ = RecvError::Disconnected;
    let _ = TryRecvError::Empty;
    let _ = RecvTimeoutError::Timeout;
    fn _device_bound<T: Device>(_h: &TargetHandle<T>, _b: TargetBuilder<'_, T>) {}

    fn _occupied(c: &Client) -> Result<std::collections::BTreeSet<u32>, ClientError> {
        c.occupied_serials()
    }
    fn _live(c: &Client) -> std::collections::BTreeSet<u32> {
        c.live_serials()
    }
    fn _mode(c: &Client) -> SerialDiscovery {
        c.serial_discovery()
    }
    let _ = (
        _occupied as fn(&Client) -> Result<std::collections::BTreeSet<u32>, ClientError>,
        _live as fn(&Client) -> std::collections::BTreeSet<u32>,
        _mode as fn(&Client) -> SerialDiscovery,
    );

    #[cfg(feature = "x360")]
    {
        use vigem_rust::{X360Button, X360Notification, X360Report, Xbox360};
        let _ = std::any::type_name::<Xbox360>();
        let _ = std::any::type_name::<X360Button>();
        let _ = std::any::type_name::<X360Report>();
        let _ = std::any::type_name::<X360Notification>();
        let _ = std::any::type_name::<TargetHandle<Xbox360, Ready>>();
        let _ = std::any::type_name::<TargetHandle<Xbox360, Pending>>();
        let _ = std::any::type_name::<TargetBuilder<'_, Xbox360>>();
        let _ = std::any::type_name::<NotificationReceiver<X360Notification>>();
    }

    #[cfg(feature = "ds4")]
    {
        use vigem_rust::{
            Ds4ButtonFlags, Ds4Buttons, Ds4Dpad, Ds4LightbarColor, Ds4Notification,
            Ds4OutputBuffer, Ds4Report, Ds4ReportEx, Ds4ReportExData, Ds4SpecialButton, Ds4Touch,
            DualShock4, TouchPoint, TouchState, TouchpadTracker,
        };
        let _ = std::any::type_name::<DualShock4>();
        let _ = std::any::type_name::<Ds4Report>();
        let _ = std::any::type_name::<Ds4ReportEx>();
        let _ = std::any::type_name::<Ds4ReportExData>();
        let _ = std::any::type_name::<Ds4SpecialButton>();
        let _ = std::any::type_name::<Ds4OutputBuffer>();
        let _ = std::any::type_name::<Ds4ButtonFlags>();
        let _ = std::any::type_name::<Ds4Buttons>();
        let _ = std::any::type_name::<Ds4Dpad>();
        let _ = std::any::type_name::<Ds4LightbarColor>();
        let _ = std::any::type_name::<Ds4Notification>();
        let _ = std::any::type_name::<Ds4Touch>();
        let _ = std::any::type_name::<TouchPoint>();
        let _ = std::any::type_name::<TouchState>();
        let _ = std::any::type_name::<TouchpadTracker>();
        let _ = std::any::type_name::<TargetHandle<DualShock4, Ready>>();
        let _ = std::any::type_name::<TargetHandle<DualShock4, Pending>>();
        let _ = std::any::type_name::<TargetBuilder<'_, DualShock4>>();
        let _ = std::any::type_name::<NotificationReceiver<Ds4Notification>>();
        let _ = std::any::type_name::<NotificationReceiver<Ds4OutputBuffer>>();
    }
}

/// Send/Sync (and Send-only for NotificationReceiver) of the public surface.
#[test]
fn public_types_are_send_sync() {
    use vigem_rust::{
        BusError, Client, ClientError, NotificationBuffer, NotificationReceiver, Operation,
        RecvError, RecvTimeoutError, TryRecvError, Win32Error,
    };

    assert_send_sync::<Client>();
    assert_send_sync::<ClientError>();
    assert_send_sync::<BusError>();
    assert_send_sync::<Operation>();
    assert_send_sync::<Win32Error>();
    assert_send_sync::<NotificationBuffer>();
    assert_send_sync::<RecvError>();
    assert_send_sync::<TryRecvError>();
    assert_send_sync::<RecvTimeoutError>();

    #[cfg(feature = "x360")]
    {
        use vigem_rust::{Pending, Ready, TargetHandle, X360Notification, Xbox360};
        assert_send_sync::<TargetHandle<Xbox360, Ready>>();
        assert_send_sync::<TargetHandle<Xbox360, Pending>>();
        assert_send::<NotificationReceiver<X360Notification>>();
    }

    #[cfg(feature = "ds4")]
    {
        use vigem_rust::{
            Ds4Notification, Ds4OutputBuffer, DualShock4, Pending, Ready, TargetHandle,
        };
        assert_send_sync::<TargetHandle<DualShock4, Ready>>();
        assert_send_sync::<TargetHandle<DualShock4, Pending>>();
        assert_send::<NotificationReceiver<Ds4Notification>>();
        assert_send::<NotificationReceiver<Ds4OutputBuffer>>();
    }
}
