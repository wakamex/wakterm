use crate::pane::CloseReason;
use crate::{Mux, MuxNotification, Tab, TabId};
use anyhow::{bail, ensure};
use config::GuiPosition;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

static WIN_ID: ::std::sync::atomic::AtomicUsize = ::std::sync::atomic::AtomicUsize::new(0);
pub type WindowId = usize;

pub struct Window {
    id: WindowId,
    tabs: Vec<Arc<Tab>>,
    workspace: String,
    title: String,
    initial_position: Option<GuiPosition>,
}

impl Window {
    pub fn new(workspace: Option<String>, initial_position: Option<GuiPosition>) -> Self {
        Self {
            id: WIN_ID.fetch_add(1, ::std::sync::atomic::Ordering::Relaxed),
            tabs: vec![],
            title: String::new(),
            workspace: workspace.unwrap_or_else(|| Mux::get().active_workspace()),
            initial_position,
        }
    }

    pub fn get_initial_position(&self) -> &Option<GuiPosition> {
        &self.initial_position
    }

    pub fn get_workspace(&self) -> &str {
        &self.workspace
    }

    pub fn set_title(&mut self, title: &str) {
        if self.title != title {
            self.title = title.to_string();
            Mux::try_get().map(|mux| {
                mux.notify(MuxNotification::WindowTitleChanged {
                    window_id: self.id,
                    title: title.to_string(),
                })
            });
        }
    }

    /// Update the window title from mirrored remote state without
    /// notifying the mux as though it were a local change.
    pub fn set_title_from_remote(&mut self, title: &str) {
        if self.title != title {
            self.title = title.to_string();
        }
    }

    pub fn get_title(&self) -> &str {
        &self.title
    }

    pub fn set_workspace(&mut self, workspace: &str) {
        if workspace == self.workspace {
            return;
        }
        self.workspace = workspace.to_string();
        Mux::get().notify(MuxNotification::WindowWorkspaceChanged(self.id));
    }

    pub fn window_id(&self) -> WindowId {
        self.id
    }

    fn check_that_tab_isnt_already_in_window(&self, tab: &Arc<Tab>) {
        for t in &self.tabs {
            assert_ne!(t.tab_id(), tab.tab_id(), "tab already added to this window");
        }
    }

    fn invalidate(&self) {
        let mux = Mux::get();
        mux.notify(MuxNotification::WindowInvalidated(self.id));
    }

    pub fn insert(&mut self, index: usize, tab: &Arc<Tab>) {
        self.check_that_tab_isnt_already_in_window(tab);
        self.tabs.insert(index, Arc::clone(tab));
        self.invalidate();
    }

    pub fn move_by_idx(&mut self, from: usize, to: usize) -> Arc<Tab> {
        if from == to {
            return Arc::clone(&self.tabs[from]);
        }
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, Arc::clone(&tab));
        self.invalidate();
        tab
    }

    /// Apply a complete tab order without emitting a notification.
    /// The caller is responsible for notifying after releasing the window lock.
    pub fn apply_tab_order(&mut self, tab_ids: &[TabId]) -> anyhow::Result<bool> {
        ensure!(
            tab_ids.len() == self.tabs.len(),
            "tab order for window {} has {} tabs, expected {}",
            self.id,
            tab_ids.len(),
            self.tabs.len()
        );
        let desired = tab_ids.iter().copied().collect::<HashSet<_>>();
        ensure!(
            desired.len() == tab_ids.len(),
            "tab order for window {} contains duplicate tab ids",
            self.id
        );
        let current = self
            .tabs
            .iter()
            .map(|tab| tab.tab_id())
            .collect::<HashSet<_>>();
        ensure!(
            desired == current,
            "tab order for window {} does not match its current tabs",
            self.id
        );
        if self
            .tabs
            .iter()
            .map(|tab| tab.tab_id())
            .eq(tab_ids.iter().copied())
        {
            return Ok(false);
        }

        let by_id = self
            .tabs
            .drain(..)
            .map(|tab| (tab.tab_id(), tab))
            .collect::<HashMap<_, _>>();
        self.tabs = tab_ids
            .iter()
            .map(|tab_id| Arc::clone(by_id.get(tab_id).expect("validated tab id")))
            .collect();
        Ok(true)
    }

    /// Reorder only the listed tabs while preserving the positions of tabs
    /// belonging to other domains. Does not emit a notification.
    pub fn apply_tab_order_subset(&mut self, tab_ids: &[TabId]) -> anyhow::Result<bool> {
        let desired = tab_ids.iter().copied().collect::<HashSet<_>>();
        ensure!(
            desired.len() == tab_ids.len(),
            "tab order subset for window {} contains duplicate tab ids",
            self.id
        );

        let current_subset = self
            .tabs
            .iter()
            .filter_map(|tab| desired.contains(&tab.tab_id()).then_some(tab.tab_id()))
            .collect::<Vec<_>>();
        if current_subset.len() != tab_ids.len() {
            bail!(
                "tab order subset for window {} contains an unknown tab id",
                self.id
            );
        }
        if current_subset == tab_ids {
            return Ok(false);
        }

        let by_id = self
            .tabs
            .iter()
            .filter(|tab| desired.contains(&tab.tab_id()))
            .map(|tab| (tab.tab_id(), Arc::clone(tab)))
            .collect::<HashMap<_, _>>();
        let mut ordered = tab_ids.iter();
        for tab in &mut self.tabs {
            if desired.contains(&tab.tab_id()) {
                let tab_id = ordered.next().expect("validated subset length");
                *tab = Arc::clone(by_id.get(tab_id).expect("validated tab id"));
            }
        }
        Ok(true)
    }

    pub fn push(&mut self, tab: &Arc<Tab>) {
        self.check_that_tab_isnt_already_in_window(tab);
        self.tabs.push(Arc::clone(tab));
        self.invalidate();
    }

    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    pub fn len(&self) -> usize {
        self.tabs.len()
    }

    pub fn get_by_idx(&self, idx: usize) -> Option<&Arc<Tab>> {
        self.tabs.get(idx)
    }

    pub fn can_close_without_prompting(&self) -> bool {
        for tab in &self.tabs {
            if !tab.can_close_without_prompting(CloseReason::Window) {
                return false;
            }
        }
        true
    }

    pub fn idx_by_id(&self, id: TabId) -> Option<usize> {
        for (idx, t) in self.tabs.iter().enumerate() {
            if t.tab_id() == id {
                return Some(idx);
            }
        }
        None
    }

    pub fn remove_by_idx(&mut self, idx: usize) -> Arc<Tab> {
        self.invalidate();
        self.tabs.remove(idx)
    }

    pub fn remove_by_id(&mut self, id: TabId) {
        if let Some(idx) = self.idx_by_id(id) {
            self.tabs.remove(idx);
            self.invalidate();
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &Arc<Tab>> {
        self.tabs.iter()
    }

    pub fn prune_dead_tabs(&mut self, live_tab_ids: &[TabId]) {
        let mut invalidated = false;
        let dead: Vec<TabId> = self
            .tabs
            .iter()
            .filter_map(|tab| {
                if tab.prune_dead_panes() {
                    invalidated = true;
                }
                if tab.is_dead() {
                    Some(tab.tab_id())
                } else {
                    None
                }
            })
            .collect();

        for tab_id in dead {
            log::trace!("Window::prune_dead_tabs: tab_id {} is dead", tab_id);
            self.remove_by_id(tab_id);
            invalidated = true;
        }

        let dead: Vec<TabId> = self
            .tabs
            .iter()
            .filter_map(|tab| {
                if live_tab_ids
                    .iter()
                    .find(|&&id| id == tab.tab_id())
                    .is_none()
                {
                    Some(tab.tab_id())
                } else {
                    None
                }
            })
            .collect();
        for tab_id in dead {
            log::trace!("Window::prune_dead_tabs: (live) tab_id {} is dead", tab_id);
            self.remove_by_id(tab_id);
        }

        if invalidated {
            self.invalidate();
        }
    }
}

#[cfg(test)]
mod test {
    use super::Window;
    use crate::{Mux, Tab};
    use std::sync::Arc;
    use wakterm_term::TerminalSize;

    #[test]
    fn move_by_idx_reorders_tabs_without_duplication() {
        let _test_lock = crate::TEST_MUX_LOCK.lock();
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let _guard = crate::TestMuxGuard;

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };

        let tab_a = Arc::new(Tab::new(&size));
        let tab_b = Arc::new(Tab::new(&size));
        let tab_c = Arc::new(Tab::new(&size));

        let mut window = Window::new(None, None);
        window.push(&tab_a);
        window.push(&tab_b);
        window.push(&tab_c);

        let moved = window.move_by_idx(2, 0);

        assert_eq!(moved.tab_id(), tab_c.tab_id());
        assert_eq!(
            window.iter().map(|tab| tab.tab_id()).collect::<Vec<_>>(),
            vec![tab_c.tab_id(), tab_a.tab_id(), tab_b.tab_id()]
        );
    }

    #[test]
    fn aggregate_tab_order_is_strict_and_subset_order_preserves_other_tabs() {
        let _test_lock = crate::TEST_MUX_LOCK.lock();
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let _guard = crate::TestMuxGuard;

        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        };
        let tab_a = Arc::new(Tab::new(&size));
        let tab_x = Arc::new(Tab::new(&size));
        let tab_b = Arc::new(Tab::new(&size));
        let mut window = Window::new(None, None);
        window.push(&tab_a);
        window.push(&tab_x);
        window.push(&tab_b);

        assert!(window
            .apply_tab_order_subset(&[tab_b.tab_id(), tab_a.tab_id()])
            .unwrap());
        assert_eq!(
            window.iter().map(|tab| tab.tab_id()).collect::<Vec<_>>(),
            vec![tab_b.tab_id(), tab_x.tab_id(), tab_a.tab_id()]
        );
        assert!(window
            .apply_tab_order(&[tab_a.tab_id(), tab_x.tab_id(), tab_b.tab_id()])
            .unwrap());
        assert!(window
            .apply_tab_order(&[tab_a.tab_id(), tab_a.tab_id(), tab_b.tab_id()])
            .is_err());
        assert!(window
            .apply_tab_order(&[tab_a.tab_id(), tab_b.tab_id()])
            .is_err());
        assert_eq!(
            window.iter().map(|tab| tab.tab_id()).collect::<Vec<_>>(),
            vec![tab_a.tab_id(), tab_x.tab_id(), tab_b.tab_id()]
        );
    }
}
