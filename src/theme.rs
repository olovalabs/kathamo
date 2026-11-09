use gpui::{App, Hsla, Window, rgba};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use terminal::ColorPalette;

/// Apply one appearance to the app chrome and generated chat components.
pub(crate) fn apply_component_theme(
    mode: gpui_component::ThemeMode,
    window: Option<&mut Window>,
    cx: &mut App,
) {
    gpui_component::Theme::change(mode, window, cx);
    let kit_mode = if mode.is_dark() {
        gpui_kit::component::ThemeMode::Dark
    } else {
        gpui_kit::component::ThemeMode::Light
    };
    gpui_kit::component::Theme::change(kit_mode, None, cx);
}

/// Which terminal color palette / block chrome to use.
///
/// `Warp` reproduces the default theme of the Warp terminal (near-black
/// background, white foreground, cyan accent and Warp's ANSI palette) and is
/// the default. `GitHub` keeps the previous GitHub Dark / Light palette.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PaletteScheme {
    #[default]
    Warp,
    Github,
}

impl PaletteScheme {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Warp => "Warp",
            Self::Github => "GitHub",
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            Self::Warp => "Warp's default dark terminal",
            Self::Github => "GitHub Dark / Light palette",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ThemePreference {
    #[default]
    System,
    Light,
    Dark,
}

impl ThemePreference {
    pub fn label(&self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }

    #[allow(dead_code)]
    pub fn description(&self) -> &'static str {
        match self {
            Self::System => "Automatically match system appearance",
            Self::Light => "Clean, crisp light theme",
            Self::Dark => "Sleek high-contrast dark theme",
        }
    }

    pub fn resolve_mode(&self, appearance: gpui::WindowAppearance) -> gpui_component::ThemeMode {
        match self {
            Self::System => appearance.into(),
            Self::Light => gpui_component::ThemeMode::Light,
            Self::Dark => gpui_component::ThemeMode::Dark,
        }
    }
}

#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
pub struct ThemeTokens {
    pub is_dark: bool,

    // Window & background
    pub window_bg: Hsla,
    pub text_primary: Hsla,
    pub text_secondary: Hsla,
    pub text_muted: Hsla,
    pub border: Hsla,
    pub border_subtle: Hsla,

    // Titlebar
    pub titlebar_bg: Hsla,
    pub titlebar_border: Hsla,
    pub titlebar_fg: Hsla,
    pub titlebar_btn_hover: Hsla,

    // Sidebar
    pub sidebar_bg: Hsla,
    pub sidebar_bg_start: Hsla,
    pub sidebar_bg_end: Hsla,
    pub sidebar_border: Hsla,
    pub sidebar_fg: Hsla,
    pub sidebar_muted: Hsla,
    pub sidebar_item_hover: Hsla,
    pub sidebar_item_active: Hsla,
    pub sidebar_item_active_fg: Hsla,
    pub sidebar_section_header: Hsla,
    pub sidebar_icon_btn_hover: Hsla,
    pub sidebar_icon: Hsla,
    pub sidebar_icon_hover: Hsla,
    pub sidebar_footer_bg: Hsla,
    pub sidebar_footer_border: Hsla,

    // Terminal Panes
    pub pane_bg: Hsla,
    pub pane_border: Hsla,
    pub pane_border_focused: Hsla,
    pub pane_header_bg: Hsla,
    pub pane_header_fg: Hsla,
    pub pane_header_btn_hover: Hsla,
    pub pane_header_icon: Hsla,
    pub pane_header_icon_hover: Hsla,

    // Controls & Buttons
    pub primary_btn_bg: Hsla,
    pub primary_btn_hover: Hsla,
    pub primary_btn_fg: Hsla,
    pub secondary_btn_bg: Hsla,
    pub secondary_btn_hover: Hsla,
    pub secondary_btn_fg: Hsla,

    // Cards & Panels (Settings page & Dialogs)
    pub card_bg: Hsla,
    pub card_border: Hsla,
    pub card_hover: Hsla,
    pub pill_active_bg: Hsla,
    pub pill_active_fg: Hsla,
    pub pill_inactive_bg: Hsla,
    pub pill_inactive_fg: Hsla,
    pub ring: Hsla,

    // Command blocks (Warp style)
    pub block_bg: Hsla,
    pub block_bg_hover: Hsla,
    pub block_border: Hsla,
    pub block_border_selected: Hsla,
    pub block_success: Hsla,
    pub block_failure: Hsla,
    pub block_warning: Hsla,
    pub block_separator: Hsla,
    pub block_status_bg: Hsla,
    pub editor_bar_bg: Hsla,
    pub editor_bar_border: Hsla,
}

impl ThemeTokens {
    /// Build tokens for a mode and palette scheme. The Warp scheme reproduces
    /// Warp's default terminal chrome: near-black surfaces, white text, a cyan
    /// accent and block colors derived from Warp's ANSI palette.
    pub fn from_mode_with_scheme(mode: gpui_component::ThemeMode, scheme: PaletteScheme) -> Self {
        let mut tokens = if mode.is_dark() {
            Self::dark()
        } else {
            Self::light()
        };
        if scheme == PaletteScheme::Warp {
            tokens.apply_warp(mode.is_dark());
        }
        tokens
    }

    /// Override surfaces, accents and block chrome with Warp's default theme.
    ///
    /// Colors come from Warp's `default_themes.rs` (background `#050505`,
    /// foreground `#FFFFFF`, accent `#19AAD8`) and its derived UI colors
    /// (`warp_core::ui::theme::color`): surfaces are the background blended
    /// with low-opacity foreground, outlines are foreground at ~4% alpha, and
    /// block selection is the accent at ~10% alpha.
    fn apply_warp(&mut self, dark: bool) {
        if dark {
            let bg = rgba(0x050505ff);
            let fg = rgba(0xffffffff);
            let accent = rgba(0x19aad8ff);
            // neutral_1 / neutral_2: background blended with foreground at
            // ~2% / ~4% alpha (Warp `internal_colors::neutral_1/2`).
            let surface_1 = rgba(0x0a0a0aff);
            let surface_2 = rgba(0x0f0f0fff);
            let surface_3 = rgba(0x141414ff);
            // outline: foreground at ~4% alpha (Warp `fg_overlay_2`).
            let outline = rgba(0xffffff1a);
            let text_sub = rgba(0xffffffb3);
            let text_disabled = rgba(0xffffff66);

            self.window_bg = bg.into();
            self.text_primary = fg.into();
            self.text_secondary = text_sub.into();
            self.text_muted = text_disabled.into();
            self.border = outline.into();
            self.border_subtle = rgba(0xffffff0d).into();

            self.titlebar_bg = bg.into();
            self.titlebar_border = rgba(0xffffff0d).into();
            self.titlebar_fg = text_sub.into();
            self.titlebar_btn_hover = surface_2.into();

            self.sidebar_bg = bg.into();
            self.sidebar_bg_start = bg.into();
            self.sidebar_bg_end = bg.into();
            self.sidebar_border = rgba(0xffffff0d).into();
            self.sidebar_fg = fg.into();
            self.sidebar_muted = text_disabled.into();
            self.sidebar_item_hover = rgba(0xffffff0d).into();
            self.sidebar_item_active = rgba(0xffffff1a).into();
            self.sidebar_item_active_fg = fg.into();
            self.sidebar_section_header = text_disabled.into();
            self.sidebar_icon_btn_hover = rgba(0xffffff1a).into();
            self.sidebar_icon = text_disabled.into();
            self.sidebar_icon_hover = fg.into();
            self.sidebar_footer_bg = bg.into();
            self.sidebar_footer_border = rgba(0xffffff0d).into();

            self.pane_bg = bg.into();
            self.pane_border = rgba(0xffffff0d).into();
            self.pane_border_focused = rgba(0x19aad866).into();
            self.pane_header_bg = bg.into();
            self.pane_header_fg = text_disabled.into();
            self.pane_header_btn_hover = surface_2.into();
            self.pane_header_icon = text_disabled.into();
            self.pane_header_icon_hover = fg.into();

            self.primary_btn_bg = accent.into();
            self.primary_btn_hover = rgba(0x3fb9e4ff).into();
            self.primary_btn_fg = rgba(0x050505ff).into();
            self.secondary_btn_bg = surface_2.into();
            self.secondary_btn_hover = surface_3.into();
            self.secondary_btn_fg = fg.into();

            self.card_bg = surface_1.into();
            self.card_border = outline.into();
            self.card_hover = surface_2.into();
            self.pill_active_bg = accent.into();
            self.pill_active_fg = rgba(0x050505ff).into();
            self.pill_inactive_bg = surface_2.into();
            self.pill_inactive_fg = text_sub.into();
            self.ring = accent.into();

            // Warp blocks sit on the terminal background, delineated by
            // dividers rather than card fills. Selection is the accent at
            // ~10% alpha; failure is Warp's ANSI red.
            self.block_bg = rgba(0x00000000).into();
            self.block_bg_hover = rgba(0xffffff08).into();
            self.block_border = outline.into();
            self.block_border_selected = accent.into();
            self.block_success = rgba(0xb4fa72ff).into();
            self.block_failure = rgba(0xff8272ff).into();
            self.block_warning = rgba(0xfefdc2ff).into();
            self.block_separator = outline.into();
            self.block_status_bg = rgba(0x00000000).into();
            self.editor_bar_bg = bg.into();
            self.editor_bar_border = outline.into();
        } else {
            let bg = rgba(0xffffffff);
            let fg = rgba(0x111111ff);
            let accent = rgba(0x00c2ffff);
            let surface_1 = rgba(0xf7f7f7ff);
            let surface_2 = rgba(0xefefefff);
            let surface_3 = rgba(0xe7e7e7ff);
            let outline = rgba(0x1111111a);
            let text_sub = rgba(0x111111b3);
            let text_disabled = rgba(0x11111166);

            self.window_bg = bg.into();
            self.text_primary = fg.into();
            self.text_secondary = text_sub.into();
            self.text_muted = text_disabled.into();
            self.border = outline.into();
            self.border_subtle = rgba(0x1111110d).into();

            self.titlebar_bg = bg.into();
            self.titlebar_border = rgba(0x1111110d).into();
            self.titlebar_fg = text_sub.into();
            self.titlebar_btn_hover = surface_2.into();

            self.sidebar_bg = bg.into();
            self.sidebar_bg_start = bg.into();
            self.sidebar_bg_end = bg.into();
            self.sidebar_border = rgba(0x1111110d).into();
            self.sidebar_fg = fg.into();
            self.sidebar_muted = text_disabled.into();
            self.sidebar_item_hover = rgba(0x1111110d).into();
            self.sidebar_item_active = rgba(0x1111111a).into();
            self.sidebar_item_active_fg = fg.into();
            self.sidebar_section_header = text_disabled.into();
            self.sidebar_icon_btn_hover = rgba(0x1111111a).into();
            self.sidebar_icon = text_disabled.into();
            self.sidebar_icon_hover = fg.into();
            self.sidebar_footer_bg = bg.into();
            self.sidebar_footer_border = rgba(0x1111110d).into();

            self.pane_bg = bg.into();
            self.pane_border = rgba(0x1111110d).into();
            self.pane_border_focused = rgba(0x00c2ff66).into();
            self.pane_header_bg = bg.into();
            self.pane_header_fg = text_disabled.into();
            self.pane_header_btn_hover = surface_2.into();
            self.pane_header_icon = text_disabled.into();
            self.pane_header_icon_hover = fg.into();

            self.primary_btn_bg = accent.into();
            self.primary_btn_hover = rgba(0x00a8e0ff).into();
            self.primary_btn_fg = rgba(0xffffffff).into();
            self.secondary_btn_bg = surface_2.into();
            self.secondary_btn_hover = surface_3.into();
            self.secondary_btn_fg = fg.into();

            self.card_bg = surface_1.into();
            self.card_border = outline.into();
            self.card_hover = surface_2.into();
            self.pill_active_bg = accent.into();
            self.pill_active_fg = rgba(0xffffffff).into();
            self.pill_inactive_bg = surface_2.into();
            self.pill_inactive_fg = text_sub.into();
            self.ring = accent.into();

            self.block_bg = rgba(0x00000000).into();
            self.block_bg_hover = rgba(0x11111108).into();
            self.block_border = outline.into();
            self.block_border_selected = accent.into();
            self.block_success = rgba(0x10a778ff).into();
            self.block_failure = rgba(0xc30771ff).into();
            self.block_warning = rgba(0xa89c14ff).into();
            self.block_separator = outline.into();
            self.block_status_bg = rgba(0x00000000).into();
            self.editor_bar_bg = bg.into();
            self.editor_bar_border = outline.into();
        }
    }

    pub fn dark() -> Self {
        Self {
            is_dark: true,
            window_bg: rgba(0x14151bff).into(),
            text_primary: rgba(0xe6edf3ff).into(),
            text_secondary: rgba(0x8788a5ff).into(),
            text_muted: rgba(0x625b69ff).into(),
            border: rgba(0x22232cff).into(),
            border_subtle: rgba(0x20212aff).into(),

            titlebar_bg: rgba(0x14151bff).into(),
            titlebar_border: rgba(0x20212aff).into(),
            titlebar_fg: rgba(0x8788a5ff).into(),
            titlebar_btn_hover: rgba(0x23242cff).into(),

            sidebar_bg: rgba(0x121319ff).into(),
            sidebar_bg_start: rgba(0x121319ff).into(),
            sidebar_bg_end: rgba(0x121319ff).into(),
            sidebar_border: rgba(0x242531ff).into(),
            sidebar_fg: rgba(0xe6edf3ff).into(),
            sidebar_muted: rgba(0x8b879eff).into(),
            sidebar_item_hover: rgba(0xffffff15).into(),
            sidebar_item_active: rgba(0xffffff22).into(),
            sidebar_item_active_fg: rgba(0xffffffff).into(),
            sidebar_section_header: rgba(0x9a95adff).into(),
            sidebar_icon_btn_hover: rgba(0xffffff1a).into(),
            sidebar_icon: rgba(0x9a95adff).into(),
            sidebar_icon_hover: rgba(0xffffffff).into(),
            sidebar_footer_bg: rgba(0x13141aff).into(),
            sidebar_footer_border: rgba(0x242531ff).into(),

            pane_bg: rgba(0x0c0d12ff).into(),
            pane_border: rgba(0x22232cff).into(),
            pane_border_focused: rgba(0x38394eff).into(),
            pane_header_bg: rgba(0x0c0d12ff).into(),
            pane_header_fg: rgba(0x918995ff).into(),
            pane_header_btn_hover: rgba(0x23242cff).into(),
            pane_header_icon: rgba(0x817987ff).into(),
            pane_header_icon_hover: rgba(0xf0f6fcff).into(),

            primary_btn_bg: rgba(0x1f6febff).into(),
            primary_btn_hover: rgba(0x2f81f7ff).into(),
            primary_btn_fg: rgba(0xffffffff).into(),
            secondary_btn_bg: rgba(0x26272fff).into(),
            secondary_btn_hover: rgba(0x2d2e38ff).into(),
            secondary_btn_fg: rgba(0xffffffff).into(),

            card_bg: rgba(0x1a1b24ff).into(),
            card_border: rgba(0x2b2c3cff).into(),
            card_hover: rgba(0x222330ff).into(),
            pill_active_bg: rgba(0x1f6febff).into(),
            pill_active_fg: rgba(0xffffffff).into(),
            pill_inactive_bg: rgba(0x23242cff).into(),
            pill_inactive_fg: rgba(0x918995ff).into(),
            ring: rgba(0x388bfdff).into(),

            block_bg: rgba(0x11151aff).into(),
            block_bg_hover: rgba(0x151a21ff).into(),
            block_border: rgba(0x1f242bff).into(),
            block_border_selected: rgba(0x4c9aedff).into(),
            block_success: rgba(0x3fb950ff).into(),
            block_failure: rgba(0xf85149ff).into(),
            block_warning: rgba(0xd29922ff).into(),
            block_separator: rgba(0x1b2027ff).into(),
            block_status_bg: rgba(0x0d1116ff).into(),
            editor_bar_bg: rgba(0x0d1014ff).into(),
            editor_bar_border: rgba(0x1f242bff).into(),
        }
    }

    pub fn light() -> Self {
        Self {
            is_dark: false,
            window_bg: rgba(0xf6f8faff).into(),
            text_primary: rgba(0x1f2328ff).into(),
            text_secondary: rgba(0x59636eff).into(),
            text_muted: rgba(0x746d7aff).into(),
            border: rgba(0xd1d9e0ff).into(),
            border_subtle: rgba(0xe1e4e8ff).into(),

            titlebar_bg: rgba(0xffffffff).into(),
            titlebar_border: rgba(0xd1d9e0ff).into(),
            titlebar_fg: rgba(0x1f2328ff).into(),
            titlebar_btn_hover: rgba(0xeaeef2ff).into(),

            sidebar_bg: rgba(0xf6f8faff).into(),
            sidebar_bg_start: rgba(0xf6f8faff).into(),
            sidebar_bg_end: rgba(0xf6f8faff).into(),
            sidebar_border: rgba(0xd1d9e0ff).into(),
            sidebar_fg: rgba(0x1f2328ff).into(),
            sidebar_muted: rgba(0x656d76ff).into(),
            sidebar_item_hover: rgba(0x0000000a).into(),
            sidebar_item_active: rgba(0x00000014).into(),
            sidebar_item_active_fg: rgba(0x1f2328ff).into(),
            sidebar_section_header: rgba(0x656d76ff).into(),
            sidebar_icon_btn_hover: rgba(0x0000000e).into(),
            sidebar_icon: rgba(0x656d76ff).into(),
            sidebar_icon_hover: rgba(0x1f2328ff).into(),
            sidebar_footer_bg: rgba(0xf0f2f5ff).into(),
            sidebar_footer_border: rgba(0xd1d9e0ff).into(),

            pane_bg: rgba(0xffffffff).into(),
            pane_border: rgba(0xd1d9e0ff).into(),
            pane_border_focused: rgba(0x0969daff).into(),
            pane_header_bg: rgba(0xf6f8faff).into(),
            pane_header_fg: rgba(0x59636eff).into(),
            pane_header_btn_hover: rgba(0xeaeef2ff).into(),
            pane_header_icon: rgba(0x59636eff).into(),
            pane_header_icon_hover: rgba(0x1f2328ff).into(),

            primary_btn_bg: rgba(0x0969daff).into(),
            primary_btn_hover: rgba(0x0854b0ff).into(),
            primary_btn_fg: rgba(0xffffffff).into(),
            secondary_btn_bg: rgba(0xeaeef2ff).into(),
            secondary_btn_hover: rgba(0xdfe4e8ff).into(),
            secondary_btn_fg: rgba(0x1f2328ff).into(),

            card_bg: rgba(0xffffffff).into(),
            card_border: rgba(0xd1d9e0ff).into(),
            card_hover: rgba(0xf6f8faff).into(),
            pill_active_bg: rgba(0x0969daff).into(),
            pill_active_fg: rgba(0xffffffff).into(),
            pill_inactive_bg: rgba(0xeaeef2ff).into(),
            pill_inactive_fg: rgba(0x59636eff).into(),
            ring: rgba(0x0969daff).into(),

            block_bg: rgba(0xffffffff).into(),
            block_bg_hover: rgba(0xf6f8faff).into(),
            block_border: rgba(0xd1d9e0ff).into(),
            block_border_selected: rgba(0x0969daff).into(),
            block_success: rgba(0x1a7f37ff).into(),
            block_failure: rgba(0xcf222eff).into(),
            block_warning: rgba(0x9a6700ff).into(),
            block_separator: rgba(0xeaeef2ff).into(),
            block_status_bg: rgba(0xf6f8faff).into(),
            editor_bar_bg: rgba(0xffffffff).into(),
            editor_bar_border: rgba(0xd1d9e0ff).into(),
        }
    }
}

pub fn parse_hex(s: &str) -> Option<u32> {
    let hex = s.strip_prefix('#')?;
    if hex.len() == 6 {
        u32::from_str_radix(&format!("{hex}ff"), 16).ok()
    } else if hex.len() == 8 {
        u32::from_str_radix(hex, 16).ok()
    } else {
        None
    }
}

pub fn hex_to_rgb(hex: u32) -> (u8, u8, u8) {
    (
        ((hex >> 24) & 0xff) as u8,
        ((hex >> 16) & 0xff) as u8,
        ((hex >> 8) & 0xff) as u8,
    )
}

pub fn load_github_palette(dark: bool) -> ColorPalette {
    let theme_data = crate::assets::AppAssets::get("themes/github.json");
    let target_name = if dark { "GitHub Dark" } else { "GitHub Light" };
    if let Some(file) = theme_data
        && let Ok(Value::Array(themes)) = serde_json::from_slice::<Value>(&file.data)
        && let Some(target_theme) = themes
            .iter()
            .find(|t| t.get("name").and_then(Value::as_str) == Some(target_name))
    {
        let mut builder = ColorPalette::builder();
        let style = target_theme.get("style").and_then(Value::as_object);

        let get_color = |key: &str| -> Option<(u8, u8, u8)> {
            style
                .and_then(|s| s.get(key))
                .and_then(Value::as_str)
                .and_then(parse_hex)
                .map(hex_to_rgb)
        };

        if let Some((r, g, b)) = get_color("terminal.background")
            .or_else(|| get_color("editor.background"))
            .or_else(|| get_color("background"))
        {
            builder = builder.background(r, g, b);
        }

        if let Some((r, g, b)) = get_color("terminal.foreground")
            .or_else(|| get_color("editor.foreground"))
            .or_else(|| get_color("text"))
        {
            builder = builder.foreground(r, g, b);
        }

        let cursor_color = target_theme
            .get("players")
            .and_then(Value::as_array)
            .and_then(|p| p.first())
            .and_then(|p| p.get("cursor"))
            .and_then(Value::as_str)
            .and_then(parse_hex)
            .map(hex_to_rgb)
            .or_else(|| get_color("text.accent"))
            .or_else(|| get_color("terminal.bright_foreground"))
            .or_else(|| get_color("terminal.foreground"));

        if let Some((r, g, b)) = cursor_color {
            builder = builder.cursor(r, g, b);
        }

        let ansi_keys = [
            ("terminal.ansi.black", 0),
            ("terminal.ansi.red", 1),
            ("terminal.ansi.green", 2),
            ("terminal.ansi.yellow", 3),
            ("terminal.ansi.blue", 4),
            ("terminal.ansi.magenta", 5),
            ("terminal.ansi.cyan", 6),
            ("terminal.ansi.white", 7),
            ("terminal.ansi.bright_black", 8),
            ("terminal.ansi.bright_red", 9),
            ("terminal.ansi.bright_green", 10),
            ("terminal.ansi.bright_yellow", 11),
            ("terminal.ansi.bright_blue", 12),
            ("terminal.ansi.bright_magenta", 13),
            ("terminal.ansi.bright_cyan", 14),
            ("terminal.ansi.bright_white", 15),
        ];

        for (key, idx) in ansi_keys {
            if let Some((r, g, b)) = get_color(key) {
                builder = match idx {
                    0 => builder.black(r, g, b),
                    1 => builder.red(r, g, b),
                    2 => builder.green(r, g, b),
                    3 => builder.yellow(r, g, b),
                    4 => builder.blue(r, g, b),
                    5 => builder.magenta(r, g, b),
                    6 => builder.cyan(r, g, b),
                    7 => builder.white(r, g, b),
                    8 => builder.bright_black(r, g, b),
                    9 => builder.bright_red(r, g, b),
                    10 => builder.bright_green(r, g, b),
                    11 => builder.bright_yellow(r, g, b),
                    12 => builder.bright_blue(r, g, b),
                    13 => builder.bright_magenta(r, g, b),
                    14 => builder.bright_cyan(r, g, b),
                    15 => builder.bright_white(r, g, b),
                    _ => builder,
                };
            }
        }

        return builder.build();
    }

    if dark {
        ColorPalette::builder()
            .background(0x01, 0x04, 0x09)
            .foreground(0xf0, 0xf6, 0xfc)
            .cursor(0x44, 0x93, 0xf8)
            .build()
    } else {
        ColorPalette::builder()
            .background(0xff, 0xff, 0xff)
            .foreground(0x1f, 0x23, 0x28)
            .cursor(0x09, 0x69, 0xda)
            .build()
    }
}

/// Load the terminal palette for a scheme and light/dark mode.
pub fn load_palette(scheme: PaletteScheme, dark: bool) -> ColorPalette {
    match scheme {
        PaletteScheme::Warp => load_warp_palette(dark),
        PaletteScheme::Github => load_github_palette(dark),
    }
}

/// Warp's default terminal palette, transcribed from the open-source Warp
/// repository (`app/src/themes/default_themes.rs`).
///
/// Dark: background `#050505`, foreground `#FFFFFF`, cursor/accent `#19AAD8`.
/// Light: background `#FFFFFF`, foreground `#111111`, accent `#00C2FF`.
/// The 16 ANSI colors match Warp's `DARK_MODE_*` / `LIGHT_MODE_*` constants.
pub fn load_warp_palette(dark: bool) -> ColorPalette {
    let builder = ColorPalette::builder();
    if dark {
        builder
            .background(0x05, 0x05, 0x05)
            .foreground(0xff, 0xff, 0xff)
            .cursor(0x19, 0xaa, 0xd8)
            // normal
            .black(0x61, 0x61, 0x61)
            .red(0xff, 0x82, 0x72)
            .green(0xb4, 0xfa, 0x72)
            .yellow(0xfe, 0xfd, 0xc2)
            .blue(0xa5, 0xd5, 0xfe)
            .magenta(0xff, 0x8f, 0xfd)
            .cyan(0xd0, 0xd1, 0xfe)
            .white(0xf1, 0xf1, 0xf1)
            // bright
            .bright_black(0x8e, 0x8e, 0x8e)
            .bright_red(0xff, 0xc4, 0xbd)
            .bright_green(0xd6, 0xfc, 0xb9)
            .bright_yellow(0xfe, 0xfd, 0xd5)
            .bright_blue(0xc1, 0xe3, 0xfe)
            .bright_magenta(0xff, 0xb1, 0xfe)
            .bright_cyan(0xe5, 0xe6, 0xfe)
            .bright_white(0xfe, 0xff, 0xff)
            .build()
    } else {
        builder
            .background(0xff, 0xff, 0xff)
            .foreground(0x11, 0x11, 0x11)
            .cursor(0x00, 0xc2, 0xff)
            // normal
            .black(0x21, 0x21, 0x21)
            .red(0xc3, 0x07, 0x71)
            .green(0x10, 0xa7, 0x78)
            .yellow(0xa8, 0x9c, 0x14)
            .blue(0x00, 0x8e, 0xc4)
            .magenta(0x52, 0x3c, 0x79)
            .cyan(0x20, 0xa5, 0xba)
            .white(0xe0, 0xe0, 0xe0)
            // bright
            .bright_black(0x21, 0x21, 0x21)
            .bright_red(0xfb, 0x00, 0x7a)
            .bright_green(0x5f, 0xd7, 0xaf)
            .bright_yellow(0xf3, 0xe4, 0x30)
            .bright_blue(0x20, 0xbb, 0xfc)
            .bright_magenta(0x68, 0x55, 0xde)
            .bright_cyan(0x4f, 0xb8, 0xcc)
            .bright_white(0xf1, 0xf1, 0xf1)
            .build()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_theme_preference_default_is_system() {
        assert_eq!(ThemePreference::default(), ThemePreference::System);
    }

    #[test]
    fn test_theme_tokens_modes() {
        let dark = ThemeTokens::dark();
        assert!(dark.is_dark);
        assert!(dark.sidebar_bg.l < 0.2);

        let light = ThemeTokens::light();
        assert!(!light.is_dark);
        assert!(
            light.sidebar_bg.l > 0.8,
            "Light sidebar should be directly light like other places"
        );
        assert_eq!(light.sidebar_bg, light.window_bg);
    }

    #[test]
    fn test_load_github_palette_dark_and_light() {
        let dark_palette = load_github_palette(true);
        let light_palette = load_github_palette(false);
        // Dark background should be very dark, light background very light
        assert!(dark_palette.background().l < 0.2);
        assert!(light_palette.background().l > 0.8);
    }

    #[test]
    fn warp_palette_matches_upstream_colors() {
        let dark = load_warp_palette(true);
        // Warp dark background is #050505, foreground #FFFFFF.
        assert!(dark.background().l < 0.05);
        assert!(dark.foreground().l > 0.95);
        // ANSI red is Warp's #FF8272 and green #B4FA72.
        let ansi = dark.ansi_colors();
        let red = terminal::hsla_to_rgb(ansi[1]);
        assert_eq!((red.r, red.g, red.b), (0xff, 0x82, 0x72));
        let green = terminal::hsla_to_rgb(ansi[2]);
        assert_eq!((green.r, green.g, green.b), (0xb4, 0xfa, 0x72));

        let light = load_warp_palette(false);
        assert!(light.background().l > 0.95);
        assert!(light.foreground().l < 0.2);
    }

    #[test]
    fn warp_scheme_is_default_and_overrides_tokens() {
        assert_eq!(PaletteScheme::default(), PaletteScheme::Warp);
        let mode = gpui_component::ThemeMode::Dark;
        let warp = ThemeTokens::from_mode_with_scheme(mode, PaletteScheme::Warp);
        let github = ThemeTokens::from_mode_with_scheme(mode, PaletteScheme::Github);
        // Warp pane background is near-black; GitHub's is a blue-grey.
        assert!(warp.pane_bg.l < 0.05);
        assert_ne!(warp.pane_bg, github.pane_bg);
        // Warp selection accent is the cyan #19AAD8.
        let accent = terminal::hsla_to_rgb(warp.block_border_selected);
        assert_eq!((accent.r, accent.g, accent.b), (0x19, 0xaa, 0xd8));
    }

    #[test]
    fn load_palette_dispatches_by_scheme() {
        let warp = load_palette(PaletteScheme::Warp, true);
        let github = load_palette(PaletteScheme::Github, true);
        assert_eq!(warp.background(), load_warp_palette(true).background());
        assert_eq!(github.background(), load_github_palette(true).background());
    }
}
