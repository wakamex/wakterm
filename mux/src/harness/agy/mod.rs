//! agy's step log: a JSONL transcript of steps, each with a `type`, such as
//! `USER_INPUT`, `PLANNER_RESPONSE` or `SYSTEM_MESSAGE`, and a `status`.

use serde_json::Value;

/// What a step does to the turn it arrives in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TurnStep {
    /// The user's input starts a turn.
    StartedByUser,
    /// A system notice that arrives while no turn runs, such as a background
    /// task finishing, wakes agy to work on it: a turn without user input.
    StartedBySystem,
    /// A finished response that calls no tools ends the turn.
    Ended,
    Other,
}

/// Whether an agy turn runs, by the one rule both of Wakterm's readers of a
/// transcript follow: the observer from the latest user input, the event
/// projector from where its cursor left off. A step whose status is `ERROR`
/// does not end a turn: agy goes on after it.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TurnTracker {
    pub open: bool,
}

impl TurnTracker {
    pub(crate) fn step(&mut self, record: &Value) -> TurnStep {
        match record.get("type").and_then(Value::as_str) {
            Some("USER_INPUT") => {
                self.open = true;
                TurnStep::StartedByUser
            }
            Some("SYSTEM_MESSAGE") if !self.open => {
                self.open = true;
                TurnStep::StartedBySystem
            }
            Some("PLANNER_RESPONSE")
                if self.open
                    && record.get("status").and_then(Value::as_str) == Some("DONE")
                    && !calls_tools(record) =>
            {
                self.open = false;
                TurnStep::Ended
            }
            _ => TurnStep::Other,
        }
    }
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
