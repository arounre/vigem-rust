//! Circle the left stick on a virtual Xbox 360 pad and print host rumble/LED.

use std::thread;
use std::time::Duration;
use vigem_rust::{Client, ClientError, X360Report};

fn main() -> Result<(), ClientError> {
    // Requires the ViGEmBus driver to be installed.
    let client = Client::connect()?;
    println!("Connected to ViGEm bus");

    // plug -> Pending; wait_for_ready -> Ready (needed before update / notifications).
    let x360 = client.new_x360_target().plug()?.wait_for_ready()?;
    println!("Plugged in virtual Xbox 360 controller");
    println!("Controller is ready. You can test it at https://hardwaretester.com/gamepad");

    // Host feedback (rumble, player LED). Dropping the last receiver cancels the worker.
    let notifications = x360.register_notification()?;
    thread::spawn(move || {
        println!("Notification thread started");
        while let Ok(n) = notifications.recv() {
            println!(
                "rumble L={} R={} led={}",
                n.large_motor, n.small_motor, n.led_number
            );
        }
    });

    // Sticks are signed i16 (+/-32767). Buttons stay clear: face/D-pad spam will
    // navigate Steam, games, and any focused XInput client.
    let mut report = X360Report::default();
    let mut angle: f64 = 0.0;

    loop {
        // Slow circle on the left stick so a gamepad tester shows motion.
        angle += 0.05;
        let (sin, cos) = angle.sin_cos();
        report.thumb_lx = (sin * 32767.0) as i16;
        report.thumb_ly = (cos * 32767.0) as i16;

        x360.update(&report)?;

        thread::sleep(Duration::from_millis(16));
    }
}
