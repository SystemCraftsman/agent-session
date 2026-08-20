// Cross-agent session cloning across Claude, Codex and Cursor.
//
// For file-backed targets (Claude, Codex) this is a "full native
// reconstruction": the source conversation is transplanted into a brand-new
// *native* session file for the target agent, so the target's own CLI can
// resume it. Envelope lines (Codex `session_meta` / `turn_context`, Claude
// entry metadata) are copied from a recent real session of the target agent so
// the synthesized file matches the installed CLI version; only the ids, cwd,
// timestamps and message turns are freshly generated. Codex tolerates
// cli_version drift on resume, which is what makes this reliable.
//
// Cursor is different: `cursor-agent --resume` fetches the conversation from
// Cursor's cloud keyed by chat id, so a locally synthesized chat cannot be
// resumed. Targeting Cursor therefore uses *context seeding*: the source
// conversation is written to an import file and a fresh `cursor-agent` chat is
// launched, pre-instructed to read that file and continue from it.

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use chrono::{Datelike, SecondsFormat, Utc};
use serde_json::{json, Value};

use crate::codex::{get_codex_home, load_codex_conversation, newest_rollout_path, save_codex_title};
use crate::cursor::{get_cursor_home, load_cursor_conversation, save_cursor_title};
use crate::discovery::{get_claude_home, load_conversation};
use crate::session::{Agent, ConversationMessage, MessageRole, Session};

/// Outcome of a clone: the new native session id and the command that resumes it.
pub struct CloneResult {
    pub new_id: String,
    pub resume_command: String,
}

/// Clone a session into `target` and return how to resume/launch the result.
///
/// Claude and Codex targets become native session files (real reconstruction);
/// a Cursor target is context-seeded into a fresh `cursor-agent` chat.
pub fn clone_to_other_agent(session: &Session, target: Agent) -> Result<CloneResult, String> {
    let messages = load_source_conversation(session);
    if messages.is_empty() {
        return Err("source conversation is empty; nothing to clone".to_string());
    }
    match target {
        Agent::Claude => write_claude_session(session, &messages),
        Agent::Codex => write_codex_session(session, &messages),
        // A same-agent reseed reads as a "fork"; a cross-agent one as a
        // "reconstruct", matching the Claude/Codex title strategy.
        Agent::Cursor => {
            let suffix = if session.agent == target {
                "fork"
            } else {
                "reconstruct"
            };
            seed_cursor_session(session, &messages, suffix)
        }
    }
}

/// Load a source session's conversation, dispatching by its own agent.
fn load_source_conversation(session: &Session) -> Vec<ConversationMessage> {
    match session.agent {
        Agent::Claude => load_conversation(&get_claude_home(), session),
        Agent::Codex => load_codex_conversation(session),
        Agent::Cursor => load_cursor_conversation(session),
    }
}

/// Build a label for a derived session: the source's display title (its custom
/// title, else its first message) followed by a parenthetical `suffix` such as
/// "reconstruct". Returns `None` when the source has no usable title text.
pub fn labeled_title(session: &Session, suffix: &str) -> Option<String> {
    let base = session
        .custom_title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .or_else(|| Some(session.first_message.trim()).filter(|t| !t.is_empty()))?;
    Some(format!("{base} ({suffix})"))
}

/// Reconstruct a conversation as a native Codex rollout file.
fn write_codex_session(
    session: &Session,
    messages: &[ConversationMessage],
) -> Result<CloneResult, String> {
    let codex_home = get_codex_home();
    let now = Utc::now();
    let new_id = generate_uuid(&format!(
        "{}-{}",
        session.id,
        now.timestamp_nanos_opt().unwrap_or_default()
    ));
    let ts = now.to_rfc3339_opts(SecondsFormat::Millis, true);

    let (meta_line, turn_line) = codex_envelope(&codex_home, session, &new_id, &ts);

    let mut out = String::new();
    out.push_str(&meta_line);
    out.push('\n');
    if let Some(turn) = turn_line {
        out.push_str(&turn);
        out.push('\n');
    }
    for m in messages {
        out.push_str(&codex_message_line(m));
        out.push('\n');
    }

    // Path: <codex_home>/sessions/YYYY/MM/DD/rollout-<TS>-<uuid>.jsonl
    let dir = codex_home
        .join("sessions")
        .join(format!("{:04}", now.year()))
        .join(format!("{:02}", now.month()))
        .join(format!("{:02}", now.day()));
    fs::create_dir_all(&dir).map_err(|e| format!("failed to create codex session dir: {e}"))?;
    let file_ts = now.format("%Y-%m-%dT%H-%M-%S");
    let path = dir.join(format!("rollout-{file_ts}-{new_id}.jsonl"));
    fs::write(&path, out).map_err(|e| format!("failed to write codex rollout: {e}"))?;

    // Carry the source title (with a "(reconstruct)" suffix) into Codex's title
    // sidecar. A title failure must not fail the clone, so ignore its result.
    if let Some(title) = labeled_title(session, "reconstruct") {
        let _ = save_codex_title(&codex_home, &new_id, &title);
    }

    let escaped_cwd = session.cwd.replace('\'', "'\\''");
    Ok(CloneResult {
        resume_command: format!("cd '{escaped_cwd}' && codex resume {new_id}"),
        new_id,
    })
}

/// Build the Codex envelope (`session_meta` + optional `turn_context`) for a new
/// rollout. Prefers copying the newest real rollout's envelope so the file
/// matches the installed CLI; falls back to a minimal `session_meta` otherwise.
fn codex_envelope(
    codex_home: &Path,
    session: &Session,
    new_id: &str,
    ts: &str,
) -> (String, Option<String>) {
    if let Some(tpl) = newest_rollout_path(codex_home) {
        if let Ok(content) = fs::read_to_string(&tpl) {
            let mut meta: Option<Value> = None;
            let mut turn: Option<Value> = None;
            for line in content.lines() {
                let v: Value = match serde_json::from_str(line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                match v.get("type").and_then(|t| t.as_str()) {
                    Some("session_meta") if meta.is_none() => meta = Some(v),
                    Some("turn_context") if turn.is_none() => turn = Some(v),
                    _ => {}
                }
                if meta.is_some() && turn.is_some() {
                    break;
                }
            }
            if let Some(mut m) = meta {
                if let Some(obj) = m.as_object_mut() {
                    obj.insert("timestamp".to_string(), json!(ts));
                    if let Some(p) = obj.get_mut("payload").and_then(|p| p.as_object_mut()) {
                        p.insert("id".to_string(), json!(new_id));
                        p.insert("cwd".to_string(), json!(session.cwd));
                        p.insert("timestamp".to_string(), json!(ts));
                        p.insert(
                            "git".to_string(),
                            json!({ "branch": session.git_branch.clone().unwrap_or_default() }),
                        );
                    }
                }
                let meta_line = serde_json::to_string(&m).unwrap_or_default();
                let turn_line = turn.map(|mut t| {
                    if let Some(obj) = t.as_object_mut() {
                        obj.insert("timestamp".to_string(), json!(ts));
                        if let Some(p) = obj.get_mut("payload").and_then(|p| p.as_object_mut()) {
                            p.insert("cwd".to_string(), json!(session.cwd));
                        }
                    }
                    serde_json::to_string(&t).unwrap_or_default()
                });
                return (meta_line, turn_line);
            }
        }
    }

    // Fallback: minimal envelope when no template rollout is available.
    let meta = json!({
        "timestamp": ts,
        "type": "session_meta",
        "payload": {
            "id": new_id,
            "timestamp": ts,
            "cwd": session.cwd,
            "originator": "codex_cli_rs",
            "source": "cli",
            "git": { "branch": session.git_branch.clone().unwrap_or_default() },
        }
    });
    (serde_json::to_string(&meta).unwrap_or_default(), None)
}

/// Serialize one conversation message as a Codex `response_item` message line.
fn codex_message_line(m: &ConversationMessage) -> String {
    let (role, block_type) = match m.role {
        MessageRole::User => ("user", "input_text"),
        MessageRole::Assistant => ("assistant", "output_text"),
    };
    let ts = m.timestamp.to_rfc3339_opts(SecondsFormat::Millis, true);
    let v = json!({
        "timestamp": ts,
        "type": "response_item",
        "payload": {
            "type": "message",
            "role": role,
            "content": [ { "type": block_type, "text": m.text } ],
        }
    });
    serde_json::to_string(&v).unwrap_or_default()
}

/// Reconstruct a conversation as a native Claude JSONL session file.
fn write_claude_session(
    session: &Session,
    messages: &[ConversationMessage],
) -> Result<CloneResult, String> {
    let claude_home = get_claude_home();
    let now = Utc::now();
    let session_id = generate_uuid(&format!(
        "{}-{}",
        session.id,
        now.timestamp_nanos_opt().unwrap_or_default()
    ));
    let version = newest_claude_version(&claude_home).unwrap_or_else(|| "2.0.0".to_string());
    let branch = session.git_branch.clone().unwrap_or_default();

    let mut out = String::new();
    let mut parent: Option<String> = None;
    for (i, m) in messages.iter().enumerate() {
        let uuid = generate_uuid(&format!("{}-{}-{}", session_id, i, m.text.len()));
        let ts = m.timestamp.to_rfc3339_opts(SecondsFormat::Millis, true);
        let (type_str, message) = match m.role {
            MessageRole::User => ("user", json!({ "role": "user", "content": m.text })),
            MessageRole::Assistant => (
                "assistant",
                json!({
                    "role": "assistant",
                    "content": [ { "type": "text", "text": m.text } ],
                }),
            ),
        };
        let entry = json!({
            "type": type_str,
            "parentUuid": parent,
            "uuid": uuid,
            "sessionId": session_id,
            "cwd": session.cwd,
            "gitBranch": branch,
            "timestamp": ts,
            "userType": "external",
            "isSidechain": false,
            "version": version,
            "message": message,
        });
        out.push_str(&serde_json::to_string(&entry).unwrap_or_default());
        out.push('\n');
        parent = Some(uuid);
    }

    let encoded = encode_claude_dir(&session.cwd);
    let dir = claude_home.join("projects").join(&encoded);
    fs::create_dir_all(&dir).map_err(|e| format!("failed to create claude project dir: {e}"))?;
    let path = dir.join(format!("{session_id}.jsonl"));
    fs::write(&path, out).map_err(|e| format!("failed to write claude session: {e}"))?;

    // Carry the source title (with a "(reconstruct)" suffix) as a Claude
    // custom-title line. A title failure must not fail the clone, so ignore it.
    if let Some(title) = labeled_title(session, "reconstruct") {
        let _ = crate::titles::save_custom_title(&encoded, &session_id, &title);
    }

    let escaped_cwd = session.cwd.replace('\'', "'\\''");
    Ok(CloneResult {
        resume_command: format!("cd '{escaped_cwd}' && claude -r {session_id}"),
        new_id: session_id,
    })
}

/// Claude encodes a cwd into a project directory name by replacing `/` and `.`
/// with `-`. Mirrors the encoding used elsewhere for discovery/grouping.
fn encode_claude_dir(cwd: &str) -> String {
    cwd.replace(['/', '.'], "-")
}

/// Context-seed a conversation into a Cursor chat.
///
/// Cursor resumes from its cloud (not from local files), so we cannot synthesize
/// a resumable chat. Instead we write the source conversation to an import file
/// and launch a `cursor-agent` chat pre-instructed to read it and continue.
///
/// To carry the source title (matching the Claude/Codex fork strategy) we mint a
/// real chat id up front with `cursor-agent create-chat`, register the labeled
/// title in Cursor's title sidecar under that id, and resume it. If create-chat
/// is unavailable (not logged in, offline, old CLI) we fall back to launching a
/// fresh chat whose id Cursor mints itself; `new_id` then only names the import
/// file and no title can be pre-registered.
fn seed_cursor_session(
    session: &Session,
    messages: &[ConversationMessage],
    suffix: &str,
) -> Result<CloneResult, String> {
    let cursor_home = get_cursor_home();
    let now = Utc::now();

    let source_agent = match session.agent {
        Agent::Claude => "Claude",
        Agent::Codex => "Codex",
        Agent::Cursor => "Cursor",
    };

    // Mint a real chat id when possible so the title can travel with it.
    let created_id = create_cursor_chat();
    let new_id = created_id.clone().unwrap_or_else(|| {
        generate_uuid(&format!(
            "{}-{}",
            session.id,
            now.timestamp_nanos_opt().unwrap_or_default()
        ))
    });

    // Render the conversation as a readable Markdown transcript.
    let labeled = labeled_title(session, suffix);
    let mut doc = String::new();
    doc.push_str(&format!("# Imported conversation (from {source_agent})\n\n"));
    if let Some(title) = &labeled {
        doc.push_str(&format!("Title: {title}\n\n"));
    }
    doc.push_str(
        "This is a prior conversation carried over from another AI agent. \
         Read it in full, then continue helping from where it left off.\n\n---\n\n",
    );
    for m in messages {
        let who = match m.role {
            MessageRole::User => "User",
            MessageRole::Assistant => "Assistant",
        };
        doc.push_str(&format!("## {who}\n\n{}\n\n", m.text));
    }

    let import_dir = cursor_home.join("agent-session-imports");
    fs::create_dir_all(&import_dir)
        .map_err(|e| format!("failed to create cursor import dir: {e}"))?;
    let import_path = import_dir.join(format!("{new_id}.md"));
    fs::write(&import_path, doc).map_err(|e| format!("failed to write cursor import file: {e}"))?;

    // Carry the source title into Cursor's sidecar when we control the chat id.
    // A title failure must not fail the seed, so ignore its result.
    if created_id.is_some() {
        if let Some(title) = &labeled {
            let _ = save_cursor_title(&cursor_home, &new_id, title);
        }
    }

    let import_str = import_path.to_string_lossy();
    let prompt = format!(
        "We are continuing a prior {source_agent} conversation. \
         The full transcript is saved at {import_str}. \
         Please read that file, then continue helping me from where we left off."
    );

    let escaped_cwd = session.cwd.replace('\'', "'\\''");
    let escaped_prompt = prompt.replace('\'', "'\\''");
    let resume_command = match &created_id {
        Some(id) => format!("cd '{escaped_cwd}' && cursor-agent --resume {id} '{escaped_prompt}'"),
        None => format!("cd '{escaped_cwd}' && cursor-agent '{escaped_prompt}'"),
    };
    Ok(CloneResult {
        resume_command,
        new_id,
    })
}

/// Mint a new empty Cursor chat via `cursor-agent create-chat` and return its id.
///
/// Returns `None` if the binary is missing, the command fails (e.g. not logged
/// in / offline), or the output does not contain a chat id. Runs `cursor-agent`
/// bare so it never routes through any shell wrapper.
fn create_cursor_chat() -> Option<String> {
    // Escape hatch so tests (and offline runs) never hit Cursor's cloud.
    if std::env::var_os("CC_SESSION_NO_CURSOR_CREATE").is_some() {
        return None;
    }
    let output = std::process::Command::new("cursor-agent")
        .arg("create-chat")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    // create-chat prints the id; scan defensively for a UUID-shaped token.
    text.split_whitespace()
        .map(str::trim)
        .find(|tok| is_uuid(tok))
        .map(str::to_string)
}

/// Return true if `s` is shaped like a canonical 8-4-4-4-12 hex UUID.
fn is_uuid(s: &str) -> bool {
    let groups = [8usize, 4, 4, 4, 12];
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != groups.len() {
        return false;
    }
    parts
        .iter()
        .zip(groups.iter())
        .all(|(p, &n)| p.len() == n && p.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Read the `version` field from the newest Claude session file, used so a
/// reconstructed session advertises a plausible CLI version.
fn newest_claude_version(claude_home: &Path) -> Option<String> {
    let projects = claude_home.join("projects");
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;

    let Ok(proj_iter) = fs::read_dir(&projects) else {
        return None;
    };
    for proj in proj_iter.flatten() {
        if !proj.path().is_dir() {
            continue;
        }
        let Ok(files) = fs::read_dir(proj.path()) else {
            continue;
        };
        for f in files.flatten() {
            let p = f.path();
            if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(modified) = f.metadata().and_then(|m| m.modified()) else {
                continue;
            };
            if newest.as_ref().map(|(t, _)| modified > *t).unwrap_or(true) {
                newest = Some((modified, p));
            }
        }
    }

    let (_, path) = newest?;
    let content = fs::read_to_string(path).ok()?;
    let first = content.lines().next()?;
    let v: Value = serde_json::from_str(first).ok()?;
    v.get("version")
        .and_then(|x| x.as_str())
        .map(String::from)
}

/// Generate a v4-formatted UUID string deterministically from `seed`.
///
/// The project has no `uuid`/`rand` dependency, so we derive 128 bits from two
/// salted std hashes of the seed and set the version/variant nibbles. Callers
/// mix a nanosecond timestamp into the seed to keep successive clones unique.
fn generate_uuid(seed: &str) -> String {
    let hash_with = |salt: &str| -> u64 {
        let mut h = DefaultHasher::new();
        seed.hash(&mut h);
        salt.hash(&mut h);
        h.finish()
    };
    let hi = hash_with("agent-session-hi").to_be_bytes();
    let lo = hash_with("agent-session-lo").to_be_bytes();

    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&hi);
    b[8..].copy_from_slice(&lo);
    // Version 4 and RFC 4122 variant bits.
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;

    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}
