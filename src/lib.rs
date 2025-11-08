#![doc = include_str!("../README.md")]

mod internal;

pub mod client;
pub mod controller;
pub mod target;

pub use client::Client;
pub use target::{TargetBuilder, TargetHandle};

#[cfg(feature = "x360")]
pub use controller::x360::{X360Button, X360Notification, X360Report};

#[cfg(feature = "ds4")]
pub use controller::ds4::{Ds4Button, Ds4Dpad, Ds4LightbarColor, Ds4Notification, Ds4Report};
