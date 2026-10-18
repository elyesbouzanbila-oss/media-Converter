//! DOCX writer: renders the document IR as Office Open XML.
//!
//! # Why the package is assembled by hand
//!
//! The obvious shortcut is to use an existing DOCX library. Two things argued
//! against it. First, the libraries in this space are write-only and generally
//! *are* the format: they own the styles, the numbering and the section
//! properties, which is exactly the surface a converter needs to control, since
//! the entire job is translating somebody else's decisions into these ones.
//! Second, the part of DOCX that is genuinely load-bearing for a converter —
//! paragraph and character styles Word recognises, real tables, page geometry —
//! is a small fraction of the format.
//!
//! So the writer owns its package. The cost is that the content-type map and the
//! relationship graph must be kept in step with the parts by hand; the test
//! `every_part_is_covered_by_content_types` exists specifically to catch that
//! class of mistake, because Word's failure mode for it is refusing the file
//! outright rather than degrading.

mod body;
mod parts;

use dconv_ir as ir;
use std::io::{Cursor, Write};
use std::path::Path;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

pub use dconv_ir as ir_crate;

#[derive(Debug, thiserror::Error)]
pub enum DocxError {
    #[error("the .docx archive could not be assembled: {0}")]
    Zip(#[from] zip::result::ZipError),

    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
}

/// Collects the relationships `document.xml` needs as the body is rendered.
///
/// `rId1` is reserved for the styles part, so hyperlinks are numbered from
/// `rId2` upwards. Anything that emits an `r:id` must go through this, or the
/// document will reference a relationship that does not exist.
#[derive(Default)]
pub(crate) struct RelSink {
    hyperlinks: Vec<String>,
}

impl RelSink {
    pub(crate) fn add_link(&mut self, target: &str) -> String {
        // Reuse an existing relationship for a repeated target. Documents cite
        // the same URL many times, and one relationship per occurrence bloats
        // the rels part for no benefit.
        if let Some(index) = self.hyperlinks.iter().position(|t| t == target) {
            return format!("rId{}", index + 2);
        }
        self.hyperlinks.push(target.to_string());
        format!("rId{}", self.hyperlinks.len() + 1)
    }
}

/// Render a document to the bytes of a `.docx` package.
pub fn write_bytes(document: &ir::Document) -> Result<Vec<u8>, DocxError> {
    let mut rels = RelSink::default();
    let body = body::render(document, &mut rels);

    let document_xml = format!(
        "{decl}<w:document xmlns:w=\"{w}\" \
xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\">\
<w:body>{body}</w:body></w:document>",
        decl = parts::XML_DECL,
        w = parts::W_NS,
    );

    let mut zip = ZipWriter::new(Cursor::new(Vec::with_capacity(32 * 1024)));
    // Deflate rather than store: these are text-heavy XML parts that compress
    // by roughly 10x, and the CPU cost is negligible next to conversion.
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    add_part(&mut zip, parts::part::CONTENT_TYPES, &parts::content_types(), options)?;
    add_part(&mut zip, parts::part::PACKAGE_RELS, &parts::package_rels(), options)?;
    add_part(&mut zip, parts::part::DOCUMENT, &document_xml, options)?;
    add_part(
        &mut zip,
        parts::part::DOCUMENT_RELS,
        &parts::document_rels(&rels.hyperlinks),
        options,
    )?;
    add_part(&mut zip, parts::part::STYLES, &parts::styles(), options)?;
    add_part(
        &mut zip,
        parts::part::CORE_PROPS,
        &parts::core_properties(document),
        options,
    )?;
    add_part(&mut zip, parts::part::APP_PROPS, &parts::app_properties(), options)?;

    let finished = zip.finish()?;
    Ok(finished.into_inner())
}

/// Render a document and write it to `path`.
///
/// The write is atomic: a temporary file in the destination directory is
/// flushed and then renamed over the target. A batch that is cancelled halfway
/// must not leave a truncated `.docx` behind, because Word reports a truncated
/// file as *corrupt*, which is a much worse user experience than a missing one —
/// the user concludes the converter produced bad output rather than that they
/// stopped it.
pub fn write_to_path(document: &ir::Document, path: &Path) -> Result<(), DocxError> {
    let bytes = write_bytes(document)?;

    let directory = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    let mut temp = tempfile::NamedTempFile::new_in(directory).map_err(|source| DocxError::Io {
        context: format!("creating a temporary file in {}", directory.display()),
        source,
    })?;

    temp.as_file_mut()
        .write_all(&bytes)
        .and_then(|()| temp.as_file_mut().sync_all())
        .map_err(|source| DocxError::Io {
            context: format!("writing the temporary file for {}", path.display()),
            source,
        })?;

    temp.persist(path).map_err(|error| DocxError::Io {
        context: format!("moving the finished document into place at {}", path.display()),
        source: error.error,
    })?;

    Ok(())
}

fn add_part<W: Write + std::io::Seek>(
    zip: &mut ZipWriter<W>,
    name: &str,
    content: &str,
    options: SimpleFileOptions,
) -> Result<(), DocxError> {
    zip.start_file(name, options)?;
    zip.write_all(content.as_bytes())
        .map_err(|source| DocxError::Io {
            context: format!("writing the {name} part into the archive"),
            source,
        })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn sample_document() -> ir::Document {
        use ir::*;

        let bold = TextStyle {
            bold: true,
            ..Default::default()
        };

        let mut table = TableBlock {
            caption: None,
            columns: vec![],
            borders: BorderStyle::Visible,
            rows: vec![
                TableRow::new([
                    TableCell {
                        blocks: vec![Block::Paragraph {
                            content: vec![Inline::Text(TextRun::styled("Propriété", bold.clone()))],
                            align: Align::Start,
                            style: None,
                        }],
                        is_header: true,
                        ..Default::default()
                    },
                    TableCell {
                        blocks: vec![Block::Paragraph {
                            content: vec![Inline::text("Description")],
                            align: Align::Start,
                            style: None,
                        }],
                        is_header: true,
                        ..Default::default()
                    },
                ]),
                // Deliberately ragged: OOXML rejects it, so the writer must pad.
                TableRow::new([TableCell::text("Atomicité")]),
            ],
        };
        table.pad_to_rectangle();

        let mut document = Document::new();
        document.metadata.title = Some("Conversion test".into());
        document.blocks = vec![
            Block::Heading {
                level: 1,
                content: vec![Inline::text("Rappel de cours")],
                style: None,
            },
            Block::Paragraph {
                content: vec![
                    Inline::text("Une transaction est "),
                    Inline::Text(TextRun::styled("indivisible", bold)),
                    Inline::text("."),
                ],
                align: Align::Start,
                style: None,
            },
            Block::List(ListBlock {
                ordered: false,
                start: None,
                marker: None,
                items: vec![
                    ListItem {
                        level: 0,
                        blocks: vec![Block::Paragraph {
                            content: vec![Inline::text("Atomicité")],
                            align: Align::Start,
                            style: None,
                        }],
                    },
                    ListItem {
                        level: 0,
                        blocks: vec![Block::Paragraph {
                            content: vec![Inline::text("Cohérence")],
                            align: Align::Start,
                            style: None,
                        }],
                    },
                ],
            }),
            Block::Table(table),
            Block::Paragraph {
                content: vec![Inline::Link {
                    content: vec![Inline::text("documentation")],
                    target: "https://example.invalid/docs".into(),
                    title: None,
                }],
                align: Align::Start,
                style: None,
            },
        ];
        document
    }

    fn read_part(bytes: &[u8], name: &str) -> String {
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes.to_vec())).expect("open archive");
        let mut file = archive.by_name(name).expect("part present");
        let mut text = String::new();
        file.read_to_string(&mut text).expect("part is utf-8");
        text
    }

    fn part_names(bytes: &[u8]) -> Vec<String> {
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes.to_vec())).expect("open archive");
        (0..archive.len())
            .map(|index| {
                archive
                    .by_index(index)
                    .expect("entry")
                    .name()
                    .expect("entry name")
                    .to_string()
            })
            .collect()
    }

    /// The invariant that keeps hand-assembled OOXML valid: every part in the
    /// archive must be reachable from `[Content_Types].xml`, by extension
    /// default or by explicit override. Word rejects the whole file otherwise,
    /// with no indication of which part was the problem.
    #[test]
    fn every_part_is_covered_by_content_types() {
        let bytes = write_bytes(&sample_document()).expect("render");
        let types = read_part(&bytes, "[Content_Types].xml");

        for name in part_names(&bytes) {
            if name.ends_with('/') {
                continue; // directory entry
            }

            let overridden = types.contains(&format!("PartName=\"/{name}\""));
            let extension = name.rsplit('.').next().unwrap_or_default();
            let defaulted = types.contains(&format!("Extension=\"{extension}\""));

            assert!(
                overridden || defaulted,
                "part {name} has neither an Override nor a Default extension in [Content_Types].xml"
            );
        }
    }

    #[test]
    fn content_type_overrides_point_at_real_parts() {
        let bytes = write_bytes(&sample_document()).expect("render");
        let types = read_part(&bytes, "[Content_Types].xml");
        let names = part_names(&bytes);

        // The mirror of the test above, and the direction that is easy to miss:
        // a stale override naming a part that no longer exists is also
        // rejected.
        for chunk in types.split("PartName=\"/").skip(1) {
            let part = chunk.split('"').next().unwrap_or_default();
            assert!(
                names.iter().any(|n| n == part),
                "content types mention /{part}, which is not in the package"
            );
        }
    }

    #[test]
    fn headings_use_the_builtin_word_style_ids() {
        let bytes = write_bytes(&sample_document()).expect("render");
        let xml = read_part(&bytes, "word/document.xml");
        let styles = read_part(&bytes, "word/styles.xml");

        assert!(xml.contains("<w:pStyle w:val=\"Heading1\"/>"), "got: {xml}");
        // `heading 1`, lowercase, is what makes Word treat this as its own
        // Heading 1 rather than a custom style that merely looks similar.
        assert!(styles.contains("<w:name w:val=\"heading 1\"/>"), "got: {styles}");
        assert!(styles.contains("w:styleId=\"Heading1\""), "got: {styles}");
    }

    #[test]
    fn bold_runs_emit_run_properties() {
        let bytes = write_bytes(&sample_document()).expect("render");
        let xml = read_part(&bytes, "word/document.xml");
        assert!(
            xml.contains("<w:rPr><w:b/></w:rPr><w:t xml:space=\"preserve\">indivisible</w:t>"),
            "got: {xml}"
        );
    }

    #[test]
    fn tables_are_real_word_tables_and_are_padded() {
        let bytes = write_bytes(&sample_document()).expect("render");
        let xml = read_part(&bytes, "word/document.xml");

        assert!(xml.contains("<w:tbl>"), "expected a real table, got: {xml}");
        assert!(xml.contains("<w:tblGrid>"), "got: {xml}");
        assert!(xml.contains("<w:gridCol"), "got: {xml}");

        // Both rows must have two cells, because the source row was ragged and
        // OOXML rejects a ragged table.
        assert_eq!(xml.matches("<w:tr>").count(), 2, "got: {xml}");
        assert_eq!(xml.matches("<w:tc>").count(), 4, "got: {xml}");

        // Repeated across page breaks.
        assert!(xml.contains("<w:tblHeader/>"), "got: {xml}");
    }

    #[test]
    fn a_table_ending_the_document_gets_a_trailing_paragraph() {
        let mut document = ir::Document::new();
        document.blocks = vec![ir::Block::Table(ir::TableBlock {
            caption: None,
            columns: vec![],
            borders: ir::BorderStyle::Visible,
            rows: vec![ir::TableRow::new([ir::TableCell::text("only")])],
        })];

        let bytes = write_bytes(&document).expect("render");
        let xml = read_part(&bytes, "word/document.xml");
        let body = xml.split("<w:body>").nth(1).expect("body");
        assert!(
            body.contains("</w:tbl><w:p/>"),
            "a trailing table must be followed by a paragraph, got: {body}"
        );
    }

    #[test]
    fn hyperlinks_get_a_relationship_and_a_matching_id() {
        let bytes = write_bytes(&sample_document()).expect("render");
        let xml = read_part(&bytes, "word/document.xml");
        let rels = read_part(&bytes, "word/_rels/document.xml.rels");

        assert!(xml.contains("<w:hyperlink r:id=\"rId2\">"), "got: {xml}");
        assert!(rels.contains("Target=\"https://example.invalid/docs\""), "got: {rels}");
        assert!(rels.contains("TargetMode=\"External\""), "got: {rels}");
        // rId1 is the styles part and must not be reused.
        assert!(rels.contains("Id=\"rId1\"") && rels.contains("styles.xml"));
    }

    #[test]
    fn repeated_hyperlinks_share_one_relationship() {
        let mut document = ir::Document::new();
        let link = |text: &str| ir::Block::Paragraph {
            content: vec![ir::Inline::Link {
                content: vec![ir::Inline::text(text)],
                target: "https://example.invalid/same".into(),
                title: None,
            }],
            align: ir::Align::Start,
            style: None,
        };
        document.blocks = vec![link("one"), link("two")];

        let bytes = write_bytes(&document).expect("render");
        let rels = read_part(&bytes, "word/_rels/document.xml.rels");
        assert_eq!(rels.matches("Target=\"https://example.invalid/same\"").count(), 1);
        // ...and both references must still point at it, not at rId3.
        let xml = read_part(&bytes, "word/document.xml");
        assert_eq!(xml.matches("r:id=\"rId2\"").count(), 2, "got: {xml}");
    }

    /// Characters XML cannot represent must not reach the output at all. The PDF
    /// reader drops them too; this is the writer's own guarantee, because the IR
    /// is also fed by other readers.
    #[test]
    fn unrepresentable_characters_are_dropped() {
        let mut document = ir::Document::new();
        document.blocks = vec![ir::Block::Paragraph {
            content: vec![ir::Inline::text("before\u{2}after\u{FFFF}")],
            align: ir::Align::Start,
            style: None,
        }];

        let bytes = write_bytes(&document).expect("render");
        let xml = read_part(&bytes, "word/document.xml");
        assert!(xml.contains("beforeafter"), "got: {xml}");
        assert!(!xml.contains('\u{2}'), "a control character survived into XML");
    }

    #[test]
    fn text_is_escaped() {
        let mut document = ir::Document::new();
        document.blocks = vec![ir::Block::Paragraph {
            content: vec![ir::Inline::text("a < b & c > d \"quoted\"")],
            align: ir::Align::Start,
            style: None,
        }];

        let bytes = write_bytes(&document).expect("render");
        let xml = read_part(&bytes, "word/document.xml");
        assert!(xml.contains("a &lt; b &amp; c &gt; d \"quoted\""), "got: {xml}");
    }

    #[test]
    fn page_geometry_is_carried_into_the_section_properties() {
        let mut document = ir::Document::new();
        document.page = ir::PageSetup {
            width: ir::Length::mm(210.0),
            height: ir::Length::mm(297.0),
            margin_top: ir::Length::mm(20.0),
            margin_bottom: ir::Length::mm(20.0),
            margin_start: ir::Length::mm(25.0),
            margin_end: ir::Length::mm(15.0),
        };

        let bytes = write_bytes(&document).expect("render");
        let xml = read_part(&bytes, "word/document.xml");

        // A4 in twips, to Word's own canonical values, and the margins as given.
        assert!(xml.contains("<w:pgSz w:w=\"11906\" w:h=\"16838\"/>"), "got: {xml}");
        assert!(xml.contains("w:top=\"1134\""), "got: {xml}");
        assert!(xml.contains("w:left=\"1417\""), "got: {xml}");
        assert!(xml.contains("w:right=\"850\""), "got: {xml}");
    }

    #[test]
    fn writing_replaces_an_existing_file() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("out.docx");
        std::fs::write(&path, b"stale contents").expect("seed the target");

        write_to_path(&sample_document(), &path).expect("write");

        let written = std::fs::read(&path).expect("read back");
        assert_eq!(&written[..2], b"PK", "not a zip archive");
        // No temporary files may be left behind next to the output.
        let leftovers: Vec<_> = std::fs::read_dir(directory.path())
            .expect("list")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n != "out.docx")
            .collect();
        assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
    }
}
