use crate::customglyph::harness_icon_stack_glyph;
use crate::overlay::selector::{matcher_pattern, matcher_score};
use crate::termwindow::TabHarnessIcon;
use chrono::{DateTime, Utc};
use mux::agent::{AgentSnapshot, AgentStatus, AgentTurnState};
use mux::pane::{CachePolicy, PaneId};
use mux::tab::TabId;
use mux::termwiztermtab::TermWizTerminal;
use mux::window::WindowId;
use mux::Mux;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::mpsc::sync_channel;
use std::time::{Duration, Instant};
use termwiz::cell::{unicode_column_width, AttributeChange, CellAttributes};
use termwiz::color::ColorAttribute;
use termwiz::input::{InputEvent, KeyCode, KeyEvent, Modifiers, MouseButtons, MouseEvent};
use termwiz::surface::{Change, Position};
use termwiz::terminal::Terminal;
use termwiz_funcs::{pad_left, pad_right, truncate_right};

const ICON_GUTTER_WIDTH: usize = 3;
const TABLE_PREFIX_WIDTH: usize = ICON_GUTTER_WIDTH + 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NavigatorView {
    All,
    Visible,
    Parked,
}

impl NavigatorView {
    fn next(self) -> Self {
        match self {
            Self::All => Self::Visible,
            Self::Visible => Self::Parked,
            Self::Parked => Self::All,
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::All => Self::Parked,
            Self::Visible => Self::All,
            Self::Parked => Self::Visible,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NavigatorSort {
    TabOrder,
    Response,
}

#[derive(Clone)]
struct TabNavigatorPaneRow {
    pane_id: PaneId,
    active: bool,
    identity: String,
    cwd: String,
    status: String,
    harness_icons: Vec<TabHarnessIcon>,
}

#[derive(Clone)]
pub struct TabNavigatorRow {
    tab_id: TabId,
    title: String,
    parked: bool,
    pane_count: usize,
    cwd: String,
    branch: Option<String>,
    agent_names: Vec<String>,
    harness_icons: Vec<TabHarnessIcon>,
    status: String,
    needs_attention: bool,
    last_response: Option<DateTime<Utc>>,
    rss_bytes: Option<u64>,
    panes: Vec<TabNavigatorPaneRow>,
}

pub struct TabNavigatorArgs {
    window_id: WindowId,
    host_tab_id: TabId,
    active_tab_id: TabId,
    rows: Vec<TabNavigatorRow>,
}

impl TabNavigatorArgs {
    pub fn new(window_id: WindowId, host_tab_id: TabId) -> anyhow::Result<Self> {
        let rows = snapshot_rows(window_id)?;
        Ok(Self {
            window_id,
            host_tab_id,
            active_tab_id: host_tab_id,
            rows,
        })
    }
}

fn format_status(status: &AgentStatus, turn_state: &AgentTurnState) -> String {
    match turn_state {
        AgentTurnState::WaitingOnUser => "waiting".to_string(),
        AgentTurnState::WaitingOnAgent => "busy".to_string(),
        AgentTurnState::Unknown => format!("{status:?}").to_lowercase(),
    }
}

fn format_agents_status<'a>(agents: impl IntoIterator<Item = &'a AgentSnapshot>) -> String {
    let agents = agents.into_iter().collect::<Vec<_>>();
    if agents.is_empty() {
        "terminal".to_string()
    } else if agents.len() == 1 {
        format_status(&agents[0].runtime.status, &agents[0].runtime.turn_state)
    } else if agents
        .iter()
        .any(|agent| matches!(agent.runtime.turn_state, AgentTurnState::WaitingOnAgent))
    {
        "busy".to_string()
    } else if agents
        .iter()
        .any(|agent| matches!(agent.runtime.turn_state, AgentTurnState::WaitingOnUser))
    {
        "waiting".to_string()
    } else {
        "idle".to_string()
    }
}

fn harness_icons_for_agents<'a>(
    agents: impl IntoIterator<Item = &'a AgentSnapshot>,
) -> Vec<TabHarnessIcon> {
    let mut icons = Vec::new();
    let mut seen = HashSet::new();
    for agent in agents {
        if let Some(icon) = TabHarnessIcon::from_agent_harness(agent.runtime.harness.clone()) {
            if seen.insert(icon) {
                icons.push(icon);
            }
        }
    }
    icons
}

fn process_leaf(name: String) -> String {
    Path::new(&name)
        .file_name()
        .and_then(|leaf| leaf.to_str())
        .filter(|leaf| !leaf.is_empty())
        .unwrap_or(&name)
        .to_string()
}

fn snapshot_rows(window_id: WindowId) -> anyhow::Result<Vec<TabNavigatorRow>> {
    let mux = Mux::get();
    let titles = mux.display_tab_titles_for_window(window_id);
    let mut agents = mux.list_agents_cached();
    agents.extend(mux.mirrored_agent_snapshots_for_window(window_id));
    let mut agents_by_tab = HashMap::<TabId, Vec<_>>::new();
    for agent in agents
        .into_iter()
        .filter(|agent| agent.window_id == window_id)
    {
        agents_by_tab.entry(agent.tab_id).or_default().push(agent);
    }
    let window = mux
        .get_window(window_id)
        .ok_or_else(|| anyhow::anyhow!("no such window {window_id}"))?;
    let remote_domain_ids = window
        .iter()
        .flat_map(|tab| tab.iter_panes_ignoring_zoom())
        .map(|pane| pane.pane.domain_id())
        .filter(|domain_id| {
            mux.get_domain(*domain_id).is_some_and(|domain| {
                domain
                    .downcast_ref::<wakterm_client::domain::ClientDomain>()
                    .is_some()
            })
        })
        .collect::<HashSet<_>>();
    let mut rows = Vec::with_capacity(window.len());
    for tab in window.iter() {
        let tab_id = tab.tab_id();
        let active_pane = mux
            .active_view_id()
            .and_then(|view_id| {
                mux.get_active_pane_for_tab_for_client(view_id.as_ref(), window_id, tab_id)
            })
            .or_else(|| tab.get_active_pane());
        let active_pane_id = active_pane.as_ref().map(|pane| pane.pane_id());
        let cwd = active_pane
            .as_ref()
            .and_then(|pane| pane.get_current_working_dir(CachePolicy::AllowStale))
            .map(|url| url.path().to_string())
            .unwrap_or_default();
        let tab_agents = agents_by_tab.remove(&tab_id).unwrap_or_default();
        let icons = harness_icons_for_agents(&tab_agents);
        let status = format_agents_status(&tab_agents);
        let last_response = tab_agents
            .iter()
            .filter_map(|agent| agent.runtime.last_turn_completed_at)
            .max();
        let branch = tab_agents
            .iter()
            .find_map(|agent| agent.metadata.branch.clone());
        let agent_names = tab_agents
            .iter()
            .map(|agent| agent.metadata.name.clone())
            .collect();
        let panes = tab
            .iter_panes_ignoring_zoom()
            .into_iter()
            .map(|positioned| {
                let pane_id = positioned.pane.pane_id();
                let pane_agents = tab_agents
                    .iter()
                    .filter(|agent| agent.pane_id == pane_id)
                    .collect::<Vec<_>>();
                let identity = if pane_agents.is_empty() {
                    positioned
                        .pane
                        .get_foreground_process_name(CachePolicy::AllowStale)
                        .map(process_leaf)
                        .filter(|name| !name.is_empty())
                        .unwrap_or_else(|| positioned.pane.get_title())
                } else {
                    pane_agents
                        .iter()
                        .map(|agent| agent.metadata.name.as_str())
                        .collect::<Vec<_>>()
                        .join("+")
                };
                TabNavigatorPaneRow {
                    pane_id,
                    active: active_pane_id == Some(pane_id),
                    identity,
                    cwd: positioned
                        .pane
                        .get_current_working_dir(CachePolicy::AllowStale)
                        .map(|url| url.path().to_string())
                        .unwrap_or_default(),
                    status: format_agents_status(pane_agents.iter().copied()),
                    harness_icons: harness_icons_for_agents(pane_agents.iter().copied()),
                }
            })
            .collect();
        let badge = mux.tab_badge_state_for_current_identity(tab_id);
        rows.push(TabNavigatorRow {
            tab_id,
            title: titles.get(&tab_id).cloned().unwrap_or_default(),
            parked: window.is_tab_parked(tab_id),
            pane_count: tab.count_panes().unwrap_or_default(),
            cwd,
            branch,
            agent_names,
            harness_icons: icons,
            status,
            needs_attention: badge.needs_attention,
            last_response,
            rss_bytes: mux.approximate_tab_process_rss(tab_id),
            panes,
        });
    }
    drop(window);
    for domain_id in remote_domain_ids {
        promise::spawn::spawn_into_main_thread(async move {
            let Some(mux) = Mux::try_get() else {
                return;
            };
            let Some(domain) = mux.get_domain(domain_id) else {
                return;
            };
            let Some(domain) = domain.downcast_ref::<wakterm_client::domain::ClientDomain>() else {
                return;
            };
            if let Err(err) = domain.refresh_cached_remote_status().await {
                log::debug!("tab navigator could not refresh domain {domain_id}: {err:#}");
            }
        })
        .detach();
    }
    Ok(rows)
}

fn on_main_thread<T, F>(func: F) -> anyhow::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
{
    let (tx, rx) = sync_channel(1);
    promise::spawn::spawn_into_main_thread(async move {
        let _ = tx.send(func());
    })
    .detach();
    rx.recv()
        .map_err(|_| anyhow::anyhow!("main-thread tab operation was cancelled"))?
}

const HEADER_ROWS: usize = 4;
const FOOTER_ROWS: usize = 2;
const COLUMN_GAP: usize = 2;

#[derive(Clone, Copy, Debug)]
struct NavigatorColumns {
    title: usize,
    status: Option<usize>,
    last: Option<usize>,
    cwd: Option<usize>,
    branch: Option<usize>,
    panes: Option<usize>,
    rss: Option<usize>,
}

#[derive(Clone, Copy, Debug)]
struct PaneColumns {
    id: usize,
    identity: usize,
    status: Option<usize>,
    cwd: Option<usize>,
}

fn column_width(value: &str) -> usize {
    unicode_column_width(value, None)
}

fn fitted_cell(value: &str, width: usize, right_aligned: bool) -> String {
    let value = truncate_right(value, width);
    if right_aligned {
        pad_left(value, width)
    } else {
        pad_right(value, width)
    }
}

fn grow_width(current: &mut usize, desired: usize, remaining: &mut usize) {
    let growth = desired.saturating_sub(*current).min(*remaining);
    *current += growth;
    *remaining -= growth;
}

fn fit_column(used: &mut usize, available: usize, minimum: usize) -> Option<usize> {
    if used.saturating_add(COLUMN_GAP + minimum) <= available {
        *used += COLUMN_GAP + minimum;
        Some(minimum)
    } else {
        None
    }
}

fn append_column(line: &mut String, value: &str, width: Option<usize>, right_aligned: bool) {
    if let Some(width) = width {
        line.push_str("  ");
        line.push_str(&fitted_cell(value, width, right_aligned));
    }
}

struct NavigatorState {
    args: TabNavigatorArgs,
    view: NavigatorView,
    sort: NavigatorSort,
    dense: bool,
    query: String,
    filtered: Vec<usize>,
    selected: usize,
    top_row: usize,
    pending_close: Option<TabId>,
    message: Option<String>,
    last_refresh: Instant,
}

impl NavigatorState {
    fn new(args: TabNavigatorArgs) -> Self {
        let mut state = Self {
            args,
            view: NavigatorView::All,
            sort: NavigatorSort::TabOrder,
            dense: true,
            query: String::new(),
            filtered: Vec::new(),
            selected: 0,
            top_row: 0,
            pending_close: None,
            message: None,
            last_refresh: Instant::now(),
        };
        state.rebuild(Some(state.args.active_tab_id));
        state
    }

    fn rebuild(&mut self, preserve_tab_id: Option<TabId>) {
        let pattern = (!self.query.is_empty()).then(|| matcher_pattern(&self.query));
        let mut matches = self
            .args
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| match self.view {
                NavigatorView::Visible => !row.parked,
                NavigatorView::Parked => row.parked,
                NavigatorView::All => true,
            })
            .filter_map(|(idx, row)| {
                let score = match pattern.as_ref() {
                    Some(pattern) => matcher_score(pattern, &row.title)?,
                    None => 0,
                };
                Some((idx, score))
            })
            .collect::<Vec<_>>();
        if pattern.is_some() {
            matches.sort_by(|a, b| b.1.cmp(&a.1));
        } else if self.sort == NavigatorSort::Response {
            matches.sort_by(|(left, _), (right, _)| {
                self.args.rows[*right]
                    .last_response
                    .cmp(&self.args.rows[*left].last_response)
            });
        }
        self.filtered = matches.into_iter().map(|(idx, _)| idx).collect();
        self.selected = preserve_tab_id
            .and_then(|tab_id| {
                self.filtered
                    .iter()
                    .position(|idx| self.args.rows[*idx].tab_id == tab_id)
            })
            .unwrap_or_else(|| self.selected.min(self.filtered.len().saturating_sub(1)));
        self.top_row = self.top_row.min(self.selected);
    }

    fn selected_row(&self) -> Option<&TabNavigatorRow> {
        self.filtered
            .get(self.selected)
            .and_then(|idx| self.args.rows.get(*idx))
    }

    fn refresh(&mut self) {
        let preserve = self.selected_row().map(|row| row.tab_id);
        match on_main_thread({
            let window_id = self.args.window_id;
            move || snapshot_rows(window_id)
        }) {
            Ok(rows) => {
                self.args.rows = rows;
                self.rebuild(preserve);
                self.message = None;
            }
            Err(err) => self.message = Some(format!("Refresh failed: {err:#}")),
        }
        self.last_refresh = Instant::now();
    }

    fn mutate_parked(&mut self) -> bool {
        let Some(row) = self.selected_row().cloned() else {
            return false;
        };
        let window_id = self.args.window_id;
        let tab_id = row.tab_id;
        let was_parked = row.parked;
        match on_main_thread(move || Mux::get().set_tab_parked(window_id, tab_id, !was_parked)) {
            Ok(_) => {
                self.refresh();
                tab_id == self.args.host_tab_id && !was_parked
            }
            Err(err) => {
                self.message = Some(format!("Cannot change parked state: {err:#}"));
                false
            }
        }
    }

    fn activate_selected(&mut self) -> bool {
        let Some(row) = self.selected_row().cloned() else {
            return false;
        };
        let window_id = self.args.window_id;
        let tab_id = row.tab_id;
        match on_main_thread(move || {
            let mux = Mux::get();
            if row.parked {
                mux.set_tab_parked(window_id, tab_id, false)?;
            }
            mux.set_active_tab_for_current_identity(window_id, tab_id)?;
            let pane = mux
                .get_active_pane_for_window_for_current_identity(window_id)
                .ok_or_else(|| anyhow::anyhow!("tab {tab_id} has no active pane"))?;
            if let Some(client_id) = mux.active_identity() {
                mux.set_focused_pane_for_client(client_id.as_ref(), pane.pane_id())?;
            }
            pane.focus_changed(true);
            Ok(())
        }) {
            Ok(()) => true,
            Err(err) => {
                self.message = Some(format!("Cannot activate tab: {err:#}"));
                false
            }
        }
    }

    fn close_selected(&mut self) -> bool {
        let Some(tab_id) = self.pending_close else {
            return false;
        };
        match on_main_thread(move || {
            Mux::get().remove_tab(tab_id);
            Ok(())
        }) {
            Ok(()) => {
                self.pending_close = None;
                self.refresh();
                tab_id == self.args.host_tab_id
            }
            Err(err) => {
                self.pending_close = None;
                self.message = Some(format!("Cannot close tab: {err:#}"));
                false
            }
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.filtered.is_empty() {
            self.selected = 0;
            return;
        }
        self.selected = self
            .selected
            .saturating_add_signed(delta)
            .min(self.filtered.len() - 1);
    }

    fn format_relative_time(at: Option<DateTime<Utc>>) -> String {
        let Some(at) = at else {
            return "-".to_string();
        };
        let seconds = (Utc::now() - at).num_seconds().max(0);
        if seconds < 60 {
            format!("{seconds}s")
        } else if seconds < 3600 {
            format!("{}m", seconds / 60)
        } else if seconds < 86400 {
            format!("{}h", seconds / 3600)
        } else {
            format!("{}d", seconds / 86400)
        }
    }

    fn format_rss(bytes: Option<u64>) -> String {
        match bytes {
            Some(bytes) if bytes >= 1024 * 1024 * 1024 => {
                format!("{:.1} GB RSS", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
            }
            Some(bytes) if bytes >= 1024 * 1024 => {
                format!("{} MB RSS", bytes / (1024 * 1024))
            }
            Some(bytes) if bytes >= 1024 => format!("{} KB RSS", bytes / 1024),
            Some(bytes) => format!("{bytes} B RSS"),
            None => "RSS unavailable".to_string(),
        }
    }

    fn compact_title(row: &TabNavigatorRow) -> String {
        let mut title = row.title.clone();
        if row.parked {
            title.push_str(" [parked]");
        }
        if row.needs_attention {
            title.push_str(" [attention]");
        }
        title
    }

    fn pane_identity(pane: &TabNavigatorPaneRow) -> String {
        let identity = if pane.identity.is_empty() {
            "terminal"
        } else {
            pane.identity.as_str()
        };
        identity.to_string()
    }

    fn icon_gutter(icons: &[TabHarnessIcon]) -> String {
        let mask = icons.iter().fold(0, |mask, icon| {
            mask | match icon {
                TabHarnessIcon::Agy => 16,
                TabHarnessIcon::Claude => 1,
                TabHarnessIcon::Codex => 2,
                TabHarnessIcon::Gemini => 4,
                TabHarnessIcon::OpenCode => 8,
            }
        });
        match harness_icon_stack_glyph(mask) {
            Some(glyph) => format!("{glyph}{}", " ".repeat(ICON_GUTTER_WIDTH - 1)),
            None => " ".repeat(ICON_GUTTER_WIDTH),
        }
    }

    fn columns(&self, width: usize) -> NavigatorColumns {
        let rows = self
            .filtered
            .iter()
            .filter_map(|idx| self.args.rows.get(*idx))
            .collect::<Vec<_>>();
        let mut title_desired = column_width("TAB");
        let mut status_desired = column_width("STATUS");
        let mut last_desired = column_width("LAST");
        let mut cwd_desired = column_width("CWD");
        let mut branch_desired = column_width("BRANCH");
        let mut panes_desired = column_width("PANES");
        let mut rss_desired = column_width("MEMORY");
        for row in rows {
            title_desired = title_desired.max(column_width(&Self::compact_title(row)));
            status_desired = status_desired.max(column_width(&row.status));
            last_desired =
                last_desired.max(column_width(&Self::format_relative_time(row.last_response)));
            cwd_desired = cwd_desired.max(column_width(&row.cwd));
            branch_desired = branch_desired.max(column_width(row.branch.as_deref().unwrap_or("-")));
            panes_desired = panes_desired.max(column_width(&row.pane_count.to_string()));
            let rss = if row.parked {
                Self::format_rss(row.rss_bytes)
            } else {
                "-".to_string()
            };
            rss_desired = rss_desired.max(column_width(&rss));
        }
        title_desired = title_desired.min(40);
        cwd_desired = cwd_desired.min(60);
        branch_desired = branch_desired.min(20);

        let title = 8.min(width.saturating_sub(TABLE_PREFIX_WIDTH).max(1));
        let mut used = TABLE_PREFIX_WIDTH.saturating_add(title);
        let status = fit_column(&mut used, width, status_desired);
        let last = fit_column(&mut used, width, last_desired);
        let cwd = fit_column(&mut used, width, 12.min(cwd_desired.max(1)));
        let branch = fit_column(&mut used, width, 8.min(branch_desired.max(1)));
        let panes = fit_column(&mut used, width, 5.min(panes_desired.max(1)));
        let rss = fit_column(&mut used, width, 6.min(rss_desired.max(1)));

        let mut columns = NavigatorColumns {
            title,
            status,
            last,
            cwd,
            branch,
            panes,
            rss,
        };
        let mut remaining = width.saturating_sub(used);
        grow_width(&mut columns.title, title_desired, &mut remaining);
        if let Some(current) = columns.cwd.as_mut() {
            grow_width(current, cwd_desired, &mut remaining);
        }
        if let Some(current) = columns.branch.as_mut() {
            grow_width(current, branch_desired, &mut remaining);
        }
        if let Some(current) = columns.panes.as_mut() {
            grow_width(current, panes_desired, &mut remaining);
        }
        if let Some(current) = columns.rss.as_mut() {
            grow_width(current, rss_desired, &mut remaining);
        }
        columns
    }

    fn pane_columns(&self, width: usize) -> PaneColumns {
        let panes = self
            .filtered
            .iter()
            .filter_map(|idx| self.args.rows.get(*idx))
            .flat_map(|row| row.panes.iter())
            .collect::<Vec<_>>();
        let mut id_desired = 1usize;
        let mut identity_desired = 8usize;
        let mut status_desired = column_width("STATUS");
        let mut cwd_desired = 12usize;
        for pane in panes {
            id_desired = id_desired.max(column_width(&pane.pane_id.to_string()));
            identity_desired = identity_desired.max(column_width(&Self::pane_identity(pane)));
            status_desired = status_desired.max(column_width(&pane.status));
            cwd_desired = cwd_desired.max(column_width(&pane.cwd));
        }
        identity_desired = identity_desired.min(32);
        cwd_desired = cwd_desired.min(60);

        let id = id_desired;
        let identity = 8.min(
            width
                .saturating_sub(TABLE_PREFIX_WIDTH + id + COLUMN_GAP)
                .max(1),
        );
        let mut used = TABLE_PREFIX_WIDTH.saturating_add(id + COLUMN_GAP + identity);
        let status = if used.saturating_add(COLUMN_GAP + status_desired) <= width {
            used += COLUMN_GAP + status_desired;
            Some(status_desired)
        } else {
            None
        };
        let cwd = if used.saturating_add(COLUMN_GAP + 12) <= width {
            used += COLUMN_GAP + 12;
            Some(12)
        } else {
            None
        };
        let mut columns = PaneColumns {
            id,
            identity,
            status,
            cwd,
        };
        let mut remaining = width.saturating_sub(used);
        grow_width(&mut columns.identity, identity_desired, &mut remaining);
        if let Some(current) = columns.cwd.as_mut() {
            grow_width(current, cwd_desired, &mut remaining);
        }
        columns
    }

    fn format_header(columns: NavigatorColumns) -> String {
        let mut line = format!(
            "{}  {}",
            " ".repeat(ICON_GUTTER_WIDTH),
            fitted_cell("TAB", columns.title, false)
        );
        append_column(&mut line, "STATUS", columns.status, false);
        append_column(&mut line, "LAST", columns.last, true);
        append_column(&mut line, "CWD", columns.cwd, false);
        append_column(&mut line, "BRANCH", columns.branch, false);
        append_column(&mut line, "PANES", columns.panes, true);
        append_column(&mut line, "MEMORY", columns.rss, true);
        line
    }

    fn format_tab_line(row: &TabNavigatorRow, selected: bool, columns: NavigatorColumns) -> String {
        let mut line = format!(
            "{}{} {}",
            Self::icon_gutter(&row.harness_icons),
            if selected { ">" } else { " " },
            fitted_cell(&Self::compact_title(row), columns.title, false)
        );
        append_column(&mut line, &row.status, columns.status, false);
        append_column(
            &mut line,
            &Self::format_relative_time(row.last_response),
            columns.last,
            true,
        );
        append_column(&mut line, &row.cwd, columns.cwd, false);
        append_column(
            &mut line,
            row.branch.as_deref().unwrap_or("-"),
            columns.branch,
            false,
        );
        append_column(&mut line, &row.pane_count.to_string(), columns.panes, true);
        let rss = if row.parked {
            Self::format_rss(row.rss_bytes)
        } else {
            "-".to_string()
        };
        append_column(&mut line, &rss, columns.rss, true);
        line
    }

    fn format_pane_line(pane: &TabNavigatorPaneRow, columns: PaneColumns) -> String {
        let mut line = format!(
            "{}{} {}  {}",
            Self::icon_gutter(&pane.harness_icons),
            if pane.active { "*" } else { " " },
            fitted_cell(&pane.pane_id.to_string(), columns.id, true),
            fitted_cell(&Self::pane_identity(pane), columns.identity, false)
        );
        append_column(&mut line, &pane.status, columns.status, false);
        append_column(&mut line, &pane.cwd, columns.cwd, false);
        line
    }

    fn displayed_height(&self, display_idx: usize) -> usize {
        if self.dense {
            return 1;
        }
        self.filtered
            .get(display_idx)
            .and_then(|idx| self.args.rows.get(*idx))
            .map(|row| 1 + row.panes.len())
            .unwrap_or(1)
    }

    fn display_index_at_body_line(&self, line: usize) -> Option<usize> {
        let mut top = 0usize;
        for display_idx in self.top_row..self.filtered.len() {
            let bottom = top.saturating_add(self.displayed_height(display_idx));
            if line < bottom {
                return Some(display_idx);
            }
            top = bottom;
        }
        None
    }

    fn render(&mut self, term: &mut TermWizTerminal) -> termwiz::Result<()> {
        let size = term.get_screen_size()?;
        let width = size.cols.saturating_sub(2);
        let available = size.rows.saturating_sub(HEADER_ROWS + FOOTER_ROWS);
        if self.selected < self.top_row {
            self.top_row = self.selected;
        }
        while self.top_row < self.selected {
            let occupied = (self.top_row..=self.selected)
                .map(|idx| self.displayed_height(idx))
                .sum::<usize>();
            if occupied <= available.max(1) {
                break;
            }
            self.top_row += 1;
        }

        let view = match self.view {
            NavigatorView::All => "[All] Visible Parked",
            NavigatorView::Visible => "All [Visible] Parked",
            NavigatorView::Parked => "All Visible [Parked]",
        };
        let sort = match self.sort {
            NavigatorSort::TabOrder => "[Tab order] Response",
            NavigatorSort::Response => "Tab order [Response]",
        };
        let columns = self.columns(width);
        let pane_columns = self.pane_columns(width);
        let mut changes = vec![
            Change::ClearScreen(ColorAttribute::Default),
            Change::CursorPosition {
                x: Position::Absolute(0),
                y: Position::Absolute(0),
            },
            Change::Text("Tabs\r\n".to_string()),
            Change::Text(truncate_right(&format!("Search: {}", self.query), width)),
            Change::Text("\r\n".to_string()),
            Change::Text(truncate_right(
                &format!("View: {view}   Sort: {sort}"),
                width,
            )),
            Change::Text("\r\n".to_string()),
            Change::Text(truncate_right(&Self::format_header(columns), width)),
            Change::Text("\r\n".to_string()),
        ];

        let mut body_lines = 0usize;
        for display_idx in self.top_row..self.filtered.len() {
            if body_lines >= available {
                break;
            }
            let row_idx = self.filtered[display_idx];
            let row = &self.args.rows[row_idx];
            let selected = display_idx == self.selected;
            if selected {
                changes.push(AttributeChange::Reverse(true).into());
            }
            changes.push(Change::Text(truncate_right(
                &Self::format_tab_line(row, selected, columns),
                width,
            )));
            changes.push(Change::ClearToEndOfLine(ColorAttribute::Default));
            if selected {
                changes.push(AttributeChange::Reverse(false).into());
            }
            changes.push(Change::AllAttributes(CellAttributes::default()));
            changes.push(Change::Text("\r\n".to_string()));
            body_lines += 1;
            if !self.dense {
                for pane in &row.panes {
                    if body_lines >= available {
                        break;
                    }
                    changes.push(Change::Text(truncate_right(
                        &Self::format_pane_line(pane, pane_columns),
                        width,
                    )));
                    changes.push(Change::ClearToEndOfLine(ColorAttribute::Default));
                    changes.push(Change::Text("\r\n".to_string()));
                    body_lines += 1;
                }
            }
        }

        let message = if let Some(tab_id) = self.pending_close {
            self.args
                .rows
                .iter()
                .find(|row| row.tab_id == tab_id)
                .map(|row| {
                    let agents = if row.agent_names.is_empty() {
                        "0 agents".to_string()
                    } else {
                        format!("agents: {}", row.agent_names.join(", "))
                    };
                    format!(
                        "Close {} permanently? {} pane{}, {}, {}. y/n",
                        row.title,
                        row.pane_count,
                        if row.pane_count == 1 { "" } else { "s" },
                        agents,
                        Self::format_rss(row.rss_bytes),
                    )
                })
                .unwrap_or_else(|| "That tab no longer exists. Press n to cancel.".to_string())
        } else {
            self.message.clone().unwrap_or_default()
        };
        changes.push(Change::CursorPosition {
            x: Position::Absolute(0),
            y: Position::Absolute(size.rows.saturating_sub(2)),
        });
        changes.push(Change::Text(truncate_right(&message, width)));
        changes.push(Change::ClearToEndOfLine(ColorAttribute::Default));
        changes.push(Change::Text("\r\n".to_string()));
        changes.push(Change::Text(truncate_right(
            &format!(
                "enter activate   ctrl+shift+s park   ctrl+x close   left/right view   ctrl+r sort   ctrl+o {}   esc clear/exit",
                if self.dense { "comfortable" } else { "dense" }
            ),
            width,
        )));
        changes.push(Change::ClearToEndOfLine(ColorAttribute::Default));
        term.render(&changes)
    }

    fn run(&mut self, term: &mut TermWizTerminal) -> anyhow::Result<()> {
        self.render(term)?;
        loop {
            let event = term.poll_input(Some(Duration::from_secs(1)))?;
            let mut should_exit = false;
            if let Some(event) = event {
                if self.pending_close.is_some() {
                    match event {
                        InputEvent::Key(KeyEvent {
                            key: KeyCode::Char('y' | 'Y'),
                            ..
                        }) => should_exit = self.close_selected(),
                        InputEvent::Key(KeyEvent {
                            key: KeyCode::Char('n' | 'N') | KeyCode::Escape,
                            ..
                        }) => self.pending_close = None,
                        _ => {}
                    }
                } else {
                    match event {
                        InputEvent::Key(KeyEvent {
                            key: KeyCode::UpArrow,
                            ..
                        }) => self.move_selection(-1),
                        InputEvent::Key(KeyEvent {
                            key: KeyCode::DownArrow,
                            ..
                        }) => self.move_selection(1),
                        InputEvent::Key(KeyEvent {
                            key: KeyCode::LeftArrow,
                            ..
                        }) => {
                            self.view = self.view.previous();
                            self.rebuild(None);
                        }
                        InputEvent::Key(KeyEvent {
                            key: KeyCode::RightArrow,
                            ..
                        }) => {
                            self.view = self.view.next();
                            self.rebuild(None);
                        }
                        InputEvent::Key(KeyEvent {
                            key: KeyCode::Enter,
                            ..
                        }) => should_exit = self.activate_selected(),
                        InputEvent::Key(KeyEvent {
                            key: KeyCode::Tab,
                            modifiers: Modifiers::NONE,
                        }) => {
                            self.view = self.view.next();
                            self.rebuild(None);
                        }
                        InputEvent::Key(KeyEvent {
                            key: KeyCode::Tab,
                            modifiers: Modifiers::SHIFT,
                        }) => {
                            self.view = self.view.previous();
                            self.rebuild(None);
                        }
                        InputEvent::Key(KeyEvent {
                            key: KeyCode::Char('S'),
                            modifiers: Modifiers::CTRL,
                        }) => should_exit = self.mutate_parked(),
                        InputEvent::Key(KeyEvent {
                            key: KeyCode::Char('X'),
                            modifiers: Modifiers::CTRL,
                        }) => {
                            self.pending_close = self.selected_row().map(|row| row.tab_id);
                        }
                        InputEvent::Key(KeyEvent {
                            key: KeyCode::Char('R'),
                            modifiers: Modifiers::CTRL,
                        }) => {
                            self.sort = match self.sort {
                                NavigatorSort::TabOrder => NavigatorSort::Response,
                                NavigatorSort::Response => NavigatorSort::TabOrder,
                            };
                            self.rebuild(self.selected_row().map(|row| row.tab_id));
                        }
                        InputEvent::Key(KeyEvent {
                            key: KeyCode::Char('O'),
                            modifiers: Modifiers::CTRL,
                        }) => self.dense = !self.dense,
                        InputEvent::Key(KeyEvent {
                            key: KeyCode::Backspace,
                            ..
                        }) => {
                            self.query.pop();
                            self.rebuild(None);
                        }
                        InputEvent::Key(KeyEvent {
                            key: KeyCode::Escape,
                            ..
                        }) => {
                            if self.query.is_empty() {
                                should_exit = true;
                            } else {
                                self.query.clear();
                                self.rebuild(None);
                            }
                        }
                        InputEvent::Key(KeyEvent {
                            key: KeyCode::Char(c),
                            modifiers,
                        }) if !modifiers.contains(Modifiers::CTRL)
                            && !modifiers.contains(Modifiers::ALT) =>
                        {
                            self.query.push(c);
                            self.rebuild(None);
                        }
                        InputEvent::Mouse(MouseEvent {
                            y,
                            mouse_buttons: MouseButtons::LEFT,
                            ..
                        }) => {
                            let size = term.get_screen_size()?;
                            let y = y as usize;
                            if y >= HEADER_ROWS && y < size.rows.saturating_sub(FOOTER_ROWS) {
                                if let Some(idx) = self.display_index_at_body_line(y - HEADER_ROWS)
                                {
                                    self.selected = idx;
                                    should_exit = self.activate_selected();
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            if should_exit {
                break;
            }
            if self.last_refresh.elapsed() >= Duration::from_secs(5) {
                self.refresh();
            }
            self.render(term)?;
        }
        Ok(())
    }
}

pub fn tab_navigator(args: TabNavigatorArgs, mut term: TermWizTerminal) -> anyhow::Result<()> {
    term.set_raw_mode()?;
    term.render(&[Change::Title("Tab Navigator".to_string())])?;
    NavigatorState::new(args).run(&mut term)
}

#[cfg(test)]
mod test {
    use super::*;
    use chrono::TimeZone;

    fn row(tab_id: TabId, title: &str, parked: bool) -> TabNavigatorRow {
        TabNavigatorRow {
            tab_id,
            title: title.to_string(),
            parked,
            pane_count: 2,
            cwd: "/code/project".to_string(),
            branch: Some("agent/review".to_string()),
            agent_names: vec!["reviewer".to_string()],
            harness_icons: vec![TabHarnessIcon::Codex],
            status: "waiting".to_string(),
            needs_attention: true,
            last_response: None,
            rss_bytes: Some(128 * 1024 * 1024),
            panes: vec![TabNavigatorPaneRow {
                pane_id: tab_id,
                active: true,
                identity: "reviewer".to_string(),
                cwd: "/code/project".to_string(),
                status: "waiting".to_string(),
                harness_icons: vec![TabHarnessIcon::Codex],
            }],
        }
    }

    fn state(rows: Vec<TabNavigatorRow>) -> NavigatorState {
        NavigatorState::new(TabNavigatorArgs {
            window_id: 1,
            host_tab_id: 10,
            active_tab_id: 10,
            rows,
        })
    }

    #[test]
    fn visibility_navigation_cycles_in_display_order() {
        assert_eq!(NavigatorView::All.next(), NavigatorView::Visible);
        assert_eq!(NavigatorView::Visible.next(), NavigatorView::Parked);
        assert_eq!(NavigatorView::Parked.next(), NavigatorView::All);
        assert_eq!(NavigatorView::All.previous(), NavigatorView::Parked);
        assert_eq!(NavigatorView::Visible.previous(), NavigatorView::All);
        assert_eq!(NavigatorView::Parked.previous(), NavigatorView::Visible);
    }

    #[test]
    fn compact_is_default_and_table_columns_align() {
        let mut short = row(10, "one", false);
        short.harness_icons.clear();
        short.status = "busy".to_string();
        short.cwd = "/a".to_string();
        let mut long = row(20, "a-longer-title", false);
        long.harness_icons = vec![TabHarnessIcon::Claude, TabHarnessIcon::Codex];
        long.needs_attention = false;
        long.cwd = "/code/a-longer-project".to_string();
        let state = state(vec![short, long]);

        assert!(state.dense);
        let columns = state.columns(160);
        let header = NavigatorState::format_header(columns);
        let first = NavigatorState::format_tab_line(&state.args.rows[0], true, columns);
        let second = NavigatorState::format_tab_line(&state.args.rows[1], false, columns);
        let display_column =
            |line: &str, text: &str| column_width(&line[..line.find(text).unwrap()]);
        assert_eq!(
            display_column(&header, "STATUS"),
            display_column(&first, "busy")
        );
        assert_eq!(
            display_column(&header, "STATUS"),
            display_column(&second, "waiting")
        );
        assert_eq!(display_column(&header, "CWD"), display_column(&first, "/a"));
        assert_eq!(
            display_column(&header, "CWD"),
            display_column(&second, "/code/a-longer-project")
        );
        assert_eq!(
            display_column(&first, "one"),
            display_column(&second, "a-longer-title")
        );
        assert_eq!(
            column_width(&NavigatorState::icon_gutter(
                &state.args.rows[1].harness_icons,
            )),
            ICON_GUTTER_WIDTH
        );

        let wide = state.columns(160);
        assert!(wide.cwd.is_some());
        assert!(wide.branch.is_some());
        assert!(wide.panes.is_some());
        assert!(wide.rss.is_some());
        let narrow = state.columns(28);
        assert!(narrow.status.is_some());
        assert!(narrow.last.is_some());
        assert!(narrow.cwd.is_none());
        assert!(narrow.branch.is_none());
    }

    #[test]
    fn expanded_rows_map_every_pane_line_to_its_tab() {
        let mut first = row(10, "first", false);
        first.panes.push(TabNavigatorPaneRow {
            pane_id: 11,
            active: false,
            identity: "zsh".to_string(),
            cwd: "/code/project".to_string(),
            status: "terminal".to_string(),
            harness_icons: vec![],
        });
        let second = row(20, "second", false);
        let mut state = state(vec![first, second]);
        state.dense = false;
        state.args.rows[0].panes[0].harness_icons.clear();

        assert_eq!(state.displayed_height(0), 3);
        assert_eq!(state.displayed_height(1), 2);
        assert_eq!(state.display_index_at_body_line(0), Some(0));
        assert_eq!(state.display_index_at_body_line(2), Some(0));
        assert_eq!(state.display_index_at_body_line(3), Some(1));
        assert_eq!(state.display_index_at_body_line(4), Some(1));

        let columns = state.pane_columns(120);
        let first = NavigatorState::format_pane_line(&state.args.rows[0].panes[0], columns);
        let second = NavigatorState::format_pane_line(&state.args.rows[0].panes[1], columns);
        assert_eq!(first.find("waiting"), second.find("terminal"));
        assert_eq!(first.find("/code/project"), second.find("/code/project"));
    }

    #[test]
    fn search_matches_only_the_tab_title() {
        let mut state = state(vec![row(10, "project-title", false)]);
        state.query = "project-title".to_string();
        state.rebuild(None);
        assert_eq!(state.filtered, vec![0]);

        for query in [
            "/code/project",
            "agent/review",
            "waiting",
            "reviewer",
            "codex",
        ] {
            state.query = query.to_string();
            state.rebuild(None);
            assert!(state.filtered.is_empty(), "query {:?}", query);
        }
    }

    #[test]
    fn view_and_response_sorting_do_not_change_authoritative_row_order() {
        let mut old = row(10, "old", false);
        old.last_response = Some(Utc.with_ymd_and_hms(2026, 8, 20, 10, 0, 0).unwrap());
        let mut new = row(20, "new", true);
        new.last_response = Some(Utc.with_ymd_and_hms(2026, 8, 20, 11, 0, 0).unwrap());
        let mut state = state(vec![old, new]);

        assert_eq!(state.view, NavigatorView::All);
        assert_eq!(state.filtered, vec![0, 1]);
        state.view = NavigatorView::Visible;
        state.rebuild(None);
        assert_eq!(state.filtered, vec![0]);
        state.view = NavigatorView::Parked;
        state.rebuild(None);
        assert_eq!(state.filtered, vec![1]);
        state.view = NavigatorView::All;
        state.sort = NavigatorSort::Response;
        state.rebuild(None);
        assert_eq!(state.filtered, vec![1, 0]);
        assert_eq!(state.args.rows[0].tab_id, 10);
        assert_eq!(state.args.rows[1].tab_id, 20);
    }
}
