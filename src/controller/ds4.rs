//! | You want... | Use |
//! |-----------|-----|
//! | Buttons, sticks, triggers | [`Ds4Report`](crate::Ds4Report) + [`update`](crate::TargetHandle::update) |
//! | Gyro, accel, touchpad | [`Ds4ReportEx`](crate::Ds4ReportEx) + [`update_ex`](crate::TargetHandle::update_ex) |
//! | Easy touch tracking ids | [`TouchpadTracker`](crate::TouchpadTracker) |
//! | Rumble + lightbar from games | [`Ds4Notification`](crate::Ds4Notification) via `register_notification` |
//! | Full 64-byte host output | [`Ds4OutputBuffer`](crate::Ds4OutputBuffer) via `register_notification_raw_buffer` |
//!
//! Face buttons: `report.buttons |= Ds4ButtonFlags::...`. D-pad:
//! [`Ds4Report::set_dpad`](crate::Ds4Report::set_dpad).

use bitflags::bitflags;
use std::{
    fmt,
    ops::{BitAnd, BitAndAssign, BitOr, BitOrAssign, Deref, DerefMut, Sub, SubAssign},
};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use crate::internal::ioctl::{WireZeroable, zeroed_wire};

bitflags! {
    /// Face, shoulder, and stick-click buttons (combine with `|`).
    ///
    /// Use `report.buttons |= flags` so the d-pad is preserved. See [`Ds4Buttons`].
    ///
    /// # Example
    /// ```
    /// use vigem_rust::Ds4ButtonFlags;
    /// let flags = Ds4ButtonFlags::TRIANGLE | Ds4ButtonFlags::SHOULDER_RIGHT;
    /// assert!(flags.contains(Ds4ButtonFlags::TRIANGLE));
    /// ```
    #[repr(transparent)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
    #[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
    pub struct Ds4ButtonFlags: u16 {
        /// Right thumbstick click (R3).
        const THUMB_RIGHT      = 1 << 15;
        /// Left thumbstick click (L3).
        const THUMB_LEFT       = 1 << 14;
        /// Options button.
        const OPTIONS          = 1 << 13;
        /// Share button.
        const SHARE            = 1 << 12;
        /// Right trigger digital bit (R2).
        ///
        /// Independent of the analog [`Ds4Report::trigger_right`] byte. A real
        /// pad sets both once the trigger passes its actuation point; set both
        /// if the host may read either.
        const TRIGGER_RIGHT    = 1 << 11;
        /// Left trigger digital bit (L2).
        ///
        /// Independent of the analog [`Ds4Report::trigger_left`] byte. See
        /// [`TRIGGER_RIGHT`](Self::TRIGGER_RIGHT).
        const TRIGGER_LEFT     = 1 << 10;
        /// Right shoulder (R1).
        const SHOULDER_RIGHT   = 1 << 9;
        /// Left shoulder (L1).
        const SHOULDER_LEFT    = 1 << 8;
        /// Triangle face button.
        const TRIANGLE         = 1 << 7;
        /// Circle face button.
        const CIRCLE           = 1 << 6;
        /// Cross face button.
        const CROSS            = 1 << 5;
        /// Square face button.
        const SQUARE           = 1 << 4;
    }
}

bitflags! {
    /// PS button and touchpad click (separate from the main button word).
    #[repr(transparent)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
    #[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
    pub struct Ds4SpecialButton: u8 {
        /// PlayStation / home button.
        const PS           = 1 << 0;
        /// Touchpad press (click), not finger position.
        const TOUCHPAD     = 1 << 1;
    }
}

/// D-pad direction (one value, not a bitflags set).
///
/// Set with [`Ds4Report::set_dpad`] so face buttons are not wiped.
#[repr(u8)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum Ds4Dpad {
    /// Up (`0`).
    North = 0,
    /// Up-right (`1`).
    NorthEast = 1,
    /// Right (`2`).
    East = 2,
    /// Down-right (`3`).
    SouthEast = 3,
    /// Down (`4`).
    South = 4,
    /// Down-left (`5`).
    SouthWest = 5,
    /// Left (`6`).
    West = 6,
    /// Up-left (`7`).
    NorthWest = 7,
    /// Released (`8`).
    #[default]
    Neutral = 8,
}

/// Packs a D-pad nibble into the low 4 bits of a DS4 buttons word.
///
/// Single source of truth for [`Ds4Buttons::with_dpad`], [`Ds4Report::set_dpad`],
/// and [`Ds4ReportExData::set_dpad`].
#[inline]
pub(crate) const fn ds4_pack_dpad(buttons: u16, dpad: Ds4Dpad) -> u16 {
    const DPAD_MASK: u16 = 0x000F;
    (buttons & !DPAD_MASK) | (dpad as u16)
}

/// Button and d-pad word for a DualShock 4 report.
///
/// The low 4 bits hold the d-pad; the rest are [`Ds4ButtonFlags`]. Using `|=`,
/// `&=`, `-=`, or the combinators keeps the d-pad when you change face/shoulder
/// buttons. There is no `BitOr<Ds4Buttons>`: the d-pad is one direction, not
/// flags, so bitwise OR of two full words would have to invent a merge rule. Combine
/// flags with `|= Ds4ButtonFlags::...` and set the d-pad with [`Ds4Buttons::with_dpad`].
/// Writing a raw `u16` from `flags.bits()` can wipe the d-pad.
///
/// With the `serde` feature, serializes as a raw `u16`.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize), serde(transparent))]
pub struct Ds4Buttons(u16);

const _: () = {
    use core::mem::{align_of, size_of};
    assert!(size_of::<Ds4Buttons>() == 2 && align_of::<Ds4Buttons>() == 2);
};

impl Ds4Buttons {
    /// Neutral d-pad and no face/shoulder buttons pressed.
    #[must_use]
    #[inline]
    pub const fn new() -> Self {
        Self(Ds4Dpad::Neutral as u16)
    }

    /// Add face/shoulder buttons; the d-pad is preserved.
    #[must_use]
    #[inline]
    pub const fn with(self, b: Ds4ButtonFlags) -> Self {
        Self(self.0 | b.bits())
    }

    /// Clear the given face/shoulder buttons; the d-pad is preserved.
    #[must_use]
    #[inline]
    pub const fn without(self, b: Ds4ButtonFlags) -> Self {
        Self(self.0 & !b.bits())
    }

    /// Set only the d-pad; face/shoulder buttons are preserved.
    #[must_use]
    #[inline]
    pub const fn with_dpad(self, d: Ds4Dpad) -> Self {
        Self(ds4_pack_dpad(self.0, d))
    }

    /// Whether all bits of `b` are set.
    #[must_use]
    #[inline]
    pub const fn contains(self, b: Ds4ButtonFlags) -> bool {
        (self.0 & b.bits()) == b.bits()
    }

    /// D-pad direction.
    ///
    /// Unknown values are returned as [`Ds4Dpad::Neutral`].
    #[must_use]
    #[inline]
    pub const fn dpad(self) -> Ds4Dpad {
        match self.0 & 0x000F {
            0 => Ds4Dpad::North,
            1 => Ds4Dpad::NorthEast,
            2 => Ds4Dpad::East,
            3 => Ds4Dpad::SouthEast,
            4 => Ds4Dpad::South,
            5 => Ds4Dpad::SouthWest,
            6 => Ds4Dpad::West,
            7 => Ds4Dpad::NorthWest,
            _ => Ds4Dpad::Neutral,
        }
    }

    /// Raw bits (button flags + d-pad).
    #[must_use]
    #[inline]
    pub const fn bits(self) -> u16 {
        self.0
    }

    /// Build from a raw buttons word.
    ///
    /// Unknown bits are kept as-is. The low nibble is the d-pad.
    #[must_use]
    #[inline]
    pub const fn from_bits_retain(bits: u16) -> Self {
        Self(bits)
    }

    /// Face/shoulder buttons only (d-pad cleared).
    #[must_use]
    #[inline]
    pub const fn flags(self) -> Ds4ButtonFlags {
        Ds4ButtonFlags::from_bits_truncate(self.0 & !0x000F)
    }
}

impl Default for Ds4Buttons {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Ds4Buttons {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `X360Button` Debug is `X360Button(A | START)`. Match that shape, with
        // the d-pad called out because it is a direction, not a flag.
        f.write_str("Ds4Buttons(")?;
        let mut wrote_flag = false;
        for (name, _) in self.flags().iter_names() {
            if wrote_flag {
                f.write_str(" | ")?;
            }
            f.write_str(name)?;
            wrote_flag = true;
        }
        if wrote_flag {
            f.write_str(" | ")?;
        }
        write!(f, "dpad: {:?})", self.dpad())
    }
}

impl BitOrAssign<Ds4ButtonFlags> for Ds4Buttons {
    #[inline]
    fn bitor_assign(&mut self, rhs: Ds4ButtonFlags) {
        *self = self.with(rhs);
    }
}

impl BitOr<Ds4ButtonFlags> for Ds4Buttons {
    type Output = Self;

    #[inline]
    fn bitor(self, rhs: Ds4ButtonFlags) -> Self {
        self.with(rhs)
    }
}

impl BitAnd<Ds4ButtonFlags> for Ds4Buttons {
    type Output = Self;

    /// Keep only flags in `rhs`; the d-pad is preserved.
    #[inline]
    fn bitand(self, rhs: Ds4ButtonFlags) -> Self {
        Self::from(self.flags() & rhs).with_dpad(self.dpad())
    }
}

impl BitAndAssign<Ds4ButtonFlags> for Ds4Buttons {
    #[inline]
    fn bitand_assign(&mut self, rhs: Ds4ButtonFlags) {
        *self = *self & rhs;
    }
}

impl Sub<Ds4ButtonFlags> for Ds4Buttons {
    type Output = Self;

    /// Remove face/shoulder flags; the d-pad is preserved.
    #[inline]
    fn sub(self, rhs: Ds4ButtonFlags) -> Self {
        self.without(rhs)
    }
}

impl SubAssign<Ds4ButtonFlags> for Ds4Buttons {
    #[inline]
    fn sub_assign(&mut self, rhs: Ds4ButtonFlags) {
        *self = self.without(rhs);
    }
}

impl From<Ds4ButtonFlags> for Ds4Buttons {
    #[inline]
    fn from(flags: Ds4ButtonFlags) -> Self {
        // Neutral dpad + flags (flags never occupy the low nibble).
        Self::new().with(flags)
    }
}

/// Standard DualShock 4 input (buttons, sticks, triggers) for
/// [`update`](crate::TargetHandle::update).
///
/// Stick axes are `0..=255` with **128 = center**. Prefer `buttons |= ...` and
/// [`set_dpad`](Self::set_dpad). For gyro and touchpad, use [`Ds4ReportEx`].
///
/// # Example
///
/// ```
/// use vigem_rust::{Ds4Report, Ds4ButtonFlags, Ds4Dpad};
///
/// let mut report = Ds4Report::default();
/// report.thumb_lx = 200; // right of center
/// report.thumb_ly = 50;  // above center
/// report.buttons |= Ds4ButtonFlags::CROSS;
/// report.set_dpad(Ds4Dpad::South);
/// report.trigger_right = 255;
/// report.buttons |= Ds4ButtonFlags::TRIGGER_RIGHT; // digital bit too
/// ```
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Ds4Report {
    /// Left stick X (`0..=255`, `128` = center; `0` = left, `255` = right).
    pub thumb_lx: u8,
    /// Left stick Y (`0..=255`, `128` = center; `0` = **up**, `255` = **down**).
    ///
    /// Note the axis is inverted relative to `X360Report::thumb_ly` (when the
    /// `x360` feature is enabled), where positive is up.
    pub thumb_ly: u8,
    /// Right stick X (`0..=255`, `128` = center; `0` = left, `255` = right).
    pub thumb_rx: u8,
    /// Right stick Y (`0..=255`, `128` = center; `0` = **up**, `255` = **down**).
    pub thumb_ry: u8,
    /// Face/shoulder buttons and d-pad. See [`Ds4Buttons`] and [`Ds4ButtonFlags`].
    pub buttons: Ds4Buttons,
    /// PS and touchpad-click buttons. See [`Ds4SpecialButton`].
    ///
    /// Prefer flags (`report.special |= Ds4SpecialButton::PS`) over raw bytes.
    /// For a raw `u8`, use [`Ds4SpecialButton::from_bits_retain`].
    pub special: Ds4SpecialButton,
    /// Left trigger (`0..=255`).
    pub trigger_left: u8,
    /// Right trigger (`0..=255`).
    pub trigger_right: u8,
}

impl Ds4Report {
    /// Set the d-pad without clearing face/shoulder buttons.
    ///
    /// # Examples
    ///
    /// ```
    /// use vigem_rust::{Ds4Report, Ds4ButtonFlags, Ds4Dpad};
    ///
    /// let mut report = Ds4Report::default();
    /// report.buttons |= Ds4ButtonFlags::SQUARE;
    /// report.set_dpad(Ds4Dpad::East);
    ///
    /// assert!(report.buttons.contains(Ds4ButtonFlags::SQUARE));
    /// assert_eq!(report.buttons.dpad(), Ds4Dpad::East);
    /// ```
    #[inline]
    pub fn set_dpad(&mut self, dpad: Ds4Dpad) {
        self.buttons = self.buttons.with_dpad(dpad);
    }

    /// Replace face/shoulder buttons without clearing the d-pad.
    #[inline]
    pub fn set_buttons(&mut self, flags: Ds4ButtonFlags) {
        let dpad = self.buttons.dpad();
        self.buttons = Ds4Buttons::from(flags).with_dpad(dpad);
    }
}

// SAFETY: `u8` fields + bitflags/newtype wrappers with no niches; zero is valid
// for every field (D-pad nibble 0 is a defined dpad encoding). Trailing pad zeroed.
unsafe impl WireZeroable for Ds4Report {}

impl Default for Ds4Report {
    /// Neutral pad: sticks centered at `128`, no buttons, d-pad
    /// [`Neutral`](Ds4Dpad::Neutral), triggers released.
    fn default() -> Self {
        // Zero first so the trailing align pad is defined (copied inside submits).
        let mut r = zeroed_wire::<Self>();
        r.thumb_lx = 128;
        r.thumb_ly = 128;
        r.thumb_rx = 128;
        r.thumb_ry = 128;
        r.buttons = Ds4Buttons::new();
        r.special = Ds4SpecialButton::empty();
        r.trigger_left = 0;
        r.trigger_right = 0;
        r
    }
}

// EXTENDED REPORT SECTION

/// A DualShock 4 touchpad coordinate (validated).
///
/// Origin is top-left; +X is right, +Y is down.
/// Range: `x` in `0..=`[`MAX_X`](Self::MAX_X), `y` in `0..=`[`MAX_Y`](Self::MAX_Y).
/// Prefer [`new`](Self::new); use [`new_clamped`](Self::new_clamped) only when
/// silent clamping is OK.
///
/// With the `serde` feature, out-of-range values fail to deserialize.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct TouchPoint {
    x: u16,
    y: u16,
}

impl TouchPoint {
    /// Maximum inclusive X (1919).
    pub const MAX_X: u16 = 1919;
    /// Maximum inclusive Y (942).
    pub const MAX_Y: u16 = 942;

    /// Create a validated touchpad point.
    ///
    /// Returns `None` if either coordinate is out of range.
    #[must_use]
    pub const fn new(x: u16, y: u16) -> Option<Self> {
        if x > Self::MAX_X || y > Self::MAX_Y {
            None
        } else {
            Some(Self { x, y })
        }
    }

    /// Create a point, clamping out-of-range coordinates.
    ///
    /// No error is reported; prefer [`new`](Self::new) when you want to know.
    #[must_use]
    pub const fn new_clamped(x: u16, y: u16) -> Self {
        let x = if x > Self::MAX_X { Self::MAX_X } else { x };
        let y = if y > Self::MAX_Y { Self::MAX_Y } else { y };
        Self { x, y }
    }

    /// X coordinate (`0..=1919`).
    #[must_use]
    pub const fn x(self) -> u16 {
        self.x
    }

    /// Y coordinate (`0..=942`).
    #[must_use]
    pub const fn y(self) -> u16 {
        self.y
    }
}

#[cfg(feature = "serde")]
impl<'de> Deserialize<'de> for TouchPoint {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Raw {
            x: u16,
            y: u16,
        }
        let raw = Raw::deserialize(deserializer)?;
        Self::new(raw.x, raw.y).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "TouchPoint out of range: x={} y={} (max {}x{})",
                raw.x,
                raw.y,
                Self::MAX_X,
                Self::MAX_Y
            ))
        })
    }
}

/// Contact state for one finger on the DualShock 4 touchpad.
///
/// Use with [`Ds4Touch::set_finger_1`] / [`set_finger_2`](Ds4Touch::set_finger_2),
/// or let [`TouchpadTracker`] assign tracking numbers for you.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum TouchState {
    /// Finger is down at `at` with the given contact id (`0..=127`).
    Down {
        /// Contact id for this press. Keep it stable for the whole
        /// press-drag-release gesture (see [`TouchpadTracker`]).
        tracking_num: u8,
        /// Pad coordinates.
        at: TouchPoint,
    },
    /// Finger is up.
    Up,
}

/// Assigns tracking numbers and packet counters for multi-frame touch gestures.
///
/// | Field | Rule |
/// |-------|------|
/// | `tracking_num` | Stays the same for one contact; new id on each press |
/// | `packet_counter` | Increments once per [`apply`](Self::apply) |
///
/// Prefer this when you only have per-frame finger positions and need the host
/// to track gestures correctly.
///
/// # Example
///
/// ```
/// use vigem_rust::{Ds4ReportEx, TouchPoint, TouchpadTracker};
///
/// let mut tracker = TouchpadTracker::new();
/// let mut report = Ds4ReportEx::default();
/// let p = TouchPoint::new(100, 200).unwrap();
/// tracker.apply(&mut report, Some(p), None);
/// assert!(report.current_touch.is_down_1());
/// ```
#[derive(Debug, Clone, Default)]
pub struct TouchpadTracker {
    packet_counter: u8,
    next_tracking_num: u8,
    finger_1_tracking: Option<u8>,
    finger_2_tracking: Option<u8>,
}

impl TouchpadTracker {
    /// New tracker: counters at zero, both fingers up.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            packet_counter: 0,
            next_tracking_num: 0,
            finger_1_tracking: None,
            finger_2_tracking: None,
        }
    }

    /// Update `report.current_touch` for this frame.
    ///
    /// - Increments the packet counter once.
    /// - On a new press, assigns a tracking number and keeps it until lift.
    /// - On lift (or still up), writes [`TouchState::Up`].
    pub fn apply(
        &mut self,
        report: &mut Ds4ReportEx,
        finger_1: Option<TouchPoint>,
        finger_2: Option<TouchPoint>,
    ) {
        self.packet_counter = self.packet_counter.wrapping_add(1);

        let state_1 = Self::transition(
            &mut self.finger_1_tracking,
            &mut self.next_tracking_num,
            finger_1,
        );
        let state_2 = Self::transition(
            &mut self.finger_2_tracking,
            &mut self.next_tracking_num,
            finger_2,
        );

        report.touch_packets_n = 1;
        report.current_touch.packet_counter = self.packet_counter;
        report.current_touch.set_finger_1(state_1);
        report.current_touch.set_finger_2(state_2);
    }

    /// Packet counter written so far (the next [`apply`](Self::apply) increments it).
    #[must_use]
    pub const fn packet_counter(&self) -> u8 {
        self.packet_counter
    }

    fn transition(
        slot: &mut Option<u8>,
        next_tracking_num: &mut u8,
        point: Option<TouchPoint>,
    ) -> TouchState {
        match (*slot, point) {
            (Some(id), Some(at)) => TouchState::Down {
                tracking_num: id,
                at,
            },
            (None, Some(at)) => {
                let id = *next_tracking_num & 0x7F;
                *next_tracking_num = next_tracking_num.wrapping_add(1) & 0x7F;
                *slot = Some(id);
                TouchState::Down {
                    tracking_num: id,
                    at,
                }
            }
            (Some(_), None) => {
                *slot = None;
                TouchState::Up
            }
            (None, None) => TouchState::Up,
        }
    }
}

/// One DualShock 4 touchpad sample (two fingers + sequence counter).
///
/// Prefer [`set_finger_1`](Self::set_finger_1) / [`TouchState`] or
/// [`TouchpadTracker`] over the older [`set_touch_1`](Self::set_touch_1) helpers.
///
/// [`Default`] is both fingers **up**. An all-zero sample is *not* idle on the
/// wire: bit 7 clear means the finger is down.
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Ds4Touch {
    /// Packet sequence counter.
    pub packet_counter: u8,

    /// Finger 1 status: bit 7 is `1` if up, `0` if down; bits `0..=6` are the
    /// tracking id. Prefer [`set_finger_1`](Self::set_finger_1).
    pub is_up_tracking_num_1: u8,

    /// Finger 1 packed 12-bit X/Y (see [`coords_1`](Self::coords_1)).
    pub touch_data_1: [u8; 3],

    /// Finger 2 status: same layout as [`is_up_tracking_num_1`](Self::is_up_tracking_num_1).
    pub is_up_tracking_num_2: u8,

    /// Finger 2 packed 12-bit X/Y (see [`coords_2`](Self::coords_2)).
    pub touch_data_2: [u8; 3],
}

/// Bit 7 set = finger up (DS4 HID). All-zero is *down* at tracking id 0.
const DS4_TOUCH_FINGER_UP: u8 = 0x80;

impl Default for Ds4Touch {
    /// Both fingers released (`0x80` on each tracking byte).
    fn default() -> Self {
        Self {
            packet_counter: 0,
            is_up_tracking_num_1: DS4_TOUCH_FINGER_UP,
            touch_data_1: [0; 3],
            is_up_tracking_num_2: DS4_TOUCH_FINGER_UP,
            touch_data_2: [0; 3],
        }
    }
}

impl Ds4Touch {
    /// Packs touchpad coordinates into the required 3-byte format, clamping to the pad.
    #[inline]
    fn pack_coords(buf: &mut [u8; 3], x: u16, y: u16) {
        let p = TouchPoint::new_clamped(x, y);
        Self::pack_coords_exact(buf, p.x(), p.y());
    }

    /// Packs already-validated coordinates (no clamp).
    #[inline]
    fn pack_coords_exact(buf: &mut [u8; 3], x: u16, y: u16) {
        // Pack the 12-bit X and 12-bit Y coordinates into 3 bytes.
        // Masks keep each cast within `u8` range (clippy: intentional truncation).
        #[allow(clippy::cast_possible_truncation)]
        {
            buf[0] = (x & 0xFF) as u8;
            buf[1] = (((x >> 8) & 0x0F) | ((y & 0x0F) << 4)) as u8;
            buf[2] = ((y >> 4) & 0xFF) as u8;
        }
    }

    /// Unpacks touchpad coordinates from the 3-byte format.
    #[inline]
    fn unpack_coords(buf: &[u8; 3]) -> (u16, u16) {
        let x = (buf[0] as u16) | (((buf[1] & 0x0F) as u16) << 8);
        let y = (((buf[1] & 0xF0) as u16) >> 4) | ((buf[2] as u16) << 4);
        (x, y)
    }

    /// Pack `state` into finger 1's status byte and coordinate bytes.
    #[inline]
    pub fn set_finger_1(&mut self, state: TouchState) {
        Self::set_finger(
            &mut self.is_up_tracking_num_1,
            &mut self.touch_data_1,
            state,
        );
    }

    /// Pack `state` into finger 2's status byte and coordinate bytes.
    #[inline]
    pub fn set_finger_2(&mut self, state: TouchState) {
        Self::set_finger(
            &mut self.is_up_tracking_num_2,
            &mut self.touch_data_2,
            state,
        );
    }

    /// Finger 1 as a [`TouchState`].
    #[must_use]
    #[inline]
    pub fn finger_1(&self) -> TouchState {
        Self::finger(self.is_down_1(), self.tracking_num_1(), self.coords_1())
    }

    /// Finger 2 as a [`TouchState`].
    #[must_use]
    #[inline]
    pub fn finger_2(&self) -> TouchState {
        Self::finger(self.is_down_2(), self.tracking_num_2(), self.coords_2())
    }

    #[inline]
    fn set_finger(is_up_tracking: &mut u8, touch_data: &mut [u8; 3], state: TouchState) {
        match state {
            TouchState::Down { tracking_num, at } => {
                *is_up_tracking = tracking_num & 0x7F; // down -> MSB clear
                Self::pack_coords_exact(touch_data, at.x(), at.y());
            }
            TouchState::Up => {
                *is_up_tracking |= DS4_TOUCH_FINGER_UP;
            }
        }
    }

    #[inline]
    fn finger(is_down: bool, tracking_num: u8, coords: (u16, u16)) -> TouchState {
        if is_down {
            TouchState::Down {
                tracking_num,
                // Wire coords are always produced via clamp/exact pack, so they fit.
                at: TouchPoint::new_clamped(coords.0, coords.1),
            }
        } else {
            TouchState::Up
        }
    }

    /// Set finger 1 from raw fields (deprecated).
    ///
    /// Prefer [`set_finger_1`](Self::set_finger_1) with [`TouchState`].
    #[deprecated(
        since = "0.2.0",
        note = "use set_finger_1 with TouchState (or TouchpadTracker)"
    )]
    #[inline]
    pub fn set_touch_1(&mut self, is_down: bool, tracking_num: u8, x: u16, y: u16) {
        if is_down {
            self.set_finger_1(TouchState::Down {
                tracking_num,
                at: TouchPoint::new_clamped(x, y),
            });
        } else {
            // Preserve legacy behaviour: write tracking + coords even when up.
            self.is_up_tracking_num_1 = (1 << 7) | (tracking_num & 0x7F);
            Self::pack_coords(&mut self.touch_data_1, x, y);
        }
    }

    /// Set finger 2 from raw fields (deprecated).
    ///
    /// Prefer [`set_finger_2`](Self::set_finger_2) with [`TouchState`].
    #[deprecated(
        since = "0.2.0",
        note = "use set_finger_2 with TouchState (or TouchpadTracker)"
    )]
    #[inline]
    pub fn set_touch_2(&mut self, is_down: bool, tracking_num: u8, x: u16, y: u16) {
        if is_down {
            self.set_finger_2(TouchState::Down {
                tracking_num,
                at: TouchPoint::new_clamped(x, y),
            });
        } else {
            self.is_up_tracking_num_2 = (1 << 7) | (tracking_num & 0x7F);
            Self::pack_coords(&mut self.touch_data_2, x, y);
        }
    }

    /// `true` if finger 1 is down.
    #[inline]
    pub fn is_down_1(&self) -> bool {
        (self.is_up_tracking_num_1 & 0x80) == 0
    }

    /// Tracking id for finger 1 (`0..=127`).
    #[inline]
    pub fn tracking_num_1(&self) -> u8 {
        self.is_up_tracking_num_1 & 0x7F
    }

    /// `(x, y)` for finger 1 (`x` 0..=1919, `y` 0..=942).
    #[inline]
    pub fn coords_1(&self) -> (u16, u16) {
        Self::unpack_coords(&self.touch_data_1)
    }

    /// `true` if finger 2 is down.
    #[inline]
    pub fn is_down_2(&self) -> bool {
        (self.is_up_tracking_num_2 & 0x80) == 0
    }

    /// Tracking id for finger 2 (`0..=127`).
    #[inline]
    pub fn tracking_num_2(&self) -> u8 {
        self.is_up_tracking_num_2 & 0x7F
    }

    /// `(x, y)` for finger 2 (`x` 0..=1919, `y` 0..=942).
    #[inline]
    pub fn coords_2(&self) -> (u16, u16) {
        Self::unpack_coords(&self.touch_data_2)
    }
}

/// Full DS4 input including motion sensors and touchpad (payload of [`Ds4ReportEx`]).
///
/// Most code should use [`Ds4ReportEx`] and set fields through `Deref`
/// (`report_ex.gyro_x = ...`). Start from [`Default`]. Reserved padding fields
/// mean you cannot use struct update syntax from outside this crate.
///
/// This type is packed. Read and write multi-byte fields by value
/// (`let g = report.gyro_x;`), not by taking references to them.
///
/// Motion, timestamp, and battery fields are the raw HID values ViGEmBus
/// forwards. This crate does not convert them to physical units.
///
/// With the `serde` feature, reserved padding is skipped (zeros on deserialize).
#[repr(C, packed)]
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Ds4ReportExData {
    /// Left stick X (`0..=255`, `128` = center; `0` = left, `255` = right).
    pub thumb_lx: u8,
    /// Left stick Y (`0..=255`, `128` = center; `0` = **up**, `255` = **down**).
    pub thumb_ly: u8,
    /// Right stick X (`0..=255`, `128` = center; `0` = left, `255` = right).
    pub thumb_rx: u8,
    /// Right stick Y (`0..=255`, `128` = center; `0` = **up**, `255` = **down**).
    pub thumb_ry: u8,
    /// Face/shoulder buttons and d-pad; see [`Ds4Buttons`].
    pub buttons: Ds4Buttons,
    /// PS and touchpad-click buttons; see [`Ds4SpecialButton`].
    pub special: Ds4SpecialButton,
    /// Left trigger (`0..=255`).
    pub trigger_left: u8,
    /// Right trigger (`0..=255`).
    pub trigger_right: u8,
    /// 16-bit rolling timestamp from the pad (not wall-clock).
    pub timestamp: u16,
    /// Raw battery field (not a percentage this crate computes).
    pub battery_lvl: u8,
    /// Raw gyroscope X.
    pub gyro_x: i16,
    /// Raw gyroscope Y.
    pub gyro_y: i16,
    /// Raw gyroscope Z.
    pub gyro_z: i16,
    /// Raw accelerometer X.
    pub accel_x: i16,
    /// Raw accelerometer Y.
    pub accel_y: i16,
    /// Raw accelerometer Z.
    pub accel_z: i16,
    #[cfg_attr(feature = "serde", serde(skip))]
    _reserved1: [u8; 5],
    /// Raw extra battery / charging bits from the extended HID report.
    pub battery_lvl_special: u8,
    #[cfg_attr(feature = "serde", serde(skip))]
    _reserved2: [u8; 2],
    /// Number of touch packets in this report (usually `1`).
    pub touch_packets_n: u8,
    /// Current touch sample.
    pub current_touch: Ds4Touch,
    /// Previous touch samples.
    pub previous_touch: [Ds4Touch; 2],
}

impl Ds4ReportExData {
    /// Copy the basic buttons/sticks/triggers into a [`Ds4Report`].
    #[inline]
    pub fn to_report(&self) -> Ds4Report {
        Ds4Report {
            thumb_lx: self.thumb_lx,
            thumb_ly: self.thumb_ly,
            thumb_rx: self.thumb_rx,
            thumb_ry: self.thumb_ry,
            buttons: self.buttons,
            special: self.special,
            trigger_left: self.trigger_left,
            trigger_right: self.trigger_right,
        }
    }

    /// Overwrite buttons/sticks/triggers from `src`; leave motion and touch alone.
    #[inline]
    pub fn set_report(&mut self, src: &Ds4Report) {
        self.thumb_lx = src.thumb_lx;
        self.thumb_ly = src.thumb_ly;
        self.thumb_rx = src.thumb_rx;
        self.thumb_ry = src.thumb_ry;
        self.buttons = src.buttons;
        self.special = src.special;
        self.trigger_left = src.trigger_left;
        self.trigger_right = src.trigger_right;
    }

    /// Set the d-pad without clearing face/shoulder buttons.
    #[inline]
    pub fn set_dpad(&mut self, dpad: Ds4Dpad) {
        self.buttons = self.buttons.with_dpad(dpad);
    }

    /// Replace face/shoulder buttons without clearing the d-pad.
    #[inline]
    pub fn set_buttons(&mut self, flags: Ds4ButtonFlags) {
        let dpad = self.buttons.dpad();
        self.buttons = Ds4Buttons::from(flags).with_dpad(dpad);
    }
}

impl Default for Ds4ReportExData {
    /// Centered sticks; motion zeroed; touch fingers up.
    fn default() -> Self {
        let base = Ds4Report::default();
        Self {
            thumb_lx: base.thumb_lx,
            thumb_ly: base.thumb_ly,
            thumb_rx: base.thumb_rx,
            thumb_ry: base.thumb_ry,
            buttons: base.buttons,
            special: base.special,
            trigger_left: base.trigger_left,
            trigger_right: base.trigger_right,
            timestamp: 0,
            battery_lvl: 0,
            gyro_x: 0,
            gyro_y: 0,
            gyro_z: 0,
            accel_x: 0,
            accel_y: 0,
            accel_z: 0,
            _reserved1: [0; 5],
            battery_lvl_special: 0,
            _reserved2: [0; 2],
            touch_packets_n: 0,
            current_touch: Ds4Touch::default(),
            previous_touch: [Ds4Touch::default(); 2],
        }
    }
}

impl fmt::Debug for Ds4ReportExData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Packed fields cannot be borrowed; format by-value copies.
        let thumb_lx = self.thumb_lx;
        let thumb_ly = self.thumb_ly;
        let thumb_rx = self.thumb_rx;
        let thumb_ry = self.thumb_ry;
        let buttons = self.buttons;
        let special = self.special;
        let trigger_left = self.trigger_left;
        let trigger_right = self.trigger_right;
        let timestamp = self.timestamp;
        let battery_lvl = self.battery_lvl;
        let gyro_x = self.gyro_x;
        let gyro_y = self.gyro_y;
        let gyro_z = self.gyro_z;
        let accel_x = self.accel_x;
        let accel_y = self.accel_y;
        let accel_z = self.accel_z;
        let battery_lvl_special = self.battery_lvl_special;
        let touch_packets_n = self.touch_packets_n;
        let current_touch = self.current_touch;
        let previous_touch = self.previous_touch;
        f.debug_struct("Ds4ReportExData")
            .field("thumb_lx", &thumb_lx)
            .field("thumb_ly", &thumb_ly)
            .field("thumb_rx", &thumb_rx)
            .field("thumb_ry", &thumb_ry)
            .field("buttons", &buttons)
            .field("special", &special)
            .field("trigger_left", &trigger_left)
            .field("trigger_right", &trigger_right)
            .field("timestamp", &timestamp)
            .field("battery_lvl", &battery_lvl)
            .field("gyro_x", &gyro_x)
            .field("gyro_y", &gyro_y)
            .field("gyro_z", &gyro_z)
            .field("accel_x", &accel_x)
            .field("accel_y", &accel_y)
            .field("accel_z", &accel_z)
            .field("battery_lvl_special", &battery_lvl_special)
            .field("touch_packets_n", &touch_packets_n)
            .field("current_touch", &current_touch)
            .field("previous_touch", &previous_touch)
            .finish_non_exhaustive()
    }
}

/// Extended DualShock 4 report for [`update_ex`](crate::TargetHandle::update_ex):
/// everything in [`Ds4Report`] plus gyro, accelerometer, and touchpad.
///
/// The public fields live on [`Ds4ReportExData`]. This type implements
/// [`Deref`] / [`DerefMut`] to that payload, so `report_ex.gyro_x = ...` works,
/// but rustdoc lists those fields on [`Ds4ReportExData`], not here. For raw bytes,
/// use [`as_bytes`](Self::as_bytes) / [`from_bytes`](Self::from_bytes).
///
/// For multi-frame touch gestures, prefer [`TouchpadTracker`] so tracking numbers
/// stay consistent.
///
/// With the `serde` feature, serializes as a flat map of the public fields
/// (reserved bytes skipped / zeroed on deserialize).
#[repr(C, packed)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Ds4ReportEx {
    #[cfg_attr(feature = "serde", serde(flatten))]
    data: Ds4ReportExData,
    #[cfg_attr(feature = "serde", serde(skip))]
    _reserved_tail: [u8; 3],
}

impl Ds4ReportEx {
    /// The typed payload (sticks, gyro, touch, ...). For the raw 63 bytes, see [`as_bytes`](Self::as_bytes).
    #[inline]
    pub const fn data(&self) -> &Ds4ReportExData {
        &self.data
    }

    /// Mutable access to the typed payload.
    #[inline]
    pub fn data_mut(&mut self) -> &mut Ds4ReportExData {
        &mut self.data
    }

    /// Raw 63-byte payload.
    #[inline]
    pub fn as_bytes(&self) -> &[u8; 63] {
        // SAFETY: `Ds4ReportEx` is `repr(C, packed)`, size 63, align 1, all-integer
        // fields, no niches and no interior padding beyond the explicit 3-byte tail.
        // Viewing it as `[u8; 63]` is layout-preserving.
        unsafe { &*core::ptr::from_ref(self).cast::<[u8; 63]>() }
    }

    /// Mutable raw 63-byte payload.
    #[inline]
    pub fn as_bytes_mut(&mut self) -> &mut [u8; 63] {
        // SAFETY: same layout argument as [`Self::as_bytes`].
        unsafe { &mut *core::ptr::from_mut(self).cast::<[u8; 63]>() }
    }

    /// Build a report from a raw 63-byte buffer.
    #[inline]
    pub const fn from_bytes(buf: [u8; 63]) -> Self {
        // SAFETY: `Ds4ReportEx` and `[u8; 63]` have identical size and alignment;
        // the type is a plain integer aggregate with no invalid bit patterns.
        // `Ds4Buttons` / `Ds4SpecialButton` are `repr(transparent)` over integers
        // with no niches, so every bit pattern is valid.
        unsafe { core::mem::transmute::<[u8; 63], Self>(buf) }
    }
}

impl Deref for Ds4ReportEx {
    type Target = Ds4ReportExData;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

impl DerefMut for Ds4ReportEx {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.data
    }
}

impl fmt::Debug for Ds4ReportEx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ds4ReportEx")
            .field("data", self.data())
            .finish_non_exhaustive()
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct Ds4SubmitReport {
    pub size: u32,
    pub serial_no: u32,
    pub report: Ds4Report,
}

// SAFETY: integers + `Ds4Report: WireZeroable`; trailing pad is zero-valid.
unsafe impl WireZeroable for Ds4SubmitReport {}

impl Ds4SubmitReport {
    /// Zeroed wire buffer (padding defined), then size/serial/report fields.
    ///
    /// Report is copied field-wise so a caller's uninit `Ds4Report` pad is not
    /// propagated. Sizes stay 20 (not packed) to match MSVC / ViGEm `Size`.
    pub(crate) fn new(serial_no: u32, report: &Ds4Report) -> Self {
        let mut s = zeroed_wire::<Self>();
        #[allow(clippy::cast_possible_truncation)]
        {
            s.size = core::mem::size_of::<Self>() as u32;
        }
        s.serial_no = serial_no;
        s.report.thumb_lx = report.thumb_lx;
        s.report.thumb_ly = report.thumb_ly;
        s.report.thumb_rx = report.thumb_rx;
        s.report.thumb_ry = report.thumb_ry;
        s.report.buttons = report.buttons;
        s.report.special = report.special;
        s.report.trigger_left = report.trigger_left;
        s.report.trigger_right = report.trigger_right;
        s
    }
}

impl Default for Ds4SubmitReport {
    fn default() -> Self {
        zeroed_wire()
    }
}

#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub(crate) struct Ds4SubmitReportEx {
    pub size: u32,
    pub serial_no: u32,
    pub report: Ds4ReportEx,
}

// Wire-format layout pins (must match ViGEmBus).
const _: () = {
    use core::mem::{align_of, offset_of, size_of};
    assert!(size_of::<Ds4Report>() == 10 && align_of::<Ds4Report>() == 2);
    assert!(offset_of!(Ds4Report, buttons) == 4);
    assert!(offset_of!(Ds4Report, trigger_right) == 8);
    assert!(size_of::<Ds4Buttons>() == 2 && align_of::<Ds4Buttons>() == 2);
    // `special` is Ds4SpecialButton, not raw u8 - same wire size/align.
    assert!(size_of::<Ds4SpecialButton>() == 1 && align_of::<Ds4SpecialButton>() == 1);
    assert!(size_of::<Ds4Touch>() == 9 && align_of::<Ds4Touch>() == 1);
    assert!(size_of::<Ds4ReportExData>() == 60 && align_of::<Ds4ReportExData>() == 1);
    assert!(offset_of!(Ds4ReportExData, timestamp) == 9);
    assert!(offset_of!(Ds4ReportExData, touch_packets_n) == 32);
    assert!(offset_of!(Ds4ReportExData, current_touch) == 33);
    // Struct form of former union: data at 0 + 3 reserved tail = 63.
    assert!(size_of::<Ds4ReportEx>() == 63 && align_of::<Ds4ReportEx>() == 1);
    assert!(offset_of!(Ds4ReportEx, data) == 0);
    // Deref reborrow of `current_touch` requires Touch align == container align (1).
    assert!(align_of::<Ds4Touch>() == 1);
    // Ds4SubmitReport: u32 + u32 + Ds4Report(10) + 2 pad to align 4 = 20 under repr(C)
    // (keep unpadded size - do not pack; zero padding at construction instead)
    assert!(size_of::<Ds4SubmitReport>() == 20);
    assert!(offset_of!(Ds4SubmitReport, report) == 8);
    // Ds4SubmitReportEx is packed: 4 + 4 + 63 = 71
    assert!(size_of::<Ds4SubmitReportEx>() == 71);
    assert!(align_of::<Ds4ReportExData>() == 1 && align_of::<Ds4Report>() == 2);
};

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::size_of;

    #[test]
    fn ds4_submit_report_new_zeros_padding() {
        let s = Ds4SubmitReport::new(1, &Ds4Report::default());
        // SAFETY: POD wire view; padding at 17 (report tail) and 18-19 (struct tail).
        let bytes = unsafe {
            core::slice::from_raw_parts((&raw const s).cast::<u8>(), size_of::<Ds4SubmitReport>())
        };
        assert_eq!(bytes[17], 0);
        assert_eq!(bytes[18], 0);
        assert_eq!(bytes[19], 0);
        assert_eq!(s.serial_no, 1);
        assert_eq!(s.size, 20);
    }

    #[test]
    fn set_dpad_and_set_report_do_not_clobber_timestamp() {
        let mut data = Ds4ReportExData {
            timestamp: 0xBEEF,
            ..Ds4ReportExData::default()
        };
        data.set_report(&Ds4Report::default());
        data.set_dpad(Ds4Dpad::East);
        // Packed padding must not overwrite timestamp.
        assert_eq!({ data.timestamp }, 0xBEEF);
        assert_eq!(data.buttons.dpad(), Ds4Dpad::East); // East == 2

        // `with` / `|=` / `set_buttons` keep existing D-pad (packed-bitfield aliasing).
        let buttons = Ds4Buttons::new()
            .with_dpad(Ds4Dpad::East)
            .with(Ds4ButtonFlags::CROSS);
        assert_eq!(buttons.dpad(), Ds4Dpad::East);
        assert!(buttons.contains(Ds4ButtonFlags::CROSS));

        let mut report = Ds4Report::default();
        report.set_dpad(Ds4Dpad::West);
        report.buttons |= Ds4ButtonFlags::TRIANGLE;
        assert_eq!(report.buttons.dpad(), Ds4Dpad::West);
        assert!(report.buttons.contains(Ds4ButtonFlags::TRIANGLE));
        report.set_buttons(Ds4ButtonFlags::CIRCLE);
        assert_eq!(report.buttons.dpad(), Ds4Dpad::West);
        assert!(report.buttons.contains(Ds4ButtonFlags::CIRCLE));
        assert!(!report.buttons.contains(Ds4ButtonFlags::TRIANGLE));
    }

    #[test]
    fn report_ex_from_bytes_round_trips() {
        let mut touch = Ds4Touch::default();
        touch.set_finger_1(TouchState::Down {
            tracking_num: 3,
            at: TouchPoint::new(100, 200).unwrap(),
        });
        let report = Ds4ReportEx {
            data: Ds4ReportExData {
                gyro_x: 0x1234,
                timestamp: 0xBEEF,
                touch_packets_n: 1,
                current_touch: touch,
                ..Ds4ReportExData::default()
            },
            _reserved_tail: [0; 3],
        };

        let bytes = *report.as_bytes();
        let restored = Ds4ReportEx::from_bytes(bytes);
        assert_eq!(restored, report);
        assert_eq!({ restored.gyro_x }, 0x1234);
        assert_eq!({ restored.timestamp }, 0xBEEF);
        assert!(restored.current_touch.is_down_1());
        assert_eq!(restored.current_touch.tracking_num_1(), 3);
        assert_eq!(restored.current_touch.coords_1(), (100, 200));
    }

    #[test]
    fn ds4_touch_default_fingers_are_up() {
        let t = Ds4Touch::default();
        assert!(!t.is_down_1());
        assert!(!t.is_down_2());
        assert!(matches!(t.finger_1(), TouchState::Up));
        assert!(matches!(t.finger_2(), TouchState::Up));

        let report = Ds4ReportEx::default();
        assert!(matches!(report.current_touch.finger_1(), TouchState::Up));
        assert!(matches!(report.current_touch.finger_2(), TouchState::Up));
        assert!(matches!(
            report.previous_touch[0].finger_1(),
            TouchState::Up
        ));
        assert!(matches!(
            report.previous_touch[1].finger_2(),
            TouchState::Up
        ));

        // Setting only finger 1 must not leave finger 2 as a phantom contact.
        let mut only_one = Ds4Touch::default();
        only_one.set_finger_1(TouchState::Down {
            tracking_num: 1,
            at: TouchPoint::new(10, 20).unwrap(),
        });
        assert!(only_one.is_down_1());
        assert!(!only_one.is_down_2());
    }

    #[test]
    fn ds4_buttons_flag_operators_preserve_dpad() {
        let mut buttons = Ds4Buttons::new()
            .with_dpad(Ds4Dpad::East)
            .with(Ds4ButtonFlags::CROSS | Ds4ButtonFlags::CIRCLE);
        buttons -= Ds4ButtonFlags::CIRCLE;
        assert_eq!(buttons.dpad(), Ds4Dpad::East);
        assert!(buttons.contains(Ds4ButtonFlags::CROSS));
        assert!(!buttons.contains(Ds4ButtonFlags::CIRCLE));

        buttons &= Ds4ButtonFlags::CROSS;
        assert_eq!(buttons.dpad(), Ds4Dpad::East);
        assert!(buttons.contains(Ds4ButtonFlags::CROSS));

        buttons |= Ds4ButtonFlags::TRIANGLE;
        assert_eq!(buttons.dpad(), Ds4Dpad::East);
        assert!(buttons.contains(Ds4ButtonFlags::CROSS | Ds4ButtonFlags::TRIANGLE));
    }

    #[test]
    fn ds4_buttons_debug_prints_flags_and_dpad() {
        assert_eq!(
            format!("{:?}", Ds4Buttons::new()),
            "Ds4Buttons(dpad: Neutral)"
        );

        let buttons = Ds4Buttons::new()
            .with_dpad(Ds4Dpad::South)
            .with(Ds4ButtonFlags::CROSS);
        let s = format!("{buttons:?}");
        assert_eq!(s, "Ds4Buttons(CROSS | dpad: South)");

        let both = Ds4Buttons::from(Ds4ButtonFlags::CROSS | Ds4ButtonFlags::OPTIONS)
            .with_dpad(Ds4Dpad::East);
        let s = format!("{both:?}");
        assert!(
            s.starts_with("Ds4Buttons(") && s.ends_with(" | dpad: East)"),
            "got {s}"
        );
        assert!(s.contains("CROSS") && s.contains("OPTIONS"), "got {s}");
    }

    #[test]
    fn touch_point_new_rejects_out_of_range() {
        assert_eq!(TouchPoint::new(1920, 0), None);
        assert_eq!(TouchPoint::new(0, 943), None);
        assert_eq!(
            TouchPoint::new(1919, 942),
            Some(TouchPoint::new_clamped(1919, 942))
        );
        let clamped = TouchPoint::new_clamped(5000, 5000);
        assert_eq!(
            (clamped.x(), clamped.y()),
            (TouchPoint::MAX_X, TouchPoint::MAX_Y)
        );
    }

    #[test]
    fn touchpad_tracker_holds_tracking_across_drag_and_reallocates_after_up() {
        let mut tracker = TouchpadTracker::new();
        let mut report = Ds4ReportEx::default();

        let start = TouchPoint::new(10, 20).unwrap();
        tracker.apply(&mut report, Some(start), None);
        let first_id = report.current_touch.tracking_num_1();
        let first_packet = report.current_touch.packet_counter;
        assert!(report.current_touch.is_down_1());
        assert!(!report.current_touch.is_down_2());
        assert_eq!(report.touch_packets_n, 1);

        // 10-frame drag: tracking_num must stay constant; packet_counter advances.
        for i in 1..=10 {
            let p = TouchPoint::new(10 + i, 20).unwrap();
            tracker.apply(&mut report, Some(p), None);
            assert_eq!(
                report.current_touch.tracking_num_1(),
                first_id,
                "tracking_num must be stable across a drag"
            );
            assert_eq!(
                report.current_touch.packet_counter,
                first_packet.wrapping_add(u8::try_from(i).expect("i in 1..=10"))
            );
            assert_eq!(report.current_touch.coords_1(), (10 + i, 20));
        }

        // Lift finger.
        tracker.apply(&mut report, None, None);
        assert!(!report.current_touch.is_down_1());
        assert!(matches!(report.current_touch.finger_1(), TouchState::Up));

        // New contact gets a different tracking_num.
        tracker.apply(&mut report, Some(TouchPoint::new(50, 60).unwrap()), None);
        assert!(report.current_touch.is_down_1());
        assert_ne!(report.current_touch.tracking_num_1(), first_id);
    }

    /// Exhaustive pack/unpack of 12-bit X/Y touchpad coordinates at edges.
    #[test]
    fn pack_unpack_coords_round_trip_full_x_and_y_edges() {
        for x in 0u16..=1919 {
            for y in [0u16, 1, 471, 942] {
                let p = TouchPoint::new(x, y).expect("in range");
                let mut touch = Ds4Touch::default();
                touch.set_finger_1(TouchState::Down {
                    tracking_num: 3,
                    at: p,
                });
                assert_eq!(touch.coords_1(), (x, y));
                assert!(touch.is_down_1());
                assert_eq!(touch.tracking_num_1(), 3);
            }
        }
        for y in 0u16..=942 {
            for x in [0u16, 1, 960, 1919] {
                let p = TouchPoint::new(x, y).expect("in range");
                let mut touch = Ds4Touch::default();
                touch.set_finger_1(TouchState::Down {
                    tracking_num: 7,
                    at: p,
                });
                assert_eq!(touch.coords_1(), (x, y));
            }
        }
    }

    #[cfg(feature = "serde")]
    #[test]
    fn report_and_notification_serde_round_trip() {
        let mut report = Ds4Report::default();
        report.buttons |= Ds4ButtonFlags::CROSS;
        report.set_dpad(Ds4Dpad::South);
        report.trigger_right = 200;
        let json = serde_json::to_string(&report).unwrap();
        let restored: Ds4Report = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, report);

        let ex = Ds4ReportEx {
            data: Ds4ReportExData {
                gyro_x: 42,
                timestamp: 0xBEEF,
                ..Ds4ReportExData::default()
            },
            _reserved_tail: [0; 3],
        };
        let json = serde_json::to_string(&ex).unwrap();
        // Flattened fields at the top level (not nested under "data").
        assert!(json.contains("\"gyro_x\":42"), "{json}");
        assert!(!json.contains("\"data\""), "{json}");
        let restored: Ds4ReportEx = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, ex);

        let n = Ds4Notification::new(1, 2, Ds4LightbarColor::new(255, 0, 128));
        let json = serde_json::to_string(&n).unwrap();
        let restored: Ds4Notification = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, n);

        let point = TouchPoint::new(100, 200).unwrap();
        let json = serde_json::to_string(&point).unwrap();
        let restored: TouchPoint = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, point);
        assert!(serde_json::from_str::<TouchPoint>(r#"{"x":1920,"y":0}"#).is_err());

        let buf = Ds4OutputBuffer::new([7; 64]);
        let json = serde_json::to_string(&buf).unwrap();
        assert!(json.contains("\"bytes\""), "{json}");
        assert!(!json.contains("\"buf\""), "{json}");
        let restored: Ds4OutputBuffer = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, buf);
    }
}

/// RGB lightbar color from the host ([`Ds4Notification::lightbar`]).
///
/// # Examples
///
/// ```
/// use vigem_rust::Ds4LightbarColor;
///
/// let color = Ds4LightbarColor::new(255, 0, 128);
/// assert_eq!(color.red, 255);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct Ds4LightbarColor {
    /// Red (`0..=255`).
    pub red: u8,
    /// Green (`0..=255`).
    pub green: u8,
    /// Blue (`0..=255`).
    pub blue: u8,
}

impl Ds4LightbarColor {
    /// Build an RGB color.
    ///
    /// This type is `#[non_exhaustive]`; use this instead of a struct literal.
    #[inline]
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self {
            red: r,
            green: g,
            blue: b,
        }
    }
}

/// Host feedback for a DualShock 4: rumble and lightbar color.
///
/// From [`register_notification`](crate::TargetHandle::register_notification).
///
/// # Example
///
/// ```no_run
/// # use vigem_rust::Client;
/// # use std::error::Error;
/// # fn main() -> Result<(), Box<dyn Error>> {
/// # let client = Client::connect()?;
/// # let ds4 = client.new_ds4_target().plug()?.wait_for_ready()?;
/// let receiver = ds4.register_notification()?;
/// if let Ok(n) = receiver.recv() {
///     println!("rumble {}/{}  lightbar {:?}", n.large_motor, n.small_motor, n.lightbar);
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct Ds4Notification {
    /// Large motor strength (`0..=255`).
    pub large_motor: u8,
    /// Small motor strength (`0..=255`).
    pub small_motor: u8,
    /// Lightbar color requested by the host.
    pub lightbar: Ds4LightbarColor,
}

impl Ds4Notification {
    /// Build a notification.
    ///
    /// This type is `#[non_exhaustive]`, so this is the only way to construct
    /// one outside this crate, useful for tests and replaying captures.
    #[inline]
    pub const fn new(large_motor: u8, small_motor: u8, lightbar: Ds4LightbarColor) -> Self {
        Self {
            large_motor,
            small_motor,
            lightbar,
        }
    }
}

/// Full 64-byte host output report for one DualShock 4.
///
/// From [`register_notification_raw_buffer`](crate::TargetHandle::register_notification_raw_buffer).
/// If you only need rumble and lightbar, use [`Ds4Notification`] instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Ds4OutputBuffer {
    /// Raw output report bytes.
    pub bytes: [u8; 64],
}

impl Ds4OutputBuffer {
    /// Wrap a 64-byte payload.
    ///
    /// This type is `#[non_exhaustive]`; use this instead of a struct literal.
    #[inline]
    pub const fn new(bytes: [u8; 64]) -> Self {
        Self { bytes }
    }
}

// serde only derives `[T; N]` up through 32; the 64-byte host buffer needs a
// tiny manual impl (still feature-gated). Round-trips as `{"bytes":[...64 bytes...]}`.
#[cfg(feature = "serde")]
impl Serialize for Ds4OutputBuffer {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        #[derive(Serialize)]
        struct Helper<'a> {
            bytes: &'a [u8],
        }
        Helper {
            bytes: self.bytes.as_slice(),
        }
        .serialize(serializer)
    }
}

#[cfg(feature = "serde")]
impl<'de> Deserialize<'de> for Ds4OutputBuffer {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Helper {
            bytes: Vec<u8>,
        }
        let helper = Helper::deserialize(deserializer)?;
        let len = helper.bytes.len();
        let bytes: [u8; 64] = helper.bytes.try_into().map_err(|_| {
            serde::de::Error::custom(format!(
                "Ds4OutputBuffer.bytes: expected 64 bytes, got {len}"
            ))
        })?;
        Ok(Self { bytes })
    }
}
