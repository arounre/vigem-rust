//! Shared helpers for driver-required and stress integration tests.
//!
//! Not auto-discovered as a test target (only `tests/*.rs` are). Included via
//! `mod common;` from each integration test that needs it.
//!
//! `#![allow(dead_code)]` - not every test binary uses every helper.

#![allow(dead_code)]

use std::env;
use std::fmt::Write as _;
use std::sync::{Mutex, MutexGuard, OnceLock};

use vigem_rust::Client;
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};

// ---------------------------------------------------------------------------
// Driver serialization
// ---------------------------------------------------------------------------

/// Process-wide lock so driver / stress tests do not race ViGEm serials.
///
/// Cargo runs integration test **binaries** sequentially within one `cargo test`
/// invocation, so a process-wide mutex is enough. Running two `cargo test`
/// processes against the same machine at once is **not** supported (they would
/// share the driver namespace without a cross-process lock).
pub fn driver_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Connect or print a standard skip line and return `None`.
pub fn try_client() -> Option<Client> {
    match Client::connect() {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("skip: no driver ({e})");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Process metrics
// ---------------------------------------------------------------------------

pub fn process_handle_count() -> u32 {
    let mut count = 0u32;
    // SAFETY: current process pseudo-handle; out-param is a stack u32.
    unsafe {
        GetProcessHandleCount(GetCurrentProcess(), &mut count).expect("GetProcessHandleCount");
    }
    count
}

/// Private working set and pagefile (commit) usage in bytes.
pub fn process_memory() -> (usize, usize) {
    let mut counters = PROCESS_MEMORY_COUNTERS::default();
    // SAFETY: current process; counters is a valid out buffer of known size.
    unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut counters,
            std::mem::size_of_val(&counters) as u32,
        )
        .expect("GetProcessMemoryInfo");
    }
    (counters.WorkingSetSize, counters.PagefileUsage)
}

pub fn env_u32(key: &str, default: u32) -> u32 {
    env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

pub fn format_bytes(n: usize) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    let n = n as f64;
    if n >= MIB {
        format!("{:.2} MiB", n / MIB)
    } else if n >= KIB {
        format!("{:.1} KiB", n / KIB)
    } else {
        format!("{n} B")
    }
}

/// True if the last handle sample is not above the median of the first half
/// plus `slack` (catches a ratcheting leak better than end-point alone).
pub fn handle_trend_ok(samples: &[u32], slack: i64) -> bool {
    if samples.len() < 2 {
        return true;
    }
    let half = (samples.len() / 2).max(1);
    let mut first: Vec<u32> = samples[..half].to_vec();
    first.sort_unstable();
    let median = first[first.len() / 2];
    let last = *samples.last().expect("len >= 2");
    i64::from(last) <= i64::from(median) + slack
}

// ---------------------------------------------------------------------------
// Aligned summary blocks
// ---------------------------------------------------------------------------

/// Key/value lines for stress-test PASS/FAIL summaries (`key:  value`).
#[derive(Default)]
pub struct Report {
    title: String,
    lines: Vec<(String, String)>,
}

impl Report {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            lines: Vec::new(),
        }
    }

    pub fn line(mut self, key: impl Into<String>, value: impl std::fmt::Display) -> Self {
        self.lines.push((key.into(), value.to_string()));
        self
    }

    pub fn print(&self) {
        let width = self
            .lines
            .iter()
            .map(|(k, _)| k.len())
            .max()
            .unwrap_or(0)
            .max(1);
        println!();
        println!("=== {} ===", self.title);
        for (k, v) in &self.lines {
            let mut line = String::new();
            let _ = write!(line, "{k:<width$}");
            println!("{line}  {v}");
        }
        println!("=========================================");
    }
}
