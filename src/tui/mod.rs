pub mod input;
pub mod syntax;
pub mod table;
pub mod view;

use std::collections::{HashMap, HashSet};
use std::io::stdout;
use std::path::PathBuf;
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

/// TUI interaction mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Browsing,
    Conversation,
    ConversationSearch,
    ConfirmArchive,
    MoveSelectProject,
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
    CopyCommand(String),
    NewSession(String),
    ForkSession(String),
    BackToList,
    ArchiveSession(usize),
    MoveSession { display_idx: usize, target_project: String, target_cwd: String },
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
    Session { project_idx: usize, display_idx: usize },
}

/// Build project groups from a flat list of sessions.
/// Groups are sorted by the latest session timestamp (newest group first).
/// Sessions within each group are ordered by their position in `display_entries`.
pub fn group_by_project(sessions: &[Session], display_entries: &[DisplayEntry]) -> Vec<ProjectGroup> {
    group_by_project_with_content(sessions, display_entries, &[])
}

/// Build project groups, including content-only search results.
pub fn group_by_project_with_content(sessions: &[Session], display_entries: &[DisplayEntry], content_results: &[Session]) -> Vec<ProjectGroup> {
    let mut group_map: std::collections::HashMap<String, Vec<usize>> = std::collections::HashMap::new();

    for (di, entry) in display_entries.iter().enumerate() {
        let session = match &entry.source {
            DisplaySource::Sessions(idx) => &sessions[*idx],
            DisplaySource::Content(idx) => &content_results[*idx],
        };
        group_map
            .entry(session.project_path.clone())
            .or_default()
            .push(di);
    }

    let mut groups: Vec<ProjectGroup> = group_map
        .into_iter()
        .map(|(path, indices)| {
            let first_session = indices.first()
                .and_then(|&di| {
                    match &display_entries.get(di)?.source {
                        DisplaySource::Sessions(idx) => sessions.get(*idx),
                        DisplaySource::Content(idx) => content_results.get(*idx),
                    }
                });
            let name = first_session
                .map(|s| s.project_name.clone())
                .unwrap_or_else(|| "unknown".to_string());
            let cwd = first_session
                .map(|s| s.cwd.clone())
                .unwrap_or_default();
            ProjectGroup {
                name,
                path,
                cwd,
                session_indices: indices,
                expanded: false,
            }
        })
        .collect();

    // Sort groups by latest activity (newest first)
    groups.sort_by(|a, b| {
        let ts_a = a.session_indices.first()
            .map(|&i| display_entries[i].timestamp)
            .unwrap_or_else(|| DateTime::<Utc>::MIN_UTC);
        let ts_b = b.session_indices.first()
            .map(|&i| display_entries[i].timestamp)
            .unwrap_or_else(|| DateTime::<Utc>::MIN_UTC);
        ts_b.cmp(&ts_a)
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
                rows.push(TreeRow::Session { project_idx: gi, display_idx: di });
            }
        }
    }
    rows
}

/// State for the project picker when moving a session.
pub struct MoveState {
    pub display_idx: usize,
    pub projects: Vec<(String, String, String)>,  // (encoded_path, display_name, cwd)
    pub selected: usize,
}

/// What the title edit is for.
pub enum TitleEditContext {
    Rename { session_id: String },
    NewSession { cwd: String },
    Fork { session_id: String, cwd: String },
}

/// State for title editing.
pub struct TitleEditState {
    pub context: TitleEditContext,
    pub query: String,
    pub cursor: usize,
    pub return_mode: Mode,
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
    /// Title being edited.
    pub title_edit: Option<TitleEditState>,
}

impl App {
    pub fn new(sessions: Vec<Session>, session_index: HashMap<PathBuf, Session>, theme: Theme, grouped_view: bool) -> Self {
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
            title_edit: None,
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
        let content_ids: HashSet<&str> = self
            .content_results
            .iter()
            .map(|s| s.id.as_str())
            .collect();
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
        let old_expanded: HashMap<String, bool> = self.project_groups
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
        let claude_home = get_claude_home();
        let messages = load_conversation(&claude_home, &session);

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
        std::fs::rename(&src, &dst)
            .map_err(|e| format!("failed to move session: {e}"))?;

        let label = session.first_message.chars().take(40).collect::<String>();

        match entry.source {
            DisplaySource::Sessions(sidx) => {
                if sidx < self.sessions.len() && self.sessions[sidx].id == session.id {
                    self.sessions.remove(sidx);
                }
            }
            DisplaySource::Content(cidx) => {
                if cidx < self.content_results.len() && self.content_results[cidx].id == session.id {
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
        let current_path = self.display_session(entry).project_path.clone();

        let mut seen = HashSet::new();
        let mut projects: Vec<(String, String, String)> = Vec::new();
        for s in &self.sessions {
            if s.project_path == current_path {
                continue;
            }
            if seen.insert(s.project_path.clone()) {
                projects.push((s.project_path.clone(), s.project_name.clone(), s.cwd.clone()));
            }
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

    /// Move a session to a different project directory, updating cwd in the JSONL.
    pub fn move_session(&mut self, display_idx: usize, target_encoded_dir: &str, target_cwd: &str) -> Result<String, String> {
        if display_idx >= self.display_entries.len() {
            return Err("invalid index".to_string());
        }
        let entry = &self.display_entries[display_idx];
        let session = self.display_session(entry).clone();

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
                            obj.insert("cwd".to_string(), serde_json::Value::String(target_cwd.to_string()));
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
                if cidx < self.content_results.len() && self.content_results[cidx].id == session.id {
                    self.content_results.remove(cidx);
                }
            }
        }
        self.apply_filter();

        Ok(format!("{} → {}", label, target_name))
    }

    /// Start editing a title for the given session (rename).
    pub fn start_title_edit(&mut self, session_id: String, return_mode: Mode) {
        let existing = self.find_session(&session_id)
            .and_then(|s| s.custom_title.clone())
            .unwrap_or_default();
        let cursor = existing.len();
        self.title_edit = Some(TitleEditState {
            context: TitleEditContext::Rename { session_id },
            query: existing,
            cursor,
            return_mode,
        });
        self.mode = Mode::TitleEdit;
    }

    /// Start title input for a new session.
    pub fn start_new_session_title(&mut self, cwd: String) {
        self.title_edit = Some(TitleEditState {
            context: TitleEditContext::NewSession { cwd },
            query: String::new(),
            cursor: 0,
            return_mode: Mode::Browsing,
        });
        self.mode = Mode::TitleEdit;
    }

    /// Start title input for forking a session.
    pub fn start_fork_title(&mut self, session_id: String, cwd: String) {
        let existing = self.find_session(&session_id)
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
            TitleEditContext::Rename { session_id } => {
                if !title.is_empty() {
                    let duplicate = self.sessions.iter()
                        .chain(self.content_results.iter())
                        .any(|s| s.id != session_id && s.custom_title.as_deref() == Some(&title));
                    if duplicate {
                        let cursor = title.len();
                        self.title_edit = Some(TitleEditState {
                            context: TitleEditContext::Rename { session_id },
                            query: title,
                            cursor,
                            return_mode: state.return_mode,
                        });
                        self.mode = Mode::TitleEdit;
                        return Err("title already in use".to_string());
                    }
                }

                let project_path = self.find_session(&session_id)
                    .map(|s| s.project_path.clone())
                    .ok_or("session not found")?;

                crate::titles::save_custom_title(&project_path, &session_id, &title)?;
                self.update_session_title(&session_id, if title.is_empty() { None } else { Some(title) });
                self.mode = state.return_mode;
                Ok(None)
            }
            TitleEditContext::NewSession { cwd } => {
                let escaped_cwd = cwd.replace('\'', "'\\''");

                if title.is_empty() {
                    self.mode = Mode::Browsing;
                    return Ok(Some(Action::NewSession(format!("cd '{}' && claude", escaped_cwd))));
                }

                let duplicate = self.sessions.iter()
                    .chain(self.content_results.iter())
                    .any(|s| s.custom_title.as_deref() == Some(&title));
                if duplicate {
                    let cursor = title.len();
                    self.title_edit = Some(TitleEditState {
                        context: TitleEditContext::NewSession { cwd },
                        query: title,
                        cursor,
                        return_mode: state.return_mode,
                    });
                    self.mode = Mode::TitleEdit;
                    return Err("title already in use".to_string());
                }

                let escaped_title = title.replace('\'', "'\\''");
                self.mode = Mode::Browsing;
                Ok(Some(Action::NewSession(format!("cd '{}' && claude -n '{}'", escaped_cwd, escaped_title))))
            }
            TitleEditContext::Fork { session_id, cwd } => {
                let escaped_cwd = cwd.replace('\'', "'\\''");
                if title.is_empty() {
                    self.mode = state.return_mode;
                    return Ok(Some(Action::ForkSession(
                        format!("cd '{}' && claude -r {} --fork-session", escaped_cwd, session_id)
                    )));
                }
                let escaped_title = title.replace('\'', "'\\''");
                self.mode = state.return_mode;
                Ok(Some(Action::ForkSession(
                    format!("cd '{}' && claude -r {} --fork-session -n '{}'", escaped_cwd, session_id, escaped_title)
                )))
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
        session.custom_title.clone().unwrap_or_else(|| session.first_message.clone())
    }

    /// Find a session by ID across sessions and content_results.
    fn find_session(&self, session_id: &str) -> Option<&Session> {
        self.sessions.iter()
            .chain(self.content_results.iter())
            .find(|s| s.id == session_id)
    }

    /// Update custom_title on a session in memory.
    fn update_session_title(&mut self, session_id: &str, title: Option<String>) {
        for s in self.sessions.iter_mut().chain(self.content_results.iter_mut()) {
            if s.id == session_id {
                s.custom_title = title;
                return;
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

    /// Clear expired status messages.
    pub fn tick_status(&mut self) {
        if let Some((_, when)) = &self.status_message {
            if when.elapsed() > Duration::from_secs(3) {
                self.status_message = None;
            }
        }
    }
}

/// Run the interactive TUI session picker.
pub fn run(sessions: Vec<Session>, theme: Theme, grouped_view: bool) -> Result<(), Box<dyn std::error::Error>> {
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
    let mut deferred_command: Option<String> = None;

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
                    Action::CopyCommand(cmd) | Action::NewSession(cmd) | Action::ForkSession(cmd) => {
                        deferred_command = Some(cmd);
                        break;
                    }
                    Action::ArchiveSession(display_idx) => {
                        match app.archive_session(display_idx) {
                            Ok(label) => {
                                app.set_status(format!("Archived: {}", label));
                                if app.selected >= app.visible_row_count() && app.selected > 0 {
                                    app.selected -= 1;
                                }
                            }
                            Err(e) => {
                                app.set_status(format!("Archive failed: {}", e));
                            }
                        }
                    }
                    Action::MoveSession { display_idx, target_project, target_cwd } => {
                        match app.move_session(display_idx, &target_project, &target_cwd) {
                            Ok(label) => {
                                app.set_status(format!("Moved: {}", label));
                                if app.selected >= app.visible_row_count() && app.selected > 0 {
                                    app.selected -= 1;
                                }
                            }
                            Err(e) => {
                                app.set_status(format!("Move failed: {}", e));
                            }
                        }
                    }
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

    if let Some(cmd) = deferred_command {
        use std::process::Command;
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let status = Command::new(&shell)
            .arg("-ic")
            .arg(&cmd)
            .status();
        match status {
            Ok(s) => std::process::exit(s.code().unwrap_or(1)),
            Err(e) => {
                eprintln!("Failed to exec: {e}");
                println!("{cmd}");
            }
        }
    }

    Ok(())
}
