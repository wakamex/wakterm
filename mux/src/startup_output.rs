//! Times how long the panes a mux restores at startup take to print their
//! first output and to draw a first screen, so a slow restart can be told
//! apart from slow programs. Each pane's reader thread records its times as
//! the output arrives, so a busy main thread does not delay them.

use crate::pane::PaneId;
use crate::Mux;
use parking_lot::Mutex;
use std::time::{Duration, Instant};

/// Output this long is taken as a first screen drawn: an agent's terminal
/// queries and mode switches come first and are much shorter.
pub const SCREEN_BYTES: u64 = 4096;
/// Panes that have not drawn a screen by then are reported as silent.
const REPORT_AFTER: Duration = Duration::from_secs(180);

struct Tracker {
    began: Instant,
    /// The number of panes restored, once the restore has finished.
    expected: Option<usize>,
    /// Each pane's first output and first screen, after it started.
    panes: Vec<(PaneId, Duration, Duration)>,
}

static TRACKER: Mutex<Option<Tracker>> = Mutex::new(None);

/// Starts timing the panes that start from now on, as a restore begins.
pub fn begin() {
    *TRACKER.lock() = Some(Tracker {
        began: Instant::now(),
        expected: None,
        panes: Vec::new(),
    });
    std::thread::Builder::new()
        .name("startup-output".to_string())
        .spawn(|| {
            std::thread::sleep(REPORT_AFTER);
            report();
        })
        .ok();
}

/// Notes that the restore started `panes` panes, and reports once all of
/// them have drawn a screen.
pub fn expect(panes: usize) {
    let done = TRACKER.lock().as_mut().is_some_and(|tracker| {
        tracker.expected = Some(panes);
        tracker.panes.len() >= panes
    });
    if done {
        report();
    }
}

/// Records that a pane printed its first output `first` after it started
/// and drew a first screen `screen` after it started.
pub fn record(pane_id: PaneId, first: Duration, screen: Duration) {
    crate::agent_start_gate::screen_drawn();
    let done = TRACKER.lock().as_mut().is_some_and(|tracker| {
        tracker.panes.push((pane_id, first, screen));
        tracker
            .expected
            .is_some_and(|expected| tracker.panes.len() >= expected)
    });
    if done {
        report();
    }
}

fn report() {
    let Some(tracker) = TRACKER.lock().take() else {
        return;
    };
    let mut panes = tracker.panes;
    if panes.is_empty() {
        return;
    }
    let median = |mut times: Vec<Duration>| {
        times.sort();
        times[times.len() / 2].as_secs_f64()
    };
    let first = median(panes.iter().map(|(_, first, _)| *first).collect());
    let screen = median(panes.iter().map(|(_, _, screen)| *screen).collect());
    panes.sort_by(|a, b| b.2.cmp(&a.2));
    let name = |pane_id: PaneId| {
        Mux::try_get()
            .and_then(|mux| {
                mux.get_agent_metadata_for_pane(pane_id)
                    .map(|metadata| metadata.name.clone())
                    .or_else(|| mux.get_pane(pane_id).map(|pane| pane.get_title()))
            })
            .unwrap_or_else(|| format!("pane {pane_id}"))
    };
    let slowest = panes
        .iter()
        .take(5)
        .map(|(pane_id, _, screen)| format!("{} {:.1} s", name(*pane_id), screen.as_secs_f64()))
        .collect::<Vec<_>>()
        .join(", ");
    let silent = tracker
        .expected
        .map(|expected| expected.saturating_sub(panes.len()))
        .unwrap_or(0);
    log::info!(
        "startup: {} restored panes drew a first screen within {:.1} s of the restore beginning; \
         median {first:.1} s to first output and {screen:.1} s to a first screen after each \
         started; slowest {slowest}; {silent} still without a screen",
        panes.len(),
        tracker.began.elapsed().as_secs_f64(),
    );
}
