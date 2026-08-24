use crate::PKI;
use anyhow::{anyhow, Context};
use codec::*;
use config::TermConfig;
use mux::client::ClientId;
use mux::domain::SplitSource;
use mux::pane::{CachePolicy, Pane, PaneId};
use mux::renderable::{RenderableDimensions, StableCursorPosition};
use mux::tab::{NotifyMux, TabId};
use mux::{Mux, MuxNotification};
use promise::spawn::spawn_into_main_thread;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use termwiz::surface::SequenceNo;
use url::Url;
use wakterm_term::terminal::Alert;
use wakterm_term::StableRowIndex;

const MAX_PENDING_PANE_ALERTS: usize = 256;

#[derive(Clone)]
pub struct PduSender {
    func: Arc<dyn Fn(QueuedPdu) -> anyhow::Result<()> + Send + Sync>,
}

impl PduSender {
    pub fn send(&self, pdu: DecodedPdu) -> anyhow::Result<()> {
        (self.func)(QueuedPdu {
            decoded: pdu,
            queued_at: Instant::now(),
        })
    }

    pub fn new<T>(f: T) -> Self
    where
        T: Fn(QueuedPdu) -> anyhow::Result<()> + Send + Sync + 'static,
    {
        Self { func: Arc::new(f) }
    }
}

pub struct QueuedPdu {
    pub decoded: DecodedPdu,
    pub queued_at: Instant,
}

pub struct RenderBatch {
    pub pdus: Vec<DecodedPdu>,
    pub queued_at: Instant,
    scheduler: RenderScheduler,
}

impl RenderBatch {
    pub fn complete(&self) {
        self.scheduler.complete_one();
    }
}

#[derive(Clone)]
pub struct RenderBatchSender {
    func: Arc<dyn Fn(RenderBatch) -> anyhow::Result<()> + Send + Sync>,
}

impl RenderBatchSender {
    pub fn new<T>(f: T) -> Self
    where
        T: Fn(RenderBatch) -> anyhow::Result<()> + Send + Sync + 'static,
    {
        Self { func: Arc::new(f) }
    }

    fn send(&self, batch: RenderBatch) -> anyhow::Result<()> {
        (self.func)(batch)
    }
}

struct PendingPaneRefresh {
    pane_id: PaneId,
    per_pane: Arc<Mutex<PerPane>>,
}

#[derive(Default)]
struct RenderSchedulerState {
    active: bool,
    dirty: VecDeque<PendingPaneRefresh>,
    dirty_ids: HashSet<PaneId>,
}

#[derive(Clone)]
pub struct RenderScheduler {
    state: Arc<Mutex<RenderSchedulerState>>,
    sender: RenderBatchSender,
}

impl RenderScheduler {
    pub fn new(sender: RenderBatchSender) -> Self {
        Self {
            state: Arc::new(Mutex::new(RenderSchedulerState::default())),
            sender,
        }
    }

    fn schedule(
        &self,
        pane_id: PaneId,
        per_pane: Arc<Mutex<PerPane>>,
        input_serial: Option<InputSerial>,
    ) {
        if let Some(input_serial) = input_serial {
            let mut pane_state = per_pane.lock().unwrap();
            pane_state.pending_input_serial = Some(
                pane_state
                    .pending_input_serial
                    .map_or(input_serial, |prior| prior.max(input_serial)),
            );
        }

        let next = {
            let mut state = self.state.lock().unwrap();
            if state.dirty_ids.insert(pane_id) {
                state
                    .dirty
                    .push_back(PendingPaneRefresh { pane_id, per_pane });
                metrics::counter!("mux_server.render_refresh.queued").increment(1);
            } else {
                metrics::counter!("mux_server.render_refresh.coalesced").increment(1);
            }
            metrics::gauge!("mux_server.render_refresh.dirty_panes").set(state.dirty.len() as f64);

            if state.active {
                None
            } else {
                state.active = true;
                pop_next_refresh(&mut state)
            }
        };

        if let Some(next) = next {
            self.spawn_compute(next);
        }
    }

    fn spawn_compute(&self, refresh: PendingPaneRefresh) {
        let scheduler = self.clone();
        let sender = self.sender.clone();
        let pane_id = refresh.pane_id;
        let per_pane = refresh.per_pane;
        spawn_into_main_thread(async move {
            let start = Instant::now();
            let result = (|| {
                let mux = Mux::get();
                let pane = mux
                    .get_pane(pane_id)
                    .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                collect_pane_changes(&pane, per_pane)
            })();
            metrics::histogram!("mux_server.render_refresh.compute_latency")
                .record(start.elapsed());

            match result {
                Ok(pdus) if pdus.is_empty() => scheduler.complete_one(),
                Ok(pdus) => {
                    let batch = RenderBatch {
                        pdus,
                        queued_at: Instant::now(),
                        scheduler: scheduler.clone(),
                    };
                    if let Err(err) = sender.send(batch) {
                        log::debug!("render connection closed while queueing refresh: {err:#}");
                        scheduler.complete_one();
                    }
                }
                Err(err) => {
                    log::debug!("unable to compute pane {pane_id} refresh: {err:#}");
                    scheduler.complete_one();
                }
            }

            Ok::<(), anyhow::Error>(())
        })
        .detach();
    }

    fn complete_one(&self) {
        let next = {
            let mut state = self.state.lock().unwrap();
            let next = pop_next_refresh(&mut state);
            if next.is_none() {
                state.active = false;
            }
            metrics::gauge!("mux_server.render_refresh.dirty_panes").set(state.dirty.len() as f64);
            next
        };

        if let Some(next) = next {
            self.spawn_compute(next);
        }
    }
}

fn pop_next_refresh(state: &mut RenderSchedulerState) -> Option<PendingPaneRefresh> {
    let next = state.dirty.pop_front()?;
    state.dirty_ids.remove(&next.pane_id);
    Some(next)
}

#[derive(Default, Debug)]
pub(crate) struct PerPane {
    cursor_position: StableCursorPosition,
    title: String,
    working_dir: Option<Url>,
    dimensions: RenderableDimensions,
    mouse_grabbed: bool,
    sent_initial_palette: bool,
    seqno: SequenceNo,
    pending_input_serial: Option<InputSerial>,
    config_generation: usize,
    pub(crate) notifications: Vec<Alert>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AlertStateKey<'a> {
    CurrentWorkingDirectory,
    IconTitle,
    WindowTitle,
    TabTitle,
    Palette,
    UserVar(&'a str),
    OutputSinceFocusLost,
    Progress,
}

fn alert_state_key(alert: &Alert) -> Option<AlertStateKey<'_>> {
    match alert {
        Alert::Bell | Alert::ToastNotification { .. } => None,
        Alert::CurrentWorkingDirectoryChanged => Some(AlertStateKey::CurrentWorkingDirectory),
        Alert::IconTitleChanged(_) => Some(AlertStateKey::IconTitle),
        Alert::WindowTitleChanged(_) => Some(AlertStateKey::WindowTitle),
        Alert::TabTitleChanged(_) => Some(AlertStateKey::TabTitle),
        Alert::PaletteChanged => Some(AlertStateKey::Palette),
        Alert::SetUserVar { name, .. } => Some(AlertStateKey::UserVar(name)),
        Alert::OutputSinceFocusLost => Some(AlertStateKey::OutputSinceFocusLost),
        Alert::Progress(_) => Some(AlertStateKey::Progress),
    }
}

impl PerPane {
    pub(crate) fn push_notification(&mut self, alert: Alert) {
        if let Some(key) = alert_state_key(&alert) {
            if let Some(existing) = self
                .notifications
                .iter_mut()
                .find(|existing| alert_state_key(existing) == Some(key))
            {
                *existing = alert;
                metrics::counter!("mux_server.render_alert.coalesced").increment(1);
                return;
            }
        }

        if self.notifications.len() >= MAX_PENDING_PANE_ALERTS {
            metrics::counter!("mux_server.render_alert.dropped_at_limit").increment(1);
            return;
        }
        self.notifications.push(alert);
    }

    fn compute_changes(
        &mut self,
        pane: &Arc<dyn Pane>,
        force_with_input_serial: Option<InputSerial>,
    ) -> Option<GetPaneRenderChangesResponse> {
        let mut changed = false;
        let mouse_grabbed = pane.is_mouse_grabbed();
        if mouse_grabbed != self.mouse_grabbed {
            changed = true;
        }

        let dims = pane.get_dimensions();
        if dims != self.dimensions {
            changed = true;
        }

        let cursor_position = pane.get_cursor_position();
        if cursor_position != self.cursor_position {
            changed = true;
        }

        let title = pane.get_title();
        if title != self.title {
            changed = true;
        }

        let working_dir = pane.get_current_working_dir(CachePolicy::AllowStale);
        if working_dir != self.working_dir {
            changed = true;
        }

        let old_seqno = self.seqno;
        self.seqno = pane.get_current_seqno();
        let mut all_dirty_lines = pane.get_changed_since(
            0..dims.physical_top + dims.viewport_rows as StableRowIndex,
            old_seqno,
        );
        if !all_dirty_lines.is_empty() {
            changed = true;
        }

        if !changed && !force_with_input_serial.is_some() {
            return None;
        }

        // Figure out what we're going to send as dirty lines vs bonus lines
        let viewport_range =
            dims.physical_top..dims.physical_top + dims.viewport_rows as StableRowIndex;

        let (first_line, lines) = pane.get_lines(viewport_range);
        let mut bonus_lines = lines
            .into_iter()
            .enumerate()
            .filter_map(|(idx, mut line)| {
                let stable_row = first_line + idx as StableRowIndex;
                if all_dirty_lines.contains(stable_row) {
                    all_dirty_lines.remove(stable_row);
                    line.compress_for_scrollback();
                    Some((stable_row, line))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        // Always send the cursor's row, as that tends to the busiest and we don't
        // have a sequencing concept for our idea of the remote state.
        let (cursor_line_idx, mut lines) = pane.get_lines(cursor_position.y..cursor_position.y + 1);
        let mut cursor_line = lines.remove(0);
        cursor_line.compress_for_scrollback();
        bonus_lines.push((cursor_line_idx, cursor_line));

        self.cursor_position = cursor_position;
        self.title = title.clone();
        self.working_dir = working_dir.clone();
        self.dimensions = dims;
        self.mouse_grabbed = mouse_grabbed;

        let bonus_lines = bonus_lines.into();
        Some(GetPaneRenderChangesResponse {
            pane_id: pane.pane_id(),
            mouse_grabbed,
            dirty_lines: all_dirty_lines.iter().cloned().collect(),
            dimensions: dims,
            cursor_position,
            title,
            bonus_lines,
            working_dir: working_dir.map(Into::into),
            input_serial: force_with_input_serial,
            seqno: self.seqno,
        })
    }
}

fn collect_pane_changes(
    pane: &Arc<dyn Pane>,
    per_pane: Arc<Mutex<PerPane>>,
) -> anyhow::Result<Vec<DecodedPdu>> {
    let mut per_pane = per_pane.lock().unwrap();
    let mut pdus = vec![];
    let input_serial = per_pane.pending_input_serial.take();
    if let Some(resp) = per_pane.compute_changes(pane, input_serial) {
        pdus.push(DecodedPdu {
            pdu: Pdu::GetPaneRenderChangesResponse(resp),
            serial: 0,
        });
    }

    let config = config::configuration();
    if per_pane.config_generation != config.generation() {
        per_pane.config_generation = config.generation();
        // If the config changed, it may have changed colors
        // in the palette that we need to push down, so we
        // synthesize a palette change notification to let
        // the client know
        per_pane.push_notification(Alert::PaletteChanged);
        per_pane.sent_initial_palette = true;
    }

    if !per_pane.sent_initial_palette {
        per_pane.push_notification(Alert::PaletteChanged);
        per_pane.sent_initial_palette = true;
    }
    for alert in per_pane.notifications.drain(..) {
        match alert {
            Alert::PaletteChanged => {
                pdus.push(DecodedPdu {
                    pdu: Pdu::SetPalette(SetPalette {
                        pane_id: pane.pane_id(),
                        palette: pane.palette(),
                    }),
                    serial: 0,
                });
            }
            alert => {
                pdus.push(DecodedPdu {
                    pdu: Pdu::NotifyAlert(NotifyAlert {
                        pane_id: pane.pane_id(),
                        alert,
                    }),
                    serial: 0,
                });
            }
        }
    }
    Ok(pdus)
}

pub struct SessionHandler {
    to_write_tx: PduSender,
    render_scheduler: RenderScheduler,
    per_pane: HashMap<TabId, Arc<Mutex<PerPane>>>,
    client_id: Option<Arc<ClientId>>,
    proxy_client_id: Option<ClientId>,
}

impl Drop for SessionHandler {
    fn drop(&mut self) {
        if let Some(client_id) = self.client_id.take() {
            let mux = Mux::get();
            mux.unregister_client(&client_id);
        }
    }
}

impl SessionHandler {
    pub fn new(to_write_tx: PduSender, render_tx: RenderBatchSender) -> Self {
        Self {
            to_write_tx,
            render_scheduler: RenderScheduler::new(render_tx),
            per_pane: HashMap::new(),
            client_id: None,
            proxy_client_id: None,
        }
    }

    pub fn notification_originates_here(&self, origin: Option<&Arc<ClientId>>) -> bool {
        origin
            .zip(self.client_id.as_ref())
            .is_some_and(|(origin, client_id)| origin == client_id)
    }

    pub fn tab_title_for_client(&self, tab_id: TabId) -> codec::TabTitleChanged {
        let mux = Mux::get();
        let _identity = mux.with_identity(self.client_id.clone());
        codec::TabTitleChanged {
            tab_id,
            title: mux.raw_tab_title(tab_id),
            badge: mux.tab_badge_state_for_current_identity(tab_id),
        }
    }

    pub(crate) fn per_pane(&mut self, pane_id: PaneId) -> Arc<Mutex<PerPane>> {
        Arc::clone(
            self.per_pane
                .entry(pane_id)
                .or_insert_with(|| Arc::new(Mutex::new(PerPane::default()))),
        )
    }

    pub fn schedule_pane_push(&mut self, pane_id: PaneId) {
        let per_pane = self.per_pane(pane_id);
        self.render_scheduler.schedule(pane_id, per_pane, None);
    }

    pub fn process_one(&mut self, decoded: DecodedPdu) {
        let start = Instant::now();
        let sender = self.to_write_tx.clone();
        let serial = decoded.serial;

        if let Some(client_id) = &self.client_id {
            if decoded.pdu.is_user_input() {
                Mux::get().client_had_input(client_id);
            }
        }

        let send_response = move |result: anyhow::Result<Pdu>| {
            let pdu = match result {
                Ok(pdu) => pdu,
                Err(err) => Pdu::ErrorResponse(ErrorResponse {
                    reason: format!("Error: {err:#}"),
                }),
            };
            log::trace!("{} processing time {:?}", serial, start.elapsed());
            if let Err(err) = sender.send(DecodedPdu { pdu, serial }) {
                log::debug!("connection closed before response {serial} could be queued: {err:#}");
            }
        };

        fn catch<F, SND>(f: F, send_response: SND)
        where
            F: FnOnce() -> anyhow::Result<Pdu>,
            SND: Fn(anyhow::Result<Pdu>),
        {
            send_response(f());
        }

        match decoded.pdu {
            Pdu::Ping(Ping {}) => send_response(Ok(Pdu::Pong(Pong {}))),
            Pdu::SetWindowWorkspace(SetWindowWorkspace {
                window_id,
                workspace,
            }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let mut window = mux
                                .get_window_mut(window_id)
                                .ok_or_else(|| anyhow!("window {} is invalid", window_id))?;
                            window.set_workspace(&workspace);
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
                .detach();
            }
            Pdu::SetClientId(SetClientId {
                mut client_id,
                view_id,
                is_proxy,
                client_version_string,
            }) => {
                if is_proxy {
                    if self.proxy_client_id.is_none() {
                        // Copy proxy identity, but don't assign it to the mux;
                        // we'll use it to annotate the actual clients own
                        // identity when they send it
                        self.proxy_client_id.replace(client_id);
                    }
                } else {
                    // If this session is a proxy, override the incoming id with
                    // the proxy information so that it is clear what is going
                    // on from the `wakterm cli list-clients` information
                    if let Some(proxy_id) = &self.proxy_client_id {
                        client_id.ssh_auth_sock = proxy_id.ssh_auth_sock.clone();
                        // Note that this `via proxy pid` string is coupled
                        // with the logic in mux/src/ssh_agent
                        client_id.hostname =
                            format!("{} (via proxy pid {})", client_id.hostname, proxy_id.pid);
                    }

                    if let Some(client_version_string) = client_version_string {
                        log::info!(
                            "Client connected: {} from {} (pid {}, version {})",
                            client_id.hostname,
                            client_id.username,
                            client_id.pid,
                            client_version_string,
                        );
                    } else {
                        log::info!(
                            "Client connected: {} from {} (pid {})",
                            client_id.hostname,
                            client_id.username,
                            client_id.pid,
                        );
                    }
                    let client_id = Arc::new(client_id);
                    let view_id = Arc::new(view_id);
                    self.client_id.replace(client_id.clone());
                    let send_response = send_response.clone();
                    log::info!(
                        "SetClientId scheduling register_client for {} (pid {}, view {:?})",
                        client_id.hostname,
                        client_id.pid,
                        view_id.as_ref(),
                    );
                    spawn_into_main_thread(async move {
                        log::info!(
                            "SetClientId main-thread start for {} (pid {}, view {:?})",
                            client_id.hostname,
                            client_id.pid,
                            view_id.as_ref(),
                        );
                        let mux = Mux::get();
                        log::info!(
                            "SetClientId before register_client for {} (pid {}, view {:?})",
                            client_id.hostname,
                            client_id.pid,
                            view_id.as_ref(),
                        );
                        mux.register_client(client_id, view_id);
                        log::info!("SetClientId after register_client");
                        log::info!("SetClientId sending UnitResponse");
                        send_response(Ok(Pdu::UnitResponse(UnitResponse {})));
                        log::info!("SetClientId sent UnitResponse");
                    })
                    .detach();
                    return;
                }
                send_response(Ok(Pdu::UnitResponse(UnitResponse {})))
            }
            Pdu::SetClientActiveTab(SetClientActiveTab { window_id, tab_id }) => {
                let client_id = self.client_id.clone();
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let _identity = mux.with_identity(client_id);
                            if mux
                                .get_window(window_id)
                                .is_some_and(|window| window.is_tab_parked(tab_id))
                            {
                                mux.set_tab_parked(window_id, tab_id, false)?;
                            }
                            mux.set_active_tab_for_current_identity(window_id, tab_id)?;
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
                .detach();
            }
            Pdu::SetFocusedPane(SetFocusedPane { pane_id }) => {
                let client_id = self.client_id.clone();
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let client_id = client_id.ok_or_else(|| {
                                anyhow::anyhow!("SetFocusedPane before SetClientId")
                            })?;
                            mux.set_focused_pane_for_client(client_id.as_ref(), pane_id)?;

                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
                .detach();
            }
            Pdu::GetClientList(GetClientList) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let clients = mux.iter_clients();
                            Ok(Pdu::GetClientListResponse(GetClientListResponse {
                                clients,
                            }))
                        },
                        send_response,
                    )
                })
                .detach();
            }
            Pdu::ListAgents(ListAgents {}) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            Ok(Pdu::ListAgentsResponse(ListAgentsResponse {
                                agents: mux.agent_service().list_agents(),
                            }))
                        },
                        send_response,
                    )
                })
                .detach();
            }
            Pdu::ListAgentsCached(ListAgentsCached {}) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            Ok(Pdu::ListAgentsCachedResponse(ListAgentsCachedResponse {
                                agents: mux.agent_service().list_agents_cached(),
                            }))
                        },
                        send_response,
                    )
                })
                .detach();
            }
            Pdu::GetAgentApiCapabilities(GetAgentApiCapabilities {}) => {
                spawn_into_main_thread(async move {
                    let mux = Mux::get();
                    send_response(Ok(Pdu::GetAgentApiCapabilitiesResponse(
                        GetAgentApiCapabilitiesResponse {
                            capabilities: mux.agent_service().capabilities(),
                        },
                    )));
                })
                .detach();
            }
            Pdu::ListAgentApiCatalog(ListAgentApiCatalog {}) => {
                spawn_into_main_thread(async move {
                    let mux = Mux::get();
                    send_response(Ok(Pdu::ListAgentApiCatalogResponse(
                        ListAgentApiCatalogResponse {
                            catalog: mux.agent_service().catalog(),
                        },
                    )));
                })
                .detach();
            }
            Pdu::AdmitAgentPrompt(AdmitAgentPrompt { request }) => {
                schedule_agent_prompt_admission(request, move |result| {
                    send_response(result.map(|receipt| {
                        Pdu::AdmitAgentPromptResponse(AdmitAgentPromptResponse { receipt })
                    }))
                });
            }
            Pdu::SubmitAgentRequest(SubmitAgentRequest {
                pane_id,
                request_id,
                prompt,
                paste,
                timeout_ms,
            }) => {
                spawn_into_main_thread(async move {
                    let request = {
                        let mux = Mux::get();
                        let target = mux
                            .list_agents_cached()
                            .into_iter()
                            .find(|agent| agent.pane_id == pane_id)
                            .ok_or_else(|| anyhow!("pane {pane_id} is not an adopted agent"));
                        target.and_then(|target| {
                            let incarnation_id =
                                mux::agent_admission::incarnation_id(&target.metadata).ok_or_else(
                                    || anyhow!("target process incarnation is not confirmed"),
                                )?;
                            Ok(mux::agent_admission::AgentPromptAdmissionRequest {
                                request_id,
                                agent_id: target.metadata.agent_id.clone(),
                                incarnation_id,
                                prompt,
                                paste,
                                return_final: true,
                                timeout_ms,
                            })
                        })
                    };
                    match request {
                        Ok(request) => schedule_agent_prompt_admission(request, move |result| {
                            let response = result.and_then(|receipt| {
                                let detail = receipt.detail.clone().unwrap_or_else(|| {
                                    format!("agent prompt admission was {:?}", receipt.status)
                                });
                                match receipt.request {
                                    Some(request) => Ok(Pdu::SubmitAgentRequestResponse(
                                        SubmitAgentRequestResponse { request },
                                    )),
                                    None => Err(anyhow!("{detail}")),
                                }
                            });
                            send_response(response);
                        }),
                        Err(err) => send_response(Err(err)),
                    }
                })
                .detach();
            }
            Pdu::GetAgentRequest(GetAgentRequest { request_id }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            Ok(Pdu::GetAgentRequestResponse(GetAgentRequestResponse {
                                request: mux.agent_service().get_request(&request_id)?,
                            }))
                        },
                        send_response,
                    )
                })
                .detach();
            }
            Pdu::ListAgentRequestEvents(ListAgentRequestEvents {
                after_sequence,
                limit,
            }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            Ok(Pdu::ListAgentRequestEventsResponse(
                                ListAgentRequestEventsResponse {
                                    requests: mux
                                        .agent_service()
                                        .list_request_events(after_sequence, limit.min(1000))?,
                                },
                            ))
                        },
                        send_response,
                    )
                })
                .detach();
            }
            Pdu::CancelAgentRequest(CancelAgentRequest { request_id }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            Ok(Pdu::CancelAgentRequestResponse(
                                CancelAgentRequestResponse {
                                    request: mux.agent_service().cancel_request(&request_id)?,
                                },
                            ))
                        },
                        send_response,
                    )
                })
                .detach();
            }
            Pdu::ReadAgentOutput(ReadAgentOutput {
                agent_id,
                cursor,
                limit,
            }) => {
                spawn_into_main_thread(async move {
                    let prepared = (|| {
                        let mux = Mux::get();
                        mux.agent_service().prepare_output(&agent_id)
                    })();
                    let result = match prepared {
                        Ok(mux::agent_service::PreparedAgentOutput::Immediate(page)) => {
                            Ok(Pdu::ReadAgentOutputResponse(ReadAgentOutputResponse {
                                page,
                            }))
                        }
                        Ok(mux::agent_service::PreparedAgentOutput::Codex(source)) => {
                            promise::spawn::spawn_into_new_thread(move || {
                                source.read_page(cursor.as_deref(), limit as usize)
                            })
                            .await
                            .map(|page| {
                                Pdu::ReadAgentOutputResponse(ReadAgentOutputResponse { page })
                            })
                        }
                        Err(err) => Err(err),
                    };
                    send_response(result);
                })
                .detach();
            }
            Pdu::ReadAgentEvents(ReadAgentEvents {
                after_sequence,
                limit,
            }) => {
                spawn_into_main_thread(async move {
                    let store = Mux::get().agent_service().event_store();
                    let result = promise::spawn::spawn_into_new_thread(move || {
                        store.read_page(after_sequence, limit as usize)
                    })
                    .await
                    .map(|page| Pdu::ReadAgentEventsResponse(ReadAgentEventsResponse { page }));
                    send_response(result);
                })
                .detach();
            }
            Pdu::PrepareCodexLaunch(request) => {
                spawn_into_main_thread(async move {
                    let result = promise::spawn::spawn_into_new_thread(move || {
                        Mux::get().prepare_codex_app_server_launch(request)
                    })
                    .await
                    .map(Pdu::PreparedCodexLaunch);
                    send_response(result);
                })
                .detach();
            }
            Pdu::ListPanes(ListPanes {}) => {
                let client_id = self.client_id.clone();
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let _identity = mux.with_identity(client_id);
                            let mut view_state =
                                mux.client_window_view_state_for_current_identity();
                            let mut tabs = vec![];
                            let mut tab_titles = vec![];
                            let mut effective_tab_titles = vec![];
                            let mut tab_badges = vec![];
                            let mut parked_tab_ids = vec![];
                            let mut window_titles = HashMap::new();
                            for window_id in mux.iter_windows().into_iter() {
                                let effective_titles =
                                    mux.effective_tab_titles_for_window(window_id);
                                let window = mux.get_window(window_id).unwrap();
                                parked_tab_ids.extend(window.parked_tab_ids());
                                window_titles.insert(window_id, window.get_title().to_string());
                                let window_state = view_state.entry(window_id).or_default();
                                if window_state.active_tab_id.is_none() {
                                    window_state.active_tab_id =
                                        window.iter_visible().next().map(|tab| tab.tab_id());
                                }
                                for tab in window.iter() {
                                    let tab_state =
                                        window_state.tabs.entry(tab.tab_id()).or_default();
                                    if tab_state.active_pane_id.is_none() {
                                        tab_state.active_pane_id =
                                            tab.get_active_pane().map(|pane| pane.pane_id());
                                    }
                                    let active_pane_id = window_state
                                        .tabs
                                        .get(&tab.tab_id())
                                        .and_then(|tab_state| tab_state.active_pane_id)
                                        .or_else(|| {
                                            tab.get_active_pane().map(|pane| pane.pane_id())
                                        });
                                    let mut tree =
                                        tab.codec_pane_tree_with_active_pane_id(active_pane_id);
                                    mux.annotate_pane_tree_with_agent_metadata(&mut tree);
                                    tabs.push(tree);
                                    tab_titles.push(mux.raw_tab_title(tab.tab_id()));
                                    effective_tab_titles.push(
                                        effective_titles
                                            .get(&tab.tab_id())
                                            .cloned()
                                            .unwrap_or_default(),
                                    );
                                    tab_badges.push(
                                        mux.tab_badge_state_for_current_identity(tab.tab_id()),
                                    );
                                }
                            }
                            log::trace!("ListPanes {tabs:#?} {tab_badges:?}");
                            Ok(Pdu::ListPanesResponse(ListPanesResponse {
                                tabs,
                                tab_titles,
                                effective_tab_titles,
                                tab_badges,
                                agents: Vec::new(),
                                tab_rss_bytes: HashMap::new(),
                                parked_tab_ids,
                                window_titles,
                                client_window_view_state: view_state,
                            }))
                        },
                        send_response,
                    )
                })
                .detach();
            }
            Pdu::GetPaneStatus(GetPaneStatus {}) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let status = mux.tab_resource_status();
                            Ok(Pdu::GetPaneStatusResponse(GetPaneStatusResponse {
                                sampled_at_ms: status.sampled_at_ms,
                                agents: mux.list_agents_cached(),
                                tab_rss_bytes: status.tab_rss_bytes,
                            }))
                        },
                        send_response,
                    )
                })
                .detach();
            }
            Pdu::SetAgentMetadata(SetAgentMetadata { pane_id, metadata }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            mux.set_agent_metadata(pane_id, metadata)?;
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
                .detach();
            }
            Pdu::ClearAgentMetadata(ClearAgentMetadata { pane_id }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            mux.get_pane(pane_id)
                                .ok_or_else(|| anyhow!("pane {} is invalid", pane_id))?;
                            mux.clear_agent_metadata(pane_id);
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::RenameWorkspace(RenameWorkspace {
                old_workspace,
                new_workspace,
            }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            mux.rename_workspace(&old_workspace, &new_workspace);
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::WriteToPane(WriteToPane { pane_id, data }) => {
                let render_scheduler = self.render_scheduler.clone();
                let per_pane = self.per_pane(pane_id);
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            pane.writer().write_all(&data)?;
                            render_scheduler.schedule(pane_id, per_pane, None);
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    );
                })
                .detach();
            }
            Pdu::EraseScrollbackRequest(EraseScrollbackRequest {
                pane_id,
                erase_mode,
            }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            pane.erase_scrollback(erase_mode);
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    );
                })
                .detach();
            }
            Pdu::KillPane(KillPane { pane_id }) => {
                let render_scheduler = self.render_scheduler.clone();
                let per_pane = self.per_pane(pane_id);
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            pane.kill();
                            mux.remove_pane(pane_id);
                            render_scheduler.schedule(pane_id, per_pane, None);
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    );
                })
                .detach();
            }
            Pdu::SendPaste(SendPaste { pane_id, data }) => {
                let render_scheduler = self.render_scheduler.clone();
                let per_pane = self.per_pane(pane_id);
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            pane.send_paste(&data)?;
                            render_scheduler.schedule(pane_id, per_pane, None);
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::SearchScrollbackRequest(SearchScrollbackRequest {
                pane_id,
                pattern,
                range,
                limit,
            }) => {
                use mux::pane::Pattern;

                async fn do_search(
                    pane_id: TabId,
                    pattern: Pattern,
                    range: std::ops::Range<StableRowIndex>,
                    limit: Option<u32>,
                ) -> anyhow::Result<Pdu> {
                    let mux = Mux::get();
                    let pane = mux
                        .get_pane(pane_id)
                        .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;

                    pane.search(pattern, range, limit).await.map(|results| {
                        Pdu::SearchScrollbackResponse(SearchScrollbackResponse { results })
                    })
                }

                spawn_into_main_thread(async move {
                    promise::spawn::spawn(async move {
                        let result = do_search(pane_id, pattern, range, limit).await;
                        send_response(result);
                    })
                    .detach();
                })
                .detach();
            }

            Pdu::SetPaneZoomed(SetPaneZoomed {
                containing_tab_id,
                pane_id,
                zoomed,
            }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            let tab = mux
                                .get_tab(containing_tab_id)
                                .ok_or_else(|| anyhow!("no such tab {}", containing_tab_id))?;
                            match tab.get_zoomed_pane() {
                                Some(p) => {
                                    let is_zoomed = p.pane_id() == pane_id;
                                    if is_zoomed != zoomed {
                                        tab.set_zoomed(false);
                                        if zoomed {
                                            tab.set_active_pane(&pane, NotifyMux::Yes);
                                            tab.set_zoomed(zoomed);
                                        }
                                    }
                                }
                                None => {
                                    if zoomed {
                                        tab.set_active_pane(&pane, NotifyMux::Yes);
                                        tab.set_zoomed(zoomed);
                                    }
                                }
                            }
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::GetPaneDirection(GetPaneDirection { pane_id, direction }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let (_domain_id, _window_id, tab_id) = mux
                                .resolve_pane_id(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            let tab = mux
                                .get_tab(tab_id)
                                .ok_or_else(|| anyhow!("no such tab {}", tab_id))?;
                            let panes = tab.iter_panes_ignoring_zoom();
                            let pane_id = tab
                                .get_pane_direction(direction, true)
                                .map(|pane_index| panes[pane_index].pane.pane_id());

                            Ok(Pdu::GetPaneDirectionResponse(GetPaneDirectionResponse {
                                pane_id,
                            }))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::ActivatePaneDirection(ActivatePaneDirection { pane_id, direction }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let (_domain_id, _window_id, tab_id) = mux
                                .resolve_pane_id(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            let tab = mux
                                .get_tab(tab_id)
                                .ok_or_else(|| anyhow!("no such tab {}", tab_id))?;
                            tab.activate_pane_direction(direction);
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::Resize(Resize {
                containing_tab_id,
                pane_id,
                size,
            }) => {
                let client_id = self.client_id.clone();
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let _identity = mux.with_identity(client_id);
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            pane.resize(size)?;
                            let tab = mux
                                .get_tab(containing_tab_id)
                                .ok_or_else(|| anyhow!("no such tab {}", containing_tab_id))?;
                            tab.rebuild_splits_sizes_from_contained_panes();
                            mux.notify_tab_resized(containing_tab_id);
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::ResizeTab(ResizeTab { tab_id, pane_sizes }) => {
                let client_id = self.client_id.clone();
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let _identity = mux.with_identity(client_id);
                            // Apply all pane sizes atomically, then rebuild once
                            let tab = mux
                                .get_tab(tab_id)
                                .ok_or_else(|| anyhow!("no such tab {}", tab_id))?;
                            let tab_panes = tab.iter_panes();
                            if pane_sizes.len() != tab_panes.len() {
                                log::error!(
                                    "resize_tab pane count mismatch: tab_id={} batch_panes={} tab_panes={} {}",
                                    tab_id,
                                    pane_sizes.len(),
                                    tab_panes.len(),
                                    tab.debug_size_snapshot()
                                );
                            }

                            let tab_pane_ids = tab_panes
                                .iter()
                                .map(|pane| pane.pane.pane_id())
                                .collect::<Vec<_>>();
                            let mut missing_pane_ids = Vec::new();
                            for (pane_id, size) in &pane_sizes {
                                if !tab_pane_ids.contains(pane_id) {
                                    missing_pane_ids.push(*pane_id);
                                    continue;
                                }
                                match mux.get_pane(*pane_id) {
                                    Some(pane) => pane.resize(*size)?,
                                    None => missing_pane_ids.push(*pane_id),
                                }
                            }
                            if !missing_pane_ids.is_empty() {
                                log::error!(
                                    "resize_tab referenced unknown panes: tab_id={} pane_ids={:?} {}",
                                    tab_id,
                                    missing_pane_ids,
                                    tab.debug_size_snapshot()
                                );
                            }
                            tab.rebuild_splits_sizes_from_contained_panes();
                            tab.log_runtime_invariant_errors("server.resize_tab");
                            mux.notify_tab_resized(tab_id);
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::SendKeyDown(SendKeyDown {
                pane_id,
                event,
                input_serial,
            }) => {
                let render_scheduler = self.render_scheduler.clone();
                let per_pane = self.per_pane(pane_id);
                let input_received_at = start;
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            pane.key_down(event.key, event.modifiers)?;
                            metrics::histogram!("mux_server.input.dispatch_to_pty_latency")
                                .record(input_received_at.elapsed());

                            // Preserve only the newest prediction serial while a
                            // prior render batch is still being written. The next
                            // coalesced delta will include the current cursor.
                            render_scheduler.schedule(pane_id, per_pane, Some(input_serial));
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
                .detach();
            }
            Pdu::SendMouseEvent(SendMouseEvent { pane_id, event }) => {
                let render_scheduler = self.render_scheduler.clone();
                let per_pane = self.per_pane(pane_id);
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            pane.mouse_event(event)?;
                            render_scheduler.schedule(pane_id, per_pane, None);
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::SpawnV2(spawn) => {
                let client_id = self.client_id.clone();
                spawn_into_main_thread(async move {
                    schedule_domain_spawn_v2(spawn, send_response, client_id);
                })
                .detach();
            }

            Pdu::SplitPane(split) => {
                let client_id = self.client_id.clone();
                spawn_into_main_thread(async move {
                    schedule_split_pane(split, send_response, client_id);
                })
                .detach();
            }

            Pdu::MovePaneToNewTab(request) => {
                let client_id = self.client_id.clone();
                spawn_into_main_thread(async move {
                    schedule_move_pane(request, send_response, client_id);
                })
                .detach();
            }

            Pdu::GetPaneRenderableDimensions(GetPaneRenderableDimensions { pane_id }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            let cursor_position = pane.get_cursor_position();
                            let dimensions = pane.get_dimensions();
                            Ok(Pdu::GetPaneRenderableDimensionsResponse(
                                GetPaneRenderableDimensionsResponse {
                                    pane_id,
                                    cursor_position,
                                    dimensions,
                                },
                            ))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::GetPaneRenderChanges(GetPaneRenderChanges { pane_id, .. }) => {
                let render_scheduler = self.render_scheduler.clone();
                let per_pane = self.per_pane(pane_id);
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let is_alive = match mux.get_pane(pane_id) {
                                Some(_) => {
                                    render_scheduler.schedule(pane_id, per_pane, None);
                                    true
                                }
                                None => false,
                            };
                            Ok(Pdu::LivenessResponse(LivenessResponse {
                                pane_id,
                                is_alive,
                            }))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::GetLines(GetLines { pane_id, lines }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            let mut lines_and_indices = vec![];

                            for range in lines {
                                let (first_row, lines) = pane.get_lines(range);
                                for (idx, mut line) in lines.into_iter().enumerate() {
                                    let stable_row = first_row + idx as StableRowIndex;
                                    line.compress_for_scrollback();
                                    lines_and_indices.push((stable_row, line));
                                }
                            }
                            Ok(Pdu::GetLinesResponse(GetLinesResponse {
                                pane_id,
                                lines: lines_and_indices.into(),
                            }))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::GetImageCell(GetImageCell {
                pane_id,
                line_idx,
                cell_idx,
                data_hash,
            }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let mut data = None;

                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;

                            let (_, lines) = pane.get_lines(line_idx..line_idx + 1);
                            'found_data: for line in lines {
                                if let Some(cell) = line.get_cell(cell_idx) {
                                    if let Some(images) = cell.attrs().images() {
                                        for im in images {
                                            if im.image_data().hash() == data_hash {
                                                data.replace(im.image_data().clone());
                                                break 'found_data;
                                            }
                                        }
                                    }
                                }
                            }
                            Ok(Pdu::GetImageCellResponse(GetImageCellResponse {
                                pane_id,
                                data,
                            }))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::GetCodecVersion(_) => {
                log::info!(
                    "Client requested codec version; server is {} (codec {})",
                    config::wakterm_version(),
                    CODEC_VERSION,
                );
                match std::env::current_exe().context("resolving current_exe") {
                    Err(err) => send_response(Err(err)),
                    Ok(executable_path) => {
                        send_response(Ok(Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                            codec_vers: CODEC_VERSION,
                            version_string: config::wakterm_version().to_owned(),
                            executable_path,
                            config_file_path: std::env::var_os("WAKTERM_CONFIG_FILE")
                                .map(Into::into),
                        })))
                    }
                }
            }

            Pdu::GetTlsCreds(_) => {
                catch(
                    move || {
                        let client_cert_pem = PKI.generate_client_cert()?;
                        let ca_cert_pem = PKI.ca_pem_string()?;
                        Ok(Pdu::GetTlsCredsResponse(GetTlsCredsResponse {
                            client_cert_pem,
                            ca_cert_pem,
                        }))
                    },
                    send_response,
                );
            }
            Pdu::WindowTitleChanged(WindowTitleChanged { window_id, title }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let mut window = mux
                                .get_window_mut(window_id)
                                .ok_or_else(|| anyhow!("no such window {window_id}"))?;

                            window.set_title(&title);

                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
                .detach();
            }
            Pdu::TabTitleChanged(TabTitleChanged { tab_id, title, .. }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let tab = mux
                                .get_tab(tab_id)
                                .ok_or_else(|| anyhow!("no such tab {tab_id}"))?;

                            tab.set_title(&title);

                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
                .detach();
            }
            Pdu::SetPalette(SetPalette { pane_id, palette }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;

                            match pane.get_config() {
                                Some(config) => match config.downcast_ref::<TermConfig>() {
                                    Some(tc) => tc.set_client_palette(palette),
                                    None => {
                                        log::error!(
                                            "pane {pane_id} doesn't \
                                            have TermConfig as its config! \
                                            Ignoring client palette update"
                                        );
                                    }
                                },
                                None => {
                                    let config = TermConfig::new();
                                    config.set_client_palette(palette);
                                    pane.set_config(Arc::new(config));
                                }
                            }

                            mux.notify(MuxNotification::Alert {
                                pane_id,
                                alert: Alert::PaletteChanged,
                            });

                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::AdjustPaneSize(AdjustPaneSize {
                pane_id,
                direction,
                amount,
            }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let (_pane_domain_id, _window_id, tab_id) = mux
                                .resolve_pane_id(pane_id)
                                .ok_or_else(|| anyhow!("pane_id {} invalid", pane_id))?;

                            let tab = match mux.get_tab(tab_id) {
                                Some(tab) => tab,
                                None => {
                                    return Err(anyhow!(
                                        "Failed to retrieve tab with ID {}",
                                        tab_id
                                    ));
                                }
                            };

                            tab.adjust_pane_size(direction, amount);
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::RotatePanes(RotatePanes { tab_id, clockwise }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let tab = mux
                                .get_tab(tab_id)
                                .ok_or_else(|| anyhow!("no such tab {}", tab_id))?;
                            if clockwise {
                                tab.rotate_clockwise();
                            } else {
                                tab.rotate_counter_clockwise();
                            }
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::SetTabOrder(SetTabOrder { window_id, tab_ids }) => {
                let client_id = self.client_id.clone();
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let _identity = mux.with_identity(client_id);
                            let changed = mux
                                .get_window_mut(window_id)
                                .ok_or_else(|| anyhow!("no such window {}", window_id))?
                                .apply_tab_order(&tab_ids)?;
                            if changed {
                                mux.notify(MuxNotification::WindowInvalidated(window_id));
                                mux.notify_tab_order_changed(window_id, tab_ids);
                            }
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::SetParkedTabs(SetParkedTabs {
                window_id,
                tab_ids,
                parked_tab_ids,
            }) => {
                let client_id = self.client_id.clone();
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let _identity = mux.with_identity(client_id);
                            let changed = mux
                                .get_window_mut(window_id)
                                .ok_or_else(|| anyhow!("no such window {}", window_id))?
                                .apply_parked_tabs(&tab_ids, &parked_tab_ids)?;
                            if changed {
                                mux.repair_client_views_after_parked_change(window_id);
                                mux.notify(MuxNotification::WindowInvalidated(window_id));
                                mux.notify_parked_tabs_changed(window_id, tab_ids, parked_tab_ids);
                            }
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::AcknowledgeAgentAttention(AcknowledgeAgentAttention { pane_id }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            anyhow::ensure!(
                                mux.get_pane(pane_id).is_some(),
                                "no such pane {pane_id}"
                            );
                            mux.acknowledge_agent_attention(pane_id);
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::Invalid { .. } => send_response(Err(anyhow!("invalid PDU {:?}", decoded.pdu))),
            Pdu::Pong { .. }
            | Pdu::ListPanesResponse { .. }
            | Pdu::GetPaneStatusResponse { .. }
            | Pdu::ListAgentsResponse { .. }
            | Pdu::ListAgentsCachedResponse { .. }
            | Pdu::SubmitAgentRequestResponse { .. }
            | Pdu::GetAgentRequestResponse { .. }
            | Pdu::ListAgentRequestEventsResponse { .. }
            | Pdu::CancelAgentRequestResponse { .. }
            | Pdu::ReadAgentOutputResponse { .. }
            | Pdu::ReadAgentEventsResponse { .. }
            | Pdu::GetAgentApiCapabilitiesResponse { .. }
            | Pdu::ListAgentApiCatalogResponse { .. }
            | Pdu::AdmitAgentPromptResponse { .. }
            | Pdu::PreparedCodexLaunch { .. }
            | Pdu::SetClipboard { .. }
            | Pdu::NotifyAlert { .. }
            | Pdu::SpawnResponse { .. }
            | Pdu::GetPaneRenderChangesResponse { .. }
            | Pdu::UnitResponse { .. }
            | Pdu::LivenessResponse { .. }
            | Pdu::GetPaneDirectionResponse { .. }
            | Pdu::SearchScrollbackResponse { .. }
            | Pdu::GetLinesResponse { .. }
            | Pdu::GetCodecVersionResponse { .. }
            | Pdu::WindowWorkspaceChanged { .. }
            | Pdu::GetTlsCredsResponse { .. }
            | Pdu::GetClientListResponse { .. }
            | Pdu::PaneRemoved { .. }
            | Pdu::PaneFocused { .. }
            | Pdu::TabResized { .. }
            | Pdu::TabOrderChanged { .. }
            | Pdu::ParkedTabsChanged { .. }
            | Pdu::AgentMetadataChanged { .. }
            | Pdu::GetImageCellResponse { .. }
            | Pdu::MovePaneToNewTabResponse { .. }
            | Pdu::TabAddedToWindow { .. }
            | Pdu::GetPaneRenderableDimensionsResponse { .. }
            | Pdu::ErrorResponse { .. } => {
                send_response(Err(anyhow!("expected a request, got {:?}", decoded.pdu)))
            }
        }
    }

    pub fn wants_mux_notifications(&self) -> bool {
        self.client_id.is_some()
    }
}

fn schedule_agent_prompt_admission<SND>(
    request: mux::agent_admission::AgentPromptAdmissionRequest,
    send_response: SND,
) where
    SND: FnOnce(anyhow::Result<mux::agent_admission::AgentAdmissionReceipt>) + Send + 'static,
{
    spawn_into_main_thread(async move {
        send_response(admit_agent_prompt(request).await);
    })
    .detach();
}

async fn admit_agent_prompt(
    request: mux::agent_admission::AgentPromptAdmissionRequest,
) -> anyhow::Result<mux::agent_admission::AgentAdmissionReceipt> {
    use mux::agent_admission::{
        request_matches_admission, AgentAdmissionCapture, AgentAdmissionReceipt,
        AgentAdmissionStatus, OneWayAdmissionClaim,
    };
    use mux::agent_request::AgentRequestState;

    let (request_store, admission_store) = {
        let mux = Mux::get();
        let service = mux.agent_service();
        (service.request_store(), service.admission_store())
    };

    if request.return_final {
        let store = request_store.clone();
        let request_id = request.request_id.clone();
        let existing =
            match promise::spawn::spawn_into_new_thread(move || store.get(&request_id)).await {
                Ok(existing) => existing,
                Err(err) => {
                    return Ok(AgentAdmissionReceipt::rejected(
                        &request,
                        AgentAdmissionStatus::InternalFailure,
                        format!("could not read durable admission state: {err:#}"),
                    ));
                }
            };
        if let Some(existing) = existing {
            if !request_matches_admission(&existing, &request) {
                return Ok(AgentAdmissionReceipt::rejected(
                    &request,
                    AgentAdmissionStatus::Invalid,
                    "request_id was already used for different input",
                ));
            }
            return Ok(
                if matches!(
                    existing.state,
                    AgentRequestState::Registered
                        | AgentRequestState::DeliveryFailed
                        | AgentRequestState::Indeterminate
                ) {
                    AgentAdmissionReceipt::indeterminate(
                        &request,
                        Some(existing),
                        "the prior return-final admission has indeterminate delivery",
                    )
                } else {
                    AgentAdmissionReceipt::accepted(&request, Some(existing))
                },
            );
        }
    } else {
        let store = admission_store.clone();
        let claim_request = request.clone();
        let claim = match promise::spawn::spawn_into_new_thread(move || store.claim(&claim_request))
            .await
        {
            Ok(claim) => claim,
            Err(err) => {
                return Ok(AgentAdmissionReceipt::rejected(
                    &request,
                    AgentAdmissionStatus::InternalFailure,
                    format!("could not durably claim prompt admission: {err:#}"),
                ));
            }
        };
        match claim {
            OneWayAdmissionClaim::New => {}
            OneWayAdmissionClaim::Existing(receipt) => return Ok(receipt),
            OneWayAdmissionClaim::Conflict => {
                return Ok(AgentAdmissionReceipt::rejected(
                    &request,
                    AgentAdmissionStatus::Invalid,
                    "request_id was already used for different input",
                ));
            }
        }
    }

    let candidate = match Mux::get()
        .agent_service()
        .capture_admission(request.clone())
    {
        AgentAdmissionCapture::Candidate(candidate) => candidate,
        AgentAdmissionCapture::Rejected(receipt) => {
            if let Err(err) =
                release_unwritten_claim(&request, request_store.clone(), admission_store.clone())
                    .await
            {
                return Ok(AgentAdmissionReceipt::rejected(
                    &request,
                    AgentAdmissionStatus::InternalFailure,
                    format!("prompt was not written, but its admission claim could not be released: {err:#}"),
                ));
            }
            return Ok(receipt);
        }
    };
    let candidate = match promise::spawn::spawn_into_new_thread(move || Ok(candidate.refresh()))
        .await
    {
        Ok(candidate) => candidate,
        Err(err) => {
            let _ =
                release_unwritten_claim(&request, request_store.clone(), admission_store.clone())
                    .await;
            return Ok(AgentAdmissionReceipt::rejected(
                &request,
                AgentAdmissionStatus::InternalFailure,
                format!("could not refresh target observation: {err:#}"),
            ));
        }
    };

    if let Some(receipt) = Mux::get().agent_service().validate_admission(&candidate) {
        if let Err(err) =
            release_unwritten_claim(&request, request_store.clone(), admission_store.clone()).await
        {
            return Ok(AgentAdmissionReceipt::rejected(
                &request,
                AgentAdmissionStatus::InternalFailure,
                format!("prompt was not written, but its admission claim could not be released: {err:#}"),
            ));
        }
        return Ok(receipt);
    }

    let mut return_request = match candidate.proposed_return_request() {
        Ok(request) => request,
        Err(receipt) => {
            if let Err(cleanup_err) =
                release_unwritten_claim(&request, request_store.clone(), admission_store.clone())
                    .await
            {
                return Ok(AgentAdmissionReceipt::rejected(
                    &request,
                    AgentAdmissionStatus::InternalFailure,
                    format!("prompt was not written, but its admission claim could not be released after observer failure: {cleanup_err:#}"),
                ));
            }
            return Ok(receipt);
        }
    };
    if let Some(proposed) = return_request.as_ref() {
        let store = request_store.clone();
        let proposed = proposed.clone();
        let (stored, created) =
            match promise::spawn::spawn_into_new_thread(move || store.create(&proposed)).await {
                Ok(result) => result,
                Err(err) => {
                    return Ok(AgentAdmissionReceipt::rejected(
                        &request,
                        AgentAdmissionStatus::InternalFailure,
                        format!("could not durably prepare return-final admission: {err:#}"),
                    ));
                }
            };
        if !created {
            return Ok(if request_matches_admission(&stored, &request) {
                AgentAdmissionReceipt::accepted(&request, Some(stored))
            } else {
                AgentAdmissionReceipt::rejected(
                    &request,
                    AgentAdmissionStatus::Invalid,
                    "request_id was already used for different input",
                )
            });
        }
        return_request = Some(stored);
    }

    if let Some(receipt) = Mux::get().agent_service().validate_admission(&candidate) {
        if let Err(err) =
            release_unwritten_claim(&request, request_store.clone(), admission_store.clone()).await
        {
            return Ok(AgentAdmissionReceipt::rejected(
                &request,
                AgentAdmissionStatus::InternalFailure,
                format!("prompt was not written, but its admission claim could not be released: {err:#}"),
            ));
        }
        return Ok(receipt);
    }

    let delivery = Mux::get().agent_service().write_admitted_prompt(&candidate);
    match delivery {
        Ok(()) => {
            if let Some(mut nested) = return_request {
                nested.mark_submitted();
                let store = request_store;
                let save_result = promise::spawn::spawn_into_new_thread(move || {
                    store.save(&mut nested)?;
                    Ok(nested)
                })
                .await;
                match save_result {
                    Ok(saved) => Ok(AgentAdmissionReceipt::accepted(&request, Some(saved))),
                    Err(err) => Ok(AgentAdmissionReceipt::indeterminate(
                        &request,
                        None,
                        format!(
                            "prompt was written but its receipt could not be persisted: {err:#}"
                        ),
                    )),
                }
            } else {
                let receipt = AgentAdmissionReceipt::accepted(&request, None);
                let store = admission_store;
                let stored_receipt = receipt.clone();
                match promise::spawn::spawn_into_new_thread(move || store.finish(&stored_receipt))
                    .await
                {
                    Ok(()) => Ok(receipt),
                    Err(err) => Ok(AgentAdmissionReceipt::indeterminate(
                        &request,
                        None,
                        format!(
                            "prompt was written but its receipt could not be persisted: {err:#}"
                        ),
                    )),
                }
            }
        }
        Err(err) => {
            let detail = format!("prompt delivery may have been partial: {err:#}");
            if let Some(nested) = return_request {
                let mut nested =
                    mux::agent_admission::reconcile_written_request_after_failure(nested, &detail);
                let store = request_store;
                let nested_for_receipt = nested.clone();
                let _ =
                    promise::spawn::spawn_into_new_thread(move || store.save(&mut nested)).await;
                Ok(AgentAdmissionReceipt::indeterminate(
                    &request,
                    Some(nested_for_receipt),
                    detail,
                ))
            } else {
                let receipt = AgentAdmissionReceipt::indeterminate(&request, None, detail);
                let store = admission_store;
                let stored_receipt = receipt.clone();
                let _ =
                    promise::spawn::spawn_into_new_thread(move || store.finish(&stored_receipt))
                        .await;
                Ok(receipt)
            }
        }
    }
}

async fn release_unwritten_claim(
    request: &mux::agent_admission::AgentPromptAdmissionRequest,
    request_store: mux::agent_request::AgentRequestStore,
    admission_store: mux::agent_admission::AgentAdmissionStore,
) -> anyhow::Result<()> {
    if request.return_final {
        let request_id = request.request_id.clone();
        promise::spawn::spawn_into_new_thread(move || request_store.delete_registered(&request_id))
            .await
    } else {
        let request_id = request.request_id.clone();
        promise::spawn::spawn_into_new_thread(move || {
            admission_store.release_unwritten(&request_id)
        })
        .await
    }
}

// Dancing around a little bit here; we can't directly spawn_into_main_thread the domain_spawn
// function below because the compiler thinks that all of its locals then need to be Send.
// We need to shimmy through this helper to break that aspect of the compiler flow
// analysis and allow things to compile.
fn schedule_domain_spawn_v2<SND>(
    spawn: SpawnV2,
    send_response: SND,
    client_id: Option<Arc<ClientId>>,
) where
    SND: Fn(anyhow::Result<Pdu>) + 'static,
{
    promise::spawn::spawn(async move { send_response(domain_spawn_v2(spawn, client_id).await) })
        .detach();
}

fn schedule_split_pane<SND>(split: SplitPane, send_response: SND, client_id: Option<Arc<ClientId>>)
where
    SND: Fn(anyhow::Result<Pdu>) + 'static,
{
    promise::spawn::spawn(async move { send_response(split_pane(split, client_id).await) })
        .detach();
}

async fn split_pane(split: SplitPane, client_id: Option<Arc<ClientId>>) -> anyhow::Result<Pdu> {
    let mux = Mux::get();
    let _identity = mux.with_identity(client_id);

    let (_pane_domain_id, window_id, tab_id) = mux
        .resolve_pane_id(split.pane_id)
        .ok_or_else(|| anyhow!("pane_id {} invalid", split.pane_id))?;

    // If the client provided its tab size, resize the tab first so the
    // split uses the client's actual dimensions rather than the server's
    // potentially stale size. This fixes the race where split-pane runs
    // before the client's resize PDU has been processed.
    if let Some(tab_size) = split.tab_size {
        if let Some(tab) = mux.get_tab(tab_id) {
            tab.resize(tab_size);
        }
    }

    let source = if let Some(move_pane_id) = split.move_pane_id {
        SplitSource::MovePane(move_pane_id)
    } else {
        SplitSource::Spawn {
            command: split.command,
            command_dir: split.command_dir,
        }
    };

    let (pane, size) = mux
        .split_pane(split.pane_id, split.split_request, source, split.domain)
        .await?;

    Ok::<Pdu, anyhow::Error>(Pdu::SpawnResponse(SpawnResponse {
        pane_id: pane.pane_id(),
        tab_id,
        window_id,
        size,
    }))
}

async fn domain_spawn_v2(spawn: SpawnV2, client_id: Option<Arc<ClientId>>) -> anyhow::Result<Pdu> {
    let mux = Mux::get();
    let _identity = mux.with_identity(client_id);

    let (tab, pane, window_id) = mux
        .spawn_tab_or_window(
            spawn.window_id,
            spawn.domain,
            spawn.command,
            spawn.command_dir,
            spawn.size,
            spawn.current_pane_id,
            spawn.workspace,
            None, // optional gui window position
        )
        .await?;

    Ok::<Pdu, anyhow::Error>(Pdu::SpawnResponse(SpawnResponse {
        pane_id: pane.pane_id(),
        tab_id: tab.tab_id(),
        window_id,
        size: tab.get_size(),
    }))
}

fn schedule_move_pane<SND>(
    request: MovePaneToNewTab,
    send_response: SND,
    client_id: Option<Arc<ClientId>>,
) where
    SND: Fn(anyhow::Result<Pdu>) + 'static,
{
    promise::spawn::spawn(async move { send_response(move_pane(request, client_id).await) })
        .detach();
}

async fn move_pane(
    request: MovePaneToNewTab,
    client_id: Option<Arc<ClientId>>,
) -> anyhow::Result<Pdu> {
    let mux = Mux::get();
    let _identity = mux.with_identity(client_id);

    let (tab, window_id) = mux
        .move_pane_to_new_tab(
            request.pane_id,
            request.window_id,
            request.workspace_for_new_window,
        )
        .await?;

    Ok::<Pdu, anyhow::Error>(Pdu::MovePaneToNewTabResponse(MovePaneToNewTabResponse {
        tab_id: tab.tab_id(),
        window_id,
    }))
}

#[cfg(test)]
mod test {
    use super::*;
    use chrono::{TimeZone, Utc};
    use mux::agent::AgentMetadata;
    use mux::client::{ClientTabViewState, ClientViewId};
    use mux::pane::{alloc_pane_id, CachePolicy, LogicalLine, Pane};
    use mux::renderable::RenderableDimensions;
    use mux::tab::{SplitDirection, SplitRequest, SplitSize, Tab};
    use mux::window::WindowId;
    use promise::spawn::SimpleExecutor;
    use rangeset::RangeSet;
    use std::io::Write;
    use std::ops::Range;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use termwiz::surface::{CursorShape, CursorVisibility, Line, SequenceNo};
    use url::Url;
    use wakterm_term::color::ColorPalette;
    use wakterm_term::{KeyCode, KeyModifiers, MouseEvent, StableRowIndex, TerminalSize};

    static TEST_AGENT_DB_ID: AtomicUsize = AtomicUsize::new(0);

    fn test_mux() -> Arc<Mux> {
        Arc::new(Mux::new_with_agent_state_path(
            None,
            std::env::temp_dir().join(format!(
                "wakterm-sessionhandler-test-{}-{}.sqlite3",
                std::process::id(),
                TEST_AGENT_DB_ID.fetch_add(1, Ordering::Relaxed)
            )),
        ))
    }

    struct TestPane {
        id: PaneId,
        size: Mutex<TerminalSize>,
        title: String,
        foreground_process_name: Option<String>,
        panic_on_process_name: bool,
        panic_on_working_dir: bool,
        panic_on_focus_changed: bool,
        key_down_count: Option<Arc<AtomicUsize>>,
    }

    impl TestPane {
        fn new(id: PaneId, size: TerminalSize, title: &str) -> Arc<dyn Pane> {
            Arc::new(Self {
                id,
                size: Mutex::new(size),
                title: title.to_string(),
                foreground_process_name: None,
                panic_on_process_name: false,
                panic_on_working_dir: false,
                panic_on_focus_changed: false,
                key_down_count: None,
            })
        }

        fn new_with_process(
            id: PaneId,
            size: TerminalSize,
            title: &str,
            foreground_process_name: &str,
        ) -> Arc<dyn Pane> {
            Arc::new(Self {
                id,
                size: Mutex::new(size),
                title: title.to_string(),
                foreground_process_name: Some(foreground_process_name.to_string()),
                panic_on_process_name: false,
                panic_on_working_dir: false,
                panic_on_focus_changed: false,
                key_down_count: None,
            })
        }

        fn new_for_listing_regression(
            id: PaneId,
            size: TerminalSize,
            title: &str,
            foreground_process_name: &str,
        ) -> Arc<dyn Pane> {
            Arc::new(Self {
                id,
                size: Mutex::new(size),
                title: title.to_string(),
                foreground_process_name: Some(foreground_process_name.to_string()),
                panic_on_process_name: true,
                panic_on_working_dir: true,
                panic_on_focus_changed: false,
                key_down_count: None,
            })
        }

        fn new_for_focus_regression(id: PaneId, size: TerminalSize, title: &str) -> Arc<dyn Pane> {
            Arc::new(Self {
                id,
                size: Mutex::new(size),
                title: title.to_string(),
                foreground_process_name: None,
                panic_on_process_name: false,
                panic_on_working_dir: false,
                panic_on_focus_changed: true,
                key_down_count: None,
            })
        }

        fn new_for_input_latency(
            id: PaneId,
            size: TerminalSize,
            title: &str,
        ) -> (Arc<dyn Pane>, Arc<AtomicUsize>) {
            let key_down_count = Arc::new(AtomicUsize::new(0));
            (
                Arc::new(Self {
                    id,
                    size: Mutex::new(size),
                    title: title.to_string(),
                    foreground_process_name: None,
                    panic_on_process_name: false,
                    panic_on_working_dir: false,
                    panic_on_focus_changed: false,
                    key_down_count: Some(Arc::clone(&key_down_count)),
                }),
                key_down_count,
            )
        }
    }

    impl Pane for TestPane {
        fn pane_id(&self) -> PaneId {
            self.id
        }

        fn get_cursor_position(&self) -> StableCursorPosition {
            StableCursorPosition {
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
            _with_lines: &mut dyn mux::pane::WithPaneLines,
        ) {
            unimplemented!()
        }

        fn for_each_logical_line_in_stable_range_mut(
            &self,
            _lines: Range<StableRowIndex>,
            _for_line: &mut dyn mux::pane::ForEachPaneLogicalLine,
        ) {
            unimplemented!()
        }

        fn get_lines(&self, lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
            let width = self.size.lock().unwrap().cols;
            let first = lines.start;
            let count = lines.end.saturating_sub(lines.start) as usize;
            (
                first,
                (0..count).map(|_| Line::with_width(width, 0)).collect(),
            )
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
            self.title.clone()
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
            if let Some(count) = &self.key_down_count {
                count.fetch_add(1, Ordering::SeqCst);
            }
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

        fn domain_id(&self) -> mux::domain::DomainId {
            0
        }

        fn is_mouse_grabbed(&self) -> bool {
            false
        }

        fn is_alt_screen_active(&self) -> bool {
            false
        }

        fn get_current_working_dir(&self, _policy: CachePolicy) -> Option<Url> {
            assert!(
                !self.panic_on_working_dir,
                "ListPanes should not synchronously inspect pane working directory"
            );
            None
        }

        fn get_working_dir_for_listing(&self) -> Option<Url> {
            None
        }

        fn get_foreground_process_name(&self, _policy: CachePolicy) -> Option<String> {
            assert!(
                !self.panic_on_process_name,
                "ListPanes should not synchronously inspect foreground process name"
            );
            self.foreground_process_name.clone()
        }

        fn focus_changed(&self, _focused: bool) {
            assert!(
                !self.panic_on_focus_changed,
                "SetFocusedPane should not synthesize pane focus changes"
            );
        }
    }

    struct MuxGuard;

    impl Drop for MuxGuard {
        fn drop(&mut self) {
            Mux::shutdown();
        }
    }

    lazy_static::lazy_static! {
        static ref TEST_MUX_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
    }

    struct HandlerHarness {
        handler: SessionHandler,
        responses: smol::channel::Receiver<DecodedPdu>,
        render_batches: smol::channel::Receiver<RenderBatch>,
    }

    impl HandlerHarness {
        fn new(client_id: Arc<ClientId>) -> Self {
            let (tx, rx) = smol::channel::unbounded();
            let sender = PduSender::new(move |queued| {
                tx.try_send(queued.decoded).unwrap();
                Ok(())
            });
            let (render_tx, render_rx) = smol::channel::bounded(1);
            let render_sender = RenderBatchSender::new(move |batch| {
                render_tx.try_send(batch).unwrap();
                Ok(())
            });
            let mut handler = SessionHandler::new(sender, render_sender);
            handler.client_id = Some(client_id);
            Self {
                handler,
                responses: rx,
                render_batches: render_rx,
            }
        }

        fn new_unregistered() -> Self {
            let (tx, rx) = smol::channel::unbounded();
            let sender = PduSender::new(move |queued| {
                tx.try_send(queued.decoded).unwrap();
                Ok(())
            });
            let (render_tx, render_rx) = smol::channel::bounded(1);
            let render_sender = RenderBatchSender::new(move |batch| {
                render_tx.try_send(batch).unwrap();
                Ok(())
            });
            Self {
                handler: SessionHandler::new(sender, render_sender),
                responses: rx,
                render_batches: render_rx,
            }
        }

        fn request(&mut self, executor: &SimpleExecutor, pdu: Pdu) -> Pdu {
            self.handler.process_one(DecodedPdu { pdu, serial: 1 });
            loop {
                if let Ok(decoded) = self.responses.try_recv() {
                    return decoded.pdu;
                }
                executor.tick().unwrap();
            }
        }
    }

    #[test]
    fn resize_notifications_are_suppressed_only_for_the_originating_client() {
        let client_a = Arc::new(ClientId::new());
        let client_b = Arc::new(ClientId::new());
        let mut handler_a = HandlerHarness::new(client_a.clone());
        let mut handler_b = HandlerHarness::new(client_b.clone());
        let unregistered = HandlerHarness::new_unregistered();

        assert!(handler_a
            .handler
            .notification_originates_here(Some(&client_a)));
        assert!(!handler_b
            .handler
            .notification_originates_here(Some(&client_a)));
        assert!(!unregistered
            .handler
            .notification_originates_here(Some(&client_a)));
        assert!(!handler_a.handler.notification_originates_here(None));

        handler_a.handler.client_id.take();
        handler_b.handler.client_id.take();
    }

    #[test]
    fn tab_order_requests_are_atomic_strict_and_last_accepted_wins() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let executor = SimpleExecutor::new();
        let mux = test_mux();
        Mux::set_mux(&mux);
        let _guard = MuxGuard;
        let layout = build_test_layout(&mux);
        let (client_a, view_a, mut handler_a) = register_test_client(&mux, "tab-order-a");
        let (client_b, _view_b, mut handler_b) = register_test_client(&mux, "tab-order-b");
        mux.set_active_tab_for_client_view(view_a.as_ref(), layout.window_id, layout.right_tab_id)
            .unwrap();

        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_for_subscription = observed.clone();
        mux.subscribe(move |notification| {
            if let MuxNotification::TabOrderChanged {
                window_id,
                tab_ids,
                origin,
            } = notification
            {
                observed_for_subscription
                    .lock()
                    .unwrap()
                    .push((window_id, tab_ids, origin));
            }
            true
        });

        let cba = vec![layout.split_tab_id, layout.right_tab_id, layout.left_tab_id];
        assert!(matches!(
            handler_a.request(
                &executor,
                Pdu::SetTabOrder(SetTabOrder {
                    window_id: layout.window_id,
                    tab_ids: cba.clone(),
                })
            ),
            Pdu::UnitResponse(_)
        ));
        assert_eq!(
            mux.get_window(layout.window_id)
                .unwrap()
                .iter()
                .map(|tab| tab.tab_id())
                .collect::<Vec<_>>(),
            cba
        );
        assert_eq!(
            mux.get_active_tab_for_window_for_client(view_a.as_ref(), layout.window_id)
                .map(|tab| tab.tab_id()),
            Some(layout.right_tab_id)
        );

        let listed = match handler_b.request(&executor, Pdu::ListPanes(ListPanes {})) {
            Pdu::ListPanesResponse(response) => response,
            other => panic!("expected ListPanesResponse, got {:?}", other),
        };
        assert_eq!(
            listed
                .tabs
                .iter()
                .filter_map(|tab| tab.window_and_tab_ids().map(|(_, tab_id)| tab_id))
                .collect::<Vec<_>>(),
            cba
        );

        for invalid in [
            vec![layout.left_tab_id, layout.left_tab_id, layout.split_tab_id],
            vec![layout.left_tab_id, layout.right_tab_id],
        ] {
            assert!(matches!(
                handler_a.request(
                    &executor,
                    Pdu::SetTabOrder(SetTabOrder {
                        window_id: layout.window_id,
                        tab_ids: invalid,
                    })
                ),
                Pdu::ErrorResponse(_)
            ));
            assert_eq!(
                mux.get_window(layout.window_id)
                    .unwrap()
                    .iter()
                    .map(|tab| tab.tab_id())
                    .collect::<Vec<_>>(),
                cba
            );
        }

        let abc = vec![layout.left_tab_id, layout.right_tab_id, layout.split_tab_id];
        assert!(matches!(
            handler_b.request(
                &executor,
                Pdu::SetTabOrder(SetTabOrder {
                    window_id: layout.window_id,
                    tab_ids: abc.clone(),
                })
            ),
            Pdu::UnitResponse(_)
        ));
        assert_eq!(
            mux.get_window(layout.window_id)
                .unwrap()
                .iter()
                .map(|tab| tab.tab_id())
                .collect::<Vec<_>>(),
            abc
        );

        let observed = observed.lock().unwrap();
        assert_eq!(observed.len(), 2);
        assert_eq!(observed[0].0, layout.window_id);
        assert_eq!(observed[0].1, cba);
        assert_eq!(observed[0].2.as_ref(), Some(&client_a));
        assert_eq!(observed[1].1, abc);
        assert_eq!(observed[1].2.as_ref(), Some(&client_b));
    }

    #[test]
    fn parked_tab_requests_are_atomic_shared_and_repair_every_client_view() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let executor = SimpleExecutor::new();
        let mux = test_mux();
        Mux::set_mux(&mux);
        let _guard = MuxGuard;
        let layout = build_test_layout(&mux);
        let (client_a, view_a, mut handler_a) = register_test_client(&mux, "parked-a");
        let (client_b, view_b, mut handler_b) = register_test_client(&mux, "parked-b");
        for view in [&view_a, &view_b] {
            mux.set_active_tab_for_client_view(
                view.as_ref(),
                layout.window_id,
                layout.right_tab_id,
            )
            .unwrap();
        }
        mux.set_focused_pane_for_client(client_a.as_ref(), layout.right_pane_id)
            .unwrap();
        mux.set_focused_pane_for_client(client_b.as_ref(), layout.right_pane_id)
            .unwrap();

        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_for_subscription = observed.clone();
        mux.subscribe(move |notification| {
            if let MuxNotification::ParkedTabsChanged {
                window_id,
                tab_ids,
                parked_tab_ids,
                origin,
            } = notification
            {
                observed_for_subscription.lock().unwrap().push((
                    window_id,
                    tab_ids,
                    parked_tab_ids,
                    origin,
                ));
            }
            true
        });

        let tab_ids = vec![layout.left_tab_id, layout.right_tab_id, layout.split_tab_id];
        assert!(matches!(
            handler_a.request(
                &executor,
                Pdu::SetParkedTabs(SetParkedTabs {
                    window_id: layout.window_id,
                    tab_ids: tab_ids.clone(),
                    parked_tab_ids: vec![layout.right_tab_id],
                })
            ),
            Pdu::UnitResponse(_)
        ));

        let window = mux.get_window(layout.window_id).unwrap();
        assert_eq!(window.parked_tab_ids(), vec![layout.right_tab_id]);
        assert_eq!(
            window
                .iter_visible()
                .map(|tab| tab.tab_id())
                .collect::<Vec<_>>(),
            vec![layout.left_tab_id, layout.split_tab_id]
        );
        drop(window);
        for view in [&view_a, &view_b] {
            assert_eq!(
                mux.get_active_tab_for_window_for_client(view.as_ref(), layout.window_id)
                    .map(|tab| tab.tab_id()),
                Some(layout.split_tab_id)
            );
        }
        for client in [&client_a, &client_b] {
            assert_eq!(
                mux.resolve_focused_pane(client.as_ref())
                    .map(|(_, _, tab_id, _)| tab_id),
                Some(layout.split_tab_id)
            );
        }

        let listed = match handler_b.request(&executor, Pdu::ListPanes(ListPanes {})) {
            Pdu::ListPanesResponse(response) => response,
            other => panic!("expected ListPanesResponse, got {:?}", other),
        };
        assert_eq!(listed.parked_tab_ids, vec![layout.right_tab_id]);

        for parked_tab_ids in [
            vec![layout.right_tab_id, layout.right_tab_id],
            tab_ids.clone(),
        ] {
            assert!(matches!(
                handler_a.request(
                    &executor,
                    Pdu::SetParkedTabs(SetParkedTabs {
                        window_id: layout.window_id,
                        tab_ids: tab_ids.clone(),
                        parked_tab_ids,
                    })
                ),
                Pdu::ErrorResponse(_)
            ));
            assert_eq!(
                mux.get_window(layout.window_id).unwrap().parked_tab_ids(),
                vec![layout.right_tab_id]
            );
        }

        assert!(matches!(
            handler_b.request(
                &executor,
                Pdu::SetClientActiveTab(SetClientActiveTab {
                    window_id: layout.window_id,
                    tab_id: layout.right_tab_id,
                })
            ),
            Pdu::UnitResponse(_)
        ));
        assert!(!mux
            .get_window(layout.window_id)
            .unwrap()
            .is_tab_parked(layout.right_tab_id));
        assert_eq!(
            mux.get_active_tab_for_window_for_client(view_b.as_ref(), layout.window_id)
                .map(|tab| tab.tab_id()),
            Some(layout.right_tab_id)
        );

        let observed = observed.lock().unwrap();
        assert_eq!(observed.len(), 2);
        assert_eq!(observed[0].0, layout.window_id);
        assert_eq!(observed[0].1, tab_ids);
        assert_eq!(observed[0].2, vec![layout.right_tab_id]);
        assert_eq!(observed[0].3.as_ref(), Some(&client_a));
        assert!(observed[1].2.is_empty());
        assert_eq!(observed[1].3.as_ref(), Some(&client_b));
    }

    struct TestLayout {
        window_id: WindowId,
        left_tab_id: TabId,
        left_pane_id: PaneId,
        right_tab_id: TabId,
        right_pane_id: PaneId,
        split_tab_id: TabId,
        split_left_pane_id: PaneId,
        split_right_pane_id: PaneId,
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

    fn build_test_layout(mux: &Arc<Mux>) -> TestLayout {
        let window_id = *mux.new_empty_window(Some("default".to_string()), None);
        let tab_size = size(120, 40);

        let left_tab = Arc::new(Tab::new(&tab_size));
        let left_pane = TestPane::new(alloc_pane_id(), tab_size, "left");
        let left_pane_id = left_pane.pane_id();
        left_tab.assign_pane(&left_pane);
        mux.add_tab_and_active_pane(&left_tab).unwrap();
        mux.add_tab_to_window(&left_tab, window_id).unwrap();

        let right_tab = Arc::new(Tab::new(&tab_size));
        let right_pane = TestPane::new(alloc_pane_id(), tab_size, "right");
        let right_pane_id = right_pane.pane_id();
        right_tab.assign_pane(&right_pane);
        mux.add_tab_and_active_pane(&right_tab).unwrap();
        mux.add_tab_to_window(&right_tab, window_id).unwrap();

        let split_tab = Arc::new(Tab::new(&tab_size));
        let split_left = TestPane::new(alloc_pane_id(), tab_size, "split-left");
        let split_left_pane_id = split_left.pane_id();
        split_tab.assign_pane(&split_left);
        let split_right = TestPane::new(alloc_pane_id(), tab_size, "split-right");
        let split_right_pane_id = split_right.pane_id();
        split_tab
            .split_and_insert(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    target_is_second: true,
                    top_level: false,
                    size: SplitSize::Percent(50),
                },
                split_right,
            )
            .unwrap();
        mux.add_tab_and_active_pane(&split_tab).unwrap();
        mux.add_tab_to_window(&split_tab, window_id).unwrap();

        TestLayout {
            window_id,
            left_tab_id: left_tab.tab_id(),
            left_pane_id,
            right_tab_id: right_tab.tab_id(),
            right_pane_id,
            split_tab_id: split_tab.tab_id(),
            split_left_pane_id,
            split_right_pane_id,
        }
    }

    fn register_test_client(
        mux: &Arc<Mux>,
        view_name: &str,
    ) -> (Arc<ClientId>, Arc<ClientViewId>, HandlerHarness) {
        let client_id = Arc::new(ClientId::new());
        let view_id = Arc::new(ClientViewId(view_name.to_string()));
        mux.register_client(client_id.clone(), view_id.clone());
        let harness = HandlerHarness::new(client_id.clone());
        (client_id, view_id, harness)
    }

    fn build_focus_regression_layout(mux: &Arc<Mux>) -> (WindowId, TabId, PaneId, PaneId) {
        let window_id = *mux.new_empty_window(Some("default".to_string()), None);
        let tab_size = size(120, 40);

        let tab = Arc::new(Tab::new(&tab_size));
        let left = TestPane::new(alloc_pane_id(), tab_size, "left");
        let left_pane_id = left.pane_id();
        tab.assign_pane(&left);
        let right = TestPane::new_for_focus_regression(alloc_pane_id(), tab_size, "right");
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

        (window_id, tab.tab_id(), left_pane_id, right_pane_id)
    }

    #[test]
    fn set_client_id_waits_for_client_registration_before_replying() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let executor = SimpleExecutor::new();
        let mux = test_mux();
        Mux::set_mux(&mux);
        let _guard = MuxGuard;

        let _layout = build_test_layout(&mux);
        let client_id = Arc::new(ClientId::new());
        let view_id = Arc::new(ClientViewId("view-a".to_string()));
        let mut harness = HandlerHarness::new_unregistered();

        harness.handler.process_one(DecodedPdu {
            pdu: Pdu::SetClientId(SetClientId {
                client_id: client_id.as_ref().clone(),
                view_id: view_id.as_ref().clone(),
                is_proxy: false,
                client_version_string: None,
            }),
            serial: 1,
        });

        assert!(
            harness.responses.try_recv().is_err(),
            "SetClientId should not reply before register_client runs"
        );
        assert!(
            mux.iter_clients()
                .into_iter()
                .all(|info| info.client_id.as_ref() != client_id.as_ref()),
            "client should not be visible until the main-thread registration task runs"
        );

        let mut saw_registered = false;
        let response = loop {
            if mux
                .iter_clients()
                .into_iter()
                .any(|info| info.client_id.as_ref() == client_id.as_ref())
            {
                saw_registered = true;
            }
            if let Ok(decoded) = harness.responses.try_recv() {
                break decoded;
            }
            executor.tick().unwrap();
        };

        assert!(
            saw_registered
                || mux
                    .iter_clients()
                    .into_iter()
                    .any(|info| info.client_id.as_ref() == client_id.as_ref()),
            "client should be registered before SetClientId replies"
        );

        match response {
            DecodedPdu {
                pdu: Pdu::UnitResponse(_),
                ..
            } => {}
            other => panic!("expected UnitResponse after registration, got {:?}", other),
        }
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
            codex_app_server: None,
            adopted_pid: None,
            adopted_start_time: None,
        }
    }

    #[test]
    fn render_scheduler_coalesces_until_prior_batch_is_written() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let executor = SimpleExecutor::new();
        let mux = test_mux();
        Mux::set_mux(&mux);
        let _guard = MuxGuard;

        let layout = build_test_layout(&mux);
        let mut harness = HandlerHarness::new_unregistered();
        harness.handler.schedule_pane_push(layout.left_pane_id);
        while harness.render_batches.is_empty() {
            executor.tick().unwrap();
        }
        assert_eq!(harness.render_batches.len(), 1);

        let per_pane = harness.handler.per_pane(layout.left_pane_id);
        for _ in 0..10_000 {
            harness.handler.render_scheduler.schedule(
                layout.left_pane_id,
                Arc::clone(&per_pane),
                Some(InputSerial::now()),
            );
        }
        {
            let state = harness.handler.render_scheduler.state.lock().unwrap();
            assert!(state.active);
            assert_eq!(state.dirty.len(), 1);
            assert_eq!(state.dirty_ids.len(), 1);
        }
        assert_eq!(harness.render_batches.len(), 1);

        let first = harness.render_batches.try_recv().unwrap();
        first.complete();
        while harness.render_batches.is_empty() {
            executor.tick().unwrap();
        }
        assert_eq!(harness.render_batches.len(), 1);

        let second = harness.render_batches.try_recv().unwrap();
        assert!(second.pdus.iter().any(|decoded| matches!(
            &decoded.pdu,
            Pdu::GetPaneRenderChangesResponse(response) if response.input_serial.is_some()
        )));
        second.complete();
        let state = harness.handler.render_scheduler.state.lock().unwrap();
        assert!(!state.active);
        assert!(state.dirty.is_empty());
    }

    #[test]
    fn pane_alert_backlog_keeps_latest_state_and_caps_events() {
        let mut pane = PerPane::default();
        for percent in 0..100 {
            pane.push_notification(Alert::Progress(
                wakterm_term::terminal::Progress::Percentage(percent),
            ));
        }
        assert_eq!(
            pane.notifications,
            vec![Alert::Progress(
                wakterm_term::terminal::Progress::Percentage(99)
            )]
        );

        for _ in 0..(MAX_PENDING_PANE_ALERTS * 2) {
            pane.push_notification(Alert::Bell);
        }
        assert_eq!(pane.notifications.len(), MAX_PENDING_PANE_ALERTS);
    }

    #[test]
    fn ctrl_c_reaches_the_pty_while_render_output_is_stalled() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let executor = SimpleExecutor::new();
        let mux = test_mux();
        Mux::set_mux(&mux);
        let _guard = MuxGuard;

        let window_id = *mux.new_empty_window(Some("default".to_string()), None);
        let tab_size = size(120, 40);
        let tab = Arc::new(Tab::new(&tab_size));
        let (pane, key_down_count) =
            TestPane::new_for_input_latency(alloc_pane_id(), tab_size, "input-latency");
        let pane_id = pane.pane_id();
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();

        let mut harness = HandlerHarness::new_unregistered();
        harness.handler.schedule_pane_push(pane_id);
        while harness.render_batches.is_empty() {
            executor.tick().unwrap();
        }

        // Keep the first render batch unacknowledged, as if the client socket
        // were stalled, and saturate the pane with redundant output notices.
        let per_pane = harness.handler.per_pane(pane_id);
        for _ in 0..100_000 {
            harness
                .handler
                .render_scheduler
                .schedule(pane_id, Arc::clone(&per_pane), None);
        }

        let started = Instant::now();
        harness.handler.process_one(DecodedPdu {
            pdu: Pdu::SendKeyDown(SendKeyDown {
                pane_id,
                event: termwiz::input::KeyEvent {
                    key: KeyCode::Char('c'),
                    modifiers: KeyModifiers::CTRL,
                },
                input_serial: InputSerial::now(),
            }),
            serial: 77,
        });
        while key_down_count.load(Ordering::SeqCst) == 0 {
            executor.tick().unwrap();
        }

        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "Ctrl+C took {:?} to reach the pane",
            started.elapsed()
        );
        let response = harness.responses.try_recv().unwrap();
        assert_eq!(response.serial, 77);
        assert!(matches!(response.pdu, Pdu::UnitResponse(_)));

        let state = harness.handler.render_scheduler.state.lock().unwrap();
        assert!(state.active);
        assert_eq!(state.dirty.len(), 1);
        assert_eq!(harness.render_batches.len(), 1);
    }

    #[test]
    fn codex_resume_selector_enter_is_not_prompt_submission() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let executor = SimpleExecutor::new();
        let mux = test_mux();
        Mux::set_mux(&mux);
        let _guard = MuxGuard;

        let window_id = *mux.new_empty_window(Some("default".to_string()), None);
        let tab_size = size(120, 40);
        let tab = Arc::new(Tab::new(&tab_size));
        let pane =
            TestPane::new_with_process(alloc_pane_id(), tab_size, "codex", "/usr/local/bin/codex");
        let pane_id = pane.pane_id();
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();
        mux.set_agent_metadata(pane_id, sample_agent_metadata("resume-selector"))
            .unwrap();

        let mut harness = HandlerHarness::new_unregistered();
        assert!(matches!(
            harness.request(
                &executor,
                Pdu::SendKeyDown(SendKeyDown {
                    pane_id,
                    event: termwiz::input::KeyEvent {
                        key: KeyCode::Enter,
                        modifiers: KeyModifiers::NONE,
                    },
                    input_serial: InputSerial::now(),
                })
            ),
            Pdu::UnitResponse(_)
        ));

        let runtime = mux
            .list_agents()
            .into_iter()
            .find(|agent| agent.pane_id == pane_id)
            .expect("agent runtime")
            .runtime;
        assert_eq!(runtime.last_input_at, None);
    }

    #[test]
    fn set_client_active_tab_updates_only_requesting_view() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let executor = SimpleExecutor::new();
        let mux = test_mux();
        Mux::set_mux(&mux);
        let _guard = MuxGuard;

        let layout = build_test_layout(&mux);
        let (_client_a, view_a, mut handler_a) = register_test_client(&mux, "view-a");
        let (_client_b, view_b, _handler_b) = register_test_client(&mux, "view-b");

        mux.set_active_tab_for_client_view(view_a.as_ref(), layout.window_id, layout.left_tab_id)
            .unwrap();
        mux.set_active_tab_for_client_view(view_b.as_ref(), layout.window_id, layout.left_tab_id)
            .unwrap();

        assert!(matches!(
            handler_a.request(
                &executor,
                Pdu::SetClientActiveTab(SetClientActiveTab {
                    window_id: layout.window_id,
                    tab_id: layout.right_tab_id,
                })
            ),
            Pdu::UnitResponse(_)
        ));

        assert_eq!(
            mux.get_active_tab_for_window_for_client(view_a.as_ref(), layout.window_id)
                .map(|tab| tab.tab_id()),
            Some(layout.right_tab_id)
        );
        assert_eq!(
            mux.get_active_tab_for_window_for_client(view_b.as_ref(), layout.window_id)
                .map(|tab| tab.tab_id()),
            Some(layout.left_tab_id)
        );
    }

    #[test]
    fn set_focused_pane_updates_only_requesting_view() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let executor = SimpleExecutor::new();
        let mux = test_mux();
        Mux::set_mux(&mux);
        let _guard = MuxGuard;

        let layout = build_test_layout(&mux);
        let (_client_a, view_a, mut handler_a) = register_test_client(&mux, "view-a");
        let (_client_b, view_b, _handler_b) = register_test_client(&mux, "view-b");

        mux.set_active_tab_for_client_view(view_a.as_ref(), layout.window_id, layout.split_tab_id)
            .unwrap();
        mux.set_active_tab_for_client_view(view_b.as_ref(), layout.window_id, layout.split_tab_id)
            .unwrap();
        mux.set_active_pane_for_client_view(
            view_a.as_ref(),
            layout.window_id,
            layout.split_tab_id,
            layout.split_left_pane_id,
        )
        .unwrap();
        mux.set_active_pane_for_client_view(
            view_b.as_ref(),
            layout.window_id,
            layout.split_tab_id,
            layout.split_left_pane_id,
        )
        .unwrap();

        assert!(matches!(
            handler_a.request(
                &executor,
                Pdu::SetFocusedPane(SetFocusedPane {
                    pane_id: layout.split_right_pane_id,
                })
            ),
            Pdu::UnitResponse(_)
        ));

        assert_eq!(
            mux.get_active_pane_id_for_tab_for_client(
                view_a.as_ref(),
                layout.window_id,
                layout.split_tab_id,
            ),
            Some(layout.split_right_pane_id)
        );
        assert_eq!(
            mux.get_active_pane_id_for_tab_for_client(
                view_b.as_ref(),
                layout.window_id,
                layout.split_tab_id,
            ),
            Some(layout.split_left_pane_id)
        );

        assert!(matches!(
            handler_a.request(&executor, Pdu::GetClientList(GetClientList)),
            Pdu::GetClientListResponse(_)
        ));
    }

    #[test]
    fn set_focused_pane_does_not_synthesize_server_pane_focus() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let executor = SimpleExecutor::new();
        let mux = test_mux();
        Mux::set_mux(&mux);
        let _guard = MuxGuard;

        let (window_id, tab_id, left_pane_id, right_pane_id) = build_focus_regression_layout(&mux);
        let (_client, view_id, mut handler) = register_test_client(&mux, "focus-regression");

        mux.set_active_tab_for_client_view(view_id.as_ref(), window_id, tab_id)
            .unwrap();
        mux.set_active_pane_for_client_view(view_id.as_ref(), window_id, tab_id, left_pane_id)
            .unwrap();

        assert!(matches!(
            handler.request(
                &executor,
                Pdu::SetFocusedPane(SetFocusedPane {
                    pane_id: right_pane_id,
                })
            ),
            Pdu::UnitResponse(_)
        ));
        assert!(matches!(
            handler.request(&executor, Pdu::GetClientList(GetClientList)),
            Pdu::GetClientListResponse(_)
        ));
    }

    #[test]
    fn list_panes_returns_requesting_clients_window_view_state() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let executor = SimpleExecutor::new();
        let mux = test_mux();
        Mux::set_mux(&mux);
        let _guard = MuxGuard;

        let layout = build_test_layout(&mux);
        let (_client_a, view_a, mut handler_a) = register_test_client(&mux, "view-a");
        let (_client_b, view_b, mut handler_b) = register_test_client(&mux, "view-b");

        mux.set_active_tab_for_client_view(view_a.as_ref(), layout.window_id, layout.right_tab_id)
            .unwrap();
        mux.set_active_tab_for_client_view(view_b.as_ref(), layout.window_id, layout.split_tab_id)
            .unwrap();
        mux.set_active_pane_for_client_view(
            view_b.as_ref(),
            layout.window_id,
            layout.split_tab_id,
            layout.split_right_pane_id,
        )
        .unwrap();

        let response_a = match handler_a.request(&executor, Pdu::ListPanes(ListPanes {})) {
            Pdu::ListPanesResponse(response) => response,
            other => panic!("expected ListPanesResponse, got {:?}", other),
        };
        let response_b = match handler_b.request(&executor, Pdu::ListPanes(ListPanes {})) {
            Pdu::ListPanesResponse(response) => response,
            other => panic!("expected ListPanesResponse, got {:?}", other),
        };

        assert!(response_a.agents.is_empty());
        assert!(response_a.tab_rss_bytes.is_empty());
        let status = match handler_a.request(&executor, Pdu::GetPaneStatus(GetPaneStatus {})) {
            Pdu::GetPaneStatusResponse(response) => response,
            other => panic!("expected GetPaneStatusResponse, got {:?}", other),
        };
        assert!(status.sampled_at_ms > 0);

        let state_a = response_a
            .client_window_view_state
            .get(&layout.window_id)
            .expect("client A to have view state");
        assert_eq!(state_a.active_tab_id, Some(layout.right_tab_id));
        assert_eq!(state_a.last_active_tab_id, Some(layout.left_tab_id));
        assert_eq!(
            state_a.tabs.get(&layout.left_tab_id),
            Some(&ClientTabViewState {
                active_pane_id: Some(layout.left_pane_id),
            })
        );
        assert_eq!(
            state_a.tabs.get(&layout.right_tab_id),
            Some(&ClientTabViewState {
                active_pane_id: Some(layout.right_pane_id),
            })
        );
        assert_eq!(
            state_a.tabs.get(&layout.split_tab_id),
            Some(&ClientTabViewState {
                active_pane_id: Some(layout.split_right_pane_id),
            })
        );

        let state_b = response_b
            .client_window_view_state
            .get(&layout.window_id)
            .expect("client B to have view state");
        assert_eq!(state_b.active_tab_id, Some(layout.split_tab_id));
        assert_eq!(state_b.last_active_tab_id, Some(layout.left_tab_id));
        assert_eq!(
            state_b.tabs.get(&layout.left_tab_id),
            Some(&ClientTabViewState {
                active_pane_id: Some(layout.left_pane_id),
            })
        );
        assert_eq!(
            state_b.tabs.get(&layout.right_tab_id),
            Some(&ClientTabViewState {
                active_pane_id: Some(layout.right_pane_id),
            })
        );
        assert_eq!(
            state_b.tabs.get(&layout.split_tab_id),
            Some(&ClientTabViewState {
                active_pane_id: Some(layout.split_right_pane_id),
            })
        );
    }

    #[test]
    fn list_panes_decorates_titles_for_tabs_waiting_on_user() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let executor = SimpleExecutor::new();
        let mux = test_mux();
        Mux::set_mux(&mux);
        let _guard = MuxGuard;

        let temp = tempfile::tempdir().unwrap();
        let cwd = "/tmp/title-badge";
        let project_dir = temp.path().join(cwd.replace('/', "-"));
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(
            project_dir.join("session.jsonl"),
            concat!(
                "{\"type\":\"user\",\"timestamp\":\"2026-03-17T12:00:00Z\"}\n",
                "{\"type\":\"assistant\",\"timestamp\":\"2026-03-17T12:00:02Z\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"done\"}]}}\n"
            ),
        )
        .unwrap();
        unsafe {
            std::env::set_var("WAKTERM_AGENT_CLAUDE_DIR", temp.path());
        }

        let tab_size = TerminalSize {
            cols: 120,
            rows: 40,
            pixel_width: 960,
            pixel_height: 720,
            dpi: 96,
        };
        let window_id = *mux.new_empty_window(Some("default".to_string()), None);
        let tab = Arc::new(Tab::new(&tab_size));
        let pane = TestPane::new_with_process(alloc_pane_id(), tab_size, "scrape-pane", "claude");
        let pane_id = pane.pane_id();
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();
        tab.set_title("scrape");
        let mut metadata = sample_agent_metadata("scrape");
        metadata.launch_cmd = "claude".to_string();
        metadata.declared_cwd = cwd.to_string();
        mux.set_agent_metadata(pane_id, metadata).unwrap();

        let refresh_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while mux
            .agent_service()
            .list_agents_cached()
            .into_iter()
            .find(|agent| agent.pane_id == pane_id)
            .and_then(|agent| agent.runtime.last_harness_refresh_at)
            .is_none()
        {
            executor.tick().unwrap();
            assert!(
                std::time::Instant::now() < refresh_deadline,
                "timed out waiting for async agent observer refresh"
            );
            std::thread::yield_now();
        }

        let (_client, _view, mut handler) = register_test_client(&mux, "view-a");
        let response = match handler.request(&executor, Pdu::ListPanes(ListPanes {})) {
            Pdu::ListPanesResponse(response) => response,
            other => panic!("expected ListPanesResponse, got {:?}", other),
        };

        unsafe {
            std::env::remove_var("WAKTERM_AGENT_CLAUDE_DIR");
        }

        assert!(response.tab_titles.iter().any(|title| title == "scrape"));
        assert!(response
            .effective_tab_titles
            .iter()
            .any(|title| title == "scrape"));
        assert!(response
            .tab_badges
            .iter()
            .any(|badge| badge.waiting_on_user && badge.needs_attention));
        let status = match handler.request(&executor, Pdu::GetPaneStatus(GetPaneStatus {})) {
            Pdu::GetPaneStatusResponse(response) => response,
            other => panic!("expected GetPaneStatusResponse, got {:?}", other),
        };
        assert!(status
            .agents
            .iter()
            .any(|agent| agent.pane_id == pane_id && agent.needs_attention));

        assert!(matches!(
            handler.request(
                &executor,
                Pdu::AcknowledgeAgentAttention(AcknowledgeAgentAttention { pane_id })
            ),
            Pdu::UnitResponse(_)
        ));
        let response = match handler.request(&executor, Pdu::ListPanes(ListPanes {})) {
            Pdu::ListPanesResponse(response) => response,
            other => panic!("expected ListPanesResponse, got {:?}", other),
        };
        assert!(response
            .tab_badges
            .iter()
            .all(|badge| !badge.needs_attention));
        let status = match handler.request(&executor, Pdu::GetPaneStatus(GetPaneStatus {})) {
            Pdu::GetPaneStatusResponse(response) => response,
            other => panic!("expected GetPaneStatusResponse, got {:?}", other),
        };
        assert!(status.agents.iter().all(|agent| !agent.needs_attention));
    }

    #[test]
    fn list_panes_does_not_detect_agents_synchronously() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let executor = SimpleExecutor::new();
        let mux = test_mux();
        Mux::set_mux(&mux);
        let _guard = MuxGuard;

        let tab_size = TerminalSize {
            cols: 120,
            rows: 40,
            pixel_width: 960,
            pixel_height: 720,
            dpi: 96,
        };
        let window_id = *mux.new_empty_window(Some("default".to_string()), None);
        let tab = Arc::new(Tab::new(&tab_size));
        let pane =
            TestPane::new_for_listing_regression(alloc_pane_id(), tab_size, "wakterm", "codex");
        let pane_id = pane.pane_id();
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab).unwrap();
        mux.add_tab_to_window(&tab, window_id).unwrap();

        let (_client, _view, mut handler) = register_test_client(&mux, "view-a");
        let response = match handler.request(&executor, Pdu::ListPanes(ListPanes {})) {
            Pdu::ListPanesResponse(response) => response,
            other => panic!("expected ListPanesResponse, got {:?}", other),
        };

        assert_eq!(response.tabs.len(), 1);
        match &response.tabs[0] {
            mux::tab::PaneNode::Leaf(entry) => assert_eq!(entry.pane_id, pane_id),
            other => panic!("expected leaf pane node, got {:?}", other),
        }
        assert!(response
            .tab_badges
            .iter()
            .all(|badge| !badge.waiting_on_user));
    }

    #[test]
    fn set_client_active_tab_rejects_invalid_targets_cleanly() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let executor = SimpleExecutor::new();
        let mux = test_mux();
        Mux::set_mux(&mux);
        let _guard = MuxGuard;

        let layout = build_test_layout(&mux);
        let (_client_a, view_a, mut handler_a) = register_test_client(&mux, "view-a");
        mux.set_active_tab_for_client_view(view_a.as_ref(), layout.window_id, layout.left_tab_id)
            .unwrap();

        let invalid_window = handler_a.request(
            &executor,
            Pdu::SetClientActiveTab(SetClientActiveTab {
                window_id: layout.window_id + 999,
                tab_id: layout.left_tab_id,
            }),
        );
        let invalid_tab = handler_a.request(
            &executor,
            Pdu::SetClientActiveTab(SetClientActiveTab {
                window_id: layout.window_id,
                tab_id: layout.right_tab_id + 999,
            }),
        );

        match invalid_window {
            Pdu::ErrorResponse(ErrorResponse { reason }) => {
                assert!(reason.contains("window"), "{}", reason);
            }
            other => panic!("expected ErrorResponse, got {:?}", other),
        }
        match invalid_tab {
            Pdu::ErrorResponse(ErrorResponse { reason }) => {
                assert!(reason.contains("tab"), "{}", reason);
            }
            other => panic!("expected ErrorResponse, got {:?}", other),
        }

        assert_eq!(
            mux.get_active_tab_for_window_for_client(view_a.as_ref(), layout.window_id)
                .map(|tab| tab.tab_id()),
            Some(layout.left_tab_id)
        );
    }

    #[test]
    fn get_client_list_reports_bootstrapped_workspace_and_focus() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let executor = SimpleExecutor::new();
        let mux = test_mux();
        Mux::set_mux(&mux);
        let _guard = MuxGuard;

        let layout = build_test_layout(&mux);
        let (client_id, _view_id, mut handler) = register_test_client(&mux, "client-list");

        let response = match handler.request(&executor, Pdu::GetClientList(GetClientList)) {
            Pdu::GetClientListResponse(response) => response,
            other => panic!("expected GetClientListResponse, got {:?}", other),
        };

        let client = response
            .clients
            .into_iter()
            .find(|info| info.client_id.as_ref() == client_id.as_ref())
            .expect("client to be listed");

        assert_eq!(client.active_workspace.as_deref(), Some("default"));
        assert_eq!(client.focused_pane_id, Some(layout.left_pane_id));
    }

    #[test]
    fn set_list_and_clear_agent_metadata_round_trip() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let executor = SimpleExecutor::new();
        let mux = test_mux();
        Mux::set_mux(&mux);
        let _guard = MuxGuard;

        let layout = build_test_layout(&mux);
        let (_client_id, _view_id, mut handler) = register_test_client(&mux, "agents");

        assert!(matches!(
            handler.request(
                &executor,
                Pdu::SetAgentMetadata(SetAgentMetadata {
                    pane_id: layout.left_pane_id,
                    metadata: sample_agent_metadata("alpha"),
                }),
            ),
            Pdu::UnitResponse(_)
        ));

        let listed = match handler.request(&executor, Pdu::ListAgents(ListAgents {})) {
            Pdu::ListAgentsResponse(response) => response,
            other => panic!("expected ListAgentsResponse, got {:?}", other),
        };
        assert_eq!(listed.agents.len(), 1);
        assert_eq!(listed.agents[0].metadata.name, "alpha");
        assert_eq!(listed.agents[0].pane_id, layout.left_pane_id);
        assert_eq!(listed.agents[0].tab_id, layout.left_tab_id);
        assert_eq!(listed.agents[0].window_id, layout.window_id);

        let capabilities = match handler.request(
            &executor,
            Pdu::GetAgentApiCapabilities(GetAgentApiCapabilities {}),
        ) {
            Pdu::GetAgentApiCapabilitiesResponse(response) => response.capabilities,
            other => panic!("expected GetAgentApiCapabilitiesResponse, got {:?}", other),
        };
        assert_eq!(capabilities.schema, mux::agent_admission::AGENT_API_SCHEMA);
        assert!(capabilities
            .capabilities
            .contains(&"prompt_admission.v1".to_string()));

        let catalog =
            match handler.request(&executor, Pdu::ListAgentApiCatalog(ListAgentApiCatalog {})) {
                Pdu::ListAgentApiCatalogResponse(response) => response.catalog,
                other => panic!("expected ListAgentApiCatalogResponse, got {:?}", other),
            };
        assert_eq!(catalog.schema, mux::agent_admission::AGENT_API_SCHEMA);
        assert_eq!(catalog.agents.len(), 1);
        assert_eq!(catalog.agents[0].agent_id, "agent-alpha");
        assert_eq!(catalog.agents[0].pane_id, layout.left_pane_id as u64);
        assert_eq!(catalog.agents[0].incarnation_id, None);

        let events = match handler.request(
            &executor,
            Pdu::ReadAgentEvents(ReadAgentEvents {
                after_sequence: catalog.as_of_event_sequence,
                limit: 100,
            }),
        ) {
            Pdu::ReadAgentEventsResponse(response) => response.page,
            other => panic!("expected ReadAgentEventsResponse, got {:?}", other),
        };
        assert_eq!(events.schema, mux::agent_event::AGENT_EVENT_SCHEMA);
        assert_eq!(events.status, mux::agent_event::AgentEventStatus::Ok);

        let output = match handler.request(
            &executor,
            Pdu::ReadAgentOutput(ReadAgentOutput {
                agent_id: "agent-alpha".to_string(),
                cursor: None,
                limit: 100,
            }),
        ) {
            Pdu::ReadAgentOutputResponse(response) => response.page,
            other => panic!("expected ReadAgentOutputResponse, got {:?}", other),
        };
        assert_eq!(
            output.status,
            mux::agent_service::AgentOutputStatus::ObserverUnavailable
        );
        assert_eq!(output.agent_id, "agent-alpha");

        let panes = match handler.request(&executor, Pdu::ListPanes(ListPanes {})) {
            Pdu::ListPanesResponse(response) => response,
            other => panic!("expected ListPanesResponse, got {:?}", other),
        };
        let mut found = None;
        for tab in panes.tabs {
            let mut cursor = tab.into_tree().cursor();
            loop {
                if let Some(entry) = cursor.leaf_mut() {
                    if entry.pane_id == layout.left_pane_id {
                        found = entry.agent_metadata.clone();
                        break;
                    }
                }
                match cursor.preorder_next() {
                    Ok(next) => cursor = next,
                    Err(_) => break,
                }
            }
        }
        assert_eq!(
            found.as_ref().map(|metadata| metadata.name.as_str()),
            Some("alpha")
        );

        assert!(matches!(
            handler.request(
                &executor,
                Pdu::ClearAgentMetadata(ClearAgentMetadata {
                    pane_id: layout.left_pane_id,
                }),
            ),
            Pdu::UnitResponse(_)
        ));

        let listed = match handler.request(&executor, Pdu::ListAgents(ListAgents {})) {
            Pdu::ListAgentsResponse(response) => response,
            other => panic!("expected ListAgentsResponse, got {:?}", other),
        };
        assert!(listed.agents.is_empty());
    }

    #[test]
    fn agent_admission_from_connection_thread_returns_durable_receipt() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let executor = SimpleExecutor::new();
        let mux = test_mux();
        Mux::set_mux(&mux);
        let _guard = MuxGuard;

        let request = mux::agent_admission::AgentPromptAdmissionRequest {
            request_id: "connection-thread-admission".to_string(),
            agent_id: "agent-alpha".to_string(),
            incarnation_id: "incarnation-alpha".to_string(),
            prompt: "keep this prompt at most once".to_string(),
            paste: true,
            return_final: false,
            timeout_ms: 0,
        };
        let store = mux.agent_service().admission_store();
        assert!(matches!(
            store.claim(&request).unwrap(),
            mux::agent_admission::OneWayAdmissionClaim::New
        ));

        let HandlerHarness {
            mut handler,
            responses,
            render_batches: _,
        } = HandlerHarness::new_unregistered();
        let request_for_handler = request.clone();
        std::thread::spawn(move || {
            handler.process_one(DecodedPdu {
                pdu: Pdu::AdmitAgentPrompt(AdmitAgentPrompt {
                    request: request_for_handler,
                }),
                serial: 1,
            });
        })
        .join()
        .unwrap();

        let response = loop {
            if let Ok(decoded) = responses.try_recv() {
                break decoded.pdu;
            }
            executor.tick().unwrap();
        };
        let receipt = match response {
            Pdu::AdmitAgentPromptResponse(response) => response.receipt,
            other => panic!("expected AdmitAgentPromptResponse, got {:?}", other),
        };
        assert_eq!(receipt.request_id, request.request_id);
        assert_eq!(
            receipt.status,
            mux::agent_admission::AgentAdmissionStatus::Indeterminate
        );
        assert!(!receipt.definitive);

        match store.claim(&request).unwrap() {
            mux::agent_admission::OneWayAdmissionClaim::Existing(stored) => {
                assert_eq!(stored, receipt);
            }
            _ => panic!("expected the durable admission receipt to remain at most once"),
        }
    }

    #[test]
    fn set_agent_metadata_rejects_invalid_pane() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let executor = SimpleExecutor::new();
        let mux = test_mux();
        Mux::set_mux(&mux);
        let _guard = MuxGuard;

        let (_client_id, _view_id, mut handler) = register_test_client(&mux, "agents");

        match handler.request(
            &executor,
            Pdu::SetAgentMetadata(SetAgentMetadata {
                pane_id: 999_999,
                metadata: sample_agent_metadata("alpha"),
            }),
        ) {
            Pdu::ErrorResponse(ErrorResponse { reason }) => {
                assert!(reason.contains("pane"), "{}", reason);
            }
            other => panic!("expected ErrorResponse, got {:?}", other),
        }
    }
}
