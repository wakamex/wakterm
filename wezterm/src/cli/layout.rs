use anyhow::{anyhow, Context};
use clap::{Parser, ValueHint};
use codec::{
    ListPanesResponse, SetPaneZoomed, SpawnV2, SplitPane as SplitPanePdu, TabTitleChanged,
    WindowTitleChanged,
};
use config::keyassignment::SpawnTabDomain;
use mux::agent::AgentMetadata;
use mux::pane::PaneId;
use mux::tab::{PaneEntry, PaneNode, SerdeUrl, SplitDirection, SplitRequest, SplitSize, TabId};
use mux::window::WindowId;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use wezterm_client::client::Client;
use wezterm_term::TerminalSize;

const LAYOUT_VERSION: usize = 4;

fn default_layout_path() -> PathBuf {
    config::CONFIG_DIRS
        .first()
        .cloned()
        .unwrap_or_else(|| config::HOME_DIR.join(".config").join("wezterm"))
        .join("layout.json")
}

#[derive(Debug, Parser, Clone)]
pub struct SaveLayout {
    /// Output file (default: ~/.config/wezterm/layout.json)
    #[arg(value_hint = ValueHint::FilePath)]
    file: Option<PathBuf>,

    /// Print to stdout instead of writing a file
    #[arg(long)]
    stdout: bool,
}

#[derive(Debug, Parser, Clone)]
pub struct RestoreLayout {
    /// Input file (default: ~/.config/wezterm/layout.json)
    #[arg(value_hint = ValueHint::FilePath)]
    file: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct SavedLayout {
    version: usize,
    windows: Vec<SavedWindow>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct SavedWindow {
    title: String,
    workspace: String,
    active_tab_index: usize,
    tabs: Vec<SavedTab>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct SavedTab {
    title: String,
    size: TerminalSize,
    root: SavedPaneTree,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum SavedPaneTree {
    Split {
        direction: SplitDirection,
        second_cells: usize,
        first: Box<SavedPaneTree>,
        second: Box<SavedPaneTree>,
    },
    Pane {
        cwd: Option<String>,
        agent_metadata: Option<AgentMetadata>,
        is_active: bool,
        is_zoomed: bool,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct RestoredPaneState {
    active_pane_id: Option<PaneId>,
    zoomed_pane_id: Option<PaneId>,
}

impl RestoredPaneState {
    fn merge(self, other: Self) -> Self {
        Self {
            active_pane_id: self.active_pane_id.or(other.active_pane_id),
            zoomed_pane_id: self.zoomed_pane_id.or(other.zoomed_pane_id),
        }
    }
}

impl SavedPaneTree {
    fn from_pane_node(node: PaneNode) -> anyhow::Result<Self> {
        match node {
            PaneNode::Empty => Err(anyhow!("cannot save empty pane tree")),
            PaneNode::Leaf(entry) => Ok(Self::Pane {
                cwd: entry.working_dir.map(url_to_path_string),
                agent_metadata: entry.agent_metadata,
                is_active: entry.is_active_pane,
                is_zoomed: entry.is_zoomed_pane,
            }),
            PaneNode::Split { left, right, node } => Ok(Self::Split {
                direction: node.direction,
                second_cells: match node.direction {
                    SplitDirection::Horizontal => node.second.cols,
                    SplitDirection::Vertical => node.second.rows,
                },
                first: Box::new(Self::from_pane_node(*left)?),
                second: Box::new(Self::from_pane_node(*right)?),
            }),
        }
    }

    fn first_leaf(&self) -> &Self {
        match self {
            Self::Split { first, .. } => first.first_leaf(),
            Self::Pane { .. } => self,
        }
    }

    fn first_leaf_cwd(&self) -> Option<&str> {
        match self.first_leaf() {
            Self::Pane { cwd, .. } => cwd.as_deref(),
            Self::Split { .. } => None,
        }
    }

    fn contains_active(&self) -> bool {
        match self {
            Self::Pane { is_active, .. } => *is_active,
            Self::Split { first, second, .. } => {
                first.contains_active() || second.contains_active()
            }
        }
    }
}

impl SavedLayout {
    fn from_list_panes(response: ListPanesResponse) -> anyhow::Result<Self> {
        let ListPanesResponse {
            tabs,
            tab_titles,
            window_titles,
            client_window_view_state,
        } = response;

        let mut windows = Vec::new();
        let mut current_window_id: Option<WindowId> = None;

        for (idx, tabroot) in tabs.into_iter().enumerate() {
            let (window_id, tab_id) = tabroot
                .window_and_tab_ids()
                .ok_or_else(|| anyhow!("missing window/tab id for pane tree"))?;
            let size = tabroot
                .root_size()
                .ok_or_else(|| anyhow!("missing root size for tab in window {}", window_id))?;
            let workspace = first_entry(&tabroot)
                .ok_or_else(|| anyhow!("missing first leaf for tab in window {}", window_id))?;
            let workspace = workspace.workspace.clone();
            let tab_title = tab_titles.get(idx).cloned().unwrap_or_default();
            let saved_tab = SavedTab {
                title: tab_title,
                size,
                root: SavedPaneTree::from_pane_node(tabroot)?,
            };

            if current_window_id != Some(window_id) {
                windows.push(SavedWindow {
                    title: window_titles.get(&window_id).cloned().unwrap_or_default(),
                    workspace,
                    active_tab_index: 0,
                    tabs: vec![saved_tab],
                });
                if client_window_view_state
                    .get(&window_id)
                    .and_then(|window_state| window_state.active_tab_id)
                    == Some(tab_id)
                {
                    windows
                        .last_mut()
                        .expect("window was just pushed")
                        .active_tab_index = 0;
                }
                current_window_id = Some(window_id);
            } else if let Some(window) = windows.last_mut() {
                if client_window_view_state
                    .get(&window_id)
                    .and_then(|window_state| window_state.active_tab_id)
                    == Some(tab_id)
                {
                    window.active_tab_index = window.tabs.len();
                }
                window.tabs.push(saved_tab);
            }
        }

        Ok(Self {
            version: LAYOUT_VERSION,
            windows,
        })
    }
}

impl SaveLayout {
    pub async fn run(self, client: Client) -> anyhow::Result<()> {
        let layout = SavedLayout::from_list_panes(client.list_panes().await?)?;
        let serialized = serde_json::to_string_pretty(&layout)? + "\n";

        if self.stdout {
            print!("{serialized}");
            return Ok(());
        }

        let path = self.file.unwrap_or_else(default_layout_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        fs::write(&path, serialized).with_context(|| format!("writing {}", path.display()))?;

        let tab_count = layout
            .windows
            .iter()
            .map(|window| window.tabs.len())
            .sum::<usize>();
        let pane_count = layout
            .windows
            .iter()
            .flat_map(|window| &window.tabs)
            .map(|tab| count_leaves(&tab.root))
            .sum::<usize>();
        eprintln!(
            "Saved {} windows, {} tabs, {} panes -> {}",
            layout.windows.len(),
            tab_count,
            pane_count,
            path.display()
        );
        Ok(())
    }
}

impl RestoreLayout {
    pub async fn run(self, client: Client) -> anyhow::Result<()> {
        let path = self.file.unwrap_or_else(default_layout_path);
        let layout: SavedLayout = if path == Path::new("-") {
            serde_json::from_reader(std::io::stdin().lock()).context("parsing layout from stdin")?
        } else {
            serde_json::from_slice(
                &fs::read(&path).with_context(|| format!("reading {}", path.display()))?,
            )
            .with_context(|| format!("parsing {}", path.display()))?
        };

        if layout.version != LAYOUT_VERSION {
            return Err(anyhow!(
                "unsupported layout version {}; expected {}",
                layout.version,
                LAYOUT_VERSION
            ));
        }

        for window in layout.windows {
            restore_window(&client, window).await?;
        }

        Ok(())
    }
}

async fn restore_window(client: &Client, window: SavedWindow) -> anyhow::Result<()> {
    let mut window_id = None;
    let mut restored_active_window_pane_id = None;

    if !window.tabs.is_empty() && window.active_tab_index >= window.tabs.len() {
        anyhow::bail!(
            "window '{}' has invalid active_tab_index {} for {} tabs",
            window.title,
            window.active_tab_index,
            window.tabs.len()
        );
    }

    let mut source_pane_id = None;
    for (tab_idx, tab) in window.tabs.iter().enumerate() {
        let command_dir = saved_command_dir(tab.root.first_leaf_cwd());

        let spawned = client
            .spawn_v2(SpawnV2 {
                domain: SpawnTabDomain::DefaultDomain,
                window_id,
                current_pane_id: source_pane_id,
                command: None,
                command_dir,
                size: tab.size,
                workspace: window.workspace.clone(),
            })
            .await?;

        window_id.get_or_insert(spawned.window_id);
        source_pane_id = Some(spawned.pane_id);

        if !window.title.is_empty() {
            client
                .set_window_title(WindowTitleChanged {
                    window_id: spawned.window_id,
                    title: window.title.clone(),
                })
                .await?;
        }

        if !tab.title.is_empty() {
            client
                .set_tab_title(TabTitleChanged {
                    tab_id: spawned.tab_id,
                    title: tab.title.clone(),
                })
                .await?;
        }

        let restored =
            restore_tree(client, spawned.tab_id, tab.size, spawned.pane_id, &tab.root).await?;

        if let Some(zoomed_pane_id) = restored.zoomed_pane_id {
            client
                .set_zoomed(SetPaneZoomed {
                    containing_tab_id: spawned.tab_id,
                    pane_id: zoomed_pane_id,
                    zoomed: true,
                })
                .await?;
        }

        if tab_idx == window.active_tab_index {
            restored_active_window_pane_id = restored.active_pane_id;
        }
    }

    if let Some(active_pane_id) = restored_active_window_pane_id {
        client
            .set_focused_pane_id(codec::SetFocusedPane {
                pane_id: active_pane_id,
            })
            .await?;
    }

    Ok(())
}

fn restore_tree<'a>(
    client: &'a Client,
    _tab_id: TabId,
    tab_size: TerminalSize,
    pane_id: PaneId,
    tree: &'a SavedPaneTree,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<RestoredPaneState>> + 'a>> {
    Box::pin(async move {
        match tree {
            SavedPaneTree::Pane {
                agent_metadata,
                is_active,
                is_zoomed,
                ..
            } => {
                if let Some(metadata) = agent_metadata.clone() {
                    client
                        .set_agent_metadata(codec::SetAgentMetadata { pane_id, metadata })
                        .await?;
                }
                Ok(RestoredPaneState {
                    active_pane_id: if *is_active { Some(pane_id) } else { None },
                    zoomed_pane_id: if *is_zoomed { Some(pane_id) } else { None },
                })
            }
            SavedPaneTree::Split {
                direction,
                second_cells,
                first,
                second,
            } => {
                let split = client
                    .split_pane(SplitPanePdu {
                        pane_id,
                        split_request: SplitRequest {
                            direction: *direction,
                            target_is_second: true,
                            size: SplitSize::Cells(*second_cells),
                            top_level: false,
                        },
                        command: None,
                        command_dir: saved_command_dir(second.first_leaf_cwd()),
                        domain: SpawnTabDomain::CurrentPaneDomain,
                        move_pane_id: None,
                        tab_size: Some(tab_size),
                    })
                    .await?;

                // Replay the subtree containing the saved active pane last, because
                // split-and-insert makes the newly created branch active.
                let (first_state, second_state) = if first.contains_active() {
                    let second_state =
                        restore_tree(client, _tab_id, tab_size, split.pane_id, second).await?;
                    let first_state =
                        restore_tree(client, _tab_id, tab_size, pane_id, first).await?;
                    (first_state, second_state)
                } else {
                    let first_state =
                        restore_tree(client, _tab_id, tab_size, pane_id, first).await?;
                    let second_state =
                        restore_tree(client, _tab_id, tab_size, split.pane_id, second).await?;
                    (first_state, second_state)
                };

                Ok(first_state.merge(second_state))
            }
        }
    })
}

fn first_entry(node: &PaneNode) -> Option<&PaneEntry> {
    match node {
        PaneNode::Empty => None,
        PaneNode::Leaf(entry) => Some(entry),
        PaneNode::Split { left, right, .. } => first_entry(left).or_else(|| first_entry(right)),
    }
}

fn count_leaves(tree: &SavedPaneTree) -> usize {
    match tree {
        SavedPaneTree::Pane { .. } => 1,
        SavedPaneTree::Split { first, second, .. } => count_leaves(first) + count_leaves(second),
    }
}

fn saved_command_dir(cwd: Option<&str>) -> Option<String> {
    cwd.map(ToOwned::to_owned)
}

fn url_to_path_string(url: SerdeUrl) -> String {
    if url.url.scheme() == "file" {
        return url.url.path().to_string();
    }
    url.url.as_str().to_string()
}

#[cfg(test)]
mod test {
    use super::*;
    use chrono::{TimeZone, Utc};
    use mux::agent::AgentMetadata;
    use mux::renderable::StableCursorPosition;
    use mux::tab::{PaneEntry, PaneNode, SplitDirectionAndSize};
    use termwiz::surface::{CursorShape, CursorVisibility};

    fn size(cols: usize, rows: usize) -> TerminalSize {
        TerminalSize {
            cols,
            rows,
            pixel_width: cols * 12,
            pixel_height: rows * 24,
            dpi: 144,
        }
    }

    fn leaf(
        window_id: WindowId,
        tab_id: TabId,
        pane_id: PaneId,
        left_col: usize,
        top_row: usize,
        pane_size: TerminalSize,
        cwd: &str,
        is_active_pane: bool,
        is_zoomed_pane: bool,
    ) -> PaneNode {
        PaneNode::Leaf(PaneEntry {
            window_id,
            tab_id,
            pane_id,
            agent_metadata: None,
            title: String::new(),
            size: pane_size,
            working_dir: Some(SerdeUrl {
                url: url::Url::from_file_path(cwd).unwrap(),
            }),
            is_active_pane,
            is_zoomed_pane,
            workspace: "default".to_string(),
            cursor_pos: StableCursorPosition {
                x: 0,
                y: 0,
                shape: CursorShape::Default,
                visibility: CursorVisibility::Visible,
            },
            physical_top: 0,
            top_row,
            left_col,
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

    fn sample_agent_metadata(name: &str) -> AgentMetadata {
        AgentMetadata {
            agent_id: format!("agent-{name}"),
            name: name.to_string(),
            launch_cmd: "codex".to_string(),
            declared_cwd: format!("/tmp/{name}"),
            created_at: Utc.with_ymd_and_hms(2026, 3, 17, 12, 0, 0).unwrap(),
            repo_root: None,
            worktree: None,
            branch: None,
            managed_checkout: false,
        }
    }

    #[test]
    fn converts_l_shaped_tree_to_saved_layout() {
        let left = leaf(1, 2, 10, 0, 0, size(124, 68), "/tmp/left", false, false);
        let top = leaf(1, 2, 11, 125, 0, size(125, 33), "/tmp/top", false, false);
        let bottom = leaf(1, 2, 12, 125, 34, size(125, 34), "/tmp/bottom", true, true);
        let right = split(
            top,
            bottom,
            SplitDirectionAndSize {
                direction: SplitDirection::Vertical,
                first: size(125, 33),
                second: size(125, 34),
            },
        );
        let root = split(
            left,
            right,
            SplitDirectionAndSize {
                direction: SplitDirection::Horizontal,
                first: size(124, 68),
                second: size(125, 68),
            },
        );

        let saved = SavedPaneTree::from_pane_node(root).unwrap();
        assert!(saved.contains_active());
        assert_eq!(saved.first_leaf_cwd(), Some("/tmp/left"));

        match saved {
            SavedPaneTree::Split {
                direction,
                second_cells,
                ..
            } => {
                assert_eq!(direction, SplitDirection::Horizontal);
                assert_eq!(second_cells, 125);
            }
            other => panic!("expected split, got {:?}", other),
        }
    }

    #[test]
    fn layout_groups_tabs_by_window() {
        let tab0 = leaf(1, 2, 10, 0, 0, size(250, 68), "/tmp/a", true, false);
        let tab1 = leaf(1, 3, 11, 0, 0, size(250, 68), "/tmp/b", true, false);
        let tab2 = leaf(9, 4, 12, 0, 0, size(250, 68), "/tmp/c", true, false);
        let response = ListPanesResponse {
            tabs: vec![tab0, tab1, tab2],
            tab_titles: vec!["one".into(), "two".into(), "three".into()],
            window_titles: std::collections::HashMap::from([
                (1, "win-a".into()),
                (9, "win-b".into()),
            ]),
            client_window_view_state: std::collections::HashMap::from([
                (
                    1,
                    mux::client::ClientWindowViewState {
                        active_tab_id: Some(3),
                        ..Default::default()
                    },
                ),
                (
                    9,
                    mux::client::ClientWindowViewState {
                        active_tab_id: Some(4),
                        ..Default::default()
                    },
                ),
            ]),
        };

        let layout = SavedLayout::from_list_panes(response).unwrap();
        assert_eq!(layout.version, LAYOUT_VERSION);
        assert_eq!(layout.windows.len(), 2);
        assert_eq!(layout.windows[0].title, "win-a");
        assert_eq!(layout.windows[0].active_tab_index, 1);
        assert_eq!(layout.windows[0].tabs.len(), 2);
        assert_eq!(layout.windows[1].title, "win-b");
        assert_eq!(layout.windows[1].active_tab_index, 0);
        assert_eq!(layout.windows[1].tabs.len(), 1);
    }

    #[test]
    fn file_urls_are_saved_as_paths() {
        let path = url_to_path_string(SerdeUrl {
            url: url::Url::parse("file://fedora/code/wezterm").unwrap(),
        });
        assert_eq!(path, "/code/wezterm");
    }

    #[test]
    fn saved_layout_preserves_agent_metadata_on_leaf() {
        let mut leaf = leaf(1, 2, 10, 0, 0, size(80, 24), "/tmp/agent", true, false);
        if let PaneNode::Leaf(entry) = &mut leaf {
            entry.agent_metadata = Some(sample_agent_metadata("reviewer"));
        }

        let saved = SavedPaneTree::from_pane_node(leaf).unwrap();
        match saved {
            SavedPaneTree::Pane {
                cwd,
                agent_metadata,
                is_active,
                is_zoomed,
            } => {
                assert_eq!(cwd.as_deref(), Some("/tmp/agent"));
                assert_eq!(
                    agent_metadata
                        .as_ref()
                        .map(|metadata| metadata.name.as_str()),
                    Some("reviewer")
                );
                assert!(is_active);
                assert!(!is_zoomed);
            }
            other => panic!("expected pane, got {:?}", other),
        }
    }
}
