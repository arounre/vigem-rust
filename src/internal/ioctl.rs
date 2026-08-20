use core::mem::size_of;

use windows::Win32::System::Ioctl::{
    FILE_DEVICE_BUS_EXTENDER, FILE_READ_ACCESS, FILE_WRITE_ACCESS, METHOD_BUFFERED,
};

use crate::target::TargetType;

const fn ctl_code(device_type: u32, function: u32, method: u32, access: u32) -> u32 {
    (device_type << 16) | (access << 14) | (function << 2) | method
}

/// `size_of::<T>()` as `u32` for ViGEm `Size` / IOCTL buffer-length fields.
///
/// All wire structs in this crate are a few dozen bytes; the cast is lossless.
#[inline]
#[allow(clippy::cast_possible_truncation)]
pub(crate) const fn wire_size_of<T>() -> u32 {
    size_of::<T>() as u32
}

/// Types for which an all-zero bit pattern is a valid value of `Self`.
///
/// Used to gate [`zeroed_wire`] so padding-safe construction cannot be applied
/// to niches (`NonZero*`), references, or other invalid-zero types in safe code.
///
/// # Safety
/// Implementing this trait asserts that `core::mem::zeroed::<Self>()` produces a
/// valid value: no niches, no invalid discriminants, and every byte may be zero.
/// Implement only for integer-only / bitflags `repr(C)` wire PODs (and nested
/// aggregates of such types).
pub(crate) unsafe trait WireZeroable {}

/// All-zero wire value so `repr(C)` padding bytes are defined.
///
/// `METHOD_BUFFERED` copies the full `Size` into a system buffer; struct literals
/// leave padding uninit. Matches upstream `RtlZeroMemory` in ViGEm `*_INIT`
/// helpers. Do not pack unpadded wire types unless the MSVC layout is packed -
/// several bus structs are 12/16/20 with trailing pad that must be zeroed.
///
/// Only types that implement [`WireZeroable`] may be constructed this way.
#[inline]
pub(crate) fn zeroed_wire<T: WireZeroable>() -> T {
    // SAFETY: `WireZeroable` requires that the all-zero bit pattern is valid for `T`.
    unsafe { core::mem::zeroed() }
}

const FILE_DEVICE_BUSENUM: u32 = FILE_DEVICE_BUS_EXTENDER;
const IOCTL_VIGEM_BASE: u32 = 0x801;

pub(crate) const IOCTL_VIGEM_PLUGIN_TARGET: u32 = ctl_code(
    FILE_DEVICE_BUSENUM,
    IOCTL_VIGEM_BASE,
    METHOD_BUFFERED,
    FILE_WRITE_ACCESS,
);

pub(crate) const IOCTL_VIGEM_UNPLUG_TARGET: u32 = ctl_code(
    FILE_DEVICE_BUSENUM,
    IOCTL_VIGEM_BASE + 0x001,
    METHOD_BUFFERED,
    FILE_WRITE_ACCESS,
);

pub(crate) const IOCTL_VIGEM_CHECK_VERSION: u32 = ctl_code(
    FILE_DEVICE_BUSENUM,
    IOCTL_VIGEM_BASE + 0x002,
    METHOD_BUFFERED,
    FILE_WRITE_ACCESS,
);

pub(crate) const IOCTL_VIGEM_WAIT_DEVICE_READY: u32 = ctl_code(
    FILE_DEVICE_BUSENUM,
    IOCTL_VIGEM_BASE + 0x003,
    METHOD_BUFFERED,
    FILE_WRITE_ACCESS,
);

#[cfg(feature = "x360")]
pub(crate) const IOCTL_XUSB_REQUEST_NOTIFICATION: u32 = ctl_code(
    FILE_DEVICE_BUSENUM,
    IOCTL_VIGEM_BASE + 0x200,
    METHOD_BUFFERED,
    FILE_READ_ACCESS | FILE_WRITE_ACCESS,
);

#[cfg(feature = "x360")]
pub(crate) const IOCTL_XUSB_SUBMIT_REPORT: u32 = ctl_code(
    FILE_DEVICE_BUSENUM,
    IOCTL_VIGEM_BASE + 0x201,
    METHOD_BUFFERED,
    FILE_WRITE_ACCESS,
);

#[cfg(feature = "ds4")]
pub(crate) const IOCTL_DS4_SUBMIT_REPORT: u32 = ctl_code(
    FILE_DEVICE_BUSENUM,
    IOCTL_VIGEM_BASE + 0x202,
    METHOD_BUFFERED,
    FILE_WRITE_ACCESS,
);

#[cfg(feature = "ds4")]
pub(crate) const IOCTL_DS4_REQUEST_NOTIFICATION: u32 = ctl_code(
    FILE_DEVICE_BUSENUM,
    IOCTL_VIGEM_BASE + 0x203,
    METHOD_BUFFERED,
    FILE_WRITE_ACCESS,
);

#[cfg(feature = "x360")]
pub(crate) const IOCTL_XUSB_GET_USER_INDEX: u32 = ctl_code(
    FILE_DEVICE_BUSENUM,
    IOCTL_VIGEM_BASE + 0x206,
    METHOD_BUFFERED,
    FILE_READ_ACCESS | FILE_WRITE_ACCESS,
);

#[cfg(feature = "ds4")]
pub(crate) const IOCTL_DS4_AWAIT_OUTPUT_AVAILABLE: u32 = ctl_code(
    FILE_DEVICE_BUSENUM,
    IOCTL_VIGEM_BASE + 0x207,
    METHOD_BUFFERED,
    FILE_READ_ACCESS | FILE_WRITE_ACCESS,
);

// Wire structs mirror ViGEmClient (BusShared.h / Common.h); sizes pinned below.
// Packed types match upstream pshpack1. DS4 notification is write-only (upstream).

#[derive(Debug)]
#[repr(C)]
pub(crate) struct CheckVersion {
    pub(crate) size: u32,
    pub(crate) version: u32,
}

#[derive(Debug)]
#[repr(C)]
pub(crate) struct PluginTarget {
    pub(crate) size: u32,
    pub(crate) serial_no: u32,
    pub(crate) target_type: TargetType,
    pub(crate) vendor_id: u16,
    pub(crate) product_id: u16,
}

#[repr(C)]
pub(crate) struct UnPlugTarget {
    pub(crate) size: u32,
    pub(crate) serial_no: u32,
}

#[repr(C)]
pub(crate) struct WaitDeviceReady {
    pub(crate) size: u32,
    pub(crate) serial_no: u32,
}

#[cfg(feature = "x360")]
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct XusbRequestNotification {
    pub(crate) size: u32,
    pub(crate) serial_no: u32,
    pub(crate) large_motor: u8,
    pub(crate) small_motor: u8,
    pub(crate) led_number: u8,
}

// SAFETY: integer-only `repr(C)` wire POD (trailing pad is zero-valid).
#[cfg(feature = "x360")]
unsafe impl WireZeroable for XusbRequestNotification {}

#[cfg(feature = "x360")]
impl XusbRequestNotification {
    /// Zeroed (incl. trailing pad), then `Size` / serial set - like upstream INIT.
    pub(crate) fn new(serial_no: u32) -> Self {
        let mut req = zeroed_wire::<Self>();
        req.size = wire_size_of::<Self>();
        req.serial_no = serial_no;
        req
    }
}

#[cfg(feature = "x360")]
impl Default for XusbRequestNotification {
    fn default() -> Self {
        zeroed_wire()
    }
}

#[cfg(feature = "ds4")]
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct Ds4LightbarColorRaw {
    pub(crate) red: u8,
    pub(crate) green: u8,
    pub(crate) blue: u8,
}

#[cfg(feature = "ds4")]
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct Ds4OutputDataRaw {
    pub(crate) small_motor: u8,
    pub(crate) large_motor: u8,
    pub(crate) lightbar_color: Ds4LightbarColorRaw,
}

#[cfg(feature = "ds4")]
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct Ds4RequestNotification {
    pub(crate) size: u32,
    pub(crate) serial_no: u32,
    pub(crate) report: Ds4OutputDataRaw,
}

// SAFETY: integer-only `repr(C)` wire POD nested in integer fields (trailing pad zero-valid).
#[cfg(feature = "ds4")]
unsafe impl WireZeroable for Ds4RequestNotification {}

#[cfg(feature = "ds4")]
impl Ds4RequestNotification {
    /// Zeroed (incl. trailing pad), then `Size` / serial set - like upstream INIT.
    pub(crate) fn new(serial_no: u32) -> Self {
        let mut req = zeroed_wire::<Self>();
        req.size = wire_size_of::<Self>();
        req.serial_no = serial_no;
        req
    }
}

#[cfg(feature = "ds4")]
impl Default for Ds4RequestNotification {
    fn default() -> Self {
        zeroed_wire()
    }
}

/// Mirrors upstream `DS4_OUTPUT_BUFFER` (Common.h, inside `pshpack1`).
#[cfg(feature = "ds4")]
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub(crate) struct Ds4OutputBufferRaw {
    pub(crate) buffer: [u8; 64],
}

#[cfg(feature = "ds4")]
impl Default for Ds4OutputBufferRaw {
    fn default() -> Self {
        Self { buffer: [0; 64] }
    }
}

/// Mirrors upstream `DS4_AWAIT_OUTPUT` (BusShared.h, inside `pshpack1`).
///
/// `sizeof` is 72 with or without packing, but upstream is pack(1), so the Rust type is
/// too - keeping the declared alignment part of the ABI match.
#[cfg(feature = "ds4")]
#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub(crate) struct Ds4AwaitOutput {
    pub(crate) size: u32,
    pub(crate) serial_no: u32,
    pub(crate) report: Ds4OutputBufferRaw,
}

#[cfg(feature = "x360")]
#[repr(C)]
pub(crate) struct XusbGetUserIndex {
    pub(crate) size: u32,
    pub(crate) serial_no: u32,
    pub(crate) user_index: u32,
}

// Compile-time layout pins.
const _: () = {
    use core::mem::{offset_of, size_of};

    assert!(size_of::<CheckVersion>() == 8);
    assert!(size_of::<PluginTarget>() == 16);
    assert!(offset_of!(PluginTarget, vendor_id) == 12);
    assert!(size_of::<UnPlugTarget>() == 8);
    assert!(size_of::<WaitDeviceReady>() == 8);
};

#[cfg(feature = "x360")]
const _: () = {
    use core::mem::{align_of, offset_of, size_of};
    // 4+4+1+1+1 = 11, padded to 12 under align-4
    assert!(size_of::<XusbRequestNotification>() == 12);
    assert!(align_of::<XusbRequestNotification>() == 4);
    assert!(size_of::<XusbGetUserIndex>() == 12);
    assert!(offset_of!(XusbGetUserIndex, user_index) == 8);
};

#[cfg(feature = "ds4")]
const _: () = {
    use core::mem::{align_of, offset_of, size_of};
    assert!(size_of::<Ds4LightbarColorRaw>() == 3 && align_of::<Ds4LightbarColorRaw>() == 1);
    assert!(size_of::<Ds4OutputDataRaw>() == 5 && align_of::<Ds4OutputDataRaw>() == 1);
    // DS4_OUTPUT_REPORT is small-then-large; pin offsets so a field swap fails the build.
    assert!(offset_of!(Ds4OutputDataRaw, small_motor) == 0);
    assert!(offset_of!(Ds4OutputDataRaw, large_motor) == 1);
    assert!(offset_of!(Ds4OutputDataRaw, lightbar_color) == 2);
    // 4+4+5 = 13, padded to 16 under align-4 (zero pad at construct; do not pack)
    assert!(size_of::<Ds4RequestNotification>() == 16 && align_of::<Ds4RequestNotification>() == 4);
    assert!(offset_of!(Ds4RequestNotification, report) == 8);
    assert!(size_of::<Ds4OutputBufferRaw>() == 64 && align_of::<Ds4OutputBufferRaw>() == 1);
    // packed: 4+4+64 = 72, align 1
    assert!(size_of::<Ds4AwaitOutput>() == 72 && align_of::<Ds4AwaitOutput>() == 1);
    assert!(offset_of!(Ds4AwaitOutput, report) == 8);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zeroed_wire_is_all_zero_bytes() {
        #[cfg(feature = "x360")]
        {
            let v = zeroed_wire::<XusbRequestNotification>();
            // SAFETY: `v` is a live stack value; length matches its size.
            let bytes = unsafe {
                core::slice::from_raw_parts(
                    core::ptr::from_ref(&v).cast::<u8>(),
                    core::mem::size_of_val(&v),
                )
            };
            assert!(bytes.iter().all(|&b| b == 0));
        }
        #[cfg(feature = "ds4")]
        {
            let v = zeroed_wire::<Ds4RequestNotification>();
            // SAFETY: `v` is a live stack value; length matches its size.
            let bytes = unsafe {
                core::slice::from_raw_parts(
                    core::ptr::from_ref(&v).cast::<u8>(),
                    core::mem::size_of_val(&v),
                )
            };
            assert!(bytes.iter().all(|&b| b == 0));
        }
    }
}
