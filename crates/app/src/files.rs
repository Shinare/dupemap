//! Cache of file listings (names come from the database through the query worker).

use crate::worker::{Query, Worker};
use dupemap_core::index::{DirIx, FileRow, ListOptions};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Rows kept before least-recently-used listings are dropped (about 100 bytes each).
const MAX_ROWS: usize = 1_500_000;

type Key = (DirIx, ListOptions);

#[derive(Default)]
pub struct FileCache {
    generation: u64,
    map: HashMap<Key, (Arc<Vec<FileRow>>, u64)>,
    pending: HashSet<Key>,
    errors: HashMap<Key, String>,
    rows: usize,
    clock: u64,
    /// Bumped whenever a listing arrives, so panes know to rebuild their rows.
    pub version: u64,
}

impl FileCache {
    /// Forget everything (a new index makes old listings meaningless).
    pub fn reset(&mut self, generation: u64) {
        *self = FileCache { generation, version: self.version + 1, ..Default::default() };
    }

    pub fn get(&mut self, dir: DirIx, opts: &ListOptions) -> Option<Arc<Vec<FileRow>>> {
        self.clock += 1;
        let clock = self.clock;
        let e = self.map.get_mut(&(dir, opts.clone()))?;
        e.1 = clock;
        Some(e.0.clone())
    }

    pub fn error(&self, dir: DirIx, opts: &ListOptions) -> Option<&str> {
        self.errors.get(&(dir, opts.clone())).map(|s| s.as_str())
    }

    /// Ask the worker for a listing unless it is already cached or on its way.
    pub fn request(&mut self, worker: &Worker, dir: DirIx, opts: &ListOptions) {
        let key = (dir, opts.clone());
        if self.map.contains_key(&key) || self.pending.contains(&key) || self.errors.contains_key(&key) {
            return;
        }
        self.pending.insert(key);
        worker.send(Query::Files { generation: self.generation, dir, opts: opts.clone() });
    }

    pub fn is_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    pub fn insert(&mut self, generation: u64, dir: DirIx, opts: ListOptions, files: Result<Arc<Vec<FileRow>>, String>) {
        if generation != self.generation {
            return;
        }
        let key = (dir, opts);
        self.pending.remove(&key);
        match files {
            Ok(list) => {
                self.clock += 1;
                self.rows += list.len();
                if let Some(old) = self.map.insert(key, (list, self.clock)) {
                    self.rows -= old.0.len();
                }
                self.evict();
            }
            Err(e) => {
                self.errors.insert(key, e);
            }
        }
        self.version += 1;
    }

    fn evict(&mut self) {
        if self.rows <= MAX_ROWS {
            return;
        }
        let mut by_age: Vec<(u64, Key)> = self.map.iter().map(|(k, v)| (v.1, k.clone())).collect();
        by_age.sort_by_key(|x| x.0);
        for (_, k) in by_age {
            if self.rows <= MAX_ROWS * 3 / 4 {
                break;
            }
            if let Some((list, _)) = self.map.remove(&k) {
                self.rows -= list.len();
            }
        }
    }
}
