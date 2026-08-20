use std::path::PathBuf;

use agent_session::discovery::discover_sessions;
use agent_session::filter::filter_sessions;
use agent_session::tui::{
    build_tree_rows, group_by_project, DisplayEntry, DisplaySource, MatchType, TreeRow,
};

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn make_display_entries(sessions: &[agent_session::session::Session]) -> Vec<DisplayEntry> {
    sessions
        .iter()
        .enumerate()
        .map(|(idx, s)| DisplayEntry {
            match_type: MatchType::Metadata,
            source: DisplaySource::Sessions(idx),
            timestamp: s.timestamp,
        })
        .collect()
}

#[test]
fn group_by_project_creates_correct_groups() {
    let sessions = discover_sessions(&fixture_dir());
    let entries = make_display_entries(&sessions);
    let groups = group_by_project(&sessions, &entries);
    assert_eq!(groups.len(), 2, "should create 2 project groups");

    let names: Vec<&str> = groups.iter().map(|g| g.name.as_str()).collect();
    assert!(names.contains(&"project-a"));
    assert!(names.contains(&"project-b"));
}

#[test]
fn group_by_project_correct_session_counts() {
    let sessions = discover_sessions(&fixture_dir());
    let entries = make_display_entries(&sessions);
    let groups = group_by_project(&sessions, &entries);

    let group_a = groups.iter().find(|g| g.name == "project-a").unwrap();
    let group_b = groups.iter().find(|g| g.name == "project-b").unwrap();

    assert_eq!(
        group_a.session_indices.len(),
        2,
        "project-a should have 2 sessions"
    );
    assert_eq!(
        group_b.session_indices.len(),
        1,
        "project-b should have 1 session"
    );
}

#[test]
fn claude_and_codex_in_same_dir_merge_into_one_group() {
    use agent_session::session::{Agent, Session};
    use chrono::Utc;

    let cwd = "/Users/test/shared-project";
    let sessions = vec![
        Session {
            id: "claude-1".into(),
            // Claude keys its project on an encoded dir name...
            project_path: "-Users-test-shared-project".into(),
            project_name: "shared-project".into(),
            git_branch: None,
            timestamp: Utc::now(),
            first_message: "claude turn".into(),
            cwd: cwd.into(),
            project_exists: false,
            custom_title: None,
            agent: Agent::Claude,
            source_path: None,
        },
        Session {
            id: "codex-1".into(),
            // ...while Codex keys on the raw cwd; they must still merge.
            project_path: cwd.into(),
            project_name: "shared-project".into(),
            git_branch: None,
            timestamp: Utc::now(),
            first_message: "codex turn".into(),
            cwd: cwd.into(),
            project_exists: false,
            custom_title: None,
            agent: Agent::Codex,
            source_path: Some("/tmp/rollout.jsonl".into()),
        },
    ];

    let entries = make_display_entries(&sessions);
    let groups = group_by_project(&sessions, &entries);

    assert_eq!(groups.len(), 1, "same cwd should merge across agents");
    assert_eq!(groups[0].session_indices.len(), 2);
    assert_eq!(groups[0].name, "shared-project");
}

#[test]
fn groups_sorted_by_latest_activity() {
    let sessions = discover_sessions(&fixture_dir());
    let entries = make_display_entries(&sessions);
    let groups = group_by_project(&sessions, &entries);

    // project-a has newer sessions (2025-02-20) than project-b (2025-02-18)
    assert_eq!(
        groups[0].name, "project-a",
        "newest project should be first"
    );
    assert_eq!(
        groups[1].name, "project-b",
        "older project should be second"
    );
}

#[test]
fn groups_default_to_collapsed() {
    let sessions = discover_sessions(&fixture_dir());
    let entries = make_display_entries(&sessions);
    let groups = group_by_project(&sessions, &entries);

    for group in &groups {
        assert!(!group.expanded, "groups should default to collapsed");
    }
}

#[test]
fn build_tree_rows_default_collapsed() {
    let sessions = discover_sessions(&fixture_dir());
    let entries = make_display_entries(&sessions);
    let groups = group_by_project(&sessions, &entries);
    let rows = build_tree_rows(&groups);

    // Default is collapsed: only 2 project headers
    assert_eq!(rows.len(), 2, "default collapsed: only 2 project headers");
    assert!(matches!(rows[0], TreeRow::Project(0)));
    assert!(matches!(rows[1], TreeRow::Project(1)));
}

#[test]
fn build_tree_rows_all_expanded() {
    let sessions = discover_sessions(&fixture_dir());
    let entries = make_display_entries(&sessions);
    let mut groups = group_by_project(&sessions, &entries);

    groups[0].expanded = true;
    groups[1].expanded = true;
    let rows = build_tree_rows(&groups);

    // 2 project headers + 3 sessions = 5 rows
    assert_eq!(rows.len(), 5, "all expanded: 2 headers + 3 sessions");

    assert!(matches!(rows[0], TreeRow::Project(0)));
    assert!(matches!(rows[1], TreeRow::Session { project_idx: 0, .. }));
    assert!(matches!(rows[2], TreeRow::Session { project_idx: 0, .. }));
    assert!(matches!(rows[3], TreeRow::Project(1)));
    assert!(matches!(rows[4], TreeRow::Session { project_idx: 1, .. }));
}

#[test]
fn build_tree_rows_one_expanded() {
    let sessions = discover_sessions(&fixture_dir());
    let entries = make_display_entries(&sessions);
    let mut groups = group_by_project(&sessions, &entries);

    // Expand only project-b (second group)
    groups[1].expanded = true;
    let rows = build_tree_rows(&groups);

    // 2 project headers + 1 session from project-b = 3 rows
    assert_eq!(rows.len(), 3, "one expanded: 2 headers + 1 session");

    assert!(matches!(rows[0], TreeRow::Project(0)));
    assert!(matches!(rows[1], TreeRow::Project(1)));
    assert!(matches!(rows[2], TreeRow::Session { project_idx: 1, .. }));
}

#[test]
fn empty_sessions_no_groups() {
    let groups = group_by_project(&[], &[]);
    assert!(groups.is_empty());
    let rows = build_tree_rows(&groups);
    assert!(rows.is_empty());
}

#[test]
fn filter_only_shows_matching_groups() {
    let sessions = discover_sessions(&fixture_dir());
    let filtered = filter_sessions(&sessions, "project-b");

    let entries: Vec<DisplayEntry> = filtered
        .iter()
        .map(|&idx| DisplayEntry {
            match_type: MatchType::Metadata,
            source: DisplaySource::Sessions(idx),
            timestamp: sessions[idx].timestamp,
        })
        .collect();

    let groups = group_by_project(&sessions, &entries);
    assert_eq!(groups.len(), 1, "filter should leave only project-b");
    assert_eq!(groups[0].name, "project-b");
}

#[test]
fn tree_row_equality() {
    assert_eq!(TreeRow::Project(0), TreeRow::Project(0));
    assert_ne!(TreeRow::Project(0), TreeRow::Project(1));
    assert_eq!(
        TreeRow::Session {
            project_idx: 0,
            display_idx: 1
        },
        TreeRow::Session {
            project_idx: 0,
            display_idx: 1
        }
    );
    assert_ne!(
        TreeRow::Session {
            project_idx: 0,
            display_idx: 1
        },
        TreeRow::Session {
            project_idx: 0,
            display_idx: 2
        }
    );
}
