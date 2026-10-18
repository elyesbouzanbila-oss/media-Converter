//! Phase 0 spike A — PDFium positioned-text extraction.
//!
//! Answers three questions before we commit to an architecture:
//!
//!   1. Can we get per-glyph geometry out of PDFium fast enough?
//!      Target: a 100-page text document in under one second, single thread.
//!   2. Is the geometry good enough to reconstruct lines and paragraphs?
//!      There is no point building a layout analyser on coordinates that are
//!      not stable, so this prints reconstructed lines for eyeballing.
//!   3. What does style recovery actually look like for base-14 fonts?
//!      We expect PDFium's reported font weights to be unreliable there, and
//!      to have to fall back to name matching.
//!
//! Run:
//!   cargo run -p spike-pdf-extract --release -- tests/fixtures/generated/perf.pdf
//!   cargo run -p spike-pdf-extract --release -- tests/fixtures/generated/table.pdf --dump 1
//!
//! Set PDFIUM_DLL_DIR to override where pdfium.dll is loaded from.

use anyhow::{anyhow, Context, Result};
use pdfium_render::prelude::*;
use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// One positioned character, normalised into the fields the layout analyser
/// will actually consume.
#[derive(Debug, Clone)]
struct Glyph {
    ch: char,
    /// Left edge of the glyph's *advance* box, in PDF points from the page's
    /// left edge.
    ///
    /// This is deliberately the loose box, not the ink box. Gap-based space
    /// inference compares adjacent glyph edges, and ink boxes are too
    /// shape-dependent for that: the ink box of `.` or `i` starts well inside
    /// its advance box, so an ink-based comparison invents spaces before
    /// punctuation and around narrow letters. Advance boxes are what a
    /// typesetting engine advances the pen by, which is the quantity we
    /// actually want.
    x: f32,
    /// Right edge of the advance box. Kept alongside `x` because inter-glyph
    /// gaps are what let us decide whether a space was elided between words.
    right: f32,
    /// Ink-only extents. Unused for grouping; retained because underline and
    /// strikeout detection genuinely does need glyph ink, not advance widths.
    ink_left: f32,
    ink_right: f32,
    /// Bottom edge. PDF coordinates grow upwards, so larger `bottom` means
    /// higher on the page.
    bottom: f32,
    top: f32,
    /// Font size in points, unscaled by any transform matrix.
    size: f32,
    font: String,
    bold: bool,
    italic: bool,
    /// True when PDFium synthesised this character rather than reading it from
    /// the content stream. Producers that position each word individually emit
    /// no space glyphs at all, and PDFium fills them in.
    generated: bool,
}

/// A text line reconstructed from glyph geometry.
#[derive(Debug, Clone)]
struct Line {
    text: String,
    bottom: f32,
    top: f32,
    /// Dominant size on the line, taken as the maximum. Mixed-size lines are
    /// rare and are almost always a heading with a stray superscript.
    size: f32,
    bold: bool,
    italic: bool,
    fonts: Vec<String>,
}

/// How spaces got into the reconstructed text.
///
/// Worth measuring precisely: a high `inferred` count means the producer
/// positioned words individually and we are reconstructing word boundaries, so
/// our gap heuristic is load-bearing for correctness. A high `pdfium` count
/// means PDFium already did that work and our own inference rarely fires.
#[derive(Debug, Default, Clone, Copy)]
struct SpaceStats {
    /// Real space characters present in the content stream.
    literal: usize,
    /// Spaces synthesized by PDFium.
    pdfium: usize,
    /// Spaces we synthesized ourselves from glyph gaps.
    inferred: usize,
}

#[derive(Debug)]
struct Report {
    pages: usize,
    glyphs: usize,
    lines: usize,
    load: Duration,
    extract: Duration,
    spaces: SpaceStats,
    /// Synthetic characters PDFium inserted that we discarded as having no
    /// usable geometry.
    synthetic_dropped: usize,
}

fn main() -> Result<()> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() || argv[0].starts_with("--") {
        eprintln!("usage: spike-pdf-extract <file.pdf> [--dump N] [--repeat N]");
        eprintln!();
        eprintln!("  --dump N     print the first N pages of reconstructed lines");
        eprintln!("  --raw N      dump the first N raw glyphs of page 1, with codepoints");
        eprintln!("  --repeat N   run the extraction N times and report each run");
        std::process::exit(2);
    }

    let path = PathBuf::from(&argv[0]);
    let dump = flag(&argv, "--dump").unwrap_or(0);
    let raw = flag(&argv, "--raw").unwrap_or(0);
    let repeat = flag(&argv, "--repeat").unwrap_or(1).max(1);

    let lib_dir = dll_dir();
    let lib = Pdfium::pdfium_platform_library_name_at_path(&lib_dir);

    let bind_start = Instant::now();
    let bindings = Pdfium::bind_to_library(&lib).map_err(|e| {
        anyhow!(
            "could not bind PDFium from {}: {e}\n\
             Expected pdfium.dll in that directory. Run .\\scripts\\fetch-pdfium.ps1, \
             or set PDFIUM_DLL_DIR.",
            lib.display()
        )
    })?;
    let pdfium = Pdfium::new(bindings);
    let bind = bind_start.elapsed();

    if raw > 0 {
        return dump_raw(&pdfium, &path, raw);
    }

    let mut last: Option<Report> = None;
    for run in 1..=repeat {
        // Dump on the final run only, so a warm run is what gets printed.
        let this_dump = if run == repeat { dump } else { 0 };
        let report = run_once(&pdfium, &path, this_dump)?;
        if repeat > 1 {
            println!(
                "  run {run}/{repeat}: load {:.1} ms, extract {:.1} ms",
                ms(report.load),
                ms(report.extract)
            );
        }
        last = Some(report);
    }

    let report = last.expect("at least one run");
    print_report(&path, &report, bind, repeat);
    Ok(())
}

fn run_once(pdfium: &Pdfium, path: &Path, dump: usize) -> Result<Report> {    let t_load = Instant::now();
    let doc = pdfium
        .load_pdf_from_file(path, None)
        .with_context(|| format!("opening {}", path.display()))?;
    let load = t_load.elapsed();

    let pages = doc.pages().len() as usize;
    let mut glyphs = 0usize;
    let mut lines = 0usize;
    let mut synthetic_dropped = 0usize;
    let mut spaces = SpaceStats::default();
    let mut dumped: Vec<(usize, Vec<Line>)> = Vec::new();

    let t_extract = Instant::now();
    for (index, page) in doc.pages().iter().enumerate() {
        let (page_glyphs, dropped) = extract_page(&page)
            .with_context(|| format!("extracting page {}", index + 1))?;
        glyphs += page_glyphs.len();
        synthetic_dropped += dropped;

        // Clone only for the few pages we intend to print; cloning every page
        // would pollute the timing we are trying to measure.
        let want = index < dump;
        let page_lines = group_lines(page_glyphs, &mut spaces);
        lines += page_lines.len();
        if want {
            dumped.push((index + 1, page_lines));
        }
    }
    let extract = t_extract.elapsed();

    for (page_no, page_lines) in &dumped {
        println!("\n===== page {page_no} — {} lines =====", page_lines.len());
        for line in page_lines {
            let style = format!(
                "{:>5.1}pt {}{}",
                line.size,
                if line.bold { "BOLD " } else { "" },
                if line.italic { "ITAL " } else { "" },
            );
            println!("  y={:>7.1} {style:<16} | {}", line.bottom, line.text);
            let _ = &line.fonts;
        }
    }

    Ok(Report {
        pages,
        glyphs,
        lines,
        load,
        extract,
        spaces,
        synthetic_dropped,
    })
}

/// Pull every visible character on a page, with geometry and style.
///
/// Returns the glyphs plus the number of synthetic characters discarded as
/// geometrically meaningless.
fn extract_page(page: &PdfPage) -> Result<(Vec<Glyph>, usize)> {
    let text = page.text()?;
    let mut out = Vec::with_capacity(text.len().max(0) as usize);
    let mut dropped = 0usize;

    for ch in text.chars().iter() {
        let Some(c) = ch.unicode_char() else {
            // Invalid/unpaired code unit. PDFium emits these for some
            // malformed embedded subsets.
            continue;
        };
        if c == '\0' || c == '\u{FFFE}' || c == '\u{FFFF}' {
            continue;
        }

        // Two different boxes, for two different jobs. Loose (advance) bounds
        // drive grouping and space inference; tight (ink) bounds are only
        // useful for decoration detection, so a failure there is not fatal.
        let Ok(loose) = ch.loose_bounds() else {
            continue;
        };

        let generated = ch.is_generated().unwrap_or(false);
        let height = loose.top().value - loose.bottom().value;

        // PDFium synthesises line and word separators (CR, LF, space) that
        // carry a nominal 1pt size and a zero-height box pinned to the
        // baseline. They must not take part in geometry: a zero-height glyph
        // makes a useless cluster anchor, and because the tolerance is derived
        // from the anchor's size, one of these silently splits a line in two.
        // Word boundaries are recovered from glyph gaps instead, which is
        // strictly more general than relying on what the producer chose to emit.
        if generated && height <= 0.01 {
            dropped += 1;
            continue;
        }

        let name = ch.font_name();
        let lower = name.to_ascii_lowercase();

        out.push(Glyph {
            ch: c,
            x: loose.left().value,
            right: loose.right().value,
            ink_left: ch
                .tight_bounds()
                .map(|t| t.left().value)
                .unwrap_or(loose.left().value),
            ink_right: ch
                .tight_bounds()
                .map(|t| t.right().value)
                .unwrap_or(loose.right().value),
            bottom: loose.bottom().value,
            top: loose.top().value,
            size: ch.unscaled_font_size().value,
            bold: is_bold(ch.font_weight(), &lower),
            italic: ch.font_is_italic() || lower.contains("italic") || lower.contains("oblique"),
            generated,
            font: name,
        });
    }

    Ok((out, dropped))
}

/// Decide whether a character is bold.
///
/// PDFium's weight is authoritative only for embedded fonts with a real
/// descriptor. The base-14 standard fonts report nothing useful (`from_pdfium`
/// maps -1 to `None`), so for those we fall back to the font's name, which is
/// how "Helvetica-Bold" and friends announce themselves.
fn is_bold(weight: Option<PdfFontWeight>, lower_name: &str) -> bool {
    match weight {
        Some(PdfFontWeight::Weight600)
        | Some(PdfFontWeight::Weight700Bold)
        | Some(PdfFontWeight::Weight800)
        | Some(PdfFontWeight::Weight900) => true,
        // A style is bold if it advertises semi-bold or heavier.
        Some(PdfFontWeight::Custom(w)) => w >= 600,
        Some(_) => false,
        None => lower_name.contains("bold") || lower_name.contains("black"),
    }
}

/// Cluster glyphs into lines by vertical proximity, then order each line by x.
///
/// This is the seed of the real layout analyser. It is deliberately naive here
/// -- one tolerance, no column awareness, no rotation handling -- because the
/// point of the spike is only to prove the coordinates are usable.
fn group_lines(mut glyphs: Vec<Glyph>, stats: &mut SpaceStats) -> Vec<Line> {
    // PDF y grows upwards, so sort descending by baseline to go down the page,
    // and ascending by x to go left to right within a line.
    glyphs.sort_by(|a, b| {
        b.bottom
            .partial_cmp(&a.bottom)
            .unwrap_or(Ordering::Equal)
            .then(a.x.partial_cmp(&b.x).unwrap_or(Ordering::Equal))
    });

    let mut groups: Vec<Vec<Glyph>> = Vec::new();
    for glyph in glyphs {
        match groups.last_mut() {
            Some(group) => {
                // Tolerance comes from the *largest* glyph in the group, not
                // from whichever glyph happened to arrive first. Deriving it
                // from an arbitrary anchor makes grouping order-dependent and
                // brittle: a stray small glyph (a footnote marker, a
                // superscript) would shrink the tolerance and split the line.
                let size = group.iter().map(|g| g.size).fold(0.0f32, f32::max);
                let tolerance = size.max(4.0) * 0.5;
                let anchor = group[0].bottom;

                if (anchor - glyph.bottom).abs() <= tolerance {
                    group.push(glyph);
                } else {
                    groups.push(vec![glyph]);
                }
            }
            None => groups.push(vec![glyph]),
        }
    }

    groups
        .into_iter()
        .map(|g| Line::from_glyphs(g, stats))
        .collect()
}

impl Line {
    fn from_glyphs(mut glyphs: Vec<Glyph>, stats: &mut SpaceStats) -> Self {
        glyphs.sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap_or(Ordering::Equal));

        let size = glyphs.iter().map(|g| g.size).fold(0.0f32, f32::max);
        let bold_count = glyphs.iter().filter(|g| g.bold).count();
        let bold = !glyphs.is_empty() && bold_count * 2 >= glyphs.len();
        let italic_count = glyphs.iter().filter(|g| g.italic).count();
        let italic = !glyphs.is_empty() && italic_count * 2 >= glyphs.len();

        // Reinsert the spaces that PDF omits: a gap wider than a quarter of the
        // font size means the producer positioned the next word rather than
        // emitting a space character. Compared against advance boxes, so
        // narrow glyphs like `.` and `i` do not trigger phantom spaces.
        let mut text = String::new();
        let mut previous_right: Option<f32> = None;
        for glyph in &glyphs {
            if glyph.ch == ' ' {
                if glyph.generated {
                    stats.pdfium += 1;
                } else {
                    stats.literal += 1;
                }
            }

            if let Some(prev) = previous_right {
                let gap = glyph.x - prev;
                if gap > size * 0.25 && glyph.ch != ' ' && !text.ends_with(' ') {
                    text.push(' ');
                    stats.inferred += 1;
                }
            }

            text.push(glyph.ch);
            previous_right = Some(glyph.right);
        }

        let bottom = glyphs.iter().map(|g| g.bottom).fold(f32::MAX, f32::min);
        let top = glyphs.iter().map(|g| g.top).fold(f32::MIN, f32::max);

        Self {
            text: text.trim_end().to_string(),
            bottom,
            top,
            size,
            bold,
            italic,
            fonts: glyphs.into_iter().map(|g| g.font).collect(),
        }
    }
}

/// Dump raw glyphs from page 1, one per line.
///
/// Diagnostic mode: lets us see exactly what PDFium reports at the character
/// level, including the synthetic characters it inserts. Eyeballing
/// reconstructed lines is not enough to tell a real glyph from a generated one.
fn dump_raw(pdfium: &Pdfium, path: &Path, limit: usize) -> Result<()> {
    let doc = pdfium
        .load_pdf_from_file(path, None)
        .with_context(|| format!("opening {}", path.display()))?;
    let page = doc.pages().first()?;
    let (glyphs, dropped) = extract_page(&page)?;

    println!(
        "page 1: {} glyphs total, showing first {}  ({dropped} synthetic glyphs discarded)",
        glyphs.len(),
        limit.min(glyphs.len())
    );
    println!(
        "{:>5} {:>8} {:>7} {:>7} {:>8} {:>8} {:>8} {:>8}  {:<22} gen",
        "i", "code", "char", "size", "x", "right", "bottom", "top", "font"
    );

    for (i, g) in glyphs.iter().enumerate().take(limit) {
        let shown = if g.ch.is_whitespace() {
            format!("U+{:04X}", g.ch as u32)
        } else {
            g.ch.to_string()
        };
        println!(
            "{:>5} {:>8} {:>7} {:>7.2} {:>8.2} {:>8.2} {:>8.2} {:>8.2}  {:<22} {}",
            i, g.ch as u32, shown, g.size, g.x, g.right, g.bottom, g.top, g.font, g.generated
        );
    }

    let generated = glyphs.iter().filter(|g| g.generated).count();
    let whitespace = glyphs.iter().filter(|g| g.ch.is_whitespace()).count();
    let odd_size = glyphs.iter().filter(|g| g.size < 2.0).count();
    println!(
        "\n  generated by pdfium: {generated}\n  whitespace chars:    {whitespace}\n  size < 2.0pt:        {odd_size}"
    );

    // The interesting question is not just *how many* characters PDFium
    // synthesises, but *where* it puts them. If they land on real baselines
    // they are useful word boundaries; if they land anywhere else they are
    // noise that must not be allowed to form lines.
    let mut synth: Vec<&Glyph> = glyphs.iter().filter(|g| g.generated).collect();
    synth.sort_by(|a, b| b.bottom.partial_cmp(&a.bottom).unwrap_or(Ordering::Equal));
    println!("\n  first 25 synthetic glyphs by page position:");
    println!(
        "  {:>7} {:>7} {:>8} {:>8} {:>7}  {}",
        "char", "size", "x", "bottom", "height", "whitespace"
    );
    for g in synth.iter().take(25) {
        let shown = if g.ch.is_whitespace() {
            format!("U+{:04X}", g.ch as u32)
        } else {
            g.ch.to_string()
        };
        println!(
            "  {:>7} {:>7.2} {:>8.2} {:>8.2} {:>7.2}  {}",
            shown,
            g.size,
            g.x,
            g.bottom,
            g.top - g.bottom,
            g.ch.is_whitespace()
        );
    }

    Ok(())
}

fn print_report(path: &Path, r: &Report, bind: Duration, repeat: usize) {
    let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let secs = r.extract.as_secs_f64();

    println!("\n=== {} ===", path.display());
    println!("  file            {bytes:>12} bytes");
    println!("  pages           {:>12}", r.pages);
    println!("  glyphs          {:>12}", r.glyphs);
    println!("  lines           {:>12}", r.lines);
    println!("  bind pdfium     {:>12.1} ms", ms(bind));
    println!("  load document   {:>12.1} ms", ms(r.load));
    println!(
        "  extract+group   {:>12.1} ms  (best of {repeat})",
        ms(r.extract)
    );

    if secs > 0.0 {
        println!(
            "  throughput      {:>12.1} pages/s, {:.2} mglyph/s",
            r.pages as f64 / secs,
            r.glyphs as f64 / secs / 1e6
        );
        println!("  per page        {:>12.2} ms", secs * 1000.0 / r.pages.max(1) as f64);
    }
    println!(
        "  avg per page    {:>12.1} glyphs, {:.1} lines",
        r.glyphs as f64 / r.pages.max(1) as f64,
        r.lines as f64 / r.pages.max(1) as f64
    );
    println!(
        "  spaces          {:>12} literal, {} from pdfium, {} inferred",
        r.spaces.literal, r.spaces.pdfium, r.spaces.inferred
    );
    println!(
        "  synthetic drops {:>12}",
        r.synthetic_dropped
    );

    if let Some(g) = r.glyphs.checked_div(r.lines) {
        println!("  glyphs/line     {g:>12}");
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn flag(args: &[String], name: &str) -> Option<usize> {
    let idx = args.iter().position(|a| a == name)?;
    args.get(idx + 1)?.parse().ok()
}

/// Where to load `pdfium.dll` from.
fn dll_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("PDFIUM_DLL_DIR") {
        return PathBuf::from(dir);
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("spike lives two levels below the workspace root")
        .join("third_party")
        .join("pdfium")
        .join("bin")
}
