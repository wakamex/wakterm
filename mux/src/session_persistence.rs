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

/// Restore a single tab by recursively walking the PaneNode tree.
///
/// Strategy: spawn the first leaf as the initial pane (creates the tab),
/// then recursively split panes to match the tree structure. At each
/// Split node, the left subtree already exists as the current pane,
/// and the right subtree is created by splitting it.
async fn restore_tab(
    domain: &Arc<dyn crate::domain::Domain>,
    saved_tab: &SavedTab,
    default_size: wezterm_term::TerminalSize,
    window_id: crate::WindowId,
) -> anyhow::Result<()> {
    let first_cwd = first_leaf_cwd(&saved_tab.tree);

    let tab = domain
        .spawn(default_size, None::<CommandBuilder>, first_cwd, window_id)
        .await
        .context("spawning first pane for tab")?;

    tab.set_title(&saved_tab.title);

    // The first leaf is pane index 0. Now recursively split to create
    // the rest of the tree. leaf_index tracks which pane index in the
    // tab's pane list corresponds to the "current" left-side pane.
    let mut leaf_index = 0;
    restore_node(domain, &tab, &saved_tab.tree, &mut leaf_index).await?;

    Ok(())
}

/// Recursively restore a PaneNode subtree.
///
/// For Leaf nodes: nothing to do (already exists as pane at `leaf_index`).
/// For Split nodes: the left subtree is already the pane at `leaf_index`.
///   Split that pane to create the right subtree, then recurse into both.
fn restore_node<'a>(
    domain: &'a Arc<dyn crate::domain::Domain>,
    tab: &'a crate::tab::Tab,
    node: &'a PaneNode,
    leaf_index: &'a mut usize,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + 'a>> {
    Box::pin(async move {
        match node {
            PaneNode::Empty => {}
            PaneNode::Leaf(_) => {
                // This leaf already exists — advance the index
                *leaf_index += 1;
            }
            PaneNode::Split { left, right, node: split_data } => {
                // First, recursively restore the left subtree.
                // After this, all left-side leaves exist in the tab.
                restore_node(domain, tab, left, leaf_index).await?;

                // The pane we need to split is the one just before
                // the current leaf_index (the last leaf of the left subtree)
                let split_pane_index = leaf_index.saturating_sub(1);

                // Spawn a new pane for the right side
                let cwd = first_leaf_cwd(right);
                let pane = domain
                    .spawn_pane(split_data.second, None::<CommandBuilder>, cwd)
                    .await
                    .context("spawning pane for split")?;

                Mux::get().add_pane(&pane)?;

                // Use percentage-based splits so the proportions adapt
                // to the actual tab size at restore time (which may differ
                // from the saved size if the window is a different size).
                let pct = match split_data.direction {
                    crate::tab::SplitDirection::Horizontal => {
                        let total = split_data.first.cols + 1 + split_data.second.cols;
                        if total > 0 {
                            ((split_data.second.cols as u64 * 100) / total as u64) as u8
                        } else {
                            50
                        }
                    }
                    crate::tab::SplitDirection::Vertical => {
                        let total = split_data.first.rows + 1 + split_data.second.rows;
                        if total > 0 {
                            ((split_data.second.rows as u64 * 100) / total as u64) as u8
                        } else {
                            50
                        }
                    }
                };

                let request = crate::tab::SplitRequest {
                    direction: split_data.direction,
                    target_is_second: true,
                    top_level: false,
                    size: crate::tab::SplitSize::Percent(pct.max(5).min(95)),
                };

                if let Err(err) = tab.split_and_insert(split_pane_index, request, pane) {
                    log::warn!(
                        "Failed to split pane {} ({:?}): {:#}",
                        split_pane_index,
                        split_data.direction,
                        err
                    );
                }

                // Now recursively restore the right subtree
                restore_node(domain, tab, right, leaf_index).await?;
            }
        }
        Ok(())
    })
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
