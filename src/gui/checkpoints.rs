//! Cache Checkpoint management UI (缓存关键帧).
//!
//! Shows every stored checkpoint with its creation time and how many caches
//! appeared, changed or disappeared after it, and lets the user create,
//! rename, delete and switch between checkpoints. Switching only changes
//! which time boundary the asset tabs filter by — it never touches cache data.

use fluent_bundle::{FluentArgs, FluentBundle, FluentResource};
use std::sync::Arc;

use crate::logic::{self, checkpoints};
use crate::locale;

pub struct CheckpointUi {
    /// Whether the full-cache refresh has been requested on first entry.
    entered: bool,
    /// Generation the cached diffs were computed for.
    last_generation: u64,
    /// Checkpoint ids the cached diffs were computed for.
    last_ids: Vec<String>,
    /// Cached diff results per checkpoint id.
    cached_diffs: Vec<(String, checkpoints::CheckpointDiff)>,
    /// Checkpoint currently being renamed.
    renaming: Option<String>,
    rename_text: String,
}

impl CheckpointUi {
    pub fn new() -> Self {
        Self {
            entered: false,
            last_generation: u64::MAX,
            last_ids: Vec::new(),
            cached_diffs: Vec::new(),
            renaming: None,
            rename_text: String::new(),
        }
    }

    /// Recompute cached diffs when a refresh finished or the checkpoint list
    /// changed; returns a copy of the diff for `id` (or an empty diff).
    fn diff_for(&mut self, id: &str) -> checkpoints::CheckpointDiff {
        let current = checkpoints::get_current_cache();
        let generation = checkpoints::get_current_generation();
        let checkpoints = checkpoints::get_checkpoints();
        let ids: Vec<String> = checkpoints.iter().map(|cp| cp.id.clone()).collect();

        if generation != self.last_generation || ids != self.last_ids {
            self.last_generation = generation;
            self.last_ids = ids;
            self.cached_diffs = checkpoints
                .iter()
                .map(|cp| (cp.id.clone(), checkpoints::diff_checkpoint(cp, &current)))
                .collect();
        }

        self.cached_diffs
            .iter()
            .find(|(cached_id, _)| cached_id == id)
            .map(|(_, diff)| diff.clone())
            .unwrap_or_default()
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, locale: &FluentBundle<Arc<FluentResource>>) {
        if !self.entered {
            self.entered = true;
            checkpoints::refresh_current_cache_async();
        }

        ui.heading(locale::get_message(locale, "cache-checkpoints", None));
        ui.label(locale::get_message(
            locale,
            "cache-checkpoints-description",
            None,
        ));

        ui.add_space(6.0);

        ui.horizontal(|ui| {
            let create_button = ui.add_enabled(
                !logic::get_task_running(),
                egui::Button::new(locale::get_message(locale, "checkpoint-create", None)),
            );
            if create_button.clicked() {
                checkpoints::create_checkpoint();
            }

            let refresh_button = ui.add_enabled(
                !logic::get_task_running(),
                egui::Button::new(locale::get_message(locale, "button-refresh", None)),
            );
            if refresh_button.clicked() {
                checkpoints::refresh_current_cache_async();
            }

            if logic::get_task_running() {
                ui.spinner();
                ui.label(locale::get_message(locale, "checkpoint-creating", None));
            }
        });

        // A checkpoint can be disabled again to return to the unfiltered view.
        if checkpoints::get_active_checkpoint_id().is_some() {
            ui.horizontal(|ui| {
                let disable_button = ui.add_enabled(
                    !logic::get_task_running(),
                    egui::Button::new(locale::get_message(locale, "checkpoint-disable", None)),
                );
                if disable_button.clicked() {
                    checkpoints::set_active_checkpoint(None);
                }
            });
        }

        // Refreshing the checkpoint tab also refreshes the shared file list,
        // so the change counts are always based on the latest cache state.
        ui.add_space(4.0);
        ui.label(locale::get_message(locale, "checkpoint-filter-description", None));
        ui.separator();

        let active = checkpoints::get_active_checkpoint_id();
        let checkpoints = checkpoints::get_checkpoints();

        if checkpoints.is_empty() {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| {
                ui.heading(locale::get_message(locale, "checkpoint-empty", None));
            });
            return;
        }

        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            for checkpoint in &checkpoints {
                let is_active = active.as_deref() == Some(checkpoint.id.as_str());
                let diff = self.diff_for(&checkpoint.id);

                let name = if is_active {
                    format!("▶ {}", checkpoint.name)
                } else {
                    checkpoint.name.clone()
                };
                ui.horizontal(|ui| {
                    ui.strong(name);
                    let mut args = FluentArgs::new();
                    let time = chrono::Local
                        .timestamp_opt(checkpoint.created_at_secs, 0)
                        .single()
                        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
                        .unwrap_or_default();
                    args.set("time", time);
                    ui.label(locale::get_message(
                        locale,
                        "checkpoint-created-at",
                        Some(&args),
                    ));
                });

                ui.horizontal(|ui| {
                    let mut args = FluentArgs::new();
                    args.set("count", diff.added.len());
                    ui.label(locale::get_message(locale, "checkpoint-added", Some(&args)));
                    args.set("count", diff.modified.len());
                    ui.label(locale::get_message(locale, "checkpoint-modified", Some(&args)));
                    args.set("count", diff.removed.len());
                    ui.label(locale::get_message(locale, "checkpoint-removed", Some(&args)));
                });

                ui.horizontal(|ui| {
                    if ui
                        .button(locale::get_message(locale, "checkpoint-view", None))
                        .clicked()
                    {
                        checkpoints::set_active_checkpoint(Some(checkpoint.id.clone()));
                    }
                    if ui
                        .button(locale::get_message(locale, "checkpoint-rename", None))
                        .clicked()
                    {
                        self.renaming = Some(checkpoint.id.clone());
                        self.rename_text = checkpoint.name.clone();
                    }
                    if ui
                        .button(locale::get_message(locale, "checkpoint-delete", None))
                        .clicked()
                    {
                        if checkpoints::delete_checkpoint(&checkpoint.id) {
                            // Invalidate the diff cache entry.
                            self.last_ids.clear();
                            self.renaming = None;
                        }
                    }
                });

                if self.renaming.as_deref() == Some(checkpoint.id.as_str()) {
                    let response = ui.text_edit_singleline(&mut self.rename_text);
                    let confirmed = response.lost_focus()
                        || ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if confirmed {
                        let trimmed = self.rename_text.trim().to_owned();
                        if !trimmed.is_empty() {
                            checkpoints::rename_checkpoint(&checkpoint.id, &trimmed);
                        }
                        self.renaming = None;
                    }
                }

                ui.separator();
            }
        });
    }
}

impl Default for CheckpointUi {
    fn default() -> Self {
        Self::new()
    }
}
