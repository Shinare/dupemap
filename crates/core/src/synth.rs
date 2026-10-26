//! Synthetic manifests for tests and benchmarks.
//!
//! Mimics a household setup: three Windows PCs (one with two USB drives, scanned by both the
//! main and the USB script), a TrueNAS box with two pools and three datasets, Google Drive and
//! OneDrive (SHA-256 by download plus a QuickXorHash listing). Content is shared between sources
//! the way backups really are: whole folders copied, re-downloads, renamed copies, edited files.
//! Awkward names and deliberately bad lines are included so imports can be checked against
//! `expected.json`. Output is deterministic for a given seed.

use crate::manifest::{to_hex, HEADER};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;

/// Deterministic SplitMix64 generator.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed ^ 0x5DEE_CE66_D1CE_4E5B)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        mix(self.0)
    }

    /// Uniform in `0..n` (`n > 0`).
    pub fn below(&mut self, n: u64) -> u64 {
        ((self.next_u64() as u128 * n as u128) >> 64) as u64
    }

    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    pub fn chance(&mut self, p: f64) -> bool {
        self.unit() < p
    }

    pub fn pick<'a, T>(&mut self, v: &'a [T]) -> &'a T {
        &v[self.below(v.len() as u64) as usize]
    }

    /// Log-uniform integer in `[lo, hi]`.
    pub fn log_uniform(&mut self, lo: u64, hi: u64) -> u64 {
        let (a, b) = ((lo as f64).ln(), (hi as f64).ln());
        ((a + (b - a) * self.unit()).exp() as u64).clamp(lo, hi)
    }
}

fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

const MB: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cat {
    Photo,
    HomeVideo,
    Movie,
    Tv,
    Music,
    Doc,
    Software,
    Iso,
    Archive,
    Vm,
    Project,
}

struct Item {
    cat: Cat,
    folder: u32,
    year: u16,
    name: String,
    size: u64,
}

struct Catalog {
    items: Vec<Item>,
    folders: Vec<Vec<String>>,
    seed: u64,
}

impl Catalog {
    fn hash(&self, id: usize, variant: u64) -> [u8; 32] {
        let mut out = [0u8; 32];
        let base =
            self.seed ^ (id as u64).wrapping_mul(0xA24B_AED4_963E_E407) ^ variant.wrapping_mul(0x9FB2_1C65_1E98_DF25);
        for j in 0..4 {
            out[j * 8..j * 8 + 8]
                .copy_from_slice(&mix(base.wrapping_add((j as u64).wrapping_mul(0x632B_E59B_D9B4_E019))).to_le_bytes());
        }
        out
    }

    fn ids(&self, cat: Cat) -> Vec<usize> {
        self.items.iter().enumerate().filter(|(_, it)| it.cat == cat).map(|(i, _)| i).collect()
    }
}

const EVENTS: &[&str] = &[
    "Holiday",
    "Wedding",
    "Christmas",
    "Birthday",
    "Kraków trip",
    "City Marathon",
    "Giant's Causeway",
    "Gdańsk",
    "Zakopane hike",
    "Dublin weekend",
    "Lisbon",
    "Shkodër",
    "Garden",
    "New house",
    "Concert",
    "Beach",
    "Snow day",
    "Łódź family visit",
    "Road trip",
    "Misc",
    "Mourne Mountains",
    "Easter",
    "Football",
    "Kids",
    "Rome",
    "Barcelona",
];
const WORDS: &[&str] = &[
    "Blue",
    "River",
    "Night",
    "Summer",
    "Last",
    "Iron",
    "Silent",
    "Golden",
    "Dark",
    "City",
    "Lost",
    "Star",
    "Wild",
    "Paper",
    "Glass",
    "Northern",
    "Electric",
    "Hidden",
    "Broken",
    "Velvet",
    "Ocean",
    "Winter",
    "Red",
    "Empire",
    "Shadow",
    "Garden",
    "Dream",
    "Echo",
    "Zielony",
    "Księżyc",
    "Światło",
    "Żółty",
    "Mgła",
    "Harbour",
    "Signal",
];
const DOC_FOLDERS: &[&str] = &[
    "Finance",
    "House",
    "Car",
    "Work",
    "Travel",
    "Taxes",
    "Manuals",
    "Receipts",
    "School",
    "Insurance",
    "Solar",
    "Heat pump",
];
const DOC_STEMS: &[&str] = &[
    "Invoice",
    "Statement",
    "Contract",
    "Manual",
    "Receipt",
    "Report",
    "Notes",
    "Plan",
    "Quote",
    "Warranty",
    "Policy",
    "Letter",
    "Scan",
    "Datasheet",
];
const DOC_EXTS: &[(&str, u64, u64)] =
    &[("pdf", 1, 60), ("docx", 1, 15), ("xlsx", 1, 25), ("pptx", 2, 120), ("psd", 20, 600), ("odt", 1, 10)];
const SOFTWARE: &[&str] =
    &["setup", "installer", "driver", "firmware", "update", "tool", "studio", "player", "launcher", "bios"];
const SOFT_EXTS: &[(&str, u64, u64)] =
    &[("exe", 1, 900), ("msi", 1, 400), ("zip", 1, 2000), ("apk", 5, 200), ("bin", 1, 64)];
const ISOS: &[&str] = &[
    "ubuntu-24.04.2-desktop-amd64.iso",
    "Win11_24H2_EnglishInternational_x64.iso",
    "debian-12.9.0-amd64-netinst.iso",
    "TrueNAS-SCALE-24.10.2.iso",
    "proxmox-ve_8.3-1.iso",
    "Fedora-Workstation-Live-x86_64-41.iso",
    "Win10_22H2_English_x64.iso",
    "memtest86-usb.img",
    "gparted-live-1.6.0-amd64.iso",
    "HomeAssistant-OS-14.img",
];
const SHOWS: &[&str] =
    &["The Silent River", "Northern Signal", "Glass Empire", "Iron Harbour", "Velvet Night", "Dark City", "Lost Echo"];

fn build_catalog(seed: u64, n: usize) -> Catalog {
    let mut rng = Rng::new(seed ^ 0xCA7A);
    let mut items: Vec<Item> = Vec::with_capacity(n);
    let mut folders: Vec<Vec<String>> = Vec::new();
    let folder = |parts: Vec<String>, folders: &mut Vec<Vec<String>>| -> u32 {
        if folders.last() == Some(&parts) {
            return (folders.len() - 1) as u32;
        }
        folders.push(parts);
        (folders.len() - 1) as u32
    };
    let share = |f: f64| ((n as f64 * f).round() as usize).max(1);

    // Photos: 2005..=2025, a few events per year, one folder per event.
    let photos = share(0.52);
    let years = 21usize;
    for k in 0..photos {
        let year = 2005 + (k * years / photos) as u16;
        let per_year = (photos / years).max(1);
        let ev_idx = (k % per_year) * 6 / per_year;
        let event = EVENTS[(mix((year as u64 * 31 + ev_idx as u64) ^ seed) % EVENTS.len() as u64) as usize];
        let month = 1 + (ev_idx * 2) % 12;
        let f = folder(vec!["Photos".into(), year.to_string(), format!("{year}-{month:02} {event}")], &mut folders);
        let num = k;
        let (name, size) = match rng.below(20) {
            0..=11 => (format!("IMG_{num:05}.JPG"), rng.log_uniform(MB, 12 * MB)),
            12..=14 => (format!("IMG_{num:05}.HEIC"), rng.log_uniform(MB, 5 * MB)),
            15..=16 => (format!("DSC_{num:05}.NEF"), rng.log_uniform(18 * MB, 60 * MB)),
            17 => (format!("PXL_{year}{month:02}{:02}_{num:07}.jpg", 1 + k % 28), rng.log_uniform(MB, 9 * MB)),
            18 => (format!("Screenshot {year}-{month:02}-{:02} {num}.png", 1 + k % 28), rng.log_uniform(MB, 25 * MB)),
            _ => (format!("_DSC{num:05}.ARW"), rng.log_uniform(20 * MB, 50 * MB)),
        };
        items.push(Item { cat: Cat::Photo, folder: f, year, name, size });
    }
    // Home videos
    for k in 0..share(0.05) {
        let year = 2008 + (k % 18) as u16;
        let f = folder(vec!["Videos".into(), year.to_string()], &mut folders);
        let name = format!("VID_{year}{:02}{:02}_{:06}.mp4", 1 + k % 12, 1 + k % 28, (k * 7919) % 1_000_000);
        items.push(Item { cat: Cat::HomeVideo, folder: f, year, name, size: rng.log_uniform(15 * MB, 3000 * MB) });
    }
    // Movies: one folder per film
    for k in 0..share(0.015) {
        let year = 1970 + (k % 56) as u16;
        let title = format!("{} {} {}", rng.pick(WORDS), rng.pick(WORDS), k);
        let f = folder(vec!["Movies".into(), format!("{title} ({year})")], &mut folders);
        let ext = if rng.chance(0.8) { "mkv" } else { "mp4" };
        items.push(Item {
            cat: Cat::Movie,
            folder: f,
            year,
            name: format!("{title} ({year}).{ext}"),
            size: rng.log_uniform(700 * MB, 25_000 * MB),
        });
    }
    // TV
    let tv = share(0.04);
    for k in 0..tv {
        let s_idx = k * SHOWS.len() / tv;
        let show = SHOWS[s_idx];
        let j = k - (s_idx * tv).div_ceil(SHOWS.len());
        let season = 1 + j / 10;
        let ep = 1 + j % 10;
        let f = folder(vec!["TV".into(), show.into(), format!("Season {season:02}")], &mut folders);
        items.push(Item {
            cat: Cat::Tv,
            folder: f,
            year: 2015,
            name: format!("{show} S{season:02}E{ep:02}.mkv"),
            size: rng.log_uniform(150 * MB, 4000 * MB),
        });
    }
    // Music: artist / album / track
    let music = share(0.20);
    for k in 0..music {
        let album_no = k / 12;
        let artist = format!("The {} {}s", WORDS[album_no / 5 % WORDS.len()], WORDS[(album_no / 5 + 7) % WORDS.len()]);
        let year = 1975 + (album_no % 50) as u16;
        let f = folder(
            vec!["Music".into(), artist, format!("{} {album_no} ({year})", WORDS[(album_no * 3) % WORDS.len()])],
            &mut folders,
        );
        let (ext, lo, hi) =
            *rng.pick(&[("flac", 15, 80), ("flac", 15, 80), ("mp3", 3, 15), ("m4a", 2, 12), ("wav", 30, 300)]);
        let name = format!("{:02} - {} {}.{ext}", 1 + k % 12, rng.pick(WORDS), rng.pick(WORDS));
        items.push(Item { cat: Cat::Music, folder: f, year, name, size: rng.log_uniform(lo * MB, hi * MB) });
    }
    // Documents
    for k in 0..share(0.10) {
        let year = 2005 + (k % 21) as u16;
        let f =
            folder(vec!["Documents".into(), DOC_FOLDERS[k % DOC_FOLDERS.len()].into(), year.to_string()], &mut folders);
        let (ext, lo, hi) = *rng.pick(DOC_EXTS);
        let name = format!("{} {year}-{:02} {}.{ext}", rng.pick(DOC_STEMS), 1 + k % 12, k);
        items.push(Item { cat: Cat::Doc, folder: f, year, name, size: rng.log_uniform(lo * MB, hi * MB) });
    }
    // Software and ISOs
    for k in 0..share(0.02) {
        let f = folder(vec!["Software".into()], &mut folders);
        let (ext, lo, hi) = *rng.pick(SOFT_EXTS);
        let name = format!("{}-{}-v{}.{k}.{ext}", rng.pick(WORDS).to_lowercase(), rng.pick(SOFTWARE), 1 + k % 9);
        items.push(Item { cat: Cat::Software, folder: f, year: 2020, name, size: rng.log_uniform(lo * MB, hi * MB) });
    }
    for k in 0..share(0.003) {
        let f = folder(vec!["ISOs".into()], &mut folders);
        let base = ISOS[k % ISOS.len()];
        let name = if k < ISOS.len() {
            base.to_string()
        } else {
            format!("{} ({}).{}", &base[..base.len() - 4], k, &base[base.len() - 3..])
        };
        items.push(Item { cat: Cat::Iso, folder: f, year: 2024, name, size: rng.log_uniform(300 * MB, 9000 * MB) });
    }
    // Archives and VM images
    for k in 0..share(0.012) {
        let f = folder(vec!["Archives".into()], &mut folders);
        let ext = *rng.pick(&["zip", "7z", "rar", "tar.gz"]);
        let name = format!("backup-{}-{:02}-{:02} #{k}.{ext}", 2012 + k % 14, 1 + k % 12, 1 + k % 28);
        items.push(Item { cat: Cat::Archive, folder: f, year: 2018, name, size: rng.log_uniform(MB, 30_000 * MB) });
    }
    for k in 0..share(0.002) {
        let f = folder(vec!["VMs".into()], &mut folders);
        let ext = *rng.pick(&["vhdx", "vmdk", "qcow2"]);
        items.push(Item {
            cat: Cat::Vm,
            folder: f,
            year: 2022,
            name: format!("{} {} {k}.{ext}", rng.pick(WORDS), rng.pick(&["Dev", "Test", "Win11", "Ubuntu"])),
            size: rng.log_uniform(2000 * MB, 120_000 * MB),
        });
    }
    // Projects (code + data)
    for k in 0..share(0.038) {
        let proj = format!("{}-{}", rng.pick(WORDS).to_lowercase(), k / 40);
        let f = folder(vec!["Projects".into(), proj, "data".into()], &mut folders);
        let ext = *rng.pick(&["bin", "db", "sqlite", "csv", "parquet", "pst", "dat"]);
        items.push(Item {
            cat: Cat::Project,
            folder: f,
            year: 2021,
            name: format!("dataset_{k}.{ext}"),
            size: rng.log_uniform(MB, 5000 * MB),
        });
    }
    Catalog { items, folders, seed }
}

/// What an import of each generated manifest should report.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpectedManifest {
    pub file_name: String,
    /// Valid rows (SHA-256 + flagged).
    pub rows: u64,
    pub flagged: BTreeMap<String, u64>,
    pub malformed: BTreeMap<String, u64>,
    pub duplicate_listings: u64,
    pub same_path_cloud: u64,
    pub non_utf8_paths: u64,
    pub crlf: bool,
    pub bom: bool,
    pub hosts: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpectedOverlap {
    /// Imported second (alphabetical order).
    pub newer: String,
    pub older: String,
    pub paths: u64,
    pub changed: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Expected {
    pub seed: u64,
    pub target_rows: u64,
    pub total_rows: u64,
    pub manifests: Vec<ExpectedManifest>,
    pub overlaps: Vec<ExpectedOverlap>,
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Approximate number of rows across all manifests.
    pub rows: u64,
    pub seed: u64,
    /// Include deliberately malformed lines (they raise alerts on import).
    pub bad_lines: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config { rows: 1_000_000, seed: 42, bad_lines: true }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Style {
    Win,
    Posix,
    Cloud,
}

fn join(style: Style, root: &[&str], folder: &[String], name: &[u8]) -> Vec<u8> {
    let mut parts: Vec<&[u8]> = Vec::with_capacity(root.len() + folder.len() + 1);
    for r in root {
        parts.push(r.as_bytes());
    }
    for f in folder {
        parts.push(f.as_bytes());
    }
    parts.push(name);
    let mut out = Vec::with_capacity(96);
    match style {
        Style::Win => {
            for (i, p) in parts.iter().enumerate() {
                if i > 0 {
                    out.push(b'\\');
                }
                out.extend_from_slice(p);
            }
        }
        Style::Posix => {
            for p in &parts {
                out.push(b'/');
                out.extend_from_slice(p);
            }
        }
        Style::Cloud => {
            out.extend_from_slice(parts[0]);
            for p in &parts[1..] {
                out.push(b'/');
                out.extend_from_slice(p);
            }
        }
    }
    out
}

/// A manifest being written. Rows are counted into its expectation.
struct Out {
    w: Option<BufWriter<File>>,
    eol: &'static [u8],
    exp: ExpectedManifest,
}

impl Out {
    fn new(dir: Option<&Path>, name: &str, crlf: bool, bom: bool) -> io::Result<Out> {
        let w = match dir {
            Some(d) => {
                let mut w = BufWriter::with_capacity(1 << 20, File::create(d.join(name))?);
                if bom {
                    w.write_all(&[0xEF, 0xBB, 0xBF])?;
                }
                w.write_all(HEADER.as_bytes())?;
                w.write_all(if crlf { b"\r\n" } else { b"\n" })?;
                Some(w)
            }
            None => None,
        };
        Ok(Out {
            w,
            eol: if crlf { b"\r\n" } else { b"\n" },
            exp: ExpectedManifest { file_name: name.into(), crlf, bom, ..Default::default() },
        })
    }

    fn row(&mut self, hash: &str, size: u64, host: &str, path: &[u8]) -> io::Result<()> {
        if let Some(w) = &mut self.w {
            w.write_all(hash.as_bytes())?;
            w.write_all(b"\t")?;
            write!(w, "{size}")?;
            w.write_all(b"\t")?;
            w.write_all(host.as_bytes())?;
            w.write_all(b"\t")?;
            w.write_all(path)?;
            w.write_all(self.eol)?;
        }
        self.exp.rows += 1;
        *self.exp.hosts.entry(host.to_string()).or_insert(0) += 1;
        if std::str::from_utf8(path).is_err() {
            self.exp.non_utf8_paths += 1;
        }
        if let Some((algo, _)) = hash.split_once(':') {
            *self.exp.flagged.entry(algo.to_string()).or_insert(0) += 1;
        }
        Ok(())
    }

    /// A line that should be rejected with `reason` (`None` = no line terminator).
    fn bad(&mut self, line: &[u8], reason: &str, terminate: bool) -> io::Result<()> {
        if let Some(w) = &mut self.w {
            w.write_all(line)?;
            if terminate {
                w.write_all(self.eol)?;
            }
        }
        *self.exp.malformed.entry(reason.to_string()).or_insert(0) += 1;
        Ok(())
    }

    fn finish(mut self) -> io::Result<ExpectedManifest> {
        if let Some(w) = &mut self.w {
            w.flush()?;
        }
        Ok(self.exp)
    }
}

/// One generated row before it is written (used where two manifests share rows).
#[derive(Clone)]
struct GenRow {
    hash: String,
    size: u64,
    path: Vec<u8>,
}

/// Rename the way copies get renamed in practice.
fn copy_name(rng: &mut Rng, name: &str, style: Style) -> String {
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) => (s, format!(".{e}")),
        None => (name, String::new()),
    };
    match (style, rng.below(3)) {
        (Style::Cloud, _) => format!("Copy of {name}"),
        (_, 0) => format!("{stem} (1){ext}"),
        (_, 1) => format!("{stem} - Copy{ext}"),
        _ => format!("{stem} (2){ext}"),
    }
}

/// Awkward but legal names for each style.
fn weird_name(rng: &mut Rng, style: Style, ext: &str, uniq: usize) -> Vec<u8> {
    let common: &[&str] = &[
        "it's done",
        "price $100 `final`",
        "-rf",
        "--help",
        "[2019] {draft} #3 & more; ok!",
        "100% done ~ v2^",
        "Zdjęcia z Łodzi — część 1",
        "日本語ファイル",
        "emoji 😀🎉",
        "name with  double  spaces",
        " leading space",
        "Ünïcödé Ñame",
        "x=1+2@home",
        "a,b,c",
        "semi;colon",
        "tab-free",
        "ŻÓŁĆ gęślą jaźń",
    ];
    let posix_only: &[&str] = &[
        "quote \"double\"",
        r"back\slash",
        "star*",
        "question?",
        "colon:name",
        "pipe|name",
        "<angle>",
        "trailing space ",
    ];
    let mut s = if style != Style::Win && rng.chance(0.4) {
        rng.pick(posix_only).to_string()
    } else {
        rng.pick(common).to_string()
    };
    if rng.chance(0.05) {
        s = format!("{} {}", "very long name".repeat(14), s);
    }
    s = format!("{s} {uniq}");
    let mut out = s.into_bytes();
    out.push(b'.');
    out.extend_from_slice(ext.as_bytes());
    out
}

fn latin1_name(rng: &mut Rng, ext: &str, uniq: usize) -> Vec<u8> {
    // Names written by an old Windows tool in Latin-1: not valid UTF-8.
    let stems: &[&[u8]] =
        &[b"caf\xe9 menu", b"r\xe9sum\xe9", b"se\xf1or", b"na\xefve", b"\xc5ngstr\xf6m", b"gar\xe7on"];
    let mut v = rng.pick(stems).to_vec();
    v.extend_from_slice(format!(" {uniq}.{ext}").as_bytes());
    v
}

fn ext_of(name: &str) -> &str {
    name.rsplit_once('.').map(|(_, e)| e).unwrap_or("bin")
}

/// Generate all manifests into `dir` (or only compute expectations when `dir` is `None`).
pub fn generate(dir: Option<&Path>, cfg: &Config) -> io::Result<Expected> {
    // calibrate catalog size so the total lands near cfg.rows
    let probe = 20_000usize;
    let measured = generate_with(None, cfg, probe)?.total_rows.max(1);
    let n_items = ((cfg.rows as f64) * probe as f64 / measured as f64).round().max(50.0) as usize;
    let mut exp = generate_with(dir, cfg, n_items)?;
    exp.target_rows = cfg.rows;
    if let Some(d) = dir {
        let f = File::create(d.join("expected.json"))?;
        serde_json::to_writer_pretty(f, &exp).map_err(io::Error::other)?;
    }
    Ok(exp)
}

fn generate_with(dir: Option<&Path>, cfg: &Config, n_items: usize) -> io::Result<Expected> {
    let cat = build_catalog(cfg.seed, n_items);
    let mut rng = Rng::new(cfg.seed);
    let hex = |c: &Catalog, id: usize, v: u64| to_hex(&c.hash(id, v));
    let photos = cat.ids(Cat::Photo);
    let by = |c: Cat| cat.ids(c);
    let mut manifests = Vec::new();
    let mut overlaps = Vec::new();

    // ---------------------------------------------------------------- TrueNAS (LF)
    {
        let mut o = Out::new(dir, "manifest-truenas.tsv", false, false)?;
        let host = "truenas";
        let put = |o: &mut Out, root: &[&str], id: usize, strip: usize| -> io::Result<()> {
            let it = &cat.items[id];
            let folder = &cat.folders[it.folder as usize][strip..];
            o.row(&hex(&cat, id, 0), it.size, host, &join(Style::Posix, root, folder, it.name.as_bytes()))
        };
        for &id in &photos {
            put(&mut o, &["mnt", "tank", "photos"], id, 1)?;
        }
        for &id in &by(Cat::HomeVideo) {
            put(&mut o, &["mnt", "tank", "photos"], id, 0)?;
        }
        for c in [Cat::Movie, Cat::Tv, Cat::Music] {
            for &id in &by(c) {
                put(&mut o, &["mnt", "tank", "media"], id, 0)?;
            }
        }
        let mut n = 0;
        for &id in &by(Cat::Doc) {
            n += 1;
            if n % 10 < 7 {
                put(&mut o, &["mnt", "backup", "archive"], id, 0)?;
            }
        }
        for c in [Cat::Archive, Cat::Vm] {
            for &id in &by(c) {
                put(&mut o, &["mnt", "backup", "archive"], id, 0)?;
            }
        }
        // a full backup of PC1's D: from 2021: duplicates inside the same source
        for &id in &photos {
            if cat.items[id].year <= 2021 {
                put(&mut o, &["mnt", "backup", "archive", "PC1-D-2021"], id, 0)?;
            }
        }
        // old laptop dump with odd names
        let old: Vec<usize> =
            by(Cat::Doc).into_iter().step_by(5).chain(by(Cat::Software).into_iter().step_by(3)).collect();
        for (k, &id) in old.iter().enumerate() {
            let it = &cat.items[id];
            let name: Vec<u8> = match k % 40 {
                0 => latin1_name(&mut rng, ext_of(&it.name), k),
                1 | 2 => weird_name(&mut rng, Style::Posix, ext_of(&it.name), k),
                _ => it.name.as_bytes().to_vec(),
            };
            o.row(
                &hex(&cat, id, 0),
                it.size,
                host,
                &join(Style::Posix, &["mnt", "backup", "archive", "old-laptop"], &[], &name),
            )?;
        }
        // case-only difference: two different files on Linux
        if let Some(&id) = photos.first() {
            let it = &cat.items[id];
            o.row(&hex(&cat, id, 0), it.size, host, b"/mnt/backup/archive/old-laptop/Photo.JPG")?;
            o.row(&hex(&cat, id, 7), it.size + 1, host, b"/mnt/backup/archive/old-laptop/photo.jpg")?;
        }
        if cfg.bad_lines {
            // a file name containing a newline: the first half is a valid row, the rest is not
            let id = old.first().copied().unwrap_or(0);
            o.row(&hex(&cat, id, 9), MB * 2, host, b"/mnt/backup/archive/old-laptop/notes")?;
            o.bad(b" part two.txt", "missing columns", true)?;
        }
        manifests.push(o.finish()?);
    }

    // ---------------------------------------------------------------- PC1 (CRLF)
    {
        let mut o = Out::new(dir, "manifest-PC1.tsv", true, false)?;
        let d = "PC1 D: (Data)";
        let c = "PC1 C: (Windows)";
        let mut k = 0u64;
        for &id in &photos {
            let it = &cat.items[id];
            k += 1;
            if it.year <= 2023 && !k.is_multiple_of(5) {
                o.row(
                    &hex(&cat, id, 0),
                    it.size,
                    d,
                    &join(Style::Win, &["D:"], &cat.folders[it.folder as usize], it.name.as_bytes()),
                )?;
            }
            if cfg.bad_lines && k == 1000 {
                o.bad(b"only\tthree\tcolumns", "missing columns", true)?;
                o.bad(
                    format!("{}\t12,345\t{d}\tD:\\x.bin", hex(&cat, id, 3)).as_bytes(),
                    "size is not a number",
                    true,
                )?;
                o.bad(format!("\t5000000\t{d}\tD:\\y.bin").as_bytes(), "empty hash", true)?;
            }
        }
        for (cat_id, every, of) in [(Cat::HomeVideo, 5, 3), (Cat::Music, 2, 1), (Cat::Doc, 5, 3), (Cat::Project, 1, 1)]
        {
            for (i, &id) in by(cat_id).iter().enumerate() {
                if (i as u64) % every < of {
                    let it = &cat.items[id];
                    o.row(
                        &hex(&cat, id, 0),
                        it.size,
                        d,
                        &join(Style::Win, &["D:"], &cat.folders[it.folder as usize], it.name.as_bytes()),
                    )?;
                }
            }
        }
        let dl = ["C:", "Users", "User", "Downloads"];
        for c_id in [Cat::Software, Cat::Iso, Cat::Archive] {
            for (i, &id) in by(c_id).iter().enumerate() {
                if (c_id == Cat::Archive && i % 5 != 0) || (c_id == Cat::Software && i % 5 >= 3) {
                    continue;
                }
                let it = &cat.items[id];
                let h = hex(&cat, id, 0);
                o.row(&h, it.size, c, &join(Style::Win, &dl, &[], it.name.as_bytes()))?;
                if i % 20 == 0 {
                    // downloaded twice
                    let n2 = copy_name(&mut rng, &it.name, Style::Win);
                    o.row(&h, it.size, c, &join(Style::Win, &dl, &[], n2.as_bytes()))?;
                }
                if i % 97 == 0 {
                    let wn = weird_name(&mut rng, Style::Win, ext_of(&it.name), id);
                    o.row(&hex(&cat, id, 11), it.size, c, &join(Style::Win, &dl, &[], &wn))?;
                }
            }
        }
        for (i, &id) in photos.iter().enumerate() {
            let it = &cat.items[id];
            if it.year >= 2023 && i % 10 < 7 {
                o.row(
                    &hex(&cat, id, 0),
                    it.size,
                    c,
                    &join(Style::Win, &["C:", "Users", "User", "Pictures", "Phone Backup"], &[], it.name.as_bytes()),
                )?;
            }
        }
        if cfg.bad_lines {
            // the file was copied while the export was still writing: truncated last line
            o.bad(b"3fa9c2e1d4b5", "missing columns", false)?;
        }
        manifests.push(o.finish()?);
    }

    // ---------------------------------------------------------------- PC2: main script + USB script
    {
        let e = "PC2 E: (Backup 2TB)";
        let mut e_rows: Vec<GenRow> = Vec::new();
        for (i, &id) in photos.iter().enumerate() {
            let it = &cat.items[id];
            if it.year <= 2022 && i % 10 != 3 {
                let mut folder = vec!["Backup".to_string(), "PC1".into(), "D".into()];
                folder.extend(cat.folders[it.folder as usize].iter().cloned());
                e_rows.push(GenRow {
                    hash: hex(&cat, id, 0),
                    size: it.size,
                    path: join(Style::Win, &["E:"], &folder, it.name.as_bytes()),
                });
            }
        }
        for (i, &id) in by(Cat::Music).iter().enumerate() {
            if i % 5 < 2 {
                let it = &cat.items[id];
                let mut folder = vec!["Backup".to_string(), "PC1".into(), "D".into()];
                folder.extend(cat.folders[it.folder as usize].iter().cloned());
                e_rows.push(GenRow {
                    hash: hex(&cat, id, 0),
                    size: it.size,
                    path: join(Style::Win, &["E:"], &folder, it.name.as_bytes()),
                });
            }
        }
        for (i, &id) in by(Cat::Doc).iter().enumerate() {
            if i % 10 < 3 {
                let it = &cat.items[id];
                let mut folder = vec!["Backup".to_string(), "Laptop".into()];
                folder.extend(cat.folders[it.folder as usize].iter().cloned());
                e_rows.push(GenRow {
                    hash: hex(&cat, id, 0),
                    size: it.size,
                    path: join(Style::Win, &["E:"], &folder, it.name.as_bytes()),
                });
            }
        }

        // main script: C: plus the USB drive E: (it scans every fixed disk)
        let mut main = Out::new(dir, "manifest-PC2.tsv", true, false)?;
        let c = "PC2 C: (OS)";
        for (i, &id) in by(Cat::Software).iter().enumerate() {
            if i % 10 < 3 {
                let it = &cat.items[id];
                main.row(
                    &hex(&cat, id, 0),
                    it.size,
                    c,
                    &join(Style::Win, &["C:", "Users", "User", "Downloads"], &[], it.name.as_bytes()),
                )?;
            }
        }
        for (i, &id) in by(Cat::Doc).iter().enumerate() {
            if i % 10 == 9 {
                let it = &cat.items[id];
                main.row(
                    &hex(&cat, id, 0),
                    it.size,
                    c,
                    &join(Style::Win, &["C:", "Users", "User", "Desktop"], &[], it.name.as_bytes()),
                )?;
            }
        }
        // the main script ran first; one percent of E: was deleted before the USB scan
        let mut in_main = Vec::with_capacity(e_rows.len());
        for (i, r) in e_rows.iter().enumerate() {
            main.row(&r.hash, r.size, e, &r.path)?;
            in_main.push(i);
        }
        manifests.push(main.finish()?);

        let mut usb = Out::new(dir, "manifest-PC2-usb.tsv", true, false)?;
        let mut shared = 0u64;
        let mut changed = 0u64;
        for (i, r) in e_rows.iter().enumerate() {
            if i % 100 == 42 {
                continue; // deleted since the main scan
            }
            shared += 1;
            if i % 50 == 7 {
                // edited since the main scan
                changed += 1;
                let mut h = crate::manifest::hex32(r.hash.as_bytes()).unwrap();
                h[0] ^= 0xFF;
                usb.row(&to_hex(&h), r.size + 1, e, &r.path)?;
            } else {
                usb.row(&r.hash, r.size, e, &r.path)?;
            }
        }
        let f = "PC2 F: (Old Seagate)";
        for (cat_id, every, of) in [(Cat::Movie, 2, 1), (Cat::Tv, 10, 3), (Cat::Music, 10, 3)] {
            for (i, &id) in by(cat_id).iter().enumerate() {
                if (i as u64) % every < of {
                    let it = &cat.items[id];
                    let mut folder = vec!["stuff".to_string()];
                    folder.extend(cat.folders[it.folder as usize].iter().cloned());
                    usb.row(&hex(&cat, id, 0), it.size, f, &join(Style::Win, &["F:"], &folder, it.name.as_bytes()))?;
                }
            }
        }
        for (i, &id) in photos.iter().enumerate() {
            let it = &cat.items[id];
            if it.year <= 2012 && i % 2 == 0 {
                let name = if i % 40 == 0 { copy_name(&mut rng, &it.name, Style::Win) } else { it.name.clone() };
                usb.row(
                    &hex(&cat, id, 0),
                    it.size,
                    f,
                    &join(Style::Win, &["F:", "New folder (2)", &it.year.to_string()], &[], name.as_bytes()),
                )?;
            }
        }
        manifests.push(usb.finish()?);
        overlaps.push(ExpectedOverlap {
            newer: "manifest-PC2.tsv".into(),
            older: "manifest-PC2-usb.tsv".into(),
            paths: shared,
            changed,
        });
    }

    // ---------------------------------------------------------------- PC3: scanned C:\ and C:\Users\User\Documents
    {
        let mut o = Out::new(dir, "manifest-PC3.tsv", true, false)?;
        let c = "PC3 C: (Windows)";
        let docs_root = ["C:", "Users", "User", "Documents"];
        let mut docs: Vec<GenRow> = Vec::new();
        for (i, &id) in by(Cat::Doc).iter().enumerate() {
            if i % 10 < 4 {
                let it = &cat.items[id];
                let folder = &cat.folders[it.folder as usize][1..];
                docs.push(GenRow {
                    hash: hex(&cat, id, 0),
                    size: it.size,
                    path: join(Style::Win, &docs_root, folder, it.name.as_bytes()),
                });
            }
        }
        for r in &docs {
            o.row(&r.hash, r.size, c, &r.path)?;
        }
        for (i, &id) in by(Cat::Project).iter().enumerate() {
            if i % 10 < 3 {
                let it = &cat.items[id];
                o.row(
                    &hex(&cat, id, 0),
                    it.size,
                    c,
                    &join(
                        Style::Win,
                        &["C:", "Users", "User", "Desktop"],
                        &cat.folders[it.folder as usize],
                        it.name.as_bytes(),
                    ),
                )?;
            }
        }
        for (i, &id) in photos.iter().enumerate() {
            let it = &cat.items[id];
            if it.year >= 2024 && i % 10 < 3 {
                o.row(
                    &hex(&cat, id, 0),
                    it.size,
                    c,
                    &join(Style::Win, &["C:", "Users", "User", "Pictures"], &[], it.name.as_bytes()),
                )?;
            }
        }
        // second -Paths entry: Documents again, with the folder written in a different case
        for r in &docs {
            let p = String::from_utf8_lossy(&r.path).replacen(r"\Documents\", r"\documents\", 1);
            o.row(&r.hash, r.size, c, p.as_bytes())?;
        }
        let mut exp = o.finish()?;
        exp.duplicate_listings = docs.len() as u64;
        manifests.push(exp);
    }

    // ---------------------------------------------------------------- Google Drive (CRLF + BOM)
    {
        let mut o = Out::new(dir, "manifest-cloud-gdrive.tsv", true, true)?;
        let host = "gdrive:";
        let mut same_path = 0u64;
        let mut n = 0u64;
        let mut emit = |o: &mut Out, rng: &mut Rng, id: usize, folder: Vec<String>| -> io::Result<()> {
            let it = &cat.items[id];
            n += 1;
            let hash = if n % 200 == 17 {
                // older uploads without SHA-256: the export script fell back to MD5 / SHA-1
                format!("md5:{}", &hex(&cat, id, 0)[..32])
            } else if n % 1000 == 501 {
                format!("sha1:{}", &hex(&cat, id, 0)[..40])
            } else {
                hex(&cat, id, 0)
            };
            let name = if n % 150 == 3 { copy_name(rng, &it.name, Style::Cloud) } else { it.name.clone() };
            let path = join(Style::Cloud, &[host], &folder, name.as_bytes());
            o.row(&hash, it.size, host, &path)?;
            if n % 500 == 250 {
                // Google Drive allows two files with the same name in one folder
                let h2 = if n % 1000 == 250 { hash.clone() } else { hex(&cat, id, 5) };
                o.row(&h2, it.size, host, &path)?;
                same_path += 1;
            }
            Ok(())
        };
        for (i, &id) in photos.iter().enumerate() {
            let it = &cat.items[id];
            if it.year >= 2015 && i % 10 < 4 {
                emit(&mut o, &mut rng, id, vec!["Google Photos".into(), it.year.to_string()])?;
            }
        }
        for (i, &id) in by(Cat::Doc).iter().enumerate() {
            if i % 10 < 3 {
                let it = &cat.items[id];
                emit(&mut o, &mut rng, id, cat.folders[it.folder as usize].clone())?;
            }
        }
        let mut exp = o.finish()?;
        exp.same_path_cloud = same_path;
        manifests.push(exp);
    }

    // ---------------------------------------------------------------- OneDrive: SHA-256 (download) and QuickXorHash listings
    {
        let host = "onedrive:";
        let mut rows: Vec<(usize, Vec<u8>, u64)> = Vec::new();
        for (i, &id) in by(Cat::Doc).iter().enumerate() {
            if i % 4 == 1 {
                let it = &cat.items[id];
                rows.push((
                    id,
                    join(Style::Cloud, &[host], &cat.folders[it.folder as usize], it.name.as_bytes()),
                    it.size,
                ));
            }
        }
        for (i, &id) in photos.iter().enumerate() {
            let it = &cat.items[id];
            if it.year >= 2020 && i % 5 == 0 {
                rows.push((
                    id,
                    join(Style::Cloud, &[host, "Pictures", "Camera Roll"], &[], it.name.as_bytes()),
                    it.size,
                ));
            }
        }
        let mut sha = Out::new(dir, "manifest-cloud-onedrive-sha256.tsv", true, false)?;
        for (id, p, size) in &rows {
            sha.row(&hex(&cat, *id, 0), *size, host, p)?;
        }
        manifests.push(sha.finish()?);
        let mut qx = Out::new(dir, "manifest-cloud-onedrive.tsv", true, false)?;
        let mut shared = 0u64;
        for (k, (id, p, size)) in rows.iter().enumerate() {
            if k % 10 < 3 {
                let h = cat.hash(*id, 0);
                let mut b64 = String::new();
                const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
                for chunk in h[..20].chunks(3) {
                    let v = (chunk[0] as u32) << 16
                        | (*chunk.get(1).unwrap_or(&0) as u32) << 8
                        | *chunk.get(2).unwrap_or(&0) as u32;
                    for s in [18, 12, 6, 0] {
                        b64.push(T[((v >> s) & 63) as usize] as char);
                    }
                }
                b64.truncate(27);
                b64.push('=');
                qx.row(&format!("quickxor:{b64}"), *size, host, p)?;
                shared += 1;
            }
        }
        manifests.push(qx.finish()?);
        overlaps.push(ExpectedOverlap {
            newer: "manifest-cloud-onedrive.tsv".into(),
            older: "manifest-cloud-onedrive-sha256.tsv".into(),
            paths: shared,
            changed: shared,
        });
    }

    let total_rows = manifests.iter().map(|m| m.rows).sum();
    Ok(Expected { seed: cfg.seed, target_rows: cfg.rows, total_rows, manifests, overlaps })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_and_close_to_target() {
        let cfg = Config { rows: 30_000, seed: 7, bad_lines: true };
        let a = generate(None, &cfg).unwrap();
        let b = generate(None, &cfg).unwrap();
        assert_eq!(a, b);
        let ratio = a.total_rows as f64 / cfg.rows as f64;
        assert!((0.9..1.1).contains(&ratio), "total {} vs target {}", a.total_rows, cfg.rows);
        assert_eq!(a.manifests.len(), 8);
    }

    #[test]
    fn rng_is_uniform_enough() {
        let mut r = Rng::new(1);
        let mut buckets = [0u32; 10];
        for _ in 0..100_000 {
            buckets[r.below(10) as usize] += 1;
        }
        for b in buckets {
            assert!((9_000..11_000).contains(&b), "{buckets:?}");
        }
    }
}
