//! The duplicate groups view: every content with two or more copies, most wasted space first.
//! Rows are fetched a page at a time from the query worker as they scroll into view.

use crate::actions::Action;
use crate::colors;
use crate::format;
use crate::selection::{Linked, Target};
use crate::widgets;
use crate::worker::{GroupRow, Query, Worker, GROUP_PAGE};
use dupemap_core::index::{DupQuery, HashFilter, Index, Spread};
use egui::{CornerRadius, FontId, Key, Pos2, Rangef, Rect, ScrollArea, Sense, Stroke, Ui, Vec2};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

const ROW_H: f32 = 22.0;
const HEADER_H: f32 = 24.0;
const PAD: f32 = 8.0;
/// Pages kept before the cache starts over (each page is GROUP_PAGE rows).
const MAX_PAGES: usize = 400;

type PageKey = (DupQuery, usize);

/// Pages of duplicate groups, shared by every groups tab.
#[derive(Default)]
pub struct GroupCache {
    generation: u64,
    pages: HashMap<PageKey, Arc<Vec<GroupRow>>>,
    errors: HashMap<PageKey, String>,
    counts: HashMap<DupQuery, usize>,
    totals: HashMap<DupQuery, (usize, u64)>,
    pending: HashSet<PageKey>,
}

impl GroupCache {
    pub fn reset(&mut self, generation: u64) {
        *self = GroupCache { generation, ..Default::default() };
    }

    pub fn page(&self, q: &DupQuery, page: usize) -> Option<&Arc<Vec<GroupRow>>> {
        self.pages.get(&(q.clone(), page))
    }

    pub fn error(&self, q: &DupQuery, page: usize) -> Option<&str> {
        self.errors.get(&(q.clone(), page)).map(|s| s.as_str())
    }

    /// Number of matching groups, once the first reply for this query arrived.
    pub fn count(&self, q: &DupQuery) -> Option<usize> {
        self.counts.get(q).copied()
    }

    /// (groups, bytes in extra copies) for a query.
    pub fn totals(&self, q: &DupQuery) -> Option<(usize, u64)> {
        self.totals.get(q).copied()
    }

    pub fn is_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// The biggest group of the default view, once loaded.
    pub fn first_row(&self) -> Option<&GroupRow> {
        self.pages.get(&(GroupsPane::default().query, 0)).and_then(|p| p.first())
    }

    pub fn request(&mut self, worker: &Worker, q: &DupQuery, page: usize) {
        let key = (q.clone(), page);
        if self.pages.contains_key(&key) || self.pending.contains(&key) || self.errors.contains_key(&key) {
            return;
        }
        let totals = !self.totals.contains_key(q) && !self.pending.iter().any(|(pq, _)| pq == q);
        self.pending.insert(key);
        worker.send(Query::Groups { generation: self.generation, query: q.clone(), page, totals });
    }

    #[allow(clippy::too_many_arguments)]
    pub fn insert(
        &mut self,
        generation: u64,
        q: DupQuery,
        page: usize,
        total: usize,
        totals: Option<(usize, u64)>,
        rows: Result<Arc<Vec<GroupRow>>, String>,
    ) {
        if generation != self.generation {
            return;
        }
        let key = (q.clone(), page);
        self.pending.remove(&key);
        self.counts.insert(q.clone(), total);
        if let Some(t) = totals {
            self.totals.insert(q, t);
        }
        if self.pages.len() >= MAX_PAGES {
            self.pages.clear();
        }
        match rows {
            Ok(r) => {
                self.pages.insert(key, r);
            }
            Err(e) => {
                self.errors.insert(key, e);
            }
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct GroupsPane {
    pub query: DupQuery,
    #[serde(skip)]
    view: View,
}

#[derive(Clone, Default)]
struct View {
    generation: u64,
    /// Selected row (index into the current listing).
    selected: Option<usize>,
    offset: f32,
    viewport_h: f32,
    scroll_to: Option<f32>,
}

pub struct GroupsCtx<'a> {
    pub ix: &'a Arc<Index>,
    pub generation: u64,
    pub cache: &'a mut GroupCache,
    pub worker: &'a Worker,
    pub actions: &'a mut Vec<Action>,
    pub pane_id: u64,
    pub focused: bool,
    pub linked: Option<&'a Linked>,
}

#[derive(Clone, Copy)]
struct Cols {
    name: Rangef,
    size: Rangef,
    copies: Rangef,
    sources: Rangef,
    wasted: Rangef,
}

impl Default for GroupsPane {
    fn default() -> Self {
        GroupsPane { query: DupQuery { min_copies: 2, ..Default::default() }, view: View::default() }
    }
}

impl GroupsPane {
    pub fn title(&self) -> String {
        "Duplicate groups".into()
    }

    pub fn ui(&mut self, ui: &mut Ui, cx: &mut GroupsCtx) {
        if self.view.generation != cx.generation {
            self.view = View { generation: cx.generation, ..Default::default() };
        }
        let full = ui.available_rect_before_wrap();
        if ui.rect_contains_pointer(full) && ui.input(|i| i.pointer.any_pressed()) && !cx.focused {
            cx.actions.push(Action::Focus(cx.pane_id));
        }
        let before = self.query.clone();
        self.toolbar(ui, cx);
        if self.query != before {
            self.view.selected = None;
            self.view.scroll_to = Some(0.0);
        }
        ui.add_space(4.0);
        let Some(total) = cx.cache.count(&self.query) else {
            cx.cache.request(cx.worker, &self.query, 0);
            ui.add_space(20.0);
            ui.vertical_centered(|ui| {
                ui.add(egui::Spinner::new());
                ui.label(egui::RichText::new("Finding duplicate groups…").color(colors::INK_MUTED));
            });
            return;
        };
        if total == 0 {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new("No duplicate groups match").color(colors::INK_SECONDARY));
            });
            return;
        }
        let mut select: Option<usize> = None;
        self.table(ui, cx, total, &mut select);
        if cx.focused && ui.memory(|m| m.focused().is_none()) {
            self.keyboard(ui, cx, total, &mut select);
        }
        if let Some(i) = select {
            self.select(cx, i);
        }
    }

    fn select(&mut self, cx: &mut GroupsCtx, i: usize) {
        self.view.selected = Some(i);
        if let Some(row) = cx.cache.page(&self.query, i / GROUP_PAGE).and_then(|p| p.get(i % GROUP_PAGE)) {
            cx.actions.push(Action::Inspect(Some(Target::File { file: row.file, content: row.group.content })));
        }
    }

    fn toolbar(&mut self, ui: &mut Ui, cx: &mut GroupsCtx) {
        let ix = cx.ix.clone();
        // two rows in a narrow tab, so nothing is pushed off screen
        let narrow = ui.available_width() < 600.0;
        let q = &mut self.query;
        let pane = cx.pane_id;
        let first = |ui: &mut Ui, q: &mut DupQuery| {
            let inv_text = match q.involving.and_then(|id| ix.source_ix(id)) {
                Some(s) => format!("With a copy in {}", format::host(&ix.sources()[s as usize].host)),
                None => "Any source".into(),
            };
            egui::ComboBox::from_id_salt(("g-inv", pane)).selected_text(inv_text).width(200.0).height(420.0).show_ui(
                ui,
                |ui| {
                    ui.selectable_value(&mut q.involving, None, "Any source");
                    for s in ix.sources() {
                        ui.selectable_value(
                            &mut q.involving,
                            Some(s.db_id),
                            format!("With a copy in {}", format::host(&s.host)),
                        );
                    }
                },
            );
            let spread_text = match q.spread {
                Spread::Any => "Anywhere",
                Spread::OneSource => "Within one source",
                Spread::SeveralSources => "Across sources",
            };
            egui::ComboBox::from_id_salt(("g-spread", pane)).selected_text(spread_text).width(140.0).show_ui(
                ui,
                |ui| {
                    ui.selectable_value(&mut q.spread, Spread::Any, "Anywhere");
                    ui.selectable_value(&mut q.spread, Spread::OneSource, "Within one source")
                        .on_hover_text("Every copy is on the same machine, drive or cloud account");
                    ui.selectable_value(&mut q.spread, Spread::SeveralSources, "Across sources")
                        .on_hover_text("Copies on two or more machines, drives or cloud accounts");
                },
            );
        };
        let second = |ui: &mut Ui, q: &mut DupQuery| {
            let copies_text = format!("{}+ copies", q.min_copies.max(2));
            egui::ComboBox::from_id_salt(("g-min", pane)).selected_text(copies_text).width(90.0).show_ui(ui, |ui| {
                for n in [2u32, 3, 4, 5, 10] {
                    ui.selectable_value(&mut q.min_copies, n, format!("{n}+ copies"));
                }
            });
            let hash_text = match q.hash {
                HashFilter::Any => "Any hash",
                HashFilter::Sha256 => "SHA-256 only",
                HashFilter::Flagged => "Flagged only",
            };
            egui::ComboBox::from_id_salt(("g-hash", pane)).selected_text(hash_text).width(110.0).show_ui(ui, |ui| {
                ui.selectable_value(&mut q.hash, HashFilter::Any, "Any hash");
                ui.selectable_value(&mut q.hash, HashFilter::Sha256, "SHA-256 only");
                ui.selectable_value(&mut q.hash, HashFilter::Flagged, "Flagged only")
                    .on_hover_text("Rows whose hash is not SHA-256 (they only match the same algorithm)");
            });
        };
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            first(ui, q);
            if !narrow {
                second(ui, q);
            }
        });
        if narrow {
            ui.horizontal(|ui| {
                ui.add_space(4.0);
                second(ui, q);
            });
        }
        ui.horizontal(|ui| {
            ui.add_space(6.0);
            if let Some((n, wasted)) = cx.cache.totals(&self.query) {
                let t = format!(
                    "{} · {} in extra copies",
                    format::plural(n as u64, "group", "groups"),
                    format::bytes(wasted)
                );
                ui.add(egui::Label::new(egui::RichText::new(&t).color(colors::INK_SECONDARY)).truncate())
                    .on_hover_text(format!(
                        "{t}\nExtra copies = every copy beyond one per group. Which copy to keep is up to you."
                    ));
            }
            if cx.cache.is_pending() {
                ui.add(egui::Spinner::new().size(12.0));
            }
        });
    }

    fn columns(left: f32, right: f32) -> Cols {
        // Each, Copies, Sources, Extra copies; the last two (elided text, bar) give way first
        let mut w = [84.0f32, 66.0, 130.0, 180.0];
        const MIN: [f32; 4] = [72.0, 62.0, 74.0, 96.0];
        let width = right - left;
        let name_min = 160.0f32.max((width * 0.4).min(width - MIN.iter().sum::<f32>()));
        let avail = (right - left - name_min).max(0.0);
        let text_first = [w[0], w[1], MIN[2], MIN[3]];
        if widgets::shrink_columns(&mut w, &text_first, avail) > 0.0
            && widgets::shrink_columns(&mut w, &MIN, avail) > 0.0
        {
            widgets::scale_columns(&mut w, avail);
        }
        let e0 = right - w[3];
        let s0 = e0 - w[2];
        let c0 = s0 - w[1];
        let z0 = c0 - w[0];
        Cols {
            name: Rangef::new(left, z0),
            size: Rangef::new(z0, c0),
            copies: Rangef::new(c0, s0),
            sources: Rangef::new(s0, e0),
            wasted: Rangef::new(e0, right),
        }
    }

    fn table(&mut self, ui: &mut Ui, cx: &mut GroupsCtx, total: usize, select: &mut Option<usize>) {
        let scroll = &ui.spacing().scroll;
        let bar = scroll.bar_width + scroll.bar_inner_margin + scroll.bar_outer_margin;
        let full = ui.available_rect_before_wrap().intersect(ui.clip_rect());
        let cols = Self::columns(full.left(), full.right() - bar);
        let (hrect, _) = ui.allocate_exact_size(Vec2::new(full.width(), HEADER_H), Sense::hover());
        header(ui, hrect, cols);
        // the biggest group sets the bar scale (rows are sorted, so it is the first one)
        let max_wasted =
            cx.cache.page(&self.query, 0).and_then(|p| p.first()).map(|r| r.group.wasted).unwrap_or(1).max(1);
        ui.spacing_mut().item_spacing.y = 0.0;
        let mut area = ScrollArea::vertical()
            .id_salt(("groups", cx.pane_id))
            .auto_shrink([false, false])
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible);
        if let Some(off) = self.view.scroll_to.take() {
            area = area.vertical_scroll_offset(off);
        }
        let out = area.show_rows(ui, ROW_H, total, |ui, range| {
            for i in range {
                let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H), Sense::hover());
                let page = i / GROUP_PAGE;
                let row = cx.cache.page(&self.query, page).and_then(|p| p.get(i % GROUP_PAGE)).cloned();
                match row {
                    Some(r) => self.row(ui, i, rect, cols, &r, max_wasted, cx, select),
                    None => {
                        cx.cache.request(cx.worker, &self.query, page);
                        let p = ui.painter_at(rect);
                        let text = cx.cache.error(&self.query, page).map(|e| format!("Could not load: {e}"));
                        widgets::text_left(
                            &p,
                            Rect::from_x_y_ranges(Rangef::new(cols.name.min + PAD, cols.name.max), rect.y_range()),
                            text.as_deref().unwrap_or("…"),
                            FontId::proportional(13.0),
                            colors::INK_MUTED,
                        );
                    }
                }
            }
        });
        self.view.offset = out.state.offset.y;
        self.view.viewport_h = out.inner_rect.height();
    }

    #[allow(clippy::too_many_arguments)]
    fn row(
        &self,
        ui: &mut Ui,
        i: usize,
        rect: Rect,
        cols: Cols,
        r: &GroupRow,
        max_wasted: u64,
        cx: &mut GroupsCtx,
        select: &mut Option<usize>,
    ) {
        let resp = ui.interact(rect, ui.id().with(("grow", cx.pane_id, i)), Sense::CLICK);
        if resp.clicked() || resp.secondary_clicked() || resp.double_clicked() {
            ui.memory_mut(|m| {
                if let Some(f) = m.focused() {
                    m.surrender_focus(f);
                }
            });
        }
        let p = ui.painter_at(rect);
        let selected = self.view.selected == Some(i) || cx.linked.is_some_and(|l| l.content == Some(r.group.content));
        if selected {
            p.rect_filled(rect, CornerRadius::ZERO, colors::ROW_SELECTED);
            p.rect_filled(
                Rect::from_min_size(rect.min, Vec2::new(2.0, rect.height())),
                CornerRadius::ZERO,
                colors::ACCENT,
            );
        } else if resp.hovered() {
            p.rect_filled(rect, CornerRadius::ZERO, colors::ROW_HOVER);
        } else if i % 2 == 1 {
            p.rect_filled(rect, CornerRadius::ZERO, colors::ROW_STRIPE);
        }
        let font = FontId::proportional(13.0);
        let small = FontId::proportional(11.5);
        let g = &r.group;
        // name
        let icon = Rect::from_min_size(Pos2::new(cols.name.min + PAD, rect.top()), Vec2::new(16.0, ROW_H));
        widgets::file_icon(&p, icon, colors::INK_MUTED);
        let mut nr = Rect::from_x_y_ranges(Rangef::new(icon.right() + 6.0, cols.name.max - PAD), rect.y_range());
        if g.flagged {
            let label = format!("⚠ {}", cx.ix.content_algo_name(g.content).to_uppercase());
            let w = p.layout_no_wrap(label.clone(), small.clone(), colors::INK).size().x + 10.0;
            if nr.width() > w + 60.0 {
                widgets::tag(
                    &p,
                    Pos2::new(nr.right() - w, rect.center().y),
                    &label,
                    small.clone(),
                    colors::INK,
                    colors::WARNING,
                );
                nr.max.x -= w + 6.0;
            }
        }
        widgets::text_left(&p, nr, &r.name, font.clone(), colors::INK);
        // size of each copy
        widgets::text_right(&p, inner(cols.size, rect), &format::bytes(g.size), font.clone(), colors::INK_SECONDARY);
        // copies
        widgets::pill(
            &p,
            Pos2::new(cols.copies.max - PAD, rect.center().y),
            &format::count(g.copies as u64),
            colors::copies_fill(g.copies),
            small.clone(),
        );
        // sources
        let src_text = if g.sources == 1 {
            match cx.ix.content_sources(g.content).first() {
                Some(&(s, _)) => format!("only {}", format::host(&cx.ix.sources()[s as usize].host)),
                None => "1 source".into(),
            }
        } else {
            format::plural(g.sources as u64, "source", "sources")
        };
        let sr = inner(cols.sources, rect);
        let gal = widgets::elided(&p, &src_text, small.clone(), colors::INK_SECONDARY, sr.width());
        p.galley(Pos2::new(sr.right() - gal.size().x, sr.center().y - gal.size().y / 2.0), gal, colors::INK_SECONDARY);
        // wasted: bar (share of the biggest group) + number
        let wr = inner(cols.wasted, rect);
        let text_w = 64.0f32.min(wr.width());
        widgets::text_right(&p, wr, &format::bytes(g.wasted), font, colors::INK);
        let bar = Rect::from_x_y_ranges(Rangef::new(wr.left(), wr.right() - text_w - 8.0), rect.y_range());
        if bar.width() >= 16.0 {
            widgets::bar(&p, bar, g.wasted as f32 / max_wasted as f32, colors::DUP);
        }

        let resp = resp.on_hover_ui(|ui| {
            ui.label(egui::RichText::new(&r.name).strong());
            ui.label(format!(
                "{} each · {} · {} in extra copies",
                format::bytes(g.size),
                format::plural(g.copies as u64, "copy", "copies"),
                format::bytes(g.wasted)
            ));
            for &(s, n) in cx.ix.content_sources(g.content) {
                ui.label(format!(
                    "{}: {}",
                    format::host(&cx.ix.sources()[s as usize].host),
                    format::plural(n as u64, "copy", "copies")
                ));
            }
            ui.label(
                egui::RichText::new("Double-click to open the first copy in a tab").small().color(colors::INK_MUTED),
            );
        });
        if resp.clicked() || resp.secondary_clicked() {
            *select = Some(i);
        }
        if resp.double_clicked() {
            cx.actions.push(Action::ShowInTab(r.file));
        }
        resp.context_menu(|ui| {
            if ui.button("Show the first copy in a tab").clicked() {
                cx.actions.push(Action::ShowInTab(r.file));
                ui.close();
            }
            if ui.button("Copy name").clicked() {
                cx.actions.push(Action::CopyText(r.name.clone()));
                ui.close();
            }
            if ui.button("Copy path of the first copy").clicked() {
                cx.actions.push(Action::CopyText(cx.ix.path_with_name(cx.ix.file_dir(r.file), &r.name)));
                ui.close();
            }
        });
    }

    fn keyboard(&mut self, ui: &mut Ui, cx: &mut GroupsCtx, total: usize, select: &mut Option<usize>) {
        use egui::Modifiers as M;
        let (down, up, pgdn, pgup, home, end, enter) = ui.input_mut(|i| {
            (
                i.consume_key(M::NONE, Key::ArrowDown),
                i.consume_key(M::NONE, Key::ArrowUp),
                i.consume_key(M::NONE, Key::PageDown),
                i.consume_key(M::NONE, Key::PageUp),
                i.consume_key(M::NONE, Key::Home),
                i.consume_key(M::NONE, Key::End),
                i.consume_key(M::NONE, Key::Enter),
            )
        });
        if total == 0 {
            return;
        }
        let page = ((self.view.viewport_h / ROW_H) as usize).saturating_sub(1).max(1);
        let cur = self.view.selected;
        let target = match cur {
            None if down || up || pgdn || pgup || home || end => Some(0),
            None => None,
            Some(c) if down => Some((c + 1).min(total - 1)),
            Some(c) if up => Some(c.saturating_sub(1)),
            Some(c) if pgdn => Some((c + page).min(total - 1)),
            Some(c) if pgup => Some(c.saturating_sub(page)),
            Some(_) if home => Some(0),
            Some(_) if end => Some(total - 1),
            Some(_) => None,
        };
        if enter {
            if let Some(row) =
                cur.and_then(|c| cx.cache.page(&self.query, c / GROUP_PAGE).and_then(|p| p.get(c % GROUP_PAGE)))
            {
                cx.actions.push(Action::ShowInTab(row.file));
            }
        }
        if let Some(t) = target {
            *select = Some(t);
            let top = t as f32 * ROW_H;
            let vh = self.view.viewport_h.max(ROW_H);
            if top < self.view.offset {
                self.view.scroll_to = Some(top);
            } else if top + ROW_H > self.view.offset + vh {
                self.view.scroll_to = Some(top + ROW_H - vh);
            }
        }
    }
}

fn inner(x: Rangef, row: Rect) -> Rect {
    Rect::from_x_y_ranges(Rangef::new(x.min + PAD, x.max - PAD), row.y_range())
}

fn header(ui: &mut Ui, rect: Rect, cols: Cols) {
    let p = ui.painter_at(rect);
    p.rect_filled(rect, CornerRadius::ZERO, colors::SURFACE);
    p.line_segment([rect.left_bottom(), rect.right_bottom()], Stroke::new(1.0, colors::BASELINE));
    let font = FontId::proportional(12.5);
    let titles: [(&str, Rangef, bool, &str); 5] = [
        (
            "Name (first copy)",
            cols.name,
            false,
            "Name of the first copy in tree order; other copies may be named differently",
        ),
        ("Each", cols.size, true, "Size of one copy"),
        ("Copies", cols.copies, true, "Copies anywhere"),
        ("Sources", cols.sources, true, "Machines, drives or cloud accounts holding a copy"),
        (
            "Extra copies",
            cols.wasted,
            true,
            "Bytes in every copy beyond one: what keeping a single copy would free. Sorted by this.",
        ),
    ];
    for (i, (t, x, right, tip)) in titles.into_iter().enumerate() {
        let cell = Rect::from_x_y_ranges(x, rect.y_range());
        let inner = if right {
            Rect::from_x_y_ranges(Rangef::new(x.min + 2.0, x.max - PAD), rect.y_range())
        } else {
            cell.shrink2(Vec2::new(PAD, 0.0))
        };
        let ink = if i == 4 { colors::INK } else { colors::INK_SECONDARY };
        if right {
            widgets::header_right(&p, inner, t, font.clone(), ink);
        } else {
            widgets::text_left(&p, inner, t, font.clone(), ink);
        }
        ui.interact(cell, ui.id().with(("ghdr", i)), Sense::hover()).on_hover_text(tip);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_tracks_pages_counts_and_generations() {
        let mut c = GroupCache::default();
        c.reset(3);
        let q = DupQuery { min_copies: 2, ..Default::default() };
        assert_eq!(c.count(&q), None);
        c.insert(3, q.clone(), 0, 1234, Some((1234, 99)), Ok(Arc::new(Vec::new())));
        assert_eq!(c.count(&q), Some(1234));
        assert_eq!(c.totals(&q), Some((1234, 99)));
        assert!(c.page(&q, 0).is_some());
        // a reply for an older index is ignored
        c.insert(2, q.clone(), 1, 1, None, Ok(Arc::new(Vec::new())));
        assert!(c.page(&q, 1).is_none());
        // a different query has its own pages
        let q2 = DupQuery { spread: Spread::OneSource, ..q.clone() };
        assert_eq!(c.count(&q2), None);
        c.insert(3, q2.clone(), 5, 7, None, Err("boom".into()));
        assert_eq!(c.error(&q2, 5), Some("boom"));
        c.reset(4);
        assert_eq!(c.count(&q), None);
    }

    #[test]
    fn columns_keep_the_right_edge_and_a_usable_name() {
        let c = GroupsPane::columns(0.0, 1000.0);
        assert_eq!(c.wasted.max, 1000.0);
        assert!(c.name.span() >= 400.0 - 0.01);
        let c = GroupsPane::columns(0.0, 500.0);
        assert!(c.name.span() >= 190.0);
        assert!(c.size.span() >= 72.0 - 0.01 && c.copies.span() >= 62.0 - 0.01);
        assert!((c.name.max - c.size.min).abs() < 0.01);
        // the number columns keep their room while Sources and Extra copies shrink
        let c = GroupsPane::columns(0.0, 760.0);
        assert!((c.size.span() - 84.0).abs() < 0.01 && (c.copies.span() - 66.0).abs() < 0.01);
        assert!(c.sources.span() < 130.0 && c.wasted.span() < 180.0);
    }
}
