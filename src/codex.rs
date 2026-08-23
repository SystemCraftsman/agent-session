// Codex session discovery: scanning ~/.codex/sessions/ for rollout JSONL files.
//
// Codex stores each session as a JSONL "rollout" file under
// `~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`. The first line is a
// `session_meta` record carrying the session id (UUID), cwd and timestamp; the
// conversation itself is a stream of `response_item` records whose payload is a
// `message` with a role (`user` / `assistant` / `developer`) and content blocks.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use rayon::prelude::*;
use serde_json::Value;

use crate::session::{
    clean_message, clean_message_multiline, is_meta_message, Agent, ConversationMessage,
    MessageRole, Session,
};

/// Return the Codex home directory.
///
/// Checks the `CODEX_HOME` env var first, then falls back to `~/.codex`.
pub fn get_codex_home() -> PathBuf {
    if let Ok(home) = std::env::var("CODEX_HOME") {
        return PathBuf::from(home);
    }
    dirs::home_dir()
        .expect("could not determine home directory")
        .join(".codex")
}

/// Discover all Codex sessions under `codex_home/sessions/`.
pub fn discover_codex_sessions(codex_home: &Path) -> Vec<Session> {
    discover_codex_in(codex_home, "sessions")
}

/// Discover archived Codex sessions under `codex_home/sessions-archive/`, where
/// [`archive_codex_session`] moves rollouts. Empty when nothing is archived.
pub fn discover_archived_codex_sessions(codex_home: &Path) -> Vec<Session> {
    discover_codex_in(codex_home, "sessions-archive")
}

/// Shared Codex discovery over `codex_home/<subdir>/`, used for both the live
/// (`sessions`) and archived (`sessions-archive`) trees, which share the same
/// date-based rollout layout.
fn discover_codex_in(codex_home: &Path, subdir: &str) -> Vec<Session> {
    let sessions_dir = codex_home.join(subdir);
    if !sessions_dir.is_dir() {
        return Vec::new();
    }

    let mut rollout_files: Vec<PathBuf> = Vec::new();
    collect_rollout_files(&sessions_dir, &mut rollout_files);

    let mut sessions: Vec<Session> = rollout_files
        .par_iter()
        .filter_map(|path| parse_rollout_file(path))
        .collect();

    // Apply custom titles from the sidecar store (rollout files stay untouched).
    let titles = load_codex_titles(codex_home);
    if !titles.is_empty() {
        for s in &mut sessions {
            if let Some(t) = titles.get(&s.id) {
                s.custom_title = Some(t.clone());
            }
        }
    }

    sessions.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    sessions
}

/// Path to the title sidecar for Codex sessions.
///
/// Codex rollout files must stay faithful for `codex resume`, so custom titles
/// live in a sidecar map (`<codex_home>/agent-session-titles.json`) keyed by
/// session id rather than being written into the rollout itself.
fn codex_titles_path(codex_home: &Path) -> PathBuf {
    codex_home.join("agent-session-titles.json")
}

/// Legacy sidecar path from the tool's former `cc-session` name. Read as a
/// fallback so titles saved before the rename are not lost.
fn legacy_codex_titles_path(codex_home: &Path) -> PathBuf {
    codex_home.join("cc-session-titles.json")
}

/// Load the id -> custom title map for Codex sessions. A missing or corrupt file
/// yields an empty map; the legacy `cc-session-titles.json` is read as a
/// fallback when the current sidecar is absent.
pub fn load_codex_titles(codex_home: &Path) -> HashMap<String, String> {
    let read = |p: PathBuf| fs::read_to_string(p).ok();
    match read(codex_titles_path(codex_home))
        .or_else(|| read(legacy_codex_titles_path(codex_home)))
    {
        Some(data) => serde_json::from_str(&data).unwrap_or_default(),
        None => HashMap::new(),
    }
}

/// Persist a custom title for a Codex session. An empty title removes the entry.
pub fn save_codex_title(codex_home: &Path, session_id: &str, title: &str) -> Result<(), String> {
    let mut titles = load_codex_titles(codex_home);
    if title.is_empty() {
        titles.remove(session_id);
    } else {
        titles.insert(session_id.to_string(), title.to_string());
    }
    let serialized = serde_json::to_string_pretty(&titles)
        .map_err(|e| format!("failed to encode titles: {e}"))?;
    fs::write(codex_titles_path(codex_home), serialized)
        .map_err(|e| format!("failed to write titles file: {e}"))
}

/// Move a Codex rollout file into `<codex_home>/sessions-archive/…`, preserving
/// its date-based subpath so it drops out of discovery without being destroyed.
pub fn archive_codex_session(codex_home: &Path, source_path: &str) -> Result<(), String> {
    let src = PathBuf::from(source_path);
    let sessions_dir = codex_home.join("sessions");
    let rel = src
        .strip_prefix(&sessions_dir)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| {
            // Fall back to just the file name if the layout is unexpected.
            PathBuf::from(src.file_name().unwrap_or_default())
        });
    let dst = codex_home.join("sessions-archive").join(rel);
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("failed to create archive dir: {e}"))?;
    }
    fs::rename(&src, &dst).map_err(|e| format!("failed to archive session: {e}"))
}

/// Move an archived Codex rollout back into `<codex_home>/sessions/…`, reversing
/// [`archive_codex_session`] and preserving its date-based subpath. `source_path`
/// is the rollout's current (archived) location.
pub fn restore_codex_session(codex_home: &Path, source_path: &str) -> Result<(), String> {
    let src = PathBuf::from(source_path);
    let archive_dir = codex_home.join("sessions-archive");
    let rel = src
        .strip_prefix(&archive_dir)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| PathBuf::from(src.file_name().unwrap_or_default()));
    let dst = codex_home.join("sessions").join(rel);
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("failed to create sessions dir: {e}"))?;
    }
    fs::rename(&src, &dst).map_err(|e| format!("failed to restore session: {e}"))
}

/// Rewrite the working directory recorded in a Codex rollout so it regroups under
/// `new_cwd`. The `session_meta` and `turn_context` records carry `cwd`; the file
/// stays in its date-based location.
pub fn move_codex_session(source_path: &str, new_cwd: &str) -> Result<(), String> {
    let content =
        fs::read_to_string(source_path).map_err(|e| format!("failed to read session file: {e}"))?;
    let mut updated = String::with_capacity(content.len());
    for line in content.lines() {
        updated.push_str(&rewrite_cwd_line(line, new_cwd));
        updated.push('\n');
    }
    fs::write(source_path, updated).map_err(|e| format!("failed to write session file: {e}"))
}

/// Rewrite `payload.cwd` on `session_meta`/`turn_context` lines. Other lines and
/// any unparseable line pass through unchanged.
fn rewrite_cwd_line(line: &str, new_cwd: &str) -> String {
    let mut val: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return line.to_string(),
    };
    let line_type = val.get("type").and_then(|t| t.as_str()).unwrap_or("");
    if line_type == "session_meta" || line_type == "turn_context" {
        if let Some(payload) = val.get_mut("payload").and_then(|p| p.as_object_mut()) {
            if payload.contains_key("cwd") {
                payload.insert("cwd".to_string(), Value::String(new_cwd.to_string()));
            }
        }
    }
    serde_json::to_string(&val).unwrap_or_else(|_| line.to_string())
}

/// Recursively collect `rollout-*.jsonl` files under `dir`.
fn collect_rollout_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rollout_files(&path, out);
        } else if is_rollout_file(&path) {
            out.push(path);
        }
    }
}

/// Path to the most recently modified rollout file, used as an envelope template
/// when reconstructing a session for Codex.
pub fn newest_rollout_path(codex_home: &Path) -> Option<PathBuf> {
    let sessions_dir = codex_home.join("sessions");
    if !sessions_dir.is_dir() {
        return None;
    }
    let mut files: Vec<PathBuf> = Vec::new();
    collect_rollout_files(&sessions_dir, &mut files);
    files
        .into_iter()
        .filter_map(|p| {
            let modified = fs::metadata(&p).ok()?.modified().ok()?;
            Some((modified, p))
        })
        .max_by_key(|(m, _)| *m)
        .map(|(_, p)| p)
}

/// Whether a path is a Codex rollout JSONL file.
fn is_rollout_file(path: &Path) -> bool {
    if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
        return false;
    }
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.starts_with("rollout-"))
        .unwrap_or(false)
}

/// Whether a Codex user message is an injected context block rather than a real
/// prompt. Codex prepends an `<environment_context>` message whose stripped text
/// (a bare cwd path) survives the generic tag cleaning, so it needs its own check.
fn is_codex_injected(raw_text: &str) -> bool {
    let trimmed = raw_text.trim_start();
    trimmed.starts_with("<environment_context>") || trimmed.starts_with("<user_instructions>")
}

/// Extract concatenated text from a Codex `message` payload's content blocks.
fn message_text(payload: &Value) -> String {
    payload
        .get("content")
        .and_then(|c| c.as_array())
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

/// Parse a single Codex rollout file into a `Session`.
///
/// Returns `None` if the file has no `session_meta` record.
fn parse_rollout_file(path: &Path) -> Option<Session> {
    let file = fs::File::open(path).ok()?;
    let reader = BufReader::new(file);

    let mut id: Option<String> = None;
    let mut cwd = String::new();
    let mut git_branch: Option<String> = None;
    let mut timestamp: DateTime<Utc> = Utc::now();
    let mut first_message = String::new();
    let mut found_meta = false;
    let mut found_message = false;
    let mut line_count: usize = 0;

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        if line.trim().is_empty() {
            continue;
        }

        let value: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let line_type = value.get("type").and_then(|t| t.as_str()).unwrap_or("");

        match line_type {
            "session_meta" => {
                let payload = match value.get("payload") {
                    Some(p) => p,
                    None => continue,
                };
                id = payload
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                cwd = payload
                    .get("cwd")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                git_branch = payload
                    .get("git")
                    .and_then(|g| g.get("branch"))
                    .and_then(|b| b.as_str())
                    .map(|s| s.to_string());
                timestamp = payload
                    .get("timestamp")
                    .and_then(|v| v.as_str())
                    .or_else(|| value.get("timestamp").and_then(|v| v.as_str()))
                    .and_then(parse_timestamp)
                    .unwrap_or_else(Utc::now);
                found_meta = true;
            }
            "response_item" if !found_message => {
                // Cap how deep we scan for the first genuine user prompt.
                line_count += 1;
                if line_count > 100 {
                    continue;
                }
                let payload = match value.get("payload") {
                    Some(p) => p,
                    None => continue,
                };
                if payload.get("type").and_then(|t| t.as_str()) != Some("message") {
                    continue;
                }
                if payload.get("role").and_then(|r| r.as_str()) != Some("user") {
                    continue;
                }
                let raw_text = message_text(payload);
                // Skip Codex's injected <environment_context> / permissions blocks.
                if is_codex_injected(&raw_text) || is_meta_message(&raw_text) {
                    continue;
                }
                let cleaned = clean_message(&raw_text);
                if cleaned.is_empty() {
                    continue;
                }
                first_message = cleaned
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(200)
                    .collect();
                found_message = true;
            }
            _ => {}
        }
    }

    let id = id?;
    if !found_meta {
        return None;
    }

    let project_name = Path::new(&cwd)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    let project_exists = Path::new(&cwd).exists();

    Some(Session {
        id,
        // For Codex, project_path holds the cwd so sessions group by working
        // directory; the rollout file location lives in source_path.
        project_path: cwd.clone(),
        project_name,
        git_branch,
        timestamp,
        first_message,
        cwd,
        project_exists,
        custom_title: None,
        agent: Agent::Codex,
        source_path: Some(path.to_string_lossy().to_string()),
    })
}

/// Parse a timestamp string, tolerating both RFC 3339 and space-separated forms.
fn parse_timestamp(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .ok()
        .or_else(|| s.parse::<DateTime<Utc>>().ok())
}

/// Load all conversation messages (user + assistant) from a Codex rollout file.
///
/// Reads from `session.source_path` (the rollout file). Skips developer and
/// system messages, tool calls, and meta-messages. Consecutive messages from
/// the same role are merged.
pub fn load_codex_conversation(session: &Session) -> Vec<ConversationMessage> {
    let source = match &session.source_path {
        Some(p) => p,
        None => return Vec::new(),
    };
    let file = match fs::File::open(source) {
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
        let value: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if value.get("type").and_then(|t| t.as_str()) != Some("response_item") {
            continue;
        }
        let payload = match value.get("payload") {
            Some(p) => p,
            None => continue,
        };
        if payload.get("type").and_then(|t| t.as_str()) != Some("message") {
            continue;
        }

        let role = match payload.get("role").and_then(|r| r.as_str()) {
            Some("user") => MessageRole::User,
            Some("assistant") => MessageRole::Assistant,
            _ => continue,
        };

        let raw_text = message_text(payload);
        if role == MessageRole::User && (is_codex_injected(&raw_text) || is_meta_message(&raw_text))
        {
            continue;
        }

        let text = clean_message_multiline(&raw_text);
        if text.is_empty() {
            continue;
        }

        let timestamp: DateTime<Utc> = value
            .get("timestamp")
            .and_then(|v| v.as_str())
            .and_then(parse_timestamp)
            .unwrap_or_else(Utc::now);

        if let Some(last) = messages.last_mut() {
            if last.role == role {
                if !last.text.ends_with(&text) {
                    last.text.push_str("\n\n");
                    last.text.push_str(&text);
                }
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
