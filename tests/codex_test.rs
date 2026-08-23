use std::path::{Path, PathBuf};

use agent_session::codex::{
    archive_codex_session, discover_archived_codex_sessions, discover_codex_sessions,
    load_codex_conversation, load_codex_titles, move_codex_session, restore_codex_session,
    save_codex_title,
};
use agent_session::session::{Agent, MessageRole};

fn codex_home() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex")
}

/// Write a minimal but valid rollout (session_meta + turn_context + one user
/// turn) under `home/sessions/2026/03/24/`, returning its path.
fn write_rollout(home: &Path, id: &str, cwd: &str) -> PathBuf {
    let dir = home.join("sessions/2026/03/24");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("rollout-2026-03-24T10-00-00-{id}.jsonl"));
    let body = format!(
        concat!(
            "{{\"timestamp\":\"2026-03-24T10:00:00.000Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"cwd\":\"{cwd}\",\"timestamp\":\"2026-03-24T10:00:00.000Z\",\"git\":{{\"branch\":\"main\"}}}}}}\n",
            "{{\"timestamp\":\"2026-03-24T10:00:01.000Z\",\"type\":\"turn_context\",\"payload\":{{\"cwd\":\"{cwd}\",\"model\":\"gpt-5.4\"}}}}\n",
            "{{\"timestamp\":\"2026-03-24T10:00:05.000Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"content\":[{{\"type\":\"input_text\",\"text\":\"Hello there\"}}]}}}}\n",
        ),
        id = id,
        cwd = cwd,
    );
    std::fs::write(&path, body).unwrap();
    path
}

#[test]
fn discovers_codex_rollouts_recursively() {
    let sessions = discover_codex_sessions(&codex_home());
    assert_eq!(sessions.len(), 2, "expected 2 codex sessions from fixtures");
    assert!(sessions.iter().all(|s| s.agent == Agent::Codex));
}

#[test]
fn sessions_sorted_newest_first() {
    let sessions = discover_codex_sessions(&codex_home());
    assert_eq!(sessions[0].id, "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb");
    assert_eq!(sessions[1].id, "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa");
}

#[test]
fn parses_metadata_and_skips_injected_context() {
    let sessions = discover_codex_sessions(&codex_home());
    let s = sessions
        .iter()
        .find(|s| s.id == "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa")
        .expect("session aaaa present");

    // The <environment_context> injection must not leak as the first message.
    assert_eq!(s.first_message, "Refactor the payment module to use async.");
    assert_eq!(s.cwd, "/tmp/codex-fixture-project");
    assert_eq!(s.project_name, "codex-fixture-project");
    assert_eq!(s.git_branch.as_deref(), Some("main"));
    // project_path groups by cwd; the rollout file lives in source_path.
    assert_eq!(s.project_path, "/tmp/codex-fixture-project");
    assert!(s
        .source_path
        .as_deref()
        .is_some_and(|p| p.ends_with(".jsonl")));
}

#[test]
fn resume_command_uses_codex_shell_wrapper() {
    let sessions = discover_codex_sessions(&codex_home());
    let s = sessions
        .iter()
        .find(|s| s.id == "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa")
        .expect("session aaaa present");
    // Agent Session emits a bare `codex`; the user's `codex()` shell function owns
    // any wrapping (mirroring the `claude` wrapper).
    assert_eq!(
        s.resume_command(),
        "cd '/tmp/codex-fixture-project' && codex resume aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"
    );
}

#[test]
fn loads_conversation_merging_consecutive_assistant_turns() {
    let sessions = discover_codex_sessions(&codex_home());
    let s = sessions
        .iter()
        .find(|s| s.id == "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa")
        .expect("session aaaa present");

    let messages = load_codex_conversation(s);
    assert_eq!(messages.len(), 2, "user + merged assistant turns");

    assert_eq!(messages[0].role, MessageRole::User);
    assert_eq!(
        messages[0].text,
        "Refactor the payment module to use async."
    );

    assert_eq!(messages[1].role, MessageRole::Assistant);
    // The two assistant turns (across a tool call) are merged.
    assert!(messages[1]
        .text
        .contains("Sure, I'll refactor the payment module."));
    assert!(messages[1].text.contains("Done, all tests pass."));
}

#[test]
fn codex_titles_roundtrip_and_removal() {
    let home = tempfile::tempdir().unwrap();
    save_codex_title(home.path(), "id-1", "My Codex Title").unwrap();
    save_codex_title(home.path(), "id-2", "Another").unwrap();

    let titles = load_codex_titles(home.path());
    assert_eq!(
        titles.get("id-1").map(String::as_str),
        Some("My Codex Title")
    );
    assert_eq!(titles.get("id-2").map(String::as_str), Some("Another"));

    // An empty title removes the entry.
    save_codex_title(home.path(), "id-1", "").unwrap();
    let titles = load_codex_titles(home.path());
    assert!(!titles.contains_key("id-1"));
    assert!(titles.contains_key("id-2"));
}

#[test]
fn custom_title_applied_during_discovery() {
    let home = tempfile::tempdir().unwrap();
    write_rollout(home.path(), "titled-session", "/tmp/parity-proj");
    save_codex_title(home.path(), "titled-session", "Renamed In Sidecar").unwrap();

    let sessions = discover_codex_sessions(home.path());
    let s = sessions
        .iter()
        .find(|s| s.id == "titled-session")
        .expect("session discovered");
    assert_eq!(s.custom_title.as_deref(), Some("Renamed In Sidecar"));
}

#[test]
fn archive_moves_rollout_out_of_sessions() {
    let home = tempfile::tempdir().unwrap();
    let path = write_rollout(home.path(), "to-archive", "/tmp/parity-proj");
    assert_eq!(discover_codex_sessions(home.path()).len(), 1);

    archive_codex_session(home.path(), path.to_str().unwrap()).unwrap();

    assert!(!path.exists(), "original rollout should be gone");
    assert!(
        home.path()
            .join("sessions-archive/2026/03/24")
            .join(path.file_name().unwrap())
            .exists(),
        "rollout should live under sessions-archive with its date subpath"
    );
    assert_eq!(discover_codex_sessions(home.path()).len(), 0);
}

#[test]
fn archived_rollout_is_discoverable_and_restorable() {
    let home = tempfile::tempdir().unwrap();
    let path = write_rollout(home.path(), "round-trip", "/tmp/parity-proj");
    archive_codex_session(home.path(), path.to_str().unwrap()).unwrap();

    // The archived rollout shows up in the archive listing (not the live one).
    assert_eq!(discover_codex_sessions(home.path()).len(), 0);
    let archived = discover_archived_codex_sessions(home.path());
    assert_eq!(archived.len(), 1);
    let source = archived[0].source_path.clone().expect("archived source path");

    // Restoring moves it back into sessions/ with its date subpath preserved.
    restore_codex_session(home.path(), &source).unwrap();
    assert!(path.exists(), "restored rollout should be back in sessions/");
    assert_eq!(discover_archived_codex_sessions(home.path()).len(), 0);
    assert_eq!(discover_codex_sessions(home.path()).len(), 1);
}

#[test]
fn move_rewrites_cwd_in_place() {
    let home = tempfile::tempdir().unwrap();
    let path = write_rollout(home.path(), "to-move", "/tmp/old-cwd");

    move_codex_session(path.to_str().unwrap(), "/tmp/new-cwd").unwrap();

    assert!(path.exists(), "codex move keeps the file in its date dir");
    let sessions = discover_codex_sessions(home.path());
    let s = sessions.iter().find(|s| s.id == "to-move").unwrap();
    assert_eq!(s.cwd, "/tmp/new-cwd");
    assert_eq!(s.project_path, "/tmp/new-cwd");
    assert_eq!(s.project_name, "new-cwd");
}
