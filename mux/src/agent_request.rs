use crate::agent::{
    AgentHarness, AgentMetadata, AgentObservedTurn, AgentObservedTurnOutcome, AgentRuntimeSnapshot,
    AgentTransport, AgentTurnState,
};
use crate::pane::PaneId;
use anyhow::{bail, Context};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentRequestState {
    Registered,
    Submitted,
    Bound,
    Completed,
    Aborted,
    TimedOut,
    Cancelled,
    DeliveryFailed,
    Indeterminate,
}

impl AgentRequestState {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed
                | Self::Aborted
                | Self::TimedOut
                | Self::Cancelled
                | Self::DeliveryFailed
                | Self::Indeterminate
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentRequest {
    pub request_id: String,
    pub target_agent_id: String,
    pub target_agent_name: String,
    pub target_pane_id: PaneId,
    pub target_harness: AgentHarness,
    pub target_pid: u32,
    pub target_process_start_time: u64,
    pub target_session_path: String,
    pub baseline_provider_turn_id: String,
    pub baseline_cursor: u64,
    pub baseline_user_message_count: u32,
    pub prompt_sha256: String,
    pub submission_paste: bool,
    pub timeout_ms: u64,
    pub state: AgentRequestState,
    pub provider_turn_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deadline_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub final_message: Option<String>,
    pub detail: Option<String>,
    pub terminal_event_sequence: Option<u64>,
}

impl AgentRequest {
    pub fn new(
        request_id: String,
        metadata: &AgentMetadata,
        pane_id: PaneId,
        runtime: &AgentRuntimeSnapshot,
        prompt: &str,
        submission_paste: bool,
        timeout_ms: u64,
        deadline_at: Option<DateTime<Utc>>,
    ) -> anyhow::Result<Self> {
        validate_baseline(metadata, runtime)?;
        let turn = runtime
            .observed_turn
            .as_ref()
            .expect("baseline was validated");
        let now = Utc::now();
        Ok(Self {
            request_id,
            target_agent_id: metadata.agent_id.clone(),
            target_agent_name: metadata.name.clone(),
            target_pane_id: pane_id,
            target_harness: runtime.harness.clone(),
            target_pid: metadata.adopted_pid.expect("baseline was validated"),
            target_process_start_time: metadata.adopted_start_time.expect("baseline was validated"),
            target_session_path: runtime
                .session_path
                .clone()
                .expect("baseline was validated"),
            baseline_provider_turn_id: turn.provider_turn_id.clone(),
            baseline_cursor: turn.latest_cursor.expect("baseline was validated"),
            baseline_user_message_count: turn.user_message_count,
            prompt_sha256: prompt_sha256(prompt),
            submission_paste,
            timeout_ms,
            state: AgentRequestState::Registered,
            provider_turn_id: None,
            created_at: now,
            updated_at: now,
            deadline_at,
            completed_at: None,
            final_message: None,
            detail: None,
            terminal_event_sequence: None,
        })
    }

    pub fn mark_submitted(&mut self) {
        self.state = AgentRequestState::Submitted;
        self.updated_at = Utc::now();
    }

    pub fn reconcile(
        &mut self,
        metadata: Option<&AgentMetadata>,
        runtime: Option<&AgentRuntimeSnapshot>,
        now: DateTime<Utc>,
    ) {
        if self.state.is_terminal() {
            return;
        }
        if matches!(self.state, AgentRequestState::Registered) {
            self.finish(
                AgentRequestState::Indeterminate,
                now,
                "mux restarted before prompt submission was durably confirmed",
            );
            return;
        }
        if self.deadline_at.is_some_and(|deadline| now >= deadline) {
            self.finish(
                AgentRequestState::TimedOut,
                now,
                "final response did not arrive before the request deadline",
            );
            return;
        }
        let Some(metadata) = metadata else {
            return;
        };
        if metadata.agent_id != self.target_agent_id
            || metadata.adopted_pid != Some(self.target_pid)
            || metadata.adopted_start_time != Some(self.target_process_start_time)
        {
            self.finish(
                AgentRequestState::Indeterminate,
                now,
                "target process incarnation changed",
            );
            return;
        }
        let Some(runtime) = runtime else {
            return;
        };
        if !runtime.alive {
            self.finish(
                AgentRequestState::Indeterminate,
                now,
                "target agent exited before the correlated turn completed",
            );
            return;
        }
        let Some(session_path) = runtime.session_path.as_deref() else {
            return;
        };
        if session_path != self.target_session_path {
            self.finish(
                AgentRequestState::Indeterminate,
                now,
                "observer session changed after prompt submission",
            );
            return;
        }
        let Some(turn) = runtime.observed_turn.as_ref() else {
            return;
        };
        self.reconcile_turn(turn, now);
    }

    fn reconcile_turn(&mut self, turn: &AgentObservedTurn, now: DateTime<Utc>) {
        if turn.provider_turn_id == self.baseline_provider_turn_id {
            if turn.latest_cursor.unwrap_or(self.baseline_cursor) > self.baseline_cursor
                && turn.user_message_count > self.baseline_user_message_count
            {
                self.finish(
                    AgentRequestState::Indeterminate,
                    now,
                    "prompt was attached to the baseline provider turn",
                );
            }
            return;
        }

        if let Some(bound) = self.provider_turn_id.as_deref() {
            if turn.provider_turn_id != bound {
                self.finish(
                    AgentRequestState::Indeterminate,
                    now,
                    "agent advanced beyond the correlated provider turn",
                );
                return;
            }
        } else {
            if turn
                .started_cursor
                .is_none_or(|cursor| cursor <= self.baseline_cursor)
            {
                self.finish(
                    AgentRequestState::Indeterminate,
                    now,
                    "new provider turn did not start after the armed output cursor",
                );
                return;
            }
            let Some(primary_prompt_sha256) = turn.primary_user_message_sha256.as_deref() else {
                if matches!(turn.outcome, AgentObservedTurnOutcome::Running) {
                    return;
                }
                self.finish(
                    AgentRequestState::Indeterminate,
                    now,
                    "provider turn ended without observable prompt identity",
                );
                return;
            };
            if primary_prompt_sha256 != self.prompt_sha256 {
                self.finish(
                    AgentRequestState::Indeterminate,
                    now,
                    "a different prompt started the next provider turn",
                );
                return;
            }
            self.provider_turn_id = Some(turn.provider_turn_id.clone());
            self.state = AgentRequestState::Bound;
            self.updated_at = now;
        }

        if turn.user_message_count != 1 {
            self.finish(
                AgentRequestState::Indeterminate,
                now,
                "the correlated provider turn received additional user input",
            );
            return;
        }
        match turn.outcome {
            AgentObservedTurnOutcome::Running => {}
            AgentObservedTurnOutcome::Aborted => self.finish(
                AgentRequestState::Aborted,
                turn.completed_at.unwrap_or(now),
                "the correlated provider turn was aborted",
            ),
            AgentObservedTurnOutcome::Completed => {
                if let Some(message) = turn.final_message.clone() {
                    self.state = AgentRequestState::Completed;
                    self.completed_at = turn.completed_at.or(Some(now));
                    self.final_message = Some(message);
                    self.detail = None;
                    self.updated_at = now;
                } else {
                    self.finish(
                        AgentRequestState::Indeterminate,
                        now,
                        "provider completed the correlated turn without a final assistant message",
                    );
                }
            }
        }
    }

    pub fn finish(&mut self, state: AgentRequestState, now: DateTime<Utc>, detail: &str) {
        debug_assert!(state.is_terminal());
        self.state = state;
        self.completed_at = Some(now);
        self.updated_at = now;
        self.detail = Some(detail.to_string());
    }
}

pub fn prompt_sha256(prompt: &str) -> String {
    format!("{:x}", Sha256::digest(prompt.trim().as_bytes()))
}

fn validate_baseline(
    metadata: &AgentMetadata,
    runtime: &AgentRuntimeSnapshot,
) -> anyhow::Result<()> {
    if !matches!(runtime.harness, AgentHarness::Codex) {
        bail!("--return-final currently requires a codex agent");
    }
    if !matches!(runtime.transport, AgentTransport::ObservedPty) {
        bail!("--return-final requires an observer-backed session");
    }
    if !matches!(runtime.turn_state, AgentTurnState::WaitingOnUser) {
        bail!("--return-final requires an idle agent");
    }
    if metadata.adopted_pid.is_none() || metadata.adopted_start_time.is_none() {
        bail!("--return-final requires a confirmed target process incarnation");
    }
    if runtime.session_path.is_none() {
        bail!("--return-final requires an exact observer session");
    }
    let turn = runtime
        .observed_turn
        .as_ref()
        .context("--return-final requires stable provider turn identity")?;
    if !matches!(turn.outcome, AgentObservedTurnOutcome::Completed) {
        bail!("--return-final requires a completed baseline turn");
    }
    if turn.latest_cursor.is_none() {
        bail!("--return-final requires an observer cursor for the baseline turn");
    }
    Ok(())
}

pub struct AgentRequestStore {
    path: PathBuf,
}

impl AgentRequestStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn connect(&self) -> anyhow::Result<Connection> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let conn = Connection::open(&self.path)
            .with_context(|| format!("opening {}", self.path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS agent_request (
                 request_id TEXT PRIMARY KEY,
                 fingerprint TEXT NOT NULL,
                 snapshot_json TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS agent_request_event (
                 sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                 request_id TEXT NOT NULL UNIQUE REFERENCES agent_request(request_id)
             );",
        )?;
        Ok(conn)
    }

    pub fn create(&self, request: &AgentRequest) -> anyhow::Result<(AgentRequest, bool)> {
        let conn = self.connect()?;
        let fingerprint = request_fingerprint(request);
        if let Some((existing_fingerprint, snapshot)) = conn
            .query_row(
                "SELECT fingerprint, snapshot_json FROM agent_request WHERE request_id = ?1",
                params![request.request_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
        {
            if existing_fingerprint != fingerprint {
                bail!(
                    "request id {} was already used for different input",
                    request.request_id
                );
            }
            return Ok((
                serde_json::from_str(&snapshot).context("decoding stored agent request")?,
                false,
            ));
        }
        conn.execute(
            "INSERT INTO agent_request(request_id, fingerprint, snapshot_json) VALUES (?1, ?2, ?3)",
            params![
                request.request_id,
                fingerprint,
                serde_json::to_string(request)?
            ],
        )?;
        Ok((request.clone(), true))
    }

    pub fn save(&self, request: &mut AgentRequest) -> anyhow::Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        if request.state.is_terminal() && request.terminal_event_sequence.is_none() {
            tx.execute(
                "INSERT OR IGNORE INTO agent_request_event(request_id) VALUES (?1)",
                params![request.request_id],
            )?;
            request.terminal_event_sequence = Some(event_sequence(&tx, &request.request_id)?);
        }
        tx.execute(
            "UPDATE agent_request SET snapshot_json = ?2 WHERE request_id = ?1",
            params![request.request_id, serde_json::to_string(request)?],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn get(&self, request_id: &str) -> anyhow::Result<Option<AgentRequest>> {
        let conn = self.connect()?;
        let snapshot = conn
            .query_row(
                "SELECT snapshot_json FROM agent_request WHERE request_id = ?1",
                params![request_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        snapshot
            .map(|snapshot| {
                serde_json::from_str(&snapshot).context("decoding stored agent request")
            })
            .transpose()
    }

    pub fn active(&self) -> anyhow::Result<Vec<AgentRequest>> {
        let conn = self.connect()?;
        let mut statement = conn.prepare(
            "SELECT snapshot_json FROM agent_request WHERE request_id NOT IN
             (SELECT request_id FROM agent_request_event)",
        )?;
        let snapshots = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        snapshots
            .into_iter()
            .map(|snapshot| {
                serde_json::from_str(&snapshot).context("decoding stored agent request")
            })
            .collect()
    }

    pub fn events_after(&self, after: u64, limit: usize) -> anyhow::Result<Vec<AgentRequest>> {
        let conn = self.connect()?;
        let mut statement = conn.prepare(
            "SELECT r.snapshot_json
             FROM agent_request_event e
             JOIN agent_request r ON r.request_id = e.request_id
             WHERE e.sequence > ?1 ORDER BY e.sequence LIMIT ?2",
        )?;
        let snapshots = statement
            .query_map(params![after, limit as u64], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        snapshots
            .into_iter()
            .map(|snapshot| {
                serde_json::from_str(&snapshot).context("decoding stored agent request")
            })
            .collect()
    }
}

fn event_sequence(tx: &Transaction<'_>, request_id: &str) -> anyhow::Result<u64> {
    Ok(tx.query_row(
        "SELECT sequence FROM agent_request_event WHERE request_id = ?1",
        params![request_id],
        |row| row.get(0),
    )?)
}

fn request_fingerprint(request: &AgentRequest) -> String {
    prompt_sha256(&format!(
        "{}\0{}\0{}\0{}\0{}\0{}",
        request.target_agent_id,
        request.target_session_path,
        request.baseline_cursor,
        request.prompt_sha256,
        request.submission_paste,
        request.timeout_ms
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use tempfile::tempdir;

    fn metadata() -> AgentMetadata {
        AgentMetadata {
            agent_id: "agent-1".to_string(),
            name: "zola".to_string(),
            launch_cmd: "codex".to_string(),
            declared_cwd: "/code/zola".to_string(),
            adopted_pid: Some(42),
            adopted_start_time: Some(99),
            created_at: Utc::now(),
            repo_root: None,
            worktree: None,
            branch: None,
            managed_checkout: false,
        }
    }

    fn turn(
        id: &str,
        cursor: u64,
        prompt: Option<&str>,
        outcome: AgentObservedTurnOutcome,
    ) -> AgentObservedTurn {
        AgentObservedTurn {
            provider_turn_id: id.to_string(),
            outcome,
            started_at: Some(Utc::now()),
            completed_at: None,
            started_cursor: Some(cursor),
            latest_cursor: Some(cursor),
            primary_user_message_sha256: prompt.map(prompt_sha256),
            user_message_count: 1,
            final_message: None,
        }
    }

    fn runtime() -> AgentRuntimeSnapshot {
        let metadata = metadata();
        let mut runtime = AgentRuntimeSnapshot::new(&metadata);
        runtime.harness = AgentHarness::Codex;
        runtime.transport = AgentTransport::ObservedPty;
        runtime.turn_state = AgentTurnState::WaitingOnUser;
        runtime.session_path = Some("/sessions/exact.jsonl".to_string());
        runtime.observed_turn = Some(AgentObservedTurn {
            completed_at: Some(Utc::now()),
            final_message: Some("old".to_string()),
            ..turn(
                "baseline",
                10,
                Some("old"),
                AgentObservedTurnOutcome::Completed,
            )
        });
        runtime
    }

    #[test]
    fn binds_only_matching_prompt_after_armed_cursor() {
        let metadata = metadata();
        let runtime = runtime();
        let mut request = AgentRequest::new(
            "request-1".to_string(),
            &metadata,
            7,
            &runtime,
            "do work",
            true,
            0,
            None,
        )
        .unwrap();
        request.mark_submitted();
        let mut next = turn(
            "turn-2",
            11,
            Some("do work"),
            AgentObservedTurnOutcome::Running,
        );
        let mut observed = runtime.clone();
        observed.observed_turn = Some(next.clone());
        request.reconcile(Some(&metadata), Some(&observed), Utc::now());
        assert_eq!(request.state, AgentRequestState::Bound);

        next.outcome = AgentObservedTurnOutcome::Completed;
        next.completed_at = Some(Utc::now());
        next.latest_cursor = Some(14);
        next.final_message = Some("done".to_string());
        observed.observed_turn = Some(next);
        request.reconcile(Some(&metadata), Some(&observed), Utc::now());
        assert_eq!(request.state, AgentRequestState::Completed);
        assert_eq!(request.final_message.as_deref(), Some("done"));
    }

    #[test]
    fn unrelated_next_prompt_is_terminal_indeterminate() {
        let metadata = metadata();
        let runtime = runtime();
        let mut request = AgentRequest::new(
            "request-1".to_string(),
            &metadata,
            7,
            &runtime,
            "expected",
            true,
            0,
            None,
        )
        .unwrap();
        request.mark_submitted();
        let mut observed = runtime.clone();
        observed.observed_turn = Some(turn(
            "turn-2",
            11,
            Some("unrelated"),
            AgentObservedTurnOutcome::Completed,
        ));
        request.reconcile(Some(&metadata), Some(&observed), Utc::now());
        assert_eq!(request.state, AgentRequestState::Indeterminate);
        assert!(request.final_message.is_none());
    }

    #[test]
    fn waits_for_new_turn_prompt_before_deciding_correlation() {
        let metadata = metadata();
        let runtime = runtime();
        let mut request = AgentRequest::new(
            "request-1".to_string(),
            &metadata,
            7,
            &runtime,
            "expected",
            true,
            0,
            None,
        )
        .unwrap();
        request.mark_submitted();

        let mut next = turn("turn-2", 11, None, AgentObservedTurnOutcome::Running);
        let mut observed = runtime.clone();
        observed.observed_turn = Some(next.clone());
        request.reconcile(Some(&metadata), Some(&observed), Utc::now());
        assert_eq!(request.state, AgentRequestState::Submitted);
        assert!(request.provider_turn_id.is_none());

        next.primary_user_message_sha256 = Some(prompt_sha256("expected"));
        observed.observed_turn = Some(next);
        request.reconcile(Some(&metadata), Some(&observed), Utc::now());
        assert_eq!(request.state, AgentRequestState::Bound);
        assert_eq!(request.provider_turn_id.as_deref(), Some("turn-2"));
    }

    #[test]
    fn terminal_turn_without_prompt_identity_is_indeterminate() {
        let metadata = metadata();
        let runtime = runtime();
        let mut request = AgentRequest::new(
            "request-1".to_string(),
            &metadata,
            7,
            &runtime,
            "expected",
            true,
            0,
            None,
        )
        .unwrap();
        request.mark_submitted();

        let mut observed = runtime.clone();
        observed.observed_turn = Some(turn(
            "turn-2",
            11,
            None,
            AgentObservedTurnOutcome::Completed,
        ));
        request.reconcile(Some(&metadata), Some(&observed), Utc::now());

        assert_eq!(request.state, AgentRequestState::Indeterminate);
        assert_eq!(
            request.detail.as_deref(),
            Some("provider turn ended without observable prompt identity")
        );
    }

    #[test]
    fn session_or_process_reuse_cannot_complete_request() {
        let metadata = metadata();
        let runtime = runtime();
        let mut request = AgentRequest::new(
            "request-1".to_string(),
            &metadata,
            7,
            &runtime,
            "expected",
            true,
            0,
            None,
        )
        .unwrap();
        request.mark_submitted();
        let mut reused = metadata.clone();
        reused.adopted_start_time = Some(100);
        request.reconcile(Some(&reused), Some(&runtime), Utc::now());
        assert_eq!(request.state, AgentRequestState::Indeterminate);
    }

    #[test]
    fn restored_request_waits_for_metadata_and_observer_to_reappear() {
        let metadata = metadata();
        let runtime = runtime();
        let mut request = AgentRequest::new(
            "request-1".to_string(),
            &metadata,
            7,
            &runtime,
            "expected",
            true,
            0,
            None,
        )
        .unwrap();
        request.mark_submitted();
        request.reconcile(None, None, Utc::now());
        assert_eq!(request.state, AgentRequestState::Submitted);

        let mut restoring = runtime;
        restoring.session_path = None;
        restoring.observed_turn = None;
        request.reconcile(Some(&metadata), Some(&restoring), Utc::now());
        assert_eq!(request.state, AgentRequestState::Submitted);
    }

    #[test]
    fn store_persists_and_sequences_terminal_results_idempotently() {
        let dir = tempdir().unwrap();
        let store = AgentRequestStore::new(dir.path().join("requests.sqlite3"));
        let mut request = AgentRequest::new(
            "request-1".to_string(),
            &metadata(),
            7,
            &runtime(),
            "do work",
            true,
            300_000,
            Some(Utc::now() + Duration::minutes(5)),
        )
        .unwrap();
        assert!(store.create(&request).unwrap().1);
        assert!(!store.create(&request).unwrap().1);
        request.mark_submitted();
        store.save(&mut request).unwrap();
        request.finish(AgentRequestState::Cancelled, Utc::now(), "cancelled");
        store.save(&mut request).unwrap();
        let sequence = request.terminal_event_sequence.unwrap();
        store.save(&mut request).unwrap();
        assert_eq!(request.terminal_event_sequence, Some(sequence));
        assert_eq!(store.events_after(0, 10).unwrap(), vec![request.clone()]);
        assert!(store.events_after(sequence, 10).unwrap().is_empty());
        assert_eq!(store.get("request-1").unwrap(), Some(request));
    }
}
