//! Ignored folders.
//!
//! Folders matching an enabled rule are left out of the index completely: they are not shown,
//! are not counted in any statistic, and their files do not count as copies of anything. This
//! is meant for installed software, system folders and app data, where duplicates are expected
//! and deleting them would break things.
//!
//! Patterns are matched against a folder's full path, case-insensitively:
//! * `C:\Espressif` — that folder (and so everything under it)
//! * `?:\Program Files` — `?` is any one character (here: any drive)
//! * `C:\$*` — `*` is any run of characters within one folder name
//! * `**\node_modules` — `**` is any number of folders, including none
//! * `/mnt/*/.zfs`, `gdrive:/Trash` — Linux and cloud paths work the same way
//!
//! A rule can apply to every source or to one source (host).

use crate::paths::fold_case;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IgnoreRule {
    pub pattern: String,
    /// Only this source (host); `None` = every source.
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Shipped with DupeMap (shown as a default in the editor).
    #[serde(default)]
    pub builtin: bool,
}

fn yes() -> bool {
    true
}

impl IgnoreRule {
    pub fn new(pattern: impl Into<String>, host: Option<String>) -> Self {
        IgnoreRule { pattern: pattern.into(), host, enabled: true, builtin: false }
    }

    fn builtin(pattern: &str, enabled: bool) -> Self {
        IgnoreRule { pattern: pattern.into(), host: None, enabled, builtin: true }
    }

    /// Why a pattern can't be used, if it can't.
    pub fn validate(pattern: &str) -> Result<(), String> {
        let segs = split_pattern(pattern);
        if segs.is_empty() {
            return Err("the pattern is empty".into());
        }
        if segs.iter().all(|s| s == "**") {
            return Err("the pattern would ignore everything".into());
        }
        if segs.len() > 63 {
            return Err("the pattern has too many folders".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IgnoreSettings {
    /// Master switch: when off, nothing is ignored.
    pub enabled: bool,
    pub rules: Vec<IgnoreRule>,
}

impl Default for IgnoreSettings {
    fn default() -> Self {
        IgnoreSettings { enabled: true, rules: default_rules() }
    }
}

impl IgnoreSettings {
    /// Nothing ignored (tests, tools that need every row).
    pub fn disabled() -> Self {
        IgnoreSettings { enabled: false, rules: Vec::new() }
    }

    /// Add the built-in rules that are missing (after an update), keeping the user's choices.
    pub fn merge_new_defaults(&mut self) {
        for d in default_rules() {
            if !self.rules.iter().any(|r| r.builtin && r.pattern == d.pattern) {
                self.rules.push(d);
            }
        }
    }

    /// Rules that take part in matching.
    pub fn active(&self) -> impl Iterator<Item = (usize, &IgnoreRule)> {
        let on = self.enabled;
        self.rules.iter().enumerate().filter(move |(_, r)| on && r.enabled && IgnoreRule::validate(&r.pattern).is_ok())
    }
}

/// System and software folders that are ignored out of the box, plus a few suggestions that
/// start switched off.
pub fn default_rules() -> Vec<IgnoreRule> {
    vec![
        IgnoreRule::builtin(r"?:\Windows", true),
        IgnoreRule::builtin(r"?:\Program Files", true),
        IgnoreRule::builtin(r"?:\Program Files (x86)", true),
        IgnoreRule::builtin(r"?:\ProgramData", true),
        IgnoreRule::builtin(r"?:\$*", true),
        IgnoreRule::builtin(r"?:\Recovery", true),
        IgnoreRule::builtin(r"?:\System Volume Information", true),
        IgnoreRule::builtin(r"**\AppData", true),
        IgnoreRule::builtin("/mnt/*/.zfs", true),
        IgnoreRule::builtin(r"**\node_modules", false),
        IgnoreRule::builtin(r"**\.git", false),
        IgnoreRule::builtin(r"**\__pycache__", false),
    ]
}

/// Split a pattern into folder segments the same way paths are split into an anchor plus
/// components: `C:\a\b` → `C:`, `a`, `b`; `/mnt/x` → `/`, `mnt`, `x`; `gdrive:/a` → `gdrive:`, `a`.
fn split_pattern(p: &str) -> Vec<String> {
    let p = p.trim();
    let parts = |s: &str| -> Vec<String> { s.split(['\\', '/']).filter(|x| !x.is_empty()).map(fold_case).collect() };
    if let Some(r) = p.strip_prefix('/') {
        let mut out = vec!["/".to_string()];
        out.extend(parts(r));
        return out;
    }
    if let Some(r) = p.strip_prefix(r"\\") {
        // UNC: \\server\share is one anchor, as in the folder tree
        let segs = parts(r);
        if segs.len() < 2 {
            return segs.first().map(|s| vec![format!(r"\\{s}")]).unwrap_or_default();
        }
        let mut out = vec![format!(r"\\{}\{}", segs[0], segs[1])];
        out.extend(segs.into_iter().skip(2));
        return out;
    }
    parts(p)
}

/// A compiled rule: segments plus the source it is limited to.
#[derive(Debug, Clone)]
pub(crate) struct Compiled {
    pub(crate) rule: usize,
    segs: Vec<String>,
    pub(crate) host: Option<String>,
}

pub(crate) fn compile(settings: &IgnoreSettings) -> Vec<Compiled> {
    settings
        .active()
        .map(|(i, r)| Compiled { rule: i, segs: split_pattern(&r.pattern), host: r.host.clone() })
        .collect()
}

impl Compiled {
    /// Pattern positions reachable before any folder is consumed.
    pub(crate) fn start(&self) -> u64 {
        self.closure(1)
    }

    fn closure(&self, mut s: u64) -> u64 {
        loop {
            let mut n = s;
            for (i, seg) in self.segs.iter().enumerate() {
                if s & (1 << i) != 0 && seg == "**" {
                    n |= 1 << (i + 1);
                }
            }
            if n == s {
                return s;
            }
            s = n;
        }
    }

    /// Positions after consuming one folder name (already case-folded).
    pub(crate) fn step(&self, s: u64, name_folded: &str) -> u64 {
        let mut n = 0u64;
        for (i, seg) in self.segs.iter().enumerate() {
            if s & (1 << i) == 0 {
                continue;
            }
            if seg == "**" {
                n |= 1 << i;
            } else if glob(seg, name_folded) {
                n |= 1 << (i + 1);
            }
        }
        self.closure(n)
    }

    /// Does the folder reached with these positions match the whole pattern?
    pub(crate) fn accepts(&self, s: u64) -> bool {
        s & (1 << self.segs.len()) != 0
    }
}

/// `*` and `?` wildcards within one name.
fn glob(pat: &str, name: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let (mut pi, mut ni) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Test a pattern against a full folder path directly (used by the settings editor).
pub fn pattern_matches_path(pattern: &str, path: &str) -> bool {
    let c = Compiled { rule: 0, segs: split_pattern(pattern), host: None };
    if c.segs.is_empty() {
        return false;
    }
    let mut s = c.start();
    for seg in split_pattern(path) {
        s = c.step(s, &seg);
        if s == 0 {
            return false;
        }
    }
    c.accepts(s)
}

/// Per-rule effect, reported by the index.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleEffect {
    /// Folders matched by the rule (each one hides its whole subtree).
    pub folders: u64,
    /// Files hidden (files under a folder matched by several rules count for the first).
    pub files: u64,
    pub bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitting() {
        assert_eq!(split_pattern(r"C:\Espressif\tools"), ["c:", "espressif", "tools"]);
        assert_eq!(split_pattern("/mnt/*/.zfs"), ["/", "mnt", "*", ".zfs"]);
        assert_eq!(split_pattern(r"**\AppData"), ["**", "appdata"]);
        assert_eq!(split_pattern("gdrive:/Trash"), ["gdrive:", "trash"]);
        assert_eq!(split_pattern(r"\\NAS\Media\Old"), [r"\\nas\media", "old"]);
    }

    #[test]
    fn matching() {
        let m = pattern_matches_path;
        assert!(m(r"C:\Espressif", r"C:\Espressif"));
        assert!(m(r"c:\espressif", r"C:\Espressif"));
        assert!(
            !m(r"C:\Espressif", r"C:\Espressif\tools"),
            "a rule names folders; subfolders are hidden through their parent"
        );
        assert!(!m(r"C:\Espressif", r"D:\Espressif"));
        assert!(m(r"?:\Program Files", r"D:\Program Files"));
        assert!(m(r"?:\$*", r"C:\$WinREAgent"));
        assert!(m(r"?:\$*", r"C:\$Recycle.Bin"));
        assert!(!m(r"?:\$*", r"C:\Users\$x"));
        assert!(m(r"**\AppData", r"C:\Users\Alex\AppData"));
        assert!(m(r"**\node_modules", r"D:\proj\web\node_modules"));
        assert!(m(r"**\node_modules", "/home/x/node_modules"));
        assert!(m(r"C:\Users\**\Cache", r"C:\Users\Cache"), "** may match no folders");
        assert!(m(r"C:\Users\**\Cache", r"C:\Users\a\b\Cache"));
        assert!(m("/mnt/*/.zfs", "/mnt/tank/.zfs"));
        assert!(!m("/mnt/*/.zfs", "/mnt/tank/media/.zfs"));
        assert!(m("gdrive:/Trash", "gdrive:/Trash"));
        assert!(m(r"**\*.app", r"C:\Apps\Foo.app"));
        assert!(m("**/build-*", "/src/proj/build-release"));
    }

    #[test]
    fn validation_and_defaults() {
        assert!(IgnoreRule::validate("").is_err());
        assert!(IgnoreRule::validate("**").is_err());
        assert!(IgnoreRule::validate(r"**\**").is_err());
        assert!(IgnoreRule::validate(r"C:\x").is_ok());
        let d = IgnoreSettings::default();
        assert!(d.enabled);
        assert!(d.rules.iter().all(|r| IgnoreRule::validate(&r.pattern).is_ok()));
        assert_eq!(d.active().count(), d.rules.iter().filter(|r| r.enabled).count());
        let mut s = IgnoreSettings { enabled: true, rules: vec![IgnoreRule::new(r"C:\Espressif", None)] };
        s.merge_new_defaults();
        assert_eq!(s.rules.len(), 1 + default_rules().len());
        s.merge_new_defaults();
        assert_eq!(s.rules.len(), 1 + default_rules().len(), "merging twice adds nothing");
        assert_eq!(IgnoreSettings::disabled().active().count(), 0);
    }

    #[test]
    fn serde_round_trip_with_defaults() {
        let s: IgnoreSettings = serde_json::from_str(r#"{"enabled":true,"rules":[{"pattern":"C:\\x"}]}"#).unwrap();
        assert_eq!(s.rules[0], IgnoreRule::new(r"C:\x", None));
    }
}
