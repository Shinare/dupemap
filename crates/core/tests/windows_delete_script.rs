//! Windows only: the PowerShell delete script on real files — dry run, the Recycle Bin, moving,
//! deleting, a path over 260 characters, awkward names, and the files it must leave alone.
//! (CI runs this on windows-latest.)

#[cfg(windows)]
#[test]
fn the_powershell_delete_script_removes_exactly_what_it_should() {
    use dupemap_core::db::Db;
    use dupemap_core::export::{plan, write, ExportOptions, WindowsMode, Written};
    use dupemap_core::index::Index;
    use dupemap_core::marks::{resolve, MarkAction, MarkRule, MarkState};
    use dupemap_core::scan::{list_volumes, scan_volume, ScanSettings, Volume};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicBool;

    // under target\tmp (not under AppData, which the scan skips)
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("dupemap-del-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let data = dir.join("data");
    std::fs::create_dir_all(data.join("sub dir")).unwrap();
    // a folder chain that takes the path past 260 characters
    let mut deep = data.clone();
    for i in 0..6 {
        deep = deep.join(format!("a rather long folder name number {i} for the test"));
    }
    std::fs::create_dir_all(&deep).unwrap();
    let long = deep.join("long path file.bin");
    assert!(long.to_string_lossy().len() > 260, "the long path is long");
    let files: Vec<(PathBuf, u8)> = vec![
        (data.join("a.bin"), 1),
        (data.join("sub dir").join("b c.bin"), 2),
        (data.join("it's $HOME [1].bin"), 3),
        (data.join("żółw.bin"), 4),
        (long.clone(), 5),
        (data.join("changed.bin"), 6),
        (data.join("gone.bin"), 7),
        (data.join("same size.bin"), 8),
        (data.join("recycle me.bin"), 9),
    ];
    for (p, b) in &files {
        std::fs::write(p, vec![*b; 3000]).unwrap();
    }
    let letter = data.to_string_lossy().chars().next().unwrap().to_ascii_uppercase();
    let drive = list_volumes()
        .into_iter()
        .find(|v| v.root.to_string_lossy().to_ascii_uppercase().starts_with(&format!("{letter}:")))
        .expect("the drive of the test folder is listed");
    let scanned = scan_volume(
        &Volume::folder(&data, &drive.host),
        &dir.join("scans"),
        &ScanSettings { min_size: 1000, ..Default::default() },
        false,
        &mut |_| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    let mut db = Db::open_in_memory().unwrap();
    db.import_file(&scanned.manifest, &mut |_| true).unwrap();
    let ix = Index::build(&db).unwrap();
    let mark = |p: &Path| MarkRule {
        host: drive.host.clone(),
        path: p.display().to_string(),
        is_dir: p.is_dir(),
        action: MarkAction::Delete,
    };
    let rules = vec![mark(&data)];
    let places = resolve(&ix, &db, &rules).unwrap();
    assert!(places[0].is_some(), "the folder rule finds the folder");
    let st = MarkState::compute(&ix, &rules, &places);
    assert_eq!(st.total.files, files.len() as u64);
    let plans = plan(&ix, &db, &st, true).unwrap();
    // after the scan: another size, gone, other content of the same size
    std::fs::write(data.join("changed.bin"), vec![6u8; 10]).unwrap();
    std::fs::remove_file(data.join("gone.bin")).unwrap();
    std::fs::write(data.join("same size.bin"), vec![10u8; 3000]).unwrap();

    let work = dir.join("work");
    let run = |opts: &ExportOptions, args: &[&str], env: Option<(&str, &Path)>| -> String {
        let _ = std::fs::remove_dir_all(&work);
        let written = write(&plans, opts, &work, "now").unwrap();
        let script = written.iter().find(|w| w.what == Written::PowerShell).unwrap();
        let mut cmd = std::process::Command::new("powershell.exe");
        cmd.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"]).arg(&script.path).args(args);
        if let Some((k, v)) = env {
            cmd.env(k, v);
        }
        let out = cmd.output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(out.status.success(), "{text}\n{}", String::from_utf8_lossy(&out.stderr));
        text
    };
    let exists = |p: &Path| std::fs::symlink_metadata(p).is_ok();

    // a dry run changes nothing
    let opts = ExportOptions { windows: WindowsMode::Delete, check_hash: true, ..Default::default() };
    let text = run(&opts, &["-DryRun", "-Yes"], None);
    assert!(text.contains("Delete for good: 7 files"), "{text}");
    assert!(text.contains("1 are not there any more"));
    assert!(text.contains("1 have another size"));
    assert!(files.iter().filter(|(p, _)| !p.ends_with("gone.bin")).all(|(p, _)| exists(p)));

    // the Recycle Bin, for one file
    let one = vec![dupemap_core::export::SourcePlan {
        files: plans[0]
            .files
            .iter()
            .filter(|f| String::from_utf8_lossy(&f.rel).ends_with("recycle me.bin"))
            .cloned()
            .collect(),
        ..plans[0].clone()
    }];
    let written = write(&one, &ExportOptions::default(), &dir.join("work-rb"), "now").unwrap();
    let script = written.iter().find(|w| w.what == Written::PowerShell).unwrap();
    let out = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&script.path)
        .arg("-Yes")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}\n{}", String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("Done: 1 files"), "{text}");
    assert!(!exists(&data.join("recycle me.bin")));

    // delete for good, with the hash check: same size.bin changed and stays
    let text = run(&opts, &["-Yes"], None);
    assert!(text.contains("Done: 5 files"), "{text}");
    assert!(text.contains("1 had another SHA-256"));
    for gone in [
        data.join("a.bin"),
        data.join("sub dir").join("b c.bin"),
        data.join("it's $HOME [1].bin"),
        data.join("żółw.bin"),
        long.clone(),
    ] {
        assert!(!exists(&gone), "{} should be gone", gone.display());
    }
    for stays in ["changed.bin", "same size.bin"] {
        assert!(exists(&data.join(stays)), "{stays} should stay");
    }
    let log = std::fs::read_to_string(
        work.join(format!("dupemap-delete-{}.log", dupemap_core::export::file_stem(&drive.host))),
    )
    .unwrap();
    assert!(log.contains("Delete") && log.contains("another SHA-256"), "{log}");

    // move: same size.bin goes into "_DupeMap deleted" (no hash check now)
    let moved = dir.join("moved");
    std::fs::create_dir_all(&moved).unwrap();
    let text = run(
        &ExportOptions { windows: WindowsMode::Move, ..Default::default() },
        &["-Yes"],
        Some(("DUPEMAP_MOVE_ROOT", &moved)),
    );
    assert!(text.contains("Done: 1 files"), "{text}");
    assert!(!exists(&data.join("same size.bin")));
    let stamp = std::fs::read_dir(moved.join("_DupeMap deleted")).unwrap().next().unwrap().unwrap().path();
    let rest = data.join("same size.bin").to_string_lossy()[3..].to_string();
    assert!(stamp.join(rest).is_file(), "moved into _DupeMap deleted");

    // another drive label: refuses to start
    let mut other = plans.clone();
    other[0].host = format!("{} {letter}: (Some Other Disk)", dupemap_core::scan::computer_name());
    let written = write(&other, &ExportOptions::default(), &dir.join("work-other"), "now").unwrap();
    let script = written.iter().find(|w| w.what == Written::PowerShell).unwrap();
    let out = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&script.path)
        .arg("-Yes")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("but these files were scanned on 'Some Other Disk'"));
    let _ = std::fs::remove_dir_all(&dir);
}
