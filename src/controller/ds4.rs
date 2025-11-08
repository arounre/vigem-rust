use bitflags::bitflags;
use std::{
    mem,
    ops::{Deref, DerefMut},
};

bitflags! {
    /// Represents the main digital buttons on a virtual DualShock 4 controller.
    ///
    /// These flags can be combined using bitwise OR to represent multiple button presses.
    ///
    /// # Examples
    ///
    /// ```
    /// use vigem_rust::controller::ds4::Ds4Button;
    ///
    /// // Press the Triangle and Right Shoulder buttons.
    /// let buttons = Ds4Button::TRIANGLE | Ds4Button::SHOULDER_RIGHT;
    ///
    /// assert!(buttons.contains(Ds4Button::TRIANGLE));
    /// assert!(!buttons.contains(Ds4Button::CIRCLE));
    /// ```
    #[repr(transparent)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
    pub struct Ds4Button: u16 {
        const THUMB_RIGHT      = 1 << 15;
        const THUMB_LEFT       = 1 << 14;
        const OPTIONS          = 1 << 13;
        const SHARE            = 1 << 12;
        const TRIGGER_RIGHT    = 1 << 11;
        const TRIGGER_LEFT     = 1 << 10;
        const SHOULDER_RIGHT   = 1 << 9;
        const SHOULDER_LEFT    = 1 << 8;
        const TRIANGLE         = 1 << 7;
        const CIRCLE           = 1 << 6;
        const CROSS            = 1 << 5;
        const SQUARE           = 1 << 4;
    }
}

bitflags! {
    /// Represents the special buttons (PS, Touchpad) on a virtual DualShock 4 controller.
    #[repr(transparent)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
    pub struct Ds4SpecialButton: u8 {
        const PS           = 1 << 0;
        const TOUCHPAD     = 1 << 1;
    }
}

/// Represents the state of the D-Pad on a virtual DualShock 4 controller.
///
/// Unlike the main buttons, the D-Pad is represented by a single value, not a bitmask.
/// The `Ds4Report` struct provides a helper method `set_dpad` to correctly apply this state.
#[repr(u8)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Ds4Dpad {
    North = 0,
    NorthEast = 1,
    East = 2,
    SouthEast = 3,
    South = 4,
    SouthWest = 5,
    West = 6,
    NorthWest = 7,
    #[default]
    Neutral = 8,
}

/// Represents the standard input state of a virtual DualShock 4 controller.
///
/// An instance of this struct is sent to the bus via `TargetHandle::update` to
/// update the controller's state.
///
/// # Examples
///
/// ```
/// use vigem_rust::controller::ds4::{Ds4Report, Ds4Button, Ds4Dpad};
///
/// let mut report = Ds4Report::default();
///
/// // Set analog stick positions (128 is center)
/// report.thumb_lx = 200; // Move right
/// report.thumb_ly = 50;  // Move up
///
/// // Press the Cross button
/// report.buttons = Ds4Button::CROSS.bits();
///
/// // Set the D-Pad to South
/// report.set_dpad(Ds4Dpad::South);
///
/// // Pull the right trigger
/// report.trigger_r = 255;
/// ```
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Ds4Report {
    /// Left thumbstick X-axis (0-255). 128 is center.
    pub thumb_lx: u8,
    /// Left thumbstick Y-axis (0-255). 128 is center.
    pub thumb_ly: u8,
    /// Right thumbstick X-axis (0-255). 128 is center.
    pub thumb_rx: u8,
    /// Right thumbstick Y-axis (0-255). 128 is center.
    pub thumb_ry: u8,
    /// A bitmask of the main digital buttons and D-Pad state. See `Ds4Button` and `set_dpad`.
    pub buttons: u16,
    /// A bitmask of the special buttons. See `Ds4SpecialButton`.
    pub special: u8,
    /// Left trigger value (0-255).
    pub trigger_l: u8,
    /// Right trigger value (0-255).
    pub trigger_r: u8,
}

impl Ds4Report {
    /// Sets the D-Pad state on the report.
    ///
    /// This helper correctly manipulates the lower 4 bits of the `buttons` field
    /// to set the D-Pad state, leaving the other button flags untouched.
    ///
    /// # Examples
    ///
    /// ```
    /// use vigem_rust::controller::ds4::{Ds4Report, Ds4Button, Ds4Dpad};
    ///
    /// let mut report = Ds4Report::default();
    /// report.buttons = Ds4Button::SQUARE.bits();
    /// report.set_dpad(Ds4Dpad::East);
    ///
    /// // The Square button is still set
    /// assert!(report.buttons & Ds4Button::SQUARE.bits() != 0);
    /// // The D-Pad bits are correctly set
    /// assert_eq!(report.buttons & 0x000F, Ds4Dpad::East as u16);
    /// ```
    #[inline]
    pub fn set_dpad(&mut self, dpad: Ds4Dpad) {
        const DPAD_MASK: u16 = 0x000F;
        self.buttons = (self.buttons & !DPAD_MASK) | (dpad as u16);
    }
}

impl Default for Ds4Report {
    fn default() -> Self {
        let mut report = Self {
            thumb_lx: 128,
            thumb_ly: 128,
            thumb_rx: 128,
            thumb_ry: 128,
            buttons: 0,
            special: 0,
            trigger_l: 0,
            trigger_r: 0,
        };
        report.set_dpad(Ds4Dpad::Neutral);
        report
    }
}

// EXTENDED REPORT SECTION

/// Represents a single packet of touchpad data for a DualShock 4 controller.
///
/// The DS4 can track up to two simultaneous touch points. This struct contains
/// the state for both potential touches, along with a counter to sequence the packets.
/// It is used within the [`Ds4ReportExData`] struct.
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Ds4Touch {
    /// A timestamp or packet counter that increments with each new touch data packet,
    /// used to sequence events.
    pub packet_counter: u8,

    /// Touch state and tracking ID for the first touch point. This is a bit-packed field:
    /// - The most significant bit (MSB, `0x80`) indicates the contact state. This is
    ///   "active-low", meaning `0` for touch down and `1` for touch up.
    /// - The lower 7 bits (`0x7F`) are the tracking ID for the finger. This ID is unique
    ///   for a single press-drag-release gesture and increments for a new press.
    pub is_up_tracking_num_1: u8,

    /// The raw X/Y coordinate data for the first touch point, with a resolution of **1920x943**.
    ///
    /// This is a packed 24-bit value encoding a 12-bit X and 12-bit Y coordinate. The middle
    /// byte holds the 4 least significant bits of X and the 4 most significant bits of Y.
    ///
    /// You can unpack the coordinates like this:
    /// ```rust,ignore
    /// let x = (self.touch_data_1[0] as u16) | (((self.touch_data_1[1] & 0x0F) as u16) << 8);
    /// let y = (((self.touch_data_1[1] & 0xF0) as u16) >> 4) | ((self.touch_data_1[2] as u16) << 4);
    /// ```
    pub touch_data_1: [u8; 3],

    /// Touch state and tracking ID for the second touch point.
    /// Formatted identically to `is_up_tracking_num_1`.
    pub is_up_tracking_num_2: u8,

    /// The raw X/Y coordinate data for the second touch point.
    /// Formatted identically to `touch_data_1`.
    pub touch_data_2: [u8; 3],
}

impl Ds4Touch {
    /// Packs touchpad coordinates into the required 3-byte format.
    #[inline]
    fn pack_coords(buf: &mut [u8; 3], x: u16, y: u16) {
        // Clamp values to the valid touchpad range (1920x943).
        let x = x.min(1919);
        let y = y.min(942);

        // Pack the 12-bit X and 12-bit Y coordinates into 3 bytes.
        buf[0] = (x & 0xFF) as u8;
        buf[1] = (((x >> 8) & 0x0F) | ((y & 0x0F) << 4)) as u8;
        buf[2] = ((y >> 4) & 0xFF) as u8;
    }

    /// Unpacks touchpad coordinates from the 3-byte format.
    #[inline]
    fn unpack_coords(buf: &[u8; 3]) -> (u16, u16) {
        let x = (buf[0] as u16) | (((buf[1] & 0x0F) as u16) << 8);
        let y = (((buf[1] & 0xF0) as u16) >> 4) | ((buf[2] as u16) << 4);
        (x, y)
    }

    /// Sets the state for the first touch contact, abstracting away the bit-packing.
    ///
    /// # Arguments
    /// * `is_down` - `true` if the finger is touching the pad, `false` otherwise.
    /// * `tracking_num` - A unique ID for the finger gesture (0-127).
    /// * `x` - The X coordinate (0-1919).
    /// * `y` - The Y coordinate (0-942).
    #[inline]
    pub fn set_touch_1(&mut self, is_down: bool, tracking_num: u8, x: u16, y: u16) {
        let up_bit = if is_down { 0 } else { 1 << 7 };
        // Lower 7 bits are the tracking number
        self.is_up_tracking_num_1 = up_bit | (tracking_num & 0x7F);
        Self::pack_coords(&mut self.touch_data_1, x, y);
    }

    /// Sets the state for the second touch contact, abstracting away the bit-packing.
    ///
    /// # Arguments
    /// * `is_down` - `true` if the finger is touching the pad, `false` otherwise.
    /// * `tracking_num` - A unique ID for the finger gesture (0-127).
    /// * `x` - The X coordinate (0-1919).
    /// * `y` - The Y coordinate (0-942).
    #[inline]
    pub fn set_touch_2(&mut self, is_down: bool, tracking_num: u8, x: u16, y: u16) {
        let up_bit = if is_down { 0 } else { 1 << 7 };
        self.is_up_tracking_num_2 = up_bit | (tracking_num & 0x7F);
        Self::pack_coords(&mut self.touch_data_2, x, y);
    }

    /// Returns the packet counter/timestamp for this touch event.
    #[inline]
    pub fn get_packet_counter(&self) -> u8 {
        self.packet_counter
    }

    /// Returns `true` if the first touch point is active (finger is down).
    #[inline]
    pub fn get_is_down_1(&self) -> bool {
        // MSB is 0 for down, 1 for up.
        (self.is_up_tracking_num_1 & 0x80) == 0
    }

    /// Returns the tracking ID for the first touch point (0-127).
    #[inline]
    pub fn get_tracking_num_1(&self) -> u8 {
        // Lower 7 bits are the tracking number.
        self.is_up_tracking_num_1 & 0x7F
    }

    /// Returns the (X, Y) coordinates for the first touch point.
    /// X is in the range 0-1919, Y is in the range 0-942.
    #[inline]
    pub fn get_coords_1(&self) -> (u16, u16) {
        Self::unpack_coords(&self.touch_data_1)
    }

    /// Returns `true` if the second touch point is active (finger is down).
    #[inline]
    pub fn get_is_down_2(&self) -> bool {
        // MSB is 0 for down, 1 for up.
        (self.is_up_tracking_num_2 & 0x80) == 0
    }

    /// Returns the tracking ID for the second touch point (0-127).
    #[inline]
    pub fn get_tracking_num_2(&self) -> u8 {
        // Lower 7 bits are the tracking number.
        self.is_up_tracking_num_2 & 0x7F
    }

    /// Returns the (X, Y) coordinates for the second touch point.
    /// X is in the range 0-1919, Y is in the range 0-942.
    #[inline]
    pub fn get_coords_2(&self) -> (u16, u16) {
        Self::unpack_coords(&self.touch_data_2)
    }
}

/// Represents the complete, extended input state of a virtual DualShock 4 controller.
///
/// This struct is used for advanced scenarios that require simulating motion controls
/// (gyroscope and accelerometer) and detailed touchpad activity. It contains all the
/// fields from the standard [`Ds4Report`] plus additional data.
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct Ds4ReportExData {
    pub thumb_lx: u8,
    pub thumb_ly: u8,
    pub thumb_rx: u8,
    pub thumb_ry: u8,
    pub buttons: u16,
    pub special: u8,
    pub trigger_l: u8,
    pub trigger_r: u8,
    pub timestamp: u16,
    pub battery_lvl: u8,
    pub gyro_x: i16,
    pub gyro_y: i16,
    pub gyro_z: i16,
    pub accel_x: i16,
    pub accel_y: i16,
    pub accel_z: i16,
    pub _unknown1: [u8; 5],
    pub battery_lvl_special: u8,
    pub _unknown2: [u8; 2],
    pub touch_packets_n: u8,
    pub current_touch: Ds4Touch,
    pub previous_touch: [Ds4Touch; 2],
}

impl Ds4ReportExData {
    /// Returns an immutable reference to the standard [`Ds4Report`] portion of this extended report.
    /// This is safe because [`Ds4ReportExData`] is `#[repr(C)]` and starts with the exact
    /// same fields as [`Ds4Report`].
    #[inline]
    pub fn as_report(&self) -> &Ds4Report {
        // SAFETY: The memory layout is guaranteed to match due to #[repr(C)] on both structs.
        unsafe { &*(self as *const _ as *const Ds4Report) }
    }

    /// Returns a mutable reference to the standard [`Ds4Report`] portion of this extended report.
    /// This allows using helpers like `set_dpad` on the extended report.
    #[inline]
    pub fn as_report_mut(&mut self) -> &mut Ds4Report {
        // SAFETY: The memory layout is guaranteed to match due to #[repr(C)] on both structs.
        unsafe { &mut *(self as *mut _ as *mut Ds4Report) }
    }

    /// A convenience method to set the D-Pad state on the extended report.
    /// It correctly manipulates the `buttons` field.
    pub fn set_dpad(&mut self, dpad: Ds4Dpad) {
        self.as_report_mut().set_dpad(dpad);
    }
}

impl Default for Ds4ReportExData {
    /// Creates a new `Ds4ReportExData` with a valid default state (e.g., centered sticks).
    fn default() -> Self {
        let mut report: Self = unsafe { mem::zeroed() };
        let base = Ds4Report::default();
        report.thumb_lx = base.thumb_lx;
        report.thumb_ly = base.thumb_ly;
        report.thumb_rx = base.thumb_rx;
        report.thumb_ry = base.thumb_ry;
        report.buttons = base.buttons;
        report.special = base.special;
        report.trigger_l = base.trigger_l;
        report.trigger_r = base.trigger_r;
        report
    }
}

/// An extended report for DualShock 4, including motion and touch data.
///
/// This is used for more advanced scenarios where you need to simulate more than basic inputs,
/// such as gyroscope, accelerometer, or touchpad data. It is sent via the `update` method
/// on a `TargetHandle<DualShock4>`.
#[repr(C, packed)]
pub union Ds4ReportEx {
    pub report: Ds4ReportExData,
    pub report_buffer: [u8; 63],
}

impl Deref for Ds4ReportEx {
    type Target = Ds4ReportExData;

    fn deref(&self) -> &Self::Target {
        // SAFETY: Accessing the active union field is safe.
        unsafe { &self.report }
    }
}

impl DerefMut for Ds4ReportEx {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: Accessing the active union field is safe.
        unsafe { &mut self.report }
    }
}

impl Clone for Ds4ReportEx {
    fn clone(&self) -> Self {
        *self
    }
}

impl Copy for Ds4ReportEx {}

impl Default for Ds4ReportEx {
    /// Creates a new `Ds4ReportEx` with a valid default state.
    fn default() -> Self {
        Self {
            report: Ds4ReportExData::default(),
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Ds4SubmitReport {
    pub size: u32,
    pub serial_no: u32,
    pub report: Ds4Report,
}

#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub(crate) struct Ds4SubmitReportEx {
    pub size: u32,
    pub serial_no: u32,
    pub report: Ds4ReportEx,
}

// sanity check
const _: () = {
    assert!(
        mem::size_of::<Ds4ReportExData>() == 60,
        "Ds4ReportExData must be 60 bytes!"
    );
    assert!(
        mem::size_of::<Ds4ReportEx>() == 63,
        "Ds4ReportEx union must be 63 bytes!"
    );
};

/// Represents an RGB color for the DualShock 4 lightbar.
///
/// This struct is part of a `Ds4Notification` and contains the color values
/// sent by the host to be displayed on the controller's lightbar.
///
/// # Examples
///
/// ```
/// use vigem_rust::controller::ds4::Ds4LightbarColor;
///
/// let color = Ds4LightbarColor::new(255, 0, 128); // A pink color
/// assert_eq!(color.red, 255);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Ds4LightbarColor {
    /// The red component of the color (0-255).
    pub red: u8,
    /// The green component of the color (0-255).
    pub green: u8,
    /// The blue component of the color (0-255).
    pub blue: u8,
}

impl Ds4LightbarColor {
    #[inline]
    pub fn new(r: u8, g: u8, b: u8) -> Self {
        Self {
            red: r,
            green: g,
            blue: b,
        }
    }
}

/// A notification received from the bus for a DualShock 4 target.
///
/// This contains feedback from the system (e.g., a game), like rumble and
/// lightbar color commands. You can listen for these notifications using
/// `TargetHandle<DualShock4>::register_notification`.
///
/// # Examples
///
/// ```no_run
/// # use vigem_rust::{Client, target::DualShock4};
/// # use std::error::Error;
/// # fn main() -> Result<(), Box<dyn Error>> {
/// # let client = Client::connect()?;
/// # let ds4 = client.new_ds4_target().plugin()?;
/// let receiver = ds4.register_notification()?;
///
/// // In a separate thread or an event loop:
/// if let Ok(Ok(notification)) = receiver.recv() {
///     println!("Rumble: large={}, small={}", notification.large_motor, notification.small_motor);
///     println!("Lightbar: R={}, G={}, B={}",
///         notification.lightbar.red,
///         notification.lightbar.green,
///         notification.lightbar.blue
///     );
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Ds4Notification {
    /// Rumble strength for the large motor (0-255).
    pub large_motor: u8,
    /// Rumble strength for the small motor (0-255).
    pub small_motor: u8,
    /// The color for the controller's lightbar.
    pub lightbar: Ds4LightbarColor,
}

/// A raw 64-byte output packet received from the bus for a DS4 target.
///
/// This is for advanced use cases where you need to parse the raw output report from
/// the bus yourself, which may contain more detailed information than the standard
/// `Ds4Notification`. Obtain a receiver for this type via
/// `TargetHandle<DualShock4>::register_notification_raw_buffer`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ds4OutputBuffer {
    pub buf: [u8; 64],
}
