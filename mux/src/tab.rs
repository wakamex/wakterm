use crate::domain::DomainId;
use crate::pane::*;
use crate::renderable::StableCursorPosition;
use crate::{Mux, MuxNotification, WindowId};
use bintree::PathBranch;
use config::configuration;
use config::keyassignment::PaneDirection;
use parking_lot::Mutex;
use rangeset::intersects_range;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::convert::TryInto;
use std::sync::Arc;
use url::Url;
use wezterm_term::{StableRowIndex, TerminalSize};

pub type Tree = bintree::Tree<Arc<dyn Pane>, SplitDirectionAndSize>;
pub type Cursor = bintree::Cursor<Arc<dyn Pane>, SplitDirectionAndSize>;

static TAB_ID: ::std::sync::atomic::AtomicUsize = ::std::sync::atomic::AtomicUsize::new(0);
pub type TabId = usize;

#[derive(Default)]
struct Recency {
    count: usize,
    by_idx: HashMap<usize, usize>,
}

impl Recency {
    fn tag(&mut self, idx: usize) {
        self.by_idx.insert(idx, self.count);
        self.count += 1;
    }

    fn score(&self, idx: usize) -> usize {
        self.by_idx.get(&idx).copied().unwrap_or(0)
    }
}

struct TabInner {
    id: TabId,
    pane: Option<Tree>,
    size: TerminalSize,
    size_before_zoom: TerminalSize,
    active: usize,
    zoomed: Option<Arc<dyn Pane>>,
    title: String,
    recency: Recency,
}

/// A Tab is a container of Panes
pub struct Tab {
    inner: Mutex<TabInner>,
    tab_id: TabId,
}

#[derive(Clone)]
pub struct PositionedPane {
    /// The topological pane index that can be used to reference this pane
    pub index: usize,
    /// true if this is the active pane at the time the position was computed
    pub is_active: bool,
    /// true if this pane is zoomed
    pub is_zoomed: bool,
    /// The offset from the top left corner of the containing tab to the top
    /// left corner of this pane, in cells.
    pub left: usize,
    /// The offset from the top left corner of the containing tab to the top
    /// left corner of this pane, in cells.
    pub top: usize,
    /// The width of this pane in cells
    pub width: usize,
    pub pixel_width: usize,
    /// The height of this pane in cells
    pub height: usize,
    pub pixel_height: usize,
    /// The pane instance
    pub pane: Arc<dyn Pane>,
}

impl std::fmt::Debug for PositionedPane {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::result::Result<(), std::fmt::Error> {
        fmt.debug_struct("PositionedPane")
            .field("index", &self.index)
            .field("is_active", &self.is_active)
            .field("left", &self.left)
            .field("top", &self.top)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("pane_id", &self.pane.pane_id())
            .finish()
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub enum SplitDirection {
    Horizontal,
    Vertical,
}

/// The size is of the (first, second) child of the split
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub struct SplitDirectionAndSize {
    pub direction: SplitDirection,
    pub first: TerminalSize,
    pub second: TerminalSize,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub enum SplitSize {
    Cells(usize),
    Percent(u8),
}

impl Default for SplitSize {
    fn default() -> Self {
        Self::Percent(50)
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub struct SplitRequest {
    pub direction: SplitDirection,
    /// Whether the newly created item will be in the second part
    /// of the split (right/bottom)
    pub target_is_second: bool,
    /// Split across the top of the tab rather than the active pane
    pub top_level: bool,
    /// The size of the new item
    pub size: SplitSize,
}

impl Default for SplitRequest {
    fn default() -> Self {
        Self {
            direction: SplitDirection::Horizontal,
            target_is_second: true,
            top_level: false,
            size: SplitSize::default(),
        }
    }
}

impl SplitDirectionAndSize {
    fn top_of_second(&self) -> usize {
        match self.direction {
            SplitDirection::Horizontal => 0,
            SplitDirection::Vertical => self.first.rows as usize + 1,
        }
    }

    fn left_of_second(&self) -> usize {
        match self.direction {
            SplitDirection::Horizontal => self.first.cols as usize + 1,
            SplitDirection::Vertical => 0,
        }
    }

    pub fn width(&self) -> usize {
        if self.direction == SplitDirection::Horizontal {
            self.first.cols + self.second.cols + 1
        } else {
            self.first.cols
        }
    }

    pub fn height(&self) -> usize {
        if self.direction == SplitDirection::Vertical {
            self.first.rows + self.second.rows + 1
        } else {
            self.first.rows
        }
    }

    pub fn size(&self) -> TerminalSize {
        let cell_width = self.first.pixel_width / self.first.cols;
        let cell_height = self.first.pixel_height / self.first.rows;

        let rows = self.height();
        let cols = self.width();

        TerminalSize {
            rows,
            cols,
            pixel_height: cell_height * rows,
            pixel_width: cell_width * cols,
            dpi: self.first.dpi,
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct PositionedSplit {
    /// The topological node index that can be used to reference this split
    pub index: usize,
    pub direction: SplitDirection,
    /// The offset from the top left corner of the containing tab to the top
    /// left corner of this split, in cells.
    pub left: usize,
    /// The offset from the top left corner of the containing tab to the top
    /// left corner of this split, in cells.
    pub top: usize,
    /// For Horizontal splits, how tall the split should be, for Vertical
    /// splits how wide it should be
    pub size: usize,
}

fn is_pane(pane: &Arc<dyn Pane>, other: &Option<&Arc<dyn Pane>>) -> bool {
    if let Some(other) = other {
        other.pane_id() == pane.pane_id()
    } else {
        false
    }
}

fn pane_tree(
    tree: &Tree,
    tab_id: TabId,
    window_id: WindowId,
    active: Option<&Arc<dyn Pane>>,
    zoomed: Option<&Arc<dyn Pane>>,
    workspace: &str,
    left_col: usize,
    top_row: usize,
) -> PaneNode {
    match tree {
        Tree::Empty => PaneNode::Empty,
        Tree::Node { left, right, data } => {
            let data = data.unwrap();
            PaneNode::Split {
                left: Box::new(pane_tree(
                    &*left, tab_id, window_id, active, zoomed, workspace, left_col, top_row,
                )),
                right: Box::new(pane_tree(
                    &*right,
                    tab_id,
                    window_id,
                    active,
                    zoomed,
                    workspace,
                    if data.direction == SplitDirection::Vertical {
                        left_col
                    } else {
                        left_col + data.left_of_second()
                    },
                    if data.direction == SplitDirection::Horizontal {
                        top_row
                    } else {
                        top_row + data.top_of_second()
                    },
                )),
                node: data,
            }
        }
        Tree::Leaf(pane) => {
            let dims = pane.get_dimensions();
            let working_dir = pane.get_current_working_dir(CachePolicy::AllowStale);
            let cursor_pos = pane.get_cursor_position();

            PaneNode::Leaf(PaneEntry {
                window_id,
                tab_id,
                pane_id: pane.pane_id(),
                title: pane.get_title(),
                is_active_pane: is_pane(pane, &active),
                is_zoomed_pane: is_pane(pane, &zoomed),
                size: TerminalSize {
                    cols: dims.cols,
                    rows: dims.viewport_rows,
                    pixel_height: dims.pixel_height,
                    pixel_width: dims.pixel_width,
                    dpi: dims.dpi,
                },
                working_dir: working_dir.map(Into::into),
                workspace: workspace.to_string(),
                cursor_pos,
                physical_top: dims.physical_top,
                left_col,
                top_row,
                tty_name: pane.tty_name(),
            })
        }
    }
}

fn build_from_pane_tree<F>(
    tree: bintree::Tree<PaneEntry, SplitDirectionAndSize>,
    active: &mut Option<Arc<dyn Pane>>,
    zoomed: &mut Option<Arc<dyn Pane>>,
    make_pane: &mut F,
) -> Tree
where
    F: FnMut(PaneEntry) -> Arc<dyn Pane>,
{
    match tree {
        bintree::Tree::Empty => Tree::Empty,
        bintree::Tree::Node { left, right, data } => Tree::Node {
            left: Box::new(build_from_pane_tree(*left, active, zoomed, make_pane)),
            right: Box::new(build_from_pane_tree(*right, active, zoomed, make_pane)),
            data,
        },
        bintree::Tree::Leaf(entry) => {
            let is_zoomed_pane = entry.is_zoomed_pane;
            let is_active_pane = entry.is_active_pane;
            let pane = make_pane(entry);
            if is_zoomed_pane {
                zoomed.replace(Arc::clone(&pane));
            }
            if is_active_pane {
                active.replace(Arc::clone(&pane));
            }
            Tree::Leaf(pane)
        }
    }
}

/// Computes the minimum (x, y) size based on the panes in this portion
/// of the tree.
fn compute_min_size(tree: &mut Tree) -> (usize, usize) {
    match tree {
        Tree::Node { data: None, .. } | Tree::Empty => (1, 1),
        Tree::Node {
            left,
            right,
            data: Some(data),
        } => {
            let (left_x, left_y) = compute_min_size(&mut *left);
            let (right_x, right_y) = compute_min_size(&mut *right);
            match data.direction {
                SplitDirection::Vertical => (left_x.max(right_x), left_y + right_y + 1),
                SplitDirection::Horizontal => (left_x + right_x + 1, left_y.max(right_y)),
            }
        }
        Tree::Leaf(_) => (1, 1),
    }
}

fn adjust_x_size(tree: &mut Tree, mut x_adjust: isize, cell_dimensions: &TerminalSize) {
    let (min_x, _) = compute_min_size(tree);
    while x_adjust != 0 {
        match tree {
            Tree::Empty | Tree::Leaf(_) => return,
            Tree::Node { data: None, .. } => return,
            Tree::Node {
                left,
                right,
                data: Some(data),
            } => {
                data.first.dpi = cell_dimensions.dpi;
                data.second.dpi = cell_dimensions.dpi;
                match data.direction {
                    SplitDirection::Vertical => {
                        let new_cols = (data.first.cols as isize)
                            .saturating_add(x_adjust)
                            .max(min_x as isize);
                        x_adjust = new_cols.saturating_sub(data.first.cols as isize);

                        if x_adjust != 0 {
                            adjust_x_size(&mut *left, x_adjust, cell_dimensions);
                            data.first.cols = new_cols.try_into().unwrap();
                            data.first.pixel_width =
                                data.first.cols.saturating_mul(cell_dimensions.pixel_width);

                            adjust_x_size(&mut *right, x_adjust, cell_dimensions);
                            data.second.cols = data.first.cols;
                            data.second.pixel_width = data.first.pixel_width;
                        }
                        return;
                    }
                    SplitDirection::Horizontal if x_adjust > 0 => {
                        adjust_x_size(&mut *left, 1, cell_dimensions);
                        data.first.cols += 1;
                        data.first.pixel_width =
                            data.first.cols.saturating_mul(cell_dimensions.pixel_width);
                        x_adjust -= 1;

                        if x_adjust > 0 {
                            adjust_x_size(&mut *right, 1, cell_dimensions);
                            data.second.cols += 1;
                            data.second.pixel_width =
                                data.second.cols.saturating_mul(cell_dimensions.pixel_width);
                            x_adjust -= 1;
                        }
                    }
                    SplitDirection::Horizontal => {
                        // x_adjust is negative
                        let mut made_progress = false;
                        if data.first.cols > 1 {
                            adjust_x_size(&mut *left, -1, cell_dimensions);
                            data.first.cols -= 1;
                            data.first.pixel_width =
                                data.first.cols.saturating_mul(cell_dimensions.pixel_width);
                            x_adjust += 1;
                            made_progress = true;
                        }
                        if x_adjust < 0 && data.second.cols > 1 {
                            adjust_x_size(&mut *right, -1, cell_dimensions);
                            data.second.cols -= 1;
                            data.second.pixel_width =
                                data.second.cols.saturating_mul(cell_dimensions.pixel_width);
                            x_adjust += 1;
                            made_progress = true;
                        }
                        if !made_progress {
                            return;
                        }
                    }
                }
            }
        }
    }
}

fn adjust_y_size(tree: &mut Tree, mut y_adjust: isize, cell_dimensions: &TerminalSize) {
    let (_, min_y) = compute_min_size(tree);
    while y_adjust != 0 {
        match tree {
            Tree::Empty | Tree::Leaf(_) => return,
            Tree::Node { data: None, .. } => return,
            Tree::Node {
                left,
                right,
                data: Some(data),
            } => {
                data.first.dpi = cell_dimensions.dpi;
                data.second.dpi = cell_dimensions.dpi;
                match data.direction {
                    SplitDirection::Horizontal => {
                        let new_rows = (data.first.rows as isize)
                            .saturating_add(y_adjust)
                            .max(min_y as isize);
                        y_adjust = new_rows.saturating_sub(data.first.rows as isize);

                        if y_adjust != 0 {
                            adjust_y_size(&mut *left, y_adjust, cell_dimensions);
                            data.first.rows = new_rows.try_into().unwrap();
                            data.first.pixel_height =
                                data.first.rows.saturating_mul(cell_dimensions.pixel_height);

                            adjust_y_size(&mut *right, y_adjust, cell_dimensions);
                            data.second.rows = data.first.rows;
                            data.second.pixel_height = data.first.pixel_height;
                        }
                        return;
                    }
                    SplitDirection::Vertical if y_adjust > 0 => {
                        adjust_y_size(&mut *left, 1, cell_dimensions);
                        data.first.rows += 1;
                        data.first.pixel_height =
                            data.first.rows.saturating_mul(cell_dimensions.pixel_height);
                        y_adjust -= 1;
                        if y_adjust > 0 {
                            adjust_y_size(&mut *right, 1, cell_dimensions);
                            data.second.rows += 1;
                            data.second.pixel_height = data
                                .second
                                .rows
                                .saturating_mul(cell_dimensions.pixel_height);
                            y_adjust -= 1;
                        }
                    }
                    SplitDirection::Vertical => {
                        // y_adjust is negative
                        let mut made_progress = false;
                        if data.first.rows > 1 {
                            adjust_y_size(&mut *left, -1, cell_dimensions);
                            data.first.rows -= 1;
                            data.first.pixel_height =
                                data.first.rows.saturating_mul(cell_dimensions.pixel_height);
                            y_adjust += 1;
                            made_progress = true;
                        }
                        if y_adjust < 0 && data.second.rows > 1 {
                            adjust_y_size(&mut *right, -1, cell_dimensions);
                            data.second.rows -= 1;
                            data.second.pixel_height = data
                                .second
                                .rows
                                .saturating_mul(cell_dimensions.pixel_height);
                            y_adjust += 1;
                            made_progress = true;
                        }
                        // If both children are at minimum (1 row), we can't
                        // shrink further — break to avoid an infinite loop.
                        if !made_progress {
                            return;
                        }
                    }
                }
            }
        }
    }
}

/// Collect all leaf pane sizes from the split tree into a vec.
/// Used to build a batched resize PDU.
fn collect_pane_sizes(tree: &Tree, size: &TerminalSize, out: &mut Vec<(PaneId, TerminalSize)>) {
    match tree {
        Tree::Empty => {}
        Tree::Node { data: None, .. } => {}
        Tree::Node {
            left,
            right,
            data: Some(data),
        } => {
            collect_pane_sizes(&*left, &data.first, out);
            collect_pane_sizes(&*right, &data.second, out);
        }
        Tree::Leaf(pane) => {
            out.push((pane.pane_id(), *size));
        }
    }
}

fn apply_sizes_from_splits(tree: &Tree, size: &TerminalSize) {
    match tree {
        Tree::Empty => return,
        Tree::Node { data: None, .. } => return,
        Tree::Node {
            left,
            right,
            data: Some(data),
        } => {
            apply_sizes_from_splits(&*left, &data.first);
            apply_sizes_from_splits(&*right, &data.second);
        }
        Tree::Leaf(pane) => {
            pane.resize(*size).ok();
        }
    }
}

/// Top-down reconciliation pass that enforces parent-child size constraints.
/// Prevents accumulated drift from interleaved per-pane resize PDUs.
fn reconcile_tree_sizes(tree: &mut Tree, allocated: &TerminalSize) {
    match tree {
        Tree::Empty | Tree::Leaf(_) => {}
        Tree::Node { data: None, .. } => {}
        Tree::Node {
            left,
            right,
            data: Some(data),
        } => {
            let cell_width = allocated.pixel_width.checked_div(allocated.cols).unwrap_or(1);
            let cell_height = allocated.pixel_height.checked_div(allocated.rows).unwrap_or(1);

            match data.direction {
                SplitDirection::Horizontal => {
                    data.first.rows = allocated.rows;
                    data.second.rows = allocated.rows;
                    if data.first.cols + 2 > allocated.cols {
                        data.first.cols = allocated.cols.saturating_sub(2);
                    }
                    data.second.cols = allocated.cols.saturating_sub(1 + data.first.cols);

                    data.first.pixel_width = data.first.cols * cell_width;
                    data.first.pixel_height = data.first.rows * cell_height;
                    data.second.pixel_width = data.second.cols * cell_width;
                    data.second.pixel_height = data.second.rows * cell_height;
                    data.first.dpi = allocated.dpi;
                    data.second.dpi = allocated.dpi;
                }
                SplitDirection::Vertical => {
                    data.first.cols = allocated.cols;
                    data.second.cols = allocated.cols;
                    if data.first.rows + 2 > allocated.rows {
                        data.first.rows = allocated.rows.saturating_sub(2);
                    }
                    data.second.rows = allocated.rows.saturating_sub(1 + data.first.rows);

                    data.first.pixel_width = data.first.cols * cell_width;
                    data.first.pixel_height = data.first.rows * cell_height;
                    data.second.pixel_width = data.second.cols * cell_width;
                    data.second.pixel_height = data.second.rows * cell_height;
                    data.first.dpi = allocated.dpi;
                    data.second.dpi = allocated.dpi;
                }
            }

            reconcile_tree_sizes(left, &data.first);
            reconcile_tree_sizes(right, &data.second);
        }
    }
}

#[cfg(debug_assertions)]
fn debug_assert_tree_invariants(tree: &Tree, size: &TerminalSize) {
    fn check(tree: &Tree, allocated: &TerminalSize, errors: &mut Vec<String>) {
        match tree {
            Tree::Empty | Tree::Leaf(_) => {}
            Tree::Node { data: None, .. } => {}
            Tree::Node { left, right, data: Some(data) } => {
                match data.direction {
                    SplitDirection::Horizontal => {
                        if data.first.rows != allocated.rows {
                            errors.push(format!("H first.rows={} != {}", data.first.rows, allocated.rows));
                        }
                        if data.second.rows != allocated.rows {
                            errors.push(format!("H second.rows={} != {}", data.second.rows, allocated.rows));
                        }
                        let total = data.first.cols + 1 + data.second.cols;
                        if total != allocated.cols {
                            errors.push(format!("H cols {}+1+{}={} != {}", data.first.cols, data.second.cols, total, allocated.cols));
                        }
                    }
                    SplitDirection::Vertical => {
                        if data.first.cols != allocated.cols {
                            errors.push(format!("V first.cols={} != {}", data.first.cols, allocated.cols));
                        }
                        if data.second.cols != allocated.cols {
                            errors.push(format!("V second.cols={} != {}", data.second.cols, allocated.cols));
                        }
                        let total = data.first.rows + 1 + data.second.rows;
                        if total != allocated.rows {
                            errors.push(format!("V rows {}+1+{}={} != {}", data.first.rows, data.second.rows, total, allocated.rows));
                        }
                    }
                }
                check(left, &data.first, errors);
                check(right, &data.second, errors);
            }
        }
    }
    let mut errors = Vec::new();
    check(tree, size, &mut errors);
    assert!(errors.is_empty(), "Split tree invariant violation: {:?}", errors);
}

fn cell_dimensions(size: &TerminalSize) -> TerminalSize {
    TerminalSize {
        rows: 1,
        cols: 1,
        pixel_width: size.pixel_width / size.cols,
        pixel_height: size.pixel_height / size.rows,
        dpi: size.dpi,
    }
}

impl Tab {
    pub fn new(size: &TerminalSize) -> Self {
        let inner = TabInner::new(size);
        let tab_id = inner.id;
        Self {
            inner: Mutex::new(inner),
            tab_id,
        }
    }

    pub fn get_title(&self) -> String {
        self.inner.lock().title.clone()
    }

    pub fn set_title(&self, title: &str) {
        let mut inner = self.inner.lock();
        if inner.title != title {
            inner.title = title.to_string();
            Mux::try_get().map(|mux| {
                mux.notify(MuxNotification::TabTitleChanged {
                    tab_id: inner.id,
                    title: title.to_string(),
                })
            });
        }
    }

    /// Called by the multiplexer client when building a local tab to
    /// mirror a remote tab.  The supplied `root` is the information
    /// about our counterpart in the the remote server.
    /// This method builds a local tree based on the remote tree which
    /// then replaces the local tree structure.
    ///
    /// The `make_pane` function is provided by the caller, and its purpose
    /// is to lookup an existing Pane that corresponds to the provided
    /// PaneEntry, or to create a new Pane from that entry.
    /// make_pane is expected to add the pane to the mux if it creates
    /// a new pane, otherwise the pane won't poll/update in the GUI.
    pub fn sync_with_pane_tree<F>(&self, size: TerminalSize, root: PaneNode, make_pane: F)
    where
        F: FnMut(PaneEntry) -> Arc<dyn Pane>,
    {
        self.inner.lock().sync_with_pane_tree(size, root, make_pane)
    }

    pub fn codec_pane_tree(&self) -> PaneNode {
        self.inner.lock().codec_pane_tree()
    }

    /// Returns a count of how many panes are in this tab
    pub fn count_panes(&self) -> Option<usize> {
        self.inner.try_lock().map(|mut inner| inner.count_panes())
    }

    /// Sets the zoom state, returns the prior state
    pub fn set_zoomed(&self, zoomed: bool) -> bool {
        self.inner.lock().set_zoomed(zoomed)
    }

    pub fn toggle_zoom(&self) {
        self.inner.lock().toggle_zoom()
    }

    pub fn contains_pane(&self, pane: PaneId) -> bool {
        self.inner.lock().contains_pane(pane)
    }

    pub fn iter_panes(&self) -> Vec<PositionedPane> {
        self.inner.lock().iter_panes()
    }

    pub fn iter_panes_ignoring_zoom(&self) -> Vec<PositionedPane> {
        self.inner.lock().iter_panes_ignoring_zoom()
    }

    pub fn rotate_counter_clockwise(&self) {
        self.inner.lock().rotate_counter_clockwise()
    }

    pub fn rotate_clockwise(&self) {
        self.inner.lock().rotate_clockwise()
    }

    pub fn iter_splits(&self) -> Vec<PositionedSplit> {
        self.inner.lock().iter_splits()
    }

    pub fn tab_id(&self) -> TabId {
        self.tab_id
    }

    pub fn get_size(&self) -> TerminalSize {
        self.inner.lock().get_size()
    }

    /// Apply the new size of the tab to the panes contained within.
    /// The delta between the current and the new size is computed,
    /// and is distributed between the splits.  For small resizes
    /// this algorithm biases towards adjusting the left/top nodes
    /// first.  For large resizes this tends to proportionally adjust
    /// the relative sizes of the elements in a split.
    pub fn resize(&self, size: TerminalSize) {
        self.inner.lock().resize(size)
    }

    /// Collect the current pane sizes from the split tree.
    /// Returns (tab_id, vec of (pane_id, size)) suitable for building
    /// a batched ResizeTab PDU.
    pub fn collect_pane_sizes(&self) -> (TabId, Vec<(PaneId, TerminalSize)>) {
        let inner = self.inner.lock();
        let mut sizes = Vec::new();
        if let Some(root) = inner.pane.as_ref() {
            collect_pane_sizes(root, &inner.size, &mut sizes);
        }
        (inner.id, sizes)
    }

    /// Called when running in the mux server after an individual pane
    /// has been resized.
    /// Because the split manipulation happened on the GUI we "lost"
    /// the information that would have allowed us to call resize_split_by()
    /// and instead need to back-infer the split size information.
    /// We rely on the client to have resized (or be in the process
    /// of resizing) affected panes consistently with its own Tab
    /// tree model.
    /// This method does a simple tree walk to the leaves to back-propagate
    /// the size of the panes up to their containing node split data.
    /// Without this step, disconnecting and reconnecting would cause
    /// the GUI to use stale size information for the window it spawns
    /// to attach this tab.
    pub fn rebuild_splits_sizes_from_contained_panes(&self) {
        self.inner
            .lock()
            .rebuild_splits_sizes_from_contained_panes()
    }

    /// Given split_index, the topological index of a split returned by
    /// iter_splits() as PositionedSplit::index, revised the split position
    /// by the provided delta; positive values move the split to the right/bottom,
    /// and negative values to the left/top.
    /// The adjusted size is propogated downwards to contained children and
    /// their panes are resized accordingly.
    pub fn resize_split_by(&self, split_index: usize, delta: isize) {
        self.inner.lock().resize_split_by(split_index, delta)
    }

    /// Adjusts the size of the active pane in the specified direction
    /// by the specified amount.
    pub fn adjust_pane_size(&self, direction: PaneDirection, amount: usize) {
        self.inner.lock().adjust_pane_size(direction, amount)
    }

    /// Activate an adjacent pane in the specified direction.
    /// In cases where there are multiple adjacent panes in the
    /// intended direction, we take the pane that has the largest
    /// edge intersection.
    pub fn activate_pane_direction(&self, direction: PaneDirection) {
        self.inner.lock().activate_pane_direction(direction)
    }

    /// Returns an adjacent pane in the specified direction.
    /// In cases where there are multiple adjacent panes in the
    /// intended direction, we take the pane that has the largest
    /// edge intersection.
    pub fn get_pane_direction(&self, direction: PaneDirection, ignore_zoom: bool) -> Option<usize> {
        self.inner.lock().get_pane_direction(direction, ignore_zoom)
    }

    pub fn prune_dead_panes(&self) -> bool {
        self.inner.lock().prune_dead_panes()
    }

    pub fn kill_pane(&self, pane_id: PaneId) -> bool {
        self.inner.lock().kill_pane(pane_id)
    }

    pub fn kill_panes_in_domain(&self, domain: DomainId) -> bool {
        self.inner.lock().kill_panes_in_domain(domain)
    }

    /// Remove pane from tab.
    /// The pane is still live in the mux; the intent is for the pane to
    /// be added to a different tab.
    pub fn remove_pane(&self, pane_id: PaneId) -> Option<Arc<dyn Pane>> {
        self.inner.lock().remove_pane(pane_id)
    }

    pub fn can_close_without_prompting(&self, reason: CloseReason) -> bool {
        self.inner.lock().can_close_without_prompting(reason)
    }

    pub fn is_dead(&self) -> bool {
        self.inner.lock().is_dead()
    }

    pub fn get_active_pane(&self) -> Option<Arc<dyn Pane>> {
        self.inner.lock().get_active_pane()
    }

    #[allow(unused)]
    pub fn get_active_idx(&self) -> usize {
        self.inner.lock().get_active_idx()
    }

    pub fn set_active_pane(&self, pane: &Arc<dyn Pane>) {
        self.inner.lock().set_active_pane(pane)
    }

    pub fn set_active_idx(&self, pane_index: usize) {
        self.inner.lock().set_active_idx(pane_index)
    }

    /// Assigns the root pane.
    /// This is suitable when creating a new tab and then assigning
    /// the initial pane
    pub fn assign_pane(&self, pane: &Arc<dyn Pane>) {
        self.inner.lock().assign_pane(pane)
    }

    /// Swap the active pane with the specified pane_index
    pub fn swap_active_with_index(&self, pane_index: usize, keep_focus: bool) -> Option<()> {
        self.inner
            .lock()
            .swap_active_with_index(pane_index, keep_focus)
    }

    /// Computes the size of the pane that would result if the specified
    /// pane was split in a particular direction.
    /// The intent is to call this prior to spawning the new pane so that
    /// you can create it with the correct size.
    /// May return None if the specified pane_index is invalid.
    pub fn compute_split_size(
        &self,
        pane_index: usize,
        request: SplitRequest,
    ) -> Option<SplitDirectionAndSize> {
        self.inner.lock().compute_split_size(pane_index, request)
    }

    /// Split the pane that has pane_index in the given direction and assign
    /// the right/bottom pane of the newly created split to the provided Pane
    /// instance.  Returns the resultant index of the newly inserted pane.
    /// Both the split and the inserted pane will be resized.
    pub fn split_and_insert(
        &self,
        pane_index: usize,
        request: SplitRequest,
        pane: Arc<dyn Pane>,
    ) -> anyhow::Result<usize> {
        self.inner
            .lock()
            .split_and_insert(pane_index, request, pane)
    }

    pub fn get_zoomed_pane(&self) -> Option<Arc<dyn Pane>> {
        self.inner.lock().get_zoomed_pane()
    }
}

impl TabInner {
    fn new(size: &TerminalSize) -> Self {
        Self {
            id: TAB_ID.fetch_add(1, ::std::sync::atomic::Ordering::Relaxed),
            pane: Some(Tree::new()),
            size: *size,
            size_before_zoom: *size,
            active: 0,
            zoomed: None,
            title: String::new(),
            recency: Recency::default(),
        }
    }

    fn sync_with_pane_tree<F>(&mut self, size: TerminalSize, root: PaneNode, mut make_pane: F)
    where
        F: FnMut(PaneEntry) -> Arc<dyn Pane>,
    {
        let mut active = None;
        let mut zoomed = None;

        log::debug!("sync_with_pane_tree with size {:?}", size);

        let t = build_from_pane_tree(root.into_tree(), &mut active, &mut zoomed, &mut make_pane);
        let mut cursor = t.cursor();

        self.active = 0;
        if let Some(active) = active {
            // Resolve the active pane to its index
            let mut index = 0;
            loop {
                if let Some(pane) = cursor.leaf_mut() {
                    if active.pane_id() == pane.pane_id() {
                        // Found it
                        self.active = index;
                        self.recency.tag(index);
                        break;
                    }
                    index += 1;
                }
                match cursor.preorder_next() {
                    Ok(c) => cursor = c,
                    Err(c) => {
                        // Didn't find it
                        cursor = c;
                        break;
                    }
                }
            }
        }
        self.pane.replace(cursor.tree());
        self.zoomed = zoomed;
        self.size = size;

        self.resize(size);

        log::debug!(
            "sync tab: {:#?} zoomed: {} {:#?}",
            size,
            self.zoomed.is_some(),
            self.iter_panes()
        );
        assert!(self.pane.is_some());
    }

    fn codec_pane_tree(&mut self) -> PaneNode {
        let mux = Mux::get();
        let tab_id = self.id;
        let window_id = match mux.window_containing_tab(tab_id) {
            Some(w) => w,
            None => {
                log::error!("no window contains tab {}", tab_id);
                return PaneNode::Empty;
            }
        };

        let workspace = match mux
            .get_window(window_id)
            .map(|w| w.get_workspace().to_string())
        {
            Some(ws) => ws,
            None => {
                log::error!("window id {} doesn't have a window!?", window_id);
                return PaneNode::Empty;
            }
        };

        let active = self.get_active_pane();
        let zoomed = self.zoomed.as_ref();
        if let Some(root) = self.pane.as_ref() {
            pane_tree(
                root,
                tab_id,
                window_id,
                active.as_ref(),
                zoomed,
                &workspace,
                0,
                0,
            )
        } else {
            PaneNode::Empty
        }
    }

    /// Returns a count of how many panes are in this tab
    fn count_panes(&mut self) -> usize {
        let mut count = 0;
        let mut cursor = self.pane.take().unwrap().cursor();

        loop {
            if cursor.is_leaf() {
                count += 1;
            }
            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    return count;
                }
            }
        }
    }

    /// Sets the zoom state, returns the prior state
    fn set_zoomed(&mut self, zoomed: bool) -> bool {
        if self.zoomed.is_some() == zoomed {
            // Current zoom state matches intended zoom state,
            // so we have nothing to do.
            return zoomed;
        }
        self.toggle_zoom();
        !zoomed
    }

    fn toggle_zoom(&mut self) {
        let size = self.size;
        if self.zoomed.take().is_some() {
            // We were zoomed, but now we are not.
            // Re-apply the size to the panes
            if let Some(pane) = self.get_active_pane() {
                pane.set_zoomed(false);
            }
            self.size = self.size_before_zoom;
            self.resize(size);
        } else {
            // We weren't zoomed, but now we want to zoom.
            // Locate the active pane
            self.size_before_zoom = size;
            if let Some(pane) = self.get_active_pane() {
                pane.set_zoomed(true);
                pane.resize(size).ok();
                self.zoomed.replace(pane);
            }
        }
        Mux::try_get().map(|mux| mux.notify(MuxNotification::TabResized(self.id)));
    }

    fn contains_pane(&self, pane: PaneId) -> bool {
        fn contains(tree: &Tree, pane: PaneId) -> bool {
            match tree {
                Tree::Empty => false,
                Tree::Node { left, right, .. } => contains(left, pane) || contains(right, pane),
                Tree::Leaf(p) => p.pane_id() == pane,
            }
        }
        match &self.pane {
            Some(root) => contains(root, pane),
            None => false,
        }
    }

    /// Walks the pane tree to produce the topologically ordered flattened
    /// list of PositionedPane instances along with their positioning information.
    fn iter_panes(&mut self) -> Vec<PositionedPane> {
        self.iter_panes_impl(true)
    }

    /// Like iter_panes, except that it will include all panes, regardless of
    /// whether one of them is currently zoomed.
    fn iter_panes_ignoring_zoom(&mut self) -> Vec<PositionedPane> {
        self.iter_panes_impl(false)
    }

    fn rotate_counter_clockwise(&mut self) {
        let panes = self.iter_panes_ignoring_zoom();
        if panes.is_empty() {
            // Shouldn't happen, but we check for this here so that the
            // expect below cannot trigger a panic
            return;
        }
        let mut pane_to_swap = panes
            .first()
            .map(|p| p.pane.clone())
            .expect("at least one pane");

        let mut cursor = self.pane.take().unwrap().cursor();

        loop {
            if cursor.is_leaf() {
                std::mem::swap(&mut pane_to_swap, cursor.leaf_mut().unwrap());
            }

            match cursor.postorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    let size = self.size;
                    apply_sizes_from_splits(self.pane.as_mut().unwrap(), &size);
                    break;
                }
            }
        }
    }

    fn rotate_clockwise(&mut self) {
        let panes = self.iter_panes_ignoring_zoom();
        if panes.is_empty() {
            // Shouldn't happen, but we check for this here so that the
            // expect below cannot trigger a panic
            return;
        }
        let mut pane_to_swap = panes
            .last()
            .map(|p| p.pane.clone())
            .expect("at least one pane");

        let mut cursor = self.pane.take().unwrap().cursor();

        loop {
            if cursor.is_leaf() {
                std::mem::swap(&mut pane_to_swap, cursor.leaf_mut().unwrap());
            }

            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    let size = self.size;
                    apply_sizes_from_splits(self.pane.as_mut().unwrap(), &size);
                    break;
                }
            }
        }
        Mux::try_get().map(|mux| mux.notify(MuxNotification::TabResized(self.id)));
    }

    fn iter_panes_impl(&mut self, respect_zoom_state: bool) -> Vec<PositionedPane> {
        let mut panes = vec![];

        if respect_zoom_state {
            if let Some(zoomed) = self.zoomed.as_ref() {
                let size = self.size;
                panes.push(PositionedPane {
                    index: 0,
                    is_active: true,
                    is_zoomed: true,
                    left: 0,
                    top: 0,
                    width: size.cols.into(),
                    pixel_width: size.pixel_width.into(),
                    height: size.rows.into(),
                    pixel_height: size.pixel_height.into(),
                    pane: Arc::clone(zoomed),
                });
                return panes;
            }
        }

        let active_idx = self.active;
        let zoomed_id = self.zoomed.as_ref().map(|p| p.pane_id());
        let root_size = self.size;
        let mut cursor = self.pane.take().unwrap().cursor();

        loop {
            if cursor.is_leaf() {
                let index = panes.len();
                let mut left = 0usize;
                let mut top = 0usize;
                let mut parent_size = None;
                for (branch, node) in cursor.path_to_root() {
                    if let Some(node) = node {
                        if parent_size.is_none() {
                            parent_size.replace(if branch == PathBranch::IsRight {
                                node.second
                            } else {
                                node.first
                            });
                        }
                        if branch == PathBranch::IsRight {
                            top += node.top_of_second();
                            left += node.left_of_second();
                        }
                    }
                }

                let pane = Arc::clone(cursor.leaf_mut().unwrap());
                let dims = parent_size.unwrap_or_else(|| root_size);

                panes.push(PositionedPane {
                    index,
                    is_active: index == active_idx,
                    is_zoomed: zoomed_id == Some(pane.pane_id()),
                    left,
                    top,
                    width: dims.cols as _,
                    height: dims.rows as _,
                    pixel_width: dims.pixel_width as _,
                    pixel_height: dims.pixel_height as _,
                    pane,
                });
            }

            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    break;
                }
            }
        }

        panes
    }

    fn iter_splits(&mut self) -> Vec<PositionedSplit> {
        let mut dividers = vec![];
        if self.zoomed.is_some() {
            return dividers;
        }

        let mut cursor = self.pane.take().unwrap().cursor();
        let mut index = 0;

        loop {
            if !cursor.is_leaf() {
                let mut left = 0usize;
                let mut top = 0usize;
                for (branch, p) in cursor.path_to_root() {
                    if let Some(p) = p {
                        if branch == PathBranch::IsRight {
                            left += p.left_of_second();
                            top += p.top_of_second();
                        }
                    }
                }
                if let Ok(Some(node)) = cursor.node_mut() {
                    match node.direction {
                        SplitDirection::Horizontal => left += node.first.cols as usize,
                        SplitDirection::Vertical => top += node.first.rows as usize,
                    }

                    dividers.push(PositionedSplit {
                        index,
                        direction: node.direction,
                        left,
                        top,
                        size: if node.direction == SplitDirection::Horizontal {
                            node.height() as usize
                        } else {
                            node.width() as usize
                        },
                    })
                }
                index += 1;
            }

            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    break;
                }
            }
        }

        dividers
    }

    fn get_size(&self) -> TerminalSize {
        self.size
    }

    fn resize(&mut self, size: TerminalSize) {
        if size.rows == 0 || size.cols == 0 {
            // Ignore "impossible" resize requests
            return;
        }

        if let Some(zoomed) = &self.zoomed {
            self.size = size;
            zoomed.resize(size).ok();
        } else {
            let dims = cell_dimensions(&size);
            let (min_x, min_y) = compute_min_size(self.pane.as_mut().unwrap());
            let current_size = self.size;

            // Constrain the new size to the minimum possible dimensions
            let cols = size.cols.max(min_x);
            let rows = size.rows.max(min_y);
            let size = TerminalSize {
                rows,
                cols,
                pixel_width: cols * dims.pixel_width,
                pixel_height: rows * dims.pixel_height,
                dpi: dims.dpi,
            };

            // Update the split nodes with adjusted sizes
            adjust_x_size(
                self.pane.as_mut().unwrap(),
                cols as isize - current_size.cols as isize,
                &dims,
            );
            adjust_y_size(
                self.pane.as_mut().unwrap(),
                rows as isize - current_size.rows as isize,
                &dims,
            );

            self.size = size;

            // Enforce top-down constraints before applying sizes to panes.
            reconcile_tree_sizes(self.pane.as_mut().unwrap(), &size);

            // And then resize the individual panes to match
            apply_sizes_from_splits(self.pane.as_mut().unwrap(), &size);

            // Send a batched ResizeTab PDU with all pane sizes at once,
            // preventing interleaving from individual per-pane PDUs.
            let mut pane_sizes = Vec::new();
            collect_pane_sizes(self.pane.as_ref().unwrap(), &size, &mut pane_sizes);
            if let Some(first_pane) = self.get_active_pane() {
                first_pane.send_resize_batch(self.id, pane_sizes);
            }

            #[cfg(debug_assertions)]
            debug_assert_tree_invariants(self.pane.as_ref().unwrap(), &self.size);
        }

        Mux::try_get().map(|mux| mux.notify(MuxNotification::TabResized(self.id)));
    }

    fn apply_pane_size(&mut self, pane_size: TerminalSize, cursor: &mut Cursor) {
        let cell_width = pane_size
            .pixel_width
            .checked_div(pane_size.cols)
            .unwrap_or(1);
        let cell_height = pane_size
            .pixel_height
            .checked_div(pane_size.rows)
            .unwrap_or(1);
        if let Ok(Some(node)) = cursor.node_mut() {
            // Adjust the size of the node; we preserve the size of the first
            // child and adjust the second, so if we are split down the middle
            // and the window is made wider, the right column will grow in
            // size, leaving the left at its current width.
            if node.direction == SplitDirection::Horizontal {
                node.first.rows = pane_size.rows;
                node.second.rows = pane_size.rows;

                node.second.cols = pane_size.cols.saturating_sub(1 + node.first.cols);
            } else {
                node.first.cols = pane_size.cols;
                node.second.cols = pane_size.cols;

                node.second.rows = pane_size.rows.saturating_sub(1 + node.first.rows);
            }
            node.first.pixel_width = node.first.cols * cell_width;
            node.first.pixel_height = node.first.rows * cell_height;

            node.second.pixel_width = node.second.cols * cell_width;
            node.second.pixel_height = node.second.rows * cell_height;
        }
    }

    fn rebuild_splits_sizes_from_contained_panes(&mut self) {
        if self.zoomed.is_some() {
            return;
        }

        fn compute_size(node: &mut Tree) -> Option<TerminalSize> {
            match node {
                Tree::Empty => None,
                Tree::Leaf(pane) => {
                    let dims = pane.get_dimensions();
                    let size = TerminalSize {
                        cols: dims.cols,
                        rows: dims.viewport_rows,
                        pixel_height: dims.pixel_height,
                        pixel_width: dims.pixel_width,
                        dpi: dims.dpi,
                    };
                    Some(size)
                }
                Tree::Node { left, right, data } => {
                    if let Some(data) = data {
                        if let Some(first) = compute_size(left) {
                            data.first = first;
                        }
                        if let Some(second) = compute_size(right) {
                            data.second = second;
                        }
                        Some(data.size())
                    } else {
                        None
                    }
                }
            }
        }

        if let Some(root) = self.pane.as_mut() {
            if let Some(size) = compute_size(root) {
                self.size = size;
                reconcile_tree_sizes(root, &self.size);

                #[cfg(debug_assertions)]
                debug_assert_tree_invariants(root, &self.size);
            }
        }
        Mux::try_get().map(|mux| mux.notify(MuxNotification::TabResized(self.id)));
    }

    fn resize_split_by(&mut self, split_index: usize, delta: isize) {
        if self.zoomed.is_some() {
            return;
        }

        let mut cursor = self.pane.take().unwrap().cursor();
        let mut index = 0;

        // Position cursor on the specified split
        loop {
            if !cursor.is_leaf() {
                if index == split_index {
                    // Found it
                    break;
                }
                index += 1;
            }
            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    // Didn't find it
                    self.pane.replace(c.tree());
                    return;
                }
            }
        }

        // Now cursor is looking at the split
        self.adjust_node_at_cursor(&mut cursor, delta);
        self.cascade_size_from_cursor(cursor);
        Mux::try_get().map(|mux| mux.notify(MuxNotification::TabResized(self.id)));
    }

    fn adjust_node_at_cursor(&mut self, cursor: &mut Cursor, delta: isize) {
        let cell_dimensions = self.cell_dimensions();
        if let Ok(Some(node)) = cursor.node_mut() {
            match node.direction {
                SplitDirection::Horizontal => {
                    let width = node.width();

                    let mut cols = node.first.cols as isize;
                    cols = cols
                        .saturating_add(delta)
                        .max(1)
                        .min((width as isize).saturating_sub(2));
                    node.first.cols = cols as usize;
                    node.first.pixel_width =
                        node.first.cols.saturating_mul(cell_dimensions.pixel_width);

                    node.second.cols = width.saturating_sub(node.first.cols.saturating_add(1));
                    node.second.pixel_width =
                        node.second.cols.saturating_mul(cell_dimensions.pixel_width);
                }
                SplitDirection::Vertical => {
                    let height = node.height();

                    let mut rows = node.first.rows as isize;
                    rows = rows
                        .saturating_add(delta)
                        .max(1)
                        .min((height as isize).saturating_sub(2));
                    node.first.rows = rows as usize;
                    node.first.pixel_height =
                        node.first.rows.saturating_mul(cell_dimensions.pixel_height);

                    node.second.rows = height.saturating_sub(node.first.rows.saturating_add(1));
                    node.second.pixel_height = node
                        .second
                        .rows
                        .saturating_mul(cell_dimensions.pixel_height);
                }
            }
        }
    }

    fn cascade_size_from_cursor(&mut self, mut cursor: Cursor) {
        // Now we need to cascade this down to children
        match cursor.preorder_next() {
            Ok(c) => cursor = c,
            Err(c) => {
                self.pane.replace(c.tree());
                return;
            }
        }
        let root_size = self.size;

        loop {
            // Figure out the available size by looking at our immediate parent node.
            // If we are the root, look at the provided new size
            let pane_size = if let Some((branch, Some(parent))) = cursor.path_to_root().next() {
                if branch == PathBranch::IsRight {
                    parent.second
                } else {
                    parent.first
                }
            } else {
                root_size
            };

            if cursor.is_leaf() {
                // Apply our size to the tty
                cursor.leaf_mut().map(|pane| pane.resize(pane_size));
            } else {
                self.apply_pane_size(pane_size, &mut cursor);
            }
            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    break;
                }
            }
        }
        Mux::try_get().map(|mux| mux.notify(MuxNotification::TabResized(self.id)));
    }

    fn adjust_pane_size(&mut self, direction: PaneDirection, amount: usize) {
        if self.zoomed.is_some() {
            return;
        }
        let active_index = self.active;
        let mut cursor = self.pane.take().unwrap().cursor();
        let mut index = 0;

        // Position cursor on the active leaf
        loop {
            if cursor.is_leaf() {
                if index == active_index {
                    // Found it
                    break;
                }
                index += 1;
            }
            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    // Didn't find it
                    self.pane.replace(c.tree());
                    return;
                }
            }
        }

        // We are on the active leaf.
        // Now we go up until we find the parent node that is
        // aligned with the desired direction.
        let split_direction = match direction {
            PaneDirection::Left | PaneDirection::Right => SplitDirection::Horizontal,
            PaneDirection::Up | PaneDirection::Down => SplitDirection::Vertical,
            PaneDirection::Next | PaneDirection::Prev => unreachable!(),
        };
        let delta = match direction {
            PaneDirection::Down | PaneDirection::Right => amount as isize,
            PaneDirection::Up | PaneDirection::Left => -(amount as isize),
            PaneDirection::Next | PaneDirection::Prev => unreachable!(),
        };
        loop {
            match cursor.go_up() {
                Ok(mut c) => {
                    if let Ok(Some(node)) = c.node_mut() {
                        if node.direction == split_direction {
                            self.adjust_node_at_cursor(&mut c, delta);
                            self.cascade_size_from_cursor(c);
                            return;
                        }
                    }

                    cursor = c;
                }

                Err(c) => {
                    self.pane.replace(c.tree());
                    return;
                }
            }
        }
    }

    fn activate_pane_direction(&mut self, direction: PaneDirection) {
        if self.zoomed.is_some() {
            if !configuration().unzoom_on_switch_pane {
                return;
            }
            self.toggle_zoom();
        }
        if let Some(panel_idx) = self.get_pane_direction(direction, false) {
            self.set_active_idx(panel_idx);
        }
        let mux = Mux::get();
        if let Some(window_id) = mux.window_containing_tab(self.id) {
            mux.notify(MuxNotification::WindowInvalidated(window_id));
        }
    }

    fn get_pane_direction(&mut self, direction: PaneDirection, ignore_zoom: bool) -> Option<usize> {
        let panes = if ignore_zoom {
            self.iter_panes_ignoring_zoom()
        } else {
            self.iter_panes()
        };

        let active = match panes.iter().find(|pane| pane.is_active) {
            Some(p) => p,
            None => {
                // No active pane somehow...
                return Some(0);
            }
        };

        if matches!(direction, PaneDirection::Next | PaneDirection::Prev) {
            let max_pane_id = panes.iter().map(|p| p.index).max().unwrap_or(active.index);

            return Some(if direction == PaneDirection::Next {
                if active.index == max_pane_id {
                    0
                } else {
                    active.index + 1
                }
            } else {
                if active.index == 0 {
                    max_pane_id
                } else {
                    active.index - 1
                }
            });
        }

        let mut best = None;

        let recency = &self.recency;

        fn edge_intersects(
            active_start: usize,
            active_size: usize,
            current_start: usize,
            current_size: usize,
        ) -> bool {
            intersects_range(
                &(active_start..active_start + active_size),
                &(current_start..current_start + current_size),
            )
        }

        for pane in &panes {
            let score = match direction {
                PaneDirection::Right => {
                    if pane.left == active.left + active.width + 1
                        && edge_intersects(active.top, active.height, pane.top, pane.height)
                    {
                        1 + recency.score(pane.index)
                    } else {
                        0
                    }
                }
                PaneDirection::Left => {
                    if pane.left + pane.width + 1 == active.left
                        && edge_intersects(active.top, active.height, pane.top, pane.height)
                    {
                        1 + recency.score(pane.index)
                    } else {
                        0
                    }
                }
                PaneDirection::Up => {
                    if pane.top + pane.height + 1 == active.top
                        && edge_intersects(active.left, active.width, pane.left, pane.width)
                    {
                        1 + recency.score(pane.index)
                    } else {
                        0
                    }
                }
                PaneDirection::Down => {
                    if active.top + active.height + 1 == pane.top
                        && edge_intersects(active.left, active.width, pane.left, pane.width)
                    {
                        1 + recency.score(pane.index)
                    } else {
                        0
                    }
                }
                PaneDirection::Next | PaneDirection::Prev => unreachable!(),
            };

            if score > 0 {
                let target = match best.take() {
                    Some((best_score, best_pane)) if best_score > score => (best_score, best_pane),
                    _ => (score, pane),
                };
                best.replace(target);
            }
        }

        if let Some((_, target)) = best.take() {
            return Some(target.index);
        }
        None
    }

    fn prune_dead_panes(&mut self) -> bool {
        let mux = Mux::get();
        !self
            .remove_pane_if(
                |_, pane| {
                    // If the pane is no longer known to the mux, then its liveness
                    // state isn't guaranteed to be monitored or updated, so let's
                    // consider the pane effectively dead if it isn't in the mux.
                    // <https://github.com/wezterm/wezterm/issues/4030>
                    let in_mux = mux.get_pane(pane.pane_id()).is_some();
                    let dead = pane.is_dead();
                    log::trace!(
                        "prune_dead_panes: pane_id={} dead={} in_mux={}",
                        pane.pane_id(),
                        dead,
                        in_mux
                    );
                    dead || !in_mux
                },
                true,
            )
            .is_empty()
    }

    fn kill_pane(&mut self, pane_id: PaneId) -> bool {
        !self
            .remove_pane_if(|_, pane| pane.pane_id() == pane_id, true)
            .is_empty()
    }

    fn kill_panes_in_domain(&mut self, domain: DomainId) -> bool {
        !self
            .remove_pane_if(|_, pane| pane.domain_id() == domain, true)
            .is_empty()
    }

    fn remove_pane(&mut self, pane_id: PaneId) -> Option<Arc<dyn Pane>> {
        let panes = self.remove_pane_if(|_, pane| pane.pane_id() == pane_id, false);
        for pane in panes {
            return Some(pane);
        }
        None
    }

    fn remove_pane_if<F>(&mut self, f: F, kill: bool) -> Vec<Arc<dyn Pane>>
    where
        F: Fn(usize, &Arc<dyn Pane>) -> bool,
    {
        let mut dead_panes = vec![];
        let zoomed_pane = self.zoomed.as_ref().map(|p| p.pane_id());

        {
            let root_size = self.size;
            let mut cursor = self.pane.take().unwrap().cursor();
            let mut pane_index = 0;
            let mut removed_indices = vec![];
            let cell_dims = self.cell_dimensions();

            loop {
                // Figure out the available size by looking at our immediate parent node.
                // If we are the root, look at the tab size
                let pane_size = if let Some((branch, Some(parent))) = cursor.path_to_root().next() {
                    if branch == PathBranch::IsRight {
                        parent.second
                    } else {
                        parent.first
                    }
                } else {
                    root_size
                };

                if cursor.is_leaf() {
                    let pane = Arc::clone(cursor.leaf_mut().unwrap());
                    if f(pane_index, &pane) {
                        removed_indices.push(pane_index);
                        if Some(pane.pane_id()) == zoomed_pane {
                            // If we removed the zoomed pane, un-zoom our state!
                            self.zoomed.take();
                        }
                        let parent;
                        match cursor.unsplit_leaf() {
                            Ok((c, dead, p)) => {
                                dead_panes.push(dead);
                                parent = p.unwrap();
                                cursor = c;
                            }
                            Err(c) => {
                                // We might be the root, for example
                                if c.is_top() && c.is_leaf() {
                                    self.pane.replace(Tree::Empty);
                                    dead_panes.push(pane);
                                } else {
                                    self.pane.replace(c.tree());
                                }
                                break;
                            }
                        };

                        // Now we need to increase the size of the current node
                        // and propagate the revised size to its children.
                        let size = TerminalSize {
                            rows: parent.height(),
                            cols: parent.width(),
                            pixel_width: cell_dims.pixel_width * parent.width(),
                            pixel_height: cell_dims.pixel_height * parent.height(),
                            dpi: cell_dims.dpi,
                        };

                        if let Some(unsplit) = cursor.leaf_mut() {
                            unsplit.resize(size).ok();
                        } else {
                            self.apply_pane_size(size, &mut cursor);
                        }
                    } else if !dead_panes.is_empty() {
                        // Apply our revised size to the tty
                        pane.resize(pane_size).ok();
                    }

                    pane_index += 1;
                } else if !dead_panes.is_empty() {
                    self.apply_pane_size(pane_size, &mut cursor);
                }
                match cursor.preorder_next() {
                    Ok(c) => cursor = c,
                    Err(c) => {
                        self.pane.replace(c.tree());
                        break;
                    }
                }
            }

            // Figure out which pane should now be active.
            // If panes earlier than the active pane were closed, then we
            // need to shift the active pane down
            let active_idx = self.active;
            removed_indices.retain(|&idx| idx <= active_idx);
            self.active = active_idx.saturating_sub(removed_indices.len());
        }

        if !dead_panes.is_empty() && kill {
            let to_kill: Vec<_> = dead_panes.iter().map(|p| p.pane_id()).collect();
            promise::spawn::spawn_into_main_thread(async move {
                let mux = Mux::get();
                for pane_id in to_kill.into_iter() {
                    mux.remove_pane(pane_id);
                }
            })
            .detach();
        }
        dead_panes
    }

    fn can_close_without_prompting(&mut self, reason: CloseReason) -> bool {
        let panes = self.iter_panes_ignoring_zoom();
        for pos in &panes {
            if !pos.pane.can_close_without_prompting(reason) {
                return false;
            }
        }
        true
    }

    fn is_dead(&mut self) -> bool {
        // Make sure we account for all panes, so that we don't
        // kill the whole tab if the zoomed pane is dead!
        let panes = self.iter_panes_ignoring_zoom();
        let mut dead_count = 0;
        for pos in &panes {
            if pos.pane.is_dead() {
                dead_count += 1;
            }
        }
        dead_count == panes.len()
    }

    fn get_active_pane(&mut self) -> Option<Arc<dyn Pane>> {
        if let Some(zoomed) = self.zoomed.as_ref() {
            return Some(Arc::clone(zoomed));
        }

        self.iter_panes_ignoring_zoom()
            .iter()
            .nth(self.active)
            .map(|p| Arc::clone(&p.pane))
    }

    fn get_active_idx(&self) -> usize {
        self.active
    }

    fn set_active_pane(&mut self, pane: &Arc<dyn Pane>) {
        let prior = self.get_active_pane();

        if is_pane(pane, &prior.as_ref()) {
            return;
        }

        if self.zoomed.is_some() {
            if !configuration().unzoom_on_switch_pane {
                return;
            }
            self.toggle_zoom();
        }

        if let Some(item) = self
            .iter_panes_ignoring_zoom()
            .iter()
            .find(|p| p.pane.pane_id() == pane.pane_id())
        {
            self.active = item.index;
            self.recency.tag(item.index);
            self.advise_focus_change(prior);
        }
    }

    fn advise_focus_change(&mut self, prior: Option<Arc<dyn Pane>>) {
        let mux = Mux::get();
        let current = self.get_active_pane();
        match (prior, current) {
            (Some(prior), Some(current)) if prior.pane_id() != current.pane_id() => {
                prior.focus_changed(false);
                current.focus_changed(true);
                mux.notify(MuxNotification::PaneFocused(current.pane_id()));
            }
            (None, Some(current)) => {
                current.focus_changed(true);
                mux.notify(MuxNotification::PaneFocused(current.pane_id()));
            }
            (Some(prior), None) => {
                prior.focus_changed(false);
            }
            (Some(_), Some(_)) | (None, None) => {
                // no change
            }
        }
    }

    fn set_active_idx(&mut self, pane_index: usize) {
        let prior = self.get_active_pane();
        self.active = pane_index;
        self.recency.tag(pane_index);
        self.advise_focus_change(prior);
    }

    fn assign_pane(&mut self, pane: &Arc<dyn Pane>) {
        match Tree::new().cursor().assign_top(Arc::clone(pane)) {
            Ok(c) => self.pane = Some(c.tree()),
            Err(_) => panic!("tried to assign root pane to non-empty tree"),
        }
    }

    fn cell_dimensions(&self) -> TerminalSize {
        cell_dimensions(&self.size)
    }

    fn swap_active_with_index(&mut self, pane_index: usize, keep_focus: bool) -> Option<()> {
        let active_idx = self.get_active_idx();
        let mut pane = self.get_active_pane()?;
        log::trace!(
            "swap_active_with_index: pane_index {} active {}",
            pane_index,
            active_idx
        );

        {
            let mut cursor = self.pane.take().unwrap().cursor();

            // locate the requested index
            match cursor.go_to_nth_leaf(pane_index) {
                Ok(c) => cursor = c,
                Err(c) => {
                    log::trace!("didn't find pane {pane_index}");
                    self.pane.replace(c.tree());
                    return None;
                }
            };

            std::mem::swap(&mut pane, cursor.leaf_mut().unwrap());

            // re-position to the root
            cursor = cursor.tree().cursor();

            // and now go and update the active idx
            match cursor.go_to_nth_leaf(active_idx) {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    log::trace!("didn't find active {active_idx}");
                    return None;
                }
            };

            std::mem::swap(&mut pane, cursor.leaf_mut().unwrap());
            self.pane.replace(cursor.tree());

            // Advise the panes of their new sizes
            let size = self.size;
            apply_sizes_from_splits(self.pane.as_mut().unwrap(), &size);
        }

        // And update focus
        if keep_focus {
            self.set_active_idx(pane_index);
        } else {
            self.advise_focus_change(Some(pane));
        }
        None
    }

    fn compute_split_size(
        &mut self,
        pane_index: usize,
        request: SplitRequest,
    ) -> Option<SplitDirectionAndSize> {
        let cell_dims = self.cell_dimensions();

        fn split_dimension(dim: usize, request: SplitRequest) -> (usize, usize) {
            let target_size = match request.size {
                SplitSize::Cells(n) => n,
                SplitSize::Percent(n) => (dim * (n as usize)) / 100,
            }
            .max(1);

            let remain = dim.saturating_sub(target_size + 1);

            if request.target_is_second {
                (remain, target_size)
            } else {
                (target_size, remain)
            }
        }

        if request.top_level {
            let size = self.size;

            let ((width1, width2), (height1, height2)) = match request.direction {
                SplitDirection::Horizontal => (
                    split_dimension(size.cols as usize, request),
                    (size.rows as usize, size.rows as usize),
                ),
                SplitDirection::Vertical => (
                    (size.cols as usize, size.cols as usize),
                    split_dimension(size.rows as usize, request),
                ),
            };

            return Some(SplitDirectionAndSize {
                direction: request.direction,
                first: TerminalSize {
                    rows: height1 as _,
                    cols: width1 as _,
                    pixel_height: cell_dims.pixel_height * height1,
                    pixel_width: cell_dims.pixel_width * width1,
                    dpi: cell_dims.dpi,
                },
                second: TerminalSize {
                    rows: height2 as _,
                    cols: width2 as _,
                    pixel_height: cell_dims.pixel_height * height2,
                    pixel_width: cell_dims.pixel_width * width2,
                    dpi: cell_dims.dpi,
                },
            });
        }

        // Ensure that we're not zoomed, otherwise we'll end up in
        // a bogus split state (https://github.com/wezterm/wezterm/issues/723)
        self.set_zoomed(false);

        self.iter_panes().iter().nth(pane_index).map(|pos| {
            let ((width1, width2), (height1, height2)) = match request.direction {
                SplitDirection::Horizontal => (
                    split_dimension(pos.width, request),
                    (pos.height, pos.height),
                ),
                SplitDirection::Vertical => {
                    ((pos.width, pos.width), split_dimension(pos.height, request))
                }
            };

            SplitDirectionAndSize {
                direction: request.direction,
                first: TerminalSize {
                    rows: height1 as _,
                    cols: width1 as _,
                    pixel_height: cell_dims.pixel_height * height1,
                    pixel_width: cell_dims.pixel_width * width1,
                    dpi: cell_dims.dpi,
                },
                second: TerminalSize {
                    rows: height2 as _,
                    cols: width2 as _,
                    pixel_height: cell_dims.pixel_height * height2,
                    pixel_width: cell_dims.pixel_width * width2,
                    dpi: cell_dims.dpi,
                },
            }
        })
    }

    fn split_and_insert(
        &mut self,
        pane_index: usize,
        request: SplitRequest,
        pane: Arc<dyn Pane>,
    ) -> anyhow::Result<usize> {
        if self.zoomed.is_some() {
            anyhow::bail!("cannot split while zoomed");
        }

        {
            let split_info = self
                .compute_split_size(pane_index, request)
                .ok_or_else(|| {
                    anyhow::anyhow!("invalid pane_index {}; cannot split!", pane_index)
                })?;

            let tab_size = self.size;
            if split_info.first.rows == 0
                || split_info.first.cols == 0
                || split_info.second.rows == 0
                || split_info.second.cols == 0
                || split_info.top_of_second() + split_info.second.rows > tab_size.rows
                || split_info.left_of_second() + split_info.second.cols > tab_size.cols
            {
                log::error!(
                    "No space for split!!! {:#?} height={} width={} top_of_second={} left_of_second={} tab_size={:?}",
                    split_info,
                    split_info.height(),
                    split_info.width(),
                    split_info.top_of_second(),
                    split_info.left_of_second(),
                    tab_size
                );
                anyhow::bail!("No space for split!");
            }

            let needs_resize = if request.top_level {
                self.pane.as_ref().unwrap().num_leaves() > 1
            } else {
                false
            };

            if needs_resize {
                // Pre-emptively resize the tab contents down to
                // match the target size; it's easier to reuse
                // existing resize logic that way
                if request.target_is_second {
                    self.resize(split_info.first.clone());
                } else {
                    self.resize(split_info.second.clone());
                }
            }

            let mut cursor = self.pane.take().unwrap().cursor();

            if request.top_level && !cursor.is_leaf() {
                let result = if request.target_is_second {
                    cursor.split_node_and_insert_right(Arc::clone(&pane))
                } else {
                    cursor.split_node_and_insert_left(Arc::clone(&pane))
                };
                cursor = match result {
                    Ok(c) => {
                        cursor = match c.assign_node(Some(split_info)) {
                            Err(c) | Ok(c) => c,
                        };

                        self.pane.replace(cursor.tree());

                        let pane_index = if request.target_is_second {
                            self.pane.as_ref().unwrap().num_leaves().saturating_sub(1)
                        } else {
                            0
                        };

                        self.active = pane_index;
                        self.recency.tag(pane_index);
                        return Ok(pane_index);
                    }
                    Err(cursor) => cursor,
                };
            }

            match cursor.go_to_nth_leaf(pane_index) {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    anyhow::bail!("invalid pane_index {}; cannot split!", pane_index);
                }
            };

            let existing_pane = Arc::clone(cursor.leaf_mut().unwrap());

            let (pane1, pane2) = if request.target_is_second {
                (existing_pane, pane)
            } else {
                (pane, existing_pane)
            };

            pane1.resize(split_info.first)?;
            pane2.resize(split_info.second.clone())?;

            *cursor.leaf_mut().unwrap() = pane1;

            match cursor.split_leaf_and_insert_right(pane2) {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    anyhow::bail!("invalid pane_index {}; cannot split!", pane_index);
                }
            };

            // cursor now points to the newly created split node;
            // we need to populate its split information
            match cursor.assign_node(Some(split_info)) {
                Err(c) | Ok(c) => self.pane.replace(c.tree()),
            };

            if request.target_is_second {
                self.active = pane_index + 1;
                self.recency.tag(pane_index + 1);
            }
        }

        log::debug!("split info after split: {:#?}", self.iter_splits());
        log::debug!("pane info after split: {:#?}", self.iter_panes());

        Ok(if request.target_is_second {
            pane_index + 1
        } else {
            pane_index
        })
    }

    fn get_zoomed_pane(&self) -> Option<Arc<dyn Pane>> {
        self.zoomed.clone()
    }
}

/// This type is used directly by the codec, take care to bump
/// the codec version if you change this
#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub enum PaneNode {
    Empty,
    Split {
        left: Box<PaneNode>,
        right: Box<PaneNode>,
        node: SplitDirectionAndSize,
    },
    Leaf(PaneEntry),
}

impl PaneNode {
    pub fn into_tree(self) -> bintree::Tree<PaneEntry, SplitDirectionAndSize> {
        match self {
            PaneNode::Empty => bintree::Tree::Empty,
            PaneNode::Split { left, right, node } => bintree::Tree::Node {
                left: Box::new((*left).into_tree()),
                right: Box::new((*right).into_tree()),
                data: Some(node),
            },
            PaneNode::Leaf(e) => bintree::Tree::Leaf(e),
        }
    }

    pub fn root_size(&self) -> Option<TerminalSize> {
        match self {
            PaneNode::Empty => None,
            PaneNode::Split { node, .. } => Some(node.size()),
            PaneNode::Leaf(entry) => Some(entry.size),
        }
    }

    pub fn window_and_tab_ids(&self) -> Option<(WindowId, TabId)> {
        match self {
            PaneNode::Empty => None,
            PaneNode::Split { left, right, .. } => match left.window_and_tab_ids() {
                Some(res) => Some(res),
                None => right.window_and_tab_ids(),
            },
            PaneNode::Leaf(entry) => Some((entry.window_id, entry.tab_id)),
        }
    }
}

/// This type is used directly by the codec, take care to bump
/// the codec version if you change this
#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct PaneEntry {
    pub window_id: WindowId,
    pub tab_id: TabId,
    pub pane_id: PaneId,
    pub title: String,
    pub size: TerminalSize,
    pub working_dir: Option<SerdeUrl>,
    pub is_active_pane: bool,
    pub is_zoomed_pane: bool,
    pub workspace: String,
    pub cursor_pos: StableCursorPosition,
    pub physical_top: StableRowIndex,
    pub top_row: usize,
    pub left_col: usize,
    pub tty_name: Option<String>,
}

#[derive(Deserialize, Clone, Serialize, PartialEq, Debug)]
#[serde(try_from = "String", into = "String")]
pub struct SerdeUrl {
    pub url: Url,
}

impl std::convert::TryFrom<String> for SerdeUrl {
    type Error = url::ParseError;
    fn try_from(s: String) -> Result<SerdeUrl, url::ParseError> {
        let url = Url::parse(&s)?;
        Ok(SerdeUrl { url })
    }
}

impl From<Url> for SerdeUrl {
    fn from(url: Url) -> SerdeUrl {
        SerdeUrl { url }
    }
}

impl Into<Url> for SerdeUrl {
    fn into(self) -> Url {
        self.url
    }
}

impl Into<String> for SerdeUrl {
    fn into(self) -> String {
        self.url.as_str().into()
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::renderable::*;
    use parking_lot::{MappedMutexGuard, Mutex};
    use rangeset::RangeSet;
    use std::ops::Range;
    use termwiz::surface::SequenceNo;
    use url::Url;
    use wezterm_term::color::ColorPalette;
    use wezterm_term::{KeyCode, KeyModifiers, Line, MouseEvent, StableRowIndex};

    struct FakePane {
        id: PaneId,
        size: Mutex<TerminalSize>,
    }

    impl FakePane {
        fn new(id: PaneId, size: TerminalSize) -> Arc<dyn Pane> {
            Arc::new(Self {
                id,
                size: Mutex::new(size),
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
            _: SequenceNo,
        ) -> RangeSet<StableRowIndex> {
            unimplemented!();
        }

        fn with_lines_mut(
            &self,
            _stable_range: Range<StableRowIndex>,
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

        fn get_lines(&self, _lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
            unimplemented!();
        }

        fn get_logical_lines(&self, _lines: Range<StableRowIndex>) -> Vec<LogicalLine> {
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
            unimplemented!()
        }
        fn send_paste(&self, _text: &str) -> anyhow::Result<()> {
            unimplemented!()
        }
        fn reader(&self) -> anyhow::Result<Option<Box<dyn std::io::Read + Send>>> {
            Ok(None)
        }
        fn writer(&self) -> MappedMutexGuard<dyn std::io::Write> {
            unimplemented!()
        }
        fn resize(&self, size: TerminalSize) -> anyhow::Result<()> {
            *self.size.lock() = size;
            Ok(())
        }

        fn key_down(&self, _key: KeyCode, _mods: KeyModifiers) -> anyhow::Result<()> {
            unimplemented!()
        }
        fn key_up(&self, _: KeyCode, _: KeyModifiers) -> anyhow::Result<()> {
            unimplemented!()
        }
        fn mouse_event(&self, _event: MouseEvent) -> anyhow::Result<()> {
            unimplemented!()
        }
        fn is_dead(&self) -> bool {
            false
        }
        fn palette(&self) -> ColorPalette {
            unimplemented!()
        }
        fn domain_id(&self) -> DomainId {
            1
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

    #[test]
    fn tab_splitting() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };

        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(1, size));

        let panes = tab.iter_panes();
        assert_eq!(1, panes.len());
        assert_eq!(0, panes[0].index);
        assert_eq!(true, panes[0].is_active);
        assert_eq!(0, panes[0].left);
        assert_eq!(0, panes[0].top);
        assert_eq!(80, panes[0].width);
        assert_eq!(24, panes[0].height);

        assert!(tab
            .compute_split_size(
                1,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                }
            )
            .is_none());

        let horz_size = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            horz_size,
            SplitDirectionAndSize {
                direction: SplitDirection::Horizontal,
                second: TerminalSize {
                    rows: 24,
                    cols: 40,
                    pixel_width: 400,
                    pixel_height: 600,
                    dpi: 96,
                },
                first: TerminalSize {
                    rows: 24,
                    cols: 39,
                    pixel_width: 390,
                    pixel_height: 600,
                    dpi: 96,
                },
            }
        );

        let vert_size = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Vertical,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            vert_size,
            SplitDirectionAndSize {
                direction: SplitDirection::Vertical,
                second: TerminalSize {
                    rows: 12,
                    cols: 80,
                    pixel_width: 800,
                    pixel_height: 300,
                    dpi: 96,
                },
                first: TerminalSize {
                    rows: 11,
                    cols: 80,
                    pixel_width: 800,
                    pixel_height: 275,
                    dpi: 96,
                }
            }
        );

        let new_index = tab
            .split_and_insert(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
                FakePane::new(2, horz_size.second),
            )
            .unwrap();
        assert_eq!(new_index, 1);

        let panes = tab.iter_panes();
        assert_eq!(2, panes.len());

        assert_eq!(0, panes[0].index);
        assert_eq!(false, panes[0].is_active);
        assert_eq!(0, panes[0].left);
        assert_eq!(0, panes[0].top);
        assert_eq!(39, panes[0].width);
        assert_eq!(24, panes[0].height);
        assert_eq!(390, panes[0].pixel_width);
        assert_eq!(600, panes[0].pixel_height);
        assert_eq!(1, panes[0].pane.pane_id());

        assert_eq!(1, panes[1].index);
        assert_eq!(true, panes[1].is_active);
        assert_eq!(40, panes[1].left);
        assert_eq!(0, panes[1].top);
        assert_eq!(40, panes[1].width);
        assert_eq!(24, panes[1].height);
        assert_eq!(400, panes[1].pixel_width);
        assert_eq!(600, panes[1].pixel_height);
        assert_eq!(2, panes[1].pane.pane_id());

        let vert_size = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Vertical,
                    ..Default::default()
                },
            )
            .unwrap();
        let new_index = tab
            .split_and_insert(
                0,
                SplitRequest {
                    direction: SplitDirection::Vertical,
                    top_level: false,
                    target_is_second: true,
                    size: Default::default(),
                },
                FakePane::new(3, vert_size.second),
            )
            .unwrap();
        assert_eq!(new_index, 1);

        let panes = tab.iter_panes();
        assert_eq!(3, panes.len());

        assert_eq!(0, panes[0].index);
        assert_eq!(false, panes[0].is_active);
        assert_eq!(0, panes[0].left);
        assert_eq!(0, panes[0].top);
        assert_eq!(39, panes[0].width);
        assert_eq!(11, panes[0].height);
        assert_eq!(390, panes[0].pixel_width);
        assert_eq!(275, panes[0].pixel_height);
        assert_eq!(1, panes[0].pane.pane_id());

        assert_eq!(1, panes[1].index);
        assert_eq!(true, panes[1].is_active);
        assert_eq!(0, panes[1].left);
        assert_eq!(12, panes[1].top);
        assert_eq!(39, panes[1].width);
        assert_eq!(12, panes[1].height);
        assert_eq!(390, panes[1].pixel_width);
        assert_eq!(300, panes[1].pixel_height);
        assert_eq!(3, panes[1].pane.pane_id());

        assert_eq!(2, panes[2].index);
        assert_eq!(false, panes[2].is_active);
        assert_eq!(40, panes[2].left);
        assert_eq!(0, panes[2].top);
        assert_eq!(40, panes[2].width);
        assert_eq!(24, panes[2].height);
        assert_eq!(400, panes[2].pixel_width);
        assert_eq!(600, panes[2].pixel_height);
        assert_eq!(2, panes[2].pane.pane_id());

        tab.resize_split_by(1, 1);
        let panes = tab.iter_panes();
        assert_eq!(39, panes[0].width);
        assert_eq!(12, panes[0].height);
        assert_eq!(390, panes[0].pixel_width);
        assert_eq!(300, panes[0].pixel_height);

        assert_eq!(39, panes[1].width);
        assert_eq!(11, panes[1].height);
        assert_eq!(390, panes[1].pixel_width);
        assert_eq!(275, panes[1].pixel_height);

        assert_eq!(40, panes[2].width);
        assert_eq!(24, panes[2].height);
        assert_eq!(400, panes[2].pixel_width);
        assert_eq!(600, panes[2].pixel_height);
    }

    fn check_tree_invariants(tree: &Tree, allocated: &TerminalSize) -> Vec<String> {
        let mut errors = Vec::new();
        match tree {
            Tree::Empty | Tree::Leaf(_) => {}
            Tree::Node { data: None, .. } => {}
            Tree::Node {
                left,
                right,
                data: Some(data),
            } => {
                match data.direction {
                    SplitDirection::Horizontal => {
                        // Both children must have the same height as allocated
                        if data.first.rows != allocated.rows {
                            errors.push(format!(
                                "H-split first.rows={} != allocated.rows={}",
                                data.first.rows, allocated.rows
                            ));
                        }
                        if data.second.rows != allocated.rows {
                            errors.push(format!(
                                "H-split second.rows={} != allocated.rows={}",
                                data.second.rows, allocated.rows
                            ));
                        }
                        // Widths should sum: first.cols + 1 + second.cols == allocated.cols
                        let total_cols = data.first.cols + 1 + data.second.cols;
                        if total_cols != allocated.cols {
                            errors.push(format!(
                                "H-split cols: {} + 1 + {} = {} != allocated.cols={}",
                                data.first.cols, data.second.cols, total_cols, allocated.cols
                            ));
                        }
                    }
                    SplitDirection::Vertical => {
                        // Both children must have the same width as allocated
                        if data.first.cols != allocated.cols {
                            errors.push(format!(
                                "V-split first.cols={} != allocated.cols={}",
                                data.first.cols, allocated.cols
                            ));
                        }
                        if data.second.cols != allocated.cols {
                            errors.push(format!(
                                "V-split second.cols={} != allocated.cols={}",
                                data.second.cols, allocated.cols
                            ));
                        }
                        // Rows should sum: first.rows + 1 + second.rows == allocated.rows
                        let total_rows = data.first.rows + 1 + data.second.rows;
                        if total_rows != allocated.rows {
                            errors.push(format!(
                                "V-split rows: {} + 1 + {} = {} != allocated.rows={}",
                                data.first.rows, data.second.rows, total_rows, allocated.rows
                            ));
                        }
                    }
                }
                errors.extend(check_tree_invariants(left, &data.first));
                errors.extend(check_tree_invariants(right, &data.second));
            }
        }
        errors
    }

    /// Build the L-shaped 3-pane layout from the bug report:
    ///
    /// ```text
    /// +----------+----------+
    /// |          |  pane 1  |
    /// |  pane 0  +----------+
    /// |          |  pane 2  |
    /// +----------+----------+
    /// ```
    ///
    /// Returns (tab, pane0, pane1, pane2) so callers can resize
    /// individual panes to simulate mux server behavior.
    fn make_l_shaped_tab(
        size: TerminalSize,
    ) -> (Tab, Arc<dyn Pane>, Arc<dyn Pane>, Arc<dyn Pane>) {
        let tab = Tab::new(&size);
        let pane0 = FakePane::new(0, size);
        tab.assign_pane(&pane0);

        // Horizontal split: pane0 (left), pane1 (right)
        let hsplit = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
            )
            .unwrap();
        let pane1 = FakePane::new(1, hsplit.second);
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                ..Default::default()
            },
            pane1.clone(),
        )
        .unwrap();

        // Vertical sub-split on the right pane (pane1 index=1):
        // pane1 (top-right), pane2 (bottom-right)
        let vsplit = tab
            .compute_split_size(
                1,
                SplitRequest {
                    direction: SplitDirection::Vertical,
                    ..Default::default()
                },
            )
            .unwrap();
        let pane2 = FakePane::new(2, vsplit.second);
        tab.split_and_insert(
            1,
            SplitRequest {
                direction: SplitDirection::Vertical,
                ..Default::default()
            },
            pane2.clone(),
        )
        .unwrap();

        (tab, pane0, pane1, pane2)
    }

    /// Baseline: the normal single-client path (create layout → drag
    /// divider → resize window → resize back) never breaks the invariant.
    /// This is a regression guard — if this fails, the core resize logic
    /// itself is broken, not just the mux interleaving path.
    #[test]
    fn nested_split_normal_resize_preserves_invariants() {
        let size = TerminalSize {
            rows: 80,
            cols: 160,
            pixel_width: 1600,
            pixel_height: 2000,
            dpi: 96,
        };
        let (tab, _pane0, _pane1, _pane2) = make_l_shaped_tab(size);

        let check = |label: &str, tab: &Tab| {
            let inner = tab.inner.lock();
            let errors = check_tree_invariants(inner.pane.as_ref().unwrap(), &inner.size);
            assert!(errors.is_empty(), "{}: {:?}", label, errors);
        };

        check("after creation", &tab);

        tab.resize_split_by(1, 10);
        check("after divider drag", &tab);

        let bigger = TerminalSize {
            rows: 90,
            cols: 170,
            pixel_width: 1700,
            pixel_height: 2250,
            dpi: 96,
        };
        tab.resize(bigger);
        check("after resize up", &tab);

        tab.resize(size);
        check("after resize back", &tab);
    }

    /// Simulate the mux server state after interleaved per-pane resize PDUs
    /// from two rapid resize events.
    ///
    /// When a client resizes its window, `apply_sizes_from_splits` calls
    /// `pane.resize()` on each leaf, spawning independent async `Pdu::Resize`
    /// tasks. If two resize events fire in quick succession, their PDUs can
    /// interleave — leaving some panes at sizes from event 1 and others from
    /// event 2.
    ///
    /// Returns (tab, pane0, pane1, pane2) with panes already set to the
    /// inconsistent interleaved sizes.
    fn make_interleaved_resize_state(
    ) -> (Tab, Arc<dyn Pane>, Arc<dyn Pane>, Arc<dyn Pane>) {
        let size = TerminalSize {
            rows: 80,
            cols: 160,
            pixel_width: 1600,
            pixel_height: 2000,
            dpi: 96,
        };
        let (tab, pane0, pane1, pane2) = make_l_shaped_tab(size);

        // Drag the vertical divider to create asymmetric right-side split
        tab.resize_split_by(1, 10);

        let panes_before = tab.iter_panes();
        let p0_rows = panes_before[0].height;
        let p1_rows = panes_before[1].height;
        let p2_rows = panes_before[2].height;

        // Verify precondition: right column sums correctly
        assert_eq!(
            p1_rows + 1 + p2_rows,
            p0_rows,
            "precondition: right column should sum to left pane height"
        );

        // Simulate two rapid resize events on the CLIENT side.
        // We create temporary client-side tabs to compute what sizes
        // each event would produce.

        // Event 1: 80→90 rows
        let size_e1 = TerminalSize {
            rows: 90,
            cols: 160,
            pixel_width: 1600,
            pixel_height: 2250,
            dpi: 96,
        };
        let (client_tab_e1, _, _, _) = make_l_shaped_tab(size);
        client_tab_e1.resize_split_by(1, 10);
        client_tab_e1.resize(size_e1);
        let e1_panes = client_tab_e1.iter_panes();

        // Event 2: 80→90→95 rows
        let size_e2 = TerminalSize {
            rows: 95,
            cols: 160,
            pixel_width: 1600,
            pixel_height: 2375,
            dpi: 96,
        };
        let (client_tab_e2, _, _, _) = make_l_shaped_tab(size);
        client_tab_e2.resize_split_by(1, 10);
        client_tab_e2.resize(size_e1);
        client_tab_e2.resize(size_e2);
        let e2_panes = client_tab_e2.iter_panes();

        // Helper to extract TerminalSize from a PositionedPane
        let pane_size = |p: &PositionedPane| TerminalSize {
            rows: p.height,
            cols: p.width,
            pixel_width: p.pixel_width,
            pixel_height: p.pixel_height,
            dpi: 96,
        };

        // Apply the interleaved final state to the server's panes:
        // pane0 and pane1 got E2's sizes (latest), but pane2 got E1's
        // STALE size because its E1 PDU arrived after its E2 PDU.
        pane0.resize(pane_size(&e2_panes[0])).unwrap();
        pane1.resize(pane_size(&e2_panes[1])).unwrap();
        pane2.resize(pane_size(&e1_panes[2])).unwrap(); // stale!

        (tab, pane0, pane1, pane2)
    }

    /// Prove that interleaved per-pane resize PDUs break the size invariant.
    ///
    /// After two rapid client resize events whose PDUs interleave, the
    /// server's panes end up with sizes from different events. The right
    /// column's children (pane1 + divider + pane2) no longer sum to the
    /// left pane's height.
    #[test]
    fn interleaved_pdus_break_pane_size_invariant() {
        let (_tab, pane0, pane1, pane2) = make_interleaved_resize_state();

        let p0 = pane0.get_dimensions();
        let p1 = pane1.get_dimensions();
        let p2 = pane2.get_dimensions();

        let left_rows = p0.viewport_rows;
        let right_total = p1.viewport_rows + 1 + p2.viewport_rows;

        assert_ne!(
            right_total, left_rows,
            "Raw pane sizes should be inconsistent after interleaving. \
             left={}, top_right={}, bot_right={}, right_total={}",
            left_rows,
            p1.viewport_rows,
            p2.viewport_rows,
            right_total,
        );
    }

    /// Prove that rebuild_splits_sizes_from_contained_panes (with
    /// reconcile_tree_sizes) restores the tree invariant even when
    /// panes report inconsistent sizes from interleaved PDUs.
    #[test]
    fn reconcile_fixes_interleaved_pdu_overflow() {
        let (tab, _pane0, _pane1, _pane2) = make_interleaved_resize_state();

        tab.rebuild_splits_sizes_from_contained_panes();

        let inner = tab.inner.lock();
        let errors = check_tree_invariants(inner.pane.as_ref().unwrap(), &inner.size);
        assert!(
            errors.is_empty(),
            "Tree invariants should hold after reconciliation, but got: {:?}",
            errors,
        );
    }

    /// Build a 4-pane layout with 3 panes stacked in the right column:
    ///
    /// ```text
    /// +---------+---------+
    /// |         |  pane 1 |
    /// |         +---------+
    /// | pane 0  |  pane 2 |
    /// |         +---------+
    /// |         |  pane 3 |
    /// +---------+---------+
    /// ```
    fn make_deep_nested_tab(
        size: TerminalSize,
    ) -> (Tab, Arc<dyn Pane>, Arc<dyn Pane>, Arc<dyn Pane>, Arc<dyn Pane>) {
        let tab = Tab::new(&size);
        let pane0 = FakePane::new(0, size);
        tab.assign_pane(&pane0);

        // Horizontal split: pane0 (left), pane1 (right)
        let hsplit = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
            )
            .unwrap();
        let pane1 = FakePane::new(1, hsplit.second);
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                ..Default::default()
            },
            pane1.clone(),
        )
        .unwrap();

        // First vertical sub-split on the right: pane1 (top), pane2 (middle)
        let vsplit1 = tab
            .compute_split_size(
                1,
                SplitRequest {
                    direction: SplitDirection::Vertical,
                    ..Default::default()
                },
            )
            .unwrap();
        let pane2 = FakePane::new(2, vsplit1.second);
        tab.split_and_insert(
            1,
            SplitRequest {
                direction: SplitDirection::Vertical,
                ..Default::default()
            },
            pane2.clone(),
        )
        .unwrap();

        // Second vertical sub-split: pane2 (middle), pane3 (bottom)
        let vsplit2 = tab
            .compute_split_size(
                2,
                SplitRequest {
                    direction: SplitDirection::Vertical,
                    ..Default::default()
                },
            )
            .unwrap();
        let pane3 = FakePane::new(3, vsplit2.second);
        tab.split_and_insert(
            2,
            SplitRequest {
                direction: SplitDirection::Vertical,
                ..Default::default()
            },
            pane3.clone(),
        )
        .unwrap();

        (tab, pane0, pane1, pane2, pane3)
    }

    /// Pattern 2: interleaved PDUs cause column width inconsistency.
    /// Panes in the same vertical column end up with different widths
    /// because their col-count PDUs came from different resize events.
    #[test]
    fn interleaved_pdus_break_column_width() {
        let size = TerminalSize {
            rows: 80,
            cols: 160,
            pixel_width: 1600,
            pixel_height: 2000,
            dpi: 96,
        };
        let (tab, pane0, pane1, pane2) = make_l_shaped_tab(size);
        tab.resize_split_by(1, 10);

        // Event 1: resize cols 160→180
        let size_e1 = TerminalSize {
            rows: 80,
            cols: 180,
            pixel_width: 1800,
            pixel_height: 2000,
            dpi: 96,
        };
        let (c1, _, _, _) = make_l_shaped_tab(size);
        c1.resize_split_by(1, 10);
        c1.resize(size_e1);
        let e1 = c1.iter_panes();

        // Event 2: resize cols 160→180→200
        let size_e2 = TerminalSize {
            rows: 80,
            cols: 200,
            pixel_width: 2000,
            pixel_height: 2000,
            dpi: 96,
        };
        let (c2, _, _, _) = make_l_shaped_tab(size);
        c2.resize_split_by(1, 10);
        c2.resize(size_e1);
        c2.resize(size_e2);
        let e2 = c2.iter_panes();

        let ps = |p: &PositionedPane| TerminalSize {
            rows: p.height,
            cols: p.width,
            pixel_width: p.pixel_width,
            pixel_height: p.pixel_height,
            dpi: 96,
        };

        // Interleave: pane0 from E2, pane1 from E2, pane2 from E1 (stale cols)
        pane0.resize(ps(&e2[0])).unwrap();
        pane1.resize(ps(&e2[1])).unwrap();
        pane2.resize(ps(&e1[2])).unwrap(); // stale!

        // Prove the width inconsistency at the pane level
        let p1_cols = pane1.get_dimensions().cols;
        let p2_cols = pane2.get_dimensions().cols;
        assert_ne!(
            p1_cols, p2_cols,
            "Panes in same vertical column should have inconsistent widths. \
             pane1.cols={}, pane2.cols={}",
            p1_cols, p2_cols,
        );

        // Prove reconciliation fixes it
        tab.rebuild_splits_sizes_from_contained_panes();
        let inner = tab.inner.lock();
        let errors = check_tree_invariants(inner.pane.as_ref().unwrap(), &inner.size);
        assert!(
            errors.is_empty(),
            "Tree invariants should hold after reconciliation, but got: {:?}",
            errors,
        );
    }

    /// Pattern 3: deeply nested layout (4 panes, 3 stacked in right column).
    /// Interleaved PDUs can cause multi-level inconsistencies that a single
    /// reconciliation pass must fix through all nesting levels.
    #[test]
    fn deep_nested_interleaved_pdus() {
        let size = TerminalSize {
            rows: 90,
            cols: 160,
            pixel_width: 1600,
            pixel_height: 2250,
            dpi: 96,
        };
        let (tab, pane0, pane1, pane2, pane3) = make_deep_nested_tab(size);

        // Make it asymmetric
        tab.resize_split_by(1, 5);
        tab.resize_split_by(2, 8);

        // Event 1: 90→100 rows
        let size_e1 = TerminalSize {
            rows: 100,
            cols: 160,
            pixel_width: 1600,
            pixel_height: 2500,
            dpi: 96,
        };
        let (c1, _, _, _, _) = make_deep_nested_tab(size);
        c1.resize_split_by(1, 5);
        c1.resize_split_by(2, 8);
        c1.resize(size_e1);
        let e1 = c1.iter_panes();

        // Event 2: 90→100→110 rows
        let size_e2 = TerminalSize {
            rows: 110,
            cols: 160,
            pixel_width: 1600,
            pixel_height: 2750,
            dpi: 96,
        };
        let (c2, _, _, _, _) = make_deep_nested_tab(size);
        c2.resize_split_by(1, 5);
        c2.resize_split_by(2, 8);
        c2.resize(size_e1);
        c2.resize(size_e2);
        let e2 = c2.iter_panes();

        let ps = |p: &PositionedPane| TerminalSize {
            rows: p.height,
            cols: p.width,
            pixel_width: p.pixel_width,
            pixel_height: p.pixel_height,
            dpi: 96,
        };

        // Interleave: pane0+pane1 from E2, pane2+pane3 from E1 (stale)
        pane0.resize(ps(&e2[0])).unwrap();
        pane1.resize(ps(&e2[1])).unwrap();
        pane2.resize(ps(&e1[2])).unwrap(); // stale
        pane3.resize(ps(&e1[3])).unwrap(); // stale

        // Prove the invariant is broken at the pane level
        let p0 = pane0.get_dimensions();
        let p1 = pane1.get_dimensions();
        let p2 = pane2.get_dimensions();
        let p3 = pane3.get_dimensions();
        let right_total = p1.viewport_rows + 1 + p2.viewport_rows + 1 + p3.viewport_rows;
        assert_ne!(
            right_total,
            p0.viewport_rows,
            "Right column should be inconsistent. left={}, right_total={}",
            p0.viewport_rows,
            right_total,
        );

        // Prove reconciliation fixes the deep nesting
        tab.rebuild_splits_sizes_from_contained_panes();
        let inner = tab.inner.lock();
        let errors = check_tree_invariants(inner.pane.as_ref().unwrap(), &inner.size);
        assert!(
            errors.is_empty(),
            "Deep nested tree invariants should hold after reconciliation, but got: {:?}",
            errors,
        );
    }

    /// Build a T-shaped layout: top-level vertical split with a horizontal
    /// sub-split on top and a full-width pane on the bottom:
    ///
    /// ```text
    /// +----------+----------+
    /// |  pane 0  |  pane 1  |
    /// +----------+----------+
    /// |       pane 2        |
    /// +---------------------+
    /// ```
    fn make_t_shaped_tab(
        size: TerminalSize,
    ) -> (Tab, Arc<dyn Pane>, Arc<dyn Pane>, Arc<dyn Pane>) {
        let tab = Tab::new(&size);
        let pane0 = FakePane::new(0, size);
        tab.assign_pane(&pane0);

        // Vertical split: pane0 (top), pane2 (bottom)
        let vsplit = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Vertical,
                    ..Default::default()
                },
            )
            .unwrap();
        let pane2 = FakePane::new(2, vsplit.second);
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Vertical,
                ..Default::default()
            },
            pane2.clone(),
        )
        .unwrap();

        // Horizontal sub-split on the top pane (pane0): pane0 (left), pane1 (right)
        let hsplit = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
            )
            .unwrap();
        let pane1 = FakePane::new(1, hsplit.second);
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                ..Default::default()
            },
            pane1.clone(),
        )
        .unwrap();

        (tab, pane0, pane1, pane2)
    }

    /// Pattern 4: T-shaped layout (vertical split with horizontal sub-split
    /// on top, full-width pane on bottom). This is the "inverted L" — the
    /// bottom pane spans both columns.
    ///
    /// Tests that reconcile_tree_sizes correctly handles the case where
    /// the vertical split's children have different widths because the
    /// top side is an H-split (width = left + 1 + right) while the bottom
    /// is a single pane (width = allocated).
    #[test]
    fn t_shaped_interleaved_pdus() {
        let size = TerminalSize {
            rows: 80,
            cols: 160,
            pixel_width: 1600,
            pixel_height: 2000,
            dpi: 96,
        };
        let (tab, pane0, pane1, pane2) = make_t_shaped_tab(size);

        // Make dividers asymmetric
        tab.resize_split_by(0, 5); // vertical divider
        tab.resize_split_by(1, 8); // horizontal divider in top half

        let check = |label: &str, tab: &Tab| {
            let inner = tab.inner.lock();
            let errors = check_tree_invariants(inner.pane.as_ref().unwrap(), &inner.size);
            assert!(errors.is_empty(), "{}: {:?}", label, errors);
        };

        check("t-shape after creation + divider drag", &tab);

        // Event 1: resize rows 80→90
        let size_e1 = TerminalSize {
            rows: 90,
            cols: 160,
            pixel_width: 1600,
            pixel_height: 2250,
            dpi: 96,
        };
        let (c1, _, _, _) = make_t_shaped_tab(size);
        c1.resize_split_by(0, 5);
        c1.resize_split_by(1, 8);
        c1.resize(size_e1);
        let e1 = c1.iter_panes();

        // Event 2: resize rows 80→90→100
        let size_e2 = TerminalSize {
            rows: 100,
            cols: 160,
            pixel_width: 1600,
            pixel_height: 2500,
            dpi: 96,
        };
        let (c2, _, _, _) = make_t_shaped_tab(size);
        c2.resize_split_by(0, 5);
        c2.resize_split_by(1, 8);
        c2.resize(size_e1);
        c2.resize(size_e2);
        let e2 = c2.iter_panes();

        let ps = |p: &PositionedPane| TerminalSize {
            rows: p.height,
            cols: p.width,
            pixel_width: p.pixel_width,
            pixel_height: p.pixel_height,
            dpi: 96,
        };

        // Interleave WITHIN the H-sub-split: pane0 from E2, pane1 from E1
        // (stale rows), pane2 from E2. This breaks the H-split constraint
        // that first.rows == second.rows.
        pane0.resize(ps(&e2[0])).unwrap();
        pane1.resize(ps(&e1[1])).unwrap(); // stale: different row count
        pane2.resize(ps(&e2[2])).unwrap();

        // Prove the inconsistency: pane0 (E2) and pane1 (E1) have different
        // row counts, violating the H-split constraint.
        let p0 = pane0.get_dimensions();
        let p1 = pane1.get_dimensions();
        assert_ne!(
            p0.viewport_rows, p1.viewport_rows,
            "Top-left (E2) and top-right (E1) should have different heights. \
             pane0.rows={}, pane1.rows={}",
            p0.viewport_rows,
            p1.viewport_rows,
        );

        // Prove reconciliation fixes it
        tab.rebuild_splits_sizes_from_contained_panes();
        let inner = tab.inner.lock();
        let errors = check_tree_invariants(inner.pane.as_ref().unwrap(), &inner.size);
        assert!(
            errors.is_empty(),
            "T-shaped tree invariants should hold after reconciliation, but got: {:?}",
            errors,
        );
    }

    /// Verify that normal resize of all layout shapes preserves invariants.
    /// This is a regression guard for all layout helpers.
    #[test]
    fn all_layouts_resize_preserves_invariants() {
        let size = TerminalSize {
            rows: 80,
            cols: 160,
            pixel_width: 1600,
            pixel_height: 2000,
            dpi: 96,
        };
        let bigger = TerminalSize {
            rows: 100,
            cols: 200,
            pixel_width: 2000,
            pixel_height: 2500,
            dpi: 96,
        };

        let check = |label: &str, tab: &Tab| {
            let inner = tab.inner.lock();
            let errors = check_tree_invariants(inner.pane.as_ref().unwrap(), &inner.size);
            assert!(errors.is_empty(), "{}: {:?}", label, errors);
        };

        // L-shaped
        let (tab, _, _, _) = make_l_shaped_tab(size);
        tab.resize_split_by(1, 10);
        tab.resize(bigger);
        tab.resize(size);
        check("L-shape resize cycle", &tab);

        // T-shaped
        let (tab, _, _, _) = make_t_shaped_tab(size);
        tab.resize_split_by(0, 5);
        tab.resize_split_by(1, 8);
        tab.resize(bigger);
        tab.resize(size);
        check("T-shape resize cycle", &tab);

        // Deep nested (4-pane)
        let (tab, _, _, _, _) = make_deep_nested_tab(size);
        tab.resize_split_by(1, 5);
        tab.resize_split_by(2, 8);
        tab.resize(bigger);
        tab.resize(size);
        check("deep nested resize cycle", &tab);

        // 2x2 grid
        let (tab, _, _, _, _) = make_grid_tab(size);
        tab.resize_split_by(1, 7);
        tab.resize_split_by(2, -5);
        tab.resize(bigger);
        tab.resize(size);
        check("grid resize cycle", &tab);
    }

    /// Build a 2x2 grid: horizontal split, each side has a vertical sub-split.
    ///
    /// ```text
    /// +---------+---------+
    /// | pane 0  | pane 1  |
    /// +---------+---------+
    /// | pane 2  | pane 3  |
    /// +---------+---------+
    /// ```
    fn make_grid_tab(
        size: TerminalSize,
    ) -> (Tab, Arc<dyn Pane>, Arc<dyn Pane>, Arc<dyn Pane>, Arc<dyn Pane>) {
        let tab = Tab::new(&size);
        let pane0 = FakePane::new(0, size);
        tab.assign_pane(&pane0);

        // Horizontal split: left | right
        let hsplit = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
            )
            .unwrap();
        let pane1 = FakePane::new(1, hsplit.second);
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                ..Default::default()
            },
            pane1.clone(),
        )
        .unwrap();

        // Vertical sub-split on the LEFT: pane0 (top-left), pane2 (bottom-left)
        let vsplit_l = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Vertical,
                    ..Default::default()
                },
            )
            .unwrap();
        let pane2 = FakePane::new(2, vsplit_l.second);
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Vertical,
                ..Default::default()
            },
            pane2.clone(),
        )
        .unwrap();

        // Vertical sub-split on the RIGHT: pane1 (top-right), pane3 (bottom-right)
        let vsplit_r = tab
            .compute_split_size(
                2,
                SplitRequest {
                    direction: SplitDirection::Vertical,
                    ..Default::default()
                },
            )
            .unwrap();
        let pane3 = FakePane::new(3, vsplit_r.second);
        tab.split_and_insert(
            2,
            SplitRequest {
                direction: SplitDirection::Vertical,
                ..Default::default()
            },
            pane3.clone(),
        )
        .unwrap();

        (tab, pane0, pane1, pane2, pane3)
    }

    /// 2x2 grid with interleaved PDUs: left column from E2, right column
    /// from E1. Both vertical sub-splits get stale/fresh data, and the
    /// horizontal split's children may have different total heights.
    #[test]
    fn grid_interleaved_pdus() {
        let size = TerminalSize {
            rows: 80,
            cols: 160,
            pixel_width: 1600,
            pixel_height: 2000,
            dpi: 96,
        };
        let (tab, pane0, pane1, pane2, pane3) = make_grid_tab(size);

        // Make both V-splits asymmetric
        tab.resize_split_by(1, 7);
        tab.resize_split_by(2, -5);

        // Event 1: 80→90 rows
        let size_e1 = TerminalSize {
            rows: 90,
            cols: 160,
            pixel_width: 1600,
            pixel_height: 2250,
            dpi: 96,
        };
        let (c1, _, _, _, _) = make_grid_tab(size);
        c1.resize_split_by(1, 7);
        c1.resize_split_by(2, -5);
        c1.resize(size_e1);
        let e1 = c1.iter_panes();

        // Event 2: 80→90→100 rows
        let size_e2 = TerminalSize {
            rows: 100,
            cols: 160,
            pixel_width: 1600,
            pixel_height: 2500,
            dpi: 96,
        };
        let (c2, _, _, _, _) = make_grid_tab(size);
        c2.resize_split_by(1, 7);
        c2.resize_split_by(2, -5);
        c2.resize(size_e1);
        c2.resize(size_e2);
        let e2 = c2.iter_panes();

        let ps = |p: &PositionedPane| TerminalSize {
            rows: p.height,
            cols: p.width,
            pixel_width: p.pixel_width,
            pixel_height: p.pixel_height,
            dpi: 96,
        };

        // Interleave: left column (pane0, pane2) from E2,
        // right column (pane1, pane3) from E1 (stale)
        pane0.resize(ps(&e2[0])).unwrap();
        pane2.resize(ps(&e2[1])).unwrap();
        pane1.resize(ps(&e1[2])).unwrap(); // stale
        pane3.resize(ps(&e1[3])).unwrap(); // stale

        // Prove inconsistency: left and right columns have different
        // total heights because they're from different events
        let p0 = pane0.get_dimensions();
        let p2 = pane2.get_dimensions();
        let p1 = pane1.get_dimensions();
        let p3 = pane3.get_dimensions();
        let left_total = p0.viewport_rows + 1 + p2.viewport_rows;
        let right_total = p1.viewport_rows + 1 + p3.viewport_rows;
        assert_ne!(
            left_total, right_total,
            "Left and right columns should have different heights. \
             left={}, right={}",
            left_total, right_total,
        );

        // Prove reconciliation fixes it
        tab.rebuild_splits_sizes_from_contained_panes();
        let inner = tab.inner.lock();
        let errors = check_tree_invariants(inner.pane.as_ref().unwrap(), &inner.size);
        assert!(
            errors.is_empty(),
            "Grid tree invariants should hold after reconciliation, but got: {:?}",
            errors,
        );
    }

    /// Verify that interleaving the FIRST pane as stale (not last) also
    /// triggers and is fixed by reconciliation. Previous tests always made
    /// the last pane stale — this ensures the fix works regardless of
    /// which pane is out of date.
    #[test]
    fn first_pane_stale_interleaving() {
        let size = TerminalSize {
            rows: 80,
            cols: 160,
            pixel_width: 1600,
            pixel_height: 2000,
            dpi: 96,
        };
        let (tab, pane0, pane1, pane2) = make_l_shaped_tab(size);
        tab.resize_split_by(1, 10);

        // Event 1: 80→90
        let size_e1 = TerminalSize {
            rows: 90,
            cols: 160,
            pixel_width: 1600,
            pixel_height: 2250,
            dpi: 96,
        };
        let (c1, _, _, _) = make_l_shaped_tab(size);
        c1.resize_split_by(1, 10);
        c1.resize(size_e1);
        let e1 = c1.iter_panes();

        // Event 2: 80→90→100
        let size_e2 = TerminalSize {
            rows: 100,
            cols: 160,
            pixel_width: 1600,
            pixel_height: 2500,
            dpi: 96,
        };
        let (c2, _, _, _) = make_l_shaped_tab(size);
        c2.resize_split_by(1, 10);
        c2.resize(size_e1);
        c2.resize(size_e2);
        let e2 = c2.iter_panes();

        let ps = |p: &PositionedPane| TerminalSize {
            rows: p.height,
            cols: p.width,
            pixel_width: p.pixel_width,
            pixel_height: p.pixel_height,
            dpi: 96,
        };

        // Interleave: pane0 from E1 (STALE — first pane!),
        // pane1 and pane2 from E2
        pane0.resize(ps(&e1[0])).unwrap(); // stale!
        pane1.resize(ps(&e2[1])).unwrap();
        pane2.resize(ps(&e2[2])).unwrap();

        // Prove inconsistency: pane0 (left, from E1) has different height
        // than the right column (from E2)
        let p0 = pane0.get_dimensions();
        let p1 = pane1.get_dimensions();
        let p2 = pane2.get_dimensions();
        assert_ne!(
            p0.viewport_rows,
            p1.viewport_rows + 1 + p2.viewport_rows,
            "Left pane (E1) should differ from right column (E2). \
             left={}, right={}",
            p0.viewport_rows,
            p1.viewport_rows + 1 + p2.viewport_rows,
        );

        // Prove reconciliation fixes it
        tab.rebuild_splits_sizes_from_contained_panes();
        let inner = tab.inner.lock();
        let errors = check_tree_invariants(inner.pane.as_ref().unwrap(), &inner.size);
        assert!(
            errors.is_empty(),
            "First-pane-stale invariants should hold after reconciliation, but got: {:?}",
            errors,
        );
    }

    /// Verify that removing panes from various layouts preserves tree
    /// invariants. Tests the `remove_pane_if` → `apply_pane_size` cascade.
    #[test]
    fn pane_removal_preserves_invariants() {
        let size = TerminalSize {
            rows: 80,
            cols: 160,
            pixel_width: 1600,
            pixel_height: 2000,
            dpi: 96,
        };

        let check = |label: &str, tab: &Tab| {
            let inner = tab.inner.lock();
            if inner.pane.as_ref().map_or(true, |t| t.num_leaves() < 2) {
                return; // single pane or empty — no split to check
            }
            let errors = check_tree_invariants(inner.pane.as_ref().unwrap(), &inner.size);
            assert!(errors.is_empty(), "{}: {:?}", label, errors);
        };

        // L-shaped: remove bottom-right pane (pane2), should leave 2-pane horizontal
        let (tab, _pane0, _pane1, pane2) = make_l_shaped_tab(size);
        tab.resize_split_by(1, 10);
        check("L-shape before removal", &tab);
        tab.remove_pane(pane2.pane_id());
        check("L-shape after removing bottom-right", &tab);

        // L-shaped: remove top-right pane (pane1), should leave 2-pane horizontal
        let (tab, _pane0, pane1, _pane2) = make_l_shaped_tab(size);
        tab.resize_split_by(1, 10);
        tab.remove_pane(pane1.pane_id());
        check("L-shape after removing top-right", &tab);

        // L-shaped: remove left pane (pane0), should leave 2-pane vertical
        let (tab, pane0, _pane1, _pane2) = make_l_shaped_tab(size);
        tab.resize_split_by(1, 10);
        tab.remove_pane(pane0.pane_id());
        check("L-shape after removing left", &tab);

        // Grid: remove one corner, leaving a T-shape
        let (tab, _pane0, _pane1, _pane2, pane3) = make_grid_tab(size);
        tab.resize_split_by(1, 7);
        tab.resize_split_by(2, -5);
        check("Grid before removal", &tab);
        tab.remove_pane(pane3.pane_id());
        check("Grid after removing bottom-right", &tab);

        // T-shaped: remove bottom pane, leaving 2-pane horizontal
        let (tab, _pane0, _pane1, pane2) = make_t_shaped_tab(size);
        tab.resize_split_by(0, 5);
        tab.resize_split_by(1, 8);
        check("T-shape before removal", &tab);
        tab.remove_pane(pane2.pane_id());
        check("T-shape after removing bottom", &tab);
    }

    /// Verify that adding splits to an already-nested layout and then
    /// resizing preserves invariants. Tests the split_and_insert path
    /// combined with resize.
    #[test]
    fn split_then_resize_preserves_invariants() {
        let size = TerminalSize {
            rows: 80,
            cols: 160,
            pixel_width: 1600,
            pixel_height: 2000,
            dpi: 96,
        };

        let check = |label: &str, tab: &Tab| {
            let inner = tab.inner.lock();
            let errors = check_tree_invariants(inner.pane.as_ref().unwrap(), &inner.size);
            assert!(errors.is_empty(), "{}: {:?}", label, errors);
        };

        // Start with L-shape, then add a 4th pane by splitting the left
        let (tab, _pane0, _pane1, _pane2) = make_l_shaped_tab(size);
        tab.resize_split_by(1, 10);

        let vsplit = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Vertical,
                    ..Default::default()
                },
            )
            .unwrap();
        let pane3 = FakePane::new(3, vsplit.second);
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Vertical,
                ..Default::default()
            },
            pane3,
        )
        .unwrap();

        check("L-shape + extra split", &tab);

        // Resize up
        let bigger = TerminalSize {
            rows: 100,
            cols: 200,
            pixel_width: 2000,
            pixel_height: 2500,
            dpi: 96,
        };
        tab.resize(bigger);
        check("after resize up", &tab);

        // Resize back down
        tab.resize(size);
        check("after resize back", &tab);
    }

    // Note: zoom/unzoom with interleaving cannot be tested as a pure unit
    // test because toggle_zoom() requires the Mux singleton. The zoom path
    // is protected by resize() having reconcile_tree_sizes, which is called
    // when unzoom triggers resize().

    /// Test extreme resize: shrink a nested layout to near-minimum size
    /// and then grow it back. Exercises the clamping logic in
    /// reconcile_tree_sizes and adjust_y_size/adjust_x_size.
    ///
    /// The primary assertion is that this does NOT hang (the infinite loop
    /// bug in adjust_y_size/adjust_x_size that was #4878). Secondary: tree
    /// invariants hold after growing back to a reasonable size.
    #[test]
    fn extreme_shrink_and_grow() {
        let size = TerminalSize {
            rows: 80,
            cols: 160,
            pixel_width: 1600,
            pixel_height: 2000,
            dpi: 96,
        };

        let check = |label: &str, tab: &Tab| {
            let inner = tab.inner.lock();
            let errors = check_tree_invariants(inner.pane.as_ref().unwrap(), &inner.size);
            assert!(errors.is_empty(), "{}: {:?}", label, errors);
        };

        // L-shape: shrink to tiny, then grow back
        let (tab, _pane0, _pane1, _pane2) = make_l_shaped_tab(size);
        tab.resize_split_by(1, 10);
        let tiny = TerminalSize {
            rows: 5,
            cols: 6,
            pixel_width: 60,
            pixel_height: 125,
            dpi: 96,
        };
        tab.resize(tiny);
        // Don't check invariants at tiny size — panes may be at minimum
        // and the tree structure is degraded. The important thing is
        // it didn't hang.
        tab.resize(size);
        check("L-shape after grow back from tiny", &tab);

        // Deep nested: same pattern
        let (tab, _, _, _, _) = make_deep_nested_tab(size);
        tab.resize_split_by(1, 5);
        tab.resize_split_by(2, 8);
        let tiny_deep = TerminalSize {
            rows: 8,
            cols: 6,
            pixel_width: 60,
            pixel_height: 200,
            dpi: 96,
        };
        tab.resize(tiny_deep);
        tab.resize(size);
        check("deep nested after grow back from tiny", &tab);

        // Grid: same pattern
        let (tab, _, _, _, _) = make_grid_tab(size);
        tab.resize_split_by(1, 7);
        tab.resize(tiny);
        tab.resize(size);
        check("grid after grow back from tiny", &tab);
    }

    fn is_send_and_sync<T: Send + Sync>() -> bool {
        true
    }

    #[test]
    fn tab_is_send_and_sync() {
        assert!(is_send_and_sync::<Tab>());
    }
}

