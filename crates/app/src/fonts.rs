//! Fonts. egui's built-in fonts cover Latin, Greek and Cyrillic; file names can be in any
//! script, so the system's fonts are added as fallbacks (loaded in the background, so startup
//! is not delayed by reading ~40 MB of font files).

use egui::{FontData, FontDefinitions, FontFamily};
use std::path::PathBuf;
use std::sync::Arc;

struct Candidate {
    name: &'static str,
    file: &'static str,
    /// Use as the primary proportional font (instead of a fallback).
    primary: bool,
    monospace_primary: bool,
}

const fn c(name: &'static str, file: &'static str) -> Candidate {
    Candidate { name, file, primary: false, monospace_primary: false }
}

#[cfg(windows)]
fn candidates() -> (PathBuf, Vec<Candidate>) {
    let dir =
        std::env::var_os("WINDIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows")).join("Fonts");
    (
        dir,
        vec![
            Candidate { primary: true, ..c("Segoe UI", "segoeui.ttf") },
            Candidate { monospace_primary: true, ..c("Consolas", "consola.ttf") },
            c("Segoe UI Symbol", "seguisym.ttf"),
            c("Microsoft YaHei", "msyh.ttc"),
            c("Microsoft JhengHei", "msjh.ttc"),
            c("Yu Gothic", "YuGothM.ttc"),
            c("Meiryo", "meiryo.ttc"),
            c("Malgun Gothic", "malgun.ttf"),
            c("Nirmala UI", "Nirmala.ttf"),
            c("Leelawadee UI", "LeelawUI.ttf"),
            c("Ebrima", "ebrima.ttf"),
            c("Segoe UI Historic", "seguihis.ttf"),
        ],
    )
}

#[cfg(not(windows))]
fn candidates() -> (PathBuf, Vec<Candidate>) {
    (
        PathBuf::from("/usr/share/fonts"),
        vec![
            Candidate { primary: true, ..c("Inter", "opentype/inter/Inter-Regular.otf") },
            Candidate { monospace_primary: true, ..c("DejaVu Sans Mono", "truetype/dejavu/DejaVuSansMono.ttf") },
            c("DejaVu Sans", "truetype/dejavu/DejaVuSans.ttf"),
            c("Noto Sans CJK", "opentype/noto/NotoSansCJK-Regular.ttc"),
            c("FreeSans", "truetype/freefont/FreeSans.ttf"),
        ],
    )
}

/// Read the system fonts on a background thread and install them when ready.
pub fn install_in_background(ctx: &egui::Context) {
    let ctx = ctx.clone();
    let _ = std::thread::Builder::new().name("dupemap-fonts".into()).spawn(move || {
        let defs = load();
        ctx.set_fonts(defs);
        ctx.request_repaint();
    });
}

fn load() -> FontDefinitions {
    let mut defs = FontDefinitions::default();
    let (dir, list) = candidates();
    let mut primary: Vec<String> = Vec::new();
    let mut mono_primary: Vec<String> = Vec::new();
    let mut fallbacks: Vec<String> = Vec::new();
    for cand in list {
        let Ok(bytes) = std::fs::read(dir.join(cand.file)) else { continue };
        defs.font_data.insert(cand.name.to_string(), Arc::new(FontData::from_owned(bytes)));
        if cand.primary {
            primary.push(cand.name.to_string());
        } else if cand.monospace_primary {
            mono_primary.push(cand.name.to_string());
        } else {
            fallbacks.push(cand.name.to_string());
        }
    }
    if let Some(list) = defs.families.get_mut(&FontFamily::Proportional) {
        for (i, n) in primary.iter().enumerate() {
            list.insert(i, n.clone());
        }
        list.extend(fallbacks.iter().cloned());
    }
    if let Some(list) = defs.families.get_mut(&FontFamily::Monospace) {
        for (i, n) in mono_primary.iter().enumerate() {
            list.insert(i, n.clone());
        }
        // the proportional primary covers symbols the monospace font lacks
        list.extend(primary.iter().cloned());
        list.extend(fallbacks.iter().cloned());
    }
    defs
}
