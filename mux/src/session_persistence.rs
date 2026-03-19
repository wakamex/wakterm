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

const SESSION_VERSION: u32 = 4;

fn session_path() -> PathBuf {
    config::RUNTIME_DIR.join("session.json")
}

fn build_saved_session(mux: &Mux) -> SavedSession {
    let mut windows = Vec::new();

    for window_id in mux.iter_windows() {
        if let Some(window) = mux.get_window(window_id) {
            let workspace = window.get_workspace().to_string();
            let mut tabs = Vec::new();
            for tab in window.iter() {
                let title = tab.get_title();
                let mut tree = tab.codec_pane_tree_with_active_pane_id(None);
                mux.annotate_pane_tree_with_agent_metadata(&mut tree);
                // Fix any degenerate splits (< 3 cols/rows on one side)
                // before saving, so the restore produces a usable layout
                heal_tree(&mut tree);
                tabs.push(SavedTab { title, tree });
            }
            if !tabs.is_empty() {
                windows.push(SavedWindow { workspace, tabs });
            }
        }
    }

    SavedSession {
        version: SESSION_VERSION,
        windows,
    }
}

/// Fix degenerate split sizes in a PaneNode tree before saving.
/// If a split has one side < 3 cols or rows, rebalance to 50/50.
/// This prevents broken layouts from being persisted and restored.
fn heal_tree(node: &mut PaneNode) {
    if let PaneNode::Split { left, right, node: split_data } = node {
        let min_dim = 3;
        match split_data.direction {
            crate::tab::SplitDirection::Horizontal => {
                if split_data.first.cols < min_dim || split_data.second.cols < min_dim {
                    let total = split_data.first.cols + 1 + split_data.second.cols;
                    let half = total.saturating_sub(1) / 2;
                    split_data.first.cols = half;
                    split_data.second.cols = total.saturating_sub(1 + half);
                    log::debug!("Healed H-split: {}+1+{} = {}", half, total.saturating_sub(1+half), total);
                }
            }
            crate::tab::SplitDirection::Vertical => {
                if split_data.first.rows < min_dim || split_data.second.rows < min_dim {
                    let total = split_data.first.rows + 1 + split_data.second.rows;
                    let half = total.saturating_sub(1) / 2;
                    split_data.first.rows = half;
                    split_data.second.rows = total.saturating_sub(1 + half);
                    log::debug!("Healed V-split: {}+1+{} = {}", half, total.saturating_sub(1+half), total);
                }
            }
        }
        heal_tree(left);
        heal_tree(right);
    }
}

/// Save the current mux session to disk.
pub fn save_session() -> anyhow::Result<PathBuf> {
    let mux = Mux::try_get().context("no mux instance")?;
    let session = build_saved_session(&mux);

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

    // Use a generous size for spawning so split percentages produce
    // usable pane sizes. The client will resize all tabs to its actual
    // window size on connect. We can't know the client's window size
    // at restore time (the server starts before any client connects).
    let restore_size = {
        let saved = saved_tab.tree.root_size().unwrap_or(default_size);
        // Use the larger of saved size and a minimum (200x60) to ensure
        // splits have room to work
        wezterm_term::TerminalSize {
            rows: saved.rows.max(60),
            cols: saved.cols.max(200),
            pixel_width: saved.pixel_width.max(200 * 10),
            pixel_height: saved.pixel_height.max(60 * 20),
            dpi: saved.dpi,
        }
    };

    let tab = domain
        .spawn(restore_size, None::<CommandBuilder>, first_cwd, window_id)
        .await
        .context("spawning first pane for tab")?;

    tab.set_title(&saved_tab.title);

    // The first leaf is pane index 0. Now recursively split to create
    // the rest of the tree. leaf_index tracks which pane index in the
    // tab's pane list corresponds to the "current" left-side pane.
    let mut leaf_index = 0;
    restore_node(domain, &tab, &saved_tab.tree, &mut leaf_index).await?;

    // Force a resize to reconcile the tree — the splits were created
    // at intermediate sizes and may have accumulated inconsistencies
    // (e.g., column heights not matching across an H-split).
    tab.resize(restore_size);

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
            PaneNode::Leaf(entry) => {
                if let Some(metadata) = entry.agent_metadata.clone() {
                    if let Some(positioned) = tab.iter_panes().get(*leaf_index) {
                        Mux::get()
                            .set_agent_metadata(positioned.pane.pane_id(), metadata)
                            .context("restoring pane agent metadata")?;
                    }
                }
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
                    // Clamp to 10-90% to prevent degenerate splits where
                    // one side gets 1-2 cols/rows
                    size: crate::tab::SplitSize::Percent(pct.max(10).min(90)),
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

#[cfg(test)]
mod test {
    use crate::agent::AgentMetadata;
    use super::*;
    use crate::client::{ClientId, ClientViewId};
    use crate::pane::{alloc_pane_id, CachePolicy, LogicalLine, Pane};
    use crate::renderable::RenderableDimensions;
    use crate::tab::{SplitDirection, SplitRequest, SplitSize, Tab};
    use crate::Mux;
    use rangeset::RangeSet;
    use std::io::Write;
    use std::ops::Range;
    use std::sync::{Arc, Mutex};
    use termwiz::surface::{CursorShape, CursorVisibility, Line, SequenceNo};
    use url::Url;
    use wezterm_term::color::ColorPalette;
    use wezterm_term::{KeyCode, KeyModifiers, MouseEvent, StableRowIndex, TerminalSize};
    use chrono::{TimeZone, Utc};

    struct TestPane {
        id: crate::pane::PaneId,
        size: Mutex<TerminalSize>,
    }

    impl TestPane {
        fn new(id: crate::pane::PaneId, size: TerminalSize) -> Arc<dyn Pane> {
            Arc::new(Self {
                id,
                size: Mutex::new(size),
            })
        }
    }

    impl Pane for TestPane {
        fn pane_id(&self) -> crate::pane::PaneId {
            self.id
        }

        fn get_cursor_position(&self) -> crate::renderable::StableCursorPosition {
            crate::renderable::StableCursorPosition {
                x: 0,
                y: 0,
                shape: CursorShape::Default,
                visibility: CursorVisibility::Visible,
            }
        }

        fn get_current_seqno(&self) -> SequenceNo {
            0
        }

        fn get_changed_since(
            &self,
            _lines: Range<StableRowIndex>,
            _seqno: SequenceNo,
        ) -> RangeSet<StableRowIndex> {
            RangeSet::new()
        }

        fn with_lines_mut(
            &self,
            _stable_range: Range<StableRowIndex>,
            _with_lines: &mut dyn crate::pane::WithPaneLines,
        ) {
            unimplemented!()
        }

        fn for_each_logical_line_in_stable_range_mut(
            &self,
            _lines: Range<StableRowIndex>,
            _for_line: &mut dyn crate::pane::ForEachPaneLogicalLine,
        ) {
            unimplemented!()
        }

        fn get_lines(&self, _lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
            (0, vec![])
        }

        fn get_logical_lines(&self, _lines: Range<StableRowIndex>) -> Vec<LogicalLine> {
            vec![]
        }

        fn get_dimensions(&self) -> RenderableDimensions {
            let size = self.size.lock().unwrap();
            RenderableDimensions {
                cols: size.cols,
                viewport_rows: size.rows,
                scrollback_rows: size.rows,
                physical_top: 0,
                scrollback_top: 0,
                dpi: size.dpi,
                pixel_width: size.pixel_width,
                pixel_height: size.pixel_height,
                reverse_video: false,
            }
        }

        fn get_title(&self) -> String {
            String::new()
        }

        fn send_paste(&self, _text: &str) -> anyhow::Result<()> {
            Ok(())
        }

        fn reader(&self) -> anyhow::Result<Option<Box<dyn std::io::Read + Send>>> {
            Ok(None)
        }

        fn writer(&self) -> parking_lot::MappedMutexGuard<'_, dyn Write> {
            unimplemented!()
        }

        fn resize(&self, size: TerminalSize) -> anyhow::Result<()> {
            *self.size.lock().unwrap() = size;
            Ok(())
        }

        fn key_down(&self, _key: KeyCode, _mods: KeyModifiers) -> anyhow::Result<()> {
            Ok(())
        }

        fn key_up(&self, _key: KeyCode, _mods: KeyModifiers) -> anyhow::Result<()> {
            Ok(())
        }

        fn mouse_event(&self, _event: MouseEvent) -> anyhow::Result<()> {
            Ok(())
        }

        fn is_dead(&self) -> bool {
            false
        }

        fn palette(&self) -> ColorPalette {
            ColorPalette::default()
        }

        fn domain_id(&self) -> crate::domain::DomainId {
            0
        }

        fn is_mouse_grabbed(&self) -> bool {
            false
        }

        fn is_alt_screen_active(&self) -> bool {
            false
        }

        fn get_current_working_dir(&self, _policy: CachePolicy) -> Option<Url> {
            None
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
        }
    }

    #[test]
    fn saved_session_omits_per_client_active_state() {
        let _test_lock = crate::TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let _guard = crate::TestMuxGuard;

        let client_id = Arc::new(ClientId::new());
        let view_id = Arc::new(ClientViewId("save-test".to_string()));
        mux.register_client(client_id.clone(), view_id.clone());
        let _identity = mux.with_identity(Some(client_id));

        let window_id = *mux.new_empty_window(Some("default".to_string()), None);
        let tab_size = size(120, 40);

        let tab = Arc::new(Tab::new(&tab_size));
        let left = TestPane::new(alloc_pane_id(), tab_size);
        let left_pane_id = left.pane_id();
        tab.assign_pane(&left);
        let right = TestPane::new(alloc_pane_id(), tab_size);
        let right_pane_id = right.pane_id();
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                target_is_second: true,
                top_level: false,
                size: SplitSize::Percent(50),
            },
            right,
        )
        .unwrap();
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();
        mux.set_active_tab_for_client_view(view_id.as_ref(), window_id, tab.tab_id())
            .unwrap();
        mux.set_active_pane_for_client_view(view_id.as_ref(), window_id, tab.tab_id(), right_pane_id)
            .unwrap();

        let session = build_saved_session(&mux);
        let json = serde_json::to_string(&session).unwrap();

        assert_eq!(session.version, SESSION_VERSION);
        assert_eq!(session.windows.len(), 1);
        assert!(!json.contains("active_tab_index"));
        assert!(!json.contains("\"is_active_pane\":true"));

        let saved_tree = &session.windows[0].tabs[0].tree;
        let mut leaves = vec![];
        fn collect_leaves<'a>(node: &'a PaneNode, leaves: &mut Vec<&'a crate::tab::PaneEntry>) {
            match node {
                PaneNode::Empty => {}
                PaneNode::Leaf(entry) => leaves.push(entry),
                PaneNode::Split { left, right, .. } => {
                    collect_leaves(left, leaves);
                    collect_leaves(right, leaves);
                }
            }
        }
        collect_leaves(saved_tree, &mut leaves);
        assert_eq!(leaves.len(), 2);
        assert!(leaves.iter().all(|entry| !entry.is_active_pane));
        assert_eq!(
            mux.get_active_pane_id_for_tab_for_client(view_id.as_ref(), window_id, tab.tab_id()),
            Some(right_pane_id)
        );
        assert_ne!(Some(left_pane_id), Some(right_pane_id));
    }

    #[test]
    fn saved_session_includes_agent_metadata_on_leaf_nodes() {
        let _test_lock = crate::TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let _guard = crate::TestMuxGuard;

        let window_id = *mux.new_empty_window(Some("default".to_string()), None);
        let tab_size = size(120, 40);

        let tab = Arc::new(Tab::new(&tab_size));
        let pane = TestPane::new(alloc_pane_id(), tab_size);
        let pane_id = pane.pane_id();
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();
        mux.set_agent_metadata(pane_id, sample_agent_metadata("reviewer"))
            .unwrap();

        let session = build_saved_session(&mux);
        let saved_tree = &session.windows[0].tabs[0].tree;

        match saved_tree {
            PaneNode::Leaf(entry) => {
                assert_eq!(
                    entry
                        .agent_metadata
                        .as_ref()
                        .map(|metadata| metadata.name.as_str()),
                    Some("reviewer")
                );
            }
            other => panic!("expected single leaf, got {:?}", other),
        }
    }
}
