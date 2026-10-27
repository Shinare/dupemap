//! Optional check against real manifests, which are never committed.
//!
//!   DUPEMAP_REAL_MANIFESTS=C:\Hashes cargo test --release --test real_manifests -- --nocapture
//!
//! For every `manifest-*.tsv` in the folder: the import must have no malformed lines, every
//! stored file must reproduce its original path exactly, and copy counts must equal a plain
//! group-by of (hash, size) over the raw rows. Without the variable the test does nothing.

use dupemap_core::ignore::IgnoreSettings;
use dupemap_core::index::{FileIx, Index};
use dupemap_core::manifest::{is_header, parse_row, LineReader};
use dupemap_core::paths::parse_path;
use dupemap_core::Db;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

#[test]
fn real_manifests_round_trip() {
    let Ok(dir) = std::env::var("DUPEMAP_REAL_MANIFESTS") else {
        eprintln!("DUPEMAP_REAL_MANIFESTS not set; skipping");
        return;
    };
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            let n = p.file_name().unwrap().to_string_lossy().to_lowercase();
            n.starts_with("manifest-") && n.ends_with(".tsv")
        })
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no manifest-*.tsv in {dir}");

    let mut db = Db::open_in_memory().unwrap();
    // raw rows: (host, canonical path) -> (hash text, size)
    let mut raw: HashMap<(String, String), (String, u64)> = HashMap::new();
    for f in &files {
        let report = db.import_file(f, &mut |_| true).unwrap();
        eprintln!(
            "{}: {} rows, flagged {:?}, malformed {:?}, findings {:?}",
            report.file_name,
            report.rows_imported,
            report.flagged,
            report.malformed,
            report.findings()
        );
        assert_eq!(report.malformed_total(), 0, "{} has malformed lines", report.file_name);
        let (mut lines, _) = LineReader::new(std::fs::File::open(f).unwrap()).unwrap();
        while let Some((_, line)) = lines.next_line().unwrap() {
            if is_header(line) || line.trim_ascii().is_empty() {
                continue;
            }
            let row = parse_row(line).unwrap();
            let canonical = parse_path(row.path).display();
            raw.insert((row.host.to_string(), canonical), (row.digest.display(), row.size.unwrap_or(0)));
        }
    }

    let ix = Index::build_with(&db, &IgnoreSettings::disabled(), &mut |_, _| {}).unwrap();
    let with_defaults = Index::build(&db).unwrap();
    let hidden = with_defaults.ignored_total();
    eprintln!("default ignore rules hide {} folders, {} files", hidden.folders, hidden.files);
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut by_content: HashMap<u32, (String, u64)> = HashMap::new();
    for f in 0..ix.file_count() as FileIx {
        let host = ix.sources()[ix.file_source(f) as usize].host.clone();
        let path = ix.file_path(&db, f).unwrap();
        let key = (host, path);
        let (hash, size) = raw.get(&key).unwrap_or_else(|| panic!("index path not in any manifest: {key:?}"));
        assert_eq!(*size, ix.file_size(f));
        // all files of one content must share hash and size
        let entry = by_content.entry(ix.file_content(f)).or_insert_with(|| (hash.clone(), *size));
        assert_eq!(entry, &(hash.clone(), *size), "content mixes different hashes: {key:?}");
        assert!(seen.insert(key.clone()), "path listed twice in the index: {key:?}");
    }
    assert_eq!(seen.len(), raw.len(), "every distinct manifest path appears exactly once");

    let mut groups: HashMap<(String, u64), u32> = HashMap::new();
    for v in raw.values() {
        *groups.entry(v.clone()).or_insert(0) += 1;
    }
    for f in 0..ix.file_count() as FileIx {
        let c = ix.file_content(f);
        assert_eq!(ix.copies(c), groups[&by_content[&c]], "copy count");
    }
    let dup_groups = groups.values().filter(|&&n| n >= 2).count();
    assert_eq!(ix.dup_group_count(), dup_groups);
    eprintln!("checked {} files, {} duplicate groups", ix.file_count(), dup_groups);
}
