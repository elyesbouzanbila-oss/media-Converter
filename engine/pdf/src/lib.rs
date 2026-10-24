//! PDF reader.
//!
//! Turns a PDF into the document IR in two stages, deliberately kept in separate
//! modules because they fail in unrelated ways:
//!
//! * [`glyph`] and [`layout`] read *geometry* from PDFium: where each character
//!   is, and which characters belong together. These fail when the engine
//!   misreads a font or a producer lies about coordinates.
//! * [`analyze`] infers *structure*: paragraphs, headings, lists, tables. This
//!   fails when the page's implied structure is genuinely ambiguous — which is
//!   normal, because a PDF does not record paragraphs. It records glyphs.
//!
//! Mixing the two makes a misread font look like a layout bug and vice versa,
//! which is the single most expensive thing that can happen while tuning
//! heuristics. So the split is enforced by module boundaries and by the fact
//! that everything from scratch-space upward is testable with no PDF at all.
//!
//! Two passes over the document are unavoidable. Classifying a line as a heading
//! means comparing it against the document's body size, and the body size is a
//! fact about the whole document, so every page is grouped before any block is
//! built.
//!
//! Not handled yet, deliberately: rotated pages (the geometry is read in page
//! space, so a page with a `/Rotate` would come out sideways), OCR of scanned
//! pages, images, and PDFium's own paragraph reconstruction.

pub mod analyze;
pub mod bind;
pub mod glyph;
pub mod layout;
pub mod rules;

pub use bind::{bind, bind_from};
pub use dconv_ir as ir;

/// Re-exported so callers can name the engine handle without depending on
/// pdfium-render directly, and so that replacing the binding crate later does
/// not ripple out into the CLI and the UI.
pub use pdfium_render::prelude::Pdfium;

use pdfium_render::prelude::*;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum PdfError {
    #[error("PDFium was not found. Looked in:\n  {}", .searched.join("\n  "))]
    LibraryNotFound { searched: Vec<String> },

    #[error("PDFium at {library} could not be bound: {reason}")]
    Bind { library: String, reason: String },

    #[error("this build carries no copy of PDFium to unpack")]
    NoEmbeddedLibrary,

    #[error("{context}: {source}")]
    Unpack {
        context: String,
        #[source]
        source: std::io::Error,
    },

    #[error("{context}: {source}")]
    Engine {
        context: String,
        #[source]
        source: PdfiumError,
    },

    #[error("the document has no pages")]
    NoPages,

    #[error("the conversion was cancelled")]
    Cancelled,
}

/// Progress and cancellation for a document in flight.
///
/// A trait rather than a closure because cancellation has to travel back from
/// the caller — the GUI wants a cancel button, and killing the thread is not an
/// option because PDFium's state is process-global. One interface serves the
/// CLI's progress line and the GUI's progress bar, which is what keeps their
/// output identical.
pub trait Sink {
    /// Called after each page is grouped. `done` counts pages finished.
    fn page(&mut self, done: usize, total: usize) {
        let _ = (done, total);
    }

    /// Checked between pages. Returning true abandons the conversion.
    ///
    /// Only ever consulted between pages: the expensive per-page work is not
    /// interrupted, because a half-read page would leave PDFium's iterator in
    /// an undefined position.
    fn cancelled(&self) -> bool {
        false
    }
}

/// A sink that discards everything.
pub struct Silent;

impl Sink for Silent {}

impl PdfError {
    fn engine(context: impl Into<String>, source: PdfiumError) -> Self {
        Self::Engine {
            context: context.into(),
            source,
        }
    }
}

/// What the reader found, for reporting and for spotting regressions.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReadStats {
    pub pages: usize,
    pub glyphs: usize,
    pub bands: usize,
    /// PDFium-synthesised separators discarded for having no geometry.
    pub synthetic_dropped: usize,
    /// Discarded code points, by frequency. See
    /// [`glyph::Extraction::dropped_codepoints`].
    pub dropped_codepoints: std::collections::BTreeMap<u32, usize>,
}

impl ReadStats {
    pub fn control_dropped(&self) -> usize {
        self.dropped_codepoints.values().sum()
    }
}

/// Read a PDF, binding PDFium first.
///
/// Binding is process-global and can only happen once, so a caller converting
/// more than one file must use [`read_with`] and hold one [`Pdfium`].
pub fn read(path: &Path) -> Result<(ir::Document, ReadStats), PdfError> {
    let pdfium = bind::bind()?;
    read_with(&pdfium, path)
}

/// Read a PDF with an already-bound engine.
pub fn read_with(pdfium: &Pdfium, path: &Path) -> Result<(ir::Document, ReadStats), PdfError> {
    read_tracked(pdfium, path, &mut Silent)
}

/// Read a PDF, reporting progress page by page and honouring cancellation.
pub fn read_tracked(
    pdfium: &Pdfium,
    path: &Path,
    sink: &mut dyn Sink,
) -> Result<(ir::Document, ReadStats), PdfError> {
    let document = pdfium
        .load_pdf_from_file(path, None)
        .map_err(|error| PdfError::engine(format!("opening {}", path.display()), error))?;

    let metadata = read_metadata(&document);

    let mut stats = ReadStats::default();
    let mut page_bands: Vec<Vec<layout::Band>> = Vec::new();
    let mut page_rules: Vec<Vec<rules::Rule>> = Vec::new();
    let mut page_setup = ir::PageSetup::default();
    let total = document.pages().len() as usize;

    for (index, page) in document.pages().iter().enumerate() {
        if sink.cancelled() {
            return Err(PdfError::Cancelled);
        }

        let extraction = glyph::extract_page(&page)
            .map_err(|error| PdfError::engine(format!("reading page {}", index + 1), error))?;
        // Read before the blocks are built, because a table's row grouping
        // needs the rulings; like the glyphs, they are page-level geometry.
        let rulings = rules::extract(&page);

        stats.glyphs += extraction.glyphs.len();
        stats.synthetic_dropped += extraction.synthetic_dropped;
        for (codepoint, count) in extraction.dropped_codepoints {
            *stats.dropped_codepoints.entry(codepoint).or_default() += count;
        }

        if index == 0 {
            page_setup = ir::PageSetup {
                width: ir::Length::pt(page.width().value as f64),
                height: ir::Length::pt(page.height().value as f64),
                ..Default::default()
            };
        }

        // The space width is measured per page, not per document: pages of one
        // file routinely come from different producers, and a document-wide
        // median would be wrong for every page but the majority.
        let space_em = glyph::measure_space_em(&extraction.glyphs);
        let bands = layout::analyse_page(&extraction.glyphs, space_em);
        stats.bands += bands.len();
        page_bands.push(bands);
        page_rules.push(rulings);

        sink.page(index + 1, total);
    }

    stats.pages = page_bands.len();
    if stats.pages == 0 {
        return Err(PdfError::NoPages);
    }

    let profile = analyze::DocumentProfile::from_bands(&page_bands);

    let mut blocks = Vec::new();
    for (index, bands) in page_bands.iter().enumerate() {
        // Page breaks are explicit because the IR has no per-page sections: the
        // writer emits one section property for the whole document, so the only
        // way to preserve where the pages actually fell is to say so.
        if index > 0 {
            blocks.push(ir::Block::PageBreak);
        }
        blocks.extend(analyze::build_page(
            bands,
            &page_rules[index],
            &profile,
            &page_setup,
        ));
    }

    let mut out = ir::Document {
        metadata,
        page: page_setup,
        blocks,
        footnotes: Vec::new(),
    };
    // Collapses the run-level styling into something a writer can consume
    // without re-deriving it.
    out.normalise();

    Ok((out, stats))
}

fn read_metadata(document: &PdfDocument) -> ir::Metadata {
    let source = document.metadata();

    let tag = |tag_type: PdfDocumentMetadataTagType| -> Option<String> {
        let value = source.get(tag_type)?.value().trim().to_string();
        (!value.is_empty()).then_some(value)
    };

    ir::Metadata {
        title: tag(PdfDocumentMetadataTagType::Title),
        author: tag(PdfDocumentMetadataTagType::Author),
        subject: tag(PdfDocumentMetadataTagType::Subject),
        description: tag(PdfDocumentMetadataTagType::Producer),
        created: tag(PdfDocumentMetadataTagType::CreationDate),
        // Left unset: the document says nothing about it, and a wrong guess
        // changes how Word proofreads and hyphenates the whole file.
        language: None,
        keywords: source
            .get(PdfDocumentMetadataTagType::Keywords)
            .map(|tag| {
                tag.value()
                    .split([';', ','])
                    .map(|keyword| keyword.trim().to_string())
                    .filter(|keyword| !keyword.is_empty())
                    .collect()
            })
            .unwrap_or_default(),
    }
}
