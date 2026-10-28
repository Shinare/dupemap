//! Background threads.
//!
//! * The **query worker** answers file listings (they need names from SQLite, which takes a
//!   moment for very large folders) so the UI thread never waits on the database.
//! * The **search thread** runs name and hash searches on a database connection of its own, so
//!   browsing stays responsive while a search reads the whole file table.
//! * A **job** runs one long operation (open, import, scan, remove, change ignore rules, demo
//!   data) with progress and cancellation, and ends by building a fresh index.

use crate::selection::Target;
use dupemap_core::db::{ChecksumImport, ImportAs};
use dupemap_core::db::{ManifestInfo, Progress as ImportProgress};
use dupemap_core::export::{ExportOptions, WrittenFile};
use dupemap_core::ignore::IgnoreSettings;
use dupemap_core::index::{
    DirIx, DirStats, DupGroup, DupQuery, FileIx, FileRow, Filter, Index, ListOptions, Location, Node, OutsideCopies,
};
use dupemap_core::manifest::{find_size_list, first_checksum_paths, guess_checksums, sniff_format, ListFormat};
use dupemap_core::marks::{resolve, MarkRule, MarkState, MarkTotals, Place};
use dupemap_core::natsort::natural_cmp;
use dupemap_core::scan::{scan_volume, ScanProgress, ScanSettings, Volume, VolumeResult};
use dupemap_core::search::{search, SearchQuery, SearchResult, MAX_HITS};
use dupemap_core::summary::{Categories, Summary};
use dupemap_core::{Db, Error, ImportReport, Severity};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

/// Asks the UI to repaint (wraps `egui::Context::request_repaint`).
pub type Repaint = Arc<dyn Fn() + Send + Sync>;

pub enum Query {
    /// A new index (and the database it came from) replaces the old one.
    SetIndex {
        generation: u64,
        index: Arc<Index>,
        db_path: PathBuf,
    },
    Files {
        generation: u64,
        dir: DirIx,
        opts: ListOptions,
    },
    /// The stored import report of a manifest.
    Report {
        manifest_id: i64,
    },
    /// Dismiss (or restore) a manifest's alert; replies with the refreshed manifest list.
    SetAlertDismissed {
        manifest_id: i64,
        dismissed: bool,
    },
    /// Everything the inspector shows about a file or folder.
    Inspect {
        generation: u64,
        target: Target,
    },
    /// One page of duplicate groups (plus the totals when `totals` is set).
    Groups {
        generation: u64,
        query: DupQuery,
        page: usize,
        totals: bool,
    },
    /// Totals by file type for one source (database id) or all of them.
    Summary {
        generation: u64,
        scope: Option<i64>,
        categories: Arc<Categories>,
        /// Which version of the categories this is (replies for older ones are dropped).
        version: u64,
    },
    /// Store edited categories.
    SetCategories(Categories),
    /// Work out what the marks for deletion mark (only the newest of several waiting is done).
    Marks {
        generation: u64,
        version: u64,
        rules: Arc<Vec<MarkRule>>,
    },
    /// Store the marks for deletion (only the newest of several waiting is written).
    SaveMarks(Arc<Vec<MarkRule>>),
    /// Write delete scripts.
    Export(Box<ExportRequest>),
    /// Lay out a treemap tab (only the newest waiting request of each tab is done).
    Treemap(Box<crate::treemap_ui::TreemapRequest>),
    /// Find files and folders by name or hash (answered by the search thread; a newer search
    /// stops an older one).
    Search {
        /// The tab asking; its newer search stops its older one.
        pane: u64,
        id: u64,
        generation: u64,
        query: SearchQuery,
    },
}

/// What to export: the marks as worked out for this index, for some sources.
pub struct ExportRequest {
    pub generation: u64,
    pub state: Arc<MarkState>,
    pub sources: Vec<dupemap_core::index::SourceIx>,
    pub options: ExportOptions,
    pub dir: PathBuf,
}

/// What an export wrote.
#[derive(Debug, Clone)]
pub struct ExportDone {
    pub dir: PathBuf,
    pub written: Vec<WrittenFile>,
    /// Marked files left out because every copy of them is marked.
    pub left_out: MarkTotals,
    /// Marked files no script can name (a line break in the name).
    pub unscriptable: usize,
}

/// Where each mark (source, path, folder?) is in the index of one generation.
type PlaceCache = HashMap<(String, String, bool), Option<Place>>;

/// Rows per page of duplicate groups.
pub const GROUP_PAGE: usize = 200;

/// The inspector's data for a file: every copy, grouped by source by the UI.
#[derive(Debug, Clone)]
pub struct FileInfo {
    pub file: FileIx,
    pub name: String,
    pub size: u64,
    /// Whether the size was listed, taken from a copy, or is not known.
    pub size_state: dupemap_core::index::SizeState,
    /// `sha256` or the flagged algorithm.
    pub algo: String,
    /// Lowercase hex for SHA-256, `algo:value` otherwise.
    pub hash: String,
    pub flagged: bool,
    pub copies: u32,
    pub other_sources: u32,
    /// Every copy including this one, in source order and natural path order.
    pub locations: Vec<Location>,
    /// Per location: the file is on this computer (Windows only). Checked once here, off the UI
    /// thread, because looking at a path on a sleeping drive or share can take seconds.
    pub on_this_pc: Vec<bool>,
}

/// The inspector's data for a folder.
#[derive(Debug, Clone)]
pub struct DirInfo {
    pub dir: DirIx,
    pub path: String,
    /// Unfiltered statistics.
    pub stats: DirStats,
    pub outside: OutsideCopies,
}

#[derive(Debug, Clone)]
pub enum Inspection {
    File(FileInfo),
    Dir(DirInfo),
}

/// A duplicate group with a representative file (the first copy in tree order).
#[derive(Debug, Clone)]
pub struct GroupRow {
    pub group: DupGroup,
    pub file: FileIx,
    pub name: String,
}

pub enum Reply {
    Treemap(Arc<crate::treemap_ui::TreemapView>),
    Files {
        generation: u64,
        dir: DirIx,
        opts: ListOptions,
        files: Result<Arc<Vec<FileRow>>, String>,
    },
    Report(Result<Box<ImportReport>, String>),
    Manifests {
        manifests: Vec<ManifestInfo>,
        alert_hosts: BTreeSet<String>,
    },
    Inspect {
        generation: u64,
        target: Target,
        info: Result<Box<Inspection>, String>,
    },
    Groups {
        generation: u64,
        query: DupQuery,
        page: usize,
        total: usize,
        totals: Option<(usize, u64)>,
        rows: Result<Arc<Vec<GroupRow>>, String>,
    },
    Summary {
        generation: u64,
        scope: Option<i64>,
        version: u64,
        summary: Arc<Summary>,
    },
    /// The categories as stored (cleaned up), or why they could not be.
    Categories(Result<Categories, String>),
    Search {
        pane: u64,
        id: u64,
        generation: u64,
        result: Result<Arc<SearchResult>, String>,
        elapsed_ms: u64,
    },
    Marks {
        generation: u64,
        version: u64,
        state: Arc<MarkState>,
    },
    /// Saving the marks failed.
    MarksNotSaved(String),
    Exported(Result<ExportDone, String>),
}

pub struct Worker {
    tx: Sender<Query>,
    rx: Receiver<Reply>,
    search_tx: Sender<Query>,
    /// The newest search of each tab; an older one still running stops.
    latest_search: Arc<Mutex<HashMap<u64, u64>>>,
}

impl Worker {
    pub fn spawn(repaint: Repaint) -> Worker {
        let (qtx, qrx) = channel::<Query>();
        let (rtx, rrx) = channel::<Reply>();
        let latest_search = Arc::new(Mutex::new(HashMap::new()));
        let search_tx = spawn_search_thread(rtx.clone(), repaint.clone(), latest_search.clone());
        std::thread::Builder::new()
            .name("dupemap-query".into())
            .spawn(move || {
                let mut state: Option<(u64, Arc<Index>, Db, PathBuf)> = None;
                // where each mark is, for the index of this generation
                let mut places: (u64, PlaceCache) = (0, HashMap::new());
                while let Ok(first) = qrx.recv() {
                    let mut batch = vec![first];
                    batch.extend(qrx.try_iter());
                    // the newest index wins; queries for older generations are dropped
                    for q in prioritise(batch) {
                        match q {
                            Query::SetIndex { generation, index, db_path } => {
                                let reuse = matches!(&state, Some((_, _, _, p)) if *p == db_path);
                                if reuse {
                                    if let Some(s) = state.as_mut() {
                                        s.0 = generation;
                                        s.1 = index;
                                    }
                                } else {
                                    match Db::open_reader(&db_path) {
                                        Ok(db) => state = Some((generation, index, db, db_path)),
                                        Err(_) => state = None,
                                    }
                                }
                            }
                            Query::Files { generation, dir, opts } => {
                                let Some((g, ix, db, _)) = &state else { continue };
                                if *g != generation {
                                    continue;
                                }
                                let files = ix
                                    .list(db, Node::Dir(dir), &opts)
                                    .map(|l| Arc::new(l.files))
                                    .map_err(|e| e.to_string());
                                let _ = rtx.send(Reply::Files { generation, dir, opts, files });
                                repaint();
                            }
                            Query::Report { manifest_id } => {
                                let Some((_, _, db, _)) = &state else { continue };
                                let _ = rtx.send(Reply::Report(
                                    db.report(manifest_id).map(Box::new).map_err(|e| e.to_string()),
                                ));
                                repaint();
                            }
                            Query::Inspect { generation, target } => {
                                let Some((g, ix, db, _)) = &state else { continue };
                                if *g != generation {
                                    continue;
                                }
                                let info = inspect(ix, db, target).map(Box::new).map_err(|e| e.to_string());
                                let _ = rtx.send(Reply::Inspect { generation, target, info });
                                repaint();
                            }
                            Query::Groups { generation, query, page, totals } => {
                                let Some((g, ix, db, _)) = &state else { continue };
                                if *g != generation {
                                    continue;
                                }
                                let (total, groups) = ix.dup_groups(&query, page * GROUP_PAGE, GROUP_PAGE);
                                let totals = totals.then(|| ix.dup_totals(&query));
                                let rows = groups
                                    .into_iter()
                                    .map(|group| {
                                        let file = ix.content_files(group.content)[0];
                                        let name = db.file_record(ix.file_db_id(file))?.name;
                                        Ok(GroupRow { group, file, name })
                                    })
                                    .collect::<Result<Vec<_>, Error>>()
                                    .map(Arc::new)
                                    .map_err(|e| e.to_string());
                                let _ = rtx.send(Reply::Groups { generation, query, page, total, totals, rows });
                                repaint();
                            }
                            Query::Summary { generation, scope, categories, version } => {
                                let Some((g, ix, _, _)) = &state else { continue };
                                if *g != generation {
                                    continue;
                                }
                                // a source that is gone summarises as nothing
                                let summary = match scope.map(|id| ix.source_ix(id)) {
                                    Some(None) => Summary::empty(&categories),
                                    Some(Some(s)) => ix.summary(Some(s), &categories),
                                    None => ix.summary(None, &categories),
                                };
                                let summary = Arc::new(summary);
                                let _ = rtx.send(Reply::Summary { generation, scope, version, summary });
                                repaint();
                            }
                            Query::SetCategories(c) => {
                                let Some((_, _, db, _)) = &state else { continue };
                                let r = db.set_categories(&c).and_then(|_| db.categories()).map_err(|e| e.to_string());
                                let _ = rtx.send(Reply::Categories(r));
                                repaint();
                            }
                            Query::Search { .. } => {}
                            Query::Treemap(req) => {
                                let Some((g, ix, db, _)) = &state else { continue };
                                if *g != req.key.generation {
                                    continue;
                                }
                                let view = crate::treemap_ui::make_view(ix, db, &req);
                                let _ = rtx.send(Reply::Treemap(Arc::new(view)));
                                repaint();
                            }
                            Query::Marks { generation, version, rules } => {
                                let Some((g, ix, db, _)) = &state else { continue };
                                if *g != generation {
                                    continue;
                                }
                                if places.0 != generation {
                                    places = (generation, HashMap::new());
                                }
                                let key = |r: &MarkRule| (r.host.clone(), r.path.clone(), r.is_dir);
                                let missing: Vec<MarkRule> =
                                    rules.iter().filter(|r| !places.1.contains_key(&key(r))).cloned().collect();
                                if !missing.is_empty() {
                                    match resolve(ix, db, &missing) {
                                        Ok(found) => {
                                            for (r, p) in missing.iter().zip(found) {
                                                places.1.insert(key(r), p);
                                            }
                                        }
                                        Err(_) => {
                                            for r in &missing {
                                                places.1.insert(key(r), None);
                                            }
                                        }
                                    }
                                }
                                let at: Vec<Option<Place>> =
                                    rules.iter().map(|r| places.1.get(&key(r)).copied().flatten()).collect();
                                let st = Arc::new(MarkState::compute(ix, &rules, &at));
                                let _ = rtx.send(Reply::Marks { generation, version, state: st });
                                repaint();
                            }
                            Query::SaveMarks(rules) => {
                                let Some((_, _, db, _)) = &state else { continue };
                                if let Err(e) = db.set_marks(&rules) {
                                    let _ = rtx.send(Reply::MarksNotSaved(e.to_string()));
                                    repaint();
                                }
                            }
                            Query::Export(req) => {
                                let Some((g, ix, db, _)) = &state else { continue };
                                let r = if *g != req.generation {
                                    Err("the data changed meanwhile; try again".to_string())
                                } else {
                                    export(ix, db, &req).map_err(|e| e.to_string())
                                };
                                let _ = rtx.send(Reply::Exported(r));
                                repaint();
                            }
                            Query::SetAlertDismissed { manifest_id, dismissed } => {
                                let Some((_, _, db, _)) = &state else { continue };
                                if db.set_alert_dismissed(manifest_id, dismissed).is_ok() {
                                    let manifests = db.manifests().unwrap_or_default();
                                    let alert_hosts = alert_hosts(db, &manifests);
                                    let _ = rtx.send(Reply::Manifests { manifests, alert_hosts });
                                    repaint();
                                }
                            }
                        }
                    }
                }
            })
            .expect("spawn query thread");
        Worker { tx: qtx, rx: rrx, search_tx, latest_search }
    }

    pub fn send(&self, q: Query) {
        match q {
            Query::Search { pane, id, .. } => {
                if let Ok(mut l) = self.latest_search.lock() {
                    l.insert(pane, id);
                }
                let _ = self.search_tx.send(q);
            }
            Query::SetIndex { generation, ref index, ref db_path } => {
                let _ =
                    self.search_tx.send(Query::SetIndex { generation, index: index.clone(), db_path: db_path.clone() });
                let _ = self.tx.send(q);
            }
            q => {
                let _ = self.tx.send(q);
            }
        }
    }

    pub fn poll(&self) -> Vec<Reply> {
        self.rx.try_iter().collect()
    }
}

/// The search thread: its own connection, the newest index, one search at a time (of those
/// waiting, only each tab's newest runs).
fn spawn_search_thread(rtx: Sender<Reply>, repaint: Repaint, latest: Arc<Mutex<HashMap<u64, u64>>>) -> Sender<Query> {
    let (tx, rx) = channel::<Query>();
    std::thread::Builder::new()
        .name("dupemap-search".into())
        .spawn(move || {
            let mut state: Option<(u64, Arc<Index>, Db, PathBuf)> = None;
            let is_latest = |pane: u64, id: u64| latest.lock().map(|l| l.get(&pane) == Some(&id)).unwrap_or(false);
            while let Ok(first) = rx.recv() {
                let mut batch = vec![first];
                batch.extend(rx.try_iter());
                let mut todo: Vec<(u64, u64, u64, SearchQuery)> = Vec::new();
                for q in batch {
                    match q {
                        Query::SetIndex { generation, index, db_path } => {
                            let reuse = matches!(&state, Some((_, _, _, p)) if *p == db_path);
                            if reuse {
                                if let Some(s) = state.as_mut() {
                                    s.0 = generation;
                                    s.1 = index;
                                }
                            } else {
                                state = Db::open_reader(&db_path).ok().map(|db| (generation, index, db, db_path));
                            }
                        }
                        Query::Search { pane, id, generation, query } => {
                            todo.retain(|t| t.0 != pane);
                            todo.push((pane, id, generation, query));
                        }
                        _ => {}
                    }
                }
                for (pane, id, generation, query) in todo {
                    if !is_latest(pane, id) {
                        continue;
                    }
                    let Some((g, ix, db, _)) = &state else { continue };
                    if *g != generation {
                        continue;
                    }
                    let t = std::time::Instant::now();
                    let keep_going = || is_latest(pane, id);
                    let result = search(db, ix, &query, MAX_HITS, &keep_going).map(Arc::new).map_err(|e| e.to_string());
                    if !keep_going() {
                        continue;
                    }
                    let elapsed_ms = t.elapsed().as_millis() as u64;
                    let _ = rtx.send(Reply::Search { pane, id, generation, result, elapsed_ms });
                    repaint();
                }
            }
        })
        .expect("spawn search thread");
    tx
}

/// Order a batch of queries: index changes first, then what the user is looking at right now
/// (inspector, groups, reports), then file listings. Only the newest inspector request counts.
fn prioritise(batch: Vec<Query>) -> Vec<Query> {
    let last_inspect = batch.iter().rposition(|q| matches!(q, Query::Inspect { .. }));
    let last_marks = batch.iter().rposition(|q| matches!(q, Query::Marks { .. }));
    let last_save = batch.iter().rposition(|q| matches!(q, Query::SaveMarks(_)));
    // a tab being resized asks again every frame: only its newest layout is wanted
    let newer_treemap = |i: usize, pane: u64, batch: &[Query]| {
        batch[i + 1..].iter().any(|q| matches!(q, Query::Treemap(r) if r.key.pane == pane))
    };
    let stale: Vec<bool> = (0..batch.len())
        .map(|i| match &batch[i] {
            Query::Treemap(r) => newer_treemap(i, r.key.pane, &batch),
            _ => false,
        })
        .collect();
    let (mut first, mut second, mut third) = (Vec::new(), Vec::new(), Vec::new());
    for (i, q) in batch.into_iter().enumerate() {
        match q {
            Query::SetIndex { .. } => first.push(q),
            Query::Inspect { .. } if Some(i) != last_inspect => {}
            Query::Marks { .. } if Some(i) != last_marks => {}
            Query::SaveMarks(_) if Some(i) != last_save => {}
            Query::Treemap(_) if stale[i] => {}
            Query::Files { .. } => third.push(q),
            _ => second.push(q),
        }
    }
    first.extend(second);
    first.extend(third);
    first
}

/// Plan and write the delete scripts of an export request.
fn export(ix: &Index, db: &Db, req: &ExportRequest) -> Result<ExportDone, Error> {
    let mut plans = dupemap_core::export::plan(ix, db, &req.state, req.options.include_last_copies)?;
    plans.retain(|p| req.sources.contains(&p.source));
    let mut left_out = MarkTotals::default();
    let mut unscriptable = 0;
    for p in &plans {
        left_out.files += p.left_out.files;
        left_out.bytes += p.left_out.bytes;
        unscriptable += p.unscriptable.len();
    }
    let made = dupemap_core::export::utc_text(std::time::SystemTime::now());
    let written = dupemap_core::export::write(&plans, &req.options, &req.dir, &made)?;
    Ok(ExportDone { dir: req.dir.clone(), written, left_out, unscriptable })
}

fn inspect(ix: &Index, db: &Db, target: Target) -> Result<Inspection, Error> {
    match target {
        Target::File { file, content } => {
            let rec = db.file_record(ix.file_db_id(file))?;
            let mut locations = ix.locations(db, content)?;
            locations.sort_by(|a, b| a.source.cmp(&b.source).then_with(|| natural_cmp(&a.path, &b.path)));
            let on_this_pc = locations
                .iter()
                .map(|l| {
                    cfg!(windows) && crate::actions::is_this_computer(&l.host) && std::path::Path::new(&l.path).exists()
                })
                .collect();
            Ok(Inspection::File(FileInfo {
                file,
                name: rec.name,
                size: ix.file_size(file),
                size_state: ix.size_state(file),
                algo: rec.digest.algo_name().to_string(),
                hash: rec.digest.display(),
                flagged: !rec.digest.is_sha256(),
                copies: ix.copies(content),
                other_sources: ix.copies_other_sources(file),
                locations,
                on_this_pc,
            }))
        }
        Target::Dir(dir) => Ok(Inspection::Dir(DirInfo {
            dir,
            path: ix.dir_path(dir),
            stats: ix.rollup(&Filter::default()).stats[dir as usize],
            outside: ix.copies_outside(dir),
        })),
    }
}

/// A file to import, and how (`None`: a manifest, or a checksum list imported before under
/// this file name with the same settings; a new checksum list is handed back to ask).
#[derive(Debug, Clone)]
pub struct ImportItem {
    pub path: PathBuf,
    pub how: Option<ImportAs>,
}

impl ImportItem {
    pub fn decide(path: PathBuf) -> ImportItem {
        ImportItem { path, how: None }
    }
}

/// A checksum list that needs its source before it can be imported.
#[derive(Debug, Clone)]
pub struct ChecksumAsk {
    pub path: PathBuf,
    pub guess: ChecksumImport,
    /// The first few paths in it.
    pub sample: Vec<String>,
}

/// One drive of a scan.
#[derive(Debug, Clone)]
pub struct ScanTarget {
    pub volume: Volume,
    /// Continue an interrupted scan of it (else start over).
    pub resume: bool,
}

/// Where a scan is, for the scan window.
#[derive(Debug, Clone, Default)]
pub struct ScanStatus {
    /// 0-based index of the drive being scanned, and how many there are.
    pub volume: usize,
    pub volumes: usize,
    pub name: String,
    pub progress: ScanProgress,
    /// The scanned manifests are being imported.
    pub importing: bool,
}

/// The result of one drive's scan.
#[derive(Debug, Clone)]
pub struct ScanDone {
    pub name: String,
    pub result: Result<VolumeResult, String>,
}

/// A long operation.
#[derive(Debug, Clone)]
pub enum Job {
    /// Open the database and build the index.
    Open,
    Import(Vec<ImportItem>),
    /// Hash drives of this computer into manifests, then import them.
    Scan {
        targets: Vec<ScanTarget>,
        settings: ScanSettings,
        out_dir: PathBuf,
    },
    Remove(Vec<i64>),
    /// Store new ignore rules and rebuild.
    SetIgnore(IgnoreSettings),
    /// Generate synthetic manifests into a folder and import them.
    Demo {
        rows: u64,
        folder: PathBuf,
    },
}

#[derive(Debug, Clone, Default)]
pub struct JobProgress {
    pub title: String,
    pub step: String,
    /// 0..=1, or negative for "unknown".
    pub fraction: f32,
    /// Details of a running scan.
    pub scan: Option<ScanStatus>,
}

#[derive(Default)]
pub struct JobOutcome {
    pub index: Option<Arc<Index>>,
    pub manifests: Vec<ManifestInfo>,
    /// The file-type categories stored in the database.
    pub categories: Option<Categories>,
    /// The marks for deletion stored in the database.
    pub marks: Option<Vec<MarkRule>>,
    /// Sources of manifests whose alerts were not dismissed.
    pub alert_hosts: BTreeSet<String>,
    pub reports: Vec<ImportReport>,
    pub notes: Vec<String>,
    pub error: Option<String>,
    pub cancelled: bool,
    /// Checksum lists that need their source asked before they can be imported.
    pub ask_checksums: Vec<ChecksumAsk>,
    /// Per drive, when the job was a scan.
    pub scan: Vec<ScanDone>,
    /// The job was a scan.
    pub was_scan: bool,
}

pub struct RunningJob {
    progress: Arc<Mutex<JobProgress>>,
    cancel: Arc<AtomicBool>,
    rx: Receiver<JobOutcome>,
}

impl RunningJob {
    pub fn progress(&self) -> JobProgress {
        self.progress.lock().map(|p| p.clone()).unwrap_or_default()
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelling(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    pub fn poll(&self) -> Option<JobOutcome> {
        self.rx.try_recv().ok()
    }
}

pub fn start(db_path: PathBuf, job: Job, repaint: Repaint) -> RunningJob {
    let title = match &job {
        Job::Open => "Opening database",
        Job::Import(_) => "Importing manifests",
        Job::Scan { .. } => "Scanning this computer",
        Job::Remove(_) => "Removing manifests",
        Job::SetIgnore(_) => "Applying ignore rules",
        Job::Demo { .. } => "Creating demo data",
    }
    .to_string();
    let progress =
        Arc::new(Mutex::new(JobProgress { title: title.clone(), step: String::new(), fraction: -1.0, scan: None }));
    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, rx) = channel();
    let (p2, c2) = (progress.clone(), cancel.clone());
    std::thread::Builder::new()
        .name("dupemap-job".into())
        .spawn(move || {
            let out = run(&db_path, job, &p2, &c2, &repaint);
            let _ = tx.send(out);
            repaint();
        })
        .expect("spawn job thread");
    RunningJob { progress, cancel, rx }
}

fn set(p: &Mutex<JobProgress>, step: impl Into<String>, fraction: f32, repaint: &Repaint) {
    if let Ok(mut g) = p.lock() {
        g.step = step.into();
        g.fraction = fraction;
    }
    repaint();
}

fn run(db_path: &Path, job: Job, p: &Mutex<JobProgress>, cancel: &AtomicBool, repaint: &Repaint) -> JobOutcome {
    let mut out = JobOutcome::default();
    if let Some(dir) = db_path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut db = match Db::open(db_path) {
        Ok(db) => db,
        Err(e) => {
            out.error = Some(format!("Could not open {}: {e}", db_path.display()));
            return out;
        }
    };
    // whether the index has to be rebuilt (an import of nothing leaves it as it is)
    let mut changed = true;
    let result: Result<(), Error> = (|| {
        match job {
            Job::Open => {}
            Job::Import(items) => {
                let mut ready = Vec::new();
                for (path, how) in expand_items(items, &mut out.notes) {
                    match how {
                        Some(h) => ready.push((path, h)),
                        // a checksum list: as imported before under this name, else ask
                        None => match earlier_checksum_settings(&db, &path) {
                            Some(c) => ready.push((path, ImportAs::Checksums(c))),
                            None => out.ask_checksums.push(checksum_ask(&path)),
                        },
                    }
                }
                let n = import(&mut db, ready, p, cancel, repaint, &mut out)?;
                changed = n > 0;
            }
            Job::Scan { targets, settings, out_dir } => {
                out.was_scan = true;
                let manifests = scan(&targets, &settings, &out_dir, p, cancel, repaint, &mut out);
                // what was scanned completely is imported, even after a cancel
                let keep_going = AtomicBool::new(false);
                if let Ok(mut g) = p.lock() {
                    if let Some(s) = g.scan.as_mut() {
                        s.importing = true;
                    }
                }
                let items = manifests.into_iter().map(|m| (m, ImportAs::Manifest)).collect();
                let n = import(&mut db, items, p, &keep_going, repaint, &mut out)?;
                changed = n > 0;
            }
            Job::Remove(ids) => {
                let names: std::collections::HashMap<i64, String> =
                    db.manifests()?.into_iter().map(|m| (m.id, m.file_name)).collect();
                for (i, id) in ids.iter().enumerate() {
                    let name = names.get(id).cloned().unwrap_or_else(|| format!("manifest {id}"));
                    set(p, format!("Removing {name}"), i as f32 / ids.len() as f32, repaint);
                    let s = db.remove_manifest(*id)?;
                    out.notes.push(format!(
                        "Removed {name} ({}){}{}",
                        crate::format::plural(s.rows_removed, "row", "rows"),
                        if s.rows_restored > 0 {
                            format!(
                                "; {} it was hiding are shown again",
                                crate::format::plural(s.rows_restored, "listing", "listings")
                            )
                        } else {
                            String::new()
                        },
                        if s.sources_removed.is_empty() {
                            String::new()
                        } else {
                            format!("; sources gone: {}", s.sources_removed.join(", "))
                        }
                    ));
                }
            }
            Job::SetIgnore(settings) => db.set_ignore_settings(&settings)?,
            Job::Demo { rows, folder } => {
                set(p, format!("Generating about {} rows of demo manifests", rows), -1.0, repaint);
                std::fs::create_dir_all(&folder)?;
                let cfg = dupemap_core::synth::Config { rows, seed: 42, bad_lines: true };
                dupemap_core::synth::generate(Some(&folder), &cfg)?;
                let mut paths: Vec<PathBuf> = std::fs::read_dir(&folder)?
                    .filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| p.extension().is_some_and(|x| x == "tsv"))
                    .collect();
                paths.sort();
                let items = paths.into_iter().map(|m| (m, ImportAs::Manifest)).collect();
                import(&mut db, items, p, cancel, repaint, &mut out)?;
            }
        }
        Ok(())
    })();
    match result {
        Ok(()) => {}
        Err(Error::Cancelled) => out.cancelled = true,
        Err(e) => out.error = Some(e.to_string()),
    }
    out.manifests = db.manifests().unwrap_or_default();
    out.alert_hosts = alert_hosts(&db, &out.manifests);
    out.categories = db.categories().ok();
    out.marks = db.marks().ok();
    if !changed {
        return out;
    }
    // end with an index of whatever the database now holds
    set(p, "Building index", 0.0, repaint);
    // the build reads the whole file table once: a large cache would only stay behind as memory
    let _ = db.set_cache_kib(dupemap_core::db::READER_CACHE_KIB);
    match Index::build_with_progress(&db, &mut |stage, f| set(p, format!("Building index: {stage}"), f, repaint)) {
        Ok(ix) => {
            // folder statistics for the default view, computed here rather than on the UI thread
            set(p, "Building index: folder totals", 1.0, repaint);
            ix.rollup(&dupemap_core::index::Filter::default());
            out.index = Some(Arc::new(ix));
        }
        Err(e) => {
            out.error = Some(out.error.take().map(|x| format!("{x}; ")).unwrap_or_default() + &format!("index: {e}"))
        }
    }
    out
}

/// Import files; returns how many were imported.
fn import(
    db: &mut Db,
    mut items: Vec<(PathBuf, ImportAs)>,
    p: &Mutex<JobProgress>,
    cancel: &AtomicBool,
    repaint: &Repaint,
    out: &mut JobOutcome,
) -> Result<usize, Error> {
    items.sort_by_key(|(x, _)| x.file_name().map(|n| n.to_os_string()));
    items.dedup_by(|a, b| a.0 == b.0);
    let total = items.len().max(1);
    let mut imported = 0;
    for (i, (path, how)) in items.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if let Some(old) = db.manifest_by_name(&name)? {
            set(p, format!("Replacing earlier import of {name}"), i as f32 / total as f32, repaint);
            db.remove_manifest(old.id)?;
            out.notes.push(format!("{name}: replaced the earlier import"));
            imported += 1;
        }
        let mut last = std::time::Instant::now();
        let mut cb = |pr: ImportProgress| -> bool {
            if last.elapsed().as_millis() > 100 {
                last = std::time::Instant::now();
                let f = if pr.bytes_total > 0 { pr.bytes_done as f32 / pr.bytes_total as f32 } else { 0.0 };
                set(
                    p,
                    format!("{name} ({} of {}) · {} rows", i + 1, items.len(), crate::format::count(pr.rows)),
                    (i as f32 + f.min(1.0)) / total as f32,
                    repaint,
                );
            }
            !cancel.load(Ordering::Relaxed)
        };
        match db.import_file_as(path, how, &mut cb) {
            Ok(r) => {
                out.reports.push(r);
                imported += 1;
            }
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            Err(e) => out.notes.push(format!("{name}: import failed: {e}")),
        }
    }
    Ok(imported)
}

/// Decide how each file is imported: given, or (`None` in the result) a checksum list whose
/// settings still have to be found. Folders become the manifests in them.
fn expand_items(items: Vec<ImportItem>, notes: &mut Vec<String>) -> Vec<(PathBuf, Option<ImportAs>)> {
    let mut out = Vec::new();
    for it in items {
        if let Some(how) = it.how {
            out.push((it.path, Some(how)));
            continue;
        }
        for path in expand_paths(std::slice::from_ref(&it.path), notes) {
            match std::fs::File::open(&path).and_then(sniff_format).unwrap_or(ListFormat::Manifest) {
                ListFormat::Manifest => out.push((path, Some(ImportAs::Manifest))),
                ListFormat::Checksums => out.push((path, None)),
            }
        }
    }
    out
}

/// A checksum list's settings when it was imported before under the same file name.
fn earlier_checksum_settings(db: &Db, path: &Path) -> Option<ChecksumImport> {
    let name = path.file_name()?.to_string_lossy().into_owned();
    let old = db.manifest_by_name(&name).ok()??;
    db.report(old.id).ok()?.checksums
}

/// What to ask about a new checksum list: a guess at its source and a few of its paths.
pub fn checksum_ask(path: &Path) -> ChecksumAsk {
    let first = std::fs::File::open(path).and_then(|r| first_checksum_paths(r, 20)).unwrap_or_default();
    let refs: Vec<&[u8]> = first.iter().map(|v| v.as_slice()).collect();
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut guess = guess_checksums(&name, &refs);
    guess.sizes = find_size_list(path);
    let sample = first.iter().take(5).map(|p| String::from_utf8_lossy(p).into_owned()).collect();
    ChecksumAsk { path: path.to_path_buf(), guess, sample }
}

/// Scan drives one after the other; returns the manifests that were completed.
fn scan(
    targets: &[ScanTarget],
    settings: &ScanSettings,
    out_dir: &Path,
    p: &Mutex<JobProgress>,
    cancel: &AtomicBool,
    repaint: &Repaint,
    out: &mut JobOutcome,
) -> Vec<PathBuf> {
    let mut done = Vec::new();
    let n = targets.len();
    for (i, t) in targets.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let name = t.volume.display.clone();
        let mut last = std::time::Instant::now() - std::time::Duration::from_secs(1);
        let mut report = |pr: &ScanProgress| {
            if last.elapsed().as_millis() < 100 && pr.phase != dupemap_core::scan::Phase::Done {
                return;
            }
            last = std::time::Instant::now();
            let within = if pr.bytes_total > 0 { pr.bytes_done as f32 / pr.bytes_total as f32 } else { 0.0 };
            let (step, fraction) = match pr.phase {
                dupemap_core::scan::Phase::Counting => (
                    format!(
                        "{name} ({} of {n}): counting · {} files, {}",
                        i + 1,
                        crate::format::count(pr.files_total),
                        crate::format::bytes(pr.bytes_total)
                    ),
                    -1.0,
                ),
                _ => (
                    format!(
                        "{name} ({} of {n}): {} of {} · {}/s",
                        i + 1,
                        crate::format::bytes(pr.bytes_done),
                        crate::format::bytes(pr.bytes_total),
                        crate::format::bytes(pr.rate() as u64)
                    ),
                    (i as f32 + within.min(1.0)) / n as f32,
                ),
            };
            if let Ok(mut g) = p.lock() {
                g.step = step;
                g.fraction = fraction;
                g.scan = Some(ScanStatus {
                    volume: i,
                    volumes: n,
                    name: name.clone(),
                    progress: pr.clone(),
                    importing: false,
                });
            }
            repaint();
        };
        let r = scan_volume(&t.volume, out_dir, settings, t.resume, &mut report, cancel);
        match &r {
            Ok(v) if v.complete => {
                done.push(v.manifest.clone());
                let pr = &v.progress;
                let mut msg = format!(
                    "Scanned {name}: {} files, {}",
                    crate::format::count(pr.files_done),
                    crate::format::bytes(pr.bytes_done)
                );
                if pr.failed > 0 {
                    msg.push_str(&format!(" · {} could not be read", crate::format::count(pr.failed)));
                }
                if pr.placeholders_skipped > 0 {
                    msg.push_str(&format!(
                        " · {} online-only cloud files skipped",
                        crate::format::count(pr.placeholders_skipped)
                    ));
                }
                out.notes.push(msg);
            }
            Ok(_) => out.notes.push(format!("Scan of {name} stopped; it continues where it stopped next time")),
            Err(e) => out.notes.push(format!("Could not scan {name}: {e}")),
        }
        out.scan.push(ScanDone { name, result: r.map_err(|e| e.to_string()) });
    }
    if cancel.load(Ordering::Relaxed) {
        out.cancelled = true;
    }
    done
}

/// Hosts named in the reports of manifests with an undismissed alert.
pub fn alert_hosts(db: &Db, manifests: &[ManifestInfo]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for m in manifests.iter().filter(|m| m.severity == Severity::Alert && !m.alert_dismissed) {
        if let Ok(r) = db.report(m.id) {
            out.extend(r.hosts.keys().cloned());
        }
    }
    out
}

/// Files to import: plain files as given; for a folder, its `manifest-*.tsv` files (or every
/// `.tsv` file when it has none).
pub fn expand_paths(paths: &[PathBuf], notes: &mut Vec<String>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for p in paths {
        if p.is_dir() {
            let mut tsv: Vec<PathBuf> = std::fs::read_dir(p)
                .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).collect())
                .unwrap_or_default();
            tsv.retain(|f| f.is_file() && f.extension().is_some_and(|x| x.eq_ignore_ascii_case("tsv")));
            let manifests: Vec<PathBuf> = tsv
                .iter()
                .filter(|f| f.file_name().is_some_and(|n| n.to_string_lossy().to_lowercase().starts_with("manifest-")))
                .cloned()
                .collect();
            let chosen = if manifests.is_empty() { tsv } else { manifests };
            if chosen.is_empty() {
                notes.push(format!("{}: no .tsv files in this folder", p.display()));
            }
            out.extend(chosen);
        } else {
            out.push(p.clone());
        }
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_are_prioritised() {
        let files = |g| Query::Files { generation: g, dir: 0, opts: ListOptions::default() };
        let inspect = |d| Query::Inspect { generation: 1, target: Target::Dir(d) };
        let batch = vec![files(1), inspect(5), files(1), Query::Report { manifest_id: 3 }, inspect(9), files(1)];
        let order: Vec<String> = prioritise(batch)
            .iter()
            .map(|q| match q {
                Query::Files { .. } => "files".to_string(),
                Query::Inspect { target: Target::Dir(d), .. } => format!("inspect {d}"),
                Query::Report { .. } => "report".to_string(),
                _ => "other".to_string(),
            })
            .collect();
        assert_eq!(order, ["report", "inspect 9", "files", "files", "files"], "superseded inspector request dropped");
    }

    #[test]
    fn folders_expand_to_their_manifests() {
        let dir = std::env::temp_dir().join(format!("dupemap-expand-{}", std::process::id()));
        let sub = dir.join("only-tsv");
        std::fs::create_dir_all(&sub).unwrap();
        for f in ["manifest-a.tsv", "Manifest-B.TSV", "notes.tsv", "readme.txt"] {
            std::fs::write(dir.join(f), "").unwrap();
        }
        std::fs::write(sub.join("hashes.tsv"), "").unwrap();
        let mut notes = Vec::new();
        let got = expand_paths(&[dir.clone(), sub.clone(), dir.join("readme.txt")], &mut notes);
        let names: Vec<String> = got.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["Manifest-B.TSV", "manifest-a.tsv", "hashes.tsv", "readme.txt"]);
        let empty = dir.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(expand_paths(&[empty], &mut notes).is_empty());
        assert_eq!(notes.len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
