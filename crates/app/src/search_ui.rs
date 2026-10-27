//! The search tab: files and folders by name, or files by hash, across every source.

use crate::actions::Action;
use crate::colors;
use crate::format;
use crate::selection::{Linked, Target};
use crate::widgets;
use crate::worker::{Query, Worker};
use dupemap_core::index::{Filter, Index, SizeState};
use dupemap_core::search::{
    looks_like_sha256, Hit, SearchMode, SearchQuery, SearchResult, SearchWhat, MAX_HITS, MIN_HASH_DIGITS,
};
use egui::{CornerRadius, FontId, Key, Pos2, Rangef, Rect, RichText, ScrollArea, Sense, Stroke, Ui, Vec2};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

const ROW_H: f32 = 22.0;
const HEADER_H: f32 = 24.0;
const PAD: f32 = 8.0;
/// A pause in typing this long starts the search.
const TYPING_PAUSE: f64 = 0.45;

static NEXT_SEARCH: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SearchSort {
    Name,
    Folder,
    Source,
    Size,
    Copies,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SearchPane {
    /// What is typed (and chosen) in the toolbar.
    pub draft: SearchQuery,
    /// The search the results belong to.
    pub query: Option<SearchQuery>,
    sort: SearchSort,
    descending: bool,
    #[serde(skip)]
    view: View,
}

#[derive(Clone, Default)]
struct View {
    generation: u64,
    /// Id of the search sent and not answered yet.
    pending: Option<u64>,
    result: Option<Arc<SearchResult>>,
    error: Option<String>,
    elapsed_ms: u64,
    /// Display order (indices into the hits) and what it was sorted for.
    order: Vec<u32>,
    order_key: Option<(SearchSort, bool, usize)>,
    selected: Option<usize>,
    offset: f32,
    viewport_h: f32,
    scroll_to: Option<f32>,
    /// When the text was last changed (the search starts after a pause).
    edited_at: Option<f64>,
    focus_field: bool,
}

pub struct SearchCtx<'a> {
    pub ix: &'a Arc<Index>,
    pub generation: u64,
    pub worker: &'a Worker,
    pub actions: &'a mut Vec<Action>,
    pub pane_id: u64,
    pub focused: bool,
    pub linked: Option<&'a Linked>,
    /// Marks for deletion: the rules, and what they mark (`None` while worked out).
    pub book: &'a crate::marks_ui::MarkBook,
    pub marks: Option<&'a dupemap_core::marks::MarkState>,
}

#[derive(Clone, Copy)]
struct Cols {
    name: Rangef,
    folder: Rangef,
    source: Rangef,
    size: Rangef,
    copies: Rangef,
    /// Folder, Source, Copies fit (a narrow tab drops Source, then Copies, then Folder).
    shown: [bool; 3],
}

impl Default for SearchPane {
    fn default() -> Self {
        SearchPane {
            draft: SearchQuery::default(),
            query: None,
            sort: SearchSort::Size,
            descending: true,
            view: View { focus_field: true, ..Default::default() },
        }
    }
}

impl SearchPane {
    pub fn title(&self) -> String {
        match &self.query {
            Some(q) if !q.text.trim().is_empty() => {
                let t: String = q.text.trim().chars().take(24).collect();
                format!("Search: {t}")
            }
            _ => "Search".into(),
        }
    }

    /// Restored from a saved session: run the search again once there is an index.
    pub fn after_load(&mut self) {
        self.view.focus_field = false;
    }

    /// Put the cursor in the search field (Ctrl+F).
    pub fn focus(&mut self) {
        self.view.focus_field = true;
    }

    /// Search for this right away (the screenshots).
    pub fn set_text(&mut self, text: &str) {
        self.draft.text = text.to_string();
        self.view.edited_at = Some(-1.0);
        self.view.focus_field = false;
    }

    /// A search is waiting to start or for its results.
    pub fn is_pending(&self) -> bool {
        self.view.pending.is_some() || self.view.edited_at.is_some()
    }

    /// A reply from the search thread.
    pub fn on_result(&mut self, id: u64, generation: u64, result: Result<Arc<SearchResult>, String>, elapsed_ms: u64) {
        if self.view.pending != Some(id) || generation != self.view.generation {
            return;
        }
        self.view.pending = None;
        self.view.elapsed_ms = elapsed_ms;
        self.view.order_key = None;
        self.view.selected = None;
        self.view.scroll_to = Some(0.0);
        match result {
            Ok(r) => {
                self.view.result = Some(r);
                self.view.error = None;
            }
            Err(e) => {
                self.view.result = None;
                self.view.error = Some(e);
            }
        }
    }

    fn send(&mut self, cx: &SearchCtx, q: SearchQuery) {
        let id = NEXT_SEARCH.fetch_add(1, Ordering::Relaxed);
        self.view.pending = Some(id);
        self.query = Some(q.clone());
        cx.worker.send(Query::Search { pane: cx.pane_id, id, generation: cx.generation, query: q });
    }

    /// Enough typed to search for?
    fn ready(q: &SearchQuery) -> bool {
        let t = q.text.trim();
        match q.mode {
            SearchMode::Name => t.chars().count() >= 2 || t.contains('*') || t.contains('?'),
            SearchMode::Hash => {
                let digits = t.rsplit(':').next().unwrap_or(t);
                digits.chars().filter(|c| c.is_ascii_hexdigit()).count() >= MIN_HASH_DIGITS
            }
        }
    }

    pub fn ui(&mut self, ui: &mut Ui, cx: &mut SearchCtx) {
        if self.view.generation != cx.generation {
            // a new index: file numbers changed, so search again (what is being typed stays)
            let (focus, edited_at) = (self.view.focus_field, self.view.edited_at);
            self.view = View { generation: cx.generation, focus_field: focus, edited_at, ..Default::default() };
            if let Some(q) = self.query.clone() {
                self.send(cx, q);
            }
        }
        let full = ui.available_rect_before_wrap();
        if ui.rect_contains_pointer(full) && ui.input(|i| i.pointer.any_pressed()) && !cx.focused {
            cx.actions.push(Action::Focus(cx.pane_id));
        }
        self.toolbar(ui, cx);
        ui.add_space(4.0);
        let Some(result) = self.view.result.clone() else {
            ui.add_space(16.0);
            if self.view.pending.is_some() {
                ui.vertical_centered(|ui| {
                    ui.add(egui::Spinner::new());
                    ui.label(RichText::new("Searching every source…").color(colors::INK_MUTED));
                });
            } else if let Some(e) = &self.view.error {
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new(e).color(colors::INK_SECONDARY));
                });
            } else {
                help(ui);
            }
            return;
        };
        if result.hits.is_empty() {
            ui.add_space(16.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("Nothing found").color(colors::INK_SECONDARY));
            });
            return;
        }
        self.sort_rows(cx.ix, &result);
        let mut select = None;
        self.table(ui, cx, &result, &mut select);
        if cx.focused && ui.memory(|m| m.focused().is_none()) {
            self.keyboard(ui, cx, &result, &mut select);
        }
        if let Some(i) = select {
            self.select(cx, &result, i);
        }
    }

    fn toolbar(&mut self, ui: &mut Ui, cx: &mut SearchCtx) {
        let ix = cx.ix.clone();
        let pane = cx.pane_id;
        let now = ui.input(|i| i.time);
        let before = self.draft.clone();
        let mut enter = false;
        // the field and the "by" choice on one row, where to look on the next unless all fits
        let avail = ui.available_rect_before_wrap().intersect(ui.clip_rect()).width();
        let spacing = ui.spacing().item_spacing.x;
        let field_w = (avail - 90.0 - 3.0 * spacing - 16.0).clamp(140.0, 340.0);
        let one_row = avail >= field_w + 90.0 + 150.0 + 130.0 + 4.0 * spacing + 16.0;
        let mut field = |ui: &mut Ui, this: &mut SearchPane| {
            let hint = match this.draft.mode {
                SearchMode::Name => "Name or part of it; *.mkv, IMG_20??.jpg",
                SearchMode::Hash => "SHA-256, or its first digits",
            };
            let edit = ui.add(egui::TextEdit::singleline(&mut this.draft.text).hint_text(hint).desired_width(field_w));
            if std::mem::take(&mut this.view.focus_field) {
                edit.request_focus();
                // everything selected, so typing replaces the last search
                let mut st = egui::TextEdit::load_state(ui.ctx(), edit.id).unwrap_or_default();
                let n = this.draft.text.chars().count();
                st.cursor.set_char_range(Some(egui::text::CCursorRange::two(
                    egui::text::CCursor::new(0),
                    egui::text::CCursor::new(n),
                )));
                st.store(ui.ctx(), edit.id);
            }
            enter = edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            let mode_text = match this.draft.mode {
                SearchMode::Name => "By name",
                SearchMode::Hash => "By hash",
            };
            egui::ComboBox::from_id_salt(("s-mode", pane)).selected_text(mode_text).width(90.0).show_ui(ui, |ui| {
                ui.selectable_value(&mut this.draft.mode, SearchMode::Name, "By name");
                ui.selectable_value(&mut this.draft.mode, SearchMode::Hash, "By hash")
                    .on_hover_text("Every file with this hash: paste a whole SHA-256 or type its first digits");
            });
        };
        let scope = |ui: &mut Ui, this: &mut SearchPane| {
            let scope_text = match this.draft.scope.and_then(|id| ix.source_ix(id)) {
                Some(s) => format::host(&ix.sources()[s as usize].host).to_string(),
                None => "All sources".into(),
            };
            egui::ComboBox::from_id_salt(("s-scope", pane))
                .selected_text(scope_text)
                .width(150.0)
                .height(420.0)
                .truncate()
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut this.draft.scope, None, "All sources");
                    for s in ix.sources() {
                        ui.selectable_value(&mut this.draft.scope, Some(s.db_id), format::host(&s.host));
                    }
                });
            if this.draft.mode == SearchMode::Name {
                let what_text = match this.draft.what {
                    SearchWhat::FilesAndFolders => "Files and folders",
                    SearchWhat::Files => "Files",
                    SearchWhat::Folders => "Folders",
                };
                egui::ComboBox::from_id_salt(("s-what", pane)).selected_text(what_text).width(130.0).show_ui(
                    ui,
                    |ui| {
                        ui.selectable_value(&mut this.draft.what, SearchWhat::FilesAndFolders, "Files and folders");
                        ui.selectable_value(&mut this.draft.what, SearchWhat::Files, "Files");
                        ui.selectable_value(&mut this.draft.what, SearchWhat::Folders, "Folders");
                    },
                );
            }
        };
        if one_row {
            ui.horizontal(|ui| {
                ui.add_space(4.0);
                field(ui, self);
                scope(ui, self);
            });
        } else {
            ui.horizontal(|ui| {
                ui.add_space(4.0);
                field(ui, self);
            });
            ui.horizontal(|ui| {
                ui.add_space(4.0);
                scope(ui, self);
            });
        }
        // a pasted hash switches to a hash search
        if self.draft.text != before.text && self.draft.mode == SearchMode::Name && looks_like_sha256(&self.draft.text)
        {
            self.draft.mode = SearchMode::Hash;
        }
        if self.draft != before {
            self.view.edited_at = Some(now);
        }
        let changed_choice =
            self.draft.mode != before.mode || self.draft.scope != before.scope || self.draft.what != before.what;
        let paused = self.view.edited_at.is_some_and(|t| now - t >= TYPING_PAUSE);
        if let Some(t) = self.view.edited_at {
            if !paused {
                ui.ctx().request_repaint_after(std::time::Duration::from_secs_f64(TYPING_PAUSE - (now - t) + 0.02));
            }
        }
        let differs = self.query.as_ref() != Some(&self.draft);
        if (enter || changed_choice || paused) && differs {
            self.view.edited_at = None;
            if Self::ready(&self.draft) {
                self.send(cx, self.draft.clone());
            } else if enter {
                self.view.result = None;
                self.view.error = Some(match self.draft.mode {
                    SearchMode::Name => "Type at least two letters, or a pattern such as *.mkv".into(),
                    SearchMode::Hash => format!("Type at least {MIN_HASH_DIGITS} digits of the hash"),
                });
            }
        } else if paused {
            self.view.edited_at = None;
        }
        // what was found
        ui.horizontal(|ui| {
            ui.add_space(6.0);
            if let Some(r) = &self.view.result {
                let (mut files, mut dirs) = (0usize, 0usize);
                for h in &r.hits {
                    match h.hit {
                        Hit::File(_) => files += 1,
                        Hit::Dir(_) => dirs += 1,
                    }
                }
                let mut t = match (files, dirs) {
                    (f, 0) => format::plural(f as u64, "file", "files"),
                    (0, d) => format::plural(d as u64, "folder", "folders"),
                    (f, d) => format!(
                        "{} and {}",
                        format::plural(f as u64, "file", "files"),
                        format::plural(d as u64, "folder", "folders")
                    ),
                };
                if r.truncated {
                    t = format!(
                        "The first {} matches (narrow the search to see the rest)",
                        format::count(MAX_HITS as u64)
                    );
                }
                t.push_str(&format!(" · {:.1} s", self.view.elapsed_ms as f64 / 1000.0));
                ui.add(egui::Label::new(RichText::new(t).color(colors::INK_SECONDARY)).truncate());
            }
            if self.view.pending.is_some() {
                ui.add(egui::Spinner::new().size(12.0));
            }
        });
    }

    fn sort_rows(&mut self, ix: &Index, r: &Arc<SearchResult>) {
        let key = (self.sort, self.descending, Arc::as_ptr(r) as usize);
        if self.view.order_key == Some(key) && self.view.order.len() == r.hits.len() {
            return;
        }
        let roll = ix.rollup(&Filter::default());
        let size = |h: &Hit| -> u64 {
            match *h {
                Hit::File(f) => ix.file_size(f),
                Hit::Dir(d) => roll.stats[d as usize].bytes,
            }
        };
        let copies = |h: &Hit| -> u64 {
            match *h {
                Hit::File(f) => ix.copies(ix.file_content(f)) as u64,
                Hit::Dir(d) => roll.stats[d as usize].dup_files as u64,
            }
        };
        let folder = |h: &Hit| -> String {
            match *h {
                Hit::File(f) => ix.dir_path(ix.file_dir(f)),
                Hit::Dir(d) => ix.dir_parent(d).map(|p| ix.dir_path(p)).unwrap_or_default(),
            }
        };
        let source = |h: &Hit| -> u16 {
            match *h {
                Hit::File(f) => ix.file_source(f),
                Hit::Dir(d) => ix.dir_source(d),
            }
        };
        let mut order: Vec<u32> = (0..r.hits.len() as u32).collect();
        let hits = &r.hits;
        let name_cmp =
            |a: &u32, b: &u32| dupemap_core::natsort::natural_cmp(&hits[*a as usize].name, &hits[*b as usize].name);
        match self.sort {
            SearchSort::Name => order.sort_by(name_cmp),
            SearchSort::Folder => {
                let folders: Vec<String> = hits.iter().map(|h| folder(&h.hit)).collect();
                order.sort_by(|a, b| {
                    dupemap_core::natsort::natural_cmp(&folders[*a as usize], &folders[*b as usize])
                        .then_with(|| name_cmp(a, b))
                });
            }
            SearchSort::Source => order.sort_by(|a, b| {
                source(&hits[*a as usize].hit).cmp(&source(&hits[*b as usize].hit)).then_with(|| name_cmp(a, b))
            }),
            SearchSort::Size => order.sort_by(|a, b| {
                size(&hits[*a as usize].hit).cmp(&size(&hits[*b as usize].hit)).then_with(|| name_cmp(b, a))
            }),
            SearchSort::Copies => order.sort_by(|a, b| {
                copies(&hits[*a as usize].hit)
                    .cmp(&copies(&hits[*b as usize].hit))
                    .then_with(|| size(&hits[*a as usize].hit).cmp(&size(&hits[*b as usize].hit)))
            }),
        }
        // sorted ascending above
        if self.descending {
            order.reverse();
        }
        self.view.order = order;
        self.view.order_key = Some(key);
    }

    fn columns(left: f32, right: f32) -> Cols {
        // Size, Copies, Source; Name and Folder share what is left
        const MIN: [f32; 3] = [100.0, 64.0, 96.0];
        let width = right - left;
        let mut shown = [true; 3];
        // drop Source, then Copies, then Folder; the name keeps at least 130 px
        let need = |shown: &[bool; 3]| -> f32 {
            130.0
                + 84.0
                + if shown[0] { MIN[0] } else { 0.0 }
                + if shown[1] { MIN[1] } else { 0.0 }
                + if shown[2] { MIN[2] } else { 0.0 }
        };
        for k in [2usize, 1, 0] {
            if need(&shown) <= width {
                break;
            }
            shown[k] = false;
        }
        let size_w = 84.0f32;
        let copies_w = if shown[1] { 70.0 } else { 0.0 };
        let source_w = if shown[2] { (width * 0.14).clamp(MIN[2], 180.0) } else { 0.0 };
        let rest = (width - size_w - copies_w - source_w).max(0.0);
        let (name_w, folder_w) = if shown[0] { (rest * 0.55, rest * 0.45) } else { (rest, 0.0) };
        let n1 = left + name_w;
        let f1 = n1 + folder_w;
        let s1 = f1 + source_w;
        let z1 = s1 + size_w;
        Cols {
            name: Rangef::new(left, n1),
            folder: Rangef::new(n1, f1),
            source: Rangef::new(f1, s1),
            size: Rangef::new(s1, z1),
            copies: Rangef::new(z1, right),
            shown,
        }
    }

    fn table(&mut self, ui: &mut Ui, cx: &mut SearchCtx, r: &Arc<SearchResult>, select: &mut Option<usize>) {
        let scroll = &ui.spacing().scroll;
        let bar = scroll.bar_width + scroll.bar_inner_margin + scroll.bar_outer_margin;
        let full = ui.available_rect_before_wrap().intersect(ui.clip_rect());
        let cols = Self::columns(full.left(), full.right() - bar);
        let (hrect, _) = ui.allocate_exact_size(Vec2::new(full.width(), HEADER_H), Sense::hover());
        if let Some((s, d)) = self.header(ui, hrect, cols, cx.pane_id) {
            self.sort = s;
            self.descending = d;
            self.view.order_key = None;
            self.view.selected = None;
        }
        self.sort_rows(cx.ix, r);
        let roll = cx.ix.rollup(&Filter::default());
        ui.spacing_mut().item_spacing.y = 0.0;
        let mut area = ScrollArea::vertical()
            .id_salt(("search", cx.pane_id))
            .auto_shrink([false, false])
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible);
        if let Some(off) = self.view.scroll_to.take() {
            area = area.vertical_scroll_offset(off);
        }
        let n = self.view.order.len();
        let out = area.show_rows(ui, ROW_H, n, |ui, range| {
            for i in range {
                let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H), Sense::hover());
                let h = &r.hits[self.view.order[i] as usize];
                self.row(ui, i, rect, cols, h, &roll, cx, select);
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
        h: &dupemap_core::search::SearchHit,
        roll: &dupemap_core::index::Rollup,
        cx: &mut SearchCtx,
        select: &mut Option<usize>,
    ) {
        let ix = cx.ix;
        let resp = ui.interact(rect, ui.id().with(("srow", cx.pane_id, i)), Sense::CLICK);
        if resp.clicked() || resp.secondary_clicked() || resp.double_clicked() {
            ui.memory_mut(|m| {
                if let Some(f) = m.focused() {
                    m.surrender_focus(f);
                }
            });
        }
        let p = ui.painter_at(rect);
        let linked = match h.hit {
            Hit::File(f) => cx.linked.is_some_and(|l| l.content == Some(ix.file_content(f))),
            Hit::Dir(d) => cx.linked.is_some_and(|l| l.main_dirs.contains(&d)),
        };
        if self.view.selected == Some(i) {
            p.rect_filled(rect, CornerRadius::ZERO, colors::ROW_SELECTED);
            p.rect_filled(
                Rect::from_min_size(rect.min, Vec2::new(2.0, rect.height())),
                CornerRadius::ZERO,
                colors::ACCENT,
            );
        } else if linked {
            p.rect_filled(rect, CornerRadius::ZERO, colors::ROW_LINKED);
        } else if resp.hovered() {
            p.rect_filled(rect, CornerRadius::ZERO, colors::ROW_HOVER);
        } else if i % 2 == 1 {
            p.rect_filled(rect, CornerRadius::ZERO, colors::ROW_STRIPE);
        }
        let font = FontId::proportional(13.0);
        let small = FontId::proportional(11.5);
        let icon = Rect::from_min_size(Pos2::new(cols.name.min + PAD, rect.top()), Vec2::new(16.0, ROW_H));
        let (folder, source, size_text, copies) = match h.hit {
            Hit::File(f) => {
                if cx.marks.is_some_and(|m| m.is_marked(f)) {
                    widgets::trash_icon(&p, icon, colors::CRITICAL);
                } else {
                    widgets::file_icon(&p, icon, colors::INK_MUTED);
                }
                let size = match ix.size_state(f) {
                    SizeState::Unknown => "?".to_string(),
                    _ => format::bytes(ix.file_size(f)),
                };
                (ix.dir_path(ix.file_dir(f)), ix.file_source(f), size, Some(ix.copies(ix.file_content(f))))
            }
            Hit::Dir(d) => {
                let full = cx.marks.is_some_and(|m| crate::marks_ui::dir_fully_marked(ix, m, d));
                widgets::folder_icon(&p, icon, if full { colors::CRITICAL } else { colors::INK_MUTED });
                let parent = ix.dir_parent(d).map(|x| ix.dir_path(x)).unwrap_or_default();
                (parent, ix.dir_source(d), format::bytes(roll.stats[d as usize].bytes), None)
            }
        };
        let nr = Rect::from_x_y_ranges(Rangef::new(icon.right() + 6.0, cols.name.max - PAD), rect.y_range());
        let marked = cx.marks.is_some_and(|m| match h.hit {
            Hit::File(f) => m.is_marked(f),
            Hit::Dir(d) => crate::marks_ui::dir_fully_marked(ix, m, d),
        });
        let nm =
            widgets::text_left(&p, nr, &h.name, font.clone(), if marked { colors::INK_SECONDARY } else { colors::INK });
        if marked {
            let y = nm.center().y + 0.5;
            p.line_segment(
                [Pos2::new(nm.left(), y), Pos2::new(nm.right(), y)],
                egui::Stroke::new(1.0, colors::alpha(colors::CRITICAL, 0.9)),
            );
        }
        if cols.shown[0] {
            let fr = Rect::from_x_y_ranges(Rangef::new(cols.folder.min + PAD, cols.folder.max - PAD), rect.y_range());
            // the end of a long path says more than its start
            let shown = elide_start(&p, &folder, &small, fr.width());
            widgets::text_left(&p, fr, &shown, small.clone(), colors::INK_SECONDARY);
        }
        if cols.shown[2] {
            let sr = Rect::from_x_y_ranges(Rangef::new(cols.source.min + PAD, cols.source.max - PAD), rect.y_range());
            widgets::text_left(
                &p,
                sr,
                format::host(&ix.sources()[source as usize].host),
                small.clone(),
                colors::INK_SECONDARY,
            );
        }
        let zr = Rect::from_x_y_ranges(Rangef::new(cols.size.min + PAD, cols.size.max - PAD), rect.y_range());
        widgets::text_right(&p, zr, &size_text, font.clone(), colors::INK_SECONDARY);
        if cols.shown[1] {
            match copies {
                Some(c) => {
                    widgets::pill(
                        &p,
                        Pos2::new(cols.copies.max - PAD, rect.center().y),
                        &format::count(c as u64),
                        colors::copies_fill(c),
                        small.clone(),
                    );
                }
                None => {
                    let cr = Rect::from_x_y_ranges(
                        Rangef::new(cols.copies.min + PAD, cols.copies.max - PAD),
                        rect.y_range(),
                    );
                    widgets::text_right(&p, cr, "folder", small.clone(), colors::INK_MUTED);
                }
            }
        }
        let path = match h.hit {
            Hit::File(f) => ix.path_with_name(ix.file_dir(f), &h.name),
            Hit::Dir(d) => ix.dir_path(d),
        };
        let resp = resp.on_hover_ui(|ui| {
            ui.label(RichText::new(&path).strong());
            ui.label(format!("{} · {}", format::host(&ix.sources()[source as usize].host), size_text));
            if let Hit::File(f) = h.hit {
                match ix.size_state(f) {
                    SizeState::Unknown => {
                        ui.label(
                            RichText::new("Size not known: it came from a checksum list without sizes")
                                .color(colors::INK_MUTED),
                        );
                    }
                    SizeState::FromCopy => {
                        ui.label(RichText::new("Size taken from a copy with the same hash").color(colors::INK_MUTED));
                    }
                    SizeState::Known => {}
                }
            }
            ui.label(RichText::new("Double-click to show it in a tab").small().color(colors::INK_MUTED));
        });
        if resp.clicked() || resp.secondary_clicked() {
            *select = Some(i);
        }
        if resp.double_clicked() {
            cx.actions.push(match h.hit {
                Hit::File(f) => Action::ShowInTab(f),
                Hit::Dir(d) => Action::ShowDirInTab(d),
            });
        }
        resp.context_menu(|ui| {
            let (place, name) = match h.hit {
                Hit::File(f) => (dupemap_core::marks::Place::File(f), Some(h.name.as_str())),
                Hit::Dir(d) => (dupemap_core::marks::Place::Dir(d), None),
            };
            crate::marks_ui::menu(ui, ix, cx.book, cx.marks, place, name, cx.actions);
            ui.separator();
            if ui.button("Show in a tab").clicked() {
                cx.actions.push(match h.hit {
                    Hit::File(f) => Action::ShowInTab(f),
                    Hit::Dir(d) => Action::ShowDirInTab(d),
                });
                ui.close();
            }
            if ui.button("Copy path").clicked() {
                cx.actions.push(Action::CopyText(path.clone()));
                ui.close();
            }
            if ui.button("Copy name").clicked() {
                cx.actions.push(Action::CopyText(h.name.clone()));
                ui.close();
            }
            if cfg!(windows)
                && crate::actions::is_this_computer(&ix.sources()[source as usize].host)
                && ui.button("Show in Explorer").clicked()
            {
                cx.actions.push(Action::Reveal(path.clone()));
                ui.close();
            }
        });
    }

    fn header(&self, ui: &mut Ui, rect: Rect, cols: Cols, pane_id: u64) -> Option<(SearchSort, bool)> {
        let p = ui.painter_at(rect);
        p.rect_filled(rect, CornerRadius::ZERO, colors::SURFACE);
        p.line_segment([rect.left_bottom(), rect.right_bottom()], Stroke::new(1.0, colors::BASELINE));
        let font = FontId::proportional(12.5);
        let titles: [(&str, Rangef, SearchSort, bool, bool); 5] = [
            ("Name", cols.name, SearchSort::Name, false, true),
            ("Folder", cols.folder, SearchSort::Folder, false, cols.shown[0]),
            ("Source", cols.source, SearchSort::Source, false, cols.shown[2]),
            ("Size", cols.size, SearchSort::Size, true, true),
            ("Copies", cols.copies, SearchSort::Copies, true, cols.shown[1]),
        ];
        let mut changed = None;
        for (i, (t, x, key, right, shown)) in titles.into_iter().enumerate() {
            if !shown {
                continue;
            }
            let cell = Rect::from_x_y_ranges(x, rect.y_range());
            let resp = ui.interact(cell, ui.id().with(("shdr", pane_id, i)), Sense::CLICK);
            if resp.hovered() {
                p.rect_filled(cell, CornerRadius::ZERO, colors::SURFACE_RAISED);
            }
            let active = key == self.sort;
            let ink = if active { colors::INK } else { colors::INK_SECONDARY };
            let inner = Rect::from_x_y_ranges(Rangef::new(x.min + PAD, x.max - PAD), rect.y_range());
            let tr = if right {
                widgets::header_right(&p, inner, t, font.clone(), ink)
            } else {
                widgets::text_left(&p, inner, t, font.clone(), ink)
            };
            if active {
                let at = if right {
                    Pos2::new(tr.left() - 8.0, rect.center().y)
                } else {
                    Pos2::new(tr.right() + 8.0, rect.center().y)
                };
                widgets::sort_arrow(&p, at, self.descending, ink);
            }
            if resp.on_hover_text("Click to sort; again to reverse").clicked() {
                // numbers largest first, text A to Z on a new column; reversed on the same one
                changed = Some(if key == self.sort { (key, !self.descending) } else { (key, right) });
            }
        }
        changed
    }

    fn select(&mut self, cx: &mut SearchCtx, r: &SearchResult, i: usize) {
        self.view.selected = Some(i);
        if let Some(&k) = self.view.order.get(i) {
            let t = match r.hits[k as usize].hit {
                Hit::File(f) => Target::File { file: f, content: cx.ix.file_content(f) },
                Hit::Dir(d) => Target::Dir(d),
            };
            cx.actions.push(Action::Inspect(Some(t)));
        }
    }

    fn keyboard(&mut self, ui: &mut Ui, cx: &mut SearchCtx, r: &SearchResult, select: &mut Option<usize>) {
        use egui::Modifiers as M;
        if ui.input_mut(|i| i.consume_key(M::NONE, Key::Delete)) {
            if let (Some(&k), Some(st)) = (self.view.selected.and_then(|c| self.view.order.get(c)), cx.marks) {
                let h = &r.hits[k as usize];
                let (place, name) = match h.hit {
                    Hit::File(f) => (dupemap_core::marks::Place::File(f), Some(h.name.as_str())),
                    Hit::Dir(d) => (dupemap_core::marks::Place::Dir(d), None),
                };
                cx.actions.push(crate::marks_ui::toggle(cx.ix, cx.book, st, place, name));
            }
        }
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
        let total = self.view.order.len();
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
            if let Some(&k) = cur.and_then(|c| self.view.order.get(c)) {
                cx.actions.push(match r.hits[k as usize].hit {
                    Hit::File(f) => Action::ShowInTab(f),
                    Hit::Dir(d) => Action::ShowDirInTab(d),
                });
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

/// `…\Photos\2019` instead of `D:\Users\…`: drop the start of a path until it fits.
fn elide_start(p: &egui::Painter, text: &str, font: &FontId, max: f32) -> String {
    let width = |t: &str| p.layout_no_wrap(t.to_string(), font.clone(), colors::INK).size().x;
    if width(text) <= max {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    // binary search the number of characters to keep
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let t: String = std::iter::once('…').chain(chars[chars.len() - mid..].iter().copied()).collect();
        if width(&t) <= max {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    std::iter::once('…').chain(chars[chars.len() - lo..].iter().copied()).collect()
}

fn help(ui: &mut Ui) {
    ui.horizontal(|ui| {
        ui.add_space(10.0);
        ui.vertical(|ui| {
            ui.set_max_width(520.0);
            ui.label(RichText::new("Find files and folders in every source").color(colors::INK_SECONDARY));
            ui.add_space(6.0);
            for line in [
                "holiday: names containing it; case and accents do not matter (plaza finds Plaża)",
                "*.mkv or IMG_20??.jpg: whole names, with * for anything and ? for one character",
                "a SHA-256, or its first digits: every copy of that file",
            ] {
                ui.add(egui::Label::new(RichText::new(line).color(colors::INK_MUTED)).wrap());
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enough_to_search() {
        let q = |t: &str, mode| SearchQuery { text: t.into(), mode, ..Default::default() };
        assert!(!SearchPane::ready(&q("a", SearchMode::Name)));
        assert!(SearchPane::ready(&q("ab", SearchMode::Name)));
        assert!(SearchPane::ready(&q("*", SearchMode::Name)));
        assert!(!SearchPane::ready(&q("abc12", SearchMode::Hash)));
        assert!(SearchPane::ready(&q("abc123", SearchMode::Hash)));
        assert!(SearchPane::ready(&q("md5:abc123", SearchMode::Hash)));
    }

    #[test]
    fn columns_fit_and_give_way() {
        let c = SearchPane::columns(0.0, 1200.0);
        assert_eq!(c.shown, [true; 3]);
        assert!((c.copies.max - 1200.0).abs() < 0.01);
        assert!(c.name.span() > 200.0 && c.folder.span() > 200.0);
        let c = SearchPane::columns(0.0, 420.0);
        assert!(!c.shown[2], "Source goes first");
        assert!(c.shown[0] && c.shown[1], "Folder and Copies still fit at 420 px");
        let c = SearchPane::columns(0.0, 230.0);
        assert_eq!(c.shown, [false, false, false]);
        assert!(c.name.span() >= 130.0);
    }
}
