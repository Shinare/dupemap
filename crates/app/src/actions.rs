//! Requests from panes and windows, carried out by the app after the frame is drawn.

use crate::selection::Target;
use crate::tree::Root;
use dupemap_core::ignore::{IgnoreRule, IgnoreSettings};
use dupemap_core::index::{DirIx, FileIx};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub enum Action {
    CopyText(String),
    /// Add one ignore rule and rebuild.
    AddIgnoreRule(IgnoreRule),
    /// Replace all ignore settings and rebuild.
    SetIgnore(IgnoreSettings),
    OpenIgnoreEditor,
    OpenManifests,
    /// Show the import dialog.
    Import,
    ImportPaths(Vec<PathBuf>),
    RemoveManifests(Vec<i64>),
    ShowReport(i64),
    SetAlertDismissed(i64, bool),
    NewTab(Root),
    /// A pane became the keyboard target.
    Focus(u64),
    /// Show a file or folder in Windows Explorer.
    Reveal(String),
    /// A message for the status bar.
    Note(String),
    /// Show this in the inspector and highlight its copies in every tab (`None` clears).
    Inspect(Option<Target>),
    /// Open the tree at this file (expanding folders) in the best tab for its source.
    ShowInTab(FileIx),
    /// Open the tree at this folder.
    ShowDirInTab(DirIx),
    /// Open (or switch to) the duplicate groups tab.
    OpenGroups,
    ToggleInspector,
    OpenInspector,
    /// Open a summary tab for one source or all of them (or switch to one already open).
    OpenSummary(Root),
    /// Open (or switch to) the treemap tab, at a folder (database id) or every source.
    OpenTreemap(Option<i64>),
    /// Open the category editor.
    OpenCategories,
    /// Store new file-type categories.
    SetCategories(dupemap_core::summary::Categories),
    /// Import one file in a given way (a checksum list with its source).
    ImportAs(PathBuf, dupemap_core::db::ImportAs),
    /// Open (or switch to) the search tab.
    OpenSearch,
    /// The "Scan this PC" window.
    OpenScan,
    /// The "Scan another computer" window.
    OpenRemoteScan,
    StartScan(Vec<crate::worker::ScanTarget>, dupemap_core::scan::ScanSettings),
    /// Start DupeMap again as administrator (for the scan).
    RestartElevated,
    /// Stop the running job (a scan keeps what it hashed).
    CancelJob,
    /// Change the marks for deletion: add rules (replacing any on the same places), remove
    /// others. `label` names the change for undo ("mark D:\Backup").
    Mark {
        label: String,
        add: Vec<dupemap_core::marks::MarkRule>,
        remove: Vec<dupemap_core::marks::PlaceKey>,
    },
    Undo,
    Redo,
    /// Open (or switch to) the Marks tab.
    OpenMarks,
    /// Open the window that writes the delete scripts.
    OpenExport,
    /// Write the delete scripts.
    Export(crate::marks_ui::ExportChoice),
}

/// Whether a manifest host names this computer ("PC1" or "PC1 D: (Media)" on PC1).
pub fn is_this_computer(host: &str) -> bool {
    let Some(name) = computer_name() else { return false };
    let h = host.trim().to_lowercase();
    let n = name.to_lowercase();
    h == n || h.strip_prefix(&n).is_some_and(|rest| rest.starts_with(' '))
}

fn computer_name() -> Option<String> {
    std::env::var("COMPUTERNAME").ok().or_else(|| std::env::var("HOSTNAME").ok()).filter(|s| !s.is_empty())
}

/// Open Explorer with the item selected.
pub fn reveal(path: &str) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // explorer parses its own command line: /select,"<path>" must reach it verbatim
        std::process::Command::new("explorer.exe").raw_arg(format!("/select,\"{path}\"")).spawn()?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Err(std::io::Error::other("only available on Windows"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_matching() {
        std::env::set_var("COMPUTERNAME", "PC1");
        assert!(is_this_computer("PC1"));
        assert!(is_this_computer("pc1"));
        assert!(is_this_computer("PC1 D: (Media)"));
        assert!(!is_this_computer("PC10"));
        assert!(!is_this_computer("PC2 D: (Media)"));
        assert!(!is_this_computer("gdrive:"));
    }
}
