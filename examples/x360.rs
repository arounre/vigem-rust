use std::thread;
use std::time::Duration;
use vigem_rust::{Client, X360Button, X360Report};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Connect to the ViGEm bus
    // This can fail if the ViGEm bus driver is not installed.
    let client = Client::connect()?;
    println!("Connected to ViGEm bus");

    // Create and plugin the virtual controller
    let x360 = client.new_x360_target().plugin()?;
    println!("Plugged in virtual Xbox 360 controller");

    // Wait for the controller to be ready
    // The virtual controller needs a moment to be recognized
    // by the system before it can receive updates.
    x360.wait_for_ready()?;
    println!("Controller is ready. You can test it at https://hardwaretester.com/gamepad");

    // Set up a notification listener in a separate thread
    // This allows us to react to feedback from the system, like rumble or LED changes.
    let notifications = x360.register_notification()?;
    thread::spawn(move || {
        println!("Notification Thread Started. Waiting for feedback...");
        while let Ok(Ok(notification)) = notifications.recv() {
            println!("Notification Thread Received feedback:");
            println!(
                "  - Rumble: Large Motor = {}, Small Motor = {}",
                notification.large_motor, notification.small_motor
            );
            println!("  - LED Number/Player Index: {}", notification.led_number);
        }
    });

    // Here, we'll send reports to the controller to simulate input.
    let mut report = X360Report::default();
    let mut angle: f64 = 0.0;
    let mut step = 0;

    loop {
        // Animate the left thumbstick in a circle
        angle += 0.1;
        let (sin, cos) = angle.sin_cos();
        report.thumb_lx = (sin * 32767.0) as i16;
        report.thumb_ly = (cos * 32767.0) as i16;

        // Alternate pressing A and B buttons
        if step % 2 == 0 {
            report.buttons = X360Button::A;
        } else {
            report.buttons = X360Button::B;
        }

        // Send the updated report to the controller
        x360.update(&report)?;

        thread::sleep(Duration::from_millis(16));
        step += 1;
    }
}
