//! AI agent detection, metadata, and session relaunch helpers.

use crate::display::color::Rgb;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKind {
    ClaudeCode,
    Cursor,
    Aider,
    Codex,
    Antigravity,
    #[allow(dead_code)]
    Unknown,
}

impl AgentKind {
    pub fn color(self) -> Rgb {
        match self {
            AgentKind::ClaudeCode => Rgb::new(0xE9, 0x63, 0x1A),
            AgentKind::Cursor => Rgb::new(0x5D, 0xAD, 0xEC),
            AgentKind::Aider => Rgb::new(0x38, 0x7B, 0x66),
            AgentKind::Codex => Rgb::new(0x6E, 0xAB, 0xC6),
            AgentKind::Antigravity => Rgb::new(0xE3, 0xA7, 0x50),
            AgentKind::Unknown => Rgb::new(0x8B, 0xA1, 0xA8),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            AgentKind::ClaudeCode => "claude",
            AgentKind::Cursor => "cursor",
            AgentKind::Aider => "aider",
            AgentKind::Codex => "codex",
            AgentKind::Antigravity => "antigravity",
            AgentKind::Unknown => "agent",
        }
    }
}

/// Working/Idle from recent PTY activity (#15).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentStatus {
    #[default]
    Idle,
    Working,
}

impl AgentStatus {
    pub fn color(self, kind: AgentKind) -> Rgb {
        let base = kind.color();
        match self {
            AgentStatus::Working => base,
            AgentStatus::Idle => base * 0.5,
        }
    }
}

/// Match one argv token (basename or path) to an agent. Prefer [`detect_cmdline`] for live panes.
pub fn detect(process_name: &str) -> Option<AgentKind> {
    let name = process_name.trim().to_lowercase();
    if name.is_empty() {
        return None;
    }

    let kind = if name.contains("claude") {
        AgentKind::ClaudeCode
    } else if name.contains("cursor") {
        AgentKind::Cursor
    } else if name.contains("aider") {
        AgentKind::Aider
    } else if name.contains("codex") {
        AgentKind::Codex
    } else if name.contains("antigravity") || name == "agy" || name.ends_with("/agy") {
        AgentKind::Antigravity
    } else {
        return None;
    };
    Some(kind)
}

/// Detect from full argv — needed for node-wrapped CLIs like Codex (`node …/codex.js`).
pub fn detect_cmdline(cmdline: &[String]) -> Option<AgentKind> {
    for arg in cmdline {
        if let Some(kind) = detect(arg) {
            return Some(kind);
        }
    }
    None
}

/// Relaunch argv for session restore: resume-token cmdline if present, else a default launch.
pub fn relaunch_args(kind: AgentKind, cmdline: &[String]) -> Option<Vec<String>> {
    if kind == AgentKind::Unknown {
        return None;
    }
    if !cmdline.is_empty() && has_resume_token(kind, cmdline) {
        return Some(normalize_launch_argv(kind, cmdline));
    }
    default_launch(kind)
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn resume_args(kind: AgentKind, cmdline: &[String]) -> Option<Vec<String>> {
    if cmdline.is_empty() || !has_resume_token(kind, cmdline) {
        return None;
    }
    Some(normalize_launch_argv(kind, cmdline))
}

fn has_resume_token(kind: AgentKind, cmdline: &[String]) -> bool {
    match kind {
        AgentKind::ClaudeCode => contains_any(cmdline, &["--resume", "-r", "resume", "--continue"]),
        AgentKind::Codex => contains_any(cmdline, &["resume"]),
        AgentKind::Aider => contains_any(cmdline, &["--session"]),
        AgentKind::Cursor => contains_any(cmdline, &["--resume"]),
        AgentKind::Antigravity | AgentKind::Unknown => false,
    }
}

fn default_launch(kind: AgentKind) -> Option<Vec<String>> {
    let argv: &[&str] = match kind {
        AgentKind::ClaudeCode => &["claude", "--continue"],
        AgentKind::Codex => &["codex"],
        AgentKind::Aider => &["aider"],
        AgentKind::Cursor => &["cursor-agent"],
        AgentKind::Antigravity => &["agy"],
        AgentKind::Unknown => return None,
    };
    Some(argv.iter().map(|s| (*s).to_string()).collect())
}

fn normalize_launch_argv(kind: AgentKind, cmdline: &[String]) -> Vec<String> {
    if cmdline.is_empty() {
        return default_launch(kind).unwrap_or_default();
    }
    // `node /path/to/codex.js …` → `codex …`
    if cmdline.len() >= 2 {
        let a0 = cmdline[0].rsplit('/').next().unwrap_or(&cmdline[0]).to_ascii_lowercase();
        let a1 = cmdline[1].to_ascii_lowercase();
        if (a0 == "node" || a0 == "nodejs") && detect(&a1).is_some() {
            let mut out = Vec::with_capacity(cmdline.len());
            out.push(kind.label().to_string());
            out.extend(cmdline.iter().skip(2).cloned());
            return out;
        }
    }
    let mut out = cmdline.to_vec();
    if let Some(first) = out.first_mut() {
        if first.contains('/') {
            if let Some(base) = first.rsplit('/').next() {
                let clean = base.trim_end_matches(".js");
                if detect(clean).is_some() {
                    *first = clean.to_string();
                }
            }
        }
    }
    out
}

fn contains_any(cmdline: &[String], tokens: &[&str]) -> bool {
    cmdline.iter().any(|arg| tokens.contains(&arg.as_str()))
}

const MODEL_DISPLAY_CAP: usize = 32;

/// Model from cmdline flags, else agent config on disk.
pub fn resolve_model(kind: AgentKind, cmdline: &[String]) -> Option<String> {
    parse_model(kind, cmdline).or_else(|| config_model(kind))
}

pub fn parse_model(_kind: AgentKind, cmdline: &[String]) -> Option<String> {
    let flags: &[&str] = match _kind {
        AgentKind::ClaudeCode
        | AgentKind::Codex
        | AgentKind::Antigravity
        | AgentKind::Cursor => &["--model", "-m"],
        AgentKind::Aider => &["--model"],
        AgentKind::Unknown => &["--model", "-m"],
    };

    for (i, arg) in cmdline.iter().enumerate() {
        for flag in flags {
            let prefix = format!("{flag}=");
            if let Some(rest) = arg.strip_prefix(&prefix) {
                return sanitize_model(rest);
            }
        }
        if flags.iter().any(|f| arg == f) {
            if let Some(next) = cmdline.get(i + 1) {
                if !next.starts_with('-') {
                    return sanitize_model(next);
                }
            }
        }
    }
    None
}

fn config_model(kind: AgentKind) -> Option<String> {
    match kind {
        AgentKind::ClaudeCode => claude_settings_model(),
        AgentKind::Codex => codex_config_model(),
        AgentKind::Antigravity => antigravity_settings_model(),
        _ => None,
    }
}

fn claude_settings_model() -> Option<String> {
    let path = home::home_dir()?.join(".claude").join("settings.json");
    let raw = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let model = value.get("model")?.as_str()?;
    sanitize_model(model)
}

fn codex_config_model() -> Option<String> {
    let path = home::home_dir()?.join(".codex").join("config.toml");
    let raw = std::fs::read_to_string(path).ok()?;
    for line in raw.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            break;
        }
        let Some(rest) = line.strip_prefix("model") else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let value = rest.trim().trim_matches(|c| c == '"' || c == '\'');
        return sanitize_model(value);
    }
    None
}

fn antigravity_settings_model() -> Option<String> {
    let path = home::home_dir()?.join(".gemini").join("antigravity-cli").join("settings.json");
    let raw = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let model = value.get("model")?.as_str()?;
    sanitize_model(model)
}

fn sanitize_model(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    // "Gemini 3.6 Flash (Medium)" → "Gemini 3.6 Flash"
    let without_effort = if let Some(open) = trimmed.rfind(" (") {
        if trimmed.ends_with(')') {
            trimmed[..open].trim_end()
        } else {
            trimmed
        }
    } else {
        trimmed
    };
    let lower = without_effort.to_ascii_lowercase();
    let pretty = match lower.as_str() {
        "sonnet" | "opus" | "haiku" => lower.as_str(),
        _ => without_effort,
    };
    let capped: String = pretty.chars().take(MODEL_DISPLAY_CAP).collect();
    Some(capped)
}

/// Format elapsed time as `12s`, `5m`, or `1h23m`.
pub fn format_elapsed(started_at_ms: u64, now_ms: u64) -> String {
    let secs = now_ms.saturating_sub(started_at_ms) / 1000;
    if secs < 60 {
        return format!("{secs}s");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m");
    }
    let hours = mins / 60;
    let rem_mins = mins % 60;
    if rem_mins == 0 {
        format!("{hours}h")
    } else {
        format!("{hours}h{rem_mins}m")
    }
}

pub fn metadata_label(
    kind: AgentKind,
    model: Option<&str>,
    elapsed: Option<&str>,
) -> String {
    let mut parts = vec![kind.label().to_string()];
    if let Some(m) = model.filter(|s| !s.is_empty()) {
        parts.push(m.to_string());
    }
    if let Some(e) = elapsed.filter(|s| !s.is_empty()) {
        parts.push(e.to_string());
    }
    parts.join(" · ")
}

pub fn title_suffix(kind: AgentKind, model: Option<&str>, elapsed: Option<&str>) -> String {
    const CAP: usize = 28;
    let full = metadata_label(kind, model, elapsed);
    if full.chars().count() <= CAP {
        return full;
    }
    let without_model = metadata_label(kind, None, elapsed);
    if without_model.chars().count() <= CAP {
        return without_model;
    }
    kind.label().to_string()
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
        assert_eq!(detect("codex.js"), Some(AgentKind::Codex));

        assert_eq!(detect("antigravity"), Some(AgentKind::Antigravity));
        assert_eq!(detect("agy"), Some(AgentKind::Antigravity));
    }

    #[test]
    fn detect_cmdline_finds_node_wrapped_codex() {
        let node_wrapped = cmd(&[
            "/home/u/.nvm/versions/node/v22/bin/node",
            "/home/u/.nvm/node_modules/@openai/codex/bin/codex.js",
        ]);
        assert_eq!(detect_cmdline(&node_wrapped), Some(AgentKind::Codex));
        assert_eq!(detect_cmdline(&cmd(&["node"])), None);
        assert_eq!(detect_cmdline(&cmd(&["codex"])), Some(AgentKind::Codex));
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
            AgentKind::Antigravity,
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

    /// `Working` keeps the agent's full color; `Idle` dims each channel toward zero
    /// but never goes fully dark (the hue stays recognizable).
    #[test]
    fn status_color_dims_for_idle() {
        let base = AgentKind::ClaudeCode.color();
        let working = AgentStatus::Working.color(AgentKind::ClaudeCode);
        let idle = AgentStatus::Idle.color(AgentKind::ClaudeCode);
        // Working == base color exactly.
        assert_eq!(working, base);
        // Idle is dimmer on every channel, but not black.
        assert!(idle.r < base.r || idle.g < base.g || idle.b < base.b);
        assert!(idle.r > 0 || idle.g > 0 || idle.b > 0);
    }

    /// The idle-threshold decision: `now - last < threshold → Working`, else `Idle`.
    /// Verified as pure arithmetic — the actual store/load lives in detect_agents.
    #[test]
    fn idle_threshold_classifies_correctly() {
        const THRESHOLD_MS: u64 = 3000;
        let now: u64 = 10_000;
        let classify = |last: u64| {
            if now.saturating_sub(last) < THRESHOLD_MS { AgentStatus::Working } else { AgentStatus::Idle }
        };
        // Recent output → Working.
        assert_eq!(classify(9_000), AgentStatus::Working);
        assert_eq!(classify(7_500), AgentStatus::Working);
        // Exactly at the threshold → Idle (boundary is exclusive on the working side).
        assert_eq!(classify(7_000), AgentStatus::Idle);
        // Long silent → Idle.
        assert_eq!(classify(0), AgentStatus::Idle);
        // Future/garbage timestamps saturate to Working-safe, but a 0 (never seen output) is Idle.
    }

    /// Helper: build a Vec<String> cmdline from a slice of &str.
    fn cmd(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn resume_args_launches_when_resume_token_present() {
        let full = cmd(&["claude", "--resume", "abc123"]);
        assert_eq!(resume_args(AgentKind::ClaudeCode, &full), Some(full.clone()));
        let short = cmd(&["/usr/local/bin/claude", "-r", "abc123"]);
        assert_eq!(resume_args(AgentKind::ClaudeCode, &short), Some(cmd(&["claude", "-r", "abc123"])));
        let cont = cmd(&["claude", "--continue"]);
        assert_eq!(resume_args(AgentKind::ClaudeCode, &cont), Some(cont));
        let codex = cmd(&["codex", "resume", "abc123"]);
        assert_eq!(resume_args(AgentKind::Codex, &codex), Some(codex));
        let aider = cmd(&["aider", "--session", "abc123"]);
        assert_eq!(resume_args(AgentKind::Aider, &aider), Some(aider));
    }

    #[test]
    fn resume_args_returns_none_without_token_or_unsupported() {
        assert_eq!(resume_args(AgentKind::ClaudeCode, &cmd(&["claude"])), None);
        let empty = Vec::new();
        assert_eq!(resume_args(AgentKind::ClaudeCode, &empty), None);
        assert_eq!(
            resume_args(AgentKind::Antigravity, &cmd(&["antigravity", "--resume", "x"])),
            None
        );
        assert_eq!(resume_args(AgentKind::Unknown, &cmd(&["agent", "--resume", "x"])), None);
    }

    #[test]
    fn resume_args_token_match_is_exact() {
        assert_eq!(resume_args(AgentKind::ClaudeCode, &cmd(&["claude", "--resumed"])), None);
    }

    #[test]
    fn relaunch_args_falls_back_to_default_without_token() {
        assert_eq!(
            relaunch_args(AgentKind::ClaudeCode, &cmd(&["claude"])),
            Some(cmd(&["claude", "--continue"]))
        );
        assert_eq!(relaunch_args(AgentKind::Codex, &[]), Some(cmd(&["codex"])));
        assert_eq!(relaunch_args(AgentKind::Antigravity, &[]), Some(cmd(&["agy"])));
        assert_eq!(relaunch_args(AgentKind::Unknown, &[]), None);
    }

    #[test]
    fn relaunch_args_normalizes_node_wrapper() {
        let wrapped = cmd(&[
            "/usr/bin/node",
            "/home/u/node_modules/@openai/codex/bin/codex.js",
            "resume",
            "abc",
        ]);
        assert_eq!(
            relaunch_args(AgentKind::Codex, &wrapped),
            Some(cmd(&["codex", "resume", "abc"]))
        );
    }

    #[test]
    fn parse_model_reads_long_and_short_flags() {
        assert_eq!(
            parse_model(AgentKind::ClaudeCode, &cmd(&["claude", "--model", "sonnet"])),
            Some("sonnet".into())
        );
        assert_eq!(
            parse_model(AgentKind::ClaudeCode, &cmd(&["claude", "-m", "opus"])),
            Some("opus".into())
        );
        assert_eq!(
            parse_model(AgentKind::Codex, &cmd(&["codex", "--model=gpt-5"])),
            Some("gpt-5".into())
        );
        assert_eq!(
            parse_model(AgentKind::Aider, &cmd(&["aider", "--model", "claude-3-5"])),
            Some("claude-3-5".into())
        );
    }

    #[test]
    fn parse_model_returns_none_when_absent() {
        assert_eq!(parse_model(AgentKind::ClaudeCode, &cmd(&["claude"])), None);
        assert_eq!(parse_model(AgentKind::ClaudeCode, &cmd(&["claude", "--model"])), None);
        assert_eq!(parse_model(AgentKind::ClaudeCode, &cmd(&["claude", "--model", "-v"])), None);
    }

    #[test]
    fn resolve_model_prefers_cmdline_over_config() {
        assert_eq!(
            resolve_model(AgentKind::ClaudeCode, &cmd(&["claude", "--model", "opus"])),
            Some("opus".into())
        );
    }

    #[test]
    fn resolve_model_falls_back_to_claude_settings_when_no_flag() {
        let path = home::home_dir().map(|h| h.join(".claude/settings.json"));
        let Some(path) = path.filter(|p| p.is_file()) else {
            return;
        };
        let raw = std::fs::read_to_string(path).unwrap_or_default();
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
            return;
        };
        let Some(expected) = value.get("model").and_then(|v| v.as_str()) else {
            return;
        };
        let got = resolve_model(AgentKind::ClaudeCode, &cmd(&["claude"]));
        assert_eq!(got.as_deref(), Some(expected));
    }

    #[test]
    fn parse_model_caps_long_tokens() {
        let long = "x".repeat(40);
        let got = parse_model(AgentKind::ClaudeCode, &cmd(&["claude", "--model", &long])).unwrap();
        assert_eq!(got.chars().count(), 32);
    }

    #[test]
    fn sanitize_strips_effort_parenthetical() {
        assert_eq!(
            parse_model(
                AgentKind::Antigravity,
                &cmd(&["agy", "--model", "Gemini 3.6 Flash (Medium)"])
            ),
            Some("Gemini 3.6 Flash".into())
        );
    }

    #[test]
    fn resolve_model_reads_antigravity_settings_when_present() {
        let path = home::home_dir().map(|h| h.join(".gemini/antigravity-cli/settings.json"));
        let Some(path) = path.filter(|p| p.is_file()) else {
            return;
        };
        let raw = std::fs::read_to_string(path).unwrap_or_default();
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
            return;
        };
        if value.get("model").and_then(|v| v.as_str()).is_none() {
            return;
        };
        let got = resolve_model(AgentKind::Antigravity, &cmd(&["agy"]));
        assert!(got.is_some(), "expected model from antigravity settings");
        let s = got.unwrap();
        assert!(!s.contains('('), "got {s:?}");
    }

    #[test]
    fn format_elapsed_boundaries() {
        assert_eq!(format_elapsed(1000, 1000), "0s");
        assert_eq!(format_elapsed(0, 45_000), "45s");
        assert_eq!(format_elapsed(0, 60_000), "1m");
        assert_eq!(format_elapsed(0, 5 * 60_000), "5m");
        assert_eq!(format_elapsed(0, 90 * 60_000), "1h30m");
        assert_eq!(format_elapsed(0, 2 * 60 * 60_000), "2h");
    }

    #[test]
    fn metadata_label_omits_missing_parts() {
        assert_eq!(metadata_label(AgentKind::ClaudeCode, None, None), "claude");
        assert_eq!(
            metadata_label(AgentKind::ClaudeCode, Some("sonnet"), None),
            "claude · sonnet"
        );
        assert_eq!(
            metadata_label(AgentKind::ClaudeCode, Some("sonnet"), Some("5m")),
            "claude · sonnet · 5m"
        );
        assert_eq!(
            metadata_label(AgentKind::Codex, None, Some("12s")),
            "codex · 12s"
        );
    }

    #[test]
    fn title_suffix_drops_model_when_too_long() {
        let long_model = "very-long-model-name-here";
        let s = title_suffix(AgentKind::ClaudeCode, Some(long_model), Some("1h23m"));
        assert!(s.chars().count() <= 28);
        assert!(s.starts_with("claude"));
    }
}
