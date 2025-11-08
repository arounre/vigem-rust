use std::thread;
use std::time::Duration;
use vigem_rust::{Client, Ds4Button, Ds4Dpad, Ds4Report};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Connect to the ViGEm bus
    // This can fail if the ViGEm bus driver is not installed.
    let client = Client::connect()?;
    println!("Connected to ViGEm bus");

    // Create and plugin the virtual controller
    let ds4 = client.new_ds4_target().plugin()?;
    println!("Plugged in virtual DualShock 4 controller");

    // Wait for the controller to be ready
    ds4.wait_for_ready()?;
    println!("Controller is ready. You can test it at https://hardwaretester.com/gamepad");

    // Set up a notification listener in a separate thread
    // This allows us to react to feedback from the system, like rumble or LED changes.
    let notifications = ds4.register_notification()?;
    thread::spawn(move || {
        println!("Notification Thread Started. Waiting for feedback from the host...");
        while let Ok(Ok(notification)) = notifications.recv() {
            println!("Notification Thread Received feedback:");
            println!(
                "  - Rumble: Large Motor = {}, Small Motor = {}",
                notification.large_motor, notification.small_motor
            );
            println!(
                "  - Lightbar Color: R={}, G={}, B={}",
                notification.lightbar.red, notification.lightbar.green, notification.lightbar.blue
            );
        }
    });

    // Here, we'll send reports to the controller to simulate input.

    let mut report = Ds4Report::default();

    // Hold the right D-pad button
    report.set_dpad(Ds4Dpad::East);
    // Hold the Cross button
    report.buttons |= Ds4Button::CROSS.bits();
    // Fully press the right trigger
    report.trigger_r = 255;

    let mut angle: f64 = 0.0;

    loop {
        // Animate the right thumbstick in a circle
        let (sin, cos) = angle.sin_cos();

        // DS4 thumbsticks are 0-255, with 128 as the center.
        report.thumb_rx = (128.0 + sin * 127.0) as u8;
        report.thumb_ry = (128.0 + cos * 127.0) as u8;

        // Send the updated report to the controller
        ds4.update(&report)?;

        thread::sleep(Duration::from_millis(16));
        angle += 0.05;
    }
}
