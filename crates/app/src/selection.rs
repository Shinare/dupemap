//! What is selected app-wide: it drives the inspector and the copy highlighting in every tab.

use dupemap_core::index::{ContentIx, DirIx, FileIx, Index, SourceIx};
use std::collections::HashSet;

/// A selected file or folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Target {
    File { file: FileIx, content: ContentIx },
    Dir(DirIx),
}

/// What every tab highlights for the current selection.
///
/// * a file: every copy of it (`content`; the selected row itself shows as selected), and every
///   folder on the way to one (`dirs`)
/// * a folder: the folders where copies of its files mostly are (`main_dirs`), and the folders on
///   the way to them (`dirs`)
#[derive(Debug, Clone, Default)]
pub struct Linked {
    pub content: Option<ContentIx>,
    pub main_dirs: HashSet<DirIx>,
    pub dirs: HashSet<DirIx>,
}

/// Is `d` specific enough to point at as "where the copies are"? A folder is; a drive only when
/// its source has several; the source itself never.
pub fn names_a_place(ix: &Index, d: DirIx) -> bool {
    match ix.dir_parent(d) {
        None => false,
        Some(p) if ix.dir_parent(p).is_none() => ix.dir_children(p).len() > 1,
        Some(_) => true,
    }
}

impl Linked {
    /// Only for files that have other copies; a unique file has nothing to link to.
    pub fn for_file(ix: &Index, file: FileIx) -> Option<Linked> {
        let content = ix.file_content(file);
        (ix.copies(content) > 1).then(|| Linked {
            content: Some(content),
            main_dirs: HashSet::new(),
            dirs: ix.highlight_dirs(content),
        })
    }

    /// For a folder: where its files' copies mostly are (see `Index::copies_outside`). A whole
    /// source (or its only drive) says nothing and is left out.
    pub fn for_dir(ix: &Index, main: &[(SourceIx, DirIx, u64)]) -> Option<Linked> {
        let mut l = Linked::default();
        for &(_, d, _) in main {
            if !names_a_place(ix, d) {
                continue;
            }
            l.main_dirs.insert(d);
            l.dirs.extend(ix.dir_ancestors(d));
        }
        (!l.main_dirs.is_empty()).then_some(l)
    }

    /// Strength of the highlight for a folder row: 2 = a main folder, 1 = on the way to one or to
    /// a copy, 0 = none.
    pub fn dir_level(&self, d: DirIx) -> u8 {
        if self.main_dirs.contains(&d) {
            2
        } else if self.dirs.contains(&d) {
            1
        } else {
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dupemap_core::ignore::IgnoreSettings;
    use dupemap_core::manifest::HEADER;
    use dupemap_core::Db;

    #[test]
    fn linked_marks_every_folder_on_the_way_to_a_copy() {
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        let c = "c".repeat(64);
        let text = format!(
            "{HEADER}\n{a}\t5\tPC\tD:\\x\\one.bin\n{a}\t5\tnas\t/mnt/y/two.bin\n{b}\t7\tPC\tD:\\z\\alone.bin\n\
             {c}\t9\tPC\tE:\\w\\three.bin\n"
        );
        let mut db = Db::open_in_memory().unwrap();
        db.import_reader("m.tsv", "m.tsv", 0, None, text.as_bytes(), &mut |_| true).unwrap();
        let ix = Index::build_with(&db, &IgnoreSettings::disabled(), &mut |_, _| {}).unwrap();
        let file_named = |want: &str| -> FileIx {
            (0..ix.file_count() as FileIx)
                .find(|&f| {
                    let p = ix.file_path(&db, f).unwrap();
                    p.ends_with(want)
                })
                .unwrap()
        };
        let l = Linked::for_file(&ix, file_named("one.bin")).unwrap();
        let names: HashSet<&str> = l.dirs.iter().map(|&d| ix.dir_name(d)).collect();
        for n in ["x", "D:", "y", "mnt", "/"] {
            assert!(names.contains(n), "{n} should be marked: {names:?}");
        }
        assert!(!names.contains("z"), "a folder without a copy is not marked");
        assert!(Linked::for_file(&ix, file_named("alone.bin")).is_none(), "unique files link nothing");

        // a folder's copies: the main folder is marked strongly, the way to it weakly
        let x = ix.file_dir(file_named("one.bin"));
        let out = ix.copies_outside(x);
        let l = Linked::for_dir(&ix, &out.main_dir).unwrap();
        let y = ix.file_dir(file_named("two.bin"));
        assert_eq!(ix.dir_name(y), "y");
        assert_eq!(l.dir_level(y), 2);
        let mnt = ix.dir_parent(y).unwrap();
        assert_eq!(l.dir_level(mnt), 1);
        assert_eq!(l.dir_level(x), 0, "the folder itself is not one of its copies' folders");
        // copies that only reach a source's top level are not worth highlighting
        assert!(Linked::for_dir(&ix, &[(0, ix.sources()[0].root, 5)]).is_none());
        // a drive names a place only when its source has more than one
        let d_drive = ix.dir_parent(x).unwrap();
        assert_eq!(ix.dir_name(d_drive), "D:");
        assert!(names_a_place(&ix, d_drive), "PC has D: and E:");
        assert!(!names_a_place(&ix, ix.dir_parent(mnt).unwrap()), "the NAS has only /");
        assert!(!names_a_place(&ix, ix.dir_parent(d_drive).unwrap()), "a whole source is no place");
        assert!(names_a_place(&ix, x) && names_a_place(&ix, mnt));
    }
}
