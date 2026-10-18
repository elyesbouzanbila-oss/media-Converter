//! The boundary between the UI and whatever actually converts documents.
//!
//! The real engine (M1) is a pool of worker *processes* around the facade:
//! `spawn_real_engine` turns `EngineCommand`s into events exactly like the
//! demo did, and `main.rs` holds the one-line selection between them.
//!
//! Why processes: PDFium is a process-global singleton (`engine/pdf/src/bind.rs`
//! — the second bind in a process fails, finding A1), so parallelism is worker
//! processes, not threads. Each child is this same executable re-invoked with
//! [`WORKER_FLAG`]; it binds PDFium once, takes job lines over its stdin, and
//! emits one JSON event line per UI event over its stdout. The child calls
//! `Converter::convert` exactly the way `dconv` (engine/cli) does — that is
//! what makes byte-identical CLI vs GUI output true by construction rather
//! than by discipline.
//!
//! # Protocol contract (both implementations must honour)
//!
//! 1. One channel of commands in, one channel of events out. The UI never
//!    calls into the engine from more than its interface thread, and the
//!    engine never blocks the UI thread.
//! 2. Every job listed in a `Start` command produces **exactly one terminal
//!    event** (`Finished`, `Failed` or `Cancelled`). A job that is cancelled
//!    mid-flight must still be accounted for, or the UI's busy counter would
//!    wait forever.
//! 3. `Progress` fractions are `0.0..=1.0`.
//!
//! The demo deliberately mirrors the measured architecture from Phase 0 (a
//! pool of workers pulling from a shared queue, each reporting progress over
//! the channel) so the real adapter could be dropped in without re-shaping
//! the UI's threading assumptions — which is now how it actually runs.

use crate::state::{JobId, TargetFormat};
use dconv_engine::{Converter, EngineError, Format, PdfError, ReadStats, all_targets, targets_for};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Protocol
// ---------------------------------------------------------------------------

/// A job handed to the engine. The output path is already resolved (the UI
/// owns naming policy); the engine must write exactly there.
#[derive(Debug, Clone)]
pub struct JobSpec {
    pub id: JobId,
    pub source: PathBuf,
    pub target: TargetFormat,
    pub output: PathBuf,
}

/// UI → engine.
#[derive(Debug)]
pub enum EngineCommand {
    /// Queue jobs for the conversion pool. `workers` is the desired pool size;
    /// repeated `Start`s may grow the pool mid-batch.
    Start { jobs: Vec<JobSpec>, workers: usize },
    /// Cancel every queued job and stop every running one. Jobs already
    /// finished cannot be cancelled, obviously.
    CancelAll,
    /// Stop the engine and join its threads. Also fine to cancel everything.
    Shutdown,
}

/// Engine → UI.
#[derive(Debug)]
pub enum EngineEvent {
    Started {
        id: JobId,
    },
    Progress {
        id: JobId,
        frac: f32,
    },
    Warning {
        id: JobId,
        text: String,
    },
    Finished {
        id: JobId,
        output: PathBuf,
        elapsed: Duration,
    },
    Failed {
        id: JobId,
        summary: String,
        detail: Option<String>,
        elapsed: Duration,
    },
    Cancelled {
        id: JobId,
    },
}

/// The pair of channel ends the app holds for one engine instance.
pub struct EngineChannels {
    pub commands: Sender<EngineCommand>,
    pub events: Receiver<EngineEvent>,
}

// ---------------------------------------------------------------------------
// The real engine (M1): worker processes around the facade
// ---------------------------------------------------------------------------

/// CLI flag the GUI re-invokes itself with to become one conversion worker.
/// Namespaced so it can never collide with a user-facing flag.
pub const WORKER_FLAG: &str = "--dconvert-worker";

/// Which output formats the engine can produce for a file with this
/// extension.
///
/// Asks the facade instead of mirroring it: `Format::from_extension` owns the
/// dot and the case, and [`targets_for`] walks the same `CONVERSIONS` table
/// `Converter::convert` gates on, so what the settings panel offers and what
/// a conversion would accept cannot drift apart. What remains is
/// [`target_of`]'s deliberate seam: when the facade learns a target this UI
/// does not name, `app` stops compiling there, which forces the mapping
/// decision instead of hiding a new format behind a catch-all.
pub fn supported(source_ext: &str) -> Vec<TargetFormat> {
    let Some(source) = Format::from_extension(source_ext) else {
        return Vec::new();
    };
    targets_for(source)
        .into_iter()
        .filter_map(target_of)
        .collect()
}

/// The union of [`supported`] over every file the engine reads — the list
/// the settings panel offers. Taken from the facade's [`all_targets`], which
/// is documented as exactly "the union a settings panel offers".
pub fn supported_targets() -> Vec<TargetFormat> {
    all_targets().into_iter().filter_map(target_of).collect()
}

/// Whether the engine reads this file's extension at all. Used to keep
/// unsupported files out of the queue with an upfront explanation instead of
/// one failed job per file ("unknown format" / "unsupported pair"), which
/// would be noise rather than guidance.
pub fn readable(path: &Path) -> bool {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_default();
    !supported(&ext).is_empty()
}

/// Facade format → settings value, an *exhaustive* match on purpose. A
/// catch-all would make a new facade target silently never appear in the
/// settings panel; a missing arm makes `app` fail to compile until the UI
/// owner classifies it. `Format::Pdf` refuses for a reason worth stating: a
/// PDF is a source, never a target.
fn target_of(format: Format) -> Option<TargetFormat> {
    match format {
        Format::Pdf => None,
        Format::Docx => Some(TargetFormat::Docx),
    }
}

// --- worker protocol: one JSON line per message, ids travel with events ---

/// Parent → child: assign a job, or stop.
///
/// The child processes exactly one job at a time and prints event lines, so
/// `target` is a bare format name (`docx`) rather than a `TargetFormat`: the
/// child has no dependency on the UI's settings types beyond this string.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum WorkerIn {
    Job {
        id: u64,
        source: PathBuf,
        output: PathBuf,
        target: String,
    },
    Shutdown,
}

/// Child → parent: one step of a job, or a terminal report.
///
/// `Fatal` covers "could not even bind PDFium", before any job existed. The
/// dispatcher binds first and refuses to spawn children when that fails, so
/// this is a defensive path, not the expected one.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum WorkerOut {
    Started {
        id: u64,
    },
    Progress {
        id: u64,
        frac: f32,
    },
    Warning {
        id: u64,
        text: String,
    },
    Finished {
        id: u64,
        output: PathBuf,
        elapsed_ms: u64,
    },
    Failed {
        id: u64,
        summary: String,
        detail: Option<String>,
        elapsed_ms: u64,
    },
    Cancelled {
        id: u64,
    },
    Fatal {
        summary: String,
        detail: String,
    },
}

/// Translate one child line into a protocol event. Returns the job id only
/// when the event is terminal, which is the reader's queue for registry
/// bookkeeping. `Fatal` is not an event: it becomes the failure detail when
/// the child is reaped below.
fn worker_event(out: WorkerOut) -> Option<(EngineEvent, Option<u64>)> {
    let event = match out {
        WorkerOut::Started { id } => (EngineEvent::Started { id: JobId(id) }, None),
        WorkerOut::Progress { id, frac } => {
            // Contract item 3: clamp here so a future Sink change cannot leak
            // an out-of-range fraction into the UI.
            (
                EngineEvent::Progress {
                    id: JobId(id),
                    frac: frac.clamp(0.0, 1.0),
                },
                None,
            )
        }
        WorkerOut::Warning { id, text } => (
            EngineEvent::Warning {
                id: JobId(id),
                text,
            },
            None,
        ),
        WorkerOut::Finished {
            id,
            output,
            elapsed_ms,
        } => (
            EngineEvent::Finished {
                id: JobId(id),
                output,
                elapsed: Duration::from_millis(elapsed_ms),
            },
            Some(id),
        ),
        WorkerOut::Failed {
            id,
            summary,
            detail,
            elapsed_ms,
        } => (
            EngineEvent::Failed {
                id: JobId(id),
                summary,
                detail,
                elapsed: Duration::from_millis(elapsed_ms),
            },
            Some(id),
        ),
        WorkerOut::Cancelled { id } => (EngineEvent::Cancelled { id: JobId(id) }, Some(id)),
        WorkerOut::Fatal { .. } => return None,
    };
    Some(event)
}

/// Human words for a facade error: a short phrase for the log line, the full
/// text for the details tab.
///
/// Cancellation matches by variant now that the facade re-exports `PdfError`.
/// It still cannot arise in the worker path — workers have no cooperative
/// cancel; the parent kills the process, and `Cancelled` events come from the
/// parent's death bookkeeping — but this helper classifies whatever the
/// facade returns, and mislabelling a cancel as a failure is exactly the
/// confusion the re-export was asked for to prevent.
fn explain_error(error: &EngineError) -> (String, Option<String>) {
    let summary = match error {
        EngineError::Reader(PdfError::Cancelled) => "the conversion was cancelled",
        EngineError::UnknownFormat { .. } => "the file format could not be determined",
        EngineError::UnsupportedPair { .. } => "this output format is not supported yet",
        _ => "the conversion failed",
    };
    (summary.to_owned(), Some(error.to_string()))
}

/// The warning lines for one finished job.
///
/// Same rules as the CLI's `describe`: a converter that silently deletes
/// characters must say so, and one that deletes none must not clutter the
/// log with zeros. The numbers come straight from the facade's `ReadStats`
/// (re-exported alongside `Sink`); per-front-end phrasing is presentation,
/// which is why the wording lives here and the counting does not have to.
fn warning_lines(stats: &ReadStats) -> Vec<String> {
    let control_dropped = stats.control_dropped();
    let mut out = Vec::new();
    if stats.synthetic_dropped > 0 || control_dropped > 0 {
        out.push(format!(
            "discarded {} synthetic separators, {} code points",
            stats.synthetic_dropped, control_dropped
        ));
    }
    // U+0002 is worth naming: PDFium maps some real hyphens to it, the
    // character is not recoverable, and the omission is visible in the text
    // (finding D3 — a wrong character would be invisible corruption).
    let dropped_dash = stats.dropped_codepoints.get(&2).copied().unwrap_or(0);
    if dropped_dash > 0 {
        out.push(format!(
            "{dropped_dash} dash character(s) could not be read — PDFium maps some \
             real U+002D hyphens to U+0002, and the character is not recoverable"
        ));
    }
    out
}

// --- the child side ---------------------------------------------------------

/// Worker-mode entry point, called by `main` before any GUI code exists.
/// Never spawns windows, never prints to stderr: its whole interface is its
/// two stdio pipes.
pub fn worker_main() -> i32 {
    match run_worker() {
        Ok(()) => 0,
        Err(_) => 1,
    }
}

fn run_worker() -> std::io::Result<()> {
    let mut out = BufWriter::new(std::io::stdout().lock());

    // Bind once, up front: without PDFium no job in this process can run, and
    // the parent rather sees one `Fatal` than a queue of doomed jobs. (The
    // parent binds first as well, so a failed bind normally prevents any
    // child from being spawned at all.)
    let converter = match Converter::bind() {
        Ok(converter) => converter,
        Err(error) => {
            emit(
                &mut out,
                &WorkerOut::Fatal {
                    summary: "PDFium (pdfium.dll) was not found".to_owned(),
                    detail: error.to_string(),
                },
            );
            return Ok(());
        }
    };

    // Line machine: jobs in, event lines out. An EOF means the parent closed
    // our stdin — which is both its clean shutdown and its cancel mechanism,
    // since killing the process is safe only here: PDFium's global state dies
    // with the process, so an interrupted page leaves no undefined state.
    let mut input = std::io::stdin().lock().lines();
    while let Some(line) = input.next() {
        let Ok(line) = line else { return Ok(()) };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<WorkerIn>(line) {
            Ok(WorkerIn::Job {
                id,
                source,
                output,
                target,
            }) => {
                run_job(&converter, &mut out, id, source, output, &target);
            }
            Ok(WorkerIn::Shutdown) => return Ok(()),
            // Unknown lines are ignored rather than fatal, so the protocol
            // can grow without coordinating releases.
            Err(_) => {}
        }
    }
    Ok(())
}

/// Write one line and flush: the transport is unbuffered line-delimited JSON,
/// and a lost parent is not an error worth failing over — the job should
/// still finish its real work; the loop notices the pipe once it next reads.
fn emit<W: Write>(out: &mut W, line: &WorkerOut) {
    let _ = serde_json::to_writer(&mut *out, line);
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

/// The `Sink` for the job in flight. Progress arrives per page (what `Sink`
/// offers today); no cooperative cancel — the parent kills the process, which
/// is the cancellation story for worker processes by construction.
struct JobProgress<'a, W: Write> {
    out: &'a mut W,
    id: u64,
}

impl<W: Write> dconv_engine::Sink for JobProgress<'_, W> {
    fn page(&mut self, done: usize, total: usize) {
        emit(
            self.out,
            &WorkerOut::Progress {
                id: self.id,
                frac: done as f32 / total.max(1) as f32,
            },
        );
    }
}

fn run_job(
    converter: &Converter,
    out: &mut BufWriter<std::io::StdoutLock<'static>>,
    id: u64,
    source: PathBuf,
    output: PathBuf,
    target: &str,
) {
    let started = Instant::now();
    emit(out, &WorkerOut::Started { id });

    // The CLI creates the output folder before converting; do the same so a
    // first run with `-o out\` (its settings-panel equivalent) behaves
    // identically instead of failing on a missing directory.
    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(error) = std::fs::create_dir_all(parent) {
                emit(
                    out,
                    &WorkerOut::Failed {
                        id,
                        summary: "creating the output folder failed".to_owned(),
                        detail: Some(error.to_string()),
                        elapsed_ms: started.elapsed().as_millis() as u64,
                    },
                );
                return;
            }
        }
    }

    // Only what `convert` actually accepts may come through; anything else is
    // a UI bug at worst (the capability list gates the selection), reported
    // as a clean failure at best.
    if target != TargetFormat::Docx.extension() {
        let name = if target == TargetFormat::PlainText.extension() {
            "plain text"
        } else {
            target
        };
        emit(
            out,
            &WorkerOut::Failed {
                id,
                summary: format!("{name} output is not supported yet"),
                detail: Some(
                    "This build converts PDF to DOCX; the format list in settings \
                     only offers what the engine supports."
                        .to_owned(),
                ),
                elapsed_ms: started.elapsed().as_millis() as u64,
            },
        );
        return;
    }

    let mut sink = JobProgress { out: &mut *out, id };
    match converter.convert(&source, &output, &mut sink) {
        Ok(report) => {
            drop(sink); // the borrow must end before more emits borrow `out`
            for text in warning_lines(&report.stats) {
                emit(out, &WorkerOut::Warning { id, text });
            }
            emit(
                out,
                &WorkerOut::Finished {
                    id,
                    output,
                    elapsed_ms: started.elapsed().as_millis() as u64,
                },
            );
        }
        Err(error) => {
            drop(sink);
            let (summary, detail) = explain_error(&error);
            emit(
                out,
                &WorkerOut::Failed {
                    id,
                    summary,
                    detail,
                    elapsed_ms: started.elapsed().as_millis() as u64,
                },
            );
        }
    }
}

// --- the parent side ---------------------------------------------------------

/// Start the real engine: a pool of worker processes over a shared queue,
/// speaking the exact same command/event protocol as the demo.
pub fn spawn_real_engine() -> EngineChannels {
    let (cmd_tx, cmd_rx) = channel();
    let (ev_tx, ev_rx) = channel();
    std::thread::Builder::new()
        .name("engine".into())
        .spawn(move || run_dispatcher(cmd_rx, ev_tx))
        .expect("spawn engine dispatcher");
    EngineChannels {
        commands: cmd_tx,
        events: ev_rx,
    }
}

/// What the dispatcher, the per-worker writer threads and the reader threads
/// share. The queue and both flags sit in one shape so that a job being
/// handed to a worker and a "cancel all" can never disagree about who owns
/// it: jobs not yet pushed into a child are cancelled by the dispatcher, and
/// jobs already inside a child are cancelled by that child's reader when the
/// process dies.
struct PoolShared {
    queue: Mutex<VecDeque<JobSpec>>,
    ready: Condvar,
    /// Set while a cancel is unwinding. A child that dies during this window
    /// means its registered jobs were *cancelled*; outside the window, the
    /// child exiting without finishing them is a *failure*.
    cancelling: AtomicBool,
    exiting: AtomicBool,
}

/// A job written into a child's stdin and not yet finished: registered at
/// write time so that a kill racing the write still finds it accounted for.
struct SentJob {
    spec: JobSpec,
    sent_at: Instant,
}

/// One live worker process. The dispatcher owns the `Child` (to kill and reap
/// it); the reader thread owns the pipes and the `sent` registry between them.
struct WorkerSlot {
    child: Child,
}

/// Whether the parent process has bound PDFium yet: once per process (finding
/// A1 — the second bind in a process fails); failures are retried each batch
/// since a mid-session repair is legitimate and a rebind attempt costs
/// directory lookups only.
type BindState = bool;

fn run_dispatcher(cmd_rx: Receiver<EngineCommand>, ev_tx: Sender<EngineEvent>) {
    let shared = Arc::new(PoolShared {
        queue: Mutex::new(VecDeque::new()),
        ready: Condvar::new(),
        cancelling: AtomicBool::new(false),
        exiting: AtomicBool::new(false),
    });
    let mut slots: Vec<WorkerSlot> = Vec::new();
    let mut bound: BindState = false;

    while let Ok(command) = cmd_rx.recv() {
        match command {
            EngineCommand::Start { jobs, workers } => {
                if let Err((summary, detail)) = ensure_bound(&mut bound) {
                    // Fail before doing any work, so a batch of fifty files
                    // does not fail on the fiftieth after forty-nine ran:
                    // the same promise the facade makes for unsupported pairs.
                    for spec in jobs {
                        let _ = ev_tx.send(EngineEvent::Failed {
                            id: spec.id,
                            summary: summary.clone(),
                            detail: Some(detail.clone()),
                            elapsed: Duration::ZERO,
                        });
                    }
                    continue;
                }

                let wanted = workers.max(1).min(16);
                {
                    let mut queue = shared.queue.lock().unwrap();
                    queue.extend(jobs);
                }
                shared.ready.notify_all();

                // Reap slots whose process exited (a crash, or the previous
                // cancel), then refill to the requested size. Starts may
                // grow the pool mid-batch; shrinking is ignored until the
                // next batch, matching the demo's behaviour and the protocol
                // comment on `Start`.
                slots.retain_mut(|slot| {
                    slot.child
                        .try_wait()
                        .map_or(true, |status| status.is_none())
                });
                let mut spawn_error = None;
                while slots.len() < wanted {
                    match spawn_worker(&shared, &ev_tx) {
                        Ok(slot) => slots.push(slot),
                        Err(error) => {
                            spawn_error = Some(error);
                            break;
                        }
                    }
                }
                if slots.is_empty() {
                    let detail = spawn_error
                        .unwrap_or_else(|| "no worker process could be started".to_owned());
                    let summary = "the conversion pool could not start".to_owned();
                    let mut queue = shared.queue.lock().unwrap();
                    for spec in queue.drain(..) {
                        let _ = ev_tx.send(EngineEvent::Failed {
                            id: spec.id,
                            summary: summary.clone(),
                            detail: Some(detail.clone()),
                            elapsed: Duration::ZERO,
                        });
                    }
                }
            }
            EngineCommand::CancelAll => {
                // Two populations, two owners, exactly like the demo: jobs
                // not yet pushed into a child are drained and cancelled
                // here; jobs already inside a child die with the process,
                // and that child's reader turns each of them into exactly
                // one `Cancelled`. Arm the flag *before* killing anything —
                // the flag is what decides that a dying child's leftovers
                // read as cancelled rather than failed.
                shared.cancelling.store(true, Ordering::Relaxed);
                {
                    let mut queue = shared.queue.lock().unwrap();
                    for spec in queue.drain(..) {
                        let _ = ev_tx.send(EngineEvent::Cancelled { id: spec.id });
                    }
                }
                for slot in &mut slots {
                    let _ = slot.child.kill();
                }
                shared.ready.notify_all();
            }
            EngineCommand::Shutdown => {
                shared.exiting.store(true, Ordering::Relaxed);
                shared.ready.notify_all();
                // Kill, then reap: the kill breaks every pipe immediately and
                // the process list holds no handles afterwards. Writers see
                // the flag or the broken pipe and leave; readers see EOF.
                for slot in &mut slots {
                    let _ = slot.child.kill();
                }
                for mut slot in slots.drain(..) {
                    let _ = slot.child.wait();
                }
                drop(ev_tx);
                return;
            }
        }
    }
    // The UI dropped its command sender without Shutdown: tear down the same
    // way anyway so no worker process outlives the app.
    shared.exiting.store(true, Ordering::Relaxed);
    shared.ready.notify_all();
    for slot in &mut slots {
        let _ = slot.child.kill();
    }
}

/// Retry-or-cache PDFium binding in the parent process. Success is cached for
/// the process lifetime; failure only returns the message, and the next batch
/// tries the search again.
fn ensure_bound(bound: &mut bool) -> Result<(), (String, String)> {
    if *bound {
        return Ok(());
    }
    match Converter::bind() {
        Ok(_) => {
            *bound = true;
            Ok(())
        }
        Err(error) => {
            // The error's display lists every directory that was searched,
            // which is the useful part; the repairs spell out what to do
            // about it.
            let detail = format!(
                "{error}\n\nRun scripts\\fetch-pdfium.ps1 to fetch it, or set the \
                 DCONV_PDFIUM_DIR environment variable to a folder that contains \
                 pdfium.dll.",
            );
            Err(("PDFium (pdfium.dll) was not found".to_owned(), detail))
        }
    }
}

/// Spawn one worker child and its writer/reader threads.
///
/// Best-effort with the reason on hand: a deterministic spawn failure (locked
/// or moved executable) becomes a clean batch failure, not a silent stall.
fn spawn_worker(
    shared: &Arc<PoolShared>,
    ev_tx: &Sender<EngineEvent>,
) -> Result<WorkerSlot, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;

    let mut command = Command::new(&exe);
    command
        .arg(WORKER_FLAG)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // The child deliberately writes nothing to stderr, and a pipe nobody
        // drains is a deadlock waiting to happen, so give it nothing at all
        // rather than leaking a console into the parent's window tree.
        .stderr(Stdio::null());
    hide_worker_window(&mut command);

    let mut child = command.spawn().map_err(|e| e.to_string())?;
    let (Some(child_stdin), Some(child_stdout)) = (child.stdin.take(), child.stdout.take()) else {
        let _ = child.kill();
        return Err("the worker process did not expose stdio pipes".to_owned());
    };

    let sent = Arc::new(Mutex::new(HashMap::new()));

    // Writer thread: pops from the shared queue and pushes job lines into the
    // child. It does not wait for the previous job to finish: the child
    // buffers unstarted jobs in its stdin pipe, which is how the pool stays
    // saturated and the queue never wraps around a busy worker.
    {
        let shared = Arc::clone(shared);
        let sent = Arc::clone(&sent);
        std::thread::Builder::new()
            .name("engine-writer".into())
            .spawn(move || pump_jobs(shared, sent, child_stdin))
            .expect("spawn engine writer");
    }
    {
        let shared = Arc::clone(shared);
        let sent = Arc::clone(&sent);
        let ev_tx = ev_tx.clone();
        std::thread::Builder::new()
            .name("engine-reader".into())
            .spawn(move || read_events(child_stdout, shared, sent, ev_tx))
            .expect("spawn engine reader");
    }

    Ok(WorkerSlot { child })
}

#[cfg(windows)]
fn hide_worker_window(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    // dconvert is a console-subsystem binary; a GUI process spawning it would
    // otherwise get one console window per worker flashing up.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn hide_worker_window(_command: &mut Command) {}

/// Take jobs from the shared queue and write them into the child. The
/// pop-and-register order matters: register *before* the first byte of the
/// write, so a kill racing the write finds the job accounted for either way.
fn pump_jobs(
    shared: Arc<PoolShared>,
    sent: Arc<Mutex<HashMap<u64, SentJob>>>,
    mut pipe: impl Write,
) {
    loop {
        let spec = {
            let mut queue = shared.queue.lock().unwrap();
            loop {
                if shared.exiting.load(Ordering::Relaxed) {
                    return;
                }
                if let Some(spec) = queue.pop_front() {
                    break spec;
                }
                queue = shared.ready.wait(queue).unwrap();
            }
        };
        sent.lock().unwrap().insert(
            spec.id.0,
            SentJob {
                spec: spec.clone(),
                sent_at: Instant::now(),
            },
        );
        let line = WorkerIn::Job {
            id: spec.id.0,
            source: spec.source,
            output: spec.output,
            target: spec.target.extension().to_string(),
        };
        // Serialisation of this type cannot fail (no maps or floats in its
        // keys), so the expects document an invariant rather than guard one.
        let body = serde_json::to_string(&line).expect("worker line serialises");
        if pipe
            .write_all(body.as_bytes())
            .and_then(|_| pipe.write_all(b"\n"))
            .and_then(|_| pipe.flush())
            .is_err()
        {
            // The child is gone; its reader turns the registered jobs into
            // terminal events. Nothing queued here is lost.
            return;
        }
    }
}

/// Read the child's event lines until the pipe ends, then turn anything still
/// registered into exactly one terminal event per job. The reader owns the
/// accounting: terminal lines de-register on the way through, so a child that
/// dies after *partially* reporting is still reconciled correctly.
fn read_events(
    pipe: impl Read,
    shared: Arc<PoolShared>,
    sent: Arc<Mutex<HashMap<u64, SentJob>>>,
    ev_tx: Sender<EngineEvent>,
) {
    let mut reader = BufReader::new(pipe);
    let mut line = String::new();
    let mut fatal: Option<(String, Option<String>)> = None;
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(out) = serde_json::from_str::<WorkerOut>(line) else {
            // Unknown line shapes are ignored, matching the child's tolerance.
            continue;
        };
        match out {
            WorkerOut::Fatal { summary, detail } => {
                fatal = Some((summary, Some(detail)));
                break;
            }
            other => {
                let Some((event, terminal_id)) = worker_event(other) else {
                    continue;
                };
                let _ = ev_tx.send(event);
                if let Some(id) = terminal_id {
                    sent.lock().unwrap().remove(&id);
                }
            }
        }
    }

    // A shutdown is in progress: the UI is going away, events no longer
    // matter, and emitting them would race the app's teardown.
    if shared.exiting.load(Ordering::Relaxed) {
        return;
    }

    let leftover = {
        let mut registry = sent.lock().unwrap();
        std::mem::take(&mut *registry)
    };
    let cancelling = shared.cancelling.load(Ordering::Relaxed);
    for (id, job) in leftover {
        let elapsed = job.sent_at.elapsed();
        let event = match (cancelling, &fatal) {
            (true, _) => {
                // A cancelled job must not leave a half-written file behind.
                // A *finished* job had its id removed from the registry on
                // its way through, so a valid output is never deleted here.
                let _ = std::fs::remove_file(&job.spec.output);
                EngineEvent::Cancelled { id: JobId(id) }
            }
            (false, Some((summary, detail))) => EngineEvent::Failed {
                id: JobId(id),
                summary: summary.clone(),
                detail: detail.clone(),
                elapsed,
            },
            (false, None) => EngineEvent::Failed {
                id: JobId(id),
                summary: "the worker process exited unexpectedly".to_owned(),
                detail: Some(format!(
                    "{}: the worker exited before it reported a result; the job \
                     may or may not have started.",
                    job.spec.source.display()
                )),
                elapsed,
            },
        };
        let _ = ev_tx.send(event);
    }
}
// ---------------------------------------------------------------------------
// Demo engine
// ---------------------------------------------------------------------------
//
// Kept intact for UI review without a PDFium dependency: swapping it into
// `main.rs` with one line runs the whole shell against a simulated pool.
// Demo sensitivities: a file name containing `fail` is reported as failed,
// one containing `warn` produces warnings, and job runtimes are derived from
// the file name so batches are reproducible.

#[allow(dead_code)] // reachable only after the one-line swap in `main.rs`
pub mod demo {
    use super::*;
    pub fn spawn_demo_engine() -> EngineChannels {
        let (cmd_tx, cmd_rx) = channel();
        let (ev_tx, ev_rx) = channel();
        std::thread::Builder::new()
            .name("demo-engine".into())
            .spawn(move || run_dispatcher(cmd_rx, ev_tx))
            .expect("spawn demo engine dispatcher");
        EngineChannels {
            commands: cmd_tx,
            events: ev_rx,
        }
    }

    /// What workers and the dispatcher share. Queue + the running-registry live
    /// under one mutex together: a job being popped by a worker and "cancel all"
    /// read by the dispatcher must see the same world, or a job slipping between
    /// the two would be neither queued nor running and therefore un-cancellable.
    struct Shared {
        queue: Mutex<Queues>,
        ready: Condvar,
        cancel_requested: Mutex<HashSet<u64>>,
        exiting: AtomicBool,
    }

    #[derive(Default)]
    struct Queues {
        pending: VecDeque<JobSpec>,
        running: HashMap<u64, ()>,
    }

    fn run_dispatcher(cmd_rx: Receiver<EngineCommand>, ev_tx: Sender<EngineEvent>) {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queues::default()),
            ready: Condvar::new(),
            cancel_requested: Mutex::new(HashSet::new()),
            exiting: AtomicBool::new(false),
        });
        let mut workers: Vec<std::thread::JoinHandle<()>> = Vec::new();

        while let Ok(cmd) = cmd_rx.recv() {
            match cmd {
                EngineCommand::Start { jobs, workers: n } => {
                    let workers_wanted = n.max(1).min(16);
                    {
                        let mut q = shared.queue.lock().unwrap();
                        q.pending.extend(jobs);
                    }
                    while workers.len() < workers_wanted {
                        let worker_shared = Arc::clone(&shared);
                        let worker_ev = ev_tx.clone();
                        workers.push(
                            std::thread::Builder::new()
                                .name("demo-worker".into())
                                .spawn(move || run_worker(worker_shared, worker_ev))
                                .expect("spawn demo worker"),
                        );
                    }
                    shared.ready.notify_all();
                }
                EngineCommand::CancelAll => {
                    // Queued jobs are cancelled wholesale by the dispatcher;
                    // jobs already taken by a worker are flagged, and each worker
                    // discovers and reports its own `Cancelled` at the next
                    // progress step. Split that way on purpose: exactly one
                    // `Cancelled` per job, whoever owns it when the command
                    // arrives.
                    let (queued_ids, running_ids) = {
                        let mut q = shared.queue.lock().unwrap();
                        let queued: Vec<u64> = q.pending.iter().map(|j| j.id.0).collect();
                        let _ = q.pending.drain(..);
                        let running: Vec<u64> = q.running.keys().copied().collect();
                        (queued, running)
                    };
                    shared
                        .cancel_requested
                        .lock()
                        .unwrap()
                        .extend(queued_ids.iter().chain(&running_ids).copied());
                    for id in &queued_ids {
                        let _ = ev_tx.send(EngineEvent::Cancelled { id: JobId(*id) });
                    }
                    shared.ready.notify_all();
                }
                EngineCommand::Shutdown => {
                    shared.exiting.store(true, Ordering::Relaxed);
                    shared.ready.notify_all();
                    for w in workers.drain(..) {
                        let _ = w.join();
                    }
                    drop(ev_tx);
                    return;
                }
            }
        }
        // cmd_rx closed without Shutdown: tear down politely anyway.
        shared.exiting.store(true, Ordering::Relaxed);
        shared.ready.notify_all();
        for w in workers.drain(..) {
            let _ = w.join();
        }
    }

    fn run_worker(shared: Arc<Shared>, ev_tx: Sender<EngineEvent>) {
        loop {
            // Take a job atomically with registering it as running, which is what
            // makes the CancelAll race window above impossible rather than merely
            // narrow.
            let spec = {
                let mut q = shared.queue.lock().unwrap();
                let next = loop {
                    if shared.exiting.load(Ordering::Relaxed) {
                        return;
                    }
                    if let Some(next) = q.pending.pop_front() {
                        break next;
                    }
                    q = shared.ready.wait(q).unwrap();
                };
                q.running.insert(next.id.0, ());
                next
            };

            let started = Instant::now();
            let _ = ev_tx.send(EngineEvent::Started { id: spec.id });

            let total = demo_job_duration(&spec.source);
            let steps = 20;
            for step in 1..=steps {
                std::thread::sleep(total / steps);
                if shared.cancel_requested.lock().unwrap().remove(&spec.id.0) {
                    let mut q = shared.queue.lock().unwrap();
                    q.running.remove(&spec.id.0);
                    drop(q);
                    let _ = ev_tx.send(EngineEvent::Cancelled { id: spec.id });
                    break;
                }
                let _ = ev_tx.send(EngineEvent::Progress {
                    id: spec.id,
                    frac: step as f32 / steps as f32,
                });

                let name = spec
                    .source
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let lower = name.to_lowercase();
                if step == steps / 3 && lower.contains("warn") {
                    let _ = ev_tx.send(EngineEvent::Warning {
                        id: spec.id,
                        text: "demo: 1 hyphen lost in a subscripted ligature (cf. finding D3)"
                            .into(),
                    });
                }
                if step == steps - 3 && lower.contains("warn") {
                    let _ = ev_tx.send(EngineEvent::Warning {
                        id: spec.id,
                        text: "demo: tense cluster of 6 tokens re-ordered (cf. finding D2)".into(),
                    });
                }
            }

            let elapsed = started.elapsed();
            {
                let mut q = shared.queue.lock().unwrap();
                q.running.remove(&spec.id.0);
            }

            let name = spec
                .source
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            if name.to_lowercase().contains("fail") {
                let _ = ev_tx.send(EngineEvent::Failed {
                    id: spec.id,
                    summary: "demo failure".into(),
                    detail: Some(
                        "The demo engine reports a failure for any file whose name contains \
                     \"fail\", so the failure path of the UI can be reviewed without a \
                     real converter. The real engine (M1) reports errors from the facade."
                            .into(),
                    ),
                    elapsed,
                });
            } else {
                let _ = ev_tx.send(EngineEvent::Finished {
                    id: spec.id,
                    output: spec.output,
                    elapsed,
                });
            }
        }
    }

    /// Deterministic pseudo-runtime per job, 0.5–2.1 s, keyed by the file name so
    /// demos and screenshots reproduce.
    fn demo_job_duration(source: &Path) -> Duration {
        let name = source
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        Hash::hash(&name, &mut hasher);
        let millis = 500 + hasher.finish() % 1_600;
        Duration::from_millis(millis as u64)
    }
} // mod demo

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_lines_round_trip_through_the_protocol() {
        let line = WorkerIn::Job {
            id: 7,
            source: PathBuf::from("C:/u/3IRS_TP2.PDF"),
            output: PathBuf::from("C:/out/3IRS_TP2.docx"),
            target: "docx".to_owned(),
        };
        let text = serde_json::to_string(&line).unwrap();
        match serde_json::from_str::<WorkerIn>(&text).unwrap() {
            WorkerIn::Job {
                id,
                source,
                output,
                target,
            } => {
                assert_eq!(id, 7);
                assert_eq!(source, PathBuf::from("C:/u/3IRS_TP2.PDF"));
                assert_eq!(output, PathBuf::from("C:/out/3IRS_TP2.docx"));
                assert_eq!(target, "docx");
            }
            _ => panic!("wrong tag"),
        }
    }

    #[test]
    fn event_lines_translate_with_one_terminal_each() {
        let (event, id) = worker_event(WorkerOut::Finished {
            id: 3,
            output: PathBuf::from("C:/out/doc.docx"),
            elapsed_ms: 1500,
        })
        .unwrap();
        assert!(
            matches!(event, EngineEvent::Finished { ref output, .. } if output == &PathBuf::from("C:/out/doc.docx"))
        );
        assert_eq!(id, Some(3), "finished is terminal");

        let (event, id) = worker_event(WorkerOut::Progress { id: 1, frac: 1.7 }).unwrap();
        assert!(
            matches!(event, EngineEvent::Progress { frac, .. } if (frac - 1.0).abs() < 1e-6),
            "fraction clamps to 1.0"
        );
        assert_eq!(id, None, "progress is not terminal");

        let (event, id) = worker_event(WorkerOut::Failed {
            id: 2,
            summary: "s".into(),
            detail: None,
            elapsed_ms: 0,
        })
        .unwrap();
        assert!(matches!(event, EngineEvent::Failed { .. }));
        assert_eq!(id, Some(2));

        assert!(
            worker_event(WorkerOut::Fatal {
                summary: "s".into(),
                detail: "d".into()
            })
            .is_none()
        );
    }

    #[test]
    fn cancel_lines_translate_to_cancelled_events() {
        let (event, id) = worker_event(WorkerOut::Cancelled { id: 9 }).unwrap();
        assert!(matches!(event, EngineEvent::Cancelled { id: JobId(9) }));
        assert_eq!(id, Some(9));
    }

    #[test]
    fn capability_asks_the_facade_not_a_mirror() {
        // The facade's CONVERSIONS table is PDF → DOCX and nothing else. The
        // delegated query must reproduce exactly that: no target for a
        // `.docx` input (no reader yet), none for `.txt` (no writer yet),
        // while the dot and case handling is `Format::from_extension`'s job
        // and no longer ours.
        assert_eq!(supported("pdf"), vec![TargetFormat::Docx]);
        assert_eq!(
            supported("PDF"),
            vec![TargetFormat::Docx],
            "case-insensitive"
        );
        assert_eq!(supported(".pdf"), vec![TargetFormat::Docx], "dot tolerated");
        assert_eq!(supported_targets(), supported("pdf"));
        assert!(supported("docx").is_empty());
        assert!(supported("txt").is_empty());
        assert!(supported("").is_empty());
    }

    #[test]
    fn dropped_code_points_become_cli_worded_warnings() {
        // Same wording as the CLI's describe(): a converter that silently
        // deletes characters says so, and the unrecoverable dash gets named.
        // U+0002 is the code point PDFium hands back for unreadable hyphens.
        let stats = ReadStats {
            synthetic_dropped: 2,
            dropped_codepoints: [(2, 3)].into_iter().collect(),
            ..ReadStats::default()
        };
        let warnings = warning_lines(&stats);
        assert_eq!(warnings.len(), 2);
        assert!(
            warnings[0].contains("2 synthetic separators"),
            "{}",
            warnings[0]
        );
        assert!(warnings[0].contains("3 code points"), "{}", warnings[0]);
        assert!(warnings[1].contains("U+002D"), "{}", warnings[1]);
    }

    #[test]
    fn a_clean_report_produces_no_warnings() {
        assert!(warning_lines(&ReadStats::default()).is_empty());
    }

    #[test]
    fn errors_map_to_failed_with_detail() {
        let (summary, detail) = explain_error(&EngineError::UnknownFormat {
            path: PathBuf::from("no-extension-file"),
        });
        assert!(summary.contains("format"));
        assert!(detail.is_some());

        let error = EngineError::UnsupportedPair {
            from: dconv_engine::Format::Docx,
            to: dconv_engine::Format::Docx,
        };
        let (summary, _) = explain_error(&error);
        assert!(summary.contains("not supported"));

        // Cancellation classifies by variant since `PdfError` is re-exported:
        // it must never read as a conversion failure.
        let (summary, _) = explain_error(&EngineError::Reader(PdfError::Cancelled));
        assert!(summary.contains("cancelled"), "{summary}");
    }

    /// Runs only when the development tree has PDFium and fixtures present
    /// (both are gitignored), and exercises the exact call the worker child
    /// makes: one bound `Converter`, one `convert`, in-process.
    #[test]
    fn smoke_pdf_to_docx_when_the_tree_has_fixtures() {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let pdfium = manifest.join("..").join("third_party/pdfium/bin");
        let fixtures = manifest.join("..").join("tests/fixtures/private");

        let mut pdf = None;
        if let Ok(entries) = std::fs::read_dir(&fixtures) {
            let mut names: Vec<PathBuf> = entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|p| {
                    p.extension()
                        .map_or(false, |e| e.eq_ignore_ascii_case("pdf"))
                })
                .collect();
            names.sort();
            pdf = names.into_iter().next();
        }
        let (Some(pdf), true) = (pdf, pdfium.join("pdfium.dll").is_file()) else {
            eprintln!("skipping smoke test: no fixtures or no pdfium.dll in the tree");
            return;
        };

        let out = tempfile::tempdir().unwrap();
        let output = out.path().join("smoke.docx");
        let converter = Converter::bind_from(&pdfium).expect("bind pdfium");

        struct NoSink;
        impl dconv_engine::Sink for NoSink {}

        let report = converter
            .convert(&pdf, &output, &mut NoSink)
            .expect("the primary fixture converts");
        let written = std::fs::metadata(&output).expect("output exists");
        assert!(written.len() > 0);
        assert!(report.pages() > 0);
    }
}
