//! Directory/session switcher palette.
//!
//! A keyboard-first modal overlay that lists saved sessions (each keyed by a project root) and
//! lets the user switch to one. When open, keystrokes feed a fuzzy-filtered query rather than
//! the terminal; Enter restores the selected session, Escape closes the palette.

use crate::session::{self, SessionEntry};

#[cfg(test)]
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct PaletteState {
    open: bool,
    query: String,
    selected: usize,
    entries: Vec<SessionEntry>,
    filtered: Vec<usize>,
}

impl Default for PaletteState {
    fn default() -> Self {
        Self { open: false, query: String::new(), selected: 0, entries: Vec::new(), filtered: Vec::new() }
    }
}

impl PaletteState {
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Refresh entries from disk, reset selection/query, and open.
    pub fn open(&mut self) {
        self.entries = session::list();
        self.rebuild_filter();
        self.selected = 0;
        self.query.clear();
        self.open = true;
    }

    pub fn close(&mut self) {
        self.open = false;
    }

    pub fn toggle(&mut self) {
        if self.open {
            self.close();
        } else {
            self.open();
        }
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn input(&mut self, c: char) {
        self.query.push(c);
        self.rebuild_filter();
        self.selected = 0;
    }

    pub fn backspace(&mut self) {
        self.query.pop();
        self.rebuild_filter();
        self.selected = 0;
    }

    pub fn move_up(&mut self) {
        if !self.filtered.is_empty() {
            self.selected = self.selected.saturating_sub(1);
        }
    }

    pub fn move_down(&mut self) {
        if !self.filtered.is_empty() {
            self.selected = (self.selected + 1).min(self.filtered.len() - 1);
        }
    }

    pub fn visible(&self) -> Vec<(usize, &SessionEntry)> {
        self.filtered.iter().map(|&i| (i, &self.entries[i])).collect()
    }

    pub fn selected_session(&self) -> Option<session::SessionState> {
        let entry = self.selected_entry()?;
        session::load(&entry.root)
    }

    fn selected_entry(&self) -> Option<&SessionEntry> {
        let &idx = self.filtered.get(self.selected)?;
        self.entries.get(idx)
    }

    fn rebuild_filter(&mut self) {
        let q = self.query.trim().to_lowercase();
        if q.is_empty() {
            self.filtered = (0..self.entries.len()).collect();
            return;
        }

        let mut scored: Vec<(usize, i64)> = self
            .entries
            .iter()
            .enumerate()
            // Score against the visible label (repo-rooted) and the full root path;
            // keep the better match so deep-subdir names stay searchable.
            .filter_map(|(i, e)| {
                let by_label = fuzzy_score(&e.label.to_string_lossy(), &q);
                let by_root = fuzzy_score(&e.root.to_string_lossy(), &q);
                by_label.max(by_root).map(|s| (i, s))
            })
            .collect();

        // Higher score first; ties keep entry order (already most-recent first).
        scored.sort_by(|a, b| b.1.cmp(&a.1));
        self.filtered = scored.into_iter().map(|(i, _)| i).collect();
    }

    #[allow(dead_code)]
    pub fn display_height(&self) -> usize {
        self.filtered.len() + 2
    }
}

/// Subsequence fuzzy match; rewards runs and word boundaries, returns `None` if `query` isn't a
/// subsequence of `haystack`.
fn fuzzy_score(haystack: &str, query: &str) -> Option<i64> {
    let h = haystack.to_lowercase();
    let h_bytes = h.as_bytes();
    let q_bytes = query.as_bytes();

    if q_bytes.is_empty() {
        return Some(0);
    }

    let mut score: i64 = 0;
    let mut run = 0i64;
    let mut prev_matched = false;
    let mut hi = 0;

    for &qc in q_bytes {
        let mut found = false;
        while hi < h_bytes.len() {
            let hc = h_bytes[hi];
            hi += 1;
            if hc == qc {
                found = true;
                // Bonus for word-boundary starts (preceded by '/' or start).
                let boundary = hi == 1 || matches!(h_bytes.get(hi - 2), Some(b'/') | Some(b' ') | Some(b'_') | Some(b'-'));
                if boundary {
                    score += 30;
                }
                // Bonus for consecutive matches.
                if prev_matched {
                    run += 1;
                    score += run * 5;
                } else {
                    run = 0;
                }
                // Reward early matches.
                score += (h_bytes.len() - hi) as i64 / 4;
                prev_matched = true;
                break;
            } else {
                prev_matched = false;
                run = 0;
            }
        }
        if !found {
            return None;
        }
    }

    Some(score)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_matches_subsequence() {
        assert!(fuzzy_score("/home/user/work/api", "api").is_some());
        assert!(fuzzy_score("/home/user/work/api", "wa").is_some()); // "w"ork/"a"pi
        assert!(fuzzy_score("/home/user/work/api", "xyz").is_none());
    }

    #[test]
    fn word_boundary_scores_higher() {
        let s1 = fuzzy_score("api-server", "api").unwrap();
        let s2 = fuzzy_score("xapi-server", "api").unwrap();
        // Matching at a word boundary should score at least as well as mid-word.
        assert!(s1 >= s2);
    }

    #[test]
    fn empty_query_matches_everything() {
        let mut p = PaletteState::default();
        p.entries = vec![
            SessionEntry {
                root: PathBuf::from("/a"),
                label: PathBuf::from("/a"),
                last_used: 1,
                pane_count: 1,
            },
            SessionEntry {
                root: PathBuf::from("/b"),
                label: PathBuf::from("/b"),
                last_used: 2,
                pane_count: 1,
            },
        ];
        p.open = true;
        p.rebuild_filter();
        assert_eq!(p.visible().len(), 2);
    }

    #[test]
    fn query_filters_entries() {
        let mut p = PaletteState::default();
        p.entries = vec![
            SessionEntry {
                root: PathBuf::from("/home/u/api"),
                label: PathBuf::from("/home/u/api"),
                last_used: 1,
                pane_count: 1,
            },
            SessionEntry {
                root: PathBuf::from("/home/u/web"),
                label: PathBuf::from("/home/u/web"),
                last_used: 2,
                pane_count: 1,
            },
        ];
        p.open = true;
        p.query = "ap".to_string();
        p.rebuild_filter();
        let vis = p.visible();
        assert_eq!(vis.len(), 1);
        assert_eq!(vis[0].1.root, PathBuf::from("/home/u/api"));
    }

    #[test]
    fn navigation_clamps() {
        let mut p = PaletteState::default();
        p.entries = vec![
            SessionEntry {
                root: PathBuf::from("/a"),
                label: PathBuf::from("/a"),
                last_used: 1,
                pane_count: 1,
            },
            SessionEntry {
                root: PathBuf::from("/b"),
                label: PathBuf::from("/b"),
                last_used: 2,
                pane_count: 1,
            },
        ];
        p.open = true;
        p.rebuild_filter();
        p.move_up(); // already at 0
        assert_eq!(p.selected, 0);
        p.move_down();
        p.move_down(); // past last
        assert_eq!(p.selected, 1);
    }
}
