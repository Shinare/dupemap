//! The treemap tab: what a folder (or every source) holds, as nested rectangles sized by their
//! bytes and coloured by where the copies are, by file type, or by the marks for deletion.
//!
//! Layouts are made on the query thread (core `treemap`) for the tab's size; while a new one is
//! on its way the old one is stretched to fit. Clicks do what they do in a tree: select (the
//! inspector and the highlighting follow), double-click a folder to zoom into it, right-click
//! for the same menu, Del to mark.

use crate::actions::Action;
use crate::colors;
use crate::format;
use crate::marks_ui::MarkBook;
use crate::selection::{Linked, Target};
use crate::widgets;
use crate::worker::{Query, Worker};
use dupemap_core::db::Db;
use dupemap_core::index::{DirIx, Filter, Index, Node};
use dupemap_core::marks::{MarkState, Place};
use dupemap_core::summary::Categories;
use dupemap_core::treemap::{self, Item, Treemap};
use egui::{Align2, Color32, CornerRadius, FontId, Key, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, Vec2};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

/// What the colours show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ColorMode {
    #[default]
    Duplication,
    FileType,
    Marks,
}

impl ColorMode {
    const ALL: [ColorMode; 3] = [ColorMode::Duplication, ColorMode::FileType, ColorMode::Marks];

    fn label(self) -> &'static str {
        match self {
            ColorMode::Duplication => "Where the copies are",
            ColorMode::FileType => "File type",
            ColorMode::Marks => "Marked for deletion",
        }
    }

    /// Order of the colour bands in a box (the most telling first).
    fn band_order(self, classes: usize) -> Vec<u8> {
        match self {
            ColorMode::Duplication | ColorMode::Marks => vec![2, 1, 0],
            ColorMode::FileType => (0..classes.min(treemap::CLASSES) as u8).collect(),
        }
    }
}

/// Everything a layout depends on.
#[derive(Debug, Clone, PartialEq)]
pub struct TreemapKey {
    pub pane: u64,
    pub generation: u64,
    pub focus: Option<DirIx>,
    pub filter: Filter,
    pub mode: ColorMode,
    pub size: [u32; 2],
    /// The categories' version (file type) or the marks' (generation, version) the colours
    /// came from; 0 when the colours do not depend on them.
    pub version: (u64, u64),
}

pub struct TreemapRequest {
    pub key: TreemapKey,
    pub categories: Arc<Categories>,
    pub marks: Option<Arc<MarkState>>,
}

pub struct TreemapView {
    pub key: TreemapKey,
    pub tm: Treemap,
    /// Per box: its name (folders, files; empty for the rest).
    pub labels: Vec<String>,
    /// Colour classes: names, and bytes over everything shown.
    pub classes: Vec<String>,
    pub totals: Vec<u64>,
    /// The box of each folder and file drawn, and of each folder's rest.
    at: HashMap<Item, usize>,
    rests: HashMap<Option<DirIx>, usize>,
}

impl TreemapView {
    /// The box showing a file or folder: its own, or else the closed folder or folded rest
    /// holding it. `None` when it is outside what is shown.
    fn drawn(&self, ix: &Index, item: Item) -> Option<usize> {
        if let Some(&b) = self.at.get(&item) {
            return Some(b);
        }
        let mut d = match item {
            Item::File(f) => Some(ix.file_dir(f)),
            Item::Dir(d) => ix.dir_parent(d),
            Item::Rest { .. } => None,
        };
        while let Some(x) = d {
            if let Some(&b) = self.at.get(&Item::Dir(x)) {
                let bx = &self.tm.boxes[b];
                if !bx.open {
                    return Some(b);
                }
                // open, so what leads down to it was folded into its rest
                return Some(self.rests.get(&Some(bx.through.unwrap_or(x))).copied().unwrap_or(b));
            }
            d = ix.dir_parent(x);
        }
        // a source folded among the sources
        match item {
            Item::Dir(d) if ix.dir_parent(d).is_none() => self.rests.get(&None).copied(),
            _ => self.rests.get(&None).copied().filter(|_| self.key.focus.is_none()),
        }
    }
}

/// Make a layout (query thread).
pub fn make_view(ix: &Index, db: &Db, req: &TreemapRequest) -> TreemapView {
    let k = &req.key;
    let node = k.focus.map(Node::Dir).unwrap_or(Node::AllSources);
    let area = treemap::Rect::new(0.0, 0.0, k.size[0] as f32, k.size[1] as f32);
    let opts = treemap::Options::default();
    let build = |class: &dyn Fn(u32) -> u8| treemap::build(ix, node, &k.filter, area, &opts, class);
    let (classes, tm): (Vec<String>, Treemap) = match k.mode {
        ColorMode::Duplication => {
            (["Unique", "Same source", "Elsewhere"].map(String::from).to_vec(), build(&|f| ix.status_of(f) as u8))
        }
        ColorMode::FileType => {
            // category n is class n (the last few share one), "Other" the last class
            let cats = &req.categories.list;
            let class_of = |c: usize| if c >= cats.len() { OTHER_CLASS } else { c.min(OTHER_CLASS as usize - 1) as u8 };
            let ext_class: Vec<u8> =
                (0..ix.ext_count()).map(|e| class_of(req.categories.category_of(ix.ext_name(e as u32)))).collect();
            let mut names = vec![String::new(); treemap::CLASSES];
            for (i, c) in cats.iter().enumerate() {
                let n = &mut names[class_of(i) as usize];
                if n.is_empty() {
                    *n = c.name.clone();
                } else if !n.ends_with('…') {
                    n.push_str(", …");
                }
            }
            names[OTHER_CLASS as usize] = dupemap_core::summary::OTHER.to_string();
            (names, build(&|f| ext_class[ix.file_ext_id(f) as usize]))
        }
        ColorMode::Marks => {
            let st = req.marks.clone();
            let class = move |f| match &st {
                Some(s) if s.is_last_copy(f) => 2,
                Some(s) if s.is_marked(f) => 1,
                _ => 0,
            };
            (["Not marked", "Marked", "Last copy marked"].map(String::from).to_vec(), build(&class))
        }
    };
    let labels = tm
        .boxes
        .iter()
        .map(|b| match b.item {
            Item::Dir(d) => match b.through {
                // a chain of single folders under one header: "truenas › / › mnt"
                Some(t) => {
                    let mut chain = vec![t];
                    while let Some(p) = ix.dir_parent(*chain.last().unwrap()).filter(|&p| p != d) {
                        chain.push(p);
                    }
                    let mut label = dir_label(ix, d);
                    for &c in chain.iter().rev() {
                        label.push_str(" › ");
                        label.push_str(ix.dir_name(c));
                    }
                    label
                }
                None => dir_label(ix, d),
            },
            Item::File(f) => db.file_record(ix.file_db_id(f)).map(|r| r.name).unwrap_or_default(),
            Item::Rest { .. } => String::new(),
        })
        .collect();
    let mut totals = vec![0u64; classes.len()];
    for (i, b) in tm.boxes.iter().enumerate() {
        if !b.open {
            for &(c, bytes) in tm.mix(i) {
                if let Some(t) = totals.get_mut(c as usize) {
                    *t += bytes;
                }
            }
        }
    }
    let mut at = HashMap::new();
    let mut rests = HashMap::new();
    for (i, b) in tm.boxes.iter().enumerate() {
        match b.item {
            Item::Rest { in_dir, .. } => {
                rests.insert(in_dir, i);
            }
            it => {
                at.insert(it, i);
            }
        }
    }
    TreemapView { key: k.clone(), tm, labels, classes, totals, at, rests }
}

fn dir_label(ix: &Index, d: DirIx) -> String {
    if ix.dir_parent(d).is_none() {
        format::host(&ix.sources()[ix.dir_source(d) as usize].host).to_string()
    } else {
        ix.dir_name(d).to_string()
    }
}

/// How a colour class is painted.
#[derive(Clone, Copy)]
enum Fill {
    Solid(Color32),
    /// Copies only in the same source: orange stripes, as in the summary.
    Stripes,
}

fn class_fill(mode: ColorMode, class: u8) -> Fill {
    match mode {
        ColorMode::Duplication => match class {
            2 => Fill::Solid(colors::DUP),
            1 => Fill::Stripes,
            _ => Fill::Solid(crate::summary_ui::UNIQUE),
        },
        ColorMode::FileType => {
            Fill::Solid(colors::CATEGORY.get(class as usize).copied().unwrap_or(crate::summary_ui::UNIQUE))
        }
        ColorMode::Marks => match class {
            2 => Fill::Solid(colors::CRITICAL),
            1 => Fill::Solid(colors::SERIOUS),
            _ => Fill::Solid(NOT_MARKED),
        },
    }
}

const NOT_MARKED: Color32 = Color32::from_rgb(0x45, 0x44, 0x41);
/// Outline of the copies of the selection (orange step 250).
const LINKED: Color32 = Color32::from_rgb(0xfe, 0x90, 0x68);
/// File types: the class of "Other" (categories past the eighth are grey too).
const OTHER_CLASS: u8 = (treemap::CLASSES - 1) as u8;

/// The ink for a label on a fill.
fn ink_on(fill: Fill) -> Color32 {
    match fill {
        Fill::Solid(c) => colors::label_on(c),
        Fill::Stripes => colors::INK,
    }
}

fn paint_fill(p: &egui::Painter, r: Rect, fill: Fill) {
    match fill {
        Fill::Solid(c) => {
            p.rect_filled(r, CornerRadius::ZERO, c);
        }
        Fill::Stripes => {
            p.rect_filled(r, CornerRadius::ZERO, colors::alpha(colors::DUP, 0.28));
            if r.width() >= 4.0 && r.height() >= 4.0 {
                let clip = p.with_clip_rect(r.intersect(p.clip_rect()));
                let mut x = r.left() - r.height();
                while x < r.right() {
                    clip.line_segment(
                        [Pos2::new(x, r.bottom()), Pos2::new(x + r.height(), r.top())],
                        Stroke::new(1.4, colors::DUP),
                    );
                    x += 4.0;
                }
            }
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct TreemapPane {
    /// The folder shown, by database id; `None` = every source.
    focus: Option<i64>,
    #[serde(default)]
    mode: ColorMode,
    #[serde(default)]
    filter: Filter,
    #[serde(skip)]
    view: Option<Arc<TreemapView>>,
    #[serde(skip)]
    pending: Option<TreemapKey>,
    /// The selected box's item, in the index of `selected_gen`.
    #[serde(skip)]
    selected: Option<(u64, Item)>,
    /// What was right-clicked (the menu stays open on it).
    #[serde(skip)]
    menu: Option<(u64, Item)>,
    #[serde(skip)]
    painted: Option<Arc<Painted>>,
}

/// A layout turned into shapes for one place on screen: one mesh for every tile and frame
/// (drawn as it is each frame), and what still needs drawing on top.
struct Painted {
    /// The view it was made from (its address; the pane holds the view while this is kept).
    view: usize,
    rect: Rect,
    ppp: f32,
    mesh: Arc<egui::Mesh>,
    /// Bands striped as "same source" big enough to show the stripes.
    stripes: Vec<Rect>,
    /// (line, text, small, ink)
    texts: Vec<(Rect, String, bool, Color32)>,
}

pub struct TreemapCtx<'a> {
    pub ix: &'a Arc<Index>,
    pub generation: u64,
    pub worker: &'a Worker,
    pub actions: &'a mut Vec<Action>,
    pub pane_id: u64,
    pub focused: bool,
    pub linked: Option<&'a Linked>,
    pub book: &'a MarkBook,
    pub marks: Option<&'a Arc<MarkState>>,
    /// (generation, version) of `marks`.
    pub marks_key: (u64, u64),
    pub categories: &'a Arc<Categories>,
    pub cats_version: u64,
}

impl TreemapPane {
    /// A treemap of a folder (by database id), or of every source.
    pub fn new(focus: Option<i64>) -> TreemapPane {
        TreemapPane { focus, ..Default::default() }
    }

    /// Show another folder (database id) or every source, keeping the colours and filter.
    pub fn set_focus(&mut self, focus: Option<i64>) {
        self.focus = focus;
    }

    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub fn title(&self, ix: &Index) -> String {
        match self.focus.and_then(|id| ix.dir_by_db_id(id)) {
            Some(d) => format!("Treemap: {}", dir_label(ix, d)),
            None => "Treemap: all sources".into(),
        }
    }

    pub fn on_view(&mut self, view: Arc<TreemapView>) {
        if self.pending.as_ref() == Some(&view.key) {
            self.pending = None;
        }
        self.view = Some(view);
    }

    fn focus_dir(&self, ix: &Index) -> Option<DirIx> {
        self.focus.and_then(|id| ix.dir_by_db_id(id))
    }

    fn zoom(&mut self, ix: &Index, d: Option<DirIx>) {
        self.focus = d.map(|d| ix.dir_db_id(d));
    }

    pub fn ui(&mut self, ui: &mut Ui, cx: &mut TreemapCtx) {
        let ix = cx.ix.clone();
        // a folder that is gone (re-import, ignored) leaves every source
        if self.focus.is_some() && self.focus_dir(&ix).is_none() {
            self.focus = None;
        }
        let focus = self.focus_dir(&ix);
        egui::Frame::new().inner_margin(egui::Margin::symmetric(8, 6)).show(ui, |ui| {
            self.toolbar(ui, &ix, focus, cx);
        });
        if let Some(v) = &self.view {
            if v.key.generation == cx.generation {
                ui.add_space(-2.0);
                egui::Frame::new().inner_margin(egui::Margin { left: 8, right: 8, top: 0, bottom: 4 }).show(ui, |ui| {
                    legend(ui, v);
                });
            }
        }
        // the tab's own rectangle, whatever the rows above asked for
        let max = ui.max_rect();
        let top = ui.cursor().top();
        let full = Rect::from_min_max(Pos2::new(max.left(), top), max.right_bottom());
        let resp = ui.allocate_rect(full, Sense::click());
        // (the visible part of the tab, even if a row above came out wider)
        let rect = full.intersect(ui.clip_rect());
        if resp.clicked() || resp.secondary_clicked() {
            cx.actions.push(Action::Focus(cx.pane_id));
        }
        let rect = rect.shrink2(Vec2::new(8.0, 0.0)).with_max_y(rect.bottom() - 6.0);
        if rect.width() < 20.0 || rect.height() < 20.0 {
            return;
        }
        self.request(rect, focus, cx);
        let Some(view) = self.view.clone().filter(|v| v.key.generation == cx.generation) else {
            ui.painter().text(
                rect.center(),
                Align2::CENTER_CENTER,
                "Laying out…",
                FontId::proportional(13.0),
                colors::INK_MUTED,
            );
            return;
        };
        if view.tm.boxes.is_empty() {
            ui.painter().text(
                rect.center(),
                Align2::CENTER_CENTER,
                if self.filter.is_all() { "Nothing here" } else { "No files here match the filter" },
                FontId::proportional(13.0),
                colors::INK_MUTED,
            );
            return;
        }
        // stretch a layout made for another size until the new one arrives
        let sx = rect.width() / view.key.size[0] as f32;
        let sy = rect.height() / view.key.size[1] as f32;
        let to_screen = |r: &treemap::Rect| -> Rect {
            Rect::from_min_size(Pos2::new(rect.left() + r.x * sx, rect.top() + r.y * sy), Vec2::new(r.w * sx, r.h * sy))
        };
        let hover = resp.hover_pos().and_then(|p| view.tm.hit((p.x - rect.left()) / sx, (p.y - rect.top()) / sy));
        let painter = ui.painter_at(rect);
        let painted = self.painted(&view, rect, &to_screen, ui.ctx().pixels_per_point());
        self.paint(&painter, &view, &painted, &to_screen, hover, cx);

        // pointer
        if let Some(b) = hover {
            let resp = resp.clone().on_hover_ui_at_pointer(|ui| tooltip(ui, &ix, &view, b));
            let item = view.tm.boxes[b].item;
            if resp.clicked() {
                self.select(&ix, item, cx);
            }
            if resp.double_clicked() {
                self.open(&ix, &view, b, cx);
            }
            if resp.secondary_clicked() {
                self.select(&ix, item, cx);
                self.menu = Some((cx.generation, item));
            }
        }
        if let Some((g, item)) = self.menu {
            if g == cx.generation {
                let name = view.tm.find(item).map(|b| view.labels[b].clone()).unwrap_or_default();
                let mut zoom = None;
                resp.context_menu(|ui| zoom = self.menu_ui(ui, &ix, item, &name, focus, cx));
                if let Some(z) = zoom {
                    self.zoom(&ix, z);
                }
            }
        }
        if cx.focused {
            self.keyboard(ui, &ix, &view, focus, cx);
        }
    }

    fn request(&mut self, rect: Rect, focus: Option<DirIx>, cx: &mut TreemapCtx) {
        let version = match self.mode {
            ColorMode::Duplication => (0, 0),
            ColorMode::FileType => (cx.cats_version, 0),
            ColorMode::Marks => cx.marks_key,
        };
        let key = TreemapKey {
            pane: cx.pane_id,
            generation: cx.generation,
            focus,
            filter: self.filter.clone(),
            mode: self.mode,
            size: [rect.width().round() as u32, rect.height().round() as u32],
            version,
        };
        let shown = self.view.as_ref().map(|v| &v.key);
        if shown == Some(&key) || self.pending.as_ref() == Some(&key) {
            return;
        }
        // colours by marks wait for the marks to be worked out
        if self.mode == ColorMode::Marks && cx.marks.is_none() {
            return;
        }
        let marks = if self.mode == ColorMode::Marks { cx.marks.cloned() } else { None };
        cx.worker.send(Query::Treemap(Box::new(TreemapRequest {
            key: key.clone(),
            categories: cx.categories.clone(),
            marks,
        })));
        self.pending = Some(key);
    }

    fn toolbar(&mut self, ui: &mut Ui, ix: &Index, focus: Option<DirIx>, cx: &mut TreemapCtx) {
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt(("treemap-mode", cx.pane_id))
                .selected_text(self.mode.label())
                .show_ui(ui, |ui| {
                    for m in ColorMode::ALL {
                        ui.selectable_value(&mut self.mode, m, m.label());
                    }
                })
                .response
                .on_hover_text("What the colours show");
            let narrow = ui.available_width() < 260.0;
            if let Some(f) = crate::pane::filter_button(ui, ix, &self.filter, cx.pane_id, narrow) {
                self.filter = f;
            }
        });
        ui.add_space(2.0);
        // where we are: All sources › source › ... › folder; a deep trail keeps its last three
        let mut zoom: Option<Option<DirIx>> = None;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 5.0;
            let trail: Vec<DirIx> = focus.map(|d| ix.dir_ancestors(d)).unwrap_or_default();
            if crumb(ui, "All sources", trail.is_empty()) {
                zoom = Some(None);
            }
            const KEEP: usize = 3;
            let hidden = trail.len().saturating_sub(KEEP);
            if hidden > 0 {
                ui.label(egui::RichText::new("›").color(colors::INK_MUTED));
                ui.menu_button("…", |ui| {
                    for &d in &trail[..hidden] {
                        if ui.button(dir_label(ix, d)).clicked() {
                            zoom = Some(Some(d));
                            ui.close();
                        }
                    }
                })
                .response
                .on_hover_text("The folders above");
            }
            for (i, &d) in trail.iter().enumerate().skip(hidden) {
                ui.label(egui::RichText::new("›").color(colors::INK_MUTED));
                if crumb(ui, &dir_label(ix, d), i + 1 == trail.len()) {
                    zoom = Some(Some(d));
                }
            }
        });
        if let Some(z) = zoom {
            if z != focus {
                self.zoom(ix, z);
            }
        }
    }

    /// The shapes of a layout at `rect`, made once and kept while neither changes.
    fn painted(
        &mut self,
        view: &Arc<TreemapView>,
        rect: Rect,
        to_screen: &dyn Fn(&treemap::Rect) -> Rect,
        ppp: f32,
    ) -> Arc<Painted> {
        let id = Arc::as_ptr(view) as usize;
        if let Some(p) = self.painted.as_ref().filter(|p| p.view == id && p.rect == rect && p.ppp == ppp) {
            return p.clone();
        }
        let tm = &view.tm;
        let order = self.mode.band_order(view.classes.len());
        let px = 1.0 / ppp;
        let snap = |v: f32| (v * ppp).round() / ppp;
        let mut mesh = egui::Mesh::default();
        let mut stripes = Vec::new();
        let mut texts = Vec::new();
        for (i, b) in tm.boxes.iter().enumerate() {
            let r = to_screen(&b.rect);
            // whole pixels, and a one-pixel gap to the right and below when there is room
            let (x0, y0, x1, y1) = (snap(r.left()), snap(r.top()), snap(r.right()), snap(r.bottom()));
            let gx = if x1 - x0 > 2.0 * px { px } else { 0.0 };
            let gy = if y1 - y0 > 2.0 * px { px } else { 0.0 };
            let r = Rect::from_min_max(Pos2::new(x0, y0), Pos2::new(x1 - gx, y1 - gy));
            if r.width() <= 0.0 || r.height() <= 0.0 {
                continue;
            }
            let label = &view.labels[i];
            if b.open {
                // a folder drawn with its contents: a frame, a header with its name and size
                let bg = if b.depth % 2 == 0 { colors::SURFACE } else { colors::SURFACE_RAISED };
                mesh.add_colored_rect(r, colors::BASELINE);
                mesh.add_colored_rect(r.shrink(px), bg);
                let header = Rect::from_min_size(r.min + Vec2::new(4.0, 1.0), Vec2::new(r.width() - 8.0, 14.0));
                if header.width() > 24.0 {
                    texts.push((header, format!("{label}  {}", format::bytes(b.bytes)), true, colors::INK_SECONDARY));
                }
                continue;
            }
            // drawn whole: bands of colour along the longer side
            let mix = tm.mix(i);
            let total: u64 = mix.iter().map(|m| m.1).sum();
            let mut dominant = (Fill::Solid(colors::SURFACE_ALT), 0u64);
            if total == 0 {
                mesh.add_colored_rect(r, colors::SURFACE_ALT);
            }
            let along_x = r.width() >= r.height();
            let len = if along_x { r.width() } else { r.height() };
            let mut at = 0.0f32;
            let mut done = 0u64;
            for &c in &order {
                let Some(&(_, bytes)) = mix.iter().find(|m| m.0 == c) else { continue };
                done += bytes;
                let end = if done >= total { len } else { snap(len * (done as f64 / total as f64) as f32) };
                let band = if along_x {
                    Rect::from_min_max(Pos2::new(r.left() + at, r.top()), Pos2::new(r.left() + end, r.bottom()))
                } else {
                    Rect::from_min_max(Pos2::new(r.left(), r.top() + at), Pos2::new(r.right(), r.top() + end))
                };
                let fill = class_fill(self.mode, c);
                match fill {
                    Fill::Solid(col) => mesh.add_colored_rect(band, col),
                    Fill::Stripes => {
                        mesh.add_colored_rect(band, colors::alpha(colors::DUP, 0.28));
                        if band.width() >= 12.0 && band.height() >= 12.0 {
                            stripes.push(band);
                        }
                    }
                }
                if bytes > dominant.1 {
                    dominant = (fill, bytes);
                }
                at = end;
            }
            if let Item::Rest { .. } = b.item {
                // the folded rest: dimmed, so it does not read as one big file
                mesh.add_colored_rect(r, colors::alpha(colors::PAGE, 0.45));
            }
            if r.width() >= 34.0 && r.height() >= 15.0 {
                let ink = if matches!(b.item, Item::Rest { .. }) { colors::INK } else { ink_on(dominant.0) };
                let name = match b.item {
                    Item::Rest { items, .. } => format!("{} smaller items", format::count(items as u64)),
                    _ => label.clone(),
                };
                let line = Rect::from_min_size(r.min + Vec2::new(4.0, 2.0), Vec2::new(r.width() - 8.0, 14.0));
                texts.push((line, name, false, ink));
                if r.height() >= 32.0 {
                    texts.push((
                        line.translate(Vec2::new(0.0, 15.0)),
                        format::bytes(b.bytes),
                        true,
                        colors::alpha(ink, 0.8),
                    ));
                }
            }
        }
        let p = Arc::new(Painted { view: id, rect, ppp, mesh: Arc::new(mesh), stripes, texts });
        self.painted = Some(p.clone());
        p
    }

    fn paint(
        &self,
        p: &egui::Painter,
        view: &TreemapView,
        painted: &Painted,
        to_screen: &dyn Fn(&treemap::Rect) -> Rect,
        hover: Option<usize>,
        cx: &TreemapCtx,
    ) {
        let tm = &view.tm;
        let selected = self.selected.filter(|(g, _)| *g == cx.generation).map(|(_, i)| i);
        p.add(egui::Shape::Mesh(painted.mesh.clone()));
        for &band in &painted.stripes {
            let clip = p.with_clip_rect(band.intersect(p.clip_rect()));
            let mut x = band.left() - band.height();
            while x < band.right() {
                clip.line_segment(
                    [Pos2::new(x, band.bottom()), Pos2::new(x + band.height(), band.top())],
                    Stroke::new(1.4, colors::DUP),
                );
                x += 4.0;
            }
        }
        let (font, small) = (FontId::proportional(12.5), FontId::proportional(11.5));
        for (line, text, is_small, ink) in &painted.texts {
            widgets::text_left(p, *line, text, if *is_small { small.clone() } else { font.clone() }, *ink);
        }
        // copies of the app-wide selection, the selection, the pointer
        if let Some(l) = cx.linked {
            // each copy, or the folder or rest box it is hidden in
            let mut boxes: Vec<usize> = match l.content {
                Some(c) => cx.ix.content_files(c).iter().filter_map(|&f| view.drawn(cx.ix, Item::File(f))).collect(),
                None => l.main_dirs.iter().filter_map(|&d| view.drawn(cx.ix, Item::Dir(d))).collect(),
            };
            boxes.sort_unstable();
            boxes.dedup();
            for b in boxes {
                p.rect_stroke(
                    to_screen(&tm.boxes[b].rect),
                    CornerRadius::same(2),
                    Stroke::new(2.5, LINKED),
                    StrokeKind::Inside,
                );
            }
        }
        if let Some(i) = selected.and_then(|it| tm.find(it)) {
            p.rect_stroke(
                to_screen(&tm.boxes[i].rect),
                CornerRadius::same(2),
                Stroke::new(2.5, colors::ACCENT),
                StrokeKind::Inside,
            );
        }
        if let Some(i) = hover {
            p.rect_stroke(
                to_screen(&tm.boxes[i].rect),
                CornerRadius::same(2),
                Stroke::new(1.5, colors::INK),
                StrokeKind::Inside,
            );
        }
    }

    fn select(&mut self, ix: &Index, item: Item, cx: &mut TreemapCtx) {
        self.selected = Some((cx.generation, item));
        cx.actions.push(Action::Focus(cx.pane_id));
        if let Some(t) = target(ix, item) {
            cx.actions.push(Action::Inspect(Some(t)));
        }
    }

    /// Double-click or Enter: into a folder, or a file shown in a tab.
    fn open(&mut self, ix: &Index, view: &TreemapView, b: usize, cx: &mut TreemapCtx) {
        match view.tm.boxes[b].item {
            Item::Dir(d) => self.zoom(ix, Some(view.tm.boxes[b].through.unwrap_or(d))),
            Item::Rest { in_dir: Some(d), .. } => self.zoom(ix, Some(d)),
            Item::Rest { in_dir: None, .. } => {}
            Item::File(f) => cx.actions.push(Action::ShowInTab(f)),
        }
    }

    fn keyboard(&mut self, ui: &mut Ui, ix: &Index, view: &TreemapView, focus: Option<DirIx>, cx: &mut TreemapCtx) {
        // not while a text field (the filter's, a search's) has the keyboard
        if ui.ctx().egui_wants_keyboard_input() {
            return;
        }
        let (up, enter, del) = ui.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::NONE, Key::Backspace),
                i.consume_key(egui::Modifiers::NONE, Key::Enter),
                i.consume_key(egui::Modifiers::NONE, Key::Delete),
            )
        });
        if up {
            if let Some(d) = focus {
                self.zoom(ix, ix.dir_parent(d));
            }
        }
        let Some(item) = self.selected.filter(|(g, _)| *g == cx.generation).map(|(_, i)| i) else { return };
        if enter {
            if let Some(b) = view.tm.find(item) {
                self.open(ix, view, b, cx);
            }
        }
        if del {
            if let (Some(st), Some(place)) = (cx.marks, place(item)) {
                let name = view.tm.find(item).map(|b| view.labels[b].clone());
                let name = if matches!(item, Item::File(_)) { name } else { None };
                cx.actions.push(crate::marks_ui::toggle(ix, cx.book, st, place, name.as_deref()));
            }
        }
    }

    /// The right-click menu. Returns a folder to zoom to.
    fn menu_ui(
        &mut self,
        ui: &mut Ui,
        ix: &Index,
        item: Item,
        name: &str,
        focus: Option<DirIx>,
        cx: &mut TreemapCtx,
    ) -> Option<Option<DirIx>> {
        let mut zoom = None;
        match item {
            Item::Dir(d) => {
                crate::marks_ui::menu(ui, ix, cx.book, cx.marks.map(|m| &**m), Place::Dir(d), None, cx.actions);
                ui.separator();
                if ui.button("Zoom in").clicked() {
                    zoom = Some(Some(d));
                    ui.close();
                }
                if ui.button("Where are the copies?").clicked() {
                    cx.actions.push(Action::Inspect(Some(Target::Dir(d))));
                    cx.actions.push(Action::OpenInspector);
                    ui.close();
                }
                if ui.button("Show in a tab").clicked() {
                    cx.actions.push(Action::ShowDirInTab(d));
                    ui.close();
                }
                if ix.dir_parent(d).is_some() {
                    ui.separator();
                    let path = ix.dir_path(d);
                    if ui.button("Copy path").clicked() {
                        cx.actions.push(Action::CopyText(path.clone()));
                        ui.close();
                    }
                    let host = &ix.sources()[ix.dir_source(d) as usize].host;
                    crate::pane::reveal_item(ui, host, &path, cx.actions);
                }
            }
            Item::File(f) => {
                crate::marks_ui::menu(ui, ix, cx.book, cx.marks.map(|m| &**m), Place::File(f), Some(name), cx.actions);
                ui.separator();
                if ui.button("Show every copy").clicked() {
                    cx.actions.push(Action::Inspect(Some(Target::File { file: f, content: ix.file_content(f) })));
                    cx.actions.push(Action::OpenInspector);
                    ui.close();
                }
                if ui.button("Show in a tab").clicked() {
                    cx.actions.push(Action::ShowInTab(f));
                    ui.close();
                }
                ui.separator();
                let path = ix.path_with_name(ix.file_dir(f), name);
                if ui.button("Copy path").clicked() {
                    cx.actions.push(Action::CopyText(path.clone()));
                    ui.close();
                }
                let host = &ix.sources()[ix.file_source(f) as usize].host;
                crate::pane::reveal_item(ui, host, &path, cx.actions);
            }
            Item::Rest { in_dir, items } => {
                ui.label(
                    egui::RichText::new(format!("{} items too small to draw", format::count(items as u64)))
                        .color(colors::INK_MUTED),
                );
                if let Some(d) = in_dir {
                    if ui.button("Zoom into their folder").clicked() {
                        zoom = Some(Some(d));
                        ui.close();
                    }
                    if ui.button("Show the folder in a tab").clicked() {
                        cx.actions.push(Action::ShowDirInTab(d));
                        ui.close();
                    }
                }
            }
        }
        if let Some(d) = focus {
            ui.separator();
            if ui.button("Up one level").clicked() {
                zoom = Some(ix.dir_parent(d));
                ui.close();
            }
        }
        zoom
    }
}

fn target(ix: &Index, item: Item) -> Option<Target> {
    match item {
        Item::Dir(d) => Some(Target::Dir(d)),
        Item::File(f) => Some(Target::File { file: f, content: ix.file_content(f) }),
        Item::Rest { .. } => None,
    }
}

fn place(item: Item) -> Option<Place> {
    match item {
        Item::Dir(d) => Some(Place::Dir(d)),
        Item::File(f) => Some(Place::File(f)),
        Item::Rest { .. } => None,
    }
}

/// A breadcrumb: a link, or plain text for where we are.
fn crumb(ui: &mut Ui, text: &str, current: bool) -> bool {
    if current {
        ui.label(egui::RichText::new(text).strong().color(colors::INK));
        false
    } else {
        widgets::link(ui, text).clicked()
    }
}

/// One line: a swatch and the bytes of each colour (largest first for file types).
fn legend(ui: &mut Ui, v: &TreemapView) {
    let mode = v.key.mode;
    let total: u64 = v.totals.iter().sum();
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let mut shown: Vec<usize> = (0..v.classes.len()).filter(|&c| v.totals[c] > 0).collect();
        match mode {
            ColorMode::FileType => {
                // eight colours in the categories' order; the rest share grey
                shown.sort_by_key(|&c| std::cmp::Reverse(v.totals[c]));
                let (coloured, grey): (Vec<usize>, Vec<usize>) =
                    shown.into_iter().partition(|&c| c < colors::CATEGORY.len());
                for c in coloured {
                    legend_item(ui, class_fill(mode, c as u8), &v.classes[c], v.totals[c], total, "");
                }
                let rest: u64 = grey.iter().map(|&c| v.totals[c]).sum();
                if rest > 0 {
                    let names: Vec<&str> = grey.iter().map(|&c| v.classes[c].as_str()).collect();
                    legend_item(ui, Fill::Solid(crate::summary_ui::UNIQUE), &names.join(", "), rest, total, "");
                }
            }
            _ => {
                for c in mode.band_order(v.classes.len()) {
                    let c = c as usize;
                    if c < v.totals.len() && v.totals[c] > 0 {
                        legend_item(
                            ui,
                            class_fill(mode, c as u8),
                            &v.classes[c],
                            v.totals[c],
                            total,
                            class_tip(mode, c),
                        );
                    }
                }
            }
        }
        if v.tm.truncated {
            ui.label(egui::RichText::new("· zoom in for more detail").small().color(colors::INK_MUTED));
        }
    });
}

fn legend_item(ui: &mut Ui, fill: Fill, name: &str, bytes: u64, total: u64, tip: &str) {
    // one widget of known width, so the line wraps between items, never inside one
    let pct = if total > 0 { bytes as f64 * 100.0 / total as f64 } else { 0.0 };
    let text = format!("{name} {} ({})", format::bytes(bytes), format::percent(pct));
    let galley = ui.painter().layout_no_wrap(text, FontId::proportional(11.5), colors::INK_SECONDARY);
    let size = Vec2::new(16.0 + galley.size().x, galley.size().y.max(12.0));
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    let swatch = Rect::from_min_size(Pos2::new(rect.left(), rect.center().y - 6.0), Vec2::splat(12.0));
    paint_fill(ui.painter(), swatch, fill);
    ui.painter().galley(
        Pos2::new(rect.left() + 16.0, rect.center().y - galley.size().y / 2.0),
        galley,
        colors::INK_SECONDARY,
    );
    if !tip.is_empty() {
        resp.on_hover_text(tip);
    }
    ui.add_space(6.0);
}

/// What a colour means, for the legend's hover.
fn class_tip(mode: ColorMode, class: usize) -> &'static str {
    match (mode, class) {
        (ColorMode::Duplication, 0) => "The only copy anywhere",
        (ColorMode::Duplication, 1) => "Other copies exist, but only in the file's own source",
        (ColorMode::Duplication, 2) => "At least one copy is in another source",
        (ColorMode::Marks, 1) => "Marked for deletion; another copy stays",
        (ColorMode::Marks, 2) => "Marked, and so is every other copy: deleting them all loses the file",
        _ => "",
    }
}

fn tooltip(ui: &mut Ui, ix: &Index, v: &TreemapView, b: usize) {
    let bx = &v.tm.boxes[b];
    match bx.item {
        Item::Dir(d) => {
            let path = if ix.dir_parent(d).is_none() { dir_label(ix, d) } else { ix.dir_path(d) };
            ui.label(egui::RichText::new(path).color(colors::INK));
            ui.label(format!("{} · {}", format::bytes(bx.bytes), format::plural(bx.files as u64, "file", "files")));
        }
        Item::File(f) => {
            ui.label(egui::RichText::new(ix.path_with_name(ix.file_dir(f), &v.labels[b])).color(colors::INK));
            let c = ix.file_content(f);
            let copies = ix.copies(c);
            let mut s = format!("{} · {}", format::bytes(bx.bytes), format::plural(copies as u64, "copy", "copies"));
            if copies > 1 {
                s.push_str(&format!(
                    " in total, {} in other sources",
                    format::count(ix.copies_other_sources(f) as u64)
                ));
            }
            ui.label(s);
        }
        Item::Rest { in_dir, items } => {
            let where_ = in_dir.map(|d| format!(" in {}", dir_label(ix, d))).unwrap_or_default();
            ui.label(
                egui::RichText::new(format!("{} smaller items{where_}", format::count(items as u64)))
                    .color(colors::INK),
            );
            ui.label(format!(
                "{} · {} — too small to draw one by one; double-click to zoom in",
                format::bytes(bx.bytes),
                format::plural(bx.files as u64, "file", "files")
            ));
        }
    }
    if !bx.open {
        let total: u64 = v.tm.mix(b).iter().map(|m| m.1).sum();
        if total > 0 {
            ui.add_space(2.0);
            for c in v.key.mode.band_order(v.classes.len()) {
                if let Some(&(_, bytes)) = v.tm.mix(b).iter().find(|m| m.0 == c) {
                    ui.horizontal(|ui| {
                        let (r, _) = ui.allocate_exact_size(Vec2::new(10.0, 10.0), Sense::hover());
                        paint_fill(ui.painter(), r, class_fill(v.key.mode, c));
                        let name = v.classes.get(c as usize).map(String::as_str).unwrap_or("?");
                        ui.label(format!(
                            "{name}: {} ({})",
                            format::bytes(bytes),
                            format::percent(bytes as f64 * 100.0 / total as f64)
                        ));
                    });
                }
            }
        }
    }
    ui.label(
        egui::RichText::new("Double-click a folder to zoom in, right-click for more").small().color(colors::INK_MUTED),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PC: D:\Big\b.bin (big), D:\Small\s0..s299.txt (tiny); NAS: /mnt/tank/b.bin (a copy).
    fn fixture() -> (Db, Arc<Index>) {
        let mut db = Db::open_in_memory().unwrap();
        let h = |n: u32| format!("{n:064x}");
        let mut pc = format!("hash\tsize\thost\tpath\n{}\t50000000\tPC D: (Data)\tD:\\Big\\b.bin\n", h(1));
        for i in 0..300 {
            pc.push_str(&format!("{}\t1000\tPC D: (Data)\tD:\\Small\\s{i}.txt\n", h(10 + i)));
        }
        let nas = format!("hash\tsize\thost\tpath\n{}\t50000000\tnas\t/mnt/tank/b.bin\n", h(1));
        db.import_reader("pc.tsv", "pc.tsv", 0, None, pc.as_bytes(), &mut |_| true).unwrap();
        db.import_reader("nas.tsv", "nas.tsv", 0, None, nas.as_bytes(), &mut |_| true).unwrap();
        let ix = Arc::new(Index::build(&db).unwrap());
        (db, ix)
    }

    fn view(db: &Db, ix: &Index, mode: ColorMode) -> TreemapView {
        let key = TreemapKey {
            pane: 1,
            generation: 1,
            focus: None,
            filter: Filter::default(),
            mode,
            size: [800, 500],
            version: (0, 0),
        };
        make_view(ix, db, &TreemapRequest { key, categories: Arc::new(Categories::default()), marks: None })
    }

    #[test]
    fn labels_totals_and_where_hidden_files_are_drawn() {
        let (db, ix) = fixture();
        let v = view(&db, &ix, ColorMode::Duplication);
        // the NAS's single-folder chain is one box, named along the way
        let nas = ix.sources().iter().find(|s| s.host == "nas").unwrap().root;
        let nb = v.tm.find(Item::Dir(nas)).unwrap();
        assert_eq!(v.labels[nb], "nas › / › mnt › tank");
        // the big file is drawn and named; both copies count as "elsewhere"
        let big: Vec<u32> = (0..ix.file_count() as u32).filter(|&f| ix.file_size(f) == 50_000_000).collect();
        for &f in &big {
            assert!(v.labels[v.drawn(&ix, Item::File(f)).unwrap()].ends_with("b.bin"));
        }
        assert_eq!(v.totals[2], 100_000_000);
        assert_eq!(v.totals[0], 300_000, "the small files are unique");
        // a tiny file is not drawn on its own: its folder's box (or its rest) stands for it
        let tiny = (0..ix.file_count() as u32).find(|&f| ix.file_size(f) == 1000).unwrap();
        let b = v.drawn(&ix, Item::File(tiny)).unwrap();
        assert!(matches!(v.tm.boxes[b].item, Item::Dir(_) | Item::Rest { .. }));
        assert!(v.tm.boxes[b].rect.contains(v.tm.boxes[b].rect.x + 0.5, v.tm.boxes[b].rect.y + 0.5));
    }

    #[test]
    fn file_types_put_other_in_grey() {
        let (db, ix) = fixture();
        let v = view(&db, &ix, ColorMode::FileType);
        let docs = Categories::default().list.iter().position(|c| c.name == "Documents").unwrap();
        assert_eq!(v.totals[docs], 300_000, ".txt is a document");
        assert_eq!(v.totals[OTHER_CLASS as usize], 100_000_000, ".bin is in no category");
        assert_eq!(v.classes[OTHER_CLASS as usize], "Other");
        assert!(
            matches!(class_fill(ColorMode::FileType, OTHER_CLASS), Fill::Solid(c) if c == crate::summary_ui::UNIQUE)
        );
    }
}
