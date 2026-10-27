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
mod ignore_ui;
mod inspector;
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

/// The window icon: two overlapping documents (blue = size, orange = copies), drawn at startup.
fn icon() -> egui::IconData {
    const N: u32 = 64;
    let mut rgba = vec![0u8; (N * N * 4) as usize];
    let mut put = |x: u32, y: u32, c: [u8; 3]| {
        let i = ((y * N + x) * 4) as usize;
        rgba[i..i + 4].copy_from_slice(&[c[0], c[1], c[2], 255]);
    };
    let rounded = |x: u32, y: u32, x0: u32, y0: u32, x1: u32, y1: u32, r: u32| -> bool {
        if x < x0 || x >= x1 || y < y0 || y >= y1 {
            return false;
        }
        let cx = if x < x0 + r {
            x0 + r
        } else if x >= x1 - r {
            x1 - r - 1
        } else {
            x
        };
        let cy = if y < y0 + r {
            y0 + r
        } else if y >= y1 - r {
            y1 - r - 1
        } else {
            y
        };
        let (dx, dy) = (x as i64 - cx as i64, y as i64 - cy as i64);
        dx * dx + dy * dy <= (r * r) as i64
    };
    for y in 0..N {
        for x in 0..N {
            if rounded(x, y, 0, 0, N, N, 12) {
                put(x, y, [0x1a, 0x1a, 0x19]);
            }
            if rounded(x, y, 12, 10, 38, 44, 4) {
                put(x, y, [0x39, 0x87, 0xe5]);
            }
            if rounded(x, y, 26, 20, 52, 54, 4) {
                put(x, y, [0xf1, 0x62, 0x24]);
            }
            if rounded(x, y, 26, 20, 38, 44, 0) {
                put(x, y, [0xf0, 0xef, 0xec]);
            }
        }
    }
    egui::IconData { rgba, width: N, height: N }
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
