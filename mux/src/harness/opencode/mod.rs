//! Reads OpenCode's input box from its screen.
//!
//! OpenCode draws its prompt as rows that start with a `┃` bar on the box's
//! own background, ending in a mode line, such as `┃  Build auto · <model>`,
//! right above the box's `╹▀▀▀` bottom border. In an empty box it draws a
//! placeholder, `Ask anything… "<example>"` or, in shell mode,
//! `Run a command… "<example>"`, in its theme's muted color, the color of
//! the mode line's `·` separators. Typed text is drawn in another color.

use crate::agent::{
    cwd_lookup_variants, default_launch_cmd_for_harness, harness_dir, home_dir, parse_unix_millis,
    truncate_summary, zcode_db_path, AgentHarness, AgentTurnState, HarnessObservation,
};
use crate::agent_event::{
    digest, input_accepted, observer_failure, AgentEventKind, AgentInputRole, PendingEvent,
};
use anyhow::Context;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use url::Url;
use wakterm_term::color::ColorAttribute;
use wakterm_term::Line;

const BAR: &str = "\u{2503}";
const BOTTOM: char = '\u{2579}';
const SEPARATOR: &str = "\u{b7}";
const INTERRUPT_HINT: &str = "esc interrupt";
const PLACEHOLDERS: [&str; 2] = ["Ask anything\u{2026}", "Run a command\u{2026}"];

/// A notification or reminder OpenCode injects as a user message, which
/// continues the current turn rather than starting one.
pub(crate) fn runtime_message(message: &serde_json::Value) -> bool {
    message
        .get("synthetic")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
        || message
            .pointer("/semantics/origin")
            .and_then(serde_json::Value::as_str)
            == Some("agent_runtime")
}

/// Whether a message starts a turn: input the user sent. A turn is named
/// by its user message's ID, which OpenCode's replies give as `parentID`.
/// Both of Wakterm's readers of a session follow this and `turn_end`.
pub(crate) fn starts_turn(message: &serde_json::Value) -> bool {
    message.get("role").and_then(serde_json::Value::as_str) == Some("user")
        && !runtime_message(message)
}

/// Whether an assistant message ends its turn, and whether it completed:
/// a reply that stopped, or a provider failure, which ends it without an
/// answer. A tool step, or a reply still streaming, ends nothing.
pub(crate) fn turn_end(message: &serde_json::Value) -> Option<bool> {
    if message.get("role").and_then(serde_json::Value::as_str) != Some("assistant") {
        return None;
    }
    if message.get("error").is_some() {
        Some(false)
    } else if message.get("finish").and_then(serde_json::Value::as_str) == Some("stop") {
        Some(true)
    } else {
        None
    }
}

/// The turn an assistant message answers: its user message's ID.
pub(crate) fn reply_to(message: &serde_json::Value) -> Option<&str> {
    message.get("parentID").and_then(serde_json::Value::as_str)
}

/// The background just right of the row's `┃` bar, which is the box's own.
fn bar_background(line: &Line) -> Option<ColorAttribute> {
    let mut cells = line.visible_cells().skip_while(|cell| cell.str() != BAR);
    cells.next()?;
    Some(cells.next()?.attrs().background())
}

/// The text typed into OpenCode's input box, empty when it holds only its
/// placeholder, or None when the box is not on screen.
pub fn input_box(lines: &[Line]) -> Option<String> {
    let plain: Vec<String> = lines.iter().map(|line| line.as_str().to_string()).collect();
    let mode_row = mode_row(&plain)?;
    let background = bar_background(&lines[mode_row])?;
    let muted = lines[mode_row]
        .visible_cells()
        .find(|cell| cell.str() == SEPARATOR)
        .map(|cell| cell.attrs().foreground());
    let first_row = (0..mode_row)
        .rev()
        .take_while(|&i| {
            plain[i].trim_start().starts_with(BAR) && bar_background(&lines[i]) == Some(background)
        })
        .last()
        .unwrap_or(mode_row);
    let rows: Vec<String> = lines[first_row..mode_row]
        .iter()
        .map(|line| {
            line.visible_cells()
                .skip_while(|cell| cell.str() != BAR)
                .skip(1)
                .filter(|cell| Some(cell.attrs().foreground()) != muted)
                .map(|cell| cell.str().to_string())
                .collect::<String>()
                .trim()
                .to_string()
        })
        .filter(|row| !row.is_empty())
        .collect();
    let text = rows.join("\n");
    // Without a separator to learn the muted color from, the placeholder
    // is known by its words.
    if muted.is_none() && PLACEHOLDERS.iter().any(|words| text.starts_with(words)) {
        return Some(String::new());
    }
    Some(text)
}

/// The input box's last row, which shows its mode and model, just above the
/// box's bottom edge.
fn mode_row(plain: &[String]) -> Option<usize> {
    (0..plain.len().saturating_sub(1)).rev().find(|&i| {
        plain[i].trim_start().starts_with(BAR) && plain[i + 1].trim_start().starts_with(BOTTOM)
    })
}

/// Whether OpenCode's screen shows a turn running: under its input box, in
/// place of the folder, it shows a spinner and how to interrupt the turn.
/// None when the box is not on screen.
pub fn working(lines: &[Line]) -> Option<bool> {
    let plain: Vec<String> = lines.iter().map(|line| line.as_str().to_string()).collect();
    let mode_row = mode_row(&plain)?;
    Some(
        plain[mode_row + 2..]
            .iter()
            .any(|line| line.contains(INTERRUPT_HINT)),
    )
}

pub(crate) fn observe(
    harness: &AgentHarness,
    pid: Option<u32>,
    cwd: &str,
    preferred_session: Option<&str>,
    updated_after: Option<DateTime<Utc>>,
    exact_session: Option<&str>,
) -> anyhow::Result<Option<HarnessObservation>> {
    let _blocking = crate::main_thread_blocking::blocking("reading an OpenCode session database");
    let Some(db_path) = schema_db_path(harness, pid) else {
        return Ok(None);
    };
    if !db_path.is_file() {
        return Ok(None);
    }

    let connection =
        Connection::open_with_flags(&db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.busy_timeout(std::time::Duration::from_secs(2))?;

    // A session named on the command line or by restore is exact; the
    // directory and recency selection below is only a fallback.
    if let Some(session_id) = exact_session {
        return read_last_observation(&connection, &db_path, session_id, None);
    }

    if let Some(preferred_session) = preferred_session {
        if let Some((preferred_db_path, preferred_session_id)) =
            parse_session_path(preferred_session)
        {
            if preferred_db_path == db_path {
                if let Some(observed) = read_last_observation(
                    &connection,
                    &db_path,
                    &preferred_session_id,
                    updated_after,
                )? {
                    return Ok(Some(observed));
                }
            }
        }
    }

    let Some((session_id, _)) = select_session(&connection, cwd, updated_after)? else {
        return Ok(None);
    };
    read_last_observation(&connection, &db_path, &session_id, updated_after)
}

pub(crate) fn describe_pending(
    harness: &AgentHarness,
    pid: Option<u32>,
    cwd: &str,
    updated_after: Option<DateTime<Utc>>,
) -> anyhow::Result<Option<String>> {
    let _blocking = crate::main_thread_blocking::blocking("reading an OpenCode session database");
    let Some(db_path) = schema_db_path(harness, pid) else {
        return Ok(None);
    };
    let name = default_launch_cmd_for_harness(harness).unwrap_or("agent");
    if !db_path.is_file() {
        return Ok(Some(format!(
            "{name} session database has not appeared yet"
        )));
    }

    let connection =
        Connection::open_with_flags(&db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.busy_timeout(std::time::Duration::from_secs(2))?;

    let has_recent_session = select_session(&connection, cwd, updated_after)?.is_some();
    let has_session = select_session(&connection, cwd, None)?.is_some();

    Ok(Some(if has_recent_session {
        format!("{name} session exists but observer has not attached yet")
    } else if has_session {
        format!("{name} session exists but no new turn appeared yet")
    } else {
        format!("{name} session has not appeared yet")
    }))
}

/// The session database for a harness that uses OpenCode's schema.
pub(crate) fn schema_db_path(harness: &AgentHarness, pid: Option<u32>) -> Option<PathBuf> {
    match harness {
        AgentHarness::Opencode => db_path(pid),
        AgentHarness::Zcode => zcode_db_path(),
        _ => None,
    }
}

/// Where the OpenCode process `pid` keeps its database: under the data
/// directory its environment's XDG_DATA_HOME names, else
/// `~/.local/share`.
pub(crate) fn db_path(pid: Option<u32>) -> Option<PathBuf> {
    std::env::var_os("WAKTERM_AGENT_OPENCODE_DB")
        .map(PathBuf::from)
        .or_else(|| {
            harness_dir(
                pid,
                "XDG_DATA_HOME",
                home_dir().map(|home| home.join(".local").join("share")),
            )
            .map(|data| data.join("opencode").join("opencode.db"))
        })
}

pub(crate) fn encode_session_path(db_path: &Path, session_id: &str) -> String {
    let mut url = Url::parse("opencode://session").expect("static opencode url is valid");
    url.query_pairs_mut()
        .append_pair("db", &db_path.to_string_lossy())
        .append_pair("id", session_id);
    url.to_string()
}

pub(crate) fn parse_session_path(value: &str) -> Option<(PathBuf, String)> {
    let url = Url::parse(value).ok()?;
    if url.scheme() != "opencode" {
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

pub(crate) fn select_session(
    connection: &Connection,
    cwd: &str,
    updated_after: Option<DateTime<Utc>>,
) -> anyhow::Result<Option<(String, i64)>> {
    let prefer_earliest = updated_after.is_some();
    let mut selected = None;

    for candidate in cwd_lookup_variants(cwd) {
        let row = if let Some(cutoff) = updated_after {
            let cutoff_millis = cutoff.timestamp_millis();
            let sql = if prefer_earliest {
                "SELECT id, time_updated FROM session \
                 WHERE directory = ?1 AND time_updated >= ?2 \
                 ORDER BY time_updated ASC LIMIT 1"
            } else {
                "SELECT id, time_updated FROM session \
                 WHERE directory = ?1 AND time_updated >= ?2 \
                 ORDER BY time_updated DESC LIMIT 1"
            };
            connection
                .query_row(sql, params![candidate, cutoff_millis], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })
                .optional()?
        } else {
            let sql = if prefer_earliest {
                "SELECT id, time_updated FROM session \
                 WHERE directory = ?1 ORDER BY time_updated ASC LIMIT 1"
            } else {
                "SELECT id, time_updated FROM session \
                 WHERE directory = ?1 ORDER BY time_updated DESC LIMIT 1"
            };
            connection
                .query_row(sql, params![candidate], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })
                .optional()?
        };

        let Some((session_id, updated_millis)) = row else {
            continue;
        };
        match &selected {
            Some((_, existing_updated_millis))
                if (prefer_earliest && *existing_updated_millis <= updated_millis)
                    || (!prefer_earliest && *existing_updated_millis >= updated_millis) => {}
            _ => selected = Some((session_id, updated_millis)),
        }
    }

    Ok(selected)
}

pub(crate) fn read_last_observation(
    connection: &Connection,
    db_path: &Path,
    session_id: &str,
    updated_after: Option<DateTime<Utc>>,
) -> anyhow::Result<Option<HarnessObservation>> {
    let Some(updated_millis) = connection
        .query_row(
            "SELECT time_updated FROM session WHERE id = ?1",
            params![session_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
    else {
        return Ok(None);
    };

    if updated_after
        .map(|cutoff| updated_millis < cutoff.timestamp_millis())
        .unwrap_or(false)
    {
        return Ok(None);
    }

    // The latest turn, and the message that ended it if one has, by the
    // rule the event projector follows: a turn is a user message, and ends
    // with a reply to it that stopped, or a provider failure.
    let mut latest_end: Option<(String, bool, Option<DateTime<Utc>>)> = None;
    let mut ends = Vec::new();
    let mut turn: Option<(String, Option<DateTime<Utc>>)> = None;
    let mut message_stmt = connection.prepare(
        "SELECT id, time_created, data \
         FROM message \
         WHERE session_id = ?1 \
         ORDER BY time_created DESC, rowid DESC",
    )?;
    let mut message_rows = message_stmt.query(params![session_id])?;
    while let Some(row) = message_rows.next()? {
        let message_id = row.get::<_, String>(0)?;
        let time_created = row.get::<_, i64>(1)?;
        let message_data = row.get::<_, String>(2)?;
        let Ok(message) = serde_json::from_str::<Value>(&message_data) else {
            continue;
        };
        if let Some(completed) = turn_end(&message) {
            let at = message
                .pointer("/time/completed")
                .and_then(Value::as_i64)
                .and_then(parse_unix_millis)
                .or_else(|| parse_unix_millis(time_created));
            let end = (
                reply_to(&message).unwrap_or_default().to_string(),
                completed,
                at,
            );
            if latest_end.is_none() {
                latest_end = Some(end.clone());
            }
            ends.push(end);
        } else if starts_turn(&message) {
            turn = Some((message_id, parse_unix_millis(time_created)));
            break;
        }
    }
    let (turn_state, last_turn_completed_at, failed) = match &turn {
        Some((turn_id, _)) => match ends.iter().find(|(ended, _, _)| ended == turn_id) {
            Some((_, completed, at)) => (AgentTurnState::WaitingOnUser, *at, !completed),
            None => (
                AgentTurnState::WaitingOnAgent,
                latest_end.as_ref().and_then(|(_, _, at)| *at),
                false,
            ),
        },
        // Replies with no turn of their own in the session wait on the user.
        None => match &latest_end {
            Some((_, completed, at)) => (AgentTurnState::WaitingOnUser, *at, !completed),
            None => (AgentTurnState::Unknown, None, false),
        },
    };

    let mut summary = None;
    let mut part_stmt = connection.prepare(
        "SELECT p.data, m.data \
         FROM part p \
         JOIN message m ON p.message_id = m.id \
         WHERE p.session_id = ?1 \
         ORDER BY p.rowid DESC",
    )?;
    let mut part_rows = part_stmt.query(params![session_id])?;
    while let Some(row) = part_rows.next()? {
        let part_data = row.get::<_, String>(0)?;
        let message_data = row.get::<_, String>(1)?;
        let Ok(part) = serde_json::from_str::<Value>(&part_data) else {
            continue;
        };
        let Ok(message) = serde_json::from_str::<Value>(&message_data) else {
            continue;
        };
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        if part.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            let text = text.trim();
            if !text.is_empty() {
                summary = Some(truncate_summary(text));
                break;
            }
        }
    }

    Ok(Some(HarnessObservation {
        session_path: Some(encode_session_path(db_path, session_id)),
        progress_summary: summary,
        harness_mode: None,
        turn_phase: failed.then(|| "failed".to_string()),
        updated_at: parse_unix_millis(updated_millis),
        turn_state,
        last_turn_completed_at,
        observed_turn: None,
    }))
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Cursor {
    pub source_id: String,
    pub last_message_rowid: i64,
    pub last_message_id: Option<String>,
    pub last_part_rowid: i64,
    pub last_part_id: Option<String>,
    pub last_final_message_rowid: i64,
}

/// The text a user typed or pasted into an OpenCode message: its text
/// parts that OpenCode did not add itself, in order.
fn user_text(conn: &Connection, message_id: &str) -> anyhow::Result<Option<String>> {
    let mut stmt = conn.prepare(
        "SELECT json_extract(data, '$.text') FROM part
         WHERE message_id = ?1 AND json_extract(data, '$.type') = 'text'
           AND COALESCE(json_extract(data, '$.synthetic'), 0) = 0
         ORDER BY rowid",
    )?;
    let parts = stmt
        .query_map(params![message_id], |row| row.get::<_, Option<String>>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let text = parts.into_iter().flatten().collect::<Vec<_>>().join("\n");
    Ok((!text.trim().is_empty()).then_some(text))
}

fn has_later_message(conn: &Connection, session_id: &str, rowid: i64) -> anyhow::Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM message WHERE session_id = ?1 AND rowid > ?2 LIMIT 1",
            params![session_id, rowid],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

pub(crate) fn project(
    session_path: &str,
    cursor: Option<Cursor>,
) -> anyhow::Result<(Cursor, Vec<PendingEvent>)> {
    let (db_path, source_id) = parse_session_path(session_path)
        .with_context(|| format!("{session_path} does not name an OpenCode session"))?;
    let conn = Connection::open_with_flags(&db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(std::time::Duration::from_secs(2))?;
    let maxima = maxima(&conn, &source_id)?;
    let (message_max, part_max, final_message_max) = &maxima;
    let baseline_identity = digest(
        format!(
            "{}\0{}\0{}\0{}\0{}",
            message_max.0,
            message_max.1.as_deref().unwrap_or(""),
            part_max.0,
            part_max.1.as_deref().unwrap_or(""),
            final_message_max
        )
        .as_bytes(),
    );
    let mut cursor = match cursor {
        Some(cursor) if cursor.source_id == source_id => cursor,
        Some(_) => {
            let detail =
                "confirmed OpenCode provider session changed; a new tail baseline was armed";
            return Ok((
                Cursor {
                    source_id: source_id.clone(),
                    last_message_rowid: message_max.0,
                    last_message_id: message_max.1.clone(),
                    last_part_rowid: part_max.0,
                    last_part_id: part_max.1.clone(),
                    last_final_message_rowid: *final_message_max,
                },
                vec![observer_failure(
                    format!("opencode:{source_id}:baseline-reset:{baseline_identity}"),
                    Utc::now(),
                    None,
                    detail,
                )],
            ));
        }
        None => {
            return Ok((
                Cursor {
                    source_id,
                    last_message_rowid: message_max.0,
                    last_message_id: message_max.1.clone(),
                    last_part_rowid: part_max.0,
                    last_part_id: part_max.1.clone(),
                    last_final_message_rowid: *final_message_max,
                },
                Vec::new(),
            ))
        }
    };
    if !checkpoint_matches(&conn, &source_id, &cursor)? {
        let detail = "OpenCode provider database was rebuilt; a new tail baseline was armed";
        return Ok((
            Cursor {
                source_id: source_id.clone(),
                last_message_rowid: message_max.0,
                last_message_id: message_max.1.clone(),
                last_part_rowid: part_max.0,
                last_part_id: part_max.1.clone(),
                last_final_message_rowid: *final_message_max,
            },
            vec![observer_failure(
                format!("opencode:{source_id}:baseline-reset:{baseline_identity}"),
                Utc::now(),
                None,
                detail,
            )],
        ));
    }
    let mut events = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT rowid, id, time_created, data FROM message
             WHERE session_id = ?1 AND rowid > ?2 ORDER BY rowid",
        )?;
        let rows = stmt.query_map(params![source_id, cursor.last_message_rowid], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        for row in rows {
            let (rowid, message_id, time_created, data) = row?;
            let message: Value = serde_json::from_str(&data)?;
            if starts_turn(&message) {
                // OpenCode writes a user message's text parts after the
                // message. Wait for them so the turn carries its input's
                // hash, unless a later message shows the input has none.
                let text = user_text(&conn, &message_id)?;
                if text.is_none() && !has_later_message(&conn, &source_id, rowid)? {
                    break;
                }
                let timestamp =
                    DateTime::from_timestamp_millis(time_created).unwrap_or_else(Utc::now);
                let mut started = PendingEvent::new(
                    format!("opencode:{source_id}:{message_id}:started"),
                    AgentEventKind::TurnStarted,
                    timestamp,
                );
                started.turn_id = Some(message_id.clone());
                started.input_sha256 = text.as_deref().map(crate::agent::message_sha256);
                let accepted = started.input_sha256.clone().map(|input_sha256| {
                    input_accepted(
                        format!("opencode:{source_id}:{message_id}:input"),
                        timestamp,
                        Some(message_id.clone()),
                        input_sha256,
                        AgentInputRole::Start,
                    )
                });
                events.push(started);
                events.extend(accepted);
                let mut state = PendingEvent::new(
                    format!("opencode:{source_id}:{message_id}:state"),
                    AgentEventKind::TurnStateChanged,
                    timestamp,
                );
                state.turn_id = Some(message_id.clone());
                state.turn_state = Some("waiting_on_agent".to_string());
                events.push(state);
            }
            cursor.last_message_rowid = rowid;
            cursor.last_message_id = Some(message_id);
        }
    }
    {
        let mut stmt = conn.prepare(
            "SELECT p.rowid, p.id, p.time_created, p.data, m.id, m.data
             FROM part p JOIN message m ON p.message_id = m.id
             WHERE p.session_id = ?1 AND p.rowid > ?2 ORDER BY p.rowid",
        )?;
        let rows = stmt.query_map(params![source_id, cursor.last_part_rowid], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?;
        for row in rows {
            let (rowid, part_id, time_created, part_data, _message_id, message_data) = row?;
            let part: Value = serde_json::from_str(&part_data)?;
            let message: Value = serde_json::from_str(&message_data)?;
            let assistant_text = message.get("role").and_then(Value::as_str) == Some("assistant")
                && part.get("type").and_then(Value::as_str) == Some("text");
            // OpenCode creates a text part when a reply starts and fills it
            // in as the reply streams, then marks it ended. A part read
            // before then is read again once it is finished, and so are the
            // parts after it, which keep their order.
            if assistant_text
                && part.pointer("/time/end").is_none()
                && message.pointer("/time/completed").is_none()
            {
                break;
            }
            if assistant_text {
                let turn_id = message.get("parentID").and_then(Value::as_str);
                let text = part
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|text| !text.is_empty());
                match (turn_id, text) {
                    (Some(turn_id), Some(text)) => {
                        let mut output = PendingEvent::new(
                            format!("opencode:{source_id}:{part_id}:message"),
                            AgentEventKind::AssistantMessage,
                            DateTime::from_timestamp_millis(time_created).unwrap_or_else(Utc::now),
                        );
                        output.turn_id = Some(turn_id.to_string());
                        output.text = Some(text.to_string());
                        events.push(output);
                    }
                    (None, Some(_)) => events.push(observer_failure(
                        format!("opencode:{source_id}:{part_id}:missing-turn"),
                        DateTime::from_timestamp_millis(time_created).unwrap_or_else(Utc::now),
                        None,
                        "OpenCode assistant message lacks an exact parent provider turn id",
                    )),
                    _ => {}
                }
            }
            cursor.last_part_rowid = rowid;
            cursor.last_part_id = Some(part_id);
        }
    }
    {
        let mut stmt = conn.prepare(
            "SELECT m.rowid, m.id, m.time_created, m.data,
                    (SELECT GROUP_CONCAT(text, char(10)) FROM (
                         SELECT json_extract(p.data, '$.text') AS text
                         FROM part p WHERE p.message_id = m.id
                           AND json_extract(p.data, '$.type') = 'text'
                         ORDER BY p.rowid
                     ))
             FROM message m
             WHERE m.session_id = ?1 AND m.rowid > ?2
               AND json_extract(m.data, '$.role') = 'assistant'
               AND (json_extract(m.data, '$.finish') = 'stop'
                    OR json_extract(m.data, '$.error') IS NOT NULL)
             ORDER BY m.rowid",
        )?;
        let rows = stmt.query_map(params![source_id, cursor.last_final_message_rowid], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })?;
        for row in rows {
            let (rowid, message_id, time_created, data, text) = row?;
            let message: Value = serde_json::from_str(&data)?;
            // The query only narrows the rows to candidates; the rule decides.
            if turn_end(&message).is_none() {
                continue;
            }
            let Some(turn_id) = reply_to(&message) else {
                events.push(observer_failure(
                    format!("opencode:{source_id}:{message_id}:final-missing-turn"),
                    DateTime::from_timestamp_millis(time_created).unwrap_or_else(Utc::now),
                    None,
                    "OpenCode completed message lacks an exact parent provider turn id",
                ));
                cursor.last_final_message_rowid = rowid;
                continue;
            };
            let timestamp = message
                .get("time")
                .and_then(|time| time.get("completed"))
                .and_then(Value::as_i64)
                .and_then(DateTime::from_timestamp_millis)
                .unwrap_or_else(|| {
                    DateTime::from_timestamp_millis(time_created).unwrap_or_else(Utc::now)
                });
            let failed = message.get("error");
            if let Some(error) = failed {
                // A provider failure ends the turn without an answer; its
                // message is kept for diagnosis but not passed on.
                let detail = error
                    .pointer("/data/message")
                    .or_else(|| error.get("message"))
                    .or_else(|| error.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("OpenCode reported a provider failure");
                events.push(observer_failure(
                    format!("opencode:{source_id}:{message_id}:provider-failure"),
                    timestamp,
                    Some(turn_id.to_string()),
                    detail,
                ));
            }
            let mut final_event = PendingEvent::new(
                format!("opencode:{source_id}:{message_id}:final"),
                AgentEventKind::TurnFinal,
                timestamp,
            );
            final_event.turn_id = Some(turn_id.to_string());
            if failed.is_some() {
                final_event.outcome = Some("aborted".to_string());
                final_event.reason = Some("provider_failure".to_string());
                final_event.detail = Some(
                    "The provider reported an error, so the turn ended without a reply."
                        .to_string(),
                );
            } else {
                final_event.outcome = Some("completed".to_string());
                final_event.text = text
                    .map(|text| text.trim().to_string())
                    .filter(|text| !text.is_empty());
            }
            events.push(final_event);
            let mut state = PendingEvent::new(
                format!("opencode:{source_id}:{message_id}:state"),
                AgentEventKind::TurnStateChanged,
                timestamp,
            );
            state.turn_id = Some(turn_id.to_string());
            state.turn_state = Some("waiting_on_user".to_string());
            events.push(state);
            cursor.last_final_message_rowid = rowid;
        }
    }
    Ok((cursor, events))
}

fn maxima(
    conn: &Connection,
    session_id: &str,
) -> anyhow::Result<((i64, Option<String>), (i64, Option<String>), i64)> {
    let message = conn
        .query_row(
            "SELECT rowid, id FROM message WHERE session_id = ?1 ORDER BY rowid DESC LIMIT 1",
            params![session_id],
            |row| Ok((row.get(0)?, Some(row.get(1)?))),
        )
        .optional()?
        .unwrap_or((0, None));
    let part = conn
        .query_row(
            "SELECT rowid, id FROM part WHERE session_id = ?1 ORDER BY rowid DESC LIMIT 1",
            params![session_id],
            |row| Ok((row.get(0)?, Some(row.get(1)?))),
        )
        .optional()?
        .unwrap_or((0, None));
    let final_message = conn.query_row(
        "SELECT COALESCE(MAX(rowid), 0) FROM message
         WHERE session_id = ?1 AND json_extract(data, '$.role') = 'assistant'
           AND (json_extract(data, '$.finish') = 'stop'
                OR json_extract(data, '$.error') IS NOT NULL)",
        params![session_id],
        |row| row.get(0),
    )?;
    Ok((message, part, final_message))
}

fn checkpoint_matches(
    conn: &Connection,
    session_id: &str,
    cursor: &Cursor,
) -> anyhow::Result<bool> {
    let message_matches = if cursor.last_message_rowid == 0 {
        true
    } else {
        conn.query_row(
            "SELECT id FROM message WHERE session_id = ?1 AND rowid = ?2",
            params![session_id, cursor.last_message_rowid],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .as_deref()
            == cursor.last_message_id.as_deref()
    };
    let part_matches = if cursor.last_part_rowid == 0 {
        true
    } else {
        conn.query_row(
            "SELECT id FROM part WHERE session_id = ?1 AND rowid = ?2",
            params![session_id, cursor.last_part_rowid],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .as_deref()
            == cursor.last_part_id.as_deref()
    };
    Ok(message_matches && part_matches)
}

#[cfg(test)]
mod test {
    use super::*;
    use wakterm_term::color::SrgbaTuple;
    use wakterm_term::{Cell, CellAttributes};

    fn color(r: u8, g: u8, b: u8) -> ColorAttribute {
        ColorAttribute::TrueColorWithDefaultFallback(SrgbaTuple::from((r, g, b, 255)))
    }

    /// A screen row: text runs, each with its foreground and background.
    fn row(runs: &[(&str, ColorAttribute, ColorAttribute)]) -> Line {
        let mut cells = Vec::new();
        for (text, fg, bg) in runs {
            let mut attrs = CellAttributes::default();
            attrs.set_foreground(*fg);
            attrs.set_background(*bg);
            cells.extend(text.chars().map(|c| Cell::new(c, attrs.clone())));
        }
        Line::from_cells(cells, 0)
    }

    #[test]
    fn the_box_holds_typed_text_and_not_its_placeholder() {
        let (page, boxed) = (color(10, 10, 10), color(30, 30, 30));
        let (accent, text, muted) = (
            color(92, 156, 245),
            color(255, 255, 255),
            color(128, 128, 128),
        );
        let screen = |typed: &[(&str, ColorAttribute)]| {
            let mut box_row = vec![("  ", text, page), (BAR, accent, page), ("  ", text, boxed)];
            box_row.extend(typed.iter().map(|(words, fg)| (*words, *fg, boxed)));
            vec![
                // A user message in the history also has a bar, on the page.
                row(&[
                    ("  ", text, page),
                    (BAR, accent, page),
                    ("  earlier message", text, page),
                ]),
                row(&[("     \u{25a3}  Build \u{b7} model \u{b7} 3.5s", muted, page)]),
                row(&[
                    ("  ", text, page),
                    (BAR, accent, page),
                    ("   ", text, boxed),
                ]),
                row(&box_row),
                row(&[
                    ("  ", text, page),
                    (BAR, accent, page),
                    ("  ", text, boxed),
                    ("Build", accent, boxed),
                    (" ", text, boxed),
                    ("auto", muted, boxed),
                    (" \u{b7} ", muted, boxed),
                    ("DeepSeek V4.1 Flash", text, boxed),
                ]),
                row(&[("  \u{2579}\u{2580}\u{2580}\u{2580}", accent, page)]),
                row(&[("   /code/gta6", muted, page)]),
            ]
        };
        assert_eq!(input_box(&screen(&[])).as_deref(), Some(""));
        // While a turn runs, a spinner and the interrupt hint replace the
        // folder under the box; the hint elsewhere on screen is not it.
        let mut running = screen(&[]);
        *running.last_mut().unwrap() =
            row(&[("   \u{2b1d}\u{2b1d}\u{2b1d}  esc interrupt", muted, page)]);
        assert_eq!(working(&running), Some(true));
        let mut quoted = screen(&[]);
        quoted.insert(0, row(&[("  press esc interrupt to stop", text, page)]));
        assert_eq!(working(&quoted), Some(false));
        assert_eq!(working(&[row(&[("$ ls", text, page)])]), None);
        assert_eq!(
            input_box(&screen(&[("Ask anything\u{2026} \"Fix a TODO\"", muted)])).as_deref(),
            Some("")
        );
        assert_eq!(
            input_box(&screen(&[("half a thought", text)])).as_deref(),
            Some("half a thought")
        );
        // A screen without OpenCode's box.
        assert_eq!(input_box(&[row(&[("$ ls", text, page)])]), None);
    }
}
