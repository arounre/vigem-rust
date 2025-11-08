use std::thread;
use std::time::Duration;
use vigem_rust::Client;
use vigem_rust::controller::ds4::Ds4ReportEx;

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

    // Main input loop for extended reports
    let mut report_ex = Ds4ReportEx::default();

    // Variables to animate the touch point
    let mut packet_counter: u8 = 0;
    let mut touch_x: i32 = 0;
    let mut direction: i32 = 12;

    loop {
        // Move the touch point back and forth horizontally.
        if touch_x <= 0 {
            direction = 12;
        }
        if touch_x >= 1919 {
            direction = -12;
        }
        touch_x += direction;

        report_ex.touch_packets_n = 1;

        // Get a mut reference to the touch data struct inside the report.
        let touch = &mut report_ex.current_touch;

        // This counter should increment for each new packet of touch data (not sure if necessary for functionality).
        touch.packet_counter = packet_counter;
        packet_counter = packet_counter.wrapping_add(1);

        touch.set_touch_1(true, 1, touch_x as u16, 471); // Finger 1 is down, centered vertically.
        touch.set_touch_2(false, 0, 0, 0); // Finger 2 is up (inactive).

        // Send the updated report to the controller
        ds4.update_ex(&report_ex)?;

        thread::sleep(Duration::from_millis(16));
    }
}
