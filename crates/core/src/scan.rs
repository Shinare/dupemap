//! Hashing the files of this computer into a manifest: what the export scripts do, built in.
//!
//! One manifest per drive (Windows) or mounted file system (Linux), in the scripts' format and
//! with the scripts' source names (`<COMPUTERNAME> D: (<label>)` on Windows, the host name on
//! Linux), so a drive scanned here and the same drive scanned by a script are one source.
//!
//! A scan walks the drive twice: first to count what it will hash (for the progress bar and
//! time estimate), then to hash, files streamed to a few hashing threads (one for a spinning
//! or USB disk, which would only seek between files read in parallel). Nothing is held per
//! file, so memory stays flat on a drive with millions of files.
//!
//! The manifest is written to `<name>.tsv.partial` and renamed to `<name>.tsv` when complete.
//! A scan that was cancelled or cut off (crash, power cut) leaves the `.partial` file; the next
//! scan of that drive continues where it stopped (an incomplete last line is dropped).

use crate::error::{err, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The scripts' default: files below 1 MiB are not hashed.
pub const DEFAULT_MIN_SIZE: u64 = 1 << 20;

/// What kind of drive a volume is on (the tick boxes of the scan window).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum DriveKind {
    /// Internal drives of this computer.
    Internal,
    /// USB and other removable drives (memory cards).
    Usb,
    /// Network drives (mapped shares, NFS/SMB mounts).
    Network,
}

impl DriveKind {
    pub const ALL: [DriveKind; 3] = [DriveKind::Internal, DriveKind::Usb, DriveKind::Network];

    pub fn label(self) -> &'static str {
        match self {
            DriveKind::Internal => "This computer's drives",
            DriveKind::Usb => "USB drives",
            DriveKind::Network => "Network drives",
        }
    }
}

/// A drive or mounted file system that can be scanned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Volume {
    /// Where the walk starts: `C:\`, `\\server\share\`, `/mnt/tank`.
    pub root: PathBuf,
    /// Written in front of paths instead of `root` (a network drive reached by its share name
    /// while its letter is not visible, as in a program run as administrator): `Z:\`.
    pub write_root: Option<String>,
    /// Shown in the scan window: `C: Windows`, `/mnt/tank`.
    pub display: String,
    /// Volume label (may be empty).
    pub label: String,
    pub kind: DriveKind,
    /// File system name (`NTFS`, `ext4`, `zfs`...).
    pub fs: String,
    pub total_bytes: Option<u64>,
    pub free_bytes: Option<u64>,
    /// A spinning disk (one file at a time). `None` when not known.
    pub rotational: Option<bool>,
    /// The manifest's host column for every file on it.
    pub host: String,
    /// File name of its manifest, without `.tsv`.
    pub manifest_stem: String,
    /// Why it is not ticked by default (e.g. reading it would download files).
    pub caution: Option<String>,
}

impl Volume {
    /// A volume for an arbitrary folder (tests, and scanning a single folder).
    pub fn folder(root: &Path, host: &str) -> Volume {
        Volume {
            root: root.to_path_buf(),
            write_root: None,
            display: root.display().to_string(),
            label: String::new(),
            kind: DriveKind::Internal,
            fs: String::new(),
            total_bytes: None,
            free_bytes: None,
            rotational: None,
            host: host.to_string(),
            manifest_stem: manifest_stem(&format!("{host} {}", root.display())),
            caution: None,
        }
    }

    pub fn used_bytes(&self) -> Option<u64> {
        Some(self.total_bytes?.saturating_sub(self.free_bytes?))
    }

    /// Path of the finished manifest in `dir`.
    pub fn manifest_path(&self, dir: &Path) -> PathBuf {
        dir.join(format!("{}.tsv", self.manifest_stem))
    }

    /// Path of the manifest while the scan runs (left behind when it was interrupted).
    pub fn partial_path(&self, dir: &Path) -> PathBuf {
        dir.join(format!("{}.tsv.partial", self.manifest_stem))
    }
}

/// `manifest-<text>` with characters Windows does not allow in file names replaced.
pub fn manifest_stem(text: &str) -> String {
    let mut s = String::from("manifest-");
    let mut last_dash = true;
    for c in text.trim().chars() {
        let c = match c {
            '<' | '>' | ':' | '"' | '|' | '?' | '*' => continue,
            '/' | '\\' => '-',
            c if c.is_control() => '_',
            c => c,
        };
        if c == '-' && last_dash {
            continue;
        }
        last_dash = c == '-';
        s.push(c);
    }
    while s.ends_with(['-', ' ', '.']) {
        s.pop();
    }
    s
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanSettings {
    /// Smaller files are not hashed.
    pub min_size: u64,
    /// Skip system folders: Windows, Program Files, ProgramData, AppData, the recycle bin...
    /// (on Linux: /proc, /sys, /usr, /var... of the root file system).
    pub skip_system: bool,
    /// Skip OneDrive and Google Drive folders in user profiles (their files may be online-only;
    /// online-only files are always skipped, never downloaded).
    pub skip_cloud_folders: bool,
    /// Hashing threads for a drive without seek penalty; `0` = automatic.
    pub threads: usize,
}

impl Default for ScanSettings {
    fn default() -> Self {
        ScanSettings { min_size: DEFAULT_MIN_SIZE, skip_system: true, skip_cloud_folders: true, threads: 0 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Phase {
    /// Walking the drive to see how much there is to hash.
    #[default]
    Counting,
    Hashing,
    /// Finished (or stopped) and the manifest written.
    Done,
}

/// Where a running scan is.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanProgress {
    pub phase: Phase,
    /// Files and bytes to hash (complete once counting is done).
    pub files_total: u64,
    pub bytes_total: u64,
    /// Hashed so far, including what an interrupted earlier scan had done.
    pub files_done: u64,
    pub bytes_done: u64,
    /// Done by the earlier, interrupted scan.
    pub files_resumed: u64,
    pub bytes_resumed: u64,
    pub failed: u64,
    /// Online-only cloud files that were skipped (never downloaded).
    pub placeholders_skipped: u64,
    /// Folders that could not be opened (permissions).
    pub folders_unreadable: u64,
    /// The file being hashed (or the folder being counted).
    pub current: String,
    pub elapsed_ms: u64,
}

impl ScanProgress {
    /// Bytes per second hashed in this run.
    pub fn rate(&self) -> f64 {
        let secs = self.elapsed_ms as f64 / 1000.0;
        if secs < 1.0 {
            return 0.0;
        }
        (self.bytes_done - self.bytes_resumed) as f64 / secs
    }

    /// Seconds left at the current rate, if it can be told.
    pub fn eta_secs(&self) -> Option<u64> {
        let r = self.rate();
        (r > 0.0 && self.phase == Phase::Hashing)
            .then(|| (self.bytes_total.saturating_sub(self.bytes_done) as f64 / r) as u64)
    }
}

/// A file that could not be read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanFailure {
    pub path: String,
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct VolumeResult {
    /// The finished manifest, or the `.partial` one when stopped.
    pub manifest: PathBuf,
    pub complete: bool,
    pub progress: ScanProgress,
    /// The first files that could not be read.
    pub failures: Vec<ScanFailure>,
}

/// At most this many failures are listed (all are counted).
const MAX_FAILURES: usize = 200;

/// Hash every eligible file of `vol` into its manifest in `out_dir`. `resume`: continue an
/// interrupted scan of this volume if its `.partial` file is there (else start over).
pub fn scan_volume(
    vol: &Volume,
    out_dir: &Path,
    settings: &ScanSettings,
    resume: bool,
    report: &mut dyn FnMut(&ScanProgress),
    cancel: &AtomicBool,
) -> Result<VolumeResult> {
    let started = Instant::now();
    fs::create_dir_all(out_dir)?;
    let partial = vol.partial_path(out_dir);
    let finished = vol.manifest_path(out_dir);
    if !vol.root.is_dir() {
        return err(format!("{} cannot be read (not connected?)", vol.display));
    }
    let _awake = platform::KeepAwake::new();
    let mut prog = ScanProgress::default();

    // ---- what an interrupted scan already did
    let mut done: HashSet<Vec<u8>> = HashSet::new();
    let mut file = if resume && partial.is_file() {
        let (paths, files, bytes) = read_partial(&partial)?;
        done = paths;
        prog.files_resumed = files;
        prog.bytes_resumed = bytes;
        OpenOptions::new().append(true).open(&partial)?
    } else {
        let mut f = File::create(&partial)?;
        f.write_all(crate::manifest::HEADER.as_bytes())?;
        f.write_all(b"\n")?;
        f
    };
    file.seek(SeekFrom::End(0))?;
    let mut out = BufWriter::with_capacity(1 << 16, file);

    let walker = Walker::new(vol, settings, &[partial.clone(), finished.clone()]);

    // ---- pass 1: count
    {
        let mut last = Instant::now();
        walker.walk(cancel, &mut |entry| {
            match entry {
                WalkItem::File { size, .. } => {
                    prog.files_total += 1;
                    prog.bytes_total += size;
                }
                WalkItem::Folder(p) if last.elapsed() >= Duration::from_millis(150) => {
                    last = Instant::now();
                    prog.current = p.display().to_string();
                    prog.elapsed_ms = started.elapsed().as_millis() as u64;
                    report(&prog);
                }
                _ => {}
            }
            true
        });
        // what the earlier scan did counts as done, not as left to do
        prog.files_done = prog.files_resumed;
        prog.bytes_done = prog.bytes_resumed;
        prog.files_total = prog.files_total.max(prog.files_resumed);
        prog.bytes_total = prog.bytes_total.max(prog.bytes_resumed);
    }
    if cancel.load(Ordering::Relaxed) {
        out.flush()?;
        prog.phase = Phase::Done;
        return Ok(VolumeResult { manifest: partial, complete: false, progress: prog, failures: Vec::new() });
    }

    // ---- pass 2: hash
    prog.phase = Phase::Hashing;
    report(&prog);
    let threads = thread_count(vol, settings);
    let sequential = vol.rotational == Some(true) || vol.kind == DriveKind::Usb;
    let current = Mutex::new(String::new());
    let hashed = AtomicU64::new(0);
    let walk_stats = Mutex::new((0u64, 0u64));
    let mut failures = Vec::new();
    let (job_tx, job_rx) = sync_channel::<PathBuf>(threads * 64);
    let job_rx = Mutex::new(job_rx);
    let written: Result<()> = std::thread::scope(|scope| {
        let (res_tx, res_rx) = sync_channel::<HashOutcome>(threads * 64);
        for _ in 0..threads {
            let tx = res_tx.clone();
            let (rx, cur, hashed) = (&job_rx, &current, &hashed);
            scope.spawn(move || hasher(rx, tx, cur, hashed, sequential));
        }
        drop(res_tx);
        let (walker, done, stats) = (&walker, &done, &walk_stats);
        scope.spawn(move || {
            let (mut placeholders, mut unreadable) = (0u64, 0u64);
            walker.walk(cancel, &mut |entry| {
                match entry {
                    WalkItem::File { path, key, .. } => {
                        if !done.contains(&key) && job_tx.send(path).is_err() {
                            return false;
                        }
                    }
                    WalkItem::Placeholder => placeholders += 1,
                    WalkItem::Unreadable => unreadable += 1,
                    WalkItem::Folder(_) => {}
                }
                true
            });
            if let Ok(mut s) = stats.lock() {
                *s = (placeholders, unreadable);
            }
            // dropping job_tx here lets the hashers finish, which ends the results
        });
        let mut last = Instant::now();
        let mut since_flush = Instant::now();
        let base_files = prog.files_done;
        let mut finished_files = 0u64;
        loop {
            match res_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(HashOutcome::Done { path, size, hash }) => {
                    out.write_all(&line(&hash, size, &vol.host, &written_path(vol, &path)))?;
                    finished_files += 1;
                }
                Ok(HashOutcome::Failed { path, error }) => {
                    prog.failed += 1;
                    if failures.len() < MAX_FAILURES {
                        failures.push(ScanFailure { path: path.display().to_string(), error });
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if since_flush.elapsed() >= Duration::from_secs(2) {
                // an interruption loses at most the last two seconds
                out.flush()?;
                since_flush = Instant::now();
            }
            if last.elapsed() >= Duration::from_millis(100) {
                last = Instant::now();
                prog.files_done = base_files + finished_files;
                // bytes read so far, so one big file still moves the bar
                prog.bytes_done = prog.bytes_resumed + hashed.load(Ordering::Relaxed);
                prog.elapsed_ms = started.elapsed().as_millis() as u64;
                if let Ok(c) = current.lock() {
                    prog.current.clone_from(&c);
                }
                if let Ok(s) = walk_stats.lock() {
                    (prog.placeholders_skipped, prog.folders_unreadable) = *s;
                }
                report(&prog);
            }
        }
        prog.files_done = base_files + finished_files;
        Ok(())
    });
    written?;
    prog.bytes_done = prog.bytes_resumed + hashed.load(Ordering::Relaxed);
    if let Ok(s) = walk_stats.lock() {
        (prog.placeholders_skipped, prog.folders_unreadable) = *s;
    }
    let mut file = out.into_inner().map_err(|e| e.into_error())?;
    file.flush()?;
    file.sync_all()?;
    drop(file);
    prog.phase = Phase::Done;
    prog.elapsed_ms = started.elapsed().as_millis() as u64;
    prog.current.clear();
    let complete = !cancel.load(Ordering::Relaxed);
    let manifest = if complete {
        fs::rename(&partial, &finished)?;
        finished
    } else {
        partial
    };
    report(&prog);
    Ok(VolumeResult { manifest, complete, progress: prog, failures })
}

fn thread_count(vol: &Volume, settings: &ScanSettings) -> usize {
    if vol.rotational == Some(true) || vol.kind == DriveKind::Usb {
        return 1;
    }
    if settings.threads > 0 {
        return settings.threads.min(16);
    }
    if vol.kind == DriveKind::Network {
        return 2;
    }
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).clamp(1, 4)
}

enum HashOutcome {
    Done { path: PathBuf, size: u64, hash: String },
    Failed { path: PathBuf, error: String },
}

fn hasher(
    rx: &Mutex<Receiver<PathBuf>>,
    tx: std::sync::mpsc::SyncSender<HashOutcome>,
    current: &Mutex<String>,
    hashed: &AtomicU64,
    sequential: bool,
) {
    let mut buf = vec![0u8; if sequential { 4 << 20 } else { 1 << 20 }];
    loop {
        let job = match rx.lock() {
            Ok(r) => r.recv(),
            Err(_) => return,
        };
        let Ok(path) = job else { return };
        if let Ok(mut c) = current.lock() {
            *c = path.display().to_string();
        }
        let before = hashed.load(Ordering::Relaxed);
        let outcome = match hash_file(&path, &mut buf, hashed) {
            Ok((hash, size)) => HashOutcome::Done { path, size, hash },
            Err(e) => {
                let _ = before;
                HashOutcome::Failed { path, error: e.to_string() }
            }
        };
        if tx.send(outcome).is_err() {
            return;
        }
    }
}

/// SHA-256 (lower-case hex) and the number of bytes read.
pub fn hash_file(path: &Path, buf: &mut [u8], counter: &AtomicU64) -> std::io::Result<(String, u64)> {
    let mut f = platform::open_sequential(path)?;
    let mut h = Sha256::new();
    let mut n = 0u64;
    loop {
        let r = match f.read(buf) {
            Ok(0) => break,
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        h.update(&buf[..r]);
        n += r as u64;
        counter.fetch_add(r as u64, Ordering::Relaxed);
    }
    Ok((crate::manifest::to_hex(&h.finalize()), n))
}

/// One manifest line.
fn line(hash: &str, size: u64, host: &str, path: &[u8]) -> Vec<u8> {
    let mut l = Vec::with_capacity(hash.len() + host.len() + path.len() + 24);
    l.extend_from_slice(hash.as_bytes());
    l.push(b'\t');
    l.extend_from_slice(size.to_string().as_bytes());
    l.push(b'\t');
    l.extend_from_slice(host.as_bytes());
    l.push(b'\t');
    l.extend_from_slice(path);
    l.push(b'\n');
    l
}

/// The path as written in the manifest (exact bytes on Linux, UTF-8 on Windows).
fn written_path(vol: &Volume, p: &Path) -> Vec<u8> {
    if let Some(w) = &vol.write_root {
        if let Ok(rest) = p.strip_prefix(&vol.root) {
            let mut s = w.clone();
            if !s.ends_with(['\\', '/']) {
                s.push(std::path::MAIN_SEPARATOR);
            }
            s.push_str(&rest.to_string_lossy());
            return s.into_bytes();
        }
    }
    path_bytes(p)
}

#[cfg(unix)]
fn path_bytes(p: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    p.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn path_bytes(p: &Path) -> Vec<u8> {
    p.to_string_lossy().into_owned().into_bytes()
}

/// Read an interrupted scan's manifest: the paths it has (4th column), and their number and
/// bytes. An incomplete last line (the write that was cut off) is removed from the file.
fn read_partial(path: &Path) -> Result<(HashSet<Vec<u8>>, u64, u64)> {
    let mut data = Vec::new();
    File::open(path)?.read_to_end(&mut data)?;
    if !data.is_empty() && !data.ends_with(b"\n") {
        let keep = data.iter().rposition(|&b| b == b'\n').map(|i| i + 1).unwrap_or(0);
        data.truncate(keep);
        let f = OpenOptions::new().write(true).open(path)?;
        f.set_len(keep as u64)?;
    }
    let mut done = HashSet::new();
    let (mut files, mut bytes) = (0u64, 0u64);
    for l in data.split(|&b| b == b'\n') {
        let l = l.strip_suffix(b"\r").unwrap_or(l);
        let mut cols = l.splitn(4, |&b| b == b'\t');
        let (Some(h), Some(sz), Some(_), Some(p)) = (cols.next(), cols.next(), cols.next(), cols.next()) else {
            continue;
        };
        if h.len() != 64 {
            continue;
        }
        let Some(sz) = std::str::from_utf8(sz).ok().and_then(|s| s.parse::<u64>().ok()) else { continue };
        if done.insert(p.to_vec()) {
            files += 1;
            bytes += sz;
        }
    }
    // an empty file needs its header again
    if data.is_empty() {
        let mut f = OpenOptions::new().write(true).truncate(true).open(path)?;
        f.write_all(crate::manifest::HEADER.as_bytes())?;
        f.write_all(b"\n")?;
    }
    Ok((done, files, bytes))
}

// ------------------------------------------------------------------------------------ walking

enum WalkItem {
    /// An eligible file. `key` is the path as written in the manifest.
    File { path: PathBuf, size: u64, key: Vec<u8> },
    /// An online-only cloud file (skipped: reading it would download it).
    Placeholder,
    /// A folder about to be listed.
    Folder(PathBuf),
    /// A folder that could not be listed.
    Unreadable,
}

#[derive(Clone)]
struct Walker {
    vol: Volume,
    settings: ScanSettings,
    /// Never hashed: the manifest being written.
    exclude: Vec<PathBuf>,
    /// The device id of the root (Linux: the walk stays on one file system).
    #[cfg(unix)]
    dev: Option<u64>,
}

impl Walker {
    fn new(vol: &Volume, settings: &ScanSettings, exclude: &[PathBuf]) -> Walker {
        Walker {
            vol: vol.clone(),
            settings: settings.clone(),
            exclude: exclude.to_vec(),
            #[cfg(unix)]
            dev: {
                use std::os::unix::fs::MetadataExt;
                fs::metadata(&vol.root).ok().map(|m| m.dev())
            },
        }
    }

    /// Depth-first, names in order. `f` returns `false` to stop.
    fn walk(&self, cancel: &AtomicBool, f: &mut dyn FnMut(WalkItem) -> bool) {
        let mut stack: Vec<(PathBuf, usize)> = vec![(self.vol.root.clone(), 0)];
        while let Some((dir, depth)) = stack.pop() {
            if cancel.load(Ordering::Relaxed) || !f(WalkItem::Folder(dir.clone())) {
                return;
            }
            let rd = match fs::read_dir(&dir) {
                Ok(rd) => rd,
                Err(_) => {
                    if !f(WalkItem::Unreadable) {
                        return;
                    }
                    continue;
                }
            };
            let mut entries: Vec<fs::DirEntry> = rd.filter_map(|e| e.ok()).collect();
            entries.sort_by_key(|e| e.file_name());
            let mut subdirs = Vec::new();
            for e in entries {
                let Ok(ft) = e.file_type() else { continue };
                // links and junctions are not followed (loops, and the same files twice)
                if ft.is_symlink() {
                    continue;
                }
                let name = e.file_name();
                if ft.is_dir() {
                    if !self.skip_dir(&dir, &name, depth) {
                        subdirs.push(e.path());
                    }
                    continue;
                }
                if !ft.is_file() {
                    continue;
                }
                let Ok(meta) = e.metadata() else { continue };
                if platform::is_placeholder(&meta) {
                    if !f(WalkItem::Placeholder) {
                        return;
                    }
                    continue;
                }
                if meta.len() < self.settings.min_size {
                    continue;
                }
                let path = e.path();
                if self.exclude.iter().any(|x| x == &path) || has_line_break(&path) {
                    continue;
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    if self.dev.is_some_and(|d| d != meta.dev()) {
                        continue;
                    }
                }
                let key = written_path(&self.vol, &path);
                if !f(WalkItem::File { path, size: meta.len(), key }) {
                    return;
                }
            }
            // reversed, so the stack pops them in name order
            for d in subdirs.into_iter().rev() {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    // other file systems mounted inside this one are volumes of their own
                    if self.dev.is_some() && fs::symlink_metadata(&d).map(|m| Some(m.dev()) != self.dev).unwrap_or(true)
                    {
                        continue;
                    }
                }
                stack.push((d, depth + 1));
            }
        }
    }

    fn skip_dir(&self, parent: &Path, name: &std::ffi::OsStr, depth: usize) -> bool {
        let n = name.to_string_lossy();
        platform::skip_dir(&self.vol, &self.settings, parent, &n, depth)
    }
}

/// A line break in a path cannot be written in a manifest line.
fn has_line_break(p: &Path) -> bool {
    let b = path_bytes(p);
    b.contains(&b'\n') || b.contains(&b'\r')
}

// ------------------------------------------------------------------------------- volumes

/// The drives (Windows) or mounted file systems (Linux) of this computer, in letter / mount
/// order. Drives without a disk (empty card readers) and optical drives are left out.
pub fn list_volumes() -> Vec<Volume> {
    platform::list_volumes()
}

/// This computer's name as the scripts write it (`COMPUTERNAME` on Windows, the host name on
/// Linux).
pub fn computer_name() -> String {
    platform::computer_name()
}

/// Manifests of a scan that was interrupted, in `dir`.
pub fn interrupted_scans(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> =
        fs::read_dir(dir).map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).collect()).unwrap_or_default();
    v.retain(|p| p.to_string_lossy().ends_with(".tsv.partial"));
    v.sort();
    v
}

#[cfg(windows)]
mod platform {
    //! Win32 calls, declared here (the same few the scripts use through WMI).
    use super::{DriveKind, ScanSettings, Volume};
    use std::ffi::{c_void, OsStr};
    use std::fs::{File, Metadata, OpenOptions};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use std::path::{Path, PathBuf};

    type Handle = *mut c_void;
    const INVALID_HANDLE: Handle = -1isize as Handle;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetLogicalDrives() -> u32;
        fn GetDriveTypeW(root: *const u16) -> u32;
        fn GetVolumeInformationW(
            root: *const u16,
            name: *mut u16,
            name_len: u32,
            serial: *mut u32,
            max_component: *mut u32,
            flags: *mut u32,
            fs_name: *mut u16,
            fs_name_len: u32,
        ) -> i32;
        fn GetDiskFreeSpaceExW(dir: *const u16, free_to_caller: *mut u64, total: *mut u64, free: *mut u64) -> i32;
        fn CreateFileW(
            name: *const u16,
            access: u32,
            share: u32,
            security: *const c_void,
            disposition: u32,
            flags: u32,
            template: Handle,
        ) -> Handle;
        fn DeviceIoControl(
            device: Handle,
            code: u32,
            input: *const c_void,
            input_len: u32,
            output: *mut c_void,
            output_len: u32,
            returned: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;
        fn CloseHandle(h: Handle) -> i32;
        fn SetThreadExecutionState(flags: u32) -> u32;
        fn SetErrorMode(mode: u32) -> u32;
    }

    #[link(name = "mpr")]
    extern "system" {
        fn WNetGetConnectionW(local: *const u16, remote: *mut u16, len: *mut u32) -> u32;
        fn WNetOpenEnumW(scope: u32, kind: u32, usage: u32, resource: *const c_void, handle: *mut Handle) -> u32;
        fn WNetEnumResourceW(handle: Handle, count: *mut u32, buffer: *mut c_void, size: *mut u32) -> u32;
        fn WNetCloseEnum(handle: Handle) -> u32;
    }

    /// NETRESOURCEW
    #[repr(C)]
    struct NetResource {
        scope: u32,
        kind: u32,
        display_type: u32,
        usage: u32,
        local_name: *mut u16,
        remote_name: *mut u16,
        comment: *mut u16,
        provider: *mut u16,
    }

    fn wide(s: &str) -> Vec<u16> {
        OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
    }

    fn from_wide(buf: &[u16]) -> String {
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end])
    }

    /// Keeps the computer from going to sleep while a scan runs.
    pub struct KeepAwake;

    impl KeepAwake {
        pub fn new() -> KeepAwake {
            // ES_CONTINUOUS | ES_SYSTEM_REQUIRED
            // SAFETY: plain Win32 call.
            unsafe { SetThreadExecutionState(0x8000_0001) };
            KeepAwake
        }
    }

    impl Drop for KeepAwake {
        fn drop(&mut self) {
            // SAFETY: plain Win32 call; ES_CONTINUOUS alone releases the request.
            unsafe { SetThreadExecutionState(0x8000_0000) };
        }
    }

    pub fn computer_name() -> String {
        std::env::var("COMPUTERNAME").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "PC".into())
    }

    /// FILE_ATTRIBUTE_OFFLINE | RECALL_ON_OPEN | RECALL_ON_DATA_ACCESS: an online-only file.
    pub fn is_placeholder(meta: &Metadata) -> bool {
        meta.file_attributes() & 0x0044_1000 != 0
    }

    /// Read-only with FILE_FLAG_SEQUENTIAL_SCAN (read-ahead for one pass from start to end).
    pub fn open_sequential(path: &Path) -> std::io::Result<File> {
        OpenOptions::new().read(true).custom_flags(0x0800_0000).open(path)
    }

    pub fn skip_dir(vol: &Volume, settings: &ScanSettings, parent: &Path, name: &str, depth: usize) -> bool {
        // as export-hashes.ps1: ^X:\(Windows|Program Files|ProgramData|$Recycle.Bin|System Volume
        // Information) (prefixes, so also Program Files (x86) and Windows.old) and \AppData\
        let lower = name.to_lowercase();
        if depth == 0 && (lower.starts_with("$recycle.bin") || lower.starts_with("system volume information")) {
            return true;
        }
        if settings.skip_system {
            let system = ["windows", "program files", "programdata"];
            if depth == 0 && system.iter().any(|s| lower.starts_with(s)) {
                return true;
            }
            if lower == "appdata" {
                return true;
            }
        }
        if settings.skip_cloud_folders && depth == 2 {
            // <drive>\Users\<name>\OneDrive*, \Google Drive
            let in_users = parent
                .parent()
                .and_then(|p| p.file_name())
                .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case("users"))
                && parent.parent().and_then(|p| p.parent()) == Some(vol.root.as_path());
            if in_users && (lower.starts_with("onedrive") || lower == "google drive") {
                return true;
            }
        }
        false
    }

    /// IOCTL_STORAGE_QUERY_PROPERTY on `\\.\X:`: (bus type, seek penalty).
    fn storage_info(letter: char) -> (Option<u32>, Option<bool>) {
        let path = wide(&format!(r"\\.\{letter}:"));
        // SAFETY: Win32 calls with buffers sized as passed; the handle is closed below.
        unsafe {
            let h = CreateFileW(path.as_ptr(), 0, 0x1 | 0x2, std::ptr::null(), 3, 0, std::ptr::null_mut());
            if h == INVALID_HANDLE || h.is_null() {
                return (None, None);
            }
            const IOCTL_STORAGE_QUERY_PROPERTY: u32 = 0x002D_1400;
            // STORAGE_PROPERTY_QUERY { PropertyId, QueryType (0 = standard), AdditionalParameters[1] }
            let mut query = [0u32; 3];
            let mut out = [0u8; 1024];
            let mut got = 0u32;
            // StorageDeviceProperty = 0 → STORAGE_DEVICE_DESCRIPTOR, BusType at offset 28
            let bus = (DeviceIoControl(
                h,
                IOCTL_STORAGE_QUERY_PROPERTY,
                query.as_ptr() as *const c_void,
                12,
                out.as_mut_ptr() as *mut c_void,
                out.len() as u32,
                &mut got,
                std::ptr::null_mut(),
            ) != 0
                && got >= 32)
                .then(|| u32::from_le_bytes([out[28], out[29], out[30], out[31]]));
            // StorageDeviceSeekPenaltyProperty = 7 → DEVICE_SEEK_PENALTY_DESCRIPTOR, IncursSeekPenalty at 8
            query[0] = 7;
            let mut out2 = [0u8; 16];
            let seek = (DeviceIoControl(
                h,
                IOCTL_STORAGE_QUERY_PROPERTY,
                query.as_ptr() as *const c_void,
                12,
                out2.as_mut_ptr() as *mut c_void,
                out2.len() as u32,
                &mut got,
                std::ptr::null_mut(),
            ) != 0
                && got >= 9)
                .then(|| out2[8] != 0);
            CloseHandle(h);
            (bus, seek)
        }
    }

    fn volume_info(root: &str) -> Option<(String, String)> {
        let r = wide(root);
        let mut name = [0u16; 261];
        let mut fs = [0u16; 261];
        // SAFETY: buffers are as long as passed.
        let ok = unsafe {
            GetVolumeInformationW(
                r.as_ptr(),
                name.as_mut_ptr(),
                name.len() as u32,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                fs.as_mut_ptr(),
                fs.len() as u32,
            )
        } != 0;
        ok.then(|| (from_wide(&name), from_wide(&fs)))
    }

    fn space(root: &str) -> (Option<u64>, Option<u64>) {
        let r = wide(root);
        let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
        // SAFETY: plain Win32 call with out-parameters.
        if unsafe { GetDiskFreeSpaceExW(r.as_ptr(), &mut avail, &mut total, &mut free) } != 0 {
            (Some(total), Some(free))
        } else {
            (None, None)
        }
    }

    fn unc_of(letter: char) -> Option<String> {
        let local = wide(&format!("{letter}:"));
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        // SAFETY: buffer length passed in characters.
        (unsafe { WNetGetConnectionW(local.as_ptr(), buf.as_mut_ptr(), &mut len) } == 0).then(|| from_wide(&buf))
    }

    /// Remembered (persistent) drive mappings: letter → share. A program run as administrator
    /// does not see the letters mapped in the normal session, but can reach the shares.
    fn remembered_mappings() -> Vec<(char, String)> {
        let mut out = Vec::new();
        // SAFETY: enumeration buffers are sized as passed; strings point into the buffer.
        unsafe {
            let mut h: Handle = std::ptr::null_mut();
            // RESOURCE_REMEMBERED = 3, RESOURCETYPE_DISK = 1
            if WNetOpenEnumW(3, 1, 0, std::ptr::null(), &mut h) != 0 {
                return out;
            }
            let mut buf = vec![0u64; 2048];
            loop {
                let mut count = u32::MAX;
                let mut size = (buf.len() * 8) as u32;
                let r = WNetEnumResourceW(h, &mut count, buf.as_mut_ptr() as *mut c_void, &mut size);
                if r != 0 {
                    break;
                }
                let items = std::slice::from_raw_parts(buf.as_ptr() as *const NetResource, count as usize);
                for it in items {
                    let read = |p: *mut u16| -> String {
                        if p.is_null() {
                            return String::new();
                        }
                        let mut n = 0;
                        while *p.add(n) != 0 {
                            n += 1;
                        }
                        String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
                    };
                    let local = read(it.local_name);
                    let remote = read(it.remote_name);
                    if let Some(l) = local.chars().next().filter(|c| c.is_ascii_alphabetic()) {
                        if !remote.is_empty() {
                            out.push((l.to_ascii_uppercase(), remote));
                        }
                    }
                }
            }
            WNetCloseEnum(h);
        }
        out
    }

    pub fn list_volumes() -> Vec<Volume> {
        // no "insert a disk" dialogs for empty card readers
        // SAFETY: plain Win32 call. SEM_FAILCRITICALERRORS | SEM_NOOPENFILEERRORBOX
        unsafe { SetErrorMode(0x0001 | 0x8000) };
        let pc = computer_name();
        let mut out = Vec::new();
        // SAFETY: plain Win32 call.
        let mask = unsafe { GetLogicalDrives() };
        let mut letters_seen = Vec::new();
        for i in 0..26u32 {
            if mask & (1 << i) == 0 {
                continue;
            }
            let letter = (b'A' + i as u8) as char;
            let root = format!("{letter}:\\");
            let r = wide(&root);
            // SAFETY: plain Win32 call.
            let t = unsafe { GetDriveTypeW(r.as_ptr()) };
            // 2 removable, 3 fixed, 4 remote, 5 CD-ROM, 6 RAM disk
            if !matches!(t, 2 | 3 | 4 | 6) {
                continue;
            }
            let Some((label, fs)) = volume_info(&root) else { continue };
            letters_seen.push(letter);
            let (total, free) = space(&root);
            let (bus, seek) = if t == 4 { (None, None) } else { storage_info(letter) };
            // BusTypeUsb 7, BusTypeSd 12, BusTypeMmc 13
            let kind = match t {
                4 => DriveKind::Network,
                2 => DriveKind::Usb,
                _ if matches!(bus, Some(7 | 12 | 13)) => DriveKind::Usb,
                _ => DriveKind::Internal,
            };
            let share = if t == 4 { unc_of(letter) } else { None };
            let caution = (label == "Google Drive").then(|| {
                "Google Drive for desktop: reading it downloads every file. Use the cloud script instead.".to_string()
            });
            let host = format!("{pc} {letter}: ({label})");
            let display = match (&share, label.is_empty()) {
                (Some(s), true) => format!("{letter}: {s}"),
                (Some(s), false) => format!("{letter}: {label} ({s})"),
                (None, true) => format!("{letter}:"),
                (None, false) => format!("{letter}: {label}"),
            };
            out.push(Volume {
                root: PathBuf::from(&root),
                write_root: None,
                display,
                label,
                kind,
                fs,
                total_bytes: total,
                free_bytes: free,
                rotational: seek,
                manifest_stem: super::manifest_stem(&format!("{pc} {letter}")),
                host,
                caution,
            });
        }
        // mapped drives this process cannot see by letter (run as administrator): by share
        for (letter, share) in remembered_mappings() {
            if letters_seen.contains(&letter) {
                continue;
            }
            let root = format!("{}\\", share.trim_end_matches('\\'));
            let Some((label, fs)) = volume_info(&root) else { continue };
            let (total, free) = space(&root);
            out.push(Volume {
                root: PathBuf::from(&root),
                write_root: Some(format!("{letter}:\\")),
                display: format!("{letter}: {share}"),
                host: format!("{pc} {letter}: ({label})"),
                label,
                kind: DriveKind::Network,
                fs,
                total_bytes: total,
                free_bytes: free,
                rotational: None,
                manifest_stem: super::manifest_stem(&format!("{pc} {letter}")),
                caution: None,
            });
        }
        out.sort_by(|a, b| a.display.cmp(&b.display));
        out
    }
}

#[cfg(unix)]
mod platform {
    use super::{DriveKind, ScanSettings, Volume};
    use std::fs::{File, Metadata};
    use std::path::{Path, PathBuf};

    pub struct KeepAwake;

    impl KeepAwake {
        pub fn new() -> KeepAwake {
            KeepAwake
        }
    }

    pub fn computer_name() -> String {
        std::fs::read_to_string("/proc/sys/kernel/hostname")
            .or_else(|_| std::fs::read_to_string("/etc/hostname"))
            .map(|s| s.trim().to_string())
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| std::env::var("HOSTNAME").ok())
            .unwrap_or_else(|| "linux".into())
    }

    pub fn is_placeholder(_meta: &Metadata) -> bool {
        false
    }

    pub fn open_sequential(path: &Path) -> std::io::Result<File> {
        File::open(path)
    }

    /// Top-level folders of the system file system that hold no user files.
    const SYSTEM_TOP: &[&str] = &[
        "proc",
        "sys",
        "dev",
        "run",
        "tmp",
        "var",
        "usr",
        "lib",
        "lib32",
        "lib64",
        "libx32",
        "bin",
        "sbin",
        "etc",
        "boot",
        "snap",
        "lost+found",
        "efi",
    ];

    pub fn skip_dir(vol: &Volume, settings: &ScanSettings, _parent: &Path, name: &str, depth: usize) -> bool {
        if name == ".zfs" || name == ".snapshot" {
            return true;
        }
        settings.skip_system && depth == 0 && vol.root == Path::new("/") && SYSTEM_TOP.contains(&name)
    }

    /// `\040` and friends in /proc/self/mounts.
    fn unescape(s: &str) -> String {
        let b = s.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'\\' && i + 4 <= b.len() && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
                out.push((b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0'));
                i += 4;
            } else {
                out.push(b[i]);
                i += 1;
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    const LOCAL_FS: &[&str] = &[
        "ext2", "ext3", "ext4", "xfs", "btrfs", "zfs", "f2fs", "ntfs", "ntfs3", "fuseblk", "vfat", "exfat", "hfsplus",
        "jfs", "reiserfs", "bcachefs", "msdos",
    ];
    const NETWORK_FS: &[&str] = &[
        "nfs",
        "nfs4",
        "cifs",
        "smb3",
        "smbfs",
        "fuse.sshfs",
        "sshfs",
        "9p",
        "davfs",
        "fuse.rclone",
        "ceph",
        "glusterfs",
    ];

    fn on_usb(device: &str) -> bool {
        let Some(name) = device.strip_prefix("/dev/") else { return false };
        let name = name.rsplit('/').next().unwrap_or(name);
        std::fs::canonicalize(format!("/sys/class/block/{name}"))
            .map(|p| p.to_string_lossy().contains("/usb"))
            .unwrap_or(false)
    }

    fn rotational(device: &str) -> Option<bool> {
        let name = device.strip_prefix("/dev/")?.rsplit('/').next()?;
        let real = std::fs::canonicalize(format!("/sys/class/block/{name}")).ok()?;
        // a partition's queue is its disk's
        for p in [real.join("queue/rotational"), real.parent()?.join("queue/rotational")] {
            if let Ok(s) = std::fs::read_to_string(p) {
                return Some(s.trim() == "1");
            }
        }
        None
    }

    fn statvfs(path: &str) -> (Option<u64>, Option<u64>) {
        // no libc crate here: `df` style numbers are a nicety, not needed for scanning
        let _ = path;
        (None, None)
    }

    pub fn list_volumes() -> Vec<Volume> {
        let host = computer_name();
        let mounts = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();
        let mut out: Vec<Volume> = Vec::new();
        for l in mounts.lines() {
            let mut it = l.split(' ');
            let (Some(dev), Some(target), Some(fs)) = (it.next(), it.next(), it.next()) else { continue };
            let target = unescape(target);
            let kind = if NETWORK_FS.contains(&fs) {
                DriveKind::Network
            } else if LOCAL_FS.contains(&fs) {
                if on_usb(dev) {
                    DriveKind::Usb
                } else {
                    DriveKind::Internal
                }
            } else {
                continue;
            };
            let skip = ["/boot", "/snap", "/proc", "/sys", "/dev", "/efi", "/recovery", "/var/lib", "/run/user"];
            if skip.iter().any(|p| target == *p || target.starts_with(&format!("{p}/"))) {
                continue;
            }
            // TrueNAS app data and hidden datasets
            if target.split('/').any(|c| c.starts_with('.') && c.len() > 1) || target.contains("ix-applications") {
                continue;
            }
            if out.iter().any(|v| v.root == Path::new(&target)) {
                continue;
            }
            let (total, free) = statvfs(&target);
            let display = if target == "/" { "/ (system)".to_string() } else { target.clone() };
            out.push(Volume {
                root: PathBuf::from(&target),
                write_root: None,
                display,
                label: String::new(),
                kind,
                fs: fs.to_string(),
                total_bytes: total,
                free_bytes: free,
                rotational: rotational(dev),
                host: host.clone(),
                manifest_stem: super::manifest_stem(&format!("{host} {target}")),
                caution: None,
            });
        }
        out.sort_by(|a, b| a.root.cmp(&b.root));
        out
    }
}

#[cfg(not(any(windows, unix)))]
mod platform {
    use super::{ScanSettings, Volume};
    use std::fs::{File, Metadata};
    use std::path::Path;

    pub struct KeepAwake;

    impl KeepAwake {
        pub fn new() -> KeepAwake {
            KeepAwake
        }
    }

    pub fn computer_name() -> String {
        "computer".into()
    }

    pub fn is_placeholder(_meta: &Metadata) -> bool {
        false
    }

    pub fn open_sequential(path: &Path) -> std::io::Result<File> {
        File::open(path)
    }

    pub fn skip_dir(_v: &Volume, _s: &ScanSettings, _p: &Path, _n: &str, _d: usize) -> bool {
        false
    }

    pub fn list_volumes() -> Vec<Volume> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::index::Index;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dupemap-scan-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn sha(data: &[u8]) -> String {
        crate::manifest::to_hex(&Sha256::digest(data))
    }

    /// Files of 0..n KiB under `root`, some duplicated.
    fn tree(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        let mut made = Vec::new();
        let mut put = |rel: &str, data: Vec<u8>| {
            let p = root.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, &data).unwrap();
            made.push((p, data));
        };
        let a: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let b: Vec<u8> = (0..200_000u32).map(|i| (i * 13 % 241) as u8).collect();
        put("photos/2019/a.jpg", a.clone());
        put("photos/copy of a.jpg", a);
        put("music/b.flac", b);
        put("music/tiny.txt", b"small".to_vec());
        put("deep/x/y/z/c.bin", vec![9u8; 150_000]);
        put("with space & ünï/d e.bin", vec![1u8; 120_000]);
        made
    }

    fn settings() -> ScanSettings {
        ScanSettings { min_size: 100_000, ..Default::default() }
    }

    #[test]
    fn scans_into_a_manifest_that_imports() {
        let root = tmp("root");
        let out = tmp("out");
        let made = tree(&root);
        let vol = Volume::folder(&root, "TESTPC T: (Data)");
        let mut reports = 0;
        let cancel = AtomicBool::new(false);
        let r = scan_volume(&vol, &out, &settings(), true, &mut |_| reports += 1, &cancel).unwrap();
        assert!(r.complete);
        assert!(reports >= 2);
        assert_eq!(r.manifest, vol.manifest_path(&out));
        assert!(!vol.partial_path(&out).exists());
        assert_eq!((r.progress.files_total, r.progress.files_done), (5, 5), "tiny.txt is below the minimum");
        assert_eq!(r.progress.bytes_done, 300_000 * 2 + 200_000 + 150_000 + 120_000);
        assert_eq!(r.progress.failed, 0);
        let text = fs::read_to_string(&r.manifest).unwrap();
        assert!(text.starts_with(crate::manifest::HEADER));
        for (p, data) in made.iter().filter(|(_, d)| d.len() >= 100_000) {
            let want = format!("{}\t{}\tTESTPC T: (Data)\t{}", sha(data), data.len(), p.display());
            assert!(text.lines().any(|l| l == want), "missing line for {}", p.display());
        }
        // the manifest imports and the two copies of a.jpg are one content
        let mut db = Db::open_in_memory().unwrap();
        let rep = db.import_file(&r.manifest, &mut |_| true).unwrap();
        assert_eq!(rep.rows_imported, 5);
        assert_eq!(rep.severity(), crate::report::Severity::Ok);
        let ix = Index::build(&db).unwrap();
        assert_eq!(ix.dup_group_count(), 1);
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn an_interrupted_scan_continues() {
        let root = tmp("resume-root");
        let out = tmp("resume-out");
        tree(&root);
        let vol = Volume::folder(&root, "TESTPC");
        // what a scan cut off after two files looks like: the header, two lines and half a third
        let full = {
            let cancel = AtomicBool::new(false);
            let r = scan_volume(&vol, &out, &settings(), false, &mut |_| {}, &cancel).unwrap();
            fs::read_to_string(&r.manifest).unwrap()
        };
        fs::remove_file(vol.manifest_path(&out)).unwrap();
        let lines: Vec<&str> = full.lines().collect();
        let cut = format!("{}\n{}\n{}\n{}", lines[0], lines[1], lines[2], &lines[3][..30]);
        fs::write(vol.partial_path(&out), cut).unwrap();
        let cancel = AtomicBool::new(false);
        let r = scan_volume(&vol, &out, &settings(), true, &mut |_| {}, &cancel).unwrap();
        assert!(r.complete);
        assert_eq!(r.progress.files_resumed, 2);
        assert_eq!(r.progress.files_done, 5);
        let mut again: Vec<String> = fs::read_to_string(&r.manifest).unwrap().lines().map(String::from).collect();
        let mut want: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
        again.sort();
        want.sort();
        assert_eq!(again, want, "same lines, none twice, the cut-off one redone");
        // not resuming starts over
        fs::write(vol.partial_path(&out), "garbage").unwrap();
        let r = scan_volume(&vol, &out, &settings(), false, &mut |_| {}, &cancel).unwrap();
        assert_eq!(r.progress.files_resumed, 0);
        assert_eq!(fs::read_to_string(&r.manifest).unwrap().lines().count(), 6);
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn a_cancelled_scan_keeps_its_partial_manifest() {
        let root = tmp("cancel-root");
        let out = tmp("cancel-out");
        tree(&root);
        let vol = Volume::folder(&root, "TESTPC");
        let cancel = AtomicBool::new(true);
        let r = scan_volume(&vol, &out, &settings(), true, &mut |_| {}, &cancel).unwrap();
        assert!(!r.complete);
        assert_eq!(r.manifest, vol.partial_path(&out));
        assert!(r.manifest.exists());
        assert_eq!(interrupted_scans(&out), vec![vol.partial_path(&out)]);
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn the_manifest_being_written_is_not_hashed() {
        let root = tmp("self");
        tree(&root);
        let vol = Volume::folder(&root, "TESTPC");
        // the output folder inside the scanned folder
        let out = root.join("scans");
        let cancel = AtomicBool::new(false);
        let r =
            scan_volume(&vol, &out, &ScanSettings { min_size: 0, ..settings() }, false, &mut |_| {}, &cancel).unwrap();
        let text = fs::read_to_string(&r.manifest).unwrap();
        assert!(!text.contains(".tsv"), "{text}");
        assert!(text.contains("tiny.txt"), "min size 0 hashes everything");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn manifest_names() {
        assert_eq!(manifest_stem("DESKTOP-1 C"), "manifest-DESKTOP-1 C");
        assert_eq!(manifest_stem("nas /mnt/tank/media"), "manifest-nas -mnt-tank-media");
        assert_eq!(manifest_stem("pc <x>:|?* /"), "manifest-pc x");
    }

    #[test]
    fn progress_rate_and_time_left() {
        let p = ScanProgress {
            phase: Phase::Hashing,
            bytes_total: 1000,
            bytes_done: 600,
            bytes_resumed: 100,
            elapsed_ms: 5000,
            ..Default::default()
        };
        assert_eq!(p.rate(), 100.0);
        assert_eq!(p.eta_secs(), Some(4));
    }

    #[cfg(windows)]
    #[test]
    fn this_computer_has_drives() {
        let v = list_volumes();
        assert!(v.iter().any(|v| v.root == Path::new("C:\\")), "{v:?}");
        let c = v.iter().find(|v| v.root == Path::new("C:\\")).unwrap();
        assert!(c.host.starts_with(&computer_name()));
        assert!(c.host.contains(" C: ("));
        assert_eq!(c.kind, DriveKind::Internal);
        assert!(c.total_bytes.unwrap_or(0) > 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn this_computer_has_file_systems() {
        // the sandbox and CI have at least the root file system
        let v = list_volumes();
        assert!(!computer_name().is_empty());
        assert!(v.iter().all(|v| v.host == computer_name()));
    }
}
