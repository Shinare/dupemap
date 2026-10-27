//! The application: owns the index, the background threads, the dock of tabs and the windows.

use crate::actions::{self, Action};
use crate::categories_ui::CategoryEditor;
use crate::checksum_ui::ChecksumDialog;
use crate::colors;
use crate::files::FileCache;
use crate::format;
use crate::groups::{GroupCache, GroupsCtx, GroupsPane};
use crate::ignore_ui::IgnoreEditor;
use crate::inspector::Inspector;
use crate::manifests_ui::{ManifestsWindow, ReportWindow};
use crate::marks_ui::{ExportWindow, MarkBook, MarksCtx, MarksPane};
use crate::pane::{PaneCtx, Sel, TreePane};
use crate::scan_ui::{RemoteScanWindow, ScanCtx, ScanWindow};
use crate::search_ui::{SearchCtx, SearchPane};
use crate::selection::{Linked, Target};
use crate::session::{Saver, Session, SessionOut, WindowState, VERSION};
use crate::summary_ui::{SummaryCache, SummaryCtx, SummaryPane};
use crate::theme;
use crate::tree::Root;
use crate::widgets;
use crate::worker::{
    self, ImportItem, Inspection, Job, JobOutcome, Query, Repaint, Reply, RunningJob, ScanDone, Worker,
};
use dupemap_core::db::ManifestInfo;
use dupemap_core::ignore::IgnoreSettings;
use dupemap_core::index::{DirIx, FileIx, Index};
use dupemap_core::sessionfile::SessionFile;
use dupemap_core::summary::Categories;
use dupemap_core::Severity;
use egui::text::{LayoutJob, TextFormat};
use egui::{FontId, RichText, Ui};
use egui_dock::{DockArea, DockState, NodeIndex, NodePath, SurfaceIndex, TabViewer};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

/// Command-line options.
#[derive(Clone, Default)]
pub struct Options {
    pub db_path: PathBuf,
    /// The session read from the session file at startup (see `session.rs`).
    pub session: Option<Session>,
    /// Where to autosave the session; `None` switches saving off (screenshots).
    pub session_file: Option<SessionFile>,
    /// Open "Scan this PC" at start (after restarting as administrator for it).
    pub open_scan: bool,
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
    /// The groups stage selects its first group once the list is there.
    picked: bool,
    idle_frames: u32,
    requested: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Tab {
    pub id: u64,
    pub kind: TabKind,
}

// a handful of tabs: boxing the tree pane would save nothing worth the indirection
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Serialize, Deserialize)]
pub enum TabKind {
    Marks(MarksPane),
    Tree(TreePane),
    Groups(GroupsPane),
    Summary(SummaryPane),
    Search(SearchPane),
}

impl TabKind {
    /// The tab was read from a saved session.
    pub fn after_load(&mut self) {
        match self {
            TabKind::Tree(p) => p.after_load(),
            TabKind::Groups(p) => p.after_load(),
            TabKind::Search(p) => p.after_load(),
            TabKind::Marks(_) => {}
            TabKind::Summary(_) => {}
        }
    }
}

struct Note {
    text: String,
    error: bool,
    at: f64,
}

pub struct DupeMapApp {
    ctx: egui::Context,
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
    inspector: Inspector,
    /// Copies of the inspected file (or where a folder's copies are), highlighted in every tab.
    linked: Option<Linked>,
    groups: GroupCache,
    /// File-type categories as stored in the database.
    categories: Arc<Categories>,
    /// Bumped whenever the categories change (summaries computed for older ones are dropped).
    cats_version: u64,
    summaries: SummaryCache,
    category_editor: Option<CategoryEditor>,
    /// Marks for deletion: the rules, and what they mark in the current index.
    marks: MarkBook,
    mark_state: Option<Arc<dupemap_core::marks::MarkState>>,
    /// The (generation, version) `mark_state` was worked out for.
    mark_state_for: (u64, u64),
    export: ExportWindow,
    /// Autosave of the session (none when saving is off).
    saver: Option<Saver>,
    session_file: Option<SessionFile>,
    /// The text last handed to the saver.
    saved_text: String,
    last_save_check: f64,
    save_failed_noted: bool,
    window: Option<WindowState>,
    inspector_width: Option<f32>,
    /// The inspected item of the saved session, selected once the index is there.
    restore_inspected: Option<Sel>,
    /// Checksum lists waiting for their source (the first one's window is shown).
    checksum_dialogs: VecDeque<ChecksumDialog>,
    scan_win: ScanWindow,
    remote_win: RemoteScanWindow,
    /// Results of the last scan, per drive.
    last_scan: Vec<ScanDone>,
    /// Runs as administrator (Windows).
    elevated: bool,
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
        let (dock, next_tab_id, focused, inspector) = match opts.session {
            Some(s) => {
                let next = s.next_tab_id();
                let focused = s.focused.or_else(|| s.dock.iter_all_tabs().next().map(|(_, t)| t.id));
                let mut inspector = Inspector::default();
                inspector.open = s.inspector_open;
                (s.dock, next, focused, (inspector, s.inspector_width, s.inspected, s.window))
            }
            None => (DockState::new(vec![first]), 2, Some(1), (Inspector::default(), None, None, None)),
        };
        let (inspector, inspector_width, restore_inspected, window) = inspector;
        let session_file = opts.session_file.clone();
        let saver = opts.session_file.map(Saver::spawn);
        let mut app = DupeMapApp {
            ctx: ctx.clone(),
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
            dock,
            next_tab_id,
            focused,
            manifests_win: ManifestsWindow::default(),
            reports: ReportWindow::default(),
            ignore: None,
            about: false,
            log_open: false,
            notes: Vec::new(),
            shot: opts.screenshot.map(|path| {
                // captures must not depend on where the mouse pointer happens to be
                ctx.all_styles_mut(|s| s.interaction.tooltip_delay = 1.0e9);
                Shot { path, stage: 0, prepared: false, picked: false, idle_frames: 0, requested: false }
            }),
            inspector,
            linked: None,
            groups: GroupCache::default(),
            categories: Arc::new(Categories::default()),
            cats_version: 0,
            summaries: SummaryCache::default(),
            category_editor: None,
            marks: MarkBook::default(),
            mark_state: None,
            mark_state_for: (0, 0),
            export: ExportWindow::default(),
            saver,
            session_file,
            saved_text: String::new(),
            last_save_check: 0.0,
            save_failed_noted: false,
            window,
            inspector_width,
            restore_inspected,
            checksum_dialogs: VecDeque::new(),
            scan_win: ScanWindow::default(),
            remote_win: RemoteScanWindow::default(),
            last_scan: Vec::new(),
            elevated: crate::elevate::is_elevated(),
        };
        if opts.open_scan {
            app.scan_win.open(&ctx);
        }
        app
    }

    fn busy(&self) -> bool {
        self.job.is_some() || !self.queue.is_empty()
    }

    fn note(&mut self, text: impl Into<String>, error: bool) {
        let at = self.ctx.input(|i| i.time);
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
                TabKind::Summary(p) => p.root,
                TabKind::Groups(_) | TabKind::Search(_) | TabKind::Marks(_) => Root::AllSources,
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
        /// Captures in order: the main view, the duplicate groups, the summary, then one window at
        /// a time.
        const SHOTS: [&str; 11] =
            ["", "groups", "summary", "search", "marks", "export", "scan", "remote", "ignore", "report", "manifests"];
        ctx.request_repaint();
        let image = ctx.input(|i| {
            i.raw.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        let idle = self.index.is_some()
            && self.job.is_none()
            && self.queue.is_empty()
            && !self.files.is_pending()
            && !self.groups.is_pending()
            && !self.summaries.is_pending()
            && !self.inspector.is_pending()
            && !self.scan_win.is_listing()
            && !(self.marks.loaded && self.mark_state_for != (self.generation, self.marks.version))
            && !self.dock.iter_all_tabs().any(|(_, t)| matches!(&t.kind, TabKind::Search(p) if p.is_pending()));
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
            shot.picked = false;
            if shot.stage as usize >= SHOTS.len() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            return;
        }
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
            self.scan_win.open = false;
            self.remote_win.open = false;
            self.export.open = false;
            match stage {
                0 => self.screenshot_layout(),
                1 => self.process_simple(vec![Action::OpenGroups]),
                2 => {
                    // the summary is a wide table: give it the room the inspector took
                    self.inspector.open = false;
                    self.process_simple(vec![Action::OpenSummary(Root::AllSources)]);
                }
                3 => {
                    self.inspector.open = true;
                    self.process_simple(vec![Action::OpenSearch]);
                    for (_, tab) in self.dock.iter_all_tabs_mut() {
                        if let TabKind::Search(p) = &mut tab.kind {
                            p.set_text("summer");
                        }
                    }
                }
                4 => self.screenshot_marks(),
                5 => self.export.open(crate::marks_ui::default_export_dir(&self.db_path)),
                6 => self.scan_win.open(&self.ctx.clone()),
                7 => self.remote_win.open = true,
                8 => self.ignore = Some(IgnoreEditor::new(&self.current_ignore())),
                9 => self.reports.open = true,
                _ => self.manifests_win.open = true,
            }
        } else if shot.prepared && shot.stage == 1 && !shot.picked && shot.idle_frames >= 3 {
            // select the biggest group: the inspector lists its copies, the tree highlights them
            shot.picked = true;
            shot.idle_frames = 0;
            let first = self.groups.first_row().map(|r| (r.file, r.group.content));
            if let Some((file, content)) = first {
                self.process_simple(vec![
                    Action::Inspect(Some(Target::File { file, content })),
                    Action::ShowInTab(file),
                ]);
            }
        } else if shot.prepared && shot.stage == 2 && !shot.picked && shot.idle_frames >= 3 {
            // open the biggest category to show its extensions
            shot.picked = true;
            shot.idle_frames = 0;
            let biggest = self
                .summaries
                .get(None)
                .and_then(|s| (0..s.names.len()).max_by_key(|&i| s.categories[i].bytes).map(|i| s.names[i].clone()));
            if let Some(name) = biggest {
                for (_, tab) in self.dock.iter_all_tabs_mut() {
                    if let TabKind::Summary(p) = &mut tab.kind {
                        p.expand(&name);
                    }
                }
            }
        } else if shot.prepared && (!matches!(shot.stage, 1 | 2) || shot.picked) && shot.idle_frames >= 12 {
            shot.requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
        }
    }

    /// Marks to show in the screenshots: in the two largest sources, the largest folder two
    /// levels below the drive; the first marked whole, the second only its files with a copy
    /// outside it. Then the Marks tab.
    fn screenshot_marks(&mut self) {
        use dupemap_core::marks::{MarkAction, MarkRule};
        let Some(ix) = self.index.clone() else { return };
        let roll = ix.rollup(&dupemap_core::index::Filter::default());
        let mut by_size: Vec<_> = ix.source_summaries().to_vec();
        by_size.sort_by_key(|s| std::cmp::Reverse(s.bytes));
        let mut add = Vec::new();
        for s in by_size.iter().take(2) {
            let mut d = ix.sources()[s.source as usize].root;
            for _ in 0..3 {
                match ix.dir_children(d).iter().copied().max_by_key(|&c| roll.stats[c as usize].bytes) {
                    Some(c) => d = c,
                    None => break,
                }
            }
            let action = if add.is_empty() { MarkAction::Delete } else { MarkAction::DeleteDuplicates };
            add.push(MarkRule::for_dir(&ix, d, action));
        }
        self.process_simple(vec![Action::Mark { label: "mark".into(), add, remove: vec![] }, Action::OpenMarks]);
    }

    /// A representative layout: every source on the left with its biggest folder inspected, the
    /// biggest folder of the second source on the right.
    fn screenshot_layout(&mut self) {
        let Some(ix) = self.index.clone() else { return };
        let mut by_size: Vec<_> = ix.source_summaries().to_vec();
        by_size.sort_by_key(|s| std::cmp::Reverse(s.bytes));
        let mut inspect = None;
        if let Some(first) = by_size.first() {
            let root = ix.sources()[first.source as usize].root;
            for (_, tab) in self.dock.iter_all_tabs_mut() {
                if let TabKind::Tree(p) = &mut tab.kind {
                    if p.root == Root::AllSources {
                        inspect = p.reveal_largest(&ix, Some(root));
                    }
                }
            }
        }
        if let Some(second) = by_size.get(1) {
            let mut tab = self.make_tab(Root::Source(second.db_id));
            if let TabKind::Tree(p) = &mut tab.kind {
                p.reveal_largest(&ix, None);
            }
            self.dock.main_surface_mut().split_right(NodeIndex::root(), 0.5, vec![tab]);
        }
        if let Some(d) = inspect {
            self.process_simple(vec![Action::Inspect(Some(Target::Dir(d)))]);
        }
    }

    // ------------------------------------------------------------------ background results

    fn poll(&mut self, ctx: &egui::Context) {
        if let Some(out) = self.job.as_ref().and_then(|j| j.poll()) {
            self.job = None;
            self.finish_job(out);
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
                Reply::Report(Err(e)) => self.note(format!("Could not read the report: {e}"), true),
                Reply::Manifests { manifests, alert_hosts } => {
                    self.manifests = manifests;
                    self.alert_hosts = alert_hosts;
                }
                Reply::Inspect { generation, target, info } => {
                    // a folder's copies are only known now: highlight where they mostly are
                    if let (Ok(i), Some(ix)) = (&info, &self.index) {
                        if let Inspection::Dir(d) = i.as_ref() {
                            if generation == self.generation && self.inspector.target() == Some(target) {
                                self.linked = Linked::for_dir(ix, &d.outside.main_dir);
                            }
                        }
                    }
                    self.inspector.on_reply(generation, target, info);
                }
                Reply::Groups { generation, query, page, total, totals, rows } => {
                    self.groups.insert(generation, query, page, total, totals, rows)
                }
                Reply::Summary { generation, scope, version, summary } => {
                    self.summaries.insert(generation, scope, version, summary)
                }
                Reply::Categories(Ok(c)) => {
                    self.set_categories(c);
                    self.note("File type categories saved", false);
                }
                Reply::Categories(Err(e)) => self.note(format!("Could not save the categories: {e}"), true),
                Reply::Marks { generation, version, state } => {
                    if generation == self.generation && version == self.marks.version {
                        self.mark_state = Some(state);
                        self.mark_state_for = (generation, version);
                    }
                }
                Reply::MarksNotSaved(e) => self.note(format!("Could not save the marks: {e}"), true),
                Reply::Exported(r) => {
                    if let Ok(d) = &r {
                        let scripts =
                            d.written.iter().filter(|w| w.what != dupemap_core::export::Written::List).count();
                        self.note(
                            format!(
                                "Wrote {} in {}",
                                format::plural(scripts as u64, "delete script", "delete scripts"),
                                d.dir.display()
                            ),
                            false,
                        );
                    }
                    self.export.running = false;
                    self.export.result = Some(r);
                }
                Reply::Search { pane, id, generation, result, elapsed_ms } => {
                    for (_, tab) in self.dock.iter_all_tabs_mut() {
                        if tab.id == pane {
                            if let TabKind::Search(p) = &mut tab.kind {
                                p.on_result(id, generation, result.clone(), elapsed_ms);
                            }
                        }
                    }
                }
            }
        }
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect()
        });
        if !dropped.is_empty() {
            self.queue_job(Job::Import(dropped.into_iter().map(ImportItem::decide).collect()));
        }
    }

    fn finish_job(&mut self, out: JobOutcome) {
        if let Some(ix) = out.index {
            self.generation += 1;
            self.worker.send(Query::SetIndex {
                generation: self.generation,
                index: ix.clone(),
                db_path: self.db_path.clone(),
            });
            self.files.reset(self.generation);
            self.groups.reset(self.generation);
            self.summaries.reset(self.generation, self.cats_version);
            // file and folder numbers belong to the old index
            self.inspector.clear();
            self.linked = None;
            if let Some(ed) = &mut self.ignore {
                ed.rebase(ix.ignore_settings());
            }
            self.index = Some(ix);
            self.restore_inspection();
            // folder and file numbers changed: work the marks out again
            self.mark_state = None;
            self.request_marks();
        }
        if !self.queue.iter().any(|j| matches!(j, Job::SetIgnore(_))) {
            self.ignore_pending = None;
        }
        self.manifests = out.manifests;
        self.alert_hosts = out.alert_hosts;
        if let Some(c) = out.categories {
            if *self.categories != c {
                self.set_categories(c);
            }
        }
        if let Some(m) = out.marks {
            if !self.marks.loaded {
                self.marks.load(m);
                self.mark_state = None;
                self.request_marks();
            }
        }
        for n in out.notes {
            self.note(n, false);
        }
        if out.cancelled {
            if out.was_scan {
                self.note(
                    "Scan stopped. Drives scanned completely were imported; the next scan continues where it stopped.",
                    false,
                );
            } else {
                self.note("Cancelled. Manifests imported before the cancel are kept.", false);
            }
        }
        if out.was_scan {
            self.last_scan = out.scan;
        }
        for ask in out.ask_checksums {
            self.checksum_dialogs.push_back(ChecksumDialog::new(ask));
        }
        if let Some(e) = out.error {
            self.note(e, true);
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
            self.note(msg, false);
            self.reports.show_reports(out.reports);
        }
    }

    // ------------------------------------------------------------------ session

    /// Select again what the saved session had selected, as soon as there is an index to find it
    /// in. Items that no longer exist are dropped.
    fn restore_inspection(&mut self) {
        let (Some(sel), Some(ix)) = (self.restore_inspected.take(), self.index.clone()) else { return };
        let target = match sel {
            Sel::Dir(id) => ix.dir_by_db_id(id).map(Target::Dir),
            Sel::File(id) => ix.file_by_db_id(id).map(|file| Target::File { file, content: ix.file_content(file) }),
        };
        if target.is_some() {
            self.inspect(target);
        }
    }

    /// The inspected item as a database id (survives index rebuilds).
    fn inspected_sel(&self) -> Option<Sel> {
        let Some(ix) = &self.index else { return self.restore_inspected };
        match self.inspector.target()? {
            Target::Dir(d) => Some(Sel::Dir(ix.dir_db_id(d))),
            Target::File { file, .. } => Some(Sel::File(ix.file_db_id(file))),
        }
    }

    /// Remember where the window is (not while maximised or minimised: the normal rectangle is
    /// the one to come back to).
    fn track_window(&mut self, ctx: &egui::Context) {
        let w = ctx.input(|i| {
            let v = i.viewport();
            (v.inner_rect, v.outer_rect, v.maximized.unwrap_or(false), v.minimized.unwrap_or(false), v.fullscreen)
        });
        let (inner, outer, maximized, minimized, fullscreen) = w;
        if minimized {
            return;
        }
        let prev = self.window;
        let mut now = prev.unwrap_or(WindowState { size: [1440.0, 900.0], pos: None, maximized: false });
        now.maximized = maximized;
        if !maximized && fullscreen != Some(true) {
            if let Some(r) = inner {
                now.size = [r.width(), r.height()];
            }
            if let Some(r) = outer {
                now.pos = Some([r.min.x, r.min.y]);
            }
        }
        if let Some(w) = now.sane() {
            self.window = Some(w);
        }
    }

    fn session_text(&self) -> Option<String> {
        serde_json::to_string(&SessionOut {
            version: VERSION,
            dock: &self.dock,
            focused: self.focused,
            inspector_open: self.inspector.open,
            inspector_width: self.inspector_width,
            inspected: self.inspected_sel(),
            window: self.window,
        })
        .ok()
    }

    /// Hand the session to the saver when it changed (checked about once a second).
    fn autosave(&mut self, ctx: &egui::Context) {
        if self.saver.is_none() {
            return;
        }
        self.track_window(ctx);
        let now = ctx.input(|i| i.time);
        if now - self.last_save_check < 1.0 {
            // a change right now is picked up a moment later even if nothing else happens
            ctx.request_repaint_after(std::time::Duration::from_millis(1000));
            return;
        }
        self.last_save_check = now;
        if let Some(text) = self.session_text() {
            if text != self.saved_text {
                if let Some(s) = &self.saver {
                    s.submit(text.clone());
                }
                self.saved_text = text;
            }
        }
        let failed = self.saver.as_ref().is_some_and(|s| s.failed());
        if failed && !self.save_failed_noted {
            self.save_failed_noted = true;
            self.note("Could not save the session (tabs and layout) next to the database", true);
        } else if !failed {
            self.save_failed_noted = false;
        }
    }

    /// Final save on exit: waits for the file to be written.
    fn save_on_exit(&mut self) {
        if let Some(text) = self.session_text() {
            if let Some(s) = &mut self.saver {
                s.submit(text);
                s.finish();
            }
        }
    }

    /// View → Reset layout: one tab with every source.
    fn reset_layout(&mut self) {
        let tab = self.make_tab(Root::AllSources);
        self.focused = Some(tab.id);
        self.dock = DockState::new(vec![tab]);
        self.inspect(None);
    }

    // ------------------------------------------------------------------ actions

    /// Carry out actions outside a frame (the screenshot sequence).
    fn process_simple(&mut self, actions: Vec<Action>) {
        self.process(None, actions);
    }

    fn process(&mut self, frame: Option<&eframe::Frame>, actions: Vec<Action>) {
        for a in actions {
            match a {
                Action::CopyText(t) => {
                    self.ctx.copy_text(t.clone());
                    let shown: String = t.chars().take(80).collect();
                    self.note(format!("Copied {shown}{}", if t.chars().count() > 80 { "…" } else { "" }), false);
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
                Action::Inspect(t) => self.inspect(t),
                Action::ShowInTab(f) => {
                    if let Some(ix) = self.index.clone() {
                        self.show_in_tab(&ix, ix.file_dir(f), Some(f));
                    }
                }
                Action::ShowDirInTab(d) => {
                    if let Some(ix) = self.index.clone() {
                        self.show_in_tab(&ix, d, None);
                    }
                }
                Action::OpenGroups => self.open_groups(),
                Action::OpenSummary(root) => self.open_summary(root),
                Action::OpenCategories => {
                    if self.category_editor.is_none() {
                        self.category_editor = Some(CategoryEditor::new(&self.categories));
                    }
                }
                Action::SetCategories(c) => self.worker.send(Query::SetCategories(c)),
                Action::ToggleInspector => self.inspector.open = !self.inspector.open,
                Action::OpenInspector => self.inspector.open = true,
                Action::ImportPaths(p) => self.queue_job(Job::Import(p.into_iter().map(ImportItem::decide).collect())),
                Action::ImportAs(path, how) => self.queue_job(Job::Import(vec![ImportItem { path, how: Some(how) }])),
                Action::OpenSearch => self.open_search(),
                Action::OpenScan => self.scan_win.open(&self.ctx.clone()),
                Action::OpenRemoteScan => self.remote_win.open = true,
                Action::StartScan(targets, settings) => {
                    let out_dir = self.scans_dir();
                    self.queue_job(Job::Scan { targets, settings, out_dir });
                }
                Action::RestartElevated => self.restart_elevated(),
                Action::Mark { label, add, remove } => {
                    if self.marks.loaded && self.marks.apply(label, &add, &remove) {
                        self.marks_changed();
                    }
                }
                Action::Undo => {
                    if let Some(l) = self.marks.undo() {
                        self.note(format!("Undone: {l}"), false);
                        self.marks_changed();
                    }
                }
                Action::Redo => {
                    if let Some(l) = self.marks.redo() {
                        self.note(format!("Redone: {l}"), false);
                        self.marks_changed();
                    }
                }
                Action::OpenMarks => self.open_marks(),
                Action::OpenExport => self.export.open(crate::marks_ui::default_export_dir(&self.db_path)),
                Action::Export(choice) => match (&self.mark_state, self.mark_state_for.0 == self.generation) {
                    (Some(st), true) => self.worker.send(Query::Export(Box::new(crate::worker::ExportRequest {
                        generation: self.generation,
                        state: st.clone(),
                        sources: choice.sources,
                        options: choice.options,
                        dir: choice.dir,
                    }))),
                    _ => {
                        self.export.running = false;
                        self.note("The marks are still being worked out; try again in a moment", true);
                    }
                },
                Action::CancelJob => {
                    if let Some(j) = &self.job {
                        j.cancel();
                    }
                }
                Action::RemoveManifests(ids) => self.queue_job(Job::Remove(ids)),
                Action::ShowReport(id) => self.worker.send(Query::Report { manifest_id: id }),
                Action::SetAlertDismissed(id, d) => {
                    self.worker.send(Query::SetAlertDismissed { manifest_id: id, dismissed: d })
                }
                Action::NewTab(root) => self.add_tab(root),
                Action::Focus(id) => self.focused = Some(id),
                Action::Note(n) => self.note(n, false),
                Action::Reveal(p) => {
                    if let Err(e) = actions::reveal(&p) {
                        self.note(format!("Could not open Explorer: {e}"), true);
                    }
                }
            }
        }
    }

    /// Inspect a file or folder (or nothing) and highlight its copies in every tab.
    fn inspect(&mut self, t: Option<Target>) {
        if t == self.inspector.target() {
            return;
        }
        self.inspector.select(self.generation, t, &self.worker);
        self.linked = match (t, &self.index) {
            (Some(Target::File { file, .. }), Some(ix)) => Linked::for_file(ix, file),
            // a folder's highlight arrives with its details (Reply::Inspect)
            _ => None,
        };
    }

    /// Open the tree at a file or folder in the best tab: one showing that source, else the
    /// focused tab if it shows every source, else any tab showing every source, else a new tab.
    fn show_in_tab(&mut self, ix: &Arc<Index>, dir: DirIx, file: Option<FileIx>) {
        let src_db = ix.sources()[ix.dir_source(dir) as usize].db_id;
        let (mut exact, mut focused_all, mut any_all) = (None, None, None);
        for (_, tab) in self.dock.iter_all_tabs() {
            if let TabKind::Tree(p) = &tab.kind {
                match p.root {
                    Root::Source(id) if id == src_db => {
                        exact.get_or_insert(tab.id);
                    }
                    Root::AllSources => {
                        if Some(tab.id) == self.focused {
                            focused_all = Some(tab.id);
                        }
                        any_all.get_or_insert(tab.id);
                    }
                    Root::Source(_) => {}
                }
            }
        }
        let id = match exact.or(focused_all).or(any_all) {
            Some(id) => id,
            None => {
                let tab = self.make_tab(Root::Source(src_db));
                let id = tab.id;
                self.dock.push_to_focused_leaf(tab);
                id
            }
        };
        let mut note = None;
        for (_, tab) in self.dock.iter_all_tabs_mut() {
            if tab.id == id {
                if let TabKind::Tree(p) = &mut tab.kind {
                    note = p.reveal(ix, dir, file);
                }
            }
        }
        self.activate_tab(id);
        if let Some(n) = note {
            self.note(n, false);
        }
    }

    fn activate_tab(&mut self, id: u64) {
        if let Some(path) = self.dock.find_tab_from(|t| t.id == id) {
            let _ = self.dock.set_active_tab(path);
            self.dock.set_focused_node_and_surface(NodePath { surface: path.surface, node: path.node });
        }
        self.focused = Some(id);
    }

    /// Ask the query thread what the marks mark in the current index.
    fn request_marks(&mut self) {
        if self.index.is_none() || !self.marks.loaded {
            return;
        }
        self.worker.send(Query::Marks {
            generation: self.generation,
            version: self.marks.version,
            rules: Arc::new(self.marks.rules().to_vec()),
        });
    }

    /// The marks changed: work them out and store them.
    fn marks_changed(&mut self) {
        self.request_marks();
        self.worker.send(Query::SaveMarks(Arc::new(self.marks.rules().to_vec())));
    }

    /// What the marks mark, when it is up to date with the index (it may be one edit behind the
    /// rules for a moment, which is fine for drawing).
    fn marks_now(&self) -> Option<&dupemap_core::marks::MarkState> {
        self.mark_state.as_deref().filter(|_| self.mark_state_for.0 == self.generation)
    }

    /// Switch to the Marks tab, opening it beside the tree the first time.
    fn open_marks(&mut self) {
        let existing = self.dock.iter_all_tabs().find(|(_, t)| matches!(t.kind, TabKind::Marks(_))).map(|(_, t)| t.id);
        if let Some(id) = existing {
            self.activate_tab(id);
            return;
        }
        let tab = Tab { id: self.new_tab_id(), kind: TabKind::Marks(MarksPane::default()) };
        let id = tab.id;
        if self.dock.iter_all_tabs().count() == 1 {
            self.dock.main_surface_mut().split_right(NodeIndex::root(), 0.5, vec![tab]);
        } else {
            self.dock.push_to_focused_leaf(tab);
        }
        self.activate_tab(id);
    }

    /// New categories: summaries are recomputed with them.
    fn set_categories(&mut self, c: Categories) {
        self.categories = Arc::new(c);
        self.cats_version += 1;
        self.summaries.reset(self.generation, self.cats_version);
        if let Some(ed) = &mut self.category_editor {
            ed.rebase(&self.categories);
        }
    }

    /// Show a summary: an open summary tab switches to `root`, otherwise a new one opens beside
    /// the tree.
    fn open_summary(&mut self, root: Root) {
        let mut existing = None;
        for (_, tab) in self.dock.iter_all_tabs_mut() {
            if let TabKind::Summary(p) = &mut tab.kind {
                p.root = root;
                existing = Some(tab.id);
                break;
            }
        }
        if let Some(id) = existing {
            self.activate_tab(id);
            return;
        }
        let tab = Tab { id: self.new_tab_id(), kind: TabKind::Summary(SummaryPane::new(root)) };
        let id = tab.id;
        if self.dock.iter_all_tabs().count() == 1 {
            self.dock.main_surface_mut().split_right(NodeIndex::root(), 0.5, vec![tab]);
        } else {
            self.dock.push_to_focused_leaf(tab);
        }
        self.activate_tab(id);
    }

    /// The root of the focused tab (what "Summary" opens on).
    fn focused_root(&self) -> Root {
        self.dock
            .iter_all_tabs()
            .find(|(_, t)| Some(t.id) == self.focused)
            .map(|(_, t)| match &t.kind {
                TabKind::Tree(p) => p.root,
                TabKind::Summary(p) => p.root,
                TabKind::Groups(_) | TabKind::Search(_) | TabKind::Marks(_) => Root::AllSources,
            })
            .unwrap_or(Root::AllSources)
    }

    /// Switch to the search tab (opening it beside the tree the first time) and put the cursor
    /// in its field.
    fn open_search(&mut self) {
        let mut existing = None;
        for (_, tab) in self.dock.iter_all_tabs_mut() {
            if let TabKind::Search(p) = &mut tab.kind {
                p.focus();
                existing = Some(tab.id);
                break;
            }
        }
        if let Some(id) = existing {
            self.activate_tab(id);
            return;
        }
        let tab = Tab { id: self.new_tab_id(), kind: TabKind::Search(SearchPane::default()) };
        let id = tab.id;
        if self.dock.iter_all_tabs().count() == 1 {
            self.dock.main_surface_mut().split_right(NodeIndex::root(), 0.5, vec![tab]);
        } else {
            self.dock.push_to_focused_leaf(tab);
        }
        self.activate_tab(id);
    }

    /// Where the built-in scan saves its manifests: beside the database.
    fn scans_dir(&self) -> PathBuf {
        self.db_path.parent().map(|p| p.join("scans")).unwrap_or_else(|| PathBuf::from("scans"))
    }

    /// Save the session and start DupeMap again as administrator, with the scan window open.
    fn restart_elevated(&mut self) {
        self.save_on_exit();
        let args = vec!["--db".to_string(), self.db_path.display().to_string(), "--scan".to_string()];
        match crate::elevate::restart_elevated(&args) {
            Ok(()) => self.ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            Err(e) => {
                // keep saving the session here
                if let Some(f) = self.session_file.clone() {
                    self.saver = Some(Saver::spawn(f));
                }
                self.note(format!("Not restarted: {e}"), true);
            }
        }
    }

    /// Switch to the duplicate groups tab, opening it beside the tree the first time.
    fn open_groups(&mut self) {
        let existing = self.dock.iter_all_tabs().find(|(_, t)| matches!(t.kind, TabKind::Groups(_))).map(|(_, t)| t.id);
        if let Some(id) = existing {
            self.activate_tab(id);
            return;
        }
        let tab = Tab { id: self.new_tab_id(), kind: TabKind::Groups(GroupsPane::default()) };
        let id = tab.id;
        if self.dock.iter_all_tabs().count() == 1 {
            self.dock.main_surface_mut().split_right(NodeIndex::root(), 0.5, vec![tab]);
        } else {
            self.dock.push_to_focused_leaf(tab);
        }
        self.activate_tab(id);
    }

    #[allow(unused_variables)]
    fn import_dialog(&mut self, frame: Option<&eframe::Frame>) {
        #[cfg(windows)]
        {
            let mut dialog = rfd::FileDialog::new()
                .set_title("Import hash manifests or checksum lists")
                .add_filter(
                    "Manifests and checksum lists (*.tsv, *.txt, *.sha256)",
                    &["tsv", "txt", "sha256", "sha256sum"],
                )
                .add_filter("All files", &["*"]);
            if let Some(frame) = frame {
                dialog = dialog.set_parent(frame);
            }
            let picked = dialog.pick_files();
            if let Some(files) = picked {
                if !files.is_empty() {
                    self.queue_job(Job::Import(files.into_iter().map(ImportItem::decide).collect()));
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
                if ui.button("Scan this PC…").on_hover_text("Hash this computer's drives and import them").clicked() {
                    actions.push(Action::OpenScan);
                    ui.close();
                }
                if ui
                    .button("Scan another computer…")
                    .on_hover_text("Save a script to run on another Windows or Linux computer")
                    .clicked()
                {
                    actions.push(Action::OpenRemoteScan);
                    ui.close();
                }
                ui.separator();
                let any_marked = self.marks_now().is_some_and(|m| !m.is_empty());
                if ui
                    .add_enabled(any_marked, egui::Button::new("Write delete scripts…"))
                    .on_hover_text("A script per source to remove the files marked for deletion")
                    .clicked()
                {
                    actions.push(Action::OpenExport);
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
            ui.menu_button("Edit", |ui| {
                let undo = self.marks.undo_label().map(|l| format!("Undo {l}"));
                if ui
                    .add_enabled(
                        undo.is_some(),
                        egui::Button::new(undo.as_deref().unwrap_or("Undo")).shortcut_text("Ctrl+Z"),
                    )
                    .clicked()
                {
                    actions.push(Action::Undo);
                    ui.close();
                }
                let redo = self.marks.redo_label().map(|l| format!("Redo {l}"));
                if ui
                    .add_enabled(
                        redo.is_some(),
                        egui::Button::new(redo.as_deref().unwrap_or("Redo")).shortcut_text("Ctrl+Y"),
                    )
                    .clicked()
                {
                    actions.push(Action::Redo);
                    ui.close();
                }
                ui.separator();
                if ui.add(egui::Button::new("Marked for deletion").shortcut_text("Ctrl+M")).clicked() {
                    actions.push(Action::OpenMarks);
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
                if ui.add(egui::Button::new("Search").shortcut_text("Ctrl+F")).clicked() {
                    actions.push(Action::OpenSearch);
                    ui.close();
                }
                if ui.add(egui::Button::new("Duplicate groups").shortcut_text("Ctrl+G")).clicked() {
                    actions.push(Action::OpenGroups);
                    ui.close();
                }
                if ui.add(egui::Button::new("Summary by file type").shortcut_text("Ctrl+U")).clicked() {
                    actions.push(Action::OpenSummary(self.focused_root()));
                    ui.close();
                }
                if ui.button("File type categories…").clicked() {
                    actions.push(Action::OpenCategories);
                    ui.close();
                }
                let label = if self.inspector.open { "Hide inspector" } else { "Show inspector" };
                if ui.add(egui::Button::new(label).shortcut_text("Ctrl+I")).clicked() {
                    actions.push(Action::ToggleInspector);
                    ui.close();
                }
                if ui
                    .button("Reset layout")
                    .on_hover_text("Close every tab and start with one tab of all sources")
                    .clicked()
                {
                    self.reset_layout();
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
                // the right side (marks, alerts, a running job) needs its room in a narrow window
                let alerts = self.manifests.iter().any(|m| m.severity == Severity::Alert && !m.alert_dismissed);
                let marked = self.mark_state.as_deref().is_some_and(|m| !m.is_empty());
                let reserve = if marked { 340.0 } else { 0.0 }
                    + if alerts { 220.0 } else { 0.0 }
                    + if self.job.is_some() { 420.0 } else { 0.0 };
                let hint = if ui.available_width() - reserve > 440.0 {
                    "· only files ≥ the scripts' minimum size (1 MB) are hashed"
                } else {
                    "· files ≥ 1 MB"
                };
                ui.label(RichText::new(hint).color(colors::INK_MUTED)).on_hover_text(
                    "Only files of at least the scripts' minimum size (1 MB) are hashed: smaller files never appear here.",
                );
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
                if let Some(m) =
                    self.mark_state.as_deref().filter(|m| !m.is_empty() && self.mark_state_for.0 == self.generation)
                {
                    let mut text = format!("Marked: {}", crate::marks_ui::files_bytes(m.total.files, m.total.bytes));
                    if m.total.last_files > 0 {
                        text.push_str(&format!(" · ⚠ {} last copies", format::count(m.total.last_files)));
                    }
                    let b = egui::Button::new(RichText::new(text).color(colors::INK))
                        .fill(colors::alpha(colors::CRITICAL, if m.total.last_files > 0 { 0.35 } else { 0.15 }))
                        .stroke(egui::Stroke::new(1.0, colors::alpha(colors::CRITICAL, 0.7)));
                    if ui.add(b).on_hover_text("Files marked for deletion: open the list (Ctrl+M)").clicked() {
                        actions.push(Action::OpenMarks);
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
            let size = egui::vec2(220.0, 32.0);
            if ui.add(egui::Button::new(RichText::new("Scan this PC…").size(15.0)).min_size(size)).clicked() {
                actions.push(Action::OpenScan);
            }
            ui.label(
                RichText::new("hash this computer's drives (its own, USB, network) and import them")
                    .color(colors::INK_MUTED),
            );
            ui.add_space(10.0);
            if ui.add(egui::Button::new(RichText::new("Scan another computer…").size(15.0)).min_size(size)).clicked()
            {
                actions.push(Action::OpenRemoteScan);
            }
            ui.label(
                RichText::new("save a script to run on another Windows or Linux computer").color(colors::INK_MUTED),
            );
            ui.add_space(10.0);
            if ui.add(egui::Button::new(RichText::new("Import manifests…").size(15.0)).min_size(size)).clicked() {
                actions.push(Action::Import);
            }
            ui.label(
                RichText::new("or drop manifests or checksum lists (or a folder of them) anywhere on this window")
                    .color(colors::INK_MUTED),
            );
            ui.add_space(18.0);
            ui.label(
                RichText::new(
                    "A manifest lists one line per file with its SHA-256, size, source and path; the scan and the \
                     export scripts write them, and checksum lists from sha256sum or rclone hashsum import too. \
                     Import one per machine, drive or cloud account; DupeMap shows every copy of every file across \
                     all of them. It never deletes anything itself.",
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
        // undo and redo of marks, unless a text field has the keyboard (it has its own undo)
        if ctx.memory(|m| m.focused().is_none()) {
            let (undo, redo) = ctx.input_mut(|i| {
                (
                    i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::Z)),
                    i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::Y))
                        || i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z)),
                )
            });
            if undo {
                actions.push(Action::Undo);
            }
            if redo {
                actions.push(Action::Redo);
            }
        }
        if ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::M))) {
            actions.push(Action::OpenMarks);
        }
        let (open, tab, quit, groups, inspector, summary, find) = ctx.input_mut(|i| {
            (
                i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::O)),
                i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::T)),
                i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::Q)),
                i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::G)),
                i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::I)),
                i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::U)),
                i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::F)),
            )
        });
        if find {
            actions.push(Action::OpenSearch);
        }
        if summary {
            actions.push(Action::OpenSummary(self.focused_root()));
        }
        if groups {
            actions.push(Action::OpenGroups);
        }
        if inspector {
            actions.push(Action::ToggleInspector);
        }
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

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.save_on_exit();
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

        let has_data = self.index.as_ref().is_some_and(|ix| !ix.sources().is_empty()) || !self.manifests.is_empty();
        if self.inspector.open && has_data {
            // about a quarter of a small window, at most 400 px; resizable
            let default = (ui.available_width() * 0.28).clamp(260.0, 400.0);
            let default = self.inspector_width.map(|w| w.clamp(240.0, 760.0)).unwrap_or(default);
            let shown = egui::Panel::right("inspector")
                .resizable(true)
                .default_size(default)
                .size_range(240.0..=760.0)
                .frame(
                    egui::Frame::new()
                        .fill(colors::PANEL)
                        .inner_margin(egui::Margin::symmetric(12, 8))
                        .stroke(egui::Stroke::new(1.0, colors::BASELINE)),
                )
                .show(ui, |ui| {
                    let marks =
                        if self.mark_state_for.0 == self.generation { self.mark_state.as_deref() } else { None };
                    self.inspector.ui(ui, self.index.as_deref(), &self.marks, marks, &mut actions)
                });
            self.inspector_width = Some(shown.response.rect.width());
        }

        egui::CentralPanel::no_frame().show(ui, |ui| {
            let ix = self.index.clone();
            match ix {
                Some(ix) if has_data => {
                    let mut added: Vec<NodePath> = Vec::new();
                    let style = theme::dock_style(ui.style());
                    let mut viewer = Viewer {
                        ix: &ix,
                        generation: self.generation,
                        files: &mut self.files,
                        groups: &mut self.groups,
                        summaries: &mut self.summaries,
                        categories: &self.categories,
                        worker: &self.worker,
                        actions: &mut actions,
                        focused: self.focused,
                        alert_hosts: &self.alert_hosts,
                        linked: self.linked.as_ref(),
                        added: &mut added,
                        book: &self.marks,
                        marks: if self.mark_state_for.0 == self.generation { self.mark_state.as_deref() } else { None },
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
        if let Some(ix) = self.index.clone() {
            let marks = if self.mark_state_for.0 == self.generation { self.mark_state.as_deref() } else { None };
            self.export.show(&ctx, &ix, marks, Some(frame), &mut actions);
        }
        self.reports.show(&ctx, &self.manifests, &mut actions);
        if let Some(ed) = &mut self.ignore {
            if !ed.show(&ctx, self.index.as_deref(), busy, &mut actions) {
                self.ignore = None;
            }
        }
        if let Some(ed) = &mut self.category_editor {
            // the extensions in no category come from the all-sources summary
            if self.index.is_some() && self.summaries.get(None).is_none() {
                self.summaries.request(&self.worker, None, &self.categories);
            }
            if !ed.show(&ctx, self.summaries.get(None).map(|s| s.as_ref()), &mut actions) {
                self.category_editor = None;
            }
        }
        // checksum lists waiting for their source, one at a time
        let sources: Vec<String> =
            self.index.as_ref().map(|ix| ix.sources().iter().map(|s| s.host.clone()).collect()).unwrap_or_default();
        if let Some(d) = self.checksum_dialogs.front_mut() {
            if !d.show(&ctx, &sources, Some(frame), &mut actions) {
                self.checksum_dialogs.pop_front();
            }
        }
        {
            let progress = self.job.as_ref().map(|j| (j.progress(), j.is_cancelling()));
            let running = progress.as_ref().and_then(|(p, _)| p.scan.clone());
            let out_dir = self.scans_dir();
            let mut cx = ScanCtx {
                running: running.as_ref(),
                busy: self.busy(),
                cancelling: progress.as_ref().is_some_and(|(_, c)| *c),
                last: &self.last_scan,
                elevated: self.elevated,
                out_dir: &out_dir,
                actions: &mut actions,
            };
            self.scan_win.show(&ctx, &mut cx);
            if running.is_some() {
                ctx.request_repaint_after(std::time::Duration::from_millis(250));
            }
        }
        let scripts_dir = self.db_path.parent().map(PathBuf::from).unwrap_or_default();
        self.remote_win.show(&ctx, Some(frame), &scripts_dir, &mut actions);
        if self.about {
            self.about_window(&ctx);
        }
        if self.log_open {
            self.log_window(&ctx);
        }
        self.drop_overlay(&ctx);
        self.process(Some(frame), actions);
        self.autosave(&ctx);
        if self.shot.is_some() {
            self.screenshot_step(&ctx);
        }
    }
}

struct Viewer<'a> {
    ix: &'a Arc<Index>,
    generation: u64,
    files: &'a mut FileCache,
    groups: &'a mut GroupCache,
    summaries: &'a mut SummaryCache,
    categories: &'a Arc<Categories>,
    worker: &'a Worker,
    actions: &'a mut Vec<Action>,
    focused: Option<u64>,
    alert_hosts: &'a BTreeSet<String>,
    linked: Option<&'a Linked>,
    added: &'a mut Vec<NodePath>,
    book: &'a MarkBook,
    marks: Option<&'a dupemap_core::marks::MarkState>,
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
            TabKind::Groups(g) => g.title().into(),
            TabKind::Summary(p) => p.title(Some(self.ix)).into(),
            TabKind::Search(p) => p.title().into(),
            TabKind::Marks(p) => {
                let n = self.marks.map(|m| m.total.files).unwrap_or(0);
                let last = self.marks.is_some_and(|m| m.total.last_files > 0);
                let title = if n > 0 { format!("{} ({})", p.title(), format::count(n)) } else { p.title() };
                if last {
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
                    linked: self.linked,
                    book: self.book,
                    marks: self.marks,
                };
                p.ui(ui, &mut cx);
            }
            TabKind::Groups(g) => {
                let mut cx = GroupsCtx {
                    ix: self.ix,
                    generation: self.generation,
                    cache: self.groups,
                    worker: self.worker,
                    actions: self.actions,
                    pane_id: tab.id,
                    focused: self.focused == Some(tab.id),
                    linked: self.linked,
                    marks: self.marks,
                };
                g.ui(ui, &mut cx);
            }
            TabKind::Search(p) => {
                let mut cx = SearchCtx {
                    ix: self.ix,
                    generation: self.generation,
                    worker: self.worker,
                    actions: self.actions,
                    pane_id: tab.id,
                    focused: self.focused == Some(tab.id),
                    linked: self.linked,
                    book: self.book,
                    marks: self.marks,
                };
                p.ui(ui, &mut cx);
            }
            TabKind::Marks(p) => {
                let mut cx = MarksCtx {
                    ix: self.ix,
                    book: self.book,
                    state: self.marks,
                    actions: self.actions,
                    pane_id: tab.id,
                    focused: self.focused == Some(tab.id),
                };
                p.ui(ui, &mut cx);
            }
            TabKind::Summary(p) => {
                let mut cx = SummaryCtx {
                    ix: self.ix,
                    cache: self.summaries,
                    categories: self.categories,
                    worker: self.worker,
                    actions: self.actions,
                    pane_id: tab.id,
                    focused: self.focused == Some(tab.id),
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
