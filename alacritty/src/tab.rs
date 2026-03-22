//! Tab and pane management for Flare terminal.

use std::fmt;
use std::sync::Arc;

use alacritty_terminal::event_loop::Notifier;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;

use crate::event::{EventProxy, SearchState};

/// Direction for pane splits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDirection {
    Horizontal,
    Vertical,
}

/// A binary tree of panes within a tab.
#[derive(Debug)]
pub enum PaneNode {
    /// A single terminal pane.
    Leaf(Pane),
    /// A split containing two child nodes.
    Split { direction: SplitDirection, ratio: f32, first: Box<PaneNode>, second: Box<PaneNode> },
}

/// Pixel region occupied by a pane in the viewport.
#[derive(Debug, Clone, Copy)]
pub struct PaneViewport {
    /// X offset in pixels from the viewport left edge.
    pub x: f32,
    /// Y offset in pixels from the viewport top edge (below tab bar).
    pub y: f32,
    /// Width in pixels.
    pub width: f32,
    /// Height in pixels.
    pub height: f32,
}

impl PaneViewport {
    pub fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self { x, y, width, height }
    }

    pub fn contains(self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }
}

impl PaneNode {
    /// Get the currently active pane (traverses to the leftmost/deepest leaf).
    pub fn active_pane(&self) -> &Pane {
        match self {
            PaneNode::Leaf(pane) => pane,
            PaneNode::Split { first, second, .. } => {
                if first.has_active() {
                    first.active_pane()
                } else if second.has_active() {
                    second.active_pane()
                } else {
                    first.active_pane()
                }
            },
        }
    }

    /// Get mutable reference to the currently active pane.
    pub fn active_pane_mut(&mut self) -> &mut Pane {
        match self {
            PaneNode::Leaf(pane) => pane,
            PaneNode::Split { first, second, .. } => {
                if first.has_active() {
                    first.active_pane_mut()
                } else if second.has_active() {
                    second.active_pane_mut()
                } else {
                    first.active_pane_mut()
                }
            },
        }
    }

    /// Total number of leaf panes.
    pub fn pane_count(&self) -> usize {
        match self {
            PaneNode::Leaf(_) => 1,
            PaneNode::Split { first, second, .. } => first.pane_count() + second.pane_count(),
        }
    }

    /// Iterate over all leaf panes in left-to-right, top-to-bottom order.
    pub fn iter_leaves(&self) -> Vec<&Pane> {
        match self {
            PaneNode::Leaf(pane) => vec![pane],
            PaneNode::Split { first, second, .. } => {
                let mut leaves = first.iter_leaves();
                leaves.extend(second.iter_leaves());
                leaves
            },
        }
    }

    /// Whether this node is a single leaf (no splits).
    pub fn is_leaf(&self) -> bool {
        matches!(self, PaneNode::Leaf(_))
    }

    /// Split the active pane, inserting `new_pane` on one side.
    ///
    /// The existing active pane stays as `first`, and the new pane becomes `second`.
    pub fn split_active(&mut self, direction: SplitDirection, new_pane: Pane) {
        match self {
            PaneNode::Leaf(pane) => {
                let existing = Pane {
                    terminal: pane.terminal.clone(),
                    notifier: pane.notifier.clone(),
                    search_state: SearchState::default(),
                    active: false,
                    #[cfg(not(windows))]
                    master_fd: pane.master_fd,
                    #[cfg(not(windows))]
                    shell_pid: pane.shell_pid,
                };
                *self = PaneNode::Split {
                    direction,
                    ratio: 0.5,
                    first: Box::new(PaneNode::Leaf(existing)),
                    second: Box::new(PaneNode::Leaf(new_pane)),
                };
            },
            PaneNode::Split { first, second, .. } => {
                if first.has_active() {
                    first.split_active(direction, new_pane);
                } else if second.has_active() {
                    second.split_active(direction, new_pane);
                } else {
                    first.split_active(direction, new_pane);
                }
            },
        }
    }

    /// Try to close the active pane. Returns the removed pane if successful.
    ///
    /// If only one pane remains, returns `None` (the last pane cannot be closed).
    pub fn close_active(&mut self) -> Option<Pane> {
        match self {
            PaneNode::Leaf(_) => None,
            PaneNode::Split { first, second, .. } => {
                if first.has_active() && first.is_leaf() {
                    // Active pane is first (leaf). Replace this split with second,
                    // returning the pane from first.
                    let old_self = unsafe { std::ptr::read(self as *mut PaneNode) };
                    if let PaneNode::Split { first: old_first, second: old_second, .. } = old_self {
                        let PaneNode::Leaf(removed) = *old_first else { unreachable!() };
                        let mut replacement = *old_second;
                        replacement.ensure_active_first();
                        unsafe {
                            std::ptr::write(self as *mut PaneNode, replacement);
                        }
                        return Some(removed);
                    }
                    unreachable!()
                }

                if second.has_active() && second.is_leaf() {
                    let old_self = unsafe { std::ptr::read(self as *mut PaneNode) };
                    if let PaneNode::Split { first: old_first, second: old_second, .. } = old_self {
                        let PaneNode::Leaf(removed) = *old_second else { unreachable!() };
                        let mut replacement = *old_first;
                        replacement.ensure_active_last();
                        unsafe {
                            std::ptr::write(self as *mut PaneNode, replacement);
                        }
                        return Some(removed);
                    }
                    unreachable!()
                }

                if first.has_active() {
                    if let Some(removed) = first.close_active() {
                        return Some(removed);
                    }
                }

                if second.has_active() {
                    return second.close_active();
                }

                None
            },
        }
    }

    /// Navigate focus to the adjacent pane in the given direction.
    ///
    /// Returns `true` if focus was changed.
    pub fn navigate(&mut self, direction: SplitDirection, reverse: bool) -> bool {
        match self {
            PaneNode::Leaf(_) => false,
            PaneNode::Split { direction: split_dir, first, second, .. } => {
                if *split_dir != direction {
                    if first.has_active() {
                        first.navigate(direction, reverse)
                    } else if second.has_active() {
                        second.navigate(direction, reverse)
                    } else {
                        false
                    }
                } else if first.has_active() {
                    if first.navigate(direction, reverse) {
                        true
                    } else if !reverse {
                        first.clear_active();
                        second.ensure_active_first();
                        true
                    } else {
                        false
                    }
                } else if second.has_active() {
                    if second.navigate(direction, reverse) {
                        true
                    } else if reverse {
                        second.clear_active();
                        first.ensure_active_last();
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            },
        }
    }

    fn has_active(&self) -> bool {
        match self {
            PaneNode::Leaf(pane) => pane.active,
            PaneNode::Split { first, second, .. } => first.has_active() || second.has_active(),
        }
    }

    fn clear_active(&mut self) {
        match self {
            PaneNode::Leaf(pane) => pane.active = false,
            PaneNode::Split { first, second, .. } => {
                first.clear_active();
                second.clear_active();
            },
        }
    }

    fn ensure_active_first(&mut self) {
        match self {
            PaneNode::Leaf(pane) => pane.active = true,
            PaneNode::Split { first, second, .. } => {
                second.clear_active();
                first.ensure_active_first();
            },
        }
    }

    fn ensure_active_last(&mut self) {
        match self {
            PaneNode::Leaf(pane) => pane.active = true,
            PaneNode::Split { first, second, .. } => {
                first.clear_active();
                second.ensure_active_last();
            },
        }
    }

    fn active_leaf_index(&self) -> Option<usize> {
        self.active_leaf_index_inner(0).map(|(index, _)| index)
    }

    fn active_leaf_index_inner(&self, next_index: usize) -> Option<(usize, usize)> {
        match self {
            PaneNode::Leaf(pane) => pane.active.then_some((next_index, next_index + 1)),
            PaneNode::Split { first, second, .. } => {
                if let Some((index, next)) = first.active_leaf_index_inner(next_index) {
                    return Some((index, next));
                }

                let next_index = next_index + first.pane_count();
                second.active_leaf_index_inner(next_index)
            },
        }
    }

    fn set_active_by_index(&mut self, target_index: usize) -> bool {
        let mut current_index = 0;
        self.set_active_by_index_inner(target_index, &mut current_index)
    }

    fn set_active_by_index_inner(
        &mut self,
        target_index: usize,
        current_index: &mut usize,
    ) -> bool {
        match self {
            PaneNode::Leaf(pane) => {
                let is_target = *current_index == target_index;
                pane.active = is_target;
                *current_index += 1;
                is_target
            },
            PaneNode::Split { first, second, .. } => {
                let first_matched = first.set_active_by_index_inner(target_index, current_index);
                let second_matched = second.set_active_by_index_inner(target_index, current_index);
                first_matched || second_matched
            },
        }
    }

    /// Collect the viewport rectangles for all leaf panes.
    ///
    /// `viewport` is the total area available (below tab bar, inside padding).
    pub fn pane_viewports(&self, viewport: PaneViewport) -> Vec<(PaneViewport, &Pane)> {
        let mut result = Vec::new();
        self.collect_viewports(viewport, &mut result);
        result
    }

    fn collect_viewports<'a>(
        &'a self,
        viewport: PaneViewport,
        result: &mut Vec<(PaneViewport, &'a Pane)>,
    ) {
        match self {
            PaneNode::Leaf(pane) => {
                result.push((viewport, pane));
            },
            PaneNode::Split { direction, ratio, first, second } => {
                let split_gap = 2.0; // pixels between panes
                let available = match direction {
                    SplitDirection::Horizontal => viewport.width - split_gap,
                    SplitDirection::Vertical => viewport.height - split_gap,
                };

                let first_size = available * ratio;
                let second_size = available * (1.0 - ratio);

                let (first_viewport, second_viewport) = match direction {
                    SplitDirection::Horizontal => (
                        PaneViewport::new(viewport.x, viewport.y, first_size, viewport.height),
                        PaneViewport::new(
                            viewport.x + first_size + split_gap,
                            viewport.y,
                            second_size,
                            viewport.height,
                        ),
                    ),
                    SplitDirection::Vertical => (
                        PaneViewport::new(viewport.x, viewport.y, viewport.width, first_size),
                        PaneViewport::new(
                            viewport.x,
                            viewport.y + first_size + split_gap,
                            viewport.width,
                            second_size,
                        ),
                    ),
                };

                first.collect_viewports(first_viewport, result);
                second.collect_viewports(second_viewport, result);
            },
        }
    }
}

/// A single terminal pane with its own PTY and terminal state.
pub struct Pane {
    pub terminal: Arc<FairMutex<Term<EventProxy>>>,
    pub notifier: Notifier,
    pub search_state: SearchState,
    pub active: bool,
    #[cfg(not(windows))]
    pub master_fd: std::os::unix::io::RawFd,
    #[cfg(not(windows))]
    pub shell_pid: u32,
}

impl fmt::Debug for Pane {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pane").finish_non_exhaustive()
    }
}

/// A single tab containing a tree of panes and a title.
pub struct Tab {
    pub root: PaneNode,
    pub title: String,
}

impl Tab {
    pub fn active_pane(&self) -> &Pane {
        self.root.active_pane()
    }

    pub fn active_pane_mut(&mut self) -> &mut Pane {
        self.root.active_pane_mut()
    }

    pub fn pane_count(&self) -> usize {
        self.root.pane_count()
    }

    pub fn auto_title(index: usize) -> String {
        format!("Tab {}", index + 1)
    }

    /// Whether this tab has multiple panes (split view).
    pub fn is_split(&self) -> bool {
        !self.root.is_leaf()
    }

    /// Get viewport rectangles for all panes in this tab.
    pub fn pane_viewports(&self, viewport: PaneViewport) -> Vec<(PaneViewport, &Pane)> {
        self.root.pane_viewports(viewport)
    }

    pub fn focus_pane_at_point(&mut self, viewport: PaneViewport, x: f32, y: f32) -> bool {
        let Some(index) = self
            .pane_viewports(viewport)
            .iter()
            .position(|(pane_viewport, _)| pane_viewport.contains(x, y))
        else {
            return false;
        };

        self.root.set_active_by_index(index)
    }

    pub fn focus_adjacent_pane(
        &mut self,
        direction: SplitDirection,
        reverse: bool,
        viewport: PaneViewport,
    ) -> bool {
        let pane_viewports = self.pane_viewports(viewport);
        let Some(active_index) = self.root.active_leaf_index() else {
            return false;
        };
        let Some((active_viewport, _)) = pane_viewports.get(active_index) else {
            return false;
        };

        let active_center_x = active_viewport.x + active_viewport.width / 2.0;
        let active_center_y = active_viewport.y + active_viewport.height / 2.0;

        let mut best_index = None;
        let mut best_primary_distance = f32::MAX;
        let mut best_secondary_distance = f32::MAX;

        for (index, (candidate, _)) in pane_viewports.iter().enumerate() {
            if index == active_index {
                continue;
            }

            let candidate_center_x = candidate.x + candidate.width / 2.0;
            let candidate_center_y = candidate.y + candidate.height / 2.0;

            let overlaps_on_cross_axis = match direction {
                SplitDirection::Horizontal => {
                    candidate.y < active_viewport.y + active_viewport.height
                        && active_viewport.y < candidate.y + candidate.height
                },
                SplitDirection::Vertical => {
                    candidate.x < active_viewport.x + active_viewport.width
                        && active_viewport.x < candidate.x + candidate.width
                },
            };

            if !overlaps_on_cross_axis {
                continue;
            }

            let (primary_distance, secondary_distance) = match direction {
                SplitDirection::Horizontal => {
                    let delta_x = candidate_center_x - active_center_x;
                    if (!reverse && delta_x <= 0.0) || (reverse && delta_x >= 0.0) {
                        continue;
                    }
                    (delta_x.abs(), (candidate_center_y - active_center_y).abs())
                },
                SplitDirection::Vertical => {
                    let delta_y = candidate_center_y - active_center_y;
                    if (!reverse && delta_y <= 0.0) || (reverse && delta_y >= 0.0) {
                        continue;
                    }
                    (delta_y.abs(), (candidate_center_x - active_center_x).abs())
                },
            };

            if primary_distance < best_primary_distance
                || (primary_distance == best_primary_distance
                    && secondary_distance < best_secondary_distance)
            {
                best_index = Some(index);
                best_primary_distance = primary_distance;
                best_secondary_distance = secondary_distance;
            }
        }

        let Some(index) = best_index else {
            return false;
        };

        self.root.set_active_by_index(index)
    }
}

/// Manages all tabs in a window.
pub struct TabManager {
    tabs: Vec<Tab>,
    active_tab_index: usize,
}

impl TabManager {
    pub fn new() -> Self {
        Self { tabs: Vec::new(), active_tab_index: 0 }
    }

    pub fn active_tab_index(&self) -> usize {
        self.active_tab_index
    }

    pub fn active_tab(&self) -> &Tab {
        &self.tabs[self.active_tab_index]
    }

    pub fn active_tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active_tab_index]
    }

    pub fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    pub fn tabs(&self) -> &[Tab] {
        &self.tabs
    }

    pub fn select_tab(&mut self, index: usize) {
        if index < self.tabs.len() {
            self.active_tab_index = index;
        }
    }

    pub fn select_next_tab(&mut self) {
        if self.tabs.len() > 1 {
            self.active_tab_index = (self.active_tab_index + 1) % self.tabs.len();
        }
    }

    pub fn select_previous_tab(&mut self) {
        if self.tabs.len() > 1 {
            self.active_tab_index = (self.active_tab_index + self.tabs.len() - 1) % self.tabs.len();
        }
    }

    pub fn close_tab(&mut self, index: usize) {
        if self.tabs.len() <= 1 {
            return;
        }
        self.tabs.remove(index);
        if index < self.active_tab_index {
            self.active_tab_index -= 1;
        } else if self.active_tab_index >= self.tabs.len() {
            self.active_tab_index = self.tabs.len() - 1;
        }
    }

    /// Add a pre-built tab to the manager.
    pub fn add_tab(&mut self, tab: Tab) {
        self.tabs.push(tab);
        self.active_tab_index = self.tabs.len() - 1;
    }
}

impl Default for TabManager {
    fn default() -> Self {
        Self::new()
    }
}
