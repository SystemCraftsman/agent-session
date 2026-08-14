use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serial_test::serial;

use cc_session::discovery::discover_sessions;
use cc_session::theme::Theme;
use cc_session::tui::{input::handle_input, Action, App, Mode, TreeRow};

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn make_app(grouped: bool) -> App {
    let sessions = discover_sessions(&fixture_dir());
    let session_index = HashMap::new();
    let theme = Theme::dark();
    App::new(sessions, session_index, theme, grouped)
}

fn make_app_expanded() -> App {
    let mut app = make_app(true);
    for i in 0..app.project_groups.len() {
        if !app.project_groups[i].expanded {
            app.toggle_project(i);
        }
    }
    app.selected = 0;
    app
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn key_ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

#[test]
fn tab_toggles_grouped_view() {
    let mut app = make_app(true);
    assert!(app.grouped_view);

    let action = handle_input(&mut app, key(KeyCode::Tab));
    assert!(matches!(action, Action::Continue));
    assert!(!app.grouped_view, "Tab should switch to flat view");

    let action = handle_input(&mut app, key(KeyCode::Tab));
    assert!(matches!(action, Action::Continue));
    assert!(app.grouped_view, "Tab should switch back to grouped view");
}

#[test]
fn enter_on_project_toggles_expand() {
    let mut app = make_app(true);
    assert!(matches!(app.tree_rows[0], TreeRow::Project(0)));
    assert!(!app.project_groups[0].expanded, "should start collapsed");

    app.selected = 0;
    let action = handle_input(&mut app, key(KeyCode::Enter));
    assert!(matches!(action, Action::Continue));
    assert!(app.project_groups[0].expanded, "Enter should expand project");

    let action = handle_input(&mut app, key(KeyCode::Enter));
    assert!(matches!(action, Action::Continue));
    assert!(!app.project_groups[0].expanded, "Enter should collapse project");
}

#[test]
fn enter_on_session_enters_conversation() {
    let mut app = make_app_expanded();
    app.selected = 1;
    assert!(matches!(app.tree_rows[1], TreeRow::Session { .. }));

    let action = handle_input(&mut app, key(KeyCode::Enter));
    match action {
        Action::EnterConversation(idx) => {
            assert!(idx < app.display_entries.len());
        }
        _ => panic!("Expected EnterConversation action on session row"),
    }
}

#[test]
fn left_arrow_collapses_project() {
    let mut app = make_app_expanded();
    app.selected = 0;
    assert!(app.project_groups[0].expanded);

    handle_input(&mut app, key(KeyCode::Left));
    assert!(!app.project_groups[0].expanded, "Left should collapse expanded project");
}

#[test]
fn left_arrow_on_collapsed_project_is_noop() {
    let mut app = make_app(true);
    app.selected = 0;
    assert!(!app.project_groups[0].expanded);

    handle_input(&mut app, key(KeyCode::Left));
    assert!(!app.project_groups[0].expanded, "Left on collapsed project should be noop");
}

#[test]
fn right_arrow_expands_project() {
    let mut app = make_app(true);
    app.selected = 0;
    assert!(!app.project_groups[0].expanded);

    handle_input(&mut app, key(KeyCode::Right));
    assert!(app.project_groups[0].expanded, "Right should expand collapsed project");
}

#[test]
fn right_arrow_on_expanded_project_is_noop() {
    let mut app = make_app_expanded();
    app.selected = 0;
    assert!(app.project_groups[0].expanded);

    handle_input(&mut app, key(KeyCode::Right));
    assert!(app.project_groups[0].expanded, "Right on expanded project should be noop");
}

#[test]
fn left_arrow_on_session_jumps_to_parent_project() {
    let mut app = make_app_expanded();
    app.selected = 2;
    assert!(matches!(app.tree_rows[2], TreeRow::Session { project_idx: 0, .. }));

    handle_input(&mut app, key(KeyCode::Left));
    assert_eq!(app.selected, 0, "Left on session should jump to parent project");
    assert!(matches!(app.tree_rows[0], TreeRow::Project(0)));
}

#[test]
fn up_down_navigates_tree_rows() {
    let mut app = make_app(true);
    let total = app.tree_rows.len();
    assert!(total >= 2);

    app.selected = 0;
    handle_input(&mut app, key(KeyCode::Down));
    assert_eq!(app.selected, 1);

    handle_input(&mut app, key(KeyCode::Up));
    assert_eq!(app.selected, 0);

    // Up at top stays at 0
    handle_input(&mut app, key(KeyCode::Up));
    assert_eq!(app.selected, 0);
}

#[test]
fn end_key_goes_to_last_row_in_grouped_view() {
    let mut app = make_app(true);
    let total = app.tree_rows.len();

    handle_input(&mut app, key(KeyCode::End));
    assert_eq!(app.selected, total - 1);
}

#[test]
fn flat_view_enter_enters_conversation() {
    let mut app = make_app(false);
    app.selected = 0;

    let action = handle_input(&mut app, key(KeyCode::Enter));
    match action {
        Action::EnterConversation(idx) => {
            assert_eq!(idx, 0);
        }
        _ => panic!("Expected EnterConversation in flat view"),
    }
}

#[test]
fn visible_row_count_reflects_view_mode() {
    let mut app = make_app(true);
    let grouped_count = app.visible_row_count();
    assert_eq!(grouped_count, app.tree_rows.len());

    app.toggle_view();
    let flat_count = app.visible_row_count();
    assert_eq!(flat_count, app.display_entries.len());
}

// ---- Archive tests ----

#[test]
fn a_key_sets_archive_confirm() {
    let mut app = make_app(false);
    app.selected = 0;
    assert!(app.archive_confirm.is_none());

    let action = handle_input(&mut app, key_ctrl('a'));
    assert!(matches!(action, Action::Continue));
    assert_eq!(app.archive_confirm, Some(0));
}

#[test]
fn a_key_in_grouped_view_on_session_sets_confirm() {
    let mut app = make_app_expanded();
    app.selected = 1;
    assert!(matches!(app.tree_rows[1], TreeRow::Session { .. }));

    let action = handle_input(&mut app, key_ctrl('a'));
    assert!(matches!(action, Action::Continue));
    assert!(app.archive_confirm.is_some());
}

#[test]
fn a_key_in_grouped_view_on_project_is_noop() {
    let mut app = make_app(true);
    app.selected = 0;
    assert!(matches!(app.tree_rows[0], TreeRow::Project(_)));

    let action = handle_input(&mut app, key_ctrl('a'));
    assert!(matches!(action, Action::Continue));
    assert!(app.archive_confirm.is_none());
}

#[test]
fn archive_confirm_y_triggers_archive() {
    let mut app = make_app(false);
    app.archive_confirm = Some(0);
    app.mode = Mode::ConfirmArchive;

    let action = handle_input(&mut app, key(KeyCode::Char('y')));
    assert!(matches!(action, Action::ArchiveSession(0)));
    assert!(app.archive_confirm.is_none());
    assert!(matches!(app.mode, Mode::Browsing));
}

#[test]
fn archive_confirm_esc_cancels() {
    let mut app = make_app(false);
    app.archive_confirm = Some(0);
    app.mode = Mode::ConfirmArchive;

    let action = handle_input(&mut app, key(KeyCode::Esc));
    assert!(matches!(action, Action::Continue));
    assert!(app.archive_confirm.is_none());
    assert!(matches!(app.mode, Mode::Browsing));
}

#[test]
fn archive_confirm_other_key_cancels() {
    let mut app = make_app(false);
    app.archive_confirm = Some(0);
    app.mode = Mode::ConfirmArchive;

    let action = handle_input(&mut app, key(KeyCode::Char('n')));
    assert!(matches!(action, Action::Continue));
    assert!(app.archive_confirm.is_none());
    assert!(matches!(app.mode, Mode::Browsing));
}

#[test]
fn a_key_ignored_when_filter_active() {
    let mut app = make_app(false);
    app.filter_active = true;
    app.selected = 0;

    let action = handle_input(&mut app, key(KeyCode::Char('a')));
    assert!(matches!(action, Action::Continue));
    assert!(app.archive_confirm.is_none(), "should not trigger archive when filtering");
}

#[test]
fn ctrl_a_archives_even_when_filter_active() {
    let mut app = make_app(false);
    app.filter_active = true;
    app.selected = 0;

    let action = handle_input(&mut app, key_ctrl('a'));
    assert!(matches!(action, Action::Continue));
    assert_eq!(
        app.archive_confirm,
        Some(0),
        "Ctrl shortcuts must work while filtering"
    );
}

#[test]
fn ctrl_v_moves_even_when_filter_active() {
    let mut app = make_app(false);
    app.filter_active = true;
    app.selected = 0;

    let action = handle_input(&mut app, key_ctrl('v'));
    assert!(matches!(action, Action::Continue));
    assert!(
        app.move_state.is_some(),
        "Ctrl shortcuts must work while filtering"
    );
}

#[test]
#[serial]
fn archive_session_moves_file() {
    let tmp = tempfile::tempdir().unwrap();
    let project_dir = tmp.path().join("projects").join("-Users-test-myproject");
    fs::create_dir_all(&project_dir).unwrap();

    let session_id = "abc123";
    let session_file = project_dir.join(format!("{session_id}.jsonl"));
    fs::write(&session_file, r#"{"type":"user","cwd":"/Users/test/myproject","sessionId":"abc123","message":{"role":"user","content":"hello"},"uuid":"u1","timestamp":"2025-01-01T00:00:00.000Z"}
{"type":"assistant","cwd":"/Users/test/myproject","sessionId":"abc123","message":{"role":"assistant","content":[{"type":"text","text":"hi"}]},"uuid":"u2","timestamp":"2025-01-01T00:01:00.000Z"}
"#).unwrap();

    std::env::set_var("CLAUDE_HOME", tmp.path().to_str().unwrap());

    let sessions = discover_sessions(tmp.path());
    assert!(!sessions.is_empty(), "should discover the test session");

    let session_index = HashMap::new();
    let theme = Theme::dark();
    let mut app = App::new(sessions, session_index, theme, false);
    assert!(!app.display_entries.is_empty());

    let result = app.archive_session(0);
    assert!(result.is_ok(), "archive should succeed: {:?}", result);

    assert!(!session_file.exists(), "original file should be moved");

    let archive_file = tmp.path()
        .join("projects-archive")
        .join("-Users-test-myproject")
        .join(format!("{session_id}.jsonl"));
    assert!(archive_file.exists(), "file should exist in archive");

    std::env::remove_var("CLAUDE_HOME");
}

#[test]
fn ctrl_v_on_session_starts_move() {
    let mut app = make_app(false);
    app.selected = 0;

    let action = handle_input(&mut app, key_ctrl('v'));
    assert!(matches!(action, Action::Continue));
    assert!(matches!(app.mode, Mode::MoveSelectProject));
    assert!(app.move_state.is_some());
}

#[test]
fn m_key_ignored_when_filter_active() {
    let mut app = make_app(false);
    app.filter_active = true;
    app.selected = 0;

    let action = handle_input(&mut app, key(KeyCode::Char('m')));
    assert!(matches!(action, Action::Continue));
    assert!(app.move_state.is_none(), "should not start move when filtering");
}

#[test]
fn ctrl_v_in_grouped_view_on_project_is_noop() {
    let mut app = make_app(true);
    app.selected = 0;
    assert!(matches!(app.tree_rows[0], TreeRow::Project(_)));

    let action = handle_input(&mut app, key_ctrl('v'));
    assert!(matches!(action, Action::Continue));
    assert!(app.move_state.is_none());
}

#[test]
fn move_picker_esc_cancels() {
    let mut app = make_app(false);
    app.selected = 0;
    handle_input(&mut app, key_ctrl('v'));
    assert!(matches!(app.mode, Mode::MoveSelectProject));

    let action = handle_input(&mut app, key(KeyCode::Esc));
    assert!(matches!(action, Action::Continue));
    assert!(app.move_state.is_none());
    assert!(matches!(app.mode, Mode::Browsing));
}

#[test]
fn move_picker_navigation() {
    let mut app = make_app(false);
    app.selected = 0;
    handle_input(&mut app, key_ctrl('v'));

    let project_count = app.move_state.as_ref().unwrap().projects.len();
    if project_count > 1 {
        handle_input(&mut app, key(KeyCode::Down));
        assert_eq!(app.move_state.as_ref().unwrap().selected, 1);

        handle_input(&mut app, key(KeyCode::Up));
        assert_eq!(app.move_state.as_ref().unwrap().selected, 0);
    }
}

#[test]
fn move_picker_enter_triggers_move() {
    let mut app = make_app(false);
    app.selected = 0;
    handle_input(&mut app, key_ctrl('v'));

    if app.move_state.is_some() {
        let action = handle_input(&mut app, key(KeyCode::Enter));
        assert!(matches!(action, Action::MoveSession { .. }));
        assert!(matches!(app.mode, Mode::Browsing));
    }
}

#[test]
#[serial]
fn move_session_moves_file() {
    let tmp = tempfile::tempdir().unwrap();
    let src_dir = tmp.path().join("projects").join("-Users-test-src-project");
    let dst_dir_name = "-Users-test-dst-project";
    fs::create_dir_all(&src_dir).unwrap();
    fs::create_dir_all(tmp.path().join("projects").join(dst_dir_name)).unwrap();

    let session_id = "move-test-123";
    let session_file = src_dir.join(format!("{session_id}.jsonl"));
    fs::write(&session_file, r#"{"type":"user","cwd":"/Users/test/src-project","sessionId":"move-test-123","message":{"role":"user","content":"test move"},"uuid":"u1","timestamp":"2025-01-01T00:00:00.000Z"}
{"type":"assistant","cwd":"/Users/test/src-project","sessionId":"move-test-123","message":{"role":"assistant","content":[{"type":"text","text":"ok"}]},"uuid":"u2","timestamp":"2025-01-01T00:01:00.000Z"}
"#).unwrap();

    std::env::set_var("CLAUDE_HOME", tmp.path().to_str().unwrap());

    let sessions = discover_sessions(tmp.path());
    assert!(!sessions.is_empty(), "should discover the test session");

    let session_index = HashMap::new();
    let theme = Theme::dark();
    let mut app = App::new(sessions, session_index, theme, false);

    let target_cwd = "/Users/test/dst-project";
    let result = app.move_session(0, dst_dir_name, target_cwd);
    assert!(result.is_ok(), "move should succeed: {:?}", result);

    assert!(!session_file.exists(), "original file should be gone");

    let moved_file = tmp.path()
        .join("projects")
        .join(dst_dir_name)
        .join(format!("{session_id}.jsonl"));
    assert!(moved_file.exists(), "file should exist in target project");

    let content = fs::read_to_string(&moved_file).unwrap();
    assert!(content.contains(target_cwd), "cwd should be updated to target");
    assert!(!content.contains("/Users/test/src-project"), "old cwd should be replaced");

    std::env::remove_var("CLAUDE_HOME");
}
