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

pub mod prompt;

use crate::agent_event::{input_accepted, AgentEventKind, PendingEvent};
use anyhow::Context;
use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use url::Url;

pub(crate) fn db_path() -> Option<PathBuf> {
    db_path_for(None)
}

/// The session database the Devin process `pid` uses. Devin works it out
/// at startup and passes it to its `devin acp` worker as
/// CHISEL_SESSION_DB, ignoring a value set for itself, so the worker's
/// environment names it exactly. Before the worker starts, it follows from
/// Devin's environment: DEVIN_SESSIONS_DIR names a directory holding
/// cli/sessions.db, and otherwise it is devin/cli/sessions.db under
/// XDG_DATA_HOME, `~/.local/share` by default.
pub(crate) fn db_path_for(pid: Option<u32>) -> Option<PathBuf> {
    use crate::agent::harness_dir;
    std::env::var_os("WAKTERM_AGENT_DEVIN_DB")
        .map(PathBuf::from)
        .or_else(|| {
            worker_of(pid?).and_then(|worker| harness_dir(Some(worker), "CHISEL_SESSION_DB", None))
        })
        .or_else(|| {
            harness_dir(pid, "DEVIN_SESSIONS_DIR", None)
                .map(|sessions| sessions.join("cli").join("sessions.db"))
        })
        .or_else(|| {
            let home = std::env::var_os("HOME")
                .filter(|home| !home.is_empty())
                .map(PathBuf::from);
            harness_dir(
                pid,
                "XDG_DATA_HOME",
                home.map(|home| home.join(".local").join("share")),
            )
            .map(|data| data.join("devin").join("cli").join("sessions.db"))
        })
}

/// The `devin acp` worker that the Devin process `pid` started.
#[cfg(target_os = "linux")]
fn worker_of(pid: u32) -> Option<u32> {
    let tasks = std::fs::read_dir(format!("/proc/{pid}/task")).ok()?;
    tasks
        .flatten()
        .filter_map(|task| std::fs::read_to_string(task.path().join("children")).ok())
        .flat_map(|children| {
            children
                .split_whitespace()
                .filter_map(|child| child.parse().ok())
                .collect::<Vec<u32>>()
        })
        .find(|child| {
            std::fs::read(format!("/proc/{child}/cmdline")).is_ok_and(|cmdline| {
                cmdline
                    .split(|byte| *byte == 0)
                    .skip(1)
                    .any(|arg| arg == b"acp")
            })
        })
}

#[cfg(not(target_os = "linux"))]
fn worker_of(_pid: u32) -> Option<u32> {
    None
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
    /// The result Devin records for a tool call the user interrupted or
    /// rejected at its permission prompt, either of which ends the turn
    /// without a reply.
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
    let interrupted = role == "tool"
        && (content.starts_with("Canceled due to user interrupt")
            || content.starts_with("Tool execution was rejected by the user"));
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
/// Where the Devin projection stopped: the session's highest message row it
/// has read, and the turn its latest user input started. Stored with the
/// agent's events, so its fields keep their names.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Cursor {
    pub source_id: String,
    pub last_row_id: i64,
    pub current_turn_id: Option<String>,
    pub turn_open: bool,
}

/// Observes a Devin session: the one whose lock the pane's Devin process
/// holds, else the session a restore expects, else the session named by
/// `preferred_session`, else the newest session started in `cwd` since
/// `updated_after`. A session Devin has locked
/// but written nothing to yet is observed as waiting for the user, so that
/// events can be read from its first message.
pub(crate) fn observe(
    cwd: &str,
    preferred_session: Option<&str>,
    updated_after: Option<DateTime<Utc>>,
    adopted_pid: Option<u32>,
    expected_session: Option<&str>,
) -> anyhow::Result<Option<crate::agent::HarnessObservation>> {
    let Some(db_path) = db_path_for(adopted_pid) else {
        return Ok(None);
    };
    if !db_path.is_file() {
        return Ok(None);
    }
    let connection = open(&db_path)?;
    let session_id = match adopted_pid
        .and_then(|pid| session_of_process(&db_path, pid))
        .or_else(|| expected_session.map(str::to_string))
    {
        Some(session_id) => session_id,
        None => match preferred_session
            .and_then(parse_session_url)
            .filter(|(path, _)| *path == db_path)
        {
            Some((_, session_id)) => session_id,
            None => match newest_session_in(&connection, cwd, updated_after)? {
                Some(session_id) => session_id,
                None => return Ok(None),
            },
        },
    };
    let messages = conversation(&connection, &session_id)?;
    let session_path = Some(session_url(&db_path, &session_id));
    let Some(turn) = latest_turn(&messages) else {
        return Ok(Some(crate::agent::HarnessObservation {
            session_path,
            progress_summary: None,
            harness_mode: None,
            turn_phase: None,
            updated_at: messages.last().map(|message| message.created_at),
            turn_state: crate::agent::AgentTurnState::WaitingOnUser,
            last_turn_completed_at: None,
            observed_turn: None,
        }));
    };
    let user = &messages[turn.start];
    let reply = turn.end.map(|end| &messages[end]);
    let latest = messages.last().unwrap_or(user);
    let final_message = reply
        .filter(|reply| reply.ends_turn)
        .map(|reply| reply.content.trim().to_string())
        .filter(|text| !text.is_empty());
    let (outcome, turn_state, completed_at, turn_phase) = match reply {
        Some(reply) if reply.interrupted => (
            crate::agent::AgentObservedTurnOutcome::Aborted,
            crate::agent::AgentTurnState::WaitingOnUser,
            Some(reply.created_at),
            "aborted",
        ),
        Some(reply) => (
            crate::agent::AgentObservedTurnOutcome::Completed,
            crate::agent::AgentTurnState::WaitingOnUser,
            Some(reply.created_at),
            "final_answer",
        ),
        None => (
            crate::agent::AgentObservedTurnOutcome::Running,
            crate::agent::AgentTurnState::WaitingOnAgent,
            None,
            "working",
        ),
    };
    Ok(Some(crate::agent::HarnessObservation {
        session_path,
        progress_summary: final_message.as_deref().map(crate::agent::truncate_summary),
        harness_mode: None,
        turn_phase: Some(turn_phase.to_string()),
        updated_at: Some(latest.created_at),
        turn_state,
        last_turn_completed_at: completed_at,
        observed_turn: Some(crate::agent::AgentObservedTurn {
            provider_turn_id: turn_id(&session_id, user),
            outcome,
            started_at: Some(user.created_at),
            completed_at,
            started_cursor: Some(user.row_id.max(0) as u64),
            latest_cursor: Some(latest.row_id.max(0) as u64),
            primary_user_message_sha256: Some(crate::agent::message_sha256(&user.content)),
            user_message_count: turn.user_messages,
            final_message,
        }),
    }))
}

/// Projects a Devin session's conversation. Each user input starts a turn,
/// or, while one runs, steers it; the reply that stops without tool calls
/// ends it. Messages compaction copied from earlier rows are skipped.
/// The events in a Devin session since `cursor`, and where they end. A
/// session read for the first time, or another session than the cursor's,
/// starts from what it holds now, so its history is not reported again.
pub(crate) fn project(
    session_path: &str,
    cursor: Option<Cursor>,
) -> anyhow::Result<(Cursor, Vec<PendingEvent>)> {
    let (db_path, source_id) = parse_session_url(session_path)
        .with_context(|| format!("{session_path} does not name a Devin session"))?;
    let connection = open(&db_path)?;
    let messages = conversation(&connection, &source_id)?;
    let mut cursor = match cursor {
        Some(cursor) if cursor.source_id == source_id => cursor,
        // A session observed for the first time starts from what it holds
        // now, so its history is not reported again. A session with no
        // messages yet is read from its first one.
        _ => {
            let turn = latest_turn(&messages);
            return Ok((
                Cursor {
                    source_id: source_id.clone(),
                    last_row_id: messages
                        .iter()
                        .map(|message| message.row_id)
                        .max()
                        .unwrap_or(0),
                    current_turn_id: turn.map(|turn| turn_id(&source_id, &messages[turn.start])),
                    turn_open: turn.is_some_and(|turn| turn.end.is_none()),
                },
                Vec::new(),
            ));
        }
    };
    let seen_through = cursor.last_row_id;
    let mut events = Vec::new();
    for message in &messages {
        if message.row_id <= seen_through {
            continue;
        }
        cursor.last_row_id = cursor.last_row_id.max(message.row_id);
        if seen_before(&connection, &source_id, message, seen_through)? {
            continue;
        }
        let record_key = format!("devin:{source_id}:{}", message.node_id);
        let timestamp = message.created_at;
        let text = message.content.trim();
        let mut tracker = TurnTracker {
            open: cursor.turn_open,
        };
        let step = tracker.step(message);
        cursor.turn_open = tracker.open;
        match (step, cursor.current_turn_id.clone()) {
            (TurnStep::Steered, Some(turn_id)) => {
                let sha256 = crate::agent::message_sha256(&message.content);
                events.push(input_accepted(
                    format!("{record_key}:input"),
                    timestamp,
                    Some(turn_id),
                    sha256,
                    false,
                ));
            }
            (TurnStep::Started | TurnStep::Steered, _) => {
                let sha256 = crate::agent::message_sha256(&message.content);
                let turn_id = turn_id(&source_id, message);
                cursor.current_turn_id = Some(turn_id.clone());
                let mut started = PendingEvent::new(
                    format!("{record_key}:started"),
                    AgentEventKind::TurnStarted,
                    timestamp,
                );
                started.turn_id = Some(turn_id.clone());
                started.input_sha256 = Some(sha256.clone());
                let accepted = input_accepted(
                    format!("{record_key}:input"),
                    timestamp,
                    Some(turn_id.clone()),
                    sha256,
                    false,
                );
                let mut state = PendingEvent::new(
                    format!("{record_key}:state"),
                    AgentEventKind::TurnStateChanged,
                    timestamp,
                );
                state.turn_id = Some(turn_id);
                state.turn_state = Some("waiting_on_agent".to_string());
                events.extend([started, accepted, state]);
            }
            (_, None) => {}
            (step, Some(turn_id)) => {
                let ended = step == TurnStep::Ended;
                // An interruption ends the turn without a reply.
                if !message.interrupted && !text.is_empty() {
                    let mut assistant = PendingEvent::new(
                        format!("{record_key}:message"),
                        AgentEventKind::AssistantMessage,
                        timestamp,
                    );
                    assistant.turn_id = Some(turn_id.clone());
                    assistant.text = Some(text.to_string());
                    events.push(assistant);
                }
                if ended {
                    let completed = !message.interrupted;
                    let mut final_event = PendingEvent::new(
                        format!("{record_key}:final"),
                        AgentEventKind::TurnFinal,
                        timestamp,
                    );
                    final_event.turn_id = Some(turn_id.clone());
                    final_event.outcome =
                        Some(if completed { "completed" } else { "aborted" }.to_string());
                    final_event.text = (completed && !text.is_empty()).then(|| text.to_string());
                    let mut state = PendingEvent::new(
                        format!("{record_key}:final-state"),
                        AgentEventKind::TurnStateChanged,
                        timestamp,
                    );
                    state.turn_id = Some(turn_id);
                    state.turn_state = Some("waiting_on_user".to_string());
                    events.extend([final_event, state]);
                }
            }
        }
    }
    Ok((cursor, events))
}

/// The ID of the turn a user message starts. Devin can rebuild a session's
/// chain with copies of earlier messages at new nodes, keeping their
/// message IDs, so the message ID names the turn the same way before and
/// after; a message without one is named by its node.
pub(crate) fn turn_id(session_id: &str, message: &Message) -> String {
    match &message.message_id {
        Some(message_id) => format!("{session_id}:{message_id}"),
        None => format!("{session_id}:{}", message.node_id),
    }
}

/// What a message does to the turn it arrives in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TurnStep {
    /// A user message with no turn running starts one.
    Started,
    /// A user message while a turn runs steers it.
    Steered,
    /// A reply that ends the turn, or an interruption, ends the running turn.
    Ended,
    /// Anything else belongs to whatever turn runs.
    Content,
}

/// Whether a Devin turn runs, by the one rule both of Wakterm's readers of
/// a session follow, the observer from the start of the conversation and
/// the event projector from where its cursor left off.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TurnTracker {
    pub open: bool,
}

impl TurnTracker {
    pub(crate) fn step(&mut self, message: &Message) -> TurnStep {
        if message.user_input {
            if self.open {
                TurnStep::Steered
            } else {
                self.open = true;
                TurnStep::Started
            }
        } else if self.open && (message.ends_turn || message.interrupted) {
            self.open = false;
            TurnStep::Ended
        } else {
            TurnStep::Content
        }
    }
}

/// The latest turn in a conversation: the index of the user message that
/// started it, how many user messages it took, steers included, and the
/// index of the message that ended it, if one has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LatestTurn {
    pub start: usize,
    pub user_messages: u32,
    pub end: Option<usize>,
}

pub(crate) fn latest_turn(messages: &[Message]) -> Option<LatestTurn> {
    let mut tracker = TurnTracker::default();
    let mut latest = None;
    for (index, message) in messages.iter().enumerate() {
        match (tracker.step(message), latest.as_mut()) {
            (TurnStep::Started, _) => {
                latest = Some(LatestTurn {
                    start: index,
                    user_messages: 1,
                    end: None,
                })
            }
            (TurnStep::Steered, Some(turn)) => turn.user_messages += 1,
            (TurnStep::Ended, Some(turn)) => turn.end = Some(index),
            _ => {}
        }
    }
    latest
}

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

        /// The result Devin records when the user denies a permission.
        pub(crate) fn reject(&mut self, id: &str) -> i64 {
            self.push(json!({
                "message_id": id, "role": "tool",
                "content": "Tool execution was rejected by the user", "tool_call_id": "call"
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
