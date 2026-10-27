//! The summary tab: what kinds of files one source (or every source) holds, by category and by
//! extension, and how much of each is duplicated. Every amount is split three ways:
//! * **Elsewhere**: the file has a copy in another source (solid orange),
//! * **Same source**: it has copies, but only within its own source (striped orange),
//! * **Unique**: the only copy anywhere (grey).

use crate::actions::Action;
use crate::colors;
use crate::format;
use crate::tree::Root;
use crate::widgets;
use crate::worker::{Query, Worker};
use dupemap_core::index::Index;
use dupemap_core::summary::{Categories, ExtRow, Split, Summary};
use egui::{Color32, CornerRadius, FontId, Painter, Pos2, Rangef, Rect, RichText, Sense, Stroke, Ui, Vec2};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

const ROW_H: f32 = 24.0;
const HEADER_H: f32 = 24.0;
const PAD: f32 = 8.0;
const BAR_H: f32 = 10.0;
/// Extensions listed before "Show all".
const TOP_EXTS: usize = 20;
/// Extensions listed under an open category before "N more".
const CATEGORY_EXTS: usize = 12;
/// Bars for files without another copy anywhere.
pub const UNIQUE: Color32 = Color32::from_rgb(0x6b, 0x6a, 0x66);

/// Summaries for the current index and categories, shared by every summary tab.
#[derive(Default)]
pub struct SummaryCache {
    generation: u64,
    version: u64,
    done: HashMap<Option<i64>, Arc<Summary>>,
    pending: HashSet<Option<i64>>,
}

impl SummaryCache {
    /// Start over for a new index or new categories.
    pub fn reset(&mut self, generation: u64, version: u64) {
        *self = SummaryCache { generation, version, ..Default::default() };
    }

    pub fn get(&self, scope: Option<i64>) -> Option<&Arc<Summary>> {
        self.done.get(&scope)
    }

    pub fn is_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    pub fn request(&mut self, worker: &Worker, scope: Option<i64>, categories: &Arc<Categories>) {
        if self.done.contains_key(&scope) || !self.pending.insert(scope) {
            return;
        }
        worker.send(Query::Summary {
            generation: self.generation,
            scope,
            categories: categories.clone(),
            version: self.version,
        });
    }

    pub fn insert(&mut self, generation: u64, scope: Option<i64>, version: u64, summary: Arc<Summary>) {
        if generation != self.generation || version != self.version {
            return;
        }
        self.pending.remove(&scope);
        self.done.insert(scope, summary);
    }
}

/// Column a summary tab is sorted by.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SumKey {
    Name,
    #[default]
    Size,
    Elsewhere,
    Within,
    Unique,
    Files,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SummaryPane {
    pub root: Root,
    sort: SumKey,
    descending: bool,
    /// Categories opened to show their extensions, by name.
    expanded: BTreeSet<String>,
    /// Every extension in the extension table, not just the largest.
    all_exts: bool,
}

pub struct SummaryCtx<'a> {
    pub ix: &'a Arc<Index>,
    pub cache: &'a mut SummaryCache,
    pub categories: &'a Arc<Categories>,
    pub worker: &'a Worker,
    pub actions: &'a mut Vec<Action>,
    pub pane_id: u64,
    pub focused: bool,
}

/// One table line.
enum Line<'a> {
    Source {
        db_id: i64,
        label: String,
        split: &'a Split,
    },
    Category {
        name: &'a str,
        split: &'a Split,
        open: bool,
        has_exts: bool,
    },
    Ext {
        row: &'a ExtRow,
        nested: bool,
    },
    /// "N more extensions" under an open category.
    More {
        files: u64,
        bytes: u64,
        count: usize,
    },
}

impl Line<'_> {
    fn split(&self) -> Option<&Split> {
        match self {
            Line::Source { split, .. } | Line::Category { split, .. } => Some(split),
            Line::Ext { row, .. } => Some(&row.split),
            Line::More { .. } => None,
        }
    }
}

/// Columns: name, size, bar, then the three amounts and the file count. A narrow tab drops
/// Files, then Unique, then Same source, then Elsewhere; the bar always stays.
#[derive(Clone, Copy)]
struct Cols {
    name: Rangef,
    size: Rangef,
    bar: Rangef,
    elsewhere: Rangef,
    within: Rangef,
    unique: Rangef,
    files: Rangef,
    /// elsewhere, within, unique, files
    shown: [bool; 4],
}

impl Cols {
    fn new(left: f32, right: f32) -> Cols {
        const SIZE_W: f32 = 84.0;
        const BAR_MIN: f32 = 60.0;
        const BAR_MAX: f32 = 320.0;
        const NAME_MIN: f32 = 140.0;
        const NAME_PREF_MAX: f32 = 280.0;
        let pref = [92.0f32, 100.0, 84.0, 76.0];
        let min = [80.0f32, 92.0, 72.0, 64.0];
        let width = right - left;
        let mut shown = [true; 4];
        for k in [3, 2, 1, 0] {
            let need = NAME_MIN + SIZE_W + BAR_MIN + (0..4).filter(|&i| shown[i]).map(|i| min[i]).sum::<f32>();
            if need <= width {
                break;
            }
            shown[k] = false;
        }
        let mut w = [0.0f32; 4];
        let mut m = [0.0f32; 4];
        for i in 0..4 {
            if shown[i] {
                w[i] = pref[i];
                m[i] = min[i];
            }
        }
        let room = (width - NAME_MIN - SIZE_W - BAR_MIN).max(0.0);
        if widgets::shrink_columns(&mut w, &m, room) > 0.0 {
            widgets::scale_columns(&mut w, room);
        }
        let numbers: f32 = w.iter().sum();
        // what is left goes to the name and the bar: the name up to a comfortable width, the
        // bar the rest (up to its maximum, then the name again)
        let rest = (width - SIZE_W - numbers).max(0.0);
        let mut name = (rest * 0.45).clamp(NAME_MIN.min(rest), NAME_PREF_MAX.max(NAME_MIN));
        let mut bar = rest - name;
        if bar < BAR_MIN {
            bar = BAR_MIN.min(rest);
            name = rest - bar;
        } else if bar > BAR_MAX {
            bar = BAR_MAX;
            name = rest - bar;
        }
        let mut x = left;
        let mut take = |w: f32| {
            let r = Rangef::new(x, x + w);
            x += w;
            r
        };
        let name = take(name);
        let size = take(SIZE_W);
        let bar = take(bar);
        let elsewhere = take(w[0]);
        let within = take(w[1]);
        let unique = take(w[2]);
        let files = take(w[3]);
        Cols { name, size, bar, elsewhere, within, unique, files, shown }
    }
}

impl SummaryPane {
    pub fn new(root: Root) -> Self {
        SummaryPane { root, sort: SumKey::Size, descending: true, expanded: BTreeSet::new(), all_exts: false }
    }

    pub fn title(&self, ix: Option<&Index>) -> String {
        match (self.root, ix) {
            (Root::AllSources, _) => "Summary: all sources".into(),
            (Root::Source(id), Some(ix)) => match ix.source_ix(id) {
                Some(s) => format!("Summary: {}", format::host(&ix.sources()[s as usize].host)),
                None => "Summary: (removed source)".into(),
            },
            (Root::Source(_), None) => "Summary".into(),
        }
    }

    /// Open the category with this name (the screenshot sequence shows one).
    pub fn expand(&mut self, name: &str) {
        self.expanded.insert(name.to_string());
    }

    pub fn ui(&mut self, ui: &mut Ui, cx: &mut SummaryCtx) {
        let full = ui.available_rect_before_wrap();
        if ui.rect_contains_pointer(full) && ui.input(|i| i.pointer.any_pressed()) && !cx.focused {
            cx.actions.push(Action::Focus(cx.pane_id));
        }
        self.toolbar(ui, cx);
        let scope = match self.root {
            Root::AllSources => None,
            Root::Source(id) => Some(id),
        };
        let Some(sum) = cx.cache.get(scope).cloned() else {
            cx.cache.request(cx.worker, scope, cx.categories);
            ui.add_space(20.0);
            ui.vertical_centered(|ui| {
                ui.add(egui::Spinner::new());
                ui.label(RichText::new("Adding up file types…").color(colors::INK_MUTED));
            });
            return;
        };
        ui.add_space(4.0);
        egui::ScrollArea::vertical().id_salt(("summary", cx.pane_id)).auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            self.headline(ui, &sum);
            if sum.total.files == 0 {
                return;
            }
            if scope.is_none() {
                self.section(ui, "By source", None);
                let lines: Vec<Line> = self.source_lines(cx.ix, &sum);
                self.table(ui, "Source", &lines, cx);
            }
            self.section(
                ui,
                "By category",
                Some("Click a category for its extensions. Right-click to edit categories."),
            );
            let lines = self.category_lines(&sum);
            self.table(ui, "Category", &lines, cx);
            self.section(ui, "By extension", None);
            let (lines, hidden) = self.ext_lines(&sum);
            self.table(ui, "Extension", &lines, cx);
            if hidden > 0 || self.all_exts {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.add_space(6.0);
                    let text = if self.all_exts {
                        format!("Show the largest {TOP_EXTS} only")
                    } else {
                        format!("Show all {} extensions", sum.exts.len())
                    };
                    if ui.link(text).clicked() {
                        self.all_exts = !self.all_exts;
                    }
                });
            }
            ui.add_space(12.0);
        });
    }

    fn toolbar(&mut self, ui: &mut Ui, cx: &mut SummaryCtx) {
        ui.horizontal_wrapped(|ui| {
            ui.add_space(4.0);
            ui.label(RichText::new("Source").color(colors::INK_MUTED));
            let current = match self.root {
                Root::AllSources => "All sources".to_string(),
                Root::Source(id) => match cx.ix.source_ix(id) {
                    Some(s) => format::host(&cx.ix.sources()[s as usize].host).to_string(),
                    None => "(removed source)".to_string(),
                },
            };
            egui::ComboBox::from_id_salt(("sum-root", cx.pane_id))
                .selected_text(current)
                .width((ui.available_width() - 150.0).clamp(110.0, 240.0))
                .height(480.0)
                .truncate()
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.root, Root::AllSources, "All sources");
                    ui.separator();
                    for s in cx.ix.source_summaries() {
                        let text = format!("{}   {}", format::host(&s.host), format::bytes(s.bytes));
                        ui.selectable_value(&mut self.root, Root::Source(s.db_id), text);
                    }
                });
            if ui.button("Categories…").on_hover_text("Choose which extensions make up each category").clicked() {
                cx.actions.push(Action::OpenCategories);
            }
            if cx.cache.is_pending() {
                ui.add(egui::Spinner::new().size(12.0));
            }
        });
    }

    /// Total size and a full-width bar split three ways, labelled directly (it doubles as the
    /// legend for every bar below).
    fn headline(&self, ui: &mut Ui, sum: &Summary) {
        let t = &sum.total;
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.add_space(6.0);
            ui.label(RichText::new(format::bytes(t.bytes)).size(20.0).strong().color(colors::INK));
            ui.label(
                RichText::new(format!(
                    "in {} · only files the scripts hashed (1 MB and larger)",
                    format::plural(t.files, "file", "files")
                ))
                .color(colors::INK_SECONDARY),
            );
        });
        if t.files == 0 {
            ui.add_space(8.0);
            ui.label(RichText::new("    Nothing here yet.").color(colors::INK_MUTED));
            return;
        }
        ui.add_space(6.0);
        let w = (ui.available_width() - 12.0).max(40.0);
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 16.0), Sense::hover());
        let bar = Rect::from_min_size(Pos2::new(rect.left() + 6.0, rect.top() + 2.0), Vec2::new(w, 12.0));
        stacked_bar(ui.painter(), bar, t, t.bytes, 12.0);
        resp.on_hover_ui(|ui| split_tooltip(ui, "Everything here", t));
        ui.add_space(6.0);
        // direct labels for the three parts, which double as the legend for every bar below;
        // one line when there is room, otherwise one line each (a label never leaves its swatch)
        let pct = |b: u64| format::percent(if t.bytes == 0 { 0.0 } else { b as f64 * 100.0 / t.bytes as f64 });
        let items = [
            (
                Kind::Elsewhere,
                "Elsewhere",
                t.elsewhere_bytes,
                "A copy exists in another source (another machine, drive or cloud account)",
            ),
            (Kind::Within, "Same source", t.within_bytes, "Copies exist, but only within the file's own source"),
            (Kind::Unique, "Unique", t.unique_bytes, "The only copy anywhere"),
        ];
        let item = |ui: &mut Ui, (kind, label, bytes, tip): (Kind, &str, u64, &str)| {
            let (r, resp) = ui.allocate_exact_size(Vec2::new(14.0, 14.0), Sense::hover());
            swatch(ui.painter(), r.shrink(1.0), kind);
            resp.on_hover_text(tip);
            ui.label(RichText::new(format!("{label} {} ({})", format::bytes(bytes), pct(bytes))).color(colors::INK))
                .on_hover_text(tip);
        };
        if ui.available_width() >= 620.0 {
            ui.horizontal(|ui| {
                ui.add_space(6.0);
                for it in items {
                    item(ui, it);
                    ui.add_space(10.0);
                }
            });
        } else {
            for it in items {
                ui.horizontal(|ui| {
                    ui.add_space(6.0);
                    item(ui, it);
                });
                ui.add_space(2.0);
            }
        }
        ui.add_space(4.0);
    }

    fn section(&self, ui: &mut Ui, title: &str, note: Option<&str>) {
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            ui.add_space(6.0);
            ui.label(RichText::new(title).strong().size(15.0).color(colors::INK));
            if let Some(n) = note {
                ui.label(RichText::new(n).small().color(colors::INK_MUTED));
            }
        });
        ui.add_space(4.0);
    }

    fn cmp(&self, a: (&str, &Split), b: (&str, &Split)) -> std::cmp::Ordering {
        let key = |s: &Split| match self.sort {
            SumKey::Name | SumKey::Size => s.bytes,
            SumKey::Elsewhere => s.elsewhere_bytes,
            SumKey::Within => s.within_bytes,
            SumKey::Unique => s.unique_bytes,
            SumKey::Files => s.files,
        };
        let o = match self.sort {
            SumKey::Name => dupemap_core::natsort::natural_cmp(a.0, b.0),
            _ => key(a.1).cmp(&key(b.1)).then_with(|| b.0.cmp(a.0)),
        };
        if self.descending {
            o.reverse()
        } else {
            o
        }
    }

    fn source_lines<'a>(&self, ix: &Index, sum: &'a Summary) -> Vec<Line<'a>> {
        let mut v: Vec<(String, i64, &Split)> = sum
            .sources
            .iter()
            .filter(|(_, sp)| sp.files > 0)
            .map(|(s, sp)| {
                let src = &ix.sources()[*s as usize];
                (format::host(&src.host).to_string(), src.db_id, sp)
            })
            .collect();
        v.sort_by(|a, b| self.cmp((&a.0, a.2), (&b.0, b.2)));
        v.into_iter().map(|(label, db_id, split)| Line::Source { db_id, label, split }).collect()
    }

    fn category_lines<'a>(&self, sum: &'a Summary) -> Vec<Line<'a>> {
        let mut cats: Vec<usize> = (0..sum.names.len()).filter(|&i| sum.categories[i].files > 0).collect();
        cats.sort_by(|&a, &b| self.cmp((&sum.names[a], &sum.categories[a]), (&sum.names[b], &sum.categories[b])));
        let mut out = Vec::new();
        for c in cats {
            let open = self.expanded.contains(&sum.names[c]);
            let mut exts: Vec<&ExtRow> = sum.exts.iter().filter(|e| e.category == c).collect();
            out.push(Line::Category {
                name: &sum.names[c],
                split: &sum.categories[c],
                open,
                has_exts: !exts.is_empty(),
            });
            if open {
                exts.sort_by(|a, b| {
                    self.cmp((ext_label(&a.ext).as_str(), &a.split), (ext_label(&b.ext).as_str(), &b.split))
                });
                let shown = exts.len().min(CATEGORY_EXTS);
                for e in &exts[..shown] {
                    out.push(Line::Ext { row: e, nested: true });
                }
                if exts.len() > shown {
                    let rest = &exts[shown..];
                    out.push(Line::More {
                        files: rest.iter().map(|e| e.split.files).sum(),
                        bytes: rest.iter().map(|e| e.split.bytes).sum(),
                        count: rest.len(),
                    });
                }
            }
        }
        out
    }

    /// The extension table's lines, and how many extensions it leaves out.
    fn ext_lines<'a>(&self, sum: &'a Summary) -> (Vec<Line<'a>>, usize) {
        let mut exts: Vec<&ExtRow> = sum.exts.iter().collect();
        exts.sort_by(|a, b| self.cmp((ext_label(&a.ext).as_str(), &a.split), (ext_label(&b.ext).as_str(), &b.split)));
        let n = if self.all_exts { exts.len() } else { exts.len().min(TOP_EXTS) };
        let hidden = exts.len() - n;
        (exts[..n].iter().map(|e| Line::Ext { row: e, nested: false }).collect(), hidden)
    }

    fn table(&mut self, ui: &mut Ui, first: &str, lines: &[Line], cx: &mut SummaryCtx) {
        let full = ui.available_rect_before_wrap().intersect(ui.clip_rect());
        let cols = Cols::new(full.left(), full.right());
        let (hrect, _) = ui.allocate_exact_size(Vec2::new(full.width(), HEADER_H), Sense::hover());
        self.header(ui, hrect, cols, first, cx.pane_id);
        // bars share one scale per table: the largest top-level line fills the column
        let max = lines
            .iter()
            .filter(|l| !matches!(l, Line::Ext { nested: true, .. } | Line::More { .. }))
            .filter_map(|l| l.split().map(|s| s.bytes))
            .max()
            .unwrap_or(1)
            .max(1);
        for (i, line) in lines.iter().enumerate() {
            let (rect, resp) = ui.allocate_exact_size(Vec2::new(full.width(), ROW_H), Sense::click());
            if !ui.is_rect_visible(rect) {
                continue;
            }
            self.row(ui, rect, resp, i, line, cols, max, cx);
        }
    }

    fn header(&mut self, ui: &mut Ui, rect: Rect, cols: Cols, first: &str, pane_id: u64) {
        let p = ui.painter_at(rect);
        p.rect_filled(rect, CornerRadius::ZERO, colors::SURFACE);
        p.line_segment([rect.left_bottom(), rect.right_bottom()], Stroke::new(1.0, colors::BASELINE));
        let font = FontId::proportional(12.5);
        let titles: [(&str, Rangef, Option<SumKey>, bool, &str); 7] = [
            (first, cols.name, Some(SumKey::Name), false, "Click to sort by name"),
            ("Size", cols.size, Some(SumKey::Size), true, "Total size of the hashed files"),
            ("", cols.bar, None, false, "Size, split into elsewhere / same source / unique"),
            (
                "Elsewhere",
                cols.elsewhere,
                Some(SumKey::Elsewhere),
                true,
                "Bytes of files with a copy in another source",
            ),
            (
                "Same source",
                cols.within,
                Some(SumKey::Within),
                true,
                "Bytes of files whose copies are all in their own source",
            ),
            ("Unique", cols.unique, Some(SumKey::Unique), true, "Bytes of files that are the only copy anywhere"),
            ("Files", cols.files, Some(SumKey::Files), true, "Number of files"),
        ];
        for (i, (title, x, key, right, tip)) in titles.into_iter().enumerate() {
            if i >= 3 && !cols.shown[i - 3] {
                continue;
            }
            let cell = Rect::from_x_y_ranges(x, rect.y_range());
            let resp = ui.interact(cell, ui.id().with(("sum-hdr", pane_id, first, i)), Sense::click());
            if resp.hovered() && key.is_some() {
                p.rect_filled(cell, CornerRadius::ZERO, colors::SURFACE_RAISED);
            }
            let active = key.is_some() && key == Some(self.sort);
            let ink = if active { colors::INK } else { colors::INK_SECONDARY };
            let inner = if right {
                Rect::from_x_y_ranges(Rangef::new(x.min + 2.0, x.max - PAD), rect.y_range())
            } else {
                cell.shrink2(Vec2::new(PAD, 0.0))
            };
            let tr = if right {
                widgets::header_right(&p, inner, title, font.clone(), ink)
            } else {
                widgets::text_left(&p, inner, title, font.clone(), ink)
            };
            if active {
                let c = if right {
                    Pos2::new(tr.left() - 8.0, rect.center().y)
                } else {
                    Pos2::new(tr.right() + 8.0, rect.center().y)
                };
                widgets::sort_arrow(&p, c, self.descending, ink);
            }
            let resp = resp.on_hover_text(tip);
            if let Some(k) = key {
                if resp.clicked() {
                    if self.sort == k {
                        self.descending = !self.descending;
                    } else {
                        self.sort = k;
                        self.descending = k != SumKey::Name;
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn row(
        &mut self,
        ui: &mut Ui,
        rect: Rect,
        resp: egui::Response,
        i: usize,
        line: &Line,
        cols: Cols,
        max: u64,
        cx: &mut SummaryCtx,
    ) {
        let p = ui.painter_at(rect);
        if resp.hovered() {
            p.rect_filled(rect, CornerRadius::ZERO, colors::ROW_HOVER);
        } else if i % 2 == 1 {
            p.rect_filled(rect, CornerRadius::ZERO, colors::ROW_STRIPE);
        }
        let font = FontId::proportional(13.0);
        let small = FontId::proportional(11.5);
        let name_r = Rect::from_x_y_ranges(Rangef::new(cols.name.min + PAD, cols.name.max - PAD), rect.y_range());
        match line {
            Line::Source { label, .. } => {
                widgets::text_left(&p, name_r, label, font.clone(), colors::LINK);
            }
            Line::Category { name, open, has_exts, .. } => {
                let exp = Rect::from_min_size(name_r.min, Vec2::new(14.0, ROW_H));
                if *has_exts {
                    widgets::expander(&p, exp, *open, colors::INK_SECONDARY);
                }
                let r = Rect::from_x_y_ranges(Rangef::new(exp.right() + 4.0, name_r.right()), rect.y_range());
                widgets::text_left(&p, r, name, font.clone(), colors::INK);
            }
            Line::Ext { row, nested } => {
                let indent = if *nested { 18.0 } else { 0.0 };
                let r = Rect::from_x_y_ranges(Rangef::new(name_r.left() + indent, name_r.right()), rect.y_range());
                let label = ext_label(&row.ext);
                let tr = widgets::text_left(&p, r, &label, font.clone(), colors::INK);
                if !*nested {
                    // which category it counts under
                    let cat = Rect::from_x_y_ranges(Rangef::new(tr.right() + 8.0, name_r.right()), rect.y_range());
                    if cat.width() > 30.0 {
                        let name = category_name(cx.categories, row.category);
                        widgets::text_left(&p, cat, &name, small.clone(), colors::INK_MUTED);
                    }
                }
            }
            Line::More { files, bytes, count } => {
                let r = Rect::from_x_y_ranges(Rangef::new(name_r.left() + 18.0, name_r.right()), rect.y_range());
                let t = format!(
                    "{} more ({} · {})",
                    format::plural(*count as u64, "extension", "extensions"),
                    format::plural(*files, "file", "files"),
                    format::bytes(*bytes)
                );
                widgets::text_left(&p, r, &t, small.clone(), colors::INK_MUTED);
            }
        }
        if let Some(sp) = line.split() {
            let inner = |x: Rangef| Rect::from_x_y_ranges(Rangef::new(x.min + PAD, x.max - PAD), rect.y_range());
            widgets::text_right(&p, inner(cols.size), &format::bytes(sp.bytes), font.clone(), colors::INK);
            let bar = Rect::from_x_y_ranges(
                Rangef::new(cols.bar.min + PAD, cols.bar.max - PAD),
                Rangef::new(rect.center().y - BAR_H / 2.0, rect.center().y + BAR_H / 2.0),
            );
            if bar.width() >= 8.0 {
                stacked_bar(&p, bar, sp, max, BAR_H);
            }
            let amount = |x: Rangef, b: u64| {
                let (t, ink) = if b == 0 {
                    ("–".to_string(), colors::INK_MUTED)
                } else {
                    (format::bytes(b), colors::INK_SECONDARY)
                };
                widgets::text_right(&p, inner(x), &t, font.clone(), ink);
            };
            if cols.shown[0] {
                amount(cols.elsewhere, sp.elsewhere_bytes);
            }
            if cols.shown[1] {
                amount(cols.within, sp.within_bytes);
            }
            if cols.shown[2] {
                amount(cols.unique, sp.unique_bytes);
            }
            if cols.shown[3] {
                widgets::text_right(
                    &p,
                    inner(cols.files),
                    &format::count(sp.files),
                    font.clone(),
                    colors::INK_SECONDARY,
                );
            }
        }

        // interaction
        let title = match line {
            Line::Source { label, .. } => label.clone(),
            Line::Category { name, .. } => name.to_string(),
            Line::Ext { row, .. } => ext_label(&row.ext),
            Line::More { .. } => String::new(),
        };
        let resp =
            if let Some(sp) = line.split() { resp.on_hover_ui(|ui| split_tooltip(ui, &title, sp)) } else { resp };
        match line {
            Line::Source { db_id, .. } => {
                if resp.clicked() {
                    self.root = Root::Source(*db_id);
                }
                resp.on_hover_cursor(egui::CursorIcon::PointingHand).context_menu(|ui| {
                    if ui.button("Summary of this source").clicked() {
                        self.root = Root::Source(*db_id);
                        ui.close();
                    }
                    if ui.button("Open in a new tab").clicked() {
                        cx.actions.push(Action::NewTab(Root::Source(*db_id)));
                        ui.close();
                    }
                });
            }
            Line::Category { name, has_exts, .. } => {
                if resp.clicked() && *has_exts && !self.expanded.remove(*name) {
                    self.expanded.insert(name.to_string());
                }
                resp.context_menu(|ui| {
                    if ui.button("Edit categories…").clicked() {
                        cx.actions.push(Action::OpenCategories);
                        ui.close();
                    }
                });
            }
            Line::Ext { row, .. } => {
                resp.context_menu(|ui| {
                    let label = ext_label(&row.ext);
                    if row.ext.is_empty() {
                        ui.label(RichText::new("Files without an extension stay in Other").color(colors::INK_MUTED));
                        return;
                    }
                    ui.label(RichText::new(format!("Count {label} as")).color(colors::INK_MUTED));
                    let names = cx.categories.names();
                    for (k, n) in names.iter().enumerate() {
                        let current = k == row.category;
                        if ui.add_enabled(!current, egui::Button::new(n.as_str())).clicked() {
                            cx.actions.push(Action::SetCategories(move_ext(cx.categories, &row.ext, k)));
                            ui.close();
                        }
                    }
                    ui.separator();
                    if ui.button("Edit categories…").clicked() {
                        cx.actions.push(Action::OpenCategories);
                        ui.close();
                    }
                });
            }
            Line::More { .. } => {}
        }
    }
}

/// `.mkv`, or "(no extension)".
pub fn ext_label(ext: &str) -> String {
    if ext.is_empty() {
        "(no extension)".into()
    } else {
        format!(".{ext}")
    }
}

fn category_name(cats: &Categories, i: usize) -> String {
    cats.names().get(i).cloned().unwrap_or_default()
}

/// The mapping with `ext` moved to category `to` (`cats.list.len()` = Other).
pub fn move_ext(cats: &Categories, ext: &str, to: usize) -> Categories {
    let mut c = cats.clone();
    for cat in &mut c.list {
        cat.exts.retain(|e| e != ext);
    }
    if let Some(cat) = c.list.get_mut(to) {
        cat.exts.push(ext.to_string());
    }
    c
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Elsewhere,
    Within,
    Unique,
}

/// A segment fill: solid orange, striped orange or grey.
fn fill(p: &Painter, r: Rect, kind: Kind, radius: CornerRadius) {
    match kind {
        Kind::Elsewhere => {
            p.rect_filled(r, radius, colors::DUP);
        }
        Kind::Within => {
            // orange stripes at 45 degrees on a faint orange ground: same family, clearly different
            p.rect_filled(r, radius, colors::alpha(colors::DUP, 0.28));
            let clip = p.with_clip_rect(r.intersect(p.clip_rect()));
            let step = 4.0;
            let mut x = r.left() - r.height();
            while x < r.right() {
                clip.line_segment(
                    [Pos2::new(x, r.bottom()), Pos2::new(x + r.height(), r.top())],
                    Stroke::new(1.6, colors::DUP),
                );
                x += step;
            }
        }
        Kind::Unique => {
            p.rect_filled(r, radius, UNIQUE);
        }
    }
}

fn swatch(p: &Painter, r: Rect, kind: Kind) {
    fill(p, r, kind, CornerRadius::same(2));
}

/// A bar `max`-scaled to `rect`'s width, split into elsewhere / same source / unique with a
/// 2 px gap between parts and rounded outer ends.
fn stacked_bar(p: &Painter, rect: Rect, s: &Split, max: u64, h: f32) {
    if s.bytes == 0 || max == 0 {
        return;
    }
    let total_w = (rect.width() * (s.bytes as f64 / max as f64) as f32).max(2.0);
    let parts = [(Kind::Elsewhere, s.elsewhere_bytes), (Kind::Within, s.within_bytes), (Kind::Unique, s.unique_bytes)];
    let visible: Vec<(Kind, f32)> =
        parts.iter().filter(|(_, b)| *b > 0).map(|&(k, b)| (k, total_w * (b as f64 / s.bytes as f64) as f32)).collect();
    let n = visible.len();
    let y = rect.center().y - h / 2.0;
    let mut x = rect.left();
    for (i, (kind, w)) in visible.into_iter().enumerate() {
        let gap = if i + 1 < n { 2.0 } else { 0.0 };
        let seg_w = (w - gap).max(1.0);
        let r = Rect::from_min_size(Pos2::new(x, y), Vec2::new(seg_w, h));
        let radius = CornerRadius {
            nw: if i == 0 { 2 } else { 0 },
            sw: if i == 0 { 2 } else { 0 },
            ne: if i + 1 == n { 2 } else { 0 },
            se: if i + 1 == n { 2 } else { 0 },
        };
        fill(p, r, kind, radius);
        x += w;
    }
}

fn split_tooltip(ui: &mut Ui, title: &str, s: &Split) {
    ui.label(RichText::new(title).strong());
    ui.label(format!("{} · {}", format::plural(s.files, "file", "files"), format::bytes(s.bytes)));
    let pct = |b: u64| format::percent(if s.bytes == 0 { 0.0 } else { b as f64 * 100.0 / s.bytes as f64 });
    for (kind, label, files, bytes) in [
        (Kind::Elsewhere, "Copy in another source", s.elsewhere_files, s.elsewhere_bytes),
        (Kind::Within, "Copies only in its own source", s.within_files, s.within_bytes),
        (Kind::Unique, "Unique", s.unique_files, s.unique_bytes),
    ] {
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(Vec2::new(12.0, 12.0), Sense::hover());
            swatch(ui.painter(), r, kind);
            ui.label(format!(
                "{label}: {} ({}) · {}",
                format::bytes(bytes),
                pct(bytes),
                format::plural(files, "file", "files")
            ));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_fill_the_width_and_drop_in_order() {
        let c = Cols::new(0.0, 1200.0);
        assert_eq!(c.shown, [true; 4]);
        assert!((c.files.max - 1200.0).abs() < 0.01);
        assert!(c.bar.span() <= 320.0 + 0.01 && c.bar.span() >= 60.0);
        assert!(c.name.span() >= 140.0);
        // narrower: Files, then Unique, then Same source, then Elsewhere
        assert_eq!(Cols::new(0.0, 700.0).shown, [true; 4]);
        assert_eq!(Cols::new(0.0, 560.0).shown, [true, true, true, false]);
        assert_eq!(Cols::new(0.0, 500.0).shown, [true, true, false, false]);
        assert_eq!(Cols::new(0.0, 420.0).shown, [true, false, false, false]);
        assert_eq!(Cols::new(0.0, 300.0).shown, [false, false, false, false]);
        let c = Cols::new(0.0, 300.0);
        assert!(c.bar.span() >= 60.0 - 0.01 && c.name.span() >= 140.0 - 0.01);
        // a half-width tab: the bar gets real room, not just its minimum
        let c = Cols::new(0.0, 700.0);
        assert!(c.bar.span() >= 100.0, "{}", c.bar.span());
    }

    #[test]
    fn moving_an_extension_between_categories() {
        let cats = Categories::default();
        let iso = cats.category_of("iso");
        let moved = move_ext(&cats, "iso", 0);
        assert_eq!(moved.category_of("iso"), 0);
        assert_ne!(iso, 0);
        let other = move_ext(&cats, "iso", cats.list.len());
        assert_eq!(other.category_of("iso"), cats.list.len(), "moved to Other = in no category");
    }

    #[test]
    fn cache_keeps_current_results_only() {
        let mut c = SummaryCache::default();
        c.reset(2, 5);
        let s = Arc::new(Summary::empty(&Categories::default()));
        c.insert(1, None, 5, s.clone());
        assert!(c.get(None).is_none(), "older index");
        c.insert(2, None, 4, s.clone());
        assert!(c.get(None).is_none(), "older categories");
        c.insert(2, None, 5, s);
        assert!(c.get(None).is_some());
    }
}
