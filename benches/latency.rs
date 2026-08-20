//! Per-`update` latency (Criterion only).
//!
//! Requires ViGEmBus. Not run in CI. Handle flatness lives in
//! `tests/stress.rs::soak_update_handles` (ignored stress test).
//!
//! ```text
//! cargo bench --bench latency
//! cargo bench --bench latency -- x360_update
//! # ds4_update needs the ds4 feature (on by default with x360):
//! cargo bench --bench latency --all-features
//! # Without x360 the bench target is skipped (required-features):
//! cargo bench --bench latency --no-default-features --features ds4
//! ```
//!
//! - `event_pair_only`: CreateEventW+CloseHandle baseline
//! - `x360_update` / `ds4_update`: full `TargetHandle::update`

#![cfg(windows)]

use std::hint::black_box;
use std::sync::OnceLock;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Threading::CreateEventW;

use vigem_rust::{Client, X360Report};

#[cfg(feature = "ds4")]
use vigem_rust::DualShock4;
#[cfg(feature = "ds4")]
use vigem_rust::{Ds4Report, TargetHandle};

/// One bus connection for all pad benches (x360 + optional ds4 share it).
fn shared_client() -> Option<&'static Client> {
    static CLIENT: OnceLock<Option<Client>> = OnceLock::new();
    CLIENT
        .get_or_init(|| match Client::connect() {
            Ok(c) => Some(c),
            Err(e) => {
                eprintln!("skip pad benches: ViGEmBus not available ({e})");
                None
            }
        })
        .as_ref()
}

/// Pure Win32: one auto-reset event create + close (no DeviceIoControl).
fn bench_event_pair_only(c: &mut Criterion) {
    c.bench_function("event_pair_only", |b| {
        b.iter(|| {
            // SAFETY: unnamed auto-reset event; close the returned handle.
            let h: HANDLE =
                unsafe { CreateEventW(None, false, false, None) }.expect("CreateEventW");
            unsafe {
                CloseHandle(h).expect("CloseHandle");
            }
            black_box(h);
        });
    });
}

/// Full path: `TargetHandle::update` -> overlapped `DeviceIoControl` submit report.
fn bench_x360_update(c: &mut Criterion) {
    let Some(client) = shared_client() else {
        return;
    };
    let pad = match client
        .new_x360_target()
        .plug()
        .and_then(|p| p.wait_for_ready())
    {
        Ok(p) => p,
        Err(e) => {
            eprintln!("skip x360_update: plug/ready failed ({e})");
            return;
        }
    };

    let report = X360Report::default();
    let mut group = c.benchmark_group("x360_update");
    group.throughput(Throughput::Elements(1));
    group.bench_function("default_report", |b| {
        b.iter(|| {
            pad.update(black_box(&report))
                .expect("update should succeed while target is live");
        });
    });
    group.finish();

    // Keep the pad alive until the group finishes (drop order).
    drop(pad);
}

#[cfg(feature = "ds4")]
fn bench_ds4_update(c: &mut Criterion) {
    let Some(client) = shared_client() else {
        return;
    };
    let pad: TargetHandle<DualShock4> = match client
        .new_ds4_target()
        .plug()
        .and_then(|p| p.wait_for_ready())
    {
        Ok(p) => p,
        Err(e) => {
            eprintln!("skip ds4_update: plug/ready failed ({e})");
            return;
        }
    };

    let report = Ds4Report::default();
    let mut group = c.benchmark_group("ds4_update");
    group.throughput(Throughput::Elements(1));
    group.bench_function("default_report", |b| {
        b.iter(|| {
            pad.update(black_box(&report))
                .expect("update should succeed while target is live");
        });
    });
    group.finish();
    drop(pad);
}

fn benches(c: &mut Criterion) {
    bench_event_pair_only(c);
    bench_x360_update(c);
    #[cfg(feature = "ds4")]
    bench_ds4_update(c);
}

criterion_group!(latency_benches, benches);
criterion_main!(latency_benches);
