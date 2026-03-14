//! Tab management for Flare terminal.

#![allow(dead_code)]

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
    Split {
        direction: SplitDirection,
        ratio: f32,
        first: Box<PaneNode>,
        second: Box<PaneNode>,
    },
}

impl PaneNode {
    pub fn active_pane(&self) -> &Pane {
        match self {
            PaneNode::Leaf(pane) => pane,
            PaneNode::Split { first, .. } => first.active_pane(),
        }
    }

    pub fn active_pane_mut(&mut self) -> &mut Pane {
        match self {
            PaneNode::Leaf(pane) => pane,
            PaneNode::Split { first, .. } => first.active_pane_mut(),
        }
    }

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
}

/// A single terminal pane with its own PTY and terminal state.
pub struct Pane {
    pub terminal: Arc<FairMutex<Term<EventProxy>>>,
    pub notifier: Notifier,
    pub search_state: SearchState,
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
        let title = Tab::auto_title(self.tabs.len());
        self.tabs.push(Tab { root: tab.root, title });
        self.active_tab_index = self.tabs.len() - 1;
    }
}

impl Default for TabManager {
    fn default() -> Self {
        Self::new()
    }
}
