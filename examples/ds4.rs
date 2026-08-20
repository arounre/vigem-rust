//! Circle the left stick on a virtual DualShock 4 and print rumble/lightbar.

use std::thread;
use std::time::Duration;
use vigem_rust::{Client, ClientError, Ds4Report};

fn main() -> Result<(), ClientError> {
    // Requires the ViGEmBus driver to be installed.
    let client = Client::connect()?;
    println!("Connected to ViGEm bus");

    // plug -> Pending; wait_for_ready -> Ready (needed before update / notifications).
    let ds4 = client.new_ds4_target().plug()?.wait_for_ready()?;
    println!("Plugged in virtual DualShock 4 controller");
    println!("Controller is ready. You can test it at https://hardwaretester.com/gamepad");

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

    // Sticks are u8 with center at 128 (not signed like X360). Report stays at
    // defaults except the stick circle: held face buttons / D-pad / triggers act
    // like a real pad in Steam and games.
    let mut report = Ds4Report::default();
    let mut angle: f64 = 0.0;

    loop {
        // Slow circle on the left stick so a gamepad tester shows motion.
        angle += 0.05;
        let (sin, cos) = angle.sin_cos();
        report.thumb_lx = (128.0 + sin * 127.0) as u8;
        report.thumb_ly = (128.0 + cos * 127.0) as u8;

        ds4.update(&report)?;

        thread::sleep(Duration::from_millis(16));
    }
}
