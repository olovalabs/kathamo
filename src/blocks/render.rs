//! Block card rendering: output wrapping, ANSI-aware painting, chrome and
//! per-block action buttons.
//!
//! The output is captured as styled runs (see `terminal::shell_integration`),
//! so this module only has to wrap those runs to the current card width and
//! paint them with the terminal's cell metrics.

use std::rc::Rc;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, Bounds, CursorStyle, Edges, Font, FontFeatures, FontStyle, FontWeight, Hsla,
    InteractiveElement as _, IntoElement, MouseButton, ParentElement, Pixels, Point, SharedString,
    Stateful, StrikethroughStyle, Styled, TextRun, UnderlineStyle, WeakEntity, Window, div, px,
    quad, size, transparent_black,
};
use terminal::{BlockRun, BlockSnapshot};
use unicode_width::UnicodeWidthChar as _;

use crate::AppState;
use crate::blocks::{BlockAction, BlockCardData};
use crate::theme::ThemeTokens;

/// Lines rendered per card by default. Cards taller than this keep the full
/// snapshot but only lay out the first N rows until "show full" is clicked.
pub const MAX_RENDER_LINES: usize = 1_000;
pub const MAX_RENDER_LINES_EXPANDED: usize = 10_000;

#[derive(Clone, Debug)]
pub struct PlacedRun {
    pub run: BlockRun,
    pub start_col: usize,
    pub cells: usize,
}

pub type WrappedRow = Vec<PlacedRun>;

#[derive(Clone)]
pub struct CardStyle {
    pub tokens: ThemeTokens,
    pub font_size: Pixels,
    pub cell_width: Pixels,
    pub cell_height: Pixels,
    pub mono_font: SharedString,
    pub target_cols: usize,
}

/// Wrap a snapshot to `cols` cells, stopping after `limit` display rows.
/// Returns the rows, the unwrapped total row count, and whether the display
/// was capped.
pub fn wrap_snapshot(
    snapshot: &BlockSnapshot,
    cols: usize,
    limit: usize,
) -> (Rc<Vec<WrappedRow>>, usize, bool) {
    let cols = cols.max(1);
    let limit = limit.max(1);
    let mut rows = Vec::new();
    let mut total = 0usize;

    for line in &snapshot.lines {
        let wrapped = wrap_line(line, cols);
        total += wrapped.len();
        if rows.len() < limit {
            let room = limit - rows.len();
            rows.extend(wrapped.into_iter().take(room));
        }
    }

    let capped = total > rows.len();
    (Rc::new(rows), total, capped)
}

fn same_style(a: &BlockRun, b: &BlockRun) -> bool {
    a.fg == b.fg
        && a.bg == b.bg
        && a.bold == b.bold
        && a.italic == b.italic
        && a.underline == b.underline
        && a.strikethrough == b.strikethrough
        && a.wavy == b.wavy
}

fn wrap_line(line: &terminal::BlockLine, cols: usize) -> Vec<WrappedRow> {
    let mut rows: Vec<WrappedRow> = Vec::new();
    let mut row: WrappedRow = Vec::new();
    let mut current: Option<PlacedRun> = None;
    let mut col = 0usize;

    for run in &line.runs {
        for ch in run.text.chars() {
            let width = ch.width().unwrap_or(0);
            if width == 0 {
                if let Some(cur) = current.as_mut() {
                    cur.run.text.push(ch);
                } else if let Some(last) = rows.last_mut().and_then(|row| row.last_mut()) {
                    last.run.text.push(ch);
                } else {
                    current = Some(PlacedRun {
                        run: BlockRun {
                            text: ch.to_string(),
                            ..run.clone()
                        },
                        start_col: col,
                        cells: 0,
                    });
                }
                continue;
            }

            if col + width > cols {
                if let Some(cur) = current.take() {
                    row.push(cur);
                }
                if !row.is_empty() {
                    rows.push(std::mem::take(&mut row));
                }
                col = 0;
            }

            match current.as_mut() {
                Some(cur) if same_style(&cur.run, run) => {
                    cur.run.text.push(ch);
                    cur.cells += width;
                }
                _ => {
                    if let Some(cur) = current.take() {
                        row.push(cur);
                    }
                    current = Some(PlacedRun {
                        run: BlockRun {
                            text: ch.to_string(),
                            ..run.clone()
                        },
                        start_col: col,
                        cells: width,
                    });
                }
            }
            col += width;
        }

        // Keep source runs as separate placed runs so styles never merge.
        if let Some(cur) = current.take() {
            row.push(cur);
        }
    }

    if let Some(cur) = current.take() {
        row.push(cur);
    }
    if !row.is_empty() || rows.is_empty() {
        rows.push(row);
    }
    rows
}

fn paint_rows(
    rows: &[WrappedRow],
    bounds: Bounds<Pixels>,
    style: &CardStyle,
    window: &mut Window,
    cx: &mut App,
) {
    let font_features = FontFeatures::disable_ligatures();
    for (row_index, row) in rows.iter().enumerate() {
        let y = bounds.origin.y + style.cell_height * (row_index as f32);
        for placed in row {
            let x = bounds.origin.x + style.cell_width * (placed.start_col as f32);
            if let Some(bg) = placed.run.bg {
                let rect = Bounds {
                    origin: Point { x, y },
                    size: size(style.cell_width * (placed.cells as f32), style.cell_height),
                };
                window.paint_quad(quad(
                    rect,
                    px(0.0),
                    bg,
                    Edges::<Pixels>::default(),
                    transparent_black(),
                    Default::default(),
                ));
            }

            let font = Font {
                family: style.mono_font.clone(),
                features: font_features.clone(),
                fallbacks: None,
                weight: if placed.run.bold {
                    FontWeight::BOLD
                } else {
                    FontWeight::NORMAL
                },
                style: if placed.run.italic {
                    FontStyle::Italic
                } else {
                    FontStyle::Normal
                },
            };
            let text_run = TextRun {
                len: placed.run.text.len(),
                font,
                color: placed.run.fg,
                background_color: None,
                underline: placed.run.underline.then_some(UnderlineStyle {
                    thickness: px(1.0),
                    color: Some(placed.run.fg),
                    wavy: placed.run.wavy,
                }),
                strikethrough: placed.run.strikethrough.then_some(StrikethroughStyle {
                    thickness: px(1.0),
                    color: Some(placed.run.fg),
                }),
            };
            let shaped = window.text_system().shape_line(
                placed.run.text.clone().into(),
                style.font_size,
                &[text_run],
                None,
            );
            let _ = shaped.paint(Point { x, y }, style.cell_height, window, cx);
        }
    }
}

fn action_button(
    label: &'static str,
    action: BlockAction,
    weak: WeakEntity<AppState>,
    pane_id: usize,
    block_id: u64,
    tokens: ThemeTokens,
) -> Stateful<gpui::Div> {
    div()
        .id((label, block_id as usize))
        .px(px(5.0))
        .py(px(2.0))
        .rounded(px(4.0))
        .text_size(px(9.5))
        .text_color(tokens.text_muted)
        .cursor(CursorStyle::PointingHand)
        .hover(|this| {
            this.bg(tokens.block_bg_hover)
                .text_color(tokens.text_primary)
        })
        .child(label)
        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
            cx.stop_propagation();
            let _ = weak.update(cx, |state, cx| {
                state.block_action(pane_id, block_id, action, window, cx);
            });
        })
}

pub fn render_card(
    card: &BlockCardData,
    style: &CardStyle,
    weak: &WeakEntity<AppState>,
    pane_id: usize,
    wrapped: &Rc<Vec<WrappedRow>>,
    total_rows: usize,
    display_capped: bool,
) -> AnyElement {
    let tokens = style.tokens;
    let block_id = card.id;
    let card_for_click = weak.clone();
    let status_color: Hsla = if card.status.is_failure() {
        tokens.block_failure
    } else if card.status == crate::blocks::BlockStatus::Success {
        tokens.block_success
    } else {
        tokens.text_muted
    };

    let output: AnyElement = if card.full_screen {
        div()
            .px(px(10.0))
            .pb(px(8.0))
            .text_size(px(11.0))
            .text_color(tokens.text_muted)
            .child("Full-screen application — output is not captured")
            .into_any_element()
    } else if card.snapshot_pending {
        div()
            .px(px(10.0))
            .pb(px(8.0))
            .text_size(px(11.0))
            .text_color(tokens.text_muted)
            .child("Rendering output…")
            .into_any_element()
    } else if wrapped.is_empty() {
        div().into_any_element()
    } else {
        let rows = Rc::clone(wrapped);
        let paint_style = style.clone();
        let height = px(f32::from(style.cell_height) * (rows.len() as f32));
        div()
            .px(px(10.0))
            .pt(px(6.0))
            .pb(px(8.0))
            .w_full()
            .child(
                gpui::canvas(
                    move |bounds, _window, _cx| bounds,
                    move |bounds, _, window, cx| {
                        paint_rows(&rows, bounds, &paint_style, window, cx);
                    },
                )
                .h(height)
                .w_full(),
            )
            .into_any_element()
    };

    let mut footer = format!(
        "{} lines",
        if display_capped {
            format!("{} / {}", wrapped.len(), total_rows)
        } else {
            total_rows.to_string()
        }
    );
    if let Some(cwd) = &card.cwd {
        footer.push_str(" · ");
        footer.push_str(cwd);
    }
    if card.truncated {
        footer.push_str(" · capture truncated");
    }

    let bookmark = if card.bookmarked { "★" } else { "☆" };
    let fold = if card.collapsed { "unfold" } else { "fold" };
    let show_full = if card.show_full_output {
        "less"
    } else {
        "more"
    };

    div()
        .id(("block-card", block_id as usize))
        .w_full()
        .flex()
        .flex_col()
        .rounded(px(8.0))
        .border_1()
        .border_color(if card.selected {
            tokens.block_border_selected
        } else {
            tokens.card_border
        })
        .bg(tokens.block_bg)
        .hover(|this| this.bg(tokens.block_bg_hover))
        .when(cfg!(test) && card.index == 0, |this| {
            this.child(
                div()
                    .id("kathamo-block-card-probe")
                    .debug_selector(|| "kathamo-block-card-probe".to_string())
                    .w_full()
                    .h(px(4.0)),
            )
        })
        .overflow_hidden()
        .on_mouse_down(MouseButton::Left, move |_, _, cx| {
            let _ = card_for_click.update(cx, |state, cx| {
                state.select_block(pane_id, block_id, cx);
            });
        })
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .px(px(10.0))
                .py(px(8.0))
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(if card.status.is_failure() {
                            tokens.block_failure
                        } else {
                            tokens.block_success
                        })
                        .child("❯"),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_family(style.mono_font.clone())
                        .text_size(px(13.0))
                        .text_color(tokens.text_primary)
                        .child(if card.command.is_empty() {
                            "(no command)".to_string()
                        } else {
                            card.command.clone()
                        }),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .text_size(px(10.5))
                        .child(div().text_color(status_color).child(card.status.label()))
                        .child(
                            div()
                                .text_color(tokens.text_muted)
                                .child(format!("{:.2}s", card.duration.as_secs_f64())),
                        ),
                ),
        )
        .when(!card.collapsed, |this| {
            this.child(div().h(px(1.0)).w_full().bg(tokens.block_separator))
                .child(output)
        })
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(8.0))
                .px(px(10.0))
                .py(px(6.0))
                .bg(tokens.block_status_bg)
                .border_t_1()
                .border_color(tokens.block_separator)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(10.0))
                        .text_color(tokens.text_muted)
                        .child(footer),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(1.0))
                        .child(action_button(
                            "copy",
                            BlockAction::CopyCommand,
                            weak.clone(),
                            pane_id,
                            block_id,
                            tokens,
                        ))
                        .child(action_button(
                            "out",
                            BlockAction::CopyOutput,
                            weak.clone(),
                            pane_id,
                            block_id,
                            tokens,
                        ))
                        .child(action_button(
                            "run",
                            BlockAction::Rerun,
                            weak.clone(),
                            pane_id,
                            block_id,
                            tokens,
                        ))
                        .child(action_button(
                            fold,
                            BlockAction::ToggleCollapse,
                            weak.clone(),
                            pane_id,
                            block_id,
                            tokens,
                        ))
                        .child(action_button(
                            bookmark,
                            BlockAction::ToggleBookmark,
                            weak.clone(),
                            pane_id,
                            block_id,
                            tokens,
                        ))
                        .when(display_capped, |this| {
                            this.child(action_button(
                                show_full,
                                BlockAction::ShowFullOutput,
                                weak.clone(),
                                pane_id,
                                block_id,
                                tokens,
                            ))
                        })
                        .child(action_button(
                            "md",
                            BlockAction::CopyMarkdown,
                            weak.clone(),
                            pane_id,
                            block_id,
                            tokens,
                        )),
                ),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use terminal::{BlockLine, BlockSnapshot};

    fn run(text: &str) -> BlockRun {
        BlockRun {
            text: text.to_string(),
            fg: gpui::black(),
            bg: None,
            bold: false,
            italic: false,
            underline: false,
            strikethrough: false,
            wavy: false,
        }
    }

    fn snapshot(text: &str) -> BlockSnapshot {
        BlockSnapshot {
            lines: text
                .split('\n')
                .map(|line| BlockLine {
                    runs: if line.is_empty() {
                        Vec::new()
                    } else {
                        vec![run(line)]
                    },
                    cells: line.chars().count(),
                })
                .collect(),
            truncated: false,
        }
    }

    #[test]
    fn wraps_at_the_column_limit() {
        let snapshot = snapshot("abcdefghij");
        let (rows, total, capped) = wrap_snapshot(&snapshot, 5, 10);
        assert_eq!(total, 2);
        assert!(!capped);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0].run.text, "abcde");
        assert_eq!(rows[1][0].run.text, "fghij");
    }

    #[test]
    fn wraps_wide_characters_without_splitting_them() {
        let snapshot = snapshot("a界b");
        // 1 + 2 cells exactly fills the first row; `b` moves to the next.
        let (rows, total, _) = wrap_snapshot(&snapshot, 3, 10);
        assert_eq!(total, 2);
        assert_eq!(rows[0][0].run.text, "a界");
        assert_eq!(rows[0][0].cells, 3);
        assert_eq!(rows[1][0].run.text, "b");
    }

    #[test]
    fn preserves_blank_lines_and_caps_the_display() {
        let snapshot = snapshot("a\n\nb\nc");
        let (rows, total, capped) = wrap_snapshot(&snapshot, 10, 3);
        assert_eq!(total, 4);
        assert!(capped);
        assert_eq!(rows.len(), 3);
        assert!(rows[1].is_empty());
    }

    #[test]
    fn empty_snapshot_has_no_rows() {
        let (rows, total, capped) = wrap_snapshot(&BlockSnapshot::default(), 10, 10);
        assert!(rows.is_empty());
        assert_eq!(total, 0);
        assert!(!capped);
    }
}
