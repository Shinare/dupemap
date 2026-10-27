//! A tree pane: one source (or every source) as an expandable folder table.
//!
//! Columns: Name · Size (bar = share of the parent folder, colour = absolute size) · Files ·
//! Copies (files: copies anywhere, as a pill; folders: files that have copies) · Elsewhere
//! (files: copies in other sources; folders: share of bytes that also exist outside the folder).

use crate::actions::Action;
use crate::colors;
use crate::files::FileCache;
use crate::format;
use crate::selection::{Linked, Target};
use crate::tree::{self, Root, Row, RowKind};
use crate::widgets;
use crate::worker::Worker;
use dupemap_core::ignore::IgnoreRule;
use dupemap_core::index::{
    CopiesFilter, DirIx, DirStats, FileIx, FileRow, Filter, HashFilter, Index, ListOptions, Rollup, SortKey,
};
use dupemap_core::paths::PathKind;
use egui::{Align2, CornerRadius, FontId, Key, Pos2, Rangef, Rect, ScrollArea, Sense, Stroke, Ui, Vec2};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

pub const ROW_H: f32 = 22.0;
const HEADER_H: f32 = 24.0;
const INDENT: f32 = 14.0;
const PAD: f32 = 8.0;
const NAME_MIN: f32 = 140.0;
/// "Expand everything below" opens at most this many folders.
const MAX_EXPAND: usize = 5_000;

/// The selected row, by database id (survives index rebuilds).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Sel {
    Dir(i64),
    File(i64),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Widths {
    pub size: f32,
    pub files: f32,
    pub copies: f32,
    pub elsewhere: f32,
}

impl Default for Widths {
    fn default() -> Self {
        Widths { size: 170.0, files: 78.0, copies: 76.0, elsewhere: 170.0 }
    }
}

/// Narrowest Size, Files, Copies and Elsewhere columns: the number still fits, the bar goes.
const MIN_W: [f32; 4] = [70.0, 60.0, 60.0, 72.0];
/// Number columns a pane too narrow for all of them gives up, in this order (Files, Copies,
/// Elsewhere); the name keeps at least `NAME_MIN`.
const DROP_ORDER: [usize; 3] = [1, 2, 3];
/// Narrowest indentation per folder level.
const INDENT_MIN: f32 = 6.0;

#[derive(Clone, Serialize, Deserialize)]
pub struct TreePane {
    pub root: Root,
    pub opts: ListOptions,
    /// Expanded folders by database id.
    pub expanded: HashSet<i64>,
    pub selected: Option<Sel>,
    pub widths: Widths,
    #[serde(skip)]
    view: View,
}

#[derive(Clone, Default)]
struct View {
    key: Option<(u64, u64, u64)>,
    version: u64,
    rows: Vec<Row>,
    files: HashMap<DirIx, Arc<Vec<FileRow>>>,
    offset: f32,
    viewport_h: f32,
    scroll_to: Option<f32>,
    /// A row to bring into view once it is listed (after a reveal); frames left to wait for it.
    pending_reveal: Option<(Sel, u16)>,
    /// Depth of the deepest listed row (sets the indentation in narrow panes).
    max_depth: u16,
}

/// What a pane needs from the app while drawing.
pub struct PaneCtx<'a> {
    pub ix: &'a Arc<Index>,
    pub generation: u64,
    pub files: &'a mut FileCache,
    pub worker: &'a Worker,
    pub actions: &'a mut Vec<Action>,
    pub pane_id: u64,
    pub focused: bool,
    pub alert_hosts: &'a BTreeSet<String>,
    /// Copies of the app-wide selection, highlighted in every tab.
    pub linked: Option<&'a Linked>,
}

enum Event {
    Toggle(DirIx),
    Select(Sel, Target),
    Filter(Filter),
    Sort(SortKey),
    Root(Root),
    CollapseAll,
    ExpandBelow(DirIx),
    CollapseBelow(DirIx),
}

#[derive(Clone, Copy)]
struct Cols {
    name: Rangef,
    size: Rangef,
    files: Rangef,
    copies: Rangef,
    elsewhere: Rangef,
    /// Which of Size, Files, Copies and Elsewhere fit: a narrow pane drops Files first, then
    /// Copies, then Elsewhere (see `DROP_ORDER`). Size always stays.
    shown: [bool; 4],
    /// Indentation per folder level: narrower when deep folders would leave no room for names.
    indent: f32,
    /// The number columns are wider than their minimum, so dragging their edges changes
    /// something (in a narrow pane they are pinned at the minimum and the names get the rest).
    adjustable: bool,
}

impl TreePane {
    pub fn new(root: Root) -> Self {
        TreePane {
            root,
            opts: ListOptions::default(),
            expanded: HashSet::new(),
            selected: None,
            widths: Widths::default(),
            view: View::default(),
        }
    }

    pub fn title(&self, ix: Option<&Index>) -> String {
        match self.root {
            Root::AllSources => "All sources".into(),
            Root::Source(id) => match ix
                .and_then(|ix| ix.source_ix(id).map(|s| format::host(&ix.sources()[s as usize].host).to_string()))
            {
                Some(h) => h,
                None if ix.is_none() => "…".into(),
                None => "Removed source".into(),
            },
        }
    }

    /// Host shown by this pane, if it shows one source.
    pub fn host<'i>(&self, ix: &'i Index) -> Option<&'i str> {
        match self.root {
            Root::AllSources => None,
            Root::Source(id) => ix.source_ix(id).map(|s| ix.sources()[s as usize].host.as_str()),
        }
    }

    fn invalidate(&mut self) {
        self.view.version += 1;
    }

    /// Expand the path to the folder with the most duplicated bytes that directly holds files,
    /// starting at `start` (or this pane's root), so a fresh view shows real content. Returns the
    /// folder it selected.
    pub fn reveal_largest(&mut self, ix: &Index, start: Option<DirIx>) -> Option<DirIx> {
        let mut d = start.or_else(|| tree::root_dir(ix, self.root))?;
        let mut picked = None;
        for _ in 0..16 {
            self.expanded.insert(ix.dir_db_id(d));
            let (_, kids) = ix.list_dirs(dupemap_core::index::Node::Dir(d), &self.opts);
            // most bytes that also exist elsewhere, then most bytes: shows duplication at work
            let Some(k) = kids.iter().max_by_key(|k| (k.stats.elsewhere_bytes, k.stats.bytes)) else { break };
            if k.direct_files > 0 {
                self.expanded.insert(ix.dir_db_id(k.dir));
                self.selected = Some(Sel::Dir(ix.dir_db_id(k.dir)));
                self.view.pending_reveal = Some((Sel::Dir(ix.dir_db_id(k.dir)), 600));
                picked = Some(k.dir);
                break;
            }
            d = k.dir;
        }
        self.invalidate();
        picked
    }

    /// Show another source (or all sources). A single source opens with its first levels expanded
    /// down to where the content starts (`/` → `mnt` → `tank`).
    pub fn set_root(&mut self, ix: &Index, r: Root) {
        if self.root == r && self.view.key.is_some() {
            return;
        }
        self.root = r;
        self.selected = None;
        self.view.scroll_to = Some(0.0);
        if let Some(d) = tree::root_dir(ix, r) {
            for c in tree::expansion_chain(ix, d, &self.opts).into_iter().skip(1) {
                self.expanded.insert(ix.dir_db_id(c));
            }
        }
        self.invalidate();
    }

    /// Rebuild the flattened rows if anything they depend on changed.
    fn refresh(&mut self, ix: &Index, files: &mut FileCache, worker: &Worker, generation: u64) {
        let key = (generation, files.version, self.view.version);
        if self.view.key == Some(key) {
            return;
        }
        let used: RefCell<HashMap<DirIx, Arc<Vec<FileRow>>>> = RefCell::new(HashMap::new());
        let missing: RefCell<Vec<DirIx>> = RefCell::new(Vec::new());
        let cache = RefCell::new(&mut *files);
        let expanded = &self.expanded;
        let opts = &self.opts;
        let rows = tree::flatten(ix, self.root, opts, &|d| expanded.contains(&ix.dir_db_id(d)), &|d| {
            let got = cache.borrow_mut().get(d, opts);
            match &got {
                Some(list) => {
                    used.borrow_mut().insert(d, list.clone());
                }
                None => missing.borrow_mut().push(d),
            }
            got
        });
        for d in missing.into_inner() {
            files.request(worker, d, opts);
        }
        self.view.max_depth = rows.iter().map(|r| r.depth).max().unwrap_or(0);
        self.view.rows = rows;
        self.view.files = used.into_inner();
        self.view.key = Some((generation, files.version, self.view.version));
    }

    pub fn ui(&mut self, ui: &mut Ui, cx: &mut PaneCtx) {
        let ix = cx.ix.clone();
        let full = ui.available_rect_before_wrap();
        if ui.rect_contains_pointer(full) && ui.input(|i| i.pointer.any_pressed()) && !cx.focused {
            cx.actions.push(Action::Focus(cx.pane_id));
        }
        if let Root::Source(id) = self.root {
            if ix.source_ix(id).is_none() {
                self.missing_source(ui, &ix);
                return;
            }
        }
        self.refresh(&ix, cx.files, cx.worker, cx.generation);
        self.scroll_to_pending_reveal(&ix, cx.files.is_pending());
        let mut ev = Vec::new();
        self.toolbar(ui, &ix, cx, &mut ev);
        ui.add_space(4.0);
        if self.view.rows.is_empty() {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new("Nothing to show").color(colors::INK_SECONDARY));
                if ix.sources().is_empty() {
                    ui.add_space(6.0);
                    if ui.button("Import manifests…").clicked() {
                        cx.actions.push(Action::Import);
                    }
                }
            });
        } else {
            self.table(ui, &ix, cx, &mut ev);
        }
        // keys go to the tree unless a widget (text field, button, combo…) has keyboard focus
        if cx.focused && ui.memory(|m| m.focused().is_none()) {
            self.keyboard(ui, &ix, cx, &mut ev);
        }
        for e in &ev {
            if let Event::Select(_, t) = e {
                cx.actions.push(Action::Inspect(Some(*t)));
            }
        }
        let mut notes = Vec::new();
        self.apply(ev, &ix, &mut notes);
        cx.actions.extend(notes.into_iter().map(Action::Note));
    }

    /// Open the tree at a file or folder: expand the folders above it (and the folder holding a
    /// file), select it and scroll it into view once listed. Clears this tab's filter if the
    /// filter hides it. Returns a note for the status bar when it had to.
    pub fn reveal(&mut self, ix: &Index, dir: DirIx, file: Option<FileIx>) -> Option<String> {
        let mut note = None;
        let hidden = match file {
            Some(f) => !ix.matches(&self.opts.filter, f),
            None => ix.rollup(&self.opts.filter).stats[dir as usize].files == 0,
        };
        if hidden {
            self.opts.filter = Filter::default();
            note = Some("Cleared the tab's filter to show it".to_string());
        }
        if let Root::Source(id) = self.root {
            if ix.source_ix(id) != Some(ix.dir_source(dir)) {
                self.root = Root::AllSources;
            }
        }
        let chain = ix.dir_ancestors(dir);
        let open_to = if file.is_some() { chain.len() } else { chain.len() - 1 };
        for &d in &chain[..open_to] {
            self.expanded.insert(ix.dir_db_id(d));
        }
        let sel = match file {
            Some(f) => Sel::File(ix.file_db_id(f)),
            None => Sel::Dir(ix.dir_db_id(dir)),
        };
        self.selected = Some(sel);
        self.view.pending_reveal = Some((sel, 600));
        self.invalidate();
        note
    }

    /// Scroll a just-revealed row to the middle of the view once its listing arrived.
    fn scroll_to_pending_reveal(&mut self, ix: &Index, files_pending: bool) {
        let Some((sel, left)) = self.view.pending_reveal else { return };
        let found = (0..self.view.rows.len()).find(|&i| self.row_sel(ix, i) == Some(sel));
        match found {
            Some(i) => {
                let vh = self.view.viewport_h.max(ROW_H * 10.0);
                self.view.scroll_to = Some((i as f32 * ROW_H - vh / 2.0 + ROW_H / 2.0).max(0.0));
                self.view.pending_reveal = None;
            }
            // everything is listed and it is not there, or we waited long enough: give up quietly
            None if !files_pending || left == 0 => self.view.pending_reveal = None,
            None => self.view.pending_reveal = Some((sel, left - 1)),
        }
    }

    fn missing_source(&mut self, ui: &mut Ui, ix: &Index) {
        ui.add_space(24.0);
        ui.vertical_centered(|ui| {
            ui.label(egui::RichText::new("This source is no longer in the database.").color(colors::INK_SECONDARY));
            ui.add_space(6.0);
            if ui.button("Show all sources").clicked() {
                self.root = Root::AllSources;
                self.invalidate();
            }
            for s in ix.sources() {
                if ui.button(format::host(&s.host)).clicked() {
                    self.root = Root::Source(s.db_id);
                    self.invalidate();
                }
            }
        });
    }

    fn toolbar(&mut self, ui: &mut Ui, ix: &Index, cx: &mut PaneCtx, ev: &mut Vec<Event>) {
        // a narrow pane puts the totals on their own line and lets the controls wrap
        let narrow = ui.available_width() < 640.0;
        let controls = |this: &mut Self, ui: &mut Ui, ev: &mut Vec<Event>, cx: &mut PaneCtx| {
            ui.add_space(4.0);
            ui.label(egui::RichText::new("Source").color(colors::INK_MUTED));
            let current = this.title(Some(ix));
            egui::ComboBox::from_id_salt(("root", cx.pane_id))
                .selected_text(current)
                .width((ui.available_width() - 190.0).clamp(110.0, 240.0))
                .height(480.0)
                .truncate()
                .show_ui(ui, |ui| {
                    if ui.selectable_label(this.root == Root::AllSources, "All sources").clicked() {
                        ev.push(Event::Root(Root::AllSources));
                    }
                    ui.separator();
                    for s in ix.source_summaries() {
                        let text = format!(
                            "{}   {} · {}",
                            format::host(&s.host),
                            format::plural(s.files, "file", "files"),
                            format::bytes(s.bytes)
                        );
                        let alert = cx.alert_hosts.contains(&s.host);
                        let label = if alert { format!("⚠ {text}") } else { text };
                        if ui.selectable_label(this.root == Root::Source(s.db_id), label).clicked() {
                            ev.push(Event::Root(Root::Source(s.db_id)));
                        }
                    }
                });
            this.filter_button(ui, ix, cx, ev, narrow);
            if ui.button("Collapse all").on_hover_text("Collapse every folder in this tab").clicked() {
                ev.push(Event::CollapseAll);
            }
        };
        let st = self.root_stats(ix);
        let pct = if st.files > 0 { st.dup_files as f64 * 100.0 / st.files as f64 } else { 0.0 };
        let summary = format!(
            "{} · {} · {} with copies ({})",
            format::plural(st.files as u64, "file", "files"),
            format::bytes(st.bytes),
            format::count(st.dup_files as u64),
            format::percent(pct)
        );
        let pending = cx.files.is_pending();
        let totals = |ui: &mut Ui| {
            // cut, never overlap the buttons
            ui.add(egui::Label::new(egui::RichText::new(&summary).color(colors::INK_SECONDARY)).truncate())
                .on_hover_text(&summary);
            if pending {
                ui.add(egui::Spinner::new().size(12.0));
            }
        };
        if narrow {
            ui.horizontal_wrapped(|ui| controls(self, ui, ev, cx));
            ui.horizontal(|ui| {
                ui.add_space(6.0);
                totals(ui);
            });
        } else {
            ui.horizontal(|ui| {
                controls(self, ui, ev, cx);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(6.0);
                    totals(ui);
                });
            });
        }
    }

    /// The filter button: a popup to choose which files this tab shows. Folder totals are then
    /// computed over the matching files only (copies are always counted over everything).
    fn filter_button(&mut self, ui: &mut Ui, ix: &Index, cx: &mut PaneCtx, ev: &mut Vec<Event>, narrow: bool) {
        let f = &self.opts.filter;
        let active = !f.is_all();
        // a narrow pane shows only that a filter is on (the button is filled); the hover says which
        let label = if active && !narrow { format!("Filter: {}", filter_summary(ix, f)) } else { "Filter".to_string() };
        let mut button = egui::Button::new(egui::RichText::new(label).color(colors::INK)).truncate();
        if active {
            button = button.fill(colors::SELECTION);
        }
        let tip = if active {
            format!("Showing: {}\nClick to change which files this tab shows", filter_summary(ix, f))
        } else {
            "Choose which files this tab shows".to_string()
        };
        let resp = ui.add(button).on_hover_text(tip);
        let mut new = f.clone();
        egui::Popup::from_toggle_button_response(&resp)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .id(ui.id().with(("filter-popup", cx.pane_id)))
            .show(|ui| {
                ui.set_min_width(300.0);
                ui.label(egui::RichText::new("Show files").strong());
                for (v, text, tip) in [
                    (CopiesFilter::Any, "All", "Every file"),
                    (CopiesFilter::Unique, "Unique", "The only copy anywhere"),
                    (CopiesFilter::Duplicated, "With copies", "Two or more copies anywhere, in any source"),
                    (
                        CopiesFilter::InOtherSources,
                        "With a copy in another source",
                        "Another machine, drive or cloud account also has it",
                    ),
                    (
                        CopiesFilter::OnlyThisSource,
                        "With no copy in another source",
                        "Only its own machine, drive or cloud account has it (perhaps more than once)",
                    ),
                ] {
                    ui.radio_value(&mut new.copies, v, text).on_hover_text(tip);
                }
                ui.add_space(6.0);
                ui.label(egui::RichText::new("Missing from").strong())
                    .on_hover_text("Only files that have no copy in the chosen source: what that source lacks");
                ui.radio_value(&mut new.absent_from, None, "Any source (no condition)");
                for s in ix.sources() {
                    ui.radio_value(&mut new.absent_from, Some(s.db_id), format!("Not in {}", format::host(&s.host)));
                }
                ui.add_space(6.0);
                ui.label(egui::RichText::new("Hash").strong());
                ui.radio_value(&mut new.hash, HashFilter::Any, "Any");
                ui.radio_value(&mut new.hash, HashFilter::Sha256, "SHA-256 only");
                ui.radio_value(&mut new.hash, HashFilter::Flagged, "Not SHA-256 (flagged)")
                    .on_hover_text("Rows whose hash is not SHA-256: they only match rows of the same algorithm");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.add_enabled(!new.is_all(), egui::Button::new("Show all files")).clicked() {
                        new = Filter::default();
                    }
                    ui.label(
                        egui::RichText::new("Folder totals count matching files only").small().color(colors::INK_MUTED),
                    );
                });
            });
        if active && ui.small_button("✕").on_hover_text("Clear the filter").clicked() {
            new = Filter::default();
        }
        if new != self.opts.filter {
            ev.push(Event::Filter(new));
        }
    }

    fn root_stats(&self, ix: &Index) -> DirStats {
        match tree::root_dir(ix, self.root) {
            Some(d) => ix.rollup(&self.opts.filter).stats[d as usize],
            None => ix.list_dirs(dupemap_core::index::Node::AllSources, &self.opts).0,
        }
    }

    fn columns(&self, left: f32, right: f32) -> Cols {
        let width = right - left;
        // drop number columns the pane has no room for, least important first
        let mut shown = [true; 4];
        for &k in &DROP_ORDER {
            let need: f32 = NAME_MIN + (0..4).filter(|&i| shown[i]).map(|i| MIN_W[i]).sum::<f32>();
            if need <= width {
                break;
            }
            shown[k] = false;
        }
        let w = &self.widths;
        let mut fixed = [w.size, w.files, w.copies, w.elsewhere];
        let mut min = MIN_W;
        for i in 0..4 {
            if !shown[i] {
                fixed[i] = 0.0;
                min[i] = 0.0;
            }
        }
        // names keep half of a narrow pane (split views) unless the numbers would no longer fit.
        // The other columns shrink: first the two with bars (a bar gives way before any number
        // does), then all of them to their minimum, then evenly.
        let min_sum: f32 = min.iter().sum();
        // once a column had to go, every spare pixel goes to the names
        let name_min = if shown.iter().all(|&v| v) {
            NAME_MIN.max((width * 0.5).min(width - min_sum))
        } else {
            NAME_MIN.max(width - min_sum)
        };
        let avail = (width - name_min).max(0.0);
        let bars_first = [min[0], fixed[1], fixed[2], min[3]];
        if widgets::shrink_columns(&mut fixed, &bars_first, avail) > 0.0
            && widgets::shrink_columns(&mut fixed, &min, avail) > 0.0
        {
            widgets::scale_columns(&mut fixed, avail);
        }
        let e0 = right - fixed[3];
        let c0 = e0 - fixed[2];
        let f0 = c0 - fixed[1];
        let s0 = f0 - fixed[0];
        Cols {
            name: Rangef::new(left, s0),
            size: Rangef::new(s0, f0),
            files: Rangef::new(f0, c0),
            copies: Rangef::new(c0, e0),
            elsewhere: Rangef::new(e0, right),
            shown,
            indent: INDENT,
            adjustable: shown.iter().all(|&v| v) && fixed.iter().sum::<f32>() > MIN_W.iter().sum::<f32>() + 0.5,
        }
    }

    fn table(&mut self, ui: &mut Ui, ix: &Index, cx: &mut PaneCtx, ev: &mut Vec<Event>) {
        let roll = ix.rollup(&self.opts.filter);
        let scroll = &ui.spacing().scroll;
        let bar = scroll.bar_width + scroll.bar_inner_margin + scroll.bar_outer_margin;
        // never wider than what is visible (a wide toolbar must not push columns off screen)
        let full = ui.available_rect_before_wrap().intersect(ui.clip_rect());
        let mut cols = self.columns(full.left(), full.right() - bar);
        // deep folders in a narrow pane: indent less, so the names keep some room
        let levels = self.view.max_depth.max(1) as f32;
        cols.indent = (cols.name.span() * 0.35 / levels).clamp(INDENT_MIN, INDENT);

        // header
        let (hrect, _) = ui.allocate_exact_size(Vec2::new(full.width(), HEADER_H), Sense::hover());
        self.header(ui, hrect, cols, cx.pane_id, ev);

        // rows (only the visible ones are laid out)
        let n = self.view.rows.len();
        ui.spacing_mut().item_spacing.y = 0.0;
        let mut area = ScrollArea::vertical()
            .id_salt(("rows", cx.pane_id))
            .auto_shrink([false, false])
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible);
        if let Some(off) = self.view.scroll_to.take() {
            area = area.vertical_scroll_offset(off);
        }
        let out = area.show_rows(ui, ROW_H, n, |ui, range| {
            for i in range {
                let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H), Sense::hover());
                self.row(ui, i, rect, cols, ix, &roll, cx, ev);
            }
        });
        self.view.offset = out.state.offset.y;
        self.view.viewport_h = out.inner_rect.height();
    }

    fn header(&mut self, ui: &mut Ui, rect: Rect, cols: Cols, pane_id: u64, ev: &mut Vec<Event>) {
        let p = ui.painter_at(rect);
        p.rect_filled(rect, CornerRadius::ZERO, colors::SURFACE);
        p.line_segment([rect.left_bottom(), rect.right_bottom()], Stroke::new(1.0, colors::BASELINE));
        let font = FontId::proportional(12.5);
        let titles: [(&str, Rangef, Option<SortKey>, bool, &str); 5] = [
            ("Name", cols.name, Some(SortKey::Name), false, "Click to sort by name"),
            ("Size", cols.size, Some(SortKey::Size), true, "Total size. The bar shows the share of the parent folder; its colour shows the absolute size (darker = smaller).\nClick to sort."),
            ("Files", cols.files, Some(SortKey::Files), true, "Files in the folder, including subfolders.\nClick to sort."),
            ("Copies", cols.copies, Some(SortKey::Copies), true, "Files: copies anywhere, including this one.\nFolders: how many of the files have at least one other copy.\nClick to sort."),
            ("Elsewhere", cols.elsewhere, Some(SortKey::Elsewhere), true, "Files: copies in other sources (other machines, drives or cloud).\nFolders: share of the bytes that also exist outside the folder; ✔ when every file has a copy outside it.\nClick to sort."),
        ];
        for (i, (title, x, sort, right, tip)) in titles.into_iter().enumerate() {
            if i > 0 && !cols.shown[i - 1] {
                continue;
            }
            let cell = Rect::from_x_y_ranges(x, rect.y_range());
            let id = ui.id().with(("hdr", pane_id, i));
            let resp = ui.interact(cell, id, if sort.is_some() { Sense::CLICK } else { Sense::hover() });
            if resp.hovered() && sort.is_some() {
                p.rect_filled(cell, CornerRadius::ZERO, colors::SURFACE_RAISED);
            }
            let ink = if sort.is_some() && sort == Some(self.opts.sort) { colors::INK } else { colors::INK_SECONDARY };
            // right-aligned titles may use the column's left padding before they are cut
            let inner = if right {
                Rect::from_x_y_ranges(Rangef::new(x.min + 2.0, x.max - PAD), rect.y_range())
            } else {
                cell.shrink2(Vec2::new(PAD, 0.0))
            };
            let text_rect = if right {
                widgets::header_right(&p, inner, title, font.clone(), ink)
            } else {
                widgets::text_left(&p, inner, title, font.clone(), ink)
            };
            if sort == Some(self.opts.sort) {
                // sort direction arrow next to the title
                let c = if right {
                    Pos2::new(text_rect.left() - 9.0, rect.center().y)
                } else {
                    Pos2::new(text_rect.right() + 9.0, rect.center().y)
                };
                widgets::sort_arrow(&p, c, self.opts.descending, colors::INK_SECONDARY);
            }
            let resp = resp.on_hover_text(tip);
            if resp.clicked() {
                if let Some(k) = sort {
                    ev.push(Event::Sort(k));
                }
            }
        }
        // resize handles on the left edge of each shown number column. A narrow pane keeps the
        // numbers at their minimum and gives the rest to the names, so there is nothing to drag:
        // the lines stay, the handles go.
        let draggable = cols.adjustable;
        let bounds = [cols.size.min, cols.files.min, cols.copies.min, cols.elsewhere.min];
        for (b, x) in bounds.into_iter().enumerate() {
            if !cols.shown[b] {
                continue;
            }
            let handle = Rect::from_x_y_ranges(Rangef::new(x - 3.0, x + 3.0), rect.y_range());
            let sense = if draggable { Sense::drag() } else { Sense::hover() };
            let resp = ui.interact(handle, ui.id().with(("resize", pane_id, b)), sense);
            let hot = draggable && (resp.hovered() || resp.dragged());
            p.line_segment(
                [Pos2::new(x, rect.top() + 5.0), Pos2::new(x, rect.bottom() - 5.0)],
                Stroke::new(1.0, if hot { colors::INK_SECONDARY } else { colors::GRID }),
            );
            if hot {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
            }
            if resp.dragged() {
                let dx = resp.drag_delta().x;
                self.resize(b, dx, cols.shown);
            }
        }
    }

    /// Move column boundary `b` (0 = name|size … 3 = copies|elsewhere) by `dx`.
    fn resize(&mut self, b: usize, dx: f32, shown: [bool; 4]) {
        let w = &mut self.widths;
        let mut arr = [w.size, w.files, w.copies, w.elsewhere];
        // the boundary is the left edge of column b; columns to its right keep the right edge,
        // the nearest shown column to its left (or the name) gives or takes the width
        let grow = -dx;
        let new_b = (arr[b] + grow).max(MIN_W[b]);
        let delta = new_b - arr[b];
        match (0..b).rev().find(|&k| shown[k]) {
            None => arr[b] = new_b,
            Some(k) => {
                let left = arr[k] - delta;
                if left < MIN_W[k] {
                    return;
                }
                arr[k] = left;
                arr[b] = new_b;
            }
        }
        *w = Widths { size: arr[0], files: arr[1], copies: arr[2], elsewhere: arr[3] };
    }

    #[allow(clippy::too_many_arguments)]
    fn row(
        &self,
        ui: &mut Ui,
        i: usize,
        rect: Rect,
        cols: Cols,
        ix: &Index,
        roll: &Rollup,
        cx: &mut PaneCtx,
        ev: &mut Vec<Event>,
    ) {
        let row = &self.view.rows[i];
        let (sel, file) = match row.kind {
            RowKind::Dir { dir, .. } => (Some(Sel::Dir(ix.dir_db_id(dir))), None),
            RowKind::File { dir, index } => match self.view.files.get(&dir).and_then(|l| l.get(index)) {
                Some(f) => (Some(Sel::File(f.db_id)), Some(f)),
                None => (None, None),
            },
            RowKind::Loading { .. } => (None, None),
        };
        // how strongly this row is part of the selection's copies (see `Linked`)
        let link = match (cx.linked, row.kind, file) {
            (Some(l), RowKind::Dir { dir, .. }, _) => l.dir_level(dir),
            (Some(l), RowKind::File { .. }, Some(f)) if l.content == Some(f.content) => 2,
            _ => 0,
        };
        let target = match (row.kind, file) {
            (RowKind::Dir { dir, .. }, _) => Some(Target::Dir(dir)),
            (RowKind::File { .. }, Some(f)) => Some(Target::File { file: f.file, content: f.content }),
            _ => None,
        };
        let id = ui.id().with(("row", cx.pane_id, i, sel));
        // click-only, not focusable: the arrow keys belong to the tree, not to egui's focus
        let resp = ui.interact(rect, id, Sense::CLICK);
        if resp.clicked() || resp.secondary_clicked() || resp.double_clicked() {
            ui.memory_mut(|m| {
                if let Some(f) = m.focused() {
                    m.surrender_focus(f);
                }
            });
        }
        let p = ui.painter_at(rect);
        let selected = sel.is_some() && self.selected == sel;
        if selected {
            p.rect_filled(rect, CornerRadius::ZERO, colors::ROW_SELECTED);
            p.rect_filled(
                Rect::from_min_size(rect.min, Vec2::new(2.0, rect.height())),
                CornerRadius::ZERO,
                colors::ACCENT,
            );
        } else if resp.hovered() {
            p.rect_filled(rect, CornerRadius::ZERO, colors::ROW_HOVER);
        } else if link == 2 {
            p.rect_filled(rect, CornerRadius::ZERO, colors::ROW_LINKED);
        } else if link == 1 {
            p.rect_filled(rect, CornerRadius::ZERO, colors::ROW_LINKED_PATH);
        } else if i % 2 == 1 {
            p.rect_filled(rect, CornerRadius::ZERO, colors::ROW_STRIPE);
        }
        if link > 0 && !selected {
            // a marker as well as the tint, so the highlight does not rely on colour alone
            let w = if link == 2 { 4.0 } else { 2.0 };
            let c = if link == 2 { colors::DUP } else { colors::alpha(colors::DUP, 0.6) };
            p.rect_filled(Rect::from_min_size(rect.min, Vec2::new(w, rect.height())), CornerRadius::ZERO, c);
        }
        let font = FontId::proportional(13.0);
        let small = FontId::proportional(11.5);
        let x = cols.name.min + PAD + row.depth as f32 * cols.indent;
        let exp_rect = Rect::from_min_size(Pos2::new(x - 2.0, rect.top()), Vec2::new(INDENT + 2.0, ROW_H));
        let icon_rect = Rect::from_min_size(Pos2::new(x + INDENT, rect.top()), Vec2::new(16.0, ROW_H));
        let name_left = x + INDENT + 20.0;
        let name_rect = Rect::from_x_y_ranges(Rangef::new(name_left, cols.name.max - PAD), rect.y_range());

        match row.kind {
            RowKind::Dir { dir, expandable } => {
                let st = roll.stats[dir as usize];
                let open = expandable && self.expanded.contains(&ix.dir_db_id(dir));
                if expandable {
                    widgets::expander(&p, exp_rect, open, colors::INK_SECONDARY);
                }
                let is_source = ix.dir_parent(dir).is_none();
                if is_source {
                    let s = &ix.sources()[ix.dir_source(dir) as usize];
                    source_icon(&p, icon_rect, s.kind);
                    let r = widgets::text_left(
                        &p,
                        name_rect,
                        format::host(&s.host),
                        FontId::proportional(13.5),
                        colors::INK,
                    );
                    let mut tx = r.right() + 8.0;
                    if cx.alert_hosts.contains(&s.host) && tx < name_rect.right() - 40.0 {
                        let t = widgets::tag(
                            &p,
                            Pos2::new(tx, rect.center().y),
                            "⚠ alert",
                            small.clone(),
                            colors::INK,
                            colors::CRITICAL,
                        );
                        tx = t.right() + 6.0;
                    }
                    if tx < name_rect.right() - 50.0 {
                        widgets::tag(
                            &p,
                            Pos2::new(tx, rect.center().y),
                            kind_label(s.kind),
                            small.clone(),
                            colors::INK_MUTED,
                            colors::BASELINE,
                        );
                    }
                } else {
                    widgets::folder_icon(&p, icon_rect, colors::INK_MUTED);
                    widgets::text_left(&p, name_rect, ix.dir_name(dir), font.clone(), colors::INK);
                }
                size_cell(&p, cols.size, rect, st.bytes, row.parent_bytes, false);
                if cols.shown[1] {
                    widgets::text_right(
                        &p,
                        inner(cols.files, rect),
                        &format::count(st.files as u64),
                        font.clone(),
                        colors::INK_SECONDARY,
                    );
                }
                if cols.shown[2] {
                    let (text, ink) = if st.dup_files > 0 {
                        (format::count(st.dup_files as u64), colors::INK_SECONDARY)
                    } else {
                        ("–".to_string(), colors::INK_MUTED)
                    };
                    widgets::text_right(&p, inner(cols.copies, rect), &text, font.clone(), ink);
                }
                if cols.shown[3] {
                    elsewhere_dir_cell(&p, cols.elsewhere, rect, &st, small.clone());
                }

                let resp = resp.on_hover_ui(|ui| dir_tooltip(ui, ix, dir, &st));
                if resp.clicked() {
                    let on_expander = resp.interact_pointer_pos().is_some_and(|p| exp_rect.contains(p));
                    if on_expander && expandable {
                        ev.push(Event::Toggle(dir));
                    } else if let (Some(s), Some(t)) = (sel, target) {
                        ev.push(Event::Select(s, t));
                    }
                }
                if resp.double_clicked() && expandable {
                    ev.push(Event::Toggle(dir));
                }
                if resp.secondary_clicked() {
                    if let (Some(s), Some(t)) = (sel, target) {
                        ev.push(Event::Select(s, t));
                    }
                }
                resp.context_menu(|ui| self.dir_menu(ui, ix, dir, cx, ev));
            }
            RowKind::File { dir, .. } => {
                let Some(f) = file else { return };
                let unique = f.copies <= 1;
                let ink = if unique { colors::INK_MUTED } else { colors::INK };
                widgets::file_icon(&p, icon_rect, colors::INK_MUTED);
                let mut nr = name_rect;
                if f.flagged {
                    // flagged rows never mix with SHA-256 matches; say so next to the name
                    let algo = ix.content_algo_name(f.content).to_uppercase();
                    let label = format!("⚠ {algo}");
                    let g = p.layout_no_wrap(label.clone(), small.clone(), colors::INK);
                    let tw = g.size().x + 10.0;
                    if nr.width() > tw + 60.0 {
                        widgets::tag(
                            &p,
                            Pos2::new(nr.right() - tw, rect.center().y),
                            &label,
                            small.clone(),
                            colors::INK,
                            colors::WARNING,
                        );
                        nr.max.x -= tw + 6.0;
                    }
                }
                widgets::text_left(&p, nr, &f.name, font.clone(), ink);
                size_cell(&p, cols.size, rect, f.size, row.parent_bytes, unique);
                if cols.shown[2] {
                    widgets::pill(
                        &p,
                        Pos2::new(cols.copies.max - PAD, rect.center().y),
                        &format::count(f.copies as u64),
                        colors::copies_fill(f.copies),
                        small.clone(),
                    );
                }
                let er = inner(cols.elsewhere, rect);
                if !cols.shown[3] {
                    // no room for the column
                } else if f.copies_other_sources == 0 {
                    // unique, or its copies are all in this source (the Copies pill tells which)
                    widgets::text_right(&p, er, "–", font.clone(), colors::INK_MUTED);
                } else {
                    // copies in other sources (several can be in the same one)
                    let t = format::plural(f.copies_other_sources as u64, "copy", "copies");
                    let g = widgets::elided(&p, &t, small.clone(), colors::INK_SECONDARY, er.width());
                    p.galley(
                        Pos2::new(er.right() - g.size().x, er.center().y - g.size().y / 2.0),
                        g,
                        colors::INK_SECONDARY,
                    );
                }
                let resp = resp.on_hover_ui(|ui| file_tooltip(ui, ix, f));
                if resp.clicked() || resp.secondary_clicked() {
                    if let (Some(s), Some(t)) = (sel, target) {
                        ev.push(Event::Select(s, t));
                    }
                }
                resp.context_menu(|ui| self.file_menu(ui, ix, dir, f, cx));
            }
            RowKind::Loading { dir } => {
                let text = match cx.files.error(dir, &self.opts) {
                    Some(e) => format!("Could not list files: {e}"),
                    None => "Loading files…".to_string(),
                };
                widgets::text_left(&p, name_rect, &text, font, colors::INK_MUTED);
            }
        }
    }

    fn dir_menu(&self, ui: &mut Ui, ix: &Index, dir: DirIx, cx: &mut PaneCtx, ev: &mut Vec<Event>) {
        let is_source = ix.dir_parent(dir).is_none();
        let source = &ix.sources()[ix.dir_source(dir) as usize];
        if ui
            .button("Where are the copies?")
            .on_hover_text("Which sources and folders hold copies of these files")
            .clicked()
        {
            cx.actions.push(Action::Inspect(Some(Target::Dir(dir))));
            cx.actions.push(Action::OpenInspector);
            ui.close();
        }
        ui.separator();
        if is_source {
            if ui.button("Summary by file type").on_hover_text("What kinds of files this source holds").clicked() {
                cx.actions.push(Action::OpenSummary(Root::Source(source.db_id)));
                ui.close();
            }
            if ui.button("Open in new tab").clicked() {
                cx.actions.push(Action::NewTab(Root::Source(source.db_id)));
                ui.close();
            }
        } else {
            let path = ix.dir_path(dir);
            if ui.button("Copy path").clicked() {
                cx.actions.push(Action::CopyText(path.clone()));
                ui.close();
            }
            if ui.button("Copy name").clicked() {
                cx.actions.push(Action::CopyText(ix.dir_name(dir).to_string()));
                ui.close();
            }
            reveal_item(ui, &source.host, &path, cx);
        }
        ui.separator();
        if ui.button("Expand everything below").clicked() {
            ev.push(Event::ExpandBelow(dir));
            ui.close();
        }
        if ui.button("Collapse everything below").clicked() {
            ev.push(Event::CollapseBelow(dir));
            ui.close();
        }
        if !is_source {
            ui.separator();
            let path = ix.dir_path(dir);
            let name = ix.dir_name(dir).to_string();
            ui.menu_button("Ignore this folder", |ui| {
                ui.label(
                    egui::RichText::new("Hidden from every tab; its files never count as copies")
                        .color(colors::INK_MUTED)
                        .small(),
                );
                let mut add = |rule: IgnoreRule| {
                    cx.actions.push(Action::AddIgnoreRule(rule));
                };
                if ui.button(format!("{path}  — in {} only", format::host(&source.host))).clicked() {
                    add(IgnoreRule::new(path.clone(), Some(source.host.clone())));
                    ui.close();
                }
                if ui.button(format!("{path}  — in every source")).clicked() {
                    add(IgnoreRule::new(path.clone(), None));
                    ui.close();
                }
                let sep = if source.kind == PathKind::Windows { '\\' } else { '/' };
                let any = format!("**{sep}{name}");
                if IgnoreRule::validate(&any).is_ok()
                    && ui.button(format!("Every folder named \"{name}\"  ({any})")).clicked()
                {
                    add(IgnoreRule::new(any.clone(), None));
                    ui.close();
                }
                ui.separator();
                if ui.button("Edit ignored folders…").clicked() {
                    cx.actions.push(Action::OpenIgnoreEditor);
                    ui.close();
                }
            });
        }
    }

    fn file_menu(&self, ui: &mut Ui, ix: &Index, dir: DirIx, f: &FileRow, cx: &mut PaneCtx) {
        let path = ix.path_with_name(dir, &f.name);
        let host = &ix.sources()[ix.dir_source(dir) as usize].host;
        if ui.button("Show every copy").on_hover_text("List every copy in the inspector").clicked() {
            cx.actions.push(Action::Inspect(Some(Target::File { file: f.file, content: f.content })));
            cx.actions.push(Action::OpenInspector);
            ui.close();
        }
        ui.separator();
        if ui.button("Copy path").clicked() {
            cx.actions.push(Action::CopyText(path.clone()));
            ui.close();
        }
        if ui.button("Copy name").clicked() {
            cx.actions.push(Action::CopyText(f.name.clone()));
            ui.close();
        }
        reveal_item(ui, host, &path, cx);
    }

    fn keyboard(&mut self, ui: &mut Ui, ix: &Index, cx: &mut PaneCtx, ev: &mut Vec<Event>) {
        let n = self.view.rows.len();
        if n == 0 {
            return;
        }
        let (down, up, right, left, enter, home, end, pgdn, pgup, copy) = ui.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::NONE, Key::ArrowDown),
                i.consume_key(egui::Modifiers::NONE, Key::ArrowUp),
                i.consume_key(egui::Modifiers::NONE, Key::ArrowRight),
                i.consume_key(egui::Modifiers::NONE, Key::ArrowLeft),
                i.consume_key(egui::Modifiers::NONE, Key::Enter) || i.consume_key(egui::Modifiers::NONE, Key::Space),
                i.consume_key(egui::Modifiers::NONE, Key::Home),
                i.consume_key(egui::Modifiers::NONE, Key::End),
                i.consume_key(egui::Modifiers::NONE, Key::PageDown),
                i.consume_key(egui::Modifiers::NONE, Key::PageUp),
                i.consume_key(egui::Modifiers::COMMAND, Key::C),
            )
        });
        if !(down || up || right || left || enter || home || end || pgdn || pgup || copy) {
            return;
        }
        let cur = self.selected_row(ix);
        let page = ((self.view.viewport_h / ROW_H) as usize).saturating_sub(1).max(1);
        let moved = down || up || right || left || enter || home || end || pgdn || pgup;
        let target = if !moved {
            None
        } else {
            match cur {
                None => Some(0),
                Some(c) if down => Some((c + 1).min(n - 1)),
                Some(c) if up => Some(c.saturating_sub(1)),
                Some(_) if home => Some(0),
                Some(_) if end => Some(n - 1),
                Some(c) if pgdn => Some((c + page).min(n - 1)),
                Some(c) if pgup => Some(c.saturating_sub(page)),
                Some(c) => {
                    let row = &self.view.rows[c];
                    match row.kind {
                        RowKind::Dir { dir, expandable } => {
                            let open = self.expanded.contains(&ix.dir_db_id(dir));
                            if (right && expandable && !open) || (left && open) || (enter && expandable) {
                                ev.push(Event::Toggle(dir));
                                None
                            } else if right && open {
                                Some((c + 1).min(n - 1))
                            } else if left {
                                self.parent_row(c)
                            } else {
                                None
                            }
                        }
                        _ if left => self.parent_row(c),
                        _ => None,
                    }
                }
            }
        };
        if copy {
            if let Some(c) = cur {
                if let Some(text) = self.row_path(ix, c) {
                    cx.actions.push(Action::CopyText(text));
                }
            }
        }
        if let Some(t) = target {
            if let (Some(s), Some(tg)) = (self.row_sel(ix, t), self.row_target(t)) {
                ev.push(Event::Select(s, tg));
            }
            // keep the target row in view
            let top = t as f32 * ROW_H;
            let vh = self.view.viewport_h.max(ROW_H);
            if top < self.view.offset {
                self.view.scroll_to = Some(top);
            } else if top + ROW_H > self.view.offset + vh {
                self.view.scroll_to = Some(top + ROW_H - vh);
            }
        }
    }

    fn parent_row(&self, c: usize) -> Option<usize> {
        let d = self.view.rows[c].depth;
        if d == 0 {
            return None;
        }
        (0..c).rev().find(|&j| self.view.rows[j].depth < d && matches!(self.view.rows[j].kind, RowKind::Dir { .. }))
    }

    fn row_sel(&self, ix: &Index, i: usize) -> Option<Sel> {
        match self.view.rows.get(i)?.kind {
            RowKind::Dir { dir, .. } => Some(Sel::Dir(ix.dir_db_id(dir))),
            RowKind::File { dir, index } => self.view.files.get(&dir)?.get(index).map(|f| Sel::File(f.db_id)),
            RowKind::Loading { .. } => None,
        }
    }

    fn row_target(&self, i: usize) -> Option<Target> {
        match self.view.rows.get(i)?.kind {
            RowKind::Dir { dir, .. } => Some(Target::Dir(dir)),
            RowKind::File { dir, index } => {
                self.view.files.get(&dir)?.get(index).map(|f| Target::File { file: f.file, content: f.content })
            }
            RowKind::Loading { .. } => None,
        }
    }

    fn row_path(&self, ix: &Index, i: usize) -> Option<String> {
        match self.view.rows.get(i)?.kind {
            RowKind::Dir { dir, .. } => Some(ix.dir_path(dir)),
            RowKind::File { dir, index } => {
                self.view.files.get(&dir)?.get(index).map(|f| ix.path_with_name(dir, &f.name))
            }
            RowKind::Loading { .. } => None,
        }
    }

    fn selected_row(&self, ix: &Index) -> Option<usize> {
        let sel = self.selected?;
        (0..self.view.rows.len()).find(|&i| self.row_sel(ix, i) == Some(sel))
    }

    fn apply(&mut self, ev: Vec<Event>, ix: &Index, notes: &mut Vec<String>) {
        for e in ev {
            match e {
                Event::Toggle(dir) => {
                    let id = ix.dir_db_id(dir);
                    if self.expanded.contains(&id) {
                        self.expanded.remove(&id);
                    } else {
                        for d in tree::expansion_chain(ix, dir, &self.opts) {
                            self.expanded.insert(ix.dir_db_id(d));
                        }
                    }
                    self.invalidate();
                }
                Event::Select(s, _) => self.selected = Some(s),
                Event::Filter(f) => {
                    self.opts.filter = f;
                    self.invalidate();
                }
                Event::Sort(k) => {
                    if self.opts.sort == k {
                        self.opts.descending = !self.opts.descending;
                    } else {
                        self.opts.sort = k;
                        // names A to Z, numbers largest first
                        self.opts.descending = k != SortKey::Name;
                    }
                    self.invalidate();
                }
                Event::Root(r) => {
                    if self.root != r {
                        self.set_root(ix, r);
                    }
                }
                Event::CollapseAll => {
                    self.expanded.clear();
                    self.invalidate();
                }
                Event::ExpandBelow(dir) => {
                    let (levels, capped) = expand_levels(ix, dir, &mut self.expanded, MAX_EXPAND);
                    if capped {
                        notes.push(format!(
                            "Expanded {} below {}; the next level has too many folders to open at once",
                            format::plural(levels as u64, "level", "levels"),
                            ix.dir_name(dir)
                        ));
                    }
                    self.invalidate();
                }
                Event::CollapseBelow(dir) => {
                    let mut stack = vec![dir];
                    while let Some(d) = stack.pop() {
                        self.expanded.remove(&ix.dir_db_id(d));
                        stack.extend_from_slice(ix.dir_children(d));
                    }
                    self.invalidate();
                }
            }
        }
    }
}

/// Expand `dir` and the levels below it, a whole level at a time, stopping before a level that
/// would take the total past `max` (a half-open level is confusing). Returns (levels, capped).
fn expand_levels(ix: &Index, dir: DirIx, expanded: &mut HashSet<i64>, max: usize) -> (usize, bool) {
    let mut level = vec![dir];
    let mut total = 0;
    let mut levels = 0;
    loop {
        if total + level.len() > max {
            return (levels, true);
        }
        for &d in &level {
            expanded.insert(ix.dir_db_id(d));
        }
        total += level.len();
        levels += 1;
        let next: Vec<DirIx> = level.iter().flat_map(|&d| ix.dir_children(d).iter().copied()).collect();
        if next.is_empty() {
            return (levels, false);
        }
        level = next;
    }
}

fn inner(x: Rangef, row: Rect) -> Rect {
    Rect::from_x_y_ranges(Rangef::new(x.min + PAD, x.max - PAD), row.y_range())
}

/// Size: bar for the share of the parent (colour = absolute size), then the number.
fn size_cell(p: &egui::Painter, x: Rangef, row: Rect, bytes: u64, parent: u64, dim: bool) {
    let r = inner(x, row);
    let text_w = 64.0f32.min(r.width());
    let ink = if dim { colors::INK_MUTED } else { colors::INK };
    widgets::text_right(p, r, &format::bytes(bytes), FontId::proportional(13.0), ink);
    let bar_r = Rect::from_x_y_ranges(Rangef::new(r.left(), r.right() - text_w - 8.0), row.y_range());
    if bar_r.width() >= 16.0 {
        let share = if parent > 0 { bytes as f32 / parent as f32 } else { 0.0 };
        let mut c = colors::size_color(bytes);
        if dim {
            c = colors::alpha(c, 0.55);
        }
        widgets::bar(p, bar_r, share, c);
    }
}

fn elsewhere_dir_cell(p: &egui::Painter, x: Rangef, row: Rect, st: &DirStats, font: FontId) {
    let r = inner(x, row);
    if st.fully_elsewhere() {
        // the word carries the meaning; the green tick only reinforces it
        let tr = widgets::text_right(p, r, "all", font.clone(), colors::INK);
        p.text(Pos2::new(tr.left() - 3.0, r.center().y), Align2::RIGHT_CENTER, "✔", font, colors::GOOD);
    } else {
        let ink = if st.elsewhere_bytes > 0 { colors::INK_SECONDARY } else { colors::INK_MUTED };
        widgets::text_right(p, r, &format::percent(st.pct_elsewhere()), font, ink);
    }
    let bar_r = Rect::from_x_y_ranges(Rangef::new(r.left(), r.right() - 52.0), row.y_range());
    if bar_r.width() >= 16.0 {
        widgets::bar(p, bar_r, (st.pct_elsewhere() / 100.0) as f32, colors::DUP);
    }
}

fn source_icon(p: &egui::Painter, rect: Rect, kind: PathKind) {
    let c = rect.center();
    let col = colors::INK_SECONDARY;
    match kind {
        PathKind::Cloud => {
            p.circle_filled(Pos2::new(c.x - 3.0, c.y + 1.0), 3.5, col);
            p.circle_filled(Pos2::new(c.x + 2.0, c.y - 1.0), 4.5, col);
            p.rect_filled(
                Rect::from_min_max(Pos2::new(c.x - 6.0, c.y + 1.0), Pos2::new(c.x + 6.0, c.y + 4.5)),
                CornerRadius::same(2),
                col,
            );
        }
        _ => {
            // a drive: rounded box with an activity light
            let r = Rect::from_center_size(c, Vec2::new(13.0, 8.0));
            p.rect_stroke(r, CornerRadius::same(2), Stroke::new(1.2, col), egui::StrokeKind::Inside);
            p.circle_filled(Pos2::new(r.right() - 3.5, c.y), 1.3, col);
        }
    }
}

/// Short description of an active filter for its button: "unique · not in truenas".
fn filter_summary(ix: &Index, f: &Filter) -> String {
    let mut parts: Vec<String> = Vec::new();
    match f.copies {
        CopiesFilter::Any => {}
        CopiesFilter::Unique => parts.push("unique".into()),
        CopiesFilter::Duplicated => parts.push("with copies".into()),
        CopiesFilter::InOtherSources => parts.push("copy in another source".into()),
        CopiesFilter::OnlyThisSource => parts.push("no copy in another source".into()),
    }
    let host = |id: i64| ix.source_ix(id).map(|s| format::host(&ix.sources()[s as usize].host).to_string());
    if let Some(id) = f.absent_from {
        parts.push(format!("not in {}", host(id).unwrap_or_else(|| "a removed source".into())));
    }
    if let Some(id) = f.present_in {
        parts.push(format!("in {}", host(id).unwrap_or_else(|| "a removed source".into())));
    }
    match f.hash {
        HashFilter::Any => {}
        HashFilter::Sha256 => parts.push("SHA-256".into()),
        HashFilter::Flagged => parts.push("flagged hashes".into()),
    }
    parts.join(" · ")
}

fn kind_label(kind: PathKind) -> &'static str {
    match kind {
        PathKind::Windows => "Windows",
        PathKind::Posix => "Linux / NAS",
        PathKind::Cloud => "Cloud",
        PathKind::Relative => "Other",
    }
}

fn dir_tooltip(ui: &mut Ui, ix: &Index, dir: DirIx, st: &DirStats) {
    ui.label(egui::RichText::new(ix.dir_path(dir)).color(colors::INK));
    ui.label(format!(
        "{} · {}\n{} of them have another copy somewhere\n{} of the bytes also exist outside this folder",
        format::bytes(st.bytes),
        format::plural(st.files as u64, "file", "files"),
        format::count(st.dup_files as u64),
        format::percent(st.pct_elsewhere())
    ));
}

fn file_tooltip(ui: &mut Ui, ix: &Index, f: &FileRow) {
    ui.label(egui::RichText::new(&f.name).color(colors::INK));
    let mut s = format!("{}\n{}", format::bytes(f.size), format::plural(f.copies as u64, "copy", "copies"));
    if f.copies > 1 {
        s.push_str(&format!(" in total, {} in other sources", format::count(f.copies_other_sources as u64)));
    }
    if f.flagged {
        s.push_str(&format!(
            "\n⚠ Hash is {} (not SHA-256): only compared with other {} hashes",
            ix.content_algo_name(f.content),
            ix.content_algo_name(f.content)
        ));
    }
    ui.label(s);
}

/// "Reveal in Explorer" for paths on this computer.
fn reveal_item(ui: &mut Ui, host: &str, path: &str, cx: &mut PaneCtx) {
    if !cfg!(windows) {
        return;
    }
    let here = crate::actions::is_this_computer(host);
    let exists = here && std::path::Path::new(path).exists();
    let resp = ui.add_enabled(exists, egui::Button::new("Show in Explorer"));
    let resp = if !here {
        resp.on_disabled_hover_text(format!("{host} is not this computer"))
    } else {
        resp.on_disabled_hover_text("Not found on this computer (moved or deleted?)")
    };
    if resp.clicked() {
        cx.actions.push(Action::Reveal(path.to_string()));
        ui.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dupemap_core::ignore::IgnoreSettings;
    use dupemap_core::manifest::HEADER;
    use dupemap_core::Db;

    #[test]
    fn expand_below_opens_whole_levels_only() {
        // /r with 3 subfolders, each with 4 subfolders: 1 + 3 + 12 folders below /
        let mut body = String::new();
        let h = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        for a in 0..3 {
            for b in 0..4 {
                body.push_str(&format!("{h}\t1\tnas\t/r/a{a}/b{b}/f.bin\n"));
            }
        }
        let text = format!("{HEADER}\n{body}");
        let mut db = Db::open_in_memory().unwrap();
        db.import_reader("m.tsv", "m.tsv", 0, None, text.as_bytes(), &mut |_| true).unwrap();
        let ix = Index::build_with(&db, &IgnoreSettings::disabled(), &mut |_, _| {}).unwrap();
        let r = ix.dir_children(ix.dir_children(ix.sources()[0].root)[0])[0]; // "/" -> "r"
        assert_eq!(ix.dir_name(r), "r");
        let mut set = HashSet::new();
        assert_eq!(expand_levels(&ix, r, &mut set, 100), (3, false));
        assert_eq!(set.len(), 16);
        let mut set = HashSet::new();
        assert_eq!(expand_levels(&ix, r, &mut set, 10), (2, true), "the 12 folders of level 3 do not fit");
        assert_eq!(set.len(), 4);
    }

    #[test]
    fn columns_fit_and_resize_keeps_the_right_edge() {
        let mut p = TreePane::new(Root::AllSources);
        let c = p.columns(0.0, 1000.0);
        assert_eq!(c.elsewhere.max, 1000.0);
        assert!((c.name.max - (1000.0 - 170.0 - 78.0 - 76.0 - 170.0)).abs() < 0.01);
        // narrow window: Files goes first, so the name keeps its minimum
        let c = p.columns(0.0, 400.0);
        assert!(c.name.span() >= NAME_MIN - 0.01);
        assert_eq!(c.shown, [true, false, true, true]);
        assert_eq!(c.files.span(), 0.0);
        assert!(!c.adjustable, "nothing to drag when the numbers are at their minimum");
        assert!(p.columns(0.0, 1000.0).adjustable);
        // narrower: then Copies, then Elsewhere; Size always stays
        assert_eq!(p.columns(0.0, 300.0).shown, [true, false, false, true]);
        assert_eq!(p.columns(0.0, 200.0).shown, [true, false, false, false]);
        assert!(p.columns(0.0, 200.0).size.span() > 0.0);
        // narrow: the bar columns shrink first, so Files and Copies keep their width longer
        let c = p.columns(0.0, 760.0);
        assert!(
            (c.files.span() - 78.0).abs() < 0.01 && (c.copies.span() - 76.0).abs() < 0.01,
            "{:?}",
            (c.files, c.copies)
        );
        assert!(c.size.span() < 170.0 && c.size.span() >= MIN_W[0] - 0.01);
        // a half-width split pane keeps a usable name column
        let c = p.columns(0.0, 720.0);
        assert!(c.name.span() >= 0.5 * 720.0 - 0.01, "{}", c.name.span());
        assert_eq!(c.elsewhere.max, 720.0);
        // a third of a small screen: about half for the name, and the numbers keep their minimum
        let c = p.columns(0.0, 500.0);
        assert_eq!(c.shown, [true; 4]);
        assert!(c.name.span() >= 235.0);
        assert!(c.files.span() >= MIN_W[1] - 0.01 && c.copies.span() >= MIN_W[2] - 0.01);
        // dragging the name|size boundary left widens Size only
        p.resize(0, -30.0, [true; 4]);
        assert_eq!(p.widths.size, 200.0);
        assert_eq!(p.widths.files, 78.0);
        // dragging files|copies right moves width from Copies to Files
        p.resize(2, 10.0, [true; 4]);
        assert_eq!((p.widths.files, p.widths.copies), (88.0, 66.0));
        // never below the minimum
        p.resize(3, 1000.0, [true; 4]);
        assert_eq!(p.widths.elsewhere, MIN_W[3]);
        let before = p.widths.clone();
        p.resize(1, -1000.0, [true; 4]);
        assert_eq!(p.widths, before, "a drag that would crush the left neighbour is ignored");
        // with Files hidden, the Size|Copies boundary trades width between Size and Copies
        let before = p.widths.clone();
        p.resize(2, 10.0, [true, false, true, true]);
        assert_eq!(p.widths.files, before.files);
        assert_eq!((p.widths.size, p.widths.copies), (before.size + 10.0, before.copies - 10.0));
    }
}
