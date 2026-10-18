//! End-to-end golden tests over the committed fixtures.
//!
//! Every other test in the workspace covers one stage: the layout tests use
//! hand-built geometry, the writer's tests use a hand-built IR. Nothing before
//! this exercised the whole pipeline from a real PDF to real `.docx` bytes, and
//! the documents that the fidelity numbers come from are gitignored — so a
//! fresh clone could not reproduce *any* end-to-end result. This closes that:
//! `tests/fixtures/synthetic/` is committed, and the expected output beside each
//! fixture is committed with it.
//!
//! Three things are asserted per fixture, and the order matters:
//!
//! 1. The conversion succeeds.
//! 2. Running it twice produces **byte-identical** files. The writer already is
//!    deterministic (the zip crate's default timestamp is the fixed 1980 epoch
//!    rather than now), and this is what makes "the CLI and the GUI produce
//!    byte-identical output" a checkable claim rather than an aspiration. It
//!    would be lost silently by anything that reached for the clock or a hash
//!    map, so it is asserted rather than assumed.
//! 3. Those bytes match the committed golden file.
//!
//! The golden is the `.docx` itself rather than its text. Text would be easier
//! to read in a diff, but it is a weaker assertion: it would not notice run
//! formatting, paragraph styles, table structure or a part going missing. The
//! price is that the failure message has to do the explaining, so on a mismatch
//! the two documents are unzipped and their `word/document.xml` compared in
//! paragraph-sized chunks.
//!
//! The zip reading here is the `zip` crate and the XML comparison is plain
//! string comparison — deliberately not the writer's own code, since a reader
//! that shared the writer's assumptions could agree with it about a mistake.
//! Structural validity against a real OOXML parser is
//! `scripts/validate_docx.py`'s job, which python-docx does properly.
//!
//! Re-bless after a deliberate change to the output:
//!
//! ```powershell
//! $env:DCONV_BLESS = "1"; cargo test -p dconv-engine --profile spike
//! ```
//!
//! Then read `git diff` on the golden files. A blessing that nobody read is a
//! test that has quietly stopped testing anything.

use dconv_engine::{Converter, Sink};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// One `#[test]`, not one per fixture.
///
/// Binding PDFium is process-global and can only succeed once per process, so a
/// second test that calls `Converter::bind()` would fail depending on which
/// thread got there first. Iterating here keeps one bind, and has the pleasant
/// side effect of reporting every fixture's problem in a single run instead of
/// stopping at the first.
#[test]
fn the_synthetic_fixtures_still_produce_their_committed_output() {
    let engine = Converter::bind().unwrap_or_else(|error| {
        panic!(
            "the golden tests need PDFium, and binding it failed: {error}\n\
             Run .\\scripts\\fetch-pdfium.ps1 once, then run the tests again."
        )
    });

    let directory = fixture_directory();
    let bless = std::env::var_os("DCONV_BLESS").is_some();

    let mut fixtures: Vec<PathBuf> = std::fs::read_dir(&directory)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", directory.display()))
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "pdf"))
        .collect();
    fixtures.sort();

    assert!(
        !fixtures.is_empty(),
        "no fixtures found in {}; run `python scripts/make_fixtures.py`",
        directory.display()
    );

    let scratch = tempfile::tempdir().expect("a temporary directory");
    let mut failures = Vec::new();
    let mut blessed = Vec::new();

    for input in &fixtures {
        let name = input
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        let golden = directory.join(format!("{name}.golden.docx"));

        match produce(&engine, input, scratch.path()) {
            Err(problem) => failures.push(format!("{name}: {problem}")),
            Ok(bytes) => {
                if bless {
                    if let Err(error) = std::fs::write(&golden, &bytes) {
                        failures.push(format!("{name}: blessing failed: {error}"));
                    } else {
                        blessed.push(name);
                    }
                    continue;
                }

                match std::fs::read(&golden) {
                    Err(error) => failures.push(format!(
                        "{name}: no golden file at {}: {error}",
                        golden.display()
                    )),
                    Ok(expected) if expected == bytes => {}
                    Ok(expected) => failures.push(explain(&name, &expected, &bytes)),
                }
            }
        }
    }

    if bless {
        println!("blessed {} golden files: {}", blessed.len(), blessed.join(", "));
        println!("now read `git diff tests/fixtures/synthetic` before committing");
        assert!(failures.is_empty(), "{}", failures.join("\n"));
        return;
    }

    assert!(
        failures.is_empty(),
        "{} of {} fixtures do not match:\n\n{}",
        failures.len(),
        fixtures.len(),
        failures.join("\n\n")
    );
}

/// Convert one fixture and check that it is reproducible, returning the bytes.
fn produce(engine: &Converter, input: &Path, scratch: &Path) -> Result<Vec<u8>, String> {
    let name = input.file_stem().unwrap_or_default().to_string_lossy();
    let mut sink = NoProgress;

    let first_path = scratch.join(format!("{name}.1.docx"));
    let started = Instant::now();
    let report = engine
        .convert(input, &first_path, &mut sink)
        .map_err(|error| format!("conversion failed: {error}"))?;
    let elapsed = started.elapsed();

    if report.blocks == 0 {
        return Err("produced a document with no blocks".to_string());
    }
    if report.stats.pages == 0 {
        return Err("read no pages".to_string());
    }

    let bytes = std::fs::read(&first_path).map_err(|error| format!("reading output: {error}"))?;
    if bytes.len() < 1024 {
        return Err(format!("output is only {} bytes", bytes.len()));
    }

    // Determinism: the same input must produce the same bytes, so that two front
    // ends calling the same facade cannot differ. This alone would not catch a
    // timestamp — DOS times have two-second granularity and both conversions here
    // land inside the same second — which is why `inspect` checks the entries and
    // the golden file is the assertion that really holds.
    let second_path = scratch.join(format!("{name}.2.docx"));
    engine
        .convert(input, &second_path, &mut sink)
        .map_err(|error| format!("second conversion failed: {error}"))?;
    let again = std::fs::read(&second_path).map_err(|error| format!("reading output: {error}"))?;
    if again != bytes {
        return Err(
            "two runs of the same input produced different bytes, so the writer has picked up \
             something non-deterministic (a clock, or an unordered iteration)"
                .to_string(),
        );
    }

    // A .docx the zip crate can open, carrying the parts it must and no clock.
    // Not a substitute for validate_docx.py, but it fails here first and with a
    // clearer message when the package is broken outright.
    inspect(&bytes).map_err(|error| format!("output is not a usable .docx: {error}"))?;

    println!(
        "  {name:<14} {:>3} pages  {:>4} blocks  {:>6.1} ms  {:>6} bytes",
        report.stats.pages,
        report.blocks,
        elapsed.as_secs_f64() * 1000.0,
        bytes.len()
    );

    Ok(bytes)
}

/// A readable account of what changed, for a failure message.
fn explain(name: &str, expected: &[u8], actual: &[u8]) -> String {
    let mut out = format!("{name}: output differs from the golden file");

    match (inspect(expected), inspect(actual)) {
        (Ok(expected), Ok(actual)) => {
            let expected = chunks(&expected);
            let actual = chunks(&actual);
            out.push_str(&format!(
                "\n  {} chunks expected, {} produced",
                expected.len(),
                actual.len()
            ));

            match expected.iter().zip(&actual).position(|(a, b)| a != b) {
                Some(index) => {
                    out.push_str(&format!("\n\n  first difference at chunk {index}:"));
                    out.push_str(&format!("\n    expected: {}", truncate(&expected[index])));
                    out.push_str(&format!("\n    actual:   {}", truncate(&actual[index])));
                }
                None => out.push_str("\n  every shared chunk matches; the difference is structural"),
            }
        }
        (Err(error), _) => out.push_str(&format!("\n  the golden file is unreadable: {error}")),
        (_, Err(error)) => out.push_str(&format!("\n  the produced file is unreadable: {error}")),
    }

    out.push_str("\n\n  If the change is intended, re-bless and read the diff:");
    out.push_str("\n    $env:DCONV_BLESS = \"1\"; cargo test -p dconv-engine --profile spike");

    out
}

/// Open the produced package, check what a `.docx` must have, and return the
/// body for diagnostics.
///
/// The timestamp check is here rather than left to the byte comparison because
/// the byte comparison is nearly blind to it: DOS timestamps have two-second
/// granularity, so two conversions inside one test run land in the same second
/// and agree. The clock only shows up between runs — which is exactly how it
/// went unnoticed until the golden files caught it.
fn inspect(bytes: &[u8]) -> Result<String, String> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes.to_vec()))
        .map_err(|error| format!("not a zip archive: {error}"))?;

    for required in ["[Content_Types].xml", "word/document.xml", "word/styles.xml"] {
        if archive.by_name(required).is_err() {
            return Err(format!("no {required} part"));
        }
    }

    let pinned = zip::DateTime::default();
    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .map_err(|error| format!("reading entry {index}: {error}"))?;
        if entry.last_modified() != Some(pinned) {
            let name = entry
                .name()
                .map(|name| name.into_owned())
                .unwrap_or_else(|_| format!("entry {index}"));
            return Err(format!(
                "{name} carries the timestamp {:?} instead of the fixed {:?}, \
                 so two conversions of one document produce different bytes",
                entry.last_modified(),
                pinned
            ));
        }
    }

    let mut file = archive
        .by_name("word/document.xml")
        .map_err(|error| format!("no word/document.xml: {error}"))?;
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|error| format!("word/document.xml is not utf-8: {error}"))?;
    Ok(text)
}

/// Break the body into paragraph-sized pieces, so a mismatch can be located.
///
/// The parts are written without newlines, so the raw XML is one line; splitting
/// after the tags that end a block gives something a person can compare.
fn chunks(xml: &str) -> Vec<String> {
    let spaced = xml
        .replace("</w:p>", "</w:p>\n")
        .replace("</w:tr>", "</w:tr>\n")
        .replace("</w:tbl>", "</w:tbl>\n")
        // Inside a table, one cell per line, or a wide row is unreadable.
        .replace("<w:tc>", "\n<w:tc>");

    spaced.lines().map(str::to_string).collect()
}

fn truncate(text: &str) -> String {
    const LIMIT: usize = 300;
    if text.chars().count() <= LIMIT {
        return text.to_string();
    }
    let head: String = text.chars().take(LIMIT).collect();
    format!("{head}… ({} chars)", text.chars().count())
}

fn fixture_directory() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/synthetic")
}

/// Progress is not what these tests are about, and the default trait methods are
/// exactly "report nothing".
struct NoProgress;

impl Sink for NoProgress {}
