//! `PnP` device-node helpers for reading occupied ViGEmBus serials.
//!
//! ViGEmBus embeds each pad's serial in the child's instance id (`"%02d"` in
//! `EmulationTargetPDO.cpp`). There is no "list serials" IOCTL, so occupancy is
//! inferred by walking the children of the bus device node this client opened.
//! See [`parse_serial`] for the two instance-id shapes.
//!
//! Enumeration lags the driver's internal child list and is advisory only.
//! Plug still retries on conflict. See `SerialDiscovery` on the public client.

use std::collections::HashSet;

use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_Child, CM_Get_Device_ID_Size, CM_Get_Device_IDW, CM_Get_Sibling,
    CM_LOCATE_DEVNODE_NORMAL, CM_Locate_DevNodeW, CM_MapCrToWin32Err, CONFIGRET, CR_SUCCESS,
};
use windows::core::PCWSTR;

use crate::error::{BusError, Win32Error};

/// Sibling-walk cap against a cyclic / malformed tree. Local so this module
/// does not depend on `client`. Four times the public scan window (64).
const MAX_CHILD_ITERATIONS: u32 = 256;

/// Default Win32 code when [`CM_MapCrToWin32Err`] has no mapping.
/// (`ERROR_GEN_FAILURE` - Win32 code space, not a `CONFIGRET` value.)
const DEFAULT_CONFIGRET_WIN32: u32 = 31;

/// `ERROR_NOT_FOUND`: two children parsed to the same serial (parse assumptions broken).
const ERROR_NOT_FOUND: u32 = 1168;

/// Serials of pads currently enumerated under `devnode_id`.
///
/// Returns `Err` only when the whole walk is meaningless (devnode missing,
/// enumeration refused, or two children claiming one serial). Unparseable or
/// vanishing children are skipped.
pub(crate) fn occupied_serials(devnode_id: &[u16]) -> Result<HashSet<u32>, BusError> {
    if devnode_id.is_empty() || devnode_id.last() != Some(&0) {
        return Err(BusError::Enumeration {
            source: Win32Error::from_win32(87), // ERROR_INVALID_PARAMETER
        });
    }

    let mut bus: u32 = 0;
    // SAFETY: `devnode_id` is NUL-terminated UTF-16 owned by the caller (or a
    // clone of the id stored on `BusInner` at connect).
    let cr = unsafe {
        CM_Locate_DevNodeW(
            &raw mut bus,
            PCWSTR::from_raw(devnode_id.as_ptr()),
            CM_LOCATE_DEVNODE_NORMAL,
        )
    };
    if cr != CR_SUCCESS {
        return Err(enumeration_configret(cr));
    }

    let mut out = HashSet::new();
    let mut child: u32 = 0;
    // SAFETY: `bus` is a valid DEVINST from CM_Locate_DevNodeW.
    // Non-success is empty occupancy: under-counting is safe (plug retries);
    // over-counting would hide free serials.
    if unsafe { CM_Get_Child(&raw mut child, bus, 0) } != CR_SUCCESS {
        return Ok(out);
    }

    let mut iterations = 0u32;
    loop {
        iterations = iterations.saturating_add(1);
        if iterations > MAX_CHILD_ITERATIONS {
            break;
        }
        if let Some(serial) = read_serial(child)
            && !out.insert(serial)
        {
            return Err(BusError::Enumeration {
                source: Win32Error::from_win32(ERROR_NOT_FOUND),
            });
        }
        let mut sibling: u32 = 0;
        // SAFETY: `child` is a DEVINST from CM_Get_Child / CM_Get_Sibling.
        if unsafe { CM_Get_Sibling(&raw mut sibling, child, 0) } != CR_SUCCESS {
            break;
        }
        child = sibling;
    }
    Ok(out)
}

/// Device instance id for `devinst` as a NUL-terminated UTF-16 buffer.
///
/// Connect treats failure as `None` and degrades discovery to [`crate::SerialDiscovery::Off`].
pub(crate) fn read_device_instance_id(devinst: u32) -> Result<Vec<u16>, BusError> {
    let mut len = 0u32;
    // SAFETY: `len` is a valid out-param; flags 0 is the documented default.
    let cr = unsafe { CM_Get_Device_ID_Size(&raw mut len, devinst, 0) };
    if cr != CR_SUCCESS {
        return Err(enumeration_configret(cr));
    }
    if len == 0 {
        // Empty id is a local validation failure, not a CONFIGRET; stay in Win32 code space.
        return Err(BusError::Enumeration {
            source: Win32Error::from_win32(DEFAULT_CONFIGRET_WIN32),
        });
    }

    // CM_Get_Device_ID_Size does **not** include the terminating NUL; the buffer
    // must be one character larger (docs for CM_Get_Device_IDW).
    let mut buf = vec![0u16; len as usize + 1];
    // SAFETY: buffer is sized for character count + NUL; flags 0.
    let cr = unsafe { CM_Get_Device_IDW(devinst, &mut buf, 0) };
    if cr != CR_SUCCESS {
        return Err(enumeration_configret(cr));
    }
    if buf.last() != Some(&0) {
        buf.push(0);
    }
    Ok(buf)
}

/// Map a `CONFIGRET` to [`BusError::Enumeration`] via [`CM_MapCrToWin32Err`].
///
/// `CONFIGRET` is not a Win32 code; this keeps diagnostics in the same space as the rest of the crate.
fn enumeration_configret(cr: CONFIGRET) -> BusError {
    // SAFETY: pure mapping; no pointers.
    let win32 = unsafe { CM_MapCrToWin32Err(cr, DEFAULT_CONFIGRET_WIN32) };
    BusError::Enumeration {
        source: Win32Error::from_win32(win32),
    }
}

fn read_device_id_string(devinst: u32) -> Option<String> {
    let mut len = 0u32;
    // SAFETY: stack out-param; flags 0.
    let cr = unsafe { CM_Get_Device_ID_Size(&raw mut len, devinst, 0) };
    if cr != CR_SUCCESS || len == 0 {
        return None;
    }
    // Size excludes the terminating NUL - allocate room for it.
    let mut buf = vec![0u16; len as usize + 1];
    // SAFETY: sized from CM_Get_Device_ID_Size + 1 for NUL.
    let cr = unsafe { CM_Get_Device_IDW(devinst, &mut buf, 0) };
    if cr != CR_SUCCESS {
        return None;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..end]))
}

/// Serial from a child DEVINST, or `None` if it vanished or the id is not a pad serial.
fn read_serial(devinst: u32) -> Option<u32> {
    read_device_id_string(devinst).and_then(|id| parse_serial(&id))
}

/// Parse a ViGEmBus pad serial from a `PnP` device instance id.
///
/// Two shapes, decided by the PDO's `DEVICE_CAPABILITIES.UniqueID`:
///
/// * `UniqueID = TRUE` (XUSB) - the driver's `"%02d"` is used verbatim:
///   `USB\VID_045E&PID_028E\35`
/// * `UniqueID = FALSE` (DS4) - PnP mangles it to
///   `<idx>&<parent-hash>&<idx>&<driver-instance-id>`:
///   `USB\VID_054C&PID_05C4&REV_0100\1&1A590E2C&0&42`
///
/// In both cases the driver-assigned value is the last `&`-delimited field of
/// the final path segment. Serial `0` is rejected (driver-forbidden).
pub(crate) fn parse_serial(device_id: &str) -> Option<u32> {
    let last = device_id.rsplit('\\').next()?;
    let mut fields = last.split('&');
    let first = fields.next()?;
    let serial = match (fields.next(), fields.next(), fields.next(), fields.next()) {
        (None, None, None, None) => first,
        // Reject other shapes rather than guessing (wrong guess hides free serials).
        (Some(hash), Some(_idx), Some(driver_id), None)
            if hash.bytes().all(|b| b.is_ascii_hexdigit()) =>
        {
            driver_id
        }
        _ => return None,
    };

    if serial.is_empty() || !serial.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    serial.parse::<u32>().ok().filter(|&s| s != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_serial_table() {
        // Shapes observed against stock ViGEmBus (XUSB UniqueID=TRUE, DS4 FALSE).
        let cases: &[(&str, Option<u32>)] = &[
            // XUSB: verbatim instance id
            (r"USB\VID_045E&PID_028E\01", Some(1)),
            (r"USB\VID_045E&PID_028E\07", Some(7)),
            (r"USB\VID_045E&PID_028E\35", Some(35)),
            (r"USB\VID_045E&PID_028E\100", Some(100)),
            (r"USB\VID_045E&PID_028E\00", None), // serial 0 rejected
            (r"USB\VID_045E&PID_028E\ABC", None),
            // DS4: PnP-mangled instance id (hash of parent is hex)
            (r"USB\VID_054C&PID_05C4&REV_0100\1&1A590E2C&0&01", Some(1)),
            (r"USB\VID_054C&PID_05C4&REV_0100\1&1A590E2C&0&41", Some(41)),
            (r"USB\VID_054C&PID_05C4&REV_0100\1&1A590E2C&0&42", Some(42)),
            (r"USB\VID_054C&PID_05C4\1&abcdef01&0&64", Some(64)),
            (r"USB\VID_045E&PID_028E\01&0000", None),
            (r"USB\VID_045E&PID_028E\01#2", None),
            (r"USB\VID_054C&PID_05C4\1&not-hex&0&42", None),
            (r"USB\VID_054C&PID_05C4\1&1A590E2C&0&42&extra", None),
            ("", None),
            ("42", Some(42)),
        ];
        for &(input, expected) in cases {
            assert_eq!(
                parse_serial(input),
                expected,
                "parse_serial({input:?}) should be {expected:?}"
            );
        }
    }
}
