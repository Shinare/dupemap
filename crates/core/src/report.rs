//! Import reports: everything noteworthy about one manifest import, persisted as JSON with the
//! manifest so alerts survive restarts.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// How loudly the app should surface a report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Severity {
    /// Nothing unusual.
    Ok,
    /// Worth knowing (overlapping manifests, duplicate listings, non-UTF-8 names...).
    Notice,
    /// Needs attention: non-SHA-256 hashes, malformed lines, missing header.
    Alert,
}

impl Severity {
    pub fn code(self) -> i64 {
        match self {
            Severity::Ok => 0,
            Severity::Notice => 1,
            Severity::Alert => 2,
        }
    }

    pub fn from_code(c: i64) -> Severity {
        match c {
            2 => Severity::Alert,
            1 => Severity::Notice,
            _ => Severity::Ok,
        }
    }
}

/// Another manifest that listed some of the same paths.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Overlap {
    pub manifest_id: i64,
    pub file_name: String,
    /// Paths listed by both manifests.
    pub paths: u64,
    /// ...of which the size or hash differs (the file changed between scans).
    pub changed: u64,
    /// Paths where this manifest's listing is the one shown.
    pub this_shown: u64,
    /// Paths where the other manifest's listing is kept (it has SHA-256 and this one doesn't).
    pub other_shown: u64,
}

/// An example line kept for the report dialog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sample {
    pub line: u64,
    /// "flagged", "malformed", "duplicate listing", "same path", "overlap", "non-UTF-8 name", "mixed path styles"
    pub category: String,
    pub detail: String,
    pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportReport {
    pub manifest_id: i64,
    pub file_name: String,
    pub file_path: String,
    pub file_size: u64,
    /// Manifest file date (seconds since 1970), used to decide which of two overlapping scans is newer.
    pub file_mtime: Option<i64>,
    pub encoding: String,
    pub line_endings: String,
    pub header_found: bool,
    pub lines: u64,
    pub blank_lines: u64,
    /// Rows stored (SHA-256 and flagged, including hidden duplicate listings).
    pub rows_imported: u64,
    pub rows_sha256: u64,
    /// `sha256:<hex>` rows accepted as SHA-256.
    pub sha256_prefixed: u64,
    /// Non-SHA-256 rows by algorithm. Any entry here raises an alert.
    pub flagged: BTreeMap<String, u64>,
    /// Skipped lines by reason. Any entry here raises an alert.
    pub malformed: BTreeMap<String, u64>,
    pub non_utf8_paths: u64,
    /// Rows per host (source).
    pub hosts: BTreeMap<String, u64>,
    pub mixed_kinds: Vec<String>,
    /// Same path listed more than once in this manifest (local disks): extra listings hidden.
    pub duplicate_listings: u64,
    /// Same path listed more than once on a cloud remote (Google Drive allows identical names):
    /// kept as separate files.
    pub same_path_cloud: u64,
    pub overlaps: Vec<Overlap>,
    pub bytes_total: u64,
    pub samples: Vec<Sample>,
    pub elapsed_ms: u64,
}

impl ImportReport {
    pub fn flagged_total(&self) -> u64 {
        self.flagged.values().sum()
    }

    pub fn malformed_total(&self) -> u64 {
        self.malformed.values().sum()
    }

    pub fn severity(&self) -> Severity {
        if self.flagged_total() > 0 || self.malformed_total() > 0 || !self.header_found {
            Severity::Alert
        } else if self.duplicate_listings > 0
            || self.same_path_cloud > 0
            || !self.overlaps.is_empty()
            || !self.mixed_kinds.is_empty()
            || self.non_utf8_paths > 0
        {
            Severity::Notice
        } else {
            Severity::Ok
        }
    }

    /// Plain-language summary, one finding per line, most important first.
    pub fn findings(&self) -> Vec<String> {
        let mut out = Vec::new();
        let fl = self.flagged_total();
        if fl > 0 {
            let by: Vec<String> = self.flagged.iter().map(|(a, n)| format!("{a}: {n}")).collect();
            out.push(format!(
                "ALERT: {fl} row(s) are not SHA-256 ({}). They are imported but only match rows of the same hash type.",
                by.join(", ")
            ));
        }
        let mal = self.malformed_total();
        if mal > 0 {
            let by: Vec<String> = self.malformed.iter().map(|(a, n)| format!("{a}: {n}")).collect();
            out.push(format!("ALERT: {mal} malformed line(s) skipped ({}).", by.join(", ")));
        }
        if !self.header_found {
            out.push(
                "ALERT: header line \"hash<TAB>size<TAB>host<TAB>path\" not found; is this a DupeMap manifest?".into(),
            );
        }
        if self.duplicate_listings > 0 {
            out.push(format!(
                "{} path(s) were listed more than once in this manifest (overlapping scan folders?); extra listings are hidden so they never count as copies.",
                self.duplicate_listings
            ));
        }
        if self.same_path_cloud > 0 {
            out.push(format!(
                "{} cloud path(s) appear more than once; cloud drives allow identical names, so they are kept as separate files.",
                self.same_path_cloud
            ));
        }
        for o in &self.overlaps {
            out.push(format!(
                "{} path(s) are also listed by {} ({} differ in size or hash). Each path counts once: this manifest's listing is shown for {}, the other's for {} (SHA-256 first, then the newer scan).",
                o.paths, o.file_name, o.changed, o.this_shown, o.other_shown
            ));
        }
        for m in &self.mixed_kinds {
            out.push(m.clone());
        }
        if self.non_utf8_paths > 0 {
            out.push(format!(
                "{} path(s) contain bytes that are not valid UTF-8; shown with \u{FFFD}, exact bytes kept for scripts.",
                self.non_utf8_paths
            ));
        }
        if self.sha256_prefixed > 0 {
            out.push(format!("{} row(s) used the \"sha256:\" prefix; accepted as SHA-256.", self.sha256_prefixed));
        }
        out
    }
}

/// Collects up to `cap` samples per category/detail pair.
#[derive(Debug, Default)]
pub(crate) struct Samples {
    counts: BTreeMap<(String, String), usize>,
    pub(crate) list: Vec<Sample>,
}

impl Samples {
    pub(crate) const CAP: usize = 10;
    pub(crate) const MAX_TEXT: usize = 300;

    pub(crate) fn add(&mut self, line: u64, category: &str, detail: &str, text: &[u8]) {
        let n = self.counts.entry((category.to_string(), detail.to_string())).or_insert(0);
        if *n >= Self::CAP {
            return;
        }
        *n += 1;
        let mut t = String::from_utf8_lossy(text).into_owned();
        if t.len() > Self::MAX_TEXT {
            let mut cut = Self::MAX_TEXT;
            while !t.is_char_boundary(cut) {
                cut -= 1;
            }
            t.truncate(cut);
            t.push('…');
        }
        self.list
            .push(Sample { line, category: category.into(), detail: detail.into(), text: t.replace('\t', " ⇥ ") });
    }
}
