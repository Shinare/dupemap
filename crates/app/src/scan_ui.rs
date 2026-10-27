//! "Scan this PC": pick drives by kind (this computer's own, USB, network), hash them into
//! manifests and import them, with live progress. "Scan another computer": save a script that
//! does the same scan on another Windows or Linux computer.

use crate::actions::Action;
use crate::colors;
use crate::format;
use crate::worker::{ScanDone, ScanStatus, ScanTarget};
use dupemap_core::scan::{list_volumes, DriveKind, Phase, ScanSettings, Volume, DEFAULT_MIN_SIZE};
use dupemap_core::scripts::{self, ScriptOptions, ScriptOs};
use egui::{RichText, Ui};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Minimum sizes offered (the scripts' default is 1 MB).
const MIN_SIZES: [(u64, &str); 4] =
    [(100 << 10, "100 KB"), (DEFAULT_MIN_SIZE, "1 MB (as the scripts)"), (10 << 20, "10 MB"), (100 << 20, "100 MB")];

fn min_size_text(n: u64) -> String {
    MIN_SIZES.iter().find(|(v, _)| *v == n).map(|(_, t)| t.to_string()).unwrap_or_else(|| format::bytes(n))
}

pub struct ScanWindow {
    pub open: bool,
    /// Drives, listed on a thread (a sleeping network drive can take seconds to answer).
    volumes: Arc<Mutex<Option<Vec<Volume>>>>,
    listing: bool,
    /// Ticked drives, by root.
    chosen: BTreeSet<PathBuf>,
    /// The first listing ticks the defaults; later ones keep the choice.
    ticked_defaults: bool,
    settings: ScanSettings,
    start_over: bool,
    show_failures: bool,
    /// What the start button scans (worked out while drawing the list).
    targets: Vec<ScanTarget>,
}

impl Default for ScanWindow {
    fn default() -> Self {
        ScanWindow {
            open: false,
            volumes: Arc::new(Mutex::new(None)),
            listing: false,
            chosen: BTreeSet::new(),
            ticked_defaults: false,
            settings: ScanSettings::default(),
            start_over: false,
            show_failures: false,
            targets: Vec::new(),
        }
    }
}

/// What the app tells the window while drawing it.
pub struct ScanCtx<'a> {
    /// A scan running now.
    pub running: Option<&'a ScanStatus>,
    /// Another job runs (an import...): a scan has to wait.
    pub busy: bool,
    pub cancelling: bool,
    pub last: &'a [ScanDone],
    /// Windows only: DupeMap runs as administrator.
    pub elevated: bool,
    pub out_dir: &'a Path,
    pub actions: &'a mut Vec<Action>,
}

impl ScanWindow {
    pub fn open(&mut self, ctx: &egui::Context) {
        self.open = true;
        self.refresh(ctx);
    }

    /// Still looking for drives (the screenshots wait for the list).
    pub fn is_listing(&self) -> bool {
        self.open && self.volumes.lock().map(|v| v.is_none()).unwrap_or(false)
    }

    fn refresh(&mut self, ctx: &egui::Context) {
        if self.listing {
            return;
        }
        self.listing = true;
        let slot = self.volumes.clone();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("dupemap-drives".into())
            .spawn(move || {
                let v = list_volumes();
                if let Ok(mut s) = slot.lock() {
                    *s = Some(v);
                }
                ctx.request_repaint();
            })
            .ok();
    }

    pub fn show(&mut self, ctx: &egui::Context, cx: &mut ScanCtx) {
        if !self.open {
            return;
        }
        let mut open = true;
        egui::Window::new("Scan this PC")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(640.0)
            .default_height(600.0)
            .show(ctx, |ui| {
                if let Some(st) = cx.running {
                    self.progress(ui, st, cx);
                    return;
                }
                // the start button stays in view below the list
                let footer = 64.0;
                egui::ScrollArea::vertical()
                    .auto_shrink([false, true])
                    .max_height((ui.available_height() - footer).max(160.0))
                    .show(ui, |ui| self.body(ui, ctx, cx));
                ui.separator();
                self.footer(ui, cx);
            });
        self.open = open;
    }

    fn body(&mut self, ui: &mut Ui, ctx: &egui::Context, cx: &mut ScanCtx) {
        self.targets.clear();
        let volumes = self.volumes.lock().ok().and_then(|v| v.clone());
        if volumes.is_some() {
            self.listing = false;
        }
        ui.label(
            RichText::new(
                "DupeMap hashes the files on the drives you tick, one drive after another, and imports what it \
                 finds (the same as running the export script on this computer). It only reads files.",
            )
            .color(colors::INK_SECONDARY),
        );
        ui.add_space(6.0);
        if cfg!(windows) && !cx.elevated {
            egui::Frame::new()
                .fill(colors::alpha(colors::WARNING, 0.12))
                .stroke(egui::Stroke::new(1.0, colors::alpha(colors::WARNING, 0.6)))
                .corner_radius(4)
                .inner_margin(egui::Margin::symmetric(10, 8))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(
                        RichText::new(
                            "DupeMap is not running as administrator, so folders only an administrator can read \
                             (other users' files, some program data) would be skipped.",
                        )
                        .color(colors::INK),
                    );
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(
                                !cx.busy,
                                egui::Button::new(RichText::new("Restart as administrator").strong()),
                            )
                            .on_hover_text("Windows asks for permission; DupeMap opens again with this window")
                            .clicked()
                        {
                            cx.actions.push(Action::RestartElevated);
                        }
                        ui.label(RichText::new("or scan without").color(colors::INK_MUTED));
                    });
                });
            ui.add_space(6.0);
        }
        let Some(volumes) = volumes else {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new());
                ui.label(RichText::new("Looking for drives…").color(colors::INK_MUTED));
            });
            return;
        };
        if !self.ticked_defaults {
            self.ticked_defaults = true;
            self.chosen = volumes
                .iter()
                .filter(|v| v.kind == DriveKind::Internal && v.caution.is_none())
                .map(|v| v.root.clone())
                .collect();
        }
        for kind in DriveKind::ALL {
            let of_kind: Vec<&Volume> = volumes.iter().filter(|v| v.kind == kind).collect();
            let ticked = of_kind.iter().filter(|v| self.chosen.contains(&v.root)).count();
            let mut all = !of_kind.is_empty() && ticked == of_kind.len();
            let r = ui.add_enabled(
                !of_kind.is_empty(),
                egui::Checkbox::new(&mut all, RichText::new(kind.label()).strong())
                    .indeterminate(ticked > 0 && ticked < of_kind.len()),
            );
            if r.clicked() {
                for v in &of_kind {
                    if all && v.caution.is_none() {
                        self.chosen.insert(v.root.clone());
                    } else {
                        self.chosen.remove(&v.root);
                    }
                }
            }
            ui.indent(("scan-kind", kind as u8), |ui| {
                if of_kind.is_empty() {
                    ui.label(RichText::new("none found").color(colors::INK_MUTED));
                }
                for v in of_kind {
                    let mut on = self.chosen.contains(&v.root);
                    ui.horizontal(|ui| {
                        if ui.checkbox(&mut on, &v.display).changed() {
                            if on {
                                self.chosen.insert(v.root.clone());
                            } else {
                                self.chosen.remove(&v.root);
                            }
                        }
                        let mut facts = Vec::new();
                        if let Some(u) = v.used_bytes() {
                            facts.push(format!("{} used", format::bytes(u)));
                        }
                        match v.rotational {
                            Some(true) => facts.push("spinning disk".into()),
                            Some(false) if v.kind != DriveKind::Network => facts.push("SSD".into()),
                            _ => {}
                        }
                        if !v.fs.is_empty() {
                            facts.push(v.fs.clone());
                        }
                        ui.label(RichText::new(facts.join(" · ")).small().color(colors::INK_MUTED));
                        if v.partial_path(cx.out_dir).is_file() {
                            ui.label(RichText::new("⟳ interrupted scan").small().color(colors::WARNING)).on_hover_text(
                                "An earlier scan of this drive was stopped; it continues where it stopped",
                            );
                        }
                    });
                    if let Some(c) = &v.caution {
                        ui.label(RichText::new(format!("⚠ {c}")).small().color(colors::WARNING));
                    }
                    if v.write_root.is_some() {
                        ui.label(
                            RichText::new("Mapped in your normal session; reached through its share name.")
                                .small()
                                .color(colors::INK_MUTED),
                        );
                    }
                }
            });
            ui.add_space(4.0);
        }
        if ui.small_button("Look for drives again").clicked() {
            self.refresh(ctx);
        }
        ui.add_space(8.0);
        egui::CollapsingHeader::new("Options").default_open(false).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label("Smallest file to hash");
                egui::ComboBox::from_id_salt("scan-min").selected_text(min_size_text(self.settings.min_size)).show_ui(
                    ui,
                    |ui| {
                        for (v, t) in MIN_SIZES {
                            ui.selectable_value(&mut self.settings.min_size, v, t);
                        }
                    },
                );
            });
            ui.checkbox(
                &mut self.settings.skip_system,
                "Skip system folders (Windows, Program Files, ProgramData, AppData)",
            );
            ui.checkbox(
                &mut self.settings.skip_cloud_folders,
                "Skip OneDrive and Google Drive folders in user profiles",
            )
            .on_hover_text(
                "Their files can be online-only. Online-only files are never downloaded: they are always skipped.",
            );
            ui.checkbox(&mut self.start_over, "Start interrupted scans over instead of continuing them");
        });
        self.targets = volumes
            .iter()
            .filter(|v| self.chosen.contains(&v.root))
            .map(|v| ScanTarget { volume: v.clone(), resume: !self.start_over })
            .collect();
        if !cx.last.is_empty() {
            ui.add_space(10.0);
            ui.separator();
            self.last_results(ui, cx.last);
        }
    }

    fn footer(&mut self, ui: &mut Ui, cx: &mut ScanCtx) {
        ui.label(
            RichText::new(format!(
                "Manifests are saved in {} and imported as each drive is done.",
                cx.out_dir.display()
            ))
            .small()
            .color(colors::INK_MUTED),
        );
        ui.horizontal(|ui| {
            let label = match self.targets.len() {
                0 => "Start scan".to_string(),
                1 => "Scan 1 drive".to_string(),
                n => format!("Scan {n} drives"),
            };
            if ui
                .add_enabled(!self.targets.is_empty() && !cx.busy, egui::Button::new(RichText::new(label).strong()))
                .clicked()
            {
                cx.actions.push(Action::StartScan(self.targets.clone(), self.settings.clone()));
                self.show_failures = false;
            }
            if cx.busy {
                ui.label(RichText::new("Waiting for the current task to finish").color(colors::INK_MUTED));
            } else if self.targets.is_empty() {
                ui.label(RichText::new("Tick the drives to scan").color(colors::INK_MUTED));
            }
        });
    }

    fn progress(&mut self, ui: &mut Ui, st: &ScanStatus, cx: &mut ScanCtx) {
        let p = &st.progress;
        ui.label(RichText::new(format!("{} ({} of {})", st.name, st.volume + 1, st.volumes)).strong().size(15.0));
        ui.add_space(4.0);
        if st.importing {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new());
                ui.label("Importing the manifests…");
            });
            return;
        }
        match p.phase {
            Phase::Counting => {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new());
                    ui.label(format!(
                        "Counting files: {} to hash so far, {}",
                        format::count(p.files_total),
                        format::bytes(p.bytes_total)
                    ));
                });
            }
            _ => {
                let f = if p.bytes_total > 0 { p.bytes_done as f32 / p.bytes_total as f32 } else { 0.0 };
                ui.add(egui::ProgressBar::new(f.min(1.0)).show_percentage());
                let mut line = format!("{} of {}", format::bytes(p.bytes_done), format::bytes(p.bytes_total));
                let rate = p.rate();
                if rate > 0.0 {
                    line.push_str(&format!(" · {}/s", format::bytes(rate as u64)));
                }
                if let Some(eta) = p.eta_secs() {
                    line.push_str(&format!(" · about {} left on this drive", duration(eta)));
                }
                ui.label(line);
                ui.label(
                    RichText::new(format!("{} of {} files", format::count(p.files_done), format::count(p.files_total)))
                        .color(colors::INK_SECONDARY),
                );
            }
        }
        let mut notes = Vec::new();
        if p.files_resumed > 0 {
            notes.push(format!("{} done by the earlier scan", format::count(p.files_resumed)));
        }
        if p.failed > 0 {
            notes.push(format!("{} could not be read", format::count(p.failed)));
        }
        if p.placeholders_skipped > 0 {
            notes.push(format!("{} online-only cloud files skipped", format::count(p.placeholders_skipped)));
        }
        if p.folders_unreadable > 0 {
            notes.push(format!("{} folders could not be opened", format::count(p.folders_unreadable)));
        }
        if !notes.is_empty() {
            ui.label(RichText::new(notes.join(" · ")).color(colors::INK_MUTED));
        }
        if !p.current.is_empty() {
            ui.add(egui::Label::new(RichText::new(&p.current).small().monospace().color(colors::INK_MUTED)).truncate());
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.add_enabled(!cx.cancelling, egui::Button::new("Stop")).clicked() {
                cx.actions.push(Action::CancelJob);
            }
            ui.label(
                RichText::new(if cx.cancelling {
                    "Stopping…"
                } else {
                    "What was hashed is kept: the next scan of this drive continues from there. You can close this window; the scan goes on."
                })
                .small()
                .color(colors::INK_MUTED),
            );
        });
    }

    fn last_results(&mut self, ui: &mut Ui, last: &[ScanDone]) {
        ui.label(RichText::new("Last scan").strong());
        let mut failures = Vec::new();
        for d in last {
            match &d.result {
                Ok(r) => {
                    let p = &r.progress;
                    let state = if r.complete { "imported" } else { "stopped; continues next time" };
                    ui.label(format!(
                        "{}: {} files, {} · {} · {}",
                        d.name,
                        format::count(p.files_done),
                        format::bytes(p.bytes_done),
                        duration(p.elapsed_ms / 1000),
                        state
                    ));
                    if p.failed > 0 {
                        ui.label(
                            RichText::new(format!(
                                "   {} files could not be read (in use, or no permission)",
                                format::count(p.failed)
                            ))
                            .color(colors::WARNING),
                        );
                    }
                    failures.extend(r.failures.iter().map(|f| (d.name.clone(), f.clone())));
                }
                Err(e) => {
                    ui.label(RichText::new(format!("{}: {e}", d.name)).color(colors::CRITICAL));
                }
            }
        }
        if !failures.is_empty() {
            ui.checkbox(&mut self.show_failures, "Show the files that could not be read");
            if self.show_failures {
                egui::ScrollArea::vertical().id_salt("scan-failures").max_height(160.0).show(ui, |ui| {
                    for (_, f) in failures {
                        ui.label(
                            RichText::new(format!("{}  ({})", f.path, f.error))
                                .small()
                                .monospace()
                                .color(colors::INK_MUTED),
                        );
                    }
                });
            }
        }
    }
}

/// `2 h 05 min`, `12 min`, `40 s`.
pub fn duration(secs: u64) -> String {
    if secs >= 3600 {
        format!("{} h {:02} min", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{} min", secs / 60)
    } else {
        format!("{secs} s")
    }
}

// ----------------------------------------------------------------------- another computer

pub struct RemoteScanWindow {
    pub open: bool,
    os: ScriptOs,
    kinds: [bool; 3],
    min_size: u64,
    /// Where the script was saved, after saving.
    saved: Option<PathBuf>,
    error: Option<String>,
}

impl Default for RemoteScanWindow {
    fn default() -> Self {
        RemoteScanWindow {
            open: false,
            os: ScriptOs::Windows,
            kinds: [true, false, false],
            min_size: DEFAULT_MIN_SIZE,
            saved: None,
            error: None,
        }
    }
}

impl RemoteScanWindow {
    fn options(&self) -> ScriptOptions {
        ScriptOptions {
            os: self.os,
            kinds: DriveKind::ALL.iter().zip(self.kinds).filter(|(_, on)| *on).map(|(k, _)| *k).collect(),
            min_size: self.min_size,
        }
    }

    pub fn show(
        &mut self,
        ctx: &egui::Context,
        frame: Option<&eframe::Frame>,
        default_dir: &Path,
        actions: &mut Vec<Action>,
    ) {
        if !self.open {
            return;
        }
        let mut open = true;
        egui::Window::new("Scan another computer")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(620.0)
            .show(ctx, |ui| self.body(ui, frame, default_dir, actions));
        self.open = open;
    }

    fn body(&mut self, ui: &mut Ui, frame: Option<&eframe::Frame>, default_dir: &Path, actions: &mut Vec<Action>) {
        ui.label(
            RichText::new(
                "DupeMap saves a script that hashes the other computer's files into a manifest. Run it there with \
                 administrator rights, bring the manifest back and import it here.",
            )
            .color(colors::INK_SECONDARY),
        );
        ui.add_space(8.0);
        let before = (self.os, self.kinds, self.min_size);
        egui::Grid::new("remote-form").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label("The computer runs");
            ui.horizontal(|ui| {
                ui.radio_value(&mut self.os, ScriptOs::Windows, "Windows");
                ui.radio_value(&mut self.os, ScriptOs::Linux, ScriptOs::Linux.label());
            });
            ui.end_row();
            ui.label("Scan");
            ui.vertical(|ui| {
                let labels = match self.os {
                    ScriptOs::Windows => ["Its own drives", "USB drives", "Network drives"],
                    ScriptOs::Linux => ["Its own disks and pools (ZFS...)", "USB drives", "Network shares (NFS, SMB)"],
                };
                for (i, l) in labels.iter().enumerate() {
                    ui.checkbox(&mut self.kinds[i], *l);
                }
            });
            ui.end_row();
            ui.label("Smallest file to hash");
            egui::ComboBox::from_id_salt("remote-min").selected_text(min_size_text(self.min_size)).show_ui(ui, |ui| {
                for (v, t) in MIN_SIZES {
                    ui.selectable_value(&mut self.min_size, v, t);
                }
            });
            ui.end_row();
        });
        if (self.os, self.kinds, self.min_size) != before {
            self.saved = None;
        }
        ui.add_space(8.0);
        let any = self.kinds.iter().any(|&k| k);
        ui.horizontal(|ui| {
            if ui.add_enabled(any, egui::Button::new(RichText::new("Save script…").strong())).clicked() {
                self.save(frame, default_dir);
            }
            if !any {
                ui.label(RichText::new("Tick at least one kind of drive").color(colors::INK_MUTED));
            }
        });
        if let Some(e) = &self.error {
            ui.label(RichText::new(e).color(colors::CRITICAL));
        }
        let Some(path) = self.saved.clone() else { return };
        ui.add_space(8.0);
        ui.separator();
        ui.label(RichText::new(format!("Saved {}", path.display())).color(colors::GOOD));
        ui.add_space(4.0);
        let file = self.os.file_name();
        let manifest = scripts::manifest_name(&self.options(), "<computer>");
        let steps: Vec<(String, Option<String>)> = match self.os {
            ScriptOs::Windows => vec![
                (format!("1. Copy {file} to the other computer (a USB stick, a shared folder...)."), None),
                (
                    "2. Right-click it and choose Run with PowerShell: it asks for administrator rights itself. \
                     Or, in Terminal (Admin) in the folder of the script:"
                        .into(),
                    Some(format!("powershell -ExecutionPolicy Bypass -File .\\{file}")),
                ),
                (
                    format!(
                        "3. It hashes the files (this can take hours; it shows its progress) and writes {manifest} next to \
                         the script. If it is interrupted, run it again: it continues where it stopped."
                    ),
                    None,
                ),
                ("4. Copy the manifest back to this computer and import it:".into(), None),
            ],
            ScriptOs::Linux => vec![
                (format!("1. Copy {file} to the other computer (for TrueNAS: a dataset you can reach, or scp)."), None),
                ("2. Run it as root in the folder where the manifest should go:".into(), Some(format!("sudo bash {file}"))),
                (
                    "   A long run that should go on after you log out:".into(),
                    Some(format!("sudo nohup bash {file} > dupemap-scan.log 2>&1 &")),
                ),
                (
                    format!(
                        "3. It writes {manifest} in that folder. If it is interrupted, run it again: it continues where it \
                         stopped."
                    ),
                    None,
                ),
                ("4. Copy the manifest back to this computer and import it:".into(), None),
            ],
        };
        for (text, cmd) in steps {
            ui.label(text);
            if let Some(c) = cmd {
                ui.horizontal(|ui| {
                    ui.add_space(12.0);
                    ui.label(RichText::new(&c).monospace().color(colors::INK));
                    if ui.small_button("Copy").clicked() {
                        actions.push(Action::CopyText(c.clone()));
                    }
                });
            }
        }
        ui.horizontal(|ui| {
            ui.add_space(12.0);
            if ui.button("Import the manifest…").clicked() {
                actions.push(Action::Import);
            }
        });
    }

    fn save(&mut self, frame: Option<&eframe::Frame>, default_dir: &Path) {
        let _ = frame;
        let data = scripts::generate(&self.options());
        let file = self.os.file_name();
        #[cfg(windows)]
        let target: Option<PathBuf> = {
            let (name, ext) = match self.os {
                ScriptOs::Windows => ("PowerShell script", "ps1"),
                ScriptOs::Linux => ("bash script", "sh"),
            };
            let mut d =
                rfd::FileDialog::new().set_title("Save the scan script").set_file_name(file).add_filter(name, &[ext]);
            if let Some(f) = frame {
                d = d.set_parent(f);
            }
            d.save_file()
        };
        #[cfg(not(windows))]
        let target: Option<PathBuf> = Some(default_dir.join(file));
        let _ = default_dir;
        let Some(target) = target else { return };
        match std::fs::write(&target, data) {
            Ok(()) => {
                self.saved = Some(target);
                self.error = None;
            }
            Err(e) => self.error = Some(format!("Could not save {}: {e}", target.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_naturally() {
        assert_eq!(duration(40), "40 s");
        assert_eq!(duration(12 * 60 + 5), "12 min");
        assert_eq!(duration(2 * 3600 + 5 * 60), "2 h 05 min");
    }

    #[test]
    fn remote_options_follow_the_ticks() {
        let mut w = RemoteScanWindow { kinds: [false, true, true], ..Default::default() };
        assert_eq!(w.options().kinds, vec![DriveKind::Usb, DriveKind::Network]);
        w.os = ScriptOs::Linux;
        assert_eq!(scripts::manifest_name(&w.options(), "nas"), "manifest-nas-usb+network.tsv");
    }
}
