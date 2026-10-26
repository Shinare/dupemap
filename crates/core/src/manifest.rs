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
use std::rc::Rc;

/// The header line written by all export scripts.
pub const HEADER: &str = "hash\tsize\thost\tpath";

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
        }
    }
}

/// A validated row borrowing from the line buffer.
#[derive(Debug)]
pub struct Row<'a> {
    pub digest: Digest,
    /// `sha256:<hex>` was accepted as plain SHA-256.
    pub prefixed_sha256: bool,
    pub size: u64,
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
    Ok(Row { digest, prefixed_sha256, size, host, path })
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
        assert_eq!(r.size, 1048576);
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
    fn header_detection() {
        assert!(is_header(b"hash\tsize\thost\tpath"));
        assert!(is_header(b"Hash\tSize\tHost\tPath  "));
        assert!(!is_header(b"hash,size,host,path"));
    }
}
