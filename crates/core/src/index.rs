//! The in-memory duplicate index.
//!
//! Built from the database in one pass. Folders are numbered in depth-first pre-order (each
//! source's tree in turn, children sorted by name), and files are grouped by folder in the same
//! order, so every folder's subtree is a contiguous range of folders and of files.
//!
//! Files with the same algorithm, digest and size share a *content*. Per content the index keeps
//! the copy count, the copies per source, the list of copies, and the lowest folder containing
//! every copy (its LCA, when all copies are in one source). With the LCA, "bytes in this folder
//! that also exist outside it" is a subtree sum of +size at the file's folder and -size at the
//! LCA, so statistics for every folder, under any filter, take one linear pass.
//!
//! File names stay in SQLite and are fetched per folder when it is listed.

use crate::db::{Db, ALGO_SHA256};
use crate::error::{err, Result};
use crate::ignore::{self, IgnoreSettings, RuleEffect};
use crate::natsort::natural_cmp;
use crate::paths::{fold_case, join_path, PathKind};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

pub type DirIx = u32;
pub type FileIx = u32;
pub type ContentIx = u32;
pub type SourceIx = u16;

/// "No folder".
pub const NONE: u32 = u32::MAX;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceInfo {
    pub db_id: i64,
    pub host: String,
    pub kind: PathKind,
    /// The source's root folder (represents the whole source).
    pub root: DirIx,
}

/// Statistics of a folder's subtree, over the files matching the active filter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirStats {
    pub bytes: u64,
    pub files: u32,
    /// Files whose content exists at least twice anywhere.
    pub dup_files: u32,
    /// Bytes of files whose content also exists outside this folder.
    pub elsewhere_bytes: u64,
    /// Files whose content also exists outside this folder.
    pub elsewhere_files: u32,
}

impl DirStats {
    /// Share of this folder's bytes that also exist outside it, 0..=100.
    pub fn pct_elsewhere(&self) -> f64 {
        if self.bytes == 0 {
            0.0
        } else {
            self.elsewhere_bytes as f64 * 100.0 / self.bytes as f64
        }
    }

    /// Every file in the folder has at least one copy outside it.
    pub fn fully_elsewhere(&self) -> bool {
        self.files > 0 && self.elsewhere_files == self.files
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CopiesFilter {
    #[default]
    Any,
    /// Exactly one copy anywhere.
    Unique,
    /// Two or more copies anywhere.
    Duplicated,
    /// At least one copy in another source.
    InOtherSources,
    /// No copy in any other source (may still be duplicated within its own source).
    OnlyThisSource,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HashFilter {
    #[default]
    Any,
    Sha256,
    /// Non-SHA-256 rows (flagged on import).
    Flagged,
}

/// Which files a pane shows. Folder statistics are computed over matching files only; copies
/// are always counted over all files.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Filter {
    pub copies: CopiesFilter,
    /// Only files with no copy in this source (database id).
    pub absent_from: Option<i64>,
    /// Only files with a copy in this source (database id).
    pub present_in: Option<i64>,
    pub hash: HashFilter,
}

impl Filter {
    pub fn is_all(&self) -> bool {
        *self == Filter::default()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SortKey {
    #[default]
    Name,
    Size,
    /// Folders: files inside (a file counts as one).
    Files,
    /// Folders: files with another copy anywhere; files: copies anywhere.
    Copies,
    /// Folders: share of the bytes that also exist outside the folder; files: copies in other
    /// sources.
    Elsewhere,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ListOptions {
    pub sort: SortKey,
    pub descending: bool,
    pub folders_first: bool,
    pub filter: Filter,
}

impl Default for ListOptions {
    fn default() -> Self {
        ListOptions { sort: SortKey::Name, descending: false, folders_first: true, filter: Filter::default() }
    }
}

/// A node in a pane's tree: the virtual "all sources" root or a folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Node {
    AllSources,
    Dir(DirIx),
}

#[derive(Debug, Clone, PartialEq)]
pub struct DirRow {
    pub dir: DirIx,
    pub name: String,
    pub stats: DirStats,
    /// Has child folders with matching files.
    pub has_subdirs: bool,
    /// Matching files directly in this folder (not in subfolders).
    pub direct_files: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRow {
    pub file: FileIx,
    pub db_id: i64,
    pub name: String,
    pub size: u64,
    pub content: ContentIx,
    /// Copies anywhere, including this one.
    pub copies: u32,
    /// Copies in sources other than this file's source.
    pub copies_other_sources: u32,
    pub flagged: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Listing {
    pub node: Node,
    pub stats: DirStats,
    pub dirs: Vec<DirRow>,
    pub files: Vec<FileRow>,
}

/// One copy of a content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub file: FileIx,
    pub db_id: i64,
    pub source: SourceIx,
    pub host: String,
    pub path: String,
    pub size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DupGroup {
    pub content: ContentIx,
    pub size: u64,
    pub copies: u32,
    /// Bytes that would be freed by keeping one copy: size × (copies − 1).
    pub wasted: u64,
    /// Number of sources holding a copy.
    pub sources: u32,
    pub flagged: bool,
}

/// How the copies of a duplicate group are spread over sources.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Spread {
    #[default]
    Any,
    /// Every copy is in one source (duplicates on the same machine, drive or cloud account).
    OneSource,
    /// Copies in two or more sources.
    SeveralSources,
}

/// Order of duplicate groups. Ties keep the default order (most bytes in extra copies first).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum GroupSort {
    /// Bytes in copies beyond one.
    #[default]
    Extra,
    /// Size of one copy.
    Each,
    Copies,
    /// Sources holding a copy.
    Sources,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DupQuery {
    /// Only groups with a copy in this source (database id).
    pub involving: Option<i64>,
    pub hash: HashFilter,
    pub min_copies: u32,
    #[serde(default)]
    pub spread: Spread,
    #[serde(default)]
    pub sort: GroupSort,
    /// Smallest first (the default is largest first).
    #[serde(default)]
    pub ascending: bool,
}

/// Where the files under a folder have copies outside it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutsideCopies {
    /// Files and bytes under the folder.
    pub files: u64,
    pub bytes: u64,
    /// (source, files, bytes): files under the folder with at least one copy outside it in that
    /// source. A file counts once per source holding a copy. Most bytes first.
    pub by_source: Vec<(SourceIx, u64, u64)>,
    /// Files with no copy anywhere outside the folder.
    pub alone_files: u64,
    pub alone_bytes: u64,
    /// For each source in `by_source`, in the same order: the deepest folder of that source that
    /// still holds a copy of at least [`MAIN_SHARE`] of those bytes, and the bytes it holds
    /// ("mostly under E:\Backup\Photos"). Copies spread all over a source give its top folder.
    pub main_dir: Vec<(SourceIx, DirIx, u64)>,
}

/// Share of a source's copies a folder must hold to be named as where they mostly are.
pub const MAIN_SHARE: f64 = 0.9;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSummary {
    pub source: SourceIx,
    pub db_id: i64,
    pub host: String,
    pub kind: PathKind,
    pub files: u64,
    pub bytes: u64,
    /// Files with another copy anywhere.
    pub dup_files: u64,
    /// Bytes of files that also exist in another source.
    pub bytes_in_other_sources: u64,
    /// Files that exist only in this source.
    pub only_here_files: u64,
    pub flagged_files: u64,
}

/// Folder statistics for one filter.
#[derive(Debug, Clone)]
pub struct Rollup {
    pub stats: Vec<DirStats>,
}

/// The first 8 bytes of a hash's text (algorithms other than SHA-256 keep their hash as text),
/// big-endian, padded with zeros: what [`Index::contents_with_prefix`] compares.
pub fn text_prefix(text: &[u8]) -> u64 {
    let mut b = [0u8; 8];
    let n = text.len().min(8);
    b[..n].copy_from_slice(&text[..n]);
    u64::from_be_bytes(b)
}

pub struct Index {
    sources: Vec<SourceInfo>,
    source_by_db: HashMap<i64, SourceIx>,
    // folders, dense pre-order
    dir_db: Vec<i64>,
    dir_parent: Vec<u32>,
    dir_source: Vec<SourceIx>,
    dir_depth: Vec<u16>,
    dir_end: Vec<u32>,
    dir_name: Vec<Box<str>>,
    child_start: Vec<u32>,
    children: Vec<u32>,
    dir_file_start: Vec<u32>,
    // files, grouped by folder
    file_db: Vec<i64>,
    file_dir: Vec<u32>,
    file_size: Vec<u64>,
    file_content: Vec<u32>,
    file_ext: Vec<u32>,
    /// Files whose size is not known (checksum lists), sorted; their `file_size` is 0.
    size_unknown: Vec<u32>,
    /// Files whose size came from a copy with the same hash listed with a size, sorted.
    size_inferred: Vec<u32>,
    /// Dense file / folder indices sorted by database id (built on first use: searches,
    /// restoring a session).
    files_by_db: std::sync::OnceLock<Vec<u32>>,
    dirs_by_db: std::sync::OnceLock<Vec<u32>>,
    // contents, numbered in order of first appearance in the tree (cache locality)
    hot: Vec<ContentHot>,
    content_size: Vec<u64>,
    /// The first 8 bytes of each SHA-256 content's hash (big-endian; 0 for other algorithms):
    /// finds a hash without reading the file table.
    content_prefix: Vec<u64>,
    content_file_start: Vec<u32>,
    content_files: Vec<u32>,
    content_src: Vec<(SourceIx, u32)>,
    dup_order: Vec<u32>,
    algos: Vec<String>,
    exts: Vec<String>,
    summaries: Vec<SourceSummary>,
    cache: Mutex<Vec<(Filter, Arc<Rollup>)>>,
    /// Duplicate groups in a non-default order, for the last few queries.
    group_orders: Mutex<Vec<(DupQuery, Arc<Vec<u32>>)>>,
    ignore: IgnoreSettings,
    ignore_effects: Vec<RuleEffect>,
}

/// Per-content data read for every file during a rollup, packed into 16 bytes.
#[derive(Debug, Clone, Copy)]
struct ContentHot {
    copies: u32,
    lca: u32,
    src_start: u32,
    src_len: u16,
    algo: u16,
}

const ROLLUP_CACHE: usize = 6;

/// `file.size` of -1 while loading: not known.
const UNKNOWN_SIZE: u64 = u64::MAX;

/// How a file's size is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeState {
    /// Listed with the file.
    Known,
    /// Not listed (checksum list), taken from a copy with the same hash that was.
    FromCopy,
    /// Not listed and no copy with a size: counted as 0 bytes.
    Unknown,
}

/// Positions 0..n sorted by their database id.
fn sorted_by_db(ids: &[i64]) -> Vec<u32> {
    let mut v: Vec<u32> = (0..ids.len() as u32).collect();
    v.sort_unstable_by_key(|&i| ids[i as usize]);
    v
}

impl Index {
    /// Build the index from the database, applying the ignore rules stored in it.
    pub fn build(db: &Db) -> Result<Index> {
        Self::build_with_progress(db, &mut |_, _| {})
    }

    /// Build with the stored ignore rules, reporting (stage, fraction 0..1).
    pub fn build_with_progress(db: &Db, progress: &mut dyn FnMut(&str, f32)) -> Result<Index> {
        let ignore = db.ignore_settings()?;
        Self::build_with(db, &ignore, progress)
    }

    /// Build with explicit ignore rules.
    pub fn build_with(db: &Db, ignore: &IgnoreSettings, progress: &mut dyn FnMut(&str, f32)) -> Result<Index> {
        let conn = db.conn();
        let rules = ignore::compile(ignore);
        let mut effects = vec![RuleEffect::default(); ignore.rules.len()];
        progress("sources", 0.0);
        let mut srcs = db.sources()?;
        srcs.sort_by(|a, b| natural_cmp(&a.host, &b.host));
        if srcs.len() > SourceIx::MAX as usize {
            return err("too many sources");
        }

        // ------------------------------------------------------------ folders
        progress("folders", 0.02);
        let mut t_db: Vec<i64> = Vec::new();
        let mut t_src: Vec<i64> = Vec::new();
        let mut t_parent_db: Vec<Option<i64>> = Vec::new();
        let mut t_name: Vec<Box<str>> = Vec::new();
        {
            let mut st = conn.prepare("SELECT id, source_id, parent_id, name FROM dir ORDER BY id")?;
            let mut rows = st.query([])?;
            while let Some(r) = rows.next()? {
                t_db.push(r.get(0)?);
                t_src.push(r.get(1)?);
                t_parent_db.push(r.get(2)?);
                t_name.push(r.get::<_, String>(3)?.into_boxed_str());
            }
        }
        let nt = t_db.len();
        let max_dir_id = t_db.iter().copied().max().unwrap_or(0).max(0) as usize;
        let mut t_of_db: Vec<u32> = vec![NONE; max_dir_id + 1];
        for (t, &id) in t_db.iter().enumerate() {
            t_of_db[id as usize] = t as u32;
        }
        let mut t_children: Vec<Vec<u32>> = vec![Vec::new(); nt];
        let mut root_of_src: HashMap<i64, u32> = HashMap::new();
        for t in 0..nt {
            match t_parent_db[t] {
                Some(p) => {
                    let pt = t_of_db.get(p as usize).copied().unwrap_or(NONE);
                    if pt != NONE {
                        t_children[pt as usize].push(t as u32);
                    }
                }
                None => {
                    root_of_src.insert(t_src[t], t as u32);
                }
            }
        }
        for ch in t_children.iter_mut() {
            ch.sort_by(|&a, &b| natural_cmp(&t_name[a as usize], &t_name[b as usize]));
        }
        // depth-first pre-order, sources in name order
        let mut order: Vec<u32> = Vec::with_capacity(nt);
        let mut sources: Vec<SourceInfo> = Vec::with_capacity(srcs.len());
        let mut source_by_db = HashMap::new();
        let mut t_dense: Vec<u32> = vec![NONE; nt];
        // rule index that hides a folder (its own match or an ancestor's), u16::MAX = shown
        let mut t_hidden_by: Vec<u16> = vec![u16::MAX; nt];
        for s in &srcs {
            let Some(&root) = root_of_src.get(&s.id) else { continue };
            let six = sources.len() as SourceIx;
            sources.push(SourceInfo { db_id: s.id, host: s.host.clone(), kind: s.kind, root: order.len() as u32 });
            source_by_db.insert(s.id, six);
            let src_rules: Vec<&ignore::Compiled> =
                rules.iter().filter(|r| r.host.as_deref().is_none_or(|h| h == s.host)).collect();
            // iterative pre-order walk keeping each rule's pattern position along the path
            t_dense[root as usize] = order.len() as u32;
            order.push(root);
            let mut frames: Vec<(u32, usize)> = vec![(root, 0)];
            let mut states: Vec<Vec<u64>> = vec![src_rules.iter().map(|r| r.start()).collect()];
            while let Some(frame) = frames.last_mut() {
                let t = frame.0 as usize;
                if frame.1 >= t_children[t].len() {
                    frames.pop();
                    states.pop();
                    continue;
                }
                let c = t_children[t][frame.1];
                frame.1 += 1;
                let parent_states = states.last().expect("aligned with frames");
                let mut child_states = Vec::with_capacity(src_rules.len());
                let mut hit: Option<usize> = None;
                if parent_states.iter().any(|&x| x != 0) {
                    let name = fold_case(&t_name[c as usize]);
                    for (k, r) in src_rules.iter().enumerate() {
                        let st = if parent_states[k] == 0 { 0 } else { r.step(parent_states[k], &name) };
                        if hit.is_none() && r.accepts(st) {
                            hit = Some(r.rule);
                        }
                        child_states.push(st);
                    }
                } else {
                    child_states.resize(src_rules.len(), 0);
                }
                if let Some(rule) = hit {
                    effects[rule].folders += 1;
                    let mut st = vec![c];
                    while let Some(x) = st.pop() {
                        t_hidden_by[x as usize] = rule as u16;
                        st.extend_from_slice(&t_children[x as usize]);
                    }
                    continue;
                }
                t_dense[c as usize] = order.len() as u32;
                order.push(c);
                frames.push((c, 0));
                states.push(child_states);
            }
        }
        let nd = order.len();
        let mut dir_db = Vec::with_capacity(nd);
        let mut dir_parent = Vec::with_capacity(nd);
        let mut dir_source = Vec::with_capacity(nd);
        let mut dir_name = Vec::with_capacity(nd);
        for &t in &order {
            let t = t as usize;
            dir_db.push(t_db[t]);
            let p = t_parent_db[t].and_then(|p| t_of_db.get(p as usize).copied()).unwrap_or(NONE);
            dir_parent.push(if p == NONE { NONE } else { t_dense[p as usize] });
            dir_source.push(*source_by_db.get(&t_src[t]).unwrap_or(&0));
            dir_name.push(std::mem::take(&mut t_name[t]));
        }
        drop(t_children);
        drop(t_name);
        let mut dir_depth = vec![0u16; nd];
        for d in 0..nd {
            let p = dir_parent[d];
            if p != NONE {
                dir_depth[d] = dir_depth[p as usize].saturating_add(1);
            }
        }
        let mut sub = vec![1u32; nd];
        for d in (0..nd).rev() {
            let p = dir_parent[d];
            if p != NONE {
                sub[p as usize] += sub[d];
            }
        }
        let dir_end: Vec<u32> = (0..nd).map(|d| d as u32 + sub[d]).collect();
        drop(sub);
        let mut child_count = vec![0u32; nd + 1];
        for &p in &dir_parent {
            if p != NONE {
                child_count[p as usize] += 1;
            }
        }
        let mut child_start = vec![0u32; nd + 1];
        for d in 0..nd {
            child_start[d + 1] = child_start[d] + child_count[d];
        }
        let mut children = vec![0u32; child_start[nd] as usize];
        let mut fill = child_start.clone();
        for (d, &p) in dir_parent.iter().enumerate() {
            if p != NONE {
                children[fill[p as usize] as usize] = d as u32;
                fill[p as usize] += 1;
            }
        }
        drop(fill);
        drop(child_count);
        let dense_of_db = |db_id: i64| -> u32 {
            let t = t_of_db.get(db_id as usize).copied().unwrap_or(NONE);
            if t == NONE {
                NONE
            } else {
                t_dense[t as usize]
            }
        };

        // ------------------------------------------------------------ files
        progress("files", 0.08);
        let total_visible: u64 =
            conn.query_row("SELECT count(*) FROM file WHERE shadowed_by IS NULL", [], |r| r.get::<_, i64>(0))? as u64;
        let mut r_db: Vec<i64> = Vec::with_capacity(total_visible as usize);
        let mut r_dir: Vec<u32> = Vec::with_capacity(total_visible as usize);
        let mut r_size: Vec<u64> = Vec::with_capacity(total_visible as usize);
        let mut r_algo: Vec<u16> = Vec::with_capacity(total_visible as usize);
        let mut r_ext: Vec<u32> = Vec::with_capacity(total_visible as usize);
        let mut r_digest: Vec<[u8; 32]> = Vec::with_capacity(total_visible as usize);
        let mut other_digest: Vec<(u32, Vec<u8>)> = Vec::new();
        {
            let mut st = conn.prepare(
                "SELECT id, dir_id, size, algo_id, digest, ext_id FROM file WHERE shadowed_by IS NULL ORDER BY id",
            )?;
            let mut rows = st.query([])?;
            let mut n = 0u64;
            while let Some(r) = rows.next()? {
                let dir_db_id: i64 = r.get(1)?;
                let dd = dense_of_db(dir_db_id);
                if dd == NONE {
                    let t = t_of_db.get(dir_db_id as usize).copied().unwrap_or(NONE);
                    if t != NONE && t_hidden_by[t as usize] != u16::MAX {
                        let e = &mut effects[t_hidden_by[t as usize] as usize];
                        e.files += 1;
                        e.bytes += r.get::<_, i64>(2)?.max(0) as u64;
                    }
                    continue;
                }
                let algo: i64 = r.get(3)?;
                let digest: &[u8] = match r.get_ref(4)? {
                    rusqlite::types::ValueRef::Blob(b) => b,
                    rusqlite::types::ValueRef::Text(t) => t,
                    _ => &[],
                };
                let idx = r_db.len() as u32;
                r_db.push(r.get(0)?);
                r_dir.push(dd);
                let size: i64 = r.get(2)?;
                r_size.push(if size < 0 { UNKNOWN_SIZE } else { size as u64 });
                r_algo.push(algo as u16);
                r_ext.push(r.get::<_, i64>(5)? as u32);
                if algo == ALGO_SHA256 && digest.len() == 32 {
                    let mut d = [0u8; 32];
                    d.copy_from_slice(digest);
                    r_digest.push(d);
                } else {
                    r_digest.push([0u8; 32]);
                    other_digest.push((idx, digest.to_vec()));
                }
                n += 1;
                if n.is_multiple_of(500_000) {
                    progress("files", 0.08 + 0.5 * (n as f32 / total_visible.max(1) as f32));
                }
            }
        }
        let nr = r_db.len();
        if nr >= NONE as usize {
            return err("too many files for one index");
        }

        // ------------------------------------------------------------ contents
        progress("contents", 0.6);
        let mut r_content: Vec<u32> = vec![NONE; nr];
        let mut content_algo: Vec<u16> = Vec::new();
        let mut content_size: Vec<u64> = Vec::new();
        let mut content_prefix: Vec<u64> = Vec::new();
        {
            let is_other: HashSet<u32> = other_digest.iter().map(|(i, _)| *i).collect();
            let mut keys: Vec<(u64, u32)> = (0..nr as u32)
                .filter(|i| !is_other.contains(i))
                .map(|i| (u64::from_le_bytes(r_digest[i as usize][..8].try_into().unwrap()), i))
                .collect();
            keys.sort_unstable();
            let mut i = 0;
            while i < keys.len() {
                let mut j = i + 1;
                while j < keys.len() && keys[j].0 == keys[i].0 {
                    j += 1;
                }
                let run = &mut keys[i..j];
                if run.len() > 1 {
                    run.sort_by(|a, b| {
                        let (x, y) = (a.1 as usize, b.1 as usize);
                        r_digest[x].cmp(&r_digest[y]).then(r_size[x].cmp(&r_size[y])).then(a.1.cmp(&b.1))
                    });
                }
                // Same digest and size = one content. Files of unknown size sort after the
                // known sizes of their digest and join that content; with no known size they
                // form a content of their own.
                let mut k = 0;
                let mut known: Option<(u32, usize)> = None;
                while k < run.len() {
                    let first = run[k].1 as usize;
                    if r_size[first] == UNKNOWN_SIZE {
                        if let Some((cid, f0)) = known.filter(|&(_, f0)| r_digest[f0] == r_digest[first]) {
                            let mut m = k;
                            while m < run.len() && r_digest[run[m].1 as usize] == r_digest[f0] {
                                r_content[run[m].1 as usize] = cid;
                                m += 1;
                            }
                            k = m;
                            continue;
                        }
                    }
                    let cid = content_algo.len() as u32;
                    content_algo.push(ALGO_SHA256 as u16);
                    content_size.push(r_size[first]);
                    content_prefix.push(u64::from_be_bytes(r_digest[first][..8].try_into().unwrap()));
                    let mut m = k;
                    while m < run.len() {
                        let x = run[m].1 as usize;
                        if r_digest[x] != r_digest[first] || r_size[x] != r_size[first] {
                            break;
                        }
                        r_content[x] = cid;
                        m += 1;
                    }
                    if r_size[first] != UNKNOWN_SIZE {
                        known = Some((cid, first));
                    }
                    k = m;
                }
                i = j;
            }
            drop(keys);
            // flagged hashes: known sizes first, then the unknown ones join a known size
            let mut other_map: HashMap<(u16, Vec<u8>, u64), u32> = HashMap::new();
            let mut other_known: HashMap<(u16, Vec<u8>), u32> = HashMap::new();
            other_digest.sort_by_key(|(idx, _)| r_size[*idx as usize] == UNKNOWN_SIZE);
            for (idx, dig) in other_digest.drain(..) {
                let i = idx as usize;
                if r_size[i] == UNKNOWN_SIZE {
                    if let Some(&cid) = other_known.get(&(r_algo[i], dig.clone())) {
                        r_content[i] = cid;
                        continue;
                    }
                }
                let key = (r_algo[i], dig, r_size[i]);
                let cid = *other_map.entry(key.clone()).or_insert_with(|| {
                    let cid = content_algo.len() as u32;
                    content_algo.push(r_algo[i]);
                    content_size.push(r_size[i]);
                    content_prefix.push(text_prefix(&key.1));
                    cid
                });
                if r_size[i] != UNKNOWN_SIZE {
                    other_known.entry((key.0, key.1)).or_insert(cid);
                }
                r_content[i] = cid;
            }
        }
        // A file of unknown size takes its content's size (inferred) or stays unknown (0 bytes).
        let mut r_size_state: Vec<u8> = Vec::new();
        if r_size.contains(&UNKNOWN_SIZE) {
            r_size_state = vec![0u8; nr];
            for i in 0..nr {
                if r_size[i] == UNKNOWN_SIZE {
                    let c = r_content[i] as usize;
                    if content_size[c] == UNKNOWN_SIZE {
                        r_size[i] = 0;
                        r_size_state[i] = 2;
                    } else {
                        r_size[i] = content_size[c];
                        r_size_state[i] = 1;
                    }
                }
            }
            for cs in content_size.iter_mut() {
                if *cs == UNKNOWN_SIZE {
                    *cs = 0;
                }
            }
        }
        drop(r_digest);
        drop(r_algo);
        let nc = content_algo.len();

        // ------------------------------------------------------------ dense file order
        progress("ordering", 0.75);
        let mut dir_file_start = vec![0u32; nd + 1];
        for &d in &r_dir {
            dir_file_start[d as usize + 1] += 1;
        }
        for d in 0..nd {
            dir_file_start[d + 1] += dir_file_start[d];
        }
        let mut perm = vec![0u32; nr];
        {
            let mut fill = dir_file_start.clone();
            for (row, &d) in r_dir.iter().enumerate() {
                perm[fill[d as usize] as usize] = row as u32;
                fill[d as usize] += 1;
            }
        }
        let file_db: Vec<i64> = perm.iter().map(|&r| r_db[r as usize]).collect();
        drop(r_db);
        let file_dir: Vec<u32> = perm.iter().map(|&r| r_dir[r as usize]).collect();
        drop(r_dir);
        let file_size: Vec<u64> = perm.iter().map(|&r| r_size[r as usize]).collect();
        drop(r_size);
        let (mut size_unknown, mut size_inferred) = (Vec::new(), Vec::new());
        if !r_size_state.is_empty() {
            for (f, &r) in perm.iter().enumerate() {
                match r_size_state[r as usize] {
                    1 => size_inferred.push(f as u32),
                    2 => size_unknown.push(f as u32),
                    _ => {}
                }
            }
        }
        drop(r_size_state);
        let mut file_content: Vec<u32> = perm.iter().map(|&r| r_content[r as usize]).collect();
        drop(r_content);
        // Renumber contents by first appearance in tree order. Backups copy whole folders, so
        // the copies then also have increasing ids, which keeps rollups cache-friendly.
        let (content_algo, content_size, content_prefix) = {
            let mut remap = vec![NONE; nc];
            let mut next = 0u32;
            for c in file_content.iter_mut() {
                let r = &mut remap[*c as usize];
                if *r == NONE {
                    *r = next;
                    next += 1;
                }
                *c = *r;
            }
            let mut algo2 = vec![0u16; nc];
            let mut size2 = vec![0u64; nc];
            let mut prefix2 = vec![0u64; nc];
            for old in 0..nc {
                let n = remap[old] as usize;
                algo2[n] = content_algo[old];
                size2[n] = content_size[old];
                prefix2[n] = content_prefix[old];
            }
            (algo2, size2, prefix2)
        };
        let file_ext: Vec<u32> = perm.iter().map(|&r| r_ext[r as usize]).collect();
        drop(r_ext);
        drop(perm);

        // ------------------------------------------------------------ copies
        progress("copies", 0.85);
        let mut content_file_start = vec![0u32; nc + 1];
        for &c in &file_content {
            content_file_start[c as usize + 1] += 1;
        }
        for c in 0..nc {
            content_file_start[c + 1] += content_file_start[c];
        }
        let mut content_files = vec![0u32; nr];
        {
            let mut fill = content_file_start.clone();
            for (f, &c) in file_content.iter().enumerate() {
                content_files[fill[c as usize] as usize] = f as u32;
                fill[c as usize] += 1;
            }
        }
        let mut hot: Vec<ContentHot> = Vec::with_capacity(nc);
        let mut content_src: Vec<(SourceIx, u32)> = Vec::with_capacity(nc);
        for c in 0..nc {
            let fs = &content_files[content_file_start[c] as usize..content_file_start[c + 1] as usize];
            let src_start = content_src.len() as u32;
            // files are in pre-order, so sources come in contiguous runs
            let mut i = 0;
            while i < fs.len() {
                let s = dir_source[file_dir[fs[i] as usize] as usize];
                let mut j = i + 1;
                while j < fs.len() && dir_source[file_dir[fs[j] as usize] as usize] == s {
                    j += 1;
                }
                content_src.push((s, (j - i) as u32));
                i = j;
            }
            let mut lca_dir = NONE;
            if fs.len() >= 2 {
                let a = file_dir[fs[0] as usize];
                let b = file_dir[fs[fs.len() - 1] as usize];
                if dir_source[a as usize] == dir_source[b as usize] {
                    lca_dir = lca(&dir_parent, &dir_depth, a, b);
                }
            }
            hot.push(ContentHot {
                copies: fs.len() as u32,
                lca: lca_dir,
                src_start,
                src_len: (content_src.len() as u32 - src_start) as u16,
                algo: content_algo[c],
            });
        }
        drop(content_algo);
        let mut dup_order: Vec<u32> = (0..nc as u32).filter(|&c| hot[c as usize].copies >= 2).collect();
        let wasted = |c: u32| -> u64 { content_size[c as usize].saturating_mul(hot[c as usize].copies as u64 - 1) };
        dup_order.sort_unstable_by(|&a, &b| {
            wasted(b).cmp(&wasted(a)).then(content_size[b as usize].cmp(&content_size[a as usize])).then(a.cmp(&b))
        });

        let mut algos: Vec<String> = Vec::new();
        {
            let mut st = conn.prepare("SELECT id, name FROM algo ORDER BY id")?;
            let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
            for r in rows {
                let (id, n) = r?;
                if algos.len() <= id as usize {
                    algos.resize(id as usize + 1, String::new());
                }
                algos[id as usize] = n;
            }
        }
        let mut exts: Vec<String> = Vec::new();
        {
            let mut st = conn.prepare("SELECT id, name FROM ext ORDER BY id")?;
            let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
            for r in rows {
                let (id, n) = r?;
                if exts.len() <= id as usize {
                    exts.resize(id as usize + 1, String::new());
                }
                exts[id as usize] = n;
            }
        }

        let mut idx = Index {
            sources,
            source_by_db,
            dir_db,
            dir_parent,
            dir_source,
            dir_depth,
            dir_end,
            dir_name,
            child_start,
            children,
            dir_file_start,
            file_db,
            file_dir,
            file_size,
            file_content,
            file_ext,
            size_unknown,
            size_inferred,
            files_by_db: std::sync::OnceLock::new(),
            dirs_by_db: std::sync::OnceLock::new(),
            hot,
            content_size,
            content_prefix,
            content_file_start,
            content_files,
            content_src,
            dup_order,
            algos,
            exts,
            summaries: Vec::new(),
            cache: Mutex::new(Vec::new()),
            group_orders: Mutex::new(Vec::new()),
            ignore: ignore.clone(),
            ignore_effects: effects,
        };
        progress("summaries", 0.95);
        idx.summaries = idx.compute_summaries();
        let _ = idx.rollup(&Filter::default());
        progress("done", 1.0);
        Ok(idx)
    }

    fn compute_summaries(&self) -> Vec<SourceSummary> {
        let mut out: Vec<SourceSummary> = self
            .sources
            .iter()
            .enumerate()
            .map(|(i, s)| SourceSummary {
                source: i as SourceIx,
                db_id: s.db_id,
                host: s.host.clone(),
                kind: s.kind,
                files: 0,
                bytes: 0,
                dup_files: 0,
                bytes_in_other_sources: 0,
                only_here_files: 0,
                flagged_files: 0,
            })
            .collect();
        for f in 0..self.file_db.len() {
            let s = self.file_source(f as FileIx) as usize;
            let c = self.file_content[f] as usize;
            let size = self.file_size[f];
            let o = &mut out[s];
            o.files += 1;
            o.bytes += size;
            if self.copies(c as ContentIx) >= 2 {
                o.dup_files += 1;
            }
            if self.copies_other_sources(f as FileIx) > 0 {
                o.bytes_in_other_sources += size;
            } else {
                o.only_here_files += 1;
            }
            if self.hot[c].algo != ALGO_SHA256 as u16 {
                o.flagged_files += 1;
            }
        }
        out
    }

    // ------------------------------------------------------------------ basic accessors

    pub fn sources(&self) -> &[SourceInfo] {
        &self.sources
    }

    pub fn source_ix(&self, db_id: i64) -> Option<SourceIx> {
        self.source_by_db.get(&db_id).copied()
    }

    pub fn source_summaries(&self) -> &[SourceSummary] {
        &self.summaries
    }

    pub fn dir_count(&self) -> usize {
        self.dir_db.len()
    }

    pub fn file_count(&self) -> usize {
        self.file_db.len()
    }

    pub fn content_count(&self) -> usize {
        self.hot.len()
    }

    /// The ignore rules this index was built with.
    pub fn ignore_settings(&self) -> &IgnoreSettings {
        &self.ignore
    }

    /// What each ignore rule hid, aligned with `ignore_settings().rules`.
    pub fn ignore_effects(&self) -> &[RuleEffect] {
        &self.ignore_effects
    }

    /// Totals hidden by all ignore rules.
    pub fn ignored_total(&self) -> RuleEffect {
        self.ignore_effects.iter().fold(RuleEffect::default(), |a, e| RuleEffect {
            folders: a.folders + e.folders,
            files: a.files + e.files,
            bytes: a.bytes + e.bytes,
        })
    }

    pub fn dup_group_count(&self) -> usize {
        self.dup_order.len()
    }

    pub fn dir_db_id(&self, d: DirIx) -> i64 {
        self.dir_db[d as usize]
    }

    /// Dense folder index for a database folder id.
    pub fn dir_by_db_id(&self, db_id: i64) -> Option<DirIx> {
        let by = self.dirs_by_db.get_or_init(|| sorted_by_db(&self.dir_db));
        by.binary_search_by_key(&db_id, |&d| self.dir_db[d as usize]).ok().map(|p| by[p])
    }

    pub fn dir_parent(&self, d: DirIx) -> Option<DirIx> {
        let p = self.dir_parent[d as usize];
        (p != NONE).then_some(p)
    }

    pub fn dir_source(&self, d: DirIx) -> SourceIx {
        self.dir_source[d as usize]
    }

    pub fn dir_name(&self, d: DirIx) -> &str {
        &self.dir_name[d as usize]
    }

    pub fn dir_depth(&self, d: DirIx) -> u16 {
        self.dir_depth[d as usize]
    }

    /// Child folders in name order.
    pub fn dir_children(&self, d: DirIx) -> &[DirIx] {
        &self.children[self.child_start[d as usize] as usize..self.child_start[d as usize + 1] as usize]
    }

    /// Is `d` inside (or equal to) `ancestor`?
    pub fn dir_within(&self, d: DirIx, ancestor: DirIx) -> bool {
        d >= ancestor && d < self.dir_end[ancestor as usize]
    }

    /// Files directly in a folder (dense range).
    pub fn dir_files(&self, d: DirIx) -> std::ops::Range<FileIx> {
        self.dir_file_start[d as usize]..self.dir_file_start[d as usize + 1]
    }

    /// Files anywhere under a folder (dense range).
    pub fn subtree_files(&self, d: DirIx) -> std::ops::Range<FileIx> {
        self.dir_file_start[d as usize]..self.dir_file_start[self.dir_end[d as usize] as usize]
    }

    pub fn file_db_id(&self, f: FileIx) -> i64 {
        self.file_db[f as usize]
    }

    /// Dense file index for a database file id (the lookup table is built on first use).
    pub fn file_by_db_id(&self, db_id: i64) -> Option<FileIx> {
        let by = self.files_by_db.get_or_init(|| sorted_by_db(&self.file_db));
        by.binary_search_by_key(&db_id, |&f| self.file_db[f as usize]).ok().map(|p| by[p])
    }

    /// Whether a file's size is known (see [`SizeState`]).
    pub fn size_state(&self, f: FileIx) -> SizeState {
        if self.size_unknown.binary_search(&f).is_ok() {
            SizeState::Unknown
        } else if self.size_inferred.binary_search(&f).is_ok() {
            SizeState::FromCopy
        } else {
            SizeState::Known
        }
    }

    /// Files whose size is not known at all (they count as 0 bytes).
    pub fn unknown_size_files(&self) -> &[FileIx] {
        &self.size_unknown
    }

    /// Is the size of a content's files unknown (it then reads as 0)?
    pub fn content_size_unknown(&self, c: ContentIx) -> bool {
        !self.size_unknown.is_empty()
            && self.content_size[c as usize] == 0
            && self.content_files(c).iter().all(|&f| self.size_unknown.binary_search(&f).is_ok())
    }

    pub fn file_dir(&self, f: FileIx) -> DirIx {
        self.file_dir[f as usize]
    }

    pub fn file_size(&self, f: FileIx) -> u64 {
        self.file_size[f as usize]
    }

    pub fn file_content(&self, f: FileIx) -> ContentIx {
        self.file_content[f as usize]
    }

    pub fn file_source(&self, f: FileIx) -> SourceIx {
        self.dir_source[self.file_dir[f as usize] as usize]
    }

    pub fn file_ext(&self, f: FileIx) -> &str {
        self.exts.get(self.file_ext[f as usize] as usize).map(String::as_str).unwrap_or("")
    }

    /// The interned extension id of a file (see [`Index::ext_name`]).
    pub fn file_ext_id(&self, f: FileIx) -> u32 {
        self.file_ext[f as usize]
    }

    /// Number of interned extension ids (`0..ext_count()`).
    pub fn ext_count(&self) -> usize {
        self.exts.len()
    }

    /// An interned extension: lower-case, without the dot, `""` for none.
    pub fn ext_name(&self, e: u32) -> &str {
        self.exts.get(e as usize).map(String::as_str).unwrap_or("")
    }

    /// (copies anywhere, number of sources holding one) of a content.
    pub fn copy_spread(&self, c: ContentIx) -> (u32, u16) {
        let h = &self.hot[c as usize];
        (h.copies, h.src_len)
    }

    pub fn file_flagged(&self, f: FileIx) -> bool {
        self.hot[self.file_content[f as usize] as usize].algo != ALGO_SHA256 as u16
    }

    /// Copies of a content anywhere.
    pub fn copies(&self, c: ContentIx) -> u32 {
        self.hot[c as usize].copies
    }

    /// Contents whose hash starts with `value` in the bits of `mask`, both over the first 8 bytes
    /// of the hash (big-endian): the bytes of a SHA-256, the text of other hashes (see
    /// [`text_prefix`]). `algo` picks the algorithms. In content order.
    pub fn contents_with_prefix(&self, value: u64, mask: u64, algo: impl Fn(u16) -> bool) -> Vec<ContentIx> {
        let v = value & mask;
        self.content_prefix
            .iter()
            .enumerate()
            .filter(|&(c, &p)| p & mask == v && algo(self.hot[c].algo))
            .map(|(c, _)| c as ContentIx)
            .collect()
    }

    /// The id of an algorithm by its name (`md5`...), if any file uses it.
    pub fn algo_id(&self, name: &str) -> Option<u16> {
        self.algos.iter().position(|a| a == name).map(|i| i as u16)
    }

    pub fn content_size(&self, c: ContentIx) -> u64 {
        self.content_size[c as usize]
    }

    pub fn content_algo_name(&self, c: ContentIx) -> &str {
        self.algos.get(self.hot[c as usize].algo as usize).map(String::as_str).unwrap_or("?")
    }

    /// Every copy of a content, in tree order.
    pub fn content_files(&self, c: ContentIx) -> &[FileIx] {
        &self.content_files
            [self.content_file_start[c as usize] as usize..self.content_file_start[c as usize + 1] as usize]
    }

    /// (source, copies in that source) for a content.
    pub fn content_sources(&self, c: ContentIx) -> &[(SourceIx, u32)] {
        let h = &self.hot[c as usize];
        &self.content_src[h.src_start as usize..h.src_start as usize + h.src_len as usize]
    }

    pub fn copies_in_source(&self, c: ContentIx, s: SourceIx) -> u32 {
        self.content_sources(c).iter().find(|(x, _)| *x == s).map(|(_, n)| *n).unwrap_or(0)
    }

    /// Copies of this file's content in sources other than its own.
    pub fn copies_other_sources(&self, f: FileIx) -> u32 {
        let c = self.file_content[f as usize];
        let h = &self.hot[c as usize];
        if h.copies == 1 || h.src_len == 1 {
            return 0; // every copy is in this file's own source
        }
        h.copies - self.copies_in_source(c, self.file_source(f))
    }

    /// The lowest folder containing every copy, if all copies are in one source.
    pub fn content_lca(&self, c: ContentIx) -> Option<DirIx> {
        let l = self.hot[c as usize].lca;
        (l != NONE).then_some(l)
    }

    // ------------------------------------------------------------------ paths

    /// Full path of a folder (`D:\a\b`, `/mnt/x`, `gdrive:/a`); the host for a source root.
    pub fn dir_path(&self, d: DirIx) -> String {
        let mut chain: Vec<&str> = Vec::new();
        let mut cur = d;
        while self.dir_parent[cur as usize] != NONE {
            chain.push(&self.dir_name[cur as usize]);
            cur = self.dir_parent[cur as usize];
        }
        if chain.is_empty() {
            return self.sources[self.dir_source[d as usize] as usize].host.clone();
        }
        chain.reverse();
        join_path(chain[0], chain[1..].iter().copied())
    }

    /// Full path of a folder plus a file name.
    pub fn path_with_name(&self, d: DirIx, name: &str) -> String {
        let mut chain: Vec<&str> = Vec::new();
        let mut cur = d;
        while self.dir_parent[cur as usize] != NONE {
            chain.push(&self.dir_name[cur as usize]);
            cur = self.dir_parent[cur as usize];
        }
        chain.reverse();
        match chain.split_first() {
            Some((anchor, rest)) => join_path(anchor, rest.iter().copied().chain(std::iter::once(name))),
            None => name.to_string(),
        }
    }

    /// Folders from the source root down to `d` (inclusive).
    pub fn dir_ancestors(&self, d: DirIx) -> Vec<DirIx> {
        let mut v = vec![d];
        let mut cur = d;
        while self.dir_parent[cur as usize] != NONE {
            cur = self.dir_parent[cur as usize];
            v.push(cur);
        }
        v.reverse();
        v
    }

    // ------------------------------------------------------------------ filters and rollups

    fn compile(&self, f: &Filter) -> Compiled {
        let map = |id: Option<i64>| -> Option<Option<SourceIx>> { id.map(|x| self.source_ix(x)) };
        Compiled { copies: f.copies, hash: f.hash, absent_from: map(f.absent_from), present_in: map(f.present_in) }
    }

    /// Does a file pass the filter?
    pub fn matches(&self, filter: &Filter, f: FileIx) -> bool {
        self.compile(filter).test(self, f)
    }

    /// The filter as a test to run on many files (prepared once).
    pub fn matcher(&self, filter: &Filter) -> impl Fn(FileIx) -> bool + '_ {
        let all = filter.is_all();
        let cf = self.compile(filter);
        move |f| all || cf.test(self, f)
    }

    /// Folder statistics for a filter (cached).
    pub fn rollup(&self, filter: &Filter) -> Arc<Rollup> {
        {
            let mut cache = self.cache.lock().unwrap();
            if let Some(pos) = cache.iter().position(|(f, _)| f == filter) {
                let hit = cache.remove(pos);
                let r = hit.1.clone();
                cache.push(hit);
                return r;
            }
        }
        let r = Arc::new(self.compute_rollup(filter));
        let mut cache = self.cache.lock().unwrap();
        if cache.len() >= ROLLUP_CACHE {
            cache.remove(0);
        }
        cache.push((filter.clone(), r.clone()));
        r
    }

    /// Compute folder statistics without the cache (benchmarks).
    #[doc(hidden)]
    pub fn rollup_uncached(&self, filter: &Filter) -> Rollup {
        self.compute_rollup(filter)
    }

    fn compute_rollup(&self, filter: &Filter) -> Rollup {
        let cf = self.compile(filter);
        let nd = self.dir_db.len();
        let mut bytes = vec![0u64; nd];
        let mut files = vec![0u32; nd];
        let mut dup = vec![0u32; nd];
        let mut eb = vec![0i64; nd];
        let mut ef = vec![0i32; nd];
        let all = filter.is_all();
        for f in 0..self.file_db.len() {
            if !all && !cf.test(self, f as FileIx) {
                continue;
            }
            let d = self.file_dir[f] as usize;
            let s = self.file_size[f];
            let c = self.file_content[f] as usize;
            bytes[d] += s;
            files[d] += 1;
            let h = &self.hot[c];
            if h.copies >= 2 {
                dup[d] += 1;
                eb[d] += s as i64;
                ef[d] += 1;
                let l = h.lca;
                if l != NONE {
                    eb[l as usize] -= s as i64;
                    ef[l as usize] -= 1;
                }
            }
        }
        for d in (0..nd).rev() {
            let p = self.dir_parent[d];
            if p != NONE {
                let p = p as usize;
                bytes[p] += bytes[d];
                files[p] += files[d];
                dup[p] += dup[d];
                eb[p] += eb[d];
                ef[p] += ef[d];
            }
        }
        let stats = (0..nd)
            .map(|d| DirStats {
                bytes: bytes[d],
                files: files[d],
                dup_files: dup[d],
                elsewhere_bytes: eb[d].max(0) as u64,
                elsewhere_files: ef[d].max(0) as u32,
            })
            .collect();
        Rollup { stats }
    }

    // ------------------------------------------------------------------ listings

    /// Children of a node, filtered and sorted. Folders without matching files are omitted.
    /// Folder rows of a node, filtered and sorted, without touching the database.
    /// Returns the node's own statistics too.
    pub fn list_dirs(&self, node: Node, opts: &ListOptions) -> (DirStats, Vec<DirRow>) {
        let roll = self.rollup(&opts.filter);
        let row = |c: DirIx, name: String| -> DirRow {
            let st = roll.stats[c as usize];
            let kids = self.dir_children(c);
            let in_kids: u32 = kids.iter().map(|&g| roll.stats[g as usize].files).sum();
            DirRow {
                dir: c,
                name,
                stats: st,
                has_subdirs: kids.iter().any(|&g| roll.stats[g as usize].files > 0),
                direct_files: st.files.saturating_sub(in_kids),
            }
        };
        let mut dirs = Vec::new();
        let stats = match node {
            Node::AllSources => {
                let mut total = DirStats::default();
                for s in &self.sources {
                    let st = roll.stats[s.root as usize];
                    total.bytes += st.bytes;
                    total.files += st.files;
                    total.dup_files += st.dup_files;
                    if st.files > 0 {
                        dirs.push(row(s.root, s.host.clone()));
                    }
                }
                total
            }
            Node::Dir(d) => {
                if d as usize >= self.dir_db.len() {
                    return (DirStats::default(), Vec::new());
                }
                for &c in self.dir_children(d) {
                    if roll.stats[c as usize].files > 0 {
                        dirs.push(row(c, self.dir_name[c as usize].to_string()));
                    }
                }
                roll.stats[d as usize]
            }
        };
        dirs.sort_by(|a, b| dir_order(a, b, opts));
        (stats, dirs)
    }

    /// Children of a node, filtered and sorted. Folders without matching files are omitted.
    pub fn list(&self, db: &Db, node: Node, opts: &ListOptions) -> Result<Listing> {
        if let Node::Dir(d) = node {
            if d as usize >= self.dir_db.len() {
                return err(format!("folder {d} does not exist"));
            }
        }
        let cf = self.compile(&opts.filter);
        let (stats, dirs) = self.list_dirs(node, opts);
        let mut files = Vec::new();
        match node {
            Node::AllSources => {}
            Node::Dir(d) => {
                let range = self.dir_files(d);
                if !range.is_empty() {
                    let names = db.visible_file_names(self.dir_db[d as usize])?;
                    let mut ni = 0usize;
                    for f in range {
                        if !cf.test(self, f) {
                            continue;
                        }
                        let id = self.file_db[f as usize];
                        while ni < names.len() && names[ni].0 < id {
                            ni += 1;
                        }
                        let name =
                            if ni < names.len() && names[ni].0 == id { names[ni].1.clone() } else { String::from("?") };
                        let c = self.file_content[f as usize];
                        files.push(FileRow {
                            file: f,
                            db_id: id,
                            name,
                            size: self.file_size[f as usize],
                            content: c,
                            copies: self.copies(c),
                            copies_other_sources: self.copies_other_sources(f),
                            flagged: self.hot[c as usize].algo != ALGO_SHA256 as u16,
                        });
                    }
                }
            }
        }
        files.sort_by(|a, b| file_order(a, b, opts));
        Ok(Listing { node, stats, dirs, files })
    }

    /// Every copy of a content with its full path.
    pub fn locations(&self, db: &Db, c: ContentIx) -> Result<Vec<Location>> {
        let mut out = Vec::new();
        for &f in self.content_files(c) {
            let rec = db.file_record(self.file_db[f as usize])?;
            let s = self.file_source(f);
            out.push(Location {
                file: f,
                db_id: rec.id,
                source: s,
                host: self.sources[s as usize].host.clone(),
                path: self.path_with_name(self.file_dir[f as usize], &rec.name),
                size: self.file_size[f as usize],
            });
        }
        Ok(out)
    }

    /// Full path of one file.
    pub fn file_path(&self, db: &Db, f: FileIx) -> Result<String> {
        let rec = db.file_record(self.file_db[f as usize])?;
        Ok(self.path_with_name(self.file_dir[f as usize], &rec.name))
    }

    /// Folders to highlight when a content is selected: every folder on the way to a copy.
    pub fn highlight_dirs(&self, c: ContentIx) -> HashSet<DirIx> {
        let mut set = HashSet::new();
        for &f in self.content_files(c) {
            let mut d = self.file_dir[f as usize];
            loop {
                if !set.insert(d) {
                    break;
                }
                let p = self.dir_parent[d as usize];
                if p == NONE {
                    break;
                }
                d = p;
            }
        }
        set
    }

    /// Duplicate groups in the query's order (by default most wasted space first). Returns
    /// (total matching groups, page).
    pub fn dup_groups(&self, q: &DupQuery, offset: usize, limit: usize) -> (usize, Vec<DupGroup>) {
        let group = |c: u32| {
            let copies = self.copies(c);
            let size = self.content_size[c as usize];
            DupGroup {
                content: c,
                size,
                copies,
                wasted: size.saturating_mul(copies as u64 - 1),
                sources: self.content_sources(c).len() as u32,
                flagged: self.hot[c as usize].algo != ALGO_SHA256 as u16,
            }
        };
        if q.sort == GroupSort::Extra && !q.ascending {
            // the precomputed order: no sorting, no allocation beyond the page
            let pass = self.dup_pass(q);
            let mut total = 0usize;
            let mut page = Vec::new();
            for &c in &self.dup_order {
                if !pass(c) {
                    continue;
                }
                if total >= offset && page.len() < limit {
                    page.push(group(c));
                }
                total += 1;
            }
            return (total, page);
        }
        let order = self.group_order(q);
        let page = order.iter().skip(offset).take(limit).map(|&c| group(c)).collect();
        (order.len(), page)
    }

    /// The groups matching `q`, sorted by its key; cached for the last few queries.
    fn group_order(&self, q: &DupQuery) -> Arc<Vec<u32>> {
        if let Ok(cache) = self.group_orders.lock() {
            if let Some((_, o)) = cache.iter().find(|(k, _)| k == q) {
                return o.clone();
            }
        }
        let pass = self.dup_pass(q);
        // start from the default order, so a stable sort breaks ties by wasted space
        let mut order: Vec<u32> = self.dup_order.iter().copied().filter(|&c| pass(c)).collect();
        let key = |c: u32| -> u64 {
            match q.sort {
                GroupSort::Extra => self.content_size[c as usize].saturating_mul(self.copies(c) as u64 - 1),
                GroupSort::Each => self.content_size[c as usize],
                GroupSort::Copies => self.copies(c) as u64,
                GroupSort::Sources => self.hot[c as usize].src_len as u64,
            }
        };
        if q.ascending {
            order.sort_by_key(|&c| key(c));
        } else {
            order.sort_by_key(|&c| std::cmp::Reverse(key(c)));
        }
        let order = Arc::new(order);
        if let Ok(mut cache) = self.group_orders.lock() {
            if cache.len() >= 4 {
                cache.remove(0);
            }
            cache.push((q.clone(), order.clone()));
        }
        order
    }

    /// Number of duplicate groups matching a query, and the bytes in their extra copies.
    pub fn dup_totals(&self, q: &DupQuery) -> (usize, u64) {
        let pass = self.dup_pass(q);
        let mut n = 0usize;
        let mut wasted = 0u64;
        for &c in &self.dup_order {
            if pass(c) {
                n += 1;
                wasted += self.content_size[c as usize].saturating_mul(self.copies(c) as u64 - 1);
            }
        }
        (n, wasted)
    }

    fn dup_pass<'a>(&'a self, q: &'a DupQuery) -> impl Fn(u32) -> bool + 'a {
        let inv = q.involving.map(|id| self.source_ix(id));
        move |c: u32| -> bool {
            let h = &self.hot[c as usize];
            let algo_ok = match q.hash {
                HashFilter::Any => true,
                HashFilter::Sha256 => h.algo == ALGO_SHA256 as u16,
                HashFilter::Flagged => h.algo != ALGO_SHA256 as u16,
            };
            let inv_ok = match inv {
                None => true,
                Some(None) => false,
                Some(Some(s)) => self.copies_in_source(c, s) > 0,
            };
            let spread_ok = match q.spread {
                Spread::Any => true,
                Spread::OneSource => h.src_len == 1,
                Spread::SeveralSources => h.src_len >= 2,
            };
            algo_ok && inv_ok && spread_ok && h.copies >= q.min_copies.max(2)
        }
    }

    /// Which sources hold copies of a folder's files outside the folder (the inspector's
    /// "where are the copies" view), and where in each source they mostly are. Unfiltered: every
    /// file under the folder counts.
    pub fn copies_outside(&self, d: DirIx) -> OutsideCopies {
        let mut out = OutsideCopies::default();
        if d as usize >= self.dir_db.len() {
            return out;
        }
        let end = self.dir_end[d as usize];
        let inside = |x: u32| x >= d && x < end;
        let mut per = vec![(0u64, 0u64); self.sources.len()];
        // files of this folder that have a copy outside it, by content (a content's outside
        // copies are the same for all its files here, so each content is walked once)
        let mut shared: Vec<ContentIx> = Vec::new();
        for f in self.subtree_files(d) {
            let size = self.file_size[f as usize];
            out.files += 1;
            out.bytes += size;
            let c = self.file_content[f as usize];
            let h = &self.hot[c as usize];
            // all copies in one source and their common folder inside this one: nothing outside
            let all_inside = h.lca != NONE && inside(h.lca);
            if h.copies > 1 && !all_inside {
                shared.push(c);
            } else {
                out.alone_files += 1;
                out.alone_bytes += size;
            }
        }
        shared.sort_unstable();
        let mut marked_by = vec![u32::MAX; self.sources.len()];
        // bytes of this folder's files with at least one outside copy under each folder
        let mut cover = vec![0u64; self.dir_db.len()];
        let mut cover_mark = vec![u32::MAX; self.dir_db.len()];
        for run in shared.chunk_by(|a, b| a == b) {
            let c = run[0];
            let n = run.len() as u64;
            let bytes = self.content_size[c as usize] * n;
            for &g in self.content_files(c) {
                let gd = self.file_dir[g as usize];
                if inside(gd) {
                    continue;
                }
                let s = self.dir_source[gd as usize] as usize;
                if marked_by[s] != c {
                    marked_by[s] = c;
                    per[s].0 += n;
                    per[s].1 += bytes;
                }
                // credit the copy's folder and its ancestors, each once per content: stop at the
                // first one an earlier copy already credited
                let mut a = gd;
                while a != NONE && cover_mark[a as usize] != c {
                    cover_mark[a as usize] = c;
                    cover[a as usize] += bytes;
                    a = self.dir_parent[a as usize];
                }
            }
        }
        out.by_source =
            per.into_iter().enumerate().filter(|(_, p)| p.0 > 0).map(|(s, p)| (s as SourceIx, p.0, p.1)).collect();
        out.by_source.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));
        out.main_dir = out
            .by_source
            .iter()
            .map(|&(s, _, bytes)| {
                let need = (bytes as f64 * MAIN_SHARE).ceil() as u64;
                let mut cur = self.sources[s as usize].root;
                loop {
                    // the child holding the most, if it holds enough (ties: tree order)
                    let best = self
                        .dir_children(cur)
                        .iter()
                        .copied()
                        .filter(|&k| cover[k as usize] >= need && cover[k as usize] > 0)
                        .max_by(|&a, &b| cover[a as usize].cmp(&cover[b as usize]).then(b.cmp(&a)));
                    match best {
                        Some(k) => cur = k,
                        None => break,
                    }
                }
                (s, cur, cover[cur as usize])
            })
            .collect();
        out
    }

    /// Rough heap usage of the index in bytes.
    pub fn approx_memory(&self) -> usize {
        let v = |n: usize, s: usize| n * s;
        let names: usize = self.dir_name.iter().map(|n| n.len() + 16).sum();
        v(self.dir_db.len(), 8 + 4 + 2 + 2 + 4 + 4 + 4)
            + names
            + v(self.children.len(), 4)
            + v(self.file_db.len(), 8 + 4 + 8 + 4 + 4 + 4)
            + v(self.hot.len(), std::mem::size_of::<ContentHot>() + 8 + 8 + 4)
            + v(self.content_src.len(), 8)
            + v(self.dup_order.len(), 4)
            + self.cache.lock().map(|c| c.len() * self.dir_db.len() * std::mem::size_of::<DirStats>()).unwrap_or(0)
    }
}

struct Compiled {
    copies: CopiesFilter,
    hash: HashFilter,
    /// None = no filter; Some(None) = the source no longer exists.
    absent_from: Option<Option<SourceIx>>,
    present_in: Option<Option<SourceIx>>,
}

impl Compiled {
    #[inline]
    fn test(&self, ix: &Index, f: FileIx) -> bool {
        let c = ix.file_content[f as usize];
        match self.hash {
            HashFilter::Any => {}
            HashFilter::Sha256 => {
                if ix.hot[c as usize].algo != ALGO_SHA256 as u16 {
                    return false;
                }
            }
            HashFilter::Flagged => {
                if ix.hot[c as usize].algo == ALGO_SHA256 as u16 {
                    return false;
                }
            }
        }
        let copies = ix.copies(c);
        let ok = match self.copies {
            CopiesFilter::Any => true,
            CopiesFilter::Unique => copies == 1,
            CopiesFilter::Duplicated => copies >= 2,
            CopiesFilter::InOtherSources => ix.copies_other_sources(f) > 0,
            CopiesFilter::OnlyThisSource => ix.copies_other_sources(f) == 0,
        };
        if !ok {
            return false;
        }
        // absent from a source that no longer exists: always true
        if let Some(Some(s)) = self.absent_from {
            if ix.copies_in_source(c, s) > 0 {
                return false;
            }
        }
        if let Some(s) = self.present_in {
            match s {
                None => return false,
                Some(s) => {
                    if ix.copies_in_source(c, s) == 0 {
                        return false;
                    }
                }
            }
        }
        true
    }
}

fn lca(parent: &[u32], depth: &[u16], mut a: u32, mut b: u32) -> u32 {
    while depth[a as usize] > depth[b as usize] {
        a = parent[a as usize];
    }
    while depth[b as usize] > depth[a as usize] {
        b = parent[b as usize];
    }
    while a != b {
        a = parent[a as usize];
        b = parent[b as usize];
        if a == NONE || b == NONE {
            return NONE;
        }
    }
    a
}

/// Share of a folder's bytes that also exist outside it, compared exactly (no rounding).
fn cmp_share(a: &DirStats, b: &DirStats) -> Ordering {
    let x = a.elsewhere_bytes as u128 * b.bytes.max(1) as u128;
    let y = b.elsewhere_bytes as u128 * a.bytes.max(1) as u128;
    x.cmp(&y)
}

fn dir_order(a: &DirRow, b: &DirRow, opts: &ListOptions) -> Ordering {
    let (sa, sb) = (&a.stats, &b.stats);
    let o = match opts.sort {
        SortKey::Name => natural_cmp(&a.name, &b.name),
        SortKey::Size => sa.bytes.cmp(&sb.bytes),
        SortKey::Files => sa.files.cmp(&sb.files).then(sa.bytes.cmp(&sb.bytes)),
        SortKey::Copies => sa.dup_files.cmp(&sb.dup_files).then(sa.bytes.cmp(&sb.bytes)),
        SortKey::Elsewhere => cmp_share(sa, sb).then(sa.elsewhere_bytes.cmp(&sb.elsewhere_bytes)),
    };
    let o = if opts.descending { o.reverse() } else { o };
    o.then_with(|| natural_cmp(&a.name, &b.name)).then(a.dir.cmp(&b.dir))
}

fn file_order(a: &FileRow, b: &FileRow, opts: &ListOptions) -> Ordering {
    let o = match opts.sort {
        SortKey::Name => natural_cmp(&a.name, &b.name),
        // every file counts as one: the biggest first among equals
        SortKey::Size | SortKey::Files => a.size.cmp(&b.size),
        SortKey::Copies => a.copies.cmp(&b.copies).then(a.size.cmp(&b.size)),
        SortKey::Elsewhere => a.copies_other_sources.cmp(&b.copies_other_sources).then(a.size.cmp(&b.size)),
    };
    let o = if opts.descending { o.reverse() } else { o };
    o.then_with(|| natural_cmp(&a.name, &b.name)).then(a.file.cmp(&b.file))
}

/// One entry of a listing when folders and files are shown mixed (folders_first = false).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entry {
    Dir(usize),
    File(usize),
}

impl Listing {
    /// Display order of all entries, honouring `folders_first`.
    pub fn entries(&self, opts: &ListOptions) -> Vec<Entry> {
        let mut out: Vec<Entry> = Vec::with_capacity(self.dirs.len() + self.files.len());
        if opts.folders_first {
            out.extend((0..self.dirs.len()).map(Entry::Dir));
            out.extend((0..self.files.len()).map(Entry::File));
            return out;
        }
        let (mut i, mut j) = (0, 0);
        while i < self.dirs.len() || j < self.files.len() {
            let take_dir = if i >= self.dirs.len() {
                false
            } else if j >= self.files.len() {
                true
            } else {
                let (d, f) = (&self.dirs[i], &self.files[j]);
                let st = &d.stats;
                let o = match opts.sort {
                    SortKey::Name => natural_cmp(&d.name, &f.name),
                    SortKey::Size => st.bytes.cmp(&f.size),
                    SortKey::Files => st.files.cmp(&1).then(st.bytes.cmp(&f.size)),
                    SortKey::Copies => st.dup_files.cmp(&((f.copies > 1) as u32)).then(st.bytes.cmp(&f.size)),
                    // a file is all or nothing elsewhere
                    SortKey::Elsewhere => {
                        let fs = DirStats {
                            bytes: f.size,
                            elsewhere_bytes: if f.copies_other_sources > 0 { f.size } else { 0 },
                            ..Default::default()
                        };
                        cmp_share(st, &fs)
                    }
                };
                let o = if opts.descending { o.reverse() } else { o };
                // on a tie the folder goes first
                o.then_with(|| natural_cmp(&d.name, &f.name)) != Ordering::Greater
            };
            if take_dir {
                out.push(Entry::Dir(i));
                i += 1;
            } else {
                out.push(Entry::File(j));
                j += 1;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::HEADER;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const C: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    /// SHA-256 whose 32 raw bytes equal the ASCII text of the MD5 value below.
    const S_AS_MD5: &str = "3031323334353637383961626364656630313233343536373839616263646566";
    const MD5: &str = "md5:0123456789abcdef0123456789abcdef";

    fn fixture() -> (Db, Index) {
        let mut db = Db::open_in_memory().unwrap();
        let pc = format!(
            "{HEADER}\r\n\
             {A}\t10\tPC1 D: (Data)\tD:\\Photos\\2019\\a.jpg\r\n\
             {B}\t20\tPC1 D: (Data)\tD:\\Photos\\2019\\b.jpg\r\n\
             {A}\t10\tPC1 D: (Data)\tD:\\Photos\\copy\\a.jpg\r\n\
             {C}\t30\tPC1 D: (Data)\tD:\\Other\\c.jpg\r\n"
        );
        let nas = format!(
            "{HEADER}\n\
             {B}\t20\tnas\t/mnt/tank/b.jpg\n\
             {MD5}\t5\tnas\t/mnt/tank/q.bin\n\
             {MD5}\t5\tnas\t/mnt/tank/q2.bin\n\
             {S_AS_MD5}\t5\tnas\t/mnt/tank/s.bin\n"
        );
        db.import_reader("pc.tsv", "pc.tsv", 0, None, pc.as_bytes(), &mut |_| true).unwrap();
        db.import_reader("nas.tsv", "nas.tsv", 0, None, nas.as_bytes(), &mut |_| true).unwrap();
        let ix = Index::build(&db).unwrap();
        (db, ix)
    }

    fn dir(ix: &Index, path: &str) -> DirIx {
        (0..ix.dir_count() as DirIx).find(|&d| ix.dir_path(d) == path).unwrap_or_else(|| panic!("no folder {path}"))
    }

    fn file(ix: &Index, db: &Db, path: &str) -> FileIx {
        (0..ix.file_count() as FileIx)
            .find(|&f| ix.file_path(db, f).unwrap() == path)
            .unwrap_or_else(|| panic!("no file {path}"))
    }

    fn stats(ix: &Index, filter: &Filter, path: &str) -> DirStats {
        ix.rollup(filter).stats[dir(ix, path) as usize]
    }

    #[test]
    fn copies_and_sources() {
        let (db, ix) = fixture();
        assert_eq!(ix.sources().iter().map(|s| s.host.as_str()).collect::<Vec<_>>(), ["nas", "PC1 D: (Data)"]);
        let a1 = file(&ix, &db, r"D:\Photos\2019\a.jpg");
        let b1 = file(&ix, &db, r"D:\Photos\2019\b.jpg");
        let q = file(&ix, &db, "/mnt/tank/q.bin");
        let s = file(&ix, &db, "/mnt/tank/s.bin");
        assert_eq!(ix.copies(ix.file_content(a1)), 2);
        assert_eq!(ix.copies_other_sources(a1), 0);
        assert_eq!(ix.copies_other_sources(b1), 1);
        assert_eq!(ix.copies(ix.file_content(q)), 2);
        assert!(ix.file_flagged(q));
        // a SHA-256 never matches a flagged hash, even with identical stored bytes and size
        assert_eq!(ix.copies(ix.file_content(s)), 1);
        assert!(!ix.file_flagged(s));
        assert_eq!(ix.content_lca(ix.file_content(a1)), Some(dir(&ix, r"D:\Photos")));
        assert_eq!(ix.content_lca(ix.file_content(b1)), None);
        let sum = &ix.source_summaries()[1];
        assert_eq!(
            (sum.files, sum.bytes, sum.dup_files, sum.bytes_in_other_sources, sum.only_here_files),
            (4, 70, 3, 20, 3)
        );
        let nas = &ix.source_summaries()[0];
        assert_eq!((nas.files, nas.flagged_files), (4, 2));
    }

    #[test]
    fn folder_statistics() {
        let (_db, ix) = fixture();
        let all = Filter::default();
        let s = stats(&ix, &all, r"D:\Photos\2019");
        assert_eq!((s.bytes, s.files, s.dup_files, s.elsewhere_bytes, s.elsewhere_files), (30, 2, 2, 30, 2));
        assert!(s.fully_elsewhere());
        let s = stats(&ix, &all, r"D:\Photos\copy");
        assert!(s.fully_elsewhere());
        let s = stats(&ix, &all, r"D:\Photos");
        assert_eq!((s.bytes, s.files, s.dup_files, s.elsewhere_bytes, s.elsewhere_files), (40, 3, 3, 20, 1));
        assert!(!s.fully_elsewhere());
        assert!((s.pct_elsewhere() - 50.0).abs() < 1e-9);
        let s = stats(&ix, &all, r"D:\Other");
        assert_eq!((s.bytes, s.elsewhere_bytes), (30, 0));
        let s = stats(&ix, &all, r"D:\");
        assert_eq!((s.bytes, s.files, s.dup_files, s.elsewhere_bytes), (70, 4, 3, 20));
        let s = stats(&ix, &all, "/mnt/tank");
        assert_eq!((s.bytes, s.files, s.dup_files, s.elsewhere_bytes, s.elsewhere_files), (35, 4, 3, 20, 1));
        let s = stats(&ix, &all, "PC1 D: (Data)");
        assert_eq!((s.bytes, s.files), (70, 4));
    }

    #[test]
    fn files_without_a_size_take_it_from_a_copy() {
        let (mut db, _) = fixture();
        // a checksum list: a.jpg's content (A, 10 bytes on the PC), and a file listed nowhere else
        let how = crate::db::ImportAs::Checksums(crate::db::ChecksumImport {
            host: "onedrive:".into(),
            base: "onedrive:".into(),
            sizes: None,
        });
        let d = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
        let body = format!(
            "{A}  Pics/a copy.jpg\n{d}  Pics/new.jpg\n{d}  Pics/new again.jpg\n0123456789abcdef0123456789abcdef  Pics/q.bin\n"
        );
        db.import_reader_as("od.txt", "od.txt", 0, None, body.as_bytes(), &how, &mut |_| true).unwrap();
        let ix = Index::build(&db).unwrap();
        let a = file(&ix, &db, "onedrive:/Pics/a copy.jpg");
        assert_eq!(ix.size_state(a), SizeState::FromCopy);
        assert_eq!(ix.file_size(a), 10);
        assert_eq!(ix.copies(ix.file_content(a)), 3, "two on the PC and this one");
        let n = file(&ix, &db, "onedrive:/Pics/new.jpg");
        assert_eq!(ix.size_state(n), SizeState::Unknown);
        assert_eq!(ix.file_size(n), 0);
        assert_eq!(ix.copies(ix.file_content(n)), 2, "two unknown-size files with one hash are copies");
        assert!(ix.content_size_unknown(ix.file_content(n)));
        assert!(!ix.content_size_unknown(ix.file_content(a)));
        // a flagged hash also takes the size of a listed copy (q.bin: md5, 5 bytes on the NAS)
        let q = file(&ix, &db, "onedrive:/Pics/q.bin");
        assert_eq!((ix.size_state(q), ix.file_size(q)), (SizeState::FromCopy, 5));
        assert_eq!(ix.unknown_size_files().len(), 2);
        // a file with a size keeps it
        let pc = file(&ix, &db, r"D:\Photos\2019\a.jpg");
        assert_eq!(ix.size_state(pc), SizeState::Known);
        // folder totals: unknown sizes count as files with 0 bytes
        let od = dir(&ix, "onedrive:/Pics");
        let st = ix.rollup(&Filter::default()).stats[od as usize];
        assert_eq!((st.files, st.bytes), (4, 15));
    }

    #[test]
    fn database_ids_find_folders_and_files_again() {
        let (_db, ix) = fixture();
        for d in 0..ix.dir_count() as DirIx {
            assert_eq!(ix.dir_by_db_id(ix.dir_db_id(d)), Some(d));
        }
        for f in 0..ix.file_count() as FileIx {
            assert_eq!(ix.file_by_db_id(ix.file_db_id(f)), Some(f));
        }
        assert_eq!(ix.dir_by_db_id(-5), None);
        assert_eq!(ix.file_by_db_id(-5), None);
    }

    #[test]
    fn filters() {
        let (db, ix) = fixture();
        let count = |f: Filter| -> Vec<String> {
            let mut v: Vec<String> = (0..ix.file_count() as FileIx)
                .filter(|&x| ix.matches(&f, x))
                .map(|x| ix.file_path(&db, x).unwrap())
                .collect();
            v.sort();
            v
        };
        let nas = ix.sources()[0].db_id;
        assert_eq!(
            count(Filter { copies: CopiesFilter::Unique, ..Default::default() }),
            ["/mnt/tank/s.bin", r"D:\Other\c.jpg"]
        );
        assert_eq!(
            count(Filter { hash: HashFilter::Flagged, ..Default::default() }),
            ["/mnt/tank/q.bin", "/mnt/tank/q2.bin"]
        );
        assert_eq!(
            count(Filter { copies: CopiesFilter::InOtherSources, ..Default::default() }),
            ["/mnt/tank/b.jpg", r"D:\Photos\2019\b.jpg"]
        );
        assert_eq!(count(Filter { copies: CopiesFilter::OnlyThisSource, ..Default::default() }).len(), 6);
        assert_eq!(
            count(Filter { absent_from: Some(nas), ..Default::default() }),
            [r"D:\Other\c.jpg", r"D:\Photos\2019\a.jpg", r"D:\Photos\copy\a.jpg"]
        );
        assert_eq!(count(Filter { present_in: Some(nas), ..Default::default() }).len(), 5);
        // a source that no longer exists
        assert_eq!(count(Filter { present_in: Some(999), ..Default::default() }).len(), 0);
        assert_eq!(count(Filter { absent_from: Some(999), ..Default::default() }).len(), 8);
        // filtered rollup: only unique files
        let s = stats(&ix, &Filter { copies: CopiesFilter::Unique, ..Default::default() }, r"D:\");
        assert_eq!((s.bytes, s.files), (30, 1));
    }

    #[test]
    fn listings_sort_and_hide_empty_folders() {
        let (db, ix) = fixture();
        let opts = ListOptions::default();
        let root = ix.list(&db, Node::AllSources, &opts).unwrap();
        assert_eq!(root.dirs.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["nas", "PC1 D: (Data)"]);
        assert_eq!(root.stats.files, 8);
        let y = ix.list(&db, Node::Dir(dir(&ix, r"D:\Photos\2019")), &opts).unwrap();
        assert_eq!(y.files.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), ["a.jpg", "b.jpg"]);
        assert_eq!(y.files[1].copies_other_sources, 1);
        let by_size = ListOptions { sort: SortKey::Size, descending: true, ..Default::default() };
        let y = ix.list(&db, Node::Dir(dir(&ix, r"D:\Photos\2019")), &by_size).unwrap();
        assert_eq!(y.files.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), ["b.jpg", "a.jpg"]);
        let d = ix.list(&db, Node::Dir(dir(&ix, r"D:\")), &by_size).unwrap();
        assert_eq!(d.dirs.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["Photos", "Other"]);
        // with a filter, folders without matching files disappear
        let uniq =
            ListOptions { filter: Filter { copies: CopiesFilter::Unique, ..Default::default() }, ..Default::default() };
        let d = ix.list(&db, Node::Dir(dir(&ix, r"D:\")), &uniq).unwrap();
        assert_eq!(d.dirs.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["Other"]);
        let mixed = ListOptions { folders_first: false, ..Default::default() };
        let t = ix.list(&db, Node::Dir(dir(&ix, "/mnt/tank")), &mixed).unwrap();
        assert_eq!(t.entries(&mixed).len(), 4);
    }

    #[test]
    fn listings_sort_by_every_column() {
        let (db, ix) = fixture();
        let names = |node: Node, sort: SortKey, descending: bool| -> (Vec<String>, Vec<String>) {
            let l = ix.list(&db, node, &ListOptions { sort, descending, ..Default::default() }).unwrap();
            (l.dirs.into_iter().map(|d| d.name).collect(), l.files.into_iter().map(|f| f.name).collect())
        };
        let d = Node::Dir(dir(&ix, r"D:\"));
        // Photos: 3 files, 3 with copies, half its bytes also on the NAS; Other: 1 unique file
        assert_eq!(names(d, SortKey::Files, true).0, ["Photos", "Other"]);
        assert_eq!(names(d, SortKey::Copies, true).0, ["Photos", "Other"]);
        assert_eq!(names(d, SortKey::Elsewhere, true).0, ["Photos", "Other"]);
        assert_eq!(names(d, SortKey::Elsewhere, false).0, ["Other", "Photos"]);
        let y = Node::Dir(dir(&ix, r"D:\Photos\2019"));
        // equal copies: the bigger file first; elsewhere: b.jpg has a copy on the NAS
        assert_eq!(names(y, SortKey::Copies, true).1, ["b.jpg", "a.jpg"]);
        assert_eq!(names(y, SortKey::Elsewhere, true).1, ["b.jpg", "a.jpg"]);
        assert_eq!(names(y, SortKey::Elsewhere, false).1, ["a.jpg", "b.jpg"]);
        let t = Node::Dir(dir(&ix, "/mnt/tank"));
        assert_eq!(names(t, SortKey::Copies, false).1, ["s.bin", "q.bin", "q2.bin", "b.jpg"]);
        // mixed folders and files follow the same keys
        let mixed =
            ListOptions { sort: SortKey::Elsewhere, descending: true, folders_first: false, ..Default::default() };
        let l = ix.list(&db, d, &mixed).unwrap();
        assert_eq!(l.entries(&mixed).len(), 2);
    }

    #[test]
    fn inspector_highlight_and_groups() {
        let (db, ix) = fixture();
        let a1 = file(&ix, &db, r"D:\Photos\2019\a.jpg");
        let locs = ix.locations(&db, ix.file_content(a1)).unwrap();
        assert_eq!(
            locs.iter().map(|l| l.path.as_str()).collect::<Vec<_>>(),
            [r"D:\Photos\2019\a.jpg", r"D:\Photos\copy\a.jpg"]
        );
        assert!(locs.iter().all(|l| l.host == "PC1 D: (Data)"));
        let hl = ix.highlight_dirs(ix.file_content(a1));
        for p in [r"D:\Photos\2019", r"D:\Photos\copy", r"D:\Photos", r"D:\", "PC1 D: (Data)"] {
            assert!(hl.contains(&dir(&ix, p)), "{p}");
        }
        assert!(!hl.contains(&dir(&ix, r"D:\Other")));
        let (total, groups) = ix.dup_groups(&DupQuery::default(), 0, 10);
        assert_eq!(total, 3);
        assert_eq!(
            groups.iter().map(|g| (g.size, g.copies, g.wasted, g.sources)).collect::<Vec<_>>(),
            [(20, 2, 20, 2), (10, 2, 10, 1), (5, 2, 5, 1)]
        );
        assert!(groups[2].flagged);
        let nas = ix.sources()[0].db_id;
        let (t, g) = ix.dup_groups(&DupQuery { involving: Some(nas), ..Default::default() }, 0, 10);
        assert_eq!((t, g.len()), (2, 2));
        let (t, _) = ix.dup_groups(&DupQuery { hash: HashFilter::Flagged, ..Default::default() }, 0, 10);
        assert_eq!(t, 1);
        let (t, page) = ix.dup_groups(&DupQuery::default(), 1, 1);
        assert_eq!((t, page[0].size), (3, 10));
    }

    #[test]
    fn empty_database() {
        let db = Db::open_in_memory().unwrap();
        let ix = Index::build(&db).unwrap();
        assert_eq!(ix.file_count(), 0);
        let l = ix.list(&db, Node::AllSources, &ListOptions::default()).unwrap();
        assert!(l.dirs.is_empty());
        assert_eq!(ix.dup_groups(&DupQuery::default(), 0, 10).0, 0);
    }
}
