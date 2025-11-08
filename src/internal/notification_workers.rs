#[cfg(feature = "ds4")]
use crate::controller::ds4::{Ds4LightbarColor, Ds4Notification, Ds4OutputBuffer};
#[cfg(feature = "x360")]
use crate::controller::x360::X360Notification;
use crate::internal::ioctl::*;

pub(crate) trait NotificationWorker: Send + Sized + 'static {
    type Notification: Send + 'static;
    type Request: Default + Send + Copy + 'static;
    const IOCTL_CODE: u32;

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

    fn create_request(serial_no: u32) -> Self::Request {
        XusbRequestNotification {
            size: std::mem::size_of::<Self::Request>() as u32,
            serial_no,
            large_motor: 0,
            small_motor: 0,
            led_number: 0,
        }
    }

    fn process_response(response: &Self::Request) -> Self::Notification {
        X360Notification {
            large_motor: response.large_motor,
            small_motor: response.small_motor,
            led_number: response.led_number,
        }
    }
}

#[cfg(feature = "ds4")]
pub(crate) struct Ds4NotificationWorker;

#[cfg(feature = "ds4")]
impl NotificationWorker for Ds4NotificationWorker {
    type Notification = Ds4Notification;
    type Request = Ds4RequestNotification;

    const IOCTL_CODE: u32 = IOCTL_DS4_REQUEST_NOTIFICATION;

    fn create_request(serial_no: u32) -> Self::Request {
        Ds4RequestNotification {
            size: std::mem::size_of::<Self::Request>() as u32,
            serial_no,
            ..Default::default()
        }
    }

    fn process_response(response: &Self::Request) -> Self::Notification {
        Ds4Notification {
            large_motor: response.report.large_motor,
            small_motor: response.report.small_motor,
            lightbar: Ds4LightbarColor {
                red: response.report.lightbar_color.red,
                green: response.report.lightbar_color.green,
                blue: response.report.lightbar_color.blue,
            },
        }
    }
}

#[cfg(feature = "ds4")]
pub(crate) struct Ds4OutputWorker;

#[cfg(feature = "ds4")]
impl NotificationWorker for Ds4OutputWorker {
    type Notification = Ds4OutputBuffer;
    type Request = Ds4AwaitOutput;

    const IOCTL_CODE: u32 = IOCTL_DS4_AWAIT_OUTPUT_AVAILABLE;

    fn create_request(serial_no: u32) -> Self::Request {
        Ds4AwaitOutput {
            size: std::mem::size_of::<Self::Request>() as u32,
            serial_no,
            ..Default::default()
        }
    }

    fn process_response(response: &Self::Request) -> Self::Notification {
        Ds4OutputBuffer {
            buf: response.report.buffer,
        }
    }
}
