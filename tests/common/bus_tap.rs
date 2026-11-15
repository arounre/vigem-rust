//! A bare DS4 raw-output listener that does not use this crate.
//!
//! Opens its own ViGEmBus handle and reads `IOCTL_DS4_AWAIT_OUTPUT_AVAILABLE`
//! in a plain blocking loop. The driver gives every open handle its own copy
//! of every DS4 output report on the bus, so this sees what the crate's
//! listener is offered. Stress tests use it as the baseline for what the
//! driver itself delivers (it drops and repeats a few reports under load), so
//! that only a difference from it is held against the crate.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_Get_Device_Interface_List_SizeW,
    CM_Get_Device_Interface_ListW, CR_SUCCESS,
};
use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::{CancelIoEx, DeviceIoControl};
use windows::core::{GUID, PCWSTR};

/// ViGEmBus device interface class.
static GUID_DEVINTERFACE_BUSENUM_VIGEM: GUID = GUID::from_values(
    0x96E4_2B22,
    0xF5E9,
    0x42F8,
    [0xB0, 0x43, 0xED, 0x0F, 0x93, 0x2F, 0x01, 0x4F],
);

/// `CTL_CODE(FILE_DEVICE_BUS_EXTENDER, 0x801 + 0x207, METHOD_BUFFERED,
/// FILE_READ_ACCESS | FILE_WRITE_ACCESS)`.
const IOCTL_DS4_AWAIT_OUTPUT_AVAILABLE: u32 = 0x002A_E820;

/// `DS4_AWAIT_OUTPUT`: `Size: u32`, `SerialNo: u32`, 64 report bytes (packed).
const REQUEST_LEN: usize = 72;

#[derive(Clone, Copy)]
struct SendHandle(HANDLE);
// SAFETY: a file handle; the calls made on it are safe from any thread.
unsafe impl Send for SendHandle {}

/// Running listener; stops and closes its handle on drop.
pub struct BusTap {
    handle: SendHandle,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl BusTap {
    /// Start listening. `on_report` gets the serial of the pad each report
    /// was written to and its 64 bytes, in the order the driver hands them out.
    pub fn start(
        mut on_report: impl FnMut(u32, &[u8; 64]) + Send + 'static,
    ) -> Result<Self, String> {
        let handle = SendHandle(open_bus()?);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let spawned = thread::Builder::new()
            .name("bus-tap".into())
            .spawn(move || {
                let handle = handle;
                while !stop_thread.load(Ordering::Acquire) {
                    let mut request = [0u8; REQUEST_LEN];
                    request[..4].copy_from_slice(&(REQUEST_LEN as u32).to_le_bytes());
                    let mut response = [0u8; REQUEST_LEN];
                    let mut transferred = 0u32;
                    // SAFETY: synchronous handle; all buffers outlive the call.
                    let result = unsafe {
                        DeviceIoControl(
                            handle.0,
                            IOCTL_DS4_AWAIT_OUTPUT_AVAILABLE,
                            Some(request.as_ptr().cast()),
                            REQUEST_LEN as u32,
                            Some(response.as_mut_ptr().cast()),
                            REQUEST_LEN as u32,
                            Some(&raw mut transferred),
                            None,
                        )
                    };
                    if result.is_err() || transferred as usize != REQUEST_LEN {
                        // Cancelled by drop, or a driver error: do not spin.
                        thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    let serial = u32::from_le_bytes(response[4..8].try_into().expect("4 bytes"));
                    let report: &[u8; 64] = response[8..].try_into().expect("64 bytes");
                    on_report(serial, report);
                }
            });
        match spawned {
            Ok(thread) => Ok(Self {
                handle,
                stop,
                thread: Some(thread),
            }),
            Err(e) => {
                // SAFETY: handle from CreateFileW, not shared with any thread.
                unsafe {
                    let _ = CloseHandle(handle.0);
                }
                Err(format!("spawn bus tap thread: {e}"))
            }
        }
    }
}

impl Drop for BusTap {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            // The thread may be between the stop check and the blocking call,
            // so cancel until it has actually returned.
            while !thread.is_finished() {
                // SAFETY: the handle is open until after the join below.
                unsafe {
                    let _ = CancelIoEx(self.handle.0, None);
                }
                thread::sleep(Duration::from_millis(5));
            }
            let _ = thread.join();
        }
        // SAFETY: handle from CreateFileW; the only user has exited.
        unsafe {
            let _ = CloseHandle(self.handle.0);
        }
    }
}

fn open_bus() -> Result<HANDLE, String> {
    let mut len = 0u32;
    // SAFETY: stack out-param; static GUID; no device id filter.
    let cr = unsafe {
        CM_Get_Device_Interface_List_SizeW(
            &raw mut len,
            &raw const GUID_DEVINTERFACE_BUSENUM_VIGEM,
            PCWSTR::null(),
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        )
    };
    if cr != CR_SUCCESS || len <= 1 {
        return Err("no ViGEmBus device interface".into());
    }
    let mut paths = vec![0u16; len as usize];
    // SAFETY: `paths` is `len` characters as reported above.
    let cr = unsafe {
        CM_Get_Device_Interface_ListW(
            &raw const GUID_DEVINTERFACE_BUSENUM_VIGEM,
            PCWSTR::null(),
            &mut paths,
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        )
    };
    if cr != CR_SUCCESS {
        return Err("cannot list ViGEmBus device interfaces".into());
    }
    // First path of the NUL-separated list; it is NUL-terminated in place.
    // SAFETY: `paths` outlives the call.
    unsafe {
        CreateFileW(
            PCWSTR::from_raw(paths.as_ptr()),
            (GENERIC_READ | GENERIC_WRITE).0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .map_err(|e| format!("open ViGEmBus: {e}"))
}
