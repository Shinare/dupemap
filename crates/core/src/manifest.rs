//! Reading manifest files and validating rows.
//!
//! Manifests are read as raw bytes, line by line. UTF-8 (with or without BOM) is expected, but
//! UTF-16 files (e.g. re-saved from Excel as "Unicode text") are decoded transparently.
//! The line-ending style is taken from the first line, so a Linux file name that genuinely ends
//! in `\r` survives in an LF file.

use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::cell::Cell;
use std::io::{self, BufRead, BufReader, Cursor, Read};
use std::path::PathBuf;
use std::rc::Rc;

/// The header line written by all export scripts.
pub const HEADER: &str = "hash\tsize\thost\tpath";

/// The two kinds of hash lists DupeMap reads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ListFormat {
    /// `hash<TAB>size<TAB>host<TAB>path`, as written by the export scripts and the built-in scan.
    #[default]
    Manifest,
    /// A checksum list: `hash  path` per line, as written by `sha256sum` or `rclone hashsum`
    /// (also `hash *path` and BSD-style `SHA256 (path) = hash`). No size and no source: the
    /// source is given at import, sizes are filled in from elsewhere where possible.
    Checksums,
}

/// How a file is read on import.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImportAs {
    /// `hash<TAB>size<TAB>host<TAB>path`, as written by the export scripts and the built-in scan.
    #[default]
    Manifest,
    /// A checksum list (`hash  path` per line, as written by `sha256sum` or `rclone hashsum`).
    Checksums(ChecksumImport),
}

/// What a checksum list does not say for itself.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChecksumImport {
    /// The source every row belongs to: `onedrive:` for an rclone remote, a computer's name...
    pub host: String,
    /// Put in front of relative paths: `onedrive:` (rclone lists paths relative to the remote),
    /// `/mnt/tank/media/`, `D:\Photos\`. Empty, or an absolute path in the list: used as it is.
    pub base: String,
    /// Optional sizes: `size<TAB>path` per line (what `rclone lsf --format sp` writes), with the
    /// paths written as in the checksum list.
    #[serde(default)]
    pub sizes: Option<PathBuf>,
}

impl ChecksumImport {
    /// The full path of a listed path: the base in front of a relative one.
    pub fn full_path<'p>(&self, p: &'p [u8]) -> Cow<'p, [u8]> {
        let p = p.strip_prefix(b"./").unwrap_or(p);
        if self.base.is_empty() || is_absolute(p) {
            return Cow::Borrowed(p);
        }
        let base = self.base.as_bytes();
        // a Windows base gets Windows separators throughout
        let windows = base.len() >= 2 && base[1] == b':' && base[0].is_ascii_alphabetic() || base.starts_with(br"\\");
        let sep = if windows { b'\\' } else { b'/' };
        let mut v = Vec::with_capacity(base.len() + 1 + p.len());
        v.extend_from_slice(base);
        if !matches!(base.last(), Some(b'/' | b'\\' | b':')) {
            v.push(sep);
        }
        if windows {
            v.extend(p.iter().map(|&b| if b == b'/' { b'\\' } else { b }));
        } else {
            v.extend_from_slice(p);
        }
        Cow::Owned(v)
    }
}

/// Does a listed path stand on its own (`/x`, `C:\x`, `\\server\x`, `remote:x`)?
fn is_absolute(p: &[u8]) -> bool {
    p.starts_with(b"/")
        || p.starts_with(br"\\")
        || (p.len() >= 2 && p[1] == b':' && p[0].is_ascii_alphabetic())
        || crate::paths::parse_path(p).kind == crate::paths::PathKind::Cloud
}

/// Cloud services whose rclone remotes are usually named after them.
const CLOUD_WORDS: &[&str] = &[
    "onedrive",
    "gdrive",
    "google",
    "googledrive",
    "drive",
    "dropbox",
    "box",
    "mega",
    "pcloud",
    "icloud",
    "s3",
    "b2",
    "cloud",
    "remote",
    "proton",
    "protondrive",
    "koofr",
    "jottacloud",
    "yandex",
    "sharepoint",
];

/// A first guess at the source of a checksum list, from its file name and first paths:
/// `onedrive-hashsum.txt` with relative paths is the rclone remote `onedrive:`. Paths that are
/// already absolute keep their own anchor (no base).
pub fn guess_checksums(file_name: &str, first_paths: &[&[u8]]) -> ChecksumImport {
    let stem = file_name.rsplit_once('.').map(|(s, _)| s).unwrap_or(file_name);
    let tokens: Vec<&str> = stem.split(['-', '_', '.', ' ']).filter(|t| !t.is_empty()).collect();
    let is_cloud = |t: &str| CLOUD_WORDS.contains(&t.to_ascii_lowercase().as_str());
    // a cloud service named anywhere in the name wins; otherwise the first word that is not a
    // hash-like or common word ("manifest", "sha256", "hashes", numbers...)
    let plain = |t: &str| {
        let l = t.to_ascii_lowercase();
        !t.chars().all(|c| c.is_ascii_hexdigit())
            && !matches!(
                l.as_str(),
                "manifest" | "sha256" | "sha256sum" | "sha256sums" | "hashes" | "hashsum" | "checksums" | "rclone"
            )
    };
    let token: String = tokens
        .iter()
        .find(|t| is_cloud(t))
        .or_else(|| tokens.iter().find(|t| plain(t)))
        .copied()
        .unwrap_or("checksums")
        .to_string();
    let lower = file_name.to_ascii_lowercase();
    let relative = !first_paths.is_empty() && first_paths.iter().all(|p| !is_absolute(p));
    let cloudish = tokens.iter().any(|t| is_cloud(t)) || lower.contains("hashsum") || lower.contains("rclone");
    if relative && cloudish {
        let remote = format!("{}:", token.trim_end_matches(':'));
        ChecksumImport { host: remote.clone(), base: remote, sizes: None }
    } else {
        ChecksumImport { host: token, base: String::new(), sizes: None }
    }
}

/// A size list saved next to a checksum list by the cloud download script:
/// `onedrive-hashsum.txt` → `onedrive-sizes.tsv` (also `<name>-sizes.tsv`, `<name>.sizes.tsv`).
pub fn find_size_list(checksums: &std::path::Path) -> Option<PathBuf> {
    let dir = checksums.parent()?;
    let name = checksums.file_name()?.to_string_lossy().into_owned();
    let stem = name.rsplit_once('.').map(|(s, _)| s.to_string()).unwrap_or(name.clone());
    let mut cands = vec![format!("{stem}-sizes.tsv"), format!("{stem}.sizes.tsv")];
    if let Some(base) = stem.strip_suffix("-hashsum").or_else(|| stem.strip_suffix("_hashsum")) {
        cands.insert(0, format!("{base}-sizes.tsv"));
    }
    cands.into_iter().map(|c| dir.join(c)).find(|p| p.is_file())
}

/// The first paths of a checksum list (for [`guess_checksums`]).
pub fn first_checksum_paths(r: impl Read, n: usize) -> io::Result<Vec<Vec<u8>>> {
    let (mut lines, _) = LineReader::new(r.take(256 * 1024))?;
    let mut out = Vec::new();
    while let Some((_, l)) = lines.next_line()? {
        if let Ok(c) = parse_checksum_line(l) {
            out.push(c.path.into_owned());
            if out.len() >= n {
                break;
            }
        }
    }
    Ok(out)
}

/// A row's content hash.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Digest {
    /// The expected case: 32-byte SHA-256.
    Sha256([u8; 32]),
    /// Anything else. These rows are flagged and only ever match rows of the same algorithm.
    Other { algo: String, value: String },
}

impl Digest {
    pub fn is_sha256(&self) -> bool {
        matches!(self, Digest::Sha256(_))
    }

    pub fn algo_name(&self) -> &str {
        match self {
            Digest::Sha256(_) => "sha256",
            Digest::Other { algo, .. } => algo,
        }
    }

    /// Bytes stored in the database: 32 raw bytes for SHA-256, the digest text otherwise.
    pub fn db_bytes(&self) -> &[u8] {
        match self {
            Digest::Sha256(d) => d,
            Digest::Other { value, .. } => value.as_bytes(),
        }
    }

    /// Text form for display: lowercase hex for SHA-256, `algo:value` otherwise.
    pub fn display(&self) -> String {
        match self {
            Digest::Sha256(d) => to_hex(d),
            Digest::Other { algo, value } => format!("{algo}:{value}"),
        }
    }
}

/// Decode exactly 64 hex characters (either case).
pub fn hex32(s: &[u8]) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, pair) in s.chunks_exact(2).enumerate() {
        out[i] = (hex_val(pair[0])? << 4) | hex_val(pair[1])?;
    }
    Some(out)
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

pub fn to_hex(b: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for &x in b {
        s.push(H[(x >> 4) as usize] as char);
        s.push(H[(x & 15) as usize] as char);
    }
    s
}

/// Why a line could not be imported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Malformed {
    MissingColumns,
    EmptyHash,
    BadSize,
    EmptyHost,
    EmptyPath,
    NoFileName,
    /// A line of a checksum list that is not `hash  path`.
    NotChecksumLine,
    /// rclone wrote `ERROR` or `UNSUPPORTED` instead of a hash (the file could not be read).
    NoHash,
}

impl Malformed {
    pub fn label(self) -> &'static str {
        match self {
            Malformed::MissingColumns => "missing columns",
            Malformed::EmptyHash => "empty hash",
            Malformed::BadSize => "size is not a number",
            Malformed::EmptyHost => "empty host",
            Malformed::EmptyPath => "empty path",
            Malformed::NoFileName => "path has no file name",
            Malformed::NotChecksumLine => "not a checksum line",
            Malformed::NoHash => "no hash (rclone could not read the file)",
        }
    }
}

/// A validated row borrowing from the line buffer.
#[derive(Debug)]
pub struct Row<'a> {
    pub digest: Digest,
    /// `sha256:<hex>` was accepted as plain SHA-256.
    pub prefixed_sha256: bool,
    /// `None` when the list has no sizes (checksum lists).
    pub size: Option<u64>,
    pub host: Cow<'a, str>,
    pub path: &'a [u8],
}

/// Parse and validate one data line (without its line terminator).
pub fn parse_row(line: &[u8]) -> Result<Row<'_>, Malformed> {
    let mut it = line.splitn(4, |&b| b == b'\t');
    let (Some(h), Some(s), Some(host), Some(path)) = (it.next(), it.next(), it.next(), it.next()) else {
        return Err(Malformed::MissingColumns);
    };
    let h = h.trim_ascii();
    if h.is_empty() {
        return Err(Malformed::EmptyHash);
    }
    let (digest, prefixed_sha256) = parse_digest(h)?;
    let s = s.trim_ascii();
    if s.is_empty() || !s.iter().all(u8::is_ascii_digit) || s.len() > 19 {
        return Err(Malformed::BadSize);
    }
    let size: u64 = std::str::from_utf8(s).ok().and_then(|v| v.parse().ok()).ok_or(Malformed::BadSize)?;
    if size > i64::MAX as u64 {
        return Err(Malformed::BadSize);
    }
    if host.is_empty() {
        return Err(Malformed::EmptyHost);
    }
    if path.is_empty() {
        return Err(Malformed::EmptyPath);
    }
    let host = String::from_utf8_lossy(host);
    Ok(Row { digest, prefixed_sha256, size: Some(size), host, path })
}

/// One line of a checksum list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChecksumLine<'a> {
    pub digest: Digest,
    /// The path as written (unescaped when the line used `sha256sum`'s backslash escapes).
    pub path: Cow<'a, [u8]>,
}

/// Parse one line of a checksum list:
/// * `hash  path` (`sha256sum`, `rclone hashsum`), `hash *path` (binary mode), `hash<TAB>path`
/// * `\hash  pa\\th` (GNU escaping of names with a backslash or a line break)
/// * `SHA256 (path) = hash` (BSD / `--tag` style)
///
/// The hash length decides the algorithm: 64 hex digits are SHA-256, others are flagged (32 =
/// md5, 40 = sha1, ...). Returns the reason when the line is not usable.
pub fn parse_checksum_line(line: &[u8]) -> Result<ChecksumLine<'_>, Malformed> {
    let trimmed = line.trim_ascii_start();
    if trimmed.starts_with(b"ERROR") || trimmed.starts_with(b"UNSUPPORTED") {
        return Err(Malformed::NoHash);
    }
    // BSD style: ALGO (path) = hash
    if let Some(open) = line.windows(2).position(|w| w == b" (") {
        let tag = &line[..open];
        if !tag.is_empty() && tag.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'-') {
            if let Some(close) = line.windows(4).rposition(|w| w == b") = ") {
                if close > open + 2 {
                    let hex = line[close + 4..].trim_ascii();
                    if !hex.is_empty() && hex.iter().all(u8::is_ascii_hexdigit) {
                        let path = &line[open + 2..close];
                        let algo = String::from_utf8_lossy(tag).to_ascii_lowercase().replace('-', "");
                        return Ok(ChecksumLine {
                            digest: checksum_digest(hex, Some(&algo)),
                            path: Cow::Borrowed(path),
                        });
                    }
                }
            }
        }
    }
    let (escaped, body) = match line.strip_prefix(b"\\") {
        Some(rest) => (true, rest),
        None => (false, line),
    };
    let hex_len = body.iter().take_while(|b| b.is_ascii_hexdigit()).count();
    if !(16..=128).contains(&hex_len) {
        return Err(Malformed::NotChecksumLine);
    }
    let rest = &body[hex_len..];
    let path = if let Some(p) = rest.strip_prefix(b"  ").or_else(|| rest.strip_prefix(b" *")) {
        p
    } else if let Some(p) = rest.strip_prefix(b"\t") {
        p
    } else {
        return Err(Malformed::NotChecksumLine);
    };
    if path.is_empty() {
        return Err(Malformed::EmptyPath);
    }
    let path = if escaped { Cow::Owned(unescape_name(path)) } else { Cow::Borrowed(path) };
    Ok(ChecksumLine { digest: checksum_digest(&body[..hex_len], None), path })
}

/// `sha256sum` writes `\\` for a backslash and `\n` for a line break in an escaped name.
fn unescape_name(p: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(p.len());
    let mut i = 0;
    while i < p.len() {
        if p[i] == b'\\' && i + 1 < p.len() {
            match p[i + 1] {
                b'\\' => out.push(b'\\'),
                b'n' => out.push(b'\n'),
                b'r' => out.push(b'\r'),
                other => {
                    out.push(b'\\');
                    out.push(other);
                }
            }
            i += 2;
        } else {
            out.push(p[i]);
            i += 1;
        }
    }
    out
}

/// A checksum list's hash: SHA-256 when it is one, flagged otherwise (named by the BSD tag or
/// guessed from the length).
fn checksum_digest(hex: &[u8], tag: Option<&str>) -> Digest {
    if tag.is_none_or(|t| t == "sha256") {
        if let Some(d) = hex32(hex) {
            return Digest::Sha256(d);
        }
    }
    let algo = match tag {
        Some(t) if !t.is_empty() && t.len() <= 32 => t.to_string(),
        _ => match hex.len() {
            32 => "md5".to_string(),
            40 => "sha1".to_string(),
            56 => "sha224".to_string(),
            96 => "sha384".to_string(),
            128 => "sha512".to_string(),
            n => format!("hex{n}?"),
        },
    };
    Digest::Other { algo, value: String::from_utf8_lossy(hex).to_ascii_lowercase() }
}

/// Decide from the first lines of a file whether it is a manifest or a checksum list. Anything
/// with tab-separated columns (or the manifest header) is a manifest, so a damaged manifest
/// still gets its import report instead of being read as something else.
pub fn detect_format<'l>(lines: impl IntoIterator<Item = &'l [u8]>) -> ListFormat {
    let mut checksum = 0;
    for l in lines.into_iter().filter(|l| !l.trim_ascii().is_empty()).take(20) {
        if is_header(l) || l.iter().filter(|&&b| b == b'\t').count() >= 3 {
            return ListFormat::Manifest;
        }
        if parse_checksum_line(l).is_ok() {
            checksum += 1;
        }
    }
    if checksum > 0 {
        ListFormat::Checksums
    } else {
        ListFormat::Manifest
    }
}

/// Read the first lines of a file (any supported encoding) and decide its format.
pub fn sniff_format(r: impl Read) -> io::Result<ListFormat> {
    let (mut lines, _) = LineReader::new(r.take(64 * 1024))?;
    let mut first = Vec::new();
    while let Some((_, l)) = lines.next_line()? {
        first.push(l.to_vec());
        if first.len() >= 20 {
            break;
        }
    }
    Ok(detect_format(first.iter().map(|v| v.as_slice())))
}

fn parse_digest(h: &[u8]) -> Result<(Digest, bool), Malformed> {
    if let Some(d) = hex32(h) {
        return Ok((Digest::Sha256(d), false));
    }
    let text = String::from_utf8_lossy(h);
    if let Some((algo, val)) = text.split_once(':') {
        let algo = algo.trim().to_ascii_lowercase();
        let val = val.trim();
        if val.is_empty() {
            return Err(Malformed::EmptyHash);
        }
        if algo == "sha256" {
            if let Some(d) = hex32(val.as_bytes()) {
                return Ok((Digest::Sha256(d), true));
            }
        }
        let algo = if !algo.is_empty()
            && algo.len() <= 32
            && algo.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            algo
        } else {
            "unknown".to_string()
        };
        let value = if val.bytes().all(|b| b.is_ascii_hexdigit()) { val.to_ascii_lowercase() } else { val.to_string() };
        return Ok((Digest::Other { algo, value }, false));
    }
    let all_hex = h.iter().all(u8::is_ascii_hexdigit);
    let algo = match (all_hex, h.len()) {
        (true, 32) => "md5?".to_string(),
        (true, 40) => "sha1?".to_string(),
        (true, n) => format!("hex{n}?"),
        _ => "unknown".to_string(),
    };
    let value = if all_hex { text.to_ascii_lowercase() } else { text.into_owned() };
    Ok((Digest::Other { algo, value }, false))
}

/// Text encoding detected from the byte-order mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Encoding {
    Utf8,
    Utf8Bom,
    Utf16Le,
    Utf16Be,
}

impl Encoding {
    pub fn label(self) -> &'static str {
        match self {
            Encoding::Utf8 => "UTF-8",
            Encoding::Utf8Bom => "UTF-8 with BOM",
            Encoding::Utf16Le => "UTF-16 LE",
            Encoding::Utf16Be => "UTF-16 BE",
        }
    }
}

/// Counts bytes pulled from the underlying reader (for progress reporting).
pub struct Counting<R> {
    inner: R,
    count: Rc<Cell<u64>>,
}

impl<R> Counting<R> {
    pub fn new(inner: R) -> (Self, Rc<Cell<u64>>) {
        let count = Rc::new(Cell::new(0));
        (Counting { inner, count: count.clone() }, count)
    }
}

impl<R: Read> Read for Counting<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.count.set(self.count.get() + n as u64);
        Ok(n)
    }
}

/// Reads a manifest line by line as raw bytes.
pub struct LineReader<'r> {
    inner: BufReader<Box<dyn Read + 'r>>,
    buf: Vec<u8>,
    line_no: u64,
    crlf: Option<bool>,
}

impl<'r> LineReader<'r> {
    pub fn new<R: Read + 'r>(mut r: R) -> io::Result<(Self, Encoding)> {
        let mut head = [0u8; 3];
        let mut n = 0;
        while n < 3 {
            let k = r.read(&mut head[n..])?;
            if k == 0 {
                break;
            }
            n += k;
        }
        let head = &head[..n];
        let (enc, rest): (Encoding, Vec<u8>) = if head.starts_with(&[0xEF, 0xBB, 0xBF]) {
            (Encoding::Utf8Bom, Vec::new())
        } else if head.starts_with(&[0xFF, 0xFE]) {
            (Encoding::Utf16Le, head[2..].to_vec())
        } else if head.starts_with(&[0xFE, 0xFF]) {
            (Encoding::Utf16Be, head[2..].to_vec())
        } else {
            (Encoding::Utf8, head.to_vec())
        };
        let chained = Cursor::new(rest).chain(r);
        let boxed: Box<dyn Read + 'r> = match enc {
            Encoding::Utf16Le => Box::new(Utf16Reader::new(chained, false)),
            Encoding::Utf16Be => Box::new(Utf16Reader::new(chained, true)),
            _ => Box::new(chained),
        };
        Ok((
            LineReader {
                inner: BufReader::with_capacity(1 << 20, boxed),
                buf: Vec::with_capacity(512),
                line_no: 0,
                crlf: None,
            },
            enc,
        ))
    }

    /// The next line (1-based number, bytes without terminator), or `None` at end of file.
    pub fn next_line(&mut self) -> io::Result<Option<(u64, &[u8])>> {
        self.buf.clear();
        let n = self.inner.read_until(b'\n', &mut self.buf)?;
        if n == 0 {
            return Ok(None);
        }
        self.line_no += 1;
        let mut end = self.buf.len();
        let had_nl = self.buf[end - 1] == b'\n';
        if had_nl {
            end -= 1;
        }
        let has_cr = end > 0 && self.buf[end - 1] == b'\r';
        if self.crlf.is_none() && had_nl {
            self.crlf = Some(has_cr);
        }
        if has_cr && self.crlf != Some(false) {
            end -= 1;
        }
        Ok(Some((self.line_no, &self.buf[..end])))
    }

    /// 1-based number of the line last returned.
    pub fn line_no(&self) -> u64 {
        self.line_no
    }

    /// `Some(true)` for CRLF files, `Some(false)` for LF, `None` before the first full line.
    pub fn crlf(&self) -> Option<bool> {
        self.crlf
    }
}

/// Streaming UTF-16 → UTF-8 decoder. Unpaired surrogates become U+FFFD.
struct Utf16Reader<R> {
    inner: R,
    big_endian: bool,
    raw: Vec<u8>,
    odd: Option<u8>,
    high: Option<u16>,
    out: Vec<u8>,
    pos: usize,
    eof: bool,
}

impl<R: Read> Utf16Reader<R> {
    fn new(inner: R, big_endian: bool) -> Self {
        Utf16Reader {
            inner,
            big_endian,
            raw: vec![0; 64 * 1024],
            odd: None,
            high: None,
            out: Vec::new(),
            pos: 0,
            eof: false,
        }
    }

    fn push_char(&mut self, c: char) {
        let mut b = [0u8; 4];
        self.out.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
    }

    fn unit(&mut self, u: u16) {
        if let Some(hi) = self.high.take() {
            if (0xDC00..=0xDFFF).contains(&u) {
                let c = 0x10000 + (((hi as u32) - 0xD800) << 10) + ((u as u32) - 0xDC00);
                self.push_char(char::from_u32(c).unwrap_or('\u{FFFD}'));
                return;
            }
            self.push_char('\u{FFFD}');
        }
        if (0xD800..=0xDBFF).contains(&u) {
            self.high = Some(u);
        } else if (0xDC00..=0xDFFF).contains(&u) {
            self.push_char('\u{FFFD}');
        } else {
            self.push_char(char::from_u32(u as u32).unwrap_or('\u{FFFD}'));
        }
    }

    fn fill(&mut self) -> io::Result<()> {
        self.out.clear();
        self.pos = 0;
        while self.out.is_empty() && !self.eof {
            let n = self.inner.read(&mut self.raw)?;
            if n == 0 {
                self.eof = true;
                if self.high.take().is_some() {
                    self.push_char('\u{FFFD}');
                }
                if self.odd.take().is_some() {
                    self.push_char('\u{FFFD}');
                }
                break;
            }
            let mut bytes: Vec<u8> = Vec::with_capacity(n + 1);
            if let Some(b) = self.odd.take() {
                bytes.push(b);
            }
            bytes.extend_from_slice(&self.raw[..n]);
            if bytes.len() % 2 == 1 {
                self.odd = bytes.pop();
            }
            for pair in bytes.chunks_exact(2) {
                let u = if self.big_endian {
                    u16::from_be_bytes([pair[0], pair[1]])
                } else {
                    u16::from_le_bytes([pair[0], pair[1]])
                };
                self.unit(u);
            }
        }
        Ok(())
    }
}

impl<R: Read> Read for Utf16Reader<R> {
    fn read(&mut self, dst: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.out.len() {
            self.fill()?;
            if self.out.is_empty() {
                return Ok(0);
            }
        }
        let n = dst.len().min(self.out.len() - self.pos);
        dst[..n].copy_from_slice(&self.out[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// True when a line is the standard header (case-insensitive, trailing whitespace ignored).
pub fn is_header(line: &[u8]) -> bool {
    line.trim_ascii_end().eq_ignore_ascii_case(HEADER.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: &str = "3fa9c2e1d4b5a6978877665544332211ffeeddccbbaa99887766554433221100";

    fn lines(data: &[u8]) -> (Vec<Vec<u8>>, Encoding, Option<bool>) {
        let (mut r, enc) = LineReader::new(Cursor::new(data.to_vec())).unwrap();
        let mut out = Vec::new();
        while let Some((_, l)) = r.next_line().unwrap() {
            out.push(l.to_vec());
        }
        let crlf = r.crlf();
        (out, enc, crlf)
    }

    #[test]
    fn valid_row() {
        let line = format!("{H}\t1048576\tPC1 D: (Media)\tD:\\Videos\\a b.mkv");
        let r = parse_row(line.as_bytes()).unwrap();
        assert!(r.digest.is_sha256());
        assert_eq!(r.size, Some(1048576));
        assert_eq!(r.host, "PC1 D: (Media)");
        assert_eq!(r.path, br"D:\Videos\a b.mkv");
        assert_eq!(r.digest.display(), H);
    }

    #[test]
    fn uppercase_and_prefixed_sha256_are_sha256() {
        let up = H.to_ascii_uppercase();
        let l1 = format!("{up}\t5\th\t/p/f");
        let r = parse_row(l1.as_bytes()).unwrap();
        assert_eq!(r.digest.display(), H);
        let l2 = format!("sha256:{H}\t5\th\t/p/f");
        let p = parse_row(l2.as_bytes()).unwrap();
        assert!(p.digest.is_sha256() && p.prefixed_sha256);
    }

    #[test]
    fn flagged_algorithms() {
        let q = parse_row(b"quickxor:AbCdEfGhIjKlMnOpQrStUvWxYz0=\t5\tonedrive:\tonedrive:/a.zip").unwrap();
        assert_eq!(q.digest, Digest::Other { algo: "quickxor".into(), value: "AbCdEfGhIjKlMnOpQrStUvWxYz0=".into() });
        let m = parse_row(b"MD5:0123456789ABCDEF0123456789ABCDEF\t5\tgdrive:\tgdrive:/x").unwrap();
        assert_eq!(m.digest, Digest::Other { algo: "md5".into(), value: "0123456789abcdef0123456789abcdef".into() });
        let s1 = parse_row(b"0123456789abcdef0123456789abcdef01234567\t5\th\t/x").unwrap();
        assert_eq!(s1.digest.algo_name(), "sha1?");
        let junk = parse_row(b"not-a-hash\t5\th\t/x").unwrap();
        assert_eq!(junk.digest.algo_name(), "unknown");
        let short = parse_row(b"abc123\t5\th\t/x").unwrap();
        assert_eq!(short.digest.algo_name(), "hex6?");
    }

    #[test]
    fn malformed_rows() {
        assert_eq!(parse_row(b"abc\t12\thost").unwrap_err(), Malformed::MissingColumns);
        assert_eq!(parse_row(b"").unwrap_err(), Malformed::MissingColumns);
        assert_eq!(parse_row("\t12\th\t/p".to_string().as_bytes()).unwrap_err(), Malformed::EmptyHash);
        assert_eq!(parse_row(b"md5:\t12\th\t/p").unwrap_err(), Malformed::EmptyHash);
        assert_eq!(parse_row(format!("{H}\t12a\th\t/p").as_bytes()).unwrap_err(), Malformed::BadSize);
        assert_eq!(parse_row(format!("{H}\t-5\th\t/p").as_bytes()).unwrap_err(), Malformed::BadSize);
        assert_eq!(parse_row(format!("{H}\t\th\t/p").as_bytes()).unwrap_err(), Malformed::BadSize);
        assert_eq!(parse_row(format!("{H}\t99999999999999999999\th\t/p").as_bytes()).unwrap_err(), Malformed::BadSize);
        assert_eq!(parse_row(format!("{H}\t1\t\t/p").as_bytes()).unwrap_err(), Malformed::EmptyHost);
        assert_eq!(parse_row(format!("{H}\t1\th\t").as_bytes()).unwrap_err(), Malformed::EmptyPath);
    }

    #[test]
    fn tabs_inside_the_path_stay_in_the_path() {
        let l = format!("{H}\t1\th\t/a/b\tc.txt");
        let r = parse_row(l.as_bytes()).unwrap();
        assert_eq!(r.path, b"/a/b\tc.txt");
    }

    #[test]
    fn crlf_and_bom() {
        let data = format!("\u{FEFF}{HEADER}\r\n{H}\t1\th\tD:\\a.txt\r\n");
        let (ls, enc, crlf) = lines(data.as_bytes());
        assert_eq!(enc, Encoding::Utf8Bom);
        assert_eq!(crlf, Some(true));
        assert!(is_header(&ls[0]));
        assert_eq!(ls[1], format!("{H}\t1\th\tD:\\a.txt").as_bytes());
    }

    #[test]
    fn lf_file_keeps_a_trailing_carriage_return_in_a_name() {
        let data = format!("{HEADER}\n{H}\t1\th\t/odd\r\n");
        let (ls, enc, crlf) = lines(data.as_bytes());
        assert_eq!(enc, Encoding::Utf8);
        assert_eq!(crlf, Some(false));
        assert_eq!(ls[1], format!("{H}\t1\th\t/odd\r").as_bytes());
    }

    #[test]
    fn last_line_without_newline() {
        let data = format!("{HEADER}\r\n{H}\t1\th\t/x\r\n{H}\t2\th\t/y");
        let (ls, _, _) = lines(data.as_bytes());
        assert_eq!(ls.len(), 3);
        assert_eq!(ls[2], format!("{H}\t2\th\t/y").as_bytes());
    }

    #[test]
    fn utf16_le_and_be() {
        let text = format!("{HEADER}\r\n{H}\t7\tPC1 C: (Łódź)\tC:\\Zdjęcia\\😀.jpg\r\n");
        let mut le = vec![0xFF, 0xFE];
        let mut be = vec![0xFE, 0xFF];
        for u in text.encode_utf16() {
            le.extend_from_slice(&u.to_le_bytes());
            be.extend_from_slice(&u.to_be_bytes());
        }
        for (bytes, want) in [(le, Encoding::Utf16Le), (be, Encoding::Utf16Be)] {
            let (ls, enc, crlf) = lines(&bytes);
            assert_eq!(enc, want);
            assert_eq!(crlf, Some(true));
            let r = parse_row(&ls[1]).unwrap();
            assert_eq!(r.host, "PC1 C: (Łódź)");
            assert_eq!(r.path, "C:\\Zdjęcia\\😀.jpg".as_bytes());
        }
    }

    #[test]
    fn utf16_with_tiny_reads_and_broken_surrogate() {
        struct OneByte(Cursor<Vec<u8>>);
        impl Read for OneByte {
            fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
                let n = b.len().min(1);
                self.0.read(&mut b[..n])
            }
        }
        let mut le = vec![0xFF, 0xFE];
        for u in "a😀b".encode_utf16() {
            le.extend_from_slice(&u.to_le_bytes());
        }
        le.extend_from_slice(&0xD800u16.to_le_bytes()); // lone high surrogate
        le.extend_from_slice(&(b'c' as u16).to_le_bytes());
        let (mut r, _) = LineReader::new(OneByte(Cursor::new(le))).unwrap();
        let l = r.next_line().unwrap().unwrap().1.to_vec();
        assert_eq!(String::from_utf8(l).unwrap(), "a😀b\u{FFFD}c");
    }

    #[test]
    fn checksum_lines() {
        let gnu = format!("{H}  Folder/a b.mp4");
        let c = parse_checksum_line(gnu.as_bytes()).unwrap();
        assert_eq!(c.digest.display(), H);
        assert_eq!(c.path.as_ref(), b"Folder/a b.mp4");
        // GNU: hash, a space, the mode character (space = text, * = binary), the name; so after
        // two spaces a leading * belongs to the name
        let bin = format!("{}  *x.bin", H.to_ascii_uppercase());
        assert_eq!(parse_checksum_line(bin.as_bytes()).unwrap().path.as_ref(), b"*x.bin");
        let bin = format!("{H} *x.bin");
        assert_eq!(parse_checksum_line(bin.as_bytes()).unwrap().path.as_ref(), b"x.bin");
        let tab = format!("{H}\t  spaced.txt");
        assert_eq!(parse_checksum_line(tab.as_bytes()).unwrap().path.as_ref(), b"  spaced.txt");
        // GNU escaping
        let esc = format!("\\{H}  a\\\\b\\nc");
        assert_eq!(parse_checksum_line(esc.as_bytes()).unwrap().path.as_ref(), b"a\\b\nc");
        // BSD tag style, path with parentheses and " = " inside
        let bsd = format!("SHA256 (dir/x (1) = y.txt) = {H}");
        let b = parse_checksum_line(bsd.as_bytes()).unwrap();
        assert_eq!(b.path.as_ref(), b"dir/x (1) = y.txt");
        assert!(b.digest.is_sha256());
        let md5 = parse_checksum_line(b"MD5 (a.txt) = 0123456789ABCDEF0123456789ABCDEF").unwrap();
        assert_eq!(md5.digest.algo_name(), "md5");
        // other lengths are flagged
        let sha1 = parse_checksum_line(b"0123456789abcdef0123456789abcdef01234567  f").unwrap();
        assert_eq!(sha1.digest.algo_name(), "sha1");
        assert_eq!(parse_checksum_line(b"ERROR                     some/file").unwrap_err(), Malformed::NoHash);
        assert_eq!(parse_checksum_line(b"hello world").unwrap_err(), Malformed::NotChecksumLine);
        assert_eq!(parse_checksum_line(format!("{H} x").as_bytes()).unwrap_err(), Malformed::NotChecksumLine);
        assert_eq!(parse_checksum_line(format!("{H}  ").as_bytes()).unwrap_err(), Malformed::EmptyPath);
    }

    #[test]
    fn guessing_the_source_of_a_checksum_list() {
        let rel: Vec<&[u8]> = vec![b"Docs/a.pdf", b"b.mp4"];
        let g = guess_checksums("onedrive-hashsum.txt", &rel);
        assert_eq!((g.host.as_str(), g.base.as_str()), ("onedrive:", "onedrive:"));
        let g = guess_checksums("onedrive-sha256-example.txt", &rel);
        assert_eq!(g.host, "onedrive:");
        let g = guess_checksums("gdrive_hashes.txt", &rel);
        assert_eq!(g.host, "gdrive:");
        // a name that is not a cloud service: a source of that name, paths as listed
        let g = guess_checksums("nas-sha256.txt", &rel);
        assert_eq!((g.host.as_str(), g.base.as_str()), ("nas", ""));
        // absolute paths keep their own anchor
        let abs: Vec<&[u8]> = vec![b"/mnt/tank/a", b"/mnt/tank/b"];
        let g = guess_checksums("onedrive.txt", &abs);
        assert_eq!((g.host.as_str(), g.base.as_str()), ("onedrive", ""));
        assert_eq!(guess_checksums(".txt", &rel).host, "checksums");
        // an upload prefix or a hash-like word in front does not hide the service
        assert_eq!(guess_checksums("e8309321-onedrive-sha256-example.txt", &rel).host, "onedrive:");
        assert_eq!(guess_checksums("sha256-photos.txt", &rel).host, "photos");
    }

    #[test]
    fn size_lists_next_to_a_checksum_list_are_found() {
        let dir = std::env::temp_dir().join(format!("dupemap-sizelist-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("onedrive-hashsum.txt"), "").unwrap();
        assert_eq!(find_size_list(&dir.join("onedrive-hashsum.txt")), None);
        std::fs::write(dir.join("onedrive-sizes.tsv"), "").unwrap();
        assert_eq!(find_size_list(&dir.join("onedrive-hashsum.txt")), Some(dir.join("onedrive-sizes.tsv")));
        std::fs::write(dir.join("photos.txt"), "").unwrap();
        std::fs::write(dir.join("photos.sizes.tsv"), "").unwrap();
        assert_eq!(find_size_list(&dir.join("photos.txt")), Some(dir.join("photos.sizes.tsv")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn format_detection() {
        let list = format!("{H}  a.txt\n{H}  b/c.txt\n");
        assert_eq!(sniff_format(Cursor::new(list.into_bytes())).unwrap(), ListFormat::Checksums);
        let man = format!("{HEADER}\n{H}\t1\th\t/x\n");
        assert_eq!(sniff_format(Cursor::new(man.into_bytes())).unwrap(), ListFormat::Manifest);
        // a manifest without its header is still a manifest
        let headless = format!("{H}\t1\th\t/x\n");
        assert_eq!(sniff_format(Cursor::new(headless.into_bytes())).unwrap(), ListFormat::Manifest);
        assert_eq!(sniff_format(Cursor::new(b"random text\n".to_vec())).unwrap(), ListFormat::Manifest);
        // UTF-16 checksum list (re-saved by an editor)
        let mut le = vec![0xFF, 0xFE];
        for u in format!("{H}  x.txt\r\n").encode_utf16() {
            le.extend_from_slice(&u.to_le_bytes());
        }
        assert_eq!(sniff_format(Cursor::new(le)).unwrap(), ListFormat::Checksums);
    }

    #[test]
    fn header_detection() {
        assert!(is_header(b"hash\tsize\thost\tpath"));
        assert!(is_header(b"Hash\tSize\tHost\tPath  "));
        assert!(!is_header(b"hash,size,host,path"));
    }
}
