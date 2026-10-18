# Phase 0 findings

Three spikes, run before committing to an architecture. Everything here is
measured on the development machine, not estimated.

**Machine:** AMD Ryzen 9 8940HX, 16 cores / 32 logical processors, 15.1 GB RAM,
Windows, Rust 1.99 MSVC. Mobile APU with a shared power budget — this matters,
see spike C.

Each spike answers a question that would be expensive to get wrong later, and
two of the three changed the design.

---

## Spike A — PDFium positioned-text extraction

`spikes/pdf-extract`. Can we get per-glyph geometry out of PDFium fast enough,
and is it good enough to rebuild lines and paragraphs from?

**Result on a 100-page, 381,902-glyph document:**

| Measurement | Value |
|---|---|
| Extraction + line grouping | **239.8 ms** (best of 3) |
| Target | < 1000 ms |
| Throughput | **417 pages/s**, 1.59 M glyphs/s |
| Per page | 2.40 ms |
| PDFium bind | 4.4 ms (warm) |
| Document load | 0.2 ms |
| Peak working set | 17.1 MB (12.0 MB for a 2-page document) |

Extraction speed is a solved problem and will not be the bottleneck. The layout
*heuristics* will be. Per-glyph FFI is affordable: the measurement includes a
round trip per glyph for loose bounds, tight bounds, font name, weight, italic
flag and generated flag.

### Finding A1 — PDFium is a process-global singleton

`Pdfium::bind_to_library` and `Pdfium::new` both route through a static
`BINDINGS.get_or_init(...)`, and a second bind returns
`PdfiumLibraryBindingsAlreadyInitialized`. Independent per-thread instances are
impossible; the `thread_safe` feature merely serialises one instance behind a
mutex.

**Consequence:** real parallelism requires a pool of worker *processes*, each
owning its own PDFium. The planned architecture is confirmed on evidence rather
than assumption.

### Finding A2 — use advance boxes, not ink boxes, for space inference

PDF omits space characters when a producer positions words individually. Spaces
must be reconstructed from the x-gap between glyphs.

The first implementation used tight (ink) bounds and emitted `741 .54` for
`741.54`: the ink box of `.` starts well inside its advance box, so the gap
looked like a word break. Switching to loose (advance) bounds fixed it.

**Consequence:** loose bounds drive grouping and space inference; tight bounds
are retained only for ink-level work such as underline and strikeout detection.

### Finding A3 — PDFium synthesises zero-height glyphs that must be filtered

PDFium inserts `U+000A`, `U+000D` and `U+0020` with a nominal 1.0pt size and a
zero-height box (`top - bottom == 0`) pinned to the text baseline. 77 per page
in the table fixture, ~96 per page in the dense one.

These corrupted line grouping in a non-obvious way. Line-grouping tolerance was
derived from the size of the group's *first* glyph. A synthetic CR with size
1.0 anchored a group, giving tolerance `max(1.0, 4.0) * 0.5 = 2.0` instead of
`16 * 0.5 = 8.0`. Real title glyphs 3.38pt away fell outside it, splitting every
title onto its own line and doubling the line count (34 instead of 17).

Two fixes, both now in the design: filter degenerate synthetic glyphs out of
geometry, and derive tolerance from the *largest* glyph in the group rather than
an arbitrary anchor.

Word boundaries are recovered from glyph gaps instead, which is strictly more
general than depending on what each producer chose to emit.

### Finding A4 — line grouping must be two-dimensional

A single grouping pass along x fuses the last line of column 1 with the heading
of column 2 when they share a baseline. Observed directly:

```
Fox over over at seventeen between is 2. Method and Approach
```

reported at 12pt, because the merged line takes the *maximum* font size — so a
naive reader would classify the whole line as a heading.

**Consequence:** grouping must band along y, then segment along x-gap, then
form cells. This is now a Phase 1 requirement rather than a guess.

### Ephemeral fixtures

`tests/fixtures/generate_fixtures.py` writes four synthetic PDFs (single-column,
two-column, ruled table, 100-page perf) with no pip dependencies. They use
base-14 fonts with **no embedding**, which is deliberately the worst case for
style recovery: font weights are unreliable there and the name-matching fallback
(`Helvetica-Oblique` → italic) is load-bearing. Real documents will exercise
things these cannot: embedded subset encodings, ligatures, rotated text, real
column detection.

---

## Spike B — embedded Typst PDF generation

`spikes/typst-pdf`. Validates the "anything → PDF" direction: how fast is Typst
embedded end to end, and does the font cache actually matter?

**Result, genuine cold compiles of distinct documents:**

| Pages | compile | PDF render | total | per page |
|---|---|---|---|---|
| 1 | 1.7 ms | 0.8 ms | 2.5 ms | 2.5 ms |
| 12 | 22.6 ms | 4.2 ms | 27.0 ms | 2.25 ms |
| 47 | 91.9 ms | 16.0 ms | 108.4 ms | 2.31 ms |

Essentially linear at **~2.3 ms per page** (1.9 ms compile + 0.34 ms render),
with negligible fixed overhead. ~430 pages/s single-threaded.

Round-tripped through spike A: the extractor reads the Typst output back with
correct text, 14pt bold headings and 10pt body. 47 pages produce 226,800 bytes,
zero warnings.

### Finding B1 — benchmark methodology: `comemo` will lie to you

Typst memoises compilation globally through `comemo`, keyed on source content.
The first version of this spike generated near-identical source per run and
reported a compile time of **0.12 ms** after the first iteration. That is a hash
lookup, not a compiler.

When the variant was woven into a single heading, "distinct" documents reported
**15 ms** compile. When it was woven into *every paragraph*, the same documents
reported **92 ms** — a **3.5× understatement** from partial cache hits.

**Consequence:** any Typst benchmark or test must vary content throughout, not
in one place. This is recorded because the failure mode is silent and flattering.

### Finding B2 — font discovery is cheap; the planned optimisation is unnecessary

Engine setup — building the standard library and discovering the 17 embedded
fonts — costs **2.7 ms, once per process**. Constructing a per-document world is
**0.48 ms**.

The plan assumed "font loading dominates per-file cost, so share one font
`World` across jobs". For embedded fonts that assumption is wrong: 2.7 ms is
~2.5% of a 47-page document. Sharing the cache is still correct because it is
nearly free, but it is not a meaningful optimisation and should not be
presented as one.

Layout (compile) is 85% of the cost; PDF render is 15%. Optimising the compiler
caps out at a 1.2× gain.

---

## Spike C — worker-process pool scaling

`spikes/pdf-pool` plus `scripts/cpu-scale.py` (control). Does N worker processes
give N× throughput? **No, and the reason is mostly the machine.**

**Measured scaling**, identical work per mode, 100-page documents:

| Workers | Parallel wall | Speedup | Efficiency | Per-worker inflation | Jobs/s |
|---|---|---|---|---|---|
| 1 | 2165 ms | 1.02× | 102% | 0.98× | 3.7 |
| 2 | 1129 ms | 2.05× | 103% | 0.97× | 7.1 |
| 4 | 619 ms | 3.55× | 89% | 1.09× | 12.9 |
| 8 | 444 ms | 4.83× | 60% | 1.58× | 18.0 |
| 16 | 829 ms | 5.60× | 35% | 2.60× | 19.3 |
| 32 | 1607 ms | 5.62× | 18% | **5.11×** | 19.9 |

Throughput **saturates at ~20 documents/s**, essentially reached by 8 workers.
Going from 8 to 32 workers buys 10% more throughput for 4× the processes.
Per-job process overhead is only 15–16 ms, so this is not startup cost.

**Control experiment** (`scripts/cpu-scale.py`), pure scalar compute in N
processes on the same machine:

| Processes | 1 | 2 | 4 | 8 | 16 | 32 |
|---|---|---|---|---|---|---|
| Scaling | 1.00× | 1.84× | 3.31× | 4.32× | 5.69× | 8.12× |

The machine does not scale linearly under sustained multi-process load even for
pure integer arithmetic. At 8 processes, pure compute reaches 4.32× and the PDF
pool reaches 4.83× — the pool is already at the demonstrated hardware ceiling.

At 32 processes PDF (5.62×) trails compute (8.12×), so the PDF workload does add
extra contention at high concurrency, but the dominant limit is the platform.

### Finding C1 — pool sizing must not assume cores = throughput

`available_parallelism()` returns 32 on this machine. A pool of 32 is the wrong
default: 4× the memory and processes for ~10% more throughput than 8, with
5.1× per-worker slowdown.

Recommended default: **`min(8, physical_cores)`**, overridable, and explicitly
documented as machine-dependent. For interactive single-file conversion, one
worker gives the lowest latency (~271 ms for a 100-page document).

This also applies to the Typst write path, which will meet the same ceiling.

### Finding C2 — memory is not the constraint

17.1 MB peak for a 100-page document, 12.0 MB for a 2-page one. ~12 MB fixed
plus ~5 MB per 100 pages. A 32-worker pool costs under 600 MB. Over-provisioning
the pool wastes CPU and process churn, not memory.

---

## Real documents — the first honest test

Two genuine course handouts (French, 5 and 6 pages) arrived after the synthetic
fixtures. They immediately falsified an assumption the fixtures could not reach,
and supplied the baseline the synthetic set never could.

### Finding D1 — a fixed fraction of the font size is the wrong word-gap rule

Space inference thresholded on `gap > size * 0.25`. On real text that rule was
**simultaneously too strict and too loose for different spaces in the same
document**, because this font's space is *narrower* than the assumed 0.25 em:

```
        1  x=466.15  width=5.58  advance=8.00  gap=+2.42   <- a real space
   U+0020  x=488.14  width=2.49                          <- the font's own space
```

The gap after `1` is 2.42 pt and the document's own space glyph is 2.49 pt wide,
but the threshold was `11 pt * 0.25 = 2.75 pt`. A genuine space measured *below*
the threshold and was deleted. Every corruption, confirmed against MuPDF:

| ours (wrong) | MuPDF (correct) | occurrences |
|---|---|---|
| `Page 1sur 6` | `Page 1 sur 6` | 1 |
| `(sousOracle)` | `(sous Oracle)` | 4 |
| `MVet l'hôte` | `MV et l'hôte` | 10 |

Intra-word noise is at most ±0.4 pt, so the two distributions are cleanly
separated. Only the threshold's location was wrong.

**Fix:** measure the page's own space width from its `U+0020` glyphs, take the
median (not the mean, so stretched justified spaces cannot drag it upwards), and
threshold at a third of that. Measured value on these documents: **0.203 em**,
against the assumed 0.25 em.

### Finding D2 — quantified against an independent implementation

`scripts/compare_text.py` diffs our reconstruction against PyMuPDF (MuPDF).
MuPDF rather than PDFium deliberately: PDFium is the engine under test, so a
shared engine would hide engine-specific mistakes behind a shared opinion.

| Document | Before | After |
|---|---|---|
| `3IRS_TP2.pdf` (1640 tokens) | 13.84% WER | **1.46%** |
| `TP2 Admin BD` (2022 tokens) | 8.41% WER | **6.92%** |

The shape of the residual matters more than its size:

| Document | missing tokens | extra tokens |
|---|---|---|
| `3IRS_TP2.pdf` | 2 | 3 |
| `TP2 Admin BD` | 3 | 3 |

**Only five words in 1640, and six in 2022, are actually lost or invented.** The
remaining error is *reordering* — the layout analyser's job in Phase 1, not an
extraction defect. Word extraction is for practical purposes complete on these
documents.

### Finding D3 — control characters leak from font encodings

Two hyphens arrived as `U+0002`, because the font's encoding maps that glyph to a
control code. The character it should have been is unrecoverable from the page
description alone.

**Fix:** drop C0/C1 control characters. Emitting a raw control byte is strictly
worse than dropping it — it silently poisons the text, the XML and then the
DOCX. The cost is a lost hyphen in `entre-` and `ci-`, recorded as a known
limitation rather than hidden.

### Finding D4 — superscripts orphan onto their own lines

`2ème` split, leaving a line holding only `ème` at 7pt. The superscript sits
9.2 pt off an 11 pt baseline, beyond the 5.5 pt tolerance. Phase 1 needs
explicit superscript handling rather than treating every off-baseline glyph as a
new line.

### Finding D5 — line style must not be the maximum glyph size

`• Tester différents moyens…` was reported at 14 pt where its text is 12 pt: the
bullet glyph is set larger than the body it introduces, and line size was taken
as the maximum. A dominant size weighted by glyph count, or the modal size, is
correct; the maximum is not. This is the same class of error as finding A3, and
points the same way — derive line properties from the bulk of the line, never
from one extreme member.

### Finding D6 — two-column tables already read correctly

`Propriété Description` followed by `Atomicité La transaction s'exécute…` shows
cell structure surviving intact in the x-gaps. The 2-D segmentation from finding
A4 is the right shape for this; what it needs is the column/cell split, not a
redesign.

---

## Corrections to the original plan

1. **Static PDFium linking is not possible** from the bblanchon distribution —
   `pdfium.dll.lib` is an import library, not a static one. Building Chromium's
   PDFium needs depot_tools + GN/Ninja (hours, ~10 GB). The single-file
   deliverable instead embeds `pdfium.dll` via `include_bytes!` and extracts it
   to a version-keyed cache on first run. Still one `.exe`, no installer.
   `scripts/fetch-pdfium.ps1` pins the version and validates the package layout.

2. **Shared font cache is not a meaningful optimisation** for embedded fonts
   (2.7 ms). Keep it, but stop treating it as the reason for the `World` design.

3. **N workers ≠ N× throughput.** Expect ~4–5× at 8 workers on a mobile
   platform, and saturation beyond ~16. Pool size is a tuning parameter, not a
   function of core count.

4. **`pdfium-render` pins its API to chromium/7881**, but current PDFium
   binaries are chromium/8100. Newer is a superset and safe; older is not. The
   fetch script warns if the pin drifts.

---

## Open questions for Phase 1

> Superseded by the Phase 1 findings below, which answer most of these with
> measurements. Kept as written, because the difference between what was
> expected here and what actually happened is the useful part.

- Reading order in multi-column and table layouts is now the dominant source of
  error, measured, not guessed: ~134 of 140 remaining token differences on the
  second real document (finding D2). Table reconstruction needs the
  x-segmentation stage from finding A4 before it can even be evaluated.
- Superscript and subscript attachment (finding D4) has no implementation yet.
- Dominant line size by glyph count instead of maximum (finding D5) is a
  one-line change that has not been made.
- Whether heading inference should use font size against the document's modal
  size, or against a per-page distribution, is still unresolved. On the first
  real document, H1 and H2 are both 13 pt and differ only by italic, so size
  alone will not separate them.

---

# Phase 1 findings — the first end-to-end conversion

The pipeline runs: PDF → positioned glyphs → bands and segments → Doc IR →
`.docx`, through `dconv`. Measured against MuPDF, reading the text back *out of
the produced `.docx`* rather than out of our own code, so that a writer bug
cannot hide behind the reader that fed it.

| document | pages | word error rate | similarity | missing | extra |
| --- | --- | --- | --- | --- | --- |
| `3IRS_TP2.pdf` | 5 | **1.28 %** | 99.21 % | 1 | 1 |
| `TP2 Admin BD 2026_2027.pdf` | 6 | **6.92 %** | 96.34 % | 3 | 3 |

The word error rate is order-sensitive, and its reference is MuPDF's own block
order, which is not guaranteed to be reading order. The missing/extra token
counts are order-*independent*, and they are the honest measure of extraction:
every remaining difference is now ordering rather than loss. Both documents'
token counts now match the reference exactly.

## Finding D7 — a script's raise must be bounded against the base text too

An 11 pt bullet 10.94 pt below a 16 pt title was classified as a subscript of it.
The bullet glyph was pulled into the title line, which then read `Tra•nsactions`,
and — because the bullet was gone from its own line — that line stopped looking
like a list item at all. One misgrouped glyph cost a title, a list, and a
paragraph.

Bounding the raise by the arriving glyph's *own* size is not enough: 10.94 pt is
0.99 of the bullet's height, which is plausible for a script. Bounding it against
the *base* as well rejects it, because it is 0.68 em of the 16 pt line. Both
bounds are needed — the real `3ème` raises a 6.96 pt script 5.7 pt, which is 0.82
of its own height but only 0.52 em of the 11.04 pt base.

## Finding D8 — sorting by baseline visits a superscript before its base

Grouping glyphs by baseline descending is the obvious way to walk a page, and it
is wrong for exactly the case it most needs to get right: a raised glyph has a
*higher* baseline, so it is seen first, opens its own band, and the full-size
glyph it annotates can no longer join it. `2ème` consequently split into `2` and
`ème`. Two changes fix it, and neither works alone: offer each glyph to the most
recent existing bands before starting a new one, and re-anchor a band when a
taller glyph arrives, because a taller glyph is the base text rather than a
script.

## Finding D9 — the size-ratio test has to be symmetric

The same root cause seen from the other side. Asking whether the *arriving* glyph
is small is not the same question as whether the two glyphs differ much in size,
and the two answers diverge precisely when the script arrives first. The rule
compares `min` against `max`, not the newcomer against the band.

## Finding D10 — a band of nothing but spaces is not a paragraph

A single 12 pt space at the top of page 2 of `TP2 Admin BD` was emitted as an
empty heading: whitespace has no text but it does have a size, and a size above
the body's reads as a heading. Word gaps are inferred from geometry, so a band
with no letters in it carries no information at all. It is now dropped at the
layout stage rather than left for the classifier to keep re-deciding about.

## Finding D11 — list punctuation is information, and cheap to keep

`1)` was rendered as `1.`, and a list the source interrupted and resumed restarted
at 1. Both are visible edits to the document. The IR gained a `Numbering`
punctuation style and the reader carries the source's own number, so a writer
*reproduces* a list instead of inventing one.

## Finding D12 — control characters are a real, small, unfixable loss

`U+0002 × 1` on `3IRS_TP2.pdf`, `U+0002 × 3` on `TP2 Admin BD`, and each one sits
exactly at a line-ending hyphen (`sont-`, `entre-`, `ci-`). The glyphs are real
U+002D hyphens in the file; PDFium's ToUnicode mapping is what is broken, and the
code point that arrives carries no information about the character it replaced.
Recovering it would mean guessing, and a wrong character is invisible corruption
where an omission is a visible gap, so the character stays dropped.

What changed is that it is no longer *silent*. The reader keeps a histogram of
discarded code points and `dconv` reports them, which turned "the hyphens are
missing" into "exactly three code points, all `U+0002`" — the difference between
a mystery and a known limitation.

The same histogram shows `U+000A × 244` and `U+000D × 244` in one document, and
those are *correctly* discarded: a line break in a PDF is a positioning
operation, never a character, so one of each per line is the producer's
arithmetic showing through.

## Finding D13 — size alone over-promotes a note box to thirteen headings

`TP2 Admin BD` sets a bordered note and its questions at 12 pt inside an 11 pt
body. Size is the only signal the heading rule uses, and 12 pt clears the
`body_size + 0.5` cutoff, so `Question :` became a level-3 heading eleven times,
along with `Questions :`, `Connecté en Session A :`, and the tail of a wrapped
line (`dessous.`).

The document's size histogram says plainly that 12 pt is *never* a heading here —
it appears only in that note — but the profile cannot see that, because it
decides from a character count per size and 12 pt carries only 317 characters
against 11 pt's 8521. A character count cannot separate "rare because it is a
heading" from "rare because it is in a box".

What would separate them is not another size threshold but a second signal, and
the document offers an obvious one: a heading has extra space above it. The
genuine headings sit 18–30 pt below the preceding line against a 14 pt body
pitch, while the note's lines sit 14.5–16.5 pt apart. That is the same class of
evidence as the bander's other rules — geometry the producer already committed to
— and it is the natural next step. It is not in this slice.

This one does not move the token counts at all: every character survives, and the
error rate is unchanged. It is visible fidelity, not extraction, which is why it
is worth fixing next rather than now — a heading rule is the kind of thing worth
agreeing on before it is written, and finding D2's note that H1 and H2 may differ
only by italic is the same open question.

## Correcting the earlier reading-order question

The note that table reconstruction "needs the x-segmentation stage before it can
even be evaluated" is resolved: x-segmentation works — columns separate cleanly,
and the simple glossary table in `TP2 Admin BD` is recovered as a real 3×2 table.
What defeats it is **wrapped cells**. In that glossary the first column wraps to
two lines and the second to three, so a visual line is not a row, and the band
holding only the tail of one cell (`temps.`) has one segment where its neighbours
have two. The region stops looking like a table and falls back to flow, which
interleaves a wrapped cell with its neighbour.

That accounts for the whole of the remaining error: of 140 differing tokens on
the second document, 3 are the lost hyphens and 137 are this. It is a
well-defined feature — group bands into rows by column membership, then merge a
cell's wrapped lines — rather than an open question.

## What the thin slice established

- The architecture holds end to end: reader → IR → writer, with the CLI sharing
  every line of it with the GUI.
- `dconv --dump` prints the analysed structure, and
  `cargo run -p dconv-pdf --example bands` prints the bands and the geometry
  behind them. Both exist because "why did this line come out wrong?" is the
  question that actually gets asked, and it is answerable in seconds.
- A fidelity number is only trustworthy if the thing being measured is the file
  a user opens. The first measurement here was wrong in a way that looked like a
  conversion bug: the harness read text node by node and so concatenated a list
  marker with its item across a `<w:tab/>`, which reads as `•Tester`. That is now
  a rule about `w:tab` and `w:br`, not a guess.

