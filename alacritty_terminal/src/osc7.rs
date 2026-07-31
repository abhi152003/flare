//! Stream scanner that extracts the working directory from OSC 7 escape sequences.
//!
//! Shells that support shell-integration emit `\x1b]7;file://<host>/<path>\x07` (or terminated with
//! `ESC \`) whenever their CWD changes. Since the stock `vte` parser swallows OSC 7, this scanner
//! runs on the raw PTY bytes before they reach the parser and reports the path via a callback.
//!
//! The scanner carries state across reads, so a sequence split across two PTY reads is handled.

use std::path::PathBuf;

/// Maximum OSC payload we'll buffer before discarding (guards against a runaway shell).
const MAX_PAYLOAD: usize = 4096;

#[derive(Default)]
pub struct Osc7Scanner {
    state: ScanState,
    /// Accumulates the bytes between the OSC 7 introducer and the terminator.
    payload: Vec<u8>,
}

/// Internal parser state.
#[derive(Default, PartialEq, Eq)]
enum ScanState {
    /// Outside any escape sequence.
    #[default]
    Ground,
    /// Saw ESC.
    Esc,
    /// Saw `ESC ]` — inside an OSC string, code not yet known.
    Osc,
    /// Saw `ESC ] 7` — need ';' to confirm.
    Osc7Pending,
    /// Confirmed OSC 7; collecting payload until terminator.
    Osc7,
    /// Inside an OSC string that is NOT OSC 7; skip until terminator.
    OscOther,
    /// Saw ESC inside an OSC 7 string (potential ST terminator).
    Osc7St,
    /// Saw ESC inside a non-7 OSC string (potential ST terminator).
    OscOtherSt,
}

impl Osc7Scanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk of raw bytes. When a complete OSC 7 sequence is recognized, `on_cwd` is called
    /// with the parsed path.
    pub fn feed<F: FnMut(PathBuf)>(&mut self, bytes: &[u8], mut on_cwd: F) {
        for &b in bytes {
            self.step(b, &mut on_cwd);
        }
    }

    fn step<F: FnMut(PathBuf)>(&mut self, b: u8, on_cwd: &mut F) {
        match self.state {
            ScanState::Ground => {
                if b == 0x1b {
                    self.state = ScanState::Esc;
                }
            },
            ScanState::Esc => match b {
                b']' => self.state = ScanState::Osc,
                _ => self.state = ScanState::Ground,
            },
            ScanState::Osc => match b {
                b'7' => self.state = ScanState::Osc7Pending,
                _ => self.state = ScanState::OscOther,
            },
            ScanState::Osc7Pending => match b {
                b';' => {
                    self.payload.clear();
                    self.state = ScanState::Osc7;
                },
                _ => self.state = ScanState::Ground,
            },
            ScanState::Osc7 => match b {
                0x07 => self.finish(on_cwd),
                0x1b => self.state = ScanState::Osc7St,
                _ => {
                    if self.payload.len() < MAX_PAYLOAD {
                        self.payload.push(b);
                    } else {
                        self.reset();
                    }
                },
            },
            ScanState::OscOther => match b {
                0x07 => self.state = ScanState::Ground,
                0x1b => self.state = ScanState::OscOtherSt,
                _ => (),
            },
            ScanState::Osc7St => match b {
                b'\\' => self.finish(on_cwd),
                b']' => self.state = ScanState::Osc,
                _ => self.state = ScanState::Ground,
            },
            ScanState::OscOtherSt => match b {
                b'\\' => self.state = ScanState::Ground,
                b']' => self.state = ScanState::Osc,
                _ => self.state = ScanState::Ground,
            },
        }
    }

    fn finish<F: FnMut(PathBuf)>(&mut self, on_cwd: &mut F) {
        if !self.payload.is_empty() {
            if let Some(path) = parse_payload(&self.payload) {
                on_cwd(path);
            }
        }
        self.reset();
    }

    fn reset(&mut self) {
        self.state = ScanState::Ground;
        self.payload.clear();
    }
}

/// Parse an OSC 7 payload (`file://host/path` or a bare path) into a `PathBuf`.
fn parse_payload(bytes: &[u8]) -> Option<PathBuf> {
    let s = std::str::from_utf8(bytes).ok()?.trim();
    if s.is_empty() {
        return None;
    }

    let s = s.trim_matches('"');

    if let Some(rest) = s.strip_prefix("file://") {
        // Drop the authority (everything up to the first '/').
        let path_part = rest.find('/').map(|idx| &rest[idx..])?;
        Some(percent_decode(path_part))
    } else {
        Some(PathBuf::from(s))
    }
}

/// Minimal percent-decoding for file URIs (spaces, etc).
fn percent_decode(input: &str) -> PathBuf {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(
                std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""),
                16,
            ) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    PathBuf::from(String::from_utf8_lossy(&out).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn scan_one(input: &[u8]) -> Option<PathBuf> {
        let mut scanner = Osc7Scanner::new();
        let mut result = None;
        scanner.feed(input, |p| result = Some(p));
        result
    }

    #[test]
    fn parses_bel_terminated() {
        let seq = b"\x1b]7;file://localhost/home/user/project\x07";
        assert_eq!(scan_one(seq).as_deref(), Some(Path::new("/home/user/project")));
    }

    #[test]
    fn parses_st_terminated() {
        let seq = b"\x1b]7;file://localhost/tmp\x1b\\";
        assert_eq!(scan_one(seq).as_deref(), Some(Path::new("/tmp")));
    }

    #[test]
    fn ignores_other_osc_codes() {
        assert!(scan_one(b"\x1b]0;my title\x07").is_none());
        assert!(scan_one(b"\x1b]8;;http://example.com\x1b\\").is_none());
    }

    #[test]
    fn handles_split_across_reads() {
        let mut scanner = Osc7Scanner::new();
        let mut result = None;
        scanner.feed(b"\x1b]7;file://local", |_| {});
        scanner.feed(b"host/home/u\x07", |p| result = Some(p));
        assert_eq!(result.as_deref(), Some(Path::new("/home/u")));
    }

    #[test]
    fn percent_decodes_spaces() {
        let seq = b"\x1b]7;file://x/home/user/my%20dir\x07";
        assert_eq!(scan_one(seq).as_deref(), Some(Path::new("/home/user/my dir")));
    }

    #[test]
    fn ignores_non_osc_escapes() {
        assert!(scan_one(b"\x1b[31mhello\x1b[0m").is_none());
    }

    #[test]
    fn handles_multiple_sequences_in_one_stream() {
        let mut scanner = Osc7Scanner::new();
        let mut paths = Vec::new();
        let input = b"\x1b]7;file://h/a\x07some text\x1b]7;file://h/b\x07";
        scanner.feed(input, |p| paths.push(p));
        assert_eq!(paths, vec![PathBuf::from("/a"), PathBuf::from("/b")]);
    }

    #[test]
    fn resets_on_garbage_after_introducer() {
        assert!(scan_one(b"\x1b]x\x07").is_none());
    }

    #[test]
    fn handles_st_terminated_split_across_reads() {
        let mut scanner = Osc7Scanner::new();
        let mut result = None;
        scanner.feed(b"\x1b]7;file://h/a\x1b", |_| {});
        scanner.feed(b"\\", |p| result = Some(p));
        assert_eq!(result.as_deref(), Some(Path::new("/a")));
    }
}
