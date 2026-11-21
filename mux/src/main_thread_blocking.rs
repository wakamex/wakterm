//! Names the blocking work that holds the mux main thread.
//!
//! Every client request and pane update waits while the main thread reads
//! `/proc`, queries a database or waits on a file lock. Code that does such
//! work holds a `Blocking` guard for its duration; when the guard ends on
//! the main thread after `SLOW` or longer, it logs what the work was, where
//! it was started, and how long it took, at most once per `REPEAT` for each
//! place.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::panic::Location;
use std::time::{Duration, Instant};

/// Shorter work on the main thread is not reported.
const SLOW: Duration = Duration::from_millis(5);
/// The least time between two reports from the same place.
const REPEAT: Duration = Duration::from_secs(60);

/// When each place last reported, and how many reports it held back since.
static REPORTED: Mutex<Option<HashMap<&'static Location<'static>, (Instant, u64)>>> =
    Mutex::new(None);

/// Times blocking work until dropped; see the module documentation.
pub struct Blocking {
    what: &'static str,
    location: &'static Location<'static>,
    started: Instant,
}

/// Starts timing blocking work described by `what`, such as "reading a
/// pane's process tree", at the caller's location.
#[track_caller]
pub fn blocking(what: &'static str) -> Blocking {
    Blocking {
        what,
        location: Location::caller(),
        started: Instant::now(),
    }
}

impl Drop for Blocking {
    fn drop(&mut self) {
        let took = self.started.elapsed();
        if took < SLOW || !crate::Mux::try_get().is_some_and(|mux| mux.is_main_thread()) {
            return;
        }
        if let Some(held_back) = should_report(self.location, Instant::now()) {
            log::warn!(
                "the main thread spent {} ms {} at {}{}",
                took.as_millis(),
                self.what,
                self.location,
                if held_back > 0 {
                    format!(", after {held_back} more such waits there since its last report")
                } else {
                    String::new()
                }
            );
        }
    }
}

/// Whether `location` may report at `now`, and how many reports it held
/// back since it last did.
fn should_report(location: &'static Location<'static>, now: Instant) -> Option<u64> {
    let mut reported = REPORTED.lock();
    let entry = reported
        .get_or_insert_with(HashMap::new)
        .entry(location)
        .or_insert((now - REPEAT, 0));
    if now.duration_since(entry.0) < REPEAT {
        entry.1 += 1;
        return None;
    }
    let held_back = entry.1;
    *entry = (now, 0);
    Some(held_back)
}

/// A value, such as a database connection, used for blocking work, timed
/// as one piece of work for as long as it is held.
pub struct Timed<T> {
    value: T,
    _blocking: Blocking,
}

/// Times `value` as blocking work described by `what` until it is dropped.
#[track_caller]
pub fn timed<T>(what: &'static str, value: T) -> Timed<T> {
    Timed {
        value,
        _blocking: blocking(what),
    }
}

impl<T> std::ops::Deref for Timed<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T> std::ops::DerefMut for Timed<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.value
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn each_place_reports_at_most_once_per_repeat_and_counts_the_rest() {
        let (here, there) = (Location::caller(), Location::caller());
        let start = Instant::now();
        assert_eq!(should_report(here, start), Some(0));
        assert_eq!(should_report(here, start + Duration::from_secs(1)), None);
        assert_eq!(should_report(here, start + Duration::from_secs(2)), None);
        // Another place reports on its own schedule.
        assert_eq!(
            should_report(there, start + Duration::from_secs(2)),
            Some(0)
        );
        // Once the repeat interval has passed, the next report counts the
        // waits it held back.
        assert_eq!(should_report(here, start + REPEAT), Some(2));
        assert_eq!(should_report(here, start + REPEAT), None);
    }
}
