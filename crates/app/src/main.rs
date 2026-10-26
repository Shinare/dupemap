//! DupeMap: explore and compare hash manifests from many machines.
//!
//!   DupeMap.exe [--db <path>] [--demo <rows>]
//!
//! The database defaults to %LOCALAPPDATA%\DupeMap\dupemap.db (Linux: ~/.local/share/dupemap).

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod actions;
mod app;
mod colors;
mod files;
mod fonts;
mod format;
mod groups;
mod ignore_ui;
mod inspector;
mod manifests_ui;
mod pane;
mod selection;
mod shot;
mod theme;
mod tree;
mod widgets;
mod worker;

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

fn parse_args() -> Result<app::Options, String> {
    let mut opts = app::Options { db_path: default_db_path(), demo_rows: None, screenshot: None };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--db" => opts.db_path = PathBuf::from(args.next().ok_or("--db needs a path")?),
            "--demo" => {
                let n = args.next().ok_or("--demo needs a row count")?;
                opts.demo_rows = Some(n.replace(['_', ','], "").parse().map_err(|_| format!("bad row count: {n}"))?);
            }
            "--screenshot" => opts.screenshot = Some(PathBuf::from(args.next().ok_or("--screenshot needs a file")?)),
            "-h" | "--help" => return Err(String::new()),
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(opts)
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
    let opts = match parse_args() {
        Ok(o) => o,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("{e}");
            }
            eprintln!("usage: DupeMap [--db <path>] [--demo <rows>]");
            std::process::exit(if e.is_empty() { 0 } else { 2 });
        }
    };
    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("DupeMap")
            .with_app_id("dupemap")
            .with_inner_size([1440.0, 900.0])
            .with_min_inner_size([760.0, 440.0])
            .with_icon(icon()),
        ..Default::default()
    };
    eframe::run_native("DupeMap", native, Box::new(move |cc| Ok(Box::new(app::DupeMapApp::new(cc, opts)))))
}
