//! dupemap-bench: end-to-end timings on synthetic data.
//!
//!   dupemap-bench [--rows 1000000] [--seed 42] [--work DIR] [--keep]
//!
//! Generates manifests, imports them into a database file, builds the index and times what the
//! app does on start-up and on common interactions. Prints a Markdown table.

use dupemap_core::index::{CopiesFilter, DupQuery, Filter, Index, ListOptions, Node, SortKey, Spread};
use dupemap_core::synth::{generate, Config};
use dupemap_core::Db;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// (current, peak) resident memory in bytes. On Windows: working set and peak working set.
#[cfg(windows)]
fn rss() -> (Option<u64>, Option<u64>) {
    #[repr(C)]
    struct ProcessMemoryCounters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentProcess() -> *mut std::ffi::c_void;
        fn K32GetProcessMemoryInfo(
            process: *mut std::ffi::c_void,
            counters: *mut ProcessMemoryCounters,
            cb: u32,
        ) -> i32;
    }
    // SAFETY: plain Win32 calls with a correctly sized, zero-initialised output struct.
    unsafe {
        let mut c: ProcessMemoryCounters = std::mem::zeroed();
        c.cb = std::mem::size_of::<ProcessMemoryCounters>() as u32;
        if K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) == 0 {
            return (None, None);
        }
        (Some(c.working_set_size as u64), Some(c.peak_working_set_size as u64))
    }
}

/// (current, peak) resident memory in bytes, from /proc on Linux.
#[cfg(not(windows))]
fn rss() -> (Option<u64>, Option<u64>) {
    let Ok(s) = std::fs::read_to_string("/proc/self/status") else { return (None, None) };
    let field = |name: &str| -> Option<u64> {
        s.lines()
            .find(|l| l.starts_with(name))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
            .map(|kb| kb * 1024)
    };
    (field("VmRSS:"), field("VmHWM:"))
}

fn reset_peak() {
    let _ = std::fs::write("/proc/self/clear_refs", "5");
}

fn mb(b: Option<u64>) -> String {
    b.map(|v| format!("{:.0} MB", v as f64 / 1_048_576.0)).unwrap_or_else(|| "n/a".into())
}

fn dir_size(p: &Path) -> u64 {
    std::fs::read_dir(p)
        .map(|rd| {
            rd.filter_map(|e| e.ok()).filter_map(|e| e.metadata().ok()).filter(|m| m.is_file()).map(|m| m.len()).sum()
        })
        .unwrap_or(0)
}

struct Table(Vec<(String, String)>);

impl Table {
    fn add(&mut self, k: impl Into<String>, v: impl Into<String>) {
        let (k, v) = (k.into(), v.into());
        eprintln!("  {k}: {v}");
        self.0.push((k, v));
    }
    fn ms(&mut self, k: &str, t: Instant) -> f64 {
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        self.add(k, if ms >= 1000.0 { format!("{:.2} s", ms / 1000.0) } else { format!("{ms:.1} ms") });
        ms
    }
}

fn main() {
    let mut cfg = Config::default();
    let mut work: Option<PathBuf> = None;
    let mut keep = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--rows" => cfg.rows = args.next().and_then(|v| v.replace('_', "").parse().ok()).unwrap_or(cfg.rows),
            "--seed" => cfg.seed = args.next().and_then(|v| v.parse().ok()).unwrap_or(cfg.seed),
            "--work" => work = args.next().map(PathBuf::from),
            "--keep" => keep = true,
            _ => {
                eprintln!("usage: dupemap-bench [--rows N] [--seed S] [--work DIR] [--keep]");
                std::process::exit(2);
            }
        }
    }
    let work = work.unwrap_or_else(|| std::env::temp_dir().join(format!("dupemap-bench-{}", cfg.rows)));
    let data = work.join("manifests");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&data).unwrap();
    let mut t = Table(Vec::new());

    eprintln!("generating {} rows...", cfg.rows);
    let s = Instant::now();
    let exp = generate(Some(&data), &cfg).unwrap();
    t.ms("generate manifests", s);
    t.add("rows generated", exp.total_rows.to_string());
    t.add("manifest bytes", format!("{:.0} MB", dir_size(&data) as f64 / 1e6));

    let db_path = work.join("bench.dupemap");
    let mut db = Db::open(&db_path).unwrap();
    let mut names: Vec<String> = exp.manifests.iter().map(|m| m.file_name.clone()).collect();
    names.sort();
    reset_peak();
    let s = Instant::now();
    for n in &names {
        let si = Instant::now();
        let r = db.import_file(&data.join(n), &mut |_| true).unwrap();
        let secs = si.elapsed().as_secs_f64();
        eprintln!(
            "    {n}: {} rows in {secs:.2}s ({:.0} rows/s)",
            r.rows_imported,
            r.rows_imported as f64 / secs.max(1e-9)
        );
    }
    let import_ms = t.ms("import all manifests", s);
    t.add("import rate", format!("{:.0} rows/s", exp.total_rows as f64 / (import_ms / 1000.0)));
    t.add("peak RSS during import", mb(rss().1));
    let _ = db.conn().execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    t.add("database size", format!("{:.0} MB", dir_size(&work) as f64 / 1e6));

    // what the app does at start-up: open the database, build the index, list the first screen
    drop(db);
    reset_peak();
    let s = Instant::now();
    let db = Db::open(&db_path).unwrap();
    let mut stages: Vec<(String, Instant)> = Vec::new();
    let ix = Index::build_with_progress(&db, &mut |stage, _| {
        if stages.last().map(|(n, _)| n.as_str()) != Some(stage) {
            stages.push((stage.to_string(), Instant::now()));
        }
    })
    .unwrap();
    t.ms("open database + build index", s);
    let parts: Vec<String> =
        stages.windows(2).map(|w| format!("{} {:.0} ms", w[0].0, (w[1].1 - w[0].1).as_secs_f64() * 1000.0)).collect();
    t.add("  build stages", parts.join(", "));
    let (cur, peak) = rss();
    t.add(
        "index: files / folders / contents / dup groups",
        format!("{} / {} / {} / {}", ix.file_count(), ix.dir_count(), ix.content_count(), ix.dup_group_count()),
    );
    t.add("index memory (estimate)", format!("{:.0} MB", ix.approx_memory() as f64 / 1_048_576.0));
    t.add("RSS after index build / peak during build", format!("{} / {}", mb(cur), mb(peak)));

    let opts = ListOptions::default();
    let s = Instant::now();
    let root = ix.list(&db, Node::AllSources, &opts).unwrap();
    for d in &root.dirs {
        let _ = ix.list(&db, Node::Dir(d.dir), &opts).unwrap();
    }
    t.ms("first screen: all sources + each source root", s);

    // the folder with the most files directly inside it
    let big = (0..ix.dir_count() as u32).max_by_key(|&d| ix.dir_files(d).len()).unwrap();
    let n_big = ix.dir_files(big).len();
    let s = Instant::now();
    let l = ix.list(&db, Node::Dir(big), &opts).unwrap();
    t.ms(&format!("list largest folder ({n_big} files, by name)"), s);
    assert_eq!(l.files.len(), n_big);
    let s = Instant::now();
    let _ = ix
        .list(&db, Node::Dir(big), &ListOptions { sort: SortKey::Size, descending: true, ..Default::default() })
        .unwrap();
    t.ms("same folder sorted by size", s);

    let s = Instant::now();
    let _ = ix.rollup_uncached(&Filter::default());
    t.ms("folder stats, no filter (uncached)", s);
    let first_source = ix.sources()[0].db_id;
    for (label, f) in [
        ("unique only", Filter { copies: CopiesFilter::Unique, ..Default::default() }),
        ("not present in first source", Filter { absent_from: Some(first_source), ..Default::default() }),
    ] {
        let s = Instant::now();
        let _ = ix.rollup_uncached(&f);
        t.ms(&format!("folder stats, filter {label}"), s);
    }
    let f = Filter { copies: CopiesFilter::InOtherSources, ..Default::default() };
    let s = Instant::now();
    let _ = ix.rollup(&f);
    t.ms("new filter (has copies in other sources): all folder stats", s);
    let s = Instant::now();
    let _ = ix.list(&db, Node::AllSources, &ListOptions { filter: f.clone(), ..Default::default() }).unwrap();
    t.ms("list with cached filter", s);

    let s = Instant::now();
    let (total, groups) = ix.dup_groups(&DupQuery::default(), 0, 100);
    t.ms(&format!("duplicate groups: top 100 of {total}"), s);
    let s = Instant::now();
    let locs = ix.locations(&db, groups[0].content).unwrap();
    t.ms(&format!("inspector: locations of top group ({} copies)", locs.len()), s);
    let s = Instant::now();
    let hl = ix.highlight_dirs(groups[0].content);
    t.ms(&format!("highlight folders for a selection ({} folders)", hl.len()), s);
    let s = Instant::now();
    let (deep_total, deep) = ix.dup_groups(&DupQuery::default(), total.saturating_sub(200) / 2, 200);
    t.ms(&format!("duplicate groups: a page of 200 halfway down ({} rows)", deep.len()), s);
    assert_eq!(deep_total, total);
    let s = Instant::now();
    let across = ix.dup_totals(&DupQuery { spread: Spread::SeveralSources, ..Default::default() });
    t.ms(&format!("duplicate groups across sources: count and extra bytes ({} groups)", across.0), s);

    // the inspector's "where are the copies" for a folder; the biggest folders are the slowest
    let mut slowest = (0.0f64, 0u64, String::new());
    let mut biggest_dirs: Vec<u32> = ix.sources().iter().map(|s| s.root).collect();
    biggest_dirs.extend(ix.sources().iter().flat_map(|s| ix.dir_children(s.root).iter().copied()));
    for d in biggest_dirs {
        let s = Instant::now();
        let o = ix.copies_outside(d);
        let ms = s.elapsed().as_secs_f64() * 1000.0;
        if ms > slowest.0 {
            slowest = (ms, o.files, ix.dir_path(d));
        }
    }
    t.add(
        "inspector: where a folder's copies are, slowest source or drive",
        format!("{:.1} ms ({} files)", slowest.0, slowest.1),
    );

    // the summary tab: every source, then the biggest one
    let cats = dupemap_core::summary::Categories::default();
    let s = Instant::now();
    let all = ix.summary(None, &cats);
    t.ms(&format!("summary by file type, all sources ({} extensions)", all.exts.len()), s);
    if let Some(big) = ix.source_summaries().iter().max_by_key(|s| s.files) {
        let s = Instant::now();
        let _ = ix.summary(Some(big.source), &cats);
        t.ms(&format!("summary by file type, biggest source ({} files)", big.files), s);
    }

    drop(ix);
    drop(db);
    let mut db = Db::open(&db_path).unwrap();
    let usb = db.manifest_by_name("manifest-PC2-usb.tsv").unwrap().unwrap();
    let s = Instant::now();
    let rm = db.remove_manifest(usb.id).unwrap();
    t.ms(&format!("remove manifest-PC2-usb ({} rows, {} restored)", rm.rows_removed, rm.rows_restored), s);
    let s = Instant::now();
    db.import_file(&data.join("manifest-PC2-usb.tsv"), &mut |_| true).unwrap();
    t.ms("re-import it (overlaps manifest-PC2)", s);

    println!("\n| step | result |\n|---|---|");
    for (k, v) in &t.0 {
        println!("| {k} | {v} |");
    }
    if !keep {
        drop(db);
        let _ = std::fs::remove_dir_all(&work);
    } else {
        println!("\nkept {}", work.display());
    }
}
