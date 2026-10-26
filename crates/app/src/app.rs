//! The application: owns the index, the background threads, the dock of tabs and the windows.

use crate::actions::{self, Action};
use crate::colors;
use crate::files::FileCache;
use crate::format;
use crate::ignore_ui::IgnoreEditor;
use crate::manifests_ui::{ManifestsWindow, ReportWindow};
use crate::pane::{PaneCtx, TreePane};
use crate::theme;
use crate::tree::Root;
use crate::widgets;
use crate::worker::{self, Job, JobOutcome, Query, Repaint, Reply, RunningJob, Worker};
use dupemap_core::db::ManifestInfo;
use dupemap_core::ignore::IgnoreSettings;
use dupemap_core::index::Index;
use dupemap_core::Severity;
use egui::text::{LayoutJob, TextFormat};
use egui::{FontId, RichText, Ui};
use egui_dock::{DockArea, DockState, NodeIndex, NodePath, SurfaceIndex, TabViewer};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

/// Command-line options.
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub db_path: PathBuf,
    /// Generate and import synthetic manifests of about this many rows.
    pub demo_rows: Option<u64>,
    /// Save screenshots here once loaded, then exit (CI).
    pub screenshot: Option<PathBuf>,
}

/// Progress of `--screenshot`.
struct Shot {
    path: PathBuf,
    stage: u8,
    prepared: bool,
    idle_frames: u32,
    requested: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Tab {
    pub id: u64,
    pub kind: TabKind,
}

#[derive(Clone, Serialize, Deserialize)]
pub enum TabKind {
    Tree(TreePane),
}

struct Note {
    text: String,
    error: bool,
    at: f64,
}

pub struct DupeMapApp {
    db_path: PathBuf,
    repaint: Repaint,
    index: Option<Arc<Index>>,
    generation: u64,
    manifests: Vec<ManifestInfo>,
    alert_hosts: BTreeSet<String>,
    worker: Worker,
    files: FileCache,
    job: Option<RunningJob>,
    queue: VecDeque<Job>,
    /// Ignore settings of the newest queued or running SetIgnore job (later edits build on it).
    ignore_pending: Option<IgnoreSettings>,
    dock: DockState<Tab>,
    next_tab_id: u64,
    focused: Option<u64>,
    manifests_win: ManifestsWindow,
    reports: ReportWindow,
    ignore: Option<IgnoreEditor>,
    about: bool,
    log_open: bool,
    notes: Vec<Note>,
    shot: Option<Shot>,
}

const STATUS_SECS: f64 = 12.0;

impl DupeMapApp {
    pub fn new(cc: &eframe::CreationContext<'_>, opts: Options) -> Self {
        let ctx = cc.egui_ctx.clone();
        theme::apply(&ctx);
        crate::fonts::install_in_background(&ctx);
        let c2 = ctx.clone();
        let repaint: Repaint = Arc::new(move || c2.request_repaint());
        let worker = Worker::spawn(repaint.clone());
        let mut queue = VecDeque::new();
        match opts.demo_rows {
            Some(rows) => {
                let folder =
                    opts.db_path.parent().map(|p| p.join("demo-manifests")).unwrap_or_else(|| "demo-manifests".into());
                queue.push_back(Job::Demo { rows, folder });
            }
            None => queue.push_back(Job::Open),
        }
        let first = Tab { id: 1, kind: TabKind::Tree(TreePane::new(Root::AllSources)) };
        DupeMapApp {
            db_path: opts.db_path,
            repaint,
            index: None,
            generation: 0,
            manifests: Vec::new(),
            alert_hosts: BTreeSet::new(),
            worker,
            files: FileCache::default(),
            job: None,
            queue,
            ignore_pending: None,
            dock: DockState::new(vec![first]),
            next_tab_id: 2,
            focused: Some(1),
            manifests_win: ManifestsWindow::default(),
            reports: ReportWindow::default(),
            ignore: None,
            about: false,
            log_open: false,
            notes: Vec::new(),
            shot: opts.screenshot.map(|path| {
                // captures must not depend on where the mouse pointer happens to be
                ctx.all_styles_mut(|s| s.interaction.tooltip_delay = 1.0e9);
                Shot { path, stage: 0, prepared: false, idle_frames: 0, requested: false }
            }),
        }
    }

    fn busy(&self) -> bool {
        self.job.is_some() || !self.queue.is_empty()
    }

    fn note(&mut self, ctx: &egui::Context, text: impl Into<String>, error: bool) {
        let at = ctx.input(|i| i.time);
        self.notes.push(Note { text: text.into(), error, at });
        if self.notes.len() > 300 {
            self.notes.remove(0);
        }
    }

    fn queue_job(&mut self, job: Job) {
        if let Job::SetIgnore(s) = &job {
            // only the newest ignore settings matter
            self.queue.retain(|j| !matches!(j, Job::SetIgnore(_)));
            self.ignore_pending = Some(s.clone());
        }
        self.queue.push_back(job);
    }

    /// Ignore settings that new edits should start from.
    fn current_ignore(&self) -> IgnoreSettings {
        self.ignore_pending
            .clone()
            .or_else(|| self.index.as_ref().map(|ix| ix.ignore_settings().clone()))
            .unwrap_or_default()
    }

    fn new_tab_id(&mut self) -> u64 {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        id
    }

    fn make_tab(&mut self, root: Root) -> Tab {
        let mut pane = TreePane::new(Root::AllSources);
        if let Some(ix) = &self.index {
            pane.set_root(ix, root);
        } else {
            pane.root = root;
        }
        Tab { id: self.new_tab_id(), kind: TabKind::Tree(pane) }
    }

    fn add_tab(&mut self, root: Root) {
        let tab = self.make_tab(root);
        self.focused = Some(tab.id);
        self.dock.push_to_focused_leaf(tab);
    }

    fn split(&mut self, right: bool) {
        let root = self
            .dock
            .iter_all_tabs()
            .find(|(_, t)| Some(t.id) == self.focused)
            .map(|(_, t)| match &t.kind {
                TabKind::Tree(p) => p.root,
            })
            .unwrap_or(Root::AllSources);
        let (surface, node) = match self.focused.and_then(|id| self.dock.find_tab_from(|t| t.id == id)) {
            Some(path) => (path.surface, path.node),
            None => (SurfaceIndex::main(), NodeIndex::root()),
        };
        let tab = self.make_tab(root);
        self.focused = Some(tab.id);
        let tree = &mut self.dock[surface];
        if right {
            tree.split_right(node, 0.5, vec![tab]);
        } else {
            tree.split_below(node, 0.5, vec![tab]);
        }
    }

    // ------------------------------------------------------------------ --screenshot

    fn screenshot_step(&mut self, ctx: &egui::Context) {
        /// Captures in order: the main view, then one window at a time.
        const SHOTS: [&str; 4] = ["", "ignore", "report", "manifests"];
        ctx.request_repaint();
        let image = ctx.input(|i| {
            i.raw.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        let Some(shot) = &mut self.shot else { return };
        if let Some(img) = image {
            let name = SHOTS[(shot.stage as usize).min(SHOTS.len() - 1)];
            let path = if name.is_empty() {
                shot.path.clone()
            } else {
                let stem = shot.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                shot.path.with_file_name(format!("{stem}-{name}.png"))
            };
            let rgba: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_srgba_unmultiplied()).collect();
            if let Err(e) = crate::shot::write_png(&path, img.size[0] as u32, img.size[1] as u32, &rgba) {
                eprintln!("screenshot: {e}");
            }
            shot.stage += 1;
            shot.requested = false;
            shot.idle_frames = 0;
            shot.prepared = false;
            if shot.stage as usize >= SHOTS.len() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            return;
        }
        let idle = self.index.is_some() && self.job.is_none() && self.queue.is_empty() && !self.files.is_pending();
        if !idle || shot.requested {
            return;
        }
        shot.idle_frames += 1;
        if !shot.prepared && shot.idle_frames >= 3 {
            let stage = shot.stage;
            shot.prepared = true;
            shot.idle_frames = 0;
            self.ignore = None;
            self.manifests_win.open = false;
            self.reports.open = false;
            match stage {
                0 => self.screenshot_layout(),
                1 => self.ignore = Some(IgnoreEditor::new(&self.current_ignore())),
                2 => self.reports.open = true,
                _ => self.manifests_win.open = true,
            }
        } else if shot.prepared && shot.idle_frames >= 12 {
            shot.requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
        }
    }

    /// A representative layout: every source on the left, the biggest folder of the second
    /// source on the right.
    fn screenshot_layout(&mut self) {
        let Some(ix) = self.index.clone() else { return };
        let mut by_size: Vec<_> = ix.source_summaries().to_vec();
        by_size.sort_by_key(|s| std::cmp::Reverse(s.bytes));
        if let Some(first) = by_size.first() {
            let root = ix.sources()[first.source as usize].root;
            for (_, tab) in self.dock.iter_all_tabs_mut() {
                let TabKind::Tree(p) = &mut tab.kind;
                if p.root == Root::AllSources {
                    p.reveal_largest(&ix, Some(root));
                }
            }
        }
        if let Some(second) = by_size.get(1) {
            let mut tab = self.make_tab(Root::Source(second.db_id));
            let TabKind::Tree(p) = &mut tab.kind;
            p.reveal_largest(&ix, None);
            self.dock.main_surface_mut().split_right(NodeIndex::root(), 0.52, vec![tab]);
        }
    }

    // ------------------------------------------------------------------ background results

    fn poll(&mut self, ctx: &egui::Context) {
        if let Some(out) = self.job.as_ref().and_then(|j| j.poll()) {
            self.job = None;
            self.finish_job(ctx, out);
        }
        if self.job.is_none() {
            if let Some(next) = self.queue.pop_front() {
                self.job = Some(worker::start(self.db_path.clone(), next, self.repaint.clone()));
            }
        }
        for r in self.worker.poll() {
            match r {
                Reply::Files { generation, dir, opts, files } => self.files.insert(generation, dir, opts, files),
                Reply::Report(Ok(rep)) => self.reports.show_reports(vec![*rep]),
                Reply::Report(Err(e)) => self.note(ctx, format!("Could not read the report: {e}"), true),
                Reply::Manifests { manifests, alert_hosts } => {
                    self.manifests = manifests;
                    self.alert_hosts = alert_hosts;
                }
            }
        }
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect()
        });
        if !dropped.is_empty() {
            self.queue_job(Job::Import(dropped));
        }
    }

    fn finish_job(&mut self, ctx: &egui::Context, out: JobOutcome) {
        if let Some(ix) = out.index {
            self.generation += 1;
            self.worker.send(Query::SetIndex {
                generation: self.generation,
                index: ix.clone(),
                db_path: self.db_path.clone(),
            });
            self.files.reset(self.generation);
            if let Some(ed) = &mut self.ignore {
                ed.rebase(ix.ignore_settings());
            }
            self.index = Some(ix);
        }
        if !self.queue.iter().any(|j| matches!(j, Job::SetIgnore(_))) {
            self.ignore_pending = None;
        }
        self.manifests = out.manifests;
        self.alert_hosts = out.alert_hosts;
        for n in out.notes {
            self.note(ctx, n, false);
        }
        if out.cancelled {
            self.note(ctx, "Cancelled. Manifests imported before the cancel are kept.", false);
        }
        if let Some(e) = out.error {
            self.note(ctx, e, true);
        }
        if !out.reports.is_empty() {
            let rows: u64 = out.reports.iter().map(|r| r.rows_imported).sum();
            let alerts = out.reports.iter().filter(|r| r.severity() == Severity::Alert).count();
            let mut msg = format!(
                "Imported {}: {} rows",
                format::plural(out.reports.len() as u64, "manifest", "manifests"),
                format::count(rows)
            );
            if alerts > 0 {
                msg.push_str(&format!(" · {} with alerts", alerts));
            }
            self.note(ctx, msg, false);
            self.reports.show_reports(out.reports);
        }
    }

    // ------------------------------------------------------------------ actions

    fn process(&mut self, ctx: &egui::Context, frame: &eframe::Frame, actions: Vec<Action>) {
        for a in actions {
            match a {
                Action::CopyText(t) => {
                    ctx.copy_text(t.clone());
                    let shown: String = t.chars().take(80).collect();
                    self.note(ctx, format!("Copied {shown}{}", if t.chars().count() > 80 { "…" } else { "" }), false);
                }
                Action::AddIgnoreRule(rule) => {
                    let mut s = self.current_ignore();
                    let turned_on = !s.enabled;
                    s.enabled = true;
                    let exists =
                        s.rules.iter().any(|r| r.pattern.eq_ignore_ascii_case(&rule.pattern) && r.host == rule.host);
                    if exists {
                        if let Some(r) = s
                            .rules
                            .iter_mut()
                            .find(|r| r.pattern.eq_ignore_ascii_case(&rule.pattern) && r.host == rule.host)
                        {
                            r.enabled = true;
                        }
                    } else {
                        s.rules.push(rule.clone());
                    }
                    let scope = rule.host.as_deref().map(format::host).unwrap_or("every source").to_string();
                    self.note(
                        ctx,
                        format!(
                            "Ignoring {} in {scope}{}. Undo in File → Ignored folders.",
                            rule.pattern,
                            if turned_on { " (ignoring was switched off; it is on again)" } else { "" }
                        ),
                        false,
                    );
                    self.queue_job(Job::SetIgnore(s));
                }
                Action::SetIgnore(s) => self.queue_job(Job::SetIgnore(s)),
                Action::OpenIgnoreEditor => {
                    if self.ignore.is_none() {
                        self.ignore = Some(IgnoreEditor::new(&self.current_ignore()));
                    }
                }
                Action::OpenManifests => self.manifests_win.open = true,
                Action::Import => self.import_dialog(frame),
                Action::ImportPaths(p) => self.queue_job(Job::Import(p)),
                Action::RemoveManifests(ids) => self.queue_job(Job::Remove(ids)),
                Action::ShowReport(id) => self.worker.send(Query::Report { manifest_id: id }),
                Action::SetAlertDismissed(id, d) => {
                    self.worker.send(Query::SetAlertDismissed { manifest_id: id, dismissed: d })
                }
                Action::NewTab(root) => self.add_tab(root),
                Action::Focus(id) => self.focused = Some(id),
                Action::Note(n) => self.note(ctx, n, false),
                Action::Reveal(p) => {
                    if let Err(e) = actions::reveal(&p) {
                        self.note(ctx, format!("Could not open Explorer: {e}"), true);
                    }
                }
            }
        }
    }

    #[allow(unused_variables)]
    fn import_dialog(&mut self, frame: &eframe::Frame) {
        #[cfg(windows)]
        {
            let picked = rfd::FileDialog::new()
                .set_title("Import hash manifests")
                .add_filter("Hash manifests (*.tsv)", &["tsv"])
                .add_filter("All files", &["*"])
                .set_parent(frame)
                .pick_files();
            if let Some(files) = picked {
                if !files.is_empty() {
                    self.queue_job(Job::Import(files));
                }
            }
        }
        #[cfg(not(windows))]
        {
            // no native dialog here: the Manifests window takes a typed path
            self.manifests_win.open = true;
        }
    }

    // ------------------------------------------------------------------ drawing

    fn menu_bar(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.add(egui::Button::new("Import manifests…").shortcut_text("Ctrl+O")).clicked() {
                    actions.push(Action::Import);
                    ui.close();
                }
                if ui.button("Manifests…").clicked() {
                    actions.push(Action::OpenManifests);
                    ui.close();
                }
                ui.separator();
                if ui.button("Ignored folders…").clicked() {
                    actions.push(Action::OpenIgnoreEditor);
                    ui.close();
                }
                ui.separator();
                if ui.add(egui::Button::new("Quit").shortcut_text("Ctrl+Q")).clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    ui.close();
                }
            });
            ui.menu_button("View", |ui| {
                if ui.add(egui::Button::new("New tab").shortcut_text("Ctrl+T")).clicked() {
                    actions.push(Action::NewTab(Root::AllSources));
                    ui.close();
                }
                if ui.button("Split right").clicked() {
                    self.split(true);
                    ui.close();
                }
                if ui.button("Split below").clicked() {
                    self.split(false);
                    ui.close();
                }
                ui.separator();
                if ui.button("Messages…").clicked() {
                    self.log_open = true;
                    ui.close();
                }
            });
            ui.menu_button("Help", |ui| {
                if ui.button("About DupeMap").clicked() {
                    self.about = true;
                    ui.close();
                }
            });
        });
    }

    fn status_bar(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        ui.horizontal_centered(|ui| {
            if let Some(ix) = &self.index {
                let s = ix.source_summaries();
                let files: u64 = s.iter().map(|x| x.files).sum();
                let bytes: u64 = s.iter().map(|x| x.bytes).sum();
                ui.label(
                    RichText::new(format!(
                        "{} · {} · {}",
                        format::plural(s.len() as u64, "source", "sources"),
                        format::plural(files, "file", "files"),
                        format::bytes(bytes)
                    ))
                    .color(colors::INK_SECONDARY),
                );
                let ig = ix.ignored_total();
                if ix.ignore_settings().enabled && ig.folders > 0 {
                    ui.label(RichText::new("·").color(colors::INK_MUTED));
                    let r = widgets::link(ui, format!("{} ignored", format::plural(ig.folders, "folder", "folders")));
                    if r.on_hover_text(format!(
                        "{} ({}) left out by ignore rules. Click to edit.",
                        format::plural(ig.files, "file", "files"),
                        format::bytes(ig.bytes)
                    ))
                    .clicked()
                    {
                        actions.push(Action::OpenIgnoreEditor);
                    }
                } else if !ix.ignore_settings().enabled {
                    ui.label(RichText::new("·").color(colors::INK_MUTED));
                    if widgets::link(ui, "ignoring is off").clicked() {
                        actions.push(Action::OpenIgnoreEditor);
                    }
                }
                ui.label(
                    RichText::new("· only files ≥ the scripts' minimum size (1 MB) are hashed")
                        .color(colors::INK_MUTED),
                )
                .on_hover_text("The export scripts skip small files, so they never appear here.");
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let alerts =
                    self.manifests.iter().filter(|m| m.severity == Severity::Alert && !m.alert_dismissed).count();
                if alerts > 0 {
                    let text = RichText::new(format!(
                        "⚠ {} need{} attention",
                        format::plural(alerts as u64, "manifest", "manifests"),
                        if alerts == 1 { "s" } else { "" }
                    ))
                    .color(colors::INK);
                    let b = egui::Button::new(text)
                        .fill(colors::alpha(colors::CRITICAL, 0.35))
                        .stroke(egui::Stroke::new(1.0, colors::CRITICAL));
                    if ui.add(b).on_hover_text("Open the manifest list to read the reports").clicked() {
                        actions.push(Action::OpenManifests);
                    }
                }
                if let Some(job) = &self.job {
                    let p = job.progress();
                    if ui.add_enabled(!job.is_cancelling(), egui::Button::new("Cancel").small()).clicked() {
                        job.cancel();
                    }
                    if p.fraction >= 0.0 {
                        ui.add(egui::ProgressBar::new(p.fraction).desired_width(160.0).show_percentage());
                    } else {
                        ui.add(egui::Spinner::new().size(14.0));
                    }
                    let step = if p.step.is_empty() { p.title.clone() } else { format!("{}: {}", p.title, p.step) };
                    ui.add(egui::Label::new(RichText::new(step).color(colors::INK_SECONDARY)).truncate());
                } else if let Some(n) = self.notes.last() {
                    let now = ui.input(|i| i.time);
                    if now - n.at < STATUS_SECS {
                        let r = if n.error {
                            widgets::status_label(ui, "⚠", colors::CRITICAL, n.text.clone())
                        } else {
                            ui.add(egui::Label::new(RichText::new(&n.text).color(colors::INK_SECONDARY)).truncate())
                        };
                        if r.interact(egui::Sense::click()).clicked() {
                            self.log_open = true;
                        }
                        ui.ctx().request_repaint_after(std::time::Duration::from_secs_f64(
                            STATUS_SECS - (now - n.at) + 0.1,
                        ));
                    }
                }
            });
        });
    }

    fn welcome(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        ui.vertical_centered(|ui| {
            ui.add_space((ui.available_height() * 0.22).max(20.0));
            ui.label(RichText::new("DupeMap").size(28.0).color(colors::INK));
            ui.add_space(4.0);
            ui.label(
                RichText::new("Custom cross-machine duplicate file comparator based on hashes.")
                    .color(colors::INK_SECONDARY),
            );
            ui.add_space(18.0);
            if self.busy() {
                ui.add(egui::Spinner::new().size(20.0));
                return;
            }
            if ui
                .add(egui::Button::new(RichText::new("Import manifests…").size(15.0)).min_size(egui::vec2(200.0, 32.0)))
                .clicked()
            {
                actions.push(Action::Import);
            }
            ui.add_space(10.0);
            ui.label(
                RichText::new("or drop manifest-*.tsv files (or a folder of them) anywhere on this window")
                    .color(colors::INK_MUTED),
            );
            ui.add_space(18.0);
            ui.label(
                RichText::new(
                    "Manifests come from the export-hashes scripts: one line per file with its SHA-256, size, \
                     source and path. Import one per machine, drive or cloud account; DupeMap shows every copy of \
                     every file across all of them. It never deletes anything itself.",
                )
                .color(colors::INK_MUTED),
            );
        });
    }

    fn drop_overlay(&self, ctx: &egui::Context) {
        let hovering = ctx.input(|i| !i.raw.hovered_files.is_empty());
        if !hovering {
            return;
        }
        let rect = ctx.content_rect();
        let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("drop")));
        painter.rect_filled(rect, egui::CornerRadius::ZERO, colors::alpha(colors::PAGE, 0.8));
        painter.rect_stroke(
            rect.shrink(12.0),
            egui::CornerRadius::same(10),
            egui::Stroke::new(2.0, colors::ACCENT),
            egui::StrokeKind::Inside,
        );
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "Drop to import manifests",
            FontId::proportional(22.0),
            colors::INK,
        );
    }

    fn about_window(&mut self, ctx: &egui::Context) {
        let mut open = self.about;
        egui::Window::new("About DupeMap").open(&mut open).collapsible(false).resizable(false).default_width(460.0).show(ctx, |ui| {
            ui.label(RichText::new(format!("DupeMap {}", env!("CARGO_PKG_VERSION"))).strong());
            ui.label("Custom cross-machine duplicate file comparator based on hashes.");
            ui.add_space(6.0);
            ui.label(RichText::new("Works offline: no network access, no telemetry. DupeMap never deletes files; it only shows them and (later) writes scripts you review and run yourself.").color(colors::INK_SECONDARY));
            ui.add_space(6.0);
            ui.label(RichText::new(format!("Database: {}", self.db_path.display())).color(colors::INK_MUTED));
            if let Some(ix) = &self.index {
                ui.label(
                    RichText::new(format!(
                        "Index: {} folders · {} files · {} distinct contents · {} duplicate groups · ~{} in memory",
                        format::count(ix.dir_count() as u64),
                        format::count(ix.file_count() as u64),
                        format::count(ix.content_count() as u64),
                        format::count(ix.dup_group_count() as u64),
                        format::bytes(ix.approx_memory() as u64)
                    ))
                    .color(colors::INK_MUTED),
                );
            }
        });
        self.about = open;
    }

    fn log_window(&mut self, ctx: &egui::Context) {
        let mut open = self.log_open;
        egui::Window::new("Messages").open(&mut open).default_width(640.0).default_height(320.0).show(ctx, |ui| {
            egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
                if self.notes.is_empty() {
                    ui.label(RichText::new("No messages yet").color(colors::INK_MUTED));
                }
                for n in &self.notes {
                    if n.error {
                        widgets::status_label(ui, "⚠", colors::CRITICAL, n.text.clone());
                    } else {
                        ui.label(&n.text);
                    }
                }
            });
        });
        self.log_open = open;
    }

    fn shortcuts(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        use egui::{Key, KeyboardShortcut, Modifiers};
        let (open, tab, quit) = ctx.input_mut(|i| {
            (
                i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::O)),
                i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::T)),
                i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::Q)),
            )
        });
        if open {
            actions.push(Action::Import);
        }
        if tab {
            actions.push(Action::NewTab(Root::AllSources));
        }
        if quit {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

impl eframe::App for DupeMapApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll(ctx);
    }

    fn ui(&mut self, ui: &mut Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let mut actions = Vec::new();
        self.shortcuts(&ctx, &mut actions);

        egui::Panel::top("menu")
            .frame(egui::Frame::new().fill(colors::PAGE).inner_margin(egui::Margin::symmetric(8, 3)))
            .show(ui, |ui| self.menu_bar(ui, &mut actions));
        egui::Panel::bottom("status")
            .exact_size(28.0)
            .frame(egui::Frame::new().fill(colors::PAGE).inner_margin(egui::Margin::symmetric(10, 2)))
            .show(ui, |ui| self.status_bar(ui, &mut actions));

        egui::CentralPanel::no_frame().show(ui, |ui| {
            let ix = self.index.clone();
            match ix {
                Some(ix) if !ix.sources().is_empty() || !self.manifests.is_empty() => {
                    let mut added: Vec<NodePath> = Vec::new();
                    let style = theme::dock_style(ui.style());
                    let mut viewer = Viewer {
                        ix: &ix,
                        generation: self.generation,
                        files: &mut self.files,
                        worker: &self.worker,
                        actions: &mut actions,
                        focused: self.focused,
                        alert_hosts: &self.alert_hosts,
                        added: &mut added,
                    };
                    DockArea::new(&mut self.dock)
                        .style(style)
                        .show_add_buttons(true)
                        .show_leaf_collapse_buttons(false)
                        .show_leaf_close_all_buttons(false)
                        .show_inside(ui, &mut viewer);
                    for path in added {
                        let tab = self.make_tab(Root::AllSources);
                        self.focused = Some(tab.id);
                        self.dock.set_focused_node_and_surface(path);
                        self.dock.push_to_focused_leaf(tab);
                    }
                    if self.dock.iter_all_tabs().next().is_none() {
                        let tab = self.make_tab(Root::AllSources);
                        self.focused = Some(tab.id);
                        self.dock = DockState::new(vec![tab]);
                    }
                }
                _ => {
                    egui::Frame::new().fill(colors::PANEL).show(ui, |ui| {
                        ui.set_min_size(ui.available_size());
                        self.welcome(ui, &mut actions);
                    });
                }
            }
        });

        // windows
        let busy = self.busy();
        self.manifests_win.show(&ctx, &self.manifests, &self.db_path, busy, &mut actions);
        self.reports.show(&ctx, &self.manifests, &mut actions);
        if let Some(ed) = &mut self.ignore {
            if !ed.show(&ctx, self.index.as_deref(), busy, &mut actions) {
                self.ignore = None;
            }
        }
        if self.about {
            self.about_window(&ctx);
        }
        if self.log_open {
            self.log_window(&ctx);
        }
        self.drop_overlay(&ctx);
        self.process(&ctx, frame, actions);
        if self.shot.is_some() {
            self.screenshot_step(&ctx);
        }
    }
}

struct Viewer<'a> {
    ix: &'a Arc<Index>,
    generation: u64,
    files: &'a mut FileCache,
    worker: &'a Worker,
    actions: &'a mut Vec<Action>,
    focused: Option<u64>,
    alert_hosts: &'a BTreeSet<String>,
    added: &'a mut Vec<NodePath>,
}

impl TabViewer for Viewer<'_> {
    type Tab = Tab;

    fn id(&mut self, tab: &mut Tab) -> egui::Id {
        egui::Id::new(("dupemap-tab", tab.id))
    }

    fn title(&mut self, tab: &mut Tab) -> egui::WidgetText {
        match &tab.kind {
            TabKind::Tree(p) => {
                let title = p.title(Some(self.ix));
                let alert = match p.root {
                    Root::AllSources => !self.alert_hosts.is_empty(),
                    Root::Source(_) => p.host(self.ix).is_some_and(|h| self.alert_hosts.contains(h)),
                };
                if alert {
                    // red badge with an icon, so it does not rely on colour alone
                    let mut job = LayoutJob::default();
                    job.append("⚠ ", 0.0, TextFormat::simple(FontId::proportional(13.0), colors::CRITICAL));
                    job.append(&title, 0.0, TextFormat::simple(FontId::proportional(13.5), colors::INK));
                    job.into()
                } else {
                    title.into()
                }
            }
        }
    }

    fn ui(&mut self, ui: &mut Ui, tab: &mut Tab) {
        match &mut tab.kind {
            TabKind::Tree(p) => {
                let mut cx = PaneCtx {
                    ix: self.ix,
                    generation: self.generation,
                    files: self.files,
                    worker: self.worker,
                    actions: self.actions,
                    pane_id: tab.id,
                    focused: self.focused == Some(tab.id),
                    alert_hosts: self.alert_hosts,
                };
                p.ui(ui, &mut cx);
            }
        }
    }

    fn on_tab_button(&mut self, tab: &mut Tab, response: &egui::Response) {
        if response.clicked() || response.drag_started() {
            self.actions.push(Action::Focus(tab.id));
        }
    }

    fn on_add(&mut self, path: NodePath) {
        self.added.push(path);
    }

    fn scroll_bars(&self, _tab: &Tab) -> [bool; 2] {
        [false, false]
    }
}
