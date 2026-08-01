//! Path display utilities shared across tab titles and the palette.

use std::path::Path;

/// Compact a path for display: replace the home prefix with `~`, and if still long keep only the
/// last few segments.
pub(crate) fn shorten_path(path: &Path) -> String {
    let s = match home::home_dir() {
        Some(home) if path.starts_with(&home) => {
            format!("~/{}", path.strip_prefix(&home).unwrap_or(Path::new("")).display())
        },
        _ => path.display().to_string(),
    };

    const MAX_LEN: usize = 40;
    if s.len() <= MAX_LEN {
        return s;
    }

    let segments: Vec<&str> = s.split('/').filter(|p| !p.is_empty()).collect();
    if segments.len() <= 3 {
        return s;
    }
    let tail = segments[segments.len() - 3..].join("/");
    format!("…/{}", tail)
}
