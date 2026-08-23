//! Read-only helper for the optional claude profile state file
//! (`~/.claude-default-profile`).
//!
//! This file is owned by the user's shell launcher (the `claude()` function
//! in the shell profile). Agent Session only *reflects* the active claude profile
//! in the status bar. Returns `None` when the file is absent, so the indicator
//! stays invisible for anyone without that setup.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

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

/// Known per-session profiles, matching the user's `_claude_run_<name>` shell
/// launchers. Restricting to this set keeps a tagged value safe to interpolate
/// into the resume command as a shell function name.
pub const PROFILES: [&str; 2] = ["work", "personal"];

/// Sidecar mapping session id -> profile, stored alongside the Claude sessions.
fn session_profiles_path(claude_home: &Path) -> PathBuf {
    claude_home.join("agent-session-profiles.json")
}

/// Load the session-id -> profile map. Returns an empty map when the sidecar is
/// absent or unreadable, so a missing/corrupt file never breaks the browser.
pub fn load_session_profiles(claude_home: &Path) -> HashMap<String, String> {
    fs::read_to_string(session_profiles_path(claude_home))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Tag `id` with `profile`, or clear the tag when `profile` is empty. Only the
/// known [`PROFILES`] values are accepted; anything else is rejected.
pub fn save_session_profile(claude_home: &Path, id: &str, profile: &str) -> std::io::Result<()> {
    let mut map = load_session_profiles(claude_home);
    if profile.is_empty() {
        map.remove(id);
    } else if PROFILES.contains(&profile) {
        map.insert(id.to_string(), profile.to_string());
    } else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "unknown profile",
        ));
    }
    let json = serde_json::to_string_pretty(&map).unwrap_or_else(|_| "{}".to_string());
    fs::write(session_profiles_path(claude_home), json)
}

/// Shell launcher for a Claude resume command: the user's `_claude_run_<profile>`
/// function for a tagged session, or plain `claude` (which honors the global
/// default) when untagged or the tag is unknown.
pub fn claude_launcher(profile: Option<&str>) -> String {
    match profile {
        Some(p) if PROFILES.contains(&p) => format!("_claude_run_{p}"),
        _ => "claude".to_string(),
    }
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
