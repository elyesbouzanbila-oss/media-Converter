# AGENTS.md

Guidance for any agent (or human) picking this repository up. Read this before
changing anything; it will save you from re-deriving decisions that already have
measurements behind them.

---

## TL;DR

- **Two owners, two directories.** The engine lives in `engine/`, the GUI lives in
  `app/`. Never edit across that line.
- The engine works: `dconv file.pdf` produces a real `.docx` at **1.28 % word
  error** on the primary test document. Everything is committed and 96 tests
  pass.
- **It ships as one file.** `pdfium.dll` is embedded in the executable and
  unpacked into a version-keyed cache on first run, so a build runs on a machine
  that has never seen PDFium. Measured end to end, not argued — but it is a
  property of a build made *after* the fetch, which cargo does not arrange by
  itself; see [the single-file check](#checking-the-single-file-claim).
- **Conversion is byte-for-byte reproducible, and that is now asserted.** Seven
  committed fixtures under `tests/fixtures/synthetic/` are converted end to end
  and checked against a committed `.docx` each, so a fresh clone can run a real
  end-to-end test. The *fidelity percentages* are the part that still needs the
  gitignored private documents.
- **The GUI converts.** The egui/eframe shell runs the real facade through a
  pool of worker *processes*; the demo pool is kept one swap away for
  PDFium-free UI review. The formats the UI offers still mirror the facade by
  hand — the facade's capability query has landed, and `app/src/engine.rs` has
  yet to switch to it. See [the GUI section](#the-gui-app).
- **The CLI has a pool too**, and it is where the byte-identical claim is
  asserted: `dconv -j 4 *.pdf` re-runs this same executable as four workers, and
  `engine/cli/tests/pool.rs` converts every fixture serially and in a pool and
  compares with each other *and* with the committed goldens.
- The real test PDFs are **gitignored**. Reproducing the numbers below needs
  somebody to drop the documents into `tests/fixtures/private/`.
- `docs/phase0-findings.md` is the decision record. Read the findings before
  touching the layout analyser — most of its odd-looking constants exist because a
  real document contradicted a plausible guess.

---

## The one rule that matters: two owners, two directories

This repository was built by two agents working in parallel, and the directory
split is the whole safety mechanism:

| Owner | Directory | Crate / binary |
| --- | --- | --- |
| Engine | `engine/**` | `dconv-*`, binary `dconv` |
| GUI | `app/**` | `dconv-app`, binary `dconvert` |

**Do not edit the other side's directory.** Not "be careful" — don't open the
files. The only shared surfaces are the workspace manifest, `docs/`, `scripts/`,
and the published API of `dconv-engine`. If you need something from the other
side, change the *contract* and say so; do not reach in.

The GUI is deliberately **not** matched by a glob in `Cargo.toml`
(`members = ["engine/*", "spikes/*", "app"]`). Cargo resolves every glob match
into a package, so a half-created UI directory would have broken the engine's
build. That is why the member list is explicit.

---

## Layout

```
engine/
  ir/          dconv-ir        the Doc IR: the contract between reader and writer
  pdf/         dconv-pdf       PDFium binding, glyph extraction, layout analysis;
                              build.rs embeds the vendored library in the binary
  docx/        dconv-docx      the writer (hand-built zip + XML)
  convert/     dconv-engine    the facade the CLI and GUI both call
  cli/         dconv-cli       the `dconv` binary
app/           dconv-app       the egui/eframe GUI, binary `dconvert`  (UI owner)
spikes/        pdf-extract, typst-pdf, pdf-pool   Phase 0 throwaway experiments
scripts/       fetch-pdfium.ps1, make_fixtures.py, compare_*.py, validate_docx.py
docs/          phase0-findings.md   the decision record
third_party/   pdfium/    (gitignored; fetched by script)
tests/fixtures/synthetic/ (committed) the fixtures, each with a golden .docx
tests/fixtures/private/   (gitignored) the real documents — the only source of
                          the fidelity numbers below
out/           (gitignored; scratch conversion output)
```

`spikes/` is history, not code to build on. They answered their questions; the
findings are what survived.

---

## Build, test, run

```powershell
cargo build  --profile spike            # whole workspace
cargo test   --profile spike            # whole workspace, 96 tests
cargo build  -p dconv-cli --profile spike
target\spike\dconv.exe --help
```

Two release profiles, and the distinction matters for iteration speed:

- `release` — `opt-level 3`, fat LTO, `codegen-units 1`, stripped. The deliverable.
- `spike` — inherits release but `lto = false`, not stripped. **Use this while
  developing**; the PDF work is CPU-heavy enough that debug builds are unusable,
  but full LTO costs minutes on every relink for nothing.

`[profile.dev.package."*"] opt-level = 2` keeps dependencies (typst, pdfium) sane
in a plain `cargo build`.

Bootstrap PDFium once — it is gitignored:

```powershell
.\scripts\fetch-pdfium.ps1
```

`engine/pdf/src/bind.rs` has `PDFIUM_VERSION = "8100"`, and this script is the
source of truth for it. Keep them in step; `pdfium-render 0.9.4` generates
bindings for the `chromium/7881` API, so newer drops are a safe superset and older
ones are not.

**Order matters for the deliverable.** The fetch has to happen before the build
that ships, because `engine/pdf/build.rs` embeds the vendored library only if it
is there when the crate is compiled; a tree built without it still compiles, but
the binary carries nothing and will need a `pdfium.dll` next to it.

The order is not enforced by cargo, and the case worth knowing about is a fetch
made *after* a build. Cargo decides whether to re-run a build script by comparing
the watched file's timestamp against the last build's, and `tar` restores the
archive's timestamps — so a library the fetch script has just vendored is *older*
than the build it has to invalidate, and the build after it still carries nothing.
Silently. Two things catch it, both measured rather than assumed: the fetch script
touches `engine/pdf/build.rs` and the library itself, dirtying the crate as a
source change cargo cannot miss; and `what_ships_is_the_library_every_test_binds_to`
fails, with the fix in the message, when the vendored library is on disk while
this build carries no copy of it. Vendoring PDFium by hand is the one path that
does neither — touch `engine/pdf/build.rs` or run `cargo clean -p dconv-pdf`
before building a release.

### Checking the single-file claim

What the tests cannot assert is that the *program* still works with no library on
disk anywhere: every bind on this machine finds the vendored copy first. (The
bytes are asserted — that is the seven tests in `bind.rs`.) So this is a
procedure, run when the claim matters (a release, or a change to `bind.rs`):

1. Rename `third_party/pdfium/bin/pdfium.dll` aside, and delete
   `%LOCALAPPDATA%\docconvert\pdfium\8100\` if it exists — the two ways the
   library could still be found on disk.
2. Copy `target\spike\dconv.exe` (or the release build) into an empty directory of
   its own, with no other file in it.
3. Run it over any fixture from there. It must convert, and it must leave a
   `pdfium.dll` in the cache directory, byte-identical to the one that was
   renamed aside.
4. Run it again: the second run must not rewrite the cached copy, because the
   cache is one of the searched directories and a hit skips unpacking entirely.
5. Put the vendored library back — and do not rebuild while it is aside, since a
   build in that window is a build with no embedded copy.

Step 3's output is worth diffing against the committed golden while the tree is in
that state: a converter that unpacks its library correctly and a converter that
converts correctly are two different claims.

What is *asserted* is the part with no environment in it: the bytes this build
carries are the vendored library's, byte for byte; unpacking is idempotent, repairs
a damaged cache copy, replaces one left by a different build, and tolerates four
threads doing it at once. Those are the seven tests in `bind.rs`.

---

## Architecture: the IR is the contract

```
PDF ──(reader)──▶ positioned glyphs ──(layout analyser)──▶ Doc IR ──(writer)──▶ DOCX
```

`engine/ir` defines the AST. Readers produce it, writers consume it. That is 2N
components instead of N×M — the pandoc model. Adding a format means writing one
side, not every pairing.

**PDF is the only format with a three-stage reader**, because a PDF has no
paragraphs: it has glyphs at coordinates. `dconv-pdf` therefore goes:

1. `glyph.rs` — pull positioned glyphs, measure the page's own space width.
2. `layout.rs` — group glyphs into **bands** (one visual baseline) and split each
   band into **segments** (runs separated by a column-sized gap).
3. `analyze.rs` — classify bands into paragraphs, headings, lists and tables, and
   build IR blocks.

The other formats the project will support (DOCX, PPTX, XLSX, ODT, HTML, MD, TXT)
read directly into IR with no analyser stage.

### Why the analyser looks strange

Grouping text by baseline is a **two-dimensional** problem, and almost every
non-obvious constant in `layout.rs` / `analyze.rs` is a scar from a real document.
Highlights (full detail in `docs/phase0-findings.md`, findings A1–A4, D1–D13):

- Bands are built by sorting glyphs **baseline-descending** and offering each
  glyph to the nearest existing band first (`BAND_SEARCH`), then re-anchoring a
  band when a taller glyph arrives. This exists because a superscript has a
  *higher* baseline and is therefore seen **before** the base text it annotates.
  Without it, `2ème` splits.
- The script test must be **symmetric** (`min < max * SCRIPT_SIZE`) and the raise
  must be bounded **both** against the base text (`SCRIPT_RAISE_EM`) and against
  the glyph's own height (`SCRIPT_RAISE_OWN`). One bound alone merges a bullet into
  the title above it, or an 11 pt line into a 24 pt heading.
- Word gaps use **loose (advance) boxes**, never tight (ink) boxes, and the space
  width is measured per page from the page's own `U+0020`.
- A band of nothing but whitespace is dropped, because whitespace has a size and a
  size above the body's reads as a heading (this produced an empty `H3` from a
  single 12 pt space).
- Line font size is the **character-weighted dominant** size, not the maximum.

### Deliberate v1 scope decisions — do not relitigate without reason

- **OCR is out of scope.** Text-based PDFs only. Leave the architectural hook; do
  not build it.
- **No de-hyphenation.** A trailing hyphen is ambiguous, and joining wrongly
  corrupts two words. Leave it.
- **Dropped code points are reported, never guessed.** PDFium maps some real
  `U+002D` hyphens to `U+0002`. The character is not recoverable, and a wrong
  character is invisible corruption where an omission is a visible gap. The reader
  keeps a histogram and the CLI prints it.
- **Lists use literal markers + hanging indent**, not `w:numPr`/`numbering.xml`.
- **Images emit a `[alt]` placeholder paragraph**; footnote refs become superscript
  `[label]` with no note body.
- **`w:sz` is emitted explicitly on every run.** `w:pPr/w:rPr` does *not* apply to
  runs.
- **Twips/EMU round half-away-from-zero**, not truncate.
- **Format dispatch is a closed `match`, not a plugin registry**, until there is a
  second reader or writer to register.
- **Explicit `Block::PageBreak` between pages**, not after the last.

---

## The engine facade

`engine/convert/src/lib.rs` is the only conversion entry point, for both front
ends. Neither the CLI nor the GUI owns any conversion logic — they differ only in
how they collect inputs and show progress. This is what makes byte-identical CLI
and GUI output achievable rather than aspirational.

```rust
let engine = Converter::bind()?;                     // binds PDFium (once per process)
let report = engine.convert(&input, &output, &mut sink)?;   // reader+writer by extension
let document = engine.analyse(&input, &mut sink)?;          // no write, for inspection
```

`Sink` (`engine/pdf/src/lib.rs`) is the progress *and cancellation* interface:

```rust
fn page(&mut self, done: usize, total: usize)   // after each page
fn cancelled(&self) -> bool                     // checked between pages only
```

A trait rather than a closure because cancellation has to travel back from the
caller, and killing a thread is not an option — PDFium's state is process-global.
Cancellation is deliberately only consulted **between** pages: interrupting one
mid-read would leave PDFium's iterator undefined.

### What the facade can do: ask it, do not mirror it

```rust
pub const CONVERSIONS: &[(Format, Format)] = &[(Format::Pdf, Format::Docx)];

pub fn targets_for(source: Format) -> Vec<Format>   // what convert accepts from it
pub fn readers() -> Vec<Format>                     // every source it reads
pub fn all_targets() -> Vec<Format>                 // the union a settings panel offers
```

`CONVERSIONS` is the list `Converter::convert` gates on, and every query is derived
from it, so a front end cannot offer a format the facade will refuse. The dispatch
`match` decides only *how*; the table decides *whether*. A test walks every pair of
known formats and fails if the two disagree, because a row with no match arm is a
promise that dies at the last moment — once per file, after the queue has been
accepted.

`Format::from_extension` exists for the same reason: a front end filters a queue by
extension string long before it has a path to hand `Format::from_path`.

Two types are re-exported so a front end need not depend on the reader crate:
`PdfError` (cancellation reaches a caller as `PdfError::Cancelled` through the
reader chain) and `ReadStats` (the diagnostics a conversion reports *are* one).

### Critical: PDFium binds once per *process*

`bind_to_library` routes through a process-global static; the **second bind in a
process fails** (finding A1). Consequences:

- One `Converter` per process, reused for a whole batch. Both front ends do this.
- Real parallelism therefore requires a **pool of worker processes**, not threads.
  Both front ends have one, built the same way: re-invoke the current executable
  with a hidden flag (`dconvert --dconvert-worker`, `dconv --dconv-worker`) and
  speak one JSON object per line over its pipes. Re-running itself is what keeps
  the deliverable one file, and it means a worker can never be a different build
  from the parent that spawned it.
- For single-file delivery the DLL is embedded with `include_bytes!` and unpacked
  into a version-keyed cache on first run — in `engine/pdf/build.rs` and
  `bind::unpack_embedded`. A build carries the copy only if the vendored library
  was present when it was compiled, so the fetch script must run *before* the
  build that ships. Static linking is not possible (`lib/pdfium.dll.lib` is an
  *import* library) — see the corrections section of the findings.

---

## The GUI (`app/`)

The egui/eframe 0.37 shell is complete (19 tests) and converts for real: the
facade runs in a pool of worker **processes**, not threads.

```
app/src/main.rs       eframe setup; the worker-mode gate; picks the engine
                      (real pool in production; demo swappable one line away)
app/src/engine.rs     the engine boundary: protocol types + the real
                      worker-process pool + capability list; `engine::demo`
                      holds the M0 demo pool for PDFium-free UI review
app/src/state.rs      jobs, queue, settings, output naming (egui-free, tested)
app/src/ui.rs         all drawing
app/src/app.rs        the eframe App: owns state, drains events, paints
app/src/log_ring.rs   bounded in-memory log backing the console tab
```

What it does: drag & drop (folders expanded recursively, bounded), add-files and
add-folder dialogs, a job queue table with per-job progress and control, cancel-all,
clear-finished, a details/log/warnings tab strip, a settings side panel (target
format, output folder, overwrite policy, worker count, theme), persisted settings,
a progress percentage in the window title, and an in-window log fed by `tracing`.
Formats are offered only as far as the facade can deliver: unsupported files are
refused at add time with a log line, not as failed jobs.

Two facts a new agent should not trip over:

- **egui 0.37 unified `SidePanel`/`TopBottomPanel` into `egui::Panel`** with
  `left/right/top/bottom` constructors, each `show` taking the parent `Ui`, and
  eframe's `App` trait method is `fn ui(&mut self, ui, frame)` — not the older
  `update(&mut self, ctx, frame)`. Copy the existing patterns.
- **Output naming policy lives in the UI**, not the engine. `resolve_output_path`
  runs once at batch start so the queue can show final names before anything runs.
  The engine must write **exactly** where it is told. Two same-named inputs from
  different folders must get distinct outputs.

### How a conversion actually runs

1. `main` checks for `--dconvert-worker` before any GUI code exists: the engine
   re-invokes this same binary as one worker child (hidden, `CREATE_NO_WINDOW`),
   which binds PDFium once and processes JSON lines from its stdin, emitting
   one JSON event line per UI event. Byte-identical CLI vs GUI output holds by
   construction here: the child calls `Converter::convert` exactly the way
   `dconv` does.
2. The parent dispatcher queues jobs, streams them into the children's stdin
   (the pool never idles), and owns the kill. `CancelAll` kills the processes,
   and a killed child's half-written output file is removed by the parent, so a
   cancelled job never leaves a broken document behind.
3. Per-job accounting is enforced twice: terminal lines deregister their job on
   the way through, and a child's leftover registry is reconciled at death —
   `Cancelled` if a cancel was unwinding, `Failed` otherwise. The UI's in-flight
   counter can therefore never strand, short of a lost engine channel.

Format honesty: the settings list and the accepted file extensions come from
`engine::supported(ext)` / `readable(path)` in `app/src/engine.rs`, which still
**mirror the facade's closed match by hand** (`Format::from_path` + the one
supported pair, PDF → DOCX). The facade's capability query
([above](#what-the-facade-can-do-ask-it-do-not-mirror-it)) has landed, so this can
become three one-line delegations to `targets_for` / `all_targets` / `readers`;
until then, **if the facade learns a new pair, the GUI offers nothing new until
this mirror is updated.**

### The demo engine

`engine::demo::spawn_demo_engine()` is a one-line swap in `main.rs` away from
production: a pool of threads that *sleep* instead of converting, the same shape
as the real pool so UI code cannot tell the difference. Demo sensitivities, so
every UI path is reviewable without PDFium: a file name containing `fail` fails,
one containing `warn` emits warnings, and runtimes derive from the file name so
batches are reproducible.

### The frozen protocol

Both implementations must honour `app/src/engine.rs`:

```rust
EngineCommand::Start { jobs: Vec<JobSpec>, workers: usize }
EngineCommand::CancelAll
EngineCommand::Shutdown

EngineEvent::Started   { id }
EngineEvent::Progress  { id, frac }                 // 0.0..=1.0
EngineEvent::Warning   { id, text }
EngineEvent::Finished  { id, output, elapsed }
EngineEvent::Failed    { id, summary, detail, elapsed }
EngineEvent::Cancelled { id }
```

Contract, in order of how easy it is to get wrong:

1. **Exactly one terminal event per job** (`Finished` / `Failed` / `Cancelled`). A
   job cancelled mid-flight is still a job that must be accounted for, or the UI's
   in-flight counter waits forever.
2. **Progress fractions are `0.0..=1.0`.**
3. The engine never blocks the UI thread; the UI drains events non-blocking once
   per frame and asks for repaints only while work is live.

---

## What the GUI needs from the engine (contract asks)

The adapter is done. Two of the four asks have landed; what remains is small.

1. ~~**Re-export `PdfError` and `ReadStats` from `dconv-engine`**~~ — **landed.**
   Both are re-exported alongside `Sink`, so a front end can match
   `PdfError::Cancelled` by variant instead of by display string, and can name
   `ReadStats` instead of taking the numbers apart as scalars. `warning_lines` in
   `app/src/engine.rs` still does the latter, and no longer has to.
2. ~~**A capability query** (`supported(ext) -> [targets]`)~~ — **landed**, as
   `targets_for` / `readers` / `all_targets` over the one `CONVERSIONS` table
   `convert` gates on. `engine::supported` can stop mirroring the closed match.
3. **Byte-identical CLI vs GUI output is the standing assertion** that no
   conversion logic leaked into a front end. The CLI half is now asserted on every
   fixture by `engine/cli/tests/pool.rs`: serial and pooled runs, compared with
   each other *and* with the committed goldens, so a mistake shared by both paths
   still fails. The GUI half is writable and still to be written — and easier than
   it looks, because the worker child already calls the exact
   `Converter::convert` call the CLI does. The test is "the worker's output equals
   the committed golden"; it needs no CLI involvement at all.
4. **Progress granularity is per page.** Fine as it is; extend `Sink` only if the
   UI genuinely needs more, keeping the CLI on the same interface or the outputs
   can drift.

---

## Measuring fidelity

The numbers this project quotes come from a harness that reads the text back **out
of the produced `.docx`**, not out of the IR, so a writer bug cannot hide behind
the reader that fed it. The reference is MuPDF (PyMuPDF), an independent
implementation.

```powershell
$env:PYTHONIOENCODING = "utf-8"          # or accented output is mangled
cargo build -p dconv-cli --profile spike
target\spike\dconv.exe "tests\fixtures\private\<doc>.pdf" -o "out\"
python scripts\compare_docx.py "out\<doc>.docx" "tests\fixtures\private\<doc>.pdf"
```

| Document | Pages | Word error | Similarity | Missing | Extra |
| --- | --- | --- | --- | --- | --- |
| `3IRS_TP2.pdf` | 5 | **1.28 %** | 99.21 % | 1 | 1 |
| `TP2 Admin BD 2026_2027.pdf` | 6 | 6.92 % | 96.34 % | 3 | 3 |

**Read the missing/extra columns, not the headline.** Word error rate is
order-sensitive and its reference is MuPDF's own block order, which is not
guaranteed to be reading order. Missing/extra tokens are order-*independent* and
are the honest measure of extraction. Both documents' token counts match the
reference exactly, so **every remaining difference is ordering except four
tokens** — and all four are the `U+0002` hyphen.

`scripts/textdiff.py` holds the single implementation of normalisation and token
reporting; both harnesses import it. Do not fork it.

The synthetic fixtures are a separate instrument that answers a different
question. `scripts/make_fixtures.py` generates them, so their geometry is stated
rather than inferred, and each one targets a rule that has a finding behind it.
`cargo test -p dconv-engine --profile spike` converts all seven and compares the
bytes with `tests/fixtures/synthetic/<name>.golden.docx`. Two of them
(`table-wrapped`, `columns`) pin output that is **known to be wrong**, on purpose:
the day either is fixed, it shows up as a diff rather than as a surprise. Re-bless
a deliberate change with `$env:DCONV_BLESS = "1"`, then read
`git diff tests/fixtures/synthetic` before committing — a blessing nobody read is
a test that has quietly stopped testing anything.

The fixtures are also the reason the byte-for-byte claim is now checkable. Nothing
asserts a *number* about them; the assertion is that the bytes do not change, which
is the only thing that catches a writer that has picked up a clock or an unordered
iteration.

Also available: `scripts/compare_text.py` (extractor-level diff, no DOCX involved),
`scripts/validate_docx.py` (independent structural validation with python-docx —
`--sample` adds assertions about the hand-written sample only), `scripts/cpu-scale.py`
(pool scaling).

A validator that fails on a valid document teaches you to ignore it. Keep
structural checks (always true of any `.docx` we write) separate from
sample-content assertions (true only of a specific fixture).

---

## Debugging "why did this line come out wrong?"

This is *the* question that gets asked, so there are two tools for it. Reach for
them instead of inferring from a second library, which reintroduces the very
assumptions under test:

```powershell
# The analysed block tree: headings, paragraphs, lists, tables, page breaks.
target\spike\dconv.exe --dump "tests\fixtures\private\<doc>.pdf"

# The bands and segments, with the geometry behind them (and every glyph's
# x/bottom/size in bander-walk order with --glyphs). This is how findings D7-D9
# were found.
cargo run -p dconv-pdf --profile spike --example bands -- "tests\fixtures\private\<doc>.pdf" 2 --glyphs
```

---

## Conventions

- **Comments explain why, and cite the evidence.** The existing style names the
  real document, the measured number, or the failed alternative. A constant with a
  plausible-sounding justification is worth less than one that says which document
  contradicted it.
- **Every analyser bug becomes a regression test built from the geometry that
  produced it** — real coordinates, not a tidied-up version. See
  `a_smaller_line_below_a_heading_is_not_a_subscript` in `layout.rs`, and
  `the_real_threescript_geometry_survives_the_constants`, which reproduces the
  measured 11.04/6.96 pt `3ème` because the comfortable 4 pt version of the same
  test would not notice the constants moving.
- **Golden files are never blessed without reading the diff.** They are the only
  check that spans reader, analyser and writer at once, and they are worthless if
  they are re-blessed reflexively.
- **No speculative abstraction.** Closed matches until there is a second
  implementation.
- Rust edition 2024, workspace `rust-version` 1.92. All engine crates are
  `publish = false`.
- Line ending: the repo has LF files and **no `.gitattributes`**, so git warns
  `LF will be replaced by CRLF` on every `git add`. Harmless, but do not "fix" it
  mid-task; nothing depends on it yet.

---

## Environment traps (Windows / PowerShell)

These have each cost real time here:

- **`>` redirection writes UTF-16 with a BOM.** `dconv --dump > f.txt` then reading
  it as UTF-8 fails with `byte 0xff in position 0`. Use
  `Out-File -Encoding utf8`, `[IO.File]::WriteAllText`, or capture with Python's
  `subprocess`.
- **Capturing a native command's stdout decodes it as the OEM codepage**, so
  `dconv --dump | Out-String` mangles every accent *before* your own
  `[IO.File]::WriteAllText(..., utf8)` gets a chance to preserve them. Set
  `[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)` for the capture
  too. This one is nasty because the mojibake looks like a reader bug, and because
  an ASCII-only fixture set never notices it.
- **Cargo writes progress to stderr, so PowerShell reports a nonzero exit code for
  successful builds.** Do not read that as failure — read the output.
- **Run cargo invocations sequentially.** Parallel runs contend on the
  package-cache file lock and one fails confusingly.
- **Set `$env:PYTHONIOENCODING = "utf-8"`** before Python, or accented console
  output is mangled.
- **`Start-Process -PassThru` + `WaitForExit` yields `PeakWorkingSet64 == 0`.**
  Poll with `$p.Refresh()` instead.
- **Here-strings mangle nested quotes.** Multi-line Python passed through
  `python -c @"..."@` silently corrupts. Write a `.py` file to
  `C:\Users\ilyes\AppData\Local\Temp\opencode\` and run that.
- PowerShell reports nonzero exit for the Python **comparison scripts** too,
  because they `sys.exit(1)` when a check fails. That one *is* meaningful.

---

## Where the work stands

**Done and committed** — Phase 0 spikes and findings; the docx writer; the whole
PDF reader (`bind`, `glyph`, `layout`, `analyze`); the Doc IR; the engine facade,
including the capability query; the `dconv` CLI with a `-j` worker-process pool;
the fidelity harnesses; the synthetic fixtures with their golden files; and the
GUI shell with its real engine (worker-process pool around the facade).
`git log --oneline` is the source of truth for what
landed when; this file deliberately names no commit hashes, because a hash in a
document goes stale the moment anyone commits.

**Tests:** 96 — `dconv-ir` 17, `dconv-pdf` 30 (23 geometry, plus the seven bind
tests that pin what is embedded and how it is unpacked), `dconv-docx` 12,
`dconv-engine` 9 (eight unit plus the end-to-end golden test), `dconv-cli` 9
(seven unit, plus two end-to-end pool runs that compare a pooled run with a serial
one and with the goldens), `dconv-app` 19 (nine queue/settings, two log, eight
engine protocol).
All green. The engine counts are the engine owner's to move; the app count the UI
owner's.

**Known gaps, in the order they are worth doing:**

1. **Wrapped table cells** — 137 of the 140 remaining differing tokens on the
   second document. Columns separate cleanly and simple tables are recovered as
   real Word tables; what defeats it is a cell that wraps, because then a visual
   line is not a row and the band holding one cell's tail has fewer segments than
   its neighbours, so the region stops looking like a table and falls back to flow.
   The fix is well-defined: group bands into rows by column membership, then merge
   a cell's wrapped lines.
2. **Heading inference** — a 12 pt note box in an 11 pt document becomes thirteen
   spurious `H3`s (`Question :`). Costs **zero** tokens; it is visible fidelity
   only. Size alone cannot fix it. The document offers a second signal: genuine
   headings sit 18–30 pt below the preceding line against a 14 pt body pitch,
   while the note's lines sit 14.5–16.5 pt apart. This is finding D13.
3. **GUI polish with measurements behind it.** The worker pool, per-page live
   progress and mid-batch cancel are in the M1 adapter. What remains: an ETA in
   the top bar (computable from the per-page progress rate); the GUI-vs-CLI
   golden comparison — the byte-identical assertion's GUI half, which needs no
   CLI at all, just a comparison of the worker child's output with the committed
   goldens; and switching `engine::supported` from its hand-written mirror to the
   facade's capability query, which has landed.
4. **More readers/writers** — DOCX, PPTX, XLSX, ODT, HTML, MD, TXT into IR. This is
   when the closed `match` in the facade becomes a real registry.
5. **Multi-column reading order.** `tests/fixtures/synthetic/columns.pdf` pins the
   current behaviour, which is wrong: text is read *across* the columns rather than
   down each one, so a sentence from the left column has the right column's opening
   line spliced into it, and the rows that do align are taken for a table. No
   document here has needed it, which is why it is this far down — but the fixture
   makes the day one does a deliberate diff rather than a surprise.
6. **The superscript bound tightens as the script gets smaller.** The bander
   measures a script's raise between loose box *bottoms*, and a smaller glyph has a
   shallower box, so the difference in descent is added to the offset it compares
   against a bound that shrinks with the script. At the measured proportions the
   offset is 0.82 of the script against a 0.90 bound; keep the raise the same
   fraction of the base and shrink the script to half the base's size, and the same
   arithmetic gives 1.10, which splits. The crossover is near 59 %, real
   superscripts run 50–70 %, and this is a live edge that no document here sits on.
   Comparing baselines rather than bottoms is the fix; both real documents would
   have to be re-measured to re-tune the constants against the new quantity. Full
   detail in `SCRIPT_RAISE_OWN`'s comment in `engine/pdf/src/layout.rs`.
7. **Packaging — and the notices that go with it.** The embed makes the binary
   self-sufficient, but nothing assembles a release yet: there is no script that
   fetches, builds `--profile release` and names the artifact, and nothing sets
   `#![windows_subsystem = "windows"]` in `app/src/main.rs`, so the GUI build
   opens a console window behind it. That script is also where the embed stops
   being a hope and becomes something checked — the artifact is either several
   megabytes bigger than a binary without it or it is not the deliverable, and
   the procedure above is the honest test. PDFium is BSD-3-Clause and its own
   dependencies have their notices vendored in `third_party/pdfium/licenses/`;
   shipping an `.exe` that carries the library means shipping those texts with it,
   which is a decision for whoever writes the packaging step — beside the `.exe`,
   or embedded and printed by a `--licenses` flag, which is the only shape that
   keeps the promise of one file.

Full detail, including the measurements behind each, is in
`docs/phase0-findings.md`.
