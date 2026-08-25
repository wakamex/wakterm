use crate::agent::{
    adopted_agent_matches_process_info, agent_observer_artifact_paths, agent_observer_watch_roots,
    default_launch_cmd_for_harness, derive_runtime_status, detect_harness_process,
    finalize_runtime_snapshot, infer_harness, native_restore_launch_command,
    prime_runtime_for_new_agent, refresh_runtime_from_harness_with_expected_session,
    restorable_session_id, AgentHarness, AgentMetadata, AgentOrigin, AgentRuntimeSnapshot,
    AgentSnapshot, AgentTabBadgeState, ExpectedAgentSession,
};
use crate::agent_event::AgentEventStore;
use crate::agent_request::{AgentRequest, AgentRequestState, AgentRequestStore};
use crate::client::{ClientId, ClientInfo, ClientViewId, ClientViewState, ClientWindowViewState};
use crate::pane::{CachePolicy, Pane, PaneId};
use crate::ssh_agent::AgentProxy;
use crate::tab::{NotifyMux, SplitRequest, Tab, TabId};
use crate::window::{Window, WindowId};
use anyhow::{anyhow, ensure, Context, Error};
use chrono::{DateTime, Utc};
use config::keyassignment::SpawnTabDomain;
use config::{configuration, ExitBehavior, GuiPosition};
use domain::{Domain, DomainId, DomainState, SplitSource};
use filedescriptor::{poll, pollfd, socketpair, AsRawSocketDescriptor, FileDescriptor, POLLIN};
#[cfg(unix)]
use libc::{c_int, SOL_SOCKET, SO_RCVBUF, SO_SNDBUF};
use log::error;
use metrics::{counter, histogram};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use parking_lot::{
    MappedRwLockReadGuard, MappedRwLockWriteGuard, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard,
};
use percent_encoding::percent_decode_str;
use portable_pty::{CommandBuilder, ExitStatus, PtySize};
use std::collections::{HashMap, HashSet};
use std::convert::TryInto;
use std::io::{Read, Write};
#[cfg(windows)]
use std::os::raw::c_int;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Weak};
use std::thread;
use std::time::{Duration, Instant};
use termwiz::escape::csi::{DecPrivateMode, DecPrivateModeCode, Device, Mode};
use termwiz::escape::{Action, CSI};
use thiserror::*;
use url::Url;
use wakterm_term::{Clipboard, ClipboardSelection, DownloadHandler, TerminalSize};
#[cfg(windows)]
use winapi::um::winsock2::{SOL_SOCKET, SO_RCVBUF, SO_SNDBUF};

pub mod activity;
pub mod agent;
pub mod agent_admission;
pub mod agent_event;
pub mod agent_request;
pub mod agent_service;
pub mod client;
pub mod codex_app_server;
pub mod connui;
pub mod domain;
pub mod localpane;
pub mod memory_report;
pub mod pane;
pub mod renderable;
pub mod session_persistence;
pub mod ssh;
pub mod ssh_agent;
pub mod tab;
pub mod termwiztermtab;
pub mod tmux;
pub mod tmux_commands;
mod tmux_pty;
pub mod window;

use crate::activity::Activity;

pub const DEFAULT_WORKSPACE: &str = "default";
const TAB_RESOURCE_STATUS_TTL: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TabResourceStatusSnapshot {
    pub sampled_at_ms: u64,
    pub tab_rss_bytes: HashMap<TabId, u64>,
}

#[derive(Default)]
struct TabResourceStatusCache {
    sampled_at: Option<Instant>,
    snapshot: TabResourceStatusSnapshot,
}

#[derive(Clone, Debug)]
pub enum MuxNotification {
    PaneOutput(PaneId),
    PaneAdded(PaneId),
    PaneRemoved(PaneId),
    WindowCreated(WindowId),
    WindowRemoved(WindowId),
    WindowInvalidated(WindowId),
    WindowWorkspaceChanged(WindowId),
    ActiveWorkspaceChanged(Arc<ClientId>),
    Alert {
        pane_id: PaneId,
        alert: wakterm_term::Alert,
    },
    Empty,
    AssignClipboard {
        pane_id: PaneId,
        selection: ClipboardSelection,
        clipboard: Option<String>,
    },
    SaveToDownloads {
        name: Option<String>,
        data: Arc<Vec<u8>>,
    },
    TabAddedToWindow {
        tab_id: TabId,
        window_id: WindowId,
    },
    PaneFocused(PaneId),
    TabResized {
        tab_id: TabId,
        origin: Option<Arc<ClientId>>,
    },
    TabOrderChanged {
        window_id: WindowId,
        tab_ids: Vec<TabId>,
        origin: Option<Arc<ClientId>>,
    },
    ParkedTabsChanged {
        window_id: WindowId,
        tab_ids: Vec<TabId>,
        parked_tab_ids: Vec<TabId>,
        origin: Option<Arc<ClientId>>,
    },
    AgentMetadataChanged {
        pane_id: PaneId,
        metadata: Option<AgentMetadata>,
    },
    TabTitleChanged {
        tab_id: TabId,
        title: String,
    },
    WindowTitleChanged {
        window_id: WindowId,
        title: String,
    },
    WorkspaceRenamed {
        old_workspace: String,
        new_workspace: String,
    },
}

fn notification_changes_saved_session(notification: &MuxNotification) -> bool {
    matches!(
        notification,
        MuxNotification::PaneAdded(_)
            | MuxNotification::PaneRemoved(_)
            | MuxNotification::WindowCreated(_)
            | MuxNotification::WindowRemoved(_)
            | MuxNotification::WindowWorkspaceChanged(_)
            | MuxNotification::TabAddedToWindow { .. }
            | MuxNotification::TabResized { .. }
            | MuxNotification::TabOrderChanged { .. }
            | MuxNotification::ParkedTabsChanged { .. }
            | MuxNotification::Alert {
                alert: wakterm_term::Alert::CurrentWorkingDirectoryChanged,
                ..
            }
    )
}

static SUB_ID: AtomicUsize = AtomicUsize::new(0);
static MUX_INSTANCE_ID: AtomicUsize = AtomicUsize::new(1);

/// Per-pane action buffer byte sizes, updated by parse_buffered_data reader threads.
/// Tracks cumulative bytes parsed since last flush — grows unbounded when
/// SynchronizedOutput is held open.
/// Used by memory_report to detect unbounded growth.
static ACTION_BUFFER_SIZES: std::sync::LazyLock<RwLock<HashMap<PaneId, Arc<AtomicUsize>>>> =
    std::sync::LazyLock::new(|| RwLock::new(HashMap::new()));

pub struct Mux {
    instance_id: usize,
    tabs: RwLock<HashMap<TabId, Arc<Tab>>>,
    panes: RwLock<HashMap<PaneId, Arc<dyn Pane>>>,
    mirrored_agent_harness_by_pane: RwLock<HashMap<PaneId, crate::agent::AgentHarness>>,
    mirrored_agent_cwd_by_pane: RwLock<HashMap<PaneId, String>>,
    mirrored_agent_snapshot_by_pane: RwLock<HashMap<PaneId, AgentSnapshot>>,
    mirrored_agent_badge_by_tab: RwLock<HashMap<TabId, AgentTabBadgeState>>,
    mirrored_tab_rss_bytes: RwLock<HashMap<TabId, u64>>,
    tab_resource_status_cache: Mutex<TabResourceStatusCache>,
    agent_panes_by_name: RwLock<HashMap<String, PaneId>>,
    agent_metadata_by_pane: RwLock<HashMap<PaneId, Arc<AgentMetadata>>>,
    detected_agent_panes: RwLock<HashSet<PaneId>>,
    agent_adoption_candidates: RwLock<HashMap<PaneId, AgentAdoptionCandidate>>,
    pending_agent_restores: RwLock<HashMap<PaneId, PendingAgentRestore>>,
    failed_agent_restores: RwLock<HashMap<PaneId, FailedAgentRestore>>,
    agent_artifact_watcher: Mutex<AgentArtifactWatcherState>,
    last_detected_agent_full_scan: Mutex<Option<Instant>>,
    agent_runtime_by_pane: RwLock<HashMap<PaneId, AgentRuntimeSnapshot>>,
    agent_observer_state_by_pane: RwLock<HashMap<PaneId, AgentObserverState>>,
    agent_observer_generation_by_pane: RwLock<HashMap<PaneId, u64>>,
    agent_request_store: AgentRequestStore,
    agent_admission_store: agent_admission::AgentAdmissionStore,
    agent_event_store: AgentEventStore,
    agent_output_reader: agent_service::AgentOutputReader,
    agent_input_generation_by_pane: RwLock<HashMap<PaneId, u64>>,
    agent_attention_seen_at: RwLock<HashMap<PaneId, DateTime<Utc>>>,
    windows: RwLock<HashMap<WindowId, Window>>,
    default_domain: RwLock<Option<Arc<dyn Domain>>>,
    domains: RwLock<HashMap<DomainId, Arc<dyn Domain>>>,
    domains_by_name: RwLock<HashMap<String, Arc<dyn Domain>>>,
    subscribers: RwLock<HashMap<usize, Box<dyn Fn(MuxNotification) -> bool + Send + Sync>>>,
    pending_pane_output_notifications: Mutex<HashSet<PaneId>>,
    banner: RwLock<Option<String>>,
    clients: RwLock<HashMap<ClientId, ClientInfo>>,
    client_views: RwLock<HashMap<ClientViewId, ClientViewState>>,
    identity: RwLock<Option<Arc<ClientId>>>,
    num_panes_by_workspace: RwLock<HashMap<String, usize>>,
    main_thread_id: std::thread::ThreadId,
    agent_observer_tx: Sender<AgentObserverCommand>,
    agent_observer_timer_tx: Sender<PaneId>,
    codex_app_server: codex_app_server::CodexAppServer,
    agent: Option<AgentProxy>,
}

const BUFSIZE: usize = 1024 * 1024;
const AGENT_HARNESS_REFRESH_THROTTLE: Duration = Duration::from_millis(250);
const AGENT_ARTIFACT_HINT_DEBOUNCE: Duration = Duration::from_millis(25);
const AGENT_ARTIFACT_DIRTY_PATH_LIMIT: usize = 1024;
const AGENT_DETECTED_FULL_SCAN_THROTTLE: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AgentTabBadgeMode {
    Identity,
    Attention,
    Turn,
    Off,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AgentRefreshPolicy {
    Throttled,
    Immediate,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AgentTitleFingerprint {
    harness: crate::agent::AgentHarness,
    transport: crate::agent::AgentTransport,
    has_session_path: bool,
    turn_state: crate::agent::AgentTurnState,
    last_turn_completed_at: Option<DateTime<Utc>>,
    attention_reason: Option<String>,
}

struct DetectedAgentState {
    pane_id: PaneId,
    tab_id: TabId,
    window_id: WindowId,
    workspace: String,
    domain_id: DomainId,
    launch_cmd: String,
    declared_cwd: String,
    adopted_pid: Option<u32>,
    adopted_start_time: Option<u64>,
    runtime: AgentRuntimeSnapshot,
    detection_source: String,
}

#[derive(Clone)]
struct AgentAdoptionCandidate {
    pane_id: PaneId,
    harness: crate::agent::AgentHarness,
    declared_cwd: String,
    launch_cmd: String,
    foreground_pid: Option<u32>,
    process_start_time: Option<u64>,
    created_at: DateTime<Utc>,
    tab_id: TabId,
    window_id: WindowId,
    workspace: String,
    domain_id: DomainId,
    detection_source: String,
}

#[derive(Clone)]
struct PendingAgentRestore {
    harness: AgentHarness,
    metadata: AgentMetadata,
    session_id: String,
}

#[derive(Clone, Copy)]
struct FailedAgentRestore {
    foreground_pid: Option<u32>,
    process_start_time: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AgentRestoreOutcome {
    Pending,
    Completed,
    Failed,
}

#[derive(Default)]
struct PendingAgentArtifactEvents {
    paths: HashSet<PathBuf>,
    refresh_all: bool,
}

impl PendingAgentArtifactEvents {
    fn record(&mut self, paths: Vec<PathBuf>) {
        if self.refresh_all {
            return;
        }
        for path in paths {
            if self.paths.len() >= AGENT_ARTIFACT_DIRTY_PATH_LIMIT && !self.paths.contains(&path) {
                self.paths.clear();
                self.refresh_all = true;
                return;
            }
            self.paths.insert(path);
        }
    }

    fn take(&mut self) -> (Vec<PathBuf>, bool) {
        let paths = std::mem::take(&mut self.paths).into_iter().collect();
        let refresh_all = std::mem::take(&mut self.refresh_all);
        (paths, refresh_all)
    }
}

struct AgentArtifactWatcherState {
    watcher: Option<RecommendedWatcher>,
    roots_by_pane: HashMap<PaneId, Vec<PathBuf>>,
    panes_by_root: HashMap<PathBuf, HashSet<PaneId>>,
    discovery_panes_by_root: HashMap<PathBuf, HashSet<PaneId>>,
    artifact_paths_by_pane: HashMap<PaneId, Vec<PathBuf>>,
    panes_by_artifact_path: HashMap<PathBuf, HashSet<PaneId>>,
    last_hint_at_by_pane: HashMap<PaneId, Instant>,
}

fn normalize_agent_artifact_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| {
        path.parent()
            .and_then(|parent| std::fs::canonicalize(parent).ok())
            .and_then(|parent| path.file_name().map(|name| parent.join(name)))
            .unwrap_or_else(|| path.to_path_buf())
    })
}

impl AgentArtifactWatcherState {
    fn new() -> Self {
        let pending_events = Arc::new(Mutex::new(PendingAgentArtifactEvents::default()));
        let worker_pending_events = Arc::clone(&pending_events);
        let (event_tx, event_rx) = mpsc::sync_channel::<()>(1);
        thread::spawn(move || {
            while event_rx.recv().is_ok() {
                thread::sleep(AGENT_ARTIFACT_HINT_DEBOUNCE);
                while event_rx.try_recv().is_ok() {}
                let (paths, refresh_all) = worker_pending_events.lock().take();
                if paths.is_empty() && !refresh_all {
                    continue;
                }
                promise::spawn::spawn_into_main_thread(async move {
                    if let Some(mux) = Mux::try_get() {
                        mux.handle_agent_artifact_batch(paths, refresh_all);
                    }
                })
                .detach();
            }
        });

        let callback_pending_events = Arc::clone(&pending_events);
        let watcher =
            notify::recommended_watcher(
                move |result: notify::Result<notify::Event>| match result {
                    Ok(event) => match event.kind {
                        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) => {
                            if !event.paths.is_empty() {
                                callback_pending_events.lock().record(event.paths);
                                let _ = event_tx.try_send(());
                            }
                        }
                        _ => {}
                    },
                    Err(err) => {
                        log::warn!("agent artifact watcher error: {err}");
                    }
                },
            );

        let watcher = match watcher {
            Ok(watcher) => Some(watcher),
            Err(err) => {
                log::warn!("unable to start agent artifact watcher: {err}");
                None
            }
        };

        Self {
            watcher,
            roots_by_pane: HashMap::new(),
            panes_by_root: HashMap::new(),
            discovery_panes_by_root: HashMap::new(),
            artifact_paths_by_pane: HashMap::new(),
            panes_by_artifact_path: HashMap::new(),
            last_hint_at_by_pane: HashMap::new(),
        }
    }

    fn watch_pane(
        &mut self,
        pane_id: PaneId,
        harness: &AgentHarness,
        cwd: &str,
        session_path: Option<&str>,
    ) {
        let roots = agent_observer_watch_roots(harness, cwd)
            .into_iter()
            .map(|path| std::fs::canonicalize(&path).unwrap_or(path))
            .collect::<Vec<_>>();

        self.unwatch_pane(pane_id);
        if roots.is_empty() || self.watcher.is_none() {
            return;
        }

        let mut watched_roots = vec![];
        for root in roots {
            let first_watcher_for_root = !self.panes_by_root.contains_key(&root);
            if first_watcher_for_root {
                let Some(watcher) = self.watcher.as_mut() else {
                    return;
                };
                if let Err(err) = watcher.watch(&root, RecursiveMode::Recursive) {
                    log::debug!("unable to watch agent artifact root {:?}: {err}", root);
                    continue;
                }
            }

            self.panes_by_root
                .entry(root.clone())
                .or_default()
                .insert(pane_id);
            self.discovery_panes_by_root
                .entry(root.clone())
                .or_default()
                .insert(pane_id);
            watched_roots.push(root);
        }

        if !watched_roots.is_empty() {
            self.roots_by_pane.insert(pane_id, watched_roots);
        }
        self.set_confirmed_artifact(pane_id, harness, session_path);
    }

    fn set_confirmed_artifact(
        &mut self,
        pane_id: PaneId,
        harness: &AgentHarness,
        session_path: Option<&str>,
    ) {
        self.remove_confirmed_artifact(pane_id);
        let Some(session_path) = session_path else {
            return;
        };
        let paths = agent_observer_artifact_paths(harness, session_path)
            .into_iter()
            .map(|path| normalize_agent_artifact_path(&path))
            .collect::<Vec<_>>();
        for path in &paths {
            self.panes_by_artifact_path
                .entry(path.clone())
                .or_default()
                .insert(pane_id);
        }
        if !paths.is_empty() {
            let roots = self
                .roots_by_pane
                .get(&pane_id)
                .cloned()
                .unwrap_or_default();
            for root in roots {
                let remove_root = self
                    .discovery_panes_by_root
                    .get_mut(&root)
                    .map(|panes| {
                        panes.remove(&pane_id);
                        panes.is_empty()
                    })
                    .unwrap_or(false);
                if remove_root {
                    self.discovery_panes_by_root.remove(&root);
                }
            }
            self.artifact_paths_by_pane.insert(pane_id, paths);
        }
    }

    fn remove_confirmed_artifact(&mut self, pane_id: PaneId) {
        if let Some(paths) = self.artifact_paths_by_pane.remove(&pane_id) {
            for path in paths {
                if let Some(panes) = self.panes_by_artifact_path.get_mut(&path) {
                    panes.remove(&pane_id);
                    if panes.is_empty() {
                        self.panes_by_artifact_path.remove(&path);
                    }
                }
            }
        }
        for root in self.roots_by_pane.get(&pane_id).into_iter().flatten() {
            self.discovery_panes_by_root
                .entry(root.clone())
                .or_default()
                .insert(pane_id);
        }
    }

    fn unwatch_pane(&mut self, pane_id: PaneId) {
        self.remove_confirmed_artifact(pane_id);
        let Some(roots) = self.roots_by_pane.remove(&pane_id) else {
            self.last_hint_at_by_pane.remove(&pane_id);
            return;
        };

        for root in roots {
            let remove_discovery_root = self
                .discovery_panes_by_root
                .get_mut(&root)
                .map(|panes| {
                    panes.remove(&pane_id);
                    panes.is_empty()
                })
                .unwrap_or(false);
            if remove_discovery_root {
                self.discovery_panes_by_root.remove(&root);
            }
            let remove_root = self
                .panes_by_root
                .get_mut(&root)
                .map(|panes| {
                    panes.remove(&pane_id);
                    panes.is_empty()
                })
                .unwrap_or(false);
            if remove_root {
                self.panes_by_root.remove(&root);
                if let Some(watcher) = self.watcher.as_mut() {
                    if let Err(err) = watcher.unwatch(&root) {
                        log::debug!("unable to unwatch agent artifact root {:?}: {err}", root);
                    }
                }
            }
        }
        self.last_hint_at_by_pane.remove(&pane_id);
    }

    fn matching_panes(&mut self, event_paths: &[PathBuf]) -> Vec<PaneId> {
        let mut matched = HashSet::new();
        for event_path in event_paths {
            if let Some(panes) = self.panes_by_artifact_path.get(event_path) {
                matched.extend(panes.iter().copied());
            } else {
                for (artifact_path, panes) in &self.panes_by_artifact_path {
                    if event_path.starts_with(artifact_path)
                        || artifact_path.starts_with(event_path)
                    {
                        matched.extend(panes.iter().copied());
                    }
                }
            }
            for (root, panes) in &self.discovery_panes_by_root {
                if event_path.starts_with(root) || root.starts_with(&event_path) {
                    matched.extend(panes.iter().copied());
                }
            }
        }

        self.debounce_matching_panes(matched)
    }

    fn all_watched_panes(&mut self) -> Vec<PaneId> {
        let matched = self
            .roots_by_pane
            .keys()
            .chain(self.artifact_paths_by_pane.keys())
            .copied()
            .collect();
        self.debounce_matching_panes(matched)
    }

    fn debounce_matching_panes(&mut self, matched: HashSet<PaneId>) -> Vec<PaneId> {
        let now = Instant::now();
        let mut panes = matched
            .into_iter()
            .filter(|pane_id| {
                let should_hint = self
                    .last_hint_at_by_pane
                    .get(pane_id)
                    .map(|last| now.duration_since(*last) >= AGENT_ARTIFACT_HINT_DEBOUNCE)
                    .unwrap_or(true);
                if should_hint {
                    self.last_hint_at_by_pane.insert(*pane_id, now);
                }
                should_hint
            })
            .collect::<Vec<_>>();
        panes.sort_unstable();
        panes
    }
}

#[derive(Clone)]
struct AgentObserverRequest {
    pane_id: PaneId,
    generation: u64,
    requested_at: Instant,
    metadata: AgentMetadata,
    runtime: AgentRuntimeSnapshot,
    expected_session: Option<ExpectedAgentSession>,
    adopted: bool,
    schedule_trailing_refresh: bool,
}

enum AgentObserverCommand {
    Refresh(AgentObserverRequest),
    Unavailable {
        metadata: AgentMetadata,
        observed_at: DateTime<Utc>,
        reason: String,
    },
}

struct AgentObserverUpdate {
    pane_id: PaneId,
    generation: u64,
    runtime: AgentRuntimeSnapshot,
    queue_delay: Duration,
    refresh_elapsed: Duration,
    schedule_trailing_refresh: bool,
}

#[derive(Default)]
struct AgentObserverState {
    latest_generation: u64,
    inflight_generation: Option<u64>,
    pending_request: Option<AgentObserverRequest>,
    last_requested_at: Option<DateTime<Utc>>,
    trailing_refresh_scheduled: bool,
}

/// This function applies parsed actions to the pane and notifies any
/// mux subscribers about the output event
fn send_actions_to_mux(pane: &Weak<dyn Pane>, dead: &Arc<AtomicBool>, actions: Vec<Action>) {
    let start = Instant::now();
    match pane.upgrade() {
        Some(pane) => {
            pane.perform_actions(actions);
            histogram!("send_actions_to_mux.perform_actions.latency").record(start.elapsed());
            Mux::notify_from_any_thread(MuxNotification::PaneOutput(pane.pane_id()));
        }
        None => {
            // Something else removed the pane from
            // the mux, so signal that we should stop
            // trying to process it in read_from_pane_pty.
            dead.store(true, Ordering::Relaxed);
        }
    }
    histogram!("send_actions_to_mux.rate").record(1.);
}

fn parse_buffered_data(pane: Weak<dyn Pane>, dead: &Arc<AtomicBool>, mut rx: FileDescriptor) {
    let mut buf = vec![0; configuration().mux_output_parser_buffer_size];
    let mut parser = termwiz::escape::parser::Parser::new();
    let mut actions = vec![];
    let mut hold = false;
    let mut action_size = 0;
    let mut delay = Duration::from_millis(configuration().mux_output_parser_coalesce_delay_ms);
    let mut deadline = None;

    // Register an atomic counter so the memory report can observe our action buffer size.
    let pane_id = pane.upgrade().map(|p| p.pane_id());
    let action_buf_gauge = Arc::new(AtomicUsize::new(0));
    if let Some(id) = pane_id {
        ACTION_BUFFER_SIZES
            .write()
            .insert(id, Arc::clone(&action_buf_gauge));
    }

    loop {
        match rx.read(&mut buf) {
            Ok(size) if size == 0 => {
                dead.store(true, Ordering::Relaxed);
                break;
            }
            Err(_) => {
                dead.store(true, Ordering::Relaxed);
                break;
            }
            Ok(size) => {
                parser.parse(&buf[0..size], |action| {
                    let mut flush = false;
                    match &action {
                        Action::CSI(CSI::Mode(Mode::SetDecPrivateMode(DecPrivateMode::Code(
                            DecPrivateModeCode::SynchronizedOutput,
                        )))) => {
                            hold = true;

                            // Flush prior actions
                            if !actions.is_empty() {
                                send_actions_to_mux(&pane, &dead, std::mem::take(&mut actions));
                                action_size = 0;
                            }
                        }
                        Action::CSI(CSI::Mode(Mode::ResetDecPrivateMode(
                            DecPrivateMode::Code(DecPrivateModeCode::SynchronizedOutput),
                        ))) => {
                            hold = false;
                            flush = true;
                        }
                        Action::CSI(CSI::Device(dev)) if matches!(**dev, Device::SoftReset) => {
                            hold = false;
                            flush = true;
                        }
                        _ => {}
                    };
                    action.append_to(&mut actions);

                    if flush && !actions.is_empty() {
                        send_actions_to_mux(&pane, &dead, std::mem::take(&mut actions));
                        action_size = 0;
                    }
                });
                action_size += size;
                action_buf_gauge.store(action_size, Ordering::Relaxed);

                // Safety valve: if SynchronizedOutput is held open and the
                // buffer has grown past 4MB, flush anyway to prevent OOM.
                // This may cause a partial frame to render, but that's better
                // than unbounded memory growth from a stuck or crashed app.
                const SYNC_OUTPUT_MAX_BYTES: usize = 4 * 1024 * 1024;
                if hold && action_size > SYNC_OUTPUT_MAX_BYTES {
                    log::warn!(
                        "SynchronizedOutput held with {}MB buffered, forcing flush",
                        action_size / (1024 * 1024)
                    );
                    send_actions_to_mux(&pane, &dead, std::mem::take(&mut actions));
                    action_size = 0;
                    action_buf_gauge.store(0, Ordering::Relaxed);
                }

                if !actions.is_empty() && !hold {
                    // If we haven't accumulated too much data,
                    // pause for a short while to increase the chances
                    // that we coalesce a full "frame" from an unoptimized
                    // TUI program
                    if action_size < buf.len() {
                        let poll_delay = match deadline {
                            None => {
                                deadline.replace(Instant::now() + delay);
                                Some(delay)
                            }
                            Some(target) => target.checked_duration_since(Instant::now()),
                        };
                        if poll_delay.is_some() {
                            let mut pfd = [pollfd {
                                fd: rx.as_socket_descriptor(),
                                events: POLLIN,
                                revents: 0,
                            }];
                            if let Ok(1) = poll(&mut pfd, poll_delay) {
                                // We can read now without blocking, so accumulate
                                // more data into actions
                                continue;
                            }

                            // Not readable in time: let the data we have flow into
                            // the terminal model
                        }
                    }

                    send_actions_to_mux(&pane, &dead, std::mem::take(&mut actions));
                    deadline = None;
                    action_size = 0;
                }

                let config = configuration();
                buf.resize(config.mux_output_parser_buffer_size, 0);
                delay = Duration::from_millis(config.mux_output_parser_coalesce_delay_ms);
            }
        }
    }

    // Don't forget to send anything that we might have buffered
    // to be displayed before we return from here; this is important
    // for very short lived commands so that we don't forget to
    // display what they displayed.
    if !actions.is_empty() {
        send_actions_to_mux(&pane, &dead, std::mem::take(&mut actions));
    }

    // Clean up gauge
    if let Some(id) = pane_id {
        ACTION_BUFFER_SIZES.write().remove(&id);
    }
}

fn set_socket_buffer(fd: &mut FileDescriptor, option: i32, size: usize) -> anyhow::Result<()> {
    let size = size as c_int;
    let socklen = std::mem::size_of_val(&size);
    unsafe {
        let res = libc::setsockopt(
            fd.as_socket_descriptor(),
            SOL_SOCKET,
            option,
            &size as *const c_int as *const _,
            socklen as _,
        );
        if res == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error()).context("setsockopt")
        }
    }
}

fn allocate_socketpair() -> anyhow::Result<(FileDescriptor, FileDescriptor)> {
    let (mut tx, mut rx) = socketpair().context("socketpair")?;
    set_socket_buffer(&mut tx, SO_SNDBUF, BUFSIZE)
        .context("SO_SNDBUF")
        .ok();
    set_socket_buffer(&mut rx, SO_RCVBUF, BUFSIZE)
        .context("SO_RCVBUF")
        .ok();
    Ok((tx, rx))
}

/// This function is run in a separate thread; its purpose is to perform
/// blocking reads from the pty (non-blocking reads are not portable to
/// all platforms and pty/tty types), parse the escape sequences and
/// relay the actions to the mux thread to apply them to the pane.
fn read_from_pane_pty(
    pane: Weak<dyn Pane>,
    banner: Option<String>,
    mut reader: Box<dyn std::io::Read>,
) {
    let mut buf = vec![0; BUFSIZE];

    // This is used to signal that an error occurred either in this thread,
    // or in the main mux thread.  If `true`, this thread will terminate.
    let dead = Arc::new(AtomicBool::new(false));

    let (pane_id, exit_behavior) = match pane.upgrade() {
        Some(pane) => (pane.pane_id(), pane.exit_behavior()),
        None => return,
    };

    let (mut tx, rx) = match allocate_socketpair() {
        Ok(pair) => pair,
        Err(err) => {
            log::error!("read_from_pane_pty: Unable to allocate a socketpair: {err:#}");
            localpane::emit_output_for_pane(
                pane_id,
                &format!(
                    "⚠️  wakterm: read_from_pane_pty: \
                    Unable to allocate a socketpair: {err:#}"
                ),
            );
            return;
        }
    };

    std::thread::spawn({
        let dead = Arc::clone(&dead);
        move || parse_buffered_data(pane, &dead, rx)
    });

    if let Some(banner) = banner {
        tx.write_all(banner.as_bytes()).ok();
    }

    while !dead.load(Ordering::Relaxed) {
        match reader.read(&mut buf) {
            Ok(size) if size == 0 => {
                log::trace!("read_pty EOF: pane_id {}", pane_id);
                break;
            }
            Err(err) => {
                error!("read_pty failed: pane {} {:?}", pane_id, err);
                break;
            }
            Ok(size) => {
                histogram!("read_from_pane_pty.bytes.rate").record(size as f64);
                log::trace!("read_pty pane {pane_id} read {size} bytes");
                if let Err(err) = tx.write_all(&buf[..size]) {
                    error!(
                        "read_pty failed to write to parser: pane {} {:?}",
                        pane_id, err
                    );
                    break;
                }
            }
        }
    }

    match exit_behavior.unwrap_or_else(|| configuration().exit_behavior) {
        ExitBehavior::Hold | ExitBehavior::CloseOnCleanExit => {
            // We don't know if we can unilaterally close
            // this pane right now, so don't!
            promise::spawn::spawn_into_main_thread(async move {
                let mux = Mux::get();
                log::trace!("checking for dead windows after EOF on pane {}", pane_id);
                mux.prune_dead_windows();
            })
            .detach();
        }
        ExitBehavior::Close => {
            promise::spawn::spawn_into_main_thread(async move {
                let mux = Mux::get();
                mux.remove_pane(pane_id);
            })
            .detach();
        }
    }

    dead.store(true, Ordering::Relaxed);
}

fn spawn_agent_observer_worker(event_store: AgentEventStore) -> Sender<AgentObserverCommand> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || run_agent_observer_worker(rx, event_store));
    tx
}

fn spawn_agent_observer_timer(mux_instance_id: usize) -> Sender<PaneId> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut deadlines = HashMap::<PaneId, Instant>::new();
        loop {
            let received = match deadlines.values().min().copied() {
                Some(deadline) => {
                    rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                }
                None => match rx.recv() {
                    Ok(pane_id) => {
                        deadlines.insert(pane_id, Instant::now() + AGENT_HARNESS_REFRESH_THROTTLE);
                        continue;
                    }
                    Err(_) => break,
                },
            };
            match received {
                Ok(pane_id) => {
                    deadlines.insert(pane_id, Instant::now() + AGENT_HARNESS_REFRESH_THROTTLE);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }

            let now = Instant::now();
            let due = deadlines
                .iter()
                .filter_map(|(pane_id, deadline)| (*deadline <= now).then_some(*pane_id))
                .collect::<Vec<_>>();
            for pane_id in &due {
                deadlines.remove(pane_id);
            }
            if due.is_empty() {
                continue;
            }
            promise::spawn::spawn_into_main_thread(async move {
                let Some(mux) = Mux::try_get() else {
                    return;
                };
                if mux.instance_id != mux_instance_id {
                    return;
                }
                for pane_id in due {
                    let should_refresh = {
                        let mut states = mux.agent_observer_state_by_pane.write();
                        let Some(state) = states.get_mut(&pane_id) else {
                            continue;
                        };
                        if !state.trailing_refresh_scheduled {
                            continue;
                        }
                        state.trailing_refresh_scheduled = false;
                        true
                    };
                    if should_refresh {
                        mux.refresh_agent_runtime_for_pane_with_update_inner(
                            pane_id,
                            false,
                            AgentRefreshPolicy::Throttled,
                            false,
                            |_| {},
                        );
                    }
                }
            })
            .detach();
        }
    });
    tx
}

fn run_agent_observer_worker(rx: Receiver<AgentObserverCommand>, event_store: AgentEventStore) {
    let mut event_writer = None;
    while let Ok(command) = rx.recv() {
        if event_writer.is_none() {
            match event_store.writer() {
                Ok(writer) => event_writer = Some(writer),
                Err(err) => log::error!("failed to open agent event writer: {err:#}"),
            }
        }
        let mut writer_failed = false;
        match command {
            AgentObserverCommand::Refresh(request) => {
                counter!("mux.agent_observer.refresh.rate").increment(1);
                let started = Instant::now();
                let mut runtime = request.runtime;
                refresh_runtime_from_harness_with_expected_session(
                    &mut runtime,
                    &request.metadata,
                    request.expected_session.as_ref(),
                );
                if request.adopted {
                    if let Some(writer) = event_writer.as_mut() {
                        if let Err(err) = writer.observe_agent(&request.metadata, &runtime) {
                            log::error!("failed to persist agent observation events: {err:#}");
                            writer_failed = true;
                        }
                    }
                }
                let refresh_elapsed = started.elapsed();
                let queue_delay = started.saturating_duration_since(request.requested_at);
                histogram!("mux.agent_observer.refresh.latency").record(refresh_elapsed);
                histogram!("mux.agent_observer.refresh.queue_delay").record(queue_delay);

                let update = AgentObserverUpdate {
                    pane_id: request.pane_id,
                    generation: request.generation,
                    runtime,
                    queue_delay,
                    refresh_elapsed,
                    schedule_trailing_refresh: request.schedule_trailing_refresh,
                };

                promise::spawn::spawn_into_main_thread(async move {
                    if let Some(mux) = Mux::try_get() {
                        mux.apply_agent_observer_update(update);
                    }
                })
                .detach();
            }
            AgentObserverCommand::Unavailable {
                metadata,
                observed_at,
                reason,
            } => {
                if let Some(writer) = event_writer.as_mut() {
                    if let Err(err) = writer.record_unavailable(&metadata, observed_at, &reason) {
                        log::error!("failed to persist unavailable agent event: {err:#}");
                        writer_failed = true;
                    }
                }
            }
        }
        if writer_failed {
            event_writer = None;
        }
    }
}

lazy_static::lazy_static! {
    static ref MUX: Mutex<Option<Arc<Mux>>> = Mutex::new(None);
}

#[cfg(test)]
lazy_static::lazy_static! {
    pub(crate) static ref TEST_MUX_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
}

#[cfg(test)]
pub(crate) struct TestMuxGuard;

#[cfg(test)]
impl Drop for TestMuxGuard {
    fn drop(&mut self) {
        Mux::shutdown();
    }
}

pub struct MuxWindowBuilder {
    window_id: WindowId,
    activity: Option<Activity>,
    notified: bool,
}

impl MuxWindowBuilder {
    fn notify(&mut self) {
        if self.notified {
            return;
        }
        self.notified = true;
        let activity = self.activity.take().unwrap();
        let window_id = self.window_id;
        let mux = Mux::get();
        if mux.is_main_thread() {
            // If we're already on the mux thread, just send the notification
            // immediately.
            // This is super important for Wayland; if we push it to the
            // spawn queue below then the extra milliseconds of delay
            // causes it to get confused and shutdown the connection!?
            mux.notify(MuxNotification::WindowCreated(window_id));
        } else {
            promise::spawn::spawn_into_main_thread(async move {
                if let Some(mux) = Mux::try_get() {
                    mux.notify(MuxNotification::WindowCreated(window_id));
                    drop(activity);
                }
            })
            .detach();
        }
    }
}

impl Drop for MuxWindowBuilder {
    fn drop(&mut self) {
        self.notify();
    }
}

impl std::ops::Deref for MuxWindowBuilder {
    type Target = WindowId;

    fn deref(&self) -> &WindowId {
        &self.window_id
    }
}

impl Mux {
    pub fn new(default_domain: Option<Arc<dyn Domain>>) -> Self {
        #[cfg(test)]
        let agent_state_path = std::env::temp_dir().join(format!(
            "wakterm-agent-test-{}.sqlite3",
            uuid::Uuid::new_v4()
        ));
        #[cfg(not(test))]
        let agent_state_path = config::DATA_DIR.join("agent-requests.sqlite3");
        Self::new_with_agent_state_path(default_domain, agent_state_path)
    }

    #[doc(hidden)]
    pub fn new_with_agent_state_path(
        default_domain: Option<Arc<dyn Domain>>,
        agent_state_path: std::path::PathBuf,
    ) -> Self {
        let mut domains = HashMap::new();
        let mut domains_by_name = HashMap::new();
        if let Some(default_domain) = default_domain.as_ref() {
            domains.insert(default_domain.domain_id(), Arc::clone(default_domain));

            domains_by_name.insert(
                default_domain.domain_name().to_string(),
                Arc::clone(default_domain),
            );
        }

        let agent = if config::configuration().mux_enable_ssh_agent {
            Some(AgentProxy::new())
        } else {
            None
        };
        let agent_event_store = AgentEventStore::new(agent_state_path.clone());
        let agent_observer_tx = spawn_agent_observer_worker(agent_event_store.clone());
        let agent_output_reader = agent_service::AgentOutputReader::new();

        let instance_id = MUX_INSTANCE_ID.fetch_add(1, Ordering::Relaxed);
        let agent_observer_timer_tx = spawn_agent_observer_timer(instance_id);
        Self {
            instance_id,
            tabs: RwLock::new(HashMap::new()),
            panes: RwLock::new(HashMap::new()),
            mirrored_agent_harness_by_pane: RwLock::new(HashMap::new()),
            mirrored_agent_cwd_by_pane: RwLock::new(HashMap::new()),
            mirrored_agent_snapshot_by_pane: RwLock::new(HashMap::new()),
            mirrored_agent_badge_by_tab: RwLock::new(HashMap::new()),
            mirrored_tab_rss_bytes: RwLock::new(HashMap::new()),
            tab_resource_status_cache: Mutex::new(TabResourceStatusCache::default()),
            agent_panes_by_name: RwLock::new(HashMap::new()),
            agent_metadata_by_pane: RwLock::new(HashMap::new()),
            detected_agent_panes: RwLock::new(HashSet::new()),
            agent_adoption_candidates: RwLock::new(HashMap::new()),
            pending_agent_restores: RwLock::new(HashMap::new()),
            failed_agent_restores: RwLock::new(HashMap::new()),
            agent_artifact_watcher: Mutex::new(AgentArtifactWatcherState::new()),
            last_detected_agent_full_scan: Mutex::new(None),
            agent_runtime_by_pane: RwLock::new(HashMap::new()),
            agent_observer_state_by_pane: RwLock::new(HashMap::new()),
            agent_observer_generation_by_pane: RwLock::new(HashMap::new()),
            agent_request_store: AgentRequestStore::new(agent_state_path.clone()),
            agent_admission_store: agent_admission::AgentAdmissionStore::new(agent_state_path),
            agent_event_store,
            agent_output_reader,
            agent_input_generation_by_pane: RwLock::new(HashMap::new()),
            agent_attention_seen_at: RwLock::new(HashMap::new()),
            windows: RwLock::new(HashMap::new()),
            default_domain: RwLock::new(default_domain),
            domains_by_name: RwLock::new(domains_by_name),
            domains: RwLock::new(domains),
            subscribers: RwLock::new(HashMap::new()),
            pending_pane_output_notifications: Mutex::new(HashSet::new()),
            banner: RwLock::new(None),
            clients: RwLock::new(HashMap::new()),
            client_views: RwLock::new(HashMap::new()),
            identity: RwLock::new(None),
            num_panes_by_workspace: RwLock::new(HashMap::new()),
            main_thread_id: std::thread::current().id(),
            agent_observer_tx,
            agent_observer_timer_tx,
            codex_app_server: codex_app_server::CodexAppServer::new(instance_id),
            agent,
        }
    }

    pub fn prepare_codex_app_server_launch(
        &self,
        request: codex_app_server::PrepareCodexLaunch,
    ) -> anyhow::Result<codex_app_server::PreparedCodexLaunch> {
        self.codex_app_server.prepare(request)
    }

    pub(crate) fn apply_codex_app_server_notification(&self, message: &serde_json::Value) {
        codex_app_server::apply_notification_to_runtime(self, message);
    }

    pub(crate) fn codex_app_server_disconnected(&self) {
        self.codex_app_server.mark_disconnected();
        let managed: Vec<_> = self
            .agent_metadata_by_pane
            .read()
            .iter()
            .filter_map(|(pane_id, metadata)| {
                metadata.codex_app_server.as_ref().map(|session| {
                    (
                        *pane_id,
                        codex_app_server::RecoveryThread {
                            name: metadata.name.clone(),
                            cwd: metadata.declared_cwd.clone(),
                            session: session.clone(),
                        },
                    )
                })
            })
            .collect();
        let recovery = self.codex_app_server.recover(
            &managed
                .iter()
                .map(|(_, thread)| thread.clone())
                .collect::<Vec<_>>(),
        );
        let mut runtimes = self.agent_runtime_by_pane.write();
        for (pane_id, thread) in &managed {
            if let Some(runtime) = runtimes.get_mut(pane_id) {
                let error = match recovery.as_ref() {
                    Err(err) => Some(format!("{err:#}")),
                    Ok(failures) => failures.get(&thread.session.thread_id).cloned(),
                };
                if let Some(error) = error {
                    runtime.status = crate::agent::AgentStatus::Errored;
                    runtime.observer_error =
                        Some(format!("shared Codex app-server recovery failed: {error}"));
                } else {
                    runtime.observer_error = None;
                }
                runtime.observed_at = Utc::now();
            }
        }
        drop(runtimes);
        let pane_ids: Vec<_> = self
            .agent_metadata_by_pane
            .read()
            .iter()
            .filter_map(|(pane_id, metadata)| metadata.codex_app_server.as_ref().map(|_| *pane_id))
            .collect();
        for pane_id in pane_ids {
            if let Some((_, _, tab_id)) = self.resolve_pane_id(pane_id) {
                self.notify_tab_title_changed(tab_id);
            }
        }
    }

    /// Begin one authoritative mux runtime epoch for durable agent lifecycle
    /// projection. Call this once for a mux that will serve Agent API clients,
    /// after construction and before accepting requests.
    pub fn start_agent_event_runtime_epoch(&self) -> anyhow::Result<()> {
        self.agent_event_store.start_runtime_epoch()
    }

    fn get_default_workspace(&self) -> String {
        let config = configuration();
        config
            .default_workspace
            .as_deref()
            .unwrap_or(DEFAULT_WORKSPACE)
            .to_string()
    }

    pub fn is_main_thread(&self) -> bool {
        std::thread::current().id() == self.main_thread_id
    }

    fn recompute_pane_count(&self) {
        let mut count = HashMap::new();
        for window in self.windows.read().values() {
            let workspace = window.get_workspace();
            for tab in window.iter() {
                *count.entry(workspace.to_string()).or_insert(0) += match tab.count_panes() {
                    Some(n) => n,
                    None => {
                        // Busy: abort this and we'll retry later
                        return;
                    }
                };
            }
        }
        *self.num_panes_by_workspace.write() = count;
    }

    pub fn client_had_input(&self, client_id: &ClientId) {
        if let Some(info) = self.clients.write().get_mut(client_id) {
            info.update_last_input();
        }
        if let Some(agent) = &self.agent {
            agent.update_target();
        }
    }

    pub fn record_input_for_current_identity(&self) {
        if let Some(ident) = self.identity.read().as_ref() {
            self.client_had_input(ident);
        }
    }

    pub fn active_view_id(&self) -> Option<Arc<ClientViewId>> {
        let ident = self.identity.read().clone()?;
        self.active_view_id_for_client(ident.as_ref())
    }

    pub fn active_view_id_for_client(&self, client_id: &ClientId) -> Option<Arc<ClientViewId>> {
        self.clients
            .read()
            .get(client_id)
            .map(|info| info.view_id.clone())
    }

    pub fn client_window_view_state_for_view(
        &self,
        view_id: &ClientViewId,
    ) -> HashMap<WindowId, ClientWindowViewState> {
        self.client_views
            .read()
            .get(view_id)
            .map(|state| state.windows.clone())
            .unwrap_or_default()
    }

    pub fn client_window_view_state_for_current_identity(
        &self,
    ) -> HashMap<WindowId, ClientWindowViewState> {
        self.active_view_id()
            .map(|view_id| self.client_window_view_state_for_view(view_id.as_ref()))
            .unwrap_or_default()
    }

    pub fn set_agent_metadata(
        &self,
        pane_id: PaneId,
        metadata: AgentMetadata,
    ) -> anyhow::Result<()> {
        self.set_agent_metadata_with_initial_refresh(pane_id, metadata)
    }

    pub fn restore_agent_metadata(
        &self,
        pane_id: PaneId,
        metadata: AgentMetadata,
    ) -> anyhow::Result<()> {
        self.set_agent_metadata_with_initial_refresh(pane_id, metadata)
    }

    pub fn register_agent_restore_intent(
        &self,
        pane_id: PaneId,
        harness: AgentHarness,
        metadata: AgentMetadata,
        session_id: String,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.get_pane(pane_id).is_some(),
            "pane {} is invalid",
            pane_id
        );
        anyhow::ensure!(
            matches!(harness, AgentHarness::Claude | AgentHarness::Codex),
            "automatic restore is not implemented for {:?}",
            harness
        );
        anyhow::ensure!(
            infer_harness(&metadata.launch_cmd, None) == harness,
            "restore requires a launch command for {:?}",
            harness
        );
        anyhow::ensure!(
            !session_id.trim().is_empty(),
            "agent session ID must not be empty"
        );
        self.pending_agent_restores.write().insert(
            pane_id,
            PendingAgentRestore {
                harness,
                metadata,
                session_id,
            },
        );
        crate::session_persistence::request_session_save();
        Ok(())
    }

    pub(crate) fn agent_restore_intent_for_pane(
        &self,
        pane_id: PaneId,
    ) -> Option<(AgentHarness, AgentMetadata, String)> {
        if let Some(pending) = self.pending_agent_restores.read().get(&pane_id).cloned() {
            return Some((pending.harness, pending.metadata, pending.session_id));
        }
        let metadata = self.get_agent_metadata_for_pane(pane_id)?;
        if let Some(session) = metadata.codex_app_server.as_ref() {
            return Some((
                AgentHarness::Codex,
                (*metadata).clone(),
                session.session_id.clone(),
            ));
        }
        let runtime = self.agent_runtime_by_pane.read().get(&pane_id).cloned()?;
        let harness = runtime.harness;
        if !matches!(harness, AgentHarness::Claude | AgentHarness::Codex) {
            return None;
        }
        let session_path = runtime.session_path.as_deref()?;
        let session_id = restorable_session_id(&harness, Path::new(session_path))
            .ok()
            .flatten()?;
        let mut restored_metadata = (*metadata).clone();
        if let Some(launch_cmd) = self
            .get_pane(pane_id)
            .and_then(|pane| pane.get_foreground_process_info(CachePolicy::AllowStale))
            .as_ref()
            .and_then(|process| native_restore_launch_command(&harness, process))
        {
            restored_metadata.launch_cmd = launch_cmd;
        } else if infer_harness(&restored_metadata.launch_cmd, None) != harness {
            restored_metadata.launch_cmd = default_launch_cmd_for_harness(&harness)?.to_string();
        }
        Some((harness, restored_metadata, session_id))
    }

    pub fn set_mirrored_agent_metadata(&self, pane_id: PaneId, metadata: Option<&AgentMetadata>) {
        let harness = metadata.and_then(|metadata| {
            let harness = infer_harness(&metadata.launch_cmd, None);
            if matches!(harness, crate::agent::AgentHarness::Unknown) {
                None
            } else {
                Some(harness)
            }
        });
        match harness {
            Some(harness) => {
                self.mirrored_agent_harness_by_pane
                    .write()
                    .insert(pane_id, harness);
            }
            None => {
                self.mirrored_agent_harness_by_pane.write().remove(&pane_id);
            }
        }
        match metadata {
            Some(metadata) => {
                self.mirrored_agent_cwd_by_pane
                    .write()
                    .insert(pane_id, metadata.declared_cwd.clone());
            }
            None => {
                self.mirrored_agent_cwd_by_pane.write().remove(&pane_id);
                self.mirrored_agent_snapshot_by_pane
                    .write()
                    .remove(&pane_id);
            }
        }
    }

    pub fn set_mirrored_agent_badge(&self, tab_id: TabId, badge: AgentTabBadgeState) {
        if badge == AgentTabBadgeState::default() {
            self.mirrored_agent_badge_by_tab.write().remove(&tab_id);
        } else {
            self.mirrored_agent_badge_by_tab
                .write()
                .insert(tab_id, badge);
        }
    }

    pub fn set_mirrored_tab_rss(&self, tab_id: TabId, rss_bytes: Option<u64>) {
        match rss_bytes {
            Some(rss_bytes) => {
                self.mirrored_tab_rss_bytes
                    .write()
                    .insert(tab_id, rss_bytes);
            }
            None => {
                self.mirrored_tab_rss_bytes.write().remove(&tab_id);
            }
        }
        self.invalidate_tab_resource_status();
    }

    pub fn set_mirrored_agent_snapshot(&self, pane_id: PaneId, snapshot: Option<AgentSnapshot>) {
        match snapshot {
            Some(snapshot) => {
                self.mirrored_agent_snapshot_by_pane
                    .write()
                    .insert(pane_id, snapshot);
            }
            None => {
                self.mirrored_agent_snapshot_by_pane
                    .write()
                    .remove(&pane_id);
            }
        }
    }

    pub fn mirrored_agent_snapshots_for_window(&self, window_id: WindowId) -> Vec<AgentSnapshot> {
        self.mirrored_agent_snapshot_by_pane
            .read()
            .values()
            .filter(|snapshot| snapshot.window_id == window_id)
            .cloned()
            .collect()
    }

    fn install_agent_metadata_runtime(
        &self,
        pane_id: PaneId,
        metadata: AgentMetadata,
        runtime: AgentRuntimeSnapshot,
    ) -> anyhow::Result<()> {
        let foreground_process_info = self
            .get_pane(pane_id)
            .and_then(|pane| pane.get_foreground_process_info(CachePolicy::AllowStale));
        self.install_agent_metadata_runtime_with_process_info(
            pane_id,
            metadata,
            runtime,
            foreground_process_info.as_ref(),
        )
    }

    fn install_agent_metadata_runtime_without_process_identity(
        &self,
        pane_id: PaneId,
        metadata: AgentMetadata,
        runtime: AgentRuntimeSnapshot,
    ) -> anyhow::Result<()> {
        self.install_agent_metadata_runtime_with_process_info(pane_id, metadata, runtime, None)
    }

    fn install_agent_metadata_runtime_with_process_info(
        &self,
        pane_id: PaneId,
        mut metadata: AgentMetadata,
        runtime: AgentRuntimeSnapshot,
        foreground_process_info: Option<&procinfo::LocalProcessInfo>,
    ) -> anyhow::Result<()> {
        self.detected_agent_panes.write().remove(&pane_id);
        self.agent_adoption_candidates.write().remove(&pane_id);
        self.pending_agent_restores.write().remove(&pane_id);
        self.failed_agent_restores.write().remove(&pane_id);
        self.agent_artifact_watcher.lock().unwatch_pane(pane_id);
        let pane = self
            .get_pane(pane_id)
            .ok_or_else(|| anyhow!("pane {} is invalid", pane_id))?;
        let foreground_process_name = pane.get_foreground_process_name(CachePolicy::AllowStale);
        let tty_name = pane.tty_name();
        let terminal_progress = pane.get_progress();
        let alive = !pane.is_dead();
        if foreground_process_info.is_some() {
            Self::stamp_adopted_process_identity(&mut metadata, foreground_process_info);
        }

        let mut names = self.agent_panes_by_name.write();
        let mut metadata_by_pane = self.agent_metadata_by_pane.write();

        if let Some(existing_pane_id) = names.get(&metadata.name).copied() {
            anyhow::ensure!(
                existing_pane_id == pane_id,
                "agent name {} is already assigned to pane {}",
                metadata.name,
                existing_pane_id
            );
        }

        if let Some(existing) = metadata_by_pane.get(&pane_id) {
            if existing.name != metadata.name {
                names.remove(&existing.name);
            }
        }

        names.insert(metadata.name.clone(), pane_id);
        let mut runtime = runtime;
        self.agent_attention_seen_at.write().remove(&pane_id);
        runtime.alive = alive;
        runtime.foreground_process_name = foreground_process_name.clone();
        runtime.tty_name = tty_name;
        runtime.terminal_progress = terminal_progress;
        runtime.harness = infer_harness(&metadata.launch_cmd, foreground_process_name.as_deref());
        let observer_harness = runtime.harness.clone();
        let observer_cwd = metadata.declared_cwd.clone();
        let observer_session_path = runtime.session_path.clone();
        self.agent_runtime_by_pane.write().insert(pane_id, runtime);
        metadata_by_pane.insert(pane_id, Arc::new(metadata));
        drop(metadata_by_pane);
        drop(names);
        self.agent_artifact_watcher.lock().watch_pane(
            pane_id,
            &observer_harness,
            &observer_cwd,
            observer_session_path.as_deref(),
        );
        crate::session_persistence::request_session_save();
        Ok(())
    }

    fn set_agent_metadata_with_initial_refresh(
        &self,
        pane_id: PaneId,
        metadata: AgentMetadata,
    ) -> anyhow::Result<()> {
        let tab_id = self.resolve_pane_id(pane_id).map(|(_, _, tab_id)| tab_id);
        let foreground_process_name = self
            .get_pane(pane_id)
            .and_then(|pane| pane.get_foreground_process_name(CachePolicy::AllowStale));
        let mut runtime = self
            .agent_runtime_by_pane
            .write()
            .remove(&pane_id)
            .unwrap_or_else(|| AgentRuntimeSnapshot::new(&metadata));
        prime_runtime_for_new_agent(&mut runtime, &metadata, foreground_process_name.as_deref());
        self.install_agent_metadata_runtime(pane_id, metadata, runtime)?;

        self.refresh_agent_runtime_for_pane_with_update(
            pane_id,
            false,
            AgentRefreshPolicy::Throttled,
            |_| {},
        );
        self.notify(MuxNotification::AgentMetadataChanged {
            pane_id,
            metadata: self
                .get_agent_metadata_for_pane(pane_id)
                .map(|metadata| (*metadata).clone()),
        });
        if let Some(tab_id) = tab_id {
            self.notify_tab_title_changed(tab_id);
        }
        Ok(())
    }

    pub fn clear_agent_metadata(&self, pane_id: PaneId) -> Option<Arc<AgentMetadata>> {
        self.mirrored_agent_harness_by_pane.write().remove(&pane_id);
        self.mirrored_agent_cwd_by_pane.write().remove(&pane_id);
        let tab_id = self.resolve_pane_id(pane_id).map(|(_, _, tab_id)| tab_id);
        let metadata = {
            let mut metadata_by_pane = self.agent_metadata_by_pane.write();
            metadata_by_pane.remove(&pane_id)
        };
        let removed_pending = self
            .pending_agent_restores
            .write()
            .remove(&pane_id)
            .is_some();
        self.failed_agent_restores.write().remove(&pane_id);
        let Some(metadata) = metadata else {
            if removed_pending {
                crate::session_persistence::request_session_save();
            }
            return None;
        };
        let _ = self
            .agent_observer_tx
            .send(AgentObserverCommand::Unavailable {
                metadata: (*metadata).clone(),
                observed_at: Utc::now(),
                reason: "metadata_cleared".to_string(),
            });
        self.agent_panes_by_name.write().remove(&metadata.name);
        self.agent_runtime_by_pane.write().remove(&pane_id);
        self.agent_adoption_candidates.write().remove(&pane_id);
        self.agent_artifact_watcher.lock().unwatch_pane(pane_id);
        self.agent_observer_state_by_pane.write().remove(&pane_id);
        self.agent_input_generation_by_pane.write().remove(&pane_id);
        self.agent_attention_seen_at.write().remove(&pane_id);
        crate::session_persistence::request_session_save();
        self.notify(MuxNotification::AgentMetadataChanged {
            pane_id,
            metadata: None,
        });
        if let Some(tab_id) = tab_id {
            self.notify_tab_title_changed(tab_id);
        }
        Some(metadata)
    }

    pub fn get_agent_metadata_for_pane(&self, pane_id: PaneId) -> Option<Arc<AgentMetadata>> {
        self.agent_metadata_by_pane.read().get(&pane_id).cloned()
    }

    pub fn cached_agent_harness_for_pane(
        &self,
        pane_id: PaneId,
    ) -> Option<crate::agent::AgentHarness> {
        if let Some(harness) = self.mirrored_agent_harness_by_pane.read().get(&pane_id) {
            return Some(harness.clone());
        }

        if let Some(snapshot) = self.mirrored_agent_snapshot_by_pane.read().get(&pane_id) {
            if !matches!(
                snapshot.runtime.harness,
                crate::agent::AgentHarness::Unknown
            ) {
                return Some(snapshot.runtime.harness.clone());
            }
            let harness = infer_harness(&snapshot.metadata.launch_cmd, None);
            if !matches!(harness, crate::agent::AgentHarness::Unknown) {
                return Some(harness);
            }
        }

        if let Some(runtime) = self.agent_runtime_by_pane.read().get(&pane_id) {
            if !matches!(runtime.harness, crate::agent::AgentHarness::Unknown) {
                return Some(runtime.harness.clone());
            }
        }

        let metadata = self.get_agent_metadata_for_pane(pane_id)?;
        let harness = infer_harness(&metadata.launch_cmd, None);
        if matches!(harness, crate::agent::AgentHarness::Unknown) {
            None
        } else {
            Some(harness)
        }
    }

    fn agent_auto_adopt_on_confirmed_session_match() -> bool {
        configuration().agent_auto_adopt_on_confirmed_session_match
    }

    fn harness_slug(harness: &crate::agent::AgentHarness) -> &'static str {
        match harness {
            crate::agent::AgentHarness::Agy => "agy",
            crate::agent::AgentHarness::Claude => "claude",
            crate::agent::AgentHarness::Codex => "codex",
            crate::agent::AgentHarness::Gemini => "gemini",
            crate::agent::AgentHarness::Opencode => "opencode",
            crate::agent::AgentHarness::Unknown => "agent",
        }
    }

    fn slugify_agent_name_piece(value: &str) -> String {
        let mut slug = String::new();
        let mut last_was_underscore = false;
        for ch in value.chars() {
            let lower = ch.to_ascii_lowercase();
            if lower.is_ascii_alphanumeric() {
                slug.push(lower);
                last_was_underscore = false;
            } else if !last_was_underscore {
                slug.push('_');
                last_was_underscore = true;
            }
        }
        slug.trim_matches('_').to_string()
    }

    fn cwd_leaf_for_agent_name(declared_cwd: &str) -> Option<String> {
        let normalized = if declared_cwd.starts_with("file://") {
            Url::parse(declared_cwd)
                .ok()
                .map(|url| {
                    url.to_file_path()
                        .ok()
                        .map(|path| path.to_string_lossy().to_string())
                        .unwrap_or_else(|| url.path().to_string())
                })
                .unwrap_or_else(|| declared_cwd.to_string())
        } else {
            declared_cwd.to_string()
        };
        std::path::Path::new(&normalized)
            .file_name()
            .and_then(|name| name.to_str())
            .map(Self::slugify_agent_name_piece)
            .filter(|leaf| !leaf.is_empty())
    }

    fn detected_agent_name_base(
        harness: &crate::agent::AgentHarness,
        declared_cwd: &str,
    ) -> String {
        let harness = Self::harness_slug(harness);
        match Self::cwd_leaf_for_agent_name(declared_cwd) {
            Some(leaf) if leaf != harness => format!("{leaf}_{harness}"),
            _ => harness.to_string(),
        }
    }

    fn next_available_agent_name(taken_names: &HashSet<String>, base_name: &str) -> String {
        if !taken_names.contains(base_name) {
            return base_name.to_string();
        }

        for suffix in 2usize.. {
            let candidate = format!("{base_name}{suffix}");
            if !taken_names.contains(&candidate) {
                return candidate;
            }
        }

        unreachable!("unbounded numeric suffix loop should always find a free agent name")
    }

    fn detected_agent_created_at(runtime: &AgentRuntimeSnapshot) -> DateTime<Utc> {
        runtime
            .last_progress_at
            .or(runtime.last_turn_completed_at)
            .unwrap_or(runtime.observed_at)
    }

    fn pane_declared_cwd(
        pane: &Arc<dyn Pane>,
        process_info: Option<&procinfo::LocalProcessInfo>,
    ) -> Option<String> {
        if let Some(url) = pane.get_current_working_dir(CachePolicy::AllowStale) {
            if url.scheme() == "file" {
                if url.host_str().is_some() {
                    return Some(url.path().to_string());
                }
                return url
                    .to_file_path()
                    .ok()
                    .map(|path| path.to_string_lossy().to_string())
                    .or_else(|| Some(url.path().to_string()));
            }
            return Some(url.to_string());
        }

        process_info.and_then(|process| {
            if process.cwd.as_os_str().is_empty() {
                None
            } else {
                Some(process.cwd.to_string_lossy().to_string())
            }
        })
    }

    fn clear_detected_agent_info(&self, pane_id: PaneId) {
        self.mirrored_agent_harness_by_pane.write().remove(&pane_id);
        self.mirrored_agent_cwd_by_pane.write().remove(&pane_id);
        self.detected_agent_panes.write().remove(&pane_id);
        self.agent_adoption_candidates.write().remove(&pane_id);
        self.agent_artifact_watcher.lock().unwatch_pane(pane_id);
        if self.get_agent_metadata_for_pane(pane_id).is_none() {
            self.agent_runtime_by_pane.write().remove(&pane_id);
            self.agent_observer_state_by_pane.write().remove(&pane_id);
        }
    }

    fn failed_agent_restore_blocks_pane(&self, pane_id: PaneId, pane: &Arc<dyn Pane>) -> bool {
        let Some(failure) = self.failed_agent_restores.read().get(&pane_id).copied() else {
            return false;
        };
        if pane.is_dead() {
            self.failed_agent_restores.write().remove(&pane_id);
            return false;
        }
        let Some(process) = pane.get_foreground_process_info(CachePolicy::AllowStale) else {
            return true;
        };
        if failure.foreground_pid == Some(process.pid)
            && failure.process_start_time == Some(process.start_time)
        {
            true
        } else {
            self.failed_agent_restores.write().remove(&pane_id);
            false
        }
    }

    fn stamp_adopted_process_identity(
        metadata: &mut AgentMetadata,
        process_info: Option<&procinfo::LocalProcessInfo>,
    ) {
        metadata.adopted_pid = process_info.map(|process| process.pid);
        metadata.adopted_start_time = process_info.map(|process| process.start_time);
    }

    fn detect_agent_state_for_pane(&self, pane_id: PaneId) -> Option<DetectedAgentState> {
        if self.get_agent_metadata_for_pane(pane_id).is_some() {
            self.detected_agent_panes.write().remove(&pane_id);
            self.agent_adoption_candidates.write().remove(&pane_id);
            return None;
        }

        if let Some(candidate) = self.agent_adoption_candidates.read().get(&pane_id).cloned() {
            let observer_pending = self
                .agent_observer_state_by_pane
                .read()
                .get(&pane_id)
                .map(|state| state.inflight_generation.is_some() || state.pending_request.is_some())
                .unwrap_or(false);
            if observer_pending {
                return self.detected_agent_state_from_candidate(candidate);
            }
        }

        let Some(pane) = self.get_pane(pane_id) else {
            self.pending_agent_restores.write().remove(&pane_id);
            self.clear_detected_agent_info(pane_id);
            return None;
        };
        if pane.is_dead() {
            self.pending_agent_restores.write().remove(&pane_id);
            self.clear_detected_agent_info(pane_id);
            return None;
        }
        if self.failed_agent_restore_blocks_pane(pane_id, &pane) {
            return None;
        }
        let Some((_domain_id, window_id, tab_id)) = self.resolve_pane_id(pane_id) else {
            self.pending_agent_restores.write().remove(&pane_id);
            self.clear_detected_agent_info(pane_id);
            return None;
        };
        let Some(window) = self.get_window(window_id) else {
            self.clear_detected_agent_info(pane_id);
            return None;
        };
        let title = pane.get_title();
        let title_harness = infer_harness(&title, None);
        let foreground_process_name = pane.get_foreground_process_name(CachePolicy::AllowStale);
        let quick_process_harness = infer_harness("", foreground_process_name.as_deref());
        if matches!(title_harness, crate::agent::AgentHarness::Unknown)
            && matches!(quick_process_harness, crate::agent::AgentHarness::Unknown)
        {
            self.clear_detected_agent_info(pane_id);
            return None;
        }

        let foreground_process_info = pane.get_foreground_process_info(CachePolicy::AllowStale);
        let process_match = detect_harness_process(
            foreground_process_info.as_ref(),
            foreground_process_name.as_deref(),
        );
        let process_harness = process_match
            .as_ref()
            .map(|matched| matched.harness.clone())
            .unwrap_or(crate::agent::AgentHarness::Unknown);
        let harness = if !matches!(process_harness, crate::agent::AgentHarness::Unknown) {
            process_harness.clone()
        } else {
            title_harness.clone()
        };
        if matches!(harness, crate::agent::AgentHarness::Unknown) {
            self.clear_detected_agent_info(pane_id);
            return None;
        }

        let Some(declared_cwd) = Self::pane_declared_cwd(&pane, foreground_process_info.as_ref())
        else {
            self.clear_detected_agent_info(pane_id);
            return None;
        };

        let Some(launch_cmd) = process_match
            .as_ref()
            .map(|matched| matched.launch_cmd.clone())
            .or_else(|| default_launch_cmd_for_harness(&harness).map(str::to_string))
        else {
            self.clear_detected_agent_info(pane_id);
            return None;
        };
        let metadata = AgentMetadata {
            agent_id: format!("detected-pane-{pane_id}"),
            name: format!("detected-{pane_id}"),
            launch_cmd,
            declared_cwd,
            adopted_pid: foreground_process_info.as_ref().map(|process| process.pid),
            adopted_start_time: foreground_process_info
                .as_ref()
                .map(|process| process.start_time),
            created_at: Utc::now(),
            repo_root: None,
            worktree: None,
            branch: None,
            managed_checkout: false,
            codex_app_server: None,
        };
        let existing_candidate = self.agent_adoption_candidates.read().get(&pane_id).cloned();
        let same_process_incarnation = existing_candidate
            .as_ref()
            .map(|candidate| {
                candidate.foreground_pid == metadata.adopted_pid
                    && candidate.process_start_time == metadata.adopted_start_time
            })
            .unwrap_or(true);
        let mut runtime = if same_process_incarnation {
            self.agent_runtime_by_pane
                .read()
                .get(&pane_id)
                .cloned()
                .unwrap_or_else(|| AgentRuntimeSnapshot::new(&metadata))
        } else {
            AgentRuntimeSnapshot::new(&metadata)
        };
        runtime.alive = !pane.is_dead();
        runtime.foreground_process_name = foreground_process_name;
        runtime.tty_name = pane.tty_name();
        runtime.terminal_progress = pane.get_progress();
        runtime.harness = harness.clone();

        let mut source = vec![];
        if !matches!(process_harness, crate::agent::AgentHarness::Unknown) {
            source.push("proc");
        }
        if runtime.session_path.is_some() {
            source.push("session");
        }
        if !matches!(title_harness, crate::agent::AgentHarness::Unknown) {
            source.push("title");
        }
        if source.is_empty()
            || (matches!(process_harness, crate::agent::AgentHarness::Unknown)
                && matches!(title_harness, crate::agent::AgentHarness::Unknown))
        {
            self.clear_detected_agent_info(pane_id);
            return None;
        }

        let detection_source = source.join("+");
        finalize_runtime_snapshot(&mut runtime);
        runtime.status = derive_runtime_status(&runtime);
        self.agent_runtime_by_pane
            .write()
            .insert(pane_id, runtime.clone());
        self.detected_agent_panes.write().insert(pane_id);

        let candidate = AgentAdoptionCandidate {
            pane_id,
            harness,
            declared_cwd: metadata.declared_cwd.clone(),
            launch_cmd: metadata.launch_cmd.clone(),
            foreground_pid: metadata.adopted_pid,
            process_start_time: metadata.adopted_start_time,
            created_at: metadata.created_at,
            tab_id,
            window_id,
            workspace: window.get_workspace().to_string(),
            domain_id: pane.domain_id(),
            detection_source: detection_source.clone(),
        };
        self.agent_adoption_candidates
            .write()
            .insert(pane_id, candidate.clone());
        self.agent_artifact_watcher.lock().watch_pane(
            pane_id,
            &candidate.harness,
            &candidate.declared_cwd,
            runtime.session_path.as_deref(),
        );
        let observer_metadata = self.observer_metadata_for_candidate(&candidate);
        self.schedule_agent_observer_refresh(
            pane_id,
            &observer_metadata,
            &runtime,
            AgentRefreshPolicy::Throttled,
            false,
        );

        Some(DetectedAgentState {
            pane_id,
            tab_id,
            window_id,
            workspace: window.get_workspace().to_string(),
            domain_id: pane.domain_id(),
            launch_cmd: metadata.launch_cmd,
            declared_cwd: metadata.declared_cwd,
            adopted_pid: metadata.adopted_pid,
            adopted_start_time: metadata.adopted_start_time,
            runtime,
            detection_source,
        })
    }

    fn detected_agent_state_from_candidate(
        &self,
        candidate: AgentAdoptionCandidate,
    ) -> Option<DetectedAgentState> {
        let runtime = self
            .agent_runtime_by_pane
            .read()
            .get(&candidate.pane_id)
            .cloned()?;
        Some(DetectedAgentState {
            pane_id: candidate.pane_id,
            tab_id: candidate.tab_id,
            window_id: candidate.window_id,
            workspace: candidate.workspace,
            domain_id: candidate.domain_id,
            launch_cmd: candidate.launch_cmd,
            declared_cwd: candidate.declared_cwd,
            adopted_pid: candidate.foreground_pid,
            adopted_start_time: candidate.process_start_time,
            runtime,
            detection_source: candidate.detection_source,
        })
    }

    fn metadata_from_adoption_candidate(candidate: &AgentAdoptionCandidate) -> AgentMetadata {
        AgentMetadata {
            agent_id: format!("detected-pane-{}", candidate.pane_id),
            name: format!("detected-{}", candidate.pane_id),
            launch_cmd: candidate.launch_cmd.clone(),
            declared_cwd: candidate.declared_cwd.clone(),
            adopted_pid: candidate.foreground_pid,
            adopted_start_time: candidate.process_start_time,
            created_at: candidate.created_at,
            repo_root: None,
            worktree: None,
            branch: None,
            managed_checkout: false,
            codex_app_server: None,
        }
    }

    fn observer_metadata_for_candidate(&self, candidate: &AgentAdoptionCandidate) -> AgentMetadata {
        if let Some(pending) = self
            .pending_agent_restores
            .read()
            .get(&candidate.pane_id)
            .cloned()
        {
            let mut metadata = pending.metadata;
            metadata.adopted_pid = candidate.foreground_pid;
            metadata.adopted_start_time = candidate.process_start_time;
            return metadata;
        }

        Self::metadata_from_adoption_candidate(candidate)
    }

    fn refresh_detected_agent_runtime_for_pane(&self, pane_id: PaneId) {
        let Some(candidate) = self.agent_adoption_candidates.read().get(&pane_id).cloned() else {
            return;
        };
        let Some(runtime) = self.agent_runtime_by_pane.read().get(&pane_id).cloned() else {
            return;
        };
        let metadata = self.observer_metadata_for_candidate(&candidate);
        self.schedule_agent_observer_refresh(
            pane_id,
            &metadata,
            &runtime,
            AgentRefreshPolicy::Immediate,
            false,
        );
    }

    fn fail_pending_agent_restore(&self, pane_id: PaneId, candidate: &AgentAdoptionCandidate) {
        let removed = self
            .pending_agent_restores
            .write()
            .remove(&pane_id)
            .is_some();
        self.failed_agent_restores.write().insert(
            pane_id,
            FailedAgentRestore {
                foreground_pid: candidate.foreground_pid,
                process_start_time: candidate.process_start_time,
            },
        );
        self.clear_detected_agent_info(pane_id);
        if removed {
            crate::session_persistence::request_session_save();
        }
    }

    fn complete_pending_agent_restore(
        &self,
        pane_id: PaneId,
        candidate: AgentAdoptionCandidate,
        runtime: AgentRuntimeSnapshot,
    ) -> AgentRestoreOutcome {
        let Some(pending) = self.pending_agent_restores.read().get(&pane_id).cloned() else {
            return AgentRestoreOutcome::Pending;
        };

        if !self.candidate_matches_current_process(&candidate) {
            log::warn!(
                "{:?} restore pane {} changed process incarnation before confirmation",
                pending.harness,
                pane_id,
            );
            self.fail_pending_agent_restore(pane_id, &candidate);
            return AgentRestoreOutcome::Failed;
        }

        if candidate.harness != pending.harness || runtime.harness != pending.harness {
            log::warn!(
                "{:?} restore pane {} started {:?} instead",
                pending.harness,
                pane_id,
                candidate.harness,
            );
            self.fail_pending_agent_restore(pane_id, &candidate);
            return AgentRestoreOutcome::Failed;
        }

        let Some(session_path) = runtime.session_path.as_deref() else {
            return AgentRestoreOutcome::Pending;
        };
        let actual_session_id =
            match restorable_session_id(&pending.harness, Path::new(session_path)) {
                Ok(Some(session_id)) => session_id,
                Ok(None) => {
                    log::warn!(
                        "{:?} restore pane {} session {} has no provider session ID",
                        pending.harness,
                        pane_id,
                        session_path
                    );
                    self.fail_pending_agent_restore(pane_id, &candidate);
                    return AgentRestoreOutcome::Failed;
                }
                Err(err) => {
                    log::warn!(
                        "{:?} restore pane {} could not read provider session {}: {err:#}",
                        pending.harness,
                        pane_id,
                        session_path
                    );
                    self.fail_pending_agent_restore(pane_id, &candidate);
                    return AgentRestoreOutcome::Failed;
                }
            };

        if actual_session_id != pending.session_id {
            log::warn!(
                "{:?} restore pane {} opened session {}, expected {}",
                pending.harness,
                pane_id,
                actual_session_id,
                pending.session_id
            );
            self.fail_pending_agent_restore(pane_id, &candidate);
            return AgentRestoreOutcome::Failed;
        }

        let mut metadata = pending.metadata;
        metadata.adopted_pid = candidate.foreground_pid;
        metadata.adopted_start_time = candidate.process_start_time;
        if let Err(err) =
            self.install_agent_metadata_runtime_without_process_identity(pane_id, metadata, runtime)
        {
            log::warn!(
                "{:?} restore pane {} could not bind metadata: {err:#}",
                pending.harness,
                pane_id
            );
            self.fail_pending_agent_restore(pane_id, &candidate);
            return AgentRestoreOutcome::Failed;
        }

        self.refresh_agent_runtime_for_pane_with_update(
            pane_id,
            false,
            AgentRefreshPolicy::Throttled,
            |_| {},
        );
        if let Some((_domain_id, _window_id, tab_id)) = self.resolve_pane_id(pane_id) {
            self.notify_tab_title_changed(tab_id);
        }
        AgentRestoreOutcome::Completed
    }

    #[cfg(test)]
    fn handle_agent_artifact_event(&self, paths: Vec<PathBuf>) {
        self.handle_agent_artifact_batch(paths, false);
    }

    fn handle_agent_artifact_batch(&self, paths: Vec<PathBuf>, refresh_all: bool) {
        let pane_ids = {
            let mut watcher = self.agent_artifact_watcher.lock();
            if refresh_all {
                watcher.all_watched_panes()
            } else {
                watcher.matching_panes(&paths)
            }
        };
        for pane_id in pane_ids {
            if self.get_agent_metadata_for_pane(pane_id).is_some() {
                self.refresh_agent_runtime_for_pane_with_update(
                    pane_id,
                    false,
                    AgentRefreshPolicy::Throttled,
                    |_| {},
                );
            } else {
                self.refresh_detected_agent_runtime_for_pane(pane_id);
            }
        }
    }

    fn record_detected_agent_output(&self, pane_id: PaneId) {
        let Some(candidate) = self.agent_adoption_candidates.read().get(&pane_id).cloned() else {
            return;
        };
        let Some(mut runtime) = self.agent_runtime_by_pane.read().get(&pane_id).cloned() else {
            return;
        };
        let now = Utc::now();
        runtime.last_output_at = Some(now);
        runtime.observed_at = now;
        finalize_runtime_snapshot(&mut runtime);
        runtime.status = derive_runtime_status(&runtime);
        self.agent_runtime_by_pane
            .write()
            .insert(pane_id, runtime.clone());

        let metadata = self.observer_metadata_for_candidate(&candidate);
        self.schedule_agent_observer_refresh(
            pane_id,
            &metadata,
            &runtime,
            AgentRefreshPolicy::Throttled,
            false,
        );
    }

    fn detected_agent_snapshot_from_state(
        &self,
        state: DetectedAgentState,
        name: String,
    ) -> AgentSnapshot {
        let created_at = Self::detected_agent_created_at(&state.runtime);
        let needs_attention = self.agent_turn_needs_attention(state.pane_id, &state.runtime);
        AgentSnapshot {
            metadata: AgentMetadata {
                agent_id: format!("detected-pane-{}", state.pane_id),
                name,
                launch_cmd: state.launch_cmd,
                declared_cwd: state.declared_cwd,
                adopted_pid: state.adopted_pid,
                adopted_start_time: state.adopted_start_time,
                created_at,
                repo_root: None,
                worktree: None,
                branch: None,
                managed_checkout: false,
                codex_app_server: None,
            },
            runtime: state.runtime,
            pane_id: state.pane_id,
            tab_id: state.tab_id,
            window_id: state.window_id,
            workspace: state.workspace,
            domain_id: state.domain_id,
            origin: AgentOrigin::Detected,
            detection_source: Some(state.detection_source),
            needs_attention,
        }
    }

    fn maybe_auto_adopt_detected_agent(&self, pane_id: PaneId) {
        if !Self::agent_auto_adopt_on_confirmed_session_match()
            || self.get_agent_metadata_for_pane(pane_id).is_some()
        {
            return;
        }

        let Some(candidate) = self.agent_adoption_candidates.read().get(&pane_id).cloned() else {
            return;
        };
        if self
            .agent_observer_state_by_pane
            .read()
            .get(&pane_id)
            .is_some_and(|state| {
                state.inflight_generation.is_some() || state.pending_request.is_some()
            })
        {
            return;
        }
        let Some(runtime) = self.agent_runtime_by_pane.read().get(&pane_id).cloned() else {
            return;
        };
        if !self.candidate_matches_current_process(&candidate) {
            self.clear_detected_agent_info(pane_id);
            return;
        }
        let _ = self.auto_adopt_candidate(candidate, runtime);
    }

    fn auto_adopt_state(&self, state: &DetectedAgentState) -> Option<Arc<AgentMetadata>> {
        let candidate = self
            .agent_adoption_candidates
            .read()
            .get(&state.pane_id)
            .cloned()?;
        if candidate.foreground_pid != state.adopted_pid
            || candidate.process_start_time != state.adopted_start_time
        {
            return None;
        }

        self.auto_adopt_candidate(candidate, state.runtime.clone())
    }

    fn auto_adopt_candidate(
        &self,
        candidate: AgentAdoptionCandidate,
        runtime: AgentRuntimeSnapshot,
    ) -> Option<Arc<AgentMetadata>> {
        if runtime.harness != candidate.harness
            || runtime.session_path.is_none()
            || candidate.foreground_pid.is_none()
            || candidate.process_start_time.is_none()
        {
            return None;
        }

        let tab_id = self
            .resolve_pane_id(candidate.pane_id)
            .map(|(_, _, tab_id)| tab_id);
        let taken_names = self
            .agent_panes_by_name
            .read()
            .keys()
            .cloned()
            .collect::<HashSet<_>>();
        let base_name = Self::detected_agent_name_base(&candidate.harness, &candidate.declared_cwd);
        let name = Self::next_available_agent_name(&taken_names, &base_name);
        let created_at = candidate.created_at;
        let pane_id = candidate.pane_id;
        let metadata = AgentMetadata {
            agent_id: format!("detected-pane-{pane_id}"),
            name,
            launch_cmd: candidate.launch_cmd,
            declared_cwd: candidate.declared_cwd,
            adopted_pid: candidate.foreground_pid,
            adopted_start_time: candidate.process_start_time,
            created_at,
            repo_root: None,
            worktree: None,
            branch: None,
            managed_checkout: false,
            codex_app_server: None,
        };
        if self
            .install_agent_metadata_runtime_without_process_identity(pane_id, metadata, runtime)
            .is_ok()
        {
            self.refresh_agent_runtime_for_pane_with_update(
                pane_id,
                false,
                AgentRefreshPolicy::Throttled,
                |_| {},
            );
            if let Some(tab_id) = tab_id {
                self.notify_tab_title_changed(tab_id);
            }
            return self.get_agent_metadata_for_pane(pane_id);
        }
        None
    }

    fn candidate_matches_current_process(&self, candidate: &AgentAdoptionCandidate) -> bool {
        if candidate.foreground_pid.is_none() || candidate.process_start_time.is_none() {
            return false;
        }
        let Some(pane) = self.get_pane(candidate.pane_id) else {
            return false;
        };
        if pane.is_dead() {
            return false;
        }
        let Some(process) = pane.get_foreground_process_info(CachePolicy::AllowStale) else {
            return false;
        };
        candidate.foreground_pid == Some(process.pid)
            && candidate.process_start_time == Some(process.start_time)
    }

    fn should_refresh_harness_runtime(
        observer_state: &AgentObserverState,
        policy: AgentRefreshPolicy,
        now: DateTime<Utc>,
    ) -> bool {
        match policy {
            AgentRefreshPolicy::Immediate => true,
            AgentRefreshPolicy::Throttled => observer_state
                .last_requested_at
                .map(|last| {
                    (now - last)
                        .to_std()
                        .map(|elapsed| elapsed >= AGENT_HARNESS_REFRESH_THROTTLE)
                        .unwrap_or(true)
                })
                .unwrap_or(true),
        }
    }

    fn dispatch_agent_observer_request(&self, request: AgentObserverRequest) {
        if self
            .agent_observer_tx
            .send(AgentObserverCommand::Refresh(request))
            .is_err()
        {
            log::error!("agent observer worker is no longer available");
        }
    }

    fn schedule_agent_observer_refresh(
        &self,
        pane_id: PaneId,
        metadata: &AgentMetadata,
        runtime: &AgentRuntimeSnapshot,
        refresh_policy: AgentRefreshPolicy,
        schedule_trailing_refresh: bool,
    ) {
        let now = Utc::now();
        let adopted = !self.detected_agent_panes.read().contains(&pane_id);
        let requires_lossless_observation = adopted
            && metadata.adopted_pid.is_some()
            && metadata.adopted_start_time.is_some()
            && runtime.session_path.is_some();
        let prior_generation = self
            .agent_observer_generation_by_pane
            .read()
            .get(&pane_id)
            .copied()
            .unwrap_or_default();
        let mut schedule_rate_limited_trailing_refresh = false;
        let request = {
            let mut observer_state_by_pane = self.agent_observer_state_by_pane.write();
            let observer_state = observer_state_by_pane.entry(pane_id).or_default();
            observer_state.latest_generation =
                observer_state.latest_generation.max(prior_generation);
            if !Self::should_refresh_harness_runtime(observer_state, refresh_policy, now) {
                counter!("mux.agent_observer.refresh.skipped.rate").increment(1);
                schedule_rate_limited_trailing_refresh =
                    schedule_trailing_refresh && requires_lossless_observation;
                None
            } else {
                observer_state.latest_generation += 1;
                self.agent_observer_generation_by_pane
                    .write()
                    .insert(pane_id, observer_state.latest_generation);
                observer_state.last_requested_at = Some(now);
                let request = AgentObserverRequest {
                    pane_id,
                    generation: observer_state.latest_generation,
                    requested_at: Instant::now(),
                    metadata: metadata.clone(),
                    runtime: runtime.clone(),
                    expected_session: self.pending_agent_restores.read().get(&pane_id).map(
                        |pending| ExpectedAgentSession {
                            harness: pending.harness.clone(),
                            session_id: pending.session_id.clone(),
                        },
                    ),
                    adopted,
                    schedule_trailing_refresh: schedule_trailing_refresh
                        && requires_lossless_observation,
                };

                if observer_state.inflight_generation.is_some() {
                    if observer_state.pending_request.replace(request).is_some() {
                        counter!("mux.agent_observer.refresh.replaced_pending.rate").increment(1);
                    } else {
                        counter!("mux.agent_observer.refresh.coalesced.rate").increment(1);
                    }
                    None
                } else {
                    observer_state.inflight_generation = Some(request.generation);
                    Some(request)
                }
            }
        };

        if schedule_rate_limited_trailing_refresh {
            self.schedule_trailing_agent_observer_refresh(pane_id);
        }

        let Some(request) = request else {
            return;
        };

        counter!("mux.agent_observer.refresh.scheduled.rate").increment(1);
        self.dispatch_agent_observer_request(request);
    }

    fn apply_agent_observer_update(&self, update: AgentObserverUpdate) {
        let schedule_trailing_refresh = update.schedule_trailing_refresh;
        let next_request = {
            let mut observer_state_by_pane = self.agent_observer_state_by_pane.write();
            let Some(observer_state) = observer_state_by_pane.get_mut(&update.pane_id) else {
                counter!("mux.agent_observer.refresh.dropped_no_state.rate").increment(1);
                return;
            };

            if observer_state.inflight_generation == Some(update.generation) {
                observer_state.inflight_generation = None;
            }

            let is_stale = update.generation < observer_state.latest_generation;
            let next_request = observer_state.pending_request.take().map(|request| {
                observer_state.inflight_generation = Some(request.generation);
                request
            });

            if is_stale {
                counter!("mux.agent_observer.refresh.stale.rate").increment(1);
            }

            (is_stale, next_request)
        };

        if let Some(request) = next_request.1 {
            self.dispatch_agent_observer_request(request);
        }

        if next_request.0 {
            return;
        }

        let Some((_domain_id, _window_id, tab_id)) = self.resolve_pane_id(update.pane_id) else {
            counter!("mux.agent_observer.refresh.dropped_missing_pane.rate").increment(1);
            return;
        };
        if self.get_agent_metadata_for_pane(update.pane_id).is_none()
            && !self.detected_agent_panes.read().contains(&update.pane_id)
        {
            counter!("mux.agent_observer.refresh.dropped_missing_target.rate").increment(1);
            return;
        }

        let (before_title, after_title, session_identity_changed) = {
            let mut runtime_by_pane = self.agent_runtime_by_pane.write();
            let Some(runtime) = runtime_by_pane.get_mut(&update.pane_id) else {
                counter!("mux.agent_observer.refresh.dropped_missing_runtime.rate").increment(1);
                return;
            };

            let before_title = Self::title_fingerprint(runtime);
            let before_session_path = runtime.session_path.clone();
            runtime.harness = update.runtime.harness;
            runtime.transport = update.runtime.transport;
            runtime.observed_at = update.runtime.observed_at;
            runtime.session_path = update.runtime.session_path;
            runtime.progress_summary = update.runtime.progress_summary;
            runtime.harness_mode = update.runtime.harness_mode;
            runtime.turn_phase = update.runtime.turn_phase;
            runtime.turn_state = update.runtime.turn_state;
            runtime.last_turn_completed_at = update.runtime.last_turn_completed_at;
            runtime.observed_turn = update.runtime.observed_turn;
            runtime.observer_error = update.runtime.observer_error;
            runtime.observer_started_at = update.runtime.observer_started_at;
            runtime.last_harness_refresh_at = update.runtime.last_harness_refresh_at;
            finalize_runtime_snapshot(runtime);
            runtime.status = derive_runtime_status(runtime);
            (
                before_title,
                Self::title_fingerprint(runtime),
                before_session_path != runtime.session_path,
            )
        };

        histogram!("mux.agent_observer.refresh.apply.queue_delay").record(update.queue_delay);
        histogram!("mux.agent_observer.refresh.apply.latency").record(update.refresh_elapsed);
        counter!("mux.agent_observer.refresh.applied.rate").increment(1);

        if schedule_trailing_refresh {
            self.schedule_trailing_agent_observer_refresh(update.pane_id);
        }

        let mut adopted = false;
        if self.get_agent_metadata_for_pane(update.pane_id).is_none()
            && self.detected_agent_panes.read().contains(&update.pane_id)
        {
            let pending_restore = self
                .pending_agent_restores
                .read()
                .contains_key(&update.pane_id);
            let candidate = self
                .agent_adoption_candidates
                .read()
                .get(&update.pane_id)
                .cloned();
            let runtime = self
                .agent_runtime_by_pane
                .read()
                .get(&update.pane_id)
                .cloned();
            match (candidate, runtime) {
                (Some(candidate), Some(runtime)) => {
                    if pending_restore {
                        if matches!(
                            self.complete_pending_agent_restore(
                                update.pane_id,
                                candidate,
                                runtime,
                            ),
                            AgentRestoreOutcome::Completed
                        ) {
                            adopted = true;
                        }
                    } else if !self.candidate_matches_current_process(&candidate) {
                        self.clear_detected_agent_info(update.pane_id);
                    } else if Self::agent_auto_adopt_on_confirmed_session_match()
                        && runtime.harness == candidate.harness
                        && runtime.session_path.is_some()
                    {
                        adopted = self.auto_adopt_candidate(candidate, runtime).is_some();
                    }
                }
                _ => {}
            }
        }

        if !adopted && before_title != after_title {
            self.notify_tab_title_changed(tab_id);
        }
        if session_identity_changed {
            if let (Some(metadata), Some(runtime)) = (
                self.get_agent_metadata_for_pane(update.pane_id),
                self.agent_runtime_by_pane
                    .read()
                    .get(&update.pane_id)
                    .cloned(),
            ) {
                self.agent_artifact_watcher.lock().watch_pane(
                    update.pane_id,
                    &runtime.harness,
                    &metadata.declared_cwd,
                    runtime.session_path.as_deref(),
                );
                crate::session_persistence::request_session_save();
            } else if let Some(runtime) = self
                .agent_runtime_by_pane
                .read()
                .get(&update.pane_id)
                .cloned()
            {
                self.agent_artifact_watcher.lock().set_confirmed_artifact(
                    update.pane_id,
                    &runtime.harness,
                    runtime.session_path.as_deref(),
                );
            }
        }
    }

    fn schedule_trailing_agent_observer_refresh(&self, pane_id: PaneId) {
        let should_schedule = {
            let mut states = self.agent_observer_state_by_pane.write();
            let Some(state) = states.get_mut(&pane_id) else {
                return;
            };
            if state.trailing_refresh_scheduled {
                false
            } else {
                state.trailing_refresh_scheduled = true;
                true
            }
        };
        if !should_schedule {
            return;
        }
        if self.agent_observer_timer_tx.send(pane_id).is_err() {
            if let Some(state) = self.agent_observer_state_by_pane.write().get_mut(&pane_id) {
                state.trailing_refresh_scheduled = false;
            }
        }
    }

    fn title_fingerprint(runtime: &AgentRuntimeSnapshot) -> AgentTitleFingerprint {
        AgentTitleFingerprint {
            harness: runtime.harness.clone(),
            transport: runtime.transport.clone(),
            has_session_path: runtime.session_path.is_some(),
            turn_state: runtime.turn_state.clone(),
            last_turn_completed_at: runtime.last_turn_completed_at,
            attention_reason: runtime.attention_reason.clone(),
        }
    }

    /// Record a prompt that Wakterm atomically submitted to the provider.
    ///
    /// Raw PTY input is not sufficient evidence: Enter can launch a provider,
    /// choose a resume target, or navigate the TUI without starting a turn.
    pub fn record_agent_prompt_submission(&self, pane_id: PaneId) {
        self.record_agent_input_generation(pane_id);
        self.refresh_agent_runtime_for_pane_with_update(
            pane_id,
            true,
            AgentRefreshPolicy::Throttled,
            |runtime| {
                let now = chrono::Utc::now();
                runtime.last_input_at = Some(now);
                runtime.observed_at = now;
            },
        );
    }

    pub(crate) fn record_agent_input_generation(&self, pane_id: PaneId) {
        if self.get_agent_metadata_for_pane(pane_id).is_none() {
            return;
        }
        *self
            .agent_input_generation_by_pane
            .write()
            .entry(pane_id)
            .or_default() += 1;
    }

    pub(crate) fn agent_input_generation(&self, pane_id: PaneId) -> u64 {
        self.agent_input_generation_by_pane
            .read()
            .get(&pane_id)
            .copied()
            .unwrap_or(0)
    }

    fn reconcile_agent_requests(&self) -> anyhow::Result<()> {
        let now = Utc::now();
        for mut request in self.agent_request_store.active()? {
            let before = request.clone();
            let metadata = self.get_agent_metadata_for_pane(request.target_pane_id);
            let runtime = self
                .agent_runtime_by_pane
                .read()
                .get(&request.target_pane_id)
                .cloned();
            request.reconcile(metadata.as_deref(), runtime.as_ref(), now);
            if request != before {
                self.agent_request_store.save(&mut request)?;
            }
        }
        Ok(())
    }

    pub fn list_agent_request_events(
        &self,
        after_sequence: u64,
        limit: usize,
    ) -> anyhow::Result<Vec<AgentRequest>> {
        self.reconcile_agent_requests()?;
        self.agent_request_store.events_after(after_sequence, limit)
    }

    pub fn get_agent_request(&self, request_id: &str) -> anyhow::Result<Option<AgentRequest>> {
        self.reconcile_agent_requests()?;
        self.agent_request_store.get(request_id)
    }

    pub fn cancel_agent_request(&self, request_id: &str) -> anyhow::Result<AgentRequest> {
        self.reconcile_agent_requests()?;
        let mut request = self
            .agent_request_store
            .get(request_id)?
            .ok_or_else(|| anyhow!("no agent request with id {request_id}"))?;
        if !request.state.is_terminal() {
            request.finish(
                AgentRequestState::Cancelled,
                Utc::now(),
                "request was cancelled",
            );
            self.agent_request_store.save(&mut request)?;
        }
        Ok(request)
    }

    pub fn record_agent_output(&self, pane_id: PaneId) {
        if let (Some(metadata), Some(pane)) = (
            self.get_agent_metadata_for_pane(pane_id),
            self.get_pane(pane_id),
        ) {
            let process = pane.get_foreground_process_info(CachePolicy::AllowStale);
            if !adopted_agent_matches_process_info(metadata.as_ref(), process.as_ref()) {
                self.clear_agent_metadata(pane_id);
            }
        }

        if self.get_agent_metadata_for_pane(pane_id).is_none() {
            if self
                .mirrored_agent_harness_by_pane
                .read()
                .contains_key(&pane_id)
            {
                return;
            }
            let before_detected = self.detected_agent_panes.read().contains(&pane_id);
            let detected = self.detect_agent_state_for_pane(pane_id);
            if let Some(state) = detected {
                if !before_detected {
                    self.notify_tab_title_changed(state.tab_id);
                } else {
                    self.record_detected_agent_output(pane_id);
                }
            }
            return;
        }
        self.refresh_agent_runtime_for_pane_with_update(
            pane_id,
            true,
            AgentRefreshPolicy::Throttled,
            |runtime| {
                let now = chrono::Utc::now();
                runtime.last_output_at = Some(now);
                runtime.observed_at = now;
            },
        );
    }

    pub fn record_agent_terminal_progress(
        &self,
        pane_id: PaneId,
        progress: wakterm_term::Progress,
    ) {
        self.refresh_agent_runtime_for_pane_with_update(
            pane_id,
            true,
            AgentRefreshPolicy::Throttled,
            |runtime| {
                let now = chrono::Utc::now();
                runtime.terminal_progress = progress;
                runtime.last_progress_at = Some(now);
                runtime.observed_at = now;
            },
        );
    }

    fn refresh_agent_runtime_for_pane_with_update<F>(
        &self,
        pane_id: PaneId,
        notify_title: bool,
        refresh_policy: AgentRefreshPolicy,
        update: F,
    ) where
        F: FnOnce(&mut AgentRuntimeSnapshot),
    {
        self.refresh_agent_runtime_for_pane_with_update_inner(
            pane_id,
            notify_title,
            refresh_policy,
            true,
            update,
        );
    }

    fn refresh_agent_runtime_for_pane_with_update_inner<F>(
        &self,
        pane_id: PaneId,
        notify_title: bool,
        refresh_policy: AgentRefreshPolicy,
        schedule_trailing_refresh: bool,
        update: F,
    ) where
        F: FnOnce(&mut AgentRuntimeSnapshot),
    {
        let Some(metadata) = self.get_agent_metadata_for_pane(pane_id) else {
            return;
        };
        let Some(pane) = self.get_pane(pane_id) else {
            return;
        };
        let Some((_, _, tab_id)) = self.resolve_pane_id(pane_id) else {
            return;
        };
        let mut runtime = self
            .agent_runtime_by_pane
            .read()
            .get(&pane_id)
            .cloned()
            .unwrap_or_else(|| AgentRuntimeSnapshot::new(metadata.as_ref()));
        let before_title = notify_title.then(|| Self::title_fingerprint(&runtime));
        update(&mut runtime);
        runtime.alive = !pane.is_dead();
        runtime.foreground_process_name = pane.get_foreground_process_name(CachePolicy::AllowStale);
        runtime.tty_name = pane.tty_name();
        runtime.terminal_progress = pane.get_progress();
        runtime.harness = infer_harness(
            &metadata.launch_cmd,
            runtime.foreground_process_name.as_deref(),
        );
        self.schedule_agent_observer_refresh(
            pane_id,
            metadata.as_ref(),
            &runtime,
            refresh_policy,
            schedule_trailing_refresh,
        );
        finalize_runtime_snapshot(&mut runtime);
        runtime.status = derive_runtime_status(&runtime);
        let after_title = notify_title.then(|| Self::title_fingerprint(&runtime));
        self.agent_runtime_by_pane.write().insert(pane_id, runtime);

        if notify_title && before_title != after_title {
            self.notify_tab_title_changed(tab_id);
        }
    }

    pub fn refresh_agent_runtime_for_tab(&self, tab_id: TabId) {
        let Some(tab) = self.get_tab(tab_id) else {
            return;
        };
        let pane_ids = tab
            .iter_panes_ignoring_zoom()
            .into_iter()
            .map(|p| p.pane.pane_id())
            .collect::<Vec<_>>();
        for pane_id in pane_ids {
            if self.get_agent_metadata_for_pane(pane_id).is_some() {
                self.refresh_agent_runtime_for_pane_with_update(
                    pane_id,
                    false,
                    AgentRefreshPolicy::Throttled,
                    |_| {},
                );
            } else {
                let _ = self.detect_agent_state_for_pane(pane_id);
                self.maybe_auto_adopt_detected_agent(pane_id);
            }
        }
    }

    fn notify_tab_title_changed(&self, tab_id: TabId) {
        self.notify(MuxNotification::TabTitleChanged {
            tab_id,
            title: self.raw_tab_title(tab_id),
        });
    }

    pub fn agent_attention_seen_at(&self, pane_id: PaneId) -> Option<DateTime<Utc>> {
        self.agent_attention_seen_at.read().get(&pane_id).copied()
    }

    pub fn restore_agent_attention_seen_at(&self, pane_id: PaneId, seen_at: DateTime<Utc>) {
        self.agent_attention_seen_at
            .write()
            .insert(pane_id, seen_at);
    }

    pub fn acknowledge_agent_attention(&self, pane_id: PaneId) {
        let Some((_domain_id, _window_id, tab_id)) = self.resolve_pane_id(pane_id) else {
            return;
        };
        let completed_at = self
            .agent_runtime_by_pane
            .read()
            .get(&pane_id)
            .and_then(|runtime| runtime.last_turn_completed_at);
        let completed_at = completed_at.or_else(|| {
            self.detect_agent_state_for_pane(pane_id)
                .and_then(|state| state.runtime.last_turn_completed_at)
        });
        let Some(completed_at) = completed_at else {
            return;
        };

        let mut seen = self.agent_attention_seen_at.write();
        if seen
            .get(&pane_id)
            .is_some_and(|seen_at| *seen_at >= completed_at)
        {
            return;
        }
        seen.insert(pane_id, completed_at);
        drop(seen);
        crate::session_persistence::request_session_save();
        self.notify_tab_title_changed(tab_id);
    }

    fn agent_turn_needs_attention(&self, pane_id: PaneId, runtime: &AgentRuntimeSnapshot) -> bool {
        if !matches!(
            runtime.turn_state,
            crate::agent::AgentTurnState::WaitingOnUser
        ) {
            return false;
        }

        let Some(completed_at) = runtime.last_turn_completed_at else {
            return false;
        };

        self.agent_attention_seen_at(pane_id)
            .map(|seen_at| seen_at < completed_at)
            .unwrap_or(true)
    }

    fn agent_waiting_on_user(runtime: &AgentRuntimeSnapshot) -> bool {
        matches!(
            runtime.turn_state,
            crate::agent::AgentTurnState::WaitingOnUser
        )
    }

    fn agent_tab_badge_mode() -> AgentTabBadgeMode {
        match configuration().agent_tab_badge_mode.as_str() {
            "off" => AgentTabBadgeMode::Off,
            "turn" => AgentTabBadgeMode::Turn,
            "attention" => AgentTabBadgeMode::Attention,
            "identity" => AgentTabBadgeMode::Identity,
            _ => AgentTabBadgeMode::Identity,
        }
    }

    fn agent_tab_badge_text() -> Option<String> {
        let badge = configuration().agent_tab_badge.clone();
        if badge.is_empty() {
            None
        } else {
            Some(badge)
        }
    }

    pub fn sanitize_tab_title_text(title: &str) -> String {
        let mut stripped = title;
        loop {
            let mut changed = false;
            for badge in IntoIterator::into_iter([
                Some(configuration().agent_tab_badge.clone()),
                Some("🤖 ".to_string()),
            ])
            .flatten()
            .filter(|badge| !badge.is_empty())
            {
                if let Some(rest) = stripped.strip_prefix(badge.as_str()) {
                    stripped = rest;
                    changed = true;
                    break;
                }
            }
            if !changed {
                break;
            }
        }
        stripped.to_string()
    }

    pub fn raw_tab_title(&self, tab_id: TabId) -> String {
        self.get_tab(tab_id)
            .and_then(|tab| tab.get_explicit_title())
            .map(|title| Self::sanitize_tab_title_text(&title))
            .unwrap_or_default()
    }

    fn cwd_leaf_for_tab_title(cwd: &str) -> Option<String> {
        let path = if cwd.starts_with("file://") {
            Url::parse(cwd)
                .ok()
                .and_then(|url| url.to_file_path().ok())
                .unwrap_or_else(|| PathBuf::from(cwd))
        } else {
            PathBuf::from(cwd)
        };
        path.file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string)
            .filter(|name| !name.is_empty())
    }

    pub fn agent_folder_title_for_pane(&self, pane_id: PaneId) -> Option<String> {
        let cwd = if let Some(metadata) = self.get_agent_metadata_for_pane(pane_id) {
            metadata.declared_cwd.clone()
        } else if let Some(cwd) = self.mirrored_agent_cwd_by_pane.read().get(&pane_id) {
            cwd.clone()
        } else if let Some(snapshot) = self.mirrored_agent_snapshot_by_pane.read().get(&pane_id) {
            snapshot.metadata.declared_cwd.clone()
        } else if let Some(candidate) = self.agent_adoption_candidates.read().get(&pane_id) {
            candidate.declared_cwd.clone()
        } else {
            return None;
        };
        Self::cwd_leaf_for_tab_title(&cwd)
    }

    fn aggregate_agent_folder_title_for_tab(&self, tab: &Tab) -> Option<String> {
        let mut seen = HashSet::new();
        let mut titles = Vec::new();
        for positioned in tab.iter_panes_ignoring_zoom() {
            let Some(title) = self.agent_folder_title_for_pane(positioned.pane.pane_id()) else {
                continue;
            };
            if seen.insert(title.clone()) {
                titles.push(title);
            }
        }

        match titles.len() {
            0 => None,
            1 => titles.pop(),
            2 => Some(titles.join("+")),
            count => Some(format!("{}+{}", titles[0], count - 1)),
        }
    }

    pub fn effective_tab_titles_for_window(&self, window_id: WindowId) -> HashMap<TabId, String> {
        let Some(window) = self.get_window(window_id) else {
            return HashMap::new();
        };
        let view_id = self.active_view_id();
        let mut rows = Vec::with_capacity(window.len());
        for tab in window.iter() {
            let explicit = self.raw_tab_title(tab.tab_id());
            let pane = view_id
                .as_ref()
                .and_then(|view_id| {
                    self.get_active_pane_for_tab_for_client(
                        view_id.as_ref(),
                        window_id,
                        tab.tab_id(),
                    )
                })
                .or_else(|| tab.get_active_pane());
            let automatic = self
                .aggregate_agent_folder_title_for_tab(tab)
                .or_else(|| pane.as_ref().map(|pane| pane.get_title()));
            rows.push((tab.tab_id(), explicit, automatic.unwrap_or_default()));
        }
        drop(window);

        let mut used = rows
            .iter()
            .filter_map(|(_, explicit, _)| (!explicit.is_empty()).then_some(explicit.clone()))
            .collect::<HashSet<_>>();
        let mut result = HashMap::with_capacity(rows.len());
        for (tab_id, explicit, automatic) in rows {
            let title = if !explicit.is_empty() {
                explicit
            } else if automatic.is_empty() || !used.contains(&automatic) {
                used.insert(automatic.clone());
                automatic
            } else {
                let mut ordinal = 2usize;
                loop {
                    let candidate = format!("{automatic}{ordinal}");
                    if used.insert(candidate.clone()) {
                        break candidate;
                    }
                    ordinal += 1;
                }
            };
            result.insert(tab_id, title);
        }
        result
    }

    pub fn display_tab_titles_for_window(&self, window_id: WindowId) -> HashMap<TabId, String> {
        let view_id = self.active_view_id();
        let mut titles = self.effective_tab_titles_for_window(window_id);
        for (tab_id, title) in &mut titles {
            if self.should_badge_tab_for_agents(*tab_id, view_id.as_deref())
                && !self.tab_has_known_harness(*tab_id)
            {
                if let Some(badge) = Self::agent_tab_badge_text() {
                    title.insert_str(0, &badge);
                }
            }
        }
        titles
    }

    fn compute_tab_process_rss(&self, tab_id: TabId) -> Option<u64> {
        fn collect(
            process: &procinfo::LocalProcessInfo,
            seen: &mut HashSet<u32>,
            total: &mut u64,
            measured: &mut bool,
        ) {
            if seen.insert(process.pid) {
                if let Some(bytes) = procinfo::LocalProcessInfo::resident_set_bytes(process.pid) {
                    *total = total.saturating_add(bytes);
                    *measured = true;
                }
            }
            for child in process.children.values() {
                collect(child, seen, total, measured);
            }
        }

        let tab = self.get_tab(tab_id)?;
        let mut seen = HashSet::new();
        let mut total = 0u64;
        let mut measured = false;
        for pane in tab.iter_panes_ignoring_zoom() {
            if let Some(process) = pane
                .pane
                .get_foreground_process_info(CachePolicy::AllowStale)
            {
                collect(&process, &mut seen, &mut total, &mut measured);
            }
        }
        measured
            .then_some(total)
            .or_else(|| self.mirrored_tab_rss_bytes.read().get(&tab_id).copied())
    }

    pub fn tab_resource_status(&self) -> TabResourceStatusSnapshot {
        let mut cache = self.tab_resource_status_cache.lock();
        if cache
            .sampled_at
            .is_some_and(|sampled_at| sampled_at.elapsed() <= TAB_RESOURCE_STATUS_TTL)
        {
            return cache.snapshot.clone();
        }

        let mut tab_rss_bytes = HashMap::new();
        for tab_id in self.tabs.read().keys().copied().collect::<Vec<_>>() {
            if let Some(rss_bytes) = self.compute_tab_process_rss(tab_id) {
                tab_rss_bytes.insert(tab_id, rss_bytes);
            }
        }
        let snapshot = TabResourceStatusSnapshot {
            sampled_at_ms: Utc::now().timestamp_millis().max(0) as u64,
            tab_rss_bytes,
        };
        cache.sampled_at = Some(Instant::now());
        cache.snapshot = snapshot.clone();
        snapshot
    }

    pub fn approximate_tab_process_rss(&self, tab_id: TabId) -> Option<u64> {
        self.tab_resource_status()
            .tab_rss_bytes
            .get(&tab_id)
            .copied()
    }

    fn invalidate_tab_resource_status(&self) {
        self.tab_resource_status_cache.lock().sampled_at = None;
    }

    fn cached_tab_badge_state_for_agents(
        &self,
        tab_id: TabId,
        _view_id: Option<&ClientViewId>,
    ) -> AgentTabBadgeState {
        let Some(tab) = self.get_tab(tab_id) else {
            return AgentTabBadgeState::default();
        };
        let runtime_by_pane = self.agent_runtime_by_pane.read();
        let detected_agent_panes = self.detected_agent_panes.read();
        let mut badge = AgentTabBadgeState::default();
        for positioned in tab.iter_panes_ignoring_zoom() {
            let pane_id = positioned.pane.pane_id();
            let runtime = if self.get_agent_metadata_for_pane(pane_id).is_some()
                || detected_agent_panes.contains(&pane_id)
            {
                runtime_by_pane.get(&pane_id)
            } else {
                None
            };
            if let Some(runtime) = runtime {
                if Self::agent_waiting_on_user(runtime) {
                    badge.waiting_on_user = true;
                }
                let needs_attention = self.agent_turn_needs_attention(pane_id, runtime);
                if needs_attention {
                    badge.needs_attention = true;
                }
                if badge.waiting_on_user && badge.needs_attention {
                    break;
                }
            }
        }
        if let Some(mirrored) = self.mirrored_agent_badge_by_tab.read().get(&tab_id) {
            badge.waiting_on_user |= mirrored.waiting_on_user;
            badge.needs_attention |= mirrored.needs_attention;
        }
        badge
    }

    pub fn tab_badge_state_for_view(
        &self,
        view_id: &ClientViewId,
        tab_id: TabId,
    ) -> AgentTabBadgeState {
        self.cached_tab_badge_state_for_agents(tab_id, Some(view_id))
    }

    pub fn tab_badge_state_for_current_identity(&self, tab_id: TabId) -> AgentTabBadgeState {
        match self.active_view_id() {
            Some(view_id) => self.tab_badge_state_for_view(view_id.as_ref(), tab_id),
            None => self.cached_tab_badge_state_for_agents(tab_id, None),
        }
    }

    pub fn activate_next_agent_needing_attention(
        &self,
        window_id: WindowId,
    ) -> anyhow::Result<Option<(TabId, PaneId)>> {
        let mut agents = self.list_agents_cached();
        agents.extend(self.mirrored_agent_snapshots_for_window(window_id));
        let mut targets = agents
            .into_iter()
            .filter(|agent| agent.window_id == window_id && agent.needs_attention)
            .map(|agent| {
                (
                    agent.runtime.last_turn_completed_at,
                    agent.tab_id,
                    agent.pane_id,
                )
            })
            .collect::<Vec<_>>();
        targets.sort_by(|left, right| right.0.cmp(&left.0));
        let Some((_completed_at, tab_id, pane_id)) = targets.first().copied() else {
            return Ok(None);
        };
        if self
            .get_window(window_id)
            .is_some_and(|window| window.is_tab_parked(tab_id))
        {
            self.set_tab_parked(window_id, tab_id, false)?;
        }
        self.set_focused_pane_for_current_identity_lightweight(pane_id)?;
        Ok(Some((tab_id, pane_id)))
    }

    fn should_badge_tab_for_agents(&self, tab_id: TabId, view_id: Option<&ClientViewId>) -> bool {
        let badge_mode = Self::agent_tab_badge_mode();
        match badge_mode {
            AgentTabBadgeMode::Off => false,
            AgentTabBadgeMode::Identity => self.tab_has_any_agent(tab_id),
            AgentTabBadgeMode::Turn => {
                self.cached_tab_badge_state_for_agents(tab_id, view_id)
                    .waiting_on_user
            }
            AgentTabBadgeMode::Attention => {
                self.cached_tab_badge_state_for_agents(tab_id, view_id)
                    .needs_attention
            }
        }
    }

    fn tab_has_any_agent(&self, tab_id: TabId) -> bool {
        let Some(tab) = self.get_tab(tab_id) else {
            return false;
        };
        for pos in tab.iter_panes_ignoring_zoom() {
            let pane_id = pos.pane.pane_id();
            if self.get_agent_metadata_for_pane(pane_id).is_some()
                || self.detected_agent_panes.read().contains(&pane_id)
                || self
                    .mirrored_agent_harness_by_pane
                    .read()
                    .contains_key(&pane_id)
                || self
                    .mirrored_agent_snapshot_by_pane
                    .read()
                    .contains_key(&pane_id)
            {
                return true;
            }
        }
        false
    }

    fn tab_has_known_harness(&self, tab_id: TabId) -> bool {
        let Some(tab) = self.get_tab(tab_id) else {
            return false;
        };
        for pos in tab.iter_panes_ignoring_zoom() {
            if let Some(harness) = self.cached_agent_harness_for_pane(pos.pane.pane_id()) {
                if !matches!(harness, crate::agent::AgentHarness::Unknown) {
                    return true;
                }
            }
        }
        false
    }

    /// Returns the list of agent harness icons that should be visible for a tab,
    /// filtered by the current `agent_tab_badge_mode`. Deduplicates by harness type.
    pub fn visible_harness_icons_for_tab(
        &self,
        tab_id: TabId,
        view_id: Option<&ClientViewId>,
    ) -> Vec<crate::agent::AgentHarness> {
        use crate::agent::AgentHarness;

        let badge_mode = Self::agent_tab_badge_mode();
        if matches!(badge_mode, AgentTabBadgeMode::Off) {
            return vec![];
        }

        let Some(tab) = self.get_tab(tab_id) else {
            return vec![];
        };

        let runtime_by_pane = self.agent_runtime_by_pane.read();
        let mirrored_badge = self
            .mirrored_agent_badge_by_tab
            .read()
            .get(&tab_id)
            .cloned()
            .unwrap_or_default();
        let mut seen = std::collections::HashSet::new();
        let mut icons = Vec::new();

        for pos in tab.iter_panes_ignoring_zoom() {
            let pane_id = pos.pane.pane_id();

            // cached_agent_harness_for_pane checks mirrored (remote), runtime,
            // and metadata sources — a known harness means it's an agent pane.
            let Some(harness) = self.cached_agent_harness_for_pane(pane_id) else {
                continue;
            };
            if matches!(harness, AgentHarness::Unknown) {
                continue;
            }

            let visible = match badge_mode {
                AgentTabBadgeMode::Identity => true,
                AgentTabBadgeMode::Turn => runtime_by_pane
                    .get(&pane_id)
                    .map_or(mirrored_badge.waiting_on_user, |rt| {
                        Self::agent_waiting_on_user(rt)
                    }),
                AgentTabBadgeMode::Attention => runtime_by_pane
                    .get(&pane_id)
                    .map_or(mirrored_badge.needs_attention, |rt| {
                        self.agent_turn_needs_attention(pane_id, rt)
                    }),
                AgentTabBadgeMode::Off => false,
            };

            if visible {
                let key = std::mem::discriminant(&harness);
                if seen.insert(key) {
                    icons.push(harness);
                }
            }
        }

        log::trace!(
            "visible_harness_icons_for_tab tab_id={} view_id={:?} badge_mode={:?} icons={:?}",
            tab_id,
            view_id.map(|v| &v.0),
            badge_mode,
            icons
        );

        icons
    }

    pub fn effective_tab_title_for_view(&self, view_id: &ClientViewId, tab_id: TabId) -> String {
        let base_title = self.raw_tab_title(tab_id);
        // Only prepend text badge for agents with Unknown harness (no icon available).
        // When a known harness icon is present, the icon IS the badge.
        if self.should_badge_tab_for_agents(tab_id, Some(view_id))
            && !self.tab_has_known_harness(tab_id)
        {
            if let Some(badge) = Self::agent_tab_badge_text() {
                return format!("{badge}{base_title}");
            }
        }
        base_title
    }

    pub fn effective_tab_title(&self, tab_id: TabId) -> String {
        match self.active_view_id() {
            Some(view_id) => self.effective_tab_title_for_view(view_id.as_ref(), tab_id),
            None => {
                let base_title = self.raw_tab_title(tab_id);
                if self.should_badge_tab_for_agents(tab_id, None)
                    && !self.tab_has_known_harness(tab_id)
                {
                    if let Some(badge) = Self::agent_tab_badge_text() {
                        return format!("{badge}{base_title}");
                    }
                }
                base_title
            }
        }
    }

    fn runtime_snapshot_for_agent(
        &self,
        pane_id: PaneId,
        metadata: &AgentMetadata,
        pane: &Arc<dyn Pane>,
    ) -> AgentRuntimeSnapshot {
        self.refresh_agent_runtime_for_pane_with_update(
            pane_id,
            false,
            AgentRefreshPolicy::Throttled,
            |_| {},
        );
        self.agent_runtime_by_pane
            .read()
            .get(&pane_id)
            .cloned()
            .unwrap_or_else(|| {
                let mut runtime = AgentRuntimeSnapshot::new(metadata);
                runtime.alive = !pane.is_dead();
                runtime.foreground_process_name =
                    pane.get_foreground_process_name(CachePolicy::AllowStale);
                runtime.tty_name = pane.tty_name();
                runtime.terminal_progress = pane.get_progress();
                runtime.harness = infer_harness(
                    &metadata.launch_cmd,
                    runtime.foreground_process_name.as_deref(),
                );
                finalize_runtime_snapshot(&mut runtime);
                runtime
            })
    }

    fn cached_runtime_snapshot_for_agent(
        &self,
        pane_id: PaneId,
        metadata: &AgentMetadata,
        pane: &Arc<dyn Pane>,
    ) -> AgentRuntimeSnapshot {
        let mut runtime = self
            .agent_runtime_by_pane
            .read()
            .get(&pane_id)
            .cloned()
            .unwrap_or_else(|| AgentRuntimeSnapshot::new(metadata));
        runtime.alive = !pane.is_dead();
        runtime.tty_name = runtime.tty_name.or_else(|| pane.tty_name());
        runtime.terminal_progress = pane.get_progress();
        finalize_runtime_snapshot(&mut runtime);
        runtime.status = derive_runtime_status(&runtime);
        runtime
    }

    fn snapshot_with_runtime(
        &self,
        pane_id: PaneId,
        metadata: AgentMetadata,
        runtime: AgentRuntimeSnapshot,
        origin: AgentOrigin,
        detection_source: Option<String>,
    ) -> Option<AgentSnapshot> {
        let pane = self.get_pane(pane_id)?;
        let (_domain_id, window_id, tab_id) = self.resolve_pane_id(pane_id)?;
        let window = self.get_window(window_id)?;
        let needs_attention = self.agent_turn_needs_attention(pane_id, &runtime);
        Some(AgentSnapshot {
            metadata,
            runtime,
            pane_id,
            tab_id,
            window_id,
            workspace: window.get_workspace().to_string(),
            domain_id: pane.domain_id(),
            origin,
            detection_source,
            needs_attention,
        })
    }

    fn build_agent_snapshot(
        &self,
        pane_id: PaneId,
        metadata: Arc<AgentMetadata>,
    ) -> Option<AgentSnapshot> {
        let pane = self.get_pane(pane_id)?;
        let runtime = self.runtime_snapshot_for_agent(pane_id, metadata.as_ref(), &pane);
        let foreground_process_info = pane.get_foreground_process_info(CachePolicy::AllowStale);
        if !adopted_agent_matches_process_info(metadata.as_ref(), foreground_process_info.as_ref())
            && runtime.session_path.is_none()
        {
            self.clear_agent_metadata(pane_id);
            return None;
        }
        self.snapshot_with_runtime(
            pane_id,
            (*metadata).clone(),
            runtime,
            AgentOrigin::Adopted,
            None,
        )
    }

    fn build_cached_agent_snapshot(
        &self,
        pane_id: PaneId,
        metadata: Arc<AgentMetadata>,
    ) -> Option<AgentSnapshot> {
        let pane = self.get_pane(pane_id)?;
        let runtime = self.cached_runtime_snapshot_for_agent(pane_id, metadata.as_ref(), &pane);
        self.snapshot_with_runtime(
            pane_id,
            (*metadata).clone(),
            runtime,
            AgentOrigin::Adopted,
            None,
        )
    }

    fn build_cached_detected_agent_snapshot(
        &self,
        pane_id: PaneId,
        taken_names: &mut HashSet<String>,
    ) -> Option<AgentSnapshot> {
        if self.get_agent_metadata_for_pane(pane_id).is_some() {
            return None;
        }

        let runtime = self.agent_runtime_by_pane.read().get(&pane_id).cloned()?;
        if matches!(runtime.harness, crate::agent::AgentHarness::Unknown) {
            return None;
        }

        if let Some(candidate) = self.agent_adoption_candidates.read().get(&pane_id).cloned() {
            let base_name =
                Self::detected_agent_name_base(&candidate.harness, &candidate.declared_cwd);
            let name = Self::next_available_agent_name(taken_names, &base_name);
            taken_names.insert(name.clone());
            return self.snapshot_with_runtime(
                pane_id,
                AgentMetadata {
                    agent_id: format!("detected-pane-{pane_id}"),
                    name,
                    launch_cmd: candidate.launch_cmd,
                    declared_cwd: candidate.declared_cwd,
                    adopted_pid: candidate.foreground_pid,
                    adopted_start_time: candidate.process_start_time,
                    created_at: candidate.created_at,
                    repo_root: None,
                    worktree: None,
                    branch: None,
                    managed_checkout: false,
                    codex_app_server: None,
                },
                runtime,
                AgentOrigin::Detected,
                Some("cached".to_string()),
            );
        }

        let pane = self.get_pane(pane_id)?;

        let declared_cwd = Self::pane_declared_cwd(&pane, None).unwrap_or_default();
        let base_name = Self::detected_agent_name_base(&runtime.harness, &declared_cwd);
        let name = Self::next_available_agent_name(taken_names, &base_name);
        taken_names.insert(name.clone());
        let launch_cmd = default_launch_cmd_for_harness(&runtime.harness)?.to_string();
        let created_at = Self::detected_agent_created_at(&runtime);

        self.snapshot_with_runtime(
            pane_id,
            AgentMetadata {
                agent_id: format!("detected-pane-{pane_id}"),
                name,
                launch_cmd,
                declared_cwd,
                adopted_pid: None,
                adopted_start_time: None,
                created_at,
                repo_root: None,
                worktree: None,
                branch: None,
                managed_checkout: false,
                codex_app_server: None,
            },
            runtime,
            AgentOrigin::Detected,
            Some("cached".to_string()),
        )
    }

    pub fn list_agents(&self) -> Vec<AgentSnapshot> {
        let metadata_by_pane = self.agent_metadata_by_pane.read().clone();
        let mut agents = metadata_by_pane
            .into_iter()
            .filter_map(|(pane_id, metadata)| self.build_agent_snapshot(pane_id, metadata))
            .collect::<Vec<_>>();
        let mut taken_names = agents
            .iter()
            .map(|agent| agent.metadata.name.clone())
            .collect::<HashSet<_>>();
        let now = Instant::now();
        let full_detected_scan = {
            let mut last_scan = self.last_detected_agent_full_scan.lock();
            let should_scan = last_scan
                .map(|last_scan| now.duration_since(last_scan) >= AGENT_DETECTED_FULL_SCAN_THROTTLE)
                .unwrap_or(true);
            if should_scan {
                *last_scan = Some(now);
            }
            should_scan
        };
        let mut pane_ids = if full_detected_scan {
            self.panes.read().keys().copied().collect::<Vec<_>>()
        } else {
            self.detected_agent_panes
                .read()
                .iter()
                .copied()
                .collect::<Vec<_>>()
        };
        pane_ids.sort_unstable();
        for pane_id in pane_ids {
            if self.get_agent_metadata_for_pane(pane_id).is_some() {
                continue;
            }
            let Some(state) = self.detect_agent_state_for_pane(pane_id) else {
                continue;
            };
            if Self::agent_auto_adopt_on_confirmed_session_match()
                && state.runtime.session_path.is_some()
            {
                if let Some(metadata) = self.auto_adopt_state(&state) {
                    if let Some(snapshot) = self.build_agent_snapshot(pane_id, metadata) {
                        taken_names.insert(snapshot.metadata.name.clone());
                        agents.push(snapshot);
                    }
                    continue;
                }
            }
            let base_name =
                Self::detected_agent_name_base(&state.runtime.harness, &state.declared_cwd);
            let name = Self::next_available_agent_name(&taken_names, &base_name);
            taken_names.insert(name.clone());
            agents.push(self.detected_agent_snapshot_from_state(state, name));
        }
        agents.sort_by(|a, b| {
            a.metadata
                .name
                .cmp(&b.metadata.name)
                .then_with(|| a.pane_id.cmp(&b.pane_id))
        });
        agents
    }

    pub fn list_agents_cached(&self) -> Vec<AgentSnapshot> {
        let metadata_by_pane = self.agent_metadata_by_pane.read().clone();
        let mut agents = metadata_by_pane
            .into_iter()
            .filter_map(|(pane_id, metadata)| self.build_cached_agent_snapshot(pane_id, metadata))
            .collect::<Vec<_>>();
        let mut taken_names = agents
            .iter()
            .map(|agent| agent.metadata.name.clone())
            .collect::<HashSet<_>>();
        let mut pane_ids = self
            .detected_agent_panes
            .read()
            .iter()
            .copied()
            .collect::<Vec<_>>();
        pane_ids.sort_unstable();
        for pane_id in pane_ids {
            if let Some(agent) =
                self.build_cached_detected_agent_snapshot(pane_id, &mut taken_names)
            {
                agents.push(agent);
            }
        }
        agents.sort_by(|a, b| {
            a.metadata
                .name
                .cmp(&b.metadata.name)
                .then_with(|| a.pane_id.cmp(&b.pane_id))
        });
        agents
    }

    pub fn annotate_pane_tree_with_agent_metadata(&self, node: &mut crate::tab::PaneNode) {
        match node {
            crate::tab::PaneNode::Empty => {}
            crate::tab::PaneNode::Leaf(entry) => {
                entry.agent_metadata = self
                    .get_agent_metadata_for_pane(entry.pane_id)
                    .map(|metadata| (*metadata).clone());
            }
            crate::tab::PaneNode::Split { left, right, .. } => {
                self.annotate_pane_tree_with_agent_metadata(left);
                self.annotate_pane_tree_with_agent_metadata(right);
            }
        }
    }

    pub fn get_active_tab_id_for_window_for_client(
        &self,
        view_id: &ClientViewId,
        window_id: WindowId,
    ) -> Option<TabId> {
        self.client_views
            .read()
            .get(view_id)?
            .windows
            .get(&window_id)?
            .active_tab_id
    }

    pub fn get_last_active_tab_id_for_window_for_client(
        &self,
        view_id: &ClientViewId,
        window_id: WindowId,
    ) -> Option<TabId> {
        self.client_views
            .read()
            .get(view_id)?
            .windows
            .get(&window_id)?
            .last_active_tab_id
    }

    pub fn get_active_tab_for_window_for_client(
        &self,
        view_id: &ClientViewId,
        window_id: WindowId,
    ) -> Option<Arc<Tab>> {
        let tab_id = self.get_active_tab_id_for_window_for_client(view_id, window_id)?;
        self.get_tab(tab_id)
    }

    pub fn get_active_tab_for_window_for_current_identity(
        &self,
        window_id: WindowId,
    ) -> Option<Arc<Tab>> {
        let view_id = self.active_view_id()?;
        self.get_active_tab_for_window_for_client(view_id.as_ref(), window_id)
    }

    pub fn get_active_tab_idx_for_window_for_current_identity(
        &self,
        window_id: WindowId,
    ) -> Option<usize> {
        let tab_id = self
            .get_active_tab_for_window_for_current_identity(window_id)?
            .tab_id();
        let window = self.get_window(window_id)?;
        window.visible_idx_by_id(tab_id)
    }

    pub fn get_last_active_tab_idx_for_window_for_current_identity(
        &self,
        window_id: WindowId,
    ) -> Option<usize> {
        let view_id = self.active_view_id()?;
        let tab_id =
            self.get_last_active_tab_id_for_window_for_client(view_id.as_ref(), window_id)?;
        let window = self.get_window(window_id)?;
        window.visible_idx_by_id(tab_id)
    }

    pub fn get_active_pane_id_for_tab_for_client(
        &self,
        view_id: &ClientViewId,
        window_id: WindowId,
        tab_id: TabId,
    ) -> Option<PaneId> {
        self.client_views
            .read()
            .get(view_id)?
            .windows
            .get(&window_id)?
            .tabs
            .get(&tab_id)?
            .active_pane_id
    }

    pub fn get_active_pane_for_tab_for_client(
        &self,
        view_id: &ClientViewId,
        window_id: WindowId,
        tab_id: TabId,
    ) -> Option<Arc<dyn Pane>> {
        let pane_id = self.get_active_pane_id_for_tab_for_client(view_id, window_id, tab_id)?;
        self.get_pane(pane_id)
    }

    pub fn get_active_pane_for_window_for_current_identity(
        &self,
        window_id: WindowId,
    ) -> Option<Arc<dyn Pane>> {
        let view_id = self.active_view_id()?;
        let tab_id = self.get_active_tab_id_for_window_for_client(view_id.as_ref(), window_id)?;
        self.get_active_pane_for_tab_for_client(view_id.as_ref(), window_id, tab_id)
    }

    pub fn resolve_focused_pane(
        &self,
        client_id: &ClientId,
    ) -> Option<(DomainId, WindowId, TabId, PaneId)> {
        let pane_id = self.clients.read().get(client_id)?.focused_pane_id?;
        let (domain, window, tab) = self.resolve_pane_id(pane_id)?;
        Some((domain, window, tab, pane_id))
    }

    /// Heavy per-client focus path.
    /// Updates current focus bookkeeping, clears per-view attention,
    /// and synthesizes pane focus callbacks.
    pub fn record_focus_for_client(&self, client_id: &ClientId, pane_id: PaneId) {
        let mut prior = None;
        let mut view_id = None;
        if let Some(info) = self.clients.write().get_mut(client_id) {
            prior = info.focused_pane_id;
            view_id = Some(info.view_id.clone());
            info.update_focused_pane(pane_id);
        }

        if let (Some(view_id), Some((_domain_id, window_id, tab_id))) =
            (view_id, self.resolve_pane_id(pane_id))
        {
            let _ =
                self.set_active_pane_for_client_view(view_id.as_ref(), window_id, tab_id, pane_id);
            self.acknowledge_agent_attention(pane_id);
        }

        if prior == Some(pane_id) {
            return;
        }
        // Synthesize focus events
        if let Some(prior_id) = prior {
            if let Some(pane) = self.get_pane(prior_id) {
                pane.focus_changed(false);
            }
        }
        if let Some(pane) = self.get_pane(pane_id) {
            pane.focus_changed(true);
        }
    }

    /// Updates client focus bookkeeping and per-view active pane state
    /// without synthesizing pane focus callbacks.
    pub fn set_focused_pane_for_client(
        &self,
        client_id: &ClientId,
        pane_id: PaneId,
    ) -> anyhow::Result<()> {
        let (_domain_id, window_id, tab_id) = self
            .resolve_pane_id(pane_id)
            .ok_or_else(|| anyhow!("pane {pane_id} not found"))?;
        let tab = self
            .get_tab(tab_id)
            .ok_or_else(|| anyhow!("tab {tab_id} not found"))?;

        let view_id = {
            let mut clients = self.clients.write();
            let info = clients
                .get_mut(client_id)
                .ok_or_else(|| anyhow!("client {:?} not found", client_id))?;
            let view_id = info.view_id.clone();
            info.update_focused_pane(pane_id);
            view_id
        };

        let mut client_views = self.client_views.write();
        let view_state = client_views.entry((*view_id).clone()).or_default();
        let window_state = view_state.windows.entry(window_id).or_default();
        window_state.set_active_pane(tab_id, pane_id);
        Self::seed_view_state_for_tab(window_state, &tab);
        drop(client_views);
        self.acknowledge_agent_attention(pane_id);

        Ok(())
    }

    fn set_focused_pane_for_current_identity_lightweight(
        &self,
        pane_id: PaneId,
    ) -> anyhow::Result<()> {
        let Some(client_id) = self.active_identity() else {
            return Ok(());
        };
        self.set_focused_pane_for_client(client_id.as_ref(), pane_id)
    }

    /// Called by PaneFocused event handlers to reconcile a remote
    /// pane focus event and apply its effects locally
    pub fn focus_pane_and_containing_tab(&self, pane_id: PaneId) -> anyhow::Result<()> {
        let pane = self
            .get_pane(pane_id)
            .ok_or_else(|| anyhow::anyhow!("pane {pane_id} not found"))?;

        let (_domain, window_id, tab_id) = self
            .resolve_pane_id(pane_id)
            .ok_or_else(|| anyhow::anyhow!("can't find {pane_id} in the mux"))?;

        log::debug!(
            "focus_pane_and_containing_tab start pane_id={pane_id} window_id={window_id} tab_id={tab_id}"
        );

        self.reconcile_focused_pane_for_current_identity(pane_id)?;

        // Focus/activate the pane locally
        let tab = self
            .get_tab(tab_id)
            .ok_or_else(|| anyhow::anyhow!("tab {tab_id} not found"))?;

        tab.set_active_pane(&pane, NotifyMux::No);

        log::debug!(
            "focus_pane_and_containing_tab complete pane_id={pane_id} window_id={window_id} tab_id={tab_id}"
        );

        Ok(())
    }

    pub fn reconcile_focused_pane_for_current_identity(
        &self,
        pane_id: PaneId,
    ) -> anyhow::Result<()> {
        let (_domain_id, window_id, tab_id) = self
            .resolve_pane_id(pane_id)
            .ok_or_else(|| anyhow::anyhow!("can't find {pane_id} in the mux"))?;
        self.set_active_pane_for_current_identity(window_id, tab_id, pane_id)?;
        self.acknowledge_agent_attention(pane_id);
        Ok(())
    }

    fn seed_view_state_for_tab(window_state: &mut ClientWindowViewState, tab: &Arc<Tab>) {
        let tab_id = tab.tab_id();
        window_state.tabs.entry(tab_id).or_default();
        if window_state.active_tab_id.is_none() {
            window_state.active_tab_id = Some(tab_id);
        }
        let tab_state = window_state.tabs.entry(tab_id).or_default();
        if tab_state.active_pane_id.is_none() {
            if let Some(pane) = tab.get_active_pane() {
                tab_state.active_pane_id = Some(pane.pane_id());
            }
        }
    }

    fn default_workspace_for_new_client(&self) -> String {
        let default_workspace = self.get_default_workspace();
        if !self.is_workspace_empty(&default_workspace) {
            return default_workspace;
        }

        self.iter_workspaces()
            .into_iter()
            .find(|workspace| !self.is_workspace_empty(workspace))
            .unwrap_or(default_workspace)
    }

    fn build_bootstrap_view_state_for_workspace(
        &self,
        workspace: &str,
    ) -> (ClientViewState, Option<PaneId>) {
        let mut view_state = ClientViewState::default();
        let mut focused_pane_id = None;

        for window_id in self.iter_windows_in_workspace(workspace) {
            let Some(window) = self.get_window(window_id) else {
                continue;
            };
            let window_state = view_state.windows.entry(window_id).or_default();
            for tab in window.iter_visible() {
                Self::seed_view_state_for_tab(window_state, tab);
            }
            for tab in window
                .iter()
                .filter(|tab| window.is_tab_parked(tab.tab_id()))
            {
                Self::seed_view_state_for_tab(window_state, &tab);
            }
            if focused_pane_id.is_none() {
                focused_pane_id = window_state.active_pane_id();
            }
        }

        (view_state, focused_pane_id)
    }

    fn preferred_focused_pane_for_view_in_workspace(
        &self,
        view_id: &ClientViewId,
        workspace: &str,
    ) -> Option<PaneId> {
        let window_ids = self.iter_windows_in_workspace(workspace);
        let client_views = self.client_views.read();
        let view_state = client_views.get(view_id)?;
        for window_id in window_ids {
            if let Some(pane_id) = view_state
                .windows
                .get(&window_id)
                .and_then(|window_state| window_state.active_pane_id())
            {
                if self.resolve_pane_id(pane_id).is_some() {
                    return Some(pane_id);
                }
            }
        }
        None
    }

    fn merge_bootstrap_view_state(target: &mut ClientViewState, mut bootstrap: ClientViewState) {
        for (window_id, mut bootstrap_window_state) in bootstrap.windows.drain() {
            let window_state = target.windows.entry(window_id).or_default();
            if window_state.active_tab_id.is_none() {
                window_state.active_tab_id = bootstrap_window_state.active_tab_id.take();
            }
            if window_state.last_active_tab_id.is_none() {
                window_state.last_active_tab_id = bootstrap_window_state.last_active_tab_id.take();
            }
            for (tab_id, bootstrap_tab_state) in bootstrap_window_state.tabs.drain() {
                let tab_state = window_state.tabs.entry(tab_id).or_default();
                if tab_state.active_pane_id.is_none() {
                    tab_state.active_pane_id = bootstrap_tab_state.active_pane_id;
                }
            }
        }
    }

    pub fn set_active_tab_for_client_view(
        &self,
        view_id: &ClientViewId,
        window_id: WindowId,
        tab_id: TabId,
    ) -> anyhow::Result<()> {
        self.set_active_tab_for_client_view_impl(view_id, window_id, tab_id, true)
    }

    fn set_active_tab_for_client_view_impl(
        &self,
        view_id: &ClientViewId,
        window_id: WindowId,
        tab_id: TabId,
        notify: bool,
    ) -> anyhow::Result<()> {
        let tab = self
            .get_tab(tab_id)
            .ok_or_else(|| anyhow!("tab {tab_id} not found"))?;
        let window = self
            .get_window(window_id)
            .ok_or_else(|| anyhow!("window {window_id} not found"))?;
        if window.idx_by_id(tab_id).is_none() {
            anyhow::bail!("tab {tab_id} is not in window {window_id}");
        }
        if window.is_tab_parked(tab_id) {
            anyhow::bail!("tab {tab_id} is parked in window {window_id}");
        }
        drop(window);

        let mut client_views = self.client_views.write();
        let view_state = client_views.entry(view_id.clone()).or_default();
        let window_state = view_state.windows.entry(window_id).or_default();
        window_state.set_active_tab(tab_id);
        Self::seed_view_state_for_tab(window_state, &tab);
        drop(client_views);

        if notify {
            self.notify(MuxNotification::WindowInvalidated(window_id));
        }
        Ok(())
    }

    pub fn set_active_tab_for_current_identity(
        &self,
        window_id: WindowId,
        tab_id: TabId,
    ) -> anyhow::Result<()> {
        let view_id = self
            .active_view_id()
            .ok_or_else(|| anyhow!("no current client identity"))?;
        self.set_active_tab_for_client_view(view_id.as_ref(), window_id, tab_id)
    }

    /// Updates current client view state without invalidating the window.
    /// This is intended for attach-time reconciliation where the caller is
    /// already synchronizing the pane tree and wants to avoid re-entrant GUI
    /// notifications while wiring up local state.
    pub fn seed_active_tab_for_current_identity(
        &self,
        window_id: WindowId,
        tab_id: TabId,
    ) -> anyhow::Result<()> {
        let view_id = self
            .active_view_id()
            .ok_or_else(|| anyhow!("no current client identity"))?;
        self.set_active_tab_for_client_view_impl(view_id.as_ref(), window_id, tab_id, false)
    }

    pub fn set_active_pane_for_client_view(
        &self,
        view_id: &ClientViewId,
        window_id: WindowId,
        tab_id: TabId,
        pane_id: PaneId,
    ) -> anyhow::Result<()> {
        self.set_active_pane_for_client_view_impl(view_id, window_id, tab_id, pane_id, true)
    }

    fn set_active_pane_for_client_view_impl(
        &self,
        view_id: &ClientViewId,
        window_id: WindowId,
        tab_id: TabId,
        pane_id: PaneId,
        notify: bool,
    ) -> anyhow::Result<()> {
        let (_domain_id, pane_window_id, pane_tab_id) = self
            .resolve_pane_id(pane_id)
            .ok_or_else(|| anyhow!("pane {pane_id} not found"))?;
        if pane_window_id != window_id || pane_tab_id != tab_id {
            anyhow::bail!(
                "pane {pane_id} is in window/tab {pane_window_id}/{pane_tab_id}, not {window_id}/{tab_id}"
            );
        }

        let tab = self
            .get_tab(tab_id)
            .ok_or_else(|| anyhow!("tab {tab_id} not found"))?;
        let mut client_views = self.client_views.write();
        let view_state = client_views.entry(view_id.clone()).or_default();
        let window_state = view_state.windows.entry(window_id).or_default();
        window_state.set_active_pane(tab_id, pane_id);
        Self::seed_view_state_for_tab(window_state, &tab);
        drop(client_views);

        if notify {
            self.notify(MuxNotification::WindowInvalidated(window_id));
        }
        Ok(())
    }

    pub fn set_active_pane_for_current_identity(
        &self,
        window_id: WindowId,
        tab_id: TabId,
        pane_id: PaneId,
    ) -> anyhow::Result<()> {
        let view_id = self
            .active_view_id()
            .ok_or_else(|| anyhow!("no current client identity"))?;
        self.set_active_pane_for_client_view(view_id.as_ref(), window_id, tab_id, pane_id)
    }

    /// Updates current client view state without invalidating the window.
    /// This is intended for attach-time reconciliation where the caller is
    /// already synchronizing the pane tree and wants to avoid re-entrant GUI
    /// notifications while wiring up local state.
    pub fn seed_active_pane_for_current_identity(
        &self,
        window_id: WindowId,
        tab_id: TabId,
        pane_id: PaneId,
    ) -> anyhow::Result<()> {
        let view_id = self
            .active_view_id()
            .ok_or_else(|| anyhow!("no current client identity"))?;
        self.set_active_pane_for_client_view_impl(
            view_id.as_ref(),
            window_id,
            tab_id,
            pane_id,
            false,
        )
    }

    pub fn register_client(&self, client_id: Arc<ClientId>, view_id: Arc<ClientViewId>) {
        let workspace = self.default_workspace_for_new_client();
        let (bootstrap_view_state, bootstrap_focused_pane_id) =
            self.build_bootstrap_view_state_for_workspace(&workspace);

        {
            let mut client_views = self.client_views.write();
            let view_state = client_views.entry((*view_id).clone()).or_default();
            Self::merge_bootstrap_view_state(view_state, bootstrap_view_state);
        }

        let focused_pane_id = self
            .preferred_focused_pane_for_view_in_workspace(view_id.as_ref(), &workspace)
            .or(bootstrap_focused_pane_id);

        let client_key = (*client_id).clone();
        let mut info = ClientInfo::new(client_id, view_id);
        info.active_workspace.replace(workspace);
        info.focused_pane_id = focused_pane_id;
        self.clients.write().insert(client_key, info);
    }

    pub fn iter_clients(&self) -> Vec<ClientInfo> {
        self.clients
            .read()
            .values()
            .map(|info| info.clone())
            .collect()
    }

    /// Returns a list of the unique workspace names known to the mux.
    /// This is taken from all known windows.
    pub fn iter_workspaces(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .windows
            .read()
            .values()
            .map(|w| w.get_workspace().to_string())
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Generate a new unique workspace name
    pub fn generate_workspace_name(&self) -> String {
        let used = self.iter_workspaces();
        for candidate in names::Generator::default() {
            if !used.contains(&candidate) {
                return candidate;
            }
        }
        unreachable!();
    }

    /// Returns the effective active workspace name
    pub fn active_workspace(&self) -> String {
        self.identity
            .read()
            .as_ref()
            .and_then(|ident| {
                self.clients
                    .read()
                    .get(&ident)
                    .and_then(|info| info.active_workspace.clone())
            })
            .unwrap_or_else(|| self.get_default_workspace())
    }

    /// Returns the effective active workspace name for a given client
    pub fn active_workspace_for_client(&self, ident: &Arc<ClientId>) -> String {
        self.clients
            .read()
            .get(&ident)
            .and_then(|info| info.active_workspace.clone())
            .unwrap_or_else(|| self.get_default_workspace())
    }

    pub fn set_active_workspace_for_client(&self, ident: &Arc<ClientId>, workspace: &str) {
        let mut clients = self.clients.write();
        if let Some(info) = clients.get_mut(&ident) {
            info.active_workspace.replace(workspace.to_string());
            self.notify(MuxNotification::ActiveWorkspaceChanged(ident.clone()));
        }
    }

    /// Assigns the active workspace name for the current identity
    pub fn set_active_workspace(&self, workspace: &str) {
        if let Some(ident) = self.identity.read().clone() {
            self.set_active_workspace_for_client(&ident, workspace);
        }
    }

    pub fn rename_workspace(&self, old_workspace: &str, new_workspace: &str) {
        if old_workspace == new_workspace {
            return;
        }
        self.notify(MuxNotification::WorkspaceRenamed {
            old_workspace: old_workspace.to_string(),
            new_workspace: new_workspace.to_string(),
        });

        for window in self.windows.write().values_mut() {
            if window.get_workspace() == old_workspace {
                window.set_workspace(new_workspace);
            }
        }
        self.recompute_pane_count();
        for client in self.clients.write().values_mut() {
            if client.active_workspace.as_deref() == Some(old_workspace) {
                client.active_workspace.replace(new_workspace.to_string());
                self.notify(MuxNotification::ActiveWorkspaceChanged(
                    client.client_id.clone(),
                ));
            }
        }
    }

    /// Overrides the current client identity.
    /// Returns `IdentityHolder` which will restore the prior identity
    /// when it is dropped.
    /// This can be used to change the identity for the duration of a block.
    pub fn with_identity(&self, id: Option<Arc<ClientId>>) -> IdentityHolder {
        let prior = self.replace_identity(id);
        IdentityHolder { prior }
    }

    /// Replace the identity, returning the prior identity
    pub fn replace_identity(&self, id: Option<Arc<ClientId>>) -> Option<Arc<ClientId>> {
        std::mem::replace(&mut *self.identity.write(), id)
    }

    /// Returns the active identity
    pub fn active_identity(&self) -> Option<Arc<ClientId>> {
        self.identity.read().clone()
    }

    pub fn unregister_client(&self, client_id: &ClientId) {
        self.clients.write().remove(client_id);
    }

    pub fn subscribe<F>(&self, subscriber: F)
    where
        F: Fn(MuxNotification) -> bool + 'static + Send + Sync,
    {
        let sub_id = SUB_ID.fetch_add(1, Ordering::Relaxed);
        self.subscribers
            .write()
            .insert(sub_id, Box::new(subscriber));
    }

    fn should_skip_queued_notification(&self, notification: &MuxNotification) -> bool {
        match notification {
            MuxNotification::PaneOutput(pane_id) => !self
                .pending_pane_output_notifications
                .lock()
                .insert(*pane_id),
            _ => false,
        }
    }

    fn clear_pending_notification(&self, notification: &MuxNotification) {
        if let MuxNotification::PaneOutput(pane_id) = notification {
            self.pending_pane_output_notifications
                .lock()
                .remove(pane_id);
        }
    }

    pub fn notify(&self, notification: MuxNotification) {
        self.clear_pending_notification(&notification);
        if notification_changes_saved_session(&notification) {
            crate::session_persistence::request_session_save();
        }
        match &notification {
            MuxNotification::PaneOutput(pane_id) => self.record_agent_output(*pane_id),
            MuxNotification::Alert {
                pane_id,
                alert: wakterm_term::Alert::Progress(progress),
            } => self.record_agent_terminal_progress(*pane_id, progress.clone()),
            _ => {}
        }
        let mut subscribers = self.subscribers.write();
        subscribers.retain(|_, notify| notify(notification.clone()));
    }

    pub fn notify_tab_resized(&self, tab_id: TabId) {
        self.notify(MuxNotification::TabResized {
            tab_id,
            origin: self.active_identity(),
        });
    }

    pub fn notify_tab_order_changed(&self, window_id: WindowId, tab_ids: Vec<TabId>) {
        self.notify(MuxNotification::TabOrderChanged {
            window_id,
            tab_ids,
            origin: self.active_identity(),
        });
    }

    pub fn notify_parked_tabs_changed(
        &self,
        window_id: WindowId,
        tab_ids: Vec<TabId>,
        parked_tab_ids: Vec<TabId>,
    ) {
        self.notify(MuxNotification::ParkedTabsChanged {
            window_id,
            tab_ids,
            parked_tab_ids,
            origin: self.active_identity(),
        });
    }

    pub fn set_tab_parked(
        &self,
        window_id: WindowId,
        tab_id: TabId,
        parked: bool,
    ) -> anyhow::Result<bool> {
        let (tab_ids, parked_tab_ids, changed) = {
            let mut window = self
                .get_window_mut(window_id)
                .ok_or_else(|| anyhow!("no such window {}", window_id))?;
            let tab_ids = window.iter().map(|tab| tab.tab_id()).collect::<Vec<_>>();
            ensure!(
                tab_ids.contains(&tab_id),
                "tab {} does not belong to window {}",
                tab_id,
                window_id
            );
            let mut parked_tab_ids = window.parked_tab_ids();
            if parked {
                if !parked_tab_ids.contains(&tab_id) {
                    parked_tab_ids.push(tab_id);
                }
            } else {
                parked_tab_ids.retain(|candidate| *candidate != tab_id);
            }
            parked_tab_ids.sort_by_key(|candidate| {
                tab_ids
                    .iter()
                    .position(|tab_id| tab_id == candidate)
                    .unwrap_or(usize::MAX)
            });
            let changed = window.apply_parked_tabs(&tab_ids, &parked_tab_ids)?;
            (tab_ids, parked_tab_ids, changed)
        };
        if changed {
            self.repair_client_views_after_parked_change(window_id);
            self.notify(MuxNotification::WindowInvalidated(window_id));
            self.notify_parked_tabs_changed(window_id, tab_ids, parked_tab_ids);
        }
        Ok(changed)
    }

    pub fn repair_client_views_after_parked_change(&self, window_id: WindowId) {
        let Some(window) = self.get_window(window_id) else {
            return;
        };
        let tab_ids = window.iter().map(|tab| tab.tab_id()).collect::<Vec<_>>();
        let visible_tab_ids = window
            .iter_visible()
            .map(|tab| tab.tab_id())
            .collect::<Vec<_>>();
        let parked_tab_ids = window.parked_tab_ids().into_iter().collect::<HashSet<_>>();
        drop(window);
        if visible_tab_ids.is_empty() {
            return;
        }

        let replacement_for = |tab_id: TabId| {
            let old_idx = tab_ids.iter().position(|candidate| *candidate == tab_id)?;
            visible_tab_ids
                .iter()
                .copied()
                .find(|candidate| {
                    tab_ids
                        .iter()
                        .position(|tab_id| tab_id == candidate)
                        .is_some_and(|idx| idx >= old_idx)
                })
                .or_else(|| visible_tab_ids.last().copied())
        };

        let mut client_views = self.client_views.write();
        for view in client_views.values_mut() {
            let Some(window_state) = view.windows.get_mut(&window_id) else {
                continue;
            };
            if let Some(active_tab_id) = window_state.active_tab_id {
                if parked_tab_ids.contains(&active_tab_id) {
                    if let Some(replacement) = replacement_for(active_tab_id) {
                        window_state.active_tab_id = Some(replacement);
                    }
                }
            } else {
                window_state.active_tab_id = visible_tab_ids.first().copied();
            }
            if window_state
                .last_active_tab_id
                .is_some_and(|tab_id| parked_tab_ids.contains(&tab_id))
            {
                window_state.last_active_tab_id = None;
            }
        }
        let active_pane_by_view = client_views
            .iter()
            .filter_map(|(view_id, view)| {
                view.windows
                    .get(&window_id)
                    .and_then(ClientWindowViewState::active_pane_id)
                    .map(|pane_id| (view_id.clone(), pane_id))
            })
            .collect::<HashMap<_, _>>();
        drop(client_views);

        for client in self.clients.write().values_mut() {
            let focused_is_parked_in_window = client
                .focused_pane_id
                .and_then(|pane_id| self.resolve_pane_id(pane_id))
                .is_some_and(|(_, pane_window_id, tab_id)| {
                    pane_window_id == window_id && parked_tab_ids.contains(&tab_id)
                });
            if focused_is_parked_in_window {
                client.focused_pane_id = active_pane_by_view.get(client.view_id.as_ref()).copied();
            }
        }
    }

    pub fn notify_from_any_thread(notification: MuxNotification) {
        if let Some(mux) = Mux::try_get() {
            if mux.is_main_thread() {
                mux.notify(notification);
                return;
            }
            if mux.should_skip_queued_notification(&notification) {
                return;
            }
        }
        promise::spawn::spawn_into_main_thread(async {
            if let Some(mux) = Mux::try_get() {
                mux.notify(notification);
            }
        })
        .detach();
    }

    pub fn default_domain(&self) -> Arc<dyn Domain> {
        self.default_domain.read().as_ref().map(Arc::clone).unwrap()
    }

    pub fn set_default_domain(&self, domain: &Arc<dyn Domain>) {
        *self.default_domain.write() = Some(Arc::clone(domain));
    }

    pub fn get_domain(&self, id: DomainId) -> Option<Arc<dyn Domain>> {
        self.domains.read().get(&id).cloned()
    }

    pub fn get_domain_by_name(&self, name: &str) -> Option<Arc<dyn Domain>> {
        self.domains_by_name.read().get(name).cloned()
    }

    pub fn add_domain(&self, domain: &Arc<dyn Domain>) {
        if self.default_domain.read().is_none() {
            *self.default_domain.write() = Some(Arc::clone(domain));
        }
        self.domains
            .write()
            .insert(domain.domain_id(), Arc::clone(domain));
        self.domains_by_name
            .write()
            .insert(domain.domain_name().to_string(), Arc::clone(domain));
    }

    pub fn set_mux(mux: &Arc<Mux>) {
        MUX.lock().replace(Arc::clone(mux));
    }

    pub fn shutdown() {
        MUX.lock().take();
    }

    pub fn get() -> Arc<Mux> {
        Self::try_get().unwrap()
    }

    pub fn agent_service(&self) -> agent_service::AgentService<'_> {
        agent_service::AgentService::new(self)
    }

    pub fn try_get() -> Option<Arc<Mux>> {
        MUX.lock().as_ref().map(Arc::clone)
    }

    pub fn get_pane(&self, pane_id: PaneId) -> Option<Arc<dyn Pane>> {
        self.panes.read().get(&pane_id).map(Arc::clone)
    }

    pub fn get_tab(&self, tab_id: TabId) -> Option<Arc<Tab>> {
        self.tabs.read().get(&tab_id).map(Arc::clone)
    }

    pub fn add_pane(&self, pane: &Arc<dyn Pane>) -> Result<(), Error> {
        if self.panes.read().contains_key(&pane.pane_id()) {
            return Ok(());
        }

        let clipboard: Arc<dyn Clipboard> = Arc::new(MuxClipboard {
            pane_id: pane.pane_id(),
        });
        pane.set_clipboard(&clipboard);

        let downloader: Arc<dyn DownloadHandler> = Arc::new(MuxDownloader {});
        pane.set_download_handler(&downloader);

        self.panes.write().insert(pane.pane_id(), Arc::clone(pane));
        self.invalidate_tab_resource_status();
        let pane_id = pane.pane_id();
        if let Some(reader) = pane.reader()? {
            let banner = self.banner.read().clone();
            let pane = Arc::downgrade(pane);
            thread::spawn(move || read_from_pane_pty(pane, banner, reader));
        }
        self.recompute_pane_count();
        self.notify(MuxNotification::PaneAdded(pane_id));
        Ok(())
    }

    pub fn add_tab_no_panes(&self, tab: &Arc<Tab>) {
        self.tabs.write().insert(tab.tab_id(), Arc::clone(tab));
        self.invalidate_tab_resource_status();
        self.recompute_pane_count();
    }

    pub fn add_tab_and_active_pane(&self, tab: &Arc<Tab>) -> Result<(), Error> {
        self.tabs.write().insert(tab.tab_id(), Arc::clone(tab));
        let pane = tab
            .get_active_pane()
            .ok_or_else(|| anyhow!("tab MUST have an active pane"))?;
        self.add_pane(&pane)
    }

    fn remove_pane_internal(&self, pane_id: PaneId) {
        log::debug!("removing pane {}", pane_id);
        let mut changed = false;
        let pane_location = self.resolve_pane_id(pane_id);
        self.invalidate_tab_resource_status();
        self.clear_agent_metadata(pane_id);
        self.clear_detected_agent_info(pane_id);
        self.agent_artifact_watcher.lock().unwatch_pane(pane_id);
        self.agent_observer_state_by_pane.write().remove(&pane_id);
        self.mirrored_agent_harness_by_pane.write().remove(&pane_id);
        self.mirrored_agent_cwd_by_pane.write().remove(&pane_id);
        self.mirrored_agent_snapshot_by_pane
            .write()
            .remove(&pane_id);
        if let Some(pane) = self.panes.write().remove(&pane_id).clone() {
            log::debug!("killing pane {}", pane_id);
            pane.kill();
            self.notify(MuxNotification::PaneRemoved(pane_id));
            changed = true;
        }

        if let Some((_domain_id, window_id, tab_id)) = pane_location {
            let replacement_pane_id = self
                .get_tab(tab_id)
                .and_then(|tab| tab.get_active_pane())
                .map(|pane| pane.pane_id());
            let mut client_views = self.client_views.write();
            for view_state in client_views.values_mut() {
                if let Some(window_state) = view_state.windows.get_mut(&window_id) {
                    if let Some(tab_state) = window_state.tabs.get_mut(&tab_id) {
                        if tab_state.active_pane_id == Some(pane_id) {
                            tab_state.active_pane_id = replacement_pane_id;
                        }
                    }
                }
            }
        }

        if changed {
            self.recompute_pane_count();
        }
    }

    fn remove_tab_internal(&self, tab_id: TabId) -> Option<Arc<Tab>> {
        log::debug!("remove_tab_internal tab {}", tab_id);

        let tab = self.tabs.write().remove(&tab_id)?;
        self.invalidate_tab_resource_status();
        self.mirrored_agent_badge_by_tab.write().remove(&tab_id);
        self.mirrored_tab_rss_bytes.write().remove(&tab_id);

        let mut removed_from_windows = vec![];
        if let Some(mut windows) = self.windows.try_write() {
            for w in windows.values_mut() {
                if let Some(idx) = w.idx_by_id(tab_id) {
                    w.remove_by_id(tab_id);
                    removed_from_windows.push((
                        w.window_id(),
                        idx,
                        w.iter().map(|tab| tab.tab_id()).collect::<Vec<_>>(),
                    ));
                }
            }
        }
        for (window_id, removed_idx, remaining_tab_ids) in removed_from_windows {
            self.repair_client_view_state_after_tab_removed(
                window_id,
                tab_id,
                removed_idx,
                &remaining_tab_ids,
            );
        }

        let mut pane_ids = vec![];
        for pos in tab.iter_panes_ignoring_zoom() {
            pane_ids.push(pos.pane.pane_id());
        }
        log::debug!("panes to remove: {pane_ids:?}");
        for pane_id in pane_ids {
            self.remove_pane_internal(pane_id);
        }
        self.recompute_pane_count();

        Some(tab)
    }

    fn remove_window_internal(&self, window_id: WindowId) {
        log::debug!("remove_window_internal {}", window_id);

        let window = self.windows.write().remove(&window_id);
        if let Some(window) = window {
            for view_state in self.client_views.write().values_mut() {
                view_state.windows.remove(&window_id);
            }
            // Gather all the domains referenced by this window
            let mut domains_of_window = HashSet::new();
            for tab in window.iter() {
                for pane in tab.iter_panes_ignoring_zoom() {
                    domains_of_window.insert(pane.pane.domain_id());
                }
            }

            for domain_id in domains_of_window {
                if let Some(domain) = self.get_domain(domain_id) {
                    if domain.detachable() {
                        log::info!("detaching domain");
                        if let Err(err) = domain.detach() {
                            log::error!(
                                "while detaching domain {domain_id} {}: {err:#}",
                                domain.domain_name()
                            );
                        }
                    }
                }
            }

            for tab in window.iter() {
                self.remove_tab_internal(tab.tab_id());
            }
            self.notify(MuxNotification::WindowRemoved(window_id));
        }
        self.recompute_pane_count();
    }

    pub fn remove_pane(&self, pane_id: PaneId) {
        self.remove_pane_internal(pane_id);
        self.prune_dead_windows();
    }

    pub fn remove_tab(&self, tab_id: TabId) -> Option<Arc<Tab>> {
        let tab = self.remove_tab_internal(tab_id);
        self.prune_dead_windows();
        tab
    }

    pub fn prune_dead_windows(&self) {
        if Activity::count() > 0 {
            log::trace!("prune_dead_windows: Activity::count={}", Activity::count());
            return;
        }
        let live_tab_ids: Vec<TabId> = self.tabs.read().keys().cloned().collect();
        let mut dead_windows = vec![];
        let dead_tab_ids: Vec<TabId>;

        {
            let mut windows = match self.windows.try_write() {
                Some(w) => w,
                None => {
                    // It's ok if our caller already locked it; we can prune later.
                    log::trace!("prune_dead_windows: self.windows already borrowed");
                    return;
                }
            };
            for (window_id, win) in windows.iter_mut() {
                win.prune_dead_tabs(&live_tab_ids);
                if win.is_empty() {
                    log::trace!("prune_dead_windows: window is now empty");
                    dead_windows.push(*window_id);
                }
            }

            dead_tab_ids = self
                .tabs
                .read()
                .iter()
                .filter_map(|(&id, tab)| if tab.is_dead() { Some(id) } else { None })
                .collect();
        }

        for tab_id in dead_tab_ids {
            log::trace!("tab {} is dead", tab_id);
            self.remove_tab_internal(tab_id);
        }

        for window_id in dead_windows {
            log::trace!("window {} is dead", window_id);
            self.remove_window_internal(window_id);
        }

        if self.is_empty() {
            log::trace!("prune_dead_windows: is_empty, send MuxNotification::Empty");
            self.notify(MuxNotification::Empty);
        } else {
            log::trace!("prune_dead_windows: not empty");
        }
    }

    pub fn kill_window(&self, window_id: WindowId) {
        self.remove_window_internal(window_id);
        self.prune_dead_windows();
    }

    pub fn get_window(&self, window_id: WindowId) -> Option<MappedRwLockReadGuard<'_, Window>> {
        if !self.windows.read().contains_key(&window_id) {
            return None;
        }
        Some(RwLockReadGuard::map(self.windows.read(), |windows| {
            windows.get(&window_id).unwrap()
        }))
    }

    pub fn get_window_mut(
        &self,
        window_id: WindowId,
    ) -> Option<MappedRwLockWriteGuard<'_, Window>> {
        if !self.windows.read().contains_key(&window_id) {
            return None;
        }
        Some(RwLockWriteGuard::map(self.windows.write(), |windows| {
            windows.get_mut(&window_id).unwrap()
        }))
    }

    pub fn new_empty_window(
        &self,
        workspace: Option<String>,
        position: Option<GuiPosition>,
    ) -> MuxWindowBuilder {
        let window = Window::new(workspace, position);
        let window_id = window.window_id();
        self.windows.write().insert(window_id, window);
        MuxWindowBuilder {
            window_id,
            activity: Some(Activity::new()),
            notified: false,
        }
    }

    pub fn add_tab_to_window(&self, tab: &Arc<Tab>, window_id: WindowId) -> anyhow::Result<()> {
        let tab_id = tab.tab_id();
        {
            let mut window = self
                .get_window_mut(window_id)
                .ok_or_else(|| anyhow!("add_tab_to_window: no such window_id {}", window_id))?;
            window.push(tab);
        }
        if let Some(view_id) = self.active_view_id() {
            let mut client_views = self.client_views.write();
            let view_state = client_views.entry((*view_id).clone()).or_default();
            let window_state = view_state.windows.entry(window_id).or_default();
            Self::seed_view_state_for_tab(window_state, tab);
        }
        self.recompute_pane_count();
        self.notify(MuxNotification::TabAddedToWindow { tab_id, window_id });
        Ok(())
    }

    fn repair_client_view_state_after_tab_removed(
        &self,
        window_id: WindowId,
        removed_tab_id: TabId,
        removed_tab_idx: usize,
        remaining_tab_ids: &[TabId],
    ) {
        let replacement_idx = removed_tab_idx.min(remaining_tab_ids.len().saturating_sub(1));
        let replacement_tab_id = remaining_tab_ids.get(replacement_idx).copied();
        let replacement_from_last = |state: &ClientWindowViewState| {
            state
                .last_active_tab_id
                .filter(|tab_id| remaining_tab_ids.contains(tab_id))
        };

        let mut client_views = self.client_views.write();
        for view_state in client_views.values_mut() {
            let mut remove_window_state = false;
            if let Some(window_state) = view_state.windows.get_mut(&window_id) {
                let removed_was_active = window_state.active_tab_id == Some(removed_tab_id);
                window_state.clear_removed_tab(removed_tab_id);

                if remaining_tab_ids.is_empty() {
                    remove_window_state = true;
                } else if removed_was_active {
                    let replacement = replacement_from_last(window_state).or(replacement_tab_id);
                    if let Some(tab_id) = replacement {
                        window_state.set_active_tab(tab_id);
                    }
                } else if let Some(active_tab_id) = window_state.active_tab_id {
                    if !remaining_tab_ids.contains(&active_tab_id) {
                        if let Some(tab_id) =
                            replacement_from_last(window_state).or(replacement_tab_id)
                        {
                            window_state.set_active_tab(tab_id);
                        }
                    }
                }
            }
            if remove_window_state {
                view_state.windows.remove(&window_id);
            }
        }
        drop(client_views);

        if let Some(tab_id) = replacement_tab_id {
            if let Some(tab) = self.get_tab(tab_id) {
                let mut client_views = self.client_views.write();
                for view_state in client_views.values_mut() {
                    if let Some(window_state) = view_state.windows.get_mut(&window_id) {
                        Self::seed_view_state_for_tab(window_state, &tab);
                    }
                }
            }
        }
    }

    pub fn window_containing_tab(&self, tab_id: TabId) -> Option<WindowId> {
        for w in self.windows.read().values() {
            for t in w.iter() {
                if t.tab_id() == tab_id {
                    return Some(w.window_id());
                }
            }
        }
        None
    }

    pub fn is_empty(&self) -> bool {
        self.panes.read().is_empty()
    }

    pub fn is_workspace_empty(&self, workspace: &str) -> bool {
        *self
            .num_panes_by_workspace
            .read()
            .get(workspace)
            .unwrap_or(&0)
            == 0
    }

    pub fn is_active_workspace_empty(&self) -> bool {
        let workspace = self.active_workspace();
        self.is_workspace_empty(&workspace)
    }

    pub fn iter_panes(&self) -> Vec<Arc<dyn Pane>> {
        self.panes
            .read()
            .iter()
            .map(|(_, v)| Arc::clone(v))
            .collect()
    }

    pub fn iter_windows_in_workspace(&self, workspace: &str) -> Vec<WindowId> {
        let mut windows: Vec<WindowId> = self
            .windows
            .read()
            .iter()
            .filter_map(|(k, w)| {
                if w.get_workspace() == workspace {
                    Some(k)
                } else {
                    None
                }
            })
            .cloned()
            .collect();
        windows.sort();
        windows
    }

    pub fn iter_windows(&self) -> Vec<WindowId> {
        self.windows.read().keys().cloned().collect()
    }

    pub fn iter_domains(&self) -> Vec<Arc<dyn Domain>> {
        self.domains.read().values().cloned().collect()
    }

    pub fn resolve_pane_id(&self, pane_id: PaneId) -> Option<(DomainId, WindowId, TabId)> {
        let mut ids = None;
        for tab in self.tabs.read().values() {
            for p in tab.iter_panes_ignoring_zoom() {
                if p.pane.pane_id() == pane_id {
                    ids = Some((tab.tab_id(), p.pane.domain_id()));
                    break;
                }
            }
        }
        let (tab_id, domain_id) = ids?;
        let window_id = self.window_containing_tab(tab_id)?;
        Some((domain_id, window_id, tab_id))
    }

    pub fn domain_was_detached(&self, domain: DomainId) {
        let mut dead_panes = vec![];
        for pane in self.panes.read().values() {
            if pane.domain_id() == domain {
                dead_panes.push(pane.pane_id());
            }
        }

        // Collect tabs while holding the windows lock, then release it
        // before calling into tabs. This avoids a lock-ordering deadlock
        // where windows.write() → tab.inner.lock() conflicts with the
        // GUI render path that may hold tab.inner while waiting for
        // windows or panes. (#7661)
        let tabs: Vec<_> = {
            let windows = self.windows.read();
            windows
                .values()
                .flat_map(|win| win.iter().cloned())
                .collect()
        };
        for tab in &tabs {
            tab.kill_panes_in_domain(domain);
        }

        log::info!("domain detached panes: {:?}", dead_panes);
        for pane_id in dead_panes {
            self.remove_pane_internal(pane_id);
        }

        self.prune_dead_windows();
    }

    pub fn set_banner(&self, banner: Option<String>) {
        *self.banner.write() = banner;
    }

    pub fn resolve_spawn_tab_domain(
        &self,
        // TODO: disambiguate with TabId
        pane_id: Option<PaneId>,
        domain: &config::keyassignment::SpawnTabDomain,
    ) -> anyhow::Result<Arc<dyn Domain>> {
        let domain = match domain {
            SpawnTabDomain::DefaultDomain => self.default_domain(),
            SpawnTabDomain::CurrentPaneDomain => match pane_id {
                Some(pane_id) => {
                    let (pane_domain_id, _window_id, _tab_id) = self
                        .resolve_pane_id(pane_id)
                        .ok_or_else(|| anyhow!("pane_id {} invalid", pane_id))?;
                    self.get_domain(pane_domain_id)
                        .expect("resolve_pane_id to give valid domain_id")
                }
                None => self.default_domain(),
            },
            SpawnTabDomain::DomainId(domain_id) => self
                .get_domain(*domain_id)
                .ok_or_else(|| anyhow!("domain id {} is invalid", domain_id))?,
            SpawnTabDomain::DomainName(name) => {
                self.get_domain_by_name(&name).ok_or_else(|| {
                    let names: Vec<String> = self
                        .domains_by_name
                        .read()
                        .keys()
                        .map(|name| format!("\"{name}\""))
                        .collect();
                    anyhow!(
                        "domain name \"{name}\" is invalid. Possible names are {}.",
                        names.join(", ")
                    )
                })?
            }
        };
        Ok(domain)
    }

    fn resolve_cwd(
        &self,
        command_dir: Option<String>,
        pane: Option<Arc<dyn Pane>>,
        target_domain: DomainId,
        policy: CachePolicy,
    ) -> Option<String> {
        command_dir.or_else(|| {
            match pane {
                Some(pane) if pane.domain_id() == target_domain => pane
                    .get_current_working_dir(policy)
                    .and_then(|url| {
                        percent_decode_str(url.path())
                            .decode_utf8()
                            .ok()
                            .map(|path| path.into_owned())
                    })
                    .map(|path| {
                        // On Windows the file URI can produce a path like:
                        // `/C:\Users` which is valid in a file URI, but the leading slash
                        // is not liked by the windows file APIs, so we strip it off here.
                        let bytes = path.as_bytes();
                        if bytes.len() > 2 && bytes[0] == b'/' && bytes[2] == b':' {
                            path[1..].to_owned()
                        } else {
                            path
                        }
                    }),
                _ => None,
            }
        })
    }

    pub async fn split_pane(
        &self,
        // TODO: disambiguate with TabId
        pane_id: PaneId,
        request: SplitRequest,
        source: SplitSource,
        domain: config::keyassignment::SpawnTabDomain,
    ) -> anyhow::Result<(Arc<dyn Pane>, TerminalSize)> {
        let (_pane_domain_id, window_id, tab_id) = self
            .resolve_pane_id(pane_id)
            .ok_or_else(|| anyhow!("pane_id {} invalid", pane_id))?;

        let domain = self
            .resolve_spawn_tab_domain(Some(pane_id), &domain)
            .context("resolve_spawn_tab_domain")?;

        if domain.state() == DomainState::Detached {
            domain.attach(Some(window_id)).await?;
        }

        let current_pane = self
            .get_pane(pane_id)
            .ok_or_else(|| anyhow!("pane_id {} is invalid", pane_id))?;
        let term_config = current_pane.get_config();

        let source = match source {
            SplitSource::Spawn {
                command,
                command_dir,
            } => SplitSource::Spawn {
                command,
                command_dir: self.resolve_cwd(
                    command_dir,
                    Some(Arc::clone(&current_pane)),
                    domain.domain_id(),
                    CachePolicy::FetchImmediate,
                ),
            },
            other => other,
        };

        let pane = domain.split_pane(source, tab_id, pane_id, request).await?;
        if let Some(config) = term_config {
            pane.set_config(config);
        }

        // Force all panes to match the tree allocation. The split may
        // have changed the tree structure but individual pane PTYs might
        // not have been resized if the resize was suppressed or batched.
        if let Some(tab) = self.get_tab(tab_id) {
            let tab_size = tab.get_size();
            tab.resize(tab_size);
            tab.log_runtime_invariant_errors("mux.split_pane");
        }

        self.set_focused_pane_for_current_identity_lightweight(pane.pane_id())
            .ok();

        // FIXME: clipboard

        let dims = pane.get_dimensions();

        let size = TerminalSize {
            cols: dims.cols,
            rows: dims.viewport_rows,
            pixel_height: 0, // FIXME: split pane pixel dimensions
            pixel_width: 0,
            dpi: dims.dpi,
        };

        Ok((pane, size))
    }

    pub async fn move_pane_to_new_tab(
        &self,
        pane_id: PaneId,
        window_id: Option<WindowId>,
        workspace_for_new_window: Option<String>,
    ) -> anyhow::Result<(Arc<Tab>, WindowId)> {
        let (domain_id, _src_window, src_tab) = self
            .resolve_pane_id(pane_id)
            .ok_or_else(|| anyhow::anyhow!("pane {} not found", pane_id))?;

        let domain = self
            .get_domain(domain_id)
            .ok_or_else(|| anyhow::anyhow!("domain {domain_id} of pane {pane_id} not found"))?;

        if let Some((tab, window_id)) = domain
            .move_pane_to_new_tab(pane_id, window_id, workspace_for_new_window.clone())
            .await?
        {
            return Ok((tab, window_id));
        }

        let src_tab = match self.get_tab(src_tab) {
            Some(t) => t,
            None => anyhow::bail!("Invalid tab id {}", src_tab),
        };

        let window_builder;
        let (window_id, size) = if let Some(window_id) = window_id {
            let _window = self
                .get_window(window_id)
                .ok_or_else(|| anyhow!("window_id {} not found on this server", window_id))?;
            let tab = self
                .get_active_tab_for_window_for_current_identity(window_id)
                .ok_or_else(|| anyhow!("window {} has no active tab for this client", window_id))?;
            let size = tab.get_size();

            (window_id, size)
        } else {
            window_builder = self.new_empty_window(workspace_for_new_window, None);
            (*window_builder, src_tab.get_size())
        };

        let pane = src_tab
            .remove_pane(pane_id)
            .ok_or_else(|| anyhow::anyhow!("pane {} wasn't in its containing tab!?", pane_id))?;

        let tab = Arc::new(Tab::new(&size));
        tab.assign_pane(&pane);
        pane.resize(size)?;
        self.add_tab_and_active_pane(&tab)?;
        self.add_tab_to_window(&tab, window_id)?;
        self.set_focused_pane_for_current_identity_lightweight(pane_id)
            .ok();

        if src_tab.is_dead() {
            self.remove_tab(src_tab.tab_id());
        }

        Ok((tab, window_id))
    }

    pub async fn spawn_tab_or_window(
        &self,
        window_id: Option<WindowId>,
        domain: SpawnTabDomain,
        command: Option<CommandBuilder>,
        command_dir: Option<String>,
        size: TerminalSize,
        current_pane_id: Option<PaneId>,
        workspace_for_new_window: String,
        window_position: Option<GuiPosition>,
    ) -> anyhow::Result<(Arc<Tab>, Arc<dyn Pane>, WindowId)> {
        let domain = self
            .resolve_spawn_tab_domain(current_pane_id, &domain)
            .context("resolve_spawn_tab_domain")?;

        let window_builder;
        let term_config;

        let (window_id, size) = if let Some(window_id) = window_id {
            let _window = self
                .get_window(window_id)
                .ok_or_else(|| anyhow!("window_id {} not found on this server", window_id))?;
            let pane_id = current_pane_id.ok_or_else(|| {
                anyhow!(
                    "existing-window spawn for window {} requires current_pane_id",
                    window_id
                )
            })?;
            let (_, pane_window_id, tab_id) = self
                .resolve_pane_id(pane_id)
                .ok_or_else(|| anyhow!("current_pane_id {} is invalid", pane_id))?;
            anyhow::ensure!(
                pane_window_id == window_id,
                "current_pane_id {} is in window {}, not requested window {}",
                pane_id,
                pane_window_id,
                window_id
            );
            let tab = self
                .get_tab(tab_id)
                .ok_or_else(|| anyhow!("tab {} not found for pane {}", tab_id, pane_id))?;
            let pane = self
                .get_pane(pane_id)
                .ok_or_else(|| anyhow!("pane {} not found", pane_id))?;
            term_config = pane.get_config();

            // Trust the caller's size for existing-window spawns so the new
            // tab inherits the live client dimensions rather than a stale
            // server-side tab size.
            if tab.get_size() != size {
                tab.resize(size);
            }

            (window_id, size)
        } else {
            term_config = None;
            window_builder = self.new_empty_window(Some(workspace_for_new_window), window_position);
            (*window_builder, size)
        };

        if domain.state() == DomainState::Detached {
            domain.attach(Some(window_id)).await?;
        }

        let cwd = self.resolve_cwd(
            command_dir,
            match current_pane_id {
                Some(id) => {
                    // Only use the cwd from the current pane if the domain
                    // is the same as the one we are spawning into
                    let (current_domain_id, _, _) = self
                        .resolve_pane_id(id)
                        .ok_or_else(|| anyhow!("pane_id {} invalid", id))?;
                    if current_domain_id == domain.domain_id() {
                        self.get_pane(id)
                    } else {
                        None
                    }
                }
                None => None,
            },
            domain.domain_id(),
            CachePolicy::FetchImmediate,
        );

        let tab = domain
            .spawn(size, command.clone(), cwd.clone(), window_id)
            .await
            .with_context(|| {
                format!(
                    "Spawning in domain `{}`: {size:?} command={command:?} cwd={cwd:?}",
                    domain.domain_name()
                )
            })?;

        let pane = tab
            .get_active_pane()
            .ok_or_else(|| anyhow!("missing active pane on tab!?"))?;

        if let Some(config) = term_config {
            pane.set_config(config);
        }

        // FIXME: clipboard?

        self.set_active_tab_for_current_identity(window_id, tab.tab_id())
            .ok();

        Ok((tab, pane, window_id))
    }
}

pub struct IdentityHolder {
    prior: Option<Arc<ClientId>>,
}

impl Drop for IdentityHolder {
    fn drop(&mut self) {
        if let Some(mux) = Mux::try_get() {
            mux.replace_identity(self.prior.take());
        }
    }
}

#[derive(Debug, Error)]
#[allow(dead_code)]
pub enum SessionTerminated {
    #[error("Process exited: {:?}", status)]
    ProcessStatus { status: ExitStatus },
    #[error("Error: {:?}", err)]
    Error { err: Error },
    #[error("Window Closed")]
    WindowClosed,
}

pub(crate) fn terminal_size_to_pty_size(size: TerminalSize) -> anyhow::Result<PtySize> {
    Ok(PtySize {
        rows: size.rows.try_into()?,
        cols: size.cols.try_into()?,
        pixel_height: size.pixel_height.try_into()?,
        pixel_width: size.pixel_width.try_into()?,
    })
}

struct MuxClipboard {
    pane_id: PaneId,
}

impl Clipboard for MuxClipboard {
    fn set_contents(
        &self,
        selection: ClipboardSelection,
        clipboard: Option<String>,
    ) -> anyhow::Result<()> {
        let mux =
            Mux::try_get().ok_or_else(|| anyhow::anyhow!("MuxClipboard::set_contents: no Mux?"))?;
        mux.notify(MuxNotification::AssignClipboard {
            pane_id: self.pane_id,
            selection,
            clipboard,
        });
        Ok(())
    }
}

struct MuxDownloader {}

impl wakterm_term::DownloadHandler for MuxDownloader {
    fn save_to_downloads(&self, name: Option<String>, data: Vec<u8>) {
        if let Some(mux) = Mux::try_get() {
            mux.notify(MuxNotification::SaveToDownloads {
                name,
                data: Arc::new(data),
            });
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::agent::AgentMetadata;
    use crate::client::{ClientId, ClientViewId};
    use crate::domain::{alloc_domain_id, Domain, DomainId, DomainState};
    use crate::pane::{alloc_pane_id, CachePolicy, ForEachPaneLogicalLine, Pane, WithPaneLines};
    use crate::renderable::{RenderableDimensions, StableCursorPosition};
    use anyhow::Error;
    use async_trait::async_trait;
    use chrono::{Datelike, TimeZone, Utc};
    use parking_lot::{MappedMutexGuard, Mutex};
    use procinfo::{LocalProcessInfo, LocalProcessStatus};
    use rangeset::RangeSet;
    use std::collections::HashMap;
    use std::ops::Range;
    use std::path::PathBuf;
    use tempfile::TempDir;
    use termwiz::surface::SequenceNo;
    use url::Url;
    use wakterm_term::color::ColorPalette;
    use wakterm_term::{KeyCode, KeyModifiers, Line, MouseEvent, StableRowIndex};

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    #[ignore]
    fn bench_agent_infra_repeated_process_lookups() {
        let pid = std::process::id();
        let started = Instant::now();
        for _ in 0..200 {
            std::hint::black_box(
                procinfo::LocalProcessInfo::with_root_pid_cached(pid, Duration::from_secs(3600))
                    .unwrap(),
            );
        }
        eprintln!("BENCH_PROC_NS={}", started.elapsed().as_nanos());
    }

    #[test]
    #[cfg(target_os = "linux")]
    #[ignore]
    fn bench_tab_resource_status_24_tabs() {
        const TAB_COUNT: usize = 24;
        const WARM_SAMPLES: usize = 100;
        const NAVIGATOR_REFRESHES: usize = 10_000;

        fn process_usage() -> (i64, i64, i64, i64, i64) {
            let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
            assert_eq!(
                unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) },
                0
            );
            let usage = unsafe { usage.assume_init() };
            let cpu_us = (usage.ru_utime.tv_sec + usage.ru_stime.tv_sec) * 1_000_000
                + usage.ru_utime.tv_usec
                + usage.ru_stime.tv_usec;
            (
                cpu_us,
                usage.ru_nvcsw,
                usage.ru_nivcsw,
                usage.ru_minflt,
                usage.ru_majflt,
            )
        }

        fn process_rss_kib() -> u64 {
            let statm = std::fs::read_to_string("/proc/self/statm").unwrap();
            let pages = statm
                .split_whitespace()
                .nth(1)
                .unwrap()
                .parse::<u64>()
                .unwrap();
            let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
            pages * page_size as u64 / 1024
        }

        let size = TerminalSize {
            rows: 73,
            cols: 253,
            pixel_width: 2024,
            pixel_height: 1314,
            dpi: 96,
        };
        let mux = Mux::new(None);
        let domain_id = alloc_domain_id();
        let mut children = Vec::with_capacity(TAB_COUNT);
        let mut tab_ids = Vec::with_capacity(TAB_COUNT);
        for index in 0..TAB_COUNT {
            let child = std::process::Command::new("/usr/bin/sleep")
                .arg("infinity")
                .spawn()
                .unwrap();
            let tab = Arc::new(Tab::new(&size));
            let pane = FakePane::new_with_process_root(
                alloc_pane_id(),
                size,
                domain_id,
                &format!("harness-{}", index + 1),
                child.id(),
            );
            let pane: Arc<dyn Pane> = pane;
            tab.assign_pane(&pane);
            tab_ids.push(tab.tab_id());
            mux.add_tab_and_active_pane(&tab).unwrap();
            children.push(child);
        }

        thread::sleep(Duration::from_millis(310));
        mux.invalidate_tab_resource_status();
        let cold_rss_before = process_rss_kib();
        eprintln!("BENCH_TAB_RESOURCE_COLD_BEGIN");
        let cold_usage_before = process_usage();
        let started = Instant::now();
        let cold = mux.tab_resource_status();
        let cold_ns = started.elapsed().as_nanos();
        let cold_usage_after = process_usage();
        eprintln!("BENCH_TAB_RESOURCE_COLD_END");
        let cold_rss_after = process_rss_kib();
        assert_eq!(cold.tab_rss_bytes.len(), TAB_COUNT);

        eprintln!("BENCH_TAB_RESOURCE_WARM_BEGIN");
        let warm_usage_before = process_usage();
        let started = Instant::now();
        for _ in 0..WARM_SAMPLES {
            mux.invalidate_tab_resource_status();
            std::hint::black_box(mux.tab_resource_status());
        }
        let warm_ns = started.elapsed().as_nanos();
        let warm_usage_after = process_usage();
        eprintln!("BENCH_TAB_RESOURCE_WARM_END");

        eprintln!("BENCH_TAB_RESOURCE_PER_ROW_BEGIN");
        let started = Instant::now();
        for _ in 0..NAVIGATOR_REFRESHES {
            let rss = tab_ids
                .iter()
                .map(|tab_id| mux.approximate_tab_process_rss(*tab_id))
                .collect::<Vec<_>>();
            std::hint::black_box(rss);
        }
        let per_row_ns = started.elapsed().as_nanos();
        eprintln!("BENCH_TAB_RESOURCE_PER_ROW_END");

        eprintln!("BENCH_TAB_RESOURCE_ONE_SNAPSHOT_BEGIN");
        let started = Instant::now();
        for _ in 0..NAVIGATOR_REFRESHES {
            let snapshot = mux.tab_resource_status();
            let rss = tab_ids
                .iter()
                .map(|tab_id| snapshot.tab_rss_bytes.get(tab_id).copied())
                .collect::<Vec<_>>();
            std::hint::black_box(rss);
        }
        let one_snapshot_ns = started.elapsed().as_nanos();
        eprintln!("BENCH_TAB_RESOURCE_ONE_SNAPSHOT_END");

        for child in &mut children {
            let _ = child.kill();
            let _ = child.wait();
        }
        eprintln!(
            "BENCH_TAB_RESOURCE_COLD_NS={cold_ns} WARM_NS={warm_ns} WARM_SAMPLES={WARM_SAMPLES} \
             PER_ROW_NS={per_row_ns} ONE_SNAPSHOT_NS={one_snapshot_ns} \
             NAVIGATOR_REFRESHES={NAVIGATOR_REFRESHES} TABS={TAB_COUNT} \
             COLD_CPU_US={} COLD_VOL_CTX={} COLD_INVOL_CTX={} COLD_MINOR={} COLD_MAJOR={} \
             COLD_RSS_DELTA_KIB={} WARM_CPU_US={} WARM_VOL_CTX={} WARM_INVOL_CTX={} \
             WARM_MINOR={} WARM_MAJOR={} ",
            cold_usage_after.0 - cold_usage_before.0,
            cold_usage_after.1 - cold_usage_before.1,
            cold_usage_after.2 - cold_usage_before.2,
            cold_usage_after.3 - cold_usage_before.3,
            cold_usage_after.4 - cold_usage_before.4,
            cold_rss_after.saturating_sub(cold_rss_before),
            warm_usage_after.0 - warm_usage_before.0,
            warm_usage_after.1 - warm_usage_before.1,
            warm_usage_after.2 - warm_usage_before.2,
            warm_usage_after.3 - warm_usage_before.3,
            warm_usage_after.4 - warm_usage_before.4,
        );
    }

    #[test]
    #[ignore]
    fn bench_agent_infra_duplicate_artifact_bursts() {
        let path = PathBuf::from("/tmp/wakterm-artifact-burst/session.jsonl");
        let started = Instant::now();
        let mut retained = 0usize;
        for _ in 0..100 {
            let pending = Arc::new(Mutex::new(PendingAgentArtifactEvents::default()));
            let (tx, rx) = mpsc::sync_channel::<()>(1);
            for _ in 0..10_000 {
                pending.lock().record(vec![path.clone()]);
                let _ = tx.try_send(());
            }
            rx.recv().unwrap();
            let (paths, refresh_all) = pending.lock().take();
            assert!(!refresh_all);
            retained += paths.len();
        }
        eprintln!(
            "BENCH_BURST_NS={} RETAINED={}",
            started.elapsed().as_nanos(),
            retained
        );
    }

    #[test]
    #[ignore]
    fn bench_agent_infra_confirmed_artifact_routing() {
        let root = Path::new("/tmp/wakterm-artifact-routing");
        let pane_ids = (0..24).map(|_| alloc_pane_id()).collect::<Vec<_>>();
        let mut watcher = artifact_watcher_for_routing_test(root, &pane_ids);
        for (index, pane_id) in pane_ids.iter().enumerate() {
            let session = root.join(format!("session-{index}.jsonl"));
            watcher.set_confirmed_artifact(*pane_id, &AgentHarness::Codex, session.to_str());
        }
        let event = vec![root.join("session-7.jsonl")];
        let started = Instant::now();
        let mut candidates = 0usize;
        for _ in 0..100_000 {
            watcher.last_hint_at_by_pane.clear();
            candidates += watcher.matching_panes(&event).len();
        }
        eprintln!(
            "BENCH_ROUTE_NS={} CANDIDATES={}",
            started.elapsed().as_nanos(),
            candidates
        );
    }

    #[test]
    fn artifact_event_burst_coalesces_duplicate_paths() {
        let path = PathBuf::from("/tmp/wakterm-artifact-burst/session.jsonl");
        let mut pending = PendingAgentArtifactEvents::default();
        for _ in 0..10_000 {
            pending.record(vec![path.clone()]);
        }

        assert_eq!(pending.paths.len(), 1);
        assert!(!pending.refresh_all);
        let (paths, refresh_all) = pending.take();
        assert_eq!(paths, vec![path]);
        assert!(!refresh_all);
        assert!(pending.paths.is_empty());
    }

    #[test]
    fn artifact_event_distinct_path_overflow_is_bounded_and_refreshes_all() {
        let mut pending = PendingAgentArtifactEvents::default();
        pending.record(
            (0..=AGENT_ARTIFACT_DIRTY_PATH_LIMIT)
                .map(|index| PathBuf::from(format!("/tmp/wakterm-artifact-burst/{index}")))
                .collect(),
        );

        assert!(pending.paths.is_empty());
        assert!(pending.refresh_all);
        pending.record(vec![PathBuf::from("/tmp/ignored-after-overflow")]);
        assert!(pending.paths.is_empty());
        let (paths, refresh_all) = pending.take();
        assert!(paths.is_empty());
        assert!(refresh_all);
    }

    fn artifact_watcher_for_routing_test(
        root: &Path,
        pane_ids: &[PaneId],
    ) -> AgentArtifactWatcherState {
        AgentArtifactWatcherState {
            watcher: None,
            roots_by_pane: pane_ids
                .iter()
                .map(|pane_id| (*pane_id, vec![root.to_path_buf()]))
                .collect(),
            panes_by_root: HashMap::from([(
                root.to_path_buf(),
                pane_ids.iter().copied().collect(),
            )]),
            discovery_panes_by_root: HashMap::from([(
                root.to_path_buf(),
                pane_ids.iter().copied().collect(),
            )]),
            artifact_paths_by_pane: HashMap::new(),
            panes_by_artifact_path: HashMap::new(),
            last_hint_at_by_pane: HashMap::new(),
        }
    }

    #[test]
    fn confirmed_artifact_path_routes_one_of_many_panes() {
        let root = Path::new("/tmp/wakterm-artifact-routing");
        let pane_ids = (0..24).map(|_| alloc_pane_id()).collect::<Vec<_>>();
        let mut watcher = artifact_watcher_for_routing_test(root, &pane_ids);
        for (index, pane_id) in pane_ids.iter().enumerate() {
            let session = root.join(format!("session-{index}.jsonl"));
            watcher.set_confirmed_artifact(*pane_id, &AgentHarness::Codex, session.to_str());
        }

        assert_eq!(
            watcher.matching_panes(&[root.join("session-7.jsonl")]),
            vec![pane_ids[7]],
        );
    }

    #[test]
    fn unconfirmed_artifacts_use_broad_discovery_routing() {
        let root = Path::new("/tmp/wakterm-artifact-discovery-routing");
        let confirmed = alloc_pane_id();
        let unconfirmed = alloc_pane_id();
        let mut watcher = artifact_watcher_for_routing_test(root, &[confirmed, unconfirmed]);
        watcher.set_confirmed_artifact(
            confirmed,
            &AgentHarness::Claude,
            root.join("confirmed.jsonl").to_str(),
        );

        assert_eq!(
            watcher.matching_panes(&[root.join("new-session.jsonl")]),
            vec![unconfirmed],
        );

        watcher.set_confirmed_artifact(confirmed, &AgentHarness::Claude, None);
        watcher.last_hint_at_by_pane.clear();
        assert_eq!(
            watcher.matching_panes(&[root.join("new-session.jsonl")]),
            vec![confirmed, unconfirmed],
        );
    }

    #[test]
    fn artifact_directory_event_preserves_prefix_routing() {
        let root = Path::new("/tmp/wakterm-artifact-prefix-routing");
        let confirmed = alloc_pane_id();
        let unconfirmed = alloc_pane_id();
        let mut watcher = artifact_watcher_for_routing_test(root, &[confirmed, unconfirmed]);
        watcher.set_confirmed_artifact(
            confirmed,
            &AgentHarness::Codex,
            root.join("confirmed.jsonl").to_str(),
        );

        assert_eq!(
            watcher.matching_panes(&[root.to_path_buf()]),
            vec![confirmed, unconfirmed],
        );
    }

    #[test]
    fn final_pane_removes_shared_artifact_watch() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let pane_a = alloc_pane_id();
        let pane_b = alloc_pane_id();
        let mut watcher = AgentArtifactWatcherState::new();
        let Some(os_watcher) = watcher.watcher.as_mut() else {
            return;
        };
        os_watcher.watch(&root, RecursiveMode::Recursive).unwrap();
        watcher.roots_by_pane.insert(pane_a, vec![root.clone()]);
        watcher.roots_by_pane.insert(pane_b, vec![root.clone()]);
        watcher
            .panes_by_root
            .insert(root.clone(), HashSet::from([pane_a, pane_b]));

        watcher.unwatch_pane(pane_a);
        assert_eq!(
            watcher.panes_by_root.get(&root),
            Some(&HashSet::from([pane_b]))
        );

        watcher.unwatch_pane(pane_b);
        assert!(!watcher.panes_by_root.contains_key(&root));
        assert!(watcher.discovery_panes_by_root.is_empty());
        assert!(watcher.roots_by_pane.is_empty());
    }

    #[test]
    fn shared_database_artifact_routes_every_confirmed_session() {
        let root = Path::new("/tmp/wakterm-artifact-shared-routing");
        let db_path = root.join("opencode.db");
        let pane_a = alloc_pane_id();
        let pane_b = alloc_pane_id();
        let mut watcher = artifact_watcher_for_routing_test(root, &[pane_a, pane_b]);
        for (pane_id, session_id) in [(pane_a, "session-a"), (pane_b, "session-b")] {
            watcher.set_confirmed_artifact(
                pane_id,
                &AgentHarness::Opencode,
                Some(
                    format!(
                        "opencode://session?db={}&id={session_id}",
                        db_path.display()
                    )
                    .as_str(),
                ),
            );
        }

        assert_eq!(
            watcher.matching_panes(&[PathBuf::from(format!("{}-wal", db_path.display()))]),
            vec![pane_a, pane_b]
        );
    }

    #[test]
    fn tab_resource_status_reuses_one_sample_until_invalidated() {
        let mux = Mux::new(None);
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 640,
            pixel_height: 480,
            dpi: 96,
        };
        let tab = Arc::new(Tab::new(&size));
        let (pane, calls) = FakePane::new_detected_counted(
            alloc_pane_id(),
            size,
            alloc_domain_id(),
            "status-cache",
            "/tmp/status-cache",
            "codex",
            &["codex"],
        );
        let pane: Arc<dyn Pane> = pane;
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab).unwrap();

        let first = mux.tab_resource_status();
        let second = mux.tab_resource_status();
        assert_eq!(first, second);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        mux.invalidate_tab_resource_status();
        let third = mux.tab_resource_status();
        assert!(third.sampled_at_ms >= second.sampled_at_ms);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    struct FakePane {
        id: PaneId,
        size: Mutex<TerminalSize>,
        domain_id: DomainId,
        title: String,
        cwd: Option<Url>,
        foreground_process_name: Option<String>,
        foreground_process_info: Option<LocalProcessInfo>,
        #[cfg(target_os = "linux")]
        foreground_process_root_pid: Option<u32>,
        foreground_process_info_calls: Option<Arc<AtomicUsize>>,
    }

    impl FakePane {
        fn test_file_url(path: &str) -> Url {
            Url::parse(&format!("file://test-host{path}")).unwrap()
        }

        fn new(id: PaneId, size: TerminalSize, domain_id: DomainId) -> Arc<dyn Pane> {
            Arc::new(Self {
                id,
                size: Mutex::new(size),
                domain_id,
                title: String::new(),
                cwd: None,
                foreground_process_name: None,
                foreground_process_info: None,
                #[cfg(target_os = "linux")]
                foreground_process_root_pid: None,
                foreground_process_info_calls: None,
            })
        }

        fn new_detected(
            id: PaneId,
            size: TerminalSize,
            domain_id: DomainId,
            title: &str,
            cwd: &str,
            foreground_process_name: &str,
            argv: &[&str],
        ) -> Arc<dyn Pane> {
            Arc::new(Self {
                id,
                size: Mutex::new(size),
                domain_id,
                title: title.to_string(),
                cwd: Some(Self::test_file_url(cwd)),
                foreground_process_name: Some(foreground_process_name.to_string()),
                foreground_process_info: Some(LocalProcessInfo {
                    pid: 1,
                    ppid: 0,
                    name: PathBuf::from(foreground_process_name)
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or(foreground_process_name)
                        .to_string(),
                    executable: PathBuf::from(foreground_process_name),
                    argv: argv.iter().map(|arg| (*arg).to_string()).collect(),
                    cwd: PathBuf::from(cwd),
                    status: LocalProcessStatus::Run,
                    start_time: 1,
                    #[cfg(windows)]
                    console: 0,
                    children: HashMap::new(),
                }),
                #[cfg(target_os = "linux")]
                foreground_process_root_pid: None,
                foreground_process_info_calls: None,
            })
        }

        fn new_detected_counted(
            id: PaneId,
            size: TerminalSize,
            domain_id: DomainId,
            title: &str,
            cwd: &str,
            foreground_process_name: &str,
            argv: &[&str],
        ) -> (Arc<Self>, Arc<AtomicUsize>) {
            let foreground_process_info_calls = Arc::new(AtomicUsize::new(0));
            let pane = Arc::new(Self {
                id,
                size: Mutex::new(size),
                domain_id,
                title: title.to_string(),
                cwd: Some(Self::test_file_url(cwd)),
                foreground_process_name: Some(foreground_process_name.to_string()),
                foreground_process_info: Some(LocalProcessInfo {
                    pid: 1,
                    ppid: 0,
                    name: PathBuf::from(foreground_process_name)
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or(foreground_process_name)
                        .to_string(),
                    executable: PathBuf::from(foreground_process_name),
                    argv: argv.iter().map(|arg| (*arg).to_string()).collect(),
                    cwd: PathBuf::from(cwd),
                    status: LocalProcessStatus::Run,
                    start_time: 1,
                    #[cfg(windows)]
                    console: 0,
                    children: HashMap::new(),
                }),
                #[cfg(target_os = "linux")]
                foreground_process_root_pid: None,
                foreground_process_info_calls: Some(foreground_process_info_calls.clone()),
            });
            (pane, foreground_process_info_calls)
        }

        fn new_title_only(
            id: PaneId,
            size: TerminalSize,
            domain_id: DomainId,
            title: &str,
            cwd: &str,
        ) -> Arc<Self> {
            Arc::new(Self {
                id,
                size: Mutex::new(size),
                domain_id,
                title: title.to_string(),
                cwd: Some(Self::test_file_url(cwd)),
                foreground_process_name: None,
                foreground_process_info: None,
                #[cfg(target_os = "linux")]
                foreground_process_root_pid: None,
                foreground_process_info_calls: None,
            })
        }

        fn new_detected_with_url(
            id: PaneId,
            size: TerminalSize,
            domain_id: DomainId,
            title: &str,
            cwd_url: &str,
            foreground_process_name: &str,
            argv: &[&str],
        ) -> Arc<dyn Pane> {
            let cwd_path = Url::parse(cwd_url)
                .ok()
                .map(|url| url.path().to_string())
                .unwrap_or_default();
            Arc::new(Self {
                id,
                size: Mutex::new(size),
                domain_id,
                title: title.to_string(),
                cwd: Some(Url::parse(cwd_url).unwrap()),
                foreground_process_name: Some(foreground_process_name.to_string()),
                foreground_process_info: Some(LocalProcessInfo {
                    pid: 1,
                    ppid: 0,
                    name: PathBuf::from(foreground_process_name)
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or(foreground_process_name)
                        .to_string(),
                    executable: PathBuf::from(foreground_process_name),
                    argv: argv.iter().map(|arg| (*arg).to_string()).collect(),
                    cwd: PathBuf::from(cwd_path),
                    status: LocalProcessStatus::Run,
                    start_time: 1,
                    #[cfg(windows)]
                    console: 0,
                    children: HashMap::new(),
                }),
                #[cfg(target_os = "linux")]
                foreground_process_root_pid: None,
                foreground_process_info_calls: None,
            })
        }

        #[cfg(target_os = "linux")]
        fn new_with_process_root(
            id: PaneId,
            size: TerminalSize,
            domain_id: DomainId,
            title: &str,
            root_pid: u32,
        ) -> Arc<Self> {
            Arc::new(Self {
                id,
                size: Mutex::new(size),
                domain_id,
                title: title.to_string(),
                cwd: None,
                foreground_process_name: Some("sleep".to_string()),
                foreground_process_info: None,
                foreground_process_root_pid: Some(root_pid),
                foreground_process_info_calls: None,
            })
        }
    }

    impl Pane for FakePane {
        fn pane_id(&self) -> PaneId {
            self.id
        }

        fn get_cursor_position(&self) -> StableCursorPosition {
            unimplemented!();
        }

        fn get_current_seqno(&self) -> SequenceNo {
            unimplemented!();
        }

        fn get_changed_since(
            &self,
            _lines: Range<StableRowIndex>,
            _seqno: SequenceNo,
        ) -> RangeSet<StableRowIndex> {
            unimplemented!();
        }

        fn get_lines(&self, _lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
            unimplemented!();
        }

        fn with_lines_mut(
            &self,
            _lines: Range<StableRowIndex>,
            _with_lines: &mut dyn WithPaneLines,
        ) {
            unimplemented!();
        }

        fn for_each_logical_line_in_stable_range_mut(
            &self,
            _lines: Range<StableRowIndex>,
            _for_line: &mut dyn ForEachPaneLogicalLine,
        ) {
            unimplemented!();
        }

        fn get_logical_lines(
            &self,
            _lines: Range<StableRowIndex>,
        ) -> Vec<crate::pane::LogicalLine> {
            unimplemented!();
        }

        fn get_dimensions(&self) -> RenderableDimensions {
            let size = self.size.lock();
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
            self.title.clone()
        }

        fn send_paste(&self, _text: &str) -> anyhow::Result<()> {
            Ok(())
        }

        fn reader(&self) -> anyhow::Result<Option<Box<dyn std::io::Read + Send>>> {
            Ok(None)
        }

        fn writer(&self) -> MappedMutexGuard<'_, dyn std::io::Write> {
            unimplemented!();
        }

        fn resize(&self, size: TerminalSize) -> anyhow::Result<()> {
            *self.size.lock() = size;
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
            unimplemented!()
        }

        fn domain_id(&self) -> DomainId {
            self.domain_id
        }

        fn is_mouse_grabbed(&self) -> bool {
            false
        }

        fn is_alt_screen_active(&self) -> bool {
            false
        }

        fn get_current_working_dir(&self, _policy: CachePolicy) -> Option<Url> {
            self.cwd.clone()
        }

        fn get_foreground_process_name(&self, _policy: CachePolicy) -> Option<String> {
            self.foreground_process_name.clone()
        }

        fn get_foreground_process_info(&self, _policy: CachePolicy) -> Option<LocalProcessInfo> {
            if let Some(calls) = &self.foreground_process_info_calls {
                calls.fetch_add(1, Ordering::SeqCst);
            }
            #[cfg(target_os = "linux")]
            if let Some(pid) = self.foreground_process_root_pid {
                return LocalProcessInfo::with_root_pid_cached(pid, Duration::from_millis(300));
            }
            self.foreground_process_info.clone()
        }
    }

    struct FakeDomain {
        id: DomainId,
        last_spawn_size: Mutex<Option<TerminalSize>>,
    }

    impl FakeDomain {
        fn new() -> Self {
            Self {
                id: alloc_domain_id(),
                last_spawn_size: Mutex::new(None),
            }
        }
    }

    #[async_trait(?Send)]
    impl Domain for FakeDomain {
        async fn spawn_pane(
            &self,
            size: TerminalSize,
            _command: Option<CommandBuilder>,
            _command_dir: Option<String>,
        ) -> anyhow::Result<Arc<dyn Pane>> {
            self.last_spawn_size.lock().replace(size);
            Ok(FakePane::new(alloc_pane_id(), size, self.id))
        }

        fn detachable(&self) -> bool {
            false
        }

        fn domain_id(&self) -> DomainId {
            self.id
        }

        fn domain_name(&self) -> &str {
            "fake"
        }

        async fn attach(&self, _window_id: Option<WindowId>) -> anyhow::Result<()> {
            Ok(())
        }

        fn detach(&self) -> Result<(), Error> {
            Ok(())
        }

        fn state(&self) -> DomainState {
            DomainState::Attached
        }
    }

    fn register_test_client(mux: &Arc<Mux>, view_name: &str) -> (Arc<ClientId>, Arc<ClientViewId>) {
        let client_id = Arc::new(ClientId::new());
        let view_id = Arc::new(ClientViewId(view_name.to_string()));
        mux.register_client(client_id.clone(), view_id.clone());
        (client_id, view_id)
    }

    #[test]
    fn notification_subscriber_is_removed_after_first_false_result() {
        let mux = Mux::new(None);
        let calls = Arc::new(AtomicUsize::new(0));
        let subscriber_calls = Arc::clone(&calls);
        mux.subscribe(move |_| {
            subscriber_calls.fetch_add(1, Ordering::SeqCst);
            false
        });

        mux.notify(MuxNotification::Empty);
        mux.notify(MuxNotification::Empty);

        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn register_client_bootstraps_existing_session_view_state() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let size = TerminalSize {
            rows: 40,
            cols: 120,
            pixel_width: 1200,
            pixel_height: 800,
            dpi: 96,
        };

        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);

        let tab_a = Arc::new(Tab::new(&size));
        let pane_a = FakePane::new(10, size, domain.id);
        tab_a.assign_pane(&pane_a);
        mux.add_tab_and_active_pane(&tab_a).unwrap();
        mux.add_tab_to_window(&tab_a, window_id).unwrap();

        let tab_b = Arc::new(Tab::new(&size));
        let pane_b = FakePane::new(11, size, domain.id);
        tab_b.assign_pane(&pane_b);
        mux.add_tab_and_active_pane(&tab_b).unwrap();
        mux.add_tab_to_window(&tab_b, window_id).unwrap();

        let (client_id, view_id) = register_test_client(&mux, "bootstrap-view");

        assert_eq!(
            mux.active_workspace_for_client(&client_id),
            DEFAULT_WORKSPACE.to_string()
        );
        assert_eq!(
            mux.get_active_tab_for_window_for_client(view_id.as_ref(), window_id)
                .map(|tab| tab.tab_id()),
            Some(tab_a.tab_id())
        );
        assert_eq!(
            mux.get_active_pane_id_for_tab_for_client(view_id.as_ref(), window_id, tab_a.tab_id()),
            Some(pane_a.pane_id())
        );
        assert_eq!(
            mux.iter_clients()
                .into_iter()
                .find(|info| info.client_id.as_ref() == client_id.as_ref())
                .and_then(|info| info.focused_pane_id),
            Some(pane_a.pane_id())
        );
        assert_eq!(
            mux.get_active_pane_id_for_tab_for_client(view_id.as_ref(), window_id, tab_b.tab_id()),
            Some(pane_b.pane_id())
        );
    }

    #[test]
    fn register_client_uses_first_non_empty_workspace_when_default_is_empty() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };

        let window_id = *mux.new_empty_window(Some("alt".to_string()), None);
        let tab = Arc::new(Tab::new(&size));
        let pane = FakePane::new(20, size, domain.id);
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();

        let (client_id, view_id) = register_test_client(&mux, "non-empty-workspace");

        assert_eq!(
            mux.active_workspace_for_client(&client_id),
            "alt".to_string()
        );
        assert_eq!(
            mux.get_active_tab_for_window_for_client(view_id.as_ref(), window_id)
                .map(|tab| tab.tab_id()),
            Some(tab.tab_id())
        );
        assert_eq!(
            mux.iter_clients()
                .into_iter()
                .find(|info| info.client_id.as_ref() == client_id.as_ref())
                .and_then(|info| info.focused_pane_id),
            Some(pane.pane_id())
        );
    }

    #[test]
    fn reconnecting_persistent_view_preserves_existing_choices_and_bootstraps_new_windows() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let size = TerminalSize {
            rows: 30,
            cols: 100,
            pixel_width: 1000,
            pixel_height: 600,
            dpi: 96,
        };

        let window_a = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
        let tab_a = Arc::new(Tab::new(&size));
        let pane_a = FakePane::new(30, size, domain.id);
        tab_a.assign_pane(&pane_a);
        mux.add_tab_and_active_pane(&tab_a).unwrap();
        mux.add_tab_to_window(&tab_a, window_a).unwrap();

        let tab_b = Arc::new(Tab::new(&size));
        let pane_b = FakePane::new(31, size, domain.id);
        tab_b.assign_pane(&pane_b);
        mux.add_tab_and_active_pane(&tab_b).unwrap();
        mux.add_tab_to_window(&tab_b, window_a).unwrap();

        let client_a = Arc::new(ClientId::new());
        let view_id = Arc::new(ClientViewId("persistent-view".to_string()));
        mux.register_client(client_a.clone(), view_id.clone());
        mux.set_active_tab_for_client_view(view_id.as_ref(), window_a, tab_b.tab_id())
            .unwrap();
        mux.set_active_pane_for_client_view(
            view_id.as_ref(),
            window_a,
            tab_b.tab_id(),
            pane_b.pane_id(),
        )
        .unwrap();
        mux.set_focused_pane_for_client(client_a.as_ref(), pane_b.pane_id())
            .unwrap();
        mux.unregister_client(client_a.as_ref());

        let window_b = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
        let tab_c = Arc::new(Tab::new(&size));
        let pane_c = FakePane::new(32, size, domain.id);
        tab_c.assign_pane(&pane_c);
        mux.add_tab_and_active_pane(&tab_c).unwrap();
        mux.add_tab_to_window(&tab_c, window_b).unwrap();

        let client_b = Arc::new(ClientId::new());
        mux.register_client(client_b.clone(), view_id.clone());

        assert_eq!(
            mux.get_active_tab_for_window_for_client(view_id.as_ref(), window_a)
                .map(|tab| tab.tab_id()),
            Some(tab_b.tab_id())
        );
        assert_eq!(
            mux.get_active_pane_id_for_tab_for_client(view_id.as_ref(), window_a, tab_b.tab_id()),
            Some(pane_b.pane_id())
        );
        assert_eq!(
            mux.get_active_tab_for_window_for_client(view_id.as_ref(), window_b)
                .map(|tab| tab.tab_id()),
            Some(tab_c.tab_id())
        );
        assert_eq!(
            mux.get_active_pane_id_for_tab_for_client(view_id.as_ref(), window_b, tab_c.tab_id()),
            Some(pane_c.pane_id())
        );
        assert_eq!(
            mux.iter_clients()
                .into_iter()
                .find(|info| info.client_id.as_ref() == client_b.as_ref())
                .and_then(|info| info.focused_pane_id),
            Some(pane_b.pane_id())
        );
    }

    fn sample_agent_metadata(name: &str) -> AgentMetadata {
        AgentMetadata {
            agent_id: format!("agent-{name}"),
            name: name.to_string(),
            launch_cmd: "codex".to_string(),
            declared_cwd: format!("file:///tmp/{name}"),
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

    struct TestConfigGuard;

    impl TestConfigGuard {
        fn new(mode: &str, badge: &str) -> Self {
            Self::new_with_auto_adopt(mode, badge, false)
        }

        fn new_with_auto_adopt(mode: &str, badge: &str, auto_adopt: bool) -> Self {
            let mut config = config::Config::default();
            config.agent_tab_badge_mode = mode.to_string();
            config.agent_tab_badge = badge.to_string();
            config.agent_auto_adopt_on_confirmed_session_match = auto_adopt;
            config::use_this_configuration(config);
            Self
        }
    }

    impl Drop for TestConfigGuard {
        fn drop(&mut self) {
            config::use_test_configuration();
        }
    }

    fn wait_for_main_thread_work<F>(
        executor: &promise::spawn::SimpleExecutor,
        mut ready: F,
        context: &str,
    ) where
        F: FnMut() -> bool,
    {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !ready() {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {}",
                context
            );
            executor.tick().expect("run queued main-thread work");
        }
    }

    #[test]
    fn agent_metadata_is_listed_and_cleared_when_pane_is_removed() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };

        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
        let tab = Arc::new(Tab::new(&size));
        let pane = FakePane::new(40, size, domain.id);
        let pane_id = pane.pane_id();
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();

        mux.set_agent_metadata(pane_id, sample_agent_metadata("alpha"))
            .unwrap();

        let agents = mux.list_agents();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].metadata.name, "alpha");
        assert_eq!(agents[0].pane_id, pane_id);
        assert_eq!(agents[0].tab_id, tab.tab_id());
        assert_eq!(agents[0].window_id, window_id);
        assert_eq!(agents[0].workspace, DEFAULT_WORKSPACE);

        mux.remove_pane(pane_id);
        assert!(mux.list_agents().is_empty());
        assert!(mux.get_agent_metadata_for_pane(pane_id).is_none());
    }

    #[test]
    fn adopted_agent_is_cleared_when_harness_exits_back_to_shell() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };

        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
        let tab = Arc::new(Tab::new(&size));
        let pane = FakePane::new_detected(
            41,
            size,
            domain.id,
            "claude",
            "/tmp/claude-exit",
            "/usr/bin/claude",
            &["claude"],
        );
        let pane_id = pane.pane_id();
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();
        mux.set_agent_metadata(
            pane_id,
            AgentMetadata {
                launch_cmd: "claude".to_string(),
                declared_cwd: "/tmp/claude-exit".to_string(),
                ..sample_agent_metadata("claude-exit")
            },
        )
        .unwrap();

        assert_eq!(mux.list_agents().len(), 1);

        let shell_pane: Arc<dyn Pane> = Arc::new(FakePane {
            id: pane_id,
            size: Mutex::new(size),
            domain_id: domain.id,
            title: "zsh".to_string(),
            cwd: Some(FakePane::test_file_url("/tmp/claude-exit")),
            foreground_process_name: Some("/usr/bin/zsh".to_string()),
            foreground_process_info: Some(LocalProcessInfo {
                pid: 2,
                ppid: 0,
                name: "zsh".to_string(),
                executable: PathBuf::from("/usr/bin/zsh"),
                argv: vec!["zsh".to_string()],
                cwd: PathBuf::from("/tmp/claude-exit"),
                status: LocalProcessStatus::Run,
                start_time: 1,
                #[cfg(windows)]
                console: 0,
                children: HashMap::new(),
            }),
            #[cfg(target_os = "linux")]
            foreground_process_root_pid: None,
            foreground_process_info_calls: None,
        });
        mux.panes.write().insert(pane_id, shell_pane);

        assert!(mux.list_agents().is_empty());
        assert!(mux.get_agent_metadata_for_pane(pane_id).is_none());
    }

    #[test]
    fn pane_output_replaces_stale_agent_folder_title_after_harness_restart() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };

        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
        let tab = Arc::new(Tab::new(&size));
        let personal_pane = FakePane::new(42, size, domain.id);
        let transcribe_pane = FakePane::new_detected(
            43,
            size,
            domain.id,
            "codex",
            "/code/transcribe",
            "/usr/bin/codex",
            &["codex"],
        );
        let replaced_pane_id = transcribe_pane.pane_id();
        tab.assign_pane(&personal_pane);
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: crate::tab::SplitDirection::Horizontal,
                target_is_second: true,
                size: crate::tab::SplitSize::Percent(50),
                top_level: false,
            },
            transcribe_pane.clone(),
        )
        .unwrap();
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_pane(&transcribe_pane).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();

        let mut personal = sample_agent_metadata("personal_codex");
        personal.declared_cwd = "/code/personal".to_string();
        mux.set_mirrored_agent_metadata(personal_pane.pane_id(), Some(&personal));
        let mut transcribe = sample_agent_metadata("transcribe_codex");
        transcribe.declared_cwd = "/code/transcribe".to_string();
        mux.set_agent_metadata(replaced_pane_id, transcribe)
            .unwrap();

        assert_eq!(
            mux.effective_tab_titles_for_window(window_id)
                .get(&tab.tab_id())
                .map(String::as_str),
            Some("personal+transcribe")
        );

        let (mut replacement, _) = FakePane::new_detected_counted(
            replaced_pane_id,
            size,
            domain.id,
            "codex",
            "/code/personal",
            "/usr/bin/codex",
            &["codex"],
        );
        let process = Arc::get_mut(&mut replacement)
            .expect("replacement pane is uniquely owned")
            .foreground_process_info
            .as_mut()
            .expect("replacement process info");
        process.pid = 2;
        process.start_time = 2;
        mux.panes
            .write()
            .insert(replaced_pane_id, replacement as Arc<dyn Pane>);

        mux.record_agent_output(replaced_pane_id);

        assert!(mux.get_agent_metadata_for_pane(replaced_pane_id).is_none());
        assert!(mux.detected_agent_panes.read().contains(&replaced_pane_id));
        assert_eq!(
            mux.effective_tab_titles_for_window(window_id)
                .get(&tab.tab_id())
                .map(String::as_str),
            Some("personal")
        );
    }

    #[test]
    fn agent_metadata_changes_notify_titles_and_exact_metadata() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };

        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
        let tab = Arc::new(Tab::new(&size));
        let pane = FakePane::new(40, size, domain.id);
        let pane_id = pane.pane_id();
        let tab_id = tab.tab_id();
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();

        let title_changes = std::sync::Arc::new(Mutex::new(0usize));
        let title_changes_for_sub = std::sync::Arc::clone(&title_changes);
        let metadata_changes = std::sync::Arc::new(Mutex::new(Vec::new()));
        let metadata_changes_for_sub = std::sync::Arc::clone(&metadata_changes);
        mux.subscribe(move |notification| {
            if matches!(notification, MuxNotification::TabTitleChanged { tab_id: changed, .. } if changed == tab_id) {
                *title_changes_for_sub.lock() += 1;
            }
            if let MuxNotification::AgentMetadataChanged {
                pane_id: changed,
                metadata,
            } = notification
            {
                if changed == pane_id {
                    metadata_changes_for_sub.lock().push(metadata.clone());
                }
            }
            true
        });

        mux.set_agent_metadata(pane_id, sample_agent_metadata("alpha"))
            .unwrap();
        mux.clear_agent_metadata(pane_id);

        assert_eq!(*title_changes.lock(), 2);
        let metadata_changes = metadata_changes.lock();
        assert_eq!(metadata_changes.len(), 2);
        assert_eq!(
            metadata_changes[0]
                .as_ref()
                .map(|metadata| metadata.name.as_str()),
            Some("alpha")
        );
        assert!(metadata_changes[1].is_none());
    }

    #[test]
    fn detects_opencode_from_title_with_hosted_file_cwd() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let mut config = config::Config::default();
        config.agent_auto_adopt_on_confirmed_session_match = true;
        config::use_this_configuration(config);

        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };

        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
        let tab = Arc::new(Tab::new(&size));
        let pane = FakePane::new_detected_with_url(
            40,
            size,
            domain.id,
            "OC | Casual Greeting",
            "file://fedora/home/mihai",
            "opencode",
            &["opencode"],
        );
        let pane_id = pane.pane_id();
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();

        let agents = mux.list_agents();
        assert_eq!(agents.len(), 1);
        assert!(matches!(agents[0].origin, AgentOrigin::Detected));
        assert_eq!(agents[0].pane_id, pane_id);
        assert_eq!(
            agents[0].runtime.harness,
            crate::agent::AgentHarness::Opencode
        );
        assert_eq!(agents[0].metadata.declared_cwd, "/home/mihai");
        assert_eq!(agents[0].detection_source.as_deref(), Some("proc+title"));
    }

    #[test]
    fn detects_agy_from_process_and_title() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };

        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
        let tab = Arc::new(Tab::new(&size));
        let pane = FakePane::new_detected_with_url(
            41,
            size,
            domain.id,
            "agy",
            "file://fedora/code/wakterm",
            "agy",
            &["agy", "--dangerously-skip-permissions"],
        );
        let pane_id = pane.pane_id();
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();

        let agents = mux.list_agents();
        assert_eq!(agents.len(), 1);
        assert!(matches!(agents[0].origin, AgentOrigin::Detected));
        assert_eq!(agents[0].pane_id, pane_id);
        assert_eq!(agents[0].runtime.harness, AgentHarness::Agy);
        assert_eq!(agents[0].metadata.declared_cwd, "/code/wakterm");
        assert_eq!(
            agents[0].metadata.launch_cmd,
            "agy --dangerously-skip-permissions"
        );
        assert_eq!(agents[0].detection_source.as_deref(), Some("proc+title"));
    }

    #[test]
    fn detected_agent_is_cleared_when_pane_is_removed() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };

        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
        let tab = Arc::new(Tab::new(&size));
        let pane = FakePane::new_detected(
            150,
            size,
            domain.id,
            "codex",
            "/tmp/detected-remove",
            "/usr/bin/codex",
            &["codex"],
        );
        let pane_id = pane.pane_id();
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();

        let agents = mux.list_agents();
        assert_eq!(agents.len(), 1);
        assert!(matches!(agents[0].origin, AgentOrigin::Detected));
        assert!(mux.detected_agent_panes.read().contains(&pane_id));

        mux.remove_pane(pane_id);

        assert!(mux.list_agents().is_empty());
        assert!(mux.get_agent_metadata_for_pane(pane_id).is_none());
        assert!(mux.agent_runtime_by_pane.read().get(&pane_id).is_none());
        assert!(mux.detected_agent_panes.read().get(&pane_id).is_none());
        assert!(mux
            .agent_observer_state_by_pane
            .read()
            .get(&pane_id)
            .is_none());
    }

    #[test]
    fn auto_adopted_agent_is_cleared_when_pane_is_removed() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let mut config = config::Config::default();
        config.agent_auto_adopt_on_confirmed_session_match = true;
        config::use_this_configuration(config);

        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };

        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
        let tab = Arc::new(Tab::new(&size));
        let pane = FakePane::new_detected(
            151,
            size,
            domain.id,
            "codex",
            "/tmp/adopted-remove",
            "/usr/bin/codex",
            &["codex"],
        );
        let pane_id = pane.pane_id();
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();

        mux.detected_agent_panes.write().insert(pane_id);
        mux.agent_runtime_by_pane.write().insert(
            pane_id,
            AgentRuntimeSnapshot {
                harness: crate::agent::AgentHarness::Codex,
                transport: crate::agent::AgentTransport::ObservedPty,
                status: crate::agent::AgentStatus::Starting,
                turn_state: crate::agent::AgentTurnState::WaitingOnUser,
                alive: true,
                foreground_process_name: Some("/usr/bin/codex".to_string()),
                tty_name: Some("/dev/pts/1".to_string()),
                last_input_at: None,
                last_output_at: None,
                last_progress_at: None,
                last_turn_completed_at: Some(Utc.with_ymd_and_hms(2026, 3, 29, 18, 39, 0).unwrap()),
                observed_turn: None,
                observed_at: Utc.with_ymd_and_hms(2026, 3, 29, 18, 39, 8).unwrap(),
                session_path: Some("/tmp/codex-session.jsonl".to_string()),
                progress_summary: Some("done".to_string()),
                harness_mode: Some("default".to_string()),
                turn_phase: Some("final_answer".to_string()),
                attention_reason: None,
                terminal_progress: wakterm_term::Progress::None,
                observer_error: None,
                observer_started_at: None,
                last_harness_refresh_at: None,
            },
        );

        let agents = mux.list_agents();
        assert_eq!(agents.len(), 1);
        assert!(matches!(agents[0].origin, AgentOrigin::Adopted));
        assert!(mux.get_agent_metadata_for_pane(pane_id).is_some());

        mux.remove_pane(pane_id);

        assert!(mux.list_agents().is_empty());
        assert!(mux.get_agent_metadata_for_pane(pane_id).is_none());
        assert!(mux.agent_runtime_by_pane.read().get(&pane_id).is_none());
        assert!(mux.detected_agent_panes.read().get(&pane_id).is_none());
        assert!(mux
            .agent_observer_state_by_pane
            .read()
            .get(&pane_id)
            .is_none());
    }

    #[test]
    fn agent_runtime_tracks_prompt_submission_and_output_activity() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };

        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
        let tab = Arc::new(Tab::new(&size));
        let pane = FakePane::new(41, size, domain.id);
        let pane_id = pane.pane_id();
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();
        mux.set_agent_metadata(pane_id, sample_agent_metadata("tracker"))
            .unwrap();

        assert_eq!(mux.agent_input_generation(pane_id), 0);
        mux.record_agent_prompt_submission(pane_id);
        assert_eq!(mux.agent_input_generation(pane_id), 1);
        mux.notify(MuxNotification::PaneOutput(pane_id));

        let agents = mux.list_agents();
        assert_eq!(agents.len(), 1);
        let runtime = &agents[0].runtime;
        assert_eq!(runtime.harness, crate::agent::AgentHarness::Codex);
        assert_eq!(runtime.status, crate::agent::AgentStatus::Busy);
        assert!(runtime.alive);
        assert!(runtime.last_input_at.is_some());
        assert!(runtime.last_output_at.is_some());
        assert_eq!(runtime.foreground_process_name, None);
    }

    #[test]
    fn repeated_output_without_badge_change_does_not_emit_tab_title_change() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };

        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
        let tab = Arc::new(Tab::new(&size));
        let pane = FakePane::new(142, size, domain.id);
        let pane_id = pane.pane_id();
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();
        mux.set_agent_metadata(pane_id, sample_agent_metadata("quiet"))
            .unwrap();

        let title_changes = std::sync::Arc::new(Mutex::new(0usize));
        let title_changes_for_sub = std::sync::Arc::clone(&title_changes);
        mux.subscribe(move |notification| {
            if matches!(notification, MuxNotification::TabTitleChanged { .. }) {
                *title_changes_for_sub.lock() += 1;
            }
            true
        });

        mux.notify(MuxNotification::PaneOutput(pane_id));

        assert_eq!(*title_changes.lock(), 0);
    }

    #[test]
    fn pane_output_detects_agent_without_list_command() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };

        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
        let tab = Arc::new(Tab::new(&size));
        let pane = FakePane::new_detected(
            144,
            size,
            domain.id,
            "codex",
            "/tmp/pane-output-detect",
            "/usr/bin/codex",
            &["codex"],
        );
        let pane_id = pane.pane_id();
        let tab_id = tab.tab_id();
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();

        let title_changes = std::sync::Arc::new(Mutex::new(0usize));
        let title_changes_for_sub = std::sync::Arc::clone(&title_changes);
        mux.subscribe(move |notification| {
            if matches!(notification, MuxNotification::TabTitleChanged { tab_id: changed, .. } if changed == tab_id)
            {
                *title_changes_for_sub.lock() += 1;
            }
            true
        });

        mux.notify(MuxNotification::PaneOutput(pane_id));

        assert!(mux.detected_agent_panes.read().contains(&pane_id));
        assert!(mux.agent_runtime_by_pane.read().get(&pane_id).is_some());
        assert_eq!(*title_changes.lock(), 1);
    }

    #[test]
    fn observer_refresh_notifies_tab_title_changed_when_session_attach_changes_icon_state() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };

        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
        let tab = Arc::new(Tab::new(&size));
        let pane = FakePane::new(143, size, domain.id);
        let pane_id = pane.pane_id();
        let tab_id = tab.tab_id();
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();
        mux.set_agent_metadata(pane_id, sample_agent_metadata("refresh"))
            .unwrap();

        let title_changes = std::sync::Arc::new(Mutex::new(0usize));
        let title_changes_for_sub = std::sync::Arc::clone(&title_changes);
        mux.subscribe(move |notification| {
            if matches!(notification, MuxNotification::TabTitleChanged { tab_id: changed, .. } if changed == tab_id)
            {
                *title_changes_for_sub.lock() += 1;
            }
            true
        });

        *title_changes.lock() = 0;

        mux.apply_agent_observer_update(AgentObserverUpdate {
            pane_id,
            generation: 1,
            runtime: AgentRuntimeSnapshot {
                harness: crate::agent::AgentHarness::Codex,
                transport: crate::agent::AgentTransport::ObservedPty,
                status: crate::agent::AgentStatus::Starting,
                turn_state: crate::agent::AgentTurnState::WaitingOnUser,
                alive: true,
                foreground_process_name: Some("/usr/bin/codex".to_string()),
                tty_name: Some("/dev/pts/1".to_string()),
                last_input_at: None,
                last_output_at: None,
                last_progress_at: None,
                last_turn_completed_at: None,
                observed_turn: None,
                observed_at: Utc.with_ymd_and_hms(2026, 3, 29, 15, 24, 39).unwrap(),
                session_path: Some("/tmp/codex-session.jsonl".to_string()),
                progress_summary: Some("done".to_string()),
                harness_mode: Some("default".to_string()),
                turn_phase: Some("final_answer".to_string()),
                attention_reason: None,
                terminal_progress: wakterm_term::Progress::None,
                observer_error: None,
                observer_started_at: None,
                last_harness_refresh_at: None,
            },
            queue_delay: Duration::ZERO,
            refresh_elapsed: Duration::ZERO,
            schedule_trailing_refresh: false,
        });

        assert_eq!(*title_changes.lock(), 1);
    }

    #[test]
    fn repeated_output_throttles_harness_refresh() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let executor = promise::spawn::SimpleExecutor::new();
        let _env_lock = ENV_LOCK.lock().unwrap();
        let temp = TempDir::new().unwrap();
        let day = Utc::now();
        let dir = temp
            .path()
            .join(format!("{:04}", day.year_ce().1))
            .join(format!("{:02}", day.month()))
            .join(format!("{:02}", day.day()));
        std::fs::create_dir_all(&dir).unwrap();
        let session = dir.join("rollout-throttle.jsonl");
        std::fs::write(
            &session,
            concat!(
                "{\"payload\":{\"cwd\":\"/tmp/throttle-project\"}}\n",
                "{\"type\":\"event_msg\",\"timestamp\":\"2026-03-17T12:00:00Z\",\"payload\":{\"type\":\"task_started\",\"collaboration_mode_kind\":\"default\"}}\n",
                "{\"type\":\"response_item\",\"timestamp\":\"2026-03-17T12:00:00Z\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[]}}\n",
                "{\"type\":\"event_msg\",\"timestamp\":\"2026-03-17T12:00:02Z\",\"payload\":{\"type\":\"agent_message\",\"phase\":\"commentary\"}}\n"
            ),
        )
        .unwrap();
        unsafe {
            std::env::set_var("WAKTERM_AGENT_CODEX_DIR", temp.path());
        }

        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };

        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
        let tab = Arc::new(Tab::new(&size));
        let pane = FakePane::new_detected(
            147,
            size,
            domain.id,
            "codex",
            "/tmp/throttle-project",
            "/usr/bin/codex",
            &["codex"],
        );
        let pane_id = pane.pane_id();
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();
        mux.set_agent_metadata(
            pane_id,
            AgentMetadata {
                agent_id: "agent-throttle".to_string(),
                name: "throttle".to_string(),
                launch_cmd: "codex".to_string(),
                declared_cwd: "/tmp/throttle-project".to_string(),
                adopted_pid: None,
                adopted_start_time: None,
                created_at: Utc.with_ymd_and_hms(2026, 3, 17, 12, 0, 0).unwrap(),
                repo_root: None,
                worktree: None,
                branch: None,
                managed_checkout: false,
                codex_app_server: None,
            },
        )
        .unwrap();

        wait_for_main_thread_work(
            &executor,
            || {
                mux.agent_runtime_by_pane
                    .read()
                    .get(&pane_id)
                    .and_then(|runtime| runtime.last_harness_refresh_at)
                    .is_some()
            },
            "initial harness refresh",
        );

        let first_refresh = mux
            .agent_runtime_by_pane
            .read()
            .get(&pane_id)
            .and_then(|runtime| runtime.last_harness_refresh_at)
            .expect("initial harness refresh");

        mux.record_agent_output(pane_id);
        let throttled_refresh = mux
            .agent_runtime_by_pane
            .read()
            .get(&pane_id)
            .and_then(|runtime| runtime.last_harness_refresh_at)
            .expect("throttled harness refresh timestamp");
        assert_eq!(throttled_refresh, first_refresh);

        std::thread::sleep(AGENT_HARNESS_REFRESH_THROTTLE + Duration::from_millis(50));
        mux.record_agent_output(pane_id);
        wait_for_main_thread_work(
            &executor,
            || {
                mux.agent_runtime_by_pane
                    .read()
                    .get(&pane_id)
                    .and_then(|runtime| runtime.last_harness_refresh_at)
                    .map(|refreshed_at| refreshed_at > first_refresh)
                    .unwrap_or(false)
            },
            "throttled harness refresh",
        );
        let refreshed_again = mux
            .agent_runtime_by_pane
            .read()
            .get(&pane_id)
            .and_then(|runtime| runtime.last_harness_refresh_at)
            .expect("refresh after throttle window");
        assert!(refreshed_again > first_refresh);

        unsafe {
            std::env::remove_var("WAKTERM_AGENT_CODEX_DIR");
        }
    }

    #[test]
    fn confirmed_observer_burst_is_throttled_with_one_trailing_refresh() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;
        let pane_id = 9_001;
        let mut metadata = sample_agent_metadata("lossless-observer");
        metadata.adopted_pid = Some(std::process::id());
        metadata.adopted_start_time = Some(1);
        let mut runtime = AgentRuntimeSnapshot::new(&metadata);
        runtime.harness = crate::agent::AgentHarness::Codex;
        runtime.session_path = Some("/tmp/nonexistent-lossless-session.jsonl".to_string());

        mux.schedule_agent_observer_refresh(
            pane_id,
            &metadata,
            &runtime,
            AgentRefreshPolicy::Throttled,
            true,
        );
        mux.schedule_agent_observer_refresh(
            pane_id,
            &metadata,
            &runtime,
            AgentRefreshPolicy::Throttled,
            true,
        );

        let states = mux.agent_observer_state_by_pane.read();
        let state = states.get(&pane_id).unwrap();
        assert_eq!(state.latest_generation, 1);
        assert_eq!(state.inflight_generation, Some(1));
        assert!(state.pending_request.is_none());
        assert!(state.trailing_refresh_scheduled);
    }

    #[test]
    fn shared_timer_dispatches_trailing_refresh() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let executor = promise::spawn::SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;
        let pane_id = 10_000;
        mux.agent_observer_state_by_pane.write().insert(
            pane_id,
            AgentObserverState {
                trailing_refresh_scheduled: true,
                ..AgentObserverState::default()
            },
        );
        mux.agent_observer_timer_tx.send(pane_id).unwrap();

        wait_for_main_thread_work(
            &executor,
            || {
                mux.agent_observer_state_by_pane
                    .read()
                    .get(&pane_id)
                    .is_some_and(|state| !state.trailing_refresh_scheduled)
            },
            "shared trailing refresh timer",
        );
    }

    #[test]
    fn record_agent_output_preserves_mirrored_harness_for_remote_pane() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };

        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
        let tab = Arc::new(Tab::new(&size));
        let pane = FakePane::new(148, size, domain.id);
        let pane_id = pane.pane_id();
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();

        let metadata = AgentMetadata {
            agent_id: "remote-claude".to_string(),
            name: "remote_claude".to_string(),
            launch_cmd:
                "claude --dangerously-skip-permissions --add-dir /home/mihai --add-dir /code"
                    .to_string(),
            declared_cwd: "/code/application".to_string(),
            adopted_pid: None,
            adopted_start_time: None,
            created_at: Utc.with_ymd_and_hms(2026, 4, 3, 16, 27, 0).unwrap(),
            repo_root: None,
            worktree: None,
            branch: None,
            managed_checkout: false,
            codex_app_server: None,
        };
        mux.set_mirrored_agent_metadata(pane_id, Some(&metadata));

        assert!(matches!(
            mux.cached_agent_harness_for_pane(pane_id),
            Some(crate::agent::AgentHarness::Claude)
        ));

        mux.record_agent_output(pane_id);

        assert!(matches!(
            mux.cached_agent_harness_for_pane(pane_id),
            Some(crate::agent::AgentHarness::Claude)
        ));
    }

    #[test]
    fn restore_agent_metadata_queues_initial_harness_refresh() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let executor = promise::spawn::SimpleExecutor::new();
        let _env_lock = ENV_LOCK.lock().unwrap();
        let temp = TempDir::new().unwrap();
        let day = Utc::now();
        let dir = temp
            .path()
            .join(format!("{:04}", day.year_ce().1))
            .join(format!("{:02}", day.month()))
            .join(format!("{:02}", day.day()));
        std::fs::create_dir_all(&dir).unwrap();
        let session = dir.join("rollout-restore-agent.jsonl");
        std::fs::write(
            &session,
            concat!(
                "{\"payload\":{\"cwd\":\"/tmp/restore-agent-project\"}}\n",
                "{\"type\":\"event_msg\",\"timestamp\":\"2026-03-17T12:00:00Z\",\"payload\":{\"type\":\"task_started\",\"collaboration_mode_kind\":\"default\"}}\n",
                "{\"type\":\"response_item\",\"timestamp\":\"2026-03-17T12:00:00Z\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[]}}\n",
                "{\"type\":\"event_msg\",\"timestamp\":\"2026-03-17T12:00:02Z\",\"payload\":{\"type\":\"agent_message\",\"phase\":\"commentary\"}}\n"
            ),
        )
        .unwrap();
        unsafe {
            std::env::set_var("WAKTERM_AGENT_CODEX_DIR", temp.path());
        }

        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };

        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
        let tab = Arc::new(Tab::new(&size));
        let pane = FakePane::new_detected(
            150,
            size,
            domain.id,
            "codex",
            "/tmp/restore-agent-project",
            "/usr/bin/codex",
            &["codex"],
        );
        let pane_id = pane.pane_id();
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();

        mux.restore_agent_metadata(
            pane_id,
            AgentMetadata {
                agent_id: "agent-restore".to_string(),
                name: "restore".to_string(),
                launch_cmd: "codex".to_string(),
                declared_cwd: "/tmp/restore-agent-project".to_string(),
                adopted_pid: None,
                adopted_start_time: None,
                created_at: Utc.with_ymd_and_hms(2026, 3, 17, 12, 0, 0).unwrap(),
                repo_root: None,
                worktree: None,
                branch: None,
                managed_checkout: false,
                codex_app_server: None,
            },
        )
        .unwrap();

        let initial_refresh = mux
            .agent_runtime_by_pane
            .read()
            .get(&pane_id)
            .and_then(|runtime| runtime.last_harness_refresh_at);
        assert_eq!(initial_refresh, None);

        wait_for_main_thread_work(
            &executor,
            || {
                mux.agent_runtime_by_pane
                    .read()
                    .get(&pane_id)
                    .and_then(|runtime| runtime.last_harness_refresh_at)
                    .is_some()
            },
            "async restored agent refresh",
        );

        unsafe {
            std::env::remove_var("WAKTERM_AGENT_CODEX_DIR");
        }
    }

    #[test]
    fn list_agents_does_not_refresh_adopted_observer_synchronously() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let executor = promise::spawn::SimpleExecutor::new();
        let _env_lock = ENV_LOCK.lock().unwrap();
        let temp = TempDir::new().unwrap();
        let day = Utc::now();
        let dir = temp
            .path()
            .join(format!("{:04}", day.year_ce().1))
            .join(format!("{:02}", day.month()))
            .join(format!("{:02}", day.day()));
        std::fs::create_dir_all(&dir).unwrap();
        let session = dir.join("rollout-list-agents.jsonl");
        std::fs::write(
            &session,
            concat!(
                "{\"payload\":{\"cwd\":\"/tmp/list-agents-project\"}}\n",
                "{\"type\":\"event_msg\",\"timestamp\":\"2026-03-17T12:00:00Z\",\"payload\":{\"type\":\"task_started\",\"collaboration_mode_kind\":\"default\"}}\n",
                "{\"type\":\"response_item\",\"timestamp\":\"2026-03-17T12:00:00Z\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[]}}\n",
                "{\"type\":\"event_msg\",\"timestamp\":\"2026-03-17T12:00:02Z\",\"payload\":{\"type\":\"agent_message\",\"phase\":\"commentary\"}}\n"
            ),
        )
        .unwrap();
        unsafe {
            std::env::set_var("WAKTERM_AGENT_CODEX_DIR", temp.path());
        }

        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };

        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
        let tab = Arc::new(Tab::new(&size));
        let pane = FakePane::new_detected(
            148,
            size,
            domain.id,
            "codex",
            "/tmp/list-agents-project",
            "/usr/bin/codex",
            &["codex"],
        );
        let pane_id = pane.pane_id();
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();
        mux.set_agent_metadata(
            pane_id,
            AgentMetadata {
                agent_id: "agent-list-agents".to_string(),
                name: "list-agents".to_string(),
                launch_cmd: "codex".to_string(),
                declared_cwd: "/tmp/list-agents-project".to_string(),
                adopted_pid: None,
                adopted_start_time: None,
                created_at: Utc.with_ymd_and_hms(2026, 3, 17, 12, 0, 0).unwrap(),
                repo_root: None,
                worktree: None,
                branch: None,
                managed_checkout: false,
                codex_app_server: None,
            },
        )
        .unwrap();

        wait_for_main_thread_work(
            &executor,
            || {
                mux.agent_runtime_by_pane
                    .read()
                    .get(&pane_id)
                    .and_then(|runtime| runtime.last_harness_refresh_at)
                    .is_some()
            },
            "initial list_agents harness refresh",
        );

        let first_refresh = mux
            .agent_runtime_by_pane
            .read()
            .get(&pane_id)
            .and_then(|runtime| runtime.last_harness_refresh_at)
            .expect("initial harness refresh");

        std::thread::sleep(AGENT_HARNESS_REFRESH_THROTTLE + Duration::from_millis(50));

        let agents = mux.list_agents();
        assert_eq!(agents.len(), 1);
        assert_eq!(
            agents[0].runtime.last_harness_refresh_at,
            Some(first_refresh)
        );

        wait_for_main_thread_work(
            &executor,
            || {
                mux.agent_runtime_by_pane
                    .read()
                    .get(&pane_id)
                    .and_then(|runtime| runtime.last_harness_refresh_at)
                    .map(|refreshed_at| refreshed_at > first_refresh)
                    .unwrap_or(false)
            },
            "async list_agents observer refresh",
        );

        unsafe {
            std::env::remove_var("WAKTERM_AGENT_CODEX_DIR");
        }
    }

    #[test]
    fn list_agents_cached_does_not_refresh_adopted_observer() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let executor = promise::spawn::SimpleExecutor::new();
        let _env_lock = ENV_LOCK.lock().unwrap();
        let temp = TempDir::new().unwrap();
        let day = Utc::now();
        let dir = temp
            .path()
            .join(format!("{:04}", day.year_ce().1))
            .join(format!("{:02}", day.month()))
            .join(format!("{:02}", day.day()));
        std::fs::create_dir_all(&dir).unwrap();
        let session = dir.join("rollout-list-agents-cached.jsonl");
        std::fs::write(
            &session,
            concat!(
                "{\"payload\":{\"cwd\":\"/tmp/list-agents-cached-project\"}}\n",
                "{\"type\":\"event_msg\",\"timestamp\":\"2026-03-17T12:00:00Z\",\"payload\":{\"type\":\"task_started\",\"collaboration_mode_kind\":\"default\"}}\n",
                "{\"type\":\"response_item\",\"timestamp\":\"2026-03-17T12:00:00Z\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[]}}\n",
                "{\"type\":\"event_msg\",\"timestamp\":\"2026-03-17T12:00:02Z\",\"payload\":{\"type\":\"agent_message\",\"phase\":\"commentary\"}}\n"
            ),
        )
        .unwrap();
        unsafe {
            std::env::set_var("WAKTERM_AGENT_CODEX_DIR", temp.path());
        }

        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };

        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
        let tab = Arc::new(Tab::new(&size));
        let pane = FakePane::new_detected(
            149,
            size,
            domain.id,
            "codex",
            "/tmp/list-agents-cached-project",
            "/usr/bin/codex",
            &["codex"],
        );
        let pane_id = pane.pane_id();
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();
        mux.set_agent_metadata(
            pane_id,
            AgentMetadata {
                agent_id: "agent-list-agents-cached".to_string(),
                name: "list-agents-cached".to_string(),
                launch_cmd: "codex".to_string(),
                declared_cwd: "/tmp/list-agents-cached-project".to_string(),
                adopted_pid: None,
                adopted_start_time: None,
                created_at: Utc.with_ymd_and_hms(2026, 3, 17, 12, 0, 0).unwrap(),
                repo_root: None,
                worktree: None,
                branch: None,
                managed_checkout: false,
                codex_app_server: None,
            },
        )
        .unwrap();

        wait_for_main_thread_work(
            &executor,
            || {
                mux.agent_runtime_by_pane
                    .read()
                    .get(&pane_id)
                    .and_then(|runtime| runtime.last_harness_refresh_at)
                    .is_some()
            },
            "initial cached list harness refresh",
        );

        let first_refresh = mux
            .agent_runtime_by_pane
            .read()
            .get(&pane_id)
            .and_then(|runtime| runtime.last_harness_refresh_at)
            .expect("initial harness refresh");

        std::thread::sleep(AGENT_HARNESS_REFRESH_THROTTLE + Duration::from_millis(50));

        let agents = mux.list_agents_cached();
        assert_eq!(agents.len(), 1);
        assert_eq!(
            agents[0].runtime.last_harness_refresh_at,
            Some(first_refresh)
        );
        assert_eq!(
            mux.agent_runtime_by_pane
                .read()
                .get(&pane_id)
                .and_then(|runtime| runtime.last_harness_refresh_at),
            Some(first_refresh)
        );

        unsafe {
            std::env::remove_var("WAKTERM_AGENT_CODEX_DIR");
        }
    }

    #[test]
    fn refresh_agent_runtime_for_tab_queues_observer_refresh() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let executor = promise::spawn::SimpleExecutor::new();
        let _env_lock = ENV_LOCK.lock().unwrap();
        let temp = TempDir::new().unwrap();
        let day = Utc::now();
        let dir = temp
            .path()
            .join(format!("{:04}", day.year_ce().1))
            .join(format!("{:02}", day.month()))
            .join(format!("{:02}", day.day()));
        std::fs::create_dir_all(&dir).unwrap();
        let session = dir.join("rollout-refresh-tab.jsonl");
        std::fs::write(
            &session,
            concat!(
                "{\"payload\":{\"cwd\":\"/tmp/refresh-tab-project\"}}\n",
                "{\"type\":\"event_msg\",\"timestamp\":\"2026-03-17T12:00:00Z\",\"payload\":{\"type\":\"task_started\",\"collaboration_mode_kind\":\"default\"}}\n",
                "{\"type\":\"response_item\",\"timestamp\":\"2026-03-17T12:00:00Z\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[]}}\n",
                "{\"type\":\"event_msg\",\"timestamp\":\"2026-03-17T12:00:02Z\",\"payload\":{\"type\":\"agent_message\",\"phase\":\"commentary\"}}\n"
            ),
        )
        .unwrap();
        unsafe {
            std::env::set_var("WAKTERM_AGENT_CODEX_DIR", temp.path());
        }

        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };

        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
        let tab = Arc::new(Tab::new(&size));
        let pane = FakePane::new_detected(
            149,
            size,
            domain.id,
            "codex",
            "/tmp/refresh-tab-project",
            "/usr/bin/codex",
            &["codex"],
        );
        let pane_id = pane.pane_id();
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();
        mux.set_agent_metadata(
            pane_id,
            AgentMetadata {
                agent_id: "agent-refresh-tab".to_string(),
                name: "refresh-tab".to_string(),
                launch_cmd: "codex".to_string(),
                declared_cwd: "/tmp/refresh-tab-project".to_string(),
                adopted_pid: None,
                adopted_start_time: None,
                created_at: Utc.with_ymd_and_hms(2026, 3, 17, 12, 0, 0).unwrap(),
                repo_root: None,
                worktree: None,
                branch: None,
                managed_checkout: false,
                codex_app_server: None,
            },
        )
        .unwrap();

        wait_for_main_thread_work(
            &executor,
            || {
                mux.agent_runtime_by_pane
                    .read()
                    .get(&pane_id)
                    .and_then(|runtime| runtime.last_harness_refresh_at)
                    .is_some()
            },
            "initial tab harness refresh",
        );

        let first_refresh = mux
            .agent_runtime_by_pane
            .read()
            .get(&pane_id)
            .and_then(|runtime| runtime.last_harness_refresh_at)
            .expect("initial harness refresh");

        std::thread::sleep(AGENT_HARNESS_REFRESH_THROTTLE + Duration::from_millis(50));

        mux.refresh_agent_runtime_for_tab(tab.tab_id());
        let queued_refresh = mux
            .agent_runtime_by_pane
            .read()
            .get(&pane_id)
            .and_then(|runtime| runtime.last_harness_refresh_at)
            .expect("queued harness refresh timestamp");
        assert_eq!(queued_refresh, first_refresh);

        wait_for_main_thread_work(
            &executor,
            || {
                mux.agent_runtime_by_pane
                    .read()
                    .get(&pane_id)
                    .and_then(|runtime| runtime.last_harness_refresh_at)
                    .map(|refreshed_at| refreshed_at > first_refresh)
                    .unwrap_or(false)
            },
            "async tab observer refresh",
        );

        unsafe {
            std::env::remove_var("WAKTERM_AGENT_CODEX_DIR");
        }
    }

    #[test]
    fn detected_harness_panes_are_listed_without_adoption() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };

        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
        let tab = Arc::new(Tab::new(&size));
        let pane = FakePane::new_detected(
            145,
            size,
            domain.id,
            "codex",
            "/tmp/wakterm",
            "/usr/bin/codex",
            &["codex", "-a", "never"],
        );
        let pane_id = pane.pane_id();
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();

        let agents = mux.list_agents();
        assert_eq!(agents.len(), 1);
        let agent = &agents[0];
        assert!(matches!(agent.origin, AgentOrigin::Detected));
        assert_eq!(agent.metadata.name, "wakterm_codex");
        assert_eq!(agent.metadata.launch_cmd, "codex -a never");
        assert_eq!(agent.metadata.declared_cwd, "/tmp/wakterm");
        assert_eq!(agent.pane_id, pane_id);
        assert_eq!(agent.workspace, DEFAULT_WORKSPACE);
        assert_eq!(agent.detection_source.as_deref(), Some("proc+title"));
        assert_eq!(agent.runtime.harness, crate::agent::AgentHarness::Codex);
        assert!(mux.get_agent_metadata_for_pane(pane_id).is_none());
    }

    #[test]
    fn pending_auto_adoption_reuses_process_evidence_across_output_and_listing() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let _config = TestConfigGuard::new_with_auto_adopt("attention", "🤖 ", true);
        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };
        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
        let tab = Arc::new(Tab::new(&size));
        let (pane, process_info_calls) = FakePane::new_detected_counted(
            152,
            size,
            domain.id,
            "codex",
            "/tmp/pending-candidate",
            "/usr/bin/codex",
            &["codex"],
        );
        let pane_id = pane.pane_id();
        let pane_for_tab: Arc<dyn Pane> = pane;
        tab.assign_pane(&pane_for_tab);
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();

        mux.record_agent_output(pane_id);
        for _ in 0..4 {
            mux.record_agent_output(pane_id);
            let _ = mux.list_agents();
            let _ = mux.list_agents_cached();
        }

        assert_eq!(process_info_calls.load(Ordering::SeqCst), 1);
        assert!(mux.agent_adoption_candidates.read().contains_key(&pane_id));
        assert!(mux
            .agent_observer_state_by_pane
            .read()
            .get(&pane_id)
            .is_some_and(|state| state.inflight_generation.is_some()));
    }

    #[test]
    fn observer_confirmation_adopts_once_from_cached_candidate() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let _config = TestConfigGuard::new_with_auto_adopt("attention", "🤖 ", true);
        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };
        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
        let tab = Arc::new(Tab::new(&size));
        let (pane, process_info_calls) = FakePane::new_detected_counted(
            153,
            size,
            domain.id,
            "codex",
            "/tmp/confirm-once",
            "/usr/bin/codex",
            &["codex"],
        );
        let pane_id = pane.pane_id();
        let pane_for_tab: Arc<dyn Pane> = pane;
        let tab_id = tab.tab_id();
        tab.assign_pane(&pane_for_tab);
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();

        let title_changes = Arc::new(Mutex::new(0usize));
        let title_changes_for_sub = Arc::clone(&title_changes);
        mux.subscribe(move |notification| {
            if matches!(notification, MuxNotification::TabTitleChanged { tab_id: changed, .. } if changed == tab_id) {
                *title_changes_for_sub.lock() += 1;
            }
            true
        });

        mux.record_agent_output(pane_id);
        *title_changes.lock() = 0;
        let mut runtime = mux
            .agent_runtime_by_pane
            .read()
            .get(&pane_id)
            .cloned()
            .expect("cached candidate runtime");
        runtime.session_path = Some("/tmp/confirmed-session.jsonl".to_string());
        runtime.transport = crate::agent::AgentTransport::ObservedPty;
        let update = AgentObserverUpdate {
            pane_id,
            generation: 1,
            runtime,
            queue_delay: Duration::ZERO,
            refresh_elapsed: Duration::ZERO,
            schedule_trailing_refresh: false,
        };

        mux.apply_agent_observer_update(update);
        assert!(mux.get_agent_metadata_for_pane(pane_id).is_some());
        assert_eq!(*title_changes.lock(), 1);
        let process_calls_after_adoption = process_info_calls.load(Ordering::SeqCst);

        let adopted_runtime = mux
            .agent_runtime_by_pane
            .read()
            .get(&pane_id)
            .cloned()
            .expect("adopted runtime");
        mux.apply_agent_observer_update(AgentObserverUpdate {
            pane_id,
            generation: 1,
            runtime: adopted_runtime,
            queue_delay: Duration::ZERO,
            refresh_elapsed: Duration::ZERO,
            schedule_trailing_refresh: false,
        });

        assert!(mux.get_agent_metadata_for_pane(pane_id).is_some());
        assert_eq!(*title_changes.lock(), 1);
        assert_eq!(
            process_info_calls.load(Ordering::SeqCst),
            process_calls_after_adoption
        );
    }

    #[test]
    fn observer_confirmation_discards_replaced_process_incarnation() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let _config = TestConfigGuard::new_with_auto_adopt("attention", "🤖 ", true);
        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };
        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
        let tab = Arc::new(Tab::new(&size));
        let (pane, _) = FakePane::new_detected_counted(
            154,
            size,
            domain.id,
            "codex",
            "/tmp/replaced-incarnation",
            "/usr/bin/codex",
            &["codex"],
        );
        let pane_id = pane.pane_id();
        let pane_for_tab: Arc<dyn Pane> = pane;
        tab.assign_pane(&pane_for_tab);
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();
        mux.record_agent_output(pane_id);

        let (mut replacement, _) = FakePane::new_detected_counted(
            pane_id,
            size,
            domain.id,
            "codex",
            "/tmp/replaced-incarnation",
            "/usr/bin/codex",
            &["codex"],
        );
        Arc::get_mut(&mut replacement)
            .expect("replacement pane is uniquely owned")
            .foreground_process_info
            .as_mut()
            .expect("replacement process info")
            .pid = 2;
        Arc::get_mut(&mut replacement)
            .expect("replacement pane is uniquely owned")
            .foreground_process_info
            .as_mut()
            .expect("replacement process info")
            .start_time = 2;
        mux.panes
            .write()
            .insert(pane_id, replacement as Arc<dyn Pane>);

        let mut runtime = mux
            .agent_runtime_by_pane
            .read()
            .get(&pane_id)
            .cloned()
            .expect("cached candidate runtime");
        runtime.session_path = Some("/tmp/stale-session.jsonl".to_string());
        mux.apply_agent_observer_update(AgentObserverUpdate {
            pane_id,
            generation: 1,
            runtime,
            queue_delay: Duration::ZERO,
            refresh_elapsed: Duration::ZERO,
            schedule_trailing_refresh: false,
        });

        assert!(mux.get_agent_metadata_for_pane(pane_id).is_none());
        assert!(!mux.detected_agent_panes.read().contains(&pane_id));
        assert!(!mux.agent_adoption_candidates.read().contains_key(&pane_id));
    }

    #[test]
    fn title_only_evidence_never_adopts_a_session() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let _config = TestConfigGuard::new_with_auto_adopt("attention", "🤖 ", true);
        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };
        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
        let tab = Arc::new(Tab::new(&size));
        let pane = FakePane::new_title_only(155, size, domain.id, "codex", "/tmp/title-only");
        let pane_id = pane.pane_id();
        let pane_for_tab: Arc<dyn Pane> = pane;
        tab.assign_pane(&pane_for_tab);
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();

        let agents = mux.list_agents();
        assert_eq!(agents.len(), 1);
        assert!(matches!(agents[0].origin, AgentOrigin::Detected));
        let mut runtime = mux
            .agent_runtime_by_pane
            .read()
            .get(&pane_id)
            .cloned()
            .expect("title candidate runtime");
        runtime.session_path = Some("/tmp/title-only-session.jsonl".to_string());
        mux.apply_agent_observer_update(AgentObserverUpdate {
            pane_id,
            generation: 1,
            runtime,
            queue_delay: Duration::ZERO,
            refresh_elapsed: Duration::ZERO,
            schedule_trailing_refresh: false,
        });

        assert!(mux.get_agent_metadata_for_pane(pane_id).is_none());
        assert!(!mux.detected_agent_panes.read().contains(&pane_id));
    }

    #[test]
    fn codex_restore_intent_uses_confirmed_runtime_and_concrete_process_command() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };
        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
        let tab = Arc::new(Tab::new(&size));
        let session_id = "00000000-0000-4000-8000-000000000004";
        let pane = FakePane::new_detected(
            209,
            size,
            domain.id,
            "codex",
            "/code/alias",
            "/usr/local/bin/codex",
            &["/usr/local/bin/codex", "-a", "never", "resume", session_id],
        );
        let pane_id = pane.pane_id();
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();

        let session_dir = tempfile::tempdir().unwrap();
        let session_path = session_dir.path().join("rollout.jsonl");
        std::fs::write(
            &session_path,
            format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{session_id}\"}}}}\n"),
        )
        .unwrap();
        let mut metadata = sample_agent_metadata("alias");
        metadata.launch_cmd = "co".to_string();
        mux.set_agent_metadata(pane_id, metadata.clone()).unwrap();
        let mut runtime = AgentRuntimeSnapshot::new(&metadata);
        runtime.harness = AgentHarness::Codex;
        runtime.transport = crate::agent::AgentTransport::ObservedPty;
        runtime.alive = true;
        runtime.session_path = Some(session_path.to_string_lossy().into_owned());
        mux.agent_runtime_by_pane.write().insert(pane_id, runtime);

        let (harness, restored, restored_session_id) =
            mux.agent_restore_intent_for_pane(pane_id).unwrap();
        assert_eq!(harness, AgentHarness::Codex);
        assert_eq!(restored_session_id, session_id);
        assert_eq!(restored.launch_cmd, "/usr/local/bin/codex -a never");
        assert_eq!(restored.agent_id, metadata.agent_id);
    }

    #[test]
    fn claude_restore_intent_uses_confirmed_runtime_and_concrete_process_command() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };
        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
        let tab = Arc::new(Tab::new(&size));
        let session_id = "00000000-0000-4000-8000-000000000005";
        let pane = FakePane::new_detected(
            213,
            size,
            domain.id,
            "claude",
            "/code/alias",
            "/home/mihai/.local/bin/claude",
            &[
                "/home/mihai/.local/bin/claude",
                "--dangerously-skip-permissions",
                "--add-dir",
                "/home/mihai",
                "--resume",
                session_id,
            ],
        );
        let pane_id = pane.pane_id();
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();

        let session_dir = tempfile::tempdir().unwrap();
        let session_path = session_dir.path().join(format!("{session_id}.jsonl"));
        std::fs::write(
            &session_path,
            format!("{{\"type\":\"user\",\"sessionId\":\"{session_id}\"}}\n"),
        )
        .unwrap();
        let mut metadata = sample_agent_metadata("alias");
        metadata.launch_cmd = "cl".to_string();
        mux.set_agent_metadata(pane_id, metadata.clone()).unwrap();
        let mut runtime = AgentRuntimeSnapshot::new(&metadata);
        runtime.harness = AgentHarness::Claude;
        runtime.transport = crate::agent::AgentTransport::ObservedPty;
        runtime.alive = true;
        runtime.session_path = Some(session_path.to_string_lossy().into_owned());
        mux.agent_runtime_by_pane.write().insert(pane_id, runtime);

        let (harness, restored, restored_session_id) =
            mux.agent_restore_intent_for_pane(pane_id).unwrap();
        assert_eq!(harness, AgentHarness::Claude);
        assert_eq!(restored_session_id, session_id);
        assert_eq!(
            restored.launch_cmd,
            "/home/mihai/.local/bin/claude --dangerously-skip-permissions --add-dir /home/mihai"
        );
        assert_eq!(restored.agent_id, metadata.agent_id);
    }

    #[test]
    fn codex_restore_handshake_binds_only_the_expected_provider_session() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };

        let matching_session = tempfile::tempdir().unwrap();
        let matching_path = matching_session.path().join("rollout.jsonl");
        std::fs::write(
            &matching_path,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"session-match\"}}\n",
        )
        .unwrap();

        let matching_window = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
        let matching_tab = Arc::new(Tab::new(&size));
        let matching_pane = FakePane::new_detected(
            210,
            size,
            domain.id,
            "codex",
            "/tmp/codex-restore-match",
            "/usr/bin/codex",
            &["codex"],
        );
        let matching_pane_id = matching_pane.pane_id();
        let matching_tab_id = matching_tab.tab_id();
        matching_tab.assign_pane(&matching_pane);
        mux.add_tab_and_active_pane(&matching_tab).unwrap();
        mux.add_tab_to_window(&matching_tab, matching_window)
            .unwrap();

        let matching_metadata = sample_agent_metadata("restore-match");
        mux.register_agent_restore_intent(
            matching_pane_id,
            AgentHarness::Codex,
            matching_metadata.clone(),
            "session-match".to_string(),
        )
        .unwrap();
        mux.detected_agent_panes.write().insert(matching_pane_id);
        let mut matching_runtime = AgentRuntimeSnapshot::new(&matching_metadata);
        matching_runtime.transport = crate::agent::AgentTransport::ObservedPty;
        matching_runtime.turn_state = crate::agent::AgentTurnState::WaitingOnUser;
        matching_runtime.session_path = Some(matching_path.to_string_lossy().into_owned());
        mux.agent_runtime_by_pane
            .write()
            .insert(matching_pane_id, matching_runtime.clone());
        let matching_candidate = AgentAdoptionCandidate {
            pane_id: matching_pane_id,
            harness: crate::agent::AgentHarness::Codex,
            declared_cwd: matching_metadata.declared_cwd.clone(),
            launch_cmd: matching_metadata.launch_cmd.clone(),
            foreground_pid: Some(1),
            process_start_time: Some(1),
            created_at: matching_metadata.created_at,
            tab_id: matching_tab_id,
            window_id: matching_window,
            workspace: DEFAULT_WORKSPACE.to_string(),
            domain_id: domain.id,
            detection_source: "restore-test".to_string(),
        };
        mux.agent_adoption_candidates
            .write()
            .insert(matching_pane_id, matching_candidate.clone());

        assert_eq!(
            mux.complete_pending_agent_restore(
                matching_pane_id,
                matching_candidate,
                matching_runtime,
            ),
            AgentRestoreOutcome::Completed
        );
        assert_eq!(
            mux.get_agent_metadata_for_pane(matching_pane_id)
                .as_deref()
                .map(|metadata| metadata.name.as_str()),
            Some("restore-match")
        );
        assert!(!mux
            .pending_agent_restores
            .read()
            .contains_key(&matching_pane_id));

        let mismatching_session = tempfile::tempdir().unwrap();
        let mismatching_path = mismatching_session.path().join("rollout.jsonl");
        std::fs::write(
            &mismatching_path,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"session-other\"}}\n",
        )
        .unwrap();

        let mismatching_window = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
        let mismatching_tab = Arc::new(Tab::new(&size));
        let mismatching_pane = FakePane::new_detected(
            211,
            size,
            domain.id,
            "codex",
            "/tmp/codex-restore-mismatch",
            "/usr/bin/codex",
            &["codex"],
        );
        let mismatching_pane_id = mismatching_pane.pane_id();
        let mismatching_tab_id = mismatching_tab.tab_id();
        mismatching_tab.assign_pane(&mismatching_pane);
        mux.add_tab_and_active_pane(&mismatching_tab).unwrap();
        mux.add_tab_to_window(&mismatching_tab, mismatching_window)
            .unwrap();

        let mismatching_metadata = sample_agent_metadata("restore-mismatch");
        mux.register_agent_restore_intent(
            mismatching_pane_id,
            AgentHarness::Codex,
            mismatching_metadata.clone(),
            "session-expected".to_string(),
        )
        .unwrap();
        mux.detected_agent_panes.write().insert(mismatching_pane_id);
        let mut mismatching_runtime = AgentRuntimeSnapshot::new(&mismatching_metadata);
        mismatching_runtime.transport = crate::agent::AgentTransport::ObservedPty;
        mismatching_runtime.turn_state = crate::agent::AgentTurnState::WaitingOnUser;
        mismatching_runtime.session_path = Some(mismatching_path.to_string_lossy().into_owned());
        mux.agent_runtime_by_pane
            .write()
            .insert(mismatching_pane_id, mismatching_runtime.clone());
        let mismatching_candidate = AgentAdoptionCandidate {
            pane_id: mismatching_pane_id,
            harness: crate::agent::AgentHarness::Codex,
            declared_cwd: mismatching_metadata.declared_cwd.clone(),
            launch_cmd: mismatching_metadata.launch_cmd.clone(),
            foreground_pid: Some(1),
            process_start_time: Some(1),
            created_at: mismatching_metadata.created_at,
            tab_id: mismatching_tab_id,
            window_id: mismatching_window,
            workspace: DEFAULT_WORKSPACE.to_string(),
            domain_id: domain.id,
            detection_source: "restore-test".to_string(),
        };
        mux.agent_adoption_candidates
            .write()
            .insert(mismatching_pane_id, mismatching_candidate.clone());

        assert_eq!(
            mux.complete_pending_agent_restore(
                mismatching_pane_id,
                mismatching_candidate,
                mismatching_runtime,
            ),
            AgentRestoreOutcome::Failed
        );
        assert!(mux
            .get_agent_metadata_for_pane(mismatching_pane_id)
            .is_none());
        assert!(!mux
            .pending_agent_restores
            .read()
            .contains_key(&mismatching_pane_id));
        assert!(!mux
            .detected_agent_panes
            .read()
            .contains(&mismatching_pane_id));
        assert!(mux
            .list_agents()
            .iter()
            .all(|agent| agent.pane_id != mismatching_pane_id));
    }

    #[test]
    fn claude_restore_handshake_binds_the_exact_provider_session() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };
        let session_id = "00000000-0000-4000-8000-000000000009";
        let session_dir = tempfile::tempdir().unwrap();
        let session_path = session_dir.path().join(format!("{session_id}.jsonl"));
        std::fs::write(
            &session_path,
            format!("{{\"type\":\"mode\",\"sessionId\":\"{session_id}\"}}\n"),
        )
        .unwrap();

        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
        let tab = Arc::new(Tab::new(&size));
        let pane = FakePane::new_detected(
            214,
            size,
            domain.id,
            "claude",
            "/tmp/claude-restore-match",
            "/home/mihai/.local/bin/claude",
            &["claude", "--resume", session_id],
        );
        let pane_id = pane.pane_id();
        let tab_id = tab.tab_id();
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();

        let mut metadata = sample_agent_metadata("claude-restore-match");
        metadata.launch_cmd = "claude --dangerously-skip-permissions".to_string();
        mux.register_agent_restore_intent(
            pane_id,
            AgentHarness::Claude,
            metadata.clone(),
            session_id.to_string(),
        )
        .unwrap();
        mux.detected_agent_panes.write().insert(pane_id);
        let mut runtime = AgentRuntimeSnapshot::new(&metadata);
        runtime.harness = AgentHarness::Claude;
        runtime.transport = crate::agent::AgentTransport::ObservedPty;
        runtime.session_path = Some(session_path.to_string_lossy().into_owned());
        mux.agent_runtime_by_pane
            .write()
            .insert(pane_id, runtime.clone());
        let candidate = AgentAdoptionCandidate {
            pane_id,
            harness: AgentHarness::Claude,
            declared_cwd: metadata.declared_cwd.clone(),
            launch_cmd: metadata.launch_cmd.clone(),
            foreground_pid: Some(1),
            process_start_time: Some(1),
            created_at: metadata.created_at,
            tab_id,
            window_id,
            workspace: DEFAULT_WORKSPACE.to_string(),
            domain_id: domain.id,
            detection_source: "restore-test".to_string(),
        };
        mux.agent_adoption_candidates
            .write()
            .insert(pane_id, candidate.clone());

        assert_eq!(
            mux.complete_pending_agent_restore(pane_id, candidate, runtime),
            AgentRestoreOutcome::Completed
        );
        assert_eq!(
            mux.get_agent_metadata_for_pane(pane_id)
                .as_deref()
                .map(|metadata| metadata.name.as_str()),
            Some("claude-restore-match")
        );
        assert!(!mux.pending_agent_restores.read().contains_key(&pane_id));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pending_codex_restore_observes_with_persisted_cwd() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let executor = promise::spawn::SimpleExecutor::new();
        let _env_lock = ENV_LOCK.lock().unwrap();
        let temp = TempDir::new().unwrap();
        let session = temp.path().join("rollout-pending-restore.jsonl");
        std::fs::write(
            &session,
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"pending-session\",\"cwd\":\"/tmp/pending-restore\"}}\n",
                "{\"type\":\"event_msg\",\"timestamp\":\"2026-03-17T12:00:00Z\",\"payload\":{\"type\":\"task_complete\"}}\n"
            ),
        )
        .unwrap();
        let other_session = temp.path().join("rollout-other.jsonl");
        std::fs::write(
            &other_session,
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"other-session\",\"cwd\":\"/tmp/pending-restore\"}}\n",
                "{\"type\":\"event_msg\",\"timestamp\":\"2026-03-17T12:00:01Z\",\"payload\":{\"type\":\"task_complete\"}}\n"
            ),
        )
        .unwrap();
        std::fs::File::open(&session)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1))
            .unwrap();
        std::fs::File::open(&other_session)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(2))
            .unwrap();
        let _open_session = std::fs::File::open(&session).unwrap();
        let _open_other_session = std::fs::File::open(&other_session).unwrap();
        unsafe {
            std::env::set_var("WAKTERM_AGENT_CODEX_DIR", temp.path());
        }

        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };
        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
        let tab = Arc::new(Tab::new(&size));
        let mut process = LocalProcessInfo::with_root_pid(std::process::id()).unwrap();
        process.name = "codex".to_string();
        process.executable = PathBuf::from("/usr/bin/codex");
        process.argv = vec![
            "codex".to_string(),
            "resume".to_string(),
            "pending-session".to_string(),
        ];
        process.cwd = PathBuf::from("/tmp/pending-restore");
        let pane: Arc<dyn Pane> = Arc::new(FakePane {
            id: 212,
            size: Mutex::new(size),
            domain_id: domain.id,
            title: "codex".to_string(),
            cwd: Some(FakePane::test_file_url("/tmp/pending-restore/")),
            foreground_process_name: Some("/usr/bin/codex".to_string()),
            foreground_process_info: Some(process),
            #[cfg(target_os = "linux")]
            foreground_process_root_pid: None,
            foreground_process_info_calls: None,
        });
        let pane_id = pane.pane_id();
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();

        let mut metadata = sample_agent_metadata("pending-restore");
        metadata.declared_cwd = "/tmp/pending-restore".to_string();
        mux.register_agent_restore_intent(
            pane_id,
            AgentHarness::Codex,
            metadata.clone(),
            "pending-session".to_string(),
        )
        .unwrap();

        let agents = mux.list_agents();
        assert_eq!(agents.len(), 1);
        assert!(matches!(agents[0].origin, AgentOrigin::Detected));
        assert_eq!(agents[0].metadata.declared_cwd, "/tmp/pending-restore/");

        wait_for_main_thread_work(
            &executor,
            || mux.get_agent_metadata_for_pane(pane_id).is_some(),
            "pending Codex restore confirmation",
        );
        unsafe {
            std::env::remove_var("WAKTERM_AGENT_CODEX_DIR");
        }

        let restored = mux.get_agent_metadata_for_pane(pane_id).unwrap();
        assert_eq!(restored.name, metadata.name);
        assert_eq!(restored.declared_cwd, metadata.declared_cwd);
        assert!(!mux.pending_agent_restores.read().contains_key(&pane_id));
        assert_eq!(
            mux.agent_runtime_by_pane
                .read()
                .get(&pane_id)
                .and_then(|runtime| runtime.session_path.as_deref())
                .map(Path::new),
            Some(session.as_path())
        );
    }

    #[test]
    fn confirmed_harnesses_keep_provider_identity_during_auto_adoption() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let _config = TestConfigGuard::new_with_auto_adopt("attention", "🤖 ", true);
        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };
        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
        let cases = [
            (155, "agy", "/usr/bin/agy", crate::agent::AgentHarness::Agy),
            (
                156,
                "claude",
                "/usr/bin/claude",
                crate::agent::AgentHarness::Claude,
            ),
            (
                157,
                "codex",
                "/usr/bin/codex",
                crate::agent::AgentHarness::Codex,
            ),
            (
                158,
                "gemini",
                "/usr/bin/gemini",
                crate::agent::AgentHarness::Gemini,
            ),
            (
                159,
                "opencode",
                "/usr/bin/opencode",
                crate::agent::AgentHarness::Opencode,
            ),
        ];

        for (pane_id, title, process_name, harness) in cases {
            let tab = Arc::new(Tab::new(&size));
            let pane = FakePane::new_detected(
                pane_id,
                size,
                domain.id,
                title,
                &format!("/tmp/confirmed-{title}"),
                process_name,
                &[title],
            );
            tab.assign_pane(&pane);
            mux.add_tab_and_active_pane(&tab).unwrap();
            mux.add_tab_to_window(&tab, window_id).unwrap();
            mux.record_agent_output(pane_id);

            let mut runtime = mux
                .agent_runtime_by_pane
                .read()
                .get(&pane_id)
                .cloned()
                .expect("provider candidate runtime");
            runtime.harness = harness.clone();
            runtime.session_path = Some(format!("/tmp/{title}-session"));
            runtime.transport = crate::agent::AgentTransport::ObservedPty;
            mux.apply_agent_observer_update(AgentObserverUpdate {
                pane_id,
                generation: 1,
                runtime,
                queue_delay: Duration::ZERO,
                refresh_elapsed: Duration::ZERO,
                schedule_trailing_refresh: false,
            });

            let metadata = mux
                .get_agent_metadata_for_pane(pane_id)
                .expect("confirmed harness metadata");
            assert_eq!(infer_harness(&metadata.launch_cmd, None), harness);
            assert_eq!(metadata.adopted_pid, Some(1));
            assert_eq!(metadata.adopted_start_time, Some(1));
        }
    }

    #[test]
    fn filesystem_artifact_event_triggers_detected_agent_adoption() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let executor = promise::spawn::SimpleExecutor::new();
        let _env_lock = ENV_LOCK.lock().unwrap();
        let temp = TempDir::new().unwrap();
        unsafe {
            std::env::set_var("WAKTERM_AGENT_CODEX_DIR", temp.path());
        }
        let _config = TestConfigGuard::new_with_auto_adopt("attention", "🤖 ", true);

        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };
        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
        let tab = Arc::new(Tab::new(&size));
        let pane = FakePane::new_detected(
            157,
            size,
            domain.id,
            "codex",
            "/tmp/filesystem-watcher-project",
            "/usr/bin/codex",
            &["codex"],
        );
        let pane_id = pane.pane_id();
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();

        let session_dir = temp.path().join("2026").join("03").join("21");
        std::fs::create_dir_all(&session_dir).unwrap();
        let session = session_dir.join("rollout-filesystem-watcher.jsonl");
        mux.record_agent_output(pane_id);
        assert!(mux.agent_adoption_candidates.read().contains_key(&pane_id));
        wait_for_main_thread_work(
            &executor,
            || {
                mux.agent_runtime_by_pane
                    .read()
                    .get(&pane_id)
                    .and_then(|runtime| runtime.last_harness_refresh_at)
                    .is_some()
            },
            "initial observer refresh before filesystem event",
        );

        std::fs::write(
            &session,
            concat!(
                "{\"payload\":{\"cwd\":\"/tmp/filesystem-watcher-project\"}}\n",
                "{\"type\":\"event_msg\",\"timestamp\":\"2026-03-21T12:00:00Z\",\"payload\":{\"type\":\"task_started\",\"collaboration_mode_kind\":\"default\"}}\n",
                "{\"type\":\"event_msg\",\"timestamp\":\"2026-03-21T12:00:01Z\",\"payload\":{\"type\":\"agent_message\",\"phase\":\"commentary\"}}\n"
            ),
        )
        .unwrap();

        wait_for_main_thread_work(
            &executor,
            || mux.get_agent_metadata_for_pane(pane_id).is_some(),
            "filesystem watcher detected agent adoption",
        );

        assert_eq!(
            mux.get_agent_metadata_for_pane(pane_id)
                .and_then(|metadata| metadata.adopted_pid),
            Some(1)
        );
        unsafe {
            std::env::remove_var("WAKTERM_AGENT_CODEX_DIR");
        }
    }

    #[test]
    fn filesystem_artifact_event_refreshes_adopted_agent() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let executor = promise::spawn::SimpleExecutor::new();
        let _env_lock = ENV_LOCK.lock().unwrap();
        let temp = TempDir::new().unwrap();
        unsafe {
            std::env::set_var("WAKTERM_AGENT_CODEX_DIR", temp.path());
        }

        let session_dir = temp.path().join("2026").join("08").join("24");
        std::fs::create_dir_all(&session_dir).unwrap();
        let session = session_dir.join("rollout-adopted-event-stream.jsonl");
        std::fs::write(
            &session,
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"session-adopted-stream\",\"cwd\":\"/tmp/adopted-event-stream\"}}\n",
                "{\"ordinal\":2,\"type\":\"event_msg\",\"timestamp\":\"2026-08-24T00:40:20Z\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"turn-live\"}}\n",
                "{\"ordinal\":3,\"type\":\"response_item\",\"timestamp\":\"2026-08-24T00:40:24Z\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"done live\"}],\"internal_chat_message_metadata_passthrough\":{\"turn_id\":\"turn-live\"}}}\n",
                "{\"ordinal\":4,\"type\":\"event_msg\",\"timestamp\":\"2026-08-24T00:40:24Z\",\"payload\":{\"type\":\"task_complete\",\"turn_id\":\"turn-live\",\"last_agent_message\":\"done live\"}}\n"
            ),
        )
        .unwrap();

        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };
        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
        let tab = Arc::new(Tab::new(&size));
        let pane = FakePane::new_detected(
            158,
            size,
            domain.id,
            "codex",
            "/tmp/adopted-event-stream",
            "/usr/bin/codex",
            &["codex"],
        );
        let pane_id = pane.pane_id();
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();
        let metadata = AgentMetadata {
            agent_id: "agent-adopted-event-stream".to_string(),
            name: "adopted_event_stream".to_string(),
            launch_cmd: "codex".to_string(),
            declared_cwd: "/tmp/adopted-event-stream".to_string(),
            adopted_pid: Some(1),
            adopted_start_time: Some(1),
            created_at: Utc::now(),
            repo_root: None,
            worktree: None,
            branch: None,
            managed_checkout: false,
            codex_app_server: None,
        };
        let mut runtime = AgentRuntimeSnapshot::new(&metadata);
        runtime.harness = crate::agent::AgentHarness::Codex;
        runtime.transport = crate::agent::AgentTransport::ObservedPty;
        runtime.session_path = Some(session.to_string_lossy().into_owned());
        runtime.alive = true;
        mux.install_agent_metadata_runtime_without_process_identity(pane_id, metadata, runtime)
            .unwrap();
        mux.handle_agent_artifact_event(vec![session.clone()]);

        wait_for_main_thread_work(
            &executor,
            || {
                mux.agent_runtime_by_pane
                    .read()
                    .get(&pane_id)
                    .and_then(|runtime| runtime.observed_turn.as_ref())
                    .is_some_and(|turn| turn.provider_turn_id == "turn-live")
            },
            "adopted artifact watcher observation",
        );
        let runtime = mux
            .agent_runtime_by_pane
            .read()
            .get(&pane_id)
            .cloned()
            .expect("adopted runtime after artifact hint");
        let observed_turn = runtime.observed_turn.expect("completed observed turn");
        assert_eq!(observed_turn.provider_turn_id, "turn-live");
        assert_eq!(
            observed_turn.outcome,
            crate::agent::AgentObservedTurnOutcome::Completed
        );
        assert_eq!(observed_turn.final_message.as_deref(), Some("done live"));

        unsafe {
            std::env::remove_var("WAKTERM_AGENT_CODEX_DIR");
        }
    }

    #[test]
    fn confirmed_detected_sessions_can_auto_adopt() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let executor = promise::spawn::SimpleExecutor::new();
        let _env_lock = ENV_LOCK.lock().unwrap();
        let temp = TempDir::new().unwrap();
        let day = Utc::now();
        let dir = temp
            .path()
            .join(format!("{:04}", day.year_ce().1))
            .join(format!("{:02}", day.month()))
            .join(format!("{:02}", day.day()));
        std::fs::create_dir_all(&dir).unwrap();
        let session = dir.join("rollout-auto-adopt.jsonl");
        std::fs::write(
            &session,
            concat!(
                "{\"payload\":{\"cwd\":\"/tmp/auto-adopt-project\"}}\n",
                "{\"type\":\"event_msg\",\"timestamp\":\"2026-03-17T12:00:00Z\",\"payload\":{\"type\":\"task_started\",\"collaboration_mode_kind\":\"default\"}}\n",
                "{\"type\":\"response_item\",\"timestamp\":\"2026-03-17T12:00:00Z\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[]}}\n",
                "{\"type\":\"event_msg\",\"timestamp\":\"2026-03-17T12:00:02Z\",\"payload\":{\"type\":\"agent_message\",\"phase\":\"final_answer\"}}\n",
                "{\"type\":\"response_item\",\"timestamp\":\"2026-03-17T12:00:03Z\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"done\"}]}}\n",
                "{\"type\":\"event_msg\",\"timestamp\":\"2026-03-17T12:00:04Z\",\"payload\":{\"type\":\"task_complete\",\"last_agent_message\":\"done\"}}\n"
            ),
        )
        .unwrap();
        unsafe {
            std::env::set_var("WAKTERM_AGENT_CODEX_DIR", temp.path());
        }

        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;
        let _config = TestConfigGuard::new_with_auto_adopt("attention", "🤖 ", true);

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };

        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
        let tab = Arc::new(Tab::new(&size));
        let pane = FakePane::new_detected(
            146,
            size,
            domain.id,
            "codex",
            "/tmp/auto-adopt-project",
            "/usr/bin/codex",
            &["codex"],
        );
        let pane_id = pane.pane_id();
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();

        let initial_agents = mux.list_agents();
        assert_eq!(initial_agents.len(), 1);
        assert!(matches!(initial_agents[0].origin, AgentOrigin::Detected));
        assert_eq!(
            mux.effective_tab_titles_for_window(window_id)
                .get(&tab.tab_id())
                .map(String::as_str),
            Some("auto-adopt-project")
        );
        wait_for_main_thread_work(
            &executor,
            || mux.get_agent_metadata_for_pane(pane_id).is_some(),
            "detected agent auto-adoption",
        );
        let agents = mux.list_agents();
        unsafe {
            std::env::remove_var("WAKTERM_AGENT_CODEX_DIR");
        }
        let session_path = session.to_string_lossy().to_string();

        assert_eq!(agents.len(), 1);
        let agent = &agents[0];
        assert!(matches!(agent.origin, AgentOrigin::Adopted));
        assert_eq!(agent.metadata.name, "auto_adopt_project_codex");
        assert_eq!(
            agent.runtime.session_path.as_deref(),
            Some(session_path.as_str())
        );
        assert!(mux.get_agent_metadata_for_pane(pane_id).is_some());
    }

    #[test]
    fn auto_adopt_preserves_confirmed_runtime_until_async_refresh() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let executor = promise::spawn::SimpleExecutor::new();
        let _env_lock = ENV_LOCK.lock().unwrap();
        let temp = TempDir::new().unwrap();
        let day = Utc::now();
        let dir = temp
            .path()
            .join(format!("{:04}", day.year_ce().1))
            .join(format!("{:02}", day.month()))
            .join(format!("{:02}", day.day()));
        std::fs::create_dir_all(&dir).unwrap();
        let session = dir.join("rollout-auto-adopt-preserve.jsonl");
        std::fs::write(
            &session,
            concat!(
                "{\"payload\":{\"cwd\":\"/tmp/auto-adopt-preserve-project\"}}\n",
                "{\"type\":\"event_msg\",\"timestamp\":\"2026-03-17T12:00:00Z\",\"payload\":{\"type\":\"task_started\",\"collaboration_mode_kind\":\"default\"}}\n",
                "{\"type\":\"response_item\",\"timestamp\":\"2026-03-17T12:00:00Z\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[]}}\n",
                "{\"type\":\"event_msg\",\"timestamp\":\"2026-03-17T12:00:02Z\",\"payload\":{\"type\":\"agent_message\",\"phase\":\"final_answer\"}}\n",
                "{\"type\":\"response_item\",\"timestamp\":\"2026-03-17T12:00:03Z\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"done\"}]}}\n",
                "{\"type\":\"event_msg\",\"timestamp\":\"2026-03-17T12:00:04Z\",\"payload\":{\"type\":\"task_complete\",\"last_agent_message\":\"done\"}}\n"
            ),
        )
        .unwrap();
        unsafe {
            std::env::set_var("WAKTERM_AGENT_CODEX_DIR", temp.path());
        }

        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;
        let _config = TestConfigGuard::new_with_auto_adopt("attention", "🤖 ", false);

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };

        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
        let tab = Arc::new(Tab::new(&size));
        let pane = FakePane::new_detected(
            147,
            size,
            domain.id,
            "codex",
            "/tmp/auto-adopt-preserve-project",
            "/usr/bin/codex",
            &["codex"],
        );
        let pane_id = pane.pane_id();
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();

        let initial_agents = mux.list_agents();
        assert_eq!(initial_agents.len(), 1);
        assert!(matches!(initial_agents[0].origin, AgentOrigin::Detected));

        wait_for_main_thread_work(
            &executor,
            || {
                mux.agent_runtime_by_pane
                    .read()
                    .get(&pane_id)
                    .and_then(|runtime| runtime.session_path.as_deref())
                    .is_some()
                    && mux.get_agent_metadata_for_pane(pane_id).is_none()
            },
            "confirmed detected session",
        );

        let preserved_session = mux
            .agent_runtime_by_pane
            .read()
            .get(&pane_id)
            .and_then(|runtime| runtime.session_path.clone())
            .expect("confirmed detected session path");
        std::fs::remove_file(&session).unwrap();

        let mut config = config::Config::default();
        config.agent_tab_badge_mode = "attention".to_string();
        config.agent_tab_badge = "🤖 ".to_string();
        config.agent_auto_adopt_on_confirmed_session_match = true;
        config::use_this_configuration(config);

        mux.maybe_auto_adopt_detected_agent(pane_id);

        assert!(mux.get_agent_metadata_for_pane(pane_id).is_some());
        let runtime = mux
            .agent_runtime_by_pane
            .read()
            .get(&pane_id)
            .cloned()
            .expect("adopted runtime");
        assert_eq!(
            runtime.session_path.as_deref(),
            Some(preserved_session.as_str())
        );
        assert_eq!(runtime.transport, crate::agent::AgentTransport::ObservedPty);

        unsafe {
            std::env::remove_var("WAKTERM_AGENT_CODEX_DIR");
        }
    }

    #[test]
    fn list_agents_promotes_confirmed_detected_agent_to_adopted() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let mut config = config::Config::default();
        config.agent_auto_adopt_on_confirmed_session_match = true;
        config::use_this_configuration(config);

        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };

        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
        let tab = Arc::new(Tab::new(&size));
        let pane = FakePane::new_detected(
            148,
            size,
            domain.id,
            "codex",
            "/tmp/list-agents-inline-adopt",
            "/usr/bin/codex",
            &["codex"],
        );
        let pane_id = pane.pane_id();
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();

        mux.detected_agent_panes.write().insert(pane_id);
        mux.agent_runtime_by_pane.write().insert(
            pane_id,
            AgentRuntimeSnapshot {
                harness: crate::agent::AgentHarness::Codex,
                transport: crate::agent::AgentTransport::ObservedPty,
                status: crate::agent::AgentStatus::Starting,
                turn_state: crate::agent::AgentTurnState::WaitingOnUser,
                alive: true,
                foreground_process_name: Some("/usr/bin/codex".to_string()),
                tty_name: Some("/dev/pts/1".to_string()),
                last_input_at: None,
                last_output_at: None,
                last_progress_at: None,
                last_turn_completed_at: Some(Utc.with_ymd_and_hms(2026, 3, 29, 18, 39, 0).unwrap()),
                observed_turn: None,
                observed_at: Utc.with_ymd_and_hms(2026, 3, 29, 18, 39, 8).unwrap(),
                session_path: Some("/tmp/codex-session.jsonl".to_string()),
                progress_summary: Some("done".to_string()),
                harness_mode: Some("default".to_string()),
                turn_phase: Some("final_answer".to_string()),
                attention_reason: None,
                terminal_progress: wakterm_term::Progress::None,
                observer_error: None,
                observer_started_at: None,
                last_harness_refresh_at: None,
            },
        );

        let agents = mux.list_agents();
        assert_eq!(agents.len(), 1);
        let agent = &agents[0];
        assert!(matches!(agent.origin, AgentOrigin::Adopted));
        assert_eq!(agent.pane_id, pane_id);
        assert_eq!(agent.metadata.name, "list_agents_inline_adopt_codex");
        assert_eq!(agent.metadata.declared_cwd, "/tmp/list-agents-inline-adopt");
        assert_eq!(
            agent.runtime.session_path.as_deref(),
            Some("/tmp/codex-session.jsonl")
        );
        assert!(mux.get_agent_metadata_for_pane(pane_id).is_some());
    }

    #[test]
    fn auto_adopt_notifies_tab_title_changed() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let mut config = config::Config::default();
        config.agent_auto_adopt_on_confirmed_session_match = true;
        config::use_this_configuration(config);

        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };

        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
        let tab = Arc::new(Tab::new(&size));
        let pane = FakePane::new_detected(
            149,
            size,
            domain.id,
            "codex",
            "/tmp/auto-adopt-notify",
            "/usr/bin/codex",
            &["codex"],
        );
        let pane_id = pane.pane_id();
        let tab_id = tab.tab_id();
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();

        let title_changes = std::sync::Arc::new(Mutex::new(0usize));
        let title_changes_for_sub = std::sync::Arc::clone(&title_changes);
        mux.subscribe(move |notification| {
            if matches!(notification, MuxNotification::TabTitleChanged { tab_id: changed, .. } if changed == tab_id) {
                *title_changes_for_sub.lock() += 1;
            }
            true
        });

        mux.agent_runtime_by_pane.write().insert(
            pane_id,
            AgentRuntimeSnapshot {
                harness: crate::agent::AgentHarness::Codex,
                transport: crate::agent::AgentTransport::ObservedPty,
                status: crate::agent::AgentStatus::Starting,
                turn_state: crate::agent::AgentTurnState::WaitingOnUser,
                alive: true,
                foreground_process_name: Some("/usr/bin/codex".to_string()),
                tty_name: Some("/dev/pts/1".to_string()),
                last_input_at: None,
                last_output_at: None,
                last_progress_at: None,
                last_turn_completed_at: Some(Utc.with_ymd_and_hms(2026, 3, 29, 18, 39, 0).unwrap()),
                observed_turn: None,
                observed_at: Utc.with_ymd_and_hms(2026, 3, 29, 18, 39, 8).unwrap(),
                session_path: Some("/tmp/codex-session.jsonl".to_string()),
                progress_summary: Some("done".to_string()),
                harness_mode: Some("default".to_string()),
                turn_phase: Some("final_answer".to_string()),
                attention_reason: None,
                terminal_progress: wakterm_term::Progress::None,
                observer_error: None,
                observer_started_at: None,
                last_harness_refresh_at: None,
            },
        );

        mux.list_agents();

        assert!(mux.get_agent_metadata_for_pane(pane_id).is_some());
        assert_eq!(*title_changes.lock(), 1);
    }

    #[test]
    fn agent_folder_title_does_not_set_an_explicit_tab_name() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        config::use_test_configuration();
        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };
        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
        let tab = Arc::new(Tab::new(&size));
        let pane = FakePane::new(243, size, domain.id);
        let pane_id = pane.pane_id();
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();

        tab.set_title_from_terminal("code");
        assert_eq!(tab.get_title(), "code");
        assert_eq!(tab.get_explicit_title(), None);
        assert_eq!(mux.raw_tab_title(tab.tab_id()), "");

        let mut metadata = sample_agent_metadata("codex");
        metadata.declared_cwd = "file:///code/testytest".to_string();
        mux.set_agent_metadata(pane_id, metadata).unwrap();
        assert_eq!(
            mux.agent_folder_title_for_pane(pane_id).as_deref(),
            Some("testytest")
        );
        assert_eq!(mux.effective_tab_title(tab.tab_id()), "");

        tab.set_title("review");
        assert_eq!(tab.get_explicit_title().as_deref(), Some("review"));
        assert_eq!(mux.raw_tab_title(tab.tab_id()), "review");
        assert_eq!(mux.effective_tab_title(tab.tab_id()), "review");

        tab.set_title("");
        assert_eq!(tab.get_explicit_title(), None);
        assert_eq!(mux.effective_tab_title(tab.tab_id()), "");
    }

    #[test]
    fn mirrored_agent_identity_is_available_from_metadata_or_snapshot() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        config::use_test_configuration();
        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };
        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
        let tab = Arc::new(Tab::new(&size));
        let pane = FakePane::new(244, size, domain.id);
        let pane_id = pane.pane_id();
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();

        let mut metadata = sample_agent_metadata("remote_codex");
        metadata.declared_cwd = "file:///code/testytest".to_string();
        mux.set_mirrored_agent_metadata(pane_id, Some(&metadata));
        assert_eq!(
            mux.agent_folder_title_for_pane(pane_id).as_deref(),
            Some("testytest")
        );
        assert_eq!(mux.raw_tab_title(tab.tab_id()), "");

        mux.set_mirrored_agent_badge(
            tab.tab_id(),
            AgentTabBadgeState {
                waiting_on_user: true,
                needs_attention: true,
            },
        );
        let _config = TestConfigGuard::new("attention", "🤖 ");
        assert!(
            mux.tab_badge_state_for_current_identity(tab.tab_id())
                .needs_attention
        );
        assert_eq!(
            mux.visible_harness_icons_for_tab(tab.tab_id(), None).len(),
            1
        );

        mux.set_mirrored_agent_metadata(pane_id, None);
        assert_eq!(mux.agent_folder_title_for_pane(pane_id), None);

        let runtime = AgentRuntimeSnapshot::new(&metadata);
        mux.set_mirrored_agent_snapshot(
            pane_id,
            Some(AgentSnapshot {
                metadata,
                runtime,
                pane_id,
                tab_id: tab.tab_id(),
                window_id,
                workspace: DEFAULT_WORKSPACE.to_string(),
                domain_id: domain.id,
                origin: AgentOrigin::Adopted,
                detection_source: None,
                needs_attention: true,
            }),
        );
        assert_eq!(
            mux.agent_folder_title_for_pane(pane_id).as_deref(),
            Some("testytest")
        );
        assert_eq!(
            mux.visible_harness_icons_for_tab(tab.tab_id(), None).len(),
            1
        );
    }

    #[test]
    fn automatic_tab_title_aggregates_harness_projects_independent_of_active_pane() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        config::use_test_configuration();
        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };
        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
        let tab = Arc::new(Tab::new(&size));
        let first_pane = FakePane::new(245, size, domain.id);
        let second_pane = FakePane::new(246, size, domain.id);
        tab.assign_pane(&first_pane);
        let second_index = tab
            .split_and_insert(
                0,
                SplitRequest {
                    direction: crate::tab::SplitDirection::Horizontal,
                    target_is_second: true,
                    size: crate::tab::SplitSize::Percent(50),
                    top_level: false,
                },
                second_pane.clone(),
            )
            .unwrap();
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_pane(&second_pane).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();

        let mut first = sample_agent_metadata("planner");
        first.declared_cwd = "file:///code/frontend".to_string();
        let mut second = sample_agent_metadata("reviewer");
        second.declared_cwd = "file:///code/backend".to_string();
        mux.set_mirrored_agent_metadata(first_pane.pane_id(), Some(&first));

        assert_eq!(
            mux.agent_folder_title_for_pane(first_pane.pane_id())
                .as_deref(),
            Some("frontend")
        );
        tab.set_active_idx_no_notify(second_index);
        assert_eq!(
            mux.effective_tab_titles_for_window(window_id)
                .get(&tab.tab_id())
                .map(String::as_str),
            Some("frontend")
        );

        let mut same_project = second.clone();
        same_project.declared_cwd = first.declared_cwd.clone();
        mux.set_mirrored_agent_metadata(second_pane.pane_id(), Some(&same_project));
        assert_eq!(
            mux.effective_tab_titles_for_window(window_id)
                .get(&tab.tab_id())
                .map(String::as_str),
            Some("frontend")
        );

        mux.set_mirrored_agent_metadata(second_pane.pane_id(), Some(&second));
        assert_eq!(
            mux.agent_folder_title_for_pane(second_pane.pane_id())
                .as_deref(),
            Some("backend")
        );
        assert_eq!(
            mux.effective_tab_titles_for_window(window_id)
                .get(&tab.tab_id())
                .map(String::as_str),
            Some("frontend+backend")
        );

        let third_pane = FakePane::new(247, size, domain.id);
        tab.split_and_insert(
            second_index,
            SplitRequest {
                direction: crate::tab::SplitDirection::Vertical,
                target_is_second: true,
                size: crate::tab::SplitSize::Percent(50),
                top_level: false,
            },
            third_pane.clone(),
        )
        .unwrap();
        mux.add_pane(&third_pane).unwrap();
        let mut third = sample_agent_metadata("database");
        third.declared_cwd = "file:///code/database".to_string();
        mux.set_mirrored_agent_metadata(third_pane.pane_id(), Some(&third));
        assert_eq!(
            mux.display_tab_titles_for_window(window_id)
                .get(&tab.tab_id())
                .map(String::as_str),
            Some("frontend+2")
        );
    }

    #[test]
    fn automatic_agent_folder_titles_use_compact_collision_suffixes() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        config::use_test_configuration();
        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };
        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
        let mut tab_ids = Vec::new();
        for ordinal in 1..=3 {
            let tab = Arc::new(Tab::new(&size));
            let pane = FakePane::new(alloc_pane_id(), size, domain.id);
            let pane_id = pane.pane_id();
            tab.assign_pane(&pane);
            mux.add_tab_and_active_pane(&tab).unwrap();
            mux.add_tab_to_window(&tab, window_id).unwrap();
            let mut metadata = sample_agent_metadata(&format!("wakterm-{ordinal}"));
            metadata.declared_cwd = "file:///code/wakterm".to_string();
            mux.set_agent_metadata(pane_id, metadata).unwrap();
            tab_ids.push(tab.tab_id());
        }

        let titles = mux.effective_tab_titles_for_window(window_id);
        assert_eq!(titles.get(&tab_ids[0]).map(String::as_str), Some("wakterm"));
        assert_eq!(
            titles.get(&tab_ids[1]).map(String::as_str),
            Some("wakterm2")
        );
        assert_eq!(
            titles.get(&tab_ids[2]).map(String::as_str),
            Some("wakterm3")
        );

        mux.get_tab(tab_ids[2]).unwrap().set_title("wakterm2");
        let titles = mux.effective_tab_titles_for_window(window_id);
        assert_eq!(titles.get(&tab_ids[0]).map(String::as_str), Some("wakterm"));
        assert_eq!(
            titles.get(&tab_ids[1]).map(String::as_str),
            Some("wakterm3")
        );
        assert_eq!(
            titles.get(&tab_ids[2]).map(String::as_str),
            Some("wakterm2")
        );
    }

    #[test]
    fn effective_tab_title_badges_tabs_waiting_on_user() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        config::use_test_configuration();
        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };

        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
        let tab = Arc::new(Tab::new(&size));
        tab.set_title("🤖 🤖 scrape");
        let pane = FakePane::new(43, size, domain.id);
        let pane_id = pane.pane_id();
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();
        mux.set_agent_metadata(pane_id, sample_agent_metadata("scraper"))
            .unwrap();

        {
            let mut runtime_by_pane = mux.agent_runtime_by_pane.write();
            let runtime = runtime_by_pane.get_mut(&pane_id).unwrap();
            runtime.turn_state = crate::agent::AgentTurnState::WaitingOnUser;
            runtime.last_turn_completed_at =
                Some(Utc.with_ymd_and_hms(2026, 3, 18, 12, 0, 0).unwrap());
        }

        let _config = TestConfigGuard::new("turn", "🤖 ");
        // Known harness (Codex) → text badge suppressed, icon takes over
        assert_eq!(mux.effective_tab_title(tab.tab_id()), "scrape");
        // But the icon should be visible
        let icons = mux.visible_harness_icons_for_tab(tab.tab_id(), None);
        assert_eq!(icons.len(), 1);
        assert!(matches!(icons[0], crate::agent::AgentHarness::Codex));
    }

    #[test]
    fn effective_tab_title_uses_text_badge_for_unknown_harness() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        config::use_test_configuration();
        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };

        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
        let tab = Arc::new(Tab::new(&size));
        tab.set_title("scrape");
        let pane = FakePane::new(143, size, domain.id);
        let pane_id = pane.pane_id();
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();
        // Use an unknown harness so the text badge is the only indicator
        let mut metadata = sample_agent_metadata("scraper");
        metadata.launch_cmd = "unknown-tool".to_string();
        mux.set_agent_metadata(pane_id, metadata).unwrap();

        {
            let mut runtime_by_pane = mux.agent_runtime_by_pane.write();
            let runtime = runtime_by_pane.get_mut(&pane_id).unwrap();
            runtime.turn_state = crate::agent::AgentTurnState::WaitingOnUser;
            runtime.last_turn_completed_at =
                Some(Utc.with_ymd_and_hms(2026, 3, 18, 12, 0, 0).unwrap());
        }

        // Unknown harness → text badge is used as fallback
        let _config = TestConfigGuard::new("turn", "🤖 ");
        assert_eq!(mux.effective_tab_title(tab.tab_id()), "🤖 scrape");
        assert_eq!(
            mux.effective_tab_titles_for_window(window_id)
                .get(&tab.tab_id())
                .map(String::as_str),
            Some("scrape")
        );
        assert_eq!(
            mux.display_tab_titles_for_window(window_id)
                .get(&tab.tab_id())
                .map(String::as_str),
            Some("🤖 scrape")
        );
        // No harness icon available
        assert!(mux
            .visible_harness_icons_for_tab(tab.tab_id(), None)
            .is_empty());

        // With empty badge text → no badge at all
        let _config2 = TestConfigGuard::new("turn", "");
        assert_eq!(mux.effective_tab_title(tab.tab_id()), "scrape");
    }

    #[test]
    fn attention_badge_clears_globally_when_any_client_focuses_agent() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        config::use_test_configuration();
        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;
        let _config = TestConfigGuard::new("attention", "🤖 ");

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };

        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
        let tab = Arc::new(Tab::new(&size));
        tab.set_title("scrape");
        let pane = FakePane::new(44, size, domain.id);
        let pane_id = pane.pane_id();
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();
        mux.set_agent_metadata(pane_id, sample_agent_metadata("scraper"))
            .unwrap();

        {
            let mut runtime_by_pane = mux.agent_runtime_by_pane.write();
            let runtime = runtime_by_pane.get_mut(&pane_id).unwrap();
            runtime.turn_state = crate::agent::AgentTurnState::WaitingOnUser;
            runtime.last_turn_completed_at =
                Some(Utc.with_ymd_and_hms(2026, 3, 18, 12, 30, 0).unwrap());
        }

        let client_a = Arc::new(ClientId::new());
        let view_a = Arc::new(ClientViewId("view-a".to_string()));
        mux.register_client(client_a.clone(), view_a.clone());
        mux.set_active_tab_for_client_view(view_a.as_ref(), window_id, tab.tab_id())
            .unwrap();

        let client_b = Arc::new(ClientId::new());
        let view_b = Arc::new(ClientViewId("view-b".to_string()));
        mux.register_client(client_b.clone(), view_b.clone());
        mux.set_active_tab_for_client_view(view_b.as_ref(), window_id, tab.tab_id())
            .unwrap();

        // Known harness → text badge suppressed, but icons reflect attention state
        assert_eq!(
            mux.effective_tab_title_for_view(view_a.as_ref(), tab.tab_id()),
            "scrape"
        );
        assert_eq!(
            mux.effective_tab_title_for_view(view_b.as_ref(), tab.tab_id()),
            "scrape"
        );
        // Both views see the icon (attention mode, neither has acknowledged)
        assert_eq!(
            mux.visible_harness_icons_for_tab(tab.tab_id(), Some(view_a.as_ref()))
                .len(),
            1
        );
        assert_eq!(
            mux.visible_harness_icons_for_tab(tab.tab_id(), Some(view_b.as_ref()))
                .len(),
            1
        );

        mux.set_focused_pane_for_client(client_a.as_ref(), pane_id)
            .unwrap();
        mux.acknowledge_agent_attention(pane_id);

        // Review acknowledgement is shared across clients.
        assert_eq!(
            mux.visible_harness_icons_for_tab(tab.tab_id(), Some(view_a.as_ref()))
                .len(),
            0
        );
        assert_eq!(
            mux.visible_harness_icons_for_tab(tab.tab_id(), Some(view_b.as_ref()))
                .len(),
            0
        );
    }

    #[test]
    fn attention_badge_clears_for_current_identity_focus_path() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        config::use_test_configuration();
        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;
        let _config = TestConfigGuard::new("attention", "🤖 ");

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };

        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
        let tab = Arc::new(Tab::new(&size));
        tab.set_title("scrape");
        let pane = FakePane::new(144, size, domain.id);
        let pane_id = pane.pane_id();
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();
        mux.set_agent_metadata(pane_id, sample_agent_metadata("scraper"))
            .unwrap();

        {
            let mut runtime_by_pane = mux.agent_runtime_by_pane.write();
            let runtime = runtime_by_pane.get_mut(&pane_id).unwrap();
            runtime.turn_state = crate::agent::AgentTurnState::WaitingOnUser;
            runtime.last_turn_completed_at =
                Some(Utc.with_ymd_and_hms(2026, 3, 18, 13, 0, 0).unwrap());
        }

        let (client_id, view_id) = register_test_client(&mux, "focus-view");
        mux.set_active_tab_for_client_view(view_id.as_ref(), window_id, tab.tab_id())
            .unwrap();

        // Known harness → text badge suppressed, icon shows attention
        assert_eq!(
            mux.effective_tab_title_for_view(view_id.as_ref(), tab.tab_id()),
            "scrape"
        );
        assert_eq!(
            mux.visible_harness_icons_for_tab(tab.tab_id(), Some(view_id.as_ref()))
                .len(),
            1
        );

        let _identity = mux.with_identity(Some(client_id));
        mux.focus_pane_and_containing_tab(pane_id).unwrap();

        // After focusing, attention is acknowledged → icon hidden
        assert_eq!(
            mux.visible_harness_icons_for_tab(tab.tab_id(), Some(view_id.as_ref()))
                .len(),
            0
        );
    }

    #[test]
    fn agent_names_are_unique_across_panes_but_replaceable_on_same_pane() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };

        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);

        let tab_a = Arc::new(Tab::new(&size));
        let pane_a = FakePane::new(41, size, domain.id);
        tab_a.assign_pane(&pane_a);
        mux.add_tab_and_active_pane(&tab_a).unwrap();
        mux.add_tab_to_window(&tab_a, window_id).unwrap();

        let tab_b = Arc::new(Tab::new(&size));
        let pane_b = FakePane::new(42, size, domain.id);
        tab_b.assign_pane(&pane_b);
        mux.add_tab_and_active_pane(&tab_b).unwrap();
        mux.add_tab_to_window(&tab_b, window_id).unwrap();

        mux.set_agent_metadata(pane_a.pane_id(), sample_agent_metadata("alpha"))
            .unwrap();

        let err = mux
            .set_agent_metadata(pane_b.pane_id(), sample_agent_metadata("alpha"))
            .unwrap_err();
        assert!(err.to_string().contains("already assigned"));

        mux.set_agent_metadata(pane_a.pane_id(), sample_agent_metadata("beta"))
            .unwrap();
        let agents = mux.list_agents();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].metadata.name, "beta");
        assert_eq!(agents[0].pane_id, pane_a.pane_id());
    }

    #[test]
    fn spawn_tab_in_existing_window_uses_provided_size() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;
        let (client_id, _view_id) = register_test_client(&mux, "spawn-test");
        let _identity = mux.with_identity(Some(client_id));

        smol::block_on(async move {
            let window_builder = mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
            let window_id = *window_builder;

            let stale = TerminalSize {
                rows: 1,
                cols: 1,
                pixel_width: 8,
                pixel_height: 16,
                dpi: 96,
            };
            let stale_tab = Arc::new(Tab::new(&stale));
            stale_tab.assign_pane(&FakePane::new(1, stale, domain.id));
            mux.add_tab_and_active_pane(&stale_tab).unwrap();
            mux.add_tab_to_window(&stale_tab, window_id).unwrap();

            let desired = TerminalSize {
                rows: 40,
                cols: 120,
                pixel_width: 1200,
                pixel_height: 800,
                dpi: 96,
            };

            let (spawned_tab, _pane, spawned_window_id) = mux
                .spawn_tab_or_window(
                    Some(window_id),
                    config::keyassignment::SpawnTabDomain::DefaultDomain,
                    None,
                    None,
                    desired,
                    Some(1),
                    DEFAULT_WORKSPACE.to_string(),
                    None,
                )
                .await
                .unwrap();

            assert_eq!(spawned_window_id, window_id);
            assert_eq!(*domain.last_spawn_size.lock(), Some(desired));
            assert_eq!(stale_tab.get_size(), desired);
            assert_eq!(spawned_tab.get_size(), desired);
        });
    }

    #[test]
    fn spawn_tab_in_existing_window_uses_explicit_current_pane_without_client_view() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        smol::block_on(async move {
            let window_builder = mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
            let window_id = *window_builder;

            let stale = TerminalSize {
                rows: 1,
                cols: 1,
                pixel_width: 8,
                pixel_height: 16,
                dpi: 96,
            };
            let stale_tab = Arc::new(Tab::new(&stale));
            let source_pane = FakePane::new(1, stale, domain.id);
            stale_tab.assign_pane(&source_pane);
            mux.add_tab_and_active_pane(&stale_tab).unwrap();
            mux.add_tab_to_window(&stale_tab, window_id).unwrap();

            let desired = TerminalSize {
                rows: 40,
                cols: 120,
                pixel_width: 1200,
                pixel_height: 800,
                dpi: 96,
            };

            let (spawned_tab, _pane, spawned_window_id) = mux
                .spawn_tab_or_window(
                    Some(window_id),
                    config::keyassignment::SpawnTabDomain::DefaultDomain,
                    None,
                    None,
                    desired,
                    Some(1),
                    DEFAULT_WORKSPACE.to_string(),
                    None,
                )
                .await
                .unwrap();

            assert_eq!(spawned_window_id, window_id);
            assert_eq!(*domain.last_spawn_size.lock(), Some(desired));
            assert_eq!(stale_tab.get_size(), desired);
            assert_eq!(spawned_tab.get_size(), desired);
        });
    }

    #[test]
    fn spawn_tab_in_existing_window_requires_explicit_current_pane() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        smol::block_on(async move {
            let window_builder = mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
            let window_id = *window_builder;

            let stale = TerminalSize {
                rows: 1,
                cols: 1,
                pixel_width: 8,
                pixel_height: 16,
                dpi: 96,
            };
            let stale_tab = Arc::new(Tab::new(&stale));
            stale_tab.assign_pane(&FakePane::new(1, stale, domain.id));
            mux.add_tab_and_active_pane(&stale_tab).unwrap();
            mux.add_tab_to_window(&stale_tab, window_id).unwrap();

            let err = match mux
                .spawn_tab_or_window(
                    Some(window_id),
                    config::keyassignment::SpawnTabDomain::DefaultDomain,
                    None,
                    None,
                    stale,
                    None,
                    DEFAULT_WORKSPACE.to_string(),
                    None,
                )
                .await
            {
                Ok(_) => panic!("spawn_tab_or_window should require current_pane_id"),
                Err(err) => err,
            };

            assert!(err.to_string().contains("requires current_pane_id"));
        });
    }

    #[test]
    fn adopting_spawned_pane_does_not_consume_next_pane_id() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        smol::block_on(async move {
            let window_builder = mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);
            let window_id = *window_builder;

            let stale = TerminalSize {
                rows: 1,
                cols: 1,
                pixel_width: 8,
                pixel_height: 16,
                dpi: 96,
            };
            let stale_tab = Arc::new(Tab::new(&stale));
            let source_pane = FakePane::new(1, stale, domain.id);
            stale_tab.assign_pane(&source_pane);
            mux.add_tab_and_active_pane(&stale_tab).unwrap();
            mux.add_tab_to_window(&stale_tab, window_id).unwrap();

            let desired = TerminalSize {
                rows: 40,
                cols: 120,
                pixel_width: 1200,
                pixel_height: 800,
                dpi: 96,
            };

            let (_first_tab, first_pane, _) = mux
                .spawn_tab_or_window(
                    Some(window_id),
                    config::keyassignment::SpawnTabDomain::DefaultDomain,
                    None,
                    None,
                    desired,
                    Some(source_pane.pane_id()),
                    DEFAULT_WORKSPACE.to_string(),
                    None,
                )
                .await
                .unwrap();

            let first_pane_id = first_pane.pane_id();
            mux.set_agent_metadata(first_pane_id, sample_agent_metadata("adopted"))
                .unwrap();

            let (_second_tab, second_pane, _) = mux
                .spawn_tab_or_window(
                    Some(window_id),
                    config::keyassignment::SpawnTabDomain::DefaultDomain,
                    None,
                    None,
                    desired,
                    Some(first_pane_id),
                    DEFAULT_WORKSPACE.to_string(),
                    None,
                )
                .await
                .unwrap();

            assert_eq!(second_pane.pane_id(), first_pane_id + 1);
        });
    }

    #[test]
    fn client_views_keep_independent_active_tabs_in_same_window() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let (client_a, view_a) = register_test_client(&mux, "view-a");
        let (_client_b, view_b) = register_test_client(&mux, "view-b");

        let size = TerminalSize {
            rows: 40,
            cols: 120,
            pixel_width: 1200,
            pixel_height: 800,
            dpi: 96,
        };

        let _identity = mux.with_identity(Some(client_a));
        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);

        let tab_a = Arc::new(Tab::new(&size));
        tab_a.assign_pane(&FakePane::new(10, size, domain.id));
        mux.add_tab_and_active_pane(&tab_a).unwrap();
        mux.add_tab_to_window(&tab_a, window_id).unwrap();

        let tab_b = Arc::new(Tab::new(&size));
        tab_b.assign_pane(&FakePane::new(11, size, domain.id));
        mux.add_tab_and_active_pane(&tab_b).unwrap();
        mux.add_tab_to_window(&tab_b, window_id).unwrap();

        mux.set_active_tab_for_client_view(view_b.as_ref(), window_id, tab_b.tab_id())
            .unwrap();

        assert_eq!(
            mux.get_active_tab_for_window_for_client(view_a.as_ref(), window_id)
                .map(|tab| tab.tab_id()),
            Some(tab_a.tab_id())
        );
        assert_eq!(
            mux.get_active_tab_for_window_for_client(view_b.as_ref(), window_id)
                .map(|tab| tab.tab_id()),
            Some(tab_b.tab_id())
        );

        mux.set_active_tab_for_client_view(view_a.as_ref(), window_id, tab_b.tab_id())
            .unwrap();

        assert_eq!(
            mux.get_active_tab_for_window_for_client(view_a.as_ref(), window_id)
                .map(|tab| tab.tab_id()),
            Some(tab_b.tab_id())
        );
        assert_eq!(
            mux.get_active_tab_for_window_for_client(view_b.as_ref(), window_id)
                .map(|tab| tab.tab_id()),
            Some(tab_b.tab_id())
        );
    }

    #[test]
    fn removing_active_tab_reassigns_only_affected_view() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let (client_a, view_a) = register_test_client(&mux, "view-a");
        let (_client_b, view_b) = register_test_client(&mux, "view-b");

        let size = TerminalSize {
            rows: 30,
            cols: 100,
            pixel_width: 1000,
            pixel_height: 600,
            dpi: 96,
        };

        let _identity = mux.with_identity(Some(client_a));
        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);

        let tab_a = Arc::new(Tab::new(&size));
        tab_a.assign_pane(&FakePane::new(20, size, domain.id));
        mux.add_tab_and_active_pane(&tab_a).unwrap();
        mux.add_tab_to_window(&tab_a, window_id).unwrap();

        let tab_b = Arc::new(Tab::new(&size));
        tab_b.assign_pane(&FakePane::new(21, size, domain.id));
        mux.add_tab_and_active_pane(&tab_b).unwrap();
        mux.add_tab_to_window(&tab_b, window_id).unwrap();

        let tab_c = Arc::new(Tab::new(&size));
        tab_c.assign_pane(&FakePane::new(22, size, domain.id));
        mux.add_tab_and_active_pane(&tab_c).unwrap();
        mux.add_tab_to_window(&tab_c, window_id).unwrap();

        mux.set_active_tab_for_client_view(view_a.as_ref(), window_id, tab_b.tab_id())
            .unwrap();
        mux.set_active_tab_for_client_view(view_b.as_ref(), window_id, tab_c.tab_id())
            .unwrap();

        mux.remove_tab(tab_b.tab_id());

        assert_eq!(
            mux.get_active_tab_for_window_for_client(view_a.as_ref(), window_id)
                .map(|tab| tab.tab_id()),
            Some(tab_a.tab_id())
        );
        assert_eq!(
            mux.get_active_tab_for_window_for_client(view_b.as_ref(), window_id)
                .map(|tab| tab.tab_id()),
            Some(tab_c.tab_id())
        );
    }

    #[test]
    fn lightweight_focus_bookkeeping_moves_focus_only_for_current_identity() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };

        let (client_a, view_a) = register_test_client(&mux, "split-view-a");
        let (_client_b, view_b) = register_test_client(&mux, "split-view-b");

        let _identity = mux.with_identity(Some(client_a.clone()));
        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);

        let tab_a = Arc::new(Tab::new(&size));
        let pane_a = FakePane::new(50, size, domain.id);
        let pane_a_id = pane_a.pane_id();
        tab_a.assign_pane(&pane_a);
        mux.add_tab_and_active_pane(&tab_a).unwrap();
        mux.add_tab_to_window(&tab_a, window_id).unwrap();

        let tab_b = Arc::new(Tab::new(&size));
        let pane_b = FakePane::new(51, size, domain.id);
        let pane_b_id = pane_b.pane_id();
        tab_b.assign_pane(&pane_b);
        mux.add_tab_and_active_pane(&tab_b).unwrap();
        mux.add_tab_to_window(&tab_b, window_id).unwrap();

        mux.set_active_tab_for_client_view(view_a.as_ref(), window_id, tab_a.tab_id())
            .unwrap();
        mux.set_active_pane_for_client_view(view_a.as_ref(), window_id, tab_a.tab_id(), pane_a_id)
            .unwrap();
        mux.set_active_tab_for_client_view(view_b.as_ref(), window_id, tab_a.tab_id())
            .unwrap();
        mux.set_active_pane_for_client_view(view_b.as_ref(), window_id, tab_a.tab_id(), pane_a_id)
            .unwrap();

        mux.set_focused_pane_for_current_identity_lightweight(pane_b_id)
            .unwrap();

        let view_a_state = mux.client_window_view_state_for_view(view_a.as_ref());
        let view_b_state = mux.client_window_view_state_for_view(view_b.as_ref());
        assert_eq!(
            view_a_state
                .get(&window_id)
                .and_then(|window| window.tabs.get(&tab_b.tab_id()))
                .and_then(|tab| tab.active_pane_id),
            Some(pane_b_id)
        );
        assert_eq!(
            mux.resolve_focused_pane(client_a.as_ref())
                .map(|(_, _, _, pane_id)| pane_id),
            Some(pane_b_id)
        );
        assert_eq!(
            view_b_state
                .get(&window_id)
                .and_then(|window| window.tabs.get(&tab_a.tab_id()))
                .and_then(|tab| tab.active_pane_id),
            Some(pane_a_id)
        );
    }

    #[test]
    fn seed_active_focus_for_current_identity_does_not_invalidate_window() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };

        let (client_a, _view_a) = register_test_client(&mux, "seed-focus-view");
        let _identity = mux.with_identity(Some(client_a));
        let window_id = *mux.new_empty_window(Some(DEFAULT_WORKSPACE.to_string()), None);

        let tab = Arc::new(Tab::new(&size));
        let pane = FakePane::new(61, size, domain.id);
        let pane_id = pane.pane_id();
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();

        let invalidations = Arc::new(Mutex::new(0usize));
        let invalidations_for_sub = Arc::clone(&invalidations);
        mux.subscribe(move |notification| {
            if matches!(notification, MuxNotification::WindowInvalidated(id) if id == window_id) {
                *invalidations_for_sub.lock() += 1;
            }
            true
        });

        mux.seed_active_tab_for_current_identity(window_id, tab.tab_id())
            .unwrap();
        mux.seed_active_pane_for_current_identity(window_id, tab.tab_id(), pane_id)
            .unwrap();

        assert_eq!(*invalidations.lock(), 0);
    }

    #[test]
    fn session_save_notifications_exclude_ordinary_agent_activity() {
        assert!(notification_changes_saved_session(
            &MuxNotification::TabResized {
                tab_id: 1,
                origin: None,
            }
        ));
        assert!(notification_changes_saved_session(
            &MuxNotification::Alert {
                pane_id: 1,
                alert: wakterm_term::Alert::CurrentWorkingDirectoryChanged,
            }
        ));
        assert!(!notification_changes_saved_session(
            &MuxNotification::PaneOutput(1)
        ));
        assert!(!notification_changes_saved_session(
            &MuxNotification::Alert {
                pane_id: 1,
                alert: wakterm_term::Alert::Progress(wakterm_term::Progress::None),
            }
        ));
        assert!(!notification_changes_saved_session(
            &MuxNotification::WindowInvalidated(1)
        ));
        assert!(!notification_changes_saved_session(
            &MuxNotification::TabTitleChanged {
                tab_id: 1,
                title: "generated title".to_string(),
            }
        ));
    }

    #[test]
    fn pane_output_notifications_coalesce_while_pending() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let domain = Arc::new(FakeDomain::new());
        let mux = Arc::new(Mux::new(Some(Arc::clone(&domain) as Arc<dyn Domain>)));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        assert!(!mux.should_skip_queued_notification(&MuxNotification::PaneOutput(1)));
        assert!(mux.should_skip_queued_notification(&MuxNotification::PaneOutput(1)));
        assert!(!mux.should_skip_queued_notification(&MuxNotification::PaneOutput(2)));

        mux.clear_pending_notification(&MuxNotification::PaneOutput(1));

        assert!(!mux.should_skip_queued_notification(&MuxNotification::PaneOutput(1)));
        assert!(mux.should_skip_queued_notification(&MuxNotification::PaneOutput(1)));
    }

    /// Helper: spawn parse_buffered_data on a FakePane, return the write end
    /// and the pane_id so the caller can feed data and observe ACTION_BUFFER_SIZES.
    fn spawn_parser_thread(pane_id: PaneId) -> (FileDescriptor, Arc<dyn Pane>) {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };
        let pane = FakePane::new(pane_id, size, 0);
        let weak = Arc::downgrade(&pane);
        let (tx, rx) = filedescriptor::socketpair().expect("socketpair");
        let dead = Arc::new(AtomicBool::new(false));

        std::thread::spawn(move || {
            parse_buffered_data(weak, &dead, rx);
        });

        (tx, pane)
    }

    #[test]
    fn synchronized_output_accumulates_unbounded_actions() {
        // Reproduce: send CSI?2026h (enable SynchronizedOutput) then stream
        // output without ever sending CSI?2026l (disable). The action buffer
        // should grow without bound — this is the suspected OOM cause.
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let pane_id: PaneId = 9900;
        let (mut tx, _pane) = spawn_parser_thread(pane_id);

        // Enable SynchronizedOutput
        tx.write_all(b"\x1b[?2026h").unwrap();
        // Give the parser thread a moment to process
        std::thread::sleep(Duration::from_millis(50));

        // Verify the parser thread registered
        let registered = ACTION_BUFFER_SIZES.read().contains_key(&pane_id);
        assert!(
            registered,
            "parser thread did not register pane {} in ACTION_BUFFER_SIZES",
            pane_id
        );

        // Stream output while in synchronized mode — 100 rounds of 10KB each
        for _ in 0..100 {
            let data = vec![b'A'; 10_000];
            tx.write_all(&data).unwrap();
        }
        // Let the parser thread consume everything
        std::thread::sleep(Duration::from_millis(200));

        let buf_bytes = ACTION_BUFFER_SIZES
            .read()
            .get(&pane_id)
            .map(|a| a.load(Ordering::Relaxed))
            .unwrap_or(0);

        // We sent 1MB of data while SynchronizedOutput was held open.
        // The action_size counter tracks raw bytes parsed since last flush,
        // so it should reflect most of the 1MB.
        assert!(
            buf_bytes > 500_000,
            "expected >500KB buffered during SynchronizedOutput hold, got {} bytes",
            buf_bytes
        );

        // Now disable synchronized output and let it flush
        tx.write_all(b"\x1b[?2026l").unwrap();
        std::thread::sleep(Duration::from_millis(200));

        let buf_bytes_after = ACTION_BUFFER_SIZES
            .read()
            .get(&pane_id)
            .map(|a| a.load(Ordering::Relaxed))
            .unwrap_or(0);

        assert!(
            buf_bytes_after < 1_000,
            "expected buffer flushed after SynchronizedOutput reset, got {} bytes",
            buf_bytes_after
        );

        // Clean up
        drop(tx);
    }

    #[test]
    fn synchronized_output_capped_at_4mb() {
        // Verify the safety valve: even with SynchronizedOutput held open,
        // the buffer is force-flushed at 4MB to prevent OOM.
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let pane_id: PaneId = 9902;
        let (mut tx, _pane) = spawn_parser_thread(pane_id);

        // Enable SynchronizedOutput
        tx.write_all(b"\x1b[?2026h").unwrap();
        std::thread::sleep(Duration::from_millis(50));

        // Stream 8MB of data — well over the 4MB cap
        for _ in 0..80 {
            let data = vec![b'C'; 100_000];
            tx.write_all(&data).unwrap();
        }
        std::thread::sleep(Duration::from_millis(500));

        let buf_bytes = ACTION_BUFFER_SIZES
            .read()
            .get(&pane_id)
            .map(|a| a.load(Ordering::Relaxed))
            .unwrap_or(0);

        // The buffer should have been force-flushed and be well under 8MB.
        // It could be up to 4MB + one read chunk, but not the full 8MB.
        assert!(
            buf_bytes < 5 * 1024 * 1024,
            "expected buffer capped under 5MB, got {} bytes",
            buf_bytes
        );

        drop(tx);
    }

    #[test]
    fn normal_output_flushes_actions_promptly() {
        // Control test: without SynchronizedOutput, actions should be flushed
        // after each read cycle, so the buffer never grows large.
        let _test_lock = TEST_MUX_LOCK.lock();
        let _executor = promise::spawn::SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let _guard = TestMuxGuard;

        let pane_id = 9901;
        let (mut tx, _pane) = spawn_parser_thread(pane_id);

        // Stream 1MB of output in normal mode
        for _ in 0..100 {
            let data = vec![b'B'; 10_000];
            tx.write_all(&data).unwrap();
            // Small delay so the parser can flush between writes
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(200));

        let buf_bytes = ACTION_BUFFER_SIZES
            .read()
            .get(&pane_id)
            .map(|a| a.load(Ordering::Relaxed))
            .unwrap_or(0);

        // In normal mode, action_size resets to 0 after each flush,
        // so the buffer should be small (whatever's in-flight from the last read)
        assert!(
            buf_bytes < 100_000,
            "expected small action buffer in normal mode, got {} bytes",
            buf_bytes
        );

        drop(tx);
    }
}
