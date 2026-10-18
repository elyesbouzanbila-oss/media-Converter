//! Conversion facade.
//!
//! The single entry point the CLI and the GUI both call, so that "what a
//! conversion does" is defined in exactly one place. The CLI and the GUI are
//! required to produce byte-identical output, and the only way to guarantee that
//! rather than hope for it is for neither of them to own any conversion logic at
//! all: they differ in how they collect inputs and how they show progress, and
//! in nothing else.
//!
//! Format handling is deliberately a closed match rather than a plugin registry.
//! A registry is the right shape for the external-engine layer planned later,
//! but until there is a second reader or writer to register, it would be an
//! indirection with one occupant — and it would hide the fact that the pairs
//! supported are a curated list, not a matrix.

pub use dconv_ir as ir;
// `PdfError` and `ReadStats` are re-exported rather than left to `dconv-pdf`
// because a front end must be able to name them without depending on the reader
// crate: cancellation reaches a front end as `PdfError::Cancelled` through the
// reader chain, and the diagnostics a conversion reports *are* a `ReadStats`.
// Without this a front end either matches on error strings or takes the stats
// apart field by field, and both of those go stale the first time the reader
// gains a variant. Asked for by the GUI owner; the ask was right.
pub use dconv_pdf::{PdfError, ReadStats, Sink};

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Reader(#[from] PdfError),

    #[error(transparent)]
    Writer(#[from] dconv_docx::DocxError),

    #[error("{} has no file extension, so its format cannot be determined", .path.display())]
    UnknownFormat { path: PathBuf },

    #[error("converting {from} to {to} is not supported yet")]
    UnsupportedPair { from: Format, to: Format },
}

/// A format the engine can name. Only some of these can be read or written so
/// far; the rest are listed because error messages are better for saying
/// "not supported yet" than for saying "unrecognised".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Pdf,
    Docx,
}

impl Format {
    /// Every format this build can name, for exhaustive checks.
    ///
    /// A list rather than a `strum` derive or a macro: with two formats the
    /// derive would be more machinery than the thing it derives.
    pub const ALL: &[Format] = &[Format::Pdf, Format::Docx];

    pub const fn extension(self) -> &'static str {
        match self {
            Format::Pdf => "pdf",
            Format::Docx => "docx",
        }
    }

    /// The format a bare extension names, case-insensitively and without the
    /// dot. Separate from [`Format::from_path`] because a front end usually has
    /// an extension before it has a path: a queue is filtered as files are
    /// added, and a folder is scanned before anything is opened.
    pub fn from_extension(extension: &str) -> Option<Self> {
        match extension.trim_start_matches('.').to_ascii_lowercase().as_str() {
            "pdf" => Some(Format::Pdf),
            "docx" => Some(Format::Docx),
            _ => None,
        }
    }

    pub fn from_path(path: &Path) -> Option<Self> {
        Self::from_extension(path.extension()?.to_str()?)
    }
}

impl std::fmt::Display for Format {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Format::Pdf => "PDF",
            Format::Docx => "DOCX",
        })
    }
}

/// Every conversion this build can actually perform, as `(reader, writer)`.
///
/// The single list behind both [`Converter::convert`] and [`targets_for`], so
/// that what a front end offers and what the facade does cannot drift apart. A
/// front end cannot see the dispatch match in `convert`, so without this it has
/// to copy the match — and a copy is what goes stale. The GUI owner asked for
/// this query in a comment; the comment was right, and the reason it is right is
/// that a UI offering a format the engine rejects fails one job per file, fifty
/// times, after the user has queued them.
///
/// A row here is a promise about `convert`, not a wish: `every_promised_pair_has_
/// a_dispatch_arm` fails if a row has no arm to carry it out.
pub const CONVERSIONS: &[(Format, Format)] = &[(Format::Pdf, Format::Docx)];

/// The outputs this build can produce from `source`.
///
/// Empty means the format is not read at all, which is the question a front end
/// is really asking when it filters a queue: is this file worth offering?
pub fn targets_for(source: Format) -> Vec<Format> {
    CONVERSIONS
        .iter()
        .filter(|(from, _)| *from == source)
        .map(|(_, to)| *to)
        .collect()
}

/// The formats this build can read, in the order [`CONVERSIONS`] lists them.
pub fn readers() -> Vec<Format> {
    let mut seen = Vec::new();
    for (from, _) in CONVERSIONS {
        if !seen.contains(from) {
            seen.push(*from);
        }
    }
    seen
}

/// Every output format reachable from anything this build reads — the union a
/// settings panel offers before a queue has decided what is in it.
pub fn all_targets() -> Vec<Format> {
    let mut seen = Vec::new();
    for (_, to) in CONVERSIONS {
        if !seen.contains(to) {
            seen.push(*to);
        }
    }
    seen
}

/// Whether [`Converter::convert`] accepts this pair.
///
/// Checked before the dispatch match rather than left to its catch-all arm, so
/// that [`CONVERSIONS`] decides *whether* and the match decides only *how*.
fn supported_pair(from: Format, to: Format) -> bool {
    CONVERSIONS.contains(&(from, to))
}

/// What one conversion did.
#[derive(Debug, Clone)]
pub struct Report {
    pub input: PathBuf,
    pub output: PathBuf,
    pub blocks: usize,
    /// Time spent reading, which is where the PDF work is.
    pub read: Duration,
    /// Time spent building the DOCX package.
    pub write: Duration,
    pub output_bytes: u64,
    /// What the reader found, including anything it had to discard. Carried
    /// whole rather than copied field by field so that a new diagnostic does not
    /// need plumbing through three layers to become visible.
    pub stats: ReadStats,
}

impl Report {
    pub fn total(&self) -> Duration {
        self.read + self.write
    }

    pub fn pages(&self) -> usize {
        self.stats.pages
    }
}

/// A bound engine.
///
/// Holding the bound library is the whole point: binding PDFium is
/// process-global and can only succeed once, so a batch must reuse one handle.
/// This type is that handle, and it is why the CLI constructs one `Converter`
/// for a whole run rather than one per file.
pub struct Converter {
    pdfium: dconv_pdf::Pdfium,
}

impl Converter {
    /// Bind PDFium from the standard locations.
    pub fn bind() -> Result<Self, EngineError> {
        Ok(Self {
            pdfium: dconv_pdf::bind::bind()?,
        })
    }

    /// Bind PDFium from an explicit directory.
    pub fn bind_from(directory: &Path) -> Result<Self, EngineError> {
        Ok(Self {
            pdfium: dconv_pdf::bind_from(directory)?,
        })
    }

    /// Convert one file, choosing the reader and writer from the extensions.
    ///
    /// Fails before doing any work if the pair is not supported, so that a batch
    /// of fifty files does not fail on the fiftieth after doing forty-nine.
    pub fn convert(
        &self,
        input: &Path,
        output: &Path,
        sink: &mut dyn Sink,
    ) -> Result<Report, EngineError> {
        let from = Format::from_path(input).ok_or_else(|| EngineError::UnknownFormat {
            path: input.to_path_buf(),
        })?;
        let to = Format::from_path(output).ok_or_else(|| EngineError::UnknownFormat {
            path: output.to_path_buf(),
        })?;

        // The capability table decides whether the pair is supported, so that
        // `targets_for` and `convert` answer with the same list rather than with
        // two opinions that have to be kept in step by hand.
        if !supported_pair(from, to) {
            return Err(EngineError::UnsupportedPair { from, to });
        }

        match (from, to) {
            (Format::Pdf, Format::Docx) => self.pdf_to_docx(input, output, sink),
            // Unreachable while `CONVERSIONS` and this match agree — the
            // capability tests hold them together — but a clean error is better
            // than a panic if they ever drift.
            (from, to) => Err(EngineError::UnsupportedPair { from, to }),
        }
    }

    /// Read and analyse without writing anything.
    ///
    /// Exists because "why did the converter produce that?" is the question that
    /// actually gets asked, and answering it needs the intermediate structure.
    /// The CLI prints it and the GUI can preview it, and neither has to
    /// reimplement any part of the pipeline to get it.
    pub fn analyse(&self, input: &Path, sink: &mut dyn Sink) -> Result<ir::Document, EngineError> {
        let from = Format::from_path(input).ok_or_else(|| EngineError::UnknownFormat {
            path: input.to_path_buf(),
        })?;
        if from != Format::Pdf {
            return Err(EngineError::UnsupportedPair {
                from,
                to: Format::Docx,
            });
        }

        let (document, _) = dconv_pdf::read_tracked(&self.pdfium, input, sink)?;
        Ok(document)
    }

    fn pdf_to_docx(
        &self,
        input: &Path,
        output: &Path,
        sink: &mut dyn Sink,
    ) -> Result<Report, EngineError> {
        let started = Instant::now();
        let (document, stats) = dconv_pdf::read_tracked(&self.pdfium, input, sink)?;
        let read = started.elapsed();

        let started = Instant::now();
        dconv_docx::write_to_path(&document, output)?;
        let write = started.elapsed();

        Ok(Report {
            input: input.to_path_buf(),
            output: output.to_path_buf(),
            blocks: document.blocks.len(),
            read,
            write,
            output_bytes: std::fs::metadata(output).map(|m| m.len()).unwrap_or(0),
            stats,
        })
    }
}

/// Where the conversion of `input` should be written.
///
/// `target` is interpreted the way a user means it rather than the way `Path`
/// wants it: an existing directory receives the file under its own name, and a
/// path ending in a separator is treated as a directory even before it exists,
/// which is what makes `-o out/` work on a first run.
pub fn output_path(input: &Path, target: Option<&Path>, to: Format) -> PathBuf {
    let name = input
        .file_stem()
        .map(|stem| Path::new(stem).with_extension(to.extension()))
        .unwrap_or_else(|| PathBuf::from(format!("output.{}", to.extension())));

    match target {
        None => input.with_extension(to.extension()),
        Some(target) if looks_like_a_directory(target) => target.join(name),
        Some(target) => target.to_path_buf(),
    }
}

fn looks_like_a_directory(path: &Path) -> bool {
    path.is_dir() || path.to_string_lossy().ends_with(['/', '\\'])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_come_from_extensions_case_insensitively() {
        assert_eq!(Format::from_path(Path::new("a.PDF")), Some(Format::Pdf));
        assert_eq!(Format::from_path(Path::new("a.docx")), Some(Format::Docx));
        assert_eq!(Format::from_path(Path::new("a.txt")), None);
        assert_eq!(Format::from_path(Path::new("noextension")), None);
    }

    #[test]
    fn a_bare_extension_names_a_format_with_or_without_its_dot() {
        // A front end filters a queue by extension string, before it has a path
        // to hand `from_path`.
        assert_eq!(Format::from_extension("PDF"), Some(Format::Pdf));
        assert_eq!(Format::from_extension(".docx"), Some(Format::Docx));
        assert_eq!(Format::from_extension("txt"), None);
        assert_eq!(Format::from_extension(""), None);
    }

    #[test]
    fn the_capability_query_answers_for_every_known_format() {
        assert_eq!(targets_for(Format::Pdf), vec![Format::Docx]);
        assert_eq!(targets_for(Format::Docx), Vec::new());
        assert_eq!(readers(), vec![Format::Pdf]);
        assert_eq!(all_targets(), vec![Format::Docx]);
    }

    #[test]
    fn every_promised_pair_has_a_dispatch_arm() {
        // The query exists so a front end does not mirror `convert`'s match, and
        // the reason it should not is that a mirror goes stale silently. That
        // makes the table and the match agreeing an invariant rather than a
        // detail: a row with no arm offers a conversion that fails at the last
        // moment, one job per file, after the queue has already been accepted.
        let dispatched = |from, to| matches!((from, to), (Format::Pdf, Format::Docx));
        for &from in Format::ALL {
            for &to in Format::ALL {
                assert_eq!(
                    supported_pair(from, to),
                    dispatched(from, to),
                    "{from} -> {to}: CONVERSIONS and the dispatch match disagree"
                );
            }
        }
    }

    #[test]
    fn without_a_target_the_file_lands_beside_its_source() {
        let path = output_path(Path::new("/docs/report.pdf"), None, Format::Docx);
        assert_eq!(path, Path::new("/docs/report.docx"));
    }

    #[test]
    fn a_trailing_separator_means_a_directory_even_before_it_exists() {
        let path = output_path(Path::new("/docs/report.pdf"), Some(Path::new("out/")), Format::Docx);
        assert_eq!(path, Path::new("out/report.docx"));
    }

    #[test]
    fn an_explicit_file_name_is_used_as_given() {
        let path = output_path(
            Path::new("/docs/report.pdf"),
            Some(Path::new("renamed.docx")),
            Format::Docx,
        );
        assert_eq!(path, Path::new("renamed.docx"));
    }

    #[test]
    fn the_extension_of_the_second_dot_is_preserved_in_the_stem() {
        // `3IRS_TP2.v2.pdf` must not become `3IRS_TP2.docx`.
        let path = output_path(Path::new("3IRS_TP2.v2.pdf"), None, Format::Docx);
        assert_eq!(path, Path::new("3IRS_TP2.v2.docx"));
    }
}
