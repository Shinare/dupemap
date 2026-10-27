//! dupemap-check: import real manifests and print what DupeMap sees, without the GUI.
//!
//!   dupemap-check [--db FILE] [--top N] [--source NAME] [--base PREFIX] [--sizes FILE] FILE|FOLDER|PATTERN...
//!
//! Checksum lists (`hash  path`, from sha256sum or rclone hashsum) are recognised by their
//! content; they are imported for the source given with --source (default: guessed from the
//! file name, e.g. `onedrive-hashsum.txt` → `onedrive:`), relative paths prefixed with --base.
//! A folder means every `manifest-*.tsv` inside it. Wildcards (`*`, `?`) are expanded here,
//! because Windows shells pass them through unexpanded.
//!
//! Every manifest is validated (non-SHA-256 hashes and malformed lines are reported with line
//! numbers), then a per-source summary and the largest duplicate groups are printed.
//! With --db the database is kept and re-running with the same manifest file name replaces it.

use dupemap_core::db::{ChecksumImport, ImportAs};
use dupemap_core::index::{DupQuery, Index};
use dupemap_core::manifest::{find_size_list, first_checksum_paths, guess_checksums, sniff_format, ListFormat};
use dupemap_core::{Db, Severity};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

fn human(n: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < units.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", units[u])
    }
}

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Case-insensitive wildcard match on a file name (`*` and `?`).
fn wild_match(pat: &str, name: &str) -> bool {
    let p: Vec<char> = pat.to_lowercase().chars().collect();
    let n: Vec<char> = name.to_lowercase().chars().collect();
    let (mut pi, mut ni, mut star, mut mark) = (0usize, 0usize, None::<usize>, 0usize);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Expand folders and wildcard patterns into manifest files.
fn expand(arg: &str) -> Vec<PathBuf> {
    let p = PathBuf::from(arg);
    let (dir, pat) = if p.is_dir() {
        (p, "manifest-*.tsv".to_string())
    } else if arg.contains('*') || arg.contains('?') {
        let dir =
            p.parent().filter(|d| !d.as_os_str().is_empty()).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
        let pat = p.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
        (dir, pat)
    } else {
        return vec![p];
    };
    let mut out: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().is_file() && wild_match(&pat, &e.file_name().to_string_lossy()))
                .map(|e| e.path())
                .collect()
        })
        .unwrap_or_default();
    if out.is_empty() {
        eprintln!("warning: nothing matches {arg}");
    }
    out.sort();
    out
}

fn main() -> ExitCode {
    let mut db_path: Option<PathBuf> = None;
    let mut top = 15usize;
    let (mut source, mut base, mut sizes): (Option<String>, Option<String>, Option<PathBuf>) = (None, None, None);
    let mut files: Vec<PathBuf> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--db" => db_path = args.next().map(PathBuf::from),
            "--top" => top = args.next().and_then(|v| v.parse().ok()).unwrap_or(top),
            "--source" => source = args.next(),
            "--base" => base = args.next(),
            "--sizes" => sizes = args.next().map(PathBuf::from),
            "-h" | "--help" => {
                println!(
                    "usage: dupemap-check [--db FILE] [--top N] [--source NAME] [--base PREFIX] [--sizes FILE] FILE|FOLDER|PATTERN..."
                );
                return ExitCode::SUCCESS;
            }
            _ => files.extend(expand(&a)),
        }
    }
    if files.is_empty() && db_path.is_none() {
        eprintln!("usage: dupemap-check [--db FILE] [--top N] MANIFEST|FOLDER|PATTERN...");
        return ExitCode::from(2);
    }
    let mut db = match &db_path {
        Some(p) => Db::open(p),
        None => Db::open_in_memory(),
    }
    .unwrap_or_else(|e| {
        eprintln!("cannot open database: {e}");
        std::process::exit(1)
    });

    let mut alerts = 0;
    files.sort_by_key(|f| f.file_name().map(|n| n.to_os_string()));
    files.dedup();
    for f in &files {
        let name = f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if let Ok(Some(old)) = db.manifest_by_name(&name) {
            println!("{name}: replacing the earlier import of this file");
            if let Err(e) = db.remove_manifest(old.id) {
                eprintln!("  could not remove the earlier import: {e}");
            }
        }
        let t = Instant::now();
        let format = std::fs::File::open(f).and_then(sniff_format).unwrap_or(ListFormat::Manifest);
        let how = match format {
            ListFormat::Manifest => ImportAs::Manifest,
            ListFormat::Checksums => {
                let first = std::fs::File::open(f).and_then(|r| first_checksum_paths(r, 20)).unwrap_or_default();
                let refs: Vec<&[u8]> = first.iter().map(|v| v.as_slice()).collect();
                let guess = guess_checksums(&name, &refs);
                let c = ChecksumImport {
                    host: source.clone().unwrap_or(guess.host),
                    base: base.clone().unwrap_or(guess.base),
                    sizes: sizes.clone().or_else(|| find_size_list(f)),
                };
                println!(
                    "{name}: checksum list, imported as source \"{}\"{}{}",
                    c.host,
                    if c.base.is_empty() { String::new() } else { format!(", relative paths under {}", c.base) },
                    c.sizes.as_ref().map(|p| format!(", sizes from {}", p.display())).unwrap_or_default()
                );
                ImportAs::Checksums(c)
            }
        };
        let report = match db.import_file_as(f, &how, &mut |_| true) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("{}: import failed: {e}", f.display());
                alerts += 1;
                continue;
            }
        };
        let sev = match report.severity() {
            Severity::Alert => {
                alerts += 1;
                "  [ALERT]"
            }
            Severity::Notice => "  [notice]",
            Severity::Ok => "",
        };
        println!(
            "{name}: {} rows ({} SHA-256), {}, {} {}, {:.1}s{sev}",
            thousands(report.rows_imported),
            thousands(report.rows_sha256),
            human(report.bytes_total),
            report.encoding,
            report.line_endings,
            t.elapsed().as_secs_f64()
        );
        for line in report.findings() {
            println!("  {line}");
        }
        for s in report.samples.iter().filter(|s| s.category == "flagged" || s.category == "malformed").take(12) {
            println!("    line {}: {} ({})  {}", s.line, s.category, s.detail, s.text);
        }
    }

    let t = Instant::now();
    let ix = match Index::build(&db) {
        Ok(ix) => ix,
        Err(e) => {
            eprintln!("index build failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!(
        "\nIndex: {} files, {} folders, {} distinct contents, {} duplicate groups ({:.1}s)",
        thousands(ix.file_count() as u64),
        thousands(ix.dir_count() as u64),
        thousands(ix.content_count() as u64),
        thousands(ix.dup_group_count() as u64),
        t.elapsed().as_secs_f64()
    );
    println!(
        "\n{:<32} {:>11} {:>10} {:>10} {:>16} {:>11} {:>8}",
        "source", "files", "size", "dup files", "in other sources", "only here", "flagged"
    );
    for s in ix.source_summaries() {
        println!(
            "{:<32} {:>11} {:>10} {:>10} {:>16} {:>11} {:>8}",
            s.host,
            thousands(s.files),
            human(s.bytes),
            thousands(s.dup_files),
            human(s.bytes_in_other_sources),
            thousands(s.only_here_files),
            thousands(s.flagged_files)
        );
    }
    let (total, groups) = ix.dup_groups(&DupQuery::default(), 0, top);
    let wasted: u64 = {
        let (_, all) = ix.dup_groups(&DupQuery::default(), 0, usize::MAX);
        all.iter().map(|g| g.wasted).sum()
    };
    println!("\n{} duplicate groups; keeping one copy of each would free {}.", thousands(total as u64), human(wasted));
    for (i, g) in groups.iter().enumerate() {
        let locs = ix.locations(&db, g.content).unwrap_or_default();
        println!(
            "{:>3}. {} wasted, {} copies of {}{}",
            i + 1,
            human(g.wasted),
            g.copies,
            human(g.size),
            if g.flagged { " [non-SHA-256]" } else { "" }
        );
        for l in locs.iter().take(6) {
            println!("       {}  [{}]", l.path, l.host);
        }
        if locs.len() > 6 {
            println!("       ... and {} more", locs.len() - 6);
        }
    }
    if alerts > 0 {
        println!("\n{alerts} manifest(s) raised alerts; see above.");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::wild_match;

    #[test]
    fn wildcards() {
        assert!(wild_match("manifest-*.tsv", "manifest-PC1.tsv"));
        assert!(wild_match("MANIFEST-*.TSV", "manifest-cloud-gdrive.tsv"));
        assert!(wild_match("*", "x"));
        assert!(wild_match("m?n*", "man"));
        assert!(!wild_match("manifest-*.tsv", "expected.json"));
        assert!(!wild_match("a*b", "ac"));
    }
}
