//! Scan scripts for other computers: the same scan as the app's, as a PowerShell script for
//! Windows or a bash script for Linux, with the drive kinds and minimum size chosen in the app.
//! Their manifests use the same source names and skip rules as the app's own scan.

use crate::scan::DriveKind;
use serde::{Deserialize, Serialize};

const WINDOWS: &str = include_str!("scripts/scan-windows.ps1");
const LINUX: &str = include_str!("scripts/scan-linux.sh");

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ScriptOs {
    #[default]
    Windows,
    Linux,
}

impl ScriptOs {
    pub fn label(self) -> &'static str {
        match self {
            ScriptOs::Windows => "Windows",
            ScriptOs::Linux => "Linux (TrueNAS SCALE, a Linux PC...)",
        }
    }

    /// Suggested file name for the script.
    pub fn file_name(self) -> &'static str {
        match self {
            ScriptOs::Windows => "dupemap-scan-windows.ps1",
            ScriptOs::Linux => "dupemap-scan-linux.sh",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptOptions {
    pub os: ScriptOs,
    /// Drive kinds to scan (in this order in the manifest name).
    pub kinds: Vec<DriveKind>,
    pub min_size: u64,
}

fn kind_word(k: DriveKind) -> &'static str {
    match k {
        DriveKind::Internal => "internal",
        DriveKind::Usb => "usb",
        DriveKind::Network => "network",
    }
}

fn kind_text(k: DriveKind) -> &'static str {
    match k {
        DriveKind::Internal => "own drives",
        DriveKind::Usb => "USB drives",
        DriveKind::Network => "network drives",
    }
}

/// `1 MB`, `100 KB`, `250000 bytes`.
pub fn size_text(n: u64) -> String {
    if n >= 1 << 20 && n % (1 << 20) == 0 {
        format!("{} MB", n >> 20)
    } else if n >= 1 << 10 && n % (1 << 10) == 0 {
        format!("{} KB", n >> 10)
    } else {
        format!("{n} bytes")
    }
}

/// `own drives`, `own drives and USB drives`, `own drives, USB drives and network drives`.
fn kinds_text(kinds: &[DriveKind]) -> String {
    let words: Vec<&str> = kinds.iter().map(|&k| kind_text(k)).collect();
    match words.len() {
        0 => "drives".into(),
        1 => words[0].into(),
        n => format!("{} and {}", words[..n - 1].join(", "), words[n - 1]),
    }
}

/// The script text, ready to save (CRLF line endings and a BOM for Windows PowerShell 5.1,
/// which reads a file without BOM in the ANSI code page).
pub fn generate(o: &ScriptOptions) -> Vec<u8> {
    let mut kinds = o.kinds.clone();
    kinds.sort();
    kinds.dedup();
    let words: Vec<&str> = kinds.iter().map(|&k| kind_word(k)).collect();
    let (template, sep) = match o.os {
        ScriptOs::Windows => (WINDOWS, ","),
        ScriptOs::Linux => (LINUX, " "),
    };
    let text = template
        .replace("@@VERSION@@", env!("CARGO_PKG_VERSION"))
        .replace("@@KINDS@@", &words.join(sep))
        .replace("@@KINDS_TEXT@@", &kinds_text(&kinds))
        .replace("@@WHAT@@", &words.join("+"))
        .replace("@@MIN_SIZE_TEXT@@", &size_text(o.min_size))
        .replace("@@MIN_SIZE@@", &o.min_size.to_string())
        .replace("@@FILE_NAME@@", o.os.file_name());
    match o.os {
        ScriptOs::Windows => {
            let mut out = vec![0xEF, 0xBB, 0xBF];
            out.extend_from_slice(text.replace("\r\n", "\n").replace('\n', "\r\n").as_bytes());
            out
        }
        ScriptOs::Linux => text.replace("\r\n", "\n").into_bytes(),
    }
}

/// The manifest name the script will write on a computer called `computer`.
pub fn manifest_name(o: &ScriptOptions, computer: &str) -> String {
    let mut kinds = o.kinds.clone();
    kinds.sort();
    kinds.dedup();
    let words: Vec<&str> = kinds.iter().map(|&k| kind_word(k)).collect();
    format!("manifest-{computer}-{}.tsv", words.join("+"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(os: ScriptOs) -> ScriptOptions {
        ScriptOptions { os, kinds: vec![DriveKind::Usb, DriveKind::Internal], min_size: 1 << 20 }
    }

    #[test]
    fn placeholders_are_filled() {
        for os in [ScriptOs::Windows, ScriptOs::Linux] {
            let t = String::from_utf8(generate(&opts(os))).unwrap();
            assert!(!t.contains("@@"), "{os:?} has an unfilled placeholder");
            assert!(t.contains("own drives and USB drives"));
            assert!(t.contains("1 MB"));
            assert!(t.contains("internal+usb"));
        }
        let w = generate(&opts(ScriptOs::Windows));
        assert!(w.starts_with(&[0xEF, 0xBB, 0xBF]), "BOM for PowerShell 5.1");
        let w = String::from_utf8(w[3..].to_vec()).unwrap();
        assert!(w.contains("[string]$Kinds = 'internal,usb'"));
        assert!(w.contains("[long]$MinSize = 1048576"));
        assert!(!w.replace("\r\n", "").contains('\n'), "CRLF throughout");
        let l = String::from_utf8(generate(&opts(ScriptOs::Linux))).unwrap();
        assert!(l.starts_with("#!/bin/bash\n"));
        assert!(l.contains("KINDS=${KINDS:-\"internal usb\"}"));
        assert!(!l.contains('\r'));
        assert_eq!(manifest_name(&opts(ScriptOs::Linux), "nas"), "manifest-nas-internal+usb.tsv");
        assert_eq!(size_text(100 << 10), "100 KB");
        assert_eq!(size_text(1000), "1000 bytes");
    }

    /// The bash script and the built-in scan write the same lines for the same folder.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_linux_script_writes_what_the_app_writes() {
        use crate::scan::{scan_volume, ScanSettings, Volume};
        use std::sync::atomic::AtomicBool;
        let dir = std::env::temp_dir().join(format!("dupemap-script-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let data = dir.join("data");
        let work = dir.join("work");
        std::fs::create_dir_all(data.join("sub dir/deeper")).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(data.join("a.bin"), vec![1u8; 3000]).unwrap();
        std::fs::write(data.join("sub dir/b c.bin"), vec![2u8; 5000]).unwrap();
        std::fs::write(data.join("sub dir/deeper/back\\slash.bin"), vec![3u8; 4000]).unwrap();
        std::fs::write(data.join("small.txt"), b"tiny").unwrap();
        let o = ScriptOptions { os: ScriptOs::Linux, kinds: vec![DriveKind::Internal], min_size: 2000 };
        let script = dir.join("scan.sh");
        std::fs::write(&script, generate(&o)).unwrap();
        let run = || {
            let st = std::process::Command::new("bash")
                .arg(&script)
                .arg(&data)
                .current_dir(&work)
                .stdout(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(st.success());
        };
        run();
        let host = crate::scan::computer_name();
        let out = work.join(format!("manifest-{host}-folders.tsv"));
        let mut got: Vec<String> = std::fs::read_to_string(&out).unwrap().lines().map(String::from).collect();
        // the app's scan of the same folder
        let vol = Volume::folder(&data, &host);
        let r = scan_volume(
            &vol,
            &dir.join("app"),
            &ScanSettings { min_size: 2000, ..Default::default() },
            false,
            &mut |_| {},
            &AtomicBool::new(false),
        )
        .unwrap();
        let mut want: Vec<String> = std::fs::read_to_string(&r.manifest).unwrap().lines().map(String::from).collect();
        got.sort();
        want.sort();
        assert_eq!(got, want);
        assert_eq!(got.len(), 4, "header and three files");
        // running it again continues: nothing is added twice
        run();
        assert_eq!(std::fs::read_to_string(&out).unwrap().lines().count(), 4);
        // a line cut off by an interruption is redone
        let text = std::fs::read_to_string(&out).unwrap();
        let cut = &text[..text.len() - 20];
        std::fs::write(&out, cut).unwrap();
        run();
        let mut again: Vec<String> = std::fs::read_to_string(&out).unwrap().lines().map(String::from).collect();
        again.sort();
        assert_eq!(again, want);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
