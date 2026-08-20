//! Live ViGEmBus integration tests. Run with: `cargo test -- --ignored`
//!
//! Serialized via [`common::driver_lock`] (shared with stress tests).
//! Controller-specific tests are feature-gated on the function (same pattern as
//! `ds4_submit_paths_accepted_by_driver`) so `--features ds4` / `x360` only
//! registers the suites that can run.

#![cfg(windows)]

mod common;

#[cfg(feature = "x360")]
use std::time::{Duration, Instant};

use vigem_rust::Client;

use common::driver_lock;

/// After unplug, further `update` calls must return promptly (no hung overlapped wait)
/// and surface `TargetDoesNotExist`.
#[test]
#[ignore = "requires ViGEmBus driver"]
#[cfg(feature = "x360")]
fn update_after_unplug_errors() {
    use vigem_rust::{ClientError, X360Report};

    let _guard = driver_lock();
    let client = Client::connect().expect("ViGEmBus must be installed for this test");
    let x360 = client
        .new_x360_target()
        .plug()
        .expect("plugin should succeed")
        .assume_ready();
    x360.unplug().expect("unplug");

    let start = Instant::now();
    let err = x360
        .update(&X360Report::default())
        .expect_err("update after unplug must fail");
    assert!(
        start.elapsed() < Duration::from_millis(500),
        "update after unplug took {:?} - must not hang on overlapped wait",
        start.elapsed()
    );
    assert!(
        matches!(err, ClientError::TargetDoesNotExist { .. }),
        "expected TargetDoesNotExist, got {err:?}"
    );
    assert!(!x360.is_attached().expect("client still live"));
}

/// Repeated plug -> ready -> unplug stays under 1 s per cycle.
///
/// Also covers zero-timeout wait once plug-time wait-ready has already
/// completed (a single probe should succeed on a current driver).
#[test]
#[ignore = "requires ViGEmBus driver"]
#[cfg(feature = "x360")]
fn wait_for_ready_completes_under_1s() {
    let _guard = driver_lock();
    let client = Client::connect().expect("ViGEmBus must be installed for this test");

    for _ in 0..50 {
        let start = Instant::now();
        let x360 = client
            .new_x360_target()
            .plug()
            .expect("plugin should succeed")
            .wait_for_ready()
            .expect("wait_for_ready should succeed");
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "plug+wait_for_ready took too long: {:?}",
            start.elapsed()
        );
        x360.unplug().expect("unplug");
    }

    // Zero-timeout wait succeeds when plug already waited (modern drivers).
    let pending = client
        .new_x360_target()
        .plug()
        .expect("plugin should succeed");
    let ready = pending
        .wait_for_ready_timeout(Duration::ZERO)
        .expect("already-ready pad should pass a single zero-timeout probe");
    ready.unplug().expect("unplug");
}

/// After a bus-level `update` failure, a later `update` on the same handle works.
#[test]
#[ignore = "requires ViGEmBus driver"]
#[cfg(feature = "x360")]
fn cached_call_survives_a_failed_submit() {
    use vigem_rust::{ClientError, X360Report};

    let _guard = driver_lock();
    let client = Client::connect().expect("ViGEmBus must be installed for this test");
    // assume_ready: first submit may hit transient TargetNotReady.
    let pad = client
        .new_x360_target()
        .plug()
        .expect("plugin")
        .assume_ready();

    let mut saw_bus_failure = false;
    let mut saw_success = false;
    for _ in 0..100 {
        match pad.update(&X360Report::default()) {
            Ok(()) => {
                saw_success = true;
                break;
            }
            Err(ClientError::Bus(_)) => {
                saw_bus_failure = true;
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => panic!("unexpected update error before success: {e:?}"),
        }
    }
    assert!(
        saw_success,
        "update must eventually succeed on a live handle \
         (saw_bus_failure={saw_bus_failure})"
    );
    pad.update(&X360Report::default())
        .expect("second update must not be poisoned by a prior failure/reset");
    if saw_bus_failure {
        pad.update(&X360Report::default())
            .expect("third update after recovered bus failure");
    }
    pad.unplug().expect("unplug");
}

/// Unplug clears the target `alive` flag; the notification worker must exit so a
/// later plug that recycles the serial cannot feed the stale receiver.
///
/// With fan-out, **both** receivers on the same pad must observe `Disconnected`.
#[test]
#[ignore = "requires ViGEmBus driver"]
#[cfg(feature = "x360")]
fn notification_worker_stops_on_unplug() {
    use vigem_rust::RecvTimeoutError;

    let _guard = driver_lock();
    let client = Client::connect().expect("ViGEmBus must be installed for this test");
    let x360 = client
        .new_x360_target()
        .plug()
        .expect("plugin")
        .wait_for_ready()
        .expect("ready");
    let rx1 = x360.register_notification().expect("register_notification");
    let rx2 = x360.register_notification().expect("second subscriber");

    x360.unplug().expect("unplug");

    // Worker observes `alive == false` within one wait poll (~250 ms) and closes
    // the fan-out; drain any residual item then expect disconnect on every receiver.
    let deadline = Duration::from_secs(2);
    for (label, rx) in [("rx1", &rx1), ("rx2", &rx2)] {
        let start = Instant::now();
        loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Err(RecvTimeoutError::Disconnected | RecvTimeoutError::Bus(_)) => break,
                Err(RecvTimeoutError::Timeout) if start.elapsed() < deadline => continue,
                Err(RecvTimeoutError::Timeout) => {
                    panic!(
                        "{label}: notification worker did not disconnect within {deadline:?} after unplug"
                    )
                }
                Ok(_) if start.elapsed() < deadline => continue,
                Ok(other) => {
                    panic!("{label}: unexpected late notification after unplug: {other:?}")
                }
                Err(_) => break,
            }
        }
    }
}

/// Two receivers share one worker: second registration opens no new process
/// handles, and dropping one leaves the other connected.
#[test]
#[ignore = "requires ViGEmBus driver"]
#[cfg(feature = "x360")]
fn two_receivers_share_one_worker() {
    use vigem_rust::RecvTimeoutError;

    use common::process_handle_count;

    let _guard = driver_lock();
    let client = Client::connect().expect("ViGEmBus must be installed for this test");
    let x360 = client
        .new_x360_target()
        .plug()
        .expect("plugin")
        .wait_for_ready()
        .expect("ready");

    let before = process_handle_count();
    let rx1 = x360.register_notification().expect("first register");
    let after_first = process_handle_count();
    let rx2 = x360.register_notification().expect("second register");
    let after_second = process_handle_count();

    let first_delta = after_first.saturating_sub(before);
    let second_delta = after_second.saturating_sub(after_first);
    assert!(
        first_delta > 0,
        "first registration should open at least one handle (event); before={before}, after={after_first}"
    );
    // Second subscriber only joins the existing fan-out (no new worker / event).
    assert_eq!(
        second_delta, 0,
        "second registration must share the worker (no new process handles); \
         first_delta={first_delta}, second_delta={second_delta} \
         (before={before}, after_first={after_first}, after_second={after_second})"
    );

    // Drop first receiver; second must stay open (not disconnected immediately).
    drop(rx1);
    if let Err(RecvTimeoutError::Disconnected) = rx2.recv_timeout(Duration::from_millis(100)) {
        panic!("second receiver disconnected after first was dropped")
    }

    drop(rx2);
    x360.unplug().expect("unplug");
}

/// DS4 basic + extended submit and both notification workers must arm without
/// an immediate fatal wire-size / MIN_TRANSFER failure (closes the DS4 live gap).
#[test]
#[ignore = "requires ViGEmBus driver"]
#[cfg(feature = "ds4")]
fn ds4_submit_paths_accepted_by_driver() {
    use std::time::Duration;
    use vigem_rust::RecvTimeoutError;
    use vigem_rust::{Ds4Report, Ds4ReportEx, TouchPoint, TouchpadTracker};

    let _guard = driver_lock();
    let client = Client::connect().expect("ViGEmBus must be installed for this test");
    let ds4 = client
        .new_ds4_target()
        .plug()
        .expect("plugin")
        .wait_for_ready()
        .expect("ready");

    // Size == 20 path.
    ds4.update(&Ds4Report::default())
        .expect("basic DS4 submit (Size=20)");

    // Size == 71 path - the driver selects the extended format by Size, so this
    // is the only way to learn whether 71 is accepted.
    let mut ex = Ds4ReportEx::default();
    let mut tracker = TouchpadTracker::new();
    ex.gyro_x = 1234;
    tracker.apply(
        &mut ex,
        Some(TouchPoint::new(100, 200).expect("in range")),
        None,
    );
    ds4.update_ex(&ex).expect("extended DS4 submit (Size=71)");

    // Both notification kinds must at least arm without an immediate fatal error.
    // A MIN_TRANSFER / wire-size reject shows up as the worker burning retries and
    // closing the fan-out (Disconnected), which looks like "host sent no rumble."
    let rumble = ds4
        .register_notification()
        .expect("ds4 rumble/lightbar worker");
    let raw = ds4
        .register_notification_raw_buffer()
        .expect("ds4 output worker");
    for (label, disconnected) in [
        (
            "rumble",
            matches!(
                rumble.recv_timeout(Duration::from_millis(300)),
                Err(RecvTimeoutError::Disconnected | RecvTimeoutError::Bus(_))
            ),
        ),
        (
            "raw",
            matches!(
                raw.recv_timeout(Duration::from_millis(300)),
                Err(RecvTimeoutError::Disconnected | RecvTimeoutError::Bus(_))
            ),
        ),
    ] {
        assert!(
            !disconnected,
            "{label}: worker died immediately (check MIN_TRANSFER / wire size)"
        );
    }

    ds4.unplug().expect("unplug");
}

// ---------------------------------------------------------------------------
// Serial discovery (PnP occupancy)
// ---------------------------------------------------------------------------

/// Poll until `occupied_serials` is a superset of `expected`, or timeout.
#[cfg(feature = "x360")]
fn wait_occupied_superset(
    client: &Client,
    expected: &std::collections::BTreeSet<u32>,
    timeout: Duration,
) -> std::collections::BTreeSet<u32> {
    let deadline = Instant::now() + timeout;
    let mut last = std::collections::BTreeSet::new();
    loop {
        match client.occupied_serials() {
            Ok(set) => {
                last = set;
                if expected.iter().all(|s| last.contains(s)) {
                    return last;
                }
            }
            Err(e) => {
                eprintln!("occupied_serials transient error: {e}");
            }
        }
        if Instant::now() >= deadline {
            panic!(
                "occupied_serials did not become a superset of {expected:?} within {timeout:?}; last={last:?}"
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Poll until none of `freed` appear in `occupied_serials` (removal lag).
#[cfg(feature = "x360")]
fn wait_occupied_disjoint(
    client: &Client,
    freed: &std::collections::BTreeSet<u32>,
    timeout: Duration,
) -> std::collections::BTreeSet<u32> {
    let deadline = Instant::now() + timeout;
    let mut last = std::collections::BTreeSet::new();
    loop {
        match client.occupied_serials() {
            Ok(set) => {
                last = set;
                if freed.iter().all(|s| !last.contains(s)) {
                    return last;
                }
            }
            Err(e) => {
                eprintln!("occupied_serials transient error: {e}");
            }
        }
        if Instant::now() >= deadline {
            panic!(
                "occupied_serials still contains freed serials {freed:?} within {timeout:?}; last={last:?}"
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Occupancy appears for live pads (this client + peer), and **shrinks** after unplug.
///
/// A stub that always returns `1..=64` would pass a superset-only test; the
/// post-unplug disjoint check kills that failure mode.
#[test]
#[ignore = "requires ViGEmBus driver"]
#[cfg(feature = "x360")]
fn occupied_serials_sees_pads_and_shrinks_on_unplug() {
    let _guard = driver_lock();
    let a = Client::connect().expect("client A");
    let b = Client::connect().expect("client B");
    assert!(
        a.occupied_serials().is_ok(),
        "connect must capture a bus devnode id so discovery can run"
    );

    let mut pads_a = Vec::new();
    for _ in 0..3 {
        pads_a.push(a.new_x360_target().plug().expect("A plugin").assume_ready());
    }
    let live_a = a.live_serials();
    assert_eq!(live_a.len(), 3, "live_a={live_a:?}");
    eprintln!("A live={live_a:?}");

    let occupied_a = wait_occupied_superset(&a, &live_a, Duration::from_secs(3));
    let occupied_b = wait_occupied_superset(&b, &live_a, Duration::from_secs(3));
    eprintln!("A occupied={occupied_a:?}");
    eprintln!("B occupied={occupied_b:?} (must see A's pads)");
    for s in &live_a {
        assert!(
            occupied_b.contains(s),
            "B must see A's serial {s}; occupied_b={occupied_b:?}"
        );
    }

    for h in pads_a {
        h.unplug().expect("unplug A");
    }
    let after = wait_occupied_disjoint(&a, &live_a, Duration::from_secs(3));
    eprintln!("A occupied after unplug={after:?} (must be disjoint from {live_a:?})");
    for s in &live_a {
        assert!(
            !after.contains(s),
            "freed serial {s} still enumerated; snapshot never shrinks?"
        );
    }
}
