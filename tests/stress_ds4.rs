//! DS4 host-output chaos test: routing, lifetime and leak checks under load.
//!
//! Requires ViGEmBus. Not run in CI. Excluded from default `cargo test` via
//! `#[ignore]`. The summary needs `--nocapture`:
//!
//! ```text
//! cargo test --test stress_ds4 -- --ignored --nocapture
//! VIGEM_STRESS_SECONDS=120 cargo test --test stress_ds4 -- --ignored --nocapture
//! ```
//!
//! Env knobs (optional):
//! - `VIGEM_STRESS_SECONDS` - how long the chaos runs (default `10`)
//! - `VIGEM_STRESS_PADS` - long-lived DS4 pads (default `3`, max `8`)
//! - `VIGEM_STRESS_SEED` - seed for the random choices (default: clock). The
//!   summary prints the seed used. Thread timing still differs between runs.
//!
//! # How it provokes host output
//!
//! The test plays the game's part: it opens each virtual pad's HID interface
//! and writes DS4 output reports (`common::hid`). Every report carries a tag:
//! the identity of the pad it was written to (unique per plug, so a pad on a
//! reused serial has a new one) and a sequence number. A receiver can then
//! tell exactly what it was given:
//!
//! - a report tagged for **another** pad: mis-routed, or a previous pad's
//!   report replayed on a reused serial. Always a failure.
//! - a sequence number that does not increase: repeated or out of order
//! - a missing sequence number on an unbounded receiver: lost
//!
//! Reports without a tag come from host software (Steam, `GameInput`), which
//! writes to every DS4 pad all the time. They are only counted.
//!
//! # What the driver does on its own
//!
//! Measured on ViGEmBus 1.21.442 with this test (about 2,400 reports/s over
//! three pads):
//!
//! - **Raw output: about 1% of reports never reach a listener, and one in
//!   several thousand is handed out twice.** A bare `DeviceIoControl` loop on
//!   its own bus handle (`common::bus_tap`, none of this crate's code) loses
//!   the same share, and each handle loses different reports. The typed
//!   stream of the same pad does get them, so they did reach the driver.
//! - **Typed notifications: 2 to 4% arrive out of order** (an older report
//!   after a newer one); almost none are lost.
//!
//! So loss and order are not asserted exactly. Raw loss and repeats on the
//! long-lived pads are compared with what the bare listener got in the same
//! run, and typed disorder has a limit several times the measured rate.

#![cfg(all(windows, feature = "ds4"))]

mod common;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use vigem_rust::{
    BusError, Client, ClientError, Ds4Notification, Ds4OutputBuffer, Ds4Report, Ds4ReportEx,
    DualShock4, NotificationBuffer, NotificationReceiver, RecvTimeoutError, TargetHandle,
};

use common::bus_tap::BusTap;
use common::hid::{DS4_OUTPUT_REPORT_LEN, Ds4HidOutput};
use common::{
    Report, driver_lock, env_u32, format_bytes, handle_trend_ok, process_handle_count,
    process_memory, try_client,
};

// =============================================================================
// Tagged output reports
// =============================================================================

/// Lightbar red of every tagged report (`report[6]`).
const TAG_RED: u8 = 0xD5;
/// Flash on/off bytes of every tagged report (`report[9..11]`). Only the raw
/// 64-byte report carries them, so raw tags are checked against both marks.
const TAG_FLASH: [u8; 2] = [0xC3, 0x3C];
/// First pad identity handed out. Identities are sequential from here.
const FIRST_IDENT: u16 = 0x4000;

/// A subscription to a plugged pad must deliver something within this long,
/// with a report written every [`DELIVERY_RETRY`].
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(3);
/// One report can be dropped by the driver, so delivery checks keep writing.
const DELIVERY_RETRY: Duration = Duration::from_millis(250);
/// Receivers must report `Disconnected` within this long of their pad going away.
const DISCONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// A new pad's HID interface must accept a write within this long.
const HID_TIMEOUT: Duration = Duration::from_secs(5);
/// Actors must return within this long of being told to stop (deadlock guard).
const JOIN_TIMEOUT: Duration = Duration::from_secs(30);

/// (pad identity, sequence number)
type Tag = (u16, u16);

/// A DS4 USB output report (id `0x05`) carrying `tag`.
///
/// Layout: `[4]` small motor, `[5]` large motor, `[6..9]` lightbar RGB,
/// `[9]`/`[10]` flash on/off. The sequence goes in the motors and the identity
/// in green/blue, so the typed [`Ds4Notification`] carries the tag too.
fn tagged_report((ident, seq): Tag) -> [u8; DS4_OUTPUT_REPORT_LEN] {
    let mut report = [0u8; DS4_OUTPUT_REPORT_LEN];
    report[0] = 0x05;
    report[1] = 0xFF;
    report[4..6].copy_from_slice(&seq.to_be_bytes());
    report[6] = TAG_RED;
    report[7..9].copy_from_slice(&ident.to_be_bytes());
    report[9..11].copy_from_slice(&TAG_FLASH);
    report
}

fn raw_tag(b: &[u8; 64]) -> Option<Tag> {
    (b[6] == TAG_RED && b[9..11] == TAG_FLASH).then(|| {
        (
            u16::from_be_bytes([b[7], b[8]]),
            u16::from_be_bytes([b[4], b[5]]),
        )
    })
}

fn typed_tag(n: &Ds4Notification, shared: &Shared) -> Option<Tag> {
    let ident = u16::from_be_bytes([n.lightbar.green, n.lightbar.blue]);
    // No flash bytes here, so also require an identity that was handed out.
    (n.lightbar.red == TAG_RED && shared.is_issued(ident))
        .then(|| (ident, u16::from_be_bytes([n.small_motor, n.large_motor])))
}

/// `a` was written after `b` (sequence numbers wrap).
fn after(a: u16, b: u16) -> bool {
    a != b && a.wrapping_sub(b) < 0x8000
}

// =============================================================================
// Shared state
// =============================================================================

/// Cap printed failure lines so a death spiral does not flood the console.
const MAX_LOGGED_FAILURES: usize = 48;

/// What a thread was last seen doing, for the hang report.
struct Activity {
    who: String,
    step: &'static str,
    since: Instant,
}

/// What the bare listener ([`BusTap`]) has seen for one pad.
#[derive(Default, Clone, Copy)]
struct Tapped {
    received: u64,
    disorder: u64,
    last: Option<u16>,
}

#[derive(Default)]
struct Shared {
    stop: AtomicBool,
    idents_issued: AtomicU16,
    failures: AtomicU64,
    failure_log: Mutex<Vec<String>>,
    activity: Mutex<HashMap<thread::ThreadId, Activity>>,
    tapped: Mutex<HashMap<u16, Tapped>>,

    written: AtomicU64,
    raw_tagged: AtomicU64,
    raw_host: AtomicU64,
    raw_disorder: AtomicU64,
    typed_tagged: AtomicU64,
    typed_host: AtomicU64,
    typed_disorder: AtomicU64,
    subscriptions: AtomicU64,
    deliveries_checked: AtomicU64,
    disconnects_checked: AtomicU64,
    pads_plugged: AtomicU64,
    clients_opened: AtomicU64,

    /// Short bursts on pads of the churn and second-client actors.
    burst_written: AtomicU64,
    burst_received: AtomicU64,
    /// Long-lived pads, from the moment their permanent receiver worked:
    /// reports written, and what that receiver / the bare listener got.
    kept_written: AtomicU64,
    kept_received: AtomicU64,
    kept_disorder: AtomicU64,
    tap_received: AtomicU64,
    tap_disorder: AtomicU64,
}

impl Shared {
    fn stopping(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    fn new_ident(&self) -> u16 {
        FIRST_IDENT + self.idents_issued.fetch_add(1, Ordering::Relaxed)
    }

    fn is_issued(&self, ident: u16) -> bool {
        ident
            .checked_sub(FIRST_IDENT)
            .is_some_and(|n| n < self.idents_issued.load(Ordering::Relaxed))
    }

    fn fail(&self, message: String) {
        self.failures.fetch_add(1, Ordering::Relaxed);
        let mut log = self.failure_log.lock().unwrap_or_else(|e| e.into_inner());
        if log.len() < MAX_LOGGED_FAILURES {
            // Also print at once so a failure shows up before the summary.
            eprintln!("[ds4 chaos] {message}");
            log.push(message);
        }
    }

    fn check(&self, context: &str, result: Result<(), String>) {
        if let Err(e) = result {
            self.fail(format!("{context}: {e}"));
        }
    }

    /// Name the calling thread for the hang report.
    fn enter(&self, who: String) {
        let activity = Activity {
            who,
            step: "starting",
            since: Instant::now(),
        };
        self.activity
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(thread::current().id(), activity);
    }

    /// Record what the calling thread is about to do.
    fn at(&self, step: &'static str) {
        let mut map = self.activity.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(activity) = map.get_mut(&thread::current().id()) {
            activity.step = step;
            activity.since = Instant::now();
        }
    }

    /// One line per thread: what it was doing and for how long.
    fn activity_report(&self) -> String {
        let map = self.activity.lock().unwrap_or_else(|e| e.into_inner());
        let mut lines: Vec<String> = map
            .values()
            .map(|a| format!("  {}: {} (for {:.1?})", a.who, a.step, a.since.elapsed()))
            .collect();
        lines.sort();
        lines.join("\n")
    }

    /// Called by the bare listener for every report on the bus.
    fn tap(&self, report: &[u8; 64]) {
        let Some((ident, seq)) = raw_tag(report) else {
            return;
        };
        let mut map = self.tapped.lock().unwrap_or_else(|e| e.into_inner());
        let pad = map.entry(ident).or_default();
        if pad.last.is_none_or(|last| after(seq, last)) {
            pad.last = Some(seq);
            pad.received += 1;
        } else {
            pad.disorder += 1;
        }
    }

    fn tapped(&self, ident: u16) -> Tapped {
        let map = self.tapped.lock().unwrap_or_else(|e| e.into_inner());
        map.get(&ident).copied().unwrap_or_default()
    }
}

fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

/// xorshift64; only has to make the actors' choices differ.
struct Rng(u64);

impl Rng {
    fn new(seed: u32, stream: u64) -> Self {
        Self((u64::from(seed) ^ (stream << 32)).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 16) % n
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    fn millis(&mut self, max: u64) -> Duration {
        Duration::from_millis(self.below(max + 1))
    }
}

// =============================================================================
// One plugged pad and its HID writer
// =============================================================================

struct Writer {
    hid: Ds4HidOutput,
    next_seq: u16,
    count: u64,
}

/// A plugged DS4 pad, its identity, and the "game" end of its HID interface.
struct Pad {
    handle: TargetHandle<DualShock4>,
    ident: u16,
    /// One lock for "take a sequence number and write it", so reports reach
    /// the driver in sequence order whichever thread writes.
    writer: Mutex<Writer>,
}

impl Pad {
    fn plug(client: &Client, shared: &Shared) -> Result<Self, String> {
        shared.at("plugging a pad");
        let handle = client
            .new_ds4_target()
            .plug()
            .and_then(|p| p.wait_for_ready())
            .map_err(|e| format!("plug DS4 pad: {e} ({e:?})"))?;
        let serial = handle.serial_no();
        let ident = shared.new_ident();
        bump(&shared.pads_plugged);

        // Open, then prove the interface with one write: right after a pad is
        // plugged on a reused serial, the previous pad's interface can still
        // be listed for a moment and fails the write.
        shared.at("opening the pad's HID interface");
        let start = Instant::now();
        loop {
            let attempt = Ds4HidOutput::open(serial, HID_TIMEOUT)
                .and_then(|hid| hid.write(&tagged_report((ident, 1))).map(|()| hid));
            match attempt {
                Ok(hid) => {
                    bump(&shared.written);
                    let writer = Mutex::new(Writer {
                        hid,
                        next_seq: 2,
                        count: 1,
                    });
                    return Ok(Self {
                        handle,
                        ident,
                        writer,
                    });
                }
                Err(e) if start.elapsed() >= HID_TIMEOUT => {
                    return Err(format!("serial {serial}: {e}"));
                }
                Err(_) => thread::sleep(Duration::from_millis(10)),
            }
        }
    }

    fn name(&self) -> String {
        format!(
            "pad {:#06x} (serial {})",
            self.ident,
            self.handle.serial_no()
        )
    }

    /// Write the next tagged report; returns its sequence number.
    fn write(&self, shared: &Shared) -> Result<u16, String> {
        shared.at("writing a HID output report");
        let mut w = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let seq = w.next_seq;
        w.hid
            .write(&tagged_report((self.ident, seq)))
            .map_err(|e| format!("write output report {seq}: {e}"))?;
        w.next_seq = seq.wrapping_add(1);
        w.count += 1;
        bump(&shared.written);
        Ok(seq)
    }

    /// Reports written so far.
    fn written(&self) -> u64 {
        self.writer.lock().unwrap_or_else(|e| e.into_inner()).count
    }

    /// Write a few reports nobody is subscribed for. A later pad on this
    /// serial must never receive them.
    fn write_unheard(&self, count: u64, shared: &Shared) -> Result<(), String> {
        for _ in 0..count {
            self.write(shared)?;
        }
        Ok(())
    }

    fn subscribe_raw(&self, buffer: NotificationBuffer, shared: &Shared) -> Result<RawRx, String> {
        bump(&shared.subscriptions);
        shared.at("register_notification_raw_buffer");
        self.handle
            .register_notification_raw_buffer_with(buffer)
            .map_err(|e| format!("register_notification_raw_buffer: {e} ({e:?})"))
    }

    fn subscribe_typed(
        &self,
        shared: &Shared,
    ) -> Result<NotificationReceiver<Ds4Notification>, String> {
        bump(&shared.subscriptions);
        shared.at("register_notification");
        self.handle
            .register_notification_with(NotificationBuffer::Unbounded)
            .map_err(|e| format!("register_notification: {e} ({e:?})"))
    }
}

// =============================================================================
// Checking what a receiver is given
// =============================================================================

type RawRx = NotificationReceiver<Ds4OutputBuffer>;

/// Everything one receiver has been given, checked item by item.
struct Stream<'a> {
    shared: &'a Shared,
    kind: &'static str,
    ident: u16,
    last: Option<u16>,
    /// Reports for this pad, each newer than the one before.
    received: u64,
    /// Reports for this pad that were not (repeated or out of order).
    disorder: u64,
}

impl<'a> Stream<'a> {
    fn new(shared: &'a Shared, kind: &'static str, pad: &Pad) -> Self {
        Self {
            shared,
            kind,
            ident: pad.ident,
            last: None,
            received: 0,
            disorder: 0,
        }
    }

    /// Returns the sequence number if this was a new report for this pad.
    fn see(&mut self, tag: Option<Tag>) -> Option<u16> {
        let (ident, seq) = tag?;
        if ident != self.ident {
            // The one thing that must never happen, whatever the driver does.
            self.shared.fail(format!(
                "{} receiver of pad {:#06x} was given report {seq} written to pad {ident:#06x}",
                self.kind, self.ident
            ));
            return None;
        }
        if self.last.is_some_and(|last| !after(seq, last)) {
            self.disorder += 1;
            return None;
        }
        self.last = Some(seq);
        self.received += 1;
        Some(seq)
    }

    fn raw(&mut self, buffer: &Ds4OutputBuffer) -> Option<u16> {
        let tag = raw_tag(&buffer.bytes);
        let seen = self.see(tag);
        bump(match (tag, seen) {
            (None, _) => &self.shared.raw_host,
            (Some(_), Some(_)) => &self.shared.raw_tagged,
            (Some(_), None) => &self.shared.raw_disorder,
        });
        seen
    }

    fn typed(&mut self, n: &Ds4Notification) -> Option<u16> {
        let tag = typed_tag(n, self.shared);
        let seen = self.see(tag);
        bump(match (tag, seen) {
            (None, _) => &self.shared.typed_host,
            (Some(_), Some(_)) => &self.shared.typed_tagged,
            (Some(_), None) => &self.shared.typed_disorder,
        });
        seen
    }
}

/// One receive, with the ways a live subscription must not end as errors.
fn recv_live(rx: &RawRx, wait: Duration) -> Result<Option<Ds4OutputBuffer>, String> {
    match rx.recv_timeout(wait) {
        Ok(buffer) => Ok(Some(buffer)),
        Err(RecvTimeoutError::Timeout) => Ok(None),
        Err(RecvTimeoutError::Disconnected) => {
            Err("receiver disconnected while its pad is plugged in".into())
        }
        Err(RecvTimeoutError::Bus(e)) => Err(format!("listener failed: {e} ({e:?})")),
        Err(e) => Err(format!("unexpected receive error: {e:?}")),
    }
}

/// The subscription behind `rx` is alive: a report written now arrives.
///
/// Writes a report every [`DELIVERY_RETRY`] (the driver can drop one) until
/// one of them arrives, and fails if none has after [`DELIVERY_TIMEOUT`].
/// `seen` is called for every new report of this pad read on the way.
fn check_delivery(
    pad: &Pad,
    rx: &RawRx,
    stream: &mut Stream<'_>,
    mut seen: impl FnMut(u16),
) -> Result<(), String> {
    let shared = stream.shared;
    let first = pad.write(shared)?;
    let (start, mut last_write, mut writes) = (Instant::now(), Instant::now(), 1);
    while start.elapsed() < DELIVERY_TIMEOUT {
        shared.at("waiting for a report on a live subscription");
        if let Some(buffer) = recv_live(rx, Duration::from_millis(50))?
            && let Some(got) = stream.raw(&buffer)
        {
            seen(got);
            if got == first || after(got, first) {
                bump(&shared.deliveries_checked);
                return Ok(());
            }
        }
        if last_write.elapsed() >= DELIVERY_RETRY {
            pad.write(shared)?;
            last_write = Instant::now();
            writes += 1;
        }
    }
    Err(format!(
        "nothing arrived on a live subscription within {DELIVERY_TIMEOUT:?} \
         ({writes} reports written, from report {first})"
    ))
}

/// Write `count` reports back to back, then check the subscription is still
/// alive. How many of the burst arrived is recorded, not asserted (the driver
/// drops some).
///
/// Only for an unbounded receiver of a pad that no other thread writes to.
fn check_burst(pad: &Pad, rx: &RawRx, stream: &mut Stream<'_>, count: u64) -> Result<(), String> {
    let shared = stream.shared;
    let first = pad.write(shared)?;
    let mut last = first;
    for _ in 1..count {
        last = pad.write(shared)?;
    }
    let mut received = 0;
    let result = check_delivery(pad, rx, stream, |seq| {
        if !after(first, seq) && !after(seq, last) {
            received += 1;
        }
    });
    shared.burst_written.fetch_add(count, Ordering::Relaxed);
    shared.burst_received.fetch_add(received, Ordering::Relaxed);
    result
}

/// Read `rx` for `window`, checking every item.
fn drain(rx: &RawRx, stream: &mut Stream<'_>, window: Duration) -> Result<(), String> {
    stream.shared.at("reading a raw receiver");
    let end = Instant::now() + window;
    while let Some(left) = end.checked_duration_since(Instant::now()) {
        if let Some(buffer) = recv_live(rx, left.min(Duration::from_millis(50)))? {
            stream.raw(&buffer);
        }
    }
    Ok(())
}

/// The pad (or its client) is gone: `rx` must end, not hang or keep delivering.
fn check_disconnect<N>(
    rx: &NotificationReceiver<N>,
    what: &str,
    shared: &Shared,
) -> Result<(), String> {
    shared.at("waiting for a receiver to disconnect");
    let start = Instant::now();
    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            // A failed listener also ends the stream; the pad is gone either way.
            Err(RecvTimeoutError::Disconnected | RecvTimeoutError::Bus(_)) => {
                bump(&shared.disconnects_checked);
                return Ok(());
            }
            // Reports from before the unplug may still be queued.
            Ok(_) | Err(_) if start.elapsed() < DISCONNECT_TIMEOUT => {}
            _ => {
                return Err(format!(
                    "{what} receiver did not disconnect within {DISCONNECT_TIMEOUT:?}"
                ));
            }
        }
    }
}

// =============================================================================
// Actors
// =============================================================================

/// A pad that stays plugged for the whole run, with four things going on at
/// once: a writer producing host output and input reports, a permanent
/// unbounded raw receiver, a permanent typed receiver, and a subscriber that
/// keeps registering and dropping receivers.
fn long_lived_pad(client: &Client, shared: &Shared, seed: u32, id: u64) {
    let pad = match Pad::plug(client, shared) {
        Ok(pad) => pad,
        Err(e) => return shared.fail(format!("long-lived pad {id}: {e}")),
    };
    let name = pad.name();
    let receivers = pad
        .subscribe_raw(NotificationBuffer::Unbounded, shared)
        .and_then(|raw| pad.subscribe_typed(shared).map(|typed| (raw, typed)));
    let (kept_rx, typed_rx) = match receivers {
        Ok(pair) => pair,
        Err(e) => return shared.fail(format!("{name}: {e}")),
    };

    // From here on, count what is written to this pad against what the
    // permanent receiver and the bare listener get.
    let mut kept = Stream::new(shared, "raw", &pad);
    if let Err(e) = check_delivery(&pad, &kept_rx, &mut kept, |_| {}) {
        return shared.fail(format!("{name}: {e}"));
    }
    let written_before = pad.written();
    let (received_before, disorder_before) = (kept.received, kept.disorder);
    let tapped_before = shared.tapped(pad.ident);

    let writers = AtomicU32::new(2);
    thread::scope(|scope| {
        scope.spawn(|| {
            shared.enter(format!("{name} writer"));
            write_loop(&pad, shared, Rng::new(seed, id * 4 + 1));
            shared.at("finished");
            writers.fetch_sub(1, Ordering::Release);
        });
        scope.spawn(|| {
            shared.enter(format!("{name} resubscriber"));
            resubscribe_loop(&pad, shared, Rng::new(seed, id * 4 + 2));
            shared.at("finished");
            writers.fetch_sub(1, Ordering::Release);
        });
        scope.spawn(|| {
            shared.enter(format!("{name} typed reader"));
            let mut typed = Stream::new(shared, "typed", &pad);
            while writers.load(Ordering::Acquire) > 0 {
                match typed_rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(n) => {
                        typed.typed(&n);
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(e) => {
                        shared.fail(format!("{name}: typed receiver ended: {e:?}"));
                        break;
                    }
                }
            }
            shared.at("finished");
        });

        // Read until both writers are done and their last reports are in:
        // nothing of this pad's for half a second (host software never stops).
        shared.at("reading the permanent raw receiver");
        let mut last_own = Instant::now();
        loop {
            match recv_live(&kept_rx, Duration::from_millis(50)) {
                Ok(Some(buffer)) => {
                    if raw_tag(&buffer.bytes).is_some() {
                        last_own = Instant::now();
                    }
                    kept.raw(&buffer);
                }
                Ok(None) => {}
                Err(e) => {
                    shared.fail(format!("{name}: {e}"));
                    break;
                }
            }
            if writers.load(Ordering::Acquire) == 0
                && last_own.elapsed() > Duration::from_millis(500)
            {
                break;
            }
        }
    });

    let tapped = shared.tapped(pad.ident);
    let add = |counter: &AtomicU64, n: u64| counter.fetch_add(n, Ordering::Relaxed);
    add(&shared.kept_written, pad.written() - written_before);
    add(&shared.kept_received, kept.received - received_before);
    add(&shared.kept_disorder, kept.disorder - disorder_before);
    add(
        &shared.tap_received,
        tapped.received - tapped_before.received,
    );
    add(
        &shared.tap_disorder,
        tapped.disorder - tapped_before.disorder,
    );
}

/// Host output in uneven bursts, plus input reports on the same pad.
fn write_loop(pad: &Pad, shared: &Shared, mut rng: Rng) {
    let mut report = Ds4Report::default();
    let mut report_ex = Ds4ReportEx::default();
    let mut tick = 0u8;
    while !shared.stopping() {
        let burst = if rng.chance(5) { 1 + rng.below(16) } else { 1 };
        for _ in 0..burst {
            if let Err(e) = pad.write(shared) {
                return shared.fail(format!("{}: {e}", pad.name()));
            }
        }
        tick = tick.wrapping_add(1);
        shared.at("update");
        let submitted = match rng.below(4) {
            0 => {
                report.trigger_right = tick;
                pad.handle.update(&report)
            }
            1 => {
                report_ex.gyro_x = i16::from(tick);
                pad.handle.update_ex(&report_ex)
            }
            _ => Ok(()),
        };
        match submitted {
            // Dropped because the host had no read waiting; not a failure.
            Ok(()) | Err(ClientError::Bus(BusError::TargetNotReady { .. })) => {}
            Err(e) => shared.fail(format!("{}: update: {e} ({e:?})", pad.name())),
        }
        thread::sleep(Duration::from_micros(rng.below(3000)));
    }
}

/// Register and drop raw receivers on a plugged pad, in every order and with
/// every buffer kind, checking that each one works for as long as it lives.
fn resubscribe_loop(pad: &Pad, shared: &Shared, mut rng: Rng) {
    while !shared.stopping() {
        let buffer = match rng.below(4) {
            0 => NotificationBuffer::Unbounded,
            1 => NotificationBuffer::Bounded { capacity: 1 },
            2 => NotificationBuffer::Bounded { capacity: 8 },
            _ => NotificationBuffer::default(),
        };
        let rx = match pad.subscribe_raw(buffer, shared) {
            Ok(rx) => rx,
            Err(e) => return shared.fail(format!("{}: {e}", pad.name())),
        };
        let mut stream = Stream::new(shared, "raw", pad);
        let result = match rng.below(5) {
            0 => Ok(()),
            1 | 2 => check_delivery(pad, &rx, &mut stream, |_| {}),
            3 => drain(&rx, &mut stream, rng.millis(150)),
            _ => {
                // Two receivers; dropping the first must not disturb the second.
                let second = NotificationBuffer::Bounded { capacity: 1 };
                pad.subscribe_raw(second, shared).and_then(|rx2| {
                    let mut stream2 = Stream::new(shared, "raw", pad);
                    drain(&rx, &mut stream, rng.millis(30))?;
                    shared.at("dropping a raw receiver");
                    drop(rx);
                    check_delivery(pad, &rx2, &mut stream2, |_| {})
                })
            }
        };
        // `rx` (if not moved above) is dropped at the end of this iteration.
        shared.at("dropping a raw receiver");
        shared.check(&pad.name(), result);
        if rng.chance(30) {
            thread::sleep(rng.millis(20));
        }
    }
}

/// Plug and unplug pads on the shared client, ending each in a different way.
/// Serials are reused constantly, so every new pad is a chance to be handed a
/// previous pad's reports.
fn churn_loop(client: &Client, shared: &Shared, mut rng: Rng) {
    while !shared.stopping() {
        match Pad::plug(client, shared) {
            Ok(pad) => {
                let name = pad.name();
                shared.check(&name, churn_once(pad, shared, &mut rng));
            }
            Err(e) => {
                shared.fail(format!("churn: {e}"));
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

fn churn_once(pad: Pad, shared: &Shared, rng: &mut Rng) -> Result<(), String> {
    let unplug = |pad: &Pad| {
        shared.at("unplug");
        pad.handle.unplug().map_err(|e| format!("unplug: {e}"))
    };
    match rng.below(5) {
        // Subscribe, use, unsubscribe, then output nobody hears, then unplug.
        0 => {
            let rx = pad.subscribe_raw(NotificationBuffer::default(), shared)?;
            check_delivery(&pad, &rx, &mut Stream::new(shared, "raw", &pad), |_| {})?;
            drop(rx);
            pad.write_unheard(1 + rng.below(30), shared)?;
            shared.at("dropping the last handle of a pad");
            Ok(())
        }
        // Explicit unplug with raw and typed receivers alive.
        1 => {
            let raw = pad.subscribe_raw(NotificationBuffer::Unbounded, shared)?;
            let typed = pad.subscribe_typed(shared)?;
            let mut stream = Stream::new(shared, "raw", &pad);
            check_burst(&pad, &raw, &mut stream, 1 + rng.below(24))?;
            unplug(&pad)?;
            check_disconnect(&raw, "raw", shared)?;
            check_disconnect(&typed, "typed", shared)?;
            match pad.handle.register_notification_raw_buffer() {
                Err(ClientError::TargetDoesNotExist { .. }) => Ok(()),
                other => Err(format!("registering on an unplugged pad gave {other:?}")),
            }
        }
        // Never subscribed.
        2 => {
            pad.write_unheard(1 + rng.below(30), shared)?;
            shared.at("dropping the last handle of a pad");
            Ok(())
        }
        // Dropping the last handle unplugs; the receiver outlives the pad.
        3 => {
            let rx = pad.subscribe_raw(NotificationBuffer::default(), shared)?;
            check_delivery(&pad, &rx, &mut Stream::new(shared, "raw", &pad), |_| {})?;
            shared.at("dropping the last handle of a pad");
            drop(pad);
            check_disconnect(&rx, "raw", shared)
        }
        // Another thread registers receivers while this one unplugs.
        _ => {
            let first = pad.subscribe_raw(NotificationBuffer::default(), shared)?;
            check_delivery(&pad, &first, &mut Stream::new(shared, "raw", &pad), |_| {})?;
            let handle = pad.handle.clone();
            thread::scope(|scope| {
                let registering = scope.spawn(move || {
                    shared.enter("churn: registering during unplug".into());
                    let mut receivers = Vec::new();
                    while receivers.len() < 64 {
                        match handle.register_notification_raw_buffer() {
                            Ok(rx) => receivers.push(rx),
                            Err(ClientError::TargetDoesNotExist { .. }) => break,
                            Err(e) => return Err(format!("register during unplug: {e} ({e:?})")),
                        }
                    }
                    shared.at("finished");
                    Ok(receivers)
                });
                thread::sleep(Duration::from_micros(rng.below(2000)));
                let unplugged = unplug(&pad);
                shared.at("joining the registering thread");
                let receivers = registering
                    .join()
                    .map_err(|_| "registering thread panicked".to_string())??;
                unplugged?;
                bump(&shared.subscriptions);
                for rx in receivers.iter().chain([&first]) {
                    check_disconnect(rx, "raw", shared)?;
                }
                Ok(())
            })
        }
    }
}

/// A second `Client` in this process, opened and dropped over and over.
///
/// It has its own bus handle and its own raw-output listener, which starts and
/// stops with each subscription (the shared client's listener never stops,
/// because the long-lived pads stay subscribed). The driver gives every open
/// handle a copy of every DS4 report on the bus, so each client's listener
/// also sees, and must discard, all of the other's.
fn second_client_loop(shared: &Shared, mut rng: Rng) {
    while !shared.stopping() {
        shared.at("Client::connect");
        let client = match Client::connect() {
            Ok(client) => client,
            Err(e) => {
                shared.fail(format!("second client: connect: {e} ({e:?})"));
                thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        bump(&shared.clients_opened);
        let result = second_client_once(client, shared, &mut rng);
        shared.check("second client", result);
    }
}

fn second_client_once(client: Client, shared: &Shared, rng: &mut Rng) -> Result<(), String> {
    // A few pads one after another (same serial each time, most likely), then
    // one that is still plugged when the client goes away.
    for _ in 0..rng.below(3) {
        let pad = Pad::plug(&client, shared)?;
        let name = pad.name();
        second_client_pad(&pad, shared, rng)
            .map(drop)
            .map_err(|e| format!("{name}: {e}"))?;
        shared.at("dropping the last handle of a pad");
    }
    let pad = Pad::plug(&client, shared)?;
    let name = pad.name();
    let rx = second_client_pad(&pad, shared, rng).map_err(|e| format!("{name}: {e}"))?;

    match rng.below(3) {
        // In order: receiver, pad, client.
        0 => {
            shared.at("dropping receiver, pad, client");
            Ok(())
        }
        // Client first: the receiver must end and the pad must stop working.
        1 => {
            shared.at("dropping the client before its pad");
            drop(client);
            if let Some(rx) = &rx {
                check_disconnect(rx, "raw (client dropped)", shared)
                    .map_err(|e| format!("{name}: {e}"))?;
            }
            shared.at("update on a pad whose client is gone");
            match pad.handle.update(&Ds4Report::default()) {
                Err(_) => Ok(()),
                Ok(()) => Err(format!(
                    "{name}: update succeeded after its client was dropped"
                )),
            }
        }
        // Pad, then client, then the receiver last.
        _ => {
            shared.at("dropping pad, client, receiver");
            drop(pad);
            drop(client);
            match &rx {
                Some(rx) => check_disconnect(rx, "raw (pad dropped)", shared)
                    .map_err(|e| format!("{name}: {e}")),
                None => Ok(()),
            }
        }
    }
}

/// Use one pad of the second client. May return a live receiver.
fn second_client_pad(pad: &Pad, shared: &Shared, rng: &mut Rng) -> Result<Option<RawRx>, String> {
    if rng.chance(25) {
        pad.write_unheard(1 + rng.below(30), shared)?;
        return Ok(None);
    }
    let mut stream = Stream::new(shared, "raw", pad);
    let rx = pad.subscribe_raw(NotificationBuffer::Unbounded, shared)?;
    check_burst(pad, &rx, &mut stream, 1 + rng.below(16))?;
    if rng.chance(50) {
        return Ok(Some(rx));
    }
    // Stop listening, let output pile up unread, listen again.
    shared.at("dropping a raw receiver");
    drop(rx);
    pad.write_unheard(1 + rng.below(30), shared)?;
    if rng.chance(50) {
        return Ok(None);
    }
    let rx = pad.subscribe_raw(NotificationBuffer::Unbounded, shared)?;
    check_delivery(pad, &rx, &mut stream, |_| {})?;
    Ok(Some(rx))
}

// =============================================================================
// The test
// =============================================================================

/// One pad, both receivers: proves the HID writer and the tag decoding before
/// the chaos relies on them, and loads everything that is loaded once per
/// process so it is not counted as a leak.
fn calibrate(client: &Client, shared: &Shared) -> Result<(), String> {
    let pad = Pad::plug(client, shared)?;
    let raw = pad.subscribe_raw(NotificationBuffer::Unbounded, shared)?;
    let typed = pad.subscribe_typed(shared)?;

    // A tag of this pad with a sequence number that was really written.
    let ours = |tag: Option<Tag>, newest: u16| {
        tag.is_some_and(|(ident, seq)| ident == pad.ident && (1..=newest).contains(&seq))
    };
    let (mut raw_ok, mut typed_ok) = (false, false);
    let mut last_typed = None;
    let start = Instant::now();
    while !(raw_ok && typed_ok) {
        if start.elapsed() > DELIVERY_TIMEOUT {
            return Err(format!(
                "reports written to the pad's HID interface did not come back \
                 (raw receiver: {}, typed receiver: {}, last typed notification: {last_typed:?})",
                if raw_ok { "ok" } else { "nothing" },
                if typed_ok { "ok" } else { "nothing" },
            ));
        }
        let newest = pad.write(shared)?;
        let wait = Duration::from_millis(100);
        if let Some(buffer) = recv_live(&raw, wait)? {
            raw_ok |= ours(raw_tag(&buffer.bytes), newest);
        }
        if let Ok(n) = typed.recv_timeout(wait) {
            typed_ok |= ours(typed_tag(&n, shared), newest);
            last_typed = Some(n);
        }
    }
    Ok(())
}

/// Runs `actor` on a named thread; `done` hears about it when it returns,
/// even by panic.
fn spawn_actor(
    name: &str,
    shared: &Arc<Shared>,
    done: &mpsc::Sender<String>,
    actor: impl FnOnce(&Shared) + Send + 'static,
) -> (String, thread::JoinHandle<()>) {
    struct Done(String, mpsc::Sender<String>);
    impl Drop for Done {
        fn drop(&mut self) {
            let _ = self.1.send(std::mem::take(&mut self.0));
        }
    }
    let guard = Done(name.to_owned(), done.clone());
    let shared = Arc::clone(shared);
    let who = name.to_owned();
    let handle = thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let _guard = guard;
            shared.enter(who);
            actor(&shared);
            shared.at("finished");
        })
        .expect("spawn actor thread");
    (name.to_owned(), handle)
}

fn percent(part: u64, whole: u64) -> String {
    if whole == 0 {
        "-".into()
    } else {
        format!("{:.2}%", part as f64 * 100.0 / whole as f64)
    }
}

/// DS4 host output under concurrent subscribe / unsubscribe / plug / unplug /
/// client churn, with every delivered report checked against its tag.
///
/// Actors, all at once:
/// - `VIGEM_STRESS_PADS` long-lived pads (see [`long_lived_pad`])
/// - one thread plugging and unplugging pads on the same client
///   (see [`churn_once`])
/// - one thread opening and dropping a second client (see
///   [`second_client_loop`])
///
/// Pass criteria:
/// - no receiver is ever given a report written to another pad (mis-routing,
///   or a previous pad's report on a reused serial)
/// - every subscription that is checked for it delivers within 3 s
/// - receivers disconnect within 3 s of unplug / last handle drop / client drop
/// - no API call fails unexpectedly, no actor panics, none is still running
///   30 s after stop (the failure then lists what each thread was doing)
/// - raw reports lost by the long-lived pads' permanent receivers: at most
///   twice what a bare listener lost in the same run, plus 1%
/// - raw reports repeated or out of order on those receivers: at most twice
///   the bare listener's, plus 10
/// - typed notifications out of order: at most 15%
/// - after teardown, handles <= baseline + 4
/// - handle samples do not ratchet: last <= median(first half) + 32
/// - pagefile (commit) growth <= 16 MiB
///
/// See the module docs for why loss and order are relative to the driver.
#[test]
#[ignore = "requires ViGEmBus; long-running"]
fn ds4_output_chaos() {
    const HANDLE_END_SLACK: i64 = 4;
    // Live handles swing with the pads, receivers and clients that exist at
    // each sample; a leak per cycle would add hundreds over a run.
    const HANDLE_TREND_SLACK: i64 = 32;
    const PAGEFILE_GROWTH_SLACK: usize = 16 * 1024 * 1024;
    const TYPED_DISORDER_LIMIT_PERCENT: u64 = 15;

    let _guard = driver_lock();
    let seconds = env_u32("VIGEM_STRESS_SECONDS", 10).max(1);
    let pads = env_u32("VIGEM_STRESS_PADS", 3).clamp(1, 8);
    let clock_seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(1, |d| d.subsec_nanos());
    let seed = env_u32("VIGEM_STRESS_SEED", clock_seed);

    let shared = Arc::new(Shared::default());

    let Some(warm_up) = try_client() else {
        return;
    };
    if let Err(e) = calibrate(&warm_up, &shared) {
        panic!(
            "cannot drive DS4 host output on this machine: {e}\n\
             (the test writes HID output reports to the virtual pad; a tool that hides or \
             locks HID devices, such as HidHide, can prevent that)"
        );
    }
    drop(warm_up);
    thread::sleep(Duration::from_millis(300));

    let baseline_handles = process_handle_count();
    let (baseline_ws, baseline_pf) = process_memory();

    let tap = {
        let shared = Arc::clone(&shared);
        BusTap::start(move |_serial, report| shared.tap(report)).expect("start bare listener")
    };
    // Long-lived pads + one churn pad + headroom.
    let client = Client::builder()
        .max_targets(pads + 4)
        .connect()
        .expect("connect");

    let t0 = Instant::now();
    let (done_tx, done_rx) = mpsc::channel();
    let mut actors = Vec::new();
    for id in 0..u64::from(pads) {
        let client = client.clone();
        let name = format!("long-lived pad {id}");
        actors.push(spawn_actor(&name, &shared, &done_tx, move |shared| {
            long_lived_pad(&client, shared, seed, id);
        }));
    }
    {
        let client = client.clone();
        actors.push(spawn_actor("churn", &shared, &done_tx, move |shared| {
            churn_loop(&client, shared, Rng::new(seed, 1000));
        }));
    }
    actors.push(spawn_actor(
        "second client",
        &shared,
        &done_tx,
        move |shared| second_client_loop(shared, Rng::new(seed, 2000)),
    ));

    // --- sample handles while the chaos runs ---------------------------------
    let mut max_handles = baseline_handles;
    let mut handle_samples = vec![baseline_handles];
    let run_until = t0 + Duration::from_secs(u64::from(seconds));
    while Instant::now() < run_until {
        thread::sleep(Duration::from_millis(250));
        let h = process_handle_count();
        max_handles = max_handles.max(h);
        handle_samples.push(h);
    }
    shared.stop.store(true, Ordering::Relaxed);

    // --- wait for every actor; a missing one is a deadlock -------------------
    let mut running: Vec<&str> = actors.iter().map(|(name, _)| name.as_str()).collect();
    let join_until = Instant::now() + JOIN_TIMEOUT;
    while !running.is_empty() {
        let left = join_until.saturating_duration_since(Instant::now());
        match done_rx.recv_timeout(left) {
            Ok(name) => running.retain(|r| *r != name),
            Err(_) => panic!(
                "ds4 chaos FAILED: still running {JOIN_TIMEOUT:?} after stop (deadlock?): \
                 {running:?}  (seed {seed})\nthreads:\n{}",
                shared.activity_report()
            ),
        }
    }
    for (name, handle) in actors {
        if handle.join().is_err() {
            shared.fail(format!("{name} panicked"));
        }
    }

    drop(client);
    drop(tap);
    thread::sleep(Duration::from_millis(300));

    let end_handles = process_handle_count();
    let (end_ws, end_pf) = process_memory();
    let elapsed = t0.elapsed();

    let count = |c: &AtomicU64| c.load(Ordering::Relaxed);
    let failures = count(&shared.failures);
    let failure_log = shared
        .failure_log
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();

    let kept_written = count(&shared.kept_written);
    let kept_lost = kept_written.saturating_sub(count(&shared.kept_received));
    let tap_lost = kept_written.saturating_sub(count(&shared.tap_received));
    let (kept_disorder, tap_disorder) = (count(&shared.kept_disorder), count(&shared.tap_disorder));
    let (typed_tagged, typed_disorder) =
        (count(&shared.typed_tagged), count(&shared.typed_disorder));
    let (burst_written, burst_received) =
        (count(&shared.burst_written), count(&shared.burst_received));

    let end_delta = i64::from(end_handles) - i64::from(baseline_handles);
    let peak_delta = i64::from(max_handles) - i64::from(baseline_handles);
    let pf_growth = end_pf.saturating_sub(baseline_pf);

    // The run must have exercised what it claims to check.
    let pass_coverage = count(&shared.deliveries_checked) > 0
        && count(&shared.disconnects_checked) > 0
        && kept_written > 0
        && count(&shared.tap_received) > 0
        && typed_tagged > 0;
    let pass_failures = failures == 0;
    let pass_loss = kept_lost <= tap_lost * 2 + kept_written / 100;
    let pass_raw_order = kept_disorder <= tap_disorder * 2 + 10;
    let pass_typed_order =
        typed_disorder * 100 <= (typed_tagged + typed_disorder) * TYPED_DISORDER_LIMIT_PERCENT;
    let pass_end = end_delta <= HANDLE_END_SLACK;
    let pass_trend = handle_trend_ok(&handle_samples, HANDLE_TREND_SLACK);
    let pass_memory = pf_growth <= PAGEFILE_GROWTH_SLACK;
    let pass = pass_coverage
        && pass_failures
        && pass_loss
        && pass_raw_order
        && pass_typed_order
        && pass_end
        && pass_trend
        && pass_memory;

    let shown: Vec<u32> = handle_samples.iter().copied().take(64).collect();
    let mut report = Report::new("ds4_output_chaos")
        .line(
            "config:",
            format!("{pads} long-lived pads + churn + second client, {seconds}s, seed {seed}"),
        )
        .line(
            "env:",
            "VIGEM_STRESS_PADS / SECONDS / SEED (defaults 3 / 10 / clock)",
        )
        .line("elapsed:", format!("{elapsed:?}"))
        .line(
            "pads / clients:",
            format!(
                "{} pads plugged, {} second clients, {} subscriptions",
                count(&shared.pads_plugged),
                count(&shared.clients_opened),
                count(&shared.subscriptions)
            ),
        )
        .line("reports written:", count(&shared.written))
        .line(
            "raw received:",
            format!(
                "{} tagged, {} repeated or out of order, {} from host software",
                count(&shared.raw_tagged),
                count(&shared.raw_disorder),
                count(&shared.raw_host)
            ),
        )
        .line(
            "typed received:",
            format!(
                "{typed_tagged} tagged, {typed_disorder} out of order ({}), {} from host software",
                percent(typed_disorder, typed_tagged + typed_disorder),
                count(&shared.typed_host)
            ),
        )
        .line(
            "checked:",
            format!(
                "{} deliveries after subscribe, {} disconnects",
                count(&shared.deliveries_checked),
                count(&shared.disconnects_checked)
            ),
        )
        .line(
            "long-lived raw:",
            format!(
                "{kept_written} written; permanent receivers lost {kept_lost} ({}), \
                 {kept_disorder} repeated or out of order",
                percent(kept_lost, kept_written)
            ),
        )
        .line(
            "bare listener:",
            format!(
                "same reports, no crate code: lost {tap_lost} ({}), \
                 {tap_disorder} repeated or out of order",
                percent(tap_lost, kept_written)
            ),
        )
        .line(
            "bursts:",
            format!(
                "{burst_received}/{burst_written} arrived on churn and second-client pads \
                 (lost {})",
                percent(burst_written.saturating_sub(burst_received), burst_written)
            ),
        )
        .line("failures:", failures);
    for (i, line) in failure_log.iter().enumerate() {
        report = report.line(format!("  [{i}]"), line);
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
        .line("handles samples (<=64):", format!("{shown:?}"))
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
                end_pf as i64 - baseline_pf as i64
            ),
        )
        .line(
            "limits:",
            format!(
                "failures=0, raw loss<=2x bare+1%, raw disorder<=2x bare+10, typed disorder<=\
                 {TYPED_DISORDER_LIMIT_PERCENT}%, end delta<={HANDLE_END_SLACK}, trend slack=\
                 {HANDLE_TREND_SLACK}, pagefile growth<={}",
                format_bytes(PAGEFILE_GROWTH_SLACK)
            ),
        )
        .line("result:", if pass { "PASS" } else { "FAIL" })
        .print();

    assert!(
        pass,
        "ds4 chaos FAILED (seed {seed}: failures={failures}, coverage_ok={pass_coverage}, \
         raw_loss_ok={pass_loss}, raw_order_ok={pass_raw_order}, \
         typed_order_ok={pass_typed_order}, end_delta={end_delta}, trend_ok={pass_trend}, \
         pagefile_growth={})",
        format_bytes(pf_growth)
    );
}
