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
use crate::state::{Counts, JobId, JobStatus, Queue, Settings, resolve_output_path};
use eframe::egui;
use std::collections::HashSet;
use std::time::Duration;

const SETTINGS_KEY: &str = "settings_v1";

pub struct App {
    pub settings: Settings,
    pub queue: Queue,

    engine: EngineChannels,
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

    /// Set when the engine channel dies (it should not, but a dead channel
    /// must fail loudly instead of leaving "running" jobs hanging).
    engine_gone: bool,
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

        if settings.dark_theme {
            cc.egui_ctx.set_theme(egui::Theme::Dark);
        } else {
            cc.egui_ctx.set_theme(egui::Theme::Light);
        }

        log.lock().push_timed(format!(
            "engine started: worker-process pool, default {} worker(s)",
            settings.workers_or_default()
        ));

        Self {
            settings,
            queue: Queue::new(),
            engine,
            in_flight: 0,
            log,
            warnings: Vec::new(),
            selected: None,
            bottom_tab: BottomTab::Log,
            settings_visible: true,
            engine_gone: false,
            reported_disconnect: false,
            shown_title: String::new(),
        }
    }

    pub fn counts(&self) -> Counts {
        self.queue.counts()
    }

    /// Live while the engine holds any of our jobs.
    pub fn busy(&self) -> bool {
        self.in_flight > 0 || self.counts().busy()
    }

    /// The engine channel was created once at startup; if it dies, the queue
    /// is failed loudly rather than left hanging.
    pub fn engine_alive(&self) -> bool {
        !self.engine_gone
    }

    // -------------------------------------------------------------------------
    // Engine commands
    // -------------------------------------------------------------------------

    /// Start every currently queued job.
    ///
    /// Output names are resolved here, once, before anything runs, so the
    /// queue shows the final names from the first frame of the batch.
    pub fn start_batch(&mut self) {
        if self.engine_gone {
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
        let n = specs.len();
        match self.engine.commands.send(EngineCommand::Start {
            jobs: specs,
            workers,
        }) {
            Ok(()) => {
                self.in_flight += n;
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

    pub fn cancel_all(&mut self) {
        if self.engine_gone {
            return;
        }
        match self.engine.commands.send(EngineCommand::CancelAll) {
            Ok(()) => self.log.lock().push_timed("cancel requested".to_string()),
            Err(err) => self.on_engine_gone(Some(err.to_string())),
        }
    }

    // -------------------------------------------------------------------------
    // Engine events
    // -------------------------------------------------------------------------

    /// Drain every pending engine event for this frame.
    pub fn drain_events(&mut self) {
        if self.engine_gone {
            return;
        }
        loop {
            match self.engine.events.try_recv() {
                Ok(ev) => self.apply_event(ev),
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.on_engine_gone(None);
                    break;
                }
            }
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
        self.engine_gone = true;
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
        // The dialog filters first, so the user sees the file picker do the
        // honest thing; anything that still gets through an "All files" pass
        // is filtered by `add_files` with a log line.
        let picked = rfd::FileDialog::new()
            .set_title("Add documents to convert")
            .add_filter("Documents (PDF)", &["pdf"])
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
        if files.is_empty() {
            self.log.lock().push_timed(format!(
                "nothing added: skipped {total} file(s) the engine cannot read (PDF only for now)"
            ));
            return;
        }

        let target = self.settings.target;
        let ids = self.queue.add(files.iter().cloned(), target);
        if ids.len() < total {
            self.log.lock().push_timed(format!(
                "added {} file(s), skipped {} the engine cannot read (PDF only for now)",
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

    // -------------------------------------------------------------------------
    // Settings & persistence
    // -------------------------------------------------------------------------

    /// Apply the theme when the toggle moves, rather than every frame.
    pub fn apply_theme(&self, ctx: &egui::Context) {
        ctx.set_theme(if self.settings.dark_theme {
            egui::Theme::Dark
        } else {
            egui::Theme::Light
        });
    }

    fn update_window_title(&mut self, ctx: &egui::Context) {
        let title = match self.queue.batch_progress() {
            Some(p) if self.busy() => format!("dconvert — {}%", (p * 100.0) as u32),
            _ => "dconvert — document converter".to_string(),
        };
        if title != self.shown_title {
            self.shown_title = title.clone();
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title));
        }
    }

    /// The engine ticks ~20×/s while a batch runs; ask for just enough
    /// repaints to stay smooth, and otherwise let the UI sleep.
    pub fn request_repaint_as_needed(&self, ctx: &egui::Context) {
        if self.busy() {
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

        let ctx = ui.ctx().clone();
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
        if self.engine_alive() {
            let _ = self.engine.commands.send(EngineCommand::Shutdown);
        }
        tracing::info!("shutdown: engine stopped");
    }
}
