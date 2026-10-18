//! The static XML parts of a `.docx` package.
//!
//! A `.docx` is an OPC (Open Packaging Conventions) container: a zip of XML
//! parts with a content-type map and relationship graphs. Word is strict about
//! this. A package is *rejected* rather than merely rendered oddly if the
//! content types do not cover every part, or if any relationship points at a
//! part that does not exist.
//!
//! So the part list written by [`crate::write_bytes`] and the relationship list
//! here have to stay in step by hand. That coupling is the one genuinely fragile
//! thing about assembling OOXML directly rather than through a document
//! library, and it is why the static parts live together in one file instead of
//! being scattered across the writer.

use dconv_ir as ir;

pub const XML_DECL: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n";

pub const CT_NS: &str = "http://schemas.openxmlformats.org/package/2006/content-types";
pub const REL_NS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
pub const W_NS: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";

/// Part names, in one place, so the writer and the relationship graph cannot
/// drift apart silently.
pub mod part {
    pub const DOCUMENT: &str = "word/document.xml";
    pub const STYLES: &str = "word/styles.xml";
    pub const DOCUMENT_RELS: &str = "word/_rels/document.xml.rels";
    pub const PACKAGE_RELS: &str = "_rels/.rels";
    pub const CONTENT_TYPES: &str = "[Content_Types].xml";
    pub const CORE_PROPS: &str = "docProps/core.xml";
    pub const APP_PROPS: &str = "docProps/app.xml";
}

/// `[Content_Types].xml`.
///
/// Both `rels` and `xml` need a `Default`, because parts are keyed by extension
/// before any `Override` is consulted. `word/document.xml` is the one override
/// that is genuinely mandatory: without it the file is not recognised as a Word
/// document at all.
pub fn content_types() -> String {
    format!(
        "{XML_DECL}\
<Types xmlns=\"{CT_NS}\">\
<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\
<Default Extension=\"xml\" ContentType=\"application/xml\"/>\
<Override PartName=\"/{doc}\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/>\
<Override PartName=\"/{styles}\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml\"/>\
<Override PartName=\"/{core}\" ContentType=\"application/vnd.openxmlformats-package.core-properties+xml\"/>\
<Override PartName=\"/{app}\" ContentType=\"application/vnd.openxmlformats-officedocument.extended-properties+xml\"/>\
</Types>",
        doc = part::DOCUMENT,
        styles = part::STYLES,
        core = part::CORE_PROPS,
        app = part::APP_PROPS,
    )
}

/// `_rels/.rels` — the package-level relationships. This is how Word finds the
/// main document part from the container root.
pub fn package_rels() -> String {
    format!(
        "{XML_DECL}\
<Relationships xmlns=\"{REL_NS}\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"{doc}\"/>\
<Relationship Id=\"rId2\" Type=\"http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties\" Target=\"{core}\"/>\
<Relationship Id=\"rId3\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/extended-properties\" Target=\"{app}\"/>\
</Relationships>",
        doc = part::DOCUMENT,
        core = part::CORE_PROPS,
        app = part::APP_PROPS,
    )
}

/// `word/_rels/document.xml.rels`.
///
/// `rId1` is reserved for the styles part; hyperlinks are numbered from `rId2`.
/// [`crate::RelSink`] owns that numbering, so this function must not invent IDs
/// of its own.
pub fn document_rels(hyperlinks: &[String]) -> String {
    let mut out = String::with_capacity(512);
    out.push_str(XML_DECL);
    out.push_str(&format!("<Relationships xmlns=\"{REL_NS}\">"));
    out.push_str(&format!(
        "<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles\" Target=\"styles.xml\"/>"
    ));
    for (index, target) in hyperlinks.iter().enumerate() {
        out.push_str(&format!(
            "<Relationship Id=\"rId{}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink\" Target=\"{}\" TargetMode=\"External\"/>",
            index + 2,
            escape_attr(target)
        ));
    }
    out.push_str("</Relationships>");
    out
}

/// `word/styles.xml`.
///
/// Only the styles we actually reference are defined. Heading styles use the
/// exact `styleId` and lowercase `name` that Word expects for its built-in
/// headings, which is what makes the output navigable in Word's outline pane
/// and re-styleable by the user. Getting the names wrong here would still
/// *render* fine while quietly breaking the one thing a converter most needs to
/// preserve.
pub fn styles() -> String {
    // (level, half-point size, bold, italic, space before in twentieths of a point)
    const HEADINGS: [(u8, u32, bool, bool, u32); 6] = [
        (1, 36, true, false, 360),
        (2, 30, true, false, 320),
        (3, 26, true, false, 280),
        (4, 24, true, true, 240),
        (5, 22, true, false, 240),
        (6, 22, false, true, 240),
    ];

    let mut out = String::with_capacity(4096);
    out.push_str(XML_DECL);
    out.push_str(&format!("<w:styles xmlns:w=\"{W_NS}\">"));

    // Document defaults: what an unstyled run inherits. Calibri 11pt is Word's
    // own default, so a document we produce opens looking like a document Word
    // would have produced.
    out.push_str(
        "<w:docDefaults>\
<w:rPrDefault><w:rPr>\
<w:rFonts w:ascii=\"Calibri\" w:hAnsi=\"Calibri\" w:cs=\"Calibri\"/>\
<w:sz w:val=\"22\"/><w:szCs w:val=\"22\"/>\
</w:rPr></w:rPrDefault>\
<w:pPrDefault><w:pPr><w:spacing w:after=\"120\" w:line=\"240\" w:lineRule=\"auto\"/></w:pPr></w:pPrDefault>\
</w:docDefaults>",
    );

    out.push_str(
        "<w:style w:type=\"paragraph\" w:default=\"1\" w:styleId=\"Normal\">\
<w:name w:val=\"Normal\"/><w:qFormat/>\
</w:style>",
    );

    for (level, half_points, bold, italic, space_before) in HEADINGS {
        out.push_str(&format!(
            "<w:style w:type=\"paragraph\" w:styleId=\"Heading{level}\">\
<w:name w:val=\"heading {level}\"/>\
<w:basedOn w:val=\"Normal\"/><w:next w:val=\"Normal\"/><w:qFormat/>\
<w:pPr><w:keepNext/><w:spacing w:before=\"{space_before}\" w:after=\"80\"/><w:outlineLvl w:val=\"{outline}\"/></w:pPr>\
<w:rPr>{bold}{italic}<w:sz w:val=\"{half_points}\"/></w:rPr>\
</w:style>",
            outline = level - 1,
            bold = if bold { "<w:b/>" } else { "" },
            italic = if italic { "<w:i/>" } else { "" },
        ));
    }

    out.push_str("</w:styles>");
    out
}

/// `docProps/core.xml` — the Dublin Core metadata Word shows in the document
/// panel. Absent fields are omitted rather than written empty.
pub fn core_properties(doc: &ir::Document) -> String {
    let m = &doc.metadata;
    let mut body = String::new();

    let mut field = |tag: &str, value: &Option<String>| {
        if let Some(v) = value {
            if !v.trim().is_empty() {
                body.push_str(&format!("<{tag}>"));
                escape_into(v, &mut body);
                body.push_str(&format!("</{tag}>"));
            }
        }
    };

    field("dc:title", &m.title);
    field("dc:creator", &m.author);
    field("dc:subject", &m.subject);
    field("dc:description", &m.description);

    if !m.keywords.is_empty() {
        body.push_str("<cp:keywords>");
        escape_into(&m.keywords.join(", "), &mut body);
        body.push_str("</cp:keywords>");
    }

    if let Some(created) = &m.created {
        body.push_str(&format!(
            "<dcterms:created xsi:type=\"dcterms:W3CDTF\">{}</dcterms:created>",
            escape_text(created)
        ));
    }

    format!(
        "{XML_DECL}\
<cp:coreProperties \
xmlns:cp=\"http://schemas.openxmlformats.org/package/2006/metadata/core-properties\" \
xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
xmlns:dcterms=\"http://purl.org/dc/terms/\" \
xmlns:dcmitype=\"http://purl.org/dc/dcmitype/\" \
xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\">\
{body}\
</cp:coreProperties>"
    )
}

pub fn app_properties() -> String {
    format!(
        "{XML_DECL}\
<Properties xmlns=\"http://schemas.openxmlformats.org/officeDocument/2006/extended-properties\">\
<Application>docconvert</Application>\
</Properties>"
    )
}

// ---------------------------------------------------------------------------
// Escaping
// ---------------------------------------------------------------------------

/// True for code points that cannot appear in XML 1.0 at all, not even escaped.
///
/// The C0 range minus tab, newline and carriage return is simply not
/// representable. Dropping these is the only option, and it matters: the PDF
/// reader has already had to drop them once (finding D3), so this is a
/// deliberate second line of defence rather than duplicated work.
fn is_illegal_xml(c: char) -> bool {
    matches!(c, '\u{0}'..='\u{8}' | '\u{B}' | '\u{C}' | '\u{E}'..='\u{1F}' | '\u{FFFE}' | '\u{FFFF}')
}

/// Escape text content and drop code points XML cannot represent.
pub fn escape_into(text: &str, out: &mut String) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c if is_illegal_xml(c) => {}
            c => out.push(c),
        }
    }
}

pub fn escape_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    escape_into(text, &mut out);
    out
}

/// Escape a value destined for a double-quoted attribute. Both quote characters
/// must go: an apostrophe inside `w:val="..."` is harmless in principle, but
/// producers round-trip these attributes inconsistently.
pub fn escape_attr(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c if is_illegal_xml(c) => {}
            c => out.push(c),
        }
    }
    out
}
