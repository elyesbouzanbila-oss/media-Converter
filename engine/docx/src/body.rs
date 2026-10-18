//! Rendering the document IR into `word/document.xml`.
//!
//! Everything here is text assembly rather than a DOM, on purpose. The IR is a
//! small, closed vocabulary, and the output is a single XML part with a flat
//! structure; building it as a string keeps the mapping from IR node to OOXML
//! node directly readable, which is the property that matters most when a
//! fidelity bug needs chasing.

use crate::parts::{escape_into, escape_attr};
use crate::RelSink;
use dconv_ir as ir;

/// What a block needs beyond itself: the page geometry (for table column
/// widths) and the relationship sink (for hyperlinks).
pub(crate) struct Ctx<'a> {
    page: &'a ir::PageSetup,
    rels: &'a mut RelSink,
    /// Extra left indent in twips, applied to every paragraph produced by the
    /// current nested context. Block quotes raise it on the way in.
    indent: i32,
}

/// Render the body contents: the blocks, then the section properties that carry
/// page geometry.
pub(crate) fn render(doc: &ir::Document, rels: &mut RelSink) -> String {
    let mut out = String::with_capacity(8192);
    let mut ctx = Ctx {
        page: &doc.page,
        rels,
        indent: 0,
    };

    for top in &doc.blocks {
        block(&mut ctx, top, &mut out, 0);
    }

    // A document whose last block is a table needs a trailing paragraph. Word
    // merges the final table into the section properties otherwise, which
    // silently loses the last row's formatting.
    if matches!(doc.blocks.last(), Some(ir::Block::Table(_))) {
        out.push_str("<w:p/>");
    }

    out.push_str(&sect_pr(doc.page));
    out
}

fn block(ctx: &mut Ctx, b: &ir::Block, out: &mut String, list_level: u8) {
    match b {
        ir::Block::Heading {
            level,
            content,
            style,
        } => {
            let mut ppr = format!("<w:pStyle w:val=\"Heading{}\"/>", (*level).clamp(1, 6));
            let style = style.clone().unwrap_or_default();
            if let Some(align) = style.align {
                ppr.push_str(&jc(align));
            }
            ppr.push_str(&indent(&style, ctx.indent));
            let mut inner = String::new();
            inline_content(ctx, content, &mut inner);
            paragraph(out, &ppr, &inner);
        }

        ir::Block::Paragraph {
            content,
            align,
            style,
        } => {
            let style = style.clone().unwrap_or_default();
            let mut ppr = String::new();
            ppr.push_str(&indent(&style, ctx.indent));
            ppr.push_str(&spacing(&style));
            // The block's own alignment wins; `style.align` is the fallback for
            // readers that only recorded it there.
            match (align, style.align) {
                (ir::Align::Start, Some(from_style)) if from_style != ir::Align::Start => {
                    ppr.push_str(&jc(from_style))
                }
                (align, _) if *align != ir::Align::Start => ppr.push_str(&jc(*align)),
                _ => {}
            }

            let mut inner = String::new();
            inline_content(ctx, content, &mut inner);
            paragraph(out, &ppr, &inner);
        }

        ir::Block::List(list) => list_block(ctx, list, out, list_level),

        ir::Block::Table(t) => table_block(ctx, t, out),

        ir::Block::Quote { blocks } => {
            // Raise the indent, render, restore. Simpler than rewriting emitted
            // XML after the fact, and it composes: a quote nested in a list item
            // keeps the list indentation underneath it.
            ctx.indent += 720;
            for inner_block in blocks {
                block(ctx, inner_block, out, list_level);
            }
            ctx.indent -= 720;
        }

        ir::Block::CodeBlock { text, .. } => {
            let mut inner = String::new();
            for (index, line) in text.split('\n').enumerate() {
                if index > 0 {
                    inner.push_str("<w:br/>");
                }
                inner.push_str("<w:t xml:space=\"preserve\">");
                escape_into(line, &mut inner);
                inner.push_str("</w:t>");
            }
            let ppr = "<w:pStyle w:val=\"Normal\"/><w:shd w:val=\"clear\" w:color=\"auto\" w:fill=\"F5F5F5\"/>";
            let mut run = String::from("<w:r><w:rPr><w:rFonts w:ascii=\"Consolas\" w:hAnsi=\"Consolas\"/><w:sz w:val=\"18\"/></w:rPr>");
            run.push_str(&inner);
            run.push_str("</w:r>");
            paragraph(out, ppr, &run);
        }

        ir::Block::Caption { content } => {
            let mut inner = String::new();
            inline_content(ctx, content, &mut inner);
            paragraph(
                out,
                "<w:pStyle w:val=\"Normal\"/><w:jc w:val=\"center\"/>",
                &format!("<w:r><w:rPr><w:i/><w:sz w:val=\"18\"/></w:rPr>{inner}</w:r>"),
            );
        }

        ir::Block::Rule => {
            paragraph(
                out,
                "<w:pBdr><w:bottom w:val=\"single\" w:sz=\"6\" w:space=\"1\" w:color=\"auto\"/></w:pBdr>",
                "",
            );
        }

        ir::Block::PageBreak => {
            paragraph(out, "", "<w:r><w:br w:type=\"page\"/></w:r>");
        }

        ir::Block::Image(image) => {
            // Images need media parts, content-type defaults and a drawing
            // relationship. Deferred until the PDF reader actually produces
            // image blocks, so that the code can be written against real input
            // rather than guessed at.
            let alt = image.alt.clone().unwrap_or_else(|| "image".to_string());
            let mut inner = String::new();
            run_xml(&format!("[{alt}]"), &ir::TextStyle::default(), &mut inner);
            paragraph(
                out,
                "<w:jc w:val=\"center\"/>",
                &format!("<w:r><w:rPr><w:i/><w:color w:val=\"808080\"/></w:rPr>{inner}</w:r>"),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Lists
// ---------------------------------------------------------------------------

fn list_block(ctx: &mut Ctx, list: &ir::ListBlock, out: &mut String, list_level: u8) {
    let mut number = list.start.unwrap_or(1);

    for item in &list.items {
        let level = list_level.saturating_add(item.level);
        let left = 360 * (level as i32 + 1) + ctx.indent;
        let ppr = format!("<w:ind w:left=\"{left}\" w:hanging=\"360\"/>");

        // The marker is emitted as literal text rather than as a numbering
        // definition. A real `w:numPr` needs a `word/numbering.xml` part with
        // abstract numbering definitions and a relationship to point at it,
        // which is a meaningful amount of machinery for a first pass. The
        // visible result is close enough to review, and swapping in real
        // numbering later is contained to this function.
        let marker = if list.ordered {
            let text = format!("{number}.");
            number += 1;
            text
        } else {
            list.marker.clone().unwrap_or_else(|| "•".to_string())
        };

        match item.blocks.first() {
            Some(ir::Block::Paragraph { content, .. }) => {
                let mut inner = String::new();
                marker_run(&marker, &mut inner);
                inline_content(ctx, content, &mut inner);
                paragraph(out, &ppr, &inner);
            }
            Some(other) => {
                // A list item whose body is not a paragraph (a nested list, a
                // table) has no marker to hang off. Emit it as-is rather than
                // forcing a synthetic paragraph around it.
                block(ctx, other, out, level);
            }
            None => {}
        }

        for extra in item.blocks.iter().skip(1) {
            block(ctx, extra, out, level);
        }
    }
}

fn marker_run(marker: &str, out: &mut String) {
    // A literal tab character inside `w:t` is not rendered as a tab by Word;
    // it has to be a `w:tab` element. Getting this wrong collapses every list
    // marker against its text.
    out.push_str("<w:r><w:t xml:space=\"preserve\">");
    escape_into(marker, out);
    out.push_str("</w:t><w:tab/></w:r>");
}

// ---------------------------------------------------------------------------
// Tables
// ---------------------------------------------------------------------------

fn table_block(ctx: &mut Ctx, table: &ir::TableBlock, out: &mut String) {
    // OOXML rejects ragged rows outright, so this is not cosmetic.
    let mut table = table.clone();
    table.pad_to_rectangle();

    if table.rows.is_empty() {
        return;
    }

    let columns = table.column_count().max(1);
    let content_width = ctx.page.content_width().0.max(1.0);
    let even = content_width / columns as f64;

    let widths: Vec<f64> = (0..columns)
        .map(|index| {
            table
                .columns
                .get(index)
                .and_then(|c| c.width)
                .map(|w| w.0)
                .filter(|w| *w > 0.0)
                .unwrap_or(even)
        })
        .collect();

    out.push_str("<w:tbl><w:tblPr>");
    // Fixed layout, because we know the column widths from the source geometry.
    // Autofit would let Word re-flow the columns and destroy the alignment the
    // source document had.
    out.push_str(&format!(
        "<w:tblW w:w=\"{}\" w:type=\"dxa\"/><w:tblLayout w:type=\"fixed\"/>",
        content_width.round() as i32
    ));
    if table.borders != ir::BorderStyle::None {
        out.push_str("<w:tblBorders>");
        for edge in ["top", "left", "bottom", "right", "insideH", "insideV"] {
            out.push_str(&format!(
                "<w:{edge} w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"auto\"/>"
            ));
        }
        out.push_str("</w:tblBorders>");
    }
    out.push_str("</w:tblPr>");

    out.push_str("<w:tblGrid>");
    for width in &widths {
        out.push_str(&format!("<w:gridCol w:w=\"{}\"/>", width.round() as i32));
    }
    out.push_str("</w:tblGrid>");

    for row in &table.rows {
        let is_header_row = row.cells.iter().any(|c| c.is_header);
        out.push_str("<w:tr>");
        if is_header_row {
            // Repeats the header when the table crosses a page break.
            out.push_str("<w:trPr><w:tblHeader/></w:trPr>");
        }

        let mut column = 0usize;
        for cell in &row.cells {
            let span = cell.col_span.max(1) as usize;
            let width: f64 = (column..(column + span).min(widths.len()))
                .map(|index| widths[index])
                .sum::<f64>()
                .max(1.0);

            out.push_str("<w:tc><w:tcPr>");
            out.push_str(&format!(
                "<w:tcW w:w=\"{}\" w:type=\"dxa\"/>",
                width.round() as i32
            ));
            if span > 1 {
                out.push_str(&format!("<w:gridSpan w:val=\"{span}\"/>"));
            }
            out.push_str(match cell.vertical_align {
                ir::VerticalAlign::Top => "<w:vAlign w:val=\"top\"/>",
                ir::VerticalAlign::Middle => "<w:vAlign w:val=\"center\"/>",
                ir::VerticalAlign::Bottom => "<w:vAlign w:val=\"bottom\"/>",
            });
            out.push_str("</w:tcPr>");

            if cell.blocks.is_empty() {
                // A `w:tc` with no `w:p` is invalid and Word will refuse the
                // file rather than render an empty cell.
                out.push_str("<w:p/>");
            } else {
                for inner_block in &cell.blocks {
                    block(ctx, inner_block, out, 0);
                }
            }
            out.push_str("</w:tc>");
            column += span;
        }

        out.push_str("</w:tr>");
    }

    out.push_str("</w:tbl>");
}

// ---------------------------------------------------------------------------
// Inline content
// ---------------------------------------------------------------------------

fn inline_content(ctx: &mut Ctx, inlines: &[ir::Inline], out: &mut String) {
    for inline in inlines {
        match inline {
            ir::Inline::Text(run) => run_xml(&run.text, &run.style, out),

            ir::Inline::LineBreak => out.push_str("<w:r><w:br/></w:r>"),

            // A non-breaking space is a real character, not a construct: Word
            // preserves U+00A0 verbatim and will not break the line on it.
            ir::Inline::NonBreakingSpace => {
                run_xml("\u{00A0}", &ir::TextStyle::default(), out)
            }

            ir::Inline::Math { source } => run_xml(source, &ir::TextStyle::default(), out),

            ir::Inline::FootnoteRef { label } => {
                // Rendered as a superscript marker with no note body attached.
                // Real footnotes need a `word/footnotes.xml` part, an ID per
                // note, and a relationship; the IR already has somewhere to put
                // the bodies, so this is a writer gap rather than a design one.
                let style = ir::TextStyle {
                    vertical: ir::VerticalShift::Superscript,
                    ..Default::default()
                };
                run_xml(&format!("[{label}]"), &style, out);
            }

            ir::Inline::Link { content, target, .. } => {
                let id = ctx.rels.add_link(target);
                out.push_str(&format!("<w:hyperlink r:id=\"{id}\">"));
                inline_content(ctx, content, out);
                out.push_str("</w:hyperlink>");
            }
        }
    }
}

fn run_xml(text: &str, style: &ir::TextStyle, out: &mut String) {
    if text.is_empty() {
        return;
    }

    out.push_str("<w:r>");
    let props = run_props(style);
    if !props.is_empty() {
        out.push_str("<w:rPr>");
        out.push_str(&props);
        out.push_str("</w:rPr>");
    }

    // Newlines inside a run are hard breaks. Word has no concept of a newline
    // in text content, so leaving one in would silently delete it.
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            out.push_str("<w:br/>");
        }
        if line.is_empty() {
            continue;
        }
        out.push_str("<w:t xml:space=\"preserve\">");
        escape_into(line, out);
        out.push_str("</w:t>");
    }

    out.push_str("</w:r>");
}

fn run_props(style: &ir::TextStyle) -> String {
    let mut out = String::new();

    // Font has to be decided once, because `code` and an explicit font both
    // resolve to the same `w:rFonts` element and emitting two would be invalid.
    if let Some(name) = style.font.as_deref().or(if style.code {
        Some("Consolas")
    } else {
        None
    }) {
        let name = escape_attr(name);
        out.push_str(&format!(
            "<w:rFonts w:ascii=\"{name}\" w:hAnsi=\"{name}\" w:cs=\"{name}\"/>"
        ));
    }

    if style.bold {
        out.push_str("<w:b/>");
    }
    if style.italic {
        out.push_str("<w:i/>");
    }
    if style.underline {
        out.push_str("<w:u w:val=\"single\"/>");
    }
    if style.strike {
        out.push_str("<w:strike/>");
    }
    if style.small_caps {
        out.push_str("<w:smallCaps/>");
    }

    out.push_str(match style.vertical {
        ir::VerticalShift::Superscript => "<w:vertAlign w:val=\"superscript\"/>",
        ir::VerticalShift::Subscript => "<w:vertAlign w:val=\"subscript\"/>",
        ir::VerticalShift::None => "",
    });

    if let Some(size) = style.size {
        // OOXML font size is in half-points.
        let half_points = (size.as_pt() * 2.0).round().max(2.0) as i32;
        out.push_str(&format!("<w:sz w:val=\"{half_points}\"/>"));
    }

    if let Some(color) = style.color {
        out.push_str(&format!(
            "<w:color w:val=\"{:02X}{:02X}{:02X}\"/>",
            color.r, color.g, color.b
        ));
    }

    if let Some(highlight) = style.highlight {
        // Run shading rather than `w:highlight`, because `w:highlight` only
        // accepts a fixed palette of named colours and the IR carries an
        // arbitrary RGB value.
        out.push_str(&format!(
            "<w:shd w:val=\"clear\" w:color=\"auto\" w:fill=\"{:02X}{:02X}{:02X}\"/>",
            highlight.r, highlight.g, highlight.b
        ));
    }

    out
}

// ---------------------------------------------------------------------------
// Paragraph property helpers
// ---------------------------------------------------------------------------

fn paragraph(out: &mut String, ppr: &str, inner: &str) {
    out.push_str("<w:p>");
    if !ppr.is_empty() {
        out.push_str("<w:pPr>");
        out.push_str(ppr);
        out.push_str("</w:pPr>");
    }
    out.push_str(inner);
    out.push_str("</w:p>");
}

fn jc(align: ir::Align) -> String {
    // The IR is direction-neutral, OOXML's older attributes are not. The
    // directional equivalents ("start"/"end") only exist in newer schema
    // versions, so the physical names are the portable choice.
    let value = match align {
        ir::Align::Start => "left",
        ir::Align::Center => "center",
        ir::Align::End => "right",
        ir::Align::Justify => "both",
    };
    format!("<w:jc w:val=\"{value}\"/>")
}

fn indent(style: &ir::ParagraphStyle, extra: i32) -> String {
    let left = extra + style.indent_start.map(|l| l.as_twips()).unwrap_or(0);
    let right = style.indent_end.map(|l| l.as_twips()).unwrap_or(0);
    if left == 0 && right == 0 {
        return String::new();
    }
    // `firstLine="0"` cancels Word's default first-line indent so that wrapped
    // source lines do not acquire a phantom indentation.
    format!("<w:ind w:left=\"{left}\" w:right=\"{right}\" w:firstLine=\"0\"/>")
}

fn spacing(style: &ir::ParagraphStyle) -> String {
    let mut attrs = String::new();
    if let Some(before) = style.space_before {
        attrs.push_str(&format!(" w:before=\"{}\"", before.as_twips()));
    }
    if let Some(after) = style.space_after {
        attrs.push_str(&format!(" w:after=\"{}\"", after.as_twips()));
    }
    if let Some(height) = style.line_height {
        // Exact line height, in twentieths of a point.
        attrs.push_str(&format!(
            " w:line=\"{}\" w:lineRule=\"exact\"",
            height.as_twips()
        ));
    }
    if attrs.is_empty() {
        String::new()
    } else {
        format!("<w:spacing{attrs}/>")
    }
}

// ---------------------------------------------------------------------------
// Section properties
// ---------------------------------------------------------------------------

fn sect_pr(page: ir::PageSetup) -> String {
    let width = page.width.as_twips();
    let height = page.height.as_twips();
    let orientation = if width > height {
        " w:orient=\"landscape\""
    } else {
        ""
    };

    format!(
        "<w:sectPr>\
<w:pgSz w:w=\"{width}\" w:h=\"{height}\"{orientation}/>\
<w:pgMar w:top=\"{top}\" w:right=\"{right}\" w:bottom=\"{bottom}\" w:left=\"{left}\" \
w:header=\"720\" w:footer=\"720\" w:gutter=\"0\"/>\
</w:sectPr>",
        top = page.margin_top.as_twips(),
        right = page.margin_end.as_twips(),
        bottom = page.margin_bottom.as_twips(),
        left = page.margin_start.as_twips(),
    )
}
