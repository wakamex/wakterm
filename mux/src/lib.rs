use crate::agent::{
    default_launch_cmd_for_harness, derive_runtime_status, detect_harness_process,
    finalize_runtime_snapshot, infer_harness, prime_runtime_for_new_agent,
    refresh_runtime_from_harness, AgentMetadata, AgentOrigin, AgentRuntimeSnapshot, AgentSnapshot,
    AgentTabBadgeState,
};
use crate::client::{ClientId, ClientInfo, ClientViewId, ClientViewState, ClientWindowViewState};
use crate::pane::{CachePolicy, Pane, PaneId};
use crate::ssh_agent::AgentProxy;
use crate::tab::{NotifyMux, SplitRequest, Tab, TabId};
use crate::window::{Window, WindowId};
use anyhow::{anyhow, Context, Error};
use chrono::{DateTime, Utc};
use config::keyassignment::SpawnTabDomain;
use config::{configuration, ExitBehavior, GuiPosition};
use domain::{Domain, DomainId, DomainState, SplitSource};
use filedescriptor::{poll, pollfd, socketpair, AsRawSocketDescriptor, FileDescriptor, POLLIN};
#[cfg(unix)]
use libc::{c_int, SOL_SOCKET, SO_RCVBUF, SO_SNDBUF};
use log::error;
use metrics::{counter, histogram};
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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
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
pub mod client;
pub mod connui;
pub mod domain;
pub mod localpane;
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
    TabResized(TabId),
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

static SUB_ID: AtomicUsize = AtomicUsize::new(0);

pub struct Mux {
    tabs: RwLock<HashMap<TabId, Arc<Tab>>>,
    panes: RwLock<HashMap<PaneId, Arc<dyn Pane>>>,
    agent_panes_by_name: RwLock<HashMap<String, PaneId>>,
    agent_metadata_by_pane: RwLock<HashMap<PaneId, Arc<AgentMetadata>>>,
    detected_agent_panes: RwLock<HashSet<PaneId>>,
    last_detected_agent_full_scan: Mutex<Option<Instant>>,
    agent_runtime_by_pane: RwLock<HashMap<PaneId, AgentRuntimeSnapshot>>,
    agent_observer_state_by_pane: RwLock<HashMap<PaneId, AgentObserverState>>,
    agent_attention_seen_by_view: RwLock<HashMap<ClientViewId, HashMap<PaneId, DateTime<Utc>>>>,
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
    agent_observer_tx: Sender<AgentObserverRequest>,
    agent: Option<AgentProxy>,
}

const BUFSIZE: usize = 1024 * 1024;
const AGENT_HARNESS_REFRESH_THROTTLE: Duration = Duration::from_millis(250);
const AGENT_DETECTED_FULL_SCAN_THROTTLE: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AgentTabBadgeMode {
    Attention,
    Turn,
    Off,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AgentRefreshPolicy {
    Throttled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InitialAgentRefresh {
    Sync,
    Async,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AgentTitleFingerprint {
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
    runtime: AgentRuntimeSnapshot,
    detection_source: String,
}

#[derive(Clone)]
struct AgentObserverRequest {
    pane_id: PaneId,
    generation: u64,
    requested_at: Instant,
    metadata: AgentMetadata,
    runtime: AgentRuntimeSnapshot,
}

struct AgentObserverUpdate {
    pane_id: PaneId,
    generation: u64,
    runtime: AgentRuntimeSnapshot,
    queue_delay: Duration,
    refresh_elapsed: Duration,
}

#[derive(Default)]
struct AgentObserverState {
    latest_generation: u64,
    inflight_generation: Option<u64>,
    pending_request: Option<AgentObserverRequest>,
    last_requested_at: Option<DateTime<Utc>>,
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

fn spawn_agent_observer_worker() -> Sender<AgentObserverRequest> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || run_agent_observer_worker(rx));
    tx
}

fn run_agent_observer_worker(rx: Receiver<AgentObserverRequest>) {
    let mut pending = HashMap::<PaneId, AgentObserverRequest>::new();

    loop {
        let request = match rx.recv() {
            Ok(request) => request,
            Err(_) => return,
        };
        pending.insert(request.pane_id, request);

        while let Ok(request) = rx.try_recv() {
            pending.insert(request.pane_id, request);
        }

        for request in pending.drain().map(|(_, request)| request) {
            counter!("mux.agent_observer.refresh.rate").increment(1);
            let started = Instant::now();
            let mut runtime = request.runtime;
            refresh_runtime_from_harness(&mut runtime, &request.metadata);
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
            };

            promise::spawn::spawn_into_main_thread(async move {
                if let Some(mux) = Mux::try_get() {
                    mux.apply_agent_observer_update(update);
                }
            })
            .detach();
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
        let agent_observer_tx = spawn_agent_observer_worker();

        Self {
            tabs: RwLock::new(HashMap::new()),
            panes: RwLock::new(HashMap::new()),
            agent_panes_by_name: RwLock::new(HashMap::new()),
            agent_metadata_by_pane: RwLock::new(HashMap::new()),
            detected_agent_panes: RwLock::new(HashSet::new()),
            last_detected_agent_full_scan: Mutex::new(None),
            agent_runtime_by_pane: RwLock::new(HashMap::new()),
            agent_observer_state_by_pane: RwLock::new(HashMap::new()),
            agent_attention_seen_by_view: RwLock::new(HashMap::new()),
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
            agent,
        }
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
        self.set_agent_metadata_with_initial_refresh(pane_id, metadata, InitialAgentRefresh::Sync)
    }

    pub fn restore_agent_metadata(
        &self,
        pane_id: PaneId,
        metadata: AgentMetadata,
    ) -> anyhow::Result<()> {
        self.set_agent_metadata_with_initial_refresh(pane_id, metadata, InitialAgentRefresh::Async)
    }

    fn install_agent_metadata_runtime(
        &self,
        pane_id: PaneId,
        metadata: AgentMetadata,
        runtime: AgentRuntimeSnapshot,
    ) -> anyhow::Result<()> {
        self.detected_agent_panes.write().remove(&pane_id);
        let pane = self
            .get_pane(pane_id)
            .ok_or_else(|| anyhow!("pane {} is invalid", pane_id))?;
        let foreground_process_name = pane.get_foreground_process_name(CachePolicy::AllowStale);
        let tty_name = pane.tty_name();
        let terminal_progress = pane.get_progress();
        let alive = !pane.is_dead();

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
        for seen in self.agent_attention_seen_by_view.write().values_mut() {
            seen.remove(&pane_id);
        }
        runtime.alive = alive;
        runtime.foreground_process_name = foreground_process_name.clone();
        runtime.tty_name = tty_name;
        runtime.terminal_progress = terminal_progress;
        runtime.harness = infer_harness(&metadata.launch_cmd, foreground_process_name.as_deref());
        self.agent_runtime_by_pane.write().insert(pane_id, runtime);
        metadata_by_pane.insert(pane_id, Arc::new(metadata));
        Ok(())
    }

    fn set_agent_metadata_with_initial_refresh(
        &self,
        pane_id: PaneId,
        metadata: AgentMetadata,
        initial_refresh: InitialAgentRefresh,
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

        match initial_refresh {
            InitialAgentRefresh::Sync => self.refresh_agent_runtime_for_pane(pane_id, true),
            InitialAgentRefresh::Async => self.refresh_agent_runtime_for_pane_with_update(
                pane_id,
                false,
                AgentRefreshPolicy::Throttled,
                |_| {},
            ),
        }
        if let Some(tab_id) = tab_id {
            self.notify_tab_title_changed(tab_id);
        }
        Ok(())
    }

    pub fn clear_agent_metadata(&self, pane_id: PaneId) -> Option<Arc<AgentMetadata>> {
        let tab_id = self.resolve_pane_id(pane_id).map(|(_, _, tab_id)| tab_id);
        let metadata = {
            let mut metadata_by_pane = self.agent_metadata_by_pane.write();
            metadata_by_pane.remove(&pane_id)?
        };
        self.agent_panes_by_name.write().remove(&metadata.name);
        self.agent_runtime_by_pane.write().remove(&pane_id);
        self.agent_observer_state_by_pane.write().remove(&pane_id);
        for seen in self.agent_attention_seen_by_view.write().values_mut() {
            seen.remove(&pane_id);
        }
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
                .and_then(|url| url.to_file_path().ok())
                .map(|path| path.to_string_lossy().to_string())
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
                return url
                    .to_file_path()
                    .ok()
                    .map(|path| path.to_string_lossy().to_string());
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
        self.detected_agent_panes.write().remove(&pane_id);
        if self.get_agent_metadata_for_pane(pane_id).is_none() {
            self.agent_runtime_by_pane.write().remove(&pane_id);
            self.agent_observer_state_by_pane.write().remove(&pane_id);
        }
    }

    fn detect_agent_state_for_pane(&self, pane_id: PaneId) -> Option<DetectedAgentState> {
        if self.get_agent_metadata_for_pane(pane_id).is_some() {
            self.detected_agent_panes.write().remove(&pane_id);
            return None;
        }

        let Some(pane) = self.get_pane(pane_id) else {
            self.clear_detected_agent_info(pane_id);
            return None;
        };
        let Some((_domain_id, window_id, tab_id)) = self.resolve_pane_id(pane_id) else {
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
            created_at: Utc::now(),
            repo_root: None,
            worktree: None,
            branch: None,
            managed_checkout: false,
        };
        let mut runtime = self
            .agent_runtime_by_pane
            .read()
            .get(&pane_id)
            .cloned()
            .unwrap_or_else(|| AgentRuntimeSnapshot::new(&metadata));
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
        self.schedule_agent_observer_refresh(
            pane_id,
            &metadata,
            &runtime,
            AgentRefreshPolicy::Throttled,
        );
        finalize_runtime_snapshot(&mut runtime);
        runtime.status = derive_runtime_status(&runtime);
        self.agent_runtime_by_pane
            .write()
            .insert(pane_id, runtime.clone());
        self.detected_agent_panes.write().insert(pane_id);

        Some(DetectedAgentState {
            pane_id,
            tab_id,
            window_id,
            workspace: window.get_workspace().to_string(),
            domain_id: pane.domain_id(),
            launch_cmd: metadata.launch_cmd,
            declared_cwd: metadata.declared_cwd,
            runtime,
            detection_source,
        })
    }

    fn detected_agent_snapshot_from_state(
        state: DetectedAgentState,
        name: String,
    ) -> AgentSnapshot {
        let created_at = Self::detected_agent_created_at(&state.runtime);
        AgentSnapshot {
            metadata: AgentMetadata {
                agent_id: format!("detected-pane-{}", state.pane_id),
                name,
                launch_cmd: state.launch_cmd,
                declared_cwd: state.declared_cwd,
                created_at,
                repo_root: None,
                worktree: None,
                branch: None,
                managed_checkout: false,
            },
            runtime: state.runtime,
            pane_id: state.pane_id,
            tab_id: state.tab_id,
            window_id: state.window_id,
            workspace: state.workspace,
            domain_id: state.domain_id,
            origin: AgentOrigin::Detected,
            detection_source: Some(state.detection_source),
        }
    }

    fn maybe_auto_adopt_detected_agent(&self, pane_id: PaneId) {
        if !Self::agent_auto_adopt_on_confirmed_session_match()
            || self.get_agent_metadata_for_pane(pane_id).is_some()
        {
            return;
        }

        let Some(state) = self.detect_agent_state_for_pane(pane_id) else {
            return;
        };
        if state.runtime.session_path.is_none() {
            return;
        }

        let taken_names = self
            .agent_panes_by_name
            .read()
            .keys()
            .cloned()
            .collect::<HashSet<_>>();
        let base_name = Self::detected_agent_name_base(&state.runtime.harness, &state.declared_cwd);
        let name = Self::next_available_agent_name(&taken_names, &base_name);
        let created_at = Self::detected_agent_created_at(&state.runtime);
        let metadata = AgentMetadata {
            agent_id: format!("detected-pane-{}", state.pane_id),
            name,
            launch_cmd: state.launch_cmd,
            declared_cwd: state.declared_cwd,
            created_at,
            repo_root: None,
            worktree: None,
            branch: None,
            managed_checkout: false,
        };
        if self
            .install_agent_metadata_runtime(pane_id, metadata, state.runtime)
            .is_ok()
        {
            self.refresh_agent_runtime_for_pane_with_update(
                pane_id,
                false,
                AgentRefreshPolicy::Throttled,
                |_| {},
            );
        }
    }

    fn should_refresh_harness_runtime(
        observer_state: &AgentObserverState,
        policy: AgentRefreshPolicy,
        now: DateTime<Utc>,
    ) -> bool {
        match policy {
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
        if self.agent_observer_tx.send(request).is_err() {
            log::error!("agent observer worker is no longer available");
        }
    }

    fn note_sync_agent_observer_refresh(&self, pane_id: PaneId, refreshed_at: DateTime<Utc>) {
        let mut observer_state_by_pane = self.agent_observer_state_by_pane.write();
        let observer_state = observer_state_by_pane.entry(pane_id).or_default();
        observer_state.last_requested_at = Some(refreshed_at);
        observer_state.pending_request = None;
        observer_state.inflight_generation = None;
    }

    fn schedule_agent_observer_refresh(
        &self,
        pane_id: PaneId,
        metadata: &AgentMetadata,
        runtime: &AgentRuntimeSnapshot,
        refresh_policy: AgentRefreshPolicy,
    ) {
        let now = Utc::now();
        let request = {
            let mut observer_state_by_pane = self.agent_observer_state_by_pane.write();
            let observer_state = observer_state_by_pane.entry(pane_id).or_default();
            if !Self::should_refresh_harness_runtime(observer_state, refresh_policy, now) {
                counter!("mux.agent_observer.refresh.skipped.rate").increment(1);
                return;
            }

            observer_state.latest_generation += 1;
            observer_state.last_requested_at = Some(now);
            let request = AgentObserverRequest {
                pane_id,
                generation: observer_state.latest_generation,
                requested_at: Instant::now(),
                metadata: metadata.clone(),
                runtime: runtime.clone(),
            };

            if observer_state.inflight_generation.is_some() {
                if observer_state.pending_request.replace(request).is_some() {
                    counter!("mux.agent_observer.refresh.replaced_pending.rate").increment(1);
                } else {
                    counter!("mux.agent_observer.refresh.coalesced.rate").increment(1);
                }
                return;
            }

            observer_state.inflight_generation = Some(request.generation);
            request
        };

        counter!("mux.agent_observer.refresh.scheduled.rate").increment(1);
        self.dispatch_agent_observer_request(request);
    }

    fn apply_agent_observer_update(&self, update: AgentObserverUpdate) {
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

        let (before_title, after_title) = {
            let mut runtime_by_pane = self.agent_runtime_by_pane.write();
            let Some(runtime) = runtime_by_pane.get_mut(&update.pane_id) else {
                counter!("mux.agent_observer.refresh.dropped_missing_runtime.rate").increment(1);
                return;
            };

            let before_title = Self::title_fingerprint(runtime);
            runtime.harness = update.runtime.harness;
            runtime.transport = update.runtime.transport;
            runtime.observed_at = update.runtime.observed_at;
            runtime.session_path = update.runtime.session_path;
            runtime.progress_summary = update.runtime.progress_summary;
            runtime.harness_mode = update.runtime.harness_mode;
            runtime.turn_phase = update.runtime.turn_phase;
            runtime.turn_state = update.runtime.turn_state;
            runtime.last_turn_completed_at = update.runtime.last_turn_completed_at;
            runtime.observer_error = update.runtime.observer_error;
            runtime.observer_started_at = update.runtime.observer_started_at;
            runtime.last_harness_refresh_at = update.runtime.last_harness_refresh_at;
            finalize_runtime_snapshot(runtime);
            runtime.status = derive_runtime_status(runtime);
            (before_title, Self::title_fingerprint(runtime))
        };

        histogram!("mux.agent_observer.refresh.apply.queue_delay").record(update.queue_delay);
        histogram!("mux.agent_observer.refresh.apply.latency").record(update.refresh_elapsed);
        counter!("mux.agent_observer.refresh.applied.rate").increment(1);

        if Self::agent_auto_adopt_on_confirmed_session_match()
            && self.get_agent_metadata_for_pane(update.pane_id).is_none()
            && self.detected_agent_panes.read().contains(&update.pane_id)
            && self
                .agent_runtime_by_pane
                .read()
                .get(&update.pane_id)
                .and_then(|runtime| runtime.session_path.as_deref())
                .is_some()
        {
            self.maybe_auto_adopt_detected_agent(update.pane_id);
        }

        if before_title != after_title {
            self.notify_tab_title_changed(tab_id);
        }
    }

    fn title_fingerprint(runtime: &AgentRuntimeSnapshot) -> AgentTitleFingerprint {
        AgentTitleFingerprint {
            turn_state: runtime.turn_state.clone(),
            last_turn_completed_at: runtime.last_turn_completed_at,
            attention_reason: runtime.attention_reason.clone(),
        }
    }

    pub fn record_agent_input(&self, pane_id: PaneId) {
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

    pub fn record_agent_output(&self, pane_id: PaneId) {
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

    fn refresh_agent_runtime_for_pane(&self, pane_id: PaneId, notify_title: bool) {
        self.refresh_agent_runtime_for_pane_sync_with_update(pane_id, notify_title, |_| {});
    }

    fn refresh_agent_runtime_for_pane_sync_with_update<F>(
        &self,
        pane_id: PaneId,
        notify_title: bool,
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
        refresh_runtime_from_harness(&mut runtime, metadata.as_ref());
        runtime.status = derive_runtime_status(&runtime);
        let refreshed_at = runtime.last_harness_refresh_at;
        let after_title = notify_title.then(|| Self::title_fingerprint(&runtime));
        self.agent_runtime_by_pane.write().insert(pane_id, runtime);
        if let Some(refreshed_at) = refreshed_at {
            self.note_sync_agent_observer_refresh(pane_id, refreshed_at);
        }

        if notify_title && before_title != after_title {
            self.notify_tab_title_changed(tab_id);
        }
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
        self.schedule_agent_observer_refresh(pane_id, metadata.as_ref(), &runtime, refresh_policy);
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

    fn agent_attention_seen_at_for_view(
        &self,
        view_id: &ClientViewId,
        pane_id: PaneId,
    ) -> Option<DateTime<Utc>> {
        self.agent_attention_seen_by_view
            .read()
            .get(view_id)
            .and_then(|seen| seen.get(&pane_id).copied())
    }

    fn acknowledge_agent_attention_for_view(&self, view_id: &ClientViewId, pane_id: PaneId) {
        let Some((_domain_id, _window_id, tab_id)) = self.resolve_pane_id(pane_id) else {
            return;
        };
        let completed_at = self
            .agent_runtime_by_pane
            .read()
            .get(&pane_id)
            .and_then(|runtime| runtime.last_turn_completed_at)
            .or_else(|| {
                self.detect_agent_state_for_pane(pane_id)
                    .and_then(|state| state.runtime.last_turn_completed_at)
            });
        let Some(completed_at) = completed_at else {
            return;
        };

        self.agent_attention_seen_by_view
            .write()
            .entry(view_id.clone())
            .or_default()
            .insert(pane_id, completed_at);
        self.notify_tab_title_changed(tab_id);
    }

    fn agent_turn_needs_attention_for_view(
        &self,
        view_id: &ClientViewId,
        pane_id: PaneId,
        runtime: &AgentRuntimeSnapshot,
    ) -> bool {
        if !matches!(
            runtime.turn_state,
            crate::agent::AgentTurnState::WaitingOnUser
        ) {
            return false;
        }

        let Some(completed_at) = runtime.last_turn_completed_at else {
            return false;
        };

        self.agent_attention_seen_at_for_view(view_id, pane_id)
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
            _ => AgentTabBadgeMode::Attention,
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
            .map(|tab| Self::sanitize_tab_title_text(&tab.get_title()))
            .unwrap_or_default()
    }

    fn cached_tab_badge_state_for_agents(
        &self,
        tab_id: TabId,
        view_id: Option<&ClientViewId>,
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
                let needs_attention = match view_id {
                    Some(view_id) => {
                        self.agent_turn_needs_attention_for_view(view_id, pane_id, runtime)
                    }
                    None => Self::agent_waiting_on_user(runtime),
                };
                if needs_attention {
                    badge.needs_attention = true;
                }
                if badge.waiting_on_user && badge.needs_attention {
                    break;
                }
            }
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

    fn should_badge_tab_for_agents(&self, tab_id: TabId, view_id: Option<&ClientViewId>) -> bool {
        let badge_mode = Self::agent_tab_badge_mode();
        if matches!(badge_mode, AgentTabBadgeMode::Off) {
            return false;
        }
        let badge = self.cached_tab_badge_state_for_agents(tab_id, view_id);
        match badge_mode {
            AgentTabBadgeMode::Off => false,
            AgentTabBadgeMode::Turn => badge.waiting_on_user,
            AgentTabBadgeMode::Attention => badge.needs_attention,
        }
    }

    pub fn effective_tab_title_for_view(&self, view_id: &ClientViewId, tab_id: TabId) -> String {
        let base_title = self.raw_tab_title(tab_id);
        if self.should_badge_tab_for_agents(tab_id, Some(view_id)) {
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
                if self.should_badge_tab_for_agents(tab_id, None) {
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
        })
    }

    fn build_agent_snapshot(
        &self,
        pane_id: PaneId,
        metadata: Arc<AgentMetadata>,
    ) -> Option<AgentSnapshot> {
        let pane = self.get_pane(pane_id)?;
        let runtime = self.runtime_snapshot_for_agent(pane_id, metadata.as_ref(), &pane);
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

        let pane = self.get_pane(pane_id)?;
        let runtime = self.agent_runtime_by_pane.read().get(&pane_id).cloned()?;
        if matches!(runtime.harness, crate::agent::AgentHarness::Unknown) {
            return None;
        }

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
                created_at,
                repo_root: None,
                worktree: None,
                branch: None,
                managed_checkout: false,
            },
            runtime,
            AgentOrigin::Detected,
            Some("cached".to_string()),
        )
    }

    pub fn list_agents(&self) -> Vec<AgentSnapshot> {
        if Self::agent_auto_adopt_on_confirmed_session_match() {
            let pane_ids = self.panes.read().keys().copied().collect::<Vec<_>>();
            for pane_id in pane_ids {
                self.maybe_auto_adopt_detected_agent(pane_id);
            }
        }

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
            let base_name =
                Self::detected_agent_name_base(&state.runtime.harness, &state.declared_cwd);
            let name = Self::next_available_agent_name(&taken_names, &base_name);
            taken_names.insert(name.clone());
            agents.push(Self::detected_agent_snapshot_from_state(state, name));
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
        window.idx_by_id(tab_id)
    }

    pub fn get_last_active_tab_idx_for_window_for_current_identity(
        &self,
        window_id: WindowId,
    ) -> Option<usize> {
        let view_id = self.active_view_id()?;
        let tab_id =
            self.get_last_active_tab_id_for_window_for_client(view_id.as_ref(), window_id)?;
        let window = self.get_window(window_id)?;
        window.idx_by_id(tab_id)
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
            self.acknowledge_agent_attention_for_view(view_id.as_ref(), pane_id);
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
        if let Some(view_id) = self.active_view_id() {
            self.acknowledge_agent_attention_for_view(view_id.as_ref(), pane_id);
        }
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
            for tab in window.iter() {
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
        self.clear_agent_metadata(pane_id);
        self.agent_observer_state_by_pane.write().remove(&pane_id);
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

    struct FakePane {
        id: PaneId,
        size: Mutex<TerminalSize>,
        domain_id: DomainId,
        title: String,
        cwd: Option<Url>,
        foreground_process_name: Option<String>,
        foreground_process_info: Option<LocalProcessInfo>,
    }

    impl FakePane {
        fn new(id: PaneId, size: TerminalSize, domain_id: DomainId) -> Arc<dyn Pane> {
            Arc::new(Self {
                id,
                size: Mutex::new(size),
                domain_id,
                title: String::new(),
                cwd: None,
                foreground_process_name: None,
                foreground_process_info: None,
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
                cwd: Some(Url::from_file_path(cwd).unwrap()),
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
                    children: HashMap::new(),
                }),
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
            created_at: Utc.with_ymd_and_hms(2026, 3, 17, 12, 0, 0).unwrap(),
            repo_root: None,
            worktree: None,
            branch: None,
            managed_checkout: false,
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
    fn set_agent_metadata_notifies_tab_title_changed() {
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
        mux.subscribe(move |notification| {
            if matches!(notification, MuxNotification::TabTitleChanged { tab_id: changed, .. } if changed == tab_id) {
                *title_changes_for_sub.lock() += 1;
            }
            true
        });

        mux.set_agent_metadata(pane_id, sample_agent_metadata("alpha"))
            .unwrap();

        assert_eq!(*title_changes.lock(), 1);
    }

    #[test]
    fn agent_runtime_tracks_input_and_output_activity() {
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

        mux.record_agent_input(pane_id);
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
                created_at: Utc.with_ymd_and_hms(2026, 3, 17, 12, 0, 0).unwrap(),
                repo_root: None,
                worktree: None,
                branch: None,
                managed_checkout: false,
            },
        )
        .unwrap();

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
                created_at: Utc.with_ymd_and_hms(2026, 3, 17, 12, 0, 0).unwrap(),
                repo_root: None,
                worktree: None,
                branch: None,
                managed_checkout: false,
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
                created_at: Utc.with_ymd_and_hms(2026, 3, 17, 12, 0, 0).unwrap(),
                repo_root: None,
                worktree: None,
                branch: None,
                managed_checkout: false,
            },
        )
        .unwrap();

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
        let _executor = promise::spawn::SimpleExecutor::new();
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
                created_at: Utc.with_ymd_and_hms(2026, 3, 17, 12, 0, 0).unwrap(),
                repo_root: None,
                worktree: None,
                branch: None,
                managed_checkout: false,
            },
        )
        .unwrap();

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
                created_at: Utc.with_ymd_and_hms(2026, 3, 17, 12, 0, 0).unwrap(),
                repo_root: None,
                worktree: None,
                branch: None,
                managed_checkout: false,
            },
        )
        .unwrap();

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
        assert_eq!(mux.effective_tab_title(tab.tab_id()), "🤖 scrape");
    }

    #[test]
    fn effective_tab_title_hides_badge_when_configured_empty() {
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
        tab.set_title("🤖 scrape");
        let pane = FakePane::new(143, size, domain.id);
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

        let _config = TestConfigGuard::new("turn", "");
        assert_eq!(mux.effective_tab_title(tab.tab_id()), "scrape");
    }

    #[test]
    fn attention_badge_clears_only_for_view_that_focuses_agent() {
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
        mux.set_active_pane_for_client_view(view_a.as_ref(), window_id, tab.tab_id(), pane_id)
            .unwrap();

        let client_b = Arc::new(ClientId::new());
        let view_b = Arc::new(ClientViewId("view-b".to_string()));
        mux.register_client(client_b.clone(), view_b.clone());
        mux.set_active_tab_for_client_view(view_b.as_ref(), window_id, tab.tab_id())
            .unwrap();
        mux.set_active_pane_for_client_view(view_b.as_ref(), window_id, tab.tab_id(), pane_id)
            .unwrap();

        assert_eq!(
            mux.effective_tab_title_for_view(view_a.as_ref(), tab.tab_id()),
            "🤖 scrape"
        );
        assert_eq!(
            mux.effective_tab_title_for_view(view_b.as_ref(), tab.tab_id()),
            "🤖 scrape"
        );

        mux.set_focused_pane_for_client(client_a.as_ref(), pane_id)
            .unwrap();
        mux.acknowledge_agent_attention_for_view(view_a.as_ref(), pane_id);

        assert_eq!(
            mux.effective_tab_title_for_view(view_a.as_ref(), tab.tab_id()),
            "scrape"
        );
        assert_eq!(
            mux.effective_tab_title_for_view(view_b.as_ref(), tab.tab_id()),
            "🤖 scrape"
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
        mux.set_active_pane_for_client_view(view_id.as_ref(), window_id, tab.tab_id(), pane_id)
            .unwrap();

        assert_eq!(
            mux.effective_tab_title_for_view(view_id.as_ref(), tab.tab_id()),
            "🤖 scrape"
        );

        let _identity = mux.with_identity(Some(client_id));
        mux.focus_pane_and_containing_tab(pane_id).unwrap();

        assert_eq!(
            mux.effective_tab_title_for_view(view_id.as_ref(), tab.tab_id()),
            "scrape"
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
}
