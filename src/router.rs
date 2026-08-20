//! Read-only helper for the optional claude profile state file
//! (`~/.claude-default-profile`).
//!
//! This file is owned by the user's shell launcher (the `claude()` function
//! in the shell profile). Agent Session only *reflects* the active claude profile
//! in the status bar. Returns `None` when the file is absent, so the indicator
//! stays invisible for anyone without that setup.

use std::fs;
use std::path::Path;

fn profile_name_at(home: &Path) -> Option<String> {
    let raw = fs::read_to_string(home.join(".claude-default-profile")).ok()?;
    let name = raw.trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// Claude profile label (`work` / `personal`) from `~/.claude-default-profile`,
/// or `None` when the file is absent or empty.
pub fn profile_name() -> Option<String> {
    profile_name_at(&dirs::home_dir()?)
}

#[cfg(test)]
mod tests {
    use super::*;

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
