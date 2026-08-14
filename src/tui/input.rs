use std::sync::atomic::Ordering;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{Action, App, ContentSearchState, Mode, TreeRow};

/// Resolve the display index of the currently selected session, if any.
///
/// In grouped view this is only defined when a session row (not a project
/// header) is selected; in flat view it is the current selection.
fn current_selection_idx(app: &App) -> Option<usize> {
    if app.grouped_view {
        match app.selected_tree_row().cloned() {
            Some(TreeRow::Session { display_idx, .. }) => Some(display_idx),
            _ => None,
        }
    } else if app.selected < app.display_entries.len() {
        Some(app.selected)
    } else {
        None
    }
}

/// Resolve the working directory for the current selection.
///
/// Unlike [`current_selection_idx`], a project header also resolves (to that
/// project's cwd) so a new session can be started from a group header.
fn current_selection_cwd(app: &App) -> Option<String> {
    if app.grouped_view {
        match app.selected_tree_row().cloned() {
            Some(TreeRow::Project(gi)) => Some(app.project_groups[gi].cwd.clone()),
            Some(TreeRow::Session { project_idx, .. }) => {
                Some(app.project_groups[project_idx].cwd.clone())
            }
            None => None,
        }
    } else if app.selected < app.display_entries.len() {
        let entry = &app.display_entries[app.selected];
        Some(app.display_session(entry).cwd.clone())
    } else {
        None
    }
}

/// Handle a key event and return the resulting action.
pub fn handle_input(app: &mut App, key: KeyEvent) -> Action {
    // Ctrl-C always quits
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return Action::Quit;
    }

    match app.mode {
        Mode::Browsing => handle_browse(app, key),
        Mode::Conversation => handle_conversation(app, key),
        Mode::ConversationSearch => handle_conversation_search(app, key),
        Mode::ConfirmArchive => handle_confirm_archive(app, key),
        Mode::MoveSelectProject => handle_move_select(app, key),
        Mode::TitleEdit => handle_title_edit(app, key),
    }
}

fn handle_browse(app: &mut App, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Tab | KeyCode::BackTab => {
            app.toggle_view();
            Action::Continue
        }
        KeyCode::Esc => {
            if !app.filter_query.is_empty() || app.filter_active {
                app.cancel_content_search();
                app.filter_query.clear();
                app.filter_active = false;
                app.apply_filter();
                Action::Continue
            } else {
                Action::Quit
            }
        }
        KeyCode::Enter => {
            if app.grouped_view {
                match app.selected_tree_row().cloned() {
                    Some(TreeRow::Project(gi)) => {
                        app.toggle_project(gi);
                        Action::Continue
                    }
                    Some(TreeRow::Session { display_idx, .. }) => {
                        Action::EnterConversation(display_idx)
                    }
                    None => Action::Continue,
                }
            } else if app.selected < app.display_entries.len() {
                Action::EnterConversation(app.selected)
            } else {
                Action::Continue
            }
        }
        KeyCode::Right => {
            if app.grouped_view {
                if let Some(TreeRow::Project(gi)) = app.selected_tree_row().cloned() {
                    if !app.project_groups[gi].expanded {
                        app.toggle_project(gi);
                    }
                }
            }
            Action::Continue
        }
        KeyCode::Left => {
            if app.grouped_view {
                match app.selected_tree_row().cloned() {
                    Some(TreeRow::Project(gi)) => {
                        if app.project_groups[gi].expanded {
                            app.toggle_project(gi);
                        }
                    }
                    Some(TreeRow::Session { project_idx, .. }) => {
                        // Jump to parent project header
                        if let Some(pos) = app.tree_rows.iter().position(|r| *r == TreeRow::Project(project_idx)) {
                            app.selected = pos;
                        }
                    }
                    None => {}
                }
            }
            Action::Continue
        }
        KeyCode::Down => {
            app.move_down();
            Action::Continue
        }
        KeyCode::Up => {
            app.move_up();
            Action::Continue
        }
        KeyCode::PageDown => {
            for _ in 0..20 {
                app.move_down();
            }
            Action::Continue
        }
        KeyCode::PageUp => {
            for _ in 0..20 {
                app.move_up();
            }
            Action::Continue
        }
        KeyCode::Home => {
            app.selected = 0;
            app.scroll_offset = 0;
            Action::Continue
        }
        KeyCode::End => {
            let count = app.visible_row_count();
            if count > 0 {
                app.selected = count - 1;
            }
            Action::Continue
        }
        KeyCode::Backspace => {
            if !app.filter_query.is_empty() {
                app.filter_query.pop();
                app.cancel_flag.store(true, Ordering::Relaxed);
                app.search_receiver = None;
                app.content_results.clear();
                if app.filter_query.is_empty() {
                    app.content_search_state = ContentSearchState::Idle;
                    app.last_keystroke = None;
                } else {
                    app.content_search_state = ContentSearchState::Debouncing;
                    app.last_keystroke = Some(Instant::now());
                }
                app.apply_filter();
            }
            Action::Continue
        }
        KeyCode::Char(c) => {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

            // `/` enters filter mode explicitly (kept for discoverability).
            if !ctrl && c == '/' && app.filter_query.is_empty() && !app.filter_active {
                app.filter_active = true;
                return Action::Continue;
            }

            // Action shortcuts use Ctrl so that bare typing always feeds the
            // search filter (cc-session's "just start typing" model). Ctrl
            // shortcuts work whether or not the filter is currently active.
            // Move is Ctrl-v because Ctrl-m is indistinguishable from Enter.
            if ctrl && c == 'a' {
                if let Some(idx) = current_selection_idx(app) {
                    app.archive_confirm = Some(idx);
                    app.mode = Mode::ConfirmArchive;
                }
                return Action::Continue;
            }
            if ctrl && c == 'n' {
                if let Some(cwd) = current_selection_cwd(app).filter(|c| !c.is_empty()) {
                    app.start_new_session_title(cwd);
                }
                return Action::Continue;
            }
            if ctrl && c == 't' {
                if let Some(idx) = current_selection_idx(app) {
                    let entry = &app.display_entries[idx];
                    let session_id = app.display_session(entry).id.clone();
                    app.start_title_edit(session_id, Mode::Browsing);
                }
                return Action::Continue;
            }
            if ctrl && c == 'v' {
                if let Some(idx) = current_selection_idx(app) {
                    app.start_move(idx);
                }
                return Action::Continue;
            }

            // Ignore any other Ctrl-modified key so it never pollutes the filter.
            if ctrl {
                return Action::Continue;
            }

            app.filter_active = true;
            app.filter_query.push(c);
            app.cancel_flag.store(true, Ordering::Relaxed);
            app.search_receiver = None;
            app.content_results.clear();
            app.content_search_state = ContentSearchState::Debouncing;
            app.last_keystroke = Some(Instant::now());
            app.apply_filter();
            Action::Continue
        }
        _ => Action::Continue,
    }
}

fn handle_conversation(app: &mut App, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc => {
            // First Esc: clear highlights if any are active
            if let Some(conv) = &mut app.conversation {
                let has_highlights = !conv.initial_search_terms.is_empty()
                    || conv.search_confirmed;
                if has_highlights {
                    conv.initial_search_terms.clear();
                    conv.search_confirmed = false;
                    conv.search_query.clear();
                    conv.match_positions.clear();
                    conv.rendered_width = 0; // force re-render without highlights
                    return Action::Continue;
                }
            }
            // Second Esc (no highlights): go back to list
            Action::BackToList
        }
        KeyCode::Char('q') => Action::BackToList,
        KeyCode::Char(' ') => {
            if let Some(conv) = &mut app.conversation {
                let max = conv.lines.len().saturating_sub(conv.page_height);
                conv.scroll_offset = (conv.scroll_offset + conv.page_height).min(max);
            }
            Action::Continue
        }
        KeyCode::Char('b') => {
            if let Some(conv) = &mut app.conversation {
                conv.scroll_offset = conv.scroll_offset.saturating_sub(conv.page_height);
            }
            Action::Continue
        }
        KeyCode::Char('g') => {
            if let Some(conv) = &mut app.conversation {
                conv.scroll_offset = 0;
            }
            Action::Continue
        }
        KeyCode::Char('G') => {
            if let Some(conv) = &mut app.conversation {
                let max = conv.lines.len().saturating_sub(conv.page_height);
                conv.scroll_offset = max;
            }
            Action::Continue
        }
        KeyCode::PageDown => {
            if let Some(conv) = &mut app.conversation {
                let max = conv.lines.len().saturating_sub(conv.page_height);
                conv.scroll_offset = (conv.scroll_offset + conv.page_height).min(max);
            }
            Action::Continue
        }
        KeyCode::PageUp => {
            if let Some(conv) = &mut app.conversation {
                conv.scroll_offset = conv.scroll_offset.saturating_sub(conv.page_height);
            }
            Action::Continue
        }
        KeyCode::Char('j') | KeyCode::Down => {
            if let Some(conv) = &mut app.conversation {
                let max = conv.lines.len().saturating_sub(conv.page_height);
                conv.scroll_offset = (conv.scroll_offset + 1).min(max);
            }
            Action::Continue
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if let Some(conv) = &mut app.conversation {
                conv.scroll_offset = conv.scroll_offset.saturating_sub(1);
            }
            Action::Continue
        }
        KeyCode::Char('n') => {
            jump_to_next_match(app);
            Action::Continue
        }
        KeyCode::Char('N') => {
            jump_to_prev_match(app);
            Action::Continue
        }
        KeyCode::Char('t') => {
            if let Some(conv) = &app.conversation {
                let session_id = conv.session.id.clone();
                app.start_title_edit(session_id, Mode::Conversation);
            }
            Action::Continue
        }
        KeyCode::Char('f') => {
            if let Some(conv) = &app.conversation {
                let session_id = conv.session.id.clone();
                let cwd = conv.session.cwd.clone();
                app.start_fork_title(session_id, cwd);
            }
            Action::Continue
        }
        KeyCode::Char('/') => {
            if let Some(conv) = &mut app.conversation {
                conv.search_active = true;
                if conv.search_confirmed && !conv.search_query.is_empty() {
                    conv.search_replacing = true;
                    conv.search_cursor = conv.search_query.len();
                } else if !conv.initial_search_terms.is_empty() {
                    conv.search_query = conv.initial_search_terms.join(" ");
                    conv.search_replacing = true;
                    conv.search_cursor = conv.search_query.len();
                } else {
                    conv.search_query.clear();
                    conv.search_replacing = false;
                    conv.search_cursor = 0;
                }
                conv.search_confirmed = false;
                conv.rendered_width = 0;
            }
            app.mode = Mode::ConversationSearch;
            Action::Continue
        }
        KeyCode::Enter => {
            if let Some(conv) = &app.conversation {
                let cmd = conv.session.resume_command();
                Action::CopyCommand(cmd)
            } else {
                Action::Continue
            }
        }
        _ => Action::Continue,
    }
}

fn handle_conversation_search(app: &mut App, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc => {
            if let Some(conv) = &mut app.conversation {
                conv.search_active = false;
                conv.search_query.clear();
                conv.search_confirmed = false;
                conv.search_replacing = false;
                conv.rendered_width = 0;
            }
            app.mode = Mode::Conversation;
            Action::Continue
        }
        KeyCode::Enter => {
            if let Some(conv) = &mut app.conversation {
                conv.search_active = false;
                conv.search_replacing = false;
                if !conv.search_query.is_empty() && !conv.match_positions.is_empty() {
                    conv.search_confirmed = true;
                    conv.current_match = 0;
                    let max = conv.lines.len().saturating_sub(conv.page_height);
                    conv.scroll_offset = conv.match_positions[0]
                        .saturating_sub(conv.page_height / 2)
                        .min(max);
                }
            }
            app.mode = Mode::Conversation;
            Action::Continue
        }
        KeyCode::Backspace => {
            if let Some(conv) = &mut app.conversation {
                if conv.search_replacing {
                    conv.search_query.clear();
                    conv.search_cursor = 0;
                    conv.search_replacing = false;
                } else if conv.search_cursor > 0 {
                    let prev = conv.search_query[..conv.search_cursor]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    conv.search_query.remove(prev);
                    conv.search_cursor = prev;
                }
                conv.rendered_width = 0;
            }
            Action::Continue
        }
        KeyCode::Left => {
            if let Some(conv) = &mut app.conversation {
                if conv.search_replacing {
                    conv.search_replacing = false;
                    conv.search_cursor = 0;
                } else if conv.search_cursor > 0 {
                    conv.search_cursor = conv.search_query[..conv.search_cursor]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                }
            }
            Action::Continue
        }
        KeyCode::Right => {
            if let Some(conv) = &mut app.conversation {
                if conv.search_replacing {
                    conv.search_replacing = false;
                    conv.search_cursor = conv.search_query.len();
                } else if conv.search_cursor < conv.search_query.len() {
                    conv.search_cursor = conv.search_query[conv.search_cursor..]
                        .char_indices()
                        .nth(1)
                        .map(|(i, _)| conv.search_cursor + i)
                        .unwrap_or(conv.search_query.len());
                }
            }
            Action::Continue
        }
        KeyCode::Char(c) => {
            if let Some(conv) = &mut app.conversation {
                if conv.search_replacing {
                    conv.search_query.clear();
                    conv.search_cursor = 0;
                    conv.search_replacing = false;
                }
                conv.search_query.insert(conv.search_cursor, c);
                conv.search_cursor += c.len_utf8();
                conv.rendered_width = 0;
            }
            Action::Continue
        }
        _ => Action::Continue,
    }
}

fn handle_move_select(app: &mut App, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.move_state = None;
            app.mode = Mode::Browsing;
            Action::Continue
        }
        KeyCode::Enter => {
            if let Some(state) = app.move_state.take() {
                let (target, _, cwd) = &state.projects[state.selected];
                let target = target.clone();
                let cwd = cwd.clone();
                app.mode = Mode::Browsing;
                return Action::MoveSession {
                    display_idx: state.display_idx,
                    target_project: target,
                    target_cwd: cwd,
                };
            }
            app.mode = Mode::Browsing;
            Action::Continue
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Some(state) = &mut app.move_state {
                if state.selected + 1 < state.projects.len() {
                    state.selected += 1;
                }
            }
            Action::Continue
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if let Some(state) = &mut app.move_state {
                state.selected = state.selected.saturating_sub(1);
            }
            Action::Continue
        }
        KeyCode::Home => {
            if let Some(state) = &mut app.move_state {
                state.selected = 0;
            }
            Action::Continue
        }
        KeyCode::End => {
            if let Some(state) = &mut app.move_state {
                if !state.projects.is_empty() {
                    state.selected = state.projects.len() - 1;
                }
            }
            Action::Continue
        }
        _ => Action::Continue,
    }
}

fn handle_title_edit(app: &mut App, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc => {
            app.cancel_title_edit();
            Action::Continue
        }
        KeyCode::Enter => {
            match app.finish_title_edit() {
                Ok(Some(action)) => action,
                Ok(None) => Action::Continue,
                Err(msg) => {
                    app.set_status(msg);
                    Action::Continue
                }
            }
        }
        KeyCode::Backspace => {
            if let Some(state) = &mut app.title_edit {
                if state.cursor > 0 {
                    let prev = state.query[..state.cursor]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    state.query.remove(prev);
                    state.cursor = prev;
                }
            }
            Action::Continue
        }
        KeyCode::Left => {
            if let Some(state) = &mut app.title_edit {
                if state.cursor > 0 {
                    state.cursor = state.query[..state.cursor]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                }
            }
            Action::Continue
        }
        KeyCode::Right => {
            if let Some(state) = &mut app.title_edit {
                if state.cursor < state.query.len() {
                    let next = state.query[state.cursor..]
                        .char_indices()
                        .nth(1)
                        .map(|(i, _)| state.cursor + i)
                        .unwrap_or(state.query.len());
                    state.cursor = next;
                }
            }
            Action::Continue
        }
        KeyCode::Char(c) => {
            if let Some(state) = &mut app.title_edit {
                state.query.insert(state.cursor, c);
                state.cursor += c.len_utf8();
            }
            Action::Continue
        }
        _ => Action::Continue,
    }
}

fn handle_confirm_archive(app: &mut App, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Enter | KeyCode::Char('y') => {
            let idx = app.archive_confirm.take();
            app.mode = Mode::Browsing;
            if let Some(display_idx) = idx {
                return Action::ArchiveSession(display_idx);
            }
            Action::Continue
        }
        _ => {
            app.archive_confirm = None;
            app.mode = Mode::Browsing;
            Action::Continue
        }
    }
}

fn jump_to_next_match(app: &mut App) {
    if let Some(conv) = &mut app.conversation {
        if conv.match_positions.is_empty() {
            return;
        }
        conv.current_match = (conv.current_match + 1) % conv.match_positions.len();
        let max = conv.lines.len().saturating_sub(conv.page_height);
        conv.scroll_offset = conv.match_positions[conv.current_match]
            .saturating_sub(conv.page_height / 2)
            .min(max);
    }
}

fn jump_to_prev_match(app: &mut App) {
    if let Some(conv) = &mut app.conversation {
        if conv.match_positions.is_empty() {
            return;
        }
        if conv.current_match == 0 {
            conv.current_match = conv.match_positions.len() - 1;
        } else {
            conv.current_match -= 1;
        }
        let max = conv.lines.len().saturating_sub(conv.page_height);
        conv.scroll_offset = conv.match_positions[conv.current_match]
            .saturating_sub(conv.page_height / 2)
            .min(max);
    }
}
