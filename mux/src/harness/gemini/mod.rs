//! Gemini CLI's sessions, which it keeps as chat files of messages: the
//! user's (`type` "user") and Gemini's (`type` "gemini"), whose tool calls,
//! if any, it records on the message that makes them.

use serde_json::Value;

/// Whether a message starts a turn: input the user sent. Both of Wakterm's
/// readers of a session follow this and `ends_turn`.
pub(crate) fn starts_turn(message: &Value) -> bool {
    message.get("type").and_then(Value::as_str) == Some("user")
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
