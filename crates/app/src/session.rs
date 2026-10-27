//! The saved session: which tabs are open and how they are laid out, per tab the expanded
//! folders, selection, sort, filters and scroll position, the inspector, and the window.
//!
//! It is written (at most once a second, only when something changed) by a background thread to
//! a file beside the database (`dupemap.session.json`, see `dupemap_core::sessionfile`), so a
//! crash loses at most the last second of changes. Folders, files and sources are saved by
//! database id, so the session survives re-imports and index rebuilds; ids that no longer exist
//! are simply ignored.

use crate::app::Tab;
use crate::pane::Sel;
use dupemap_core::sessionfile::SessionFile;
use egui_dock::DockState;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;

/// Bumped when a change to the format cannot be read by older versions.
pub const VERSION: u32 = 1;
/// Smallest window the app can be restored to.
pub const MIN_WINDOW: [f32; 2] = [760.0, 440.0];
/// More tabs than this is not a session someone made by hand: start fresh.
const MAX_TABS: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowState {
    /// Size of the window's inner area (logical points), as it was when not maximised.
    pub size: [f32; 2],
    /// Top left corner of the window (logical points).
    pub pos: Option<[f32; 2]>,
    pub maximized: bool,
}

impl WindowState {
    /// Is it something we can open a window with? Sizes are clamped; a position far outside any
    /// plausible desktop is dropped (the window then opens where the system puts it).
    pub fn sane(mut self) -> Option<Self> {
        if !self.size.iter().all(|v| v.is_finite()) {
            return None;
        }
        self.size = [self.size[0].clamp(MIN_WINDOW[0], 16000.0), self.size[1].clamp(MIN_WINDOW[1], 16000.0)];
        if let Some([x, y]) = self.pos {
            if !(x.is_finite() && y.is_finite())
                || !(-8000.0..=16000.0).contains(&x)
                || !(-2000.0..=16000.0).contains(&y)
            {
                self.pos = None;
            }
        }
        Some(self)
    }
}

/// What gets written. Borrows from the app.
#[derive(Serialize)]
pub struct SessionOut<'a> {
    pub version: u32,
    pub dock: &'a DockState<Tab>,
    pub focused: Option<u64>,
    pub inspector_open: bool,
    pub inspector_width: Option<f32>,
    pub inspected: Option<Sel>,
    pub window: Option<WindowState>,
}

/// What is read back.
#[derive(Clone, Deserialize)]
pub struct Session {
    pub version: u32,
    pub dock: DockState<Tab>,
    #[serde(default)]
    pub focused: Option<u64>,
    #[serde(default = "yes")]
    pub inspector_open: bool,
    #[serde(default)]
    pub inspector_width: Option<f32>,
    #[serde(default)]
    pub inspected: Option<Sel>,
    #[serde(default)]
    pub window: Option<WindowState>,
}

fn yes() -> bool {
    true
}

/// The dock keeps the rectangle of every node, and before a node was first drawn that is
/// "nothing" (infinite), which JSON writes as `null` and cannot read back as a number. The
/// rectangles are recomputed at the next frame, so any number will do.
fn finite_rects(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(map) => {
            for key in ["x", "y"] {
                if let Some(n) = map.get_mut(key) {
                    if n.is_null() {
                        *n = serde_json::Value::from(0.0);
                    }
                }
            }
            for child in map.values_mut() {
                finite_rects(child);
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(finite_rects),
        _ => {}
    }
}

impl Session {
    /// Read one saved text. Refuses what this version cannot use: a newer format, no tabs, tabs
    /// sharing an id.
    pub fn parse(text: &str) -> Result<Session, String> {
        let mut json: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        finite_rects(&mut json);
        let mut s: Session = serde_json::from_value(json).map_err(|e| e.to_string())?;
        if s.version == 0 || s.version > VERSION {
            return Err(format!("session format {} is not supported", s.version));
        }
        let ids: Vec<u64> = s.dock.iter_all_tabs().map(|(_, t)| t.id).collect();
        if ids.is_empty() {
            return Err("no tabs".into());
        }
        if ids.len() > MAX_TABS {
            return Err(format!("{} tabs", ids.len()));
        }
        if ids.iter().collect::<HashSet<_>>().len() != ids.len() {
            return Err("tab ids are not unique".into());
        }
        s.window = s.window.and_then(WindowState::sane);
        if s.focused.is_some_and(|f| !ids.contains(&f)) {
            s.focused = ids.first().copied();
        }
        for (_, tab) in s.dock.iter_all_tabs_mut() {
            tab.kind.after_load();
        }
        Ok(s)
    }

    /// The newest saved session that can be read, and what went wrong with newer candidates.
    pub fn load(file: &SessionFile) -> (Option<Session>, Vec<String>) {
        let mut problems = Vec::new();
        for (i, text) in file.read().into_iter().enumerate() {
            match Session::parse(&text) {
                Ok(s) => {
                    if i > 0 {
                        problems.push("The saved layout was damaged; restored the previous one".to_string());
                    }
                    return (Some(s), problems);
                }
                Err(e) => problems.push(format!("Could not read the saved layout: {e}")),
            }
        }
        (None, problems)
    }

    pub fn next_tab_id(&self) -> u64 {
        self.dock.iter_all_tabs().map(|(_, t)| t.id).max().unwrap_or(0) + 1
    }
}

/// Writes sessions on a background thread; only the newest submitted text is ever written.
pub struct Saver {
    tx: Option<Sender<String>>,
    thread: Option<JoinHandle<()>>,
    failed: Arc<AtomicBool>,
}

impl Saver {
    pub fn spawn(file: SessionFile) -> Saver {
        let (tx, rx) = mpsc::channel::<String>();
        let failed = Arc::new(AtomicBool::new(false));
        let flag = failed.clone();
        let thread = std::thread::Builder::new().name("session-saver".into()).spawn(move || {
            while let Ok(mut text) = rx.recv() {
                while let Ok(newer) = rx.try_recv() {
                    text = newer;
                }
                flag.store(file.write(&text).is_err(), Ordering::Relaxed);
            }
        });
        Saver { tx: Some(tx), thread: thread.ok(), failed }
    }

    pub fn submit(&self, text: String) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(text);
        }
    }

    /// The last write failed (read-only folder, full disk…).
    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }

    /// Write what was submitted and stop the thread.
    pub fn finish(&mut self) {
        self.tx = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Saver {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::TabKind;
    use crate::groups::GroupsPane;
    use crate::pane::TreePane;
    use crate::tree::Root;
    use dupemap_core::index::{CopiesFilter, SortKey};
    use egui_dock::NodeIndex;

    fn out(dock: &DockState<Tab>) -> String {
        serde_json::to_string(&SessionOut {
            version: VERSION,
            dock,
            focused: Some(2),
            inspector_open: false,
            inspector_width: Some(310.0),
            inspected: Some(Sel::File(42)),
            window: Some(WindowState { size: [1200.0, 800.0], pos: Some([10.0, 20.0]), maximized: false }),
        })
        .unwrap()
    }

    fn sample() -> DockState<Tab> {
        let mut tree = TreePane::new(Root::Source(7));
        tree.expanded.extend([3, 5, 9]);
        tree.selected = Some(Sel::Dir(5));
        tree.opts.sort = SortKey::Elsewhere;
        tree.opts.descending = true;
        tree.opts.filter.copies = CopiesFilter::Unique;
        tree.scroll = 440.0;
        let mut dock = DockState::new(vec![Tab { id: 1, kind: TabKind::Tree(tree) }]);
        dock.main_surface_mut().split_right(
            NodeIndex::root(),
            0.4,
            vec![Tab { id: 2, kind: TabKind::Groups(GroupsPane::default()) }],
        );
        dock
    }

    #[test]
    fn round_trip_keeps_tabs_layout_and_view_state() {
        let text = out(&sample());
        let s = Session::parse(&text).unwrap();
        assert_eq!(s.next_tab_id(), 3);
        assert_eq!(s.focused, Some(2));
        assert!(!s.inspector_open);
        assert_eq!(s.inspector_width, Some(310.0));
        assert_eq!(s.inspected, Some(Sel::File(42)));
        assert_eq!(s.window.unwrap().size, [1200.0, 800.0]);
        let tabs: Vec<_> = s.dock.iter_all_tabs().map(|(_, t)| t).collect();
        assert_eq!(tabs.len(), 2);
        let TabKind::Tree(p) = &tabs.iter().find(|t| t.id == 1).unwrap().kind else { panic!("tree tab") };
        assert_eq!(p.root, Root::Source(7));
        assert_eq!(p.expanded, HashSet::from([3, 5, 9]));
        assert_eq!(p.selected, Some(Sel::Dir(5)));
        assert_eq!(p.opts.sort, SortKey::Elsewhere);
        assert!(p.opts.descending);
        assert_eq!(p.opts.filter.copies, CopiesFilter::Unique);
        assert_eq!(p.scroll, 440.0);
        // the split survives: two leaves side by side
        assert_eq!(s.dock.main_surface().iter().filter(|n| n.is_leaf()).count(), 2);
        // a restored session can be saved and restored again
        assert!(Session::parse(&out(&s.dock)).is_ok());
    }

    #[test]
    fn unusable_sessions_are_refused() {
        let good = out(&sample());
        assert!(Session::parse("{").is_err());
        assert!(Session::parse(&good.replace("\"version\":1", "\"version\":99")).is_err());
        assert!(Session::parse(&good.replace("\"version\":1", "\"version\":0")).is_err());
        // two tabs with the same id
        assert!(Session::parse(&good.replace("\"id\":2", "\"id\":1")).is_err());
        // no tabs
        let empty: DockState<Tab> = DockState::new(vec![]);
        assert!(Session::parse(&out(&empty)).is_err());
    }

    #[test]
    fn a_focus_on_a_missing_tab_moves_to_a_tab_that_exists() {
        let text = out(&sample()).replace("\"focused\":2", "\"focused\":77");
        let s = Session::parse(&text).unwrap();
        assert!(s.focused.is_some_and(|f| f == 1 || f == 2));
    }

    #[test]
    fn missing_optional_fields_use_defaults() {
        let text = out(&sample());
        let mut v: serde_json::Value = serde_json::from_str(&text).unwrap();
        for k in ["focused", "inspector_open", "inspector_width", "inspected", "window"] {
            v.as_object_mut().unwrap().remove(k);
        }
        let s = Session::parse(&v.to_string()).unwrap();
        assert!(s.inspector_open);
        assert!(s.window.is_none() && s.inspected.is_none());
    }

    #[test]
    fn window_values_are_made_sane() {
        let w = WindowState { size: [100.0, 100.0], pos: Some([99999.0, 5.0]), maximized: true };
        let s = w.sane().unwrap();
        assert_eq!(s.size, MIN_WINDOW);
        assert_eq!(s.pos, None);
        assert!(s.maximized);
        assert!(WindowState { size: [f32::NAN, 800.0], pos: None, maximized: false }.sane().is_none());
        // a window on a monitor to the left of the main one keeps its position
        let left = WindowState { size: [1200.0, 800.0], pos: Some([-1900.0, 30.0]), maximized: false };
        assert_eq!(left.sane().unwrap().pos, Some([-1900.0, 30.0]));
    }

    #[test]
    fn saver_writes_the_newest_text_on_finish() {
        let dir = std::env::temp_dir().join(format!("dupemap-saver-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = SessionFile::beside(&dir.join("t.db"));
        let mut saver = Saver::spawn(file.clone());
        for i in 0..50 {
            saver.submit(format!("{{\"n\":{i}}}"));
        }
        saver.finish();
        assert!(!saver.failed());
        assert_eq!(file.read()[0], "{\"n\":49}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
