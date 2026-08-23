// Session discovery: scanning ~/.claude/projects/ for session JSONL files

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use rayon::prelude::*;

use crate::session::{
    clean_message, clean_message_multiline, is_meta_message, Agent, ConversationMessage,
    MessageRole, Session, SessionFileEntry,
};

/// Return the Claude home directory.
///
/// Checks the `CLAUDE_HOME` env var first, then falls back to `~/.claude`.
pub fn get_claude_home() -> PathBuf {
    if let Ok(home) = std::env::var("CLAUDE_HOME") {
        return PathBuf::from(home);
    }
    dirs::home_dir()
        .expect("could not determine home directory")
        .join(".claude")
}

/// Discover all sessions under `claude_home/projects/`.
pub fn discover_sessions(claude_home: &Path) -> Vec<Session> {
    discover_sessions_in(claude_home, "projects")
}

/// Discover archived sessions under `claude_home/projects-archive/`, the
/// directory [`archive`](crate::tui) moves sessions into. Empty when nothing has
/// been archived.
pub fn discover_archived_sessions(claude_home: &Path) -> Vec<Session> {
    discover_sessions_in(claude_home, "projects-archive")
}

/// Shared session discovery over `claude_home/<subdir>/`, used for both the live
/// (`projects`) and archived (`projects-archive`) directories, which share the
/// same `<encoded-project>/<id>.jsonl` layout.
fn discover_sessions_in(claude_home: &Path, subdir: &str) -> Vec<Session> {
    let projects_dir = claude_home.join(subdir);
    if !projects_dir.is_dir() {
        return Vec::new();
    }

    // Collect all .jsonl file paths
    let mut jsonl_files: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = fs::read_dir(&projects_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Ok(files) = fs::read_dir(&path) {
                    for file in files.flatten() {
                        let fpath = file.path();
                        if fpath.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                            jsonl_files.push(fpath);
                        }
                    }
                }
            }
        }
    }

    // Parse files in parallel
    let mut sessions: Vec<Session> = jsonl_files
        .par_iter()
        .filter_map(|path| parse_session_file(path))
        .collect();

    // Sort by timestamp descending (newest first)
    sessions.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    sessions
}

/// Decode a Claude project directory name to a real filesystem path.
///
/// Claude encodes paths by replacing '/' with '-', e.g.:
///   `-Users-abulgu-github-repos-mabulgu-cluster-baremetal-operator`
/// becomes:
///   `/Users/abulgu/github-repos/mabulgu/cluster-baremetal-operator`
///
/// Uses filesystem probing to handle dashes in actual directory names.
fn decode_encoded_dir(encoded: &str) -> String {
    let trimmed = encoded.trim_start_matches('-');
    let parts: Vec<&str> = trimmed.split('-').collect();
    let mut path = PathBuf::from("/");
    let mut i = 0;

    while i < parts.len() {
        let mut found = false;
        for j in (i + 1..=parts.len()).rev() {
            let candidate = parts[i..j].join("-");
            let full = path.join(&candidate);
            if full.is_dir() {
                path = full;
                i = j;
                found = true;
                break;
            }
        }
        if !found {
            path = path.join(parts[i]);
            i += 1;
        }
    }

    path.to_string_lossy().to_string()
}

/// Parse a single JSONL session file and extract the first user message.
fn parse_session_file(path: &Path) -> Option<Session> {
    let session_id = path.file_stem()?.to_str()?.to_string();

    let encoded_dir = path.parent()?.file_name()?.to_str()?.to_string();

    let file = fs::File::open(path).ok()?;
    let reader = BufReader::new(file);

    let mut cwd = String::new();
    let mut git_branch: Option<String> = None;
    let mut timestamp: DateTime<Utc> = Utc::now();
    let mut first_message = String::new();
    let mut found_metadata = false;
    let mut found_message = false;
    let mut custom_title: Option<String> = None;
    let mut line_count: usize = 0;

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        if line.trim().is_empty() {
            continue;
        }

        if line.contains("\"custom-title\"") {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&line) {
                if val.get("type").and_then(|v| v.as_str()) == Some("custom-title") {
                    custom_title = val
                        .get("customTitle")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                }
            }
            continue;
        }

        if found_message {
            continue;
        }
        line_count += 1;
        if line_count > 50 {
            continue;
        }

        let entry: SessionFileEntry = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(_) => continue,
        };

        if entry.entry_type != "user" {
            continue;
        }

        if !found_metadata {
            cwd = entry.cwd.clone().unwrap_or_default();
            git_branch = entry.git_branch.clone();
            timestamp = entry
                .timestamp
                .as_deref()
                .and_then(|t| t.parse().ok())
                .unwrap_or_else(Utc::now);
            found_metadata = true;
        }

        let raw_text = entry.message.map(|m| m.content.text()).unwrap_or_default();
        if is_meta_message(&raw_text) {
            continue;
        }

        let cleaned = clean_message(&raw_text);
        first_message = cleaned
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(200)
            .collect();
        found_message = true;
    }

    if !found_metadata {
        return None;
    }

    let project_name = {
        let decoded = decode_encoded_dir(&encoded_dir);
        let decoded_path = Path::new(&decoded);
        if decoded_path.exists() {
            decoded_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .to_string()
        } else {
            Path::new(&cwd)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .to_string()
        }
    };

    let project_exists = Path::new(&cwd).exists();

    Some(Session {
        id: session_id,
        project_path: encoded_dir,
        project_name,
        git_branch,
        timestamp,
        first_message,
        cwd,
        project_exists,
        custom_title,
        agent: Agent::Claude,
        source_path: None,
    })
}

/// Load all conversation messages (user + assistant) from a session JSONL file.
///
/// Returns messages in chronological order. Skips file-history-snapshot entries,
/// system entries, tool-use blocks, and meta-messages. Consecutive messages from
/// the same role are merged into a single message with paragraphs separated by
/// blank lines.
pub fn load_conversation(claude_home: &Path, session: &Session) -> Vec<ConversationMessage> {
    let rel = Path::new(&session.project_path).join(format!("{}.jsonl", session.id));
    let live_path = claude_home.join("projects").join(&rel);
    // Fall back to the archive directory so archived sessions remain viewable.
    let file_path = if live_path.exists() {
        live_path
    } else {
        claude_home.join("projects-archive").join(&rel)
    };

    let file = match fs::File::open(&file_path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };

    let reader = BufReader::new(file);
    let mut messages: Vec<ConversationMessage> = Vec::new();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        if line.trim().is_empty() {
            continue;
        }
        let entry: SessionFileEntry = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(_) => continue,
        };

        let role = match entry.entry_type.as_str() {
            "user" => MessageRole::User,
            "assistant" => MessageRole::Assistant,
            _ => continue,
        };

        let raw_text = match &entry.message {
            Some(m) => m.content.text(),
            None => continue,
        };

        // Skip meta messages for user entries
        if role == MessageRole::User && is_meta_message(&raw_text) {
            continue;
        }

        let text = clean_message_multiline(&raw_text);
        if text.is_empty() {
            continue;
        }

        let timestamp: DateTime<Utc> = entry
            .timestamp
            .and_then(|t| t.parse().ok())
            .unwrap_or_else(Utc::now);

        // Merge consecutive messages from the same role, skipping duplicates
        if let Some(last) = messages.last_mut() {
            if last.role == role {
                // Skip if the text is a duplicate of the last segment
                // (happens with skill expansions that get repeated in JSONL)
                if !last.text.ends_with(&text) {
                    last.text.push_str("\n\n");
                    last.text.push_str(&text);
                }
                // Keep the latest timestamp
                last.timestamp = timestamp;
                continue;
            }
        }

        messages.push(ConversationMessage {
            role,
            text,
            timestamp,
        });
    }

    messages
}

/// Apply optional time-based and count-based filters to a session list.
///
/// `since` keeps only sessions newer than `Utc::now() - since`.
/// `last` keeps only the first N sessions (already sorted newest-first).
pub fn apply_filters(
    mut sessions: Vec<Session>,
    since: Option<chrono::Duration>,
    last: Option<usize>,
) -> Vec<Session> {
    if let Some(duration) = since {
        let cutoff = Utc::now() - duration;
        sessions.retain(|s| s.timestamp >= cutoff);
    }
    if let Some(n) = last {
        sessions.truncate(n);
    }
    sessions
}
