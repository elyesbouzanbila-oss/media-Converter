//! `dconv` — the command line front end.
//!
//! Deliberately thin. Everything that decides *what* a conversion produces lives
//! in `dconv-engine` so that the GUI cannot drift from it; this crate only
//! parses arguments, resolves output paths, prints progress, and adds up
//! failures. If a change here would alter the output file, it is in the wrong
//! crate.

use anyhow::{bail, Context, Result};
use clap::Parser;
use dconv_engine::{Converter, Format, Report, Sink};
use dconv_engine::ir;
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
    #[arg(required = true, value_name = "INPUT")]
    inputs: Vec<PathBuf>,

    /// Output file, or output directory when converting several files.
    #[arg(short, long, value_name = "PATH")]
    output: Option<PathBuf>,

    /// Directory holding pdfium.dll, overriding the built-in search.
    #[arg(long, value_name = "DIR")]
    pdfium: Option<PathBuf>,

    /// Show per-page progress while reading.
    #[arg(short, long)]
    verbose: bool,

    /// Print the analysed block structure instead of converting.
    #[arg(long)]
    dump: bool,

    /// Print only errors.
    #[arg(short, long, conflicts_with = "verbose")]
    quiet: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let started = Instant::now();

    // Refuse the ambiguous case before doing any work: with several inputs and
    // an --output that names a file, every conversion after the first would
    // silently overwrite the one before it.
    if args.inputs.len() > 1 {
        if let Some(output) = &args.output {
            if !looks_like_a_directory(output) {
                bail!(
                    "--output {} names a file, but {} inputs were given; \
                     pass a directory instead",
                    output.display(),
                    args.inputs.len()
                );
            }
        }
    }

    let converter = match &args.pdfium {
        Some(directory) => Converter::bind_from(directory),
        None => Converter::bind(),
    }
    .context("starting the PDF engine")?;

    let mut reports = Vec::new();
    let mut failures = 0usize;

    for input in &args.inputs {
        let output = dconv_engine::output_path(input, args.output.as_deref(), Format::Docx);

        if let Some(parent) = output.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
        }

        let mut progress = PageProgress {
            // Per-page progress is opt-in: a one-line summary per file is the
            // useful default, and a reused progress line is noise in a log.
            quiet: args.quiet || !args.verbose,
            label: file_name(input),
            rendered: false,
        };

        if args.dump {
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
            continue;
        }

        match converter.convert(input, &output, &mut progress) {
            Ok(report) => {
                progress.finish();
                if !args.quiet {
                    println!("{}", describe(&report, args.verbose));
                }
                reports.push(report);
            }
            Err(error) => {
                progress.finish();
                failures += 1;
                eprintln!("{}: {error}", input.display());
            }
        }
    }

    if !args.quiet && reports.len() > 1 {
        let pages: usize = reports.iter().map(|r| r.pages()).sum();
        let bytes: u64 = reports.iter().map(|r| r.output_bytes).sum();
        println!(
            "\n{} files, {pages} pages, {} in {:.2} s",
            reports.len(),
            human_bytes(bytes),
            started.elapsed().as_secs_f64()
        );
    }

    if failures > 0 {
        bail!("{failures} of {} conversions failed", args.inputs.len());
    }

    Ok(())
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

fn describe(report: &Report, verbose: bool) -> String {
    let stats = &report.stats;
    let mut out = format!(
        "{} -> {}\n  {} pages, {} blocks, {} (read {}, write {}), {}",
        report.input.display(),
        report.output.display(),
        stats.pages,
        report.blocks,
        human_duration(report.total()),
        human_duration(report.read),
        human_duration(report.write),
        human_bytes(report.output_bytes),
    );

    // Only shown when non-zero: a converter that silently deletes characters
    // should say so, and a converter that deletes none should not clutter its
    // output with a row of zeroes.
    if stats.synthetic_dropped > 0 || stats.control_dropped() > 0 {
        out.push_str(&format!(
            "\n  discarded {} synthetic separators, {} code points",
            stats.synthetic_dropped,
            stats.control_dropped(),
        ));
    }

    if verbose && !stats.dropped_codepoints.is_empty() {
        for (codepoint, count) in &stats.dropped_codepoints {
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
