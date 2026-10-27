//! "Import a checksum list": a `hash  path` list (sha256sum, rclone hashsum) says neither which
//! source it belongs to nor how big its files are. This asks for the source and the folder its
//! relative paths start in, and finds or picks a size list.

use crate::actions::Action;
use crate::colors;
use crate::worker::ChecksumAsk;
use dupemap_core::db::{ChecksumImport, ImportAs};
use dupemap_core::paths::{parse_path, PathKind};
use egui::{RichText, Ui};
use std::path::PathBuf;

pub struct ChecksumDialog {
    ask: ChecksumAsk,
    host: String,
    base: String,
    sizes: Option<PathBuf>,
    sizes_input: String,
}

impl ChecksumDialog {
    pub fn new(ask: ChecksumAsk) -> Self {
        let g = ask.guess.clone();
        ChecksumDialog {
            sizes_input: g.sizes.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
            host: g.host,
            base: g.base,
            sizes: g.sizes,
            ask,
        }
    }

    fn settings(&self) -> ChecksumImport {
        ChecksumImport {
            host: self.host.trim().to_string(),
            base: self.base.trim().to_string(),
            sizes: self.sizes.clone(),
        }
    }

    /// Draw the window. Returns `false` once it is done (imported or skipped).
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        sources: &[String],
        frame: Option<&eframe::Frame>,
        actions: &mut Vec<Action>,
    ) -> bool {
        let mut keep = true;
        let mut open = true;
        let name = self.ask.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        egui::Window::new("Import a checksum list")
            .id(egui::Id::new(("checksums", self.ask.path.clone())))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(620.0)
            .show(ctx, |ui| keep = self.body(ui, &name, sources, frame, actions));
        keep && open
    }

    fn body(
        &mut self,
        ui: &mut Ui,
        name: &str,
        sources: &[String],
        frame: Option<&eframe::Frame>,
        actions: &mut Vec<Action>,
    ) -> bool {
        let _ = frame;
        ui.label(RichText::new(name).strong());
        ui.label(
            RichText::new(
                "This file lists a hash and a path per line (as sha256sum or rclone hashsum write them), \
                 without sizes or the source the files are on.",
            )
            .color(colors::INK_SECONDARY),
        );
        if !self.ask.sample.is_empty() {
            ui.add_space(4.0);
            for p in &self.ask.sample {
                ui.label(RichText::new(p).monospace().small().color(colors::INK_MUTED));
            }
        }
        ui.add_space(8.0);
        egui::Grid::new("checksum-form").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label("Source").on_hover_text(
                "The machine, drive or cloud account the files are on. For an rclone remote use its name with a \
                 colon, e.g. onedrive:. Pick an existing source to add to it.",
            );
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.host).desired_width(220.0).hint_text("onedrive:"));
                if !sources.is_empty() {
                    egui::ComboBox::from_id_salt("checksum-existing").selected_text("Existing…").width(110.0).show_ui(ui, |ui| {
                        for s in sources {
                            if ui.selectable_label(false, s).clicked() {
                                self.host = s.clone();
                                if s.ends_with(':') {
                                    self.base = s.clone();
                                }
                            }
                        }
                    });
                }
            });
            ui.end_row();
            ui.label("Paths start at").on_hover_text(
                "Put in front of every relative path in the list: the rclone remote (onedrive:), or the folder the \
                 list was made in (/mnt/tank/photos, D:\\Photos). Leave empty to keep the paths as they are.",
            );
            ui.add(egui::TextEdit::singleline(&mut self.base).desired_width(320.0).hint_text("onedrive:"));
            ui.end_row();
            ui.label("Sizes");
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut self.sizes_input)
                            .desired_width(320.0)
                            .hint_text("none (optional)"),
                    );
                    if r.changed() {
                        let t = self.sizes_input.trim();
                        self.sizes = (!t.is_empty()).then(|| PathBuf::from(t));
                    }
                    #[cfg(windows)]
                    if ui.button("Choose…").clicked() {
                        let mut d = rfd::FileDialog::new()
                            .set_title("Size list (size<TAB>path per line)")
                            .add_filter("Size lists (*.tsv, *.txt)", &["tsv", "txt"])
                            .add_filter("All files", &["*"]);
                        if let Some(dir) = self.ask.path.parent() {
                            d = d.set_directory(dir);
                        }
                        if let Some(f) = frame {
                            d = d.set_parent(f);
                        }
                        if let Some(p) = d.pick_file() {
                            self.sizes_input = p.display().to_string();
                            self.sizes = Some(p);
                        }
                    }
                });
                let note = match &self.sizes {
                    Some(p) if p.is_file() => "Sizes are taken from this list (size<TAB>path per line, as rclone lsf --format sp writes it).".to_string(),
                    Some(_) => "That file does not exist.".to_string(),
                    None => "Without sizes, a file takes the size of a copy with the same SHA-256 elsewhere; the others show their size as unknown.".to_string(),
                };
                ui.label(RichText::new(note).small().color(colors::INK_MUTED));
            });
            ui.end_row();
        });
        // what a path becomes
        if let Some(first) = self.ask.sample.first() {
            let full = self.settings().full_path(first.as_bytes()).into_owned();
            let pp = parse_path(&full);
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("First file:").color(colors::INK_MUTED));
                ui.label(RichText::new(pp.display()).monospace());
            });
            if pp.kind == PathKind::Relative {
                ui.label(
                    RichText::new(
                        "The paths stay relative: fill in where they start, so they line up with your other sources.",
                    )
                    .small()
                    .color(colors::WARNING),
                );
            }
        }
        ui.add_space(10.0);
        let mut keep = true;
        let host_ok = !self.host.trim().is_empty();
        let sizes_ok = self.sizes.as_ref().is_none_or(|p| p.is_file());
        ui.horizontal(|ui| {
            if ui.add_enabled(host_ok && sizes_ok, egui::Button::new(RichText::new("Import").strong())).clicked() {
                actions.push(Action::ImportAs(self.ask.path.clone(), ImportAs::Checksums(self.settings())));
                keep = false;
            }
            if ui.button("Skip this file").clicked() {
                keep = false;
            }
            if !host_ok {
                ui.label(RichText::new("Name the source").color(colors::INK_MUTED));
            }
        });
        keep
    }
}
