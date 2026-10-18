//! Positioned glyph extraction.
//!
//! Everything downstream works on [`Glyph`], never on PDFium types, so that the
//! layout analyser is testable without a PDF engine and a bug in it cannot be
//! confused with a bug in the extraction.
//!
//! Three decisions here are load-bearing and were each established by a concrete
//! failure on a real document, not by reasoning:
//!
//! 1. **`x` is the advance box, not the ink box.** Word-boundary inference
//!    compares adjacent glyph edges, and ink boxes are too shape-dependent: the
//!    ink box of `.` or `i` starts well inside its advance box, so an ink-based
//!    comparison invents spaces before punctuation and around narrow letters.
//!
//! 2. **PDFium's synthesised separators are dropped.** It emits CR, LF and space
//!    characters carrying a nominal 1pt size and a zero-height box pinned to the
//!    baseline. They have no usable geometry, and leaving them in corrupts both
//!    line grouping and word-gap measurement.
//!
//! 3. **Control code points are dropped.** Some font encodings map a glyph to a
//!    control code — a hyphen arriving as U+0002 was observed on a real
//!    document. The character it should have been is not recoverable from the
//!    page description, so emitting the raw code point would poison the text,
//!    the XML and the DOCX, which is strictly worse than losing one hyphen.

use pdfium_render::prelude::*;
use std::collections::BTreeMap;

/// One positioned character, normalised into what the layout analyser consumes.
#[derive(Debug, Clone)]
pub struct Glyph {
    pub ch: char,

    /// Left edge of the glyph's *advance* box, in points from the page's left
    /// edge. See the module comment for why this and not the ink box.
    pub x: f32,

    /// Pen position, straight from `FPDFText_GetCharOrigin`.
    ///
    /// Distinct from `x`, which is the left edge of the advance box. The two are
    /// usually equal, but the *difference* between consecutive origins is the
    /// advance actually consumed, which is the ground truth for whether the
    /// producer elided a space. Kept for that diagnosis even though the current
    /// gap rule reads from the advance boxes.
    pub origin_x: f32,

    /// Right edge of the advance box.
    pub right: f32,

    /// Baseline and cap height of the advance box, in PDF points from the page's
    /// bottom edge.
    pub bottom: f32,
    pub top: f32,

    /// Effective rendered size: the font size multiplied by any vertical scale
    /// on the character's transformation matrix. This, not the nominal `Tf`
    /// size, is what the reader saw.
    pub size: f32,

    pub bold: bool,
    pub italic: bool,
    pub font: Option<String>,

    /// True when PDFium inserted this character rather than it being present in
    /// the content stream.
    pub generated: bool,
}

/// The result of reading one page's characters.
#[derive(Debug, Default)]
pub struct Extraction {
    pub glyphs: Vec<Glyph>,
    /// Synthesised separators discarded for having no geometry.
    pub synthetic_dropped: usize,
    /// Code points discarded for being unrepresentable, by frequency.
    ///
    /// Kept as a histogram rather than a count because *which* code point was
    /// dropped decides what to do about it: one stray U+0002 is noise, whereas a
    /// whole family mapping to the same value is a systematic loss with a fix.
    /// A bare count cannot tell those apart, and the difference is exactly what
    /// was needed to diagnose the missing hyphens on `3IRS_TP2.pdf`.
    pub dropped_codepoints: BTreeMap<u32, usize>,
}

impl Extraction {
    /// Total code points discarded.
    pub fn control_dropped(&self) -> usize {
        self.dropped_codepoints.values().sum()
    }
}

/// Read every character on a page.
pub fn extract_page(page: &PdfPage) -> Result<Extraction, PdfiumError> {
    let text = page.text()?;
    let mut out = Extraction::default();

    for character in text.chars().iter() {
        let Some(ch) = character.unicode_char() else {
            // Unpaired code unit. PDFium produces these for some malformed
            // embedded subsets.
            continue;
        };

        // Non-characters, and the whole C0/C1 range. Tab and newline are dropped
        // along with the rest: a real line break in a PDF is a positioning
        // operation, never a character, so a literal one is always an artefact.
        if ch.is_control() || ch == '\u{FFFE}' || ch == '\u{FFFF}' {
            *out.dropped_codepoints.entry(ch as u32).or_default() += 1;
            continue;
        }

        let Ok(loose) = character.loose_bounds() else {
            continue;
        };

        let generated = character.is_generated().unwrap_or(false);
        let height = loose.top().value - loose.bottom().value;

        if generated && height <= 0.01 {
            out.synthetic_dropped += 1;
            continue;
        }

        let name = character.font_name();

        out.glyphs.push(Glyph {
            ch,
            x: loose.left().value,
            origin_x: character
                .origin_x()
                .map(|points| points.value)
                .unwrap_or_else(|_| loose.left().value),
            right: loose.right().value,
            bottom: loose.bottom().value,
            top: loose.top().value,
            size: character.scaled_font_size().value,
            bold: is_bold(&character),
            italic: character.font_is_italic(),
            font: (!name.trim().is_empty()).then_some(name),
            generated,
        });
    }

    Ok(out)
}

/// Word-gap fallback, in em, for pages with no space character to measure.
///
/// Deliberately on the wide side. The rule derived from it is a *threshold*
/// below the narrowest real space, so erring wide costs precision rather than
/// silently deleting words.
pub const DEFAULT_SPACE_EM: f32 = 0.25;

/// Measure a page's space width as a fraction of font size.
///
/// A PDF's space character has a real advance box, so the document states its
/// own space width and there is no need to guess from the font size. This is the
/// fix for the word-gap bug: the original rule assumed 0.25 em, but the first
/// real document's space measured 0.203 em, so the threshold sat *above* a
/// genuine space and deleted it while still catching wider spaces elsewhere.
///
/// The median, not the mean: a few stretched justified spaces must not drag the
/// estimate up, because the threshold has to stay below the *narrowest* real
/// space, not the average one.
pub fn measure_space_em(glyphs: &[Glyph]) -> f32 {
    let mut ratios: Vec<f32> = glyphs
        .iter()
        .filter(|g| g.ch == ' ' && !g.generated && g.size > 0.0)
        .map(|g| (g.right - g.x) / g.size)
        .filter(|ratio| *ratio > 0.02 && *ratio < 1.0)
        .collect();

    if ratios.is_empty() {
        return DEFAULT_SPACE_EM;
    }

    ratios.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    ratios[ratios.len() / 2]
}

/// Whether a character is rendered bold.
///
/// Base-14 standard fonts report no weight at all, so the font name is the only
/// evidence available for them. Both paths are needed; neither alone is enough.
fn is_bold(character: &PdfPageTextChar) -> bool {
    match character.font_weight() {
        Some(PdfFontWeight::Weight600)
        | Some(PdfFontWeight::Weight700Bold)
        | Some(PdfFontWeight::Weight800)
        | Some(PdfFontWeight::Weight900) => true,
        Some(PdfFontWeight::Custom(value)) => value >= 600,
        Some(_) => character.font_is_bold_reenforced(),
        None => {
            let name = character.font_name().to_ascii_lowercase();
            name.contains("bold") || name.contains("black") || name.contains("heavy")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glyph(ch: char, size: f32, x: f32, right: f32) -> Glyph {
        Glyph {
            ch,
            x,
            origin_x: x,
            right,
            bottom: 0.0,
            top: size,
            size,
            bold: false,
            italic: false,
            font: None,
            generated: false,
        }
    }

    #[test]
    fn measures_a_documents_own_space_width() {
        // The first real document's font: 2.49pt of space at 11pt.
        let glyphs = vec![
            glyph('a', 11.0, 0.0, 5.0),
            glyph(' ', 11.0, 5.0, 7.49),
            glyph('b', 11.0, 7.49, 12.0),
            glyph(' ', 11.0, 12.0, 14.49),
        ];
        let em = measure_space_em(&glyphs);
        assert!((em - 0.2264).abs() < 0.001, "got {em}");
    }

    #[test]
    fn a_justified_space_does_not_drag_the_estimate_up() {
        // One grotesquely stretched space must not move the median.
        let glyphs = vec![
            glyph(' ', 10.0, 0.0, 2.5),
            glyph(' ', 10.0, 0.0, 2.5),
            glyph(' ', 10.0, 0.0, 2.5),
            glyph(' ', 10.0, 0.0, 40.0),
        ];
        let em = measure_space_em(&glyphs);
        assert!((em - 0.25).abs() < 0.001, "got {em}");
    }

    #[test]
    fn falls_back_when_a_page_has_no_spaces() {
        let glyphs = vec![glyph('a', 11.0, 0.0, 5.0)];
        assert_eq!(measure_space_em(&glyphs), DEFAULT_SPACE_EM);
    }

    #[test]
    fn ignores_generated_spaces_when_measuring() {
        let mut generated = glyph(' ', 1.0, 0.0, 1.0);
        generated.generated = true;
        let glyphs = vec![glyph(' ', 10.0, 0.0, 2.5), generated];
        assert!((measure_space_em(&glyphs) - 0.25).abs() < 0.001);
    }
}
