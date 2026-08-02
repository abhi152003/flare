//! Automatic shell-integration injection (#22).
//!
//! Flare writes a small per-shell wrapper script at startup that sources Flare's
//! integration hooks and then the user's real rc file, so integration works with
//! zero `.bashrc` editing. The hooks emit OSC 7 on every directory change, letting
//! the terminal track the real working directory (via the existing [`crate::osc7`]
//! scanner) instead of relying on `/proc` heuristics.
//!
//! This is also the foundation for future agent-state reporting (#15 Blocked/Done)
//! and progress reporting (#18): those will add further OSC emissions to the same
//! hooks injected here.

use std::io;
use std::path::{Path, PathBuf};

/// Bash integration hooks. `PROMPT_COMMAND` fires before each prompt, emitting the
/// current directory as OSC 7.
const BASH: &str = "\
# --- Flare shell integration (auto-injected, #22) ---
__flare_osc7_cwd() { printf '\\e]7;file://%s%s\\a' \"$HOSTNAME\" \"$PWD\"; }
PROMPT_COMMAND=\"__flare_osc7_cwd${PROMPT_COMMAND:+;$PROMPT_COMMAND}\"
__flare_osc7_cwd
# --- end Flare shell integration ---\
";

/// Zsh integration hooks. `chpwd` fires on every directory change; we also emit once
/// at startup so the initial cwd is reported.
const ZSH: &str = "\
# --- Flare shell integration (auto-injected, #22) ---
__flare_osc7_cwd() { printf '\\e]7;file://%s%s\\a' \"$HOSTNAME\" \"$PWD\"; }
chpwd_functions=(__flare_osc7_cwd $chpwd_functions)
__flare_osc7_cwd
# --- end Flare shell integration ---\
";

/// Fish integration hooks. Fish emits on the `PWD` variable changing.
const FISH: &str = "\
# --- Flare shell integration (auto-injected, #22) ---
function __flare_osc7_cwd --on-variable PWD
    printf '\\e]7;file://%s%s\\a' \"$hostname\" \"$PWD\"
end
__flare_osc7_cwd
# --- end Flare shell integration ---\
";

/// The recognized shell family, derived from the program basename.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellFamily {
    Bash,
    Zsh,
    Fish,
}

/// Detect the shell family from a program path (e.g. `/usr/bin/zsh` → `Zsh`).
/// Returns `None` for unrecognized shells so injection is skipped (safe default).
pub fn family_for(program: &str) -> Option<ShellFamily> {
    // A leading '-' means a login shell (argv[0] convention); strip it.
    let name = program.trim_start_matches('-');
    // Take the basename.
    let base = name.rsplit('/').next().unwrap_or(name);
    match base {
        "bash" => Some(ShellFamily::Bash),
        "zsh" => Some(ShellFamily::Zsh),
        "fish" => Some(ShellFamily::Fish),
        _ => None,
    }
}

/// The integration hook script body for `family`, if recognized.
pub fn hooks_for(family: ShellFamily) -> &'static str {
    match family {
        ShellFamily::Bash => BASH,
        ShellFamily::Zsh => ZSH,
        ShellFamily::Fish => FISH,
    }
}

/// Write a wrapper init-file for the given shell family and return its path.
///
/// The wrapper sources Flare's integration hooks and then the user's real rc file,
/// so user customizations are preserved. For bash this replaces `~/.bashrc` as the
/// init file (via `--init-file`), so sourcing the real rc here is mandatory.
///
/// The file is written under the system temp dir with a `flare-shell-integration-`
/// prefix and restrictive permissions. It is not cleaned up (best-effort; temp
/// files vanish on reboot).
pub fn write_wrapper(family: ShellFamily, user_home: &Path) -> io::Result<PathBuf> {
    let hooks = hooks_for(family);
    let rc_source = real_rc_source(family, user_home);

    // Build the wrapper: hooks first, then the user's real rc.
    let body = match family {
        // Bash reads the wrapper as its init file (--init-file), so we must source
        // the real .bashrc ourselves to preserve user config.
        ShellFamily::Bash => format!("{hooks}\n\n# Source the user's real bashrc.\n{rc_source}\n"),
        // zsh: the wrapper is sourced via ZDOTDIR/.zshrc; source the real one too.
        ShellFamily::Zsh => format!("{hooks}\n\n# Source the user's real zshrc.\n{rc_source}\n"),
        // fish: conf.d snippet; fish auto-sources the real config independently.
        ShellFamily::Fish => format!("{hooks}\n"),
    };

    let pid = std::process::id();

    // zsh sources `$ZDOTDIR/.zshrc`, so the wrapper must live in a dedicated dir
    // named exactly `.zshrc` (a bare temp file won't be picked up). bash and fish
    // read an explicit path, so a flat temp file suffices.
    let path = if matches!(family, ShellFamily::Zsh) {
        let dir = std::env::temp_dir().join(format!("flare-shell-integration-{pid}"));
        std::fs::create_dir_all(&dir)?;
        dir.join(".zshrc")
    } else {
        let ext = match family {
            ShellFamily::Bash => "sh",
            ShellFamily::Fish => "fish",
            ShellFamily::Zsh => "zsh", // unreachable here, but keeps the match exhaustive
        };
        std::env::temp_dir().join(format!("flare-shell-integration-{pid}.{ext}"))
    };

    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;
        file.write_all(body.as_bytes())?;
    }

    // Restrict permissions to the owner (integration may reference $HOME paths).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }

    Ok(path)
}

/// The shell command to source the user's real rc file, preserving their config.
fn real_rc_source(family: ShellFamily, user_home: &Path) -> String {
    let rc = match family {
        ShellFamily::Bash => user_home.join(".bashrc"),
        ShellFamily::Zsh => user_home.join(".zshrc"),
        ShellFamily::Fish => return String::new(), // fish auto-sources its config
    };
    // Guard with [ -f ] so a missing rc is silently skipped (no error spam).
    format!("[ -f \"{rc}\" ] && source \"{rc}\"", rc = rc.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_known_shells_by_basename() {
        assert_eq!(family_for("bash"), Some(ShellFamily::Bash));
        assert_eq!(family_for("/usr/bin/bash"), Some(ShellFamily::Bash));
        assert_eq!(family_for("/usr/local/bin/zsh"), Some(ShellFamily::Zsh));
        assert_eq!(family_for("fish"), Some(ShellFamily::Fish));
        // Login-shell argv[0] convention (leading dash) still detects.
        assert_eq!(family_for("-zsh"), Some(ShellFamily::Zsh));
    }

    #[test]
    fn unknown_shells_return_none() {
        assert_eq!(family_for("sh"), None);
        assert_eq!(family_for("dash"), None);
        assert_eq!(family_for("/bin/nushell"), None);
        assert_eq!(family_for(""), None);
    }

    #[test]
    fn hooks_emit_well_formed_osc7() {
        for family in [ShellFamily::Bash, ShellFamily::Zsh, ShellFamily::Fish] {
            let hooks = hooks_for(family);
            // Every family must emit the OSC 7 sequence with the file:// scheme.
            assert!(hooks.contains("\\e]7;file://"), "{family:?} hooks missing OSC 7");
        }
    }

    #[test]
    fn bash_wrapper_sources_real_bashrc() {
        let home = Path::new("/home/test");
        let path = write_wrapper(ShellFamily::Bash, home).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        // The wrapper must contain the hooks…
        assert!(body.contains("__flare_osc7_cwd"), "hooks missing from wrapper");
        // …AND source the user's real .bashrc (the #1 correctness requirement,
        // since --init-file replaces .bashrc).
        assert!(body.contains(".bashrc"), "wrapper must source real .bashrc");
        assert!(body.contains("/home/test/.bashrc"), "wrapper must use the real home path");
    }

    #[test]
    fn zsh_wrapper_sources_real_zshrc() {
        let home = Path::new("/home/test");
        let path = write_wrapper(ShellFamily::Zsh, home).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        assert!(body.contains("__flare_osc7_cwd"));
        assert!(body.contains(".zshrc"));
    }

    #[test]
    fn fish_wrapper_has_hooks_without_rc_source() {
        let home = Path::new("/home/test");
        let path = write_wrapper(ShellFamily::Fish, home).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        // Fish auto-sources its own config, so the wrapper carries only the hooks.
        assert!(body.contains("__flare_osc7_cwd"));
        assert!(!body.contains("source"), "fish wrapper should not manually source rc");
    }
}
