//! Background threads.
//!
//! * The **query worker** answers file listings (they need names from SQLite, which takes a
//!   moment for very large folders) so the UI thread never waits on the database.
//! * A **job** runs one long operation (open, import, remove, change ignore rules, demo data)
//!   with progress and cancellation, and ends by building a fresh index.

use dupemap_core::db::{ManifestInfo, Progress as ImportProgress};
use dupemap_core::ignore::IgnoreSettings;
use dupemap_core::index::{DirIx, FileRow, Index, ListOptions, Node};
use dupemap_core::{Db, Error, ImportReport, Severity};
use std::collections::BTreeSet;
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
}

pub enum Reply {
    Files { generation: u64, dir: DirIx, opts: ListOptions, files: Result<Arc<Vec<FileRow>>, String> },
    Report(Result<Box<ImportReport>, String>),
    Manifests { manifests: Vec<ManifestInfo>, alert_hosts: BTreeSet<String> },
}

pub struct Worker {
    tx: Sender<Query>,
    rx: Receiver<Reply>,
}

impl Worker {
    pub fn spawn(repaint: Repaint) -> Worker {
        let (qtx, qrx) = channel::<Query>();
        let (rtx, rrx) = channel::<Reply>();
        std::thread::Builder::new()
            .name("dupemap-query".into())
            .spawn(move || {
                let mut state: Option<(u64, Arc<Index>, Db, PathBuf)> = None;
                while let Ok(first) = qrx.recv() {
                    let mut batch = vec![first];
                    batch.extend(qrx.try_iter());
                    // the newest index wins; queries for older generations are dropped
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
                                    match Db::open(&db_path) {
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
        Worker { tx: qtx, rx: rrx }
    }

    pub fn send(&self, q: Query) {
        let _ = self.tx.send(q);
    }

    pub fn poll(&self) -> Vec<Reply> {
        self.rx.try_iter().collect()
    }
}

/// A long operation.
#[derive(Debug, Clone)]
pub enum Job {
    /// Open the database and build the index.
    Open,
    Import(Vec<PathBuf>),
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
}

#[derive(Default)]
pub struct JobOutcome {
    pub index: Option<Arc<Index>>,
    pub manifests: Vec<ManifestInfo>,
    /// Sources of manifests whose alerts were not dismissed.
    pub alert_hosts: BTreeSet<String>,
    pub reports: Vec<ImportReport>,
    pub notes: Vec<String>,
    pub error: Option<String>,
    pub cancelled: bool,
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
        Job::Remove(_) => "Removing manifests",
        Job::SetIgnore(_) => "Applying ignore rules",
        Job::Demo { .. } => "Creating demo data",
    }
    .to_string();
    let progress = Arc::new(Mutex::new(JobProgress { title: title.clone(), step: String::new(), fraction: -1.0 }));
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
    let result: Result<(), Error> = (|| {
        match job {
            Job::Open => {}
            Job::Import(paths) => {
                let files = expand_paths(&paths, &mut out.notes);
                import(&mut db, files, p, cancel, repaint, &mut out)?
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
                import(&mut db, paths, p, cancel, repaint, &mut out)?;
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
    // always end with an index of whatever the database now holds
    set(p, "Building index", 0.0, repaint);
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

fn import(
    db: &mut Db,
    mut paths: Vec<PathBuf>,
    p: &Mutex<JobProgress>,
    cancel: &AtomicBool,
    repaint: &Repaint,
    out: &mut JobOutcome,
) -> Result<(), Error> {
    paths.sort_by_key(|x| x.file_name().map(|n| n.to_os_string()));
    paths.dedup();
    let total = paths.len().max(1);
    for (i, path) in paths.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if let Some(old) = db.manifest_by_name(&name)? {
            set(p, format!("Replacing earlier import of {name}"), i as f32 / total as f32, repaint);
            db.remove_manifest(old.id)?;
            out.notes.push(format!("{name}: replaced the earlier import"));
        }
        let mut last = std::time::Instant::now();
        let mut cb = |pr: ImportProgress| -> bool {
            if last.elapsed().as_millis() > 100 {
                last = std::time::Instant::now();
                let f = if pr.bytes_total > 0 { pr.bytes_done as f32 / pr.bytes_total as f32 } else { 0.0 };
                set(
                    p,
                    format!("{name} ({} of {}) · {} rows", i + 1, paths.len(), crate::format::count(pr.rows)),
                    (i as f32 + f.min(1.0)) / total as f32,
                    repaint,
                );
            }
            !cancel.load(Ordering::Relaxed)
        };
        match db.import_file(path, &mut cb) {
            Ok(r) => out.reports.push(r),
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            Err(e) => out.notes.push(format!("{name}: import failed: {e}")),
        }
    }
    Ok(())
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
