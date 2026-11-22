//! Replays the harness sessions stored on this machine through both of a
//! harness's readers, record by record, and reports every point where the
//! observer and the event projector disagree about whether a turn runs,
//! which turn it is, or the answer it ended with.
//!
//! Run with `WAKTERM_REPLAY=agy cargo test -p mux --lib replay_stored_sessions
//! -- --ignored --nocapture`, with `agy`, `muse`, `gemini`, `opencode` or
//! `zcode`, and
//! `WAKTERM_REPLAY_LIMIT=N` to replay only N sessions. agy's readers judge a
//! background wait by the clock, which the replay sets to each step's time
//! as it is written. Devin has no
//! replay: it rewrites a conversation's rows when it rebuilds the chain, so
//! its database keeps no record of what its readers saw before a rebuild.

use super::*;
use std::io::Write;

/// What the projector's events say about the latest turn.
#[derive(Default)]
struct ProjectedTurn {
    turn: Option<String>,
    open: bool,
    final_text: Option<String>,
}

impl ProjectedTurn {
    fn apply(&mut self, events: &[PendingEvent]) {
        for event in events {
            match event.kind {
                AgentEventKind::TurnStarted => {
                    self.turn = event.turn_id.clone();
                    self.open = true;
                    self.final_text = None;
                }
                AgentEventKind::TurnFinal if event.turn_id == self.turn => {
                    self.open = false;
                    self.final_text = event.text.clone();
                }
                _ => {}
            }
        }
    }
}

/// What the observer says about the latest turn.
struct ObservedTurn {
    /// None when the observer does not know.
    open: Option<bool>,
    turn: Option<String>,
    final_text: Option<String>,
}

#[derive(Default)]
struct Tally {
    sessions: usize,
    points: usize,
    disagreements: usize,
    sessions_with_disagreements: usize,
    examples: Vec<String>,
}

impl Tally {
    /// Compares the two readers after a record, noting a disagreement.
    fn compare(
        &mut self,
        session: &Path,
        index: usize,
        record_kind: &str,
        projected: &ProjectedTurn,
        observed: &ObservedTurn,
        first_in_session: &mut bool,
    ) {
        self.points += 1;
        let mut problems = Vec::new();
        if let Some(open) = observed.open {
            // Before the projector has seen a turn start, it has no view.
            if projected.turn.is_some() && open != projected.open {
                problems.push(format!(
                    "observer {} but projector {}",
                    if open { "running" } else { "ended" },
                    if projected.open { "running" } else { "ended" }
                ));
            }
        }
        if let (Some(observed_turn), Some(projected_turn)) = (&observed.turn, &projected.turn) {
            if observed_turn != projected_turn {
                problems.push(format!("turn {} vs {}", observed_turn, projected_turn));
            }
        }
        if observed.open == Some(false)
            && !projected.open
            && projected.turn.is_some()
            && observed.turn.is_some()
            && observed.final_text != projected.final_text
        {
            problems.push(format!(
                "final {:?} vs {:?}",
                observed
                    .final_text
                    .as_deref()
                    .map(|text| text.chars().take(40).collect::<String>()),
                projected
                    .final_text
                    .as_deref()
                    .map(|text| text.chars().take(40).collect::<String>())
            ));
        }
        if problems.is_empty() {
            return;
        }
        self.disagreements += 1;
        if *first_in_session {
            *first_in_session = false;
            self.sessions_with_disagreements += 1;
            if self.examples.len() < 40 {
                self.examples.push(format!(
                    "{} record {} ({}): {}",
                    session.display(),
                    index,
                    record_kind,
                    problems.join("; ")
                ));
            }
        }
    }

    fn report(&self, harness: &str) {
        eprintln!(
            "{}: {} sessions, {} points compared, {} disagreements in {} sessions",
            harness,
            self.sessions,
            self.points,
            self.disagreements,
            self.sessions_with_disagreements
        );
        for example in &self.examples {
            eprintln!("  {}", example);
        }
    }
}

fn home() -> PathBuf {
    crate::agent::home_dir().unwrap()
}

fn limit() -> usize {
    std::env::var("WAKTERM_REPLAY_LIMIT")
        .ok()
        .and_then(|limit| limit.parse().ok())
        .unwrap_or(usize::MAX)
}

fn append_line(path: &Path, line: &str) {
    let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(file, "{}", line).unwrap();
}

fn json_lines(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|line| serde_json::from_str::<Value>(line).is_ok())
        .map(str::to_string)
        .collect()
}

fn record_kind(line: &str) -> String {
    serde_json::from_str::<Value>(line)
        .ok()
        .and_then(|record| {
            record
                .get("type")
                .or_else(|| record.get("payload_type"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default()
}

fn replay_agy() -> Tally {
    let mut tally = Tally::default();
    let brain = home().join(".gemini/antigravity-cli/brain");
    let mut sessions = fs::read_dir(&brain)
        .unwrap()
        .flatten()
        .filter_map(|entry| {
            let logs = entry.path().join(".system_generated/logs");
            let full = logs.join("transcript_full.jsonl");
            let plain = logs.join("transcript.jsonl");
            vec![full, plain].into_iter().find(|path| path.is_file())
        })
        .collect::<Vec<_>>();
    sessions.sort();
    let temp = tempfile::TempDir::new().unwrap();
    for source in sessions.into_iter().take(limit()) {
        let conversation = source.ancestors().nth(3).unwrap().file_name().unwrap();
        let logs = temp
            .path()
            .join("brain")
            .join(conversation)
            .join(".system_generated/logs");
        fs::create_dir_all(&logs).unwrap();
        let path = logs.join("transcript.jsonl");
        fs::write(&path, "").unwrap();
        let mut cursor = project_agy(&path, None).unwrap().cursor;
        let mut projected = ProjectedTurn::default();
        let mut first = true;
        tally.sessions += 1;
        for (index, line) in json_lines(&source).iter().enumerate() {
            // Read each step when it was written.
            let written = serde_json::from_str::<Value>(line)
                .ok()
                .and_then(|record| crate::harness::agy::record_time(&record));
            if written.is_some() {
                crate::harness::agy::TEST_NOW.with(|now| now.set(written));
            }
            append_line(&path, line);
            let step = project_agy(&path, Some(cursor)).unwrap();
            projected.apply(&step.events);
            cursor = step.cursor;
            let details = crate::harness::agy::read_last_observation(&path).unwrap();
            let observed = ObservedTurn {
                open: match details.turn_state {
                    AgentTurnState::WaitingOnAgent => Some(true),
                    AgentTurnState::WaitingOnUser => Some(false),
                    _ => None,
                },
                turn: details
                    .observed_turn
                    .as_ref()
                    .map(|turn| turn.provider_turn_id.clone()),
                final_text: details.observed_turn.and_then(|turn| turn.final_message),
            };
            tally.compare(
                &source,
                index,
                &record_kind(line),
                &projected,
                &observed,
                &mut first,
            );
        }
    }
    tally
}

fn replay_muse() -> Tally {
    let mut tally = Tally::default();
    let root = home().join(".local/share/muse/sessions");
    let mut sessions = Vec::new();
    let mut dirs = vec![root];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.file_name().is_some_and(|name| name == "session.jsonl") {
                sessions.push(path);
            }
        }
    }
    sessions.sort();
    let temp = tempfile::TempDir::new().unwrap();
    let metadata = super::tests::metadata("muse");
    for source in sessions.into_iter().take(limit()) {
        let session_dir = temp
            .path()
            .join(source.parent().unwrap().file_name().unwrap());
        fs::create_dir_all(&session_dir).unwrap();
        let path = session_dir.join("session.jsonl");
        fs::write(&path, "").unwrap();
        let mut cursor = project_muse(&path, None, &metadata).unwrap().cursor;
        let mut projected = ProjectedTurn::default();
        let mut first = true;
        tally.sessions += 1;
        for (index, line) in json_lines(&source).iter().enumerate() {
            append_line(&path, line);
            let step = project_muse(&path, Some(cursor), &metadata).unwrap();
            projected.apply(&step.events);
            cursor = step.cursor;
            let view = crate::harness::muse::read_turn(&path).unwrap();
            let observed = ObservedTurn {
                open: view.turn_id.as_ref().map(|_| view.ended.is_none()),
                turn: view.turn_id.clone(),
                final_text: view
                    .ended
                    .filter(|completed| *completed)
                    .and(view.reply.clone())
                    .map(|reply| reply.trim().to_string()),
            };
            tally.compare(
                &source,
                index,
                &record_kind(line),
                &projected,
                &observed,
                &mut first,
            );
        }
    }
    tally
}

fn replay_gemini() -> Tally {
    let mut tally = Tally::default();
    let mut sessions = Vec::new();
    for project in fs::read_dir(home().join(".gemini/tmp"))
        .into_iter()
        .flatten()
        .flatten()
    {
        for chat in fs::read_dir(project.path().join("chats"))
            .into_iter()
            .flatten()
            .flatten()
        {
            let path = chat.path();
            if crate::harness::gemini::is_session_file(&path) {
                sessions.push(path);
            }
        }
    }
    sessions.sort();
    let temp = tempfile::TempDir::new().unwrap();
    for (number, source) in sessions.into_iter().take(limit()).enumerate() {
        let jsonl = source
            .extension()
            .is_some_and(|extension| extension == "jsonl");
        let path = temp.path().join(format!(
            "session-{}.{}",
            number,
            if jsonl { "jsonl" } else { "json" }
        ));
        // A prefix of a chat: its first records for a JSONL chat, its first
        // messages for a JSON one.
        let (header, items): (Value, Vec<String>) = if jsonl {
            (Value::Null, json_lines(&source))
        } else {
            let Ok(chat) = serde_json::from_str::<Value>(&fs::read_to_string(&source).unwrap())
            else {
                continue;
            };
            let messages = chat
                .get("messages")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            (chat, messages.iter().map(Value::to_string).collect())
        };
        let write_prefix = |count: usize| {
            if jsonl {
                fs::write(
                    &path,
                    items[..count]
                        .iter()
                        .map(|line| format!("{}\n", line))
                        .collect::<String>(),
                )
                .unwrap();
            } else {
                let mut chat = header.clone();
                chat["messages"] = Value::Array(
                    items[..count]
                        .iter()
                        .map(|message| serde_json::from_str(message).unwrap())
                        .collect(),
                );
                fs::write(&path, chat.to_string()).unwrap();
            }
        };
        // The session ID comes with the chat's first records.
        let seed = items
            .iter()
            .position(|item| item.contains("sessionId"))
            .map_or(0, |at| at + 1);
        write_prefix(if jsonl { seed } else { 0 });
        let Ok(start) = project_gemini(&path, None) else {
            continue;
        };
        let mut cursor = start.cursor;
        let mut projected = ProjectedTurn::default();
        let mut first = true;
        tally.sessions += 1;
        // Gemini records a reply that calls tools as the reply, its metadata
        // and the reply again with its tool calls, in one synchronous step,
        // so the replay does not read the chat partway through that step:
        // where the next record other than metadata updates the last message
        // written.
        let parsed = items
            .iter()
            .map(|item| serde_json::from_str::<Value>(item).unwrap_or(Value::Null))
            .collect::<Vec<_>>();
        fn message(record: &Value) -> Option<&str> {
            record
                .get("$set")
                .is_none()
                .then(|| record.get("id").and_then(Value::as_str))
                .flatten()
        }
        let mid_step = |count: usize| {
            let last = parsed[..count]
                .iter()
                .rev()
                .find(|record| record.get("$set").is_none());
            let next = parsed[count..]
                .iter()
                .find(|record| record.get("$set").is_none());
            jsonl
                && last.and_then(message).is_some()
                && last.and_then(message) == next.and_then(message)
        };
        for count in (if jsonl { seed } else { 0 }) + 1..=items.len() {
            if mid_step(count) {
                continue;
            }
            write_prefix(count);
            let Ok(step) = project_gemini(&path, Some(cursor.clone())) else {
                continue;
            };
            projected.apply(&step.events);
            cursor = step.cursor;
            let Ok(details) = crate::harness::gemini::read_last_observation(&path) else {
                continue;
            };
            // Gemini's observer names no turn and no final answer.
            let observed = ObservedTurn {
                open: match details.turn_state {
                    AgentTurnState::WaitingOnAgent => Some(true),
                    AgentTurnState::WaitingOnUser => Some(false),
                    _ => None,
                },
                turn: None,
                final_text: None,
            };
            tally.compare(
                &source,
                count - 1,
                &record_kind(&items[count - 1]),
                &projected,
                &observed,
                &mut first,
            );
        }
    }
    tally
}

/// The turn state an observation reports, if it knows one.
fn observed_open(turn_state: &AgentTurnState) -> Option<bool> {
    match turn_state {
        AgentTurnState::WaitingOnAgent => Some(true),
        AgentTurnState::WaitingOnUser => Some(false),
        _ => None,
    }
}

/// Replays an OpenCode-schema database, OpenCode's or ZCode's, one message
/// at a time with its parts, into an empty copy of its session, message and
/// part tables.
fn replay_opencode(harness: AgentHarness) -> Tally {
    let mut tally = Tally::default();
    let Some(source) = crate::harness::opencode::schema_db_path(&harness, None) else {
        return tally;
    };
    let temp = tempfile::TempDir::new().unwrap();
    let db_path = temp.path().join("replay.db");
    let db = Connection::open(&db_path).unwrap();
    db.execute(
        "ATTACH DATABASE ?1 AS source",
        params![source.to_string_lossy()],
    )
    .unwrap();
    for table in ["project", "session", "message", "part"] {
        let sql = db
            .query_row(
                "SELECT sql FROM source.sqlite_master WHERE type = 'table' AND name = ?1",
                params![table],
                |row| row.get::<_, String>(0),
            )
            .ok();
        if let Some(sql) = sql {
            db.execute_batch(&sql).unwrap();
        }
    }
    let has_project = db
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name = 'project'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap()
        > 0;
    if has_project {
        db.execute("INSERT INTO project SELECT * FROM source.project", [])
            .unwrap();
    }
    let mut sessions = db
        .prepare("SELECT id FROM source.session ORDER BY rowid")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .flatten()
        .collect::<Vec<_>>();
    sessions.truncate(limit());
    for session_id in sessions {
        db.execute_batch("DELETE FROM part; DELETE FROM message; DELETE FROM session;")
            .unwrap();
        db.execute(
            "INSERT INTO session SELECT * FROM source.session WHERE id = ?1",
            params![session_id],
        )
        .unwrap();
        let messages = db
            .prepare("SELECT id FROM source.message WHERE session_id = ?1 ORDER BY rowid")
            .unwrap()
            .query_map(params![session_id], |row| row.get::<_, String>(0))
            .unwrap()
            .flatten()
            .collect::<Vec<_>>();
        let session_path = crate::harness::opencode::encode_session_path(&db_path, &session_id);
        let Ok(start) = project_opencode(&session_path, None) else {
            continue;
        };
        let mut cursor = start.cursor;
        let mut projected = ProjectedTurn::default();
        let mut first = true;
        tally.sessions += 1;
        let label = PathBuf::from(format!("{}#{}", source.display(), session_id));
        for (index, message_id) in messages.iter().enumerate() {
            db.execute(
                "INSERT INTO message SELECT * FROM source.message WHERE id = ?1",
                params![message_id],
            )
            .unwrap();
            db.execute(
                "INSERT INTO part SELECT * FROM source.part WHERE message_id = ?1 ORDER BY rowid",
                params![message_id],
            )
            .unwrap();
            let Ok(step) = project_opencode(&session_path, Some(cursor.clone())) else {
                continue;
            };
            projected.apply(&step.events);
            cursor = step.cursor;
            let reader =
                Connection::open_with_flags(&db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                    .unwrap();
            let Ok(Some(observation)) = crate::harness::opencode::read_last_observation(
                &reader,
                &db_path,
                &session_id,
                None,
            ) else {
                continue;
            };
            let role = db
                .query_row(
                    "SELECT json_extract(data, '$.role') FROM message WHERE id = ?1",
                    params![message_id],
                    |row| row.get::<_, Option<String>>(0),
                )
                .unwrap()
                .unwrap_or_default();
            // OpenCode's observer names no turn and no final answer.
            let observed = ObservedTurn {
                open: observed_open(&observation.turn_state),
                turn: None,
                final_text: None,
            };
            tally.compare(&label, index, &role, &projected, &observed, &mut first);
        }
    }
    tally
}

#[test]
#[ignore]
fn replay_stored_sessions() {
    let harness = std::env::var("WAKTERM_REPLAY").unwrap_or_else(|_| "agy".to_string());
    let tally = match harness.as_str() {
        "agy" => replay_agy(),
        "muse" => replay_muse(),
        "gemini" => replay_gemini(),
        "opencode" => replay_opencode(AgentHarness::Opencode),
        "zcode" => replay_opencode(AgentHarness::Zcode),
        other => panic!("no replay for {}", other),
    };
    tally.report(&harness);
}
