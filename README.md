# vigem-rust

A safe and pure Rust interface for the ViGEmBus Windows driver.

[![Crates.io](https://img.shields.io/crates/v/vigem-rust)](https://crates.io/crates/vigem-rust)
[![Documentation](https://docs.rs/vigem-rust/badge.svg)](https://docs.rs/vigem-rust)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![CI](https://github.com/arounre/vigem-rust/actions/workflows/ci.yml/badge.svg)](https://github.com/arounre/vigem-rust/actions/workflows/ci.yml)

This library provides a high-level API to interact with the [ViGEmBus driver](https://github.com/ViGEm/ViGEmBus), allowing you to create and control virtual game controllers like the Xbox 360 and DualShock 4.

The library is designed to be thread-safe, enabling you to manage and update controllers from multiple threads concurrently. It features a type-safe interface for building input reports and for receiving output notifications such as rumble commands and LED state changes.

## Features
- Safely emulate Xbox 360 and Dualshock 4 controllers  using modular Rust features.
- RAII-based resource management and thread-safe by design.
- Receive rumble and LED feedback via standard Rust channels.
- Supports DS4 motion controls and detailed multi-touch touchpad data.

## Usage

Here is a simple example that creates a virtual Xbox 360 controller, waits for it to be ready, listens for notifications, and sends an input report.

```rust,ignore
use vigem_rust::{Client, X360Report, X360Button};
use std::thread;
use std::error::Error;
use std::time::Duration;

fn main() -> Result<(), Box<dyn Error>> {
    // Initialize the ViGEm client.
    let client = Client::connect()?;

    // Create a new virtual Xbox 360 controller.
    let x360 = client.new_x360_target().plugin()?;

    // Wait for the controller to be ready. This is crucial.
    x360.wait_for_ready()?;

    // Set up a thread to receive notifications (e.g., rumble, LED changes).
    let notification_receiver = x360.register_notification()?;
    thread::spawn(move || {
        // This loop will exit when the `x360` handle is dropped.
        while let Ok(Ok(notification)) = notification_receiver.recv() {
            println!("Received notification: {:?}", notification);
        }
    });

    // Create an input report and send it to the virtual controller.
    let mut report = X360Report::default();
    report.buttons.insert(X360Button::A);
    x360.update(&report)?;

    println!("Virtual controller created and 'A' button pressed.");

    // Controller will then be unplugged on drop.
    Ok(())
}
```

## Examples

More detailed examples can be found in the `examples` folder of the project repository.

## License

This project is licensed under either of

*   Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or
    <http://www.apache.org/licenses/LICENSE-2.0>)
*   MIT license ([LICENSE-MIT](LICENSE-MIT) or
    <http://opensource.org/licenses/MIT>)

at your option.

## Contributing

Contributions are welcome! Please feel free to submit a pull request or open an issue.