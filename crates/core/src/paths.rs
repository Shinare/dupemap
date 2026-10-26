//! Path model.
//!
//! Every manifest path is split into an *anchor* (drive letter, UNC share, `/` or an rclone
//! remote such as `gdrive:`) followed by its components. The anchor decides how the path is
//! joined back together and whether names compare case-insensitively (Windows only).
//!
//! Components are kept as raw bytes when they are not valid UTF-8 (possible on Linux/TrueNAS),
//! so delete scripts can later reproduce the exact file name.

use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::fmt::Write as _;

/// Path style, decided per path from its anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PathKind {
    /// `D:\...` or `\\server\share\...`
    Windows,
    /// `/mnt/pool/...`
    Posix,
    /// rclone remote, `gdrive:/...` or `onedrive:Folder/...`
    Cloud,
    /// Anything else (no recognisable root).
    Relative,
}

impl PathKind {
    pub fn code(self) -> i64 {
        match self {
            PathKind::Windows => 1,
            PathKind::Posix => 2,
            PathKind::Cloud => 3,
            PathKind::Relative => 4,
        }
    }

    pub fn from_code(c: i64) -> PathKind {
        match c {
            1 => PathKind::Windows,
            2 => PathKind::Posix,
            3 => PathKind::Cloud,
            _ => PathKind::Relative,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            PathKind::Windows => "Windows",
            PathKind::Posix => "Linux",
            PathKind::Cloud => "Cloud (rclone)",
            PathKind::Relative => "Relative",
        }
    }

    /// Windows file systems compare names case-insensitively.
    pub fn case_insensitive(self) -> bool {
        self == PathKind::Windows
    }
}

/// One path component. `raw` holds the original bytes when they are not valid UTF-8; `text`
/// is then a lossy rendering for display only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comp<'a> {
    pub text: Cow<'a, str>,
    pub raw: Option<&'a [u8]>,
}

impl<'a> Comp<'a> {
    pub fn from_bytes(b: &'a [u8]) -> Self {
        match std::str::from_utf8(b) {
            Ok(s) => Comp { text: Cow::Borrowed(s), raw: None },
            Err(_) => Comp { text: Cow::Owned(String::from_utf8_lossy(b).into_owned()), raw: Some(b) },
        }
    }

    /// Key deciding whether two entries in the same folder are the same entry
    /// (`None` = the text itself).
    pub fn key(&self, kind: PathKind) -> Option<String> {
        name_key(&self.text, self.raw, kind)
    }
}

/// Comparison key for a name, or `None` when the name itself is the key.
///
/// * Windows names are case-folded.
/// * Non-UTF-8 names are keyed on their exact bytes (two different byte sequences can render
///   to the same lossy text). The key starts with NUL, which can never appear in a real name.
pub fn name_key(text: &str, raw: Option<&[u8]>, kind: PathKind) -> Option<String> {
    if let Some(raw) = raw {
        let mut k = String::with_capacity(1 + raw.len() * 2);
        k.push('\0');
        for b in raw {
            let _ = write!(k, "{b:02x}");
        }
        return Some(k);
    }
    if kind.case_insensitive() {
        let folded = fold_case(text);
        if folded != text {
            return Some(folded);
        }
    }
    None
}

/// Simple per-character lower-casing, close to how NTFS compares names.
pub fn fold_case(s: &str) -> String {
    if s.is_ascii() {
        return s.to_ascii_lowercase();
    }
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        out.extend(c.to_lowercase());
    }
    out
}

/// A parsed manifest path: anchor plus components (folders, then the file name last).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedPath<'a> {
    pub kind: PathKind,
    pub anchor: Cow<'a, str>,
    pub parts: Vec<Comp<'a>>,
}

impl ParsedPath<'_> {
    /// The path joined back together in its canonical form.
    pub fn display(&self) -> String {
        join_path(&self.anchor, self.parts.iter().map(|c| c.text.as_ref()))
    }
}

/// Parse a manifest path (raw bytes from the TSV).
pub fn parse_path(p: &[u8]) -> ParsedPath<'_> {
    if let Some(rest) = p.strip_prefix(br"\\?\UNC\") {
        return parse_unc(rest);
    }
    let p = p.strip_prefix(br"\\?\").unwrap_or(p);
    if is_drive(p) {
        let letter = (p[0] as char).to_ascii_uppercase();
        return ParsedPath {
            kind: PathKind::Windows,
            anchor: Cow::Owned(format!("{letter}:")),
            parts: split(&p[2..], |b| b == b'\\' || b == b'/'),
        };
    }
    if let Some(rest) = p.strip_prefix(br"\\") {
        return parse_unc(rest);
    }
    if let Some(rest) = p.strip_prefix(b"/") {
        return ParsedPath { kind: PathKind::Posix, anchor: Cow::Borrowed("/"), parts: split(rest, |b| b == b'/') };
    }
    if let Some(i) = remote_colon(p) {
        let anchor = match std::str::from_utf8(&p[..=i]) {
            Ok(s) => Cow::Borrowed(s),
            Err(_) => Cow::Owned(String::from_utf8_lossy(&p[..=i]).into_owned()),
        };
        return ParsedPath { kind: PathKind::Cloud, anchor, parts: split(&p[i + 1..], |b| b == b'/') };
    }
    ParsedPath { kind: PathKind::Relative, anchor: Cow::Borrowed("."), parts: split(p, |b| b == b'/' || b == b'\\') }
}

fn is_drive(p: &[u8]) -> bool {
    p.len() >= 2 && p[0].is_ascii_alphabetic() && p[1] == b':' && (p.len() == 2 || p[2] == b'\\' || p[2] == b'/')
}

fn parse_unc(rest: &[u8]) -> ParsedPath<'_> {
    let comps = split(rest, |b| b == b'\\' || b == b'/');
    if comps.len() < 2 {
        let server = comps.first().map(|c| c.text.to_string()).unwrap_or_default();
        return ParsedPath { kind: PathKind::Windows, anchor: Cow::Owned(format!(r"\\{server}")), parts: Vec::new() };
    }
    let anchor = format!(r"\\{}\{}", comps[0].text, comps[1].text);
    ParsedPath { kind: PathKind::Windows, anchor: Cow::Owned(anchor), parts: comps.into_iter().skip(2).collect() }
}

/// Position of the colon in an rclone remote prefix (`name:`), which must come before any
/// separator and have a name of at least two characters (one letter would be a drive).
fn remote_colon(p: &[u8]) -> Option<usize> {
    for (i, &b) in p.iter().enumerate() {
        match b {
            b':' => return if i >= 2 { Some(i) } else { None },
            b'/' | b'\\' => return None,
            _ => {}
        }
    }
    None
}

fn split(bytes: &[u8], is_sep: impl Fn(u8) -> bool) -> Vec<Comp<'_>> {
    bytes.split(|&b| is_sep(b)).filter(|c| !c.is_empty()).map(Comp::from_bytes).collect()
}

/// Path style implied by an anchor name as stored in the folder tree.
pub fn anchor_kind(anchor: &str) -> PathKind {
    let b = anchor.as_bytes();
    if anchor == "/" {
        PathKind::Posix
    } else if anchor.starts_with(r"\\") || (b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':') {
        PathKind::Windows
    } else if anchor.len() > 2 && anchor.ends_with(':') {
        PathKind::Cloud
    } else {
        PathKind::Relative
    }
}

/// Join an anchor and components into a full path, using the anchor's conventions:
/// `D:\a\b`, `\\srv\share\a`, `/mnt/a`, `gdrive:/a/b`, `a/b`.
pub fn join_path<'s>(anchor: &str, parts: impl IntoIterator<Item = &'s str>) -> String {
    let kind = anchor_kind(anchor);
    let mut out = String::with_capacity(64);
    let sep = match kind {
        PathKind::Windows => {
            out.push_str(anchor);
            out.push('\\');
            '\\'
        }
        PathKind::Posix => {
            out.push('/');
            '/'
        }
        PathKind::Cloud => {
            out.push_str(anchor);
            out.push('/');
            '/'
        }
        PathKind::Relative => '/',
    };
    for (i, p) in parts.into_iter().enumerate() {
        if i > 0 {
            out.push(sep);
        }
        out.push_str(p);
    }
    out
}

/// Lower-cased extension without the dot (`""` if none). Compound tar extensions are kept
/// together (`tar.gz`). Hidden files such as `.bashrc` have no extension.
pub fn extension(name: &str) -> String {
    let Some((base, ext)) = name.rsplit_once('.') else {
        return String::new();
    };
    if base.is_empty() || ext.is_empty() || ext.len() > 12 || ext.contains(' ') {
        return String::new();
    }
    let ext = fold_case(ext);
    if matches!(ext.as_str(), "gz" | "bz2" | "xz" | "zst" | "lz" | "lzma") {
        let lb = base.to_ascii_lowercase();
        if lb.ends_with(".tar") && lb.len() > 4 {
            return format!("tar.{ext}");
        }
    }
    ext
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(p: &ParsedPath) -> Vec<String> {
        p.parts.iter().map(|c| c.text.to_string()).collect()
    }

    #[test]
    fn windows_drive() {
        let p = parse_path(br"d:\Videos\Show S01\ep 1.mkv");
        assert_eq!(p.kind, PathKind::Windows);
        assert_eq!(p.anchor, "D:");
        assert_eq!(texts(&p), ["Videos", "Show S01", "ep 1.mkv"]);
        assert_eq!(p.display(), r"D:\Videos\Show S01\ep 1.mkv");
    }

    #[test]
    fn windows_forward_slashes_and_doubled_separators() {
        let p = parse_path(br"C:/Users//User\Downloads\\x.iso");
        assert_eq!(p.kind, PathKind::Windows);
        assert_eq!(texts(&p), ["Users", "User", "Downloads", "x.iso"]);
        assert_eq!(p.display(), r"C:\Users\User\Downloads\x.iso");
    }

    #[test]
    fn long_path_prefixes() {
        let p = parse_path(br"\\?\E:\Backup\a.zip");
        assert_eq!(p.anchor, "E:");
        assert_eq!(p.display(), r"E:\Backup\a.zip");
        let u = parse_path(br"\\?\UNC\nas\share\dir\f.bin");
        assert_eq!(u.anchor, r"\\nas\share");
        assert_eq!(u.display(), r"\\nas\share\dir\f.bin");
    }

    #[test]
    fn unc() {
        let p = parse_path(br"\\TrueNAS\media\Movies\a b.mkv");
        assert_eq!(p.kind, PathKind::Windows);
        assert_eq!(p.anchor, r"\\TrueNAS\media");
        assert_eq!(texts(&p), ["Movies", "a b.mkv"]);
        assert_eq!(p.display(), r"\\TrueNAS\media\Movies\a b.mkv");
    }

    #[test]
    fn posix_backslash_is_a_normal_character() {
        let p = parse_path(br"/mnt/tank/odd\name.txt");
        assert_eq!(p.kind, PathKind::Posix);
        assert_eq!(texts(&p), ["mnt", "tank", r"odd\name.txt"]);
        assert_eq!(p.display(), r"/mnt/tank/odd\name.txt");
    }

    #[test]
    fn cloud_with_and_without_leading_slash() {
        let a = parse_path(b"gdrive:/TV/Show.mkv");
        assert_eq!(a.kind, PathKind::Cloud);
        assert_eq!(a.anchor, "gdrive:");
        assert_eq!(texts(&a), ["TV", "Show.mkv"]);
        let b = parse_path(b"onedrive:Photos/x.jpg");
        assert_eq!(b.anchor, "onedrive:");
        assert_eq!(texts(&b), ["Photos", "x.jpg"]);
        assert_eq!(b.display(), "onedrive:/Photos/x.jpg");
    }

    #[test]
    fn relative_and_degenerate() {
        let p = parse_path(b"some/rel\\path.txt");
        assert_eq!(p.kind, PathKind::Relative);
        assert_eq!(texts(&p), ["some", "rel", "path.txt"]);
        assert_eq!(p.display(), "some/rel/path.txt");
        let d = parse_path(br"D:\");
        assert_eq!(d.kind, PathKind::Windows);
        assert!(d.parts.is_empty());
        assert_eq!(d.display(), r"D:\");
        // single letter + colon without a separator is not a cloud remote
        assert_eq!(parse_path(b"D:file.txt").kind, PathKind::Relative);
    }

    #[test]
    fn non_utf8_components_keep_raw_bytes() {
        let p = parse_path(b"/mnt/tank/caf\xe9/menu.pdf");
        let c = &p.parts[2];
        assert_eq!(c.raw, Some(&b"caf\xe9"[..]));
        assert!(c.text.contains('\u{FFFD}'));
        let k1 = c.key(PathKind::Posix).unwrap();
        let other = parse_path(b"/mnt/tank/caf\xe8/menu.pdf");
        let k2 = other.parts[2].key(PathKind::Posix).unwrap();
        assert_ne!(k1, k2, "different bytes must never share a key");
        assert!(k1.starts_with('\0'));
    }

    #[test]
    fn keys_fold_case_only_on_windows() {
        let w = Comp::from_bytes("Zdjęcia Łódź.JPG".as_bytes());
        assert_eq!(w.key(PathKind::Windows).as_deref(), Some("zdjęcia łódź.jpg"));
        assert_eq!(w.key(PathKind::Posix), None);
        assert_eq!(Comp::from_bytes(b"lower.jpg").key(PathKind::Windows), None);
    }

    #[test]
    fn anchor_kinds() {
        assert_eq!(anchor_kind("/"), PathKind::Posix);
        assert_eq!(anchor_kind("C:"), PathKind::Windows);
        assert_eq!(anchor_kind(r"\\nas\share"), PathKind::Windows);
        assert_eq!(anchor_kind("gdrive:"), PathKind::Cloud);
        assert_eq!(anchor_kind("."), PathKind::Relative);
    }

    #[test]
    fn round_trip_canonical_paths() {
        for s in [
            r"D:\a\b c\d.txt",
            r"\\srv\share\x\y.bin",
            "/mnt/backup/archive/a b/'quoted' \"dq\" $HOME `tick`.txt",
            "gdrive:/Google Photos/2019/IMG_0001.JPG",
            "rel/a.txt",
        ] {
            assert_eq!(parse_path(s.as_bytes()).display(), s, "{s}");
        }
    }

    #[test]
    fn extensions() {
        assert_eq!(extension("Movie (2019).MKV"), "mkv");
        assert_eq!(extension("backup.TAR.GZ"), "tar.gz");
        assert_eq!(extension("archive.gz"), "gz");
        assert_eq!(extension(".bashrc"), "");
        assert_eq!(extension("README"), "");
        assert_eq!(extension("weird.name with space"), "");
        assert_eq!(extension("trailing."), "");
    }
}
