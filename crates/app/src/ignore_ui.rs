//! The "Ignored folders" window: switch the built-in rules on or off and add your own folders.

use crate::actions::Action;
use crate::colors;
use crate::format;
use dupemap_core::ignore::{default_rules, IgnoreRule, IgnoreSettings, RuleEffect};
use dupemap_core::index::Index;
use egui::{RichText, Ui};

pub struct IgnoreEditor {
    pub draft: IgnoreSettings,
    base: IgnoreSettings,
    new_pattern: String,
    new_host: Option<String>,
    error: Option<String>,
}

impl IgnoreEditor {
    pub fn new(current: &IgnoreSettings) -> Self {
        IgnoreEditor {
            draft: current.clone(),
            base: current.clone(),
            new_pattern: String::new(),
            new_host: None,
            error: None,
        }
    }

    /// The settings the index was rebuilt with (after Apply finished elsewhere).
    pub fn rebase(&mut self, current: &IgnoreSettings) {
        if self.draft == self.base {
            self.draft = current.clone();
        }
        self.base = current.clone();
    }

    pub fn is_dirty(&self) -> bool {
        self.draft != self.base
    }

    /// Draw the window. Returns `false` once it was closed.
    pub fn show(&mut self, ctx: &egui::Context, ix: Option<&Index>, busy: bool, actions: &mut Vec<Action>) -> bool {
        let mut open = true;
        egui::Window::new("Ignored folders")
            .open(&mut open)
            .default_width(860.0)
            .default_height(640.0)
            .min_height(420.0)
            .collapsible(false)
            .resizable(true)
            .show(ctx, |ui| self.body(ui, ix, busy, actions));
        open
    }

    fn body(&mut self, ui: &mut Ui, ix: Option<&Index>, busy: bool, actions: &mut Vec<Action>) {
        ui.label(
            RichText::new(
                "Folders matching a rule are left out completely: they are not shown, not counted, and their files \
                 never count as copies of anything. Use it for installed software, system folders and app data, \
                 where duplicates are expected and deleting them would break things.",
            )
            .color(colors::INK_SECONDARY),
        );
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.draft.enabled, "Hide ignored folders");
            if let Some(ix) = ix {
                let t = ix.ignored_total();
                let now = if ix.ignore_settings().enabled && t.folders > 0 {
                    format!(
                        "Hidden now: {} · {} · {}",
                        format::plural(t.folders, "folder", "folders"),
                        format::plural(t.files, "file", "files"),
                        format::bytes(t.bytes)
                    )
                } else {
                    "Nothing is hidden now".to_string()
                };
                ui.label(RichText::new(now).color(colors::INK_MUTED));
            }
        });
        ui.add_space(6.0);

        let hosts: Vec<String> = ix.map(|ix| ix.sources().iter().map(|s| s.host.clone()).collect()).unwrap_or_default();
        let effects = ix.map(|ix| (ix.ignore_settings().clone(), ix.ignore_effects().to_vec()));

        // the rule list: your own folders first, then the built-in rules
        let mut delete = None;
        let list_h = (ui.available_height() - 200.0).max(200.0);
        let order: Vec<usize> = (0..self.draft.rules.len())
            .filter(|&i| !self.draft.rules[i].builtin)
            .chain((0..self.draft.rules.len()).filter(|&i| self.draft.rules[i].builtin))
            .collect();
        let own = self.draft.rules.iter().filter(|r| !r.builtin).count();
        egui::Frame::new().fill(colors::SURFACE).corner_radius(4).inner_margin(6).show(ui, |ui| {
            egui::ScrollArea::vertical().max_height(list_h).auto_shrink([false, true]).show(ui, |ui| {
                egui::Grid::new("ignore-rules").num_columns(5).spacing([12.0, 6.0]).striped(true).show(ui, |ui| {
                    ui.label(RichText::new("On").color(colors::INK_MUTED));
                    ui.label(RichText::new("Folder or pattern").color(colors::INK_MUTED));
                    ui.label(RichText::new("Applies to").color(colors::INK_MUTED));
                    ui.label(RichText::new("Hides").color(colors::INK_MUTED));
                    ui.label("");
                    ui.end_row();
                    if own == 0 {
                        ui.label("");
                        ui.label(
                            RichText::new("No folders of your own yet — add one below")
                                .italics()
                                .color(colors::INK_MUTED),
                        );
                        ui.end_row();
                    }
                    for (n, &i) in order.iter().enumerate() {
                        if n == own {
                            ui.label("");
                            ui.label(RichText::new("Built in").strong().color(colors::INK_SECONDARY));
                            ui.end_row();
                        }
                        let rule = &mut self.draft.rules[i];
                        ui.add_enabled(self.draft.enabled, egui::Checkbox::without_text(&mut rule.enabled));
                        let valid = IgnoreRule::validate(&rule.pattern);
                        ui.horizontal(|ui| {
                            if rule.builtin {
                                ui.label(RichText::new(&rule.pattern).monospace().color(if rule.enabled {
                                    colors::INK
                                } else {
                                    colors::INK_MUTED
                                }));
                            } else {
                                ui.add(
                                    egui::TextEdit::singleline(&mut rule.pattern)
                                        .desired_width(330.0)
                                        .font(egui::TextStyle::Monospace),
                                );
                            }
                            if let Err(e) = &valid {
                                ui.label(RichText::new("⚠").color(colors::WARNING)).on_hover_text(e);
                            }
                        });
                        scope_combo(ui, ("scope", i), &mut rule.host, &hosts);
                        let effect = effect_of(rule, self.draft.enabled, effects.as_ref());
                        ui.label(effect);
                        if rule.builtin {
                            ui.label(RichText::new("default").small().color(colors::INK_MUTED))
                                .on_hover_text("Shipped with DupeMap. It can be switched off but not deleted.");
                        } else if ui.small_button("Remove").clicked() {
                            delete = Some(i);
                        }
                        ui.end_row();
                    }
                });
            });
        });
        if let Some(i) = delete {
            self.draft.rules.remove(i);
        }

        // add your own
        ui.add_space(8.0);
        ui.label(RichText::new("Add a folder").strong());
        let mut add = false;
        ui.horizontal(|ui| {
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.new_pattern)
                    .hint_text(r"D:\Games\Steam   or   **\node_modules")
                    .desired_width(360.0)
                    .font(egui::TextStyle::Monospace),
            );
            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                add = true;
            }
            #[cfg(windows)]
            if ui.button("Browse…").on_hover_text("Pick a folder on this computer").clicked() {
                if let Some(p) = rfd::FileDialog::new().set_title("Folder to ignore").pick_folder() {
                    self.new_pattern = p.display().to_string();
                }
            }
            scope_combo(ui, "new-scope", &mut self.new_host, &hosts);
            if ui.button("Add").clicked() {
                add = true;
            }
        });
        if add {
            self.add_rule();
        }
        if let Some(e) = &self.error {
            crate::widgets::status_label(ui, "⚠", colors::WARNING, e.clone());
        }
        ui.label(
            RichText::new(
                "Matches whole folder paths, ignoring case. ? = any one character, * = anything within one folder \
                 name, ** = any number of folders. Examples: D:\\Games\\Steam · ?:\\Temp · **\\node_modules · \
                 /mnt/tank/.recycle · gdrive:/Trash. Tip: right-click any folder in a tab and choose \"Ignore this folder\".",
            )
            .small()
            .color(colors::INK_MUTED),
        );

        ui.add_space(8.0);
        ui.separator();
        ui.horizontal(|ui| {
            if ui
                .button("Restore defaults")
                .on_hover_text("Switch the built-in rules back to their defaults; your own rules stay")
                .clicked()
            {
                restore_defaults(&mut self.draft);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let invalid = self.draft.rules.iter().any(|r| r.enabled && IgnoreRule::validate(&r.pattern).is_err());
                let can_apply = self.is_dirty() && !busy && !invalid;
                let apply = ui.add_enabled(can_apply, egui::Button::new(RichText::new("Apply").strong()));
                let apply = if busy {
                    apply.on_disabled_hover_text("Wait for the current task to finish")
                } else if invalid {
                    apply.on_disabled_hover_text("Fix the rules marked ⚠ first")
                } else {
                    apply
                };
                if apply.clicked() {
                    for r in &mut self.draft.rules {
                        r.pattern = r.pattern.trim().to_string();
                    }
                    actions.push(Action::SetIgnore(self.draft.clone()));
                }
                if ui.add_enabled(self.is_dirty(), egui::Button::new("Revert")).clicked() {
                    self.draft = self.base.clone();
                    self.error = None;
                }
                if self.is_dirty() {
                    ui.label(
                        RichText::new("Changes take effect when applied (the index is rebuilt)")
                            .color(colors::INK_MUTED),
                    );
                }
            });
        });
    }

    fn add_rule(&mut self) {
        let pattern = self.new_pattern.trim().trim_end_matches(['\\', '/']).to_string();
        let pattern = if pattern.is_empty() { self.new_pattern.trim().to_string() } else { pattern };
        if let Err(e) = IgnoreRule::validate(&pattern) {
            self.error = Some(e);
            return;
        }
        let dup = self.draft.rules.iter().any(|r| r.pattern.eq_ignore_ascii_case(&pattern) && r.host == self.new_host);
        if dup {
            self.error = Some("That rule is already in the list".into());
            return;
        }
        self.draft.rules.push(IgnoreRule::new(pattern, self.new_host.clone()));
        self.new_pattern.clear();
        self.error = None;
    }
}

fn scope_combo(ui: &mut Ui, id: impl std::hash::Hash + std::fmt::Debug, host: &mut Option<String>, hosts: &[String]) {
    let text = host.as_deref().map(format::host).unwrap_or("All sources").to_string();
    egui::ComboBox::from_id_salt(id).selected_text(text).width(190.0).height(420.0).show_ui(ui, |ui| {
        ui.selectable_value(host, None, "All sources");
        for h in hosts {
            ui.selectable_value(host, Some(h.clone()), format::host(h));
        }
        if let Some(h) = host.clone() {
            if !hosts.contains(&h) {
                ui.selectable_value(host, Some(h.clone()), format!("{} (not imported)", format::host(&h)));
            }
        }
    });
}

/// What a rule hid in the current index, if the index was built with this exact rule.
fn effect_of(rule: &IgnoreRule, master: bool, built: Option<&(IgnoreSettings, Vec<RuleEffect>)>) -> RichText {
    if !master || !rule.enabled {
        return RichText::new("off").color(colors::INK_MUTED);
    }
    let Some((settings, effects)) = built else { return RichText::new("") };
    let same = settings.enabled.then(|| {
        settings.rules.iter().position(|r| r.enabled && r.pattern.trim() == rule.pattern.trim() && r.host == rule.host)
    });
    match same.flatten().and_then(|i| effects.get(i)) {
        Some(e) if e.folders == 0 => RichText::new("nothing found").color(colors::INK_MUTED),
        Some(e) => RichText::new(format!(
            "{} · {} · {}",
            format::plural(e.folders, "folder", "folders"),
            format::plural(e.files, "file", "files"),
            format::bytes(e.bytes)
        ))
        .color(colors::INK_SECONDARY),
        None => RichText::new("apply to see").color(colors::INK_MUTED).italics(),
    }
}

fn restore_defaults(s: &mut IgnoreSettings) {
    s.enabled = true;
    for d in default_rules() {
        match s.rules.iter_mut().find(|r| r.builtin && r.pattern == d.pattern) {
            Some(r) => {
                r.enabled = d.enabled;
                r.host = None;
            }
            None => s.rules.push(d),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adding_rules_validates_and_rejects_duplicates() {
        let mut e = IgnoreEditor::new(&IgnoreSettings::default());
        let n = e.draft.rules.len();
        e.new_pattern = r"  D:\Games\Steam\  ".into();
        e.add_rule();
        assert_eq!(e.draft.rules.len(), n + 1);
        assert_eq!(e.draft.rules[n].pattern, r"D:\Games\Steam", "trimmed, trailing separator dropped");
        assert!(e.is_dirty());
        e.new_pattern = r"d:\games\steam".into();
        e.add_rule();
        assert_eq!(e.draft.rules.len(), n + 1, "same rule twice is refused");
        assert!(e.error.is_some());
        // same folder for one source only is a different rule
        e.new_host = Some("PC1".into());
        e.new_pattern = r"D:\Games\Steam".into();
        e.add_rule();
        assert_eq!(e.draft.rules.len(), n + 2);
        e.new_pattern = "**".into();
        e.add_rule();
        assert_eq!(e.draft.rules.len(), n + 2);
        assert!(e.error.is_some());
        // "/" alone stays a valid pattern (the whole Linux tree)
        e.new_host = None;
        e.new_pattern = "/".into();
        e.add_rule();
        assert_eq!(e.draft.rules.last().unwrap().pattern, "/");
    }

    #[test]
    fn restore_defaults_keeps_user_rules() {
        let mut s = IgnoreSettings { enabled: false, ..Default::default() };
        s.rules.retain(|r| r.pattern != r"?:\Windows");
        s.rules.iter_mut().for_each(|r| r.enabled = !r.enabled);
        s.rules.push(IgnoreRule::new(r"D:\x", None));
        restore_defaults(&mut s);
        assert!(s.enabled);
        assert!(s.rules.iter().any(|r| r.pattern == r"D:\x"));
        for d in default_rules() {
            let r = s.rules.iter().find(|r| r.pattern == d.pattern).unwrap();
            assert_eq!(r.enabled, d.enabled, "{}", d.pattern);
        }
    }

    #[test]
    fn rebase_keeps_unsaved_edits() {
        let mut e = IgnoreEditor::new(&IgnoreSettings::default());
        e.new_pattern = r"D:\y".into();
        e.add_rule();
        let mut applied = IgnoreSettings::default();
        applied.rules.push(IgnoreRule::new(r"D:\z", None));
        e.rebase(&applied);
        assert!(e.draft.rules.iter().any(|r| r.pattern == r"D:\y"), "unsaved draft kept");
        let mut clean = IgnoreEditor::new(&IgnoreSettings::default());
        clean.rebase(&applied);
        assert_eq!(clean.draft, applied, "a clean editor follows the applied settings");
    }
}
