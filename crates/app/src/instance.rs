//! One DupeMap per database. Two would each keep their own session and marks and overwrite the
//! other's, so a second start brings the open window to the front instead.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Held while DupeMap runs; the lock goes when the process ends, however it ends.
pub struct InstanceLock {
    _file: File,
}

pub enum Lock {
    Held(InstanceLock),
    /// Another DupeMap has this database open.
    Taken,
    /// No lock could be taken at all (a read-only folder, a file system without locks): run
    /// anyway.
    Unavailable,
}

/// `<database>.lock`, beside the database.
pub fn lock_path(db: &Path) -> PathBuf {
    let mut name = db.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

/// Lock the database, waiting up to `wait` for another DupeMap to let go of it (a restart as
/// administrator starts the new one while the old one is still closing).
pub fn lock(db: &Path, wait: Duration) -> Lock {
    if let Some(dir) = db.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(file) = OpenOptions::new().create(true).write(true).truncate(false).open(lock_path(db)) else {
        return Lock::Unavailable;
    };
    let start = Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => return Lock::Held(InstanceLock { _file: file }),
            Err(TryLockError::WouldBlock) if start.elapsed() < wait => std::thread::sleep(Duration::from_millis(200)),
            Err(TryLockError::WouldBlock) => return Lock::Taken,
            Err(TryLockError::Error(_)) => return Lock::Unavailable,
        }
    }
}

#[cfg(windows)]
mod win {
    use std::ffi::c_void;
    #[link(name = "user32")]
    extern "system" {
        pub fn FindWindowW(class: *const u16, title: *const u16) -> *mut c_void;
        pub fn IsIconic(hwnd: *mut c_void) -> i32;
        pub fn ShowWindow(hwnd: *mut c_void, cmd: i32) -> i32;
        pub fn SetForegroundWindow(hwnd: *mut c_void) -> i32;
        pub fn MessageBoxW(hwnd: *mut c_void, text: *const u16, caption: *const u16, kind: u32) -> i32;
    }
    pub const SW_RESTORE: i32 = 9;
    pub const MB_ICONINFORMATION: u32 = 0x40;

    pub fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
}

/// Bring the open DupeMap window to the front. `false` if there is none (it is still starting,
/// or closing) or Windows would not switch to it.
#[cfg(windows)]
pub fn bring_to_front() -> bool {
    // the class winit gives its windows (eframe keeps it): an Explorer window showing a folder
    // called DupeMap has the same title, but another class
    let class = win::wide("Window Class");
    let title = win::wide("DupeMap");
    // SAFETY: NUL-terminated UTF-16 strings that live across the call; the handle is only
    // passed back to the window functions.
    unsafe {
        let hwnd = win::FindWindowW(class.as_ptr(), title.as_ptr());
        if hwnd.is_null() {
            return false;
        }
        if win::IsIconic(hwnd) != 0 {
            win::ShowWindow(hwnd, win::SW_RESTORE);
        }
        win::SetForegroundWindow(hwnd) != 0
    }
}

#[cfg(not(windows))]
pub fn bring_to_front() -> bool {
    false
}

/// Say that another DupeMap has the database open.
#[cfg(windows)]
pub fn say_open(db: &Path) {
    let text = win::wide(&format!(
        "DupeMap is already open with this database:\n{}\n\nSwitch to that window, or close it before starting DupeMap again.",
        db.display()
    ));
    let title = win::wide("DupeMap");
    // SAFETY: NUL-terminated UTF-16 strings that live across the call; no owner window.
    unsafe {
        win::MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), win::MB_ICONINFORMATION);
    }
}

#[cfg(not(windows))]
pub fn say_open(db: &Path) {
    eprintln!("DupeMap is already open with this database: {}", db.display());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_lock_on_the_same_database_waits_then_gives_up() {
        let dir = std::env::temp_dir().join(format!("dupemap-lock-{}", std::process::id()));
        let db = dir.join("test.dupemap");
        let first = lock(&db, Duration::ZERO);
        assert!(matches!(first, Lock::Held(_)));
        let start = Instant::now();
        assert!(matches!(lock(&db, Duration::from_millis(500)), Lock::Taken));
        assert!(start.elapsed() >= Duration::from_millis(400), "it waited");
        // another database is not affected
        assert!(matches!(lock(&dir.join("other.dupemap"), Duration::ZERO), Lock::Held(_)));
        drop(first);
        assert!(matches!(lock(&db, Duration::ZERO), Lock::Held(_)), "free again once the first one is gone");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
