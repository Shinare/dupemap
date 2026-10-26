//! Generate synthetic manifests, import them from disk and compare every import report with
//! what the generator says it wrote.

use dupemap_core::synth::{generate, Config};
use dupemap_core::{Db, Severity};
use std::path::PathBuf;

fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("dupemap-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn reports_match_generator_expectations() {
    let dir = temp_dir("genimport");
    let exp = generate(Some(&dir), &Config { rows: 40_000, seed: 3, bad_lines: true }).unwrap();
    let mut db = Db::open(&dir.join("test.dupemap")).unwrap();

    let mut names: Vec<String> = exp.manifests.iter().map(|m| m.file_name.clone()).collect();
    names.sort();
    let mut reports = Vec::new();
    for n in &names {
        let r = db.import_file(&dir.join(n), &mut |_| true).unwrap();
        reports.push(r);
    }

    for m in &exp.manifests {
        let r = reports.iter().find(|r| r.file_name == m.file_name).unwrap();
        assert!(r.header_found, "{}", m.file_name);
        assert_eq!(r.rows_imported, m.rows, "{} rows", m.file_name);
        assert_eq!(r.flagged, m.flagged, "{} flagged", m.file_name);
        assert_eq!(r.malformed, m.malformed, "{} malformed", m.file_name);
        assert_eq!(r.duplicate_listings, m.duplicate_listings, "{} duplicate listings", m.file_name);
        assert_eq!(r.same_path_cloud, m.same_path_cloud, "{} same path", m.file_name);
        assert_eq!(r.non_utf8_paths, m.non_utf8_paths, "{} non-utf8", m.file_name);
        assert_eq!(r.hosts, m.hosts, "{} hosts", m.file_name);
        assert_eq!(r.line_endings, if m.crlf { "CRLF" } else { "LF" }, "{}", m.file_name);
        assert_eq!(r.encoding, if m.bom { "UTF-8 with BOM" } else { "UTF-8" }, "{}", m.file_name);
        let should_alert = !m.flagged.is_empty() || !m.malformed.is_empty();
        assert_eq!(r.severity() == Severity::Alert, should_alert, "{} severity {:?}", m.file_name, r.severity());
        assert!(r.mixed_kinds.is_empty(), "{}: {:?}", m.file_name, r.mixed_kinds);
    }

    for o in &exp.overlaps {
        let r = reports.iter().find(|r| r.file_name == o.newer).unwrap();
        let found = r
            .overlaps
            .iter()
            .find(|x| x.file_name == o.older)
            .unwrap_or_else(|| panic!("no overlap {} vs {}", o.newer, o.older));
        assert_eq!(found.paths, o.paths, "{} overlap paths", o.newer);
        assert_eq!(found.changed, o.changed, "{} overlap changed", o.newer);
        assert_eq!(found.this_shown + found.other_shown, found.paths);
        // no other manifest pair should overlap
        assert_eq!(r.overlaps.len(), 1, "{}: {:?}", o.newer, r.overlaps);
    }
    // the SHA-256 OneDrive listing must win over the QuickXorHash one regardless of order
    let qx = reports.iter().find(|r| r.file_name == "manifest-cloud-onedrive.tsv").unwrap();
    assert_eq!(qx.overlaps[0].other_shown, qx.overlaps[0].paths);

    // every path is visible exactly once (cloud same-path rows excepted)
    let c = db.counts().unwrap();
    let total_rows: u64 = exp.manifests.iter().map(|m| m.rows).sum();
    assert_eq!(c.files_visible + c.files_hidden, total_rows);
    let dup_hidden: u64 = exp.manifests.iter().map(|m| m.duplicate_listings).sum();
    let overlap_hidden: u64 = exp.overlaps.iter().map(|o| o.paths).sum();
    assert_eq!(c.files_hidden, dup_hidden + overlap_hidden);

    // removing every manifest leaves an empty database
    for m in db.manifests().unwrap() {
        db.remove_manifest(m.id).unwrap();
    }
    let c = db.counts().unwrap();
    assert_eq!((c.sources, c.dirs, c.files_visible, c.files_hidden, c.manifests), (0, 0, 0, 0, 0));
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
