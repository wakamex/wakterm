use crate::cli::{resolve_relative_cwd, CliOutputFormatKind};
use anyhow::{bail, Context};
use chrono::Utc;
use clap::{Parser, Subcommand, ValueEnum, ValueHint};
use codec::{InputSerial, ListPanesResponse, SendKeyDown, SpawnV2, TabTitleChanged};
use config::keyassignment::SpawnTabDomain;
use config::ConfigHandle;
use mux::agent::{
    infer_harness, pending_observer_detail, AgentHarness, AgentMetadata, AgentSnapshot,
    AgentStatus, AgentTransport, AgentTurnState,
};
use mux::pane::PaneId;
use mux::tab::{SplitDirection, SplitRequest, SplitSize};
use mux::window::WindowId;
use portable_pty::cmdbuilder::CommandBuilder;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::future::Future;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::time::{Duration, Instant};
use tabout::{tabulate_output, Alignment, Column};
use termwiz::input::{KeyCode, KeyEvent, Modifiers};
use uuid::Uuid;
use wakterm_client::client::Client;

const STARTUP_STABILIZATION_DELAY_MS: u64 = 200;

#[derive(Debug, Parser, Clone)]
pub struct AgentCommand {
    #[command(subcommand)]
    sub: AgentSubCommand,
}

#[derive(Debug, Subcommand, Clone)]
enum AgentSubCommand {
    #[command(
        name = "start",
        about = "start an agent harness in the current pane, a split, a new tab, or a new window"
    )]
    Start(SpawnAgentCommand),

    #[command(name = "adopt", about = "adopt an existing pane as an agent")]
    Adopt(AdoptAgentCommand),

    #[command(name = "list", about = "list agent-tagged panes")]
    List(ListAgentsCommand),

    #[command(
        name = "watch",
        about = "stream latest observer-backed harness messages for agent panes"
    )]
    Watch(WatchAgentsCommand),

    #[command(name = "inspect", about = "inspect a single agent by name or id")]
    Inspect(InspectAgentCommand),

    #[command(name = "send", about = "send a message to an agent pane")]
    Send(SendAgentCommand),

    #[command(name = "interrupt", about = "interrupt a native harness turn")]
    Interrupt(InterruptAgentCommand),

    #[command(name = "set", about = "attach agent metadata to a pane")]
    Set(SetAgentCommand),

    #[command(name = "clear", about = "remove agent metadata from a pane")]
    Clear(ClearAgentCommand),
}

impl AgentCommand {
    pub async fn run(&self, client: Client, config: &ConfigHandle) -> anyhow::Result<()> {
        match &self.sub {
            AgentSubCommand::Start(cmd) => cmd.run(client, config).await,
            AgentSubCommand::Adopt(cmd) => cmd.run(client).await,
            AgentSubCommand::List(cmd) => cmd.run(client).await,
            AgentSubCommand::Watch(cmd) => cmd.run(client).await,
            AgentSubCommand::Inspect(cmd) => cmd.run(client).await,
            AgentSubCommand::Send(cmd) => cmd.run(client).await,
            AgentSubCommand::Interrupt(cmd) => cmd.run(client).await,
            AgentSubCommand::Set(cmd) => cmd.run(client).await,
            AgentSubCommand::Clear(cmd) => cmd.run(client).await,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WorktreeMode {
    None,
    Auto,
    Path(PathBuf),
}

#[derive(Debug, Clone)]
struct PreparedAgentLaunch {
    command: CommandBuilder,
    launch_cmd: String,
    command_dir: String,
    repo_root: Option<String>,
    worktree: Option<String>,
    branch: Option<String>,
    managed_checkout: bool,
}

#[derive(Debug, Clone)]
struct PaneContext {
    window_id: WindowId,
    tab_id: mux::tab::TabId,
    tab_title: String,
    tab_size: wakterm_term::TerminalSize,
    cwd: Option<String>,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum AgentStartHarness {
    Claude,
    Codex,
    Gemini,
    Opencode,
}

impl AgentStartHarness {
    fn as_agent_harness(self) -> AgentHarness {
        match self {
            Self::Claude => AgentHarness::Claude,
            Self::Codex => AgentHarness::Codex,
            Self::Gemini => AgentHarness::Gemini,
            Self::Opencode => AgentHarness::Opencode,
        }
    }

    fn default_command(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
            Self::Opencode => "opencode",
        }
    }
}

#[derive(Debug, Parser, Clone)]
pub struct SpawnAgentCommand {
    /// Built-in harness to launch. Preferred for claude/codex/gemini/opencode.
    #[arg(value_enum, value_name = "HARNESS", required_unless_present = "cmd")]
    harness: Option<AgentStartHarness>,

    /// Start the harness in the current pane instead of creating a new pane/tab/window.
    #[arg(long, conflicts_with_all = &["split", "new_window", "workspace", "horizontal", "left", "right", "top", "bottom", "cells", "percent"])]
    here: bool,

    /// Replace the current shell process when used with --here.
    #[arg(long, requires = "here")]
    replace: bool,

    /// Stable human-readable name for this agent. Defaults to codex/claude with a numeric suffix.
    #[arg(long)]
    name: Option<String>,

    /// Spawn into a split instead of creating a new tab
    #[arg(long, conflicts_with_all = &["new_window", "workspace"])]
    split: bool,

    /// Specify the current pane or split target. Defaults to WAKTERM_PANE.
    #[arg(long)]
    pane_id: Option<PaneId>,

    /// When not splitting, create a new window instead of a new tab.
    #[arg(long, conflicts_with = "split")]
    new_window: bool,

    /// Workspace to use when creating a new window.
    #[arg(long, requires = "new_window")]
    workspace: Option<String>,

    /// Equivalent to `--right`.
    #[arg(long, conflicts_with_all = &["left", "right", "top", "bottom"])]
    horizontal: bool,

    /// Split horizontally, with the new pane on the left
    #[arg(long, conflicts_with_all = &["right", "top", "bottom"])]
    left: bool,

    /// Split horizontally, with the new pane on the right
    #[arg(long, conflicts_with_all = &["left", "top", "bottom"])]
    right: bool,

    /// Split vertically, with the new pane on the top
    #[arg(long, conflicts_with_all = &["left", "right", "bottom"])]
    top: bool,

    /// Split vertically, with the new pane on the bottom
    #[arg(long, conflicts_with_all = &["left", "right", "top"])]
    bottom: bool,

    /// Number of cells for the new split
    #[arg(long, conflicts_with = "percent")]
    cells: Option<usize>,

    /// Percentage for the new split
    #[arg(long)]
    percent: Option<u8>,

    /// Repository root or any path inside the target repository
    #[arg(long, value_hint = ValueHint::DirPath)]
    repo: Option<PathBuf>,

    /// Worktree mode: `none`, `auto`, or an explicit path
    #[arg(long, default_value = "none", value_parser = parse_worktree_mode)]
    worktree: WorktreeMode,

    /// Branch to create or checkout before launch
    #[arg(long)]
    branch: Option<String>,

    /// Override the launch cwd directly
    #[arg(long, value_parser, value_hint = ValueHint::DirPath)]
    cwd: Option<OsString>,

    /// Explicit command line to launch. Overrides the default command for the selected harness.
    #[arg(long)]
    cmd: Option<String>,
}

impl SpawnAgentCommand {
    fn resolved_harness(&self) -> anyhow::Result<AgentHarness> {
        if let Some(harness) = self.harness {
            return Ok(harness.as_agent_harness());
        }

        let launch_cmd = self
            .cmd
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("agent start requires a harness name or --cmd"))?;
        let harness = infer_harness(launch_cmd, None);
        anyhow::ensure!(
            !matches!(harness, AgentHarness::Unknown),
            "agent start requires a recognized harness (currently: claude, codex, gemini, opencode); if you are using a wrapper command, specify the harness positionally and pass the wrapper via --cmd"
        );
        Ok(harness)
    }

    fn resolved_launch_cmd(&self) -> anyhow::Result<&str> {
        if let Some(cmd) = self.cmd.as_deref() {
            anyhow::ensure!(!cmd.trim().is_empty(), "--cmd must not be empty");
            return Ok(cmd);
        }

        let harness = self
            .harness
            .ok_or_else(|| anyhow::anyhow!("agent start requires a harness name or --cmd"))?;
        Ok(harness.default_command())
    }

    async fn run(&self, client: Client, config: &ConfigHandle) -> anyhow::Result<()> {
        let snapshot = self
            .run_with(
                config,
                || client.list_agents(),
                || client.list_panes(),
                || client.list_agents(),
                |pane_id| client.resolve_pane_id(pane_id),
                |request| client.spawn_v2(request),
                |request| client.split_pane(request),
                |request| client.send_paste(request),
                |request| client.key_down(request),
                |request| client.set_tab_title(request),
                |request| client.set_agent_metadata(request),
                |request| client.clear_agent_metadata(request),
                |request| client.kill_pane(request),
                |cmd, agent_name, agents, current_cwd| {
                    cmd.prepare_launch(agent_name, agents, current_cwd)
                },
            )
            .await?;
        write_json(&snapshot)
    }

    async fn run_with<
        ListAgents,
        ListAgentsFut,
        ListPanes,
        ListPanesFut,
        ListAgentsAfterSet,
        ListAgentsAfterSetFut,
        ResolvePaneId,
        ResolvePaneIdFut,
        SpawnV2Fn,
        SpawnV2Fut,
        SplitPaneFn,
        SplitPaneFut,
        SendPasteFn,
        SendPasteFut,
        KeyDownFn,
        KeyDownFut,
        SetTabTitleFn,
        SetTabTitleFut,
        SetAgentMetadataFn,
        SetAgentMetadataFut,
        ClearAgentMetadataFn,
        ClearAgentMetadataFut,
        KillPaneFn,
        KillPaneFut,
        PrepareLaunchFn,
    >(
        &self,
        config: &ConfigHandle,
        list_agents: ListAgents,
        list_panes: ListPanes,
        mut list_agents_after_set: ListAgentsAfterSet,
        resolve_pane_id: ResolvePaneId,
        spawn_v2: SpawnV2Fn,
        split_pane: SplitPaneFn,
        mut send_paste: SendPasteFn,
        mut key_down: KeyDownFn,
        mut set_tab_title: SetTabTitleFn,
        mut set_agent_metadata: SetAgentMetadataFn,
        mut clear_agent_metadata: ClearAgentMetadataFn,
        mut kill_pane: KillPaneFn,
        prepare_launch: PrepareLaunchFn,
    ) -> anyhow::Result<AgentSnapshot>
    where
        ListAgents: FnOnce() -> ListAgentsFut,
        ListAgentsFut: Future<Output = anyhow::Result<codec::ListAgentsResponse>>,
        ListPanes: FnOnce() -> ListPanesFut,
        ListPanesFut: Future<Output = anyhow::Result<ListPanesResponse>>,
        ListAgentsAfterSet: FnMut() -> ListAgentsAfterSetFut,
        ListAgentsAfterSetFut: Future<Output = anyhow::Result<codec::ListAgentsResponse>>,
        ResolvePaneId: FnOnce(Option<PaneId>) -> ResolvePaneIdFut,
        ResolvePaneIdFut: Future<Output = anyhow::Result<PaneId>>,
        SpawnV2Fn: FnOnce(codec::SpawnV2) -> SpawnV2Fut,
        SpawnV2Fut: Future<Output = anyhow::Result<codec::SpawnResponse>>,
        SplitPaneFn: FnOnce(codec::SplitPane) -> SplitPaneFut,
        SplitPaneFut: Future<Output = anyhow::Result<codec::SpawnResponse>>,
        SendPasteFn: FnMut(codec::SendPaste) -> SendPasteFut,
        SendPasteFut: Future<Output = anyhow::Result<codec::UnitResponse>>,
        KeyDownFn: FnMut(SendKeyDown) -> KeyDownFut,
        KeyDownFut: Future<Output = anyhow::Result<codec::UnitResponse>>,
        SetTabTitleFn: FnMut(codec::TabTitleChanged) -> SetTabTitleFut,
        SetTabTitleFut: Future<Output = anyhow::Result<codec::UnitResponse>>,
        SetAgentMetadataFn: FnMut(codec::SetAgentMetadata) -> SetAgentMetadataFut,
        SetAgentMetadataFut: Future<Output = anyhow::Result<codec::UnitResponse>>,
        ClearAgentMetadataFn: FnMut(codec::ClearAgentMetadata) -> ClearAgentMetadataFut,
        ClearAgentMetadataFut: Future<Output = anyhow::Result<codec::UnitResponse>>,
        KillPaneFn: FnMut(codec::KillPane) -> KillPaneFut,
        KillPaneFut: Future<Output = anyhow::Result<codec::UnitResponse>>,
        PrepareLaunchFn: FnOnce(
            &SpawnAgentCommand,
            &str,
            &[AgentSnapshot],
            Option<String>,
        ) -> anyhow::Result<PreparedAgentLaunch>,
    {
        let context_pane_id =
            if self.here || self.split || self.pane_id.is_some() || !self.new_window {
                Some(resolve_pane_id(self.pane_id).await?)
            } else {
                None
            };

        let panes = if context_pane_id.is_some() {
            Some(list_panes().await?)
        } else {
            None
        };
        let pane_context = context_pane_id.and_then(|pane_id| {
            panes
                .as_ref()
                .and_then(|panes| find_pane_context(panes, pane_id))
        });

        let agents = list_agents().await?.agents;
        let harness = self.resolved_harness()?;
        let launch_cmd = self.resolved_launch_cmd()?;
        let agent_name = resolve_spawn_agent_name(harness, self.name.as_deref(), &agents)?;

        let prepared = prepare_launch(
            self,
            &agent_name,
            &agents,
            pane_context
                .as_ref()
                .and_then(|context| context.cwd.clone()),
        )?;

        let metadata = AgentMetadata {
            agent_id: Uuid::new_v4().to_string(),
            name: agent_name.clone(),
            launch_cmd: prepared.launch_cmd.clone(),
            declared_cwd: prepared.command_dir.clone(),
            created_at: Utc::now(),
            repo_root: prepared.repo_root.clone(),
            worktree: prepared.worktree.clone(),
            branch: prepared.branch.clone(),
            managed_checkout: prepared.managed_checkout,
        };

        let spawned = if self.here {
            let pane_id =
                context_pane_id.ok_or_else(|| anyhow::anyhow!("--here requires a pane"))?;
            let pane_context = pane_context
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("unable to resolve current pane context"))?;

            set_agent_metadata(codec::SetAgentMetadata {
                pane_id,
                metadata: metadata.clone(),
            })
            .await?;

            let launch_text = build_in_place_launch_command(
                pane_context.cwd.as_deref(),
                &prepared.command_dir,
                launch_cmd,
                self.replace,
            )?;

            if let Err(err) = send_paste(codec::SendPaste {
                pane_id,
                data: launch_text,
            })
            .await
            {
                let _ = clear_agent_metadata(codec::ClearAgentMetadata { pane_id }).await;
                return Err(err.context("set agent metadata but failed to send launch command"));
            }

            if let Err(err) = key_down(SendKeyDown {
                pane_id,
                event: KeyEvent {
                    key: KeyCode::Enter,
                    modifiers: Modifiers::NONE,
                },
                input_serial: InputSerial::now(),
            })
            .await
            {
                let _ = clear_agent_metadata(codec::ClearAgentMetadata { pane_id }).await;
                return Err(err.context("sent launch command but failed to submit it"));
            }

            if pane_context.tab_title.is_empty() {
                let _ = set_tab_title(TabTitleChanged {
                    tab_id: pane_context.tab_id,
                    title: agent_name.clone(),
                    badge: Default::default(),
                })
                .await;
            }

            codec::SpawnResponse {
                tab_id: pane_context.tab_id,
                pane_id,
                window_id: pane_context.window_id,
                size: pane_context.tab_size,
            }
        } else if self.split {
            let pane_id =
                context_pane_id.ok_or_else(|| anyhow::anyhow!("split requires a pane"))?;
            let tab_size = pane_context
                .as_ref()
                .map(|context| context.tab_size)
                .ok_or_else(|| anyhow::anyhow!("unable to resolve split tab size"))?;
            split_pane(codec::SplitPane {
                pane_id,
                split_request: self.split_request(),
                command: Some(prepared.command.clone()),
                command_dir: Some(prepared.command_dir.clone()),
                domain: SpawnTabDomain::CurrentPaneDomain,
                move_pane_id: None,
                tab_size: Some(tab_size),
            })
            .await?
        } else {
            let window_id = if self.new_window {
                None
            } else {
                Some(
                    pane_context
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("unable to resolve current window"))?
                        .window_id,
                )
            };
            let size = pane_context
                .as_ref()
                .map(|context| context.tab_size)
                .unwrap_or_else(|| config.initial_size(0, None));
            let workspace = self.workspace.clone().unwrap_or_else(|| {
                config
                    .default_workspace
                    .as_deref()
                    .unwrap_or(mux::DEFAULT_WORKSPACE)
                    .to_string()
            });

            spawn_v2(SpawnV2 {
                domain: SpawnTabDomain::DefaultDomain,
                window_id,
                current_pane_id: context_pane_id,
                command: Some(prepared.command.clone()),
                command_dir: Some(prepared.command_dir.clone()),
                size,
                workspace,
            })
            .await?
        };

        if !self.split && !self.here {
            if let Err(err) = set_tab_title(TabTitleChanged {
                tab_id: spawned.tab_id,
                title: agent_name.clone(),
                badge: Default::default(),
            })
            .await
            {
                let _ = kill_pane(codec::KillPane {
                    pane_id: spawned.pane_id,
                })
                .await;
                return Err(err.context("spawned pane but failed to set initial tab title"));
            }
        }

        if !self.here {
            if let Err(err) = set_agent_metadata(codec::SetAgentMetadata {
                pane_id: spawned.pane_id,
                metadata,
            })
            .await
            {
                let _ = kill_pane(codec::KillPane {
                    pane_id: spawned.pane_id,
                })
                .await;
                return Err(err.context("spawned pane but failed to attach agent metadata"));
            }
        }

        reload_spawned_agent_after_startup(
            &mut list_agents_after_set,
            spawned.pane_id,
            &agent_name,
            STARTUP_STABILIZATION_DELAY_MS,
        )
        .await
    }

    fn split_request(&self) -> SplitRequest {
        let direction = if self.left || self.right || self.horizontal {
            SplitDirection::Horizontal
        } else if self.top || self.bottom {
            SplitDirection::Vertical
        } else {
            SplitDirection::Horizontal
        };
        let target_is_second = !(self.left || self.top);
        let size = match (self.cells, self.percent) {
            (Some(cells), _) => SplitSize::Cells(cells),
            (_, Some(percent)) => SplitSize::Percent(percent),
            (None, None) => SplitSize::Percent(50),
        };

        SplitRequest {
            direction,
            target_is_second,
            size,
            top_level: false,
        }
    }

    fn prepare_launch(
        &self,
        agent_name: &str,
        _agents: &[AgentSnapshot],
        current_cwd: Option<String>,
    ) -> anyhow::Result<PreparedAgentLaunch> {
        let _harness = self.resolved_harness()?;
        let launch_cmd = self.resolved_launch_cmd()?;

        let repo_root = self
            .repo
            .as_ref()
            .map(|path| resolve_repo_root(path))
            .transpose()?;
        let worktree_path = match &self.worktree {
            WorktreeMode::None => None,
            WorktreeMode::Auto => {
                let repo_root = repo_root
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("--worktree auto requires --repo"))?;
                Some(auto_worktree_path(repo_root, agent_name))
            }
            WorktreeMode::Path(path) => Some(normalize_path(path)?),
        };

        if self.branch.is_some() && repo_root.is_none() && worktree_path.is_none() {
            bail!("--branch requires --repo or --worktree");
        }

        let mut managed_checkout = false;
        if let Some(worktree_path) = worktree_path.as_ref() {
            let repo_root = repo_root
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("--worktree requires --repo"))?;
            managed_checkout = ensure_worktree(repo_root, worktree_path, self.branch.as_deref())?;
        } else if let (Some(repo_root), Some(branch)) = (repo_root.as_ref(), self.branch.as_deref())
        {
            ensure_branch_checkout(repo_root, branch)?;
        }

        let command_dir = resolve_relative_cwd(self.cwd.clone())?
            .or_else(|| worktree_path.as_ref().map(|path| path_to_string(path)))
            .or_else(|| repo_root.as_ref().map(|path| path_to_string(path)))
            .or(current_cwd)
            .unwrap_or(
                std::env::current_dir()
                    .context("resolving current directory")?
                    .to_string_lossy()
                    .to_string(),
            );

        Ok(PreparedAgentLaunch {
            command: command_builder_from_cmd(launch_cmd)?,
            launch_cmd: launch_cmd.to_string(),
            command_dir,
            repo_root: repo_root.as_ref().map(|path| path_to_string(path)),
            worktree: worktree_path.as_ref().map(|path| path_to_string(path)),
            branch: self.branch.clone(),
            managed_checkout,
        })
    }
}

fn parse_worktree_mode(s: &str) -> anyhow::Result<WorktreeMode> {
    Ok(match s {
        "none" => WorktreeMode::None,
        "auto" => WorktreeMode::Auto,
        path => WorktreeMode::Path(PathBuf::from(path)),
    })
}

fn ensure_agent_name_available(
    agents: &[AgentSnapshot],
    requested_name: &str,
) -> anyhow::Result<()> {
    if let Some(existing) = agents
        .iter()
        .find(|agent| agent.metadata.name == requested_name)
    {
        bail!(
            "agent name {} is already assigned to pane {}",
            requested_name,
            existing.pane_id
        );
    }
    Ok(())
}

fn next_available_agent_name(agents: &[AgentSnapshot], base_name: &str) -> String {
    if !agents.iter().any(|agent| agent.metadata.name == base_name) {
        return base_name.to_string();
    }

    let mut suffix = 2usize;
    loop {
        let candidate = format!("{base_name}{suffix}");
        if !agents.iter().any(|agent| agent.metadata.name == candidate) {
            return candidate;
        }
        suffix += 1;
    }
}

fn resolve_spawn_agent_name(
    harness: AgentHarness,
    requested_name: Option<&str>,
    agents: &[AgentSnapshot],
) -> anyhow::Result<String> {
    if let Some(name) = requested_name {
        ensure_agent_name_available(agents, name)?;
        return Ok(name.to_string());
    }

    let base_name = match harness {
        AgentHarness::Codex => "codex",
        AgentHarness::Claude => "claude",
        AgentHarness::Gemini => "gemini",
        AgentHarness::Opencode => "opencode",
        AgentHarness::Unknown => {
            bail!(
                "agent start requires a recognized harness (currently: claude, codex, gemini, opencode)"
            )
        }
    };

    Ok(next_available_agent_name(agents, base_name))
}

fn build_in_place_launch_command(
    current_cwd: Option<&str>,
    target_cwd: &str,
    cmd: &str,
    replace: bool,
) -> anyhow::Result<String> {
    let launcher = if replace {
        format!("exec {cmd}")
    } else {
        cmd.to_string()
    };

    if current_cwd == Some(target_cwd) {
        return Ok(launcher);
    }

    let quoted_dir =
        shlex::try_quote(target_cwd).map_err(|err| anyhow::anyhow!("invalid cwd: {err}"))?;
    Ok(format!("cd {quoted_dir} && {launcher}"))
}

fn find_pane_context(panes: &ListPanesResponse, pane_id: PaneId) -> Option<PaneContext> {
    for (idx, tabroot) in panes.tabs.iter().enumerate() {
        let Some(root_size) = tabroot.root_size() else {
            continue;
        };
        let mut cursor = tabroot.clone().into_tree().cursor();

        loop {
            if let Some(entry) = cursor.leaf_mut() {
                if entry.pane_id == pane_id {
                    return Some(PaneContext {
                        window_id: entry.window_id,
                        tab_id: entry.tab_id,
                        tab_title: panes.tab_titles.get(idx).cloned().unwrap_or_default(),
                        tab_size: root_size,
                        cwd: pane_working_dir(entry.working_dir.as_ref()),
                    });
                }
            }
            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(_) => break,
            }
        }
    }

    None
}

fn pane_working_dir(working_dir: Option<&mux::tab::SerdeUrl>) -> Option<String> {
    let url = &working_dir?.url;
    if url.scheme() == "file" {
        return url
            .to_file_path()
            .ok()
            .map(|path| path.to_string_lossy().to_string());
    }
    Some(url.as_str().to_string())
}

fn resolve_repo_root(path: &Path) -> anyhow::Result<PathBuf> {
    let path = normalize_path(path)?;
    let git_dir = if path.is_file() {
        path.parent()
            .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", path.display()))?
            .to_path_buf()
    } else {
        path
    };

    let mut cmd = ProcessCommand::new("git");
    cmd.arg("-C")
        .arg(&git_dir)
        .args(["rev-parse", "--show-toplevel"]);
    let stdout = capture_command_output(&mut cmd, "resolving git repository root")?;
    normalize_path(Path::new(stdout.trim()))
}

fn auto_worktree_path(repo_root: &Path, name: &str) -> PathBuf {
    let repo_parent = repo_root.parent().unwrap_or(repo_root);
    let repo_name = repo_root
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_else(|| OsString::from("repo"));
    repo_parent
        .join(".wakterm-agents")
        .join(repo_name)
        .join(name)
}

fn normalize_path(path: &Path) -> anyhow::Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("resolving current directory")?
            .join(path)
    };

    if path.exists() {
        path.canonicalize()
            .with_context(|| format!("canonicalizing {}", path.display()))
    } else {
        Ok(path)
    }
}

fn ensure_worktree(
    repo_root: &Path,
    worktree_path: &Path,
    branch: Option<&str>,
) -> anyhow::Result<bool> {
    let repo_root = normalize_path(repo_root)?;
    let worktree_path = normalize_path(worktree_path)?;

    let registered = git_worktree_exists(&repo_root, &worktree_path)?;
    if worktree_path.exists() {
        anyhow::ensure!(
            registered,
            "worktree path {} exists but is not registered in {}",
            worktree_path.display(),
            repo_root.display()
        );
        if let Some(branch) = branch {
            ensure_branch_checkout(&worktree_path, branch)?;
        }
        return Ok(false);
    }

    let parent = worktree_path.parent().ok_or_else(|| {
        anyhow::anyhow!(
            "worktree path {} has no parent directory",
            worktree_path.display()
        )
    })?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;

    let mut cmd = ProcessCommand::new("git");
    cmd.arg("-C").arg(&repo_root).arg("worktree").arg("add");
    if let Some(branch) = branch {
        if git_local_branch_exists(&repo_root, branch)? {
            cmd.arg(&worktree_path).arg(branch);
        } else {
            cmd.arg("-b").arg(branch).arg(&worktree_path);
        }
    } else {
        cmd.arg("--detach").arg(&worktree_path);
    }
    capture_command_output(&mut cmd, "creating git worktree")?;

    anyhow::ensure!(
        git_worktree_exists(&repo_root, &worktree_path)?,
        "git created {} but did not register it as a worktree",
        worktree_path.display()
    );
    Ok(true)
}

fn ensure_branch_checkout(repo_or_worktree: &Path, branch: &str) -> anyhow::Result<()> {
    let repo_or_worktree = normalize_path(repo_or_worktree)?;
    let branch_exists = git_local_branch_exists(&repo_or_worktree, branch)?;

    let mut cmd = ProcessCommand::new("git");
    cmd.arg("-C").arg(&repo_or_worktree).arg("checkout");
    if branch_exists {
        cmd.arg(branch);
    } else {
        cmd.arg("-b").arg(branch);
    }
    capture_command_output(&mut cmd, "checking out git branch")?;
    Ok(())
}

fn git_local_branch_exists(repo_or_worktree: &Path, branch: &str) -> anyhow::Result<bool> {
    let status = ProcessCommand::new("git")
        .arg("-C")
        .arg(repo_or_worktree)
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("refs/heads/{branch}"))
        .status()
        .with_context(|| format!("checking whether git branch {branch} exists"))?;
    Ok(status.success())
}

fn git_worktree_exists(repo_root: &Path, worktree_path: &Path) -> anyhow::Result<bool> {
    let mut cmd = ProcessCommand::new("git");
    cmd.arg("-C")
        .arg(repo_root)
        .args(["worktree", "list", "--porcelain"]);
    let stdout = capture_command_output(&mut cmd, "listing git worktrees")?;
    let requested = normalize_path(worktree_path)?;

    for line in stdout.lines() {
        let Some(path) = line.strip_prefix("worktree ") else {
            continue;
        };
        if normalize_path(Path::new(path))? == requested {
            return Ok(true);
        }
    }

    Ok(false)
}

fn capture_command_output(cmd: &mut ProcessCommand, description: &str) -> anyhow::Result<String> {
    let output = cmd
        .output()
        .with_context(|| format!("running {description}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let detail = if !stderr.is_empty() { stderr } else { stdout };
        bail!("{description} failed: {detail}");
    }

    String::from_utf8(output.stdout)
        .map(|stdout| stdout.trim().to_string())
        .context("command output was not valid utf-8")
}

fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

fn command_builder_from_cmd(cmd: &str) -> anyhow::Result<CommandBuilder> {
    let argv = shell_words::split(cmd).context("parsing --cmd")?;
    anyhow::ensure!(!argv.is_empty(), "--cmd must not be empty");
    Ok(CommandBuilder::from_argv(
        argv.into_iter().map(OsString::from).collect(),
    ))
}

#[derive(Debug, Parser, Clone, Copy)]
pub struct ListAgentsCommand {
    /// Controls the output format.
    /// "table" and "json" are possible formats.
    #[arg(long = "format", default_value = "table")]
    format: CliOutputFormatKind,

    /// Stream latest observer-backed harness message updates instead of printing a snapshot.
    #[arg(short = 'f', long)]
    follow: bool,

    /// Poll interval for follow/watch mode.
    #[arg(long, default_value_t = 500, requires = "follow")]
    poll_ms: u64,
}

impl ListAgentsCommand {
    async fn run(&self, client: Client) -> anyhow::Result<()> {
        if self.follow {
            return WatchAgentsCommand {
                format: self.format,
                poll_ms: self.poll_ms,
            }
            .run(client)
            .await;
        }

        let agents = client.list_agents().await?.agents;

        match self.format {
            CliOutputFormatKind::Json => write_json(&agents),
            CliOutputFormatKind::Table => {
                let cols = vec![
                    Column {
                        name: "NAME".to_string(),
                        alignment: Alignment::Left,
                    },
                    Column {
                        name: "PANEID".to_string(),
                        alignment: Alignment::Right,
                    },
                    Column {
                        name: "TABID".to_string(),
                        alignment: Alignment::Right,
                    },
                    Column {
                        name: "WINID".to_string(),
                        alignment: Alignment::Right,
                    },
                    Column {
                        name: "WORKSPACE".to_string(),
                        alignment: Alignment::Left,
                    },
                    Column {
                        name: "STATUS".to_string(),
                        alignment: Alignment::Left,
                    },
                    Column {
                        name: "TURN".to_string(),
                        alignment: Alignment::Left,
                    },
                    Column {
                        name: "HARNESS".to_string(),
                        alignment: Alignment::Left,
                    },
                    Column {
                        name: "TRANSPORT".to_string(),
                        alignment: Alignment::Left,
                    },
                    Column {
                        name: "CWD".to_string(),
                        alignment: Alignment::Left,
                    },
                    Column {
                        name: "PROGRESS".to_string(),
                        alignment: Alignment::Left,
                    },
                    Column {
                        name: "CMD".to_string(),
                        alignment: Alignment::Left,
                    },
                ];
                let data = agents
                    .iter()
                    .map(|agent| {
                        vec![
                            agent.metadata.name.clone(),
                            agent.pane_id.to_string(),
                            agent.tab_id.to_string(),
                            agent.window_id.to_string(),
                            agent.workspace.clone(),
                            runtime_status_label(&agent.runtime.status),
                            turn_state_label(&agent.runtime.turn_state),
                            harness_label(&agent.runtime.harness),
                            transport_label(&agent.runtime.transport),
                            agent.metadata.declared_cwd.clone(),
                            inline_progress_summary(agent),
                            agent.metadata.launch_cmd.clone(),
                        ]
                    })
                    .collect::<Vec<_>>();
                tabulate_output(&cols, &data, &mut std::io::stdout().lock())?;
                Ok(())
            }
        }
    }
}

#[derive(Debug, Parser, Clone, Copy)]
pub struct WatchAgentsCommand {
    /// Controls the output format.
    /// "table" streams tab-separated lines, while "json" streams JSON lines.
    #[arg(long = "format", default_value = "table")]
    format: CliOutputFormatKind,

    /// Poll interval while streaming updates.
    #[arg(long, default_value_t = 500)]
    poll_ms: u64,
}

impl WatchAgentsCommand {
    async fn run(&self, client: Client) -> anyhow::Result<()> {
        let mut out = std::io::stdout().lock();
        self.run_with(|| client.list_agents(), &mut out, None).await
    }

    async fn run_with<ListAgents, ListAgentsFut, W: Write>(
        &self,
        mut list_agents: ListAgents,
        out: &mut W,
        max_polls: Option<usize>,
    ) -> anyhow::Result<()>
    where
        ListAgents: FnMut() -> ListAgentsFut,
        ListAgentsFut: Future<Output = anyhow::Result<codec::ListAgentsResponse>>,
    {
        let mut seen = HashMap::new();
        let mut remaining_polls = max_polls;

        loop {
            let agents = list_agents().await?.agents;
            let events = collect_agent_watch_events(&mut seen, &agents);
            self.write_events(out, &events)?;
            out.flush()?;

            if let Some(remaining) = remaining_polls.as_mut() {
                if *remaining <= 1 {
                    return Ok(());
                }
                *remaining -= 1;
            }

            smol::Timer::after(Duration::from_millis(self.poll_ms)).await;
        }
    }

    fn write_events<W: Write>(
        &self,
        out: &mut W,
        events: &[AgentWatchEvent],
    ) -> anyhow::Result<()> {
        for event in events {
            match self.format {
                CliOutputFormatKind::Json => {
                    serde_json::to_writer(&mut *out, event)?;
                    writeln!(out)?;
                }
                CliOutputFormatKind::Table => {
                    writeln!(out, "{}\t{}\t{}", event.name, event.harness, event.message)?;
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Parser, Clone)]
pub struct InspectAgentCommand {
    /// Agent name or stable id
    target: String,
}

impl InspectAgentCommand {
    async fn run(&self, client: Client) -> anyhow::Result<()> {
        let agents = client.list_agents().await?.agents;
        let agent = find_agent(&agents, &self.target)
            .cloned()
            .with_context(|| format!("no agent named or identified by {}", self.target))?;
        write_json(&agent)
    }
}

#[derive(Debug, Parser, Clone)]
pub struct SendAgentCommand {
    /// Agent name or stable id
    target: String,

    /// Send the text directly, rather than as a bracketed paste
    #[arg(long)]
    no_paste: bool,

    /// Do not press Enter after sending the text
    #[arg(long)]
    no_submit: bool,

    /// Maximum time to wait for observer-backed acknowledgement
    #[arg(long, default_value_t = 2000)]
    ack_timeout_ms: u64,

    /// Poll interval while waiting for acknowledgement
    #[arg(long, default_value_t = 50)]
    ack_poll_ms: u64,

    /// The text to send. If omitted, reads from stdin
    text: Option<String>,
}

impl SendAgentCommand {
    async fn run(&self, client: Client) -> anyhow::Result<()> {
        let result = self
            .run_with(
                || client.list_agents(),
                |request| client.write_to_pane(request),
                |request| client.send_paste(request),
            )
            .await?;
        write_json(&result)
    }

    async fn run_with<
        ListAgents,
        ListAgentsFut,
        WriteToPaneFn,
        WriteToPaneFut,
        SendPasteFn,
        SendPasteFut,
    >(
        &self,
        mut list_agents: ListAgents,
        write_to_pane: WriteToPaneFn,
        send_paste: SendPasteFn,
    ) -> anyhow::Result<AgentSendResult>
    where
        ListAgents: FnMut() -> ListAgentsFut,
        ListAgentsFut: Future<Output = anyhow::Result<codec::ListAgentsResponse>>,
        WriteToPaneFn: Fn(codec::WriteToPane) -> WriteToPaneFut,
        WriteToPaneFut: Future<Output = anyhow::Result<codec::UnitResponse>>,
        SendPasteFn: Fn(codec::SendPaste) -> SendPasteFut,
        SendPasteFut: Future<Output = anyhow::Result<codec::UnitResponse>>,
    {
        let agents = list_agents().await?.agents;
        let agent = find_agent(&agents, &self.target)
            .cloned()
            .with_context(|| format!("no agent named or identified by {}", self.target))?;
        let text = self.read_text()?;
        let baseline = AgentAckBaseline::from_agent(&agent);
        let use_raw_write = self.no_paste || prefers_raw_input(&agent.runtime.harness);

        if use_raw_write {
            write_to_pane(codec::WriteToPane {
                pane_id: agent.pane_id,
                data: text.into_bytes(),
            })
            .await?;
        } else {
            send_paste(codec::SendPaste {
                pane_id: agent.pane_id,
                data: text,
            })
            .await?;
        }

        let submitted = !self.no_submit;
        if submitted {
            submit_native_harness_prompt(agent.pane_id, &write_to_pane).await?;
        }

        let mut acknowledgement = self
            .wait_for_acknowledgement(&mut list_agents, &agent, &baseline)
            .await?;
        if submitted && should_retry_submit(&agent, &baseline, &acknowledgement) {
            submit_native_harness_prompt(agent.pane_id, &write_to_pane).await?;
            acknowledgement = self
                .wait_for_acknowledgement(&mut list_agents, &agent, &baseline)
                .await?;
        }

        Ok(AgentSendResult {
            agent_id: agent.metadata.agent_id.clone(),
            agent_name: agent.metadata.name.clone(),
            pane_id: agent.pane_id,
            transport: agent.runtime.transport,
            submitted,
            acknowledgement,
        })
    }

    fn read_text(&self) -> anyhow::Result<String> {
        match &self.text {
            Some(text) => Ok(text.clone()),
            None => {
                let mut text = String::new();
                std::io::stdin()
                    .read_to_string(&mut text)
                    .context("reading stdin")?;
                Ok(text)
            }
        }
    }

    async fn wait_for_acknowledgement<ListAgents, ListAgentsFut>(
        &self,
        list_agents: &mut ListAgents,
        baseline_agent: &AgentSnapshot,
        baseline: &AgentAckBaseline,
    ) -> anyhow::Result<AgentSendAcknowledgement>
    where
        ListAgents: FnMut() -> ListAgentsFut,
        ListAgentsFut: Future<Output = anyhow::Result<codec::ListAgentsResponse>>,
    {
        if self.no_submit {
            return Ok(AgentSendAcknowledgement {
                kind: AgentAckKind::NotRequested,
                acknowledged: false,
                latency_ms: None,
                session_path: baseline.session_path.clone(),
                detail: Some("submit skipped by --no-submit".to_string()),
            });
        }

        if !supports_observer_ack(&baseline_agent.runtime.harness) {
            return Ok(AgentSendAcknowledgement {
                kind: AgentAckKind::Unavailable,
                acknowledged: false,
                latency_ms: None,
                session_path: baseline.session_path.clone(),
                detail: Some(
                    "agent has no supported observer-backed acknowledgement path".to_string(),
                ),
            });
        }

        wait_for_observer_acknowledgement(
            list_agents,
            baseline_agent,
            baseline,
            self.ack_timeout_ms,
            self.ack_poll_ms,
        )
        .await
    }
}

fn prefers_raw_input(harness: &AgentHarness) -> bool {
    matches!(harness, AgentHarness::Gemini)
}

async fn submit_native_harness_prompt<WriteToPaneFn, WriteToPaneFut>(
    pane_id: PaneId,
    write_to_pane: &WriteToPaneFn,
) -> anyhow::Result<()>
where
    WriteToPaneFn: Fn(codec::WriteToPane) -> WriteToPaneFut,
    WriteToPaneFut: Future<Output = anyhow::Result<codec::UnitResponse>>,
{
    // Native harnesses reliably accept a raw carriage return after the prompt
    // text; synthetic Enter key events were leaving Claude and Gemini prompts
    // unsubmitted.
    std::thread::sleep(Duration::from_millis(200));
    write_to_pane(codec::WriteToPane {
        pane_id,
        data: b"\r".to_vec(),
    })
    .await?;
    Ok(())
}

#[derive(Debug, Parser, Clone)]
pub struct InterruptAgentCommand {
    /// Agent name or stable id
    target: String,

    /// Maximum time to wait for observer-backed acknowledgement
    #[arg(long, default_value_t = 2000)]
    ack_timeout_ms: u64,

    /// Poll interval while waiting for acknowledgement
    #[arg(long, default_value_t = 50)]
    ack_poll_ms: u64,
}

impl InterruptAgentCommand {
    async fn run(&self, client: Client) -> anyhow::Result<()> {
        let result = self
            .run_with(|| client.list_agents(), |request| client.key_down(request))
            .await?;
        write_json(&result)
    }

    async fn run_with<ListAgents, ListAgentsFut, KeyDownFn, KeyDownFut>(
        &self,
        mut list_agents: ListAgents,
        key_down: KeyDownFn,
    ) -> anyhow::Result<AgentInterruptResult>
    where
        ListAgents: FnMut() -> ListAgentsFut,
        ListAgentsFut: Future<Output = anyhow::Result<codec::ListAgentsResponse>>,
        KeyDownFn: Fn(SendKeyDown) -> KeyDownFut,
        KeyDownFut: Future<Output = anyhow::Result<codec::UnitResponse>>,
    {
        let agents = list_agents().await?.agents;
        let agent = find_agent(&agents, &self.target)
            .cloned()
            .with_context(|| format!("no agent named or identified by {}", self.target))?;

        if agent.runtime.harness != AgentHarness::Codex {
            bail!(
                "agent {} uses {}; interrupt is currently only implemented for codex panes",
                agent.metadata.name,
                harness_label(&agent.runtime.harness)
            );
        }

        let baseline = AgentAckBaseline::from_agent(&agent);
        key_down(SendKeyDown {
            pane_id: agent.pane_id,
            event: KeyEvent {
                key: KeyCode::Char('C'),
                modifiers: Modifiers::CTRL,
            },
            input_serial: InputSerial::now(),
        })
        .await?;

        let acknowledgement = if !matches!(agent.runtime.transport, AgentTransport::ObservedPty) {
            AgentSendAcknowledgement {
                kind: AgentAckKind::Unavailable,
                acknowledged: false,
                latency_ms: None,
                session_path: baseline.session_path.clone(),
                detail: Some("agent has no observer-backed session path".to_string()),
            }
        } else {
            wait_for_observer_acknowledgement(
                &mut list_agents,
                &agent,
                &baseline,
                self.ack_timeout_ms,
                self.ack_poll_ms,
            )
            .await?
        };

        Ok(AgentInterruptResult {
            agent_id: agent.metadata.agent_id.clone(),
            agent_name: agent.metadata.name.clone(),
            pane_id: agent.pane_id,
            harness: agent.runtime.harness,
            acknowledgement,
        })
    }
}

#[derive(Debug, Parser, Clone)]
pub struct AdoptAgentCommand {
    /// Specify the target pane. Defaults to WAKTERM_PANE.
    #[arg(long)]
    pane_id: Option<PaneId>,

    /// Stable human-readable name for this agent
    #[arg(long)]
    name: String,

    /// Launch command to use for restart and restore
    #[arg(long)]
    cmd: String,

    /// Override the declared checkout/cwd for this agent
    #[arg(long)]
    cwd: Option<String>,

    #[arg(long)]
    repo_root: Option<String>,

    #[arg(long)]
    worktree: Option<String>,

    #[arg(long)]
    branch: Option<String>,
}

impl AdoptAgentCommand {
    async fn run(&self, client: Client) -> anyhow::Result<()> {
        self.run_with(
            || client.list_agents(),
            || client.list_panes(),
            || client.list_agents(),
            |pane_id| client.resolve_pane_id(pane_id),
            |request| client.set_agent_metadata(request),
        )
        .await
    }

    async fn run_with<
        ListAgents,
        ListAgentsFut,
        ListPanes,
        ListPanesFut,
        ListAgentsAfterSet,
        ListAgentsAfterSetFut,
        ResolvePaneId,
        ResolvePaneIdFut,
        SetAgentMetadataFn,
        SetAgentMetadataFut,
    >(
        &self,
        list_agents: ListAgents,
        list_panes: ListPanes,
        list_agents_after_set: ListAgentsAfterSet,
        resolve_pane_id: ResolvePaneId,
        set_agent_metadata: SetAgentMetadataFn,
    ) -> anyhow::Result<()>
    where
        ListAgents: FnOnce() -> ListAgentsFut,
        ListAgentsFut: Future<Output = anyhow::Result<codec::ListAgentsResponse>>,
        ListPanes: FnOnce() -> ListPanesFut,
        ListPanesFut: Future<Output = anyhow::Result<ListPanesResponse>>,
        ListAgentsAfterSet: FnOnce() -> ListAgentsAfterSetFut,
        ListAgentsAfterSetFut: Future<Output = anyhow::Result<codec::ListAgentsResponse>>,
        ResolvePaneId: FnOnce(Option<PaneId>) -> ResolvePaneIdFut,
        ResolvePaneIdFut: Future<Output = anyhow::Result<PaneId>>,
        SetAgentMetadataFn: FnOnce(codec::SetAgentMetadata) -> SetAgentMetadataFut,
        SetAgentMetadataFut: Future<Output = anyhow::Result<codec::UnitResponse>>,
    {
        let pane_id = resolve_pane_id(self.pane_id).await?;
        let agents = list_agents().await?.agents;
        let existing = agents.iter().find(|agent| agent.pane_id == pane_id);
        if let Some(existing) = agents
            .iter()
            .find(|agent| agent.metadata.name == self.name && agent.pane_id != pane_id)
        {
            bail!(
                "agent name {} is already assigned to pane {}",
                self.name,
                existing.pane_id
            );
        }
        let panes = list_panes().await?;

        let metadata = build_agent_metadata(
            pane_id,
            existing,
            &panes,
            &self.name,
            Some(self.cmd.as_str()),
            self.cwd.clone(),
            self.repo_root.clone(),
            self.worktree.clone(),
            self.branch.clone(),
            Some(false),
        )?;

        set_agent_metadata(codec::SetAgentMetadata { pane_id, metadata }).await?;

        let updated = list_agents_after_set()
            .await?
            .agents
            .into_iter()
            .find(|agent| agent.pane_id == pane_id)
            .ok_or_else(|| anyhow::anyhow!("agent metadata was set but could not be reloaded"))?;

        write_json(&updated)
    }
}

#[derive(Debug, Parser, Clone)]
pub struct SetAgentCommand {
    /// Specify the target pane. Defaults to WAKTERM_PANE.
    #[arg(long)]
    pane_id: Option<PaneId>,

    /// Stable human-readable name for this agent
    #[arg(long)]
    name: String,

    /// Launch command used to recreate this agent on restore
    #[arg(long)]
    launch_cmd: Option<String>,

    /// Override the declared launch cwd
    #[arg(long)]
    cwd: Option<String>,

    #[arg(long)]
    repo_root: Option<String>,

    #[arg(long)]
    worktree: Option<String>,

    #[arg(long)]
    branch: Option<String>,

    /// Mark the checkout as being provisioned by wakterm
    #[arg(long, conflicts_with = "unmanaged_checkout")]
    managed_checkout: bool,

    /// Mark the checkout as not being provisioned by wakterm
    #[arg(long, conflicts_with = "managed_checkout")]
    unmanaged_checkout: bool,
}

impl SetAgentCommand {
    async fn run(&self, client: Client) -> anyhow::Result<()> {
        self.run_with(
            || client.list_agents(),
            || client.list_panes(),
            || client.list_agents(),
            |pane_id| client.resolve_pane_id(pane_id),
            |request| client.set_agent_metadata(request),
        )
        .await
    }

    async fn run_with<
        ListAgents,
        ListAgentsFut,
        ListPanes,
        ListPanesFut,
        ListAgentsAfterSet,
        ListAgentsAfterSetFut,
        ResolvePaneId,
        ResolvePaneIdFut,
        SetAgentMetadata,
        SetAgentMetadataFut,
    >(
        &self,
        list_agents: ListAgents,
        list_panes: ListPanes,
        list_agents_after_set: ListAgentsAfterSet,
        resolve_pane_id: ResolvePaneId,
        set_agent_metadata: SetAgentMetadata,
    ) -> anyhow::Result<()>
    where
        ListAgents: FnOnce() -> ListAgentsFut,
        ListAgentsFut: std::future::Future<Output = anyhow::Result<codec::ListAgentsResponse>>,
        ListPanes: FnOnce() -> ListPanesFut,
        ListPanesFut: std::future::Future<Output = anyhow::Result<ListPanesResponse>>,
        ListAgentsAfterSet: FnOnce() -> ListAgentsAfterSetFut,
        ListAgentsAfterSetFut:
            std::future::Future<Output = anyhow::Result<codec::ListAgentsResponse>>,
        ResolvePaneId: FnOnce(Option<PaneId>) -> ResolvePaneIdFut,
        ResolvePaneIdFut: std::future::Future<Output = anyhow::Result<PaneId>>,
        SetAgentMetadata: FnOnce(codec::SetAgentMetadata) -> SetAgentMetadataFut,
        SetAgentMetadataFut: std::future::Future<Output = anyhow::Result<codec::UnitResponse>>,
    {
        let pane_id = resolve_pane_id(self.pane_id).await?;
        let agents = list_agents().await?.agents;
        let existing = agents.iter().find(|agent| agent.pane_id == pane_id);
        let panes = list_panes().await?;

        let managed_checkout = if self.managed_checkout {
            Some(true)
        } else if self.unmanaged_checkout {
            Some(false)
        } else {
            None
        };
        let metadata = build_agent_metadata(
            pane_id,
            existing,
            &panes,
            &self.name,
            self.launch_cmd.as_deref(),
            self.cwd.clone(),
            self.repo_root.clone(),
            self.worktree.clone(),
            self.branch.clone(),
            managed_checkout,
        )?;

        set_agent_metadata(codec::SetAgentMetadata { pane_id, metadata }).await?;

        let updated = list_agents_after_set()
            .await?
            .agents
            .into_iter()
            .find(|agent| agent.pane_id == pane_id)
            .ok_or_else(|| anyhow::anyhow!("agent metadata was set but could not be reloaded"))?;

        write_json(&updated)
    }
}

#[derive(Debug, Parser, Clone)]
pub struct ClearAgentCommand {
    /// Specify the target pane. Defaults to WAKTERM_PANE.
    #[arg(long)]
    pane_id: Option<PaneId>,
}

impl ClearAgentCommand {
    async fn run(&self, client: Client) -> anyhow::Result<()> {
        let pane_id = client.resolve_pane_id(self.pane_id).await?;
        client
            .clear_agent_metadata(codec::ClearAgentMetadata { pane_id })
            .await?;
        write_json(&ClearAgentResult {
            pane_id,
            cleared: true,
        })
    }
}

#[derive(Serialize)]
struct ClearAgentResult {
    pane_id: PaneId,
    cleared: bool,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum AgentAckKind {
    SessionObserver,
    TimedOut,
    Unavailable,
    NotRequested,
}

#[derive(Debug, Serialize)]
struct AgentSendAcknowledgement {
    kind: AgentAckKind,
    acknowledged: bool,
    latency_ms: Option<u64>,
    session_path: Option<String>,
    detail: Option<String>,
}

#[derive(Debug, Serialize)]
struct AgentSendResult {
    agent_id: String,
    agent_name: String,
    pane_id: PaneId,
    transport: AgentTransport,
    submitted: bool,
    acknowledgement: AgentSendAcknowledgement,
}

#[derive(Debug, Serialize)]
struct AgentInterruptResult {
    agent_id: String,
    agent_name: String,
    pane_id: PaneId,
    harness: AgentHarness,
    acknowledgement: AgentSendAcknowledgement,
}

#[derive(Debug, Clone)]
struct AgentAckBaseline {
    session_path: Option<String>,
    last_progress_at: Option<chrono::DateTime<Utc>>,
    message: String,
}

impl AgentAckBaseline {
    fn from_agent(agent: &AgentSnapshot) -> Self {
        Self {
            session_path: agent.runtime.session_path.clone(),
            last_progress_at: agent.runtime.last_progress_at,
            message: inline_progress_summary(agent),
        }
    }

    fn is_acknowledged_by(&self, agent: &AgentSnapshot) -> bool {
        if agent.runtime.session_path != self.session_path && agent.runtime.session_path.is_some() {
            return true;
        }

        if agent.runtime.last_progress_at > self.last_progress_at {
            return true;
        }

        let current_message = inline_progress_summary(agent);
        !current_message.is_empty() && current_message != self.message
    }
}

fn supports_observer_ack(harness: &AgentHarness) -> bool {
    !matches!(harness, AgentHarness::Unknown)
}

fn should_retry_submit(
    agent: &AgentSnapshot,
    baseline: &AgentAckBaseline,
    acknowledgement: &AgentSendAcknowledgement,
) -> bool {
    acknowledgement.kind == AgentAckKind::TimedOut
        && !acknowledgement.acknowledged
        && supports_observer_ack(&agent.runtime.harness)
        && !matches!(agent.runtime.transport, AgentTransport::ObservedPty)
        && baseline.session_path.is_none()
}

async fn wait_for_observer_acknowledgement<ListAgents, ListAgentsFut>(
    list_agents: &mut ListAgents,
    baseline_agent: &AgentSnapshot,
    baseline: &AgentAckBaseline,
    ack_timeout_ms: u64,
    ack_poll_ms: u64,
) -> anyhow::Result<AgentSendAcknowledgement>
where
    ListAgents: FnMut() -> ListAgentsFut,
    ListAgentsFut: Future<Output = anyhow::Result<codec::ListAgentsResponse>>,
{
    let started = Instant::now();
    let timeout = Duration::from_millis(ack_timeout_ms);
    let poll = Duration::from_millis(ack_poll_ms);

    loop {
        let agent = list_agents()
            .await?
            .agents
            .into_iter()
            .find(|agent| agent.metadata.agent_id == baseline_agent.metadata.agent_id)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "agent {} disappeared while waiting for acknowledgement",
                    baseline_agent.metadata.name
                )
            })?;

        if baseline.is_acknowledged_by(&agent) {
            return Ok(AgentSendAcknowledgement {
                kind: AgentAckKind::SessionObserver,
                acknowledged: true,
                latency_ms: Some(started.elapsed().as_millis() as u64),
                session_path: agent.runtime.session_path.clone(),
                detail: inline_progress_detail(&agent),
            });
        }

        if started.elapsed() >= timeout {
            return Ok(AgentSendAcknowledgement {
                kind: AgentAckKind::TimedOut,
                acknowledged: false,
                latency_ms: Some(started.elapsed().as_millis() as u64),
                session_path: agent.runtime.session_path.clone(),
                detail: inline_progress_detail(&agent)
                    .or_else(|| observer_timeout_detail(&agent, baseline)),
            });
        }

        smol::Timer::after(poll).await;
    }
}

fn observer_timeout_detail(agent: &AgentSnapshot, baseline: &AgentAckBaseline) -> Option<String> {
    if let Some(detail) = pending_observer_detail(&agent.metadata, &agent.runtime) {
        return Some(detail);
    }

    if baseline.session_path.is_none() && agent.runtime.session_path.is_none() {
        return Some("observer session did not appear before timeout".to_string());
    }

    if agent.runtime.session_path == baseline.session_path {
        return Some("observer session did not advance before timeout".to_string());
    }

    None
}

fn build_agent_metadata(
    pane_id: PaneId,
    existing: Option<&AgentSnapshot>,
    panes: &ListPanesResponse,
    name: &str,
    launch_cmd: Option<&str>,
    cwd: Option<String>,
    repo_root: Option<String>,
    worktree: Option<String>,
    branch: Option<String>,
    managed_checkout: Option<bool>,
) -> anyhow::Result<AgentMetadata> {
    Ok(AgentMetadata {
        agent_id: existing
            .map(|agent| agent.metadata.agent_id.clone())
            .unwrap_or_else(|| Uuid::new_v4().to_string()),
        name: name.to_string(),
        launch_cmd: launch_cmd
            .map(str::to_string)
            .or_else(|| existing.map(|agent| agent.metadata.launch_cmd.clone()))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "--launch-cmd/--cmd is required when tagging a pane for the first time"
                )
            })?,
        declared_cwd: cwd
            .or_else(|| existing.map(|agent| agent.metadata.declared_cwd.clone()))
            .or_else(|| find_pane_cwd(panes, pane_id))
            .ok_or_else(|| anyhow::anyhow!("unable to determine cwd; pass --cwd"))?,
        created_at: existing
            .map(|agent| agent.metadata.created_at)
            .unwrap_or_else(Utc::now),
        repo_root: repo_root
            .or_else(|| existing.and_then(|agent| agent.metadata.repo_root.clone())),
        worktree: worktree.or_else(|| existing.and_then(|agent| agent.metadata.worktree.clone())),
        branch: branch.or_else(|| existing.and_then(|agent| agent.metadata.branch.clone())),
        managed_checkout: managed_checkout
            .or_else(|| existing.map(|agent| agent.metadata.managed_checkout))
            .unwrap_or(false),
    })
}

fn write_json<T: Serialize>(value: &T) -> anyhow::Result<()> {
    serde_json::to_writer_pretty(std::io::stdout().lock(), value)?;
    println!();
    Ok(())
}

fn find_agent<'a>(agents: &'a [AgentSnapshot], target: &str) -> Option<&'a AgentSnapshot> {
    agents
        .iter()
        .find(|agent| agent.metadata.name == target || agent.metadata.agent_id == target)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentWatchFingerprint {
    transport: String,
    status: String,
    turn_state: String,
    harness_mode: Option<String>,
    turn_phase: Option<String>,
    attention_reason: Option<String>,
    session_path: Option<String>,
    last_progress_at: Option<chrono::DateTime<Utc>>,
    message: String,
}

impl AgentWatchFingerprint {
    fn from_agent(agent: &AgentSnapshot) -> Self {
        Self {
            transport: transport_label(&agent.runtime.transport),
            status: runtime_status_label(&agent.runtime.status),
            turn_state: turn_state_label(&agent.runtime.turn_state),
            harness_mode: agent.runtime.harness_mode.clone(),
            turn_phase: agent.runtime.turn_phase.clone(),
            attention_reason: agent.runtime.attention_reason.clone(),
            session_path: agent.runtime.session_path.clone(),
            last_progress_at: agent.runtime.last_progress_at,
            message: watch_event_message(agent),
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct AgentWatchEvent {
    agent_id: String,
    name: String,
    pane_id: PaneId,
    tab_id: mux::tab::TabId,
    window_id: WindowId,
    workspace: String,
    harness: String,
    transport: String,
    status: String,
    turn_state: String,
    harness_mode: Option<String>,
    turn_phase: Option<String>,
    attention_reason: Option<String>,
    observer_hint: Option<String>,
    session_path: Option<String>,
    last_progress_at: Option<chrono::DateTime<Utc>>,
    message: String,
}

impl AgentWatchEvent {
    fn from_agent(agent: &AgentSnapshot) -> Option<Self> {
        let observer_hint = pending_observer_detail(&agent.metadata, &agent.runtime);
        let message = watch_event_message(agent);
        if message.is_empty() {
            return None;
        }

        Some(Self {
            agent_id: agent.metadata.agent_id.clone(),
            name: agent.metadata.name.clone(),
            pane_id: agent.pane_id,
            tab_id: agent.tab_id,
            window_id: agent.window_id,
            workspace: agent.workspace.clone(),
            harness: harness_label(&agent.runtime.harness),
            transport: transport_label(&agent.runtime.transport),
            status: runtime_status_label(&agent.runtime.status),
            turn_state: turn_state_label(&agent.runtime.turn_state),
            harness_mode: agent.runtime.harness_mode.clone(),
            turn_phase: agent.runtime.turn_phase.clone(),
            attention_reason: agent.runtime.attention_reason.clone(),
            observer_hint,
            session_path: agent.runtime.session_path.clone(),
            last_progress_at: agent.runtime.last_progress_at,
            message,
        })
    }
}

fn collect_agent_watch_events(
    seen: &mut HashMap<String, AgentWatchFingerprint>,
    agents: &[AgentSnapshot],
) -> Vec<AgentWatchEvent> {
    let mut sorted_agents = agents.iter().collect::<Vec<_>>();
    sorted_agents.sort_by(|left, right| {
        left.metadata
            .name
            .cmp(&right.metadata.name)
            .then(left.pane_id.cmp(&right.pane_id))
    });

    let mut current_ids = HashSet::new();
    let mut events = vec![];

    for agent in sorted_agents {
        current_ids.insert(agent.metadata.agent_id.clone());

        let fingerprint = AgentWatchFingerprint::from_agent(agent);
        if fingerprint.message.is_empty() {
            seen.remove(&agent.metadata.agent_id);
            continue;
        }

        let changed = seen
            .get(&agent.metadata.agent_id)
            .map(|existing| existing != &fingerprint)
            .unwrap_or(true);
        if changed {
            if let Some(event) = AgentWatchEvent::from_agent(agent) {
                events.push(event);
            }
        }

        seen.insert(agent.metadata.agent_id.clone(), fingerprint);
    }

    seen.retain(|agent_id, _| current_ids.contains(agent_id));
    events
}

fn find_pane_cwd(panes: &ListPanesResponse, pane_id: PaneId) -> Option<String> {
    for tabroot in &panes.tabs {
        let mut cursor = tabroot.clone().into_tree().cursor();

        loop {
            if let Some(entry) = cursor.leaf_mut() {
                if entry.pane_id == pane_id {
                    return pane_working_dir(entry.working_dir.as_ref());
                }
            }
            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(_) => break,
            }
        }
    }

    None
}

fn runtime_status_label(status: &AgentStatus) -> String {
    match status {
        AgentStatus::Starting => "starting",
        AgentStatus::Busy => "busy",
        AgentStatus::Idle => "idle",
        AgentStatus::Errored => "errored",
        AgentStatus::Exited => "exited",
    }
    .to_string()
}

fn turn_state_label(state: &AgentTurnState) -> String {
    match state {
        AgentTurnState::Unknown => "unknown",
        AgentTurnState::WaitingOnAgent => "waiting-agent",
        AgentTurnState::WaitingOnUser => "waiting-user",
    }
    .to_string()
}

fn harness_label(harness: &AgentHarness) -> String {
    match harness {
        AgentHarness::Unknown => "unknown",
        AgentHarness::Claude => "claude",
        AgentHarness::Codex => "codex",
        AgentHarness::Gemini => "gemini",
        AgentHarness::Opencode => "opencode",
    }
    .to_string()
}

fn transport_label(transport: &AgentTransport) -> String {
    match transport {
        AgentTransport::PlainPty => "pty",
        AgentTransport::ObservedPty => "observed-pty",
    }
    .to_string()
}

fn inline_progress_summary(agent: &AgentSnapshot) -> String {
    let summary = agent
        .runtime
        .progress_summary
        .as_deref()
        .or(agent.runtime.observer_error.as_deref())
        .map(|summary| summary.replace('\n', " "))
        .or_else(|| {
            agent
                .runtime
                .attention_reason
                .as_deref()
                .map(|reason| format!("attention: {}", reason.replace('_', "-")))
        })
        .unwrap_or_default();

    let mut tags = vec![];
    if let Some(mode) = agent.runtime.harness_mode.as_deref() {
        let mode = mode.trim();
        if !mode.is_empty() {
            tags.push(mode.replace('_', "-"));
        }
    }
    if let Some(phase) = agent.runtime.turn_phase.as_deref() {
        let phase = phase.trim();
        if !phase.is_empty() {
            tags.push(phase.replace('_', "-"));
        }
    }

    if tags.is_empty() {
        return summary;
    }

    let prefix = format!("[{}]", tags.join(" "));
    if summary.is_empty() {
        prefix
    } else {
        format!("{prefix} {summary}")
    }
}

fn inline_progress_detail(agent: &AgentSnapshot) -> Option<String> {
    let summary = inline_progress_summary(agent);
    if summary.is_empty() {
        None
    } else {
        Some(summary)
    }
}

fn watch_event_message(agent: &AgentSnapshot) -> String {
    let summary = inline_progress_summary(agent);
    if !summary.is_empty() {
        return summary;
    }
    pending_observer_detail(&agent.metadata, &agent.runtime).unwrap_or_default()
}

async fn reload_spawned_agent_after_startup<ListAgents, ListAgentsFut>(
    list_agents: &mut ListAgents,
    pane_id: PaneId,
    agent_name: &str,
    stabilization_delay_ms: u64,
) -> anyhow::Result<AgentSnapshot>
where
    ListAgents: FnMut() -> ListAgentsFut,
    ListAgentsFut: Future<Output = anyhow::Result<codec::ListAgentsResponse>>,
{
    let initial = list_agents()
        .await?
        .agents
        .into_iter()
        .find(|agent| agent.pane_id == pane_id)
        .ok_or_else(|| anyhow::anyhow!("spawned agent but could not reload it from the mux"))?;
    ensure_spawned_agent_is_running(&initial, agent_name)?;

    smol::Timer::after(Duration::from_millis(stabilization_delay_ms)).await;

    let stabilized = list_agents()
        .await?
        .agents
        .into_iter()
        .find(|agent| agent.pane_id == pane_id)
        .ok_or_else(|| anyhow::anyhow!("agent {agent_name} disappeared shortly after startup"))?;
    ensure_spawned_agent_is_running(&stabilized, agent_name)?;
    Ok(stabilized)
}

fn ensure_spawned_agent_is_running(agent: &AgentSnapshot, agent_name: &str) -> anyhow::Result<()> {
    if !agent.runtime.alive || matches!(agent.runtime.status, AgentStatus::Exited) {
        let detail = pending_observer_detail(&agent.metadata, &agent.runtime)
            .or_else(|| agent.runtime.attention_reason.clone())
            .unwrap_or_else(|| "harness exited before the observer could attach".to_string());
        bail!("agent {agent_name} exited shortly after startup: {detail}");
    }
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use chrono::TimeZone;
    use codec::{
        ListAgentsResponse, ListPanesResponse, SendKeyDown, SendPaste, SpawnResponse, UnitResponse,
        WriteToPane,
    };
    use mux::agent::AgentMetadata;
    use mux::client::ClientWindowViewState;
    use mux::renderable::StableCursorPosition;
    use mux::tab::{PaneEntry, PaneNode, SerdeUrl, SplitDirection, SplitDirectionAndSize};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::convert::TryFrom;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;
    use std::sync::Mutex;
    use tempfile::TempDir;
    use termwiz::surface::{CursorShape, CursorVisibility};
    use wakterm_term::TerminalSize;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn set_env_var(key: &str, value: &str) {
        unsafe {
            std::env::set_var(key, value);
        }
    }

    fn remove_env_var(key: &str) {
        unsafe {
            std::env::remove_var(key);
        }
    }

    fn size(cols: usize, rows: usize) -> TerminalSize {
        TerminalSize {
            cols,
            rows,
            pixel_width: cols * 8,
            pixel_height: rows * 18,
            dpi: 96,
        }
    }

    fn leaf(
        window_id: mux::window::WindowId,
        tab_id: mux::tab::TabId,
        pane_id: PaneId,
    ) -> PaneNode {
        PaneNode::Leaf(PaneEntry {
            window_id,
            tab_id,
            pane_id,
            agent_metadata: None,
            title: format!("pane-{pane_id}"),
            size: size(80, 24),
            working_dir: Some(SerdeUrl::try_from(format!("file:///tmp/pane-{pane_id}")).unwrap()),
            is_active_pane: true,
            is_zoomed_pane: false,
            workspace: "default".to_string(),
            cursor_pos: StableCursorPosition {
                x: 0,
                y: 0,
                shape: CursorShape::Default,
                visibility: CursorVisibility::Visible,
            },
            physical_top: 0,
            top_row: 0,
            left_col: 0,
            tty_name: None,
        })
    }

    fn split(left: PaneNode, right: PaneNode, node: SplitDirectionAndSize) -> PaneNode {
        PaneNode::Split {
            left: Box::new(left),
            right: Box::new(right),
            node,
        }
    }

    fn panes_response(panes: Vec<PaneNode>) -> ListPanesResponse {
        ListPanesResponse {
            tabs: panes,
            tab_titles: vec!["tab".to_string()],
            tab_badges: vec![Default::default()],
            window_titles: HashMap::new(),
            client_window_view_state: HashMap::<mux::window::WindowId, ClientWindowViewState>::new(
            ),
        }
    }

    fn sample_agent(pane_id: PaneId, name: &str) -> AgentSnapshot {
        AgentSnapshot {
            metadata: AgentMetadata {
                agent_id: format!("id-{name}"),
                name: name.to_string(),
                launch_cmd: "codex".to_string(),
                declared_cwd: format!("file:///tmp/{name}"),
                created_at: Utc.with_ymd_and_hms(2026, 3, 17, 12, 0, 0).unwrap(),
                repo_root: None,
                worktree: None,
                branch: None,
                managed_checkout: false,
            },
            runtime: mux::agent::AgentRuntimeSnapshot {
                harness: mux::agent::AgentHarness::Codex,
                transport: mux::agent::AgentTransport::PlainPty,
                status: mux::agent::AgentStatus::Idle,
                turn_state: mux::agent::AgentTurnState::Unknown,
                alive: true,
                foreground_process_name: Some("codex".to_string()),
                tty_name: Some("/dev/pts/1".to_string()),
                last_input_at: None,
                last_output_at: None,
                last_progress_at: None,
                last_turn_completed_at: None,
                observed_at: Utc.with_ymd_and_hms(2026, 3, 17, 12, 0, 0).unwrap(),
                session_path: None,
                progress_summary: None,
                harness_mode: None,
                turn_phase: None,
                attention_reason: None,
                terminal_progress: wakterm_term::Progress::None,
                observer_error: None,
                observer_started_at: None,
            },
            pane_id,
            tab_id: 20,
            window_id: 10,
            workspace: "default".to_string(),
            domain_id: 1,
        }
    }

    fn sample_spawn_response(pane_id: PaneId, tab_id: mux::tab::TabId) -> SpawnResponse {
        SpawnResponse {
            pane_id,
            tab_id,
            window_id: 10,
            size: size(80, 24),
        }
    }

    fn run_git(dir: &Path, args: &[&str]) {
        let status = ProcessCommand::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(
            status.success(),
            "git {:?} failed in {}",
            args,
            dir.display()
        );
    }

    fn init_git_repo() -> (TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "-b", "main"]);
        run_git(&repo, &["config", "user.email", "test@example.com"]);
        run_git(&repo, &["config", "user.name", "Test User"]);
        fs::write(repo.join("README.md"), "hello\n").unwrap();
        run_git(&repo, &["add", "README.md"]);
        run_git(&repo, &["commit", "-m", "init"]);
        (temp, repo)
    }

    #[test]
    fn inspect_matches_by_name_or_id() {
        let alpha = sample_agent(30, "alpha");
        let beta = sample_agent(31, "beta");
        let agents = vec![alpha.clone(), beta.clone()];

        assert_eq!(find_agent(&agents, "alpha"), Some(&alpha));
        assert_eq!(find_agent(&agents, "id-beta"), Some(&beta));
        assert_eq!(find_agent(&agents, "missing"), None);
    }

    #[test]
    fn send_uses_observed_transport_and_waits_for_ack() {
        let paste_calls = Rc::new(RefCell::new(vec![]));
        let write_calls = Rc::new(RefCell::new(vec![]));
        let list_calls = Rc::new(RefCell::new(0usize));
        let command = SendAgentCommand {
            target: "reviewer".to_string(),
            no_paste: false,
            no_submit: false,
            ack_timeout_ms: 0,
            ack_poll_ms: 0,
            text: Some("fix this".to_string()),
        };

        let mut baseline = sample_agent(30, "reviewer");
        baseline.runtime.transport = mux::agent::AgentTransport::ObservedPty;
        baseline.runtime.session_path = Some("/tmp/reviewer.jsonl".to_string());
        baseline.runtime.last_progress_at =
            Some(Utc.with_ymd_and_hms(2026, 3, 17, 12, 0, 0).unwrap());

        let mut acknowledged = baseline.clone();
        acknowledged.runtime.last_progress_at =
            Some(Utc.with_ymd_and_hms(2026, 3, 17, 12, 0, 1).unwrap());
        acknowledged.runtime.progress_summary = Some("accepted".to_string());

        let result = promise::spawn::block_on(command.run_with(
            {
                let list_calls = Rc::clone(&list_calls);
                move || {
                    let list_calls = Rc::clone(&list_calls);
                    let baseline = baseline.clone();
                    let acknowledged = acknowledged.clone();
                    async move {
                        let idx = {
                            let mut calls = list_calls.borrow_mut();
                            *calls += 1;
                            *calls
                        };
                        Ok(ListAgentsResponse {
                            agents: vec![if idx == 1 { baseline } else { acknowledged }],
                        })
                    }
                }
            },
            {
                let write_calls = Rc::clone(&write_calls);
                move |request: WriteToPane| {
                    write_calls.borrow_mut().push(request);
                    async { Ok(UnitResponse {}) }
                }
            },
            {
                let paste_calls = Rc::clone(&paste_calls);
                move |request: SendPaste| {
                    paste_calls.borrow_mut().push(request);
                    async { Ok(UnitResponse {}) }
                }
            },
        ))
        .unwrap();

        assert_eq!(result.agent_name, "reviewer");
        assert_eq!(result.transport, mux::agent::AgentTransport::ObservedPty);
        assert!(result.submitted);
        assert_eq!(result.acknowledgement.kind, AgentAckKind::SessionObserver);
        assert!(result.acknowledgement.acknowledged);

        let paste_calls = paste_calls.borrow();
        assert_eq!(paste_calls.len(), 1);
        assert_eq!(paste_calls[0].pane_id, 30);
        assert_eq!(paste_calls[0].data, "fix this");

        let write_calls = write_calls.borrow();
        assert_eq!(write_calls.len(), 1);
        assert_eq!(write_calls[0].pane_id, 30);
        assert_eq!(write_calls[0].data, b"\r");
    }

    #[test]
    fn send_defaults_to_raw_write_for_gemini() {
        let write_calls = Rc::new(RefCell::new(vec![]));
        let command = SendAgentCommand {
            target: "reviewer".to_string(),
            no_paste: false,
            no_submit: true,
            ack_timeout_ms: 0,
            ack_poll_ms: 0,
            text: Some("fix this".to_string()),
        };

        let mut baseline = sample_agent(30, "reviewer");
        baseline.runtime.harness = mux::agent::AgentHarness::Gemini;
        baseline.runtime.foreground_process_name = Some("node".to_string());

        let result = promise::spawn::block_on(command.run_with(
            move || {
                let baseline = baseline.clone();
                async move {
                    Ok(ListAgentsResponse {
                        agents: vec![baseline],
                    })
                }
            },
            {
                let write_calls = Rc::clone(&write_calls);
                move |request: WriteToPane| {
                    write_calls.borrow_mut().push(request);
                    async { Ok(UnitResponse {}) }
                }
            },
            |_| async { panic!("send_paste should not be used for gemini") },
        ))
        .unwrap();

        assert_eq!(result.agent_name, "reviewer");
        assert_eq!(result.acknowledgement.kind, AgentAckKind::NotRequested);
        let write_calls = write_calls.borrow();
        assert_eq!(write_calls.len(), 1);
        assert_eq!(write_calls[0].pane_id, 30);
        assert_eq!(write_calls[0].data, b"fix this");
    }

    #[test]
    fn inline_progress_summary_prefixes_harness_mode_and_phase() {
        let mut agent = sample_agent(30, "reviewer");
        agent.runtime.harness_mode = Some("plan".to_string());
        agent.runtime.turn_phase = Some("final_answer".to_string());
        agent.runtime.progress_summary = Some("all good".to_string());

        assert_eq!(
            inline_progress_summary(&agent),
            "[plan final-answer] all good"
        );
    }

    #[test]
    fn inline_progress_summary_falls_back_to_attention_reason() {
        let mut agent = sample_agent(30, "reviewer");
        agent.runtime.attention_reason = Some("turn-aborted".to_string());

        assert_eq!(inline_progress_summary(&agent), "attention: turn-aborted");

        agent.runtime.harness_mode = Some("plan".to_string());
        agent.runtime.turn_phase = Some("aborted".to_string());
        assert_eq!(
            inline_progress_summary(&agent),
            "[plan aborted] attention: turn-aborted"
        );
    }

    #[test]
    fn collect_agent_watch_events_sorts_and_skips_empty_messages() {
        let mut alpha = sample_agent(30, "alpha");
        alpha.runtime.progress_summary = Some("ready".to_string());
        alpha.runtime.last_progress_at = Some(Utc.with_ymd_and_hms(2026, 3, 17, 12, 0, 1).unwrap());

        let beta = sample_agent(31, "beta");

        let mut gamma = sample_agent(32, "gamma");
        gamma.runtime.progress_summary = Some("working".to_string());
        gamma.runtime.last_progress_at = Some(Utc.with_ymd_and_hms(2026, 3, 17, 12, 0, 2).unwrap());

        let mut seen = HashMap::new();
        let events = collect_agent_watch_events(&mut seen, &[gamma.clone(), beta, alpha.clone()]);

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].name, "alpha");
        assert_eq!(events[0].message, "ready");
        assert_eq!(events[1].name, "gamma");
        assert_eq!(events[1].message, "working");
        assert_eq!(seen.len(), 2);
        assert!(seen.contains_key(&alpha.metadata.agent_id));
        assert!(seen.contains_key(&gamma.metadata.agent_id));
    }

    #[test]
    fn collect_agent_watch_events_preserves_attention_reason() {
        let mut agent = sample_agent(30, "alpha");
        agent.runtime.attention_reason = Some("exited".to_string());

        let mut seen = HashMap::new();
        let events = collect_agent_watch_events(&mut seen, &[agent]);

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].message, "attention: exited");
        assert_eq!(events[0].attention_reason.as_deref(), Some("exited"));
    }

    #[test]
    fn collect_agent_watch_events_falls_back_to_pending_observer_hint() {
        let _guard = ENV_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        set_env_var(
            "WAKTERM_AGENT_CODEX_DIR",
            temp.path().to_string_lossy().as_ref(),
        );

        let mut agent = sample_agent(30, "alpha");
        agent.runtime.status = mux::agent::AgentStatus::Starting;
        agent.runtime.observer_started_at =
            Some(Utc.with_ymd_and_hms(2026, 3, 17, 12, 0, 0).unwrap());

        let mut seen = HashMap::new();
        let events = collect_agent_watch_events(&mut seen, &[agent]);

        remove_env_var("WAKTERM_AGENT_CODEX_DIR");

        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].message,
            "codex rollout session file has not appeared yet"
        );
        assert_eq!(events[0].transport, "pty");
        assert_eq!(
            events[0].observer_hint.as_deref(),
            Some("codex rollout session file has not appeared yet")
        );
    }

    #[test]
    fn watch_run_with_streams_initial_and_changed_messages() {
        let command = WatchAgentsCommand {
            format: CliOutputFormatKind::Table,
            poll_ms: 0,
        };

        let mut baseline = sample_agent(30, "reviewer");
        baseline.runtime.progress_summary = Some("thinking".to_string());
        baseline.runtime.harness_mode = Some("plan".to_string());
        baseline.runtime.turn_phase = Some("commentary".to_string());
        baseline.runtime.last_progress_at =
            Some(Utc.with_ymd_and_hms(2026, 3, 17, 12, 0, 1).unwrap());

        let unchanged = baseline.clone();

        let mut updated = baseline.clone();
        updated.runtime.progress_summary = Some("done".to_string());
        updated.runtime.turn_phase = Some("final_answer".to_string());
        updated.runtime.last_progress_at =
            Some(Utc.with_ymd_and_hms(2026, 3, 17, 12, 0, 2).unwrap());

        let polls = Rc::new(RefCell::new(0usize));
        let mut out = Vec::new();
        promise::spawn::block_on(command.run_with(
            {
                let polls = Rc::clone(&polls);
                move || {
                    let polls = Rc::clone(&polls);
                    let baseline = baseline.clone();
                    let unchanged = unchanged.clone();
                    let updated = updated.clone();
                    async move {
                        let idx = {
                            let mut polls = polls.borrow_mut();
                            *polls += 1;
                            *polls
                        };
                        let agents = match idx {
                            1 => vec![baseline],
                            2 => vec![unchanged],
                            _ => vec![updated],
                        };
                        Ok(ListAgentsResponse { agents })
                    }
                }
            },
            &mut out,
            Some(3),
        ))
        .unwrap();

        assert_eq!(
            String::from_utf8(out).unwrap(),
            concat!(
                "reviewer\tcodex\t[plan commentary] thinking\n",
                "reviewer\tcodex\t[plan final-answer] done\n"
            )
        );
    }

    #[test]
    fn watch_event_json_uses_explicit_runtime_field_names() {
        let mut agent = sample_agent(30, "reviewer");
        agent.runtime.transport = mux::agent::AgentTransport::ObservedPty;
        agent.runtime.progress_summary = Some("done".to_string());
        agent.runtime.harness_mode = Some("plan".to_string());
        agent.runtime.turn_phase = Some("final_answer".to_string());
        let event = AgentWatchEvent::from_agent(&agent).unwrap();

        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(
            json.get("transport").and_then(|v| v.as_str()),
            Some("observed-pty")
        );
        assert_eq!(
            json.get("harness_mode").and_then(|v| v.as_str()),
            Some("plan")
        );
        assert_eq!(
            json.get("turn_phase").and_then(|v| v.as_str()),
            Some("final_answer")
        );
        assert!(json.get("mode").is_none());
        assert!(json.get("phase").is_none());
    }

    #[test]
    fn reload_spawned_agent_after_startup_reports_disappeared_pane() {
        let list_calls = Rc::new(RefCell::new(0usize));
        let err = promise::spawn::block_on(reload_spawned_agent_after_startup(
            &mut {
                let list_calls = Rc::clone(&list_calls);
                move || {
                    let list_calls = Rc::clone(&list_calls);
                    async move {
                        let idx = {
                            let mut calls = list_calls.borrow_mut();
                            *calls += 1;
                            *calls
                        };
                        Ok(ListAgentsResponse {
                            agents: if idx == 1 {
                                vec![sample_agent(30, "reviewer")]
                            } else {
                                vec![]
                            },
                        })
                    }
                }
            },
            30,
            "reviewer",
            0,
        ))
        .unwrap_err();

        assert!(err
            .to_string()
            .contains("agent reviewer disappeared shortly after startup"));
    }

    #[test]
    fn reload_spawned_agent_after_startup_reports_exited_harness() {
        let mut exited = sample_agent(30, "reviewer");
        exited.runtime.alive = false;
        exited.runtime.status = mux::agent::AgentStatus::Exited;

        let err = promise::spawn::block_on(reload_spawned_agent_after_startup(
            &mut move || {
                let exited = exited.clone();
                async move {
                    Ok(ListAgentsResponse {
                        agents: vec![exited],
                    })
                }
            },
            30,
            "reviewer",
            0,
        ))
        .unwrap_err();

        assert!(err
            .to_string()
            .contains("agent reviewer exited shortly after startup"));
    }

    #[test]
    fn interrupt_codex_uses_ctrl_c_and_waits_for_ack() {
        let key_calls = Rc::new(RefCell::new(vec![]));
        let list_calls = Rc::new(RefCell::new(0usize));
        let command = InterruptAgentCommand {
            target: "reviewer".to_string(),
            ack_timeout_ms: 0,
            ack_poll_ms: 0,
        };

        let mut baseline = sample_agent(30, "reviewer");
        baseline.runtime.transport = mux::agent::AgentTransport::ObservedPty;
        baseline.runtime.session_path = Some("/tmp/reviewer.jsonl".to_string());
        baseline.runtime.progress_summary = Some("thinking".to_string());
        baseline.runtime.harness_mode = Some("plan".to_string());
        baseline.runtime.turn_phase = Some("commentary".to_string());
        baseline.runtime.last_progress_at =
            Some(Utc.with_ymd_and_hms(2026, 3, 17, 12, 0, 0).unwrap());

        let mut acknowledged = baseline.clone();
        acknowledged.runtime.turn_phase = Some("aborted".to_string());
        acknowledged.runtime.progress_summary = None;
        acknowledged.runtime.last_progress_at =
            Some(Utc.with_ymd_and_hms(2026, 3, 17, 12, 0, 1).unwrap());

        let result = promise::spawn::block_on(command.run_with(
            {
                let list_calls = Rc::clone(&list_calls);
                move || {
                    let list_calls = Rc::clone(&list_calls);
                    let baseline = baseline.clone();
                    let acknowledged = acknowledged.clone();
                    async move {
                        let idx = {
                            let mut calls = list_calls.borrow_mut();
                            *calls += 1;
                            *calls
                        };
                        Ok(ListAgentsResponse {
                            agents: vec![if idx == 1 { baseline } else { acknowledged }],
                        })
                    }
                }
            },
            {
                let key_calls = Rc::clone(&key_calls);
                move |request: SendKeyDown| {
                    key_calls.borrow_mut().push(request);
                    async { Ok(UnitResponse {}) }
                }
            },
        ))
        .unwrap();

        assert_eq!(result.agent_name, "reviewer");
        assert_eq!(result.harness, mux::agent::AgentHarness::Codex);
        assert_eq!(result.acknowledgement.kind, AgentAckKind::SessionObserver);
        assert!(result.acknowledgement.acknowledged);
        assert_eq!(
            result.acknowledgement.detail.as_deref(),
            Some("[plan aborted]")
        );

        let key_calls = key_calls.borrow();
        assert_eq!(key_calls.len(), 1);
        assert_eq!(key_calls[0].pane_id, 30);
        assert_eq!(key_calls[0].event.key, KeyCode::Char('C'));
        assert_eq!(key_calls[0].event.modifiers, Modifiers::CTRL);
    }

    #[test]
    fn interrupt_rejects_non_codex_agents() {
        let command = InterruptAgentCommand {
            target: "reviewer".to_string(),
            ack_timeout_ms: 0,
            ack_poll_ms: 0,
        };

        let mut agent = sample_agent(30, "reviewer");
        agent.runtime.harness = mux::agent::AgentHarness::Claude;

        let err = promise::spawn::block_on(command.run_with(
            move || {
                let agent = agent.clone();
                async move {
                    Ok(ListAgentsResponse {
                        agents: vec![agent],
                    })
                }
            },
            |_| async { Ok(UnitResponse {}) },
        ))
        .unwrap_err();

        assert!(err
            .to_string()
            .contains("interrupt is currently only implemented for codex panes"));
    }

    #[test]
    fn send_plain_transport_waits_for_first_observer_session() {
        let write_calls = Rc::new(RefCell::new(vec![]));
        let list_calls = Rc::new(RefCell::new(0usize));
        let command = SendAgentCommand {
            target: "reviewer".to_string(),
            no_paste: true,
            no_submit: false,
            ack_timeout_ms: 0,
            ack_poll_ms: 0,
            text: Some("raw".to_string()),
        };

        let baseline = sample_agent(30, "reviewer");
        let mut acknowledged = baseline.clone();
        acknowledged.runtime.transport = mux::agent::AgentTransport::ObservedPty;
        acknowledged.runtime.session_path = Some("/tmp/reviewer.jsonl".to_string());
        acknowledged.runtime.last_progress_at =
            Some(Utc.with_ymd_and_hms(2026, 3, 17, 12, 0, 1).unwrap());
        acknowledged.runtime.progress_summary = Some("accepted".to_string());

        let result = promise::spawn::block_on(command.run_with(
            {
                let list_calls = Rc::clone(&list_calls);
                move || {
                    let list_calls = Rc::clone(&list_calls);
                    let baseline = baseline.clone();
                    let acknowledged = acknowledged.clone();
                    async move {
                        let idx = {
                            let mut calls = list_calls.borrow_mut();
                            *calls += 1;
                            *calls
                        };
                        Ok(ListAgentsResponse {
                            agents: vec![if idx == 1 { baseline } else { acknowledged }],
                        })
                    }
                }
            },
            {
                let write_calls = Rc::clone(&write_calls);
                move |request: WriteToPane| {
                    write_calls.borrow_mut().push(request);
                    async { Ok(UnitResponse {}) }
                }
            },
            |_| async { panic!("send_paste should not be used") },
        ))
        .unwrap();

        assert_eq!(result.transport, mux::agent::AgentTransport::PlainPty);
        assert_eq!(result.acknowledgement.kind, AgentAckKind::SessionObserver);
        assert!(result.acknowledgement.acknowledged);
        assert_eq!(
            result.acknowledgement.session_path.as_deref(),
            Some("/tmp/reviewer.jsonl")
        );

        let write_calls = write_calls.borrow();
        assert_eq!(write_calls.len(), 2);
        assert_eq!(write_calls[0].pane_id, 30);
        assert_eq!(write_calls[0].data, b"raw");
        assert_eq!(write_calls[1].pane_id, 30);
        assert_eq!(write_calls[1].data, b"\r");
    }

    #[test]
    fn send_plain_transport_retries_submit_when_no_observer_ack_arrives() {
        let write_calls = Rc::new(RefCell::new(vec![]));
        let command = SendAgentCommand {
            target: "reviewer".to_string(),
            no_paste: true,
            no_submit: false,
            ack_timeout_ms: 0,
            ack_poll_ms: 0,
            text: Some("raw".to_string()),
        };

        let result = promise::spawn::block_on(command.run_with(
            || async {
                Ok(ListAgentsResponse {
                    agents: vec![sample_agent(30, "reviewer")],
                })
            },
            {
                let write_calls = Rc::clone(&write_calls);
                move |request: WriteToPane| {
                    write_calls.borrow_mut().push(request);
                    async { Ok(UnitResponse {}) }
                }
            },
            |_| async { panic!("send_paste should not be used") },
        ))
        .unwrap();

        assert_eq!(result.transport, mux::agent::AgentTransport::PlainPty);
        assert_eq!(result.acknowledgement.kind, AgentAckKind::TimedOut);
        assert!(!result.acknowledgement.acknowledged);
        assert_eq!(
            result.acknowledgement.detail.as_deref(),
            Some("observer session did not appear before timeout")
        );

        let write_calls = write_calls.borrow();
        assert_eq!(write_calls.len(), 3);
        assert_eq!(write_calls[0].pane_id, 30);
        assert_eq!(write_calls[0].data, b"raw");
        assert_eq!(write_calls[1].pane_id, 30);
        assert_eq!(write_calls[1].data, b"\r");
        assert_eq!(write_calls[2].pane_id, 30);
        assert_eq!(write_calls[2].data, b"\r");
    }

    #[test]
    fn send_no_submit_skips_submit_and_ack_wait() {
        let paste_calls = Rc::new(RefCell::new(vec![]));
        let command = SendAgentCommand {
            target: "reviewer".to_string(),
            no_paste: false,
            no_submit: true,
            ack_timeout_ms: 1000,
            ack_poll_ms: 0,
            text: Some("draft".to_string()),
        };

        let result = promise::spawn::block_on(command.run_with(
            || async {
                let mut agent = sample_agent(30, "reviewer");
                agent.runtime.transport = mux::agent::AgentTransport::ObservedPty;
                agent.runtime.session_path = Some("/tmp/reviewer.jsonl".to_string());
                Ok(ListAgentsResponse {
                    agents: vec![agent],
                })
            },
            |_| async { panic!("write_to_pane should not be used") },
            {
                let paste_calls = Rc::clone(&paste_calls);
                move |request: SendPaste| {
                    paste_calls.borrow_mut().push(request);
                    async { Ok(UnitResponse {}) }
                }
            },
        ))
        .unwrap();

        assert!(!result.submitted);
        assert_eq!(result.acknowledgement.kind, AgentAckKind::NotRequested);
        assert!(!result.acknowledgement.acknowledged);

        let paste_calls = paste_calls.borrow();
        assert_eq!(paste_calls.len(), 1);
        assert_eq!(paste_calls[0].data, "draft");
    }

    #[test]
    fn set_preserves_existing_identity_and_fills_missing_fields() {
        let calls = Rc::new(RefCell::new(vec![]));
        let command = SetAgentCommand {
            pane_id: Some(30),
            name: "reviewer".to_string(),
            launch_cmd: None,
            cwd: None,
            repo_root: Some("/repo".to_string()),
            worktree: None,
            branch: Some("agent/reviewer".to_string()),
            managed_checkout: false,
            unmanaged_checkout: false,
        };
        let mut existing = sample_agent(30, "old-name");
        existing.metadata.managed_checkout = true;
        promise::spawn::block_on(command.run_with(
            || async {
                Ok(ListAgentsResponse {
                    agents: vec![existing.clone()],
                })
            },
            || async { Ok(panes_response(vec![leaf(10, 20, 30)])) },
            || async {
                Ok(ListAgentsResponse {
                    agents: vec![AgentSnapshot {
                        metadata: AgentMetadata {
                            name: "reviewer".to_string(),
                            repo_root: Some("/repo".to_string()),
                            branch: Some("agent/reviewer".to_string()),
                            ..existing.metadata.clone()
                        },
                        ..existing.clone()
                    }],
                })
            },
            |pane_id| async move { Ok(pane_id.expect("pane_id to be provided")) },
            |request| {
                calls.borrow_mut().push(request);
                async { Ok(UnitResponse {}) }
            },
        ))
        .unwrap();

        let call = calls.borrow();
        assert_eq!(call.len(), 1);
        assert_eq!(call[0].pane_id, 30);
        assert_eq!(call[0].metadata.agent_id, existing.metadata.agent_id);
        assert_eq!(call[0].metadata.launch_cmd, existing.metadata.launch_cmd);
        assert_eq!(
            call[0].metadata.declared_cwd,
            existing.metadata.declared_cwd
        );
        assert_eq!(call[0].metadata.name, "reviewer");
        assert_eq!(call[0].metadata.repo_root.as_deref(), Some("/repo"));
        assert_eq!(call[0].metadata.branch.as_deref(), Some("agent/reviewer"));
        assert!(call[0].metadata.managed_checkout);
    }

    #[test]
    fn adopt_uses_live_pane_cwd_and_marks_checkout_unmanaged() {
        let calls = Rc::new(RefCell::new(vec![]));
        let command = AdoptAgentCommand {
            pane_id: Some(30),
            name: "reviewer".to_string(),
            cmd: "codex --profile fast".to_string(),
            cwd: None,
            repo_root: Some("/repo".to_string()),
            worktree: None,
            branch: Some("main".to_string()),
        };

        promise::spawn::block_on(command.run_with(
            || async { Ok(ListAgentsResponse { agents: vec![] }) },
            || async { Ok(panes_response(vec![leaf(10, 20, 30)])) },
            || async {
                Ok(ListAgentsResponse {
                    agents: vec![sample_agent(30, "reviewer")],
                })
            },
            |pane_id| async move { Ok(pane_id.expect("pane_id to be provided")) },
            |request| {
                calls.borrow_mut().push(request);
                async { Ok(UnitResponse {}) }
            },
        ))
        .unwrap();

        let calls = calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].metadata.name, "reviewer");
        assert_eq!(calls[0].metadata.launch_cmd, "codex --profile fast");
        assert_eq!(calls[0].metadata.declared_cwd, "/tmp/pane-30");
        assert!(!calls[0].metadata.managed_checkout);
    }

    #[test]
    fn spawn_split_inherits_tab_context_and_real_path_cwd() {
        let split_calls = Rc::new(RefCell::new(vec![]));
        let set_calls = Rc::new(RefCell::new(vec![]));
        let command = SpawnAgentCommand {
            harness: Some(AgentStartHarness::Codex),
            name: Some("reviewer".to_string()),
            here: false,
            replace: false,
            split: true,
            pane_id: Some(30),
            new_window: false,
            workspace: None,
            horizontal: false,
            left: false,
            right: true,
            top: false,
            bottom: false,
            cells: None,
            percent: Some(40),
            repo: None,
            worktree: WorktreeMode::None,
            branch: None,
            cwd: None,
            cmd: Some("codex --model gpt-5".to_string()),
        };
        let left_size = size(80, 24);
        let right_size = size(39, 24);
        let root_size = size(120, 24);

        let agent = promise::spawn::block_on(command.run_with(
            &ConfigHandle::default_config(),
            || async { Ok(ListAgentsResponse { agents: vec![] }) },
            || async {
                Ok(panes_response(vec![split(
                    leaf(10, 20, 30),
                    leaf(10, 20, 31),
                    SplitDirectionAndSize {
                        direction: SplitDirection::Horizontal,
                        first: left_size,
                        second: right_size,
                    },
                )]))
            },
            || async {
                Ok(ListAgentsResponse {
                    agents: vec![sample_agent(44, "reviewer")],
                })
            },
            |pane_id| async move { Ok(pane_id.expect("pane id")) },
            |_| async move { panic!("spawn_v2 should not be used for split agent spawn") },
            {
                let split_calls = Rc::clone(&split_calls);
                move |request| {
                    split_calls.borrow_mut().push(request);
                    async { Ok(sample_spawn_response(44, 20)) }
                }
            },
            |_| async { panic!("send_paste should not be used for split agent start") },
            |_| async { panic!("key_down should not be used for split agent start") },
            |_| async { panic!("set_tab_title should not be used for split agent spawn") },
            {
                let set_calls = Rc::clone(&set_calls);
                move |request| {
                    set_calls.borrow_mut().push(request);
                    async { Ok(UnitResponse {}) }
                }
            },
            |_| async { panic!("clear_agent_metadata should not be used on success") },
            |_| async move { panic!("kill_pane should not be called on success") },
            |cmd, agent_name, agents, current_cwd| {
                cmd.prepare_launch(agent_name, agents, current_cwd)
            },
        ))
        .unwrap();

        assert_eq!(agent.pane_id, 44);

        let split_calls = split_calls.borrow();
        assert_eq!(split_calls.len(), 1);
        assert_eq!(split_calls[0].pane_id, 30);
        assert_eq!(split_calls[0].tab_size, Some(root_size));
        assert_eq!(split_calls[0].command_dir.as_deref(), Some("/tmp/pane-30"));
        assert_eq!(
            split_calls[0].split_request.direction,
            SplitDirection::Horizontal
        );
        assert!(split_calls[0].split_request.target_is_second);
        assert_eq!(split_calls[0].split_request.size, SplitSize::Percent(40));

        let set_calls = set_calls.borrow();
        assert_eq!(set_calls.len(), 1);
        assert_eq!(set_calls[0].pane_id, 44);
        assert_eq!(set_calls[0].metadata.name, "reviewer");
        assert_eq!(set_calls[0].metadata.declared_cwd, "/tmp/pane-30");
        assert_eq!(set_calls[0].metadata.launch_cmd, "codex --model gpt-5");
        assert!(!set_calls[0].metadata.managed_checkout);
    }

    #[test]
    fn spawn_new_tab_in_existing_window_sends_current_pane_context() {
        let spawn_calls = Rc::new(RefCell::new(vec![]));
        let title_calls = Rc::new(RefCell::new(vec![]));
        let set_calls = Rc::new(RefCell::new(vec![]));
        let command = SpawnAgentCommand {
            harness: Some(AgentStartHarness::Codex),
            name: Some("reviewer".to_string()),
            here: false,
            replace: false,
            split: false,
            pane_id: Some(30),
            new_window: false,
            workspace: None,
            horizontal: false,
            left: false,
            right: false,
            top: false,
            bottom: false,
            cells: None,
            percent: None,
            repo: None,
            worktree: WorktreeMode::None,
            branch: None,
            cwd: None,
            cmd: None,
        };
        let root_size = size(80, 24);

        let agent = promise::spawn::block_on(command.run_with(
            &ConfigHandle::default_config(),
            || async { Ok(ListAgentsResponse { agents: vec![] }) },
            || async { Ok(panes_response(vec![leaf(10, 20, 30)])) },
            || async {
                Ok(ListAgentsResponse {
                    agents: vec![sample_agent(44, "reviewer")],
                })
            },
            |pane_id| async move { Ok(pane_id.expect("pane id")) },
            {
                let spawn_calls = Rc::clone(&spawn_calls);
                move |request| {
                    spawn_calls.borrow_mut().push(request);
                    async { Ok(sample_spawn_response(44, 20)) }
                }
            },
            |_| async { panic!("split_pane should not be used for new-tab agent spawn") },
            |_| async { panic!("send_paste should not be used for new-tab agent spawn") },
            |_| async { panic!("key_down should not be used for new-tab agent spawn") },
            {
                let title_calls = Rc::clone(&title_calls);
                move |request| {
                    title_calls.borrow_mut().push(request);
                    async { Ok(UnitResponse {}) }
                }
            },
            {
                let set_calls = Rc::clone(&set_calls);
                move |request| {
                    set_calls.borrow_mut().push(request);
                    async { Ok(UnitResponse {}) }
                }
            },
            |_| async { panic!("clear_agent_metadata should not be used on success") },
            |_| async { panic!("kill_pane should not be called on success") },
            |cmd, agent_name, agents, current_cwd| {
                cmd.prepare_launch(agent_name, agents, current_cwd)
            },
        ))
        .unwrap();

        assert_eq!(agent.pane_id, 44);

        let spawn_calls = spawn_calls.borrow();
        assert_eq!(spawn_calls.len(), 1);
        assert_eq!(spawn_calls[0].window_id, Some(10));
        assert_eq!(spawn_calls[0].current_pane_id, Some(30));
        assert_eq!(spawn_calls[0].size, root_size);
        assert_eq!(spawn_calls[0].command_dir.as_deref(), Some("/tmp/pane-30"));

        let title_calls = title_calls.borrow();
        assert_eq!(title_calls.len(), 1);
        assert_eq!(title_calls[0].tab_id, 20);
        assert_eq!(title_calls[0].title, "reviewer");

        let set_calls = set_calls.borrow();
        assert_eq!(set_calls.len(), 1);
        assert_eq!(set_calls[0].pane_id, 44);
    }

    #[test]
    fn spawn_cleans_up_spawned_pane_when_metadata_attachment_fails() {
        let kill_calls = Rc::new(RefCell::new(vec![]));
        let command = SpawnAgentCommand {
            harness: Some(AgentStartHarness::Codex),
            name: Some("reviewer".to_string()),
            here: false,
            replace: false,
            split: false,
            pane_id: None,
            new_window: true,
            workspace: Some("agents".to_string()),
            horizontal: false,
            left: false,
            right: false,
            top: false,
            bottom: false,
            cells: None,
            percent: None,
            repo: None,
            worktree: WorktreeMode::None,
            branch: None,
            cwd: None,
            cmd: None,
        };

        let err = promise::spawn::block_on(command.run_with(
            &ConfigHandle::default_config(),
            || async { Ok(ListAgentsResponse { agents: vec![] }) },
            || async { panic!("list_panes should not be used for new-window agent spawn") },
            || async { panic!("list_agents_after_set should not be used on failure") },
            |_| async { panic!("resolve_pane_id should not be called") },
            |_| async { Ok(sample_spawn_response(77, 22)) },
            |_| async { panic!("split_pane should not be used") },
            |_| async { panic!("send_paste should not be used for new-window agent spawn") },
            |_| async { panic!("key_down should not be used for new-window agent spawn") },
            |_| async { Ok(UnitResponse {}) },
            |_| async { Err(anyhow::anyhow!("metadata attach failed")) },
            |_| async { panic!("clear_agent_metadata should not be used on metadata failure") },
            {
                let kill_calls = Rc::clone(&kill_calls);
                move |request| {
                    kill_calls.borrow_mut().push(request);
                    async { Ok(UnitResponse {}) }
                }
            },
            |cmd, agent_name, agents, current_cwd| {
                cmd.prepare_launch(agent_name, agents, current_cwd)
            },
        ))
        .unwrap_err();

        assert!(err
            .to_string()
            .contains("spawned pane but failed to attach agent metadata"));
        let kill_calls = kill_calls.borrow();
        assert_eq!(kill_calls.len(), 1);
        assert_eq!(kill_calls[0].pane_id, 77);
    }

    #[test]
    fn spawn_cleans_up_spawned_pane_when_initial_tab_title_fails() {
        let kill_calls = Rc::new(RefCell::new(vec![]));
        let command = SpawnAgentCommand {
            harness: Some(AgentStartHarness::Codex),
            name: Some("reviewer".to_string()),
            here: false,
            replace: false,
            split: false,
            pane_id: None,
            new_window: true,
            workspace: Some("agents".to_string()),
            horizontal: false,
            left: false,
            right: false,
            top: false,
            bottom: false,
            cells: None,
            percent: None,
            repo: None,
            worktree: WorktreeMode::None,
            branch: None,
            cwd: None,
            cmd: None,
        };

        let err = promise::spawn::block_on(command.run_with(
            &ConfigHandle::default_config(),
            || async { Ok(ListAgentsResponse { agents: vec![] }) },
            || async { panic!("list_panes should not be used for new-window agent spawn") },
            || async { panic!("list_agents_after_set should not be used on failure") },
            |_| async { panic!("resolve_pane_id should not be called") },
            |_| async { Ok(sample_spawn_response(77, 22)) },
            |_| async { panic!("split_pane should not be used") },
            |_| async { panic!("send_paste should not be used for new-window agent spawn") },
            |_| async { panic!("key_down should not be used for new-window agent spawn") },
            |_| async { Err(anyhow::anyhow!("title set failed")) },
            |_| async { panic!("set_agent_metadata should not be used on title failure") },
            |_| async { panic!("clear_agent_metadata should not be used on title failure") },
            {
                let kill_calls = Rc::clone(&kill_calls);
                move |request| {
                    kill_calls.borrow_mut().push(request);
                    async { Ok(UnitResponse {}) }
                }
            },
            |cmd, agent_name, agents, current_cwd| {
                cmd.prepare_launch(agent_name, agents, current_cwd)
            },
        ))
        .unwrap_err();

        assert!(err
            .to_string()
            .contains("spawned pane but failed to set initial tab title"));
        let kill_calls = kill_calls.borrow();
        assert_eq!(kill_calls.len(), 1);
        assert_eq!(kill_calls[0].pane_id, 77);
    }

    #[test]
    fn spawn_with_auto_worktree_creates_and_registers_worktree() {
        let (_temp, repo_root) = init_git_repo();
        let spawn_calls = Rc::new(RefCell::new(vec![]));
        let title_calls = Rc::new(RefCell::new(vec![]));
        let set_calls = Rc::new(RefCell::new(vec![]));
        let command = SpawnAgentCommand {
            harness: Some(AgentStartHarness::Codex),
            name: Some("scrape-api".to_string()),
            here: false,
            replace: false,
            split: false,
            pane_id: None,
            new_window: true,
            workspace: Some("agents".to_string()),
            horizontal: false,
            left: false,
            right: false,
            top: false,
            bottom: false,
            cells: None,
            percent: None,
            repo: Some(repo_root.clone()),
            worktree: WorktreeMode::Auto,
            branch: Some("agent/scrape-api".to_string()),
            cwd: None,
            cmd: None,
        };
        let expected_worktree = auto_worktree_path(&repo_root, "scrape-api");

        let agent = promise::spawn::block_on(command.run_with(
            &ConfigHandle::default_config(),
            || async { Ok(ListAgentsResponse { agents: vec![] }) },
            || async { panic!("list_panes should not be used for new-window agent spawn") },
            || async {
                Ok(ListAgentsResponse {
                    agents: vec![sample_agent(88, "scrape-api")],
                })
            },
            |_| async { panic!("resolve_pane_id should not be called") },
            {
                let spawn_calls = Rc::clone(&spawn_calls);
                move |request| {
                    spawn_calls.borrow_mut().push(request);
                    async { Ok(sample_spawn_response(88, 30)) }
                }
            },
            |_| async { panic!("split_pane should not be used") },
            |_| async { panic!("send_paste should not be used for new-window agent spawn") },
            |_| async { panic!("key_down should not be used for new-window agent spawn") },
            {
                let title_calls = Rc::clone(&title_calls);
                move |request| {
                    title_calls.borrow_mut().push(request);
                    async { Ok(UnitResponse {}) }
                }
            },
            {
                let set_calls = Rc::clone(&set_calls);
                move |request| {
                    set_calls.borrow_mut().push(request);
                    async { Ok(UnitResponse {}) }
                }
            },
            |_| async { panic!("clear_agent_metadata should not be used on success") },
            |_| async { panic!("kill_pane should not be called") },
            |cmd, agent_name, agents, current_cwd| {
                cmd.prepare_launch(agent_name, agents, current_cwd)
            },
        ))
        .unwrap();

        assert_eq!(agent.metadata.name, "scrape-api");
        assert!(expected_worktree.exists());
        assert!(git_worktree_exists(&repo_root, &expected_worktree).unwrap());
        let repo_root_string = repo_root.to_string_lossy().to_string();
        let worktree_string = expected_worktree.to_string_lossy().to_string();

        let spawn_calls = spawn_calls.borrow();
        assert_eq!(spawn_calls.len(), 1);
        assert_eq!(spawn_calls[0].workspace, "agents");
        assert_eq!(
            spawn_calls[0].command_dir.as_deref(),
            Some(worktree_string.as_str())
        );

        let title_calls = title_calls.borrow();
        assert_eq!(title_calls.len(), 1);
        assert_eq!(title_calls[0].tab_id, 30);
        assert_eq!(title_calls[0].title, "scrape-api");

        let set_calls = set_calls.borrow();
        assert_eq!(set_calls.len(), 1);
        assert_eq!(
            set_calls[0].metadata.repo_root.as_deref(),
            Some(repo_root_string.as_str())
        );
        assert_eq!(
            set_calls[0].metadata.worktree.as_deref(),
            Some(worktree_string.as_str())
        );
        assert_eq!(
            set_calls[0].metadata.branch.as_deref(),
            Some("agent/scrape-api")
        );
        assert!(set_calls[0].metadata.managed_checkout);
    }

    #[test]
    fn prepare_launch_allows_shared_worktree_paths() {
        let (_temp, repo_root) = init_git_repo();
        let requested_worktree = auto_worktree_path(&repo_root, "alpha");
        let command = SpawnAgentCommand {
            harness: Some(AgentStartHarness::Codex),
            name: Some("beta".to_string()),
            here: false,
            replace: false,
            split: false,
            pane_id: None,
            new_window: true,
            workspace: None,
            horizontal: false,
            left: false,
            right: false,
            top: false,
            bottom: false,
            cells: None,
            percent: None,
            repo: Some(repo_root.clone()),
            worktree: WorktreeMode::Path(requested_worktree.clone()),
            branch: None,
            cwd: None,
            cmd: None,
        };
        let mut owner = sample_agent(40, "alpha");
        owner.metadata.worktree = Some(requested_worktree.to_string_lossy().to_string());

        let prepared = command.prepare_launch("beta", &[owner], None).unwrap();
        assert_eq!(
            prepared.worktree.as_deref(),
            Some(requested_worktree.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn spawn_rejects_unrecognized_harness_commands() {
        let command = SpawnAgentCommand {
            harness: None,
            name: Some("shell".to_string()),
            here: false,
            replace: false,
            split: false,
            pane_id: None,
            new_window: true,
            workspace: Some("agents".to_string()),
            horizontal: false,
            left: false,
            right: false,
            top: false,
            bottom: false,
            cells: None,
            percent: None,
            repo: None,
            worktree: WorktreeMode::None,
            branch: None,
            cwd: None,
            cmd: Some("zsh".to_string()),
        };

        let err = command.prepare_launch("shell", &[], None).unwrap_err();
        assert!(err
            .to_string()
            .contains("agent start requires a recognized harness"));
    }

    #[test]
    fn start_parser_accepts_positional_harness() {
        let parsed = AgentCommand::try_parse_from(["agent", "start", "gemini"]).unwrap();
        let AgentSubCommand::Start(command) = parsed.sub else {
            panic!("expected start command");
        };
        assert_eq!(command.harness, Some(AgentStartHarness::Gemini));
        assert_eq!(command.cmd, None);
    }

    #[test]
    fn start_parser_allows_cmd_override_for_positional_harness() {
        let parsed = AgentCommand::try_parse_from([
            "agent",
            "start",
            "codex",
            "--cmd",
            "codex --profile fast",
        ])
        .unwrap();
        let AgentSubCommand::Start(command) = parsed.sub else {
            panic!("expected start command");
        };
        assert_eq!(command.harness, Some(AgentStartHarness::Codex));
        assert_eq!(command.cmd.as_deref(), Some("codex --profile fast"));
    }

    #[test]
    fn spawn_without_name_uses_harness_base_name() {
        let spawn_calls = Rc::new(RefCell::new(vec![]));
        let title_calls = Rc::new(RefCell::new(vec![]));
        let set_calls = Rc::new(RefCell::new(vec![]));
        let command = SpawnAgentCommand {
            harness: Some(AgentStartHarness::Codex),
            name: None,
            here: false,
            replace: false,
            split: false,
            pane_id: None,
            new_window: true,
            workspace: Some("agents".to_string()),
            horizontal: false,
            left: false,
            right: false,
            top: false,
            bottom: false,
            cells: None,
            percent: None,
            repo: None,
            worktree: WorktreeMode::None,
            branch: None,
            cwd: None,
            cmd: None,
        };

        let agent = promise::spawn::block_on(command.run_with(
            &ConfigHandle::default_config(),
            || async { Ok(ListAgentsResponse { agents: vec![] }) },
            || async { panic!("list_panes should not be used for new-window agent spawn") },
            || async {
                Ok(ListAgentsResponse {
                    agents: vec![sample_agent(88, "codex")],
                })
            },
            |_| async { panic!("resolve_pane_id should not be called") },
            {
                let spawn_calls = Rc::clone(&spawn_calls);
                move |request| {
                    spawn_calls.borrow_mut().push(request);
                    async { Ok(sample_spawn_response(88, 30)) }
                }
            },
            |_| async { panic!("split_pane should not be used") },
            |_| async { panic!("send_paste should not be used for new-window agent spawn") },
            |_| async { panic!("key_down should not be used for new-window agent spawn") },
            {
                let title_calls = Rc::clone(&title_calls);
                move |request| {
                    title_calls.borrow_mut().push(request);
                    async { Ok(UnitResponse {}) }
                }
            },
            {
                let set_calls = Rc::clone(&set_calls);
                move |request| {
                    set_calls.borrow_mut().push(request);
                    async { Ok(UnitResponse {}) }
                }
            },
            |_| async { panic!("clear_agent_metadata should not be used on success") },
            |_| async { panic!("kill_pane should not be called") },
            |cmd, agent_name, agents, current_cwd| {
                cmd.prepare_launch(agent_name, agents, current_cwd)
            },
        ))
        .unwrap();

        assert_eq!(agent.metadata.name, "codex");
        let title_calls = title_calls.borrow();
        assert_eq!(title_calls.len(), 1);
        assert_eq!(title_calls[0].title, "codex");
        let set_calls = set_calls.borrow();
        assert_eq!(set_calls.len(), 1);
        assert_eq!(set_calls[0].metadata.name, "codex");
        let spawn_calls = spawn_calls.borrow();
        assert_eq!(spawn_calls.len(), 1);
    }

    #[test]
    fn spawn_without_name_uses_next_numeric_suffix() {
        let agents = vec![sample_agent(41, "codex"), sample_agent(42, "codex2")];
        assert_eq!(
            resolve_spawn_agent_name(AgentHarness::Codex, None, &agents).unwrap(),
            "codex3"
        );
        assert_eq!(
            resolve_spawn_agent_name(AgentHarness::Claude, None, &agents).unwrap(),
            "claude"
        );
        assert_eq!(
            resolve_spawn_agent_name(AgentHarness::Gemini, None, &agents).unwrap(),
            "gemini"
        );
        assert_eq!(
            resolve_spawn_agent_name(AgentHarness::Opencode, None, &agents).unwrap(),
            "opencode"
        );
    }

    #[test]
    fn start_here_preserves_shell_and_sets_metadata() {
        let paste_calls = Rc::new(RefCell::new(vec![]));
        let key_calls = Rc::new(RefCell::new(vec![]));
        let title_calls = Rc::new(RefCell::new(vec![]));
        let set_calls = Rc::new(RefCell::new(vec![]));
        let clear_calls = Rc::new(RefCell::new(vec![]));
        let command = SpawnAgentCommand {
            harness: Some(AgentStartHarness::Codex),
            name: None,
            here: true,
            replace: false,
            split: false,
            pane_id: Some(30),
            new_window: false,
            workspace: None,
            horizontal: false,
            left: false,
            right: false,
            top: false,
            bottom: false,
            cells: None,
            percent: None,
            repo: None,
            worktree: WorktreeMode::None,
            branch: None,
            cwd: Some("/tmp/agent-start".into()),
            cmd: None,
        };

        let agent = promise::spawn::block_on(command.run_with(
            &ConfigHandle::default_config(),
            || async { Ok(ListAgentsResponse { agents: vec![] }) },
            || async {
                Ok(ListPanesResponse {
                    tabs: vec![leaf(10, 20, 30)],
                    tab_titles: vec!["".into()],
                    tab_badges: vec![Default::default()],
                    window_titles: HashMap::new(),
                    client_window_view_state: HashMap::new(),
                })
            },
            || async {
                Ok(ListAgentsResponse {
                    agents: vec![sample_agent(30, "codex")],
                })
            },
            |pane_id| async move { Ok(pane_id.expect("pane id")) },
            |_| async { panic!("spawn_v2 should not be used for --here") },
            |_| async { panic!("split_pane should not be used for --here") },
            {
                let paste_calls = Rc::clone(&paste_calls);
                move |request| {
                    paste_calls.borrow_mut().push(request);
                    async { Ok(UnitResponse {}) }
                }
            },
            {
                let key_calls = Rc::clone(&key_calls);
                move |request| {
                    key_calls.borrow_mut().push(request);
                    async { Ok(UnitResponse {}) }
                }
            },
            {
                let title_calls = Rc::clone(&title_calls);
                move |request| {
                    title_calls.borrow_mut().push(request);
                    async { Ok(UnitResponse {}) }
                }
            },
            {
                let set_calls = Rc::clone(&set_calls);
                move |request| {
                    set_calls.borrow_mut().push(request);
                    async { Ok(UnitResponse {}) }
                }
            },
            {
                let clear_calls = Rc::clone(&clear_calls);
                move |request| {
                    clear_calls.borrow_mut().push(request);
                    async { Ok(UnitResponse {}) }
                }
            },
            |_| async { panic!("kill_pane should not be used for --here") },
            |cmd, agent_name, agents, current_cwd| {
                cmd.prepare_launch(agent_name, agents, current_cwd)
            },
        ))
        .unwrap();

        assert_eq!(agent.pane_id, 30);
        assert_eq!(agent.metadata.name, "codex");

        let set_calls = set_calls.borrow();
        assert_eq!(set_calls.len(), 1);
        assert_eq!(set_calls[0].pane_id, 30);
        assert_eq!(set_calls[0].metadata.name, "codex");
        assert_eq!(set_calls[0].metadata.declared_cwd, "/tmp/agent-start");

        let paste_calls = paste_calls.borrow();
        assert_eq!(paste_calls.len(), 1);
        assert_eq!(paste_calls[0].pane_id, 30);
        assert_eq!(paste_calls[0].data, "cd /tmp/agent-start && codex");

        let key_calls = key_calls.borrow();
        assert_eq!(key_calls.len(), 1);
        assert_eq!(key_calls[0].pane_id, 30);
        assert_eq!(key_calls[0].event.key, KeyCode::Enter);

        let title_calls = title_calls.borrow();
        assert_eq!(title_calls.len(), 1);
        assert_eq!(title_calls[0].tab_id, 20);
        assert_eq!(title_calls[0].title, "codex");

        let clear_calls = clear_calls.borrow();
        assert!(clear_calls.is_empty());
    }

    #[test]
    fn start_here_clears_metadata_when_launch_send_fails() {
        let set_calls = Rc::new(RefCell::new(vec![]));
        let clear_calls = Rc::new(RefCell::new(vec![]));
        let command = SpawnAgentCommand {
            harness: Some(AgentStartHarness::Codex),
            name: Some("codex-here".to_string()),
            here: true,
            replace: false,
            split: false,
            pane_id: Some(30),
            new_window: false,
            workspace: None,
            horizontal: false,
            left: false,
            right: false,
            top: false,
            bottom: false,
            cells: None,
            percent: None,
            repo: None,
            worktree: WorktreeMode::None,
            branch: None,
            cwd: None,
            cmd: None,
        };

        let err = promise::spawn::block_on(command.run_with(
            &ConfigHandle::default_config(),
            || async { Ok(ListAgentsResponse { agents: vec![] }) },
            || async {
                Ok(ListPanesResponse {
                    tabs: vec![leaf(10, 20, 30)],
                    tab_titles: vec!["existing".into()],
                    tab_badges: vec![Default::default()],
                    window_titles: HashMap::new(),
                    client_window_view_state: HashMap::new(),
                })
            },
            || async { panic!("list_agents_after_set should not be used on failure") },
            |pane_id| async move { Ok(pane_id.expect("pane id")) },
            |_| async { panic!("spawn_v2 should not be used for --here") },
            |_| async { panic!("split_pane should not be used for --here") },
            |_| async { Err(anyhow::anyhow!("paste failed")) },
            |_| async { panic!("key_down should not be used when send_paste fails") },
            |_| async { panic!("set_tab_title should not be used when send_paste fails") },
            {
                let set_calls = Rc::clone(&set_calls);
                move |request| {
                    set_calls.borrow_mut().push(request);
                    async { Ok(UnitResponse {}) }
                }
            },
            {
                let clear_calls = Rc::clone(&clear_calls);
                move |request| {
                    clear_calls.borrow_mut().push(request);
                    async { Ok(UnitResponse {}) }
                }
            },
            |_| async { panic!("kill_pane should not be used for --here") },
            |cmd, agent_name, agents, current_cwd| {
                cmd.prepare_launch(agent_name, agents, current_cwd)
            },
        ))
        .unwrap_err();

        assert!(err
            .to_string()
            .contains("set agent metadata but failed to send launch command"));
        let set_calls = set_calls.borrow();
        assert_eq!(set_calls.len(), 1);
        let clear_calls = clear_calls.borrow();
        assert_eq!(clear_calls.len(), 1);
        assert_eq!(clear_calls[0].pane_id, 30);
    }

    #[test]
    fn start_here_replace_injects_exec_into_existing_pane() {
        let paste_calls = Rc::new(RefCell::new(vec![]));
        let command = SpawnAgentCommand {
            harness: Some(AgentStartHarness::Codex),
            name: Some("codex-replace".to_string()),
            here: true,
            replace: true,
            split: false,
            pane_id: Some(30),
            new_window: false,
            workspace: None,
            horizontal: false,
            left: false,
            right: false,
            top: false,
            bottom: false,
            cells: None,
            percent: None,
            repo: None,
            worktree: WorktreeMode::None,
            branch: None,
            cwd: Some("/tmp/agent-start".into()),
            cmd: None,
        };

        promise::spawn::block_on(command.run_with(
            &ConfigHandle::default_config(),
            || async { Ok(ListAgentsResponse { agents: vec![] }) },
            || async {
                Ok(ListPanesResponse {
                    tabs: vec![leaf(10, 20, 30)],
                    tab_titles: vec!["existing".into()],
                    tab_badges: vec![Default::default()],
                    window_titles: HashMap::new(),
                    client_window_view_state: HashMap::new(),
                })
            },
            || async {
                Ok(ListAgentsResponse {
                    agents: vec![sample_agent(30, "codex-replace")],
                })
            },
            |pane_id| async move { Ok(pane_id.expect("pane id")) },
            |_| async { panic!("spawn_v2 should not be used for --here") },
            |_| async { panic!("split_pane should not be used for --here") },
            {
                let paste_calls = Rc::clone(&paste_calls);
                move |request| {
                    paste_calls.borrow_mut().push(request);
                    async { Ok(UnitResponse {}) }
                }
            },
            |_| async { Ok(UnitResponse {}) },
            |_| async { Ok(UnitResponse {}) },
            |_| async { Ok(UnitResponse {}) },
            |_| async { Ok(UnitResponse {}) },
            |_| async { panic!("kill_pane should not be used for --here") },
            |cmd, agent_name, agents, current_cwd| {
                cmd.prepare_launch(agent_name, agents, current_cwd)
            },
        ))
        .unwrap();

        let paste_calls = paste_calls.borrow();
        assert_eq!(paste_calls.len(), 1);
        assert_eq!(paste_calls[0].data, "cd /tmp/agent-start && exec codex");
    }
}
