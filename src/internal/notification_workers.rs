#[cfg(feature = "ds4")]
use crate::controller::ds4::{Ds4LightbarColor, Ds4Notification, Ds4OutputBuffer};
#[cfg(feature = "x360")]
use crate::controller::x360::X360Notification;
use crate::error::Operation;
use crate::internal::ioctl::*;

pub(crate) trait NotificationWorker: Send + Sized + 'static {
    type Notification: Clone + Send + 'static;
    type Request: Default + Send + Copy + 'static;
    const IOCTL_CODE: u32;
    /// Named IOCTL used in `BusError` context.
    const OPERATION: Operation;
    /// Minimum bytes transferred before a completion is treated as valid.
    ///
    /// Impls use the full request size. Lower only after capturing a live
    /// driver's actual transfer length.
    const MIN_TRANSFER: u32;

    fn create_request(serial_no: u32) -> Self::Request;
    fn process_response(response: &Self::Request) -> Self::Notification;
}

#[cfg(feature = "x360")]
pub(crate) struct X360NotificationWorker;

#[cfg(feature = "x360")]
impl NotificationWorker for X360NotificationWorker {
    type Notification = X360Notification;
    type Request = XusbRequestNotification;

    const IOCTL_CODE: u32 = IOCTL_XUSB_REQUEST_NOTIFICATION;
    const OPERATION: Operation = Operation::XUSB_REQUEST_NOTIFICATION;
    const MIN_TRANSFER: u32 = wire_size_of::<XusbRequestNotification>();

    fn create_request(serial_no: u32) -> Self::Request {
        XusbRequestNotification::new(serial_no)
    }

    fn process_response(response: &Self::Request) -> Self::Notification {
        X360Notification::new(
            response.large_motor,
            response.small_motor,
            response.led_number,
        )
    }
}

#[cfg(feature = "ds4")]
pub(crate) struct Ds4NotificationWorker;

#[cfg(feature = "ds4")]
impl NotificationWorker for Ds4NotificationWorker {
    type Notification = Ds4Notification;
    type Request = Ds4RequestNotification;

    const IOCTL_CODE: u32 = IOCTL_DS4_REQUEST_NOTIFICATION;
    const OPERATION: Operation = Operation::DS4_REQUEST_NOTIFICATION;
    const MIN_TRANSFER: u32 = wire_size_of::<Ds4RequestNotification>();

    fn create_request(serial_no: u32) -> Self::Request {
        Ds4RequestNotification::new(serial_no)
    }

    fn process_response(response: &Self::Request) -> Self::Notification {
        Ds4Notification::new(
            response.report.large_motor,
            response.report.small_motor,
            Ds4LightbarColor::new(
                response.report.lightbar_color.red,
                response.report.lightbar_color.green,
                response.report.lightbar_color.blue,
            ),
        )
    }
}

#[cfg(feature = "ds4")]
pub(crate) struct Ds4OutputWorker;

#[cfg(feature = "ds4")]
impl NotificationWorker for Ds4OutputWorker {
    type Notification = Ds4OutputBuffer;
    type Request = Ds4AwaitOutput;

    const IOCTL_CODE: u32 = IOCTL_DS4_AWAIT_OUTPUT_AVAILABLE;
    const OPERATION: Operation = Operation::DS4_AWAIT_OUTPUT_AVAILABLE;
    const MIN_TRANSFER: u32 = wire_size_of::<Ds4AwaitOutput>();

    fn create_request(serial_no: u32) -> Self::Request {
        Ds4AwaitOutput {
            size: wire_size_of::<Self::Request>(),
            serial_no,
            ..Default::default()
        }
    }

    fn process_response(response: &Self::Request) -> Self::Notification {
        Ds4OutputBuffer::new(response.report.buffer)
    }
}
