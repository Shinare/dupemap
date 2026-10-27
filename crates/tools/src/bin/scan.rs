//! dupemap-scan: the app's built-in scan from the command line.
//!
//!   dupemap-scan --list                         drives / file systems and how they are classed
//!   dupemap-scan [--out DIR] [--min-size N] [--threads N] [--fresh] [--all-folders] DRIVE...
//!   dupemap-scan --folder PATH --host NAME [--out DIR]   one folder as a source of its own
//!
//! DRIVE is a letter (`D:`) or a mount point (`/mnt/tank`) from `--list`, or `internal`, `usb`,
//! `network` for every drive of that kind. Manifests are written to DIR (default: the current
//! folder), one per drive, in the export scripts' format. An interrupted scan continues where it
//! stopped unless --fresh is given.

use dupemap_core::scan::{list_volumes, scan_volume, DriveKind, Phase, ScanSettings, Volume};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

fn human(n: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
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

fn usage() -> ExitCode {
    eprintln!(
        "usage: dupemap-scan --list\n       dupemap-scan [--out DIR] [--min-size BYTES] [--threads N] [--fresh] [--all-folders] DRIVE...\n       dupemap-scan --folder PATH --host NAME [--out DIR]"
    );
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let mut out = PathBuf::from(".");
    let mut settings = ScanSettings::default();
    let mut fresh = false;
    let mut list = false;
    let mut folder: Option<PathBuf> = None;
    let mut host: Option<String> = None;
    let mut wanted: Vec<String> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--list" => list = true,
            "--out" => out = args.next().map(PathBuf::from).unwrap_or(out),
            "--min-size" => settings.min_size = args.next().and_then(|v| v.parse().ok()).unwrap_or(settings.min_size),
            "--threads" => settings.threads = args.next().and_then(|v| v.parse().ok()).unwrap_or(0),
            "--fresh" => fresh = true,
            "--all-folders" => {
                settings.skip_system = false;
                settings.skip_cloud_folders = false;
            }
            "--folder" => folder = args.next().map(PathBuf::from),
            "--host" => host = args.next(),
            "-h" | "--help" => return usage(),
            other => wanted.push(other.to_string()),
        }
    }
    let volumes = list_volumes();
    if list {
        for v in &volumes {
            println!(
                "{:<28} {:<9} {:<8} {:>10} used {:<12} host \"{}\"  -> {}.tsv{}",
                v.display,
                format!("{:?}", v.kind),
                v.fs,
                v.used_bytes().map(human).unwrap_or_else(|| "?".into()),
                match v.rotational {
                    Some(true) => "spinning",
                    Some(false) => "ssd",
                    None => "",
                },
                v.host,
                v.manifest_stem,
                v.caution.as_ref().map(|c| format!("  ({c})")).unwrap_or_default()
            );
        }
        return ExitCode::SUCCESS;
    }
    let chosen: Vec<Volume> = match &folder {
        Some(f) => match &host {
            Some(h) => vec![Volume::folder(f, h)],
            None => return usage(),
        },
        None => {
            let mut v = Vec::new();
            for w in &wanted {
                let kind = match w.to_ascii_lowercase().as_str() {
                    "internal" => Some(DriveKind::Internal),
                    "usb" => Some(DriveKind::Usb),
                    "network" => Some(DriveKind::Network),
                    _ => None,
                };
                let found: Vec<&Volume> = volumes
                    .iter()
                    .filter(|x| match kind {
                        Some(k) => x.kind == k && x.caution.is_none(),
                        None => {
                            let r = x.root.to_string_lossy();
                            r.eq_ignore_ascii_case(w)
                                || r.trim_end_matches(['\\', '/']).eq_ignore_ascii_case(w.trim_end_matches(['\\', '/']))
                                || x.write_root.as_deref().is_some_and(|wr| {
                                    wr.trim_end_matches('\\').eq_ignore_ascii_case(w.trim_end_matches('\\'))
                                })
                        }
                    })
                    .collect();
                if found.is_empty() {
                    eprintln!("no drive matches {w} (see --list)");
                    return ExitCode::from(2);
                }
                v.extend(found.into_iter().cloned());
            }
            v
        }
    };
    if chosen.is_empty() {
        return usage();
    }
    let cancel = AtomicBool::new(false);
    let mut failed = 0;
    for v in &chosen {
        println!("{} → {}", v.display, v.manifest_path(&out).display());
        let t = Instant::now();
        let mut last = Instant::now();
        let r = scan_volume(
            v,
            &out,
            &settings,
            !fresh,
            &mut |p| {
                if last.elapsed().as_secs() >= 5 {
                    last = Instant::now();
                    match p.phase {
                        Phase::Counting => eprintln!("  counting: {} files, {}", p.files_total, human(p.bytes_total)),
                        _ => eprintln!(
                            "  {}/{} files, {}/{} at {}/s",
                            p.files_done,
                            p.files_total,
                            human(p.bytes_done),
                            human(p.bytes_total),
                            human(p.rate() as u64)
                        ),
                    }
                }
            },
            &cancel,
        );
        match r {
            Ok(r) => {
                let p = &r.progress;
                println!(
                    "  {} files, {} in {:.1}s ({}/s){}{}{}",
                    p.files_done,
                    human(p.bytes_done),
                    t.elapsed().as_secs_f64(),
                    human(p.rate() as u64),
                    if p.files_resumed > 0 {
                        format!(", {} done by an earlier scan", p.files_resumed)
                    } else {
                        String::new()
                    },
                    if p.failed > 0 { format!(", {} could not be read", p.failed) } else { String::new() },
                    if p.placeholders_skipped > 0 {
                        format!(", {} online-only files skipped", p.placeholders_skipped)
                    } else {
                        String::new()
                    }
                );
                for f in r.failures.iter().take(10) {
                    println!("    failed: {}: {}", f.path, f.error);
                }
                failed += p.failed;
            }
            Err(e) => {
                eprintln!("  {e}");
                return ExitCode::FAILURE;
            }
        }
    }
    if failed > 0 {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}
