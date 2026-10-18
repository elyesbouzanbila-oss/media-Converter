//! Conversion facade.
//!
//! The single entry point both the CLI and the GUI call, so that "what a
//! conversion does" is defined in exactly one place. Filled in once the reader
//! and writer are both in place.

pub use dconv_ir as ir;
