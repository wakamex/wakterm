use crate::agent::{
    codex_complete_tail_offset, read_codex_output_messages, AgentHarness, AgentOrigin,
    AgentSnapshot,
};
use crate::agent_admission::{
    AgentAdmissionCandidate, AgentAdmissionCapture, AgentAdmissionReceipt, AgentAdmissionStore,
    AgentApiCapabilities, AgentCatalog, AgentPromptAdmissionRequest,
};
use crate::agent_request::{AgentRequest, AgentRequestStore};
use crate::Mux;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub const AGENT_OUTPUT_SCHEMA: &str = "wakterm.agent-output-shadow.experimental.v1";
const CURSOR_VERSION: u8 = 1;
const CURSOR_CHECKPOINT_BYTES: u64 = 64 * 1024;
const SOURCE_PREFIX_BYTES: u64 = 64 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentOutputStatus {
    Ok,
    CursorInvalid,
    SessionChanged,
    ObserverUnavailable,
    UnsupportedHarness,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentOutputEventKind {
    AssistantMessage,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentOutputEvent {
    pub event_id: String,
    pub kind: AgentOutputEventKind,
    pub turn_id: Option<String>,
    pub timestamp: Option<DateTime<Utc>>,
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentOutputPage {
    pub schema: String,
    pub status: AgentOutputStatus,
    pub agent_id: String,
    pub session_id: Option<String>,
    pub baseline: bool,
    pub events: Vec<AgentOutputEvent>,
    pub next_cursor: Option<String>,
    pub has_more: bool,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct AgentOutputCursor {
    version: u8,
    agent_id: String,
    session_id: String,
    offset: u64,
    checkpoint_sha256: String,
}

impl AgentOutputCursor {
    fn encode(&self) -> anyhow::Result<String> {
        Ok(URL_SAFE_NO_PAD.encode(serde_json::to_vec(self)?))
    }

    fn decode(encoded: &str) -> anyhow::Result<Self> {
        let decoded = URL_SAFE_NO_PAD.decode(encoded)?;
        let cursor = serde_json::from_slice::<Self>(&decoded)?;
        anyhow::ensure!(
            cursor.version == CURSOR_VERSION,
            "unsupported agent output cursor version {}",
            cursor.version
        );
        Ok(cursor)
    }
}

pub struct AgentService<'a> {
    mux: &'a Mux,
}

pub enum PreparedAgentOutput {
    Immediate(AgentOutputPage),
    Codex(CodexOutputSource),
}

pub struct CodexOutputSource {
    agent_id: String,
    process_id: u32,
    process_start_time: u64,
    session_path: PathBuf,
}

impl CodexOutputSource {
    pub fn read_page(&self, cursor: Option<&str>, limit: usize) -> anyhow::Result<AgentOutputPage> {
        read_codex_page(
            &self.agent_id,
            self.process_id,
            self.process_start_time,
            &self.session_path,
            cursor,
            limit,
        )
    }
}

impl<'a> AgentService<'a> {
    pub(crate) fn new(mux: &'a Mux) -> Self {
        Self { mux }
    }

    pub fn list_agents(&self) -> Vec<AgentSnapshot> {
        self.mux.list_agents()
    }

    pub fn list_agents_cached(&self) -> Vec<AgentSnapshot> {
        self.mux.list_agents_cached()
    }

    pub fn capabilities(&self) -> AgentApiCapabilities {
        self.mux.agent_api_capabilities()
    }

    pub fn catalog(&self) -> AgentCatalog {
        self.mux.agent_api_catalog()
    }

    pub fn capture_admission(&self, request: AgentPromptAdmissionRequest) -> AgentAdmissionCapture {
        self.mux.capture_agent_admission(request)
    }

    pub fn validate_admission(
        &self,
        candidate: &AgentAdmissionCandidate,
    ) -> Option<AgentAdmissionReceipt> {
        self.mux.validate_agent_admission(candidate)
    }

    pub fn write_admitted_prompt(&self, candidate: &AgentAdmissionCandidate) -> anyhow::Result<()> {
        self.mux.write_admitted_prompt(candidate)
    }

    pub fn request_store(&self) -> AgentRequestStore {
        self.mux.agent_request_store()
    }

    pub fn admission_store(&self) -> AgentAdmissionStore {
        self.mux.agent_admission_store()
    }

    pub fn get_request(&self, request_id: &str) -> anyhow::Result<Option<AgentRequest>> {
        self.mux.get_agent_request(request_id)
    }

    pub fn list_request_events(
        &self,
        after_sequence: u64,
        limit: usize,
    ) -> anyhow::Result<Vec<AgentRequest>> {
        self.mux.list_agent_request_events(after_sequence, limit)
    }

    pub fn cancel_request(&self, request_id: &str) -> anyhow::Result<AgentRequest> {
        self.mux.cancel_agent_request(request_id)
    }

    pub fn prepare_output(&self, agent_id: &str) -> anyhow::Result<PreparedAgentOutput> {
        let agent = self
            .list_agents()
            .into_iter()
            .find(|agent| {
                agent.metadata.agent_id == agent_id && matches!(agent.origin, AgentOrigin::Adopted)
            })
            .ok_or_else(|| anyhow::anyhow!("no adopted agent with id {agent_id}"))?;

        if agent.runtime.harness != AgentHarness::Codex {
            return Ok(PreparedAgentOutput::Immediate(page(
                agent_id,
                AgentOutputStatus::UnsupportedHarness,
                None,
                false,
                None,
                "normalized output is currently implemented only for Codex agents",
            )));
        }

        let Some(session_path) = agent.runtime.session_path.as_deref() else {
            return Ok(PreparedAgentOutput::Immediate(page(
                agent_id,
                AgentOutputStatus::ObserverUnavailable,
                None,
                false,
                None,
                "the Codex observer has not confirmed a session for this agent",
            )));
        };
        let (Some(process_id), Some(process_start_time)) = (
            agent.metadata.adopted_pid,
            agent.metadata.adopted_start_time,
        ) else {
            return Ok(PreparedAgentOutput::Immediate(page(
                agent_id,
                AgentOutputStatus::ObserverUnavailable,
                None,
                false,
                None,
                "the Codex observer has not confirmed the target process incarnation",
            )));
        };
        Ok(PreparedAgentOutput::Codex(CodexOutputSource {
            agent_id: agent_id.to_string(),
            process_id,
            process_start_time,
            session_path: PathBuf::from(session_path),
        }))
    }
}

fn read_codex_page(
    agent_id: &str,
    process_id: u32,
    process_start_time: u64,
    session_path: &Path,
    cursor: Option<&str>,
    limit: usize,
) -> anyhow::Result<AgentOutputPage> {
    let source_identity = match codex_source_identity(session_path) {
        Ok(identity) => identity,
        Err(err) => {
            return Ok(page(
                agent_id,
                AgentOutputStatus::ObserverUnavailable,
                None,
                false,
                None,
                &format!("the confirmed Codex session cannot be identified: {err:#}"),
            ));
        }
    };
    let session_id = opaque_session_id(
        agent_id,
        process_id,
        process_start_time,
        session_path,
        &source_identity,
    );
    let complete_tail = match codex_complete_tail_offset(session_path) {
        Ok(offset) => offset,
        Err(err) => {
            return Ok(page(
                agent_id,
                AgentOutputStatus::ObserverUnavailable,
                Some(session_id),
                false,
                None,
                &format!("the confirmed Codex session cannot be read: {err:#}"),
            ));
        }
    };

    let (offset, baseline) = match cursor {
        None => (complete_tail, true),
        Some(encoded) => {
            let decoded = match AgentOutputCursor::decode(encoded) {
                Ok(cursor) => cursor,
                Err(err) => {
                    return Ok(page(
                        agent_id,
                        AgentOutputStatus::CursorInvalid,
                        Some(session_id),
                        false,
                        None,
                        &format!("invalid agent output cursor: {err:#}"),
                    ));
                }
            };
            if decoded.agent_id != agent_id {
                return Ok(page(
                    agent_id,
                    AgentOutputStatus::CursorInvalid,
                    Some(session_id),
                    false,
                    None,
                    "the cursor belongs to a different agent",
                ));
            }
            if decoded.session_id != session_id {
                let reset = cursor_for(agent_id, &session_id, session_path, complete_tail)?;
                return Ok(page(
                    agent_id,
                    AgentOutputStatus::SessionChanged,
                    Some(session_id),
                    true,
                    Some(reset),
                    "the agent is now attached to a different Codex session",
                ));
            }
            if decoded.offset > complete_tail {
                let reset = cursor_for(agent_id, &session_id, session_path, complete_tail)?;
                return Ok(page(
                    agent_id,
                    AgentOutputStatus::CursorInvalid,
                    Some(session_id),
                    true,
                    Some(reset),
                    "the cursor is past the current complete Codex session tail",
                ));
            }
            let checkpoint = codex_checkpoint(session_path, decoded.offset)?;
            if decoded.checkpoint_sha256 != checkpoint {
                let reset = cursor_for(agent_id, &session_id, session_path, complete_tail)?;
                return Ok(page(
                    agent_id,
                    AgentOutputStatus::CursorInvalid,
                    Some(session_id),
                    true,
                    Some(reset),
                    "the provider history before this cursor changed; output between cursors is an explicit gap",
                ));
            }
            (decoded.offset, false)
        }
    };

    let (messages, next_offset, has_more) =
        read_codex_output_messages(session_path, offset, limit.clamp(1, 1000))?;
    let current_source_identity = codex_source_identity(session_path)?;
    if current_source_identity != source_identity {
        return Ok(page(
            agent_id,
            AgentOutputStatus::ObserverUnavailable,
            None,
            false,
            None,
            "the Codex session file changed while it was being read; retry the page",
        ));
    }
    let events = messages
        .into_iter()
        .map(|message| AgentOutputEvent {
            event_id: output_event_id(
                &session_id,
                message.start_offset,
                message.end_offset,
                &message.record_sha256,
            ),
            kind: AgentOutputEventKind::AssistantMessage,
            turn_id: message.turn_id,
            timestamp: message.timestamp,
            text: message.text,
        })
        .collect();
    Ok(AgentOutputPage {
        schema: AGENT_OUTPUT_SCHEMA.to_string(),
        status: AgentOutputStatus::Ok,
        agent_id: agent_id.to_string(),
        session_id: Some(session_id.clone()),
        baseline,
        events,
        next_cursor: Some(cursor_for(
            agent_id,
            &session_id,
            session_path,
            next_offset,
        )?),
        has_more,
        detail: None,
    })
}

fn page(
    agent_id: &str,
    status: AgentOutputStatus,
    session_id: Option<String>,
    baseline: bool,
    next_cursor: Option<String>,
    detail: &str,
) -> AgentOutputPage {
    AgentOutputPage {
        schema: AGENT_OUTPUT_SCHEMA.to_string(),
        status,
        agent_id: agent_id.to_string(),
        session_id,
        baseline,
        events: Vec::new(),
        next_cursor,
        has_more: false,
        detail: Some(detail.to_string()),
    }
}

fn cursor_for(
    agent_id: &str,
    session_id: &str,
    session_path: &Path,
    offset: u64,
) -> anyhow::Result<String> {
    AgentOutputCursor {
        version: CURSOR_VERSION,
        agent_id: agent_id.to_string(),
        session_id: session_id.to_string(),
        offset,
        checkpoint_sha256: codex_checkpoint(session_path, offset)?,
    }
    .encode()
}

fn opaque_session_id(
    agent_id: &str,
    process_id: u32,
    process_start_time: u64,
    path: &Path,
    source_identity: &str,
) -> String {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    sha256(&format!(
        "codex\0{agent_id}\0{process_id}\0{process_start_time}\0{}\0{source_identity}",
        canonical.display()
    ))
}

fn output_event_id(
    session_id: &str,
    start_offset: u64,
    end_offset: u64,
    record_sha256: &str,
) -> String {
    sha256(&format!(
        "{session_id}\0{start_offset}\0{end_offset}\0{record_sha256}"
    ))
}

fn codex_source_identity(path: &Path) -> anyhow::Result<String> {
    let file = fs::File::open(path)?;
    let metadata = file.metadata()?;
    let mut identity = Sha256::new();

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        identity.update(metadata.dev().to_le_bytes());
        identity.update(metadata.ino().to_le_bytes());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        identity.update(metadata.creation_time().to_le_bytes());
    }
    #[cfg(not(any(unix, windows)))]
    if let Ok(created) = metadata.created() {
        if let Ok(duration) = created.duration_since(std::time::UNIX_EPOCH) {
            identity.update(duration.as_nanos().to_le_bytes());
        }
    }

    let mut first_record = Vec::new();
    BufReader::new(file.take(SOURCE_PREFIX_BYTES)).read_until(b'\n', &mut first_record)?;
    anyhow::ensure!(
        first_record.last() == Some(&b'\n'),
        "Codex session has no complete identifying record"
    );
    identity.update(first_record);
    Ok(format!("{:x}", identity.finalize()))
}

fn codex_checkpoint(path: &Path, offset: u64) -> anyhow::Result<String> {
    let start = offset.saturating_sub(CURSOR_CHECKPOINT_BYTES);
    let mut file = fs::File::open(path)?;
    anyhow::ensure!(
        file.metadata()?.len() >= offset,
        "Codex session is shorter than cursor offset {offset}"
    );
    file.seek(SeekFrom::Start(start))?;
    let mut content = vec![0; (offset - start) as usize];
    file.read_exact(&mut content)?;
    let mut checkpoint = Sha256::new();
    checkpoint.update(start.to_le_bytes());
    checkpoint.update(offset.to_le_bytes());
    checkpoint.update(content);
    Ok(format!("{:x}", checkpoint.finalize()))
}

fn sha256(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn cursor_round_trips_without_exposing_provider_state() {
        let cursor = AgentOutputCursor {
            version: CURSOR_VERSION,
            agent_id: "agent-1".to_string(),
            session_id: "session-1".to_string(),
            offset: 42,
            checkpoint_sha256: "checkpoint-1".to_string(),
        };
        let encoded = cursor.encode().unwrap();
        assert!(!encoded.contains("agent-1"));
        assert!(!encoded.contains("session-1"));
        assert_eq!(AgentOutputCursor::decode(&encoded).unwrap(), cursor);
    }

    #[test]
    fn event_identity_is_stable_and_bound_to_position_and_content() {
        assert_eq!(
            output_event_id("session", 1, 2, "record"),
            output_event_id("session", 1, 2, "record")
        );
        assert_ne!(
            output_event_id("session", 1, 2, "record"),
            output_event_id("session", 2, 3, "record")
        );
        assert_ne!(
            output_event_id("session", 1, 2, "record"),
            output_event_id("other", 1, 2, "record")
        );
        assert_ne!(
            output_event_id("session", 1, 2, "record"),
            output_event_id("session", 1, 2, "rewritten")
        );
    }

    #[test]
    fn codex_page_baselines_then_returns_stable_normalized_events() {
        let temp = TempDir::new().unwrap();
        let session = temp.path().join("rollout.jsonl");
        fs::write(
            &session,
            "{\"type\":\"session_meta\",\"payload\":{\"cwd\":\"/tmp/project\"}}\n",
        )
        .unwrap();

        let baseline = read_codex_page("agent-1", 10, 20, &session, None, 100).unwrap();
        assert_eq!(baseline.status, AgentOutputStatus::Ok);
        assert!(baseline.baseline);
        assert!(baseline.events.is_empty());
        let cursor = baseline.next_cursor.unwrap();

        let message = "{\"type\":\"response_item\",\"timestamp\":\"2026-03-17T12:00:02Z\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"done\"}],\"internal_chat_message_metadata_passthrough\":{\"turn_id\":\"turn-1\"}}}\n";
        fs::OpenOptions::new()
            .append(true)
            .open(&session)
            .unwrap()
            .write_all(message.as_bytes())
            .unwrap();

        let page = read_codex_page("agent-1", 10, 20, &session, Some(&cursor), 100).unwrap();
        let replay = read_codex_page("agent-1", 10, 20, &session, Some(&cursor), 100).unwrap();
        assert_eq!(page.status, AgentOutputStatus::Ok);
        assert!(!page.baseline);
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.events[0].text, "done");
        assert_eq!(page.events[0].turn_id.as_deref(), Some("turn-1"));
        assert_eq!(page.events[0].event_id, replay.events[0].event_id);
    }

    #[test]
    fn codex_page_reports_session_change_with_an_explicit_reset_cursor() {
        let temp = TempDir::new().unwrap();
        let first = temp.path().join("rollout-first.jsonl");
        let second = temp.path().join("rollout-second.jsonl");
        fs::write(&first, "{}\n").unwrap();
        fs::write(&second, "{}\n").unwrap();

        let cursor = read_codex_page("agent-1", 10, 20, &first, None, 100)
            .unwrap()
            .next_cursor
            .unwrap();
        let changed = read_codex_page("agent-1", 10, 20, &second, Some(&cursor), 100).unwrap();

        assert_eq!(changed.status, AgentOutputStatus::SessionChanged);
        assert!(changed.baseline);
        assert!(changed.events.is_empty());
        assert!(changed.next_cursor.is_some());
    }

    #[test]
    fn replacing_a_codex_file_at_the_same_path_changes_the_session() {
        let temp = TempDir::new().unwrap();
        let session = temp.path().join("rollout.jsonl");
        let replacement = temp.path().join("replacement.jsonl");
        fs::write(&session, "{\"session\":\"first\"}\n").unwrap();
        fs::write(&replacement, "{\"session\":\"second\"}\n").unwrap();
        let cursor = read_codex_page("agent-1", 10, 20, &session, None, 100)
            .unwrap()
            .next_cursor
            .unwrap();

        fs::remove_file(&session).unwrap();
        fs::rename(&replacement, &session).unwrap();
        let changed = read_codex_page("agent-1", 10, 20, &session, Some(&cursor), 100).unwrap();

        assert_eq!(changed.status, AgentOutputStatus::SessionChanged);
        assert!(changed.baseline);
        assert!(changed.next_cursor.is_some());
    }

    #[test]
    fn rewriting_a_codex_file_in_place_invalidates_its_checkpoint() {
        let temp = TempDir::new().unwrap();
        let session = temp.path().join("rollout.jsonl");
        let header = "{\"type\":\"session_meta\"}\n";
        let old_record = "{\"value\":\"aaaa\"}\n";
        fs::write(&session, format!("{header}{old_record}")).unwrap();
        let cursor = read_codex_page("agent-1", 10, 20, &session, None, 100)
            .unwrap()
            .next_cursor
            .unwrap();

        let new_record = "{\"value\":\"bbbb\"}\n";
        let assistant = "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"new\"}]}}\n";
        fs::write(&session, format!("{header}{new_record}{assistant}")).unwrap();
        let changed = read_codex_page("agent-1", 10, 20, &session, Some(&cursor), 100).unwrap();

        assert_eq!(changed.status, AgentOutputStatus::CursorInvalid);
        assert!(changed.baseline);
        assert!(changed.events.is_empty());
        assert!(changed.detail.unwrap().contains("explicit gap"));
    }

    #[test]
    fn process_incarnation_changes_the_session_even_at_the_same_path() {
        let temp = TempDir::new().unwrap();
        let session = temp.path().join("rollout.jsonl");
        fs::write(&session, "{}\n").unwrap();
        let cursor = read_codex_page("agent-1", 10, 20, &session, None, 100)
            .unwrap()
            .next_cursor
            .unwrap();

        let changed = read_codex_page("agent-1", 10, 21, &session, Some(&cursor), 100).unwrap();

        assert_eq!(changed.status, AgentOutputStatus::SessionChanged);
    }
}
