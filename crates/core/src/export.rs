//! Delete scripts made from the marks for deletion: one per source. DupeMap never deletes
//! anything itself.
//!
//! * Windows drives: a PowerShell script (Recycle Bin by default, or move into a
//!   `_DupeMap deleted` folder, or delete), with paths over 260 characters handled and the
//!   drive's label checked first (a USB drive can have another letter now).
//! * Linux: a bash script (move into `_dupemap_deleted` at the top of the file system by default,
//!   or `rm`), with exact file name bytes, hard links left alone and a note on ZFS snapshots.
//! * Cloud drives: a PowerShell script running `rclone delete` (into the trash by default).
//! * Optionally a plain list of the paths per source.
//!
//! Every script checks each file before touching it (still there, same size, optionally the same
//! SHA-256), shows what it will do, asks first, has a dry run and writes a log.

use crate::db::Db;
use crate::error::Result;
use crate::index::{FileIx, Index, SizeState, SourceIx};
use crate::manifest::Digest;
use crate::marks::{MarkState, MarkTotals};
use crate::paths::{anchor_kind, PathKind};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

const WINDOWS: &str = include_str!("scripts/delete-windows.ps1");
const LINUX: &str = include_str!("scripts/delete-linux.sh");
const RCLONE: &str = include_str!("scripts/delete-rclone.ps1");

/// What the Windows script does with a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum WindowsMode {
    #[default]
    RecycleBin,
    /// Into `_DupeMap deleted\<date>` at the top of the drive.
    Move,
    Delete,
}

/// What the Linux script does with a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum LinuxMode {
    /// Into `_dupemap_deleted/<date>` at the top of the file system.
    #[default]
    Move,
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportOptions {
    /// Also files of which every copy is marked (they would be gone for good).
    pub include_last_copies: bool,
    pub windows: WindowsMode,
    pub linux: LinuxMode,
    /// Cloud: delete for good instead of into the trash.
    pub cloud_permanent: bool,
    /// The scripts check each file's SHA-256 before removing it (reads every file).
    pub check_hash: bool,
    /// Also write a plain list of the paths per source.
    pub lists: bool,
}

impl Default for ExportOptions {
    fn default() -> Self {
        ExportOptions {
            include_last_copies: false,
            windows: WindowsMode::RecycleBin,
            linux: LinuxMode::Move,
            cloud_permanent: false,
            check_hash: false,
            lists: true,
        }
    }
}

/// One file to remove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanFile {
    pub file: FileIx,
    /// `D:`, `\\server\share`, `/` or `remote:`.
    pub anchor: String,
    /// The rest of the path, exactly as on disk (not always UTF-8 on Linux), with the anchor's
    /// separator.
    pub rel: Vec<u8>,
    /// Bytes, when the manifest listed them (or a copy with the same hash did).
    pub size: Option<u64>,
    /// Lowercase hex, when the manifest had a SHA-256.
    pub sha256: Option<String>,
    /// Every copy of it is marked.
    pub last_copy: bool,
}

impl PlanFile {
    pub fn kind(&self) -> PathKind {
        anchor_kind(&self.anchor)
    }

    /// The full path as bytes (`D:\a\b`, `/mnt/a`, `remote:/a`).
    pub fn full_path(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.anchor.len() + 1 + self.rel.len());
        match self.kind() {
            PathKind::Windows => {
                out.extend_from_slice(self.anchor.as_bytes());
                out.push(b'\\');
            }
            PathKind::Posix => out.push(b'/'),
            PathKind::Cloud => {
                out.extend_from_slice(self.anchor.as_bytes());
                out.push(b'/');
            }
            PathKind::Relative => {}
        }
        out.extend_from_slice(&self.rel);
        out
    }
}

/// The marked files of one source.
#[derive(Debug, Clone, Default)]
pub struct SourcePlan {
    pub source: SourceIx,
    pub host: String,
    pub files: Vec<PlanFile>,
    pub totals: MarkTotals,
    /// Marked files left out because every copy of them is marked.
    pub left_out: MarkTotals,
    /// Marked files no script can name safely (a line break in the name, no drive or root).
    pub unscriptable: Vec<String>,
}

/// The marked files per source, with their exact paths, sizes and hashes from the database.
/// Files of which every copy is marked are left out unless `include_last_copies`.
pub fn plan(ix: &Index, db: &Db, st: &MarkState, include_last_copies: bool) -> Result<Vec<SourcePlan>> {
    let mut plans: Vec<SourcePlan> = ix
        .sources()
        .iter()
        .enumerate()
        .map(|(i, s)| SourcePlan { source: i as SourceIx, host: s.host.clone(), ..Default::default() })
        .collect();
    // each folder's anchor and exact path, built from its parent's
    let mut dirs: HashMap<u32, Option<(String, Vec<u8>)>> = HashMap::new();
    for f in st.marked_files() {
        let src = ix.file_source(f) as usize;
        let last = st.is_last_copy(f);
        let size_state = ix.size_state(f);
        let bytes = ix.file_size(f);
        if last && !include_last_copies {
            plans[src].left_out.files += 1;
            plans[src].left_out.bytes += bytes;
            plans[src].left_out.last_files += 1;
            plans[src].left_out.last_bytes += bytes;
            continue;
        }
        let rec = db.file_record(ix.file_db_id(f))?;
        let name = rec.raw.clone().unwrap_or_else(|| rec.name.clone().into_bytes());
        let dir = dir_path(ix, db, ix.file_dir(f), &mut dirs)?;
        let Some((anchor, dir_rel)) = dir else {
            plans[src].unscriptable.push(ix.path_with_name(ix.file_dir(f), &rec.name));
            continue;
        };
        if name.contains(&b'\n') || name.contains(&b'\r') || dir_rel.contains(&b'\n') || dir_rel.contains(&b'\r') {
            plans[src].unscriptable.push(ix.path_with_name(ix.file_dir(f), &rec.name));
            continue;
        }
        let sep = if anchor_kind(&anchor) == PathKind::Windows { b'\\' } else { b'/' };
        let mut rel = dir_rel.clone();
        if !rel.is_empty() {
            rel.push(sep);
        }
        rel.extend_from_slice(&name);
        let sha256 = match &rec.digest {
            Digest::Sha256(d) => Some(d.iter().fold(String::with_capacity(64), |mut s, b| {
                let _ = write!(s, "{b:02x}");
                s
            })),
            _ => None,
        };
        let size = (size_state != SizeState::Unknown).then_some(bytes);
        plans[src].totals.add(bytes, last);
        plans[src].files.push(PlanFile { file: f, anchor, rel, size, sha256, last_copy: last });
    }
    plans.retain(|p| !p.files.is_empty() || !p.left_out.is_empty() || !p.unscriptable.is_empty());
    Ok(plans)
}

/// A folder's anchor and the exact bytes of the rest of its path; `None` for a source's root and
/// for paths without a drive or root.
fn dir_path(
    ix: &Index,
    db: &Db,
    d: u32,
    cache: &mut HashMap<u32, Option<(String, Vec<u8>)>>,
) -> Result<Option<(String, Vec<u8>)>> {
    if let Some(v) = cache.get(&d) {
        return Ok(v.clone());
    }
    let value = match ix.dir_parent(d) {
        None => None,
        Some(parent) if ix.dir_parent(parent).is_none() => {
            // a child of the source's root is the anchor (`D:`, `/`, `gdrive:`)
            let anchor = ix.dir_name(d).to_string();
            (anchor_kind(&anchor) != PathKind::Relative).then(|| (anchor, Vec::new()))
        }
        Some(parent) => match dir_path(ix, db, parent, cache)? {
            None => None,
            Some((anchor, mut rel)) => {
                let (_, name, raw) = db.dir_name_raw(ix.dir_db_id(d))?;
                if !rel.is_empty() {
                    rel.push(if anchor_kind(&anchor) == PathKind::Windows { b'\\' } else { b'/' });
                }
                rel.extend_from_slice(&raw.unwrap_or_else(|| name.into_bytes()));
                Some((anchor, rel))
            }
        },
    };
    cache.insert(d, value.clone());
    Ok(value)
}

/// What kind of file an export wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Written {
    PowerShell,
    Bash,
    Rclone,
    List,
}

impl Written {
    pub fn label(self) -> &'static str {
        match self {
            Written::PowerShell => "PowerShell script",
            Written::Bash => "bash script",
            Written::Rclone => "rclone script (PowerShell)",
            Written::List => "list of paths",
        }
    }
}

/// One file an export wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrittenFile {
    pub path: PathBuf,
    pub host: String,
    pub what: Written,
    pub files: u64,
    pub bytes: u64,
}

/// `PC2 F: (Old Seagate)` → `PC2-F-Old-Seagate`.
pub fn file_stem(host: &str) -> String {
    let s = crate::scan::manifest_stem(host);
    let s = s.strip_prefix("manifest-").unwrap_or(&s).to_string();
    if s.is_empty() {
        "source".into()
    } else {
        s
    }
}

/// Write the scripts (and lists) for these plans into `dir`. `made` is the date shown in them.
pub fn write(plans: &[SourcePlan], opts: &ExportOptions, dir: &Path, made: &str) -> Result<Vec<WrittenFile>> {
    std::fs::create_dir_all(dir)?;
    let mut out = Vec::new();
    for plan in plans {
        // normally one kind of path per source; a mixed one gets a script per kind
        let mut kinds: Vec<PathKind> = Vec::new();
        for f in &plan.files {
            if !kinds.contains(&f.kind()) {
                kinds.push(f.kind());
            }
        }
        for kind in &kinds {
            let files: Vec<&PlanFile> = plan.files.iter().filter(|f| f.kind() == *kind).collect();
            let mut stem = format!("dupemap-delete-{}", file_stem(&plan.host));
            if kinds.len() > 1 {
                stem.push_str(match kind {
                    PathKind::Windows => "-windows",
                    PathKind::Posix => "-linux",
                    PathKind::Cloud => "-cloud",
                    PathKind::Relative => "-other",
                });
            }
            let bytes: u64 = files.iter().map(|f| f.size.unwrap_or(0)).sum();
            let (what, ext) = match kind {
                PathKind::Windows => (Some(Written::PowerShell), "ps1"),
                PathKind::Posix => (Some(Written::Bash), "sh"),
                PathKind::Cloud => (Some(Written::Rclone), "ps1"),
                PathKind::Relative => (None, ""),
            };
            if let Some(what) = what {
                let file_name = format!("{stem}.{ext}");
                let text = script(*kind, &plan.host, &files, opts, &file_name, &format!("{stem}.log"), made);
                let path = dir.join(&file_name);
                std::fs::write(&path, text)?;
                out.push(WrittenFile { path, host: plan.host.clone(), what, files: files.len() as u64, bytes });
            }
            if opts.lists || what.is_none() {
                let eol: &[u8] = if *kind == PathKind::Windows { b"\r\n" } else { b"\n" };
                let mut text = Vec::new();
                for f in &files {
                    text.extend_from_slice(&f.full_path());
                    text.extend_from_slice(eol);
                }
                let path = dir.join(format!("{stem}-list.txt"));
                std::fs::write(&path, text)?;
                out.push(WrittenFile {
                    path,
                    host: plan.host.clone(),
                    what: Written::List,
                    files: files.len() as u64,
                    bytes,
                });
            }
        }
    }
    Ok(out)
}

fn count_text(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    format!("{out} {}", if n == 1 { "file" } else { "files" })
}

/// 1024-based, as Explorer shows sizes.
pub fn bytes_text(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    format!("{v:.1} {}", UNITS[u])
}

/// A string for a single-quoted PowerShell literal.
fn ps_quote(s: &str) -> String {
    s.replace('\'', "''")
}

/// A single-quoted bash word.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Any bytes as a bash `$'...'` word: exact, including names that are not UTF-8.
pub fn sh_bytes(b: &[u8]) -> String {
    let mut out = String::with_capacity(b.len() + 3);
    out.push_str("$'");
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            b'\\' => out.push_str("\\\\"),
            b'\'' => out.push_str("\\'"),
            b'\n' => out.push_str("\\n"),
            b'\t' => out.push_str("\\t"),
            b'\r' => out.push_str("\\r"),
            0x20..=0x7e => out.push(c as char),
            0x80..=0xff => {
                // valid UTF-8 stays readable; anything else is written byte by byte
                let len = match c {
                    0xc2..=0xdf => 2,
                    0xe0..=0xef => 3,
                    0xf0..=0xf4 => 4,
                    _ => 0,
                };
                match (len > 0).then(|| b.get(i..i + len)).flatten().and_then(|s| std::str::from_utf8(s).ok()) {
                    Some(s) => {
                        out.push_str(s);
                        i += len;
                        continue;
                    }
                    None => {
                        let _ = write!(out, "\\x{c:02x}");
                    }
                }
            }
            _ => {
                let _ = write!(out, "\\x{c:02x}");
            }
        }
        i += 1;
    }
    out.push('\'');
    out
}

fn script(
    kind: PathKind,
    host: &str,
    files: &[&PlanFile],
    opts: &ExportOptions,
    file_name: &str,
    log_name: &str,
    made: &str,
) -> Vec<u8> {
    let bytes: u64 = files.iter().map(|f| f.size.unwrap_or(0)).sum();
    let last: Vec<&&PlanFile> = files.iter().filter(|f| f.last_copy).collect();
    let last_bytes: u64 = last.iter().map(|f| f.size.unwrap_or(0)).sum();
    let comment = if kind == PathKind::Posix { "#" } else { " " };
    let last_note = if last.is_empty() {
        format!("{comment} Each of them has a copy elsewhere that is not marked (as far as DupeMap knows).")
    } else {
        format!(
            "{comment} WARNING: {} ({}) of them are the last copy DupeMap knows of: every other copy is marked too.",
            count_text(last.len()),
            bytes_text(last_bytes)
        )
    };
    let default = " (the default)";
    let pick = |on: bool| if on { default } else { "" };
    let mut list = String::new();
    let template = match kind {
        PathKind::Windows => {
            for f in files {
                let size = f.size.map(|s| s.to_string()).unwrap_or_default();
                let path = String::from_utf8_lossy(&f.full_path()).into_owned();
                let _ = writeln!(list, "{size}\t{}\t{path}", f.sha256.as_deref().unwrap_or(""));
            }
            WINDOWS
                .replace(
                    "@@MODE@@",
                    match opts.windows {
                        WindowsMode::RecycleBin => "RecycleBin",
                        WindowsMode::Move => "Move",
                        WindowsMode::Delete => "Delete",
                    },
                )
                .replace("@@DEFAULT_RECYCLE@@", pick(opts.windows == WindowsMode::RecycleBin))
                .replace("@@DEFAULT_MOVE@@", pick(opts.windows == WindowsMode::Move))
                .replace("@@DEFAULT_DELETE@@", pick(opts.windows == WindowsMode::Delete))
                .replace("@@CHECK_HASH@@", if opts.check_hash { "true" } else { "false" })
                .replace(
                    "@@HASH_DEFAULT@@",
                    if opts.check_hash {
                        "\n                       (on here; -CheckHash:$false turns it off)"
                    } else {
                        ""
                    },
                )
        }
        PathKind::Posix => {
            for f in files {
                let size = f.size.map(|s| s.to_string()).unwrap_or_else(|| "''".into());
                let hash = f.sha256.clone().unwrap_or_else(|| "''".into());
                let _ = writeln!(list, "add {size} {hash} {}", sh_bytes(&f.full_path()));
            }
            LINUX
                .replace("@@MODE@@", if opts.linux == LinuxMode::Move { "move" } else { "delete" })
                .replace("@@DEFAULT_MOVE@@", pick(opts.linux == LinuxMode::Move))
                .replace("@@DEFAULT_DELETE@@", pick(opts.linux == LinuxMode::Delete))
                .replace("@@CHECK_HASH@@", if opts.check_hash { "1" } else { "0" })
                .replace(
                    "@@HASH_DEFAULT@@",
                    if opts.check_hash { "\n#                   (on here; --no-check-hash turns it off)" } else { "" },
                )
                .replace("@@HOST_SH@@", &sh_quote(host))
        }
        _ => {
            for f in files {
                let size = f.size.map(|s| s.to_string()).unwrap_or_default();
                let _ = writeln!(list, "{size}\t{}", String::from_utf8_lossy(&f.rel));
            }
            RCLONE.replace("@@PERMANENT@@", if opts.cloud_permanent { "true" } else { "false" }).replace(
                "@@DEFAULT_PERMANENT@@",
                if opts.cloud_permanent { " (on here; -Permanent:$false turns it off)" } else { "" },
            )
        }
    };
    let hash_note = if opts.check_hash && kind != PathKind::Cloud { ", same SHA-256" } else { "" };
    let text = template
        .replace("@@VERSION@@", env!("CARGO_PKG_VERSION"))
        .replace("@@MADE@@", made)
        .replace("@@COUNT_TEXT@@", &count_text(files.len()))
        .replace("@@BYTES_TEXT@@", &bytes_text(bytes))
        .replace("@@LAST_NOTE@@", &last_note)
        .replace("@@HASH_NOTE@@", hash_note)
        .replace("@@FILE_NAME@@", file_name)
        .replace("@@LOG_NAME@@", log_name)
        .replace("@@HOST_PS@@", &ps_quote(host))
        .replace("@@HOST@@", host)
        // last, so nothing in the list is mistaken for a placeholder
        .replace("@@LIST@@", list.trim_end_matches('\n'));
    if kind == PathKind::Posix {
        text.into_bytes()
    } else {
        // Windows PowerShell 5.1 reads a file without BOM in the ANSI code page
        let mut out = vec![0xEF, 0xBB, 0xBF];
        out.extend_from_slice(text.replace("\r\n", "\n").replace('\n', "\r\n").as_bytes());
        out
    }
}

/// `2026-10-27 18:05 UTC` for a time.
pub fn utc_text(t: std::time::SystemTime) -> String {
    let secs = t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // days since 1970-01-01 to a civil date (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02} UTC", rem / 3600, rem % 3600 / 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::marks::{resolve, MarkAction, MarkRule};

    const HEADER: &str = "hash\tsize\thost\tpath";

    fn h(n: u8) -> String {
        format!("{:064x}", n as u128 + 1)
    }

    fn state(ix: &Index, db: &Db, rules: &[MarkRule]) -> MarkState {
        let places = resolve(ix, db, rules).unwrap();
        MarkState::compute(ix, rules, &places)
    }

    /// The bash script on real files: dry run, delete, move, hash check, and every file it must
    /// leave alone (changed size, gone, now a link, other hard links, changed content).
    #[cfg(target_os = "linux")]
    #[test]
    fn the_bash_script_removes_exactly_what_it_should() {
        use crate::scan::{computer_name, scan_volume, ScanSettings, Volume};
        use std::os::unix::ffi::OsStrExt;
        use std::sync::atomic::AtomicBool;
        let dir = std::env::temp_dir().join(format!("dupemap-delete-sh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let data = dir.join("data");
        let work = dir.join("work");
        std::fs::create_dir_all(data.join("sub dir")).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        let odd = data.join(std::ffi::OsStr::from_bytes(b"odd\xff name.bin"));
        let files: Vec<(PathBuf, u8)> = vec![
            (data.join("a.bin"), 1),
            (data.join("sub dir/b c.bin"), 2),
            (data.join("it's $HOME.bin"), 3),
            (odd.clone(), 4),
            (data.join("hard.bin"), 5),
            (data.join("changed.bin"), 6),
            (data.join("gone.bin"), 7),
            (data.join("now a link.bin"), 8),
            (data.join("same size.bin"), 9),
        ];
        for (p, b) in &files {
            std::fs::write(p, vec![*b; 3000]).unwrap();
        }
        let host = computer_name();
        let scanned = scan_volume(
            &Volume::folder(&data, &host),
            &dir.join("scans"),
            &ScanSettings { min_size: 1000, ..Default::default() },
            false,
            &mut |_| {},
            &AtomicBool::new(false),
        )
        .unwrap();
        let mut db = Db::open_in_memory().unwrap();
        db.import_file(&scanned.manifest, &mut |_| true).unwrap();
        let ix = Index::build(&db).unwrap();
        let rule =
            MarkRule { host: host.clone(), path: data.display().to_string(), is_dir: true, action: MarkAction::Delete };
        let st = state(&ix, &db, &[rule]);
        assert_eq!(st.total.files, files.len() as u64);
        // every copy is marked (they are the only ones), so the plan must include last copies
        let plans = plan(&ix, &db, &st, true).unwrap();
        assert_eq!(plans.len(), 1);
        // after the scan: a hard link, another size, gone, a link instead of the file, other
        // content of the same size
        std::fs::hard_link(data.join("hard.bin"), data.join("hard link.bin")).unwrap();
        std::fs::write(data.join("changed.bin"), vec![6u8; 10]).unwrap();
        std::fs::remove_file(data.join("gone.bin")).unwrap();
        std::fs::remove_file(data.join("now a link.bin")).unwrap();
        std::os::unix::fs::symlink(data.join("a.bin"), data.join("now a link.bin")).unwrap();
        std::fs::write(data.join("same size.bin"), vec![10u8; 3000]).unwrap();
        let run = |opts: &ExportOptions, args: &[&str], env: Option<(&str, &Path)>| -> String {
            let out_dir = dir.join("scripts");
            let _ = std::fs::remove_dir_all(&out_dir);
            let written = write(&plans, opts, &out_dir, "now").unwrap();
            let script = written.iter().find(|w| w.what == Written::Bash).unwrap();
            let mut cmd = std::process::Command::new("bash");
            cmd.arg(&script.path).args(args).current_dir(&work);
            if let Some((k, v)) = env {
                cmd.env(k, v);
            }
            let out = cmd.output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        let exists = |p: &Path| std::fs::symlink_metadata(p).is_ok();
        // a dry run changes nothing
        let opts = ExportOptions { linux: LinuxMode::Delete, check_hash: true, ..Default::default() };
        let text = run(&opts, &["--dry-run"], None);
        // same size.bin passes the size check; the hash check catches it when it runs
        assert!(text.contains("Delete: 5 files"), "{text}");
        assert!(text.contains("1 are not there any more"));
        assert!(text.contains("1 have another size"));
        assert!(text.contains("1 are links or not plain files"));
        assert!(text.contains("1 have other hard links"));
        assert!(files.iter().filter(|(p, _)| !p.ends_with("gone.bin")).all(|(p, _)| exists(p)));
        // the real thing, with the hash check: same size.bin changed and stays
        let text = run(&opts, &["--yes"], None);
        assert!(text.contains("Done: 4 files"), "{text}");
        assert!(text.contains("1 had another SHA-256"));
        for gone in ["a.bin", "sub dir/b c.bin", "it's $HOME.bin"] {
            assert!(!exists(&data.join(gone)), "{gone} should be gone");
        }
        assert!(!exists(&odd), "the name that is not UTF-8 is removed too");
        for stays in ["hard.bin", "hard link.bin", "changed.bin", "now a link.bin", "same size.bin"] {
            assert!(exists(&data.join(stays)), "{stays} should stay");
        }
        // (the log has the exact name bytes, not all UTF-8)
        let log = std::fs::read(work.join("dupemap-delete-".to_string() + &file_stem(&host) + ".log")).unwrap();
        let log = String::from_utf8_lossy(&log);
        assert!(log.contains("deleted") && log.contains("another SHA-256"));
        // move mode: the hard-linked file (allowed now) goes into _dupemap_deleted
        let moved_root = dir.join("moved");
        std::fs::create_dir_all(&moved_root).unwrap();
        let opts = ExportOptions { linux: LinuxMode::Move, ..Default::default() };
        let text = run(&opts, &["--yes", "--hard-links"], Some(("DUPEMAP_MOVE_ROOT", &moved_root)));
        // hard.bin (other links allowed now) and same size.bin (no hash check now)
        assert!(text.contains("Done: 2 files"), "{text}");
        assert!(!exists(&data.join("hard.bin")));
        let stamp_dirs: Vec<_> = std::fs::read_dir(moved_root.join("_dupemap_deleted")).unwrap().collect();
        assert_eq!(stamp_dirs.len(), 1);
        let stamp = stamp_dirs[0].as_ref().unwrap().path();
        let rel = data.join("hard.bin").display().to_string();
        assert!(stamp.join(rel.trim_start_matches('/')).is_file());
        // a computer with another name refuses
        let mut other = plans.clone();
        other[0].host = "some-other-computer".into();
        let out_dir = dir.join("scripts-other");
        let written = write(&other, &ExportOptions::default(), &out_dir, "now").unwrap();
        let script = written.iter().find(|w| w.what == Written::Bash).unwrap();
        let out =
            std::process::Command::new("bash").arg(&script.path).arg("--yes").current_dir(&work).output().unwrap();
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("were scanned on some-other-computer"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dates_read_as_utc() {
        use std::time::{Duration, UNIX_EPOCH};
        assert_eq!(utc_text(UNIX_EPOCH), "1970-01-01 00:00 UTC");
        assert_eq!(utc_text(UNIX_EPOCH + Duration::from_secs(1_793_124_300)), "2026-10-27 18:05 UTC");
        assert_eq!(utc_text(UNIX_EPOCH + Duration::from_secs(951_782_400)), "2000-02-29 00:00 UTC");
    }

    #[test]
    fn bash_words_keep_every_byte() {
        assert_eq!(sh_bytes(b"/mnt/a b/c'd"), r"$'/mnt/a b/c\'d'");
        assert_eq!(sh_bytes("/mnt/Plaża/x\\y".as_bytes()), r"$'/mnt/Plaża/x\\y'");
        assert_eq!(sh_bytes(b"/mnt/\xff\x81z\t1"), r"$'/mnt/\xff\x81z\t1'");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        assert_eq!(ps_quote("it's"), "it''s");
        assert_eq!(file_stem("PC2 F: (Old Seagate)"), "PC2-F-Old-Seagate");
        assert_eq!(file_stem("onedrive:"), "onedrive");
    }

    #[test]
    fn plans_have_exact_paths_and_leave_out_last_copies() {
        let mut db = Db::open_in_memory().unwrap();
        let pc = format!(
            "{HEADER}\n\
             {a}\t100\tPC1 D: (Data)\tD:\\Photos\\a.jpg\n\
             {a}\t100\tPC1 D: (Data)\tD:\\Backup\\a.jpg\n\
             {b}\t200\tPC1 D: (Data)\tD:\\Backup\\only.bin\n",
            a = h(1),
            b = h(2)
        );
        let mut nas = format!("{HEADER}\n{a}\t100\tnas\t/mnt/tank/it's here/a.jpg\n", a = h(1)).into_bytes();
        // a Linux name that is not UTF-8
        nas.extend_from_slice(format!("{c}\t300\tnas\t/mnt/tank/odd", c = h(3)).as_bytes());
        nas.extend_from_slice(b"\xff.bin\n");
        nas.extend_from_slice(format!("{c}\t300\tnas\t/mnt/tank/copy.bin\n", c = h(3)).as_bytes());
        db.import_reader("pc.tsv", "pc.tsv", 0, None, pc.as_bytes(), &mut |_| true).unwrap();
        db.import_reader("nas.tsv", "nas.tsv", 0, None, &nas[..], &mut |_| true).unwrap();
        let ix = Index::build(&db).unwrap();
        let rules = [
            MarkRule {
                host: "PC1 D: (Data)".into(),
                path: r"D:\Backup".into(),
                is_dir: true,
                action: MarkAction::Delete,
            },
            MarkRule { host: "nas".into(), path: "/mnt/tank".into(), is_dir: true, action: MarkAction::Delete },
        ];
        let st = state(&ix, &db, &rules);
        let plans = plan(&ix, &db, &st, false).unwrap();
        let pc = plans.iter().find(|p| p.host == "PC1 D: (Data)").unwrap();
        // only.bin has no other copy: left out
        assert_eq!(pc.files.len(), 1);
        assert_eq!(pc.files[0].full_path(), br"D:\Backup\a.jpg");
        assert_eq!(pc.files[0].size, Some(100));
        assert_eq!(pc.files[0].sha256.as_deref(), Some(h(1).as_str()));
        assert_eq!(pc.left_out.files, 1);
        assert_eq!(pc.left_out.bytes, 200);
        let nas = plans.iter().find(|p| p.host == "nas").unwrap();
        let mut paths: Vec<Vec<u8>> = nas.files.iter().map(|f| f.full_path()).collect();
        paths.sort();
        // a.jpg (copies on the PC); odd\xff.bin and copy.bin are copies of each other, both
        // marked: last copies, left out
        assert_eq!(paths, [b"/mnt/tank/it's here/a.jpg".to_vec()]);
        assert_eq!(nas.left_out.files, 2);
        let with_last = plan(&ix, &db, &st, true).unwrap();
        let nas = with_last.iter().find(|p| p.host == "nas").unwrap();
        let mut paths: Vec<Vec<u8>> = nas.files.iter().map(|f| f.full_path()).collect();
        paths.sort();
        assert_eq!(
            paths,
            [b"/mnt/tank/copy.bin".to_vec(), b"/mnt/tank/it's here/a.jpg".to_vec(), b"/mnt/tank/odd\xff.bin".to_vec()]
        );
        assert!(nas.files.iter().filter(|f| f.last_copy).count() == 2);

        // the scripts
        let dir = std::env::temp_dir().join(format!("dupemap-export-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let written = write(&with_last, &ExportOptions::default(), &dir, "2026-10-27 18:05 UTC").unwrap();
        let names: Vec<String> =
            written.iter().map(|w| w.path.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert!(names.contains(&"dupemap-delete-PC1-D-Data.ps1".to_string()), "{names:?}");
        assert!(names.contains(&"dupemap-delete-PC1-D-Data-list.txt".to_string()));
        assert!(names.contains(&"dupemap-delete-nas.sh".to_string()));
        let ps = std::fs::read(dir.join("dupemap-delete-PC1-D-Data.ps1")).unwrap();
        assert!(ps.starts_with(&[0xEF, 0xBB, 0xBF]));
        let ps = String::from_utf8(ps[3..].to_vec()).unwrap();
        assert!(!ps.contains("@@"), "unfilled placeholder");
        assert!(ps.contains(&format!("100\t{}\tD:\\Backup\\a.jpg\r\n", h(1))));
        assert!(ps.contains("$source = 'PC1 D: (Data)'"));
        assert!(ps.contains("[string]$Mode = 'RecycleBin'"));
        assert!(!ps.replace("\r\n", "").contains('\n'), "CRLF throughout");
        let sh = std::fs::read(dir.join("dupemap-delete-nas.sh")).unwrap();
        let sh = String::from_utf8(sh).unwrap();
        assert!(!sh.contains("@@"));
        assert!(sh.contains(r"add 300 "), "{sh}");
        assert!(sh.contains(r"$'/mnt/tank/odd\xff.bin'"));
        assert!(sh.contains(r"$'/mnt/tank/it\'s here/a.jpg'"));
        assert!(sh.contains("WARNING: 2 files"));
        assert!(sh.contains("SOURCE='nas'"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
