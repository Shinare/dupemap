//! Requests from panes and windows, carried out by the app after the frame is drawn.

use crate::tree::Root;
use dupemap_core::ignore::{IgnoreRule, IgnoreSettings};
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
