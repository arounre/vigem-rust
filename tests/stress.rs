//! Long-running ViGEmBus stress tests (handle flatness / multi-pad chaos).
//!
//! Requires ViGEmBus. Not run in CI. Excluded from default `cargo test` via
//! `#[ignore]`. Print summaries need `--nocapture`:
//!
//! ```text
//! cargo test --test stress -- --ignored --nocapture
//! VIGEM_STRESS_SECONDS=60 cargo test --test stress -- --ignored --nocapture
//! ```
//!
//! Env knobs (optional):
//! - `VIGEM_STRESS_SECONDS` - soak / multi duration (default `10`)
//! - `VIGEM_STRESS_HZ` - submit rate (soak default `500`, multi default `250`)
//! - `VIGEM_STRESS_PADS` - multi submit workers (default `4`)
//! - `VIGEM_STRESS_CHURN` - multi concurrent churn cycles (`0` = off; default `50`)

#![cfg(all(windows, feature = "x360"))]

mod common;

use std::fmt::Debug;
use std::hint::black_box;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use vigem_rust::{
    BusError, Client, ClientError, NotificationBuffer, TargetHandle, TryRecvError, X360Button,
    X360Report, Xbox360,
};

use common::{
    Report, driver_lock, env_u32, format_bytes, handle_trend_ok, process_handle_count,
    process_memory, try_client,
};

// =============================================================================
// soak_update_handles
// =============================================================================

/// Long fixed-rate `update` soak: handles and commit charge must stay flat.
///
/// Handle endpoint/peak slacks are **fixed** (not scaled with frame count):
/// sustained submits reuse one cached overlapped call and must not open new
/// handles per frame.
///
/// Pass criteria:
/// - zero `update` errors
/// - handle delta end-start <= 2
/// - handle peak - start <= 4
/// - pagefile (commit) growth <= 8 MiB after a 1 s warm-up sample
/// - handle sample trend: last <= median(first half) + 2
#[test]
#[ignore = "requires ViGEmBus; long-running"]
fn soak_update_handles() {
    // Fixed: frame count does not create handles (one pad, cached OverlappedCall).
    const HANDLE_DELTA_SLACK: i64 = 2;
    const HANDLE_PEAK_SLACK: i64 = 4;
    const PAGEFILE_GROWTH_SLACK: usize = 8 * 1024 * 1024;

    let _guard = driver_lock();
    let seconds = env_u32("VIGEM_STRESS_SECONDS", 10).max(1);
    let hz = env_u32("VIGEM_STRESS_HZ", 500).max(1);

    let Some(client) = try_client() else {
        return;
    };
    let pad = match client
        .new_x360_target()
        .plug()
        .and_then(|p| p.wait_for_ready())
    {
        Ok(p) => p,
        Err(e) => {
            eprintln!("skip soak: plug failed ({e})");
            return;
        }
    };

    let report = X360Report::default();
    let frames = hz.saturating_mul(seconds);
    let frame = Duration::from_secs_f64(1.0 / f64::from(hz));
    let sample_every = hz; // ~1 sample / second

    let start_handles = process_handle_count();
    let (start_ws, start_pf) = process_memory();
    let t0 = Instant::now();

    let mut max_handles = start_handles;
    let mut errors = 0u64;
    let mut ok = 0u64;
    let mut handle_samples = Vec::with_capacity(seconds as usize + 2);
    handle_samples.push(start_handles);

    // Warm-up: 1 s of submits so the first memory sample is post-JIT/cache.
    let warmup_frames = hz.min(frames);
    for i in 0..warmup_frames {
        match pad.update(black_box(&report)) {
            Ok(()) => ok += 1,
            Err(_) => errors += 1,
        }
        let target = t0 + frame * (i + 1);
        let now = Instant::now();
        if target > now {
            thread::sleep(target - now);
        }
    }

    let post_warmup_handles = process_handle_count();
    let (post_warmup_ws, post_warmup_pf) = process_memory();
    max_handles = max_handles.max(post_warmup_handles);

    for i in warmup_frames..frames {
        match pad.update(black_box(&report)) {
            Ok(()) => ok += 1,
            Err(_) => errors += 1,
        }
        if (i + 1) % sample_every == 0 {
            let h = process_handle_count();
            max_handles = max_handles.max(h);
            handle_samples.push(h);
        }
        let target = t0 + frame * (i + 1);
        let now = Instant::now();
        if target > now {
            thread::sleep(target - now);
        }
    }

    let end_handles = process_handle_count();
    let (end_ws, end_pf) = process_memory();
    max_handles = max_handles.max(end_handles);
    handle_samples.push(end_handles);
    let elapsed = t0.elapsed();

    let handle_delta = i64::from(end_handles) - i64::from(start_handles);
    let handle_peak_delta = i64::from(max_handles) - i64::from(start_handles);
    let pf_growth = end_pf.saturating_sub(post_warmup_pf);
    let ws_growth = end_ws.saturating_sub(post_warmup_ws);

    let pass_errors = errors == 0;
    let pass_handles = handle_delta <= HANDLE_DELTA_SLACK;
    let pass_peak = handle_peak_delta <= HANDLE_PEAK_SLACK;
    let pass_memory = pf_growth <= PAGEFILE_GROWTH_SLACK;
    let pass_trend = handle_trend_ok(&handle_samples, HANDLE_DELTA_SLACK);
    let pass = pass_errors && pass_handles && pass_peak && pass_memory && pass_trend;

    Report::new("soak_update_handles")
        .line(
            "config:",
            format!("{hz} Hz x {seconds}s  (frames={frames})"),
        )
        .line(
            "env:",
            "VIGEM_STRESS_SECONDS / VIGEM_STRESS_HZ (defaults 10 / 500)",
        )
        .line("elapsed:", format!("{elapsed:?}"))
        .line("updates ok:", ok)
        .line("updates err:", errors)
        .line("handles start:", start_handles)
        .line("handles warm:", post_warmup_handles)
        .line(
            "handles end:",
            format!("{end_handles}  (delta {handle_delta:+})"),
        )
        .line(
            "handles max:",
            format!("{max_handles}  (peak delta {handle_peak_delta:+})"),
        )
        .line("handles/sec:", format!("{handle_samples:?}"))
        .line(
            "working set:",
            format!(
                "start {} -> warm {} -> end {}  (warm->end {:+})",
                format_bytes(start_ws),
                format_bytes(post_warmup_ws),
                format_bytes(end_ws),
                ws_growth as i64
            ),
        )
        .line(
            "pagefile:",
            format!(
                "start {} -> warm {} -> end {}  (warm->end {:+})",
                format_bytes(start_pf),
                format_bytes(post_warmup_pf),
                format_bytes(end_pf),
                pf_growth as i64
            ),
        )
        .line(
            "limits:",
            format!(
                "errors=0, handle delta<={HANDLE_DELTA_SLACK} (fixed; frames open no handles), peak delta<={HANDLE_PEAK_SLACK}, pagefile growth<={}, trend slack={HANDLE_DELTA_SLACK}",
                format_bytes(PAGEFILE_GROWTH_SLACK)
            ),
        )
        .line("result:", if pass { "PASS" } else { "FAIL" })
        .print();

    drop(pad);

    assert!(
        pass,
        "soak FAILED (errors={errors}, handle_delta={handle_delta}, \
         peak_delta={handle_peak_delta}, pagefile_growth={}, trend_ok={pass_trend})",
        format_bytes(pf_growth)
    );
}

// =============================================================================
// multi_pad_chaos
// =============================================================================

/// Cap printed error lines so a death spiral does not flood the console.
const MAX_LOGGED_ERRORS: usize = 64;

/// Brief post-ready blips under multi-plug load (see test docs).
const READY_RETRY_ATTEMPTS: u32 = 16;
const READY_RETRY_SLEEP: Duration = Duration::from_millis(2);

struct Counters {
    update_ok: AtomicU64,
    update_err: AtomicU64,
    /// Recovered `TargetNotReady` (not a hard failure).
    not_ready_retries: AtomicU64,
    churn_ok: AtomicU64,
    churn_err: AtomicU64,
    notif_ok: AtomicU64,
    notif_err: AtomicU64,
    /// Human-readable samples of failures (capped).
    error_log: Mutex<Vec<String>>,
    /// Errors past the log cap (still counted in the atomics).
    errors_suppressed: AtomicU64,
}

impl Counters {
    fn new() -> Self {
        Self {
            update_ok: AtomicU64::new(0),
            update_err: AtomicU64::new(0),
            not_ready_retries: AtomicU64::new(0),
            churn_ok: AtomicU64::new(0),
            churn_err: AtomicU64::new(0),
            notif_ok: AtomicU64::new(0),
            notif_err: AtomicU64::new(0),
            error_log: Mutex::new(Vec::new()),
            errors_suppressed: AtomicU64::new(0),
        }
    }

    /// Record a failure: bump `counter`, keep a Display+Debug sample in the log.
    fn log_err(&self, counter: &AtomicU64, where_: &str, err: &(impl std::fmt::Display + Debug)) {
        counter.fetch_add(1, Ordering::Relaxed);
        let line = format!("{where_}: {err}  ({err:?})");
        let mut log = self.error_log.lock().unwrap_or_else(|e| e.into_inner());
        if log.len() < MAX_LOGGED_ERRORS {
            // Also print immediately so failures show up before the summary.
            eprintln!("[multi] {line}");
            log.push(line);
        } else {
            self.errors_suppressed.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn is_target_not_ready(err: &ClientError) -> bool {
    matches!(err, ClientError::Bus(BusError::TargetNotReady { .. }))
}

/// `update` with short retries on transient `TargetNotReady` (multi-plug race).
fn update_resilient(
    pad: &TargetHandle<Xbox360>,
    report: &X360Report,
    counters: &Counters,
) -> Result<(), ClientError> {
    let mut attempts = 0u32;
    loop {
        match pad.update(black_box(report)) {
            Ok(()) => return Ok(()),
            Err(e) if is_target_not_ready(&e) && attempts + 1 < READY_RETRY_ATTEMPTS => {
                attempts += 1;
                counters.not_ready_retries.fetch_add(1, Ordering::Relaxed);
                thread::sleep(READY_RETRY_SLEEP);
            }
            Err(e) => return Err(e),
        }
    }
}

/// Multi-pad / multi-thread chaos stress.
///
/// Shared `Client` (cloned into workers), N submit workers, one plug/unplug
/// churner, and one notification drain on a dedicated pad - all concurrent.
///
/// Pass criteria:
/// - worker + churn hard failures == 0 (after TargetNotReady retries)
/// - after teardown, handles <= baseline + 4
/// - live peak handle growth <= baseline + 4 + 2xpads + 16
/// - pagefile growth after warm sample <= 16 MiB
///
/// Note: under concurrent multi-plug, ViGEm can return `TargetNotReady` on the
/// first submit even right after `wait_for_ready`. Updates retry that briefly;
/// only persistent failures count against PASS.
#[test]
#[ignore = "requires ViGEmBus; long-running"]
fn multi_pad_chaos() {
    const HANDLE_END_SLACK: i64 = 4;
    const PAGEFILE_GROWTH_SLACK: usize = 16 * 1024 * 1024;

    let _guard = driver_lock();
    let pads = env_u32("VIGEM_STRESS_PADS", 4).clamp(1, 16);
    let seconds = env_u32("VIGEM_STRESS_SECONDS", 10).max(1);
    let hz = env_u32("VIGEM_STRESS_HZ", 250).max(1);
    let churn_cycles = env_u32("VIGEM_STRESS_CHURN", 50);

    // Workers + one churn slot + headroom for retries.
    let max_targets = (pads + 2).clamp(4, 64);

    let client = match Client::builder().max_targets(max_targets).connect() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("skip multi: no driver ({e})");
            return;
        }
    };

    let baseline_handles = process_handle_count();
    let (baseline_ws, baseline_pf) = process_memory();
    let counters = Arc::new(Counters::new());
    let stop = Arc::new(AtomicBool::new(false));

    let t0 = Instant::now();
    let duration = Duration::from_secs(u64::from(seconds));

    // --- submit workers ------------------------------------------------------
    let mut workers = Vec::with_capacity(pads as usize);
    for id in 0..pads {
        let client = client.clone();
        let counters = Arc::clone(&counters);
        let stop = Arc::clone(&stop);
        workers.push(thread::spawn(move || {
            // Stagger plug so we don't all hit the bus in the same tick.
            thread::sleep(Duration::from_millis(u64::from(id) * 7));

            let pad = match client
                .new_x360_target()
                .plug()
                .and_then(|p| p.wait_for_ready())
            {
                Ok(p) => p,
                Err(e) => {
                    counters.log_err(
                        &counters.update_err,
                        &format!("worker {id} plugin/ready"),
                        &e,
                    );
                    return;
                }
            };
            let serial = pad.serial_no();

            let frame = Duration::from_secs_f64(1.0 / f64::from(hz));
            // Cheap LCG jitter so threads don't lock-step forever.
            let mut rng = 0xA3C5u32.wrapping_mul(id.wrapping_add(1));
            let mut report = X360Report::default();
            let mut tick = 0u32;
            let start = Instant::now();

            while !stop.load(Ordering::Relaxed) && start.elapsed() < duration {
                tick = tick.wrapping_add(1);
                // Flip a button every few frames so reports aren't identical.
                if tick.is_multiple_of(8) {
                    report.buttons.toggle(X360Button::A);
                }
                report.thumb_lx = (tick as i16).wrapping_mul(17).wrapping_add(id as i16);

                match update_resilient(&pad, &report, &counters) {
                    Ok(()) => {
                        counters.update_ok.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        counters.log_err(
                            &counters.update_err,
                            &format!(
                                "worker {id} serial={serial} update tick={tick} elapsed={:?} \
                                 (after {READY_RETRY_ATTEMPTS} tries if TargetNotReady)",
                                start.elapsed()
                            ),
                            &e,
                        );
                    }
                }

                rng = rng.wrapping_mul(1664525).wrapping_add(1013904223);
                let jitter_us = (rng % 800) as u64; // 0..800 us
                let target = start + frame * tick;
                let now = Instant::now();
                if target > now {
                    thread::sleep(target - now + Duration::from_micros(jitter_us));
                } else {
                    // Behind schedule: still inject a tiny pause so we yield.
                    thread::sleep(Duration::from_micros(jitter_us.min(200)));
                }
            }

            drop(pad);
        }));
    }

    // --- notification drain on a dedicated pad -------------------------------
    let notif_join = {
        let client = client.clone();
        let counters = Arc::clone(&counters);
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            let pad = match client
                .new_x360_target()
                .plug()
                .and_then(|p| p.wait_for_ready())
            {
                Ok(p) => p,
                Err(e) => {
                    counters.log_err(&counters.notif_err, "notif plugin/ready", &e);
                    return;
                }
            };
            let serial = pad.serial_no();

            let rx =
                match pad.register_notification_with(NotificationBuffer::Bounded { capacity: 8 }) {
                    Ok(rx) => rx,
                    Err(e) => {
                        counters.log_err(
                            &counters.notif_err,
                            &format!("notif serial={serial} register"),
                            &e,
                        );
                        drop(pad);
                        return;
                    }
                };

            // Light submit so the pad stays "alive" while we drain.
            let report = X360Report::default();
            let start = Instant::now();
            while !stop.load(Ordering::Relaxed) && start.elapsed() < duration {
                if let Err(e) = update_resilient(&pad, &report, &counters) {
                    counters.log_err(
                        &counters.notif_err,
                        &format!(
                            "notif serial={serial} update elapsed={:?} \
                             (after {READY_RETRY_ATTEMPTS} tries if TargetNotReady)",
                            start.elapsed()
                        ),
                        &e,
                    );
                }
                match rx.try_recv() {
                    Ok(_) => {
                        counters.notif_ok.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(TryRecvError::Bus(e)) => {
                        counters.log_err(
                            &counters.notif_err,
                            &format!("notif serial={serial} channel"),
                            &e,
                        );
                    }
                    Err(TryRecvError::Empty | TryRecvError::Disconnected) => {
                        // Empty / disconnected - fine when host isn't sending rumble.
                    }
                    Err(_) => {}
                }
                thread::sleep(Duration::from_millis(5));
            }

            drop(rx);
            drop(pad);
        })
    };

    // --- concurrent plug/unplug churn ----------------------------------------
    let churn_join = {
        let client = client.clone();
        let counters = Arc::clone(&counters);
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            if churn_cycles == 0 {
                return;
            }
            thread::sleep(Duration::from_millis(40));
            let report = X360Report::default();
            for cycle in 0..churn_cycles {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                match client
                    .new_x360_target()
                    .plug()
                    .and_then(|p| p.wait_for_ready())
                {
                    Ok(pad) => {
                        let serial = pad.serial_no();
                        for u in 0..3 {
                            if let Err(e) = update_resilient(&pad, &report, &counters) {
                                counters.log_err(
                                    &counters.churn_err,
                                    &format!(
                                        "churn cycle={cycle} serial={serial} update#{u} \
                                         (after {READY_RETRY_ATTEMPTS} tries if TargetNotReady)"
                                    ),
                                    &e,
                                );
                            }
                        }
                        drop(pad);
                        counters.churn_ok.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        // Slot races with workers are interesting but should be rare
                        // with max_targets headroom; count as churn error.
                        counters.log_err(
                            &counters.churn_err,
                            &format!("churn cycle={cycle} plugin/ready"),
                            &e,
                        );
                    }
                }
                // Uneven pause so churn doesn't phase-lock with workers.
                thread::sleep(Duration::from_millis(3 + u64::from(cycle % 11)));
            }
        })
    };

    // --- sample handles while chaos runs -------------------------------------
    let mut max_handles = baseline_handles;
    let mut handle_samples = Vec::with_capacity(seconds as usize + 2);
    handle_samples.push(baseline_handles);
    let sample_deadline = t0 + duration;
    while Instant::now() < sample_deadline {
        thread::sleep(Duration::from_millis(250));
        let h = process_handle_count();
        max_handles = max_handles.max(h);
        if handle_samples.len() < 64 {
            handle_samples.push(h);
        }
    }

    stop.store(true, Ordering::Relaxed);

    for (i, j) in workers.into_iter().enumerate() {
        if let Err(e) = j.join() {
            eprintln!("multi worker {i} panicked: {e:?}");
            counters.update_err.fetch_add(1, Ordering::Relaxed);
        }
    }
    if let Err(e) = notif_join.join() {
        eprintln!("multi notif panicked: {e:?}");
        counters.notif_err.fetch_add(1, Ordering::Relaxed);
    }
    if let Err(e) = churn_join.join() {
        eprintln!("multi churn panicked: {e:?}");
        counters.churn_err.fetch_add(1, Ordering::Relaxed);
    }

    // Teardown: drop client (unplugs any survivors).
    drop(client);
    thread::sleep(Duration::from_millis(100));

    let end_handles = process_handle_count();
    let (end_ws, end_pf) = process_memory();
    max_handles = max_handles.max(end_handles);
    handle_samples.push(end_handles);
    let elapsed = t0.elapsed();

    let update_ok = counters.update_ok.load(Ordering::Relaxed);
    let update_err = counters.update_err.load(Ordering::Relaxed);
    let not_ready_retries = counters.not_ready_retries.load(Ordering::Relaxed);
    let churn_ok = counters.churn_ok.load(Ordering::Relaxed);
    let churn_err = counters.churn_err.load(Ordering::Relaxed);
    let notif_ok = counters.notif_ok.load(Ordering::Relaxed);
    let notif_err = counters.notif_err.load(Ordering::Relaxed);
    let suppressed = counters.errors_suppressed.load(Ordering::Relaxed);
    let error_samples: Vec<String> = counters
        .error_log
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();

    let end_delta = i64::from(end_handles) - i64::from(baseline_handles);
    let peak_delta = i64::from(max_handles) - i64::from(baseline_handles);
    // Live budget: ~1 event per pad + notif worker handles + churn pad + OS noise.
    let handle_peak_slack = 4 + i64::from(pads) * 2 + 16;
    let pf_growth = end_pf.saturating_sub(baseline_pf);

    let pass_errors = update_err == 0 && churn_err == 0 && notif_err == 0;
    let pass_end = end_delta <= HANDLE_END_SLACK;
    let pass_peak = peak_delta <= handle_peak_slack;
    let pass_memory = pf_growth <= PAGEFILE_GROWTH_SLACK;
    let pass = pass_errors && pass_end && pass_peak && pass_memory;

    let mut report = Report::new("multi_pad_chaos")
        .line(
            "config:",
            format!(
                "{pads} workers @ {hz} Hz, {seconds}s, churn_cycles={churn_cycles}, max_targets={max_targets}"
            ),
        )
        .line(
            "env:",
            "VIGEM_STRESS_PADS / SECONDS / HZ / CHURN (defaults 4 / 10 / 250 / 50)",
        )
        .line("elapsed:", format!("{elapsed:?}"))
        .line("update ok/err:", format!("{update_ok} / {update_err}"))
        .line(
            "not_ready ret:",
            format!("{not_ready_retries}  (transient TargetNotReady recovered; not a hard fail)"),
        )
        .line("churn  ok/err:", format!("{churn_ok} / {churn_err}"))
        .line(
            "notif  ok/err:",
            format!("{notif_ok} / {notif_err}  (ok may be 0 if host sends no rumble)"),
        );

    if !error_samples.is_empty() {
        report = report.line(
            "errors logged:",
            format!(
                "{} (cap {MAX_LOGGED_ERRORS}, suppressed {suppressed})",
                error_samples.len()
            ),
        );
        for (i, line) in error_samples.iter().enumerate() {
            report = report.line(format!("  [{i}]"), line);
        }
    }

    report
        .line("handles base:", baseline_handles)
        .line(
            "handles end:",
            format!("{end_handles}  (delta {end_delta:+})"),
        )
        .line(
            "handles max:",
            format!("{max_handles}  (peak delta {peak_delta:+})"),
        )
        .line("handles samples (<=64):", format!("{handle_samples:?}"))
        .line(
            "working set:",
            format!(
                "{} -> {}  ({:+})",
                format_bytes(baseline_ws),
                format_bytes(end_ws),
                end_ws as i64 - baseline_ws as i64
            ),
        )
        .line(
            "pagefile:",
            format!(
                "{} -> {}  ({:+})",
                format_bytes(baseline_pf),
                format_bytes(end_pf),
                pf_growth as i64
            ),
        )
        .line(
            "limits:",
            format!(
                "errors=0, end delta<={HANDLE_END_SLACK}, peak delta<={handle_peak_slack}, pagefile growth<={}",
                format_bytes(PAGEFILE_GROWTH_SLACK)
            ),
        )
        .line("result:", if pass { "PASS" } else { "FAIL" })
        .print();

    assert!(
        pass,
        "multi FAILED (update_err={update_err}, churn_err={churn_err}, notif_err={notif_err}, \
         end_delta={end_delta}, peak_delta={peak_delta}, pagefile_growth={})",
        format_bytes(pf_growth)
    );
}
