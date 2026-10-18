//! Geometry: glyphs grouped into bands, segments and styled runs.
//!
//! This is the stage that turns a bag of positioned characters into something
//! with a reading order. It deliberately knows nothing about paragraphs,
//! headings or tables — those are judgements about *meaning*, and they live in
//! [`crate::analyze`]. Keeping the two apart is what makes a table bug look like
//! a table bug rather than a grouping bug.
//!
//! Three rules here come directly from failures on real documents:
//!
//! * **Grouping is two-dimensional.** Banding by baseline alone fuses the two
//!   columns of a table into one line, interleaving their text. Within a band,
//!   a gap wide enough to be a gutter splits it into separate segments.
//!
//! * **The band tolerance comes from the tallest glyph in the group**, not from
//!   whichever glyph arrived first. Anchoring on the first glyph lets a large
//!   symbol spread the tolerance for everything that follows it.
//!
//! * **A line's size is the dominant size, not the maximum.** A bullet set
//!   larger than the text it introduces otherwise reclassifies the whole line as
//!   a heading, which was observed on a real document.

use dconv_ir::VerticalShift;
use std::cmp::Ordering;

use crate::glyph::Glyph;

/// Gap above which two glyphs on one baseline are in different columns rather
/// than different words.
///
/// A word space is roughly a quarter of an em; a column gutter is at least one.
/// 1.6 em sits between them with margin on both sides. Too low and a wide
/// justified word space splits a sentence into two "columns"; too high and table
/// cells merge.
const COLUMN_GAP_EM: f32 = 1.6;

/// Fraction of the measured space width that counts as a word gap.
///
/// Well above inter-letter noise (measured at up to 0.4pt against an 11pt font)
/// and well below a real space (2.4pt in the same document), which is exactly
/// the gap the original 0.25 em assumption failed to clear.
const WORD_GAP_FRACTION: f32 = 0.33;

/// Vertical tolerance for joining a glyph to the current band, as a fraction of
/// the tallest glyph already in it.
const BAND_TOLERANCE: f32 = 0.45;

/// A glyph smaller than this fraction of the band's dominant size, and sitting
/// off the baseline, is a super- or subscript rather than a line of its own.
///
/// Both bounds matter and neither is sufficient alone. The size ratio alone
/// cannot tell an 11pt line under a 14pt one (0.79) from a genuine superscript
/// (0.46–0.73), and the offset alone cannot tell a superscript from a tight line
/// pitch. Together they are decisive, because a script is the only thing that is
/// simultaneously much smaller *and* a fraction of a line height off the
/// baseline.
const SCRIPT_SIZE: f32 = 0.78;

/// How far a script may sit from the baseline, as a fraction of the *base*
/// text's size.
const SCRIPT_RAISE_EM: f32 = 0.6;

/// …and as a fraction of the *script's own* size.
///
/// The raise needs bounding both ways, and the second bound is the one that did
/// the real work. Against the base alone, an 11pt glyph sitting a whole line
/// below a 16pt heading is 0.68 em and looks like a plausible subscript; against
/// its own height it is 0.99, which is the next line rather than a script on
/// this one. That misclassification is not hypothetical: it pulled the `•` of a
/// real document's first bullet into the title above it, which then read
/// `Tra•nsactions` and left the bullet's own text looking like an ordinary
/// paragraph.
///
/// Against the script alone it is too strict in the other direction, which is
/// why both are kept: the real `3ème` in `3IRS_TP2.pdf` measures 5.69pt between
/// loose box bottoms against an 11.04pt base — 0.52 em of the base, and 0.82 of
/// the 6.96pt script.
///
/// That 5.69pt is a *bottom* to *bottom* distance, not a baseline to baseline
/// one, and the two are not interchangeable: the script sits 4.56pt above its
/// base in baseline terms, so 1.13pt of the offset is nothing but the deeper
/// loose box of the larger glyph. Both numbers are needed to reconstruct the
/// geometry, and reading this constant as a baseline raise puts a synthetic
/// script about 20% further from its base than the real one is — far enough to
/// orphan it, which is how the fixture in `scripts/make_fixtures.py` first came
/// out wrong.
///
/// Measuring bottoms rather than baselines also makes the bound tighten as the
/// script shrinks, because the descent term grows with the size difference while
/// the allowance shrinks with the script. At the measured proportions the offset
/// is 0.82 of the script against a 0.9 bound; hold the raise at the same
/// fraction of the base and shrink the script to half the base's size and the
/// same arithmetic gives 1.10, which splits. The crossover is near 59%, and real
/// superscripts routinely run 50-70%, so this is a live edge rather than a
/// curiosity. No document here sits on it, so the measure is left alone: the fix
/// would be to compare baselines, and both real documents would have to be
/// re-measured to re-tune the three constants against the new quantity.
const SCRIPT_RAISE_OWN: f32 = 0.9;

/// How many existing bands a glyph is offered to before it starts a new one.
/// Searching the nearest first is what lets a full-size glyph join the band a
/// superscript already opened.
const BAND_SEARCH: usize = 6;

/// A run of glyphs sharing one style.
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub size: f32,
    pub font: Option<String>,
    pub vertical: VerticalShift,
}

impl Run {
    fn same_style(&self, other: &Run) -> bool {
        self.bold == other.bold
            && self.italic == other.italic
            && self.vertical == other.vertical
            && self.font == other.font
            && (self.size - other.size).abs() < 0.05
    }
}

/// A horizontal run of text with no column-sized gap inside it.
///
/// One "line" may therefore produce several segments, and each segment is what a
/// table cell or a column entry looks like from here.
#[derive(Debug, Clone)]
pub struct Segment {
    pub runs: Vec<Run>,
    pub left: f32,
    pub right: f32,
    pub bottom: f32,
    pub top: f32,
    /// Dominant size weighted by glyph count. See the module comment.
    pub size: f32,
    pub bold: bool,
    pub italic: bool,
}

impl Segment {
    pub fn text(&self) -> String {
        self.runs.iter().map(|run| run.text.as_str()).collect()
    }

    /// Number of characters, used to weight the document-wide size histogram by
    /// how much text actually carries each size rather than by how many runs do.
    pub fn char_count(&self) -> usize {
        self.runs.iter().map(|run| run.text.chars().count()).sum()
    }
}

/// What PDFium considers one line: everything sharing a baseline.
#[derive(Debug, Clone)]
pub struct Band {
    pub segments: Vec<Segment>,
    /// Lowest and highest extent on the page, in PDF points from the bottom.
    pub bottom: f32,
    pub top: f32,
}

impl Band {
    /// The band's own text, with segments separated by a space.
    ///
    /// Used for classification (does this look like a heading, a list item) and
    /// for reporting, not for output: the segments are emitted separately so a
    /// table can still be reconstructed.
    pub fn text(&self) -> String {
        self.segments
            .iter()
            .map(Segment::text)
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Whether the band holds anything a reader would see.
    ///
    /// Word gaps are inferred from geometry, so a run of spaces with no letters
    /// among them carries no information at all. Producers emit them as page
    /// furniture and as leftovers from a wrapped cell, and a band left in
    /// becomes an empty paragraph — or, when the spaces happen to be set larger
    /// than the body text, an empty *heading*. That is how an empty `H3` turned
    /// up at the top of page 2 of a real document, from a single 12pt space.
    pub fn is_visible(&self) -> bool {
        self.segments.iter().any(|segment| {
            segment
                .runs
                .iter()
                .any(|run| run.text.chars().any(|c| !c.is_whitespace()))
        })
    }
}

/// Group a page's glyphs into bands and segments.
pub fn analyse_page(glyphs: &[Glyph], space_em: f32) -> Vec<Band> {
    if glyphs.is_empty() {
        return Vec::new();
    }

    // Sort once, by baseline then left edge. Not an optimisation: producers are
    // free to emit a line's right half before its left half, and a band is
    // defined by physical position, not by document order.
    let mut ordered: Vec<&Glyph> = glyphs.iter().collect();
    ordered.sort_by(|a, b| {
        b.bottom
            .partial_cmp(&a.bottom)
            .unwrap_or(Ordering::Equal)
            .then(a.x.partial_cmp(&b.x).unwrap_or(Ordering::Equal))
    });

    let mut bands: Vec<PendingBand> = Vec::new();

    for glyph in ordered {
        let mut attached = false;

        // Newest first. Sorting by baseline puts the highest glyph first, which
        // for a superscript means the script is seen *before* the baseline it
        // belongs to — so the band it belongs to already exists, and this is the
        // step that lets its base glyph find it. Without it, `2ème` splits in
        // two: the raised glyph opens a band, and the `2` can never join it.
        for band in bands.iter_mut().rev().take(BAND_SEARCH) {
            if band.accepts(glyph) {
                // A glyph taller than everything in the band is the base text
                // rather than a script, so the band re-anchors to its baseline.
                // Without this, a band opened by a superscript keeps the raised
                // baseline and then rejects the very text it was opened for.
                if glyph.size > band.tallest {
                    band.anchor = glyph.bottom;
                    band.tallest = glyph.size;
                }
                band.glyphs.push(glyph);
                attached = true;
                break;
            }
        }

        if !attached {
            bands.push(PendingBand {
                glyphs: vec![glyph],
                anchor: glyph.bottom,
                tallest: glyph.size,
            });
        }
    }

    // Whitespace-only bands go here rather than in the classifier, because they
    // are not structure that the classifier should have an opinion about: they
    // are producer noise, and every later stage would otherwise have to keep
    // re-deciding what they mean.
    bands
        .iter()
        .map(|band| build_band(&band.glyphs, space_em))
        .filter(Band::is_visible)
        .collect()
}

/// A band being accumulated, before its segments can be built.
struct PendingBand<'a> {
    glyphs: Vec<&'a Glyph>,
    /// The baseline members are measured against: the bottom of whichever glyph
    /// opened the band.
    anchor: f32,
    /// The tallest glyph so far. The tolerance scales with it, so a large symbol
    /// widens the band for what follows it rather than being measured against
    /// whatever happened to arrive first.
    tallest: f32,
}

impl PendingBand<'_> {
    fn accepts(&self, glyph: &Glyph) -> bool {
        let smaller = self.tallest.min(glyph.size);
        let larger = self.tallest.max(glyph.size);
        let offset = (glyph.bottom - self.anchor).abs();

        // Same baseline, allowing for the rounding in the producer's positions.
        if offset <= larger * BAND_TOLERANCE {
            return true;
        }

        // One of the two is a script. A size mismatch is symmetric — it says
        // nothing about which glyph arrived first — so the test has to be too.
        // An earlier version asked only whether the *arriving* glyph was small,
        // which silently failed on `3ème`: the superscript is emitted first, so
        // by the time the full-size `3` arrives the band is the small one.
        smaller < larger * SCRIPT_SIZE
            && offset <= larger * SCRIPT_RAISE_EM
            && offset <= smaller * SCRIPT_RAISE_OWN
    }
}

fn build_band(glyphs: &[&Glyph], space_em: f32) -> Band {
    let mut sorted = glyphs.to_vec();
    sorted.sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap_or(Ordering::Equal));

    let size = dominant_size(&sorted, provisional_size(&sorted));
    let column_gap = size * COLUMN_GAP_EM;

    let mut segments = Vec::new();
    let mut current: Vec<&Glyph> = Vec::new();
    let mut previous_right: Option<f32> = None;

    for glyph in sorted {
        if let Some(previous) = previous_right {
            if glyph.x - previous > column_gap {
                segments.push(build_segment(&current, space_em));
                current.clear();
            }
        }
        current.push(glyph);
        previous_right = Some(glyph.right);
    }

    if !current.is_empty() {
        segments.push(build_segment(&current, space_em));
    }

    Band {
        bottom: segments
            .iter()
            .map(|s| s.bottom)
            .fold(f32::INFINITY, f32::min),
        top: segments
            .iter()
            .map(|s| s.top)
            .fold(f32::NEG_INFINITY, f32::max),
        segments,
    }
}

fn build_segment(glyphs: &[&Glyph], space_em: f32) -> Segment {
    // The provisional reference is the largest glyph on the line, and it is used
    // only to decide which glyphs are small enough to be scripts. Everything
    // that reports a size then ignores those glyphs, so a run of superscripts
    // cannot outvote the text it annotates: three of the four glyphs in `2ème`
    // are the 7pt script, and counting all four would report the line as 7pt.
    let provisional = provisional_size(glyphs);
    let size = dominant_size(glyphs, provisional);
    let word_gap = size * space_em * WORD_GAP_FRACTION;
    let baseline = dominant_baseline(glyphs, provisional);

    let mut runs: Vec<Run> = Vec::new();
    let mut previous_right: Option<f32> = None;

    for glyph in glyphs {
        if let Some(previous) = previous_right {
            let gap = glyph.x - previous;
            let already_spaced = runs
                .last()
                .map(|run| run.text.ends_with(' '))
                .unwrap_or(false);

            if gap > word_gap && !already_spaced {
                push_run(&mut runs, " ", style_of(glyph, baseline, size));
            }
        }

        push_run(&mut runs, &glyph.ch.to_string(), style_of(glyph, baseline, size));
        previous_right = Some(glyph.right);
    }

    // A trailing space is always an artefact of the gap rule firing before a
    // segment boundary, never real content.
    if let Some(last) = runs.last_mut() {
        while last.text.ends_with(' ') {
            last.text.pop();
        }
    }
    runs.retain(|run| !run.text.is_empty());

    Segment {
        left: glyphs.first().map(|g| g.x).unwrap_or(0.0),
        right: glyphs.last().map(|g| g.right).unwrap_or(0.0),
        bottom: glyphs.iter().map(|g| g.bottom).fold(f32::INFINITY, f32::min),
        top: glyphs
            .iter()
            .map(|g| g.top)
            .fold(f32::NEG_INFINITY, f32::max),
        size,
        bold: majority(glyphs, |g| g.bold),
        italic: majority(glyphs, |g| g.italic),
        runs,
    }
}

fn style_of(glyph: &Glyph, baseline: f32, segment_size: f32) -> Run {
    Run {
        text: String::new(),
        bold: glyph.bold,
        italic: glyph.italic,
        size: glyph.size,
        font: glyph.font.clone(),
        vertical: vertical_shift(glyph, baseline, segment_size),
    }
}

/// Classify an off-baseline smaller glyph as super- or subscript.
///
/// Without this, a superscript lands on its own line: on a real document `2ème`
/// split in two, leaving a line holding only `ème` at 7pt, 9.2pt above an 11pt
/// baseline — well beyond any grouping tolerance worth having.
fn vertical_shift(glyph: &Glyph, baseline: f32, segment_size: f32) -> VerticalShift {
    if segment_size <= 0.0 || glyph.size >= segment_size * SCRIPT_SIZE {
        return VerticalShift::None;
    }

    let offset = glyph.bottom - baseline;
    if offset > segment_size * 0.18 {
        VerticalShift::Superscript
    } else if offset < segment_size * -0.22 {
        VerticalShift::Subscript
    } else {
        VerticalShift::None
    }
}

fn push_run(runs: &mut Vec<Run>, text: &str, style: Run) {
    if let Some(last) = runs.last_mut() {
        if last.same_style(&style) {
            last.text.push_str(text);
            return;
        }
    }

    let mut run = style;
    run.text = text.to_string();
    runs.push(run);
}

/// The largest glyph on a line: a reference for deciding what counts as a
/// script, never the line's reported size.
fn provisional_size(glyphs: &[&Glyph]) -> f32 {
    glyphs.iter().map(|g| g.size).fold(0.0f32, f32::max)
}

/// The most common size among the glyphs that are not scripts, counted by glyph
/// rather than by span, so a single oversized symbol cannot outvote the text it
/// sits beside — while a superscript run cannot outvote it either.
///
/// Ties break towards the larger size: with no majority there is no bulk to
/// follow, and the larger of two equal-size groups is the one more likely to be
/// the text rather than a stray mark.
fn dominant_size(glyphs: &[&Glyph], provisional: f32) -> f32 {
    let cut = provisional * SCRIPT_SIZE;
    let mut buckets: Vec<(i32, usize)> = Vec::new();

    for glyph in glyphs.iter().filter(|g| g.size >= cut) {
        let key = (glyph.size * 2.0).round() as i32;
        match buckets.iter_mut().find(|(k, _)| *k == key) {
            Some((_, count)) => *count += 1,
            None => buckets.push((key, 1)),
        }
    }

    buckets
        .into_iter()
        .max_by_key(|(key, count)| (*count, *key))
        .map(|(key, _)| key as f32 / 2.0)
        .unwrap_or(provisional)
}

/// The most common baseline among the glyphs that are not scripts.
///
/// Scripts are excluded for the same reason: `2ème` has three glyphs on the
/// raised baseline and one on the real one, and the raised one is not the line.
fn dominant_baseline(glyphs: &[&Glyph], provisional: f32) -> f32 {
    let cut = provisional * SCRIPT_SIZE;
    let mut buckets: Vec<(i32, usize)> = Vec::new();

    for glyph in glyphs.iter().filter(|g| g.size >= cut) {
        let key = (glyph.bottom * 4.0).round() as i32;
        match buckets.iter_mut().find(|(k, _)| *k == key) {
            Some((_, count)) => *count += 1,
            None => buckets.push((key, 1)),
        }
    }

    buckets
        .into_iter()
        .max_by_key(|(key, count)| (*count, *key))
        .map(|(key, _)| key as f32 / 4.0)
        .unwrap_or_else(|| glyphs.first().map(|g| g.bottom).unwrap_or(0.0))
}

fn majority(glyphs: &[&Glyph], predicate: impl Fn(&Glyph) -> bool) -> bool {
    if glyphs.is_empty() {
        return false;
    }
    let count = glyphs.iter().filter(|g| predicate(g)).count();
    count * 2 >= glyphs.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glyph(ch: char, size: f32, x: f32, right: f32, bottom: f32) -> Glyph {
        Glyph {
            ch,
            x,
            origin_x: x,
            right,
            bottom,
            top: bottom + size,
            size,
            bold: false,
            italic: false,
            font: None,
            generated: false,
        }
    }

    /// Four words in a row, laid out with real advances and one space-width gap.
    fn sentence(y: f32) -> Vec<Glyph> {
        let mut out = Vec::new();
        let mut x = 0.0f32;
        for word in ["Page", "1", "sur", "6"] {
            for ch in word.chars() {
                out.push(glyph(ch, 10.0, x, x + 4.6, y));
                x += 4.6;
            }
            // A space at 10pt in a font whose space is 0.25em, then the gap the
            // producer leaves by positioning the next word.
            x += 2.5;
        }
        out
    }

    #[test]
    fn recovers_word_spaces_the_producer_omitted() {
        let bands = analyse_page(&sentence(100.0), 0.25);
        assert_eq!(bands.len(), 1);
        assert_eq!(bands[0].text(), "Page 1 sur 6");
    }

    #[test]
    fn a_band_of_nothing_but_spaces_is_dropped() {
        // A real document puts a single 12pt space at the top of a page, larger
        // than the 11pt body. Kept, it is a paragraph; sized above the body, it
        // is an empty *heading*.
        let mut glyphs = sentence(100.0);
        glyphs.push(glyph(' ', 12.0, 0.0, 6.3, 40.0));
        glyphs.push(glyph(' ', 12.0, 6.3, 12.6, 40.0));

        let bands = analyse_page(&glyphs, 0.25);
        assert_eq!(bands.len(), 1, "{bands:#?}");
        assert_eq!(bands[0].text(), "Page 1 sur 6");
    }

    #[test]
    fn a_space_between_words_does_not_hide_the_text() {
        // The filter must key on the whole band, not on individual glyphs: a
        // real word gap is a space glyph like any other, and a rule that dropped
        // any band containing one would drop every line of prose.
        let bands = analyse_page(&sentence(100.0), 0.25);
        assert_eq!(bands.len(), 1);
        assert!(bands[0].is_visible());
    }

    #[test]
    fn a_narrower_space_still_splits_words() {
        // The real-document case: 0.203 em rather than 0.25, which the old fixed
        // 0.25 em threshold measured as below the bar and deleted.
        let bands = analyse_page(&sentence(100.0), 0.203);
        assert_eq!(bands[0].text(), "Page 1 sur 6");
    }

    #[test]
    fn a_wide_gap_splits_a_band_into_segments() {
        // A table row: two cells whose gutter is far wider than a word space.
        let mut glyphs = Vec::new();
        let mut x = 0.0f32;
        for ch in "Atomicité".chars() {
            glyphs.push(glyph(ch, 10.0, x, x + 5.0, 100.0));
            x += 5.0;
        }
        x += 30.0;
        for ch in "Obligatoire".chars() {
            glyphs.push(glyph(ch, 10.0, x, x + 5.0, 100.0));
            x += 5.0;
        }

        let bands = analyse_page(&glyphs, 0.25);
        assert_eq!(bands.len(), 1);
        assert_eq!(bands[0].segments.len(), 2, "{:?}", bands[0]);
        assert_eq!(bands[0].segments[0].text(), "Atomicité");
        assert_eq!(bands[0].segments[1].text(), "Obligatoire");
    }

    #[test]
    fn a_superscript_joins_its_baseline_instead_of_orphaning() {
        // `2` at 11pt followed by a raised `ème` at 7pt, 4pt above the baseline.
        let mut glyphs = vec![glyph('2', 11.0, 0.0, 6.0, 100.0)];
        glyphs.push(glyph('è', 7.0, 6.5, 10.0, 104.0));
        glyphs.push(glyph('m', 7.0, 10.0, 15.0, 104.0));
        glyphs.push(glyph('e', 7.0, 15.0, 19.0, 104.0));

        let bands = analyse_page(&glyphs, 0.25);
        assert_eq!(bands.len(), 1, "the superscript must not orphan");
        assert_eq!(bands[0].text(), "2ème");

        let runs = &bands[0].segments[0].runs;
        assert!(
            runs.iter().any(|r| r.vertical == VerticalShift::Superscript),
            "the raised glyphs should be marked superscript: {runs:?}"
        );
    }

    #[test]
    fn the_real_threescript_geometry_survives_the_constants() {
        // `Groupes : 3ème` on page 1 of `3IRS_TP2.pdf`, which is the case the
        // script constants were calibrated on. The sizes and the bottoms are the
        // measured values (base 11.04pt at bottom 680.46, script 6.96pt at
        // bottom 686.15); the x positions are laid out contiguously because the
        // failure being guarded is vertical and that is the only part of the
        // geometry worth reproducing exactly.
        //
        // The 5.69pt between those bottoms is 0.52 em of the base and 0.82 of
        // the script, against bounds of 0.6 and 0.9. The margin on the second is
        // 0.08, and it is the difference between `ème` annotating its digit and
        // `ème` becoming a paragraph of its own three bands too high — which is
        // silent, because the line it was taken from still reads correctly minus
        // three characters. `a_superscript_joins_its_baseline_instead_of_orphaning`
        // covers the same rule at a comfortable 4pt gap and would not notice the
        // constants moving; this one would.
        let mut glyphs = Vec::new();
        let mut x = 0.0f32;
        for ch in "Groupes : 3".chars() {
            glyphs.push(glyph(ch, 11.04, x, x + 5.5, 680.46));
            x += 5.5;
        }
        for ch in "ème".chars() {
            glyphs.push(glyph(ch, 6.96, x, x + 4.0, 686.15));
            x += 4.0;
        }

        let bands = analyse_page(&glyphs, 0.25);
        assert_eq!(bands.len(), 1, "the real 3ème must not orphan: {bands:#?}");
        assert_eq!(bands[0].text(), "Groupes : 3ème");
    }

    #[test]
    fn a_smaller_line_below_a_heading_is_not_a_subscript() {
        // The real geometry behind a defects: an 11pt bullet 10.94pt below a
        // 16pt title. Bounding the raise by the glyph's own size alone admitted
        // it, and the bullet was pulled into the title, which then read
        // `Tra•nsactions` — and, because the bullet's glyph had gone, its own
        // line stopped looking like a list item at all.
        let mut glyphs = Vec::new();
        let mut x = 0.0f32;
        for ch in "Titre".chars() {
            glyphs.push(glyph(ch, 16.0, x, x + 8.0, 738.58));
            x += 8.0;
        }
        let mut x = 0.0f32;
        for ch in "Gestion".chars() {
            glyphs.push(glyph(ch, 11.0, x, x + 5.5, 727.64));
            x += 5.5;
        }

        let bands = analyse_page(&glyphs, 0.25);
        assert_eq!(bands.len(), 2, "{bands:#?}");
        assert_eq!(bands[0].text(), "Titre");
        assert_eq!(bands[1].text(), "Gestion");
    }

    #[test]
    fn line_size_follows_the_text_not_the_largest_glyph() {
        // A bullet set at 14pt introducing 12pt text. Taking the maximum would
        // report the line as 14pt and reclassify it as a heading.
        let mut glyphs = vec![glyph('•', 14.0, 0.0, 7.0, 100.0)];
        let mut x = 9.0f32;
        for ch in "Tester différents moyens".chars() {
            glyphs.push(glyph(ch, 12.0, x, x + 5.0, 100.0));
            x += 5.0;
        }

        let bands = analyse_page(&glyphs, 0.25);
        assert_eq!(bands[0].segments[0].size, 12.0);
    }

    #[test]
    fn separate_lines_end_up_in_separate_bands() {
        let mut glyphs = sentence(100.0);
        glyphs.extend(sentence(88.0));
        let bands = analyse_page(&glyphs, 0.25);
        assert_eq!(bands.len(), 2);
    }

    #[test]
    fn styles_split_runs_without_changing_the_text() {
        let mut glyphs = vec![glyph('a', 10.0, 0.0, 5.0, 100.0)];
        let mut bold = glyph('b', 10.0, 5.0, 10.0, 100.0);
        bold.bold = true;
        glyphs.push(bold);
        glyphs.push(glyph('c', 10.0, 10.0, 15.0, 100.0));

        let bands = analyse_page(&glyphs, 0.25);
        let runs = &bands[0].segments[0].runs;
        assert_eq!(bands[0].segments[0].text(), "abc");
        assert_eq!(runs.len(), 3);
        assert!(runs[1].bold);
        assert!(!runs[0].bold && !runs[2].bold);
    }
}
