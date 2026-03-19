use crate::cli::{resolve_relative_cwd, CliOutputFormatKind};
use anyhow::{bail, Context};
use chrono::Utc;
use clap::{Parser, Subcommand, ValueHint};
use codec::{InputSerial, ListPanesResponse, SendKeyDown, SpawnV2, TabTitleChanged};
use config::keyassignment::SpawnTabDomain;
use config::ConfigHandle;
use mux::agent::{AgentMetadata, AgentSnapshot};
use mux::pane::PaneId;
use mux::tab::{size_trace_enabled, SplitDirection, SplitRequest, SplitSize};
use mux::window::WindowId;
use portable_pty::cmdbuilder::CommandBuilder;
use serde::Serialize;
use std::ffi::OsString;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use tabout::{tabulate_output, Alignment, Column};
use termwiz::input::{KeyCode, KeyEvent, Modifiers};
use uuid::Uuid;
use wezterm_client::client::Client;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentHarness {
    Unknown,
    Claude,
    Codex,
}

fn infer_harness(cmd: &str) -> AgentHarness {
    let candidate = cmd.to_ascii_lowercase();
    if candidate.contains("claude") {
        AgentHarness::Claude
    } else if candidate.contains("codex") {
        AgentHarness::Codex
    } else {
        AgentHarness::Unknown
    }
}

#[derive(Debug, Parser, Clone)]
pub struct AgentCommand {
    #[command(subcommand)]
    sub: AgentSubCommand,
}

#[derive(Debug, Subcommand, Clone)]
enum AgentSubCommand {
    #[command(name = "start", about = "start a managed agent in the current pane, a split, a new tab, or a new window")]
    Start(SpawnAgentCommand),

    #[command(name = "adopt", about = "adopt an existing pane as a managed agent")]
    Adopt(AdoptAgentCommand),

    #[command(name = "list", about = "list agent-tagged panes")]
    List(ListAgentsCommand),

    #[command(name = "inspect", about = "inspect a single agent by name or id")]
    Inspect(InspectAgentCommand),

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
            AgentSubCommand::Inspect(cmd) => cmd.run(client).await,
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
    tab_size: wezterm_term::TerminalSize,
    cwd: Option<String>,
}

#[derive(Debug, Parser, Clone)]
pub struct SpawnAgentCommand {
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

    /// Specify the current pane or split target. Defaults to WEZTERM_PANE.
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

    /// Command line to launch inside the new pane
    #[arg(long)]
    cmd: String,
}

impl SpawnAgentCommand {
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
        list_agents_after_set: ListAgentsAfterSet,
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
        ListAgentsAfterSet: FnOnce() -> ListAgentsAfterSetFut,
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
        let context_pane_id = if self.here || self.split || self.pane_id.is_some() || !self.new_window {
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
        let agent_name = resolve_spawn_agent_name(&self.cmd, self.name.as_deref(), &agents)?;

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
                &self.cmd,
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

            if size_trace_enabled() {
                log::warn!(
                    "size-trace cli.agent.spawn split={} window_id={:?} pane_id={:?} new_window={} size={:?}",
                    self.split,
                    window_id,
                    context_pane_id,
                    self.new_window,
                    size
                );
            }

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

        list_agents_after_set()
            .await?
            .agents
            .into_iter()
            .find(|agent| agent.pane_id == spawned.pane_id)
            .ok_or_else(|| anyhow::anyhow!("spawned agent but could not reload it from the mux"))
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
        let harness = infer_harness(&self.cmd);
        anyhow::ensure!(
            !matches!(harness, AgentHarness::Unknown),
            "agent start requires a recognized harness command (currently: codex, claude); use agent adopt for generic panes"
        );

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
            command: command_builder_from_cmd(&self.cmd)?,
            launch_cmd: self.cmd.clone(),
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
    if !agents
        .iter()
        .any(|agent| agent.metadata.name == base_name)
    {
        return base_name.to_string();
    }

    let mut suffix = 2usize;
    loop {
        let candidate = format!("{base_name}{suffix}");
        if !agents
            .iter()
            .any(|agent| agent.metadata.name == candidate)
        {
            return candidate;
        }
        suffix += 1;
    }
}

fn resolve_spawn_agent_name(
    cmd: &str,
    requested_name: Option<&str>,
    agents: &[AgentSnapshot],
) -> anyhow::Result<String> {
    if let Some(name) = requested_name {
        ensure_agent_name_available(agents, name)?;
        return Ok(name.to_string());
    }

        let base_name = match infer_harness(cmd) {
            AgentHarness::Codex => "codex",
            AgentHarness::Claude => "claude",
            AgentHarness::Unknown => {
                bail!(
                "agent start requires a recognized harness command (currently: codex, claude); use agent adopt for generic panes"
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
        .join(".wezterm-agents")
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
}

impl ListAgentsCommand {
    async fn run(&self, client: Client) -> anyhow::Result<()> {
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
                        name: "CWD".to_string(),
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
                            agent.metadata.declared_cwd.clone(),
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
pub struct AdoptAgentCommand {
    /// Specify the target pane. Defaults to WEZTERM_PANE.
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
    /// Specify the target pane. Defaults to WEZTERM_PANE.
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

    /// Mark the checkout as being provisioned by wezterm
    #[arg(long, conflicts_with = "unmanaged_checkout")]
    managed_checkout: bool,

    /// Mark the checkout as not being provisioned by wezterm
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
    /// Specify the target pane. Defaults to WEZTERM_PANE.
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

#[cfg(test)]
mod test {
    use super::*;
    use chrono::TimeZone;
    use codec::{ListAgentsResponse, ListPanesResponse, SpawnResponse, UnitResponse};
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
    use tempfile::TempDir;
    use termwiz::surface::{CursorShape, CursorVisibility};
    use wezterm_term::TerminalSize;

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
            cmd: "codex --model gpt-5".to_string(),
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
            cmd: "codex".to_string(),
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
            cmd: "codex".to_string(),
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
            cmd: "codex".to_string(),
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
            cmd: "codex".to_string(),
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
            cmd: "codex".to_string(),
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
            cmd: "zsh".to_string(),
        };

        let err = command.prepare_launch("shell", &[], None).unwrap_err();
        assert!(err
            .to_string()
            .contains("agent start requires a recognized harness command"));
    }

    #[test]
    fn spawn_without_name_uses_harness_base_name() {
        let spawn_calls = Rc::new(RefCell::new(vec![]));
        let title_calls = Rc::new(RefCell::new(vec![]));
        let set_calls = Rc::new(RefCell::new(vec![]));
        let command = SpawnAgentCommand {
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
            cmd: "codex".to_string(),
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
            resolve_spawn_agent_name("codex", None, &agents).unwrap(),
            "codex3"
        );
        assert_eq!(
            resolve_spawn_agent_name("claude", None, &agents).unwrap(),
            "claude"
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
            cmd: "codex".to_string(),
        };

        let agent = promise::spawn::block_on(command.run_with(
            &ConfigHandle::default_config(),
            || async { Ok(ListAgentsResponse { agents: vec![] }) },
            || async {
                Ok(ListPanesResponse {
                    tabs: vec![leaf(10, 20, 30)],
                    tab_titles: vec!["".into()],
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
            cmd: "codex".to_string(),
        };

        let err = promise::spawn::block_on(command.run_with(
            &ConfigHandle::default_config(),
            || async { Ok(ListAgentsResponse { agents: vec![] }) },
            || async {
                Ok(ListPanesResponse {
                    tabs: vec![leaf(10, 20, 30)],
                    tab_titles: vec!["existing".into()],
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
            cmd: "codex".to_string(),
        };

        promise::spawn::block_on(command.run_with(
            &ConfigHandle::default_config(),
            || async { Ok(ListAgentsResponse { agents: vec![] }) },
            || async {
                Ok(ListPanesResponse {
                    tabs: vec![leaf(10, 20, 30)],
                    tab_titles: vec!["existing".into()],
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
