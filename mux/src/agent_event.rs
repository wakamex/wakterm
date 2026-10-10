use crate::agent::{
    read_gemini_conversation, AgentHarness, AgentMetadata, AgentObservedTurnOutcome,
    AgentRuntimeSnapshot, AgentTurnState,
};
use crate::agent_admission::incarnation_id;
use crate::agent_approval::AgentApprovalRequest;
use anyhow::{anyhow, bail, Context};
use chrono::{DateTime, Utc};
use event_listener::Event;
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{fs, thread};

pub const AGENT_EVENT_SCHEMA: &str = "wakterm.agent-events.v1";
pub const DEFAULT_EVENT_RETENTION: usize = 100_000;
const MAX_PROVIDER_RECORD_BYTES: u64 = 4 * 1024 * 1024;
const CHECKPOINT_BYTES: u64 = 64 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentEventStatus {
    Ok,
    CursorTooOld,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentEventKind {
    AgentLifecycle,
    TurnStarted,
    TurnStateChanged,
    Plan,
    AssistantMessage,
    ObserverFailure,
    TurnFinal,
    ApprovalRequested,
    /// The provider recorded input: the input that started a turn, or input
    /// queued into or steering the running one. `input_sha256` identifies it.
    InputAccepted,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentEvent {
    pub sequence: u64,
    pub event_id: String,
    pub kind: AgentEventKind,
    pub agent_id: String,
    pub incarnation_id: String,
    pub observed_at: DateTime<Utc>,
    pub turn_id: Option<String>,
    pub lifecycle: Option<String>,
    pub reason: Option<String>,
    pub turn_state: Option<String>,
    pub text: Option<String>,
    pub outcome: Option<String>,
    pub recoverable: Option<bool>,
    pub detail: Option<String>,
    #[serde(default)]
    pub approval: Option<AgentApprovalRequest>,
    /// On `turn_started`, the SHA-256 of the input that started the turn,
    /// trimmed, when the provider records it.
    #[serde(default)]
    pub input_sha256: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentEventRecovery {
    pub kind: String,
    pub catalog_as_of_sequence: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentEventPage {
    pub schema: String,
    pub status: AgentEventStatus,
    pub requested_after_sequence: u64,
    pub oldest_available_sequence: u64,
    pub latest_sequence: u64,
    pub next_after_sequence: Option<u64>,
    #[serde(default)]
    pub events: Vec<AgentEvent>,
    pub recovery: Option<AgentEventRecovery>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
struct ProjectionState {
    incarnation_id: String,
    lifecycle: Option<String>,
    #[serde(default)]
    lifecycle_generation: u64,
    observer_error: Option<String>,
    cursor: Option<ProviderCursor>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "provider", rename_all = "snake_case")]
enum ProviderCursor {
    Codex(JsonlCursor),
    Claude(JsonlCursor),
    Agy(JsonlCursor),
    Gemini(GeminiCursor),
    Opencode(OpencodeCursor),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct JsonlCursor {
    source_id: String,
    offset: u64,
    checkpoint_sha256: String,
    current_turn_id: Option<String>,
    last_assistant_text: Option<String>,
    /// Whether the current Claude turn has started without a final reply.
    #[serde(default)]
    turn_open: bool,
    /// A Codex turn whose start waits for its first user input, so that
    /// `turn_started` can carry the input's hash.
    #[serde(default)]
    pending_turn_start: Option<PendingTurnStart>,
    /// Claude's `promptId` for the current turn, which Claude repeats on the
    /// turn's tool results.
    #[serde(default)]
    current_prompt_id: Option<String>,
    /// The turn that was current when a record was skipped. A Claude tool
    /// result with that turn's `promptId` shows the turn continued past the
    /// skipped record and restores it.
    #[serde(default)]
    turn_before_gap: Option<String>,
}

impl JsonlCursor {
    fn suspend_turn_at_gap(&mut self) {
        if let Some(turn_id) = self.current_turn_id.take() {
            self.turn_before_gap = Some(turn_id);
        }
        self.last_assistant_text = None;
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct PendingTurnStart {
    record_key: String,
    turn_id: String,
    observed_at: DateTime<Utc>,
}

impl PendingTurnStart {
    /// The start and state events for this turn, with the input's hash when
    /// the turn has recorded input.
    fn events(self, input_sha256: Option<String>) -> [PendingEvent; 2] {
        let mut started = PendingEvent::new(
            format!("{}:started", self.record_key),
            AgentEventKind::TurnStarted,
            self.observed_at,
        );
        started.turn_id = Some(self.turn_id.clone());
        started.input_sha256 = input_sha256;
        let mut state = PendingEvent::new(
            format!("{}:state", self.record_key),
            AgentEventKind::TurnStateChanged,
            self.observed_at,
        );
        state.turn_id = Some(self.turn_id);
        state.turn_state = Some("waiting_on_agent".to_string());
        [started, state]
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct GeminiCursor {
    source_id: String,
    index: usize,
    last_id: Option<String>,
    current_turn_id: Option<String>,
    #[serde(default)]
    checkpoint_sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct OpencodeCursor {
    source_id: String,
    last_message_rowid: i64,
    last_message_id: Option<String>,
    last_part_rowid: i64,
    last_part_id: Option<String>,
    last_final_message_rowid: i64,
}

#[derive(Clone, Debug)]
struct PendingEvent {
    source_key: String,
    kind: AgentEventKind,
    observed_at: DateTime<Utc>,
    turn_id: Option<String>,
    lifecycle: Option<String>,
    reason: Option<String>,
    turn_state: Option<String>,
    text: Option<String>,
    outcome: Option<String>,
    recoverable: Option<bool>,
    detail: Option<String>,
    approval: Option<AgentApprovalRequest>,
    input_sha256: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentEventRuntimeUpdate {
    pub turn_id: String,
    pub turn_state: AgentTurnState,
    pub observed_at: DateTime<Utc>,
    pub outcome: AgentObservedTurnOutcome,
    pub final_message: Option<String>,
}

impl PendingEvent {
    fn new(
        source_key: impl Into<String>,
        kind: AgentEventKind,
        observed_at: DateTime<Utc>,
    ) -> Self {
        Self {
            source_key: source_key.into(),
            kind,
            observed_at,
            turn_id: None,
            lifecycle: None,
            reason: None,
            turn_state: None,
            text: None,
            outcome: None,
            recoverable: None,
            detail: None,
            approval: None,
            input_sha256: None,
        }
    }
}

#[derive(Clone)]
pub struct AgentEventStore {
    path: PathBuf,
    retention_limit: usize,
    latest_sequence: Arc<AtomicU64>,
    live: Arc<AtomicBool>,
    changed: Arc<Event>,
    reader: AgentEventReader,
}

pub(crate) struct AgentEventWriter {
    store: AgentEventStore,
    conn: Connection,
}

#[derive(Clone)]
struct AgentEventReader {
    tx: Sender<AgentEventReaderCommand>,
}

struct AgentEventReaderCommand {
    after_sequence: u64,
    limit: usize,
    completion: promise::Promise<AgentEventPage>,
}

impl AgentEventReader {
    fn spawn(path: PathBuf, latest_sequence: Arc<AtomicU64>, live: Arc<AtomicBool>) -> Self {
        let (tx, rx) = mpsc::channel::<AgentEventReaderCommand>();
        thread::spawn(move || {
            let mut conn = None;
            while let Ok(mut command) = rx.recv() {
                let result = (|| {
                    if conn.is_none() {
                        conn =
                            Some(connect_reader(&path).with_context(|| {
                                format!("opening agent event reader {:?}", path)
                            })?);
                    }
                    read_page_from_connection(
                        conn.as_ref().expect("reader connection was initialized"),
                        &latest_sequence,
                        command.after_sequence,
                        command.limit,
                    )
                })();
                live.store(result.is_ok(), Ordering::Release);
                if result.is_err() {
                    conn = None;
                }
                command.completion.result(result);
            }
        });
        Self { tx }
    }

    fn read_page(&self, after_sequence: u64, limit: usize) -> promise::Future<AgentEventPage> {
        let mut completion = promise::Promise::new();
        let future = completion
            .get_future()
            .expect("new agent event reader promise has a future");
        let command = AgentEventReaderCommand {
            after_sequence,
            limit,
            completion,
        };
        if let Err(err) = self.tx.send(command) {
            let mut command = err.0;
            command
                .completion
                .err(anyhow!("agent event reader stopped"));
        }
        future
    }
}

impl AgentEventStore {
    pub fn new(path: PathBuf) -> Self {
        Self::with_retention_limit(path, DEFAULT_EVENT_RETENTION)
    }

    pub fn with_retention_limit(path: PathBuf, retention_limit: usize) -> Self {
        let latest_sequence = Arc::new(AtomicU64::new(0));
        let live = Arc::new(AtomicBool::new(false));
        Self {
            reader: AgentEventReader::spawn(
                path.clone(),
                Arc::clone(&latest_sequence),
                Arc::clone(&live),
            ),
            path,
            retention_limit: retention_limit.max(1),
            latest_sequence,
            live,
            changed: Arc::new(Event::new()),
        }
    }

    pub fn start_runtime_epoch(&self) -> anyhow::Result<()> {
        let result = self.start_runtime_epoch_inner();
        match result {
            Ok(latest) => {
                self.publish_latest_sequence(latest);
                self.live.store(true, Ordering::Release);
                Ok(())
            }
            Err(err) => {
                self.live.store(false, Ordering::Release);
                Err(err)
            }
        }
    }

    fn start_runtime_epoch_inner(&self) -> anyhow::Result<u64> {
        let mut conn = self.connect()?;
        let previous_latest = latest_sequence(&conn)?;
        let tx = conn.transaction()?;
        let projections = {
            let mut stmt =
                tx.prepare("SELECT agent_id, record_json FROM agent_event_projection_v1")?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        for (agent_id, json) in projections {
            let mut state: ProjectionState = serde_json::from_str(&json)?;
            if state.lifecycle.as_deref() != Some("available") || state.incarnation_id.is_empty() {
                continue;
            }
            state.lifecycle_generation += 1;
            let mut event = PendingEvent::new(
                format!(
                    "lifecycle:{}:{}:unavailable:mux-restarted-after-{previous_latest}",
                    state.incarnation_id, state.lifecycle_generation
                ),
                AgentEventKind::AgentLifecycle,
                Utc::now(),
            );
            event.lifecycle = Some("unavailable".to_string());
            event.reason = Some("mux_restarted".to_string());
            insert_event(&tx, &agent_id, &state.incarnation_id, event)?;
            state.lifecycle = Some("unavailable".to_string());
            save_projection(&tx, &agent_id, &state)?;
        }
        apply_retention(&tx, self.retention_limit)?;
        tx.commit()?;
        latest_sequence(&conn)
    }

    pub fn is_live(&self) -> bool {
        self.live.load(Ordering::Acquire)
    }

    pub fn latest_sequence(&self) -> u64 {
        self.latest_sequence.load(Ordering::Acquire)
    }

    fn publish_latest_sequence(&self, latest: u64) {
        self.latest_sequence.store(latest, Ordering::Release);
        // Notify after every writer transaction. A concurrent reader may have
        // already published the same latest sequence, but passive waiters must
        // still be woken to reread the durable store.
        self.changed.notify(usize::MAX);
    }

    fn connect(&self) -> anyhow::Result<Connection> {
        connect_event_store(&self.path)
    }

    pub(crate) fn writer(&self) -> anyhow::Result<AgentEventWriter> {
        Ok(AgentEventWriter {
            store: self.clone(),
            conn: self.connect()?,
        })
    }

    pub fn observe_agent(
        &self,
        metadata: &AgentMetadata,
        runtime: &AgentRuntimeSnapshot,
    ) -> anyhow::Result<()> {
        let result = (|| {
            let mut writer = self.writer()?;
            writer.observe_agent(metadata, runtime).map(|_| ())
        })();
        self.live.store(result.is_ok(), Ordering::Release);
        result
    }

    fn observe_agent_inner(
        &self,
        conn: &mut Connection,
        metadata: &AgentMetadata,
        runtime: &AgentRuntimeSnapshot,
    ) -> anyhow::Result<Option<AgentEventRuntimeUpdate>> {
        let Some(current_incarnation) = incarnation_id(metadata) else {
            return Ok(None);
        };
        let loaded_state = load_projection(&conn, &metadata.agent_id)?.unwrap_or_default();
        let mut state = loaded_state.clone();
        let mut pending = Vec::<(String, PendingEvent)>::new();

        if !state.incarnation_id.is_empty() && state.incarnation_id != current_incarnation {
            state.lifecycle_generation += 1;
            let mut event = PendingEvent::new(
                format!(
                    "lifecycle:{}:{}:unavailable:replaced",
                    state.incarnation_id, state.lifecycle_generation
                ),
                AgentEventKind::AgentLifecycle,
                runtime.observed_at,
            );
            event.lifecycle = Some("unavailable".to_string());
            event.reason = Some("incarnation_replaced".to_string());
            pending.push((state.incarnation_id.clone(), event));
            // A resumed process continues the same provider session, so keep
            // reading where the previous incarnation stopped. Input written
            // before this observation would otherwise fall behind the new
            // tail baseline. The projector rebaselines a different session.
            state = ProjectionState {
                cursor: state.cursor.take(),
                ..ProjectionState::default()
            };
        }
        state.incarnation_id = current_incarnation.clone();

        let lifecycle = if runtime.alive {
            "available"
        } else {
            "unavailable"
        };
        if state.lifecycle.as_deref() != Some(lifecycle) {
            state.lifecycle_generation += 1;
            let mut event = PendingEvent::new(
                format!(
                    "lifecycle:{current_incarnation}:{}:{lifecycle}",
                    state.lifecycle_generation
                ),
                AgentEventKind::AgentLifecycle,
                runtime.observed_at,
            );
            event.lifecycle = Some(lifecycle.to_string());
            if !runtime.alive {
                event.reason = Some("process_exited".to_string());
            }
            pending.push((current_incarnation.clone(), event));
            state.lifecycle = Some(lifecycle.to_string());
        }

        let mut runtime_update = None;
        if runtime.alive {
            if let (Some(session_path), false) = (
                runtime.session_path.as_deref(),
                matches!(runtime.harness, AgentHarness::Unknown),
            ) {
                match project_provider_events(
                    &runtime.harness,
                    session_path,
                    state.cursor.clone(),
                    metadata,
                ) {
                    Ok(mut projected) => {
                        if let Some(closed) = close_unanswered_claude_turn(
                            &mut projected.cursor,
                            metadata,
                            runtime,
                            Path::new(session_path),
                        ) {
                            projected.events.extend(closed);
                        }
                        runtime_update = provider_runtime_update(&projected.events);
                        pending.extend(
                            projected
                                .events
                                .into_iter()
                                .map(|event| (current_incarnation.clone(), event)),
                        );
                        state.cursor = Some(projected.cursor);
                        state.observer_error = None;
                    }
                    Err(err) => {
                        let detail = format!("provider event observation failed: {err:#}");
                        if state.observer_error.as_deref() != Some(detail.as_str()) {
                            pending.push((
                                current_incarnation.clone(),
                                observer_failure(
                                    format!("provider-error:{}", digest(detail.as_bytes())),
                                    runtime.observed_at,
                                    None,
                                    &detail,
                                ),
                            ));
                        }
                        state.observer_error = Some(detail);
                    }
                }
            }
        }

        if let Some(detail) = runtime.observer_error.as_deref() {
            if state.observer_error.as_deref() != Some(detail) {
                pending.push((
                    current_incarnation.clone(),
                    observer_failure(
                        format!("runtime-error:{}", digest(detail.as_bytes())),
                        runtime.observed_at,
                        runtime
                            .observed_turn
                            .as_ref()
                            .map(|turn| turn.provider_turn_id.clone()),
                        detail,
                    ),
                ));
            }
            state.observer_error = Some(detail.to_string());
        } else if state
            .observer_error
            .as_deref()
            .is_some_and(|error| !error.starts_with("provider event observation failed:"))
        {
            state.observer_error = None;
        }

        if state == loaded_state && pending.is_empty() {
            return Ok(runtime_update);
        }

        let tx = conn.transaction()?;
        anyhow::ensure!(
            load_projection(&tx, &metadata.agent_id)?.unwrap_or_default() == loaded_state,
            "agent event projection changed while provider observation was being parsed"
        );
        for (incarnation, event) in pending {
            insert_event(&tx, &metadata.agent_id, &incarnation, event)?;
        }
        save_projection(&tx, &metadata.agent_id, &state)?;
        apply_retention(&tx, self.retention_limit)?;
        tx.commit()?;
        let latest = latest_sequence(&conn)?;
        self.publish_latest_sequence(latest);
        Ok(runtime_update)
    }

    pub fn record_unavailable(
        &self,
        metadata: &AgentMetadata,
        observed_at: DateTime<Utc>,
        reason: &str,
    ) -> anyhow::Result<()> {
        let result = (|| {
            let mut writer = self.writer()?;
            writer.record_unavailable(metadata, observed_at, reason)
        })();
        self.live.store(result.is_ok(), Ordering::Release);
        result
    }

    fn record_unavailable_inner(
        &self,
        conn: &mut Connection,
        metadata: &AgentMetadata,
        observed_at: DateTime<Utc>,
        reason: &str,
    ) -> anyhow::Result<()> {
        let Some(incarnation) = incarnation_id(metadata) else {
            return Ok(());
        };
        let tx = conn.transaction()?;
        let mut state = load_projection(&tx, &metadata.agent_id)?.unwrap_or_default();
        if state.incarnation_id == incarnation && state.lifecycle.as_deref() != Some("unavailable")
        {
            state.lifecycle_generation += 1;
            let mut event = PendingEvent::new(
                format!(
                    "lifecycle:{incarnation}:{}:unavailable:{reason}",
                    state.lifecycle_generation
                ),
                AgentEventKind::AgentLifecycle,
                observed_at,
            );
            event.lifecycle = Some("unavailable".to_string());
            event.reason = Some(reason.to_string());
            insert_event(&tx, &metadata.agent_id, &incarnation, event)?;
            state.lifecycle = Some("unavailable".to_string());
            save_projection(&tx, &metadata.agent_id, &state)?;
            apply_retention(&tx, self.retention_limit)?;
        }
        tx.commit()?;
        let latest = latest_sequence(&conn)?;
        self.publish_latest_sequence(latest);
        Ok(())
    }

    pub fn read_page(&self, after_sequence: u64, limit: usize) -> anyhow::Result<AgentEventPage> {
        promise::spawn::block_on(self.read_page_async(after_sequence, limit))
    }

    pub(crate) fn find_approval(
        &self,
        request_id: &str,
    ) -> anyhow::Result<Option<AgentApprovalRequest>> {
        let connection = self.connect()?;
        let record = connection
            .query_row(
                "SELECT record_json FROM agent_event_v1
                 WHERE json_extract(record_json, '$.approval.request_id') = ?1
                 ORDER BY sequence DESC LIMIT 1",
                [request_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let Some(record) = record else {
            return Ok(None);
        };
        let event: AgentEvent = serde_json::from_str(&record)?;
        Ok(event.approval)
    }

    pub fn read_page_async(
        &self,
        after_sequence: u64,
        limit: usize,
    ) -> promise::Future<AgentEventPage> {
        self.reader.read_page(after_sequence, limit)
    }

    pub async fn read_page_wait_async(
        &self,
        after_sequence: u64,
        limit: usize,
        wait: Duration,
    ) -> anyhow::Result<AgentEventPage> {
        let deadline = Instant::now() + wait;
        loop {
            // Register before reading SQLite so a commit between the read and
            // the await cannot be missed.
            let changed = self.changed.listen();
            let page = self.read_page_async(after_sequence, limit).await?;
            if wait.is_zero()
                || page.status != AgentEventStatus::Ok
                || !page.events.is_empty()
                || after_sequence < page.latest_sequence
            {
                return Ok(page);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(page);
            }
            let timed_out = smol::future::or(
                async {
                    changed.await;
                    false
                },
                async {
                    smol::Timer::after(remaining).await;
                    true
                },
            )
            .await;
            if timed_out {
                return Ok(page);
            }
        }
    }
}

impl AgentEventStore {
    /// Waits up to `wait` for a turn of exactly this agent incarnation to start
    /// after `after_sequence`.
    /// Waits up to `wait` after `after_sequence` for the agent incarnation to
    /// record input. With `input_sha256`, only input with that hash counts,
    /// from `input_accepted` or `turn_started`; without it, any turn start
    /// counts.
    pub async fn wait_for_input(
        &self,
        agent_id: &str,
        incarnation_id: &str,
        input_sha256: Option<&str>,
        mut after_sequence: u64,
        wait: Duration,
    ) -> anyhow::Result<InputWait> {
        let deadline = Instant::now() + wait;
        let mut other_input = false;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let page = self
                .read_page_wait_async(after_sequence, 256, remaining)
                .await?;
            if page.status != AgentEventStatus::Ok {
                return Ok(InputWait::Unobservable);
            }
            for event in &page.events {
                if event.agent_id != agent_id
                    || event.incarnation_id != incarnation_id
                    || !matches!(
                        event.kind,
                        AgentEventKind::TurnStarted | AgentEventKind::InputAccepted
                    )
                {
                    continue;
                }
                match input_sha256 {
                    None if event.kind == AgentEventKind::TurnStarted => {
                        return Ok(InputWait::Recorded)
                    }
                    None => {}
                    Some(sha256) if event.input_sha256.as_deref() == Some(sha256) => {
                        return Ok(InputWait::Recorded)
                    }
                    Some(_) => other_input = true,
                }
            }
            after_sequence = page.next_after_sequence.unwrap_or(page.latest_sequence);
            if remaining.is_zero() {
                return Ok(InputWait::Pending {
                    after_sequence,
                    other_input,
                });
            }
        }
    }
}

/// The outcome of waiting for an agent to record input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputWait {
    Recorded,
    /// Not recorded yet. `other_input` says whether the agent recorded
    /// different input meanwhile, and the wait can continue after
    /// `after_sequence`.
    Pending {
        after_sequence: u64,
        other_input: bool,
    },
    /// The event stream cannot confirm input.
    Unobservable,
}

fn connect_reader(path: &Path) -> anyhow::Result<Connection> {
    let conn = connect_event_store(path)?;
    conn.pragma_update(None, "query_only", true)?;
    Ok(conn)
}

fn connect_event_store(path: &Path) -> anyhow::Result<Connection> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(2))?;
    // SQLite refuses some lock upgrades at once rather than waiting out the
    // busy timeout, such as switching a new database to WAL while another
    // connection does the same, so the setup is retried while it is busy.
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match setup_event_store(&conn) {
            Err(err)
                if Instant::now() < deadline
                    && matches!(
                        err.sqlite_error_code(),
                        Some(
                            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                        )
                    ) =>
            {
                thread::sleep(Duration::from_millis(20));
            }
            result => {
                result?;
                return Ok(conn);
            }
        }
    }
}

fn setup_event_store(conn: &Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS agent_event_v1 (
             sequence INTEGER PRIMARY KEY AUTOINCREMENT,
             event_key TEXT NOT NULL UNIQUE,
             record_json TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS agent_event_projection_v1 (
             agent_id TEXT PRIMARY KEY,
             record_json TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS agent_event_meta_v1 (
             key TEXT PRIMARY KEY,
             value INTEGER NOT NULL
         );",
    )
}

fn read_page_from_connection(
    conn: &Connection,
    latest_sequence_cache: &AtomicU64,
    after_sequence: u64,
    limit: usize,
) -> anyhow::Result<AgentEventPage> {
    let tx = conn.unchecked_transaction()?;
    let page = read_page_from_snapshot(&tx, latest_sequence_cache, after_sequence, limit)?;
    tx.commit()?;
    Ok(page)
}

fn read_page_from_snapshot(
    conn: &Connection,
    latest_sequence_cache: &AtomicU64,
    after_sequence: u64,
    limit: usize,
) -> anyhow::Result<AgentEventPage> {
    let latest = latest_sequence(conn)?;
    let pruned_through = meta_value(conn, "pruned_through_sequence")?.unwrap_or(0);
    let oldest = conn
        .query_row("SELECT MIN(sequence) FROM agent_event_v1", [], |row| {
            row.get::<_, Option<u64>>(0)
        })?
        .unwrap_or_else(|| latest.saturating_add(1));
    latest_sequence_cache.store(latest, Ordering::Release);
    if after_sequence < pruned_through {
        return Ok(AgentEventPage {
            schema: AGENT_EVENT_SCHEMA.to_string(),
            status: AgentEventStatus::CursorTooOld,
            requested_after_sequence: after_sequence,
            oldest_available_sequence: oldest,
            latest_sequence: latest,
            next_after_sequence: None,
            events: Vec::new(),
            recovery: Some(AgentEventRecovery {
                kind: "catalog_snapshot".to_string(),
                catalog_as_of_sequence: latest,
            }),
        });
    }
    let mut stmt = conn.prepare(
        "SELECT record_json FROM agent_event_v1
         WHERE sequence > ?1 ORDER BY sequence LIMIT ?2",
    )?;
    let events = stmt
        .query_map(
            params![after_sequence, limit.clamp(1, 1000) as u64],
            |row| row.get::<_, String>(0),
        )?
        .map(|json| Ok(serde_json::from_str::<AgentEvent>(&json?)?))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let next = events
        .last()
        .map(|event| event.sequence)
        .unwrap_or(after_sequence);
    Ok(AgentEventPage {
        schema: AGENT_EVENT_SCHEMA.to_string(),
        status: AgentEventStatus::Ok,
        requested_after_sequence: after_sequence,
        oldest_available_sequence: oldest,
        latest_sequence: latest,
        next_after_sequence: Some(next),
        events,
        recovery: None,
    })
}

impl AgentEventWriter {
    pub(crate) fn observe_agent(
        &mut self,
        metadata: &AgentMetadata,
        runtime: &AgentRuntimeSnapshot,
    ) -> anyhow::Result<Option<AgentEventRuntimeUpdate>> {
        let result = self
            .store
            .observe_agent_inner(&mut self.conn, metadata, runtime);
        self.store.live.store(result.is_ok(), Ordering::Release);
        result
    }

    pub(crate) fn record_unavailable(
        &mut self,
        metadata: &AgentMetadata,
        observed_at: DateTime<Utc>,
        reason: &str,
    ) -> anyhow::Result<()> {
        let result =
            self.store
                .record_unavailable_inner(&mut self.conn, metadata, observed_at, reason);
        self.store.live.store(result.is_ok(), Ordering::Release);
        result
    }

    pub(crate) fn observe_codex_app_server_notification(
        &mut self,
        metadata: &AgentMetadata,
        runtime: &AgentRuntimeSnapshot,
        message: &Value,
    ) -> anyhow::Result<()> {
        let result = (|| {
            self.store
                .observe_agent_inner(&mut self.conn, metadata, runtime)?;
            self.store.record_codex_app_server_notification_inner(
                &mut self.conn,
                metadata,
                runtime.observed_at,
                message,
            )
        })();
        self.store.live.store(result.is_ok(), Ordering::Release);
        result
    }
}

fn provider_runtime_update(events: &[PendingEvent]) -> Option<AgentEventRuntimeUpdate> {
    let mut update = None;
    for event in events {
        let Some(turn_id) = event.turn_id.as_deref() else {
            continue;
        };
        match event.kind {
            AgentEventKind::TurnStarted => {
                update = Some(AgentEventRuntimeUpdate {
                    turn_id: turn_id.to_string(),
                    turn_state: AgentTurnState::WaitingOnAgent,
                    observed_at: event.observed_at,
                    outcome: AgentObservedTurnOutcome::Running,
                    final_message: None,
                });
            }
            AgentEventKind::TurnFinal => {
                update = Some(AgentEventRuntimeUpdate {
                    turn_id: turn_id.to_string(),
                    turn_state: AgentTurnState::WaitingOnUser,
                    observed_at: event.observed_at,
                    outcome: if event.outcome.as_deref() == Some("completed") {
                        AgentObservedTurnOutcome::Completed
                    } else {
                        AgentObservedTurnOutcome::Aborted
                    },
                    final_message: event.text.clone(),
                });
            }
            _ => {}
        }
    }
    update
}

impl AgentEventStore {
    fn record_codex_app_server_notification_inner(
        &self,
        conn: &mut Connection,
        metadata: &AgentMetadata,
        observed_at: DateTime<Utc>,
        message: &Value,
    ) -> anyhow::Result<()> {
        let Some(incarnation) = incarnation_id(metadata) else {
            return Ok(());
        };
        let Some(session) = metadata.codex_app_server.as_ref() else {
            return Ok(());
        };
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            return Ok(());
        };
        let params = message.get("params").unwrap_or(&Value::Null);
        if params.get("threadId").and_then(Value::as_str) != Some(session.thread_id.as_str()) {
            return Ok(());
        }

        let mut pending = Vec::new();
        match method {
            "turn/started" => {
                let Some(turn_id) = params.pointer("/turn/id").and_then(Value::as_str) else {
                    return Ok(());
                };
                let timestamp = params
                    .pointer("/turn/startedAt")
                    .and_then(Value::as_i64)
                    .and_then(|seconds| DateTime::from_timestamp(seconds, 0))
                    .unwrap_or(observed_at);
                let key = format!("codex-app-server:{}:{turn_id}", session.thread_id);
                let mut started = PendingEvent::new(
                    format!("{key}:started"),
                    AgentEventKind::TurnStarted,
                    timestamp,
                );
                started.turn_id = Some(turn_id.to_string());
                pending.push(started);
                let mut state = PendingEvent::new(
                    format!("{key}:started-state"),
                    AgentEventKind::TurnStateChanged,
                    timestamp,
                );
                state.turn_id = Some(turn_id.to_string());
                state.turn_state = Some("waiting_on_agent".to_string());
                pending.push(state);
            }
            "item/started" => {
                let item = params.get("item").unwrap_or(&Value::Null);
                if item.get("type").and_then(Value::as_str) != Some("userMessage") {
                    return Ok(());
                }
                let (Some(item_id), Some(content)) = (
                    item.get("id").and_then(Value::as_str),
                    item.get("content").and_then(Value::as_array),
                ) else {
                    return Ok(());
                };
                let text = content
                    .iter()
                    .filter(|input| input.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|input| input.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n");
                if text.trim().is_empty() {
                    return Ok(());
                }
                pending.push(input_accepted(
                    format!("codex-app-server:{}:{item_id}:input", session.thread_id),
                    observed_at,
                    params
                        .get("turnId")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    crate::agent::message_sha256(&text),
                    false,
                ));
            }
            "item/completed" => {
                let Some(turn_id) = params.get("turnId").and_then(Value::as_str) else {
                    return Ok(());
                };
                let item = params.get("item").unwrap_or(&Value::Null);
                if item.get("type").and_then(Value::as_str) != Some("agentMessage") {
                    return Ok(());
                }
                let Some(item_id) = item.get("id").and_then(Value::as_str) else {
                    return Ok(());
                };
                let Some(text) = item
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                else {
                    return Ok(());
                };
                let timestamp = params
                    .get("completedAtMs")
                    .and_then(Value::as_i64)
                    .and_then(DateTime::from_timestamp_millis)
                    .unwrap_or(observed_at);
                let mut event = PendingEvent::new(
                    format!(
                        "codex-app-server:{}:{turn_id}:item:{item_id}:message",
                        session.thread_id
                    ),
                    AgentEventKind::AssistantMessage,
                    timestamp,
                );
                event.turn_id = Some(turn_id.to_string());
                event.text = Some(text.to_string());
                pending.push(event);
            }
            "turn/completed" => {
                let Some(turn_id) = params.pointer("/turn/id").and_then(Value::as_str) else {
                    return Ok(());
                };
                let status = params
                    .pointer("/turn/status")
                    .and_then(Value::as_str)
                    .unwrap_or("failed");
                let timestamp = params
                    .pointer("/turn/completedAt")
                    .and_then(Value::as_i64)
                    .and_then(|seconds| DateTime::from_timestamp(seconds, 0))
                    .unwrap_or(observed_at);
                let text = params
                    .pointer("/turn/items")
                    .and_then(Value::as_array)
                    .and_then(|items| {
                        items.iter().rev().find(|item| {
                            item.get("type").and_then(Value::as_str) == Some("agentMessage")
                        })
                    })
                    .and_then(|item| item.get("text"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .map(str::to_string);
                let key = format!("codex-app-server:{}:{turn_id}", session.thread_id);
                let mut final_event =
                    PendingEvent::new(format!("{key}:final"), AgentEventKind::TurnFinal, timestamp);
                final_event.turn_id = Some(turn_id.to_string());
                final_event.outcome = Some(
                    if status == "completed" {
                        "completed"
                    } else {
                        "aborted"
                    }
                    .to_string(),
                );
                final_event.text = text;
                if status == "failed" {
                    let (reason, detail) = codex_terminal_failure(params.pointer("/turn/error"));
                    final_event.reason = Some(reason.to_string());
                    final_event.detail = Some(detail.to_string());
                }
                pending.push(final_event);
                let mut state = PendingEvent::new(
                    format!("{key}:completed-state"),
                    AgentEventKind::TurnStateChanged,
                    timestamp,
                );
                state.turn_id = Some(turn_id.to_string());
                state.turn_state = Some("waiting_on_user".to_string());
                pending.push(state);
            }
            "item/commandExecution/requestApproval" | "item/tool/requestUserInput" => {
                let Some(approval) = crate::agent_approval::approval_from_event(message, metadata)
                else {
                    return Ok(());
                };
                let mut event = PendingEvent::new(
                    format!(
                        "codex-app-server:{}:approval:{}",
                        session.thread_id, approval.request_id
                    ),
                    AgentEventKind::ApprovalRequested,
                    approval.observed_at,
                );
                event.turn_id = Some(approval.turn_id.clone());
                event.approval = Some(approval);
                pending.push(event);
            }
            _ => return Ok(()),
        }

        let tx = conn.transaction()?;
        for event in pending {
            insert_event(&tx, &metadata.agent_id, &incarnation, event)?;
        }
        apply_retention(&tx, self.retention_limit)?;
        tx.commit()?;
        self.publish_latest_sequence(latest_sequence(conn)?);
        Ok(())
    }
}

fn latest_sequence(conn: &Connection) -> anyhow::Result<u64> {
    Ok(conn.query_row(
        "SELECT COALESCE(MAX(sequence), 0) FROM agent_event_v1",
        [],
        |row| row.get(0),
    )?)
}

fn meta_value(conn: &Connection, key: &str) -> anyhow::Result<Option<u64>> {
    Ok(conn
        .query_row(
            "SELECT value FROM agent_event_meta_v1 WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()?)
}

fn load_projection(conn: &Connection, agent_id: &str) -> anyhow::Result<Option<ProjectionState>> {
    conn.query_row(
        "SELECT record_json FROM agent_event_projection_v1 WHERE agent_id = ?1",
        params![agent_id],
        |row| row.get::<_, String>(0),
    )
    .optional()?
    .map(|json| serde_json::from_str(&json).map_err(anyhow::Error::from))
    .transpose()
}

fn save_projection(
    tx: &Transaction<'_>,
    agent_id: &str,
    state: &ProjectionState,
) -> anyhow::Result<()> {
    tx.execute(
        "INSERT INTO agent_event_projection_v1(agent_id, record_json) VALUES (?1, ?2)
         ON CONFLICT(agent_id) DO UPDATE SET record_json = excluded.record_json",
        params![agent_id, serde_json::to_string(state)?],
    )?;
    Ok(())
}

fn insert_event(
    tx: &Transaction<'_>,
    agent_id: &str,
    incarnation_id: &str,
    pending: PendingEvent,
) -> anyhow::Result<()> {
    let event_key = format!("{agent_id}\0{incarnation_id}\0{}", pending.source_key);
    let event_id = format!("{:x}", Sha256::digest(event_key.as_bytes()));
    let mut event = AgentEvent {
        sequence: 0,
        event_id,
        kind: pending.kind,
        agent_id: agent_id.to_string(),
        incarnation_id: incarnation_id.to_string(),
        observed_at: pending.observed_at,
        turn_id: pending.turn_id,
        lifecycle: pending.lifecycle,
        reason: pending.reason,
        turn_state: pending.turn_state,
        text: pending.text,
        outcome: pending.outcome,
        recoverable: pending.recoverable,
        detail: pending.detail,
        approval: pending.approval,
        input_sha256: pending.input_sha256,
    };
    let inserted = tx.execute(
        "INSERT OR IGNORE INTO agent_event_v1(event_key, record_json) VALUES (?1, ?2)",
        params![event_key, serde_json::to_string(&event)?],
    )?;
    if inserted == 1 {
        event.sequence = tx.last_insert_rowid() as u64;
        tx.execute(
            "UPDATE agent_event_v1 SET record_json = ?2 WHERE sequence = ?1",
            params![event.sequence, serde_json::to_string(&event)?],
        )?;
    }
    Ok(())
}

fn apply_retention(tx: &Transaction<'_>, retention_limit: usize) -> anyhow::Result<()> {
    let count: usize = tx.query_row("SELECT COUNT(*) FROM agent_event_v1", [], |row| row.get(0))?;
    if count <= retention_limit {
        return Ok(());
    }
    let remove = count - retention_limit;
    let cutoff: u64 = tx.query_row(
        "SELECT sequence FROM agent_event_v1 ORDER BY sequence LIMIT 1 OFFSET ?1",
        params![(remove - 1) as u64],
        |row| row.get(0),
    )?;
    tx.execute(
        "DELETE FROM agent_event_v1 WHERE sequence <= ?1",
        params![cutoff],
    )?;
    tx.execute(
        "INSERT INTO agent_event_meta_v1(key, value)
         VALUES ('pruned_through_sequence', ?1)
         ON CONFLICT(key) DO UPDATE SET value = MAX(value, excluded.value)",
        params![cutoff],
    )?;
    Ok(())
}

fn codex_terminal_failure(error: Option<&Value>) -> (&'static str, &'static str) {
    let info = error.and_then(|error| error.get("codexErrorInfo"));
    // Structured variants with fields serialize as a one-key object. Only
    // classify known codes; never forward raw messages or diagnostic payloads.
    let code = info.and_then(|info| {
        info.as_str().or_else(|| {
            info.as_object()
                .filter(|object| object.len() == 1)
                .and_then(|object| object.keys().next().map(String::as_str))
        })
    });
    match code {
        Some("cyberPolicy" | "misalignmentPolicyViolation") => (
            "policy_blocked",
            "Codex could not complete this turn because the provider blocked the response under its content policy.",
        ),
        Some("contextWindowExceeded") => (
            "context_limit",
            "Codex could not complete this turn because the conversation exceeded the context limit.",
        ),
        Some("sessionBudgetExceeded" | "usageLimitExceeded") => (
            "usage_limit",
            "Codex could not complete this turn because a session budget or usage limit was reached.",
        ),
        Some("rateLimitExceeded") => (
            "rate_limited",
            "Codex could not complete this turn because the provider rate limit was reached.",
        ),
        Some("serverOverloaded") => (
            "model_at_capacity",
            "Selected model is at capacity. Please try a different model.",
        ),
        Some("httpConnectionFailed" | "responseStreamConnectionFailed"
            | "internalServerError" | "responseStreamDisconnected" | "responseTooManyFailedAttempts") => (
            "provider_unavailable",
            "Codex could not complete this turn because the provider service or connection failed.",
        ),
        Some("unauthorized") => (
            "authentication_failed",
            "Codex could not complete this turn because the provider rejected authentication.",
        ),
        Some("badRequest") => (
            "invalid_request",
            "Codex could not complete this turn because the provider rejected the request.",
        ),
        Some("sandboxError") => (
            "sandbox_error",
            "Codex could not complete this turn because its execution sandbox failed.",
        ),
        _ => (
            "provider_error",
            "Codex could not complete this turn because of a provider error.",
        ),
    }
}

/// An `input_accepted` event for input the provider recorded. `queued`
/// marks input waiting in the provider's queue for the running turn.
fn input_accepted(
    source_key: String,
    observed_at: DateTime<Utc>,
    turn_id: Option<String>,
    input_sha256: String,
    queued: bool,
) -> PendingEvent {
    let mut event = PendingEvent::new(source_key, AgentEventKind::InputAccepted, observed_at);
    event.turn_id = turn_id;
    event.input_sha256 = Some(input_sha256);
    event.reason = queued.then(|| "queued".to_string());
    event
}

fn observer_failure(
    source_key: String,
    observed_at: DateTime<Utc>,
    turn_id: Option<String>,
    detail: &str,
) -> PendingEvent {
    let mut event = PendingEvent::new(source_key, AgentEventKind::ObserverFailure, observed_at);
    event.turn_id = turn_id;
    event.recoverable = Some(true);
    event.detail = Some(detail.to_string());
    event
}

struct ProjectedEvents {
    cursor: ProviderCursor,
    events: Vec<PendingEvent>,
}

fn project_provider_events(
    harness: &AgentHarness,
    session_path: &str,
    cursor: Option<ProviderCursor>,
    metadata: &AgentMetadata,
) -> anyhow::Result<ProjectedEvents> {
    match harness {
        AgentHarness::Agy => project_agy(Path::new(session_path), cursor),
        AgentHarness::Codex => project_codex(Path::new(session_path), cursor),
        AgentHarness::Claude => project_claude(Path::new(session_path), cursor, metadata),
        AgentHarness::Gemini => project_gemini(Path::new(session_path), cursor),
        AgentHarness::Opencode | AgentHarness::Zcode => project_opencode(session_path, cursor),
        AgentHarness::Unknown => bail!("unknown harness has no provider event projection"),
    }
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn checkpoint_sha256(path: &Path, offset: u64) -> anyhow::Result<String> {
    let mut file = fs::File::open(path)?;
    let start = offset.saturating_sub(CHECKPOINT_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = vec![0; (offset - start) as usize];
    file.read_exact(&mut bytes)?;
    Ok(digest(&bytes))
}

fn jsonl_source_id(path: &Path, provider: &str) -> anyhow::Result<String> {
    let reader = BufReader::new(fs::File::open(path)?);
    for line in reader.lines().take(32) {
        let record: Value = match serde_json::from_str(&line?) {
            Ok(record) => record,
            Err(_) => continue,
        };
        let id = match provider {
            "codex" => record
                .get("payload")
                .and_then(|payload| payload.get("id").or_else(|| payload.get("session_id")))
                .and_then(Value::as_str),
            "claude" => record.get("sessionId").and_then(Value::as_str),
            _ => None,
        };
        if let Some(id) = id.filter(|id| !id.is_empty()) {
            return Ok(id.to_string());
        }
    }
    bail!("{provider} session lacks an exact provider session id")
}

/// Closes an open Claude turn that Claude reports idle without a reply,
/// such as input it displayed without working on it.
fn close_unanswered_claude_turn(
    cursor: &mut ProviderCursor,
    metadata: &AgentMetadata,
    runtime: &AgentRuntimeSnapshot,
    path: &Path,
) -> Option<Vec<PendingEvent>> {
    let ProviderCursor::Claude(cursor) = cursor else {
        return None;
    };
    if !cursor.turn_open || runtime.turn_phase.as_deref() != Some("idle") {
        return None;
    }
    if !crate::agent::claude_idle_settled(path, crate::agent::claude_status_changed_at(metadata)) {
        return None;
    }
    let turn_id = cursor.current_turn_id.clone()?;
    cursor.turn_open = false;
    let key = format!("claude:{}:{turn_id}:no-reply", cursor.source_id);
    let mut final_event = PendingEvent::new(
        format!("{key}:final"),
        AgentEventKind::TurnFinal,
        runtime.observed_at,
    );
    final_event.turn_id = Some(turn_id.clone());
    final_event.outcome = Some("aborted".to_string());
    final_event.reason = Some("no_reply".to_string());
    final_event.detail = Some("Claude went idle without replying.".to_string());
    let mut state = PendingEvent::new(
        format!("{key}:state"),
        AgentEventKind::TurnStateChanged,
        runtime.observed_at,
    );
    state.turn_id = Some(turn_id);
    state.turn_state = Some("waiting_on_user".to_string());
    Some(vec![final_event, state])
}

fn initial_jsonl_cursor(
    path: &Path,
    provider: &str,
    source_id: String,
) -> anyhow::Result<JsonlCursor> {
    let offset = complete_jsonl_tail(path)?;
    let (current_turn_id, current_prompt_id) = latest_jsonl_turn(path, provider, &source_id)?;
    Ok(JsonlCursor {
        source_id,
        offset,
        checkpoint_sha256: checkpoint_sha256(path, offset)?,
        current_turn_id,
        last_assistant_text: None,
        turn_open: false,
        pending_turn_start: None,
        current_prompt_id,
        turn_before_gap: None,
    })
}

fn complete_jsonl_tail(path: &Path) -> anyhow::Result<u64> {
    let mut file = fs::File::open(path)?;
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(0);
    }
    file.seek(SeekFrom::End(-1))?;
    let mut byte = [0];
    file.read_exact(&mut byte)?;
    if byte[0] == b'\n' {
        Ok(len)
    } else {
        let mut reader = BufReader::new(fs::File::open(path)?);
        let mut offset = 0;
        let mut line = Vec::new();
        loop {
            line.clear();
            let read = reader.read_until(b'\n', &mut line)?;
            if read == 0 || !line.ends_with(b"\n") {
                return Ok(offset);
            }
            offset += read as u64;
        }
    }
}

/// The latest turn ID in a provider session, with Claude's `promptId` for it.
fn latest_jsonl_turn(
    path: &Path,
    provider: &str,
    source_id: &str,
) -> anyhow::Result<(Option<String>, Option<String>)> {
    let reader = BufReader::new(fs::File::open(path)?);
    let mut current = None;
    let mut prompt_id = None;
    for line in reader.lines() {
        let record: Value = match serde_json::from_str(&line?) {
            Ok(record) => record,
            Err(_) => continue,
        };
        match provider {
            "codex" => {
                if let Some(turn_id) = codex_turn_id(&record) {
                    current = Some(turn_id.to_string());
                }
            }
            "claude" if claude_human_user(&record) => {
                current = record
                    .get("uuid")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                prompt_id = record
                    .get("promptId")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            "agy" if record.get("type").and_then(Value::as_str) == Some("USER_INPUT") => {
                current = agy_turn_id(source_id, &record);
            }
            _ => {}
        }
    }
    Ok((current, prompt_id))
}

fn read_new_jsonl(
    path: &Path,
    cursor: &mut JsonlCursor,
    mut visit: impl FnMut(u64, &Value, &mut JsonlCursor) -> anyhow::Result<Vec<PendingEvent>>,
) -> anyhow::Result<Vec<PendingEvent>> {
    let len = fs::metadata(path)?.len();
    if cursor.offset > len || checkpoint_sha256(path, cursor.offset)? != cursor.checkpoint_sha256 {
        bail!("provider session was truncated or rewritten at the durable cursor")
    }
    let mut reader = BufReader::new(fs::File::open(path)?);
    reader.seek(SeekFrom::Start(cursor.offset))?;
    let mut events = Vec::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        let start = cursor.offset;
        let read = reader
            .by_ref()
            .take(MAX_PROVIDER_RECORD_BYTES + 1)
            .read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        if read as u64 > MAX_PROVIDER_RECORD_BYTES {
            let mut consumed = read as u64;
            let mut complete = line.ends_with(b"\n");
            // Drain the rest without accumulating an unbounded provider record.
            // An incomplete tail remains at `start` until its newline arrives.
            while !complete {
                line.clear();
                let read = reader
                    .by_ref()
                    .take(64 * 1024)
                    .read_until(b'\n', &mut line)?;
                if read == 0 {
                    break;
                }
                consumed += read as u64;
                complete = line.ends_with(b"\n");
            }
            if !complete {
                break;
            }
            cursor.offset += consumed;
            // The skipped record might contain a new turn or an assistant
            // message. Require fresh identity instead of reusing stale context.
            cursor.suspend_turn_at_gap();
            events.push(observer_failure(
                format!("{}:{start}:oversized-record", cursor.source_id),
                Utc::now(),
                None,
                &format!("provider record at byte {start} exceeds the {MAX_PROVIDER_RECORD_BYTES}-byte bound; skipped {consumed} bytes and resumed at the next record; turn attribution was cleared"),
            ));
            continue;
        }
        if !line.ends_with(b"\n") {
            break;
        }
        cursor.offset += read as u64;
        let record: Value = match serde_json::from_slice(&line) {
            Ok(record) => record,
            Err(err) => {
                // A complete line never becomes valid later, for example when
                // a write cut short by a full disk ran into the next record.
                // Skip it like an oversized record rather than retrying it on
                // every observation.
                cursor.suspend_turn_at_gap();
                events.push(observer_failure(
                    format!("{}:{start}:invalid-record", cursor.source_id),
                    Utc::now(),
                    None,
                    &format!("provider record at byte {start} is not valid JSON ({err}); skipped {read} bytes and resumed at the next record; turn attribution was cleared"),
                ));
                continue;
            }
        };
        events.extend(visit(start, &record, cursor)?);
    }
    cursor.checkpoint_sha256 = checkpoint_sha256(path, cursor.offset)?;
    Ok(events)
}

fn event_time(record: &Value) -> DateTime<Utc> {
    record
        .get("timestamp")
        .or_else(|| record.get("created_at"))
        .and_then(Value::as_str)
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
        .unwrap_or_else(Utc::now)
}

fn codex_turn_id(record: &Value) -> Option<&str> {
    let payload = record.get("payload")?;
    payload
        .get("turn_id")
        .or_else(|| {
            payload
                .get("internal_chat_message_metadata_passthrough")
                .and_then(|metadata| metadata.get("turn_id"))
        })
        .and_then(Value::as_str)
}

fn codex_message_text(payload: &Value) -> Option<String> {
    let parts = payload
        .get("content")?
        .as_array()?
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("output_text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    (!parts.is_empty()).then(|| parts.join("\n"))
}

fn project_codex(path: &Path, cursor: Option<ProviderCursor>) -> anyhow::Result<ProjectedEvents> {
    let source_id = jsonl_source_id(path, "codex")?;
    let mut cursor = match cursor {
        Some(ProviderCursor::Codex(cursor)) if cursor.source_id == source_id => cursor,
        Some(_) => {
            return reset_jsonl_projection(path, "codex", source_id, true);
        }
        None => {
            return Ok(ProjectedEvents {
                cursor: ProviderCursor::Codex(initial_jsonl_cursor(path, "codex", source_id)?),
                events: Vec::new(),
            })
        }
    };
    let events = match read_new_jsonl(path, &mut cursor, |offset, record, cursor| {
        let mut events = Vec::new();
        let timestamp = event_time(record);
        let record_key = record
            .get("ordinal")
            .and_then(Value::as_u64)
            .map(|ordinal| format!("codex:{}:{ordinal}", cursor.source_id))
            .unwrap_or_else(|| {
                format!(
                    "codex:{}:{offset}:{}",
                    cursor.source_id,
                    digest(record.to_string().as_bytes())
                )
            });
        let turn_id = codex_turn_id(record).map(str::to_string);
        if let Some(turn_id) = turn_id.as_ref() {
            cursor.current_turn_id = Some(turn_id.clone());
        }
        match record.get("type").and_then(Value::as_str) {
            Some("event_msg") => {
                let payload = record.get("payload").unwrap_or(&Value::Null);
                match payload.get("type").and_then(Value::as_str) {
                    Some("task_started") => {
                        let Some(turn_id) = turn_id else {
                            return Ok(vec![observer_failure(
                                format!("{record_key}:missing-turn"),
                                timestamp,
                                None,
                                "Codex task_started lacks an exact provider turn id",
                            )]);
                        };
                        cursor.last_assistant_text = None;
                        // A previous turn that never recorded input starts now.
                        if let Some(pending) = cursor.pending_turn_start.take() {
                            events.extend(pending.events(None));
                        }
                        cursor.pending_turn_start = Some(PendingTurnStart {
                            record_key: record_key.clone(),
                            turn_id,
                            observed_at: timestamp,
                        });
                    }
                    Some("task_complete") | Some("turn_aborted") => {
                        let Some(turn_id) = turn_id.or_else(|| cursor.current_turn_id.clone())
                        else {
                            return Ok(vec![observer_failure(
                                format!("{record_key}:missing-turn"),
                                timestamp,
                                None,
                                "Codex terminal event lacks an exact provider turn id",
                            )]);
                        };
                        if let Some(pending) = cursor
                            .pending_turn_start
                            .take_if(|pending| pending.turn_id == turn_id)
                        {
                            events.extend(pending.events(None));
                        }
                        let completed =
                            payload.get("type").and_then(Value::as_str) == Some("task_complete");
                        let text = payload
                            .get("last_agent_message")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|text| !text.is_empty())
                            .map(str::to_string)
                            .or_else(|| cursor.last_assistant_text.clone());
                        let mut final_event = PendingEvent::new(
                            format!("{record_key}:final"),
                            AgentEventKind::TurnFinal,
                            timestamp,
                        );
                        final_event.turn_id = Some(turn_id.clone());
                        final_event.outcome =
                            Some(if completed { "completed" } else { "aborted" }.to_string());
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
                    _ => {}
                }
            }
            Some("response_item") => {
                let payload = record.get("payload").unwrap_or(&Value::Null);
                if payload.get("type").and_then(Value::as_str) == Some("message")
                    && payload.get("role").and_then(Value::as_str) == Some("user")
                    && !crate::agent::codex_response_message_is_synthetic_context(payload)
                {
                    if let Some(message) = crate::agent::codex_response_message_text(payload) {
                        let sha256 = crate::agent::message_sha256(&message);
                        // The turn's first input starts it; later input steers it.
                        let input_turn = match cursor.pending_turn_start.take() {
                            Some(pending) => {
                                let turn_id = pending.turn_id.clone();
                                events.extend(pending.events(Some(sha256.clone())));
                                Some(turn_id)
                            }
                            None => turn_id.clone().or_else(|| cursor.current_turn_id.clone()),
                        };
                        events.push(input_accepted(
                            format!("{record_key}:input"),
                            timestamp,
                            input_turn,
                            sha256,
                            false,
                        ));
                    }
                }
                if payload.get("type").and_then(Value::as_str) == Some("message")
                    && payload.get("role").and_then(Value::as_str) == Some("assistant")
                {
                    if let Some(text) = codex_message_text(payload) {
                        let Some(turn_id) = turn_id.or_else(|| cursor.current_turn_id.clone())
                        else {
                            return Ok(vec![observer_failure(
                                format!("{record_key}:missing-turn"),
                                timestamp,
                                None,
                                "Codex assistant message lacks an exact provider turn id",
                            )]);
                        };
                        if let Some(pending) = cursor
                            .pending_turn_start
                            .take_if(|pending| pending.turn_id == turn_id)
                        {
                            events.extend(pending.events(None));
                        }
                        cursor.last_assistant_text = Some(text.clone());
                        let mut message = PendingEvent::new(
                            format!("{record_key}:message"),
                            AgentEventKind::AssistantMessage,
                            timestamp,
                        );
                        message.turn_id = Some(turn_id);
                        message.text = Some(text);
                        events.push(message);
                    }
                }
            }
            _ => {}
        }
        Ok(events)
    }) {
        Ok(events) => events,
        Err(err) if err.to_string().contains("truncated or rewritten") => {
            return reset_jsonl_projection(path, "codex", source_id, false);
        }
        Err(err) => return Err(err),
    };
    Ok(ProjectedEvents {
        cursor: ProviderCursor::Codex(cursor),
        events,
    })
}

fn claude_human_user(record: &Value) -> bool {
    if record.get("type").and_then(Value::as_str) != Some("user") {
        return false;
    }
    let Some(content) = record
        .get("message")
        .and_then(|message| message.get("content"))
    else {
        return true;
    };
    !content.as_array().is_some_and(|blocks| {
        !blocks.is_empty()
            && blocks
                .iter()
                .all(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
    })
}

fn project_claude(
    path: &Path,
    cursor: Option<ProviderCursor>,
    metadata: &AgentMetadata,
) -> anyhow::Result<ProjectedEvents> {
    let source_id = jsonl_source_id(path, "claude")?;
    let mut cursor = match cursor {
        Some(ProviderCursor::Claude(cursor)) if cursor.source_id == source_id => cursor,
        Some(_) => {
            return reset_jsonl_projection(path, "claude", source_id, true);
        }
        None => {
            return Ok(ProjectedEvents {
                cursor: ProviderCursor::Claude(initial_jsonl_cursor(path, "claude", source_id)?),
                events: Vec::new(),
            })
        }
    };
    let events = match read_new_jsonl(path, &mut cursor, |offset, record, cursor| {
        let timestamp = event_time(record);
        let uuid = record
            .get("uuid")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("offset-{offset}"));
        let record_key = format!("claude:{}:{uuid}", cursor.source_id);
        if claude_human_user(record) {
            let Some(turn_id) = record
                .get("uuid")
                .and_then(Value::as_str)
                .map(str::to_string)
            else {
                return Ok(vec![observer_failure(
                    format!("{record_key}:missing-turn"),
                    timestamp,
                    None,
                    "Claude user message lacks an exact provider turn id",
                )]);
            };
            cursor.current_turn_id = Some(turn_id.clone());
            cursor.current_prompt_id = record
                .get("promptId")
                .and_then(Value::as_str)
                .map(str::to_string);
            cursor.turn_before_gap = None;
            cursor.last_assistant_text = None;
            cursor.turn_open = true;
            let mut started = PendingEvent::new(
                format!("{record_key}:started"),
                AgentEventKind::TurnStarted,
                timestamp,
            );
            started.turn_id = Some(turn_id.clone());
            started.input_sha256 = crate::agent::claude_user_text(record)
                .map(|text| crate::agent::message_sha256(crate::agent::unwrap_claude_paste(&text)));
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
            return Ok(vec![Some(started), accepted, Some(state)]
                .into_iter()
                .flatten()
                .collect());
        }
        // After a skipped record, the first tool result that names a prompt
        // shows whether the earlier turn continued or a new one started.
        if let Some(prompt_id) = record
            .get("promptId")
            .and_then(Value::as_str)
            .filter(|_| record.get("type").and_then(Value::as_str) == Some("user"))
        {
            if let Some(turn_id) = cursor.turn_before_gap.take() {
                if cursor.current_prompt_id.as_deref() == Some(prompt_id) {
                    cursor.current_turn_id = Some(turn_id);
                }
            }
        }
        // Input typed while a turn runs is queued into it, and recorded the
        // moment Claude shows it queued. Background task notifications are
        // queued the same way but are not input.
        if record.get("type").and_then(Value::as_str) == Some("queue-operation")
            && record.get("operation").and_then(Value::as_str) == Some("enqueue")
        {
            let Some(content) = record
                .get("content")
                .and_then(Value::as_str)
                .filter(|content| !content.starts_with("<task-notification>"))
            else {
                return Ok(Vec::new());
            };
            return Ok(vec![input_accepted(
                format!("{record_key}:input"),
                timestamp,
                cursor.current_turn_id.clone().filter(|_| cursor.turn_open),
                crate::agent::message_sha256(crate::agent::unwrap_claude_paste(content)),
                true,
            )]);
        }
        if record.get("type").and_then(Value::as_str) == Some("system")
            && record.get("subtype").and_then(Value::as_str) == Some("api_error")
        {
            let detail = record
                .pointer("/error/error/error/message")
                .and_then(Value::as_str)
                .or_else(|| {
                    record
                        .pointer("/error/error/message")
                        .and_then(Value::as_str)
                })
                .or_else(|| record.get("error").and_then(Value::as_str))
                .unwrap_or("Claude provider API error");
            return Ok(vec![observer_failure(
                format!("{record_key}:api-error"),
                timestamp,
                cursor.current_turn_id.clone(),
                detail,
            )]);
        }
        if record.get("type").and_then(Value::as_str) != Some("assistant") {
            return Ok(Vec::new());
        }
        if record
            .get("message")
            .and_then(|message| message.get("model"))
            .and_then(Value::as_str)
            == Some("<synthetic>")
        {
            let detail = record
                .get("message")
                .and_then(|message| extract_text(message.get("content")))
                .or_else(|| {
                    record
                        .get("error")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_else(|| "Claude emitted a synthetic provider failure".to_string());
            return Ok(vec![observer_failure(
                format!("{record_key}:synthetic-failure"),
                timestamp,
                cursor.current_turn_id.clone(),
                &detail,
            )]);
        }
        let Some(turn_id) = cursor.current_turn_id.clone() else {
            return Ok(vec![observer_failure(
                format!("{record_key}:missing-turn"),
                timestamp,
                None,
                "Claude assistant message lacks an exact provider turn id",
            )]);
        };
        let mut events = Vec::new();
        let content = record
            .get("message")
            .and_then(|message| message.get("content"))
            .and_then(Value::as_array);
        let mut text_parts = Vec::new();
        if let Some(content) = content {
            for (index, block) in content.iter().enumerate() {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(text) = block
                            .get("text")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|text| !text.is_empty())
                        {
                            text_parts.push(text.to_string());
                        }
                    }
                    Some("tool_use")
                        if block.get("name").and_then(Value::as_str) == Some("ExitPlanMode") =>
                    {
                        if let Some(plan) = block
                            .get("input")
                            .and_then(|input| input.get("plan"))
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|text| !text.is_empty())
                        {
                            let mut event = PendingEvent::new(
                                format!("{record_key}:plan:{index}"),
                                AgentEventKind::Plan,
                                timestamp,
                            );
                            event.turn_id = Some(turn_id.clone());
                            event.text = Some(plan.to_string());
                            events.push(event);
                        }
                    }
                    Some("tool_use")
                        if block.get("name").and_then(Value::as_str) == Some("AskUserQuestion") =>
                    {
                        if let Some(approval) = crate::agent_approval::claude_question_from_block(
                            block, metadata, &turn_id, timestamp,
                        ) {
                            let mut event = PendingEvent::new(
                                format!("{record_key}:question:{index}"),
                                AgentEventKind::ApprovalRequested,
                                timestamp,
                            );
                            event.turn_id = Some(turn_id.clone());
                            event.approval = Some(approval);
                            events.push(event);
                        }
                    }
                    _ => {}
                }
            }
        }
        let text = if text_parts.is_empty() {
            None
        } else {
            let text = text_parts.join("\n");
            cursor.last_assistant_text = Some(text.clone());
            let mut message = PendingEvent::new(
                format!("{record_key}:message"),
                AgentEventKind::AssistantMessage,
                timestamp,
            );
            message.turn_id = Some(turn_id.clone());
            message.text = Some(text.clone());
            events.push(message);
            Some(text)
        };
        if text.is_some()
            && record
                .get("message")
                .and_then(|message| message.get("stop_reason"))
                .and_then(Value::as_str)
                == Some("end_turn")
        {
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
            cursor.turn_open = false;
        }
        Ok(events)
    }) {
        Ok(events) => events,
        Err(err) if err.to_string().contains("truncated or rewritten") => {
            return reset_jsonl_projection(path, "claude", source_id, false);
        }
        Err(err) => return Err(err),
    };
    Ok(ProjectedEvents {
        cursor: ProviderCursor::Claude(cursor),
        events,
    })
}

/// The agy turn a user input record starts, named by its conversation and
/// step so that each input is its own turn.
fn agy_turn_id(conversation_id: &str, record: &Value) -> Option<String> {
    let step = record.get("step_index")?.as_u64()?;
    Some(format!("{conversation_id}:{step}"))
}

/// The text the user sent, which agy records between USER_REQUEST tags
/// ahead of metadata it appends.
fn agy_user_request(content: &str) -> Option<&str> {
    let start = content.find("<USER_REQUEST>")? + "<USER_REQUEST>".len();
    let end = content[start..].find("</USER_REQUEST>")? + start;
    Some(content[start..end].trim())
}

/// Projects agy's step log. The observer binds the trimmed transcript,
/// whose long fields can be truncated, so events come from its full
/// sibling when agy writes one.
fn project_agy(path: &Path, cursor: Option<ProviderCursor>) -> anyhow::Result<ProjectedEvents> {
    let full = path.with_file_name("transcript_full.jsonl");
    let path = if full.is_file() { full.as_path() } else { path };
    // The transcript lives at brain/<conversation>/.system_generated/logs.
    let source_id = path
        .ancestors()
        .nth(3)
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .context("agy transcript is outside a conversation directory")?
        .to_string();
    let mut cursor = match cursor {
        Some(ProviderCursor::Agy(cursor)) if cursor.source_id == source_id => cursor,
        Some(_) => return reset_jsonl_projection(path, "agy", source_id, true),
        None => {
            return Ok(ProjectedEvents {
                cursor: ProviderCursor::Agy(initial_jsonl_cursor(path, "agy", source_id)?),
                events: Vec::new(),
            })
        }
    };
    let events = match read_new_jsonl(path, &mut cursor, |offset, record, cursor| {
        let timestamp = event_time(record);
        let step = record
            .get("step_index")
            .and_then(Value::as_u64)
            .map(|step| step.to_string())
            .unwrap_or_else(|| format!("offset-{offset}"));
        let record_key = format!("agy:{}:{step}", cursor.source_id);
        match record.get("type").and_then(Value::as_str) {
            Some("USER_INPUT") => {
                let Some(turn_id) = agy_turn_id(&cursor.source_id, record) else {
                    return Ok(vec![observer_failure(
                        format!("{record_key}:missing-turn"),
                        timestamp,
                        None,
                        "agy user input lacks a step index",
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
                started.input_sha256 = record
                    .get("content")
                    .and_then(Value::as_str)
                    .and_then(agy_user_request)
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
            Some("PLANNER_RESPONSE")
                if record.get("status").and_then(Value::as_str) == Some("DONE") =>
            {
                let Some(turn_id) = cursor.current_turn_id.clone() else {
                    return Ok(Vec::new());
                };
                let Some(text) = record
                    .get("content")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .map(str::to_string)
                else {
                    return Ok(Vec::new());
                };
                cursor.last_assistant_text = Some(text.clone());
                let mut message = PendingEvent::new(
                    format!("{record_key}:message"),
                    AgentEventKind::AssistantMessage,
                    timestamp,
                );
                message.turn_id = Some(turn_id.clone());
                message.text = Some(text.clone());
                let mut events = vec![message];
                // A response that calls no tools ends the turn. Its tool
                // calls are recorded as text, empty when there are none.
                let calls_tools = record.get("tool_calls").is_some_and(|calls| match calls {
                    Value::Array(calls) => !calls.is_empty(),
                    Value::String(calls) => !matches!(calls.trim(), "" | "[]"),
                    Value::Null => false,
                    _ => true,
                });
                if !calls_tools && cursor.turn_open {
                    let mut final_event = PendingEvent::new(
                        format!("{record_key}:final"),
                        AgentEventKind::TurnFinal,
                        timestamp,
                    );
                    final_event.turn_id = Some(turn_id.clone());
                    final_event.outcome = Some("completed".to_string());
                    final_event.text = Some(text);
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
    }) {
        Ok(events) => events,
        Err(err) if err.to_string().contains("truncated or rewritten") => {
            return reset_jsonl_projection(path, "agy", source_id, false);
        }
        Err(err) => return Err(err),
    };
    Ok(ProjectedEvents {
        cursor: ProviderCursor::Agy(cursor),
        events,
    })
}

fn project_gemini(path: &Path, cursor: Option<ProviderCursor>) -> anyhow::Result<ProjectedEvents> {
    let conversation = read_gemini_conversation(path)?;
    let source_id = conversation
        .session_id
        .context("Gemini session lacks an exact provider session id")?
        .to_string();
    let messages = &conversation.messages;
    let mut cursor = match cursor {
        Some(ProviderCursor::Gemini(cursor)) if cursor.source_id == source_id => cursor,
        Some(_) => {
            return reset_gemini_projection(&source_id, messages, true);
        }
        None => {
            return Ok(ProjectedEvents {
                cursor: ProviderCursor::Gemini(GeminiCursor {
                    source_id,
                    index: messages.len(),
                    last_id: messages
                        .last()
                        .and_then(|message| message.get("id"))
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    current_turn_id: latest_gemini_turn(messages),
                    checkpoint_sha256: gemini_checkpoint_sha256(messages, messages.len())?,
                }),
                events: Vec::new(),
            })
        }
    };
    let checkpoint_matches = cursor.checkpoint_sha256.is_empty()
        || gemini_checkpoint_sha256(messages, cursor.index)
            .is_ok_and(|checkpoint| checkpoint == cursor.checkpoint_sha256);
    if cursor.index > messages.len()
        || (cursor.index > 0
            && messages[cursor.index - 1].get("id").and_then(Value::as_str)
                != cursor.last_id.as_deref())
        || !checkpoint_matches
    {
        return reset_gemini_projection(&source_id, messages, false);
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
            Some("user") => {
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
                let has_tool_calls = message
                    .get("toolCalls")
                    .and_then(Value::as_array)
                    .is_some_and(|calls| !calls.is_empty());
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
    cursor.checkpoint_sha256 = gemini_checkpoint_sha256(messages, cursor.index)?;
    Ok(ProjectedEvents {
        cursor: ProviderCursor::Gemini(cursor),
        events,
    })
}

fn latest_gemini_turn(messages: &[Value]) -> Option<String> {
    messages
        .iter()
        .rev()
        .find(|message| message.get("type").and_then(Value::as_str) == Some("user"))
        .and_then(|message| message.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn gemini_checkpoint_sha256(messages: &[Value], index: usize) -> anyhow::Result<String> {
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

fn reset_jsonl_projection(
    path: &Path,
    provider: &str,
    source_id: String,
    session_changed: bool,
) -> anyhow::Result<ProjectedEvents> {
    let detail = if session_changed {
        format!("confirmed {provider} provider session changed; a new tail baseline was armed")
    } else {
        format!("{provider} provider session was rewritten; a new tail baseline was armed")
    };
    let cursor = initial_jsonl_cursor(path, provider, source_id)?;
    let event = observer_failure(
        format!(
            "{provider}:{}:baseline-reset:{}:{}",
            cursor.source_id, cursor.offset, cursor.checkpoint_sha256
        ),
        Utc::now(),
        None,
        &detail,
    );
    Ok(ProjectedEvents {
        cursor: match provider {
            "codex" => ProviderCursor::Codex(cursor),
            "claude" => ProviderCursor::Claude(cursor),
            "agy" => ProviderCursor::Agy(cursor),
            _ => unreachable!(),
        },
        events: vec![event],
    })
}

fn reset_gemini_projection(
    source_id: &str,
    messages: &[Value],
    session_changed: bool,
) -> anyhow::Result<ProjectedEvents> {
    let detail = if session_changed {
        "confirmed Gemini provider session changed; a new tail baseline was armed"
    } else {
        "Gemini provider session was rewritten; a new tail baseline was armed"
    };
    let checkpoint_sha256 = gemini_checkpoint_sha256(messages, messages.len())?;
    Ok(ProjectedEvents {
        cursor: ProviderCursor::Gemini(GeminiCursor {
            source_id: source_id.to_string(),
            index: messages.len(),
            last_id: messages
                .last()
                .and_then(|message| message.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string),
            current_turn_id: latest_gemini_turn(messages),
            checkpoint_sha256: checkpoint_sha256.clone(),
        }),
        events: vec![observer_failure(
            format!("gemini:{source_id}:baseline-reset:{checkpoint_sha256}"),
            Utc::now(),
            None,
            detail,
        )],
    })
}

fn extract_text(content: Option<&Value>) -> Option<String> {
    let content = content?;
    if let Some(text) = content
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        return Some(text.to_string());
    }
    let parts = content
        .as_array()?
        .iter()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>();
    (!parts.is_empty()).then(|| parts.join("\n"))
}

fn parse_opencode_session_path(value: &str) -> anyhow::Result<(PathBuf, String)> {
    let url = url::Url::parse(value)?;
    anyhow::ensure!(
        url.scheme() == "opencode",
        "invalid OpenCode observer session URL"
    );
    let values = url
        .query_pairs()
        .collect::<std::collections::HashMap<_, _>>();
    let db = values.get("db").context("OpenCode session URL lacks db")?;
    let id = values.get("id").context("OpenCode session URL lacks id")?;
    Ok((PathBuf::from(db.as_ref()), id.to_string()))
}

/// The text a user typed or pasted into an OpenCode message: its text
/// parts that OpenCode did not add itself, in order.
fn opencode_user_text(conn: &Connection, message_id: &str) -> anyhow::Result<Option<String>> {
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

fn opencode_has_later_message(
    conn: &Connection,
    session_id: &str,
    rowid: i64,
) -> anyhow::Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM message WHERE session_id = ?1 AND rowid > ?2 LIMIT 1",
            params![session_id, rowid],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn project_opencode(
    session_path: &str,
    cursor: Option<ProviderCursor>,
) -> anyhow::Result<ProjectedEvents> {
    let (db_path, source_id) = parse_opencode_session_path(session_path)?;
    let conn = Connection::open_with_flags(&db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(std::time::Duration::from_secs(2))?;
    let maxima = opencode_maxima(&conn, &source_id)?;
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
        Some(ProviderCursor::Opencode(cursor)) if cursor.source_id == source_id => cursor,
        Some(_) => {
            let detail =
                "confirmed OpenCode provider session changed; a new tail baseline was armed";
            return Ok(ProjectedEvents {
                cursor: ProviderCursor::Opencode(OpencodeCursor {
                    source_id: source_id.clone(),
                    last_message_rowid: message_max.0,
                    last_message_id: message_max.1.clone(),
                    last_part_rowid: part_max.0,
                    last_part_id: part_max.1.clone(),
                    last_final_message_rowid: *final_message_max,
                }),
                events: vec![observer_failure(
                    format!("opencode:{source_id}:baseline-reset:{baseline_identity}"),
                    Utc::now(),
                    None,
                    detail,
                )],
            });
        }
        None => {
            return Ok(ProjectedEvents {
                cursor: ProviderCursor::Opencode(OpencodeCursor {
                    source_id,
                    last_message_rowid: message_max.0,
                    last_message_id: message_max.1.clone(),
                    last_part_rowid: part_max.0,
                    last_part_id: part_max.1.clone(),
                    last_final_message_rowid: *final_message_max,
                }),
                events: Vec::new(),
            })
        }
    };
    if !opencode_checkpoint_matches(&conn, &source_id, &cursor)? {
        let detail = "OpenCode provider database was rebuilt; a new tail baseline was armed";
        return Ok(ProjectedEvents {
            cursor: ProviderCursor::Opencode(OpencodeCursor {
                source_id: source_id.clone(),
                last_message_rowid: message_max.0,
                last_message_id: message_max.1.clone(),
                last_part_rowid: part_max.0,
                last_part_id: part_max.1.clone(),
                last_final_message_rowid: *final_message_max,
            }),
            events: vec![observer_failure(
                format!("opencode:{source_id}:baseline-reset:{baseline_identity}"),
                Utc::now(),
                None,
                detail,
            )],
        });
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
            if message.get("role").and_then(Value::as_str) == Some("user")
                && !opencode_runtime_message(&message)
            {
                // OpenCode writes a user message's text parts after the
                // message. Wait for them so the turn carries its input's
                // hash, unless a later message shows the input has none.
                let text = opencode_user_text(&conn, &message_id)?;
                if text.is_none() && !opencode_has_later_message(&conn, &source_id, rowid)? {
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
                events.push(started);
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
            if message.get("role").and_then(Value::as_str) == Some("assistant")
                && part.get("type").and_then(Value::as_str) == Some("text")
            {
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
            let Some(turn_id) = message.get("parentID").and_then(Value::as_str) else {
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
            if let Some(error) = message.get("error") {
                // A provider failure ends the step without a final answer.
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
                cursor.last_final_message_rowid = rowid;
                continue;
            }
            let mut final_event = PendingEvent::new(
                format!("opencode:{source_id}:{message_id}:final"),
                AgentEventKind::TurnFinal,
                timestamp,
            );
            final_event.turn_id = Some(turn_id.to_string());
            final_event.outcome = Some("completed".to_string());
            final_event.text = text
                .map(|text| text.trim().to_string())
                .filter(|text| !text.is_empty());
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
    Ok(ProjectedEvents {
        cursor: ProviderCursor::Opencode(cursor),
        events,
    })
}

/// A user-role message the harness injected itself, such as ZCode's task
/// notifications and reminders; it continues the current turn.
pub(crate) fn opencode_runtime_message(message: &Value) -> bool {
    message.get("synthetic").and_then(Value::as_bool) == Some(true)
        || message.pointer("/semantics/origin").and_then(Value::as_str) == Some("agent_runtime")
}

fn opencode_maxima(
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

fn opencode_checkpoint_matches(
    conn: &Connection,
    session_id: &str,
    cursor: &OpencodeCursor,
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
mod tests {
    use super::*;

    /// Projects the complete message history of the newest session in a
    /// copy of a real ZCode database, whose schema belongs to ZCode:
    /// WAKTERM_TEST_ZCODE_DB=~/.zcode/cli/db/db.sqlite.
    #[test]
    #[ignore = "requires WAKTERM_TEST_ZCODE_DB pointing to a real ZCode database"]
    fn zcode_real_database_events() {
        let source = std::path::PathBuf::from(std::env::var_os("WAKTERM_TEST_ZCODE_DB").unwrap());
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("db.sqlite");
        Connection::open_with_flags(&source, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap()
            .execute("VACUUM INTO ?1", [db_path.to_string_lossy()])
            .unwrap();
        let session: String = Connection::open(&db_path)
            .unwrap()
            .query_row(
                "SELECT id FROM session ORDER BY time_updated DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let session_path = format!("opencode://session?db={}&id={session}", db_path.display());
        let from_start = ProviderCursor::Opencode(OpencodeCursor {
            source_id: session.clone(),
            last_message_rowid: 0,
            last_message_id: None,
            last_part_rowid: 0,
            last_part_id: None,
            last_final_message_rowid: 0,
        });
        let projected = project_opencode(&session_path, Some(from_start)).unwrap();
        let count = |kind| {
            projected
                .events
                .iter()
                .filter(|event| event.kind == kind)
                .count()
        };
        let prompts: i64 = Connection::open(&db_path)
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM message WHERE session_id = ?1
                   AND json_extract(data, '$.role') = 'user'
                   AND json_extract(data, '$.semantics.origin') = 'real_user'",
                [&session],
                |row| row.get(0),
            )
            .unwrap();
        eprintln!(
            "{session}: {} events, {} turns started for {prompts} real prompts, {} assistant messages, {} finals, {} failures",
            projected.events.len(),
            count(AgentEventKind::TurnStarted),
            count(AgentEventKind::AssistantMessage),
            count(AgentEventKind::TurnFinal),
            count(AgentEventKind::ObserverFailure)
        );
        // Injected notifications and reminders do not start turns.
        assert_eq!(count(AgentEventKind::TurnStarted) as i64, prompts);
    }
    use crate::agent::{AgentStatus, AgentTransport, AgentTurnState};
    use std::fs::OpenOptions;
    use std::io::Write;
    use tempfile::TempDir;

    fn metadata(harness: &str) -> AgentMetadata {
        AgentMetadata {
            agent_id: format!("agent-{harness}"),
            name: harness.to_string(),
            launch_cmd: harness.to_string(),
            declared_cwd: format!("/tmp/{harness}"),
            adopted_pid: Some(42),
            adopted_start_time: Some(99),
            created_at: Utc::now(),
            repo_root: None,
            worktree: None,
            branch: None,
            managed_checkout: false,
            launch_supervisor: None,
            codex_app_server: None,
        }
    }

    fn runtime(
        metadata: &AgentMetadata,
        harness: AgentHarness,
        session_path: String,
    ) -> AgentRuntimeSnapshot {
        let mut runtime = AgentRuntimeSnapshot::new(metadata);
        runtime.harness = harness;
        runtime.transport = AgentTransport::ObservedPty;
        runtime.status = AgentStatus::Idle;
        runtime.turn_state = AgentTurnState::WaitingOnUser;
        runtime.session_path = Some(session_path);
        runtime.alive = true;
        runtime
    }

    fn append(path: &Path, value: &str) {
        let mut file = OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(value.as_bytes()).unwrap();
    }

    fn event_kinds(page: &AgentEventPage) -> Vec<AgentEventKind> {
        page.events.iter().map(|event| event.kind.clone()).collect()
    }

    #[test]
    fn managed_command_approval_is_a_durable_exact_identity_event() {
        let directory = TempDir::new().unwrap();
        let store = AgentEventStore::new(directory.path().join("events.sqlite3"));
        store.start_runtime_epoch().unwrap();
        let mut metadata = metadata("codex");
        metadata.adopted_pid = None;
        metadata.adopted_start_time = None;
        metadata.codex_app_server = Some(crate::agent::CodexAppServerSession {
            thread_id: "thread-approval".into(),
            session_id: "session-approval".into(),
            executable: "codex".into(),
            version: "test".into(),
            tui_args: vec![],
        });
        let mut runtime = AgentRuntimeSnapshot::new(&metadata);
        runtime.harness = AgentHarness::Codex;
        runtime.transport = AgentTransport::CodexAppServerTui;
        runtime.alive = true;
        let message = serde_json::json!({
            "id": 41,
            "method": "item/commandExecution/requestApproval",
            "params": {
                "threadId": "thread-approval",
                "turnId": "turn-approval",
                "itemId": "item-approval",
                "command": "scp result host:/tmp/",
                "availableDecisions": ["accept", "cancel"]
            }
        });
        let mut writer = store.writer().unwrap();
        writer
            .observe_codex_app_server_notification(&metadata, &runtime, &message)
            .unwrap();
        let page = store.read_page(0, 100).unwrap();
        let event = page
            .events
            .iter()
            .find(|event| event.kind == AgentEventKind::ApprovalRequested)
            .unwrap();
        let approval = event.approval.as_ref().unwrap();
        assert_eq!(approval.agent_id, metadata.agent_id);
        assert_eq!(approval.incarnation_id, incarnation_id(&metadata).unwrap());
        assert_eq!(approval.command.as_deref(), Some("scp result host:/tmp/"));
        assert_eq!(approval.choices[0].id, "allow_once");
    }

    #[test]
    fn event_stream_is_not_live_until_runtime_start_succeeds() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("events.sqlite3");
        let store = AgentEventStore::new(path.clone());

        assert!(!store.is_live());
        assert!(!path.exists());

        store.start_runtime_epoch().unwrap();
        assert!(store.is_live());
        assert!(path.exists());
    }

    #[test]
    fn persistent_writer_keeps_wal_open_and_skips_unchanged_projection_writes() {
        let temp = TempDir::new().unwrap();
        let db = temp.path().join("events.sqlite3");
        let wal = PathBuf::from(format!("{}-wal", db.display()));
        let store = AgentEventStore::new(db);
        let metadata = metadata("unknown");
        let runtime = runtime(
            &metadata,
            AgentHarness::Unknown,
            "/tmp/unused-session".to_string(),
        );
        let mut writer = store.writer().unwrap();

        writer.observe_agent(&metadata, &runtime).unwrap();
        let changes_after_first_observation = writer.conn.total_changes();
        assert!(wal.exists());

        writer.observe_agent(&metadata, &runtime).unwrap();
        assert_eq!(writer.conn.total_changes(), changes_after_first_observation);
        assert!(wal.exists());
    }

    #[test]
    fn persistent_reader_observes_later_writer_commits() {
        let temp = TempDir::new().unwrap();
        let store = AgentEventStore::new(temp.path().join("events.sqlite3"));
        store.start_runtime_epoch().unwrap();
        let metadata = metadata("unknown");
        let runtime = runtime(
            &metadata,
            AgentHarness::Unknown,
            "/tmp/unused-session".to_string(),
        );
        let mut writer = store.writer().unwrap();
        writer.observe_agent(&metadata, &runtime).unwrap();
        let first = store.read_page(0, 100).unwrap();
        let after = first.next_after_sequence.unwrap();

        writer
            .record_unavailable(&metadata, Utc::now(), "test_unavailable")
            .unwrap();
        let second = store.read_page(after, 100).unwrap();

        assert_eq!(second.events.len(), 1);
        assert_eq!(second.events[0].lifecycle.as_deref(), Some("unavailable"));
        assert_eq!(second.events[0].reason.as_deref(), Some("test_unavailable"));
    }

    #[test]
    fn event_page_uses_one_snapshot_across_concurrent_retention() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("events.sqlite3");
        let store = AgentEventStore::with_retention_limit(path.clone(), 1);
        store.start_runtime_epoch().unwrap();
        let metadata = metadata("unknown");
        let runtime = runtime(
            &metadata,
            AgentHarness::Unknown,
            "/tmp/unused-session".to_string(),
        );
        let mut writer = store.writer().unwrap();
        writer.observe_agent(&metadata, &runtime).unwrap();

        let reader = connect_reader(&path).unwrap();
        let tx = reader.unchecked_transaction().unwrap();
        let snapshot_latest = latest_sequence(&tx).unwrap();
        writer
            .record_unavailable(&metadata, Utc::now(), "test_unavailable")
            .unwrap();

        let cached_latest = AtomicU64::new(0);
        let page = read_page_from_snapshot(&tx, &cached_latest, 0, 100).unwrap();
        assert_eq!(page.status, AgentEventStatus::Ok);
        assert_eq!(page.latest_sequence, snapshot_latest);
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.events[0].sequence, snapshot_latest);
        tx.commit().unwrap();

        let current = store.read_page(0, 100).unwrap();
        assert_eq!(current.status, AgentEventStatus::CursorTooOld);
        assert!(current.latest_sequence > snapshot_latest);
    }

    #[test]
    fn blocking_reader_wakes_all_consumers_after_commit() {
        let temp = TempDir::new().unwrap();
        let store = AgentEventStore::new(temp.path().join("events.sqlite3"));
        store.start_runtime_epoch().unwrap();
        let metadata = metadata("unknown");
        let runtime = runtime(
            &metadata,
            AgentHarness::Unknown,
            "/tmp/unused-session".to_string(),
        );
        let mut writer = store.writer().unwrap();
        writer.observe_agent(&metadata, &runtime).unwrap();
        let after = store.latest_sequence();
        let ready = Arc::new(std::sync::Barrier::new(5));
        let readers = (0..4)
            .map(|_| {
                let store = store.clone();
                let ready = Arc::clone(&ready);
                thread::spawn(move || {
                    ready.wait();
                    promise::spawn::block_on(store.read_page_wait_async(
                        after,
                        100,
                        Duration::from_secs(1),
                    ))
                    .unwrap()
                })
            })
            .collect::<Vec<_>>();
        ready.wait();
        thread::sleep(Duration::from_millis(20));
        writer
            .record_unavailable(&metadata, Utc::now(), "test_unavailable")
            .unwrap();

        for reader in readers {
            let page = reader.join().unwrap();
            assert_eq!(page.events.len(), 1);
            assert_eq!(page.events[0].reason.as_deref(), Some("test_unavailable"));
        }
    }

    #[test]
    fn codex_records_input_that_starts_or_steers_a_turn() {
        let temp = TempDir::new().unwrap();
        let session = temp.path().join("codex.jsonl");
        fs::write(
            &session,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"session-codex\"}}\n",
        )
        .unwrap();
        let store = AgentEventStore::new(temp.path().join("events.sqlite3"));
        let metadata = metadata("codex");
        let runtime = runtime(
            &metadata,
            AgentHarness::Codex,
            session.to_string_lossy().into(),
        );
        store.observe_agent(&metadata, &runtime).unwrap();
        let after = store.latest_sequence();
        append(
            &session,
            concat!(
                "{\"ordinal\":1,\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"turn-1\"}}\n",
                "{\"ordinal\":2,\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"<environment_context>cwd</environment_context>\"}]}}\n",
                "{\"ordinal\":3,\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"fix the build\"}]}}\n",
                "{\"ordinal\":4,\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"also run the tests\"}]}}\n"
            ),
        );
        store.observe_agent(&metadata, &runtime).unwrap();

        let accepted = store
            .read_page(after, 100)
            .unwrap()
            .events
            .into_iter()
            .filter(|event| event.kind == AgentEventKind::InputAccepted)
            .map(|event| (event.input_sha256.unwrap(), event.turn_id))
            .collect::<Vec<_>>();
        // Injected context is not input; the first message starts the turn
        // and the second steers it.
        assert_eq!(
            accepted,
            vec![
                (
                    crate::agent::message_sha256("fix the build"),
                    Some("turn-1".to_string())
                ),
                (
                    crate::agent::message_sha256("also run the tests"),
                    Some("turn-1".to_string())
                ),
            ]
        );
    }

    #[test]
    fn turn_confirmation_requires_a_new_turn_of_the_exact_incarnation() {
        let temp = TempDir::new().unwrap();
        let session = temp.path().join("codex.jsonl");
        fs::write(
            &session,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"session-codex\"}}\n",
        )
        .unwrap();
        let store = AgentEventStore::new(temp.path().join("events.sqlite3"));
        store.start_runtime_epoch().unwrap();
        let metadata = metadata("codex");
        let runtime = runtime(
            &metadata,
            AgentHarness::Codex,
            session.to_string_lossy().into(),
        );
        let incarnation = crate::agent_admission::incarnation_id(&metadata).unwrap();
        let mut writer = store.writer().unwrap();
        writer.observe_agent(&metadata, &runtime).unwrap();
        let work = crate::agent::message_sha256("work");
        let wait_for = |after: u64, agent_id: &str, incarnation_id: &str, sha256: &str| {
            promise::spawn::block_on(store.wait_for_input(
                agent_id,
                incarnation_id,
                Some(sha256),
                after,
                Duration::from_millis(50),
            ))
            .unwrap()
        };
        let wait = |after: u64, agent_id: &str, incarnation_id: &str, _: Duration| {
            wait_for(after, agent_id, incarnation_id, &work) == InputWait::Recorded
        };
        let short = Duration::from_millis(50);

        // Written input that never starts a turn is not confirmed.
        let before_write = store.latest_sequence();
        assert!(!wait(before_write, &metadata.agent_id, &incarnation, short));

        append(
            &session,
            concat!(
                "{\"ordinal\":1,\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"turn-1\"}}\n",
                "{\"ordinal\":2,\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"work\"}]}}\n"
            ),
        );
        writer.observe_agent(&metadata, &runtime).unwrap();
        assert!(wait(before_write, &metadata.agent_id, &incarnation, short));
        assert!(!wait(
            before_write,
            &metadata.agent_id,
            "other-incarnation",
            short
        ));
        assert!(!wait(before_write, "other-agent", &incarnation, short));
        // Recorded input with another hash, such as earlier text left in the
        // input box submitted together with this input, does not confirm it.
        assert!(matches!(
            wait_for(
                before_write,
                &metadata.agent_id,
                &incarnation,
                &crate::agent::message_sha256("other")
            ),
            InputWait::Pending {
                other_input: true,
                ..
            }
        ));
        // A turn that started before the write does not confirm it.
        assert!(!wait(
            store.latest_sequence(),
            &metadata.agent_id,
            &incarnation,
            short
        ));

        // A turn that starts while admission is waiting confirms it.
        let before_second_write = store.latest_sequence();
        let waiter = {
            let store = store.clone();
            let agent_id = metadata.agent_id.clone();
            let incarnation = incarnation.clone();
            thread::spawn(move || {
                promise::spawn::block_on(store.wait_for_input(
                    &agent_id,
                    &incarnation,
                    Some(&crate::agent::message_sha256("more")),
                    before_second_write,
                    Duration::from_secs(5),
                ))
                .unwrap()
                    == InputWait::Recorded
            })
        };
        thread::sleep(Duration::from_millis(50));
        append(
            &session,
            concat!(
                "{\"ordinal\":3,\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",\"turn_id\":\"turn-1\"}}\n",
                "{\"ordinal\":4,\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"turn-2\"}}\n",
                "{\"ordinal\":5,\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"more\"}]}}\n"
            ),
        );
        writer.observe_agent(&metadata, &runtime).unwrap();
        assert!(waiter.join().unwrap());
    }

    #[test]
    fn writer_notification_survives_reader_publishing_sequence_first() {
        let temp = TempDir::new().unwrap();
        let store = AgentEventStore::new(temp.path().join("events.sqlite3"));
        let changed = store.changed.listen();
        store.latest_sequence.store(42, Ordering::Release);

        store.publish_latest_sequence(42);

        promise::spawn::block_on(changed);
    }

    #[test]
    fn persistent_reader_serializes_concurrent_clients() {
        let temp = TempDir::new().unwrap();
        let store = AgentEventStore::new(temp.path().join("events.sqlite3"));
        store.start_runtime_epoch().unwrap();
        let threads = (0..8)
            .map(|_| {
                let store = store.clone();
                thread::spawn(move || {
                    for _ in 0..25 {
                        assert_eq!(
                            store.read_page(0, 100).unwrap().status,
                            AgentEventStatus::Ok
                        );
                    }
                })
            })
            .collect::<Vec<_>>();
        for thread in threads {
            thread.join().unwrap();
        }
    }

    #[test]
    fn persistent_reader_initializes_a_missing_store() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("events.sqlite3");
        let store = AgentEventStore::new(path.clone());

        assert_eq!(
            store.read_page(0, 100).unwrap().status,
            AgentEventStatus::Ok
        );
        assert!(path.exists());
    }

    #[test]
    fn codex_events_are_exact_durable_and_restart_resumable() {
        let temp = TempDir::new().unwrap();
        let session = temp.path().join("codex.jsonl");
        fs::write(
            &session,
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"session-codex\"}}\n",
                "{\"ordinal\":1,\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"turn-baseline\"}}\n"
            ),
        )
        .unwrap();
        let db = temp.path().join("events.sqlite3");
        let store = AgentEventStore::new(db.clone());
        let metadata = metadata("codex");
        let runtime = runtime(
            &metadata,
            AgentHarness::Codex,
            session.to_string_lossy().into(),
        );
        store.observe_agent(&metadata, &runtime).unwrap();
        let baseline_sequence = store.latest_sequence();

        append(
            &session,
            concat!(
                "{\"ordinal\":2,\"type\":\"event_msg\",\"timestamp\":\"2026-08-17T12:00:00Z\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"turn-α\"}}\n",
                "{\"ordinal\":3,\"type\":\"response_item\",\"timestamp\":\"2026-08-17T12:00:01Z\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"café ✓\"}],\"internal_chat_message_metadata_passthrough\":{\"turn_id\":\"turn-α\"}}}\n",
                "{\"ordinal\":4,\"type\":\"event_msg\",\"timestamp\":\"2026-08-17T12:00:02Z\",\"payload\":{\"type\":\"task_complete\",\"turn_id\":\"turn-α\",\"last_agent_message\":\"café ✓\"}}\n"
            ),
        );
        store.observe_agent(&metadata, &runtime).unwrap();
        let page = store.read_page(baseline_sequence, 100).unwrap();
        assert_eq!(page.status, AgentEventStatus::Ok);
        assert_eq!(
            event_kinds(&page),
            vec![
                AgentEventKind::TurnStarted,
                AgentEventKind::TurnStateChanged,
                AgentEventKind::AssistantMessage,
                AgentEventKind::TurnFinal,
                AgentEventKind::TurnStateChanged,
            ]
        );
        assert!(page
            .events
            .iter()
            .all(|event| event.turn_id.as_deref() == Some("turn-α")));
        assert_eq!(
            page.events
                .iter()
                .find(|event| event.kind == AgentEventKind::TurnFinal)
                .and_then(|event| event.text.as_deref()),
            Some("café ✓")
        );
        let last = page.next_after_sequence.unwrap();
        drop(store);

        let reopened = AgentEventStore::new(db);
        reopened.start_runtime_epoch().unwrap();
        assert!(reopened.latest_sequence() > last);
        let restart = reopened.read_page(last, 100).unwrap();
        assert_eq!(restart.events.len(), 1);
        assert_eq!(restart.events[0].kind, AgentEventKind::AgentLifecycle);
        assert_eq!(restart.events[0].lifecycle.as_deref(), Some("unavailable"));
        assert_eq!(restart.events[0].reason.as_deref(), Some("mux_restarted"));
        reopened.observe_agent(&metadata, &runtime).unwrap();
        let resumed = reopened
            .read_page(restart.next_after_sequence.unwrap(), 100)
            .unwrap();
        assert_eq!(resumed.events.len(), 1);
        assert_eq!(resumed.events[0].lifecycle.as_deref(), Some("available"));
    }

    #[test]
    fn rewritten_codex_source_reports_a_gap_and_rebaselines_without_replay() {
        let temp = TempDir::new().unwrap();
        let session = temp.path().join("codex.jsonl");
        fs::write(
            &session,
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"session-rewrite\"}}\n",
                "{\"ordinal\":1,\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"old-turn\"}}\n"
            ),
        )
        .unwrap();
        let store = AgentEventStore::new(temp.path().join("events.sqlite3"));
        let metadata = metadata("codex-rewrite");
        let runtime = runtime(
            &metadata,
            AgentHarness::Codex,
            session.to_string_lossy().into(),
        );
        store.observe_agent(&metadata, &runtime).unwrap();
        let after = store.latest_sequence();
        fs::write(
            &session,
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"session-rewrite\"}}\n",
                "{\"ordinal\":9,\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"must not replay\"}],\"internal_chat_message_metadata_passthrough\":{\"turn_id\":\"unrelated-turn\"}}}\n"
            ),
        )
        .unwrap();
        store.observe_agent(&metadata, &runtime).unwrap();
        let page = store.read_page(after, 100).unwrap();
        assert_eq!(event_kinds(&page), vec![AgentEventKind::ObserverFailure]);
        assert!(page.events[0]
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("new tail baseline")));
    }

    #[test]
    fn oversized_provider_record_reports_gap_and_publishes_later_final() {
        let temp = TempDir::new().unwrap();
        let session = temp.path().join("codex-oversized.jsonl");
        fs::write(
            &session,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"session-oversized\"}}\n",
        )
        .unwrap();
        let store = AgentEventStore::new(temp.path().join("events.sqlite3"));
        let metadata = metadata("codex-oversized");
        let runtime = runtime(
            &metadata,
            AgentHarness::Codex,
            session.to_string_lossy().into(),
        );
        store.observe_agent(&metadata, &runtime).unwrap();
        let after = store.latest_sequence();
        append(
            &session,
            concat!(
                "{\"ordinal\":1,\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"turn-before-gap\"}}\n",
                "{\"ordinal\":2,\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"stale text\"}]}}\n"
            ),
        );
        // Same envelope as the incident's 5,169,128-byte Codex tool output.
        append(&session, &format!("{{\"ordinal\":3,\"type\":\"response_item\",\"payload\":{{\"type\":\"custom_tool_call_output\",\"output\":\"{}\"}}}}\n", "x".repeat(5_169_128)));
        append(&session, concat!(
            "{\"ordinal\":4,\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"must not inherit stale turn\"}]}}\n",
            "{\"ordinal\":5,\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",\"turn_id\":\"turn-after-gap\",\"last_agent_message\":\"exact final\"}}\n"
        ));

        store.observe_agent(&metadata, &runtime).unwrap();
        let page = store.read_page(after, 100).unwrap();
        assert_eq!(
            event_kinds(&page),
            vec![
                AgentEventKind::TurnStarted,
                AgentEventKind::TurnStateChanged,
                AgentEventKind::AssistantMessage,
                AgentEventKind::ObserverFailure,
                AgentEventKind::ObserverFailure,
                AgentEventKind::TurnFinal,
                AgentEventKind::TurnStateChanged,
            ]
        );
        assert!(page.events[3]
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("exceeds the 4194304-byte bound")
                && detail.contains("resumed at the next record")));
        assert_eq!(page.events[5].turn_id.as_deref(), Some("turn-after-gap"));
        assert_eq!(page.events[5].text.as_deref(), Some("exact final"));
        assert_eq!(
            page.events[6].turn_state.as_deref(),
            Some("waiting_on_user")
        );
        let last = page.latest_sequence;
        let reopened = AgentEventStore::new(temp.path().join("events.sqlite3"));
        reopened.observe_agent(&metadata, &runtime).unwrap();
        assert!(reopened.read_page(last, 100).unwrap().events.is_empty());
    }

    #[test]
    #[ignore = "requires WAKTERM_TEST_CODEX_ROLLOUT pointing to the reported provider rollout"]
    fn real_codex_rollout_projects_final_after_oversized_tool_output() {
        let path =
            PathBuf::from(std::env::var_os("WAKTERM_TEST_CODEX_ROLLOUT").expect("rollout path"));
        let initial = project_codex(&path, None).unwrap();
        let ProviderCursor::Codex(mut cursor) = initial.cursor else {
            panic!("Codex cursor")
        };
        cursor.offset = 0;
        cursor.checkpoint_sha256 = checkpoint_sha256(&path, 0).unwrap();
        cursor.current_turn_id = None;
        cursor.last_assistant_text = None;
        let projected = project_codex(&path, Some(ProviderCursor::Codex(cursor))).unwrap();
        assert!(projected.events.iter().any(|event| {
            event.kind == AgentEventKind::ObserverFailure
                && event
                    .detail
                    .as_deref()
                    .is_some_and(|detail| detail.contains("resumed at the next record"))
        }));
        let terminal = projected
            .events
            .iter()
            .find(|event| {
                event.kind == AgentEventKind::TurnFinal
                    && event.turn_id.as_deref() == Some("01a0b363-569f-7b40-833d-62e7c68ed841")
            })
            .expect("reported turn's completion survives the earlier oversized record");
        assert_eq!(terminal.outcome.as_deref(), Some("completed"));
        assert!(terminal
            .text
            .as_deref()
            .is_some_and(|text| text.starts_with("This is the earlier `b04c73cc` direction")));
    }

    #[test]
    fn oversized_partial_record_waits_for_newline_and_clears_cached_text() {
        let temp = TempDir::new().unwrap();
        let session = temp.path().join("partial.jsonl");
        fs::write(
            &session,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"partial\"}}\n",
        )
        .unwrap();
        let initial = project_codex(&session, None).unwrap();
        let ProviderCursor::Codex(mut cursor) = initial.cursor else {
            panic!("Codex cursor")
        };
        cursor.current_turn_id = Some("stale-turn".to_string());
        cursor.last_assistant_text = Some("stale reply".to_string());
        let baseline = cursor.offset;
        append(
            &session,
            &format!(
                "{{\"padding\":\"{}\"}}",
                "x".repeat(MAX_PROVIDER_RECORD_BYTES as usize)
            ),
        );
        let partial = project_codex(&session, Some(ProviderCursor::Codex(cursor))).unwrap();
        assert!(partial.events.is_empty());
        let ProviderCursor::Codex(cursor) = &partial.cursor else {
            panic!("Codex cursor")
        };
        assert_eq!(cursor.offset, baseline);
        append(&session, "\n{\"ordinal\":2,\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",\"turn_id\":\"exact-turn\"}}\n");
        let complete = project_codex(&session, Some(partial.cursor)).unwrap();
        assert_eq!(complete.events.len(), 3);
        assert_eq!(complete.events[0].kind, AgentEventKind::ObserverFailure);
        assert_eq!(complete.events[1].kind, AgentEventKind::TurnFinal);
        assert_eq!(complete.events[1].turn_id.as_deref(), Some("exact-turn"));
        assert!(complete.events[1].text.is_none());
        let ProviderCursor::Codex(cursor) = complete.cursor else {
            panic!("Codex cursor")
        };
        assert_eq!(cursor.offset, fs::metadata(&session).unwrap().len());
    }

    #[test]
    fn claude_projects_plans_messages_and_finals() {
        let temp = TempDir::new().unwrap();
        let session = temp.path().join("claude.jsonl");
        fs::write(
            &session,
            "{\"type\":\"mode\",\"sessionId\":\"session-claude\"}\n",
        )
        .unwrap();
        let store = AgentEventStore::new(temp.path().join("events.sqlite3"));
        let metadata = metadata("claude");
        let runtime = runtime(
            &metadata,
            AgentHarness::Claude,
            session.to_string_lossy().into(),
        );
        store.observe_agent(&metadata, &runtime).unwrap();
        let after = store.latest_sequence();
        append(
            &session,
            concat!(
                "{\"type\":\"user\",\"uuid\":\"claude-turn-1\",\"sessionId\":\"session-claude\",\"timestamp\":\"2026-08-17T12:00:00Z\",\"message\":{\"content\":\"work\"}}\n",
                "{\"type\":\"assistant\",\"uuid\":\"claude-plan\",\"sessionId\":\"session-claude\",\"timestamp\":\"2026-08-17T12:00:01Z\",\"message\":{\"id\":\"msg-plan\",\"model\":\"claude\",\"stop_reason\":\"tool_use\",\"content\":[{\"type\":\"tool_use\",\"name\":\"ExitPlanMode\",\"input\":{\"plan\":\"Inspect then test.\"}}]}}\n",
                "{\"type\":\"user\",\"uuid\":\"claude-tool-result\",\"sessionId\":\"session-claude\",\"timestamp\":\"2026-08-17T12:00:01Z\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"tool-1\",\"content\":\"done\"}]}}\n",
                "{\"type\":\"assistant\",\"uuid\":\"claude-final-thinking\",\"sessionId\":\"session-claude\",\"timestamp\":\"2026-08-17T12:00:02Z\",\"message\":{\"id\":\"msg-final\",\"model\":\"claude-haiku-4-5\",\"stop_reason\":\"end_turn\",\"content\":[{\"type\":\"thinking\",\"thinking\":\"\"}]}}\n",
                "{\"type\":\"assistant\",\"uuid\":\"claude-final-text\",\"parentUuid\":\"claude-final-thinking\",\"sessionId\":\"session-claude\",\"timestamp\":\"2026-08-17T12:00:02Z\",\"message\":{\"id\":\"msg-final\",\"model\":\"claude-haiku-4-5\",\"stop_reason\":\"end_turn\",\"content\":[{\"type\":\"text\",\"text\":\"done ✓\"}]}}\n"
            ),
        );
        store.observe_agent(&metadata, &runtime).unwrap();
        let page = store.read_page(after, 100).unwrap();
        assert!(event_kinds(&page).contains(&AgentEventKind::Plan));
        assert!(event_kinds(&page).contains(&AgentEventKind::AssistantMessage));
        assert!(event_kinds(&page).contains(&AgentEventKind::TurnFinal));
        assert_eq!(
            page.events
                .iter()
                .filter(|event| event.kind == AgentEventKind::TurnStarted)
                .count(),
            1
        );
        assert_eq!(
            page.events
                .iter()
                .filter(|event| event.kind == AgentEventKind::TurnFinal)
                .count(),
            1
        );
        assert_eq!(
            page.events
                .iter()
                .find(|event| event.kind == AgentEventKind::TurnFinal)
                .and_then(|event| event.text.as_deref()),
            Some("done ✓")
        );
        assert!(page
            .events
            .iter()
            .all(|event| event.turn_id.as_deref() == Some("claude-turn-1")));
    }

    #[test]
    fn turn_started_carries_the_hash_of_the_input_that_started_it() {
        let started = |page: &AgentEventPage| {
            page.events
                .iter()
                .filter(|event| event.kind == AgentEventKind::TurnStarted)
                .map(|event| (event.turn_id.clone().unwrap(), event.input_sha256.clone()))
                .collect::<Vec<_>>()
        };
        let hash = |text: &str| Some(crate::agent::message_sha256(text));

        // Claude stores a paste inside a wrapper; the hash covers its text.
        let temp = TempDir::new().unwrap();
        let session = temp.path().join("claude.jsonl");
        fs::write(&session, "{\"type\":\"mode\",\"sessionId\":\"s\"}\n").unwrap();
        let store = AgentEventStore::new(temp.path().join("claude.sqlite3"));
        let claude = metadata("claude");
        let claude_runtime = runtime(
            &claude,
            AgentHarness::Claude,
            session.to_string_lossy().into(),
        );
        store.observe_agent(&claude, &claude_runtime).unwrap();
        let after = store.latest_sequence();
        append(
            &session,
            &format!(
                "{}\n",
                serde_json::json!({"type":"user","uuid":"claude-turn","sessionId":"s",
                    "timestamp":"2026-10-04T12:00:00Z","message":{"content":
                    "\n\n<pasted_content id=\"0551\">\nsummarize\n</pasted_content id=\"0551\">\n"}})
            ),
        );
        store.observe_agent(&claude, &claude_runtime).unwrap();
        assert_eq!(
            started(&store.read_page(after, 100).unwrap()),
            vec![("claude-turn".to_string(), hash("summarize"))]
        );

        // Codex records input after task_started; the start waits for the
        // first input that is not injected context.
        let session = temp.path().join("codex.jsonl");
        fs::write(
            &session,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"s\"}}\n",
        )
        .unwrap();
        let store = AgentEventStore::new(temp.path().join("codex.sqlite3"));
        let codex = metadata("codex");
        let codex_runtime = runtime(
            &codex,
            AgentHarness::Codex,
            session.to_string_lossy().into(),
        );
        store.observe_agent(&codex, &codex_runtime).unwrap();
        let after = store.latest_sequence();
        append(
            &session,
            "{\"ordinal\":1,\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"turn-1\"}}\n",
        );
        store.observe_agent(&codex, &codex_runtime).unwrap();
        assert!(started(&store.read_page(after, 100).unwrap()).is_empty());
        append(
            &session,
            concat!(
                "{\"ordinal\":2,\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"<environment_context>cwd</environment_context>\"}]}}\n",
                "{\"ordinal\":3,\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"fix the build\"}]}}\n",
                "{\"ordinal\":4,\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",\"turn_id\":\"turn-1\",\"last_agent_message\":\"done\"}}\n",
                // A turn that records no input still starts before its final.
                "{\"ordinal\":5,\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"turn-2\"}}\n",
                "{\"ordinal\":6,\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",\"turn_id\":\"turn-2\",\"last_agent_message\":\"compacted\"}}\n"
            ),
        );
        store.observe_agent(&codex, &codex_runtime).unwrap();
        let page = store.read_page(after, 100).unwrap();
        assert_eq!(
            started(&page),
            vec![
                ("turn-1".to_string(), hash("fix the build")),
                ("turn-2".to_string(), None)
            ]
        );
        let kinds = event_kinds(&page);
        let first_start = kinds
            .iter()
            .position(|kind| *kind == AgentEventKind::TurnStarted);
        let first_final = kinds
            .iter()
            .position(|kind| *kind == AgentEventKind::TurnFinal);
        assert!(first_start < first_final);
    }

    #[test]
    fn agy_projects_turns_from_the_full_step_log() {
        let temp = TempDir::new().unwrap();
        let logs = temp
            .path()
            .join("brain")
            .join("conversation-1")
            .join(".system_generated")
            .join("logs");
        fs::create_dir_all(&logs).unwrap();
        let record = |step: u64, kind: &str, fields: serde_json::Value| {
            let mut record = serde_json::json!({"step_index": step, "source": "MODEL",
                "type": kind, "status": "DONE", "created_at": "2026-10-05T06:00:00Z"});
            record
                .as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            record.to_string() + "\n"
        };
        let reply = "The logs show two timeouts.";
        // The bound transcript truncates long fields; its full sibling does not.
        let session = logs.join("transcript.jsonl");
        let full = logs.join("transcript_full.jsonl");
        let earlier = record(
            0,
            "USER_INPUT",
            serde_json::json!({"content": "<USER_REQUEST>\nhi\n</USER_REQUEST>"}),
        ) + &record(
            1,
            "PLANNER_RESPONSE",
            serde_json::json!({"content": "hello"}),
        );
        fs::write(&session, &earlier).unwrap();
        fs::write(&full, &earlier).unwrap();
        let store = AgentEventStore::new(temp.path().join("events.sqlite3"));
        let metadata = metadata("agy");
        let runtime = runtime(
            &metadata,
            AgentHarness::Agy,
            session.to_string_lossy().into(),
        );
        store.observe_agent(&metadata, &runtime).unwrap();
        let after = store.latest_sequence();

        append(
            &full,
            &(record(
                10,
                "USER_INPUT",
                serde_json::json!({"content": "<USER_REQUEST>\nsummarize the logs\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nlocal time\n</ADDITIONAL_METADATA>"}),
            ) + &record(
                11,
                "PLANNER_RESPONSE",
                serde_json::json!({"content": "Reading them.", "tool_calls": [{"name": "view_file"}]}),
            ) + &record(12, "VIEW_FILE", serde_json::json!({"content": "log text"}))
                + &record(
                    13,
                    "FINISH",
                    serde_json::json!({"content": "Task is complete."}),
                )
                + &record(
                    14,
                    "PLANNER_RESPONSE",
                    serde_json::json!({"content": reply}),
                )),
        );
        append(
            &session,
            &record(
                14,
                "PLANNER_RESPONSE",
                serde_json::json!({"content": "The logs", "truncated_fields": ["content"]}),
            ),
        );
        store.observe_agent(&metadata, &runtime).unwrap();

        let events = store.read_page(after, 100).unwrap().events;
        assert_eq!(
            events
                .iter()
                .map(|event| event.kind.clone())
                .collect::<Vec<_>>(),
            vec![
                AgentEventKind::TurnStarted,
                AgentEventKind::InputAccepted,
                AgentEventKind::TurnStateChanged,
                AgentEventKind::AssistantMessage,
                AgentEventKind::AssistantMessage,
                AgentEventKind::TurnFinal,
                AgentEventKind::TurnStateChanged,
            ]
        );
        assert!(events
            .iter()
            .all(|event| event.turn_id.as_deref() == Some("conversation-1:10")));
        let sha256 = crate::agent::message_sha256("summarize the logs");
        assert_eq!(events[0].input_sha256.as_deref(), Some(sha256.as_str()));
        assert_eq!(events[1].input_sha256.as_deref(), Some(sha256.as_str()));
        assert_eq!(events[5].outcome.as_deref(), Some("completed"));
        assert_eq!(events[5].text.as_deref(), Some(reply));
    }

    #[test]
    fn claude_records_input_that_starts_a_turn_or_is_queued_into_one() {
        let temp = TempDir::new().unwrap();
        let session = temp.path().join("claude.jsonl");
        fs::write(
            &session,
            "{\"type\":\"mode\",\"sessionId\":\"session-claude\"}\n",
        )
        .unwrap();
        let store = AgentEventStore::new(temp.path().join("events.sqlite3"));
        let metadata = metadata("claude");
        let runtime = runtime(
            &metadata,
            AgentHarness::Claude,
            session.to_string_lossy().into(),
        );
        store.observe_agent(&metadata, &runtime).unwrap();
        let after = store.latest_sequence();
        let line = |value: serde_json::Value| value.to_string() + "\n";
        append(
            &session,
            &[
                line(serde_json::json!({"type":"user","uuid":"turn-1","sessionId":"session-claude",
                    "timestamp":"2026-10-07T10:00:00Z","message":{"content":"<pasted_content id=\"1\">\nwork\n</pasted_content id=\"1\">"}})),
                line(serde_json::json!({"type":"queue-operation","operation":"enqueue",
                    "timestamp":"2026-10-07T10:00:02Z","sessionId":"session-claude",
                    "content":"<task-notification>done</task-notification>"})),
                line(serde_json::json!({"type":"queue-operation","operation":"enqueue",
                    "timestamp":"2026-10-07T10:00:03Z","sessionId":"session-claude",
                    "content":"<pasted_content id=\"2\">\nalso check stderr\n</pasted_content id=\"2\">"})),
            ]
            .concat(),
        );
        store.observe_agent(&metadata, &runtime).unwrap();

        let accepted = store
            .read_page(after, 100)
            .unwrap()
            .events
            .into_iter()
            .filter(|event| event.kind == AgentEventKind::InputAccepted)
            .map(|event| (event.input_sha256.unwrap(), event.turn_id, event.reason))
            .collect::<Vec<_>>();
        // The notification is not input; the steer joins the running turn.
        assert_eq!(
            accepted,
            vec![
                (
                    crate::agent::message_sha256("work"),
                    Some("turn-1".to_string()),
                    None
                ),
                (
                    crate::agent::message_sha256("also check stderr"),
                    Some("turn-1".to_string()),
                    Some("queued".to_string())
                ),
            ]
        );
    }

    #[test]
    fn invalid_complete_provider_line_is_reported_once_and_skipped() {
        let temp = TempDir::new().unwrap();
        let session = temp.path().join("claude.jsonl");
        fs::write(
            &session,
            "{\"type\":\"mode\",\"sessionId\":\"session-claude\"}\n",
        )
        .unwrap();
        let store = AgentEventStore::new(temp.path().join("events.sqlite3"));
        let metadata = metadata("claude");
        let runtime = runtime(
            &metadata,
            AgentHarness::Claude,
            session.to_string_lossy().into(),
        );
        store.observe_agent(&metadata, &runtime).unwrap();
        let after = store.latest_sequence();
        // A write cut short by a full disk, with the next record appended to
        // the same line.
        append(
            &session,
            concat!(
                "{\"parentUuid\":\"a\",\"command\":\"cat runs-{\"parentUuid\":\"b\",\"type\":\"system\"}\n",
                "{\"type\":\"user\",\"uuid\":\"turn-after\",\"sessionId\":\"session-claude\",",
                "\"timestamp\":\"2026-10-08T02:51:00Z\",\"message\":{\"content\":\"after the gap\"}}\n"
            ),
        );
        store.observe_agent(&metadata, &runtime).unwrap();

        let page = store.read_page(after, 100).unwrap();
        let failures = page
            .events
            .iter()
            .filter(|event| event.kind == AgentEventKind::ObserverFailure)
            .collect::<Vec<_>>();
        assert_eq!(failures.len(), 1);
        assert!(failures[0]
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("not valid JSON")
                && detail.contains("resumed at the next record")));
        assert!(page
            .events
            .iter()
            .any(|event| event.kind == AgentEventKind::InputAccepted
                && event.input_sha256.as_deref()
                    == Some(crate::agent::message_sha256("after the gap").as_str())));

        let last = page.latest_sequence;
        store.observe_agent(&metadata, &runtime).unwrap();
        assert!(store.read_page(last, 100).unwrap().events.is_empty());
    }

    #[test]
    fn claude_turn_resumes_after_a_skipped_line_when_its_prompt_continues() {
        // (prompt of the tool result after the gap, turn expected for the final)
        for (tool_prompt, expected_turn) in [("prompt-1", Some("turn-1")), ("prompt-2", None)] {
            let temp = TempDir::new().unwrap();
            let session = temp.path().join("claude.jsonl");
            fs::write(
                &session,
                "{\"type\":\"mode\",\"sessionId\":\"session-claude\"}\n",
            )
            .unwrap();
            let store = AgentEventStore::new(temp.path().join("events.sqlite3"));
            let metadata = metadata("claude");
            let runtime = runtime(
                &metadata,
                AgentHarness::Claude,
                session.to_string_lossy().into(),
            );
            store.observe_agent(&metadata, &runtime).unwrap();
            let after = store.latest_sequence();
            let line = |value: serde_json::Value| value.to_string() + "\n";
            let assistant = |uuid: &str, stop: &str, text: &str| {
                line(
                    serde_json::json!({"type":"assistant","uuid":uuid,"sessionId":"session-claude",
                    "timestamp":"2026-10-07T21:30:00Z","message":{"id":uuid,"model":"claude",
                    "stop_reason":stop,"content":[{"type":"text","text":text}]}}),
                )
            };
            append(
                &session,
                &[
                    line(serde_json::json!({"type":"user","uuid":"turn-1","promptId":"prompt-1",
                        "sessionId":"session-claude","timestamp":"2026-10-07T21:29:00Z",
                        "message":{"content":"work"}})),
                    assistant("before-gap", "tool_use", "checking"),
                    "{\"parentUuid\":\"a\",\"command\":\"cat runs-{\"parentUuid\":\"b\"}\n".to_string(),
                    assistant("after-gap", "tool_use", "still checking"),
                    line(serde_json::json!({"type":"user","uuid":"tool-result","promptId":tool_prompt,
                        "sessionId":"session-claude","timestamp":"2026-10-07T21:31:00Z",
                        "message":{"content":[{"type":"tool_result","tool_use_id":"t","content":"ok"}]}})),
                    assistant("final", "end_turn", "done"),
                ]
                .concat(),
            );
            store.observe_agent(&metadata, &runtime).unwrap();

            let events = store.read_page(after, 100).unwrap().events;
            let missing_turn = |event: &AgentEvent| {
                event.kind == AgentEventKind::ObserverFailure
                    && event
                        .detail
                        .as_deref()
                        .is_some_and(|detail| detail.contains("lacks an exact provider turn id"))
            };
            // The reply between the gap and the tool result has no known turn.
            assert!(events.iter().any(missing_turn), "{tool_prompt}");
            let final_turn = events
                .iter()
                .find(|event| event.kind == AgentEventKind::TurnFinal)
                .and_then(|event| event.turn_id.as_deref());
            assert_eq!(final_turn, expected_turn, "{tool_prompt}");
            if expected_turn.is_none() {
                assert_eq!(events.iter().filter(|event| missing_turn(event)).count(), 2);
            }
        }
    }

    #[test]
    fn resumed_claude_process_reports_input_written_before_it_was_observed() {
        let temp = TempDir::new().unwrap();
        let session = temp.path().join("claude.jsonl");
        fs::write(
            &session,
            "{\"type\":\"mode\",\"sessionId\":\"session-claude\"}\n",
        )
        .unwrap();
        let store = AgentEventStore::new(temp.path().join("events.sqlite3"));
        let mut metadata = metadata("claude");
        let runtime_for = |metadata: &AgentMetadata| {
            runtime(
                metadata,
                AgentHarness::Claude,
                session.to_string_lossy().into(),
            )
        };
        store
            .observe_agent(&metadata, &runtime_for(&metadata))
            .unwrap();
        let after = store.latest_sequence();

        // The process is replaced, and the resumed one records input before
        // the observer sees it.
        metadata.adopted_pid = Some(43);
        append(
            &session,
            &(serde_json::json!({"type":"user","uuid":"turn-1","sessionId":"session-claude",
                "timestamp":"2026-10-07T17:22:54Z","message":{"content":"still there?"}})
            .to_string()
                + "\n"),
        );
        store
            .observe_agent(&metadata, &runtime_for(&metadata))
            .unwrap();

        let page = store.read_page(after, 100).unwrap();
        assert!(page
            .events
            .iter()
            .any(|event| event.kind == AgentEventKind::AgentLifecycle
                && event.reason.as_deref() == Some("incarnation_replaced")));
        let accepted = page
            .events
            .iter()
            .filter(|event| event.kind == AgentEventKind::InputAccepted)
            .map(|event| event.input_sha256.clone().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(accepted, vec![crate::agent::message_sha256("still there?")]);
        assert!(page
            .events
            .iter()
            .any(|event| event.kind == AgentEventKind::TurnStarted));
    }

    #[test]
    fn claude_idle_report_closes_a_turn_left_without_a_reply() {
        let temp = TempDir::new().unwrap();
        let session = temp.path().join("claude.jsonl");
        fs::write(
            &session,
            "{\"type\":\"mode\",\"sessionId\":\"session-claude\"}\n",
        )
        .unwrap();
        let store = AgentEventStore::new(temp.path().join("events.sqlite3"));
        let metadata = metadata("claude");
        let mut runtime = runtime(
            &metadata,
            AgentHarness::Claude,
            session.to_string_lossy().into(),
        );
        store.observe_agent(&metadata, &runtime).unwrap();
        let after = store.latest_sequence();
        // Claude displays a notice as input but never works on it.
        append(
            &session,
            "{\"type\":\"user\",\"uuid\":\"claude-notice\",\"sessionId\":\"session-claude\",\"timestamp\":\"2026-10-04T14:50:06Z\",\"message\":{\"content\":\"<task-notification>done</task-notification>\"}}\n",
        );
        store.observe_agent(&metadata, &runtime).unwrap();
        let set_age = |seconds: u64| {
            fs::File::options()
                .write(true)
                .open(&session)
                .unwrap()
                .set_modified(std::time::SystemTime::now() - Duration::from_secs(seconds))
                .unwrap();
        };
        let finals = || {
            store
                .read_page(after, 100)
                .unwrap()
                .events
                .into_iter()
                .filter(|event| event.kind == AgentEventKind::TurnFinal)
                .collect::<Vec<_>>()
        };

        // Without Claude's idle report the turn stays open.
        set_age(60);
        store.observe_agent(&metadata, &runtime).unwrap();
        assert!(finals().is_empty());

        // An idle report right after a write may precede the final reply.
        runtime.turn_phase = Some("idle".to_string());
        set_age(0);
        store.observe_agent(&metadata, &runtime).unwrap();
        assert!(finals().is_empty());

        set_age(60);
        store.observe_agent(&metadata, &runtime).unwrap();
        store.observe_agent(&metadata, &runtime).unwrap();
        let finals = finals();
        assert_eq!(finals.len(), 1);
        assert_eq!(finals[0].turn_id.as_deref(), Some("claude-notice"));
        assert_eq!(finals[0].outcome.as_deref(), Some("aborted"));
        assert_eq!(finals[0].reason.as_deref(), Some("no_reply"));
        let page = store.read_page(after, 100).unwrap();
        assert_eq!(
            page.events
                .last()
                .and_then(|event| event.turn_state.as_deref()),
            Some("waiting_on_user")
        );

        // A turn that ends with a reply is not closed again.
        let after_close = store.latest_sequence();
        append(
            &session,
            concat!(
                "{\"type\":\"user\",\"uuid\":\"claude-turn-2\",\"sessionId\":\"session-claude\",\"timestamp\":\"2026-10-04T15:00:00Z\",\"message\":{\"content\":\"work\"}}\n",
                "{\"type\":\"assistant\",\"uuid\":\"claude-reply\",\"sessionId\":\"session-claude\",\"timestamp\":\"2026-10-04T15:00:02Z\",\"message\":{\"id\":\"msg-2\",\"model\":\"claude\",\"stop_reason\":\"end_turn\",\"content\":[{\"type\":\"text\",\"text\":\"done\"}]}}\n"
            ),
        );
        store.observe_agent(&metadata, &runtime).unwrap();
        set_age(60);
        store.observe_agent(&metadata, &runtime).unwrap();
        let outcomes = store
            .read_page(after_close, 100)
            .unwrap()
            .events
            .into_iter()
            .filter(|event| event.kind == AgentEventKind::TurnFinal)
            .map(|event| event.outcome)
            .collect::<Vec<_>>();
        assert_eq!(outcomes, vec![Some("completed".to_string())]);
    }

    #[test]
    fn claude_projects_single_choice_questions() {
        let temp = TempDir::new().unwrap();
        let session = temp.path().join("claude-question.jsonl");
        fs::write(
            &session,
            "{\"type\":\"mode\",\"sessionId\":\"session-claude\"}\n",
        )
        .unwrap();
        let store = AgentEventStore::new(temp.path().join("events.sqlite3"));
        let metadata = metadata("claude-question");
        let runtime = runtime(
            &metadata,
            AgentHarness::Claude,
            session.to_string_lossy().into(),
        );
        store.observe_agent(&metadata, &runtime).unwrap();
        let after = store.latest_sequence();
        append(
            &session,
            &format!(
                "{}\n{}\n",
                serde_json::json!({
                    "type": "user",
                    "uuid": "claude-turn-question",
                    "message": {"content": "work"}
                }),
                serde_json::json!({
                    "type": "assistant",
                    "uuid": "claude-question",
                    "message": {
                        "model": "claude",
                        "stop_reason": "tool_use",
                        "content": [{
                            "type": "tool_use",
                            "id": "tool-question",
                            "name": "AskUserQuestion",
                            "input": {"questions": [{
                                "header": "Promotion scope",
                                "question": "How should I promote the build?",
                                "multiSelect": false,
                                "options": [
                                    {"label": "Build and activate", "description": "Replace the shared runtime."},
                                    {"label": "Hold off", "description": "Keep the current runtime."}
                                ]
                            }]}
                        }]
                    }
                })
            ),
        );
        store.observe_agent(&metadata, &runtime).unwrap();
        let page = store.read_page(after, 100).unwrap();
        let event = page
            .events
            .iter()
            .find(|event| event.kind == AgentEventKind::ApprovalRequested)
            .unwrap();
        let approval = event.approval.as_ref().unwrap();
        assert_eq!(approval.kind, "user_question");
        assert_eq!(
            approval.prompt.as_deref(),
            Some("How should I promote the build?")
        );
        assert_eq!(approval.choices[1].label, "Hold off");
        assert_eq!(
            store.find_approval(&approval.request_id).unwrap().unwrap(),
            *approval
        );
    }

    #[test]
    fn claude_synthetic_provider_error_is_an_observer_failure_not_a_final() {
        let temp = TempDir::new().unwrap();
        let session = temp.path().join("claude-error.jsonl");
        fs::write(
            &session,
            "{\"type\":\"queue-operation\",\"sessionId\":\"session-claude-error\"}\n",
        )
        .unwrap();
        let store = AgentEventStore::new(temp.path().join("events.sqlite3"));
        let metadata = metadata("claude-error");
        let runtime = runtime(
            &metadata,
            AgentHarness::Claude,
            session.to_string_lossy().into(),
        );
        store.observe_agent(&metadata, &runtime).unwrap();
        let after = store.latest_sequence();
        append(
            &session,
            concat!(
                "{\"type\":\"user\",\"uuid\":\"claude-error-turn\",\"sessionId\":\"session-claude-error\",\"message\":{\"content\":\"work\"}}\n",
                "{\"type\":\"assistant\",\"uuid\":\"claude-synthetic-error\",\"sessionId\":\"session-claude-error\",\"error\":\"authentication_failed\",\"message\":{\"model\":\"<synthetic>\",\"stop_reason\":\"stop_sequence\",\"content\":[{\"type\":\"text\",\"text\":\"Failed to authenticate\"}]}}\n"
            ),
        );

        store.observe_agent(&metadata, &runtime).unwrap();
        let page = store.read_page(after, 100).unwrap();
        assert!(event_kinds(&page).contains(&AgentEventKind::ObserverFailure));
        assert!(!event_kinds(&page).contains(&AgentEventKind::TurnFinal));
        assert!(page
            .events
            .iter()
            .all(|event| { event.turn_id.as_deref() == Some("claude-error-turn") }));
    }

    #[test]
    fn claude_retryable_api_error_is_an_observer_failure_not_a_final() {
        let temp = TempDir::new().unwrap();
        let session = temp.path().join("claude-api-error.jsonl");
        fs::write(
            &session,
            "{\"type\":\"queue-operation\",\"sessionId\":\"session-claude-api-error\"}\n",
        )
        .unwrap();
        let store = AgentEventStore::new(temp.path().join("events.sqlite3"));
        let metadata = metadata("claude-api-error");
        let runtime = runtime(
            &metadata,
            AgentHarness::Claude,
            session.to_string_lossy().into(),
        );
        store.observe_agent(&metadata, &runtime).unwrap();
        let after = store.latest_sequence();
        append(
            &session,
            concat!(
                "{\"type\":\"user\",\"uuid\":\"claude-api-error-turn\",\"sessionId\":\"session-claude-api-error\",\"message\":{\"content\":\"work\"}}\n",
                "{\"type\":\"system\",\"subtype\":\"api_error\",\"uuid\":\"claude-api-error\",\"sessionId\":\"session-claude-api-error\",\"error\":{\"status\":529,\"error\":{\"error\":{\"message\":\"Overloaded\"}}},\"retryAttempt\":1,\"maxRetries\":10}\n"
            ),
        );

        store.observe_agent(&metadata, &runtime).unwrap();
        let page = store.read_page(after, 100).unwrap();
        let failure = page
            .events
            .iter()
            .find(|event| event.kind == AgentEventKind::ObserverFailure)
            .unwrap();
        assert_eq!(failure.turn_id.as_deref(), Some("claude-api-error-turn"));
        assert_eq!(failure.detail.as_deref(), Some("Overloaded"));
        assert!(!event_kinds(&page).contains(&AgentEventKind::TurnFinal));
    }

    #[test]
    fn gemini_projects_completed_response_with_user_turn_identity() {
        let temp = TempDir::new().unwrap();
        let session = temp.path().join("gemini.json");
        fs::write(&session, r#"{"sessionId":"session-gemini","messages":[]}"#).unwrap();
        let store = AgentEventStore::new(temp.path().join("events.sqlite3"));
        let metadata = metadata("gemini");
        let runtime = runtime(
            &metadata,
            AgentHarness::Gemini,
            session.to_string_lossy().into(),
        );
        store.observe_agent(&metadata, &runtime).unwrap();
        let after = store.latest_sequence();
        fs::write(
            &session,
            r#"{"sessionId":"session-gemini","messages":[{"id":"gemini-turn-1","type":"user","timestamp":"2026-08-17T12:00:00Z","content":[{"text":"work"}]},{"id":"gemini-response-1","type":"gemini","timestamp":"2026-08-17T12:00:01Z","content":"done café ✓"}]}"#,
        )
        .unwrap();
        store.observe_agent(&metadata, &runtime).unwrap();
        let page = store.read_page(after, 100).unwrap();
        assert!(event_kinds(&page).contains(&AgentEventKind::TurnFinal));
        assert!(page
            .events
            .iter()
            .all(|event| event.turn_id.as_deref() == Some("gemini-turn-1")));
    }

    #[test]
    fn gemini_provider_error_is_observed_without_guessing_a_final() {
        let temp = TempDir::new().unwrap();
        let session = temp.path().join("gemini-error.json");
        fs::write(
            &session,
            r#"{"sessionId":"session-gemini-error","messages":[]}"#,
        )
        .unwrap();
        let store = AgentEventStore::new(temp.path().join("events.sqlite3"));
        let metadata = metadata("gemini-error");
        let runtime = runtime(
            &metadata,
            AgentHarness::Gemini,
            session.to_string_lossy().into(),
        );
        store.observe_agent(&metadata, &runtime).unwrap();
        let after = store.latest_sequence();
        fs::write(
            &session,
            r#"{"sessionId":"session-gemini-error","messages":[{"id":"gemini-error-turn","type":"user","content":"work"},{"id":"gemini-error-record","type":"error","content":"provider unavailable"}]}"#,
        )
        .unwrap();

        store.observe_agent(&metadata, &runtime).unwrap();
        let page = store.read_page(after, 100).unwrap();
        assert!(event_kinds(&page).contains(&AgentEventKind::ObserverFailure));
        assert!(!event_kinds(&page).contains(&AgentEventKind::TurnFinal));
        assert!(page
            .events
            .iter()
            .all(|event| { event.turn_id.as_deref() == Some("gemini-error-turn") }));
    }

    #[test]
    fn gemini_jsonl_projects_current_append_only_session_format() {
        let temp = TempDir::new().unwrap();
        let session = temp.path().join("session-2026-08-17-current.jsonl");
        fs::write(
            &session,
            "{\"sessionId\":\"session-gemini-jsonl\",\"projectHash\":\"hash\",\"startTime\":\"2026-08-17T12:00:00Z\",\"lastUpdated\":\"2026-08-17T12:00:00Z\"}\n",
        )
        .unwrap();
        let store = AgentEventStore::new(temp.path().join("events.sqlite3"));
        let metadata = metadata("gemini-jsonl");
        let runtime = runtime(
            &metadata,
            AgentHarness::Gemini,
            session.to_string_lossy().into(),
        );
        store.observe_agent(&metadata, &runtime).unwrap();
        let after = store.latest_sequence();
        append(
            &session,
            concat!(
                "{\"id\":\"gemini-jsonl-turn\",\"timestamp\":\"2026-08-17T12:00:01Z\",\"type\":\"user\",\"content\":[{\"text\":\"work\"}]}\n",
                "{\"$set\":{\"lastUpdated\":\"2026-08-17T12:00:01Z\"}}\n",
                "{\"id\":\"gemini-jsonl-tool\",\"timestamp\":\"2026-08-17T12:00:02Z\",\"type\":\"gemini\",\"content\":\"I will inspect first.\",\"toolCalls\":[{\"id\":\"call-1\",\"name\":\"read_file\"}]}\n",
                "{\"id\":\"gemini-jsonl-response\",\"timestamp\":\"2026-08-17T12:00:02Z\",\"type\":\"gemini\",\"content\":\"current café ✓\",\"model\":\"gemini-3-flash\"}\n",
                "{\"id\":\"gemini-jsonl-response\",\"timestamp\":\"2026-08-17T12:00:02Z\",\"type\":\"gemini\",\"content\":\"current café ✓\",\"model\":\"gemini-3-flash\",\"tokens\":{\"total\":10}}\n",
                "{\"$set\":{\"lastUpdated\":\"2026-08-17T12:00:02Z\"}}\n"
            ),
        );
        store.observe_agent(&metadata, &runtime).unwrap();
        let page = store.read_page(after, 100).unwrap();
        assert_eq!(
            page.events
                .iter()
                .filter(|event| event.kind == AgentEventKind::AssistantMessage)
                .count(),
            2
        );
        assert_eq!(
            page.events
                .iter()
                .filter(|event| event.kind == AgentEventKind::TurnFinal)
                .count(),
            1
        );
        assert!(page
            .events
            .iter()
            .all(|event| event.turn_id.as_deref() == Some("gemini-jsonl-turn")));

        let after_first_gap = store.latest_sequence();
        append(
            &session,
            "{\"id\":\"gemini-jsonl-response\",\"timestamp\":\"2026-08-17T12:00:02Z\",\"type\":\"gemini\",\"content\":\"rewritten once\"}\n",
        );
        store.observe_agent(&metadata, &runtime).unwrap();
        let first_gap = store.read_page(after_first_gap, 100).unwrap();
        assert_eq!(
            event_kinds(&first_gap),
            vec![AgentEventKind::ObserverFailure]
        );

        let after_second_gap = store.latest_sequence();
        append(
            &session,
            "{\"id\":\"gemini-jsonl-response\",\"timestamp\":\"2026-08-17T12:00:02Z\",\"type\":\"gemini\",\"content\":\"rewritten twice\"}\n",
        );
        store.observe_agent(&metadata, &runtime).unwrap();
        let second_gap = store.read_page(after_second_gap, 100).unwrap();
        assert_eq!(
            event_kinds(&second_gap),
            vec![AgentEventKind::ObserverFailure]
        );
        assert_ne!(first_gap.events[0].event_id, second_gap.events[0].event_id);
    }

    fn create_opencode_db(path: &Path) -> Connection {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE message (
                 id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
                 time_created INTEGER NOT NULL, data TEXT NOT NULL
             );
             CREATE TABLE part (
                 id TEXT PRIMARY KEY, message_id TEXT NOT NULL,
                 session_id TEXT NOT NULL, time_created INTEGER NOT NULL,
                 data TEXT NOT NULL
             );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn zcode_skips_injected_prompts_and_reports_provider_failures_once() {
        let temp = TempDir::new().unwrap();
        let provider_db = temp.path().join("db.sqlite");
        let conn = create_opencode_db(&provider_db);
        let store = AgentEventStore::new(temp.path().join("events.sqlite3"));
        let metadata = metadata("zcode");
        let session = "sess_00000000-0000-4000-8000-0000000000aa";
        let session_path = format!(
            "opencode://session?db={}&id={session}",
            url::form_urlencoded::byte_serialize(provider_db.to_string_lossy().as_bytes())
                .collect::<String>()
        );
        let runtime = runtime(&metadata, AgentHarness::Zcode, session_path);
        store.observe_agent(&metadata, &runtime).unwrap();
        let after = store.latest_sequence();
        let insert = |id: &str, at: i64, data: &str| {
            conn.execute(
                "INSERT INTO message VALUES (?1, ?2, ?3, ?4)",
                params![id, session, at, data],
            )
            .unwrap();
        };
        insert(
            "z-prompt",
            1_776_700_000_000,
            r#"{"role":"user","semantics":{"origin":"real_user","kind":"user_prompt"}}"#,
        );
        insert(
            "z-reminder",
            1_776_700_000_100,
            r#"{"role":"user","synthetic":true,"semantics":{"origin":"agent_runtime","kind":"todo_reminder"}}"#,
        );
        insert(
            "z-step",
            1_776_700_000_200,
            r#"{"role":"assistant","parentID":"z-prompt"}"#,
        );
        store.observe_agent(&metadata, &runtime).unwrap();
        // ZCode records the failure on the step after first writing it.
        conn.execute(
            "UPDATE message SET data = ?1 WHERE id = 'z-step'",
            params![r#"{"role":"assistant","parentID":"z-prompt","time":{"completed":1776700001000},"error":{"name":"AiSdkModelAdapterError","data":{"message":"Usage limit reached for 5 hour","code":"model_rate_limit"}}}"#],
        )
        .unwrap();
        store.observe_agent(&metadata, &runtime).unwrap();
        store.observe_agent(&metadata, &runtime).unwrap();

        let page = store.read_page(after, 100).unwrap();
        let of_kind = |kind| {
            page.events
                .iter()
                .filter(|event| event.kind == kind)
                .collect::<Vec<_>>()
        };
        let started = of_kind(AgentEventKind::TurnStarted);
        assert_eq!(started.len(), 1);
        assert_eq!(started[0].turn_id.as_deref(), Some("z-prompt"));
        let failures = of_kind(AgentEventKind::ObserverFailure);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].turn_id.as_deref(), Some("z-prompt"));
        assert_eq!(
            failures[0].detail.as_deref(),
            Some("Usage limit reached for 5 hour")
        );
        assert!(of_kind(AgentEventKind::TurnFinal).is_empty());
    }

    #[test]
    fn opencode_turn_start_waits_for_and_hashes_the_user_text() {
        let temp = TempDir::new().unwrap();
        let provider_db = temp.path().join("opencode.sqlite3");
        let conn = create_opencode_db(&provider_db);
        let store = AgentEventStore::new(temp.path().join("events.sqlite3"));
        let metadata = metadata("opencode");
        let session_path = format!(
            "opencode://session?db={}&id=session-hash",
            url::form_urlencoded::byte_serialize(provider_db.to_string_lossy().as_bytes())
                .collect::<String>()
        );
        let runtime = runtime(&metadata, AgentHarness::Opencode, session_path);
        store.observe_agent(&metadata, &runtime).unwrap();
        let after = store.latest_sequence();
        let started = || {
            store
                .read_page(after, 100)
                .unwrap()
                .events
                .into_iter()
                .filter(|event| event.kind == AgentEventKind::TurnStarted)
                .collect::<Vec<_>>()
        };
        let part = |id: &str, data: &str| {
            conn.execute(
                "INSERT INTO part VALUES (?1, 'turn-hash', 'session-hash', 1776600000100, ?2)",
                params![id, data],
            )
            .unwrap();
        };
        conn.execute(
            "INSERT INTO message VALUES ('turn-hash', 'session-hash', 1776600000000, ?1)",
            params![r#"{"role":"user"}"#],
        )
        .unwrap();

        // OpenCode writes the message before its text.
        store.observe_agent(&metadata, &runtime).unwrap();
        assert!(started().is_empty());

        part(
            "part-file",
            r#"{"type":"text","text":"file contents","synthetic":true}"#,
        );
        part(
            "part-text",
            r#"{"type":"text","text":"summarize the logs"}"#,
        );
        store.observe_agent(&metadata, &runtime).unwrap();
        let started = started();
        assert_eq!(started.len(), 1);
        assert_eq!(started[0].turn_id.as_deref(), Some("turn-hash"));
        assert_eq!(
            started[0].input_sha256.as_deref(),
            Some(crate::agent::message_sha256("summarize the logs").as_str())
        );
    }

    #[test]
    fn opencode_projects_parts_and_finished_messages() {
        let temp = TempDir::new().unwrap();
        let provider_db = temp.path().join("opencode.sqlite3");
        let conn = create_opencode_db(&provider_db);
        let store = AgentEventStore::new(temp.path().join("events.sqlite3"));
        let metadata = metadata("opencode");
        let session_path = format!(
            "opencode://session?db={}&id=session-open",
            url::form_urlencoded::byte_serialize(provider_db.to_string_lossy().as_bytes())
                .collect::<String>()
        );
        let runtime = runtime(&metadata, AgentHarness::Opencode, session_path);
        store.observe_agent(&metadata, &runtime).unwrap();
        let after = store.latest_sequence();
        conn.execute(
            "INSERT INTO message VALUES (?1, ?2, ?3, ?4)",
            params![
                "open-turn-1",
                "session-open",
                1_776_600_000_000_i64,
                r#"{"role":"user"}"#
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message VALUES (?1, ?2, ?3, ?4)",
            params![
                "open-tool-1",
                "session-open",
                1_776_600_000_500_i64,
                r#"{"role":"assistant","parentID":"open-turn-1","finish":"tool-calls"}"#
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                "open-tool-part-1",
                "open-tool-1",
                "session-open",
                1_776_600_000_500_i64,
                r#"{"type":"text","text":"working"}"#
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message VALUES (?1, ?2, ?3, ?4)",
            params![
                "open-answer-1",
                "session-open",
                1_776_600_001_000_i64,
                r#"{"role":"assistant","parentID":"open-turn-1","finish":"stop","time":{"created":1776600001000,"completed":1776600002000}}"#
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                "open-part-1",
                "open-answer-1",
                "session-open",
                1_776_600_001_000_i64,
                r#"{"type":"text","text":"finished ✓"}"#
            ],
        )
        .unwrap();
        store.observe_agent(&metadata, &runtime).unwrap();
        let page = store.read_page(after, 100).unwrap();
        assert!(event_kinds(&page).contains(&AgentEventKind::AssistantMessage));
        assert_eq!(
            page.events
                .iter()
                .filter(|event| event.kind == AgentEventKind::TurnFinal)
                .count(),
            1
        );
        assert!(page
            .events
            .iter()
            .all(|event| event.turn_id.as_deref() == Some("open-turn-1")));
        assert_eq!(
            page.events
                .iter()
                .find(|event| event.kind == AgentEventKind::TurnFinal)
                .map(|event| event.observed_at),
            DateTime::from_timestamp_millis(1_776_600_002_000_i64)
        );
    }

    #[test]
    fn retention_returns_explicit_cursor_gap_and_lifecycle_is_durable() {
        let temp = TempDir::new().unwrap();
        let session = temp.path().join("codex.jsonl");
        fs::write(
            &session,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"session-retention\"}}\n",
        )
        .unwrap();
        let store = AgentEventStore::with_retention_limit(temp.path().join("events.sqlite3"), 3);
        let metadata = metadata("codex");
        let runtime = runtime(
            &metadata,
            AgentHarness::Codex,
            session.to_string_lossy().into(),
        );
        store.observe_agent(&metadata, &runtime).unwrap();
        append(
            &session,
            concat!(
                "{\"ordinal\":1,\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"turn-retained\"}}\n",
                "{\"ordinal\":2,\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"done\"}],\"internal_chat_message_metadata_passthrough\":{\"turn_id\":\"turn-retained\"}}}\n",
                "{\"ordinal\":3,\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",\"turn_id\":\"turn-retained\",\"last_agent_message\":\"done\"}}\n"
            ),
        );
        store.observe_agent(&metadata, &runtime).unwrap();
        let gap = store.read_page(0, 100).unwrap();
        assert_eq!(gap.status, AgentEventStatus::CursorTooOld);
        assert_eq!(gap.recovery.unwrap().kind, "catalog_snapshot");

        store
            .record_unavailable(&metadata, Utc::now(), "metadata_cleared")
            .unwrap();
        let page = store
            .read_page(store.latest_sequence().saturating_sub(1), 100)
            .unwrap();
        assert_eq!(page.events[0].kind, AgentEventKind::AgentLifecycle);
        assert_eq!(page.events[0].lifecycle.as_deref(), Some("unavailable"));
        assert_eq!(page.events[0].reason.as_deref(), Some("metadata_cleared"));
    }

    #[test]
    fn managed_codex_records_each_user_message_as_input() {
        let temp = TempDir::new().unwrap();
        let store = AgentEventStore::new(temp.path().join("events.sqlite3"));
        store.start_runtime_epoch().unwrap();
        let mut metadata = metadata("codex");
        metadata.codex_app_server = Some(crate::agent::CodexAppServerSession {
            thread_id: "thread-input".to_string(),
            session_id: "session-input".to_string(),
            executable: "codex".to_string(),
            version: "test".to_string(),
            tui_args: vec![],
        });
        let mut runtime = AgentRuntimeSnapshot::new(&metadata);
        runtime.alive = true;
        runtime.harness = AgentHarness::Codex;
        runtime.transport = AgentTransport::CodexAppServerTui;
        let user_message = |id: &str, text: &str| {
            serde_json::json!({"method": "item/started", "params": {
                "threadId": "thread-input", "turnId": "turn-1",
                "item": {"type": "userMessage", "id": id,
                    "content": [{"type": "text", "text": text, "text_elements": []}]}}})
        };
        let mut writer = store.writer().unwrap();
        for message in [
            user_message("item-1", "fix the build"),
            user_message("item-2", "also run the tests"),
            // Duplicate delivery records one event.
            user_message("item-2", "also run the tests"),
            serde_json::json!({"method": "item/started", "params": {
                "threadId": "thread-input", "turnId": "turn-1",
                "item": {"type": "agentMessage", "id": "item-3", "text": ""}}}),
        ] {
            writer
                .observe_codex_app_server_notification(&metadata, &runtime, &message)
                .unwrap();
        }
        let accepted = store
            .read_page(0, 100)
            .unwrap()
            .events
            .into_iter()
            .filter(|event| event.kind == AgentEventKind::InputAccepted)
            .map(|event| (event.input_sha256.unwrap(), event.turn_id))
            .collect::<Vec<_>>();
        assert_eq!(
            accepted,
            vec![
                (
                    crate::agent::message_sha256("fix the build"),
                    Some("turn-1".to_string())
                ),
                (
                    crate::agent::message_sha256("also run the tests"),
                    Some("turn-1".to_string())
                ),
            ]
        );
    }

    #[test]
    fn codex_policy_failure_without_assistant_text_has_safe_terminal_detail() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("events.sqlite3");
        let store = AgentEventStore::new(path.clone());
        store.start_runtime_epoch().unwrap();
        let mut metadata = metadata("codex");
        metadata.codex_app_server = Some(crate::agent::CodexAppServerSession {
            thread_id: "thread-policy".to_string(),
            session_id: "session-policy".to_string(),
            executable: "codex".to_string(),
            version: "test".to_string(),
            tui_args: vec![],
        });
        let mut runtime = AgentRuntimeSnapshot::new(&metadata);
        runtime.alive = true;
        runtime.harness = AgentHarness::Codex;
        runtime.transport = AgentTransport::CodexAppServerTui;
        let notifications: Vec<Value> =
            serde_json::from_str(include_str!("../test-data/codex-policy-failed-turn.json"))
                .unwrap();
        let mut writer = store.writer().unwrap();
        for notification in &notifications[..2] {
            writer
                .observe_codex_app_server_notification(&metadata, &runtime, notification)
                .unwrap();
        }
        assert!(!store
            .read_page(0, 100)
            .unwrap()
            .events
            .iter()
            .any(|event| event.kind == AgentEventKind::TurnFinal));
        // The terminal payload is sufficient even if its preceding error
        // notification was missed. Duplicate delivery preserves one final.
        for _ in 0..2 {
            writer
                .observe_codex_app_server_notification(&metadata, &runtime, &notifications[2])
                .unwrap();
        }
        drop(writer);
        drop(store);
        let reopened = AgentEventStore::new(path);
        let page = reopened.read_page(0, 100).unwrap();
        let finals: Vec<_> = page
            .events
            .iter()
            .filter(|event| event.kind == AgentEventKind::TurnFinal)
            .collect();
        assert_eq!(finals.len(), 1);
        let event = finals[0];
        assert_eq!(event.turn_id.as_deref(), Some("turn-policy"));
        assert_eq!(event.outcome.as_deref(), Some("aborted"));
        assert_eq!(event.reason.as_deref(), Some("policy_blocked"));
        assert_eq!(event.detail.as_deref(), Some("Codex could not complete this turn because the provider blocked the response under its content policy."));
        assert_eq!(event.text, None);
        assert_eq!(event.recoverable, None);
        assert!(!page
            .events
            .iter()
            .any(|event| event.kind == AgentEventKind::AssistantMessage));
        assert!(page
            .events
            .iter()
            .any(|event| event.sequence > finals[0].sequence
                && event.turn_state.as_deref() == Some("waiting_on_user")));
        let serialized = serde_json::to_string(&page).unwrap();
        assert!(!serialized.contains("RAW_PROVIDER_DIAGNOSTIC"));
        assert!(!serialized.contains("PRIVATE_DIAGNOSTIC_PAYLOAD"));

        let fixtures: Value =
            serde_json::from_str(include_str!("../../docs/agent-api/v1/golden-fixtures.json"))
                .unwrap();
        let golden: AgentEvent =
            serde_json::from_value(fixtures["policy_aborted_turn_final"].clone()).unwrap();
        assert_eq!(event.reason, golden.reason);
        assert_eq!(event.detail, golden.detail);
        assert_eq!(event.outcome, golden.outcome);
        assert_eq!(event.text, golden.text);

        let mut writer = reopened.writer().unwrap();
        for status in ["completed", "interrupted"] {
            let mut completion = notifications[2].clone();
            completion["params"]["turn"]["id"] = serde_json::json!(status);
            completion["params"]["turn"]["status"] = serde_json::json!(status);
            completion["params"]["turn"]["error"] = Value::Null;
            writer
                .observe_codex_app_server_notification(&metadata, &runtime, &completion)
                .unwrap();
        }
        let page = reopened.read_page(0, 100).unwrap();
        for status in ["completed", "interrupted"] {
            let event = page
                .events
                .iter()
                .find(|event| {
                    event.kind == AgentEventKind::TurnFinal
                        && event.turn_id.as_deref() == Some(status)
                })
                .unwrap();
            assert_eq!(event.reason, None);
            assert_eq!(event.detail, None);
        }
    }

    #[test]
    fn codex_capacity_failure_matches_terminal_contract() {
        let temp = TempDir::new().unwrap();
        let store = AgentEventStore::new(temp.path().join("events.sqlite3"));
        store.start_runtime_epoch().unwrap();
        let mut metadata = metadata("codex");
        metadata.codex_app_server = Some(crate::agent::CodexAppServerSession {
            thread_id: "thread-policy".to_string(),
            session_id: "session-capacity".to_string(),
            executable: "codex".to_string(),
            version: "test".to_string(),
            tui_args: vec![],
        });
        let mut runtime = AgentRuntimeSnapshot::new(&metadata);
        runtime.alive = true;
        runtime.transport = AgentTransport::CodexAppServerTui;
        let mut notifications: Vec<Value> =
            serde_json::from_str(include_str!("../test-data/codex-policy-failed-turn.json"))
                .unwrap();
        notifications[2]["params"]["turn"]["error"]["codexErrorInfo"] =
            serde_json::json!("serverOverloaded");
        store
            .writer()
            .unwrap()
            .observe_codex_app_server_notification(&metadata, &runtime, &notifications[2])
            .unwrap();
        let page = store.read_page(0, 100).unwrap();
        let event = page
            .events
            .iter()
            .find(|event| event.kind == AgentEventKind::TurnFinal)
            .unwrap();
        let fixtures: Value =
            serde_json::from_str(include_str!("../../docs/agent-api/v1/golden-fixtures.json"))
                .unwrap();
        let golden: AgentEvent =
            serde_json::from_value(fixtures["capacity_aborted_turn_final"].clone()).unwrap();
        assert_eq!(event.reason, golden.reason);
        assert_eq!(event.detail, golden.detail);
        assert_eq!(event.outcome, golden.outcome);
        assert_eq!(event.text, None);
        assert_eq!(event.recoverable, None);
        let serialized = serde_json::to_string(event).unwrap();
        assert!(!serialized.contains("RAW_PROVIDER_DIAGNOSTIC"));
        assert!(!serialized.contains("PRIVATE_DIAGNOSTIC_PAYLOAD"));
    }

    #[test]
    fn codex_terminal_failure_classifies_codes_without_exposing_payloads() {
        for (info, reason) in [
            (
                serde_json::json!("misalignmentPolicyViolation"),
                "policy_blocked",
            ),
            (serde_json::json!("contextWindowExceeded"), "context_limit"),
            (serde_json::json!("sessionBudgetExceeded"), "usage_limit"),
            (serde_json::json!("usageLimitExceeded"), "usage_limit"),
            (serde_json::json!("rateLimitExceeded"), "rate_limited"),
            (
                serde_json::json!({"httpConnectionFailed": {"httpStatusCode": 503}}),
                "provider_unavailable",
            ),
            (serde_json::json!("serverOverloaded"), "model_at_capacity"),
            (
                serde_json::json!({"responseStreamDisconnected": {"httpStatusCode": 503}}),
                "provider_unavailable",
            ),
            (
                serde_json::json!({"responseStreamConnectionFailed": {"httpStatusCode": 503}}),
                "provider_unavailable",
            ),
            (serde_json::json!("unauthorized"), "authentication_failed"),
            (serde_json::json!("badRequest"), "invalid_request"),
            (serde_json::json!("sandboxError"), "sandbox_error"),
            (serde_json::json!("futureProviderCode"), "provider_error"),
            (Value::Null, "provider_error"),
        ] {
            let error = serde_json::json!({"codexErrorInfo": info, "message": "PRIVATE", "additionalDetails": "PRIVATE"});
            let (actual, detail) = codex_terminal_failure(Some(&error));
            assert_eq!(actual, reason);
            assert!(!detail.contains("PRIVATE"));
        }
        assert_eq!(codex_terminal_failure(None).0, "provider_error");
    }

    #[test]
    fn live_event_dtos_match_the_v1_golden_pages() {
        let fixtures: Value =
            serde_json::from_str(include_str!("../../docs/agent-api/v1/golden-fixtures.json"))
                .unwrap();
        let page: AgentEventPage = serde_json::from_value(fixtures["event_page"].clone()).unwrap();
        assert_eq!(page.schema, AGENT_EVENT_SCHEMA);
        assert_eq!(page.status, AgentEventStatus::Ok);
        assert_eq!(page.events[0].kind, AgentEventKind::AgentLifecycle);
        assert_eq!(page.events[6].kind, AgentEventKind::TurnFinal);

        let gap: AgentEventPage =
            serde_json::from_value(fixtures["cursor_too_old"].clone()).unwrap();
        assert_eq!(gap.status, AgentEventStatus::CursorTooOld);
        assert_eq!(
            gap.recovery.as_ref().map(|recovery| recovery.kind.as_str()),
            Some("catalog_snapshot")
        );
    }

    #[test]
    #[ignore]
    fn bench_agent_infra_idle_event_reads() {
        let temp = TempDir::new().unwrap();
        let store = AgentEventStore::new(temp.path().join("events.sqlite3"));
        store.start_runtime_epoch().unwrap();
        for _ in 0..100 {
            store.read_page(0, 100).unwrap();
        }
        let started = std::time::Instant::now();
        for _ in 0..20_000 {
            std::hint::black_box(store.read_page(0, 100).unwrap());
        }
        eprintln!("BENCH_EVENT_READ_NS={}", started.elapsed().as_nanos());
    }
}
