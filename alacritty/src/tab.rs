//! Tab and pane management for Flare terminal.

use std::fmt;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use alacritty_terminal::event_loop::Notifier;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;

use crate::event::EventProxy;

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

/// Pixels of empty space between adjacent panes. This gap is the draggable border.
pub const SPLIT_GAP: f32 = 2.0;

/// One grabbable border (a single split's dividing line).
#[derive(Clone)]
struct BorderTarget {
    /// Path to the Split node: `false` = first child, `true` = second.
    path: Vec<bool>,
    /// Axis of the split (Horizontal ↔ left/right, Vertical ↔ up/down).
    direction: SplitDirection,
    /// Viewport the split occupies (origin/extent for the ratio math).
    viewport: PaneViewport,
}

/// An in-progress pane-border drag. Holds one target for a plain border, or two
/// (one per axis) when the press landed on a junction where perpendicular borders
/// meet — dragging then moves both, resizing the whole corner in one motion.
#[derive(Clone)]
pub struct PaneDrag {
    targets: Vec<BorderTarget>,
}

impl PaneDrag {
    /// `true` when two perpendicular borders are grabbed (a junction/corner).
    pub fn is_junction(&self) -> bool {
        self.targets.len() >= 2
    }

    /// The single border's axis, when exactly one target is grabbed.
    pub fn single_direction(&self) -> Option<SplitDirection> {
        match self.targets.as_slice() {
            [t] => Some(t.direction),
            _ => None,
        }
    }
}

/// Split `viewport` into first/second child regions along `direction` at `ratio`.
///
/// Shared by the layout walker ([`PaneNode::collect_viewports`]) and the BSP split
/// logic so both use identical geometry.
fn split_viewport(
    viewport: PaneViewport,
    direction: SplitDirection,
    ratio: f32,
) -> (PaneViewport, PaneViewport) {
    let available = match direction {
        SplitDirection::Horizontal => viewport.width - SPLIT_GAP,
        SplitDirection::Vertical => viewport.height - SPLIT_GAP,
    };

    let first_size = available * ratio;
    let second_size = available * (1.0 - ratio);

    match direction {
        SplitDirection::Horizontal => (
            PaneViewport::new(viewport.x, viewport.y, first_size, viewport.height),
            PaneViewport::new(
                viewport.x + first_size + SPLIT_GAP,
                viewport.y,
                second_size,
                viewport.height,
            ),
        ),
        SplitDirection::Vertical => (
            PaneViewport::new(viewport.x, viewport.y, viewport.width, first_size),
            PaneViewport::new(
                viewport.x,
                viewport.y + first_size + SPLIT_GAP,
                viewport.width,
                second_size,
            ),
        ),
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

    /// Mutable variant of [`active_pane`](Self::active_pane).
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

    /// Mutable variant of [`iter_leaves`](Self::iter_leaves), for periodic state refresh
    /// (e.g. agent detection) that writes back onto each pane.
    pub fn iter_leaves_mut(&mut self) -> Vec<&mut Pane> {
        match self {
            PaneNode::Leaf(pane) => vec![pane],
            PaneNode::Split { first, second, .. } => {
                let mut leaves = first.iter_leaves_mut();
                leaves.extend(second.iter_leaves_mut());
                leaves
            },
        }
    }

    /// Whether this node is a single leaf (no splits).
    pub fn is_leaf(&self) -> bool {
        matches!(self, PaneNode::Leaf(_))
    }

    /// Split the largest leaf in this subtree, inserting `new_pane` on one side.
    ///
    /// Uses binary-space-partitioning: the largest remaining region is halved so
    /// panes stay balanced as the tree grows, regardless of which pane is focused.
    /// `viewport` is the region occupied by this node.
    ///
    /// `new_pane` is expected to arrive as the active pane; the previously active
    /// leaf is cleared so the new pane becomes the sole active leaf.
    pub fn split(&mut self, direction: SplitDirection, new_pane: Pane, viewport: PaneViewport) {
        match self {
            PaneNode::Leaf(pane) => {
                let existing = Pane {
                    terminal: pane.terminal.clone(),
                    notifier: pane.notifier.clone(),
                    active: false,
                    // A split is a re-layout, not a new pane — the existing pane keeps its id.
                    id: pane.id,
                    #[cfg(not(windows))]
                    master_fd: pane.master_fd,
                    #[cfg(not(windows))]
                    shell_pid: pane.shell_pid,
                    agent: pane.agent,
                    agent_status: pane.agent_status,
                    // Keep the existing pane's timestamp continuity (shared Arc).
                    last_output: pane.last_output.clone(),
                    agent_started_at: pane.agent_started_at,
                    agent_model: pane.agent_model.clone(),
                    agent_misses: pane.agent_misses,
                    agent_cmdline: pane.agent_cmdline.clone(),
                };
                *self = PaneNode::Split {
                    direction,
                    ratio: 0.5,
                    first: Box::new(PaneNode::Leaf(existing)),
                    second: Box::new(PaneNode::Leaf(new_pane)),
                };
            },
            PaneNode::Split { direction: split_direction, ratio, first, second } => {
                let (first_viewport, second_viewport) =
                    split_viewport(viewport, *split_direction, *ratio);

                // Recurse into the child whose subtree holds the largest leaf — the
                // defining BSP rule (split the biggest region first).
                if first.max_leaf_area(first_viewport) >= second.max_leaf_area(second_viewport) {
                    first.split(direction, new_pane, first_viewport);
                } else {
                    second.split(direction, new_pane, second_viewport);
                }

                // Rebalance repeated splits in the same direction so panes share the
                // full tab area instead of only subdividing a single subtree.
                if *split_direction == direction {
                    let first_count = first.pane_count() as f32;
                    let second_count = second.pane_count() as f32;
                    *ratio = first_count / (first_count + second_count);
                }
            },
        }
    }

    /// Largest leaf area beneath this node, given the region it occupies.
    ///
    /// Used by [`split`](Self::split) to pick the subtree to subdivide.
    fn max_leaf_area(&self, viewport: PaneViewport) -> f32 {
        match self {
            PaneNode::Leaf(_) => viewport.width * viewport.height,
            PaneNode::Split { direction, ratio, first, second } => {
                let (first_viewport, second_viewport) = split_viewport(viewport, *direction, *ratio);
                first.max_leaf_area(first_viewport).max(second.max_leaf_area(second_viewport))
            },
        }
    }

    /// Resize the border nearest the active pane that lies on the requested axis.
    ///
    /// Walks down the path containing the active pane. The **innermost** split whose
    /// direction matches `direction` is the one resized (a single border moves);
    /// once it is adjusted the walk stops, so ancestor borders are left alone. A
    /// non-matching split is descended but not mutated.
    ///
    /// `delta_ratio > 0` grows the `first` child's share, i.e. moves the border away
    /// from `first`'s origin (right for a horizontal split, down for a vertical one).
    /// The ratio is clamped to `[0.1, 0.9]` so no pane collapses.
    /// Returns `true` if a ratio was changed.
    pub fn resize_active(
        &mut self,
        direction: SplitDirection,
        delta_ratio: f32,
        viewport: PaneViewport,
    ) -> bool {
        match self {
            PaneNode::Leaf(_) => false,
            PaneNode::Split { direction: split_dir, ratio, first, second } => {
                let (first_viewport, second_viewport) =
                    split_viewport(viewport, *split_dir, *ratio);

                if first.has_active() {
                    if *split_dir == direction {
                        // This is the nearest border on the requested axis.
                        let clamped = (*ratio + delta_ratio).clamp(0.1, 0.9);
                        if (clamped - *ratio).abs() > f32::EPSILON {
                            *ratio = clamped;
                            return true;
                        }
                        return false;
                    }
                    first.resize_active(direction, delta_ratio, first_viewport)
                } else if second.has_active() {
                    if *split_dir == direction {
                        let clamped = (*ratio + delta_ratio).clamp(0.1, 0.9);
                        if (clamped - *ratio).abs() > f32::EPSILON {
                            *ratio = clamped;
                            return true;
                        }
                        return false;
                    }
                    second.resize_active(direction, delta_ratio, second_viewport)
                } else {
                    false
                }
            },
        }
    }

    /// Radius around a pane-border junction (where perpendicular borders cross)
    /// within which the corner is grabbable. Wider than the plain per-axis tolerance
    /// so junctions are easy to land on without making border segments feel sticky.
    const JUNCTION_RADIUS: f32 = 6.0;

    /// Collect every split border near `(x, y)` into `out`. A border is a hit when
    /// the cursor is within `tolerance` on the border's own axis, OR within
    /// [`JUNCTION_RADIUS`](Self::JUNCTION_RADIUS) on its axis while also within that
    /// radius on the perpendicular axis — the latter widens the grab zone only at
    /// junctions, so corners are easy to grab but plain segments stay tight.
    fn borders_at_point(
        &self,
        viewport: PaneViewport,
        x: f32,
        y: f32,
        tolerance: f32,
        out: &mut Vec<BorderTarget>,
    ) {
        match self {
            PaneNode::Leaf(_) => {},
            PaneNode::Split { direction, ratio, first, second } => {
                let (first_vp, second_vp) = split_viewport(viewport, *direction, *ratio);

                // The dividing line sits at the end of the first child's region,
                // centered in the gap. `along` is the cursor's position on the border's
                // own axis; `across` is its position on the perpendicular axis.
                let (border_pos, along, cross_lo, cross_hi, across) = match direction {
                    SplitDirection::Horizontal => {
                        let border = first_vp.x + first_vp.width + SPLIT_GAP / 2.0;
                        (border, x, viewport.y, viewport.y + viewport.height, y)
                    },
                    SplitDirection::Vertical => {
                        let border = first_vp.y + first_vp.height + SPLIT_GAP / 2.0;
                        (border, y, viewport.x, viewport.x + viewport.width, x)
                    },
                };

                let in_cross_bounds = cross_lo <= across && across <= cross_hi;
                let along_dist = (along - border_pos).abs();
                // Tight hit on this border, OR a near-junction hit: close on this
                // border's axis *and* a perpendicular border also passes nearby — the
                // latter widens the grab zone only at true junctions.
                let on_this = in_cross_bounds
                    && (along_dist <= tolerance
                        || (along_dist <= Self::JUNCTION_RADIUS
                            && Self::has_crossing_border(
                                self,
                                viewport,
                                *direction,
                                border_pos,
                                x,
                                y,
                            )));
                if on_this {
                    out.push(BorderTarget { path: Vec::new(), direction: *direction, viewport });
                }

                // Descend into the child containing the point, prefixing its targets'
                // paths so they resolve relative to this node.
                let inner = if first_vp.contains(x, y) {
                    let mut sub = Vec::new();
                    first.borders_at_point(first_vp, x, y, tolerance, &mut sub);
                    Some((false, sub))
                } else if second_vp.contains(x, y) {
                    let mut sub = Vec::new();
                    second.borders_at_point(second_vp, x, y, tolerance, &mut sub);
                    Some((true, sub))
                } else {
                    None
                };
                if let Some((went_second, sub)) = inner {
                    for mut t in sub {
                        t.path.insert(0, went_second);
                        out.push(t);
                    }
                }
            },
        }
    }

    /// Does a perpendicular border cross the line `axis == border_pos` within
    /// [`JUNCTION_RADIUS`](Self::JUNCTION_RADIUS) of `(x, y)`? Used to widen the
    /// grab zone only where borders actually meet (a true junction).
    fn has_crossing_border(
        &self,
        viewport: PaneViewport,
        my_direction: SplitDirection,
        my_border_pos: f32,
        x: f32,
        y: f32,
    ) -> bool {
        match self {
            PaneNode::Leaf(_) => false,
            PaneNode::Split { direction, ratio, first, second } => {
                let (first_vp, second_vp) = split_viewport(viewport, *direction, *ratio);
                // A perpendicular split crosses our border if its dividing line is
                // within JUNCTION_RADIUS of the cursor on the perpendicular axis.
                let (perp_border, perp_cursor) = match direction {
                    SplitDirection::Horizontal => (first_vp.x + first_vp.width + SPLIT_GAP / 2.0, x),
                    SplitDirection::Vertical => (first_vp.y + first_vp.height + SPLIT_GAP / 2.0, y),
                };
                let crosses = *direction != my_direction
                    && (perp_cursor - perp_border).abs() <= Self::JUNCTION_RADIUS
                    && (match my_direction {
                        SplitDirection::Horizontal => y,
                        SplitDirection::Vertical => x,
                    } - my_border_pos).abs() <= Self::JUNCTION_RADIUS
                    && viewport.contains(x, y);
                if crosses {
                    return true;
                }
                first.has_crossing_border(first_vp, my_direction, my_border_pos, x, y)
                    || second.has_crossing_border(second_vp, my_direction, my_border_pos, x, y)
            },
        }
    }

    /// If `(x, y)` lies on one or more split borders, return a [`PaneDrag`] for
    /// them. A junction (two perpendicular borders) yields a drag that moves both.
    /// Returns `None` for a single leaf (e.g. a zoomed tab).
    pub fn border_at_point(
        &self,
        viewport: PaneViewport,
        x: f32,
        y: f32,
        tolerance: f32,
    ) -> Option<PaneDrag> {
        let mut targets = Vec::new();
        self.borders_at_point(viewport, x, y, tolerance, &mut targets);
        // Keep at most one target per axis: a junction has exactly one Horizontal
        // and one Vertical border. If duplicates on an axis exist (nested same-axis
        // splits), prefer the innermost (deepest path) — it's the closest border.
        let mut by_axis: [Option<BorderTarget>; 2] = [None, None];
        for t in targets {
            let i = match t.direction {
                SplitDirection::Horizontal => 0,
                SplitDirection::Vertical => 1,
            };
            by_axis[i] = Some(match by_axis[i].take() {
                // Deepest path (most segments) wins → nearest border.
                Some(existing) if existing.path.len() >= t.path.len() => existing,
                _ => t,
            });
        }
        let kept: Vec<_> = by_axis.into_iter().flatten().collect();
        (!kept.is_empty()).then(|| PaneDrag { targets: kept })
    }

    /// Apply an in-progress drag: recompute the ratio of every grabbed split from
    /// the cursor position so each divider tracks the pointer. A horizontal split's
    /// ratio comes from `x`, a vertical split's from `y`; the two are independent so
    /// a junction drag moves both axes at once. Clamped to `[0.1, 0.9]`.
    pub fn set_ratio_at(&mut self, drag: &PaneDrag, x: f32, y: f32) {
        for target in &drag.targets {
            self.set_one_ratio(target, x, y);
        }
    }

    /// Recurse to the split at `target.path` and set its ratio from the cursor.
    fn set_one_ratio(&mut self, target: &BorderTarget, x: f32, y: f32) {
        match self {
            PaneNode::Leaf(_) => {},
            PaneNode::Split { ratio, first, second, .. } => {
                if target.path.is_empty() {
                    let (origin, extent, cursor) = match target.direction {
                        SplitDirection::Horizontal => (target.viewport.x, target.viewport.width, x),
                        SplitDirection::Vertical => (target.viewport.y, target.viewport.height, y),
                    };
                    let denom = (extent - SPLIT_GAP).max(1.0);
                    *ratio = ((cursor - origin) / denom).clamp(0.1, 0.9);
                } else {
                    let go_second = target.path[0];
                    let mut sub = target.clone();
                    sub.path.remove(0);
                    if go_second {
                        second.set_one_ratio(&sub, x, y);
                    } else {
                        first.set_one_ratio(&sub, x, y);
                    }
                }
            },
        }
    }
    ///
    /// If only one pane remains, returns `None` (the last pane cannot be closed).
    pub fn close_active(&mut self) -> Option<Pane> {
        match self {
            PaneNode::Leaf(_) => None,
            PaneNode::Split { direction: split_direction, ratio, first, second } => {
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
                    let rebalance_current = matches!(
                        &**first,
                        PaneNode::Split { direction: child_direction, .. }
                            if *child_direction == *split_direction
                    );
                    if let Some(removed) = first.close_active() {
                        if rebalance_current {
                            let first_count = first.pane_count() as f32;
                            let second_count = second.pane_count() as f32;
                            *ratio = first_count / (first_count + second_count);
                        }
                        return Some(removed);
                    }
                }

                if second.has_active() {
                    let rebalance_current = matches!(
                        &**second,
                        PaneNode::Split { direction: child_direction, .. }
                            if *child_direction == *split_direction
                    );
                    if let Some(removed) = second.close_active() {
                        if rebalance_current {
                            let first_count = first.pane_count() as f32;
                            let second_count = second.pane_count() as f32;
                            *ratio = first_count / (first_count + second_count);
                        }
                        return Some(removed);
                    }
                }

                None
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

    /// Focus the leaf with the given durable id; returns whether it was found (#28).
    /// Consumed by the JSON socket (#33) and CLI pane control (#34).
    #[allow(dead_code)]
    pub(crate) fn focus_pane_by_id(&mut self, id: u64) -> bool {
        match self {
            PaneNode::Leaf(pane) => {
                if pane.id == id {
                    pane.active = true;
                    true
                } else {
                    false
                }
            },
            PaneNode::Split { first, second, .. } => {
                if first.focus_pane_by_id(id) {
                    second.clear_active();
                    true
                } else if second.focus_pane_by_id(id) {
                    first.clear_active();
                    true
                } else {
                    false
                }
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
                let (first_viewport, second_viewport) = split_viewport(viewport, *direction, *ratio);
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
    pub active: bool,
    /// Durable, never-reused pane id (#28); addressable as `w<window>:p<id>`.
    pub id: crate::pane_address::PaneId,
    #[cfg(not(windows))]
    pub master_fd: std::os::unix::io::RawFd,
    #[cfg(not(windows))]
    pub shell_pid: u32,
    /// Detected AI agent running in this pane's foreground process, if any.
    /// Refreshed periodically by `WindowContext::detect_agents`.
    pub agent: Option<crate::agent::AgentKind>,
    /// Live status of the detected agent (#15), derived from `last_output`.
    /// Recomputed alongside `agent` by `detect_agents`.
    pub agent_status: crate::agent::AgentStatus,
    /// Last PTY output time (millis since UNIX_EPOCH), written by the PTY reader
    /// thread via this shared atomic so the main thread can read it lock-free.
    pub last_output: Arc<AtomicU64>,
    /// UNIX ms when the current agent was first detected.
    pub agent_started_at: Option<u64>,
    pub agent_model: Option<String>,
    /// Consecutive detect misses (grace before clearing agent + elapsed).
    pub agent_misses: u8,
    /// Argv cached while the agent was alive (for session save after the process exits).
    pub agent_cmdline: Option<Vec<String>>,
}

impl fmt::Debug for Pane {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pane").finish_non_exhaustive()
    }
}

/// A single tab containing a tree of panes.
pub struct Tab {
    pub root: PaneNode,
    pub name: Option<String>,
    pub zoomed: bool,
}

/// Per-tab info for tab-bar rendering: the resolved title plus an optional detected agent whose
/// status dot is drawn next to it.
#[derive(Clone)]
pub struct TabBarEntry {
    pub title: String,
    pub agent: Option<crate::agent::AgentKind>,
    pub agent_status: Option<crate::agent::AgentStatus>,
}

impl Tab {
    pub fn active_pane(&self) -> &Pane {
        self.root.active_pane()
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

    /// Toggle zoom: expand the active pane to fill the tab, or restore the split layout.
    pub fn toggle_zoom(&mut self) {
        if self.is_split() {
            self.zoomed = !self.zoomed;
        }
    }

    /// Get viewport rectangles for all panes in this tab.
    pub fn pane_viewports(&self, viewport: PaneViewport) -> Vec<(PaneViewport, &Pane)> {
        self.root.pane_viewports(viewport)
    }

    pub fn focus_pane_at_point(&mut self, viewport: PaneViewport, x: f32, y: f32) -> bool {
        if self.zoomed {
            return false;
        }
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
/// Tab-bar visibility filter keyed by the active pane's agent status (#16).
/// A view filter — hidden tabs keep running; switching to them still works.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TabFilter {
    /// No filtering — all tabs visible.
    #[default]
    All,
    /// Only tabs whose active agent is producing output.
    Working,
    /// Only tabs whose active agent has gone quiet.
    Idle,
}

impl TabFilter {
    /// Advance to the next filter in the cycle: All → Working → Idle → All.
    pub fn cycle(self) -> Self {
        match self {
            TabFilter::All => TabFilter::Working,
            TabFilter::Working => TabFilter::Idle,
            TabFilter::Idle => TabFilter::All,
        }
    }

    /// Does a tab with the given active-pane agent status pass this filter?
    /// `None` (no agent detected) matches only `All`.
    pub fn matches(self, status: Option<crate::agent::AgentStatus>) -> bool {
        match self {
            TabFilter::All => true,
            TabFilter::Working => status == Some(crate::agent::AgentStatus::Working),
            TabFilter::Idle => status == Some(crate::agent::AgentStatus::Idle),
        }
    }

    /// Short label shown in the tab bar when this filter is active.
    pub fn label(self) -> Option<&'static str> {
        match self {
            TabFilter::All => None,
            TabFilter::Working => Some("working"),
            TabFilter::Idle => Some("idle"),
        }
    }
}

pub struct TabManager {
    tabs: Vec<Tab>,
    active_tab_index: usize,
    /// Current tab-bar visibility filter (#16). Pure view state — never affects
    /// underlying tab state or number-key addressing.
    filter: TabFilter,
}

impl TabManager {
    pub fn new() -> Self {
        Self { tabs: Vec::new(), active_tab_index: 0, filter: TabFilter::default() }
    }

    /// The current tab-bar visibility filter.
    pub fn filter(&self) -> TabFilter {
        self.filter
    }

    /// Advance the tab-bar filter to the next step in its cycle.
    pub fn cycle_filter(&mut self) {
        self.filter = self.filter.cycle();
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

    pub fn tabs_mut(&mut self) -> &mut [Tab] {
        &mut self.tabs
    }

    pub fn select_tab(&mut self, index: usize) {
        if index < self.tabs.len() {
            self.active_tab_index = index;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Two panes must tile the viewport exactly (both sizes sum to the available
    /// space, accounting for the gap), with no overlap and the second offset past
    /// the first plus the gap.
    #[test]
    fn split_viewport_halves_along_direction() {
        let vp = PaneViewport::new(10.0, 20.0, 100.0, 60.0);

        // Horizontal split at 0.5 → divides width, preserves height.
        let (first, second) = split_viewport(vp, SplitDirection::Horizontal, 0.5);
        assert_eq!(first.width, 49.0); // (100 - 2) * 0.5
        assert_eq!(second.width, 49.0);
        assert_eq!(first.height, 60.0);
        assert_eq!(second.height, 60.0);
        // Second is offset right of the first by its width + the gap.
        assert!((second.x - (first.x + first.width + SPLIT_GAP)).abs() < 1e-3);
        assert_eq!(first.y, second.y);
    }

    #[test]
    fn split_viewport_vertical_divides_height() {
        let vp = PaneViewport::new(0.0, 0.0, 80.0, 40.0);
        let (first, second) = split_viewport(vp, SplitDirection::Vertical, 0.25);
        // (40 - 2) * 0.25 = 9.5 first, 28.5 second.
        assert!((first.height - 9.5).abs() < 1e-3);
        assert!((second.height - 28.5).abs() < 1e-3);
        assert_eq!(first.width, 80.0);
        assert_eq!(second.width, 80.0);
        // Second sits below the first.
        assert!((second.y - (first.y + first.height + SPLIT_GAP)).abs() < 1e-3);
    }

    /// The two children exactly re-tile the parent: union width/height equals the
    /// original, accounting for the one inter-pane gap. This is the invariant the
    /// BSP splitter and the layout walker both rely on.
    #[test]
    fn split_viewport_children_fill_parent() {
        let vp = PaneViewport::new(5.0, 7.0, 200.0, 120.0);
        for dir in [SplitDirection::Horizontal, SplitDirection::Vertical] {
            for ratio in [0.0, 0.1, 0.3, 0.5, 0.7, 0.9, 1.0] {
                let (first, second) = split_viewport(vp, dir, ratio);
                match dir {
                    SplitDirection::Horizontal => {
                        let total = first.width + second.width + SPLIT_GAP;
                        assert!((total - vp.width).abs() < 1e-3, "horizontal width underflow");
                        assert_eq!(first.height, vp.height);
                        assert_eq!(second.height, vp.height);
                    },
                    SplitDirection::Vertical => {
                        let total = first.height + second.height + SPLIT_GAP;
                        assert!((total - vp.height).abs() < 1e-3, "vertical height underflow");
                        assert_eq!(first.width, vp.width);
                        assert_eq!(second.width, vp.width);
                    },
                }
                // Origin never drifts for the first child.
                assert_eq!(first.x, vp.x);
                assert_eq!(first.y, vp.y);
            }
        }
    }

    /// Extreme ratios must clamp gracefully (available * 0.0 / 1.0 are valid floats).
    #[test]
    fn split_viewport_handles_extreme_ratios() {
        let vp = PaneViewport::new(0.0, 0.0, 50.0, 50.0);
        let (first, _) = split_viewport(vp, SplitDirection::Horizontal, 0.0);
        assert!(first.width <= f32::EPSILON);
        let (_, second) = split_viewport(vp, SplitDirection::Horizontal, 1.0);
        assert!(second.width <= f32::EPSILON);
    }

    /// The resize step and clamp bounds `resize_active` applies. Verified in
    /// isolation here because the walker itself operates on live `Pane`s (PTYs)
    /// that can't be built in a unit test.
    #[test]
    fn resize_ratio_clamp_math() {
        let step = 0.05f32;
        let clamp = |r: f32| r.clamp(0.1, 0.9);

        // A normal mid-range step applies in full.
        assert!((clamp(0.5 + step) - 0.55).abs() < 1e-3);
        assert!((clamp(0.5 - step) - 0.45).abs() < 1e-3);

        // Near the floor the negative step clamps to 0.1 (no pane collapses).
        assert!((clamp(0.12 - step) - 0.1).abs() < 1e-3);
        assert!((clamp(0.05 - step) - 0.1).abs() < 1e-3);

        // Near the ceiling the positive step clamps to 0.9.
        assert!((clamp(0.88 + step) - 0.9).abs() < 1e-3);
        assert!((clamp(0.95 + step) - 0.9).abs() < 1e-3);

        // A clamped ratio is exactly at a bound, never outside [0.1, 0.9].
        for r in [-1.0, 0.0, 0.05, 0.5, 0.95, 1.0, 2.0] {
            let c = clamp(r);
            assert!(c >= 0.1 && c <= 0.9);
        }
    }

    /// The drag-resize ratio formula `set_ratio_at` applies: ratio =
    /// (cursor − origin) / (extent − SPLIT_GAP), clamped [0.1, 0.9]. Verified in
    /// isolation (the walker needs a live Pane, same caveat as the other tests).
    #[test]
    fn drag_ratio_formula() {
        let origin = 0.0f32;
        let extent = 100.0f32;
        let denom = (extent - SPLIT_GAP).max(1.0);
        let ratio = |cursor: f32| ((cursor - origin) / denom).clamp(0.1, 0.9);

        // Cursor at the midpoint → 0.5.
        assert!((ratio(49.0) - 0.5).abs() < 1e-3); // (100-2)/2 = 49
        // Cursor at the left edge clamps to the 0.1 floor.
        assert!((ratio(0.0) - 0.1).abs() < 1e-3);
        // Cursor past the right edge clamps to the 0.9 ceiling.
        assert!((ratio(100.0) - 0.9).abs() < 1e-3);
        // Cursor tracking is linear within bounds.
        assert!((ratio(24.5) - 0.25).abs() < 1e-2);
    }

    /// The border line sits at `origin + (extent − SPLIT_GAP) * ratio + SPLIT_GAP/2`,
    /// i.e. at the end of the first child's region centered in the gap. This is the
    /// position `border_at_point` hit-tests against.
    #[test]
    fn drag_border_position() {
        let origin = 10.0f32;
        let extent = 100.0f32;
        for ratio in [0.25, 0.5, 0.75] {
            let first_size = (extent - SPLIT_GAP) * ratio;
            let border = origin + first_size + SPLIT_GAP / 2.0;
            // The first child ends at origin + first_size; the border is gap/2 beyond it.
            assert!((border - (origin + first_size + SPLIT_GAP / 2.0)).abs() < 1e-6);
            // Sanity: border is strictly inside the viewport.
            assert!(border > origin && border < origin + extent);
        }
    }

    #[test]
    fn tab_filter_cycles_all_working_idle() {
        assert_eq!(TabFilter::All.cycle(), TabFilter::Working);
        assert_eq!(TabFilter::Working.cycle(), TabFilter::Idle);
        assert_eq!(TabFilter::Idle.cycle(), TabFilter::All);
    }

    #[test]
    fn tab_filter_matches_by_status() {
        use crate::agent::AgentStatus;
        // All matches everything, including no-agent tabs.
        assert!(TabFilter::All.matches(None));
        assert!(TabFilter::All.matches(Some(AgentStatus::Working)));
        assert!(TabFilter::All.matches(Some(AgentStatus::Idle)));
        // Working matches only actively-working agents.
        assert!(TabFilter::Working.matches(Some(AgentStatus::Working)));
        assert!(!TabFilter::Working.matches(Some(AgentStatus::Idle)));
        assert!(!TabFilter::Working.matches(None)); // no agent → excluded
        // Idle matches only quiet agents.
        assert!(TabFilter::Idle.matches(Some(AgentStatus::Idle)));
        assert!(!TabFilter::Idle.matches(Some(AgentStatus::Working)));
        assert!(!TabFilter::Idle.matches(None));
    }
}
