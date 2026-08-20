# Changelog

## v0.1.0

First beta release under the new name **Agent Session** (formerly cc-session). Agent Session is a fast terminal UI for browsing, searching, replaying, forking, and transferring AI coding sessions across Claude Code, Codex CLI, and Cursor CLI from one place.

### What's New

- **Unified multi-agent browser**: Claude, Codex, and Cursor sessions appear in a single list, newest first, each tagged with a colored agent badge and shown with project name, git branch, and relative timestamp. Toggle between flat and grouped-by-project views with `Tab`.
- **Seamless search**: Start typing to filter instantly by project, branch, or first message; a short debounce triggers a background deep search across full conversation content.
- **Conversation replay**: Open any session to view the full exchange with syntax-highlighted code blocks, tables, markdown headings, inline styling, and clickable URLs. Search within a conversation with `/` and jump between matches with `n` / `N`.
- **Cross-agent transfer and fork**: Clone any session into any of the three agents. Claude and Codex targets are reconstructed into native session files the target CLI can resume directly; Cursor targets are context-seeded via an import doc and a fresh pre-instructed `cursor-agent` chat. Titles travel with the clone (`(fork)` same-agent, `(reconstruct)` cross-agent).
- **Session management**: Rename sessions with custom titles (`Ctrl-t`), archive them out of the active list without deleting (`Ctrl-a`), move a session to another project (`Ctrl-v`), and start a new session in a project (`Ctrl-n`).
- **Consistent Ctrl-prefixed shortcuts**: All action shortcuts use a `Ctrl` prefix so plain typing always goes to the seamless search filter.
- **Theme and time filters**: Auto-detects a dark or light terminal background with `--dark` / `--light` overrides, and scopes loading with `--since` and `--last`.
