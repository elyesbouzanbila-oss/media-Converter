# AGENTS.md

Guidance for any agent (or human) picking this repository up. Read this before
changing anything; it will save you from re-deriving decisions that already have
measurements behind them.

---

## TL;DR

- **Two owners, two directories.** The engine lives in `engine/`, the GUI lives in
  `app/`. Never edit across that line.
- The engine works: `dconv file.pdf` produces a real `.docx` at **1.28 % word
  error** on the primary test document. Everything is committed and 69 tests pass.
- **Conversion is byte-for-byte reproducible, and that is now asserted.** Seven
  committed fixtures under `tests/fixtures/synthetic/` are converted end to end
  and checked against a committed `.docx` each, so a fresh clone can run a real
  end-to-end test. The *fidelity percentages* are the part that still needs the
  gitignored private documents.
- **The GUI shell is finished but runs a *demo* engine** — it does not convert
  anything yet. Wiring the real facade into it is the next milestone, and it is a
  one-file adapter against a frozen protocol. See [The M1 seam](#the-m1-seam).
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
  pdf/         dconv-pdf       PDFium binding, glyph extraction, layout analysis
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
cargo test   --profile spike            # whole workspace, 69 tests
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

### Critical: PDFium binds once per *process*

`bind_to_library` routes through a process-global static; the **second bind in a
process fails** (finding A1). Consequences:

- One `Converter` per process, reused for a whole batch. The CLI does exactly this.
- Real parallelism therefore requires a **pool of worker processes**, not threads.
- For single-file delivery, `pdfium.dll` is embedded with `include_bytes!` and
  extracted to a version-keyed cache on first run. Static linking is not possible
  (`lib/pdfium.dll.lib` is an *import* library) — see the corrections section of
  the findings.

---

## The GUI (`app/`) — M0 shell, finished

The UI owner delivered a complete egui/eframe 0.37 shell. It compiles, its 11
tests pass, and it is reviewed-ready. **It is running a demo engine, so it does not
convert anything yet.**

```
app/src/main.rs       eframe setup; picks the engine (currently the demo)
app/src/engine.rs     the engine boundary: protocol types + the demo pool
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

Two facts a new agent should not trip over:

- **egui 0.37 unified `SidePanel`/`TopBottomPanel` into `egui::Panel`** with
  `left/right/top/bottom` constructors, each `show` taking the parent `Ui`, and
  eframe's `App` trait method is `fn ui(&mut self, ui, frame)` — not the older
  `update(&mut self, ctx, frame)`. Copy the existing patterns.
- **Output naming policy lives in the UI**, not the engine. `resolve_output_path`
  runs once at batch start so the queue can show final names before anything runs.
  The engine must write **exactly** where it is told. Two same-named inputs from
  different folders must get distinct outputs.

### The demo engine

`engine::spawn_demo_engine()` is a real worker pool over a shared queue that
*sleeps* instead of converting — deliberately the same shape as the measured
Phase 0 architecture, so the real adapter drops in without re-shaping the UI's
threading assumptions. Demo sensitivities, so every UI path is reviewable: a file
name containing `fail` fails, one containing `warn` emits warnings.

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

## The M1 seam

Wiring the real engine in is the next milestone, and the UI was built so that it is
small. Concretely:

1. Add a real adapter in `app/src/engine.rs` producing `EngineChannels` — the same
   signature as `spawn_demo_engine`. **This is GUI-side work; the engine owner
   should specify the contract, not write it.**
2. Change the single call in `app/src/main.rs` (`engine::spawn_demo_engine()`), as
   its comment says.

Things that will actually bite, and are worth settling before writing code:

- **Worker *processes*, not threads.** The demo uses threads because it sleeps. The
  real pool cannot, because of the process-global PDFium bind. `workers` in
  `EngineCommand::Start` must mean process count.
- **The format lists do not match.** `TargetFormat` in `app/src/state.rs` offers
  `Docx` and `PlainText`; `dconv_engine::Format` offers only `Pdf` and `Docx`, and
  `Converter::convert` supports exactly one pair (PDF → DOCX, everything else is
  `EngineError::UnsupportedPair`). Either implement a plain-text writer, or drive
  the UI's list from a facade capability query as `state.rs` already anticipates —
  but do not let the UI offer a format that silently fails.
- **Errors need mapping.** `EngineError` → `EngineEvent::Failed { summary, detail }`.
  `EngineError::Cancelled` must map to `Cancelled`, not `Failed`.
- **Byte-identical CLI vs GUI output is a hard requirement.** It is the test that
  proves no conversion logic leaked into a front end. Keep it.
- Progress granularity today is per page. That is coarse for a 5-page document, and
  it is what `Sink` offers — extend the trait if the UI needs more, but keep the
  CLI on the same interface or the outputs can drift.

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
PDF reader (`bind`, `glyph`, `layout`, `analyze`); the Doc IR; the engine facade;
the `dconv` CLI; the fidelity harnesses; the synthetic fixtures with their golden
files; and the GUI shell. `git log --oneline` is the source of truth for what
landed when; this file deliberately names no commit hashes, because a hash in a
document goes stale the moment anyone commits.

**Tests:** 69 — `dconv-ir` 17, `dconv-pdf` 23, `dconv-docx` 12, `dconv-engine` 6
(five unit plus the end-to-end golden test), `dconv-app` 11. All green. The app
count is the UI owner's to move.

**Known gaps, in the order they are worth doing:**

1. **The M1 seam** — the GUI still runs the demo engine. See above. Until this
   lands, there is no GUI-based conversion at all.
2. **Wrapped table cells** — 137 of the 140 remaining differing tokens on the
   second document. Columns separate cleanly and simple tables are recovered as
   real Word tables; what defeats it is a cell that wraps, because then a visual
   line is not a row and the band holding one cell's tail has fewer segments than
   its neighbours, so the region stops looking like a table and falls back to flow.
   The fix is well-defined: group bands into rows by column membership, then merge
   a cell's wrapped lines.
3. **Heading inference** — a 12 pt note box in an 11 pt document becomes thirteen
   spurious `H3`s (`Question :`). Costs **zero** tokens; it is visible fidelity
   only. Size alone cannot fix it. The document offers a second signal: genuine
   headings sit 18–30 pt below the preceding line against a 14 pt body pitch,
   while the note's lines sit 14.5–16.5 pt apart. This is finding D13.
4. **Worker process pool + live progress/ETA/mid-batch cancel** wired through
   `Sink` to the UI's `EngineEvent`. The fixtures and goldens landed ahead of it
   on purpose: they are the safety net for the CLI changes this needs.
5. **The GUI half of byte-identical output.** The writer no longer stamps the
   current local time into the zip, and the golden test proves the CLI and
   `dconv-engine` agree byte for byte — so the property is real and measured,
   rather than aspirational. What is missing is the *GUI* assertion, which cannot
   exist until the M1 adapter does.
6. **PDFium DLL embed-to-cache** (`include_bytes!`) for the single portable `.exe`.
7. **More readers/writers** — DOCX, PPTX, XLSX, ODT, HTML, MD, TXT into IR. This is
   when the closed `match` in the facade becomes a real registry.
8. **Multi-column reading order.** `tests/fixtures/synthetic/columns.pdf` pins the
   current behaviour, which is wrong: text is read *across* the columns rather than
   down each one, so a sentence from the left column has the right column's opening
   line spliced into it, and the rows that do align are taken for a table. No
   document here has needed it, which is why it is this far down — but the fixture
   makes the day one does a deliberate diff rather than a surprise.
9. **The superscript bound tightens as the script gets smaller.** The bander
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

Full detail, including the measurements behind each, is in
`docs/phase0-findings.md`.
