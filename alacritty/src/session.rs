//! Persistent terminal sessions, keyed by project directory.
//!
//! Each saved session records the tab/pane layout and every pane's working directory, so Flare
//! can restore "what I had open" on the next launch. Sessions are filed under a root directory
//! — the first pane's CWD at save time — making them project-scoped without an explicit
//! workspace concept. Storage mirrors the runtime-config override pattern: one TOML file per
//! context under `flare-sessions/` next to `flare-runtime.toml`.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use log::warn;
use serde::{Deserialize, Serialize};

use crate::config;
use crate::tab::{self, PaneNode};

const SESSION_FORMAT_VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SessionState {
    pub version: u32,
    /// Directory this session is filed under (the first pane's CWD at save time).
    pub root: PathBuf,
    /// Unix timestamp (seconds) of the last save; picks the most-recent context.
    pub last_used: u64,
    pub tabs: Vec<TabState>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TabState {
    pub root: PaneNodeState,
}

/// Serializable mirror of [`tab::PaneNode`], preserving the split tree.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
pub enum PaneNodeState {
    Leaf {
        cwd: PathBuf,
    },
    Split {
        #[serde(default = "default_ratio")]
        ratio: f32,
        direction: SplitDirectionState,
        first: Box<PaneNodeState>,
        second: Box<PaneNodeState>,
    },
}

fn default_ratio() -> f32 {
    0.5
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SplitDirectionState {
    Horizontal,
    Vertical,
}

impl From<tab::SplitDirection> for SplitDirectionState {
    fn from(d: tab::SplitDirection) -> Self {
        match d {
            tab::SplitDirection::Horizontal => SplitDirectionState::Horizontal,
            tab::SplitDirection::Vertical => SplitDirectionState::Vertical,
        }
    }
}

impl From<SplitDirectionState> for tab::SplitDirection {
    fn from(d: SplitDirectionState) -> Self {
        match d {
            SplitDirectionState::Horizontal => tab::SplitDirection::Horizontal,
            SplitDirectionState::Vertical => tab::SplitDirection::Vertical,
        }
    }
}

impl SessionState {
    /// Total number of leaf panes across all tabs.
    pub fn pane_count(&self) -> usize {
        self.tabs.iter().map(|t| pane_node_count(&t.root)).sum()
    }

    #[cfg(test)]
    pub fn leaf_cwds(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for tab in &self.tabs {
            collect_leaves(&tab.root, &mut out);
        }
        out
    }
}

fn pane_node_count(node: &PaneNodeState) -> usize {
    match node {
        PaneNodeState::Leaf { .. } => 1,
        PaneNodeState::Split { first, second, .. } => {
            pane_node_count(first) + pane_node_count(second)
        },
    }
}

#[cfg(test)]
fn collect_leaves(node: &PaneNodeState, out: &mut Vec<PathBuf>) {
    match node {
        PaneNodeState::Leaf { cwd } => out.push(cwd.clone()),
        PaneNodeState::Split { first, second, .. } => {
            collect_leaves(first, out);
            collect_leaves(second, out);
        },
    }
}

/// Build a [`SessionState`] from a live tab manager.
///
/// `cwd_of` resolves a pane's working directory from its PTY handle.
pub fn collect<F>(root: PathBuf, tabs: &[tab::Tab], cwd_of: F) -> SessionState
where
    F: Fn(&tab::Pane) -> Option<PathBuf>,
{
    let tab_states =
        tabs.iter().map(|tab| TabState { root: collect_node(&tab.root, &cwd_of) }).collect();

    SessionState { version: SESSION_FORMAT_VERSION, root, last_used: now_secs(), tabs: tab_states }
}

fn collect_node<F>(node: &PaneNode, cwd_of: &F) -> PaneNodeState
where
    F: Fn(&tab::Pane) -> Option<PathBuf>,
{
    match node {
        PaneNode::Leaf(pane) => {
            // Fall back to home so a restore never opens in `/` or fails outright.
            let cwd = cwd_of(pane).or_else(home::home_dir).unwrap_or_default();
            PaneNodeState::Leaf { cwd }
        },
        PaneNode::Split { direction, ratio, first, second } => PaneNodeState::Split {
            ratio: *ratio,
            direction: (*direction).into(),
            first: Box::new(collect_node(first, cwd_of)),
            second: Box::new(collect_node(second, cwd_of)),
        },
    }
}

// Path resolution + persistence.

/// Directory holding per-context session files, next to `flare-runtime.toml`.
#[cfg(not(windows))]
pub fn sessions_dir() -> Option<PathBuf> {
    let marker = config::runtime_override_path(&[])?;
    marker.parent().map(|p| p.join("flare-sessions"))
}

#[cfg(windows)]
pub fn sessions_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("alacritty").join("flare-sessions"))
}

/// Sanitize an absolute path into a flat filename stem: `/home/u/api` -> `home-u-api`.
fn sanitize(path: &Path) -> String {
    let mut s = path
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(seg) => seg.to_str(),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("-");
    if s.is_empty() {
        s = "default".to_string();
    }
    s
}

fn session_file_path(root: &Path) -> Option<PathBuf> {
    Some(sessions_dir()?.join(format!("{}.toml", sanitize(root))))
}

pub fn save(state: &SessionState) -> Result<(), SessionError> {
    let Some(path) = session_file_path(&state.root) else {
        return Err(SessionError::NoConfigHome);
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let serialized = toml::to_string_pretty(state)?;
    std::fs::write(&path, serialized)?;
    log::debug!("Saved session for {} -> {}", state.root.display(), path.display());
    Ok(())
}

pub fn load(root: &Path) -> Option<SessionState> {
    let path = session_file_path(root)?;
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            warn!("Failed to read session file {}: {err}", path.display());
            return None;
        },
    };
    match toml::from_str::<SessionState>(&contents) {
        Ok(state) if state.version == SESSION_FORMAT_VERSION => Some(state),
        Ok(state) => {
            warn!(
                "Ignoring session for {}: unsupported version {} (expected {})",
                root.display(),
                state.version,
                SESSION_FORMAT_VERSION
            );
            None
        },
        Err(err) => {
            warn!("Corrupt session file {}: {err}", path.display());
            None
        },
    }
}

/// A catalog entry — enough to list/choose contexts without loading full trees.
#[derive(Debug, Clone)]
pub struct SessionEntry {
    pub root: PathBuf,
    pub last_used: u64,
    #[allow(dead_code)]
    pub pane_count: usize,
}

/// Enumerate all saved sessions, most-recently-used first.
pub fn list() -> Vec<SessionEntry> {
    let Some(dir) = sessions_dir() else {
        return Vec::new();
    };
    let read = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(err) => {
            warn!("Failed to list sessions dir {}: {err}", dir.display());
            return Vec::new();
        },
    };

    let mut entries = Vec::new();
    for entry in read.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        match std::fs::read_to_string(&path)
            .map_err(SessionError::from)
            .and_then(|s| toml::from_str::<SessionState>(&s).map_err(SessionError::from))
        {
            Ok(state) if state.version == SESSION_FORMAT_VERSION => {
                let pane_count = state.pane_count();
                entries.push(SessionEntry {
                    root: state.root,
                    last_used: state.last_used,
                    pane_count,
                });
            },
            Ok(state) => {
                warn!("Skipping session {}: unsupported version {}", path.display(), state.version)
            },
            Err(err) => warn!("Skipping unreadable session {}: {err}", path.display()),
        }
    }
    entries.sort_by(|a, b| b.last_used.cmp(&a.last_used).then(a.root.cmp(&b.root)));
    entries
}

/// The most-recently-used session, fully loaded.
pub fn most_recent() -> Option<SessionState> {
    let entry = list().into_iter().next()?;
    load(&entry.root)
}

/// Delete the session filed under `root`, if it exists. Idempotent.
#[allow(dead_code)]
pub fn clear(root: &Path) {
    if let Some(path) = session_file_path(root) {
        if let Err(err) = std::fs::remove_file(&path) {
            if err.kind() != std::io::ErrorKind::NotFound {
                warn!("Failed to remove session {}: {err}", path.display());
            }
        }
    }
}

/// Delete every saved session.
#[allow(dead_code)]
pub fn clear_all() {
    let Some(dir) = sessions_dir() else { return };
    if let Err(err) = std::fs::remove_dir_all(&dir) {
        if err.kind() != std::io::ErrorKind::NotFound {
            warn!("Failed to clear sessions dir {}: {err}", dir.display());
        }
    }
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[derive(Debug)]
pub enum SessionError {
    Io(std::io::Error),
    TomlSer(toml::ser::Error),
    TomlDe(toml::de::Error),
    NoConfigHome,
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::Io(e) => write!(f, "io: {e}"),
            SessionError::TomlSer(e) => write!(f, "serialize: {e}"),
            SessionError::TomlDe(e) => write!(f, "deserialize: {e}"),
            SessionError::NoConfigHome => write!(f, "no config home determined"),
        }
    }
}

impl std::error::Error for SessionError {}

impl From<std::io::Error> for SessionError {
    fn from(e: std::io::Error) -> Self {
        SessionError::Io(e)
    }
}
impl From<toml::ser::Error> for SessionError {
    fn from(e: toml::ser::Error) -> Self {
        SessionError::TomlSer(e)
    }
}
impl From<toml::de::Error> for SessionError {
    fn from(e: toml::de::Error) -> Self {
        SessionError::TomlDe(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(cwd: &str) -> PaneNodeState {
        PaneNodeState::Leaf { cwd: PathBuf::from(cwd) }
    }

    fn split(
        dir: SplitDirectionState,
        first: PaneNodeState,
        second: PaneNodeState,
    ) -> PaneNodeState {
        PaneNodeState::Split {
            ratio: 0.5,
            direction: dir,
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    #[test]
    fn round_trips_a_complex_tree() {
        let state = SessionState {
            version: SESSION_FORMAT_VERSION,
            root: PathBuf::from("/home/u/proj"),
            last_used: 123,
            tabs: vec![
                TabState {
                    root: split(
                        SplitDirectionState::Horizontal,
                        leaf("/home/u/proj"),
                        split(SplitDirectionState::Vertical, leaf("/tmp"), leaf("/etc")),
                    ),
                },
                TabState { root: leaf("/home/u") },
            ],
        };

        let s = toml::to_string(&state).unwrap();
        let back: SessionState = toml::from_str(&s).unwrap();
        assert_eq!(back.root, state.root);
        assert_eq!(back.last_used, state.last_used);
        assert_eq!(back.pane_count(), 4);
        assert_eq!(
            back.leaf_cwds(),
            vec![
                PathBuf::from("/home/u/proj"),
                PathBuf::from("/tmp"),
                PathBuf::from("/etc"),
                PathBuf::from("/home/u"),
            ]
        );
    }

    #[test]
    fn sanitizes_paths_into_flat_filenames() {
        assert_eq!(sanitize(Path::new("/home/user/work/api")), "home-user-work-api");
        assert_eq!(sanitize(Path::new("/")), "default");
        assert_eq!(sanitize(Path::new("relative")), "relative");
    }

    #[test]
    fn rejects_unknown_version() {
        let bad = "version = 999\nroot = \"/x\"\nlast_used = 0\ntabs = []\n";
        let parsed: Result<SessionState, _> = toml::from_str(bad);
        assert!(parsed.is_ok());
        assert_ne!(parsed.unwrap().version, SESSION_FORMAT_VERSION);
    }
}
