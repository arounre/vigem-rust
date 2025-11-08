use std::{
    marker::PhantomData,
    sync::{
        Arc, Mutex, Weak,
        mpsc::{self, Receiver, RecvTimeoutError},
    },
    time::Duration,
};

#[cfg(feature = "ds4")]
use crate::controller::ds4::{Ds4Notification, Ds4OutputBuffer, Ds4Report, Ds4ReportEx};
#[cfg(feature = "x360")]
use crate::controller::x360::{X360Notification, X360Report};

use crate::{
    client::{Client, ClientError, ClientInner},
    internal::bus::{Bus, BusError},
};

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TargetType {
    Xbox360 = 0,
    DualShock4 = 2,
}

impl TargetType {
    #[inline]
    // (vendor_id, product_id)
    pub fn get_identifiers(&self) -> (u16, u16) {
        match self {
            TargetType::Xbox360 => (0x045E, 0x028E),
            TargetType::DualShock4 => (0x054C, 0x05C4),
        }
    }
}

#[derive(Debug)]
pub(crate) struct Target {
    pub(crate) kind: TargetType,
    pub(crate) serial_no: u32,
    pub(crate) vendor_id: u16,
    pub(crate) product_id: u16,
}

#[cfg(feature = "x360")]
/// A marker type representing a virtual Xbox 360 controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Xbox360;

#[cfg(feature = "ds4")]
/// A marker type representing a virtual DualShock 4 controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DualShock4;

struct TargetHandleInner<T> {
    serial_no: u32,
    bus: Bus,
    client_inner: Weak<Mutex<ClientInner>>,
    _marker: PhantomData<T>,
}

impl<T> Drop for TargetHandleInner<T> {
    fn drop(&mut self) {
        if let Some(inner_arc) = self.client_inner.upgrade()
            && let Ok(mut inner) = inner_arc.lock()
            && inner.targets.remove(&self.serial_no).is_some()
        {
            let _ = self.bus.unplug(self.serial_no);
        }
    }
}

/// An opaque handle to a plugged-in virtual controller.
///
/// This handle is returned when a new target is successfully plugged into the bus.
/// It is used to identify the target for subsequent operations like updating its state,
/// listening for notifications, or removing it.
///
/// This handle uses reference counting (`Arc`). Cloning it is cheap and creates another
/// handle to the same virtual controller. The controller is only unplugged from the bus
/// when the **last** handle is dropped.
#[derive(Clone)]
pub struct TargetHandle<T> {
    inner: Arc<TargetHandleInner<T>>,
}

impl<T> TargetHandle<T> {
    pub(crate) fn new(serial_no: u32, bus: Bus, client_inner: Weak<Mutex<ClientInner>>) -> Self {
        Self {
            inner: Arc::new(TargetHandleInner {
                serial_no,
                bus,
                client_inner,
                _marker: PhantomData,
            }),
        }
    }

    fn with_client<F, R>(&self, f: F) -> Result<R, ClientError>
    where
        F: FnOnce(&ClientInner) -> Result<R, ClientError>,
    {
        if let Some(inner_arc) = self.inner.client_inner.upgrade() {
            let inner = inner_arc.lock().expect("Client mutex was poisoned");
            if !inner.targets.contains_key(&self.inner.serial_no) {
                return Err(ClientError::TargetDoesNotExist(self.inner.serial_no));
            }
            f(&inner)
        } else {
            Err(ClientError::ClientNoLongerExists)
        }
    }

    /// Checks if the virtual controller is still attached to the bus.
    ///
    /// This can return `false` if the controller was manually unplugged
    /// or if the client was dropped.
    pub fn is_attached(&self) -> Result<bool, ClientError> {
        self.with_client(|inner| Ok(inner.targets.contains_key(&self.inner.serial_no)))
    }

    /// Explicitly unplugs the virtual controller from the bus.
    ///
    /// After calling this, any further operations on this [`TargetHandle`] (and any
    /// of its clones) will fail. The controller is also automatically unplugged when
    /// the last [`TargetHandle`] is dropped.
    pub fn unplug(&self) -> Result<(), ClientError> {
        if let Some(inner_arc) = self.inner.client_inner.upgrade() {
            let mut inner = inner_arc.lock().expect("Client mutex was poisoned");
            if inner.targets.remove(&self.inner.serial_no).is_some() {
                let _ = self.inner.bus.unplug(self.inner.serial_no);
            }
            Ok(())
        } else {
            Err(ClientError::ClientNoLongerExists)
        }
    }
}

#[cfg(feature = "x360")]
impl TargetHandle<Xbox360> {
    /// Gets the user index of a virtual Xbox 360 controller.
    ///
    /// It doesn't seem like this method is reliable for getting the dynamic player index assigned by a game.
    /// It often returns `0` even after an index has been assigned.
    ///
    /// **To reliably get the player index, use `TargetHandle<X360>::register_notification` and
    /// check the `led_number` field of the received `X360Notification`.**
    pub fn get_user_index(&self) -> Result<u32, ClientError> {
        let index = self.inner.bus.get_x360_user_index(self.inner.serial_no)?;
        Ok(index)
    }

    /// Blocks until the virtual controller is fully enumerated and ready to receive updates.
    ///
    /// It is recommended to call this after plugging in a new controller if
    /// you want to immediately send a report to the controller.
    ///
    /// # Example
    /// ```no_run
    /// use vigem_rust::{Client, X360Report};
    /// let client = Client::connect().unwrap();
    /// let x360 = client.new_x360_target().plugin().unwrap();
    ///
    /// // Wait for the controller to be ready
    /// x360.wait_for_ready().unwrap();
    ///
    /// // Now it's safe to send updates
    /// x360.update(&X360Report::default()).unwrap();
    /// ```
    pub fn wait_for_ready(&self) -> Result<(), ClientError> {
        let (sender, receiver) = mpsc::channel();
        self.inner
            .bus
            .start_x360_notification_thread(self.inner.serial_no, sender)?;
        wait_for_notifications_internal(receiver, self.inner.serial_no)
    }

    /// Registers to receive notifications for this Xbox 360 target.
    ///
    /// This returns a `Receiver` that will yield [`X360Notification`]s from the bus,
    /// which contain information like rumble data and the controller's player LED index.
    ///
    /// # Important
    /// Calling this function spawns a dedicated background thread that lives as long as
    /// the `Receiver` does.
    ///
    /// # Example
    /// ```no_run
    /// # use vigem_rust::{Client, X360Notification};
    /// # let client = Client::connect().unwrap();
    /// # let x360 = client.new_x360_target().plugin().unwrap();
    /// let receiver = x360.register_notification().unwrap();
    ///
    /// // In a loop or another thread:
    /// if let Ok(Ok(notification)) = receiver.try_recv() {
    ///     println!("Player LED is now {}", notification.led_number);
    /// }
    /// ```
    pub fn register_notification(
        &self,
    ) -> Result<Receiver<Result<X360Notification, BusError>>, ClientError> {
        let (sender, receiver) = mpsc::channel();
        self.inner
            .bus
            .start_x360_notification_thread(self.inner.serial_no, sender)?;
        Ok(receiver)
    }

    /// Submits an input state report for this Xbox 360 target.
    ///
    /// This is the primary method for sending controller inputs to the system.
    /// The provided [`X360Report`] contains the state of all buttons, triggers,
    /// and thumbsticks.
    ///
    /// # Warning
    /// Calling this method immediately after plugging in the target will likely fail,
    /// as the system needs time to enumerate the device.
    ///
    /// To reliably send updates right after creation, you must first call [`wait_for_ready()`].
    ///
    /// # Example
    /// ```no_run
    /// # use vigem_rust::{Client, X360Report, X360Button};
    /// # use std::error::Error;
    /// # fn main() -> Result<(), Box<dyn Error>> {
    /// # let client = Client::connect()?;
    /// # let x360 = client.new_x360_target().plugin()?;
    /// # x360.wait_for_ready()?;
    /// let mut report = X360Report::default();
    /// report.buttons = X360Button::A | X360Button::START;
    /// report.thumb_lx = 16384; // Move left stick right
    ///
    /// x360.update(&report)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn update(&self, report: &X360Report) -> Result<(), ClientError> {
        self.inner.bus.update_x360(self.inner.serial_no, report)?;
        Ok(())
    }
}

#[cfg(feature = "ds4")]
impl TargetHandle<DualShock4> {
    /// Blocks until the virtual controller is fully enumerated and ready to receive updates.
    ///
    /// It is recommended to call this after plugging in a new controller if
    /// you want to immediately send a report to the controller.
    ///
    /// # Example
    /// ```no_run
    /// use vigem_rust::{Client, Ds4Report};
    /// let client = Client::connect().unwrap();
    /// let ds4 = client.new_ds4_target().plugin().unwrap();
    ///
    /// // Wait for the controller to be ready
    /// ds4.wait_for_ready().unwrap();
    ///
    /// // Now it's safe to send updates
    /// ds4.update(&Ds4Report::default()).unwrap();
    /// ```
    pub fn wait_for_ready(&self) -> Result<(), ClientError> {
        let (sender, receiver) = mpsc::channel();
        self.inner
            .bus
            .start_ds4_notification_thread(self.inner.serial_no, sender)?;
        wait_for_notifications_internal(receiver, self.inner.serial_no)
    }

    /// Registers to receive notifications for this DualShock 4 target.
    ///
    /// This returns a `Receiver` that will yield [`Ds4Notification`]s from the bus,
    /// which contain information like rumble data and lightbar color commands.
    ///
    /// # Important
    /// Calling this function spawns a dedicated background thread that lives as long as
    /// the `Receiver` does.
    ///
    /// # Example
    /// ```no_run
    /// # use vigem_rust::{Client, Ds4Notification};
    /// # let client = Client::connect().unwrap();
    /// # let ds4 = client.new_ds4_target().plugin().unwrap();
    /// let receiver = ds4.register_notification().unwrap();
    ///
    /// // In a loop or another thread:
    /// if let Ok(Ok(notification)) = receiver.try_recv() {
    ///     println!("Lightbar color changed to: {:?}", notification.lightbar);
    /// }
    /// ```
    pub fn register_notification(
        &self,
    ) -> Result<Receiver<Result<Ds4Notification, BusError>>, ClientError> {
        let (sender, receiver) = mpsc::channel();
        self.inner
            .bus
            .start_ds4_notification_thread(self.inner.serial_no, sender)?;
        Ok(receiver)
    }

    /// Subscribes to raw 64-byte output buffers for a DualShock 4 target.
    ///
    /// # Warning
    /// Unlike the `register_notification` method, this one gets the raw
    /// output buffer of all connected Dualshock 4 virtual controllers
    /// via a single thread (centralized system.)
    ///
    /// # Important
    /// Calling this function spawns a dedicated background thread that lives as long as
    /// the `Receiver` does.
    ///
    /// This is an advanced function for applications that need to parse the raw output
    /// report from the bus, which may contain more detailed information than the standard
    /// [`Ds4Notification`].
    pub fn register_notification_raw_buffer(
        &self,
    ) -> Result<Receiver<Result<Ds4OutputBuffer, BusError>>, ClientError> {
        let (sender, receiver) = mpsc::channel();
        self.inner
            .bus
            .start_ds4_output_thread(self.inner.serial_no, sender)?;
        Ok(receiver)
    }

    /// Submits a standard input state report for this DualShock 4 target.
    ///
    /// This method sends a [`Ds4Report`], which covers the state of all buttons,
    /// D-Pad, triggers, and thumbsticks. For advanced features like touchpad or
    /// motion sensor data, use [`update_ex`](Self::update_ex) instead.
    ///
    /// # Warning
    /// Calling this method immediately after plugging in the target will likely fail,
    /// as the system needs time to enumerate the device.
    ///
    /// To reliably send updates right after creation, you must first call [`wait_for_ready()`].
    ///
    /// # Example
    /// ```no_run
    /// # use vigem_rust::{Client, Ds4Report, Ds4Button, Ds4Dpad};
    /// # use std::error::Error;
    /// # fn main() -> Result<(), Box<dyn Error>> {
    /// # let client = Client::connect()?;
    /// # let ds4 = client.new_ds4_target().plugin()?;
    /// # ds4.wait_for_ready()?;
    /// let mut report = Ds4Report::default();
    /// report.buttons = Ds4Button::CROSS.bits();
    /// report.set_dpad(Ds4Dpad::South);
    /// report.trigger_r = 255;
    ///
    /// ds4.update(&report)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn update(&self, report: &Ds4Report) -> Result<(), ClientError> {
        self.inner.bus.update_ds4(self.inner.serial_no, report)?;
        Ok(())
    }

    /// Submits an extended input state report for this DualShock 4 target.
    ///
    /// This method is used for advanced scenarios that require simulating motion
    /// controls (gyroscope/accelerometer) and detailed touchpad activity. It sends
    /// a [`Ds4ReportEx`], which is a superset of the standard [`Ds4Report`].
    ///
    /// # Warning
    /// Calling this method immediately after plugging in the target will likely fail,
    /// as the system needs time to enumerate the device.
    ///
    /// To reliably send updates right after creation, you must first call [`wait_for_ready()`].
    ///
    /// # Example
    /// ```no_run
    /// # use vigem_rust::{Client, controller::ds4::Ds4ReportEx};
    /// # use std::error::Error;
    /// # fn main() -> Result<(), Box<dyn Error>> {
    /// # let client = Client::connect()?;
    /// # let ds4 = client.new_ds4_target().plugin()?;
    /// # ds4.wait_for_ready()?;
    /// let mut report_ex = Ds4ReportEx::default();
    ///
    ///
    /// // Set gyro data
    /// report_ex.gyro_x = 12345;
    ///
    /// // The standard report fields are also available
    /// report_ex.thumb_lx = 200;
    ///
    /// ds4.update_ex(&report_ex)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn update_ex(&self, report: &Ds4ReportEx) -> Result<(), ClientError> {
        self.inner.bus.update_ds4_ex(self.inner.serial_no, report)?;
        Ok(())
    }
}

/// A builder for creating and plugging in a new virtual target.
///
/// Obtain a [`TargetBuilder`] from [`Client::new_x360_target()`] or [`Client::new_ds4_target()`].
pub struct TargetBuilder<'a, T> {
    client: &'a Client,
    vid: Option<u16>,
    pid: Option<u16>,
    _marker: PhantomData<T>,
}

impl<'a, T> TargetBuilder<'a, T> {
    #[inline]
    pub(crate) fn new(client: &'a Client) -> Self {
        Self {
            client,
            vid: None,
            pid: None,
            _marker: PhantomData,
        }
    }

    #[inline]
    /// Sets a custom Vendor ID (VID) for this virtual device.
    ///
    /// If not set, the default VID for the controller type will be used.
    pub fn with_vid(mut self, vid: u16) -> Self {
        self.vid = Some(vid);
        self
    }

    #[inline]
    /// Sets a custom Product ID (PID) for this virtual device.
    ///
    /// If not set, the default PID for the controller type will be used.
    pub fn with_pid(mut self, pid: u16) -> Self {
        self.pid = Some(pid);
        self
    }
}

#[cfg(feature = "x360")]
impl<'a> TargetBuilder<'a, Xbox360> {
    /// Plugs the configured target into the ViGEm bus.
    ///
    /// **WARNING:** The virtual controller may not be immediately ready for input updates
    /// (e.g., `update()` calls) upon the return of this function. Windows and the ViGEm
    /// driver require time to fully enumerate the device.
    ///
    /// For reliable operation, especially when sending updates immediately after plugging
    /// in, it is highly recommended to call [`TargetHandle<T>::wait_for_ready`] and wait for
    /// it to return successfully before sending the first report.
    ///
    /// On success, this consumes the builder and returns a [`TargetHandle`] which can
    /// be used to control the virtual device.
    pub fn plugin(self) -> Result<TargetHandle<Xbox360>, ClientError> {
        let (default_vid, default_pid) = TargetType::Xbox360.get_identifiers();
        let target = Target {
            kind: TargetType::Xbox360,
            serial_no: 0, // Will be filled in by the client
            vendor_id: self.vid.unwrap_or(default_vid),
            product_id: self.pid.unwrap_or(default_pid),
        };
        self.client.plugin_internal(target)
    }
}

#[cfg(feature = "ds4")]
impl<'a> TargetBuilder<'a, DualShock4> {
    /// Plugs the configured target into the ViGEm bus.
    ///
    /// **WARNING:** The virtual controller may not be immediately ready for input updates
    /// (e.g., `update()` calls) upon the return of this function. Windows and the ViGEm
    /// driver require time to fully enumerate the device.
    ///
    /// For reliable operation, especially when sending updates immediately after plugging
    /// in, it is highly recommended to call [`TargetHandle<T>::wait_for_ready`] and wait for
    /// it to return successfully before sending the first report.
    ///
    /// On success, this consumes the builder and returns a [`TargetHandle`] which can
    /// be used to control the virtual device.
    pub fn plugin(self) -> Result<TargetHandle<DualShock4>, ClientError> {
        // Same here, using the concrete type.
        let (default_vid, default_pid) = TargetType::DualShock4.get_identifiers();
        let target = Target {
            kind: TargetType::DualShock4,
            serial_no: 0,
            vendor_id: self.vid.unwrap_or(default_vid),
            product_id: self.pid.unwrap_or(default_pid),
        };
        self.client.plugin_internal(target)
    }
}

// HELPER

/// Blocks until the controller is ready.
///
/// The readiness logic is as follows:
/// 1. Block until the first notification arrives from the host with a 500ms timeout.
/// 2. After the first notification, enter a state with a 250ms timeout.
/// 3. Each subsequent notification resets this timeout.
/// 4. If 250ms pass without any new notifications, the controller is considered "stable"
///    and ready, and the method returns.
///
/// This approach is used because the underlying `IOCTL_VIGEM_WAIT_DEVICE_READY` signal
/// from the driver doesn't seem to working properly. Waiting for a brief period of notification
/// silence after initial activity is a more robust heuristic for device readiness.
pub(crate) fn wait_for_notifications_internal<N>(
    receiver: Receiver<Result<N, BusError>>,
    serial_no: u32,
) -> Result<(), ClientError> {
    // We wait for the first notification. If it doesnt come within 500ms,
    // then chances are the device is ready for receiving updates.
    match receiver.recv_timeout(Duration::from_millis(500)) {
        Ok(Ok(_)) => {
            // First notification received. Now, wait for notifications to stabilize.
        }
        Ok(Err(bus_error)) => {
            return Err(bus_error.into());
        }
        Err(RecvTimeoutError::Timeout) => return Ok(()),
        Err(_) => {
            return Err(ClientError::TargetDoesNotExist(serial_no));
        }
    }

    loop {
        match receiver.recv_timeout(Duration::from_millis(250)) {
            Ok(Ok(_)) => {
                // Another notification arrived. Reset the timer by looping again.
                continue;
            }
            Ok(Err(bus_error)) => {
                return Err(bus_error.into());
            }
            Err(RecvTimeoutError::Timeout) => {
                // Timeout reached. No notifications for set timeout period. Device is (hopefully) ready!
                break;
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(ClientError::TargetDoesNotExist(serial_no));
            }
        }
    }

    Ok(())
}
