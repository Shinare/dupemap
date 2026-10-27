//! Finding files and folders by name, or files by hash, across every source.
//!
//! Names live in SQLite, so a search is one pass over the `file` (and `dir`) table with a `LIKE`
//! filter: well under a second for 5 million rows. Case and accents do not matter: `plaza` finds
//! `Plaża.jpg` and `ŚWIAT` finds `świat`. `LIKE` itself ignores case for ASCII only, so names with
//! other letters are compared through a folding function (lower case, accents removed); names
//! that are plain ASCII skip it. Results are capped at [`MAX_HITS`].

use crate::db::Db;
use crate::error::{err, Result};
use crate::index::{DirIx, FileIx, Index};
use crate::manifest::hex32;
use rusqlite::functions::FunctionFlags;
use serde::{Deserialize, Serialize};

/// More results than this are not listed (the search says so).
pub const MAX_HITS: usize = 10_000;

/// Fewest hex digits a hash search accepts (shorter prefixes match too much to be useful).
pub const MIN_HASH_DIGITS: usize = 6;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SearchMode {
    /// File and folder names: a part of the name, or a pattern with `*` and `?`.
    #[default]
    Name,
    /// A hash or its first digits (SHA-256, or `algo:value` for other hashes).
    Hash,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SearchWhat {
    #[default]
    FilesAndFolders,
    Files,
    Folders,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SearchQuery {
    pub text: String,
    pub mode: SearchMode,
    /// Only this source (database id); `None` = every source.
    pub scope: Option<i64>,
    pub what: SearchWhat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Hit {
    File(FileIx),
    Dir(DirIx),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub hit: Hit,
    pub name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchResult {
    /// Folders first, then files, each in database order (roughly import order).
    pub hits: Vec<SearchHit>,
    /// More matched than [`MAX_HITS`] (or the limit given).
    pub truncated: bool,
}

/// What a name search looks for.
#[derive(Debug, Clone, PartialEq, Eq)]
struct NamePattern {
    /// `LIKE` pattern (escape character `\`), folded (lower case, no accents).
    like: String,
    /// The folded text, for names `LIKE` cannot fold.
    folded: String,
    /// `*` and `?` are wildcards and the whole name has to match.
    glob: bool,
}

impl NamePattern {
    /// `holiday` finds names containing it; `*.mkv` or `IMG_20??.jpg` match whole names.
    fn parse(text: &str) -> Result<NamePattern> {
        let t = text.trim();
        if t.is_empty() {
            return err("Type part of a name, or a pattern such as *.mkv");
        }
        let t = fold_text(t);
        let glob = t.contains('*') || t.contains('?');
        let mut like = String::with_capacity(t.len() + 2);
        if !glob {
            like.push('%');
        }
        for c in t.chars() {
            match c {
                '*' if glob => like.push('%'),
                '?' if glob => like.push('_'),
                '%' | '_' | '\\' => {
                    like.push('\\');
                    like.push(c);
                }
                c => like.push(c),
            }
        }
        if !glob {
            like.push('%');
        }
        Ok(NamePattern { like, folded: t, glob })
    }

    /// The same match in Rust, on a folded name.
    fn matches_folded(&self, name: &str) -> bool {
        if self.glob {
            glob_match(self.folded.as_bytes(), name.as_bytes())
        } else {
            name.contains(self.folded.as_str())
        }
    }

    /// `LIKE` compares plain ASCII names (it folds ASCII case); names with any other character
    /// are folded and compared by `dupemap_match`.
    fn condition(&self, column: &str) -> String {
        format!("({column} LIKE ?1 ESCAPE '\\' OR dupemap_match({column}))")
    }
}

/// Lower case without accents, as names are compared in a search: `Plaża` → `plaza`,
/// `Œuvre` → `oeuvre`, `Straße` → `strasse`. Letters without a plain form stay (lower-cased).
pub fn fold_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    fold_into(s, &mut out);
    out
}

/// [`fold_text`] into a buffer.
fn fold_into(s: &str, out: &mut String) {
    for c in s.chars().flat_map(char::to_lowercase) {
        let plain = match c {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => "a",
            'æ' => "ae",
            'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => "c",
            'ď' | 'đ' | 'ð' => "d",
            'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => "e",
            'ĝ' | 'ğ' | 'ġ' | 'ģ' => "g",
            'ĥ' | 'ħ' => "h",
            'ì' | 'í' | 'î' | 'ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı' => "i",
            'ĵ' => "j",
            'ķ' => "k",
            'ĺ' | 'ļ' | 'ľ' | 'ŀ' | 'ł' => "l",
            'ñ' | 'ń' | 'ņ' | 'ň' | 'ŉ' => "n",
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ŏ' | 'ő' => "o",
            'œ' => "oe",
            'ŕ' | 'ŗ' | 'ř' => "r",
            'ś' | 'ŝ' | 'ş' | 'š' | 'ș' => "s",
            'ß' => "ss",
            'ţ' | 'ť' | 'ŧ' | 'ț' => "t",
            'þ' => "th",
            'ù' | 'ú' | 'û' | 'ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => "u",
            'ŵ' => "w",
            'ý' | 'ÿ' | 'ŷ' => "y",
            'ź' | 'ż' | 'ž' => "z",
            _ => {
                out.push(c);
                continue;
            }
        };
        out.push_str(plain);
    }
}

/// What a hash search looks for.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HashPattern {
    /// Only this algorithm (`sha256`, `md5`...); `None` = any.
    algo: Option<String>,
    /// Lower-case hex digits.
    hex: String,
}

impl HashPattern {
    fn parse(text: &str) -> Result<HashPattern> {
        let t: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        let (algo, hex) = match t.split_once(':') {
            Some((a, h)) => (Some(a.to_ascii_lowercase()), h.to_ascii_lowercase()),
            None => (None, t.to_ascii_lowercase()),
        };
        if hex.is_empty() || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return err("A hash is written with the digits 0-9 and the letters a-f");
        }
        if hex.len() < MIN_HASH_DIGITS {
            return err(format!("Type at least {MIN_HASH_DIGITS} digits of the hash"));
        }
        if hex.len() > 128 {
            return err("That is longer than any hash");
        }
        Ok(HashPattern { algo, hex })
    }

    fn wants_sha256(&self) -> bool {
        self.algo.as_deref().is_none_or(|a| a == "sha256") && self.hex.len() <= 64
    }

    fn wants_other(&self) -> bool {
        self.algo.as_deref() != Some("sha256")
    }

    /// The SHA-256 prefix as bytes (whole bytes only; an odd last digit is checked separately).
    fn sha_prefix(&self) -> Vec<u8> {
        let even = &self.hex[..self.hex.len() / 2 * 2];
        (0..even.len() / 2).map(|i| u8::from_str_radix(&even[2 * i..2 * i + 2], 16).unwrap_or(0)).collect()
    }

    fn matches_sha(&self, digest: &[u8]) -> bool {
        if digest.len() != 32 {
            return false;
        }
        let full = crate::manifest::to_hex(digest);
        full.starts_with(&self.hex)
    }
}

/// Run a search. `keep_going` is asked between rows; returning `false` stops early (a newer
/// search replaced this one) and the partial result is returned.
pub fn search(
    db: &Db,
    ix: &Index,
    q: &SearchQuery,
    limit: usize,
    keep_going: &dyn Fn() -> bool,
) -> Result<SearchResult> {
    let scope = match q.scope {
        Some(id) => match ix.source_ix(id) {
            Some(s) => Some(s),
            // a source that is gone has nothing in it
            None => return Ok(SearchResult::default()),
        },
        None => None,
    };
    let mut out = SearchResult::default();
    match q.mode {
        SearchMode::Name => {
            let pat = NamePattern::parse(&q.text)?;
            register_matcher(db, &pat)?;
            if q.what != SearchWhat::Files {
                let sql = format!("SELECT id, name FROM dir WHERE parent_id IS NOT NULL AND {}", pat.condition("name"));
                let mut st = db.conn().prepare(&sql)?;
                let mut rows = st.query([&pat.like])?;
                while let Some(r) = rows.next()? {
                    if !keep_going() {
                        return Ok(out);
                    }
                    let Some(d) = ix.dir_by_db_id(r.get(0)?) else { continue };
                    if scope.is_some_and(|s| ix.dir_source(d) != s) || ix.dir_parent(d).is_none() {
                        continue;
                    }
                    if out.hits.len() >= limit {
                        out.truncated = true;
                        return Ok(out);
                    }
                    out.hits.push(SearchHit { hit: Hit::Dir(d), name: r.get(1)? });
                }
            }
            if q.what != SearchWhat::Folders {
                let sql = format!("SELECT id, name FROM file WHERE shadowed_by IS NULL AND {}", pat.condition("name"));
                let mut st = db.conn().prepare(&sql)?;
                let mut rows = st.query([&pat.like])?;
                collect_files(&mut rows, ix, scope, limit, keep_going, &mut out, |_| true)?;
            }
        }
        SearchMode::Hash => {
            let pat = HashPattern::parse(&q.text)?;
            if pat.wants_sha256() {
                let prefix = pat.sha_prefix();
                let mut st = db.conn().prepare(
                    "SELECT id, name, digest FROM file
                     WHERE shadowed_by IS NULL AND algo_id = 0 AND substr(digest, 1, ?1) = ?2",
                )?;
                let mut rows = st.query(rusqlite::params![prefix.len() as i64, prefix])?;
                collect_files(&mut rows, ix, scope, limit, keep_going, &mut out, |r| {
                    r.get_ref(2).ok().and_then(|v| v.as_blob().ok()).is_some_and(|d| pat.matches_sha(d))
                })?;
            }
            if pat.wants_other() && !out.truncated {
                let text = pat.hex.as_bytes().to_vec();
                let mut st = db.conn().prepare(
                    "SELECT f.id, f.name FROM file f JOIN algo a ON a.id = f.algo_id
                     WHERE f.shadowed_by IS NULL AND f.algo_id <> 0 AND substr(f.digest, 1, ?1) = ?2
                       AND (?3 IS NULL OR a.name = ?3)",
                )?;
                let mut rows = st.query(rusqlite::params![text.len() as i64, text, pat.algo])?;
                collect_files(&mut rows, ix, scope, limit, keep_going, &mut out, |_| true)?;
            }
        }
    }
    Ok(out)
}

fn collect_files(
    rows: &mut rusqlite::Rows,
    ix: &Index,
    scope: Option<crate::index::SourceIx>,
    limit: usize,
    keep_going: &dyn Fn() -> bool,
    out: &mut SearchResult,
    accept: impl Fn(&rusqlite::Row) -> bool,
) -> Result<()> {
    while let Some(r) = rows.next()? {
        if !keep_going() {
            return Ok(());
        }
        // files in ignored folders are not in the index
        let Some(f) = ix.file_by_db_id(r.get(0)?) else { continue };
        if scope.is_some_and(|s| ix.file_source(f) != s) || !accept(r) {
            continue;
        }
        if out.hits.len() >= limit {
            out.truncated = true;
            return Ok(());
        }
        out.hits.push(SearchHit { hit: Hit::File(f), name: r.get(1)? });
    }
    Ok(())
}

/// `dupemap_match(name)`: the search's match for names with characters other than plain ASCII
/// (folded first). Registered again for every search.
fn register_matcher(db: &Db, pat: &NamePattern) -> Result<()> {
    thread_local! {
        static BUF: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
    }
    let pat = pat.clone();
    db.conn().create_scalar_function("dupemap_match", 1, FunctionFlags::SQLITE_UTF8, move |ctx| {
        let Ok(name) = ctx.get_raw(0).as_str() else { return Ok(false) };
        // plain ASCII names were compared by LIKE already
        if name.is_ascii() {
            return Ok(false);
        }
        Ok(BUF.with(|b| {
            let mut buf = b.borrow_mut();
            buf.clear();
            fold_into(name, &mut buf);
            pat.matches_folded(&buf)
        }))
    })?;
    Ok(())
}

/// `*` any run of characters, `?` one character (UTF-8 aware), the rest literally.
fn glob_match(pat: &[u8], text: &[u8]) -> bool {
    let char_len = |b: u8| match b {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    };
    let (mut p, mut t) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        if p < pat.len() && pat[p] == b'?' {
            p += 1;
            t += char_len(text[t]).min(text.len() - t);
        } else if p < pat.len() && pat[p] == b'*' {
            star = Some((p, t));
            p += 1;
        } else if p < pat.len() && pat[p] == text[t] {
            p += 1;
            t += 1;
        } else if let Some((sp, st)) = star {
            // let the star take one more character
            let next = st + char_len(text[st]).min(text.len() - st);
            star = Some((sp, next));
            p = sp + 1;
            t = next;
        } else {
            return false;
        }
    }
    while p < pat.len() && pat[p] == b'*' {
        p += 1;
    }
    p == pat.len()
}

/// Is this text a full SHA-256 (64 hex digits)? The app switches to a hash search for it.
pub fn looks_like_sha256(text: &str) -> bool {
    let t = text.trim();
    let t = t.strip_prefix("sha256:").unwrap_or(t);
    hex32(t.as_bytes()).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::HEADER;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "abcdef0123456789aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const C: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

    fn fixture() -> (Db, Index) {
        let mut db = Db::open_in_memory().unwrap();
        let pc = format!(
            "{HEADER}\n\
             {A}\t10\tPC D: (Data)\tD:\\Photos\\Wakacje 2019\\IMG_0001.JPG\n\
             {B}\t20\tPC D: (Data)\tD:\\Photos\\Wakacje 2019\\Świnoujście plaża.jpg\n\
             {C}\t30\tPC D: (Data)\tD:\\Docs\\50% done_v2.pdf\n\
             md5:0123456789abcdef0123456789abcdef\t5\tPC D: (Data)\tD:\\Docs\\old.bin\n"
        );
        let nas = format!(
            "{HEADER}\n\
             {A}\t10\tnas\t/mnt/tank/backup/img_0001.jpg\n\
             {B}\t20\tnas\t/mnt/tank/backup/ŚWINOUJŚCIE PLAŻA.JPG\n"
        );
        db.import_reader("pc.tsv", "pc.tsv", 0, None, pc.as_bytes(), &mut |_| true).unwrap();
        db.import_reader("nas.tsv", "nas.tsv", 0, None, nas.as_bytes(), &mut |_| true).unwrap();
        let ix = Index::build(&db).unwrap();
        (db, ix)
    }

    fn names(db: &Db, ix: &Index, q: SearchQuery) -> Vec<String> {
        let r = search(db, ix, &q, MAX_HITS, &|| true).unwrap();
        let mut v: Vec<String> = r
            .hits
            .iter()
            .map(|h| match h.hit {
                Hit::File(f) => ix.file_path(db, f).unwrap(),
                Hit::Dir(d) => format!("[{}]", ix.dir_path(d)),
            })
            .collect();
        v.sort();
        v
    }

    fn name(text: &str) -> SearchQuery {
        SearchQuery { text: text.into(), ..Default::default() }
    }

    #[test]
    fn part_of_a_name_any_case() {
        let (db, ix) = fixture();
        assert_eq!(
            names(&db, &ix, name("img_0001")),
            ["/mnt/tank/backup/img_0001.jpg", r"D:\Photos\Wakacje 2019\IMG_0001.JPG"]
        );
        // folders too
        assert_eq!(names(&db, &ix, name("wakacje")), [r"[D:\Photos\Wakacje 2019]"]);
        assert_eq!(names(&db, &ix, SearchQuery { what: SearchWhat::Files, ..name("wakacje") }), Vec::<String>::new());
        assert_eq!(names(&db, &ix, SearchQuery { what: SearchWhat::Folders, ..name("img") }), Vec::<String>::new());
    }

    #[test]
    fn letters_beyond_ascii_ignore_case_and_accents() {
        let (db, ix) = fixture();
        let got = names(&db, &ix, name("świnoujście"));
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(names(&db, &ix, name("PLAŻA")).len(), 2);
        // typed without the Polish letters
        assert_eq!(names(&db, &ix, name("swinoujscie plaza")).len(), 2);
        assert_eq!(names(&db, &ix, name("*plaza.jpg")).len(), 2);
        assert_eq!(fold_text("Œuvre Straße ĄĆĘŁŃÓŚŹŻ"), "oeuvre strasse acelnoszz");
    }

    #[test]
    fn wildcard_matching() {
        let m = |p: &str, t: &str| glob_match(p.as_bytes(), t.as_bytes());
        assert!(m("*.mkv", "a b.mkv"));
        assert!(!m("*.mkv", "a.mkv.part"));
        assert!(m("img_20??.jpg", "img_2019.jpg"));
        assert!(m("?ółw", "żółw"), "? is one character, not one byte");
        assert!(!m("?", "żż"));
        assert!(m("*a*b*", "xxaxxbxx"));
        assert!(m("*", ""));
        assert!(!m("a", ""));
    }

    #[test]
    fn wildcards_match_whole_names() {
        let (db, ix) = fixture();
        assert_eq!(names(&db, &ix, name("*.pdf")), [r"D:\Docs\50% done_v2.pdf"]);
        assert_eq!(names(&db, &ix, name("img_000?.jpg")).len(), 2);
        assert!(names(&db, &ix, name("img_000?")).is_empty(), "a pattern matches the whole name");
        // % and _ are ordinary characters
        assert_eq!(names(&db, &ix, name("50%")), [r"D:\Docs\50% done_v2.pdf"]);
        assert_eq!(names(&db, &ix, name("e_v2")), [r"D:\Docs\50% done_v2.pdf"]);
        assert!(names(&db, &ix, name("e%v2")).is_empty());
    }

    #[test]
    fn scope_and_limit() {
        let (db, ix) = fixture();
        let nas = ix.sources().iter().find(|s| s.host == "nas").unwrap().db_id;
        assert_eq!(names(&db, &ix, SearchQuery { scope: Some(nas), ..name("img") }), ["/mnt/tank/backup/img_0001.jpg"]);
        assert!(names(&db, &ix, SearchQuery { scope: Some(999), ..name("img") }).is_empty());
        let r = search(&db, &ix, &name("i"), 2, &|| true).unwrap();
        assert_eq!(r.hits.len(), 2);
        assert!(r.truncated);
        assert!(search(&db, &ix, &name("  "), 10, &|| true).is_err());
    }

    #[test]
    fn by_hash_or_its_first_digits() {
        let (db, ix) = fixture();
        let h = |t: &str| names(&db, &ix, SearchQuery { text: t.into(), mode: SearchMode::Hash, ..Default::default() });
        assert_eq!(h(B).len(), 2, "both copies");
        assert_eq!(h("ABCDEF").len(), 2, "a prefix, any case");
        assert_eq!(h("abcdef0").len(), 2, "an odd number of digits");
        assert!(h("abcdef1").is_empty());
        assert_eq!(h(&format!("sha256:{B}")).len(), 2);
        assert_eq!(h("0123456789ab"), [r"D:\Docs\old.bin"], "other hashes by their digits");
        assert_eq!(h("md5:0123456789"), [r"D:\Docs\old.bin"]);
        assert!(h("sha1:0123456789").is_empty());
        assert!(search(
            &db,
            &ix,
            &SearchQuery { text: "abc".into(), mode: SearchMode::Hash, ..Default::default() },
            10,
            &|| true
        )
        .is_err());
        assert!(search(
            &db,
            &ix,
            &SearchQuery { text: "xyz123".into(), mode: SearchMode::Hash, ..Default::default() },
            10,
            &|| true
        )
        .is_err());
        assert!(looks_like_sha256(&format!(" {A} ")));
        assert!(!looks_like_sha256("abc"));
    }

    #[test]
    fn a_superseded_search_stops() {
        let (db, ix) = fixture();
        let r = search(&db, &ix, &name("i"), 10, &|| false).unwrap();
        assert!(r.hits.is_empty());
    }
}
