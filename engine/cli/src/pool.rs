//! Parent side of the worker pool: what `dconv -j N` does.
//!
//! Workers are this same executable re-invoked with [`WORKER_FLAG`]. Two
//! reasons, and the second is the one that is easy to overlook:
//!
//! 1. A single-file deliverable stays single-file. The pool costs no second
//!    artifact to build, ship, or keep in step.
//! 2. A worker cannot be a *different build* from the parent that spawned it. A
//!    separate worker binary is a second thing to rebuild, and a stale one
//!    converts documents with old code while the driver reports new numbers.
//!
//! Why processes at all: PDFium's C API is initialised once per process
//! (`dconv-pdf::bind`, finding A1) and a second bind fails, so threads are not
//! available for real parallelism. The GUI's pool is the same shape for the same
//! reason.
//!
//! What this module must not do is change what gets written. A worker calls
//! `Converter::convert` exactly as the in-process path does, and `tests/pool.rs`
//! converts the committed fixtures through both and compares them with each
//! other and with the golden files.

use crate::protocol::{excerpt, JobOutcome, WorkerIn, WorkerOut};
use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender};

/// The flag that turns this executable into one worker.
///
/// Namespaced so it can never collide with a flag worth giving a user, and
/// hidden from `--help`: nobody has a reason to type it — the pool passes it to
/// copies of this binary.
pub const WORKER_FLAG: &str = "--dconv-worker";

/// One file to convert, and where it goes.
pub struct Job {
    /// The job's index in the batch. It travels to the worker and back so a
    /// progress line can be attributed without the parent keeping a second map.
    pub id: u64,
    pub source: PathBuf,
    pub output: PathBuf,
}

/// What a job produced: the numbers for a summary line, or the message the
/// in-process path would have printed for the failure.
pub type Outcome = std::result::Result<JobOutcome, String>;

/// Something the parent learned while the batch ran.
pub enum Update<'a> {
    /// One job's result, in input order and exactly once per job.
    Outcome(usize, &'a Outcome),
    /// Counters moved, for a caller that draws a progress line.
    Progress {
        finished: usize,
        running: usize,
        pages: usize,
    },
}

/// Convert every job through a pool of worker processes.
///
/// Results reach `watch` in **input order**, not completion order. That costs a
/// little latency — a job waits for every earlier job to report — and buys the
/// property that `-j 8` prints exactly what `-j 1` prints, which makes a batch
/// log diffable across runs and makes the parallel path testable by comparing
/// two strings. The wait is bounded by the slowest early job, not by the batch.
pub fn run(
    jobs: &[Job],
    workers: usize,
    pdfium: Option<&Path>,
    watch: &mut dyn FnMut(Update<'_>),
) -> Result<()> {
    let total = jobs.len();
    let mut pool = Pool::spawn(workers.max(1).min(total), pdfium)?;

    let mut pending: VecDeque<usize> = (0..total).collect();
    let mut held: Vec<Option<Outcome>> = (0..total).map(|_| None).collect();
    let mut live: Vec<usize> = vec![0; total]; // pages read by jobs not yet reported

    let mut owed = 0usize; // the next index `watch` is owed
    let mut finished = 0usize;
    let mut done_pages = 0usize;

    while owed < total {
        // Hand work to every idle worker *before* waiting for anything. A pool
        // that idles while the queue is non-empty is the failure this module
        // exists to avoid.
        while let Some(slot) = pool.idle() {
            let Some(index) = pending.pop_front() else { break };
            if let Err(error) = pool.assign(slot, &jobs[index]) {
                // The worker died between jobs. Its job is not lost: it goes
                // back to the front of the queue for a healthy worker.
                pending.push_front(index);
                pool.bury(slot, &error.to_string());
            }
        }

        if pool.exhausted() {
            // Nothing can run again, so nothing may be waited for. Every job
            // still owed gets the reason the pool died, which is the same reason
            // for all of them and the only one worth printing.
            let reason = pool.reason();
            for index in pending.drain(..) {
                held[index] = Some(Err(reason.clone()));
            }
            for index in owed..total {
                if held[index].is_none() {
                    held[index] = Some(Err(reason.clone()));
                }
            }
        } else {
            let (slot, from) = pool.recv()?;
            match from {
                From::Event(WorkerOut::Progress { id, done, .. }) => {
                    if let Some(entry) = batch_index(total, id).and_then(|i| live.get_mut(i)) {
                        *entry = done;
                    }
                    let pages = done_pages + live.iter().sum::<usize>();
                    watch(Update::Progress {
                        finished,
                        running: pool.busy(),
                        pages,
                    });
                }
                From::Event(WorkerOut::Finished { id, outcome }) => {
                    let index = batch_index(total, id).with_context(|| {
                        format!("a worker reported job {id}, which is not in this batch of {total}")
                    })?;
                    live[index] = 0;
                    pool.free(slot);
                    held[index] = Some(Ok(outcome));
                }
                From::Event(WorkerOut::Failed { id, message }) => {
                    let index = batch_index(total, id).with_context(|| {
                        format!("a worker reported job {id}, which is not in this batch of {total}")
                    })?;
                    live[index] = 0;
                    pool.free(slot);
                    held[index] = Some(Err(message));
                }
                From::Event(WorkerOut::Fatal { message }) => {
                    // This worker cannot run at all. Others may still be able
                    // to, so the batch continues; if none can, the block above
                    // ends it with this message.
                    pool.bury(slot, &message);
                }
                From::Gone => {
                    let reason = pool.exit_reason(slot);
                    if let Some(index) = pool.bury(slot, &reason) {
                        held[index] = Some(Err(reason));
                    }
                }
                From::Unreadable(line) => {
                    // Both ends of this pipe are this same binary, so a line
                    // that is not part of the protocol means something printed
                    // to stdout that should have gone to stderr. That is a bug
                    // in the worker, not a condition to carry on from.
                    anyhow::bail!(
                        "a worker wrote {} to stdout, which is not part of the pool protocol; \
                         worker mode has to keep stdout to protocol lines only",
                        excerpt(&line)
                    );
                }
            }
        }

        // Hand results over in input order: a job is reported once every earlier
        // job has been.
        while owed < total {
            let Some(outcome) = held[owed].take() else { break };
            finished += 1;
            if let Ok(each) = &outcome {
                done_pages += each.pages;
                live[owed] = 0;
            }
            watch(Update::Outcome(owed, &outcome));
            owed += 1;
        }
    }

    pool.shutdown();
    Ok(())
}

/// The batch slot an event names, if the batch has such a slot.
fn batch_index(total: usize, id: u64) -> Option<usize> {
    let index = usize::try_from(id).ok()?;
    (index < total).then_some(index)
}

/// A message from one worker's stream, on its way to the parent's loop.
enum From {
    Event(WorkerOut),
    /// The worker's output ended: it exited, was killed, or crashed.
    Gone,
    /// A line that is not part of the protocol, kept whole so the error can say
    /// what it was.
    Unreadable(String),
}

struct Pool {
    slots: Vec<Slot>,
    from: Receiver<(usize, From)>,
    /// Why the most recently stopped worker stopped. When the whole pool is
    /// gone, this is also why the batch stopped.
    last: Option<String>,
}

impl Pool {
    fn spawn(count: usize, pdfium: Option<&Path>) -> Result<Self> {
        let (to, from) = channel();
        let mut slots = Vec::with_capacity(count);
        for index in 0..count {
            slots.push(Slot::spawn(index, pdfium, to.clone())?);
        }
        // The pool must not keep a sender of its own: `recv` has to be able to
        // see the channel close when the last worker's stream ends.
        drop(to);
        Ok(Self {
            slots,
            from,
            last: None,
        })
    }

    fn recv(&self) -> Result<(usize, From)> {
        self.from
            .recv()
            .context("every worker's output stream closed while jobs were outstanding")
    }

    fn idle(&self) -> Option<usize> {
        self.slots
            .iter()
            .position(|slot| !slot.dead && slot.busy.is_none())
    }

    fn busy(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| slot.busy.is_some())
            .count()
    }

    fn assign(&mut self, slot: usize, job: &Job) -> Result<()> {
        let message = WorkerIn::Job {
            id: job.id,
            source: job.source.clone(),
            output: job.output.clone(),
        };
        self.slots[slot].send(&message)?;
        self.slots[slot].busy = Some(job.id as usize);
        Ok(())
    }

    fn free(&mut self, slot: usize) {
        self.slots[slot].busy = None;
    }

    /// Write a worker off, and hand back the job it was holding, if any.
    fn bury(&mut self, slot: usize, reason: &str) -> Option<usize> {
        let held = std::mem::take(&mut self.slots[slot].busy);
        self.slots[slot].dead = true;
        self.last = Some(reason.to_owned());
        held
    }

    fn exhausted(&self) -> bool {
        self.slots.iter().all(|slot| slot.dead)
    }

    fn reason(&self) -> String {
        self.last
            .clone()
            .unwrap_or_else(|| "every worker stopped".to_owned())
    }

    /// Why a worker's stream ended, in words somebody can act on.
    fn exit_reason(&mut self, slot: usize) -> String {
        let described = match self.slots[slot].child.try_wait() {
            Ok(Some(status)) => format!("exited with {status}"),
            Ok(None) => "closed its output while still running".to_owned(),
            Err(error) => format!("could not be checked: {error}"),
        };
        format!("the worker process {described} without reporting its job")
    }

    /// Ask the workers to finish, and wait for them.
    ///
    /// Only correct with nothing in flight, because a worker reads `Shutdown`
    /// between jobs. `run` calls it exactly there.
    fn shutdown(&mut self) {
        for slot in &mut self.slots {
            slot.stop();
        }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        // An early return — a fatal worker, a broken protocol, a failed spawn —
        // must not leave workers running. `kill` rather than `stop`: a worker
        // mid-document would not read `Shutdown` until it finished, and the
        // whole point of getting here is that we are not waiting for that.
        for slot in &mut self.slots {
            if !slot.dead {
                slot.kill();
            }
        }
    }
}

struct Slot {
    child: Child,
    stdin: Option<ChildStdin>,
    /// The batch index of the job this worker is running, if any.
    busy: Option<usize>,
    dead: bool,
}

impl Slot {
    fn spawn(index: usize, pdfium: Option<&Path>, to: Sender<(usize, From)>) -> Result<Self> {
        let exe = std::env::current_exe()
            .context("locating this executable, to re-run it as a worker")?;

        let mut command = Command::new(&exe);
        command.arg(WORKER_FLAG);
        // The parent already located a working PDFium by binding before it got
        // here, so the worker is told where it is rather than searching again
        // and possibly finding something else.
        if let Some(directory) = pdfium {
            command.arg("--pdfium").arg(directory);
        }

        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Inherited on purpose: a worker that panics should say so on the
            // terminal a person is watching, rather than into a pipe nobody
            // reads. Worker mode prints nothing else — what it has to say goes
            // over the protocol.
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("starting a worker from {}", exe.display()))?;

        let stdin = child
            .stdin
            .take()
            .context("the worker's stdin was not piped")?;
        let stdout = child
            .stdout
            .take()
            .context("the worker's stdout was not piped")?;
        read_events(index, stdout, to);

        Ok(Self {
            child,
            stdin: Some(stdin),
            busy: None,
            dead: false,
        })
    }

    /// Hand this worker one job, as one protocol line.
    fn send(&mut self, message: &WorkerIn) -> Result<()> {
        let stdin = self
            .stdin
            .as_mut()
            .context("the worker is no longer accepting jobs")?;
        serde_json::to_writer(&mut *stdin, message).context("encoding a job")?;
        stdin.write_all(b"\n").context("writing a job")?;
        stdin.flush().context("writing a job")
    }

    /// Ask this worker to exit, and wait for it.
    fn stop(&mut self) {
        if self.dead {
            return;
        }
        let _ = self.send(&WorkerIn::Shutdown);
        // Closing the pipe is the belt to `Shutdown`'s braces: a worker that
        // somehow missed the message still sees EOF rather than waiting for a
        // parent that has stopped listening.
        self.stdin = None;
        let _ = self.child.wait();
        self.dead = true;
    }

    /// Stop this worker without asking.
    fn kill(&mut self) {
        self.stdin = None;
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.dead = true;
    }
}

/// Read one worker's event stream, on its own thread.
///
/// A thread per worker rather than a poll in the parent: the parent's job is to
/// keep every worker busy, and it cannot do that while blocked on one of them.
fn read_events(index: usize, stdout: ChildStdout, to: Sender<(usize, From)>) {
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<WorkerOut>(&line) {
                Ok(event) => {
                    if to.send((index, From::Event(event))).is_err() {
                        return;
                    }
                }
                Err(_) => {
                    // Stop reading: once one line is not a protocol line, the
                    // stream is out of step and nothing after it can be trusted.
                    let _ = to.send((index, From::Unreadable(line)));
                    return;
                }
            }
        }
        let _ = to.send((index, From::Gone));
    });
}
