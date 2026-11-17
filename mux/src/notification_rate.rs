//! Detects a mux notification that keeps firing for the same window, tab,
//! or pane, the sign of a feedback loop between a notification and a
//! handler that answers it by changing the state again.

use crate::MuxNotification;
use std::collections::{HashMap, VecDeque};
use std::panic::Location;
use std::time::{Duration, Instant};

/// More notifications of one kind for one object than this within
/// `PERIOD` are reported.
const LIMIT: u32 = 200;
const PERIOD: Duration = Duration::from_secs(1);
/// A key keeps being reported at most this often.
const REPORT_INTERVAL: Duration = Duration::from_secs(60);
/// How many of the latest notifications a report lists.
const RECENT: usize = 16;
/// Past this many keys, keys idle for a whole period are dropped.
const PRUNE_ABOVE: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct NotificationKey {
    kind: &'static str,
    id: u64,
}

impl NotificationKey {
    /// The key of a notification about the state of one window, tab, or
    /// pane, which changes at the pace of user actions and agent turns.
    /// Output, alerts, and the other notifications that legitimately
    /// stream are not tracked.
    pub(crate) fn of(notification: &MuxNotification) -> Option<Self> {
        let (kind, id) = match notification {
            MuxNotification::WindowInvalidated(window_id) => ("WindowInvalidated", *window_id),
            MuxNotification::WindowWorkspaceChanged(window_id) => {
                ("WindowWorkspaceChanged", *window_id)
            }
            MuxNotification::WindowTitleChanged { window_id, .. } => {
                ("WindowTitleChanged", *window_id)
            }
            MuxNotification::TabOrderChanged { window_id, .. } => ("TabOrderChanged", *window_id),
            MuxNotification::ParkedTabsChanged { window_id, .. } => {
                ("ParkedTabsChanged", *window_id)
            }
            MuxNotification::TabAddedToWindow { tab_id, .. } => ("TabAddedToWindow", *tab_id),
            MuxNotification::TabResized { tab_id, .. } => ("TabResized", *tab_id),
            MuxNotification::TabTitleChanged { tab_id, .. } => ("TabTitleChanged", *tab_id),
            MuxNotification::PaneFocused(pane_id) => ("PaneFocused", *pane_id),
            MuxNotification::AgentMetadataChanged { pane_id, .. } => {
                ("AgentMetadataChanged", *pane_id)
            }
            _ => return None,
        };
        Some(Self {
            kind,
            id: id as u64,
        })
    }
}

struct KeyRate {
    period_start: Instant,
    count: u32,
    reported_at: Option<Instant>,
}

#[derive(Default)]
pub(crate) struct NotificationRate {
    keys: HashMap<NotificationKey, KeyRate>,
    recent: VecDeque<(NotificationKey, &'static Location<'static>)>,
}

impl NotificationRate {
    /// Counts a notification sent from `sender` at `now`, and returns a
    /// report when its key has just exceeded the limit.
    pub(crate) fn record(
        &mut self,
        notification: &MuxNotification,
        sender: &'static Location<'static>,
        now: Instant,
    ) -> Option<String> {
        let key = NotificationKey::of(notification)?;
        if self.recent.len() == RECENT {
            self.recent.pop_front();
        }
        self.recent.push_back((key, sender));
        if self.keys.len() > PRUNE_ABOVE {
            self.keys
                .retain(|_, rate| now.duration_since(rate.period_start) < PERIOD);
        }

        let rate = self.keys.entry(key).or_insert(KeyRate {
            period_start: now,
            count: 0,
            reported_at: None,
        });
        if now.duration_since(rate.period_start) >= PERIOD {
            rate.period_start = now;
            rate.count = 0;
        }
        rate.count += 1;
        if rate.count != LIMIT + 1
            || rate
                .reported_at
                .is_some_and(|at| now.duration_since(at) < REPORT_INTERVAL)
        {
            return None;
        }
        rate.reported_at = Some(now);

        let mut runs: Vec<(&NotificationKey, &Location, usize)> = vec![];
        for (key, sender) in &self.recent {
            match runs.last_mut() {
                Some((last_key, last_sender, count))
                    if *last_key == key && *last_sender == *sender =>
                {
                    *count += 1
                }
                _ => runs.push((key, *sender, 1)),
            }
        }
        let recent = runs
            .iter()
            .map(|(key, sender, count)| {
                let times = if *count > 1 {
                    format!(" ({count} times)")
                } else {
                    String::new()
                };
                format!("\n  {} {} from {sender}{times}", key.kind, key.id)
            })
            .collect::<String>();
        Some(format!(
            "{} for {} fired more than {LIMIT} times within {PERIOD:?}, \
             which suggests a feedback loop between it and a handler. \
             Latest notifications:{recent}",
            key.kind, key.id
        ))
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn title(tab_id: usize) -> MuxNotification {
        MuxNotification::TabTitleChanged {
            tab_id,
            title: String::new(),
        }
    }

    fn reports(
        rate: &mut NotificationRate,
        notification: &MuxNotification,
        times: u32,
        at: Instant,
    ) -> Vec<String> {
        (0..times)
            .filter_map(|_| rate.record(notification, Location::caller(), at))
            .collect()
    }

    #[test]
    fn a_key_past_the_limit_is_reported_once_with_its_senders() {
        let mut rate = NotificationRate::default();
        let start = Instant::now();
        assert!(reports(&mut rate, &title(90), LIMIT, start).is_empty());

        let report = reports(&mut rate, &title(90), 1, start);
        assert_eq!(report.len(), 1);
        assert!(report[0].starts_with("TabTitleChanged for 90 fired more than 200 times"));
        // The sender's location, written with the platform's separator.
        assert!(report[0].contains(&format!("TabTitleChanged 90 from {}:", file!())));
        assert!(report[0].ends_with(&format!(" ({RECENT} times)")));

        // Still within the reporting interval, a new burst stays quiet.
        let later = start + PERIOD * 2;
        assert!(reports(&mut rate, &title(90), LIMIT + 1, later).is_empty());
        let much_later = start + REPORT_INTERVAL;
        assert_eq!(
            reports(&mut rate, &title(90), LIMIT + 1, much_later).len(),
            1
        );
    }

    #[test]
    fn the_count_restarts_each_period_and_for_each_object() {
        let mut rate = NotificationRate::default();
        let start = Instant::now();
        assert!(reports(&mut rate, &title(90), LIMIT, start).is_empty());
        assert!(reports(&mut rate, &title(91), LIMIT, start).is_empty());
        assert!(reports(&mut rate, &title(90), LIMIT, start + PERIOD).is_empty());
    }

    #[test]
    fn streaming_notifications_are_not_counted() {
        let mut rate = NotificationRate::default();
        let start = Instant::now();
        let output = MuxNotification::PaneOutput(1);
        assert!(reports(&mut rate, &output, LIMIT * 10, start).is_empty());
        assert!(rate.recent.is_empty());
    }
}
