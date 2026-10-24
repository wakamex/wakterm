//! The Devin CLI's sessions, which it keeps in one SQLite database.
//!
//! Each session's messages form a tree in `message_nodes`. The session's
//! conversation is the chain from `sessions.main_chain_id` back through
//! `parent_node_id`. Devin writes each node as the turn runs: the user's
//! input (`role` user, `is_user_input` true), assistant steps that call tools
//! (`finish_reason` "tool_calls"), tool results, and the assistant's reply
//! that ends the turn (`finish_reason` "stop", no tool calls). One message
//! can be written to several rows, all but one off the main chain, and
//! compaction later copies earlier messages, with their `message_id`, into a
//! new chain.
//!
//! A running session holds `session_locks/<session id>.lock` next to the
//! database, which names the process id of its `devin acp` worker, a child of
//! the `devin` process in the pane.

use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{Connection, OptionalExtension};
use serde_json::Value;
use std::path::{Path, PathBuf};
use url::Url;

pub(crate) fn db_path() -> Option<PathBuf> {
    std::env::var_os("WAKTERM_AGENT_DEVIN_DB")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|home| !home.is_empty())
                .map(PathBuf::from)
                .map(|home| {
                    home.join(".local")
                        .join("share")
                        .join("devin")
                        .join("cli")
                        .join("sessions.db")
                })
        })
}

pub(crate) fn open(db_path: &Path) -> anyhow::Result<Connection> {
    let connection =
        Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.busy_timeout(std::time::Duration::from_secs(2))?;
    Ok(connection)
}

/// Names a session as `devin://session?db=PATH&id=ID`.
pub(crate) fn session_url(db_path: &Path, session_id: &str) -> String {
    let mut url = Url::parse("devin://session").expect("static devin url is valid");
    url.query_pairs_mut()
        .append_pair("db", &db_path.to_string_lossy())
        .append_pair("id", session_id);
    url.to_string()
}

pub(crate) fn parse_session_url(value: &str) -> Option<(PathBuf, String)> {
    let url = Url::parse(value).ok()?;
    if url.scheme() != "devin" {
        return None;
    }
    let mut db_path = None;
    let mut session_id = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "db" => db_path = Some(PathBuf::from(value.into_owned())),
            "id" => session_id = Some(value.into_owned()),
            _ => {}
        }
    }
    Some((db_path?, session_id?))
}

/// The session whose lock names `pid` or a process descending from it, such
/// as the `devin acp` worker of the `devin` process in a pane. Locks of
/// sessions that have exited stay behind with stale process ids, so only a
/// live descendant counts.
pub(crate) fn session_of_process(db_path: &Path, pid: u32) -> Option<String> {
    let locks = db_path.parent()?.join("session_locks");
    std::fs::read_dir(locks).ok()?.flatten().find_map(|entry| {
        let path = entry.path();
        let session_id = path
            .file_name()?
            .to_str()?
            .strip_suffix(".lock")?
            .to_string();
        let holder: u32 = std::fs::read_to_string(&path).ok()?.trim().parse().ok()?;
        descends_from(holder, pid).then_some(session_id)
    })
}

#[cfg(target_os = "linux")]
fn descends_from(mut pid: u32, ancestor: u32) -> bool {
    for _ in 0..8 {
        if pid == ancestor {
            return true;
        }
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        // The parent follows the parenthesized command name, which may
        // itself contain spaces or parentheses.
        let Some(parent) = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().nth(1))
            .and_then(|parent| parent.parse().ok())
        else {
            return false;
        };
        if parent == 0 {
            return false;
        }
        pid = parent;
    }
    false
}

#[cfg(not(target_os = "linux"))]
fn descends_from(pid: u32, ancestor: u32) -> bool {
    pid == ancestor
}

/// The newest session started in `cwd` that was active at or after
/// `active_after`.
pub(crate) fn newest_session_in(
    connection: &Connection,
    cwd: &str,
    active_after: Option<DateTime<Utc>>,
) -> anyhow::Result<Option<String>> {
    Ok(connection
        .query_row(
            "select id from sessions where working_directory = ?1 and hidden = 0 \
             and last_activity_at >= ?2 order by last_activity_at desc limit 1",
            rusqlite::params![
                cwd,
                active_after.map(|at| at.timestamp()).unwrap_or(i64::MIN)
            ],
            |row| row.get(0),
        )
        .optional()?)
}

/// A user or assistant message of a session's conversation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Message {
    pub row_id: i64,
    pub node_id: i64,
    pub message_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub role: String,
    pub content: String,
    /// A message the user sent, rather than one Devin injected.
    pub user_input: bool,
    /// An assistant message that ends its turn: it stopped and calls no
    /// tools.
    pub ends_turn: bool,
    /// The result Devin records for a tool call the user interrupted, which
    /// ends the turn without a reply.
    pub interrupted: bool,
}

/// The session's conversation, oldest first, without system messages and
/// tool results other than an interrupted tool call's. Empty for a session that has no messages yet.
pub(crate) fn conversation(
    connection: &Connection,
    session_id: &str,
) -> anyhow::Result<Vec<Message>> {
    let Some(mut node) = connection
        .query_row(
            "select main_chain_id from sessions where id = ?1",
            [session_id],
            |row| row.get::<_, Option<i64>>(0),
        )
        .optional()?
        .flatten()
    else {
        return Ok(Vec::new());
    };
    let mut statement = connection.prepare_cached(
        "select row_id, parent_node_id, created_at, chat_message from message_nodes \
         where session_id = ?1 and node_id = ?2",
    )?;
    let mut messages = Vec::new();
    // A chain longer than the table would be a cycle.
    for _ in 0..1_000_000 {
        let Some((row_id, parent, created_at, chat_message)) = statement
            .query_row(rusqlite::params![session_id, node], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .optional()?
        else {
            break;
        };
        if let Some(message) = parse_message(row_id, node, created_at, &chat_message) {
            messages.push(message);
        }
        match parent {
            Some(parent) => node = parent,
            None => break,
        }
    }
    messages.reverse();
    Ok(messages)
}

fn parse_message(
    row_id: i64,
    node_id: i64,
    created_at: i64,
    chat_message: &str,
) -> Option<Message> {
    let message: Value = serde_json::from_str(chat_message).ok()?;
    let role = message.get("role")?.as_str()?;
    let content = message.get("content").and_then(Value::as_str).unwrap_or("");
    let interrupted = role == "tool" && content.starts_with("Canceled due to user interrupt");
    if !matches!(role, "user" | "assistant") && !interrupted {
        return None;
    }
    let metadata = message.get("metadata");
    let created_at = metadata
        .and_then(|metadata| metadata.get("created_at"))
        .and_then(Value::as_str)
        .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
        .map(|at| at.with_timezone(&Utc))
        .or_else(|| Utc.timestamp_opt(created_at, 0).single())
        .unwrap_or_else(Utc::now);
    let calls_tools = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .is_some_and(|calls| !calls.is_empty());
    let finish_reason = metadata
        .and_then(|metadata| metadata.get("finish_reason"))
        .and_then(Value::as_str);
    Some(Message {
        row_id,
        node_id,
        message_id: message
            .get("message_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        created_at,
        role: role.to_string(),
        content: content.to_string(),
        user_input: role == "user"
            && metadata
                .and_then(|metadata| metadata.get("is_user_input"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
        ends_turn: role == "assistant" && finish_reason == Some("stop") && !calls_tools,
        interrupted,
    })
}

/// Whether the message was already in the session at row `seen_through`,
/// which makes this row a copy that compaction made of it. Rows Devin wrote
/// for the message since then are the same write.
pub(crate) fn seen_before(
    connection: &Connection,
    session_id: &str,
    message: &Message,
    seen_through: i64,
) -> anyhow::Result<bool> {
    let Some(message_id) = message.message_id.as_deref() else {
        return Ok(false);
    };
    Ok(connection.query_row(
        "select exists(select 1 from message_nodes where session_id = ?1 and row_id <= ?2 \
         and json_extract(chat_message, '$.message_id') = ?3)",
        rusqlite::params![session_id, seen_through, message_id],
        |row| row.get(0),
    )?)
}

/// Devin's input box as its pane shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputBox {
    /// The text typed into the box, empty when it shows its placeholder.
    pub text: String,
    /// Whether the placeholder says a turn is running.
    pub working: bool,
}

const IDLE_PLACEHOLDER: &str = "Ask Devin to build features, fix bugs, or work on your code";
const WORKING_PLACEHOLDER: &str = "Guide Devin while it works";

/// Reads Devin's input box from the pane's visible lines: the lowest line
/// that starts with `❭` between two rules. Devin shows a placeholder in an
/// empty box, one while it waits for the user and another while a turn runs.
/// A permission prompt or other menu replaces the box, and its `❭` line has
/// no rules around it.
pub fn input_box(lines: &[wakterm_term::Line]) -> Option<InputBox> {
    let lines: Vec<String> = lines
        .iter()
        .map(|line| line.as_str().trim_end().to_string())
        .collect();
    let is_rule = |line: &str| line.trim_start().starts_with('\u{2500}');
    let prompt = (1..lines.len().saturating_sub(1)).rev().find(|&i| {
        is_rule(&lines[i - 1]) && lines[i].starts_with('\u{276d}') && is_rule(&lines[i + 1])
    })?;
    let text = lines[prompt]['\u{276d}'.len_utf8()..].trim();
    Some(match text {
        IDLE_PLACEHOLDER => InputBox {
            text: String::new(),
            working: false,
        },
        WORKING_PLACEHOLDER => InputBox {
            text: String::new(),
            working: true,
        },
        text => InputBox {
            text: text.to_string(),
            working: false,
        },
    })
}

/// A Devin database for tests, with the tables and message shapes Devin
/// writes.
#[cfg(test)]
pub(crate) mod fixture {
    use rusqlite::Connection;
    use serde_json::json;
    use std::path::Path;

    pub(crate) struct Fixture {
        connection: Connection,
        session_id: String,
        next_node: i64,
        tip: Option<i64>,
    }

    impl Fixture {
        pub(crate) fn create(db_path: &Path, session_id: &str, cwd: &str) -> Self {
            let connection = Connection::open(db_path).unwrap();
            connection
                .execute_batch(
                    "create table sessions (id text primary key, working_directory text not null, \
                     created_at integer not null, last_activity_at integer not null, \
                     main_chain_id integer, hidden integer not null default 0);
                     create table message_nodes (row_id integer primary key autoincrement, \
                     session_id text not null, node_id integer not null, parent_node_id integer, \
                     chat_message text not null, created_at integer not null, metadata text, \
                     unique(session_id, node_id));",
                )
                .unwrap();
            connection
                .execute(
                    "insert into sessions (id, working_directory, created_at, last_activity_at) \
                     values (?1, ?2, 1792789390, 1792789390)",
                    rusqlite::params![session_id, cwd],
                )
                .unwrap();
            let mut fixture = Self {
                connection,
                session_id: session_id.to_string(),
                next_node: 0,
                tip: None,
            };
            fixture.push(
                json!({"message_id": "system", "role": "system", "content": "You are Devin"}),
            );
            fixture
        }

        /// Appends a message to the main chain, as Devin does while a turn
        /// runs, and returns its node id. Like Devin, it first writes the
        /// message to a row off the chain.
        pub(crate) fn push(&mut self, message: serde_json::Value) -> i64 {
            self.insert(&message);
            let node = self.insert(&message);
            self.tip = Some(node);
            self.connection
                .execute(
                    "update sessions set main_chain_id = ?1, last_activity_at = 1792789393 where id = ?2",
                    rusqlite::params![node, self.session_id],
                )
                .unwrap();
            node
        }

        fn insert(&mut self, message: &serde_json::Value) -> i64 {
            let node = self.next_node;
            self.next_node += 1;
            self.connection
                .execute(
                    "insert into message_nodes (session_id, node_id, parent_node_id, chat_message, \
                     created_at) values (?1, ?2, ?3, ?4, 1792789393)",
                    rusqlite::params![self.session_id, node, self.tip, message.to_string()],
                )
                .unwrap();
            node
        }

        pub(crate) fn user(&mut self, id: &str, text: &str) -> i64 {
            self.push(json!({
                "message_id": id, "role": "user", "content": text,
                "metadata": {"is_user_input": true, "created_at": "2026-10-23T20:36:33.000Z"}
            }))
        }

        pub(crate) fn tool_step(&mut self, id: &str) -> i64 {
            self.push(json!({
                "message_id": id, "role": "assistant", "content": "",
                "tool_calls": [{"id": "call", "name": "exec"}],
                "metadata": {"finish_reason": "tool_calls", "created_at": "2026-10-23T20:37:18.000Z"}
            }));
            self.push(json!({
                "message_id": format!("{id}-result"), "role": "tool", "content": "done",
                "tool_call_id": "call"
            }))
        }

        /// The result Devin records when the user interrupts a running tool.
        pub(crate) fn interrupt(&mut self, id: &str) -> i64 {
            self.push(json!({
                "message_id": id, "role": "tool", "content": "Canceled due to user interrupt",
                "tool_call_id": "call"
            }))
        }

        pub(crate) fn reply(&mut self, id: &str, text: &str) -> i64 {
            self.push(json!({
                "message_id": id, "role": "assistant", "content": text, "tool_calls": [],
                "metadata": {"finish_reason": "stop", "created_at": "2026-10-23T20:37:24.000Z"}
            }))
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn lines(text: &[&str]) -> Vec<wakterm_term::Line> {
        text.iter()
            .map(|line| wakterm_term::Line::from_text(line, &Default::default(), 0, None))
            .collect()
    }

    #[test]
    fn input_box_is_read_with_its_text_and_whether_a_turn_runs() {
        let rule = "\u{2500}".repeat(20);
        let screen = |prompt: &str| {
            lines(&[
                " \u{23fa} /code/concord",
                &rule,
                prompt,
                &rule,
                "SWE-2 High",
            ])
        };
        assert_eq!(
            input_box(&screen(&format!("\u{276d} {IDLE_PLACEHOLDER}"))),
            Some(InputBox {
                text: String::new(),
                working: false
            })
        );
        assert_eq!(
            input_box(&screen(&format!("\u{276d} {WORKING_PLACEHOLDER}"))),
            Some(InputBox {
                text: String::new(),
                working: true
            })
        );
        assert_eq!(
            input_box(&screen("\u{276d} half a thought")).map(|state| state.text),
            Some("half a thought".to_string())
        );
        // A permission prompt replaces the box.
        let approval = lines(&[
            " \u{2514} $ sleep 15; echo done",
            "\u{276d} Allow       \u{2190} \u{25fc}\u{25fc}\u{25fc}\u{25fc} \u{2192} Once",
            "  Bypass mode",
            "  Deny",
            "\u{2191}\u{2193} select \u{b7} \u{2190}\u{2192} scope \u{b7} e edit \u{b7} \u{21b5} confirm \u{b7} esc cancel",
        ]);
        assert_eq!(input_box(&approval), None);
    }

    #[test]
    fn conversation_follows_the_main_chain_and_marks_the_reply_that_ends_a_turn() {
        let temp = tempfile::TempDir::new().unwrap();
        let db = temp.path().join("sessions.db");
        let mut fixture = fixture::Fixture::create(&db, "almondine-continent", "/code/concord");
        fixture.user("u1", "Run `sleep 15; echo done` and reply done");
        fixture.tool_step("a1");
        fixture.reply("a2", "done");

        let connection = open(&db).unwrap();
        let messages = conversation(&connection, "almondine-continent").unwrap();
        let summary: Vec<_> = messages
            .iter()
            .map(|message| (message.role.as_str(), message.user_input, message.ends_turn))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("user", true, false),
                ("assistant", false, false),
                ("assistant", false, true)
            ]
        );
        assert_eq!(messages[2].content, "done");
        assert_eq!(
            parse_session_url(&session_url(&db, "almondine-continent")),
            Some((db, "almondine-continent".to_string()))
        );
    }
}
