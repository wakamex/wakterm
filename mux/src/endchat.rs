//! endchat, a command-line chat that never ends, keeps each chat as a
//! profile directory: `~/.endchat/NAME`, or any directory when the name
//! contains a `/`. Its `state.json` says whether a turn is running, since
//! when and its latest step, and `turns.jsonl` gains a line as each turn
//! ends. The profile is the whole conversation, so running the same
//! command again resumes it.

use chrono::{DateTime, Utc};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// The profile an interactive endchat command chats with: `endchat` alone
/// chats with `main`, `endchat chat NAME` with NAME. None for its other
/// commands, such as `serve` or `say`, which are not a chat in a pane.
pub fn profile_name(launch_cmd: &str) -> Option<String> {
    let argv = shell_words::split(launch_cmd).ok()?;
    let mut args = argv.iter().skip(1).filter(|arg| !arg.starts_with('-'));
    match args.next().map(String::as_str) {
        None => Some("main".to_string()),
        Some("chat") => Some(args.next().cloned().unwrap_or_else(|| "main".to_string())),
        Some(_) => None,
    }
}

/// The directory of profile `name` for a command run in `cwd`.
pub fn profile_dir(name: &str, cwd: &str) -> Option<PathBuf> {
    if name.contains('/') {
        let path = Path::new(name);
        return Some(if path.is_absolute() {
            path.to_path_buf()
        } else {
            Path::new(cwd).join(path)
        });
    }
    Some(crate::agent::home_dir()?.join(".endchat").join(name))
}

/// The profile directory of an endchat launch command run in `cwd`.
pub fn profile_dir_of(launch_cmd: &str, cwd: &str) -> Option<PathBuf> {
    profile_dir(&profile_name(launch_cmd)?, cwd)
}

/// What a profile's `state.json` says it is doing.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct State {
    pub busy: bool,
    /// When the running turn started.
    pub since: Option<DateTime<Utc>>,
    /// The running turn's latest step.
    pub doing: Option<String>,
    /// Messages waiting for the running turn.
    pub queued: u64,
}

pub fn read_state(dir: &Path) -> anyhow::Result<Option<State>> {
    let text = match std::fs::read_to_string(dir.join("state.json")) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let record: Value = serde_json::from_str(&text)?;
    Ok(Some(State {
        busy: record.get("busy").and_then(Value::as_bool).unwrap_or(false),
        since: record
            .get("since")
            .and_then(Value::as_str)
            .and_then(|since| DateTime::parse_from_rfc3339(since).ok())
            .map(|since| since.with_timezone(&Utc)),
        doing: record
            .get("doing")
            .and_then(Value::as_str)
            .map(str::to_string),
        queued: record.get("queued").and_then(Value::as_u64).unwrap_or(0),
    }))
}

/// A turn that ended, from `turns.jsonl`.
#[derive(Debug, PartialEq, Eq)]
pub struct EndedTurn {
    /// How many turns had ended before it.
    pub index: usize,
    pub ended_at: DateTime<Utc>,
    /// Whether the turn ended with a reply, rather than failing or timing
    /// out.
    pub ok: bool,
}

/// The latest turn that ended, and how many have.
pub fn last_ended_turn(dir: &Path) -> anyhow::Result<Option<EndedTurn>> {
    let text = match std::fs::read_to_string(dir.join("turns.jsonl")) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let Some(last) = lines.last() else {
        return Ok(None);
    };
    let record: Value = serde_json::from_str(last)?;
    let Some(ended_at) = record
        .get("date")
        .and_then(Value::as_str)
        .and_then(|date| DateTime::parse_from_rfc3339(date).ok())
    else {
        return Ok(None);
    };
    Ok(Some(EndedTurn {
        index: lines.len() - 1,
        ended_at: ended_at.with_timezone(&Utc),
        ok: record.get("ok").and_then(Value::as_bool).unwrap_or(false),
    }))
}

/// The text typed into endchat's prompt, `›` on its last line.
pub fn input_box(lines: &[wakterm_term::Line]) -> Option<String> {
    let prompt = lines
        .iter()
        .rev()
        .map(|line| line.as_str().trim_end().to_string())
        .find(|line| !line.is_empty())?;
    Some(
        prompt
            .trim_start()
            .strip_prefix('\u{203a}')?
            .trim()
            .to_string(),
    )
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn the_profile_comes_from_the_chat_command() {
        assert_eq!(profile_name("endchat").as_deref(), Some("main"));
        assert_eq!(profile_name("endchat -v").as_deref(), Some("main"));
        assert_eq!(
            profile_name("endchat chat w3end -v").as_deref(),
            Some("w3end")
        );
        assert_eq!(
            profile_name("/home/u/.cargo/bin/endchat chat").as_deref(),
            Some("main")
        );
        assert_eq!(profile_name("endchat serve main"), None);
        assert_eq!(profile_name("endchat say main hello"), None);
        assert_eq!(
            profile_dir("work/chat", "/code/w3sim"),
            Some(PathBuf::from("/code/w3sim/work/chat"))
        );
        // A named profile lives under HOME, which Windows does not set.
        if crate::agent::home_dir().is_some() {
            assert!(profile_dir("w3end", "/code")
                .unwrap()
                .ends_with(".endchat/w3end"));
        }
    }

    #[test]
    fn state_and_ended_turns_are_read_from_the_profile() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_state(dir.path()).unwrap(), None);
        assert_eq!(last_ended_turn(dir.path()).unwrap(), None);
        std::fs::write(
            dir.path().join("state.json"),
            r#"{"busy":true,"since":"2026-11-16T11:31:00Z","effort":"medium","doing":"Bash","queued":1}"#,
        )
        .unwrap();
        let state = read_state(dir.path()).unwrap().unwrap();
        assert!(state.busy);
        assert_eq!(state.doing.as_deref(), Some("Bash"));
        assert_eq!(state.queued, 1);
        assert_eq!(
            state.since,
            Some("2026-11-16T11:31:00Z".parse::<DateTime<Utc>>().unwrap())
        );
        std::fs::write(
            dir.path().join("turns.jsonl"),
            "{\"date\":\"2026-11-16T11:30:07Z\",\"end\":3340,\"ok\":true}\n\
             {\"date\":\"2026-11-16T11:33:09Z\",\"end\":3352,\"ok\":false}\n",
        )
        .unwrap();
        let turn = last_ended_turn(dir.path()).unwrap().unwrap();
        assert_eq!(turn.index, 1);
        assert!(!turn.ok);
        assert_eq!(
            turn.ended_at,
            "2026-11-16T11:33:09Z".parse::<DateTime<Utc>>().unwrap()
        );
    }

    #[test]
    fn the_prompt_holds_what_is_typed() {
        let screen = |rows: &[&str]| {
            rows.iter()
                .map(|row| wakterm_term::Line::from_text(row, &Default::default(), 0, None))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            input_box(&screen(&["\u{00b7} idle after 2 min", "\u{203a} "])).as_deref(),
            Some("")
        );
        assert_eq!(
            input_box(&screen(&["\u{00b7} idle", "\u{203a} half a thought", ""])).as_deref(),
            Some("half a thought")
        );
        assert_eq!(input_box(&screen(&["some output"])), None);
    }
}
