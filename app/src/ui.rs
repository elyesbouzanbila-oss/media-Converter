//! All drawing code for the shell, as `impl App` blocks plus small free
//! helpers. State mutation happens only at well-defined points (button
//! results, row actions collected and applied after the table closure), so
//! egui's borrow rules never entangle the state machine.
//!
//! egui 0.37 note: `SidePanel`/`TopBottomPanel` were unified into a single
//! [`egui::Panel`] with `left/right/top/bottom` constructors, and every panel
//! `show` takes the parent `Ui` (which eframe hands the app directly).

use crate::app::{App, BottomTab};
use crate::state::{Job, JobStatus, OutputMode, OverwritePolicy};
use eframe::egui;
use egui_extras::{Column, TableBuilder};
use std::time::Duration;

// Colors from the theme where possible; the palette (theme.rs) for states.
fn status_color(status: &JobStatus, ui: &egui::Ui) -> egui::Color32 {
    match status {
        JobStatus::Queued => ui.visuals().weak_text_color(),
        JobStatus::Running => crate::theme::TEAL,
        JobStatus::Done => crate::theme::ORANGE,
        JobStatus::Failed => crate::theme::RED,
        // A cancelled job reads as "dead": red drained back toward the navy,
        // so it cannot be mistaken for a live failure (pure `RED` on the
        // navy) and differs from the queued grey.
        JobStatus::Cancelled => crate::theme::blend(crate::theme::RED, crate::theme::NAVY, 0.45),
    }
}

const COLOR_WARN: egui::Color32 = crate::theme::ORANGE;

fn fmt_elapsed(d: Duration) -> String {
    let s = d.as_secs_f32();
    if s < 60.0 {
        format!("{s:.1}s")
    } else {
        format!("{:.0}m {:.0}s", s / 60.0, s % 60.0)
    }
}

fn path_display(p: &std::path::Path, max_chars: usize) -> String {
    let s = p.display().to_string();
    let len = s.chars().count();
    if len > max_chars {
        format!("…{}", s.chars().skip(len - max_chars).collect::<String>())
    } else {
        s
    }
}

impl App {
    /// egui leaves dropped files in the input until consumed; take them once
    /// per frame. Directories are expanded by `collect_files` in state.rs.
    pub(crate) fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped: Vec<std::path::PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect()
        });
        if !dropped.is_empty() {
            self.add_files(dropped);
        }
    }

    pub(crate) fn engine_in_flight(&self) -> usize {
        self.in_flight
    }
}

// ---------------------------------------------------------------------------
// Top bar
// ---------------------------------------------------------------------------

pub fn draw_top_bar(app: &mut App, ui: &mut egui::Ui) {
    egui::Panel::top("top_bar").show(ui, |ui| {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.heading("dconvert");
            ui.weak("document converter");
            if !app.engine_alive() {
                ui.colored_label(ui.visuals().error_fg_color, "engine unavailable");
            } else if app.busy() {
                if let Some(p) = app.queue.batch_progress() {
                    ui.add(
                        egui::ProgressBar::new(p)
                            .desired_width(160.0)
                            .show_percentage(),
                    );
                }
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let gear_label = if app.settings_visible {
                    "Settings ⌄"
                } else {
                    "Settings ›"
                };
                if ui.add(egui::Button::new(gear_label)).clicked() {
                    app.settings_visible = !app.settings_visible;
                }

                let counts = app.counts();
                let finished = counts.done + counts.failed + counts.cancelled;
                if ui
                    .add_enabled(finished > 0, egui::Button::new("Clear finished"))
                    .clicked()
                {
                    let cleared = app.queue.clear_finished();
                    app.log
                        .lock()
                        .push_timed(format!("cleared {cleared} finished job(s)"));
                }

                let cancellable =
                    (counts.queued > 0 || counts.running > 0 || app.engine_in_flight() > 0)
                        && app.engine_alive();
                if ui
                    .add_enabled(
                        cancellable,
                        // Danger surface, not danger text: the crimson needs
                        // a surface to pass contrast on (theme.rs).
                        egui::Button::new("Cancel all").fill(crate::theme::CRIMSON),
                    )
                    .clicked()
                {
                    app.cancel_all();
                }

                let start_label = if counts.queued > 0 {
                    format!("Start ({} queued)", counts.queued)
                } else {
                    "Start".to_owned()
                };
                if ui
                    .add_enabled(
                        counts.queued > 0 && app.engine_alive(),
                        // The one call to action: the accent itself for a
                        // surface, navy text on it for contrast rather than
                        // the default light text, which fails on teal.
                        egui::Button::new(
                            egui::RichText::new(start_label)
                                .color(crate::theme::NAVY)
                                .strong(),
                        )
                        .fill(crate::theme::TEAL),
                    )
                    .clicked()
                {
                    app.start_batch();
                }

                ui.separator();
                if ui.button("Add folder…").clicked() {
                    app.add_folder_from_dialog();
                }
                if ui.button("Add files…").clicked() {
                    app.add_files_from_dialog();
                }
            });
        });
        ui.add_space(4.0);
    });
}

pub fn draw_settings_panel(app: &mut App, ui: &mut egui::Ui) {
    if !app.settings_visible {
        return;
    }
    egui::Panel::right("settings_panel")
        .min_size(240.0)
        .max_size(340.0)
        .default_size(280.0)
        .resizable(true)
        .show(ui, |ui| {
            ui.add_space(6.0);
            ui.strong("Target format");
            egui::ComboBox::from_id_salt("target_format")
                .selected_text(app.settings.target.label())
                .show_ui(ui, |ui| {
                    for f in crate::engine::supported_targets() {
                        ui.selectable_value(&mut app.settings.target, f, f.label());
                    }
                });
            ui.add_space(8.0);

            ui.strong("Output folder");
            egui::ComboBox::from_id_salt("output_mode")
                .selected_text(match app.settings.output_mode {
                    OutputMode::SourceFolder => "Next to each source",
                    OutputMode::Custom => "One folder",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut app.settings.output_mode,
                        OutputMode::SourceFolder,
                        "Next to each source",
                    );
                    ui.selectable_value(
                        &mut app.settings.output_mode,
                        OutputMode::Custom,
                        "One folder for everything",
                    );
                });
            if app.settings.output_mode == OutputMode::Custom {
                let dir_text = app
                    .settings
                    .custom_dir
                    .as_ref()
                    .map(|p| path_display(p, 34))
                    .unwrap_or_else(|| "(not set)".to_owned());
                if ui.button("Browse…").on_hover_text(&dir_text).clicked() {
                    app.pick_output_folder();
                }
                ui.weak(dir_text);
            }
            ui.add_space(8.0);

            ui.strong("Overwrite policy");
            egui::ComboBox::from_id_salt("overwrite")
                .selected_text(match app.settings.overwrite {
                    OverwritePolicy::AutoRename => "Auto-rename",
                    OverwritePolicy::Overwrite => "Overwrite files",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut app.settings.overwrite,
                        OverwritePolicy::AutoRename,
                        "Auto-rename (report-1.docx…)",
                    );
                    ui.selectable_value(
                        &mut app.settings.overwrite,
                        OverwritePolicy::Overwrite,
                        "Overwrite existing files",
                    );
                });
            ui.add_space(8.0);

            ui.strong("Worker processes");
            ui.add(egui::Slider::new(&mut app.settings.workers, 1..=16));
            ui.horizontal(|ui| {
                if ui
                    .button("Use suggested")
                    .on_hover_text(
                        "Phase 0 (finding C1): throughput saturates around 8 workers on \
                         this machine; more only adds churn.",
                    )
                    .clicked()
                {
                    app.settings.workers = crate::state::default_worker_count() as u32;
                }
                ui.weak(format!(
                    "suggested: {}",
                    crate::state::default_worker_count()
                ));
            });
            ui.add_space(8.0);

            ui.strong("Theme");
            if ui
                .toggle_value(&mut app.settings.dark_theme, "Dark mode")
                .clicked()
            {
                app.apply_theme(ui.ctx());
            }
            ui.add_space(8.0);
            ui.weak(
                "Files convert in a pool of worker processes around the real \
                 facade. During UI review the demo engine swaps in one line — \
                 a file whose name contains “fail” fails on purpose, “warn” \
                 produces warnings, and nothing is written to disk.",
            );
        });
}

// ---------------------------------------------------------------------------
// Queue (left panel)
// ---------------------------------------------------------------------------

enum RowAction {
    Select(crate::state::JobId),
    Remove(crate::state::JobId),
}

pub fn draw_queue_panel(app: &mut App, ui: &mut egui::Ui) {
    egui::Panel::left("queue_panel")
        .min_size(430.0)
        .default_size(700.0)
        .resizable(true)
        .show(ui, |ui| {
            ui.add_space(6.0);
            let counts = app.counts();
            ui.horizontal(|ui| {
                ui.strong("Queue");
                ui.weak(format!(
                    "{} jobs · {} running · {} queued · {} done · {} failed · {} cancelled",
                    counts.total,
                    counts.running,
                    counts.queued,
                    counts.done,
                    counts.failed,
                    counts.cancelled
                ));
            });
            ui.add_space(2.0);

            let jobs: Vec<Job> = app.queue.jobs().to_vec();
            if jobs.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.weak("The queue is empty. Use “Add files…”, or drop files anywhere.");
                });
                return;
            }

            let selected = app.selected;
            let mut actions: Vec<RowAction> = Vec::new();

            TableBuilder::new(ui)
                .vscroll(true)
                .auto_shrink(false)
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::exact(84.0))
                .column(Column::remainder())
                .column(Column::exact(104.0))
                .column(Column::exact(146.0))
                .column(Column::exact(58.0))
                .column(Column::exact(46.0))
                .column(Column::exact(32.0))
                .header(24.0, |mut header| {
                    header.col(|ui| {
                        ui.label("State");
                    });
                    header.col(|ui| {
                        ui.label("File");
                    });
                    header.col(|ui| {
                        ui.label("Format");
                    });
                    header.col(|ui| {
                        ui.label("Progress");
                    });
                    header.col(|ui| {
                        ui.label("Time");
                    });
                    header.col(|ui| {
                        ui.label("Warn");
                    });
                    header.col(|_ui| {});
                })
                .body(|mut body| {
                    for job in &jobs {
                        let can_remove = job.status != JobStatus::Running;
                        body.row(24.0, |mut row| {
                            let mut action: Option<RowAction> = None;

                            row.col(|ui| {
                                let color = status_color(&job.status, ui);
                                ui.colored_label(color, job.status.label());
                            });

                            row.col(|ui| {
                                if ui
                                    .selectable_label(selected == Some(job.id), job.source_name())
                                    .clicked()
                                {
                                    action = Some(RowAction::Select(job.id));
                                }
                            });

                            row.col(|ui| {
                                ui.horizontal(|ui| {
                                    ui.label(source_ext(&job.source));
                                    ui.weak("→");
                                    ui.label(job.target.extension());
                                });
                            });

                            row.col(|ui| progress_cell(ui, &job));

                            row.col(|ui| {
                                if let Some(e) = job.live_elapsed() {
                                    ui.weak(fmt_elapsed(e));
                                } else {
                                    ui.weak("—");
                                }
                            });

                            row.col(|ui| {
                                let n = job.warnings.len();
                                if n > 0 {
                                    ui.colored_label(COLOR_WARN, n.to_string());
                                } else {
                                    ui.weak("—");
                                }
                            });

                            row.col(|ui| {
                                if can_remove {
                                    let btn = ui
                                        .add_sized([24.0, 18.0], egui::Button::new("✕"))
                                        .on_hover_text("Remove from queue");
                                    if btn.clicked() {
                                        action = Some(RowAction::Remove(job.id));
                                    }
                                }
                            });

                            if let Some(a) = action {
                                actions.push(a);
                            }
                        });
                    }
                });

            for action in actions {
                match action {
                    RowAction::Select(id) => app.selected = Some(id),
                    RowAction::Remove(id) => {
                        let removed = app.remove_job(id);
                        if removed.is_some() && app.selected == Some(id) {
                            app.selected = None;
                        }
                    }
                }
            }
        });
}

fn source_ext(path: &std::path::Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().into_owned().to_lowercase())
        .unwrap_or_else(|| "?".to_owned())
}

fn progress_cell(ui: &mut egui::Ui, job: &Job) {
    match job.status {
        JobStatus::Running => match job.progress {
            Some(f) => {
                ui.add(
                    egui::ProgressBar::new(f.clamp(0.0, 1.0))
                        .desired_width(138.0)
                        .show_percentage(),
                );
            }
            None => {
                ui.add(egui::Spinner::new().size(14.0));
            }
        },
        JobStatus::Queued => {
            ui.weak("—");
        }
        JobStatus::Done => {
            let time = job.elapsed.map(fmt_elapsed).unwrap_or_default();
            ui.colored_label(
                status_color(&job.status, ui),
                if time.is_empty() {
                    "done".to_owned()
                } else {
                    format!("done · {time}")
                },
            );
        }
        JobStatus::Failed => {
            ui.colored_label(status_color(&job.status, ui), "failed")
                .on_hover_text(job.detail.clone().unwrap_or_default());
        }
        JobStatus::Cancelled => {
            ui.colored_label(status_color(&job.status, ui), "cancelled");
        }
    }
}

// ---------------------------------------------------------------------------
// Central (drop zone / empty state)
// ---------------------------------------------------------------------------

pub fn draw_central(app: &mut App, ui: &mut egui::Ui) {
    let hovering = ui.ctx().input(|i| !i.raw.hovered_files.is_empty());
    // Theme-driven: the deep inset paints the drop surface — the old frame
    // was transparent, and the wgpu clear colour bled through it as black —
    // and the selection accent strokes and tints the frame while files are
    // dragged over it.
    let accent = ui.visuals().selection.stroke.color;
    // The same navy as the panels, not an inset: a drop zone this tall was
    // painted as its own dark column and read as foreign to the palette.
    // The region is carried by the centred hint and the drag-hover accent,
    // which used to bleed the wgpu clear colour through the transparent
    // frame as black.
    let base = ui.visuals().panel_fill;
    let mut frame = egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(14, 10))
        .fill(base);
    if hovering {
        frame = frame
            .stroke(egui::Stroke::new(2.5, accent))
            .fill(crate::theme::blend(base, accent, 0.1));
    }
    egui::CentralPanel::default().frame(frame).show(ui, |ui| {
        let counts = app.counts();
        if counts.total == 0 {
            ui.centered_and_justified(|ui| {
                ui.heading("Drop documents here");
                ui.add_space(10.0);
                ui.weak("…or use “Add files…”. Folders are expanded recursively.");
            });
        } else {
            ui.centered_and_justified(|ui| {
                ui.weak("Tip: click a job to see its details in the bottom panel.");
            });
        }
    });
}

// ---------------------------------------------------------------------------
// Bottom tabs + status bar
// ---------------------------------------------------------------------------

pub fn draw_bottom_tabs(app: &mut App, ui: &mut egui::Ui) {
    egui::Panel::bottom("bottom_tabs")
        .min_size(120.0)
        .default_size(190.0)
        .resizable(true)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                for (label, tab) in [
                    ("Details", BottomTab::Details),
                    ("Log", BottomTab::Log),
                    ("Warnings", BottomTab::Warnings),
                ] {
                    let count = match tab {
                        BottomTab::Warnings => app.warnings.len(),
                        _ => 0,
                    };
                    let text = if count > 0 {
                        format!("{label} ({count})")
                    } else {
                        label.to_owned()
                    };
                    if ui.selectable_label(app.bottom_tab == tab, text).clicked() {
                        app.bottom_tab = tab;
                    }
                }
            });
            ui.add_space(4.0);
            egui::ScrollArea::vertical()
                .auto_shrink(false)
                .show(ui, |ui| match app.bottom_tab {
                    BottomTab::Details => draw_details(app, ui),
                    BottomTab::Log => draw_log(app, ui),
                    BottomTab::Warnings => draw_warnings(app, ui),
                });
        });
}

fn draw_details(app: &App, ui: &mut egui::Ui) {
    let Some(job) = app.selected.and_then(|id| app.queue.job(id)) else {
        ui.weak("Click a job in the queue to see its details here.");
        ui.weak("Warnings and errors accumulate per job: they are shown here and in the console.");
        return;
    };
    let job = job.clone();
    egui::Grid::new("details_grid")
        .num_columns(2)
        .spacing([8.0, 4.0])
        .show(ui, |ui| {
            ui.strong("File");
            ui.label(job.source.display().to_string());
            ui.end_row();

            ui.strong("Target");
            ui.label(job.target.label());
            ui.end_row();

            ui.strong("Output");
            match &job.output {
                Some(out) => ui.label(out.display().to_string()),
                None => ui.weak("— (resolved when the batch starts)"),
            };
            ui.end_row();

            ui.strong("State");
            ui.label(job.status.label());
            ui.end_row();

            if let Some(e) = job.elapsed {
                ui.strong("Elapsed");
                ui.label(fmt_elapsed(e));
                ui.end_row();
            }
            if !job.warnings.is_empty() {
                ui.strong("Warnings");
                ui.vertical(|w| {
                    for line in &job.warnings {
                        w.colored_label(COLOR_WARN, line);
                    }
                });
                ui.end_row();
            }
            if let Some(detail) = &job.detail {
                ui.strong("Detail");
                ui.label(detail);
                ui.end_row();
            }
        });
}

fn draw_log(app: &App, ui: &mut egui::Ui) {
    let (dropped, lines) = {
        let guard = app.log.lock();
        (guard.dropped(), guard.tail(600))
    };
    if dropped > 0 {
        ui.weak(format!(
            "… {dropped} older line(s) dropped by the ring buffer"
        ));
    }
    for line in lines {
        let color = if line.contains("FAILED") {
            ui.visuals().error_fg_color
        } else if line.contains("WARN") {
            COLOR_WARN
        } else {
            ui.visuals().text_color()
        };
        ui.colored_label(color, line);
    }
}

fn draw_warnings(app: &App, ui: &mut egui::Ui) {
    if app.warnings.is_empty() {
        ui.weak("No warnings so far. Conversion warnings (lost hyphens, reordered text, …) appear here instead of staying silent about quality.");
        return;
    }
    for w in app.warnings.iter().rev() {
        ui.colored_label(COLOR_WARN, w);
    }
}

pub fn draw_status_bar(app: &mut App, ui: &mut egui::Ui) {
    egui::Panel::bottom("status_bar")
        .exact_size(24.0)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let counts = app.counts();
                let engine = if !app.engine_alive() {
                    "engine: unavailable".to_owned()
                } else if app.busy() {
                    // The status bar deliberately does not name the engine
                    // kind: production runs the worker-process pool and the
                    // demo swap (one line in main.rs) must not lie either.
                    format!(
                        "engine: running ({} in flight, {} worker(s))",
                        app.engine_in_flight(),
                        app.settings.workers_or_default()
                    )
                } else {
                    format!(
                        "engine: idle ({} worker(s))",
                        app.settings.workers_or_default()
                    )
                };
                ui.weak(engine);
                ui.separator();
                ui.weak(format!(
                    "{} job(s) · {} running · {} done · {} failed · {} warnings",
                    counts.total,
                    counts.running,
                    counts.done,
                    counts.failed,
                    app.warnings.len()
                ));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.weak("dconv-app");
                });
            });
        });
}

impl App {
    /// Remove one job, logging it; the queue's own guard keeps running jobs
    /// in place.
    pub(crate) fn remove_job(&mut self, id: crate::state::JobId) -> Option<crate::state::Job> {
        let job = self.queue.peek(id);
        if self.queue.remove(id) {
            if let Some(job) = &job {
                self.log
                    .lock()
                    .push_timed(format!("removed job {}", job.source_name()));
            }
            job
        } else {
            None
        }
    }
}
