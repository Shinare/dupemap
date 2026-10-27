//! The application: owns the index, the background threads, the dock of tabs and the windows.

use crate::actions::{self, Action};
use crate::categories_ui::CategoryEditor;
use crate::colors;
use crate::files::FileCache;
use crate::format;
use crate::groups::{GroupCache, GroupsCtx, GroupsPane};
use crate::ignore_ui::IgnoreEditor;
use crate::inspector::Inspector;
use crate::manifests_ui::{ManifestsWindow, ReportWindow};
use crate::pane::{PaneCtx, Sel, TreePane};
use crate::selection::{Linked, Target};
use crate::session::{Saver, Session, SessionOut, WindowState, VERSION};
use crate::summary_ui::{SummaryCache, SummaryCtx, SummaryPane};
use crate::theme;
use crate::tree::Root;
use crate::widgets;
use crate::worker::{self, Inspection, Job, JobOutcome, Query, Repaint, Reply, RunningJob, Worker};
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
    Tree(TreePane),
    Groups(GroupsPane),
    Summary(SummaryPane),
}

impl TabKind {
    /// The tab was read from a saved session.
    pub fn after_load(&mut self) {
        match self {
            TabKind::Tree(p) => p.after_load(),
            TabKind::Groups(p) => p.after_load(),
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
    /// Autosave of the session (none when saving is off).
    saver: Option<Saver>,
    /// The text last handed to the saver.
    saved_text: String,
    last_save_check: f64,
    save_failed_noted: bool,
    window: Option<WindowState>,
    inspector_width: Option<f32>,
    /// The inspected item of the saved session, selected once the index is there.
    restore_inspected: Option<Sel>,
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
        let saver = opts.session_file.map(Saver::spawn);
        DupeMapApp {
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
            saver,
            saved_text: String::new(),
            last_save_check: 0.0,
            save_failed_noted: false,
            window,
            inspector_width,
            restore_inspected,
        }
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
                TabKind::Groups(_) => Root::AllSources,
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
        const SHOTS: [&str; 6] = ["", "groups", "summary", "ignore", "report", "manifests"];
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
            && !self.inspector.is_pending();
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
            match stage {
                0 => self.screenshot_layout(),
                1 => self.process_simple(vec![Action::OpenGroups]),
                2 => {
                    // the summary is a wide table: give it the room the inspector took
                    self.inspector.open = false;
                    self.process_simple(vec![Action::OpenSummary(Root::AllSources)]);
                }
                3 => self.ignore = Some(IgnoreEditor::new(&self.current_ignore())),
                4 => self.reports.open = true,
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
            }
        }
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect()
        });
        if !dropped.is_empty() {
            self.queue_job(Job::Import(dropped));
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
        for n in out.notes {
            self.note(n, false);
        }
        if out.cancelled {
            self.note("Cancelled. Manifests imported before the cancel are kept.", false);
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
                Action::ImportPaths(p) => self.queue_job(Job::Import(p)),
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
                TabKind::Groups(_) => Root::AllSources,
            })
            .unwrap_or(Root::AllSources)
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
                .set_title("Import hash manifests")
                .add_filter("Hash manifests (*.tsv)", &["tsv"])
                .add_filter("All files", &["*"]);
            if let Some(frame) = frame {
                dialog = dialog.set_parent(frame);
            }
            let picked = dialog.pick_files();
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
        let (open, tab, quit, groups, inspector, summary) = ctx.input_mut(|i| {
            (
                i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::O)),
                i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::T)),
                i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::Q)),
                i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::G)),
                i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::I)),
                i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::U)),
            )
        });
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
                .show(ui, |ui| self.inspector.ui(ui, self.index.as_deref(), &mut actions));
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
        if let Some(ed) = &mut self.category_editor {
            // the extensions in no category come from the all-sources summary
            if self.index.is_some() && self.summaries.get(None).is_none() {
                self.summaries.request(&self.worker, None, &self.categories);
            }
            if !ed.show(&ctx, self.summaries.get(None).map(|s| s.as_ref()), &mut actions) {
                self.category_editor = None;
            }
        }
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
                };
                g.ui(ui, &mut cx);
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
