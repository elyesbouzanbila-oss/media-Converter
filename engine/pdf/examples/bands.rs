//! Print the bands and segments a page produces, with the geometry behind them.
//!
//!     cargo run -p dconv-pdf --example bands -- <file.pdf> [page] [--glyphs] [--rules]
//!
//! Exists because "this text came out in the wrong order" is the single most
//! common thing to investigate, and the answer is always in the geometry that
//! *our* bander saw. Reading that geometry back out of a second library
//! reintroduces exactly the assumptions under test, so this prints the
//! intermediate structure directly. `--rules` prints the ruling lines the table
//! builder consults, because a row boundary that reads wrong is usually a rule
//! that was missed or merged into its neighbour.

use dconv_pdf::{bind, glyph, layout, rules};
use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut positional = args.iter().filter(|arg| !arg.starts_with("--"));
    let path = positional.next().ok_or("usage: bands <pdf> [page] [--glyphs] [--rules]")?;
    let index: usize = match positional.next() {
        Some(value) => value.parse()?,
        None => 0,
    };
    let show_glyphs = args.iter().any(|arg| arg == "--glyphs");
    let show_rules = args.iter().any(|arg| arg == "--rules");

    let pdfium = bind::bind()?;
    let document = pdfium.load_pdf_from_file(Path::new(path), None)?;
    let page = document.pages().get(index as i32)?;

    let extraction = glyph::extract_page(&page)?;
    let space_em = glyph::measure_space_em(&extraction.glyphs);
    println!(
        "page {index}, {} glyphs, space_em={space_em:.4}, page {:.1}x{:.1}",
        extraction.glyphs.len(),
        page.width().value,
        page.height().value
    );

    if show_glyphs {
        let mut sorted = extraction.glyphs.iter().collect::<Vec<_>>();
        sorted.sort_by(|a, b| {
            b.bottom
                .partial_cmp(&a.bottom)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal))
        });
        println!("\n-- glyphs, bottom descending (the order the bander walks) --");
        for g in sorted {
            println!(
                "  {:?} x={:7.2}..{:7.2} bottom={:7.2} top={:7.2} size={:5.2} bold={} gen={} font={:?}",
                g.ch, g.x, g.right, g.bottom, g.top, g.size, g.bold, g.generated, g.font
            );
        }
        println!();
    }

    if show_rules {
        let rules = rules::extract(&page);
        println!("\n-- rules ({}) --", rules.len());
        for rule in &rules {
            println!(
                "  x={:7.2}..{:7.2}  y={:7.2}  thickness={:.2}",
                rule.x0, rule.x1, rule.y, rule.thickness
            );
        }
    }

    for (i, band) in layout::analyse_page(&extraction.glyphs, space_em)
        .iter()
        .enumerate()
    {
        println!(
            "\nband {i}  bottom={:.2} top={:.2}",
            band.bottom, band.top
        );
        for (j, segment) in band.segments.iter().enumerate() {
            println!(
                "  segment {j}  x={:7.2}..{:7.2} bottom={:7.2} size={:5.2} bold={} text={:?}",
                segment.left,
                segment.right,
                segment.bottom,
                segment.size,
                segment.bold,
                segment.text()
            );
        }
    }

    Ok(())
}
