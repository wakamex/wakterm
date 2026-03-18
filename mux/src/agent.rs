use crate::domain::DomainId;
use crate::pane::PaneId;
use crate::tab::TabId;
use crate::window::WindowId;
use chrono::{DateTime, Datelike, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use url::Url;
use wezterm_term::Progress;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentMetadata {
    pub agent_id: String,
    pub name: String,
    pub launch_cmd: String,
    pub declared_cwd: String,
    pub created_at: DateTime<Utc>,
    pub repo_root: Option<String>,
    pub worktree: Option<String>,
    pub branch: Option<String>,
    pub managed_checkout: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum AgentHarness {
    Unknown,
    Claude,
    Codex,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum AgentTransport {
    PlainPty,
    ObservedPty,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum AgentStatus {
    Starting,
    Busy,
    Idle,
    Errored,
    Exited,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum AgentTurnState {
    Unknown,
    WaitingOnAgent,
    WaitingOnUser,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentTabBadgeState {
    pub waiting_on_user: bool,
    pub needs_attention: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentRuntimeSnapshot {
    pub harness: AgentHarness,
    pub transport: AgentTransport,
    pub status: AgentStatus,
    pub turn_state: AgentTurnState,
    pub alive: bool,
    pub foreground_process_name: Option<String>,
    pub tty_name: Option<String>,
    pub last_input_at: Option<DateTime<Utc>>,
    pub last_output_at: Option<DateTime<Utc>>,
    pub last_progress_at: Option<DateTime<Utc>>,
    pub last_turn_completed_at: Option<DateTime<Utc>>,
    pub observed_at: DateTime<Utc>,
    pub session_path: Option<String>,
    pub progress_summary: Option<String>,
    pub terminal_progress: Progress,
    pub observer_error: Option<String>,
    #[serde(skip, default)]
    pub observer_started_at: Option<DateTime<Utc>>,
}

impl AgentRuntimeSnapshot {
    pub fn new(metadata: &AgentMetadata) -> Self {
        let now = Utc::now();
        let harness = infer_harness(&metadata.launch_cmd, None);
        Self {
            harness,
            transport: AgentTransport::PlainPty,
            status: AgentStatus::Starting,
            turn_state: AgentTurnState::Unknown,
            alive: true,
            foreground_process_name: None,
            tty_name: None,
            last_input_at: None,
            last_output_at: None,
            last_progress_at: None,
            last_turn_completed_at: None,
            observed_at: now,
            session_path: None,
            progress_summary: None,
            terminal_progress: Progress::None,
            observer_error: None,
            observer_started_at: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentSnapshot {
    pub metadata: AgentMetadata,
    pub runtime: AgentRuntimeSnapshot,
    pub pane_id: PaneId,
    pub tab_id: TabId,
    pub window_id: WindowId,
    pub workspace: String,
    pub domain_id: DomainId,
}

pub fn prime_runtime_for_new_agent(
    runtime: &mut AgentRuntimeSnapshot,
    metadata: &AgentMetadata,
    foreground_process_name: Option<&str>,
) {
    let configured_harness = infer_harness(&metadata.launch_cmd, None);
    let process_harness = infer_harness("", foreground_process_name);

    if matches!(configured_harness, AgentHarness::Unknown) || configured_harness == process_harness
    {
        runtime.observer_started_at = None;
        return;
    }

    runtime.observer_started_at = Some(Utc::now());
    runtime.session_path = None;
    runtime.progress_summary = None;
    runtime.turn_state = AgentTurnState::Unknown;
    runtime.last_turn_completed_at = None;
    runtime.transport = AgentTransport::PlainPty;
}

pub fn infer_harness(launch_cmd: &str, foreground_process_name: Option<&str>) -> AgentHarness {
    let mut candidates = vec![launch_cmd.to_ascii_lowercase()];
    if let Some(name) = foreground_process_name {
        candidates.push(name.to_ascii_lowercase());
    }
    for candidate in &candidates {
        if candidate.contains("claude") {
            return AgentHarness::Claude;
        }
        if candidate.contains("codex") {
            return AgentHarness::Codex;
        }
    }
    AgentHarness::Unknown
}

pub fn derive_runtime_status(runtime: &AgentRuntimeSnapshot) -> AgentStatus {
    if !runtime.alive {
        return AgentStatus::Exited;
    }

    if matches!(runtime.terminal_progress, Progress::Error(_)) {
        return AgentStatus::Errored;
    }

    if matches!(
        runtime.terminal_progress,
        Progress::Percentage(_) | Progress::Indeterminate
    ) {
        return AgentStatus::Busy;
    }

    let activity_times = [
        runtime.last_input_at,
        runtime.last_output_at,
        runtime.last_progress_at,
    ];
    let last_activity = activity_times.iter().flatten().copied().max();

    match last_activity {
        None => AgentStatus::Starting,
        Some(ts) if Utc::now() - ts <= Duration::seconds(30) => AgentStatus::Busy,
        Some(_) => AgentStatus::Idle,
    }
}

fn derive_effective_turn_state(runtime: &AgentRuntimeSnapshot) -> AgentTurnState {
    if !runtime.alive {
        return AgentTurnState::Unknown;
    }

    if let Some(completed_at) = runtime.last_turn_completed_at {
        if runtime
            .last_input_at
            .map(|input_at| input_at > completed_at)
            .unwrap_or(false)
        {
            return AgentTurnState::WaitingOnAgent;
        }
        return AgentTurnState::WaitingOnUser;
    }

    if runtime.last_input_at.is_some() && !matches!(runtime.harness, AgentHarness::Unknown) {
        return AgentTurnState::WaitingOnAgent;
    }

    runtime.turn_state.clone()
}

pub fn refresh_runtime_from_harness(runtime: &mut AgentRuntimeSnapshot, metadata: &AgentMetadata) {
    let normalized_cwd = normalize_declared_cwd(&metadata.declared_cwd);
    let cwd = normalized_cwd.trim();
    if cwd.is_empty() {
        runtime.observed_at = Utc::now();
        runtime.turn_state = AgentTurnState::Unknown;
        runtime.last_turn_completed_at = None;
        runtime.status = derive_runtime_status(runtime);
        return;
    }

    runtime.observer_error = None;
    runtime.observed_at = Utc::now();
    let configured_harness = infer_harness(&metadata.launch_cmd, None);
    let process_harness = infer_harness("", runtime.foreground_process_name.as_deref());
    runtime.harness = match configured_harness {
        AgentHarness::Unknown => process_harness.clone(),
        _ => configured_harness.clone(),
    };

    let observing_harness = match configured_harness {
        AgentHarness::Unknown => process_harness.clone(),
        _ if process_harness == configured_harness => configured_harness.clone(),
        _ => {
            runtime.session_path = None;
            runtime.progress_summary = None;
            runtime.turn_state = AgentTurnState::Unknown;
            runtime.last_turn_completed_at = None;
            runtime.transport = AgentTransport::PlainPty;
            runtime.status = derive_runtime_status(runtime);
            return;
        }
    };

    let observed = match observing_harness {
        AgentHarness::Claude => observe_claude(
            cwd,
            runtime.session_path.as_deref(),
            runtime.observer_started_at,
        ),
        AgentHarness::Codex => observe_codex(
            cwd,
            runtime.session_path.as_deref(),
            runtime.observer_started_at,
        ),
        AgentHarness::Unknown => Ok(None),
    };

    match observed {
        Ok(Some(snapshot)) => {
            runtime.session_path = snapshot.session_path;
            runtime.progress_summary = snapshot.progress_summary;
            runtime.turn_state = snapshot.turn_state;
            runtime.last_turn_completed_at = snapshot.last_turn_completed_at;
            runtime.observer_started_at = None;
            if let Some(ts) = snapshot.updated_at {
                runtime.last_progress_at = Some(
                    runtime
                        .last_progress_at
                        .map(|existing| existing.max(ts))
                        .unwrap_or(ts),
                );
            }
        }
        Ok(None) => {
            if !matches!(runtime.harness, AgentHarness::Unknown) {
                runtime.session_path = None;
                runtime.progress_summary = None;
                runtime.turn_state = AgentTurnState::Unknown;
                runtime.last_turn_completed_at = None;
            }
        }
        Err(err) => {
            runtime.observer_error = Some(err.to_string());
        }
    }

    if matches!(runtime.harness, AgentHarness::Unknown) {
        runtime.turn_state = AgentTurnState::Unknown;
        runtime.last_turn_completed_at = None;
    }

    runtime.transport = if runtime.session_path.is_some() {
        AgentTransport::ObservedPty
    } else {
        AgentTransport::PlainPty
    };
    runtime.turn_state = derive_effective_turn_state(runtime);
    runtime.status = derive_runtime_status(runtime);
}

#[derive(Debug)]
struct HarnessObservation {
    session_path: Option<String>,
    progress_summary: Option<String>,
    updated_at: Option<DateTime<Utc>>,
    turn_state: AgentTurnState,
    last_turn_completed_at: Option<DateTime<Utc>>,
}

fn observe_claude(
    cwd: &str,
    preferred_session: Option<&str>,
    updated_after: Option<DateTime<Utc>>,
) -> anyhow::Result<Option<HarnessObservation>> {
    let Some(root) = claude_sessions_root() else {
        return Ok(None);
    };
    let project_dir = root.join(cwd.replace('/', "-"));
    if !project_dir.is_dir() {
        return Ok(None);
    }

    if let Some(preferred_session) = preferred_session {
        let preferred_path = Path::new(preferred_session);
        if preferred_path.is_file() {
            let modified_at = DateTime::<Utc>::from(fs::metadata(preferred_path)?.modified()?);
            if updated_after
                .map(|cutoff| modified_at >= cutoff)
                .unwrap_or(true)
            {
                let (progress_summary, turn_state, last_turn_completed_at) =
                    read_last_claude_observation(preferred_path)?;
                return Ok(Some(HarnessObservation {
                    session_path: Some(preferred_path.to_string_lossy().to_string()),
                    progress_summary,
                    updated_at: Some(modified_at),
                    turn_state,
                    last_turn_completed_at,
                }));
            }
        }
    }

    let prefer_earliest = updated_after.is_some();
    let mut selected: Option<(PathBuf, DateTime<Utc>)> = None;
    for entry in fs::read_dir(&project_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        if !claude_session_is_interactive(&path)? {
            continue;
        }
        let modified_at = DateTime::<Utc>::from(entry.metadata()?.modified()?);
        if updated_after
            .map(|cutoff| modified_at < cutoff)
            .unwrap_or(false)
        {
            continue;
        }
        match &selected {
            Some((_, existing_modified))
                if (prefer_earliest && *existing_modified <= modified_at)
                    || (!prefer_earliest && *existing_modified >= modified_at) => {}
            _ => selected = Some((path, modified_at)),
        }
    }

    let Some((session, modified_at)) = selected else {
        return Ok(None);
    };
    let (progress_summary, turn_state, last_turn_completed_at) =
        read_last_claude_observation(&session)?;
    Ok(Some(HarnessObservation {
        session_path: Some(session.to_string_lossy().to_string()),
        progress_summary,
        updated_at: Some(modified_at),
        turn_state,
        last_turn_completed_at,
    }))
}

fn observe_codex(
    cwd: &str,
    preferred_session: Option<&str>,
    updated_after: Option<DateTime<Utc>>,
) -> anyhow::Result<Option<HarnessObservation>> {
    let Some(root) = codex_sessions_root() else {
        return Ok(None);
    };

    if let Some(preferred_session) = preferred_session {
        let preferred_path = Path::new(preferred_session);
        if preferred_path.is_file() {
            let modified_at = DateTime::<Utc>::from(fs::metadata(preferred_path)?.modified()?);
            if updated_after
                .map(|cutoff| modified_at >= cutoff)
                .unwrap_or(true)
            {
                let (progress_summary, turn_state, last_turn_completed_at) =
                    read_last_codex_observation(preferred_path)?;
                return Ok(Some(HarnessObservation {
                    session_path: Some(preferred_path.to_string_lossy().to_string()),
                    progress_summary,
                    updated_at: Some(modified_at),
                    turn_state,
                    last_turn_completed_at,
                }));
            }
        }
    }

    let prefer_earliest = updated_after.is_some();
    let mut selected: Option<(PathBuf, DateTime<Utc>)> = None;
    for days_ago in 0..=6 {
        let day = Utc::now() - Duration::days(days_ago);
        let dir = root
            .join(format!("{:04}", day.year()))
            .join(format!("{:02}", day.month()))
            .join(format!("{:02}", day.day()));
        if !dir.is_dir() {
            continue;
        }

        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if !path
                .file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
                .unwrap_or(false)
            {
                continue;
            }
            if !codex_session_matches_cwd(&path, cwd)? {
                continue;
            }
            let modified_at = DateTime::<Utc>::from(entry.metadata()?.modified()?);
            if updated_after
                .map(|cutoff| modified_at < cutoff)
                .unwrap_or(false)
            {
                continue;
            }
            match &selected {
                Some((_, existing_modified))
                    if (prefer_earliest && *existing_modified <= modified_at)
                        || (!prefer_earliest && *existing_modified >= modified_at) => {}
                _ => selected = Some((path, modified_at)),
            }
        }
    }

    let Some((session, modified_at)) = selected else {
        return Ok(None);
    };
    let (progress_summary, turn_state, last_turn_completed_at) =
        read_last_codex_observation(&session)?;
    Ok(Some(HarnessObservation {
        session_path: Some(session.to_string_lossy().to_string()),
        progress_summary,
        updated_at: Some(modified_at),
        turn_state,
        last_turn_completed_at,
    }))
}

fn claude_sessions_root() -> Option<PathBuf> {
    std::env::var_os("WEZTERM_AGENT_CLAUDE_DIR")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join(".claude").join("projects")))
}

fn codex_sessions_root() -> Option<PathBuf> {
    std::env::var_os("WEZTERM_AGENT_CODEX_DIR")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join(".codex").join("sessions")))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

fn claude_session_is_interactive(path: &Path) -> anyhow::Result<bool> {
    let Some(first_line) = BufReader::new(fs::File::open(path)?)
        .lines()
        .next()
        .transpose()?
    else {
        return Ok(true);
    };
    let record: Value = serde_json::from_str(&first_line)?;
    Ok(record.get("type").and_then(Value::as_str) != Some("queue-operation"))
}

fn codex_session_matches_cwd(path: &Path, cwd: &str) -> anyhow::Result<bool> {
    let Some(first_line) = BufReader::new(fs::File::open(path)?)
        .lines()
        .next()
        .transpose()?
    else {
        return Ok(false);
    };
    let record: Value = serde_json::from_str(&first_line)?;
    Ok(record
        .get("payload")
        .and_then(|payload| payload.get("cwd"))
        .and_then(Value::as_str)
        == Some(cwd))
}

fn derive_turn_state(
    last_user_at: Option<DateTime<Utc>>,
    last_assistant_at: Option<DateTime<Utc>>,
) -> (AgentTurnState, Option<DateTime<Utc>>) {
    match (last_user_at, last_assistant_at) {
        (Some(user_at), Some(assistant_at)) if assistant_at >= user_at => {
            (AgentTurnState::WaitingOnUser, Some(assistant_at))
        }
        (Some(_), Some(_)) | (Some(_), None) => (AgentTurnState::WaitingOnAgent, None),
        (None, Some(assistant_at)) => (AgentTurnState::WaitingOnUser, Some(assistant_at)),
        (None, None) => (AgentTurnState::Unknown, None),
    }
}

fn parse_record_timestamp(record: &Value) -> Option<DateTime<Utc>> {
    record
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(|ts| DateTime::parse_from_rfc3339(ts).ok())
        .map(|dt| dt.with_timezone(&Utc))
}

fn read_last_claude_observation(
    path: &Path,
) -> anyhow::Result<(Option<String>, AgentTurnState, Option<DateTime<Utc>>)> {
    let reader = BufReader::new(fs::File::open(path)?);
    let mut summary = None;
    let mut last_user_at = None;
    let mut last_assistant_at = None;
    for line in reader.lines() {
        let line = line?;
        let Ok(record) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match record.get("type").and_then(Value::as_str) {
            Some("user") => {
                last_user_at = parse_record_timestamp(&record).or(last_user_at);
            }
            Some("assistant") => {}
            _ => continue,
        }
        if record.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        last_assistant_at = parse_record_timestamp(&record).or(last_assistant_at);
        let Some(content) = record
            .get("message")
            .and_then(|message| message.get("content"))
            .and_then(Value::as_array)
        else {
            continue;
        };

        let mut parts = vec![];
        for block in content {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                        let text = text.trim();
                        if !text.is_empty() {
                            parts.push(text.to_string());
                        }
                    }
                }
                Some("tool_use")
                    if block.get("name").and_then(Value::as_str) == Some("ExitPlanMode") =>
                {
                    if let Some(plan) = block
                        .get("input")
                        .and_then(|input| input.get("plan"))
                        .and_then(Value::as_str)
                    {
                        let plan = plan.trim();
                        if !plan.is_empty() {
                            parts.push(format!("PLAN: {plan}"));
                        }
                    }
                }
                _ => {}
            }
        }
        if !parts.is_empty() {
            summary = Some(truncate_summary(&parts.join("\n")));
        }
    }
    let (turn_state, last_turn_completed_at) = derive_turn_state(last_user_at, last_assistant_at);
    Ok((summary, turn_state, last_turn_completed_at))
}

fn read_last_codex_observation(
    path: &Path,
) -> anyhow::Result<(Option<String>, AgentTurnState, Option<DateTime<Utc>>)> {
    let reader = BufReader::new(fs::File::open(path)?);
    let mut summary = None;
    let mut last_user_at = None;
    let mut last_assistant_at = None;
    for line in reader.lines() {
        let line = line?;
        let Ok(record) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match record.get("type").and_then(Value::as_str) {
            Some("response_item") => {
                let Some(payload) = record.get("payload") else {
                    continue;
                };
                if payload.get("type").and_then(Value::as_str) != Some("message") {
                    continue;
                }
                match payload.get("role").and_then(Value::as_str) {
                    Some("assistant") => {
                        last_assistant_at = parse_record_timestamp(&record).or(last_assistant_at);
                        let Some(content) = payload.get("content").and_then(Value::as_array) else {
                            continue;
                        };
                        let mut parts = vec![];
                        for block in content {
                            if block.get("type").and_then(Value::as_str) == Some("output_text") {
                                if let Some(text) = block.get("text").and_then(Value::as_str) {
                                    let text = text.trim();
                                    if !text.is_empty() {
                                        parts.push(text.to_string());
                                    }
                                }
                            }
                        }
                        if !parts.is_empty() {
                            summary = Some(truncate_summary(&parts.join("\n")));
                        }
                    }
                    Some("user") => {
                        last_user_at = parse_record_timestamp(&record).or(last_user_at);
                    }
                    _ => {}
                }
            }
            Some("event_msg")
                if record
                    .get("payload")
                    .and_then(|payload| payload.get("type"))
                    .and_then(Value::as_str)
                    == Some("user_message") =>
            {
                last_user_at = parse_record_timestamp(&record).or(last_user_at);
            }
            _ => {}
        }
    }
    let (turn_state, last_turn_completed_at) = derive_turn_state(last_user_at, last_assistant_at);
    Ok((summary, turn_state, last_turn_completed_at))
}

fn truncate_summary(summary: &str) -> String {
    const MAX_CHARS: usize = 240;
    if summary.chars().count() <= MAX_CHARS {
        return summary.to_string();
    }
    let truncated = summary.chars().take(MAX_CHARS).collect::<String>();
    format!("{truncated}...")
}

fn normalize_declared_cwd(cwd: &str) -> String {
    if cwd.starts_with("file://") {
        if let Ok(url) = Url::parse(cwd) {
            if let Ok(path) = url.to_file_path() {
                return path.to_string_lossy().to_string();
            }
        }
    }
    cwd.to_string()
}

#[cfg(test)]
mod test {
    use super::*;
    use chrono::TimeZone;
    use std::sync::Mutex;
    use tempfile::TempDir;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn set_env_path(key: &str, path: &Path) {
        unsafe {
            std::env::set_var(key, path);
        }
    }

    fn remove_env_var(key: &str) {
        unsafe {
            std::env::remove_var(key);
        }
    }

    #[test]
    fn infers_harness_from_launch_command_or_foreground_process() {
        assert_eq!(
            infer_harness("codex --model gpt-5", None),
            AgentHarness::Codex
        );
        assert_eq!(
            infer_harness("python agent.py", Some("claude")),
            AgentHarness::Claude
        );
        assert_eq!(
            infer_harness("python agent.py", None),
            AgentHarness::Unknown
        );
    }

    #[test]
    fn derives_runtime_status_from_liveness_progress_and_recent_activity() {
        let metadata = AgentMetadata {
            agent_id: "id".to_string(),
            name: "alpha".to_string(),
            launch_cmd: "codex".to_string(),
            declared_cwd: "/tmp/alpha".to_string(),
            created_at: Utc::now(),
            repo_root: None,
            worktree: None,
            branch: None,
            managed_checkout: false,
        };
        let mut runtime = AgentRuntimeSnapshot::new(&metadata);
        assert_eq!(derive_runtime_status(&runtime), AgentStatus::Starting);

        runtime.last_output_at = Some(Utc::now());
        assert_eq!(derive_runtime_status(&runtime), AgentStatus::Busy);

        runtime.last_output_at = Some(Utc::now() - Duration::minutes(5));
        assert_eq!(derive_runtime_status(&runtime), AgentStatus::Idle);

        runtime.terminal_progress = Progress::Error(1);
        assert_eq!(derive_runtime_status(&runtime), AgentStatus::Errored);

        runtime.alive = false;
        assert_eq!(derive_runtime_status(&runtime), AgentStatus::Exited);
    }

    #[test]
    fn observes_latest_claude_session_summary() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let cwd = "/tmp/project-a";
        let project_dir = temp.path().join(cwd.replace('/', "-"));
        fs::create_dir_all(&project_dir).unwrap();
        let session = project_dir.join("session.jsonl");
        fs::write(
            &session,
            concat!(
                "{\"type\":\"user\",\"timestamp\":\"2026-03-17T12:00:00Z\"}\n",
                "{\"type\":\"assistant\",\"timestamp\":\"2026-03-17T12:00:02Z\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"done\"}]}}\n"
            ),
        )
        .unwrap();

        set_env_path("WEZTERM_AGENT_CLAUDE_DIR", temp.path());
        let observed = observe_claude(cwd, None, None).unwrap().unwrap();
        remove_env_var("WEZTERM_AGENT_CLAUDE_DIR");

        assert_eq!(observed.progress_summary.as_deref(), Some("done"));
        assert_eq!(
            observed.session_path.as_deref(),
            Some(session.to_string_lossy().as_ref())
        );
        assert_eq!(observed.turn_state, AgentTurnState::WaitingOnUser);
        assert_eq!(
            observed.last_turn_completed_at,
            Some(Utc.with_ymd_and_hms(2026, 3, 17, 12, 0, 2).unwrap())
        );
    }

    #[test]
    fn observes_latest_codex_session_summary() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let temp = TempDir::new().unwrap();
        let day = Utc::now();
        let dir = temp
            .path()
            .join(format!("{:04}", day.year_ce().1))
            .join(format!("{:02}", day.month()))
            .join(format!("{:02}", day.day()));
        fs::create_dir_all(&dir).unwrap();
        let session = dir.join("rollout-test.jsonl");
        fs::write(
            &session,
            concat!(
                "{\"payload\":{\"cwd\":\"/tmp/project-b\"}}\n",
                "{\"type\":\"response_item\",\"timestamp\":\"2026-03-17T12:00:00Z\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[]}}\n",
                "{\"type\":\"response_item\",\"timestamp\":\"2026-03-17T12:00:03Z\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"all good\"}]}}\n"
            ),
        )
        .unwrap();

        set_env_path("WEZTERM_AGENT_CODEX_DIR", temp.path());
        let observed = observe_codex("/tmp/project-b", None, None)
            .unwrap()
            .unwrap();
        remove_env_var("WEZTERM_AGENT_CODEX_DIR");

        assert_eq!(observed.progress_summary.as_deref(), Some("all good"));
        assert_eq!(
            observed.session_path.as_deref(),
            Some(session.to_string_lossy().as_ref())
        );
        assert_eq!(observed.turn_state, AgentTurnState::WaitingOnUser);
        assert_eq!(
            observed.last_turn_completed_at,
            Some(Utc.with_ymd_and_hms(2026, 3, 17, 12, 0, 3).unwrap())
        );
    }

    #[test]
    fn refresh_runtime_marks_waiting_on_agent_after_new_user_turn() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let cwd = "/tmp/project-c";
        let project_dir = temp.path().join(cwd.replace('/', "-"));
        fs::create_dir_all(&project_dir).unwrap();
        let session = project_dir.join("session.jsonl");
        fs::write(
            &session,
            concat!(
                "{\"type\":\"assistant\",\"timestamp\":\"2026-03-17T12:00:00Z\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"first\"}]}}\n",
                "{\"type\":\"user\",\"timestamp\":\"2026-03-17T12:00:05Z\"}\n"
            ),
        )
        .unwrap();

        set_env_path("WEZTERM_AGENT_CLAUDE_DIR", temp.path());
        let metadata = AgentMetadata {
            agent_id: "id".to_string(),
            name: "alpha".to_string(),
            launch_cmd: "claude".to_string(),
            declared_cwd: cwd.to_string(),
            created_at: Utc::now(),
            repo_root: None,
            worktree: None,
            branch: None,
            managed_checkout: false,
        };
        let mut runtime = AgentRuntimeSnapshot::new(&metadata);
        runtime.foreground_process_name = Some("claude".to_string());
        refresh_runtime_from_harness(&mut runtime, &metadata);
        remove_env_var("WEZTERM_AGENT_CLAUDE_DIR");

        assert_eq!(runtime.turn_state, AgentTurnState::WaitingOnAgent);
        assert_eq!(runtime.last_turn_completed_at, None);
        assert_eq!(runtime.transport, AgentTransport::ObservedPty);
    }

    #[test]
    fn refresh_runtime_does_not_bind_harness_session_before_process_matches() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let temp = TempDir::new().unwrap();
        let day = Utc::now();
        let dir = temp
            .path()
            .join(format!("{:04}", day.year_ce().1))
            .join(format!("{:02}", day.month()))
            .join(format!("{:02}", day.day()));
        fs::create_dir_all(&dir).unwrap();
        let session = dir.join("rollout-test.jsonl");
        fs::write(
            &session,
            concat!(
                "{\"payload\":{\"cwd\":\"/tmp/project-d\"}}\n",
                "{\"type\":\"response_item\",\"timestamp\":\"2026-03-17T12:00:03Z\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"old\"}]}}\n"
            ),
        )
        .unwrap();

        set_env_path("WEZTERM_AGENT_CODEX_DIR", temp.path());
        let metadata = AgentMetadata {
            agent_id: "id".to_string(),
            name: "delta".to_string(),
            launch_cmd: "codex".to_string(),
            declared_cwd: "/tmp/project-d".to_string(),
            created_at: Utc::now(),
            repo_root: None,
            worktree: None,
            branch: None,
            managed_checkout: false,
        };
        let mut runtime = AgentRuntimeSnapshot::new(&metadata);
        runtime.foreground_process_name = Some("zsh".to_string());
        runtime.observer_started_at = Some(Utc::now() - Duration::minutes(1));
        refresh_runtime_from_harness(&mut runtime, &metadata);
        remove_env_var("WEZTERM_AGENT_CODEX_DIR");

        assert_eq!(runtime.harness, AgentHarness::Codex);
        assert_eq!(runtime.transport, AgentTransport::PlainPty);
        assert_eq!(runtime.session_path, None);
        assert_eq!(runtime.turn_state, AgentTurnState::Unknown);
    }

    #[test]
    fn observe_codex_prefers_bound_session_over_newer_same_cwd_session() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let temp = TempDir::new().unwrap();
        let day = Utc::now();
        let dir = temp
            .path()
            .join(format!("{:04}", day.year_ce().1))
            .join(format!("{:02}", day.month()))
            .join(format!("{:02}", day.day()));
        fs::create_dir_all(&dir).unwrap();
        let older = dir.join("rollout-older.jsonl");
        fs::write(
            &older,
            concat!(
                "{\"payload\":{\"cwd\":\"/tmp/project-e\"}}\n",
                "{\"type\":\"response_item\",\"timestamp\":\"2026-03-17T12:00:03Z\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"older\"}]}}\n"
            ),
        )
        .unwrap();
        let newer = dir.join("rollout-newer.jsonl");
        fs::write(
            &newer,
            concat!(
                "{\"payload\":{\"cwd\":\"/tmp/project-e\"}}\n",
                "{\"type\":\"response_item\",\"timestamp\":\"2026-03-17T12:00:04Z\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"newer\"}]}}\n"
            ),
        )
        .unwrap();

        set_env_path("WEZTERM_AGENT_CODEX_DIR", temp.path());
        let observed = observe_codex(
            "/tmp/project-e",
            Some(older.to_string_lossy().as_ref()),
            None,
        )
        .unwrap()
        .unwrap();
        remove_env_var("WEZTERM_AGENT_CODEX_DIR");

        assert_eq!(observed.progress_summary.as_deref(), Some("older"));
        assert_eq!(
            observed.session_path.as_deref(),
            Some(older.to_string_lossy().as_ref())
        );
    }
}
