//! Cross-check the file-type summary against a brute-force count over the database rows.

use dupemap_core::ignore::IgnoreSettings;
use dupemap_core::index::Index;
use dupemap_core::manifest::HEADER;
use dupemap_core::summary::{Categories, Category, Split};
use dupemap_core::synth::{generate, Config, Rng};
use dupemap_core::Db;
use std::collections::{HashMap, HashSet};

/// algo, digest, size
type Key = (i64, Vec<u8>, u64);
/// source db id, size, extension, content key
type Row = (i64, u64, String, Key);

/// Every visible row.
fn rows(db: &Db) -> Vec<Row> {
    let c = db.conn();
    let mut st = c
        .prepare(
            "SELECT d.source_id, f.size, e.name, f.algo_id, f.digest FROM file f \
             JOIN dir d ON d.id = f.dir_id JOIN ext e ON e.id = f.ext_id WHERE f.shadowed_by IS NULL",
        )
        .unwrap();
    st.query_map([], |r| {
        let size = r.get::<_, i64>(1)? as u64;
        Ok((r.get(0)?, size, r.get(2)?, (r.get(3)?, r.get::<_, Vec<u8>>(4)?, size)))
    })
    .unwrap()
    .map(|r| r.unwrap())
    .collect()
}

fn add(s: &mut Split, kind: u8, size: u64) {
    s.files += 1;
    s.bytes += size;
    match kind {
        0 => {
            s.unique_files += 1;
            s.unique_bytes += size;
        }
        1 => {
            s.within_files += 1;
            s.within_bytes += size;
        }
        _ => {
            s.elsewhere_files += 1;
            s.elsewhere_bytes += size;
        }
    }
}

fn check(db: &Db, cats: &Categories) -> usize {
    let ix = Index::build_with(db, &IgnoreSettings::disabled(), &mut |_, _| {}).unwrap();
    let rows = rows(db);
    let mut groups: HashMap<&Key, (u32, HashSet<i64>)> = HashMap::new();
    for (src, _, _, key) in &rows {
        let g = groups.entry(key).or_default();
        g.0 += 1;
        g.1.insert(*src);
    }
    let kind = |key| {
        let (n, srcs) = &groups[key];
        if *n == 1 {
            0u8
        } else if srcs.len() == 1 {
            1
        } else {
            2
        }
    };
    let scopes: Vec<Option<u16>> = std::iter::once(None).chain((0..ix.sources().len() as u16).map(Some)).collect();
    for scope in scopes {
        let sum = ix.summary(scope, cats);
        let mut total = Split::default();
        let mut per_cat = vec![Split::default(); cats.list.len() + 1];
        let mut per_ext: HashMap<String, Split> = HashMap::new();
        let mut per_src: HashMap<i64, Split> = HashMap::new();
        for (src, size, ext, key) in &rows {
            if let Some(s) = scope {
                if ix.sources()[s as usize].db_id != *src {
                    continue;
                }
            }
            let k = kind(key);
            add(&mut total, k, *size);
            add(&mut per_cat[cats.category_of(ext)], k, *size);
            add(per_ext.entry(ext.clone()).or_default(), k, *size);
            add(per_src.entry(*src).or_default(), k, *size);
        }
        assert_eq!(sum.total, total, "total, scope {scope:?}");
        assert_eq!(sum.categories, per_cat, "categories, scope {scope:?}");
        assert_eq!(sum.names.len(), per_cat.len());
        assert_eq!(sum.exts.len(), per_ext.len(), "every extension present, once");
        for r in &sum.exts {
            assert_eq!(r.split, per_ext[&r.ext], "extension {:?}", r.ext);
            assert_eq!(r.category, cats.category_of(&r.ext));
        }
        assert!(sum.exts.windows(2).all(|w| w[0].split.bytes >= w[1].split.bytes), "most bytes first");
        match scope {
            Some(_) => assert!(sum.sources.is_empty()),
            None => {
                assert_eq!(sum.sources.len(), ix.sources().len());
                for (s, sp) in &sum.sources {
                    let id = ix.sources()[*s as usize].db_id;
                    assert_eq!(*sp, per_src.get(&id).copied().unwrap_or_default(), "source {id}");
                }
            }
        }
    }
    rows.len()
}

#[test]
fn generated_household_data() {
    let dir = std::env::temp_dir().join(format!("dupemap-summary-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let exp = generate(Some(&dir), &Config { rows: 8_000, seed: 7, bad_lines: true }).unwrap();
    let mut db = Db::open_in_memory().unwrap();
    let mut names: Vec<_> = exp.manifests.iter().map(|m| m.file_name.clone()).collect();
    names.sort();
    for n in names {
        db.import_file(&dir.join(n), &mut |_| true).unwrap();
    }
    assert!(check(&db, &Categories::default()) > 4000);
    // an edited mapping: categories renamed, merged and emptied
    let edited = Categories {
        list: vec![
            Category { name: "Media".into(), exts: vec!["mkv".into(), "mp4".into(), "jpg".into()] },
            Category { name: "Empty".into(), exts: vec![] },
        ],
    };
    check(&db, &edited);
    check(&db, &Categories { list: vec![] });
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn random_sources_extensions_and_copies() {
    for seed in 0..20u64 {
        let mut rng = Rng::new(seed);
        let mut db = Db::open_in_memory().unwrap();
        let hosts = ["PC", "nas", "gdrive:"];
        let exts = ["mkv", "MP4", "jpg", "", "tar.gz", "weird", "zip"];
        for m in 0..1 + rng.below(3) {
            let mut text = format!("{HEADER}\n");
            for i in 0..5 + rng.below(80) {
                let host = hosts[rng.below(3) as usize];
                let ext = exts[rng.below(exts.len() as u64) as usize];
                let name = if ext.is_empty() { format!("f{i}") } else { format!("f{i}.{ext}") };
                let path = match host {
                    "PC" => format!("D:\\d{}\\{name}", rng.below(4)),
                    "nas" => format!("/mnt/p/d{}/{name}", rng.below(4)),
                    _ => format!("gdrive:/d{}/{name}", rng.below(4)),
                };
                let content = rng.below(10);
                let hash = if rng.chance(0.1) { format!("md5:{content:032x}") } else { format!("{content:064x}") };
                text.push_str(&format!("{hash}\t{}\t{host}\t{path}\n", 100 + content * 3));
            }
            db.import_reader(&format!("m{m}.tsv"), "x", 0, Some(1000 + m as i64), text.as_bytes(), &mut |_| true)
                .unwrap();
        }
        check(&db, &Categories::default());
        let mine = Categories {
            list: vec![Category { name: "Video".into(), exts: vec!["mp4".into(), "mkv".into(), "".into()] }],
        };
        check(&db, &mine);
    }
}
