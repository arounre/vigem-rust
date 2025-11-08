use bitflags::bitflags;

bitflags! {
    /// Represents the digital buttons on a virtual Xbox 360 controller.
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
    pub struct X360Button: u16 {
        const DPAD_UP          = 0x0001;
        const DPAD_DOWN        = 0x0002;
        const DPAD_LEFT        = 0x0004;
        const DPAD_RIGHT       = 0x0008;
        const START            = 0x0010;
        const BACK             = 0x0020;
        const LEFT_THUMB       = 0x0040;
        const RIGHT_THUMB      = 0x0080;
        const LEFT_SHOULDER    = 0x0100;
        const RIGHT_SHOULDER   = 0x0200;
        const GUIDE            = 0x0400;
        const A                = 0x1000;
        const B                = 0x2000;
        const X                = 0x4000;
        const Y                = 0x8000;
    }
}

/// Represents the full input state of a virtual Xbox 360 controller.
///
/// An instance of this struct is sent to the bus via `TargetHandle::update` to
/// update the controller's state.
///
/// # Examples
///
/// ```no_run
/// # use vigem_rust::{Client, X360Report, X360Button};
/// # let client = Client::connect().unwrap();
/// # let x360 = client.new_x360_target().plugin().unwrap();
/// # x360.wait_for_ready().unwrap();
/// let mut report = X360Report::default();
///
/// // Press the A and Start buttons
/// report.buttons = X360Button::A | X360Button::START;
///
/// // Move the left thumbstick halfway to the right
/// report.thumb_lx = 16384;
///
/// // Pull the right trigger all the way
/// report.right_trigger = 255;
///
/// x360.update(&report).unwrap();
/// ```
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct X360Report {
    /// A bitmask of the digital buttons.
    pub buttons: X360Button,
    /// Left trigger value (0-255).
    pub left_trigger: u8,
    /// Right trigger value (0-255).
    pub right_trigger: u8,
    /// Left thumbstick X-axis (-32768 to 32767). 0 is center.
    pub thumb_lx: i16,
    /// Left thumbstick Y-axis (-32768 to 32767). 0 is center.
    pub thumb_ly: i16,
    /// Right thumbstick X-axis (-32768 to 32767). 0 is center.
    pub thumb_rx: i16,
    /// Right thumbstick Y-axis (-32768 to 32767). 0 is center.
    pub thumb_ry: i16,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct XusbSubmitReport {
    pub size: u32,
    pub serial_no: u32,
    pub report: X360Report,
}

/// A notification received from the bus for an Xbox 360 target.
///
/// This contains feedback from the system or a game, such as rumble commands
/// or the player index assigned to the controller. You can receive these by
/// calling `TargetHandle::<Xbox360>::register_notification`.
///
/// # Examples
/// ```no_run
/// # use vigem_rust::{Client, target::Xbox360};
/// # use std::time::Duration;
/// # let client = Client::connect().unwrap();
/// # let x360 = client.new_x360_target().plugin().unwrap();
/// let notifications = x360.register_notification().unwrap();
///
/// // In a real application, you might check for notifications on a separate thread.
/// if let Ok(Ok(notification)) = notifications.try_recv() {
///     println!(
///         "Received notification: Player LED = {}, Large Motor = {}",
///         notification.led_number,
///         notification.large_motor
///     );
/// }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct X360Notification {
    /// Rumble strength for the large motor (0-255).
    pub large_motor: u8,
    /// Rumble strength for the small motor (0-255).
    pub small_motor: u8,
    /// The player number (0-3) assigned to the controller, indicated by the LED.
    /// This is the most reliable way to determine the controller's player index.
    pub led_number: u8,
}
