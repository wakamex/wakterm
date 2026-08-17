use crate::agent::{
    refresh_runtime_from_harness, AgentHarness, AgentMetadata, AgentOrigin, AgentRuntimeSnapshot,
    AgentSnapshot, AgentStatus, AgentTurnState,
};
use crate::agent_request::{AgentRequest, AgentRequestState, AgentRequestStore};
use crate::pane::PaneId;
use crate::Mux;
use anyhow::Context;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

pub const AGENT_API_SCHEMA: &str = "wakterm.agent-api.v1";
pub const AGENT_API_MAJOR: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentApiCapabilities {
    pub schema: String,
    pub api_major: u32,
    pub capabilities: Vec<String>,
}

impl AgentApiCapabilities {
    pub fn current() -> Self {
        Self {
            schema: AGENT_API_SCHEMA.to_string(),
            api_major: AGENT_API_MAJOR,
            capabilities: vec![
                "catalog.v1".to_string(),
                "prompt_admission.v1".to_string(),
                "return_request_terminal_stream.v1".to_string(),
                "codex_output_shadow.experimental.v1".to_string(),
            ],
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentCatalogEntry {
    pub agent_id: String,
    pub incarnation_id: Option<String>,
    /// Ephemeral mux pane locator for joining a current live route. This is
    /// not a durable agent or process identity.
    pub pane_id: u64,
    pub name: String,
    pub harness: String,
    pub status: String,
    pub turn_state: String,
    pub alive: bool,
    pub observed_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentCatalog {
    pub schema: String,
    pub agents: Vec<AgentCatalogEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentPromptAdmissionRequest {
    pub request_id: String,
    pub agent_id: String,
    pub incarnation_id: String,
    pub prompt: String,
    pub paste: bool,
    pub return_final: bool,
    pub timeout_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentAdmissionStatus {
    Accepted,
    Busy,
    Unsupported,
    Unavailable,
    StaleIncarnation,
    Invalid,
    ObserverFailure,
    InternalFailure,
    Indeterminate,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentAdmissionReceipt {
    pub schema: String,
    pub request_id: String,
    pub status: AgentAdmissionStatus,
    pub definitive: bool,
    pub prompt_written: Option<bool>,
    pub agent_id: Option<String>,
    pub incarnation_id: Option<String>,
    pub return_final: bool,
    pub request: Option<AgentRequest>,
    pub detail: Option<String>,
}

impl AgentAdmissionReceipt {
    pub fn accepted(
        request: &AgentPromptAdmissionRequest,
        nested_request: Option<AgentRequest>,
    ) -> Self {
        Self {
            schema: AGENT_API_SCHEMA.to_string(),
            request_id: request.request_id.clone(),
            status: AgentAdmissionStatus::Accepted,
            definitive: true,
            prompt_written: Some(true),
            agent_id: Some(request.agent_id.clone()),
            incarnation_id: Some(request.incarnation_id.clone()),
            return_final: request.return_final,
            request: nested_request,
            detail: None,
        }
    }

    pub fn rejected(
        request: &AgentPromptAdmissionRequest,
        status: AgentAdmissionStatus,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            schema: AGENT_API_SCHEMA.to_string(),
            request_id: request.request_id.clone(),
            status,
            definitive: true,
            prompt_written: Some(false),
            agent_id: Some(request.agent_id.clone()),
            incarnation_id: Some(request.incarnation_id.clone()),
            return_final: request.return_final,
            request: None,
            detail: Some(detail.into()),
        }
    }

    pub fn indeterminate(
        request: &AgentPromptAdmissionRequest,
        nested_request: Option<AgentRequest>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            schema: AGENT_API_SCHEMA.to_string(),
            request_id: request.request_id.clone(),
            status: AgentAdmissionStatus::Indeterminate,
            definitive: false,
            prompt_written: None,
            agent_id: Some(request.agent_id.clone()),
            incarnation_id: Some(request.incarnation_id.clone()),
            return_final: request.return_final,
            request: nested_request,
            detail: Some(detail.into()),
        }
    }
}

#[derive(Clone)]
pub struct AgentAdmissionCandidate {
    pub request: AgentPromptAdmissionRequest,
    pub pane_id: PaneId,
    pub metadata: AgentMetadata,
    pub runtime: AgentRuntimeSnapshot,
    pub input_generation: u64,
}

impl AgentAdmissionCandidate {
    pub fn refresh(mut self) -> Self {
        let process_matches = self
            .metadata
            .adopted_pid
            .and_then(procinfo::LocalProcessInfo::with_root_pid)
            .is_some_and(|process| Some(process.start_time) == self.metadata.adopted_start_time);
        if !process_matches {
            self.runtime.alive = false;
            self.runtime.status = AgentStatus::Exited;
            return self;
        }
        refresh_runtime_from_harness(&mut self.runtime, &self.metadata);
        self
    }

    pub fn proposed_return_request(&self) -> Result<Option<AgentRequest>, AgentAdmissionReceipt> {
        if !self.request.return_final {
            return Ok(None);
        }
        let deadline_at = (self.request.timeout_ms != 0)
            .then(|| Utc::now() + chrono::Duration::milliseconds(self.request.timeout_ms as i64));
        AgentRequest::new(
            self.request.request_id.clone(),
            &self.metadata,
            self.pane_id,
            &self.runtime,
            &self.request.prompt,
            self.request.paste,
            self.request.timeout_ms,
            deadline_at,
        )
        .map(Some)
        .map_err(|err| {
            AgentAdmissionReceipt::rejected(
                &self.request,
                AgentAdmissionStatus::ObserverFailure,
                format!("observer could not prepare return-final admission: {err:#}"),
            )
        })
    }
}

pub enum AgentAdmissionCapture {
    Candidate(AgentAdmissionCandidate),
    Rejected(AgentAdmissionReceipt),
}

pub fn incarnation_id(metadata: &AgentMetadata) -> Option<String> {
    Some(incarnation_id_from_parts(
        &metadata.agent_id,
        metadata.adopted_pid?,
        metadata.adopted_start_time?,
    ))
}

fn incarnation_id_from_parts(agent_id: &str, pid: u32, process_start_time: u64) -> String {
    format!(
        "{:x}",
        Sha256::digest(format!("{agent_id}\0{pid}\0{process_start_time}").as_bytes())
    )
}

pub fn request_matches_admission(
    stored: &AgentRequest,
    request: &AgentPromptAdmissionRequest,
) -> bool {
    stored.matches_submission(
        &request.agent_id,
        &request.prompt,
        request.paste,
        request.timeout_ms,
    ) && incarnation_id_from_parts(
        &stored.target_agent_id,
        stored.target_pid,
        stored.target_process_start_time,
    ) == request.incarnation_id
}

impl Mux {
    pub(crate) fn agent_api_capabilities(&self) -> AgentApiCapabilities {
        AgentApiCapabilities::current()
    }

    pub(crate) fn agent_api_catalog(&self) -> AgentCatalog {
        let agents = self
            .list_agents_cached()
            .into_iter()
            .filter(|agent| matches!(agent.origin, AgentOrigin::Adopted))
            .map(catalog_entry)
            .collect();
        AgentCatalog {
            schema: AGENT_API_SCHEMA.to_string(),
            agents,
        }
    }

    pub(crate) fn capture_agent_admission(
        &self,
        request: AgentPromptAdmissionRequest,
    ) -> AgentAdmissionCapture {
        if request.request_id.trim().is_empty() || request.prompt.trim().is_empty() {
            return AgentAdmissionCapture::Rejected(AgentAdmissionReceipt::rejected(
                &request,
                AgentAdmissionStatus::Invalid,
                "request_id and prompt must be non-empty",
            ));
        }
        let target = self.list_agents_cached().into_iter().find(|agent| {
            matches!(agent.origin, AgentOrigin::Adopted)
                && agent.metadata.agent_id == request.agent_id
        });
        let Some(target) = target else {
            return AgentAdmissionCapture::Rejected(AgentAdmissionReceipt::rejected(
                &request,
                AgentAdmissionStatus::Unavailable,
                "the target agent is not available",
            ));
        };
        let Some(current_incarnation) = incarnation_id(&target.metadata) else {
            return AgentAdmissionCapture::Rejected(AgentAdmissionReceipt::rejected(
                &request,
                AgentAdmissionStatus::Unavailable,
                "the target process incarnation is not confirmed",
            ));
        };
        if current_incarnation != request.incarnation_id {
            return AgentAdmissionCapture::Rejected(AgentAdmissionReceipt::rejected(
                &request,
                AgentAdmissionStatus::StaleIncarnation,
                "the target process incarnation changed",
            ));
        }
        AgentAdmissionCapture::Candidate(AgentAdmissionCandidate {
            request,
            pane_id: target.pane_id,
            metadata: target.metadata.clone(),
            runtime: target.runtime,
            input_generation: self.agent_input_generation(target.pane_id),
        })
    }

    pub(crate) fn validate_agent_admission(
        &self,
        candidate: &AgentAdmissionCandidate,
    ) -> Option<AgentAdmissionReceipt> {
        let request = &candidate.request;
        let Some(metadata) = self.get_agent_metadata_for_pane(candidate.pane_id) else {
            return Some(AgentAdmissionReceipt::rejected(
                request,
                AgentAdmissionStatus::Unavailable,
                "the target agent is no longer available",
            ));
        };
        if metadata.agent_id != request.agent_id
            || incarnation_id(&metadata).as_deref() != Some(request.incarnation_id.as_str())
        {
            return Some(AgentAdmissionReceipt::rejected(
                request,
                AgentAdmissionStatus::StaleIncarnation,
                "the target process incarnation changed",
            ));
        }
        if self.agent_input_generation(candidate.pane_id) != candidate.input_generation {
            return Some(AgentAdmissionReceipt::rejected(
                request,
                AgentAdmissionStatus::Busy,
                "the target received input while admission was being observed",
            ));
        }
        if let Some(receipt) = classify_runtime(request, &candidate.runtime) {
            return Some(receipt);
        }
        let Some(pane) = self.get_pane(candidate.pane_id) else {
            return Some(AgentAdmissionReceipt::rejected(
                request,
                AgentAdmissionStatus::Unavailable,
                "the target pane disappeared",
            ));
        };
        if pane.is_dead() {
            return Some(AgentAdmissionReceipt::rejected(
                request,
                AgentAdmissionStatus::Unavailable,
                "the target pane exited",
            ));
        }
        if !pane.supports_atomic_prompt_submission() {
            return Some(AgentAdmissionReceipt::rejected(
                request,
                AgentAdmissionStatus::Unsupported,
                "the target pane does not support atomic prompt submission",
            ));
        }
        None
    }

    pub(crate) fn write_admitted_prompt(
        &self,
        candidate: &AgentAdmissionCandidate,
    ) -> anyhow::Result<()> {
        let pane = self
            .get_pane(candidate.pane_id)
            .with_context(|| format!("target pane {} disappeared", candidate.pane_id))?;
        pane.send_text_and_submit(&candidate.request.prompt, candidate.request.paste)?;
        self.record_agent_input(candidate.pane_id);
        Ok(())
    }

    pub(crate) fn agent_request_store(&self) -> AgentRequestStore {
        self.agent_request_store.clone()
    }

    pub(crate) fn agent_admission_store(&self) -> AgentAdmissionStore {
        self.agent_admission_store.clone()
    }
}

fn classify_runtime(
    request: &AgentPromptAdmissionRequest,
    runtime: &AgentRuntimeSnapshot,
) -> Option<AgentAdmissionReceipt> {
    if !runtime.alive || matches!(runtime.status, AgentStatus::Exited) {
        return Some(AgentAdmissionReceipt::rejected(
            request,
            AgentAdmissionStatus::Unavailable,
            "the target agent is not alive",
        ));
    }
    if let Some(error) = runtime.observer_error.as_deref() {
        return Some(AgentAdmissionReceipt::rejected(
            request,
            AgentAdmissionStatus::ObserverFailure,
            format!("the target observer failed: {error}"),
        ));
    }
    if !matches!(runtime.turn_state, AgentTurnState::WaitingOnUser) {
        return Some(AgentAdmissionReceipt::rejected(
            request,
            if matches!(runtime.turn_state, AgentTurnState::WaitingOnAgent)
                || matches!(runtime.status, AgentStatus::Busy)
            {
                AgentAdmissionStatus::Busy
            } else {
                AgentAdmissionStatus::Unavailable
            },
            "the target is not authoritatively idle",
        ));
    }
    if request.return_final && !matches!(runtime.harness, AgentHarness::Codex) {
        return Some(AgentAdmissionReceipt::rejected(
            request,
            AgentAdmissionStatus::Unsupported,
            "return-final admission currently supports only Codex",
        ));
    }
    None
}

fn catalog_entry(agent: AgentSnapshot) -> AgentCatalogEntry {
    AgentCatalogEntry {
        agent_id: agent.metadata.agent_id.clone(),
        incarnation_id: incarnation_id(&agent.metadata),
        pane_id: agent.pane_id as u64,
        name: agent.metadata.name.clone(),
        harness: harness_name(&agent.runtime.harness).to_string(),
        status: status_name(&agent.runtime.status).to_string(),
        turn_state: turn_state_name(&agent.runtime.turn_state).to_string(),
        alive: agent.runtime.alive,
        observed_at: agent.runtime.observed_at,
    }
}

fn harness_name(harness: &AgentHarness) -> &'static str {
    match harness {
        AgentHarness::Unknown => "unknown",
        AgentHarness::Claude => "claude",
        AgentHarness::Codex => "codex",
        AgentHarness::Gemini => "gemini",
        AgentHarness::Opencode => "opencode",
    }
}

fn status_name(status: &AgentStatus) -> &'static str {
    match status {
        AgentStatus::Starting => "starting",
        AgentStatus::Busy => "busy",
        AgentStatus::Idle => "idle",
        AgentStatus::Errored => "errored",
        AgentStatus::Exited => "exited",
    }
}

fn turn_state_name(state: &AgentTurnState) -> &'static str {
    match state {
        AgentTurnState::Unknown => "unknown",
        AgentTurnState::WaitingOnAgent => "waiting_on_agent",
        AgentTurnState::WaitingOnUser => "waiting_on_user",
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredOneWayAdmission {
    fingerprint: String,
    state: StoredAdmissionState,
    receipt: AgentAdmissionReceipt,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum StoredAdmissionState {
    Prepared,
    Accepted,
    Indeterminate,
}

pub enum OneWayAdmissionClaim {
    New,
    Existing(AgentAdmissionReceipt),
    Conflict,
}

#[derive(Clone)]
pub struct AgentAdmissionStore {
    path: PathBuf,
}

impl AgentAdmissionStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn connect(&self) -> anyhow::Result<Connection> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&self.path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS agent_prompt_admission (
                 request_id TEXT PRIMARY KEY,
                 fingerprint TEXT NOT NULL,
                 record_json TEXT NOT NULL
             );",
        )?;
        Ok(conn)
    }

    pub fn claim(
        &self,
        request: &AgentPromptAdmissionRequest,
    ) -> anyhow::Result<OneWayAdmissionClaim> {
        let conn = self.connect()?;
        let fingerprint = admission_fingerprint(request);
        let stored = StoredOneWayAdmission {
            fingerprint: fingerprint.clone(),
            state: StoredAdmissionState::Prepared,
            receipt: AgentAdmissionReceipt::indeterminate(
                request,
                None,
                "prompt admission is prepared but not complete",
            ),
        };
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO agent_prompt_admission(request_id, fingerprint, record_json)
             VALUES (?1, ?2, ?3)",
            params![
                request.request_id,
                fingerprint,
                serde_json::to_string(&stored)?
            ],
        )?;
        if inserted == 1 {
            return Ok(OneWayAdmissionClaim::New);
        }
        let existing = conn
            .query_row(
                "SELECT fingerprint, record_json FROM agent_prompt_admission WHERE request_id = ?1",
                params![request.request_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        if let Some((stored_fingerprint, json)) = existing {
            if stored_fingerprint != fingerprint {
                return Ok(OneWayAdmissionClaim::Conflict);
            }
            let mut stored: StoredOneWayAdmission = serde_json::from_str(&json)?;
            if matches!(stored.state, StoredAdmissionState::Prepared) {
                stored.state = StoredAdmissionState::Indeterminate;
                stored.receipt = AgentAdmissionReceipt::indeterminate(
                    request,
                    None,
                    "a prior admission attempt did not durably record whether prompt delivery occurred",
                );
                conn.execute(
                    "UPDATE agent_prompt_admission SET record_json = ?2 WHERE request_id = ?1",
                    params![request.request_id, serde_json::to_string(&stored)?],
                )?;
            }
            return Ok(OneWayAdmissionClaim::Existing(stored.receipt));
        }
        anyhow::bail!(
            "admission request id {} disappeared during registration",
            request.request_id
        )
    }

    pub fn finish(&self, receipt: &AgentAdmissionReceipt) -> anyhow::Result<()> {
        let conn = self.connect()?;
        let state = if matches!(receipt.status, AgentAdmissionStatus::Accepted) {
            StoredAdmissionState::Accepted
        } else {
            StoredAdmissionState::Indeterminate
        };
        let fingerprint: String = conn.query_row(
            "SELECT fingerprint FROM agent_prompt_admission WHERE request_id = ?1",
            params![receipt.request_id],
            |row| row.get(0),
        )?;
        let stored = StoredOneWayAdmission {
            fingerprint,
            state,
            receipt: receipt.clone(),
        };
        conn.execute(
            "UPDATE agent_prompt_admission SET record_json = ?2 WHERE request_id = ?1",
            params![receipt.request_id, serde_json::to_string(&stored)?],
        )?;
        Ok(())
    }

    pub fn release_unwritten(&self, request_id: &str) -> anyhow::Result<()> {
        let conn = self.connect()?;
        let record = conn
            .query_row(
                "SELECT record_json FROM agent_prompt_admission WHERE request_id = ?1",
                params![request_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(record) = record {
            let stored: StoredOneWayAdmission = serde_json::from_str(&record)?;
            if matches!(stored.state, StoredAdmissionState::Prepared) {
                conn.execute(
                    "DELETE FROM agent_prompt_admission WHERE request_id = ?1 AND record_json = ?2",
                    params![request_id, record],
                )?;
            }
        }
        Ok(())
    }
}

fn admission_fingerprint(request: &AgentPromptAdmissionRequest) -> String {
    let mut digest = Sha256::new();
    digest.update(AGENT_API_SCHEMA.as_bytes());
    digest.update([0]);
    digest.update(request.agent_id.as_bytes());
    digest.update([0]);
    digest.update(request.incarnation_id.as_bytes());
    digest.update([0]);
    digest.update(request.prompt.as_bytes());
    digest.update([request.paste as u8, request.return_final as u8]);
    digest.update(request.timeout_ms.to_le_bytes());
    format!("{:x}", digest.finalize())
}

pub fn reconcile_written_request_after_failure(
    mut request: AgentRequest,
    detail: &str,
) -> AgentRequest {
    request.finish(AgentRequestState::Indeterminate, Utc::now(), detail);
    request
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::collections::HashSet;
    use tempfile::tempdir;

    fn request(id: &str, prompt: &str) -> AgentPromptAdmissionRequest {
        AgentPromptAdmissionRequest {
            request_id: id.to_string(),
            agent_id: "agent-1".to_string(),
            incarnation_id: "incarnation-1".to_string(),
            prompt: prompt.to_string(),
            paste: true,
            return_final: false,
            timeout_ms: 0,
        }
    }

    fn metadata() -> AgentMetadata {
        AgentMetadata {
            agent_id: "agent-1".to_string(),
            name: "target".to_string(),
            launch_cmd: "codex".to_string(),
            declared_cwd: "/code/target".to_string(),
            adopted_pid: Some(10),
            adopted_start_time: Some(20),
            created_at: Utc::now(),
            repo_root: None,
            worktree: None,
            branch: None,
            managed_checkout: false,
        }
    }

    fn runtime() -> AgentRuntimeSnapshot {
        let metadata = metadata();
        let mut runtime = AgentRuntimeSnapshot::new(&metadata);
        runtime.harness = AgentHarness::Codex;
        runtime.alive = true;
        runtime.status = AgentStatus::Idle;
        runtime.turn_state = AgentTurnState::WaitingOnUser;
        runtime
    }

    #[test]
    fn busy_is_definitive_only_before_prompt_write() {
        let request = request("request-1", "work");
        let mut runtime = runtime();
        runtime.status = AgentStatus::Busy;
        runtime.turn_state = AgentTurnState::WaitingOnAgent;
        let receipt = classify_runtime(&request, &runtime).unwrap();
        assert!(receipt.definitive);
        assert_eq!(receipt.prompt_written, Some(false));
        assert_eq!(receipt.status, AgentAdmissionStatus::Busy);
    }

    #[test]
    fn missing_exact_agent_is_definitively_unavailable_without_prompt_write() {
        let mux = Mux::new(None);
        let request = request("request-missing", "work");
        let AgentAdmissionCapture::Rejected(receipt) = mux.capture_agent_admission(request) else {
            panic!("expected unavailable admission receipt");
        };
        assert_eq!(receipt.status, AgentAdmissionStatus::Unavailable);
        assert!(receipt.definitive);
        assert_eq!(receipt.prompt_written, Some(false));
    }

    #[test]
    fn missing_baseline_cursor_is_definitive_observer_failure_without_prompt_write() {
        let mut request = request("request-no-cursor", "work");
        request.return_final = true;
        let mut runtime = runtime();
        runtime.transport = crate::agent::AgentTransport::ObservedPty;
        runtime.session_path = Some("/tmp/codex-session.jsonl".to_string());
        runtime.observed_turn = Some(crate::agent::AgentObservedTurn {
            provider_turn_id: "turn-1".to_string(),
            outcome: crate::agent::AgentObservedTurnOutcome::Completed,
            started_at: None,
            completed_at: Some(Utc::now()),
            started_cursor: Some(1),
            latest_cursor: None,
            primary_user_message_sha256: None,
            user_message_count: 1,
            final_message: Some("done".to_string()),
        });
        let candidate = AgentAdmissionCandidate {
            request,
            pane_id: 7,
            metadata: metadata(),
            runtime,
            input_generation: 0,
        };

        let Err(receipt) = candidate.proposed_return_request() else {
            panic!("expected observer failure receipt");
        };
        assert_eq!(receipt.status, AgentAdmissionStatus::ObserverFailure);
        assert!(receipt.definitive);
        assert_eq!(receipt.prompt_written, Some(false));
        assert!(receipt
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("observer cursor for the baseline turn")));
    }

    #[test]
    fn capability_probe_does_not_claim_a_durable_general_event_stream() {
        let capabilities = AgentApiCapabilities::current();
        assert!(capabilities
            .capabilities
            .contains(&"prompt_admission.v1".to_string()));
        assert!(!capabilities
            .capabilities
            .iter()
            .any(|capability| capability.starts_with("event_stream.")));
    }

    #[test]
    fn one_way_store_is_idempotent_and_detects_conflicts() {
        let dir = tempdir().unwrap();
        let store = AgentAdmissionStore::new(dir.path().join("agent.sqlite3"));
        let request = request("request-1", "work");
        assert!(matches!(
            store.claim(&request).unwrap(),
            OneWayAdmissionClaim::New
        ));
        let accepted = AgentAdmissionReceipt::accepted(&request, None);
        store.finish(&accepted).unwrap();
        assert!(matches!(
            store.claim(&request).unwrap(),
            OneWayAdmissionClaim::Existing(receipt) if receipt == accepted
        ));
        assert!(matches!(
            store
                .claim(&self::request("request-1", "different"))
                .unwrap(),
            OneWayAdmissionClaim::Conflict
        ));
    }

    #[test]
    fn unfinished_one_way_claim_becomes_indeterminate_on_replay() {
        let dir = tempdir().unwrap();
        let store = AgentAdmissionStore::new(dir.path().join("agent.sqlite3"));
        let request = request("request-1", "work");
        assert!(matches!(
            store.claim(&request).unwrap(),
            OneWayAdmissionClaim::New
        ));

        let OneWayAdmissionClaim::Existing(receipt) = store.claim(&request).unwrap() else {
            panic!("expected existing receipt");
        };
        assert_eq!(receipt.status, AgentAdmissionStatus::Indeterminate);
        assert!(!receipt.definitive);
        assert_eq!(receipt.prompt_written, None);
    }

    #[test]
    fn wakterm_agent_api_golden_contract_is_self_consistent() {
        let fixtures: Value =
            serde_json::from_str(include_str!("../../docs/agent-api/v1/golden-fixtures.json"))
                .unwrap();
        assert_eq!(fixtures["fixture_schema"], "wakterm.agent-api-golden.v1");

        let current: AgentApiCapabilities =
            serde_json::from_value(fixtures["current_capabilities"].clone()).unwrap();
        assert_eq!(current, AgentApiCapabilities::current());
        assert!(!current
            .capabilities
            .iter()
            .any(|capability| capability == "event_stream.v1"));
        assert_eq!(
            fixtures["event_stream_capabilities"]["availability"],
            "fixture_only"
        );
        assert!(fixtures["event_stream_capabilities"]["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|capability| capability == "event_stream.v1"));

        let catalog: AgentCatalog = serde_json::from_value(fixtures["catalog"].clone()).unwrap();
        assert_eq!(catalog.agents.len(), 2);
        assert_eq!(catalog.agents[0].pane_id, 9);
        assert_eq!(catalog.agents[1].pane_id, 12);
        assert_ne!(catalog.agents[0].agent_id, catalog.agents[1].agent_id);
        assert_ne!(
            catalog.agents[0].incarnation_id,
            catalog.agents[1].incarnation_id
        );

        let receipts = fixtures["admission_receipts"].as_object().unwrap();
        for (name, fixture) in receipts {
            let receipt: AgentAdmissionReceipt = serde_json::from_value(fixture.clone()).unwrap();
            match name.as_str() {
                "accepted" => {
                    assert_eq!(receipt.status, AgentAdmissionStatus::Accepted);
                    assert_eq!(receipt.prompt_written, Some(true));
                }
                "busy" => {
                    assert_eq!(receipt.status, AgentAdmissionStatus::Busy);
                    assert!(receipt.definitive);
                    assert_eq!(receipt.prompt_written, Some(false));
                }
                "unavailable" => {
                    assert_eq!(receipt.status, AgentAdmissionStatus::Unavailable);
                    assert!(receipt.definitive);
                    assert_eq!(receipt.prompt_written, Some(false));
                }
                "observer_failure" => {
                    assert_eq!(receipt.status, AgentAdmissionStatus::ObserverFailure);
                    assert!(receipt.definitive);
                    assert_eq!(receipt.prompt_written, Some(false));
                }
                "indeterminate" => {
                    assert_eq!(receipt.status, AgentAdmissionStatus::Indeterminate);
                    assert!(!receipt.definitive);
                    assert_eq!(receipt.prompt_written, None);
                }
                other => panic!("unexpected receipt fixture {}", other),
            }
        }

        let page = &fixtures["event_page"];
        assert_eq!(page["availability"], "fixture_only");
        let events = page["events"].as_array().unwrap();
        let sequences = events
            .iter()
            .map(|event| event["sequence"].as_u64().unwrap())
            .collect::<Vec<_>>();
        assert!(sequences.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(sequences
            .iter()
            .all(|sequence| *sequence > page["requested_after_sequence"].as_u64().unwrap()));
        assert_eq!(
            sequences.last().copied(),
            page["next_after_sequence"].as_u64()
        );
        let kinds = events
            .iter()
            .map(|event| event["kind"].as_str().unwrap())
            .collect::<HashSet<_>>();
        for required in [
            "agent_lifecycle",
            "turn_started",
            "turn_state_changed",
            "plan",
            "assistant_message",
            "observer_failure",
            "turn_final",
        ] {
            assert!(kinds.contains(required), "missing event kind {}", required);
        }
        for event in events {
            assert!(event["agent_id"].is_string());
            assert!(event["incarnation_id"].is_string());
            if event["kind"] != "agent_lifecycle" {
                assert!(event["turn_id"].is_string());
            }
        }

        let catalog_sequence = fixtures["catalog"]["as_of_event_sequence"]
            .as_u64()
            .unwrap();
        assert!(catalog_sequence < sequences[0]);
        let lifecycle = &fixtures["lifecycle_page"];
        assert_eq!(lifecycle["events"][0]["kind"], "agent_lifecycle");
        assert_eq!(
            lifecycle["events"][0]["sequence"],
            lifecycle["next_after_sequence"]
        );

        let gap = &fixtures["cursor_too_old"];
        assert!(
            gap["requested_after_sequence"].as_u64().unwrap()
                < gap["oldest_available_sequence"].as_u64().unwrap()
        );
        assert_eq!(gap["recovery"]["kind"], "catalog_snapshot");
        assert_eq!(fixtures["retention"]["gap_behavior"], "cursor_too_old");
        assert_eq!(
            fixtures["retention"]["durable_consumer_cursor_protection"],
            "not_advertised"
        );

        let error_codes = fixtures["classified_errors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|error| error["code"].as_str().unwrap())
            .collect::<HashSet<_>>();
        for required in [
            "unsupported",
            "unavailable",
            "stale_incarnation",
            "busy",
            "invalid",
            "observer_failure",
            "internal_failure",
            "indeterminate",
            "cursor_too_old",
            "incompatible_major",
            "unknown_event_kind",
        ] {
            assert!(
                error_codes.contains(required),
                "missing error class {}",
                required
            );
        }
    }
}
