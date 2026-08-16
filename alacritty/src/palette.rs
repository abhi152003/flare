//! Navigator overlay: fuzzy switcher for saved sessions and live panes.

use crate::agent::{AgentKind, AgentStatus};
use crate::pane_address::PaneAddress;
use crate::session::{self, SessionEntry};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub enum PaletteEntry {
    Session(SessionEntry),
    Pane(LivePane),
}

#[derive(Debug, Clone)]
pub struct LivePane {
    pub address: PaneAddress,
    pub cwd: Option<PathBuf>,
    pub agent: Option<AgentKind>,
    #[allow(dead_code)]
    pub agent_status: Option<AgentStatus>,
    pub title: Option<String>,
    pub agent_model: Option<String>,
    pub agent_elapsed: Option<String>,
}

impl PaletteEntry {
    pub fn row_text(&self) -> String {
        match self {
            PaletteEntry::Session(e) => crate::path_util::shorten_path(&e.label),
            PaletteEntry::Pane(p) => {
                let base = p
                    .title
                    .clone()
                    .or_else(|| p.cwd.as_ref().map(|c| crate::path_util::shorten_path(c)))
                    .unwrap_or_else(|| p.address.to_string());
                if let Some(kind) = &p.agent {
                    let meta = crate::agent::metadata_label(
                        *kind,
                        p.agent_model.as_deref(),
                        p.agent_elapsed.as_deref(),
                    );
                    format!("{base}  · {meta}")
                } else {
                    base
                }
            },
        }
    }

    fn search_text(&self) -> Vec<String> {
        match self {
            PaletteEntry::Session(e) => vec![e.label.to_string_lossy().into_owned(), e.root.to_string_lossy().into_owned()],
            PaletteEntry::Pane(p) => {
                let mut texts = Vec::new();
                if let Some(t) = &p.title {
                    texts.push(t.clone());
                }
                if let Some(cwd) = &p.cwd {
                    texts.push(cwd.to_string_lossy().into_owned());
                }
                if let Some(kind) = &p.agent {
                    texts.push(kind.label().to_string());
                }
                texts
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct PaletteState {
    open: bool,
    query: String,
    selected: usize,
    entries: Vec<PaletteEntry>,
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

    pub fn open_with_entries(&mut self, entries: Vec<PaletteEntry>) {
        self.entries = entries;
        self.rebuild_filter();
        self.selected = 0;
        self.query.clear();
        self.open = true;
    }

    pub fn close(&mut self) {
        self.open = false;
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

    pub fn visible(&self) -> Vec<(usize, &PaletteEntry)> {
        self.filtered.iter().map(|&i| (i, &self.entries[i])).collect()
    }

    pub fn selected_session(&self) -> Option<session::SessionState> {
        let entry = self.selected_entry()?;
        match entry {
            PaletteEntry::Session(e) => session::load(&e.root),
            PaletteEntry::Pane(_) => None,
        }
    }

    pub fn selected_pane(&self) -> Option<&PaneAddress> {
        match self.selected_entry()? {
            PaletteEntry::Pane(p) => Some(&p.address),
            PaletteEntry::Session(_) => None,
        }
    }

    fn selected_entry(&self) -> Option<&PaletteEntry> {
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
            .filter_map(|(i, e)| {
                e.search_text()
                    .iter()
                    .filter_map(|t| fuzzy_score(t, &q))
                    .max()
                    .map(|s| (i, s))
            })
            .collect();

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

    fn session(root: &str) -> PaletteEntry {
        PaletteEntry::Session(SessionEntry {
            root: PathBuf::from(root),
            label: PathBuf::from(root),
            last_used: 1,
            pane_count: 1,
        })
    }

    fn pane(title: &str, cwd: &str, agent: Option<&str>) -> PaletteEntry {
        PaletteEntry::Pane(LivePane {
            address: PaneAddress::new(1, 1),
            cwd: Some(PathBuf::from(cwd)),
            agent: agent.map(|a| match a {
                "claude" => AgentKind::ClaudeCode,
                "codex" => AgentKind::Codex,
                _ => AgentKind::Unknown,
            }),
            agent_status: agent.map(|_| AgentStatus::Working),
            title: Some(title.to_string()),
            agent_model: None,
            agent_elapsed: agent.map(|_| "5m".to_string()),
        })
    }

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
        p.open_with_entries(vec![session("/a"), session("/b")]);
        assert_eq!(p.visible().len(), 2);
    }

    #[test]
    fn query_filters_entries() {
        let mut p = PaletteState::default();
        p.open_with_entries(vec![session("/home/u/api"), session("/home/u/web")]);
        p.query = "ap".to_string();
        p.rebuild_filter();
        let vis = p.visible();
        assert_eq!(vis.len(), 1);
    }

    #[test]
    fn mixed_sessions_and_panes_filter_together() {
        let mut p = PaletteState::default();
        p.open_with_entries(vec![
            session("/home/u/api"),
            pane("server logs", "/home/u/api", None),
            pane("claude session", "/home/u/web", Some("claude")),
        ]);
        // Searching "api" matches both the session and the pane in /home/u/api.
        p.query = "api".to_string();
        p.rebuild_filter();
        assert_eq!(p.visible().len(), 2);
    }

    #[test]
    fn pane_search_matches_agent_name() {
        let mut p = PaletteState::default();
        p.open_with_entries(vec![
            session("/a"),
            pane("work", "/b", Some("codex")),
        ]);
        p.query = "codex".to_string();
        p.rebuild_filter();
        let vis = p.visible();
        assert_eq!(vis.len(), 1);
        assert!(matches!(vis[0].1, PaletteEntry::Pane(_)));
    }

    #[test]
    fn pane_row_text_includes_metadata() {
        let entry = PaletteEntry::Pane(LivePane {
            address: PaneAddress::new(1, 2),
            cwd: Some(PathBuf::from("/home/u/api")),
            agent: Some(AgentKind::ClaudeCode),
            agent_status: Some(AgentStatus::Working),
            title: None,
            agent_model: Some("sonnet".into()),
            agent_elapsed: Some("5m".into()),
        });
        let text = entry.row_text();
        assert!(text.contains("claude"));
        assert!(text.contains("sonnet"));
        assert!(text.contains("5m"));
    }

    #[test]
    fn selected_session_returns_only_for_session_rows() {
        let mut p = PaletteState::default();
        p.open_with_entries(vec![pane("work", "/b", None)]);
        assert!(p.selected_session().is_none());
        assert!(p.selected_pane().is_some());
    }

    #[test]
    fn navigation_clamps() {
        let mut p = PaletteState::default();
        p.open_with_entries(vec![session("/a"), session("/b")]);
        p.move_up(); // already at 0
        assert_eq!(p.selected, 0);
        p.move_down();
        p.move_down(); // past last
        assert_eq!(p.selected, 1);
    }
}
