use windows::Win32::System::Ioctl::{
    FILE_DEVICE_BUS_EXTENDER, FILE_READ_ACCESS, FILE_WRITE_ACCESS, METHOD_BUFFERED,
};

use crate::target::TargetType;

const fn ctl_code(device_type: u32, function: u32, method: u32, access: u32) -> u32 {
    (device_type << 16) | (access << 14) | (function << 2) | method
}

const FILE_DEVICE_BUSENUM: u32 = FILE_DEVICE_BUS_EXTENDER;
const IOCTL_VIGEM_BASE: u32 = 0x801;

pub const IOCTL_VIGEM_PLUGIN_TARGET: u32 = ctl_code(
    FILE_DEVICE_BUSENUM,
    IOCTL_VIGEM_BASE,
    METHOD_BUFFERED,
    FILE_WRITE_ACCESS,
);

pub const IOCTL_VIGEM_UNPLUG_TARGET: u32 = ctl_code(
    FILE_DEVICE_BUSENUM,
    IOCTL_VIGEM_BASE + 0x001,
    METHOD_BUFFERED,
    FILE_WRITE_ACCESS,
);

pub const IOCTL_VIGEM_CHECK_VERSION: u32 = ctl_code(
    FILE_DEVICE_BUSENUM,
    IOCTL_VIGEM_BASE + 0x002,
    METHOD_BUFFERED,
    FILE_WRITE_ACCESS,
);

pub const IOCTL_VIGEM_WAIT_DEVICE_READY: u32 = ctl_code(
    FILE_DEVICE_BUSENUM,
    IOCTL_VIGEM_BASE + 0x003,
    METHOD_BUFFERED,
    FILE_WRITE_ACCESS,
);

pub const IOCTL_XUSB_REQUEST_NOTIFICATION: u32 = ctl_code(
    FILE_DEVICE_BUSENUM,
    IOCTL_VIGEM_BASE + 0x200,
    METHOD_BUFFERED,
    FILE_READ_ACCESS | FILE_WRITE_ACCESS,
);

pub const IOCTL_XUSB_SUBMIT_REPORT: u32 = ctl_code(
    FILE_DEVICE_BUSENUM,
    IOCTL_VIGEM_BASE + 0x201,
    METHOD_BUFFERED,
    FILE_WRITE_ACCESS,
);

pub const IOCTL_DS4_SUBMIT_REPORT: u32 = ctl_code(
    FILE_DEVICE_BUSENUM,
    IOCTL_VIGEM_BASE + 0x202,
    METHOD_BUFFERED,
    FILE_WRITE_ACCESS,
);

pub const IOCTL_DS4_REQUEST_NOTIFICATION: u32 = ctl_code(
    FILE_DEVICE_BUSENUM,
    IOCTL_VIGEM_BASE + 0x203,
    METHOD_BUFFERED,
    FILE_WRITE_ACCESS,
);

pub const IOCTL_XUSB_GET_USER_INDEX: u32 = ctl_code(
    FILE_DEVICE_BUSENUM,
    IOCTL_VIGEM_BASE + 0x206,
    METHOD_BUFFERED,
    FILE_READ_ACCESS | FILE_WRITE_ACCESS,
);

pub const IOCTL_DS4_AWAIT_OUTPUT_AVAILABLE: u32 = ctl_code(
    FILE_DEVICE_BUSENUM,
    IOCTL_VIGEM_BASE + 0x207,
    METHOD_BUFFERED,
    FILE_READ_ACCESS | FILE_WRITE_ACCESS,
);

// IOCTL STRUCTS
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

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct XusbRequestNotification {
    pub(crate) size: u32,
    pub(crate) serial_no: u32,
    pub(crate) large_motor: u8,
    pub(crate) small_motor: u8,
    pub(crate) led_number: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct Ds4LightbarColorRaw {
    pub(crate) red: u8,
    pub(crate) green: u8,
    pub(crate) blue: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct Ds4OutputDataRaw {
    pub(crate) large_motor: u8,
    pub(crate) small_motor: u8,
    pub(crate) lightbar_color: Ds4LightbarColorRaw,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct Ds4RequestNotification {
    pub(crate) size: u32,
    pub(crate) serial_no: u32,
    pub(crate) report: Ds4OutputDataRaw,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct Ds4OutputBufferRaw {
    pub(crate) buffer: [u8; 64],
}

impl Default for Ds4OutputBufferRaw {
    fn default() -> Self {
        Self { buffer: [0; 64] }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct Ds4AwaitOutput {
    pub(crate) size: u32,
    pub(crate) serial_no: u32,
    pub(crate) report: Ds4OutputBufferRaw,
}

#[repr(C)]
pub(crate) struct XusbGetUserIndex {
    pub(crate) size: u32,
    pub(crate) serial_no: u32,
    pub(crate) user_index: u32,
}
