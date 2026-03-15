//! Save and restore mux session state (tab layouts, CWDs, titles).
//!
//! On shutdown (or periodically), saves the current tab layout to a JSON
//! file. On startup, checks for the file and offers to restore.
//!
//! This is similar to tmux-resurrect: it saves the structure but not
//! terminal content. Processes must be relaunched.

use crate::tab::PaneNode;
use crate::Mux;
use anyhow::Context;
use portable_pty::CommandBuilder;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

/// Saved state for one tab.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedTab {
    pub title: String,
    pub tree: PaneNode,
}

/// Saved state for one window (a window contains multiple tabs).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedWindow {
    pub workspace: String,
    pub tabs: Vec<SavedTab>,
}

/// Saved state for the entire mux session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedSession {
    pub version: u32,
    pub windows: Vec<SavedWindow>,
}

const SESSION_VERSION: u32 = 1;

fn session_path() -> PathBuf {
    config::RUNTIME_DIR.join("session.json")
}

/// Save the current mux session to disk.
pub fn save_session() -> anyhow::Result<PathBuf> {
    let mux = Mux::try_get().context("no mux instance")?;
    let mut windows = Vec::new();

    for window_id in mux.iter_windows() {
        if let Some(window) = mux.get_window(window_id) {
            let workspace = window.get_workspace().to_string();
            let mut tabs = Vec::new();
            for tab in window.iter() {
                let title = tab.get_title();
                let tree = tab.codec_pane_tree();
                tabs.push(SavedTab { title, tree });
            }
            if !tabs.is_empty() {
                windows.push(SavedWindow { workspace, tabs });
            }
        }
    }

    let session = SavedSession {
        version: SESSION_VERSION,
        windows,
    };

    let path = session_path();
    let json = serde_json::to_string_pretty(&session)
        .context("serializing session")?;

    std::fs::write(&path, &json)
        .with_context(|| format!("writing session to {}", path.display()))?;

    let total_tabs: usize = session.windows.iter().map(|w| w.tabs.len()).sum();
    log::info!(
        "Saved session: {} windows, {} tabs to {}",
        session.windows.len(),
        total_tabs,
        path.display(),
    );

    Ok(path)
}

/// Load a saved session from disk (if it exists).
pub fn load_session() -> anyhow::Result<Option<SavedSession>> {
    let path = session_path();
    if !path.exists() {
        return Ok(None);
    }

    let json = std::fs::read_to_string(&path)
        .with_context(|| format!("reading session from {}", path.display()))?;

    let session: SavedSession = serde_json::from_str(&json)
        .with_context(|| format!("parsing session from {}", path.display()))?;

    if session.version != SESSION_VERSION {
        log::warn!(
            "Session file version {} != expected {}, ignoring",
            session.version,
            SESSION_VERSION
        );
        return Ok(None);
    }

    let total_tabs: usize = session.windows.iter().map(|w| w.tabs.len()).sum();
    log::info!(
        "Loaded session: {} windows, {} tabs from {}",
        session.windows.len(),
        total_tabs,
        path.display(),
    );

    Ok(Some(session))
}

/// Remove the saved session file (after successful restore or on clean exit).
pub fn clear_session() -> anyhow::Result<()> {
    let path = session_path();
    if path.exists() {
        std::fs::remove_file(&path)
            .with_context(|| format!("removing session file {}", path.display()))?;
    }
    Ok(())
}

/// Restore a saved session by spawning new panes with the saved CWDs
/// and recreating the split tree structure.
///
/// Returns the number of tabs restored.
pub async fn restore_session(
    domain: &Arc<dyn crate::domain::Domain>,
) -> anyhow::Result<usize> {
    let session = match load_session()? {
        Some(s) => s,
        None => return Ok(0),
    };

    let mux = Mux::get();
    let config = config::configuration();
    let default_size = config.initial_size(0, None);
    let mut total_tabs = 0;

    for saved_window in &session.windows {
        let workspace = Some(saved_window.workspace.clone());
        let position = None;
        let window_id = mux.new_empty_window(workspace, position);

        for saved_tab in &saved_window.tabs {
            match restore_tab(domain, &saved_tab, default_size, *window_id).await {
                Ok(()) => {
                    total_tabs += 1;
                }
                Err(err) => {
                    log::error!(
                        "Failed to restore tab '{}': {:#}",
                        saved_tab.title,
                        err
                    );
                }
            }
        }
    }

    log::info!("Restored {} tabs from saved session", total_tabs);

    // Clear the session file after successful restore
    if total_tabs > 0 {
        if let Err(err) = clear_session() {
            log::warn!("Failed to clear session file after restore: {:#}", err);
        }
    }

    Ok(total_tabs)
}

/// Restore a single tab by walking the PaneNode tree.
async fn restore_tab(
    domain: &Arc<dyn crate::domain::Domain>,
    saved_tab: &SavedTab,
    default_size: wezterm_term::TerminalSize,
    window_id: crate::WindowId,
) -> anyhow::Result<()> {
    let mux = Mux::get();

    // Count leaves to know how many panes to spawn
    let leaf_cwds = collect_leaf_cwds(&saved_tab.tree);
    if leaf_cwds.is_empty() {
        return Ok(());
    }

    // Spawn the first pane (creates the tab)
    let first_cwd = leaf_cwds[0].clone();
    let tab = domain
        .spawn(default_size, None::<CommandBuilder>, first_cwd, window_id)
        .await
        .context("spawning first pane for tab")?;

    tab.set_title(&saved_tab.title);

    // For a single-pane tab, we're done
    if leaf_cwds.len() <= 1 {
        return Ok(());
    }

    // For multi-pane tabs, recreate the split structure.
    // Walk the tree and split as needed.
    let split_ops = collect_split_ops(&saved_tab.tree, default_size);

    for op in &split_ops {
        let cwd = op.cwd.clone();
        let pane = domain
            .spawn_pane(op.size, None::<CommandBuilder>, cwd)
            .await
            .context("spawning split pane")?;

        mux.add_pane(&pane).context("adding split pane to mux")?;

        let request = crate::tab::SplitRequest {
            direction: op.direction,
            target_is_second: true,
            top_level: false,
            size: crate::tab::SplitSize::Cells(
                match op.direction {
                    crate::tab::SplitDirection::Horizontal => op.size.cols,
                    crate::tab::SplitDirection::Vertical => op.size.rows,
                }
            ),
        };

        // Find the pane to split (last pane in the tab)
        let panes = tab.iter_panes();
        let split_index = if panes.is_empty() { 0 } else { panes.len() - 1 };

        if let Err(err) = tab.split_and_insert(split_index, request, pane) {
            log::warn!("Failed to split pane in tab '{}': {:#}", saved_tab.title, err);
        }
    }

    Ok(())
}

/// Collect CWDs from all leaf panes in the tree (in preorder).
fn collect_leaf_cwds(node: &PaneNode) -> Vec<Option<String>> {
    let mut cwds = Vec::new();
    match node {
        PaneNode::Empty => {}
        PaneNode::Leaf(entry) => {
            cwds.push(
                entry
                    .working_dir
                    .as_ref()
                    .map(|url| url.url.path().to_string()),
            );
        }
        PaneNode::Split { left, right, .. } => {
            cwds.extend(collect_leaf_cwds(left));
            cwds.extend(collect_leaf_cwds(right));
        }
    }
    cwds
}

/// A split operation to recreate the tab layout.
struct SplitOp {
    direction: crate::tab::SplitDirection,
    size: wezterm_term::TerminalSize,
    cwd: Option<String>,
}

/// Walk the PaneNode tree and collect split operations needed
/// to recreate the layout. Each Split node produces one operation
/// for its right/second child.
fn collect_split_ops(
    node: &PaneNode,
    default_size: wezterm_term::TerminalSize,
) -> Vec<SplitOp> {
    let mut ops = Vec::new();
    match node {
        PaneNode::Empty | PaneNode::Leaf(_) => {}
        PaneNode::Split { left, right, node: split_data } => {
            // Recurse into left first (it's the "existing" side)
            ops.extend(collect_split_ops(left, default_size));

            // The right side needs to be created via a split
            let cwd = first_leaf_cwd(right);
            ops.push(SplitOp {
                direction: split_data.direction,
                size: split_data.second,
                cwd,
            });

            // Then recurse into right for any nested splits
            ops.extend(collect_split_ops(right, default_size));
        }
    }
    ops
}

/// Get the CWD of the first leaf in a subtree.
fn first_leaf_cwd(node: &PaneNode) -> Option<String> {
    match node {
        PaneNode::Empty => None,
        PaneNode::Leaf(entry) => entry
            .working_dir
            .as_ref()
            .map(|url| url.url.path().to_string()),
        PaneNode::Split { left, right, .. } => {
            first_leaf_cwd(left).or_else(|| first_leaf_cwd(right))
        }
    }
}
