<h1 align="center">docconvert</h1>

<p align="center">
  <a href="#licences"><img alt="License: MIT OR Apache-2.0" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg"></a>
  <img alt="Rust 1.92+" src="https://img.shields.io/badge/rust-1.92%2B-orange.svg?logo=rust&logoColor=white">
  <img alt="Platform: Windows" src="https://img.shields.io/badge/platform-windows-lightgrey.svg?logo=windows&logoColor=white">
  <img alt="Version: 0.1.0" src="https://img.shields.io/badge/version-0.1.0-orange.svg">
  <img alt="Status: in development" src="https://img.shields.io/badge/status-in%20development-orange.svg">
</p>

---

docconvert turns **text-based PDFs into Word documents**. It reads the glyphs a
PDF actually stores, groups them into lines, paragraphs, headings, lists and
tables, and writes a real `.docx` — no screenshotting, no temporary HTML, no
cloud conversion.

It ships as one engine with two front ends: a command-line tool (`dconv`) and a
desktop app (`dconvert`). Both call the same conversion facade, so they produce
byte-identical output.

> [!NOTE]
> **Ongoing development**
> The PDF reader, DOCX writer, engine facade, CLI, GUI and packaging step all
> work, and the workspace's 100 tests pass. Input formats beyond PDF and targets
> beyond DOCX are planned.

> [!NOTE]
> **Status: in development (`0.1.0`)**
> This is early software. Command-line flags, the GUI's settings format and the
> shape of the Doc IR may change between versions, without a compatibility
> guarantee.

> [!IMPORTANT]
> **A personal project, not a supported product.** It is built for the author's
> own use. Anyone is welcome to download, build and use it under the licences
> below — but do not expect long-term support: there is no release schedule, no
> promise of fixes or replies to issues, and no guarantee that a later version
> stays compatible with anything.

> [!WARNING]
> **Windows x64 only, and text-based PDFs only.** The bundled PDFium is a
> win-x64 build, and there is no OCR: a scanned PDF has no text to extract.

## Contents

- [Why docconvert](#why-docconvert)
- [How it works](#how-it-works)
- [Requirements](#requirements)
- [Quick start](#quick-start)
- [Command-line reference](#command-line-reference)
- [Package a release](#package-a-release)
- [Files and paths](#files-and-paths)
- [Testing](#testing)
- [Continuous integration](#continuous-integration)
- [Fidelity](#fidelity)
- [Known limitations](#known-limitations)
- [Roadmap](#roadmap)
- [Contributing](#contributing)
- [Licences](#licences)

## Why docconvert

**One engine, two front ends.** The CLI and the GUI own no conversion logic. They
differ only in how they collect inputs and show progress, which is what makes
their output identical rather than merely similar.

**Real parallelism, the safe way.** PDFium binds once per *process*, so a second
bind in the same process fails. The CLI's `-j` and the GUI's worker count
therefore run a pool of worker **processes**, each re-invoked from the same
executable — never a pool of threads.

**One file to run.** Each executable carries PDFium inside it and unpacks it into
a version-keyed cache on first run. Copy the `.exe` anywhere and it works, even on
a machine that has never seen PDFium.

**Output you can diff.** Conversion is byte-for-byte reproducible, and the
committed fixtures are checked against committed `.docx` goldens, so a writer
that picks up a clock or an unordered iteration fails the build.

**Honest about what it drops.** Characters PDFium cannot map (mostly a real
hyphen becoming `U+0002`) are counted and reported, never guessed at. An omission
is a visible gap; a wrong character is invisible corruption.

## How it works

A PDF has no paragraphs — it has glyphs at coordinates. The reader is therefore
three stages:

```
PDF ──▶ positioned glyphs ──▶ bands and segments ──▶ Doc IR ──▶ .docx
        glyph.rs              layout.rs, analyze.rs           docx
```

1. **Glyphs.** Pull every positioned glyph and measure the page's own space width.
2. **Bands and segments.** Sort glyphs baseline-descending and group them into
   **bands** (one visual baseline), then split a band into **segments** at a
   column-sized gap. A superscript has a higher baseline than the text it
   annotates, so it is seen first; a band immediately re-anchors when a taller
   base glyph arrives. That is why `2ème` stays one line.
3. **Classification.** Size, spacing and segment structure turn bands into
   paragraphs, headings, list items and table rows, then into Doc IR blocks.

The Doc IR is the contract between readers and writers. Adding a format means
writing one side of the pipeline, not every pairing. The measurements behind the
analyser's less obvious constants are recorded in
[`docs/phase0-findings.md`](docs/phase0-findings.md).

## Requirements

- **Windows x64.**
- **Rust 1.92 or newer** (edition 2024).
- **PowerShell** for the fetch and packaging scripts.
- **Python 3 with PyMuPDF and python-docx** only if you want to regenerate the
  fixtures or run the fidelity harnesses.

PDFium is gitignored and fetched once from the
[bblanchon/pdfium-binaries](https://github.com/bblanchon/pdfium-binaries)
`chromium/8100` build.

## Quick start

### 1. Fetch PDFium

```powershell
.\scripts\fetch-pdfium.ps1
```

This must happen **before** the build that ships: `engine/pdf/build.rs` embeds the
library only if it is present at compile time. The fetch script touches
`build.rs` and the library afterwards, so cargo cannot miss a library that
arrived after the previous build.

### 2. Build

```powershell
cargo build --profile spike
```

`--profile spike` is release codegen without fat LTO: fast enough to use while
developing, quick enough to relink. Use `--profile release` for the deliverable.

### 3. Convert

```powershell
target\spike\dconv.exe report.pdf               # report.docx beside it
target\spike\dconv.exe -j 4 *.pdf -o out\       # batch, four worker processes
target\spike\dconv.exe --dump report.pdf        # block tree, no conversion
target\spike\dconvert.exe                       # desktop GUI
```

The GUI takes drag & drop (folders included), add-files/add-folder dialogs, a job
queue with per-job progress, cancel-all, a details/log/warnings tab strip, a
settings panel and persisted settings. It only offers formats the facade can
actually deliver.

### 4. Test

```powershell
cargo test --profile spike
```

## Command-line reference

For a quick look without leaving the terminal:

```powershell
target\spike\dconv.exe --help
```

| Option | What it does |
| --- | --- |
| `[INPUT]...` | Files to convert. |
| `-o, --output <PATH>` | Output file, or an output directory when converting several files. Default: beside each source. |
| `-j, --jobs <N>` | Convert this many files at once, each in a worker process. `1` (default) stays in-process; `0` asks for one worker per core, up to 8, never more than the number of files. |
| `--pdfium <DIR>` | Directory holding `pdfium.dll`, overriding the built-in search. |
| `-v, --verbose` | Show per-page progress while reading. |
| `--dump` | Print the analysed block structure instead of converting. Never uses workers. |
| `-q, --quiet` | Print only errors. |

## Package a release

```powershell
.\scripts\package.ps1
```

`dist\` then holds `dconv.exe`, `dconvert.exe` and `LICENSES\`. The script
refuses to finish unless each executable **contains** the vendored library's
bytes: a converter that needs a `pdfium.dll` beside it is exactly the failure the
embed exists to prevent.

The deliverable is that folder, not one file. The notices have to be able to
travel with the binaries, and a folder is easier to hand on than a `--licenses`
flag.

## Files and paths

| Path | Written by | Contents |
| --- | --- | --- |
| `%LOCALAPPDATA%\docconvert\pdfium\8100\pdfium.dll` | either executable, first run | Unpacked copy of the embedded library. A hit here skips unpacking. |
| `dist\` | `scripts\package.ps1` | The deliverable: both executables and `LICENSES\`. Gitignored. |
| `target\spike\` | cargo | Development binaries. |
| `out\` | you, and the harnesses | Scratch conversion output. Gitignored. |
| `tests\fixtures\synthetic\` | `scripts\make_fixtures.py` | Seven committed fixtures, each with a golden `.docx`. |
| `tests\fixtures\private\` | you | The real documents behind the fidelity numbers. Gitignored. |

Settings, logs and output naming belong to the GUI; the engine writes exactly
where it is told.

## Testing

```powershell
cargo test --profile spike
```

The 100 tests cover the Doc IR, the PDF geometry and analyser, the DOCX writer,
the facade, the CLI's process pool and the GUI's engine protocol. Three are real
end-to-end runs over the committed fixtures:

- `engine/convert/tests/golden.rs` converts all seven and compares the bytes.
- `engine/cli/tests/pool.rs` converts every fixture serially **and** pooled and
  compares each other and the goldens.
- `app/tests/worker_golden.rs` does the same through a real GUI worker child.

Two goldens (`table-wrapped`, `columns`) deliberately pin output that is **known
to be wrong**, so fixing either shows up as a diff rather than a surprise.
Re-bless a deliberate output change with `$env:DCONV_BLESS = "1"` and read
`git diff tests/fixtures/synthetic` before committing — a blessing nobody read is
a test that has quietly stopped testing anything.

## Continuous integration

[`.github/workflows/ci.yml`](.github/workflows/ci.yml) runs on every push to
`main` and every pull request. On a Windows runner it:

1. fetches PDFium, because the tests cannot bind without it;
2. builds the whole workspace and runs the full test suite with the lockfile
   enforced (`--locked`);
3. fails if the committed goldens changed (nobody should re-bless in CI);
4. runs the packaging step, including the check that both executables carry
   PDFium's bytes;
5. hashes every file in `dist\LICENSES\` against its upstream copy, so the
   "verbatim notice" claim is checked rather than asserted.

The workflow does not publish anything. There is no installer and no automated
release; `package.ps1` is the whole release process today.

## Fidelity

Measured by reading the text back out of the produced `.docx` and comparing it
with MuPDF (PyMuPDF), an independent implementation:

| Document | Pages | Word error | Similarity | Missing | Extra |
| --- | --- | --- | --- | --- | --- |
| `3IRS_TP2.pdf` | 5 | **1.28 %** | 99.21 % | 1 | 1 |
| `TP2 Admin BD 2026_2027.pdf` | 6 | 6.92 % | 96.34 % | 3 | 3 |

Missing and extra tokens are the order-independent measure and are the honest
one; word error rate depends on MuPDF's own block order, which is not guaranteed
to be reading order. Both documents' token counts match the reference exactly, so
every other difference is ordering; the missing and extra tokens are the `U+0002`
hyphen PDFium produces for some real hyphens.

These documents are **not in the repository** — `tests/fixtures/private/` is
gitignored — so the numbers cannot be reproduced from a clone without dropping
the same PDFs in there. The synthetic fixtures are a different instrument: seven
small generated documents that pin bytes end to end.

## Known limitations

- **Multi-column reading order.** Text is read across columns instead of down
  each one on multi-column pages. This is the largest fidelity gap, and
  `columns.pdf`'s golden pins the current behaviour on purpose.
- **Fully wrapped table rows.** When every cell of a row wraps, the second line
  fills exactly the same columns as the first; without reading the rulings there
  is nothing geometric left to tell it from a second row.
- **Heading inference.** Size alone cannot distinguish a 12 pt note box from a
  heading in an 11 pt document, so some emphasis becomes spurious headings. It
  costs no text.
- **No de-hyphenation.** Joining a trailing hyphen wrongly would corrupt two
  words where an omission is a visible gap.
- **Dropped code points are reported, never guessed.** The CLI prints a
  histogram of characters PDFium could not map.
- **Images become `[alt]` placeholder paragraphs**; footnote references become
  superscript `[label]` with no note body; lists use literal markers and a
  hanging indent rather than Word numbering.
- **Only PDF → DOCX is wired up.** The settings list comes from the facade's
  capability query, so the GUI cannot offer a format `convert` would refuse.

## Roadmap

| Stage | What | Progress |
| --- | --- | --- |
| 0 | PDF reader, Doc IR, DOCX writer | ✅ |
| 1 | Facade, `dconv` CLI, fidelity harnesses | ✅ |
| 2 | `dconvert` GUI, worker-process pools, packaging | ✅ |
| 3 | Multi-column reading order | ⬜ |
| 4 | Table rows whose every cell wraps (read the rulings) | ⬜ |
| 5 | Heading inference from spacing, not size alone | ⬜ |
| 6 | More readers and writers: DOCX, PPTX, XLSX, ODT, HTML, MD, TXT | ⬜ |
| 7 | An ETA in the GUI's top bar, from per-page progress | ⬜ |
| 8 | Superscript bounds compared on baselines, not box bottoms | ⬜ |
| 9 | Rust crate notices collected with `cargo-about` | ⬜ |

## Contributing

This is a personal project, and bug reports, suggestions and documentation fixes
are welcome — just without a support promise. For anything substantial, open an
issue first; a pull request that explains *why* is worth more than one that only
shows the diff. Every analyser bug should arrive with a regression test built
from the geometry that produced it.

`AGENTS.md` documents the architecture, the measured reasons behind the
analyser's constants and the project's conventions. The engine (`engine/`) and
GUI (`app/`) have separate owners and are not edited across that line.

## Licences

**The project itself** is dual-licensed under **MIT OR Apache-2.0**, at your
option, as declared in the workspace manifest (`Cargo.toml`,
`[workspace.package] license`). The repository does not yet contain the two
licence texts; until it does, that manifest line is the statement of the
project's terms.

### Third-party notices

The binaries contain **PDFium**, the PDF renderer by The PDFium Authors, version
**8100**, taken from the `chromium/8100` build published by
[bblanchon/pdfium-binaries](https://github.com/bblanchon/pdfium-binaries). PDFium
is distributed under the BSD 3-Clause licence. It is embedded rather than
statically linked (its Windows import library cannot be linked statically), and
each executable unpacks its copy on first run.

`scripts/package.ps1` writes `dist\LICENSES\` beside the executables: the notices
travel with the binaries because binary redistribution requires them to. The
files are copied unmodified from `third_party/pdfium/`:

| What | Licence | File in `LICENSES\` |
| --- | --- | --- |
| PDFium (The PDFium Authors) | BSD 3-Clause | `pdfium.txt` |
| The pdfium-binaries repackaging (Benoit Blanchon) | MIT | `pdfium-binaries.txt` |
| Components PDFium bundles | each notice states its own | the other 16 files |

Those components are Abseil, Anti-Grain Geometry, Dragonbox (Boost and
Apache-2.0-with-LLVM-exception), fast_float, FreeType, HarfBuzz, ICU, Little CMS,
libjpeg-turbo, OpenJPEG, libpng, LLVM libc, simdutf and zlib. The authoritative
terms are the notice files themselves, not this summary. `LICENSES\README.txt`
is generated by the packaging script and explains the contents.

**Not collected yet:** the notices for the Rust crates the executables link
(egui, typst, serde and the rest). `Cargo.lock` resolves 698 packages across all
platforms and targets; the Windows subset alone is too large to write by hand.
Anyone redistributing the binaries needs that set as well — `cargo-about` is the
intended way to produce it. The generated `LICENSES\README.txt` says so where a
redistributor will read it.
