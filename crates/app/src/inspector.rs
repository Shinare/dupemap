//! The inspector: every copy of the selected file, grouped by source, or where the files of the
//! selected folder have copies. No copy is called the "original": you decide what to keep.

use crate::actions::Action;
use crate::colors;
use crate::format;
use crate::selection::Target;
use crate::widgets;
use crate::worker::{DirInfo, FileInfo, Inspection, Query, Worker};
use dupemap_core::index::{Index, Location};
use dupemap_core::paths::PathKind;
use egui::{FontId, Rect, RichText, Sense, Ui, Vec2};

pub struct Inspector {
    pub open: bool,
    generation: u64,
    target: Option<Target>,
    info: Option<Result<Box<Inspection>, String>>,
}

impl Default for Inspector {
    fn default() -> Self {
        Inspector { open: true, generation: 0, target: None, info: None }
    }
}

impl Inspector {
    pub fn target(&self) -> Option<Target> {
        self.target
    }

    /// Details requested and not here yet.
    pub fn is_pending(&self) -> bool {
        self.target.is_some() && self.info.is_none()
    }

    /// Show something else (or nothing). Asks the worker for the details.
    pub fn select(&mut self, generation: u64, target: Option<Target>, worker: &Worker) {
        if self.target == target && self.generation == generation {
            return;
        }
        self.generation = generation;
        self.target = target;
        self.info = None;
        if let Some(t) = target {
            worker.send(Query::Inspect { generation, target: t });
        }
    }

    pub fn on_reply(&mut self, generation: u64, target: Target, info: Result<Box<Inspection>, String>) {
        if generation == self.generation && Some(target) == self.target {
            self.info = Some(info);
        }
    }

    /// A new index: old file and folder numbers mean nothing any more.
    pub fn clear(&mut self) {
        self.target = None;
        self.info = None;
    }

    pub fn ui(&mut self, ui: &mut Ui, ix: Option<&Index>, actions: &mut Vec<Action>) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Inspector").strong().color(colors::INK_SECONDARY));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("Hide").on_hover_text("View → Inspector (Ctrl+I) shows it again").clicked() {
                    self.open = false;
                }
            });
        });
        ui.separator();
        let Some(ix) = ix else { return };
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match (&self.target, &self.info) {
            (None, _) => {
                ui.add_space(8.0);
                ui.label(
                    RichText::new(
                        "Select a file to see every copy of it, grouped by source.\n\n\
                             Select a folder to see which sources hold copies of its files.",
                    )
                    .color(colors::INK_MUTED),
                );
            }
            (Some(_), None) => {
                ui.add_space(8.0);
                ui.add(egui::Spinner::new());
            }
            (Some(_), Some(Err(e))) => {
                widgets::status_label(ui, "⚠", colors::CRITICAL, format!("Could not read the details: {e}"));
            }
            (Some(_), Some(Ok(info))) => match info.as_ref() {
                Inspection::File(f) => file_ui(ui, ix, f, actions),
                Inspection::Dir(d) => dir_ui(ui, ix, d, actions),
            },
        });
    }
}

fn file_ui(ui: &mut Ui, ix: &Index, f: &FileInfo, actions: &mut Vec<Action>) {
    ui.add_space(4.0);
    ui.label(RichText::new(&f.name).size(16.0).color(colors::INK));
    let copies = if f.copies <= 1 {
        "no other copy anywhere".to_string()
    } else {
        format!(
            "{} · {} in other sources",
            format::plural(f.copies as u64, "copy", "copies"),
            format::count(f.other_sources as u64)
        )
    };
    let size = match f.size_state {
        dupemap_core::index::SizeState::Unknown => "size not known".to_string(),
        _ => format::bytes(f.size),
    };
    ui.label(RichText::new(format!("{size} · {copies}")).color(colors::INK_SECONDARY));
    match f.size_state {
        dupemap_core::index::SizeState::Unknown => {
            ui.label(
                RichText::new("It came from a checksum list without sizes, and no copy with a size is listed.")
                    .small()
                    .color(colors::INK_MUTED),
            );
        }
        dupemap_core::index::SizeState::FromCopy => {
            ui.label(
                RichText::new("Size taken from a copy with the same hash (its checksum list has no sizes).")
                    .small()
                    .color(colors::INK_MUTED),
            );
        }
        dupemap_core::index::SizeState::Known => {}
    }
    let shown = f.hash.rsplit(':').next().unwrap_or(&f.hash);
    // rows with a button on the right: the text on the left gives way in a narrow panel
    egui::Sides::new().shrink_left().truncate().show(
        ui,
        |ui| {
            let algo = if f.flagged { f.algo.to_uppercase() } else { "SHA-256".to_string() };
            ui.label(RichText::new(algo).small().color(colors::INK_MUTED));
            let short = if shown.len() > 20 {
                format!("{}…{}", &shown[..10], &shown[shown.len() - 8..])
            } else {
                shown.to_string()
            };
            ui.label(RichText::new(short).monospace().color(colors::INK_SECONDARY)).on_hover_text(&f.hash);
        },
        |ui| {
            if ui.small_button("Copy hash").clicked() {
                actions.push(Action::CopyText(shown.to_string()));
            }
        },
    );
    if f.flagged {
        widgets::status_label(
            ui,
            "⚠",
            colors::WARNING,
            format!("Not SHA-256: compared only with other {} hashes", f.algo.to_uppercase()),
        );
    }
    ui.add_space(8.0);
    ui.label(RichText::new("Copies by source").strong());
    ui.label(RichText::new("Every copy is listed; none is treated as the original.").small().color(colors::INK_MUTED));
    ui.add_space(4.0);
    let mut i = 0;
    while i < f.locations.len() {
        let s = f.locations[i].source;
        let j = f.locations[i..].iter().position(|l| l.source != s).map_or(f.locations.len(), |k| i + k);
        source_header(ui, ix, s, j - i);
        for (k, l) in f.locations.iter().enumerate().take(j).skip(i) {
            let here = f.on_this_pc.get(k).copied().unwrap_or(false);
            location_row(ui, l, l.file == f.file, here, actions);
        }
        ui.add_space(6.0);
        i = j;
    }
}

fn source_header(ui: &mut Ui, ix: &Index, s: u16, n: usize) {
    let src = &ix.sources()[s as usize];
    let kind = match src.kind {
        PathKind::Windows => "Windows",
        PathKind::Posix => "Linux / NAS",
        PathKind::Cloud => "Cloud",
        PathKind::Relative => "Other",
    };
    egui::Sides::new().shrink_left().truncate().show(
        ui,
        |ui| {
            ui.label(RichText::new(format::host(&src.host)).strong().color(colors::INK));
            ui.label(RichText::new(kind).small().color(colors::INK_MUTED));
        },
        |ui| {
            ui.label(RichText::new(format::plural(n as u64, "copy", "copies")).color(colors::INK_SECONDARY));
        },
    );
}

fn location_row(ui: &mut Ui, l: &Location, selected: bool, on_this_pc: bool, actions: &mut Vec<Action>) {
    egui::Frame::new()
        .fill(if selected { colors::ROW_SELECTED } else { colors::SURFACE })
        .corner_radius(4)
        .inner_margin(egui::Margin::symmetric(8, 5))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            // one wrapping label, so the marker of the selected copy stays in front of its path
            let mut job = egui::text::LayoutJob::default();
            let font = egui::TextStyle::Body.resolve(ui.style());
            if selected {
                // a no-break space: a long path must not leave the marker alone on its line
                job.append("●\u{a0}", 0.0, egui::TextFormat::simple(font.clone(), colors::ACCENT));
            }
            job.append(&l.path, 0.0, egui::TextFormat::simple(font, colors::INK));
            let resp = ui.add(egui::Label::new(job).wrap());
            if selected {
                resp.on_hover_text("The selected file");
            }
            ui.horizontal_wrapped(|ui| {
                if ui.small_button("Show").on_hover_text("Open this copy in a tab").clicked() {
                    actions.push(Action::ShowInTab(l.file));
                }
                if ui.small_button("Copy path").clicked() {
                    actions.push(Action::CopyText(l.path.clone()));
                }
                if on_this_pc && ui.small_button("Show in Explorer").clicked() {
                    actions.push(Action::Reveal(l.path.clone()));
                }
            });
        });
    ui.add_space(3.0);
}

fn dir_ui(ui: &mut Ui, ix: &Index, d: &DirInfo, actions: &mut Vec<Action>) {
    let is_source = ix.dir_parent(d.dir).is_none();
    let src = &ix.sources()[ix.dir_source(d.dir) as usize];
    ui.add_space(4.0);
    let name = if is_source { format::host(&src.host).to_string() } else { ix.dir_name(d.dir).to_string() };
    ui.label(RichText::new(name).size(16.0).color(colors::INK));
    if !is_source {
        ui.add(egui::Label::new(RichText::new(&d.path).color(colors::INK_MUTED)).wrap());
        egui::Sides::new().shrink_left().truncate().show(
            ui,
            |ui| {
                ui.label(RichText::new(format!("in {}", format::host(&src.host))).small().color(colors::INK_MUTED));
            },
            |ui| {
                if ui.small_button("Copy path").clicked() {
                    actions.push(Action::CopyText(d.path.clone()));
                }
            },
        );
    }
    ui.add_space(6.0);
    let st = &d.stats;
    let pct = |a: u64, b: u64| if b == 0 { 0.0 } else { a as f64 * 100.0 / b as f64 };
    ui.label(format!("{} · {}", format::bytes(st.bytes), format::plural(st.files as u64, "file", "files")));
    ui.label(
        RichText::new(format!(
            "{} have another copy somewhere ({})",
            format::count(st.dup_files as u64),
            format::percent(pct(st.dup_files as u64, st.files as u64))
        ))
        .color(colors::INK_SECONDARY),
    );
    ui.label(
        RichText::new(format!("{} of the bytes also exist outside this folder", format::percent(st.pct_elsewhere())))
            .color(colors::INK_SECONDARY),
    );
    if st.fully_elsewhere() {
        widgets::status_label(ui, "✔", colors::GOOD, "Every file here has a copy outside this folder");
    }

    ui.add_space(10.0);
    ui.label(RichText::new("Where the copies are").strong());
    ui.label(
        RichText::new("Files of this folder with a copy outside it, by the source holding that copy. A file can count for several sources.")
            .small()
            .color(colors::INK_MUTED),
    );
    ui.add_space(4.0);
    let o = &d.outside;
    let total = o.bytes.max(1);
    for (i, &(s, files, bytes)) in o.by_source.iter().enumerate() {
        let host = format::host(&ix.sources()[s as usize].host).to_string();
        let same = s == ix.dir_source(d.dir);
        let label = if same { format!("{host} (elsewhere in this source)") } else { host };
        share_row(ui, &label, files, bytes, bytes as f32 / total as f32, colors::DUP);
        // where in that source: the deepest folder holding (nearly) all of these copies
        if let Some(&(_, main, held)) = o.main_dir.get(i) {
            if crate::selection::names_a_place(ix, main) {
                let share = if bytes > 0 { held as f64 * 100.0 / bytes as f64 } else { 0.0 };
                let what =
                    if share >= 99.95 { "all under".to_string() } else { format!("{} under", format::percent(share)) };
                let path = ix.dir_path(main);
                egui::Sides::new().shrink_left().truncate().show(
                    ui,
                    |ui| {
                        ui.add_space(10.0);
                        ui.label(RichText::new(what).small().color(colors::INK_MUTED));
                        ui.label(RichText::new(&path).small().color(colors::INK_SECONDARY)).on_hover_text(&path);
                    },
                    |ui| {
                        if ui.small_button("Show").on_hover_text("Open this folder in a tab").clicked() {
                            actions.push(Action::ShowDirInTab(main));
                        }
                    },
                );
            } else {
                ui.horizontal(|ui| {
                    ui.add_space(10.0);
                    ui.label(RichText::new("spread over the whole source").small().color(colors::INK_MUTED));
                });
            }
        }
        ui.add_space(2.0);
    }
    if o.alone_files > 0 {
        share_row(
            ui,
            "No copy outside this folder",
            o.alone_files,
            o.alone_bytes,
            o.alone_bytes as f32 / total as f32,
            colors::INK_MUTED,
        );
    }
    if o.files == 0 {
        ui.label(RichText::new("No files").color(colors::INK_MUTED));
    }
}

/// One labelled bar: share of the folder's bytes.
fn share_row(ui: &mut Ui, label: &str, files: u64, bytes: u64, share: f32, color: egui::Color32) {
    ui.horizontal(|ui| {
        ui.add(egui::Label::new(RichText::new(label).color(colors::INK)).truncate());
    });
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 18.0), Sense::hover());
    let p = ui.painter_at(rect);
    let text = format!(
        "{} · {} · {}",
        format::plural(files, "file", "files"),
        format::bytes(bytes),
        format::percent(share as f64 * 100.0)
    );
    let font = FontId::proportional(12.0);
    let tw = p.layout_no_wrap(text.clone(), font.clone(), colors::INK_SECONDARY).size().x;
    let bar =
        Rect::from_min_max(rect.min, egui::pos2((rect.right() - tw - 10.0).max(rect.left() + 20.0), rect.bottom()));
    widgets::bar(&p, bar, share, color);
    widgets::text_right(&p, rect, &text, font, colors::INK_SECONDARY);
    ui.add_space(4.0);
}
