//! A pane's folder tree, flattened into rows for a virtualised table.
//!
//! Folder rows come straight from the index (instant); the files of an expanded folder come
//! from the worker (they need names from the database) and show as a "loading" row until
//! they arrive.

use dupemap_core::index::{DirIx, FileRow, Index, ListOptions, Node};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// What a pane shows at its top level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Root {
    AllSources,
    /// One source, by database id (stable across index rebuilds).
    Source(i64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowKind {
    Dir {
        dir: DirIx,
        expandable: bool,
    },
    /// `index` into the file list of `dir`.
    File {
        dir: DirIx,
        index: usize,
    },
    Loading {
        dir: DirIx,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub kind: RowKind,
    pub depth: u16,
    /// Size of the parent folder (for "share of parent" bars); 0 at the top level.
    pub parent_bytes: u64,
}

/// The folder a root starts from (`None` for "all sources" or a source that is gone).
pub fn root_dir(ix: &Index, root: Root) -> Option<DirIx> {
    match root {
        Root::AllSources => None,
        Root::Source(id) => ix.source_ix(id).map(|s| ix.sources()[s as usize].root),
    }
}

/// Flatten the visible tree.
pub fn flatten(
    ix: &Index,
    root: Root,
    opts: &ListOptions,
    is_expanded: &dyn Fn(DirIx) -> bool,
    files: &dyn Fn(DirIx) -> Option<Arc<Vec<FileRow>>>,
) -> Vec<Row> {
    let mut out = Vec::new();
    match root {
        Root::AllSources => {
            let (stats, dirs) = ix.list_dirs(Node::AllSources, opts);
            for d in dirs {
                let expandable = d.has_subdirs || d.direct_files > 0;
                out.push(Row { kind: RowKind::Dir { dir: d.dir, expandable }, depth: 0, parent_bytes: stats.bytes });
                if expandable && is_expanded(d.dir) {
                    push_children(ix, d.dir, d.stats.bytes, d.direct_files, 1, opts, is_expanded, files, &mut out);
                }
            }
        }
        Root::Source(_) => {
            if let Some(r) = root_dir(ix, root) {
                let (stats, kids) = ix.list_dirs(Node::Dir(r), opts);
                let in_kids: u32 = kids.iter().map(|d| d.stats.files).sum();
                push_children(
                    ix,
                    r,
                    stats.bytes,
                    stats.files.saturating_sub(in_kids),
                    0,
                    opts,
                    is_expanded,
                    files,
                    &mut out,
                );
            }
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn push_children(
    ix: &Index,
    dir: DirIx,
    dir_bytes: u64,
    direct_files: u32,
    depth: u16,
    opts: &ListOptions,
    is_expanded: &dyn Fn(DirIx) -> bool,
    files: &dyn Fn(DirIx) -> Option<Arc<Vec<FileRow>>>,
    out: &mut Vec<Row>,
) {
    let (_, dirs) = ix.list_dirs(Node::Dir(dir), opts);
    for d in dirs {
        let expandable = d.has_subdirs || d.direct_files > 0;
        out.push(Row { kind: RowKind::Dir { dir: d.dir, expandable }, depth, parent_bytes: dir_bytes });
        if expandable && is_expanded(d.dir) {
            push_children(ix, d.dir, d.stats.bytes, d.direct_files, depth + 1, opts, is_expanded, files, out);
        }
    }
    if direct_files > 0 {
        match files(dir) {
            Some(list) => {
                for i in 0..list.len() {
                    out.push(Row { kind: RowKind::File { dir, index: i }, depth, parent_bytes: dir_bytes });
                }
            }
            None => out.push(Row { kind: RowKind::Loading { dir }, depth, parent_bytes: dir_bytes }),
        }
    }
}

/// Folders to expand when the user expands `dir`: the folder itself plus any chain of single
/// subfolders without files below it (`/` → `mnt` → `tank`), so one click reaches content.
pub fn expansion_chain(ix: &Index, dir: DirIx, opts: &ListOptions) -> Vec<DirIx> {
    let mut chain = vec![dir];
    let mut cur = dir;
    for _ in 0..64 {
        let (_, kids) = ix.list_dirs(Node::Dir(cur), opts);
        if kids.len() != 1 {
            break;
        }
        let own_files: u32 = {
            let (st, _) = ix.list_dirs(Node::Dir(cur), opts);
            st.files.saturating_sub(kids[0].stats.files)
        };
        if own_files > 0 {
            break;
        }
        cur = kids[0].dir;
        chain.push(cur);
    }
    chain
}

#[cfg(test)]
mod tests {
    use super::*;
    use dupemap_core::ignore::IgnoreSettings;
    use dupemap_core::index::{CopiesFilter, Filter, SortKey};
    use dupemap_core::manifest::HEADER;
    use dupemap_core::Db;
    use std::collections::HashSet;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn fixture() -> (Db, Index) {
        let mut db = Db::open_in_memory().unwrap();
        let t = format!(
            "{HEADER}\n{A}\t10\tPC\tD:\\Photos\\2019\\a.jpg\n{B}\t30\tPC\tD:\\Photos\\b.jpg\n{A}\t10\tnas\t/mnt/tank/x/a.jpg\n"
        );
        db.import_reader("m.tsv", "m.tsv", 0, None, t.as_bytes(), &mut |_| true).unwrap();
        let ix = Index::build_with(&db, &IgnoreSettings::disabled(), &mut |_, _| {}).unwrap();
        (db, ix)
    }

    fn names(ix: &Index, _db: &Db, rows: &[Row], files: &dyn Fn(DirIx) -> Option<Arc<Vec<FileRow>>>) -> Vec<String> {
        rows.iter()
            .map(|r| {
                let pad = "  ".repeat(r.depth as usize);
                match &r.kind {
                    RowKind::Dir { dir, .. } => {
                        if ix.dir_parent(*dir).is_none() {
                            format!("{pad}[{}]", ix.sources()[ix.dir_source(*dir) as usize].host)
                        } else {
                            format!("{pad}{}/", ix.dir_name(*dir))
                        }
                    }
                    RowKind::File { dir, index } => format!("{pad}{}", files(*dir).unwrap()[*index].name),
                    RowKind::Loading { .. } => format!("{pad}…"),
                }
            })
            .collect()
    }

    #[test]
    fn all_sources_collapsed_and_expanded() {
        let (db, ix) = fixture();
        let opts = ListOptions::default();
        let none = |_: DirIx| None;
        let rows = flatten(&ix, Root::AllSources, &opts, &|_| false, &none);
        assert_eq!(names(&ix, &db, &rows, &none), ["[nas]", "[PC]"]);
        // expand everything, files not loaded yet
        let rows = flatten(&ix, Root::AllSources, &opts, &|_| true, &none);
        assert_eq!(
            names(&ix, &db, &rows, &none),
            [
                "[nas]",
                "  //",
                "    mnt/",
                "      tank/",
                "        x/",
                "          …",
                "[PC]",
                "  D:/",
                "    Photos/",
                "      2019/",
                "        …",
                "      …"
            ]
        );
    }

    #[test]
    fn files_appear_once_loaded_and_follow_the_sort() {
        let (db, ix) = fixture();
        let opts = ListOptions { sort: SortKey::Size, descending: true, ..Default::default() };
        let pc = ix.sources().iter().find(|s| s.host == "PC").unwrap().db_id;
        let cache = |d: DirIx| -> Option<Arc<Vec<FileRow>>> {
            Some(Arc::new(ix.list(&db, Node::Dir(d), &opts).unwrap().files))
        };
        let rows = flatten(&ix, Root::Source(pc), &opts, &|_| true, &cache);
        // sorted by size, descending: Photos (40) holds 2019 (10) and b.jpg (30)
        assert_eq!(names(&ix, &db, &rows, &cache), ["D:/", "  Photos/", "    2019/", "      a.jpg", "    b.jpg"]);
        let photos = rows.iter().find_map(|r| match r.kind {
            RowKind::Dir { dir, .. } if ix.dir_name(dir) == "Photos" => Some(r.clone()),
            _ => None,
        });
        assert_eq!(photos.unwrap().parent_bytes, 40);
    }

    #[test]
    fn filters_hide_folders_without_matching_files() {
        let (db, ix) = fixture();
        let opts =
            ListOptions { filter: Filter { copies: CopiesFilter::Unique, ..Default::default() }, ..Default::default() };
        let cache = |d: DirIx| -> Option<Arc<Vec<FileRow>>> {
            Some(Arc::new(ix.list(&db, Node::Dir(d), &opts).unwrap().files))
        };
        let rows = flatten(&ix, Root::AllSources, &opts, &|_| true, &cache);
        assert_eq!(names(&ix, &db, &rows, &cache), ["[PC]", "  D:/", "    Photos/", "      b.jpg"]);
    }

    #[test]
    fn expanding_follows_single_folder_chains() {
        let (_db, ix) = fixture();
        let opts = ListOptions::default();
        let nas = ix.sources().iter().find(|s| s.host == "nas").unwrap();
        let chain: Vec<&str> = expansion_chain(&ix, nas.root, &opts).iter().map(|&d| ix.dir_name(d)).collect();
        assert_eq!(chain, ["", "/", "mnt", "tank", "x"]);
        let expanded: HashSet<DirIx> = expansion_chain(&ix, nas.root, &opts).into_iter().collect();
        let rows = flatten(&ix, Root::Source(nas.db_id), &opts, &|d| expanded.contains(&d), &|_| None);
        assert_eq!(rows.len(), 5, "/, mnt, tank, x and the loading row");
    }
}
