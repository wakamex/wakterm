//! Reminders that the mux delivers to an agent at a time or on a repeat.
//!
//! Agents schedule their own wakeups through these instead of timers inside
//! the harness, so that a wakeup always arrives as input through Wakterm,
//! which wakes a frozen agent before typing into it.

use chrono::{DateTime, Duration, Utc};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::convert::TryFrom;
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentReminder {
    pub id: String,
    pub agent_id: String,
    pub message: String,
    /// When the reminder is next delivered.
    pub due_at: DateTime<Utc>,
    /// The interval of a recurring reminder, in seconds.
    pub every_seconds: Option<u64>,
    pub created_at: DateTime<Utc>,
    /// Why the latest delivery attempt failed, if it did.
    pub last_error: Option<String>,
}

impl AgentReminder {
    /// The reminder after a delivery at `now`: a recurring one moves to its
    /// next time, skipping times that passed while it could not be
    /// delivered, so a missed run is delivered once rather than replayed.
    pub fn after_delivery(&self, now: DateTime<Utc>) -> Option<Self> {
        let every = Duration::seconds(i64::try_from(self.every_seconds?).ok()?.max(1));
        let mut due_at = self.due_at + every;
        if due_at <= now {
            due_at = now + every;
        }
        Some(Self {
            due_at,
            last_error: None,
            ..self.clone()
        })
    }
}

#[derive(Clone)]
pub struct AgentReminderStore {
    path: PathBuf,
}

impl AgentReminderStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn connect(&self) -> anyhow::Result<Connection> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&self.path)?;
        conn.busy_timeout(std::time::Duration::from_secs(2))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS agent_reminder_v1 (
                 id TEXT PRIMARY KEY,
                 due_at TEXT NOT NULL,
                 record_json TEXT NOT NULL
             );",
        )?;
        Ok(conn)
    }

    /// Adds or replaces a reminder.
    pub fn save(&self, reminder: &AgentReminder) -> anyhow::Result<()> {
        self.connect()?.execute(
            "INSERT OR REPLACE INTO agent_reminder_v1(id, due_at, record_json)
             VALUES (?1, ?2, ?3)",
            params![
                reminder.id,
                reminder.due_at.to_rfc3339(),
                serde_json::to_string(reminder)?
            ],
        )?;
        Ok(())
    }

    /// Every reminder, soonest first.
    pub fn list(&self) -> anyhow::Result<Vec<AgentReminder>> {
        let conn = self.connect()?;
        let mut statement =
            conn.prepare("SELECT record_json FROM agent_reminder_v1 ORDER BY due_at, id")?;
        let reminders = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .map(|json| Ok(serde_json::from_str(&json?)?))
            .collect::<anyhow::Result<Vec<AgentReminder>>>()?;
        Ok(reminders)
    }

    /// The reminders due at `now`, soonest first.
    pub fn due(&self, now: DateTime<Utc>) -> anyhow::Result<Vec<AgentReminder>> {
        Ok(self
            .list()?
            .into_iter()
            .take_while(|reminder| reminder.due_at <= now)
            .collect())
    }

    /// Removes a reminder, returning whether it existed.
    pub fn cancel(&self, id: &str) -> anyhow::Result<bool> {
        Ok(self
            .connect()?
            .execute("DELETE FROM agent_reminder_v1 WHERE id = ?1", params![id])?
            == 1)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use chrono::TimeZone;

    fn reminder(id: &str, due_at: DateTime<Utc>, every_seconds: Option<u64>) -> AgentReminder {
        AgentReminder {
            id: id.to_string(),
            agent_id: "agent".to_string(),
            message: "check the build".to_string(),
            due_at,
            every_seconds,
            created_at: due_at,
            last_error: None,
        }
    }

    #[test]
    fn store_lists_due_reminders_soonest_first_and_cancels() {
        let dir = tempfile::tempdir().unwrap();
        let store = AgentReminderStore::new(dir.path().join("agent.sqlite3"));
        let at = |minute| Utc.with_ymd_and_hms(2026, 10, 9, 12, minute, 0).unwrap();
        store.save(&reminder("late", at(30), None)).unwrap();
        store.save(&reminder("soon", at(5), Some(600))).unwrap();
        store.save(&reminder("now", at(10), None)).unwrap();

        let due = store.due(at(10)).unwrap();
        assert_eq!(
            due.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["soon", "now"]
        );
        assert!(store.cancel("now").unwrap());
        assert!(!store.cancel("now").unwrap());
        assert_eq!(store.list().unwrap().len(), 2);
    }

    #[test]
    fn recurring_reminder_skips_missed_times_and_one_shot_ends() {
        let at = |minute| Utc.with_ymd_and_hms(2026, 10, 9, 12, minute, 0).unwrap();
        let every_ten = reminder("r", at(0), Some(600));
        // Delivered on time: the next run is one interval later.
        assert_eq!(every_ten.after_delivery(at(0)).unwrap().due_at, at(10));
        // Delivered 35 minutes late: once, then one interval from now.
        assert_eq!(every_ten.after_delivery(at(35)).unwrap().due_at, at(45));
        assert_eq!(reminder("once", at(0), None).after_delivery(at(0)), None);
    }
}
