//! Horizontal ruling-line extraction.
//!
//! A table's rows are separated by drawn lines, and a row whose every cell wraps
//! is otherwise indistinguishable from two rows: the second line fills exactly
//! the same columns as the first. [`crate::analyze`] uses these rules to tell
//! the two apart; this module only reads them.
//!
//! **The rules are filled rectangles, not stroked lines.** Word draws each cell
//! border as its own thin filled path: measured on page 1 of
//! `TP2 Admin BD 2026_2027.pdf`, a row border is 0.4pt high and split at the
//! column border into `72.22..181.85` and `182.25..523.38` — a 0.4pt gap that is
//! the vertical border's own width. The two halves are one row boundary, so
//! collinear pieces are merged. A stroked line (`draw_line` in the fixture)
//! reports bounds that include its stroke; where PDFium reports a zero-height
//! path instead, the stroke width is used as its thickness.
//!
//! **Only horizontal rules are read.** Row grouping needs those. Vertical rules
//! are the source of column widths, which is a separate change: reading them
//! now would mean a `ColumnSpec` with no consumer.
//!
//! Deliberately not handled: a producer that draws the whole grid as one path
//! object. Its bounds are the whole table, so no thin edge can be recovered
//! without walking path segments. Both real documents and the fixtures draw one
//! object per border, which is what [`extract`] reads.
//!
//! Coordinates are the same as [`crate::glyph::Glyph`]: points from the page's
//! left and bottom edges, y increasing upward.

use pdfium_render::prelude::*;

/// A horizontal ruling line.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rule {
    pub x0: f32,
    pub x1: f32,
    /// Centre of the line's thickness.
    pub y: f32,
    pub thickness: f32,
}

/// Thickest object still read as a rule, in points.
///
/// Measured borders are 0.4pt (TP2), 0.48-0.84pt (3IRS). The shaded header row
/// of TP2 page 1 is a 2.0pt-high filled rectangle and is not a border, so the
/// bound sits below it.
const MAX_THICKNESS: f32 = 1.5;

/// Shortest object still read as a rule, in points.
///
/// Word also emits the corner joins of a border as 0.4x0.4pt squares (page 1 of
/// TP2); they are below this, and a real per-cell border measures 109.6pt on the
/// glossary's left cell.
const MIN_LENGTH: f32 = 4.0;

/// Collinear pieces separated by at most this along x are one rule.
///
/// The measured gap is the 0.4pt vertical border between two cells; 1pt admits
/// it without joining genuinely separate rules.
const JOIN_GAP: f32 = 1.0;

/// Two pieces whose centres differ by at most this in y are the same rule.
const JOIN_Y: f32 = 0.5;

impl Rule {
    /// A rule from an object's bounds, when the bounds are thin and long enough.
    pub fn from_bounds(left: f32, right: f32, bottom: f32, top: f32) -> Option<Self> {
        let thickness = top - bottom;
        let length = right - left;
        (thickness <= MAX_THICKNESS && length >= MIN_LENGTH).then_some(Self {
            x0: left,
            x1: right,
            y: (bottom + top) / 2.0,
            thickness,
        })
    }
}

/// Read the horizontal rulings on a page.
///
/// An object whose bounds cannot be read is skipped: one malformed path is not
/// a reason to fail the page, and a missed rule only costs a row boundary, which
/// falls back to the subset test.
pub fn extract(page: &PdfPage) -> Vec<Rule> {
    let mut raw = Vec::new();

    for object in page.objects().iter() {
        if object.object_type() != PdfPageObjectType::Path {
            continue;
        }
        let Ok(bounds) = object.bounds() else {
            continue;
        };
        let rect = bounds.to_rect();
        // A filled border rectangle states its own thickness. A zero-height
        // path is a stroked line whose bounds exclude the stroke, so the stroke
        // width is what a reader sees; PDFium usually includes the stroke in the
        // bounds, in which case the first branch already has it. PDFium also
        // reports a default stroke width on filled paths, which must not move a
        // filled rule's centre.
        let height = rect.height().value;
        let (bottom, top) = if height > 0.01 {
            (rect.bottom().value, rect.top().value)
        } else {
            let stroke = object.stroke_width().map(|width| width.value).unwrap_or(0.0);
            (rect.bottom().value, rect.bottom().value + stroke)
        };
        if let Some(rule) = Rule::from_bounds(rect.left().value, rect.right().value, bottom, top) {
            raw.push(rule);
        }
    }

    merge(raw)
}

/// Join the per-cell pieces of one border into a single rule.
fn merge(mut rules: Vec<Rule>) -> Vec<Rule> {
    rules.sort_by(|a, b| {
        b.y.partial_cmp(&a.y)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.x0.partial_cmp(&b.x0).unwrap_or(std::cmp::Ordering::Equal))
    });

    let mut merged: Vec<Rule> = Vec::new();
    for rule in rules {
        let existing = merged.iter_mut().find(|candidate| {
            (candidate.y - rule.y).abs() <= JOIN_Y
                && rule.x0 <= candidate.x1 + JOIN_GAP
                && candidate.x0 <= rule.x1 + JOIN_GAP
        });

        match existing {
            Some(candidate) => {
                candidate.x0 = candidate.x0.min(rule.x0);
                candidate.x1 = candidate.x1.max(rule.x1);
                candidate.y = (candidate.y + rule.y) / 2.0;
                candidate.thickness = candidate.thickness.max(rule.thickness);
            }
            None => merged.push(rule),
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_thin_long_object_is_a_rule() {
        // The left cell border of TP2 page 1: 0.4pt high, 109.6pt long.
        let rule = Rule::from_bounds(72.22, 181.85, 616.24, 616.64).expect("a rule");
        assert!((rule.y - 616.44).abs() < 0.001, "y={}", rule.y);
        assert!((rule.thickness - 0.4).abs() < 0.001, "{}", rule.thickness);
    }

    #[test]
    fn corner_squares_and_cell_shading_are_not_rules() {
        // Word's border corner join, measured at 0.4x0.4pt.
        assert!(Rule::from_bounds(0.0, 0.4, 0.0, 0.4).is_none());
        // The shaded header band of TP2 page 1 is 2.0pt high.
        assert!(Rule::from_bounds(72.03, 182.04, 0.0, 2.0).is_none());
    }

    #[test]
    fn per_cell_borders_merge_across_the_column_gap() {
        // TP2 page 2's glossary rule: two cell borders with the 0.4pt vertical
        // border between them.
        let merged = merge(vec![
            Rule {
                x0: 72.22,
                x1: 181.85,
                y: 616.44,
                thickness: 0.4,
            },
            Rule {
                x0: 182.25,
                x1: 523.38,
                y: 616.44,
                thickness: 0.4,
            },
        ]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].x0, 72.22);
        assert_eq!(merged[0].x1, 523.38);
    }

    #[test]
    fn a_gap_wider_than_a_border_is_not_one_rule() {
        // Two tables side by side: a 5pt gap is layout, not a border join.
        let merged = merge(vec![
            Rule {
                x0: 0.0,
                x1: 100.0,
                y: 500.0,
                thickness: 0.4,
            },
            Rule {
                x0: 105.0,
                x1: 200.0,
                y: 500.0,
                thickness: 0.4,
            },
        ]);
        assert_eq!(merged.len(), 2);
    }
}
