//! PDF reader.
//!
//! Turns a PDF into the document IR in two stages: positioned-glyph extraction
//! through PDFium, then layout analysis over the geometry. The two are kept in
//! separate modules because they have completely different failure modes —
//! extraction fails when the engine misreads a font, analysis fails when the
//! page's implied structure is ambiguous — and mixing them makes a bug in one
//! look like a bug in the other.
//!
//! Filled in next; the crate exists now so the workspace and the DOCX writer can
//! be built and tested independently of PDFium.

pub use dconv_ir as ir;
