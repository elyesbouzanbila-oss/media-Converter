//! Meaning: bands and segments become paragraphs, headings, lists and tables.
//!
//! Everything here is a judgement, and every judgement is wrong sometimes. The
//! approach is to make each one explicitly, in one place, with the evidence it
//! uses stated — so that when a document converts badly it is obvious which
//! rule to change, rather than a diffuse sense that "the converter is bad at
//! this kind of file".
//!
//! Two design notes worth stating up front:
//!
//! * **The body size is a document-level fact.** Classifying a line as a heading
//!   means comparing it against the rest of the document, not against its own
//!   page, so the profile is computed once over every page before any block is
//!   built.
//!
//! * **A region is classified once, not line by line.** Tables and multi-column
//!   layouts are properties of a *span of rows*: a single line cannot be a
//!   table. Deciding per region is what makes the two distinguishable at all.

use dconv_ir::{
    Align, Block, ColumnSpec, Inline, Length, Numbering, TableBlock, TableCell, TableRow, TextRun,
    TextStyle,
};
use std::cmp::Ordering;

use crate::ir;
use crate::layout::{Band, Run, Segment};
use crate::rules::Rule;

/// A longer line is never a heading, however large its type. Guards against a
/// stray oversized glyph headlining an entire paragraph.
const HEADING_MAX_CHARS: usize = 90;

/// How far a segment's left edge may drift and still count as the same table
/// column, as a fraction of the run's largest glyph.
const COLUMN_TOLERANCE_EM: f32 = 0.9;

/// Fewer aligned rows than this is not a table. Two lines that happen to line up
/// are a coincidence of layout, not structure.
const MIN_TABLE_ROWS: usize = 2;

/// The fewest horizontal rules that must fall inside a table before absence of
/// a rule between two bands means "the same row".
///
/// A printed table very often rules only its header. Treating every unruled gap
/// as a continuation there would merge the whole body into one row, so one
/// internal rule is never enough. The glossary this rule exists for carries two
/// (page 2 of `TP2 Admin BD 2026_2027.pdf`, at y=710.77 and 677.37); a fully
/// ruled table with only two rows has one and stays on the subset rule, which
/// is the safe side.
const MIN_INTERNAL_RULES: usize = 2;

/// How much of a table's text width a rule must cross to bound its rows.
///
/// Word draws one border per cell, and `rules::merge` joins the pieces; a
/// single cell's border alone covers only 28% of the two-column glossary's
/// text width, so an unmerged fragment is not evidence of a row boundary.
const RULE_COVERAGE: f32 = 0.5;

/// A two-segment region spanning more than this fraction of the content height
/// is a two-column page rather than a one-column table.
const COLUMN_LAYOUT_HEIGHT_FRACTION: f32 = 0.62;

/// How close to the flow's right edge a line must end to look "full".
///
/// Three em is roughly one long word, which is the most a genuinely full line
/// can fall short of the margin — the last word did not fit. Below that, real
/// prose breaks into one paragraph per line; much above it, a paragraph's short
/// final line stops being visible at all.
const FULL_LINE_TOLERANCE_EM: f32 = 3.0;

/// A line pulled in from both margins by at least this much, by similar amounts,
/// is centred.
const CENTRED_SLACK_EM: f32 = 1.5;

/// How far a line may be indented past the flow's left edge and still continue a
/// paragraph.
const INDENT_TOLERANCE_EM: f32 = 0.9;

const BULLETS: &[char] = &['•', '▪', '‣', '·', '○', '◦', '▪', '–', '—', '-', '*'];

/// The smallest gap through which a fused segment may be split when page-level
/// evidence says two columns meet there.
///
/// Measured on the real documents: the inter-glyph gaps that ARE column
/// boundaries run 6.86pt (the glossary term running into its definition, page 2
/// of `TP2 Admin BD 2026_2027.pdf`) up to 16.3pt (footnote marker 2 hanging in
/// the table gutter, page 3), which is 0.62em to 1.48em of the 11pt body.
/// Ordinary word gaps in the same justified text measure up to 7pt, so raw
/// width cannot separate a boundary from a stretched space, and no constant
/// tries to. What separates them is that a boundary is *reusable*: the cut must
/// clear every band it crosses, not just the band it came from — that is what
/// the window walk enforces. This constant only has to reject gaps that could
/// never carry a boundary (kerning, tight spacing) while the measured tightest
/// boundary, 0.62em, stays admissible.
const CUT_MIN_GAP_EM: f32 = 0.55;

/// A cut must leave at least this much ink on each side of its gap, in em of
/// the segment's own size. The footnote markers the cut peels onto the table
/// side measure 5.87pt wide — 0.53em of the 11pt body — so anything slimmer
/// than that is kerning, not a leftover word.
const MIN_CUT_PIECE_EM: f32 = 0.5;

/// How much of a window's content width each side of a cut must carry. This is
/// the second strip gate from the working notes: the account table's own
/// internal gutter, measured at [501.20, 521.77] on page 3 of
/// `TP2 Admin BD 2026_2027.pdf`, leaves 28.9pt on the right of a 490pt window —
/// 5.9% — and is refused here as well as by the overlap rule below.
const MIN_STRIP_SHARE: f32 = 0.12;

/// The fewest bands that must carry content on both sides of a cut before the
/// cut means a column boundary rather than a coincidence of spacing.
const MIN_STRIP_STRADDLERS: usize = 2;

/// A window must collect at least this many pieces on one side before a cut
/// means a structure this pass exists for. Every multi-column case the pass is
/// for carries three or more on a side (the script list: eight on the left);
/// demanding three refuses the two-piece accident where two consecutive
/// justified lines each happen to carry the cut and would otherwise read as a
/// two-row table. Structures that small still classify as tables through the
/// ordinary region rules, which need no cut at all.
const MIN_STRIP_PIECES: usize = 3;

/// A one-sided band joins an opened window only while its largest glyph stays
/// within this factor of the largest glyph the two-sided bands carry. The
/// trailing list items (11pt under an 11pt body) and a Session exercise's
/// wrapped cell line belong to the strip; "Question :" (12pt, page 4) and
/// "3. Contrôle transactionnel" (13pt, page 3) do not.
const TRAILING_SIZE_FACTOR: f32 = 1.05;

/// Document-wide typographic facts, derived before any block is built.
#[derive(Debug, Clone)]
pub struct DocumentProfile {
    /// The size most of the document's text is set in.
    pub body_size: f32,
    /// Distinct sizes above the body size, largest first: index 0 is heading
    /// level 1.
    pub heading_sizes: Vec<f32>,
}

impl DocumentProfile {
    /// Derive the profile from every band on every page.
    ///
    /// Weighted by character count rather than by line count, so a page of
    /// twenty tiny footnotes cannot outvote the body text.
    pub fn from_bands(pages: &[Vec<Band>]) -> Self {
        let mut histogram: Vec<(i32, usize)> = Vec::new();

        for band in pages.iter().flatten() {
            for segment in &band.segments {
                let key = (segment.size * 2.0).round() as i32;
                let weight = segment.char_count().max(1);
                match histogram.iter_mut().find(|(k, _)| *k == key) {
                    Some((_, total)) => *total += weight,
                    None => histogram.push((key, weight)),
                }
            }
        }

        // Ties break towards the smaller size: with no majority there is no bulk
        // to follow, and body text is the smaller of the plausible candidates.
        let body_key = histogram
            .iter()
            .max_by_key(|(key, count)| (*count, std::cmp::Reverse(*key)))
            .map(|(key, _)| *key)
            .unwrap_or(22);
        let body_size = body_key as f32 / 2.0;

        let mut heading_sizes: Vec<f32> = histogram
            .iter()
            .map(|(key, _)| *key as f32 / 2.0)
            .filter(|size| *size > body_size + 0.5)
            .collect();
        heading_sizes.sort_by(|a, b| b.partial_cmp(a).unwrap_or(Ordering::Equal));
        heading_sizes.truncate(6);

        Self {
            body_size,
            heading_sizes,
        }
    }

    /// Heading level for a size, if it is heading-sized at all.
    ///
    /// Size is the only signal used. On one real document the H1 and H2 are both
    /// 13pt and differ only by italic, so this will merge them — a known gap,
    /// recorded rather than papered over with a guess.
    pub fn level_for(&self, size: f32) -> Option<u8> {
        if size <= self.body_size + 0.5 {
            return None;
        }
        let index = self
            .heading_sizes
            .iter()
            .position(|candidate| (candidate - size).abs() < 0.6)?;
        Some((index + 1).min(6) as u8)
    }
}

/// One visual line, ready to be classified.
///
/// A line's vertical extent is not kept: it is only needed to measure a whole
/// aligned region, and that is measured from the bands, which carry the extent
/// for every segment rather than only for the first and last.
#[derive(Debug, Clone)]
struct Line {
    inlines: Vec<Inline>,
    text: String,
    left: f32,
    right: f32,
    size: f32,
    bold: bool,
    italic: bool,
}

impl Line {
    fn from_band(band: &Band) -> Self {
        let mut inlines = Vec::new();
        for (index, segment) in band.segments.iter().enumerate() {
            if index > 0 {
                inlines.push(Inline::text(" "));
            }
            inlines.extend(segment_inlines(segment));
        }

        Self {
            inlines,
            text: band.text(),
            left: band.segments.first().map(|s| s.left).unwrap_or(0.0),
            right: band.segments.last().map(|s| s.right).unwrap_or(0.0),
            size: weighted_size(&band.segments),
            bold: weighted_majority(&band.segments, |s| s.bold),
            italic: weighted_majority(&band.segments, |s| s.italic),
        }
    }
}

/// Build the blocks for one page.
pub fn build_page(
    bands: &[Band],
    rules: &[Rule],
    profile: &DocumentProfile,
    page: &ir::PageSetup,
) -> Vec<Block> {
    let (bands, windows) = cut_columns(bands);
    let mut blocks = Vec::new();
    let mut index = 0;

    while index < bands.len() {
        // A window the cut pass already decided: row-major windows go to the
        // table builder directly; stream windows are read as two regions, the
        // left strip first, the right after — the reading MuPDF gives the
        // script list beside the account table.
        if let Some((_, window)) = windows.iter().find(|(start, _)| *start == index) {
            match (&window.columns, window.cut) {
                (Some(columns), _) => {
                    blocks.push(build_table(&bands, index, window.end, columns, rules));
                }
                (None, Some(cut)) => {
                    let (lefts, rights) =
                        split_window(&bands[index..window.end], cut);
                    emit_region(&lefts, rules, profile, page, &mut blocks);
                    emit_region(&rights, rules, profile, page, &mut blocks);
                }
                (None, None) => unreachable!("a recorded window is a table or a stream"),
            }
            index = window.end;
            continue;
        }

        // Everything before the next recorded window is one region.
        let end = windows
            .iter()
            .map(|(start, _)| *start)
            .filter(|start| *start > index)
            .min()
            .unwrap_or(bands.len())
            .max(index + 1);
        emit_region(&bands[index..end], rules, profile, page, &mut blocks);
        index = end;
    }

    blocks
}

/// Classify and emit one region of bands — the part of a page between
/// recorded strip windows, or one side of a stream window.
fn emit_region(
    bands: &[Band],
    rules: &[Rule],
    profile: &DocumentProfile,
    page: &ir::PageSetup,
    blocks: &mut Vec<Block>,
) {
    let mut index = 0;
    while index < bands.len() {
        match classify_region(bands, index, page) {
            Region::Table { end, columns } => {
                blocks.push(build_table(bands, index, end, &columns, rules));
                index = end;
                continue;
            }
            Region::Columns { end, count } => {
                emit_columns(bands, index, end, count, profile, blocks);
                index = end;
                continue;
            }
            Region::Flow => {}
        }

        // Gather the run of rows that are neither of the above, so that the flow
        // logic sees a whole page of prose at once and can use its margins.
        let start = index;
        let mut end = index + 1;
        while end < bands.len() && matches!(classify_region(bands, end, page), Region::Flow) {
            end += 1;
        }
        emit_flow(&bands[start..end], profile, blocks);
        index = end;
    }
}

/// What a span of rows, taken from `start`, structurally is.
#[derive(Debug)]
enum Region {
    /// Ordinary prose.
    Flow,
    Table {
        end: usize,
        columns: Vec<f32>,
    },
    /// A multi-column page region: read each column to the bottom, then the next.
    Columns {
        end: usize,
        count: usize,
    },
}

/// Classify the region beginning at `start`.
///
/// Both structural cases look identical from the outside — several consecutive
/// rows, each divided into the same number of horizontally separated runs — so
/// the only thing separating a table from a two-column page is how much of the
/// page the aligned span covers. A table is part of a page; a column layout is
/// all of it.
///
/// What makes a real table awkward is that a cell may wrap: the row's tallest
/// cell then continues on a band of its own, and that band carries fewer
/// segments than the rows around it. A shape test ("same number of segments,
/// same left edges") stops at the first wrapped cell, which is how
/// `TP2 Admin BD 2026_2027.pdf` loses its ACID glossary. The run is grown by
/// *membership* instead — every segment has to sit on one of the run's columns —
/// and only the columns two bands agree on are trusted.
fn classify_region(bands: &[Band], start: usize, page: &ir::PageSetup) -> Region {
    let first = &bands[start].segments;
    if first.len() < 2 {
        return Region::Flow;
    }

    // Grow first, then keep only the columns that repeat. A column carrying a
    // single band is not structure: it is two runs that happen to share an x,
    // which is what a line of the list beside a table looks like (measured on
    // page 3 of `TP2 Admin BD 2026_2027.pdf`, where the list's first two lines
    // and the table's first two rows share columns 60.62 and 521.77 and would
    // otherwise read as a table of their own). Pruning can cut the run short,
    // so it repeats until it settles.
    let mut end = grow_run(bands, start);
    let columns = loop {
        let columns = repeated_columns(bands, start, end);
        if columns.len() < 2 {
            return Region::Flow;
        }

        let tolerance = run_tolerance(&bands[start..end]);
        let strays = (start..end).find(|index| {
            bands[*index].segments.iter().any(|segment| {
                !columns
                    .iter()
                    .any(|column| (segment.left - column).abs() <= tolerance)
            })
        });

        match strays {
            None => break columns,
            Some(index) if index == start => return Region::Flow,
            Some(index) => end = index,
        }
    };

    // Rows, not bands: a wrapped cell adds a band to a row that already exists.
    let tolerance = run_tolerance(&bands[start..end]);
    let rows = (start..end)
        .filter(|index| {
            match_columns(&bands[*index].segments, &columns, tolerance)
                .iter()
                .any(|column| *column == Some(0))
        })
        .count();

    if rows < MIN_TABLE_ROWS {
        return Region::Flow;
    }

    let height = bands[start].top - bands[end - 1].bottom;
    let content_height = page.content_height().as_pt() as f32;
    let spans_the_page = content_height > 0.0 && height > content_height * COLUMN_LAYOUT_HEIGHT_FRACTION;

    if columns.len() == 2 && spans_the_page {
        return Region::Columns { end, count: 2 };
    }

    Region::Table { end, columns }
}

/// The column each segment sits on, or `None` where one sits on no column.
fn match_columns(segments: &[Segment], columns: &[f32], tolerance: f32) -> Vec<Option<usize>> {
    segments
        .iter()
        .map(|segment| {
            columns
                .iter()
                .position(|column| (segment.left - column).abs() <= tolerance)
        })
        .collect()
}

/// The drift a column may absorb: part of the largest glyph in the run.
fn run_tolerance(bands: &[Band]) -> f32 {
    bands
        .iter()
        .flat_map(|band| band.segments.iter())
        .map(|segment| segment.size)
        .fold(0.0f32, f32::max)
        * COLUMN_TOLERANCE_EM
}

/// The columns a run of bands genuinely repeats: only a left edge that two or
/// more bands agree on.
fn repeated_columns(bands: &[Band], start: usize, end: usize) -> Vec<f32> {
    let tolerance = run_tolerance(&bands[start..end]);
    let mut clusters: Vec<(f32, usize)> = Vec::new();

    for band in &bands[start..end] {
        let mut seen: Vec<usize> = Vec::new();
        for segment in &band.segments {
            let index = clusters
                .iter()
                .position(|(left, _)| (segment.left - left).abs() <= tolerance)
                .unwrap_or_else(|| {
                    clusters.push((segment.left, 0));
                    clusters.len() - 1
                });
            // One band fills a column once, however many segments land on it.
            if !seen.contains(&index) {
                seen.push(index);
                clusters[index].1 += 1;
            }
        }
    }

    let mut columns: Vec<f32> = clusters
        .into_iter()
        .filter(|(_, bands_seen)| *bands_seen >= 2)
        .map(|(left, _)| left)
        .collect();
    columns.sort_by(|a, b| a.total_cmp(b));
    columns
}

/// How far the run that begins at `start` reaches: the longest band sequence in
/// which every segment sits on a column the sequence already has, or brings a
/// column of its own while filling at least two known ones.
///
/// The second case is a header cell merged with the one beside it: its left edge
/// is on the first column and its text runs over the second, so the column it
/// covers is one no earlier band names. Requiring it to fill two known columns
/// at the same time is what keeps an ordinary sentence from inventing columns.
fn grow_run(bands: &[Band], start: usize) -> usize {
    let mut columns: Vec<f32> = bands[start].segments.iter().map(|s| s.left).collect();
    columns.sort_by(|a, b| a.total_cmp(b));

    let mut end = start + 1;
    while end < bands.len() {
        let segments = &bands[end].segments;
        if segments.is_empty() {
            break;
        }

        let tolerance = run_tolerance(&bands[start..=end]);
        let mut matched = match_columns(segments, &columns, tolerance);
        let known = matched.iter().filter(|column| column.is_some()).count();

        if known < segments.len() {
            if known < 2 {
                break;
            }
            for segment in segments {
                if !columns.iter().any(|column| (segment.left - column).abs() <= tolerance)
                {
                    columns.push(segment.left);
                }
            }
            columns.sort_by(|a, b| a.total_cmp(b));
            matched = match_columns(segments, &columns, tolerance);
        }

        // A lone run on the first column is where a table meets the prose below
        // it: a paragraph's first line starts at the table's left column, and
        // nothing on the line itself says whether it belongs to the table.
        // Ending the run there is the conservative reading, and it is what
        // keeps a table flush with the text margin from swallowing the
        // paragraphs that follow it - and a two-column layout, whose columns
        // merge into one run wherever the left column's text reaches the right
        // one's, from reading as a table of one-column rows.
        if segments.len() == 1 && matched[0] == Some(0) {
            break;
        }

        end += 1;
    }

    end
}

/// One page-region refinement: a run of bands drawn from two vertical streams
/// or a two-column structure, cut apart at a known boundary before any region
/// is classified.
#[derive(Debug)]
struct StripWindow {
    /// One past the last band of the window.
    end: usize,
    /// For a row-major window (`table`): the two columns, already cut into
    /// every band of the window.
    columns: Option<Vec<f32>>,
    /// For a stream window: the cut each side was drawn at, applied at emit
    /// time — the bands stay interleaved here so every index in the vector
    /// keeps its meaning.
    cut: Option<f32>,
}

/// What a per-candidate cut evaluation concluded.
#[derive(Debug)]
struct StripPlan {
    /// First band carrying content on both sides of the cut: the window opens
    /// there, because one-sided bands above the structure belong to the flow
    /// (a heading, page furniture), not to a side.
    start: usize,
    /// One past the last band of the window.
    end: usize,
    cut: f32,
    table: bool,
    columns: Vec<f32>,
}

/// Cut the columns page-level evidence identifies, before anything is
/// classified.
///
/// Two structures defeat region classification from the outside, and both live
/// on the real documents: a segment that fused two columns — the footnote
/// markers `1`/`2`/`3` the producer set at the account table's own left edge
/// inside the list's lines, "[A1] Modifiez (sans valider) le solde du" running
/// into "[B1] Interrogez le solde du compte n°1." across a 10.7pt gap, the
/// glossary term running into its definition across 6.9pt — so no band shows
/// the columns it belongs to; and one-sided bands (the list beside the account
/// table) poisoning every column the region machinery grows. The cut pass
/// works the other way round from classification: it asks where a page could
/// be divided into vertical strips — the question the working notes posed —
/// and once per window decides between two readings of the same split:
///
/// * both sides line up band after band → row-major: a table. The glossary and
///   the Session A/B blocks want this (MuPDF reads them row by row), and the
///   window's columns are handed to the table builder directly.
/// * one side is its own stream → column-major: every band's left pieces in
///   band order, then every band's right pieces. The script list beside the
///   account table wants this.
///
/// Both gates from the working notes apply to every candidate: at least two
/// bands carrying content on both sides, and both sides carrying a real share
/// of the window's width — which is what refuses the account table's own
/// internal gutter. Two further gates keep a cut honest: the cut may not leave
/// overlapping ink inside one side (at a cut inside the account table, the
/// marker cells would overlap the list's own segments, because they have no
/// independent existence), and full-width lines are never carriers.
fn cut_columns(bands: &[Band]) -> (Vec<Band>, Vec<(usize, StripWindow)>) {
    let mut bands: Vec<Band> = bands.to_vec();
    let mut windows: Vec<(usize, StripWindow)> = Vec::new();
    let mut index = 0;

    while index < bands.len() {
        if let Some(plan) = best_strip(&bands, index) {
            if plan.table {
                // Row-major: fold the cut pieces into each band now; every
                // index stays where it was.
                apply_cut(&mut bands, plan.start, plan.end, plan.cut);
                windows.push((
                    plan.start,
                    StripWindow {
                        end: plan.end,
                        columns: Some(plan.columns),
                        cut: None,
                    },
                ));
            } else {
                // Stream: record the cut, keep the bands interleaved — the
                // split into sides happens when the window is emitted, each
                // side then classified as its own region.
                windows.push((
                    plan.start,
                    StripWindow {
                        end: plan.end,
                        columns: None,
                        cut: Some(plan.cut),
                    },
                ));
            }
            index = plan.end;
        } else {
            index += 1;
        }
    }

    (bands, windows)
}

/// Split a stream window's bands at its cut, handing back the left and right
/// strips as band sequences in band order. Evaluation walked the same bands in
/// the same state, so a refusal here would mean the two disagreed.
fn split_window(window: &[Band], cut: f32) -> (Vec<Band>, Vec<Band>) {
    let mut lefts = Vec::new();
    let mut rights = Vec::new();

    for band in window {
        let Some((left, right)) = split_band(band, cut) else {
            unreachable!("the band admitted this cut during evaluation");
        };
        if !left.is_empty() {
            lefts.push(Band {
                segments: left,
                bottom: band.bottom,
                top: band.top,
            });
        }
        if !right.is_empty() {
            rights.push(Band {
                segments: right,
                bottom: band.bottom,
                top: band.top,
            });
        }
    }

    (lefts, rights)
}

/// Whether any run of the segment is set in a monospaced programming face,
/// judged by the embedded (subset-prefixed) font name.
///
/// Code lines ("SET TRANSACTION …; -- fixe les propriétés", page 1 of
/// `TP2 Admin BD 2026_2027.pdf`, and the SELECT lines of 2.6 on page 2) hang
/// their trailing comment at a comment column a few points past the code, and
/// Consolas's word gaps are carrier-sized, so every space in them invites a
/// cut. Real column boundaries — the glossary at 206.67, the Session blocks
/// at 302.70 — live in proportional text. A monospaced run therefore never
/// carries a cut, and the code|comment pseudo-columns its lines invite read
/// line-major, which is what the fidelity reference gives them too.
///
/// The advance-statistics alternative was measured and discarded: the
/// producer quantizes Consolas's positions to ~0.1pt so its steps alternate
/// between two widths 3% apart, while display faces ("Anomalies et Niveaux
/// d'Isolation", 13pt, advances n/o/a/e/u/d at exactly 6.19pt) and this
/// document's own Aptos body text both produce more equal-looking steps than
/// the code does. Names separate the classes exactly where statistics
/// cannot.
const MONO_FONTS: &[&str] = &[
    "consolas",
    "courier",
    "mono",
    "menlo",
    "inconsolata",
    "jetbrains",
    "fira code",
    "source code",
    "pragmata",
    "hack",
];

fn monospace(segment: &Segment) -> bool {
    segment.runs.iter().any(|run| {
        run.font
            .as_deref()
            .is_some_and(|font| {
                let font = font.to_ascii_lowercase();
                MONO_FONTS.iter().any(|mark| font.contains(mark))
            })
    })
}

/// The best cut for a window beginning anywhere at or after `scan`.
///
/// Candidates come from the recorded inter-glyph gaps themselves — a cut has to
/// sit inside a gap of every segment it crosses, so its position is bounded by
/// the gaps of the bands it came from. Several cuts inside the same compatible
/// gap produce identical pieces (every character boundary is fixed), so the
/// choice only has to prefer the window whose bands sit nearest the scan
/// point — keep an incumbent unless a newcomer opens earlier (or, from the
/// same first band, runs wider).
fn best_strip(bands: &[Band], scan: usize) -> Option<StripPlan> {
    let mut best: Option<StripPlan> = None;

    for band in &bands[scan..] {
        for segment in &band.segments {
            let minimum = segment.size * CUT_MIN_GAP_EM;
            for &(gap_right, gap_width) in &segment.gaps {
                if gap_width < minimum {
                    continue;
                }
                let cut = gap_right + gap_width * 0.5;
                if let Some(plan) = evaluate_cut(bands, scan, cut) {
                    best = Some(match best {
                        // Adopt the newcomer when it starts earlier (or,
                        // from the same band, runs wider); keep the
                        // incumbent otherwise.
                        Some(previous) if better_strip(&previous, &plan) => plan,
                        Some(previous) => previous,
                        None => plan,
                    });
                }
            }
        }
    }

    best
}

fn better_strip(previous: &StripPlan, plan: &StripPlan) -> bool {
    // The structure nearest the scan point is read first: a window opening
    // above another one belongs to a different, smaller block — page 4's
    // 5.1 table [1..5) — and must not wait behind a wider later window
    // [10..16) whose only claim is reach. Equal starts adopt the wider
    // reach; equal windows keep the first evaluated.
    plan.start < previous.start
        || (plan.start == previous.start && plan.end > previous.end)
}

/// Evaluate one candidate cut: walk the bands from `scan` while they admit it,
/// gate the window that opens, and decide row-major against stream.
fn evaluate_cut(bands: &[Band], scan: usize, cut: f32) -> Option<StripPlan> {
    let mut window: Vec<(usize, Vec<Segment>, Vec<Segment>)> = Vec::new();
    let mut straddlers = 0usize;
    let mut straddler_size = 0.0f32;

    for (index, band) in bands.iter().enumerate().skip(scan) {
        let (left, right) = match split_band(band, cut) {
            Some(pieces) => pieces,
            // The cut does not clear this band. Above the structure that
            // means the band is not part of any column read — the question
            // lines and the heading between a block and its predecessor
            // refuse every cut, and the Session 5.2 block's fused row died
            // against them when a refusal here killed the candidate. Bands
            // have to be straddled into a window, not admitted into one, so
            // what sits above the first straddler is skipped. Once the
            // window opened there is structure around the cut and a refusal
            // is a real mismatch: it closes the window.
            None => {
                if window.is_empty() {
                    continue;
                }
                break;
            }
        };

        let straddles = !left.is_empty() && !right.is_empty();
        if straddles {
            let largest = left
                .iter()
                .chain(&right)
                .map(|s| s.size)
                .fold(0.0f32, f32::max);
            straddler_size = straddler_size.max(largest);
            straddlers += 1;
        } else if !window.is_empty() {
            // A one-sided band after the window opened: a trailing continuation
            // (the list's last items, a Session cell's wrapped line) only
            // joins while its glyphs stay the size of the structure's.
            let size = left
                .iter()
                .chain(&right)
                .map(|s| s.size)
                .fold(0.0f32, f32::max);
            if size > straddler_size * TRAILING_SIZE_FACTOR {
                break;
            }
        }

        if !window.is_empty() || straddles {
            window.push((index, left, right));
        }
    }

    if window.is_empty() {
        return None;
    }
    let start = window[0].0;
    let end = window.last().expect("non-empty").0 + 1;
    if straddlers < MIN_STRIP_STRADDLERS {
        return None;
    }

    let mut left_pieces = 0usize;
    let mut right_pieces = 0usize;
    let mut left_min = f32::INFINITY;
    let mut left_max = f32::NEG_INFINITY;
    let mut right_min = f32::INFINITY;
    let mut right_max = f32::NEG_INFINITY;
    let mut content_min = f32::INFINITY;
    let mut content_max = f32::NEG_INFINITY;
    let mut size_max = 0.0f32;
    let mut left_min_lefts: Vec<f32> = Vec::new();
    let mut right_min_lefts: Vec<f32> = Vec::new();

    for &(_, ref left, ref right) in &window {
        if !left.is_empty() {
            left_pieces += left.len();
            let min_left = left.iter().map(|s| s.left).fold(f32::INFINITY, f32::min);
            let max_right = left.iter().map(|s| s.right).fold(f32::NEG_INFINITY, f32::max);
            left_min = left_min.min(min_left);
            left_max = left_max.max(max_right);
            content_min = content_min.min(min_left);
            content_max = content_max.max(max_right);
            size_max = size_max.max(left.iter().map(|s| s.size).fold(0.0f32, f32::max));
            left_min_lefts.push(min_left);
        }
        if !right.is_empty() {
            right_pieces += right.len();
            let min_left = right.iter().map(|s| s.left).fold(f32::INFINITY, f32::min);
            let max_right = right.iter().map(|s| s.right).fold(f32::NEG_INFINITY, f32::max);
            right_min = right_min.min(min_left);
            right_max = right_max.max(max_right);
            content_min = content_min.min(min_left);
            content_max = content_max.max(max_right);
            size_max = size_max.max(right.iter().map(|s| s.size).fold(0.0f32, f32::max));
            right_min_lefts.push(min_left);
        }
    }

    if left_pieces.max(right_pieces) < MIN_STRIP_PIECES {
        return None;
    }

    // Both sides carry a real share of the window's width — the account
    // table's own internal gutter leaves 5.9% on the right and is refused
    // here as well as by the overlap rule.
    let total = content_max - content_min;
    if !(total > 0.0) {
        return None;
    }
    let left_share = (left_max - left_min) / total;
    let right_share = (right_max - right_min) / total;
    if left_share < MIN_STRIP_SHARE || right_share < MIN_STRIP_SHARE {
        return None;
    }

    // Both sides line up band after band → a two-column structure read
    // row-major. An anchor is the first band's minimum left; every band with
    // that side filled must agree within part of its largest glyph.
    let tolerance = run_tolerance(&bands[start..end]);
    let left_anchor = side_anchor(&left_min_lefts, tolerance);
    let right_anchor = side_anchor(&right_min_lefts, tolerance);

    let plan = match (left_anchor, right_anchor) {
        (Some(left), Some(right)) => StripPlan {
            start,
            end,
            cut,
            table: true,
            columns: vec![left, right],
        },
        _ => StripPlan {
            start,
            end,
            cut,
            table: false,
            columns: Vec::new(),
        },
    };
    Some(plan)
}

/// The common left edge of a side whose bands all agree, or `None` where they
/// drift beyond part of their largest glyph.
fn side_anchor(min_lefts: &[f32], tolerance: f32) -> Option<f32> {
    let first = *min_lefts.first()?;
    (min_lefts
        .iter()
        .all(|left| (left - first).abs() <= tolerance))
    .then_some(first)
}

/// Split one band at `cut`; `None` when the cut crosses ink it may not cut.
fn split_band(band: &Band, cut: f32) -> Option<(Vec<Segment>, Vec<Segment>)> {
    let mut left: Vec<Segment> = Vec::new();
    let mut right: Vec<Segment> = Vec::new();

    for segment in &band.segments {
        if segment.right <= cut {
            left.push(segment.clone());
        } else if segment.left >= cut {
            right.push(segment.clone());
        } else {
            // A full-width line was once refused outright — its word gaps
            // are wide enough to carry any cut. The Session blocks measure
            // the cost: page 4's 5.2 fused row spans [62.83, 534.47] =
            // 471.64pt across the 457.9pt threshold and carries the cut
            // inside its own (300.54, 10.16) gap, so the refusal threw away
            // the strongest straddler its window has and every Session
            // block reads as cross-read flow. cut_segment alone now guards
            // the piece: the cut must land inside a ≥0.55 em gap of this
            // segment with both pieces ≥ its minimum, monospace refuses,
            // and the window still needs corroboration at evaluate_cut —
            // ≥2 straddlers, ≥3 pieces per side, ≥12% shares and aligned
            // anchors. A false window would need consecutive lines whose
            // word gaps align within a point, which justified running text
            // does not do.
            let (left_piece, right_piece) = cut_segment(segment, cut)?;
            if let Some(piece) = left_piece {
                left.push(piece);
            }
            if let Some(piece) = right_piece {
                right.push(piece);
            }
        }
    }

    // The cut may not leave overlapping ink inside one side. The marker cells
    // at the account table's own gutter are the measured case: cut there, the
    // `1` cell inside [372.72, 378.59] overlaps the list segment
    // [60.62, 378.59] it hangs beside, because those cells have no independent
    // existence on their side.
    if overlaps_within(&left) || overlaps_within(&right) {
        return None;
    }

    left.sort_by(|a, b| a.left.total_cmp(&b.left));
    right.sort_by(|a, b| a.left.total_cmp(&b.left));
    Some((left, right))
}

/// Whether any two segments of one side overlap horizontally.
fn overlaps_within(segments: &[Segment]) -> bool {
    let mut ordered: Vec<&Segment> = segments.iter().collect();
    ordered.sort_by(|a, b| a.left.total_cmp(&b.left));
    ordered
        .windows(2)
        .any(|pair| pair[0].right > pair[1].left + 0.25)
}

/// Apply a decided cut to a window's bands, folding both sides into each band
/// left to right. Row-major reading follows from the ordinary machinery.
fn apply_cut(bands: &mut [Band], start: usize, end: usize, cut: f32) {
    for band in &mut bands[start..end] {
        let (left, right) = match split_band(band, cut) {
            Some(pieces) => pieces,
            None => unreachable!("the band admitted this cut during evaluation"),
        };
        let mut segments = left;
        segments.extend(right);
        band.segments = segments;
    }
}

/// Cut a fused segment through the recorded gap that contains `cut`, handing
/// back (left piece, right piece).
///
/// `None` when the gap is too narrow for the column rule, when either side
/// would be left slimmer than a short word — the footnote markers measure
/// 5.87pt, the bar this sets — or when the cut lands inside a glyph, which no
/// gap contains.
fn cut_segment(segment: &Segment, cut: f32) -> Option<(Option<Segment>, Option<Segment>)> {
    // Monospace text never carries: its constant advance repeats any space as
    // a carrier-sized gap, and slicing a code line's trailing comment away is
    // what builds the fake code|comment tables the reference does not have.
    if monospace(segment) {
        return None;
    }
    let &(gap_right, gap_width) = segment
        .gaps
        .iter()
        .find(|&&(right, width)| right < cut && cut < right + width)?;
    let minimum_piece = segment.size * MIN_CUT_PIECE_EM;
    if gap_width < segment.size * CUT_MIN_GAP_EM
        || gap_right - segment.left < minimum_piece
        || segment.right - (gap_right + gap_width) < minimum_piece
    {
        return None;
    }

    // First character at or after the cut: everything before it stays left.
    let mut boundary: Option<(usize, usize)> = None;
    'outer: for (run_index, run) in segment.runs.iter().enumerate() {
        for (char_index, &char_x) in run.x.iter().enumerate() {
            if char_x >= cut {
                boundary = Some((run_index, char_index));
                break 'outer;
            }
        }
    }
    let Some((run_index, char_index)) = boundary else {
        return None;
    };

    let mut left_runs: Vec<Run> = Vec::new();
    let mut right_runs: Vec<Run> = Vec::new();
    for (index, run) in segment.runs.iter().enumerate() {
        if index < run_index {
            left_runs.push(run.clone());
        } else if index > run_index {
            right_runs.push(run.clone());
        } else {
            let mut head = run.clone();
            let mut tail = run.clone();
            head.text = run.text.chars().take(char_index).collect();
            head.x = run.x[..char_index].to_vec();
            tail.text = run.text.chars().skip(char_index).collect();
            tail.x = run.x[char_index..].to_vec();
            if !head.text.is_empty() {
                left_runs.push(head);
            }
            if !tail.text.is_empty() {
                right_runs.push(tail);
            }
        }
    }

    trim_trailing_runs(&mut left_runs);
    trim_leading_runs(&mut right_runs);
    if left_runs.is_empty() || right_runs.is_empty() {
        return None;
    }

    let left = Segment {
        left: segment.left,
        right: gap_right,
        runs: left_runs,
        gaps: segment
            .gaps
            .iter()
            .copied()
            .filter(|&(right, _)| right < gap_right)
            .collect(),
        ..segment.clone()
    };
    let right = Segment {
        left: right_runs
            .first()
            .and_then(|run| run.x.first())
            .copied()
            .unwrap_or(gap_right + gap_width),
        right: segment.right,
        runs: right_runs,
        gaps: segment
            .gaps
            .iter()
            .copied()
            .filter(|&(right, _)| right > gap_right)
            .collect(),
        ..segment.clone()
    };
    Some((Some(left), Some(right)))
}

/// Drop a run's leading spaces together with their recorded x positions.
/// Keeping `x` parallel to `text` is the whole point: the analyser cuts
/// segments through the recorded gaps, so a position must not survive its
/// character.
fn trim_leading_runs(runs: &mut Vec<Run>) {
    while let Some(first) = runs.first_mut() {
        let dropped =
            first.text.chars().count() - first.text.trim_start_matches(' ').chars().count();
        if dropped == 0 {
            break;
        }
        for _ in 0..dropped {
            first.x.remove(0);
        }
        first.text = first.text.trim_start_matches(' ').to_string();
        if first.text.is_empty() {
            runs.remove(0);
        }
    }
}

/// Drop a run's trailing spaces together with their recorded x positions.
fn trim_trailing_runs(runs: &mut Vec<Run>) {
    while let Some(last) = runs.last_mut() {
        let dropped = last.text.chars().count() - last.text.trim_end_matches(' ').chars().count();
        if dropped == 0 {
            break;
        }
        for _ in 0..dropped {
            last.x.pop();
        }
        last.text = last.text.trim_end_matches(' ').to_string();
        if last.text.is_empty() {
            runs.pop();
        }
    }
}

/// Emit a multi-column region in reading order: down each column in turn.
fn emit_columns(
    bands: &[Band],
    start: usize,
    end: usize,
    count: usize,
    profile: &DocumentProfile,
    blocks: &mut Vec<Block>,
) {
    for column in 0..count {
        // Reduce each row to just this column, so the flow logic below sees an
        // ordinary single-column stream and needs no special cases.
        let column_bands: Vec<Band> = bands[start..end]
            .iter()
            .filter_map(|band| {
                let segment = band.segments.get(column)?;
                Some(Band {
                    segments: vec![segment.clone()],
                    bottom: segment.bottom,
                    top: segment.top,
                })
            })
            .collect();

        emit_flow(&column_bands, profile, blocks);
    }
}

/// Build a flow of prose: headings, lists and paragraphs, in order.
fn emit_flow(bands: &[Band], profile: &DocumentProfile, blocks: &mut Vec<Block>) {
    if bands.is_empty() {
        return;
    }

    let lines: Vec<Line> = bands.iter().map(Line::from_band).collect();

    let flow_left = lines.iter().map(|l| l.left).fold(f32::INFINITY, f32::min);
    let flow_right = lines
        .iter()
        .map(|l| l.right)
        .fold(f32::NEG_INFINITY, f32::max);

    let mut index = 0;
    while index < lines.len() {
        let line = &lines[index];

        if let Some(level) = heading_level(line, profile) {
            blocks.push(Block::Heading {
                level,
                content: line.inlines.clone(),
                style: None,
            });
            index += 1;
            continue;
        }

        if let Some(marker) = list_marker(&line.text) {
            let (block, next) = collect_list(&lines, index, &marker);
            blocks.push(block);
            index = next;
            continue;
        }

        let (end, content) = collect_paragraph(&lines, index, flow_left, flow_right, profile);
        blocks.push(Block::Paragraph {
            content,
            align: alignment(&lines[index], flow_left, flow_right),
            style: None,
        });
        index = end;
    }
}

fn heading_level(line: &Line, profile: &DocumentProfile) -> Option<u8> {
    if line.text.chars().count() > HEADING_MAX_CHARS {
        return None;
    }
    profile.level_for(line.size)
}

/// Join lines into one paragraph while the source's line breaks are soft.
fn collect_paragraph(
    lines: &[Line],
    start: usize,
    flow_left: f32,
    flow_right: f32,
    profile: &DocumentProfile,
) -> (usize, Vec<Inline>) {
    let anchor = &lines[start];
    let mut content = anchor.inlines.clone();
    let mut end = start + 1;

    while end < lines.len() {
        let previous = &lines[end - 1];
        let current = &lines[end];

        if !continues(previous, current, anchor, flow_left, flow_right, profile) {
            break;
        }

        // The break was soft, so it becomes a space. Hyphenation is left alone:
        // a trailing hyphen is ambiguous (real compounds such as `entre-` look
        // exactly like a split word), and joining wrongly corrupts two words
        // instead of one.
        content.push(Inline::text(" "));
        content.extend(current.inlines.clone());
        end += 1;
    }

    (end, content)
}

fn continues(
    previous: &Line,
    current: &Line,
    anchor: &Line,
    flow_left: f32,
    flow_right: f32,
    profile: &DocumentProfile,
) -> bool {
    if (current.size - anchor.size).abs() > 0.6 {
        return false;
    }
    if current.bold != anchor.bold || current.italic != anchor.italic {
        return false;
    }

    // Block-level lines always stand alone.
    if profile.level_for(current.size).is_some() {
        return false;
    }
    if list_marker(&current.text).is_some() {
        return false;
    }

    // A line pushed in from the left margin starts a paragraph. This is how a
    // PDF shows a new paragraph with no blank line between them.
    if current.left > anchor.left + anchor.size * INDENT_TOLERANCE_EM {
        return false;
    }

    if is_centred(current, flow_left, flow_right) {
        return false;
    }

    // The previous line must look full. If it stopped well short of the right
    // margin it ended a paragraph, whatever its type.
    if previous.right < flow_right - previous.size * FULL_LINE_TOLERANCE_EM {
        return false;
    }

    true
}

fn is_centred(line: &Line, flow_left: f32, flow_right: f32) -> bool {
    let width = flow_right - flow_left;
    if width <= 0.0 {
        return false;
    }
    let left_gap = line.left - flow_left;
    let right_gap = flow_right - line.right;
    let slack = line.size * CENTRED_SLACK_EM;
    left_gap > slack && right_gap > slack && (left_gap - right_gap).abs() <= slack
}

fn alignment(line: &Line, flow_left: f32, flow_right: f32) -> Align {
    if is_centred(line, flow_left, flow_right) {
        Align::Center
    } else {
        Align::Start
    }
}

/// A recognised list marker.
#[derive(Debug, Clone)]
struct Marker {
    ordered: bool,
    /// Characters the marker occupies, including the space after it.
    length: usize,
    /// Marker text as the writer should render the *first* item.
    text: String,
    /// The number read off an ordered marker, kept so that a list which the
    /// source starts at 5 does not silently become a list starting at 1.
    number: Option<u64>,
    numbering: Numbering,
}

fn list_marker(text: &str) -> Option<Marker> {
    let trimmed = text.trim_start();
    // Counted in characters, not bytes: the length is used to slice the line's
    // inlines, and a bullet is three bytes in UTF-8. Counting bytes ate two
    // extra characters, so `• Atomicité` came out as `omicité`.
    let skipped = text.chars().count() - trimmed.chars().count();
    let first = trimmed.chars().next()?;

    if BULLETS.contains(&first) {
        let after = &trimmed[first.len_utf8()..];
        if !after.starts_with(|c: char| c.is_whitespace()) {
            // A mid-sentence dash is punctuation, not a bullet.
            return None;
        }
        let rest = after.trim_start();
        if rest.is_empty() {
            return None;
        }
        return Some(Marker {
            ordered: false,
            length: skipped + 1 + (after.chars().count() - rest.chars().count()),
            text: first.to_string(),
            number: None,
            numbering: Numbering::Period,
        });
    }

    let (number, numbering, length) = ordered_marker(trimmed)?;

    Some(Marker {
        ordered: true,
        length: skipped + length,
        text: numbering.render(number),
        number: Some(number),
        numbering,
    })
}

/// Read an ordered marker off the front of a line.
///
/// Returns the number, how it was punctuated, and the characters consumed
/// including the space that follows.
fn ordered_marker(trimmed: &str) -> Option<(u64, Numbering, usize)> {
    let (opening, rest) = match trimmed.strip_prefix('(') {
        Some(rest) => (1usize, rest),
        None => (0usize, trimmed),
    };

    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() || digits.len() > 3 {
        return None;
    }

    let after = &rest[digits.len()..];
    let delimiter = after.chars().next()?;
    let numbering = match (opening, delimiter) {
        (1, ')') => Numbering::Parenthesised,
        (0, ')') => Numbering::Parenthesis,
        (0, '.') => Numbering::Period,
        _ => return None,
    };

    // Whitespace is required straight after the delimiter. That is what keeps
    // `2.1. Définition` out: it is a section number, not a list item, and the
    // `1` following the dot is preceded by a dot rather than a space.
    let tail = &after[delimiter.len_utf8()..];
    if !tail.starts_with(|c: char| c.is_whitespace()) {
        return None;
    }
    let body = tail.trim_start();
    if body.is_empty() {
        return None;
    }

    let consumed = opening + digits.len() + 1 + (tail.chars().count() - body.chars().count());
    Some((digits.parse().ok()?, numbering, consumed))
}

fn collect_list(lines: &[Line], start: usize, first: &Marker) -> (Block, usize) {
    let mut items = Vec::new();
    let mut index = start;

    while index < lines.len() {
        let line = &lines[index];
        let Some(marker) = list_marker(&line.text) else {
            break;
        };
        if marker.ordered != first.ordered {
            break;
        }

        items.push(ir::ListItem {
            level: 0,
            blocks: vec![Block::Paragraph {
                content: strip_prefix(&line.inlines, marker.length),
                align: Align::Start,
                style: None,
            }],
        });
        index += 1;
    }

    let block = Block::List(ir::ListBlock {
        ordered: first.ordered,
        // The source's own number, so a list the source interrupted and resumed
        // resumes at the right place instead of restarting at one.
        start: first.number,
        numbering: first.numbering,
        marker: (!first.ordered).then(|| first.text.clone()),
        items,
    });

    (block, index)
}

/// Remove the first `chars` characters of a line, used to drop a list marker.
fn strip_prefix(inlines: &[Inline], chars: usize) -> Vec<Inline> {
    let mut remaining = chars;
    let mut out: Vec<Inline> = Vec::new();

    for inline in inlines {
        if remaining == 0 {
            out.push(inline.clone());
            continue;
        }

        let Inline::Text(run) = inline else {
            out.push(inline.clone());
            continue;
        };

        let count = run.text.chars().count();
        if count <= remaining {
            remaining -= count;
            continue;
        }

        let kept: String = run.text.chars().skip(remaining).collect();
        remaining = 0;
        let kept = kept.trim_start().to_string();
        if !kept.is_empty() {
            out.push(Inline::Text(TextRun {
                text: kept,
                style: run.style.clone(),
            }));
        }
    }

    out
}

/// Build a table from a run of bands that share columns.
///
/// Rows are grouped by column membership rather than one band per row, because a
/// wrapped cell puts a row on two bands: `cohérent.` sits alone on the line below
/// `Cohérence`, filling one column where the rest of the row fills two. A band
/// whose cells are a strict subset of the previous band's can only be such a
/// tail — no table row begins part-way across — so it continues the cell above
/// it, and the wrapped break becomes a space, exactly as a soft line break does
/// in a paragraph.
///
/// The subset test fails when *every* cell of a row wraps: the continuation
/// fills the same columns as the line above, and no shape distinguishes it from
/// a second row. The producer's rulings carry that information, so a ruled table
/// is grouped by them instead — a rule between two bands ends the row, and its
/// absence means the bands belong together.
fn build_table(
    bands: &[Band],
    start: usize,
    end: usize,
    columns: &[f32],
    rules: &[Rule],
) -> Block {
    let tolerance = run_tolerance(&bands[start..end]);
    let internal = table_rules(bands, start, end, rules);
    let ruled = internal.len() >= MIN_INTERNAL_RULES;
    let mut grouped: Vec<Vec<Vec<Inline>>> = Vec::new();
    let mut previous: Vec<usize> = Vec::new();
    let mut upper: Option<&Band> = None;

    for band in &bands[start..end] {
        let matched = match_columns(&band.segments, columns, tolerance);
        let filled: Vec<usize> = matched.iter().flatten().copied().collect();
        let subset = filled.iter().all(|column| previous.contains(column));

        let wrapped = match upper {
            None => false,
            // With rulings, absence of a rule between two bands is the producer
            // saying they belong to one row. That is the only evidence that
            // separates a row whose every cell wraps from two rows: both lines
            // fill the same columns, so the subset test cannot.
            Some(upper) if ruled => {
                subset
                    && !internal
                        .iter()
                        .any(|rule| rule.y < upper.bottom && rule.y > band.top)
            }
            // A table with no internal rulings still has wrapped cells: a tail
            // band fills fewer columns than its row's first line.
            Some(_) => filled.len() < previous.len() && subset,
        };

        if !wrapped {
            grouped.push(vec![Vec::new(); columns.len()]);
        }
        previous = filled;
        upper = Some(band);

        let row = grouped.last_mut().expect("a row exists");
        for (index, segment) in band.segments.iter().enumerate() {
            // A segment on none of the columns is either litter or a cell
            // whose own column start is behind it — the wrapped `fantôme` of
            // `Lecture fantôme` (measured at x=155.79..199.81, page 2 of
            // `TP2 Admin BD 2026_2027.pdf`) hangs entirely between the term
            // column at 76.62 and the definition column at 206.67, and it is
            // the term cell's second line, not a stray.
            let column = matched[index].or_else(|| hang_column(segment, columns, tolerance));
            let Some(column) = column else { continue };
            let cell = &mut row[column];
            if !cell.is_empty() {
                cell.push(Inline::text(" "));
            }
            cell.extend(segment_inlines(segment));
        }
    }

    let mut rows: Vec<TableRow> = grouped
        .into_iter()
        .map(|cells| {
            TableRow::new(cells.into_iter().map(|content| TableCell {
                blocks: vec![Block::Paragraph {
                    content,
                    align: Align::Start,
                    style: None,
                }],
                ..Default::default()
            }))
        })
        .collect();

    // A first row that is entirely bold is a header. The rulings say where rows
    // end; they do not say which row is a header.
    if bands[start].segments.iter().all(|s| s.bold) {
        if let Some(first) = rows.first_mut() {
            for cell in &mut first.cells {
                cell.is_header = true;
            }
        }
    }

    Block::Table(TableBlock {
        caption: None,
        columns: columns.iter().map(|_| ColumnSpec::default()).collect(),
        rows,
        borders: Default::default(),
    })
}

/// The rules that can bound a row of this table: inside its vertical extent and
/// crossing at least [`RULE_COVERAGE`] of its text width.
///
/// A border above the first band or below the last is not inside and is
/// excluded — those are the table's own top and bottom edges, not row
/// boundaries. A rule that covers only a fraction of the width is one cell's
/// border in a table that does not rule every column, and is not trusted to say
/// where a row ends.
fn table_rules<'a>(bands: &[Band], start: usize, end: usize, rules: &'a [Rule]) -> Vec<&'a Rule> {
    let left = bands[start..end]
        .iter()
        .flat_map(|band| &band.segments)
        .map(|segment| segment.left)
        .fold(f32::INFINITY, f32::min);
    let right = bands[start..end]
        .iter()
        .flat_map(|band| &band.segments)
        .map(|segment| segment.right)
        .fold(f32::NEG_INFINITY, f32::max);
    let width = right - left;
    let top = bands[start].top;
    let bottom = bands[end - 1].bottom;

    rules
        .iter()
        .filter(|rule| rule.y < top && rule.y > bottom)
        .filter(|rule| {
            let overlap = rule.x1.min(right) - rule.x0.max(left);
            overlap >= width * RULE_COVERAGE
        })
        .collect()
}

/// The column a hanging continuation belongs to: entirely between the column
/// at its left and the next one (or beyond the last column, where nothing
/// bounds it). Returns `None` where no such column exists, which leaves the
/// segment dropped exactly as before.
fn hang_column(segment: &Segment, columns: &[f32], tolerance: f32) -> Option<usize> {
    let mut position: Option<usize> = None;
    for (index, column) in columns.iter().enumerate() {
        if segment.left > column + tolerance {
            position = Some(index);
        }
    }
    let position = position?;
    match columns.get(position + 1) {
        // Between two columns: the continuation must not reach into the next
        // one's start.
        Some(&next) => (segment.right < next).then_some(position),
        // Past the last column: the last cell's own continuation.
        None => Some(position),
    }
}

fn segment_inlines(segment: &Segment) -> Vec<Inline> {
    segment
        .runs
        .iter()
        .filter(|run| !run.text.is_empty())
        .map(run_inline)
        .collect()
}

fn run_inline(run: &Run) -> Inline {
    Inline::Text(TextRun {
        text: run.text.clone(),
        style: TextStyle {
            bold: run.bold,
            italic: run.italic,
            size: Some(Length::pt(run.size as f64)),
            font: run.font.clone(),
            vertical: run.vertical,
            ..Default::default()
        },
    })
}

/// A band's size, weighted by how much text carries each size.
fn weighted_size(segments: &[Segment]) -> f32 {
    let mut buckets: Vec<(i32, usize)> = Vec::new();

    for segment in segments {
        let key = (segment.size * 2.0).round() as i32;
        let weight = segment.char_count().max(1);
        match buckets.iter_mut().find(|(k, _)| *k == key) {
            Some((_, total)) => *total += weight,
            None => buckets.push((key, weight)),
        }
    }

    buckets
        .into_iter()
        .max_by_key(|(key, count)| (*count, *key))
        .map(|(key, _)| key as f32 / 2.0)
        .unwrap_or(0.0)
}

fn weighted_majority(segments: &[Segment], predicate: impl Fn(&Segment) -> bool) -> bool {
    let total: usize = segments.iter().map(|s| s.char_count().max(1)).sum();
    if total == 0 {
        return false;
    }
    let matching: usize = segments
        .iter()
        .filter(|s| predicate(s))
        .map(|s| s.char_count().max(1))
        .sum();
    matching * 2 >= total
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glyph::Glyph;
    use crate::layout::analyse_page;

    fn glyph(ch: char, size: f32, x: f32, right: f32, bottom: f32, bold: bool) -> Glyph {
        Glyph {
            ch,
            x,
            origin_x: x,
            right,
            bottom,
            top: bottom + size,
            size,
            bold,
            italic: false,
            font: None,
            generated: false,
        }
    }

    /// Lay a string out as one line at a fixed size.
    fn line(text: &str, size: f32, y: f32, origin: f32, bold: bool) -> Vec<Glyph> {
        let mut out = Vec::new();
        let mut x = origin;
        for ch in text.chars() {
            let width = size * 0.5;
            out.push(glyph(ch, size, x, x + width, y, bold));
            x += width;
            if ch == ' ' {
                x += size * 0.1;
            }
        }
        out
    }

    fn document_of(pages: Vec<Vec<Glyph>>) -> (Vec<Vec<Band>>, DocumentProfile) {
        let bands: Vec<Vec<Band>> = pages
            .into_iter()
            .map(|glyphs| analyse_page(&glyphs, 0.25))
            .collect();
        let profile = DocumentProfile::from_bands(&bands);
        (bands, profile)
    }

    fn page() -> ir::PageSetup {
        ir::PageSetup::default()
    }

    /// One styled run, with the per-character positions `Run` carries.
    fn run(text: &str, left: f32, size: f32, bold: bool) -> Run {
        Run {
            text: text.to_string(),
            x: (0..text.chars().count())
                .map(|index| left + index as f32 * size * 0.5)
                .collect(),
            bold,
            italic: false,
            size,
            font: None,
            vertical: Default::default(),
        }
    }

    /// A segment at an explicit page position. `build_table` reads its left
    /// edge, its extent and its text; the run's own x list only matters to the
    /// cut pass, which these tests bypass.
    fn segment(text: &str, left: f32, bottom: f32, size: f32, bold: bool) -> Segment {
        Segment {
            runs: vec![run(text, left, size, bold)],
            left,
            right: left + text.chars().count() as f32 * size * 0.5,
            bottom,
            top: bottom + size,
            size,
            bold,
            italic: false,
            gaps: Vec::new(),
        }
    }

    fn band(bottom: f32, top: f32, segments: Vec<Segment>) -> Band {
        Band {
            segments,
            bottom,
            top,
        }
    }

    fn rule(x0: f32, x1: f32, y: f32) -> Rule {
        Rule {
            x0,
            x1,
            y,
            thickness: 0.4,
        }
    }

    fn cell_text(cell: &TableCell) -> String {
        cell.blocks
            .iter()
            .filter_map(|block| match block {
                Block::Paragraph { content, .. } => Some(ir::inlines_to_plain_text(content)),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn body_size_ignores_a_single_oversized_glyph() {
        // 40 characters of 11pt body text plus one 14pt bullet.
        let mut glyphs = line("• Tester différents moyens de la mise en place", 11.0, 100.0, 0.0, false);
        // Make the bullet 14pt.
        glyphs[0].size = 14.0;
        glyphs[0].top = 114.0;

        let (_, profile) = document_of(vec![glyphs]);
        assert_eq!(profile.body_size, 11.0);
    }

    #[test]
    fn heading_sizes_are_ranked_and_become_levels() {
        let mut glyphs = line("Titre principal", 16.0, 100.0, 0.0, false);
        glyphs.extend(line("Définition", 14.0, 86.0, 0.0, false));
        glyphs.extend(line(
            "Le corps du texte est ici et il est long, bien plus long que les titres, \
             ce qui doit suffire a en faire la taille dominante du document entier.",
            11.0,
            72.0,
            0.0,
            false,
        ));

        let (bands, profile) = document_of(vec![glyphs]);
        assert_eq!(profile.body_size, 11.0);
        assert_eq!(profile.heading_sizes, vec![16.0, 14.0]);

        let blocks = build_page(&bands[0], &[], &profile, &page());
        assert!(matches!(blocks[0], Block::Heading { level: 1, .. }));
        assert!(matches!(blocks[1], Block::Heading { level: 2, .. }));
        assert!(matches!(blocks[2], Block::Paragraph { .. }));
    }

    #[test]
    fn soft_line_breaks_become_one_paragraph() {
        let mut glyphs = line("Le corps du texte commence ici", 11.0, 100.0, 0.0, false);
        glyphs.extend(line("et continue sur la ligne suivante.", 11.0, 88.0, 0.0, false));

        let (bands, profile) = document_of(vec![glyphs]);
        let blocks = build_page(&bands[0], &[], &profile, &page());

        assert_eq!(blocks.len(), 1, "{blocks:#?}");
        let Block::Paragraph { content, .. } = &blocks[0] else {
            panic!("expected a paragraph, got {blocks:#?}");
        };
        let text = ir::inlines_to_plain_text(content);
        assert!(text.contains("ici et continue"), "got {text:?}");
    }

    #[test]
    fn a_short_line_ends_the_paragraph() {
        // Real proportions: full lines reach the right margin, a paragraph's
        // last line stops well short of it. Equal-width full lines keep the test
        // about the rule rather than about the synthetic geometry.
        let full = "x".repeat(70);
        let short = "x".repeat(14);

        let mut glyphs = line(&full, 11.0, 100.0, 0.0, false);
        glyphs.extend(line(&full, 11.0, 88.0, 0.0, false));
        glyphs.extend(line(&short, 11.0, 76.0, 0.0, false));
        glyphs.extend(line(&full, 11.0, 64.0, 0.0, false));
        glyphs.extend(line(&short, 11.0, 52.0, 0.0, false));

        let (bands, profile) = document_of(vec![glyphs]);
        let blocks = build_page(&bands[0], &[], &profile, &page());

        // Two paragraphs, the break falling after each short line. The short
        // line belongs to the paragraph it ends, not to the next one.
        assert_eq!(blocks.len(), 2, "{blocks:#?}");

        let lengths: Vec<usize> = blocks
            .iter()
            .map(|block| match block {
                Block::Paragraph { content, .. } => {
                    ir::inlines_to_plain_text(content).chars().count()
                }
                other => panic!("expected paragraphs, got {other:?}"),
            })
            .collect();
        assert_eq!(lengths, vec![156, 85], "{blocks:#?}");
    }

    #[test]
    fn bullet_items_become_a_list_and_lose_their_marker() {
        let mut glyphs = line("• Atomicité", 11.0, 100.0, 0.0, false);
        glyphs.extend(line("• Cohérence", 11.0, 88.0, 0.0, false));
        glyphs.extend(line("• Durabilité", 11.0, 76.0, 0.0, false));

        let (bands, profile) = document_of(vec![glyphs]);
        let blocks = build_page(&bands[0], &[], &profile, &page());

        assert_eq!(blocks.len(), 1, "{blocks:#?}");
        let Block::List(list) = &blocks[0] else {
            panic!("expected a list, got {blocks:#?}");
        };
        assert!(!list.ordered);
        assert_eq!(list.items.len(), 3);

        let first = ir::inlines_to_plain_text(match &list.items[0].blocks[0] {
            Block::Paragraph { content, .. } => content,
            other => panic!("expected a paragraph, got {other:?}"),
        });
        assert_eq!(first, "Atomicité", "the marker must be stripped");
    }

    #[test]
    fn a_section_number_is_not_a_list_item() {
        assert!(list_marker("2.1. Définition").is_none());
        assert!(list_marker("2026/2027 Page 1").is_none());
        assert!(list_marker("1. Commencer").is_some());
        // A mid-word dash is punctuation.
        assert!(list_marker("tout ou rien - et réciproquement").is_none());
        assert!(list_marker("- Un vrai point").is_some());
    }

    #[test]
    fn aligned_rows_become_a_real_table() {
        let mut glyphs = Vec::new();
        // Three rows, two columns, left edges aligned per column.
        let rows = [
            ("Atomicité", "Obligatoire"),
            ("Cohérence", "Obligatoire"),
            ("Durabilité", "Obligatoire"),
        ];
        for (index, (left, right)) in rows.iter().enumerate() {
            let y = 100.0 - index as f32 * 14.0;
            glyphs.extend(line(left, 11.0, y, 40.0, false));
            glyphs.extend(line(right, 11.0, y, 260.0, false));
        }

        let (bands, profile) = document_of(vec![glyphs]);
        let blocks = build_page(&bands[0], &[], &profile, &page());

        assert_eq!(blocks.len(), 1, "{blocks:#?}");
        let Block::Table(table) = &blocks[0] else {
            panic!("expected a table, got {blocks:#?}");
        };
        assert_eq!(table.rows.len(), 3);
        assert_eq!(table.column_count(), 2);
    }

    #[test]
    fn a_full_height_two_column_region_is_not_a_table() {
        // Two columns running almost the whole page: this is a layout, not a
        // table, and treating it as one would turn every two-column page into a
        // single giant table.
        let mut glyphs = Vec::new();
        for row in 0..30 {
            let y = 780.0 - row as f32 * 24.0;
            glyphs.extend(line("colonne de gauche", 11.0, y, 40.0, false));
            glyphs.extend(line("colonne de droite", 11.0, y, 320.0, false));
        }

        let (bands, profile) = document_of(vec![glyphs]);
        let blocks = build_page(&bands[0], &[], &profile, &page());

        assert!(
            !blocks.iter().any(|b| matches!(b, Block::Table(_))),
            "a full-height two-column region must not become a table: {blocks:#?}"
        );

        // Reading order must be down the left column, then down the right.
        let text = blocks
            .iter()
            .filter_map(|b| match b {
                Block::Paragraph { content, .. } => Some(ir::inlines_to_plain_text(content)),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let left = text.find("colonne de gauche").expect("left column present");
        let right = text.find("colonne de droite").expect("right column present");
        assert!(left < right, "left column must come first:\n{text}");
    }

    #[test]
    fn a_wrapped_definition_merges_back_into_its_cell() {
        // The ACID glossary of `TP2 Admin BD 2026_2027.pdf` page 1, at its
        // measured positions. Every row but one is two segments; the row whose
        // definition wraps contributes a band that fills only the second
        // column. The old shape test stopped the table there and left four
        // stray paragraphs behind, so the tail band is the whole point.
        let rows: &[(f32, &str, &str, bool)] = &[
            (600.65, "Propriété", "Description", true),
            (580.85, "Atomicité", "La transaction s'exécute entièrement ou pas du tout.", false),
            (
                560.85,
                "Cohérence",
                "La transaction fait passer la base d'un état cohérent à un autre état",
                false,
            ),
            (527.42, "Isolation", "Les transactions concurrentes ne doivent pas s'influencer.", false),
        ];

        let mut glyphs = Vec::new();
        for (bottom, left, right, bold) in rows.iter().copied() {
            glyphs.extend(line(left, 11.0, bottom, 76.62, bold));
            glyphs.extend(line(right, 11.0, bottom, 186.65, bold));
        }
        // The tail of the second definition, on a band of its own.
        glyphs.extend(line("cohérent.", 11.0, 547.43, 186.65, false));

        let (bands, profile) = document_of(vec![glyphs]);
        let blocks = build_page(&bands[0], &[], &profile, &page());

        assert_eq!(blocks.len(), 1, "{blocks:#?}");
        let Block::Table(table) = &blocks[0] else {
            panic!("expected one table, got {blocks:#?}");
        };
        assert_eq!(table.rows.len(), 4, "{table:#?}");
        assert_eq!(table.column_count(), 2);

        let Block::Paragraph { content, .. } = &table.rows[2].cells[1].blocks[0] else {
            panic!("expected a cell paragraph");
        };
        assert_eq!(
            ir::inlines_to_plain_text(content),
            "La transaction fait passer la base d'un état cohérent à un autre état cohérent."
        );
    }

    #[test]
    fn a_list_line_beside_a_table_row_is_not_a_table() {
        // Page 3 of the same document, at its measured positions: the script
        // list on the left and the account table on the right share two
        // columns (60.62 and 521.77) and two consecutive bands, which is all
        // the old rule needed. Only the columns the rest of the run does not
        // repeat - the table's own first and second, each filled once - tell
        // the two apart.
        let mut glyphs = Vec::new();
        glyphs.extend(line("Supprime la table Compte si elle existe déjà", 11.0, 730.0, 60.62, false));
        glyphs.extend(line("Numcpte Titulaire", 11.0, 730.0, 372.72, true));
        glyphs.extend(line("Solde", 11.0, 730.0, 521.77, true));
        glyphs.extend(line("Crée la table Compte avec les colonnes numcpte", 11.0, 714.0, 60.62, false));
        glyphs.extend(line("Ali BEN SALAH", 11.0, 714.0, 429.33, false));
        glyphs.extend(line("1000", 11.0, 714.0, 521.77, false));
        glyphs.extend(line("(numérique sur 5 chiffres, clé primaire)", 11.0, 702.0, 78.62, false));

        let (bands, profile) = document_of(vec![glyphs]);
        let blocks = build_page(&bands[0], &[], &profile, &page());

        assert!(
            !blocks.iter().any(|b| matches!(b, Block::Table(_))),
            "two bands that share an x are not a table: {blocks:#?}"
        );
    }

    #[test]
    fn a_two_column_layout_is_not_a_table() {
        // Page 2 of the same document: a glossary laid out as a term column
        // and a definition column, whose texts touch wherever the left column
        // reaches the right one's start. The merged runs fill the first column
        // and look like wrapped rows; what separates them from a table is that
        // the second column's x drifts with the left column's length.
        let mut glyphs = Vec::new();
        let merged = [
            (741.67, "Lecture non répétable Une transaction relit une même ligne et obtient une valeur"),
            (728.28, "(Non-Repeatable Read) différente car une autre transaction l'a modifiée et validée entre"),
            (661.45, "Mise à jour perdue (Lost Deux transactions lisent puis écrivent la même donnée ; la"),
        ];
        for (bottom, text) in merged {
            glyphs.extend(line(text, 11.0, bottom, 76.62, false));
        }
        glyphs.extend(line("temps.", 11.0, 714.87, 206.67, false));
        glyphs.extend(line("Lecture", 11.0, 694.87, 76.62, true));
        glyphs.extend(line("fantôme Une transaction ré-exécute une requête avec condition (WHERE) et", 11.0, 694.87, 155.79, false));
        glyphs.extend(line("(Phantom Read)", 11.0, 681.47, 76.62, true));
        glyphs.extend(line("obtient un ensemble de lignes différent.", 11.0, 681.47, 206.67, false));
        glyphs.extend(line("Update)", 11.0, 648.05, 76.62, true));
        glyphs.extend(line("seconde écriture écrase la première sans en tenir compte.", 11.0, 648.05, 206.67, false));
        glyphs.extend(line("Rappel : la lecture sale n'est jamais possible sous Oracle.", 11.0, 620.45, 42.62, false));

        let (bands, profile) = document_of(vec![glyphs]);
        let blocks = build_page(&bands[0], &[], &profile, &page());

        assert!(
            !blocks.iter().any(|b| matches!(b, Block::Table(_))),
            "a two-column layout must not read as a table: {blocks:#?}"
        );
    }

    #[test]
    fn a_full_width_fused_row_is_cut_at_its_own_gap() {
        // Page 4's 5.2 block, measured: the header band carries Session A at
        // 61.83 and Session B at 310.70; beneath it the row
        // "[A1] Interrogez le solde du compte n°2 (dans une" runs to 300.54
        // and "[B1] Augmentez le solde du compte n°2 de 500" starts at
        // 310.70 — a 10.16pt carrier gap inside ONE fused segment whose
        // 471.64pt span crosses the full-width threshold (457.9). The
        // full-width refusal that used to sit before cut_segment threw the
        // row away and with it the window's strongest straddler; the cut has
        // to split it at the recorded gap instead. The helper's fixed glyph
        // widths render the same prose wider than the document's font, so
        // the left cell here ends at 342.23 and the right cell starts
        // 10.16pt later — the gap width, the full-width span (477.56 ≥
        // 457.9) and the cell origins are the measured ones, cut now
        // 347.31.
        let mut glyphs = Vec::new();
        let right_cell = 352.39; // left cell ends 342.23, carrier gap 10.16
        glyphs.extend(line("Session A", 11.0, 730.0, 61.83, true));
        glyphs.extend(line("Session B", 11.0, 730.0, right_cell, true));
        glyphs.extend(
            line("[A1] Interrogez le solde du compte n°2 (dans une", 11.0, 716.0, 62.83, false),
        );
        glyphs.extend(line("[B1] Augmentez le solde du compte", 11.0, 716.0, right_cell, false));
        glyphs.extend(line("transaction que vous laissez ouverte,", 11.0, 702.0, 62.83, false));
        glyphs.extend(line("DT.", 11.0, 702.0, right_cell, false));
        glyphs.extend(line("[B2] Validez la transaction.", 11.0, 688.0, right_cell, false));
        glyphs.extend(line("[A2] Réinterrogez le solde du compte n°2,", 11.0, 674.0, 62.83, false));
        glyphs.extend(line("même transaction qu'en [A1].", 11.0, 660.0, 62.83, false));

        let (bands, profile) = document_of(vec![glyphs]);
        let blocks = build_page(&bands[0], &[], &profile, &page());

        let table = blocks.iter().find_map(|block| match block {
            Block::Table(table) => Some(table),
            _ => None,
        });
        let Some(table) = table else {
            panic!("the Session block never read as a table: {blocks:#?}");
        };
        assert!(table.column_count() >= 2, "{blocks:#?}");
        // The fused row split: its two halves sit in different cells of the
        // same row rather than flowing together.
        let cells = table
            .rows
            .iter()
            .flat_map(|row| row.cells.iter())
            .map(|cell| {
                cell.blocks
                    .iter()
                    .map(|block| match block {
                        Block::Paragraph { content, .. } => content
                            .iter()
                            .map(|inline| match inline {
                                Inline::Text(text) => text.text.clone(),
                                _ => String::new(),
                            })
                            .collect::<String>(),
                        _ => String::new(),
                    })
                    .collect::<String>()
            })
            .collect::<Vec<String>>()
            .join("\n");
        assert!(
            cells.contains("[A1] Interrogez le solde du compte n°2 (dans une")
                && cells.contains("[B1] Augmentez le solde du compte"),
            "the fused row's halves must land in separate cells: {cells}"
        );
        assert!(
            cells.contains("[B2] Validez la transaction.") && cells.contains("[A2]"),
            "the one-sided continuation rows must feed the table: {cells}"
        );
    }

    #[test]
    fn two_tiered_session_blocks_yield_two_tables() {
        // Page 4 of `TP2 Admin BD 2026_2027.pdf` stacks the 5.1 and 5.2
        // Session blocks. Each fused row carries its own cut (5.1's inside a
        // 13.14pt carrier gap, measured cut 298.72; 5.2's inside a 10.16pt
        // one, measured cut 305.62) and the Question line between them
        // crosses both with word gaps of 1.1pt against a 6.05pt minimum,
        // which is the wall that closes each walk. Recording the wider 5.2
        // window first used to shadow the narrower 5.1 window: the scans
        // never came back — widest-wins lost the upper table. The scan now
        // adopts the earliest-opening window, so both fire, in page order.
        // Helper glyph widths squeeze the prose; cell origins, carrier-gap
        // widths and block pitches are the measured ones.
        let mut glyphs = Vec::new();
        // --- the 5.1 block: header, fused row at cut 315.70 (13.14pt gap),
        // then the left-only continuation rows that corroborate it.
        glyphs.extend(line("Session A", 11.0, 740.0, 77.03, true));
        glyphs.extend(line("Session B", 11.0, 740.0, 322.27, true));
        glyphs.extend(line("[A1] Modifiez (sans valider) le solde du", 11.0, 726.0, 77.03, false));
        glyphs.extend(line("[B1] Interrogez le solde du compte", 11.0, 726.0, 322.27, false));
        glyphs.extend(line("pour lui donner la valeur 9999.", 11.0, 712.0, 77.03, false));
        glyphs.extend(line("[A2] Annulez la transaction.", 11.0, 698.0, 77.03, false));
        // --- the wall: a full-width question line whose word gaps carry no
        // cut, refusing every candidate and closing both windows' walks.
        glyphs.extend(
            line("Quelle valeur voit la Session B en [B1] ? Est-ce 9999 ou 1000 ?", 11.0, 684.0, 60.42, false),
        );
        // --- the 5.2 block: header, fused row at cut 347.31 (10.16pt gap,
        // full-width span), wrap row, then the one-sided continuations.
        let right_cell = 352.39;
        glyphs.extend(line("Session A", 11.0, 670.0, 61.83, true));
        glyphs.extend(line("Session B", 11.0, 670.0, right_cell, true));
        glyphs.extend(
            line("[A1] Interrogez le solde du compte n°2 (dans une", 11.0, 656.0, 62.83, false),
        );
        glyphs.extend(line("[B1] Augmentez le solde du compte", 11.0, 656.0, right_cell, false));
        glyphs.extend(line("transaction que vous laissez ouverte,", 11.0, 642.0, 62.83, false));
        glyphs.extend(line("DT.", 11.0, 642.0, right_cell, false));
        glyphs.extend(line("[B2] Validez la transaction.", 11.0, 628.0, right_cell, false));
        glyphs.extend(line("[A2] Réinterrogez le solde du compte n°2,", 11.0, 614.0, 62.83, false));
        glyphs.extend(line("même transaction qu'en [A1].", 11.0, 600.0, 62.83, false));

        let (bands, profile) = document_of(vec![glyphs]);
        let blocks = build_page(&bands[0], &[], &profile, &page());

        let tables = blocks
            .iter()
            .filter(|block| matches!(block, Block::Table(_)))
            .count();
        assert_eq!(tables, 2, "both Session blocks must be tables: {blocks:#?}");
        let text = blocks
            .iter()
            .map(|block| match block {
                Block::Paragraph { content, .. } => content
                    .iter()
                    .map(|inline| match inline {
                        Inline::Text(text) => text.text.clone(),
                        _ => String::new(),
                    })
                    .collect::<String>(),
                _ => String::new(),
            })
            .collect::<String>();
        assert!(
            text.contains("Quelle valeur voit la Session B"),
            "the question line stays flow, not table: {blocks:#?}"
        );
        // The first table is the 5.1 block — the window whose start sits
        // nearest the scan — and it keeps its own split fused row.
        let first = blocks.iter().find_map(|block| match block {
            Block::Table(table) => Some(table),
            _ => None,
        });
        let cells = first
            .expect("a table was counted above")
            .rows
            .iter()
            .flat_map(|row| row.cells.iter())
            .map(|cell| {
                cell.blocks
                    .iter()
                    .map(|block| match block {
                        Block::Paragraph { content, .. } => content
                            .iter()
                            .map(|inline| match inline {
                                Inline::Text(text) => text.text.clone(),
                                _ => String::new(),
                            })
                            .collect::<String>(),
                        _ => String::new(),
                    })
                    .collect::<String>()
            })
            .collect::<Vec<String>>()
            .join("\n");
        assert!(
            cells.contains("[A1] Modifiez (sans valider)") && cells.contains("[B1] Interrogez"),
            "the upper block's fused row split into cells: {cells}"
        );
    }

    #[test]
    fn every_cell_wrapping_merges_when_no_rule_separates_the_lines() {
        // Page 2 of `TP2 Admin BD 2026_2027.pdf`, at the reader's own measured
        // band extents and the rulings `rules::extract` reports. All three rows
        // wrap both cells, so the subset test sees line 2 filling the same two
        // columns as line 1 and calls it a row. The internal rules sit at
        // 710.77 and 677.37; the top (757.57) and bottom (643.95) edges are
        // outside the text and must not count as row boundaries.
        let bands = vec![
            band(
                741.67,
                755.10,
                vec![
                    segment("Lecture non répétable", 76.62, 741.67, 11.0, false),
                    segment(
                        "Une transaction relit une même ligne et obtient une valeur",
                        206.67,
                        741.67,
                        11.0,
                        false,
                    ),
                ],
            ),
            band(
                728.28,
                741.71,
                vec![
                    segment("(Non-Repeatable Read)", 76.62, 728.28, 11.0, false),
                    segment(
                        "différente car une autre transaction l'a modifiée et validée entre",
                        206.67,
                        728.28,
                        11.0,
                        false,
                    ),
                ],
            ),
            band(
                714.87,
                728.30,
                vec![segment("temps.", 206.67, 714.87, 11.0, false)],
            ),
            band(
                694.87,
                708.30,
                vec![
                    segment("Lecture", 76.62, 694.87, 11.0, true),
                    segment("fantôme", 155.79, 694.87, 11.0, true),
                    segment(
                        "Une transaction ré-exécute une requête avec condition (WHERE) et",
                        206.67,
                        694.87,
                        11.0,
                        false,
                    ),
                ],
            ),
            band(
                681.47,
                694.90,
                vec![
                    segment("(Phantom Read)", 76.62, 681.47, 11.0, true),
                    segment(
                        "obtient un ensemble de lignes différent.",
                        206.67,
                        681.47,
                        11.0,
                        false,
                    ),
                ],
            ),
            band(
                661.45,
                674.88,
                vec![
                    segment("Mise à jour perdue (Lost", 76.62, 661.45, 11.0, false),
                    segment(
                        "Deux transactions lisent puis écrivent la même donnée ; la",
                        206.67,
                        661.45,
                        11.0,
                        false,
                    ),
                ],
            ),
            band(
                648.05,
                661.48,
                vec![
                    segment("Update)", 76.62, 648.05, 11.0, true),
                    segment(
                        "seconde écriture écrase la première sans en tenir compte.",
                        206.67,
                        648.05,
                        11.0,
                        false,
                    ),
                ],
            ),
        ];
        let rules = [
            rule(72.22, 523.38, 757.57),
            rule(72.22, 523.38, 710.77),
            rule(72.22, 523.38, 677.37),
            rule(72.22, 523.38, 643.95),
        ];

        let Block::Table(table) =
            build_table(&bands, 0, bands.len(), &[76.62, 206.67], &rules)
        else {
            panic!("expected a table");
        };

        assert_eq!(table.rows.len(), 3, "{table:#?}");
        assert_eq!(
            cell_text(&table.rows[0].cells[0]),
            "Lecture non répétable (Non-Repeatable Read)"
        );
        assert_eq!(
            cell_text(&table.rows[0].cells[1]),
            "Une transaction relit une même ligne et obtient une valeur différente car une autre transaction l'a modifiée et validée entre temps."
        );
        assert_eq!(
            cell_text(&table.rows[1].cells[0]),
            "Lecture fantôme (Phantom Read)"
        );
        assert_eq!(
            cell_text(&table.rows[2].cells[0]),
            "Mise à jour perdue (Lost Update)"
        );
    }

    #[test]
    fn a_ruled_one_sided_row_does_not_merge_into_the_row_above() {
        // Page 4's 5.2 block, measured: `[B2] Validez la transaction.` is a
        // right-only band below a row whose every cell wraps (`transaction que
        // vous laissez ouverte,` | `DT.`). Without rulings the subset test
        // merges it into the row above, which is how `[B2]` used to land in the
        // A1/B1 row. The rules at 502.01 and 476.01 say it is a row of its own.
        let bands = vec![
            band(
                507.93,
                520.87,
                vec![
                    segment(
                        "transaction que vous laissez ouverte,",
                        62.83,
                        507.93,
                        10.5,
                        false,
                    ),
                    segment("DT.", 310.70, 507.93, 10.5, false),
                ],
            ),
            band(
                482.82,
                496.25,
                vec![segment(
                    "[B2] Validez la transaction.",
                    310.70,
                    482.82,
                    10.5,
                    false,
                )],
            ),
            band(
                456.82,
                470.25,
                vec![segment(
                    "[A2] Réinterrogez le solde du compte n°2,",
                    62.83,
                    456.82,
                    10.5,
                    false,
                )],
            ),
        ];
        let rules = [rule(57.03, 538.58, 502.01), rule(57.03, 538.58, 476.01)];

        let Block::Table(table) =
            build_table(&bands, 0, bands.len(), &[62.83, 310.70], &rules)
        else {
            panic!("expected a table");
        };

        assert_eq!(table.rows.len(), 3, "{table:#?}");
        assert_eq!(
            cell_text(&table.rows[1].cells[1]),
            "[B2] Validez la transaction."
        );
        assert_eq!(
            cell_text(&table.rows[2].cells[0]),
            "[A2] Réinterrogez le solde du compte n°2,"
        );
    }

    #[test]
    fn a_header_rule_alone_does_not_license_merging() {
        // A printed table that rules only its header: one internal rule, no
        // rules between the body rows. Trusting absence there would merge every
        // body row into one, so one internal rule is never enough.
        let bands = vec![
            band(
                640.0,
                651.0,
                vec![
                    segment("Propriété", 76.62, 640.0, 11.0, true),
                    segment("Définition", 206.67, 640.0, 11.0, true),
                ],
            ),
            band(
                620.0,
                631.0,
                vec![
                    segment("Atomicité", 76.62, 620.0, 11.0, false),
                    segment("Tout ou rien", 206.67, 620.0, 11.0, false),
                ],
            ),
            band(
                600.0,
                611.0,
                vec![
                    segment("Cohérence", 76.62, 600.0, 11.0, false),
                    segment("État valide", 206.67, 600.0, 11.0, false),
                ],
            ),
            band(
                580.0,
                591.0,
                vec![
                    segment("Durabilité", 76.62, 580.0, 11.0, false),
                    segment("Effet persistant", 206.67, 580.0, 11.0, false),
                ],
            ),
        ];
        let rules = [rule(72.22, 523.38, 635.0)];

        let Block::Table(table) =
            build_table(&bands, 0, bands.len(), &[76.62, 206.67], &rules)
        else {
            panic!("expected a table");
        };

        assert_eq!(table.rows.len(), 4, "{table:#?}");
    }
}
