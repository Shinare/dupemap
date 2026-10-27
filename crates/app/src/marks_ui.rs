//! Marks for deletion in the app: the rules with undo and redo, the menu entries and Del key
//! every list shares, the Marks tab, and the window that writes the delete scripts.

use crate::actions::Action;
use crate::colors;
use crate::format;
use crate::worker::ExportDone;
use dupemap_core::export::{ExportOptions, LinuxMode, WindowsMode};
use dupemap_core::index::{DirIx, Index, SourceIx};
use dupemap_core::marks::{edit, MarkAction, MarkRule, MarkState, Place, PlaceKey};
use dupemap_core::paths::PathKind;
use egui::{RichText, Ui};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;

/// One edit of the rules, for undo and redo.
#[derive(Debug, Clone)]
struct Change {
    label: String,
    added: Vec<MarkRule>,
    removed: Vec<MarkRule>,
}

/// The rules for deletion, as the user made them, with undo and redo.
#[derive(Debug, Default)]
pub struct MarkBook {
    rules: Vec<MarkRule>,
    by_key: HashMap<PlaceKey, usize>,
    undo: Vec<Change>,
    redo: Vec<Change>,
    /// Bumped on every change (states computed for older versions are dropped).
    pub version: u64,
    /// The rules were read from the database.
    pub loaded: bool,
}

impl MarkBook {
    pub fn rules(&self) -> &[MarkRule] {
        &self.rules
    }

    fn reindex(&mut self) {
        self.by_key = self.rules.iter().enumerate().map(|(i, r)| (r.place_key(), i)).collect();
    }

    /// The rules as stored (at start); clears undo and redo.
    pub fn load(&mut self, rules: Vec<MarkRule>) {
        self.rules = rules;
        self.reindex();
        self.undo.clear();
        self.redo.clear();
        self.version += 1;
        self.loaded = true;
    }

    /// The rule on this place, if there is one.
    pub fn get(&self, key: &PlaceKey) -> Option<&MarkRule> {
        self.by_key.get(key).map(|&i| &self.rules[i])
    }

    /// Add rules (replacing any on the same places) and remove others. `false` when nothing
    /// changed.
    pub fn apply(&mut self, label: String, add: &[MarkRule], remove: &[PlaceKey]) -> bool {
        let (added, removed) = edit(&mut self.rules, add, remove);
        if added.is_empty() && removed.is_empty() {
            return false;
        }
        self.reindex();
        self.undo.push(Change { label, added, removed });
        self.redo.clear();
        self.version += 1;
        true
    }

    fn revert(&mut self, c: &Change) {
        let keys: Vec<PlaceKey> = c.added.iter().map(MarkRule::place_key).collect();
        edit(&mut self.rules, &[], &keys);
        edit(&mut self.rules, &c.removed, &[]);
        self.reindex();
        self.version += 1;
    }

    fn replay(&mut self, c: &Change) {
        let keys: Vec<PlaceKey> = c.removed.iter().map(MarkRule::place_key).collect();
        edit(&mut self.rules, &[], &keys);
        edit(&mut self.rules, &c.added, &[]);
        self.reindex();
        self.version += 1;
    }

    /// Undo the last change; its label.
    pub fn undo(&mut self) -> Option<String> {
        let c = self.undo.pop()?;
        self.revert(&c);
        let label = c.label.clone();
        self.redo.push(c);
        Some(label)
    }

    pub fn redo(&mut self) -> Option<String> {
        let c = self.redo.pop()?;
        self.replay(&c);
        let label = c.label.clone();
        self.undo.push(c);
        Some(label)
    }

    pub fn undo_label(&self) -> Option<&str> {
        self.undo.last().map(|c| c.label.as_str())
    }

    pub fn redo_label(&self) -> Option<&str> {
        self.redo.last().map(|c| c.label.as_str())
    }
}

/// A rule on a file (its folder and name) or folder.
pub fn rule_for(ix: &Index, place: Place, name: Option<&str>, action: MarkAction) -> MarkRule {
    match place {
        Place::Dir(d) => MarkRule::for_dir(ix, d, action),
        Place::File(f) => MarkRule::for_file(ix, ix.file_dir(f), name.unwrap_or_default(), action),
    }
}

/// Every file under a folder is marked.
pub fn dir_fully_marked(ix: &Index, st: &MarkState, d: DirIx) -> bool {
    let n = ix.subtree_files(d).len() as u32;
    n > 0 && st.dir(d).files == n
}

fn is_marked(ix: &Index, st: &MarkState, place: Place) -> bool {
    match place {
        Place::File(f) => st.is_marked(f),
        Place::Dir(d) => dir_fully_marked(ix, st, d),
    }
}

fn short(ix: &Index, place: Place, name: Option<&str>) -> String {
    match (place, name) {
        (Place::File(_), Some(n)) => n.to_string(),
        (Place::Dir(d), _) if ix.dir_parent(d).is_none() => {
            format::host(&ix.sources()[ix.dir_source(d) as usize].host).to_string()
        }
        (Place::Dir(d), _) => ix.dir_name(d).to_string(),
        _ => "file".into(),
    }
}

/// The Del key: mark what is not marked, unmark what is.
pub fn toggle(ix: &Index, book: &MarkBook, st: &MarkState, place: Place, name: Option<&str>) -> Action {
    toggle_rule(book, is_marked(ix, st, place), rule_for(ix, place, name, MarkAction::Delete), &short(ix, place, name))
}

/// Mark (with `rule`, a delete rule) what is not marked now, unmark what is: a mark of its own
/// is removed, a mark from a folder above gets an exception.
pub fn toggle_rule(book: &MarkBook, marked_now: bool, rule: MarkRule, what: &str) -> Action {
    let key = rule.place_key();
    let own = book.get(&key).map(|r| r.action);
    if marked_now {
        match own {
            Some(MarkAction::Delete) | Some(MarkAction::DeleteDuplicates) => {
                Action::Mark { label: format!("unmark {what}"), add: vec![], remove: vec![key] }
            }
            // marked by a folder above: keep it as an exception
            _ => Action::Mark {
                label: format!("keep {what}"),
                add: vec![MarkRule { action: MarkAction::Keep, ..rule }],
                remove: vec![],
            },
        }
    } else {
        match own {
            Some(MarkAction::Keep) => Action::Mark { label: format!("unkeep {what}"), add: vec![], remove: vec![key] },
            _ => Action::Mark { label: format!("mark {what}"), add: vec![rule], remove: vec![] },
        }
    }
}

/// The marking entries of a right-click menu for a file or folder.
pub fn menu(
    ui: &mut Ui,
    ix: &Index,
    book: &MarkBook,
    st: Option<&MarkState>,
    place: Place,
    name: Option<&str>,
    actions: &mut Vec<Action>,
) {
    let Some(st) = st else {
        ui.add_enabled(false, egui::Button::new("Mark for deletion"));
        return;
    };
    let rule = rule_for(ix, place, name, MarkAction::Delete);
    let key = rule.place_key();
    let own = book.get(&key).map(|r| r.action);
    let what = short(ix, place, name);
    let marked = is_marked(ix, st, place);
    let is_dir = matches!(place, Place::Dir(_));
    let mut push = |label: String, add: Vec<MarkRule>, remove: Vec<PlaceKey>| {
        actions.push(Action::Mark { label, add, remove });
    };
    if !marked && own != Some(MarkAction::Delete) {
        let text = if is_dir { "Mark the folder for deletion" } else { "Mark for deletion" };
        if ui.add(egui::Button::new(text).shortcut_text("Del")).clicked() {
            push(format!("mark {what}"), vec![rule.clone()], vec![]);
            ui.close();
        }
    }
    if is_dir && own != Some(MarkAction::DeleteDuplicates) {
        let r = ui.button("Mark its files that have a copy outside it").on_hover_text(
            "Every file in this folder that also exists somewhere else (in another folder or source) is marked; \
             files only here are not",
        );
        if r.clicked() {
            push(
                format!("mark the duplicates in {what}"),
                vec![MarkRule { action: MarkAction::DeleteDuplicates, ..rule.clone() }],
                vec![],
            );
            ui.close();
        }
    }
    if marked && own.is_none() {
        let text = if is_dir {
            "Keep this folder (an exception to the mark above)"
        } else {
            "Keep this file (an exception to the mark above)"
        };
        if ui.add(egui::Button::new(text).shortcut_text("Del")).clicked() {
            push(format!("keep {what}"), vec![MarkRule { action: MarkAction::Keep, ..rule.clone() }], vec![]);
            ui.close();
        }
    }
    if let Some(a) = own {
        let text = match a {
            MarkAction::Keep => "Remove the exception (keep)",
            _ => "Remove the mark",
        };
        if ui.button(text).clicked() {
            push(format!("unmark {what}"), vec![], vec![key]);
            ui.close();
        }
    }
    if let Place::File(f) = place {
        if st.is_last_copy(f) {
            ui.label(RichText::new("⚠ Every copy of this file is marked").color(colors::CRITICAL).small());
        }
    }
}

/// How many files and bytes, as "1,234 files · 5.6 GB".
pub fn files_bytes(files: u64, bytes: u64) -> String {
    format!("{} · {}", format::plural(files, "file", "files"), format::bytes(bytes))
}

// ------------------------------------------------------------------------------- the Marks tab

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct MarksPane {}

pub struct MarksCtx<'a> {
    pub ix: &'a Index,
    pub book: &'a MarkBook,
    pub state: Option<&'a MarkState>,
    pub actions: &'a mut Vec<Action>,
    pub pane_id: u64,
    pub focused: bool,
}

impl MarksPane {
    pub fn title(&self) -> String {
        "Marked for deletion".into()
    }

    pub fn ui(&mut self, ui: &mut Ui, cx: &mut MarksCtx) {
        let full = ui.available_rect_before_wrap();
        if ui.rect_contains_pointer(full) && ui.input(|i| i.pointer.any_pressed()) && !cx.focused {
            cx.actions.push(Action::Focus(cx.pane_id));
        }
        egui::Frame::new().inner_margin(egui::Margin::symmetric(10, 8)).show(ui, |ui| {
            self.toolbar(ui, cx);
            ui.add_space(6.0);
            let Some(st) = cx.state else {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new());
                    ui.label(RichText::new("Working out what the marks mark…").color(colors::INK_MUTED));
                });
                return;
            };
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                if cx.book.rules().is_empty() {
                    empty_help(ui);
                    return;
                }
                totals(ui, st);
                if st.total.last_files > 0 {
                    ui.add_space(8.0);
                    last_copies(ui, cx.ix, st, cx.actions);
                }
                ui.add_space(10.0);
                self.rules(ui, cx, st);
            });
        });
    }

    fn toolbar(&mut self, ui: &mut Ui, cx: &mut MarksCtx) {
        ui.horizontal_wrapped(|ui| {
            let undo = cx.book.undo_label().map(|l| format!("Undo {l}"));
            if ui
                .add_enabled(undo.is_some(), egui::Button::new("Undo").shortcut_text("Ctrl+Z"))
                .on_hover_text(undo.unwrap_or_else(|| "Nothing to undo".into()))
                .clicked()
            {
                cx.actions.push(Action::Undo);
            }
            let redo = cx.book.redo_label().map(|l| format!("Redo {l}"));
            if ui
                .add_enabled(redo.is_some(), egui::Button::new("Redo").shortcut_text("Ctrl+Y"))
                .on_hover_text(redo.unwrap_or_else(|| "Nothing to redo".into()))
                .clicked()
            {
                cx.actions.push(Action::Redo);
            }
            ui.separator();
            let any = cx.state.is_some_and(|s| !s.is_empty());
            if ui
                .add_enabled(any, egui::Button::new(RichText::new("Write delete scripts…").strong()))
                .on_hover_text("A script per source that you run on the computer with those files")
                .clicked()
            {
                cx.actions.push(Action::OpenExport);
            }
        });
    }

    fn rules(&mut self, ui: &mut Ui, cx: &mut MarksCtx, st: &MarkState) {
        ui.label(RichText::new("Marks").strong());
        ui.label(
            RichText::new(
                "The most specific mark decides: a mark on a file beats one on its folder, a deeper folder beats \
                 a shallower one. Right-click a file or folder anywhere to change its mark; Del marks or unmarks \
                 the selected row.",
            )
            .small()
            .color(colors::INK_MUTED),
        );
        ui.add_space(4.0);
        // by source, then path
        let mut by_source: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (i, r) in cx.book.rules().iter().enumerate() {
            by_source.entry(r.host.clone()).or_default().push(i);
        }
        let rules = cx.book.rules();
        let effects = &st.rules;
        for (host, mut list) in by_source {
            list.sort_by(|&a, &b| dupemap_core::natsort::natural_cmp(&rules[a].path, &rules[b].path));
            ui.add_space(6.0);
            let src = cx.ix.sources().iter().position(|s| s.host == host);
            ui.horizontal(|ui| {
                ui.label(RichText::new(format::host(&host)).strong().color(colors::INK));
                if let Some(s) = src {
                    let t = st.sources[s];
                    ui.label(RichText::new(files_bytes(t.files, t.bytes)).color(colors::INK_SECONDARY));
                    if t.last_files > 0 {
                        ui.label(
                            RichText::new(format!("⚠ {} last copies", format::count(t.last_files)))
                                .color(colors::CRITICAL),
                        );
                    }
                } else {
                    ui.label(RichText::new("not imported now").color(colors::INK_MUTED));
                }
            });
            for i in list {
                let r = &rules[i];
                // the state may be one edit behind; then the effect is not shown yet
                let eff = effects.get(i).filter(|_| effects.len() == rules.len());
                let (word, ink) = match r.action {
                    MarkAction::Delete => ("Delete", colors::CRITICAL),
                    MarkAction::DeleteDuplicates => ("Delete copies", colors::CRITICAL),
                    MarkAction::Keep => ("Keep", colors::GOOD),
                };
                let (effect, effect_ink, tip) = match eff {
                    Some(e) if e.place.is_none() => {
                        ("not found: removed, renamed or ignored".to_string(), colors::INK_MUTED, None)
                    }
                    Some(e) if r.action == MarkAction::Keep => {
                        (format!("keeps {}", format::plural(e.decides, "file", "files")), colors::INK_SECONDARY, None)
                    }
                    Some(e) if e.marked.last_files > 0 => (
                        format!(
                            "{} · ⚠ {} last copies",
                            files_bytes(e.marked.files, e.marked.bytes),
                            format::count(e.marked.last_files)
                        ),
                        colors::CRITICAL,
                        Some("Some of these have no copy left that is not marked"),
                    ),
                    Some(e) if e.marked.files > 0 => (
                        files_bytes(e.marked.files, e.marked.bytes),
                        colors::INK_SECONDARY,
                        Some("Every one of them keeps a copy somewhere that is not marked"),
                    ),
                    Some(_) => ("marks nothing".to_string(), colors::INK_MUTED, None),
                    None => ("…".to_string(), colors::INK_MUTED, None),
                };
                egui::Sides::new().shrink_left().truncate().show(
                    ui,
                    |ui| {
                        ui.label(RichText::new(word).color(ink)).on_hover_text(r.action.label(r.is_dir));
                        ui.label(RichText::new(r.display()).color(colors::INK)).on_hover_text(r.display());
                    },
                    |ui| {
                        if ui.small_button("✕").on_hover_text("Remove this mark").clicked() {
                            cx.actions.push(Action::Mark {
                                label: format!("remove the mark on {}", r.display()),
                                add: vec![],
                                remove: vec![r.place_key()],
                            });
                        }
                        if let Some(place) = eff.and_then(|e| e.place) {
                            if ui.small_button("Show").clicked() {
                                cx.actions.push(match place {
                                    Place::Dir(d) => Action::ShowDirInTab(d),
                                    Place::File(f) => Action::ShowInTab(f),
                                });
                            }
                        }
                        let l = ui.label(RichText::new(effect).color(effect_ink));
                        if let Some(t) = tip {
                            l.on_hover_text(t);
                        }
                    },
                );
            }
        }
    }
}

fn empty_help(ui: &mut Ui) {
    ui.add_space(12.0);
    ui.label(RichText::new("Nothing is marked for deletion.").color(colors::INK_SECONDARY));
    ui.add_space(4.0);
    ui.label(
        RichText::new(
            "Right-click a folder or file in a tree, in the search results or among the copies in the inspector \
             to mark it, or press Del on the selected row. A folder can be marked whole, or only its files that \
             have a copy outside it. DupeMap never deletes anything itself: when you are done it writes a script \
             per source that you run there.",
        )
        .color(colors::INK_MUTED),
    );
}

fn totals(ui: &mut Ui, st: &MarkState) {
    let sources = st.sources.iter().filter(|s| s.files > 0).count();
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("Marked:").color(colors::INK_SECONDARY));
        ui.label(RichText::new(files_bytes(st.total.files, st.total.bytes)).strong().color(colors::INK));
        ui.label(
            RichText::new(format!("in {}", format::plural(sources as u64, "source", "sources")))
                .color(colors::INK_SECONDARY),
        );
    });
    let unknown = st.unknown_size;
    if unknown > 0 {
        ui.label(
            RichText::new(format!(
                "{} of them have no known size (from a checksum list) and count as 0 bytes",
                format::count(unknown)
            ))
            .small()
            .color(colors::INK_MUTED),
        );
    }
}

fn last_copies(ui: &mut Ui, ix: &Index, st: &MarkState, actions: &mut Vec<Action>) {
    egui::Frame::new()
        .fill(colors::alpha(colors::CRITICAL, 0.10))
        .stroke(egui::Stroke::new(1.0, colors::alpha(colors::CRITICAL, 0.6)))
        .corner_radius(4)
        .inner_margin(egui::Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                RichText::new(format!(
                    "⚠ {} would lose their last copy: every copy of them is marked.",
                    files_bytes(st.total.last_files, st.total.last_bytes)
                ))
                .strong()
                .color(colors::INK),
            );
            ui.label(
                RichText::new(
                    "The delete scripts leave them out unless you ask for them. Keep one copy of each (right-click → \
                     Keep, or remove a mark), or include them knowingly when you write the scripts.",
                )
                .small()
                .color(colors::INK_SECONDARY),
            );
            // where they are, by folder, largest first
            let v = st.last_copies_by_dir();
            const SHOWN: usize = 30;
            ui.add_space(2.0);
            for (d, n, b) in v.iter().take(SHOWN) {
                let host = &ix.sources()[ix.dir_source(*d) as usize].host;
                // the numbers and the button stay in view; a long path is cut instead
                egui::Sides::new().shrink_left().truncate().show(
                    ui,
                    |ui| {
                        ui.label(
                            RichText::new(format!("{}  {}", format::host(host), ix.dir_path(*d))).color(colors::INK),
                        )
                        .on_hover_text(ix.dir_path(*d));
                    },
                    |ui| {
                        if ui.small_button("Show").clicked() {
                            actions.push(Action::ShowDirInTab(*d));
                        }
                        ui.label(RichText::new(files_bytes(*n, *b)).color(colors::INK_SECONDARY));
                    },
                );
            }
            if v.len() > SHOWN {
                ui.label(
                    RichText::new(format!("… and {} more folders", v.len() - SHOWN)).small().color(colors::INK_MUTED),
                );
            }
        });
}

// ------------------------------------------------------------------------------- the export window

/// A request from the export window.
#[derive(Debug, Clone)]
pub struct ExportChoice {
    pub sources: Vec<SourceIx>,
    pub options: ExportOptions,
    pub dir: PathBuf,
}

#[derive(Default)]
pub struct ExportWindow {
    pub open: bool,
    options: ExportOptions,
    /// Sources left out, by host (survives index rebuilds).
    left_out: BTreeSet<String>,
    dir: String,
    pub running: bool,
    pub result: Option<Result<ExportDone, String>>,
}

fn script_kind(kind: PathKind) -> &'static str {
    match kind {
        PathKind::Windows => "PowerShell (Windows)",
        PathKind::Posix => "bash (Linux)",
        PathKind::Cloud => "rclone (PowerShell)",
        PathKind::Relative => "list only",
    }
}

impl ExportWindow {
    /// Open it, with a fresh folder name for the scripts.
    pub fn open(&mut self, default_dir: PathBuf) {
        self.open = true;
        self.result = None;
        self.dir = default_dir.display().to_string();
    }

    #[allow(unused_variables)]
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        ix: &Index,
        st: Option<&MarkState>,
        frame: Option<&eframe::Frame>,
        actions: &mut Vec<Action>,
    ) {
        if !self.open {
            return;
        }
        let mut open = true;
        egui::Window::new("Write delete scripts")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(680.0)
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(
                        "DupeMap never deletes anything itself: it writes a script per source, which you run on the \
                         computer that has those files. Each script checks every file first (still there, same \
                         size), shows what it will do and asks before it starts; try it with its dry run first.",
                    )
                    .color(colors::INK_SECONDARY),
                );
                ui.add_space(8.0);
                let Some(st) = st else {
                    ui.add(egui::Spinner::new());
                    return;
                };
                if let Some(r) = self.result.take() {
                    if !self.result_ui(ui, &r, actions) {
                        self.result = Some(r);
                    }
                    return;
                }
                let mut chosen: Vec<SourceIx> = Vec::new();
                egui::Grid::new("export-sources").num_columns(4).spacing([14.0, 4.0]).striped(true).show(ui, |ui| {
                    ui.label(RichText::new("Source").small().color(colors::INK_MUTED));
                    ui.label(RichText::new("Marked").small().color(colors::INK_MUTED));
                    ui.label(RichText::new("Last copies").small().color(colors::INK_MUTED));
                    ui.label(RichText::new("Script").small().color(colors::INK_MUTED));
                    ui.end_row();
                    for (i, s) in ix.sources().iter().enumerate() {
                        let t = st.sources[i];
                        if t.files == 0 {
                            continue;
                        }
                        let mut on = !self.left_out.contains(&s.host);
                        if ui.checkbox(&mut on, format::host(&s.host)).changed() {
                            if on {
                                self.left_out.remove(&s.host);
                            } else {
                                self.left_out.insert(s.host.clone());
                            }
                        }
                        if on {
                            chosen.push(i as SourceIx);
                        }
                        ui.label(RichText::new(files_bytes(t.files, t.bytes)).color(colors::INK_SECONDARY));
                        if t.last_files > 0 {
                            ui.label(
                                RichText::new(format!("⚠ {}", format::count(t.last_files))).color(colors::CRITICAL),
                            );
                        } else {
                            ui.label(RichText::new("none").color(colors::INK_MUTED));
                        }
                        ui.label(RichText::new(script_kind(s.kind)).color(colors::INK_SECONDARY));
                        ui.end_row();
                    }
                });
                let last: (u64, u64) = chosen
                    .iter()
                    .map(|&s| (st.sources[s as usize].last_files, st.sources[s as usize].last_bytes))
                    .fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
                if last.0 > 0 {
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(format!(
                            "⚠ {} would lose their last copy (every copy is marked). They are left out of the scripts.",
                            files_bytes(last.0, last.1)
                        ))
                        .color(colors::CRITICAL),
                    );
                    ui.checkbox(
                        &mut self.options.include_last_copies,
                        "Include them anyway: they would be gone for good",
                    );
                }
                ui.add_space(8.0);
                let kinds: BTreeSet<u8> = chosen.iter().map(|&s| ix.sources()[s as usize].kind.code() as u8).collect();
                egui::Grid::new("export-options").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                    if kinds.contains(&(PathKind::Windows.code() as u8)) {
                        ui.label("Windows drives");
                        ui.horizontal_wrapped(|ui| {
                            ui.radio_value(&mut self.options.windows, WindowsMode::RecycleBin, "to the Recycle Bin");
                            ui.radio_value(
                                &mut self.options.windows,
                                WindowsMode::Move,
                                "into a \"_DupeMap deleted\" folder",
                            )
                            .on_hover_text("At the top of the drive; delete that folder yourself once you are sure");
                            ui.radio_value(&mut self.options.windows, WindowsMode::Delete, "delete for good");
                        });
                        ui.end_row();
                    }
                    if kinds.contains(&(PathKind::Posix.code() as u8)) {
                        ui.label("Linux");
                        ui.horizontal_wrapped(|ui| {
                            ui.radio_value(
                                &mut self.options.linux,
                                LinuxMode::Move,
                                "into a \"_dupemap_deleted\" folder",
                            )
                            .on_hover_text(
                                "At the top of each file system (ZFS: dataset); delete it yourself once you are sure",
                            );
                            ui.radio_value(&mut self.options.linux, LinuxMode::Delete, "delete for good (rm)");
                        });
                        ui.end_row();
                    }
                    if kinds.contains(&(PathKind::Cloud.code() as u8)) {
                        ui.label("Cloud drives");
                        ui.checkbox(&mut self.options.cloud_permanent, "delete for good instead of into the trash");
                        ui.end_row();
                    }
                    ui.label("Checks");
                    ui.checkbox(&mut self.options.check_hash, "also check each file's SHA-256 before removing it")
                        .on_hover_text(
                        "Reads every file, so it is slow, but it also catches a file that changed and kept its size",
                    );
                    ui.end_row();
                    ui.label("Lists");
                    ui.checkbox(&mut self.options.lists, "also write a plain list of the paths per source");
                    ui.end_row();
                    ui.label("Folder");
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut self.dir).desired_width(420.0));
                        #[cfg(windows)]
                        if ui.button("Choose…").clicked() {
                            let mut d = rfd::FileDialog::new().set_title("Folder for the delete scripts");
                            if let Some(f) = frame {
                                d = d.set_parent(f);
                            }
                            if let Some(p) = d.pick_folder() {
                                self.dir = p.display().to_string();
                            }
                        }
                    });
                    ui.end_row();
                });
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let ok = !chosen.is_empty() && !self.dir.trim().is_empty() && !self.running;
                    if ui.add_enabled(ok, egui::Button::new(RichText::new("Write the scripts").strong())).clicked() {
                        self.running = true;
                        actions.push(Action::Export(ExportChoice {
                            sources: chosen.clone(),
                            options: self.options.clone(),
                            dir: PathBuf::from(self.dir.trim()),
                        }));
                    }
                    if self.running {
                        ui.add(egui::Spinner::new());
                    }
                });
            });
        self.open = open;
    }

    /// The outcome of an export; `true` when the user wants to write again.
    fn result_ui(&mut self, ui: &mut Ui, r: &Result<ExportDone, String>, actions: &mut Vec<Action>) -> bool {
        let mut again = false;
        match r {
            Err(e) => {
                ui.label(RichText::new(format!("Could not write the scripts: {e}")).color(colors::CRITICAL));
                if ui.button("Back").clicked() {
                    again = true;
                }
            }
            Ok(done) => {
                ui.label(
                    RichText::new(format!(
                        "✔ Wrote {} in {}",
                        format::plural(done.written.len() as u64, "file", "files"),
                        done.dir.display()
                    ))
                    .color(colors::GOOD),
                );
                egui::Grid::new("export-written").num_columns(3).spacing([12.0, 3.0]).striped(true).show(ui, |ui| {
                    for w in &done.written {
                        ui.label(
                            RichText::new(
                                w.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                            )
                            .color(colors::INK),
                        );
                        ui.label(RichText::new(w.what.label()).color(colors::INK_SECONDARY));
                        ui.label(RichText::new(files_bytes(w.files, w.bytes)).color(colors::INK_SECONDARY));
                        ui.end_row();
                    }
                });
                if done.left_out.files > 0 {
                    ui.label(
                        RichText::new(format!(
                            "Left out: {} whose every copy is marked.",
                            files_bytes(done.left_out.files, done.left_out.bytes)
                        ))
                        .color(colors::WARNING),
                    );
                }
                if done.unscriptable > 0 {
                    ui.label(
                        RichText::new(format!(
                            "{} cannot be named safely in a script (a line break in the name): delete those by hand.",
                            format::plural(done.unscriptable as u64, "file", "files")
                        ))
                        .color(colors::WARNING),
                    );
                }
                ui.add_space(6.0);
                ui.label(RichText::new("Next").strong());
                ui.label(
                    RichText::new(
                        "Copy each script to the computer that has those files and run it there. Windows: right-click \
                         it → Run with PowerShell (add -DryRun to only look). Linux: sudo bash <script> --dry-run \
                         first, then without. Cloud: needs rclone with the same remote. Every script writes a log \
                         next to itself.",
                    )
                    .color(colors::INK_SECONDARY),
                );
                ui.horizontal(|ui| {
                    #[cfg(windows)]
                    if let Some(first) = done.written.first() {
                        if ui.button("Open the folder").clicked() {
                            actions.push(Action::Reveal(first.path.display().to_string()));
                        }
                    }
                    if ui.button("Write again").clicked() {
                        again = true;
                    }
                });
            }
        }
        let _ = actions;
        again
    }
}

/// Folder for a new export: `delete-scripts\<date time>` beside the database.
pub fn default_export_dir(db_path: &std::path::Path) -> PathBuf {
    let stamp = dupemap_core::export::utc_text(std::time::SystemTime::now()).trim_end_matches(" UTC").replace(':', "");
    db_path.parent().map(|p| p.join("delete-scripts")).unwrap_or_else(|| PathBuf::from("delete-scripts")).join(stamp)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(path: &str, action: MarkAction) -> MarkRule {
        MarkRule { host: "nas".into(), path: path.into(), is_dir: true, action }
    }

    #[test]
    fn undo_and_redo_walk_the_changes() {
        let mut b = MarkBook::default();
        b.load(vec![r("/a", MarkAction::Delete)]);
        assert!(b.apply("mark b".into(), &[r("/b", MarkAction::Delete)], &[]));
        assert!(b.apply("keep a".into(), &[r("/a", MarkAction::Keep)], &[]));
        assert!(!b.apply("again".into(), &[r("/a", MarkAction::Keep)], &[]), "no change");
        assert_eq!(b.rules().len(), 2);
        assert_eq!(b.get(&r("/a", MarkAction::Delete).place_key()).unwrap().action, MarkAction::Keep);
        assert_eq!(b.undo().as_deref(), Some("keep a"));
        assert_eq!(b.get(&r("/a", MarkAction::Delete).place_key()).unwrap().action, MarkAction::Delete);
        assert_eq!(b.undo().as_deref(), Some("mark b"));
        assert!(b.get(&r("/b", MarkAction::Delete).place_key()).is_none());
        assert_eq!(b.undo(), None, "loading is not undone");
        assert_eq!(b.redo().as_deref(), Some("mark b"));
        assert_eq!(b.redo().as_deref(), Some("keep a"));
        assert_eq!(b.get(&r("/a", MarkAction::Delete).place_key()).unwrap().action, MarkAction::Keep);
        assert_eq!(b.redo(), None);
        // a new change clears what could be redone
        b.undo();
        b.apply("remove b".into(), &[], &[r("/b", MarkAction::Delete).place_key()]);
        assert_eq!(b.redo(), None);
        assert_eq!(b.rules().len(), 1);
    }
}
