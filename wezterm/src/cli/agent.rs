use crate::cli::CliOutputFormatKind;
use anyhow::Context;
use chrono::Utc;
use clap::{Parser, Subcommand};
use codec::ListPanesResponse;
use mux::agent::{AgentMetadata, AgentSnapshot};
use mux::pane::PaneId;
use serde::Serialize;
use tabout::{tabulate_output, Alignment, Column};
use uuid::Uuid;
use wezterm_client::client::Client;

#[derive(Debug, Parser, Clone)]
pub struct AgentCommand {
    #[command(subcommand)]
    sub: AgentSubCommand,
}

#[derive(Debug, Subcommand, Clone)]
enum AgentSubCommand {
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
    pub async fn run(&self, client: Client) -> anyhow::Result<()> {
        match &self.sub {
            AgentSubCommand::List(cmd) => cmd.run(client).await,
            AgentSubCommand::Inspect(cmd) => cmd.run(client).await,
            AgentSubCommand::Set(cmd) => cmd.run(client).await,
            AgentSubCommand::Clear(cmd) => cmd.run(client).await,
        }
    }
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

        let metadata = AgentMetadata {
            agent_id: existing
                .map(|agent| agent.metadata.agent_id.clone())
                .unwrap_or_else(|| Uuid::new_v4().to_string()),
            name: self.name.clone(),
            launch_cmd: self
                .launch_cmd
                .clone()
                .or_else(|| existing.map(|agent| agent.metadata.launch_cmd.clone()))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "--launch-cmd is required when tagging a pane for the first time"
                    )
                })?,
            declared_cwd: self
                .cwd
                .clone()
                .or_else(|| existing.map(|agent| agent.metadata.declared_cwd.clone()))
                .or_else(|| find_pane_cwd(&panes, pane_id))
                .ok_or_else(|| anyhow::anyhow!("unable to determine cwd; pass --cwd"))?,
            created_at: existing
                .map(|agent| agent.metadata.created_at)
                .unwrap_or_else(Utc::now),
            repo_root: self
                .repo_root
                .clone()
                .or_else(|| existing.and_then(|agent| agent.metadata.repo_root.clone())),
            worktree: self
                .worktree
                .clone()
                .or_else(|| existing.and_then(|agent| agent.metadata.worktree.clone())),
            branch: self
                .branch
                .clone()
                .or_else(|| existing.and_then(|agent| agent.metadata.branch.clone())),
        };

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
                    return entry
                        .working_dir
                        .as_ref()
                        .map(|url| url.url.as_str().to_string());
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
    use codec::{ListAgentsResponse, ListPanesResponse, UnitResponse};
    use mux::agent::AgentMetadata;
    use mux::client::ClientWindowViewState;
    use mux::renderable::StableCursorPosition;
    use mux::tab::{PaneEntry, PaneNode, SerdeUrl};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::convert::TryFrom;
    use std::rc::Rc;
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

    fn leaf(window_id: mux::window::WindowId, tab_id: mux::tab::TabId, pane_id: PaneId) -> PaneNode {
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

    fn panes_response(panes: Vec<PaneNode>) -> ListPanesResponse {
        ListPanesResponse {
            tabs: panes,
            tab_titles: vec!["tab".to_string()],
            window_titles: HashMap::new(),
            client_window_view_state: HashMap::<mux::window::WindowId, ClientWindowViewState>::new(),
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
            },
            pane_id,
            tab_id: 20,
            window_id: 10,
            workspace: "default".to_string(),
            domain_id: 1,
        }
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
        };
        let existing = sample_agent(30, "old-name");
        promise::spawn::block_on(command.run_with(
            || async { Ok(ListAgentsResponse { agents: vec![existing.clone()] }) },
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
        assert_eq!(call[0].metadata.declared_cwd, existing.metadata.declared_cwd);
        assert_eq!(call[0].metadata.name, "reviewer");
        assert_eq!(call[0].metadata.repo_root.as_deref(), Some("/repo"));
        assert_eq!(call[0].metadata.branch.as_deref(), Some("agent/reviewer"));
    }
}
