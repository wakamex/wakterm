//! Meta's Muse Code CLI, which keeps each session as an event log.
//!
//! A session lives at `<data>/muse/sessions/YYYY/MM/DD/<session uuid>/
//! session.jsonl`, under `XDG_DATA_HOME` or `~/.local/share`, and
//! `<data>/muse/session-index.db` lists every session with its log and
//! workspace root. Each log line is one record with a `payload_type`; the
//! ones a turn consists of are:
//!
//! - `runtime.user_intent.accepted`: the user's input, with its text in
//!   `model_messages` and an `intent_id` that a top-level turn's run takes as
//!   its `run_id`;
//! - `runtime.session` with a `run` event `assistant_message_committed`: a
//!   reply's text;
//! - `runtime.session` with a `run` event `terminal`: the end of the run,
//!   `terminal: "completed"` or another outcome;
//! - `runtime.session` with `run` events `inbox_item_queued`, source
//!   `user_steer`, and `inbox_item_drained`, linked by the queued record's
//!   `source_run_record_id` and the drained one's `item_id`: input sent while
//!   a run works, which Muse queues and later hands to a run, either the one
//!   working or, when that ends first, a new run answering it under the
//!   input's `intent_id`;
//! - `runtime.session` with `approval` events `requested` and
//!   `decision_applied`, sharing a `pending_action_id`: a permission request
//!   and its answer, which Muse's automated reviewer can also give.
//!
//! Wrapped records with `children` repeat records written elsewhere and are
//! skipped.

use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{Connection, OptionalExtension};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Muse's data directory for the Muse process `pid`: `muse` under its
/// environment's XDG_DATA_HOME, `~/.local/share` by default.
pub(crate) fn data_dir_for(pid: Option<u32>) -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("WAKTERM_AGENT_MUSE_DIR") {
        return Some(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from);
    crate::agent::harness_dir(
        pid,
        "XDG_DATA_HOME",
        home.map(|home| home.join(".local").join("share")),
    )
    .map(|data| data.join("muse"))
}

fn open_index(data_dir: &Path) -> anyhow::Result<Option<Connection>> {
    let path = data_dir.join("session-index.db");
    if !path.is_file() {
        return Ok(None);
    }
    let connection =
        Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.busy_timeout(std::time::Duration::from_secs(2))?;
    Ok(Some(connection))
}

/// The log of the session `session_id`, from the index.
pub(crate) fn session_log(data_dir: &Path, session_id: &str) -> anyhow::Result<Option<PathBuf>> {
    let Some(index) = open_index(data_dir)? else {
        return Ok(None);
    };
    Ok(index
        .query_row(
            "select session_log_path from sessions where session_id = ?1",
            [session_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .map(PathBuf::from))
}

/// The log of the session the Muse process `pid` runs: Muse holds its
/// session directory's `.session.lock` open for as long as it runs the
/// session, while it adds the session to its index only some seconds after
/// starting. Subagents' sessions, under `subagent/`, are not the process's
/// own.
#[cfg(target_os = "linux")]
pub(crate) fn live_session_log(pid: u32, data_dir: &Path) -> Option<PathBuf> {
    let sessions = data_dir.join("sessions");
    std::fs::read_dir(format!("/proc/{pid}/fd"))
        .ok()?
        .flatten()
        .filter_map(|fd| std::fs::read_link(fd.path()).ok())
        .filter(|target| {
            target
                .file_name()
                .is_some_and(|name| name == ".session.lock")
                && target.starts_with(&sessions)
                && !target
                    .components()
                    .any(|part| part.as_os_str() == "subagent")
        })
        .filter_map(|lock| Some(lock.parent()?.join("session.jsonl")))
        .find(|log| session_id_of(log).is_some())
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn live_session_log(_pid: u32, _data_dir: &Path) -> Option<PathBuf> {
    None
}

/// The log of the most recently updated session whose workspace root is
/// `cwd` and that was updated at or after `updated_after`.
pub(crate) fn newest_session_log_in(
    data_dir: &Path,
    cwd: &str,
    updated_after: Option<DateTime<Utc>>,
) -> anyhow::Result<Option<PathBuf>> {
    let Some(index) = open_index(data_dir)? else {
        return Ok(None);
    };
    Ok(index
        .query_row(
            "select session_log_path from sessions where workspace_root = ?1 \
             and coalesce(updated_at_us, created_at_us, 0) >= ?2 \
             order by coalesce(updated_at_us, created_at_us, 0) desc limit 1",
            rusqlite::params![
                cwd,
                updated_after
                    .map(|at| at.timestamp_micros())
                    .unwrap_or(i64::MIN)
            ],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .map(PathBuf::from))
}

/// The session a launch command resumes: `muse resume UUID`, or
/// `--session-id UUID`.
pub(crate) fn resumed_session(launch_cmd: &str) -> Option<String> {
    let argv = shell_words::split(launch_cmd).ok()?;
    let mut args = argv.iter().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "resume" | "--session-id" => {
                return args.next().filter(|id| crate::agent::is_uuid(id)).cloned()
            }
            _ => {
                if let Some(id) = arg.strip_prefix("--session-id=") {
                    return crate::agent::is_uuid(id).then(|| id.to_string());
                }
            }
        }
    }
    None
}

/// The session ID a log belongs to: the name of its directory.
pub(crate) fn session_id_of(log: &Path) -> Option<String> {
    log.parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .filter(|name| crate::agent::is_uuid(name))
        .map(str::to_string)
}

/// A record of a Muse session log that a turn consists of.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Record {
    Input {
        intent_id: String,
        text: String,
    },
    Assistant {
        run_id: String,
        text: String,
    },
    Terminal {
        run_id: String,
        completed: bool,
    },
    /// Input queued for the working run `run_id` as item `item_id`.
    SteerQueued {
        run_id: String,
        item_id: String,
    },
    /// Queued item `item_id` handed to run `run_id`.
    SteerDrained {
        run_id: String,
        item_id: String,
    },
    /// A permission request. `command` is set for a request Wakterm can
    /// answer: a one-stage shell command waiting on the user, with the
    /// choices to allow it once and to abort it.
    ApprovalRequested {
        run_id: String,
        action_id: String,
        command: Option<String>,
    },
    ApprovalDecided {
        run_id: String,
        action_id: String,
    },
}

/// When a record was written: `recorded_at`, in microseconds.
pub(crate) fn recorded_at(record: &Value) -> Option<DateTime<Utc>> {
    let micros = record.get("recorded_at")?.as_i64()?;
    Utc.timestamp_micros(micros).single()
}

pub(crate) fn parse(record: &Value) -> Option<Record> {
    if record.get("children").is_some() {
        return None;
    }
    let payload = record.get("payload")?;
    let text = |value: &Value| value.as_str().map(str::to_string);
    match record.get("payload_type")?.as_str()? {
        "runtime.user_intent.accepted" => {
            let text = payload
                .get("model_messages")?
                .as_array()?
                .iter()
                .filter_map(|message| message.get("content")?.as_array().cloned())
                .flatten()
                .filter(|block| block.get("kind").and_then(Value::as_str) == Some("text"))
                .filter_map(|block| {
                    block
                        .get("text")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect::<Vec<_>>()
                .join("\n");
            Some(Record::Input {
                intent_id: text_or_none(payload.get("intent_id"))?,
                text,
            })
        }
        "runtime.session" => {
            let event = payload.get("event")?;
            let run_id = text(payload.get("run_id")?)?;
            match (payload.get("kind")?.as_str()?, event.get("kind")?.as_str()?) {
                ("run", "assistant_message_committed") => Some(Record::Assistant {
                    run_id,
                    text: text(event.get("text")?)?,
                }),
                ("run", "terminal") => Some(Record::Terminal {
                    run_id,
                    completed: event.get("terminal").and_then(Value::as_str) == Some("completed"),
                }),
                ("run", "inbox_item_queued")
                    if event.pointer("/source/source").and_then(Value::as_str)
                        == Some("user_steer") =>
                {
                    Some(Record::SteerQueued {
                        run_id,
                        item_id: text(payload.get("source_run_record_id")?)?,
                    })
                }
                ("run", "inbox_item_drained") => Some(Record::SteerDrained {
                    run_id: event
                        .pointer("/drain_target_run_stream/id")
                        .and_then(Value::as_str)
                        .map_or(run_id, str::to_string),
                    item_id: text(event.get("item_id")?)?,
                }),
                ("approval", "requested") => Some(Record::ApprovalRequested {
                    run_id,
                    action_id: text(event.get("pending_action_id")?)?,
                    command: answerable_command(event),
                }),
                ("approval", "decision_applied") => Some(Record::ApprovalDecided {
                    run_id,
                    action_id: text(event.get("pending_action_id")?)?,
                }),
                _ => None,
            }
        }
        _ => None,
    }
}

fn answerable_command(event: &Value) -> Option<String> {
    let subject = event.get("approval_subject")?;
    let choices: Vec<&str> = event
        .get("available_choices")?
        .as_array()?
        .iter()
        .filter_map(|choice| choice.get("choice_id")?.as_str())
        .collect();
    (event.get("presentation_phase")?.as_str()? == "human_pending"
        && subject.get("kind")?.as_str()? == "shell_command"
        && subject.get("stages")?.as_array()?.len() == 1
        && choices.contains(&"allow_once")
        && choices.contains(&"abort"))
    .then(|| subject.get("raw_command")?.as_str().map(str::to_string))
    .flatten()
}

/// Whether the session log `log` shows permission request `action_id` of
/// its latest turn still waiting for a decision.
pub(crate) fn approval_pending(log: &Path, action_id: &str) -> anyhow::Result<bool> {
    Ok(read_turn(log)?
        .pending_approvals
        .iter()
        .any(|pending| pending == action_id))
}

fn text_or_none(value: Option<&Value>) -> Option<String> {
    value?.as_str().map(str::to_string)
}

/// What a session log says about its latest turn.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TurnView {
    /// The latest top-level turn's run ID and input, if any.
    pub turn_id: Option<String>,
    pub input: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    /// Further input the user sent while that turn ran.
    pub inputs: u32,
    /// Runs answering that input, and the queued items not yet handed to
    /// one. The turn ends with a run of its own once nothing is queued.
    pub steer_runs: Vec<String>,
    pub queued_steers: Vec<String>,
    pub reply: Option<String>,
    /// Whether the turn ended, and whether it completed.
    pub ended: Option<bool>,
    pub ended_at: Option<DateTime<Utc>>,
    /// Permission requests of the turn without a decision.
    pub pending_approvals: Vec<String>,
    pub updated_at: Option<DateTime<Utc>>,
}

impl TurnView {
    fn owns(&self, run_id: &str) -> bool {
        self.turn_id.as_deref() == Some(run_id) || self.steer_runs.iter().any(|run| run == run_id)
    }
}

/// Reads a session log into the view of its latest turn.
pub(crate) fn read_turn(log: &Path) -> anyhow::Result<TurnView> {
    let mut view = TurnView::default();
    for line in std::fs::read_to_string(log)?.lines() {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let at = recorded_at(&record);
        if at.is_some() {
            view.updated_at = at;
        }
        match parse(&record) {
            Some(Record::Input { intent_id, text }) => {
                if view.turn_id.is_some() && view.ended.is_none() {
                    view.inputs += 1;
                } else {
                    view = TurnView {
                        turn_id: Some(intent_id),
                        input: Some(text),
                        started_at: at,
                        inputs: 1,
                        updated_at: at,
                        ..TurnView::default()
                    };
                }
            }
            Some(Record::Assistant { run_id, text }) if view.owns(&run_id) => {
                view.reply = Some(text);
            }
            Some(Record::SteerQueued { run_id, item_id }) if view.owns(&run_id) => {
                view.queued_steers.push(item_id);
            }
            Some(Record::SteerDrained { run_id, item_id })
                if view.queued_steers.contains(&item_id) =>
            {
                view.queued_steers.retain(|queued| *queued != item_id);
                if !view.owns(&run_id) {
                    view.steer_runs.push(run_id);
                }
            }
            Some(Record::Terminal { run_id, completed })
                if view.owns(&run_id) && view.queued_steers.is_empty() =>
            {
                view.ended = Some(completed);
                view.ended_at = at;
                view.pending_approvals.clear();
            }
            Some(Record::ApprovalRequested {
                run_id, action_id, ..
            }) if view.owns(&run_id) => {
                view.pending_approvals.push(action_id);
            }
            Some(Record::ApprovalDecided { action_id, .. }) => {
                view.pending_approvals
                    .retain(|pending| *pending != action_id);
            }
            _ => {}
        }
    }
    Ok(view)
}

/// The text in Muse's input box, read from the pane's visible lines: the
/// lines between the two lowest rules, the first starting with `❯`. A draft
/// longer than one line continues on lines indented by two spaces. An empty
/// box can show a tip, such as "Type @ to search and insert workspace file
/// paths", drawn in the rules' color, which is not text in the box. A
/// permission prompt or other menu replaces the box.
pub fn input_box(lines: &[wakterm_term::Line]) -> Option<String> {
    let is_rule = |line: &wakterm_term::Line| line.as_str().starts_with('\u{2500}');
    let bottom = lines.iter().rposition(is_rule)?;
    let top = lines[..bottom].iter().rposition(is_rule)?;
    let muted = lines[bottom].visible_cells().next()?.attrs().foreground();
    let (first, rest) = lines[top + 1..bottom].split_first()?;
    if !first.as_str().starts_with('\u{276f}') {
        return None;
    }
    let text = |line: &wakterm_term::Line, skip: usize| {
        line.visible_cells()
            .skip(skip)
            .filter(|cell| cell.attrs().foreground() != muted)
            .map(|cell| cell.str().to_string())
            .collect::<String>()
            .trim()
            .to_string()
    };
    Some(
        std::iter::once(text(first, 1))
            .chain(rest.iter().map(|line| text(line, 0)))
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string(),
    )
}

#[cfg(test)]
pub(crate) mod fixture {
    use serde_json::json;
    use std::io::Write;
    use std::path::Path;

    /// Appends records shaped as Muse 1.4.12 writes them to a session log.
    pub(crate) struct Log<'a> {
        pub path: &'a Path,
        pub at: i64,
    }

    impl Log<'_> {
        fn write(&mut self, payload_type: &str, payload: serde_json::Value) {
            self.at += 1_000_000;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.path)
                .unwrap();
            writeln!(
                file,
                "{}",
                json!({"schema_version": 1, "stream": {"kind": "session", "id": "s"},
                    "recorded_at": self.at, "record_type": "event",
                    "payload_type": payload_type, "payload": payload})
            )
            .unwrap();
        }

        pub(crate) fn input(&mut self, intent: &str, text: &str) {
            self.write(
                "runtime.user_intent.accepted",
                json!({"intent_id": intent, "delivery_policy": "session_current",
                    "model_messages": [{"content": [{"kind": "text", "text": text}]}]}),
            );
        }

        pub(crate) fn reply(&mut self, run: &str, text: &str) {
            self.write(
                "runtime.session",
                json!({"kind": "run", "run_id": run,
                    "event": {"kind": "assistant_message_committed", "text": text}}),
            );
        }

        pub(crate) fn end(&mut self, run: &str, terminal: &str) {
            self.write(
                "runtime.session",
                json!({"kind": "run", "run_id": run,
                    "event": {"kind": "terminal", "terminal": terminal, "reason": null}}),
            );
        }

        /// Input queued for `run` as `item`, then handed to `target`.
        pub(crate) fn steer(&mut self, run: &str, item: &str) {
            self.write(
                "runtime.session",
                json!({"kind": "run", "run_id": run, "source_run_record_id": item,
                    "event": {"kind": "inbox_item_queued", "disposition": "steer",
                        "source": {"source": "user_steer"}}}),
            );
        }

        pub(crate) fn drain(&mut self, target: &str, item: &str) {
            self.write(
                "runtime.session",
                json!({"kind": "run", "run_id": target,
                    "event": {"kind": "inbox_item_drained", "item_id": item,
                        "drain_target_run_stream": {"kind": "run", "id": target}}}),
            );
        }

        /// A request to run `ls` that waits on the user, or its decision.
        pub(crate) fn approval(&mut self, run: &str, action: &str, decided: bool) {
            let event = if decided {
                json!({"kind": "decision_applied", "pending_action_id": action})
            } else {
                json!({"kind": "requested", "pending_action_id": action,
                    "presentation_phase": "human_pending", "tool_name": "bash",
                    "approval_subject": {"kind": "shell_command", "raw_command": "ls",
                        "stages": [{"argv": ["ls"]}]},
                    "available_choices": [
                        {"choice_id": "allow_once", "label": "Allow once"},
                        {"choice_id": "allow_local_prefix", "label": "Always allow"},
                        {"choice_id": "abort", "label": "Reject"}]})
            };
            self.write(
                "runtime.session",
                json!({"kind": "approval", "run_id": run, "event": event}),
            );
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn input_box_is_the_text_between_the_two_lowest_rules() {
        use wakterm_term::{CellAttributes, Line};
        let normal = CellAttributes::default();
        let mut muted = CellAttributes::default();
        muted.set_foreground(wakterm_term::color::AnsiColor::Grey);
        // A line of text segments, each in the normal or the muted color.
        let line = |parts: &[(&str, bool)]| {
            let mut cells = Vec::new();
            for (text, is_muted) in parts {
                let attrs = if *is_muted { &muted } else { &normal };
                cells.extend(
                    Line::from_text(text, attrs, 0, None)
                        .visible_cells()
                        .map(|cell| cell.as_cell()),
                );
            }
            Line::from_cells(cells, 0)
        };
        let rule = "\u{2500}".repeat(20);
        let screen = |rows: &[&[(&str, bool)]]| {
            let mut screen = vec![
                line(&[("\u{25c6} echo: first", false)]),
                line(&[("\u{2514} queued", false)]),
                line(&[(&rule, true)]),
            ];
            screen.extend(rows.iter().map(|row| line(row)));
            screen.push(line(&[(&rule, true)]));
            screen.push(line(&[("  echo \u{b7} /code/x \u{b7} Auto-review", false)]));
            screen.push(line(&[("", false)]));
            screen
        };
        assert_eq!(
            input_box(&screen(&[&[("\u{276f} ", false)]])).as_deref(),
            Some("")
        );
        let tip = "Type @ to search and insert workspace file paths";
        assert_eq!(
            input_box(&screen(&[&[("\u{276f} ", false), (tip, true)]])).as_deref(),
            Some("")
        );
        assert_eq!(
            input_box(&screen(&[
                &[("\u{276f} third", false)],
                &[("  steer then esc", false)]
            ]))
            .as_deref(),
            Some("third\nsteer then esc")
        );
        assert_eq!(
            input_box(&screen(&[
                &[("Would you like to run the following command?", false)],
                &[("\u{27e9} 1. Allow this stage once (y)", false)]
            ])),
            None
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_live_session_is_the_one_whose_lock_the_process_holds() {
        let temp = tempfile::TempDir::new().unwrap();
        let session = |id: &str| {
            let dir = temp.path().join("sessions/2026/10/26").join(id);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(".session.lock"), "").unwrap();
            dir
        };
        let own = session("01a179b4-cd9b-7ad1-bdaa-202d2932c340");
        let subagent = session(
            "01a179b4-cd9b-7ad1-bdaa-202d2932c340/subagent/01a179b5-0000-7000-8000-000000000000",
        );
        let open = |dir: &Path| std::fs::File::open(dir.join(".session.lock")).unwrap();
        // The process holds a subagent's lock on stdin and its own on stdout.
        let mut process = std::process::Command::new("sleep")
            .arg("30")
            .stdin(open(&subagent))
            .stdout(open(&own))
            .spawn()
            .unwrap();
        let found = live_session_log(process.id(), temp.path());
        process.kill().unwrap();
        process.wait().unwrap();
        assert_eq!(found, Some(own.join("session.jsonl")));
        assert_eq!(live_session_log(std::process::id(), temp.path()), None);
    }

    #[test]
    fn reads_the_latest_turn_with_its_input_reply_end_and_approvals() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("session.jsonl");
        let mut log = fixture::Log {
            path: &path,
            at: 1_792_939_838_000_000,
        };
        log.input("r1", "first");
        log.reply("r1", "one");
        log.end("r1", "completed");
        log.input("r2", "Run the tests");
        log.approval("r2", "a1", false);
        let view = read_turn(&path).unwrap();
        assert_eq!(view.turn_id.as_deref(), Some("r2"));
        assert_eq!(view.input.as_deref(), Some("Run the tests"));
        assert_eq!(view.ended, None);
        assert_eq!(view.pending_approvals, ["a1"]);

        log.approval("r2", "a1", true);
        log.input("steer", "also lint");
        log.reply("r2", "done");
        log.end("r2", "failed");
        let view = read_turn(&path).unwrap();
        assert!(view.pending_approvals.is_empty());
        assert_eq!(view.inputs, 2);
        assert_eq!(view.reply.as_deref(), Some("done"));
        assert_eq!(view.ended, Some(false));

        // Input sent mid-turn that the working run did not take before it
        // ended is answered by a run of its own, which the turn waits for.
        log.input("r3", "first");
        log.input("r4", "second");
        log.steer("r3", "q1");
        log.reply("r3", "one");
        log.end("r3", "completed");
        let view = read_turn(&path).unwrap();
        assert_eq!((view.ended, view.inputs), (None, 2));
        log.drain("r4", "q1");
        log.reply("r4", "two");
        let view = read_turn(&path).unwrap();
        assert_eq!((view.ended, view.reply.as_deref()), (None, Some("two")));
        log.end("r4", "completed");
        let view = read_turn(&path).unwrap();
        assert_eq!(
            (view.turn_id.as_deref(), view.ended),
            (Some("r3"), Some(true))
        );

        // Interrupting hands queued input to the working run, which ends.
        log.input("r5", "third");
        log.input("r6", "steer");
        log.steer("r5", "q2");
        log.drain("r5", "q2");
        log.end("r5", "cancelled");
        assert_eq!(read_turn(&path).unwrap().ended, Some(false));

        assert_eq!(
            resumed_session("muse resume 01a1738a-c72f-7c81-909d-36e0e636494d"),
            Some("01a1738a-c72f-7c81-909d-36e0e636494d".to_string())
        );
        assert_eq!(resumed_session("muse"), None);
        assert_eq!(
            session_id_of(Path::new(
                "/x/sessions/2026/10/25/01a1738a-c72f-7c81-909d-36e0e636494d/session.jsonl"
            )),
            Some("01a1738a-c72f-7c81-909d-36e0e636494d".to_string())
        );
    }
}
