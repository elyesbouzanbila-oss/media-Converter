//! The wire format between the CLI and its conversion workers.
//!
//! One JSON object per line in each direction, and nothing else on the pipe. A
//! worker's whole interface is its two stdio pipes, so a stray `println!` in
//! worker mode would not merely look untidy — it would corrupt the stream — and
//! that is why worker mode is entered before anything else in `main` and why
//! every human-facing line in this crate goes to stderr.
//!
//! JSON lines rather than a length-prefixed binary frame: the messages are small
//! and rare (a handful per conversion), and being able to read a stuck worker's
//! stream by eye is worth more than the bytes saved.
//!
//! There is no `Cancel`: a worker is one process doing one job at a time, so the
//! parent stops it by killing it. That is also how the GUI's pool cancels, and
//! for the same reason — a half-read page would leave PDFium's iterator in an
//! undefined state.

use dconv_engine::Report;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

/// Parent → worker.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum WorkerIn {
    /// Convert one file. `id` is the job's index in the batch and comes back on
    /// every event for it.
    Job {
        id: u64,
        source: PathBuf,
        output: PathBuf,
    },
    /// Finish the job in hand and exit. Closing stdin does the same thing, so a
    /// worker is never left waiting for a parent that has gone.
    Shutdown,
}

/// Worker → parent.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum WorkerOut {
    /// A page was grouped. Carries `total` as well as `done` so the parent can
    /// report progress without knowing anything about the document.
    Progress { id: u64, done: usize, total: usize },
    /// The job finished. `outcome` is everything the summary line prints, so
    /// the parent never has to look at the file to describe it.
    Finished { id: u64, outcome: JobOutcome },
    /// The job failed, with the message the in-process path would have printed.
    ///
    /// In-band rather than as an exit code: the parent has a job to account for,
    /// and "the child exited 1" does not say which.
    Failed { id: u64, message: String },
    /// The worker could not start at all — PDFium missing, most likely. The
    /// parent stops the batch rather than letting every other worker discover
    /// the same thing one job at a time.
    Fatal { message: String },
}

/// One conversion's measurements, in a form that survives a pipe.
///
/// Deliberately primitive. This is the *only* input the parent has for printing
/// a summary line, and the in-process path prints from the same struct, so the
/// two cannot describe the same conversion differently. Durations are integer
/// microseconds for that reason: a summary that depends on float formatting is
/// a summary that differs between two code paths by a digit, and the whole point
/// of `-j 1` and `-j 8` sharing a renderer is that they do not differ at all.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobOutcome {
    pub pages: usize,
    pub blocks: usize,
    pub read_us: u64,
    pub write_us: u64,
    pub bytes: u64,
    pub synthetic_dropped: usize,
    /// Discarded code points by frequency, for `--verbose`.
    ///
    /// A list of pairs rather than a map, and in sorted order. A JSON object has
    /// string keys, so a map would put the code point on the wire as a string
    /// — and serde reads such a key back as a string, not as the `u32` it has
    /// to be to be printed as `U+0002` (the round-trip test in this file caught
    /// exactly that). A list also pins the order, and these are printed in
    /// order: an unordered map would make two identical runs differ.
    pub dropped_codepoints: Vec<(u32, usize)>,
}

impl JobOutcome {
    /// Take the measurements out of a report, quantised to microseconds.
    ///
    /// The quantisation is not cosmetic: `-j 1` builds its summary here too, so
    /// that removing the sub-microsecond digits removes them for both paths
    /// rather than only for the one that crossed a pipe.
    pub fn of(report: &Report) -> Self {
        Self {
            pages: report.pages(),
            blocks: report.blocks,
            read_us: report.read.as_micros() as u64,
            write_us: report.write.as_micros() as u64,
            bytes: report.output_bytes,
            synthetic_dropped: report.stats.synthetic_dropped,
            dropped_codepoints: report
                .stats
                .dropped_codepoints
                .iter()
                .map(|(codepoint, count)| (*codepoint, *count))
                .collect(),
        }
    }

    pub fn read(&self) -> Duration {
        Duration::from_micros(self.read_us)
    }

    pub fn write(&self) -> Duration {
        Duration::from_micros(self.write_us)
    }

    pub fn total(&self) -> Duration {
        Duration::from_micros(self.read_us + self.write_us)
    }

    pub fn control_dropped(&self) -> usize {
        self.dropped_codepoints.iter().map(|(_, count)| count).sum()
    }
}

/// Keep a bad line short enough to print in an error message.
pub fn excerpt(line: &str) -> String {
    const LIMIT: usize = 160;
    if line.chars().count() <= LIMIT {
        return line.to_owned();
    }
    let head: String = line.chars().take(LIMIT).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_message_stays_on_one_line_and_survives_a_round_trip() {
        // The parent and the child are the same binary, so a mismatch here would
        // not show up as a compile error — it would show up as a worker that
        // waits for input forever. Pinning the encoding is much cheaper than
        // debugging that.
        let incoming = [
            WorkerIn::Job {
                id: 7,
                source: "in.pdf".into(),
                output: "out/out.docx".into(),
            },
            WorkerIn::Shutdown,
        ];
        for message in incoming {
            let text = serde_json::to_string(&message).expect("serialises");
            assert!(!text.contains('\n'), "a message must fit on one line: {text}");
            let back: WorkerIn = serde_json::from_str(&text).expect("reads back");
            assert_eq!(back, message, "via {text}");
        }

        let outgoing = [
            WorkerOut::Progress {
                id: 7,
                done: 2,
                total: 5,
            },
            WorkerOut::Finished {
                id: 7,
                outcome: JobOutcome {
                    pages: 5,
                    blocks: 141,
                    read_us: 26_400,
                    write_us: 1_200,
                    bytes: 34_567,
                    synthetic_dropped: 0,
                    dropped_codepoints: [(0x0002, 1), (0x000A, 178)].into_iter().collect(),
                },
            },
            WorkerOut::Failed {
                id: 7,
                message: "the document has no pages".into(),
            },
            WorkerOut::Fatal {
                message: "PDFium was not found".into(),
            },
        ];
        for message in outgoing {
            let text = serde_json::to_string(&message).expect("serialises");
            assert!(!text.contains('\n'), "a message must fit on one line: {text}");
            let back: WorkerOut = serde_json::from_str(&text).expect("reads back");
            assert_eq!(back, message, "via {text}");
        }
    }

    #[test]
    fn dropped_code_points_keep_their_order_across_the_wire() {
        // They are printed in order, so the encoding has to preserve it — an
        // unordered map would make two identical runs print differently. The
        // code point also has to survive as a number: `U+0002` is formatted from
        // a `u32`, and a JSON object key would come back a string.
        let outcome = JobOutcome {
            pages: 1,
            blocks: 1,
            read_us: 0,
            write_us: 0,
            bytes: 0,
            synthetic_dropped: 0,
            dropped_codepoints: vec![(0x0002, 3), (0x000A, 244), (0x000D, 244)],
        };

        let text = serde_json::to_string(&outcome).expect("serialises");
        let back: JobOutcome = serde_json::from_str(&text).expect("reads back");
        assert_eq!(back, outcome);
        assert_eq!(back.control_dropped(), 491);
        assert_eq!(
            back.dropped_codepoints
                .iter()
                .map(|(codepoint, _)| format!("U+{codepoint:04X}"))
                .collect::<Vec<_>>(),
            vec!["U+0002", "U+000A", "U+000D"]
        );
    }

    #[test]
    fn a_long_bad_line_is_cut_to_something_printable() {
        let line = "x".repeat(500);
        assert_eq!(excerpt(&line).chars().count(), 161);
        assert_eq!(excerpt("short"), "short");
    }
}
