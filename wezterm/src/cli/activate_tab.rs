use clap::Parser;
use mux::pane::PaneId;
use mux::tab::TabId;
use std::collections::HashMap;
use wezterm_client::client::Client;

#[derive(Debug, Parser, Clone)]
pub struct ActivateTab {
    /// Specify the target tab by its id
    #[arg(long, conflicts_with_all=&["tab_index", "tab_relative", "no_wrap", "pane_id"])]
    tab_id: Option<TabId>,

    /// Specify the target tab by its index within the window
    /// that holds the current pane.
    /// Indices are 0-based, with 0 being the left-most tab.
    /// Negative numbers can be used to reference the right-most
    /// tab, so -1 is the right-most tab, -2 is the penultimate
    /// tab and so on.
    #[arg(long, allow_hyphen_values = true)]
    tab_index: Option<isize>,

    /// Specify the target tab by its relative offset.
    /// -1 selects the tab to the left. -2 two tabs to the left.
    /// 1 is one tab to the right and so on.
    ///
    /// Unless `--no-wrap` is specified, relative moves wrap
    /// around from the left-most to right-most and vice versa.
    #[arg(long, allow_hyphen_values = true)]
    tab_relative: Option<isize>,

    /// When used with tab-relative, prevents wrapping around
    /// and will instead clamp to the left-most when moving left
    /// or right-most when moving right.
    #[arg(long, requires = "tab_relative")]
    no_wrap: bool,

    /// Specify the current pane.
    /// The default is to use the current pane based on the
    /// environment variable WEZTERM_PANE.
    ///
    /// The pane is used to figure out which window
    /// contains appropriate tabs
    #[arg(long)]
    pane_id: Option<PaneId>,
}

impl ActivateTab {
    pub async fn run(&self, client: Client) -> anyhow::Result<()> {
        self.run_with(
            |pane_id| client.resolve_pane_id(pane_id),
            || client.list_panes(),
            |request| client.set_client_active_tab(request),
            |request| client.set_focused_pane_id(request),
        )
        .await
    }

    async fn run_with<
        ResolvePaneId,
        ResolvePaneIdFut,
        ListPanes,
        ListPanesFut,
        SetClientActiveTab,
        SetClientActiveTabFut,
        SetFocusedPane,
        SetFocusedPaneFut,
    >(
        &self,
        resolve_pane_id: ResolvePaneId,
        list_panes: ListPanes,
        set_client_active_tab: SetClientActiveTab,
        set_focused_pane_id: SetFocusedPane,
    ) -> anyhow::Result<()>
    where
        ResolvePaneId: FnOnce(Option<PaneId>) -> ResolvePaneIdFut,
        ResolvePaneIdFut: std::future::Future<Output = anyhow::Result<PaneId>>,
        ListPanes: FnOnce() -> ListPanesFut,
        ListPanesFut: std::future::Future<Output = anyhow::Result<codec::ListPanesResponse>>,
        SetClientActiveTab: FnOnce(codec::SetClientActiveTab) -> SetClientActiveTabFut,
        SetClientActiveTabFut: std::future::Future<Output = anyhow::Result<codec::UnitResponse>>,
        SetFocusedPane: FnOnce(codec::SetFocusedPane) -> SetFocusedPaneFut,
        SetFocusedPaneFut: std::future::Future<Output = anyhow::Result<codec::UnitResponse>>,
    {
        let panes = list_panes().await?;

        let mut pane_id_to_tab_id = HashMap::new();
        let mut tab_id_to_active_pane_id = HashMap::new();
        let mut tabs_by_window = HashMap::new();
        let mut window_by_tab_id = HashMap::new();

        for tabroot in panes.tabs {
            let mut cursor = tabroot.into_tree().cursor();

            loop {
                if let Some(entry) = cursor.leaf_mut() {
                    pane_id_to_tab_id.insert(entry.pane_id, entry.tab_id);
                    if entry.is_active_pane {
                        tab_id_to_active_pane_id.insert(entry.tab_id, entry.pane_id);
                    }
                    window_by_tab_id.insert(entry.tab_id, entry.window_id);
                    let win = tabs_by_window
                        .entry(entry.window_id)
                        .or_insert_with(Vec::new);
                    if win.last().copied() != Some(entry.tab_id) {
                        win.push(entry.tab_id);
                    }
                }
                match cursor.preorder_next() {
                    Ok(c) => cursor = c,
                    Err(_) => break,
                }
            }
        }

        let tab_id = if let Some(tab_id) = self.tab_id {
            tab_id
        } else {
            // Find the current tab from the pane id
            let pane_id = resolve_pane_id(self.pane_id).await?;
            let current_tab_id = pane_id_to_tab_id
                .get(&pane_id)
                .copied()
                .ok_or_else(|| anyhow::anyhow!("unable to resolve current tab"))?;
            let window = window_by_tab_id
                .get(&current_tab_id)
                .copied()
                .ok_or_else(|| anyhow::anyhow!("unable to resolve current window"))?;

            let tabs = tabs_by_window
                .get(&window)
                .ok_or_else(|| anyhow::anyhow!("unable to resolve tabs for current window"))?;
            let max = tabs.len();
            anyhow::ensure!(max > 0, "window has no tabs!?");

            if let Some(tab_index) = self.tab_index {
                // This logic is coupled with TermWindow::activate_tab
                // If you update this, update that!
                let tab_idx = if tab_index < 0 {
                    max.saturating_sub(tab_index.abs() as usize)
                } else {
                    tab_index as usize
                };

                tabs.get(tab_idx)
                    .copied()
                    .ok_or_else(|| anyhow::anyhow!("tab index {tab_index} is invalid"))?
            } else if let Some(delta) = self.tab_relative {
                // This logic is coupled with TermWindow::activate_tab_relative
                // If you update this, update that!
                let wrap = !self.no_wrap;
                let active = tabs
                    .iter()
                    .position(|&tab_id| tab_id == current_tab_id)
                    .ok_or_else(|| anyhow::anyhow!("current tab is not in window!?"))?
                    as isize;

                let tab = active + delta;
                let tab_idx = if wrap {
                    let tab = if tab < 0 { max as isize + tab } else { tab };
                    (tab as usize % max) as isize
                } else {
                    if tab < 0 {
                        0
                    } else if tab >= max as isize {
                        max as isize - 1
                    } else {
                        tab
                    }
                };
                tabs.get(tab_idx as usize)
                    .copied()
                    .ok_or_else(|| anyhow::anyhow!("tab index {tab_idx} is invalid"))?
            } else {
                anyhow::bail!("impossible arguments!");
            }
        };

        let window_id = window_by_tab_id
            .get(&tab_id)
            .copied()
            .ok_or_else(|| anyhow::anyhow!("unable to resolve target window"))?;

        set_client_active_tab(codec::SetClientActiveTab { window_id, tab_id }).await?;

        // Now that we know which tab we want to activate, figure out
        // which pane will be the active pane
        if let Some(target_pane) = tab_id_to_active_pane_id.get(&tab_id).copied() {
            set_focused_pane_id(codec::SetFocusedPane {
                pane_id: target_pane,
            })
            .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use codec::{ListPanesResponse, UnitResponse};
    use mux::client::{ClientTabViewState, ClientWindowViewState};
    use mux::renderable::StableCursorPosition;
    use mux::tab::{PaneEntry, PaneNode, SerdeUrl, SplitDirectionAndSize};
    use std::cell::RefCell;
    use std::rc::Rc;
    use termwiz::surface::{CursorShape, CursorVisibility};
    use wezterm_term::TerminalSize;

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
        window_id: mux::window::WindowId,
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
            title: String::new(),
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

    fn split(left: PaneNode, right: PaneNode, node: SplitDirectionAndSize) -> PaneNode {
        PaneNode::Split {
            left: Box::new(left),
            right: Box::new(right),
            node,
        }
    }

    #[test]
    fn activating_tab_sends_client_active_tab_then_focuses_target_active_pane() {
        let active_tab = Rc::new(RefCell::new(None));
        let focused_pane = Rc::new(RefCell::new(None));

        smol::block_on(
            ActivateTab {
                tab_id: Some(9),
                tab_index: None,
                tab_relative: None,
                no_wrap: false,
                pane_id: None,
            }
            .run_with(
                |_| async move { panic!("explicit --tab-id should not resolve pane ids") },
                || async {
                    Ok(ListPanesResponse {
                        tabs: vec![
                            leaf(3, 7, 21, size(120, 40), true),
                            leaf(3, 9, 42, size(120, 40), true),
                        ],
                        tab_titles: vec!["one".into(), "two".into()],
                        tab_badges: vec![Default::default(), Default::default()],
                        window_titles: HashMap::from([(3, "win".into())]),
                        client_window_view_state: HashMap::from([(
                            3,
                            ClientWindowViewState {
                                active_tab_id: Some(7),
                                last_active_tab_id: None,
                                tabs: HashMap::from([(
                                    9,
                                    ClientTabViewState {
                                        active_pane_id: Some(42),
                                    },
                                )]),
                            },
                        )]),
                    })
                },
                {
                    let active_tab = Rc::clone(&active_tab);
                    move |request| {
                        let active_tab = Rc::clone(&active_tab);
                        async move {
                            active_tab.borrow_mut().replace(request);
                            Ok(UnitResponse {})
                        }
                    }
                },
                {
                    let focused_pane = Rc::clone(&focused_pane);
                    move |request| {
                        let focused_pane = Rc::clone(&focused_pane);
                        async move {
                            focused_pane.borrow_mut().replace(request);
                            Ok(UnitResponse {})
                        }
                    }
                },
            ),
        )
        .unwrap();

        assert_eq!(
            active_tab.borrow().as_ref(),
            Some(&codec::SetClientActiveTab {
                window_id: 3,
                tab_id: 9,
            })
        );
        assert_eq!(
            focused_pane.borrow().as_ref(),
            Some(&codec::SetFocusedPane { pane_id: 42 })
        );
    }

    #[test]
    fn activating_tab_without_target_active_pane_skips_focus_request() {
        let active_tab = Rc::new(RefCell::new(None));
        let focus_call_count = Rc::new(RefCell::new(0usize));
        let left = leaf(5, 11, 60, size(70, 40), false);
        let right = leaf(5, 11, 61, size(49, 40), false);

        smol::block_on(
            ActivateTab {
                tab_id: Some(11),
                tab_index: None,
                tab_relative: None,
                no_wrap: false,
                pane_id: None,
            }
            .run_with(
                |_| async move { panic!("explicit --tab-id should not resolve pane ids") },
                move || async move {
                    Ok(ListPanesResponse {
                        tabs: vec![split(
                            left,
                            right,
                            SplitDirectionAndSize {
                                direction: mux::tab::SplitDirection::Horizontal,
                                first: size(70, 40),
                                second: size(49, 40),
                            },
                        )],
                        tab_titles: vec!["target".into()],
                        tab_badges: vec![Default::default()],
                        window_titles: HashMap::from([(5, "win".into())]),
                        client_window_view_state: HashMap::from([(
                            5,
                            ClientWindowViewState {
                                active_tab_id: Some(11),
                                last_active_tab_id: None,
                                tabs: HashMap::new(),
                            },
                        )]),
                    })
                },
                {
                    let active_tab = Rc::clone(&active_tab);
                    move |request| {
                        let active_tab = Rc::clone(&active_tab);
                        async move {
                            active_tab.borrow_mut().replace(request);
                            Ok(UnitResponse {})
                        }
                    }
                },
                {
                    let focus_call_count = Rc::clone(&focus_call_count);
                    move |_| {
                        let focus_call_count = Rc::clone(&focus_call_count);
                        async move {
                            *focus_call_count.borrow_mut() += 1;
                            Ok(UnitResponse {})
                        }
                    }
                },
            ),
        )
        .unwrap();

        assert_eq!(
            active_tab.borrow().as_ref(),
            Some(&codec::SetClientActiveTab {
                window_id: 5,
                tab_id: 11,
            })
        );
        assert_eq!(*focus_call_count.borrow(), 0);
    }
}
