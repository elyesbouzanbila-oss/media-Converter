//! Embeds the vendored PDFium into this crate's binaries.
//!
//! PDFium cannot be linked statically — the library the vendor ships,
//! `lib/pdfium.dll.lib`, is an *import* library — so the only way to deliver a
//! converter that is one file is to carry the library's bytes inside the program
//! and write them out on first run. That is what this script arranges:
//!
//! * The vendored library is present → the crate is compiled with
//!   `cfg(embedded_pdfium)` and gains an `include_bytes!` over it.
//! * It is absent → the crate still builds, and [`dconv_pdf::bind::embedded`]
//!   answers `None`. A fresh clone that has not run `scripts/fetch-pdfium.ps1`
//!   yet is in exactly that state, and must not fail to link because of it.
//!
//! There is deliberately no way to point this at a different library. What ships
//! is then the same file every test in this repository binds against, which is
//! worth more than the flexibility — a second source of bytes would make "the
//! tests passed, but on a different library" a possibility, and this project has
//! no way to notice that.

use std::path::Path;

/// Where `scripts/fetch-pdfium.ps1` leaves the library, relative to this crate.
const VENDORED: &str = "../../third_party/pdfium/bin/pdfium.dll";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let vendored = Path::new(env!("CARGO_MANIFEST_DIR")).join(VENDORED);

    // Watched even while it is missing, because the build that matters is the
    // first one after the fetch script creates it. Without this, a tree that was
    // built before the fetch keeps its PDFium-less binary, and the symptom is a
    // program that cannot find a library that is sitting right there.
    println!("cargo:rerun-if-changed={}", vendored.display());

    if !vendored.is_file() {
        println!(
            "cargo:warning=PDFium was not found at {}, so this build carries no copy of its own \
             and will need one on disk. Run scripts/fetch-pdfium.ps1 to vendor it.",
            vendored.display()
        );
        return;
    }

    let size = std::fs::metadata(&vendored).map(|m| m.len()).unwrap_or(0);
    if size == 0 {
        println!(
            "cargo:warning={} is empty; not embedding it",
            vendored.display()
        );
        return;
    }

    println!("cargo:rustc-cfg=embedded_pdfium");
    println!("cargo:rustc-env=DCONV_PDFIUM_EMBEDDED={}", portable(&vendored));
}

/// The path as the compiler should see it inside an `include_bytes!` argument.
///
/// Forward slashes, because a Windows path is otherwise full of backslashes and
/// that is one more layer of escaping between a file that exists and a build that
/// insists it does not.
fn portable(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}
