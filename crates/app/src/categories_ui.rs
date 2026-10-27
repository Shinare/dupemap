//! The "File type categories" window: which extensions make up each category of the summary.

use crate::actions::Action;
use crate::colors;
use crate::format;
use crate::summary_ui::ext_label;
use dupemap_core::summary::{parse_exts, Categories, Category, Summary, OTHER};
use egui::{RichText, Ui};
use std::collections::HashSet;

/// Uncategorised extensions listed with "Add to".
const UNCATEGORISED_SHOWN: usize = 40;

#[derive(Clone, PartialEq, Eq)]
struct Draft {
    name: String,
    /// Extensions as typed: spaces, commas or semicolons between them.
    exts: String,
}

pub struct CategoryEditor {
    draft: Vec<Draft>,
    base: Categories,
}

fn drafts(c: &Categories) -> Vec<Draft> {
    c.list.iter().map(|c| Draft { name: c.name.clone(), exts: c.exts.join(" ") }).collect()
}

impl CategoryEditor {
    pub fn new(current: &Categories) -> Self {
        CategoryEditor { draft: drafts(current), base: current.clone() }
    }

    /// The stored categories changed (saved from here or from a summary's right-click menu).
    /// Unsaved edits are kept.
    pub fn rebase(&mut self, current: &Categories) {
        if !self.is_dirty() {
            self.draft = drafts(current);
        }
        self.base = current.clone();
    }

    fn to_categories(&self) -> Categories {
        Categories {
            list: self.draft.iter().map(|d| Category { name: d.name.clone(), exts: parse_exts(&d.exts) }).collect(),
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.to_categories().normalized() != self.base.normalized()
    }

    /// Names used twice (they would be indistinguishable in the summary).
    fn duplicate_names(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut dup = Vec::new();
        for d in &self.draft {
            let n = d.name.trim().to_lowercase();
            if !n.is_empty() && !seen.insert(n.clone()) && !dup.contains(&d.name.trim().to_string()) {
                dup.push(d.name.trim().to_string());
            }
        }
        dup
    }

    /// Draw the window. Returns `false` once it was closed.
    pub fn show(&mut self, ctx: &egui::Context, all: Option<&Summary>, actions: &mut Vec<Action>) -> bool {
        let mut open = true;
        egui::Window::new("File type categories")
            .open(&mut open)
            .default_width(760.0)
            .default_height(640.0)
            .min_height(360.0)
            .collapsible(false)
            .resizable(true)
            .show(ctx, |ui| self.body(ui, all, actions));
        open
    }

    fn body(&mut self, ui: &mut Ui, all: Option<&Summary>, actions: &mut Vec<Action>) {
        ui.label(
            RichText::new(
                "The summary adds up files by category. A category is a list of extensions, separated by spaces \
                 or commas (tar.gz counts as one). An extension belongs to the first category that lists it; \
                 everything else counts as Other.",
            )
            .color(colors::INK_SECONDARY),
        );
        ui.add_space(8.0);
        let footer_h = 84.0;
        let mut remove: Option<usize> = None;
        let mut swap: Option<(usize, usize)> = None;
        egui::ScrollArea::vertical().max_height((ui.available_height() - footer_h).max(120.0)).show(ui, |ui| {
            let n = self.draft.len();
            for i in 0..n {
                let d = &mut self.draft[i];
                egui::Frame::new()
                    .fill(colors::SURFACE)
                    .corner_radius(4)
                    .inner_margin(egui::Margin::symmetric(8, 6))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        egui::Sides::new().shrink_left().show(
                            ui,
                            |ui| {
                                ui.add(egui::TextEdit::singleline(&mut d.name).desired_width(220.0).hint_text("Name"));
                                let count = parse_exts(&d.exts).len();
                                ui.label(
                                    RichText::new(format::plural(count as u64, "extension", "extensions"))
                                        .small()
                                        .color(colors::INK_MUTED),
                                );
                            },
                            |ui| {
                                if ui.small_button("✕").on_hover_text("Remove this category").clicked() {
                                    remove = Some(i);
                                }
                                if ui.add_enabled(i + 1 < n, egui::Button::new("↓").small()).clicked() {
                                    swap = Some((i, i + 1));
                                }
                                if ui.add_enabled(i > 0, egui::Button::new("↑").small()).clicked() {
                                    swap = Some((i - 1, i));
                                }
                            },
                        );
                        ui.add(
                            egui::TextEdit::multiline(&mut d.exts)
                                .desired_rows(2)
                                .desired_width(f32::INFINITY)
                                .hint_text("mkv mp4 avi"),
                        );
                    });
                ui.add_space(4.0);
            }
            if ui.button("Add a category").clicked() {
                self.draft.push(Draft { name: format!("Category {}", self.draft.len() + 1), exts: String::new() });
            }
            if let Some(sum) = all {
                self.uncategorised(ui, sum);
            }
        });
        if let Some(i) = remove {
            self.draft.remove(i);
        }
        if let Some((a, b)) = swap {
            self.draft.swap(a, b);
        }
        ui.separator();
        let dups = self.duplicate_names();
        if !dups.is_empty() {
            ui.label(
                RichText::new(format!("⚠ Used twice: {}. Give every category its own name.", dups.join(", ")))
                    .color(colors::WARNING),
            );
        }
        ui.horizontal(|ui| {
            if ui.button("Restore defaults").on_hover_text("The built-in categories (Apply to keep them)").clicked() {
                self.draft = drafts(&Categories::default());
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let dirty = self.is_dirty();
                if ui
                    .add_enabled(dirty && dups.is_empty(), egui::Button::new(RichText::new("Apply").strong()))
                    .clicked()
                {
                    actions.push(Action::SetCategories(self.to_categories()));
                }
                if ui.add_enabled(dirty, egui::Button::new("Revert")).clicked() {
                    self.draft = drafts(&self.base);
                }
                if dirty {
                    ui.label(RichText::new("Not applied yet").color(colors::INK_MUTED));
                }
            });
        });
    }

    /// The biggest extensions in no category, each with "Add to".
    fn uncategorised(&mut self, ui: &mut Ui, sum: &Summary) {
        // judged against the draft, so an extension disappears from here as soon as it is typed
        let draft = self.to_categories().normalized();
        let other: Vec<_> =
            sum.exts.iter().filter(|e| !e.ext.is_empty() && draft.category_of(&e.ext) == draft.list.len()).collect();
        ui.add_space(12.0);
        ui.label(RichText::new(format!("In no category ({OTHER})")).strong());
        if other.is_empty() {
            ui.label(RichText::new("Every extension found is in a category.").color(colors::INK_MUTED));
            return;
        }
        ui.label(
            RichText::new("The largest extensions found in your manifests that no category lists.")
                .small()
                .color(colors::INK_MUTED),
        );
        ui.add_space(4.0);
        let mut add: Option<(usize, String)> = None;
        egui::Grid::new("uncategorised").num_columns(4).spacing([16.0, 4.0]).striped(true).show(ui, |ui| {
            for e in other.iter().take(UNCATEGORISED_SHOWN) {
                ui.label(RichText::new(ext_label(&e.ext)).color(colors::INK));
                ui.label(RichText::new(format::bytes(e.split.bytes)).color(colors::INK_SECONDARY));
                ui.label(RichText::new(format::plural(e.split.files, "file", "files")).color(colors::INK_SECONDARY));
                egui::ComboBox::from_id_salt(("add-ext", e.ext.as_str()))
                    .selected_text("Add to…")
                    .width(170.0)
                    .show_ui(ui, |ui| {
                        for (i, d) in self.draft.iter().enumerate() {
                            if ui.selectable_label(false, d.name.as_str()).clicked() {
                                add = Some((i, e.ext.clone()));
                            }
                        }
                    });
                ui.end_row();
            }
        });
        if other.len() > UNCATEGORISED_SHOWN {
            ui.label(
                RichText::new(format!("… and {} more", other.len() - UNCATEGORISED_SHOWN))
                    .small()
                    .color(colors::INK_MUTED),
            );
        }
        if let Some((i, ext)) = add {
            let d = &mut self.draft[i];
            if !d.exts.trim().is_empty() {
                d.exts.push(' ');
            }
            d.exts.push_str(&ext);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing_round_trip() {
        let cats = Categories::default();
        let mut ed = CategoryEditor::new(&cats);
        assert!(!ed.is_dirty());
        ed.draft[0].exts.push_str(", .FOO");
        assert!(ed.is_dirty());
        let c = ed.to_categories();
        assert!(c.list[0].exts.contains(&"foo".to_string()));
        // a save elsewhere does not throw away unsaved edits
        ed.rebase(&cats);
        assert!(ed.is_dirty());
        ed.draft = drafts(&cats);
        assert!(!ed.is_dirty());
        // typing an extension that is already in another category is not a change of mapping
        // for that extension (the first category keeps it)
        ed.draft[1].exts.push_str(" mkv");
        assert_eq!(ed.to_categories().normalized().category_of("mkv"), 0);
    }

    #[test]
    fn duplicate_names_are_reported() {
        let mut ed = CategoryEditor::new(&Categories::default());
        assert!(ed.duplicate_names().is_empty());
        ed.draft[1].name = " video ".into();
        assert_eq!(ed.duplicate_names(), vec!["video".to_string()]);
    }
}
