use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use crate::discovery::get_claude_home;

/// Append a custom-title entry to a session's JSONL file.
pub fn save_custom_title(project_path: &str, session_id: &str, title: &str) -> Result<(), String> {
    let claude_home = get_claude_home();
    let file_path = claude_home
        .join("projects")
        .join(project_path)
        .join(format!("{session_id}.jsonl"));

    append_custom_title(&file_path, session_id, title)
}

fn append_custom_title(path: &Path, session_id: &str, title: &str) -> Result<(), String> {
    let entry = serde_json::json!({
        "type": "custom-title",
        "customTitle": title,
        "sessionId": session_id,
    });

    let needs_newline = std::fs::read(path)
        .map(|bytes| !bytes.is_empty() && !bytes.ends_with(b"\n"))
        .unwrap_or(false);

    let mut file = OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(|e| format!("failed to open session file: {e}"))?;

    if needs_newline {
        file.write_all(b"\n")
            .map_err(|e| format!("failed to write newline: {e}"))?;
    }

    writeln!(file, "{}", entry)
        .map_err(|e| format!("failed to write title: {e}"))
}
