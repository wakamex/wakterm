use crate::PKI;
use anyhow::{Context, anyhow};
use codec::*;
use config::TermConfig;
use mux::client::ClientId;
use mux::domain::SplitSource;
use mux::pane::{CachePolicy, Pane, PaneId};
use mux::renderable::{RenderableDimensions, StableCursorPosition};
use mux::tab::{NotifyMux, TabId};
use mux::{Mux, MuxNotification};
use promise::spawn::spawn_into_main_thread;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use termwiz::surface::SequenceNo;
use url::Url;
use wezterm_term::StableRowIndex;
use wezterm_term::terminal::Alert;

#[derive(Clone)]
pub struct PduSender {
    func: Arc<dyn Fn(DecodedPdu) -> anyhow::Result<()> + Send + Sync>,
}

impl PduSender {
    pub fn send(&self, pdu: DecodedPdu) -> anyhow::Result<()> {
        (self.func)(pdu)
    }

    pub fn new<T>(f: T) -> Self
    where
        T: Fn(DecodedPdu) -> anyhow::Result<()> + Send + Sync + 'static,
    {
        Self { func: Arc::new(f) }
    }
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
    config_generation: usize,
    pub(crate) notifications: Vec<Alert>,
}

impl PerPane {
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

fn maybe_push_pane_changes(
    pane: &Arc<dyn Pane>,
    sender: PduSender,
    per_pane: Arc<Mutex<PerPane>>,
) -> anyhow::Result<()> {
    let mut per_pane = per_pane.lock().unwrap();
    if let Some(resp) = per_pane.compute_changes(pane, None) {
        sender.send(DecodedPdu {
            pdu: Pdu::GetPaneRenderChangesResponse(resp),
            serial: 0,
        })?;
    }

    let config = config::configuration();
    if per_pane.config_generation != config.generation() {
        per_pane.config_generation = config.generation();
        // If the config changed, it may have changed colors
        // in the palette that we need to push down, so we
        // synthesize a palette change notification to let
        // the client know
        per_pane.notifications.push(Alert::PaletteChanged);
        per_pane.sent_initial_palette = true;
    }

    if !per_pane.sent_initial_palette {
        per_pane.notifications.push(Alert::PaletteChanged);
        per_pane.sent_initial_palette = true;
    }
    for alert in per_pane.notifications.drain(..) {
        match alert {
            Alert::PaletteChanged => {
                sender.send(DecodedPdu {
                    pdu: Pdu::SetPalette(SetPalette {
                        pane_id: pane.pane_id(),
                        palette: pane.palette(),
                    }),
                    serial: 0,
                })?;
            }
            alert => {
                sender.send(DecodedPdu {
                    pdu: Pdu::NotifyAlert(NotifyAlert {
                        pane_id: pane.pane_id(),
                        alert,
                    }),
                    serial: 0,
                })?;
            }
        }
    }
    Ok(())
}

pub struct SessionHandler {
    to_write_tx: PduSender,
    per_pane: HashMap<TabId, Arc<Mutex<PerPane>>>,
    client_id: Option<Arc<ClientId>>,
    proxy_client_id: Option<ClientId>,
    /// Tracks which tabs this session recently resized, to suppress
    /// self-echo TabResized notifications while forwarding those
    /// from other sessions (multi-client support).
    recent_resizes: HashMap<TabId, std::time::Instant>,
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
    pub fn new(to_write_tx: PduSender) -> Self {
        Self {
            to_write_tx,
            per_pane: HashMap::new(),
            client_id: None,
            proxy_client_id: None,
            recent_resizes: HashMap::new(),
        }
    }

    /// Record that this session just processed a resize for a tab.
    pub fn note_resize_tab(&mut self, tab_id: TabId) {
        self.recent_resizes.insert(tab_id, std::time::Instant::now());
    }

    /// Check if this session recently resized the given tab (within 2 seconds).
    /// Used to suppress self-echo TabResized notifications.
    pub fn recent_resize_tab(&mut self, tab_id: TabId) -> bool {
        if let Some(when) = self.recent_resizes.get(&tab_id) {
            if when.elapsed() < std::time::Duration::from_secs(2) {
                return true;
            }
            self.recent_resizes.remove(&tab_id);
        }
        false
    }

    pub(crate) fn per_pane(&mut self, pane_id: PaneId) -> Arc<Mutex<PerPane>> {
        Arc::clone(
            self.per_pane
                .entry(pane_id)
                .or_insert_with(|| Arc::new(Mutex::new(PerPane::default()))),
        )
    }

    pub fn schedule_pane_push(&mut self, pane_id: PaneId) {
        let sender = self.to_write_tx.clone();
        let per_pane = self.per_pane(pane_id);
        spawn_into_main_thread(async move {
            let mux = Mux::get();
            let pane = mux
                .get_pane(pane_id)
                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
            maybe_push_pane_changes(&pane, sender, per_pane)?;
            Ok::<(), anyhow::Error>(())
        })
        .detach();
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
            sender.send(DecodedPdu { pdu, serial }).ok();
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
                    // on from the `wezterm cli list-clients` information
                    if let Some(proxy_id) = &self.proxy_client_id {
                        client_id.ssh_auth_sock = proxy_id.ssh_auth_sock.clone();
                        // Note that this `via proxy pid` string is coupled
                        // with the logic in mux/src/ssh_agent
                        client_id.hostname =
                            format!("{} (via proxy pid {})", client_id.hostname, proxy_id.pid);
                    }

                    log::info!(
                        "Client connected: {} from {} (pid {})",
                        client_id.hostname,
                        client_id.username,
                        client_id.pid,
                    );
                    let client_id = Arc::new(client_id);
                    let view_id = Arc::new(view_id);
                    self.client_id.replace(client_id.clone());
                    spawn_into_main_thread(async move {
                        let mux = Mux::get();
                        mux.register_client(client_id, view_id);
                    })
                    .detach();
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
                            let _identity = mux.with_identity(client_id);

                            mux.get_pane(pane_id)
                                .ok_or_else(|| anyhow::anyhow!("pane {pane_id} not found"))?;

                            let (_domain_id, window_id, tab_id) = mux
                                .resolve_pane_id(pane_id)
                                .ok_or_else(|| anyhow::anyhow!("pane {pane_id} not found"))?;
                            mux.set_active_pane_for_current_identity(window_id, tab_id, pane_id)?;

                            mux.record_focus_for_current_identity(pane_id);

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
                                agents: mux.list_agents(),
                            }))
                        },
                        send_response,
                    )
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
                            let view_state = mux.client_window_view_state_for_current_identity();
                            let mut tabs = vec![];
                            let mut tab_titles = vec![];
                            let mut window_titles = HashMap::new();
                            for window_id in mux.iter_windows().into_iter() {
                                let window = mux.get_window(window_id).unwrap();
                                window_titles.insert(window_id, window.get_title().to_string());
                                for tab in window.iter() {
                                    let active_pane_id = view_state
                                        .get(&window_id)
                                        .and_then(|window_state| {
                                            window_state
                                                .tabs
                                                .get(&tab.tab_id())
                                                .and_then(|tab_state| tab_state.active_pane_id)
                                        });
                                    let mut tree =
                                        tab.codec_pane_tree_with_active_pane_id(active_pane_id);
                                    mux.annotate_pane_tree_with_agent_metadata(&mut tree);
                                    tabs.push(tree);
                                    tab_titles.push(tab.get_title());
                                }
                            }
                            log::trace!("ListPanes {tabs:#?} {tab_titles:?}");
                            Ok(Pdu::ListPanesResponse(ListPanesResponse {
                                tabs,
                                tab_titles,
                                window_titles,
                                client_window_view_state: view_state,
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
                let sender = self.to_write_tx.clone();
                let per_pane = self.per_pane(pane_id);
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            pane.writer().write_all(&data)?;
                            maybe_push_pane_changes(&pane, sender, per_pane)?;
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
                let sender = self.to_write_tx.clone();
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
                            maybe_push_pane_changes(&pane, sender, per_pane)?;
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    );
                })
                .detach();
            }
            Pdu::SendPaste(SendPaste { pane_id, data }) => {
                let sender = self.to_write_tx.clone();
                let per_pane = self.per_pane(pane_id);
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            pane.send_paste(&data)?;
                            maybe_push_pane_changes(&pane, sender, per_pane)?;
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
                self.note_resize_tab(containing_tab_id);
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            pane.resize(size)?;
                            let tab = mux
                                .get_tab(containing_tab_id)
                                .ok_or_else(|| anyhow!("no such tab {}", containing_tab_id))?;
                            tab.rebuild_splits_sizes_from_contained_panes();
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::ResizeTab(ResizeTab { tab_id, pane_sizes }) => {
                self.note_resize_tab(tab_id);
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            if mux::tab::size_trace_enabled() {
                                let before = mux
                                    .get_tab(tab_id)
                                    .map(|tab| tab.debug_size_snapshot())
                                    .unwrap_or_else(|| format!("tab_id={} missing", tab_id));
                                let summary = pane_sizes
                                    .iter()
                                    .map(|(pane_id, size)| {
                                        format!(
                                            "{}:{}x{} px={}x{}",
                                            pane_id,
                                            size.cols,
                                            size.rows,
                                            size.pixel_width,
                                            size.pixel_height
                                        )
                                    })
                                    .collect::<Vec<_>>()
                                    .join(", ");
                                log::warn!(
                                    "size-trace server.resize_tab.recv tab_id={} pane_sizes=[{}] {}",
                                    tab_id,
                                    summary,
                                    before
                                );
                            }

                            // Apply all pane sizes atomically, then rebuild once
                            let tab = mux
                                .get_tab(tab_id)
                                .ok_or_else(|| anyhow!("no such tab {}", tab_id))?;
                            let tab_panes = tab.iter_panes();
                            if pane_sizes.len() != tab_panes.len() {
                                log::error!(
                                    "size-trace server.resize_tab.pane_count_mismatch tab_id={} batch_panes={} tab_panes={} {}",
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
                                    "size-trace server.resize_tab.unknown_panes tab_id={} pane_ids={:?} {}",
                                    tab_id,
                                    missing_pane_ids,
                                    tab.debug_size_snapshot()
                                );
                            }
                            tab.rebuild_splits_sizes_from_contained_panes();
                            tab.log_runtime_invariant_errors("server.resize_tab");
                            if mux::tab::size_trace_enabled() {
                                log::warn!(
                                    "size-trace server.resize_tab.done tab_id={} {}",
                                    tab_id,
                                    tab.debug_size_snapshot()
                                );
                            }
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
                let sender = self.to_write_tx.clone();
                let per_pane = self.per_pane(pane_id);
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            pane.key_down(event.key, event.modifiers)?;

                            // For a key press, we want to always send back the
                            // cursor position so that the predictive echo doesn't
                            // leave the cursor in the wrong place
                            let mut per_pane = per_pane.lock().unwrap();
                            if let Some(resp) = per_pane.compute_changes(&pane, Some(input_serial))
                            {
                                sender.send(DecodedPdu {
                                    pdu: Pdu::GetPaneRenderChangesResponse(resp),
                                    serial: 0,
                                })?;
                            }
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
                .detach();
            }
            Pdu::SendMouseEvent(SendMouseEvent { pane_id, event }) => {
                let sender = self.to_write_tx.clone();
                let per_pane = self.per_pane(pane_id);
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            pane.mouse_event(event)?;
                            maybe_push_pane_changes(&pane, sender, per_pane)?;
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
                let sender = self.to_write_tx.clone();
                let per_pane = self.per_pane(pane_id);
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let is_alive = match mux.get_pane(pane_id) {
                                Some(pane) => {
                                    maybe_push_pane_changes(&pane, sender, per_pane)?;
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
                    config::wezterm_version(),
                    CODEC_VERSION,
                );
                match std::env::current_exe().context("resolving current_exe") {
                    Err(err) => send_response(Err(err)),
                    Ok(executable_path) => {
                        send_response(Ok(Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                            codec_vers: CODEC_VERSION,
                            version_string: config::wezterm_version().to_owned(),
                            executable_path,
                            config_file_path: std::env::var_os("WEZTERM_CONFIG_FILE")
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
            Pdu::TabTitleChanged(TabTitleChanged { tab_id, title }) => {
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

            Pdu::Invalid { .. } => send_response(Err(anyhow!("invalid PDU {:?}", decoded.pdu))),
            Pdu::Pong { .. }
            | Pdu::ListPanesResponse { .. }
            | Pdu::ListAgentsResponse { .. }
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
            | Pdu::GetImageCellResponse { .. }
            | Pdu::MovePaneToNewTabResponse { .. }
            | Pdu::TabAddedToWindow { .. }
            | Pdu::GetPaneRenderableDimensionsResponse { .. }
            | Pdu::ErrorResponse { .. } => {
                send_response(Err(anyhow!("expected a request, got {:?}", decoded.pdu)))
            }
        }
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
            if mux::tab::size_trace_enabled() {
                log::warn!(
                    "size-trace server.split.tab_size.begin pane_id={} tab_id={} requested_tab_size={:?} {}",
                    split.pane_id,
                    tab_id,
                    tab_size,
                    tab.debug_size_snapshot()
                );
            }
            tab.resize(tab_size);
            if mux::tab::size_trace_enabled() {
                log::warn!(
                    "size-trace server.split.tab_size.end pane_id={} tab_id={} {}",
                    split.pane_id,
                    tab_id,
                    tab.debug_size_snapshot()
                );
            }
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
    use mux::pane::LogicalLine;
    use mux::pane::{alloc_pane_id, CachePolicy, Pane};
    use mux::renderable::RenderableDimensions;
    use mux::tab::{SplitDirection, SplitRequest, SplitSize, Tab};
    use mux::window::WindowId;
    use promise::spawn::SimpleExecutor;
    use rangeset::RangeSet;
    use std::io::Write;
    use std::ops::Range;
    use termwiz::surface::{CursorShape, CursorVisibility, Line, SequenceNo};
    use url::Url;
    use wezterm_term::color::ColorPalette;
    use wezterm_term::{KeyCode, KeyModifiers, MouseEvent, StableRowIndex, TerminalSize};

    struct TestPane {
        id: PaneId,
        size: Mutex<TerminalSize>,
        title: String,
    }

    impl TestPane {
        fn new(id: PaneId, size: TerminalSize, title: &str) -> Arc<dyn Pane> {
            Arc::new(Self {
                id,
                size: Mutex::new(size),
                title: title.to_string(),
            })
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
            None
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
    }

    impl HandlerHarness {
        fn new(client_id: Arc<ClientId>) -> Self {
            let (tx, rx) = smol::channel::unbounded();
            let sender = PduSender::new(move |decoded| {
                tx.try_send(decoded).unwrap();
                Ok(())
            });
            let mut handler = SessionHandler::new(sender);
            handler.client_id = Some(client_id);
            Self { handler, responses: rx }
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
        }
    }

    #[test]
    fn set_client_active_tab_updates_only_requesting_view() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let executor = SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
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
            handler_a.request(&executor, Pdu::SetClientActiveTab(SetClientActiveTab {
                window_id: layout.window_id,
                tab_id: layout.right_tab_id,
            })),
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
        let mux = Arc::new(Mux::new(None));
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
            handler_a.request(&executor, Pdu::SetFocusedPane(SetFocusedPane {
                pane_id: layout.split_right_pane_id,
            })),
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
    }

    #[test]
    fn list_panes_returns_requesting_clients_window_view_state() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let executor = SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
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
    fn set_client_active_tab_rejects_invalid_targets_cleanly() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let executor = SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
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
        let mux = Arc::new(Mux::new(None));
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
        let mux = Arc::new(Mux::new(None));
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
    fn set_agent_metadata_rejects_invalid_pane() {
        let _test_lock = TEST_MUX_LOCK.lock();
        let executor = SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
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
