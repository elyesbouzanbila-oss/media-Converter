//! Phase 0 spike B — embedded Typst PDF generation.
//!
//! Validates the "anything -> PDF" direction of the plan, which is where the
//! native engine can genuinely beat an external converter. Questions:
//!
//!   1. How fast is Typst when embedded, end to end (compile + PDF render)?
//!   2. How much does font discovery actually cost? The plan hinges on loading
//!      fonts once and sharing them across every document in a batch, so the
//!      cold-vs-warm gap is the number that decides the design.
//!   3. Does it work without a toolchain, a network, or a Typst install?
//!
//! Run:
//!   cargo run -p spike-typst-pdf --profile spike -- --chapters 60
//!
//! `--pages` is not a thing: content flows, so the page count is an output, not
//! an input. We generate N chapters and report how many pages they produced.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use typst::diag::{FileError, FileResult};
use typst::foundations::{Bytes, Datetime, Duration as TypstDuration};
use typst::{Library, LibraryExt};
use typst::syntax::{FileId, RootedPath, Source, VirtualPath, VirtualRoot};
use typst::text::{Font, FontBook};
use typst::utils::LazyHash;
use typst::World;
use typst_layout::PagedDocument;

// ---------------------------------------------------------------------------
// Shared, reusable state
// ---------------------------------------------------------------------------

/// Everything that can be paid for once and reused for every document in a
/// batch: the standard library and the font book.
///
/// This is the whole point of the spike. `Library::default()` builds the
/// standard library, and font discovery parses font headers out of the
/// embedded font files -- both are expensive relative to typesetting a small
/// document, and neither changes between documents.
struct EngineCache {
    library: LazyHash<Library>,
    book: LazyHash<FontBook>,
    fonts: Vec<Font>,
}

impl EngineCache {
    fn load() -> Self {
        let mut book = FontBook::new();
        let mut fonts = Vec::new();

        // Each blob may be a font collection holding several faces, so index
        // until `Font::new` stops returning faces.
        for data in typst_assets::fonts() {
            let bytes = Bytes::new(data);
            let mut index = 0;
            while let Some(font) = Font::new(bytes.clone(), index) {
                book.push(font.info().clone());
                fonts.push(font);
                index += 1;
            }
        }

        Self {
            library: LazyHash::new(Library::default()),
            book: LazyHash::new(book),
            fonts,
        }
    }
}

/// A single document's world: owns the source, borrows the shared cache.
///
/// Constructing one of these is meant to be cheap. In production we build one
/// per document in the batch and a new one per retry, so it must not be where
/// the cost lives.
struct DocWorld {
    cache: Arc<EngineCache>,
    main: FileId,
    source: Source,
}

impl DocWorld {
    fn new(cache: Arc<EngineCache>, text: String) -> Self {
        // A virtual path is required by the compiler even though we resolve
        // everything in memory; it never touches the filesystem.
        let vpath = VirtualPath::new("main.typ").expect("static path is valid");
        let main = FileId::new(RootedPath::new(VirtualRoot::Project, vpath));
        Self {
            cache,
            main,
            source: Source::new(main, text),
        }
    }
}

impl World for DocWorld {
    fn library(&self) -> &LazyHash<Library> {
        &self.cache.library
    }

    fn book(&self) -> &LazyHash<FontBook> {
        &self.cache.book
    }

    fn main(&self) -> FileId {
        self.main
    }

    fn source(&self, id: FileId) -> FileResult<Source> {
        if id == self.main {
            Ok(self.source.clone())
        } else {
            // We present a single in-memory file. Anything else genuinely does
            // not exist, and saying so is better than silently resolving.
            Err(FileError::NotFound(PathBuf::from("main.typ")))
        }
    }

    fn file(&self, id: FileId) -> FileResult<Bytes> {
        // `text()` yields a `&str` borrowed from a temporary `Source`, so this
        // must copy before the temporary dies. `Bytes::from_string` holds an
        // owned string anyway.
        Ok(Bytes::from_string(self.source(id)?.text().to_owned()))
    }

    fn font(&self, index: usize) -> Option<Font> {
        self.cache.fonts.get(index).cloned()
    }

    fn today(&self, _offset: Option<TypstDuration>) -> Option<Datetime> {
        // Returning None makes `datetime.today()` error rather than lie.
        // Documents that need a stable date should be given one explicitly, or
        // the same document would produce different bytes on different days and
        // break reproducibility.
        None
    }
}

// ---------------------------------------------------------------------------
// Document generation
// ---------------------------------------------------------------------------

/// Build a Typst source document.
///
/// `variant` is woven into the content deliberately. Typst memoises compilation
/// globally through `comemo`, keyed on the source and its spans, so compiling
/// byte-identical source repeatedly does no work at all. A benchmark that does
/// not vary its input measures a hash lookup and overstates throughput by
/// several times.
fn make_source(chapters: usize, variant: usize) -> String {
    let mut s = String::with_capacity(chapters * 900);
    s.push_str("#set page(paper: \"a4\", margin: 2cm)\n");
    s.push_str("#set text(size: 10pt)\n\n");

    for chapter in 1..=chapters {
        s.push_str(&format!("= Chapter {chapter} (variant {variant})\n\n"));
        for para in 1..=12 {
            // The variant is woven into *every* paragraph, not just once.
            // Changing a single number lets `comemo` reuse most of the layout,
            // which would make a "distinct content" benchmark quietly measure
            // partial cache hits and understate true per-document cost.
            s.push_str(&format!(
                "Paragraph {para} of chapter {chapter}, revision {variant}-{chapter}-{para}. \
                 Layout recovery from fixed coordinates is a tractable problem provided \
                 sufficient attention is paid to typographic convention, and provided one \
                 is willing to accept that some information has genuinely been discarded by \
                 the producer. The quick brown fox jumps over the lazy dog throughout.\n\n"
            ));
        }
    }

    s
}

// ---------------------------------------------------------------------------
// One render pass
// ---------------------------------------------------------------------------

struct Outcome {
    world: Duration,
    compile: Duration,
    render: Duration,
    bytes: usize,
    pages: usize,
    warnings: usize,
}

fn render(cache: &Arc<EngineCache>, chapters: usize, variant: usize) -> Result<Outcome> {
    let source = make_source(chapters, variant);

    let t = Instant::now();
    let world = DocWorld::new(cache.clone(), source);
    let world_time = t.elapsed();

    let t = Instant::now();
    let compiled = typst::compile::<PagedDocument>(&world);
    let warnings = compiled.warnings.len();
    let document = compiled
        .output
        .map_err(|errors| anyhow!("typst compile failed: {} error(s): {:?}", errors.len(), errors))?;
    let compile_time = t.elapsed();

    let t = Instant::now();
    let pdf = typst_pdf::pdf(&document, &typst_pdf::PdfOptions::default())
        .map_err(|errors| anyhow!("typst pdf export failed: {:?}", errors))?;
    let render_time = t.elapsed();

    Ok(Outcome {
        world: world_time,
        compile: compile_time,
        render: render_time,
        bytes: pdf.len(),
        pages: document.pages().len(),
        warnings,
    })
}

// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let chapters = usize_arg(&argv, "--chapters").unwrap_or(60);
    let runs = usize_arg(&argv, "--runs").unwrap_or(3).max(1);
    let out = string_arg(&argv, "--out")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/typst-spike.pdf"));

    println!("generating {chapters} chapters, {runs} run(s) per mode\n");

    let t = Instant::now();
    let shared = Arc::new(EngineCache::load());
    let setup = t.elapsed();

    // Warm up allocator state, code pages and the memo tables, so the first
    // measured run is not paying one-off costs unrelated to the comparison.
    let warmup = render(&shared, chapters, 0)?;
    println!(
        "  warm-up:  compile {:.1} ms, render {:.1} ms, {} pages\n",
        ms(warmup.compile),
        ms(warmup.render),
        warmup.pages
    );

    // --- Identical content: measures the memo cache, not the compiler. -----
    let mut identical = Vec::new();
    for run in 1..=runs {
        let o = render(&shared, chapters, 1)?;
        println!(
            "  identical {run}/{runs}: compile {:>8.2} ms, render {:>6.1} ms",
            ms(o.compile),
            ms(o.render)
        );
        identical.push(o);
    }

    // --- Distinct content: what an actual batch looks like. ----------------
    let mut distinct = Vec::new();
    for run in 1..=runs {
        let o = render(&shared, chapters, 2_000_000 + run)?;
        println!(
            "  distinct  {run}/{runs}: compile {:>8.1} ms, render {:>6.1} ms",
            ms(o.compile),
            ms(o.render)
        );
        distinct.push(o);
    }

    // Write a real artefact so the output can be eyeballed rather than
    // trusted. A number that looks good but renders garbage is worthless.
    let source = make_source(chapters, 3_000_000);
    let world = DocWorld::new(shared.clone(), source);
    let compiled = typst::compile::<PagedDocument>(&world);
    let document = compiled
        .output
        .map_err(|errors| anyhow!("compile failed: {:?}", errors))?;
    let pdf = typst_pdf::pdf(&document, &typst_pdf::PdfOptions::default())
        .map_err(|errors| anyhow!("pdf export failed: {:?}", errors))?;

    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(&out, &pdf)?;

    // --- Report ------------------------------------------------------------
    let total = |o: &Outcome| o.compile + o.render;
    let best_identical = identical.iter().map(total).min().expect("runs");
    let best_distinct = distinct.iter().map(total).min().expect("runs");
    let worst_distinct = distinct.iter().map(total).max().expect("runs");
    let pages = distinct.last().map(|o| o.pages).unwrap_or(0);
    let bytes = distinct.last().map(|o| o.bytes).unwrap_or(0);
    let warnings = distinct.last().map(|o| o.warnings).unwrap_or(0);

    println!("\n=== typst embedded: {pages} pages from {chapters} chapters ===");
    println!("  fonts discovered      {:>10}", shared.fonts.len());
    println!(
        "  engine setup          {:>10.1} ms  (library + fonts, once per process)",
        ms(setup)
    );
    println!(
        "  identical content     {:>10.1} ms  per document (memoised: NOT representative)",
        ms(best_identical)
    );
    println!(
        "  distinct content      {:>10.1} ms  per document (best of {runs})",
        ms(best_distinct)
    );
    println!(
        "  distinct content      {:>10.1} ms  per document (worst of {runs})",
        ms(worst_distinct)
    );
    println!(
        "  breakdown             compile {:.1} ms + render {:.1} ms",
        ms(distinct.iter().map(|o| o.compile).min().unwrap()),
        ms(distinct.iter().map(|o| o.render).min().unwrap())
    );
    println!(
        "  world construction    {:>10.2} ms  per document",
        ms(distinct.iter().map(|o| o.world).min().unwrap())
    );
    if best_distinct.as_secs_f64() > 0.0 {
        println!(
            "  throughput            {:>10.1} pages/s  ({:.1} documents/s)",
            pages as f64 / best_distinct.as_secs_f64(),
            1.0 / best_distinct.as_secs_f64()
        );
    }
    println!("  output                {:>10} bytes", bytes);
    println!("  warnings              {:>10}", warnings);
    println!("\n  wrote {}", out.display());

    Ok(())
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn usize_arg(args: &[String], name: &str) -> Option<usize> {
    let idx = args.iter().position(|a| a == name)?;
    args.get(idx + 1)?.parse().ok()
}

fn string_arg(args: &[String], name: &str) -> Option<String> {
    let idx = args.iter().position(|a| a == name)?;
    args.get(idx + 1).cloned()
}
