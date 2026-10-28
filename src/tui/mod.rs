pub mod input;
mod launch;
pub mod syntax;
pub mod table;
pub mod view;

use std::collections::{HashMap, HashSet};
use std::io::stdout;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use crossterm::event::{self, Event};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::prelude::*;

use crate::discovery::{get_claude_home, load_conversation};
use crate::filter::filter_sessions;
use crate::search;
use crate::session::{ConversationMessage, Session};
use crate::theme::Theme;

use input::handle_input;
use launch::{wait_for_agent, NewSessionLaunch};

/// TUI interaction mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Browsing,
    Conversation,
    ConversationSearch,
    ConfirmArchive,
    MoveSelectProject,
    ForkSelectAgent,
    NewSelectAgent,
    NewSelectProfile,
    ProfileSelect,
    TitleEdit,
}

/// Phase of the background content search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentSearchState {
    Idle,
    Debouncing,
    Searching,
    Complete,
}

/// How a session matched the search query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchType {
    Metadata,
    Content,
    Both,
}

/// Reference to a session in either the main sessions list or content results.
#[derive(Debug, Clone)]
pub enum DisplaySource {
    Sessions(usize),
    Content(usize),
}

/// A single entry in the merged search results display.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct DisplayEntry {
    pub match_type: MatchType,
    pub source: DisplaySource,
    pub timestamp: DateTime<Utc>,
}

/// What the input handler tells the main loop to do.
pub enum Action {
    Continue,
    Quit,
    EnterConversation(usize),
    CopyCommand {
        cmd: String,
        title: Option<String>,
    },
    NewSession {
        cmd: String,
        title: Option<String>,
        agent: crate::session::Agent,
        profile: Option<String>,
        cwd: String,
    },
    ForkSession {
        cmd: String,
        title: Option<String>,
    },
    CloneSession {
        cmd: String,
        title: Option<String>,
        warning: Option<String>,
    },
    BackToList,
    ArchiveSession(usize),
    MoveSession {
        display_idx: usize,
        target_project: String,
        target_cwd: String,
    },
}

/// A group of sessions belonging to the same project directory.
#[derive(Debug, Clone)]
pub struct ProjectGroup {
    pub name: String,
    pub path: String,
    pub cwd: String,
    pub session_indices: Vec<usize>,
    pub expanded: bool,
}

/// A single row in the tree view: either a project header or a session under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeRow {
    Project(usize),
    Session {
        project_idx: usize,
        display_idx: usize,
    },
}

/// Encode a working directory into Claude's project-dir naming scheme
/// (`/` and `.` become `-`). Used when a move target only has Codex sessions,
/// so a Claude source can still be relocated into that directory.
fn encode_claude_dir(cwd: &str) -> String {
    cwd.replace(['/', '.'], "-")
}

/// Build project groups from a flat list of sessions.
/// Groups are sorted by the latest session timestamp (newest group first).
/// Sessions within each group are ordered by display name (title), so equally
/// titled sessions - e.g. the same session across providers - appear adjacent.
pub fn group_by_project(
    sessions: &[Session],
    display_entries: &[DisplayEntry],
) -> Vec<ProjectGroup> {
    group_by_project_with_content(sessions, display_entries, &[])
}

/// Build project groups, including content-only search results.
pub fn group_by_project_with_content(
    sessions: &[Session],
    display_entries: &[DisplayEntry],
    content_results: &[Session],
) -> Vec<ProjectGroup> {
    let mut group_map: std::collections::HashMap<String, Vec<usize>> =
        std::collections::HashMap::new();

    for (di, entry) in display_entries.iter().enumerate() {
        let session = match &entry.source {
            DisplaySource::Sessions(idx) => &sessions[*idx],
            DisplaySource::Content(idx) => &content_results[*idx],
        };
        // Group by the real working directory so Claude and Codex sessions in
        // the same project merge into one group (Claude keys on an encoded dir
        // name, Codex on the raw cwd; the cwd is the common denominator).
        let group_key = if session.cwd.is_empty() {
            session.project_path.clone()
        } else {
            session.cwd.clone()
        };
        group_map.entry(group_key).or_default().push(di);
    }

    // Resolve the session behind a display index (a normal session or a
    // content-only search hit).
    let session_at = |di: usize| -> Option<&Session> {
        match &display_entries.get(di)?.source {
            DisplaySource::Sessions(idx) => sessions.get(*idx),
            DisplaySource::Content(idx) => content_results.get(*idx),
        }
    };
    // Sort key for a session row: the displayed name (custom title, else first
    // message), normalized so a title and its clones cluster together. Clone
    // suffixes added on fork/transfer - "(reconstruct)", "(fork)", "(seed)",
    // possibly stacked - don't change identity, so we strip any trailing stack of
    // them, then lower-case for case-insensitive grouping.
    fn sort_title(s: &Session) -> String {
        let raw = s
            .custom_title
            .clone()
            .unwrap_or_else(|| s.first_message.clone());
        let mut base = raw.trim();
        loop {
            let trimmed = base.trim_end();
            match ["(reconstruct)", "(fork)", "(seed)"]
                .iter()
                .find_map(|suf| trimmed.strip_suffix(suf))
            {
                Some(rest) => base = rest.trim_end(),
                None => break,
            }
        }
        base.to_lowercase()
    }

    let mut groups: Vec<ProjectGroup> = group_map
        .into_iter()
        .map(|(path, mut indices)| {
            // Cluster identically titled sessions together - e.g. the same
            // session forked or transferred across providers. Clusters are
            // ordered by recency (the most recently used cluster first) and,
            // within a cluster, newest first, so the most recently used variant
            // sits at the very top of its cluster.
            let mut cluster_recency: HashMap<String, DateTime<Utc>> = HashMap::new();
            for &di in &indices {
                if let Some(s) = session_at(di) {
                    let ts = display_entries[di].timestamp;
                    cluster_recency
                        .entry(sort_title(s))
                        .and_modify(|m| {
                            if ts > *m {
                                *m = ts;
                            }
                        })
                        .or_insert(ts);
                }
            }
            indices.sort_by(|&a, &b| {
                let ta = session_at(a).map(sort_title).unwrap_or_default();
                let tb = session_at(b).map(sort_title).unwrap_or_default();
                let ra = cluster_recency
                    .get(&ta)
                    .copied()
                    .unwrap_or(DateTime::<Utc>::MIN_UTC);
                let rb = cluster_recency
                    .get(&tb)
                    .copied()
                    .unwrap_or(DateTime::<Utc>::MIN_UTC);
                // Most recent cluster first; keep same-title together (needed when
                // two clusters share the same recency); newest within a cluster first.
                rb.cmp(&ra).then_with(|| ta.cmp(&tb)).then_with(|| {
                    display_entries[b]
                        .timestamp
                        .cmp(&display_entries[a].timestamp)
                })
            });
            let first_session = indices.first().and_then(|&di| session_at(di));
            let name = first_session
                .map(|s| s.project_name.clone())
                .unwrap_or_else(|| "unknown".to_string());
            let cwd = first_session.map(|s| s.cwd.clone()).unwrap_or_default();
            ProjectGroup {
                name,
                path,
                cwd,
                session_indices: indices,
                expanded: false,
            }
        })
        .collect();

    // Sort groups by latest activity (newest first). Because sessions within a
    // group are now name-ordered rather than time-ordered, use the maximum
    // timestamp across the group instead of its first entry.
    groups.sort_by(|a, b| {
        let latest = |g: &ProjectGroup| {
            g.session_indices
                .iter()
                .map(|&i| display_entries[i].timestamp)
                .max()
                .unwrap_or(DateTime::<Utc>::MIN_UTC)
        };
        latest(b).cmp(&latest(a))
    });

    groups
}

/// Build the flat list of tree rows from project groups, respecting expanded state.
pub fn build_tree_rows(groups: &[ProjectGroup]) -> Vec<TreeRow> {
    let mut rows = Vec::new();
    for (gi, group) in groups.iter().enumerate() {
        rows.push(TreeRow::Project(gi));
        if group.expanded {
            for &di in &group.session_indices {
                rows.push(TreeRow::Session {
                    project_idx: gi,
                    display_idx: di,
                });
            }
        }
    }
    rows
}

/// State for the project picker when moving a session.
pub struct MoveState {
    pub display_idx: usize,
    pub projects: Vec<(String, String, String)>, // (encoded_path, display_name, cwd)
    pub selected: usize,
}

/// State for the target-agent picker when forking a session.
pub struct ForkState {
    pub session: Session,
    pub options: Vec<crate::session::Agent>,
    pub selected: usize,
}

/// State for the agent picker shown when starting a new session, so the user
/// chooses which AI (Claude / Codex / Cursor) to launch.
pub struct NewSessionState {
    pub cwd: String,
    pub options: Vec<crate::session::Agent>,
    pub selected: usize,
}

/// Profile picker state for a session that has not been created yet.
pub struct NewProfileState {
    pub cwd: String,
    pub agent: crate::session::Agent,
    pub options: Vec<String>,
    pub selected: usize,
}

/// State for the per-session profile picker. Supported for Claude (whose
/// profiles map to `_claude_run_<name>` shell launchers) and Codex (whose
/// profiles are native `--profile` config sections). Each option is a profile to
/// set, or `None` for the "clear tag" choice (fall back to the global default).
pub struct ProfileState {
    pub session_id: String,
    pub agent: crate::session::Agent,
    pub options: Vec<Option<String>>,
    pub selected: usize,
}

/// What the title edit is for.
pub enum TitleEditContext {
    Rename { session: Session },
    NewSession { cwd: String, profile: String },
    Fork { session_id: String, cwd: String },
}

/// State for title editing.
pub struct TitleEditState {
    pub context: TitleEditContext,
    pub query: String,
    pub cursor: usize,
    pub return_mode: Mode,
}

fn same_session_record(a: &Session, b: &Session) -> bool {
    if a.agent != b.agent || a.id != b.id {
        return false;
    }
    match (&a.source_path, &b.source_path) {
        (Some(a_path), Some(b_path)) => a_path == b_path,
        _ => a.project_path == b.project_path,
    }
}

/// State for the conversation viewer.
pub struct ConversationState {
    pub session: Session,
    pub messages: Vec<ConversationMessage>,
    pub lines: Vec<Line<'static>>,
    pub scroll_offset: usize,
    pub page_height: usize,
    pub rendered_width: u16,
    pub search_query: String,
    pub search_active: bool,
    pub search_confirmed: bool,
    /// First keystroke in search replaces the pre-filled text
    pub search_replacing: bool,
    /// Cursor position within search_query for editing
    pub search_cursor: usize,
    pub match_positions: Vec<usize>,
    pub current_match: usize,
    pub initial_search_terms: Vec<String>,
}

/// Application state for the TUI.
pub struct App {
    pub sessions: Vec<Session>,
    pub filtered_indices: Vec<usize>,
    pub display_entries: Vec<DisplayEntry>,
    pub selected: usize,
    pub scroll_offset: usize,
    pub mode: Mode,
    pub filter_query: String,
    /// Whether the filter UI indicator is shown (activated by / or typing)
    pub filter_active: bool,
    pub status_message: Option<(String, Instant)>,
    pub conversation: Option<ConversationState>,
    /// Content-only search results from background search.
    pub content_results: Vec<Session>,
    /// Current phase of content search.
    pub content_search_state: ContentSearchState,
    /// When the last filter keystroke occurred, for debounce.
    pub last_keystroke: Option<Instant>,
    /// Flag to cancel in-progress content search.
    pub cancel_flag: Arc<AtomicBool>,
    /// Receiver for background content search results.
    pub search_receiver: Option<mpsc::Receiver<Vec<Session>>>,
    /// Spinner frame counter.
    pub spinner_tick: usize,
    /// Pre-built file-path-to-session index for fast content search.
    pub session_index: Arc<HashMap<PathBuf, Session>>,
    /// Active color theme.
    pub theme: Theme,
    /// Syntax highlighter for code blocks.
    pub syntax_highlighter: syntax::SyntaxHighlighter,
    /// Project groups for tree view.
    pub project_groups: Vec<ProjectGroup>,
    /// Flattened tree rows for the grouped view.
    pub tree_rows: Vec<TreeRow>,
    /// Whether the grouped (tree) view is active.
    pub grouped_view: bool,
    /// Pending archive confirmation: display_idx of session to archive.
    pub archive_confirm: Option<usize>,
    /// State for the move-to-project picker.
    pub move_state: Option<MoveState>,
    /// State for the fork target-agent picker.
    pub fork_state: Option<ForkState>,
    /// State for the new-session agent picker.
    pub new_session_state: Option<NewSessionState>,
    /// State for the new-session profile picker.
    pub new_profile_state: Option<NewProfileState>,
    /// Title being edited.
    pub title_edit: Option<TitleEditState>,
    /// Cached default claude profile label from `~/.claude-default-profile`.
    pub profile_label: Option<String>,
    /// Cached default Codex profile label from `~/.codex-default-profile`.
    pub codex_profile_label: Option<String>,
    /// State for the per-session profile picker.
    pub profile_state: Option<ProfileState>,
    /// Per-session profile tags (session id -> profile), loaded from the sidecar.
    pub session_profiles: HashMap<String, String>,
    last_profile_refresh: Instant,
    /// When true, the list shows archived sessions (from the `*-archive`
    /// directories) instead of the live ones, and the only session action is
    /// restore.
    pub viewing_archived: bool,
}

impl App {
    pub fn new(
        sessions: Vec<Session>,
        session_index: HashMap<PathBuf, Session>,
        theme: Theme,
        grouped_view: bool,
    ) -> Self {
        let filtered_indices: Vec<usize> = (0..sessions.len()).collect();
        let display_entries: Vec<DisplayEntry> = filtered_indices
            .iter()
            .map(|&idx| DisplayEntry {
                match_type: MatchType::Metadata,
                source: DisplaySource::Sessions(idx),
                timestamp: sessions[idx].timestamp,
            })
            .collect();
        let project_groups = group_by_project(&sessions, &display_entries);
        let tree_rows = build_tree_rows(&project_groups);
        Self {
            sessions,
            filtered_indices,
            display_entries,
            selected: 0,
            scroll_offset: 0,
            mode: Mode::Browsing,
            filter_query: String::new(),
            filter_active: false,
            status_message: None,
            conversation: None,
            content_results: Vec::new(),
            content_search_state: ContentSearchState::Idle,
            last_keystroke: None,
            cancel_flag: Arc::new(AtomicBool::new(false)),
            search_receiver: None,
            spinner_tick: 0,
            session_index: Arc::new(session_index),
            theme,
            syntax_highlighter: syntax::SyntaxHighlighter::new(),
            project_groups,
            tree_rows,
            grouped_view,
            archive_confirm: None,
            move_state: None,
            fork_state: None,
            new_session_state: None,
            new_profile_state: None,
            title_edit: None,
            profile_label: crate::router::profile_name(),
            codex_profile_label: crate::router::codex_profile_name(),
            profile_state: None,
            session_profiles: {
                // Merge the Claude and Codex profile maps: session ids are unique
                // UUIDs across agents, so a single lookup table serves both.
                let mut m = crate::router::load_session_profiles(&get_claude_home());
                m.extend(crate::router::load_session_profiles(
                    &crate::codex::get_codex_home(),
                ));
                m
            },
            viewing_archived: false,
            last_profile_refresh: Instant::now(),
        }
    }

    /// Re-run the metadata filter and rebuild display entries.
    pub fn apply_filter(&mut self) {
        self.filtered_indices = filter_sessions(&self.sessions, &self.filter_query);
        self.rebuild_display_entries();
        self.selected = 0;
        self.scroll_offset = 0;
    }

    /// Build merged display entries from metadata matches and content results.
    pub fn rebuild_display_entries(&mut self) {
        let content_ids: HashSet<&str> =
            self.content_results.iter().map(|s| s.id.as_str()).collect();
        let metadata_ids: HashSet<&str> = self
            .filtered_indices
            .iter()
            .map(|&idx| self.sessions[idx].id.as_str())
            .collect();

        let mut entries = Vec::new();

        for &idx in &self.filtered_indices {
            let session = &self.sessions[idx];
            let match_type = if content_ids.contains(session.id.as_str()) {
                MatchType::Both
            } else {
                MatchType::Metadata
            };
            entries.push(DisplayEntry {
                match_type,
                source: DisplaySource::Sessions(idx),
                timestamp: session.timestamp,
            });
        }

        for (i, session) in self.content_results.iter().enumerate() {
            if !metadata_ids.contains(session.id.as_str()) {
                entries.push(DisplayEntry {
                    match_type: MatchType::Content,
                    source: DisplaySource::Content(i),
                    timestamp: session.timestamp,
                });
            }
        }

        entries.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        self.display_entries = entries;
        self.rebuild_tree_rows();
    }

    /// Rebuild project groups and tree rows from current display entries.
    pub fn rebuild_tree_rows(&mut self) {
        let has_filter = !self.filter_query.is_empty();
        let old_expanded: HashMap<String, bool> = self
            .project_groups
            .iter()
            .map(|g| (g.path.clone(), g.expanded))
            .collect();
        self.project_groups = group_by_project_with_content(
            &self.sessions,
            &self.display_entries,
            &self.content_results,
        );
        for group in &mut self.project_groups {
            if has_filter {
                group.expanded = true;
            } else if let Some(&was_expanded) = old_expanded.get(&group.path) {
                group.expanded = was_expanded;
            }
        }
        self.tree_rows = build_tree_rows(&self.project_groups);
    }

    /// Toggle a project group's expanded/collapsed state.
    pub fn toggle_project(&mut self, group_idx: usize) {
        if group_idx < self.project_groups.len() {
            self.project_groups[group_idx].expanded = !self.project_groups[group_idx].expanded;
            self.tree_rows = build_tree_rows(&self.project_groups);
        }
    }

    /// Toggle between flat list and grouped tree view.
    pub fn toggle_view(&mut self) {
        self.grouped_view = !self.grouped_view;
        self.selected = 0;
        self.scroll_offset = 0;
    }

    /// Get the currently selected tree row, if in grouped view.
    pub fn selected_tree_row(&self) -> Option<&TreeRow> {
        self.tree_rows.get(self.selected)
    }

    /// Total number of visible rows in the current view mode.
    pub fn visible_row_count(&self) -> usize {
        if self.grouped_view {
            self.tree_rows.len()
        } else {
            self.display_entries.len()
        }
    }

    /// Get the session referenced by a display entry.
    pub fn display_session(&self, entry: &DisplayEntry) -> &Session {
        match &entry.source {
            DisplaySource::Sessions(idx) => &self.sessions[*idx],
            DisplaySource::Content(idx) => &self.content_results[*idx],
        }
    }

    /// Cancel any in-progress content search and clear results.
    pub fn cancel_content_search(&mut self) {
        self.cancel_flag.store(true, Ordering::Relaxed);
        self.search_receiver = None;
        self.content_results.clear();
        self.content_search_state = ContentSearchState::Idle;
        self.last_keystroke = None;
    }

    /// Move the selection cursor down, clamped to bounds.
    pub fn move_down(&mut self) {
        let count = self.visible_row_count();
        if count > 0 {
            self.selected = (self.selected + 1).min(count - 1);
        }
    }

    /// Move the selection cursor up, clamped to bounds.
    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Ensure the selected item is visible by adjusting scroll_offset.
    pub fn ensure_visible(&mut self, visible_items: usize) {
        if visible_items == 0 {
            return;
        }
        if self.mode == Mode::Conversation || self.mode == Mode::ConversationSearch {
            return;
        }
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        } else if self.selected >= self.scroll_offset + visible_items {
            self.scroll_offset = self.selected - visible_items + 1;
        }
    }

    /// Enter conversation viewer for a display entry.
    pub fn enter_conversation(&mut self, display_idx: usize) {
        if display_idx >= self.display_entries.len() {
            return;
        }
        let entry = &self.display_entries[display_idx];
        let session = self.display_session(entry).clone();
        let messages = match session.agent {
            crate::session::Agent::Codex => crate::codex::load_codex_conversation(&session),
            crate::session::Agent::Cursor => crate::cursor::load_cursor_conversation(&session),
            crate::session::Agent::Claude => {
                let claude_home = get_claude_home();
                load_conversation(&claude_home, &session)
            }
        };

        let initial_search_terms = crate::filter::parse_keywords(&self.filter_query);

        self.conversation = Some(ConversationState {
            session,
            messages,
            lines: Vec::new(),
            scroll_offset: 0,
            page_height: 20,
            rendered_width: 0,
            search_query: String::new(),
            search_active: false,
            search_confirmed: false,
            search_replacing: false,
            search_cursor: 0,
            match_positions: Vec::new(),
            current_match: 0,
            initial_search_terms,
        });
        self.mode = Mode::Conversation;
    }

    /// Leave conversation viewer and return to the list.
    pub fn leave_conversation(&mut self) {
        self.conversation = None;
        self.mode = Mode::Browsing;
    }

    /// Archive a session by moving its JSONL file to a projects-archive/ directory.
    /// Returns Ok(session_name) on success.
    pub fn archive_session(&mut self, display_idx: usize) -> Result<String, String> {
        if display_idx >= self.display_entries.len() {
            return Err("invalid index".to_string());
        }
        let entry = &self.display_entries[display_idx];
        let session = self.display_session(entry).clone();

        match session.agent {
            crate::session::Agent::Codex => {
                let source = session
                    .source_path
                    .as_deref()
                    .ok_or("codex session missing source path")?;
                crate::codex::archive_codex_session(&crate::codex::get_codex_home(), source)?;
            }
            crate::session::Agent::Claude => {
                let claude_home = get_claude_home();
                let src = claude_home
                    .join("projects")
                    .join(&session.project_path)
                    .join(format!("{}.jsonl", session.id));
                let archive_dir = claude_home
                    .join("projects-archive")
                    .join(&session.project_path);

                std::fs::create_dir_all(&archive_dir)
                    .map_err(|e| format!("failed to create archive dir: {e}"))?;

                let dst = archive_dir.join(format!("{}.jsonl", session.id));
                std::fs::rename(&src, &dst).map_err(|e| format!("failed to move session: {e}"))?;
            }
            crate::session::Agent::Cursor => {
                let source = session
                    .source_path
                    .as_deref()
                    .ok_or("cursor session missing source path")?;
                crate::cursor::archive_cursor_session(source)?;
            }
        }

        let label = session.first_message.chars().take(40).collect::<String>();

        match entry.source {
            DisplaySource::Sessions(sidx) => {
                if sidx < self.sessions.len() && self.sessions[sidx].id == session.id {
                    self.sessions.remove(sidx);
                }
            }
            DisplaySource::Content(cidx) => {
                if cidx < self.content_results.len() && self.content_results[cidx].id == session.id
                {
                    self.content_results.remove(cidx);
                }
            }
        }
        self.apply_filter();

        Ok(label)
    }

    /// Toggle between the live session list and the archived list, reloading
    /// sessions from the corresponding directories.
    pub fn toggle_archived(&mut self) {
        self.viewing_archived = !self.viewing_archived;
        self.reload_sessions();
        if self.viewing_archived {
            self.set_status("Archived sessions (^r back, ^u restore)".to_string());
        } else {
            self.set_status("Active sessions".to_string());
        }
    }

    /// Reload `self.sessions` from either the live or archived directories
    /// (per `viewing_archived`), clearing any active search/filter first.
    fn reload_sessions(&mut self) {
        self.cancel_content_search();
        self.filter_query.clear();
        self.filter_active = false;

        let claude_home = get_claude_home();
        let codex_home = crate::codex::get_codex_home();
        let cursor_home = crate::cursor::get_cursor_home();

        let mut sessions = if self.viewing_archived {
            let mut s = crate::discovery::discover_archived_sessions(&claude_home);
            s.extend(crate::codex::discover_archived_codex_sessions(&codex_home));
            s.extend(crate::cursor::discover_archived_cursor_sessions(
                &cursor_home,
            ));
            s
        } else {
            let mut s = crate::discovery::discover_sessions(&claude_home);
            s.extend(crate::codex::discover_codex_sessions(&codex_home));
            s.extend(crate::cursor::discover_cursor_sessions(&cursor_home));
            s
        };
        sessions.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        self.sessions = sessions;
        self.apply_filter();
    }

    /// Restore an archived session by moving its file(s) back to the live
    /// directory. Returns Ok(session_name) on success. Mirrors
    /// [`App::archive_session`] in reverse.
    pub fn restore_session(&mut self, display_idx: usize) -> Result<String, String> {
        if display_idx >= self.display_entries.len() {
            return Err("invalid index".to_string());
        }
        let entry = &self.display_entries[display_idx];
        let session = self.display_session(entry).clone();
        let entry_source = entry.source.clone();

        match session.agent {
            crate::session::Agent::Codex => {
                let source = session
                    .source_path
                    .as_deref()
                    .ok_or("codex session missing source path")?;
                crate::codex::restore_codex_session(&crate::codex::get_codex_home(), source)?;
            }
            crate::session::Agent::Claude => {
                let claude_home = get_claude_home();
                let src = claude_home
                    .join("projects-archive")
                    .join(&session.project_path)
                    .join(format!("{}.jsonl", session.id));
                let live_dir = claude_home.join("projects").join(&session.project_path);

                std::fs::create_dir_all(&live_dir)
                    .map_err(|e| format!("failed to create projects dir: {e}"))?;

                let dst = live_dir.join(format!("{}.jsonl", session.id));
                std::fs::rename(&src, &dst)
                    .map_err(|e| format!("failed to restore session: {e}"))?;
            }
            crate::session::Agent::Cursor => {
                let source = session
                    .source_path
                    .as_deref()
                    .ok_or("cursor session missing source path")?;
                crate::cursor::restore_cursor_session(source)?;
            }
        }

        let label = session.first_message.chars().take(40).collect::<String>();

        match entry_source {
            DisplaySource::Sessions(sidx) => {
                if sidx < self.sessions.len() && self.sessions[sidx].id == session.id {
                    self.sessions.remove(sidx);
                }
            }
            DisplaySource::Content(cidx) => {
                if cidx < self.content_results.len() && self.content_results[cidx].id == session.id
                {
                    self.content_results.remove(cidx);
                }
            }
        }
        self.apply_filter();

        Ok(label)
    }

    /// Start the move-to-project flow for a given session.
    pub fn start_move(&mut self, display_idx: usize) {
        if display_idx >= self.display_entries.len() {
            return;
        }
        let entry = &self.display_entries[display_idx];
        let move_session = self.display_session(entry);
        let current_cwd = move_session.cwd.clone();

        // Build the target list keyed by cwd so Claude and Codex projects share
        // one entry per directory. Prefer a real Claude encoded dir when one
        // exists at that cwd; otherwise derive it so a Claude source can still
        // move there. Codex sources only ever need the target cwd.
        let mut seen = HashSet::new();
        let mut projects: Vec<(String, String, String)> = Vec::new();
        for s in &self.sessions {
            if s.cwd == current_cwd || s.cwd.is_empty() {
                continue;
            }
            if !seen.insert(s.cwd.clone()) {
                continue;
            }
            let encoded = self
                .sessions
                .iter()
                .find(|c| c.cwd == s.cwd && c.agent == crate::session::Agent::Claude)
                .map(|c| c.project_path.clone())
                .unwrap_or_else(|| encode_claude_dir(&s.cwd));
            projects.push((encoded, s.project_name.clone(), s.cwd.clone()));
        }

        projects.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));

        if projects.is_empty() {
            self.set_status("No other projects to move to".to_string());
            return;
        }

        self.move_state = Some(MoveState {
            display_idx,
            projects,
            selected: 0,
        });
        self.mode = Mode::MoveSelectProject;
    }

    /// Start the fork flow for a given session: pick which agent to fork into.
    pub fn start_fork(&mut self, display_idx: usize) {
        if display_idx >= self.display_entries.len() {
            return;
        }
        let entry = &self.display_entries[display_idx];
        let session = self.display_session(entry).clone();
        self.start_fork_session(session);
    }

    /// Start the fork picker for a session already open in the conversation view.
    pub fn start_fork_session(&mut self, session: Session) {
        if session.cwd.is_empty() {
            self.set_status("Cannot fork: session has no working directory".to_string());
            return;
        }
        // Offer the source agent first, then the other two agents.
        use crate::session::Agent;
        let all = [Agent::Claude, Agent::Codex, Agent::Cursor];
        let mut options = vec![session.agent];
        options.extend(all.into_iter().filter(|a| *a != session.agent));
        self.fork_state = Some(ForkState {
            session,
            options,
            selected: 0,
        });
        self.mode = Mode::ForkSelectAgent;
    }

    /// Start the per-session profile picker for the selected Claude or Codex
    /// session. Claude offers its fixed launcher profiles; Codex offers whatever
    /// it declares natively in `config.toml`.
    pub fn start_profile(&mut self, display_idx: usize) {
        self.refresh_session_profiles();
        if display_idx >= self.display_entries.len() {
            return;
        }
        let entry = &self.display_entries[display_idx];
        let session = self.display_session(entry);
        let agent = session.agent;
        let profile_names: Vec<String> = match agent {
            crate::session::Agent::Claude => crate::router::PROFILES
                .iter()
                .map(|p| p.to_string())
                .collect(),
            crate::session::Agent::Codex => {
                let names = crate::router::codex_config_profiles(&crate::codex::get_codex_home());
                if names.is_empty() {
                    self.set_status(
                        "No Codex profiles found (create ~/.codex/<name>.config.toml)".to_string(),
                    );
                    return;
                }
                names
            }
            crate::session::Agent::Cursor => {
                self.set_status("Profiles apply to Claude and Codex sessions only".to_string());
                return;
            }
        };
        let session_id = session.id.clone();
        let current = self.session_profiles.get(&session_id).cloned();
        let mut options: Vec<Option<String>> = profile_names.into_iter().map(Some).collect();
        options.push(None); // "clear" choice
        let selected = current
            .as_deref()
            .and_then(|p| options.iter().position(|o| o.as_deref() == Some(p)))
            .unwrap_or(0);
        self.profile_state = Some(ProfileState {
            session_id,
            agent,
            options,
            selected,
        });
        self.mode = Mode::ProfileSelect;
    }

    /// Persist the picked profile for the session and update the in-memory tag.
    pub fn apply_profile(&mut self) {
        let Some(state) = self.profile_state.take() else {
            return;
        };
        self.mode = Mode::Browsing;
        let choice = state.options.get(state.selected).cloned().flatten();
        let value = choice.as_deref().unwrap_or("");
        // Persist to the home matching the session's agent: Claude tags live
        // under the Claude home, Codex tags under the Codex home.
        let saved = match state.agent {
            crate::session::Agent::Codex => {
                let codex_home = crate::codex::get_codex_home();
                let known = crate::router::codex_config_profiles(&codex_home);
                crate::router::save_codex_session_profile(
                    &codex_home,
                    &state.session_id,
                    value,
                    &known,
                )
            }
            _ => crate::router::save_session_profile(&get_claude_home(), &state.session_id, value),
        };
        match saved {
            Ok(()) => match &choice {
                Some(p) => {
                    self.session_profiles.insert(state.session_id, p.clone());
                    self.set_status(format!("Profile set to {p}"));
                }
                None => {
                    self.session_profiles.remove(&state.session_id);
                    self.set_status("Profile cleared (uses default)".to_string());
                }
            },
            Err(e) => self.set_status(format!("Failed to set profile: {e}")),
        }
    }

    /// Move a session to a different project directory, updating cwd in the JSONL.
    pub fn move_session(
        &mut self,
        display_idx: usize,
        target_encoded_dir: &str,
        target_cwd: &str,
    ) -> Result<String, String> {
        if display_idx >= self.display_entries.len() {
            return Err("invalid index".to_string());
        }
        let entry = &self.display_entries[display_idx];
        let session = self.display_session(entry).clone();

        match session.agent {
            crate::session::Agent::Codex => {
                // Codex rollouts live in date-based dirs, so a "move" just
                // rewrites the recorded cwd in place; grouping is by cwd.
                let source = session
                    .source_path
                    .as_deref()
                    .ok_or("codex session missing source path")?;
                crate::codex::move_codex_session(source, target_cwd)?;
            }
            crate::session::Agent::Claude => {
                let claude_home = get_claude_home();
                let src = claude_home
                    .join("projects")
                    .join(&session.project_path)
                    .join(format!("{}.jsonl", session.id));

                let dst_dir = claude_home.join("projects").join(target_encoded_dir);
                std::fs::create_dir_all(&dst_dir)
                    .map_err(|e| format!("failed to create target dir: {e}"))?;

                let content = std::fs::read_to_string(&src)
                    .map_err(|e| format!("failed to read session file: {e}"))?;
                let old_cwd = &session.cwd;
                let mut updated: String = content
                    .lines()
                    .map(|line| {
                        if let Ok(mut val) = serde_json::from_str::<serde_json::Value>(line) {
                            if let Some(obj) = val.as_object_mut() {
                                if obj.get("cwd").and_then(|v| v.as_str()) == Some(old_cwd) {
                                    obj.insert(
                                        "cwd".to_string(),
                                        serde_json::Value::String(target_cwd.to_string()),
                                    );
                                }
                            }
                            serde_json::to_string(&val).unwrap_or_else(|_| line.to_string())
                        } else {
                            line.to_string()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if !updated.is_empty() && !updated.ends_with('\n') {
                    updated.push('\n');
                }

                let dst = dst_dir.join(format!("{}.jsonl", session.id));
                std::fs::write(&dst, &updated)
                    .map_err(|e| format!("failed to write moved session: {e}"))?;
                std::fs::remove_file(&src)
                    .map_err(|e| format!("failed to remove original session file: {e}"))?;
            }
            crate::session::Agent::Cursor => {
                // Cursor derives cwd from the transcript's project dir, so a
                // "move" relocates the chat's transcript under the target dir.
                let source = session
                    .source_path
                    .as_deref()
                    .ok_or("cursor session missing source path")?;
                crate::cursor::move_cursor_session(source, target_cwd)?;
            }
        }

        let label = session.first_message.chars().take(40).collect::<String>();
        let target_name = std::path::Path::new(target_cwd)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(target_encoded_dir);

        match entry.source {
            DisplaySource::Sessions(sidx) => {
                if sidx < self.sessions.len() && self.sessions[sidx].id == session.id {
                    self.sessions.remove(sidx);
                }
            }
            DisplaySource::Content(cidx) => {
                if cidx < self.content_results.len() && self.content_results[cidx].id == session.id
                {
                    self.content_results.remove(cidx);
                }
            }
        }
        self.apply_filter();

        Ok(format!("{} → {}", label, target_name))
    }

    /// Start editing a title for the given session (rename).
    pub fn start_title_edit(&mut self, session: Session, return_mode: Mode) {
        let existing = session.custom_title.clone().unwrap_or_default();
        let cursor = existing.len();
        self.title_edit = Some(TitleEditState {
            context: TitleEditContext::Rename { session },
            query: existing,
            cursor,
            return_mode,
        });
        self.mode = Mode::TitleEdit;
    }

    /// Start title input for a new session.
    pub fn start_new_session_title(&mut self, cwd: String, profile: String) {
        self.title_edit = Some(TitleEditState {
            context: TitleEditContext::NewSession { cwd, profile },
            query: String::new(),
            cursor: 0,
            return_mode: Mode::Browsing,
        });
        self.mode = Mode::TitleEdit;
    }

    /// Start the agent picker for a new session in `cwd`, letting the user choose
    /// which AI (Claude / Codex / Cursor) to launch.
    pub fn start_new_session(&mut self, cwd: String) {
        use crate::session::Agent;
        self.new_session_state = Some(NewSessionState {
            cwd,
            options: vec![Agent::Claude, Agent::Codex, Agent::Cursor],
            selected: 0,
        });
        self.mode = Mode::NewSelectAgent;
    }

    /// Ask which profile a new Claude or Codex session should use.
    pub fn start_new_session_profile(&mut self, agent: crate::session::Agent, cwd: String) {
        let options: Vec<String> = match agent {
            crate::session::Agent::Claude => crate::router::PROFILES
                .iter()
                .map(|profile| profile.to_string())
                .collect(),
            crate::session::Agent::Codex => {
                crate::router::codex_config_profiles(&crate::codex::get_codex_home())
            }
            crate::session::Agent::Cursor => return,
        };
        if options.is_empty() {
            self.set_status(format!("No {} profiles configured", agent.label()));
            self.mode = Mode::Browsing;
            return;
        }
        let selected = self
            .default_profile_for_agent(agent)
            .and_then(|default| options.iter().position(|profile| profile == default))
            .unwrap_or(0);
        self.new_profile_state = Some(NewProfileState {
            cwd,
            agent,
            options,
            selected,
        });
        self.mode = Mode::NewSelectProfile;
    }

    /// Start title input for forking a session.
    pub fn start_fork_title(&mut self, session_id: String, cwd: String) {
        let existing = self
            .find_session(&session_id)
            .and_then(|s| s.custom_title.clone());
        let prefill = match existing {
            Some(title) => format!("{} (fork)", title),
            None => String::new(),
        };
        let cursor = prefill.len();
        self.title_edit = Some(TitleEditState {
            context: TitleEditContext::Fork { session_id, cwd },
            query: prefill,
            cursor,
            return_mode: Mode::Conversation,
        });
        self.mode = Mode::TitleEdit;
    }

    /// Finish title editing. Returns an Action if the caller should execute it.
    pub fn finish_title_edit(&mut self) -> Result<Option<Action>, String> {
        let state = self.title_edit.take().ok_or("no title edit in progress")?;
        let title = state.query.trim().to_string();

        match state.context {
            TitleEditContext::Rename { session } => {
                let agent = session.agent;

                if !title.is_empty() {
                    let duplicate =
                        self.sessions
                            .iter()
                            .chain(self.content_results.iter())
                            .any(|s| {
                                s.agent == agent
                                    && !same_session_record(s, &session)
                                    && s.custom_title.as_deref() == Some(title.as_str())
                            });
                    if duplicate {
                        let cursor = title.len();
                        self.title_edit = Some(TitleEditState {
                            context: TitleEditContext::Rename { session },
                            query: title,
                            cursor,
                            return_mode: state.return_mode,
                        });
                        self.mode = Mode::TitleEdit;
                        return Err("title already in use".to_string());
                    }
                }

                match agent {
                    crate::session::Agent::Codex => {
                        let path = session
                            .source_path
                            .as_deref()
                            .ok_or("Codex rollout path missing")?;
                        crate::codex::save_codex_title_for_path(
                            &crate::codex::get_codex_home(),
                            std::path::Path::new(path),
                            &title,
                        )?;
                    }
                    crate::session::Agent::Claude => {
                        crate::titles::save_custom_title(
                            &session.project_path,
                            &session.id,
                            &title,
                        )?;
                    }
                    crate::session::Agent::Cursor => {
                        crate::cursor::save_cursor_title(
                            &crate::cursor::get_cursor_home(),
                            &session.id,
                            &title,
                        )?;
                    }
                }
                self.update_session_title(
                    &session,
                    if title.is_empty() { None } else { Some(title) },
                );
                self.mode = state.return_mode;
                Ok(None)
            }
            TitleEditContext::NewSession { cwd, profile } => {
                let escaped_cwd = cwd.replace('\'', "'\\''");
                let launcher = crate::router::claude_launcher(Some(&profile));

                if title.is_empty() {
                    self.mode = Mode::Browsing;
                    return Ok(Some(Action::NewSession {
                        cmd: format!("cd '{}' && {}", escaped_cwd, launcher),
                        title: Some(crate::session::format_title_label(
                            crate::session::Agent::Claude,
                            "session",
                            Some(&profile),
                        )),
                        agent: crate::session::Agent::Claude,
                        profile: Some(profile),
                        cwd,
                    }));
                }

                // A new session is always Claude, so uniqueness is scoped to
                // Claude sessions; the same title on a Codex/Cursor session is
                // intentional (cross-agent clustering) and must not block.
                let duplicate = self.title_conflicts(&title, crate::session::Agent::Claude, None);
                if duplicate {
                    let cursor = title.len();
                    self.title_edit = Some(TitleEditState {
                        context: TitleEditContext::NewSession { cwd, profile },
                        query: title,
                        cursor,
                        return_mode: state.return_mode,
                    });
                    self.mode = Mode::TitleEdit;
                    return Err("title already in use".to_string());
                }

                let escaped_title = title.replace('\'', "'\\''");
                self.mode = Mode::Browsing;
                Ok(Some(Action::NewSession {
                    cmd: format!(
                        "cd '{}' && {} -n '{}'",
                        escaped_cwd, launcher, escaped_title
                    ),
                    title: Some(crate::session::format_title_label(
                        crate::session::Agent::Claude,
                        &title,
                        Some(&profile),
                    )),
                    agent: crate::session::Agent::Claude,
                    profile: Some(profile),
                    cwd,
                }))
            }
            TitleEditContext::Fork { session_id, cwd } => {
                let escaped_cwd = cwd.replace('\'', "'\\''");
                let launcher = crate::router::claude_launcher(
                    self.session_profiles.get(&session_id).map(String::as_str),
                );
                if title.is_empty() {
                    let tab_title = self
                        .find_session(&session_id)
                        .map(|s| self.session_terminal_title(s));
                    self.mode = state.return_mode;
                    return Ok(Some(Action::ForkSession {
                        cmd: format!(
                            "cd '{}' && {} -r {} --fork-session",
                            escaped_cwd, launcher, session_id
                        ),
                        title: tab_title,
                    }));
                }
                let escaped_title = title.replace('\'', "'\\''");
                self.mode = state.return_mode;
                Ok(Some(Action::ForkSession {
                    cmd: format!(
                        "cd '{}' && {} -r {} --fork-session -n '{}'",
                        escaped_cwd, launcher, session_id, escaped_title
                    ),
                    title: Some(crate::session::format_title_label(
                        crate::session::Agent::Claude,
                        &title,
                        self.session_profiles
                            .get(&session_id)
                            .map(String::as_str)
                            .or(self.profile_label.as_deref()),
                    )),
                }))
            }
        }
    }

    /// Cancel title editing.
    pub fn cancel_title_edit(&mut self) {
        if let Some(state) = self.title_edit.take() {
            self.mode = state.return_mode;
        }
    }

    /// Get the display label for a session: custom_title if set, otherwise first_message.
    pub fn session_display_label(&self, session: &Session) -> String {
        session
            .custom_title
            .clone()
            .unwrap_or_else(|| session.first_message.clone())
    }

    /// Resolve the profile that will actually be used for a session. An
    /// explicit per-session tag wins; otherwise the matching shell default is
    /// used for Claude and Codex.
    pub fn effective_session_profile<'a>(&'a self, session: &Session) -> Option<&'a str> {
        self.session_profiles
            .get(&session.id)
            .map(String::as_str)
            .or_else(|| self.default_profile_for_agent(session.agent))
    }

    /// Default profile for a newly created or transferred session.
    pub fn default_profile_for_agent(&self, agent: crate::session::Agent) -> Option<&str> {
        match agent {
            crate::session::Agent::Claude => self.profile_label.as_deref(),
            crate::session::Agent::Codex => self.codex_profile_label.as_deref(),
            crate::session::Agent::Cursor => None,
        }
    }

    /// Terminal title using the same provider, title, profile order as the list.
    pub fn session_terminal_title(&self, session: &Session) -> String {
        session.title_label_with_profile(self.effective_session_profile(session))
    }

    /// Whether `title` clashes with another session of the same `agent`. Title
    /// uniqueness is scoped per provider on purpose: the same title across
    /// different agents is intentional - it clusters a session with its
    /// cross-agent clones - so only a same-agent match conflicts. `exclude_id`
    /// skips the session being renamed.
    fn title_conflicts(
        &self,
        title: &str,
        agent: crate::session::Agent,
        exclude_id: Option<&str>,
    ) -> bool {
        self.sessions
            .iter()
            .chain(self.content_results.iter())
            .any(|s| {
                s.agent == agent
                    && Some(s.id.as_str()) != exclude_id
                    && s.custom_title.as_deref() == Some(title)
            })
    }

    /// Find a session by ID across sessions and content_results.
    fn find_session(&self, session_id: &str) -> Option<&Session> {
        self.sessions
            .iter()
            .chain(self.content_results.iter())
            .find(|s| s.id == session_id)
    }

    /// Update custom_title on a session in memory.
    fn update_session_title(&mut self, target: &Session, title: Option<String>) {
        let selected_record = if self.grouped_view {
            self.selected_tree_row().and_then(|row| match row {
                TreeRow::Session { display_idx, .. } => Some(
                    self.display_session(&self.display_entries[*display_idx])
                        .clone(),
                ),
                TreeRow::Project(_) => None,
            })
        } else {
            None
        };
        for s in self
            .sessions
            .iter_mut()
            .chain(self.content_results.iter_mut())
        {
            if same_session_record(s, target) {
                s.custom_title = title.clone();
            }
        }
        if let Some(conversation) = &mut self.conversation {
            if same_session_record(&conversation.session, target) {
                conversation.session.custom_title = title;
            }
        }
        self.rebuild_tree_rows();
        if let Some(selected_record) = selected_record {
            if let Some((index, _)) = self.tree_rows.iter().enumerate().find(|(_, row)| {
                matches!(row, TreeRow::Session { display_idx, .. }
                if same_session_record(
                    self.display_session(&self.display_entries[*display_idx]),
                    &selected_record
                ))
            }) {
                self.selected = index;
            }
        }
    }

    /// Set a status message that disappears after a few seconds.
    #[allow(dead_code)]
    pub fn set_status(&mut self, msg: String) {
        self.status_message = Some((msg, Instant::now()));
    }

    /// Spinner character for the current tick.
    pub fn spinner_char(&self) -> char {
        const FRAMES: &[char] = &[
            '\u{280B}', '\u{2819}', '\u{2839}', '\u{2838}', '\u{283C}', '\u{2834}', '\u{2826}',
            '\u{2827}', '\u{2807}', '\u{280F}',
        ];
        FRAMES[self.spinner_tick % FRAMES.len()]
    }

    /// Check if content search results have arrived.
    pub fn poll_content_search(&mut self) -> bool {
        if let Some(rx) = &self.search_receiver {
            match rx.try_recv() {
                Ok(results) => {
                    self.search_receiver = None;
                    let selected_id = self
                        .display_entries
                        .get(self.selected)
                        .map(|e| self.display_session(e).id.clone());

                    self.content_results = results;
                    self.content_search_state = ContentSearchState::Complete;
                    self.rebuild_display_entries();

                    if let Some(id) = selected_id {
                        if let Some(pos) = self
                            .display_entries
                            .iter()
                            .position(|e| self.display_session(e).id == id)
                        {
                            self.selected = pos;
                        }
                    }
                    true
                }
                Err(mpsc::TryRecvError::Empty) => {
                    self.spinner_tick = self.spinner_tick.wrapping_add(1);
                    false
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.search_receiver = None;
                    self.content_search_state = ContentSearchState::Complete;
                    true
                }
            }
        } else {
            false
        }
    }

    /// Check if debounce period has elapsed and start content search if so.
    pub fn check_debounce(&mut self) {
        if self.content_search_state != ContentSearchState::Debouncing {
            return;
        }
        if let Some(last) = self.last_keystroke {
            if last.elapsed() >= Duration::from_millis(300) && !self.filter_query.is_empty() {
                self.content_search_state = ContentSearchState::Searching;
                self.spinner_tick = 0;

                let cancel = Arc::new(AtomicBool::new(false));
                self.cancel_flag = Arc::clone(&cancel);

                let (tx, rx) = mpsc::channel();
                self.search_receiver = Some(rx);

                let claude_home = get_claude_home();
                let index = Arc::clone(&self.session_index);
                let pattern = self.filter_query.clone();
                std::thread::spawn(move || {
                    let results =
                        search::deep_search_indexed(&claude_home, &pattern, &index, &cancel);
                    let _ = tx.send(results);
                });
            }
        }
    }

    /// Reload profile tags and defaults changed by another terminal.
    pub fn refresh_session_profiles(&mut self) {
        self.refresh_session_profiles_from(&get_claude_home(), &crate::codex::get_codex_home());
        self.profile_label = crate::router::profile_name();
        self.codex_profile_label = crate::router::codex_profile_name();
    }

    fn refresh_session_profiles_from(&mut self, claude_home: &Path, codex_home: &Path) {
        let mut profiles = crate::router::load_session_profiles(claude_home);
        profiles.extend(crate::router::load_session_profiles(codex_home));
        self.session_profiles = profiles;
        self.last_profile_refresh = Instant::now();
    }

    /// Refresh external profile changes and clear expired status messages.
    pub fn tick_status(&mut self) {
        if self.last_profile_refresh.elapsed() >= Duration::from_millis(500) {
            self.refresh_session_profiles();
        }
        if let Some((_, when)) = &self.status_message {
            if when.elapsed() > Duration::from_secs(3) {
                self.status_message = None;
            }
        }
    }
}

/// Run the interactive TUI session picker.
pub fn run(
    sessions: Vec<Session>,
    theme: Theme,
    grouped_view: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if sessions.is_empty() {
        eprintln!("No sessions found.");
        return Ok(());
    }

    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), LeaveAlternateScreen);
        original_hook(panic_info);
    }));

    enable_raw_mode()?;
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let claude_home = get_claude_home();
    let session_index = search::build_session_index(&claude_home, &sessions);
    let mut app = App::new(sessions, session_index, theme, grouped_view);
    let mut deferred_command: Option<(String, Option<String>)> = None;
    let mut launch_warning: Option<String> = None;
    let mut new_session_launch: Option<NewSessionLaunch> = None;

    loop {
        app.tick_status();

        if app.content_search_state == ContentSearchState::Searching {
            app.poll_content_search();
        }

        if app.mode == Mode::Browsing {
            app.check_debounce();
        }

        terminal.draw(|frame| {
            let height = frame.area().height.saturating_sub(2) as usize;
            app.ensure_visible(height);
            view::render(frame, &mut app);
        })?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                match handle_input(&mut app, key) {
                    Action::Quit => break,
                    Action::EnterConversation(idx) => {
                        app.enter_conversation(idx);
                    }
                    Action::NewSession {
                        cmd,
                        title,
                        agent,
                        profile,
                        cwd,
                    } => {
                        if let Some(profile) = profile {
                            let home = match agent {
                                crate::session::Agent::Claude => get_claude_home(),
                                crate::session::Agent::Codex => crate::codex::get_codex_home(),
                                crate::session::Agent::Cursor => crate::cursor::get_cursor_home(),
                            };
                            new_session_launch =
                                Some(NewSessionLaunch::new(agent, profile, cwd, home));
                        }
                        deferred_command = Some((cmd, title));
                        break;
                    }
                    Action::CopyCommand { cmd, title } | Action::ForkSession { cmd, title } => {
                        deferred_command = Some((cmd, title));
                        break;
                    }
                    Action::CloneSession {
                        cmd,
                        title,
                        warning,
                    } => {
                        launch_warning = warning;
                        deferred_command = Some((cmd, title));
                        break;
                    }
                    Action::ArchiveSession(display_idx) => match app.archive_session(display_idx) {
                        Ok(label) => {
                            app.set_status(format!("Archived: {}", label));
                            if app.selected >= app.visible_row_count() && app.selected > 0 {
                                app.selected -= 1;
                            }
                        }
                        Err(e) => {
                            app.set_status(format!("Archive failed: {}", e));
                        }
                    },
                    Action::MoveSession {
                        display_idx,
                        target_project,
                        target_cwd,
                    } => match app.move_session(display_idx, &target_project, &target_cwd) {
                        Ok(label) => {
                            app.set_status(format!("Moved: {}", label));
                            if app.selected >= app.visible_row_count() && app.selected > 0 {
                                app.selected -= 1;
                            }
                        }
                        Err(e) => {
                            app.set_status(format!("Move failed: {}", e));
                        }
                    },
                    Action::BackToList => {
                        app.leave_conversation();
                    }
                    Action::Continue => {}
                }
            }
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    if let Some((cmd, title)) = deferred_command {
        use std::process::Command;
        if let Some(warning) = launch_warning {
            eprintln!("{warning}");
        }
        // Stamp before starting the shell and again after its preexec hooks.
        // Codex launches disable its own replacement; Cursor seed prompts also
        // lead with this name.
        let launch_cmd = if let Some(title) = title.as_deref().filter(|t| !t.is_empty()) {
            let _ = execute!(std::io::stdout(), crossterm::terminal::SetTitle(title));
            let escaped_title = title.replace('\'', "'\\''");
            format!("printf '\\033]0;%s\\007' '{}' && {}", escaped_title, cmd)
        } else {
            cmd
        };
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let status = Command::new(&shell)
            .arg("-ic")
            .arg(&launch_cmd)
            .spawn()
            .and_then(|mut child| wait_for_agent(&mut child, new_session_launch.as_ref()));
        match status {
            Ok(s) => {
                std::process::exit(s.code().unwrap_or(1));
            }
            Err(e) => {
                eprintln!("Failed to exec: {e}");
                println!("{launch_cmd}");
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod title_conflict_tests {
    use super::*;
    use crate::session::{Agent, Session};
    use crate::theme::Theme;
    use chrono::Utc;

    fn session(id: &str, agent: Agent, title: Option<&str>) -> Session {
        Session {
            id: id.into(),
            project_path: "/tmp/proj".into(),
            project_name: "proj".into(),
            git_branch: None,
            timestamp: Utc::now(),
            first_message: "msg".into(),
            cwd: "/tmp/proj".into(),
            project_exists: false,
            custom_title: title.map(str::to_string),
            agent,
            source_path: None,
        }
    }

    fn app_with(sessions: Vec<Session>) -> App {
        App::new(
            sessions,
            std::collections::HashMap::new(),
            Theme::dark(),
            true,
        )
    }

    #[test]
    fn profile_refresh_updates_labels_and_resume_commands_from_both_homes() {
        let claude_home = tempfile::tempdir().unwrap();
        let codex_home = tempfile::tempdir().unwrap();
        let claude = session("claude-session", Agent::Claude, Some("Claude title"));
        let codex = session("codex-session", Agent::Codex, Some("Codex title"));
        let mut app = app_with(vec![claude.clone(), codex.clone()]);
        app.profile_label = Some("work".into());
        app.codex_profile_label = Some("work".into());
        app.session_profiles
            .insert(claude.id.clone(), "work".into());
        app.session_profiles.insert(codex.id.clone(), "work".into());
        crate::router::save_session_profile(claude_home.path(), &claude.id, "personal").unwrap();
        let known = vec!["work".into(), "personal".into()];
        crate::router::save_codex_session_profile(codex_home.path(), &codex.id, "personal", &known)
            .unwrap();

        app.refresh_session_profiles_from(claude_home.path(), codex_home.path());

        for session in [&claude, &codex] {
            assert_eq!(app.effective_session_profile(session), Some("personal"));
            assert!(app.session_terminal_title(session).contains("personal"));
            let profile = app.session_profiles.get(&session.id).map(String::as_str);
            let command = session
                .resume_command_with_profiles(&crate::router::claude_launcher(profile), profile);
            assert!(command.contains(if session.agent == Agent::Codex {
                "codex --profile personal"
            } else {
                "_claude_run_personal"
            }));
        }

        crate::router::save_session_profile(claude_home.path(), &claude.id, "").unwrap();
        crate::router::save_codex_session_profile(codex_home.path(), &codex.id, "work", &known)
            .unwrap();
        app.refresh_session_profiles_from(claude_home.path(), codex_home.path());

        assert!(!app.session_profiles.contains_key(&claude.id));
        assert_eq!(app.effective_session_profile(&claude), Some("work"));
        assert_eq!(app.effective_session_profile(&codex), Some("work"));
    }

    #[test]
    fn same_agent_same_title_conflicts() {
        let app = app_with(vec![
            session("a", Agent::Claude, Some("dev")),
            session("b", Agent::Claude, Some("other")),
        ]);
        // Renaming session "b" to "dev" clashes with the Claude session "a".
        assert!(app.title_conflicts("dev", Agent::Claude, Some("b")));
    }

    #[test]
    fn cross_agent_same_title_is_allowed() {
        let app = app_with(vec![
            session("a", Agent::Codex, Some("dev")),
            session("b", Agent::Claude, Some("other")),
        ]);
        // Renaming the Claude session "b" to "dev" must NOT clash with the Codex
        // session that already carries "dev" - cross-agent titles cluster on
        // purpose.
        assert!(!app.title_conflicts("dev", Agent::Claude, Some("b")));
    }

    #[test]
    fn excludes_self() {
        let app = app_with(vec![session("a", Agent::Claude, Some("dev"))]);
        // A session never conflicts with its own current title.
        assert!(!app.title_conflicts("dev", Agent::Claude, Some("a")));
    }

    #[test]
    fn new_claude_title_conflicts_only_with_claude() {
        let app = app_with(vec![
            session("a", Agent::Cursor, Some("dev")),
            session("b", Agent::Claude, Some("taken")),
        ]);
        // New sessions are Claude: "dev" is free (only a Cursor holds it), but
        // "taken" collides with an existing Claude session.
        assert!(!app.title_conflicts("dev", Agent::Claude, None));
        assert!(app.title_conflicts("taken", Agent::Claude, None));
    }

    #[test]
    fn renaming_fork_keeps_original_title_and_selection() {
        let mut app = app_with(vec![
            session("original", Agent::Codex, Some("Agent Session")),
            session("fork", Agent::Codex, Some("Agent Session (fork)")),
        ]);
        app.project_groups[0].expanded = true;
        app.rebuild_tree_rows();
        app.selected = app
            .tree_rows
            .iter()
            .position(|row| {
                matches!(row, TreeRow::Session { display_idx, .. }
                    if app.display_session(&app.display_entries[*display_idx]).id == "fork")
            })
            .unwrap();
        let fork_display_idx = match app.selected_tree_row() {
            Some(TreeRow::Session { display_idx, .. }) => *display_idx,
            _ => unreachable!(),
        };
        app.enter_conversation(fork_display_idx);

        let fork = app.find_session("fork").unwrap().clone();
        app.update_session_title(&fork, Some("Renamed fork".to_string()));

        assert_eq!(
            app.find_session("original")
                .unwrap()
                .custom_title
                .as_deref(),
            Some("Agent Session")
        );
        assert_eq!(
            app.find_session("fork").unwrap().custom_title.as_deref(),
            Some("Renamed fork")
        );
        assert_eq!(
            app.conversation
                .as_ref()
                .unwrap()
                .session
                .custom_title
                .as_deref(),
            Some("Renamed fork")
        );
        assert!(
            matches!(app.selected_tree_row(), Some(TreeRow::Session { display_idx, .. })
            if app.display_session(&app.display_entries[*display_idx]).id == "fork")
        );
    }

    #[test]
    fn duplicate_codex_ids_in_different_projects_rename_only_selected_record() {
        let mut first = session("shared-id", Agent::Codex, Some("Same title"));
        first.project_path = "/tmp/first".into();
        first.source_path = Some("/tmp/codex/sessions/2026/01/01/first.jsonl".into());
        let mut second = session("shared-id", Agent::Codex, Some("Same title"));
        second.project_path = "/tmp/second".into();
        second.source_path = Some("/tmp/codex/sessions/2026/01/02/second.jsonl".into());
        let mut app = app_with(vec![first, second.clone()]);

        app.start_title_edit(second.clone(), Mode::Browsing);
        assert_eq!(app.title_edit.as_ref().unwrap().query, "Same title");
        app.update_session_title(&second, Some("Only second".into()));

        assert_eq!(app.sessions[0].custom_title.as_deref(), Some("Same title"));
        assert_eq!(app.sessions[1].custom_title.as_deref(), Some("Only second"));
    }
}
