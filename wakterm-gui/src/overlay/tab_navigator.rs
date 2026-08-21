use crate::overlay::selector::{matcher_pattern, matcher_score};
use crate::termwindow::TabHarnessIcon;
use chrono::{DateTime, Utc};
use mux::agent::{AgentStatus, AgentTurnState};
use mux::pane::CachePolicy;
use mux::tab::TabId;
use mux::termwiztermtab::TermWizTerminal;
use mux::window::WindowId;
use mux::Mux;
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::sync_channel;
use std::time::{Duration, Instant};
use termwiz::cell::{AttributeChange, CellAttributes};
use termwiz::color::ColorAttribute;
use termwiz::input::{InputEvent, KeyCode, KeyEvent, Modifiers, MouseButtons, MouseEvent};
use termwiz::surface::{Change, Position};
use termwiz::terminal::Terminal;
use termwiz_funcs::truncate_right;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NavigatorView {
    Visible,
    Parked,
    All,
}

impl NavigatorView {
    fn next(self) -> Self {
        match self {
            Self::Visible => Self::Parked,
            Self::Parked => Self::All,
            Self::All => Self::Visible,
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::Visible => Self::All,
            Self::Parked => Self::Visible,
            Self::All => Self::Parked,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NavigatorSort {
    TabOrder,
    Response,
}

#[derive(Clone)]
pub struct TabNavigatorRow {
    tab_id: TabId,
    title: String,
    parked: bool,
    pane_count: usize,
    workspace: String,
    cwd: String,
    branch: Option<String>,
    agent_names: Vec<String>,
    harness_icons: Vec<TabHarnessIcon>,
    status: String,
    needs_attention: bool,
    last_response: Option<DateTime<Utc>>,
    rss_bytes: Option<u64>,
}

impl TabNavigatorRow {
    fn search_text(&self) -> String {
        format!(
            "{} {} {} {} {} {} {}",
            self.title,
            self.workspace,
            self.cwd,
            self.branch.as_deref().unwrap_or_default(),
            self.status,
            self.agent_names.join(" "),
            self.harness_icons
                .iter()
                .map(|icon| icon.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        )
    }
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
    let workspace = window.get_workspace().to_string();
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
        let cwd = active_pane
            .as_ref()
            .and_then(|pane| pane.get_current_working_dir(CachePolicy::AllowStale))
            .map(|url| url.path().to_string())
            .unwrap_or_default();
        let tab_agents = agents_by_tab.remove(&tab_id).unwrap_or_default();
        let mut icons = Vec::new();
        let mut seen_icons = HashSet::new();
        for agent in &tab_agents {
            if let Some(icon) = TabHarnessIcon::from_agent_harness(agent.runtime.harness.clone()) {
                if seen_icons.insert(icon) {
                    icons.push(icon);
                }
            }
        }
        let status = if tab_agents.is_empty() {
            "terminal".to_string()
        } else if tab_agents.len() == 1 {
            format_status(
                &tab_agents[0].runtime.status,
                &tab_agents[0].runtime.turn_state,
            )
        } else if tab_agents
            .iter()
            .any(|agent| matches!(agent.runtime.turn_state, AgentTurnState::WaitingOnAgent))
        {
            "busy".to_string()
        } else if tab_agents
            .iter()
            .any(|agent| matches!(agent.runtime.turn_state, AgentTurnState::WaitingOnUser))
        {
            "waiting".to_string()
        } else {
            "idle".to_string()
        };
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
        let badge = mux.tab_badge_state_for_current_identity(tab_id);
        rows.push(TabNavigatorRow {
            tab_id,
            title: titles.get(&tab_id).cloned().unwrap_or_default(),
            parked: window.is_tab_parked(tab_id),
            pane_count: tab.count_panes().unwrap_or_default(),
            workspace: workspace.clone(),
            cwd,
            branch,
            agent_names,
            harness_icons: icons,
            status,
            needs_attention: badge.needs_attention,
            last_response,
            rss_bytes: mux.approximate_tab_process_rss(tab_id),
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
            if let Err(err) = domain.resync_coalesced().await {
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
            view: NavigatorView::Visible,
            sort: NavigatorSort::TabOrder,
            dense: false,
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
                    Some(pattern) => matcher_score(pattern, &row.search_text())?,
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

    fn render(&mut self, term: &mut TermWizTerminal) -> termwiz::Result<()> {
        let size = term.get_screen_size()?;
        let width = size.cols.saturating_sub(2);
        let row_height = if self.dense { 1 } else { 2 };
        let header_rows = 4usize;
        let footer_rows = 2usize;
        let available = size.rows.saturating_sub(header_rows + footer_rows);
        let visible_rows = (available / row_height).max(1);
        if self.selected < self.top_row {
            self.top_row = self.selected;
        } else if self.selected >= self.top_row + visible_rows {
            self.top_row = self.selected + 1 - visible_rows;
        }

        let view = match self.view {
            NavigatorView::Visible => "[Visible] Parked All",
            NavigatorView::Parked => "Visible [Parked] All",
            NavigatorView::All => "Visible Parked [All]",
        };
        let sort = match self.sort {
            NavigatorSort::TabOrder => "[Tab order] Response",
            NavigatorSort::Response => "Tab order [Response]",
        };
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
            Change::Text("\r\n\r\n".to_string()),
        ];

        for (display_idx, row_idx) in self
            .filtered
            .iter()
            .enumerate()
            .skip(self.top_row)
            .take(visible_rows)
        {
            let row = &self.args.rows[*row_idx];
            let selected = display_idx == self.selected;
            if selected {
                changes.push(AttributeChange::Reverse(true).into());
            }
            let icons = row
                .harness_icons
                .iter()
                .map(|icon| icon.as_glyph())
                .collect::<String>();
            let attention = if row.needs_attention {
                " attention"
            } else {
                ""
            };
            let parked = if row.parked { " parked" } else { "" };
            let first = format!(
                "{} {} {}{}{}  {}",
                if selected { ">" } else { " " },
                icons,
                row.title,
                parked,
                attention,
                row.status
            );
            changes.push(Change::Text(truncate_right(&first, width)));
            changes.push(Change::ClearToEndOfLine(ColorAttribute::Default));
            if selected {
                changes.push(AttributeChange::Reverse(false).into());
            }
            changes.push(Change::AllAttributes(CellAttributes::default()));
            changes.push(Change::Text("\r\n".to_string()));
            if !self.dense {
                let metadata = format!(
                    "    {} pane{}   {}   {}   {}{}",
                    row.pane_count,
                    if row.pane_count == 1 { "" } else { "s" },
                    row.cwd,
                    row.branch.as_deref().unwrap_or("-"),
                    Self::format_relative_time(row.last_response),
                    if row.parked {
                        format!("   {}", Self::format_rss(row.rss_bytes))
                    } else {
                        String::new()
                    }
                );
                changes.push(Change::Text(truncate_right(&metadata, width)));
                changes.push(Change::ClearToEndOfLine(ColorAttribute::Default));
                changes.push(Change::Text("\r\n".to_string()));
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
                "enter activate   ctrl+s park   ctrl+x close   tab view   ctrl+r sort   ctrl+o {}   esc clear/exit",
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
                            let row_height = if self.dense { 1 } else { 2 };
                            let row = y.saturating_sub(4) as usize / row_height;
                            let idx = self.top_row + row;
                            if idx < self.filtered.len() {
                                self.selected = idx;
                                should_exit = self.activate_selected();
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
            workspace: "project-workspace".to_string(),
            cwd: "/code/project".to_string(),
            branch: Some("agent/review".to_string()),
            agent_names: vec!["reviewer".to_string()],
            harness_icons: vec![TabHarnessIcon::Codex],
            status: "waiting".to_string(),
            needs_attention: true,
            last_response: None,
            rss_bytes: Some(128 * 1024 * 1024),
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
    fn search_indexes_every_documented_row_field() {
        let mut state = state(vec![row(10, "project-title", false)]);
        for query in [
            "project-title",
            "project-workspace",
            "/code/project",
            "agent/review",
            "waiting",
            "reviewer",
            "codex",
        ] {
            state.query = query.to_string();
            state.rebuild(None);
            assert_eq!(state.filtered, vec![0], "query {query:?}");
        }
        state.query = "definitely-absent".to_string();
        state.rebuild(None);
        assert!(state.filtered.is_empty());
    }

    #[test]
    fn view_and_response_sorting_do_not_change_authoritative_row_order() {
        let mut old = row(10, "old", false);
        old.last_response = Some(Utc.with_ymd_and_hms(2026, 8, 20, 10, 0, 0).unwrap());
        let mut new = row(20, "new", true);
        new.last_response = Some(Utc.with_ymd_and_hms(2026, 8, 20, 11, 0, 0).unwrap());
        let mut state = state(vec![old, new]);

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
