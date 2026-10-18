//! The child side: bind PDFium once, convert what arrives on stdin, report on
//! stdout.
//!
//! Nothing here decides anything about a conversion — it calls the same
//! `Converter::convert` the in-process path calls, with the same `Sink`
//! discipline, so the bytes it writes cannot differ from the bytes `-j 1`
//! writes. `tests/pool.rs` converts the committed fixtures through both and
//! compares.

use crate::protocol::{JobOutcome, WorkerIn, WorkerOut};
use dconv_engine::{Converter, Sink};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

/// Run as one conversion worker until the parent shuts it down or closes the
/// pipe.
///
/// Returns the exit code rather than an error: a job's failure is reported
/// in-band, on the protocol, because the parent has a job to account for and
/// "the child exited 1" does not say which one. The exit code only has to say
/// whether the worker itself could run.
pub fn run(pdfium: Option<&Path>) -> i32 {
    let converter = match pdfium {
        Some(directory) => Converter::bind_from(directory),
        None => Converter::bind(),
    };
    let converter = match converter {
        Ok(converter) => converter,
        // Fatal, not Failed: no job exists yet, and the parent should stop the
        // whole batch rather than let every other worker rediscover this once
        // per file.
        Err(error) => {
            let mut out = std::io::stdout().lock();
            return fatal(&mut out, &error.to_string());
        }
    };

    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();

    for line in BufReader::new(stdin.lock()).lines() {
        let line = match line {
            Ok(line) => line,
            Err(error) => return fatal(&mut out, &format!("reading a job: {error}")),
        };
        if line.trim().is_empty() {
            continue;
        }

        let (id, source, output) = match serde_json::from_str::<WorkerIn>(&line) {
            Ok(WorkerIn::Job { id, source, output }) => (id, source, output),
            Ok(WorkerIn::Shutdown) => return 0,
            Err(error) => {
                return fatal(&mut out, &format!("a job line could not be read: {error}"));
            }
        };

        let (outcome, abandoned) = {
            let mut progress = Progress {
                out: &mut out,
                id,
                abandoned: false,
            };
            let outcome = converter.convert(&source, &output, &mut progress);
            (outcome, progress.abandoned)
        };

        if abandoned {
            // The parent stopped reading. Nobody is waiting for this document,
            // so there is no reason to finish it.
            return 0;
        }

        let event = match outcome {
            Ok(report) => WorkerOut::Finished {
                id,
                outcome: JobOutcome::of(&report),
            },
            Err(error) => WorkerOut::Failed {
                id,
                message: error.to_string(),
            },
        };
        if !deliver(&mut out, &event) {
            return 0;
        }
    }

    // Stdin closed: the parent went away, or finished with us. Either way this
    // is the normal way for a worker to end, and never a reason to complain.
    0
}

/// Per-page progress, sent to the parent rather than drawn.
///
/// In worker mode the terminal belongs to the parent — and the in-process path
/// skips single-page documents, which this mirrors so that the two cannot
/// report different numbers of events.
struct Progress<'a, W: Write> {
    out: &'a mut W,
    id: u64,
    abandoned: bool,
}

impl<W: Write> Sink for Progress<'_, W> {
    fn page(&mut self, done: usize, total: usize) {
        if self.abandoned || total <= 1 {
            return;
        }
        if !deliver(
            self.out,
            &WorkerOut::Progress {
                id: self.id,
                done,
                total,
            },
        ) {
            self.abandoned = true;
        }
    }

    fn cancelled(&self) -> bool {
        // Consulted between pages, which is the only place the reader is willing
        // to stop: a parent that has gone should stop the work, not merely stop
        // hearing about it.
        self.abandoned
    }
}

/// Write one protocol line, and say whether the parent received it.
///
/// Flushed per event rather than buffered: the parent reads these to decide what
/// to do next, so an event sitting in a buffer is an event that did not happen.
/// A false return means the pipe is gone — the parent was killed, most likely —
/// which the caller treats as "stop", because a worker with no parent is a
/// process nobody can see.
fn deliver(out: &mut impl Write, event: &WorkerOut) -> bool {
    serde_json::to_writer(&mut *out, event).is_ok()
        && out.write_all(b"\n").is_ok()
        && out.flush().is_ok()
}

/// Report a failure that stops this worker, and return its exit code.
fn fatal(out: &mut impl Write, message: &str) -> i32 {
    let _ = deliver(
        out,
        &WorkerOut::Fatal {
            message: message.to_owned(),
        },
    );
    1
}
