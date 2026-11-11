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

    // Zero-timeout wait succeeds when plug already waited (modern drivers):
    // either the driver confirmed readiness, or one user-index poll succeeds.
    let pending = client
        .new_x360_target()
        .plug()
        .expect("plugin should succeed");
    let ready = pending
        .wait_for_ready_timeout(Duration::ZERO)
        .expect("already-ready pad should pass a zero-timeout wait");
    ready.unplug().expect("unplug");
}

/// Resending the identical Xbox 360 report after `TargetNotReady` delivers it.
///
/// The driver accepts an Xbox 360 report equal to its cached last report
/// without touching the device. If it updated that cache before finding that
/// the host had no read pending, a resend of a dropped report would compare
/// equal, return `Ok`, and deliver nothing. This test proves it does not: the
/// resent report is read back through `XInputGetState`.
///
/// Misses are provoked two ways: the first report right after each plug
/// (`assume_ready`), and tight back-to-back bursts on the running pad. After a
/// miss the same report is resent every 1 ms until accepted, then nothing else
/// is sent while XInput is polled for that exact `(left_trigger, thumb_lx)`.
/// Reports accepted at the first try are checked the same way as a control
/// (the post-plug report, and the last report of each burst).
///
/// The pad's XInput slot is found by matching the first report on the slots
/// that were free before the plug, so at least one slot must be free (two for
/// the second half, where a blocker pad holds the lowest free slot). The
/// driver's `led_number` is printed next to it for comparison.
///
/// Asserted: every resend after a miss shows up through XInput (within 3 s;
/// the count within 500 ms is printed). If no miss was provoked, the run
/// prints that and asserts nothing.
///
/// The first report is sent before `register_notification`: subscribing first
/// gives the host time to start reading, and no miss happens.
///
/// 0.2.1, ViGEmBus 1.21.442, 25 plugs: 20 of 25 post-plug reports missed once;
/// all 20 resends showed in XInput within ~5 ms (5/5 first-try controls too).
/// The bursts (100 000 reports) never missed; 500/500 burst controls showed.
/// `led_number` equalled the XInput slot (0 without, 1 with the blocker).
#[test]
#[ignore = "requires ViGEmBus driver"]
#[cfg(feature = "x360")]
fn x360_resend_after_target_not_ready_delivers() {
    use vigem_rust::{BusError, ClientError, TargetHandle, X360Report, Xbox360};

    const PLUGS: u32 = 25;
    const BURSTS_PER_PLUG: u32 = 20;
    const BURST_LEN: u32 = 200;
    const VISIBLE: Duration = Duration::from_millis(500);
    const GIVE_UP: Duration = Duration::from_secs(3);

    #[repr(C)]
    #[derive(Default)]
    struct XInputGamepad {
        buttons: u16,
        left_trigger: u8,
        right_trigger: u8,
        thumb_lx: i16,
        thumb_ly: i16,
        thumb_rx: i16,
        thumb_ry: i16,
    }

    #[repr(C)]
    #[derive(Default)]
    struct XInputState {
        packet_number: u32,
        gamepad: XInputGamepad,
    }

    #[link(name = "xinput1_4", kind = "raw-dylib")]
    unsafe extern "system" {
        fn XInputGetState(user_index: u32, state: *mut XInputState) -> u32;
    }

    fn xinput(slot: u32) -> Option<XInputGamepad> {
        let mut state = XInputState::default();
        // SAFETY: `state` is a live XINPUT_STATE with the C layout.
        (unsafe { XInputGetState(slot, &mut state) } == 0).then_some(state.gamepad)
    }

    /// Non-zero and different from the previous sequence number's report.
    fn report_for(seq: u32) -> X360Report {
        X360Report {
            left_trigger: (seq % 255 + 1) as u8,
            thumb_lx: (seq % 32_000 + 1) as i16,
            ..X360Report::default()
        }
    }

    /// Slot and delay at which `report` shows on one of `slots`, polling up to `GIVE_UP`.
    fn seen_on(slots: &[u32], report: &X360Report) -> Option<(u32, Duration)> {
        let start = Instant::now();
        loop {
            for &slot in slots {
                if let Some(pad) = xinput(slot)
                    && pad.left_trigger == report.left_trigger
                    && pad.thumb_lx == report.thumb_lx
                {
                    return Some((slot, start.elapsed()));
                }
            }
            if start.elapsed() >= GIVE_UP {
                return None;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Sends `report`, resending the same one every 1 ms while the driver
    /// returns `TargetNotReady`. Returns how many misses came before `Ok`.
    fn send_until_ok(pad: &TargetHandle<Xbox360>, report: &X360Report) -> u32 {
        let mut misses = 0u32;
        loop {
            match pad.update(report) {
                Ok(()) => return misses,
                Err(ClientError::Bus(BusError::TargetNotReady { .. })) => {
                    misses += 1;
                    assert!(misses < 5_000, "report never accepted");
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(e) => panic!("unexpected update error: {e:?}"),
            }
        }
    }

    #[derive(Debug, Default)]
    struct Tally {
        checked: u32,
        within_500ms: u32,
        late: u32,
        never: u32,
        max_delay: Duration,
    }

    impl Tally {
        fn record(&mut self, seen: Option<(u32, Duration)>) {
            self.checked += 1;
            match seen {
                Some((_, delay)) if delay <= VISIBLE => self.within_500ms += 1,
                Some(_) => self.late += 1,
                None => self.never += 1,
            }
            if let Some((_, delay)) = seen {
                self.max_delay = self.max_delay.max(delay);
            }
        }
    }

    let _guard = driver_lock();
    let client = Client::connect().expect("ViGEmBus must be installed for this test");

    let mut seq = 0u32;
    let mut resent_after_plug = Tally::default();
    let mut resent_in_burst = Tally::default();
    let mut control_after_plug = Tally::default();
    let mut control_in_burst = Tally::default();
    let mut not_ready_total = 0u32;
    let mut led_and_slot: Vec<(Option<u8>, Option<u32>)> = Vec::new();

    // Second half: a blocker pad holds the lowest free slot, so the pad under
    // test gets a higher one and `led_number` can be compared with the slot.
    let mut blocker = None;
    for plug_no in 0..PLUGS {
        if plug_no == PLUGS / 2 {
            let free_before = (0..4).filter(|&slot| xinput(slot).is_none()).count();
            let pad = client
                .new_x360_target()
                .plug()
                .expect("blocker plugin")
                .assume_ready();
            send_until_ok(&pad, &X360Report::default());
            let start = Instant::now();
            while (0..4).filter(|&slot| xinput(slot).is_none()).count() == free_before
                && start.elapsed() < GIVE_UP
            {
                std::thread::sleep(Duration::from_millis(5));
            }
            blocker = Some(pad);
        }
        let free: Vec<u32> = (0..4).filter(|&slot| xinput(slot).is_none()).collect();
        assert!(
            !free.is_empty(),
            "all four XInput slots are taken; disconnect a controller"
        );

        let pad = client
            .new_x360_target()
            .plug()
            .expect("plugin should succeed")
            .assume_ready();

        seq += 1;
        let report = report_for(seq);
        let misses = send_until_ok(&pad, &report);
        not_ready_total += misses;
        let notifications = pad.register_notification().expect("register_notification");
        let seen = seen_on(&free, &report);
        if misses > 0 {
            resent_after_plug.record(seen);
        } else {
            control_after_plug.record(seen);
        }

        let led = notifications
            .recv_timeout(Duration::from_secs(2))
            .ok()
            .map(|n| n.led_number);
        let slot = seen.map(|(slot, _)| slot);
        led_and_slot.push((led, slot));

        if let Some(slot) = slot {
            for _ in 0..BURSTS_PER_PLUG {
                let mut last_first_try = None;
                for _ in 0..BURST_LEN {
                    seq += 1;
                    let report = report_for(seq);
                    let misses = send_until_ok(&pad, &report);
                    not_ready_total += misses;
                    if misses > 0 {
                        resent_in_burst.record(seen_on(&[slot], &report));
                        last_first_try = None;
                    } else {
                        last_first_try = Some(report);
                    }
                }
                if let Some(report) = last_first_try {
                    control_in_burst.record(seen_on(&[slot], &report));
                }
            }
        }

        drop(notifications);
        pad.unplug().expect("unplug");
        std::thread::sleep(Duration::from_millis(200));
    }
    if let Some(pad) = blocker {
        pad.unplug().expect("unplug blocker");
    }

    eprintln!(
        "x360 resend after TargetNotReady: {PLUGS} plugs, {seq} reports, \
         TargetNotReady returns={not_ready_total}\n  \
         resent after plug miss: {resent_after_plug:?}\n  \
         resent after burst miss: {resent_in_burst:?}\n  \
         control after plug (first-try Ok): {control_after_plug:?}\n  \
         control end of burst (first-try Ok): {control_in_burst:?}\n  \
         (led_number, XInput slot) per plug: {led_and_slot:?}"
    );

    let resent = resent_after_plug.checked + resent_in_burst.checked;
    if resent == 0 {
        eprintln!("no TargetNotReady provoked in this run; nothing to assert");
        return;
    }
    assert_eq!(
        resent_after_plug.never + resent_in_burst.never,
        0,
        "a report resent after TargetNotReady was accepted but never reached XInput"
    );
}

/// DS4 `wait_for_ready` on real plugs, then how well the host keeps up.
///
/// Phase 1 replays the `poll_until_ready` policy through public `update` (same
/// submit path as the probe): first miss retried at once, later misses sleep
/// 10 ms, done on the first accepted submit, 3 s deadline. It logs the outcome
/// sequence per plug (`O` = accepted, `x` = `TargetNotReady`). Asserted: no
/// plug times out.
///
/// Phase 2, on the same pad once ready, sends a back-to-back burst and records
/// how many submits succeed before the first miss. Each accepted submit uses up
/// one pending host read, so misses here mean the host fell behind.
/// Printed only; useful for choosing a retry back-off and for rechecking on other
/// drivers, several pads, or a loaded machine.
///
/// 0.2.1, ViGEmBus 1.21.442, one pad, 20 plugs: every plug `xxO` in ~10 ms;
/// 1000/1000 back-to-back submits accepted on every plug.
///
/// Run: `cargo test --features ds4 --test driver_required ds4_probe_sequence -- --ignored --nocapture`
#[test]
#[ignore = "requires ViGEmBus driver"]
#[cfg(feature = "ds4")]
fn ds4_probe_sequence_after_plug() {
    use std::time::{Duration, Instant};
    use vigem_rust::{BusError, ClientError, Ds4Report};

    const PLUGS: usize = 20;
    const POLL: Duration = Duration::from_millis(10); // READY_POLL_INTERVAL
    const TIMEOUT: Duration = Duration::from_secs(3); // DEFAULT_READY_TIMEOUT
    const BURST: usize = 1000;

    fn submit(pad: &vigem_rust::TargetHandle<vigem_rust::DualShock4>) -> bool {
        match pad.update(&Ds4Report::default()) {
            Ok(()) => true,
            Err(ClientError::Bus(BusError::TargetNotReady { .. })) => false,
            Err(e) => panic!("unexpected update error: {e:?}"),
        }
    }

    let _guard = driver_lock();
    let client = Client::connect().expect("ViGEmBus must be installed for this test");

    let mut sequences = Vec::with_capacity(PLUGS);
    let mut timed_out = 0usize;
    let mut elapsed_ms = Vec::with_capacity(PLUGS);
    let mut burst_first_miss = Vec::with_capacity(PLUGS);
    let mut burst_misses = Vec::with_capacity(PLUGS);
    let mut burst_patterns = Vec::with_capacity(PLUGS);

    for _ in 0..PLUGS {
        let pad = client
            .new_ds4_target()
            .plug()
            .expect("plugin should succeed")
            .assume_ready();

        // Phase 1: replay of poll_until_ready.
        let start = Instant::now();
        let mut seq = String::new();
        let mut misses = 0u32;
        let ready = loop {
            if submit(&pad) {
                seq.push('O');
                break true;
            } else {
                seq.push('x');
                misses += 1;
                if start.elapsed() >= TIMEOUT {
                    break false;
                }
                // First miss retried at once; later misses back off.
                if misses > 1 {
                    std::thread::sleep(POLL);
                }
            }
        };
        elapsed_ms.push(start.elapsed().as_secs_f64() * 1000.0);
        if !ready {
            timed_out += 1;
        }
        // Long sequences: keep head and tail.
        if seq.len() > 40 {
            seq = format!(
                "{}…{} (len {})",
                &seq[..20],
                &seq[seq.len() - 10..],
                seq.len()
            );
        }
        sequences.push(seq);

        // Phase 2: back-to-back burst on the now-ready pad.
        let mut pattern = String::with_capacity(BURST);
        for _ in 0..BURST {
            pattern.push(if submit(&pad) { 'O' } else { 'x' });
        }
        burst_first_miss.push(pattern.find('x').unwrap_or(BURST));
        burst_misses.push(pattern.matches('x').count());
        burst_patterns.push(pattern);

        pad.unplug().expect("unplug");
    }

    eprintln!("ds4 probe replay ({PLUGS} plugs, done on first accepted submit):");
    for (i, (s, ms)) in sequences.iter().zip(&elapsed_ms).enumerate() {
        eprintln!("  plug {i:2}: {ms:7.2} ms  {s}");
    }
    eprintln!("  timed out: {timed_out}/{PLUGS}");
    eprintln!("ds4 back-to-back burst of {BURST} after ready:");
    for (i, p) in burst_patterns.iter().enumerate() {
        // Only bursts with a miss; show where the misses fall.
        if let Some(first) = p.find('x') {
            let end = (first + 60).min(p.len());
            eprintln!("  plug {i:2}: from {first}: {}", &p[first..end]);
        }
    }
    eprintln!(
        "  successes before first miss: {burst_first_miss:?}\n  misses per burst: {burst_misses:?}"
    );

    assert_eq!(
        timed_out, 0,
        "probe replay hit ReadyTimeout on a plugged pad"
    );
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

/// DS4 raw output is one bus-wide listener per client, routed by serial.
///
/// Two raw-subscribed pads share one worker (the second pad's registration opens
/// no new process handles). Unplugging one pad disconnects only its receivers;
/// the other pad's receiver stays connected and the shared worker keeps running.
///
/// Does not provoke real host output (that needs a HID write from the test); the
/// routing itself is covered by unit tests in `internal::ds4_output_router`.
#[test]
#[ignore = "requires ViGEmBus driver"]
#[cfg(feature = "ds4")]
fn ds4_raw_output_shares_one_router_and_unplug_is_per_pad() {
    use std::time::{Duration, Instant};

    use vigem_rust::RecvTimeoutError;

    use common::process_handle_count;

    let _guard = driver_lock();
    let client = Client::connect().expect("ViGEmBus must be installed for this test");
    let plug = || {
        client
            .new_ds4_target()
            .plug()
            .expect("plugin")
            .wait_for_ready()
            .expect("ready")
    };
    let pad_a = plug();
    let pad_b = plug();
    // Third pad, never subscribed: its output must not reach anyone.
    let pad_c = plug();

    let before = process_handle_count();
    let rx_a = pad_a
        .register_notification_raw_buffer()
        .expect("register a");
    let after_a = process_handle_count();
    let rx_b = pad_b
        .register_notification_raw_buffer()
        .expect("register b");
    let after_b = process_handle_count();
    assert!(
        after_a > before,
        "first raw registration should start the router worker (event handle)"
    );
    assert_eq!(
        after_b, after_a,
        "second pad's raw registration must join the existing router, not start a worker"
    );

    pad_a.unplug().expect("unplug a");

    let deadline = Duration::from_secs(2);
    let start = Instant::now();
    loop {
        match rx_a.recv_timeout(Duration::from_millis(100)) {
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) | Ok(_) if start.elapsed() < deadline => continue,
            other => panic!("rx_a did not disconnect within {deadline:?} of unplug: {other:?}"),
        }
    }

    match rx_b.recv_timeout(Duration::from_millis(500)) {
        Err(RecvTimeoutError::Timeout) | Ok(_) => {}
        other => panic!("pad b's raw receiver must survive pad a's unplug, got {other:?}"),
    }

    drop(rx_b);
    pad_b.unplug().expect("unplug b");
    pad_c.unplug().expect("unplug c");
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
