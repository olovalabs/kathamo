# Warp Parity Plan — Terminal Preview

Goal: make Kathamo's terminal preview look and behave as close to
[Warp](https://github.com/warpdotdev/warp) as possible, fix the bugs in the
block/terminal path, and ship it as a pull request.

Design tokens below were extracted directly from Warp's open-source repo
(`app/src/themes/default_themes.rs`, `crates/warp_core/src/ui/theme/color.rs`,
`app/src/terminal/settings.rs`, `app/src/terminal/block_list_element.rs`).

## Warp's actual design (source of truth)

- **Default Dark theme**: background `#050505`, foreground `#FFFFFF`,
  accent/cursor `#19AAD8`. ANSI palette (normal): black `#616161`, red
  `#FF8272`, green `#B4FA72`, yellow `#FEFDC2`, blue `#A5D5FE`, magenta
  `#FF8FFD`, cyan `#D0D1FE`, white `#F1F1F1`; bright variants as upstream.
- **Default Light theme**: background `#FFFFFF`, foreground `#111111`,
  accent `#00C2FF`.
- **Blocks**: full-width, separated by 1px dividers (`outline` = foreground at
  ~4% alpha). Failed blocks get a red left "flag pole" + red tint (~4% alpha).
  Selected blocks get an accent tint (~10% alpha) + accent left border.
- **Prompt header** above each command: working dir (magenta) + git branch
  (yellow/green), then the command on its own line.
- **Hover actions** appear top-right on hover (copy, re-run, bookmark, …),
  not an always-visible footer.
- **Spacing** (normal): block padding top ≈1.1 cells, command padding ≈0.19,
  middle ≈0.5, bottom ≈1.0.

## Work items

1. **Warp theme** — add `load_warp_palette` (exact ANSI + bg/fg/cursor) and make
   Warp the default terminal palette; update block/pane UI tokens to Warp's
   derived colors. Keep GitHub palette available.
2. **Warp-style block cards** — prompt header (cwd + git branch + duration/exit),
   syntax-highlighted command with `❯`, output below, hover-revealed action
   buttons, red flag-pole for failures, accent selection, dividers, Warp spacing.
3. **Warp-style input bar** — prompt line (cwd + git) above a syntax-highlighted
   auto-growing editor, Warp colors.
4. **Bug fixes** in the block/terminal path (identified during review).
5. **Tests** for new logic; keep the existing suite green.
6. **Verify via CI** (no local Rust toolchain / crates.io in this sandbox) and
   open the PR.

## Constraint

This sandbox has no Rust toolchain and no crates.io access, so compilation is
verified through the repository's CI (`cargo check`, `clippy`, `fmt`, tests)
after each push. All GPUI APIs used are checked against the vendored
`components/gpui` + `components/src` source.
