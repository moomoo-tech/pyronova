//! GIL Watchdog — probe-based GIL contention monitor.
//!
//! A background Rust thread attempts to acquire the main GIL every 10ms
//! and records how long it took. If a handler holds the GIL for too long
//! (e.g. numpy without releasing GIL), the watchdog barks.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use pyo3::prelude::*;

/// Last GIL probe latency (microseconds for precision)
pub static GIL_LATENCY_LAST_US: AtomicU64 = AtomicU64::new(0);
/// Peak GIL probe latency since last reset (microseconds)
pub static GIL_LATENCY_MAX_US: AtomicU64 = AtomicU64::new(0);
/// Total probe count
pub static GIL_PROBE_COUNT: AtomicU64 = AtomicU64::new(0);
/// Total accumulated wait (microseconds)
pub static GIL_TOTAL_WAIT_US: AtomicU64 = AtomicU64::new(0);

/// Memory RSS in bytes (updated by watchdog)
pub static MEMORY_RSS_BYTES: AtomicU64 = AtomicU64::new(0);

/// Number of threads currently waiting to acquire the main GIL
pub static GIL_QUEUE_LENGTH: std::sync::atomic::AtomicIsize =
    std::sync::atomic::AtomicIsize::new(0);
/// Peak business handler GIL hold time (microseconds, reset on read)
pub static GIL_HOLD_MAX_US: AtomicU64 = AtomicU64::new(0);

/// Requests dropped due to backpressure (503 overloaded)
pub static DROPPED_REQUESTS: AtomicU64 = AtomicU64::new(0);
/// Total requests processed
pub static TOTAL_REQUESTS: AtomicU64 = AtomicU64::new(0);

/// Spawn the GIL watchdog background thread.
pub fn spawn_gil_watchdog() {
    std::thread::Builder::new()
        .name("pyre-watchdog".to_string())
        .spawn(|| {
            loop {
                let start = Instant::now();

                // Probe: try to acquire the main GIL
                Python::attach(|_py| {
                    // Got it — do nothing, release immediately
                });

                let elapsed_us = start.elapsed().as_micros() as u64;

                GIL_LATENCY_LAST_US.store(elapsed_us, Ordering::Relaxed);
                GIL_PROBE_COUNT.fetch_add(1, Ordering::Relaxed);
                GIL_TOTAL_WAIT_US.fetch_add(elapsed_us, Ordering::Relaxed);
                GIL_LATENCY_MAX_US.fetch_max(elapsed_us, Ordering::Relaxed);

                // Alert if GIL blocked > 50ms
                if elapsed_us > 50_000 {
                    tracing::warn!(
                        target: "pyre::server",
                        latency_ms = elapsed_us / 1000,
                        "GIL watchdog: main GIL congested"
                    );
                }

                // Sample memory RSS
                MEMORY_RSS_BYTES.store(get_rss_bytes(), Ordering::Relaxed);

                // Probe interval: 10ms (~100 samples/sec)
                std::thread::sleep(Duration::from_millis(10));
            }
        })
        .expect("failed to spawn GIL watchdog");
}

/// Get current process RSS in bytes (platform-specific, zero dependencies).
fn get_rss_bytes() -> u64 {
    #[cfg(target_os = "macos")]
    {
        use std::mem;
        unsafe {
            let mut info: libc_mach_task_basic_info = mem::zeroed();
            let mut count = (mem::size_of::<libc_mach_task_basic_info>() / 4) as u32;
            let kr = mach_task_self_info(&mut info, &mut count);
            if kr == 0 {
                return info.resident_size;
            }
        }
        0
    }
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/self/statm")
            .ok()
            .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok())
            .map(|pages| pages * 4096)
            .unwrap_or(0)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        0
    }
}

// macOS: minimal FFI for task_info (avoids libc crate dependency)
#[cfg(target_os = "macos")]
#[repr(C)]
struct libc_mach_task_basic_info {
    virtual_size: u64,
    resident_size: u64,
    resident_size_max: u64,
    user_time: [u32; 2],
    system_time: [u32; 2],
    policy: i32,
    suspend_count: i32,
}

#[cfg(target_os = "macos")]
unsafe fn mach_task_self_info(info: &mut libc_mach_task_basic_info, count: &mut u32) -> i32 {
    extern "C" {
        fn mach_task_self() -> u32;
        fn task_info(task: u32, flavor: u32, info: *mut u8, count: *mut u32) -> i32;
    }
    // MACH_TASK_BASIC_INFO = 20
    task_info(mach_task_self(), 20, info as *mut _ as *mut u8, count)
}

// ---------------------------------------------------------------------------
// Python-facing metrics API
// ---------------------------------------------------------------------------

/// Get all metrics. Returns tuple:
/// (last_us, peak_us, probe_count, total_wait_us, rss_bytes,
///  queue_len, hold_peak_us, dropped_requests, total_requests)
/// Resets peaks after read.
#[pyfunction]
pub fn get_gil_metrics() -> (u64, u64, u64, u64, u64, isize, u64, u64, u64) {
    let last = GIL_LATENCY_LAST_US.load(Ordering::Relaxed);
    let peak = GIL_LATENCY_MAX_US.swap(0, Ordering::Relaxed);
    let count = GIL_PROBE_COUNT.load(Ordering::Relaxed);
    let total = GIL_TOTAL_WAIT_US.load(Ordering::Relaxed);
    let rss = MEMORY_RSS_BYTES.load(Ordering::Relaxed);
    let queue = GIL_QUEUE_LENGTH.load(std::sync::atomic::Ordering::Relaxed);
    let hold_peak = GIL_HOLD_MAX_US.swap(0, Ordering::Relaxed);
    let dropped = DROPPED_REQUESTS.load(Ordering::Relaxed);
    let total_req = TOTAL_REQUESTS.load(Ordering::Relaxed);
    (
        last, peak, count, total, rss, queue, hold_peak, dropped, total_req,
    )
}
