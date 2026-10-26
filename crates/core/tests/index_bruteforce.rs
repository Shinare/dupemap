//! Cross-check every folder statistic and copy count of the index against a slow brute-force
//! computation done directly on the database rows, for many filters and random datasets.

use dupemap_core::index::{CopiesFilter, DirIx, FileIx, Filter, HashFilter, Index, ListOptions, Node};
use dupemap_core::manifest::HEADER;
use dupemap_core::synth::{generate, Config, Rng};
use dupemap_core::Db;
use std::collections::{HashMap, HashSet};

type ContentKey = (i64, Vec<u8>, u64); // algo, digest, size

#[derive(Default)]
struct Brute {
    dir_parent: HashMap<i64, Option<i64>>,
    dir_source: HashMap<i64, i64>,
    files: Vec<(i64, i64, u64, ContentKey)>, // id, dir, size, content key
    groups: HashMap<ContentKey, Vec<usize>>,
    ancestors: HashMap<i64, Vec<i64>>,
}

impl Brute {
    fn load(db: &Db) -> Brute {
        let mut b = Brute::default();
        let c = db.conn();
        let mut st = c.prepare("SELECT id, parent_id, source_id FROM dir").unwrap();
        for r in
            st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?, r.get::<_, i64>(2)?))).unwrap()
        {
            let (id, p, s) = r.unwrap();
            b.dir_parent.insert(id, p);
            b.dir_source.insert(id, s);
        }
        let mut st = c.prepare("SELECT id, dir_id, size, algo_id, digest FROM file WHERE shadowed_by IS NULL").unwrap();
        for r in st
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)? as u64,
                    r.get::<_, i64>(3)?,
                    r.get::<_, Vec<u8>>(4)?,
                ))
            })
            .unwrap()
        {
            let (id, dir, size, algo, digest) = r.unwrap();
            let key = (algo, digest, size);
            b.groups.entry(key.clone()).or_default().push(b.files.len());
            b.files.push((id, dir, size, key));
        }
        let dirs: Vec<i64> = b.dir_parent.keys().copied().collect();
        for d in dirs {
            let mut v = vec![d];
            let mut cur = d;
            while let Some(Some(p)) = b.dir_parent.get(&cur) {
                v.push(*p);
                cur = *p;
            }
            b.ancestors.insert(d, v);
        }
        b
    }

    fn copies(&self, i: usize) -> usize {
        self.groups[&self.files[i].3].len()
    }

    fn copies_in_source(&self, i: usize, source: i64) -> usize {
        self.groups[&self.files[i].3].iter().filter(|&&j| self.dir_source[&self.files[j].1] == source).count()
    }

    fn passes(&self, f: &Filter, i: usize) -> bool {
        let (_, dir, _, key) = &self.files[i];
        let src = self.dir_source[dir];
        let copies = self.copies(i);
        let other = copies - self.copies_in_source(i, src);
        let h = match f.hash {
            HashFilter::Any => true,
            HashFilter::Sha256 => key.0 == 0,
            HashFilter::Flagged => key.0 != 0,
        };
        let c = match f.copies {
            CopiesFilter::Any => true,
            CopiesFilter::Unique => copies == 1,
            CopiesFilter::Duplicated => copies >= 2,
            CopiesFilter::InOtherSources => other > 0,
            CopiesFilter::OnlyThisSource => other == 0,
        };
        let a = f.absent_from.is_none_or(|s| self.copies_in_source(i, s) == 0);
        let p = f.present_in.is_none_or(|s| self.copies_in_source(i, s) > 0);
        h && c && a && p
    }

    /// (bytes, files, dup_files, elsewhere_bytes, elsewhere_files) per folder db id.
    fn stats(&self, f: &Filter) -> HashMap<i64, (u64, u32, u32, u64, u32)> {
        let mut out: HashMap<i64, (u64, u32, u32, u64, u32)> = HashMap::new();
        for d in self.dir_parent.keys() {
            out.insert(*d, (0, 0, 0, 0, 0));
        }
        let anc_sets: HashMap<i64, HashSet<i64>> =
            self.ancestors.iter().map(|(d, v)| (*d, v.iter().copied().collect())).collect();
        for i in 0..self.files.len() {
            if !self.passes(f, i) {
                continue;
            }
            let (_, dir, size, key) = &self.files[i];
            let group = &self.groups[key];
            for a in &self.ancestors[dir] {
                let inside = group.iter().filter(|&&j| anc_sets[&self.files[j].1].contains(a)).count();
                let e = out.get_mut(a).unwrap();
                e.0 += size;
                e.1 += 1;
                if group.len() >= 2 {
                    e.2 += 1;
                }
                if group.len() > inside {
                    e.3 += size;
                    e.4 += 1;
                }
            }
        }
        out
    }
}

fn filters(db: &Db) -> Vec<Filter> {
    let mut v = vec![Filter::default()];
    for c in
        [CopiesFilter::Unique, CopiesFilter::Duplicated, CopiesFilter::InOtherSources, CopiesFilter::OnlyThisSource]
    {
        v.push(Filter { copies: c, ..Default::default() });
    }
    v.push(Filter { hash: HashFilter::Sha256, ..Default::default() });
    v.push(Filter { hash: HashFilter::Flagged, ..Default::default() });
    for s in db.sources().unwrap() {
        v.push(Filter { absent_from: Some(s.id), ..Default::default() });
        v.push(Filter { present_in: Some(s.id), copies: CopiesFilter::Duplicated, ..Default::default() });
    }
    v.push(Filter {
        copies: CopiesFilter::InOtherSources,
        hash: HashFilter::Sha256,
        absent_from: db.sources().unwrap().first().map(|s| s.id),
        present_in: None,
    });
    v
}

fn check(db: &Db) -> usize {
    let ix = Index::build(db).unwrap();
    let b = Brute::load(db);
    assert_eq!(ix.file_count(), b.files.len());
    // per-file copies
    let pos: HashMap<i64, usize> = b.files.iter().enumerate().map(|(i, f)| (f.0, i)).collect();
    for f in 0..ix.file_count() as FileIx {
        let i = pos[&ix.file_db_id(f)];
        let src = b.dir_source[&b.files[i].1];
        assert_eq!(ix.copies(ix.file_content(f)) as usize, b.copies(i), "copies of file {}", b.files[i].0);
        assert_eq!(ix.copies_other_sources(f) as usize, b.copies(i) - b.copies_in_source(i, src));
    }
    let dup_groups = b.groups.values().filter(|g| g.len() >= 2).count();
    assert_eq!(ix.dup_group_count(), dup_groups);
    // per-folder stats under each filter
    let mut checked = 0;
    for f in filters(db) {
        let want = b.stats(&f);
        let got = ix.rollup(&f);
        for d in 0..ix.dir_count() as DirIx {
            let s = got.stats[d as usize];
            let w = want[&ix.dir_db_id(d)];
            assert_eq!(
                (s.bytes, s.files, s.dup_files, s.elsewhere_bytes, s.elsewhere_files),
                w,
                "folder {} filter {:?}",
                ix.dir_path(d),
                f
            );
            checked += 1;
        }
        // listings agree with the rollup
        let opts = ListOptions { filter: f.clone(), ..Default::default() };
        let root = ix.list(db, Node::AllSources, &opts).unwrap();
        for r in &root.dirs {
            assert_eq!(r.stats, got.stats[r.dir as usize]);
            let l = ix.list(db, Node::Dir(r.dir), &opts).unwrap();
            let sum_files: u32 = l.dirs.iter().map(|x| x.stats.files).sum::<u32>() + l.files.len() as u32;
            assert_eq!(sum_files, r.stats.files);
        }
    }
    checked
}

#[test]
fn generated_household_data() {
    for seed in [1u64, 2] {
        let dir = std::env::temp_dir().join(format!("dupemap-brute-{}-{seed}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exp = generate(Some(&dir), &Config { rows: 6_000, seed, bad_lines: true }).unwrap();
        let mut db = Db::open_in_memory().unwrap();
        let mut names: Vec<_> = exp.manifests.iter().map(|m| m.file_name.clone()).collect();
        names.sort();
        for n in names {
            db.import_file(&dir.join(n), &mut |_| true).unwrap();
        }
        let n = check(&db);
        assert!(n > 1000);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Small random trees with heavy duplication: nested copies, same-folder copies, copies across
/// sources, flagged hashes and overlapping manifests.
#[test]
fn random_adversarial_trees() {
    for seed in 0..25u64 {
        let mut rng = Rng::new(seed);
        let mut db = Db::open_in_memory().unwrap();
        let hosts = [("w", 'W'), ("l", 'L'), ("c", 'C')];
        let names = ["a", "B", "b", "c", "d"];
        let n_manifests = 1 + rng.below(3);
        for m in 0..n_manifests {
            let mut text = format!("{HEADER}\n");
            let rows = 5 + rng.below(60);
            for _ in 0..rows {
                let (host, style) = hosts[rng.below(3) as usize];
                let depth = 1 + rng.below(4);
                let mut parts: Vec<String> = (0..depth).map(|_| rng.pick(&names).to_string()).collect();
                parts.push(format!("f{}.bin", rng.below(6)));
                let path = match style {
                    'W' => format!("D:\\{}", parts.join("\\")),
                    'L' => format!("/{}", parts.join("/")),
                    _ => format!("gd:/{}", parts.join("/")),
                };
                let content = rng.below(8);
                let hash = if rng.chance(0.15) { format!("md5:{content:032x}") } else { format!("{content:064x}") };
                let size = 100 + content * 7;
                text.push_str(&format!("{hash}\t{size}\t{host}\t{path}\n"));
            }
            db.import_reader(
                &format!("m{m}.tsv"),
                "x",
                0,
                Some(1000 + rng.below(3) as i64),
                text.as_bytes(),
                &mut |_| true,
            )
            .unwrap();
        }
        check(&db);
        // removing a manifest keeps everything consistent
        if n_manifests > 1 {
            let first = db.manifests().unwrap()[0].id;
            db.remove_manifest(first).unwrap();
            check(&db);
        }
    }
}
