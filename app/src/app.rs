//! The eframe application: owns the queue, the engine channels and the log,
//! drains engine events once per frame, and paints everything via the
//! functions in `ui.rs`.
//!
//! Threading story (mirrors the plan):
//! - the UI thread never converts and never blocks;
//! - engine events arrive on a channel and are drained non-blocking;
//! - while jobs are live the UI asks for a slow repaint (~20 fps), because
//!   progress events tick faster than batches finish and the machine is a
//!   mobile APU where idle CPU matters.

use crate::engine::{EngineChannels, EngineCommand, EngineEvent, JobSpec};
use crate::log_ring::LogHandle;
use crate::state::{
    Counts, JobId, JobStatus, OverwritePolicy, Queue, Settings, resolve_output_path,
};
use eframe::egui;
use std::collections::HashSet;
use std::time::{Duration, Instant};

const SETTINGS_KEY: &str = "settings_v1";

/// A batch waiting on the overwrite confirmation. The specs are already
/// resolved — the queue shows their names — so only the answer is missing;
/// `draw_overwrite_confirm` in ui.rs presents it and calls back one of
/// `confirm_overwrite` / `cancel_overwrite_confirm`.
#[derive(Debug)]
pub(crate) struct PendingBatch {
    pub(crate) specs: Vec<JobSpec>,
    pub(crate) workers: usize,
    pub(crate) existing: Vec<std::path::PathBuf>,
}

/// Display order of the queue table. The queue itself keeps arrival order —
/// that is a documented promise (`Queue::add`) — so sorting is always a
/// *view* over the jobs and never becomes a reorder of state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QueueSort {
    #[default]
    Arrival,
    /// Most-attention first: failed, then running, then queued.
    StateAsc,
    StateDesc,
    NameAsc,
    NameDesc,
}

pub struct App {
    pub settings: Settings,
    pub queue: Queue,

    /// None after a disconnect; `restart_engine` brings it back.
    engine: Option<EngineChannels>,
    /// Jobs handed to the engine whose terminal event has not arrived yet.
    /// Drives the busy indicator even for jobs still queued on the engine.
    pub(crate) in_flight: usize,

    pub log: LogHandle,
    /// Warning lines, aggregated for the warnings tab (keep all: warnings are
    /// diagnostics a converter should never lose).
    pub warnings: Vec<String>,

    pub selected: Option<JobId>,
    pub bottom_tab: BottomTab,
    pub settings_visible: bool,

    /// A start waiting on the overwrite confirmation, if any.
    pub(crate) pending_confirm: Option<PendingBatch>,
    /// Display order of the queue table (the queue keeps arrival order).
    pub(crate) queue_sort: QueueSort,
    /// Batch ETA, derived UI-side from the per-page progress the engine
    /// already streams (AGENTS.md known gap 4); None until a rate is
    /// measurable over the window.
    pub(crate) eta: Option<Duration>,
    /// Recent batch-progress samples for that estimate: (seen at, fraction).
    eta_samples: Vec<(Instant, f32)>,
    reported_disconnect: bool,
    /// Last title percentage shown, to avoid spamming viewport commands.
    shown_title: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BottomTab {
    Details,
    Log,
    Warnings,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, log: LogHandle, engine: EngineChannels) -> Self {
        let mut settings: Settings = cc
            .storage
            .and_then(|s| eframe::get_value::<Settings>(s, SETTINGS_KEY))
            .unwrap_or_default();

        // A persisted target the engine does not offer any more (or one
        // persisted by a build that offered more) is repaired to the first
        // supported one rather than silently failing at convert time.
        if !crate::engine::supported_targets().contains(&settings.target) {
            if let Some(first) = crate::engine::supported_targets().first() {
                settings.target = *first;
            }
        }

        // dconvert is permanently dark: the old "Dark mode" toggle is gone,
        // and its setting field no longer exists — a stored settings blob
        // that still carries it deserialises with it ignored. The palette
        // paints the dark theme once (its tests hold the only contrast
        // floor the appearance needs).
        crate::theme::install(&cc.egui_ctx);

        log.lock().push_timed(format!(
            "engine started: worker-process pool, default {} worker(s)",
            settings.workers_or_default()
        ));

        Self {
            settings,
            queue: Queue::new(),
            engine: Some(engine),
            in_flight: 0,
            log,
            warnings: Vec::new(),
            selected: None,
            // Details, not Log: the first screen should be the app's own
            // vocabulary, not a line about worker-process pools. The log is
            // one click away and the tab strip says so.
            bottom_tab: BottomTab::Details,
            // Settings stay closed on launch: the first screen is the drop
            // zone, and the one setting worth a glance before Start — the
            // target format — reads in the top bar instead.
            settings_visible: false,
            pending_confirm: None,
            queue_sort: QueueSort::default(),
            eta: None,
            eta_samples: Vec::new(),
            reported_disconnect: false,
            shown_title: String::new(),
        }
    }

    pub fn counts(&self) -> Counts {
        self.queue.counts()
    }

    /// True while the engine is actually working: jobs sent and not yet
    /// terminal, or a job the engine has marked Running. A filled queue is
    /// *not* converting — the window title, the ETA and the repaint cadence
    /// all read this, and "Converting… 0%" over a batch nobody started is a
    /// lie the old `busy` (which counted queued jobs) told.
    pub fn converting(&self) -> bool {
        self.in_flight > 0 || self.counts().running > 0
    }

    /// The engine lives while its channels do. `on_engine_gone` drops them;
    /// `restart_engine` builds a fresh engine over the same binary.
    pub fn engine_alive(&self) -> bool {
        self.engine.is_some()
    }

    // -------------------------------------------------------------------------
    // Engine commands
    // -------------------------------------------------------------------------

    /// Start every currently queued job.
    ///
    /// Output names are resolved here, once, before anything runs, so the
    /// queue shows the final names from the first frame of the batch.
    pub fn start_batch(&mut self) {
        if !self.engine_alive() {
            return;
        }
        let settings = self.settings.clone();
        let mut taken: HashSet<std::path::PathBuf> = HashSet::new();
        struct Unresolved {
            id: JobId,
            message: String,
        }
        let mut unresolved: Vec<Unresolved> = Vec::new();
        let mut specs: Vec<JobSpec> = Vec::new();

        for job in self.queue.pending_mut() {
            if job.status != JobStatus::Queued {
                continue;
            }
            match resolve_output_path(&settings, &job.source, &mut taken) {
                Some(output) => {
                    job.output = Some(output.clone());
                    specs.push(JobSpec {
                        id: job.id,
                        source: job.source.clone(),
                        target: job.target,
                        output,
                    });
                }
                None => unresolved.push(Unresolved {
                    id: job.id,
                    message: "no output folder set".to_string(),
                }),
            }
        }

        for u in unresolved {
            if let Some(job) = self.queue.job_mut(u.id) {
                job.status = JobStatus::Failed;
                job.detail = Some(
                    "The output folder could not be determined. Set one in settings.".to_string(),
                );
            }
            self.log
                .lock()
                .push_timed(format!("FAILED job {}: {}", u.id.0, u.message));
        }

        if specs.is_empty() {
            return;
        }
        let workers = settings.workers_or_default();

        // "Overwrite files" is the only setting that can take an existing
        // file from the user, so a batch that would replace anything waits
        // on one confirmation (draw_overwrite_confirm) instead of going
        // quietly. AutoRename — the default — can never clobber, and needs
        // no ceremony. The list comes from `existing_outputs` resolving the
        // batch's own sources fresh — the same resolution the loop below
        // ran, in the same order, so the shown names are the names the
        // engine will be handed.
        if settings.overwrite == OverwritePolicy::Overwrite {
            let existing = crate::state::existing_outputs(
                &settings,
                specs.iter().map(|spec| spec.source.clone()),
            );
            if !existing.is_empty() {
                self.log.lock().push_timed(format!(
                    "waiting for confirmation: {} output file(s) exist and would be replaced",
                    existing.len()
                ));
                self.pending_confirm = Some(PendingBatch {
                    specs,
                    workers,
                    existing,
                });
                return;
            }
        }
        self.send_batch(specs, workers);
    }

    /// Hand a resolved batch to the engine; the only place that sends a
    /// Start.
    fn send_batch(&mut self, specs: Vec<JobSpec>, workers: usize) {
        let Some(engine) = &self.engine else {
            return;
        };
        let n = specs.len();
        match engine.commands.send(EngineCommand::Start {
            jobs: specs,
            workers,
        }) {
            Ok(()) => {
                self.in_flight += n;
                // Progress samples describe one batch: a mid-run Start
                // restarts the rate window rather than smoothing across
                // two batches' speeds.
                self.eta_samples.clear();
                self.eta = None;
                self.log
                    .lock()
                    .push_timed(format!("started batch of {n} job(s), {workers} worker(s)"));
            }
            Err(err) => {
                // The channel died under us: fail the batch rather than leave
                // the queue pretending to be alive.
                self.on_engine_gone(Some(err.to_string()));
            }
        }
    }

    /// The overwrite confirmation's affirmative answer.
    pub fn confirm_overwrite(&mut self) {
        let Some(pending) = self.pending_confirm.take() else {
            return;
        };
        self.send_batch(pending.specs, pending.workers);
    }

    /// The overwrite confirmation's refusals: the backdrop click, Escape and
    /// the Cancel button all land here, and the queue stays exactly as it
    /// was — jobs queued, names already shown.
    pub fn cancel_overwrite_confirm(&mut self) {
        if self.pending_confirm.take().is_some() {
            self.log
                .lock()
                .push_timed("start cancelled: nothing was overwritten");
        }
    }

    pub fn cancel_all(&mut self) {
        let Some(engine) = &self.engine else {
            return;
        };
        match engine.commands.send(EngineCommand::CancelAll) {
            Ok(()) => self.log.lock().push_timed("cancel requested".to_string()),
            Err(err) => self.on_engine_gone(Some(err.to_string())),
        }
    }

    /// Bring the engine back after a disconnect — the same one call `main`
    /// makes at startup, so a restart is never a guess about which build the
    /// workers are. The queue itself is untouched: queued jobs stay queued;
    /// jobs failed *by the disconnect* come back through the retry action.
    pub fn restart_engine(&mut self) {
        if self.engine.is_some() {
            return;
        }
        self.engine = Some(crate::engine::spawn_real_engine());
        self.log.lock().push_timed("engine restarted".to_string());
    }

    /// Re-queue the jobs whose failure text says the engine went away — the
    /// only failures a restart repairs. Real conversion failures (an
    /// unreadable file, an unsupported pair) would fail again identically,
    /// so they stay failed where the details tab explains them.
    pub fn retry_engine_failed(&mut self) {
        let ids: Vec<JobId> = self
            .queue
            .jobs()
            .iter()
            .filter(|j| {
                j.status == JobStatus::Failed
                    && j.detail
                        .as_deref()
                        .map_or(false, |d| d.starts_with("engine unavailable"))
            })
            .map(|j| j.id)
            .collect();
        for id in &ids {
            if let Some(job) = self.queue.job_mut(*id) {
                job.status = JobStatus::Queued;
                job.detail = None;
                job.progress = None;
                job.started_at = None;
                job.elapsed = None;
                // Resolved afresh at the next start: under AutoRename the
                // name chain is re-read, under Overwrite the confirmation
                // gate is re-checked against what now exists.
                job.output = None;
            }
        }
        if !ids.is_empty() {
            self.log
                .lock()
                .push_timed(format!("re-queued {} job(s) for retry", ids.len()));
        }
    }

    // -------------------------------------------------------------------------
    // Engine events
    // -------------------------------------------------------------------------

    /// Drain every pending engine event for this frame. Events are collected
    /// under a short borrow, then applied with `&mut self` — the queue
    /// mutation and the channel access must not interleave.
    pub fn drain_events(&mut self) {
        let mut events = Vec::new();
        {
            let Some(engine) = &self.engine else { return };
            loop {
                match engine.events.try_recv() {
                    Ok(ev) => events.push(ev),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        self.on_engine_gone(None);
                        return;
                    }
                }
            }
        }
        for ev in events {
            self.apply_event(ev);
        }
    }

    fn apply_event(&mut self, ev: EngineEvent) {
        match ev {
            EngineEvent::Started { id } => {
                if let Some(job) = self.queue.job_mut(id) {
                    if job.status == JobStatus::Queued {
                        job.status = JobStatus::Running;
                        job.started_at = Some(std::time::Instant::now());
                    }
                }
            }
            EngineEvent::Progress { id, frac } => {
                if let Some(job) = self.queue.job_mut(id) {
                    if job.status == JobStatus::Running {
                        job.progress = Some(frac.clamp(0.0, 1.0));
                    }
                }
            }
            EngineEvent::Warning { id, text } => {
                let name = self
                    .queue
                    .job(id)
                    .map(|j| j.source_name())
                    .unwrap_or_else(|| format!("job {}", id.0));
                if let Some(job) = self.queue.job_mut(id) {
                    job.warnings.push(text.clone());
                }
                self.warnings.push(format!("{name}: {text}"));
                self.log.lock().push_timed(format!("WARN {name}: {text}"));
            }
            EngineEvent::Finished {
                id,
                output,
                elapsed,
            } => {
                let name = self.queue.job(id).map(|j| j.source_name());
                if let Some(job) = self.queue.job_mut(id) {
                    if !job.status.is_terminal() {
                        job.status = JobStatus::Done;
                        job.output = Some(output.clone());
                        job.progress = Some(1.0);
                        job.elapsed = Some(elapsed);
                    }
                }
                if let Some(name) = name {
                    self.log.lock().push_timed(format!(
                        "done {name} → {} ({:.1}s)",
                        output.display(),
                        elapsed.as_secs_f32()
                    ));
                }
                self.note_terminal(id);
            }
            EngineEvent::Failed {
                id,
                summary,
                detail,
                elapsed,
            } => {
                let name = self.queue.job(id).map(|j| j.source_name());
                if let Some(job) = self.queue.job_mut(id) {
                    if !job.status.is_terminal() {
                        job.status = JobStatus::Failed;
                        job.detail = detail;
                        job.progress = None;
                        job.elapsed = Some(elapsed);
                    }
                }
                if let Some(name) = name {
                    self.log
                        .lock()
                        .push_timed(format!("FAILED {name}: {summary}"));
                }
                self.note_terminal(id);
            }
            EngineEvent::Cancelled { id } => {
                if let Some(job) = self.queue.job_mut(id) {
                    if !job.status.is_terminal() {
                        job.status = JobStatus::Cancelled;
                        job.progress = None;
                        job.elapsed = job.live_elapsed();
                    }
                    self.log
                        .lock()
                        .push_timed(format!("cancelled {}", job.source_name()));
                }
                self.note_terminal(id);
            }
        }
    }

    /// Every terminal event, exactly once per job (the protocol promises
    /// this; the guards above keep a stray duplicate from double-counting).
    fn note_terminal(&mut self, _id: JobId) {
        if self.in_flight > 0 {
            self.in_flight -= 1;
        }
    }

    fn on_engine_gone(&mut self, why: Option<String>) {
        // The channels are dead: drop them, so `engine_alive` answers the
        // truth from one place and `restart_engine` can build fresh ones.
        self.engine = None;
        if self.reported_disconnect {
            return;
        }
        self.reported_disconnect = true;
        let detail = why
            .map(|w| format!(" ({w})"))
            .unwrap_or_else(|| "(channel closed)".to_string());
        // Fail everything still outstanding — no phantom "running" jobs left
        // when the engine dies.
        for job in self.queue.pending_mut() {
            if !job.status.is_terminal() {
                job.status = JobStatus::Failed;
                job.detail = Some(format!("engine unavailable {detail}"));
            }
        }
        self.in_flight = 0;
        self.log
            .lock()
            .push_timed(format!("FAILED engine gone {detail}"));
    }

    // -------------------------------------------------------------------------
    // Files
    // -------------------------------------------------------------------------

    pub fn add_files_from_dialog(&mut self) {
        // The filter comes from the facade's reader list, not a literal, so
        // the picker cannot hide a format the queue would accept. Anything
        // that still gets through an "All files" pass is filtered by
        // `add_files` with a log line.
        let extensions = crate::engine::readable_extensions();
        let filter_label = format!(
            "Documents ({})",
            extensions
                .iter()
                .map(|extension| format!(".{extension}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let picked = rfd::FileDialog::new()
            .set_title("Add documents to convert")
            .add_filter(filter_label, &extensions)
            .add_filter("All files", &["*"])
            .pick_files();
        if let Some(files) = picked {
            self.add_files(files);
        }
    }

    pub fn add_folder_from_dialog(&mut self) {
        let picked = rfd::FileDialog::new()
            .set_title("Add a folder of documents")
            .pick_folder();
        if let Some(folder) = picked {
            self.add_files(vec![folder]);
        }
    }

    pub fn add_files(&mut self, paths: Vec<std::path::PathBuf>) {
        let mut files = Vec::new();
        crate::state::collect_files(paths, &mut files);
        if files.is_empty() {
            return;
        }
        // The queue only accepts what the engine can read: offering more
        // would mean a failed job per file, which is noise instead of
        // guidance. The capability list comes from the adapter, so when the
        // facade learns a new format the queue accepts it with no UI change.
        let total = files.len();
        files.retain(|path| crate::engine::readable(path));
        // Derived, not typed: the "what can it read" half of the message is
        // the facade's answer, so it cannot go stale when a reader lands.
        let readable_list = crate::engine::readable_extensions()
            .iter()
            .map(|extension| extension.to_ascii_uppercase())
            .collect::<Vec<_>>()
            .join(", ");
        if files.is_empty() {
            self.log.lock().push_timed(format!(
                "nothing added: skipped {total} file(s) the engine cannot read (it reads {readable_list})"
            ));
            return;
        }

        let target = self.settings.target;
        let ids = self.queue.add(files.iter().cloned(), target);
        if ids.len() < total {
            self.log.lock().push_timed(format!(
                "added {} file(s), skipped {} the engine cannot read (it reads {readable_list})",
                ids.len(),
                total - ids.len()
            ));
        } else {
            self.log
                .lock()
                .push_timed(format!("added {} file(s)", ids.len()));
        }
    }

    pub fn pick_output_folder(&mut self) {
        if let Some(dir) = rfd::FileDialog::new()
            .set_title("Choose output folder")
            .pick_folder()
        {
            self.settings.output_mode = crate::state::OutputMode::Custom;
            self.settings.custom_dir = Some(dir);
        }
    }

    /// Show a produced file in Explorer, selected. Windows-only by nature:
    /// the deliverable is a Windows folder (`scripts/package.ps1`), and a
    /// cross-platform opener crate would be a dependency for one call.
    ///
    /// Failure is silent on purpose — the button only appears for an output
    /// that exists, so the remaining failure is "there is no Explorer here",
    /// which no log line would help with.
    pub(crate) fn reveal_in_explorer(path: &std::path::Path) {
        // `/select,` and the path as separate argv entries: Explorer accepts
        // the space after the comma, and letting `Command` quote the path is
        // what keeps names with spaces in one piece.
        let _ = std::process::Command::new("explorer")
            .arg("/select,")
            .arg(path)
            .spawn();
    }

    // -------------------------------------------------------------------------
    // Settings & persistence
    // -------------------------------------------------------------------------

    fn update_window_title(&mut self, ctx: &egui::Context) {
        let counts = self.counts();
        // Content, not branding: a window switcher asks what this window is
        // doing (toolbars.md › Titles). The app-name-only title returns when
        // there is nothing more specific to say.
        let title = if self.converting() {
            match self.queue.batch_progress() {
                Some(p) => format!("Converting… {}%", (p * 100.0) as u32),
                None => "Converting…".to_string(),
            }
        } else if counts.queued > 0 {
            if counts.queued == 1 {
                "1 document queued".to_string()
            } else {
                format!("{} documents queued", counts.queued)
            }
        } else {
            "dconvert".to_string()
        };
        if title != self.shown_title {
            self.shown_title = title.clone();
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title));
        }
    }

    /// Update the batch ETA from (time, fraction). The rate is a straight
    /// window average rather than an EWMA — the engine's page events arrive
    /// unevenly across jobs, and over ~12 s the window does all the
    /// smoothing the top bar can honestly claim. Console text shows `None`
    /// until the rate is measurable and near the end (the last pages are the
    /// largest), keeping what is genuinely unknowable from being guessed.
    fn update_eta(&mut self) {
        let now = Instant::now();
        if !self.converting() {
            self.eta = None;
            self.eta_samples.clear();
            return;
        }
        let Some(frac) = self.queue.batch_progress() else {
            return;
        };
        self.eta_samples.push((now, frac));

        const WINDOW: Duration = Duration::from_secs(12);
        while self
            .eta_samples
            .first()
            .map_or(false, |(seen_at, _)| now.duration_since(*seen_at) > WINDOW)
        {
            self.eta_samples.remove(0);
        }

        self.eta = match self.eta_samples.as_slice() {
            [(start, first), .., (end, last)] => {
                let dt = end.duration_since(*start).as_secs_f32();
                let df = last - first;
                if dt >= 2.0 && df > 0.0 {
                    let remaining = (1.0 - last) / (df / dt);
                    // An estimate without any honest scale is noise: hours
                    // of remaining time from a 12 s window is a story, not
                    // a measurement.
                    if remaining.is_finite() && (0.0..=7200.0).contains(&remaining) {
                        Some(Duration::from_secs_f32(remaining))
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
            _ => None,
        };
    }

    /// The shortcuts worth having, and only those: Enter starts a queued
    /// batch when no widget holds the keyboard (a focused control keeps
    /// Enter for itself — and egui fires a *fake primary click on Enter for
    /// a focused button*, `egui/src/context.rs` line 1761, so crossing that
    /// would start the batch twice), Escape closes the settings panel — a
    /// close key, never an opener — but not while a popup (a combo box) is
    /// eating the Escape to close itself, and Ctrl+O opens the file dialog.
    /// The overwrite dialog's Escape is consumed by `egui::Modal::should_close`,
    /// so these can never race it.
    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        if self.pending_confirm.is_some() {
            return;
        }
        let focused = ctx.memory(|m| m.focused().is_some());
        let (enter, escape, ctrl_o) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::Enter),
                i.key_pressed(egui::Key::Escape),
                i.modifiers.ctrl && i.key_pressed(egui::Key::O),
            )
        });
        if ctrl_o {
            self.add_files_from_dialog();
        }
        let popup_open = ctx.any_popup_open();
        if escape && !popup_open {
            self.settings_visible = false;
        }
        if enter && !focused && self.counts().queued > 0 {
            self.start_batch();
        }
    }

    /// The engine ticks ~20×/s while a batch runs; ask for just enough
    /// repaints to stay smooth, and otherwise let the UI sleep.
    pub fn request_repaint_as_needed(&self, ctx: &egui::Context) {
        if self.converting() {
            ctx.request_repaint_after(Duration::from_millis(40));
        }
    }
}

impl eframe::App for App {
    /// eframe 0.37 hands the app a root `Ui`; every panel attaches itself to
    /// it from here.
    fn ui(&mut self, ui: &mut eframe::egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_events();
        self.handle_dropped_files(ui.ctx());

        crate::ui::draw_top_bar(self, ui);
        crate::ui::draw_status_bar(self, ui);
        crate::ui::draw_bottom_tabs(self, ui);
        crate::ui::draw_queue_panel(self, ui);
        crate::ui::draw_settings_panel(self, ui);
        crate::ui::draw_central(self, ui);
        crate::ui::draw_overwrite_confirm(self, ui);

        let ctx = ui.ctx().clone();
        self.handle_shortcuts(&ctx);
        self.update_eta();
        self.update_window_title(&ctx);
        self.request_repaint_as_needed(&ctx);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, SETTINGS_KEY, &self.settings);
    }

    /// Tell the engine to stop worker processes before the process goes
    /// away. Protocol requirement 2 keeps per-job accounting honest even on
    /// the way out.
    fn on_exit(&mut self) {
        if let Some(engine) = &self.engine {
            let _ = engine.commands.send(EngineCommand::Shutdown);
        }
        tracing::info!("shutdown: engine stopped");
    }
}
