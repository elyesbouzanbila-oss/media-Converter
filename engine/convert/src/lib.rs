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
pub use dconv_pdf::Sink;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Reader(#[from] dconv_pdf::PdfError),

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
    pub const fn extension(self) -> &'static str {
        match self {
            Format::Pdf => "pdf",
            Format::Docx => "docx",
        }
    }

    pub fn from_path(path: &Path) -> Option<Self> {
        match path
            .extension()?
            .to_str()?
            .to_ascii_lowercase()
            .as_str()
        {
            "pdf" => Some(Format::Pdf),
            "docx" => Some(Format::Docx),
            _ => None,
        }
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
    pub stats: dconv_pdf::ReadStats,
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

        match (from, to) {
            (Format::Pdf, Format::Docx) => self.pdf_to_docx(input, output, sink),
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
