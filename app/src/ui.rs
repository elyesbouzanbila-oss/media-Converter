//! All drawing code for the shell, as `impl App` blocks plus small free
//! helpers. State mutation happens only at well-defined points (button
//! results, row actions collected and applied after the table closure), so
//! egui's borrow rules never entangle the state machine.
//!
//! egui 0.37 note: `SidePanel`/`TopBottomPanel` were unified into a single
//! [`egui::Panel`] with `left/right/top/bottom` constructors, and every panel
//! `show` takes the parent `Ui` (which eframe hands the app directly).

use crate::app::{App, BottomTab, QueueSort};
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
        JobStatus::Failed => crate::theme::RED_TEXT,
        // Cancelled joins queued in the weak grey: a dead state, told apart
        // by its word and its colour-free company rather than by hue. Every
        // drained-red mixture was measured before this decision (blend(RED,
        // NAVY, 45 %) ≈ 2.0:1, 20 % ≈ 3.0:1 on the navy) — none passed
        // 4.5:1, and a tint that dim reads as *a failure that lost its
        // colour*, which is precisely the confusion it was chosen to avoid.
        // Both states sit in the "not live" grey family; the label carries
        // the difference.
        JobStatus::Cancelled => ui.visuals().weak_text_color(),
    }
}

const COLOR_WARN: egui::Color32 = crate::theme::ORANGE;

/// "1 document" / "3 documents" — nothing user-facing prints a bare "(s)".
fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

fn fmt_elapsed(d: Duration) -> String {
    let s = d.as_secs_f32();
    if s < 60.0 {
        format!("{s:.1}s")
    } else {
        format!("{:.0}m {:.0}s", s / 60.0, s % 60.0)
    }
}

/// Batch-remaining display. Ceilings rather than roundings so the count
/// never reads "~0s" while pages are still turning; distances past an hour
/// are presented hourly because a seconds-precision hour-long claim from a
/// 12 s window would be a guess with decimals on it.
fn fmt_eta(d: Duration) -> String {
    let s = d.as_secs_f32();
    if s < 60.0 {
        format!("~{}s left", s.ceil() as u32)
    } else if s < 3600.0 {
        format!("~{}m left", (s / 60.0).round() as u32)
    } else {
        format!("~{}h left", (s / 3600.0).round() as u32)
    }
}

/// Tail-and-head-keeping clip: both the stem and the extension stay visible,
/// which is what distinguishes two same-directory files at a glance
/// (lists-and-tables.md › Content). Ready to be tightened further by
/// `Label::truncate()` at the cell edge, which clips in the middle but only
/// in the rare case this budget still overflows.
fn middle_ellipsis(s: &str, budget: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    let head = (budget - 1) / 2;
    let tail = budget - 1 - head;
    if chars.len() <= budget || head == 0 || tail == 0 {
        return s.to_owned();
    }
    let head: String = chars[..head.min(chars.len())].iter().collect();
    let tail: String = chars[chars.len() - tail..].iter().collect();
    format!("{head}…{tail}")
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
            // Leading edge: state, not branding. The native title bar
            // already names the app, and the same words printed twice at the
            // window's own prime spots is the one piece of chrome this
            // utility does not need.
            let counts = app.counts();
            if !app.engine_alive() {
                ui.colored_label(ui.visuals().error_fg_color, "engine unavailable");
                if ui.button("Restart engine").clicked() {
                    app.restart_engine();
                }
                ui.weak("files failed during the outage can be retried afterwards");
            } else if app.converting() {
                if let Some(p) = app.queue.batch_progress() {
                    ui.add(
                        egui::ProgressBar::new(p)
                            .desired_width(160.0)
                            .show_percentage(),
                    );
                }
                // Determinate when the pool's page rate supports it; the
                // estimate disappears rather than guessing while the rate
                // is unreadable early in a batch.
                if let Some(eta) = app.eta {
                    ui.weak(fmt_eta(eta));
                }
            } else if counts.queued > 0 {
                let n = counts.queued;
                if n == 1 {
                    ui.weak("1 document queued");
                } else {
                    ui.weak(format!("{n} documents queued"));
                }
            } else if counts.total > 0 {
                ui.weak("batch finished");
            } else {
                ui.weak("drop documents to queue them");
            }

            // The setting people look at before Start, readable with the
            // settings panel closed.
            ui.weak(format!("→ {}", app.settings.target.label()));

            // The one recovery action a failed batch can need after the
            // restart: only the disconnect-failed jobs get re-queued here —
            // real conversion failures would fail identically again.
            if app.engine_alive() && !app.converting() {
                let retryable = app
                    .queue
                    .jobs()
                    .iter()
                    .filter(|j| {
                        j.status == JobStatus::Failed
                            && j.detail
                                .as_deref()
                                .map_or(false, |d| d.starts_with("engine unavailable"))
                    })
                    .count();
                if retryable > 0
                    && ui
                        .button(format!("Retry {}", plural(retryable, "failed document")))
                        .on_hover_text("Re-queue the documents that failed when the engine went away")
                        .clicked()
                {
                    app.retry_engine_failed();
                }
            }

            // Settings on the leading side: infrequent, and the trailing
            // cluster belongs to the batch actions (toolbars.md › Item
            // groupings — the primary action on the trailing side).
            let gear_label = if app.settings_visible {
                "Settings ⌄"
            } else {
                "Settings ›"
            };
            if ui
                .button(gear_label)
                .on_hover_text(
                    "Target format, output folder, overwrite policy and workers (Escape closes)",
                )
                .clicked()
            {
                app.settings_visible = !app.settings_visible;
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Fresh for this frame: the leading branch may already have
                // run a batch (Enter/Start), and enabled-states must react
                // in the same frame they change.
                let counts = app.counts();
                // Added first ⇒ rightmost: Start is the window's one
                // prominent action and sits trailing-most, Cancel all next,
                // Clear finished last of the batch three.
                let start_label = if counts.queued > 0 {
                    format!("Start ({} queued)", counts.queued)
                } else {
                    "Start".to_owned()
                };
                let start = ui
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
                    .on_hover_text("Convert every queued document (Enter)")
                    // A greyed button with no reason is a dead end; disabled
                    // hover text is where egui puts the explanation.
                    .on_disabled_hover_text(if !app.engine_alive() {
                        "The engine is unavailable — restart it first"
                    } else {
                        "Add documents to start"
                    });
                if start.clicked() {
                    app.start_batch();
                }

                let cancellable =
                    (counts.queued > 0 || counts.running > 0 || app.engine_in_flight() > 0)
                        && app.engine_alive();
                let cancel = ui
                    .add_enabled(
                        cancellable,
                        // Danger surface, not danger text: the crimson needs
                        // a surface to pass contrast on (theme.rs).
                        egui::Button::new("Cancel all").fill(crate::theme::CRIMSON),
                    )
                    .on_hover_text(
                        "Stop after the page in progress — files already written are kept",
                    )
                    .on_disabled_hover_text("Nothing is converting");
                if cancel.clicked() {
                    app.cancel_all();
                }

                let finished = counts.done + counts.failed + counts.cancelled;
                let clear = ui
                    .add_enabled(finished > 0, egui::Button::new("Clear finished"))
                    .on_hover_text("Remove done, failed and cancelled rows from the queue")
                    .on_disabled_hover_text("No finished documents to clear");
                if clear.clicked() {
                    let cleared = app.queue.clear_finished();
                    app.log
                        .lock()
                        .push_timed(format!("cleared {cleared} finished job(s)"));
                }

                ui.separator();
                if ui
                    .button("Add files…")
                    .on_hover_text("Choose documents to convert (Ctrl+O)")
                    .clicked()
                {
                    app.add_files_from_dialog();
                }
                if ui
                    .button("Add folder…")
                    .on_hover_text("Add every readable document in a folder and its subfolders")
                    .clicked()
                {
                    app.add_folder_from_dialog();
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
                        "Documents convert in parallel; throughput peaks at the \
                         suggested number on most machines, and adding more \
                         workers past it only adds process churn.",
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
            // Just the name: the exact counts live once, in the status bar,
            // and the empty state lives once, in the central drop zone —
            // both used to be printed here as well, and a batch converter's
            // periphery does not need three copies of its own numbers.
            ui.strong("Queue");
            ui.add_space(2.0);

            if app.queue.jobs().is_empty() {
                return;
            }

            let selected = app.selected;
            let sort = app.queue_sort;
            // Sorted view, never a reorder: the queue's own promise is
            // arrival order (Queue::add), and the table's job is to show it
            // the way the user asked to see it. The snapshot is cloned, like
            // the pre-sorting table always did — the header buttons mutate
            // `queue_sort` and the rows may mutate the queue, so a borrowed
            // view would fight the closure captures it crosses.
            let mut jobs: Vec<Job> = app.queue.jobs().iter().cloned().collect();
            match sort {
                QueueSort::Arrival => {}
                QueueSort::StateAsc | QueueSort::StateDesc => {
                    // Ascending reads as attention: failed, running, queued,
                    // cancelled, done. The stable sort keyed directly by
                    // status keeps ties in arrival order in *both*
                    // directions (a plain `reverse()` on the vec would
                    // scramble them in the descending view).
                    let severity = |job: &Job| match job.status {
                        JobStatus::Failed => 0u8,
                        JobStatus::Running => 1,
                        JobStatus::Queued => 2,
                        JobStatus::Cancelled => 3,
                        JobStatus::Done => 4,
                    };
                    match sort {
                        QueueSort::StateDesc => {
                            jobs.sort_by_key(|job| std::cmp::Reverse(severity(job)))
                        }
                        _ => jobs.sort_by_key(|job| severity(job)),
                    }
                }
                QueueSort::NameAsc | QueueSort::NameDesc => {
                    match sort {
                        QueueSort::NameDesc => jobs.sort_by_key(|job| {
                            std::cmp::Reverse(job.source_name().to_lowercase())
                        }),
                        _ => jobs.sort_by_key(|job| job.source_name().to_lowercase()),
                    }
                }
            }

            let mut actions: Vec<RowAction> = Vec::new();

            // A column sorts by cycling: arrival → ascending → descending →
            // arrival. The third click means "put it back" and is worth the
            // extra state over a two-state toggle that strands a sorted
            // view at maximum. Directed arrows say which half is showing.
            let name_arrow = match sort {
                QueueSort::NameAsc => " ↑",
                QueueSort::NameDesc => " ↓",
                _ => "",
            };
            let state_arrow = match sort {
                QueueSort::StateAsc => " ↑",
                QueueSort::StateDesc => " ↓",
                _ => "",
            };

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
                        let button = ui
                            .button(format!("State{state_arrow}"))
                            .on_hover_text("Sort by state — failed first (click again to reverse)");
                        // The arrow is the only visible sign of the sort
                        // order; AccessKit gets the same fact in words, and
                        // the same pattern is used for File below.
                        ui.ctx().accesskit_node_builder(button.id, |node| {
                            node.set_label(match sort {
                                QueueSort::StateAsc => "State, sorted ascending: failed first. Activate to sort descending.",
                                QueueSort::StateDesc => "State, sorted descending. Activate to return to arrival order.",
                                _ => "State, unsorted. Activate to sort, failed documents first.",
                            });
                        });
                        if button.clicked() {
                            app.queue_sort = cycle_sort(sort, QueueSort::StateAsc);
                        }
                    });
                    header.col(|ui| {
                        let button = ui
                            .button(format!("File{name_arrow}"))
                            .on_hover_text("Sort by file name (click again to reverse)");
                        ui.ctx().accesskit_node_builder(button.id, |node| {
                            node.set_label(match sort {
                                QueueSort::NameAsc => "File, sorted ascending by name. Activate to sort descending.",
                                QueueSort::NameDesc => "File, sorted descending by name. Activate to return to arrival order.",
                                _ => "File, unsorted. Activate to sort by name, ascending.",
                            });
                        });
                        if button.clicked() {
                            app.queue_sort = cycle_sort(sort, QueueSort::NameAsc);
                        }
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
                                // Stem and extension survive a narrow cell;
                                // the full path is one hover behind, and the
                                // details tab holds it without hover at all.
                                // `selectable_label` keeps the selected-row
                                // highlight the plain label would lose.
                                let name = job.source_name();
                                let short = middle_ellipsis(&name, 48);
                                let response =
                                    ui.selectable_label(selected == Some(job.id), short)
                                        .on_hover_text(job.source.display().to_string());
                                if response.clicked() {
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

                            row.col(|ui| progress_cell(ui, job));

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
                                    // 20 pt in height is the comfortable
                                    // minimum for a click target; the old
                                    // 18 pt button missed it.
                                    let btn = ui
                                        .add_sized([24.0, 20.0], egui::Button::new("✕"))
                                        .on_hover_text("Remove from queue");
                                    // The glyph is not a name: AccessKit gets
                                    // the action and the document, so a
                                    // screen reader never says
                                    // "multiplication sign" and nothing else.
                                    ui.ctx().accesskit_node_builder(btn.id, |node| {
                                        node.set_label(format!(
                                            "Remove {} from the queue",
                                            job.source_name()
                                        ));
                                    });
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

/// One click of a sortable header: unsorted → ascending → descending →
/// unsorted, per column.
fn cycle_sort(current: QueueSort, asc: QueueSort) -> QueueSort {
    let (desc, next) = match asc {
        QueueSort::StateAsc => (QueueSort::StateDesc, QueueSort::Arrival),
        QueueSort::NameAsc => (QueueSort::NameDesc, QueueSort::Arrival),
        _ => return QueueSort::Arrival,
    };
    match current {
        s if s == asc => desc,
        s if s == desc => next,
        _ => asc,
    }
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
                ui.weak("Drop more documents here to add them.");
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
            let mut scroll = egui::ScrollArea::vertical().auto_shrink(false);
            // The log appends at the bottom; follow the tail the way a
            // console does. egui re-sticks when the reader scrolls back down,
            // so reading older lines is never fought by the follow.
            if app.bottom_tab == BottomTab::Log {
                scroll = scroll.stick_to_bottom(true);
            }
            scroll.show(ui, |ui| match app.bottom_tab {
                BottomTab::Details => draw_details(app, ui),
                BottomTab::Log => draw_log(app, ui),
                BottomTab::Warnings => draw_warnings(app, ui),
            });
        });
}

fn draw_details(app: &App, ui: &mut egui::Ui) {
    let Some(job) = app.selected.and_then(|id| app.queue.job(id)) else {
        ui.weak("Click a document in the queue to see its details here.");
        ui.weak("Warnings and errors accumulate per document; they appear here and in the Log tab.");
        return;
    };
    let job = job.clone();
    egui::Grid::new("details_grid")
        .num_columns(2)
        .spacing([8.0, 4.0])
        .show(ui, |ui| {
            ui.strong("File");
            // Selectable: paths get copied into other tools far more often
            // than they get read twice.
            ui.add(egui::Label::new(job.source.display().to_string()).selectable(true));
            ui.end_row();

            ui.strong("Target");
            ui.label(job.target.label());
            ui.end_row();

            ui.strong("Output");
            match &job.output {
                Some(out) => {
                    // Wrapped: a long path plus the button would otherwise
                    // push the button off the panel's right edge.
                    ui.horizontal_wrapped(|ui| {
                        ui.add(egui::Label::new(out.display().to_string()).selectable(true));
                        // Only once the file is on disk: a resolved name is
                        // not a written file, and a button that opens an
                        // empty folder is worse than no button.
                        if out.exists() && ui.button("Open folder").clicked() {
                            crate::app::App::reveal_in_explorer(out);
                        }
                    });
                }
                None => {
                    ui.weak("— (resolved when the batch starts)");
                }
            }
            ui.end_row();

            ui.strong("State");
            // Same colour as the queue's badge, so the two views of one fact
            // cannot disagree.
            ui.colored_label(status_color(&job.status, ui), job.status.label());
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
                ui.strong(if job.status == JobStatus::Failed {
                    "Error"
                } else {
                    "Detail"
                });
                ui.add(egui::Label::new(detail).selectable(true));
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
        ui.weak("No warnings yet. Conversion warnings — lost hyphens, reordered text — appear here.");
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
            // Additive bookkeeping only: engine state lives in the top bar
            // now (a bottom edge is the first thing a moved window hides),
            // and the queue header stopped printing its own copy of these
            // same numbers, so the counts read in exactly one place.
            ui.horizontal(|ui| {
                let counts = app.counts();
                ui.weak(format!(
                    "{} · {} running · {} done · {} failed · {} warnings",
                    plural(counts.total, "document"),
                    counts.running,
                    counts.done,
                    counts.failed,
                    app.warnings.len()
                ));
            });
        });
}

// ---------------------------------------------------------------------------
// The destructive confirmation
// ---------------------------------------------------------------------------

/// The one destructive step in the app: a start under the "Overwrite files"
/// policy that would replace files already on disk. egui's `Modal` gives the
/// backdrop that blocks the batch buttons underneath and consumes Escape
/// (`should_close`), so the safe answer is reachable three ways — Cancel,
/// Escape, backdrop — and the destructive one exactly once, disabled
/// nowhere but never the default.
pub fn draw_overwrite_confirm(app: &mut App, ui: &mut egui::Ui) {
    if app.pending_confirm.is_none() {
        return;
    }

    enum Choice {
        None,
        Proceed,
        Cancel,
    }
    let mut choice = Choice::None;

    let response = egui::Modal::new(egui::Id::unique("overwrite_confirm")).show(ui.ctx(), |ui| {
        if let Some(pending) = app.pending_confirm.as_ref() {
            ui.add_space(2.0);
            ui.strong(format!(
                "Overwrite {}?",
                plural(pending.existing.len(), "existing file")
            ));
            ui.add_space(6.0);
            // A few concrete names beat a number: the count says *how
            // many*, the names say *what anyone loses* if they accept.
            for output in pending.existing.iter().take(5) {
                let name = output
                    .file_name()
                    .map_or_else(
                        || output.display().to_string(),
                        |n| n.to_string_lossy().to_string(),
                    );
                ui.label(middle_ellipsis(&name, 48));
            }
            if pending.existing.len() > 5 {
                ui.weak(format!("… and {} more", pending.existing.len() - 5));
            }
            ui.add_space(6.0);
            ui.weak(
                "These files will be replaced by the converted documents. \\
                 Cancel keeps the queue exactly as it is.",
            );
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                // The safe button first, reading order: the wandering eye
                // hits "Cancel" before it reaches the crimson one — and the
                // Enter key (which the shortcuts handler suppresses while
                // this dialog is up) can never fire the destructive label
                // as a default. A warning button is prominent, not default.
                let cancel = ui.button("Cancel");
                let overwrite = ui.add(
                    egui::Button::new(
                        egui::RichText::new(format!(
                            "Overwrite {}",
                            pending.existing.len()
                        ))
                        .color(crate::theme::TEXT)
                        .strong(),
                    )
                    .fill(crate::theme::CRIMSON),
                );
                // A modal owns the keyboard: if focus is anywhere else —
                // the Start button that opened the dialog, or a widget Tab
                // has wandered to behind the backdrop — the safe answer
                // takes it. Tab then moves Cancel → Overwrite and the check
                // brings it back, which is the focus trap a modal is
                // supposed to have. Enter on Cancel dismisses; Enter can
                // only overwrite after a deliberate Tab.
                let focused_here = ui.memory(|memory| {
                    memory.has_focus(cancel.id) || memory.has_focus(overwrite.id)
                });
                if !focused_here {
                    cancel.request_focus();
                }
                if cancel.clicked() {
                    choice = Choice::Cancel;
                }
                if overwrite.clicked() {
                    choice = Choice::Proceed;
                }
            });
        }
    });

    // Backdrop click and Escape both mean "no": a destructive action is the
    // one place dismissing without choosing must not default to yes.
    if response.should_close() {
        choice = Choice::Cancel;
    }

    match choice {
        Choice::Proceed => app.confirm_overwrite(),
        Choice::Cancel => app.cancel_overwrite_confirm(),
        Choice::None => {}
    }
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
