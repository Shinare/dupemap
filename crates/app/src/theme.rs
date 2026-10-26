//! The dark theme (the only theme).

use crate::colors::*;
use egui::{Color32, CornerRadius, FontFamily, FontId, Margin, Stroke, Style, TextStyle, Vec2};

const fn rgb(h: u32) -> Color32 {
    Color32::from_rgb((h >> 16) as u8, (h >> 8) as u8, h as u8)
}

const HOVER: Color32 = rgb(0x2c2c29);
const PRESSED: Color32 = rgb(0x363632);
/// Filled parts of checkboxes, radio buttons and scroll bars (buttons use `weak_bg_fill`):
/// mid-grey, so an empty box or circle stands out on panels and popups alike.
const CONTROL: Color32 = rgb(0x6b6a66);
const CONTROL_HOVER: Color32 = rgb(0x7a7974);
const CONTROL_PRESSED: Color32 = rgb(0x858480);

pub fn apply(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    ctx.style_mut_of(egui::Theme::Dark, configure);
}

pub fn configure(style: &mut Style) {
    style.visuals = egui::Visuals::dark();
    let v = &mut style.visuals;
    v.panel_fill = PANEL;
    v.window_fill = SURFACE_RAISED;
    v.window_stroke = Stroke::new(1.0, BASELINE);
    v.window_corner_radius = CornerRadius::same(6);
    v.menu_corner_radius = CornerRadius::same(5);
    v.extreme_bg_color = PAGE;
    v.faint_bg_color = SURFACE_ALT;
    v.code_bg_color = SURFACE_ALT;
    v.hyperlink_color = LINK;
    v.warn_fg_color = WARNING;
    v.error_fg_color = CRITICAL;
    v.selection.bg_fill = SELECTION;
    v.selection.stroke = Stroke::new(1.0, INK);
    v.striped = true;

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = SURFACE;
    w.noninteractive.weak_bg_fill = SURFACE;
    w.noninteractive.bg_stroke = Stroke::new(1.0, GRID);
    w.noninteractive.fg_stroke = Stroke::new(1.0, INK_SECONDARY);
    w.inactive.bg_fill = CONTROL;
    w.inactive.weak_bg_fill = SURFACE_RAISED;
    w.inactive.bg_stroke = Stroke::new(1.0, GRID);
    w.inactive.fg_stroke = Stroke::new(1.0, INK);
    w.hovered.bg_fill = CONTROL_HOVER;
    w.hovered.weak_bg_fill = HOVER;
    w.hovered.bg_stroke = Stroke::new(1.0, BASELINE);
    w.hovered.fg_stroke = Stroke::new(1.0, INK);
    w.active.bg_fill = CONTROL_PRESSED;
    w.active.weak_bg_fill = PRESSED;
    w.active.bg_stroke = Stroke::new(1.0, INK_MUTED);
    w.active.fg_stroke = Stroke::new(1.0, INK);
    w.open.bg_fill = HOVER;
    w.open.weak_bg_fill = HOVER;
    w.open.bg_stroke = Stroke::new(1.0, BASELINE);
    w.open.fg_stroke = Stroke::new(1.0, INK);
    for s in [&mut w.noninteractive, &mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
        s.corner_radius = CornerRadius::same(4);
        s.expansion = 0.0;
    }

    style.text_styles = [
        (TextStyle::Small, FontId::new(11.5, FontFamily::Proportional)),
        (TextStyle::Body, FontId::new(13.5, FontFamily::Proportional)),
        (TextStyle::Button, FontId::new(13.5, FontFamily::Proportional)),
        (TextStyle::Heading, FontId::new(18.0, FontFamily::Proportional)),
        (TextStyle::Monospace, FontId::new(12.5, FontFamily::Monospace)),
    ]
    .into();

    let s = &mut style.spacing;
    s.item_spacing = Vec2::new(8.0, 5.0);
    s.button_padding = Vec2::new(9.0, 3.0);
    s.interact_size.y = 22.0;
    s.window_margin = Margin::same(12);
    s.menu_margin = Margin::same(6);
    s.scroll = egui::style::ScrollStyle::solid();
    s.scroll.bar_width = 10.0;
}

/// Dock (tab bar) style matching the theme.
pub fn dock_style(style: &Style) -> egui_dock::Style {
    let mut d = egui_dock::Style::from_egui(style);
    d.main_surface_border_stroke = Stroke::NONE;
    d.tab_bar.bg_fill = PAGE;
    d.tab_bar.hline_color = BASELINE;
    d.tab_bar.height = 30.0;
    d.tab.tab_body.bg_fill = PANEL;
    d.tab.tab_body.stroke = Stroke::NONE;
    d.tab.tab_body.inner_margin = Margin::same(4);
    d.tab.active.bg_fill = PANEL;
    d.tab.active.text_color = INK;
    d.tab.active.outline_color = BASELINE;
    d.tab.focused.bg_fill = PANEL;
    d.tab.focused.text_color = INK;
    d.tab.focused.outline_color = ACCENT;
    d.tab.inactive.bg_fill = PAGE;
    d.tab.inactive.text_color = INK_SECONDARY;
    d.tab.inactive.outline_color = PAGE;
    d.tab.hovered.bg_fill = SURFACE;
    d.tab.hovered.text_color = INK;
    d.tab.hovered.outline_color = GRID;
    d.separator.color_idle = GRID;
    d.separator.color_hovered = INK_MUTED;
    d.separator.color_dragged = ACCENT;
    d.separator.width = 2.0;
    d.overlay.selection_color = alpha(ACCENT, 0.25);
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkboxes_and_radio_buttons_are_visible() {
        for bg in [PANEL, SURFACE, SURFACE_RAISED, PAGE] {
            assert!(contrast(CONTROL, bg) >= 2.8, "empty control on {bg:?}");
        }
        assert!(contrast(CONTROL, PANEL) >= 3.0);
        // the tick and the radio dot are drawn in ink on the control
        for c in [CONTROL, CONTROL_HOVER, CONTROL_PRESSED] {
            assert!(contrast(INK, c) >= 3.0, "{c:?}");
        }
        assert!(contrast(INK, CONTROL) >= 4.5);
    }
}
