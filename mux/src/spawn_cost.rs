//! Counts what spawning pane processes costs the thread that spawns them,
//! so the restore at startup can report it: a process is forked from the
//! mux, whose size and threads make each fork dearer.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static SPAWNS: AtomicU64 = AtomicU64::new(0);
static NANOS: AtomicU64 = AtomicU64::new(0);
static MINOR_FAULTS: AtomicU64 = AtomicU64::new(0);

/// The minor page faults the calling thread has taken, where known.
fn thread_minor_faults() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        if unsafe { libc::getrusage(libc::RUSAGE_THREAD, &mut usage) } == 0 {
            return Some(usage.ru_minflt as u64);
        }
    }
    None
}

/// Runs `spawn` and counts what it cost.
pub fn measure<T>(spawn: impl FnOnce() -> T) -> T {
    let faults_before = thread_minor_faults();
    let started = Instant::now();
    let spawned = spawn();
    NANOS.fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
    if let (Some(before), Some(after)) = (faults_before, thread_minor_faults()) {
        MINOR_FAULTS.fetch_add(after.saturating_sub(before), Ordering::Relaxed);
    }
    SPAWNS.fetch_add(1, Ordering::Relaxed);
    spawned
}

/// The spawns counted since the last call, with their total time and
/// minor page faults, and starts counting afresh.
pub fn take() -> (u64, Duration, u64) {
    (
        SPAWNS.swap(0, Ordering::Relaxed),
        Duration::from_nanos(NANOS.swap(0, Ordering::Relaxed)),
        MINOR_FAULTS.swap(0, Ordering::Relaxed),
    )
}
