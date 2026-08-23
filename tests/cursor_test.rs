use std::path::Path;

use agent_session::convert::clone_to_other_agent;
use agent_session::cursor::{
    archive_cursor_session, discover_archived_cursor_sessions, discover_cursor_sessions,
    encode_cursor_dir, load_cursor_conversation, restore_cursor_session,
};
use agent_session::discovery::load_conversation;
use agent_session::session::{Agent, MessageRole, Session};

use serial_test::serial;

/// A real, existing cwd whose `/tmp/<name>` encoding round-trips cleanly through
/// the filesystem-probing decoder (single dash at a genuine `/` boundary).
const CURSOR_CWD: &str = "/tmp/agent_session_cursor_test_dir";

/// Encode a cwd into a Claude project dir name (mirrors the crate's encoding).
fn encode_claude(cwd: &str) -> String {
    cwd.replace(['/', '.'], "-")
}

/// Write a minimal Cursor transcript (one injected line, one real user query,
/// one assistant turn) under `cursor_home` and return the discovered `Session`.
fn write_cursor_source(cursor_home: &Path, cwd: &str, chat_id: &str) -> Session {
    std::fs::create_dir_all(cwd).unwrap();
    let dir = cursor_home
        .join("projects")
        .join(encode_cursor_dir(cwd))
        .join("agent-transcripts")
        .join(chat_id);
    std::fs::create_dir_all(&dir).unwrap();

    let body = concat!(
        "{\"role\":\"user\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"<external_links>\\ninjected context\\n</external_links>\"}]}}\n",
        "{\"role\":\"user\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"<user_query>\\nRefactor the payment module.\\n</user_query>\"}]}}\n",
        "{\"role\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"Done, all tests pass.\"},{\"type\":\"tool_use\",\"toolName\":\"Shell\"}]}}\n",
    );
    std::fs::write(dir.join(format!("{chat_id}.jsonl")), body).unwrap();

    discover_cursor_sessions(cursor_home)
        .into_iter()
        .find(|s| s.id == chat_id)
        .expect("cursor source discovered")
}

/// Write a minimal Claude session (one user + one assistant turn).
fn write_claude_source(claude_home: &Path, cwd: &str) -> Session {
    let id = "src-claude-cursor-0000-0000-000000000000";
    let project = encode_claude(cwd);
    let dir = claude_home.join("projects").join(&project);
    std::fs::create_dir_all(&dir).unwrap();
    let body = concat!(
        "{\"parentUuid\":null,\"isSidechain\":false,\"userType\":\"external\",\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"Refactor the payment module.\"},\"uuid\":\"u1\",\"timestamp\":\"2026-04-01T10:00:00.000Z\"}\n",
        "{\"parentUuid\":\"u1\",\"isSidechain\":false,\"userType\":\"external\",\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"Done, all tests pass.\"}]},\"uuid\":\"a1\",\"timestamp\":\"2026-04-01T10:00:05.000Z\"}\n",
    );
    std::fs::write(dir.join(format!("{id}.jsonl")), body).unwrap();

    Session {
        id: id.to_string(),
        project_path: project,
        project_name: "clone-proj".to_string(),
        git_branch: Some("main".to_string()),
        timestamp: chrono::Utc::now(),
        first_message: "Refactor the payment module.".to_string(),
        cwd: cwd.to_string(),
        project_exists: false,
        custom_title: None,
        agent: Agent::Claude,
        source_path: None,
    }
}

#[test]
#[serial]
fn discovers_and_loads_cursor_transcript() {
    let cursor_home = tempfile::tempdir().unwrap();
    std::env::set_var("CURSOR_HOME", cursor_home.path());

    let chat_id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    let source = write_cursor_source(cursor_home.path(), CURSOR_CWD, chat_id);

    assert_eq!(source.agent, Agent::Cursor);
    assert_eq!(source.cwd, CURSOR_CWD);
    assert_eq!(source.first_message, "Refactor the payment module.");

    // The injected `<external_links>` user line is dropped; the real query is
    // unwrapped; the assistant tool_use block is ignored.
    let messages = load_cursor_conversation(&source);
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role, MessageRole::User);
    assert_eq!(messages[0].text, "Refactor the payment module.");
    assert_eq!(messages[1].role, MessageRole::Assistant);
    assert_eq!(messages[1].text, "Done, all tests pass.");

    std::env::remove_var("CURSOR_HOME");
    let _ = std::fs::remove_dir_all(CURSOR_CWD);
}

#[test]
#[serial]
fn archived_cursor_session_is_discoverable_and_restorable() {
    let cursor_home = tempfile::tempdir().unwrap();
    std::env::set_var("CURSOR_HOME", cursor_home.path());

    let chat_id = "ffffffff-0000-1111-2222-333333333333";
    let source = write_cursor_source(cursor_home.path(), CURSOR_CWD, chat_id);
    let src_path = source.source_path.clone().expect("cursor source path");

    archive_cursor_session(&src_path).unwrap();
    assert!(discover_cursor_sessions(cursor_home.path()).is_empty());

    let archived = discover_archived_cursor_sessions(cursor_home.path());
    assert_eq!(archived.len(), 1);
    let archived_src = archived[0].source_path.clone().expect("archived source path");

    restore_cursor_session(&archived_src).unwrap();
    assert!(discover_archived_cursor_sessions(cursor_home.path()).is_empty());
    let restored = discover_cursor_sessions(cursor_home.path());
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].id, chat_id);

    std::env::remove_var("CURSOR_HOME");
    let _ = std::fs::remove_dir_all(CURSOR_CWD);
}

#[test]
#[serial]
fn archiving_over_a_stale_archived_stub_disambiguates_instead_of_failing() {
    let cursor_home = tempfile::tempdir().unwrap();
    std::env::set_var("CURSOR_HOME", cursor_home.path());

    let chat_id = "cccccccc-1111-2222-3333-444444444444";
    let source = write_cursor_source(cursor_home.path(), CURSOR_CWD, chat_id);
    let src_path = source.source_path.clone().expect("cursor source path");

    // Pre-seed a stale, non-empty archived directory occupying the same chat id
    // (the case that used to fail with "directory not empty").
    let archive_root = cursor_home
        .path()
        .join("projects")
        .join(encode_cursor_dir(CURSOR_CWD))
        .join("agent-transcripts-archive");
    let stale = archive_root.join(chat_id);
    std::fs::create_dir_all(&stale).unwrap();
    std::fs::write(stale.join(format!("{chat_id}.jsonl")), "stale\n").unwrap();

    // Archiving must succeed (no "directory not empty") without clobbering the
    // stale stub: the real session lands in a disambiguated `<id>-2` directory
    // whose inner transcript is renamed to match, so discovery still finds it.
    archive_cursor_session(&src_path).unwrap();
    assert!(discover_cursor_sessions(cursor_home.path()).is_empty());
    assert!(stale.exists(), "stale archived stub must be preserved");
    let disambiguated = archive_root.join(format!("{chat_id}-2"));
    assert!(disambiguated.exists(), "new archive lands in <id>-2");
    assert!(disambiguated
        .join(format!("{chat_id}-2.jsonl"))
        .exists());
    let archived = discover_archived_cursor_sessions(cursor_home.path());
    assert_eq!(archived.len(), 1, "the real session is discoverable");

    std::env::remove_var("CURSOR_HOME");
    let _ = std::fs::remove_dir_all(CURSOR_CWD);
}

#[test]
#[serial]
fn reconstructs_cursor_session_into_native_claude() {
    let cursor_home = tempfile::tempdir().unwrap();
    let claude_home = tempfile::tempdir().unwrap();
    std::env::set_var("CURSOR_HOME", cursor_home.path());
    std::env::set_var("CLAUDE_HOME", claude_home.path());

    let chat_id = "11111111-2222-3333-4444-555555555555";
    let source = write_cursor_source(cursor_home.path(), CURSOR_CWD, chat_id);

    let result = clone_to_other_agent(&source, Agent::Claude).expect("clone succeeds");
    assert!(result
        .resume_command
        .contains(&format!("claude -r {}", result.new_id)));
    assert!(result.resume_command.contains(CURSOR_CWD));

    // The synthesized Claude file round-trips both turns.
    let cloned = Session {
        id: result.new_id.clone(),
        project_path: encode_claude(CURSOR_CWD),
        project_name: "agent_session_cursor_test_dir".to_string(),
        git_branch: None,
        timestamp: chrono::Utc::now(),
        first_message: String::new(),
        cwd: CURSOR_CWD.to_string(),
        project_exists: false,
        custom_title: None,
        agent: Agent::Claude,
        source_path: None,
    };
    let messages = load_conversation(claude_home.path(), &cloned);
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].text, "Refactor the payment module.");
    assert_eq!(messages[1].text, "Done, all tests pass.");

    // The source title (its first message) carries over with the suffix.
    let file = claude_home
        .path()
        .join("projects")
        .join(encode_claude(CURSOR_CWD))
        .join(format!("{}.jsonl", result.new_id));
    let contents = std::fs::read_to_string(&file).unwrap();
    assert!(contents.contains("\"customTitle\":\"Refactor the payment module. (reconstruct)\""));

    std::env::remove_var("CURSOR_HOME");
    std::env::remove_var("CLAUDE_HOME");
    let _ = std::fs::remove_dir_all(CURSOR_CWD);
}

#[test]
#[serial]
fn seeds_new_cursor_chat_from_claude() {
    let claude_home = tempfile::tempdir().unwrap();
    let cursor_home = tempfile::tempdir().unwrap();
    std::env::set_var("CLAUDE_HOME", claude_home.path());
    std::env::set_var("CURSOR_HOME", cursor_home.path());
    // Never touch Cursor's cloud during tests: force the fresh-launch fallback.
    std::env::set_var("CC_SESSION_NO_CURSOR_CREATE", "1");

    let cwd = "/tmp/clone-proj-claude-to-cursor";
    let source = write_claude_source(claude_home.path(), cwd);

    let result = clone_to_other_agent(&source, Agent::Cursor).expect("seed succeeds");
    // Fallback path launches a fresh cursor-agent chat (no --resume id).
    assert!(result.resume_command.contains("cursor-agent"));
    assert!(!result.resume_command.contains("--resume"));
    assert!(result.resume_command.contains(cwd));

    // The import file holds the full prior conversation for cursor-agent to read.
    let import = cursor_home
        .path()
        .join("agent-session-imports")
        .join(format!("{}.md", result.new_id));
    let doc = std::fs::read_to_string(&import).expect("import file written");
    assert!(doc.contains("Refactor the payment module."));
    assert!(doc.contains("Done, all tests pass."));
    assert!(doc.contains("from Claude"));
    // The labeled title is embedded in the import doc for context. A Cursor
    // target is context-seeded, so a cross-agent copy carries a "(seed)" suffix.
    assert!(doc.contains("Refactor the payment module. (seed)"));

    std::env::remove_var("CLAUDE_HOME");
    std::env::remove_var("CURSOR_HOME");
    std::env::remove_var("CC_SESSION_NO_CURSOR_CREATE");
}
