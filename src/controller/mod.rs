//! - **`x360`** - Xbox 360 buttons, reports, rumble/LED feedback  
//! - **`ds4`** - DualShock 4 reports (standard + extended), touch helpers, lightbar feedback
//!
//! Most of these types are also re-exported at the crate root for shorter imports.

/// DualShock 4: input reports, touchpad helpers, rumble/lightbar notifications.
#[cfg(feature = "ds4")]
#[cfg_attr(docsrs, doc(cfg(feature = "ds4")))]
pub mod ds4;
/// Xbox 360: input report, buttons, rumble/LED notifications.
#[cfg(feature = "x360")]
#[cfg_attr(docsrs, doc(cfg(feature = "x360")))]
pub mod x360;
