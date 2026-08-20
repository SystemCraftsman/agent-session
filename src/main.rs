mod codex;
mod convert;
mod cursor;
mod discovery;
mod filter;
mod router;
mod search;
mod session;
mod theme;
mod titles;
mod tui;

use clap::Parser;

use discovery::{apply_filters, discover_sessions, get_claude_home};

/// Fast TUI for browsing, forking, and transferring AI coding sessions
/// across Claude, Codex, and Cursor.
#[derive(Parser, Debug)]
#[command(
    name = "agent-session",
    version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("BUILD_GIT_HASH"), " ", env!("BUILD_DATE"), ")"),
    about
)]
struct Cli {
    /// Only show sessions newer than duration (e.g. 7d, 2w, 1m)
    #[arg(long)]
    since: Option<String>,

    /// Show at most N sessions
    #[arg(long)]
    last: Option<usize>,

    /// Force light color theme
    #[arg(long = "light", conflicts_with = "dark")]
    light: bool,

    /// Force dark color theme
    #[arg(long = "dark", conflicts_with = "light")]
    dark: bool,

    /// Start in flat list view (default: grouped by project)
    #[arg(long = "flat")]
    flat: bool,
}

/// Parse a human-friendly duration string into a chrono::Duration.
///
/// Supported suffixes: `d` (days), `w` (weeks), `m` (30-day months).
fn parse_duration(s: &str) -> Result<chrono::Duration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty duration string".to_string());
    }

    let (num_str, suffix) = s.split_at(s.len() - 1);
    let num: i64 = num_str
        .parse()
        .map_err(|_| format!("invalid number in duration: {num_str:?}"))?;

    match suffix {
        "d" => Ok(chrono::Duration::days(num)),
        "w" => Ok(chrono::Duration::weeks(num)),
        "m" => Ok(chrono::Duration::days(num * 30)),
        other => Err(format!(
            "unknown duration suffix: {other:?} (expected d, w, or m)"
        )),
    }
}

fn main() {
    let cli = Cli::parse();

    let claude_home = get_claude_home();
    let projects_dir = claude_home.join("projects");
    let codex_home = codex::get_codex_home();
    let codex_sessions_dir = codex_home.join("sessions");
    let cursor_home = cursor::get_cursor_home();
    let cursor_projects_dir = cursor_home.join("projects");

    if !projects_dir.is_dir() && !codex_sessions_dir.is_dir() && !cursor_projects_dir.is_dir() {
        eprintln!(
            "No sessions found (looked in {}, {} and {})",
            projects_dir.display(),
            codex_sessions_dir.display(),
            cursor_projects_dir.display()
        );
        std::process::exit(2);
    }

    let mut sessions = discover_sessions(&claude_home);
    sessions.extend(codex::discover_codex_sessions(&codex_home));
    sessions.extend(cursor::discover_cursor_sessions(&cursor_home));
    // Merge all agents into a single newest-first list.
    sessions.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));

    // Apply --since filter
    let since_duration = cli.since.map(|s| {
        parse_duration(&s).unwrap_or_else(|e| {
            eprintln!("Invalid --since value: {e}");
            std::process::exit(1);
        })
    });

    let sessions = apply_filters(sessions, since_duration, cli.last);

    // Determine color theme
    let theme = if cli.light {
        theme::Theme::light()
    } else if cli.dark {
        theme::Theme::dark()
    } else {
        theme::Theme::detect()
    };

    // Interactive TUI
    let grouped_view = !cli.flat;
    if let Err(e) = tui::run(sessions, theme, grouped_view) {
        eprintln!("TUI error: {e}");
        std::process::exit(1);
    }
}
