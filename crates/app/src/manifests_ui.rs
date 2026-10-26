//! "Manifests" window (import, remove, open reports) and the import report window.

use crate::actions::Action;
use crate::colors;
use crate::format;
use crate::widgets;
use dupemap_core::db::ManifestInfo;
use dupemap_core::{ImportReport, Severity};
use egui::{RichText, Ui};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Default)]
pub struct ManifestsWindow {
    pub open: bool,
    /// Path typed by hand (the only way to import where no file dialog is available).
    path_input: String,
    confirm_remove: Option<(i64, String)>,
    /// Whether each manifest file is still where it was imported from. Checked once per opening
    /// of the window: looking at a path on a sleeping drive or share can take seconds.
    on_disk: HashMap<String, bool>,
}

impl ManifestsWindow {
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        manifests: &[ManifestInfo],
        db_path: &std::path::Path,
        busy: bool,
        actions: &mut Vec<Action>,
    ) {
        if !self.open {
            self.on_disk.clear();
            return;
        }
        let mut open = true;
        egui::Window::new("Manifests")
            .open(&mut open)
            .default_width(780.0)
            .default_height(420.0)
            .collapsible(false)
            .show(ctx, |ui| self.body(ui, manifests, db_path, busy, actions));
        self.open = open;
        if let Some((id, name)) = self.confirm_remove.clone() {
            let modal = egui::Modal::new(egui::Id::new("confirm-remove")).show(ctx, |ui| {
                ui.set_width(420.0);
                ui.heading("Remove manifest?");
                ui.add_space(6.0);
                ui.label(format!(
                    "{name} and its rows are removed from DupeMap. Paths it hid in other manifests come back. \
                     The file on disk is not touched; you can import it again any time."
                ));
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button(RichText::new("Remove").strong()).clicked() {
                        actions.push(Action::RemoveManifests(vec![id]));
                        self.confirm_remove = None;
                    }
                    if ui.button("Cancel").clicked() {
                        self.confirm_remove = None;
                    }
                });
            });
            if modal.should_close() {
                self.confirm_remove = None;
            }
        }
    }

    fn body(
        &mut self,
        ui: &mut Ui,
        manifests: &[ManifestInfo],
        db_path: &std::path::Path,
        busy: bool,
        actions: &mut Vec<Action>,
    ) {
        ui.horizontal(|ui| {
            if ui.add_enabled(!busy, egui::Button::new("Import manifests…")).clicked() {
                actions.push(Action::Import);
            }
            ui.label(
                RichText::new("Importing a file with the same name replaces the earlier import.")
                    .color(colors::INK_MUTED),
            );
        });
        if !cfg!(windows) {
            ui.horizontal(|ui| {
                ui.label("Path:");
                let r = ui.add(
                    egui::TextEdit::singleline(&mut self.path_input)
                        .hint_text("a manifest-*.tsv file or a folder of them")
                        .desired_width(420.0),
                );
                let go =
                    ui.add_enabled(!busy && !self.path_input.trim().is_empty(), egui::Button::new("Import")).clicked()
                        || (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                if go && !busy {
                    actions.push(Action::ImportPaths(vec![PathBuf::from(self.path_input.trim())]));
                }
            });
        }
        ui.add_space(6.0);
        if manifests.is_empty() {
            ui.label(
                RichText::new("No manifests imported yet. Drop manifest-*.tsv files on the window, or use Import.")
                    .color(colors::INK_SECONDARY),
            );
        } else {
            egui::ScrollArea::vertical().max_height(ui.available_height() - 40.0).show(ui, |ui| {
                egui::Grid::new("manifests").num_columns(6).striped(true).spacing([14.0, 6.0]).show(ui, |ui| {
                    for h in ["Status", "Manifest", "Rows", "Not SHA-256", "Malformed", ""] {
                        ui.label(RichText::new(h).color(colors::INK_MUTED));
                    }
                    ui.end_row();
                    for m in manifests {
                        severity_cell(ui, m.severity, m.alert_dismissed);
                        ui.label(&m.file_name).on_hover_text(format!(
                            "{}\n{} on disk",
                            m.file_path,
                            format::bytes(m.file_size)
                        ));
                        ui.label(format::count(m.rows));
                        count_cell(ui, m.flagged);
                        count_cell(ui, m.malformed);
                        ui.horizontal(|ui| {
                            if ui.small_button("Report").clicked() {
                                actions.push(Action::ShowReport(m.id));
                            }
                            if cfg!(windows)
                                && *self
                                    .on_disk
                                    .entry(m.file_path.clone())
                                    .or_insert_with(|| std::path::Path::new(&m.file_path).exists())
                                && ui
                                    .small_button("Open folder")
                                    .on_hover_text("Show the manifest file in Explorer")
                                    .clicked()
                            {
                                actions.push(Action::Reveal(m.file_path.clone()));
                            }
                            if ui
                                .add_enabled(!busy, egui::Button::new("Re-import").small())
                                .on_hover_text("Import the file again from its original location")
                                .clicked()
                            {
                                actions.push(Action::ImportPaths(vec![PathBuf::from(&m.file_path)]));
                            }
                            if ui.add_enabled(!busy, egui::Button::new("Remove").small()).clicked() {
                                self.confirm_remove = Some((m.id, m.file_name.clone()));
                            }
                        });
                        ui.end_row();
                    }
                });
            });
        }
        ui.add_space(4.0);
        ui.separator();
        ui.label(RichText::new(format!("Database: {}", db_path.display())).small().color(colors::INK_MUTED));
    }
}

fn count_cell(ui: &mut Ui, n: u64) {
    if n == 0 {
        ui.label(RichText::new("0").color(colors::INK_MUTED));
    } else {
        widgets::status_label(ui, "⚠", colors::WARNING, format::count(n));
    }
}

fn severity_cell(ui: &mut Ui, s: Severity, dismissed: bool) {
    match (s, dismissed) {
        (Severity::Alert, false) => widgets::status_label(ui, "⚠", colors::CRITICAL, "Alert"),
        (Severity::Alert, true) => widgets::status_label(ui, "⚠", colors::INK_MUTED, "Alert (dismissed)"),
        (Severity::Notice, _) => widgets::status_label(ui, "ℹ", colors::LINK, "Notice"),
        (Severity::Ok, _) => widgets::status_label(ui, "✔", colors::GOOD, "OK"),
    };
}

/// Import reports: after an import, or opened from the Manifests window.
#[derive(Default)]
pub struct ReportWindow {
    pub reports: Vec<ImportReport>,
    pub selected: usize,
    pub open: bool,
}

impl ReportWindow {
    pub fn show_reports(&mut self, reports: Vec<ImportReport>) {
        if reports.is_empty() {
            return;
        }
        // alerts first, so they cannot be missed
        let mut reports = reports;
        reports.sort_by_key(|r| std::cmp::Reverse(r.severity().code()));
        self.reports = reports;
        self.selected = 0;
        self.open = true;
    }

    pub fn show(&mut self, ctx: &egui::Context, manifests: &[ManifestInfo], actions: &mut Vec<Action>) {
        if !self.open || self.reports.is_empty() {
            return;
        }
        let mut open = true;
        egui::Window::new("Import report")
            .open(&mut open)
            .default_width(820.0)
            .default_height(560.0)
            .collapsible(false)
            .show(ctx, |ui| {
                if self.reports.len() > 1 {
                    ui.horizontal_wrapped(|ui| {
                        for (i, r) in self.reports.iter().enumerate() {
                            let icon = match r.severity() {
                                Severity::Alert => "⚠ ",
                                Severity::Notice => "ℹ ",
                                Severity::Ok => "",
                            };
                            if ui.selectable_label(self.selected == i, format!("{icon}{}", r.file_name)).clicked() {
                                self.selected = i;
                            }
                        }
                    });
                    ui.separator();
                }
                let i = self.selected.min(self.reports.len() - 1);
                let r = &self.reports[i];
                let info = manifests.iter().find(|m| m.id == r.manifest_id);
                report_body(ui, r, info, actions);
            });
        self.open = open;
    }
}

fn report_body(ui: &mut Ui, r: &ImportReport, info: Option<&ManifestInfo>, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        ui.heading(&r.file_name);
        match r.severity() {
            Severity::Alert => widgets::status_label(ui, "⚠", colors::CRITICAL, "Needs attention"),
            Severity::Notice => widgets::status_label(ui, "ℹ", colors::LINK, "Imported, with notes"),
            Severity::Ok => widgets::status_label(ui, "✔", colors::GOOD, "Imported cleanly"),
        };
    });
    ui.label(
        RichText::new(format!(
            "{} rows imported · {} lines read · {} · {} line endings{}",
            format::count(r.rows_imported),
            format::count(r.lines),
            r.encoding,
            r.line_endings,
            if r.elapsed_ms > 0 { format!(" · {:.1} s", r.elapsed_ms as f64 / 1000.0) } else { String::new() }
        ))
        .color(colors::INK_SECONDARY),
    );
    if let (Severity::Alert, Some(m)) = (r.severity(), info) {
        ui.horizontal(|ui| {
            if m.alert_dismissed {
                ui.label(
                    RichText::new("Alert dismissed; the rows stay flagged and never match SHA-256 rows.")
                        .color(colors::INK_MUTED),
                );
                if ui.small_button("Show alert again").clicked() {
                    actions.push(Action::SetAlertDismissed(m.id, false));
                }
            } else if ui
                .button("Dismiss alert")
                .on_hover_text("Hide the warning in the status bar and on the source tab. Flagged rows stay flagged.")
                .clicked()
            {
                actions.push(Action::SetAlertDismissed(m.id, true));
            }
        });
    }
    ui.add_space(6.0);
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        let findings = r.findings();
        if !findings.is_empty() {
            ui.label(RichText::new("Findings").strong());
            for f in findings {
                let (icon, color, text) = match f.strip_prefix("ALERT: ") {
                    Some(t) => ("⚠", colors::CRITICAL, t.to_string()),
                    None => ("ℹ", colors::LINK, f),
                };
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(icon).color(color).strong());
                    ui.label(text);
                });
            }
            ui.add_space(8.0);
        }
        ui.columns(3, |cols| {
            table(&mut cols[0], "Rows per source", r.hosts.iter().map(|(k, v)| (format::host(k).to_string(), *v)));
            table(&mut cols[1], "Not SHA-256 (by algorithm)", r.flagged.iter().map(|(k, v)| (k.clone(), *v)));
            table(&mut cols[2], "Skipped lines (by reason)", r.malformed.iter().map(|(k, v)| (k.clone(), *v)));
        });
        if !r.samples.is_empty() {
            ui.add_space(8.0);
            ui.label(RichText::new("Sample lines").strong());
            egui::Grid::new("samples").num_columns(3).striped(true).spacing([12.0, 4.0]).show(ui, |ui| {
                for s in &r.samples {
                    ui.label(RichText::new(format!("line {}", format::count(s.line))).color(colors::INK_MUTED));
                    ui.label(format!("{}: {}", s.category, s.detail));
                    ui.label(RichText::new(s.text.replace('\t', " ⇥ ")).monospace().color(colors::INK_SECONDARY));
                    ui.end_row();
                }
            });
        }
    });
}

fn table(ui: &mut Ui, title: &str, rows: impl Iterator<Item = (String, u64)>) {
    ui.label(RichText::new(title).color(colors::INK_MUTED));
    let rows: Vec<(String, u64)> = rows.collect();
    if rows.is_empty() {
        ui.label(RichText::new("none").color(colors::INK_MUTED));
        return;
    }
    egui::Grid::new(title).num_columns(2).spacing([10.0, 2.0]).show(ui, |ui| {
        for (k, v) in rows {
            ui.label(k);
            ui.label(format::count(v));
            ui.end_row();
        }
    });
}
