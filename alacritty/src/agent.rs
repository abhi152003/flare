//! AI agent detection.
//!
//! Recognizes when a known AI coding agent is running in a pane's foreground process, by matching
//! the process name (the basename of `/proc/<pid>/cmdline`'s argv[0]) against a set of profiles.
//! This is the foundation for the status-dot UI (#10): detection lives here, display lives there.

use crate::display::color::Rgb;

/// A recognized AI agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKind {
    /// `claude` / `claude-code` (Anthropic).
    ClaudeCode,
    /// `cursor-agent` / `cursor` (Cursor IDE's CLI agent).
    Cursor,
    /// `aider` (the pair-programming CLI).
    Aider,
    /// `codex` (OpenAI Codex CLI).
    Codex,
    /// `gemini` (Google Gemini CLI).
    Gemini,
    /// An agent-like process we don't have a specific profile for yet.
    #[allow(dead_code)] // not produced by cmdline detection; reserved for OSC/title-based detection.
    Unknown,
}

impl AgentKind {
    /// Color used for the tab-bar status dot, keyed by agent kind.
    ///
    /// NOTE: this colors by *which* agent is running, not its state. Agent-state coloring
    /// (blocked/working/done) is a separate concern tracked under #15 (agent status tracking);
    /// when that lands, state-based color should take precedence when state is known.
    pub fn color(self) -> Rgb {
        match self {
            AgentKind::ClaudeCode => Rgb::new(0xE9, 0x63, 0x1A), // orange — Flare accent
            AgentKind::Cursor => Rgb::new(0x5D, 0xAD, 0xEC), // blue
            AgentKind::Aider => Rgb::new(0x38, 0x7B, 0x66), // green
            AgentKind::Codex => Rgb::new(0x6E, 0xAB, 0xC6), // teal
            AgentKind::Gemini => Rgb::new(0xE3, 0xA7, 0x50), // amber
            AgentKind::Unknown => Rgb::new(0x8B, 0xA1, 0xA8), // grey
        }
    }

    /// Short human label shown next to the status dot and in the tab title (e.g. `claude`).
    pub fn label(self) -> &'static str {
        match self {
            AgentKind::ClaudeCode => "claude",
            AgentKind::Cursor => "cursor",
            AgentKind::Aider => "aider",
            AgentKind::Codex => "codex",
            AgentKind::Gemini => "gemini",
            AgentKind::Unknown => "agent",
        }
    }
}

/// Match a foreground process name to a known agent.
///
/// `process_name` is the basename of the foreground process's argv[0] (e.g. `claude`, not
/// `/usr/local/bin/claude`). Returns `None` for shells, editors, and anything that isn't a
/// recognized agent.
///
/// Matching is case-insensitive substring on the process name. The `cursor` profile is safe from
/// collision with the terminal text cursor because this only ever sees real process names
/// (`cursor-agent`, `cursor`), not arbitrary strings.
pub fn detect(process_name: &str) -> Option<AgentKind> {
    let name = process_name.trim().to_lowercase();
    if name.is_empty() {
        return None;
    }

    // Order matters only for specificity; these keys are distinct enough that substring order is
    // not ambiguous in practice.
    let kind = if name.contains("claude") {
        AgentKind::ClaudeCode
    } else if name.contains("cursor") {
        AgentKind::Cursor
    } else if name.contains("aider") {
        AgentKind::Aider
    } else if name.contains("codex") {
        AgentKind::Codex
    } else if name.contains("gemini") {
        AgentKind::Gemini
    } else {
        return None;
    };
    Some(kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_each_known_agent() {
        assert_eq!(detect("claude"), Some(AgentKind::ClaudeCode));
        assert_eq!(detect("claude-code"), Some(AgentKind::ClaudeCode));
        assert_eq!(detect("/usr/local/bin/claude"), Some(AgentKind::ClaudeCode));

        assert_eq!(detect("cursor-agent"), Some(AgentKind::Cursor));
        assert_eq!(detect("cursor"), Some(AgentKind::Cursor));

        assert_eq!(detect("aider"), Some(AgentKind::Aider));

        assert_eq!(detect("codex"), Some(AgentKind::Codex));

        assert_eq!(detect("gemini"), Some(AgentKind::Gemini));
    }

    #[test]
    fn detection_is_case_insensitive() {
        assert_eq!(detect("CLAUDE"), Some(AgentKind::ClaudeCode));
        assert_eq!(detect("Aider"), Some(AgentKind::Aider));
    }

    #[test]
    fn ignores_shells_editors_and_empty() {
        // Common non-agent foreground processes.
        assert_eq!(detect("bash"), None);
        assert_eq!(detect("zsh"), None);
        assert_eq!(detect("fish"), None);
        assert_eq!(detect("vim"), None);
        assert_eq!(detect("nvim"), None);
        assert_eq!(detect("node"), None);
        assert_eq!(detect("python3"), None);
        assert_eq!(detect("git"), None);
        assert_eq!(detect("make"), None);
        // Empty / whitespace.
        assert_eq!(detect(""), None);
        assert_eq!(detect("   "), None);
    }

    #[test]
    fn cursor_profile_only_matches_real_cursor_processes() {
        // The terminal text-cursor concept never reaches detect(); it only sees process names.
        // A bare "cursor" process name is the Cursor agent.
        assert_eq!(detect("cursor"), Some(AgentKind::Cursor));
        // Substrings DO match by design (so "cursor-agent" hits), so "cursors" also matches —
        // that's acceptable: no real non-agent process is named "cursors". The point is that we
        // never feed arbitrary UI strings here.
        assert_eq!(detect("cursors"), Some(AgentKind::Cursor));
        // Unrelated processes that don't contain the key don't false-positive.
        assert_eq!(detect("recursivo"), None);
        assert_eq!(detect("xclock"), None);
    }

    #[test]
    fn color_and_label_cover_every_variant() {
        let kinds = [
            AgentKind::ClaudeCode,
            AgentKind::Cursor,
            AgentKind::Aider,
            AgentKind::Codex,
            AgentKind::Gemini,
            AgentKind::Unknown,
        ];
        // Each variant has a non-empty label.
        assert!(kinds.iter().all(|k| !k.label().is_empty()));
        // Colors are pairwise distinct so dots are visually distinguishable.
        let colors: Vec<Rgb> = kinds.iter().map(|k| k.color()).collect();
        for i in 0..colors.len() {
            for j in (i + 1)..colors.len() {
                assert_ne!(colors[i], colors[j], "colors collide for {:?} and {:?}", kinds[i], kinds[j]);
            }
        }
    }
}
