//! ViGEmBus device handle and IOCTL helpers.
//!
//! `METHOD_BUFFERED`: input copied at submit; output at IRP completion.
//! Output and OVERLAPPED must outlive the IRP. Input and output must not alias.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;

use crate::internal::notification_hub::Fanout;

use windows::Win32::Devices::DeviceAndDriverInstallation::{
    DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, HDEVINFO, SP_DEVICE_INTERFACE_DATA,
    SP_DEVICE_INTERFACE_DETAIL_DATA_W, SP_DEVINFO_DATA, SetupDiDestroyDeviceInfoList,
    SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW,
};
#[cfg(feature = "x360")]
use windows::Win32::Foundation::ERROR_TIMEOUT;
use windows::Win32::Foundation::{
    ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER, ERROR_NO_MORE_ITEMS, ERROR_NOT_SUPPORTED,
    GENERIC_READ, GENERIC_WRITE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_NO_BUFFERING, FILE_FLAG_OVERLAPPED,
    FILE_FLAG_WRITE_THROUGH, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::{CancelIoEx, OVERLAPPED};
use windows::core::{GUID, PCWSTR};

#[cfg(feature = "ds4")]
use crate::controller::ds4::{
    Ds4Notification, Ds4OutputBuffer, Ds4Report, Ds4ReportEx, Ds4SubmitReport, Ds4SubmitReportEx,
};
#[cfg(feature = "x360")]
use crate::controller::x360::{X360Notification, X360Report, XusbSubmitReport};
use crate::error::{BusError, Operation, Win32Error};
use crate::internal::ioctl::*;
use crate::internal::notification_workers::*;
use crate::internal::overlapped::{
    BusInner, IOCTL_TEARDOWN_TIMEOUT_MS, IoctlBuffers, OverlappedCall, WAIT_POLL_MS,
    ensure_full_transfer,
};
use crate::target::Target;

const VIGEM_GUID: GUID = GUID::from_values(
    0x96E42B22,
    0xF5E9,
    0x42F8,
    [0xB0, 0x43, 0xED, 0x0F, 0x93, 0x2F, 0x01, 0x4F],
);

/// Mirrors `VIGEM_COMMON_VERSION` in ViGEmBusShared.h.
const VIGEM_COMMON_VERSION: u32 = 0x0001;

// SetupAPI detail struct needs align >= 4; `Vec<u32>` provides that.
const _: () = assert!(
    core::mem::align_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() <= core::mem::align_of::<u32>()
);

/// SetupAPI `cbSize` (not always Rust `size_of` on 32-bit).
///
/// C packing: 8 on x64, 6 on x86; the `windows` crate type is 8 on both.
/// Passing 8 on i686 fails with `ERROR_INVALID_USER_BUFFER`.
#[cfg(target_pointer_width = "64")]
const DETAIL_DATA_CB_SIZE: u32 = wire_size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();

/// See the 64-bit definition above for the SetupAPI packing rationale.
#[cfg(target_pointer_width = "32")]
const DETAIL_DATA_CB_SIZE: u32 = {
    // DevicePath is a single WCHAR at offset 4; no padding under pshpack1.
    // Values are tiny (4 + 2); the u32 cast is lossless on every pointer width.
    #[allow(clippy::cast_possible_truncation)]
    {
        (core::mem::offset_of!(SP_DEVICE_INTERFACE_DETAIL_DATA_W, DevicePath) as u32)
            + (core::mem::size_of::<u16>() as u32)
    }
};

#[cfg(not(any(target_pointer_width = "32", target_pointer_width = "64")))]
compile_error!(
    "DETAIL_DATA_CB_SIZE is only defined for 32-bit and 64-bit targets; \
     SetupAPI packing is unknown on this pointer width"
);

#[cfg(target_pointer_width = "64")]
const _: () = assert!(DETAIL_DATA_CB_SIZE == 8);
#[cfg(target_pointer_width = "32")]
const _: () = assert!(DETAIL_DATA_CB_SIZE == 6);

/// Access denied wins; else `VersionMismatch` if `CHECK_VERSION` failed for a
/// non-`INVALID_PARAMETER` reason; else `BusNotFound` (with last error if an
/// interface was seen).
fn classify_connect_failure(
    saw_interface: bool,
    version_mismatch: bool,
    last_error: Option<Win32Error>,
) -> BusError {
    const ERROR_ACCESS_DENIED: u32 = 5;
    if let Some(e) = last_error
        && e.win32_code() == Some(ERROR_ACCESS_DENIED)
    {
        return BusError::AccessDenied {
            operation: Operation::OPEN_BUS,
            serial_no: None,
        };
    }
    if version_mismatch {
        BusError::VersionMismatch {
            requested: VIGEM_COMMON_VERSION,
        }
    } else if saw_interface {
        BusError::BusNotFound { source: last_error }
    } else {
        BusError::BusNotFound { source: None }
    }
}

#[derive(Clone)]
pub(crate) struct Bus {
    pub(crate) inner: Arc<BusInner>,
}

impl Bus {
    /// For serial discovery so the PnP walk can run without holding the client mutex.
    #[inline]
    pub(crate) fn devnode_id_cloned(&self) -> Option<Vec<u16>> {
        self.inner.devnode_id.clone()
    }
}

/// `*const OVERLAPPED` wrapper so the worker slot is `Send`.
///
/// Used only as `CancelIoEx`'s `lpOverlapped` identity - never dereferenced.
#[derive(Clone, Copy)]
struct SendOverlappedPtr(*const OVERLAPPED);

// SAFETY: published only while the worker's boxed `OverlappedCall` is alive with an
// outstanding IRP; cleared under the same mutex before that call drops.
unsafe impl Send for SendOverlappedPtr {}

/// Worker shutdown flag and in-flight OVERLAPPED under one lock.
///
/// Lock spans check -> submit -> publish so Drop always sees a cancel target.
struct WorkerSlot {
    shutdown: bool,
    overlapped: Option<SendOverlappedPtr>,
}

/// Shared by the notification worker and its handle (`Arc<BusInner>` for cancel).
struct WorkerState {
    slot: Mutex<WorkerSlot>,
    bus: Arc<BusInner>,
}

/// Slot mutex; recover from poison so Drop can always join.
fn lock_slot(state: &WorkerState) -> std::sync::MutexGuard<'_, WorkerSlot> {
    state.slot.lock().unwrap_or_else(|e| e.into_inner())
}

/// Notification worker. Drop: shutdown, cancel, join.
pub(crate) struct NotificationWorkerHandle {
    state: Arc<WorkerState>,
    join: Option<JoinHandle<()>>,
}

impl Drop for NotificationWorkerHandle {
    fn drop(&mut self) {
        {
            let mut slot = lock_slot(&self.state);
            slot.shutdown = true;

            if let Some(SendOverlappedPtr(ptr)) = slot.overlapped
                && !ptr.is_null()
            {
                // SAFETY: `state.bus` keeps the device handle open (only
                // BusInner::drop closes it). `ptr` is the worker's live OVERLAPPED;
                // the worker cannot free it while we hold the slot lock.
                unsafe {
                    let _ = CancelIoEx(self.state.bus.handle, Some(ptr));
                }
            }
        } // release slot lock before join

        if let Some(handle) = self.join.take() {
            let _ = handle.join();
        }
    }
}

/// Older drivers may not implement `IOCTL_VIGEM_WAIT_DEVICE_READY`.
fn is_optional_wait_device_ready_error(err: &windows::core::Error) -> bool {
    let code = err.code();
    code == ERROR_INVALID_PARAMETER.to_hresult()
        || code == ERROR_NOT_SUPPORTED.to_hresult()
        || code == ERROR_INVALID_FUNCTION.to_hresult()
}

#[inline]
fn map_ioctl(
    operation: Operation,
    serial_no: Option<u32>,
) -> impl FnOnce(windows::core::Error) -> BusError {
    move |source| BusError::from_ioctl(operation, serial_no, source)
}

/// Consecutive transient failures before ending a notification subscription.
const NOTIFICATION_MAX_CONSECUTIVE_FAILURES: u32 = 8;

/// Fatal notification error: bus device gone (not drop-driven ABORTED).
#[inline]
fn is_fatal_notification_error(err: &BusError) -> bool {
    matches!(err, BusError::DeviceGone { .. })
}

impl Bus {
    pub(crate) fn connect() -> Result<Self, BusError> {
        // SAFETY: static ViGEm GUID; present device interfaces only.
        let devices = unsafe {
            SetupDiGetClassDevsW(
                Some(&VIGEM_GUID as *const _),
                None,
                None,
                DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
            )
        }
        .map_err(BusError::enumeration)?;

        struct DevInfoGuard(HDEVINFO);
        impl Drop for DevInfoGuard {
            fn drop(&mut self) {
                // SAFETY: sole owner from SetupDiGetClassDevsW.
                unsafe {
                    let _ = SetupDiDestroyDeviceInfoList(self.0);
                }
            }
        }
        let _guard = DevInfoGuard(devices);

        let mut saw_interface = false;
        let mut version_mismatch = false;
        let mut last_error: Option<Win32Error> = None;

        for iface_result in DeviceInterfaceIterator::new(devices) {
            let iface = match iface_result {
                Ok(iface) => iface,
                Err(e) => {
                    // Systemic enum failure; keep for BusNotFound.
                    last_error = Some(Win32Error::from(e));
                    break;
                }
            };
            saw_interface = true;

            let mut needed: u32 = 0;
            // SAFETY: size probe only; no detail buffer. Failure often means
            // ERROR_INSUFFICIENT_BUFFER with `needed` filled - that is success for us.
            let _ = unsafe {
                SetupDiGetDeviceInterfaceDetailW(
                    devices,
                    &iface as *const _,
                    None,
                    0,
                    Some(&mut needed as *mut _),
                    None,
                )
            };

            if needed == 0 {
                continue;
            }

            let words = (needed as usize).div_ceil(core::mem::size_of::<u32>());
            let mut buf: Vec<u32> = vec![0u32; words];
            let detail_ptr = buf.as_mut_ptr().cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();

            // Capture this interface's device node (multiple ViGEmBus forks can coexist).
            // `cbSize` is Rust `size_of` for SP_DEVINFO_DATA, not DETAIL_DATA_CB_SIZE.
            let mut devinfo = SP_DEVINFO_DATA {
                cbSize: u32::try_from(core::mem::size_of::<SP_DEVINFO_DATA>())
                    .expect("SP_DEVINFO_DATA size fits u32"),
                ..Default::default()
            };

            // SAFETY: `buf` is >= `needed` and 4-byte aligned (`Vec<u32>`); set `cbSize`
            // first to the C packing size, not Rust `size_of`.
            let fill_result = unsafe {
                let cb_size_ptr = &raw mut (*detail_ptr).cbSize;
                *cb_size_ptr = DETAIL_DATA_CB_SIZE;
                SetupDiGetDeviceInterfaceDetailW(
                    devices,
                    &iface as *const _,
                    Some(detail_ptr),
                    needed,
                    Some(&mut needed as *mut _),
                    Some(&raw mut devinfo),
                )
            };
            if let Err(e) = fill_result {
                last_error = Some(Win32Error::from(e));
                continue;
            }

            // Path pointer from the Vec allocation (not DevicePath's [u16; 1] field,
            // which only carries 2-byte provenance).
            // SAFETY: SetupAPI wrote a NUL-terminated path; `buf` outlives CreateFileW.
            let handle = match unsafe {
                let path = buf
                    .as_ptr()
                    .cast::<u8>()
                    .add(core::mem::offset_of!(
                        SP_DEVICE_INTERFACE_DETAIL_DATA_W,
                        DevicePath
                    ))
                    .cast::<u16>();
                CreateFileW(
                    PCWSTR::from_raw(path),
                    (GENERIC_READ | GENERIC_WRITE).0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    None,
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL
                        | FILE_FLAG_NO_BUFFERING
                        | FILE_FLAG_WRITE_THROUGH
                        | FILE_FLAG_OVERLAPPED,
                    None,
                )
            } {
                Ok(h) => h,
                Err(e) => {
                    last_error = Some(Win32Error::from(e));
                    continue;
                }
            };

            // Wrap immediately so the version-check IOCTL can hold Arc like every other call.
            let mut candidate = Arc::new(BusInner {
                handle,
                devnode_id: None,
            });

            let version = CheckVersion {
                size: wire_size_of::<CheckVersion>(),
                version: VIGEM_COMMON_VERSION,
            };
            // SAFETY: `version` lives for the call; no output buffer.
            let version_result = unsafe {
                OverlappedCall::ioctl_sync(
                    &candidate,
                    IOCTL_VIGEM_CHECK_VERSION,
                    core::ptr::from_ref(&version).cast::<c_void>(),
                    version.size,
                    core::ptr::null_mut(),
                    0,
                )
            };

            match version_result {
                Ok(_) => {
                    // Best-effort: failure degrades serial discovery to Off.
                    // Sole owner of the Arc - get_mut succeeds.
                    if let Some(inner) = Arc::get_mut(&mut candidate) {
                        inner.devnode_id =
                            crate::internal::devnode::read_device_instance_id(devinfo.DevInst).ok();
                    }
                    return Ok(Bus { inner: candidate });
                }
                // ERROR_INVALID_PARAMETER here means a malformed / wrong-size
                // CheckVersion, not a real version mismatch: leave
                // `version_mismatch` false so this surfaces as BusNotFound.
                Err(e) if e.code() == ERROR_INVALID_PARAMETER.to_hresult() => {
                    last_error = Some(Win32Error::from(e));
                }
                Err(e) => {
                    version_mismatch = true;
                    last_error = Some(Win32Error::from(e));
                }
            }
        }

        Err(classify_connect_failure(
            saw_interface,
            version_mismatch,
            last_error,
        ))
    }

    /// Plugin + best-effort wait-ready. Missing wait-ready on older drivers is non-fatal.
    /// Callers still use `wait_for_ready` (submit probe); this does not re-issue wait.
    pub(crate) fn plug(&self, target: &Target, serial_no: u32) -> Result<(), BusError> {
        let plugin = PluginTarget {
            size: wire_size_of::<PluginTarget>(),
            serial_no,
            target_type: target.kind,
            vendor_id: target.vendor_id,
            product_id: target.product_id,
        };

        // SAFETY: stack-local input; no output.
        unsafe {
            OverlappedCall::ioctl_sync(
                &self.inner,
                IOCTL_VIGEM_PLUGIN_TARGET,
                core::ptr::from_ref(&plugin).cast::<c_void>(),
                plugin.size,
                core::ptr::null_mut(),
                0,
            )
        }
        .map_err(map_ioctl(Operation::PLUGIN_TARGET, Some(serial_no)))?;

        let wait_ready = WaitDeviceReady {
            size: wire_size_of::<WaitDeviceReady>(),
            serial_no,
        };

        // SAFETY: stack-local input; no output.
        let wait_result = unsafe {
            OverlappedCall::ioctl_sync(
                &self.inner,
                IOCTL_VIGEM_WAIT_DEVICE_READY,
                core::ptr::from_ref(&wait_ready).cast::<c_void>(),
                wait_ready.size,
                core::ptr::null_mut(),
                0,
            )
        };

        match wait_result {
            Ok(_) => Ok(()),
            Err(e) if is_optional_wait_device_ready_error(&e) => Ok(()),
            Err(e) => {
                let _ = self.unplug(serial_no);
                Err(BusError::from_ioctl(
                    Operation::WAIT_DEVICE_READY,
                    Some(serial_no),
                    e,
                ))
            }
        }
    }

    pub(crate) fn unplug(&self, serial_no: u32) -> Result<(), BusError> {
        let unplug = UnPlugTarget {
            size: wire_size_of::<UnPlugTarget>(),
            serial_no,
        };

        // SAFETY: stack-local input; no output. Teardown-bounded so Drop cannot hang.
        unsafe {
            OverlappedCall::ioctl_sync_timeout(
                &self.inner,
                IOCTL_VIGEM_UNPLUG_TARGET,
                core::ptr::from_ref(&unplug).cast::<c_void>(),
                unplug.size,
                core::ptr::null_mut(),
                0,
                IOCTL_TEARDOWN_TIMEOUT_MS,
            )
        }
        .map_err(map_ioctl(Operation::UNPLUG_TARGET, Some(serial_no)))?;

        Ok(())
    }

    #[cfg(feature = "x360")]
    pub(crate) fn update_x360_on(
        &self,
        call: &mut OverlappedCall,
        serial_no: u32,
        report: &X360Report,
    ) -> Result<(), BusError> {
        let submit_report = XusbSubmitReport {
            size: wire_size_of::<XusbSubmitReport>(),
            serial_no,
            report: *report,
        };

        // SAFETY: stack-local input; no output; `call` address-stable for the call.
        unsafe {
            call.ioctl(
                IOCTL_XUSB_SUBMIT_REPORT,
                core::ptr::from_ref(&submit_report).cast::<c_void>(),
                submit_report.size,
                core::ptr::null_mut(),
                0,
            )
        }
        .map_err(map_ioctl(Operation::XUSB_SUBMIT_REPORT, Some(serial_no)))?;

        Ok(())
    }

    /// One-shot submit (allocates event) for tests without a handle.
    #[cfg(all(test, feature = "x360"))]
    pub(crate) fn update_x360(&self, serial_no: u32, report: &X360Report) -> Result<(), BusError> {
        let mut call = OverlappedCall::new(Arc::clone(&self.inner))
            .map_err(map_ioctl(Operation::CREATE_EVENT_W, Some(serial_no)))?;
        self.update_x360_on(&mut call, serial_no, report)
    }

    #[cfg(feature = "ds4")]
    pub(crate) fn update_ds4_on(
        &self,
        call: &mut OverlappedCall,
        serial_no: u32,
        report: &Ds4Report,
    ) -> Result<(), BusError> {
        let submit_report = Ds4SubmitReport::new(serial_no, report);

        // SAFETY: stack-local input; no output; `call` address-stable for the call.
        unsafe {
            call.ioctl(
                IOCTL_DS4_SUBMIT_REPORT,
                core::ptr::from_ref(&submit_report).cast::<c_void>(),
                submit_report.size,
                core::ptr::null_mut(),
                0,
            )
        }
        .map_err(map_ioctl(Operation::DS4_SUBMIT_REPORT, Some(serial_no)))?;

        Ok(())
    }

    #[cfg(feature = "ds4")]
    pub(crate) fn update_ds4_ex_on(
        &self,
        call: &mut OverlappedCall,
        serial_no: u32,
        report: &Ds4ReportEx,
    ) -> Result<(), BusError> {
        let submit_report = Ds4SubmitReportEx {
            size: wire_size_of::<Ds4SubmitReportEx>(),
            serial_no,
            report: *report,
        };

        // Same IOCTL as basic DS4; driver picks format by Size.
        // SAFETY: stack-local input; no output; `call` address-stable for the call.
        unsafe {
            call.ioctl(
                IOCTL_DS4_SUBMIT_REPORT,
                core::ptr::from_ref(&submit_report).cast::<c_void>(),
                submit_report.size,
                core::ptr::null_mut(),
                0,
            )
        }
        .map_err(map_ioctl(Operation::DS4_SUBMIT_REPORT_EX, Some(serial_no)))?;

        Ok(())
    }

    /// Spawn a notification worker. Pair with a [`Fanout`].
    ///
    /// `alive` is the target's shared liveness flag. Cleared under the client
    /// mutex on unplug; the worker treats that like shutdown so it never
    /// re-arms an IOCTL against a recycled serial.
    pub(crate) fn spawn_notification_thread<W: NotificationWorker>(
        &self,
        serial_no: u32,
        alive: Arc<AtomicBool>,
        fanout: Arc<Fanout<W::Notification>>,
    ) -> Result<NotificationWorkerHandle, BusError> {
        let bus = Arc::clone(&self.inner);
        let state = Arc::new(WorkerState {
            slot: Mutex::new(WorkerSlot {
                shutdown: false,
                overlapped: None,
            }),
            bus: Arc::clone(&self.inner),
        });
        let state_thread = Arc::clone(&state);

        let (sync_tx, sync_rx) = mpsc::channel::<Result<(), BusError>>();

        let join = std::thread::spawn(move || {
            let serial = Some(serial_no);
            let op = W::OPERATION;

            // Fail fast before acknowledging startup so the spawner sees CreateEventW errors.
            let mut io = match IoctlBuffers::new(
                Arc::clone(&bus),
                Box::new(W::create_request(serial_no)),
                Box::default(),
            ) {
                Ok(io) => io,
                Err(e) => {
                    let _ = sync_tx.send(Err(BusError::from_ioctl(op, serial, e)));
                    return;
                }
            };
            let req_size = wire_size_of::<W::Request>();

            if sync_tx.send(Ok(())).is_err() {
                let _ = io.finish();
                return;
            }

            let mut consecutive_failures: u32 = 0;

            let should_stop =
                |state: &WorkerState| lock_slot(state).shutdown || !alive.load(Ordering::Acquire);

            loop {
                if should_stop(&state_thread) {
                    break;
                }

                // Drain any leftover IRP before touching buffers it may still own.
                // `wait_cancellable` can leave `pending` set on abort or when
                // GetOverlappedResult fails while Internal is still STATUS_PENDING;
                // zeroing the shared `response` box in that state is a write-race.
                if let Err(e) = io.call().reset() {
                    let _ = fanout.deliver(Err(BusError::from_ioctl(op, serial, e)));
                    break;
                }

                // Clear stale bytes so a prior short transfer cannot surface later.
                *io.response_mut() = W::Request::default();

                let submit = {
                    let mut slot = lock_slot(&state_thread);
                    if slot.shutdown || !alive.load(Ordering::Acquire) {
                        break;
                    }

                    let in_ptr = io.request_as_void();
                    let out_ptr = io.response_as_void_mut();
                    // SAFETY: distinct heap in/out owned by `io`; both outlive the IRP.
                    let r = unsafe {
                        io.call()
                            .submit(W::IOCTL_CODE, in_ptr, req_size, out_ptr, req_size)
                    };

                    if r.is_ok() {
                        // Publish for CancelIoEx even on sync completion.
                        slot.overlapped = Some(SendOverlappedPtr(io.call().overlapped_ptr()));
                    }
                    r
                };

                // Poll so shutdown and target death are seen even if CancelIoEx never completes.
                let outcome = match submit {
                    Ok(()) => {
                        match io
                            .call()
                            .wait_cancellable(WAIT_POLL_MS, || should_stop(&state_thread))
                        {
                            Ok(transferred) => ensure_full_transfer(transferred, W::MIN_TRANSFER)
                                .map(|()| transferred),
                            Err(e) => Err(e),
                        }
                    }
                    Err(e) => Err(e),
                };

                let stop = {
                    let mut slot = lock_slot(&state_thread);
                    slot.overlapped = None;
                    slot.shutdown || !alive.load(Ordering::Acquire)
                };

                if stop {
                    break;
                }

                match outcome {
                    Ok(_) => {
                        consecutive_failures = 0;
                        let notification = W::process_response(io.response());
                        if !fanout.deliver(Ok(notification)) {
                            break;
                        }
                    }
                    Err(e) => {
                        let bus_err = BusError::from_ioctl(op, serial, e);
                        // Drop cancel / target death: exit without sending ABORTED.
                        if bus_err
                            .win32_source()
                            .is_some_and(Win32Error::is_operation_aborted)
                        {
                            break;
                        }

                        if is_fatal_notification_error(&bus_err) {
                            let _ = fanout.deliver(Err(bus_err));
                            break;
                        }

                        consecutive_failures = consecutive_failures.saturating_add(1);
                        if consecutive_failures >= NOTIFICATION_MAX_CONSECUTIVE_FAILURES {
                            let _ = fanout.deliver(Err(bus_err));
                            break;
                        }
                    }
                }
            }

            lock_slot(&state_thread).overlapped = None;

            let _ = io.finish();
            // Every receiver sees `Disconnected` once the worker exits.
            fanout.close();
        });

        match sync_rx.recv() {
            Ok(Ok(())) => Ok(NotificationWorkerHandle {
                state,
                join: Some(join),
            }),
            Ok(Err(e)) => {
                let _ = join.join();
                Err(e)
            }
            Err(_) => {
                let _ = join.join();
                // Synthetic status: thread panicked before acknowledging startup.
                // Keep as unclassified Ioctl (not a driver HRESULT).
                Err(BusError::Ioctl {
                    operation: W::OPERATION,
                    serial_no: Some(serial_no),
                    source: Win32Error::from_hresult(windows::Win32::Foundation::E_FAIL.0),
                })
            }
        }
    }

    #[cfg(feature = "x360")]
    pub(crate) fn start_x360_notification_thread(
        &self,
        serial_no: u32,
        alive: Arc<AtomicBool>,
        fanout: Arc<Fanout<X360Notification>>,
    ) -> Result<NotificationWorkerHandle, BusError> {
        self.spawn_notification_thread::<X360NotificationWorker>(serial_no, alive, fanout)
    }

    #[cfg(feature = "ds4")]
    pub(crate) fn start_ds4_notification_thread(
        &self,
        serial_no: u32,
        alive: Arc<AtomicBool>,
        fanout: Arc<Fanout<Ds4Notification>>,
    ) -> Result<NotificationWorkerHandle, BusError> {
        self.spawn_notification_thread::<Ds4NotificationWorker>(serial_no, alive, fanout)
    }

    #[cfg(feature = "ds4")]
    pub(crate) fn start_ds4_output_thread(
        &self,
        serial_no: u32,
        alive: Arc<AtomicBool>,
        fanout: Arc<Fanout<Ds4OutputBuffer>>,
    ) -> Result<NotificationWorkerHandle, BusError> {
        self.spawn_notification_thread::<Ds4OutputWorker>(serial_no, alive, fanout)
    }

    #[cfg(feature = "x360")]
    pub(crate) fn get_x360_user_index(&self, serial_no: u32) -> Result<u32, BusError> {
        // Separate in/out buffers - no aliasing through one object.
        let size = wire_size_of::<XusbGetUserIndex>();
        let mut io = IoctlBuffers::new(
            Arc::clone(&self.inner),
            Box::new(XusbGetUserIndex {
                size,
                serial_no,
                user_index: 0,
            }),
            Box::new(XusbGetUserIndex {
                size,
                serial_no,
                user_index: 0,
            }),
        )
        .map_err(map_ioctl(Operation::XUSB_GET_USER_INDEX, Some(serial_no)))?;

        let in_ptr = io.request_as_void();
        let out_ptr = io.response_as_void_mut();
        // SAFETY: two distinct heap buffers owned by `io` for the IRP lifetime.
        let ioctl_result = unsafe {
            io.call()
                .ioctl(IOCTL_XUSB_GET_USER_INDEX, in_ptr, size, out_ptr, size)
        };

        if io.finish() {
            // Never free or read response after poison (kernel may still write it).
            return Err(map_ioctl(Operation::XUSB_GET_USER_INDEX, Some(serial_no))(
                ioctl_result
                    .err()
                    .unwrap_or_else(|| windows::core::Error::from(ERROR_TIMEOUT)),
            ));
        }

        ioctl_result.map_err(map_ioctl(Operation::XUSB_GET_USER_INDEX, Some(serial_no)))?;
        Ok(io.response().user_index)
    }
}

pub(crate) struct DeviceInterfaceIterator {
    devices: HDEVINFO,
    index: u32,
    /// After end-of-list or a non-`ERROR_NO_MORE_ITEMS` failure, stop forever.
    /// Without this, a persistent enum error re-queries the same index and hangs
    /// `Bus::connect` / `Client::connect`.
    done: bool,
}

impl DeviceInterfaceIterator {
    pub(crate) fn new(devices: HDEVINFO) -> Self {
        DeviceInterfaceIterator {
            devices,
            index: 0,
            done: false,
        }
    }
}

impl Iterator for DeviceInterfaceIterator {
    type Item = Result<SP_DEVICE_INTERFACE_DATA, windows::core::Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }

        let mut data = SP_DEVICE_INTERFACE_DATA {
            cbSize: wire_size_of::<SP_DEVICE_INTERFACE_DATA>(),
            ..Default::default()
        };

        // SAFETY: `devices` valid for the iterator's lifetime.
        let result = unsafe {
            SetupDiEnumDeviceInterfaces(
                self.devices,
                None,
                &VIGEM_GUID as *const _,
                self.index,
                &mut data as *mut _,
            )
        };

        match result {
            Ok(_) => {
                self.index += 1;
                Some(Ok(data))
            }
            Err(e) if e.code() == ERROR_NO_MORE_ITEMS.to_hresult() => {
                self.done = true;
                None
            }
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}

impl core::iter::FusedIterator for DeviceInterfaceIterator {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fatal_notification_error_is_device_gone_only() {
        let gone = BusError::DeviceGone {
            operation: Operation::XUSB_REQUEST_NOTIFICATION,
            serial_no: Some(1),
        };
        assert!(is_fatal_notification_error(&gone));

        let aborted = BusError::classify_ioctl(
            Operation::XUSB_REQUEST_NOTIFICATION,
            Some(1),
            Win32Error::from_win32(995), // ERROR_OPERATION_ABORTED
        );
        assert!(
            !is_fatal_notification_error(&aborted),
            "ABORT is handled via shutdown flag, not as a fatal bus death"
        );
        assert!(
            aborted
                .win32_source()
                .is_some_and(Win32Error::is_operation_aborted)
        );

        let invalid = BusError::classify_ioctl(
            Operation::XUSB_REQUEST_NOTIFICATION,
            Some(1),
            Win32Error::from_win32(87), // ERROR_INVALID_PARAMETER
        );
        assert!(!is_fatal_notification_error(&invalid));

        let classified = BusError::classify_ioctl(
            Operation::XUSB_REQUEST_NOTIFICATION,
            Some(1),
            Win32Error::from_win32(433), // ERROR_NO_SUCH_DEVICE
        );
        assert!(is_fatal_notification_error(&classified));
    }

    #[test]
    fn classify_connect_failure_matrix() {
        let err = classify_connect_failure(false, false, None);
        match err {
            BusError::BusNotFound { source: None } => {}
            other => panic!("expected BusNotFound {{ source: None }}, got {other:?}"),
        }
        assert!(
            err.to_string().contains("vigem.org"),
            "BusNotFound display should mention the install URL"
        );

        let source = Win32Error::from_win32(2); // ERROR_FILE_NOT_FOUND
        let err = classify_connect_failure(true, false, Some(source));
        match err {
            BusError::BusNotFound { source: Some(e) } => {
                assert_eq!(e.win32_code(), Some(2));
            }
            other => panic!("expected BusNotFound with source, got {other:?}"),
        }

        // Access denied dominates BusNotFound and VersionMismatch (4-way).
        let denied = Win32Error::from_win32(5); // ERROR_ACCESS_DENIED
        for (saw_interface, version_mismatch) in
            [(true, false), (true, true), (false, false), (false, true)]
        {
            let err = classify_connect_failure(saw_interface, version_mismatch, Some(denied));
            match err {
                BusError::AccessDenied {
                    operation: Operation::OPEN_BUS,
                    serial_no: None,
                } => {}
                other => panic!(
                    "expected AccessDenied(OPEN_BUS) for saw={saw_interface} vm={version_mismatch}, got {other:?}"
                ),
            }
        }

        let source = Win32Error::from_win32(2); // ERROR_FILE_NOT_FOUND
        let err = classify_connect_failure(true, true, Some(source));
        match err {
            BusError::VersionMismatch { requested } => {
                assert_eq!(requested, VIGEM_COMMON_VERSION);
            }
            other => panic!("expected VersionMismatch, got {other:?}"),
        }
        let msg = err.to_string();
        assert!(
            msg.contains("version mismatch"),
            "VersionMismatch display should mention mismatch, got: {msg}"
        );
    }

    /// Live pin: unknown serial classifies as [`BusError::DeviceGone`].
    ///
    /// Empirical capture (ViGEmBus): never-plugged submit / get-user-index /
    /// notification return Win32 **55** (`ERROR_DEV_NOT_EXIST`,
    /// `STATUS_DEVICE_DOES_NOT_EXIST`). That code is mapped in `classify_ioctl`;
    /// `DeviceGone` does not retain the raw status, so this test asserts the
    /// public variant. Unplug of an unknown serial is a no-op (`Ok`).
    /// Notification workers treat `DeviceGone` as fatal (no 8x transient retry).
    /// Sync submit also fails fast (no event wait).
    #[test]
    #[ignore = "requires ViGEmBus driver"]
    #[cfg(feature = "x360")]
    fn unknown_serial_classifies_as_device_gone() {
        use std::time::{Duration, Instant};

        use crate::controller::x360::X360Report;
        use crate::error::BusError;

        let bus = Bus::connect().expect("ViGEmBus must be installed for this test");
        let never = 99_999u32;

        let assert_gone = |label: &str, err: BusError| {
            assert!(
                matches!(err, BusError::DeviceGone { .. }),
                "{label}: expected DeviceGone, got {err:?}"
            );
        };

        let start = Instant::now();
        let submit_err = bus
            .update_x360(never, &X360Report::default())
            .expect_err("never-plugged submit must fail");
        let elapsed = start.elapsed();
        assert_gone("XUSB_SUBMIT_REPORT", submit_err);
        assert!(
            elapsed < Duration::from_millis(500),
            "submit returned an error but took {elapsed:?} - it should not have waited"
        );

        assert_gone(
            "XUSB_GET_USER_INDEX",
            bus.get_x360_user_index(never)
                .expect_err("never-plugged get_user_index must fail"),
        );

        bus.unplug(never)
            .expect("never-plugged unplug should be Ok (no-op)");

        let fanout = Arc::new(crate::internal::notification_hub::Fanout::new());
        let (_id, rx) = fanout
            .subscribe(crate::NotificationBuffer::Bounded { capacity: 4 })
            .expect("subscribe");
        let alive = Arc::new(AtomicBool::new(true));
        let handle = bus
            .start_x360_notification_thread(never, Arc::clone(&alive), Arc::clone(&fanout))
            .expect("spawn notification worker");
        let first = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("notification worker should send a result within 2s");
        match first {
            Err(e) => assert_gone("XUSB_REQUEST_NOTIFICATION", e),
            Ok(n) => panic!("unexpected notification on never-plugged serial: {n:?}"),
        }
        // Worker exits after the fatal send.
        match rx.recv_timeout(Duration::from_secs(2)) {
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {}
            other => panic!("expected disconnect after DeviceGone, got {other:?}"),
        }
        alive.store(false, Ordering::Release);
        drop(handle);
    }

    /// Spawning and dropping notification workers releases Arc clones and does not
    /// grow process handle count (each worker owns one event for its lifetime;
    /// clean cancel+join must close it - poison path may leak deliberately).
    #[test]
    #[ignore = "requires ViGEmBus driver"]
    #[cfg(feature = "x360")]
    fn worker_spawn_drop_leaks_nothing() {
        use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};

        fn process_handle_count() -> u32 {
            let mut count = 0u32;
            // SAFETY: current process pseudo-handle; out-param is a live u32.
            unsafe {
                GetProcessHandleCount(GetCurrentProcess(), &mut count)
                    .expect("GetProcessHandleCount");
            }
            count
        }

        let bus = Bus::connect().expect("ViGEmBus must be installed for this test");
        let arc_baseline = Arc::strong_count(&bus.inner);

        // Warm one cycle so thread/CRT allocations settle before sampling baseline.
        {
            let fanout = Arc::new(crate::internal::notification_hub::Fanout::new());
            let (_id, _rx) = fanout
                .subscribe(crate::NotificationBuffer::Bounded { capacity: 4 })
                .expect("subscribe");
            let handle = bus
                .start_x360_notification_thread(
                    99_999,
                    Arc::new(AtomicBool::new(true)),
                    Arc::clone(&fanout),
                )
                .expect("spawn notification worker");
            drop(handle);
        }
        let handle_baseline = process_handle_count();

        // 50 cycles is enough to prove flatness; longer runs overlap stress churn.
        for i in 0..50 {
            let fanout = Arc::new(crate::internal::notification_hub::Fanout::new());
            let (_id, _rx) = fanout
                .subscribe(crate::NotificationBuffer::Bounded { capacity: 4 })
                .expect("subscribe");
            let handle = bus
                .start_x360_notification_thread(
                    99_999,
                    Arc::new(AtomicBool::new(true)),
                    Arc::clone(&fanout),
                )
                .expect("spawn notification worker");
            assert!(
                Arc::strong_count(&bus.inner) > arc_baseline,
                "worker/handle should hold at least one extra Arc clone while alive"
            );
            drop(handle);
            assert_eq!(
                Arc::strong_count(&bus.inner),
                arc_baseline,
                "worker / handle must release all Arc clones after drop (cycle {i})"
            );
            let now = process_handle_count();
            assert!(
                now <= handle_baseline.saturating_add(8),
                "handle count grew from {handle_baseline} to {now} after {i} spawn/drop cycles \
                 (expected flat with one event per worker)"
            );
        }

        let final_count = process_handle_count();
        assert!(
            final_count <= handle_baseline.saturating_add(8),
            "handle count after 50 cycles: baseline={handle_baseline}, final={final_count}"
        );
        assert_eq!(
            Arc::strong_count(&bus.inner),
            arc_baseline,
            "Arc count must return to baseline after 50 spawn/drop cycles"
        );
    }
}
