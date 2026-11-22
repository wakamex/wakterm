//! agy's step log: a JSONL transcript of steps, each with a `type`, such as
//! `USER_INPUT`, `PLANNER_RESPONSE` or `SYSTEM_MESSAGE`, and a `status`.

use crate::agent::{
    home_dir, is_uuid, message_sha256, parse_rfc3339_timestamp, truncate_summary,
    visit_lines_reverse, AgentObservedTurn, AgentObservedTurnOutcome, AgentTurnState,
    HarnessObservation, HarnessObservationDetails,
};
use crate::agent_event::{
    event_time, input_accepted, observer_failure, AgentEventKind, JsonlCursor, PendingEvent,
};
use chrono::{DateTime, Duration, Utc};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

/// What a step does to the turn it arrives in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TurnStep {
    /// The user's input starts a turn.
    StartedByUser,
    /// A system notice that arrives while no turn runs wakes agy to work on
    /// it: a turn without user input.
    StartedBySystem,
    /// A finished response that calls no tools while a background task the
    /// turn started still runs: an interim answer, after which agy waits for
    /// the task's notice and goes on, so the turn stays open.
    Interim,
    /// A finished response that calls no tools, with no background task
    /// pending, ends the turn.
    Ended,
    Other,
}

/// How long a turn waits, after an interim answer, for the notice of a
/// background task before it ends with that answer. Of 338 such waits in
/// 499 agy transcripts, all but one ended within 30 minutes.
pub(crate) const BACKGROUND_WAIT_LIMIT: Duration = Duration::minutes(30);

/// Whether an agy turn runs, by the one rule both of Wakterm's readers of a
/// transcript follow: the observer from the latest user input, the event
/// projector from where its cursor left off. A step whose status is `ERROR`
/// does not end a turn: agy goes on after it. A tool step agy marks
/// `RUNNING` runs as a background task, whose end agy learns from a system
/// notice; until each such task's notice arrives, an answer is interim.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TurnTracker {
    pub open: bool,
    /// Background tasks of the open turn whose notice has not arrived.
    pub pending: u32,
    /// When the open turn's interim answer was given, while it waits for a
    /// background task's notice.
    pub waiting_since: Option<DateTime<Utc>>,
}

impl TurnTracker {
    pub(crate) fn step(&mut self, record: &Value) -> TurnStep {
        let status = record.get("status").and_then(Value::as_str);
        match record.get("type").and_then(Value::as_str) {
            Some("USER_INPUT") => {
                *self = TurnTracker {
                    open: true,
                    ..TurnTracker::default()
                };
                TurnStep::StartedByUser
            }
            Some("SYSTEM_MESSAGE") if !self.open => {
                *self = TurnTracker {
                    open: true,
                    ..TurnTracker::default()
                };
                TurnStep::StartedBySystem
            }
            Some("SYSTEM_MESSAGE") if self.pending > 0 => {
                self.pending -= 1;
                self.waiting_since = None;
                TurnStep::Other
            }
            Some("GENERIC") if self.open && status == Some("RUNNING") => {
                self.pending += 1;
                TurnStep::Other
            }
            Some("PLANNER_RESPONSE")
                if self.open && status == Some("DONE") && !calls_tools(record) =>
            {
                if self.pending > 0 {
                    // agy dates every step; one it did not would wait for
                    // its notice without a limit rather than reset one.
                    self.waiting_since = record_time(record);
                    TurnStep::Interim
                } else {
                    *self = TurnTracker::default();
                    TurnStep::Ended
                }
            }
            _ => TurnStep::Other,
        }
    }

    /// Ends a turn that has waited past `BACKGROUND_WAIT_LIMIT` since its
    /// interim answer, returning whether it did. The turn ends with that
    /// answer; a notice that comes later starts a turn of its own. Readers
    /// call this with each step's own time before stepping it, so that a
    /// wait that ran out between two steps ends there however late they read
    /// them, and with the clock once they have read every step.
    pub(crate) fn expire(&mut self, now: DateTime<Utc>) -> bool {
        let expired = self.open
            && self
                .waiting_since
                .is_some_and(|since| now - since > BACKGROUND_WAIT_LIMIT);
        if expired {
            *self = TurnTracker::default();
        }
        expired
    }
}

/// When agy recorded a step.
pub(crate) fn record_time(record: &Value) -> Option<DateTime<Utc>> {
    record
        .get("created_at")
        .and_then(Value::as_str)
        .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
        .map(|at| at.with_timezone(&Utc))
}

/// Whether a response calls tools. agy records its tool calls as a list,
/// or as text, empty when there are none.
pub(crate) fn calls_tools(record: &Value) -> bool {
    record.get("tool_calls").is_some_and(|calls| match calls {
        Value::Array(calls) => !calls.is_empty(),
        Value::String(calls) => !matches!(calls.trim(), "" | "[]"),
        Value::Null => false,
        _ => true,
    })
}

/// The ID of the turn a step starts: the conversation and its step index.
pub(crate) fn turn_id(conversation_id: &str, record: &Value) -> Option<String> {
    let step = record.get("step_index")?.as_u64()?;
    Some(format!("{conversation_id}:{step}"))
}

/// The text the user sent, which agy records between USER_REQUEST tags
/// ahead of metadata it appends.
pub(crate) fn user_request(content: &str) -> Option<&str> {
    let start = content.find("<USER_REQUEST>")? + "<USER_REQUEST>".len();
    let end = content[start..].find("</USER_REQUEST>")? + start;
    Some(content[start..end].trim())
}

/// Whether the pane shows agy's feedback survey, "How's the CLI experience
/// so far?" with its numbered choices, above agy's idle hint. agy shows it
/// in place of the input box once a turn has ended, and the digit keys
/// answer it.
pub fn feedback_survey(lines: &[wakterm_term::Line]) -> bool {
    let lines: Vec<String> = lines
        .iter()
        .map(|line| line.as_str().trim().to_string())
        .filter(|line| !line.is_empty())
        .collect();
    let Some(question) = lines
        .iter()
        .rposition(|line| line.starts_with("How's the CLI experience so far?"))
    else {
        return false;
    };
    // The question, its choices and the footer close the screen.
    matches!(
        &lines[question + 1..],
        [choices, footer] if choices.contains("[0] Skip") && footer == "? for shortcuts"
    )
}

/// agy's input box as its pane shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputBox {
    /// The text in the box, its lines joined with newlines.
    pub text: String,
    /// How many screen lines the box's text takes.
    pub rows: usize,
    /// Whether the footer under the box says a turn is running.
    pub working: bool,
    /// Whether the footer shows agy's idle hint, "? for shortcuts". agy
    /// shows one hint at a time and "esc to cancel" has the lowest
    /// priority: a queued message, a second-press prompt, bash or code
    /// search mode, or a transient hint replaces it while the turn keeps
    /// running. The idle hint appears only when no turn runs and no other
    /// hint is shown.
    pub idle: bool,
    /// Whether the footer shows agy holding messages for its next turn,
    /// "Press up to edit queued messages".
    pub queued: bool,
    /// The messages agy holds for its next turn, from the `▸` rows it draws
    /// right above the box, as far as each row shows.
    pub queued_inputs: Vec<String>,
}

/// Reads agy's input box from the pane's visible lines: the lowest line that
/// starts with `>` right below a rule, and the lines under it up to the next
/// rule. agy's bottom line reads "esc to cancel" while a turn runs.
/// agy 1.5 draws a placeholder in the empty box, such as "Auto mode: edits
/// & safe commands auto-approved", in grey (ANSI bright black); it is not
/// the box's text. Typed or pasted text is drawn in the default color.
pub fn input_box(lines: &[wakterm_term::Line]) -> Option<InputBox> {
    let plain = |line: &wakterm_term::Line| line.as_str().trim_end().to_string();
    // The box's text without its grey placeholder; agy draws its rules and
    // hints in the same grey, so only the box's own rows are read this way.
    let typed = |line: &wakterm_term::Line| {
        line.visible_cells()
            .map(|cell| {
                if cell.attrs().foreground() == wakterm_term::color::ColorAttribute::PaletteIndex(8)
                {
                    " ".repeat(cell.width())
                } else {
                    cell.str().to_string()
                }
            })
            .collect::<String>()
            .trim()
            .to_string()
    };
    let is_rule = |line: &str| line.trim_start().starts_with('\u{2500}');
    let screen = lines;
    let lines: Vec<String> = screen.iter().map(plain).collect();
    let prompt = (1..lines.len())
        .rev()
        .find(|&i| is_rule(&lines[i - 1]) && lines[i].starts_with('>'))?;
    let end = (prompt + 1..lines.len()).find(|&i| is_rule(&lines[i]))?;
    let mut content = vec![typed(&screen[prompt])
        .strip_prefix('>')
        .unwrap_or_default()
        .trim()
        .to_string()];
    content.extend(screen[prompt + 1..end].iter().map(typed));
    while content.last().is_some_and(String::is_empty) && content.len() > 1 {
        content.pop();
    }
    // The hint is on agy's bottom line. Panels such as the commands a turn
    // runs can sit between the box and it, and hold any text.
    let footer = lines[end + 1..]
        .iter()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(String::as_str)
        .unwrap_or_default();
    let working = footer.contains("esc to cancel");
    let idle = footer.contains("? for shortcuts");
    let queued = footer.contains("to edit queued messages");
    let mut queued_inputs: Vec<String> = lines[..prompt - 1]
        .iter()
        .rev()
        .map_while(|line| line.trim_start().strip_prefix('\u{25b8}'))
        .map(|row| {
            row.trim()
                .trim_end_matches("(alt+m to send immediately)")
                .trim()
                .to_string()
        })
        .collect();
    queued_inputs.reverse();
    Some(InputBox {
        rows: content.len(),
        text: content.join("\n").trim().to_string(),
        working,
        idle,
        queued,
        queued_inputs,
    })
}

pub(crate) fn observe(
    _cwd: &str,
    preferred_session: Option<&str>,
    _updated_after: Option<DateTime<Utc>>,
    process_id: Option<u32>,
    process_start_time: Option<u64>,
    expected_conversation_id: Option<&str>,
) -> anyhow::Result<Option<HarnessObservation>> {
    let Some(root) = root() else {
        return Ok(None);
    };
    let Some(transcript) = session_owned_by_process(
        &root,
        process_id,
        process_start_time,
        preferred_session,
        expected_conversation_id,
    )?
    else {
        return Ok(None);
    };
    let modified_at = DateTime::<Utc>::from(fs::metadata(&transcript)?.modified()?);
    let details = read_last_observation(&transcript)?;
    Ok(Some(HarnessObservation::from_details(
        &transcript,
        details,
        modified_at,
    )))
}

#[cfg(target_os = "linux")]
pub(crate) fn session_owned_by_process(
    root: &Path,
    process_id: Option<u32>,
    process_start_time: Option<u64>,
    preferred_session: Option<&str>,
    expected_conversation_id: Option<&str>,
) -> anyhow::Result<Option<PathBuf>> {
    let (Some(process_id), Some(process_start_time)) = (process_id, process_start_time) else {
        return Ok(None);
    };
    if crate::agent::linux_process_started_at(Some(process_id), Some(process_start_time)).is_none()
    {
        return Ok(None);
    }

    let presence_root = root.join("presence");
    let canonical_presence_root = presence_root
        .canonicalize()
        .unwrap_or_else(|_| presence_root.clone());
    let canonical_preferred = preferred_session
        .map(Path::new)
        .map(|path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf()));
    let mut candidates = Vec::new();

    let Ok(entries) = fs::read_dir(format!("/proc/{process_id}/fd")) else {
        return Ok(None);
    };
    for entry in entries.flatten() {
        let Ok(path) = fs::read_link(entry.path()) else {
            continue;
        };
        let canonical_path = path.canonicalize().unwrap_or(path);
        if canonical_path.parent() != Some(canonical_presence_root.as_path())
            || canonical_path.extension().and_then(|ext| ext.to_str()) != Some("lock")
        {
            continue;
        }
        let Some(conversation_id) = canonical_path.file_stem().and_then(|name| name.to_str())
        else {
            continue;
        };
        if !is_uuid(conversation_id) {
            continue;
        }
        if expected_conversation_id.is_some_and(|expected| expected != conversation_id) {
            continue;
        }
        let transcript = root
            .join("brain")
            .join(conversation_id)
            .join(".system_generated")
            .join("logs")
            .join("transcript.jsonl");
        if !transcript.is_file() {
            continue;
        }
        let canonical_transcript = transcript.canonicalize().unwrap_or(transcript);
        if canonical_preferred.as_ref() == Some(&canonical_transcript) {
            return Ok(Some(canonical_transcript));
        }
        candidates.push(canonical_transcript);
    }

    candidates.sort_unstable();
    candidates.dedup();
    Ok((candidates.len() == 1).then(|| candidates.remove(0)))
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn session_owned_by_process(
    _root: &Path,
    _process_id: Option<u32>,
    _process_start_time: Option<u64>,
    _preferred_session: Option<&str>,
    _expected_conversation_id: Option<&str>,
) -> anyhow::Result<Option<PathBuf>> {
    Ok(None)
}

pub(crate) fn read_last_observation(path: &Path) -> anyhow::Result<HarnessObservationDetails> {
    let conversation_id = path
        .ancestors()
        .nth(3)
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .unwrap_or("agy");
    // The steps from the latest user input on, read back from the end, and
    // the last step before it, which ended the previous turn.
    let mut tail = Vec::new();
    let mut found_input = false;
    let mut previous_turn_end = None;
    visit_lines_reverse(path, |line| {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            return Ok(false);
        };
        let record_type = record.get("type").and_then(Value::as_str);
        if found_input {
            if record_type == Some("SYSTEM_MESSAGE") {
                return Ok(false);
            }
            previous_turn_end = record
                .get("created_at")
                .and_then(Value::as_str)
                .and_then(parse_rfc3339_timestamp);
            return Ok(true);
        }
        found_input = record_type == Some("USER_INPUT");
        tail.push(record);
        Ok(false)
    })?;
    tail.reverse();
    let created_at = |record: &Value| {
        record
            .get("created_at")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339_timestamp)
    };
    let latest = tail
        .iter()
        .rev()
        .find(|record| record.get("type").and_then(Value::as_str) != Some("SYSTEM_MESSAGE"));
    let Some(latest) = latest else {
        return Ok(HarnessObservationDetails {
            progress_summary: None,
            harness_mode: None,
            turn_phase: None,
            updated_at: None,
            turn_state: AgentTurnState::Unknown,
            last_turn_completed_at: None,
            observed_turn: None,
        });
    };
    let latest_at = created_at(latest);
    let latest_cursor = latest.get("step_index").and_then(Value::as_u64);

    // The turn the steps show by the rule the event projector follows: the
    // latest turn's start, by the user or by a system notice, and its end.
    let mut tracker = TurnTracker::default();
    let mut start: Option<(&Value, bool)> = None;
    let mut end: Option<&Value> = None;
    let mut interim: Option<&Value> = None;
    let mut last_end_at = previous_turn_end;
    for record in &tail {
        if record_time(record).is_some_and(|at| tracker.expire(at)) {
            end = interim;
            last_end_at = interim.and_then(created_at).or(last_end_at);
        }
        match tracker.step(record) {
            TurnStep::StartedByUser => {
                start = Some((record, true));
                end = None;
                interim = None;
            }
            TurnStep::StartedBySystem => {
                start = Some((record, false));
                end = None;
                interim = None;
            }
            TurnStep::Interim => interim = Some(record),
            TurnStep::Ended => {
                end = Some(record);
                last_end_at = created_at(record).or(last_end_at);
            }
            TurnStep::Other => {}
        }
    }
    // A turn that waited too long for a background task's notice ended with
    // its interim answer.
    if tracker.expire(Utc::now()) {
        end = interim;
        last_end_at = interim.and_then(created_at).or(last_end_at);
    }
    let Some((start, by_user)) = start else {
        return Ok(HarnessObservationDetails {
            progress_summary: None,
            harness_mode: None,
            turn_phase: None,
            updated_at: latest_at,
            turn_state: AgentTurnState::Unknown,
            last_turn_completed_at: None,
            observed_turn: None,
        });
    };
    let start_cursor = start.get("step_index").and_then(Value::as_u64);
    let user_message = by_user
        .then(|| start.get("content").and_then(Value::as_str))
        .flatten()
        .and_then(user_request)
        .filter(|message| !message.is_empty());
    let final_message = end
        .and_then(|end| end.get("content").and_then(Value::as_str))
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .map(str::to_string);
    let completed_at = end.and_then(created_at);
    let (outcome, turn_state, turn_phase) = if end.is_some() {
        (
            AgentObservedTurnOutcome::Completed,
            AgentTurnState::WaitingOnUser,
            "final_answer",
        )
    } else {
        (
            AgentObservedTurnOutcome::Running,
            AgentTurnState::WaitingOnAgent,
            "working",
        )
    };
    let provider_turn_id =
        turn_id(conversation_id, start).unwrap_or_else(|| format!("{conversation_id}:current"));

    Ok(HarnessObservationDetails {
        progress_summary: final_message.as_deref().map(truncate_summary),
        harness_mode: None,
        turn_phase: Some(turn_phase.to_string()),
        updated_at: latest_at,
        turn_state,
        // While a turn runs, the latest completed turn is an earlier one.
        last_turn_completed_at: completed_at.or(last_end_at),
        observed_turn: Some(AgentObservedTurn {
            provider_turn_id,
            outcome,
            started_at: created_at(start),
            completed_at,
            started_cursor: start_cursor,
            latest_cursor,
            primary_user_message_sha256: user_message.map(message_sha256),
            user_message_count: u32::from(by_user),
            final_message,
        }),
    })
}

pub(crate) fn describe_pending(
    _cwd: &str,
    _updated_after: Option<DateTime<Utc>>,
) -> anyhow::Result<Option<String>> {
    let Some(root) = root() else {
        return Ok(None);
    };
    Ok(Some(if root.join("presence").is_dir() {
        "agy process has not exposed a matching conversation transcript yet".to_string()
    } else {
        "agy conversation storage has not appeared yet".to_string()
    }))
}

pub(crate) fn root() -> Option<PathBuf> {
    std::env::var_os("WAKTERM_AGENT_AGY_DIR")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join(".gemini").join("antigravity-cli")))
}

/// The events one step of an agy transcript adds, with the projection's
/// state in `cursor`, for the shared incremental reader of session logs.
pub(crate) fn project_record(
    offset: u64,
    record: &Value,
    cursor: &mut JsonlCursor,
) -> anyhow::Result<Vec<PendingEvent>> {
    let timestamp = event_time(record);
    let step = record
        .get("step_index")
        .and_then(Value::as_u64)
        .map(|step| step.to_string())
        .unwrap_or_else(|| format!("offset-{offset}"));
    let record_key = format!("agy:{}:{step}", cursor.source_id);
    // A wait that ran out before this step ends its turn first.
    let mut expired = record_time(record)
        .map(|at| expire_background_wait(cursor, at))
        .unwrap_or_default();
    let mut tracker = TurnTracker {
        open: cursor.turn_open,
        pending: cursor.background_pending,
        waiting_since: cursor.background_waiting_since,
    };
    let step_kind = tracker.step(record);
    cursor.background_pending = tracker.pending;
    cursor.background_waiting_since = tracker.waiting_since;
    let events = (|| -> anyhow::Result<Vec<PendingEvent>> {
        match step_kind {
            TurnStep::StartedByUser | TurnStep::StartedBySystem => {
                let Some(turn_id) = turn_id(&cursor.source_id, record) else {
                    return Ok(vec![observer_failure(
                        format!("{record_key}:missing-turn"),
                        timestamp,
                        None,
                        "agy step lacks a step index",
                    )]);
                };
                cursor.current_turn_id = Some(turn_id.clone());
                cursor.last_assistant_text = None;
                cursor.turn_open = true;
                let mut started = PendingEvent::new(
                    format!("{record_key}:started"),
                    AgentEventKind::TurnStarted,
                    timestamp,
                );
                started.turn_id = Some(turn_id.clone());
                // A turn a system notice starts has no user input.
                started.input_sha256 = (step_kind == TurnStep::StartedByUser)
                    .then(|| record.get("content").and_then(Value::as_str))
                    .flatten()
                    .and_then(user_request)
                    .map(crate::agent::message_sha256);
                let accepted = started.input_sha256.clone().map(|sha256| {
                    input_accepted(
                        format!("{record_key}:input"),
                        timestamp,
                        Some(turn_id.clone()),
                        sha256,
                        false,
                    )
                });
                let mut state = PendingEvent::new(
                    format!("{record_key}:state"),
                    AgentEventKind::TurnStateChanged,
                    timestamp,
                );
                state.turn_id = Some(turn_id);
                state.turn_state = Some("waiting_on_agent".to_string());
                Ok(vec![Some(started), accepted, Some(state)]
                    .into_iter()
                    .flatten()
                    .collect())
            }
            _ if record.get("type").and_then(Value::as_str) == Some("PLANNER_RESPONSE")
                && record.get("status").and_then(Value::as_str) == Some("DONE") =>
            {
                let Some(turn_id) = cursor.current_turn_id.clone() else {
                    return Ok(Vec::new());
                };
                let text = record
                    .get("content")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .map(str::to_string);
                let mut events = Vec::new();
                if let Some(text) = &text {
                    cursor.last_assistant_text = Some(text.clone());
                    let mut message = PendingEvent::new(
                        format!("{record_key}:message"),
                        AgentEventKind::AssistantMessage,
                        timestamp,
                    );
                    message.turn_id = Some(turn_id.clone());
                    message.text = Some(text.clone());
                    events.push(message);
                }
                // A response without text that calls no tools still ends
                // the turn, without a final answer.
                if step_kind == TurnStep::Ended {
                    let mut final_event = PendingEvent::new(
                        format!("{record_key}:final"),
                        AgentEventKind::TurnFinal,
                        timestamp,
                    );
                    final_event.turn_id = Some(turn_id.clone());
                    final_event.outcome = Some("completed".to_string());
                    final_event.text = text;
                    let mut state = PendingEvent::new(
                        format!("{record_key}:final-state"),
                        AgentEventKind::TurnStateChanged,
                        timestamp,
                    );
                    state.turn_id = Some(turn_id);
                    state.turn_state = Some("waiting_on_user".to_string());
                    events.extend([final_event, state]);
                    cursor.turn_open = false;
                }
                Ok(events)
            }
            _ => Ok(Vec::new()),
        }
    })()?;
    expired.extend(events);
    Ok(expired)
}

/// The events that end an agy turn whose wait for a background task's
/// notice has run past `BACKGROUND_WAIT_LIMIT`, with its interim answer as
/// the final one.
pub(crate) fn expire_background_wait(
    cursor: &mut JsonlCursor,
    now: DateTime<Utc>,
) -> Vec<PendingEvent> {
    let mut tracker = TurnTracker {
        open: cursor.turn_open,
        pending: cursor.background_pending,
        waiting_since: cursor.background_waiting_since,
    };
    let Some(since) = tracker.waiting_since else {
        return Vec::new();
    };
    if !tracker.expire(now) {
        return Vec::new();
    }
    cursor.turn_open = false;
    cursor.background_pending = 0;
    cursor.background_waiting_since = None;
    let Some(turn_id) = cursor.current_turn_id.clone() else {
        return Vec::new();
    };
    let record_key = format!("agy:{}:{turn_id}:background-wait", cursor.source_id);
    let at = since + BACKGROUND_WAIT_LIMIT;
    let mut final_event =
        PendingEvent::new(format!("{record_key}:final"), AgentEventKind::TurnFinal, at);
    final_event.turn_id = Some(turn_id.clone());
    final_event.outcome = Some("completed".to_string());
    final_event.text = cursor.last_assistant_text.clone();
    let mut state = PendingEvent::new(
        format!("{record_key}:state"),
        AgentEventKind::TurnStateChanged,
        at,
    );
    state.turn_id = Some(turn_id);
    state.turn_state = Some("waiting_on_user".to_string());
    vec![final_event, state]
}
