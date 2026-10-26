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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DupQuery {
    /// Only groups with a copy in this source (database id).
    pub involving: Option<i64>,
    pub hash: HashFilter,
    pub min_copies: u32,
}

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
    // contents, numbered in order of first appearance in the tree (cache locality)
    hot: Vec<ContentHot>,
    content_size: Vec<u64>,
    content_file_start: Vec<u32>,
    content_files: Vec<u32>,
    content_src: Vec<(SourceIx, u32)>,
    dup_order: Vec<u32>,
    algos: Vec<String>,
    exts: Vec<String>,
    summaries: Vec<SourceSummary>,
    cache: Mutex<Vec<(Filter, Arc<Rollup>)>>,
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
                        e.bytes += r.get::<_, i64>(2)? as u64;
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
                r_size.push(r.get::<_, i64>(2)? as u64);
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
                let mut k = 0;
                while k < run.len() {
                    let first = run[k].1 as usize;
                    let cid = content_algo.len() as u32;
                    content_algo.push(ALGO_SHA256 as u16);
                    content_size.push(r_size[first]);
                    let mut m = k;
                    while m < run.len() {
                        let x = run[m].1 as usize;
                        if r_digest[x] != r_digest[first] || r_size[x] != r_size[first] {
                            break;
                        }
                        r_content[x] = cid;
                        m += 1;
                    }
                    k = m;
                }
                i = j;
            }
            drop(keys);
            let mut other_map: HashMap<(u16, Vec<u8>, u64), u32> = HashMap::new();
            for (idx, dig) in other_digest.drain(..) {
                let i = idx as usize;
                let key = (r_algo[i], dig, r_size[i]);
                let cid = *other_map.entry(key).or_insert_with(|| {
                    let cid = content_algo.len() as u32;
                    content_algo.push(r_algo[i]);
                    content_size.push(r_size[i]);
                    cid
                });
                r_content[i] = cid;
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
        let mut file_content: Vec<u32> = perm.iter().map(|&r| r_content[r as usize]).collect();
        drop(r_content);
        // Renumber contents by first appearance in tree order. Backups copy whole folders, so
        // the copies then also have increasing ids, which keeps rollups cache-friendly.
        let (content_algo, content_size) = {
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
            for old in 0..nc {
                let n = remap[old] as usize;
                algo2[n] = content_algo[old];
                size2[n] = content_size[old];
            }
            (algo2, size2)
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
            hot,
            content_size,
            content_file_start,
            content_files,
            content_src,
            dup_order,
            algos,
            exts,
            summaries: Vec::new(),
            cache: Mutex::new(Vec::new()),
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
        // dir_db is not sorted; a linear scan is fine for occasional lookups (session restore)
        self.dir_db.iter().position(|&x| x == db_id).map(|p| p as DirIx)
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

    pub fn file_flagged(&self, f: FileIx) -> bool {
        self.hot[self.file_content[f as usize] as usize].algo != ALGO_SHA256 as u16
    }

    /// Copies of a content anywhere.
    pub fn copies(&self, c: ContentIx) -> u32 {
        self.hot[c as usize].copies
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

    /// Duplicate groups, most wasted space first. Returns (total matching groups, page).
    pub fn dup_groups(&self, q: &DupQuery, offset: usize, limit: usize) -> (usize, Vec<DupGroup>) {
        let inv = q.involving.map(|id| self.source_ix(id));
        let pass = |c: u32| -> bool {
            let algo_ok = match q.hash {
                HashFilter::Any => true,
                HashFilter::Sha256 => self.hot[c as usize].algo == ALGO_SHA256 as u16,
                HashFilter::Flagged => self.hot[c as usize].algo != ALGO_SHA256 as u16,
            };
            let inv_ok = match inv {
                None => true,
                Some(None) => false,
                Some(Some(s)) => self.copies_in_source(c, s) > 0,
            };
            algo_ok && inv_ok && self.copies(c) >= q.min_copies.max(2)
        };
        let mut total = 0usize;
        let mut page = Vec::new();
        for &c in &self.dup_order {
            if !pass(c) {
                continue;
            }
            if total >= offset && page.len() < limit {
                let copies = self.copies(c);
                let size = self.content_size[c as usize];
                page.push(DupGroup {
                    content: c,
                    size,
                    copies,
                    wasted: size.saturating_mul(copies as u64 - 1),
                    sources: self.content_sources(c).len() as u32,
                    flagged: self.hot[c as usize].algo != ALGO_SHA256 as u16,
                });
            }
            total += 1;
        }
        (total, page)
    }

    /// Rough heap usage of the index in bytes.
    pub fn approx_memory(&self) -> usize {
        let v = |n: usize, s: usize| n * s;
        let names: usize = self.dir_name.iter().map(|n| n.len() + 16).sum();
        v(self.dir_db.len(), 8 + 4 + 2 + 2 + 4 + 4 + 4)
            + names
            + v(self.children.len(), 4)
            + v(self.file_db.len(), 8 + 4 + 8 + 4 + 4 + 4)
            + v(self.hot.len(), std::mem::size_of::<ContentHot>() + 8 + 4)
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

fn dir_order(a: &DirRow, b: &DirRow, opts: &ListOptions) -> Ordering {
    let o = match opts.sort {
        SortKey::Name => natural_cmp(&a.name, &b.name),
        SortKey::Size => a.stats.bytes.cmp(&b.stats.bytes),
    };
    let o = if opts.descending { o.reverse() } else { o };
    o.then_with(|| natural_cmp(&a.name, &b.name)).then(a.dir.cmp(&b.dir))
}

fn file_order(a: &FileRow, b: &FileRow, opts: &ListOptions) -> Ordering {
    let o = match opts.sort {
        SortKey::Name => natural_cmp(&a.name, &b.name),
        SortKey::Size => a.size.cmp(&b.size),
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
                let o = match opts.sort {
                    SortKey::Name => natural_cmp(&d.name, &f.name),
                    SortKey::Size => d.stats.bytes.cmp(&f.size),
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
