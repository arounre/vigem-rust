//! Build an [`X360Report`](crate::X360Report), OR together
//! [`X360Button`](crate::X360Button) flags, then
//! [`TargetHandle::update`](crate::TargetHandle::update). For host feedback, see
//! [`X360Notification`](crate::X360Notification).

use bitflags::bitflags;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

bitflags! {
    /// Digital buttons on a virtual Xbox 360 controller (combine with `|`).
    ///
    /// # Example
    /// ```
    /// use vigem_rust::X360Button;
    ///
    /// let buttons = X360Button::A | X360Button::LEFT_SHOULDER;
    /// assert!(buttons.contains(X360Button::A));
    /// ```
    #[repr(transparent)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
    #[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
    pub struct X360Button: u16 {
        /// D-pad up.
        const DPAD_UP          = 0x0001;
        /// D-pad down.
        const DPAD_DOWN        = 0x0002;
        /// D-pad left.
        const DPAD_LEFT        = 0x0004;
        /// D-pad right.
        const DPAD_RIGHT       = 0x0008;
        /// Start.
        const START            = 0x0010;
        /// Back.
        const BACK             = 0x0020;
        /// Left stick click (L3).
        const LEFT_THUMB       = 0x0040;
        /// Right stick click (R3).
        const RIGHT_THUMB      = 0x0080;
        /// Left bumper (LB).
        const LEFT_SHOULDER    = 0x0100;
        /// Right bumper (RB).
        const RIGHT_SHOULDER   = 0x0200;
        /// Guide / Xbox logo button.
        ///
        /// Standard XInput does not expose this bit to applications; only
        /// clients using the undocumented `XInputGetStateEx` path see it.
        /// Many games will not react to it.
        const GUIDE            = 0x0400;
        /// A (bottom face).
        const A                = 0x1000;
        /// B (right face).
        const B                = 0x2000;
        /// X (left face).
        const X                = 0x4000;
        /// Y (top face).
        const Y                = 0x8000;
    }
}

/// One frame of Xbox 360 input to send with [`update`](crate::TargetHandle::update).
///
/// Sticks are signed 16-bit (`0` = center). Triggers are `0..=255`.
/// [`Default`] is all zeroes: sticks centered, triggers released, no buttons.
///
/// # Example
///
/// ```no_run
/// # use vigem_rust::{Client, X360Report, X360Button};
/// # use std::error::Error;
/// # fn main() -> Result<(), Box<dyn Error>> {
/// # let client = Client::connect()?;
/// # let x360 = client.new_x360_target().plug()?.wait_for_ready()?;
/// let mut report = X360Report::default();
/// report.buttons = X360Button::A | X360Button::START;
/// report.thumb_lx = 16384;
/// report.right_trigger = 255;
/// x360.update(&report)?;
/// # Ok(())
/// # }
/// ```
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct X360Report {
    /// Face buttons, d-pad, shoulders, stick clicks (see [`X360Button`]).
    pub buttons: X360Button,
    /// Left trigger (`0` = released, `255` = fully pressed).
    pub left_trigger: u8,
    /// Right trigger (`0` = released, `255` = fully pressed).
    pub right_trigger: u8,
    /// Left stick X (`-32768..=32767`, `0` = center).
    pub thumb_lx: i16,
    /// Left stick Y (`-32768..=32767`, `0` = center; up is positive).
    pub thumb_ly: i16,
    /// Right stick X (`-32768..=32767`, `0` = center).
    pub thumb_rx: i16,
    /// Right stick Y (`-32768..=32767`, `0` = center; up is positive).
    pub thumb_ry: i16,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct XusbSubmitReport {
    pub size: u32,
    pub serial_no: u32,
    pub report: X360Report,
}

// Wire-format layout pins (must match ViGEmBus).
const _: () = {
    use core::mem::{align_of, offset_of, size_of};
    assert!(size_of::<X360Report>() == 12 && align_of::<X360Report>() == 2);
    assert!(size_of::<XusbSubmitReport>() == 20);
    assert!(offset_of!(XusbSubmitReport, report) == 8);
};

/// Host feedback for an Xbox 360 pad: rumble motors and player LED.
///
/// From [`register_notification`](crate::TargetHandle::register_notification).
///
/// # Example
/// ```no_run
/// # use vigem_rust::Client;
/// # use std::error::Error;
/// # fn main() -> Result<(), Box<dyn Error>> {
/// # let client = Client::connect()?;
/// # let x360 = client.new_x360_target().plug()?.wait_for_ready()?;
/// let notifications = x360.register_notification()?;
/// if let Ok(n) = notifications.try_recv() {
///     println!("LED {}  rumble L={} R={}", n.led_number, n.large_motor, n.small_motor);
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct X360Notification {
    /// Large (low-frequency) motor, `0..=255`.
    pub large_motor: u8,
    /// Small (high-frequency) motor, `0..=255`.
    pub small_motor: u8,
    /// Player LED index from the host. Prefer this over `user_index`.
    ///
    /// Typically `0..=3` on a real pad. This crate forwards the driver value as-is.
    pub led_number: u8,
}

impl X360Notification {
    /// Build a notification.
    ///
    /// This type is `#[non_exhaustive]`, so this is the only way to construct
    /// one outside this crate, useful for tests and replaying captures.
    #[inline]
    pub const fn new(large_motor: u8, small_motor: u8, led_number: u8) -> Self {
        Self {
            large_motor,
            small_motor,
            led_number,
        }
    }
}
