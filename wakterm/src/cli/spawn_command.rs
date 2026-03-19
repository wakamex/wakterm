use crate::cli::resolve_relative_cwd;
use clap::{Parser, ValueHint};
use codec::ListPanesResponse;
use config::keyassignment::SpawnTabDomain;
use config::ConfigHandle;
use mux::pane::PaneId;
use mux::window::WindowId;
use portable_pty::cmdbuilder::CommandBuilder;
use std::ffi::OsString;
use std::future::Future;
use wakterm_client::client::Client;

#[derive(Debug, Parser, Clone)]
pub struct SpawnCommand {
    /// Specify the current pane.
    /// The default is to use the current pane based on the
    /// environment variable WAKTERM_PANE.
    /// The pane is used to determine the current domain
    /// and window.
    #[arg(long)]
    pane_id: Option<PaneId>,

    #[arg(long)]
    domain_name: Option<String>,

    /// Specify the window into which to spawn a tab.
    /// If omitted, the window associated with the current
    /// pane is used.
    /// Cannot be used with `--workspace` or `--new-window`.
    #[arg(long, conflicts_with_all=&["workspace", "new_window"])]
    window_id: Option<WindowId>,

    /// Spawn into a new window, rather than a new tab.
    #[arg(long)]
    new_window: bool,

    /// Specify the current working directory for the initially
    /// spawned program
    #[arg(long, value_parser, value_hint=ValueHint::DirPath)]
    cwd: Option<OsString>,

    /// When creating a new window, override the default workspace name
    /// with the provided name.  The default name is "default".
    /// Requires `--new-window`.
    #[arg(long, requires = "new_window")]
    workspace: Option<String>,

    /// Instead of executing your shell, run PROG.
    /// For example: `wakterm cli spawn -- bash -l` will spawn bash
    /// as if it were a login shell.
    #[arg(value_parser, value_hint=ValueHint::CommandWithArguments, num_args=1..)]
    prog: Vec<OsString>,
}

impl SpawnCommand {
    pub async fn run(self, client: Client, config: &ConfigHandle) -> anyhow::Result<()> {
        let spawned = self
            .run_with(
                config,
                |pane_id| client.resolve_pane_id(pane_id),
                || client.list_panes(),
                |spawn| client.spawn_v2(spawn),
            )
            .await?;

        log::debug!("{:?}", spawned);
        println!("{}", spawned.pane_id);
        Ok(())
    }

    async fn run_with<
        ResolvePaneId,
        ResolvePaneIdFut,
        ListPanes,
        ListPanesFut,
        SpawnV2Fn,
        SpawnV2Fut,
    >(
        self,
        config: &ConfigHandle,
        mut resolve_pane_id: ResolvePaneId,
        mut list_panes: ListPanes,
        mut spawn_v2: SpawnV2Fn,
    ) -> anyhow::Result<codec::SpawnResponse>
    where
        ResolvePaneId: FnMut(Option<PaneId>) -> ResolvePaneIdFut,
        ResolvePaneIdFut: Future<Output = anyhow::Result<PaneId>>,
        ListPanes: FnMut() -> ListPanesFut,
        ListPanesFut: Future<Output = anyhow::Result<ListPanesResponse>>,
        SpawnV2Fn: FnMut(codec::SpawnV2) -> SpawnV2Fut,
        SpawnV2Fut: Future<Output = anyhow::Result<codec::SpawnResponse>>,
    {
        let SpawnCommand {
            pane_id: requested_pane_id,
            domain_name,
            window_id: requested_window_id,
            new_window,
            cwd,
            workspace,
            prog,
        } = self;

        let pane_id =
            if requested_window_id.is_none() && (requested_pane_id.is_some() || !new_window) {
                Some(resolve_pane_id(requested_pane_id).await?)
            } else {
                None
            };

        let (resolved_window_id, resolved_size) =
            if pane_id.is_some() || requested_window_id.is_some() {
                let panes = list_panes().await?;
                panes.resolve_spawn_context(pane_id, requested_window_id)
            } else {
                (None, None)
            };

        let window_id = if new_window {
            None
        } else {
            requested_window_id.or(resolved_window_id)
        };

        let workspace = workspace
            .as_deref()
            .unwrap_or(
                config
                    .default_workspace
                    .as_deref()
                    .unwrap_or(mux::DEFAULT_WORKSPACE),
            )
            .to_string();

        let size = resolved_size.unwrap_or_else(|| config.initial_size(0, None));

        spawn_v2(codec::SpawnV2 {
            domain: domain_name.map_or(SpawnTabDomain::DefaultDomain, |name| {
                SpawnTabDomain::DomainName(name)
            }),
            window_id,
            current_pane_id: pane_id,
            command: if prog.is_empty() {
                None
            } else {
                Some(CommandBuilder::from_argv(prog))
            },
            command_dir: resolve_relative_cwd(cwd)?,
            size,
            workspace,
        })
        .await
    }
}

#[cfg(test)]
mod test {
    use super::SpawnCommand;
    use codec::{ListPanesResponse, SpawnResponse};
    use config::ConfigHandle;
    use mux::tab::{PaneEntry, PaneNode, SplitDirection, SplitDirectionAndSize};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;
    use wakterm_term::TerminalSize;

    fn leaf(
        window_id: mux::window::WindowId,
        tab_id: mux::tab::TabId,
        pane_id: mux::pane::PaneId,
        size: TerminalSize,
    ) -> PaneNode {
        PaneNode::Leaf(PaneEntry {
            window_id,
            tab_id,
            pane_id,
            agent_metadata: None,
            title: String::new(),
            size,
            working_dir: None,
            is_active_pane: false,
            is_zoomed_pane: false,
            workspace: String::new(),
            cursor_pos: Default::default(),
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

    fn panes_response(tabs: Vec<PaneNode>) -> ListPanesResponse {
        ListPanesResponse {
            tabs,
            tab_titles: vec![],
            tab_badges: vec![],
            window_titles: HashMap::new(),
            client_window_view_state: HashMap::new(),
        }
    }

    #[test]
    fn run_uses_root_tab_size_from_current_pane_context() {
        let left = TerminalSize {
            rows: 40,
            cols: 50,
            pixel_width: 500,
            pixel_height: 800,
            dpi: 96,
        };
        let right = TerminalSize {
            rows: 40,
            cols: 69,
            pixel_width: 690,
            pixel_height: 800,
            dpi: 96,
        };
        let root = TerminalSize {
            rows: 40,
            cols: 120,
            pixel_width: 1200,
            pixel_height: 800,
            dpi: 96,
        };
        let request = Rc::new(RefCell::new(None));

        let response = smol::block_on(
            SpawnCommand {
                pane_id: None,
                domain_name: None,
                window_id: None,
                new_window: false,
                cwd: None,
                workspace: None,
                prog: vec![],
            }
            .run_with(
                &ConfigHandle::default_config(),
                |pane_id| async move {
                    assert_eq!(pane_id, None);
                    Ok(13)
                },
                || async {
                    Ok(panes_response(vec![split(
                        leaf(7, 11, 13, left),
                        leaf(7, 11, 17, right),
                        SplitDirectionAndSize {
                            direction: SplitDirection::Horizontal,
                            first: left,
                            second: right,
                        },
                    )]))
                },
                {
                    let request = Rc::clone(&request);
                    move |spawn| {
                        let request = Rc::clone(&request);
                        async move {
                            request.borrow_mut().replace(spawn);
                            Ok(SpawnResponse {
                                pane_id: 99,
                                tab_id: 11,
                                window_id: 7,
                                size: root,
                            })
                        }
                    }
                },
            ),
        )
        .unwrap();

        let request = request.borrow();
        let request = request.as_ref().unwrap();

        assert_eq!(response.window_id, 7);
        assert_eq!(request.window_id, Some(7));
        assert_eq!(request.current_pane_id, Some(13));
        assert_eq!(request.size, root);
        assert_ne!(request.size, left);
    }

    #[test]
    fn run_uses_explicit_window_root_size_without_resolving_pane() {
        let root = TerminalSize {
            rows: 48,
            cols: 160,
            pixel_width: 1600,
            pixel_height: 960,
            dpi: 96,
        };
        let resolve_calls = Rc::new(RefCell::new(0usize));
        let request = Rc::new(RefCell::new(None));

        smol::block_on(
            SpawnCommand {
                pane_id: Some(13),
                domain_name: None,
                window_id: Some(9),
                new_window: false,
                cwd: None,
                workspace: None,
                prog: vec![],
            }
            .run_with(
                &ConfigHandle::default_config(),
                {
                    let resolve_calls = Rc::clone(&resolve_calls);
                    move |_| {
                        let resolve_calls = Rc::clone(&resolve_calls);
                        async move {
                            *resolve_calls.borrow_mut() += 1;
                            Ok(13)
                        }
                    }
                },
                || async { Ok(panes_response(vec![leaf(9, 21, 34, root)])) },
                {
                    let request = Rc::clone(&request);
                    move |spawn| {
                        let request = Rc::clone(&request);
                        async move {
                            request.borrow_mut().replace(spawn);
                            Ok(SpawnResponse {
                                pane_id: 100,
                                tab_id: 21,
                                window_id: 9,
                                size: root,
                            })
                        }
                    }
                },
            ),
        )
        .unwrap();

        let request = request.borrow();
        let request = request.as_ref().unwrap();

        assert_eq!(*resolve_calls.borrow(), 0);
        assert_eq!(request.window_id, Some(9));
        assert_eq!(request.current_pane_id, None);
        assert_eq!(request.size, root);
    }

    #[test]
    fn run_falls_back_to_initial_size_without_existing_context() {
        let config = ConfigHandle::default_config();
        let expected = config.initial_size(0, None);
        let request = Rc::new(RefCell::new(None));

        smol::block_on(
            SpawnCommand {
                pane_id: None,
                domain_name: None,
                window_id: None,
                new_window: true,
                cwd: None,
                workspace: None,
                prog: vec![],
            }
            .run_with(
                &config,
                |_| async move { panic!("should not resolve a pane for new-window spawn") },
                || async { panic!("should not query panes for new-window spawn") },
                {
                    let request = Rc::clone(&request);
                    move |spawn| {
                        let request = Rc::clone(&request);
                        async move {
                            request.borrow_mut().replace(spawn);
                            Ok(SpawnResponse {
                                pane_id: 101,
                                tab_id: 0,
                                window_id: 0,
                                size: expected,
                            })
                        }
                    }
                },
            ),
        )
        .unwrap();

        let request = request.borrow();
        let request = request.as_ref().unwrap();

        assert_eq!(request.window_id, None);
        assert_eq!(request.current_pane_id, None);
        assert_eq!(request.size, expected);
    }
}
