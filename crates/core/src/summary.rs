//! Breakdowns by file type. Extensions are grouped into categories (Video, Audio, ...) that the
//! user can edit; the mapping is stored in the database. For any source, or all of them, the
//! summary gives totals per category, per extension and per source, each split by duplication
//! status: the only copy anywhere, copies only within the file's own source, or a copy in
//! another source.

use crate::index::{FileIx, Index, SourceIx};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A named group of extensions.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Category {
    pub name: String,
    /// Lower-case extensions without the dot: `mkv`, `tar.gz`.
    pub exts: Vec<String>,
}

/// The category mapping. Extensions in no category count as "Other".
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Categories {
    pub list: Vec<Category>,
}

/// Name of the implicit last category.
pub const OTHER: &str = "Other";

const DEFAULTS: &[(&str, &str)] = &[
    (
        "Video",
        "mp4 mkv avi mov wmv m4v mpg mpeg mpe m2ts mts ts vob webm flv f4v 3gp 3g2 divx xvid ogv rm rmvb asf mxf dv \
         h264 h265 hevc m2v",
    ),
    ("Audio", "mp3 flac wav m4a aac ogg oga opus wma aif aiff alac ape dsf dff mka wv mid midi m4b amr ac3 dts mpc"),
    (
        "Images",
        "jpg jpeg jpe jfif png heic heif gif bmp tif tiff webp avif jxl raw cr2 cr3 crw nef nrw arw srf sr2 dng orf \
         rw2 raf srw pef x3f 3fr erf kdc mrw psd psb xcf svg ico exr hdr tga dds kra",
    ),
    (
        "Documents",
        "pdf doc docx docm dot dotx xls xlsx xlsm xlsb ppt pptx pptm pps ppsx odt ods odp odg rtf txt md epub mobi \
         azw azw3 djvu xps oxps pages numbers key csv tsv one vsd vsdx pub chm tex msg eml mbox pst ost",
    ),
    (
        "Archives",
        "zip zipx 7z rar tar tar.gz tgz tar.bz2 tbz tbz2 tar.xz txz tar.zst tar.lz tar.lzma gz bz2 xz zst lz lzma \
         lz4 cab arj lzh lha z sit sitx ace cpio",
    ),
    (
        "Disk images & backups",
        "iso img dmg vhd vhdx vmdk vdi qcow2 qcow ova wim esd swm nrg mdf mds cue toast tib tibx vbk vib vrb gho \
         pbd mrimg sparseimage hds pvm bkf bak backup",
    ),
    (
        "Software",
        "exe msi msix msixbundle appx appxbundle msp msu dll sys ocx drv efi apk xapk aab ipa deb rpm pkg mpkg \
         appimage flatpak snap so dylib run",
    ),
    (
        "Development",
        "pdb lib a o obj pch ipch rlib rmeta class jar war ear whl nupkg node wasm pyd pyc ilk idb exp bc tlog \
         dmp mdmp core hprof",
    ),
    ("Databases", "db sqlite sqlite3 db3 mdb accdb sql dbf frm ibd myd myi ldf ndf realm kdbx edb wal journal"),
    (
        "Game data",
        "pak vpk bsa ba2 wad upk uasset uexp ubulk utoc ucas forge bik bk2 gcf ncf sga rpf wem bnk xnb mpq \
         assets resource",
    ),
];

impl Default for Categories {
    fn default() -> Self {
        Categories {
            list: DEFAULTS
                .iter()
                .map(|(name, exts)| Category { name: name.to_string(), exts: parse_exts(exts) })
                .collect(),
        }
    }
}

/// Extensions typed by a person: separated by spaces, commas or semicolons, with or without a
/// leading `.` or `*.`, any case. Returns them lower-cased, in order, without repeats.
pub fn parse_exts(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in text.split(|c: char| c.is_whitespace() || c == ',' || c == ';') {
        let e = raw.trim().trim_start_matches('*').trim_start_matches('.').trim().to_lowercase();
        if !e.is_empty() && !out.contains(&e) {
            out.push(e);
        }
    }
    out
}

impl Categories {
    /// Clean up a mapping a person edited: names trimmed (empty ones become "Unnamed"),
    /// extensions normalised, an extension kept only in the first category listing it, and no
    /// category may be called "Other" (that one is implicit).
    pub fn normalized(&self) -> Categories {
        let mut seen: HashMap<String, ()> = HashMap::new();
        let mut list = Vec::new();
        for c in &self.list {
            let mut name = c.name.trim().to_string();
            if name.is_empty() {
                name = "Unnamed".into();
            }
            if name.eq_ignore_ascii_case(OTHER) {
                name = format!("{name} (2)");
            }
            let exts: Vec<String> =
                parse_exts(&c.exts.join(" ")).into_iter().filter(|e| seen.insert(e.clone(), ()).is_none()).collect();
            list.push(Category { name, exts });
        }
        Categories { list }
    }

    /// Category names, with "Other" last.
    pub fn names(&self) -> Vec<String> {
        self.list.iter().map(|c| c.name.clone()).chain(std::iter::once(OTHER.to_string())).collect()
    }

    /// Index of the category holding `ext`, or `self.list.len()` for "Other".
    pub fn category_of(&self, ext: &str) -> usize {
        self.list.iter().position(|c| c.exts.iter().any(|e| e == ext)).unwrap_or(self.list.len())
    }
}

/// Files and bytes, split by duplication status. `files == unique_files + within_files +
/// elsewhere_files`, and the same for bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Split {
    pub files: u64,
    pub bytes: u64,
    /// The only copy anywhere.
    pub unique_files: u64,
    pub unique_bytes: u64,
    /// Other copies exist, all in the file's own source.
    pub within_files: u64,
    pub within_bytes: u64,
    /// At least one copy is in another source.
    pub elsewhere_files: u64,
    pub elsewhere_bytes: u64,
}

impl Split {
    fn add(&mut self, status: Status, bytes: u64) {
        self.files += 1;
        self.bytes += bytes;
        match status {
            Status::Unique => {
                self.unique_files += 1;
                self.unique_bytes += bytes;
            }
            Status::Within => {
                self.within_files += 1;
                self.within_bytes += bytes;
            }
            Status::Elsewhere => {
                self.elsewhere_files += 1;
                self.elsewhere_bytes += bytes;
            }
        }
    }

    pub fn merge(&mut self, o: &Split) {
        self.files += o.files;
        self.bytes += o.bytes;
        self.unique_files += o.unique_files;
        self.unique_bytes += o.unique_bytes;
        self.within_files += o.within_files;
        self.within_bytes += o.within_bytes;
        self.elsewhere_files += o.elsewhere_files;
        self.elsewhere_bytes += o.elsewhere_bytes;
    }

    /// Bytes of files with at least one other copy anywhere.
    pub fn duplicated_bytes(&self) -> u64 {
        self.within_bytes + self.elsewhere_bytes
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Unique,
    Within,
    Elsewhere,
}

/// One extension's totals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtRow {
    /// Lower-case, without the dot; empty for files without an extension.
    pub ext: String,
    /// Index into [`Summary::categories`].
    pub category: usize,
    pub split: Split,
}

/// Totals for one source or for all of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    /// `None` = every source.
    pub scope: Option<SourceIx>,
    pub total: Split,
    /// Category names, "Other" last; parallel to `categories`.
    pub names: Vec<String>,
    pub categories: Vec<Split>,
    /// Every extension present, most bytes first.
    pub exts: Vec<ExtRow>,
    /// Each source's own totals, in source order (only for the all-sources summary).
    pub sources: Vec<(SourceIx, Split)>,
}

impl Summary {
    /// A summary of nothing (for a source that no longer exists).
    pub fn empty(cats: &Categories) -> Summary {
        let names = cats.names();
        let categories = vec![Split::default(); names.len()];
        Summary { scope: None, total: Split::default(), names, categories, exts: Vec::new(), sources: Vec::new() }
    }
}

impl Index {
    /// Totals by category, extension and source for one source (`Some`) or every source.
    pub fn summary(&self, scope: Option<SourceIx>, cats: &Categories) -> Summary {
        let names = cats.names();
        // category of every interned extension, looked up once
        let n_ext = self.ext_count();
        let ext_cat: Vec<usize> = (0..n_ext).map(|e| cats.category_of(self.ext_name(e as u32))).collect();
        let mut per_ext = vec![Split::default(); n_ext];
        let mut per_source = vec![Split::default(); self.sources().len()];
        let files: std::ops::Range<FileIx> = match scope {
            Some(s) => self.subtree_files(self.sources()[s as usize].root),
            None => 0..self.file_count() as FileIx,
        };
        for f in files {
            let status = self.status_of(f);
            let size = self.file_size(f);
            per_ext[self.file_ext_id(f) as usize].add(status, size);
            if scope.is_none() {
                per_source[self.file_source(f) as usize].add(status, size);
            }
        }
        let mut categories = vec![Split::default(); names.len()];
        let mut total = Split::default();
        let mut exts: Vec<ExtRow> = Vec::new();
        for (e, split) in per_ext.into_iter().enumerate() {
            if split.files == 0 {
                continue;
            }
            categories[ext_cat[e]].merge(&split);
            total.merge(&split);
            exts.push(ExtRow { ext: self.ext_name(e as u32).to_string(), category: ext_cat[e], split });
        }
        exts.sort_by(|a, b| b.split.bytes.cmp(&a.split.bytes).then_with(|| a.ext.cmp(&b.ext)));
        let sources = match scope {
            Some(_) => Vec::new(),
            None => per_source.into_iter().enumerate().map(|(s, sp)| (s as SourceIx, sp)).collect(),
        };
        Summary { scope, total, names, categories, exts, sources }
    }

    fn status_of(&self, f: FileIx) -> Status {
        let (copies, sources) = self.copy_spread(self.file_content(f));
        if copies <= 1 {
            Status::Unique
        } else if sources <= 1 {
            Status::Within
        } else {
            Status::Elsewhere
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_extensions_are_normalised() {
        assert_eq!(parse_exts(" .MKV, *.mp4;avi  mkv\ttar.GZ "), vec!["mkv", "mp4", "avi", "tar.gz"]);
        assert!(parse_exts(" , ;").is_empty());
    }

    #[test]
    fn defaults_are_clean_and_each_extension_is_in_one_category() {
        let d = Categories::default();
        assert_eq!(d, d.normalized(), "the built-in mapping needs no cleaning");
        let mut seen = std::collections::HashSet::new();
        for c in &d.list {
            assert!(!c.exts.is_empty(), "{} has extensions", c.name);
            for e in &c.exts {
                assert!(seen.insert(e.clone()), "{e} is listed twice");
            }
        }
        assert_eq!(d.category_of("mkv"), 0);
        assert_eq!(d.category_of("nonsense"), d.list.len());
        assert_eq!(d.names().last().map(String::as_str), Some(OTHER));
    }

    #[test]
    fn normalising_an_edited_mapping() {
        let edited = Categories {
            list: vec![
                Category { name: "  Films ".into(), exts: vec!["MKV".into(), ".mp4".into()] },
                Category { name: "".into(), exts: vec!["mkv".into(), "iso".into()] },
                Category { name: "other".into(), exts: vec![] },
            ],
        };
        let n = edited.normalized();
        assert_eq!(n.list[0], Category { name: "Films".into(), exts: vec!["mkv".into(), "mp4".into()] });
        assert_eq!(n.list[1], Category { name: "Unnamed".into(), exts: vec!["iso".into()] }, "mkv stays in Films");
        assert_eq!(n.list[2].name, "other (2)", "Other is reserved");
    }
}
