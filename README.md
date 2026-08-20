# vigem-rust

Create virtual Xbox 360 and DualShock 4 controllers on Windows via
[ViGEmBus](https://github.com/nefarius/ViGEmBus). Push input reports; get rumble, LED, and
lightbar requests back from the host.

[![Crates.io](https://img.shields.io/crates/v/vigem-rust)](https://crates.io/crates/vigem-rust)
[![Documentation](https://docs.rs/vigem-rust/badge.svg)](https://docs.rs/vigem-rust)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](https://github.com/arounre/vigem-rust#license)
[![CI](https://github.com/arounre/vigem-rust/actions/workflows/ci.yml/badge.svg)](https://github.com/arounre/vigem-rust/actions/workflows/ci.yml)

Windows only. Needs [ViGEmBus](https://vigem.org) at runtime. Rust 1.88+, edition 2024.

```toml
[dependencies]
vigem-rust = "0.2"
```

See [CHANGELOG.md](https://github.com/arounre/vigem-rust/blob/master/CHANGELOG.md) for 0.2 changes.

If `Client::connect` fails with `BusError::BusNotFound`, ViGEmBus is missing or
not started. Install it from [vigem.org](https://vigem.org). An incompatible
driver is `BusError::VersionMismatch`. The last official Nefarius releases are
1.21.442 and 1.22.0 (same driver; 1.22 is installer-only).

## Quick start

```rust,no_run
use std::thread;
use std::time::Duration;

use vigem_rust::{Client, ClientError, X360Button, X360Report};

fn main() -> Result<(), ClientError> {
    let client = Client::connect()?;
    let pad = client.new_x360_target().plug()?.wait_for_ready()?;

    // Host feedback (rumble, player LED). Dropping the last receiver stops the worker.
    let notifications = pad.register_notification()?;
    thread::spawn(move || {
        while let Ok(n) = notifications.recv() {
            println!("rumble L={} R={} led={}", n.large_motor, n.small_motor, n.led_number);
        }
    });

    let mut report = X360Report::default();
    report.buttons.insert(X360Button::A);
    pad.update(&report)?;

    thread::sleep(Duration::from_secs(5));
    Ok(()) // unplugs on drop
}
```

```bash
cargo run --example x360
```

Also [`examples/x360.rs`](https://github.com/arounre/vigem-rust/blob/master/examples/x360.rs),
[`examples/ds4.rs`](https://github.com/arounre/vigem-rust/blob/master/examples/ds4.rs), and
[`examples/ds4_touch.rs`](https://github.com/arounre/vigem-rust/blob/master/examples/ds4_touch.rs)
(touchpad + motion).

## Lifecycle

```text
Client::connect()
  -> new_x360_target() / new_ds4_target()   TargetBuilder
  -> plug()                                 TargetHandle<_, Pending>
  -> wait_for_ready()                       TargetHandle<_, Ready>
  -> update() / register_notification()
```

`update` and `register_notification` only exist after you promote a `Pending` handle.

- `wait_for_ready` sends neutral reports until a short streak succeeds (3 s default;
  `wait_for_ready_timeout` to change). It takes `&self`, so a timeout does not unplug.
  Keep the pending handle to retry, or call `assume_ready`.
- `assume_ready` skips the probe. Early reports may be no-ops.
- `Ready` is not permanent. When plugging several pads at once, a later `update` can
  still return `BusError::TargetNotReady`. Treat that as retryable.

## Sharing and teardown

`Client` and `TargetHandle` are `Send + Sync` and cheap to clone (`Arc`). A clone is the
same pad: concurrent `update`s serialize, so it does not increase throughput.

A pad leaves the bus when its **last** handle is dropped, or on `unplug()`.
`Client::unplug_all()` removes every pad and keeps the connection.
The last handle or last `Client` unplugs on that thread, waits on the driver,
and swallows unplug errors (it does not panic on a poisoned mutex). Last-client
drop unplugs leftover pads one by one — on the order of `max_targets` × a few
seconds if the bus is wedged. Prefer `unplug()` on a frame or async thread.

## Notifications

`NotificationReceiver` is `Send`; move it onto a thread. Every subscriber on the
same pad and kind shares one background worker and sees every event. The worker
stops when the last receiver is dropped, or when you unplug / drop the client.

The default buffer is bounded (capacity 4, drop-oldest) so a slow reader cannot stall
the worker. Use `register_notification_with(NotificationBuffer::Unbounded)` to keep
every event. On DS4, `register_notification_raw_buffer` is the full host output report.

## Limits

A client may own 16 pads by default (`DEFAULT_MAX_TARGETS`), 64 at most
(`MAX_TARGETS_LIMIT`). Set it with `ClientBuilder::max_targets`. That is this client's
cap, not a reservation of machine-wide serials.

Plugging searches up to `SERIAL_SCAN_LIMIT` (64) serials in a shared pool, starting at
a random offset.

- `SerialDiscovery::Auto` (default) looks up occupied serials after the first conflict
- `Eager` looks up first
- `Off` skips the lookup

The snapshot is advisory. Inspect occupancy with `Client::occupied_serials`.

## Errors

Match `ClientError` and the nested `BusError`:

- `BusNotFound` — driver missing or not started; install from [vigem.org](https://vigem.org)
- `VersionMismatch` — driver is not compatible; install a current Nefarius build
  (1.21.442 / 1.22.0)
- `AccessDenied` — fix permissions
- `TargetNotReady` — retry shortly
- `BusSerialsExhausted` — the serial window is taken; free pads elsewhere, or retry later
- `NoFreeSlot` — you hit your own quota (the driver is not contacted); unplug a pad or
  raise `max_targets`

## Features

| Feature | Effect |
|---|---|
| `x360` | Virtual Xbox 360 (default) |
| `ds4` | Virtual DualShock 4 (default) |
| `serde` | `Serialize` / `Deserialize` on reports and notifications |
| `windows-error` | Convert a status back into `windows::core::Error` |

At least one of `x360` / `ds4` is required. `serde` and `windows-error` are off by default.

## Development

```bash
cargo test --all-features        # unit tests, no driver needed
cargo clippy --all-features --all-targets -- -D warnings
```

These need ViGEmBus and create real virtual controllers, so they are excluded from CI:

```bash
cargo test -- --ignored --nocapture              # driver integration + stress
cargo test --test stress -- --ignored --nocapture
cargo bench --bench latency                      # per-update latency
```

Stress tests read `VIGEM_STRESS_SECONDS`, `VIGEM_STRESS_HZ`, `VIGEM_STRESS_CYCLES`,
`VIGEM_STRESS_PADS`, and `VIGEM_STRESS_CHURN`. Use `--nocapture` to see PASS/FAIL summaries.

Issues and PRs welcome. Run clippy and the tests before sending a change; add
`-- --ignored` if you touched an I/O path.

## License

Licensed under either of
[Apache-2.0](https://github.com/arounre/vigem-rust/blob/master/LICENSE-APACHE) or
[MIT](https://github.com/arounre/vigem-rust/blob/master/LICENSE-MIT), at your option.
