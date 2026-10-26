//! dupemap-gen: write realistic synthetic manifests (and expected.json) for testing DupeMap.
//!
//!   dupemap-gen --out DIR [--rows 1000000] [--seed 42] [--no-bad-lines]

use dupemap_core::synth::{generate, Config};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

fn main() -> ExitCode {
    let mut cfg = Config::default();
    let mut out: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--out" => out = args.next().map(PathBuf::from),
            "--rows" => cfg.rows = args.next().and_then(|v| v.replace('_', "").parse().ok()).unwrap_or(cfg.rows),
            "--seed" => cfg.seed = args.next().and_then(|v| v.parse().ok()).unwrap_or(cfg.seed),
            "--no-bad-lines" => cfg.bad_lines = false,
            "-h" | "--help" => {
                println!("usage: dupemap-gen --out DIR [--rows N] [--seed S] [--no-bad-lines]");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("unknown argument: {other}");
                return ExitCode::from(2);
            }
        }
    }
    let Some(out) = out else {
        eprintln!("usage: dupemap-gen --out DIR [--rows N] [--seed S] [--no-bad-lines]");
        return ExitCode::from(2);
    };
    if let Err(e) = std::fs::create_dir_all(&out) {
        eprintln!("cannot create {}: {e}", out.display());
        return ExitCode::FAILURE;
    }
    let t = Instant::now();
    match generate(Some(&out), &cfg) {
        Ok(exp) => {
            for m in &exp.manifests {
                let size = std::fs::metadata(out.join(&m.file_name)).map(|x| x.len()).unwrap_or(0);
                println!("{:<40} {:>10} rows {:>8.1} MB", m.file_name, m.rows, size as f64 / 1e6);
            }
            println!(
                "total {} rows (target {}) in {:.1}s -> {}",
                exp.total_rows,
                cfg.rows,
                t.elapsed().as_secs_f64(),
                out.display()
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("generation failed: {e}");
            ExitCode::FAILURE
        }
    }
}
