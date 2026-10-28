//! DupeMap: explore and compare hash manifests from many machines.
//!
//!   DupeMap.exe [--db <path>] [--demo <rows>]
//!
//! The database defaults to %LOCALAPPDATA%\DupeMap\dupemap.db (Linux: ~/.local/share/dupemap).

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod actions;
mod app;
mod categories_ui;
mod checksum_ui;
mod colors;
mod elevate;
mod files;
mod fonts;
mod format;
mod groups;
mod icon_art;
mod ignore_ui;
mod inspector;
mod instance;
mod manifests_ui;
mod marks_ui;
mod pane;
mod scan_ui;
mod search_ui;
mod selection;
mod session;
mod shot;
mod summary_ui;
mod theme;
mod tree;
mod treemap_ui;
mod widgets;
mod worker;

use dupemap_core::sessionfile::SessionFile;
use std::path::PathBuf;

fn default_db_path() -> PathBuf {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from).map(|p| p.join("DupeMap"))
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("share")))
            .map(|p| p.join("dupemap"))
    };
    base.unwrap_or_else(|| PathBuf::from(".")).join("dupemap.db")
}

/// Window size in logical pixels: `--window 1024x768` (for trying small screens).
fn parse_size(v: &str) -> Option<[f32; 2]> {
    let (w, h) = v.split_once(['x', 'X'])?;
    let (w, h) = (w.trim().parse::<f32>().ok()?, h.trim().parse::<f32>().ok()?);
    (w >= 400.0 && h >= 300.0).then_some([w, h])
}

fn parse_args() -> Result<(app::Options, Option<[f32; 2]>, bool), String> {
    let mut opts = app::Options { db_path: default_db_path(), ..Default::default() };
    let mut size = None;
    let mut reset_session = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--db" => opts.db_path = PathBuf::from(args.next().ok_or("--db needs a path")?),
            "--demo" => {
                let n = args.next().ok_or("--demo needs a row count")?;
                opts.demo_rows = Some(n.replace(['_', ','], "").parse().map_err(|_| format!("bad row count: {n}"))?);
            }
            "--screenshot" => opts.screenshot = Some(PathBuf::from(args.next().ok_or("--screenshot needs a file")?)),
            "--window" => {
                let v = args.next().ok_or("--window needs a size like 1024x768")?;
                size = Some(parse_size(&v).ok_or(format!("bad window size: {v}"))?);
            }
            "--reset-session" => reset_session = true,
            "--scan" => opts.open_scan = true,
            "-h" | "--help" => return Err(String::new()),
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok((opts, size, reset_session))
}

/// The window icon (the same picture as DupeMap.exe's icon).
fn icon() -> egui::IconData {
    const N: u32 = 64;
    egui::IconData { rgba: icon_art::render(N), width: N, height: N }
}

fn main() -> eframe::Result {
    let (mut opts, size, reset_session) = match parse_args() {
        Ok(o) => o,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("{e}");
            }
            eprintln!(
                "usage: DupeMap [--db <path>] [--demo <rows>] [--window <width>x<height>] [--reset-session] [--scan]"
            );
            std::process::exit(if e.is_empty() { 0 } else { 2 });
        }
    };
    // one DupeMap per database (screenshots run beside the app)
    let _lock = if opts.screenshot.is_none() {
        use instance::Lock;
        match instance::lock(&opts.db_path, std::time::Duration::ZERO) {
            Lock::Held(l) => Some(l),
            Lock::Unavailable => None,
            // the other one is open: show it (not after a restart as administrator, where the
            // other one is the old window, closing)
            Lock::Taken if !opts.open_scan && instance::bring_to_front() => return Ok(()),
            // closing or starting: wait for it a little
            Lock::Taken => {
                let wait = std::time::Duration::from_secs(if opts.open_scan { 20 } else { 3 });
                match instance::lock(&opts.db_path, wait) {
                    Lock::Held(l) => Some(l),
                    Lock::Unavailable => None,
                    Lock::Taken => {
                        if opts.open_scan || !instance::bring_to_front() {
                            instance::say_open(&opts.db_path);
                        }
                        return Ok(());
                    }
                }
            }
        }
    } else {
        None
    };
    // the saved session (tabs, layout, window) lives beside the database; screenshots neither
    // read nor write it
    if opts.screenshot.is_none() {
        let file = SessionFile::beside(&opts.db_path);
        if reset_session {
            file.remove();
        } else if let (Some(session), _) = session::Session::load(&file) {
            opts.session = Some(session);
        }
        opts.session_file = Some(file);
    }
    let saved_window = opts.session.as_ref().and_then(|s| s.window);
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("DupeMap")
        .with_app_id("dupemap")
        .with_min_inner_size(session::MIN_WINDOW)
        .with_icon(icon());
    viewport = match (size, saved_window) {
        // --window wins over the saved window
        (Some(size), _) => viewport.with_inner_size(size),
        (None, Some(w)) => {
            let v = viewport.with_inner_size(w.size).with_maximized(w.maximized);
            match w.pos {
                Some([x, y]) => v.with_position([x, y]),
                None => v,
            }
        }
        (None, None) => viewport.with_inner_size([1440.0, 900.0]),
    };
    let native = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native("DupeMap", native, Box::new(move |cc| Ok(Box::new(app::DupeMapApp::new(cc, opts)))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_sizes() {
        assert_eq!(parse_size("1024x768"), Some([1024.0, 768.0]));
        assert_eq!(parse_size("1366 X 768"), Some([1366.0, 768.0]));
        assert_eq!(parse_size("10x10"), None);
        assert_eq!(parse_size("wide"), None);
    }
}
