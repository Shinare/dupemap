//! Marks for deletion.
//!
//! A mark is a rule on a file or a folder, saved by source and path so it survives re-imports and
//! rescans:
//!
//! * **Delete**: the file, or every file in the folder.
//! * **Delete duplicates** (folders): the folder's files that have a copy outside it.
//! * **Keep**: an exception inside a marked folder.
//!
//! The most specific rule decides for each file: a rule on the file beats one on its folder, a
//! deeper folder beats a shallower one. [`MarkState::compute`] works out what the rules mark in
//! an index: which files, which of them would lose their *last copy* (every copy of the content
//! is marked), and totals per folder, source and rule. DupeMap never deletes anything itself:
//! marks become scripts (see [`crate::export`]).

use crate::db::Db;
use crate::error::Result;
use crate::index::{ContentIx, DirIx, FileIx, Index, SourceIx, NONE};
use crate::paths::{fold_case, parse_path};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum MarkAction {
    /// Delete the file, or every file in the folder.
    Delete,
    /// Folder: delete its files that have a copy outside it. File: delete it if it has another
    /// copy anywhere.
    DeleteDuplicates,
    /// Keep: an exception inside a marked folder.
    Keep,
}

impl MarkAction {
    pub fn code(self) -> i64 {
        match self {
            MarkAction::Delete => 1,
            MarkAction::DeleteDuplicates => 2,
            MarkAction::Keep => 3,
        }
    }

    pub fn from_code(c: i64) -> Option<MarkAction> {
        match c {
            1 => Some(MarkAction::Delete),
            2 => Some(MarkAction::DeleteDuplicates),
            3 => Some(MarkAction::Keep),
            _ => None,
        }
    }

    /// What the rule does, in words.
    pub fn label(self, is_dir: bool) -> &'static str {
        match (self, is_dir) {
            (MarkAction::Delete, true) => "Delete the folder",
            (MarkAction::Delete, false) => "Delete",
            (MarkAction::DeleteDuplicates, true) => "Delete its files that have a copy outside it",
            (MarkAction::DeleteDuplicates, false) => "Delete if it has another copy",
            (MarkAction::Keep, _) => "Keep",
        }
    }
}

/// A rule on a file or folder.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MarkRule {
    /// The source, as the manifests name it (`PC1 D: (Data)`, `truenas`, `gdrive:`).
    pub host: String,
    /// Full path as DupeMap shows it (`D:\Backup\a.jpg`, `/mnt/tank/x`, `gdrive:/a`); empty for
    /// the whole source.
    pub path: String,
    pub is_dir: bool,
    pub action: MarkAction,
}

/// Where a rule is: two rules on the same place replace each other. Windows paths ignore case.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PlaceKey {
    pub host: String,
    pub path: String,
    pub is_dir: bool,
}

impl MarkRule {
    /// A rule on a folder (the source's root folder stands for the whole source).
    pub fn for_dir(ix: &Index, d: DirIx, action: MarkAction) -> MarkRule {
        let host = ix.sources()[ix.dir_source(d) as usize].host.clone();
        let path = if ix.dir_parent(d).is_none() { String::new() } else { ix.dir_path(d) };
        MarkRule { host, path, is_dir: true, action }
    }

    /// A rule on a file, given its folder and name.
    pub fn for_file(ix: &Index, dir: DirIx, name: &str, action: MarkAction) -> MarkRule {
        let host = ix.sources()[ix.dir_source(dir) as usize].host.clone();
        MarkRule { host, path: ix.path_with_name(dir, name), is_dir: false, action }
    }

    pub fn place_key(&self) -> PlaceKey {
        let path = if parse_path(self.path.as_bytes()).kind.case_insensitive() {
            fold_case(&self.path)
        } else {
            self.path.clone()
        };
        PlaceKey { host: self.host.clone(), path, is_dir: self.is_dir }
    }

    /// The path, or the source for a rule on the whole source.
    pub fn display(&self) -> &str {
        if self.path.is_empty() {
            &self.host
        } else {
            &self.path
        }
    }
}

/// A folder or file of an index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Place {
    Dir(DirIx),
    File(FileIx),
}

fn names_equal(a: &str, b: &str, fold: bool) -> bool {
    a == b || (fold && fold_case(a) == fold_case(b))
}

fn child_named(ix: &Index, d: DirIx, name: &str, fold: bool) -> Option<DirIx> {
    ix.dir_children(d).iter().copied().find(|&c| names_equal(ix.dir_name(c), name, fold))
}

/// Find where each rule is in this index (`None`: the path is not there, e.g. removed or
/// ignored). File names are read from the database, one folder at a time.
pub fn resolve(ix: &Index, db: &Db, rules: &[MarkRule]) -> Result<Vec<Option<Place>>> {
    let mut names: HashMap<DirIx, Vec<(i64, String)>> = HashMap::new();
    let mut hosts: HashMap<&str, Option<SourceIx>> = HashMap::new();
    let mut out = Vec::with_capacity(rules.len());
    for r in rules {
        let src = *hosts
            .entry(r.host.as_str())
            .or_insert_with(|| ix.sources().iter().position(|s| s.host == r.host).map(|p| p as SourceIx));
        let Some(s) = src else {
            out.push(None);
            continue;
        };
        let root = ix.sources()[s as usize].root;
        if r.path.is_empty() {
            out.push(r.is_dir.then_some(Place::Dir(root)));
            continue;
        }
        let p = parse_path(r.path.as_bytes());
        let fold = p.kind.case_insensitive();
        let Some(anchor) = child_named(ix, root, &p.anchor, fold) else {
            out.push(None);
            continue;
        };
        let (folders, file) = if r.is_dir {
            (&p.parts[..], None)
        } else {
            match p.parts.split_last() {
                Some((last, rest)) => (rest, Some(last.text.as_ref())),
                None => {
                    out.push(None);
                    continue;
                }
            }
        };
        let mut cur = Some(anchor);
        for c in folders {
            cur = cur.and_then(|d| child_named(ix, d, &c.text, fold));
        }
        let Some(d) = cur else {
            out.push(None);
            continue;
        };
        let place = match file {
            None => Some(Place::Dir(d)),
            Some(name) => {
                let list = match names.get(&d) {
                    Some(l) => l,
                    None => {
                        let l = db.visible_file_names(ix.dir_db_id(d))?;
                        names.entry(d).or_insert(l)
                    }
                };
                list.iter()
                    .find(|(_, n)| names_equal(n, name, fold))
                    .and_then(|&(id, _)| ix.dir_files(d).find(|&f| ix.file_db_id(f) == id))
                    .map(Place::File)
            }
        };
        out.push(place);
    }
    Ok(out)
}

/// Files and bytes, and how many of them would lose their last copy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MarkTotals {
    pub files: u64,
    pub bytes: u64,
    /// Marked files of which every copy is marked: deleting them all would lose the file.
    pub last_files: u64,
    pub last_bytes: u64,
}

impl MarkTotals {
    pub(crate) fn add(&mut self, bytes: u64, last: bool) {
        self.files += 1;
        self.bytes += bytes;
        if last {
            self.last_files += 1;
            self.last_bytes += bytes;
        }
    }

    pub fn is_empty(&self) -> bool {
        self.files == 0
    }
}

/// Marked files under a folder.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DirMarks {
    pub files: u32,
    pub bytes: u64,
    pub last_files: u32,
}

/// What one rule does in the index.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuleEffect {
    /// `None`: the path is not in the index.
    pub place: Option<Place>,
    /// Files this rule decides (no more specific rule overrides it).
    pub decides: u64,
    /// The ones of those it marks.
    pub marked: MarkTotals,
}

/// What a set of rules marks in an index.
#[derive(Debug, Clone, Default)]
pub struct MarkState {
    marked: Vec<u64>,
    last: Vec<u64>,
    /// Per content: copies marked (empty when nothing is marked).
    content_marked: Vec<u32>,
    /// Per folder: marked files below it (empty when nothing is marked).
    dirs: Vec<DirMarks>,
    /// Per source (index order).
    pub sources: Vec<MarkTotals>,
    pub total: MarkTotals,
    /// Per rule, in the order given.
    pub rules: Vec<RuleEffect>,
    at: HashMap<Place, usize>,
}

fn bit(bits: &[u64], i: u32) -> bool {
    bits.get(i as usize / 64).is_some_and(|w| w & (1 << (i % 64)) != 0)
}

fn set_bit(bits: &mut [u64], i: u32) {
    bits[i as usize / 64] |= 1 << (i % 64);
}

impl MarkState {
    /// Apply `rules` (found at `places`, see [`resolve`]) to the index.
    pub fn compute(ix: &Index, rules: &[MarkRule], places: &[Option<Place>]) -> MarkState {
        assert_eq!(rules.len(), places.len(), "one place per rule");
        let n = ix.file_count();
        let mut st = MarkState {
            sources: vec![MarkTotals::default(); ix.sources().len()],
            rules: places.iter().map(|&place| RuleEffect { place, ..Default::default() }).collect(),
            ..Default::default()
        };
        for (i, p) in places.iter().enumerate() {
            if let Some(p) = p {
                // a later rule on the same place wins (it is applied later below)
                st.at.insert(*p, i);
            }
        }
        if st.at.is_empty() || n == 0 {
            return st;
        }
        // shallow rules first, so deeper ones overwrite them; on one place the later rule wins
        let mut order: Vec<usize> = st.at.values().copied().collect();
        let depth = |p: Place| -> (u32, u8) {
            match p {
                Place::Dir(d) => (ix.dir_depth(d) as u32, 0),
                Place::File(f) => (ix.dir_depth(ix.file_dir(f)) as u32 + 1, 1),
            }
        };
        order.sort_by_key(|&i| (depth(places[i].unwrap()), i));
        let mut decided: Vec<u32> = vec![NONE; n];
        for &i in &order {
            match places[i].unwrap() {
                Place::Dir(d) => {
                    let r = ix.subtree_files(d);
                    decided[r.start as usize..r.end as usize].fill(i as u32);
                }
                Place::File(f) => decided[f as usize] = i as u32,
            }
        }
        let mut marked = vec![0u64; n.div_ceil(64)];
        // folders whose rule marks only what has a copy outside them: count copies inside
        let mut inside: Vec<u32> = Vec::new();
        for &i in &order {
            let rule = &rules[i];
            match (places[i].unwrap(), rule.action) {
                (Place::Dir(d), MarkAction::DeleteDuplicates) => {
                    if inside.is_empty() {
                        inside = vec![0; ix.content_count()];
                    }
                    let range = ix.subtree_files(d);
                    let mut touched: Vec<ContentIx> = Vec::new();
                    for f in range.clone() {
                        let c = ix.file_content(f);
                        if inside[c as usize] == 0 {
                            touched.push(c);
                        }
                        inside[c as usize] += 1;
                    }
                    for f in range {
                        if decided[f as usize] == i as u32 {
                            let c = ix.file_content(f);
                            if ix.copies(c) > inside[c as usize] {
                                set_bit(&mut marked, f);
                            }
                        }
                    }
                    for c in touched {
                        inside[c as usize] = 0;
                    }
                }
                (Place::Dir(d), MarkAction::Delete) => {
                    for f in ix.subtree_files(d) {
                        if decided[f as usize] == i as u32 {
                            set_bit(&mut marked, f);
                        }
                    }
                }
                (Place::File(f), MarkAction::Delete) => {
                    if decided[f as usize] == i as u32 {
                        set_bit(&mut marked, f);
                    }
                }
                (Place::File(f), MarkAction::DeleteDuplicates) => {
                    if decided[f as usize] == i as u32 && ix.copies(ix.file_content(f)) > 1 {
                        set_bit(&mut marked, f);
                    }
                }
                (_, MarkAction::Keep) => {}
            }
        }
        // copies marked per content, then the files that would lose their last copy
        let mut content_marked = vec![0u32; ix.content_count()];
        for (w, &word) in marked.iter().enumerate() {
            let mut b = word;
            while b != 0 {
                let f = (w * 64) as u32 + b.trailing_zeros();
                b &= b - 1;
                content_marked[ix.file_content(f) as usize] += 1;
            }
        }
        let mut last = vec![0u64; marked.len()];
        let mut dirs = vec![DirMarks::default(); ix.dir_count()];
        for f in 0..n as FileIx {
            let rule = decided[f as usize];
            if rule != NONE {
                st.rules[rule as usize].decides += 1;
            }
            if !bit(&marked, f) {
                continue;
            }
            let c = ix.file_content(f);
            let is_last = content_marked[c as usize] >= ix.copies(c);
            if is_last {
                set_bit(&mut last, f);
            }
            let bytes = ix.file_size(f);
            st.total.add(bytes, is_last);
            st.sources[ix.file_source(f) as usize].add(bytes, is_last);
            if rule != NONE {
                st.rules[rule as usize].marked.add(bytes, is_last);
            }
            let dm = &mut dirs[ix.file_dir(f) as usize];
            dm.files += 1;
            dm.bytes += bytes;
            dm.last_files += is_last as u32;
        }
        // folders are numbered in pre-order: children after parents, so add up from the end
        for d in (0..ix.dir_count() as DirIx).rev() {
            if let Some(p) = ix.dir_parent(d) {
                let child = dirs[d as usize];
                let dp = &mut dirs[p as usize];
                dp.files += child.files;
                dp.bytes += child.bytes;
                dp.last_files += child.last_files;
            }
        }
        st.marked = marked;
        st.last = last;
        st.content_marked = content_marked;
        st.dirs = dirs;
        st
    }

    pub fn is_empty(&self) -> bool {
        self.total.files == 0
    }

    pub fn is_marked(&self, f: FileIx) -> bool {
        bit(&self.marked, f)
    }

    /// Marked, and every copy of it is marked too.
    pub fn is_last_copy(&self, f: FileIx) -> bool {
        bit(&self.last, f)
    }

    /// Marked files below a folder.
    pub fn dir(&self, d: DirIx) -> DirMarks {
        self.dirs.get(d as usize).copied().unwrap_or_default()
    }

    /// Copies of a content that are marked.
    pub fn marked_copies(&self, c: ContentIx) -> u32 {
        self.content_marked.get(c as usize).copied().unwrap_or(0)
    }

    /// The rule on this file or folder, if there is one.
    pub fn rule_at(&self, p: Place) -> Option<usize> {
        self.at.get(&p).copied()
    }

    /// Every marked file, in index order.
    pub fn marked_files(&self) -> impl Iterator<Item = FileIx> + '_ {
        self.marked.iter().enumerate().flat_map(|(w, &word)| {
            let mut b = word;
            std::iter::from_fn(move || {
                if b == 0 {
                    return None;
                }
                let f = (w * 64) as u32 + b.trailing_zeros();
                b &= b - 1;
                Some(f)
            })
        })
    }

    /// Marked files that would lose their last copy, in index order.
    pub fn last_copy_files(&self) -> impl Iterator<Item = FileIx> + '_ {
        self.marked_files().filter(|&f| self.is_last_copy(f))
    }
}

/// Rules after an edit: `add` replaces any rule on the same place; `remove` drops rules by place.
/// Returns the rules actually added and removed (for undo).
pub fn edit(rules: &mut Vec<MarkRule>, add: &[MarkRule], remove: &[PlaceKey]) -> (Vec<MarkRule>, Vec<MarkRule>) {
    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut drop: std::collections::HashSet<PlaceKey> = remove.iter().cloned().collect();
    for r in add {
        drop.insert(r.place_key());
    }
    let mut kept = Vec::with_capacity(rules.len());
    for r in rules.drain(..) {
        if drop.contains(&r.place_key()) {
            removed.push(r);
        } else {
            kept.push(r);
        }
    }
    *rules = kept;
    for r in add {
        // the same rule added again is no change
        if let Some(pos) = removed.iter().position(|x| x == r) {
            removed.remove(pos);
        } else {
            added.push(r.clone());
        }
        rules.push(r.clone());
    }
    (added, removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    const HEADER: &str = "hash\tsize\thost\tpath";

    fn h(n: u8) -> String {
        format!("{:064x}", n as u128 + 1)
    }

    /// PC1 D: holds a photo folder, a backup of it and two unique files; the NAS has one photo.
    fn fixture() -> (Db, Index) {
        let mut db = Db::open_in_memory().unwrap();
        let pc = format!(
            "{HEADER}\n\
             {a}\t100\tPC1 D: (Data)\tD:\\Photos\\a.jpg\n\
             {b}\t200\tPC1 D: (Data)\tD:\\Photos\\b.jpg\n\
             {u}\t50\tPC1 D: (Data)\tD:\\Photos\\unique.jpg\n\
             {a}\t100\tPC1 D: (Data)\tD:\\Backup\\Photos\\a.jpg\n\
             {b}\t200\tPC1 D: (Data)\tD:\\Backup\\Photos\\b.jpg\n\
             {v}\t70\tPC1 D: (Data)\tD:\\Backup\\only-here.bin\n",
            a = h(1),
            b = h(2),
            u = h(3),
            v = h(4)
        );
        let nas = format!("{HEADER}\n{a}\t100\tnas\t/mnt/tank/a.jpg\n", a = h(1));
        db.import_reader("pc.tsv", "pc.tsv", 0, None, pc.as_bytes(), &mut |_| true).unwrap();
        db.import_reader("nas.tsv", "nas.tsv", 0, None, nas.as_bytes(), &mut |_| true).unwrap();
        let ix = Index::build(&db).unwrap();
        (db, ix)
    }

    fn rule(host: &str, path: &str, is_dir: bool, action: MarkAction) -> MarkRule {
        MarkRule { host: host.into(), path: path.into(), is_dir, action }
    }

    fn marked_paths(ix: &Index, db: &Db, st: &MarkState) -> Vec<String> {
        let mut v: Vec<String> = st.marked_files().map(|f| ix.file_path(db, f).unwrap()).collect();
        v.sort();
        v
    }

    fn state(ix: &Index, db: &Db, rules: &[MarkRule]) -> MarkState {
        let places = resolve(ix, db, rules).unwrap();
        MarkState::compute(ix, rules, &places)
    }

    #[test]
    fn rules_find_their_files_and_folders() {
        let (db, ix) = fixture();
        let rules = [
            rule("PC1 D: (Data)", r"D:\Backup", true, MarkAction::Delete),
            // Windows paths ignore case
            rule("PC1 D: (Data)", r"d:\photos\A.JPG", false, MarkAction::Delete),
            rule("nas", "/mnt/tank/a.jpg", false, MarkAction::Keep),
            rule("nas", "", true, MarkAction::Delete),
            rule("nas", "/mnt/tank/gone.jpg", false, MarkAction::Delete),
            rule("PC9", r"C:\x", true, MarkAction::Delete),
        ];
        let places = resolve(&ix, &db, &rules).unwrap();
        assert!(matches!(places[0], Some(Place::Dir(d)) if ix.dir_path(d) == r"D:\Backup"));
        assert!(matches!(places[1], Some(Place::File(f)) if ix.file_path(&db, f).unwrap() == r"D:\Photos\a.jpg"));
        assert!(matches!(places[2], Some(Place::File(_))));
        assert!(matches!(places[3], Some(Place::Dir(d)) if ix.dir_parent(d).is_none()));
        assert_eq!(places[4], None);
        assert_eq!(places[5], None);
        // and rules made from the index find the same places again
        for d in 0..ix.dir_count() as DirIx {
            let r = MarkRule::for_dir(&ix, d, MarkAction::Delete);
            assert_eq!(resolve(&ix, &db, &[r]).unwrap()[0], Some(Place::Dir(d)));
        }
        for f in 0..ix.file_count() as FileIx {
            let name = db.file_record(ix.file_db_id(f)).unwrap().name;
            let r = MarkRule::for_file(&ix, ix.file_dir(f), &name, MarkAction::Delete);
            assert_eq!(resolve(&ix, &db, &[r]).unwrap()[0], Some(Place::File(f)));
        }
    }

    #[test]
    fn deeper_rules_win_and_last_copies_are_found() {
        let (db, ix) = fixture();
        // delete the backup folder, but keep only-here.bin
        let st = state(
            &ix,
            &db,
            &[
                rule("PC1 D: (Data)", r"D:\Backup", true, MarkAction::Delete),
                rule("PC1 D: (Data)", r"D:\Backup\only-here.bin", false, MarkAction::Keep),
            ],
        );
        assert_eq!(marked_paths(&ix, &db, &st), [r"D:\Backup\Photos\a.jpg", r"D:\Backup\Photos\b.jpg"]);
        assert_eq!(st.total, MarkTotals { files: 2, bytes: 300, last_files: 0, last_bytes: 0 });
        assert_eq!(st.rules[0].decides, 2);
        assert_eq!(st.rules[1].decides, 1);
        assert_eq!(st.rules[1].marked.files, 0);
        // now also the photos folder: b.jpg has no copy left; a.jpg still has the NAS copy
        let st = state(
            &ix,
            &db,
            &[
                rule("PC1 D: (Data)", r"D:\Backup", true, MarkAction::Delete),
                rule("PC1 D: (Data)", r"D:\Photos", true, MarkAction::Delete),
            ],
        );
        let last: Vec<String> = st.last_copy_files().map(|f| ix.file_path(&db, f).unwrap()).collect();
        assert_eq!(last.len(), 4, "b twice, unique.jpg, only-here.bin; not a.jpg: {last:?}");
        assert!(last.iter().all(|p| !p.ends_with("a.jpg")));
        assert_eq!(st.total.files, 6);
        assert_eq!(st.total.last_bytes, 200 + 200 + 50 + 70);
        // totals per folder add up
        let d = |p: &str| (0..ix.dir_count() as DirIx).find(|&d| ix.dir_path(d) == p).unwrap();
        assert_eq!(st.dir(d(r"D:\Backup")).files, 3);
        assert_eq!(st.dir(d(r"D:\Backup")).bytes, 370);
        assert_eq!(st.dir(d(r"D:\")).files, 6);
        assert_eq!(st.dir(d("/mnt/tank")).files, 0);
        let pc = ix.sources().iter().position(|s| s.host == "PC1 D: (Data)").unwrap();
        assert_eq!(st.sources[pc].files, 6);
    }

    #[test]
    fn delete_duplicates_marks_what_has_a_copy_outside_the_folder() {
        let (db, ix) = fixture();
        let st = state(&ix, &db, &[rule("PC1 D: (Data)", r"D:\Backup", true, MarkAction::DeleteDuplicates)]);
        assert_eq!(marked_paths(&ix, &db, &st), [r"D:\Backup\Photos\a.jpg", r"D:\Backup\Photos\b.jpg"]);
        assert_eq!(st.total.last_files, 0);
        // the whole drive: a.jpg has a copy on the NAS, nothing else has one outside D:\
        let st = state(&ix, &db, &[rule("PC1 D: (Data)", r"D:\", true, MarkAction::DeleteDuplicates)]);
        assert_eq!(marked_paths(&ix, &db, &st), [r"D:\Backup\Photos\a.jpg", r"D:\Photos\a.jpg"]);
        // a file: marked if it has any other copy
        let st =
            state(&ix, &db, &[rule("PC1 D: (Data)", r"D:\Photos\unique.jpg", false, MarkAction::DeleteDuplicates)]);
        assert!(st.is_empty());
        let st = state(&ix, &db, &[rule("PC1 D: (Data)", r"D:\Photos\b.jpg", false, MarkAction::DeleteDuplicates)]);
        assert_eq!(st.total.files, 1);
        assert_eq!(st.marked_copies(ix.file_content(st.marked_files().next().unwrap())), 1);
    }

    /// Against a plain per-file reading of the rules, on generated data with random rules.
    #[test]
    fn matches_a_brute_force_reading_of_the_rules() {
        use crate::synth::{generate, Config};
        let dir = std::env::temp_dir().join(format!("dupemap-marks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = Config { rows: 3000, ..Config::default() };
        let exp = generate(Some(&dir), &cfg).unwrap();
        let mut db = Db::open_in_memory().unwrap();
        for m in &exp.manifests {
            db.import_file(&dir.join(&m.file_name), &mut |_| true).unwrap();
        }
        let ix = Index::build(&db).unwrap();
        let mut seed = 7u64;
        let mut next = |n: usize| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((seed >> 33) as usize) % n.max(1)
        };
        for round in 0..20 {
            let mut rules = Vec::new();
            for _ in 0..(3 + round) {
                let action = [MarkAction::Delete, MarkAction::DeleteDuplicates, MarkAction::Keep][next(3)];
                if next(3) == 0 {
                    let f = next(ix.file_count()) as FileIx;
                    let name = db.file_record(ix.file_db_id(f)).unwrap().name;
                    rules.push(MarkRule::for_file(&ix, ix.file_dir(f), &name, action));
                } else {
                    rules.push(MarkRule::for_dir(&ix, next(ix.dir_count()) as DirIx, action));
                }
            }
            let places = resolve(&ix, &db, &rules).unwrap();
            let st = MarkState::compute(&ix, &rules, &places);
            // brute force: per file, the deepest rule on it or an ancestor (later wins on a tie)
            let mut want = vec![false; ix.file_count()];
            for f in 0..ix.file_count() as FileIx {
                let mut best: Option<(u32, usize)> = None;
                for (i, p) in places.iter().enumerate() {
                    let depth = match p {
                        Some(Place::File(g)) if *g == f => ix.dir_depth(ix.file_dir(f)) as u32 + 1,
                        Some(Place::Dir(d)) if ix.dir_within(ix.file_dir(f), *d) => ix.dir_depth(*d) as u32,
                        _ => continue,
                    };
                    if best.is_none_or(|(bd, bi)| (depth, i) > (bd, bi)) {
                        best = Some((depth, i));
                    }
                }
                let Some((_, i)) = best else { continue };
                let c = ix.file_content(f);
                want[f as usize] = match (places[i].unwrap(), rules[i].action) {
                    (_, MarkAction::Keep) => false,
                    (_, MarkAction::Delete) => true,
                    (Place::File(_), MarkAction::DeleteDuplicates) => ix.copies(c) > 1,
                    (Place::Dir(d), MarkAction::DeleteDuplicates) => {
                        ix.content_files(c).iter().any(|&g| !ix.dir_within(ix.file_dir(g), d))
                    }
                };
            }
            for f in 0..ix.file_count() as FileIx {
                assert_eq!(st.is_marked(f), want[f as usize], "round {round}, file {f}");
                let c = ix.file_content(f);
                let all = ix.content_files(c).iter().all(|&g| want[g as usize]);
                assert_eq!(st.is_last_copy(f), want[f as usize] && all, "round {round}, last copy {f}");
            }
            let n_marked = want.iter().filter(|&&m| m).count() as u64;
            assert_eq!(st.total.files, n_marked);
            assert_eq!(st.sources.iter().map(|s| s.files).sum::<u64>(), n_marked);
            let roots: u64 = ix.sources().iter().map(|s| st.dir(s.root).files as u64).sum();
            assert_eq!(roots, n_marked);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn editing_replaces_rules_on_the_same_place() {
        let mut rules = vec![rule("PC1 D: (Data)", r"D:\A", true, MarkAction::Delete)];
        // the same place, another case: replaces
        let (added, removed) = edit(&mut rules, &[rule("PC1 D: (Data)", r"d:\a", true, MarkAction::Keep)], &[]);
        assert_eq!((added.len(), removed.len()), (1, 1));
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].action, MarkAction::Keep);
        // Linux paths do not ignore case
        edit(
            &mut rules,
            &[rule("nas", "/A", true, MarkAction::Delete), rule("nas", "/a", true, MarkAction::Delete)],
            &[],
        );
        assert_eq!(rules.len(), 3);
        // adding the same rule again changes nothing
        let (added, removed) = edit(&mut rules, &[rule("nas", "/A", true, MarkAction::Delete)], &[]);
        assert!(added.is_empty() && removed.is_empty());
        let (added, removed) = edit(&mut rules, &[], &[rule("nas", "/a", true, MarkAction::Keep).place_key()]);
        assert!(added.is_empty());
        assert_eq!(removed.len(), 1);
        assert_eq!(rules.len(), 2);
    }
}
