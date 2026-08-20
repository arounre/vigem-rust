// README's quick-start uses Xbox 360 APIs. Skip it as a doctest when `x360` is
// off (`cargo test --no-default-features --features ds4`); still show it on
// `cargo doc` (`cfg(doctest)` is unset there).
#![cfg_attr(
    any(not(doctest), feature = "x360"),
    doc = include_str!("../README.md")
)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(rustdoc::broken_intra_doc_links)]
#![warn(
    missing_docs,
    missing_debug_implementations,
    unnameable_types,
    unreachable_pub,
    clippy::undocumented_unsafe_blocks,
    clippy::missing_safety_doc,
    clippy::cast_possible_truncation,
    clippy::ptr_as_ptr,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::doc_markdown,
    clippy::doc_link_with_quotes
)]
#[cfg(not(windows))]
compile_error!("vigem-rust requires Windows: ViGEmBus is a Windows kernel-mode driver.");

// Windows-only so a non-Windows `--no-default-features` build still gets just
// the OS error above, not this as well.
#[cfg(all(windows, not(any(feature = "x360", feature = "ds4"))))]
compile_error!("vigem-rust requires at least one of the `x360` or `ds4` features");

// Keep modules off when neither controller feature is set so rustc reports only
// the compile_error above (not follow-on empty-enum / layout errors).
#[cfg(all(windows, any(feature = "x360", feature = "ds4")))]
mod internal;

/// Connect to ViGEmBus and create virtual controllers ([`Client`], [`ClientBuilder`]).
#[cfg(all(windows, any(feature = "x360", feature = "ds4")))]
pub mod client;
/// Input reports and host feedback (Xbox 360 / DualShock 4).
#[cfg(all(windows, any(feature = "x360", feature = "ds4")))]
pub mod controller;
/// Errors returned by the public APIs ([`ClientError`], [`BusError`], ...).
#[cfg(all(windows, any(feature = "x360", feature = "ds4")))]
pub mod error;
/// Virtual pads: plug, ready state, input, and host notifications.
#[cfg(all(windows, any(feature = "x360", feature = "ds4")))]
pub mod target;

/// Release notes from `CHANGELOG.md` (shown on docs.rs).
#[doc = include_str!("../CHANGELOG.md")]
#[cfg(doc)]
pub mod changelog {}

#[cfg(all(windows, any(feature = "x360", feature = "ds4")))]
#[doc(inline)]
pub use client::{
    Client, ClientBuilder, DEFAULT_MAX_TARGETS, MAX_TARGETS_LIMIT, SERIAL_SCAN_LIMIT,
    SerialDiscovery,
};
#[cfg(all(windows, any(feature = "x360", feature = "ds4")))]
#[doc(inline)]
pub use error::{
    BusError, ClientError, Operation, RecvError, RecvTimeoutError, TryRecvError, Win32Error,
};
#[cfg(all(windows, any(feature = "x360", feature = "ds4")))]
#[doc(inline)]
pub use target::{
    Device, NOTIFICATION_CHANNEL_CAPACITY, NotificationBuffer, NotificationReceiver, Pending,
    Ready, TargetBuilder, TargetHandle,
};

#[cfg(all(windows, feature = "ds4"))]
#[cfg_attr(docsrs, doc(cfg(feature = "ds4")))]
#[doc(inline)]
pub use target::DualShock4;
#[cfg(all(windows, feature = "x360"))]
#[cfg_attr(docsrs, doc(cfg(feature = "x360")))]
#[doc(inline)]
pub use target::Xbox360;

#[cfg(all(windows, feature = "x360"))]
#[cfg_attr(docsrs, doc(cfg(feature = "x360")))]
#[doc(inline)]
pub use controller::x360::{X360Button, X360Notification, X360Report};

#[cfg(all(windows, feature = "ds4"))]
#[cfg_attr(docsrs, doc(cfg(feature = "ds4")))]
#[doc(inline)]
pub use controller::ds4::{
    Ds4ButtonFlags, Ds4Buttons, Ds4Dpad, Ds4LightbarColor, Ds4Notification, Ds4OutputBuffer,
    Ds4Report, Ds4ReportEx, Ds4ReportExData, Ds4SpecialButton, Ds4Touch, TouchPoint, TouchState,
    TouchpadTracker,
};
