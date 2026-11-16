//! ConvertCraft's light visual style: palette, global egui style, and styled widgets.

use eframe::egui::{
    self, Button, Color32, CornerRadius, FontId, Frame, Margin, Response, RichText, Shadow, Stroke,
    TextStyle, Theme,
};

use crate::converter::Category;

pub const BG: Color32 = Color32::from_rgb(245, 246, 248);
pub const CARD: Color32 = Color32::WHITE;
pub const BORDER: Color32 = Color32::from_rgb(228, 231, 236);
pub const TEXT: Color32 = Color32::from_rgb(28, 32, 40);
pub const MUTED: Color32 = Color32::from_rgb(110, 116, 128);
pub const ORANGE: Color32 = Color32::from_rgb(242, 107, 48);
pub const BLUE: Color32 = Color32::from_rgb(46, 134, 245);
pub const GREEN: Color32 = Color32::from_rgb(30, 150, 80);
pub const RED: Color32 = Color32::from_rgb(220, 53, 69);
pub const AMBER: Color32 = Color32::from_rgb(200, 130, 20);
const INDIGO: Color32 = Color32::from_rgb(88, 101, 242);
const PURPLE: Color32 = Color32::from_rgb(139, 92, 246);

/// Forces the light theme and restyles widgets as flat, rounded, bordered controls.
pub fn apply(ctx: &egui::Context) {
    ctx.set_theme(Theme::Light);
    ctx.style_mut_of(Theme::Light, |style| {
        style
            .text_styles
            .insert(TextStyle::Body, FontId::proportional(14.0));
        style
            .text_styles
            .insert(TextStyle::Button, FontId::proportional(14.0));

        let spacing = &mut style.spacing;
        spacing.item_spacing = egui::vec2(8.0, 8.0);
        spacing.button_padding = egui::vec2(12.0, 6.0);
        spacing.interact_size.y = 30.0;

        let visuals = &mut style.visuals;
        visuals.panel_fill = BG;
        visuals.window_fill = CARD;
        visuals.extreme_bg_color = CARD;
        visuals.faint_bg_color = Color32::from_rgb(249, 250, 251);
        visuals.hyperlink_color = BLUE;
        visuals.slider_trailing_fill = true;
        visuals.selection.bg_fill = BLUE;
        visuals.selection.stroke = Stroke::new(1.0, Color32::WHITE);
        visuals.widgets.noninteractive.fg_stroke.color = TEXT;
        visuals.widgets.noninteractive.bg_stroke.color = BORDER;
        let widgets = &mut visuals.widgets;
        // `weak_bg_fill` paints buttons; `bg_fill` paints checkboxes and slider rails.
        for (state, button, rail, stroke) in [
            (
                &mut widgets.inactive,
                CARD,
                Color32::from_rgb(229, 232, 237),
                BORDER,
            ),
            (
                &mut widgets.hovered,
                Color32::from_rgb(241, 243, 246),
                Color32::from_rgb(218, 222, 229),
                Color32::from_rgb(200, 205, 212),
            ),
            (
                &mut widgets.active,
                Color32::from_rgb(232, 235, 240),
                Color32::from_rgb(208, 213, 221),
                Color32::from_rgb(180, 186, 196),
            ),
            (
                &mut widgets.open,
                Color32::from_rgb(241, 243, 246),
                Color32::from_rgb(218, 222, 229),
                Color32::from_rgb(200, 205, 212),
            ),
        ] {
            state.weak_bg_fill = button;
            state.bg_fill = rail;
            state.bg_stroke = Stroke::new(1.0, stroke);
            state.fg_stroke.color = TEXT;
            state.corner_radius = CornerRadius::same(8);
            state.expansion = 0.0;
        }
    });
}

/// White rounded panel with a hairline border and a soft shadow.
pub fn card() -> Frame {
    Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::same(16))
        .shadow(Shadow {
            offset: [0, 1],
            blur: 4,
            spread: 0,
            color: Color32::from_black_alpha(10),
        })
}

/// Filled orange call-to-action button.
pub fn primary(text: &str) -> Button<'static> {
    Button::new(RichText::new(text).color(Color32::WHITE).strong())
        .fill(ORANGE)
        .stroke(Stroke::NONE)
        .corner_radius(CornerRadius::same(8))
}

/// Button tinted with `color`, used for category chips and clickable status pills.
pub fn tinted(text: impl Into<String>, color: Color32) -> Button<'static> {
    Button::new(RichText::new(text).color(color))
        .fill(tint(color, 28))
        .stroke(Stroke::new(1.0, tint(color, 90)))
        .corner_radius(CornerRadius::same(8))
}

/// Non-interactive rounded label tinted with `color`.
pub fn pill(ui: &mut egui::Ui, text: &str, color: Color32) -> Response {
    Frame::new()
        .fill(tint(color, 28))
        .stroke(Stroke::new(1.0, tint(color, 90)))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(Margin::symmetric(10, 4))
        .show(ui, |ui| ui.label(RichText::new(text).color(color)))
        .response
}

/// `color` at `alpha`/255 strength over white, as an opaque color.
pub fn tint(color: Color32, alpha: u8) -> Color32 {
    let mix = |c: u8| 255 - (u16::from(255 - c) * u16::from(alpha) / 255) as u8;
    Color32::from_rgb(mix(color.r()), mix(color.g()), mix(color.b()))
}

pub fn category_color(category: Category) -> Color32 {
    match category {
        Category::Image => ORANGE,
        Category::Data => BLUE,
        Category::Audio => INDIGO,
        Category::Video => PURPLE,
    }
}

pub fn category_icon(category: Category) -> &'static str {
    match category {
        Category::Image => "🖼",
        Category::Data => "🗄",
        Category::Audio => "🎵",
        Category::Video => "🎬",
    }
}
