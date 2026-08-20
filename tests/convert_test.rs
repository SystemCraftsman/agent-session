use std::path::Path;

use cc_session::codex::{discover_codex_sessions, load_codex_conversation};
use cc_session::convert::clone_to_other_agent;
use cc_session::discovery::load_conversation;
use cc_session::session::{Agent, MessageRole, Session};

use serial_test::serial;

/// Encode a cwd into a Claude project dir name (mirrors the crate's encoding).
fn encode(cwd: &str) -> String {
    cwd.replace(['/', '.'], "-")
}

/// Write a minimal Claude session (one user + one assistant turn) and return the
/// `Session` that points at it.
fn write_claude_source(claude_home: &Path, cwd: &str) -> Session {
    let id = "src-claude-0000-0000-0000-000000000000";
    let project = encode(cwd);
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

/// Write a minimal Codex rollout and return the discovered `Session`.
fn write_codex_source(codex_home: &Path, cwd: &str) -> Session {
    let dir = codex_home.join("sessions/2026/04/01");
    std::fs::create_dir_all(&dir).unwrap();
    let id = "src-codex-0000-0000-0000-000000000000";
    let body = format!(
        concat!(
            "{{\"timestamp\":\"2026-04-01T10:00:00.000Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"cwd\":\"{cwd}\",\"timestamp\":\"2026-04-01T10:00:00.000Z\",\"git\":{{\"branch\":\"main\"}}}}}}\n",
            "{{\"timestamp\":\"2026-04-01T10:00:01.000Z\",\"type\":\"turn_context\",\"payload\":{{\"cwd\":\"{cwd}\",\"model\":\"gpt-5.4\"}}}}\n",
            "{{\"timestamp\":\"2026-04-01T10:00:05.000Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"content\":[{{\"type\":\"input_text\",\"text\":\"Add OAuth2 support.\"}}]}}}}\n",
            "{{\"timestamp\":\"2026-04-01T10:00:09.000Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{{\"type\":\"output_text\",\"text\":\"Added the OAuth2 client.\"}}]}}}}\n",
        ),
        id = id,
        cwd = cwd,
    );
    std::fs::write(dir.join(format!("rollout-2026-04-01T10-00-00-{id}.jsonl")), body).unwrap();

    discover_codex_sessions(codex_home)
        .into_iter()
        .find(|s| s.id == id)
        .expect("codex source discovered")
}

#[test]
#[serial]
fn clones_claude_session_into_native_codex_rollout() {
    let claude_home = tempfile::tempdir().unwrap();
    let codex_home = tempfile::tempdir().unwrap();
    std::env::set_var("CLAUDE_HOME", claude_home.path());
    std::env::set_var("CODEX_HOME", codex_home.path());

    let cwd = "/tmp/clone-proj-claude";
    let source = write_claude_source(claude_home.path(), cwd);

    let result = clone_to_other_agent(&source, Agent::Codex).expect("clone succeeds");
    assert!(result
        .resume_command
        .contains(&format!("codex resume {}", result.new_id)));
    assert!(result.resume_command.contains(cwd));

    // The synthesized file must be discoverable as a real Codex session...
    let discovered = discover_codex_sessions(codex_home.path());
    let cloned = discovered
        .iter()
        .find(|s| s.id == result.new_id)
        .expect("cloned codex session discovered");
    assert_eq!(cloned.cwd, cwd);
    assert_eq!(cloned.agent, Agent::Codex);
    // The source title carries over with a "(reconstruct)" suffix.
    assert_eq!(
        cloned.custom_title.as_deref(),
        Some("Refactor the payment module. (reconstruct)")
    );

    // ...and its conversation must round-trip both turns.
    let messages = load_codex_conversation(cloned);
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role, MessageRole::User);
    assert_eq!(messages[0].text, "Refactor the payment module.");
    assert_eq!(messages[1].role, MessageRole::Assistant);
    assert_eq!(messages[1].text, "Done, all tests pass.");

    std::env::remove_var("CLAUDE_HOME");
    std::env::remove_var("CODEX_HOME");
}

#[test]
#[serial]
fn clones_codex_session_into_native_claude_jsonl() {
    let claude_home = tempfile::tempdir().unwrap();
    let codex_home = tempfile::tempdir().unwrap();
    std::env::set_var("CLAUDE_HOME", claude_home.path());
    std::env::set_var("CODEX_HOME", codex_home.path());

    let cwd = "/tmp/clone-proj-codex";
    let source = write_codex_source(codex_home.path(), cwd);

    let result = clone_to_other_agent(&source, Agent::Claude).expect("clone succeeds");
    assert!(result
        .resume_command
        .contains(&format!("claude -r {}", result.new_id)));
    assert!(result.resume_command.contains(cwd));

    // Reconstruct the Session pointer for the new native Claude file and load it.
    let cloned = Session {
        id: result.new_id.clone(),
        project_path: encode(cwd),
        project_name: "clone-proj-codex".to_string(),
        git_branch: Some("main".to_string()),
        timestamp: chrono::Utc::now(),
        first_message: String::new(),
        cwd: cwd.to_string(),
        project_exists: false,
        custom_title: None,
        agent: Agent::Claude,
        source_path: None,
    };
    let messages = load_conversation(claude_home.path(), &cloned);
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role, MessageRole::User);
    assert_eq!(messages[0].text, "Add OAuth2 support.");
    assert_eq!(messages[1].role, MessageRole::Assistant);
    assert_eq!(messages[1].text, "Added the OAuth2 client.");

    // The source title carries over as a Claude custom-title line.
    let file = claude_home
        .path()
        .join("projects")
        .join(encode(cwd))
        .join(format!("{}.jsonl", result.new_id));
    let contents = std::fs::read_to_string(&file).unwrap();
    assert!(
        contents.contains("\"customTitle\":\"Add OAuth2 support. (reconstruct)\""),
        "expected a custom-title line carrying the reconstruct suffix"
    );

    std::env::remove_var("CLAUDE_HOME");
    std::env::remove_var("CODEX_HOME");
}
