//! `dconv` — the command line front end.
//!
//! Deliberately thin. Everything that decides *what* a conversion produces lives
//! in `dconv-engine` so that the GUI cannot drift from it; this crate only
//! parses arguments, resolves output paths, prints progress, and adds up
//! failures. If a change here would alter the output file, it is in the wrong
//! crate.
//!
//! With `-j`, this crate is also a pool: the same executable re-runs itself as a
//! worker process, because PDFium can only be initialised once per process
//! (finding A1). Worker mode changes nothing about the conversion — it calls the
//! same facade the in-process path calls, which `tests/pool.rs` checks by
//! converting the committed fixtures both ways and comparing the bytes.

mod pool;
mod protocol;
mod worker;

use anyhow::{bail, Context, Result};
use clap::Parser;
use dconv_engine::ir;
use dconv_engine::{Converter, Format, Sink};
use protocol::JobOutcome;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Parser, Debug)]
#[command(
    name = "dconv",
    version,
    about = "Convert documents between formats",
    long_about = "Convert documents between formats.\n\n\
        With no --output, each result is written beside its source. With \
        --output pointing at a directory, results keep their source names."
)]
struct Args {
    /// Files to convert.
    #[arg(required_unless_present = "dconv_worker", value_name = "INPUT")]
    inputs: Vec<PathBuf>,

    /// Output file, or output directory when converting several files.
    #[arg(short, long, value_name = "PATH")]
    output: Option<PathBuf>,

    /// Convert this many files at once, each in a worker process.
    ///
    /// The default of 1 converts in this process, which is what a single
    /// document wants; a pool costs a process per worker and reorders nothing
    /// else. 0 asks for one worker per core, up to 8, and never more workers
    /// than there are files.
    #[arg(short = 'j', long, value_name = "N", default_value_t = 1)]
    jobs: usize,

    /// Directory holding pdfium.dll, overriding the built-in search.
    #[arg(long, value_name = "DIR")]
    pdfium: Option<PathBuf>,

    /// Show per-page progress while reading.
    #[arg(short, long)]
    verbose: bool,

    /// Print the analysed block structure instead of converting. Never uses
    /// worker processes.
    #[arg(long)]
    dump: bool,

    /// Print only errors.
    #[arg(short, long, conflicts_with = "verbose")]
    quiet: bool,

    /// Internal: run as one conversion worker, speaking the pool protocol on
    /// stdin and stdout. The pool re-runs this same executable with this flag;
    /// there is no reason to type it.
    #[arg(long, hide = true)]
    dconv_worker: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();

    // Worker mode first, before anything could print: in worker mode stdout
    // belongs to the protocol, so a stray line here would corrupt the stream
    // rather than look untidy.
    if args.dconv_worker {
        std::process::exit(worker::run(args.pdfium.as_deref()));
    }

    let started = Instant::now();
    check_output_target(&args)?;

    // Bound once here even when the pool will do the work, because it is also
    // the pre-flight check: one "PDFium is missing" is worth more than one per
    // worker, and it is the same search the workers will run.
    let converter = match &args.pdfium {
        Some(directory) => Converter::bind_from(directory),
        None => Converter::bind(),
    }
    .context("starting the PDF engine")?;

    if args.dump {
        return dump_all(&converter, &args);
    }

    let batch: Vec<pool::Job> = args
        .inputs
        .iter()
        .enumerate()
        .map(|(index, input)| pool::Job {
            id: index as u64,
            source: input.clone(),
            output: dconv_engine::output_path(input, args.output.as_deref(), Format::Docx),
        })
        .collect();

    for job in &batch {
        make_room_for(&job.output)?;
    }

    let mut watch = Watch::new(&batch, args.verbose, args.quiet);
    let workers = workers(args.jobs);

    if workers > 1 && batch.len() > 1 {
        pool::run(&batch, workers, args.pdfium.as_deref(), &mut |update| {
            watch.update(update)
        })?;
    } else {
        run_in_process(&converter, &batch, &args, &mut watch);
    }
    watch.finish();

    if !args.quiet && watch.converted > 1 {
        println!(
            "\n{} files, {} pages, {} in {:.2} s",
            watch.converted,
            watch.pages,
            human_bytes(watch.bytes),
            started.elapsed().as_secs_f64()
        );
    }

    if watch.failed > 0 {
        bail!("{} of {} conversions failed", watch.failed, batch.len());
    }

    Ok(())
}

/// How many worker processes to use.
///
/// 0 means "as many as this machine has cores", capped at 8: Spike C measured
/// throughput saturating near 20 documents/s by 8 workers, with 8→32 buying
/// about 10% for four times the processes.
fn workers(requested: usize) -> usize {
    if requested != 0 {
        return requested.max(1);
    }
    let cores = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1);
    cores.clamp(1, 8)
}

/// Refuse the ambiguous case before doing any work: with several inputs and an
/// `--output` that names a file, every conversion after the first would silently
/// overwrite the one before it.
fn check_output_target(args: &Args) -> Result<()> {
    if args.inputs.len() > 1
        && let Some(output) = &args.output
        && !looks_like_a_directory(output)
    {
        bail!(
            "--output {} names a file, but {} inputs were given; \
             pass a directory instead",
            output.display(),
            args.inputs.len()
        );
    }
    Ok(())
}

/// Create the output's directory, so that a missing folder is discovered once up
/// front rather than in the middle of a batch.
fn make_room_for(output: &Path) -> Result<()> {
    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
    }
    Ok(())
}

/// Convert the batch in this process, one file at a time.
fn run_in_process(
    converter: &Converter,
    batch: &[pool::Job],
    args: &Args,
    watch: &mut Watch<'_>,
) {
    for (index, job) in batch.iter().enumerate() {
        let mut progress = PageProgress {
            // Per-page progress is opt-in: a one-line summary per file is the
            // useful default, and a reused progress line is noise in a log.
            quiet: args.quiet || !args.verbose,
            label: file_name(&job.source),
            rendered: false,
        };

        let outcome = match converter.convert(&job.source, &job.output, &mut progress) {
            Ok(report) => Ok(JobOutcome::of(&report)),
            Err(error) => Err(error.to_string()),
        };
        progress.finish();
        watch.update(pool::Update::Outcome(index, &outcome));
    }
}

/// Print the analysed block structure of every input.
fn dump_all(converter: &Converter, args: &Args) -> Result<()> {
    let mut failures = 0usize;

    for input in &args.inputs {
        let mut progress = PageProgress {
            quiet: args.quiet || !args.verbose,
            label: file_name(input),
            rendered: false,
        };

        match converter.analyse(input, &mut progress) {
            Ok(document) => {
                progress.finish();
                println!("=== {}", input.display());
                let mut text = String::new();
                dump(&document, &mut text);
                print!("{text}");
            }
            Err(error) => {
                progress.finish();
                failures += 1;
                eprintln!("{}: {error}", input.display());
            }
        }
    }

    if failures > 0 {
        bail!("{failures} of {} conversions failed", args.inputs.len());
    }

    Ok(())
}

/// Prints the batch as it runs, and adds it up.
///
/// One struct for both the in-process and the pooled path, for the same reason
/// `describe` takes a [`JobOutcome`] rather than a `Report`: "what a conversion
/// run prints" should be defined once, or the two paths will answer differently.
struct Watch<'a> {
    batch: &'a [pool::Job],
    verbose: bool,
    quiet: bool,
    converted: usize,
    failed: usize,
    pages: usize,
    bytes: u64,
    /// A progress line is on the terminal and needs clearing.
    rendered: bool,
}

impl<'a> Watch<'a> {
    fn new(batch: &'a [pool::Job], verbose: bool, quiet: bool) -> Self {
        Self {
            batch,
            verbose,
            quiet,
            converted: 0,
            failed: 0,
            pages: 0,
            bytes: 0,
            rendered: false,
        }
    }

    fn update(&mut self, update: pool::Update<'_>) {
        match update {
            pool::Update::Outcome(index, outcome) => self.outcome(index, outcome),
            pool::Update::Progress {
                finished,
                running,
                pages,
            } => self.progress(finished, running, pages),
        }
    }

    fn outcome(&mut self, index: usize, outcome: &pool::Outcome) {
        self.finish();
        let job = &self.batch[index];
        match outcome {
            Ok(each) => {
                self.converted += 1;
                self.pages += each.pages;
                self.bytes += each.bytes;
                if !self.quiet {
                    println!("{}", describe(&job.source, &job.output, each, self.verbose));
                }
            }
            Err(message) => {
                self.failed += 1;
                eprintln!("{}: {message}", job.source.display());
            }
        }
    }

    /// The aggregate line for a batch that cannot have a per-file one: with
    /// several workers running there is no single document to be on page 3 of.
    fn progress(&mut self, finished: usize, running: usize, pages: usize) {
        if self.quiet || !self.verbose || self.batch.len() <= 1 {
            return;
        }
        eprint!(
            "\r  {finished}/{} files, {running} running, {pages} pages",
            self.batch.len()
        );
        self.rendered = true;
    }

    fn finish(&mut self) {
        if self.rendered {
            eprintln!();
            self.rendered = false;
        }
    }
}

/// Per-page progress on one reused line, so a long document does not scroll the
/// terminal away.
struct PageProgress {
    quiet: bool,
    label: String,
    rendered: bool,
}

impl PageProgress {
    fn finish(&mut self) {
        if self.rendered {
            eprintln!();
            self.rendered = false;
        }
    }
}

impl Sink for PageProgress {
    fn page(&mut self, done: usize, total: usize) {
        if self.quiet || total <= 1 {
            return;
        }
        eprint!("\r  {:<32} page {done}/{total}", self.label);
        self.rendered = true;
    }
}

fn describe(input: &Path, output: &Path, outcome: &JobOutcome, verbose: bool) -> String {
    let mut out = format!(
        "{} -> {}\n  {} pages, {} blocks, {} (read {}, write {}), {}",
        input.display(),
        output.display(),
        outcome.pages,
        outcome.blocks,
        human_duration(outcome.total()),
        human_duration(outcome.read()),
        human_duration(outcome.write()),
        human_bytes(outcome.bytes),
    );

    // Only shown when non-zero: a converter that silently deletes characters
    // should say so, and a converter that deletes none should not clutter its
    // output with a row of zeroes.
    if outcome.synthetic_dropped > 0 || outcome.control_dropped() > 0 {
        out.push_str(&format!(
            "\n  discarded {} synthetic separators, {} code points",
            outcome.synthetic_dropped,
            outcome.control_dropped(),
        ));
    }

    if verbose && !outcome.dropped_codepoints.is_empty() {
        for (codepoint, count) in &outcome.dropped_codepoints {
            out.push_str(&format!("\n    U+{codepoint:04X} x{count}"));
        }
    }

    out
}

/// Print the block structure, one line per block.
///
/// Not JSON: the point of this output is to be read while comparing it against
/// what the page actually looks like, and a line per block keeps that comparison
/// possible without a viewer.
fn dump(document: &ir::Document, out: &mut String) {
    out.push_str(&format!(
        "  page {:.1} x {:.1} pt, margins {:.1}/{:.1}/{:.1}/{:.1}\n",
        document.page.width.as_pt(),
        document.page.height.as_pt(),
        document.page.margin_top.as_pt(),
        document.page.margin_bottom.as_pt(),
        document.page.margin_start.as_pt(),
        document.page.margin_end.as_pt(),
    ));
    for block in &document.blocks {
        dump_block(block, 1, out);
    }
}

fn dump_block(block: &ir::Block, depth: usize, out: &mut String) {
    let pad = "  ".repeat(depth);
    let text = |content: &[ir::Inline]| {
        let text = ir::inlines_to_plain_text(content);
        if text.chars().count() > 96 {
            format!("{}…", text.chars().take(96).collect::<String>())
        } else {
            text
        }
    };

    match block {
        ir::Block::Heading { level, content, .. } => {
            out.push_str(&format!("{pad}H{level}  {}\n", text(content)));
        }
        ir::Block::Paragraph { content, align, .. } => {
            out.push_str(&format!("{pad}P[{align:?}]  {}\n", text(content)));
        }
        ir::Block::List(list) => {
            let kind = if list.ordered { "ol" } else { "ul" };
            let start = list.start.unwrap_or(1);
            out.push_str(&format!(
                "{pad}{kind} start={start} numbering={:?}\n",
                list.numbering
            ));
            for item in &list.items {
                out.push_str(&format!("{pad}  · \n"));
                for inner in &item.blocks {
                    dump_block(inner, depth + 2, out);
                }
            }
        }
        ir::Block::Table(table) => {
            out.push_str(&format!(
                "{pad}TABLE {}x{}\n",
                table.rows.len(),
                table.column_count()
            ));
            for (index, row) in table.rows.iter().enumerate() {
                let cells: Vec<String> = row
                    .cells
                    .iter()
                    .map(|cell| {
                        cell.blocks
                            .iter()
                            .filter_map(|block| match block {
                                ir::Block::Paragraph { content, .. } => Some(text(content)),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .collect();
                let header = if row.cells.iter().any(|cell| cell.is_header) {
                    " header"
                } else {
                    ""
                };
                out.push_str(&format!("{pad}  r{index}{header}  | {} |\n", cells.join(" | ")));
            }
        }
        ir::Block::Quote { blocks } => {
            out.push_str(&format!("{pad}QUOTE\n"));
            for inner in blocks {
                dump_block(inner, depth + 1, out);
            }
        }
        ir::Block::CodeBlock { language, text, .. } => {
            let language = language.as_deref().unwrap_or("plain");
            out.push_str(&format!("{pad}CODE[{language}] {} chars\n", text.chars().count()));
        }
        ir::Block::Image(image) => {
            out.push_str(&format!(
                "{pad}IMAGE alt={:?} {:?}\n",
                image.alt.as_deref().unwrap_or(""),
                image.source
            ));
        }
        ir::Block::Caption { content } => {
            out.push_str(&format!("{pad}CAPTION  {}\n", text(content)));
        }
        ir::Block::Rule => out.push_str(&format!("{pad}RULE\n")),
        ir::Block::PageBreak => out.push_str(&format!("{pad}--- page break ---\n")),
    }
}

fn human_duration(duration: Duration) -> String {
    let millis = duration.as_secs_f64() * 1000.0;
    if millis < 1.0 {
        format!("{:.0} µs", millis * 1000.0)
    } else if millis < 1000.0 {
        format!("{millis:.0} ms")
    } else {
        format!("{:.2} s", duration.as_secs_f64())
    }
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn looks_like_a_directory(path: &Path) -> bool {
    path.is_dir() || path.to_string_lossy().ends_with(['/', '\\'])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asking_for_zero_workers_asks_for_the_machine_but_not_for_a_fleet() {
        // Spike C: 8 -> 32 workers bought about 10% for four times the
        // processes, so the automatic answer stops at 8.
        let cores = std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(1);
        assert_eq!(workers(0), cores.clamp(1, 8));
        assert!(workers(0) >= 1, "never zero: -j 0 still has to convert");
    }

    #[test]
    fn an_explicit_worker_count_is_taken_as_given() {
        assert_eq!(workers(1), 1);
        assert_eq!(workers(3), 3);
        assert_eq!(workers(64), 64);
        // `-j 0` is the only "no argument" spelling; a count of zero does not
        // exist as a meaning, so it cannot mean "no workers".
        assert_eq!(workers(0).min(1), 1);
    }

    #[test]
    fn a_batch_of_several_files_may_not_be_given_a_single_file_target() {
        let args = Args {
            inputs: vec!["a.pdf".into(), "b.pdf".into()],
            output: Some("out.docx".into()),
            jobs: 1,
            pdfium: None,
            verbose: false,
            dump: false,
            quiet: false,
            dconv_worker: false,
        };
        let error = check_output_target(&args).expect_err("two files cannot share one name");
        assert!(error.to_string().contains("pass a directory"), "{error}");
    }

    #[test]
    fn a_directory_target_is_fine_for_several_files() {
        let args = Args {
            inputs: vec!["a.pdf".into(), "b.pdf".into()],
            output: Some("out/".into()),
            jobs: 1,
            pdfium: None,
            verbose: false,
            dump: false,
            quiet: false,
            dconv_worker: false,
        };
        assert!(check_output_target(&args).is_ok());
    }
}
