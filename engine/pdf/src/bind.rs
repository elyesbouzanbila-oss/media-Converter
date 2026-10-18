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
//!
//! # Where the library comes from
//!
//! [`bind`] tries, in order: an override and the executable's own directory, the
//! vendored copy in a development tree, and the per-user cache. If none of them
//! has the library, and this build carries its own copy inside the executable,
//! [`unpack_embedded`] takes that copy out and leaves it in the cache for every
//! run after the first.
//!
//! That last step is what makes a single-file build possible at all. PDFium
//! cannot be linked statically, so a build that ships as one file has to carry
//! the library as bytes and produce it on the machine that runs it.

use crate::PdfError;
use pdfium_render::prelude::*;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// The PDFium build this engine is pinned to.
///
/// Kept in step with `scripts/fetch-pdfium.ps1`. It names the cache directory,
/// so a stale binary from an older fetch can never be loaded by accident.
pub const PDFIUM_VERSION: &str = "8100";

/// Directories searched for `pdfium.dll`, in order.
///
/// The order encodes intent: an explicit override wins, then the directory the
/// executable lives in, then the vendored copy in a development tree, then the
/// per-user cache — which is where [`unpack_embedded`] puts this build's own
/// copy, so a packaged run finds the library there from the second start onward.
/// Only when all of these are empty does [`bind`] unpack anything.
fn search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    for variable in ["DCONV_PDFIUM_DIR", "PDFIUM_DLL_DIR"] {
        if let Some(value) = non_empty_var(variable) {
            dirs.push(PathBuf::from(value));
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

    dirs.push(cache_dir());

    dirs
}

/// The per-user cache a packaged build unpacks its own copy of PDFium into,
/// keyed by version so that upgrading the engine cannot silently reuse an older
/// binary.
///
/// It always answers, where it used to return `None` when no home directory was
/// set: the directory is now also the *destination* of an unpack, and a build
/// that carries the library has to be able to say where to put it. A machine with
/// none of these variables — a service account, a stripped container — gets the
/// temporary directory, where the copy is unpacked afresh every run. That is a
/// worse experience than caching rather than a broken one, which is the right
/// way round for a last resort.
pub fn cache_dir() -> PathBuf {
    let base = if let Some(base) = non_empty_var("LOCALAPPDATA") {
        PathBuf::from(base)
    } else if let Some(base) = non_empty_var("XDG_CACHE_HOME") {
        // `XDG_CACHE_HOME` *is* the cache directory, while `HOME` needs `.cache`
        // pushed onto it. They are not interchangeable, and an `or_else` chain
        // over the two of them quietly produces `$XDG_CACHE_HOME/.cache`.
        PathBuf::from(base)
    } else if let Some(base) = non_empty_var("HOME") {
        PathBuf::from(base).join(".cache")
    } else {
        std::env::temp_dir()
    };

    base.join("docconvert").join("pdfium").join(PDFIUM_VERSION)
}

/// An environment variable's value, or nothing if it is unset or empty.
///
/// Empty is unset as far as these callers are concerned: `DCONV_PDFIUM_DIR=`
/// means "I have nothing to say", and a path built from an empty string is the
/// current directory, which would then be searched for a library.
fn non_empty_var(name: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}

/// The path the library for `dir` would be at, whether or not it is there.
///
/// `pdfium-render` spells the platform's naming rule (`pdfium.dll` here), which
/// is the one detail of the library's name that should not be written out again
/// here.
fn library_path_in(dir: &Path) -> PathBuf {
    PathBuf::from(Pdfium::pdfium_platform_library_name_at_path(dir))
}

fn library_in(dir: &Path) -> Option<PathBuf> {
    let candidate = library_path_in(dir);
    candidate.is_file().then_some(candidate)
}

fn bind_library(library: &Path) -> Result<Pdfium, PdfError> {
    let bindings = Pdfium::bind_to_library(library).map_err(|error| PdfError::Bind {
        library: library.display().to_string(),
        reason: error.to_string(),
    })?;

    Ok(Pdfium::new(bindings))
}

/// This build's own copy of PDFium, or `None` if it carries none.
///
/// Which of those it is was decided at compile time by `build.rs`, from whether
/// the vendored library was there when the crate was built; that file says why
/// there is no way to embed one from somewhere else. The `Option` is not
/// defensive tidiness: a build with no copy is the ordinary state of a fresh
/// clone, and it still has to compile.
#[cfg(embedded_pdfium)]
pub fn embedded() -> Option<&'static [u8]> {
    // Through an environment variable rather than a literal because the path is
    // machine-specific, and from `build.rs` because that is the only place that
    // knows where the fetch script puts things.
    Some(include_bytes!(env!("DCONV_PDFIUM_EMBEDDED")))
}

#[cfg(not(embedded_pdfium))]
pub fn embedded() -> Option<&'static [u8]> {
    None
}

/// Write this build's own copy of PDFium into `directory`, and return its path.
///
/// Idempotent, and safe to call from several threads or several processes at
/// once — which is not hypothetical. The CLI's pool binds once in every worker
/// process, and two batches run at the same time is an ordinary thing to do.
///
/// The completeness argument is the rename. The bytes go to a staging name that
/// only this call can have used, are flushed to the disk, and only then are
/// renamed onto the real name, which replaces whatever was there in one step
/// (measured, not assumed: `std::fs::rename` replaces an existing destination on
/// Windows, where it is `MoveFileEx` with `MOVEFILE_REPLACE_EXISTING`). So no
/// reader can open a half-written library, and a call that loses a race is not a
/// failure: the copy that beat it is complete by the same argument. Writing the
/// bytes straight over the real name would be the bug — two racing processes
/// would leave a file of exactly the right length with interleaved contents,
/// which is the one kind of damage the size check below cannot see.
///
/// The size is the whole check, deliberately. A hash would catch a same-length
/// corruption that this write cannot produce, at the cost of reading several
/// megabytes on every process start, every worker in a pool included.
pub fn unpack_embedded(directory: &Path) -> Result<PathBuf, PdfError> {
    let bytes = embedded().ok_or(PdfError::NoEmbeddedLibrary)?;
    let library = library_path_in(directory);

    if already_unpacked(&library, bytes.len()) {
        return Ok(library);
    }

    std::fs::create_dir_all(directory).map_err(|source| unpack_error(directory, source))?;

    // Unique per call rather than per process: threads share a process id, and
    // `create` on a shared staging name would truncate a file another thread is
    // halfway through writing.
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let name = library
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "pdfium".to_owned());
    let staging = directory.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    ));

    if let Err(source) = write_flushed(&staging, bytes) {
        let _ = std::fs::remove_file(&staging);
        return Err(unpack_error(&staging, source));
    }

    match std::fs::rename(&staging, &library) {
        Ok(()) => Ok(library),
        // On a rename within one directory, the only way this fails is the
        // destination being held open — a library another process has already
        // loaded and mapped, which Windows will not release. If a complete copy
        // is sitting there, it is the one that process is binding happily; if it
        // is not complete, the failure is real and worth reporting rather than
        // retrying in a loop.
        Err(source) => {
            let _ = std::fs::remove_file(&staging);

            if already_unpacked(&library, bytes.len()) {
                Ok(library)
            } else {
                Err(unpack_error(&library, source))
            }
        }
    }
}

fn write_flushed(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::File::create(path)?;
    file.write_all(bytes)?;

    // Flushed before the rename, so the name can never refer to bytes the disk
    // has not finished writing: without it a crash mid-unpack leaves a correctly
    // named, correctly sized, half-populated library, which is the one file the
    // size check above cannot see. The docx writer flushes before its own rename
    // for the same reason.
    file.sync_all()
}

/// Whether `path` holds an unpacked library of exactly `size` bytes.
fn already_unpacked(path: &Path, size: usize) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| metadata.len() == size as u64)
}

fn unpack_error(path: &Path, source: std::io::Error) -> PdfError {
    PdfError::Unpack {
        context: format!("unpacking this build's own PDFium at {}", path.display()),
        source,
    }
}

/// Bind PDFium, searching the standard locations and then unpacking the copy
/// this build carries, if it carries one.
///
/// Call at most once per process; see the module comment.
pub fn bind() -> Result<Pdfium, PdfError> {
    let mut searched = Vec::new();

    for dir in search_dirs() {
        if let Some(library) = library_in(&dir) {
            return bind_library(&library);
        }
        searched.push(dir.display().to_string());
    }

    // Nothing on disk anywhere. A packaged build carries its own copy, so the
    // last resort is to unpack it into the cache — which was one of the
    // directories just searched, so this happens once per machine rather than
    // once per run.
    let cache = cache_dir();
    match unpack_embedded(&cache) {
        Ok(library) => bind_library(&library),
        Err(error) => {
            searched.push(format!("{} — {error}", cache.display()));
            Err(PdfError::LibraryNotFound { searched })
        }
    }
}

/// Bind from an explicit directory. Used by the fetch script's smoke test and by
/// callers that ship PDFium somewhere unusual.
pub fn bind_from(dir: &Path) -> Result<Pdfium, PdfError> {
    let library = library_in(dir).ok_or_else(|| PdfError::LibraryNotFound {
        searched: vec![dir.display().to_string()],
    })?;

    bind_library(&library)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Where a development tree keeps the library, which is what `build.rs`
    /// embedded — when it embedded anything.
    fn vendored() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../third_party/pdfium/bin/pdfium.dll")
    }

    /// This build's own copy, or a failure saying what to do about it.
    ///
    /// Deliberately not a skip. A build with no vendored library tests nothing
    /// at all about the embed, and a green test that checks nothing is worse
    /// than a red one that says `run scripts/fetch-pdfium.ps1`.
    fn carried() -> &'static [u8] {
        embedded().expect(
            "this build carries no copy of PDFium. Either scripts/fetch-pdfium.ps1 has not been \
             run, or it has been and cargo decided nothing had changed - it compares the \
             library's timestamp against the last build's, and a file out of a tarball is older \
             than that. Touch engine/pdf/build.rs (the fetch script does), or run `cargo clean \
             -p dconv-pdf`, and build again.",
        )
    }

    fn scratch() -> tempfile::TempDir {
        tempfile::tempdir().expect("a scratch directory")
    }

    #[test]
    fn what_ships_is_the_library_every_test_binds_to() {
        let library = vendored();
        assert!(
            library.is_file(),
            "{} is missing; run scripts/fetch-pdfium.ps1",
            library.display()
        );

        let on_disk = std::fs::read(&library).expect("the vendored library reads");
        assert_eq!(
            carried().len(),
            on_disk.len(),
            "the copy inside this binary is a different length from the vendored library"
        );

        // Compared as a value, because `assert_eq!` on slices prints both of
        // them and several megabytes of mismatched bytes is not a test report.
        assert!(
            carried() == on_disk.as_slice(),
            "the copy inside this binary is not the vendored library: either something else was \
             embedded, or this build predates the last fetch"
        );
    }

    #[test]
    fn unpacking_puts_the_carried_library_where_it_was_asked_to() {
        let directory = scratch();
        let library = unpack_embedded(directory.path()).expect("unpacks to a fresh directory");

        assert_eq!(library, library_path_in(directory.path()));

        let unpacked = std::fs::read(&library).expect("the unpacked library reads");
        assert_eq!(unpacked.len(), carried().len());
        assert!(
            unpacked == carried(),
            "the unpacked copy is not the carried one"
        );
    }

    #[test]
    fn unpacking_again_uses_the_copy_already_there() {
        let directory = scratch();
        let first = unpack_embedded(directory.path()).expect("unpacks");
        let before = std::fs::metadata(&first)
            .and_then(|metadata| metadata.modified())
            .expect("a modification time");

        let second = unpack_embedded(directory.path()).expect("unpacks again");
        assert_eq!(first, second);

        // Not rewritten: the point of the size check is that every run after the
        // first reads the cache and does nothing else. `<=` rather than `==` so
        // that a coarse filesystem clock cannot make this flaky — a rewrite
        // would have to land inside the same tick to hide, and the two calls are
        // a filesystem round trip apart.
        let after = std::fs::metadata(&second)
            .and_then(|metadata| metadata.modified())
            .expect("a modification time");
        assert!(after <= before, "the second unpack rewrote the library");
    }

    #[test]
    fn a_damaged_cache_copy_is_repaired_rather_than_bound() {
        let directory = scratch();
        let library = unpack_embedded(directory.path()).expect("unpacks");

        // An interrupted write cannot leave this behind — that is what the
        // staging rename prevents — so it stands in for damage from outside,
        // which is what the size check is for. Trusting it would mean binding a
        // truncated library, and the failure would surface as a corrupt document
        // rather than as a missing cache entry.
        std::fs::write(&library, b"half a library").expect("damages the cache");

        let repaired = unpack_embedded(directory.path()).expect("repairs the cache");
        let bytes = std::fs::read(&repaired).expect("the repaired library reads");
        assert_eq!(bytes.len(), carried().len());
        assert!(bytes == carried(), "the damaged copy was left in place");
    }

    #[test]
    fn a_cache_copy_from_another_build_is_replaced() {
        let directory = scratch();
        let library = library_path_in(directory.path());

        // A stale copy is the one thing a version-keyed directory cannot rule
        // out: a different build of the same PDFium version writes to the same
        // place. Keeping it would leave the program broken until somebody
        // deleted a file they have never heard of.
        std::fs::write(&library, vec![0u8; carried().len() + 1]).expect("plants a stale copy");

        let replaced = unpack_embedded(directory.path()).expect("replaces the stale copy");
        let bytes = std::fs::read(&replaced).expect("the replaced library reads");
        assert!(bytes == carried(), "the stale copy was kept");
    }

    #[test]
    fn several_threads_can_unpack_at_once() {
        // Not hypothetical: the CLI and the GUI each bind once in every worker
        // *process* they start, and nothing orders those against a batch run
        // started by hand.
        let directory = scratch();

        let unpacked: Vec<PathBuf> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..4)
                .map(|_| scope.spawn(|| unpack_embedded(directory.path()).expect("unpacks")))
                .collect();

            handles
                .into_iter()
                .map(|handle| handle.join().expect("no thread panics"))
                .collect()
        });

        assert!(
            unpacked.windows(2).all(|pair| pair[0] == pair[1]),
            "the threads disagreed about where the library goes: {unpacked:?}"
        );

        let bytes = std::fs::read(&unpacked[0]).expect("the library reads");
        assert!(
            bytes == carried(),
            "racing unpacks produced something other than the carried library"
        );
    }

    #[test]
    fn the_cache_directory_is_keyed_by_version() {
        // The version in that path is the whole reason an upgrade cannot load
        // the previous library, so it is worth asserting rather than assuming.
        assert!(cache_dir().ends_with(PDFIUM_VERSION));
    }
}
