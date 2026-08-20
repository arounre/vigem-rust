//! DualShock 4 extended report: bounce two fingers on the touchpad.

use std::thread;
use std::time::Duration;
use vigem_rust::{Client, ClientError, Ds4ReportEx, TouchPoint, TouchpadTracker};

const LOG_TOUCH: bool = true;
const LOG_TOUCH_EVERY_N: u32 = 10;

fn main() -> Result<(), ClientError> {
    // Requires the ViGEmBus driver to be installed.
    let client = Client::connect()?;
    println!("Connected to ViGEm bus");

    // plug -> Pending; wait_for_ready -> Ready (needed before update / notifications).
    let ds4 = client.new_ds4_target().plug()?.wait_for_ready()?;
    println!("Plugged in virtual DualShock 4 controller");
    println!("Controller is ready. You can test it at https://hardwaretester.com/gamepad");
    println!(
        "Touchpad only (no buttons). Steam may still map the pad to mouse. Close games if the cursor drifts."
    );

    // Host feedback (rumble, lightbar). Dropping the last receiver cancels the worker.
    let notifications = ds4.register_notification()?;
    thread::spawn(move || {
        println!("Notification thread started");
        while let Ok(n) = notifications.recv() {
            println!(
                "rumble L={} R={} lightbar=({},{},{})",
                n.large_motor, n.small_motor, n.lightbar.red, n.lightbar.green, n.lightbar.blue
            );
        }
    });

    // Ds4ReportEx adds touchpad (and more). TouchpadTracker owns packet_counter /
    // tracking_num so multi-frame drags stay coherent on the host.
    let mut report_ex = Ds4ReportEx::default();
    let mut tracker = TouchpadTracker::new();

    // Touchpad is 1920x943 pixels (x 0..=1919, y 0..=942). Most hosts only use finger 1.
    let mut touch_x: i32 = 0;
    let mut direction: i32 = 12;
    let mut frame: u32 = 0;

    loop {
        if touch_x <= 0 {
            direction = 12;
        }
        if touch_x >= 1919 {
            direction = -12;
        }
        touch_x += direction;

        let x1 = touch_x.clamp(0, 1919) as u16;
        let x2 = (1919 - touch_x).clamp(0, 1919) as u16;

        // new_clamped clamps coords into the pad; both fingers stay down.
        let finger_1 = Some(TouchPoint::new_clamped(x1, 300));
        let finger_2 = Some(TouchPoint::new_clamped(x2, 650));
        tracker.apply(&mut report_ex, finger_1, finger_2);

        if LOG_TOUCH && frame.is_multiple_of(LOG_TOUCH_EVERY_N) {
            let t = report_ex.current_touch;
            println!(
                "touch pkt={}  f1 down={} id={} {:?}  f2 down={} id={} {:?}",
                t.packet_counter,
                t.is_down_1(),
                t.tracking_num_1(),
                t.coords_1(),
                t.is_down_2(),
                t.tracking_num_2(),
                t.coords_2(),
            );
        }
        frame = frame.wrapping_add(1);

        ds4.update_ex(&report_ex)?;

        thread::sleep(Duration::from_millis(16));
    }
}
