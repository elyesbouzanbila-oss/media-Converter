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

/// A longer line is never a heading, however large its type. Guards against a
/// stray oversized glyph headlining an entire paragraph.
const HEADING_MAX_CHARS: usize = 90;

/// How far a segment's left edge may drift and still count as the same table
/// column, as a fraction of the row's largest glyph.
const COLUMN_TOLERANCE_EM: f32 = 0.9;

/// Fewer aligned rows than this is not a table. Two lines that happen to line up
/// are a coincidence of layout, not structure.
const MIN_TABLE_ROWS: usize = 2;

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
pub fn build_page(bands: &[Band], profile: &DocumentProfile, page: &ir::PageSetup) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut index = 0;

    while index < bands.len() {
        match classify_region(bands, index, page) {
            Region::Table { end, columns } => {
                blocks.push(build_table(bands, index, end, &columns));
                index = end;
                continue;
            }
            Region::Columns { end, count } => {
                emit_columns(bands, index, end, count, profile, &mut blocks);
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
        emit_flow(&bands[start..end], profile, &mut blocks);
        index = end;
    }

    blocks
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
fn classify_region(bands: &[Band], start: usize, page: &ir::PageSetup) -> Region {
    let first = &bands[start].segments;
    if first.len() < 2 {
        return Region::Flow;
    }

    let columns: Vec<f32> = first.iter().map(|segment| segment.left).collect();
    let largest = first.iter().map(|s| s.size).fold(0.0f32, f32::max);
    let tolerance = largest * COLUMN_TOLERANCE_EM;

    let mut end = start;
    while end < bands.len() {
        let segments = &bands[end].segments;
        if segments.len() != columns.len() {
            break;
        }
        let aligned = segments
            .iter()
            .zip(&columns)
            .all(|(segment, column)| (segment.left - column).abs() <= tolerance);
        if !aligned {
            break;
        }
        end += 1;
    }

    if end - start < MIN_TABLE_ROWS {
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

fn build_table(bands: &[Band], start: usize, end: usize, columns: &[f32]) -> Block {
    let mut rows: Vec<TableRow> = Vec::new();

    for band in &bands[start..end] {
        let cells: Vec<TableCell> = band
            .segments
            .iter()
            .map(|segment| TableCell {
                blocks: vec![Block::Paragraph {
                    content: segment_inlines(segment),
                    align: Align::Start,
                    style: None,
                }],
                ..Default::default()
            })
            .collect();
        rows.push(TableRow::new(cells));
    }

    // A first row that is entirely bold is a header. Everything else has to be
    // inferred from ruling lines, which are not read yet.
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

        let blocks = build_page(&bands[0], &profile, &page());
        assert!(matches!(blocks[0], Block::Heading { level: 1, .. }));
        assert!(matches!(blocks[1], Block::Heading { level: 2, .. }));
        assert!(matches!(blocks[2], Block::Paragraph { .. }));
    }

    #[test]
    fn soft_line_breaks_become_one_paragraph() {
        let mut glyphs = line("Le corps du texte commence ici", 11.0, 100.0, 0.0, false);
        glyphs.extend(line("et continue sur la ligne suivante.", 11.0, 88.0, 0.0, false));

        let (bands, profile) = document_of(vec![glyphs]);
        let blocks = build_page(&bands[0], &profile, &page());

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
        let blocks = build_page(&bands[0], &profile, &page());

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
        let blocks = build_page(&bands[0], &profile, &page());

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
        let blocks = build_page(&bands[0], &profile, &page());

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
        let blocks = build_page(&bands[0], &profile, &page());

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
}
