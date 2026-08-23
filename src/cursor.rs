// Cursor CLI (cursor-agent) session discovery.
//
// cursor-agent mirrors every chat to a JSONL "transcript" at
// `~/.cursor/projects/<encoded-cwd>/agent-transcripts/<chatId>/<chatId>.jsonl`.
// Each line is `{"role":"user"|"assistant","message":{"content":[block,...]}}`
// where a block is `{"type":"text","text":...}` (other block types such as
// `tool_use` are ignored). User turns are wrapped in `<user_query>...`; other
// user lines (`<external_links>`, `<user_info>`, runtime notes) are injected
// context and skipped.
//
// The canonical chat store is a content-addressed SQLite blob DB under
// `~/.cursor/chats/<md5(cwd)>/<chatId>/store.db`, but resumes are served from
// Cursor's cloud keyed by chat id, so the local JSONL transcript is the only
// reliable *read* source and is what we use here.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use rayon::prelude::*;
use serde_json::Value;

use crate::session::{
    clean_message, clean_message_multiline, Agent, ConversationMessage, MessageRole, Session,
};

/// Resolve the Cursor home directory: `CURSOR_HOME` if set, else `~/.cursor`.
pub fn get_cursor_home() -> PathBuf {
    if let Ok(home) = std::env::var("CURSOR_HOME") {
        return PathBuf::from(home);
    }
    dirs::home_dir()
        .expect("could not determine home directory")
        .join(".cursor")
}

/// Discover all Cursor sessions under `cursor_home/projects/`.
pub fn discover_cursor_sessions(cursor_home: &Path) -> Vec<Session> {
    discover_cursor_in(cursor_home, "agent-transcripts")
}

/// Discover archived Cursor sessions from each project's
/// `agent-transcripts-archive/` directory, where [`archive_cursor_session`]
/// moves chats. Empty when nothing is archived.
pub fn discover_archived_cursor_sessions(cursor_home: &Path) -> Vec<Session> {
    discover_cursor_in(cursor_home, "agent-transcripts-archive")
}

/// Shared Cursor discovery over each project's `<subdir>/<chatId>/` directory,
/// used for both the live (`agent-transcripts`) and archived
/// (`agent-transcripts-archive`) trees, which share the same layout.
fn discover_cursor_in(cursor_home: &Path, subdir: &str) -> Vec<Session> {
    let projects_dir = cursor_home.join("projects");
    if !projects_dir.is_dir() {
        return Vec::new();
    }

    // Collect every `<chatId>.jsonl` transcript under any project's
    // `<subdir>/<chatId>/` directory.
    let mut transcripts: Vec<PathBuf> = Vec::new();
    if let Ok(projects) = fs::read_dir(&projects_dir) {
        for proj in projects.flatten() {
            let ppath = proj.path();
            if !ppath.is_dir() {
                continue;
            }
            let transcripts_dir = ppath.join(subdir);
            let Ok(chats) = fs::read_dir(&transcripts_dir) else {
                continue;
            };
            for chat in chats.flatten() {
                let cpath = chat.path();
                if !cpath.is_dir() {
                    continue;
                }
                let Some(chat_id) = cpath.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                let jsonl = cpath.join(format!("{chat_id}.jsonl"));
                if jsonl.is_file() {
                    transcripts.push(jsonl);
                }
            }
        }
    }

    let titles = load_cursor_titles(cursor_home);

    let mut sessions: Vec<Session> = transcripts
        .par_iter()
        .filter_map(|path| parse_transcript_file(path))
        .collect();

    for s in &mut sessions {
        if let Some(t) = titles.get(&s.id) {
            s.custom_title = Some(t.clone());
        }
    }

    sessions.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    sessions
}

/// Path to the Cursor title sidecar (Cursor keeps titles server-side, so we
/// store custom titles locally like Codex).
fn cursor_titles_path(cursor_home: &Path) -> PathBuf {
    cursor_home.join("agent-session-titles.json")
}

/// Legacy sidecar path from the tool's former `cc-session` name. Read as a
/// fallback so titles saved before the rename are not lost.
fn legacy_cursor_titles_path(cursor_home: &Path) -> PathBuf {
    cursor_home.join("cc-session-titles.json")
}

/// Load the id -> custom-title map, or an empty map on any error. The legacy
/// `cc-session-titles.json` is read as a fallback when the current sidecar is
/// absent.
pub fn load_cursor_titles(cursor_home: &Path) -> HashMap<String, String> {
    let read = |p: PathBuf| fs::read_to_string(p).ok();
    match read(cursor_titles_path(cursor_home))
        .or_else(|| read(legacy_cursor_titles_path(cursor_home)))
    {
        Some(data) => serde_json::from_str(&data).unwrap_or_default(),
        None => HashMap::new(),
    }
}

/// Persist (or clear) the custom title for a Cursor session.
pub fn save_cursor_title(cursor_home: &Path, chat_id: &str, title: &str) -> Result<(), String> {
    let mut titles = load_cursor_titles(cursor_home);
    if title.is_empty() {
        titles.remove(chat_id);
    } else {
        titles.insert(chat_id.to_string(), title.to_string());
    }
    let serialized = serde_json::to_string_pretty(&titles)
        .map_err(|e| format!("failed to encode cursor titles: {e}"))?;
    fs::write(cursor_titles_path(cursor_home), serialized)
        .map_err(|e| format!("failed to write cursor titles file: {e}"))
}

/// Archive a Cursor session by moving its `<chatId>/` transcript directory into
/// `<cursor_home>/projects/<encoded>/agent-transcripts-archive/`, so it drops
/// out of discovery without being destroyed.
pub fn archive_cursor_session(source_path: &str) -> Result<(), String> {
    let jsonl = PathBuf::from(source_path);
    // .../agent-transcripts/<chatId>/<chatId>.jsonl -> chat dir, transcripts dir
    let chat_dir = jsonl
        .parent()
        .ok_or("cursor transcript missing chat directory")?;
    let transcripts_dir = chat_dir
        .parent()
        .ok_or("cursor transcript missing agent-transcripts directory")?;
    let proj_dir = transcripts_dir
        .parent()
        .ok_or("cursor transcript missing project directory")?;

    let archive_dir = proj_dir.join("agent-transcripts-archive");
    move_chat_dir(chat_dir, &archive_dir)
}

/// Move a Cursor chat directory into `dest_parent`, disambiguating with a numeric
/// suffix when a chat of the same id already lives there (e.g. a stale archived
/// stub), and keeping the inner `<name>.jsonl` transcript in sync with the final
/// directory name so discovery, which keys off the directory name, still finds
/// it. Non-destructive: an existing chat directory is never overwritten.
fn move_chat_dir(chat_dir: &Path, dest_parent: &Path) -> Result<(), String> {
    fs::create_dir_all(dest_parent).map_err(|e| format!("failed to create cursor dir: {e}"))?;
    let chat_name = chat_dir
        .file_name()
        .ok_or("cursor transcript missing chat id")?
        .to_string_lossy()
        .to_string();

    let mut final_name = chat_name.clone();
    let mut dst = dest_parent.join(&final_name);
    let mut n = 2;
    while dst.exists() {
        final_name = format!("{chat_name}-{n}");
        dst = dest_parent.join(&final_name);
        n += 1;
    }

    fs::rename(chat_dir, &dst).map_err(|e| format!("failed to move cursor session: {e}"))?;

    // If we had to suffix the directory, rename the inner transcript to match so
    // the directory name and `<name>.jsonl` stay consistent for discovery.
    if final_name != chat_name {
        let old_jsonl = dst.join(format!("{chat_name}.jsonl"));
        let new_jsonl = dst.join(format!("{final_name}.jsonl"));
        if old_jsonl.exists() {
            fs::rename(&old_jsonl, &new_jsonl)
                .map_err(|e| format!("failed to rename cursor transcript: {e}"))?;
        }
    }
    Ok(())
}

/// Move an archived Cursor chat directory back into the live
/// `agent-transcripts/` directory, reversing [`archive_cursor_session`].
/// `source_path` is the archived transcript's `<chatId>.jsonl` path.
pub fn restore_cursor_session(source_path: &str) -> Result<(), String> {
    let jsonl = PathBuf::from(source_path);
    // .../agent-transcripts-archive/<chatId>/<chatId>.jsonl -> chat dir, archive dir
    let chat_dir = jsonl
        .parent()
        .ok_or("cursor transcript missing chat directory")?;
    let archive_dir = chat_dir
        .parent()
        .ok_or("cursor transcript missing agent-transcripts-archive directory")?;
    let proj_dir = archive_dir
        .parent()
        .ok_or("cursor transcript missing project directory")?;

    let live_dir = proj_dir.join("agent-transcripts");
    move_chat_dir(chat_dir, &live_dir)
}

/// Move a Cursor session to a different project directory. Because Cursor
/// derives a chat's cwd from its `projects/<encoded>` parent, a "move" relocates
/// the chat's transcript directory under the target project's encoding.
pub fn move_cursor_session(source_path: &str, new_cwd: &str) -> Result<(), String> {
    let jsonl = PathBuf::from(source_path);
    let chat_dir = jsonl
        .parent()
        .ok_or("cursor transcript missing chat directory")?;
    let transcripts_dir = chat_dir
        .parent()
        .ok_or("cursor transcript missing agent-transcripts directory")?;
    let proj_dir = transcripts_dir
        .parent()
        .ok_or("cursor transcript missing project directory")?;
    let projects_root = proj_dir
        .parent()
        .ok_or("cursor transcript missing projects root")?;
    let chat_name = chat_dir
        .file_name()
        .ok_or("cursor transcript missing chat id")?;

    let target_dir = projects_root
        .join(encode_cursor_dir(new_cwd))
        .join("agent-transcripts");
    fs::create_dir_all(&target_dir)
        .map_err(|e| format!("failed to create cursor target dir: {e}"))?;
    let dst = target_dir.join(chat_name);
    fs::rename(chat_dir, &dst).map_err(|e| format!("failed to move cursor session: {e}"))
}

/// Encode a cwd into a Cursor project directory name: strip the leading `/`,
/// then replace `/` and `.` with `-`.
pub fn encode_cursor_dir(cwd: &str) -> String {
    cwd.trim_start_matches('/').replace(['/', '.'], "-")
}

/// Decode a Cursor `projects/<encoded>` directory name back into an absolute
/// path. Dashes are ambiguous (they encode `/`, `.` and literal `-`), so probe
/// the filesystem greedily, mirroring the Claude discovery decoder.
fn decode_encoded_dir(encoded: &str) -> String {
    // Cursor strips the leading slash, so restore it before probing.
    let mut path = String::from("/");
    let mut current = String::new();

    for part in encoded.split('-') {
        if current.is_empty() {
            current = part.to_string();
        } else {
            // Try treating this dash as a path separator first.
            let as_dir = format!("{path}{current}");
            if Path::new(&as_dir).is_dir() {
                path = format!("{as_dir}/");
                current = part.to_string();
            } else {
                // Otherwise fold it back into the current component (as `-`).
                current = format!("{current}-{part}");
            }
        }
    }
    format!("{path}{current}")
}

/// Parse a Cursor transcript file into a `Session`. Returns `None` when the
/// file has no readable chat id or contains no messages.
fn parse_transcript_file(path: &Path) -> Option<Session> {
    let chat_id = path.file_stem()?.to_str()?.to_string();

    // .../projects/<encoded>/agent-transcripts/<chatId>/<chatId>.jsonl
    let encoded_dir = path
        .parent()? // <chatId>/
        .parent()? // agent-transcripts/
        .parent()? // <encoded>/
        .file_name()?
        .to_str()?
        .to_string();
    let cwd = decode_encoded_dir(&encoded_dir);

    let file = fs::File::open(path).ok()?;
    let reader = BufReader::new(file);

    let mut first_message = String::new();
    let mut saw_message = false;

    for line in reader.lines() {
        let Ok(line) = line else { continue };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let role = value.get("role").and_then(|r| r.as_str()).unwrap_or("");
        let raw = collect_text(&value);
        if raw.is_empty() {
            continue;
        }
        saw_message = true;
        if first_message.is_empty() && role == "user" {
            if let Some(q) = user_query(&raw) {
                let cleaned = clean_message(&q);
                if !cleaned.is_empty() {
                    first_message = cleaned.chars().take(200).collect();
                }
            }
        }
    }

    if !saw_message {
        return None;
    }

    let timestamp: DateTime<Utc> = fs::metadata(path)
        .and_then(|m| m.modified())
        .map(DateTime::<Utc>::from)
        .unwrap_or_else(|_| Utc::now());

    let project_name = Path::new(&cwd)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();
    let project_exists = Path::new(&cwd).exists();

    Some(Session {
        id: chat_id,
        // Group Cursor sessions by cwd, matching the Codex convention.
        project_path: cwd.clone(),
        project_name,
        git_branch: None,
        timestamp,
        first_message,
        cwd,
        project_exists,
        custom_title: None,
        agent: Agent::Cursor,
        source_path: Some(path.to_string_lossy().to_string()),
    })
}

/// Concatenate all `text` blocks in a transcript line's `message.content`.
fn collect_text(value: &Value) -> String {
    let content = match value.get("message").and_then(|m| m.get("content")) {
        Some(c) => c,
        None => return String::new(),
    };
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    let Some(blocks) = content.as_array() else {
        return String::new();
    };
    let mut out = String::new();
    for b in blocks {
        if b.get("type").and_then(|t| t.as_str()) == Some("text") {
            if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(t);
            }
        }
    }
    out
}

/// Extract the inner text of a `<user_query>...</user_query>` wrapper, if
/// present. Returns `None` for injected context lines (external links, user
/// info, runtime notes) that carry no user query.
fn user_query(raw: &str) -> Option<String> {
    let start = raw.find("<user_query>")? + "<user_query>".len();
    let rest = &raw[start..];
    let end = rest.find("</user_query>").unwrap_or(rest.len());
    Some(rest[..end].trim().to_string())
}

/// Load the full conversation for a Cursor session from its JSONL transcript.
/// User turns are unwrapped from `<user_query>`; injected context is dropped;
/// consecutive same-role turns are merged.
pub fn load_cursor_conversation(session: &Session) -> Vec<ConversationMessage> {
    let source = match &session.source_path {
        Some(p) => p,
        None => return Vec::new(),
    };
    let Ok(file) = fs::File::open(source) else {
        return Vec::new();
    };
    let reader = BufReader::new(file);

    let mut messages: Vec<ConversationMessage> = Vec::new();

    for line in reader.lines() {
        let Ok(line) = line else { continue };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let role = value.get("role").and_then(|r| r.as_str()).unwrap_or("");
        let raw = collect_text(&value);
        if raw.is_empty() {
            continue;
        }

        let (msg_role, text) = match role {
            "user" => {
                // Only real user queries; skip injected context lines.
                let Some(q) = user_query(&raw) else { continue };
                let cleaned = clean_message_multiline(&q);
                if cleaned.is_empty() {
                    continue;
                }
                (MessageRole::User, cleaned)
            }
            "assistant" => {
                let cleaned = clean_message_multiline(&raw);
                if cleaned.is_empty() {
                    continue;
                }
                (MessageRole::Assistant, cleaned)
            }
            _ => continue,
        };

        // Merge consecutive turns from the same role into one message.
        if let Some(last) = messages.last_mut() {
            if last.role == msg_role {
                last.text.push_str("\n\n");
                last.text.push_str(&text);
                continue;
            }
        }
        messages.push(ConversationMessage {
            role: msg_role,
            text,
            timestamp: session.timestamp,
        });
    }

    messages
}
