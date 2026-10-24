//! UI-side state: jobs, the queue, and user settings.
//!
//! Deliberately free of egui. The queue is plain data so the event-draining
//! code in `app.rs` stays mechanical and so the behaviour worth testing (output
//! naming, the removal guard, finished-clearing) can be tested without a
//! window server.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Formats
// ---------------------------------------------------------------------------

/// The output format a job converts to.
///
/// Which variants are *offered* comes from the engine adapter's capability
/// list (`engine::supported`, a stand-in for the facade's capability query
/// until the engine owner lands it) — never from this enum, which only names
/// shapes the UI can present. Enum variants are kept distinguishable rather
/// than reduced to an extension string so the UI can present friendly labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TargetFormat {
    /// Office Open XML word processing document.
    Docx,
    /// UTF-8 plain text.
    PlainText,
}

impl TargetFormat {
    /// Extension written on output files, without the dot.
    pub fn extension(self) -> &'static str {
        match self {
            TargetFormat::Docx => "docx",
            TargetFormat::PlainText => "txt",
        }
    }

    /// Label shown in the format selector.
    pub fn label(self) -> &'static str {
        match self {
            TargetFormat::Docx => "Word (.docx)",
            TargetFormat::PlainText => "Plain text (.txt)",
        }
    }
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// Where converted files are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum OutputMode {
    /// Next to the source file. The default, because it is the behaviour a
    /// user converting one file expects the result to have.
    #[default]
    SourceFolder,
    /// One fixed folder for the whole batch.
    Custom,
}

/// What to do when the output file already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum OverwritePolicy {
    /// Write `name-1.ext`, `name-2.ext`, … The default: a converter that eats
    /// the only copy of something is unforgivable, and a real overwrite needs
    /// a confirmation flow before it earns its place in the UI.
    #[default]
    AutoRename,
    /// Clobber the existing file.
    Overwrite,
}

/// Persisted user settings. Everything else in this module is per-run state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    pub output_mode: OutputMode,
    /// Only meaningful when `output_mode` is `Custom`. Kept as an optional
    /// path rather than a string so round-tripping through eframe storage is
    /// exact.
    pub custom_dir: Option<PathBuf>,
    pub target: TargetFormat,
    pub overwrite: OverwritePolicy,
    /// Worker processes for the conversion pool. See [`default_worker_count`].
    pub workers: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            output_mode: OutputMode::SourceFolder,
            custom_dir: None,
            target: TargetFormat::Docx,
            overwrite: OverwritePolicy::AutoRename,
            workers: default_worker_count() as u32,
        }
    }
}

impl Settings {
    /// Directory a converted file is written to, or `None` when the selection
    /// degenerates (a bare device path has no parent, the custom dir is unset).
    pub fn output_dir_for(&self, source: &Path) -> Option<PathBuf> {
        match self.output_mode {
            OutputMode::SourceFolder => source.parent().map(Path::to_path_buf),
            OutputMode::Custom => self.custom_dir.clone(),
        }
    }

    /// Worker count to actually hand the engine. `0` should not exist in
    /// persisted settings, but deserialised defaults get repaired here rather
    /// than spawning zero workers and silently converting nothing.
    pub fn workers_or_default(&self) -> usize {
        if self.workers == 0 {
            default_worker_count()
        } else {
            self.workers as usize
        }
    }
}

/// Default pool size, from the Phase 0 measurements (finding C1): the machine
/// reports 32 logical processors, but throughput saturates around 8 workers and
/// per-worker slowdown grows badly past that. `min(8, logic cores)` is the
/// documented compromise; the facade may override with a better-informed value
/// once it knows about physical cores.
pub fn default_worker_count() -> usize {
    let logical = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    logical.min(8).max(1)
}

// ---------------------------------------------------------------------------
// Jobs
// ---------------------------------------------------------------------------

/// Identity of a queued job. Unique within one app run, monotonic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JobId(pub u64);

#[derive(Debug, Clone, PartialEq)]
pub enum JobStatus {
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
}

impl JobStatus {
    /// Short label for the queue table.
    pub fn label(&self) -> &'static str {
        match self {
            JobStatus::Queued => "queued",
            JobStatus::Running => "running",
            JobStatus::Done => "done",
            JobStatus::Failed => "failed",
            JobStatus::Cancelled => "cancelled",
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            JobStatus::Done | JobStatus::Failed | JobStatus::Cancelled
        )
    }
}

#[derive(Debug, Clone)]
pub struct Job {
    pub id: JobId,
    pub source: PathBuf,
    pub target: TargetFormat,
    pub status: JobStatus,
    /// Where the converted file is expected to land. Resolved once, when the
    /// batch starts, so the name the user sees is the name the engine uses.
    pub output: Option<PathBuf>,
    /// Fraction of the current job, `0.0..=1.0`, while running.
    pub progress: Option<f32>,
    pub started_at: Option<Instant>,
    pub elapsed: Option<Duration>,
    pub warnings: Vec<String>,
    /// Long-form text for the details tab: error explanation of a failed job,
    /// or anything else that does not fit a table cell.
    pub detail: Option<String>,
}

impl Job {
    /// File name for display; lossy, because a converter must never refuse a
    /// file for the cosmetic reason of an unrepresentable name.
    pub fn source_name(&self) -> String {
        self.source
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "?".to_owned())
    }

    /// Elapsed time, live for a running job.
    pub fn live_elapsed(&self) -> Option<Duration> {
        self.elapsed
            .or_else(|| self.started_at.map(|t| t.elapsed()))
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Counts {
    pub total: usize,
    pub queued: usize,
    pub running: usize,
    pub done: usize,
    pub failed: usize,
    pub cancelled: usize,
}

#[derive(Debug, Default)]
pub struct Queue {
    jobs: Vec<Job>,
    next_id: u64,
}

impl Queue {
    pub fn new() -> Self {
        Self {
            jobs: Vec::new(),
            next_id: 1,
        }
    }

    pub fn jobs(&self) -> &[Job] {
        &self.jobs
    }

    pub fn job(&self, id: JobId) -> Option<&Job> {
        self.jobs.iter().find(|j| j.id == id)
    }

    pub fn job_mut(&mut self, id: JobId) -> Option<&mut Job> {
        self.jobs.iter_mut().find(|j| j.id == id)
    }

    /// Clone out a job before removal, so the UI can log its name afterwards.
    pub fn peek(&self, id: JobId) -> Option<Job> {
        self.jobs.iter().find(|j| j.id == id).cloned()
    }

    /// Register sources as jobs in this exact order; the queue preserves the
    /// order the files arrived in because that is the order the user chose.
    pub fn add(
        &mut self,
        sources: impl IntoIterator<Item = PathBuf>,
        target: TargetFormat,
    ) -> Vec<JobId> {
        sources
            .into_iter()
            .map(|source| {
                let id = JobId(self.next_id);
                self.next_id += 1;
                self.jobs.push(Job {
                    id,
                    source,
                    target,
                    status: JobStatus::Queued,
                    output: None,
                    progress: None,
                    started_at: None,
                    elapsed: None,
                    warnings: Vec::new(),
                    detail: None,
                });
                id
            })
            .collect()
    }

    /// Remove a job. Refused for running jobs: the UI must offer "cancel"
    /// there instead, and this is the guard that keeps that promise.
    pub fn remove(&mut self, id: JobId) -> bool {
        let Some(idx) = self.jobs.iter().position(|j| j.id == id) else {
            return false;
        };
        if self.jobs[idx].status == JobStatus::Running {
            return false;
        }
        self.jobs.remove(idx);
        true
    }

    pub fn clear_finished(&mut self) -> usize {
        let before = self.jobs.len();
        self.jobs.retain(|j| !j.status.is_terminal());
        before - self.jobs.len()
    }

    /// Mutable access to the running and queued jobs, in queue order. Used by
    /// batch start to resolve output paths.
    pub fn pending_mut(&mut self) -> impl Iterator<Item = &mut Job> {
        self.jobs
            .iter_mut()
            .filter(|j| matches!(j.status, JobStatus::Queued | JobStatus::Running))
    }

    pub fn counts(&self) -> Counts {
        let mut c = Counts {
            total: self.jobs.len(),
            ..Counts::default()
        };
        for j in &self.jobs {
            match j.status {
                JobStatus::Queued => c.queued += 1,
                JobStatus::Running => c.running += 1,
                JobStatus::Done => c.done += 1,
                JobStatus::Failed => c.failed += 1,
                JobStatus::Cancelled => c.cancelled += 1,
            }
        }
        c
    }

    /// Overall progress of the batch, for the top bar: terminal jobs count as
    /// whole units, running jobs contribute their fraction.
    pub fn batch_progress(&self) -> Option<f32> {
        let counts = self.counts();
        if counts.total == 0 {
            return None;
        }
        let mut acc = (counts.done + counts.failed + counts.cancelled) as f32;
        for j in &self.jobs {
            if j.status == JobStatus::Running {
                acc += j.progress.unwrap_or(0.0).clamp(0.0, 1.0);
            }
        }
        Some((acc / counts.total as f32).clamp(0.0, 1.0))
    }
}

// ---------------------------------------------------------------------------
// Output path resolution
// ---------------------------------------------------------------------------

/// Resolve where one job's output will be written.
///
/// Called once per job at batch start (not lazily by the engine) so the queue
/// can *show* the chosen names before anything runs. `taken` accumulates the
/// outputs of this batch so two identically named inputs from different
/// folders convert to distinct outputs even when the overwrite policy would
/// point both at the same name.
///
/// Returns `None` when no directory can be determined (unset custom folder).
pub fn resolve_output_path(
    settings: &Settings,
    source: &Path,
    taken: &mut HashSet<PathBuf>,
) -> Option<PathBuf> {
    let dir = settings.output_dir_for(source)?;
    let stem = source
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "untitled".to_string());
    let ext = settings.target.extension();
    let mut candidate = dir.join(format!("{stem}.{ext}"));

    if settings.overwrite == OverwritePolicy::AutoRename {
        let mut suffix = 1;
        while candidate.exists() || taken.contains(&candidate) {
            candidate = dir.join(format!("{stem}-{suffix}.{ext}"));
            suffix += 1;
            if suffix > 999 {
                // Absurd collision chains have some real cause; give up on the
                // chain rather than renumbering forever.
                break;
            }
        }
    } else {
        // Even when clobbering, in-batch duplicates must not fight over one
        // file. The filesystem itself is fair game under this policy.
        while taken.contains(&candidate) {
            let suffix = taken.len();
            candidate = dir.join(format!("{stem}-{suffix}.{ext}"));
        }
    }

    taken.insert(candidate.clone());
    Some(candidate)
}

/// The outputs of `sources` that already exist on disk, under this
/// settings' output mode and overwrite policy — what the Start action warns
/// with before "Overwrite files" is allowed to clobber anything.
///
/// It resolves with a fresh `taken` set exactly the way `start_batch` does
/// (which is why it lives beside [`resolve_output_path`]): the returned
/// names are the names the engine will be told to write, so a collision
/// here is a collision the batch would actually hit, not an approximation.
pub fn existing_outputs(
    settings: &Settings,
    sources: impl IntoIterator<Item = PathBuf>,
) -> Vec<PathBuf> {
    let mut taken = HashSet::new();
    sources
        .into_iter()
        .filter_map(|source| resolve_output_path(settings, &source, &mut taken))
        .filter(|output| output.exists())
        .collect()
}

// ---------------------------------------------------------------------------
// Drag & drop helpers
// ---------------------------------------------------------------------------

/// Expand dropped item paths into the file list to convert.
///
/// Directories are walked recursively (bounded depth, and junk directories
/// like `.git` are skipped) because "drop the whole handouts folder" is the
/// natural thing to do with a batch converter. Dropped open-document files
/// with no `path` set (clipboard textures, browser extensions' in-memory
/// payloads) cannot be converted from bytes-only for the demo engine, so are
/// dropped here.
pub fn collect_files(paths: impl IntoIterator<Item = PathBuf>, out: &mut Vec<PathBuf>) {
    const MAX_DEPTH: usize = 6;
    const MAX_FILES: usize = 5_000;
    const SKIP_DIRS: [&str; 4] = [".git", "target", "node_modules", "$RECYCLE.BIN"];

    fn walk(path: &Path, out: &mut Vec<PathBuf>, depth: usize) -> bool {
        if out.len() >= MAX_FILES || depth > MAX_DEPTH {
            return false;
        }
        if path.is_file() {
            out.push(path.to_path_buf());
            return true;
        }
        if depth == MAX_DEPTH {
            return false;
        }
        let Ok(entries) = std::fs::read_dir(path) else {
            return false;
        };
        let mut children: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .map(|n| n.to_string_lossy())
                    .map(|n| !n.starts_with('.') && !SKIP_DIRS.contains(&n.as_ref()))
                    .unwrap_or(false)
            })
            .collect();
        children.sort();
        for child in children {
            if !walk(&child, out, depth + 1) {
                return false;
            }
        }
        true
    }

    let cap_hit = out.len() >= MAX_FILES;
    for p in paths {
        if out.len() >= MAX_FILES || cap_hit {
            break;
        }
        walk(&p, out, 0);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_name_swaps_extension() {
        let settings = Settings::default();
        let mut taken = HashSet::new();
        let out = resolve_output_path(&settings, Path::new("C:/u/report.pdf"), &mut taken).unwrap();
        assert_eq!(out, PathBuf::from("C:/u/report.docx"));
    }

    #[test]
    fn auto_rename_avoids_disk_collision() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("report.txt"), b"x").unwrap();
        let settings = Settings {
            target: TargetFormat::PlainText,
            output_mode: OutputMode::Custom,
            custom_dir: Some(dir.path().to_path_buf()),
            ..Settings::default()
        };
        let mut taken = HashSet::new();
        let out = resolve_output_path(&settings, Path::new("C:/u/report.pdf"), &mut taken).unwrap();
        assert_eq!(out, dir.path().join("report-1.txt"));
    }

    #[test]
    fn in_batch_duplicates_distinct_even_when_overwriting() {
        let settings = Settings {
            overwrite: OverwritePolicy::Overwrite,
            ..Settings::default()
        };
        let mut taken = HashSet::new();
        let a = resolve_output_path(&settings, Path::new("C:/a/x.pdf"), &mut taken).unwrap();
        let b = resolve_output_path(&settings, Path::new("C:/b/x.pdf"), &mut taken).unwrap();
        assert_ne!(a, b, "same-name sources in one batch need distinct outputs");
    }

    #[test]
    fn custom_output_folder_is_used() {
        let settings = Settings {
            output_mode: OutputMode::Custom,
            custom_dir: Some(PathBuf::from("C:/out")),
            ..Settings::default()
        };
        let mut taken = HashSet::new();
        let out = resolve_output_path(&settings, Path::new("C:/u/doc.pdf"), &mut taken).unwrap();
        assert_eq!(out, PathBuf::from("C:/out/doc.docx"));
    }

    #[test]
    fn unset_custom_dir_fails_soft() {
        let settings = Settings {
            output_mode: OutputMode::Custom,
            custom_dir: None,
            ..Settings::default()
        };
        let mut taken = HashSet::new();
        assert!(resolve_output_path(&settings, Path::new("C:/u/doc.pdf"), &mut taken).is_none());
    }

    #[test]
    fn remove_refuses_running_jobs() {
        let mut q = Queue::new();
        let ids = q.add(vec![PathBuf::from("a.pdf")], TargetFormat::Docx);
        q.job_mut(ids[0]).unwrap().status = JobStatus::Running;
        assert!(!q.remove(ids[0]));
        assert_eq!(q.counts().total, 1);
    }

    #[test]
    fn clear_finished_keeps_pending() {
        let mut q = Queue::new();
        let ids = q.add(
            vec![
                PathBuf::from("a.pdf"),
                PathBuf::from("b.pdf"),
                PathBuf::from("c.pdf"),
            ],
            TargetFormat::Docx,
        );
        q.job_mut(ids[0]).unwrap().status = JobStatus::Done;
        q.job_mut(ids[1]).unwrap().status = JobStatus::Running;
        assert_eq!(q.clear_finished(), 1);
        assert_eq!(q.counts().total, 2);
    }

    #[test]
    fn batch_progress_waits_for_lagging_jobs() {
        let mut q = Queue::new();
        let ids = q.add(
            vec![PathBuf::from("a.pdf"), PathBuf::from("b.pdf")],
            TargetFormat::Docx,
        );
        let queued = q.counts();
        assert_eq!(queued.queued, 2, "nothing has started yet");
        q.job_mut(ids[0]).unwrap().status = JobStatus::Running;
        q.job_mut(ids[0]).unwrap().progress = Some(0.5);
        let p = q.batch_progress().unwrap();
        assert!((p - 0.25).abs() < 1e-6, "got {p}");
    }

    #[test]
    fn overwrite_collision_count_reports_only_existing_targets() {
        let dir = tempfile::tempdir().unwrap();
        // One target already on disk from an earlier run.
        std::fs::write(dir.path().join("first.docx"), b"x").unwrap();
        let settings = Settings {
            output_mode: OutputMode::Custom,
            custom_dir: Some(dir.path().to_path_buf()),
            ..Settings::default()
        };
        let sources = vec![
            PathBuf::from("C:/u/first.pdf"),
            PathBuf::from("C:/u/second.pdf"),
        ];

        // Under Overwrite, `first.pdf` resolves onto the file that exists.
        let mut overwrite = settings.clone();
        overwrite.overwrite = OverwritePolicy::Overwrite;
        let existing = existing_outputs(&overwrite, sources.clone());
        assert_eq!(existing.len(), 1, "exactly the existing file counts");
        assert!(
            existing.first().unwrap().ends_with("first.docx"),
            "the one existing path, {existing:?}"
        );

        // Under AutoRename the same batch resolves around it — nothing to warn about.
        assert!(
            existing_outputs(&settings, sources).is_empty(),
            "auto-rename resolves around what exists on disk"
        );
    }

    #[test]
    fn collect_files_walks_directories_once() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("a.pdf"), b"").unwrap();
        std::fs::write(dir.path().join("sub/b.pdf"), b"").unwrap();
        let _ = std::fs::write(dir.path().join(".hidden"), b"");
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/junk"), b"").unwrap();

        let mut out = Vec::new();
        collect_files(vec![dir.path().to_path_buf()], &mut out);
        assert_eq!(out.len(), 2, "got {out:?}");
    }
}
