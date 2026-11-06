//! Visual tokens: dark, low-chroma surfaces separated by hairlines, small corner radii on
//! controls, square large surfaces, and one accent colour for primary actions and focus.

use egui::{Color32, CornerRadius, FontFamily, FontId, Stroke, TextStyle, Visuals};

const fn hex(rgb: u32) -> Color32 {
    let [_, r, g, b] = rgb.to_be_bytes();
    Color32::from_rgb(r, g, b)
}

const fn alpha(rgb: u32, a: u8) -> Color32 {
    let [_, r, g, b] = rgb.to_be_bytes();
    Color32::from_rgba_unmultiplied_const(r, g, b, a)
}

/// Main content background.
pub const BG: Color32 = hex(0x131417);
/// Side columns and toolbars.
pub const PANEL: Color32 = hex(0x0d0e10);
/// Buttons and other raised controls.
pub const CONTROL: Color32 = hex(0x1d1f23);
/// Text inputs and code wells.
pub const SUNKEN: Color32 = hex(0x08090a);

pub const INK: Color32 = hex(0xeeedea);
/// Secondary text; 60 % ink on `BG` keeps WCAG AA contrast for body sizes.
pub const MUTED: Color32 = alpha(0xeeedea, 160);
pub const LINE: Color32 = alpha(0xffffff, 38);
pub const LINE_STRONG: Color32 = alpha(0xeeedea, 102);
pub const HOVER: Color32 = alpha(0xffffff, 26);

pub const ACCENT: Color32 = hex(0x3fa66b);
pub const ACCENT_INK: Color32 = hex(0x7fd3a0);
pub const WARN: Color32 = hex(0xf2c14e);
pub const BAD: Color32 = hex(0xf87171);

/// Syntax colours for code and EDN on `SUNKEN`; all keep at least 7:1 contrast.
pub mod syntax {
    use super::{Color32, alpha, hex};

    pub const KEYWORD: Color32 = hex(0x7fd3a0);
    pub const STRING: Color32 = hex(0xe8c887);
    pub const NUMBER: Color32 = hex(0x9cc4f5);
    pub const LITERAL: Color32 = hex(0xcfb0f5);
    pub const TAG: Color32 = hex(0xf2ab7c);
    pub const SYMBOL: Color32 = hex(0xeeedea);
    pub const COMMENT: Color32 = alpha(0xeeedea, 150);
    pub const MACRO: Color32 = hex(0xf2ab7c);
    pub const INVALID: Color32 = hex(0xf87171);
    /// Bracket colours cycle with nesting depth so deep data stays readable.
    pub const DELIMITERS: [Color32; 4] = [hex(0xc8c6c0), hex(0x9cc4f5), hex(0xcfb0f5), hex(0xe8c887)];
}

pub const RADIUS: CornerRadius = CornerRadius::same(3);
pub const CONTROL_H: f32 = 30.0;

/// Text-size range offered in settings (egui zoom factor).
pub const ZOOM_RANGE: std::ops::RangeInclusive<f32> = 0.8..=2.0;

pub fn hairline() -> Stroke {
    Stroke::new(1.0, LINE)
}

fn visuals() -> Visuals {
    let mut v = Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = PANEL;
    v.extreme_bg_color = SUNKEN;
    v.faint_bg_color = CONTROL;
    v.code_bg_color = SUNKEN;
    v.window_stroke = hairline();
    v.window_corner_radius = RADIUS;
    v.menu_corner_radius = RADIUS;
    v.override_text_color = Some(INK);
    v.hyperlink_color = ACCENT_INK;
    v.selection.bg_fill = ACCENT;
    v.selection.stroke = Stroke::new(1.0, Color32::WHITE);
    v.warn_fg_color = WARN;
    v.error_fg_color = BAD;
    for (w, fill, stroke) in [
        (&mut v.widgets.noninteractive, CONTROL, LINE),
        (&mut v.widgets.inactive, CONTROL, LINE),
        (&mut v.widgets.hovered, HOVER, LINE_STRONG),
        (&mut v.widgets.active, HOVER, LINE_STRONG),
        (&mut v.widgets.open, HOVER, LINE_STRONG),
    ] {
        w.bg_fill = fill;
        w.weak_bg_fill = fill;
        w.bg_stroke = Stroke::new(1.0, stroke);
        w.corner_radius = RADIUS;
        w.expansion = 0.0;
    }
    // Keyboard focus must stay visible on every control: egui draws focused widgets with the
    // `active` visuals, so they get an accent outline.
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, Color32::WHITE);
    v.widgets.active.bg_stroke = Stroke::new(2.0, ACCENT_INK);
    v.widgets.active.fg_stroke = Stroke::new(1.0, Color32::WHITE);
    v
}

pub fn apply(ctx: &egui::Context) {
    ctx.set_visuals_of(egui::Theme::Dark, visuals());
    ctx.set_theme(egui::ThemePreference::Dark);
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(10.0, 5.0);
        style.spacing.interact_size.y = CONTROL_H;
        style.text_styles = [
            (TextStyle::Heading, FontId::new(20.0, FontFamily::Proportional)),
            (TextStyle::Body, FontId::new(14.0, FontFamily::Proportional)),
            (TextStyle::Button, FontId::new(14.0, FontFamily::Proportional)),
            (TextStyle::Small, FontId::new(12.0, FontFamily::Proportional)),
            (TextStyle::Monospace, FontId::new(13.0, FontFamily::Monospace)),
        ]
        .into();
    });
}
