use crate::domain::DomainId;
use crate::pane::PaneId;
use crate::tab::TabId;
use crate::window::WindowId;
use anyhow::Context;
use chrono::{DateTime, Duration, TimeZone, Utc};
use portable_pty::CommandBuilder;
use procinfo::LocalProcessInfo;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use url::Url;
use wakterm_term::Progress;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentMetadata {
    pub agent_id: String,
    pub name: String,
    pub launch_cmd: String,
    pub declared_cwd: String,
    #[serde(default)]
    pub adopted_pid: Option<u32>,
    #[serde(default)]
    pub adopted_start_time: Option<u64>,
    pub created_at: DateTime<Utc>,
    pub repo_root: Option<String>,
    pub worktree: Option<String>,
    pub branch: Option<String>,
    pub managed_checkout: bool,
    #[serde(default)]
    pub codex_app_server: Option<CodexAppServerSession>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CodexAppServerSession {
    pub thread_id: String,
    pub session_id: String,
    pub executable: String,
    pub version: String,
    pub tui_args: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum AgentHarness {
    Unknown,
    Claude,
    Codex,
    Gemini,
    Opencode,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum AgentTransport {
    PlainPty,
    ObservedPty,
    CodexAppServerTui,
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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentObservedTurnOutcome {
    Running,
    Completed,
    Aborted,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentObservedTurn {
    pub provider_turn_id: String,
    pub outcome: AgentObservedTurnOutcome,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub started_cursor: Option<u64>,
    pub latest_cursor: Option<u64>,
    pub primary_user_message_sha256: Option<String>,
    pub user_message_count: u32,
    pub final_message: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentOrigin {
    #[default]
    Adopted,
    Detected,
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
    #[serde(default)]
    pub observed_turn: Option<AgentObservedTurn>,
    pub observed_at: DateTime<Utc>,
    pub session_path: Option<String>,
    pub progress_summary: Option<String>,
    #[serde(default)]
    pub harness_mode: Option<String>,
    #[serde(default)]
    pub turn_phase: Option<String>,
    #[serde(default)]
    pub attention_reason: Option<String>,
    pub terminal_progress: Progress,
    pub observer_error: Option<String>,
    #[serde(skip, default)]
    pub observer_started_at: Option<DateTime<Utc>>,
    #[serde(skip, default)]
    pub last_harness_refresh_at: Option<DateTime<Utc>>,
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
            observed_turn: None,
            observed_at: now,
            session_path: None,
            progress_summary: None,
            harness_mode: None,
            turn_phase: None,
            attention_reason: None,
            terminal_progress: Progress::None,
            observer_error: None,
            observer_started_at: None,
            last_harness_refresh_at: None,
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
    #[serde(default)]
    pub origin: AgentOrigin,
    #[serde(default)]
    pub detection_source: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentProcessMatch {
    pub harness: AgentHarness,
    pub launch_cmd: String,
}

pub fn prime_runtime_for_new_agent(
    runtime: &mut AgentRuntimeSnapshot,
    metadata: &AgentMetadata,
    foreground_process_name: Option<&str>,
) {
    if metadata.codex_app_server.is_some() {
        runtime.harness = AgentHarness::Codex;
        runtime.transport = AgentTransport::CodexAppServerTui;
        runtime.harness_mode = Some("app-server-tui".to_string());
        runtime.observer_started_at = None;
        runtime.observer_error = None;
        return;
    }
    let configured_harness = infer_harness(&metadata.launch_cmd, None);
    let process_harness = infer_harness("", foreground_process_name);

    if matches!(configured_harness, AgentHarness::Unknown)
        && matches!(process_harness, AgentHarness::Unknown)
    {
        runtime.observer_started_at = None;
        return;
    }

    let preserve_existing_observer_window = runtime.last_input_at.is_some()
        || runtime.last_output_at.is_some()
        || runtime.last_progress_at.is_some();

    runtime.observer_started_at = if preserve_existing_observer_window {
        None
    } else {
        Some(metadata.created_at)
    };
    runtime.last_harness_refresh_at = None;
    runtime.session_path = None;
    runtime.progress_summary = None;
    runtime.harness_mode = None;
    runtime.turn_phase = None;
    runtime.attention_reason = None;
    runtime.turn_state = AgentTurnState::Unknown;
    runtime.last_turn_completed_at = None;
    runtime.observed_turn = None;
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
        if candidate.contains("gemini")
            || candidate.starts_with("◇ ")
            || candidate.starts_with("◆ ")
        {
            return AgentHarness::Gemini;
        }
        if candidate.contains("opencode") || candidate.starts_with("oc |") {
            return AgentHarness::Opencode;
        }
    }
    AgentHarness::Unknown
}

pub fn default_launch_cmd_for_harness(harness: &AgentHarness) -> Option<&'static str> {
    match harness {
        AgentHarness::Claude => Some("claude"),
        AgentHarness::Codex => Some("codex"),
        AgentHarness::Gemini => Some("gemini"),
        AgentHarness::Opencode => Some("opencode"),
        AgentHarness::Unknown => None,
    }
}

fn infer_harness_from_process_info(process: &LocalProcessInfo) -> AgentHarness {
    let executable = process
        .executable
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let mut values = vec![process.name.as_str(), executable];
    values.extend(process.argv.iter().map(String::as_str));
    infer_harness(&values.join(" "), Some(executable))
}

fn format_process_command(process: &LocalProcessInfo) -> Option<String> {
    if !process.argv.is_empty() {
        return Some(
            process
                .argv
                .iter()
                .map(|arg| shell_words::quote(arg))
                .collect::<Vec<_>>()
                .join(" "),
        );
    }

    let executable = process.executable.to_string_lossy();
    if executable.is_empty() {
        None
    } else {
        Some(shell_words::quote(&executable).to_string())
    }
}

fn best_harness_process(process: &LocalProcessInfo) -> Option<(u64, AgentProcessMatch)> {
    let mut best = match infer_harness_from_process_info(process) {
        AgentHarness::Unknown => None,
        harness => format_process_command(process).map(|launch_cmd| {
            (
                process.start_time,
                AgentProcessMatch {
                    harness,
                    launch_cmd,
                },
            )
        }),
    };

    for child in process.children.values() {
        if let Some(candidate) = best_harness_process(child) {
            let replace = best
                .as_ref()
                .map(|(start_time, _)| candidate.0 >= *start_time)
                .unwrap_or(true);
            if replace {
                best = Some(candidate);
            }
        }
    }

    best
}

pub fn detect_harness_process(
    process: Option<&LocalProcessInfo>,
    foreground_process_name: Option<&str>,
) -> Option<AgentProcessMatch> {
    if let Some(process) = process {
        if let Some((_, matched)) = best_harness_process(process) {
            return Some(matched);
        }
    }

    let harness = infer_harness("", foreground_process_name);
    let launch_cmd = default_launch_cmd_for_harness(&harness)?;
    Some(AgentProcessMatch {
        harness,
        launch_cmd: launch_cmd.to_string(),
    })
}

fn harness_process_is_compatible(
    configured_harness: &AgentHarness,
    process_harness: &AgentHarness,
    foreground_process_name: Option<&str>,
) -> bool {
    if matches!(configured_harness, AgentHarness::Unknown) {
        return !matches!(process_harness, AgentHarness::Unknown);
    }

    if configured_harness == process_harness {
        return true;
    }

    match configured_harness {
        // Gemini launches via a node wrapper, so the foreground process
        // name is typically just `node` rather than `gemini`.
        AgentHarness::Gemini => foreground_process_name
            .and_then(|name| Path::new(name).file_name().and_then(|name| name.to_str()))
            .map(|name| {
                matches!(
                    name.to_ascii_lowercase().as_str(),
                    "node" | "node.exe" | "bun" | "bun.exe"
                )
            })
            .unwrap_or(false),
        _ => false,
    }
}

pub fn adopted_agent_matches_process_info(
    metadata: &AgentMetadata,
    process: Option<&LocalProcessInfo>,
) -> bool {
    let Some(adopted_pid) = metadata.adopted_pid else {
        return true;
    };
    let Some(adopted_start_time) = metadata.adopted_start_time else {
        return true;
    };
    let Some(process) = process else {
        return true;
    };
    process.pid == adopted_pid && process.start_time == adopted_start_time
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

fn derive_attention_reason(runtime: &AgentRuntimeSnapshot) -> Option<String> {
    if runtime.observer_error.is_some() {
        return Some("observer-error".to_string());
    }

    if matches!(runtime.turn_phase.as_deref(), Some("aborted")) {
        return Some("turn-aborted".to_string());
    }

    if matches!(runtime.terminal_progress, Progress::Error(_)) {
        return Some("terminal-error".to_string());
    }

    if !runtime.alive && !matches!(runtime.harness, AgentHarness::Unknown) {
        return Some("exited".to_string());
    }

    None
}

pub fn refresh_runtime_from_harness(runtime: &mut AgentRuntimeSnapshot, metadata: &AgentMetadata) {
    if metadata.codex_app_server.is_some() {
        runtime.harness = AgentHarness::Codex;
        runtime.transport = AgentTransport::CodexAppServerTui;
        runtime.harness_mode = Some("app-server-tui".to_string());
        runtime.last_harness_refresh_at = Some(Utc::now());
        finalize_runtime_snapshot(runtime);
        return;
    }
    let now = Utc::now();
    runtime.last_harness_refresh_at = Some(now);
    let normalized_cwd = normalize_declared_cwd(&metadata.declared_cwd);
    let cwd = normalized_cwd.trim();
    if cwd.is_empty() {
        runtime.observed_at = now;
        runtime.turn_state = AgentTurnState::Unknown;
        runtime.last_turn_completed_at = None;
        runtime.observed_turn = None;
        finalize_runtime_snapshot(runtime);
        return;
    }

    runtime.observer_error = None;
    runtime.observed_at = now;
    let configured_harness = infer_harness(&metadata.launch_cmd, None);
    let process_harness = infer_harness("", runtime.foreground_process_name.as_deref());
    runtime.harness = match configured_harness {
        AgentHarness::Unknown => process_harness.clone(),
        _ => configured_harness.clone(),
    };

    let observing_harness = if harness_process_is_compatible(
        &configured_harness,
        &process_harness,
        runtime.foreground_process_name.as_deref(),
    ) {
        match configured_harness {
            AgentHarness::Unknown => process_harness.clone(),
            _ => configured_harness.clone(),
        }
    } else {
        runtime.session_path = None;
        runtime.progress_summary = None;
        runtime.harness_mode = None;
        runtime.turn_phase = None;
        runtime.attention_reason = None;
        runtime.turn_state = AgentTurnState::Unknown;
        runtime.last_turn_completed_at = None;
        runtime.observed_turn = None;
        runtime.transport = AgentTransport::PlainPty;
        finalize_runtime_snapshot(runtime);
        return;
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
            metadata.adopted_pid,
            metadata.adopted_start_time,
        ),
        AgentHarness::Gemini => observe_gemini(
            cwd,
            runtime.session_path.as_deref(),
            runtime.observer_started_at,
        ),
        AgentHarness::Opencode => observe_opencode(
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
            runtime.harness_mode = snapshot.harness_mode;
            runtime.turn_phase = snapshot.turn_phase;
            runtime.turn_state = snapshot.turn_state;
            runtime.last_turn_completed_at = snapshot.last_turn_completed_at;
            runtime.observed_turn = snapshot.observed_turn;
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
                runtime.harness_mode = None;
                runtime.turn_phase = None;
                runtime.attention_reason = None;
                runtime.turn_state = AgentTurnState::Unknown;
                runtime.last_turn_completed_at = None;
                runtime.observed_turn = None;
            }
        }
        Err(err) => {
            runtime.observer_error = Some(err.to_string());
            runtime.harness_mode = None;
            runtime.turn_phase = None;
            runtime.attention_reason = None;
        }
    }

    if matches!(runtime.harness, AgentHarness::Unknown) {
        runtime.harness_mode = None;
        runtime.turn_phase = None;
        runtime.attention_reason = None;
        runtime.turn_state = AgentTurnState::Unknown;
        runtime.last_turn_completed_at = None;
        runtime.observed_turn = None;
    }

    runtime.transport = if runtime.session_path.is_some() {
        AgentTransport::ObservedPty
    } else {
        AgentTransport::PlainPty
    };
    finalize_runtime_snapshot(runtime);
}

pub fn finalize_runtime_snapshot(runtime: &mut AgentRuntimeSnapshot) {
    runtime.turn_state = derive_effective_turn_state(runtime);
    runtime.status = derive_runtime_status(runtime);
    runtime.attention_reason = derive_attention_reason(runtime);
}

pub fn pending_observer_detail(
    metadata: &AgentMetadata,
    runtime: &AgentRuntimeSnapshot,
) -> Option<String> {
    if runtime.session_path.is_some()
        || runtime.observer_error.is_some()
        || runtime.attention_reason.is_some()
        || !runtime.alive
    {
        return None;
    }

    let should_describe = runtime.observer_started_at.is_some()
        || runtime.last_input_at.is_some()
        || runtime.last_output_at.is_some()
        || runtime.last_progress_at.is_some();
    if !should_describe {
        return None;
    }

    let cwd = normalize_declared_cwd(&metadata.declared_cwd);
    let cwd = cwd.trim();
    if cwd.is_empty() {
        return None;
    }

    let updated_after = runtime.observer_started_at;
    match runtime.harness {
        AgentHarness::Claude => describe_pending_claude_observer(cwd, updated_after)
            .ok()
            .flatten(),
        AgentHarness::Codex => describe_pending_codex_observer(cwd, updated_after)
            .ok()
            .flatten(),
        AgentHarness::Gemini => describe_pending_gemini_observer(cwd, updated_after)
            .ok()
            .flatten(),
        AgentHarness::Opencode => describe_pending_opencode_observer(cwd, updated_after)
            .ok()
            .flatten(),
        AgentHarness::Unknown => None,
    }
}

#[derive(Debug)]
struct HarnessObservation {
    session_path: Option<String>,
    progress_summary: Option<String>,
    harness_mode: Option<String>,
    turn_phase: Option<String>,
    updated_at: Option<DateTime<Utc>>,
    turn_state: AgentTurnState,
    last_turn_completed_at: Option<DateTime<Utc>>,
    observed_turn: Option<AgentObservedTurn>,
}

#[derive(Debug)]
struct HarnessObservationDetails {
    progress_summary: Option<String>,
    harness_mode: Option<String>,
    turn_phase: Option<String>,
    updated_at: Option<DateTime<Utc>>,
    turn_state: AgentTurnState,
    last_turn_completed_at: Option<DateTime<Utc>>,
    observed_turn: Option<AgentObservedTurn>,
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
                let details = read_last_claude_observation(preferred_path)?;
                return Ok(Some(HarnessObservation {
                    session_path: Some(preferred_path.to_string_lossy().to_string()),
                    progress_summary: details.progress_summary,
                    harness_mode: details.harness_mode,
                    turn_phase: details.turn_phase,
                    updated_at: details.updated_at.or(Some(modified_at)),
                    turn_state: details.turn_state,
                    last_turn_completed_at: details.last_turn_completed_at,
                    observed_turn: details.observed_turn,
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
    let details = read_last_claude_observation(&session)?;
    Ok(Some(HarnessObservation {
        session_path: Some(session.to_string_lossy().to_string()),
        progress_summary: details.progress_summary,
        harness_mode: details.harness_mode,
        turn_phase: details.turn_phase,
        updated_at: details.updated_at.or(Some(modified_at)),
        turn_state: details.turn_state,
        last_turn_completed_at: details.last_turn_completed_at,
        observed_turn: details.observed_turn,
    }))
}

fn observe_codex(
    cwd: &str,
    preferred_session: Option<&str>,
    updated_after: Option<DateTime<Utc>>,
    process_id: Option<u32>,
    process_start_time: Option<u64>,
) -> anyhow::Result<Option<HarnessObservation>> {
    let Some(root) = codex_sessions_root() else {
        return Ok(None);
    };

    if let Some(process_session) =
        codex_session_owned_by_process(&root, cwd, process_id, process_start_time)?
    {
        let modified_at = DateTime::<Utc>::from(fs::metadata(&process_session)?.modified()?);
        let details = read_last_codex_observation(&process_session)?;
        return Ok(Some(HarnessObservation {
            session_path: Some(process_session.to_string_lossy().to_string()),
            progress_summary: details.progress_summary,
            harness_mode: details.harness_mode,
            turn_phase: details.turn_phase,
            updated_at: details.updated_at.or(Some(modified_at)),
            turn_state: details.turn_state,
            last_turn_completed_at: details.last_turn_completed_at,
            observed_turn: details.observed_turn,
        }));
    }

    #[cfg(target_os = "linux")]
    if (process_id.is_some() || process_start_time.is_some())
        && (preferred_session.is_some() || updated_after.is_some())
    {
        // Confirmed process identity is authoritative. Falling back to a
        // preferred or cwd-matching rollout here can reattach a reused pane to
        // an old session and make unrelated output look correlated.
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
                let details = read_last_codex_observation(preferred_path)?;
                return Ok(Some(HarnessObservation {
                    session_path: Some(preferred_path.to_string_lossy().to_string()),
                    progress_summary: details.progress_summary,
                    harness_mode: details.harness_mode,
                    turn_phase: details.turn_phase,
                    updated_at: details.updated_at.or(Some(modified_at)),
                    turn_state: details.turn_state,
                    last_turn_completed_at: details.last_turn_completed_at,
                    observed_turn: details.observed_turn,
                }));
            }
        }
    }

    let prefer_earliest = updated_after.is_some();
    let mut selected: Option<(PathBuf, DateTime<Utc>)> = None;
    let mut candidates = Vec::new();
    collect_codex_rollout_sessions(&root, &mut candidates)?;
    for path in candidates {
        if !codex_session_matches_cwd(&path, cwd)? {
            continue;
        }
        let modified_at = DateTime::<Utc>::from(fs::metadata(&path)?.modified()?);
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
    let details = read_last_codex_observation(&session)?;
    Ok(Some(HarnessObservation {
        session_path: Some(session.to_string_lossy().to_string()),
        progress_summary: details.progress_summary,
        harness_mode: details.harness_mode,
        turn_phase: details.turn_phase,
        updated_at: details.updated_at.or(Some(modified_at)),
        turn_state: details.turn_state,
        last_turn_completed_at: details.last_turn_completed_at,
        observed_turn: details.observed_turn,
    }))
}

fn observe_gemini(
    cwd: &str,
    preferred_session: Option<&str>,
    updated_after: Option<DateTime<Utc>>,
) -> anyhow::Result<Option<HarnessObservation>> {
    let Some(root) = gemini_root() else {
        return Ok(None);
    };
    let project_dirs = gemini_project_dirs(&root, cwd)?;
    if project_dirs.is_empty() {
        return Ok(None);
    }

    if let Some(preferred_session) = preferred_session {
        let preferred_path = preferred_gemini_session_path(Path::new(preferred_session));
        if preferred_path.is_file() {
            let modified_at = DateTime::<Utc>::from(fs::metadata(&preferred_path)?.modified()?);
            if updated_after
                .map(|cutoff| modified_at >= cutoff)
                .unwrap_or(true)
            {
                let details = read_last_gemini_observation(&preferred_path)?;
                return Ok(Some(HarnessObservation {
                    session_path: Some(preferred_path.to_string_lossy().to_string()),
                    progress_summary: details.progress_summary,
                    harness_mode: details.harness_mode,
                    turn_phase: details.turn_phase,
                    updated_at: details.updated_at.or(Some(modified_at)),
                    turn_state: details.turn_state,
                    last_turn_completed_at: details.last_turn_completed_at,
                    observed_turn: details.observed_turn,
                }));
            }
        }
    }

    let prefer_earliest = updated_after.is_some();
    let mut selected: Option<(PathBuf, DateTime<Utc>)> = None;
    for project_dir in project_dirs {
        let chats_dir = project_dir.join("chats");
        if !chats_dir.is_dir() {
            continue;
        }

        for entry in fs::read_dir(&chats_dir)? {
            let entry = entry?;
            let path = entry.path();
            if !is_gemini_session_file(&path) {
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
    let details = read_last_gemini_observation(&session)?;
    Ok(Some(HarnessObservation {
        session_path: Some(session.to_string_lossy().to_string()),
        progress_summary: details.progress_summary,
        harness_mode: details.harness_mode,
        turn_phase: details.turn_phase,
        updated_at: details.updated_at.or(Some(modified_at)),
        turn_state: details.turn_state,
        last_turn_completed_at: details.last_turn_completed_at,
        observed_turn: details.observed_turn,
    }))
}

fn observe_opencode(
    cwd: &str,
    preferred_session: Option<&str>,
    updated_after: Option<DateTime<Utc>>,
) -> anyhow::Result<Option<HarnessObservation>> {
    let Some(db_path) = opencode_db_path() else {
        return Ok(None);
    };
    if !db_path.is_file() {
        return Ok(None);
    }

    let connection =
        Connection::open_with_flags(&db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.busy_timeout(std::time::Duration::from_secs(2))?;

    if let Some(preferred_session) = preferred_session {
        if let Some((preferred_db_path, preferred_session_id)) =
            parse_opencode_session_path(preferred_session)
        {
            if preferred_db_path == db_path {
                if let Some(observed) = read_last_opencode_observation(
                    &connection,
                    &db_path,
                    &preferred_session_id,
                    updated_after,
                )? {
                    return Ok(Some(observed));
                }
            }
        }
    }

    let Some((session_id, _)) = select_opencode_session(&connection, cwd, updated_after)? else {
        return Ok(None);
    };
    read_last_opencode_observation(&connection, &db_path, &session_id, updated_after)
}

fn describe_pending_claude_observer(
    cwd: &str,
    updated_after: Option<DateTime<Utc>>,
) -> anyhow::Result<Option<String>> {
    let Some(root) = claude_sessions_root() else {
        return Ok(None);
    };
    let project_dir = root.join(cwd.replace('/', "-"));
    if !project_dir.is_dir() {
        return Ok(Some(
            "claude project directory has not appeared yet".to_string(),
        ));
    }

    let mut has_interactive = false;
    let mut has_recent_interactive = false;
    for entry in fs::read_dir(&project_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        if !claude_session_is_interactive(&path)? {
            continue;
        }
        has_interactive = true;
        let modified_at = DateTime::<Utc>::from(entry.metadata()?.modified()?);
        if updated_after
            .map(|cutoff| modified_at >= cutoff)
            .unwrap_or(true)
        {
            has_recent_interactive = true;
            break;
        }
    }

    Ok(Some(if has_recent_interactive {
        "claude session file exists but observer has not attached yet".to_string()
    } else if has_interactive {
        "claude project directory exists but no new interactive session file appeared yet"
            .to_string()
    } else {
        "claude project directory exists but no interactive session file appeared yet".to_string()
    }))
}

fn describe_pending_codex_observer(
    cwd: &str,
    updated_after: Option<DateTime<Utc>>,
) -> anyhow::Result<Option<String>> {
    let Some(root) = codex_sessions_root() else {
        return Ok(None);
    };
    if !root.is_dir() {
        return Ok(Some(
            "codex session directory has not appeared yet".to_string(),
        ));
    }

    let mut has_matching_session = false;
    let mut has_recent_matching_session = false;
    let mut candidates = Vec::new();
    collect_codex_rollout_sessions(&root, &mut candidates)?;
    for path in candidates {
        if !codex_session_matches_cwd(&path, cwd)? {
            continue;
        }
        has_matching_session = true;
        let modified_at = DateTime::<Utc>::from(fs::metadata(&path)?.modified()?);
        if updated_after
            .map(|cutoff| modified_at >= cutoff)
            .unwrap_or(true)
        {
            has_recent_matching_session = true;
            break;
        }
    }

    Ok(Some(if has_recent_matching_session {
        "codex rollout session file exists but observer has not attached yet".to_string()
    } else if has_matching_session {
        "codex session history exists but no new rollout session file appeared yet".to_string()
    } else {
        "codex rollout session file has not appeared yet".to_string()
    }))
}

fn describe_pending_gemini_observer(
    cwd: &str,
    updated_after: Option<DateTime<Utc>>,
) -> anyhow::Result<Option<String>> {
    let Some(root) = gemini_root() else {
        return Ok(None);
    };
    let project_dirs = gemini_project_dirs(&root, cwd)?;
    if project_dirs.is_empty() {
        return Ok(Some(
            "gemini project directory has not appeared yet".to_string(),
        ));
    }

    let mut has_session = false;
    let mut has_recent_session = false;
    for project_dir in project_dirs {
        let chats_dir = project_dir.join("chats");
        if !chats_dir.is_dir() {
            continue;
        }

        for entry in fs::read_dir(&chats_dir)? {
            let entry = entry?;
            let path = entry.path();
            if !is_gemini_session_file(&path) {
                continue;
            }
            has_session = true;
            let modified_at = DateTime::<Utc>::from(entry.metadata()?.modified()?);
            if updated_after
                .map(|cutoff| modified_at >= cutoff)
                .unwrap_or(true)
            {
                has_recent_session = true;
                break;
            }
        }

        if has_recent_session {
            break;
        }
    }

    Ok(Some(if has_recent_session {
        "gemini session file exists but observer has not attached yet".to_string()
    } else if has_session {
        "gemini project directory exists but no new chat session file appeared yet".to_string()
    } else {
        "gemini project directory exists but no chat session file appeared yet".to_string()
    }))
}

fn describe_pending_opencode_observer(
    cwd: &str,
    updated_after: Option<DateTime<Utc>>,
) -> anyhow::Result<Option<String>> {
    let Some(db_path) = opencode_db_path() else {
        return Ok(None);
    };
    if !db_path.is_file() {
        return Ok(Some(
            "opencode session database has not appeared yet".to_string(),
        ));
    }

    let connection =
        Connection::open_with_flags(&db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.busy_timeout(std::time::Duration::from_secs(2))?;

    let has_recent_session = select_opencode_session(&connection, cwd, updated_after)?.is_some();
    let has_session = select_opencode_session(&connection, cwd, None)?.is_some();

    Ok(Some(if has_recent_session {
        "opencode session exists but observer has not attached yet".to_string()
    } else if has_session {
        "opencode session exists but no new turn appeared yet".to_string()
    } else {
        "opencode session has not appeared yet".to_string()
    }))
}

fn claude_sessions_root() -> Option<PathBuf> {
    std::env::var_os("WAKTERM_AGENT_CLAUDE_DIR")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join(".claude").join("projects")))
}

fn codex_sessions_root() -> Option<PathBuf> {
    std::env::var_os("WAKTERM_AGENT_CODEX_DIR")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join(".codex").join("sessions")))
}

fn gemini_root() -> Option<PathBuf> {
    std::env::var_os("WAKTERM_AGENT_GEMINI_DIR")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join(".gemini")))
}

fn opencode_db_path() -> Option<PathBuf> {
    std::env::var_os("WAKTERM_AGENT_OPENCODE_DB")
        .map(PathBuf::from)
        .or_else(|| {
            home_dir().map(|home| {
                home.join(".local")
                    .join("share")
                    .join("opencode")
                    .join("opencode.db")
            })
        })
}

pub fn codex_resume_command(launch_cmd: &str, session_id: &str) -> anyhow::Result<CommandBuilder> {
    anyhow::ensure!(
        !session_id.trim().is_empty(),
        "Codex session ID must not be empty"
    );
    let mut argv = shell_words::split(launch_cmd).context("parsing Codex launch command")?;
    anyhow::ensure!(!argv.is_empty(), "Codex launch command must not be empty");
    anyhow::ensure!(
        matches!(infer_harness(launch_cmd, None), AgentHarness::Codex),
        "Codex restore requires a Codex launch command"
    );
    argv.push("resume".to_string());
    argv.push(session_id.trim().to_string());
    Ok(CommandBuilder::from_argv(
        argv.into_iter().map(OsString::from).collect(),
    ))
}

pub(crate) fn agent_observer_watch_roots(harness: &AgentHarness, cwd: &str) -> Vec<PathBuf> {
    let paths = match harness {
        AgentHarness::Claude => claude_sessions_root().map(|root| {
            let project = root.join(cwd.replace('/', "-"));
            if project.is_dir() {
                vec![project]
            } else {
                vec![root]
            }
        }),
        AgentHarness::Codex => codex_sessions_root().map(|root| vec![root]),
        AgentHarness::Gemini => gemini_root().map(|root| {
            let project_dirs = gemini_project_dirs(&root, cwd).unwrap_or_default();
            if project_dirs.is_empty() {
                vec![root]
            } else {
                project_dirs
            }
        }),
        AgentHarness::Opencode => {
            opencode_db_path().and_then(|path| path.parent().map(|path| vec![path.to_path_buf()]))
        }
        AgentHarness::Unknown => None,
    };

    paths
        .unwrap_or_default()
        .into_iter()
        .filter(|path| path.is_dir())
        .collect()
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

pub fn codex_session_id(path: &Path) -> anyhow::Result<Option<String>> {
    let Some(first_line) = BufReader::new(fs::File::open(path)?)
        .lines()
        .next()
        .transpose()?
    else {
        return Ok(None);
    };
    let record: Value = serde_json::from_str(&first_line)
        .with_context(|| format!("parsing Codex session header {}", path.display()))?;
    let id = record
        .get("session_id")
        .or_else(|| record.get("id"))
        .or_else(|| {
            record
                .get("payload")
                .and_then(|payload| payload.get("session_id"))
        })
        .or_else(|| record.get("payload").and_then(|payload| payload.get("id")))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(ToOwned::to_owned);
    Ok(id)
}

#[cfg(target_os = "linux")]
fn codex_session_owned_by_process(
    root: &Path,
    cwd: &str,
    process_id: Option<u32>,
    process_start_time: Option<u64>,
) -> anyhow::Result<Option<PathBuf>> {
    let (Some(process_id), Some(process_start_time)) = (process_id, process_start_time) else {
        return Ok(None);
    };
    fn read_process_start_time(pid: u32) -> Option<u64> {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let (_, fields) = stat.rsplit_once(')')?;
        fields.split_whitespace().nth(19)?.parse().ok()
    }

    if read_process_start_time(process_id) != Some(process_start_time) {
        return Ok(None);
    }

    fn collect_pids(pid: u32, pids: &mut Vec<u32>) {
        pids.push(pid);
        let children_path = format!("/proc/{pid}/task/{pid}/children");
        let Ok(children) = fs::read_to_string(children_path) else {
            return;
        };
        for child in children
            .split_whitespace()
            .filter_map(|child| child.parse().ok())
        {
            collect_pids(child, pids);
        }
    }

    let canonical_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut pids = Vec::new();
    collect_pids(process_id, &mut pids);
    let mut selected: Option<(PathBuf, std::time::SystemTime)> = None;

    for pid in pids {
        let Ok(entries) = fs::read_dir(format!("/proc/{pid}/fd")) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(path) = fs::read_link(entry.path()) else {
                continue;
            };
            let canonical_path = path.canonicalize().unwrap_or(path);
            if !canonical_path.starts_with(&canonical_root)
                || !canonical_path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
                    .unwrap_or(false)
                || !codex_session_matches_cwd(&canonical_path, cwd)?
            {
                continue;
            }
            let modified = fs::metadata(&canonical_path)?.modified()?;
            if selected
                .as_ref()
                .map(|(_, current)| *current >= modified)
                .unwrap_or(false)
            {
                continue;
            }
            selected = Some((canonical_path, modified));
        }
    }

    Ok(selected.map(|(path, _)| path))
}

#[cfg(not(target_os = "linux"))]
fn codex_session_owned_by_process(
    _root: &Path,
    _cwd: &str,
    _process_id: Option<u32>,
    _process_start_time: Option<u64>,
) -> anyhow::Result<Option<PathBuf>> {
    Ok(None)
}

fn collect_codex_rollout_sessions(root: &Path, out: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    if !root.is_dir() {
        return Ok(());
    }

    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_codex_rollout_sessions(&path, out)?;
            continue;
        }

        if path
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
            .unwrap_or(false)
        {
            out.push(path);
        }
    }

    Ok(())
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
        .and_then(parse_rfc3339_timestamp)
}

fn parse_rfc3339_timestamp(ts: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn parse_unix_millis(millis: i64) -> Option<DateTime<Utc>> {
    Utc.timestamp_millis_opt(millis).single()
}

fn cwd_lookup_variants(cwd: &str) -> Vec<String> {
    let mut candidates = vec![cwd.to_string()];
    if cwd != "/" {
        let trimmed = cwd.trim_end_matches('/');
        if !trimmed.is_empty() && trimmed != cwd {
            candidates.push(trimmed.to_string());
        }
    }
    candidates
}

fn gemini_project_id(root: &Path, cwd: &str) -> anyhow::Result<Option<String>> {
    let registry_path = root.join("projects.json");
    if !registry_path.is_file() {
        return Ok(None);
    }

    let registry: Value = serde_json::from_reader(fs::File::open(registry_path)?)?;
    let Some(projects) = registry.get("projects").and_then(Value::as_object) else {
        return Ok(None);
    };

    for candidate in cwd_lookup_variants(cwd) {
        if let Some(project_id) = projects.get(&candidate).and_then(Value::as_str) {
            let project_id = project_id.trim();
            if !project_id.is_empty() {
                return Ok(Some(project_id.to_string()));
            }
        }
    }

    Ok(None)
}

fn gemini_project_dirs(root: &Path, cwd: &str) -> anyhow::Result<Vec<PathBuf>> {
    let mut dirs = vec![];
    let mut seen = std::collections::HashSet::new();
    let tmp_root = root.join("tmp");

    if let Some(project_id) = gemini_project_id(root, cwd)? {
        let path = tmp_root.join(project_id);
        if path.is_dir() && seen.insert(path.clone()) {
            dirs.push(path);
        }
    }

    if !tmp_root.is_dir() {
        return Ok(dirs);
    }

    let variants = cwd_lookup_variants(cwd);
    for entry in fs::read_dir(&tmp_root)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let Some(project_root) = gemini_project_root(&path)? else {
            continue;
        };
        if variants.iter().any(|candidate| candidate == &project_root) && seen.insert(path.clone())
        {
            dirs.push(path);
        }
    }

    Ok(dirs)
}

fn gemini_project_root(project_dir: &Path) -> anyhow::Result<Option<String>> {
    let root_file = project_dir.join(".project_root");
    if !root_file.is_file() {
        return Ok(None);
    }

    let root = fs::read_to_string(root_file)?;
    let root = root.trim().trim_end_matches('/').to_string();
    if root.is_empty() {
        Ok(None)
    } else {
        Ok(Some(root))
    }
}

fn is_gemini_session_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| {
            name.starts_with("session-") && (name.ends_with(".json") || name.ends_with(".jsonl"))
        })
        .unwrap_or(false)
}

fn preferred_gemini_session_path(path: &Path) -> PathBuf {
    if path.extension().and_then(|extension| extension.to_str()) == Some("json") {
        let migrated = PathBuf::from(format!("{}l", path.to_string_lossy()));
        if migrated.is_file() {
            return migrated;
        }
    }
    path.to_path_buf()
}

#[derive(Clone, Debug)]
pub(crate) struct GeminiConversation {
    pub session_id: Option<String>,
    pub last_updated: Option<DateTime<Utc>>,
    pub messages: Vec<Value>,
}

pub(crate) fn read_gemini_conversation(path: &Path) -> anyhow::Result<GeminiConversation> {
    if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
        let record: Value = serde_json::from_reader(fs::File::open(path)?)?;
        return Ok(GeminiConversation {
            session_id: record
                .get("sessionId")
                .and_then(Value::as_str)
                .map(str::to_string),
            last_updated: record
                .get("lastUpdated")
                .and_then(Value::as_str)
                .and_then(parse_rfc3339_timestamp),
            messages: record
                .get("messages")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
        });
    }

    const MAX_GEMINI_RECORD_BYTES: u64 = 4 * 1024 * 1024;
    let mut reader = BufReader::new(fs::File::open(path)?);
    let mut line = Vec::new();
    let mut session_id = None;
    let mut last_updated = None;
    let mut messages = Vec::<Value>::new();
    let mut message_index = std::collections::HashMap::<String, usize>::new();

    loop {
        line.clear();
        let read = reader
            .by_ref()
            .take(MAX_GEMINI_RECORD_BYTES + 1)
            .read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        anyhow::ensure!(
            read as u64 <= MAX_GEMINI_RECORD_BYTES,
            "Gemini session record exceeds the {MAX_GEMINI_RECORD_BYTES}-byte bound"
        );
        if !line.ends_with(b"\n") {
            break;
        }
        let record: Value = serde_json::from_slice(&line)?;
        let metadata = record.get("$set").unwrap_or(&record);
        if let Some(value) = metadata
            .get("sessionId")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            session_id = Some(value.to_string());
        }
        if let Some(value) = metadata
            .get("lastUpdated")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339_timestamp)
        {
            last_updated = Some(value);
        }

        let Some(id) = record
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
        if record.get("type").and_then(Value::as_str).is_none() {
            continue;
        }
        if let Some(index) = message_index.get(id).copied() {
            messages[index] = record;
        } else {
            message_index.insert(id.to_string(), messages.len());
            messages.push(record);
        }
    }

    Ok(GeminiConversation {
        session_id,
        last_updated,
        messages,
    })
}

fn extract_message_text(content: &Value) -> Option<String> {
    if let Some(text) = content.as_str() {
        let text = text.trim();
        if !text.is_empty() {
            return Some(text.to_string());
        }
        return None;
    }

    let Some(blocks) = content.as_array() else {
        return None;
    };
    let mut parts = vec![];
    for block in blocks {
        if let Some(text) = block.get("text").and_then(Value::as_str) {
            let text = text.trim();
            if !text.is_empty() {
                parts.push(text.to_string());
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

fn read_last_gemini_observation(path: &Path) -> anyhow::Result<HarnessObservationDetails> {
    let conversation = read_gemini_conversation(path)?;
    let mut summary = None;
    let mut last_user_at = None;
    let mut last_assistant_at = None;
    for message in &conversation.messages {
        let timestamp = message
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339_timestamp);
        match message.get("type").and_then(Value::as_str) {
            Some("user") => {
                last_user_at = timestamp.or(last_user_at);
            }
            Some("gemini") => {
                last_assistant_at = timestamp.or(last_assistant_at);
                if let Some(content) = message.get("content").and_then(extract_message_text) {
                    summary = Some(truncate_summary(&content));
                }
            }
            _ => {}
        }
    }

    let (turn_state, last_turn_completed_at) = derive_turn_state(last_user_at, last_assistant_at);
    Ok(HarnessObservationDetails {
        progress_summary: summary,
        harness_mode: None,
        turn_phase: None,
        updated_at: conversation
            .last_updated
            .or(last_assistant_at)
            .or(last_user_at),
        turn_state,
        last_turn_completed_at,
        observed_turn: None,
    })
}

fn encode_opencode_session_path(db_path: &Path, session_id: &str) -> String {
    let mut url = Url::parse("opencode://session").expect("static opencode url is valid");
    url.query_pairs_mut()
        .append_pair("db", &db_path.to_string_lossy())
        .append_pair("id", session_id);
    url.to_string()
}

fn parse_opencode_session_path(value: &str) -> Option<(PathBuf, String)> {
    let url = Url::parse(value).ok()?;
    if url.scheme() != "opencode" {
        return None;
    }

    let mut db_path = None;
    let mut session_id = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "db" => db_path = Some(PathBuf::from(value.into_owned())),
            "id" => session_id = Some(value.into_owned()),
            _ => {}
        }
    }
    Some((db_path?, session_id?))
}

fn select_opencode_session(
    connection: &Connection,
    cwd: &str,
    updated_after: Option<DateTime<Utc>>,
) -> anyhow::Result<Option<(String, i64)>> {
    let prefer_earliest = updated_after.is_some();
    let mut selected = None;

    for candidate in cwd_lookup_variants(cwd) {
        let row = if let Some(cutoff) = updated_after {
            let cutoff_millis = cutoff.timestamp_millis();
            let sql = if prefer_earliest {
                "SELECT id, time_updated FROM session \
                 WHERE directory = ?1 AND time_updated >= ?2 \
                 ORDER BY time_updated ASC LIMIT 1"
            } else {
                "SELECT id, time_updated FROM session \
                 WHERE directory = ?1 AND time_updated >= ?2 \
                 ORDER BY time_updated DESC LIMIT 1"
            };
            connection
                .query_row(sql, params![candidate, cutoff_millis], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })
                .optional()?
        } else {
            let sql = if prefer_earliest {
                "SELECT id, time_updated FROM session \
                 WHERE directory = ?1 ORDER BY time_updated ASC LIMIT 1"
            } else {
                "SELECT id, time_updated FROM session \
                 WHERE directory = ?1 ORDER BY time_updated DESC LIMIT 1"
            };
            connection
                .query_row(sql, params![candidate], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })
                .optional()?
        };

        let Some((session_id, updated_millis)) = row else {
            continue;
        };
        match &selected {
            Some((_, existing_updated_millis))
                if (prefer_earliest && *existing_updated_millis <= updated_millis)
                    || (!prefer_earliest && *existing_updated_millis >= updated_millis) => {}
            _ => selected = Some((session_id, updated_millis)),
        }
    }

    Ok(selected)
}

fn read_last_opencode_observation(
    connection: &Connection,
    db_path: &Path,
    session_id: &str,
    updated_after: Option<DateTime<Utc>>,
) -> anyhow::Result<Option<HarnessObservation>> {
    let Some(updated_millis) = connection
        .query_row(
            "SELECT time_updated FROM session WHERE id = ?1",
            params![session_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
    else {
        return Ok(None);
    };

    if updated_after
        .map(|cutoff| updated_millis < cutoff.timestamp_millis())
        .unwrap_or(false)
    {
        return Ok(None);
    }

    let mut last_user_at = None;
    let mut last_assistant_at = None;
    let mut message_stmt = connection.prepare(
        "SELECT time_created, data \
         FROM message \
         WHERE session_id = ?1 \
         ORDER BY time_created DESC, rowid DESC",
    )?;
    let mut message_rows = message_stmt.query(params![session_id])?;
    while let Some(row) = message_rows.next()? {
        let time_created = row.get::<_, i64>(0)?;
        let message_data = row.get::<_, String>(1)?;
        let Ok(message) = serde_json::from_str::<Value>(&message_data) else {
            continue;
        };
        let Some(role) = message.get("role").and_then(Value::as_str) else {
            continue;
        };
        let timestamp = parse_unix_millis(time_created);
        match role {
            "user" if last_user_at.is_none() => last_user_at = timestamp,
            "assistant" if last_assistant_at.is_none() => last_assistant_at = timestamp,
            _ => {}
        }
        if last_user_at.is_some() && last_assistant_at.is_some() {
            break;
        }
    }

    let mut summary = None;
    let mut part_stmt = connection.prepare(
        "SELECT p.data, m.data \
         FROM part p \
         JOIN message m ON p.message_id = m.id \
         WHERE p.session_id = ?1 \
         ORDER BY p.rowid DESC",
    )?;
    let mut part_rows = part_stmt.query(params![session_id])?;
    while let Some(row) = part_rows.next()? {
        let part_data = row.get::<_, String>(0)?;
        let message_data = row.get::<_, String>(1)?;
        let Ok(part) = serde_json::from_str::<Value>(&part_data) else {
            continue;
        };
        let Ok(message) = serde_json::from_str::<Value>(&message_data) else {
            continue;
        };
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        if part.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            let text = text.trim();
            if !text.is_empty() {
                summary = Some(truncate_summary(text));
                break;
            }
        }
    }

    let (turn_state, last_turn_completed_at) = derive_turn_state(last_user_at, last_assistant_at);
    Ok(Some(HarnessObservation {
        session_path: Some(encode_opencode_session_path(db_path, session_id)),
        progress_summary: summary,
        harness_mode: None,
        turn_phase: None,
        updated_at: parse_unix_millis(updated_millis),
        turn_state,
        last_turn_completed_at,
        observed_turn: None,
    }))
}

fn read_last_claude_observation(path: &Path) -> anyhow::Result<HarnessObservationDetails> {
    let reader = BufReader::new(fs::File::open(path)?);
    let mut summary = None;
    let mut harness_mode = None;
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
                            harness_mode = Some("plan".to_string());
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
    Ok(HarnessObservationDetails {
        progress_summary: summary,
        harness_mode,
        turn_phase: None,
        updated_at: None,
        turn_state,
        last_turn_completed_at,
        observed_turn: None,
    })
}

fn visit_lines_reverse(
    path: &Path,
    mut visitor: impl FnMut(&str) -> anyhow::Result<bool>,
) -> anyhow::Result<()> {
    const CHUNK_SIZE: usize = 64 * 1024;

    let mut file = fs::File::open(path)?;
    let mut pos = file.seek(SeekFrom::End(0))?;
    let mut tail = Vec::new();

    while pos > 0 {
        let read_len = CHUNK_SIZE.min(pos as usize);
        pos -= read_len as u64;
        file.seek(SeekFrom::Start(pos))?;

        let mut chunk = vec![0u8; read_len];
        file.read_exact(&mut chunk)?;
        chunk.extend_from_slice(&tail);

        let mut end = chunk.len();
        while let Some(idx) = chunk[..end].iter().rposition(|&byte| byte == b'\n') {
            let mut line = &chunk[idx + 1..end];
            if line.last() == Some(&b'\r') {
                line = &line[..line.len() - 1];
            }
            if !line.is_empty() {
                if let Ok(line) = std::str::from_utf8(line) {
                    if visitor(line)? {
                        return Ok(());
                    }
                }
            }
            end = idx;
        }

        tail.clear();
        tail.extend_from_slice(&chunk[..end]);
    }

    if tail.last() == Some(&b'\r') {
        tail.pop();
    }
    if !tail.is_empty() {
        if let Ok(line) = std::str::from_utf8(&tail) {
            visitor(line)?;
        }
    }

    Ok(())
}

fn codex_record_turn_id(record: &Value) -> Option<&str> {
    let payload = record.get("payload")?;
    payload.get("turn_id").and_then(Value::as_str).or_else(|| {
        payload
            .get("internal_chat_message_metadata_passthrough")
            .and_then(|metadata| metadata.get("turn_id"))
            .and_then(Value::as_str)
    })
}

fn codex_record_cursor(record: &Value) -> Option<u64> {
    record.get("ordinal").and_then(Value::as_u64)
}

fn codex_response_message_text(payload: &Value) -> Option<String> {
    let content = payload.get("content")?.as_array()?;
    let parts = content
        .iter()
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    (!parts.is_empty()).then(|| parts.join("\n"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CodexOutputMessage {
    pub start_offset: u64,
    pub end_offset: u64,
    pub record_sha256: String,
    pub turn_id: Option<String>,
    pub timestamp: Option<DateTime<Utc>>,
    pub text: String,
}

const MAX_CODEX_OUTPUT_RECORDS_PER_READ: usize = 1024;
const MAX_CODEX_OUTPUT_BYTES_PER_READ: u64 = 1024 * 1024;

fn codex_assistant_output_text(record: &Value) -> Option<String> {
    if record.get("type").and_then(Value::as_str) != Some("response_item") {
        return None;
    }
    let payload = record.get("payload")?;
    if payload.get("type").and_then(Value::as_str) != Some("message")
        || payload.get("role").and_then(Value::as_str) != Some("assistant")
    {
        return None;
    }
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

pub(crate) fn codex_complete_tail_offset(path: &Path) -> anyhow::Result<u64> {
    const CHUNK_SIZE: usize = 64 * 1024;

    let mut file = fs::File::open(path)?;
    let len = file.seek(SeekFrom::End(0))?;
    if len == 0 {
        return Ok(0);
    }

    let mut pos = len;
    while pos > 0 {
        let read_len = CHUNK_SIZE.min(pos as usize);
        pos -= read_len as u64;
        file.seek(SeekFrom::Start(pos))?;
        let mut chunk = vec![0; read_len];
        file.read_exact(&mut chunk)?;
        if let Some(index) = chunk.iter().rposition(|byte| *byte == b'\n') {
            return Ok(pos + index as u64 + 1);
        }
    }
    Ok(0)
}

pub(crate) fn read_codex_output_messages(
    path: &Path,
    offset: u64,
    limit: usize,
) -> anyhow::Result<(Vec<CodexOutputMessage>, u64, bool)> {
    let complete_tail = codex_complete_tail_offset(path)?;
    anyhow::ensure!(
        offset <= complete_tail,
        "cursor offset {offset} is past the complete Codex session tail {complete_tail}"
    );

    let mut file = fs::File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut reader = BufReader::new(file.take(complete_tail - offset));
    let mut messages = Vec::new();
    let mut next_offset = offset;
    let limit = limit.max(1);
    let mut records_read = 0;

    loop {
        if records_read >= MAX_CODEX_OUTPUT_RECORDS_PER_READ
            || (records_read > 0
                && next_offset.saturating_sub(offset) >= MAX_CODEX_OUTPUT_BYTES_PER_READ)
        {
            break;
        }
        let start_offset = next_offset;
        let remaining_bytes =
            MAX_CODEX_OUTPUT_BYTES_PER_READ.saturating_sub(next_offset.saturating_sub(offset));
        let mut line = Vec::new();
        let read = reader
            .by_ref()
            .take(remaining_bytes + 1)
            .read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        if read as u64 > remaining_bytes || line.last() != Some(&b'\n') {
            if records_read > 0 {
                break;
            }
            anyhow::bail!(
                "Codex output record at offset {start_offset} exceeds the {} byte read bound",
                MAX_CODEX_OUTPUT_BYTES_PER_READ
            );
        }
        records_read += 1;
        next_offset += read as u64;
        while matches!(line.last(), Some(b'\n' | b'\r')) {
            line.pop();
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let Ok(record) = serde_json::from_slice::<Value>(&line) else {
            // Match Panetone's existing behavior for a malformed complete
            // record: skip it and advance. Incomplete records are excluded by
            // complete_tail and are retried on the next read.
            continue;
        };
        let Some(text) = codex_assistant_output_text(&record) else {
            continue;
        };
        messages.push(CodexOutputMessage {
            start_offset,
            end_offset: next_offset,
            record_sha256: format!("{:x}", Sha256::digest(&line)),
            turn_id: codex_record_turn_id(&record).map(str::to_string),
            timestamp: parse_record_timestamp(&record),
            text,
        });
        if messages.len() >= limit {
            break;
        }
    }

    Ok((messages, next_offset, next_offset < complete_tail))
}

fn message_sha256(message: &str) -> String {
    format!("{:x}", Sha256::digest(message.trim().as_bytes()))
}

fn read_last_codex_observation(path: &Path) -> anyhow::Result<HarnessObservationDetails> {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum CodexTaskLifecycle {
        Running,
        Completed,
        Aborted,
    }

    let mut summary = None;
    let mut summary_fallback = None;
    let mut harness_mode = None;
    let mut turn_phase = None;
    let mut saw_task_started = false;
    let mut saw_task_complete = false;
    let mut last_user_at = None;
    let mut last_assistant_at = None;
    let mut last_lifecycle = None;
    let mut last_lifecycle_at = None;
    let mut current_turn_id = None;
    let mut current_turn_latest_cursor = None;
    let mut current_turn_started_cursor = None;
    let mut current_turn_started_at = None;
    let mut current_turn_completed_at = None;
    let mut current_turn_primary_user_sha256 = None;
    let mut current_turn_user_message_count = 0_u32;
    let mut current_turn_last_assistant_message = None;
    let mut saw_current_turn_start = false;
    visit_lines_reverse(path, |line| {
        let Ok(record) = serde_json::from_str::<Value>(&line) else {
            return Ok(false);
        };
        let record_turn_id = codex_record_turn_id(&record);
        if current_turn_id.is_none() {
            current_turn_id = record_turn_id.map(str::to_string);
            current_turn_latest_cursor = codex_record_cursor(&record);
        }
        let belongs_to_current_turn = match (current_turn_id.as_deref(), record_turn_id) {
            (Some(current), Some(record)) => current == record,
            (None, _) => true,
            _ => false,
        };
        match record.get("type").and_then(Value::as_str) {
            Some("turn_context") => {
                if harness_mode.is_none() {
                    if let Some(mode) = record
                        .get("payload")
                        .and_then(|payload| payload.get("collaboration_mode"))
                        .and_then(|mode| mode.get("mode"))
                        .and_then(Value::as_str)
                    {
                        let mode = mode.trim();
                        if !mode.is_empty() {
                            harness_mode = Some(mode.to_string());
                        }
                    }
                }
            }
            Some("response_item") => {
                let Some(payload) = record.get("payload") else {
                    return Ok(false);
                };
                if payload.get("type").and_then(Value::as_str) != Some("message") {
                    return Ok(false);
                }
                match payload.get("role").and_then(Value::as_str) {
                    Some("assistant") => {
                        if last_assistant_at.is_none() {
                            last_assistant_at = parse_record_timestamp(&record);
                        }
                        if belongs_to_current_turn && current_turn_last_assistant_message.is_none()
                        {
                            current_turn_last_assistant_message =
                                codex_response_message_text(payload);
                        }
                        if summary.is_none() {
                            let Some(content) = payload.get("content").and_then(Value::as_array)
                            else {
                                return Ok(false);
                            };
                            let mut parts = vec![];
                            for block in content {
                                if block.get("type").and_then(Value::as_str) == Some("output_text")
                                {
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
                    }
                    Some("user") => {
                        if last_user_at.is_none() {
                            last_user_at = parse_record_timestamp(&record);
                        }
                        if belongs_to_current_turn {
                            if let Some(message) = codex_response_message_text(payload) {
                                current_turn_primary_user_sha256 = Some(message_sha256(&message));
                                current_turn_user_message_count += 1;
                            }
                        }
                    }
                    _ => {}
                }
            }
            Some("event_msg") => {
                let Some(payload) = record.get("payload") else {
                    return Ok(false);
                };
                match payload.get("type").and_then(Value::as_str) {
                    Some("user_message") => {
                        if last_user_at.is_none() {
                            last_user_at = parse_record_timestamp(&record);
                        }
                    }
                    Some("task_started") => {
                        if !belongs_to_current_turn {
                            return Ok(false);
                        }
                        saw_task_started = true;
                        saw_current_turn_start = current_turn_id.is_some();
                        current_turn_started_cursor = codex_record_cursor(&record);
                        current_turn_started_at = parse_record_timestamp(&record);
                        if last_lifecycle.is_none() {
                            last_lifecycle = Some(CodexTaskLifecycle::Running);
                        }
                        if last_lifecycle_at.is_none() {
                            last_lifecycle_at = parse_record_timestamp(&record);
                        }
                        if harness_mode.is_none() {
                            if let Some(mode) = payload
                                .get("collaboration_mode_kind")
                                .and_then(Value::as_str)
                            {
                                let mode = mode.trim();
                                if !mode.is_empty() {
                                    harness_mode = Some(mode.to_string());
                                }
                            }
                        }
                    }
                    Some("agent_message") => {
                        if last_assistant_at.is_none() {
                            last_assistant_at = parse_record_timestamp(&record);
                        }
                        if turn_phase.is_none() {
                            if let Some(phase) = payload.get("phase").and_then(Value::as_str) {
                                let phase = phase.trim();
                                if !phase.is_empty() {
                                    turn_phase = Some(phase.to_string());
                                }
                            }
                        }
                    }
                    Some("task_complete") => {
                        if !belongs_to_current_turn {
                            return Ok(false);
                        }
                        saw_task_complete = true;
                        current_turn_completed_at = parse_record_timestamp(&record);
                        if last_lifecycle.is_none() {
                            last_lifecycle = Some(CodexTaskLifecycle::Completed);
                        }
                        if last_lifecycle_at.is_none() {
                            last_lifecycle_at = parse_record_timestamp(&record);
                        }
                        if last_assistant_at.is_none() {
                            last_assistant_at = parse_record_timestamp(&record);
                        }
                        if summary.is_none() && summary_fallback.is_none() {
                            if let Some(last_message) =
                                payload.get("last_agent_message").and_then(Value::as_str)
                            {
                                let last_message = last_message.trim();
                                if !last_message.is_empty() {
                                    summary_fallback = Some(truncate_summary(last_message));
                                }
                            }
                        }
                    }
                    Some("turn_aborted") => {
                        if !belongs_to_current_turn {
                            return Ok(false);
                        }
                        current_turn_completed_at = parse_record_timestamp(&record);
                        if last_lifecycle.is_none() {
                            last_lifecycle = Some(CodexTaskLifecycle::Aborted);
                        }
                        if last_lifecycle_at.is_none() {
                            last_lifecycle_at = parse_record_timestamp(&record);
                        }
                        if turn_phase.is_none() {
                            turn_phase = Some("aborted".to_string());
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }

        let phase_settled = turn_phase.is_some() || saw_task_started;
        let summary_settled = summary.is_some();
        let harness_mode_settled = harness_mode.is_some();
        let turn_settled = match last_lifecycle {
            Some(CodexTaskLifecycle::Running) => true,
            Some(CodexTaskLifecycle::Completed) | Some(CodexTaskLifecycle::Aborted) => {
                last_lifecycle_at.is_some()
            }
            None => false,
        };
        Ok(if current_turn_id.is_some() {
            saw_current_turn_start
        } else {
            summary_settled && harness_mode_settled && phase_settled && turn_settled
        })
    })?;

    if summary.is_none() {
        summary = summary_fallback;
    }
    if turn_phase.is_none() {
        if saw_task_started {
            turn_phase = Some("started".to_string());
        } else if saw_task_complete {
            turn_phase = Some("complete".to_string());
        }
    }

    let (mut turn_state, mut last_turn_completed_at) =
        derive_turn_state(last_user_at, last_assistant_at);
    match last_lifecycle {
        Some(CodexTaskLifecycle::Running) => {
            turn_state = AgentTurnState::WaitingOnAgent;
            last_turn_completed_at = None;
        }
        Some(CodexTaskLifecycle::Completed) | Some(CodexTaskLifecycle::Aborted) => {
            turn_state = AgentTurnState::WaitingOnUser;
            last_turn_completed_at = last_lifecycle_at.or(last_turn_completed_at);
        }
        None => {}
    }
    let observed_turn = current_turn_id.map(|provider_turn_id| AgentObservedTurn {
        provider_turn_id,
        outcome: match last_lifecycle {
            Some(CodexTaskLifecycle::Completed) => AgentObservedTurnOutcome::Completed,
            Some(CodexTaskLifecycle::Aborted) => AgentObservedTurnOutcome::Aborted,
            Some(CodexTaskLifecycle::Running) | None => AgentObservedTurnOutcome::Running,
        },
        started_at: current_turn_started_at,
        completed_at: current_turn_completed_at,
        started_cursor: current_turn_started_cursor,
        latest_cursor: current_turn_latest_cursor,
        primary_user_message_sha256: current_turn_primary_user_sha256,
        user_message_count: current_turn_user_message_count,
        final_message: matches!(last_lifecycle, Some(CodexTaskLifecycle::Completed))
            .then_some(current_turn_last_assistant_message)
            .flatten(),
    });
    Ok(HarnessObservationDetails {
        progress_summary: summary,
        harness_mode,
        turn_phase,
        updated_at: None,
        turn_state,
        last_turn_completed_at,
        observed_turn,
    })
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
    use chrono::{Datelike, TimeZone};
    use std::collections::HashMap;
    use std::path::PathBuf;
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

    fn proc_info(
        name: &str,
        executable: &str,
        argv: &[&str],
        start_time: u64,
        children: Vec<LocalProcessInfo>,
    ) -> LocalProcessInfo {
        LocalProcessInfo {
            pid: start_time as u32,
            ppid: 0,
            name: name.to_string(),
            executable: PathBuf::from(executable),
            argv: argv.iter().map(|arg| (*arg).to_string()).collect(),
            cwd: PathBuf::from("/tmp"),
            status: procinfo::LocalProcessStatus::Run,
            start_time,
            #[cfg(windows)]
            console: 0,
            children: children
                .into_iter()
                .map(|child| (child.pid, child))
                .collect::<HashMap<_, _>>(),
        }
    }

    fn create_opencode_test_db(path: &Path) -> Connection {
        let connection = Connection::open(path).unwrap();
        connection
            .execute_batch(
                "
                CREATE TABLE session (
                    id TEXT PRIMARY KEY,
                    directory TEXT NOT NULL,
                    time_updated INTEGER NOT NULL
                );
                CREATE TABLE message (
                    id TEXT PRIMARY KEY,
                    session_id TEXT NOT NULL,
                    time_created INTEGER NOT NULL,
                    data TEXT NOT NULL
                );
                CREATE TABLE part (
                    id TEXT PRIMARY KEY,
                    message_id TEXT NOT NULL,
                    session_id TEXT NOT NULL,
                    time_created INTEGER NOT NULL,
                    data TEXT NOT NULL
                );
                ",
            )
            .unwrap();
        connection
    }

    #[test]
    fn infers_harness_from_launch_command_or_foreground_process() {
        assert_eq!(
            infer_harness("codex --model gpt-5", None),
            AgentHarness::Codex
        );
        assert_eq!(infer_harness("gemini --yolo", None), AgentHarness::Gemini);
        assert_eq!(
            infer_harness("◇  Ready (wakterm)", None),
            AgentHarness::Gemini
        );
        assert_eq!(
            infer_harness("opencode serve", None),
            AgentHarness::Opencode
        );
        assert_eq!(
            infer_harness("OC | Casual greeting", None),
            AgentHarness::Opencode
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
    fn detects_harness_process_from_process_tree() {
        let process = proc_info(
            "zsh",
            "/usr/bin/zsh",
            &["zsh"],
            1,
            vec![proc_info(
                "codex",
                "/usr/bin/codex",
                &["codex", "-a", "never"],
                2,
                vec![],
            )],
        );

        let matched = detect_harness_process(Some(&process), Some("/usr/bin/zsh")).unwrap();
        assert_eq!(matched.harness, AgentHarness::Codex);
        assert_eq!(matched.launch_cmd, "codex -a never");
    }

    #[test]
    fn falls_back_to_foreground_process_name_for_detection() {
        let matched = detect_harness_process(None, Some("/usr/bin/claude")).unwrap();
        assert_eq!(matched.harness, AgentHarness::Claude);
        assert_eq!(matched.launch_cmd, "claude");
    }

    #[test]
    fn treats_gemini_node_wrapper_as_compatible_foreground_process() {
        assert!(harness_process_is_compatible(
            &AgentHarness::Gemini,
            &AgentHarness::Unknown,
            Some("/home/mihai/.nvm/versions/node/v22.14.0/bin/node"),
        ));
        assert!(!harness_process_is_compatible(
            &AgentHarness::Codex,
            &AgentHarness::Unknown,
            Some("/home/mihai/.nvm/versions/node/v22.14.0/bin/node"),
        ));
    }

    #[test]
    fn derives_runtime_status_from_liveness_progress_and_recent_activity() {
        let metadata = AgentMetadata {
            agent_id: "id".to_string(),
            name: "alpha".to_string(),
            launch_cmd: "codex".to_string(),
            declared_cwd: "/tmp/alpha".to_string(),
            adopted_pid: None,
            adopted_start_time: None,
            created_at: Utc::now(),
            repo_root: None,
            worktree: None,
            branch: None,
            managed_checkout: false,
            codex_app_server: None,
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
    fn derives_attention_reason_from_runtime_state() {
        let metadata = AgentMetadata {
            agent_id: "id".to_string(),
            name: "alpha".to_string(),
            launch_cmd: "codex".to_string(),
            declared_cwd: "/tmp/alpha".to_string(),
            adopted_pid: None,
            adopted_start_time: None,
            created_at: Utc::now(),
            repo_root: None,
            worktree: None,
            branch: None,
            managed_checkout: false,
            codex_app_server: None,
        };
        let mut runtime = AgentRuntimeSnapshot::new(&metadata);
        runtime.harness = AgentHarness::Codex;
        runtime.turn_phase = Some("aborted".to_string());
        assert_eq!(
            derive_attention_reason(&runtime).as_deref(),
            Some("turn-aborted")
        );

        runtime.turn_phase = None;
        runtime.observer_error = Some("bad parse".to_string());
        assert_eq!(
            derive_attention_reason(&runtime).as_deref(),
            Some("observer-error")
        );

        runtime.observer_error = None;
        runtime.terminal_progress = Progress::Error(1);
        assert_eq!(
            derive_attention_reason(&runtime).as_deref(),
            Some("terminal-error")
        );

        runtime.terminal_progress = Progress::None;
        runtime.alive = false;
        assert_eq!(derive_attention_reason(&runtime).as_deref(), Some("exited"));
    }

    #[test]
    fn pending_observer_detail_reports_gemini_project_without_chat_session() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let temp = TempDir::new().unwrap();
        let project_dir = temp.path().join("tmp").join("project-m");
        fs::create_dir_all(&project_dir).unwrap();
        fs::write(project_dir.join(".project_root"), "/tmp/project-m\n").unwrap();

        set_env_path("WAKTERM_AGENT_GEMINI_DIR", temp.path());
        let metadata = AgentMetadata {
            agent_id: "id".to_string(),
            name: "gemini".to_string(),
            launch_cmd: "gemini".to_string(),
            declared_cwd: "/tmp/project-m".to_string(),
            adopted_pid: None,
            adopted_start_time: None,
            created_at: Utc::now(),
            repo_root: None,
            worktree: None,
            branch: None,
            managed_checkout: false,
            codex_app_server: None,
        };
        let mut runtime = AgentRuntimeSnapshot::new(&metadata);
        runtime.harness = AgentHarness::Gemini;
        runtime.status = AgentStatus::Starting;
        runtime.observer_started_at = Some(Utc::now());

        let detail = pending_observer_detail(&metadata, &runtime);
        remove_env_var("WAKTERM_AGENT_GEMINI_DIR");

        assert_eq!(
            detail.as_deref(),
            Some("gemini project directory exists but no chat session file appeared yet")
        );
    }

    #[test]
    fn pending_observer_detail_stays_quiet_for_idle_plain_pty_agents() {
        let metadata = AgentMetadata {
            agent_id: "id".to_string(),
            name: "alpha".to_string(),
            launch_cmd: "codex".to_string(),
            declared_cwd: "/tmp/project-n".to_string(),
            adopted_pid: None,
            adopted_start_time: None,
            created_at: Utc::now(),
            repo_root: None,
            worktree: None,
            branch: None,
            managed_checkout: false,
            codex_app_server: None,
        };
        let runtime = AgentRuntimeSnapshot::new(&metadata);

        assert_eq!(pending_observer_detail(&metadata, &runtime), None);
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

        set_env_path("WAKTERM_AGENT_CLAUDE_DIR", temp.path());
        let observed = observe_claude(cwd, None, None).unwrap().unwrap();
        remove_env_var("WAKTERM_AGENT_CLAUDE_DIR");

        assert_eq!(observed.progress_summary.as_deref(), Some("done"));
        assert_eq!(observed.harness_mode, None);
        assert_eq!(observed.turn_phase, None);
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
                "{\"type\":\"event_msg\",\"timestamp\":\"2026-03-17T12:00:00Z\",\"payload\":{\"type\":\"task_started\",\"collaboration_mode_kind\":\"default\"}}\n",
                "{\"type\":\"turn_context\",\"timestamp\":\"2026-03-17T12:00:00Z\",\"payload\":{\"collaboration_mode\":{\"mode\":\"plan\"}}}\n",
                "{\"type\":\"response_item\",\"timestamp\":\"2026-03-17T12:00:00Z\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[]}}\n",
                "{\"type\":\"event_msg\",\"timestamp\":\"2026-03-17T12:00:02Z\",\"payload\":{\"type\":\"agent_message\",\"phase\":\"final_answer\"}}\n",
                "{\"type\":\"response_item\",\"timestamp\":\"2026-03-17T12:00:03Z\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"all good\"}]}}\n",
                "{\"type\":\"event_msg\",\"timestamp\":\"2026-03-17T12:00:04Z\",\"payload\":{\"type\":\"task_complete\",\"last_agent_message\":\"all good\"}}\n"
            ),
        )
        .unwrap();

        set_env_path("WAKTERM_AGENT_CODEX_DIR", temp.path());
        let observed = observe_codex("/tmp/project-b", None, None, None, None)
            .unwrap()
            .unwrap();
        remove_env_var("WAKTERM_AGENT_CODEX_DIR");

        assert_eq!(observed.progress_summary.as_deref(), Some("all good"));
        assert_eq!(observed.harness_mode.as_deref(), Some("plan"));
        assert_eq!(observed.turn_phase.as_deref(), Some("final_answer"));
        assert_eq!(
            observed.session_path.as_deref(),
            Some(session.to_string_lossy().as_ref())
        );
        assert_eq!(observed.turn_state, AgentTurnState::WaitingOnUser);
        assert_eq!(
            observed.last_turn_completed_at,
            Some(Utc.with_ymd_and_hms(2026, 3, 17, 12, 0, 4).unwrap())
        );
    }

    #[test]
    fn observes_stable_codex_turn_identity_and_full_final_message() {
        let temp = TempDir::new().unwrap();
        let session = temp.path().join("rollout-turn.jsonl");
        fs::write(
            &session,
            concat!(
                "{\"ordinal\":10,\"type\":\"event_msg\",\"timestamp\":\"2026-03-17T12:00:00Z\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"turn-2\"}}\n",
                "{\"ordinal\":11,\"type\":\"turn_context\",\"payload\":{\"turn_id\":\"turn-2\",\"collaboration_mode\":{\"mode\":\"default\"}}}\n",
                "{\"ordinal\":12,\"type\":\"response_item\",\"timestamp\":\"2026-03-17T12:00:01Z\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"do the work\"}],\"internal_chat_message_metadata_passthrough\":{\"turn_id\":\"turn-2\"}}}\n",
                "{\"ordinal\":13,\"type\":\"response_item\",\"timestamp\":\"2026-03-17T12:00:02Z\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"working\"}],\"internal_chat_message_metadata_passthrough\":{\"turn_id\":\"turn-2\"}}}\n",
                "{\"ordinal\":14,\"type\":\"response_item\",\"timestamp\":\"2026-03-17T12:00:03Z\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"complete final response\"}],\"internal_chat_message_metadata_passthrough\":{\"turn_id\":\"turn-2\"}}}\n",
                "{\"ordinal\":15,\"type\":\"event_msg\",\"timestamp\":\"2026-03-17T12:00:04Z\",\"payload\":{\"type\":\"task_complete\",\"turn_id\":\"turn-2\",\"last_agent_message\":\"complete final response\"}}\n"
            ),
        )
        .unwrap();

        let details = read_last_codex_observation(&session).unwrap();
        let turn = details.observed_turn.unwrap();

        assert_eq!(turn.provider_turn_id, "turn-2");
        assert_eq!(turn.outcome, AgentObservedTurnOutcome::Completed);
        assert_eq!(turn.started_cursor, Some(10));
        assert_eq!(turn.latest_cursor, Some(15));
        assert_eq!(
            turn.primary_user_message_sha256.as_deref(),
            Some(message_sha256("do the work").as_str())
        );
        assert_eq!(turn.user_message_count, 1);
        assert_eq!(
            turn.final_message.as_deref(),
            Some("complete final response")
        );
    }

    #[test]
    fn reads_codex_output_from_an_opaque_position_without_partial_records() {
        let temp = TempDir::new().unwrap();
        let session = temp.path().join("rollout-output.jsonl");
        let baseline = "{\"type\":\"session_meta\",\"payload\":{\"cwd\":\"/tmp/output\"}}\n";
        let first = "{\"type\":\"response_item\",\"timestamp\":\"2026-03-17T12:00:02Z\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\" first \"},{\"type\":\"output_text\",\"text\":\"line two\"}],\"internal_chat_message_metadata_passthrough\":{\"turn_id\":\"turn-1\"}}}\n";
        let partial = "{\"type\":\"response_item\",\"payload\":{";
        fs::write(&session, format!("{baseline}{first}{partial}")).unwrap();

        let baseline_offset = baseline.len() as u64;
        let (messages, next_offset, has_more) =
            read_codex_output_messages(&session, baseline_offset, 100).unwrap();

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text, "first\nline two");
        assert_eq!(messages[0].turn_id.as_deref(), Some("turn-1"));
        assert_eq!(messages[0].start_offset, baseline_offset);
        assert_eq!(messages[0].end_offset, next_offset);
        assert_eq!(next_offset, (baseline.len() + first.len()) as u64);
        assert!(!has_more);
        assert_eq!(codex_complete_tail_offset(&session).unwrap(), next_offset);
    }

    #[test]
    fn limits_codex_output_without_changing_event_positions() {
        let temp = TempDir::new().unwrap();
        let session = temp.path().join("rollout-output-limit.jsonl");
        let message = |text: &str| {
            format!(
                "{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{{\"type\":\"output_text\",\"text\":\"{text}\"}}]}}}}\n"
            )
        };
        let first = message("one");
        let second = message("two");
        fs::write(&session, format!("{first}{second}")).unwrap();

        let (first_page, cursor, has_more) = read_codex_output_messages(&session, 0, 1).unwrap();
        let (second_page, final_cursor, final_has_more) =
            read_codex_output_messages(&session, cursor, 1).unwrap();

        assert_eq!(first_page[0].text, "one");
        assert_eq!(second_page[0].text, "two");
        assert_eq!(first_page[0].start_offset, 0);
        assert_eq!(second_page[0].start_offset, cursor);
        assert!(has_more);
        assert!(!final_has_more);
        assert_eq!(final_cursor, (first.len() + second.len()) as u64);
    }

    #[test]
    fn bounds_tool_only_codex_output_scans_before_the_next_assistant_message() {
        let temp = TempDir::new().unwrap();
        let session = temp.path().join("rollout-output-tool-gap.jsonl");
        let tool = "{\"type\":\"response_item\",\"payload\":{\"type\":\"function_call_output\"}}\n";
        let assistant = "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"done\"}]}}\n";
        fs::write(
            &session,
            format!(
                "{}{}",
                tool.repeat(MAX_CODEX_OUTPUT_RECORDS_PER_READ + 1),
                assistant
            ),
        )
        .unwrap();

        let (first_page, cursor, has_more) = read_codex_output_messages(&session, 0, 1).unwrap();
        let (second_page, _, _) = read_codex_output_messages(&session, cursor, 1).unwrap();

        assert!(first_page.is_empty());
        assert!(has_more);
        assert_eq!(
            cursor,
            (tool.len() * MAX_CODEX_OUTPUT_RECORDS_PER_READ) as u64
        );
        assert_eq!(second_page[0].text, "done");
    }

    #[test]
    fn rejects_a_codex_output_record_larger_than_the_byte_bound() {
        let temp = TempDir::new().unwrap();
        let session = temp.path().join("rollout-output-oversized.jsonl");
        let oversized = format!(
            "{{\"value\":\"{}\"}}\n",
            "x".repeat(MAX_CODEX_OUTPUT_BYTES_PER_READ as usize)
        );
        fs::write(&session, oversized).unwrap();

        let error = read_codex_output_messages(&session, 0, 1).unwrap_err();

        assert!(error
            .to_string()
            .contains("exceeds the 1048576 byte read bound"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn observe_codex_prefers_session_open_by_matching_process() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let temp = TempDir::new().unwrap();
        let old = temp.path().join("rollout-old.jsonl");
        let live = temp.path().join("rollout-live.jsonl");
        fs::write(
            &old,
            concat!(
                "{\"payload\":{\"cwd\":\"/tmp/process-owned\"}}\n",
                "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"stale\"}]}}\n"
            ),
        )
        .unwrap();
        fs::write(
            &live,
            concat!(
                "{\"payload\":{\"cwd\":\"/tmp/process-owned\"}}\n",
                "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"live\"}]}}\n"
            ),
        )
        .unwrap();
        let _open_live_session = fs::File::open(&live).unwrap();
        let process_id = std::process::id();
        let process = LocalProcessInfo::with_root_pid(process_id).unwrap();

        set_env_path("WAKTERM_AGENT_CODEX_DIR", temp.path());
        let observed = observe_codex(
            "/tmp/process-owned",
            Some(old.to_string_lossy().as_ref()),
            None,
            Some(process_id),
            Some(process.start_time),
        )
        .unwrap()
        .unwrap();
        let mismatched_incarnation = observe_codex(
            "/tmp/process-owned",
            Some(old.to_string_lossy().as_ref()),
            None,
            Some(process_id),
            Some(process.start_time + 1),
        )
        .unwrap();
        remove_env_var("WAKTERM_AGENT_CODEX_DIR");

        assert_eq!(
            observed.session_path.as_deref(),
            Some(live.to_string_lossy().as_ref())
        );
        assert_eq!(observed.progress_summary.as_deref(), Some("live"));
        assert!(mismatched_incarnation.is_none());
    }

    #[test]
    fn codex_restore_command_uses_the_persisted_session_id() {
        let temp = TempDir::new().unwrap();
        let session = temp.path().join("rollout-resume.jsonl");
        fs::write(
            &session,
            "{\"type\":\"session_meta\",\"payload\":{\"session_id\":\"session-resume\",\"cwd\":\"/tmp/project\"}}\n",
        )
        .unwrap();

        assert_eq!(
            codex_session_id(&session).unwrap().as_deref(),
            Some("session-resume")
        );

        let command =
            codex_resume_command("codex -a never -s danger-full-access", "session-resume").unwrap();
        let argv = command
            .get_argv()
            .iter()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            argv,
            vec![
                "codex",
                "-a",
                "never",
                "-s",
                "danger-full-access",
                "resume",
                "session-resume",
            ]
        );
    }

    #[test]
    fn observe_codex_finds_live_session_in_older_dated_directory() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let temp = TempDir::new().unwrap();
        let day = Utc::now() - Duration::days(7);
        let dir = temp
            .path()
            .join(format!("{:04}", day.year_ce().1))
            .join(format!("{:02}", day.month()))
            .join(format!("{:02}", day.day()));
        fs::create_dir_all(&dir).unwrap();
        let session = dir.join("rollout-live-old-dir.jsonl");
        fs::write(
            &session,
            concat!(
                "{\"timestamp\":\"2026-03-20T14:04:41.302Z\",\"type\":\"session_meta\",\"payload\":{\"cwd\":\"/tmp/project-live\"}}\n",
                "{\"type\":\"response_item\",\"timestamp\":\"2026-03-27T12:00:03Z\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"still live\"}]}}\n"
            ),
        )
        .unwrap();

        set_env_path("WAKTERM_AGENT_CODEX_DIR", temp.path());
        let observed = observe_codex("/tmp/project-live", None, None, None, None)
            .unwrap()
            .unwrap();
        remove_env_var("WAKTERM_AGENT_CODEX_DIR");

        assert_eq!(observed.progress_summary.as_deref(), Some("still live"));
        assert_eq!(
            observed.session_path.as_deref(),
            Some(session.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn describe_pending_codex_observer_checks_older_dated_live_session() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let temp = TempDir::new().unwrap();
        let day = Utc::now() - Duration::days(7);
        let dir = temp
            .path()
            .join(format!("{:04}", day.year_ce().1))
            .join(format!("{:02}", day.month()))
            .join(format!("{:02}", day.day()));
        fs::create_dir_all(&dir).unwrap();
        let session = dir.join("rollout-live-pending.jsonl");
        fs::write(
            &session,
            "{\"timestamp\":\"2026-03-20T14:04:41.302Z\",\"type\":\"session_meta\",\"payload\":{\"cwd\":\"/tmp/project-live-pending\"}}\n",
        )
        .unwrap();

        set_env_path("WAKTERM_AGENT_CODEX_DIR", temp.path());
        let detail = describe_pending_codex_observer("/tmp/project-live-pending", None)
            .unwrap()
            .unwrap();
        remove_env_var("WAKTERM_AGENT_CODEX_DIR");

        assert_eq!(
            detail,
            "codex rollout session file exists but observer has not attached yet"
        );
    }

    #[test]
    fn observe_codex_keeps_waiting_on_agent_during_commentary() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let temp = TempDir::new().unwrap();
        let day = Utc::now();
        let dir = temp
            .path()
            .join(format!("{:04}", day.year_ce().1))
            .join(format!("{:02}", day.month()))
            .join(format!("{:02}", day.day()));
        fs::create_dir_all(&dir).unwrap();
        let session = dir.join("rollout-commentary.jsonl");
        fs::write(
            &session,
            concat!(
                "{\"payload\":{\"cwd\":\"/tmp/project-f\"}}\n",
                "{\"type\":\"event_msg\",\"timestamp\":\"2026-03-17T12:00:00Z\",\"payload\":{\"type\":\"task_started\",\"collaboration_mode_kind\":\"default\"}}\n",
                "{\"type\":\"turn_context\",\"timestamp\":\"2026-03-17T12:00:00Z\",\"payload\":{\"collaboration_mode\":{\"mode\":\"plan\"}}}\n",
                "{\"type\":\"event_msg\",\"timestamp\":\"2026-03-17T12:00:00Z\",\"payload\":{\"type\":\"user_message\"}}\n",
                "{\"type\":\"event_msg\",\"timestamp\":\"2026-03-17T12:00:02Z\",\"payload\":{\"type\":\"agent_message\",\"phase\":\"commentary\"}}\n",
                "{\"type\":\"response_item\",\"timestamp\":\"2026-03-17T12:00:02Z\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"thinking\"}]}}\n"
            ),
        )
        .unwrap();

        set_env_path("WAKTERM_AGENT_CODEX_DIR", temp.path());
        let observed = observe_codex("/tmp/project-f", None, None, None, None)
            .unwrap()
            .unwrap();
        remove_env_var("WAKTERM_AGENT_CODEX_DIR");

        assert_eq!(observed.progress_summary.as_deref(), Some("thinking"));
        assert_eq!(observed.harness_mode.as_deref(), Some("plan"));
        assert_eq!(observed.turn_phase.as_deref(), Some("commentary"));
        assert_eq!(observed.turn_state, AgentTurnState::WaitingOnAgent);
        assert_eq!(observed.last_turn_completed_at, None);
    }

    #[test]
    fn observe_codex_marks_aborted_turn_as_waiting_on_user() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let temp = TempDir::new().unwrap();
        let day = Utc::now();
        let dir = temp
            .path()
            .join(format!("{:04}", day.year_ce().1))
            .join(format!("{:02}", day.month()))
            .join(format!("{:02}", day.day()));
        fs::create_dir_all(&dir).unwrap();
        let session = dir.join("rollout-aborted.jsonl");
        fs::write(
            &session,
            concat!(
                "{\"payload\":{\"cwd\":\"/tmp/project-g\"}}\n",
                "{\"type\":\"event_msg\",\"timestamp\":\"2026-03-17T12:00:00Z\",\"payload\":{\"type\":\"task_started\",\"collaboration_mode_kind\":\"default\"}}\n",
                "{\"type\":\"event_msg\",\"timestamp\":\"2026-03-17T12:00:01Z\",\"payload\":{\"type\":\"user_message\"}}\n",
                "{\"type\":\"event_msg\",\"timestamp\":\"2026-03-17T12:00:03Z\",\"payload\":{\"type\":\"turn_aborted\"}}\n"
            ),
        )
        .unwrap();

        set_env_path("WAKTERM_AGENT_CODEX_DIR", temp.path());
        let observed = observe_codex("/tmp/project-g", None, None, None, None)
            .unwrap()
            .unwrap();
        remove_env_var("WAKTERM_AGENT_CODEX_DIR");

        assert_eq!(observed.harness_mode.as_deref(), Some("default"));
        assert_eq!(observed.turn_phase.as_deref(), Some("aborted"));
        assert_eq!(observed.turn_state, AgentTurnState::WaitingOnUser);
        assert_eq!(
            observed.last_turn_completed_at,
            Some(Utc.with_ymd_and_hms(2026, 3, 17, 12, 0, 3).unwrap())
        );
    }

    #[test]
    fn read_last_codex_observation_preserves_forward_fallback_semantics() {
        let temp = TempDir::new().unwrap();
        let session = temp.path().join("rollout-fallbacks.jsonl");
        fs::write(
            &session,
            concat!(
                "{\"payload\":{\"cwd\":\"/tmp/project-semantic\"}}\n",
                "{\"type\":\"event_msg\",\"timestamp\":\"2026-03-17T12:00:00Z\",\"payload\":{\"type\":\"task_started\",\"collaboration_mode_kind\":\"default\"}}\n",
                "{\"type\":\"response_item\",\"timestamp\":\"2026-03-17T12:00:01Z\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"assistant summary wins\"}]}}\n",
                "{\"type\":\"event_msg\",\"timestamp\":\"2026-03-17T12:00:03Z\",\"payload\":{\"type\":\"task_complete\",\"last_agent_message\":\"task-complete fallback loses\"}}\n"
            ),
        )
        .unwrap();

        let observed = read_last_codex_observation(&session).unwrap();

        assert_eq!(
            observed.progress_summary.as_deref(),
            Some("assistant summary wins")
        );
        assert_eq!(observed.harness_mode.as_deref(), Some("default"));
        assert_eq!(observed.turn_phase.as_deref(), Some("started"));
        assert_eq!(observed.turn_state, AgentTurnState::WaitingOnUser);
        assert_eq!(
            observed.last_turn_completed_at,
            Some(Utc.with_ymd_and_hms(2026, 3, 17, 12, 0, 3).unwrap())
        );
    }

    #[test]
    fn observes_latest_gemini_session_summary() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let temp = TempDir::new().unwrap();
        let cwd = "/tmp/project-i";
        let chats_dir = temp.path().join("tmp").join("project-i").join("chats");
        fs::create_dir_all(&chats_dir).unwrap();
        fs::write(
            temp.path().join("projects.json"),
            r#"{"projects":{"/tmp/project-i":"project-i"}}"#,
        )
        .unwrap();
        let session = chats_dir.join("session-2026-03-17.json");
        fs::write(
            &session,
            concat!(
                "{\"lastUpdated\":\"2026-03-17T12:00:03Z\",\"messages\":[",
                "{\"type\":\"user\",\"timestamp\":\"2026-03-17T12:00:00Z\",\"content\":[{\"text\":\"hello\"}]},",
                "{\"type\":\"gemini\",\"timestamp\":\"2026-03-17T12:00:03Z\",\"content\":\"all good\"}",
                "]}"
            ),
        )
        .unwrap();

        set_env_path("WAKTERM_AGENT_GEMINI_DIR", temp.path());
        let observed = observe_gemini(cwd, None, None).unwrap().unwrap();
        remove_env_var("WAKTERM_AGENT_GEMINI_DIR");

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
        assert_eq!(
            observed.updated_at,
            Some(Utc.with_ymd_and_hms(2026, 3, 17, 12, 0, 3).unwrap())
        );
    }

    #[test]
    fn observes_current_gemini_jsonl_session_and_ignores_incomplete_tail() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let temp = TempDir::new().unwrap();
        let cwd = "/tmp/project-jsonl";
        let chats_dir = temp.path().join("tmp").join("project-jsonl").join("chats");
        fs::create_dir_all(&chats_dir).unwrap();
        fs::write(
            temp.path().join("projects.json"),
            r#"{"projects":{"/tmp/project-jsonl":"project-jsonl"}}"#,
        )
        .unwrap();
        let session = chats_dir.join("session-2026-08-17-current.jsonl");
        fs::write(
            &session,
            concat!(
                "{\"sessionId\":\"current-session\",\"projectHash\":\"hash\",\"startTime\":\"2026-08-17T12:00:00Z\",\"lastUpdated\":\"2026-08-17T12:00:00Z\"}\n",
                "{\"id\":\"turn-1\",\"timestamp\":\"2026-08-17T12:00:01Z\",\"type\":\"user\",\"content\":[{\"text\":\"hello\"}]}\n",
                "{\"id\":\"response-1\",\"timestamp\":\"2026-08-17T12:00:02Z\",\"type\":\"gemini\",\"content\":\"first value\"}\n",
                "{\"id\":\"response-1\",\"timestamp\":\"2026-08-17T12:00:02Z\",\"type\":\"gemini\",\"content\":\"current value ✓\",\"tokens\":{\"total\":10}}\n",
                "{\"$set\":{\"lastUpdated\":\"2026-08-17T12:00:03Z\"}}\n",
                "{\"id\":\"partial"
            ),
        )
        .unwrap();

        set_env_path("WAKTERM_AGENT_GEMINI_DIR", temp.path());
        let observed = observe_gemini(cwd, None, None).unwrap().unwrap();
        remove_env_var("WAKTERM_AGENT_GEMINI_DIR");

        assert_eq!(
            observed.progress_summary.as_deref(),
            Some("current value ✓")
        );
        assert_eq!(
            observed.session_path.as_deref(),
            Some(session.to_string_lossy().as_ref())
        );
        assert_eq!(observed.turn_state, AgentTurnState::WaitingOnUser);
        assert_eq!(
            observed.updated_at,
            Some(Utc.with_ymd_and_hms(2026, 8, 17, 12, 0, 3).unwrap())
        );
    }

    #[test]
    fn preferred_legacy_gemini_session_follows_jsonl_migration() {
        let temp = TempDir::new().unwrap();
        let legacy = temp.path().join("session-migrated.json");
        let migrated = temp.path().join("session-migrated.jsonl");
        fs::write(&legacy, r#"{"sessionId":"migrated","messages":[]}"#).unwrap();
        fs::write(
            &migrated,
            concat!(
                "{\"sessionId\":\"migrated\",\"lastUpdated\":\"2026-08-17T12:00:00Z\"}\n",
                "{\"id\":\"turn\",\"timestamp\":\"2026-08-17T12:00:01Z\",\"type\":\"user\",\"content\":\"hello\"}\n"
            ),
        )
        .unwrap();

        assert_eq!(preferred_gemini_session_path(&legacy), migrated);
    }

    #[test]
    fn current_gemini_jsonl_rejects_an_oversized_complete_record() {
        let temp = TempDir::new().unwrap();
        let session = temp.path().join("session-oversized.jsonl");
        fs::write(
            &session,
            format!(
                "{{\"sessionId\":\"oversized\",\"padding\":\"{}\"}}\n",
                "x".repeat(4 * 1024 * 1024)
            ),
        )
        .unwrap();

        let error = read_gemini_conversation(&session).unwrap_err();
        assert!(error.to_string().contains("exceeds the 4194304-byte bound"));
    }

    #[test]
    fn observes_gemini_session_via_project_root_file_without_projects_registry() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let temp = TempDir::new().unwrap();
        let project_dir = temp.path().join("tmp").join("fallback-project");
        let chats_dir = project_dir.join("chats");
        fs::create_dir_all(&chats_dir).unwrap();
        fs::write(project_dir.join(".project_root"), "/tmp/project-e\n").unwrap();
        let session = chats_dir.join("session-2026-03-17.json");
        fs::write(
            &session,
            concat!(
                "{\"lastUpdated\":\"2026-03-17T12:00:03Z\",\"messages\":[",
                "{\"type\":\"user\",\"timestamp\":\"2026-03-17T12:00:00Z\",\"content\":[{\"text\":\"hello\"}]},",
                "{\"type\":\"gemini\",\"timestamp\":\"2026-03-17T12:00:03Z\",\"content\":\"fallback\"}",
                "]}"
            ),
        )
        .unwrap();

        set_env_path("WAKTERM_AGENT_GEMINI_DIR", temp.path());
        let observed = observe_gemini("/tmp/project-e", None, None)
            .unwrap()
            .unwrap();
        remove_env_var("WAKTERM_AGENT_GEMINI_DIR");

        assert_eq!(observed.progress_summary.as_deref(), Some("fallback"));
        assert_eq!(
            observed.session_path.as_deref(),
            Some(session.to_string_lossy().as_ref())
        );
        assert_eq!(observed.turn_state, AgentTurnState::WaitingOnUser);
    }

    #[test]
    fn observes_latest_opencode_session_summary() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("opencode.db");
        let connection = create_opencode_test_db(&db_path);
        connection
            .execute(
                "INSERT INTO session (id, directory, time_updated) VALUES (?1, ?2, ?3)",
                params!["session-1", "/tmp/project-j", 1_773_711_603_000_i64],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO message (id, session_id, time_created, data) VALUES (?1, ?2, ?3, ?4)",
                params![
                    "message-user",
                    "session-1",
                    1_773_711_600_000_i64,
                    r#"{"role":"user"}"#
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO part (id, message_id, session_id, time_created, data) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    "part-user",
                    "message-user",
                    "session-1",
                    1_773_711_600_000_i64,
                    r#"{"type":"text","text":"hello"}"#
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO message (id, session_id, time_created, data) VALUES (?1, ?2, ?3, ?4)",
                params![
                    "message-assistant",
                    "session-1",
                    1_773_711_603_000_i64,
                    r#"{"role":"assistant"}"#
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO part (id, message_id, session_id, time_created, data) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    "part-assistant",
                    "message-assistant",
                    "session-1",
                    1_773_711_603_000_i64,
                    r#"{"type":"text","text":"all good"}"#
                ],
            )
            .unwrap();
        drop(connection);

        set_env_path("WAKTERM_AGENT_OPENCODE_DB", &db_path);
        let observed = observe_opencode("/tmp/project-j", None, None)
            .unwrap()
            .unwrap();
        remove_env_var("WAKTERM_AGENT_OPENCODE_DB");

        assert_eq!(observed.progress_summary.as_deref(), Some("all good"));
        assert_eq!(observed.turn_state, AgentTurnState::WaitingOnUser);
        assert_eq!(
            observed.last_turn_completed_at,
            Some(
                Utc.timestamp_millis_opt(1_773_711_603_000_i64)
                    .single()
                    .unwrap()
            )
        );
        let (session_db_path, session_id) =
            parse_opencode_session_path(observed.session_path.as_deref().unwrap()).unwrap();
        assert_eq!(session_db_path, db_path);
        assert_eq!(session_id, "session-1");
    }

    #[test]
    fn refresh_runtime_observes_gemini_and_opencode_sessions() {
        let _env_lock = ENV_LOCK.lock().unwrap();

        let gemini_temp = TempDir::new().unwrap();
        let gemini_cwd = "/tmp/project-k";
        let gemini_chats = gemini_temp
            .path()
            .join("tmp")
            .join("project-k")
            .join("chats");
        fs::create_dir_all(&gemini_chats).unwrap();
        fs::write(
            gemini_temp.path().join("projects.json"),
            r#"{"projects":{"/tmp/project-k":"project-k"}}"#,
        )
        .unwrap();
        fs::write(
            gemini_chats.join("session-2026-03-17.json"),
            concat!(
                "{\"messages\":[",
                "{\"type\":\"user\",\"timestamp\":\"2026-03-17T12:00:00Z\",\"content\":[{\"text\":\"hello\"}]},",
                "{\"type\":\"gemini\",\"timestamp\":\"2026-03-17T12:00:02Z\",\"content\":\"reply\"}",
                "]}"
            ),
        )
        .unwrap();

        set_env_path("WAKTERM_AGENT_GEMINI_DIR", gemini_temp.path());
        let gemini_metadata = AgentMetadata {
            agent_id: "id-gemini".to_string(),
            name: "gemini".to_string(),
            launch_cmd: "gemini".to_string(),
            declared_cwd: gemini_cwd.to_string(),
            adopted_pid: None,
            adopted_start_time: None,
            created_at: Utc::now(),
            repo_root: None,
            worktree: None,
            branch: None,
            managed_checkout: false,
            codex_app_server: None,
        };
        let mut gemini_runtime = AgentRuntimeSnapshot::new(&gemini_metadata);
        gemini_runtime.foreground_process_name =
            Some("/home/mihai/.nvm/versions/node/v22.14.0/bin/node".to_string());
        refresh_runtime_from_harness(&mut gemini_runtime, &gemini_metadata);
        remove_env_var("WAKTERM_AGENT_GEMINI_DIR");

        assert_eq!(gemini_runtime.transport, AgentTransport::ObservedPty);
        assert_eq!(gemini_runtime.harness, AgentHarness::Gemini);
        assert_eq!(gemini_runtime.turn_state, AgentTurnState::WaitingOnUser);

        let opencode_temp = TempDir::new().unwrap();
        let opencode_db = opencode_temp.path().join("opencode.db");
        let connection = create_opencode_test_db(&opencode_db);
        connection
            .execute(
                "INSERT INTO session (id, directory, time_updated) VALUES (?1, ?2, ?3)",
                params!["session-2", "/tmp/project-l", 1_773_711_605_000_i64],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO message (id, session_id, time_created, data) VALUES (?1, ?2, ?3, ?4)",
                params![
                    "message-user-2",
                    "session-2",
                    1_773_711_600_000_i64,
                    r#"{"role":"user"}"#
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO message (id, session_id, time_created, data) VALUES (?1, ?2, ?3, ?4)",
                params![
                    "message-assistant-2",
                    "session-2",
                    1_773_711_605_000_i64,
                    r#"{"role":"assistant"}"#
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO part (id, message_id, session_id, time_created, data) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    "part-assistant-2",
                    "message-assistant-2",
                    "session-2",
                    1_773_711_605_000_i64,
                    r#"{"type":"text","text":"reply"}"#
                ],
            )
            .unwrap();
        drop(connection);

        set_env_path("WAKTERM_AGENT_OPENCODE_DB", &opencode_db);
        let opencode_metadata = AgentMetadata {
            agent_id: "id-opencode".to_string(),
            name: "opencode".to_string(),
            launch_cmd: "opencode".to_string(),
            declared_cwd: "/tmp/project-l".to_string(),
            adopted_pid: None,
            adopted_start_time: None,
            created_at: Utc::now(),
            repo_root: None,
            worktree: None,
            branch: None,
            managed_checkout: false,
            codex_app_server: None,
        };
        let mut opencode_runtime = AgentRuntimeSnapshot::new(&opencode_metadata);
        opencode_runtime.foreground_process_name = Some("opencode".to_string());
        refresh_runtime_from_harness(&mut opencode_runtime, &opencode_metadata);
        remove_env_var("WAKTERM_AGENT_OPENCODE_DB");

        assert_eq!(opencode_runtime.transport, AgentTransport::ObservedPty);
        assert_eq!(opencode_runtime.harness, AgentHarness::Opencode);
        assert_eq!(opencode_runtime.turn_state, AgentTurnState::WaitingOnUser);
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

        set_env_path("WAKTERM_AGENT_CLAUDE_DIR", temp.path());
        let metadata = AgentMetadata {
            agent_id: "id".to_string(),
            name: "alpha".to_string(),
            launch_cmd: "claude".to_string(),
            declared_cwd: cwd.to_string(),
            adopted_pid: None,
            adopted_start_time: None,
            created_at: Utc::now(),
            repo_root: None,
            worktree: None,
            branch: None,
            managed_checkout: false,
            codex_app_server: None,
        };
        let mut runtime = AgentRuntimeSnapshot::new(&metadata);
        runtime.foreground_process_name = Some("claude".to_string());
        refresh_runtime_from_harness(&mut runtime, &metadata);
        remove_env_var("WAKTERM_AGENT_CLAUDE_DIR");

        assert_eq!(runtime.turn_state, AgentTurnState::WaitingOnAgent);
        assert_eq!(runtime.last_turn_completed_at, None);
        assert_eq!(runtime.attention_reason, None);
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

        set_env_path("WAKTERM_AGENT_CODEX_DIR", temp.path());
        let metadata = AgentMetadata {
            agent_id: "id".to_string(),
            name: "delta".to_string(),
            launch_cmd: "codex".to_string(),
            declared_cwd: "/tmp/project-d".to_string(),
            adopted_pid: None,
            adopted_start_time: None,
            created_at: Utc::now(),
            repo_root: None,
            worktree: None,
            branch: None,
            managed_checkout: false,
            codex_app_server: None,
        };
        let mut runtime = AgentRuntimeSnapshot::new(&metadata);
        runtime.foreground_process_name = Some("zsh".to_string());
        runtime.observer_started_at = Some(Utc::now() - Duration::minutes(1));
        refresh_runtime_from_harness(&mut runtime, &metadata);
        remove_env_var("WAKTERM_AGENT_CODEX_DIR");

        assert_eq!(runtime.harness, AgentHarness::Codex);
        assert_eq!(runtime.transport, AgentTransport::PlainPty);
        assert_eq!(runtime.session_path, None);
        assert_eq!(runtime.harness_mode, None);
        assert_eq!(runtime.turn_phase, None);
        assert_eq!(runtime.attention_reason, None);
        assert_eq!(runtime.turn_state, AgentTurnState::Unknown);
    }

    #[test]
    fn prime_runtime_for_new_agent_gates_stale_sessions_for_matching_harnesses() {
        let metadata = AgentMetadata {
            agent_id: "id".to_string(),
            name: "delta".to_string(),
            launch_cmd: "claude".to_string(),
            declared_cwd: "/tmp/project-d".to_string(),
            adopted_pid: None,
            adopted_start_time: None,
            created_at: Utc.with_ymd_and_hms(2026, 3, 17, 12, 0, 0).unwrap(),
            repo_root: None,
            worktree: None,
            branch: None,
            managed_checkout: false,
            codex_app_server: None,
        };
        let mut runtime = AgentRuntimeSnapshot::new(&metadata);
        runtime.foreground_process_name = Some("claude".to_string());
        runtime.session_path = Some("/tmp/stale.jsonl".to_string());
        runtime.progress_summary = Some("stale".to_string());

        prime_runtime_for_new_agent(&mut runtime, &metadata, Some("claude"));

        assert_eq!(runtime.observer_started_at, Some(metadata.created_at));
        assert_eq!(runtime.session_path, None);
        assert_eq!(runtime.progress_summary, None);
        assert_eq!(runtime.turn_state, AgentTurnState::Unknown);
    }

    #[test]
    fn prime_runtime_for_new_agent_preserves_existing_activity_for_adopted_panes() {
        let metadata = AgentMetadata {
            agent_id: "id".to_string(),
            name: "delta".to_string(),
            launch_cmd: "codex".to_string(),
            declared_cwd: "/tmp/project-d".to_string(),
            adopted_pid: None,
            adopted_start_time: None,
            created_at: Utc.with_ymd_and_hms(2026, 3, 17, 12, 0, 0).unwrap(),
            repo_root: None,
            worktree: None,
            branch: None,
            managed_checkout: false,
            codex_app_server: None,
        };
        let mut runtime = AgentRuntimeSnapshot::new(&metadata);
        runtime.foreground_process_name = Some("codex".to_string());
        runtime.last_output_at = Some(Utc.with_ymd_and_hms(2026, 3, 17, 11, 55, 0).unwrap());

        prime_runtime_for_new_agent(&mut runtime, &metadata, Some("codex"));

        assert_eq!(runtime.observer_started_at, None);
        assert_eq!(runtime.session_path, None);
        assert_eq!(runtime.progress_summary, None);
        assert_eq!(runtime.turn_state, AgentTurnState::Unknown);
    }

    #[test]
    fn refresh_runtime_marks_aborted_codex_turn_as_attention() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let temp = TempDir::new().unwrap();
        let day = Utc::now();
        let dir = temp
            .path()
            .join(format!("{:04}", day.year_ce().1))
            .join(format!("{:02}", day.month()))
            .join(format!("{:02}", day.day()));
        fs::create_dir_all(&dir).unwrap();
        let session = dir.join("rollout-aborted-runtime.jsonl");
        fs::write(
            &session,
            concat!(
                "{\"payload\":{\"cwd\":\"/tmp/project-h\"}}\n",
                "{\"type\":\"event_msg\",\"timestamp\":\"2026-03-17T12:00:00Z\",\"payload\":{\"type\":\"task_started\",\"collaboration_mode_kind\":\"default\"}}\n",
                "{\"type\":\"event_msg\",\"timestamp\":\"2026-03-17T12:00:01Z\",\"payload\":{\"type\":\"user_message\"}}\n",
                "{\"type\":\"event_msg\",\"timestamp\":\"2026-03-17T12:00:03Z\",\"payload\":{\"type\":\"turn_aborted\"}}\n"
            ),
        )
        .unwrap();

        set_env_path("WAKTERM_AGENT_CODEX_DIR", temp.path());
        let metadata = AgentMetadata {
            agent_id: "id".to_string(),
            name: "hotel".to_string(),
            launch_cmd: "codex".to_string(),
            declared_cwd: "/tmp/project-h".to_string(),
            adopted_pid: None,
            adopted_start_time: None,
            created_at: Utc::now(),
            repo_root: None,
            worktree: None,
            branch: None,
            managed_checkout: false,
            codex_app_server: None,
        };
        let mut runtime = AgentRuntimeSnapshot::new(&metadata);
        runtime.foreground_process_name = Some("codex".to_string());
        refresh_runtime_from_harness(&mut runtime, &metadata);
        remove_env_var("WAKTERM_AGENT_CODEX_DIR");

        assert_eq!(runtime.turn_phase.as_deref(), Some("aborted"));
        assert_eq!(runtime.turn_state, AgentTurnState::WaitingOnUser);
        assert_eq!(runtime.attention_reason.as_deref(), Some("turn-aborted"));
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

        set_env_path("WAKTERM_AGENT_CODEX_DIR", temp.path());
        let observed = observe_codex(
            "/tmp/project-e",
            Some(older.to_string_lossy().as_ref()),
            None,
            None,
            None,
        )
        .unwrap()
        .unwrap();
        remove_env_var("WAKTERM_AGENT_CODEX_DIR");

        assert_eq!(observed.progress_summary.as_deref(), Some("older"));
        assert_eq!(observed.harness_mode, None);
        assert_eq!(observed.turn_phase, None);
        assert_eq!(
            observed.session_path.as_deref(),
            Some(older.to_string_lossy().as_ref())
        );
    }
}
