# Changelog

All notable changes to Kathamo will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [Unreleased]

### Added
- Render streamed agent replies as Markdown, including headings, lists, links, and syntax-highlighted code blocks.
- Redesigned agent tool calls into Zed-style collapsible cards with tool badges, inline argument summaries, status dots, line counts, bounded scrollable output blocks, and one-click output copying.
- Collapsible thinking accordions that display live streaming thoughts while reasoning and automatically fold into compact headers when the response begins.
- Pre-warm agent bridge processes in the background upon switching to Agent mode or selecting a model.
- Enable RPC idle cache warming and default to Fast mode (`low` thinking effort) for rapid responses, with an interactive `⚡ Fast`/`🧠 Deep` toggle and `/fast` / `/thinking` slash commands.
- Model selector for rpi and Google Antigravity, with a persistent default provider/model for new panes.
- Google Antigravity OAuth and model selection in `/auth`, with separate native chat sessions using the installed oh-my-pi agent.
- `/auth` in agent mode opens provider discovery and authentication with masked input and rpi-managed credentials.
- Native per-pane pi-rust agent chat with streamed replies, tool activity, cancellation, and a terminal/agent switch. Requires a separately installed and configured `rpi` executable.
- **Kathamo input editor**:
  - Shell history from zsh, bash, and fish history files, merged with the session's own commands for Up/Down recall and ghost suggestions.
  - Context-aware completion menu (Tab) for `PATH` commands, shell builtins, files and directories, git branches, and history — with arrow navigation and click-to-accept.
  - Inline ghost suggestion from history, accepted with Right or Tab.
  - Ctrl+R reverse history search palette with live filtering.
  - Readline editing in the input bar: Ctrl+A/E, Ctrl+U/K/W, Alt+B/F, Ctrl+P/N, Shift+Enter newline, and Ctrl+C interrupt.
  - Multi-line programmatic edits now leave the cursor at the end of the line.

### Changed
- Poll active agent streams at approximately frame cadence while retaining the lower idle polling rate.
- Migrated the application and terminal renderer to GPUI Fast.
- The terminal displays a plain grid without block UI. The Warp-style bottom input is always available when the shell is idle and temporarily hides while a command runs or the alternate screen is active.
- Removed the bottom input toggle; a previously saved `bottom_input` preference no longer disables the input.

### Fixed
- Keep Markdown agent replies updating during continuous streams and reduce OMP streaming payloads.
- Display streamed agent thinking text instead of leaving the chat blank while a model is reasoning.
- `/model` opens model selection instead of authentication; selecting an already authenticated provider no longer starts another login.
- System appearance changes now update existing terminal panes and application chrome; theme changes preserve the configured component fonts.
- Notify compatible running terminal applications when the terminal palette changes, so their light and dark themes can update without a restart.

---

## [0.0.1] - 2026-11-01

### Added
- **GPU-Accelerated Terminal Emulator**:
  - Embedded `terminal` component utilizing GPUI and `alacritty_terminal` for VTE parsing.
  - High-performance 1 ms stream coalescing with a 128 KiB parse cap to preserve UI framerates at high log throughput.
  - Full support for ANSI colors (standard 16, 256-color, 24-bit RGB), text styles, and embedded Mononoki monospace fonts.
  - OSC 52 clipboard copy and paste integration.
- **Agent Attention & Status System**:
  - Parity with the Kathamo agent attention model (`blocked > done > working > idle`).
  - Latching completion tracking: agent runs that finish in an unviewed tab switch to `done` and alert the user; focusing the tab clears status to `idle`.
  - Audio chimes and desktop toast notifications for blocked queries and missed finishes (`KATHAMO_NO_SOUND=1` support).
  - Sub-agent hierarchy support: child sessions with `agent_id` nest neatly under parent sessions without duplicate notifications.
- **Multi-Project Workspace Management**:
  - Collapsible project folders in the sidebar grouping related terminal sessions.
  - Workspace state persistence to `~/.config/kathamo/workspace.json`, automatically restoring open projects and session trees across application restarts.
  - Project-level actions for adding sessions and opening directories.
- **Native Agent Integrations & CLI**:
  - One-command setup CLI (`kathamo integration setup`, `status`, `doctor`, `remove`).
  - Native hook support for Claude Code, Codex, Pi, OMP, OpenCode v2, and Command Code (Kathamo compatibility protocol).
  - Terminal CLI commands: `kathamo report`, `kathamo skill`, `kathamo status`, `kathamo pane`, and OSC 7501 ANSI sequence reporting.
- **Desktop Integration**:
  - Custom title bar and window decoration controls.
  - Automatic Linux desktop entry (`.desktop`) creation in `~/.local/share/applications/`.
  - Taskbar icon support for Linux, macOS, and Windows.
  - GitHub Dark palette theme.
