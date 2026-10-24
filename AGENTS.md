# AGENTS.md

Guidance for any agent (or human) picking this repository up. Read this before
changing anything; it will save you from re-deriving decisions that already have
measurements behind them.

---

## The one rule that matters: two owners, two directories

This repository is built as two lanes working in parallel, and the directory
split is the safety mechanism:

| Owner | Directory | Crate / binary |
| --- | --- | --- |
| Engine | `engine/**` | `dconv-*`, binary `dconv` (CLI) |
| GUI | `app/**` | `dconv-app`, binary `dconvert` |

**Do not edit the other side's directory.** Not "be careful" — don't open the
files. The only shared surfaces are the workspace manifest, `docs/`,
`scripts/`, and the published API of `dconv-engine`. If you need something from
the other side, change the *contract* and say so; do not reach in.

The GUI is deliberately **not** matched by a glob in `Cargo.toml`
(`members = ["engine/*", "spikes/*", "app"]`). Cargo resolves every glob match
into a package, so a half-created UI directory would have broken the engine's
build. That is why the member list is explicit, and why you should not convert
`app` to the glob.

---

## Layout

```
engine/
  ir/          dconv-ir        the Doc IR: the contract between reader and writer
  pdf/         dconv-pdf       PDFium binding, glyph and ruling extraction, layout
                               analysis; build.rs embeds the vendored library in
                               the binary
  docx/        dconv-docx      the writer (hand-built zip + XML)
  convert/     dconv-engine    the facade the CLI and GUI both call
  cli/         dconv-cli       the `dconv` binary
app/           dconv-app       the egui/eframe GUI, binary `dconvert`  (UI owner)
spikes/        pdf-extract, typst-pdf, pdf-pool   Phase 0 throwaway experiments
scripts/       fetch-pdfium.ps1, package.ps1, make_fixtures.py, compare_*.py,
               validate_docx.py, cpu-scale.py
docs/          phase0-findings.md   the decision record
tests/fixtures/generate_fixtures.py     spike fixture generator (Python, no deps)
tests/fixtures/generated/  (committed)  four fixtures the *spikes* use; perf.pdf
                                        is the 100-page throughput-benchmark input
tests/fixtures/synthetic/  (committed)  seven fixtures, each with a golden .docx
tests/fixtures/private/    (gitignored) the real documents — the only source of
                                        the fidelity numbers below
third_party/pdfium/        packaging metadata (VERSION, licenses/) is committed;
                           bin/, lib/, include/ are gitignored, fetched by script
out/, dist/, target/       gitignored scratch
```

Two fixture sets by two generators — don't mix them up:

- `scripts/make_fixtures.py` → `synthetic/`: what `dconv-engine`'s golden test
  converts and what byte-reproducibility is pinned against (needs PyMuPDF).
- `tests/fixtures/generate_fixtures.py` → `generated/`: baseline/perf input for
  the Phase 0 `spikes/` only, dependency-free by design.

`spikes/` is history, not code to build on. They answered their questions; the
findings are what survived.

---

## Build, test, run

```powershell
cargo build  --profile spike            # whole workspace
cargo test   --profile spike            # whole workspace, 100 tests
cargo build  -p dconv-cli --profile spike
target\spike\dconv.exe --help           # CLI; dconvert.exe is the GUI

.\scripts\package.ps1                   # dist\: both executables + LICENSES\
.\scripts\package.ps1 -Profile spike -SkipBuild   # assemble only (what CI runs)
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

`engine/pdf/src/bind.rs` has `PDFIUM_VERSION = "8100"`, and the script (via the
committed `third_party/pdfium/VERSION`) is the source of truth for it. Keep them
in step; `pdfium-render 0.9.4` generates bindings for the `chromium/7881` API, so
newer drops are a safe superset and older ones are not.

**Order matters for the deliverable.** The fetch has to happen before the build
that ships, because `engine/pdf/build.rs` embeds the vendored library only if it
is there when the crate is compiled; a tree built without it still compiles, but
the binary carries nothing and will need a `pdfium.dll` next to it.

The order is not enforced by cargo, and the case worth knowing about is a fetch
made *after* a build: cargo compares the watched file's timestamp against the
last build's, and `tar` restores the archive's timestamps — so a library the
fetch script has just vendored is *older* than the build it has to invalidate,
and the build after it still carries nothing. Silently. Two things catch it,
both measured: the fetch script touches `engine/pdf/build.rs` and the library
itself, dirtying the crate as a source change cargo cannot miss; and
`what_ships_is_the_library_every_test_binds_to` fails, with the fix in the
message, when the vendored library is on disk while this build carries no copy
of it. Vendoring PDFium by hand is the one path that does neither — touch
`engine/pdf/build.rs` or run `cargo clean -p dconv-pdf` before building a
release.

### Checking the single-file claim

What the tests cannot assert is that the *program* still works with no library
on disk anywhere: every bind on this machine finds the vendored copy first. (The
bytes are asserted — that is the seven tests in `bind.rs`.) Two things cover the
rest.

`scripts/package.ps1` assembles the deliverable and refuses to finish unless
each executable *contains* the vendored library's bytes — an ordinal search over
17 MB rather than a hash, because the library is inside the file and there is no
second file to hash. That runs against the release build, which is stripped, so
it is also what checks that stripping did not take the embed with it.

The runtime claim is a procedure, run when it matters (a release, or a change to
`bind.rs`):

1. Rename `third_party/pdfium/bin/pdfium.dll` aside, and delete
   `%LOCALAPPDATA%\docconvert\pdfium\8100\` if it exists.
2. Copy `target\spike\dconv.exe` into an empty directory of its own.
3. Run it over any fixture from there. It must convert, and it must leave a
   `pdfium.dll` in the version-keyed cache, byte-identical to the one renamed
   aside.
4. Run it again: the second run must not rewrite the cached copy.
5. Put the vendored library back — and do not rebuild while it is aside, since a
   build in that window is a build with no embedded copy.

Step 3's output is worth diffing against the committed golden while the tree is
in that state: a converter that unpacks its library correctly and a converter
that converts correctly are two different claims.

---

## CI (`.github/workflows/ci.yml`)

One Windows job on every push to `main` and every PR: fetch PDFium → `cargo
build/test --workspace --profile spike --locked` → `git diff --exit-code`
against `tests/fixtures/synthetic` (the goldens must be untouched — `DCONV_BLESS`
is never set in CI, so an accidental or unread re-bless fails here) →
`package.ps1 -Profile spike -SkipBuild` with the embed assertion → every
`dist/LICENSES/` file hash-checked against its upstream copy.

Deliberately not gated: `cargo fmt --check` and `cargo clippy -D warnings`.
Neither is clean in the tree today, and turning them on would mean reformatting
across the engine/app ownership line — the workflow header says so. Do not
"helpfully" run them tree-wide; do it lane-local if at all, and don't add lint
gates to CI as a drive-by.

`--locked` means a dependency change breaks CI unless `Cargo.lock` is committed
with it.

There is no installer and no automated release; `package.ps1` is the whole
release process.

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
   build IR blocks. The other formats (DOCX, PPTX, XLSX, ODT, HTML, MD, TXT)
   will read directly into IR with no analyser stage.

`rules.rs` reads the page's horizontal ruling lines, which `analyze.rs` uses to
group table rows; it is a side input to stage 3, not a fourth stage.

### Why the analyser looks strange

Grouping text by baseline is a **two-dimensional** problem, and almost every
non-obvious constant in `layout.rs` / `analyze.rs` is a scar from a real
document. Highlights (full detail in `docs/phase0-findings.md`, findings
A1–A4, D1–D16):

- Bands are built by sorting glyphs **baseline-descending** and offering each
  glyph to the nearest existing band first (`BAND_SEARCH`), then re-anchoring a
  band when a taller glyph arrives. This exists because a superscript has a
  *higher* baseline and is therefore seen **before** the base text it annotates.
  Without it, `2ème` splits.
- The script test must be **symmetric** (`min < max * SCRIPT_SIZE`) and the raise
  must be bounded **both** against the base text (`SCRIPT_RAISE_EM`) and against
  the glyph's own height (`SCRIPT_RAISE_OWN`). One bound alone merges a bullet
  into the title above it, or an 11 pt line into a 24 pt heading.
- Word gaps use **loose (advance) boxes**, never tight (ink) boxes, and the
  space width is measured per page from the page's own `U+0020`.
- A band of nothing but whitespace is dropped, because whitespace has a size and
  a size above the body's reads as a heading (this produced an empty `H3` from a
  single 12 pt space).
- Line font size is the **character-weighted dominant** size, not the maximum.
- A heading needs **both** a size margin and space above it. The 12 pt note in
  `TP2 Admin BD`'s 11 pt body is only 1.0 pt above the body, while the smallest
  genuine heading is 12.5 pt; `HEADING_MIN_ABOVE_BODY` excludes it. Spacing
  (`HEADING_GAP_PT`) is the second signal, but it cannot do the job alone: six
  of the note's lines follow a code block and sit 23–34 pt below it. This is
  finding D13's resolution.

### Deliberate v1 scope decisions — do not relitigate without reason

- **OCR is out of scope.** Text-based PDFs only. Leave the architectural hook;
  do not build it.
- **No de-hyphenation.** A trailing hyphen is ambiguous, and joining wrongly
  corrupts two words. Leave it.
- **Dropped code points are reported, never guessed.** PDFium maps some real
  `U+002D` hyphens to `U+0002`. The character is not recoverable, and a wrong
  character is invisible corruption where an omission is a visible gap. The
  reader keeps a histogram and the CLI prints it.
- **Lists use literal markers + a hanging indent** (`w:ind w:hanging="360"`),
  not `w:numPr`/`numbering.xml`.
- **Images emit a `[alt]` placeholder paragraph**; footnote refs become
  superscript `[label]` with no note body.
- **`w:sz` is emitted explicitly on every run.** `w:pPr/w:rPr` does *not* apply
  to runs.
- **Twips/EMU round half-away-from-zero** (`round_half_away` in `dconv-ir`),
  never truncate.
- **Format dispatch is a closed `match`, not a plugin registry**, until there is
  a second reader or writer to register.
- **Explicit `Block::PageBreak` between pages**, not after the last.

---

## The engine facade

`engine/convert/src/lib.rs` is the only conversion entry point, for both front
ends. Neither the CLI nor the GUI owns any conversion logic — they differ only
in how they collect inputs and show progress. This is what makes byte-identical
CLI and GUI output achievable rather than aspirational.

```rust
let engine = Converter::bind()?;                     // binds PDFium (once per process)
let report = engine.convert(&input, &output, &mut sink)?;   // reader+writer by extension
let document = engine.analyse(&input, &mut sink)?;          // no write, for inspection
```

`Sink` (`engine/pdf/src/lib.rs`, re-exported) is the progress *and cancellation*
interface:

```rust
fn page(&mut self, done: usize, total: usize)   // after each page
fn cancelled(&self) -> bool                     // checked between pages only
```

A trait rather than a closure because cancellation has to travel back from the
caller, and killing a thread is not an option — PDFium's state is
process-global. Cancellation is deliberately only consulted **between** pages:
interrupting one mid-read would leave PDFium's iterator undefined.

### What the facade can do: ask it, do not mirror it

```rust
pub const CONVERSIONS: &[(Format, Format)] = &[(Format::Pdf, Format::Docx)];

pub fn targets_for(source: Format) -> Vec<Format>   // what convert accepts from it
pub fn readers() -> Vec<Format>                     // every source it reads
pub fn all_targets() -> Vec<Format>                 // the union a settings panel offers
```

`CONVERSIONS` is the list `Converter::convert` gates on, and every query is
derived from it, so a front end cannot offer a format the facade will refuse.
The dispatch `match` decides only *how*; the table decides *whether*. A test
walks every pair of known formats and fails if the two disagree, because a row
with no match arm is a promise that dies at the last moment — once per file,
after the queue has been accepted.

`Format::from_extension` exists for the same reason: a front end filters a queue
by extension string long before it has a path to hand `Format::from_path`.

Two types are re-exported so a front end need not depend on the reader crate:
`PdfError` (cancellation reaches a caller as `PdfError::Cancelled` through the
reader chain) and `ReadStats` (the diagnostics a conversion reports *are* one).

### Critical: PDFium binds once per *process*

`bind_to_library` routes through a process-global static; the **second bind in
a process fails** (finding A1). Consequences:

- One `Converter` per process, reused for a whole batch. Both front ends do
  this.
- Real parallelism therefore requires a **pool of worker processes**, not
  threads. Both front ends have one, built the same way: re-invoke the current
  executable with a hidden flag (`dconvert --dconvert-worker`,
  `dconv --dconv-worker`) and speak one JSON object per line over its pipes.
  Re-running itself is what keeps the deliverable one file, and it means a
  worker can never be a different build from the parent that spawned it.
- For single-file delivery the DLL is embedded with `include_bytes!` and
  unpacked into a version-keyed cache on first run — in `engine/pdf/build.rs`
  and `bind::unpack_embedded`. A build carries the copy only if the vendored
  library was present when it was compiled, so the fetch script must run
  *before* the build that ships. Static linking is not possible
  (`lib/pdfium.dll.lib` is an *import* library).

---

## The GUI (`app/`)

The egui/eframe 0.37 shell is complete (20 tests including an end-to-end
worker-vs-golden run in `app/tests/`) and converts for real: the facade runs in
a pool of worker **processes**, not threads.

```
app/src/main.rs      eframe setup; the --dconvert-worker gate; picks the engine
app/src/engine.rs    protocol types + the real worker-process pool + capability
                     delegation; `engine::demo` is the PDFium-free review engine
app/src/state.rs     jobs, queue, settings, output naming (egui-free, tested)
app/src/ui.rs        all drawing        app/src/app.rs: the eframe App
app/src/theme.rs     the palette         app/src/log_ring.rs: the console log
```

Two facts a new agent should not trip over:

- **egui 0.37 unified `SidePanel`/`TopBottomPanel` into `egui::Panel`** with
  `left/right/top/bottom` constructors, each `show` taking the parent `Ui`, and
  eframe's `App` trait method is `fn ui(&mut self, ui, frame)` — not the older
  `update(&mut self, ctx, frame)`. Copy the existing patterns.
- **Output naming policy lives in the UI**, not the engine. `resolve_output_path`
  runs once at batch start so the queue can show final names before anything
  runs. The engine must write **exactly** where it is told. Two same-named
  inputs from different folders must get distinct outputs.

How a conversion runs: `main` checks for `--dconvert-worker` before any GUI code
exists and re-invokes this same binary as a hidden worker child, which binds
PDFium once and processes JSON lines from stdin; the parent streams the queue
into the children and owns the kill, removing a killed child's half-written
output so a cancelled job never leaves a broken document behind. Every child
job is guaranteed a terminal event, so the UI's in-flight counter cannot strand.

Format honesty: the settings list and the accepted extensions come from
`engine::supported(ext)` / `readable(path)`, which delegate to the facade's
capability query. The one hand-written piece is the facade-format →
`TargetFormat` mapping (`target_of`), and it is an **exhaustive `match` on
purpose**: when the facade learns a target the UI does not name, `app` fails to
compile there and the UI owner classifies it — a new format can neither silently
fail nor silently stay hidden.

### The frozen protocol

Both pool implementations must honour `app/src/engine.rs`:

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

1. **Exactly one terminal event per job** (`Finished` / `Failed` /
   `Cancelled`). A job cancelled mid-flight is still a job that must be
   accounted for, or the UI's in-flight counter waits forever.
2. **Progress fractions are `0.0..=1.0`.**
3. The engine never blocks the UI thread; the UI drains events non-blocking
   once per frame and asks for repaints only while work is live.

Progress granularity is per page. Extend `Sink` only if the UI genuinely needs
more, keeping the CLI on the same interface or the CLI/GUI outputs can drift.

### The demo engine and the palette

`engine::demo::spawn_demo_engine()` is a one-line swap in `main.rs` away from
production: threads that *sleep* instead of converting, the same shape as the
real pool. A file name containing `fail` fails, one containing `warn` emits
warnings, and runtimes derive from the file name — every UI path is reviewable
without PDFium.

`theme.rs` pins a five-colour palette — navy `#142537` surfaces, teal `#04BFBF`
accents, orange `#FE773D` warnings, red `#F24130` failure, crimson `#A63F48` as
the danger surface (Cancel-all, and the overwrite confirmation) — and every
other tone is a blend of those five, plus one derived text-red (`#F57164`,
RED a quarter toward white) because RED itself measures only 4.1:1 as text on
the navy. The app is **permanently dark**: the old light mode was egui's stock
visuals, on which the status colours measured 2.3–3.7:1, and `theme.rs`'s
contrast tests hold every text colour to 4.5:1 against its surface. Do not
introduce a colour outside the palette.

---

## Measuring fidelity

The numbers this project quotes come from a harness that reads the text back
**out of the produced `.docx`**, not out of the IR, so a writer bug cannot hide
behind the reader that fed it. The reference is MuPDF (PyMuPDF), an independent
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
| `TP2 Admin BD 2026_2027.pdf` | 6 | 3.76 % | 97.92 % | 3 | 3 |

**Read the missing/extra columns, not the headline.** Word error rate is
order-sensitive and its reference is MuPDF's own block order, which is not
guaranteed to be reading order. Missing/extra tokens are order-independent and
are the honest measure of extraction. Both documents' token counts match the
reference exactly, so **every remaining difference is ordering except four
tokens** — all four are the `U+0002` hyphen.

`scripts/textdiff.py` holds the single implementation of normalisation and
token reporting; both harnesses import it. Do not fork it.

Also available: `scripts/compare_text.py` (extractor-level diff, no DOCX
involved), `scripts/validate_docx.py` (independent structural validation with
python-docx — `--sample` adds assertions about the hand-written sample only),
`scripts/cpu-scale.py` (pool scaling). Keep structural checks (always true of
any `.docx` written here) separate from sample-content assertions (true only of
a specific fixture).

### The synthetic fixtures and the bless rule

`cargo test -p dconv-engine --profile spike` converts all seven `synthetic/`
fixtures and compares the bytes with their committed goldens; `dconv-cli`'s
pool test and `dconv-app`'s worker test compare their outputs with the same
goldens. The assertion is byte equality — the only check that catches a writer
that has picked up a clock or an unordered iteration. One fixture (`columns`)
pins output that is **known to be wrong**, on purpose: the day it is fixed, it
shows up as a diff rather than as a surprise. `table-wrapped` was the second
until Stage 2 read its rulings; its golden moved from four rows to three, and
the diff was read before blessing.

Re-bless a deliberate change with `$env:DCONV_BLESS = "1"`, then read
`git diff tests/fixtures/synthetic` before committing — a blessing nobody read
is a test that has quietly stopped testing anything.

---

## Debugging "why did this line come out wrong?"

This is *the* question that gets asked, so there are two tools for it. Reach
for them instead of inferring from a second library, which reintroduces the
very assumptions under test:

```powershell
# The analysed block tree: headings, paragraphs, lists, tables, page breaks.
target\spike\dconv.exe --dump "tests\fixtures\private\<doc>.pdf"

# The bands and segments, with the geometry behind them (and every glyph's
# x/bottom/size in bander-walk order with --glyphs). This is how findings
# D7-D9 were found. --rules prints the horizontal rulings, merged per border;
# a row boundary that reads wrong is usually a rule that was missed or merged
# into its neighbour.
cargo run -p dconv-pdf --profile spike --example bands -- "tests\fixtures\private\<doc>.pdf" 2 --glyphs
```

---

## Conventions

- **Comments explain why, and cite the evidence.** The existing style names the
  real document, the measured number, or the failed alternative. A constant
  with a plausible-sounding justification is worth less than one that says
  which document contradicted it.
- **Every analyser bug becomes a regression test built from the geometry that
  produced it** — real coordinates, not a tidied-up version. See
  `a_smaller_line_below_a_heading_is_not_a_subscript` in `layout.rs`, and
  `the_real_threescript_geometry_survives_the_constants`, which reproduces the
  measured 11.04/6.96 pt `3ème` because the comfortable 4 pt version of the
  same test would not notice the constants moving.
- **Golden files are never blessed without reading the diff.** They are the only
  check that spans reader, analyser and writer at once, and they are worthless
  if re-blessed reflexively.
- **No speculative abstraction.** Closed matches until there is a second
  implementation.
- Rust edition 2024, workspace `rust-version` 1.92, all crates
  `publish = false`.
- Line ending: the repo has LF files and **no `.gitattributes`**, so git warns
  `LF will be replaced by CRLF` on every `git add`. Harmless, but do not "fix"
  it mid-task; nothing depends on it yet.

---

## Environment traps (Windows / PowerShell)

These have each cost real time here:

- **`>` redirection writes UTF-16 with a BOM.** `dconv --dump > f.txt` then
  reading it as UTF-8 fails with `byte 0xff in position 0`. Use
  `Out-File -Encoding utf8`, `[IO.File]::WriteAllText`, or capture with Python's
  `subprocess`.
- **Capturing a native command's stdout decodes it as the OEM codepage**, so
  `dconv --dump | Out-String` mangles every accent *before* your own
  `[IO.File]::WriteAllText(..., utf8)` gets a chance to preserve them. Set
  `[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)` for the capture
  too. This one is nasty because the mojibake looks like a reader bug, and
  because an ASCII-only fixture set never notices it.
- **Cargo writes progress to stderr, so PowerShell reports a nonzero exit code
  for successful builds.** Read the output; the same is true of the Python
  comparison scripts, which is *meaningful* because they `sys.exit(1)` on a
  failed check.
- **Run cargo invocations sequentially.** Parallel runs contend on the
  package-cache file lock and one fails confusingly.
- **Set `$env:PYTHONIOENCODING = "utf-8"`** before Python, or accented console
  output is mangled.
- **`Start-Process -PassThru` + `WaitForExit` yields `PeakWorkingSet64 == 0`.**
  Poll with `$p.Refresh()` instead.
- **Here-strings mangle nested quotes.** Multi-line Python passed through
  `python -c @"..."@` silently corrupts. Write a `.py` file and run that.

---

## Where the work stands

Done: the PDF reader (`bind`, `glyph`, `layout`, `rules`, `analyze`), the Doc IR, the
docx writer, the engine facade with the capability query, the `dconv` CLI with
its `-j` worker pool, the fidelity harnesses, the synthetic fixtures with
goldens, the GUI with its real engine, packaging, and CI.
`git log --oneline` is the source of truth for what landed when; this file
deliberately names no commit hashes, because a hash in a document goes stale
the moment anyone commits.

**Tests:** 115 — `dconv-ir` 17, `dconv-pdf` 45 (38 geometry, plus the seven
bind tests), `dconv-docx` 12, `dconv-engine` 9 (eight unit, plus the end-to-end
golden run), `dconv-cli` 9 (seven unit, plus two end-to-end pool runs: pooled
vs serial vs goldens), `dconv-app` 23 (ten queue/settings, two log, eight
engine protocol, two palette-contrast, plus the worker-vs-golden run in
`app/tests/`, which skips on a tree that lacks a fetched `pdfium.dll`).

**Known gaps, in the order they are worth doing:**

1. **Multi-column reading order** — a remaining block of the 76 differing
   tokens on the second document. Text is read *across* the columns instead of
   down each one: page 3's script list beside the account table (the corrected
   diagnosis is in `docs/phase0-findings.md`). The fix is to split a
   page into vertical strips at an x-interval no segment's ink crosses, when at
   least two bands straddle it *and* both sides carry a real share of the content
   width — the second test is what rejects the table's own internal gutters
   (measured at [501.20, 521.77]). MuPDF reads page 3 as the whole left list and
   then the table, which is the model to match. `Region::Columns` (the
   two-column page) is a different split and stays as it is. `columns.pdf`'s
   golden pins the current, wrong behaviour on purpose. The rest of the 76 is
   the page header/footer segment order and the dropped `U+0002` hyphens.
2. **A fully ruled table with exactly two rows** — the rule-based grouping
   needs two internal rules before absence of a rule between bands means "same
   row", because a table that rules only its header would otherwise merge its
   whole body into one. A two-row table has exactly one internal rule, so it
   stays on the subset test and an every-cell-wrapped row still splits. Telling
   the two cases apart needs a second signal (the header's shading is the
   obvious one); both real documents' wrapped tables have three or more rows,
   and `table-wrapped.pdf` has three. A producer that draws the whole grid as
   one path object is the other limit: there is no thin edge to read.
3. **More readers/writers** — DOCX, PPTX, XLSX, ODT, HTML, MD, TXT into IR.
   This is when the closed `match` in the facade becomes a real registry.
4. **The superscript bound tightens as the script gets smaller.** The bander
   measures a script's raise between loose box *bottoms*, and a smaller glyph
   has a shallower box, so the offset it compares against a bound that shrinks
   with the script arrives inflated: the crossover is near 59 %, real
   superscripts run 50–70 %, and this is a live edge no document here sits on.
   The fix is comparing baselines rather than bottoms, which needs both real
   documents re-measured. Full detail in `SCRIPT_RAISE_OWN`'s comment in
   `engine/pdf/src/layout.rs`.
5. **Rust crate notices.** `scripts/package.ps1` ships `LICENSES/` beside the
   executables (the deliverable is a folder, not one file — decided, so nobody
   promises "one file"), every file verbatim from `third_party/pdfium/` and
   hash-checked by CI. Still missing: the notices for the crates the
   executables link. `Cargo.lock` resolves 698 packages; `cargo-about` is the
   intended tool, and `LICENSES/README.txt` says so where a redistributor
   reads it.
