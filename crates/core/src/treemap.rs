//! Treemaps: what a folder holds, drawn as rectangles sized by their bytes, nested folder by
//! folder. The layout is the "squarified" one (Bruls, Huizing and van Wijk, 2000), which keeps
//! the rectangles close to square so they stay readable.
//!
//! Built from the index alone (no database): sizes come from the folder totals of a filter,
//! colours from a function the caller passes, which sorts every file into a class (duplicated
//! elsewhere, a category, marked...). Items too small to draw are folded into one box per
//! folder, and the number of boxes is capped, so a treemap of millions of files stays a few
//! thousand rectangles.

use crate::index::{DirIx, FileIx, Filter, Index, Node};
use std::collections::VecDeque;

/// A rectangle in screen units.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }

    pub fn area(&self) -> f32 {
        self.w.max(0.0) * self.h.max(0.0)
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}

/// Lay out `sizes` (largest first, each above 0) in `r`: one rectangle each, with areas in
/// proportion to the sizes, filling `r` exactly.
pub fn squarify(sizes: &[f64], r: Rect) -> Vec<Rect> {
    let n = sizes.len();
    let mut out = vec![Rect::new(r.x, r.y, 0.0, 0.0); n];
    let total: f64 = sizes.iter().sum();
    if n == 0 || total <= 0.0 || r.w <= 0.0 || r.h <= 0.0 {
        return out;
    }
    debug_assert!(sizes.windows(2).all(|p| p[0] >= p[1]), "sizes must be sorted, largest first");
    let scale = r.w as f64 * r.h as f64 / total;
    let areas: Vec<f64> = sizes.iter().map(|s| s * scale).collect();
    let (mut x, mut y, mut w, mut h) = (r.x as f64, r.y as f64, r.w as f64, r.h as f64);
    let mut i = 0;
    while i < n {
        if w <= 0.0 || h <= 0.0 {
            break;
        }
        // a row along the shorter side: add items while that makes its worst aspect ratio better
        let side = w.min(h);
        let worst = |max: f64, min: f64, sum: f64| -> f64 {
            let (s2, side2) = (sum * sum, side * side);
            (side2 * max / s2).max(s2 / (side2 * min))
        };
        let mut j = i + 1;
        let mut sum = areas[i];
        let mut current = worst(areas[i], areas[i], sum);
        while j < n {
            let next_sum = sum + areas[j];
            let next = worst(areas[i], areas[j], next_sum);
            if next > current {
                break;
            }
            sum = next_sum;
            current = next;
            j += 1;
        }
        // the last row takes what is left, so rounding leaves no gap
        let last = j == n;
        if w >= h {
            let cw = if last { w } else { sum / h };
            let mut yy = y;
            for k in i..j {
                let kh = if k + 1 == j { y + h - yy } else { areas[k] / cw };
                out[k] = Rect::new(x as f32, yy as f32, cw as f32, kh as f32);
                yy += kh;
            }
            x += cw;
            w -= cw;
        } else {
            let rh = if last { h } else { sum / w };
            let mut xx = x;
            for k in i..j {
                let kw = if k + 1 == j { x + w - xx } else { areas[k] / rh };
                out[k] = Rect::new(xx as f32, y as f32, kw as f32, rh as f32);
                xx += kw;
            }
            y += rh;
            h -= rh;
        }
        i = j;
    }
    out
}

/// What a box stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Item {
    Dir(DirIx),
    File(FileIx),
    /// Everything in a folder (`None`: among the sources) too small to draw on its own.
    Rest {
        in_dir: Option<DirIx>,
        items: u32,
    },
}

impl Item {
    /// Order of equal sizes: folders, then files, by index; the rest last.
    fn key(&self) -> (u8, u32) {
        match *self {
            Item::Dir(d) => (0, d),
            Item::File(f) => (1, f),
            Item::Rest { .. } => (2, 0),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TmBox {
    pub rect: Rect,
    pub item: Item,
    /// 0 for what the root holds, 1 inside those, ...
    pub depth: u16,
    /// The folder box it is drawn in.
    pub parent: Option<u32>,
    pub bytes: u64,
    pub files: u32,
    /// A folder whose contents are drawn inside it, below its header strip.
    pub open: bool,
    /// An open folder that holds only one folder (and so on) is drawn with the innermost one's
    /// contents: that folder.
    pub through: Option<DirIx>,
    mix: (u32, u32),
    rest: (u32, u32),
}

/// Sizes that decide what is drawn.
#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    /// Height of the strip with a folder's name above its contents.
    pub header: f32,
    /// Space around a folder's contents (sides and bottom).
    pub pad: f32,
    /// Items smaller than this (square units) are folded into the folder's "rest" box.
    pub min_area: f32,
    /// The same for what the root holds, which may also use up the whole box budget: a folder
    /// of thousands of similar items is shown item by item once you zoom into it.
    pub min_area_top: f32,
    /// A folder box this wide and high has its contents drawn inside.
    pub open_w: f32,
    pub open_h: f32,
    /// At most this many boxes in all; deeper levels are left closed beyond it.
    pub max_boxes: usize,
    /// At most this many items drawn in one folder (the rest is folded).
    pub max_items: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            header: 16.0,
            pad: 2.0,
            min_area: 30.0,
            min_area_top: 6.0,
            open_w: 44.0,
            open_h: 40.0,
            max_boxes: 20_000,
            max_items: 3_000,
        }
    }
}

/// Number of colour classes a file can be sorted into.
pub const CLASSES: usize = 32;

#[derive(Debug, Clone, Default)]
pub struct Treemap {
    /// Folder boxes come before the boxes drawn in them.
    pub boxes: Vec<TmBox>,
    mix: Vec<(u8, u64)>,
    rest: Vec<Item>,
    /// Totals of what the root holds (under the filter).
    pub bytes: u64,
    pub files: u64,
    /// The box cap was reached: some folders that had room stay closed.
    pub truncated: bool,
}

impl Treemap {
    /// Bytes per colour class of a box drawn whole (not an open folder), by class.
    pub fn mix(&self, b: usize) -> &[(u8, u64)] {
        let (s, e) = self.boxes[b].mix;
        &self.mix[s as usize..e as usize]
    }

    /// What a rest box holds.
    pub fn rest_items(&self, b: usize) -> &[Item] {
        let (s, e) = self.boxes[b].rest;
        &self.rest[s as usize..e as usize]
    }

    /// The innermost box at a point.
    pub fn hit(&self, x: f32, y: f32) -> Option<usize> {
        self.boxes.iter().rposition(|b| b.rect.contains(x, y))
    }

    /// The box of a folder or file, if it is drawn.
    pub fn find(&self, item: Item) -> Option<usize> {
        self.boxes.iter().position(|b| b.item == item)
    }
}

/// Where an open folder's contents go.
pub fn inner(r: Rect, opts: &Options) -> Rect {
    Rect::new(
        r.x + opts.pad,
        r.y + opts.header,
        (r.w - 2.0 * opts.pad).max(0.0),
        (r.h - opts.header - opts.pad).max(0.0),
    )
}

/// Lay out what `root` holds in `area`. Sizes are the bytes of the files that pass `filter`;
/// `class` sorts a file into a colour class (below [`CLASSES`]).
pub fn build(
    ix: &Index,
    root: Node,
    filter: &Filter,
    area: Rect,
    opts: &Options,
    class: &dyn Fn(FileIx) -> u8,
) -> Treemap {
    let roll = ix.rollup(filter);
    let passes = ix.matcher(filter);
    let mut tm = Treemap::default();
    let mut queue: VecDeque<(Option<u32>, Node, Rect, u16)> = VecDeque::new();
    queue.push_back((None, root, area, 0));
    while let Some((parent, node, rect, depth)) = queue.pop_front() {
        // what the folder holds: (bytes, files, item)
        let mut kids: Vec<(u64, u32, Item)> = Vec::new();
        let add_dir = |d: DirIx, kids: &mut Vec<(u64, u32, Item)>| {
            let st = roll.stats[d as usize];
            if st.bytes > 0 {
                kids.push((st.bytes, st.files, Item::Dir(d)));
            }
        };
        let in_dir = match node {
            Node::AllSources => {
                for s in ix.sources() {
                    add_dir(s.root, &mut kids);
                }
                None
            }
            Node::Dir(d) => {
                for &c in ix.dir_children(d) {
                    add_dir(c, &mut kids);
                }
                for f in ix.dir_files(d) {
                    let b = ix.file_size(f);
                    if b > 0 && passes(f) {
                        kids.push((b, 1, Item::File(f)));
                    }
                }
                Some(d)
            }
        };
        // a folder holding just one folder: draw that one's contents, under one header
        if let (Some(p), true) = (parent, kids.len() == 1) {
            if let Item::Dir(only) = kids[0].2 {
                tm.boxes[p as usize].through = Some(only);
                queue.push_front((parent, Node::Dir(only), rect, depth));
                continue;
            }
        }
        if parent.is_none() {
            tm.bytes = kids.iter().map(|k| k.0).sum();
            tm.files = kids.iter().map(|k| k.1 as u64).sum();
        }
        if kids.is_empty() {
            continue;
        }
        let room = opts.max_boxes.saturating_sub(tm.boxes.len());
        if room == 0 {
            tm.truncated = true;
            if let Some(p) = parent {
                tm.boxes[p as usize].open = false;
            }
            continue;
        }
        kids.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.2.key().cmp(&b.2.key())));
        let total: u64 = kids.iter().map(|k| k.0).sum();
        let per_byte = rect.area() as f64 / total as f64;
        // big enough to draw on their own; one place is kept for the rest
        let (min_area, cap) =
            if parent.is_none() { (opts.min_area_top, room) } else { (opts.min_area, opts.max_items.min(room)) };
        let cap = cap.max(1);
        let fit = kids.iter().take_while(|k| k.0 as f64 * per_byte >= min_area as f64).count();
        let keep = if fit == kids.len() && fit <= cap { fit } else { fit.min(cap - 1) };
        let mut placed: Vec<(u64, u32, Item)> = kids[..keep].to_vec();
        let rest_start = tm.rest.len() as u32;
        if keep < kids.len() {
            let folded = &kids[keep..];
            let bytes: u64 = folded.iter().map(|k| k.0).sum();
            let files: u32 = folded.iter().map(|k| k.1).sum();
            tm.rest.extend(folded.iter().map(|k| k.2));
            let at = placed.partition_point(|k| k.0 >= bytes);
            placed.insert(at, (bytes, files, Item::Rest { in_dir, items: folded.len() as u32 }));
        }
        let rest_end = tm.rest.len() as u32;
        let sizes: Vec<f64> = placed.iter().map(|k| k.0 as f64).collect();
        let rects = squarify(&sizes, rect);
        for ((bytes, files, item), r) in placed.into_iter().zip(rects) {
            let b = tm.boxes.len() as u32;
            let rest = if matches!(item, Item::Rest { .. }) { (rest_start, rest_end) } else { (0, 0) };
            let open = match item {
                Item::Dir(_) => r.w >= opts.open_w && r.h >= opts.open_h && depth < u16::MAX - 1,
                _ => false,
            };
            tm.boxes.push(TmBox { rect: r, item, depth, parent, bytes, files, open, through: None, mix: (0, 0), rest });
            if let (true, Item::Dir(d)) = (open, item) {
                queue.push_back((Some(b), Node::Dir(d), inner(r, opts), depth + 1));
            }
        }
    }
    // colours of everything drawn whole: files, closed folders, the rest
    let mut acc = [0u64; CLASSES];
    let add = |f: FileIx, acc: &mut [u64; CLASSES]| {
        let b = ix.file_size(f);
        if b > 0 {
            acc[(class(f) as usize).min(CLASSES - 1)] += b;
        }
    };
    let add_item = |it: Item, acc: &mut [u64; CLASSES]| match it {
        Item::File(f) => add(f, acc),
        Item::Dir(d) => {
            for f in ix.subtree_files(d) {
                if passes(f) {
                    add(f, acc);
                }
            }
        }
        Item::Rest { .. } => {}
    };
    for b in 0..tm.boxes.len() {
        if tm.boxes[b].open {
            continue;
        }
        acc.fill(0);
        match tm.boxes[b].item {
            Item::Rest { .. } => {
                let (s, e) = tm.boxes[b].rest;
                for k in s..e {
                    add_item(tm.rest[k as usize], &mut acc);
                }
            }
            it => add_item(it, &mut acc),
        }
        let start = tm.mix.len() as u32;
        tm.mix.extend(acc.iter().enumerate().filter(|(_, &v)| v > 0).map(|(c, &v)| (c as u8, v)));
        tm.boxes[b].mix = (start, tm.mix.len() as u32);
    }
    tm
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::index::CopiesFilter;

    fn overlap(a: &Rect, b: &Rect) -> f32 {
        let w = (a.x + a.w).min(b.x + b.w) - a.x.max(b.x);
        let h = (a.y + a.h).min(b.y + b.h) - a.y.max(b.y);
        w.max(0.0) * h.max(0.0)
    }

    fn check_layout(sizes: &[f64], r: Rect) -> Vec<Rect> {
        let out = squarify(sizes, r);
        let total: f64 = sizes.iter().sum();
        let mut sum = 0.0;
        for (s, q) in sizes.iter().zip(&out) {
            let want = s / total * r.area() as f64;
            assert!((q.area() as f64 - want).abs() <= want * 1e-3 + 0.05, "area {} for {want}", q.area());
            assert!(q.x >= r.x - 1e-3 && q.y >= r.y - 1e-3, "{q:?} starts outside {r:?}");
            assert!(q.x + q.w <= r.x + r.w + 1e-2 && q.y + q.h <= r.y + r.h + 1e-2, "{q:?} ends outside {r:?}");
            sum += q.area() as f64;
        }
        assert!((sum - r.area() as f64).abs() < r.area() as f64 * 1e-3, "fills the rectangle");
        for i in 0..out.len() {
            for j in i + 1..out.len() {
                assert!(overlap(&out[i], &out[j]) < 0.05, "{:?} overlaps {:?}", out[i], out[j]);
            }
        }
        out
    }

    #[test]
    fn squarified_rectangles_fill_without_overlap_in_proportion() {
        check_layout(&[6.0, 6.0, 4.0, 3.0, 2.0, 2.0, 1.0], Rect::new(0.0, 0.0, 600.0, 400.0));
        check_layout(&[1.0], Rect::new(10.0, 20.0, 30.0, 40.0));
        check_layout(&[1000.0, 1.0, 1.0, 0.5], Rect::new(0.0, 0.0, 100.0, 1000.0));
        // pseudo-random sizes over several orders of magnitude, in odd shapes
        let mut seed = 12345u64;
        for round in 0..200 {
            let n = 1 + (round % 60);
            let mut sizes: Vec<f64> = (0..n)
                .map(|_| {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                    1.0 + (seed >> 40) as f64 % 1e5 * if seed & 1 == 0 { 1.0 } else { 0.001 }
                })
                .collect();
            sizes.sort_by(|a, b| b.partial_cmp(a).unwrap());
            let w = 50.0 + (round * 37 % 900) as f32;
            let h = 30.0 + (round * 53 % 700) as f32;
            check_layout(&sizes, Rect::new(5.0, 7.0, w, h));
        }
    }

    #[test]
    fn equal_sizes_come_out_nearly_square() {
        for q in check_layout(&[1.0; 16], Rect::new(0.0, 0.0, 400.0, 400.0)) {
            let ratio = q.w.max(q.h) / q.w.min(q.h);
            assert!(ratio < 1.5, "{q:?}");
        }
    }

    #[test]
    fn nothing_to_lay_out() {
        assert!(squarify(&[], Rect::new(0.0, 0.0, 10.0, 10.0)).is_empty());
        assert_eq!(squarify(&[1.0, 1.0], Rect::new(0.0, 0.0, 0.0, 10.0)).iter().map(Rect::area).sum::<f32>(), 0.0);
    }

    const HEADER: &str = "hash\tsize\thost\tpath";

    fn h(n: u32) -> String {
        format!("{n:064x}")
    }

    /// PC: D:\Big (two 5 MB files, a copy of one on the NAS), D:\Small (200 files of 1 KB),
    /// D:\one.bin (3 MB, unique). NAS: /data/b1.bin (copy of D:\Big\b1.bin).
    fn fixture() -> (Db, Index) {
        let mut db = Db::open_in_memory().unwrap();
        let mut pc = format!(
            "{HEADER}\n{}\t5000000\tPC D: (Data)\tD:\\Big\\b1.bin\n{}\t5000000\tPC D: (Data)\tD:\\Big\\b2.bin\n{}\t3000000\tPC D: (Data)\tD:\\one.bin\n",
            h(1),
            h(2),
            h(3)
        );
        for i in 0..200 {
            pc.push_str(&format!("{}\t1000\tPC D: (Data)\tD:\\Small\\s{i}.txt\n", h(100 + i)));
        }
        let nas = format!("{HEADER}\n{}\t5000000\tnas\t/data/b1.bin\n", h(1));
        db.import_reader("pc.tsv", "pc.tsv", 0, None, pc.as_bytes(), &mut |_| true).unwrap();
        db.import_reader("nas.tsv", "nas.tsv", 0, None, nas.as_bytes(), &mut |_| true).unwrap();
        let ix = Index::build(&db).unwrap();
        (db, ix)
    }

    fn dir(ix: &Index, path: &str) -> DirIx {
        (0..ix.dir_count() as DirIx).find(|&d| ix.dir_path(d) == path).unwrap()
    }

    /// Class 1: has a copy in another source; 0: not.
    fn elsewhere(ix: &Index) -> impl Fn(FileIx) -> u8 + '_ {
        move |f| u8::from(ix.copies_other_sources(f) > 0)
    }

    #[test]
    fn boxes_nest_and_their_colours_add_up() {
        let (_db, ix) = fixture();
        let area = Rect::new(0.0, 0.0, 800.0, 500.0);
        let opts = Options::default();
        let tm = build(&ix, Node::AllSources, &Filter::default(), area, &opts, &elsewhere(&ix));
        assert_eq!(tm.bytes, 5_000_000 * 3 + 3_000_000 + 200_000);
        assert_eq!(tm.files, 204);
        // the two sources fill the area
        let top: Vec<&TmBox> = tm.boxes.iter().filter(|b| b.parent.is_none()).collect();
        assert_eq!(top.len(), 2);
        assert!((top.iter().map(|b| b.rect.area()).sum::<f32>() - area.area()).abs() < 1.0);
        for (i, b) in tm.boxes.iter().enumerate() {
            if let Some(p) = b.parent {
                let pr = inner(tm.boxes[p as usize].rect, &opts);
                assert!(tm.boxes[p as usize].open);
                assert!(
                    b.rect.x >= pr.x - 0.01 && b.rect.x + b.rect.w <= pr.x + pr.w + 0.01,
                    "box {i} inside its folder"
                );
                assert!((p as usize) < i, "folders come before their contents");
            }
            if !b.open {
                let sum: u64 = tm.mix(i).iter().map(|m| m.1).sum();
                assert_eq!(sum, b.bytes, "colours of box {i} add up to its bytes");
            }
        }
        // the big files are drawn on their own, coloured by where their copies are
        let pc = ix.sources().iter().position(|s| s.host == "PC D: (Data)").unwrap();
        let big = dir(&ix, r"D:\Big");
        let b1 = ix.dir_files(big).find(|&f| ix.copies_other_sources(f) > 0).unwrap();
        let bx = tm.find(Item::File(b1)).expect("b1 is drawn");
        assert_eq!(tm.mix(bx), [(1, 5_000_000)]);
        // the NAS holds one folder in one folder: drawn as one box showing /data's contents
        let nas = ix.sources().iter().position(|s| s.host == "nas").unwrap();
        let nb = tm.find(Item::Dir(ix.sources()[nas].root)).unwrap();
        assert_eq!(tm.boxes[nb].through.map(|d| ix.dir_path(d)), Some("/data".to_string()));
        assert!(tm.boxes.iter().any(|b| b.parent == Some(nb as u32) && matches!(b.item, Item::File(_))));
        // the hit test finds the innermost box
        let r = tm.boxes[bx].rect;
        assert_eq!(tm.hit(r.x + r.w / 2.0, r.y + r.h / 2.0), Some(bx));
        assert!(tm.boxes[tm.find(Item::Dir(ix.sources()[pc].root)).unwrap()].open);
        // 200 small files fold into one box, all class 0
        let small = tm.find(Item::Dir(dir(&ix, r"D:\Small"))).unwrap();
        if tm.boxes[small].open {
            let rest = tm.boxes.iter().position(|b| matches!(b.item, Item::Rest { .. })).unwrap();
            assert_eq!(tm.boxes[rest].files as usize, tm.rest_items(rest).len());
            assert!(tm.boxes.len() < 60);
        } else {
            assert_eq!(tm.mix(small), [(0, 200_000)]);
        }
    }

    #[test]
    fn small_items_fold_into_one_box() {
        let (_db, ix) = fixture();
        let small = dir(&ix, r"D:\Small");
        let tm = build(
            &ix,
            Node::Dir(small),
            &Filter::default(),
            Rect::new(0.0, 0.0, 100.0, 100.0),
            &Options { min_area: 100.0, min_area_top: 100.0, ..Default::default() },
            &|_| 0,
        );
        // 200 equal files of 50 square units each: none big enough, so one box holds them all
        assert_eq!(tm.boxes.len(), 1);
        assert_eq!(tm.boxes[0].item, Item::Rest { in_dir: Some(small), items: 200 });
        assert_eq!((tm.boxes[0].bytes, tm.boxes[0].files), (200_000, 200));
        assert_eq!(tm.mix(0), [(0, 200_000)]);
        // with room, every file is drawn
        let tm = build(
            &ix,
            Node::Dir(small),
            &Filter::default(),
            Rect::new(0.0, 0.0, 1000.0, 1000.0),
            &Options::default(),
            &|_| 0,
        );
        assert_eq!(tm.boxes.len(), 200);
    }

    #[test]
    fn the_filter_decides_the_sizes_and_the_cap_holds() {
        let (_db, ix) = fixture();
        let unique = Filter { copies: CopiesFilter::Unique, ..Default::default() };
        let area = Rect::new(0.0, 0.0, 800.0, 500.0);
        let tm = build(&ix, Node::AllSources, &unique, area, &Options::default(), &|_| 0);
        assert_eq!(tm.bytes, 5_000_000 + 3_000_000 + 200_000, "b2, one.bin and the small files");
        assert_eq!(tm.boxes.iter().filter(|b| b.parent.is_none()).count(), 1, "the NAS has nothing unique");
        let tm = build(
            &ix,
            Node::AllSources,
            &Filter::default(),
            area,
            &Options { max_boxes: 5, ..Default::default() },
            &|_| 0,
        );
        assert!(tm.boxes.len() <= 5);
        assert!(tm.truncated);
        for (i, b) in tm.boxes.iter().enumerate() {
            if !b.open {
                assert_eq!(tm.mix(i).iter().map(|m| m.1).sum::<u64>(), b.bytes);
            }
        }
    }
}
