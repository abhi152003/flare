//! Durable pane addressing (#28).
//!
//! Every pane gets a globally-unique, never-reused numeric id, and is referenceable by a stable
//! address of the form `w<window>:p<pane>` (e.g. `w1:p3`). Ids only ever increment, so closing
//! and reopening panes never collides — and the monotonically rising counters are persisted so
//! the numbering stays stable across app restarts. This is the primitive that the JSON socket
//! (#33) and CLI pane control (#34) will consume.
//!
//! The two independent counters (one for panes, one for windows) are exposed as process-global
//! atomics. [`seed`](id::seed) is called once at startup from the persisted meta file; windows
//! call [`next_window_id`](id::next_window_id) when they open, and every pane gets its id from
//! [`next_pane_id`](id::next_pane_id).

use std::fmt;
use std::str::FromStr;

/// A pane's (or window's) durable numeric identity. Opaque; only meaningful for equality and
/// rendering into an address string.
pub type PaneId = u64;

/// A stable, script-addressable pane reference: `w<window>:p<pane>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct PaneAddress {
    pub window: PaneId,
    pub pane: PaneId,
}

impl PaneAddress {
    pub fn new(window: PaneId, pane: PaneId) -> Self {
        Self { window, pane }
    }
}

/// Serialize an address as its `wN:pM` string form (for JSON IPC replies, #33).
pub fn serialize_address<S: serde::Serializer>(
    address: &PaneAddress,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&address.to_string())
}

impl fmt::Display for PaneAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "w{}:p{}", self.window, self.pane)
    }
}

impl FromStr for PaneAddress {
    type Err = ParseAddressError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.is_empty() {
            return Err(ParseAddressError);
        }

        let (window, pane) = match s.split_once(':') {
            Some((w, p)) => {
                let window = parse_digits(parse_token(w, 'w')?)?;
                let pane = parse_digits(parse_token(p, 'p')?)?;
                if window == 0 || pane == 0 {
                    return Err(ParseAddressError);
                }
                (window, pane)
            },
            // Bare "p3" — current window (window component left 0; caller substitutes its own).
            None => {
                let pane = parse_digits(parse_token(s, 'p')?)?;
                if pane == 0 {
                    return Err(ParseAddressError);
                }
                (0, pane)
            },
        };

        Ok(PaneAddress { window, pane })
    }
}

/// Parse an `w`/`p` token: a single lowercase letter prefix followed by digits.
fn parse_token<'a>(token: &'a str, expected: char) -> Result<&'a str, ParseAddressError> {
    let mut chars = token.chars();
    if chars.next() != Some(expected) {
        return Err(ParseAddressError);
    }
    let rest: String = chars.collect();
    if rest.is_empty() || !rest.chars().all(|c| c.is_ascii_digit()) {
        return Err(ParseAddressError);
    }
    Ok(token)
}

fn parse_digits(token: &str) -> Result<u64, ParseAddressError> {
    let digits = &token[1..];
    digits.parse().map_err(|_| ParseAddressError)
}

/// `w<window>:p<pane>` could not be parsed, or referenced a zero id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseAddressError;

impl fmt::Display for ParseAddressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid pane address (expected e.g. w1:p3)")
    }
}

impl std::error::Error for ParseAddressError {}

/// Process-global durable id allocators.
pub mod id {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_PANE: AtomicU64 = AtomicU64::new(1);
    static NEXT_WINDOW: AtomicU64 = AtomicU64::new(1);

    /// Allocate the next pane id. Strictly increasing; never reused while the counter persists.
    pub fn next_pane_id() -> super::PaneId {
        NEXT_PANE.fetch_add(1, Ordering::Relaxed)
    }

    /// Allocate the next window id.
    pub fn next_window_id() -> super::PaneId {
        NEXT_WINDOW.fetch_add(1, Ordering::Relaxed)
    }

    /// Seed the counters from persisted state so ids stay unique across restarts.
    ///
    /// `next_pane`/`next_window` are the *next* values to hand out (one past the last persisted
    /// max). Later allocations always exceed these, so an id persisted in a session is never
    /// handed to a different pane.
    pub fn seed(next_pane: super::PaneId, next_window: super::PaneId) {
        bump(&NEXT_PANE, next_pane);
        bump(&NEXT_WINDOW, next_window);
    }

    /// Raise `target` to at least `next` (for restore: keep a restored pane's saved id, which
    /// may exceed the current counter if sessions were saved by a newer build).
    fn bump(target: &AtomicU64, next: u64) {
        let mut current = target.load(Ordering::Relaxed);
        while current < next {
            match target.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }

    /// Raise the pane counter so that allocating the next id will be strictly greater than `id`
    /// only when `id` isn't already in use; i.e. guarantee `id` is reserved (already issued).
    pub fn ensure_pane_at_least(id: super::PaneId) {
        // next must be > id so that on the next call we return id+1 (id already handed out).
        bump(&NEXT_PANE, id.saturating_add(1));
    }

    /// Read the current counters as the values to persist.
    pub fn snapshot() -> (super::PaneId, super::PaneId) {
        (NEXT_PANE.load(Ordering::Relaxed), NEXT_WINDOW.load(Ordering::Relaxed))
    }
}

/// Persist the id counters to `flare-meta.toml` (next to the runtime config), so ids stay
/// unique across app restarts. Atomic write, mirroring `session::save`. Best-effort: a failing
/// persist is logged, never fatal.
pub fn persist_ids() {
    let Some(path) = meta_path() else {
        return;
    };
    let (next_pane, next_window) = id::snapshot();
    let body = format!("next_pane_id = {next_pane}\nnext_window_id = {next_window}\n");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("toml.tmp");
    match std::fs::write(&tmp, body).and_then(|_| std::fs::rename(&tmp, &path)) {
        Ok(()) => log::debug!("Persisted pane-id counters to {}", path.display()),
        Err(err) => log::warn!("Failed to persist pane-id counters: {err}"),
    }
}

/// Load the counters from `flare-meta.toml` and seed the allocators. No-op if the file is
/// absent or unreadable.
pub fn load_ids() {
    let Some(path) = meta_path() else {
        return;
    };
    let Ok(body) = std::fs::read_to_string(&path) else {
        return;
    };
    #[derive(serde::Deserialize)]
    struct Meta {
        #[serde(default)]
        next_pane_id: u64,
        #[serde(default)]
        next_window_id: u64,
    }
    let Ok(meta) = toml::from_str::<Meta>(&body) else {
        return;
    };
    id::seed(meta.next_pane_id.max(1), meta.next_window_id.max(1));
}

/// Path to `flare-meta.toml`, co-located with `flare-runtime.toml`.
fn meta_path() -> Option<std::path::PathBuf> {
    let marker = crate::config::runtime_override_path(&[])?;
    marker.parent().map(|parent| parent.join("flare-meta.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn displays_wp_format() {
        assert_eq!(PaneAddress::new(1, 3).to_string(), "w1:p3");
        assert_eq!(PaneAddress::new(12, 34).to_string(), "w12:p34");
    }

    #[test]
    fn parses_full_address() {
        let addr: PaneAddress = "w1:p3".parse().unwrap();
        assert_eq!(addr, PaneAddress::new(1, 3));
        let addr: PaneAddress = "  w12:p34  ".parse().unwrap();
        assert_eq!(addr, PaneAddress::new(12, 34));
    }

    #[test]
    fn parses_bare_pane_as_zero_window() {
        let addr: PaneAddress = "p7".parse().unwrap();
        assert_eq!(addr, PaneAddress::new(0, 7));
    }

    #[test]
    fn parse_round_trips() {
        let addr: PaneAddress = "w9:p15".parse().unwrap();
        assert_eq!(addr.to_string(), "w9:p15");
        assert_eq!(addr.to_string().parse::<PaneAddress>(), Ok(addr));
    }

    #[test]
    fn rejects_garbage() {
        for bad in ["", "w", "p", "w1", "w1:p", "w:p1", "w0:p1", "w1:p0", "p0", "w1:p3x", "w-1:p2", "wp12", "w1p3"] {
            assert!(bad.parse::<PaneAddress>().is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn allocs_are_monotonic() {
        let a = id::next_pane_id();
        let b = id::next_pane_id();
        let c = id::next_pane_id();
        assert!(a < b && b < c);
    }

    #[test]
    fn seed_raises_counters_monotonically() {
        id::ensure_pane_at_least(10_000);
        let next = id::next_pane_id();
        assert!(next > 10_000, "next ({next}) must exceed the seeded id");
        let (next_pane, _) = id::snapshot();
        assert_eq!(next_pane, next + 1);
    }
}
