//! Block model for the Warp-style output list.
//!
//! `BlockList` owns immutable command blocks, independent of the terminal grid.
//! `BlockPane` adds per-pane UI state (detection, running command, virtual list
//! scroll state) and the state machine that turns shell-integration events into
//! blocks. Nothing here touches GPUI rendering, which keeps the state machine
//! unit-testable.

pub mod render;
pub mod shell_integration;

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::time::Duration;

use gpui::{ListAlignment, ListState, Pixels, px};
use terminal::BlockSnapshot;

/// Hard cap on retained blocks. Older blocks are trimmed first.
pub const MAX_BLOCKS: usize = 10_000;

/// Outputs larger than this are parsed on the background executor.
pub const SYNC_PARSE_LIMIT: usize = 256 * 1024;

/// Actions a block card can dispatch. The host performs them (clipboard,
/// editor, model mutation).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockAction {
    CopyCommand,
    CopyOutput,
    CopyMarkdown,
    Rerun,
    ToggleCollapse,
    ToggleBookmark,
    ShowFullOutput,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockStatus {
    Success,
    Failed(i32),
    Unknown,
}

impl BlockStatus {
    pub fn from_exit(exit_code: Option<i32>) -> Self {
        match exit_code {
            Some(0) => Self::Success,
            Some(code) => Self::Failed(code),
            None => Self::Unknown,
        }
    }

    pub fn is_failure(self) -> bool {
        matches!(self, Self::Failed(_))
    }

    pub fn label(self) -> String {
        match self {
            Self::Success => "ok".to_string(),
            Self::Failed(code) => format!("exit {code}"),
            Self::Unknown => "unknown".to_string(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Block {
    pub id: u64,
    pub command: String,
    pub status: BlockStatus,
    pub snapshot: Rc<BlockSnapshot>,
    /// Output is still being parsed on the background executor.
    pub snapshot_pending: bool,
    pub exit_code: Option<i32>,
    pub duration: Duration,
    pub cwd: Option<String>,
    pub capture_cols: usize,
    pub capture_rows: usize,
    pub raw: Rc<Vec<u8>>,
    pub raw_truncated: bool,
    /// The command used the alternate screen; output is not replayable.
    pub full_screen: bool,
    pub collapsed: bool,
    pub bookmarked: bool,
    /// Width the snapshot was last wrapped at (capture width or reflow width).
    pub rendered_cols: usize,
    /// Show the full capped output instead of the preview limit.
    pub show_full_output: bool,
}

impl Block {
    pub fn line_count(&self) -> usize {
        self.snapshot.lines.len()
    }

    pub fn plain_output(&self) -> String {
        self.snapshot.plain_text()
    }

    /// Markdown payload used by the block's "share" action.
    pub fn to_markdown(&self) -> String {
        let mut out = format!("## `{}`\n\n", self.command);
        out.push_str(&format!(
            "Status: {} · {:.2}s\n\n",
            self.status.label(),
            self.duration.as_secs_f64()
        ));
        if let Some(cwd) = &self.cwd {
            out.push_str(&format!("cwd: `{cwd}`\n\n"));
        }
        if self.full_screen {
            out.push_str("_Full-screen application (output not captured)._\n");
        } else {
            out.push_str("```\n");
            out.push_str(&self.plain_output());
            out.push_str("\n```\n");
        }
        out
    }
}

/// Events the pane state machine consumes. The host maps terminal
/// `ShellEvent`s into these (parsing output in between).
#[derive(Clone, Debug)]
pub enum PaneEvent {
    PromptStart,
    CommandStarted {
        command_line: Option<String>,
        cwd: Option<String>,
    },
    CommandFinished {
        snapshot: BlockSnapshot,
        exit_code: Option<i32>,
        duration: Duration,
        full_screen: bool,
        raw: Vec<u8>,
        raw_truncated: bool,
        cols: usize,
        rows: usize,
    },
    CwdChanged(String),
}

#[derive(Default)]
pub struct BlockList {
    blocks: VecDeque<Block>,
    next_id: u64,
    selected: Option<usize>,
}

impl BlockList {
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    pub fn blocks(&self) -> &VecDeque<Block> {
        &self.blocks
    }

    pub fn get(&self, index: usize) -> Option<&Block> {
        self.blocks.get(index)
    }

    pub fn get_mut(&mut self, index: usize) -> Option<&mut Block> {
        self.blocks.get_mut(index)
    }

    pub fn last(&self) -> Option<&Block> {
        self.blocks.back()
    }

    pub fn selected_index(&self) -> Option<usize> {
        self.selected
    }

    pub fn selected(&self) -> Option<&Block> {
        self.selected.and_then(|index| self.blocks.get(index))
    }

    /// Append a block and return its index. Trims the oldest blocks past the
    /// cap and keeps the selection inside the list.
    pub fn push(&mut self, mut block: Block) -> usize {
        block.id = self.next_id;
        self.next_id += 1;
        self.blocks.push_back(block);

        while self.blocks.len() > MAX_BLOCKS {
            self.blocks.pop_front();
            self.selected = self.selected.and_then(|index| index.checked_sub(1));
        }

        let index = self.blocks.len().saturating_sub(1);
        if self.selected.is_none() {
            self.selected = Some(index);
        }
        index
    }

    pub fn select_next(&mut self) -> Option<usize> {
        if self.blocks.is_empty() {
            return None;
        }
        let next = match self.selected {
            Some(index) => (index + 1).min(self.blocks.len() - 1),
            None => 0,
        };
        self.selected = Some(next);
        Some(next)
    }

    pub fn select_prev(&mut self) -> Option<usize> {
        if self.blocks.is_empty() {
            return None;
        }
        let prev = match self.selected {
            Some(index) => index.saturating_sub(1),
            None => self.blocks.len() - 1,
        };
        self.selected = Some(prev);
        Some(prev)
    }

    pub fn select(&mut self, index: usize) -> Option<usize> {
        if index < self.blocks.len() {
            self.selected = Some(index);
            Some(index)
        } else {
            None
        }
    }

    pub fn toggle_collapse(&mut self, index: usize) -> bool {
        let Some(block) = self.blocks.get_mut(index) else {
            return false;
        };
        block.collapsed = !block.collapsed;
        block.collapsed
    }

    pub fn toggle_bookmark(&mut self, index: usize) -> bool {
        let Some(block) = self.blocks.get_mut(index) else {
            return false;
        };
        block.bookmarked = !block.bookmarked;
        block.bookmarked
    }

    pub fn mark_pending(&mut self, id: u64) -> bool {
        let Some(block) = self.blocks.iter_mut().find(|block| block.id == id) else {
            return false;
        };
        block.snapshot_pending = true;
        true
    }

    pub fn set_snapshot(&mut self, id: u64, snapshot: BlockSnapshot) -> bool {
        let Some(block) = self.blocks.iter_mut().find(|block| block.id == id) else {
            return false;
        };
        block.snapshot = Rc::new(snapshot);
        block.snapshot_pending = false;
        true
    }

    pub fn set_reflowed(&mut self, id: u64, cols: usize, snapshot: BlockSnapshot) -> bool {
        let Some(block) = self.blocks.iter_mut().find(|block| block.id == id) else {
            return false;
        };
        block.snapshot = Rc::new(snapshot);
        block.rendered_cols = cols;
        block.snapshot_pending = false;
        true
    }

    pub fn clear(&mut self) {
        self.blocks.clear();
        self.selected = None;
    }
}

pub struct BlockPane {
    pub list: BlockList,
    /// `true` after the first OSC 133 prompt marker.
    pub detected: bool,
    /// A command is between `OSC 133;C` and `OSC 133;D`.
    pub running: bool,
    /// User asked for a plain terminal for this pane.
    pub classic_mode: bool,
    pub pending_command: Option<String>,
    pub pending_cwd: Option<String>,
    pub shell_cwd: Option<String>,
    pub list_state: ListState,
    /// Shared with the list's scroll handler: `true` when the newest block is
    /// visible, so new blocks should pin the view to the bottom.
    pub follow_tail: Rc<Cell<bool>>,
    /// Cached immutable card data for the virtualized list closure.
    pub cards: Option<Rc<Vec<BlockCardData>>>,
    pub cards_dirty: bool,
    pub last_width: Pixels,
    /// Grid columns the cards were last wrapped at.
    pub last_cols: usize,
    /// Blocks with an in-flight background reflow.
    pub reflowing: HashSet<u64>,
    /// Wrapped output cache, filled lazily only for cards the list renders.
    pub wrap_cache: WrapCache,
}

#[derive(Clone)]
pub struct WrapEntry {
    pub cols: usize,
    pub rows: Rc<Vec<render::WrappedRow>>,
    pub total_rows: usize,
    pub display_capped: bool,
}

pub type WrapCache = Rc<RefCell<HashMap<u64, WrapEntry>>>;

impl Default for BlockPane {
    fn default() -> Self {
        let follow_tail = Rc::new(Cell::new(true));
        let list_state = ListState::new(0, ListAlignment::Bottom, px(512.0));
        let follow = follow_tail.clone();
        list_state.set_scroll_handler(move |event, _window, _cx| {
            follow.set(event.count == 0 || event.visible_range.end >= event.count);
        });
        Self {
            list: BlockList::default(),
            detected: false,
            running: false,
            classic_mode: false,
            pending_command: None,
            pending_cwd: None,
            shell_cwd: None,
            list_state,
            follow_tail,
            cards: None,
            cards_dirty: true,
            last_width: px(0.0),
            last_cols: 0,
            reflowing: HashSet::new(),
            wrap_cache: Rc::new(RefCell::new(HashMap::new())),
        }
    }
}

/// Immutable per-block data captured for the list's render closure.
#[derive(Clone, Debug)]
pub struct BlockCardData {
    pub id: u64,
    pub index: usize,
    pub command: String,
    pub status: BlockStatus,
    pub exit_code: Option<i32>,
    pub duration: Duration,
    pub cwd: Option<String>,
    pub line_count: usize,
    pub truncated: bool,
    pub full_screen: bool,
    pub collapsed: bool,
    pub bookmarked: bool,
    pub snapshot_pending: bool,
    pub snapshot: Rc<BlockSnapshot>,
    pub raw: Rc<Vec<u8>>,
    pub capture_cols: usize,
    pub capture_rows: usize,
    pub rendered_cols: usize,
    pub show_full_output: bool,
    pub selected: bool,
}

impl BlockPane {
    /// Content area should show the block list. Until the first command has
    /// completed there is nothing to show, so the real grid stays visible
    /// (important because fish emits OSC 133 on its very first prompt).
    pub fn shows_blocks(&self) -> bool {
        self.detected && !self.classic_mode && !self.running && !self.list.is_empty()
    }

    /// Called by the editor when the user submits a command, before any PTY
    /// bytes are written. The `C` marker then confirms it started.
    pub fn note_submitted(&mut self, command: &str) {
        if !command.is_empty() {
            self.pending_command = Some(command.to_string());
        }
    }

    pub fn handle(&mut self, event: PaneEvent) -> Option<usize> {
        match event {
            PaneEvent::PromptStart => {
                self.detected = true;
                self.running = false;
                self.pending_command = None;
                self.pending_cwd = None;
                None
            }
            PaneEvent::CommandStarted { command_line, cwd } => {
                self.detected = true;
                self.running = true;
                if self.pending_command.is_none() {
                    self.pending_command = command_line;
                }
                if cwd.is_some() {
                    self.pending_cwd = cwd;
                }
                None
            }
            PaneEvent::CommandFinished {
                snapshot,
                exit_code,
                duration,
                full_screen,
                raw,
                raw_truncated,
                cols,
                rows,
            } => {
                self.running = false;
                let command = self.pending_command.take().unwrap_or_default();
                let cwd = self.pending_cwd.take().or_else(|| self.shell_cwd.clone());
                // Markers with neither a command nor output are noise (empty
                // Enter presses, shell prompt redraws).
                if command.trim().is_empty() && snapshot.is_empty() && !full_screen {
                    return None;
                }
                let status = BlockStatus::from_exit(exit_code);
                let block = Block {
                    id: 0,
                    command,
                    status,
                    snapshot: Rc::new(snapshot),
                    snapshot_pending: false,
                    exit_code,
                    duration,
                    cwd,
                    capture_cols: cols,
                    capture_rows: rows,
                    raw: Rc::new(raw),
                    raw_truncated,
                    full_screen,
                    collapsed: false,
                    bookmarked: false,
                    rendered_cols: cols,
                    show_full_output: false,
                };
                let index = self.list.push(block);
                // Bottom alignment already anchors an unspecified scroll top
                // to the newest item. Preserve the user's position only when
                // they have scrolled away from the tail.
                let keep_offset = if self.follow_tail.get() {
                    None
                } else {
                    Some(self.list_state.logical_scroll_top())
                };
                self.list_state.reset(self.list.len());
                if let Some(offset) = keep_offset {
                    self.list_state.scroll_to(offset);
                }
                self.cards_dirty = true;
                if self.list.selected_index() != Some(index) {
                    let _ = self.list.select(index);
                }
                Some(index)
            }
            PaneEvent::CwdChanged(path) => {
                self.shell_cwd = Some(path);
                None
            }
        }
    }

    /// Populate the pane with `count` synthetic successful blocks. Used by the
    /// 5,000-block scroll stress test and `KATHAMO_BLOCK_STRESS`.
    pub fn inject_synthetic_blocks(&mut self, count: usize) {
        self.detected = true;
        for index in 0..count {
            let text = format!("output line {index}");
            let line = terminal::BlockLine {
                cells: text.chars().count(),
                runs: vec![terminal::BlockRun {
                    text,
                    fg: gpui::black(),
                    bg: None,
                    bold: false,
                    italic: false,
                    underline: false,
                    strikethrough: false,
                    wavy: false,
                }],
            };
            self.handle(PaneEvent::CommandFinished {
                snapshot: BlockSnapshot {
                    lines: vec![line],
                    truncated: false,
                },
                exit_code: Some(0),
                duration: Duration::from_millis(index as u64 % 900),
                full_screen: false,
                raw: Vec::new(),
                raw_truncated: false,
                cols: 80,
                rows: 24,
            });
        }
    }

    /// Rebuild the immutable card cache if needed, wrapping output to `cols`.
    pub fn refresh_cards(&mut self, cols: usize) {
        if !self.cards_dirty {
            return;
        }
        let cols = cols.max(2);
        let selected = self.list.selected_index();
        let cards = self
            .list
            .blocks()
            .iter()
            .enumerate()
            .map(|(index, block)| BlockCardData {
                id: block.id,
                index,
                command: block.command.clone(),
                status: block.status,
                exit_code: block.exit_code,
                duration: block.duration,
                cwd: block.cwd.clone(),
                line_count: block.line_count(),
                truncated: block.snapshot.truncated || block.raw_truncated,
                full_screen: block.full_screen,
                collapsed: block.collapsed,
                bookmarked: block.bookmarked,
                snapshot_pending: block.snapshot_pending,
                snapshot: Rc::clone(&block.snapshot),
                raw: Rc::clone(&block.raw),
                capture_cols: block.capture_cols,
                capture_rows: block.capture_rows,
                rendered_cols: block.rendered_cols,
                show_full_output: block.show_full_output,
                selected: selected == Some(index),
            })
            .collect();
        self.cards = Some(Rc::new(cards));
        self.cards_dirty = false;
        self.last_cols = cols;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use terminal::BlockSnapshot;

    fn finished(exit: Option<i32>) -> PaneEvent {
        PaneEvent::CommandFinished {
            snapshot: BlockSnapshot::default(),
            exit_code: exit,
            duration: Duration::from_millis(42),
            full_screen: false,
            raw: Vec::new(),
            raw_truncated: false,
            cols: 80,
            rows: 24,
        }
    }

    fn pane() -> BlockPane {
        BlockPane::default()
    }

    #[test]
    fn status_mapping() {
        assert_eq!(BlockStatus::from_exit(Some(0)), BlockStatus::Success);
        assert_eq!(BlockStatus::from_exit(Some(3)), BlockStatus::Failed(3));
        assert_eq!(BlockStatus::from_exit(None), BlockStatus::Unknown);
        assert!(BlockStatus::Failed(1).is_failure());
    }

    #[test]
    fn state_machine_builds_a_block() {
        let mut pane = pane();
        assert!(!pane.detected);
        pane.handle(PaneEvent::PromptStart);
        assert!(pane.detected);

        pane.note_submitted("ls -la");
        pane.handle(PaneEvent::CommandStarted {
            command_line: None,
            cwd: Some("/tmp".into()),
        });
        assert!(pane.running);
        assert!(!pane.shows_blocks());

        let index = pane.handle(finished(Some(0))).expect("block index");
        assert_eq!(index, 0);
        assert!(!pane.running);
        assert!(pane.shows_blocks());
        let block = pane.list.get(0).expect("block");
        assert_eq!(block.command, "ls -la");
        assert_eq!(block.status, BlockStatus::Success);
        assert_eq!(block.cwd.as_deref(), Some("/tmp"));
        assert_eq!(block.duration, Duration::from_millis(42));
    }

    #[test]
    fn command_line_falls_back_when_editor_did_not_supply_one() {
        let mut pane = pane();
        pane.handle(PaneEvent::PromptStart);
        pane.handle(PaneEvent::CommandStarted {
            command_line: Some("echo hi".into()),
            cwd: None,
        });
        pane.handle(finished(Some(7)));
        let block = pane.list.get(0).unwrap();
        assert_eq!(block.command, "echo hi");
        assert_eq!(block.status, BlockStatus::Failed(7));
    }

    #[test]
    fn prompt_start_clears_a_running_command() {
        let mut pane = pane();
        pane.handle(PaneEvent::PromptStart);
        pane.handle(PaneEvent::CommandStarted {
            command_line: Some("sleep 100".into()),
            cwd: None,
        });
        assert!(pane.running);
        pane.handle(PaneEvent::PromptStart);
        assert!(!pane.running);
        assert!(pane.list.is_empty());
    }

    #[test]
    fn navigation_clamps_at_the_ends() {
        let mut pane = pane();
        pane.handle(PaneEvent::PromptStart);
        for index in 0..3 {
            pane.note_submitted(&format!("cmd {index}"));
            pane.handle(PaneEvent::CommandStarted {
                command_line: None,
                cwd: None,
            });
            pane.handle(finished(Some(0)));
        }
        assert_eq!(pane.list.len(), 3);
        assert_eq!(pane.list.selected_index(), Some(2));
        assert_eq!(pane.list.select_next(), Some(2));
        assert_eq!(pane.list.select_prev(), Some(1));
        assert_eq!(pane.list.select_prev(), Some(0));
        assert_eq!(pane.list.select_prev(), Some(0));
    }

    #[test]
    fn collapse_and_bookmark_are_per_block() {
        let mut pane = pane();
        pane.handle(PaneEvent::PromptStart);
        pane.note_submitted("one");
        pane.handle(PaneEvent::CommandStarted {
            command_line: None,
            cwd: None,
        });
        pane.handle(finished(Some(0)));

        assert!(!pane.list.get(0).unwrap().collapsed);
        assert!(pane.list.toggle_collapse(0));
        assert!(pane.list.get(0).unwrap().collapsed);
        assert!(!pane.list.toggle_collapse(0));

        assert!(!pane.list.get(0).unwrap().bookmarked);
        assert!(pane.list.toggle_bookmark(0));
        assert!(pane.list.get(0).unwrap().bookmarked);
    }

    #[test]
    fn trimming_keeps_the_selection_valid() {
        let mut list = BlockList::default();
        let block = || Block {
            id: 0,
            command: String::new(),
            status: BlockStatus::Success,
            snapshot: Rc::new(BlockSnapshot::default()),
            snapshot_pending: false,
            exit_code: Some(0),
            duration: Duration::ZERO,
            cwd: None,
            capture_cols: 80,
            capture_rows: 24,
            raw: Rc::new(Vec::new()),
            raw_truncated: false,
            full_screen: false,
            collapsed: false,
            bookmarked: false,
            rendered_cols: 80,
            show_full_output: false,
        };
        for _ in 0..(MAX_BLOCKS + 5) {
            list.push(block());
        }
        assert_eq!(list.len(), MAX_BLOCKS);
        let selected = list.selected_index().unwrap();
        assert!(selected < list.len());
    }

    #[test]
    fn cards_are_rebuilt_when_dirty() {
        let mut pane = pane();
        pane.handle(PaneEvent::PromptStart);
        pane.note_submitted("one");
        pane.handle(PaneEvent::CommandStarted {
            command_line: None,
            cwd: None,
        });
        pane.handle(finished(Some(0)));
        pane.refresh_cards(80);
        let cards = pane.cards.clone().expect("cards");
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].command, "one");

        pane.refresh_cards(80);
        assert!(
            pane.cards
                .as_ref()
                .is_some_and(|current| Rc::ptr_eq(current, &cards))
        );
        pane.list.toggle_collapse(0);
        // The model changed but the cache is only invalidated by handle/push;
        // callers that mutate blocks ask for a refresh.
        pane.cards_dirty = true;
        pane.refresh_cards(80);
        assert!(pane.cards.as_ref().unwrap()[0].collapsed);
    }

    #[test]
    fn five_thousand_blocks_cache_and_wrap_quickly() {
        let mut pane = BlockPane::default();
        pane.inject_synthetic_blocks(5_000);
        assert_eq!(pane.list.len(), 5_000);
        pane.refresh_cards(120);
        let cards = pane.cards.as_ref().expect("cards");
        assert_eq!(cards.len(), 5_000);
        assert_eq!(cards[4_999].line_count, 1);
        let (wrapped, total, capped) =
            render::wrap_snapshot(&cards[4_999].snapshot, 120, render::MAX_RENDER_LINES);
        assert_eq!(total, 1);
        assert!(!capped);
        assert!(!wrapped.is_empty());
    }

    #[test]
    fn bash_highlighter_is_available() {
        use gpui_component::highlighter::{HighlightTheme, SyntaxHighlighter};
        use gpui_component::input::Rope;

        let text = Rope::from_str("echo hi # comment\n");
        let mut highlighter = SyntaxHighlighter::new("bash");
        highlighter.update(None, &text);
        let styles = highlighter.styles(&(0..text.len()), &HighlightTheme::default_dark());
        assert!(
            !styles.is_empty(),
            "bash grammar should highlight a comment"
        );
    }

    #[test]
    fn markdown_contains_command_and_output() {
        let block = Block {
            id: 0,
            command: "echo hi".into(),
            status: BlockStatus::Success,
            snapshot: Rc::new(BlockSnapshot::default()),
            snapshot_pending: false,
            exit_code: Some(0),
            duration: Duration::from_millis(10),
            cwd: None,
            capture_cols: 80,
            capture_rows: 24,
            raw: Rc::new(Vec::new()),
            raw_truncated: false,
            full_screen: false,
            collapsed: false,
            bookmarked: false,
            rendered_cols: 80,
            show_full_output: false,
        };
        let markdown = block.to_markdown();
        assert!(markdown.contains("echo hi"));
        assert!(markdown.contains("ok"));
    }
}
