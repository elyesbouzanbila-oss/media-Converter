//! The boundary between the UI and whatever actually converts documents.
//!
//! M0 ships a demo engine so the whole front end can be built, polished and
//! reviewed before `dconv-engine` lands. Swapping in the real engine (M1) is a
//! one-line change: provide `spawn_real_engine` implementing the same
//! `EngineCommand` → `EngineEvent` protocol, and nothing else in the UI moves.
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
//! the channel) so the real adapter can be dropped in without re-shaping the
//! UI's threading assumptions.

use crate::state::{JobId, TargetFormat};
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
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
    #[allow(dead_code)] // used by the real engine, not by the demo
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
    Started { id: JobId },
    Progress { id: JobId, frac: f32 },
    Warning { id: JobId, text: String },
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
    Cancelled { id: JobId },
}

/// The pair of channel ends the app holds for one engine instance.
pub struct EngineChannels {
    pub commands: Sender<EngineCommand>,
    pub events: Receiver<EngineEvent>,
}

// ---------------------------------------------------------------------------
// Demo engine
// ---------------------------------------------------------------------------

/// Start the demo engine: a worker pool over a shared queue, exactly the shape
/// the real facade will have, except the "conversion" is a sleep.
///
/// Demo sensitivities, so every code path of the UI is exercisable without a
/// real converter: a file name containing `fail` is reported as failed, one
/// containing `warn` produces warnings, and the runtime of each job is derived
/// from its name so batches are reproducible.
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
