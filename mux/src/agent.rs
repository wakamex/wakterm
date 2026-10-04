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
    /// The foreground launcher enclosing an observed TUI. Its isolation cannot
    /// be reconstructed from the inner harness argv during native restoration.
    #[serde(default)]
    pub launch_supervisor: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CodexAppServerSession {
    pub thread_id: String,
    pub session_id: String,
    pub executable: String,
    pub version: String,
    pub tui_args: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteCodexTui {
    pub pid: u32,
    pub start_time: u64,
    pub endpoint: String,
    pub thread_id: String,
    pub tui_args: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum AgentHarness {
    #[default]
    Unknown,
    Agy,
    Claude,
    Codex,
    Gemini,
    Opencode,
    /// Z.ai's ZCode CLI, which stores sessions in OpenCode's schema.
    Zcode,
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
    Managed,
}

impl AgentOrigin {
    pub fn is_registered(&self) -> bool {
        matches!(self, Self::Adopted | Self::Managed)
    }

    pub fn for_registered_transport(transport: &AgentTransport) -> Self {
        match transport {
            AgentTransport::CodexAppServerTui => Self::Managed,
            AgentTransport::PlainPty | AgentTransport::ObservedPty => Self::Adopted,
        }
    }
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
    /// Set while the pane shows a Claude background job that is running
    /// outside it.
    #[serde(default)]
    pub background_job: Option<AgentBackgroundJob>,
    #[serde(skip, default)]
    pub observer_started_at: Option<DateTime<Utc>>,
    #[serde(skip, default)]
    pub last_harness_refresh_at: Option<DateTime<Utc>>,
}

/// A Claude conversation that a pane shows while Claude's daemon runs it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentBackgroundJob {
    pub job_id: String,
    pub session_id: String,
    /// How to move the conversation back into the pane.
    pub hint: String,
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
            background_job: None,
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
    #[serde(default)]
    pub needs_attention: bool,
}

#[derive(Clone, Debug)]
pub struct AgentProcessMatch<'a> {
    pub harness: AgentHarness,
    pub launch_cmd: String,
    pub process: Option<&'a LocalProcessInfo>,
    pub launch_supervisor: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExpectedAgentSession {
    pub harness: AgentHarness,
    pub session_id: String,
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
        if candidate
            .split_whitespace()
            .any(|part| Path::new(part).file_name().and_then(|name| name.to_str()) == Some("agy"))
        {
            return AgentHarness::Agy;
        }
        // Match the program name exactly: "zcode" also appears in paths.
        if candidate.split_whitespace().next().is_some_and(|program| {
            matches!(
                Path::new(program)
                    .file_name()
                    .and_then(|name| name.to_str()),
                Some("zcode" | "zcode.cjs")
            )
        }) {
            return AgentHarness::Zcode;
        }
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
        AgentHarness::Agy => Some("agy"),
        AgentHarness::Claude => Some("claude"),
        AgentHarness::Codex => Some("codex"),
        AgentHarness::Gemini => Some("gemini"),
        AgentHarness::Opencode => Some("opencode"),
        AgentHarness::Zcode => Some("zcode"),
        AgentHarness::Unknown => None,
    }
}

fn infer_harness_from_process_info(process: &LocalProcessInfo) -> AgentHarness {
    [
        AgentHarness::Agy,
        AgentHarness::Claude,
        AgentHarness::Codex,
        AgentHarness::Gemini,
        AgentHarness::Opencode,
        AgentHarness::Zcode,
    ]
    .iter()
    .cloned()
    .find(|harness| is_harness_tui_process(harness, process))
    .unwrap_or(AgentHarness::Unknown)
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

fn same_terminal_job(root: &LocalProcessInfo, child: &LocalProcessInfo) -> bool {
    #[cfg(unix)]
    {
        root.process_group != 0
            && root.process_group == child.process_group
            && root.controlling_tty.is_some()
            && root.controlling_tty == child.controlling_tty
    }
    #[cfg(windows)]
    {
        root.console != 0 && root.console == child.console
    }
    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}

/// Select the unique outermost harness in this foreground job. A harness is a
/// boundary: its tools and nested agents can never replace its pane identity.
fn foreground_harness_process(process: &LocalProcessInfo) -> Option<&LocalProcessInfo> {
    fn visit<'a>(
        root: &LocalProcessInfo,
        process: &'a LocalProcessInfo,
        matches: &mut Vec<&'a LocalProcessInfo>,
    ) {
        if matches!(
            process.status,
            procinfo::LocalProcessStatus::Zombie | procinfo::LocalProcessStatus::Dead
        ) {
            return;
        }
        if infer_harness_from_process_info(process) != AgentHarness::Unknown {
            matches.push(process);
            return;
        }
        for child in process
            .children
            .values()
            .filter(|child| same_terminal_job(root, child))
        {
            visit(root, child, matches);
        }
    }
    let mut matches = Vec::new();
    visit(process, process, &mut matches);
    (matches.len() == 1).then(|| matches[0])
}

pub fn detect_harness_process<'a>(
    process: Option<&'a LocalProcessInfo>,
    foreground_process_name: Option<&str>,
) -> Option<AgentProcessMatch<'a>> {
    if let Some(process) = process {
        let selected = foreground_harness_process(process)?;
        return Some(AgentProcessMatch {
            harness: infer_harness_from_process_info(selected),
            launch_cmd: format_process_command(selected)?,
            process: Some(selected),
            launch_supervisor: (selected.pid != process.pid)
                .then(|| process.executable.to_string_lossy().into_owned()),
        });
    }

    let harness = infer_harness("", foreground_process_name);
    let launch_cmd = default_launch_cmd_for_harness(&harness)?;
    Some(AgentProcessMatch {
        harness,
        launch_cmd: launch_cmd.to_string(),
        process: None,
        launch_supervisor: None,
    })
}

fn is_harness_tui_program(harness: &AgentHarness, value: &str) -> bool {
    Path::new(value)
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| {
            let name = name.to_ascii_lowercase();
            match harness {
                AgentHarness::Agy => matches!(name.as_str(), "agy" | "agy.exe"),
                AgentHarness::Claude => matches!(
                    name.as_str(),
                    "claude" | "claude.exe" | "claude.cmd" | "claude.bat" | "claude.js"
                ),
                AgentHarness::Codex => matches!(
                    name.as_str(),
                    "codex" | "codex.exe" | "codex.cmd" | "codex.bat" | "codex.js"
                ),
                AgentHarness::Gemini => {
                    matches!(name.as_str(), "gemini" | "gemini.exe" | "gemini.js")
                }
                AgentHarness::Opencode => matches!(name.as_str(), "opencode" | "opencode.exe"),
                AgentHarness::Zcode => matches!(name.as_str(), "zcode" | "zcode.cjs"),
                _ => false,
            }
        })
        .unwrap_or(false)
}

pub(crate) fn is_harness_tui_process(harness: &AgentHarness, process: &LocalProcessInfo) -> bool {
    is_harness_tui_program(harness, &process.name)
        || is_harness_tui_program(harness, process.executable.to_string_lossy().as_ref())
        || process
            .argv
            .first()
            .is_some_and(|arg| is_harness_tui_program(harness, arg))
        || (process
            .executable
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| matches!(name, "node" | "node.exe" | "bun" | "bun.exe" | "deno"))
            && process
                .argv
                .get(1)
                .is_some_and(|arg| is_harness_tui_program(harness, arg)))
}

fn harness_tui_process<'a>(
    harness: &AgentHarness,
    process: &'a LocalProcessInfo,
) -> Option<&'a LocalProcessInfo> {
    if is_harness_tui_process(harness, process) {
        return Some(process);
    }
    process
        .children
        .values()
        .find_map(|child| harness_tui_process(harness, child))
}

pub fn remote_codex_tui(process: &LocalProcessInfo) -> Option<RemoteCodexTui> {
    let process = harness_tui_process(&AgentHarness::Codex, process)?;
    let mut args = process.argv.iter().skip(1).peekable();
    if args
        .peek()
        .is_some_and(|arg| is_harness_tui_program(&AgentHarness::Codex, arg))
    {
        args.next();
    }
    let mut endpoint = None;
    let mut thread_id = None;
    let mut resumed = false;
    let mut positional_only = false;
    let mut settings: [Vec<(&str, Vec<String>)>; 2] = Default::default();
    while let Some(arg) = args.next() {
        if arg == "--" && !positional_only {
            positional_only = true;
            continue;
        }
        if !positional_only && arg.starts_with('-') {
            let (name, inline) = arg
                .split_once('=')
                .map_or((arg.as_str(), None), |(name, value)| (name, Some(value)));
            // Normalize attached short values as well as long --option=value forms.
            let (name, inline) =
                if !name.starts_with("--") && name.len() > 2 && name.as_bytes()[1].is_ascii() {
                    (
                        &arg[..2],
                        Some(arg[2..].strip_prefix('=').unwrap_or(&arg[2..])),
                    )
                } else {
                    (name, inline)
                };
            let (name, takes_value) = match name {
                "--remote" => ("--remote", true),
                "-C" | "--cd" => ("--cd", true),
                "-a" | "--ask-for-approval" => ("-a", true),
                "-s" | "--sandbox" => ("-s", true),
                "-m" | "--model" => ("-m", true),
                "-c" | "--config" => ("-c", true),
                "-i" | "--image" => ("-i", true),
                "--local-provider" | "--add-dir" => (name, true),
                "--yolo" | "--dangerously-bypass-approvals-and-sandbox" => {
                    ("--dangerously-bypass-approvals-and-sandbox", false)
                }
                "--not-so-yolo" | "--approve-for-me" => ("--approve-for-me", false),
                "--strict-config"
                | "--oss"
                | "--dangerously-bypass-hook-trust"
                | "--search"
                | "--no-alt-screen"
                | "--include-non-interactive"
                | "--all" => (name, false),
                // Unknown arity or process-wide overrides cannot be captured faithfully.
                _ => return None,
            };
            let mut values = Vec::new();
            if takes_value {
                let value = inline.or_else(|| args.next().map(String::as_str))?;
                if value.is_empty() || value.starts_with('-') {
                    return None;
                }
                if name == "-c" && codex_reasoning_effort_override(value).is_none() {
                    return None;
                }
                values.push(value.to_string());
                if name == "-i" && inline.is_none() {
                    while args.peek().is_some_and(|value| !value.starts_with('-')) {
                        values.push(args.next()?.clone());
                    }
                }
            } else if inline.is_some() {
                return None;
            }
            match name {
                "--remote" => endpoint = values.pop(),
                "--cd" | "--all" => {}
                _ => settings[usize::from(resumed)].push((name, values)),
            }
        } else if !resumed && !positional_only && arg == "resume" {
            resumed = true;
        } else if resumed && thread_id.is_none() {
            let parsed = uuid::Uuid::parse_str(arg).ok()?.to_string();
            if &parsed != arg {
                return None;
            }
            thread_id = Some(parsed);
        } else {
            // Never find an identity inside an option value or replay an initial prompt.
            return None;
        }
    }
    let endpoint = endpoint.filter(|value| value.starts_with("unix://"))?;
    let thread_id = thread_id?;
    let [mut root, resume] = settings;
    // Codex merges resume-scoped settings over root settings before starting the TUI.
    let sandbox_setting = |name: &str| {
        matches!(
            name,
            "-s" | "--approve-for-me" | "--dangerously-bypass-approvals-and-sandbox"
        )
    };
    if resume.iter().any(|(name, _)| sandbox_setting(name)) {
        root.retain(|(name, _)| !sandbox_setting(name));
    }
    if resume.iter().any(|(name, _)| *name == "--approve-for-me") {
        root.retain(|(name, _)| *name != "-a");
    }
    root.retain(|(name, _)| *name == "--add-dir" || !resume.iter().any(|(other, _)| name == other));
    root.extend(resume);
    // These mixed-scope combinations need config overrides to reproduce Codex's
    // merge. Managed launch does not support those overrides.
    if root.iter().any(|(name, _)| *name == "-a")
        && root.iter().any(|(name, _)| {
            matches!(
                *name,
                "--approve-for-me" | "--dangerously-bypass-approvals-and-sandbox"
            )
        })
    {
        return None;
    }
    let tui_args = root
        .into_iter()
        .flat_map(|(name, values)| {
            if values.is_empty() {
                vec![name.to_string()]
            } else {
                values
                    .into_iter()
                    .flat_map(|value| [name.to_string(), value])
                    .collect()
            }
        })
        .collect();
    Some(RemoteCodexTui {
        pid: process.pid,
        start_time: process.start_time,
        endpoint,
        thread_id,
        tui_args,
    })
}

pub(crate) fn codex_reasoning_effort_override(value: &str) -> Option<String> {
    let (key, value) = value.split_once('=')?;
    if key != "model_reasoning_effort" {
        return None;
    }
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Ok(value) = serde_json::from_str::<String>(value) {
        return (!value.is_empty()).then_some(value);
    }
    if value.len() >= 2 && value.starts_with('\'') && value.ends_with('\'') {
        let value = &value[1..value.len() - 1];
        return (!value.is_empty()).then(|| value.to_string());
    }
    Some(value.to_string())
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ClaudeOptionValues {
    None,
    One,
    Many,
    Optional,
}

fn claude_option_values(option: &str) -> ClaudeOptionValues {
    match option {
        "--add-dir" | "--allowedTools" | "--allowed-tools" | "--betas" | "--disallowedTools"
        | "--disallowed-tools" | "--file" | "--mcp-config" | "--plugin-dir" | "--tools" => {
            ClaudeOptionValues::Many
        }
        "--agent"
        | "--agents"
        | "--append-system-prompt"
        | "--append-system-prompt-file"
        | "--autocompact"
        | "--debug-file"
        | "--effort"
        | "--environment"
        | "--fallback-model"
        | "--input-format"
        | "--json-schema"
        | "--max-budget-usd"
        | "--model"
        | "-n"
        | "--name"
        | "--output-format"
        | "--permission-mode"
        | "--permission-prompts"
        | "--plugin-url"
        | "--remote-control-name"
        | "--remote-control-session-name-prefix"
        | "--setting-sources"
        | "--settings"
        | "--system-prompt"
        | "--system-prompt-file"
        | "--system-prompt-snapshot" => ClaudeOptionValues::One,
        "--cloud"
        | "-d"
        | "--debug"
        | "--from-pr"
        | "--prompt-suggestions"
        | "--remote-control"
        | "-w"
        | "--worktree" => ClaudeOptionValues::Optional,
        _ => ClaudeOptionValues::None,
    }
}

fn normalize_claude_argv(argv: &mut Vec<String>) {
    let original = std::mem::take(argv);
    let Some(program) = original.first() else {
        return;
    };
    argv.push(program.clone());
    let mut index = 1;
    if original
        .get(index)
        .is_some_and(|value| is_harness_tui_program(&AgentHarness::Claude, value))
    {
        argv.push(original[index].clone());
        index += 1;
    }
    while index < original.len() {
        let argument = &original[index];
        if matches!(argument.as_str(), "-c" | "--continue" | "--fork-session") {
            index += 1;
            continue;
        }
        if matches!(
            argument.as_str(),
            "-r" | "--resume" | "--session-id" | "--teleport"
        ) {
            index += 1;
            if original
                .get(index)
                .is_some_and(|value| !value.starts_with('-'))
            {
                index += 1;
            }
            continue;
        }
        if argument.starts_with("--resume=")
            || argument.starts_with("--session-id=")
            || argument.starts_with("--teleport=")
        {
            index += 1;
            continue;
        }
        if !argument.starts_with('-') {
            // A bare Claude argument is an initial prompt or command. Replaying
            // it after an exact resume could start an unintended turn.
            index += 1;
            continue;
        }

        argv.push(argument.clone());
        let values = claude_option_values(argument.split_once('=').map_or(argument, |pair| pair.0));
        if argument.contains('=') || values == ClaudeOptionValues::None {
            index += 1;
            continue;
        }
        index += 1;
        match values {
            ClaudeOptionValues::One => {
                if let Some(value) = original.get(index) {
                    argv.push(value.clone());
                    index += 1;
                }
            }
            ClaudeOptionValues::Many => {
                while let Some(value) = original.get(index).filter(|value| !value.starts_with('-'))
                {
                    argv.push(value.clone());
                    index += 1;
                }
            }
            ClaudeOptionValues::Optional => {
                if let Some(value) = original.get(index).filter(|value| !value.starts_with('-')) {
                    argv.push(value.clone());
                    index += 1;
                }
            }
            ClaudeOptionValues::None => {}
        }
    }
}

/// The session a zcode process was started with through `--resume`.
fn zcode_resumed_session(harness: &AgentHarness, launch_cmd: &str) -> Option<String> {
    if harness != &AgentHarness::Zcode {
        return None;
    }
    let argv = shell_words::split(launch_cmd).ok()?;
    argv.iter()
        .enumerate()
        .find_map(|(index, arg)| match arg.strip_prefix("--resume=") {
            Some(session_id) => Some(session_id.to_string()),
            None if arg == "--resume" => argv.get(index + 1).cloned(),
            None => None,
        })
        .filter(|session_id| is_zcode_session_id(session_id))
}

fn is_zcode_session_id(value: &str) -> bool {
    value.strip_prefix("sess_").is_some_and(is_uuid)
}

/// Keep a zcode command's options and drop its session selectors and
/// one-shot prompt options, which would start a new turn on restore.
fn normalize_zcode_argv(argv: &mut Vec<String>) {
    let original = std::mem::take(argv);
    let mut args = original.into_iter();
    argv.extend(args.next());
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-c" | "--continue" | "--target-replace" => {}
            "--resume" | "-p" | "--prompt" | "--target" | "--attach" => {
                args.next();
            }
            _ if ["--resume=", "--prompt=", "--target=", "--attach="]
                .iter()
                .any(|prefix| arg.starts_with(prefix)) => {}
            _ => argv.push(arg),
        }
    }
}

fn normalize_agy_argv(argv: &mut Vec<String>) {
    let original = std::mem::take(argv);
    let Some(program) = original.first() else {
        return;
    };
    argv.push(program.clone());
    let mut index = 1;
    while index < original.len() {
        let argument = &original[index];
        if matches!(
            argument.as_str(),
            "-c" | "--continue"
                | "--new-project"
                | "-i"
                | "--prompt-interactive"
                | "-p"
                | "--print"
                | "--prompt"
        ) {
            index += 1;
            continue;
        }
        if argument == "--conversation" {
            index += 1;
            if original
                .get(index)
                .is_some_and(|value| !value.starts_with('-'))
            {
                index += 1;
            }
            continue;
        }
        if argument.starts_with("--conversation=") {
            index += 1;
            continue;
        }
        if !argument.starts_with('-') {
            // A bare Agy argument can select a subcommand or supply input.
            // Replaying it during exact restoration is not safe.
            index += 1;
            continue;
        }

        argv.push(argument.clone());
        index += 1;
        let takes_value = matches!(
            argument
                .split_once('=')
                .map_or(argument.as_str(), |pair| pair.0),
            "--add-dir"
                | "--agent"
                | "--effort"
                | "--input-format"
                | "--json-schema"
                | "--log-file"
                | "--mode"
                | "--model"
                | "--output-format"
                | "--print-timeout"
                | "--project"
        );
        if !argument.contains('=') && takes_value {
            if let Some(value) = original.get(index) {
                argv.push(value.clone());
                index += 1;
            }
        }
    }
}

fn remove_native_resume_selector(harness: &AgentHarness, argv: &mut Vec<String>) -> bool {
    match harness {
        AgentHarness::Agy => normalize_agy_argv(argv),
        AgentHarness::Claude => normalize_claude_argv(argv),
        AgentHarness::Codex => {
            if let Some(resume) = argv.iter().position(|arg| arg == "resume") {
                argv.truncate(resume);
            }
        }
        AgentHarness::Zcode => normalize_zcode_argv(argv),
        _ => return false,
    }
    true
}

pub fn native_restore_launch_command(
    harness: &AgentHarness,
    process: &LocalProcessInfo,
) -> Option<String> {
    let process = harness_tui_process(harness, process)?;
    let mut argv = if process.argv.is_empty() {
        vec![process.executable.to_string_lossy().into_owned()]
    } else {
        process.argv.clone()
    };
    if !remove_native_resume_selector(harness, &mut argv) {
        return None;
    }
    if argv.is_empty() {
        return None;
    }
    Some(
        argv.iter()
            .map(|arg| shell_words::quote(arg))
            .collect::<Vec<_>>()
            .join(" "),
    )
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

pub(crate) fn registered_harness_process<'a>(
    harness: &AgentHarness,
    process: &'a LocalProcessInfo,
) -> Option<&'a LocalProcessInfo> {
    if matches!(harness, AgentHarness::Unknown) {
        return Some(process);
    }
    let selected = foreground_harness_process(process)?;
    (infer_harness_from_process_info(selected) == *harness).then_some(selected)
}

pub(crate) fn exact_harness_process<'a>(
    metadata: &AgentMetadata,
    process: &'a LocalProcessInfo,
) -> Option<&'a LocalProcessInfo> {
    if metadata.adopted_pid == Some(process.pid)
        && metadata.adopted_start_time == Some(process.start_time)
    {
        return registered_harness_process(&infer_harness(&metadata.launch_cmd, None), process)
            .filter(|selected| selected.pid == process.pid);
    }
    process
        .children
        .values()
        .find_map(|child| exact_harness_process(metadata, child))
}

pub(crate) fn observed_foreground_process_name(
    metadata: &AgentMetadata,
    process: Option<&LocalProcessInfo>,
    fallback: Option<String>,
) -> Option<String> {
    process
        .and_then(|root| {
            registered_harness_process(&infer_harness(&metadata.launch_cmd, None), root)
        })
        .filter(|selected| {
            metadata.adopted_pid == Some(selected.pid)
                && metadata.adopted_start_time == Some(selected.start_time)
        })
        .and_then(|selected| {
            default_launch_cmd_for_harness(&infer_harness_from_process_info(selected))
                .map(str::to_string)
        })
        .or(fallback)
}

pub fn agent_metadata_matches_process_info(
    metadata: &AgentMetadata,
    process: Option<&LocalProcessInfo>,
) -> bool {
    if metadata.codex_app_server.is_some() {
        let Some(process) = process else {
            return true;
        };
        let Some(remote_tui) = remote_codex_tui(process) else {
            if metadata.adopted_pid.is_some() {
                return false;
            }
            // Another harness, or a plain Codex TUI, replaced the managed
            // frontend before it was confirmed, for example after the
            // restored TUI exited to its shell.
            if [
                AgentHarness::Agy,
                AgentHarness::Claude,
                AgentHarness::Codex,
                AgentHarness::Gemini,
                AgentHarness::Opencode,
                AgentHarness::Zcode,
            ]
            .iter()
            .any(|harness| harness_tui_process(harness, process).is_some())
            {
                return false;
            }
            // A restored pane starts as a login shell before execing the
            // remote TUI. There is no contradictory process identity yet.
            return true;
        };
        return match (metadata.adopted_pid, metadata.adopted_start_time) {
            (Some(pid), Some(start_time)) => {
                remote_tui.pid == pid && remote_tui.start_time == start_time
            }
            (None, None) => true,
            _ => false,
        };
    }

    let Some(adopted_pid) = metadata.adopted_pid else {
        return true;
    };
    let Some(adopted_start_time) = metadata.adopted_start_time else {
        return true;
    };
    let Some(process) = process else {
        return true;
    };
    if process.pid == adopted_pid && process.start_time == adopted_start_time {
        // Older registrations may name a persistent launcher shell. Its PID
        // alone cannot keep a harness registered after the child has exited.
        return registered_harness_process(&infer_harness(&metadata.launch_cmd, None), process)
            .is_some();
    }
    // A suspended harness remains a child of the foreground shell.
    process
        .children
        .values()
        .any(|child| agent_metadata_matches_process_info(metadata, Some(child)))
}

pub fn derive_runtime_status(runtime: &AgentRuntimeSnapshot) -> AgentStatus {
    if !runtime.alive {
        return AgentStatus::Exited;
    }

    if runtime.observer_error.is_some()
        || matches!(runtime.terminal_progress, Progress::Error(_))
        || matches!(
            runtime.turn_phase.as_deref(),
            Some("failed" | "systemError")
        )
    {
        return AgentStatus::Errored;
    }

    if matches!(
        runtime.attention_reason.as_deref(),
        Some("approval-requested")
    ) {
        return AgentStatus::Busy;
    }

    match runtime.turn_state {
        AgentTurnState::WaitingOnAgent => return AgentStatus::Busy,
        AgentTurnState::WaitingOnUser => return AgentStatus::Idle,
        AgentTurnState::Unknown => {}
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

const CLAUDE_WAITING_PHASE_PREFIX: &str = "waiting for ";
const CLAUDE_SHELL_PHASE: &str = "shell";

/// Why typed input would not reach the harness's prompt: a dialog or
/// question has the keyboard, or input would run in a shell mode.
pub fn input_blocked_reason(runtime: &AgentRuntimeSnapshot) -> Option<String> {
    let phase = runtime.turn_phase.as_deref()?;
    if let Some(waiting_for) = phase.strip_prefix(CLAUDE_WAITING_PHASE_PREFIX) {
        return Some(format!("the target is waiting for {waiting_for}"));
    }
    (phase == CLAUDE_SHELL_PHASE).then(|| "the target is in shell mode".to_string())
}

fn derive_effective_turn_state(runtime: &AgentRuntimeSnapshot) -> AgentTurnState {
    if !runtime.alive {
        return AgentTurnState::Unknown;
    }

    if matches!(runtime.turn_state, AgentTurnState::WaitingOnAgent) {
        return AgentTurnState::WaitingOnAgent;
    }

    if matches!(runtime.turn_state, AgentTurnState::WaitingOnUser)
        && (matches!(
            runtime.turn_phase.as_deref(),
            Some("aborted" | "interrupted" | "cancelled" | "idle")
        ) || input_blocked_reason(runtime).is_some())
    {
        return AgentTurnState::WaitingOnUser;
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

    if matches!(runtime.turn_phase.as_deref(), Some("failed")) {
        return Some("turn-failed".to_string());
    }

    if matches!(runtime.turn_phase.as_deref(), Some("systemError")) {
        return Some("system-error".to_string());
    }

    if matches!(
        runtime.turn_phase.as_deref(),
        Some("aborted" | "interrupted" | "cancelled")
    ) {
        return Some("turn-aborted".to_string());
    }

    if (matches!(
        runtime.attention_reason.as_deref(),
        Some("approval-requested")
    ) || runtime
        .turn_phase
        .as_deref()
        .is_some_and(|phase| phase.starts_with(CLAUDE_WAITING_PHASE_PREFIX)))
        && matches!(runtime.turn_state, AgentTurnState::WaitingOnUser)
    {
        return Some("approval-requested".to_string());
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
    refresh_runtime_from_harness_with_expected_session(runtime, metadata, None);
}

pub(crate) fn refresh_runtime_from_harness_with_expected_session(
    runtime: &mut AgentRuntimeSnapshot,
    metadata: &AgentMetadata,
    expected_session: Option<&ExpectedAgentSession>,
) {
    runtime.background_job = None;
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
        AgentHarness::Agy => observe_agy(
            cwd,
            runtime.session_path.as_deref(),
            runtime.observer_started_at,
            metadata.adopted_pid,
            metadata.adopted_start_time,
            expected_session
                .filter(|expected| expected.harness == AgentHarness::Agy)
                .map(|expected| expected.session_id.as_str()),
        ),
        AgentHarness::Claude => observe_claude_process(cwd, metadata, runtime, expected_session)
            .map(|(observation, background_job)| {
                runtime.background_job = background_job;
                observation
            }),
        AgentHarness::Codex => observe_codex(
            cwd,
            runtime.session_path.as_deref(),
            runtime.observer_started_at,
            metadata.adopted_pid,
            metadata.adopted_start_time,
            expected_session
                .filter(|expected| expected.harness == AgentHarness::Codex)
                .map(|expected| expected.session_id.as_str()),
        ),
        AgentHarness::Gemini => observe_gemini(
            cwd,
            runtime.session_path.as_deref(),
            runtime.observer_started_at,
        ),
        AgentHarness::Opencode | AgentHarness::Zcode => observe_opencode(
            &observing_harness,
            cwd,
            runtime.session_path.as_deref(),
            runtime.observer_started_at,
            expected_session
                .filter(|expected| expected.harness == observing_harness)
                .map(|expected| expected.session_id.clone())
                .or_else(|| zcode_resumed_session(&observing_harness, &metadata.launch_cmd))
                .as_deref(),
        ),
        AgentHarness::Unknown => Ok(None),
    };

    match observed {
        Ok(Some(snapshot)) => {
            runtime.session_path = snapshot.session_path;
            runtime.progress_summary = snapshot.progress_summary;
            runtime.harness_mode = snapshot.harness_mode;
            // Derived again from this observation's phase.
            runtime.attention_reason = None;
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
    if metadata.launch_supervisor.is_some() && runtime.harness == AgentHarness::Claude {
        return Some("waiting for Claude's exact process and namespace session record".to_string());
    }
    match runtime.harness {
        AgentHarness::Agy => describe_pending_agy_observer(cwd, updated_after)
            .ok()
            .flatten(),
        AgentHarness::Claude => describe_pending_claude_observer(cwd, updated_after)
            .ok()
            .flatten(),
        AgentHarness::Codex => describe_pending_codex_observer(cwd, updated_after)
            .ok()
            .flatten(),
        AgentHarness::Gemini => describe_pending_gemini_observer(cwd, updated_after)
            .ok()
            .flatten(),
        AgentHarness::Opencode | AgentHarness::Zcode => {
            describe_pending_opencode_observer(&runtime.harness, cwd, updated_after)
                .ok()
                .flatten()
        }
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

fn observe_agy(
    _cwd: &str,
    preferred_session: Option<&str>,
    _updated_after: Option<DateTime<Utc>>,
    process_id: Option<u32>,
    process_start_time: Option<u64>,
    expected_conversation_id: Option<&str>,
) -> anyhow::Result<Option<HarnessObservation>> {
    let Some(root) = agy_root() else {
        return Ok(None);
    };
    let Some(transcript) = agy_session_owned_by_process(
        &root,
        process_id,
        process_start_time,
        preferred_session,
        expected_conversation_id,
    )?
    else {
        return Ok(None);
    };
    let modified_at = DateTime::<Utc>::from(fs::metadata(&transcript)?.modified()?);
    let details = read_last_agy_observation(&transcript)?;
    Ok(Some(HarnessObservation {
        session_path: Some(transcript.to_string_lossy().to_string()),
        progress_summary: details.progress_summary,
        harness_mode: details.harness_mode,
        turn_phase: details.turn_phase,
        updated_at: details.updated_at.or(Some(modified_at)),
        turn_state: details.turn_state,
        last_turn_completed_at: details.last_turn_completed_at,
        observed_turn: details.observed_turn,
    }))
}

#[cfg(target_os = "linux")]
fn agy_session_owned_by_process(
    root: &Path,
    process_id: Option<u32>,
    process_start_time: Option<u64>,
    preferred_session: Option<&str>,
    expected_conversation_id: Option<&str>,
) -> anyhow::Result<Option<PathBuf>> {
    let (Some(process_id), Some(process_start_time)) = (process_id, process_start_time) else {
        return Ok(None);
    };
    if linux_process_started_at(Some(process_id), Some(process_start_time)).is_none() {
        return Ok(None);
    }

    let presence_root = root.join("presence");
    let canonical_presence_root = presence_root
        .canonicalize()
        .unwrap_or_else(|_| presence_root.clone());
    let canonical_preferred = preferred_session
        .map(Path::new)
        .map(|path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf()));
    let mut candidates = Vec::new();

    let Ok(entries) = fs::read_dir(format!("/proc/{process_id}/fd")) else {
        return Ok(None);
    };
    for entry in entries.flatten() {
        let Ok(path) = fs::read_link(entry.path()) else {
            continue;
        };
        let canonical_path = path.canonicalize().unwrap_or(path);
        if canonical_path.parent() != Some(canonical_presence_root.as_path())
            || canonical_path.extension().and_then(|ext| ext.to_str()) != Some("lock")
        {
            continue;
        }
        let Some(conversation_id) = canonical_path.file_stem().and_then(|name| name.to_str())
        else {
            continue;
        };
        if !is_uuid(conversation_id) {
            continue;
        }
        if expected_conversation_id.is_some_and(|expected| expected != conversation_id) {
            continue;
        }
        let transcript = root
            .join("brain")
            .join(conversation_id)
            .join(".system_generated")
            .join("logs")
            .join("transcript.jsonl");
        if !transcript.is_file() {
            continue;
        }
        let canonical_transcript = transcript.canonicalize().unwrap_or(transcript);
        if canonical_preferred.as_ref() == Some(&canonical_transcript) {
            return Ok(Some(canonical_transcript));
        }
        candidates.push(canonical_transcript);
    }

    candidates.sort_unstable();
    candidates.dedup();
    Ok((candidates.len() == 1).then(|| candidates.remove(0)))
}

#[cfg(not(target_os = "linux"))]
fn agy_session_owned_by_process(
    _root: &Path,
    _process_id: Option<u32>,
    _process_start_time: Option<u64>,
    _preferred_session: Option<&str>,
    _expected_conversation_id: Option<&str>,
) -> anyhow::Result<Option<PathBuf>> {
    Ok(None)
}

fn read_last_agy_observation(path: &Path) -> anyhow::Result<HarnessObservationDetails> {
    let conversation_id = path
        .ancestors()
        .nth(3)
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .unwrap_or("agy");
    let mut latest_record = None;
    let mut latest_cursor = None;
    let mut latest_at = None;
    let mut user_record = None;

    visit_lines_reverse(path, |line| {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            return Ok(false);
        };
        let step_index = record.get("step_index").and_then(Value::as_u64);
        let record_at = record
            .get("created_at")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339_timestamp);
        if latest_record.is_none()
            && record.get("type").and_then(Value::as_str) != Some("SYSTEM_MESSAGE")
        {
            latest_cursor = step_index;
            latest_at = record_at;
            latest_record = Some(record.clone());
        }
        if record.get("type").and_then(Value::as_str) == Some("USER_INPUT") {
            user_record = Some(record);
            return Ok(true);
        }
        Ok(false)
    })?;

    let Some(latest) = latest_record else {
        return Ok(HarnessObservationDetails {
            progress_summary: None,
            harness_mode: None,
            turn_phase: None,
            updated_at: None,
            turn_state: AgentTurnState::Unknown,
            last_turn_completed_at: None,
            observed_turn: None,
        });
    };
    let Some(user) = user_record else {
        return Ok(HarnessObservationDetails {
            progress_summary: None,
            harness_mode: None,
            turn_phase: None,
            updated_at: latest_at,
            turn_state: AgentTurnState::Unknown,
            last_turn_completed_at: None,
            observed_turn: None,
        });
    };

    let user_cursor = user.get("step_index").and_then(Value::as_u64);
    let user_at = user
        .get("created_at")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339_timestamp);
    let user_message = user
        .get("content")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|message| !message.is_empty());
    let status = latest.get("status").and_then(Value::as_str);
    let final_message = (latest.get("type").and_then(Value::as_str) == Some("PLANNER_RESPONSE")
        && status == Some("DONE")
        && latest
            .get("tool_calls")
            .and_then(Value::as_array)
            .map(Vec::is_empty)
            .unwrap_or(true))
    .then(|| latest.get("content").and_then(Value::as_str))
    .flatten()
    .map(str::trim)
    .filter(|message| !message.is_empty())
    .map(str::to_string);
    let aborted = matches!(
        status,
        Some("ERROR" | "CANCELED" | "CANCELLED" | "INTERRUPTED" | "HALTED" | "INVALID")
    );
    let (outcome, turn_state, completed_at, turn_phase) = if final_message.is_some() {
        (
            AgentObservedTurnOutcome::Completed,
            AgentTurnState::WaitingOnUser,
            latest_at,
            Some("final_answer".to_string()),
        )
    } else if aborted {
        (
            AgentObservedTurnOutcome::Aborted,
            AgentTurnState::WaitingOnUser,
            latest_at,
            Some("aborted".to_string()),
        )
    } else {
        (
            AgentObservedTurnOutcome::Running,
            AgentTurnState::WaitingOnAgent,
            None,
            Some("working".to_string()),
        )
    };
    let provider_turn_id = format!(
        "{conversation_id}:{}",
        user_cursor
            .map(|cursor| cursor.to_string())
            .unwrap_or_else(|| "current".to_string())
    );
    let progress_summary = final_message.as_deref().map(truncate_summary);

    Ok(HarnessObservationDetails {
        progress_summary,
        harness_mode: None,
        turn_phase,
        updated_at: latest_at,
        turn_state,
        last_turn_completed_at: completed_at,
        observed_turn: Some(AgentObservedTurn {
            provider_turn_id,
            outcome,
            started_at: user_at,
            completed_at,
            started_cursor: user_cursor,
            latest_cursor,
            primary_user_message_sha256: user_message.map(message_sha256),
            user_message_count: 1,
            final_message,
        }),
    })
}

fn is_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

fn settle_codex_turn_interrupted_by_process_restart(
    details: &mut HarnessObservationDetails,
    session_modified_at: std::time::SystemTime,
    process_started_at: Option<std::time::SystemTime>,
) {
    let Some(process_started_at) = process_started_at else {
        return;
    };
    let Some(turn) = details.observed_turn.as_mut() else {
        return;
    };
    if !matches!(turn.outcome, AgentObservedTurnOutcome::Running)
        || session_modified_at >= process_started_at
    {
        return;
    }

    details.turn_state = AgentTurnState::WaitingOnUser;
    details.turn_phase = Some("interrupted".to_string());
    turn.outcome = AgentObservedTurnOutcome::Aborted;
    turn.completed_at = Some(DateTime::<Utc>::from(process_started_at));
}

fn observe_claude_process(
    cwd: &str,
    metadata: &AgentMetadata,
    runtime: &AgentRuntimeSnapshot,
    expected: Option<&ExpectedAgentSession>,
) -> anyhow::Result<(Option<HarnessObservation>, Option<AgentBackgroundJob>)> {
    let expected_id = expected
        .filter(|expected| expected.harness == AgentHarness::Claude)
        .map(|expected| expected.session_id.as_str());
    let owned = match claude_session_owned_by_process(
        cwd,
        metadata.adopted_pid,
        metadata.adopted_start_time,
        &metadata.launch_cmd,
    )? {
        ClaudeOwnership::Owned(owned) => Some(owned),
        // Guessing from transcript timestamps would bind an unrelated session.
        ClaudeOwnership::UnresolvedJob { job_id, session_id } => {
            anyhow::bail!(
                "{}",
                dead_claude_job_message(&metadata.launch_cmd, &job_id, session_id.as_deref())
            )
        }
        ClaudeOwnership::Unknown => None,
    };
    if metadata.launch_supervisor.is_some() && owned.is_none() {
        // A sandbox may run several PID 2 processes or a different session in
        // the same project. Do not confirm it by transcript timestamps.
        return Ok((None, None));
    }
    if let Some(owned) = owned.as_ref() {
        if expected_id.is_some_and(|expected| expected != owned.launched_id) {
            return Ok((None, None));
        }
    }
    let background_job = owned
        .as_ref()
        .and_then(|owned| Some((owned.job_id.clone()?, owned.current_id.clone())))
        .map(|(job_id, session_id)| AgentBackgroundJob {
            hint: running_claude_job_message(&metadata.launch_cmd, &job_id, &session_id),
            job_id,
            session_id,
        });
    let mut observation = observe_claude(
        cwd,
        runtime.session_path.as_deref(),
        runtime.observer_started_at,
        owned
            .as_ref()
            .map(|owned| owned.current_id.as_str())
            .or(expected_id),
    )?;
    // Claude's own report outranks inference from the transcript, which
    // cannot tell a running turn from input Claude showed without answering.
    if let (Some(observation), Some(reported)) = (
        observation.as_mut(),
        owned.as_ref().and_then(|owned| owned.status.as_ref()),
    ) {
        match reported.status.as_str() {
            "busy" => {
                observation.turn_state = AgentTurnState::WaitingOnAgent;
                observation.turn_phase = None;
                // Starting work acknowledges the input that started it.
                observation.updated_at = observation.updated_at.max(reported.changed_at);
            }
            "idle" => {
                // A turn Wakterm saw running ended when Claude went idle,
                // including a turn that ended without a reply. An idle report
                // alone may date from the process start.
                if matches!(runtime.turn_state, AgentTurnState::WaitingOnAgent) {
                    observation.last_turn_completed_at =
                        observation.last_turn_completed_at.max(reported.changed_at);
                }
                // A turn Claude left without a reply is over once it is idle.
                let settled = observation
                    .session_path
                    .as_deref()
                    .is_some_and(|path| claude_transcript_settled(Path::new(path)));
                if let Some(turn) = observation.observed_turn.as_mut().filter(|turn| {
                    settled && matches!(turn.outcome, AgentObservedTurnOutcome::Running)
                }) {
                    turn.outcome = AgentObservedTurnOutcome::Aborted;
                    turn.completed_at = reported.changed_at;
                }
                observation.turn_state = AgentTurnState::WaitingOnUser;
                observation.turn_phase = Some("idle".to_string());
            }
            "waiting" => {
                observation.turn_state = AgentTurnState::WaitingOnUser;
                observation.turn_phase = Some(format!(
                    "{CLAUDE_WAITING_PHASE_PREFIX}{}",
                    reported.waiting_for.as_deref().unwrap_or("input")
                ));
            }
            "shell" => {
                observation.turn_state = AgentTurnState::WaitingOnUser;
                observation.turn_phase = Some(CLAUDE_SHELL_PHASE.to_string());
            }
            _ => {}
        }
    }
    Ok((observation, background_job))
}

/// Whether the agent is Claude and its exact process has not yet written a
/// session record. Claude writes one only after startup prompts, such as
/// folder trust, are answered.
pub fn claude_session_record_missing(metadata: &AgentMetadata) -> bool {
    infer_harness(&metadata.launch_cmd, None) == AgentHarness::Claude
        && !matches!(
            claude_session_owned_by_process(
                &normalize_declared_cwd(&metadata.declared_cwd),
                metadata.adopted_pid,
                metadata.adopted_start_time,
                &metadata.launch_cmd,
            ),
            Ok(ClaudeOwnership::Owned(_))
        )
}

/// Whether Claude recorded a status change for the agent's exact process
/// after `since`. None when the agent has no confirmed Claude session record.
pub fn claude_status_changed_since(
    metadata: &AgentMetadata,
    since: std::time::SystemTime,
) -> Option<bool> {
    if infer_harness(&metadata.launch_cmd, None) != AgentHarness::Claude {
        return None;
    }
    let ClaudeOwnership::Owned(owned) = claude_session_owned_by_process(
        &normalize_declared_cwd(&metadata.declared_cwd),
        metadata.adopted_pid,
        metadata.adopted_start_time,
        &metadata.launch_cmd,
    )
    .ok()?
    else {
        return None;
    };
    let changed_at = owned.status?.changed_at?;
    Some(changed_at > DateTime::<Utc>::from(since))
}

/// Resume a Claude session in the pane with the pane's own launch flags.
/// An attach client has none, so it gets a plain command.
fn claude_pane_resume_command(launch_cmd: &str, session_id: &str) -> (String, bool) {
    let attached = claude_attach_job_id(launch_cmd).is_some();
    let command = native_resume_command(&AgentHarness::Claude, launch_cmd, session_id)
        .map(|command| {
            command
                .get_argv()
                .iter()
                .map(|arg| shell_words::quote(&arg.to_string_lossy()).into_owned())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_else(|_| format!("claude --resume {session_id}"));
    (command, attached)
}

fn running_claude_job_message(launch_cmd: &str, job_id: &str, session_id: &str) -> String {
    let (resume, attached) = claude_pane_resume_command(launch_cmd, session_id);
    format!(
        "This conversation runs as Claude background job {job_id}, outside this pane, \
         and a mux restart will stop it. To move it into this pane, exit Claude, then run \
         `claude stop {job_id}` and `{resume}`{}.",
        if attached {
            " with your usual flags"
        } else {
            ""
        }
    )
}

fn dead_claude_job_message(launch_cmd: &str, job_id: &str, session_id: Option<&str>) -> String {
    let Some(session_id) = session_id else {
        return format!(
            "Claude background job {job_id} is not running, and its session could not be found."
        );
    };
    let (resume, attached) = claude_pane_resume_command(launch_cmd, session_id);
    format!(
        "Claude background job {job_id} is not running. To continue the conversation in \
         this pane, run `{resume}`{}.",
        if attached {
            " with your usual flags"
        } else {
            ""
        }
    )
}

enum ClaudeOwnership {
    Owned(OwnedClaudeSession),
    /// The process shows a background job, but no live worker runs it.
    /// The session comes from the job's saved state when it is known.
    UnresolvedJob {
        job_id: String,
        session_id: Option<String>,
    },
    Unknown,
}

/// The Claude session a live process shows. A TUI that moves its session to
/// the background keeps the registry's `sessionId` at the launched session
/// and records `parkedJobId`; `claude attach <job>` has no registry record.
/// Either way the conversation belongs to the background worker whose
/// registry record has that `jobId`.
struct OwnedClaudeSession {
    launched_id: String,
    current_id: String,
    job_id: Option<String>,
    /// Claude's own report of the process.
    status: Option<ClaudeReportedStatus>,
}

/// The status Claude records for a process: `busy`, `idle`, `waiting` while
/// a dialog or question has the keyboard, or `shell` in its shell mode.
struct ClaudeReportedStatus {
    status: String,
    /// What a `waiting` process waits for, such as "dialog open".
    waiting_for: Option<String>,
    /// When the status last changed.
    changed_at: Option<DateTime<Utc>>,
}

/// The session a stopped or dead background job was running.
fn claude_job_saved_session(jobs_dir: &Path, job_id: &str) -> Option<String> {
    let bytes = fs::read(jobs_dir.join(job_id).join("state.json")).ok()?;
    let state: Value = serde_json::from_slice(&bytes).ok()?;
    state
        .get("sessionId")
        .and_then(Value::as_str)
        .filter(|id| is_uuid(id))
        .map(str::to_string)
}

fn claude_attach_job_id(launch_cmd: &str) -> Option<String> {
    let argv = shell_words::split(launch_cmd).ok()?;
    match argv.as_slice() {
        [_, command, job_id] if command == "attach" => Some(job_id.clone()),
        _ => None,
    }
}

#[cfg(target_os = "linux")]
fn claude_background_session(
    sessions_dir: &Path,
    job_id: &str,
    cwd: &str,
) -> anyhow::Result<Option<String>> {
    let entries = match fs::read_dir(sessions_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Ok(record) = serde_json::from_slice::<Value>(&fs::read(&path)?) else {
            continue;
        };
        if record.get("kind").and_then(Value::as_str) != Some("bg")
            || record.get("jobId").and_then(Value::as_str) != Some(job_id)
            || record.get("cwd").and_then(Value::as_str) != Some(cwd)
        {
            continue;
        }
        let pid = record
            .get("pid")
            .and_then(Value::as_u64)
            .and_then(|pid| <u32 as std::convert::TryFrom<u64>>::try_from(pid).ok());
        let start = record
            .get("procStart")
            .and_then(Value::as_str)
            .and_then(|start| start.parse::<u64>().ok());
        if linux_process_started_at(pid, start).is_none() {
            continue;
        }
        if let Some(session_id) = record
            .get("sessionId")
            .and_then(Value::as_str)
            .filter(|id| is_uuid(id))
        {
            return Ok(Some(session_id.to_string()));
        }
    }
    Ok(None)
}

#[cfg(target_os = "linux")]
fn claude_session_owned_by_process(
    cwd: &str,
    pid: Option<u32>,
    start_time: Option<u64>,
    launch_cmd: &str,
) -> anyhow::Result<ClaudeOwnership> {
    let (Some(pid), Some(start_time)) = (pid, start_time) else {
        return Ok(ClaudeOwnership::Unknown);
    };
    if linux_process_started_at(Some(pid), Some(start_time)).is_none() {
        return Ok(ClaudeOwnership::Unknown);
    }
    let Some(root) = claude_sessions_root() else {
        return Ok(ClaudeOwnership::Unknown);
    };
    let status = match fs::read_to_string(format!("/proc/{pid}/status")) {
        Ok(status) => status,
        Err(_) => return Ok(ClaudeOwnership::Unknown),
    };
    let Some(namespace_pid) = status
        .lines()
        .find_map(|line| line.strip_prefix("NSpid:"))
        .and_then(|ids| ids.split_whitespace().last())
        .and_then(|pid| pid.parse::<u32>().ok())
    else {
        return Ok(ClaudeOwnership::Unknown);
    };
    let sessions_dir = root
        .parent()
        .context("Claude projects root has no parent")?
        .join("sessions");
    let registry = sessions_dir.join(format!("{namespace_pid}.json"));
    let (launched_id, job_id, status) =
        match fs::read(&registry) {
            Ok(bytes) => {
                let record: Value = serde_json::from_slice(&bytes)?;
                let machine_id = fs::read_to_string(format!("/proc/{pid}/root/etc/machine-id"))?;
                let namespace = fs::read_link(format!("/proc/{pid}/ns/pid"))?;
                let domain = format!(
                    "linux:{}:{}",
                    machine_id.trim(),
                    namespace.to_string_lossy()
                );
                let start = start_time.to_string();
                if record.get("pid").and_then(Value::as_u64) != Some(u64::from(namespace_pid))
                    || record.get("procStart").and_then(Value::as_str) != Some(start.as_str())
                    || record.get("pidDomain").and_then(Value::as_str) != Some(domain.as_str())
                    || record.get("kind").and_then(Value::as_str) != Some("interactive")
                    || record.get("cwd").and_then(Value::as_str) != Some(cwd)
                    || linux_process_started_at(Some(pid), Some(start_time)).is_none()
                {
                    return Ok(ClaudeOwnership::Unknown);
                }
                let Some(session_id) = record
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .filter(|id| is_uuid(id))
                else {
                    return Ok(ClaudeOwnership::Unknown);
                };
                let parked = record
                    .get("parkedJobId")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let status = record.get("status").and_then(Value::as_str).map(|status| {
                    ClaudeReportedStatus {
                        status: status.to_string(),
                        waiting_for: record
                            .get("waitingFor")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        changed_at: record
                            .get("statusUpdatedAt")
                            .and_then(Value::as_i64)
                            .and_then(DateTime::<Utc>::from_timestamp_millis),
                    }
                });
                (Some(session_id.to_string()), parked, status)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let Some(job_id) = claude_attach_job_id(launch_cmd) else {
                    return Ok(ClaudeOwnership::Unknown);
                };
                (None, Some(job_id), None)
            }
            Err(error) => return Err(error.into()),
        };
    let current_id = match job_id.as_deref() {
        Some(job_id) => match claude_background_session(&sessions_dir, job_id, cwd)? {
            Some(session_id) => session_id,
            None => {
                let jobs_dir = sessions_dir.with_file_name("jobs");
                return Ok(ClaudeOwnership::UnresolvedJob {
                    job_id: job_id.to_string(),
                    session_id: claude_job_saved_session(&jobs_dir, job_id),
                });
            }
        },
        None => launched_id.clone().expect("registry session without a job"),
    };
    let path = root
        .join(claude_project_dir_name(cwd))
        .join(format!("{current_id}.jsonl"));
    if !path.is_file()
        || claude_session_id(&path)?.as_deref() != Some(current_id.as_str())
        || !claude_session_is_interactive(&path)?
    {
        return Ok(ClaudeOwnership::Unknown);
    }
    Ok(ClaudeOwnership::Owned(OwnedClaudeSession {
        launched_id: launched_id.unwrap_or_else(|| current_id.clone()),
        current_id,
        job_id,
        status,
    }))
}

#[cfg(not(target_os = "linux"))]
fn claude_session_owned_by_process(
    _cwd: &str,
    _pid: Option<u32>,
    _start_time: Option<u64>,
    _launch_cmd: &str,
) -> anyhow::Result<ClaudeOwnership> {
    Ok(ClaudeOwnership::Unknown)
}

fn observe_claude(
    cwd: &str,
    preferred_session: Option<&str>,
    updated_after: Option<DateTime<Utc>>,
    expected_session_id: Option<&str>,
) -> anyhow::Result<Option<HarnessObservation>> {
    let Some(root) = claude_sessions_root() else {
        return Ok(None);
    };
    let project_dir = root.join(claude_project_dir_name(cwd));
    if !project_dir.is_dir() {
        return Ok(None);
    }

    if let Some(expected_session_id) = expected_session_id {
        let expected_path = project_dir.join(format!("{expected_session_id}.jsonl"));
        if !expected_path.is_file()
            || claude_session_id(&expected_path)?.as_deref() != Some(expected_session_id)
        {
            return Ok(None);
        }
        let modified_at = DateTime::<Utc>::from(fs::metadata(&expected_path)?.modified()?);
        let details = read_last_claude_observation(&expected_path)?;
        return Ok(Some(HarnessObservation {
            session_path: Some(expected_path.to_string_lossy().to_string()),
            progress_summary: details.progress_summary,
            harness_mode: details.harness_mode,
            turn_phase: details.turn_phase,
            updated_at: details.updated_at.or(Some(modified_at)),
            turn_state: details.turn_state,
            last_turn_completed_at: details.last_turn_completed_at,
            observed_turn: details.observed_turn,
        }));
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
    expected_session_id: Option<&str>,
) -> anyhow::Result<Option<HarnessObservation>> {
    let process_session = match codex_session_owned_by_process(
        cwd,
        process_id,
        process_start_time,
        preferred_session,
        expected_session_id,
    )? {
        Some(path) => Some((path, true)),
        None => remote_codex_tui_session(process_id, process_start_time, expected_session_id)?
            .map(|path| (path, false)),
    };
    if let Some((process_session, owned_by_process)) = process_session {
        let modified = fs::metadata(&process_session)?.modified()?;
        let modified_at = DateTime::<Utc>::from(modified);
        let mut details = read_last_codex_observation(&process_session)?;
        // A remote TUI's turn runs in its app-server, which outlives the TUI.
        #[cfg(target_os = "linux")]
        if owned_by_process {
            settle_codex_turn_interrupted_by_process_restart(
                &mut details,
                modified,
                linux_process_started_at(process_id, process_start_time),
            );
        }
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

    let Some(root) = codex_sessions_root() else {
        return Ok(None);
    };

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
        if preferred_path.is_file() && codex_session_matches_cwd(preferred_path, cwd)? {
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
    harness: &AgentHarness,
    cwd: &str,
    preferred_session: Option<&str>,
    updated_after: Option<DateTime<Utc>>,
    exact_session: Option<&str>,
) -> anyhow::Result<Option<HarnessObservation>> {
    let Some(db_path) = opencode_schema_db_path(harness) else {
        return Ok(None);
    };
    if !db_path.is_file() {
        return Ok(None);
    }

    let connection =
        Connection::open_with_flags(&db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.busy_timeout(std::time::Duration::from_secs(2))?;

    // A session named on the command line or by restore is exact; the
    // directory and recency selection below is only a fallback.
    if let Some(session_id) = exact_session {
        return read_last_opencode_observation(&connection, &db_path, session_id, None);
    }

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

/// Claude names a project's transcript directory after its working
/// directory, with every character other than an ASCII letter or digit
/// replaced by `-`.
fn claude_project_dir_name(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn describe_pending_claude_observer(
    cwd: &str,
    updated_after: Option<DateTime<Utc>>,
) -> anyhow::Result<Option<String>> {
    let Some(root) = claude_sessions_root() else {
        return Ok(None);
    };
    let project_dir = root.join(claude_project_dir_name(cwd));
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

fn describe_pending_agy_observer(
    _cwd: &str,
    _updated_after: Option<DateTime<Utc>>,
) -> anyhow::Result<Option<String>> {
    let Some(root) = agy_root() else {
        return Ok(None);
    };
    Ok(Some(if root.join("presence").is_dir() {
        "agy process has not exposed a matching conversation transcript yet".to_string()
    } else {
        "agy conversation storage has not appeared yet".to_string()
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
    harness: &AgentHarness,
    cwd: &str,
    updated_after: Option<DateTime<Utc>>,
) -> anyhow::Result<Option<String>> {
    let Some(db_path) = opencode_schema_db_path(harness) else {
        return Ok(None);
    };
    let name = default_launch_cmd_for_harness(harness).unwrap_or("agent");
    if !db_path.is_file() {
        return Ok(Some(format!(
            "{name} session database has not appeared yet"
        )));
    }

    let connection =
        Connection::open_with_flags(&db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.busy_timeout(std::time::Duration::from_secs(2))?;

    let has_recent_session = select_opencode_session(&connection, cwd, updated_after)?.is_some();
    let has_session = select_opencode_session(&connection, cwd, None)?.is_some();

    Ok(Some(if has_recent_session {
        format!("{name} session exists but observer has not attached yet")
    } else if has_session {
        format!("{name} session exists but no new turn appeared yet")
    } else {
        format!("{name} session has not appeared yet")
    }))
}

fn claude_sessions_root() -> Option<PathBuf> {
    std::env::var_os("WAKTERM_AGENT_CLAUDE_DIR")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join(".claude").join("projects")))
}

fn agy_root() -> Option<PathBuf> {
    std::env::var_os("WAKETERM_AGENT_AGY_DIR")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join(".gemini").join("antigravity-cli")))
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

/// The session database for a harness that uses OpenCode's schema.
fn opencode_schema_db_path(harness: &AgentHarness) -> Option<PathBuf> {
    match harness {
        AgentHarness::Opencode => opencode_db_path(),
        AgentHarness::Zcode => zcode_db_path(),
        _ => None,
    }
}

fn zcode_db_path() -> Option<PathBuf> {
    std::env::var_os("WAKTERM_AGENT_ZCODE_DB")
        .map(PathBuf::from)
        .or_else(|| {
            home_dir().map(|home| home.join(".zcode").join("cli").join("db").join("db.sqlite"))
        })
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

pub fn native_resume_command(
    harness: &AgentHarness,
    launch_cmd: &str,
    session_id: &str,
) -> anyhow::Result<CommandBuilder> {
    anyhow::ensure!(
        !session_id.trim().is_empty(),
        "agent session ID must not be empty"
    );
    if matches!(harness, AgentHarness::Agy | AgentHarness::Claude) {
        anyhow::ensure!(
            is_uuid(session_id.trim()),
            "{:?} restore requires an exact session UUID",
            harness
        );
    }
    if harness == &AgentHarness::Zcode {
        anyhow::ensure!(
            is_zcode_session_id(session_id.trim()),
            "zcode restore requires an exact sess_ session ID"
        );
    }
    let mut argv = shell_words::split(launch_cmd).context("parsing agent launch command")?;
    anyhow::ensure!(!argv.is_empty(), "agent launch command must not be empty");
    anyhow::ensure!(
        infer_harness(launch_cmd, None) == *harness,
        "restore requires a launch command for {:?}",
        harness
    );
    anyhow::ensure!(
        remove_native_resume_selector(harness, &mut argv),
        "automatic restore is not implemented for {:?}",
        harness
    );
    argv.push(
        match harness {
            AgentHarness::Agy => "--conversation",
            AgentHarness::Claude | AgentHarness::Zcode => "--resume",
            AgentHarness::Codex => "resume",
            _ => unreachable!(),
        }
        .to_string(),
    );
    argv.push(session_id.trim().to_string());
    let mut command = CommandBuilder::from_argv(argv.into_iter().map(OsString::from).collect());
    if harness == &AgentHarness::Claude {
        command.env_remove("CLAUDECODE");
    }
    Ok(command)
}

pub(crate) fn agent_observer_watch_roots(harness: &AgentHarness, cwd: &str) -> Vec<PathBuf> {
    let paths = match harness {
        AgentHarness::Agy => agy_root().map(|root| vec![root.join("presence"), root.join("brain")]),
        AgentHarness::Claude => claude_sessions_root().map(|root| {
            let project = root.join(claude_project_dir_name(cwd));
            let mut paths = if project.is_dir() {
                vec![project]
            } else {
                vec![root.clone()]
            };
            if let Some(parent) = root.parent() {
                paths.push(parent.join("sessions"));
            }
            paths
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
        AgentHarness::Opencode | AgentHarness::Zcode => opencode_schema_db_path(harness)
            .and_then(|path| path.parent().map(|path| vec![path.to_path_buf()])),
        AgentHarness::Unknown => None,
    };

    paths
        .unwrap_or_default()
        .into_iter()
        .filter(|path| path.is_dir())
        .collect()
}

pub(crate) fn agent_observer_artifact_paths(
    harness: &AgentHarness,
    session_path: &str,
) -> Vec<PathBuf> {
    match harness {
        AgentHarness::Opencode | AgentHarness::Zcode => parse_opencode_session_path(session_path)
            .map(|(path, _)| {
                let path = path.to_string_lossy();
                ["", "-wal", "-shm", "-journal"]
                    .iter()
                    .map(|suffix| PathBuf::from(format!("{path}{suffix}")))
                    .collect()
            })
            .unwrap_or_default(),
        AgentHarness::Unknown => vec![],
        _ => vec![PathBuf::from(session_path)],
    }
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

pub fn claude_session_id(path: &Path) -> anyhow::Result<Option<String>> {
    const MAX_SESSION_HEADER_RECORDS: usize = 64;
    let reader = BufReader::new(fs::File::open(path)?);
    for line in reader.lines().take(MAX_SESSION_HEADER_RECORDS) {
        let line = line?;
        let record: Value = serde_json::from_str(&line)
            .with_context(|| format!("parsing Claude session header {}", path.display()))?;
        if let Some(id) = record
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
        {
            anyhow::ensure!(is_uuid(id), "Claude session ID is not a UUID");
            return Ok(Some(id.to_string()));
        }
    }
    Ok(None)
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
    Ok(codex_session_record_is_user_visible(&record)
        && record
            .get("payload")
            .unwrap_or(&record)
            .get("cwd")
            .and_then(Value::as_str)
            == Some(cwd))
}

fn codex_session_record_is_user_visible(record: &Value) -> bool {
    let payload = record.get("payload").unwrap_or(&record);
    match payload.get("thread_source").and_then(Value::as_str) {
        Some("user") | None => !payload.get("source").is_some_and(|source| {
            source.as_object().is_some_and(|source| {
                source.contains_key("subagent") || source.contains_key("internal")
            })
        }),
        Some(_) => false,
    }
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

pub fn restorable_session_id(
    harness: &AgentHarness,
    path: &Path,
) -> anyhow::Result<Option<String>> {
    match harness {
        AgentHarness::Agy => Ok(path
            .ancestors()
            .nth(3)
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .filter(|conversation_id| is_uuid(conversation_id))
            .map(ToOwned::to_owned)),
        AgentHarness::Claude => claude_session_id(path),
        AgentHarness::Codex => codex_session_id(path),
        AgentHarness::Zcode => Ok(path
            .to_str()
            .and_then(parse_opencode_session_path)
            .map(|(_, session_id)| session_id)
            .filter(|session_id| is_zcode_session_id(session_id))),
        _ => Ok(None),
    }
}

#[cfg(target_os = "linux")]
fn linux_process_started_at(
    process_id: Option<u32>,
    expected_start_time: Option<u64>,
) -> Option<std::time::SystemTime> {
    let (Some(process_id), Some(expected_start_time)) = (process_id, expected_start_time) else {
        return None;
    };
    let stat = fs::read_to_string(format!("/proc/{process_id}/stat")).ok()?;
    let (_, fields) = stat.rsplit_once(')')?;
    let actual_start_time = fields.split_whitespace().nth(19)?.parse::<u64>().ok()?;
    if actual_start_time != expected_start_time {
        return None;
    }

    let uptime = fs::read_to_string("/proc/uptime")
        .ok()?
        .split_whitespace()
        .next()?
        .parse::<f64>()
        .ok()?;
    let ticks_per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks_per_second <= 0 {
        return None;
    }
    let booted_at =
        std::time::SystemTime::now().checked_sub(std::time::Duration::from_secs_f64(uptime))?;
    booted_at.checked_add(std::time::Duration::from_secs_f64(
        actual_start_time as f64 / ticks_per_second as f64,
    ))
}

#[cfg(target_os = "linux")]
fn codex_session_owned_by_process(
    cwd: &str,
    process_id: Option<u32>,
    process_start_time: Option<u64>,
    preferred_session: Option<&str>,
    expected_session_id: Option<&str>,
) -> anyhow::Result<Option<PathBuf>> {
    let (Some(process_id), Some(process_start_time)) = (process_id, process_start_time) else {
        return Ok(None);
    };
    if linux_process_started_at(Some(process_id), Some(process_start_time)).is_none() {
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

    let canonical_preferred = preferred_session
        .map(Path::new)
        .map(|path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf()));
    let mut pids = Vec::new();
    collect_pids(process_id, &mut pids);
    let mut selected: Option<(PathBuf, std::time::SystemTime)> = None;
    let mut preferred_match = None;
    let mut expected_match = None;

    for pid in pids {
        let Ok(entries) = fs::read_dir(format!("/proc/{pid}/fd")) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(path) = fs::read_link(entry.path()) else {
                continue;
            };
            let canonical_path = path.canonicalize().unwrap_or(path);
            if !canonical_path
                .file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
                .unwrap_or(false)
                || !codex_session_matches_cwd(&canonical_path, cwd)?
            {
                continue;
            }
            if expected_session_id.is_some_and(|expected| {
                codex_session_id(&canonical_path).ok().flatten().as_deref() == Some(expected)
            }) {
                expected_match = Some(canonical_path.clone());
            }
            if canonical_preferred.as_ref() == Some(&canonical_path) {
                preferred_match = Some(canonical_path.clone());
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

    Ok(expected_match
        .or_else(|| selected.map(|(path, _)| path))
        .or(preferred_match))
}

/// A remote Codex TUI's rollout is held open by its app-server, not by the
/// TUI, so the confirmed process's own `resume <thread>` names its session.
#[cfg(target_os = "linux")]
fn remote_codex_tui_session(
    process_id: Option<u32>,
    process_start_time: Option<u64>,
    expected_session_id: Option<&str>,
) -> anyhow::Result<Option<PathBuf>> {
    let (Some(process_id), Some(process_start_time)) = (process_id, process_start_time) else {
        return Ok(None);
    };
    let Some(remote) = LocalProcessInfo::with_root_pid(process_id)
        .as_ref()
        .and_then(remote_codex_tui)
        .filter(|remote| remote.pid == process_id && remote.start_time == process_start_time)
    else {
        return Ok(None);
    };
    if expected_session_id.is_some_and(|expected| expected != remote.thread_id) {
        return Ok(None);
    }
    let Some(root) = codex_sessions_root() else {
        return Ok(None);
    };
    let suffix = format!("-{}.jsonl", remote.thread_id);
    let mut candidates = Vec::new();
    collect_codex_rollout_sessions(&root, &mut candidates)?;
    for path in candidates {
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(&suffix))
            && codex_session_id(&path)?.as_deref() == Some(remote.thread_id.as_str())
        {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

#[cfg(not(target_os = "linux"))]
fn remote_codex_tui_session(
    _process_id: Option<u32>,
    _process_start_time: Option<u64>,
    _expected_session_id: Option<&str>,
) -> anyhow::Result<Option<PathBuf>> {
    Ok(None)
}

#[cfg(not(target_os = "linux"))]
fn codex_session_owned_by_process(
    _cwd: &str,
    _process_id: Option<u32>,
    _process_start_time: Option<u64>,
    _preferred_session: Option<&str>,
    _expected_session_id: Option<&str>,
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
        (Some(_), Some(assistant_at)) => (AgentTurnState::WaitingOnAgent, Some(assistant_at)),
        (Some(_), None) => (AgentTurnState::WaitingOnAgent, None),
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
    let mut failed = false;
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
            // Notifications and reminders the harness injects continue the
            // current turn rather than starting one.
            "user"
                if last_user_at.is_none()
                    && !crate::agent_event::opencode_runtime_message(&message) =>
            {
                last_user_at = timestamp
            }
            "assistant" if last_assistant_at.is_none() => {
                last_assistant_at = timestamp;
                failed = message.get("error").is_some();
            }
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
        turn_phase: failed.then(|| "failed".to_string()),
        updated_at: parse_unix_millis(updated_millis),
        turn_state,
        last_turn_completed_at,
        observed_turn: None,
    }))
}

/// The text of a Claude user record, or None for a record that carries only
/// tool results.
fn claude_user_text(record: &Value) -> Option<String> {
    match record.get("message")?.get("content")? {
        Value::String(text) => Some(text.clone()),
        Value::Array(blocks) => {
            let texts = blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>();
            (!texts.is_empty()).then(|| texts.join("\n"))
        }
        _ => None,
    }
}

/// Claude stores pasted input inside a `<pasted_content id="N">` wrapper.
/// Returns the pasted text when the message is exactly one such paste.
fn unwrap_claude_paste(text: &str) -> &str {
    let trimmed = text.trim();
    let Some(rest) = trimmed.strip_prefix("<pasted_content id=\"") else {
        return trimmed;
    };
    let Some((id, body)) = rest.split_once("\">") else {
        return trimmed;
    };
    body.strip_suffix(&format!("</pasted_content id=\"{id}\">"))
        .map(str::trim)
        .unwrap_or(trimmed)
}

/// How long a Claude transcript must be unchanged before an idle report
/// ends a turn that has no reply, since Claude writes its final reply and
/// goes idle at about the same time.
const CLAUDE_IDLE_SETTLE: std::time::Duration = std::time::Duration::from_secs(5);

/// Whether the transcript has been unchanged for the settle period.
pub(crate) fn claude_transcript_settled(path: &Path) -> bool {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|elapsed| elapsed >= CLAUDE_IDLE_SETTLE)
}

/// Tracks the newest Claude turn: input starts a turn, input that arrives
/// while it runs joins it, and an end_turn reply or an interruption ends it.
#[derive(Default)]
struct ClaudeTurnTracker {
    turn: Option<AgentObservedTurn>,
}

impl ClaudeTurnTracker {
    fn user(&mut self, cursor: u64, record: &Value) {
        if record.get("isMeta").and_then(Value::as_bool) == Some(true) {
            return;
        }
        let Some(text) = claude_user_text(record) else {
            return;
        };
        let running = self
            .turn
            .as_ref()
            .is_some_and(|turn| matches!(turn.outcome, AgentObservedTurnOutcome::Running));
        if text
            .trim_start()
            .starts_with("[Request interrupted by user")
        {
            if let Some(turn) = self.turn.as_mut().filter(|_| running) {
                turn.outcome = AgentObservedTurnOutcome::Aborted;
                turn.completed_at = parse_record_timestamp(record);
                turn.latest_cursor = Some(cursor);
            }
            return;
        }
        if let Some(turn) = self.turn.as_mut().filter(|_| running) {
            turn.user_message_count += 1;
            turn.latest_cursor = Some(cursor);
            return;
        }
        let Some(turn_id) = record.get("uuid").and_then(Value::as_str) else {
            self.turn = None;
            return;
        };
        self.turn = Some(AgentObservedTurn {
            provider_turn_id: turn_id.to_string(),
            outcome: AgentObservedTurnOutcome::Running,
            started_at: parse_record_timestamp(record),
            completed_at: None,
            started_cursor: Some(cursor),
            latest_cursor: Some(cursor),
            primary_user_message_sha256: Some(message_sha256(unwrap_claude_paste(&text))),
            user_message_count: 1,
            final_message: None,
        });
    }

    fn assistant(&mut self, cursor: u64, record: &Value, text: Option<String>) {
        let Some(turn) = self
            .turn
            .as_mut()
            .filter(|turn| matches!(turn.outcome, AgentObservedTurnOutcome::Running))
        else {
            return;
        };
        turn.latest_cursor = Some(cursor);
        let end_turn = record
            .get("message")
            .and_then(|message| message.get("stop_reason"))
            .and_then(Value::as_str)
            == Some("end_turn");
        if end_turn && text.is_some() {
            turn.outcome = AgentObservedTurnOutcome::Completed;
            turn.completed_at = parse_record_timestamp(record);
            turn.final_message = text;
        }
    }
}

fn read_last_claude_observation(path: &Path) -> anyhow::Result<HarnessObservationDetails> {
    let mut reader = BufReader::new(fs::File::open(path)?);
    let mut summary = None;
    let mut harness_mode = None;
    let mut last_user_at = None;
    let mut last_assistant_at = None;
    let mut last_queued_input_at = None;
    let mut turns = ClaudeTurnTracker::default();
    let mut offset = 0u64;
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            break;
        }
        let cursor = offset;
        offset += read as u64;
        let Ok(record) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match record.get("type").and_then(Value::as_str) {
            Some("user") => {
                last_user_at = parse_record_timestamp(&record).or(last_user_at);
                turns.user(cursor, &record);
            }
            Some("assistant") => {}
            // Input that arrives during a running turn is queued into it.
            // Background task notifications are queued the same way but are
            // not input.
            Some("queue-operation") => {
                if record.get("operation").and_then(Value::as_str) == Some("enqueue")
                    && !record
                        .get("content")
                        .and_then(Value::as_str)
                        .is_some_and(|content| content.starts_with("<task-notification>"))
                {
                    last_queued_input_at = parse_record_timestamp(&record).or(last_queued_input_at);
                }
                continue;
            }
            _ => continue,
        }
        if record.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(content) = record
            .get("message")
            .and_then(|message| message.get("content"))
            .and_then(Value::as_array)
        else {
            last_assistant_at = parse_record_timestamp(&record).or(last_assistant_at);
            continue;
        };
        // A tool call keeps the turn running until Claude replies to its
        // result, except for tools that wait on the user's answer.
        if content.iter().any(|block| {
            block.get("type").and_then(Value::as_str) == Some("tool_use")
                && !matches!(
                    block.get("name").and_then(Value::as_str),
                    Some("ExitPlanMode" | "AskUserQuestion")
                )
        }) {
            last_user_at = parse_record_timestamp(&record).or(last_user_at);
        } else {
            last_assistant_at = parse_record_timestamp(&record).or(last_assistant_at);
        }

        let mut parts = vec![];
        let mut reply = vec![];
        for block in content {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                        let text = text.trim();
                        if !text.is_empty() {
                            parts.push(text.to_string());
                            reply.push(text.to_string());
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
        turns.assistant(
            cursor,
            &record,
            (!reply.is_empty()).then(|| reply.join("\n")),
        );
        if !parts.is_empty() {
            summary = Some(truncate_summary(&parts.join("\n")));
        }
    }
    let (turn_state, last_turn_completed_at) = derive_turn_state(last_user_at, last_assistant_at);
    Ok(HarnessObservationDetails {
        progress_summary: summary,
        harness_mode,
        turn_phase: None,
        updated_at: last_queued_input_at,
        turn_state,
        last_turn_completed_at,
        observed_turn: turns.turn,
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

/// Recover only an already-correlated turn, never infer a binding from history.
/// The finished Claude turn `turn_id` that started after `baseline_cursor`,
/// for a request whose turn a newer turn has since replaced.
pub(crate) fn read_claude_terminal_turn(
    path: &Path,
    turn_id: &str,
    baseline_cursor: u64,
) -> anyhow::Result<Option<AgentObservedTurn>> {
    let mut reader = BufReader::new(fs::File::open(path)?);
    let mut turns = ClaudeTurnTracker::default();
    let mut found = None;
    let mut offset = 0u64;
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            break;
        }
        let cursor = offset;
        offset += read as u64;
        let Ok(record) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match record.get("type").and_then(Value::as_str) {
            Some("user") => turns.user(cursor, &record),
            Some("assistant") => {
                let text = record
                    .get("message")
                    .and_then(|message| message.get("content"))
                    .and_then(Value::as_array)
                    .map(|blocks| {
                        blocks
                            .iter()
                            .filter(|block| {
                                block.get("type").and_then(Value::as_str) == Some("text")
                            })
                            .filter_map(|block| block.get("text").and_then(Value::as_str))
                            .map(str::trim)
                            .filter(|text| !text.is_empty())
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .filter(|text| !text.is_empty());
                turns.assistant(cursor, &record, text);
            }
            _ => continue,
        }
        if let Some(turn) = turns.turn.as_ref().filter(|turn| {
            turn.provider_turn_id == turn_id
                && turn
                    .started_cursor
                    .is_some_and(|cursor| cursor > baseline_cursor)
                && !matches!(turn.outcome, AgentObservedTurnOutcome::Running)
        }) {
            found = Some(turn.clone());
        }
    }
    Ok(found)
}

pub(crate) fn read_codex_terminal_turn(
    path: &Path,
    turn_id: &str,
    baseline_cursor: u64,
) -> anyhow::Result<Option<AgentObservedTurn>> {
    let mut terminal = None;
    visit_lines_reverse(path, |line| {
        let record: Value = serde_json::from_str(line)?;
        let Some(cursor) = codex_record_cursor(&record) else {
            return Ok(false);
        };
        if cursor <= baseline_cursor {
            return Ok(true);
        }
        if record.get("type").and_then(Value::as_str) != Some("event_msg")
            || codex_record_turn_id(&record) != Some(turn_id)
        {
            return Ok(false);
        }
        let payload = &record["payload"];
        let outcome = match payload.get("type").and_then(Value::as_str) {
            Some("task_complete") => AgentObservedTurnOutcome::Completed,
            Some("turn_aborted") => AgentObservedTurnOutcome::Aborted,
            // A start without a terminal boundary is genuinely indeterminate.
            Some("task_started") => return Ok(true),
            _ => return Ok(false),
        };
        terminal = Some(AgentObservedTurn {
            provider_turn_id: turn_id.to_string(),
            outcome,
            started_at: None,
            completed_at: parse_record_timestamp(&record),
            started_cursor: None,
            latest_cursor: Some(cursor),
            primary_user_message_sha256: None,
            user_message_count: 0,
            final_message: payload
                .get("last_agent_message")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string),
        });
        Ok(true)
    })?;
    Ok(terminal)
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

fn codex_response_message_is_synthetic_context(payload: &Value) -> bool {
    payload
        .get("content")
        .and_then(Value::as_array)
        .is_some_and(|content| {
            content.iter().any(|block| {
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .is_some_and(|text| {
                        text.starts_with("<environment_context>")
                            && text.ends_with("</environment_context>")
                    })
            })
        })
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

#[cfg(test)]
pub(crate) fn codex_complete_tail_offset(path: &Path) -> anyhow::Result<u64> {
    let mut file = fs::File::open(path)?;
    codex_complete_tail_offset_from_file(&mut file)
}

pub(crate) fn codex_complete_tail_offset_from_file(file: &mut fs::File) -> anyhow::Result<u64> {
    const CHUNK_SIZE: usize = 64 * 1024;

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

#[cfg(test)]
pub(crate) fn read_codex_output_messages(
    path: &Path,
    offset: u64,
    limit: usize,
) -> anyhow::Result<(Vec<CodexOutputMessage>, u64, bool)> {
    let mut file = fs::File::open(path)?;
    read_codex_output_messages_from_file(&mut file, offset, limit)
}

pub(crate) fn read_codex_output_messages_from_file(
    file: &mut fs::File,
    offset: u64,
    limit: usize,
) -> anyhow::Result<(Vec<CodexOutputMessage>, u64, bool)> {
    let complete_tail = codex_complete_tail_offset_from_file(file)?;
    anyhow::ensure!(
        offset <= complete_tail,
        "cursor offset {offset} is past the complete Codex session tail {complete_tail}"
    );

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
    let mut previous_turn_completed_at = None;
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
                        if codex_response_message_is_synthetic_context(payload) {
                            return Ok(false);
                        }
                        if last_user_at.is_none() {
                            last_user_at = parse_record_timestamp(&record);
                        }
                        if belongs_to_current_turn {
                            if let Some(message) = codex_response_message_text(payload) {
                                if current_turn_primary_user_sha256.is_none() {
                                    current_turn_primary_user_sha256 =
                                        Some(message_sha256(&message));
                                }
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
                            previous_turn_completed_at = parse_record_timestamp(&record);
                            return Ok(false);
                        }
                        if current_turn_id.is_none()
                            && saw_task_started
                            && matches!(last_lifecycle, Some(CodexTaskLifecycle::Running))
                        {
                            previous_turn_completed_at = parse_record_timestamp(&record);
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
                            previous_turn_completed_at = parse_record_timestamp(&record);
                            return Ok(false);
                        }
                        if current_turn_id.is_none()
                            && saw_task_started
                            && matches!(last_lifecycle, Some(CodexTaskLifecycle::Running))
                        {
                            previous_turn_completed_at = parse_record_timestamp(&record);
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
            Some(CodexTaskLifecycle::Running) => previous_turn_completed_at.is_some(),
            Some(CodexTaskLifecycle::Completed) | Some(CodexTaskLifecycle::Aborted) => {
                last_lifecycle_at.is_some()
            }
            None => false,
        };
        Ok(if current_turn_id.is_some() {
            saw_current_turn_start && turn_settled
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
            last_turn_completed_at = previous_turn_completed_at;
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

/// Harnesses record their working directory without a trailing slash, and
/// observers compare it exactly.
fn normalize_declared_cwd(cwd: &str) -> String {
    let mut cwd = cwd.trim().to_string();
    if cwd.starts_with("file://") {
        if let Ok(path) = Url::parse(&cwd).map(|url| url.to_file_path()) {
            if let Ok(path) = path {
                cwd = path.to_string_lossy().to_string();
            }
        }
    }
    while cwd.len() > 1 && cwd.ends_with('/') {
        cwd.pop();
    }
    cwd
}

#[cfg(test)]
mod test {
    use super::*;
    use chrono::{Datelike, TimeZone};
    use std::collections::HashMap;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use tempfile::TempDir;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn claude_input_queued_into_a_running_turn_counts_as_progress() {
        let temp = TempDir::new().unwrap();
        let session = temp.path().join("session.jsonl");
        let queued = |second: u32, content: &str| {
            serde_json::json!({"type":"queue-operation","operation":"enqueue",
                "timestamp": format!("2026-10-04T18:07:{second:02}.000Z"),
                "sessionId":"s","content": content})
            .to_string()
        };
        let mut lines = vec![
            serde_json::json!({"type":"user","timestamp":"2026-10-04T18:07:00.000Z",
                "message":{"role":"user","content":"work"}})
            .to_string(),
            serde_json::json!({"type":"assistant","timestamp":"2026-10-04T18:07:01.000Z",
                "message":{"role":"assistant","stop_reason":"tool_use",
                "content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{}}]}})
            .to_string(),
            queued(
                2,
                "<task-notification>\n<task-id>b1</task-id>\n</task-notification>",
            ),
        ];
        let write = |lines: &[String]| fs::write(&session, lines.join("\n") + "\n").unwrap();

        // A background task notification is not input.
        write(&lines);
        assert_eq!(
            read_last_claude_observation(&session).unwrap().updated_at,
            None
        );

        lines.push(queued(5, "concord agent tried to send a message"));
        write(&lines);
        let observed = read_last_claude_observation(&session).unwrap();
        assert_eq!(
            observed.updated_at,
            Some(Utc.with_ymd_and_hms(2026, 10, 4, 18, 7, 5).unwrap())
        );
        // Queued input does not end the running turn.
        assert_eq!(observed.turn_state, AgentTurnState::WaitingOnAgent);
    }

    #[test]
    fn claude_project_dir_name_matches_claude_code() {
        assert_eq!(
            claude_project_dir_name("/code/llama.cpp"),
            "-code-llama-cpp"
        );
        assert_eq!(
            claude_project_dir_name("/code/agentic_ethereum_2025"),
            "-code-agentic-ethereum-2025"
        );
        assert_eq!(
            claude_project_dir_name("/code/hyperliquid-participant-feed-re"),
            "-code-hyperliquid-participant-feed-re"
        );
        assert_eq!(
            claude_project_dir_name("/home/mihai/my notes"),
            "-home-mihai-my-notes"
        );
    }

    #[test]
    fn claude_turn_stays_open_while_a_tool_runs() {
        let temp = TempDir::new().unwrap();
        let session = temp.path().join("session.jsonl");
        let record = |kind: &str, second: u32, content: serde_json::Value, stop: Option<&str>| {
            serde_json::json!({"type": kind, "timestamp": format!("2026-10-04T16:17:{second:02}.000Z"),
                "message": {"role": kind, "content": content, "stop_reason": stop}})
            .to_string()
        };
        let tool = |name: &str| {
            serde_json::json!([{"type":"text","text":"checking"},
                {"type":"tool_use","id":"toolu_1","name":name,"input":{}}])
        };
        let write = |lines: &[String]| fs::write(&session, lines.join("\n") + "\n").unwrap();
        let mut lines = vec![
            record("user", 0, serde_json::json!("run the tests"), None),
            record("assistant", 5, tool("Bash"), Some("tool_use")),
        ];

        // Claude is running the tool, so the turn is still open.
        write(&lines);
        let running = read_last_claude_observation(&session).unwrap();
        assert_eq!(running.turn_state, AgentTurnState::WaitingOnAgent);
        assert_eq!(running.last_turn_completed_at, None);

        lines.push(record(
            "user",
            9,
            serde_json::json!([{"type":"tool_result","tool_use_id":"toolu_1","content":"ok"}]),
            None,
        ));
        lines.push(record(
            "assistant",
            12,
            serde_json::json!([{"type":"text","text":"tests pass"}]),
            Some("end_turn"),
        ));
        write(&lines);
        let finished = read_last_claude_observation(&session).unwrap();
        assert_eq!(finished.turn_state, AgentTurnState::WaitingOnUser);
        assert_eq!(
            finished.last_turn_completed_at,
            Some(Utc.with_ymd_and_hms(2026, 10, 4, 16, 17, 12).unwrap())
        );

        // Tools that ask the user something leave the turn waiting on them.
        for name in ["ExitPlanMode", "AskUserQuestion"] {
            write(&[
                record("user", 0, serde_json::json!("plan it"), None),
                record("assistant", 5, tool(name), Some("tool_use")),
            ]);
            assert_eq!(
                read_last_claude_observation(&session).unwrap().turn_state,
                AgentTurnState::WaitingOnUser,
                "{name}"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn claude_return_request_follows_the_turn_its_prompt_started() {
        use crate::agent_request::{AgentRequest, AgentRequestState};
        use std::os::unix::process::CommandExt;

        let _env_lock = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let temp = TempDir::new().unwrap();
        let cwd = "/tmp/claude-return";
        let projects = temp.path().join("projects");
        let project = projects.join("-tmp-claude-return");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(temp.path().join("sessions")).unwrap();
        struct Child(std::process::Child);
        impl Drop for Child {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let child = Child(
            std::process::Command::new("sleep")
                .arg0("claude")
                .arg("60")
                .spawn()
                .unwrap(),
        );
        let process = LocalProcessInfo::with_root_pid(child.0.id()).unwrap();
        let sid = "4c0a7e4e-8a6b-4a39-9d55-0f7c2f7ad001";
        let session = project.join(format!("{sid}.jsonl"));
        let registry = temp
            .path()
            .join("sessions")
            .join(format!("{}.json", process.pid));
        let namespace = fs::read_link(format!("/proc/{}/ns/pid", process.pid)).unwrap();
        let machine_id = fs::read_to_string("/etc/machine-id").unwrap();
        let set_status = |status: &str, changed: DateTime<Utc>| {
            fs::write(
                &registry,
                serde_json::json!({"pid": process.pid, "procStart": process.start_time.to_string(),
                    "pidDomain": format!("linux:{}:{}", machine_id.trim(), namespace.to_string_lossy()),
                    "sessionId": sid, "cwd": cwd, "kind": "interactive",
                    "status": status, "statusUpdatedAt": changed.timestamp_millis()})
                .to_string(),
            )
            .unwrap();
        };
        let at = |second: i64| {
            Utc::now() - chrono::Duration::minutes(10) + chrono::Duration::seconds(second)
        };
        let user = |uuid: &str, second: i64, content: &str| {
            serde_json::json!({"type":"user","uuid":uuid,"sessionId":sid,"cwd":cwd,
                "timestamp": at(second),"message":{"role":"user","content":content}})
            .to_string()
        };
        let reply = |uuid: &str, second: i64, text: &str| {
            serde_json::json!({"type":"assistant","uuid":uuid,"sessionId":sid,"cwd":cwd,
                "timestamp": at(second),"message":{"role":"assistant","stop_reason":"end_turn",
                "content":[{"type":"text","text":text}]}})
            .to_string()
        };
        let append = |lines: &[String]| {
            let mut file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&session)
                .unwrap();
            for line in lines {
                writeln!(file, "{line}").unwrap();
            }
        };
        let age_transcript = || {
            fs::File::options()
                .write(true)
                .open(&session)
                .unwrap()
                .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(60))
                .unwrap();
        };
        append(&[user("turn-0", 0, "hello"), reply("reply-0", 1, "hi")]);
        set_status("idle", at(1));
        set_env_path("WAKTERM_AGENT_CLAUDE_DIR", &projects);
        let metadata = AgentMetadata {
            agent_id: "claude-return".to_string(),
            name: "claude".to_string(),
            launch_cmd: "claude".to_string(),
            declared_cwd: cwd.to_string(),
            adopted_pid: Some(process.pid),
            adopted_start_time: Some(process.start_time),
            created_at: at(0),
            repo_root: None,
            worktree: None,
            branch: None,
            managed_checkout: false,
            launch_supervisor: None,
            codex_app_server: None,
        };
        let refreshed = |runtime: &mut AgentRuntimeSnapshot| {
            runtime.foreground_process_name = Some("claude".to_string());
            refresh_runtime_from_harness(runtime, &metadata);
        };
        let mut runtime = AgentRuntimeSnapshot::new(&metadata);
        refreshed(&mut runtime);
        let prompt = "[Panetone cross-agent message]\nsummarize the logs";
        let request = |runtime: &AgentRuntimeSnapshot, id: &str| {
            let mut request =
                AgentRequest::new(id.to_string(), &metadata, 1, runtime, prompt, true, 0, None)
                    .unwrap();
            request.mark_submitted();
            request
        };

        // The prompt, stored as a paste, starts a turn; a steer joins it; the
        // turn's reply completes the request.
        let mut completed = request(&runtime, "completes");
        append(&[
            user(
                "turn-1",
                10,
                &format!(
                    "\n\n<pasted_content id=\"0551\">\n{prompt}\n</pasted_content id=\"0551\">\n"
                ),
            ),
            user("steer-1", 12, "also check stderr"),
        ]);
        set_status("busy", at(10));
        refreshed(&mut runtime);
        completed.reconcile(Some(&metadata), Some(&runtime), Utc::now());
        assert_eq!(completed.state, AgentRequestState::Bound);
        append(&[reply("reply-1", 20, "the logs show two timeouts")]);
        set_status("idle", at(20));
        refreshed(&mut runtime);
        completed.reconcile(Some(&metadata), Some(&runtime), Utc::now());
        assert_eq!(completed.state, AgentRequestState::Completed);
        assert_eq!(
            completed.final_message.as_deref(),
            Some("the logs show two timeouts")
        );

        // A different input that starts the next turn first cannot be bound.
        let mut raced = request(&runtime, "raced");
        append(&[user(
            "turn-2",
            30,
            "<task-notification>done</task-notification>",
        )]);
        refreshed(&mut runtime);
        raced.reconcile(Some(&metadata), Some(&runtime), Utc::now());
        assert_eq!(raced.state, AgentRequestState::Indeterminate);
        append(&[reply("reply-2", 31, "noted")]);
        refreshed(&mut runtime);

        // A prompt Claude records but never answers ends once Claude is idle
        // and the transcript is quiet.
        let mut unanswered = request(&runtime, "unanswered");
        append(&[user("turn-3", 40, prompt)]);
        set_status("idle", at(41));
        refreshed(&mut runtime);
        unanswered.reconcile(Some(&metadata), Some(&runtime), Utc::now());
        assert_eq!(unanswered.state, AgentRequestState::Bound);
        age_transcript();
        refreshed(&mut runtime);
        unanswered.reconcile(Some(&metadata), Some(&runtime), Utc::now());
        remove_env_var("WAKTERM_AGENT_CLAUDE_DIR");
        assert_eq!(unanswered.state, AgentRequestState::Aborted);
    }

    #[test]
    fn declared_cwd_normalization_drops_trailing_slashes() {
        assert_eq!(
            normalize_declared_cwd("/code/inquisition/"),
            "/code/inquisition"
        );
        assert_eq!(
            normalize_declared_cwd("/code/inquisition//"),
            "/code/inquisition"
        );
        assert_eq!(
            normalize_declared_cwd("/code/inquisition"),
            "/code/inquisition"
        );
        assert_eq!(normalize_declared_cwd("/"), "/");
        assert_eq!(
            normalize_declared_cwd("file:///code/inquisition/"),
            "/code/inquisition"
        );
        assert_eq!(normalize_declared_cwd(""), "");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn claude_registry_binds_a_session_declared_with_a_trailing_slash() {
        use std::os::unix::process::CommandExt;

        let _env_lock = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let temp = TempDir::new().unwrap();
        let cwd = "/tmp/claude.slash";
        let projects = temp.path().join("projects");
        // Claude Code's own directory name for this working directory.
        let project = projects.join("-tmp-claude-slash");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(temp.path().join("sessions")).unwrap();
        struct Child(std::process::Child);
        impl Drop for Child {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let child = Child(
            std::process::Command::new("sleep")
                .arg0("claude")
                .arg("60")
                .spawn()
                .unwrap(),
        );
        let process = LocalProcessInfo::with_root_pid(child.0.id()).unwrap();
        let sid = "30e773ff-f0a3-4ae7-9502-594f32d21c0b";
        let session = project.join(format!("{sid}.jsonl"));
        let now = Utc::now();
        fs::write(
            &session,
            format!(
                "{}\n{}\n",
                serde_json::json!({"type":"user","uuid":"turn","sessionId":sid,"cwd":cwd,
                    "timestamp":now - chrono::Duration::seconds(2),
                    "message":{"role":"user","content":"work"}}),
                serde_json::json!({"type":"assistant","uuid":"turn-assistant","sessionId":sid,"cwd":cwd,
                    "timestamp":now - chrono::Duration::seconds(1),"parentUuid":"turn",
                    "message":{"id":"msg-turn","role":"assistant","model":"claude","stop_reason":"end_turn",
                    "content":[{"type":"text","text":"done"}]}}),
            ),
        )
        .unwrap();
        let namespace = fs::read_link(format!("/proc/{}/ns/pid", process.pid)).unwrap();
        let machine_id = fs::read_to_string("/etc/machine-id").unwrap();
        // Claude records its working directory without a trailing slash.
        fs::write(
            temp.path()
                .join("sessions")
                .join(format!("{}.json", process.pid)),
            serde_json::json!({"pid": process.pid, "procStart": process.start_time.to_string(),
                "pidDomain": format!("linux:{}:{}", machine_id.trim(), namespace.to_string_lossy()),
                "sessionId": sid, "cwd": cwd, "kind": "interactive"})
            .to_string(),
        )
        .unwrap();
        set_env_path("WAKTERM_AGENT_CLAUDE_DIR", &projects);

        let metadata = AgentMetadata {
            agent_id: "inquisition-claude3".to_string(),
            name: "inquisition_claude3".to_string(),
            launch_cmd: format!("claude --dangerously-skip-permissions --resume {sid}"),
            declared_cwd: format!("{cwd}/"),
            adopted_pid: Some(process.pid),
            adopted_start_time: Some(process.start_time),
            created_at: now - chrono::Duration::minutes(1),
            repo_root: None,
            worktree: None,
            branch: None,
            managed_checkout: false,
            launch_supervisor: None,
            codex_app_server: None,
        };
        // Claude writes its session record only after startup prompts such
        // as folder trust are answered.
        let registry = temp
            .path()
            .join("sessions")
            .join(format!("{}.json", process.pid));
        let parked = temp.path().join("registry-before-startup.json");
        fs::rename(&registry, &parked).unwrap();
        assert!(claude_session_record_missing(&metadata));
        fs::rename(&parked, &registry).unwrap();
        assert!(!claude_session_record_missing(&metadata));
        let mut codex = metadata.clone();
        codex.launch_cmd = "codex".to_string();
        assert!(!claude_session_record_missing(&codex));

        let mut runtime = AgentRuntimeSnapshot::new(&metadata);
        prime_runtime_for_new_agent(&mut runtime, &metadata, Some("claude"));
        runtime.foreground_process_name = Some("claude".to_string());
        refresh_runtime_from_harness(&mut runtime, &metadata);
        remove_env_var("WAKTERM_AGENT_CLAUDE_DIR");

        assert_eq!(runtime.session_path.as_deref(), session.to_str());
        assert_eq!(runtime.transport, AgentTransport::ObservedPty);
        assert_eq!(runtime.turn_state, AgentTurnState::WaitingOnUser);

        // Input Claude showed without starting a turn leaves the transcript
        // ending in a user record. Claude's own status decides the state.
        let mut file = fs::OpenOptions::new().append(true).open(&session).unwrap();
        writeln!(
            file,
            "{}",
            serde_json::json!({"type":"user","sessionId":sid,"cwd":cwd,"timestamp":now,
                "message":{"role":"user","content":"<task-notification>done</task-notification>"}})
        )
        .unwrap();
        drop(file);
        let registry_path = temp
            .path()
            .join("sessions")
            .join(format!("{}.json", process.pid));
        let set_status = |status: &str| {
            let mut record: serde_json::Value =
                serde_json::from_slice(&fs::read(&registry_path).unwrap()).unwrap();
            record["status"] = serde_json::json!(status);
            fs::write(&registry_path, record.to_string()).unwrap();
        };
        set_env_path("WAKTERM_AGENT_CLAUDE_DIR", &projects);
        set_status("idle");
        // Typing a draft after the last turn does not make an idle Claude busy.
        runtime.last_input_at = Some(Utc::now());
        refresh_runtime_from_harness(&mut runtime, &metadata);
        assert_eq!(runtime.turn_state, AgentTurnState::WaitingOnUser);
        assert_eq!(runtime.status, AgentStatus::Idle);
        // Starting work counts as progress, which acknowledges a send.
        let started_at = Utc
            .timestamp_millis_opt(Utc::now().timestamp_millis() + 1000)
            .unwrap();
        let mut record: serde_json::Value =
            serde_json::from_slice(&fs::read(&registry_path).unwrap()).unwrap();
        record["status"] = serde_json::json!("busy");
        record["statusUpdatedAt"] = serde_json::json!(started_at.timestamp_millis());
        fs::write(&registry_path, record.to_string()).unwrap();
        refresh_runtime_from_harness(&mut runtime, &metadata);
        assert_eq!(runtime.turn_state, AgentTurnState::WaitingOnAgent);
        assert_eq!(runtime.status, AgentStatus::Busy);
        assert_eq!(runtime.last_progress_at, Some(started_at));
        assert_eq!(input_blocked_reason(&runtime), None);

        // Going idle after Wakterm saw the turn running ends that turn, even
        // though the transcript has no reply for it.
        let transcript_end = runtime.last_turn_completed_at;
        let idle_at = started_at + chrono::Duration::seconds(3);
        let mut record: serde_json::Value =
            serde_json::from_slice(&fs::read(&registry_path).unwrap()).unwrap();
        record["status"] = serde_json::json!("idle");
        record["statusUpdatedAt"] = serde_json::json!(idle_at.timestamp_millis());
        fs::write(&registry_path, record.to_string()).unwrap();
        refresh_runtime_from_harness(&mut runtime, &metadata);
        assert!(transcript_end < Some(idle_at));
        assert_eq!(runtime.last_turn_completed_at, Some(idle_at));
        assert_eq!(runtime.turn_state, AgentTurnState::WaitingOnUser);

        // Admission confirms a Claude prompt only by a status change after
        // the write.
        let before = std::time::SystemTime::from(idle_at - chrono::Duration::seconds(1));
        let after = std::time::SystemTime::from(idle_at + chrono::Duration::seconds(1));
        assert_eq!(claude_status_changed_since(&metadata, before), Some(true));
        assert_eq!(claude_status_changed_since(&metadata, after), Some(false));
        let mut codex = metadata.clone();
        codex.launch_cmd = "codex".to_string();
        assert_eq!(claude_status_changed_since(&codex, before), None);

        // A dialog with the keyboard needs the user, and typed input would
        // go to the dialog.
        let mut record: serde_json::Value =
            serde_json::from_slice(&fs::read(&registry_path).unwrap()).unwrap();
        record["status"] = serde_json::json!("waiting");
        record["waitingFor"] = serde_json::json!("dialog open");
        fs::write(&registry_path, record.to_string()).unwrap();
        refresh_runtime_from_harness(&mut runtime, &metadata);
        assert_eq!(runtime.turn_state, AgentTurnState::WaitingOnUser);
        assert_eq!(
            runtime.attention_reason.as_deref(),
            Some("approval-requested")
        );
        assert_eq!(
            input_blocked_reason(&runtime).as_deref(),
            Some("the target is waiting for dialog open")
        );

        set_status("shell");
        refresh_runtime_from_harness(&mut runtime, &metadata);
        remove_env_var("WAKTERM_AGENT_CLAUDE_DIR");
        assert_eq!(runtime.turn_state, AgentTurnState::WaitingOnUser);
        assert_eq!(runtime.status, AgentStatus::Idle);
        assert_eq!(
            input_blocked_reason(&runtime).as_deref(),
            Some("the target is in shell mode")
        );
    }

    #[test]
    fn registered_origin_tracks_transport_ownership() {
        assert_eq!(
            AgentOrigin::for_registered_transport(&AgentTransport::PlainPty),
            AgentOrigin::Adopted
        );
        assert_eq!(
            AgentOrigin::for_registered_transport(&AgentTransport::ObservedPty),
            AgentOrigin::Adopted
        );
        assert_eq!(
            AgentOrigin::for_registered_transport(&AgentTransport::CodexAppServerTui),
            AgentOrigin::Managed
        );
        assert!(AgentOrigin::Adopted.is_registered());
        assert!(AgentOrigin::Managed.is_registered());
        assert!(!AgentOrigin::Detected.is_registered());
        assert_eq!(
            serde_json::to_string(&AgentOrigin::Managed).unwrap(),
            r#""managed""#
        );
    }

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
            #[cfg(unix)]
            process_group: 1,
            #[cfg(unix)]
            controlling_tty: Some(1),
            name: name.to_string(),
            executable: PathBuf::from(executable),
            argv: argv.iter().map(|arg| (*arg).to_string()).collect(),
            cwd: PathBuf::from("/tmp"),
            status: procinfo::LocalProcessStatus::Run,
            start_time,
            #[cfg(windows)]
            console: 1,
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
        assert_eq!(infer_harness("agy", None), AgentHarness::Agy);
        assert_eq!(
            infer_harness("agy --dangerously-skip-permissions", None),
            AgentHarness::Agy
        );
        assert_eq!(infer_harness("strategy", None), AgentHarness::Unknown);
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

    fn write_agy_transcript(root: &Path, conversation_id: &str, records: &[Value]) -> PathBuf {
        let transcript = root
            .join("brain")
            .join(conversation_id)
            .join(".system_generated")
            .join("logs")
            .join("transcript.jsonl");
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        let body = records
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&transcript, format!("{body}\n")).unwrap();
        transcript
    }

    #[test]
    fn observes_agy_running_and_completed_turns_from_transcript() {
        let temp = TempDir::new().unwrap();
        let transcript = write_agy_transcript(
            temp.path(),
            "conversation-1",
            &[
                serde_json::json!({
                    "step_index": 10,
                    "type": "USER_INPUT",
                    "status": "DONE",
                    "source": "USER_EXPLICIT",
                    "created_at": "2026-08-22T16:00:00Z",
                    "content": "Fix the lifecycle"
                }),
                serde_json::json!({
                    "step_index": 11,
                    "type": "PLANNER_RESPONSE",
                    "status": "DONE",
                    "source": "MODEL",
                    "created_at": "2026-08-22T16:00:01Z",
                    "tool_calls": [{"name": "ViewFile"}]
                }),
                serde_json::json!({
                    "step_index": 12,
                    "type": "GENERIC",
                    "status": "RUNNING",
                    "source": "MODEL",
                    "created_at": "2026-08-22T16:00:02Z",
                    "content": "tool output"
                }),
            ],
        );

        let running = read_last_agy_observation(&transcript).unwrap();
        assert_eq!(running.turn_state, AgentTurnState::WaitingOnAgent);
        assert_eq!(running.turn_phase.as_deref(), Some("working"));
        let turn = running.observed_turn.unwrap();
        assert_eq!(turn.provider_turn_id, "conversation-1:10");
        assert_eq!(turn.outcome, AgentObservedTurnOutcome::Running);
        assert_eq!(turn.started_cursor, Some(10));
        assert_eq!(turn.latest_cursor, Some(12));
        let expected_user_hash = message_sha256("Fix the lifecycle");
        assert_eq!(
            turn.primary_user_message_sha256.as_deref(),
            Some(expected_user_hash.as_str())
        );

        let mut transcript_file = std::fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .unwrap();
        writeln!(
            transcript_file,
            "{}",
            serde_json::json!({
                "step_index": 13,
                "type": "PLANNER_RESPONSE",
                "status": "DONE",
                "source": "MODEL",
                "created_at": "2026-08-22T16:00:03Z",
                "content": "Lifecycle fixed.",
                "tool_calls": []
            })
        )
        .unwrap();

        let completed = read_last_agy_observation(&transcript).unwrap();
        assert_eq!(completed.turn_state, AgentTurnState::WaitingOnUser);
        assert_eq!(completed.turn_phase.as_deref(), Some("final_answer"));
        assert_eq!(
            completed.progress_summary.as_deref(),
            Some("Lifecycle fixed.")
        );
        let turn = completed.observed_turn.unwrap();
        assert_eq!(turn.outcome, AgentObservedTurnOutcome::Completed);
        assert_eq!(turn.latest_cursor, Some(13));
        assert_eq!(turn.final_message.as_deref(), Some("Lifecycle fixed."));
        assert_eq!(
            turn.completed_at,
            Some(Utc.with_ymd_and_hms(2026, 8, 22, 16, 0, 3).unwrap())
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn observes_agy_session_owned_by_exact_process_incarnation() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let temp = TempDir::new().unwrap();
        let conversation_id = "b572d5a0-c9e0-4770-933a-083af5c453b4";
        let transcript = write_agy_transcript(
            temp.path(),
            conversation_id,
            &[
                serde_json::json!({
                    "step_index": 0,
                    "type": "USER_INPUT",
                    "status": "DONE",
                    "source": "USER_EXPLICIT",
                    "created_at": "2026-08-22T16:00:00Z",
                    "content": "Inspect this"
                }),
                serde_json::json!({
                    "step_index": 1,
                    "type": "PLANNER_RESPONSE",
                    "status": "DONE",
                    "source": "MODEL",
                    "created_at": "2026-08-22T16:00:01Z",
                    "content": "Done",
                    "tool_calls": []
                }),
            ],
        );
        let presence_dir = temp.path().join("presence");
        std::fs::create_dir_all(&presence_dir).unwrap();
        let _presence_lock =
            std::fs::File::create(presence_dir.join(format!("{conversation_id}.lock"))).unwrap();
        let _malformed_transcript = write_agy_transcript(temp.path(), "not-a-uuid", &[]);
        let _malformed_lock = std::fs::File::create(presence_dir.join("not-a-uuid.lock")).unwrap();
        let process = LocalProcessInfo::with_root_pid(std::process::id()).unwrap();

        let observed = agy_session_owned_by_process(
            temp.path(),
            Some(process.pid),
            Some(process.start_time),
            None,
            None,
        )
        .unwrap();
        assert_eq!(observed.as_deref(), Some(transcript.as_path()));
        assert!(agy_session_owned_by_process(
            temp.path(),
            Some(process.pid),
            Some(process.start_time + 1),
            None,
            None,
        )
        .unwrap()
        .is_none());

        let second_conversation_id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let _second_transcript = write_agy_transcript(temp.path(), second_conversation_id, &[]);
        let second_lock =
            std::fs::File::create(presence_dir.join(format!("{second_conversation_id}.lock")))
                .unwrap();
        assert!(agy_session_owned_by_process(
            temp.path(),
            Some(process.pid),
            Some(process.start_time),
            None,
            None,
        )
        .unwrap()
        .is_none());
        assert_eq!(
            agy_session_owned_by_process(
                temp.path(),
                Some(process.pid),
                Some(process.start_time),
                None,
                Some(conversation_id),
            )
            .unwrap()
            .as_deref(),
            Some(transcript.as_path())
        );
        assert!(agy_session_owned_by_process(
            temp.path(),
            Some(process.pid),
            Some(process.start_time),
            None,
            Some("cccccccc-dddd-4eee-8fff-000000000000"),
        )
        .unwrap()
        .is_none());
        assert_eq!(
            agy_session_owned_by_process(
                temp.path(),
                Some(process.pid),
                Some(process.start_time),
                transcript.to_str(),
                None,
            )
            .unwrap()
            .as_deref(),
            Some(transcript.as_path())
        );
        drop(second_lock);
        std::fs::remove_file(presence_dir.join(format!("{second_conversation_id}.lock"))).unwrap();

        set_env_path("WAKETERM_AGENT_AGY_DIR", temp.path());
        let metadata = AgentMetadata {
            agent_id: "agy-observer".to_string(),
            name: "agy-observer".to_string(),
            launch_cmd: "agy --dangerously-skip-permissions".to_string(),
            declared_cwd: "/code/wakterm".to_string(),
            adopted_pid: Some(process.pid),
            adopted_start_time: Some(process.start_time),
            created_at: Utc::now(),
            repo_root: None,
            worktree: None,
            branch: None,
            managed_checkout: false,
            launch_supervisor: None,
            codex_app_server: None,
        };
        let mut runtime = AgentRuntimeSnapshot::new(&metadata);
        runtime.foreground_process_name = Some("/home/test/.local/bin/agy".to_string());
        refresh_runtime_from_harness(&mut runtime, &metadata);
        remove_env_var("WAKETERM_AGENT_AGY_DIR");

        let transcript_string = transcript.to_string_lossy().to_string();
        assert_eq!(runtime.harness, AgentHarness::Agy);
        assert_eq!(runtime.transport, AgentTransport::ObservedPty);
        assert_eq!(
            runtime.session_path.as_deref(),
            Some(transcript_string.as_str())
        );
        assert_eq!(runtime.turn_state, AgentTurnState::WaitingOnUser);
        assert_eq!(runtime.progress_summary.as_deref(), Some("Done"));
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
    fn supervised_harness_binds_the_tui_instead_of_its_launcher_or_worker() {
        let process = proc_info(
            "supervisor",
            "/usr/bin/supervisor",
            &["supervisor", "--", "claude"],
            1,
            vec![proc_info(
                "claude",
                "/opt/claude",
                &["claude"],
                2,
                vec![proc_info(
                    "codex",
                    "/opt/codex",
                    &["codex", "exec"],
                    3,
                    vec![],
                )],
            )],
        );
        let matched = detect_harness_process(Some(&process), Some("/usr/bin/supervisor"))
            .expect("interactive harness behind supervisor");
        assert_eq!(matched.harness, AgentHarness::Claude);
        assert_eq!(matched.launch_cmd, "claude");
        assert_eq!(
            registered_harness_process(&AgentHarness::Claude, &process)
                .unwrap()
                .pid,
            2
        );
    }

    #[test]
    #[cfg(unix)]
    fn supervised_harness_requires_one_foreground_job_member() {
        let child = proc_info("claude", "/opt/claude", &["claude"], 2, vec![]);
        let mut root = proc_info(
            "supervisor",
            "/usr/bin/supervisor",
            &["supervisor", "claude"],
            1,
            vec![],
        );
        assert!(
            detect_harness_process(Some(&root), None).is_none(),
            "an argument is not a harness"
        );
        root.children.insert(2, child.clone());
        assert_eq!(
            detect_harness_process(Some(&root), None)
                .unwrap()
                .process
                .unwrap()
                .pid,
            2
        );
        root.children.get_mut(&2).unwrap().process_group = 9;
        assert!(
            detect_harness_process(Some(&root), None).is_none(),
            "background job"
        );
        root.children.insert(2, child.clone());
        root.children.get_mut(&2).unwrap().controlling_tty = Some(9);
        assert!(
            detect_harness_process(Some(&root), None).is_none(),
            "another terminal"
        );
        root.children.insert(2, child.clone());
        root.children
            .insert(3, proc_info("codex", "/opt/codex", &["codex"], 3, vec![]));
        assert!(
            detect_harness_process(Some(&root), None).is_none(),
            "ambiguous foreground harnesses"
        );
        assert!(registered_harness_process(&AgentHarness::Claude, &root).is_none());
        root.children.remove(&3);
        root.children.get_mut(&2).unwrap().status = procinfo::LocalProcessStatus::Zombie;
        assert!(
            detect_harness_process(Some(&root), None).is_none(),
            "exited child"
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    #[ignore = "requires WAKTERM_TEST_CLAUDE, Python 3 and bubblewrap"]
    fn real_sandboxed_claude_process_and_session_are_observed() {
        use std::process::{Command, Stdio};
        let _env_lock = ENV_LOCK.lock().unwrap();
        let temp = tempfile::Builder::new()
            .prefix("wakterm-sandbox-")
            .tempdir_in("/tmp")
            .unwrap();
        let binary = std::env::var_os("WAKTERM_TEST_CLAUDE").expect("WAKTERM_TEST_CLAUDE");
        let mut child = Command::new("python3")
            .arg("-c")
            .arg(include_str!("../test-data/supervised_claude_pty.py"))
            .arg(temp.path())
            .arg(binary)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut ready = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        let result = (|| -> anyhow::Result<()> {
            let ready: Value =
                serde_json::from_str(&ready).context("native Claude sandbox startup")?;
            let root =
                LocalProcessInfo::with_root_pid(ready["supervisor_pid"].as_u64().unwrap() as u32)
                    .context("sandbox supervisor")?;
            let matched = detect_harness_process(Some(&root), root.executable.to_str())
                .context("sandbox TUI selection")?;
            let process = matched.process.context("exact child")?;
            anyhow::ensure!(matched.harness == AgentHarness::Claude && process.pid != root.pid);
            anyhow::ensure!(
                process.process_group == root.process_group
                    && process.controlling_tty == root.controlling_tty
            );
            for ns in ["mnt", "pid", "ipc", "uts"] {
                anyhow::ensure!(
                    fs::read_link(format!("/proc/{}/ns/{ns}", process.pid))?
                        != fs::read_link(format!("/proc/self/ns/{ns}"))?
                );
            }
            for path in [
                "run/user/1000/wakterm/sock",
                "run/user/1000/panetone/control.sock",
                "home/mihai/.local/bin/wakterm",
                "home/mihai/.local/bin/panetone",
            ] {
                anyhow::ensure!(
                    !PathBuf::from(format!("/proc/{}/root/{path}", process.pid)).exists()
                );
            }
            set_env_path(
                "WAKTERM_AGENT_CLAUDE_DIR",
                Path::new(ready["projects"].as_str().unwrap()),
            );
            let metadata = AgentMetadata {
                agent_id: "offline-sandbox-test".into(),
                name: "sandbox-test".into(),
                launch_cmd: matched.launch_cmd,
                launch_supervisor: matched.launch_supervisor,
                declared_cwd: ready["cwd"].as_str().unwrap().into(),
                adopted_pid: Some(process.pid),
                adopted_start_time: Some(process.start_time),
                created_at: Utc::now(),
                repo_root: None,
                worktree: None,
                branch: None,
                managed_checkout: false,
                codex_app_server: None,
            };
            let mut runtime = AgentRuntimeSnapshot::new(&metadata);
            runtime.alive = true;
            runtime.foreground_process_name = observed_foreground_process_name(
                &metadata,
                Some(&root),
                root.executable.to_str().map(str::to_string),
            );
            refresh_runtime_from_harness(&mut runtime, &metadata);
            anyhow::ensure!(
                runtime.transport == AgentTransport::ObservedPty,
                "{:?}",
                runtime
            );
            anyhow::ensure!(runtime.turn_state == AgentTurnState::WaitingOnUser);
            anyhow::ensure!(
                claude_session_id(Path::new(runtime.session_path.as_deref().unwrap()))?.as_deref()
                    == ready["session_id"].as_str()
            );
            eprintln!("native Claude sandbox: host PID {}, supervisor {}, namespace/session identity verified; mount/PID/IPC/UTS isolated; host sockets absent", process.pid, root.pid);
            Ok(())
        })();
        remove_env_var("WAKTERM_AGENT_CLAUDE_DIR");
        drop(child.stdin.take());
        let status = child.wait().unwrap();
        result.unwrap();
        assert!(status.success());
    }

    #[test]
    fn falls_back_to_foreground_process_name_for_detection() {
        let matched = detect_harness_process(None, Some("/usr/bin/claude")).unwrap();
        assert_eq!(matched.harness, AgentHarness::Claude);
        assert_eq!(matched.launch_cmd, "claude");
    }

    #[test]
    fn codex_restore_recipe_uses_expanded_process_argv_and_removes_resume() {
        let process = proc_info(
            "zsh",
            "/usr/bin/zsh",
            &["zsh"],
            1,
            vec![proc_info(
                "codex",
                "/usr/local/bin/codex",
                &[
                    "/usr/local/bin/codex",
                    "-a",
                    "never",
                    "-s",
                    "danger-full-access",
                    "resume",
                    "00000000-0000-4000-8000-000000000001",
                ],
                2,
                vec![proc_info(
                    "codex-code-mod",
                    "/usr/local/bin/codex-code-mode-host",
                    &["/usr/local/bin/codex-code-mode-host"],
                    3,
                    vec![],
                )],
            )],
        );

        assert_eq!(
            native_restore_launch_command(&AgentHarness::Codex, &process).as_deref(),
            Some("/usr/local/bin/codex -a never -s danger-full-access")
        );
    }

    #[test]
    fn identifies_exact_remote_codex_tui_in_process_tree() {
        let thread_id = "01a02767-c120-77b2-88a1-4e17c93a7549";
        let process = proc_info(
            "zsh",
            "/usr/bin/zsh",
            &["zsh"],
            1,
            vec![proc_info(
                "codex",
                "/usr/local/bin/codex",
                &[
                    "/usr/local/bin/codex",
                    "resume",
                    "--remote",
                    "unix:///run/user/1000/wakterm/codex-app-server.sock",
                    "-C",
                    "/code/wakterm",
                    thread_id,
                    "--dangerously-bypass-approvals-and-sandbox",
                ],
                2,
                vec![],
            )],
        );

        assert_eq!(
            remote_codex_tui(&process),
            Some(RemoteCodexTui {
                pid: 2,
                start_time: 2,
                endpoint: "unix:///run/user/1000/wakterm/codex-app-server.sock".to_string(),
                thread_id: thread_id.to_string(),
                tui_args: vec!["--dangerously-bypass-approvals-and-sandbox".to_string()],
            })
        );
    }

    fn remote_codex_tui_argument_cases() -> Vec<(&'static str, Vec<&'static str>)> {
        vec![
            ("codex --remote unix:///tmp/codex.sock resume 01a02767-c120-77b2-88a1-4e17c93a7549", vec![]),
            ("codex -a never -s danger-full-access --remote unix:///tmp/codex.sock resume 01a02767-c120-77b2-88a1-4e17c93a7549", vec!["-a", "never", "-s", "danger-full-access"]),
            ("codex resume -a never --remote=unix:///tmp/codex.sock -s danger-full-access 01a02767-c120-77b2-88a1-4e17c93a7549 -C /code/zola", vec!["-a", "never", "-s", "danger-full-access"]),
            ("codex resume 01a02767-c120-77b2-88a1-4e17c93a7549 --remote unix:///tmp/codex.sock --cd=/code/zola --ask-for-approval=never --sandbox=danger-full-access", vec!["-a", "never", "-s", "danger-full-access"]),
            ("codex -anever --remote unix:///tmp/codex.sock resume -sdanger-full-access -C/code/zola 01a02767-c120-77b2-88a1-4e17c93a7549", vec!["-a", "never", "-s", "danger-full-access"]),
            ("codex -m resume --remote unix:///tmp/codex.sock resume -a never 01a02767-c120-77b2-88a1-4e17c93a7549", vec!["-m", "resume", "-a", "never"]),
            ("codex resume --remote unix:///tmp/codex.sock -C /code/aipocalypse-public 01a02767-c120-77b2-88a1-4e17c93a7549 -a never -s danger-full-access -m gpt-daybreak-blue-latest -c 'model_reasoning_effort=\"xhigh\"'", vec!["-a", "never", "-s", "danger-full-access", "-m", "gpt-daybreak-blue-latest", "-c", "model_reasoning_effort=\"xhigh\""]),
            ("codex --remote unix:///tmp/codex.sock -C 00000000-0000-4000-8000-000000000001 resume --no-alt-screen -- 01a02767-c120-77b2-88a1-4e17c93a7549", vec!["--no-alt-screen"]),
            ("codex -a on-request -s workspace-write --remote unix:///tmp/codex.sock resume --ask-for-approval never 01a02767-c120-77b2-88a1-4e17c93a7549 --sandbox danger-full-access", vec!["-a", "never", "-s", "danger-full-access"]),
            ("codex --yolo --remote unix:///tmp/codex.sock resume 01a02767-c120-77b2-88a1-4e17c93a7549 -s read-only -a on-request", vec!["-s", "read-only", "-a", "on-request"]),
            ("codex -a never --remote unix:///tmp/codex.sock resume --approve-for-me 01a02767-c120-77b2-88a1-4e17c93a7549", vec!["--approve-for-me"]),
            ("codex --add-dir '/tmp/root dir' --remote unix:///tmp/codex.sock resume 01a02767-c120-77b2-88a1-4e17c93a7549 --add-dir '/tmp/another dir' --no-alt-screen", vec!["--add-dir", "/tmp/root dir", "--add-dir", "/tmp/another dir", "--no-alt-screen"]),
            ("codex --remote unix:///tmp/codex.sock resume 01a02767-c120-77b2-88a1-4e17c93a7549 --image a.png b.png --no-alt-screen", vec!["-i", "a.png", "-i", "b.png", "--no-alt-screen"]),
        ]
    }

    #[test]
    fn remote_codex_tui_captures_settings_across_argument_positions() {
        for (command, expected) in remote_codex_tui_argument_cases() {
            let argv = shell_words::split(command).unwrap();
            let argv: Vec<_> = argv.iter().map(String::as_str).collect();
            let process = proc_info("codex", "/usr/bin/codex", &argv, 2, vec![]);
            let parsed =
                remote_codex_tui(&process).unwrap_or_else(|| panic!("rejected {}", command));
            assert_eq!(
                parsed.thread_id, "01a02767-c120-77b2-88a1-4e17c93a7549",
                "{}",
                command
            );
            assert_eq!(parsed.endpoint, "unix:///tmp/codex.sock", "{}", command);
            assert_eq!(parsed.tui_args, expected, "{}", command);
        }
    }

    #[test]
    fn remote_codex_tui_rejects_ambiguous_identity_and_unsupported_settings() {
        for command in [
            "codex --remote unix:///tmp/codex.sock -m resume 01a02767-c120-77b2-88a1-4e17c93a7549",
            "codex --remote unix:///tmp/codex.sock resume -m 01a02767-c120-77b2-88a1-4e17c93a7549",
            "codex --remote unix:///tmp/codex.sock resume named-session --add-dir 01a02767-c120-77b2-88a1-4e17c93a7549",
            "codex --remote unix:///tmp/codex.sock resume --last 01a02767-c120-77b2-88a1-4e17c93a7549",
            "codex --remote unix:///tmp/codex.sock resume 01a02767-c120-77b2-88a1-4e17c93a7549 'do some work'",
            "codex --remote unix:///tmp/codex.sock resume 01a02767-c120-77b2-88a1-4e17c93a7549 --unknown value",
            "codex --remote unix:///tmp/codex.sock resume 01a02767-c120-77b2-88a1-4e17c93a7549 -a",
            "codex --remote unix:///tmp/codex.sock resume 01a02767-c120-77b2-88a1-4e17c93a7549 -é",
            "codex --remote unix:///tmp/codex.sock resume 01A02767-C120-77B2-88A1-4E17C93A7549",
            "codex --remote unix:///tmp/codex.sock -p custom resume 01a02767-c120-77b2-88a1-4e17c93a7549",
            "codex --approve-for-me --remote unix:///tmp/codex.sock resume 01a02767-c120-77b2-88a1-4e17c93a7549 -a never",
            "codex --remote unix:///tmp/codex.sock resume 01a02767-c120-77b2-88a1-4e17c93a7549 -c model_provider=other",
        ] {
            let argv = shell_words::split(command).unwrap();
            let argv: Vec<_> = argv.iter().map(String::as_str).collect();
            let process = proc_info("codex", "/usr/bin/codex", &argv, 2, vec![]);
            assert_eq!(remote_codex_tui(&process), None, "accepted {}", command);
        }
    }

    #[test]
    #[ignore = "requires the real Codex CLI on PATH; --help exits before connecting"]
    fn remote_codex_tui_argument_forms_match_real_codex_parser() {
        for (command, _) in remote_codex_tui_argument_cases() {
            let argv = shell_words::split(command).unwrap();
            let process = proc_info(
                "codex",
                "/usr/bin/codex",
                &argv.iter().map(String::as_str).collect::<Vec<_>>(),
                2,
                vec![],
            );
            let parsed = remote_codex_tui(&process).unwrap();
            let mut original: Vec<_> = argv.into_iter().skip(1).collect();
            // Insert help before the positional-only delimiter, if any.
            let help_index = original
                .iter()
                .position(|arg| arg == "--")
                .unwrap_or(original.len());
            original.insert(help_index, "--help".into());
            let mut replay = vec![
                "resume".into(),
                "--remote".into(),
                parsed.endpoint,
                parsed.thread_id,
            ];
            replay.extend(parsed.tui_args);
            replay.push("--help".into());
            for args in [original, replay] {
                let output = std::process::Command::new("codex")
                    .args(&args)
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{}: {:?}: {}",
                    command,
                    args,
                    String::from_utf8_lossy(&output.stderr)
                );
                assert!(String::from_utf8_lossy(&output.stdout).contains("Usage:"));
            }
        }
    }

    #[test]
    fn remote_codex_tui_requires_managed_resume_shape_and_canonical_thread() {
        for argv in [
            vec!["codex", "resume", "01a02767-c120-77b2-88a1-4e17c93a7549"],
            vec![
                "codex",
                "resume",
                "--remote",
                "unix:///tmp/other.sock",
                "not-a-thread",
            ],
            vec![
                "codex",
                "resume",
                "--remote",
                "https://example.test/rpc",
                "01a02767-c120-77b2-88a1-4e17c93a7549",
            ],
        ] {
            let process = proc_info("codex", "/usr/bin/codex", &argv, 2, vec![]);
            assert_eq!(remote_codex_tui(&process), None, "accepted {argv:?}");
        }
    }

    #[test]
    fn codex_restore_recipe_quotes_process_arguments_without_evaluating_shell_text() {
        let process = proc_info(
            "codex",
            "/usr/local/bin/codex",
            &[
                "/usr/local/bin/codex",
                "--add-dir",
                "/tmp/a; $(touch /tmp/never-run)",
            ],
            1,
            vec![],
        );
        let command = native_restore_launch_command(&AgentHarness::Codex, &process).unwrap();

        assert_eq!(
            shell_words::split(&command).unwrap(),
            process.argv,
            "the normalized command must round-trip as data"
        );
    }

    #[test]
    fn codex_restore_recipe_rejects_auxiliary_code_mode_host() {
        let process = proc_info(
            "codex-code-mod",
            "/usr/local/bin/codex-code-mode-host",
            &["/usr/local/bin/codex-code-mode-host"],
            1,
            vec![],
        );

        assert_eq!(
            native_restore_launch_command(&AgentHarness::Codex, &process),
            None
        );
    }

    #[test]
    fn claude_restore_recipe_uses_expanded_process_argv_and_removes_session_selectors() {
        let process = proc_info(
            "zsh",
            "/usr/bin/zsh",
            &["zsh"],
            1,
            vec![proc_info(
                "claude",
                "/home/mihai/.local/bin/claude",
                &[
                    "/home/mihai/.local/bin/claude",
                    "--dangerously-skip-permissions",
                    "--add-dir",
                    "/home/mihai",
                    "--add-dir",
                    "/code",
                    "--add-dir",
                    ".git",
                    "--resume",
                    "00000000-0000-4000-8000-000000000002",
                    "--fork-session",
                ],
                2,
                vec![],
            )],
        );

        assert_eq!(
            native_restore_launch_command(&AgentHarness::Claude, &process).as_deref(),
            Some(
                "/home/mihai/.local/bin/claude --dangerously-skip-permissions --add-dir /home/mihai --add-dir /code --add-dir .git"
            )
        );
    }

    #[test]
    fn claude_restore_recipe_keeps_system_prompt_option_values() {
        let process = proc_info(
            "claude",
            "/home/mihai/.local/bin/claude",
            &[
                "claude",
                "--append-system-prompt-file",
                "/code/inquisition/prompt.md",
                "--system-prompt-snapshot",
                "on",
                "--resume",
                "00000000-0000-4000-8000-000000000002",
            ],
            2,
            vec![],
        );

        assert_eq!(
            native_restore_launch_command(&AgentHarness::Claude, &process).as_deref(),
            Some(
                "claude --append-system-prompt-file /code/inquisition/prompt.md --system-prompt-snapshot on"
            )
        );
    }

    #[test]
    fn agy_restore_recipe_uses_expanded_process_argv_and_removes_session_selectors() {
        let process = proc_info(
            "zsh",
            "/usr/bin/zsh",
            &["zsh"],
            1,
            vec![proc_info(
                "agy",
                "/home/mihai/.local/bin/agy",
                &[
                    "/home/mihai/.local/bin/agy",
                    "--dangerously-skip-permissions",
                    "--add-dir",
                    "/code",
                    "--continue",
                    "--conversation",
                    "00000000-0000-4000-8000-000000000001",
                    "--new-project",
                    "initial input",
                ],
                2,
                vec![],
            )],
        );

        assert_eq!(
            native_restore_launch_command(&AgentHarness::Agy, &process).as_deref(),
            Some("/home/mihai/.local/bin/agy --dangerously-skip-permissions --add-dir /code")
        );
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
    fn derives_runtime_status_from_authoritative_turn_state_then_fallback_signals() {
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
            launch_supervisor: None,
            codex_app_server: None,
        };
        let mut runtime = AgentRuntimeSnapshot::new(&metadata);
        assert_eq!(derive_runtime_status(&runtime), AgentStatus::Starting);

        runtime.last_output_at = Some(Utc::now());
        assert_eq!(derive_runtime_status(&runtime), AgentStatus::Busy);

        runtime.last_output_at = Some(Utc::now() - Duration::minutes(5));
        assert_eq!(derive_runtime_status(&runtime), AgentStatus::Idle);

        runtime.turn_state = AgentTurnState::WaitingOnAgent;
        assert_eq!(derive_runtime_status(&runtime), AgentStatus::Busy);

        runtime.turn_state = AgentTurnState::WaitingOnUser;
        runtime.last_output_at = Some(Utc::now());
        runtime.terminal_progress = Progress::Indeterminate;
        assert_eq!(derive_runtime_status(&runtime), AgentStatus::Idle);

        runtime.turn_phase = Some("interrupted".to_string());
        runtime.last_turn_completed_at = Some(Utc::now() - Duration::minutes(5));
        runtime.last_input_at = Some(Utc::now());
        assert_eq!(
            derive_effective_turn_state(&runtime),
            AgentTurnState::WaitingOnUser
        );
        runtime.turn_phase = None;
        runtime.last_turn_completed_at = None;
        runtime.last_input_at = None;

        runtime.attention_reason = Some("approval-requested".to_string());
        assert_eq!(derive_runtime_status(&runtime), AgentStatus::Busy);
        runtime.attention_reason = None;

        runtime.terminal_progress = Progress::Error(1);
        assert_eq!(derive_runtime_status(&runtime), AgentStatus::Errored);

        runtime.terminal_progress = Progress::None;
        runtime.observer_error = Some("observer failed".to_string());
        assert_eq!(derive_runtime_status(&runtime), AgentStatus::Errored);

        runtime.observer_error = None;
        runtime.turn_phase = Some("failed".to_string());
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
            launch_supervisor: None,
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
            launch_supervisor: None,
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
            launch_supervisor: None,
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
        let observed = observe_claude(cwd, None, None, None).unwrap().unwrap();
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
    fn claude_background_job_messages_reuse_the_pane_launch_flags() {
        let session = "bfe3a8cc-8d63-4b2e-b3a5-ec7a2593defa";
        assert_eq!(
            running_claude_job_message(
                "claude --dangerously-skip-permissions --add-dir /code \
                 --resume d7eba7e2-cc02-4819-b357-27b1140c6238",
                "bfe3a8cc",
                session,
            ),
            "This conversation runs as Claude background job bfe3a8cc, outside this pane, and \
             a mux restart will stop it. To move it into this pane, exit Claude, then run \
             `claude stop bfe3a8cc` and `claude --dangerously-skip-permissions --add-dir /code \
             --resume bfe3a8cc-8d63-4b2e-b3a5-ec7a2593defa`."
        );
        assert_eq!(
            dead_claude_job_message("claude attach bfe3a8cc", "bfe3a8cc", Some(session)),
            "Claude background job bfe3a8cc is not running. To continue the conversation in this \
             pane, run `claude --resume bfe3a8cc-8d63-4b2e-b3a5-ec7a2593defa` with your usual flags."
        );
    }

    #[test]
    fn restored_claude_observer_uses_the_exact_expected_session() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let cwd = "/tmp/claude-restore";
        let project_dir = temp.path().join(cwd.replace('/', "-"));
        fs::create_dir_all(&project_dir).unwrap();
        let expected_id = "00000000-0000-4000-8000-000000000006";
        let other_id = "00000000-0000-4000-8000-000000000007";
        let expected = project_dir.join(format!("{expected_id}.jsonl"));
        let other = project_dir.join(format!("{other_id}.jsonl"));
        fs::write(
            &expected,
            format!(
                "{{\"type\":\"assistant\",\"sessionId\":\"{expected_id}\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"expected\"}}]}}}}\n"
            ),
        )
        .unwrap();
        fs::write(
            &other,
            format!(
                "{{\"type\":\"assistant\",\"sessionId\":\"{other_id}\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"newer\"}}]}}}}\n"
            ),
        )
        .unwrap();

        set_env_path("WAKTERM_AGENT_CLAUDE_DIR", temp.path());
        let observed = observe_claude(cwd, None, Some(Utc::now()), Some(expected_id))
            .unwrap()
            .unwrap();
        remove_env_var("WAKTERM_AGENT_CLAUDE_DIR");

        assert_eq!(
            observed.session_path.as_deref(),
            Some(expected.to_string_lossy().as_ref())
        );
        assert_eq!(observed.progress_summary.as_deref(), Some("expected"));
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
        let observed = observe_codex("/tmp/project-b", None, None, None, None, None)
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
    fn codex_turn_identity_ignores_injected_environment_context() {
        let temp = TempDir::new().unwrap();
        let session = temp.path().join("rollout-context-injection.jsonl");
        fs::write(
            &session,
            concat!(
                "{\"ordinal\":10,\"type\":\"event_msg\",\"timestamp\":\"2026-08-25T16:35:07.000Z\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"turn-panetone\"}}\n",
                "{\"ordinal\":11,\"type\":\"response_item\",\"timestamp\":\"2026-08-25T16:35:07.067Z\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"<environment_context>\\n  <current_date>2026-08-25</current_date>\\n</environment_context>\"}],\"internal_chat_message_metadata_passthrough\":{\"turn_id\":\"turn-panetone\"}}}\n",
                "{\"ordinal\":12,\"type\":\"response_item\",\"timestamp\":\"2026-08-25T16:35:07.103Z\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"[Panetone cross-agent message]\\nFix bznz\"}],\"internal_chat_message_metadata_passthrough\":{\"turn_id\":\"turn-panetone\"}}}\n",
                "{\"ordinal\":13,\"type\":\"response_item\",\"timestamp\":\"2026-08-25T16:35:08Z\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"working\"}],\"internal_chat_message_metadata_passthrough\":{\"turn_id\":\"turn-panetone\"}}}\n",
                "{\"ordinal\":14,\"type\":\"event_msg\",\"timestamp\":\"2026-08-25T16:35:09Z\",\"payload\":{\"type\":\"task_complete\",\"turn_id\":\"turn-panetone\",\"last_agent_message\":\"done\"}}\n"
            ),
        )
        .unwrap();

        let details = read_last_codex_observation(&session).unwrap();
        let turn = details.observed_turn.unwrap();

        assert_eq!(turn.provider_turn_id, "turn-panetone");
        assert_eq!(
            turn.primary_user_message_sha256.as_deref(),
            Some(message_sha256("[Panetone cross-agent message]\nFix bznz").as_str())
        );
        assert_eq!(turn.user_message_count, 1);
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
        let configured_root = temp.path().join("standard-sessions");
        let alternate_root = temp.path().join("alternate-home").join("sessions");
        fs::create_dir_all(&configured_root).unwrap();
        fs::create_dir_all(&alternate_root).unwrap();
        let old = configured_root.join("rollout-old.jsonl");
        let live = alternate_root
            .join("rollout-2026-09-05T00-26-42-01a06fd1-992d-7c73-acdc-cbbcff7c3b7d.jsonl");
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
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"01a06fd1-992d-7c73-acdc-cbbcff7c3b7d\",\"cwd\":\"/tmp/process-owned\"}}\n",
                "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"live\"}]}}\n"
            ),
        )
        .unwrap();
        let _open_live_session = fs::File::open(&live).unwrap();
        let process_id = std::process::id();
        let process = LocalProcessInfo::with_root_pid(process_id).unwrap();

        set_env_path("WAKTERM_AGENT_CODEX_DIR", &configured_root);
        let observed = observe_codex(
            "/tmp/process-owned",
            Some(old.to_string_lossy().as_ref()),
            None,
            Some(process_id),
            Some(process.start_time),
            None,
        )
        .unwrap()
        .unwrap();
        let mismatched_incarnation = observe_codex(
            "/tmp/process-owned",
            Some(old.to_string_lossy().as_ref()),
            None,
            Some(process_id),
            Some(process.start_time + 1),
            None,
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

    #[cfg(target_os = "linux")]
    #[test]
    fn observe_codex_binds_remote_tui_to_its_resumed_thread() {
        use std::os::unix::fs::PermissionsExt;

        let _env_lock = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("sessions");
        fs::create_dir_all(root.join("2026/09/03")).unwrap();
        fs::create_dir_all(root.join("2026/09/30")).unwrap();
        let thread_id = "01a0695c-adf5-7210-b90e-20b194cb78e6";
        // The app-server holds this rollout open; the TUI process does not.
        let resumed = root.join(format!(
            "2026/09/03/rollout-2026-09-03T18-21-16-{thread_id}.jsonl"
        ));
        fs::write(
            &resumed,
            format!(
                concat!(
                    "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{}\",\"cwd\":\"/tmp/remote-tui\"}}}}\n",
                    "{{\"timestamp\":\"2026-10-01T02:27:00.000Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"turn-1\"}}}}\n"
                ),
                thread_id
            ),
        )
        .unwrap();
        // A newer rollout in the same directory must not be chosen by recency.
        let other = root.join(
            "2026/09/30/rollout-2026-09-30T22-00-00-01a0f000-0000-7000-8000-000000000000.jsonl",
        );
        fs::write(
            &other,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"01a0f000-0000-7000-8000-000000000000\",\"cwd\":\"/tmp/remote-tui\"}}\n",
        )
        .unwrap();

        let tui = temp.path().join("codex");
        fs::write(&tui, "#!/bin/sh\nsleep 60\n").unwrap();
        fs::set_permissions(&tui, fs::Permissions::from_mode(0o755)).unwrap();
        let mut child = std::process::Command::new(&tui)
            .args([
                "--remote",
                "unix:///run/user/1000/wakterm/codex-tui-1-11.sock",
                "resume",
                thread_id,
                "--cd",
                "/tmp/remote-tui",
            ])
            .spawn()
            .unwrap();
        let pid = child.id();
        let start_time = LocalProcessInfo::with_root_pid(pid).unwrap().start_time;

        set_env_path("WAKTERM_AGENT_CODEX_DIR", &root);
        // Adoption primes the observer with the creation time as its cutoff,
        // and confirms the TUI process identity.
        let metadata = AgentMetadata {
            agent_id: "remote-codex".to_string(),
            name: "codex".to_string(),
            launch_cmd: format!(
                "codex --remote unix:///run/user/1000/wakterm/codex-tui-1-11.sock resume {thread_id} --cd /tmp/remote-tui"
            ),
            declared_cwd: "/tmp/remote-tui".to_string(),
            adopted_pid: Some(pid),
            adopted_start_time: Some(start_time),
            created_at: Utc::now(),
            repo_root: None,
            worktree: None,
            branch: None,
            managed_checkout: false,
            launch_supervisor: None,
            codex_app_server: None,
        };
        let mut runtime = AgentRuntimeSnapshot::new(&metadata);
        prime_runtime_for_new_agent(&mut runtime, &metadata, Some("codex"));
        runtime.foreground_process_name = Some("codex".to_string());
        refresh_runtime_from_harness(&mut runtime, &metadata);
        let cutoff = runtime.observer_started_at.or(Some(metadata.created_at));
        let observed = observe_codex(
            "/tmp/remote-tui",
            None,
            cutoff,
            Some(pid),
            Some(start_time),
            None,
        );
        let other_expected = observe_codex(
            "/tmp/remote-tui",
            None,
            cutoff,
            Some(pid),
            Some(start_time),
            Some("01a0f000-0000-7000-8000-000000000000"),
        );
        let other_incarnation = observe_codex(
            "/tmp/remote-tui",
            None,
            cutoff,
            Some(pid),
            Some(start_time + 1),
            None,
        );
        remove_env_var("WAKTERM_AGENT_CODEX_DIR");
        child.kill().unwrap();
        child.wait().unwrap();

        // Admission reports a running turn as busy rather than unavailable.
        assert_eq!(
            runtime.session_path.as_deref(),
            Some(resumed.to_string_lossy().as_ref())
        );
        assert_eq!(runtime.transport, AgentTransport::ObservedPty);
        assert_eq!(runtime.turn_state, AgentTurnState::WaitingOnAgent);
        assert_eq!(runtime.status, AgentStatus::Busy);

        let observed = observed.unwrap().unwrap();
        assert_eq!(
            observed.session_path.as_deref(),
            Some(resumed.to_string_lossy().as_ref())
        );
        assert_eq!(observed.turn_state, AgentTurnState::WaitingOnAgent);
        assert!(other_expected.unwrap().is_none());
        assert!(other_incarnation.unwrap().is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn observe_codex_prefers_expected_session_when_process_owns_multiple() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let temp = TempDir::new().unwrap();
        let expected = temp.path().join("rollout-expected.jsonl");
        let newer = temp.path().join("rollout-newer.jsonl");
        fs::write(
            &expected,
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"session-expected\",\"cwd\":\"/tmp/process-owned\"}}\n",
                "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"expected\"}]}}\n"
            ),
        )
        .unwrap();
        fs::write(
            &newer,
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"session-other\",\"cwd\":\"/tmp/process-owned\"}}\n",
                "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"other\"}]}}\n"
            ),
        )
        .unwrap();
        fs::File::open(&expected)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1))
            .unwrap();
        fs::File::open(&newer)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(2))
            .unwrap();
        let _open_expected = fs::File::open(&expected).unwrap();
        let _open_newer = fs::File::open(&newer).unwrap();
        let process_id = std::process::id();
        let process = LocalProcessInfo::with_root_pid(process_id).unwrap();

        set_env_path("WAKTERM_AGENT_CODEX_DIR", temp.path());
        let observed = observe_codex(
            "/tmp/process-owned",
            None,
            None,
            Some(process_id),
            Some(process.start_time),
            Some("session-expected"),
        )
        .unwrap()
        .unwrap();
        remove_env_var("WAKTERM_AGENT_CODEX_DIR");

        assert_eq!(
            observed.session_path.as_deref(),
            Some(expected.to_string_lossy().as_ref())
        );
        assert_eq!(observed.progress_summary.as_deref(), Some("expected"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn observe_codex_ignores_internal_auto_review_session_owned_by_process() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let temp = TempDir::new().unwrap();
        let primary = temp.path().join("rollout-primary.jsonl");
        let auto_review = temp.path().join("rollout-auto-review.jsonl");
        fs::write(
            &primary,
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"session_id\":\"session-primary\",\"id\":\"session-primary\",\"cwd\":\"/tmp/process-owned\",\"source\":\"cli\",\"thread_source\":\"user\"}}\n",
                "{\"ordinal\":1,\"type\":\"event_msg\",\"timestamp\":\"2026-09-02T17:46:30Z\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"turn-primary\"}}\n",
                "{\"ordinal\":2,\"type\":\"response_item\",\"timestamp\":\"2026-09-02T17:46:35Z\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"visible progress\"}],\"internal_chat_message_metadata_passthrough\":{\"turn_id\":\"turn-primary\"}}}\n"
            ),
        )
        .unwrap();
        fs::write(
            &auto_review,
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"session_id\":\"session-primary\",\"id\":\"session-review\",\"parent_thread_id\":\"session-primary\",\"cwd\":\"/tmp/process-owned\",\"source\":{\"subagent\":{\"other\":\"guardian\"}},\"thread_source\":\"guardian_review\"}}\n",
                "{\"ordinal\":1,\"type\":\"event_msg\",\"timestamp\":\"2026-09-02T17:46:37Z\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"turn-review\"}}\n",
                "{\"ordinal\":2,\"type\":\"response_item\",\"timestamp\":\"2026-09-02T17:46:40Z\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"content\":[{\"type\":\"output_text\",\"text\":\"{\\\"outcome\\\":\\\"allow\\\"}\"}],\"internal_chat_message_metadata_passthrough\":{\"turn_id\":\"turn-review\",\"content_item_kinds\":[\"unknown\"]}}}\n",
                "{\"ordinal\":3,\"type\":\"event_msg\",\"timestamp\":\"2026-09-02T17:46:40Z\",\"payload\":{\"type\":\"task_complete\",\"turn_id\":\"turn-review\",\"last_agent_message\":\"{\\\"outcome\\\":\\\"allow\\\"}\"}}\n"
            ),
        )
        .unwrap();
        fs::File::open(&primary)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1))
            .unwrap();
        fs::File::open(&auto_review)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(2))
            .unwrap();
        let _open_primary = fs::File::open(&primary).unwrap();
        let _open_auto_review = fs::File::open(&auto_review).unwrap();
        let process_id = std::process::id();
        let process = LocalProcessInfo::with_root_pid(process_id).unwrap();

        set_env_path("WAKTERM_AGENT_CODEX_DIR", temp.path());
        let observed = observe_codex(
            "/tmp/process-owned",
            Some(auto_review.to_string_lossy().as_ref()),
            None,
            Some(process_id),
            Some(process.start_time),
            Some("session-primary"),
        )
        .unwrap()
        .unwrap();
        remove_env_var("WAKTERM_AGENT_CODEX_DIR");

        assert_eq!(
            observed.session_path.as_deref(),
            Some(primary.to_string_lossy().as_ref())
        );
        assert_eq!(
            observed.progress_summary.as_deref(),
            Some("visible progress")
        );
        assert_eq!(
            observed
                .observed_turn
                .as_ref()
                .map(|turn| turn.provider_turn_id.as_str()),
            Some("turn-primary")
        );
    }

    #[test]
    fn codex_session_visibility_uses_provider_thread_provenance() {
        for thread_source in [
            "subagent",
            "guardian_review",
            "memory_consolidation",
            "ambient_memory",
        ] {
            assert!(!codex_session_record_is_user_visible(&serde_json::json!({
                "type": "session_meta",
                "payload": {
                    "source": "cli",
                    "thread_source": thread_source,
                }
            })));
        }
        assert!(!codex_session_record_is_user_visible(&serde_json::json!({
            "type": "session_meta",
            "payload": {"source": {"subagent": {"other": "grader"}}}
        })));
        assert!(codex_session_record_is_user_visible(&serde_json::json!({
            "type": "session_meta",
            "payload": {"source": "cli", "thread_source": "user"}
        })));
        assert!(codex_session_record_is_user_visible(&serde_json::json!({
            "type": "session_meta",
            "payload": {"source": "cli"}
        })));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn observe_codex_follows_new_continuation_after_restore_handshake() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let temp = TempDir::new().unwrap();
        let restored = temp.path().join("rollout-restored.jsonl");
        let continuation = temp.path().join("rollout-continuation.jsonl");
        fs::write(
            &restored,
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"session-restored\",\"cwd\":\"/tmp/process-owned\"}}\n",
                "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"stale\"}]}}\n"
            ),
        )
        .unwrap();
        fs::write(
            &continuation,
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"session-continuation\",\"cwd\":\"/tmp/process-owned\"}}\n",
                "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"okay\"}]}}\n"
            ),
        )
        .unwrap();
        fs::File::open(&restored)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1))
            .unwrap();
        fs::File::open(&continuation)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(2))
            .unwrap();
        let _open_restored_session = fs::File::open(&restored).unwrap();
        let _open_continuation_session = fs::File::open(&continuation).unwrap();
        let process_id = std::process::id();
        let process = LocalProcessInfo::with_root_pid(process_id).unwrap();

        set_env_path("WAKTERM_AGENT_CODEX_DIR", temp.path());
        let observed = observe_codex(
            "/tmp/process-owned",
            Some(restored.to_string_lossy().as_ref()),
            None,
            Some(process_id),
            Some(process.start_time),
            None,
        )
        .unwrap()
        .unwrap();
        remove_env_var("WAKTERM_AGENT_CODEX_DIR");

        assert_eq!(
            observed.session_path.as_deref(),
            Some(continuation.to_string_lossy().as_ref())
        );
        assert_eq!(observed.progress_summary.as_deref(), Some("okay"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn observe_codex_settles_running_turn_left_behind_by_restart() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let temp = TempDir::new().unwrap();
        let session = temp.path().join("rollout-interrupted-by-restart.jsonl");
        fs::write(
            &session,
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"session-restored\",\"cwd\":\"/tmp/process-owned\"}}\n",
                "{\"ordinal\":1,\"type\":\"event_msg\",\"timestamp\":\"2026-03-17T12:00:00Z\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"turn-1\",\"collaboration_mode_kind\":\"default\"}}\n",
                "{\"ordinal\":2,\"type\":\"response_item\",\"timestamp\":\"2026-03-17T12:00:01Z\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"previous answer\"}],\"internal_chat_message_metadata_passthrough\":{\"turn_id\":\"turn-1\"}}}\n",
                "{\"ordinal\":3,\"type\":\"event_msg\",\"timestamp\":\"2026-03-17T12:00:02Z\",\"payload\":{\"type\":\"task_complete\",\"turn_id\":\"turn-1\",\"last_agent_message\":\"previous answer\"}}\n",
                "{\"ordinal\":4,\"type\":\"event_msg\",\"timestamp\":\"2026-03-17T12:05:00Z\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"turn-2\",\"collaboration_mode_kind\":\"default\"}}\n",
                "{\"ordinal\":5,\"type\":\"response_item\",\"timestamp\":\"2026-03-17T12:05:01Z\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"interrupted request\"}],\"internal_chat_message_metadata_passthrough\":{\"turn_id\":\"turn-2\"}}}\n"
            ),
        )
        .unwrap();
        fs::File::open(&session)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1))
            .unwrap();
        let _open_session = fs::File::open(&session).unwrap();
        let process_id = std::process::id();
        let process = LocalProcessInfo::with_root_pid(process_id).unwrap();

        set_env_path("WAKTERM_AGENT_CODEX_DIR", temp.path());
        let observed = observe_codex(
            "/tmp/process-owned",
            None,
            None,
            Some(process_id),
            Some(process.start_time),
            Some("session-restored"),
        )
        .unwrap()
        .unwrap();
        remove_env_var("WAKTERM_AGENT_CODEX_DIR");

        assert_eq!(observed.turn_state, AgentTurnState::WaitingOnUser);
        assert_eq!(observed.turn_phase.as_deref(), Some("interrupted"));
        assert_eq!(
            observed.last_turn_completed_at,
            Some(Utc.with_ymd_and_hms(2026, 3, 17, 12, 0, 2).unwrap())
        );
        let turn = observed.observed_turn.unwrap();
        assert_eq!(turn.provider_turn_id, "turn-2");
        assert_eq!(turn.outcome, AgentObservedTurnOutcome::Aborted);
        assert!(turn.completed_at.is_some());
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

        let command = native_resume_command(
            &AgentHarness::Codex,
            "codex -a never -s danger-full-access",
            "session-resume",
        )
        .unwrap();
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
    fn claude_restore_command_uses_the_persisted_session_id() {
        let temp = TempDir::new().unwrap();
        let session = temp.path().join("session.jsonl");
        fs::write(
            &session,
            concat!(
                "{\"type\":\"file-history-snapshot\"}\n",
                "{\"type\":\"user\",\"sessionId\":\"00000000-0000-4000-8000-000000000003\"}\n"
            ),
        )
        .unwrap();

        assert_eq!(
            claude_session_id(&session).unwrap().as_deref(),
            Some("00000000-0000-4000-8000-000000000003")
        );

        let command = native_resume_command(
            &AgentHarness::Claude,
            "claude --dangerously-skip-permissions 'original prompt' --resume old --fork-session",
            "00000000-0000-4000-8000-000000000003",
        )
        .unwrap();
        let argv = command
            .get_argv()
            .iter()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            argv,
            vec![
                "claude",
                "--dangerously-skip-permissions",
                "--resume",
                "00000000-0000-4000-8000-000000000003",
            ]
        );
    }

    #[test]
    fn agy_restore_command_uses_the_confirmed_conversation_id() {
        let conversation_id = "00000000-0000-4000-8000-000000000004";
        let transcript = Path::new("/tmp/antigravity-cli")
            .join("brain")
            .join(conversation_id)
            .join(".system_generated")
            .join("logs")
            .join("transcript.jsonl");

        assert_eq!(
            restorable_session_id(&AgentHarness::Agy, &transcript)
                .unwrap()
                .as_deref(),
            Some(conversation_id)
        );

        let command = native_resume_command(
            &AgentHarness::Agy,
            "agy --dangerously-skip-permissions --conversation old --continue 'old input'",
            conversation_id,
        )
        .unwrap();
        let argv = command
            .get_argv()
            .iter()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            argv,
            vec![
                "agy",
                "--dangerously-skip-permissions",
                "--conversation",
                conversation_id,
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
        let observed = observe_codex("/tmp/project-live", None, None, None, None, None)
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
        let observed = observe_codex("/tmp/project-f", None, None, None, None, None)
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
    fn running_codex_turn_keeps_previous_completed_turn_timestamp() {
        let temp = TempDir::new().unwrap();
        let session = temp.path().join("rollout-running-after-complete.jsonl");
        fs::write(
            &session,
            concat!(
                "{\"ordinal\":1,\"type\":\"event_msg\",\"timestamp\":\"2026-03-17T12:00:00Z\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"turn-1\",\"collaboration_mode_kind\":\"default\"}}\n",
                "{\"ordinal\":2,\"type\":\"response_item\",\"timestamp\":\"2026-03-17T12:00:01Z\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"first answer\"}],\"internal_chat_message_metadata_passthrough\":{\"turn_id\":\"turn-1\"}}}\n",
                "{\"ordinal\":3,\"type\":\"event_msg\",\"timestamp\":\"2026-03-17T12:00:02Z\",\"payload\":{\"type\":\"task_complete\",\"turn_id\":\"turn-1\",\"last_agent_message\":\"first answer\"}}\n",
                "{\"ordinal\":4,\"type\":\"event_msg\",\"timestamp\":\"2026-03-17T12:05:00Z\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"turn-2\",\"collaboration_mode_kind\":\"default\"}}\n",
                "{\"ordinal\":5,\"type\":\"response_item\",\"timestamp\":\"2026-03-17T12:05:01Z\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"second request\"}],\"internal_chat_message_metadata_passthrough\":{\"turn_id\":\"turn-2\"}}}\n",
                "{\"ordinal\":6,\"type\":\"event_msg\",\"timestamp\":\"2026-03-17T12:05:02Z\",\"payload\":{\"type\":\"agent_message\",\"turn_id\":\"turn-2\",\"phase\":\"commentary\"}}\n"
            ),
        )
        .unwrap();

        let observed = read_last_codex_observation(&session).unwrap();

        assert_eq!(observed.turn_state, AgentTurnState::WaitingOnAgent);
        assert_eq!(
            observed.last_turn_completed_at,
            Some(Utc.with_ymd_and_hms(2026, 3, 17, 12, 0, 2).unwrap())
        );
        let turn = observed.observed_turn.unwrap();
        assert_eq!(turn.provider_turn_id, "turn-2");
        assert_eq!(turn.outcome, AgentObservedTurnOutcome::Running);
        assert_eq!(turn.completed_at, None);
    }

    #[test]
    fn waiting_on_agent_keeps_the_previous_assistant_completion_timestamp() {
        let previous_assistant = Utc.with_ymd_and_hms(2026, 3, 17, 12, 0, 0).unwrap();
        let current_user = Utc.with_ymd_and_hms(2026, 3, 17, 12, 5, 0).unwrap();

        assert_eq!(
            derive_turn_state(Some(current_user), Some(previous_assistant)),
            (AgentTurnState::WaitingOnAgent, Some(previous_assistant))
        );
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
        let observed = observe_codex("/tmp/project-g", None, None, None, None, None)
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
        let observed =
            observe_opencode(&AgentHarness::Opencode, "/tmp/project-j", None, None, None)
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
    fn detects_zcode_by_program_name_only() {
        assert_eq!(infer_harness("zcode", None), AgentHarness::Zcode);
        assert_eq!(
            infer_harness(
                "/home/u/.local/share/zcode-cli/versions/v/zcode.cjs --resume sess_x",
                None
            ),
            AgentHarness::Zcode
        );
        assert_eq!(infer_harness("", Some("zcode")), AgentHarness::Zcode);
        assert_eq!(
            infer_harness("codex --cd /code/zcode", None),
            AgentHarness::Codex
        );
        assert_eq!(
            infer_harness("vim /code/zcode/notes", None),
            AgentHarness::Unknown
        );
    }

    #[test]
    fn zcode_restore_resumes_the_exact_session_with_the_panes_options() {
        let session = "sess_64d771f3-373b-4147-9529-0f35c720bfc8";
        let command = native_resume_command(
            &AgentHarness::Zcode,
            "zcode --mode yolo --resume sess_00000000-0000-4000-8000-000000000001 -p 'do it' -c",
            session,
        )
        .unwrap();
        assert_eq!(
            command
                .get_argv()
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            ["zcode", "--mode", "yolo", "--resume", session]
        );
        assert!(native_resume_command(&AgentHarness::Zcode, "zcode", "not-a-session").is_err());
        assert_eq!(
            zcode_resumed_session(&AgentHarness::Zcode, &format!("zcode --resume={session}")),
            Some(session.to_string())
        );
        assert_eq!(zcode_resumed_session(&AgentHarness::Zcode, "zcode"), None);
        let url = format!("opencode://session?db=/tmp/db.sqlite&id={session}");
        assert_eq!(
            restorable_session_id(&AgentHarness::Zcode, Path::new(&url)).unwrap(),
            Some(session.to_string())
        );
    }

    #[test]
    fn zcode_observes_the_resumed_session_over_a_newer_one() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("db.sqlite");
        let connection = create_opencode_test_db(&db_path);
        for (session, updated, text) in [
            (
                "sess_00000000-0000-4000-8000-00000000000a",
                1_000_i64,
                "resumed",
            ),
            (
                "sess_00000000-0000-4000-8000-00000000000b",
                2_000_i64,
                "newer",
            ),
        ] {
            connection
                .execute(
                    "INSERT INTO session (id, directory, time_updated) VALUES (?1, ?2, ?3)",
                    params![session, "/tmp/zcode-project", updated],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO message (id, session_id, time_created, data) VALUES (?1, ?2, ?3, ?4)",
                    params![format!("m-{session}"), session, updated, r#"{"role":"assistant"}"#],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO part (id, message_id, session_id, time_created, data) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        format!("p-{session}"),
                        format!("m-{session}"),
                        session,
                        updated,
                        format!(r#"{{"type":"text","text":"{text}"}}"#)
                    ],
                )
                .unwrap();
        }
        drop(connection);

        set_env_path("WAKTERM_AGENT_ZCODE_DB", &db_path);
        let by_directory =
            observe_opencode(&AgentHarness::Zcode, "/tmp/zcode-project", None, None, None)
                .unwrap()
                .unwrap();
        let resumed = observe_opencode(
            &AgentHarness::Zcode,
            "/tmp/zcode-project",
            None,
            None,
            Some("sess_00000000-0000-4000-8000-00000000000a"),
        )
        .unwrap()
        .unwrap();
        remove_env_var("WAKTERM_AGENT_ZCODE_DB");

        assert_eq!(by_directory.progress_summary.as_deref(), Some("newer"));
        assert_eq!(resumed.progress_summary.as_deref(), Some("resumed"));
        let (session_db, session_id) =
            parse_opencode_session_path(resumed.session_path.as_deref().unwrap()).unwrap();
        assert_eq!(session_db, db_path);
        assert_eq!(session_id, "sess_00000000-0000-4000-8000-00000000000a");
    }

    /// Runs against a copy of a real ZCode database, whose schema belongs to
    /// ZCode: WAKTERM_TEST_ZCODE_DB=~/.zcode/cli/db/db.sqlite.
    #[test]
    #[ignore = "requires WAKTERM_TEST_ZCODE_DB pointing to a real ZCode database"]
    fn zcode_real_database_observation() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let source = PathBuf::from(std::env::var_os("WAKTERM_TEST_ZCODE_DB").unwrap());
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("db.sqlite");
        // VACUUM INTO takes a consistent copy that includes the WAL.
        Connection::open_with_flags(&source, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap()
            .execute("VACUUM INTO ?1", params![db_path.to_string_lossy()])
            .unwrap();
        let (session, directory): (String, String) = Connection::open(&db_path)
            .unwrap()
            .query_row(
                "SELECT id, directory FROM session ORDER BY time_updated DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();

        set_env_path("WAKTERM_AGENT_ZCODE_DB", &db_path);
        let observed =
            observe_opencode(&AgentHarness::Zcode, &directory, None, None, Some(&session))
                .unwrap()
                .unwrap();
        remove_env_var("WAKTERM_AGENT_ZCODE_DB");

        let session_path = observed.session_path.unwrap();
        assert_eq!(
            restorable_session_id(&AgentHarness::Zcode, Path::new(&session_path)).unwrap(),
            Some(session.clone())
        );
        eprintln!(
            "session {session} in {directory}: {:?}, summary {:?}",
            observed.turn_state,
            observed
                .progress_summary
                .map(|text| text.chars().take(80).collect::<String>())
        );
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
            launch_supervisor: None,
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
            launch_supervisor: None,
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
    fn refresh_runtime_marks_waiting_on_agent_and_keeps_previous_turn_end() {
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
            launch_supervisor: None,
            codex_app_server: None,
        };
        let mut runtime = AgentRuntimeSnapshot::new(&metadata);
        runtime.foreground_process_name = Some("claude".to_string());
        refresh_runtime_from_harness(&mut runtime, &metadata);
        remove_env_var("WAKTERM_AGENT_CLAUDE_DIR");

        assert_eq!(runtime.turn_state, AgentTurnState::WaitingOnAgent);
        assert_eq!(
            runtime.last_turn_completed_at,
            Some(Utc.with_ymd_and_hms(2026, 3, 17, 12, 0, 0).unwrap())
        );
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
            launch_supervisor: None,
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
            launch_supervisor: None,
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
            launch_supervisor: None,
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
            launch_supervisor: None,
            codex_app_server: None,
        };
        let mut runtime = AgentRuntimeSnapshot::new(&metadata);
        runtime.foreground_process_name = Some("codex".to_string());
        runtime.last_output_at = Some(Utc::now());
        runtime.terminal_progress = Progress::Indeterminate;
        refresh_runtime_from_harness(&mut runtime, &metadata);
        remove_env_var("WAKTERM_AGENT_CODEX_DIR");

        assert_eq!(runtime.turn_phase.as_deref(), Some("aborted"));
        assert_eq!(runtime.turn_state, AgentTurnState::WaitingOnUser);
        assert_eq!(runtime.status, AgentStatus::Idle);
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
