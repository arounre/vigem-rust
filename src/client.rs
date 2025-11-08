use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use thiserror::Error;

use crate::internal::bus::{Bus, BusError};
#[cfg(feature = "ds4")]
use crate::target::DualShock4;
#[cfg(feature = "x360")]
use crate::target::Xbox360;

use crate::target::{Target, TargetBuilder, TargetHandle};

/// Errors that can occur when interacting with the ViGEm client.
#[derive(Error, Debug)]
pub enum ClientError {
    #[error("Windows API Error: {0}")]
    WindowsAPIError(#[from] windows::core::Error),

    #[error("Bus error: {0}")]
    BusError(#[from] BusError),

    #[error("No more free slots available, consider increasing slots via the Client builder")]
    NoFreeSlot,

    #[error("Target with serial ID {0} is no longer connected or has been unplugged")]
    TargetDoesNotExist(u32),

    #[error("Client has been dropped, therefore any target operations can't be done.")]
    ClientNoLongerExists,
}

const DEFAULT_VIGEM_TARGETS_MAX: u32 = 16;

pub(crate) struct ClientInner {
    pub(crate) bus: Bus,
    pub(crate) targets: HashMap<u32, Target>,
    max_targets: u32,
}

/// The main entry point for interacting with the ViGEm bus driver.
///
/// A `Client` manages the connection to the driver and keeps track of all
/// virtual controllers created by it. When the `Client` is dropped, it will
/// automatically unplug all of its connected virtual controllers.
///
/// Use `Client::builder()` or `Client::connect()` to create a new client.
pub struct Client {
    inner: Arc<Mutex<ClientInner>>,
}

/// A builder for creating a `Client`.
pub struct ClientBuilder {
    max_targets: Option<u32>,
}

impl ClientBuilder {
    #[inline]
    /// Creates a new `ClientBuilder` with default settings.
    fn new() -> Self {
        Self { max_targets: None }
    }

    #[inline]
    /// Sets the maximum number of virtual targets this client can manage.
    ///
    /// The default is 16.
    pub fn max_targets(mut self, count: u32) -> Self {
        self.max_targets = Some(count);
        self
    }

    /// Connects to the ViGEm bus and creates a `Client`.
    pub fn connect(self) -> Result<Client, ClientError> {
        let max_targets = self.max_targets.unwrap_or(DEFAULT_VIGEM_TARGETS_MAX);
        let bus = Bus::connect()?;
        let inner = ClientInner {
            bus,
            targets: HashMap::new(),
            max_targets,
        };

        Ok(Client {
            inner: Arc::new(Mutex::new(inner)),
        })
    }
}

impl Client {
    #[inline]
    /// Create a builder to configure and connect a new client.
    ///
    /// This allows setting options like the maximum number of targets.
    ///
    /// # Example
    /// ```no_run
    /// use vigem_rust::client::Client;
    /// let client = Client::builder()
    ///     .max_targets(32) // Allow up to 32 controllers
    ///     .connect()
    ///     .unwrap();
    /// ```
    pub fn builder() -> ClientBuilder {
        ClientBuilder::new()
    }

    #[inline]
    /// Creates a new client with default options and connects to the ViGEm bus.
    ///
    /// This is a convenient shortcut for `Client::builder().connect()`.
    ///
    /// # Example
    /// ```no_run
    /// use vigem_rust::Client;
    ///
    /// let client = Client::connect().unwrap();
    /// ```
    pub fn connect() -> Result<Self, ClientError> {
        Self::builder().connect()
    }

    #[inline]
    #[cfg(feature = "x360")]
    /// Creates a builder for a new virtual Xbox 360 controller.
    ///
    /// The returned builder can be used to set properties like vendor and product IDs
    /// before plugging the controller into the bus.
    ///
    /// # Example
    /// ```no_run
    /// # use vigem_rust::Client;
    /// # let mut client = Client::connect().unwrap();
    /// let x360 = client.new_x360_target()
    ///     .with_vid(0xAAAA)
    ///     .with_pid(0xBBBB)
    ///     .plugin()
    ///     .unwrap();
    /// ```
    pub fn new_x360_target(&self) -> TargetBuilder<'_, Xbox360> {
        TargetBuilder::new(self)
    }

    #[inline]
    #[cfg(feature = "ds4")]
    /// Creates a builder for a new virtual DualShock 4 controller.
    ///
    /// The returned builder can be used to set properties like vendor and product IDs
    /// before plugging the controller into the bus.
    ///
    /// # Example
    /// ```no_run
    /// # use vigem_rust::Client;
    /// # let mut client = Client::connect().unwrap();
    /// let ds4 = client.new_ds4_target()
    ///     .with_vid(0xAAAA)
    ///     .with_pid(0xBBBB)
    ///     .plugin()
    ///     .unwrap();
    /// ```
    pub fn new_ds4_target(&self) -> TargetBuilder<'_, DualShock4> {
        TargetBuilder::new(self)
    }

    pub(crate) fn plugin_internal<T>(
        &self,
        target: Target,
    ) -> Result<TargetHandle<T>, ClientError> {
        let mut inner = self.inner.lock().expect("Client mutex was poisoned");
        let mut target = target;

        for serial_no in 1..=inner.max_targets {
            if !inner.targets.contains_key(&serial_no) && inner.bus.plug(&target, serial_no).is_ok()
            {
                target.serial_no = serial_no;
                inner.targets.insert(serial_no, target);

                return Ok(TargetHandle::new(
                    serial_no,
                    inner.bus.clone(),
                    Arc::downgrade(&self.inner),
                ));
            }
        }

        Err(ClientError::NoFreeSlot)
    }
}

impl Drop for ClientInner {
    fn drop(&mut self) {
        for target in self.targets.values() {
            let _ = self.bus.unplug(target.serial_no);
        }
        self.targets.clear();
    }
}
