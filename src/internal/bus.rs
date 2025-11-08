use std::ffi::c_void;
use std::ptr;
use std::sync::mpsc::Sender;
use std::sync::{Arc, mpsc};

use thiserror::Error;
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, HDEVINFO, SP_DEVICE_INTERFACE_DATA,
    SP_DEVICE_INTERFACE_DETAIL_DATA_W, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces,
    SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW,
};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_NO_MORE_ITEMS, GENERIC_READ, GENERIC_WRITE, HANDLE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_NO_BUFFERING, FILE_FLAG_OVERLAPPED,
    FILE_FLAG_WRITE_THROUGH, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::core::{GUID, PCWSTR};

#[cfg(feature = "ds4")]
use crate::controller::ds4::{
    Ds4Notification, Ds4OutputBuffer, Ds4Report, Ds4ReportEx, Ds4SubmitReport, Ds4SubmitReportEx,
};
#[cfg(feature = "x360")]
use crate::controller::x360::{X360Notification, X360Report, XusbSubmitReport};
use crate::internal::ioctl::*;
use crate::internal::notification_workers::*;
use crate::internal::overlapped::OverlappedCall;
use crate::target::Target;

#[derive(Debug, Error)]
pub enum BusError {
    #[error("Windows API Error: {0}")]
    WindowsAPIError(#[from] windows::core::Error),

    #[error("Version mismatch")]
    VersionMismatch,

    #[error("Bus not found")]
    BusNotFound,
}

const VIGEM_GUID: GUID = GUID::from_values(
    0x96E42B22,
    0xF5E9,
    0x42F8,
    [0xB0, 0x43, 0xED, 0x0F, 0x93, 0x2F, 0x01, 0x4F],
);

struct BusInner {
    handle: HANDLE,
}

impl Drop for BusInner {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

// The Win32 handle is safe to send between threads
unsafe impl Send for BusInner {}
unsafe impl Sync for BusInner {}

#[derive(Clone)]
pub(crate) struct Bus {
    inner: Arc<BusInner>,
}

impl Bus {
    pub(crate) fn connect() -> Result<Self, BusError> {
        unsafe {
            let devices = SetupDiGetClassDevsW(
                Some(&VIGEM_GUID as *const _),
                None,
                None,
                DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
            )?;

            // Ensure the device info list is destroyed on every path.
            struct DevInfoGuard(HDEVINFO);
            impl Drop for DevInfoGuard {
                fn drop(&mut self) {
                    unsafe {
                        let _ = SetupDiDestroyDeviceInfoList(self.0);
                    }
                }
            }
            let _guard = DevInfoGuard(devices);

            for iface_result in DeviceInterfaceIterator::new(devices) {
                let iface = iface_result?;
                // get required device detail size
                let mut needed: u32 = 0;
                let _ = SetupDiGetDeviceInterfaceDetailW(
                    devices,
                    &iface as *const _,
                    None,
                    0,
                    Some(&mut needed as *mut _),
                    None,
                );

                if needed == 0 {
                    continue;
                }

                let mut buf = vec![0u8; needed as usize];
                let detail_ptr = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
                let cb_size_ptr = ptr::addr_of_mut!((*detail_ptr).cbSize);
                *cb_size_ptr = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;

                if SetupDiGetDeviceInterfaceDetailW(
                    devices,
                    &iface as *const _,
                    Some(detail_ptr),
                    needed,
                    Some(&mut needed as *mut _),
                    None,
                )
                .is_err()
                {
                    continue;
                }

                // Try to open device handle
                let handle = match CreateFileW(
                    PCWSTR::from_raw((*detail_ptr).DevicePath.as_ptr()),
                    (GENERIC_READ | GENERIC_WRITE).0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    None,
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL
                        | FILE_FLAG_NO_BUFFERING
                        | FILE_FLAG_WRITE_THROUGH
                        | FILE_FLAG_OVERLAPPED,
                    None,
                ) {
                    Ok(h) => h,
                    Err(_) => continue,
                };

                // version
                let version = CheckVersion {
                    size: size_of::<CheckVersion>() as u32,
                    version: 0x0001,
                };
                let mut transferred: u32 = 0;
                if let Ok(()) = DeviceIoControl(
                    handle,
                    IOCTL_VIGEM_CHECK_VERSION,
                    Some(&version as *const _ as *const c_void),
                    version.size,
                    None,
                    0,
                    Some(&mut transferred as *mut _),
                    None,
                ) {
                    return Ok(Bus {
                        inner: Arc::new(BusInner { handle }),
                    });
                } else {
                    // Version mismatch
                    let _ = CloseHandle(handle);
                    //return Err(BusError::VersionMismatch);
                }
            }
        }

        Err(BusError::BusNotFound)
    }

    pub(crate) fn plug(&self, target: &Target, serial_no: u32) -> Result<(), BusError> {
        let plugin = PluginTarget {
            size: size_of::<PluginTarget>() as u32,
            serial_no,
            target_type: target.kind,
            vendor_id: target.vendor_id,
            product_id: target.product_id,
        };

        unsafe {
            let mut call = OverlappedCall::new()?;

            let _ = DeviceIoControl(
                self.inner.handle,
                IOCTL_VIGEM_PLUGIN_TARGET,
                Some(&plugin as *const _ as *const c_void),
                plugin.size,
                None,
                0,
                Some(call.transferred_ptr()),
                Some(call.as_mut_overlapped()),
            );

            call.wait(self.inner.handle)?;
        }

        // This 'wait device ready' call that is supposed to block until the controller
        // can receive updates doesn't seem to properly work...
        let wait_ready = WaitDeviceReady {
            size: size_of::<WaitDeviceReady>() as u32,
            serial_no,
        };

        unsafe {
            let mut call = OverlappedCall::new()?;

            let _ = DeviceIoControl(
                self.inner.handle,
                IOCTL_VIGEM_WAIT_DEVICE_READY,
                Some(&wait_ready as *const _ as *const c_void),
                wait_ready.size,
                None,
                0,
                Some(call.transferred_ptr()),
                Some(call.as_mut_overlapped()),
            );

            call.wait(self.inner.handle)?;
        }

        Ok(())
    }

    pub(crate) fn unplug(&self, serial_no: u32) -> Result<(), BusError> {
        let unplug = UnPlugTarget {
            size: size_of::<UnPlugTarget>() as u32,
            serial_no,
        };

        unsafe {
            let mut call = OverlappedCall::new()?;

            let _ = DeviceIoControl(
                self.inner.handle,
                IOCTL_VIGEM_UNPLUG_TARGET,
                Some(&unplug as *const _ as *const c_void),
                unplug.size,
                None,
                0,
                Some(call.transferred_ptr()),
                Some(call.as_mut_overlapped()),
            );

            call.wait(self.inner.handle)?;
        }

        Ok(())
    }

    #[cfg(feature = "x360")]
    pub(crate) fn update_x360(&self, serial_no: u32, report: &X360Report) -> Result<(), BusError> {
        let submit_report = XusbSubmitReport {
            size: size_of::<XusbSubmitReport>() as u32,
            serial_no,
            report: *report,
        };

        unsafe {
            let mut call = OverlappedCall::new()?;

            let _ = DeviceIoControl(
                self.inner.handle,
                IOCTL_XUSB_SUBMIT_REPORT,
                Some(&submit_report as *const _ as *const c_void),
                submit_report.size,
                None,
                0,
                Some(call.transferred_ptr()),
                Some(call.as_mut_overlapped()),
            );

            call.wait(self.inner.handle)?;
        }

        Ok(())
    }

    #[cfg(feature = "ds4")]
    pub(crate) fn update_ds4(&self, serial_no: u32, report: &Ds4Report) -> Result<(), BusError> {
        let submit_report = Ds4SubmitReport {
            size: size_of::<Ds4SubmitReport>() as u32,
            serial_no,
            report: *report,
        };

        unsafe {
            let mut call = OverlappedCall::new()?;

            let _ = DeviceIoControl(
                self.inner.handle,
                IOCTL_DS4_SUBMIT_REPORT,
                Some(&submit_report as *const _ as *const c_void),
                submit_report.size,
                None,
                0,
                Some(call.transferred_ptr()),
                Some(call.as_mut_overlapped()),
            );

            call.wait(self.inner.handle)?;
        }

        Ok(())
    }

    #[cfg(feature = "ds4")]
    pub(crate) fn update_ds4_ex(
        &self,
        serial_no: u32,
        report: &Ds4ReportEx,
    ) -> Result<(), BusError> {
        let submit_report = Ds4SubmitReportEx {
            size: size_of::<Ds4SubmitReportEx>() as u32,
            serial_no,
            report: *report,
        };

        unsafe {
            let mut call = OverlappedCall::new()?;

            // Note: We use the same IOCTL as the basic DS4 report.
            // The driver determines the report type by the size field apparently
            let _ = DeviceIoControl(
                self.inner.handle,
                IOCTL_DS4_SUBMIT_REPORT,
                Some(&submit_report as *const _ as *const c_void),
                submit_report.size,
                None,
                0,
                Some(call.transferred_ptr()),
                Some(call.as_mut_overlapped()),
            );

            call.wait(self.inner.handle)?;
        }

        Ok(())
    }

    pub(crate) fn spawn_notification_thread<W: NotificationWorker>(
        &self,
        serial_no: u32,
        sender: Sender<Result<W::Notification, BusError>>,
    ) -> Result<(), BusError> {
        let bus = self.clone();

        // Create a dedicated channel for startup synchronization.
        let (sync_tx, sync_rx) = mpsc::channel::<Result<(), BusError>>();

        std::thread::spawn(move || {
            // This is simply to try the fallible operation before starting the loop
            if let Err(e) = OverlappedCall::new() {
                let _ = sync_tx.send(Err(e.into()));
                return;
            }

            if sync_tx.send(Ok(())).is_err() {
                return;
            }

            loop {
                let mut request = W::create_request(serial_no);
                let mut call = match OverlappedCall::new() {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = sender.send(Err(e.into()));
                        return;
                    }
                };

                let req_size = size_of::<W::Request>() as u32;

                unsafe {
                    let _ = DeviceIoControl(
                        bus.inner.handle,
                        W::IOCTL_CODE,
                        Some(&request as *const _ as *const c_void),
                        req_size,
                        Some(&mut request as *mut _ as *mut c_void),
                        req_size,
                        Some(call.transferred_ptr()),
                        Some(call.as_mut_overlapped()),
                    );
                }

                match call.wait(bus.inner.handle) {
                    Ok(_) => {
                        let notification = W::process_response(&request);
                        if sender.send(Ok(notification)).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = sender.send(Err(e.into()));
                        break;
                    }
                }
            }
        });

        match sync_rx.recv() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(BusError::WindowsAPIError(windows::core::Error::new(
                windows::Win32::Foundation::E_FAIL,
                "Notification thread panicked during startup.",
            ))),
        }
    }

    #[cfg(feature = "x360")]
    pub(crate) fn start_x360_notification_thread(
        &self,
        serial_no: u32,
        sender: Sender<Result<X360Notification, BusError>>,
    ) -> Result<(), BusError> {
        self.spawn_notification_thread::<X360NotificationWorker>(serial_no, sender)
    }

    #[cfg(feature = "ds4")]
    pub(crate) fn start_ds4_notification_thread(
        &self,
        serial_no: u32,
        sender: Sender<Result<Ds4Notification, BusError>>,
    ) -> Result<(), BusError> {
        self.spawn_notification_thread::<Ds4NotificationWorker>(serial_no, sender)
    }

    #[cfg(feature = "ds4")]
    pub(crate) fn start_ds4_output_thread(
        &self,
        serial_no: u32,
        sender: Sender<Result<Ds4OutputBuffer, BusError>>,
    ) -> Result<(), BusError> {
        self.spawn_notification_thread::<Ds4OutputWorker>(serial_no, sender)
    }

    #[cfg(feature = "x360")]
    pub(crate) fn get_x360_user_index(&self, serial_no: u32) -> Result<u32, BusError> {
        let mut get_index = XusbGetUserIndex {
            size: size_of::<XusbGetUserIndex>() as u32,
            serial_no,
            user_index: 0,
        };

        unsafe {
            let mut call = OverlappedCall::new()?;

            let _ = DeviceIoControl(
                self.inner.handle,
                IOCTL_XUSB_GET_USER_INDEX,
                Some(&get_index as *const _ as *const c_void),
                get_index.size,
                Some(&mut get_index as *mut _ as *mut c_void),
                get_index.size,
                Some(call.transferred_ptr()),
                Some(call.as_mut_overlapped()),
            );

            call.wait(self.inner.handle)?;
        }

        Ok(get_index.user_index)
    }
}

// HELPER ITERATOR

pub struct DeviceInterfaceIterator {
    devices: HDEVINFO,
    index: u32,
}

impl DeviceInterfaceIterator {
    pub fn new(devices: HDEVINFO) -> Self {
        DeviceInterfaceIterator { devices, index: 0 }
    }
}

impl Iterator for DeviceInterfaceIterator {
    type Item = Result<SP_DEVICE_INTERFACE_DATA, windows::core::Error>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut data = SP_DEVICE_INTERFACE_DATA {
            cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
            ..Default::default()
        };

        // Perform the API call
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
            Err(e) if e.code() == ERROR_NO_MORE_ITEMS.to_hresult() => None,
            Err(e) => Some(Err(e)), // Propagate other errors
        }
    }
}
