use crate::client::Client;
use crate::pane::ClientPane;
use anyhow::{anyhow, bail};
use async_trait::async_trait;
use codec::{ListPanesResponse, SpawnV2, SplitPane};
use config::keyassignment::SpawnTabDomain;
use config::{SshDomain, TlsDomainClient, UnixDomain, configuration};
use mux::agent::AgentTabBadgeState;
use mux::connui::{ConnectionUI, ConnectionUIParams};
use mux::domain::{Domain, DomainId, DomainState, SplitSource, alloc_domain_id};
use mux::pane::{Pane, PaneId};
use mux::tab::{SplitRequest, Tab, TabId};
use mux::window::WindowId;
use mux::{Mux, MuxNotification};
use portable_pty::CommandBuilder;
use promise::spawn::spawn_into_new_thread;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use wezterm_term::TerminalSize;

pub struct ClientInner {
    pub client: Client,
    pub local_domain_id: DomainId,
    pub local_echo_threshold_ms: Option<u64>,
    pub overlay_lag_indicator: bool,
    remote_to_local_window: Mutex<HashMap<WindowId, WindowId>>,
    remote_to_local_tab: Mutex<HashMap<TabId, TabId>>,
    remote_to_local_pane: Mutex<HashMap<PaneId, PaneId>>,
    pub focused_remote_pane_id: Mutex<Option<PaneId>>,
}

impl ClientInner {
    fn remote_to_local_window(&self, remote_window_id: WindowId) -> Option<WindowId> {
        let map = self.remote_to_local_window.lock().unwrap();
        map.get(&remote_window_id).cloned()
    }

    pub(crate) fn expire_stale_mappings(&self) {
        let mux = Mux::get();

        self.remote_to_local_pane
            .lock()
            .unwrap()
            .retain(|_remote_pane_id, local_pane_id| mux.get_pane(*local_pane_id).is_some());

        self.remote_to_local_tab
            .lock()
            .unwrap()
            .retain(
                |remote_tab_id, local_tab_id| match mux.get_tab(*local_tab_id) {
                    Some(tab) => {
                        for pos in tab.iter_panes_ignoring_zoom() {
                            if pos.pane.domain_id() == self.local_domain_id {
                                return true;
                            }
                        }
                        log::trace!(
                            "expire_stale_mappings: domain: {}. will remove \
                            {remote_tab_id} -> {local_tab_id} tab mapping \
                            because tab contains no panes from this domain",
                            self.local_domain_id,
                        );
                        false
                    }
                    None => false,
                },
            );

        self.remote_to_local_window
            .lock()
            .unwrap()
            .retain(
                |_remote_window_id, local_window_id| match mux.get_window(*local_window_id) {
                    Some(w) => {
                        for tab in w.iter() {
                            for pos in tab.iter_panes_ignoring_zoom() {
                                if pos.pane.domain_id() == self.local_domain_id {
                                    return true;
                                }
                            }
                        }
                        false
                    }
                    None => false,
                },
            );
    }

    fn record_remote_to_local_window_mapping(
        &self,
        remote_window_id: WindowId,
        local_window_id: WindowId,
    ) {
        let mut map = self.remote_to_local_window.lock().unwrap();
        map.insert(remote_window_id, local_window_id);
        log::trace!(
            "record_remote_to_local_window_mapping: {} -> {}",
            remote_window_id,
            local_window_id
        );
    }

    fn local_to_remote_tab(&self, local_tab_id: TabId) -> Option<TabId> {
        let map = self.remote_to_local_tab.lock().unwrap();
        for (remote, local) in map.iter() {
            if *local == local_tab_id {
                return Some(*remote);
            }
        }
        None
    }

    fn local_to_remote_window(&self, local_window_id: WindowId) -> Option<WindowId> {
        let map = self.remote_to_local_window.lock().unwrap();
        for (remote, local) in map.iter() {
            if *local == local_window_id {
                return Some(*remote);
            }
        }
        None
    }

    pub fn remote_to_local_pane_id(&self, remote_pane_id: PaneId) -> Option<TabId> {
        let mut pane_map = self.remote_to_local_pane.lock().unwrap();

        if let Some(id) = pane_map.get(&remote_pane_id) {
            return Some(*id);
        }

        let mux = Mux::get();

        for pane in mux.iter_panes() {
            if pane.domain_id() != self.local_domain_id {
                continue;
            }
            if let Some(pane) = pane.downcast_ref::<ClientPane>() {
                if pane.remote_pane_id() == remote_pane_id {
                    let local_pane_id = pane.pane_id();
                    pane_map.insert(remote_pane_id, local_pane_id);
                    return Some(local_pane_id);
                }
            }
        }
        None
    }
    pub fn remove_old_pane_mapping(&self, remote_pane_id: PaneId) {
        let mut pane_map = self.remote_to_local_pane.lock().unwrap();
        pane_map.remove(&remote_pane_id);
    }

    pub fn remove_old_tab_mapping(&self, remote_tab_id: TabId) {
        let mut tab_map = self.remote_to_local_tab.lock().unwrap();
        let old = tab_map.remove(&remote_tab_id);
        log::trace!("remove_old_tab_mapping: {remote_tab_id} -> {old:?}");
    }

    fn record_remote_to_local_tab_mapping(&self, remote_tab_id: TabId, local_tab_id: TabId) {
        let mut map = self.remote_to_local_tab.lock().unwrap();
        let prior = map.insert(remote_tab_id, local_tab_id);
        log::trace!(
            "record_remote_to_local_tab_mapping: {} -> {} \
             (prior={prior:?}, domain={})",
            remote_tab_id,
            local_tab_id,
            self.local_domain_id,
        );
    }

    pub fn remote_to_local_tab_id(&self, remote_tab_id: TabId) -> Option<TabId> {
        let map = self.remote_to_local_tab.lock().unwrap();
        map.get(&remote_tab_id).copied()
    }

    pub fn is_local(&self) -> bool {
        self.client.is_local
    }

    fn decorate_tab_title(raw_title: &str, badge: &AgentTabBadgeState) -> String {
        let raw_title = Mux::sanitize_tab_title_text(raw_title);
        let config = configuration();
        let should_badge = match config.agent_tab_badge_mode.as_str() {
            "off" => false,
            "turn" => badge.waiting_on_user,
            "attention" => badge.needs_attention,
            _ => badge.needs_attention,
        };
        if should_badge && !config.agent_tab_badge.is_empty() {
            format!("{}{}", config.agent_tab_badge, raw_title)
        } else {
            raw_title
        }
    }

    fn apply_remote_tab_title(
        &self,
        remote_tab_id: TabId,
        raw_title: &str,
        badge: &AgentTabBadgeState,
    ) {
        if let Some(local_tab_id) = self.remote_to_local_tab_id(remote_tab_id) {
            if let Some(tab) = Mux::get().get_tab(local_tab_id) {
                tab.set_title_from_remote(&Self::decorate_tab_title(raw_title, badge));
            }
        }
    }
}

#[derive(Clone, Debug)]
pub enum ClientDomainConfig {
    Unix(UnixDomain),
    Tls(TlsDomainClient),
    Ssh(SshDomain),
}

impl ClientDomainConfig {
    pub fn name(&self) -> &str {
        match self {
            ClientDomainConfig::Unix(unix) => &unix.name,
            ClientDomainConfig::Tls(tls) => &tls.name,
            ClientDomainConfig::Ssh(ssh) => &ssh.name,
        }
    }

    pub fn local_echo_threshold_ms(&self) -> Option<u64> {
        match self {
            ClientDomainConfig::Unix(unix) => unix.local_echo_threshold_ms,
            ClientDomainConfig::Tls(tls) => tls.local_echo_threshold_ms,
            ClientDomainConfig::Ssh(ssh) => ssh.local_echo_threshold_ms,
        }
    }

    pub fn overlay_lag_indicator(&self) -> bool {
        match self {
            ClientDomainConfig::Unix(unix) => unix.overlay_lag_indicator,
            ClientDomainConfig::Tls(tls) => tls.overlay_lag_indicator,
            ClientDomainConfig::Ssh(ssh) => ssh.overlay_lag_indicator,
        }
    }

    pub fn label(&self) -> String {
        match self {
            ClientDomainConfig::Unix(unix) => format!("unix mux {}", unix.socket_path().display()),
            ClientDomainConfig::Tls(tls) => format!("TLS mux {}", tls.remote_address),
            ClientDomainConfig::Ssh(ssh) => {
                if let Some(user) = &ssh.username {
                    format!("SSH mux {}@{}", user, ssh.remote_address)
                } else {
                    format!("SSH mux {}", ssh.remote_address)
                }
            }
        }
    }

    pub fn connect_automatically(&self) -> bool {
        match self {
            ClientDomainConfig::Unix(unix) => unix.connect_automatically,
            ClientDomainConfig::Tls(tls) => tls.connect_automatically,
            ClientDomainConfig::Ssh(ssh) => ssh.connect_automatically,
        }
    }
}

impl ClientInner {
    pub fn new(
        local_domain_id: DomainId,
        client: Client,
        local_echo_threshold_ms: Option<u64>,
        overlay_lag_indicator: bool,
    ) -> Self {
        Self {
            client,
            local_domain_id,
            local_echo_threshold_ms,
            overlay_lag_indicator,
            remote_to_local_window: Mutex::new(HashMap::new()),
            remote_to_local_tab: Mutex::new(HashMap::new()),
            remote_to_local_pane: Mutex::new(HashMap::new()),
            focused_remote_pane_id: Mutex::new(None),
        }
    }
}

pub struct ClientDomain {
    config: ClientDomainConfig,
    label: String,
    inner: Mutex<Option<Arc<ClientInner>>>,
    resync_in_progress: AtomicBool,
    resync_pending: AtomicBool,
    local_domain_id: DomainId,
}

async fn update_remote_workspace(
    local_domain_id: DomainId,
    pdu: codec::SetWindowWorkspace,
) -> anyhow::Result<()> {
    let inner = ClientDomain::get_client_inner_for_domain(local_domain_id)?;
    inner.client.set_window_workspace(pdu).await?;
    Ok(())
}

fn mux_notify_client_domain(local_domain_id: DomainId, notif: MuxNotification) -> bool {
    let mux = Mux::get();
    let domain = match mux.get_domain(local_domain_id) {
        Some(domain) => domain,
        None => return false,
    };
    let client_domain = match domain.downcast_ref::<ClientDomain>() {
        Some(c) => c,
        None => return false,
    };

    match notif {
        MuxNotification::ActiveWorkspaceChanged(_client_id) => {
            // TODO: advice remote host of interesting workspaces
        }
        MuxNotification::WorkspaceRenamed {
            old_workspace,
            new_workspace,
        } => {
            if let Some(inner) = client_domain.inner() {
                let workspaces = Mux::get().iter_workspaces();
                if workspaces.contains(&old_workspace) {
                    promise::spawn::spawn(async move {
                        inner
                            .client
                            .rename_workspace(codec::RenameWorkspace {
                                old_workspace,
                                new_workspace,
                            })
                            .await
                    })
                    .detach();
                }
            }
        }
        MuxNotification::WindowWorkspaceChanged(window_id) => {
            // Mux::get_window() may trigger a borrow error if called
            // immediately; defer the bulk of this work.
            // <https://github.com/wezterm/wezterm/issues/2638>
            promise::spawn::spawn_into_main_thread(async move {
                let mux = Mux::get();
                let domain = match mux.get_domain(local_domain_id) {
                    Some(domain) => domain,
                    None => return,
                };
                let domain = match domain.downcast_ref::<ClientDomain>() {
                    Some(domain) => domain,
                    None => return,
                };
                if let Some(remote_window_id) = domain.local_to_remote_window_id(window_id) {
                    if let Some(workspace) = mux
                        .get_window(window_id)
                        .map(|w| w.get_workspace().to_string())
                    {
                        promise::spawn::spawn_into_main_thread(async move {
                            let request = codec::SetWindowWorkspace {
                                window_id: remote_window_id,
                                workspace,
                            };
                            let _ = update_remote_workspace(local_domain_id, request).await;
                        })
                        .detach();
                    }
                } else {
                    log::debug!(
                        "local window id {window_id} has no known remote window \
                        id while reconciling a local WindowWorkspaceChanged event"
                    );
                }
            })
            .detach();
        }
        MuxNotification::TabTitleChanged { tab_id, title } => {
            if let Some(remote_tab_id) = client_domain.local_to_remote_tab_id(tab_id) {
                if let Some(inner) = client_domain.inner() {
                    promise::spawn::spawn(async move {
                        inner
                            .client
                            .set_tab_title(codec::TabTitleChanged {
                                tab_id: remote_tab_id,
                                title,
                                badge: AgentTabBadgeState::default(),
                            })
                            .await
                    })
                    .detach();
                }
            }
        }
        MuxNotification::WindowTitleChanged {
            window_id,
            title: _,
        } => {
            if let Some(remote_window_id) = client_domain.local_to_remote_window_id(window_id) {
                if let Some(inner) = client_domain.inner() {
                    promise::spawn::spawn_into_main_thread(async move {
                        // De-bounce the title propagation.
                        // There is a bit of a race condition with these async
                        // updates that can trigger a cycle of WindowTitleChanged
                        // PDUs being exchanged between client and server if the
                        // title is changed twice in quick succession.
                        // To avoid that, here on the client, we wait a second
                        // and then report the now-current name of the window, rather
                        // than propagating the title encoded in the MuxNotification.
                        smol::Timer::after(std::time::Duration::from_secs(1)).await;
                        if let Some(mux) = Mux::try_get() {
                            let title = mux
                                .get_window(window_id)
                                .map(|win| win.get_title().to_string());
                            if let Some(title) = title {
                                inner
                                    .client
                                    .set_window_title(codec::WindowTitleChanged {
                                        window_id: remote_window_id,
                                        title,
                                    })
                                    .await?;
                            }
                        }
                        anyhow::Result::<()>::Ok(())
                    })
                    .detach();
                }
            }
        }
        _ => {}
    }
    true
}

impl ClientDomain {
    pub fn new(config: ClientDomainConfig) -> Self {
        let local_domain_id = alloc_domain_id();
        let label = config.label();
        Mux::get().subscribe(move |notif| mux_notify_client_domain(local_domain_id, notif));
        Self {
            config,
            label,
            inner: Mutex::new(None),
            resync_in_progress: AtomicBool::new(false),
            resync_pending: AtomicBool::new(false),
            local_domain_id,
        }
    }

    fn inner(&self) -> Option<Arc<ClientInner>> {
        self.inner.lock().unwrap().as_ref().map(Arc::clone)
    }

    pub fn connect_automatically(&self) -> bool {
        self.config.connect_automatically()
    }

    pub fn perform_detach(&self) {
        log::info!("detached domain {}", self.local_domain_id);
        self.inner.lock().unwrap().take();
        let mux = Mux::get();
        mux.domain_was_detached(self.local_domain_id);
    }

    pub fn remote_to_local_pane_id(&self, remote_pane_id: TabId) -> Option<TabId> {
        let inner = self.inner()?;
        inner.remote_to_local_pane_id(remote_pane_id)
    }

    pub fn remote_to_local_window_id(&self, remote_window_id: WindowId) -> Option<WindowId> {
        let inner = self.inner()?;
        inner.remote_to_local_window(remote_window_id)
    }

    pub fn local_to_remote_window_id(&self, local_window_id: WindowId) -> Option<WindowId> {
        let inner = self.inner()?;
        inner.local_to_remote_window(local_window_id)
    }

    pub fn local_to_remote_tab_id(&self, local_tab_id: TabId) -> Option<TabId> {
        let inner = self.inner()?;
        inner.local_to_remote_tab(local_tab_id)
    }

    fn spawn_target_for_window(
        mux: &Mux,
        inner: &ClientInner,
        window: WindowId,
    ) -> (Option<WindowId>, Option<PaneId>) {
        let remote_window_id = inner.local_to_remote_window(window);
        let remote_pane_id = mux
            .get_active_pane_for_window_for_current_identity(window)
            .and_then(|pane| {
                pane.downcast_ref::<ClientPane>()
                    .map(|pane| pane.remote_pane_id())
            });

        match (remote_window_id, remote_pane_id) {
            (Some(remote_window_id), Some(remote_pane_id)) => {
                (Some(remote_window_id), Some(remote_pane_id))
            }
            _ => (None, None),
        }
    }

    pub fn get_client_inner_for_domain(domain_id: DomainId) -> anyhow::Result<Arc<ClientInner>> {
        let mux = Mux::get();
        let domain = mux
            .get_domain(domain_id)
            .ok_or_else(|| anyhow!("invalid domain id {}", domain_id))?;
        let domain = domain
            .downcast_ref::<Self>()
            .ok_or_else(|| anyhow!("domain {} is not a ClientDomain", domain_id))?;

        if let Some(inner) = domain.inner() {
            Ok(inner)
        } else {
            bail!("domain has no assigned client");
        }
    }

    /// The reader in the mux may have decided to give up on one or
    /// more tabs at the time that a disconnect was detected, and
    /// it's also possible that another client connected and adjusted
    /// the set of tabs since we were connected, so we need to re-sync.
    pub async fn reattach(domain_id: DomainId, ui: ConnectionUI) -> anyhow::Result<()> {
        let inner = Self::get_client_inner_for_domain(domain_id)?;

        let panes = inner.client.list_panes().await?;
        Self::process_pane_list(inner, panes, None)?;

        ui.close();
        Ok(())
    }

    pub async fn resync(&self) -> anyhow::Result<()> {
        if let Some(inner) = self.inner() {
            let panes = inner.client.list_panes().await?;
            Self::process_pane_list(inner, panes, None)?;
        }
        Ok(())
    }

    /// Debounce bursts of resync requests. If a resync is already in
    /// progress, mark a pending flag instead of dropping the request.
    /// When the current resync finishes, it checks the flag and runs
    /// one more to pick up any changes that arrived during the first.
    pub async fn resync_coalesced(&self) -> anyhow::Result<()> {
        if self.resync_in_progress.swap(true, Ordering::AcqRel) {
            // A resync is running — mark that another is needed when it finishes
            self.resync_pending.store(true, Ordering::Release);
            log::trace!(
                "domain {} resync already in progress, marked pending",
                self.local_domain_id
            );
            return Ok(());
        }

        loop {
            self.resync_pending.store(false, Ordering::Release);
            let result = self.resync().await;
            if let Err(err) = &result {
                self.resync_in_progress.store(false, Ordering::Release);
                return Err(anyhow::anyhow!("{:#}", err));
            }

            // If no more pending requests arrived during this resync, we're done
            if !self.resync_pending.swap(false, Ordering::AcqRel) {
                break;
            }
            log::trace!("domain {} running pending resync", self.local_domain_id);
        }

        self.resync_in_progress.store(false, Ordering::Release);
        Ok(())
    }

    pub fn process_remote_window_title_change(&self, remote_window_id: WindowId, title: String) {
        if let Some(inner) = self.inner() {
            if let Some(local_window_id) = inner.remote_to_local_window(remote_window_id) {
                if let Some(mut window) = Mux::get().get_window_mut(local_window_id) {
                    window.set_title_from_remote(&title);
                }
            }
        }
    }

    pub fn process_remote_tab_title_change(
        &self,
        remote_tab_id: TabId,
        title: String,
        badge: AgentTabBadgeState,
    ) {
        if let Some(inner) = self.inner() {
            inner.apply_remote_tab_title(remote_tab_id, &title, &badge);
        }
    }

    fn process_pane_list(
        inner: Arc<ClientInner>,
        panes: ListPanesResponse,
        mut primary_window_id: Option<WindowId>,
    ) -> anyhow::Result<()> {
        let mux = Mux::get();
        log::debug!(
            "domain {}: ListPanes result {:#?}",
            inner.local_domain_id,
            panes
        );

        // "Mark" the current set of known remote ids, so that we can "Sweep"
        // any unreferenced ids at the bottom, garbage collection style
        let mut remote_windows_to_forget: HashSet<WindowId> = inner
            .remote_to_local_window
            .lock()
            .unwrap()
            .keys()
            .copied()
            .collect();
        let mut remote_tabs_to_forget: HashSet<WindowId> = inner
            .remote_to_local_tab
            .lock()
            .unwrap()
            .keys()
            .copied()
            .collect();
        let mut remote_panes_to_forget: HashSet<WindowId> = inner
            .remote_to_local_pane
            .lock()
            .unwrap()
            .keys()
            .copied()
            .collect();

        let client_window_view_state = panes.client_window_view_state.clone();

        for ((tabroot, tab_title), tab_badge) in panes
            .tabs
            .into_iter()
            .zip(panes.tab_titles.iter())
            .zip(panes.tab_badges.iter())
        {
            let root_size = match tabroot.root_size() {
                Some(size) => size,
                None => continue,
            };

            if let Some((remote_window_id, remote_tab_id)) = tabroot.window_and_tab_ids() {
                let tab;

                remote_windows_to_forget.remove(&remote_window_id);
                remote_tabs_to_forget.remove(&remote_tab_id);

                if let Some(tab_id) = inner.remote_to_local_tab_id(remote_tab_id) {
                    match mux.get_tab(tab_id) {
                        Some(t) => tab = t,
                        None => {
                            // We likely decided that we hit EOF on the tab and
                            // removed it from the mux.  Let's add it back, but
                            // with a new id.
                            log::trace!(
                                "we had remote_to_local_tab_id mapping of \
                                 {remote_tab_id} -> {tab_id}, but the local \
                                 tab is not in the mux, make a new tab"
                            );
                            inner.remove_old_tab_mapping(remote_tab_id);
                            tab = Arc::new(Tab::new(&root_size));
                            inner.record_remote_to_local_tab_mapping(remote_tab_id, tab.tab_id());
                            mux.add_tab_no_panes(&tab);
                        }
                    };
                } else {
                    tab = Arc::new(Tab::new(&root_size));
                    mux.add_tab_no_panes(&tab);
                    inner.record_remote_to_local_tab_mapping(remote_tab_id, tab.tab_id());
                }

                inner.apply_remote_tab_title(remote_tab_id, tab_title, tab_badge);

                log::debug!("domain: {} tree: {:#?}", inner.local_domain_id, tabroot);
                let mut workspace = None;
                tab.sync_with_pane_tree(root_size, tabroot, |entry| {
                    workspace.replace(entry.workspace.clone());
                    remote_panes_to_forget.remove(&entry.pane_id);
                    if let Some(pane_id) = inner.remote_to_local_pane_id(entry.pane_id) {
                        match mux.get_pane(pane_id) {
                            Some(pane) => pane,
                            None => {
                                // We likely decided that we hit EOF on the tab and
                                // removed it from the mux.  Let's add it back, but
                                // with a new id.
                                inner.remove_old_pane_mapping(entry.pane_id);
                                let pane: Arc<dyn Pane> = Arc::new(ClientPane::new(
                                    &inner,
                                    entry.tab_id,
                                    entry.pane_id,
                                    entry.size,
                                    &entry.title,
                                ));
                                mux.add_pane(&pane).expect("failed to add pane to mux");
                                pane
                            }
                        }
                    } else {
                        let pane: Arc<dyn Pane> = Arc::new(ClientPane::new(
                            &inner,
                            entry.tab_id,
                            entry.pane_id,
                            entry.size,
                            &entry.title,
                        ));
                        log::debug!(
                            "domain: {} attaching to remote pane {:?} -> local pane_id {}",
                            inner.local_domain_id,
                            entry,
                            pane.pane_id()
                        );
                        mux.add_pane(&pane).expect("failed to add pane to mux");
                        pane
                    }
                });

                if let Some(local_window_id) = inner.remote_to_local_window(remote_window_id) {
                    let mut window = mux
                        .get_window_mut(local_window_id)
                        .expect("no such window!?");
                    log::debug!(
                        "domain: {} adding tab to existing local window {}",
                        inner.local_domain_id,
                        local_window_id
                    );
                    if window.idx_by_id(tab.tab_id()).is_none() {
                        window.push(&tab);
                    }
                    continue;
                }

                if let Some(local_window_id) = primary_window_id {
                    // Verify that the workspace is consistent between the local and remote
                    // windows
                    if Some(
                        mux.get_window(local_window_id)
                            .expect("primary window to be valid")
                            .get_workspace(),
                    ) == workspace.as_deref()
                    {
                        // Yes! We can use this window
                        log::debug!(
                            "adding remote window {} as tab to local window {}",
                            remote_window_id,
                            local_window_id
                        );
                        inner.record_remote_to_local_window_mapping(
                            remote_window_id,
                            local_window_id,
                        );
                        mux.add_tab_to_window(&tab, local_window_id)?;
                        primary_window_id.take();
                        continue;
                    }
                }
                log::debug!(
                    "making new local window for remote {} in workspace {:?}",
                    remote_window_id,
                    workspace
                );
                let position = None;
                let local_window_id = mux.new_empty_window(workspace.take(), position);
                inner.record_remote_to_local_window_mapping(remote_window_id, *local_window_id);
                mux.add_tab_to_window(&tab, *local_window_id)?;
            }
        }

        for (remote_window_id, window_title) in panes.window_titles {
            if let Some(local_window_id) = inner.remote_to_local_window(remote_window_id) {
                let mut window = mux
                    .get_window_mut(local_window_id)
                    .expect("no such window!?");
                window.set_title_from_remote(&window_title);
            }
        }

        for (remote_window_id, window_view_state) in client_window_view_state {
            let Some(remote_tab_id) = window_view_state.active_tab_id else {
                continue;
            };
            let Some(local_window_id) = inner.remote_to_local_window(remote_window_id) else {
                continue;
            };
            let Some(local_tab_id) = inner.remote_to_local_tab_id(remote_tab_id) else {
                continue;
            };
            if mux
                .set_active_tab_for_current_identity(local_window_id, local_tab_id)
                .is_err()
            {
                continue;
            }

            if let Some(remote_active_pane_id) = window_view_state
                .tabs
                .get(&remote_tab_id)
                .and_then(|tab_state| tab_state.active_pane_id)
            {
                if let Some(local_active_pane_id) =
                    inner.remote_to_local_pane_id(remote_active_pane_id)
                {
                    let _ = mux.set_active_pane_for_current_identity(
                        local_window_id,
                        local_tab_id,
                        local_active_pane_id,
                    );
                    if let Some(tab) = mux.get_tab(local_tab_id) {
                        if let Some(pane) = mux.get_pane(local_active_pane_id) {
                            tab.set_active_pane(&pane, mux::tab::NotifyMux::No);
                        }
                    }
                }
            }
        }

        // "Sweep" away our mapping for ids that are no longer present in the
        // latest sync
        log::debug!(
            "after sync, remote_windows_to_forget={remote_windows_to_forget:?}, \
                    remote_tabs_to_forget={remote_tabs_to_forget:?}, \
                    remote_panes_to_forget={remote_panes_to_forget:?}"
        );
        if !remote_windows_to_forget.is_empty() {
            let mut windows = inner.remote_to_local_window.lock().unwrap();
            for w in remote_windows_to_forget {
                windows.remove(&w);
            }
        }
        if !remote_tabs_to_forget.is_empty() {
            let mut tabs = inner.remote_to_local_tab.lock().unwrap();
            for t in remote_tabs_to_forget {
                tabs.remove(&t);
            }
        }
        if !remote_panes_to_forget.is_empty() {
            let mut panes = inner.remote_to_local_pane.lock().unwrap();
            for p in remote_panes_to_forget {
                panes.remove(&p);
            }
        }

        Ok(())
    }

    fn finish_attach(
        domain_id: DomainId,
        client: Client,
        panes: ListPanesResponse,
        primary_window_id: Option<WindowId>,
    ) -> anyhow::Result<()> {
        let mux = Mux::get();
        let domain = mux
            .get_domain(domain_id)
            .ok_or_else(|| anyhow!("invalid domain id {}", domain_id))?;
        let domain = domain
            .downcast_ref::<Self>()
            .ok_or_else(|| anyhow!("domain {} is not a ClientDomain", domain_id))?;
        let threshold = domain.config.local_echo_threshold_ms();
        let overlay_lag_indicator = domain.config.overlay_lag_indicator();

        let inner = Arc::new(ClientInner::new(
            domain_id,
            client,
            threshold,
            overlay_lag_indicator,
        ));
        *domain.inner.lock().unwrap() = Some(Arc::clone(&inner));

        Self::process_pane_list(inner, panes, primary_window_id)?;

        Ok(())
    }
}

#[async_trait(?Send)]
impl Domain for ClientDomain {
    fn domain_id(&self) -> DomainId {
        self.local_domain_id
    }

    fn domain_name(&self) -> &str {
        self.config.name()
    }

    async fn domain_label(&self) -> String {
        self.label.to_string()
    }

    async fn spawn_pane(
        &self,
        _size: TerminalSize,
        _command: Option<CommandBuilder>,
        _command_dir: Option<String>,
    ) -> anyhow::Result<Arc<dyn Pane>> {
        anyhow::bail!("spawn_pane not implemented for ClientDomain")
    }

    /// Forward the request to the remote; we need to translate the local ids
    /// to those that match the remote for the request, resync the changed
    /// structure, and then translate the results back to local
    async fn move_pane_to_new_tab(
        &self,
        pane_id: PaneId,
        window_id: Option<WindowId>,
        workspace_for_new_window: Option<String>,
    ) -> anyhow::Result<Option<(Arc<Tab>, WindowId)>> {
        let inner = self
            .inner()
            .ok_or_else(|| anyhow!("domain is not attached"))?;

        let local_pane = Mux::get()
            .get_pane(pane_id)
            .ok_or_else(|| anyhow!("pane_id {} is invalid", pane_id))?;
        let pane = local_pane
            .downcast_ref::<ClientPane>()
            .ok_or_else(|| anyhow!("pane_id {} is not a ClientPane", pane_id))?;

        let remote_window_id =
            window_id.and_then(|local_window| self.local_to_remote_window_id(local_window));

        let result = inner
            .client
            .move_pane_to_new_tab(codec::MovePaneToNewTab {
                pane_id: pane.remote_pane_id,
                window_id: remote_window_id,
                workspace_for_new_window,
            })
            .await?;

        self.resync().await?;

        let local_tab_id = inner
            .remote_to_local_tab_id(result.tab_id)
            .ok_or_else(|| anyhow!("remote tab {} didn't resolve after resync", result.tab_id))?;

        let local_win_id = self
            .remote_to_local_window_id(result.window_id)
            .ok_or_else(|| {
                anyhow!(
                    "remote window {} didn't resolve after resync",
                    result.window_id
                )
            })?;

        let tab = Mux::get()
            .get_tab(local_tab_id)
            .ok_or_else(|| anyhow!("local tab {local_tab_id} is invalid"))?;

        Ok(Some((tab, local_win_id)))
    }

    async fn spawn(
        &self,
        size: TerminalSize,
        command: Option<CommandBuilder>,
        command_dir: Option<String>,
        window: WindowId,
    ) -> anyhow::Result<Arc<Tab>> {
        let inner = self
            .inner()
            .ok_or_else(|| anyhow!("domain is not attached"))?;

        let mux = Mux::get();
        let workspace = mux.active_workspace();
        let (remote_window_id, remote_current_pane_id) =
            Self::spawn_target_for_window(&mux, inner.as_ref(), window);
        if remote_current_pane_id.is_none() {
            log::info!(
                "domain {} spawn in local window {} has no active ClientPane; creating a new remote window",
                self.local_domain_id,
                window
            );
        }
        let result = inner
            .client
            .spawn_v2(SpawnV2 {
                domain: SpawnTabDomain::DefaultDomain,
                window_id: remote_window_id,
                current_pane_id: remote_current_pane_id,
                size,
                command,
                command_dir,
                workspace,
            })
            .await?;

        inner.record_remote_to_local_window_mapping(result.window_id, window);

        let pane: Arc<dyn Pane> = Arc::new(ClientPane::new(
            &inner,
            result.tab_id,
            result.pane_id,
            size,
            "wezterm",
        ));
        let tab = Arc::new(Tab::new(&size));
        tab.assign_pane(&pane);
        inner.remove_old_tab_mapping(result.tab_id);
        inner.record_remote_to_local_tab_mapping(result.tab_id, tab.tab_id());

        let mux = Mux::get();
        mux.add_tab_and_active_pane(&tab)?;
        mux.add_tab_to_window(&tab, window)?;

        Ok(tab)
    }

    async fn split_pane(
        &self,
        source: SplitSource,
        tab_id: TabId,
        pane_id: PaneId,
        split_request: SplitRequest,
    ) -> anyhow::Result<Arc<dyn Pane>> {
        let inner = self
            .inner()
            .ok_or_else(|| anyhow!("domain is not attached"))?;

        let mux = Mux::get();

        let tab = mux
            .get_tab(tab_id)
            .ok_or_else(|| anyhow!("tab_id {} is invalid", tab_id))?;
        let local_pane = mux
            .get_pane(pane_id)
            .ok_or_else(|| anyhow!("pane_id {} is invalid", pane_id))?;
        let pane = local_pane
            .downcast_ref::<ClientPane>()
            .ok_or_else(|| anyhow!("pane_id {} is not a ClientPane", pane_id))?;

        let (command, command_dir, move_pane_id) = match source {
            SplitSource::Spawn {
                command,
                command_dir,
            } => (command, command_dir, None),
            SplitSource::MovePane(move_pane_id) => (None, None, Some(move_pane_id)),
        };

        let result = inner
            .client
            .split_pane(SplitPane {
                domain: SpawnTabDomain::CurrentPaneDomain,
                pane_id: pane.remote_pane_id,
                split_request,
                command,
                command_dir,
                move_pane_id,
                // The GUI client always has up-to-date sizes (it controls
                // the window), so tab_size isn't needed here — the server
                // already has the correct size from the client's resizes.
                tab_size: None,
            })
            .await?;

        let pane: Arc<dyn Pane> = Arc::new(ClientPane::new(
            &inner,
            result.tab_id,
            result.pane_id,
            result.size,
            "wezterm",
        ));

        let pane_index = match tab
            .iter_panes()
            .iter()
            .find(|p| p.pane.pane_id() == pane_id)
        {
            Some(p) => p.index,
            None => anyhow::bail!("invalid pane id {}", pane_id),
        };

        tab.split_and_insert(pane_index, split_request, Arc::clone(&pane))
            .ok();

        mux.add_pane(&pane)?;

        Ok(pane)
    }

    async fn attach(&self, window_id: Option<WindowId>) -> anyhow::Result<()> {
        if self.state() == DomainState::Attached {
            // Already attached
            return Ok(());
        }

        let domain_id = self.local_domain_id;
        let config = self.config.clone();

        let activity = mux::activity::Activity::new();
        let ui = ConnectionUI::with_params(ConnectionUIParams {
            window_id,
            ..Default::default()
        });
        ui.title("wezterm: Connecting...");

        ui.async_run_and_log_error({
            let ui = ui.clone();
            async move {
                let mut cloned_ui = ui.clone();
                let client = spawn_into_new_thread(move || match &config {
                    ClientDomainConfig::Unix(unix) => {
                        let initial = true;
                        let no_auto_start = false;
                        Client::new_unix_domain(
                            Some(domain_id),
                            unix,
                            initial,
                            &mut cloned_ui,
                            no_auto_start,
                        )
                    }
                    ClientDomainConfig::Tls(tls) => Client::new_tls(domain_id, tls, &mut cloned_ui),
                    ClientDomainConfig::Ssh(ssh) => Client::new_ssh(domain_id, ssh, &mut cloned_ui),
                })
                .await?;

                ui.output_str("Checking server version\n");
                client.verify_version_compat(&ui).await?;

                ui.output_str("Version check OK!  Requesting pane list...\n");
                let panes = client.list_panes().await?;
                ui.output_str(&format!(
                    "Server has {} tabs.  Attaching to local UI...\n",
                    panes.tabs.len()
                ));
                ClientDomain::finish_attach(domain_id, client, panes, window_id)
            }
        })
        .await
        .map_err(|e| {
            ui.output_str(&format!("Error during attach: {:#}\n", e));
            e
        })?;

        ui.output_str("Attached!\n");
        drop(activity);
        ui.close();
        Ok(())
    }

    fn detachable(&self) -> bool {
        true
    }

    fn detach(&self) -> anyhow::Result<()> {
        self.perform_detach();
        Ok(())
    }

    fn state(&self) -> DomainState {
        if self.inner.lock().unwrap().is_some() {
            DomainState::Attached
        } else {
            DomainState::Detached
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use mux::Mux;
    use mux::client::{ClientId, ClientTabViewState, ClientViewId, ClientWindowViewState};
    use mux::renderable::StableCursorPosition;
    use mux::tab::{PaneEntry, PaneNode, SerdeUrl};
    use mux::window::WindowId;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::Once;
    use termwiz::surface::{CursorShape, CursorVisibility};
    use wezterm_term::TerminalSize;

    struct MuxGuard;

    impl Drop for MuxGuard {
        fn drop(&mut self) {
            Mux::shutdown();
        }
    }

    lazy_static::lazy_static! {
        static ref TEST_MUX_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
    }

    fn ensure_test_executor() {
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            let _ = Box::leak(Box::new(promise::spawn::SimpleExecutor::new()));
        });
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
        window_id: WindowId,
        tab_id: TabId,
        pane_id: PaneId,
        pane_size: TerminalSize,
        is_active_pane: bool,
    ) -> PaneNode {
        PaneNode::Leaf(PaneEntry {
            window_id,
            tab_id,
            pane_id,
            agent_metadata: None,
            title: format!("pane-{pane_id}"),
            size: pane_size,
            working_dir: Some(SerdeUrl {
                url: url::Url::from_file_path("/tmp").unwrap(),
            }),
            is_active_pane,
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

    fn panes_response(
        tabs: Vec<PaneNode>,
        active_tab_id: TabId,
        active_pane_id: PaneId,
    ) -> ListPanesResponse {
        ListPanesResponse {
            tab_titles: tabs
                .iter()
                .map(|node| match node {
                    PaneNode::Leaf(entry) => format!("tab-{}", entry.tab_id),
                    _ => "tab".to_string(),
                })
                .collect(),
            tab_badges: tabs.iter().map(|_| AgentTabBadgeState::default()).collect(),
            tabs,
            window_titles: HashMap::from([(1, "remote-window".to_string())]),
            client_window_view_state: HashMap::from([(
                1,
                ClientWindowViewState {
                    active_tab_id: Some(active_tab_id),
                    last_active_tab_id: None,
                    tabs: HashMap::from([(
                        active_tab_id,
                        ClientTabViewState {
                            active_pane_id: Some(active_pane_id),
                        },
                    )]),
                },
            )]),
        }
    }

    fn make_dummy_client(
        local_domain_id: DomainId,
        view_name: &str,
    ) -> (Arc<ClientId>, Arc<ClientViewId>, Client) {
        let client_id = Arc::new(ClientId::new());
        let view_id = Arc::new(ClientViewId(view_name.to_string()));
        (
            client_id.clone(),
            view_id.clone(),
            Client::new_for_test(
                local_domain_id,
                client_id.as_ref().clone(),
                view_id.as_ref().clone(),
            ),
        )
    }

    fn install_client_domain(
        mux: &Arc<Mux>,
        view_name: &str,
    ) -> (
        Arc<ClientDomain>,
        Arc<ClientInner>,
        Arc<ClientId>,
        Arc<ClientViewId>,
    ) {
        let domain = Arc::new(ClientDomain::new(ClientDomainConfig::Unix(
            UnixDomain::default(),
        )));
        mux.add_domain(&(domain.clone() as Arc<dyn Domain>));
        let (client_id, view_id, client) = make_dummy_client(domain.local_domain_id, view_name);
        mux.register_client(client_id.clone(), view_id.clone());
        let inner = Arc::new(ClientInner::new(
            domain.local_domain_id,
            client,
            None,
            false,
        ));
        *domain.inner.lock().unwrap() = Some(inner.clone());
        (domain, inner, client_id, view_id)
    }

    fn apply_panes(
        mux: &Arc<Mux>,
        inner: Arc<ClientInner>,
        client_id: Arc<ClientId>,
        panes: ListPanesResponse,
    ) {
        let _identity = mux.with_identity(Some(client_id));
        ClientDomain::process_pane_list(inner, panes, None).unwrap();
    }

    #[test]
    fn mirrored_domains_keep_active_tabs_divergent_across_reconcile_lifecycle() {
        let _test_lock = TEST_MUX_LOCK.lock();
        ensure_test_executor();
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let _guard = MuxGuard;

        let (_domain_a, inner_a, client_a, view_a) = install_client_domain(&mux, "view-a");
        let (_domain_b, inner_b, client_b, view_b) = install_client_domain(&mux, "view-b");

        let tab_a = leaf(1, 101, 1001, size(120, 40), true);
        let tab_b = leaf(1, 102, 1002, size(120, 40), true);

        apply_panes(
            &mux,
            inner_a.clone(),
            client_a.clone(),
            panes_response(vec![tab_a.clone(), tab_b.clone()], 101, 1001),
        );
        apply_panes(
            &mux,
            inner_b.clone(),
            client_b.clone(),
            panes_response(vec![tab_a.clone(), tab_b.clone()], 102, 1002),
        );

        let local_window_a = inner_a.remote_to_local_window(1).unwrap();
        let local_window_b = inner_b.remote_to_local_window(1).unwrap();
        let local_tab_a_101 = inner_a.remote_to_local_tab_id(101).unwrap();
        let local_tab_a_102 = inner_a.remote_to_local_tab_id(102).unwrap();
        let local_tab_b_101 = inner_b.remote_to_local_tab_id(101).unwrap();
        let local_tab_b_102 = inner_b.remote_to_local_tab_id(102).unwrap();

        assert_eq!(
            mux.get_active_tab_for_window_for_client(view_a.as_ref(), local_window_a)
                .map(|tab| tab.tab_id()),
            Some(local_tab_a_101)
        );
        assert_eq!(
            mux.get_active_pane_id_for_tab_for_client(
                view_a.as_ref(),
                local_window_a,
                local_tab_a_101,
            ),
            inner_a.remote_to_local_pane_id(1001)
        );
        assert_eq!(
            mux.get_active_tab_for_window_for_client(view_b.as_ref(), local_window_b)
                .map(|tab| tab.tab_id()),
            Some(local_tab_b_102)
        );
        assert_eq!(
            mux.get_active_pane_id_for_tab_for_client(
                view_b.as_ref(),
                local_window_b,
                local_tab_b_102,
            ),
            inner_b.remote_to_local_pane_id(1002)
        );

        apply_panes(
            &mux,
            inner_a.clone(),
            client_a.clone(),
            panes_response(vec![tab_a.clone(), tab_b.clone()], 101, 1001),
        );
        apply_panes(
            &mux,
            inner_b.clone(),
            client_b.clone(),
            panes_response(vec![tab_a.clone(), tab_b.clone()], 102, 1002),
        );

        assert_eq!(inner_a.remote_to_local_tab_id(101), Some(local_tab_a_101));
        assert_eq!(inner_a.remote_to_local_tab_id(102), Some(local_tab_a_102));
        assert_eq!(inner_b.remote_to_local_tab_id(101), Some(local_tab_b_101));
        assert_eq!(inner_b.remote_to_local_tab_id(102), Some(local_tab_b_102));

        let tab_c = leaf(1, 103, 1003, size(120, 40), true);
        apply_panes(
            &mux,
            inner_a.clone(),
            client_a.clone(),
            panes_response(vec![tab_a.clone(), tab_b.clone(), tab_c.clone()], 103, 1003),
        );
        apply_panes(
            &mux,
            inner_b.clone(),
            client_b.clone(),
            panes_response(vec![tab_a.clone(), tab_b.clone(), tab_c.clone()], 102, 1002),
        );

        let local_tab_a_103 = inner_a.remote_to_local_tab_id(103).unwrap();
        let local_tab_b_103 = inner_b.remote_to_local_tab_id(103).unwrap();
        assert_eq!(
            mux.get_active_tab_for_window_for_client(view_a.as_ref(), local_window_a)
                .map(|tab| tab.tab_id()),
            Some(local_tab_a_103)
        );
        assert_eq!(
            mux.get_active_tab_for_window_for_client(view_b.as_ref(), local_window_b)
                .map(|tab| tab.tab_id()),
            Some(local_tab_b_102)
        );
        assert_eq!(mux.get_tab(local_tab_a_103).is_some(), true);
        assert_eq!(mux.get_tab(local_tab_b_103).is_some(), true);

        apply_panes(
            &mux,
            inner_a.clone(),
            client_a.clone(),
            panes_response(vec![tab_a.clone(), tab_c.clone()], 103, 1003),
        );
        apply_panes(
            &mux,
            inner_b.clone(),
            client_b.clone(),
            panes_response(vec![tab_a, tab_c], 101, 1001),
        );

        assert_eq!(inner_a.remote_to_local_tab_id(102), None);
        assert_eq!(inner_b.remote_to_local_tab_id(102), None);
        assert_eq!(
            mux.get_active_tab_for_window_for_client(view_a.as_ref(), local_window_a)
                .map(|tab| tab.tab_id()),
            Some(local_tab_a_103)
        );
        assert_eq!(
            mux.get_active_tab_for_window_for_client(view_b.as_ref(), local_window_b)
                .map(|tab| tab.tab_id()),
            Some(local_tab_b_101)
        );
        assert_eq!(
            mux.get_active_pane_id_for_tab_for_client(
                view_a.as_ref(),
                local_window_a,
                local_tab_a_103,
            ),
            inner_a.remote_to_local_pane_id(1003)
        );
        assert_eq!(
            mux.get_active_pane_id_for_tab_for_client(
                view_b.as_ref(),
                local_window_b,
                local_tab_b_101,
            ),
            inner_b.remote_to_local_pane_id(1001)
        );
    }

    #[test]
    fn first_reconcile_seeds_usable_active_pane_for_current_identity() {
        let _test_lock = TEST_MUX_LOCK.lock();
        ensure_test_executor();
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let _guard = MuxGuard;

        let (_domain, inner, client_id, _view_id) = install_client_domain(&mux, "focus-view");

        let tab_a = leaf(1, 101, 1001, size(120, 40), true);
        let tab_b = leaf(1, 102, 1002, size(120, 40), true);

        apply_panes(
            &mux,
            inner.clone(),
            client_id.clone(),
            panes_response(vec![tab_a, tab_b], 102, 1002),
        );

        let local_window_id = inner.remote_to_local_window(1).unwrap();
        let local_tab_id = inner.remote_to_local_tab_id(102).unwrap();
        let local_pane_id = inner.remote_to_local_pane_id(1002).unwrap();

        let _identity = mux.with_identity(Some(client_id));
        assert_eq!(
            mux.get_active_tab_for_window_for_current_identity(local_window_id)
                .map(|tab| tab.tab_id()),
            Some(local_tab_id)
        );
        assert_eq!(
            mux.get_active_pane_for_window_for_current_identity(local_window_id)
                .map(|pane| pane.pane_id()),
            Some(local_pane_id)
        );
    }

    #[test]
    fn spawn_target_for_window_uses_active_client_pane_when_present() {
        let _test_lock = TEST_MUX_LOCK.lock();
        ensure_test_executor();
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let _guard = MuxGuard;

        let (_domain, inner, client_id, _view_id) = install_client_domain(&mux, "spawn-source");

        let tab = leaf(1, 101, 1001, size(120, 40), true);
        apply_panes(
            &mux,
            inner.clone(),
            client_id.clone(),
            panes_response(vec![tab], 101, 1001),
        );

        let local_window_id = inner.remote_to_local_window(1).unwrap();
        let _identity = mux.with_identity(Some(client_id));
        let target = ClientDomain::spawn_target_for_window(&mux, inner.as_ref(), local_window_id);

        assert_eq!(target, (Some(1), Some(1001)));
    }

    #[test]
    fn spawn_target_for_window_falls_back_to_new_remote_window_without_client_pane() {
        let _test_lock = TEST_MUX_LOCK.lock();
        ensure_test_executor();
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let _guard = MuxGuard;

        let (_domain, inner, client_id, _view_id) = install_client_domain(&mux, "spawn-empty");
        let local_window_id = *mux.new_empty_window(Some("default".to_string()), None);

        let _identity = mux.with_identity(Some(client_id));
        let target = ClientDomain::spawn_target_for_window(&mux, inner.as_ref(), local_window_id);

        assert_eq!(target, (None, None));
    }
}
