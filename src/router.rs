//! Read-only helpers for the optional local model-router state files
//! (`~/.claude-router`, `~/.claude-default-profile`).
//!
//! These files are owned by the user's shell launcher (the `claude()` function
//! in the shell profile), which decides whether Claude Code talks to a local
//! model router or straight to Claude. cc-session only *reflects* that state in
//! the status bar so you can see it while browsing; toggling is left to the
//! shell / a slash command. Every helper returns `None` when the files are
//! absent, so the feature stays fully invisible for anyone without that setup.

use std::fs;
use std::path::Path;

fn router_state_at(home: &Path) -> Option<bool> {
    let raw = fs::read_to_string(home.join(".claude-router")).ok()?;
    match raw.trim() {
        "on" => Some(true),
        "off" => Some(false),
        _ => None,
    }
}

fn profile_name_at(home: &Path) -> Option<String> {
    let raw = fs::read_to_string(home.join(".claude-default-profile")).ok()?;
    let name = raw.trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// Current auto-router state: `Some(true)` = on, `Some(false)` = off,
/// `None` when `~/.claude-router` is absent or unrecognized (no router setup).
pub fn router_state() -> Option<bool> {
    router_state_at(&dirs::home_dir()?)
}

/// Default profile label (`work` / `personal`) from `~/.claude-default-profile`,
/// or `None` when the file is absent or empty.
pub fn profile_name() -> Option<String> {
    profile_name_at(&dirs::home_dir()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn router_state_reads_on_off_and_hides_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(router_state_at(tmp.path()), None, "absent file => hidden");

        fs::write(tmp.path().join(".claude-router"), "on\n").unwrap();
        assert_eq!(router_state_at(tmp.path()), Some(true));

        fs::write(tmp.path().join(".claude-router"), "off").unwrap();
        assert_eq!(router_state_at(tmp.path()), Some(false));

        fs::write(tmp.path().join(".claude-router"), "garbage").unwrap();
        assert_eq!(router_state_at(tmp.path()), None, "unrecognized => hidden");
    }

    #[test]
    fn profile_name_reads_value_and_hides_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(profile_name_at(tmp.path()), None);

        fs::write(tmp.path().join(".claude-default-profile"), "work\n").unwrap();
        assert_eq!(profile_name_at(tmp.path()).as_deref(), Some("work"));

        fs::write(tmp.path().join(".claude-default-profile"), "   ").unwrap();
        assert_eq!(profile_name_at(tmp.path()), None, "empty => hidden");
    }
}
