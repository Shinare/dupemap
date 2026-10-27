//! Windows only: the generated PowerShell scan script writes the same manifest lines as the
//! app's built-in scan, and continues an interrupted run. (CI runs this on windows-latest.)

#[cfg(windows)]
#[test]
fn the_windows_script_writes_what_the_app_writes() {
    use dupemap_core::scan::{list_volumes, scan_volume, DriveKind, ScanSettings, Volume};
    use dupemap_core::scripts::{generate, ScriptOptions, ScriptOs};
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;

    // under target\tmp (not under AppData, which both scans skip)
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("dupemap-ps-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let data = dir.join("data");
    std::fs::create_dir_all(data.join("sub dir").join("głęboko")).unwrap();
    std::fs::write(data.join("a.bin"), vec![1u8; 3000]).unwrap();
    std::fs::write(data.join("sub dir").join("b c.bin"), vec![2u8; 5000]).unwrap();
    std::fs::write(data.join("sub dir").join("głęboko").join("żółw.bin"), vec![3u8; 4000]).unwrap();
    std::fs::write(data.join("small.txt"), b"tiny").unwrap();

    let o = ScriptOptions { os: ScriptOs::Windows, kinds: vec![DriveKind::Internal], min_size: 2000 };
    let script = dir.join("scan.ps1");
    std::fs::write(&script, generate(&o)).unwrap();
    let out = dir.join("manifest-script.tsv");
    let run = || {
        let st = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(&script)
            .arg("-Paths")
            .arg(&data)
            .arg("-Out")
            .arg(&out)
            .arg("-NoElevate")
            .status()
            .unwrap();
        assert!(st.success());
    };
    run();

    // the app's scan of the same folder, with the source name of its drive
    let letter = data.to_string_lossy().chars().next().unwrap().to_ascii_uppercase();
    let drive = list_volumes()
        .into_iter()
        .find(|v| v.root.to_string_lossy().to_ascii_uppercase().starts_with(&format!("{letter}:")))
        .expect("the drive of the test folder is listed");
    let vol = Volume::folder(&data, &drive.host);
    let r = scan_volume(
        &vol,
        &dir.join("app"),
        &ScanSettings { min_size: 2000, ..Default::default() },
        false,
        &mut |_| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    let read = |p: &std::path::Path| -> Vec<String> {
        let t = std::fs::read_to_string(p).unwrap();
        let mut v: Vec<String> = t.trim_start_matches('\u{FEFF}').lines().map(String::from).collect();
        v.sort();
        v
    };
    let want = read(&r.manifest);
    assert_eq!(want.len(), 4, "header and three files: {want:?}");
    assert_eq!(read(&out), want);

    // run again: continues, nothing twice
    run();
    assert_eq!(read(&out), want);
    // a cut-off last line is redone
    let text = std::fs::read(&out).unwrap();
    std::fs::write(&out, &text[..text.len() - 20]).unwrap();
    run();
    assert_eq!(read(&out), want);
    let _ = std::fs::remove_dir_all(&dir);
}
