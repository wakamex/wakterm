//! Gemini CLI's sessions, which it keeps as chat files of messages: the
//! user's (`type` "user") and Gemini's (`type` "gemini"), whose tool calls,
//! if any, it records on the message that makes them.

use crate::agent::{
    cwd_lookup_variants, extract_message_text, harness_dir, home_dir, parse_rfc3339_timestamp,
    truncate_summary, AgentTurnState, HarnessObservation, HarnessObservationDetails,
};
use crate::agent_event::{
    digest, event_time, extract_text, observer_failure, AgentEventKind, PendingEvent,
};
use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

/// Whether a message starts a turn: input the user sent. Gemini also
/// records a tool's result as a user message, whose parts are all
/// `functionResponse`, and opens a session with a user message of injected
/// context; neither is input, and neither is a message whose text is empty. Both of Wakterm's readers of a session follow
/// this and `ends_turn`.
pub(crate) fn starts_turn(message: &Value) -> bool {
    if message.get("type").and_then(Value::as_str) != Some("user") {
        return false;
    }
    let texts = match message.get("content") {
        Some(Value::String(text)) => vec![text.as_str()],
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect(),
        _ => Vec::new(),
    };
    texts.iter().any(|text| {
        let text = text.trim();
        !text.is_empty() && !text.starts_with("<session_context>")
    })
}

/// Whether a message ends its turn: a reply from Gemini that calls no tools.
/// A reply that calls tools is a step, and the turn goes on.
pub(crate) fn ends_turn(message: &Value) -> bool {
    message.get("type").and_then(Value::as_str) == Some("gemini")
        && !message
            .get("toolCalls")
            .and_then(Value::as_array)
            .is_some_and(|calls| !calls.is_empty())
}

#[derive(Clone, Debug)]
pub(crate) struct Conversation {
    pub session_id: Option<String>,
    pub last_updated: Option<DateTime<Utc>>,
    pub messages: Vec<Value>,
}

pub(crate) fn observe(
    cwd: &str,
    pid: Option<u32>,
    preferred_session: Option<&str>,
    updated_after: Option<DateTime<Utc>>,
) -> anyhow::Result<Option<HarnessObservation>> {
    let Some(root) = root_for(pid) else {
        return Ok(None);
    };
    let project_dirs = project_dirs(&root, cwd)?;
    if project_dirs.is_empty() {
        return Ok(None);
    }

    if let Some(preferred_session) = preferred_session {
        let preferred_path = preferred_session_path(Path::new(preferred_session));
        if preferred_path.is_file() {
            let modified_at = DateTime::<Utc>::from(fs::metadata(&preferred_path)?.modified()?);
            if updated_after
                .map(|cutoff| modified_at >= cutoff)
                .unwrap_or(true)
            {
                let details = read_last_observation(&preferred_path)?;
                return Ok(Some(HarnessObservation::from_details(
                    &preferred_path,
                    details,
                    modified_at,
                )));
            }
        }
    }

    let prefer_earliest = updated_after.is_some();
    let mut selected: Option<(PathBuf, DateTime<Utc>)> = None;
    for project_dir in project_dirs {
        let chats_dir = project_dir.join("chats");
        if !chats_dir.is_dir() {
            continue;
        }

        for entry in fs::read_dir(&chats_dir)? {
            let entry = entry?;
            let path = entry.path();
            if !is_session_file(&path) {
                continue;
            }
            let modified_at = DateTime::<Utc>::from(entry.metadata()?.modified()?);
            if updated_after
                .map(|cutoff| modified_at < cutoff)
                .unwrap_or(false)
            {
                continue;
            }
            match &selected {
                Some((_, existing_modified))
                    if (prefer_earliest && *existing_modified <= modified_at)
                        || (!prefer_earliest && *existing_modified >= modified_at) => {}
                _ => selected = Some((path, modified_at)),
            }
        }
    }

    let Some((session, modified_at)) = selected else {
        return Ok(None);
    };
    let details = read_last_observation(&session)?;
    Ok(Some(HarnessObservation::from_details(
        &session,
        details,
        modified_at,
    )))
}

pub(crate) fn describe_pending(
    cwd: &str,
    pid: Option<u32>,
    updated_after: Option<DateTime<Utc>>,
) -> anyhow::Result<Option<String>> {
    let Some(root) = root_for(pid) else {
        return Ok(None);
    };
    let project_dirs = project_dirs(&root, cwd)?;
    if project_dirs.is_empty() {
        return Ok(Some(
            "gemini project directory has not appeared yet".to_string(),
        ));
    }

    let mut has_session = false;
    let mut has_recent_session = false;
    for project_dir in project_dirs {
        let chats_dir = project_dir.join("chats");
        if !chats_dir.is_dir() {
            continue;
        }

        for entry in fs::read_dir(&chats_dir)? {
            let entry = entry?;
            let path = entry.path();
            if !is_session_file(&path) {
                continue;
            }
            has_session = true;
            let modified_at = DateTime::<Utc>::from(entry.metadata()?.modified()?);
            if updated_after
                .map(|cutoff| modified_at >= cutoff)
                .unwrap_or(true)
            {
                has_recent_session = true;
                break;
            }
        }

        if has_recent_session {
            break;
        }
    }

    Ok(Some(if has_recent_session {
        "gemini session file exists but observer has not attached yet".to_string()
    } else if has_session {
        "gemini project directory exists but no new chat session file appeared yet".to_string()
    } else {
        "gemini project directory exists but no chat session file appeared yet".to_string()
    }))
}

pub(crate) fn root() -> Option<PathBuf> {
    root_for(None)
}

/// Where the Gemini CLI process `pid` keeps its files: `.gemini` under the
/// directory its environment's GEMINI_CLI_HOME names, which stands in for
/// the user's home, else under the home.
pub(crate) fn root_for(pid: Option<u32>) -> Option<PathBuf> {
    std::env::var_os("WAKTERM_AGENT_GEMINI_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            harness_dir(pid, "GEMINI_CLI_HOME", home_dir()).map(|home| home.join(".gemini"))
        })
}

pub(crate) fn project_id(root: &Path, cwd: &str) -> anyhow::Result<Option<String>> {
    let registry_path = root.join("projects.json");
    if !registry_path.is_file() {
        return Ok(None);
    }

    let registry: Value = serde_json::from_reader(fs::File::open(registry_path)?)?;
    let Some(projects) = registry.get("projects").and_then(Value::as_object) else {
        return Ok(None);
    };

    for candidate in cwd_lookup_variants(cwd) {
        if let Some(project_id) = projects.get(&candidate).and_then(Value::as_str) {
            let project_id = project_id.trim();
            if !project_id.is_empty() {
                return Ok(Some(project_id.to_string()));
            }
        }
    }

    Ok(None)
}

pub(crate) fn project_dirs(root: &Path, cwd: &str) -> anyhow::Result<Vec<PathBuf>> {
    let mut dirs = vec![];
    let mut seen = std::collections::HashSet::new();
    let tmp_root = root.join("tmp");

    if let Some(project_id) = project_id(root, cwd)? {
        let path = tmp_root.join(project_id);
        if path.is_dir() && seen.insert(path.clone()) {
            dirs.push(path);
        }
    }

    if !tmp_root.is_dir() {
        return Ok(dirs);
    }

    let variants = cwd_lookup_variants(cwd);
    for entry in fs::read_dir(&tmp_root)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let Some(project_root) = project_root(&path)? else {
            continue;
        };
        if variants.iter().any(|candidate| candidate == &project_root) && seen.insert(path.clone())
        {
            dirs.push(path);
        }
    }

    Ok(dirs)
}

pub(crate) fn project_root(project_dir: &Path) -> anyhow::Result<Option<String>> {
    let root_file = project_dir.join(".project_root");
    if !root_file.is_file() {
        return Ok(None);
    }

    let root = fs::read_to_string(root_file)?;
    let root = root.trim().trim_end_matches('/').to_string();
    if root.is_empty() {
        Ok(None)
    } else {
        Ok(Some(root))
    }
}

pub(crate) fn is_session_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| {
            name.starts_with("session-") && (name.ends_with(".json") || name.ends_with(".jsonl"))
        })
        .unwrap_or(false)
}

pub(crate) fn preferred_session_path(path: &Path) -> PathBuf {
    if path.extension().and_then(|extension| extension.to_str()) == Some("json") {
        let migrated = PathBuf::from(format!("{}l", path.to_string_lossy()));
        if migrated.is_file() {
            return migrated;
        }
    }
    path.to_path_buf()
}

pub(crate) fn read_conversation(path: &Path) -> anyhow::Result<Conversation> {
    if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
        let record: Value = serde_json::from_reader(fs::File::open(path)?)?;
        return Ok(Conversation {
            session_id: record
                .get("sessionId")
                .and_then(Value::as_str)
                .map(str::to_string),
            last_updated: record
                .get("lastUpdated")
                .and_then(Value::as_str)
                .and_then(parse_rfc3339_timestamp),
            messages: record
                .get("messages")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
        });
    }

    const MAX_GEMINI_RECORD_BYTES: u64 = 4 * 1024 * 1024;
    let mut reader = BufReader::new(fs::File::open(path)?);
    let mut line = Vec::new();
    let mut session_id = None;
    let mut last_updated = None;
    let mut messages = Vec::<Value>::new();
    let mut message_index = std::collections::HashMap::<String, usize>::new();

    loop {
        line.clear();
        let read = reader
            .by_ref()
            .take(MAX_GEMINI_RECORD_BYTES + 1)
            .read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        anyhow::ensure!(
            read as u64 <= MAX_GEMINI_RECORD_BYTES,
            "Gemini session record exceeds the {MAX_GEMINI_RECORD_BYTES}-byte bound"
        );
        if !line.ends_with(b"\n") {
            break;
        }
        let record: Value = serde_json::from_slice(&line)?;
        let metadata = record.get("$set").unwrap_or(&record);
        if let Some(value) = metadata
            .get("sessionId")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            session_id = Some(value.to_string());
        }
        if let Some(value) = metadata
            .get("lastUpdated")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339_timestamp)
        {
            last_updated = Some(value);
        }

        let Some(id) = record
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
        if record.get("type").and_then(Value::as_str).is_none() {
            continue;
        }
        if let Some(index) = message_index.get(id).copied() {
            messages[index] = record;
        } else {
            message_index.insert(id.to_string(), messages.len());
            messages.push(record);
        }
    }

    Ok(Conversation {
        session_id,
        last_updated,
        messages,
    })
}

pub(crate) fn read_last_observation(path: &Path) -> anyhow::Result<HarnessObservationDetails> {
    let conversation = read_conversation(path)?;
    let mut summary = None;
    let mut last_user_at = None;
    let mut last_assistant_at = None;
    // Whether the latest turn is still open, and when the latest one ended,
    // by the rule the event projector follows.
    let mut turn_open = false;
    let mut last_turn_completed_at = None;
    for message in &conversation.messages {
        let timestamp = message
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339_timestamp);
        if starts_turn(message) {
            last_user_at = timestamp.or(last_user_at);
            turn_open = true;
        } else if message.get("type").and_then(Value::as_str) == Some("gemini") {
            last_assistant_at = timestamp.or(last_assistant_at);
            if let Some(content) = message.get("content").and_then(extract_message_text) {
                summary = Some(truncate_summary(&content));
            }
            if ends_turn(message) {
                turn_open = false;
                last_turn_completed_at = timestamp.or(last_turn_completed_at);
            }
        }
    }

    let turn_state = if turn_open {
        AgentTurnState::WaitingOnAgent
    } else if last_turn_completed_at.is_some() || last_assistant_at.is_some() {
        AgentTurnState::WaitingOnUser
    } else {
        AgentTurnState::Unknown
    };
    Ok(HarnessObservationDetails {
        progress_summary: summary,
        harness_mode: None,
        turn_phase: None,
        updated_at: conversation
            .last_updated
            .or(last_assistant_at)
            .or(last_user_at),
        turn_state,
        last_turn_completed_at,
        observed_turn: None,
    })
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Cursor {
    pub source_id: String,
    pub index: usize,
    pub last_id: Option<String>,
    pub current_turn_id: Option<String>,
    #[serde(default)]
    pub checkpoint_sha256: String,
}

pub(crate) fn project(
    path: &Path,
    cursor: Option<Cursor>,
) -> anyhow::Result<(Cursor, Vec<PendingEvent>)> {
    let conversation = read_conversation(path)?;
    let source_id = conversation
        .session_id
        .context("Gemini session lacks an exact provider session id")?
        .to_string();
    let messages = &conversation.messages;
    let mut cursor = match cursor {
        Some(cursor) if cursor.source_id == source_id => cursor,
        Some(_) => {
            return reset_projection(&source_id, messages, true);
        }
        None => {
            return Ok((
                Cursor {
                    source_id,
                    index: messages.len(),
                    last_id: messages
                        .last()
                        .and_then(|message| message.get("id"))
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    current_turn_id: latest_turn(messages),
                    checkpoint_sha256: checkpoint_sha256(messages, messages.len())?,
                },
                Vec::new(),
            ))
        }
    };
    let checkpoint_matches = cursor.checkpoint_sha256.is_empty()
        || checkpoint_sha256(messages, cursor.index)
            .is_ok_and(|checkpoint| checkpoint == cursor.checkpoint_sha256);
    if cursor.index > messages.len()
        || (cursor.index > 0
            && messages[cursor.index - 1].get("id").and_then(Value::as_str)
                != cursor.last_id.as_deref())
        || !checkpoint_matches
    {
        return reset_projection(&source_id, messages, false);
    }
    let mut events = Vec::new();
    for message in &messages[cursor.index..] {
        let timestamp = event_time(message);
        let Some(message_id) = message
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            events.push(observer_failure(
                format!("gemini:{}:missing-message-id:{}", source_id, cursor.index),
                timestamp,
                cursor.current_turn_id.clone(),
                "Gemini message lacks an exact provider message id",
            ));
            cursor.index += 1;
            continue;
        };
        let record_key = format!("gemini:{source_id}:{message_id}");
        match message.get("type").and_then(Value::as_str) {
            _ if starts_turn(message) => {
                cursor.current_turn_id = Some(message_id.to_string());
                let mut started = PendingEvent::new(
                    format!("{record_key}:started"),
                    AgentEventKind::TurnStarted,
                    timestamp,
                );
                started.turn_id = Some(message_id.to_string());
                events.push(started);
                let mut state = PendingEvent::new(
                    format!("{record_key}:state"),
                    AgentEventKind::TurnStateChanged,
                    timestamp,
                );
                state.turn_id = Some(message_id.to_string());
                state.turn_state = Some("waiting_on_agent".to_string());
                events.push(state);
            }
            Some("gemini") => {
                let Some(turn_id) = cursor.current_turn_id.clone() else {
                    events.push(observer_failure(
                        format!("{record_key}:missing-turn"),
                        timestamp,
                        None,
                        "Gemini response lacks an exact preceding provider user turn",
                    ));
                    cursor.index += 1;
                    cursor.last_id = Some(message_id.to_string());
                    continue;
                };
                let text = extract_text(message.get("content"));
                let has_tool_calls = !ends_turn(message);
                if let Some(text) = text.as_ref() {
                    let mut output = PendingEvent::new(
                        format!("{record_key}:message"),
                        AgentEventKind::AssistantMessage,
                        timestamp,
                    );
                    output.turn_id = Some(turn_id.clone());
                    output.text = Some(text.clone());
                    events.push(output);
                }
                if has_tool_calls {
                    cursor.index += 1;
                    cursor.last_id = Some(message_id.to_string());
                    continue;
                }
                let mut final_event = PendingEvent::new(
                    format!("{record_key}:final"),
                    AgentEventKind::TurnFinal,
                    timestamp,
                );
                final_event.turn_id = Some(turn_id.clone());
                final_event.outcome = Some("completed".to_string());
                final_event.text = text;
                events.push(final_event);
                let mut state = PendingEvent::new(
                    format!("{record_key}:state"),
                    AgentEventKind::TurnStateChanged,
                    timestamp,
                );
                state.turn_id = Some(turn_id);
                state.turn_state = Some("waiting_on_user".to_string());
                events.push(state);
            }
            Some("error") => {
                let detail = extract_text(message.get("content"))
                    .unwrap_or_else(|| "Gemini emitted a provider error".to_string());
                events.push(observer_failure(
                    format!("{record_key}:provider-error"),
                    timestamp,
                    cursor.current_turn_id.clone(),
                    &detail,
                ));
            }
            _ => {}
        }
        cursor.index += 1;
        cursor.last_id = Some(message_id.to_string());
    }
    cursor.checkpoint_sha256 = checkpoint_sha256(messages, cursor.index)?;
    Ok((cursor, events))
}

fn latest_turn(messages: &[Value]) -> Option<String> {
    messages
        .iter()
        .rev()
        .find(|message| starts_turn(message))
        .and_then(|message| message.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn checkpoint_sha256(messages: &[Value], index: usize) -> anyhow::Result<String> {
    anyhow::ensure!(
        index <= messages.len(),
        "Gemini cursor exceeds message history"
    );
    let relevant = messages[..index]
        .iter()
        .map(|message| {
            serde_json::json!({
                "id": message.get("id"),
                "type": message.get("type"),
                "timestamp": message.get("timestamp"),
                "content": message.get("content"),
                "toolCalls": message.get("toolCalls"),
            })
        })
        .collect::<Vec<_>>();
    Ok(digest(&serde_json::to_vec(&relevant)?))
}

fn reset_projection(
    source_id: &str,
    messages: &[Value],
    session_changed: bool,
) -> anyhow::Result<(Cursor, Vec<PendingEvent>)> {
    let detail = if session_changed {
        "confirmed Gemini provider session changed; a new tail baseline was armed"
    } else {
        "Gemini provider session was rewritten; a new tail baseline was armed"
    };
    let checkpoint_sha256 = checkpoint_sha256(messages, messages.len())?;
    Ok((
        Cursor {
            source_id: source_id.to_string(),
            index: messages.len(),
            last_id: messages
                .last()
                .and_then(|message| message.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string),
            current_turn_id: latest_turn(messages),
            checkpoint_sha256: checkpoint_sha256.clone(),
        },
        vec![observer_failure(
            format!("gemini:{source_id}:baseline-reset:{checkpoint_sha256}"),
            Utc::now(),
            None,
            detail,
        )],
    ))
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn only_a_user_message_with_text_of_its_own_starts_a_turn() {
        let user = |content: Value| serde_json::json!({"type": "user", "content": content});
        assert!(starts_turn(&user(
            serde_json::json!([{"text": "run the tests"}])
        )));
        assert!(starts_turn(&user(serde_json::json!("run the tests"))));
        // A tool's result, the injected context, and empty text are not
        // input.
        assert!(!starts_turn(&user(
            serde_json::json!([{"functionResponse": {"id": "run"}}])
        )));
        assert!(!starts_turn(&user(
            serde_json::json!([{"text": "<session_context>\nGemini CLI"}])
        )));
        assert!(!starts_turn(&user(
            serde_json::json!([{"text": "  "}, {"functionResponse": {"id": "run"}}])
        )));
        assert!(!starts_turn(&user(serde_json::json!(""))));
        assert!(!starts_turn(
            &serde_json::json!({"type": "gemini", "content": "hi"})
        ));
    }
}
