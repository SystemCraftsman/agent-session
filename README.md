# Agent Sessions

A fast terminal UI for browsing, searching, replaying, forking, and transferring your AI coding sessions across **Claude Code**, **Codex CLI**, and **Cursor CLI**, all from one place.

## Overview

Every coding agent stores its own conversation history in its own format and its own directory. Agent Sessions reads all of them, merges the sessions into a single searchable list, and lets you jump back into any of them with one keypress. It also lets you carry a conversation from one agent into another, so work you started in Claude can continue in Codex or Cursor.

Built in Rust for instant startup, it discovers and renders thousands of sessions in well under a second by parsing transcripts in parallel.

## Supported Agents

Agent Sessions discovers sessions from all three agents automatically and tags each one with a colored badge in the list.

| Agent  | Source directory        | Resume mechanism                          |
|--------|-------------------------|-------------------------------------------|
| Claude | `~/.claude/projects/`   | `claude -r <id>` (native)                 |
| Codex  | `~/.codex/sessions/`    | `codex resume <id>` (native)              |
| Cursor | `~/.cursor/projects/`   | `cursor-agent --resume <id>` (cloud)      |

## Features

The tool is organized around a single-line-per-session browser that flows straight into search and conversation replay.

### Unified Multi-Agent Browser

All Claude, Codex, and Cursor sessions appear in one list, newest first, each with an agent badge, project name, git branch, and relative timestamp. You can view sessions flat or grouped by project.

### Seamless Search

Start typing to filter instantly by project name, branch, or first message; no mode switch is needed. Matches are case-insensitive substrings, and a short debounce triggers a background deep search that scans full conversation content so sessions matching only deep inside the transcript still surface.

### Conversation Replay

Press Enter on a session to open the conversation viewer, which renders the full exchange with syntax-highlighted code blocks, tables, markdown headings, inline styling, and clickable URLs. Press `/` to search within a conversation and `n` / `N` to jump between matches.

### Cross-Agent Transfer And Fork

This is what sets Agent Sessions apart from a single-agent history viewer. From the fork picker you can clone any session into any of the three agents:

- **Claude and Codex targets** are reconstructed into a real, native session file that the target's own CLI can resume directly.
- **Cursor targets** are context-seeded: because Cursor resumes from its own cloud, the source transcript is written to an import file and a fresh `cursor-agent` chat is launched pre-instructed to read it and continue.

Titles travel with the clone: a same-agent copy is labeled `(fork)` and a cross-agent copy is labeled `(reconstruct)`, matching across all three agents.

### Session Management

Beyond browsing, you can rename sessions with custom titles, archive sessions out of the active list without deleting them, and move a session to a different project directory.

### Theme And Time Filters

Rendering auto-detects a dark or light terminal background, with `--dark` / `--light` overrides. Use `--since 7d` and `--last 50` to scope which sessions load.

## Installation

The primary way to install today is to build from source with a stable Rust toolchain (1.80.0 or newer).

```bash
git clone https://github.com/SystemCraftsman/agent-session.git
cd agent-session
cargo build --release
```

The compiled binary is written to `target/release/agent-session`; copy it somewhere on your `PATH` to run it from anywhere.

## Usage

Run the command with no arguments to browse every session across all installed agents.

```bash
agent-session                 # browse all sessions
agent-session --last 20       # only the 20 most recent
agent-session --since 7d      # only sessions from the last 7 days
agent-session --dark          # force the dark theme
```

## Key Bindings

The session list and the conversation viewer each have their own bindings.

### Session List

| Key                 | Action                                         |
|---------------------|------------------------------------------------|
| `Down` / `Up`       | Move the cursor                                |
| Type any text       | Filter sessions (seamless search)              |
| `Enter`             | Open the selected session's conversation       |
| `t`                 | Rename the selected session (custom title)     |
| `a`                 | Archive the selected session                   |
| `m`                 | Move the session to another project            |
| `f`                 | Fork or transfer the session to another agent  |
| `Esc`               | Clear the filter, then quit                    |
| `Ctrl-C`            | Quit                                           |

### Conversation Viewer

| Key                    | Action                          |
|------------------------|---------------------------------|
| `Space` / `PageDown`   | Scroll down one page            |
| `b` / `PageUp`         | Scroll up one page              |
| `g` / `G`              | Jump to top / bottom            |
| `Down` / `Up`          | Scroll one line                 |
| `/`                    | Search within the conversation  |
| `n` / `N`              | Jump to next / previous match   |
| `Esc`                  | Exit search, then the viewer    |

## How It Works

1. **Discovery** scans each agent's session directory in parallel with rayon and reads session metadata from the transcript files.
2. **Parsing** extracts the first message, timestamp, project, and git branch, cleaning agent-specific markup as it goes.
3. **Display** shows one line per session, left-aligned project and message with a right-aligned timestamp.
4. **Deep search** scans full conversation content in the background after a short debounce and merges those matches into the list.
5. **Transfer** reconstructs native session files for Claude and Codex, and context-seeds a fresh chat for Cursor.

## Storage Locations

Agent Sessions reads each agent's own history and keeps its small amount of extra state in the agent home directories.

| Path                                          | Purpose                                  |
|-----------------------------------------------|------------------------------------------|
| `~/.claude/projects/`                         | Claude session transcripts (read)        |
| `~/.codex/sessions/`                          | Codex session rollouts (read)            |
| `~/.cursor/projects/`                         | Cursor session transcripts (read)        |
| `<agent-home>/agent-session-titles.json`      | Custom title sidecar (Codex, Cursor)     |
| `~/.cursor/agent-session-imports/`            | Import docs written for Cursor seeding    |

## License

MIT
