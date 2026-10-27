//! SQLite storage: sources, the folder tree, file rows, manifests and their import reports.
//!
//! Identical paths listed by more than one manifest (or twice in one manifest) are stored once
//! as *visible* and the rest as *hidden* (`shadowed_by` = the manifest whose listing wins), so a
//! file scanned twice can never be counted as two copies. Removing a manifest restores the
//! listings it was hiding.
//!
//! Which listing wins: a SHA-256 listing beats a flagged one, then a listing with a size beats
//! one without (checksum lists have no sizes); otherwise the newer scan wins (manifest file date,
//! falling back to import time), and on a tie the later import.

use crate::error::{err, Error, Result};
use crate::ignore::IgnoreSettings;
use crate::manifest::{is_header, parse_checksum_line, parse_row, Counting, Digest, LineReader, Malformed, Row};
pub use crate::manifest::{ChecksumImport, ImportAs};
use crate::paths::{self, Comp, PathKind};
use crate::report::{ImportReport, Overlap, Samples, Severity};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// 3: `file.size` is -1 for a file whose size is not known (rows from a checksum list).
pub const SCHEMA_VERSION: i64 = 3;

/// Algorithm id reserved for SHA-256.
pub const ALGO_SHA256: i64 = 0;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS manifest (
    id              INTEGER PRIMARY KEY,
    file_name       TEXT NOT NULL,
    file_path       TEXT NOT NULL,
    file_size       INTEGER NOT NULL,
    file_mtime      INTEGER,
    imported_at     INTEGER NOT NULL,
    scan_time       INTEGER NOT NULL,
    rows            INTEGER NOT NULL DEFAULT 0,
    flagged         INTEGER NOT NULL DEFAULT 0,
    malformed       INTEGER NOT NULL DEFAULT 0,
    severity        INTEGER NOT NULL DEFAULT 0,
    alert_dismissed INTEGER NOT NULL DEFAULT 0,
    report          TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS source (
    id   INTEGER PRIMARY KEY,
    host TEXT NOT NULL UNIQUE,
    kind INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS dir (
    id        INTEGER PRIMARY KEY,
    source_id INTEGER NOT NULL,
    parent_id INTEGER,
    name      TEXT NOT NULL,
    name_key  TEXT,
    raw       BLOB
);
CREATE UNIQUE INDEX IF NOT EXISTS dir_by_parent ON dir(source_id, ifnull(parent_id, 0), ifnull(name_key, name));
CREATE TABLE IF NOT EXISTS algo (
    id   INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE
);
CREATE TABLE IF NOT EXISTS ext (
    id   INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE
);
CREATE TABLE IF NOT EXISTS file (
    id          INTEGER PRIMARY KEY,
    manifest_id INTEGER NOT NULL,
    line_no     INTEGER NOT NULL,
    dir_id      INTEGER NOT NULL,
    name        TEXT NOT NULL,
    name_key    TEXT,
    raw         BLOB,
    size        INTEGER NOT NULL, -- -1 = not known (checksum lists have no sizes)
    algo_id     INTEGER NOT NULL,
    digest      BLOB NOT NULL,
    ext_id      INTEGER NOT NULL,
    shadowed_by INTEGER
);
CREATE INDEX IF NOT EXISTS file_by_dir ON file(dir_id, ifnull(name_key, name));
CREATE INDEX IF NOT EXISTS file_by_manifest ON file(manifest_id);
CREATE INDEX IF NOT EXISTS file_shadowed ON file(shadowed_by) WHERE shadowed_by IS NOT NULL;
CREATE TABLE IF NOT EXISTS setting (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
INSERT OR IGNORE INTO algo(id, name) VALUES (0, 'sha256');
INSERT OR IGNORE INTO ext(id, name) VALUES (0, '');
"#;

/// Read a size list: `size<TAB>path` per line. Lines that are not are skipped.
fn read_sizes(path: &Path) -> Result<HashMap<Vec<u8>, u64>> {
    let (mut lines, _) = LineReader::new(File::open(path)?)?;
    let mut out = HashMap::new();
    while let Some((_, l)) = lines.next_line()? {
        let Some(tab) = l.iter().position(|&b| b == b'\t') else { continue };
        let Ok(n) = std::str::from_utf8(&l[..tab]).map(str::trim) else { continue };
        if let Ok(size) = n.parse::<u64>() {
            let p = &l[tab + 1..];
            out.insert(p.strip_prefix(b"./").unwrap_or(p).to_vec(), size);
        }
    }
    Ok(out)
}

/// An open DupeMap database.
pub struct Db {
    conn: Connection,
    path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestInfo {
    pub id: i64,
    pub file_name: String,
    pub file_path: String,
    pub file_size: u64,
    pub imported_at: i64,
    pub rows: u64,
    pub flagged: u64,
    pub malformed: u64,
    pub severity: Severity,
    pub alert_dismissed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRow {
    pub id: i64,
    pub host: String,
    pub kind: PathKind,
}

/// Import progress, reported roughly every 20 000 rows.
#[derive(Debug, Clone, Copy)]
pub struct Progress {
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub rows: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoveSummary {
    pub rows_removed: u64,
    pub rows_restored: u64,
    pub dirs_pruned: u64,
    pub sources_removed: Vec<String>,
}

/// One stored file row with everything needed for the inspector and for scripts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRecord {
    pub id: i64,
    pub manifest_id: i64,
    pub line_no: u64,
    pub dir_id: i64,
    pub name: String,
    pub raw: Option<Vec<u8>>,
    /// `None` when the list it came from had no size.
    pub size: Option<u64>,
    pub digest: Digest,
    pub shadowed_by: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DbCounts {
    pub manifests: u64,
    pub sources: u64,
    pub dirs: u64,
    pub files_visible: u64,
    pub files_hidden: u64,
}

fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

impl Db {
    /// Open (or create) a database file.
    pub fn open(path: &Path) -> Result<Db> {
        let conn = Connection::open(path)?;
        // the app reads on one connection while a background job writes on another
        conn.busy_timeout(std::time::Duration::from_secs(60))?;
        Self::init(&conn, true)?;
        Ok(Db { conn, path: Some(path.to_path_buf()) })
    }

    /// A throw-away in-memory database (tests, benchmarks).
    pub fn open_in_memory() -> Result<Db> {
        let conn = Connection::open_in_memory()?;
        Self::init(&conn, false)?;
        Ok(Db { conn, path: None })
    }

    fn init(conn: &Connection, file_backed: bool) -> Result<()> {
        if file_backed {
            conn.pragma_update(None, "journal_mode", "WAL")?;
            conn.pragma_update(None, "synchronous", "NORMAL")?;
            conn.pragma_update(None, "mmap_size", 268_435_456i64)?;
        }
        conn.pragma_update(None, "temp_store", "MEMORY")?;
        conn.pragma_update(None, "cache_size", -131_072i64)?;
        conn.pragma_update(None, "foreign_keys", "OFF")?;
        let has_meta: bool =
            conn.query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name='meta'", [], |r| {
                r.get::<_, i64>(0)
            })? > 0;
        let mut current = 0;
        if has_meta {
            let v: Option<String> =
                conn.query_row("SELECT value FROM meta WHERE key='schema_version'", [], |r| r.get(0)).optional()?;
            current = v.and_then(|s| s.parse().ok()).unwrap_or(0);
            if current > SCHEMA_VERSION {
                return err(format!(
                    "this database was written by a newer DupeMap (schema {current}, this build understands {SCHEMA_VERSION})"
                ));
            }
        }
        if current != SCHEMA_VERSION {
            // only an older or new database is written to; opening a current one is read-only
            conn.execute_batch(SCHEMA)?;
            conn.execute(
                "INSERT INTO meta(key, value) VALUES ('schema_version', ?1) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [SCHEMA_VERSION.to_string()],
            )?;
        }
        Ok(())
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Import a manifest file. `progress` returns `false` to cancel (nothing is kept).
    pub fn import_file(&mut self, path: &Path, progress: &mut dyn FnMut(Progress) -> bool) -> Result<ImportReport> {
        self.import_file_as(path, &ImportAs::Manifest, progress)
    }

    /// Import a manifest or a checksum list.
    pub fn import_file_as(
        &mut self,
        path: &Path,
        how: &ImportAs,
        progress: &mut dyn FnMut(Progress) -> bool,
    ) -> Result<ImportReport> {
        let f = File::open(path)?;
        let meta = f.metadata()?;
        let mtime = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64);
        let name =
            path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
        self.import_reader_as(&name, &path.display().to_string(), meta.len(), mtime, f, how, progress)
    }

    /// Import a manifest from any reader (used by `import_file` and by tests).
    pub fn import_reader(
        &mut self,
        file_name: &str,
        file_path: &str,
        file_size: u64,
        file_mtime: Option<i64>,
        reader: impl Read,
        progress: &mut dyn FnMut(Progress) -> bool,
    ) -> Result<ImportReport> {
        self.import_reader_as(file_name, file_path, file_size, file_mtime, reader, &ImportAs::Manifest, progress)
    }

    /// Import a manifest or a checksum list from any reader.
    #[allow(clippy::too_many_arguments)]
    pub fn import_reader_as(
        &mut self,
        file_name: &str,
        file_path: &str,
        file_size: u64,
        file_mtime: Option<i64>,
        reader: impl Read,
        how: &ImportAs,
        progress: &mut dyn FnMut(Progress) -> bool,
    ) -> Result<ImportReport> {
        let started = Instant::now();
        let checksums = match how {
            ImportAs::Manifest => None,
            ImportAs::Checksums(c) => {
                if c.host.trim().is_empty() {
                    return err("a checksum list needs a source name");
                }
                let sizes = match &c.sizes {
                    Some(p) => Some(read_sizes(p).map_err(|e| Error::Msg(format!("size list {}: {e}", p.display())))?),
                    None => None,
                };
                Some((c, sizes))
            }
        };
        let (counting, counter) = Counting::new(reader);
        let (mut lines, encoding) = LineReader::new(counting)?;
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_secs();
        let scan_time = file_mtime.unwrap_or(now);
        tx.execute(
            "INSERT INTO manifest(file_name, file_path, file_size, file_mtime, imported_at, scan_time, report)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, '{}')",
            params![file_name, file_path, file_size as i64, file_mtime, now, scan_time],
        )?;
        let mid = tx.last_insert_rowid();
        let mut imp = Importer::new(&tx, mid, scan_time)?;
        imp.report.manifest_id = mid;
        imp.report.file_mtime = file_mtime;
        imp.report.file_name = file_name.to_string();
        imp.report.file_path = file_path.to_string();
        imp.report.file_size = file_size;
        imp.report.encoding = encoding.label().to_string();
        if let Some((c, sizes)) = &checksums {
            imp.report.checksums = Some((*c).clone());
            imp.report.size_list_entries = sizes.as_ref().map(|m| m.len() as u64);
        }

        let mut first = true;
        while let Some((line_no, line)) = lines.next_line()? {
            imp.report.lines += 1;
            if first {
                first = false;
                if is_header(line) {
                    imp.report.header_found = true;
                    continue;
                }
            }
            if line.trim_ascii().is_empty() {
                imp.report.blank_lines += 1;
                continue;
            }
            match &checksums {
                None => match parse_row(line) {
                    Ok(row) => imp.add_row(line_no, row, line)?,
                    Err(m) => imp.malformed(line_no, m, line),
                },
                Some((c, sizes)) => match parse_checksum_line(line) {
                    Ok(cl) => {
                        let size = sizes.as_ref().and_then(|m| m.get(cl.path.as_ref()).copied());
                        if size.is_some() {
                            imp.report.sizes_from_list += 1;
                        }
                        let full = c.full_path(&cl.path);
                        let row = Row {
                            digest: cl.digest,
                            prefixed_sha256: false,
                            size,
                            host: Cow::Borrowed(c.host.trim()),
                            path: &full,
                        };
                        imp.add_row(line_no, row, line)?
                    }
                    Err(m) => imp.malformed(line_no, m, line),
                },
            }
            let done = imp.report.rows_imported + imp.report.malformed_total();
            if done % 20_000 == 0
                && !progress(Progress {
                    bytes_done: counter.get(),
                    bytes_total: file_size,
                    rows: imp.report.rows_imported,
                })
            {
                return Err(Error::Cancelled);
            }
        }
        imp.report.line_endings = match lines.crlf() {
            Some(true) => "CRLF",
            Some(false) => "LF",
            None => "none",
        }
        .to_string();
        let mut report = imp.finish()?;
        report.elapsed_ms = started.elapsed().as_millis() as u64;
        let json = serde_json::to_string(&report)?;
        tx.execute(
            "UPDATE manifest SET rows = ?1, flagged = ?2, malformed = ?3, severity = ?4, report = ?5 WHERE id = ?6",
            params![
                report.rows_imported as i64,
                report.flagged_total() as i64,
                report.malformed_total() as i64,
                report.severity().code(),
                json,
                mid
            ],
        )?;
        tx.commit()?;
        progress(Progress { bytes_done: counter.get(), bytes_total: file_size, rows: report.rows_imported });
        Ok(report)
    }

    /// Remove a manifest and everything only it contributed; restores listings it was hiding.
    pub fn remove_manifest(&mut self, mid: i64) -> Result<RemoveSummary> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let exists: bool =
            tx.query_row("SELECT count(*) FROM manifest WHERE id = ?1", [mid], |r| r.get::<_, i64>(0))? > 0;
        if !exists {
            return err(format!("manifest {mid} does not exist"));
        }
        let sources: Vec<i64> = {
            let mut st = tx.prepare(
                "SELECT DISTINCT d.source_id FROM file f JOIN dir d ON d.id = f.dir_id WHERE f.manifest_id = ?1",
            )?;
            let v = st.query_map([mid], |r| r.get(0))?.collect::<rusqlite::Result<Vec<i64>>>()?;
            v
        };
        let mut summary = RemoveSummary::default();
        // Listings this manifest was hiding: per path, the best remaining listing wins
        // (SHA-256 first, then the newest manifest), counting listings that are already
        // visible; everything else stays hidden behind the winner.
        tx.execute_batch("DROP TABLE IF EXISTS temp.restore_rows; DROP TABLE IF EXISTS temp.restore_cand;")?;
        tx.execute(
            "CREATE TEMP TABLE restore_rows AS
             SELECT id, manifest_id, dir_id, ifnull(name_key, name) AS k, (algo_id = 0) AS sha, (size >= 0) AS sized
             FROM file WHERE shadowed_by = ?1 AND manifest_id <> ?1",
            [mid],
        )?;
        tx.execute(
            "CREATE TEMP TABLE restore_cand AS
             SELECT c.*, m.scan_time FROM (
                 SELECT id, manifest_id, dir_id, k, sha, sized, 1 AS restore FROM temp.restore_rows
                 UNION ALL
                 SELECT f.id, f.manifest_id, f.dir_id, ifnull(f.name_key, f.name), (f.algo_id = 0), (f.size >= 0), 0
                 FROM file f JOIN (SELECT DISTINCT dir_id, k FROM temp.restore_rows) rk
                   ON f.dir_id = rk.dir_id AND ifnull(f.name_key, f.name) = rk.k
                 WHERE f.shadowed_by IS NULL AND f.manifest_id <> ?1
             ) AS c JOIN manifest m ON m.id = c.manifest_id",
            [mid],
        )?;
        tx.execute_batch(
            "UPDATE file SET shadowed_by = CASE WHEN ranked.manifest_id = ranked.win THEN NULL ELSE ranked.win END
             FROM (SELECT id, manifest_id, restore,
                          first_value(manifest_id) OVER (PARTITION BY dir_id, k
                                                         ORDER BY sha DESC, sized DESC, scan_time DESC, manifest_id DESC) AS win
                   FROM temp.restore_cand) AS ranked
             WHERE file.id = ranked.id AND ranked.restore = 1;",
        )?;
        summary.rows_restored = tx.query_row(
            "SELECT count(*) FROM file WHERE id IN (SELECT id FROM temp.restore_rows) AND shadowed_by IS NULL",
            [],
            |r| r.get::<_, i64>(0),
        )? as u64;
        tx.execute_batch("DROP TABLE temp.restore_rows; DROP TABLE temp.restore_cand;")?;
        summary.rows_removed = tx.execute("DELETE FROM file WHERE manifest_id = ?1", [mid])? as u64;
        tx.execute("DELETE FROM manifest WHERE id = ?1", [mid])?;
        for s in sources {
            let (pruned, removed_host) = prune_source(&tx, s)?;
            summary.dirs_pruned += pruned;
            if let Some(h) = removed_host {
                summary.sources_removed.push(h);
            }
        }
        tx.commit()?;
        Ok(summary)
    }

    pub fn manifests(&self) -> Result<Vec<ManifestInfo>> {
        let mut st = self.conn.prepare(
            "SELECT id, file_name, file_path, file_size, imported_at, rows, flagged, malformed, severity, alert_dismissed
             FROM manifest ORDER BY id",
        )?;
        let rows = st
            .query_map([], |r| {
                Ok(ManifestInfo {
                    id: r.get(0)?,
                    file_name: r.get(1)?,
                    file_path: r.get(2)?,
                    file_size: r.get::<_, i64>(3)? as u64,
                    imported_at: r.get(4)?,
                    rows: r.get::<_, i64>(5)? as u64,
                    flagged: r.get::<_, i64>(6)? as u64,
                    malformed: r.get::<_, i64>(7)? as u64,
                    severity: Severity::from_code(r.get(8)?),
                    alert_dismissed: r.get::<_, i64>(9)? != 0,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The most recently imported manifest with this file name, if any.
    pub fn manifest_by_name(&self, file_name: &str) -> Result<Option<ManifestInfo>> {
        Ok(self.manifests()?.into_iter().rev().find(|m| m.file_name == file_name))
    }

    pub fn report(&self, mid: i64) -> Result<ImportReport> {
        let json: String = self.conn.query_row("SELECT report FROM manifest WHERE id = ?1", [mid], |r| r.get(0))?;
        Ok(serde_json::from_str(&json)?)
    }

    /// A stored setting (JSON), if present.
    pub fn setting<T: serde::de::DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        let v: Option<String> =
            self.conn.query_row("SELECT value FROM setting WHERE key = ?1", [key], |r| r.get(0)).optional()?;
        Ok(match v {
            Some(s) => Some(serde_json::from_str(&s)?),
            None => None,
        })
    }

    pub fn set_setting<T: serde::Serialize>(&self, key: &str, value: &T) -> Result<()> {
        let json = serde_json::to_string(value)?;
        self.conn.execute(
            "INSERT INTO setting(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, json],
        )?;
        Ok(())
    }

    /// Ignore rules: the stored ones (with any new built-in defaults added), or the defaults.
    pub fn ignore_settings(&self) -> Result<IgnoreSettings> {
        Ok(match self.setting::<IgnoreSettings>("ignore")? {
            Some(mut s) => {
                s.merge_new_defaults();
                s
            }
            None => IgnoreSettings::default(),
        })
    }

    pub fn set_ignore_settings(&self, s: &IgnoreSettings) -> Result<()> {
        self.set_setting("ignore", s)
    }

    /// The file-type categories for summaries (the built-in ones until edited).
    pub fn categories(&self) -> Result<crate::summary::Categories> {
        Ok(self.setting::<crate::summary::Categories>("categories")?.unwrap_or_default())
    }

    /// Store edited categories (cleaned up first).
    pub fn set_categories(&self, c: &crate::summary::Categories) -> Result<()> {
        self.set_setting("categories", &c.normalized())
    }

    pub fn set_alert_dismissed(&self, mid: i64, dismissed: bool) -> Result<()> {
        self.conn.execute("UPDATE manifest SET alert_dismissed = ?1 WHERE id = ?2", params![dismissed as i64, mid])?;
        Ok(())
    }

    pub fn sources(&self) -> Result<Vec<SourceRow>> {
        let mut st = self.conn.prepare("SELECT id, host, kind FROM source ORDER BY id")?;
        let rows = st
            .query_map([], |r| Ok(SourceRow { id: r.get(0)?, host: r.get(1)?, kind: PathKind::from_code(r.get(2)?) }))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Visible files directly inside a folder, ordered by id.
    pub fn visible_file_names(&self, dir_id: i64) -> Result<Vec<(i64, String)>> {
        let mut st = self
            .conn
            .prepare_cached("SELECT id, name FROM file WHERE dir_id = ?1 AND shadowed_by IS NULL ORDER BY id")?;
        let rows = st.query_map([dir_id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn file_record(&self, id: i64) -> Result<FileRecord> {
        let mut st = self.conn.prepare_cached(
            "SELECT f.id, f.manifest_id, f.line_no, f.dir_id, f.name, f.raw, f.size, a.name, f.digest, f.shadowed_by
             FROM file f JOIN algo a ON a.id = f.algo_id WHERE f.id = ?1",
        )?;
        let rec = st.query_row([id], |r| {
            let algo: String = r.get(7)?;
            let bytes: Vec<u8> = r.get(8)?;
            Ok((
                FileRecord {
                    id: r.get(0)?,
                    manifest_id: r.get(1)?,
                    line_no: r.get::<_, i64>(2)? as u64,
                    dir_id: r.get(3)?,
                    name: r.get(4)?,
                    raw: r.get(5)?,
                    size: u64::try_from(r.get::<_, i64>(6)?).ok(),
                    digest: Digest::Sha256([0; 32]),
                    shadowed_by: r.get(9)?,
                },
                algo,
                bytes,
            ))
        })?;
        let (mut fr, algo, bytes) = rec;
        fr.digest = if algo == "sha256" && bytes.len() == 32 {
            let mut d = [0u8; 32];
            d.copy_from_slice(&bytes);
            Digest::Sha256(d)
        } else {
            Digest::Other { algo, value: String::from_utf8_lossy(&bytes).into_owned() }
        };
        Ok(fr)
    }

    pub fn counts(&self) -> Result<DbCounts> {
        let q = |sql: &str| -> Result<u64> { Ok(self.conn.query_row(sql, [], |r| r.get::<_, i64>(0))? as u64) };
        Ok(DbCounts {
            manifests: q("SELECT count(*) FROM manifest")?,
            sources: q("SELECT count(*) FROM source")?,
            dirs: q("SELECT count(*) FROM dir")?,
            files_visible: q("SELECT count(*) FROM file WHERE shadowed_by IS NULL")?,
            files_hidden: q("SELECT count(*) FROM file WHERE shadowed_by IS NOT NULL")?,
        })
    }
}

/// Delete folders of a source that no longer contain any file row (visible or hidden), and the
/// source itself when nothing is left. Returns (folders deleted, host if the source was removed).
fn prune_source(tx: &Transaction, source_id: i64) -> Result<(u64, Option<String>)> {
    let dirs: Vec<(i64, Option<i64>)> = {
        let mut st = tx.prepare("SELECT id, parent_id FROM dir WHERE source_id = ?1")?;
        let v = st.query_map([source_id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        v
    };
    let parent: HashMap<i64, Option<i64>> = dirs.iter().copied().collect();
    let mut keep: HashSet<i64> = HashSet::new();
    {
        let mut has_file = tx.prepare_cached("SELECT EXISTS(SELECT 1 FROM file WHERE dir_id = ?1)")?;
        for &(id, _) in &dirs {
            if keep.contains(&id) {
                continue;
            }
            let used: bool = has_file.query_row([id], |r| r.get::<_, i64>(0))? != 0;
            if used {
                let mut cur = Some(id);
                while let Some(c) = cur {
                    if !keep.insert(c) {
                        break;
                    }
                    cur = parent.get(&c).copied().flatten();
                }
            }
        }
    }
    let mut deleted = 0u64;
    {
        let mut del = tx.prepare_cached("DELETE FROM dir WHERE id = ?1")?;
        for &(id, _) in &dirs {
            if !keep.contains(&id) {
                del.execute([id])?;
                deleted += 1;
            }
        }
    }
    if keep.is_empty() {
        let host: Option<String> =
            tx.query_row("SELECT host FROM source WHERE id = ?1", [source_id], |r| r.get(0)).optional()?;
        tx.execute("DELETE FROM source WHERE id = ?1", [source_id])?;
        return Ok((deleted, host));
    }
    Ok((deleted, None))
}

struct SourceState {
    id: i64,
    root: i64,
    kind: PathKind,
    mixed_reported: Vec<PathKind>,
}

/// Cached folder chain of the previous row: consecutive rows almost always share folders.
#[derive(Default)]
struct Chain {
    source: i64,
    keys: Vec<String>,
    ids: Vec<i64>,
}

struct Importer<'t> {
    tx: &'t Transaction<'t>,
    mid: i64,
    scan_time: i64,
    scan_times: HashMap<i64, i64>,
    sources: HashMap<String, SourceState>,
    dirs: HashMap<(i64, String), i64>,
    chain: Chain,
    algos: HashMap<String, i64>,
    exts: HashMap<String, i64>,
    overlaps: BTreeMap<i64, Overlap>,
    samples: Samples,
    report: ImportReport,
}

impl<'t> Importer<'t> {
    fn new(tx: &'t Transaction<'t>, mid: i64, scan_time: i64) -> Result<Self> {
        let mut algos = HashMap::new();
        {
            let mut st = tx.prepare("SELECT id, name FROM algo")?;
            for r in st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))? {
                let (id, n) = r?;
                algos.insert(n, id);
            }
        }
        let mut exts = HashMap::new();
        {
            let mut st = tx.prepare("SELECT id, name FROM ext")?;
            for r in st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))? {
                let (id, n) = r?;
                exts.insert(n, id);
            }
        }
        Ok(Importer {
            tx,
            mid,
            scan_time,
            scan_times: HashMap::new(),
            sources: HashMap::new(),
            dirs: HashMap::new(),
            chain: Chain::default(),
            algos,
            exts,
            overlaps: BTreeMap::new(),
            samples: Samples::default(),
            report: ImportReport::default(),
        })
    }

    fn malformed(&mut self, line_no: u64, m: Malformed, line: &[u8]) {
        *self.report.malformed.entry(m.label().to_string()).or_insert(0) += 1;
        self.samples.add(line_no, "malformed", m.label(), line);
    }

    /// Source id and root folder id for a host, creating them on first use.
    fn source(&mut self, host: &str, kind: PathKind, line_no: u64, line: &[u8]) -> Result<(i64, i64)> {
        if let Some(s) = self.sources.get_mut(host) {
            if s.kind != kind && !s.mixed_reported.contains(&kind) {
                s.mixed_reported.push(kind);
                self.report.mixed_kinds.push(format!(
                    "Source \"{host}\" mixes path styles ({} and {}); each path is still joined using its own style.",
                    s.kind.label(),
                    kind.label()
                ));
                self.samples.add(line_no, "mixed path styles", host, line);
            }
            return Ok((s.id, s.root));
        }
        let existing: Option<(i64, i64)> = self
            .tx
            .query_row("SELECT id, kind FROM source WHERE host = ?1", [host], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        let (id, root, src_kind) = match existing {
            Some((id, k)) => {
                let root: i64 =
                    self.tx.query_row("SELECT id FROM dir WHERE source_id = ?1 AND parent_id IS NULL", [id], |r| {
                        r.get(0)
                    })?;
                let mut st = self
                    .tx
                    .prepare("SELECT id, parent_id, ifnull(name_key, name) FROM dir WHERE source_id = ?1 AND parent_id IS NOT NULL")?;
                for r in
                    st.query_map([id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?)))?
                {
                    let (did, parent, key) = r?;
                    self.dirs.insert((parent, key), did);
                }
                (id, root, PathKind::from_code(k))
            }
            None => {
                self.tx.execute("INSERT INTO source(host, kind) VALUES (?1, ?2)", params![host, kind.code()])?;
                let id = self.tx.last_insert_rowid();
                self.tx.execute("INSERT INTO dir(source_id, parent_id, name) VALUES (?1, NULL, '')", [id])?;
                (id, self.tx.last_insert_rowid(), kind)
            }
        };
        self.sources.insert(host.to_string(), SourceState { id, root, kind: src_kind, mixed_reported: Vec::new() });
        if src_kind != kind {
            // pre-existing source with another style: report through the normal path
            return self.source(host, kind, line_no, line);
        }
        Ok((id, root))
    }

    fn dir(&mut self, source_id: i64, parent: i64, comp: &Comp, kind: PathKind) -> Result<i64> {
        let key = comp.key(kind).unwrap_or_else(|| comp.text.to_string());
        if let Some(&id) = self.dirs.get(&(parent, key.clone())) {
            return Ok(id);
        }
        let name_key: Option<&str> = if key != comp.text { Some(key.as_str()) } else { None };
        self.tx
            .prepare_cached("INSERT INTO dir(source_id, parent_id, name, name_key, raw) VALUES (?1, ?2, ?3, ?4, ?5)")?
            .execute(params![source_id, parent, comp.text.as_ref(), name_key, comp.raw])?;
        let id = self.tx.last_insert_rowid();
        self.dirs.insert((parent, key), id);
        Ok(id)
    }

    /// Folder id for anchor + folders, reusing the previous row's chain where it matches.
    fn folder(&mut self, source_id: i64, root: i64, kind: PathKind, comps: &[Comp]) -> Result<i64> {
        if self.chain.source != source_id {
            self.chain = Chain { source: source_id, keys: Vec::new(), ids: Vec::new() };
        }
        let mut parent = root;
        for (depth, c) in comps.iter().enumerate() {
            let key: Cow<str> = match c.key(kind) {
                Some(k) => Cow::Owned(k),
                None => Cow::Borrowed(c.text.as_ref()),
            };
            if depth < self.chain.keys.len() && self.chain.keys[depth] == key.as_ref() {
                parent = self.chain.ids[depth];
                continue;
            }
            self.chain.keys.truncate(depth);
            self.chain.ids.truncate(depth);
            let id = self.dir(source_id, parent, c, kind)?;
            self.chain.keys.push(key.into_owned());
            self.chain.ids.push(id);
            parent = id;
        }
        Ok(parent)
    }

    fn algo_id(&mut self, name: &str) -> Result<i64> {
        if let Some(&id) = self.algos.get(name) {
            return Ok(id);
        }
        self.tx.execute("INSERT INTO algo(name) VALUES (?1)", [name])?;
        let id = self.tx.last_insert_rowid();
        self.algos.insert(name.to_string(), id);
        Ok(id)
    }

    fn ext_id(&mut self, name: &str) -> Result<i64> {
        if let Some(&id) = self.exts.get(name) {
            return Ok(id);
        }
        self.tx.execute("INSERT INTO ext(name) VALUES (?1)", [name])?;
        let id = self.tx.last_insert_rowid();
        self.exts.insert(name.to_string(), id);
        Ok(id)
    }

    fn add_row(&mut self, line_no: u64, row: Row, line: &[u8]) -> Result<()> {
        let pp = paths::parse_path(row.path);
        if pp.parts.is_empty() {
            self.malformed(line_no, Malformed::NoFileName, line);
            return Ok(());
        }
        let kind = pp.kind;
        let (source_id, root) = self.source(&row.host, kind, line_no, line)?;
        let mut comps: Vec<Comp> = Vec::with_capacity(pp.parts.len());
        comps.push(Comp { text: pp.anchor.clone(), raw: None });
        comps.extend(pp.parts[..pp.parts.len() - 1].iter().cloned());
        let dir_id = self.folder(source_id, root, kind, &comps)?;
        let fname = &pp.parts[pp.parts.len() - 1];
        if pp.parts.iter().any(|c| c.raw.is_some()) {
            self.report.non_utf8_paths += 1;
            self.samples.add(line_no, "non-UTF-8 name", "", line);
        }
        let key = fname.key(kind);
        let key_ref: &str = key.as_deref().unwrap_or(fname.text.as_ref());
        let algo_id = self.algo_id(row.digest.algo_name())?;
        let ext_id = self.ext_id(&paths::extension(&fname.text))?;
        let is_sha = row.digest.is_sha256();
        let digest = row.digest.db_bytes();
        // -1: not known (a checksum list without a size list)
        let size = row.size.map(|s| s as i64).unwrap_or(-1);

        // Other visible listings of the same path decide which one is shown.
        let existing: Vec<(i64, i64, i64, i64, Vec<u8>)> = {
            let mut st = self.tx.prepare_cached(
                "SELECT id, manifest_id, size, algo_id, digest FROM file
                 WHERE dir_id = ?1 AND ifnull(name_key, name) = ?2 AND shadowed_by IS NULL",
            )?;
            let v = st
                .query_map(params![dir_id, key_ref], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            v
        };
        let mut shadowed_by: Option<i64> = None;
        for (eid, emid, esize, ealgo, edigest) in existing {
            if emid == self.mid {
                if kind == PathKind::Cloud {
                    self.report.same_path_cloud += 1;
                    self.samples.add(line_no, "same path", "kept as separate cloud files", line);
                } else {
                    self.report.duplicate_listings += 1;
                    self.samples.add(line_no, "duplicate listing", "hidden", line);
                    shadowed_by = Some(self.mid);
                }
                continue;
            }
            let ov = self.overlaps.entry(emid).or_insert_with(|| Overlap { manifest_id: emid, ..Default::default() });
            ov.paths += 1;
            let same_size = esize == size || esize < 0 || size < 0;
            if !(same_size && ealgo == algo_id && edigest == digest) {
                ov.changed += 1;
            }
            let e_scan = match self.scan_times.get(&emid) {
                Some(&t) => t,
                None => {
                    let t: i64 =
                        self.tx.query_row("SELECT scan_time FROM manifest WHERE id = ?1", [emid], |r| r.get(0))?;
                    self.scan_times.insert(emid, t);
                    t
                }
            };
            let e_sha = ealgo == ALGO_SHA256;
            // SHA-256 first, then a listing with a size, then the newer scan
            let (sized, e_sized) = (size >= 0, esize >= 0);
            let new_wins = (is_sha && !e_sha)
                || (is_sha == e_sha && ((sized && !e_sized) || (sized == e_sized && self.scan_time >= e_scan)));
            let ov = self.overlaps.get_mut(&emid).expect("overlap entry");
            if new_wins {
                self.tx
                    .prepare_cached("UPDATE file SET shadowed_by = ?1 WHERE id = ?2")?
                    .execute(params![self.mid, eid])?;
                ov.this_shown += 1;
            } else {
                shadowed_by = Some(emid);
                ov.other_shown += 1;
            }
            if ov.paths <= Samples::CAP as u64 {
                self.samples.add(line_no, "overlap", "path also in another manifest", line);
            }
        }

        let name_key: Option<&str> = key.as_deref();
        self.tx
            .prepare_cached(
                "INSERT INTO file(manifest_id, line_no, dir_id, name, name_key, raw, size, algo_id, digest, ext_id, shadowed_by)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )?
            .execute(params![
                self.mid,
                line_no as i64,
                dir_id,
                fname.text.as_ref(),
                name_key,
                fname.raw,
                size,
                algo_id,
                digest,
                ext_id,
                shadowed_by
            ])?;

        self.report.rows_imported += 1;
        match row.size {
            Some(s) => self.report.bytes_total += s,
            None => self.report.rows_unknown_size += 1,
        }
        *self.report.hosts.entry(row.host.into_owned()).or_insert(0) += 1;
        if is_sha {
            self.report.rows_sha256 += 1;
            if row.prefixed_sha256 {
                self.report.sha256_prefixed += 1;
            }
        } else {
            let a = row.digest.algo_name().to_string();
            self.samples.add(line_no, "flagged", &a, line);
            *self.report.flagged.entry(a).or_insert(0) += 1;
        }
        Ok(())
    }

    fn finish(mut self) -> Result<ImportReport> {
        let mut overlaps: Vec<Overlap> = std::mem::take(&mut self.overlaps).into_values().collect();
        for o in &mut overlaps {
            o.file_name = self
                .tx
                .query_row("SELECT file_name FROM manifest WHERE id = ?1", [o.manifest_id], |r| r.get(0))
                .optional()?
                .unwrap_or_else(|| format!("manifest #{}", o.manifest_id));
        }
        overlaps.sort_by_key(|o| std::cmp::Reverse(o.paths));
        self.report.overlaps = overlaps;
        self.report.samples = std::mem::take(&mut self.samples.list);
        Ok(self.report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::HEADER;

    pub(crate) const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    pub(crate) const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    pub(crate) const C: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

    fn import(db: &mut Db, name: &str, body: &str) -> ImportReport {
        let text = format!("{HEADER}\r\n{body}");
        db.import_reader(name, name, text.len() as u64, None, text.as_bytes(), &mut |_| true).unwrap()
    }

    fn visible(db: &Db) -> u64 {
        db.counts().unwrap().files_visible
    }

    #[test]
    fn basic_import_builds_sources_and_folders() {
        let mut db = Db::open_in_memory().unwrap();
        let r = import(
            &mut db,
            "manifest-PC1.tsv",
            &format!(
                "{A}\t10\tPC1 D: (Media)\tD:\\Videos\\a.mkv\r\n{B}\t20\tPC1 D: (Media)\tD:\\Videos\\Sub\\b.mkv\r\n{C}\t30\tPC1 C: (OS)\tC:\\Users\\U\\c.iso\r\n"
            ),
        );
        assert!(r.header_found);
        assert_eq!(r.rows_imported, 3);
        assert_eq!(r.rows_sha256, 3);
        assert_eq!(r.line_endings, "CRLF");
        assert_eq!(r.severity(), Severity::Ok);
        let c = db.counts().unwrap();
        assert_eq!(c.sources, 2);
        // roots(2) + D: + Videos + Sub + C: + Users + U
        assert_eq!(c.dirs, 8);
        assert_eq!(c.files_visible, 3);
        assert_eq!(r.hosts.get("PC1 D: (Media)"), Some(&2));
    }

    #[test]
    fn windows_folders_merge_case_insensitively_posix_do_not() {
        let mut db = Db::open_in_memory().unwrap();
        import(&mut db, "w.tsv", &format!("{A}\t1\tPC\tD:\\Photos\\x.jpg\r\n{B}\t1\tPC\td:\\photos\\y.jpg\r\n"));
        import(&mut db, "l.tsv", &format!("{A}\t1\tnas\t/mnt/Photos/x.jpg\r\n{B}\t1\tnas\t/mnt/photos/y.jpg\r\n"));
        let n: i64 = db
            .conn()
            .query_row("SELECT count(*) FROM dir WHERE name IN ('Photos','photos')", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 3, "one merged Windows folder + two distinct Linux folders");
    }

    #[test]
    fn duplicate_listing_in_one_manifest_is_hidden() {
        let mut db = Db::open_in_memory().unwrap();
        // the same file reached twice (e.g. -Paths D:\,D:\Photos)
        let r =
            import(&mut db, "m.tsv", &format!("{A}\t5\tPC\tD:\\Photos\\x.jpg\r\n{A}\t5\tPC\tD:\\photos\\X.JPG\r\n"));
        assert_eq!(r.duplicate_listings, 1);
        assert_eq!(r.rows_imported, 2);
        assert_eq!(visible(&db), 1);
        assert_eq!(r.severity(), Severity::Notice);
    }

    #[test]
    fn same_path_on_cloud_is_kept() {
        let mut db = Db::open_in_memory().unwrap();
        let r = import(
            &mut db,
            "g.tsv",
            &format!("{A}\t5\tgdrive:\tgdrive:/a/x.jpg\r\n{B}\t6\tgdrive:\tgdrive:/a/x.jpg\r\n"),
        );
        assert_eq!(r.same_path_cloud, 1);
        assert_eq!(visible(&db), 2);
    }

    #[test]
    fn newer_manifest_wins_and_removal_restores() {
        let mut db = Db::open_in_memory().unwrap();
        import(
            &mut db,
            "manifest-PC2.tsv",
            &format!("{A}\t5\tPC2 E: (USB)\tE:\\b\\x.zip\r\n{B}\t6\tPC2 E: (USB)\tE:\\b\\y.zip\r\n"),
        );
        let r = import(
            &mut db,
            "manifest-PC2-usb.tsv",
            &format!("{A}\t5\tPC2 E: (USB)\tE:\\b\\x.zip\r\n{C}\t7\tPC2 E: (USB)\tE:\\b\\y.zip\r\n"),
        );
        assert_eq!(r.overlaps.len(), 1);
        let o = &r.overlaps[0];
        assert_eq!((o.paths, o.changed, o.this_shown, o.other_shown), (2, 1, 2, 0));
        assert_eq!(o.file_name, "manifest-PC2.tsv");
        assert_eq!(visible(&db), 2);
        let shown: Vec<i64> = {
            let mut st =
                db.conn().prepare("SELECT manifest_id FROM file WHERE shadowed_by IS NULL ORDER BY id").unwrap();
            st.query_map([], |r| r.get(0)).unwrap().map(|x| x.unwrap()).collect()
        };
        assert_eq!(shown, vec![2, 2]);
        let s = db.remove_manifest(2).unwrap();
        assert_eq!(s.rows_removed, 2);
        assert_eq!(s.rows_restored, 2);
        assert_eq!(visible(&db), 2);
        assert_eq!(db.counts().unwrap().files_hidden, 0);
    }

    #[test]
    fn newer_scan_wins_even_when_imported_first() {
        let mut db = Db::open_in_memory().unwrap();
        let t_new = format!("{HEADER}\r\n{B}\t6\tPC\tD:\\x.bin\r\n");
        let t_old = format!("{HEADER}\r\n{A}\t5\tPC\tD:\\x.bin\r\n");
        db.import_reader("new.tsv", "new.tsv", 0, Some(2_000_000_000), t_new.as_bytes(), &mut |_| true).unwrap();
        let r =
            db.import_reader("old.tsv", "old.tsv", 0, Some(1_000_000_000), t_old.as_bytes(), &mut |_| true).unwrap();
        assert_eq!(r.overlaps[0].other_shown, 1);
        assert_eq!(r.overlaps[0].changed, 1);
        let shown: i64 =
            db.conn().query_row("SELECT manifest_id FROM file WHERE shadowed_by IS NULL", [], |r| r.get(0)).unwrap();
        assert_eq!(shown, 1);
        // removing the newer scan shows the older one again
        db.remove_manifest(1).unwrap();
        let shown: i64 =
            db.conn().query_row("SELECT manifest_id FROM file WHERE shadowed_by IS NULL", [], |r| r.get(0)).unwrap();
        assert_eq!(shown, 2);
    }

    #[test]
    fn sha256_listing_beats_a_newer_flagged_one() {
        let mut db = Db::open_in_memory().unwrap();
        import(&mut db, "od-sha.tsv", &format!("{A}\t5\tonedrive:\tonedrive:/Docs/a.zip\r\n"));
        let r = import(
            &mut db,
            "od-qx.tsv",
            "quickxor:AAAAAAAAAAAAAAAAAAAAAAAAAAA=\t5\tonedrive:\tonedrive:/Docs/a.zip\r\n",
        );
        assert_eq!(r.flagged.get("quickxor"), Some(&1));
        assert_eq!(r.overlaps[0].other_shown, 1);
        assert_eq!(r.severity(), Severity::Alert);
        let algo: i64 =
            db.conn().query_row("SELECT algo_id FROM file WHERE shadowed_by IS NULL", [], |r| r.get(0)).unwrap();
        assert_eq!(algo, ALGO_SHA256);
        // and removing the SHA-256 manifest brings the flagged listing back
        db.remove_manifest(1).unwrap();
        let algo: i64 =
            db.conn().query_row("SELECT algo_id FROM file WHERE shadowed_by IS NULL", [], |r| r.get(0)).unwrap();
        assert_ne!(algo, ALGO_SHA256);
    }

    #[test]
    fn removal_with_three_overlapping_manifests() {
        let mut db = Db::open_in_memory().unwrap();
        for (i, h) in [A, B, C].iter().enumerate() {
            import(&mut db, &format!("m{i}.tsv"), &format!("{h}\t5\tPC\tD:\\x.bin\r\n"));
        }
        // m2 (newest) shown; removing the middle one must keep m2 shown and m0 hidden behind it
        db.remove_manifest(2).unwrap();
        let rows: Vec<(i64, Option<i64>)> = {
            let mut st = db.conn().prepare("SELECT manifest_id, shadowed_by FROM file ORDER BY manifest_id").unwrap();
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(|x| x.unwrap()).collect()
        };
        assert_eq!(rows, vec![(1, Some(3)), (3, None)]);
        db.remove_manifest(3).unwrap();
        let rows: Vec<(i64, Option<i64>)> = {
            let mut st = db.conn().prepare("SELECT manifest_id, shadowed_by FROM file").unwrap();
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(|x| x.unwrap()).collect()
        };
        assert_eq!(rows, vec![(1, None)]);
    }

    #[test]
    fn removal_prunes_folders_and_sources() {
        let mut db = Db::open_in_memory().unwrap();
        import(&mut db, "a.tsv", &format!("{A}\t5\tPC\tD:\\keep\\x.bin\r\n"));
        import(&mut db, "b.tsv", &format!("{B}\t5\tPC\tD:\\gone\\deep\\y.bin\r\n{C}\t5\tnas\t/mnt/z.bin\r\n"));
        let before = db.counts().unwrap();
        let s = db.remove_manifest(2).unwrap();
        assert_eq!(s.sources_removed, vec!["nas".to_string()]);
        let after = db.counts().unwrap();
        assert_eq!(after.sources, 1);
        // gone + deep removed from PC; root, /, mnt removed with nas
        assert_eq!(before.dirs - after.dirs, 5);
        assert_eq!(s.dirs_pruned, 5);
    }

    #[test]
    fn malformed_and_flagged_rows_are_reported_with_samples() {
        let mut db = Db::open_in_memory().unwrap();
        let body = format!(
            "{A}\t5\th\t/ok.bin\r\nonly\ttwo\r\n{A}\tabc\th\t/x\r\n\t5\th\t/x\r\nmd5:0123456789abcdef0123456789abcdef\t5\th\t/m.bin\r\n{A}\t5\th\t/\r\n\r\n"
        );
        let r = import(&mut db, "bad.tsv", &body);
        assert_eq!(r.rows_imported, 2);
        assert_eq!(r.flagged.get("md5"), Some(&1));
        assert_eq!(r.malformed.get("missing columns"), Some(&1));
        assert_eq!(r.malformed.get("size is not a number"), Some(&1));
        assert_eq!(r.malformed.get("empty hash"), Some(&1));
        assert_eq!(r.malformed.get("path has no file name"), Some(&1));
        assert_eq!(r.blank_lines, 1);
        assert_eq!(r.severity(), Severity::Alert);
        let lines: Vec<u64> = r.samples.iter().filter(|s| s.category == "malformed").map(|s| s.line).collect();
        assert_eq!(lines, vec![3, 4, 5, 7]);
        // persisted and reloaded
        let back = db.report(r.manifest_id).unwrap();
        assert_eq!(back, r);
        let m = &db.manifests().unwrap()[0];
        assert_eq!((m.flagged, m.malformed, m.severity), (1, 4, Severity::Alert));
    }

    #[test]
    fn missing_header_is_an_alert_but_rows_still_import() {
        let mut db = Db::open_in_memory().unwrap();
        let text = format!("{A}\t5\th\t/a.bin\n");
        let r = db.import_reader("x.tsv", "x.tsv", 0, None, text.as_bytes(), &mut |_| true).unwrap();
        assert!(!r.header_found);
        assert_eq!(r.rows_imported, 1);
        assert_eq!(r.line_endings, "LF");
        assert_eq!(r.severity(), Severity::Alert);
    }

    #[test]
    fn non_utf8_names_are_kept_exactly() {
        let mut db = Db::open_in_memory().unwrap();
        let mut body = format!("{HEADER}\n").into_bytes();
        body.extend_from_slice(format!("{A}\t5\tnas\t/mnt/caf").as_bytes());
        body.extend_from_slice(b"\xe9/men\xfa.pdf\n");
        body.extend_from_slice(format!("{B}\t5\tnas\t/mnt/caf").as_bytes());
        body.extend_from_slice(b"\xe8/men\xfa.pdf\n");
        let r = db.import_reader("n.tsv", "n.tsv", 0, None, &body[..], &mut |_| true).unwrap();
        assert_eq!(r.non_utf8_paths, 2);
        assert_eq!(visible(&db), 2, "different bytes are different folders");
        let raw: Vec<u8> = db.conn().query_row("SELECT raw FROM file LIMIT 1", [], |r| r.get(0)).unwrap();
        assert_eq!(raw, b"men\xfa.pdf");
    }

    #[test]
    fn cancel_rolls_back() {
        let mut db = Db::open_in_memory().unwrap();
        let mut body = String::from(HEADER);
        body.push('\n');
        for i in 0..50_000 {
            body.push_str(&format!("{A}\t{i}\th\t/d/{i}.bin\n"));
        }
        let res = db.import_reader("big.tsv", "big.tsv", body.len() as u64, None, body.as_bytes(), &mut |_| false);
        assert!(matches!(res, Err(Error::Cancelled)));
        assert_eq!(db.counts().unwrap(), DbCounts::default());
    }

    #[test]
    fn reopen_file_database() {
        let dir = std::env::temp_dir().join(format!("dupemap-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.dupemap");
        let _ = std::fs::remove_file(&p);
        {
            let mut db = Db::open(&p).unwrap();
            import(&mut db, "a.tsv", &format!("{A}\t5\tPC\tD:\\x.bin\r\n"));
        }
        let db = Db::open(&p).unwrap();
        assert_eq!(db.counts().unwrap().files_visible, 1);
        assert_eq!(db.manifests().unwrap()[0].file_name, "a.tsv");
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn older_schema_is_upgraded_and_newer_refused() {
        let dir = std::env::temp_dir().join(format!("dupemap-upgrade-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("old.dupemap");
        let _ = std::fs::remove_file(&p);
        {
            let mut db = Db::open(&p).unwrap();
            import(&mut db, "a.tsv", &format!("{A}\t5\tPC\tD:\\x.bin\r\n"));
            // what a Phase 1 database looked like: schema 1, no settings table
            db.conn
                .execute_batch("DROP TABLE setting; UPDATE meta SET value = '1' WHERE key = 'schema_version';")
                .unwrap();
        }
        {
            let db = Db::open(&p).unwrap();
            assert_eq!(db.counts().unwrap().files_visible, 1, "data kept");
            db.set_setting("k", &5).unwrap();
            assert_eq!(db.setting::<i32>("k").unwrap(), Some(5), "settings table created");
            let v: String =
                db.conn.query_row("SELECT value FROM meta WHERE key='schema_version'", [], |r| r.get(0)).unwrap();
            assert_eq!(v, SCHEMA_VERSION.to_string());
            db.conn.execute("UPDATE meta SET value = '99' WHERE key = 'schema_version'", []).unwrap();
        }
        assert!(Db::open(&p).is_err(), "a database from a newer DupeMap is not touched");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn import_checksums(db: &mut Db, name: &str, body: &str, host: &str, base: &str) -> ImportReport {
        let how = ImportAs::Checksums(ChecksumImport { host: host.into(), base: base.into(), sizes: None });
        db.import_reader_as(name, name, body.len() as u64, None, body.as_bytes(), &how, &mut |_| true).unwrap()
    }

    fn size_of(db: &Db, name: &str) -> Vec<i64> {
        let mut st =
            db.conn.prepare("SELECT size FROM file WHERE name = ?1 AND shadowed_by IS NULL ORDER BY id").unwrap();
        st.query_map([name], |r| r.get(0)).unwrap().collect::<rusqlite::Result<Vec<i64>>>().unwrap()
    }

    #[test]
    fn checksum_list_rows_have_no_size_and_the_given_source() {
        let mut db = Db::open_in_memory().unwrap();
        let body = format!("{A}  Docs/a.pdf\n{B} *b.bin\nERROR                  broken.bin\nnot a line\n");
        let r = import_checksums(&mut db, "onedrive-hashsum.txt", &body, "onedrive:", "onedrive:");
        assert_eq!(r.rows_imported, 2);
        assert_eq!(r.rows_unknown_size, 2);
        assert_eq!(r.malformed.get("no hash (rclone could not read the file)"), Some(&1));
        assert_eq!(r.malformed.get("not a checksum line"), Some(&1));
        assert!(r.is_checksum_list());
        assert!(!r.header_found);
        // malformed lines are an alert; a clean checksum list is only a notice (no sizes)
        assert_eq!(r.severity(), Severity::Alert);
        assert!(r.findings().iter().all(|f| !f.contains("header line")), "no header is expected");
        let clean = import_checksums(&mut db, "clean.txt", &format!("{C}  c.bin\n"), "onedrive:", "onedrive:");
        assert_eq!(clean.severity(), Severity::Notice);
        assert_eq!(size_of(&db, "a.pdf"), [-1]);
        let srcs = db.sources().unwrap();
        assert_eq!(srcs.len(), 1);
        assert_eq!(srcs[0].host, "onedrive:");
        assert_eq!(srcs[0].kind, PathKind::Cloud);
        let rec = db.file_record(1).unwrap();
        assert_eq!(rec.size, None);
    }

    #[test]
    fn a_size_list_fills_in_sizes() {
        let dir = std::env::temp_dir().join(format!("dupemap-sizes-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sizes = dir.join("onedrive-sizes.tsv");
        // what rclone lsf --format sp --separator "\t" writes (the script saves it with a BOM)
        std::fs::write(&sizes, "\u{FEFF}1048576\tDocs/a.pdf\r\n2000000\tother.bin\r\n").unwrap();
        let mut db = Db::open_in_memory().unwrap();
        let how = ImportAs::Checksums(ChecksumImport {
            host: "onedrive:".into(),
            base: "onedrive:".into(),
            sizes: Some(sizes.clone()),
        });
        let body = format!("{A}  Docs/a.pdf\n{B}  b.bin\n");
        let r = db.import_reader_as("x.txt", "x.txt", 0, None, body.as_bytes(), &how, &mut |_| true).unwrap();
        assert_eq!(r.size_list_entries, Some(2));
        assert_eq!(r.sizes_from_list, 1);
        assert_eq!(r.rows_unknown_size, 1);
        assert_eq!(r.bytes_total, 1048576);
        assert_eq!(size_of(&db, "a.pdf"), [1048576]);
        assert_eq!(size_of(&db, "b.bin"), [-1]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_listing_with_a_size_beats_one_without() {
        let mut db = Db::open_in_memory().unwrap();
        // the manifest with sizes is older, but it still wins over the newer checksum list
        let t_old = format!("{HEADER}\n{A}\t7\tonedrive:\tonedrive:/Docs/a.pdf\n");
        db.import_reader("m.tsv", "m.tsv", 0, Some(1_000), t_old.as_bytes(), &mut |_| true).unwrap();
        let how =
            ImportAs::Checksums(ChecksumImport { host: "onedrive:".into(), base: "onedrive:".into(), sizes: None });
        let body = format!("{A}  Docs/a.pdf\n");
        let r = db.import_reader_as("c.txt", "c.txt", 0, Some(2_000), body.as_bytes(), &how, &mut |_| true).unwrap();
        assert_eq!(r.overlaps.len(), 1);
        assert_eq!((r.overlaps[0].changed, r.overlaps[0].this_shown, r.overlaps[0].other_shown), (0, 0, 1));
        assert_eq!(size_of(&db, "a.pdf"), [7]);
        // removing the manifest shows the checksum listing again
        db.remove_manifest(1).unwrap();
        assert_eq!(size_of(&db, "a.pdf"), [-1]);
        // and importing it again hides the checksum listing once more
        db.import_reader("m2.tsv", "m2.tsv", 0, Some(1_000), t_old.as_bytes(), &mut |_| true).unwrap();
        assert_eq!(size_of(&db, "a.pdf"), [7]);
        // removal ranking: with three listings, the sized one is restored first
        let t3 = format!("{HEADER}\n{A}\t7\tonedrive:\tonedrive:/Docs/a.pdf\n");
        db.import_reader("m3.tsv", "m3.tsv", 0, Some(3_000), t3.as_bytes(), &mut |_| true).unwrap();
        let m3 = db.manifest_by_name("m3.tsv").unwrap().unwrap().id;
        db.remove_manifest(m3).unwrap();
        assert_eq!(size_of(&db, "a.pdf"), [7], "m2 (sized) is shown again, not the checksum list");
    }

    #[test]
    fn checksum_paths_are_joined_to_the_base() {
        let c = |base: &str, p: &str| {
            let ci = ChecksumImport { host: "h".into(), base: base.into(), sizes: None };
            String::from_utf8(ci.full_path(p.as_bytes()).into_owned()).unwrap()
        };
        assert_eq!(c("onedrive:", "Docs/a.pdf"), "onedrive:Docs/a.pdf");
        assert_eq!(c("onedrive:/", "./Docs/a.pdf"), "onedrive:/Docs/a.pdf");
        assert_eq!(c("/mnt/tank", "Docs/a.pdf"), "/mnt/tank/Docs/a.pdf");
        assert_eq!(c("/mnt/tank/", "/abs/x"), "/abs/x", "absolute paths stay");
        assert_eq!(c(r"D:\Photos", "2019/a.jpg"), r"D:\Photos\2019\a.jpg");
        assert_eq!(c(r"\\nas\share", "a/b.jpg"), r"\\nas\share\a\b.jpg");
        assert_eq!(c("", "Docs/a.pdf"), "Docs/a.pdf");
        assert_eq!(c("x:", "gdrive:a.pdf"), "gdrive:a.pdf", "a remote path stays");
    }

    #[test]
    fn a_checksum_list_needs_a_source() {
        let mut db = Db::open_in_memory().unwrap();
        let how = ImportAs::Checksums(ChecksumImport { host: " ".into(), base: String::new(), sizes: None });
        assert!(db.import_reader_as("x", "x", 0, None, &b""[..], &how, &mut |_| true).is_err());
    }

    #[test]
    fn a_second_connection_waits_for_a_writer() {
        let dir = std::env::temp_dir().join(format!("dupemap-busy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("busy.dupemap");
        let _ = std::fs::remove_file(&p);
        let writer = Db::open(&p).unwrap();
        writer.conn.execute_batch("BEGIN IMMEDIATE; INSERT INTO setting(key, value) VALUES ('x', '1');").unwrap();
        let p2 = p.clone();
        let reader = std::thread::spawn(move || {
            // opening and reading while another connection holds the write lock must work
            let db = Db::open(&p2).unwrap();
            db.manifests().unwrap().len()
        });
        std::thread::sleep(std::time::Duration::from_millis(300));
        writer.conn.execute_batch("COMMIT;").unwrap();
        assert_eq!(reader.join().unwrap(), 0);
        drop(writer);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
