//! Small painting helpers shared by the panes and windows.

use crate::colors;
use egui::text::{LayoutJob, TextFormat, TextWrapping};
use egui::{Align2, Color32, CornerRadius, FontId, Galley, Painter, Pos2, Rect, Response, Sense, Stroke, Ui, Vec2};
use std::sync::Arc;

/// Lay out one line of text, cut with "…" to fit `max_width`.
pub fn elided(painter: &Painter, text: &str, font: FontId, color: Color32, max_width: f32) -> Arc<Galley> {
    let mut job = LayoutJob::single_section(text.to_owned(), TextFormat::simple(font, color));
    job.wrap = TextWrapping::truncate_at_width(max_width.max(8.0));
    painter.layout_job(job)
}

/// Paint `text` left-aligned and vertically centred in `rect`, elided to fit.
pub fn text_left(painter: &Painter, rect: Rect, text: &str, font: FontId, color: Color32) -> Rect {
    let g = elided(painter, text, font, color, rect.width());
    let pos = Pos2::new(rect.left(), rect.center().y - g.size().y / 2.0);
    let r = Rect::from_min_size(pos, g.size());
    painter.galley(pos, g, color);
    r
}

/// Paint `text` right-aligned and vertically centred in `rect` (not elided: numbers must not lie).
pub fn text_right(painter: &Painter, rect: Rect, text: &str, font: FontId, color: Color32) -> Rect {
    painter.text(Pos2::new(rect.right(), rect.center().y), Align2::RIGHT_CENTER, text, font, color)
}

/// Shrink column widths toward `min` (each in proportion to its room above its minimum) until
/// they fit in `avail`. Returns how much they are still too wide.
pub fn shrink_columns(w: &mut [f32], min: &[f32], avail: f32) -> f32 {
    let excess = w.iter().sum::<f32>() - avail;
    if excess <= 0.0 {
        return 0.0;
    }
    let slack: f32 = w.iter().zip(min).map(|(a, m)| (a - m).max(0.0)).sum();
    if slack <= 0.0 {
        return excess;
    }
    let take = excess.min(slack);
    for (a, m) in w.iter_mut().zip(min) {
        *a -= take * (*a - m).max(0.0) / slack;
    }
    excess - take
}

/// Scale every column evenly so that together they fit in `avail`.
pub fn scale_columns(w: &mut [f32], avail: f32) {
    let total: f32 = w.iter().sum();
    if total > avail && total > 0.0 {
        let k = avail.max(0.0) / total;
        for a in w.iter_mut() {
            *a *= k;
        }
    }
}

/// Header text right-aligned in `rect`, cut with "…" when the column is narrower than the text.
pub fn header_right(painter: &Painter, rect: Rect, text: &str, font: FontId, color: Color32) -> Rect {
    let g = elided(painter, text, font, color, rect.width());
    let pos = Pos2::new(rect.right() - g.size().x, rect.center().y - g.size().y / 2.0);
    let r = Rect::from_min_size(pos, g.size());
    painter.galley(pos, g, color);
    r
}

/// A horizontal bar: `share` (0..=1) of the track filled with `color`, on a faint full-width track.
/// Thin and rounded at the data end, anchored at the left.
pub fn bar(painter: &Painter, rect: Rect, share: f32, color: Color32) {
    let h = 6.0f32.min(rect.height());
    let track = Rect::from_center_size(rect.center(), Vec2::new(rect.width(), h));
    painter.rect_filled(track, CornerRadius::same(2), colors::GRID);
    let share = share.clamp(0.0, 1.0);
    if share > 0.0 {
        let w = (track.width() * share).max(2.0);
        let fill = Rect::from_min_size(track.min, Vec2::new(w, h));
        painter.rect_filled(fill, CornerRadius::same(2), color);
    }
}

/// A count in a rounded pill (copies). Returns the pill's rect. `fill = None` draws the number
/// without a pill, in muted ink.
pub fn pill(painter: &Painter, right_center: Pos2, text: &str, fill: Option<Color32>, font: FontId) -> Rect {
    match fill {
        Some(fill) => {
            let ink = colors::label_on(fill);
            let g = painter.layout_no_wrap(text.to_owned(), font, ink);
            let w = (g.size().x + 12.0).max(24.0);
            let h = (g.size().y + 2.0).min(18.0);
            let r = Rect::from_min_max(
                Pos2::new(right_center.x - w, right_center.y - h / 2.0),
                Pos2::new(right_center.x, right_center.y + h / 2.0),
            );
            painter.rect_filled(r, CornerRadius::same((h / 2.0) as u8), fill);
            painter.galley(Pos2::new(r.center().x - g.size().x / 2.0, r.center().y - g.size().y / 2.0), g, ink);
            r
        }
        None => painter.text(
            Pos2::new(right_center.x - 8.0, right_center.y),
            Align2::RIGHT_CENTER,
            text,
            font,
            colors::INK_MUTED,
        ),
    }
}

/// A small outlined label, e.g. "default", "Windows". Returns its width.
pub fn tag(painter: &Painter, left_center: Pos2, text: &str, font: FontId, ink: Color32, stroke: Color32) -> Rect {
    let g = painter.layout_no_wrap(text.to_owned(), font, ink);
    let r = Rect::from_min_size(
        Pos2::new(left_center.x, left_center.y - (g.size().y + 2.0) / 2.0),
        Vec2::new(g.size().x + 10.0, g.size().y + 2.0),
    );
    painter.rect_stroke(r, CornerRadius::same(3), Stroke::new(1.0, stroke), egui::StrokeKind::Inside);
    painter.galley(Pos2::new(r.left() + 5.0, r.top() + 1.0), g, ink);
    r
}

/// A status label: coloured icon followed by text in ink (status colour never carries meaning alone).
pub fn status_label(ui: &mut Ui, icon: &str, color: Color32, text: impl Into<String>) -> Response {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        ui.label(egui::RichText::new(icon).color(color).strong());
        ui.label(text.into());
    })
    .response
}

/// A small triangle beside a sorted column's title: down for descending, up for ascending.
pub fn sort_arrow(painter: &Painter, c: Pos2, descending: bool, color: Color32) {
    let s = 3.5;
    let pts = if descending {
        vec![Pos2::new(c.x - s, c.y - s * 0.6), Pos2::new(c.x + s, c.y - s * 0.6), Pos2::new(c.x, c.y + s * 0.7)]
    } else {
        vec![Pos2::new(c.x - s, c.y + s * 0.6), Pos2::new(c.x + s, c.y + s * 0.6), Pos2::new(c.x, c.y - s * 0.7)]
    };
    painter.add(egui::Shape::convex_polygon(pts, color, Stroke::NONE));
}

/// The expand/collapse triangle.
pub fn expander(painter: &Painter, rect: Rect, open: bool, color: Color32) {
    let c = rect.center();
    let s = 4.0;
    let pts = if open {
        vec![Pos2::new(c.x - s, c.y - s * 0.5), Pos2::new(c.x + s, c.y - s * 0.5), Pos2::new(c.x, c.y + s * 0.6)]
    } else {
        vec![Pos2::new(c.x - s * 0.5, c.y - s), Pos2::new(c.x + s * 0.6, c.y), Pos2::new(c.x - s * 0.5, c.y + s)]
    };
    painter.add(egui::Shape::convex_polygon(pts, color, Stroke::NONE));
}

/// A simple folder glyph (tab shape plus body), drawn so no icon font is needed.
pub fn folder_icon(painter: &Painter, rect: Rect, color: Color32) {
    let c = rect.center();
    let body = Rect::from_center_size(Pos2::new(c.x, c.y + 1.0), Vec2::new(12.0, 8.0));
    let tab = Rect::from_min_size(Pos2::new(body.left(), body.top() - 2.5), Vec2::new(5.0, 3.0));
    painter.rect_filled(tab, CornerRadius::same(1), color);
    painter.rect_filled(body, CornerRadius::same(1), color);
}

/// A file glyph (page with a folded corner outline).
pub fn file_icon(painter: &Painter, rect: Rect, color: Color32) {
    let c = rect.center();
    let r = Rect::from_center_size(c, Vec2::new(9.0, 11.0));
    painter.rect_stroke(r, CornerRadius::same(1), Stroke::new(1.0, color), egui::StrokeKind::Inside);
}

/// A clickable text that looks like a link.
pub fn link(ui: &mut Ui, text: impl Into<String>) -> Response {
    let text = text.into();
    let r = ui.add(egui::Label::new(egui::RichText::new(text).color(colors::LINK)).sense(Sense::click()));
    r.on_hover_cursor(egui::CursorIcon::PointingHand)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_shrink_toward_their_minimum_first() {
        let mut w = [170.0, 78.0, 76.0, 170.0];
        let min = [90.0, 78.0, 76.0, 90.0];
        assert_eq!(shrink_columns(&mut w, &min, 1000.0), 0.0);
        assert_eq!(w, [170.0, 78.0, 76.0, 170.0]);
        assert_eq!(shrink_columns(&mut w, &min, 418.0), 0.0);
        assert!((w[0] - 132.0).abs() < 0.01 && (w[3] - 132.0).abs() < 0.01 && w[1] == 78.0 && w[2] == 76.0);
        let left = shrink_columns(&mut w, &min, 300.0);
        assert!((left - 34.0).abs() < 0.01, "{left}");
        assert_eq!(w, [90.0, 78.0, 76.0, 90.0]);
        scale_columns(&mut w, 300.0);
        assert!((w.iter().sum::<f32>() - 300.0).abs() < 0.01);
    }
}
