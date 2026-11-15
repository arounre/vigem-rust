//! Writes HID output reports to a virtual DS4 pad, the way a game does.
//!
//! This is what makes ViGEmBus produce DS4 host output (rumble, lightbar, the
//! raw 64-byte report) without a game running. The pad is found by the serial
//! ViGEmBus embeds in its device instance id, so several pads can be plugged
//! at once and each test thread still writes to its own.

use std::time::{Duration, Instant};

use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_Get_Device_ID_Size, CM_Get_Device_IDW,
    CM_Get_Device_Interface_List_SizeW, CM_Get_Device_Interface_ListW, CM_Get_Parent,
    CM_LOCATE_DEVNODE_NORMAL, CM_Locate_DevNodeW, CR_SUCCESS,
};
use windows::Win32::Foundation::{CloseHandle, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, WriteFile,
};
use windows::core::{GUID, PCWSTR};

/// Length of a DS4 USB output report (report id `0x05`).
pub const DS4_OUTPUT_REPORT_LEN: usize = 32;

static GUID_DEVINTERFACE_HID: GUID = GUID::from_values(
    0x4D1E_55B2,
    0xF16F,
    0x11CF,
    [0x88, 0xCB, 0x00, 0x11, 0x11, 0x00, 0x00, 0x30],
);

/// An open HID interface of one virtual DS4 pad, for writing output reports.
pub struct Ds4HidOutput {
    handle: HANDLE,
}

// SAFETY: a file handle; `WriteFile` on it is safe from any thread.
unsafe impl Send for Ds4HidOutput {}
// SAFETY: as above; concurrent synchronous writes are serialized by the kernel.
unsafe impl Sync for Ds4HidOutput {}

impl Ds4HidOutput {
    /// Open the HID interface of the DS4 pad plugged on `serial_no`, waiting
    /// up to `timeout` for Windows to create it.
    pub fn open(serial_no: u32, timeout: Duration) -> Result<Self, String> {
        let start = Instant::now();
        let mut last = String::from("no HID interface with that serial");
        loop {
            for path in hid_interface_paths() {
                // Default DS4 ids only: other devices' instance ids can end
                // in a number too, and must never be written to.
                if !path.to_ascii_lowercase().contains("vid_054c&pid_05c4")
                    || parent_serial(&path) != Some(serial_no)
                {
                    continue;
                }
                let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
                // SAFETY: `wide` is NUL-terminated and outlives the call.
                let opened = unsafe {
                    CreateFileW(
                        PCWSTR::from_raw(wide.as_ptr()),
                        GENERIC_WRITE.0,
                        FILE_SHARE_READ | FILE_SHARE_WRITE,
                        None,
                        OPEN_EXISTING,
                        FILE_ATTRIBUTE_NORMAL,
                        None,
                    )
                };
                match opened {
                    Ok(handle) => return Ok(Self { handle }),
                    Err(e) => last = format!("open {path}: {e}"),
                }
            }
            if start.elapsed() >= timeout {
                return Err(format!(
                    "HID interface of DS4 serial {serial_no} not usable within {timeout:?} ({last})"
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Write one output report. Fails once the pad is unplugged.
    pub fn write(&self, report: &[u8; DS4_OUTPUT_REPORT_LEN]) -> Result<(), String> {
        let mut written = 0u32;
        // SAFETY: synchronous handle; `report` and `written` outlive the call.
        unsafe { WriteFile(self.handle, Some(report), Some(&mut written), None) }
            .map_err(|e| format!("WriteFile: {e}"))?;
        if written as usize == DS4_OUTPUT_REPORT_LEN {
            Ok(())
        } else {
            Err(format!("short HID write ({written} bytes)"))
        }
    }
}

impl Drop for Ds4HidOutput {
    fn drop(&mut self) {
        // SAFETY: handle from CreateFileW, closed once.
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

/// Paths of every present HID device interface.
fn hid_interface_paths() -> Vec<String> {
    // The list can grow between the size query and the read; retry a few times.
    for _ in 0..4 {
        let mut len = 0u32;
        // SAFETY: stack out-param; static GUID; no device id filter.
        let cr = unsafe {
            CM_Get_Device_Interface_List_SizeW(
                &raw mut len,
                &raw const GUID_DEVINTERFACE_HID,
                PCWSTR::null(),
                CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
            )
        };
        if cr != CR_SUCCESS || len <= 1 {
            return Vec::new();
        }
        let mut buf = vec![0u16; len as usize];
        // SAFETY: `buf` is `len` characters as reported above.
        let cr = unsafe {
            CM_Get_Device_Interface_ListW(
                &raw const GUID_DEVINTERFACE_HID,
                PCWSTR::null(),
                &mut buf,
                CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
            )
        };
        if cr != CR_SUCCESS {
            continue;
        }
        // Multi-string: NUL-separated, ends with an empty string.
        return buf
            .split(|&c| c == 0)
            .filter(|s| !s.is_empty())
            .map(String::from_utf16_lossy)
            .collect();
    }
    Vec::new()
}

/// ViGEmBus serial of the pad that owns the HID interface at `path`.
///
/// `\\?\hid#vid_054c&pid_05c4#<instance>#{guid}` names the device
/// `HID\VID_054C&PID_05C4\<instance>`; its parent is the pad's USB device,
/// whose instance id ends in the serial (`...\1&1A590E2C&0&42`).
fn parent_serial(path: &str) -> Option<u32> {
    let body = path.strip_prefix(r"\\?\")?;
    let body = &body[..body.rfind("#{")?];
    let instance_id: Vec<u16> = body
        .replace('#', r"\")
        .encode_utf16()
        .chain(Some(0))
        .collect();

    let mut hid = 0u32;
    // SAFETY: `instance_id` is NUL-terminated and outlives the call.
    let cr = unsafe {
        CM_Locate_DevNodeW(
            &raw mut hid,
            PCWSTR::from_raw(instance_id.as_ptr()),
            CM_LOCATE_DEVNODE_NORMAL,
        )
    };
    if cr != CR_SUCCESS {
        return None;
    }
    let mut parent = 0u32;
    // SAFETY: `hid` is a DEVINST from CM_Locate_DevNodeW.
    if unsafe { CM_Get_Parent(&raw mut parent, hid, 0) } != CR_SUCCESS {
        return None;
    }

    let mut len = 0u32;
    // SAFETY: stack out-param.
    if unsafe { CM_Get_Device_ID_Size(&raw mut len, parent, 0) } != CR_SUCCESS {
        return None;
    }
    // The size excludes the terminating NUL.
    let mut buf = vec![0u16; len as usize + 1];
    // SAFETY: buffer sized as documented for CM_Get_Device_IDW.
    if unsafe { CM_Get_Device_IDW(parent, &mut buf, 0) } != CR_SUCCESS {
        return None;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let parent_id = String::from_utf16_lossy(&buf[..end]).to_ascii_uppercase();

    // Only a ViGEmBus DS4 parent: `USB\VID_054C&PID_05C4...\<idx>&<hash>&<idx>&<serial>`.
    if !parent_id.starts_with(r"USB\VID_054C&PID_05C4") {
        return None;
    }
    let last = parent_id.rsplit('\\').next()?;
    let fields: Vec<&str> = last.split('&').collect();
    if fields.len() != 4 {
        return None;
    }
    fields[3].parse().ok().filter(|&s| s != 0)
}
