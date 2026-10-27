//! The file a session (tabs, layout, selection…) is autosaved to, written so that a crash or a
//! power cut never leaves a half-written file behind.
//!
//! A save writes `<name>.session.json.tmp`, flushes it to disk and renames it over the real
//! file (an atomic replace). The file it replaces is first copied to `….bak` (at most once per
//! [`BACKUP_EVERY`]), so a file that is damaged for any other reason still has a recent
//! predecessor. Reading returns the candidates, newest first; the caller keeps the first one it
//! can parse.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How often the previous session file is kept as the backup.
pub const BACKUP_EVERY: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct SessionFile {
    path: PathBuf,
}

impl SessionFile {
    /// The session file that belongs to a database: `dupemap.db` → `dupemap.session.json` next
    /// to it (each database keeps its own session).
    pub fn beside(db: &Path) -> Self {
        let mut name = db.file_stem().map(|s| s.to_os_string()).unwrap_or_else(|| "dupemap".into());
        name.push(".session.json");
        SessionFile { path: db.with_file_name(name) }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn with_suffix(&self, suffix: &str) -> PathBuf {
        let mut s = self.path.clone().into_os_string();
        s.push(suffix);
        PathBuf::from(s)
    }

    fn backup_path(&self) -> PathBuf {
        self.with_suffix(".bak")
    }

    fn temp_path(&self) -> PathBuf {
        self.with_suffix(".tmp")
    }

    /// The saved texts that exist, newest first: the session file, then its backup. A leftover
    /// temporary file (an interrupted save) is never read.
    pub fn read(&self) -> Vec<String> {
        [self.path.clone(), self.backup_path()]
            .into_iter()
            .filter_map(|p| fs::read_to_string(p).ok())
            .filter(|t| !t.trim().is_empty())
            .collect()
    }

    /// Replace the session file with `text`.
    pub fn write(&self, text: &str) -> io::Result<()> {
        if let Some(dir) = self.path.parent() {
            if !dir.as_os_str().is_empty() {
                fs::create_dir_all(dir)?;
            }
        }
        let tmp = self.temp_path();
        let result = (|| {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
            drop(f);
            self.keep_backup();
            fs::rename(&tmp, &self.path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }

    /// Copy the current file to the backup unless the backup is recent. Best effort.
    fn keep_backup(&self) {
        let bak = self.backup_path();
        let recent = fs::metadata(&bak)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age < BACKUP_EVERY);
        if !recent && self.path.exists() {
            let _ = fs::copy(&self.path, &bak);
        }
    }

    /// Delete the session file, its backup and any leftover temporary file.
    pub fn remove(&self) {
        for p in [self.path.clone(), self.backup_path(), self.temp_path()] {
            let _ = fs::remove_file(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dupemap-session-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn named_after_the_database() {
        let f = SessionFile::beside(Path::new("/x/y/dupemap.db"));
        assert_eq!(f.path(), Path::new("/x/y/dupemap.session.json"));
        let g = SessionFile::beside(Path::new("other"));
        assert_eq!(g.path(), Path::new("other.session.json"));
    }

    #[test]
    fn write_then_read() {
        let d = dir("rw");
        let f = SessionFile::beside(&d.join("a.db"));
        assert!(f.read().is_empty());
        f.write("{\"n\":1}").unwrap();
        f.write("{\"n\":2}").unwrap();
        let got = f.read();
        assert_eq!(got[0], "{\"n\":2}");
        // the first save became the backup when the second replaced it
        assert_eq!(got[1], "{\"n\":1}");
        assert!(!f.temp_path().exists());
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn damaged_file_falls_back_to_the_backup() {
        let d = dir("bak");
        let f = SessionFile::beside(&d.join("a.db"));
        f.write("{\"n\":1}").unwrap();
        f.write("{\"n\":2}").unwrap();
        // a stale backup: age it past BACKUP_EVERY by writing it directly, then break the main file
        fs::write(f.backup_path(), "{\"n\":1}").unwrap();
        fs::write(f.path(), "{\"n\":").unwrap();
        let got = f.read();
        assert_eq!(got.len(), 2);
        assert_eq!(got[1], "{\"n\":1}");
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn interrupted_save_leaves_the_old_file() {
        let d = dir("torn");
        let f = SessionFile::beside(&d.join("a.db"));
        f.write("{\"n\":1}").unwrap();
        // a save that died after writing part of the temporary file
        fs::write(f.temp_path(), "{\"n\":9").unwrap();
        assert_eq!(f.read()[0], "{\"n\":1}");
        // the next save replaces the leftover
        f.write("{\"n\":3}").unwrap();
        assert_eq!(f.read()[0], "{\"n\":3}");
        assert!(!f.temp_path().exists());
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn remove_deletes_everything() {
        let d = dir("rm");
        let f = SessionFile::beside(&d.join("a.db"));
        f.write("{}").unwrap();
        f.write("{\"a\":1}").unwrap();
        f.remove();
        assert!(f.read().is_empty());
        let _ = fs::remove_dir_all(d);
    }
}
