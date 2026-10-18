//! Locating and binding PDFium.
//!
//! PDFium's C API is process-global: the library can only be initialised once
//! per process, and a second attempt fails with
//! `PdfiumLibraryBindingsAlreadyInitialized`. That is not an inconvenience to
//! work around, it is the architectural constraint that decides how this engine
//! parallelises at all — real concurrency needs *worker processes*, not threads,
//! and that decision propagates all the way up to the batch UI.
//!
//! So [`bind`] is documented as call-once-per-process and the callers that need
//! it repeatedly (the CLI, the GUI) bind once and pass the result down.

use crate::PdfError;
use pdfium_render::prelude::*;
use std::path::{Path, PathBuf};

/// The PDFium build this engine is pinned to.
///
/// Kept in step with `scripts/fetch-pdfium.ps1`. It names the cache directory,
/// so a stale binary from an older fetch can never be loaded by accident.
pub const PDFIUM_VERSION: &str = "8100";

/// Directories searched for `pdfium.dll`, in order.
///
/// The order encodes intent: an explicit override wins, then the directory the
/// executable lives in (which is what a portable single-file build ships
/// alongside, and what the extracted cache looks like), then the vendored copy
/// in a development tree, then the per-user cache.
fn search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    for variable in ["DCONV_PDFIUM_DIR", "PDFIUM_DLL_DIR"] {
        if let Some(value) = std::env::var_os(variable) {
            if !value.is_empty() {
                dirs.push(PathBuf::from(value));
            }
        }
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            dirs.push(parent.to_path_buf());
        }
    }

    // Only meaningful when running from a development tree; harmless otherwise.
    dirs.push(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../third_party/pdfium/bin"),
    );

    if let Some(cache) = cache_dir() {
        dirs.push(cache);
    }

    dirs
}

/// The per-user cache a packaged build extracts its embedded copy of PDFium
/// into, keyed by version so that upgrading the engine cannot silently reuse an
/// older binary.
pub fn cache_dir() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("XDG_CACHE_HOME"))
        .or_else(|| std::env::var_os("HOME"))?;

    let mut path = PathBuf::from(base);
    if std::env::var_os("HOME").is_some() && std::env::var_os("LOCALAPPDATA").is_none() {
        path.push(".cache");
    }
    path.push("docconvert");
    path.push("pdfium");
    path.push(PDFIUM_VERSION);
    Some(path)
}

fn library_in(dir: &Path) -> Option<PathBuf> {
    let candidate = PathBuf::from(Pdfium::pdfium_platform_library_name_at_path(dir));
    candidate.is_file().then_some(candidate)
}

/// Bind PDFium, searching the standard locations.
///
/// Call at most once per process; see the module comment.
pub fn bind() -> Result<Pdfium, PdfError> {
    let mut searched = Vec::new();

    for dir in search_dirs() {
        if let Some(library) = library_in(&dir) {
            let bindings = Pdfium::bind_to_library(&library).map_err(|error| PdfError::Bind {
                library: library.display().to_string(),
                reason: error.to_string(),
            })?;
            return Ok(Pdfium::new(bindings));
        }
        searched.push(dir.display().to_string());
    }

    Err(PdfError::LibraryNotFound { searched })
}

/// Bind from an explicit directory. Used by the fetch script's smoke test and by
/// callers that ship PDFium somewhere unusual.
pub fn bind_from(dir: &Path) -> Result<Pdfium, PdfError> {
    let library = library_in(dir).ok_or_else(|| PdfError::LibraryNotFound {
        searched: vec![dir.display().to_string()],
    })?;

    let bindings = Pdfium::bind_to_library(&library).map_err(|error| PdfError::Bind {
        library: library.display().to_string(),
        reason: error.to_string(),
    })?;

    Ok(Pdfium::new(bindings))
}
