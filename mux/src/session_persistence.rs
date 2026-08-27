//! Save and restore mux session state (tab layouts, CWDs, titles).
//!
//! On shutdown (or periodically), saves the current tab layout to a durable
//! JSON file. On startup, checks for the file and restores it.
//!
//! This is similar to tmux-resurrect: it saves the structure but not
//! terminal content. Processes must be relaunched.

use crate::agent::{infer_harness, native_resume_command, AgentHarness, AgentMetadata};
use crate::codex_app_server::{PrepareCodexLaunch, PreparedCodexLaunch};
use crate::pane::PaneId;
use crate::tab::PaneNode;
use crate::Mux;
use anyhow::{anyhow, Context};
use portable_pty::CommandBuilder;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

/// Saved state for one tab.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedTab {
    pub title: String,
    pub tree: PaneNode,
    #[serde(default)]
    pub parked: bool,
}

/// Saved state for one window (a window contains multiple tabs).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedWindow {
    pub workspace: String,
    pub tabs: Vec<SavedTab>,
}

/// A provider session that can be resumed after the mux process restarts.
///
/// The pane metadata is kept separately from the layout tree because restored
/// panes get new IDs. The old pane ID is only the key used while walking the
/// saved tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedAgentRestoreIntent {
    pub pane_id: PaneId,
    #[serde(default)]
    pub harness: AgentHarness,
    pub metadata: AgentMetadata,
    pub session_id: String,
    #[serde(default)]
    pub attention_seen_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl SavedAgentRestoreIntent {
    fn harness(&self) -> AgentHarness {
        match self.harness {
            AgentHarness::Unknown => infer_harness(&self.metadata.launch_cmd, None),
            ref harness => harness.clone(),
        }
    }
}

struct PreparedAgentRestore {
    command: CommandBuilder,
    intent: SavedAgentRestoreIntent,
}

/// Saved state for the entire mux session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedSession {
    pub version: u32,
    pub windows: Vec<SavedWindow>,
    #[serde(default)]
    pub agent_restore_intents: Vec<SavedAgentRestoreIntent>,
}

const SESSION_VERSION: u32 = 5;
const AUTO_SAVE_DEBOUNCE: Duration = Duration::from_millis(500);
const AUTO_SAVE_INTERVAL: Duration = Duration::from_secs(60);

static AUTO_SAVE_TX: OnceLock<SyncSender<()>> = OnceLock::new();
static SAVE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
static LAST_COMMITTED_SESSION: LazyLock<Mutex<Option<Vec<u8>>>> =
    LazyLock::new(|| Mutex::new(None));
static SHUTDOWN_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

pub fn begin_shutdown() {
    SHUTDOWN_IN_PROGRESS.store(true, Ordering::Release);
}

pub fn shutdown_in_progress() -> bool {
    SHUTDOWN_IN_PROGRESS.load(Ordering::Acquire)
}

#[cfg(test)]
fn end_shutdown_for_test() {
    SHUTDOWN_IN_PROGRESS.store(false, Ordering::Release);
}

struct AutoSaveSchedule {
    debounce: Duration,
    interval: Duration,
    dirty_deadline: Option<Instant>,
    periodic_deadline: Instant,
}

impl AutoSaveSchedule {
    fn new(now: Instant, debounce: Duration, interval: Duration) -> Self {
        Self {
            debounce,
            interval,
            dirty_deadline: None,
            periodic_deadline: now + interval,
        }
    }

    fn mark_dirty(&mut self, now: Instant) {
        self.dirty_deadline = Some(now + self.debounce);
    }

    fn next_deadline(&self) -> Instant {
        self.dirty_deadline
            .map(|deadline| deadline.min(self.periodic_deadline))
            .unwrap_or(self.periodic_deadline)
    }

    fn take_save_due(&mut self, now: Instant) -> bool {
        if now >= self.periodic_deadline {
            while self.periodic_deadline <= now {
                self.periodic_deadline += self.interval;
            }
            self.dirty_deadline = None;
            return true;
        }

        if self.dirty_deadline.is_some_and(|deadline| now >= deadline) {
            self.dirty_deadline = None;
            return true;
        }

        false
    }
}

/// Mark recovery-relevant mux state dirty. Calls before auto-save starts are
/// intentionally ignored so startup restoration cannot overwrite its source.
pub fn request_session_save() {
    if let Some(tx) = AUTO_SAVE_TX.get() {
        let _ = tx.try_send(());
    }
}

/// Save the fully restored startup state, then start one coalescing owner for
/// event-driven saves and the periodic reconciliation save.
pub fn start_auto_save() {
    if AUTO_SAVE_TX.get().is_some() {
        return;
    }

    if let Err(err) = save_session() {
        log::warn!("initial session auto-save: {err:#}");
    }

    let (tx, rx) = sync_channel(1);
    if AUTO_SAVE_TX.set(tx).is_err() {
        return;
    }

    thread::Builder::new()
        .name("session-auto-save".to_string())
        .spawn(move || run_auto_save(rx))
        .expect("failed to spawn session auto-save thread");
}

fn run_auto_save(rx: Receiver<()>) {
    let mut schedule =
        AutoSaveSchedule::new(Instant::now(), AUTO_SAVE_DEBOUNCE, AUTO_SAVE_INTERVAL);

    loop {
        let now = Instant::now();
        let timeout = schedule.next_deadline().saturating_duration_since(now);
        match rx.recv_timeout(timeout) {
            Ok(()) => schedule.mark_dirty(Instant::now()),
            Err(RecvTimeoutError::Timeout) => {
                if schedule.take_save_due(Instant::now()) {
                    if let Err(err) = save_session() {
                        log::debug!("auto-save session: {err:#}");
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn session_path() -> PathBuf {
    config::DATA_DIR.join("session.json")
}

fn previous_session_path() -> PathBuf {
    config::DATA_DIR.join("session.json.prev")
}

fn legacy_session_path() -> PathBuf {
    config::RUNTIME_DIR.join("session.json")
}

fn build_saved_session(mux: &Mux) -> SavedSession {
    let mut windows = Vec::new();
    let mut agent_restore_intents = Vec::new();
    let mut window_ids = mux.iter_windows();
    window_ids.sort();

    // Collect workspace names and tab Arcs while holding the windows read
    // lock, then release it before locking tab.inner. This avoids a
    // lock-ordering deadlock: if a main-thread future holds tab.inner and
    // tries windows.write(), it would block on our windows.read(), while
    // we block on its tab.inner. (#7661 pattern)
    let window_data: Vec<(String, Vec<_>)> = window_ids
        .iter()
        .filter_map(|&window_id| {
            let window = mux.get_window(window_id)?;
            let workspace = window.get_workspace().to_string();
            let tabs: Vec<_> = window
                .iter()
                .map(|tab| (Arc::clone(tab), window.is_tab_parked(tab.tab_id())))
                .collect();
            Some((workspace, tabs))
        })
        .collect();

    for (workspace, tabs) in window_data {
        let mut saved_tabs = Vec::new();
        for (tab, parked) in tabs {
            let title = mux.raw_tab_title(tab.tab_id());
            let mut tree = tab.codec_pane_tree_with_active_pane_id(None);
            mux.annotate_pane_tree_with_agent_metadata(&mut tree);
            collect_agent_restore_intents(mux, &tree, &mut agent_restore_intents);
            // Fix any degenerate splits (< 3 cols/rows on one side)
            // before saving, so the restore produces a usable layout
            heal_tree(&mut tree);
            saved_tabs.push(SavedTab {
                title,
                tree,
                parked,
            });
        }
        if !saved_tabs.is_empty() {
            windows.push(SavedWindow {
                workspace,
                tabs: saved_tabs,
            });
        }
    }

    SavedSession {
        version: SESSION_VERSION,
        windows,
        agent_restore_intents,
    }
}

fn collect_agent_restore_intents(
    mux: &Mux,
    node: &PaneNode,
    intents: &mut Vec<SavedAgentRestoreIntent>,
) {
    match node {
        PaneNode::Empty => {}
        PaneNode::Leaf(entry) => {
            if let Some((harness, metadata, session_id)) =
                mux.agent_restore_intent_for_pane(entry.pane_id)
            {
                intents.push(SavedAgentRestoreIntent {
                    pane_id: entry.pane_id,
                    harness,
                    metadata,
                    session_id,
                    attention_seen_at: mux.agent_attention_seen_at(entry.pane_id),
                });
            }
        }
        PaneNode::Split { left, right, .. } => {
            collect_agent_restore_intents(mux, left, intents);
            collect_agent_restore_intents(mux, right, intents);
        }
    }
}

/// Fix degenerate split sizes in a PaneNode tree before saving.
/// If a split has one side < 3 cols or rows, rebalance to 50/50.
/// This prevents broken layouts from being persisted and restored.
fn heal_tree(node: &mut PaneNode) {
    if let PaneNode::Split {
        left,
        right,
        node: split_data,
    } = node
    {
        let min_dim = 3;
        match split_data.direction {
            crate::tab::SplitDirection::Horizontal => {
                if split_data.first.cols < min_dim || split_data.second.cols < min_dim {
                    let total = split_data.first.cols + 1 + split_data.second.cols;
                    let half = total.saturating_sub(1) / 2;
                    split_data.first.cols = half;
                    split_data.second.cols = total.saturating_sub(1 + half);
                    log::debug!(
                        "Healed H-split: {}+1+{} = {}",
                        half,
                        total.saturating_sub(1 + half),
                        total
                    );
                }
            }
            crate::tab::SplitDirection::Vertical => {
                if split_data.first.rows < min_dim || split_data.second.rows < min_dim {
                    let total = split_data.first.rows + 1 + split_data.second.rows;
                    let half = total.saturating_sub(1) / 2;
                    split_data.first.rows = half;
                    split_data.second.rows = total.saturating_sub(1 + half);
                    log::debug!(
                        "Healed V-split: {}+1+{} = {}",
                        half,
                        total.saturating_sub(1 + half),
                        total
                    );
                }
            }
        }
        heal_tree(left);
        heal_tree(right);
    }
}

/// Save the current mux session to disk.
pub fn save_session() -> anyhow::Result<PathBuf> {
    let _save_guard = SAVE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mux = Mux::try_get().context("no mux instance")?;
    let session = build_saved_session(&mux);

    let path = session_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating session directory {}", parent.display()))?;
    }
    let json = serde_json::to_vec_pretty(&session).context("serializing session")?;
    let mut last_committed = LAST_COMMITTED_SESSION
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !commit_serialized_session(&path, &previous_session_path(), &json, &mut last_committed)? {
        log::debug!("Session unchanged: {}", path.display());
        return Ok(path);
    }

    let total_tabs: usize = session.windows.iter().map(|w| w.tabs.len()).sum();
    log::info!(
        "Saved session: {} windows, {} tabs to {}",
        session.windows.len(),
        total_tabs,
        path.display(),
    );

    Ok(path)
}

fn commit_serialized_session(
    path: &std::path::Path,
    previous_path: &std::path::Path,
    bytes: &[u8],
    last_committed: &mut Option<Vec<u8>>,
) -> anyhow::Result<bool> {
    if last_committed.is_none() {
        *last_committed = read_valid_session_bytes(path)?;
    }
    if last_committed.as_deref() == Some(bytes) {
        return Ok(false);
    }
    if let Some(previous) = last_committed.as_deref() {
        write_atomic_file(previous_path, previous)?;
    }
    write_atomic_file(path, bytes)?;
    sync_parent_directory(path)?;
    *last_committed = Some(bytes.to_vec());
    Ok(true)
}

fn read_valid_session_bytes(path: &std::path::Path) -> anyhow::Result<Option<Vec<u8>>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    let session: SavedSession = match serde_json::from_slice(&bytes) {
        Ok(session) => session,
        Err(_) => return Ok(None),
    };
    Ok((session.version == SESSION_VERSION).then_some(bytes))
}

fn write_atomic_file(path: &std::path::Path, bytes: &[u8]) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("creating temporary file in {}", parent.display()))?;
    temp.write_all(bytes)
        .with_context(|| format!("writing temporary session for {}", path.display()))?;
    temp.as_file_mut()
        .sync_all()
        .with_context(|| format!("syncing temporary session for {}", path.display()))?;
    temp.persist(path)
        .map_err(|err| err.error)
        .with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn sync_parent_directory(path: &std::path::Path) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
    std::fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("syncing session directory {}", parent.display()))
}

#[cfg(not(unix))]
fn sync_parent_directory(_path: &std::path::Path) -> anyhow::Result<()> {
    Ok(())
}

/// Load a saved session from disk (if it exists).
pub fn load_session() -> anyhow::Result<Option<SavedSession>> {
    let Some((path, session)) = load_first_valid_session([
        session_path(),
        previous_session_path(),
        legacy_session_path(),
    ])?
    else {
        return Ok(None);
    };

    let total_tabs: usize = session.windows.iter().map(|w| w.tabs.len()).sum();
    log::info!(
        "Loaded session: {} windows, {} tabs from {}",
        session.windows.len(),
        total_tabs,
        path.display(),
    );

    Ok(Some(session))
}

fn load_first_valid_session(
    paths: impl IntoIterator<Item = PathBuf>,
) -> anyhow::Result<Option<(PathBuf, SavedSession)>> {
    let mut failures = Vec::new();
    for path in paths {
        if !path.exists() {
            continue;
        }
        match std::fs::read(&path)
            .with_context(|| format!("reading session from {}", path.display()))
            .and_then(|json| {
                serde_json::from_slice::<SavedSession>(&json)
                    .with_context(|| format!("parsing session from {}", path.display()))
            }) {
            Ok(session) if session.version == SESSION_VERSION => {
                return Ok(Some((path, session)));
            }
            Ok(session) => failures.push(anyhow!(
                "session file {} has version {}, expected {}",
                path.display(),
                session.version,
                SESSION_VERSION
            )),
            Err(err) => failures.push(err),
        }
    }
    if let Some(err) = failures.pop() {
        return Err(err.context("no valid saved session fallback"));
    }
    Ok(None)
}

/// Remove the saved session file (after successful restore or on clean exit).
pub fn clear_session() -> anyhow::Result<()> {
    for path in [
        session_path(),
        previous_session_path(),
        legacy_session_path(),
    ] {
        if path.exists() {
            std::fs::remove_file(&path)
                .with_context(|| format!("removing session file {}", path.display()))?;
        }
    }
    *LAST_COMMITTED_SESSION
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    Ok(())
}

/// Restore a saved session by spawning new panes with the saved CWDs
/// and recreating the split tree structure.
///
/// Returns the number of tabs restored.
pub async fn restore_session(domain: &Arc<dyn crate::domain::Domain>) -> anyhow::Result<usize> {
    let session = match load_session()? {
        Some(s) => s,
        None => return Ok(0),
    };

    let mux = Mux::get();
    let config = config::configuration();
    let default_size = config.initial_size(0, None);
    let restore_intents: HashMap<PaneId, SavedAgentRestoreIntent> = session
        .agent_restore_intents
        .iter()
        .cloned()
        .map(|intent| (intent.pane_id, intent))
        .collect();
    let mut total_tabs = 0;

    for saved_window in &session.windows {
        let workspace = Some(saved_window.workspace.clone());
        let position = None;
        let window_id = mux.new_empty_window(workspace, position);
        let mut restored_tab_ids = Vec::new();
        let mut restored_parked_tab_ids = Vec::new();

        for saved_tab in &saved_window.tabs {
            match restore_tab(
                domain,
                &saved_tab,
                default_size,
                *window_id,
                &restore_intents,
            )
            .await
            {
                Ok(tab_id) => {
                    total_tabs += 1;
                    restored_tab_ids.push(tab_id);
                    if saved_tab.parked {
                        restored_parked_tab_ids.push(tab_id);
                    }
                }
                Err(err) => {
                    log::error!("Failed to restore tab '{}': {:#}", saved_tab.title, err);
                }
            }
        }
        if !restored_parked_tab_ids.is_empty() {
            let result = mux
                .get_window_mut(*window_id)
                .context("restored window disappeared")?
                .apply_parked_tabs(&restored_tab_ids, &restored_parked_tab_ids);
            if let Err(err) = result {
                log::warn!(
                    "Could not restore parked tabs for window {}: {:#}",
                    *window_id,
                    err
                );
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
    default_size: wakterm_term::TerminalSize,
    window_id: crate::WindowId,
    restore_intents: &HashMap<PaneId, SavedAgentRestoreIntent>,
) -> anyhow::Result<crate::tab::TabId> {
    let first_entry = first_leaf_entry(&saved_tab.tree);
    let first_cwd = first_entry.and_then(|entry| restore_cwd_for_entry(entry, restore_intents));
    let first_restore = match first_entry {
        Some(entry) => prepare_restore_for_entry(entry, restore_intents)?,
        None => None,
    };
    let (first_command, first_intent) = match first_restore {
        Some(prepared) => (Some(prepared.command), Some(prepared.intent)),
        None => (None, None),
    };

    // Use a generous size for spawning so split percentages produce
    // usable pane sizes. The client will resize all tabs to its actual
    // window size on connect. We can't know the client's window size
    // at restore time (the server starts before any client connects).
    let restore_size = {
        let saved = saved_tab.tree.root_size().unwrap_or(default_size);
        // Use the larger of saved size and a minimum (200x60) to ensure
        // splits have room to work
        wakterm_term::TerminalSize {
            rows: saved.rows.max(60),
            cols: saved.cols.max(200),
            pixel_width: saved.pixel_width.max(200 * 10),
            pixel_height: saved.pixel_height.max(60 * 20),
            dpi: saved.dpi,
        }
    };

    let tab = domain
        .spawn(restore_size, first_command, first_cwd, window_id)
        .await
        .context("spawning first pane for tab")?;

    if let Some(intent) = first_intent.as_ref() {
        let pane_id = tab
            .iter_panes_ignoring_zoom()
            .first()
            .map(|positioned| positioned.pane.pane_id())
            .context("restored tab has no first pane")?;
        restore_agent_intent(pane_id, intent)
            .context("registering agent restore intent for first pane")?;
    }

    tab.set_title(&Mux::sanitize_tab_title_text(&saved_tab.title));

    // The first leaf is pane index 0. Now recursively split to create
    // the rest of the tree. leaf_index tracks which pane index in the
    // tab's pane list corresponds to the "current" left-side pane.
    let mut leaf_index = 0;
    restore_node(
        domain,
        &tab,
        &saved_tab.tree,
        &mut leaf_index,
        restore_intents,
    )
    .await?;

    // Force a resize to reconcile the tree — the splits were created
    // at intermediate sizes and may have accumulated inconsistencies
    // (e.g., column heights not matching across an H-split).
    tab.resize(restore_size);

    Ok(tab.tab_id())
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
    restore_intents: &'a HashMap<PaneId, SavedAgentRestoreIntent>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + 'a>> {
    Box::pin(async move {
        match node {
            PaneNode::Empty => {}
            PaneNode::Leaf(_entry) => {
                // This leaf already exists — advance the index
                *leaf_index += 1;
            }
            PaneNode::Split {
                left,
                right,
                node: split_data,
            } => {
                // First, recursively restore the left subtree.
                // After this, all left-side leaves exist in the tab.
                restore_node(domain, tab, left, leaf_index, restore_intents).await?;

                // The pane we need to split is the one just before
                // the current leaf_index (the last leaf of the left subtree)
                let split_pane_index = leaf_index.saturating_sub(1);

                // Spawn a new pane for the right side
                let right_entry = first_leaf_entry(right);
                let cwd =
                    right_entry.and_then(|entry| restore_cwd_for_entry(entry, restore_intents));
                let right_restore = match right_entry {
                    Some(entry) => prepare_restore_for_entry(entry, restore_intents)?,
                    None => None,
                };
                let (right_command, right_intent) = match right_restore {
                    Some(prepared) => (Some(prepared.command), Some(prepared.intent)),
                    None => (None, None),
                };
                let pane = domain
                    .spawn_pane(split_data.second, right_command, cwd)
                    .await
                    .context("spawning pane for split")?;

                Mux::get().add_pane(&pane)?;

                if let Some(intent) = right_intent.as_ref() {
                    restore_agent_intent(pane.pane_id(), intent)
                        .context("registering agent restore intent for split pane")?;
                }

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
                restore_node(domain, tab, right, leaf_index, restore_intents).await?;
            }
        }
        Ok(())
    })
}

fn restore_cwd_for_entry(
    entry: &crate::tab::PaneEntry,
    restore_intents: &HashMap<PaneId, SavedAgentRestoreIntent>,
) -> Option<String> {
    if let Some(intent) = restore_intents.get(&entry.pane_id) {
        let cwd = intent.metadata.declared_cwd.trim();
        if !cwd.is_empty() {
            if cwd.starts_with("file://") {
                if let Ok(url) = url::Url::parse(cwd) {
                    return Some(
                        url.to_file_path()
                            .unwrap_or_else(|_| PathBuf::from(url.path()))
                            .to_string_lossy()
                            .into_owned(),
                    );
                }
            }
            return Some(cwd.to_string());
        }
    }

    entry
        .working_dir
        .as_ref()
        .map(|url| url.url.path().to_string())
}

fn first_leaf_entry(node: &PaneNode) -> Option<&crate::tab::PaneEntry> {
    match node {
        PaneNode::Empty => None,
        PaneNode::Leaf(entry) => Some(entry),
        PaneNode::Split { left, right, .. } => {
            first_leaf_entry(left).or_else(|| first_leaf_entry(right))
        }
    }
}

fn restore_agent_intent(pane_id: PaneId, intent: &SavedAgentRestoreIntent) -> anyhow::Result<()> {
    let mux = Mux::get();
    let harness = intent.harness();
    let result = if harness == AgentHarness::Codex && intent.metadata.codex_app_server.is_some() {
        mux.restore_agent_metadata(pane_id, intent.metadata.clone())
    } else {
        mux.register_agent_restore_intent(
            pane_id,
            harness,
            intent.metadata.clone(),
            intent.session_id.clone(),
        )
    };
    if result.is_ok() {
        if let Some(seen_at) = intent.attention_seen_at {
            mux.restore_agent_attention_seen_at(pane_id, seen_at);
        }
    }
    result
}

fn prepare_restore_for_entry(
    entry: &crate::tab::PaneEntry,
    restore_intents: &HashMap<PaneId, SavedAgentRestoreIntent>,
) -> anyhow::Result<Option<PreparedAgentRestore>> {
    let Some(intent) = restore_intents.get(&entry.pane_id) else {
        return Ok(None);
    };
    let mut prepared = prepare_agent_restore(intent, |request| {
        Mux::get().prepare_codex_app_server_launch(request)
    })?;
    prepared.command = restored_harness_then_shell(prepared.command)?;
    Ok(Some(prepared))
}

#[cfg(unix)]
fn restored_harness_then_shell(command: CommandBuilder) -> anyhow::Result<CommandBuilder> {
    let shell = command
        .get_env("SHELL")
        .filter(|shell| !shell.is_empty())
        .unwrap_or_else(|| std::ffi::OsStr::new("/bin/sh"))
        .to_owned();
    let invocation = command
        .get_argv()
        .iter()
        .map(|arg| {
            arg.to_str()
                .context("restored harness argument is not valid UTF-8")
                .map(shell_words::quote)
        })
        .collect::<anyhow::Result<Vec<_>>>()?
        .join(" ");
    Ok(CommandBuilder::from_argv(vec![
        shell.clone(),
        "-l".into(),
        "-i".into(),
        "-c".into(),
        format!("{invocation}; exec \"$0\" -l").into(),
        shell,
    ]))
}

#[cfg(windows)]
fn restored_harness_then_shell(command: CommandBuilder) -> anyhow::Result<CommandBuilder> {
    Ok(command)
}

fn prepare_agent_restore(
    intent: &SavedAgentRestoreIntent,
    prepare_managed: impl FnOnce(PrepareCodexLaunch) -> anyhow::Result<PreparedCodexLaunch>,
) -> anyhow::Result<PreparedAgentRestore> {
    let harness = intent.harness();
    if matches!(harness, AgentHarness::Agy | AgentHarness::Claude) {
        return Ok(PreparedAgentRestore {
            command: native_resume_command(
                &harness,
                &intent.metadata.launch_cmd,
                &intent.session_id,
            )
            .with_context(|| {
                format!(
                    "building {:?} native restore for session {}",
                    harness, intent.session_id
                )
            })?,
            intent: intent.clone(),
        });
    }
    anyhow::ensure!(
        harness == AgentHarness::Codex,
        "automatic restore is not implemented for {:?}",
        harness
    );
    let persisted_managed = intent.metadata.codex_app_server.as_ref();
    let resume_thread_id = persisted_managed
        .map(|session| session.thread_id.as_str())
        .unwrap_or(intent.session_id.as_str());
    let tui_args = persisted_managed
        .map(|session| session.tui_args.clone())
        .map(Ok)
        .unwrap_or_else(|| {
            let mut argv = shell_words::split(&intent.metadata.launch_cmd)
                .context("parsing adopted Codex launch command")?;
            anyhow::ensure!(!argv.is_empty(), "Codex launch command must not be empty");
            argv.remove(0);
            Ok::<_, anyhow::Error>(argv)
        })?;
    let request = PrepareCodexLaunch {
        name: intent.metadata.name.clone(),
        cwd: intent.metadata.declared_cwd.clone(),
        resume_thread_id: Some(resume_thread_id.to_string()),
        tui_args,
    };

    match prepare_managed(request) {
        Ok(prepared) => {
            anyhow::ensure!(
                prepared.session.thread_id == resume_thread_id,
                "Codex restored thread {}, expected {}",
                prepared.session.thread_id,
                resume_thread_id
            );
            if let Some(previous) = persisted_managed {
                anyhow::ensure!(
                    prepared.session.session_id == previous.session_id,
                    "Codex restored session {}, expected {}",
                    prepared.session.session_id,
                    previous.session_id
                );
            }
            let mut restored = intent.clone();
            restored.metadata.launch_cmd = prepared.session.executable.clone();
            restored.metadata.adopted_pid = None;
            restored.metadata.adopted_start_time = None;
            restored.session_id = prepared.session.session_id.clone();
            restored.metadata.codex_app_server = Some(prepared.session);
            Ok(PreparedAgentRestore {
                command: CommandBuilder::from_argv(
                    prepared.argv.into_iter().map(Into::into).collect(),
                ),
                intent: restored,
            })
        }
        Err(managed_error) => {
            log::warn!(
                "Could not restore Codex thread {} through the shared app-server; using exact native resume: {:#}",
                resume_thread_id,
                managed_error
            );
            let mut restored = intent.clone();
            restored.metadata.codex_app_server = None;
            restored.session_id = resume_thread_id.to_string();
            let mut command = native_resume_command(
                &AgentHarness::Codex,
                &restored.metadata.launch_cmd,
                resume_thread_id,
            )
            .with_context(|| {
                format!(
                    "building Codex native fallback for restored thread {}",
                    resume_thread_id
                )
            })?;
            if let Some(session) = persisted_managed {
                command
                    .get_argv_mut()
                    .extend(session.tui_args.iter().cloned().map(Into::into));
            }
            Ok(PreparedAgentRestore {
                command,
                intent: restored,
            })
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::agent::{AgentMetadata, CodexAppServerSession};
    use crate::client::{ClientId, ClientViewId};
    use crate::domain::{Domain, DomainState};
    use crate::pane::{alloc_pane_id, CachePolicy, LogicalLine, Pane};
    use crate::renderable::RenderableDimensions;
    use crate::tab::{SplitDirection, SplitRequest, SplitSize, Tab};
    use crate::Mux;
    use async_trait::async_trait;
    use chrono::{TimeZone, Utc};
    use rangeset::RangeSet;
    use std::io::Write;
    use std::ops::Range;
    use std::sync::{Arc, Mutex};
    use termwiz::surface::{CursorShape, CursorVisibility, Line, SequenceNo};
    use url::Url;
    use wakterm_term::color::ColorPalette;
    use wakterm_term::{KeyCode, KeyModifiers, MouseEvent, StableRowIndex, TerminalSize};

    fn expected_restored_spawn(argv: Vec<String>) -> Vec<Vec<String>> {
        #[cfg(unix)]
        {
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
            let invocation = argv
                .iter()
                .map(|arg| shell_words::quote(arg))
                .collect::<Vec<_>>()
                .join(" ");
            vec![vec![
                shell.clone(),
                "-l".to_string(),
                "-i".to_string(),
                "-c".to_string(),
                format!("{invocation}; exec \"$0\" -l"),
                shell,
            ]]
        }
        #[cfg(windows)]
        {
            vec![argv]
        }
    }

    #[test]
    fn automatic_session_uses_durable_data_directory() {
        assert_eq!(session_path(), config::DATA_DIR.join("session.json"));
        assert_eq!(
            previous_session_path(),
            config::DATA_DIR.join("session.json.prev")
        );
        assert_eq!(
            legacy_session_path(),
            config::RUNTIME_DIR.join("session.json")
        );
    }

    #[cfg(unix)]
    #[test]
    fn restored_harness_runs_as_a_shell_child() {
        let mut command =
            CommandBuilder::from_argv(vec!["codex".into(), "argument with spaces".into()]);
        command.env("SHELL", "/usr/bin/zsh");
        let wrapped = restored_harness_then_shell(command).unwrap();
        let argv = wrapped
            .get_argv()
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(&argv[..4], &["/usr/bin/zsh", "-l", "-i", "-c"]);
        assert_eq!(argv[4], "codex 'argument with spaces'; exec \"$0\" -l");
        assert_eq!(argv[5], "/usr/bin/zsh");
    }

    #[test]
    fn auto_save_debounce_tracks_the_latest_dirty_event() {
        let start = Instant::now();
        let mut schedule =
            AutoSaveSchedule::new(start, Duration::from_millis(500), Duration::from_secs(60));

        schedule.mark_dirty(start);
        schedule.mark_dirty(start + Duration::from_millis(100));

        assert!(!schedule.take_save_due(start + Duration::from_millis(500)));
        assert!(schedule.take_save_due(start + Duration::from_millis(600)));
        assert!(!schedule.take_save_due(start + Duration::from_millis(601)));
    }

    #[test]
    fn periodic_auto_save_reconciles_and_clears_pending_dirty_state() {
        let start = Instant::now();
        let mut schedule =
            AutoSaveSchedule::new(start, Duration::from_secs(5), Duration::from_secs(60));

        schedule.mark_dirty(start + Duration::from_secs(59));

        assert!(schedule.take_save_due(start + Duration::from_secs(60)));
        assert!(!schedule.take_save_due(start + Duration::from_secs(64)));
        assert!(schedule.take_save_due(start + Duration::from_secs(120)));
    }

    #[test]
    fn atomic_session_commit_skips_unchanged_bytes_and_keeps_previous_generation() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("session.json");
        let previous = dir.path().join("session.json.prev");
        let first = serde_json::to_vec(&SavedSession {
            version: SESSION_VERSION,
            windows: Vec::new(),
            agent_restore_intents: Vec::new(),
        })
        .unwrap();
        let second = serde_json::to_vec(&SavedSession {
            version: SESSION_VERSION,
            windows: vec![SavedWindow {
                workspace: "default".to_string(),
                tabs: Vec::new(),
            }],
            agent_restore_intents: Vec::new(),
        })
        .unwrap();
        let mut committed = None;

        assert!(commit_serialized_session(&current, &previous, &first, &mut committed).unwrap());
        assert!(!previous.exists());
        assert!(!commit_serialized_session(&current, &previous, &first, &mut committed).unwrap());
        assert!(commit_serialized_session(&current, &previous, &second, &mut committed).unwrap());
        assert_eq!(std::fs::read(&current).unwrap(), second);
        assert_eq!(std::fs::read(&previous).unwrap(), first);
    }

    #[test]
    fn session_load_falls_back_to_previous_valid_generation() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("session.json");
        let previous = dir.path().join("session.json.prev");
        std::fs::write(&current, b"incomplete").unwrap();
        let expected = SavedSession {
            version: SESSION_VERSION,
            windows: Vec::new(),
            agent_restore_intents: Vec::new(),
        };
        std::fs::write(&previous, serde_json::to_vec(&expected).unwrap()).unwrap();

        let (loaded_path, loaded) = load_first_valid_session([current, previous.clone()])
            .unwrap()
            .unwrap();
        assert_eq!(loaded_path, previous);
        assert_eq!(loaded.version, SESSION_VERSION);
    }

    #[test]
    #[ignore]
    fn session_commit_performance_workload() {
        let dir = match std::env::var_os("WAKTERM_SESSION_BENCH_DIR") {
            Some(path) => tempfile::tempdir_in(path).unwrap(),
            None => tempfile::tempdir().unwrap(),
        };
        let current = dir.path().join("session.json");
        let previous = dir.path().join("session.json.prev");
        let mut bytes = vec![b'x'; 63 * 1024];
        let mut committed = Some(bytes.clone());

        let unchanged_iterations = 10_000;
        let unchanged_started = Instant::now();
        for _ in 0..unchanged_iterations {
            assert!(
                !commit_serialized_session(&current, &previous, &bytes, &mut committed,).unwrap()
            );
        }
        let unchanged_elapsed = unchanged_started.elapsed();

        let changed_iterations = 100;
        let changed_started = Instant::now();
        for iteration in 0..changed_iterations {
            bytes[0] = iteration as u8;
            assert!(
                commit_serialized_session(&current, &previous, &bytes, &mut committed,).unwrap()
            );
        }
        let changed_elapsed = changed_started.elapsed();

        eprintln!(
            "session_commit_workload bytes={} unchanged_iterations={} unchanged_ns={} changed_iterations={} changed_ns={}",
            bytes.len(),
            unchanged_iterations,
            unchanged_elapsed.as_nanos(),
            changed_iterations,
            changed_elapsed.as_nanos(),
        );
    }

    struct TestPane {
        id: crate::pane::PaneId,
        size: Mutex<TerminalSize>,
        dead: bool,
    }

    impl TestPane {
        fn new(id: crate::pane::PaneId, size: TerminalSize) -> Arc<dyn Pane> {
            Arc::new(Self {
                id,
                size: Mutex::new(size),
                dead: false,
            })
        }

        fn new_dead(id: crate::pane::PaneId, size: TerminalSize) -> Arc<dyn Pane> {
            Arc::new(Self {
                id,
                size: Mutex::new(size),
                dead: true,
            })
        }
    }

    struct TestDomain {
        commands: Arc<Mutex<Vec<Vec<String>>>>,
        command_dirs: Arc<Mutex<Vec<Option<String>>>>,
    }

    impl TestDomain {
        fn new() -> Self {
            Self {
                commands: Arc::new(Mutex::new(Vec::new())),
                command_dirs: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    #[async_trait(?Send)]
    impl Domain for TestDomain {
        async fn spawn_pane(
            &self,
            size: TerminalSize,
            command: Option<CommandBuilder>,
            command_dir: Option<String>,
        ) -> anyhow::Result<Arc<dyn Pane>> {
            self.command_dirs.lock().unwrap().push(command_dir);
            if let Some(command) = command {
                self.commands.lock().unwrap().push(
                    command
                        .get_argv()
                        .iter()
                        .map(|arg| arg.to_string_lossy().into_owned())
                        .collect(),
                );
            }
            Ok(TestPane::new(alloc_pane_id(), size))
        }

        fn detachable(&self) -> bool {
            false
        }

        fn domain_id(&self) -> crate::domain::DomainId {
            0
        }

        fn domain_name(&self) -> &str {
            "test"
        }

        async fn attach(&self, _window_id: Option<crate::window::WindowId>) -> anyhow::Result<()> {
            Ok(())
        }

        fn detach(&self) -> anyhow::Result<()> {
            Ok(())
        }

        fn state(&self) -> DomainState {
            DomainState::Attached
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
            self.dead
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

    struct ShutdownStateGuard;

    impl Drop for ShutdownStateGuard {
        fn drop(&mut self) {
            end_shutdown_for_test();
        }
    }

    #[test]
    fn shutdown_keeps_dead_panes_in_recoverable_layout_until_explicit_close() {
        let _test_lock = crate::TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let _mux_guard = crate::TestMuxGuard;
        let window_id = *mux.new_empty_window(Some("default".to_string()), None);
        let tab_size = size(120, 40);
        let tab = Arc::new(Tab::new(&tab_size));
        let pane = TestPane::new_dead(alloc_pane_id(), tab_size);
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();

        begin_shutdown();
        let _shutdown_guard = ShutdownStateGuard;
        mux.prune_dead_windows();

        assert!(mux.get_tab(tab.tab_id()).is_some());
        assert_eq!(mux.get_window(window_id).unwrap().len(), 1);

        mux.remove_tab(tab.tab_id());
        assert!(mux.get_tab(tab.tab_id()).is_none());
    }

    #[test]
    fn restorable_pane_exit_keeps_layout_until_explicit_close() {
        let _test_lock = crate::TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let _mux_guard = crate::TestMuxGuard;
        let window_id = *mux.new_empty_window(Some("default".to_string()), None);
        let tab_size = size(120, 40);

        let restorable_tab = Arc::new(Tab::new(&tab_size));
        let restorable_pane = TestPane::new_dead(alloc_pane_id(), tab_size);
        let restorable_pane_id = restorable_pane.pane_id();
        restorable_tab.assign_pane(&restorable_pane);
        mux.add_tab_and_active_pane(&restorable_tab).unwrap();
        mux.add_tab_to_window(&restorable_tab, window_id).unwrap();
        let mut metadata = sample_agent_metadata("restorable");
        metadata.codex_app_server = Some(CodexAppServerSession {
            thread_id: "exact-thread".to_string(),
            session_id: "exact-session".to_string(),
            executable: "codex".to_string(),
            version: "codex-cli current".to_string(),
            tui_args: Vec::new(),
        });
        mux.set_agent_metadata(restorable_pane_id, metadata)
            .unwrap();

        let disposable_tab = Arc::new(Tab::new(&tab_size));
        let disposable_pane = TestPane::new_dead(alloc_pane_id(), tab_size);
        disposable_tab.assign_pane(&disposable_pane);
        mux.add_tab_and_active_pane(&disposable_tab).unwrap();
        mux.add_tab_to_window(&disposable_tab, window_id).unwrap();

        mux.record_agent_pane_exit(restorable_pane_id);
        mux.prune_dead_windows();

        assert!(mux.get_tab(restorable_tab.tab_id()).is_some());
        assert!(mux.get_tab(disposable_tab.tab_id()).is_none());
        let runtime = mux
            .agent_runtime_by_pane
            .read()
            .get(&restorable_pane_id)
            .cloned()
            .unwrap();
        assert!(!runtime.alive);
        assert_eq!(runtime.status, crate::agent::AgentStatus::Exited);

        mux.remove_tab(restorable_tab.tab_id());
        assert!(mux.get_tab(restorable_tab.tab_id()).is_none());
    }

    fn sample_agent_metadata(name: &str) -> AgentMetadata {
        AgentMetadata {
            agent_id: format!("agent-{name}"),
            name: name.to_string(),
            launch_cmd: "codex".to_string(),
            declared_cwd: format!("/tmp/{name}"),
            adopted_pid: None,
            adopted_start_time: None,
            created_at: Utc.with_ymd_and_hms(2026, 3, 17, 12, 0, 0).unwrap(),
            repo_root: None,
            worktree: None,
            branch: None,
            managed_checkout: false,
            codex_app_server: None,
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
        mux.set_active_pane_for_client_view(
            view_id.as_ref(),
            window_id,
            tab.tab_id(),
            right_pane_id,
        )
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

    #[test]
    fn saved_session_includes_busy_codex_restore_intent() {
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

        let metadata = sample_agent_metadata("resume");
        mux.set_agent_metadata(pane_id, metadata.clone()).unwrap();
        let session_dir = tempfile::tempdir().unwrap();
        let session_path = session_dir.path().join("rollout.jsonl");
        std::fs::write(
            &session_path,
            "{\"type\":\"session_meta\",\"payload\":{\"session_id\":\"saved-session\"}}\n",
        )
        .unwrap();
        let mut runtime = mux
            .agent_runtime_by_pane
            .read()
            .get(&pane_id)
            .cloned()
            .expect("agent runtime");
        runtime.turn_state = crate::agent::AgentTurnState::WaitingOnAgent;
        runtime.session_path = Some(session_path.to_string_lossy().into_owned());
        mux.agent_runtime_by_pane.write().insert(pane_id, runtime);
        let attention_seen_at = chrono::Utc::now();
        mux.restore_agent_attention_seen_at(pane_id, attention_seen_at);

        let session = build_saved_session(&mux);
        assert_eq!(session.agent_restore_intents.len(), 1);
        assert_eq!(session.agent_restore_intents[0].pane_id, pane_id);
        assert_eq!(session.agent_restore_intents[0].metadata.name, "resume");
        assert_eq!(session.agent_restore_intents[0].session_id, "saved-session");
        assert_eq!(
            session.agent_restore_intents[0].attention_seen_at,
            Some(attention_seen_at)
        );
    }

    #[test]
    fn saved_session_includes_confirmed_claude_restore_intent() {
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

        let mut metadata = sample_agent_metadata("claude-resume");
        metadata.launch_cmd = "cl".to_string();
        mux.set_agent_metadata(pane_id, metadata).unwrap();
        let session_dir = tempfile::tempdir().unwrap();
        let session_id = "00000000-0000-4000-8000-000000000008";
        let session_path = session_dir.path().join(format!("{session_id}.jsonl"));
        std::fs::write(
            &session_path,
            format!("{{\"type\":\"user\",\"sessionId\":\"{session_id}\"}}\n"),
        )
        .unwrap();
        let mut runtime = mux
            .agent_runtime_by_pane
            .read()
            .get(&pane_id)
            .cloned()
            .expect("agent runtime");
        runtime.harness = AgentHarness::Claude;
        runtime.transport = crate::agent::AgentTransport::ObservedPty;
        runtime.session_path = Some(session_path.to_string_lossy().into_owned());
        mux.agent_runtime_by_pane.write().insert(pane_id, runtime);

        let session = build_saved_session(&mux);

        assert_eq!(session.agent_restore_intents.len(), 1);
        let intent = &session.agent_restore_intents[0];
        assert_eq!(intent.harness, AgentHarness::Claude);
        assert_eq!(intent.session_id, session_id);
        assert_eq!(intent.metadata.launch_cmd, "claude");
        assert_eq!(intent.metadata.name, "claude-resume");
    }

    #[test]
    fn saved_session_includes_confirmed_agy_restore_intent() {
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

        let conversation_id = "00000000-0000-4000-8000-000000000009";
        let mut metadata = sample_agent_metadata("agy-resume");
        metadata.launch_cmd = "agy --dangerously-skip-permissions".to_string();
        mux.set_agent_metadata(pane_id, metadata).unwrap();
        let transcript = PathBuf::from("/tmp/antigravity-cli")
            .join("brain")
            .join(conversation_id)
            .join(".system_generated")
            .join("logs")
            .join("transcript.jsonl");
        let mut runtime = mux
            .agent_runtime_by_pane
            .read()
            .get(&pane_id)
            .cloned()
            .expect("agent runtime");
        runtime.harness = AgentHarness::Agy;
        runtime.transport = crate::agent::AgentTransport::ObservedPty;
        runtime.session_path = Some(transcript.to_string_lossy().into_owned());
        mux.agent_runtime_by_pane.write().insert(pane_id, runtime);

        let session = build_saved_session(&mux);

        assert_eq!(session.agent_restore_intents.len(), 1);
        let intent = &session.agent_restore_intents[0];
        assert_eq!(intent.harness, AgentHarness::Agy);
        assert_eq!(intent.session_id, conversation_id);
        assert_eq!(
            intent.metadata.launch_cmd,
            "agy --dangerously-skip-permissions"
        );
        assert_eq!(intent.metadata.name, "agy-resume");
    }

    #[test]
    fn saved_session_preserves_pending_codex_restore_intent() {
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

        mux.register_agent_restore_intent(
            pane_id,
            AgentHarness::Codex,
            sample_agent_metadata("pending-resume"),
            "pending-session".to_string(),
        )
        .unwrap();

        let session = build_saved_session(&mux);

        assert_eq!(session.agent_restore_intents.len(), 1);
        assert_eq!(session.agent_restore_intents[0].pane_id, pane_id);
        assert_eq!(
            session.agent_restore_intents[0].metadata.name,
            "pending-resume"
        );
        assert_eq!(
            session.agent_restore_intents[0].session_id,
            "pending-session"
        );
    }

    #[test]
    fn restore_intent_without_harness_field_retains_codex_session_compatibility() {
        let intent = SavedAgentRestoreIntent {
            pane_id: 7,
            harness: AgentHarness::Codex,
            metadata: sample_agent_metadata("legacy"),
            session_id: "legacy-session".to_string(),
            attention_seen_at: None,
        };
        let mut value = serde_json::to_value(intent).unwrap();
        value.as_object_mut().unwrap().remove("harness");

        let restored: SavedAgentRestoreIntent = serde_json::from_value(value).unwrap();

        assert_eq!(restored.harness(), AgentHarness::Codex);
        assert_eq!(restored.session_id, "legacy-session");
    }

    #[test]
    fn adopted_codex_restore_optimistically_promotes_to_current_app_server() {
        let thread_id = "00000000-0000-4000-8000-000000000001";
        let mut metadata = sample_agent_metadata("adopted");
        metadata.launch_cmd = "codex -a never -s danger-full-access".to_string();
        metadata.adopted_pid = Some(42);
        metadata.adopted_start_time = Some(99);
        let intent = SavedAgentRestoreIntent {
            pane_id: 7,
            harness: AgentHarness::Codex,
            metadata,
            session_id: thread_id.to_string(),
            attention_seen_at: None,
        };

        let restored = prepare_agent_restore(&intent, |request| {
            assert_eq!(request.resume_thread_id.as_deref(), Some(thread_id));
            assert_eq!(
                request.tui_args,
                vec!["-a", "never", "-s", "danger-full-access"]
            );
            Ok(PreparedCodexLaunch {
                argv: vec!["latest-codex".into(), "resume".into(), thread_id.into()],
                session: CodexAppServerSession {
                    thread_id: thread_id.to_string(),
                    session_id: "current-session".to_string(),
                    executable: "latest-codex".to_string(),
                    version: "codex-cli latest".to_string(),
                    tui_args: vec![
                        "-a".to_string(),
                        "never".to_string(),
                        "-s".to_string(),
                        "danger-full-access".to_string(),
                    ],
                },
            })
        })
        .unwrap();

        assert_eq!(
            restored
                .command
                .get_argv()
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            vec!["latest-codex", "resume", thread_id]
        );
        assert_eq!(restored.intent.metadata.launch_cmd, "latest-codex");
        assert!(crate::agent_admission::incarnation_id(&restored.intent.metadata).is_some());
        assert_eq!(restored.intent.metadata.adopted_pid, None);
        assert_eq!(restored.intent.metadata.adopted_start_time, None);
        assert_eq!(
            restored
                .intent
                .metadata
                .codex_app_server
                .as_ref()
                .map(|session| session.version.as_str()),
            Some("codex-cli latest")
        );
    }

    #[test]
    fn adopted_claude_restore_uses_exact_native_session_without_codex_preparation() {
        let session_id = "00000000-0000-4000-8000-000000000004";
        let mut metadata = sample_agent_metadata("claude");
        metadata.launch_cmd =
            "claude --dangerously-skip-permissions --add-dir /home/mihai --add-dir /code"
                .to_string();
        let intent = SavedAgentRestoreIntent {
            pane_id: 10,
            harness: AgentHarness::Claude,
            metadata,
            session_id: session_id.to_string(),
            attention_seen_at: None,
        };

        let restored = prepare_agent_restore(&intent, |_| {
            panic!("Claude native restore must not start the Codex app-server")
        })
        .unwrap();

        assert_eq!(
            restored
                .command
                .get_argv()
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            vec![
                "claude",
                "--dangerously-skip-permissions",
                "--add-dir",
                "/home/mihai",
                "--add-dir",
                "/code",
                "--resume",
                session_id,
            ]
        );
        assert_eq!(restored.intent.harness, AgentHarness::Claude);
        assert_eq!(restored.intent.metadata.name, "claude");
    }

    #[test]
    fn adopted_agy_restore_uses_exact_native_conversation_without_codex_preparation() {
        let conversation_id = "00000000-0000-4000-8000-000000000010";
        let mut metadata = sample_agent_metadata("agy");
        metadata.launch_cmd =
            "agy --dangerously-skip-permissions --conversation old --continue".to_string();
        let intent = SavedAgentRestoreIntent {
            pane_id: 11,
            harness: AgentHarness::Agy,
            metadata,
            session_id: conversation_id.to_string(),
            attention_seen_at: None,
        };

        let restored = prepare_agent_restore(&intent, |_| {
            panic!("Agy native restore must not start the Codex app-server")
        })
        .unwrap();

        assert_eq!(
            restored
                .command
                .get_argv()
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            vec![
                "agy",
                "--dangerously-skip-permissions",
                "--conversation",
                conversation_id,
            ]
        );
        assert_eq!(restored.intent.harness, AgentHarness::Agy);
        assert_eq!(restored.intent.metadata.name, "agy");
    }

    #[test]
    fn managed_codex_restore_accepts_latest_version_and_preserves_identity() {
        let thread_id = "00000000-0000-4000-8000-000000000002";
        let mut metadata = sample_agent_metadata("managed");
        metadata.codex_app_server = Some(CodexAppServerSession {
            thread_id: thread_id.to_string(),
            session_id: "provider-session".to_string(),
            executable: "old-codex".to_string(),
            version: "codex-cli old".to_string(),
            tui_args: vec!["--no-alt-screen".to_string()],
        });
        let intent = SavedAgentRestoreIntent {
            pane_id: 8,
            harness: AgentHarness::Codex,
            metadata,
            session_id: "provider-session".to_string(),
            attention_seen_at: None,
        };

        let restored = prepare_agent_restore(&intent, |request| {
            assert_eq!(request.resume_thread_id.as_deref(), Some(thread_id));
            assert_eq!(request.tui_args, vec!["--no-alt-screen"]);
            Ok(PreparedCodexLaunch {
                argv: vec!["latest-codex".into(), "resume".into(), thread_id.into()],
                session: CodexAppServerSession {
                    thread_id: thread_id.to_string(),
                    session_id: "provider-session".to_string(),
                    executable: "latest-codex".to_string(),
                    version: "codex-cli latest".to_string(),
                    tui_args: vec!["--no-alt-screen".to_string()],
                },
            })
        })
        .unwrap();

        let session = restored.intent.metadata.codex_app_server.unwrap();
        assert_eq!(session.thread_id, thread_id);
        assert_eq!(session.session_id, "provider-session");
        assert_eq!(session.executable, "latest-codex");
        assert_eq!(session.version, "codex-cli latest");
    }

    #[test]
    fn failed_managed_restore_falls_back_to_exact_native_thread() {
        let thread_id = "00000000-0000-4000-8000-000000000003";
        let mut metadata = sample_agent_metadata("fallback");
        metadata.codex_app_server = Some(CodexAppServerSession {
            thread_id: thread_id.to_string(),
            session_id: "provider-session".to_string(),
            executable: "old-codex".to_string(),
            version: "codex-cli old".to_string(),
            tui_args: vec!["--no-alt-screen".to_string()],
        });
        let intent = SavedAgentRestoreIntent {
            pane_id: 9,
            harness: AgentHarness::Codex,
            metadata,
            session_id: "provider-session".to_string(),
            attention_seen_at: None,
        };

        let restored =
            prepare_agent_restore(&intent, |_| anyhow::bail!("thread unavailable")).unwrap();

        assert_eq!(
            restored
                .command
                .get_argv()
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            vec!["codex", "resume", thread_id, "--no-alt-screen"]
        );
        assert_eq!(restored.intent.session_id, thread_id);
        assert!(restored.intent.metadata.codex_app_server.is_none());
    }

    #[test]
    fn restore_tab_resumes_exact_busy_codex_session_and_registers_pending_intent() {
        let _test_lock = crate::TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let source_mux = Arc::new(Mux::new(None));
        Mux::set_mux(&source_mux);

        let window_id = *source_mux.new_empty_window(Some("default".to_string()), None);
        let tab_size = size(120, 40);
        let tab = Arc::new(Tab::new(&tab_size));
        let pane = TestPane::new(alloc_pane_id(), tab_size);
        let pane_id = pane.pane_id();
        tab.assign_pane(&pane);
        source_mux.add_tab_and_active_pane(&tab).unwrap();
        source_mux.add_tab_to_window(&tab, window_id).unwrap();

        let metadata = sample_agent_metadata("resume");
        source_mux
            .set_agent_metadata(pane_id, metadata.clone())
            .unwrap();
        let session_dir = tempfile::tempdir().unwrap();
        let session_path = session_dir.path().join("rollout.jsonl");
        std::fs::write(
            &session_path,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"resume-session\"}}\n",
        )
        .unwrap();
        let mut runtime = source_mux
            .agent_runtime_by_pane
            .read()
            .get(&pane_id)
            .cloned()
            .expect("agent runtime");
        runtime.turn_state = crate::agent::AgentTurnState::WaitingOnAgent;
        runtime.session_path = Some(session_path.to_string_lossy().into_owned());
        source_mux
            .agent_runtime_by_pane
            .write()
            .insert(pane_id, runtime);

        let saved_session = build_saved_session(&source_mux);
        let saved_tab = saved_session.windows[0].tabs[0].clone();
        let intents = saved_session
            .agent_restore_intents
            .into_iter()
            .map(|intent| (intent.pane_id, intent))
            .collect::<HashMap<_, _>>();

        let restored_mux = Arc::new(Mux::new(None));
        Mux::set_mux(&restored_mux);
        let _guard = crate::TestMuxGuard;
        let restored_window = *restored_mux.new_empty_window(Some("default".to_string()), None);
        let recording_domain = Arc::new(TestDomain::new());
        let commands = Arc::clone(&recording_domain.commands);
        let command_dirs = Arc::clone(&recording_domain.command_dirs);
        let domain: Arc<dyn Domain> = recording_domain;

        smol::block_on(async {
            restore_tab(&domain, &saved_tab, tab_size, restored_window, &intents)
                .await
                .expect("restore tab");
        });

        assert_eq!(
            commands.lock().unwrap().as_slice(),
            expected_restored_spawn(vec![
                "codex".to_string(),
                "resume".to_string(),
                "resume-session".to_string(),
            ])
        );
        assert_eq!(
            command_dirs.lock().unwrap().as_slice(),
            &[Some(metadata.declared_cwd)]
        );
        assert_eq!(restored_mux.pending_agent_restores.read().len(), 1);
    }

    #[test]
    fn restore_tab_resumes_exact_claude_session_and_registers_pending_intent() {
        let _test_lock = crate::TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let source_mux = Arc::new(Mux::new(None));
        Mux::set_mux(&source_mux);

        let window_id = *source_mux.new_empty_window(Some("default".to_string()), None);
        let tab_size = size(120, 40);
        let tab = Arc::new(Tab::new(&tab_size));
        let pane = TestPane::new(alloc_pane_id(), tab_size);
        let pane_id = pane.pane_id();
        tab.assign_pane(&pane);
        source_mux.add_tab_and_active_pane(&tab).unwrap();
        source_mux.add_tab_to_window(&tab, window_id).unwrap();

        let mut metadata = sample_agent_metadata("claude-resume");
        metadata.launch_cmd = "claude --dangerously-skip-permissions".to_string();
        source_mux
            .set_agent_metadata(pane_id, metadata.clone())
            .unwrap();
        let session_dir = tempfile::tempdir().unwrap();
        let session_id = "00000000-0000-4000-8000-000000000010";
        let session_path = session_dir.path().join(format!("{session_id}.jsonl"));
        std::fs::write(
            &session_path,
            format!("{{\"type\":\"mode\",\"sessionId\":\"{session_id}\"}}\n"),
        )
        .unwrap();
        let mut runtime = source_mux
            .agent_runtime_by_pane
            .read()
            .get(&pane_id)
            .cloned()
            .expect("agent runtime");
        runtime.harness = AgentHarness::Claude;
        runtime.transport = crate::agent::AgentTransport::ObservedPty;
        runtime.session_path = Some(session_path.to_string_lossy().into_owned());
        source_mux
            .agent_runtime_by_pane
            .write()
            .insert(pane_id, runtime);

        let saved_session = build_saved_session(&source_mux);
        let saved_tab = saved_session.windows[0].tabs[0].clone();
        let intents = saved_session
            .agent_restore_intents
            .into_iter()
            .map(|intent| (intent.pane_id, intent))
            .collect::<HashMap<_, _>>();

        let restored_mux = Arc::new(Mux::new(None));
        Mux::set_mux(&restored_mux);
        let _guard = crate::TestMuxGuard;
        let restored_window = *restored_mux.new_empty_window(Some("default".to_string()), None);
        let recording_domain = Arc::new(TestDomain::new());
        let commands = Arc::clone(&recording_domain.commands);
        let command_dirs = Arc::clone(&recording_domain.command_dirs);
        let domain: Arc<dyn Domain> = recording_domain;

        smol::block_on(async {
            restore_tab(&domain, &saved_tab, tab_size, restored_window, &intents)
                .await
                .expect("restore tab");
        });

        assert_eq!(
            commands.lock().unwrap().as_slice(),
            expected_restored_spawn(vec![
                "claude".to_string(),
                "--dangerously-skip-permissions".to_string(),
                "--resume".to_string(),
                session_id.to_string(),
            ])
        );
        assert_eq!(
            command_dirs.lock().unwrap().as_slice(),
            &[Some(metadata.declared_cwd)]
        );
        let pending = restored_mux.pending_agent_restores.read();
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending.values().next().map(|intent| &intent.harness),
            Some(&AgentHarness::Claude)
        );
    }

    #[test]
    fn restore_tab_resumes_exact_agy_conversation_and_registers_pending_intent() {
        let _test_lock = crate::TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let source_mux = Arc::new(Mux::new(None));
        Mux::set_mux(&source_mux);

        let window_id = *source_mux.new_empty_window(Some("default".to_string()), None);
        let tab_size = size(120, 40);
        let tab = Arc::new(Tab::new(&tab_size));
        let pane = TestPane::new(alloc_pane_id(), tab_size);
        let pane_id = pane.pane_id();
        tab.assign_pane(&pane);
        source_mux.add_tab_and_active_pane(&tab).unwrap();
        source_mux.add_tab_to_window(&tab, window_id).unwrap();

        let mut metadata = sample_agent_metadata("agy-resume");
        metadata.launch_cmd = "agy --dangerously-skip-permissions".to_string();
        source_mux
            .set_agent_metadata(pane_id, metadata.clone())
            .unwrap();
        let conversation_id = "00000000-0000-4000-8000-000000000011";
        let session_dir = tempfile::tempdir().unwrap();
        let session_path = session_dir
            .path()
            .join("brain")
            .join(conversation_id)
            .join(".system_generated")
            .join("logs")
            .join("transcript.jsonl");
        std::fs::create_dir_all(session_path.parent().unwrap()).unwrap();
        std::fs::write(&session_path, "{}\n").unwrap();
        let mut runtime = source_mux
            .agent_runtime_by_pane
            .read()
            .get(&pane_id)
            .cloned()
            .expect("agent runtime");
        runtime.harness = AgentHarness::Agy;
        runtime.transport = crate::agent::AgentTransport::ObservedPty;
        runtime.session_path = Some(session_path.to_string_lossy().into_owned());
        source_mux
            .agent_runtime_by_pane
            .write()
            .insert(pane_id, runtime);

        let saved_session = build_saved_session(&source_mux);
        let saved_tab = saved_session.windows[0].tabs[0].clone();
        let intents = saved_session
            .agent_restore_intents
            .into_iter()
            .map(|intent| (intent.pane_id, intent))
            .collect::<HashMap<_, _>>();

        let restored_mux = Arc::new(Mux::new(None));
        Mux::set_mux(&restored_mux);
        let _guard = crate::TestMuxGuard;
        let restored_window = *restored_mux.new_empty_window(Some("default".to_string()), None);
        let recording_domain = Arc::new(TestDomain::new());
        let commands = Arc::clone(&recording_domain.commands);
        let command_dirs = Arc::clone(&recording_domain.command_dirs);
        let domain: Arc<dyn Domain> = recording_domain;

        smol::block_on(async {
            restore_tab(&domain, &saved_tab, tab_size, restored_window, &intents)
                .await
                .expect("restore tab");
        });

        assert_eq!(
            commands.lock().unwrap().as_slice(),
            expected_restored_spawn(vec![
                "agy".to_string(),
                "--dangerously-skip-permissions".to_string(),
                "--conversation".to_string(),
                conversation_id.to_string(),
            ])
        );
        assert_eq!(
            command_dirs.lock().unwrap().as_slice(),
            &[Some(metadata.declared_cwd)]
        );
        let pending = restored_mux.pending_agent_restores.read();
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending.values().next().map(|intent| &intent.harness),
            Some(&AgentHarness::Agy)
        );
    }

    #[test]
    fn restored_session_does_not_reapply_agent_metadata_to_shell_panes() {
        let _test_lock = crate::TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let source_mux = Arc::new(Mux::new(None));
        Mux::set_mux(&source_mux);
        let _guard = crate::TestMuxGuard;

        let window_id = *source_mux.new_empty_window(Some("default".to_string()), None);
        let tab_size = size(120, 40);

        let tab = Arc::new(Tab::new(&tab_size));
        let pane = TestPane::new(alloc_pane_id(), tab_size);
        let pane_id = pane.pane_id();
        tab.assign_pane(&pane);
        source_mux.add_tab_and_active_pane(&tab).unwrap();
        source_mux.add_tab_to_window(&tab, window_id).unwrap();
        source_mux
            .set_agent_metadata(pane_id, sample_agent_metadata("reviewer"))
            .unwrap();

        let saved_tab = build_saved_session(&source_mux).windows[0].tabs[0].clone();

        let restored_mux = Arc::new(Mux::new(None));
        Mux::set_mux(&restored_mux);
        let restored_window = *restored_mux.new_empty_window(Some("default".to_string()), None);
        let domain: Arc<dyn Domain> = Arc::new(TestDomain::new());

        smol::block_on(async {
            restore_tab(
                &domain,
                &saved_tab,
                tab_size,
                restored_window,
                &HashMap::new(),
            )
            .await
            .expect("restore tab");
        });

        assert!(restored_mux.list_agents().is_empty());
    }

    #[test]
    fn saved_session_sanitizes_badged_tab_titles() {
        let _test_lock = crate::TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let _guard = crate::TestMuxGuard;

        let window_id = *mux.new_empty_window(Some("default".to_string()), None);
        let tab_size = size(120, 40);

        let tab = Arc::new(Tab::new(&tab_size));
        let pane = TestPane::new(alloc_pane_id(), tab_size);
        tab.assign_pane(&pane);
        tab.set_title("🤖 🤖 scrape");
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();

        let session = build_saved_session(&mux);
        assert_eq!(session.windows[0].tabs[0].title, "scrape");
    }

    #[test]
    fn saved_session_preserves_window_and_tab_order() {
        let _test_lock = crate::TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let _guard = crate::TestMuxGuard;

        let window_a = *mux.new_empty_window(Some("default".to_string()), None);
        let tab_size = size(120, 40);

        let first = Arc::new(Tab::new(&tab_size));
        first.assign_pane(&TestPane::new(alloc_pane_id(), tab_size));
        first.set_title("first");
        mux.add_tab_and_active_pane(&first).unwrap();
        mux.add_tab_to_window(&first, window_a).unwrap();

        let second = Arc::new(Tab::new(&tab_size));
        second.assign_pane(&TestPane::new(alloc_pane_id(), tab_size));
        second.set_title("second");
        mux.add_tab_and_active_pane(&second).unwrap();
        mux.add_tab_to_window(&second, window_a).unwrap();

        mux.get_window_mut(window_a)
            .unwrap()
            .apply_tab_order(&[second.tab_id(), first.tab_id()])
            .unwrap();
        mux.get_window_mut(window_a)
            .unwrap()
            .apply_parked_tabs(&[second.tab_id(), first.tab_id()], &[first.tab_id()])
            .unwrap();

        let window_b = *mux.new_empty_window(Some("default".to_string()), None);
        let third = Arc::new(Tab::new(&tab_size));
        third.assign_pane(&TestPane::new(alloc_pane_id(), tab_size));
        third.set_title("third");
        mux.add_tab_and_active_pane(&third).unwrap();
        mux.add_tab_to_window(&third, window_b).unwrap();

        let session = build_saved_session(&mux);

        assert_eq!(session.windows.len(), 2);
        assert_eq!(
            session.windows[0]
                .tabs
                .iter()
                .map(|tab| tab.title.as_str())
                .collect::<Vec<_>>(),
            vec!["second", "first"]
        );
        assert_eq!(
            session.windows[0]
                .tabs
                .iter()
                .map(|tab| tab.parked)
                .collect::<Vec<_>>(),
            vec![false, true]
        );
        assert_eq!(
            session.windows[1]
                .tabs
                .iter()
                .map(|tab| tab.title.as_str())
                .collect::<Vec<_>>(),
            vec!["third"]
        );
    }
}
