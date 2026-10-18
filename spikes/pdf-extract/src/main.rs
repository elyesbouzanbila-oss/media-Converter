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
    /// Pen position for this glyph, straight from PDFium (`FPDFText_GetCharOrigin`).
    ///
    /// Distinct from `x`, which is the left edge of the advance box. The two are
    /// usually identical, but the *difference* between consecutive origin
    /// positions is the advance actually consumed — the ground truth for whether
    /// the producer elided a space. Comparing advance boxes instead conflates a
    /// glyph's own width with whatever follows it, which is why a threshold
    /// expressed as a fraction of the font size misfires in both directions.
    origin_x: f32,
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
    /// Measured space width as a fraction of font size, averaged over pages.
    ///
    /// Reported because it is the quantity the word-gap threshold is derived
    /// from, and it varies by font. The first real document tested against has
    /// a space of 0.226 em, against an assumed 0.25 em.
    space_em: f32,
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
        eprintln!("  --quiet      print one machine-readable RESULT line and nothing else");
        eprintln!("  --text       print every page's reconstructed text, one line each");
        eprintln!("  --space-em F override the measured space width, in em (default: measured)");
        std::process::exit(2);
    }

    let path = PathBuf::from(&argv[0]);
    let dump = flag(&argv, "--dump").unwrap_or(0);
    let raw = flag(&argv, "--raw").unwrap_or(0);
    let repeat = flag(&argv, "--repeat").unwrap_or(1).max(1);
    let quiet = argv.iter().any(|a| a == "--quiet");
    let text_only = argv.iter().any(|a| a == "--text");
    let space_em_override = float_flag(&argv, "--space-em");

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

    if text_only {
        return dump_text(&pdfium, &path, space_em_override);
    }

    let mut last: Option<Report> = None;
    for run in 1..=repeat {
        // Dump on the final run only, so a warm run is what gets printed.
        let this_dump = if run == repeat { dump } else { 0 };
        let report = run_once(&pdfium, &path, this_dump, space_em_override)?;
        if repeat > 1 && !quiet {
            println!(
                "  run {run}/{repeat}: load {:.1} ms, extract {:.1} ms",
                ms(report.load),
                ms(report.extract)
            );
        }
        last = Some(report);
    }

    let report = last.expect("at least one run");
    if quiet {
        // Single machine-readable line, for the process-pool scaling driver to
        // parse. Kept stable and space-separated so it is trivial to read back.
        println!(
            "RESULT pages={} glyphs={} lines={} load_ms={:.3} extract_ms={:.3}",
            report.pages,
            report.glyphs,
            report.lines,
            ms(report.load),
            ms(report.extract)
        );
    } else {
        print_report(&path, &report, bind, repeat);
    }
    Ok(())
}

fn run_once(
    pdfium: &Pdfium,
    path: &Path,
    dump: usize,
    space_em_override: Option<f32>,
) -> Result<Report> {
    let t_load = Instant::now();
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
    let mut space_em_total = 0.0f32;
    let mut space_em_pages = 0usize;

    for (index, page) in doc.pages().iter().enumerate() {
        let (page_glyphs, dropped) = extract_page(&page)
            .with_context(|| format!("extracting page {}", index + 1))?;
        glyphs += page_glyphs.len();
        synthetic_dropped += dropped;

        // Calibrate the word-gap threshold against this page's own space
        // characters rather than assuming a fraction of the font size. The
        // assumption was wrong in a way that only real documents exposed: this
        // font's space is 0.226 em, so a 0.25 em threshold sat *above* a genuine
        // space and silently deleted it, while still catching wider spaces
        // elsewhere. The rule was simultaneously too strict and too loose for
        // different spaces in the same document.
        let page_space_em = space_em_override.unwrap_or_else(|| measure_space_em(&page_glyphs));
        if space_em_override.is_none() {
            space_em_total += page_space_em;
            space_em_pages += 1;
        }

        // Clone only for the few pages we intend to print; cloning every page
        // would pollute the timing we are trying to measure.
        let want = index < dump;
        let page_lines = group_lines(page_glyphs, &mut spaces, page_space_em);
        lines += page_lines.len();
        if want {
            dumped.push((index + 1, page_lines));
        }
    }
    let extract = t_extract.elapsed();

    let space_em = match space_em_override {
        Some(value) => value,
        None if space_em_pages > 0 => space_em_total / space_em_pages as f32,
        None => DEFAULT_SPACE_EM,
    };

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
        space_em,
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

        // C0 and C1 control codes are never text. They surface when a font's
        // encoding maps a glyph to an undefined or private code — a font-level
        // defect whose correct character is genuinely unrecoverable from the
        // page description alone. Emitting the raw code point is strictly worse
        // than dropping it, because it silently poisons every downstream
        // consumer: the text, the XML, and finally the DOCX.
        if c.is_control() {
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
            origin_x: ch.origin_x().map(|p| p.value).unwrap_or(loose.left().value),
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
fn group_lines(mut glyphs: Vec<Glyph>, stats: &mut SpaceStats, space_em: f32) -> Vec<Line> {
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
        .map(|g| Line::from_glyphs(g, stats, space_em))
        .collect()
}

impl Line {
    fn from_glyphs(mut glyphs: Vec<Glyph>, stats: &mut SpaceStats, space_em: f32) -> Self {
        glyphs.sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap_or(Ordering::Equal));

        let size = glyphs.iter().map(|g| g.size).fold(0.0f32, f32::max);
        let bold_count = glyphs.iter().filter(|g| g.bold).count();
        let bold = !glyphs.is_empty() && bold_count * 2 >= glyphs.len();
        let italic_count = glyphs.iter().filter(|g| g.italic).count();
        let italic = !glyphs.is_empty() && italic_count * 2 >= glyphs.len();

        // Reinsert the spaces that PDF omits: producers very often position each
        // word individually instead of emitting a space character at all.
        //
        // The threshold is calibrated, not guessed. `space_em` is this page's
        // measured space width as a fraction of the font size, and the test is a
        // third of that: comfortably above the sub-point noise between adjacent
        // letters, comfortably below a real space. Assuming a fixed fraction of
        // the font size instead puts the threshold *inside* the distribution of
        // real spaces, so it misses some and invents others.
        //
        // Compared against advance boxes, so narrow glyphs like `.` and `i` do
        // not trigger phantom spaces.
        let gap_threshold = size * space_em * 0.33;
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
                if gap > gap_threshold && glyph.ch != ' ' && !text.ends_with(' ') {
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

    dump_positions(&glyphs, 90);

    Ok(())
}

/// Print glyphs in page order (top-left to bottom-right) with the advance
/// arithmetic spelled out, so a human can see whether a word gap resembles a
/// space character.
///
/// Sorted by position rather than content-stream order, because producers are
/// free to emit the right half of a line before the left half. Only the last
/// `tail` entries are shown, which is where footers live — and the footer is
/// usually the most heavily laid-out part of a page, since it is assembled from
/// separately positioned runs rather than flowing text.
fn dump_positions(glyphs: &[Glyph], tail: usize) {
    let mut sorted: Vec<&Glyph> = glyphs.iter().collect();
    sorted.sort_by(|a, b| {
        b.bottom
            .partial_cmp(&a.bottom)
            .unwrap_or(Ordering::Equal)
            .then(a.x.partial_cmp(&b.x).unwrap_or(Ordering::Equal))
    });

    let start = sorted.len().saturating_sub(tail);
    println!(
        "\n  last {} glyphs by page position, with advance arithmetic:",
        sorted.len() - start
    );
    println!(
        "  {:>7} {:>7} {:>8} {:>8} {:>7} {:>7} {:>8} {:>7}  {}",
        "char", "size", "x", "origin", "right", "width", "adv", "adv-w", "verdict"
    );

    for (i, g) in sorted.iter().enumerate().skip(start) {
        let width = g.right - g.x;

        // The advance actually consumed is the distance to the next glyph's pen
        // origin, measured only against glyphs on the same baseline at the same
        // size. Anything else is a separate run, not a continuation of this one.
        let next = sorted
            .get(i + 1)
            .filter(|n| (n.bottom - g.bottom).abs() < 0.5 && (n.size - g.size).abs() < 0.25);
        let adv = next.map(|n| n.origin_x - g.origin_x);
        let extra = adv.map(|a| a - width);

        // A Latin space is roughly 0.25-0.33 em. The band below is deliberately
        // loose: the purpose of this dump is to show the distribution, and a
        // sharper threshold would be false confidence dressed as precision.
        let verdict = match extra {
            Some(e) if e > g.size * 0.16 => "GAP",
            Some(e) if e < -g.size * 0.04 => "overlap",
            _ => "",
        };

        let shown = if g.ch.is_whitespace() {
            format!("U+{:04X}", g.ch as u32)
        } else {
            g.ch.to_string()
        };

        println!(
            "  {:>7} {:>7.2} {:>8.2} {:>8.2} {:>7.2} {:>7.2} {:>8} {:>7}  {}",
            shown,
            g.size,
            g.x,
            g.origin_x,
            g.right,
            width,
            adv.map(|a| format!("{a:.2}")).unwrap_or_else(|| "-".into()),
            extra
                .map(|e| format!("{e:+.2}"))
                .unwrap_or_else(|| "-".into()),
            verdict
        );
    }
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
        "  space width     {:>12.4} em  (measured; drives the word-gap threshold)",
        r.space_em
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

fn float_flag(args: &[String], name: &str) -> Option<f32> {
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

/// Fallback space width in em, used only when a page contains no space
/// characters to measure. 0.25 is the value the original heuristic assumed.
const DEFAULT_SPACE_EM: f32 = 0.25;

/// Measure a page's space width as a fraction of the font size.
///
/// A PDF's space character has a real advance box, so the document states its
/// own space width; there is no need to guess from the font size. The median is
/// used rather than the mean so that a few stretched justified spaces cannot
/// drag the estimate upwards — the threshold derived from this has to stay below
/// the *narrowest* real space, not the average one.
fn measure_space_em(glyphs: &[Glyph]) -> f32 {
    let mut ratios: Vec<f32> = glyphs
        .iter()
        .filter(|g| g.ch == ' ' && !g.generated && g.size > 0.0)
        .map(|g| (g.right - g.x) / g.size)
        .filter(|r| *r > 0.02 && *r < 1.0)
        .collect();

    if ratios.is_empty() {
        return DEFAULT_SPACE_EM;
    }

    ratios.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
    ratios[ratios.len() / 2]
}

/// Print the reconstructed text of every page, one output line per text line.
///
/// Exists so that reconstruction can be diffed against an independent
/// extractor. Text quality is not something to eyeball at scale; an external
/// reference turns "looks about right" into a number that can regress.
fn dump_text(pdfium: &Pdfium, path: &Path, space_em_override: Option<f32>) -> Result<()> {
    let doc = pdfium
        .load_pdf_from_file(path, None)
        .with_context(|| format!("opening {}", path.display()))?;

    let mut stats = SpaceStats::default();
    for page in doc.pages().iter() {
        let (glyphs, _) = extract_page(&page)?;
        let space_em = space_em_override.unwrap_or_else(|| measure_space_em(&glyphs));
        for line in group_lines(glyphs, &mut stats, space_em) {
            let text = line.text.trim();
            if !text.is_empty() {
                println!("{text}");
            }
        }
    }

    Ok(())
}
