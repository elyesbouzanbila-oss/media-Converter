//! The document intermediate representation (IR).
//!
//! Every *reader* (PDF, DOCX, PPTX, XLSX, ODT, HTML, Markdown, …) parses its
//! input into this one structure, and every *writer* renders this structure out
//! to its target format. That keeps the conversion matrix at `2N` components
//! instead of `N x M`.
//!
//! # Design rules
//!
//! 1. **Format-agnostic.** Nothing here may reference PDF, OOXML, ODF or any
//!    other concrete format. If a reader needs to carry format-specific data
//!    through, it belongs in that reader's own crate, not here.
//! 2. **Lossy where formats disagree.** IR is the intersection of what the
//!    supported formats can express, plus a small escape hatch
//!    ([`Block::CodeBlock`], [`Block::Rule`]). We prefer dropping decoration to
//!    inventing structure no writer can honour.
//! 3. **Absolute geometry only.** All [`Length`] values are absolute
//!    typographic points. Relative units are a presentation concern and are
//!    resolved by the reader that understands them.
//!
//! # Nesting
//!
//! [`Block`] is recursive: list items, table cells and block quotes contain
//! `Vec<Block>`. Writers that cannot express the nesting (Markdown table cells,
//! for instance) are expected to flatten, not to fail.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Primitive geometry and styling values
// ---------------------------------------------------------------------------

/// An absolute length, in typographic points (1pt = 1/72 inch).
///
/// Readers convert whatever unit their format uses into points on the way in;
/// writers convert out of points on the way out.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct Length(pub f64);

impl Length {
    pub const ZERO: Length = Length(0.0);

    #[inline]
    pub const fn pt(v: f64) -> Self {
        Self(v)
    }
    #[inline]
    pub const fn inch(v: f64) -> Self {
        Self(v * 72.0)
    }
    #[inline]
    pub const fn mm(v: f64) -> Self {
        Self(v * 72.0 / 25.4)
    }
    /// CSS pixels at the conventional 96dpi.
    #[inline]
    pub const fn px(v: f64) -> Self {
        Self(v * 72.0 / 96.0)
    }

    #[inline]
    pub const fn as_pt(self) -> f64 {
        self.0
    }
    #[inline]
    pub const fn as_inch(self) -> f64 {
        self.0 / 72.0
    }
    #[inline]
    pub const fn as_mm(self) -> f64 {
        self.0 * 25.4 / 72.0
    }
    /// English Metric Units, the integer unit used throughout OOXML.
    #[inline]
    pub const fn as_emu(self) -> i64 {
        (self.0 * 12700.0) as i64
    }
    /// Twips, the unit used by the legacy Word binary formats.
    #[inline]
    pub const fn as_twips(self) -> i32 {
        (self.0 * 20.0) as i32
    }
}

impl Default for Length {
    fn default() -> Self {
        Self::ZERO
    }
}

/// A non-premultiplied 8-bit RGBA colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const BLACK: Color = Color::rgb(0, 0, 0);
    pub const WHITE: Color = Color::rgb(255, 255, 255);

    #[inline]
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }
    #[inline]
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }
    /// Parse `#rrggbb` / `#rgb` / `#rrggbbaa`. Returns `None` on malformed input.
    pub fn from_hex(s: &str) -> Option<Self> {
        let s = s.strip_prefix('#')?;
        let nib = |i: usize| -> Option<u8> {
            u8::from_str_radix(s.get(i..i + 1)?, 16).ok()
        };
        match s.len() {
            3 => Some(Self::rgb(
                nib(0)? * 17,
                nib(1)? * 17,
                nib(2)? * 17,
            )),
            6 => Some(Self::rgb(
                nib(0)? * 16 + nib(1)?,
                nib(2)? * 16 + nib(3)?,
                nib(4)? * 16 + nib(5)?,
            )),
            8 => Some(Self::rgba(
                nib(0)? * 16 + nib(1)?,
                nib(2)? * 16 + nib(3)?,
                nib(4)? * 16 + nib(5)?,
                nib(6)? * 16 + nib(7)?,
            )),
            _ => None,
        }
    }
}

/// Horizontal text alignment. Named `Start`/`End` rather than left/right so the
/// IR stays direction-neutral; writers map these to whatever their format calls
/// them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Align {
    #[default]
    Start,
    Center,
    End,
    Justify,
}

/// Vertical alignment, used inside table cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VerticalAlign {
    #[default]
    Top,
    Middle,
    Bottom,
}

// ---------------------------------------------------------------------------
// Inline content
// ---------------------------------------------------------------------------

/// Character-level formatting applied to a run of text.
///
/// Every field is optional or boolean, and `None`/`false` means "inherit from
/// the surrounding context". Writers translate that to their own notion of
/// inherited style (a Word `rPr`, a CSS class, …).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct TextStyle {
    #[serde(default, skip_serializing_if = "is_false")]
    pub bold: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub italic: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub underline: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub strike: bool,
    /// Monospaced presentation hint (inline code, `<tt>`, …).
    #[serde(default, skip_serializing_if = "is_false")]
    pub code: bool,
    /// Small-caps presentation hint.
    #[serde(default, skip_serializing_if = "is_false")]
    pub small_caps: bool,
    pub vertical: VerticalShift,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<Length>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<Color>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub highlight: Option<Color>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Superscript / subscript. Raised or lowered relative to the baseline by the
/// writer's own convention, since IR carries no baseline offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VerticalShift {
    #[default]
    None,
    Superscript,
    Subscript,
}

/// A contiguous span of characters sharing one [`TextStyle`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextRun {
    pub text: String,
    #[serde(default, skip_serializing_if = "TextStyle::is_default")]
    pub style: TextStyle,
}

impl TextStyle {
    /// True when this style carries no information, so it can be omitted from
    /// serialised IR and collapse into the default run style.
    pub fn is_default(&self) -> bool {
        *self == TextStyle::default()
    }
}

impl TextRun {
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: TextStyle::default(),
        }
    }

    pub fn styled(text: impl Into<String>, style: TextStyle) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }
}

/// Inline-level content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Inline {
    /// The common case: text with optional character formatting.
    Text(TextRun),
    /// A hyperlink wrapping further inline content.
    Link {
        content: Vec<Inline>,
        target: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    /// A hard line break inside the same block.
    LineBreak,
    /// A non-breaking space, kept distinct so writers emit `&nbsp;`/`U+00A0`
    /// rather than collapsing it into ordinary whitespace.
    NonBreakingSpace,
    /// Math, retained as source text (LaTeX-ish) because no target format in
    /// scope shares a native formula encoding.
    Math { source: String },
    /// A footnote/endnote marker. The note body lives in
    /// [`Document::footnotes`].
    FootnoteRef { label: String },
}

impl Inline {
    pub fn text(s: impl Into<String>) -> Self {
        Inline::Text(TextRun::plain(s))
    }

    /// Append the plain-text form of this inline to `out`.
    pub fn write_plain_text(&self, out: &mut String) {
        match self {
            Inline::Text(run) => out.push_str(&run.text),
            Inline::Link { content, .. } => {
                for c in content {
                    c.write_plain_text(out);
                }
            }
            Inline::LineBreak => out.push('\n'),
            Inline::NonBreakingSpace => out.push('\u{00A0}'),
            Inline::Math { source } => out.push_str(source),
            Inline::FootnoteRef { label } => {
                out.push('[');
                out.push_str(label);
                out.push(']');
            }
        }
    }

    /// True for content that carries no visible text, and can therefore be
    /// dropped when a writer has nothing to render it into.
    pub fn is_empty(&self) -> bool {
        match self {
            Inline::Text(run) => run.text.is_empty(),
            Inline::Link { content, .. } => content.iter().all(Inline::is_empty),
            Inline::LineBreak | Inline::NonBreakingSpace => false,
            Inline::Math { source } => source.is_empty(),
            Inline::FootnoteRef { .. } => false,
        }
    }
}

/// Flatten inline content to plain text. Used by the plain-text writer, by
/// search/indexing, and heavily by the test suite.
pub fn inlines_to_plain_text(inlines: &[Inline]) -> String {
    let mut s = String::new();
    for i in inlines {
        i.write_plain_text(&mut s);
    }
    s
}

/// Concatenate adjacent runs that share a style.
///
/// Layout analysis produces very fragmented runs (one per glyph cluster), so
/// every reader is expected to call this before handing IR to a writer. Keeping
/// run counts low matters: it is the single biggest factor in DOCX output size
/// and in writer speed.
pub fn merge_adjacent_runs(inlines: &mut Vec<Inline>) {
    let mut out: Vec<Inline> = Vec::with_capacity(inlines.len());
    for inline in inlines.drain(..) {
        match inline {
            Inline::Text(run) => match out.last_mut() {
                Some(Inline::Text(prev)) if prev.style == run.style => {
                    prev.text.push_str(&run.text);
                }
                _ => out.push(Inline::Text(run)),
            },
            Inline::Link {
                content,
                target,
                title,
            } => {
                let mut content = content;
                merge_adjacent_runs(&mut content);
                // Adjacent links to the same target are equally wasteful.
                match out.last_mut() {
                    Some(Inline::Link {
                        content: prev,
                        target: prev_target,
                        ..
                    }) if *prev_target == target => prev.extend(content),
                    _ => out.push(Inline::Link {
                        content,
                        target,
                        title,
                    }),
                }
            }
            other => out.push(other),
        }
    }
    *inlines = out;
}

// ---------------------------------------------------------------------------
// Images
// ---------------------------------------------------------------------------

/// Where an image's bytes come from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "from", rename_all = "snake_case")]
pub enum ImageSource {
    /// Bytes already in memory. The common case after PDF/OOXML extraction,
    /// where the image is a stream inside the container.
    Bytes { data: Vec<u8>, mime: String },
    /// A file on disk, referenced rather than loaded so a passthrough copy
    /// stays cheap.
    Path(PathBuf),
    /// A remote URL, left unresolved. Writers that cannot fetch should emit a
    /// placeholder rather than failing the conversion.
    Url(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Image {
    pub source: ImageSource,
    /// Intrinsic display size. `None` means "use the image's own pixel
    /// dimensions at 96dpi", resolved by the writer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<Length>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<Length>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alt: Option<String>,
}

// ---------------------------------------------------------------------------
// Tables
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TableCell {
    pub blocks: Vec<Block>,
    #[serde(default = "one")]
    pub col_span: u32,
    #[serde(default = "one")]
    pub row_span: u32,
    #[serde(default)]
    pub vertical_align: VerticalAlign,
    /// Set on cells the reader identified as belonging to a header row.
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_header: bool,
}

fn one() -> u32 {
    1
}

impl Default for TableCell {
    fn default() -> Self {
        Self {
            blocks: Vec::new(),
            col_span: 1,
            row_span: 1,
            vertical_align: VerticalAlign::default(),
            is_header: false,
        }
    }
}

impl TableCell {
    pub fn text(s: impl Into<String>) -> Self {
        Self {
            blocks: vec![Block::Paragraph {
                content: vec![Inline::text(s)],
                align: Align::Start,
                style: None,
            }],
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TableRow {
    pub cells: Vec<TableCell>,
}

impl TableRow {
    pub fn new(cells: impl IntoIterator<Item = TableCell>) -> Self {
        Self {
            cells: cells.into_iter().collect(),
        }
    }
}

/// Per-column presentation. A reader that genuinely knows column widths (XLSX,
/// or a PDF with ruling lines) sets `width`; otherwise the writer autofits.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ColumnSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<Length>,
    #[serde(default)]
    pub align: Align,
}

/// Whether the reader found visible cell borders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BorderStyle {
    /// Unknown or mixed; let the writer decide.
    #[default]
    Default,
    None,
    Visible,
}

// ---------------------------------------------------------------------------
// Blocks
// ---------------------------------------------------------------------------

/// An item in an ordered or unordered list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ListItem {
    /// Nesting depth, 0-based. Deeper items refer to the nearest enclosing
    /// list, so a reader emitting nested lists must emit the intermediate
    /// `Block::List` too.
    #[serde(default)]
    pub level: u8,
    /// The item's own body. Frequently a single paragraph; may be more for
    /// loose list items.
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ListBlock {
    #[serde(default)]
    pub ordered: bool,
    /// First number for an ordered list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<u64>,
    /// Marker hint for unordered lists, e.g. `"•"`, `"–"`. Purely
    /// presentational; writers may ignore it and use their own marker for the
    /// nesting depth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marker: Option<String>,
    pub items: Vec<ListItem>,
}

/// Paragraph-level presentation that survives round-tripping.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ParagraphStyle {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align: Option<Align>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indent_start: Option<Length>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indent_end: Option<Length>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_before: Option<Length>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_after: Option<Length>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_height: Option<Length>,
    /// Name of a named style in the source document, carried through when the
    /// target format has an equivalent (e.g. Word's `Heading 1`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub named: Option<String>,
}

/// A block-level element.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    /// A heading of level 1..=6.
    Heading {
        level: u8,
        content: Vec<Inline>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        style: Option<ParagraphStyle>,
    },
    Paragraph {
        content: Vec<Inline>,
        #[serde(default)]
        align: Align,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        style: Option<ParagraphStyle>,
    },
    List(ListBlock),
    Table(TableBlock),
    Image(Image),
    /// Verbatim text. `language` is a hint for syntax highlighting.
    CodeBlock {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        language: Option<String>,
    },
    /// A pull quote / block quote, possibly containing several blocks.
    Quote { blocks: Vec<Block> },
    /// Preformatted acknowledgement, caption, or label attached to the previous
    /// block (figure caption, table caption, …).
    Caption { content: Vec<Inline> },
    /// Horizontal rule / thematic break.
    Rule,
    /// Explicit page break. Writers without pagination (Markdown) may render it
    /// as a rule.
    PageBreak,
}

impl Block {
    /// Plain-text rendering of this block, without recursion into nested
    /// containers beyond one level of flattering.
    pub fn write_plain_text(&self, out: &mut String) {
        match self {
            Block::Heading { content, .. } => {
                for i in content {
                    i.write_plain_text(out);
                }
                out.push('\n');
            }
            Block::Paragraph { content, .. } | Block::Caption { content } => {
                for i in content {
                    i.write_plain_text(out);
                }
                out.push('\n');
            }
            Block::List(list) => {
                for (idx, item) in list.items.iter().enumerate() {
                    let marker = if list.ordered {
                        format!("{}. ", list.start.unwrap_or(1) as usize + idx)
                    } else {
                        "- ".to_string()
                    };
                    out.push_str(&marker);
                    for (i, b) in item.blocks.iter().enumerate() {
                        if i > 0 {
                            out.push_str("   ");
                        }
                        b.write_plain_text(out);
                    }
                }
            }
            Block::Table(table) => {
                for row in &table.rows {
                    let cells: Vec<String> = row
                        .cells
                        .iter()
                        .map(|c| blocks_to_plain_text(&c.blocks))
                        .collect();
                    out.push_str(&cells.join("\t"));
                    out.push('\n');
                }
            }
            Block::Image(_) => out.push_str("[image]\n"),
            Block::CodeBlock { text, .. } => {
                out.push_str(text);
                out.push('\n');
            }
            Block::Quote { blocks } => {
                for b in blocks {
                    b.write_plain_text(out);
                }
            }
            Block::Rule => out.push_str("---\n"),
            Block::PageBreak => out.push('\n'),
        }
    }

    /// Collapse fragmented runs inside this block and, recursively, its
    /// children. Every reader should call this on the way out.
    pub fn merge_runs(&mut self) {
        match self {
            Block::Heading { content, .. } | Block::Paragraph { content, .. } => {
                merge_adjacent_runs(content)
            }
            Block::Caption { content } => merge_adjacent_runs(content),
            Block::List(list) => {
                for item in &mut list.items {
                    merge_runs(&mut item.blocks);
                }
            }
            Block::Table(table) => {
                for row in &mut table.rows {
                    for cell in &mut row.cells {
                        merge_runs(&mut cell.blocks);
                    }
                }
            }
            Block::Quote { blocks } => merge_runs(blocks),
            Block::Image(_) | Block::CodeBlock { .. } | Block::Rule | Block::PageBreak => {}
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TableBlock {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<Vec<Inline>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub columns: Vec<ColumnSpec>,
    pub rows: Vec<TableRow>,
    #[serde(default)]
    pub borders: BorderStyle,
}

impl TableBlock {
    /// True when the table has no rows, or every row is empty. Readers use this
    /// to discard the ghost tables that layout analysis sometimes hallucinates
    /// out of stray whitespace.
    pub fn is_empty(&self) -> bool {
        self.rows
            .iter()
            .all(|r| r.cells.iter().all(|c| c.blocks.is_empty()))
    }

    /// Number of grid columns, accounting for horizontal spans.
    pub fn column_count(&self) -> usize {
        self.rows
            .iter()
            .map(|r| r.cells.iter().map(|c| c.col_span as usize).sum::<usize>())
            .max()
            .unwrap_or(0)
    }

    /// Ragged rows are illegal in most target formats (OOXML rejects them), so
    /// readers pad short rows and writers must tolerate raggedness. This
    /// normalises the table to a rectangle.
    pub fn pad_to_rectangle(&mut self) {
        let width = self.column_count();
        for row in &mut self.rows {
            let mut have: usize = row.cells.iter().map(|c| c.col_span as usize).sum();
            while have < width {
                row.cells.push(TableCell::default());
                have += 1;
            }
        }
    }
}

/// Collapse a block sequence to plain text.
pub fn blocks_to_plain_text(blocks: &[Block]) -> String {
    let mut s = String::new();
    for (idx, b) in blocks.iter().enumerate() {
        if idx > 0 {
            s.push('\n');
        }
        b.write_plain_text(&mut s);
    }
    s.trim_end_matches('\n').to_string()
}

/// Recursively normalise fragmented runs throughout a block sequence.
pub fn merge_runs(blocks: &mut [Block]) {
    for b in blocks.iter_mut() {
        b.merge_runs();
    }
}

// ---------------------------------------------------------------------------
// Page setup and metadata
// ---------------------------------------------------------------------------

/// Page geometry, used by paginating writers (PDF, print-oriented DOCX).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PageSetup {
    pub width: Length,
    pub height: Length,
    pub margin_top: Length,
    pub margin_bottom: Length,
    pub margin_start: Length,
    pub margin_end: Length,
}

impl Default for PageSetup {
    /// A4 portrait with 2cm margins — the most common default worldwide, and
    /// what most PDF producers actually emit.
    fn default() -> Self {
        let m = Length::mm(20.0);
        Self {
            width: Length::mm(210.0),
            height: Length::mm(297.0),
            margin_top: m,
            margin_bottom: m,
            margin_start: m,
            margin_end: m,
        }
    }
}

impl PageSetup {
    /// US Letter, for readers that detect it.
    pub fn letter() -> Self {
        let m = Length::inch(1.0);
        Self {
            width: Length::inch(8.5),
            height: Length::inch(11.0),
            margin_top: m,
            margin_bottom: m,
            margin_start: m,
            margin_end: m,
        }
    }

    /// Content width available between the start and end margins.
    pub fn content_width(&self) -> Length {
        Length(self.width.0 - self.margin_start.0 - self.margin_end.0)
    }

    pub fn content_height(&self) -> Length {
        Length(self.height.0 - self.margin_top.0 - self.margin_bottom.0)
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Metadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// ISO-8601, kept as a string because formats disagree wildly about
    /// precision and timezone and we gain nothing by normalising.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,
}

/// A footnote or endnote body, referenced by [`Inline::FootnoteRef`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Footnote {
    pub label: String,
    pub blocks: Vec<Block>,
}

// ---------------------------------------------------------------------------
// The document
// ---------------------------------------------------------------------------

/// A parsed document, ready for a writer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Document {
    #[serde(default)]
    pub metadata: Metadata,
    #[serde(default)]
    pub page: PageSetup,
    #[serde(default)]
    pub blocks: Vec<Block>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub footnotes: Vec<Footnote>,
}

impl Default for Document {
    fn default() -> Self {
        Self {
            metadata: Metadata::default(),
            page: PageSetup::default(),
            blocks: Vec::new(),
            footnotes: Vec::new(),
        }
    }
}

impl Document {
    pub fn new() -> Self {
        Self::default()
    }

    /// Render the whole document to plain text.
    pub fn plain_text(&self) -> String {
        blocks_to_plain_text(&self.blocks)
    }

    /// Collapse fragmented runs across the whole document. Cheap, and always
    /// worth calling before handing a document to a writer.
    pub fn normalise(&mut self) {
        merge_runs(&mut self.blocks);
        for f in &mut self.footnotes {
            merge_runs(&mut f.blocks);
        }
    }

    /// Count of blocks, recursing into containers. Useful for tests and for the
    /// UI's "produced N elements" summary.
    pub fn block_count(&self) -> usize {
        fn count(blocks: &[Block]) -> usize {
            blocks
                .iter()
                .map(|b| {
                    1 + match b {
                        Block::List(l) => l.items.iter().map(|i| count(&i.blocks)).sum::<usize>(),
                        Block::Table(t) => t
                            .rows
                            .iter()
                            .map(|r| {
                                r.cells
                                    .iter()
                                    .map(|c| count(&c.blocks))
                                    .sum::<usize>()
                            })
                            .sum(),
                        Block::Quote { blocks } => count(blocks),
                        _ => 0,
                    }
                })
                .sum()
        }
        count(&self.blocks)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn length_conversions_round_trip() {
        assert!((Length::inch(1.0).as_pt() - 72.0).abs() < 1e-9);
        assert!((Length::mm(25.4).as_pt() - 72.0).abs() < 1e-9);
        assert!((Length::px(96.0).as_pt() - 72.0).abs() < 1e-9);
        assert_eq!(Length::inch(1.0).as_twips(), 1440);
        assert_eq!(Length::inch(1.0).as_emu(), 914_400);
    }

    #[test]
    fn hex_colour_parsing() {
        assert_eq!(Color::from_hex("#fff"), Some(Color::rgb(255, 255, 255)));
        assert_eq!(Color::from_hex("#000000"), Some(Color::BLACK));
        assert_eq!(Color::from_hex("#ff000080"), Some(Color::rgba(255, 0, 0, 128)));
        assert_eq!(Color::from_hex("ff0000"), None, "missing #");
        assert_eq!(Color::from_hex("#ff00"), None, "bad length");
        assert_eq!(Color::from_hex("#gggggg"), None, "not hex");
    }

    #[test]
    fn merges_adjacent_runs_with_equal_style() {
        let bold = TextStyle {
            bold: true,
            ..Default::default()
        };
        let mut inlines = vec![
            Inline::Text(TextRun::plain("Hello, ")),
            Inline::Text(TextRun::plain("world")),
            Inline::Text(TextRun::styled("!", bold.clone())),
            Inline::Text(TextRun::styled(" Again", bold)),
        ];
        merge_adjacent_runs(&mut inlines);
        assert_eq!(inlines.len(), 2);
        assert_eq!(inlines_to_plain_text(&inlines), "Hello, world! Again");
    }

    #[test]
    fn does_not_merge_across_different_styles() {
        let bold = TextStyle {
            bold: true,
            ..Default::default()
        };
        let mut inlines = vec![
            Inline::Text(TextRun::plain("a")),
            Inline::Text(TextRun::styled("b", bold)),
            Inline::Text(TextRun::plain("c")),
        ];
        merge_adjacent_runs(&mut inlines);
        assert_eq!(inlines.len(), 3);
    }

    #[test]
    fn merges_adjacent_links_to_same_target() {
        let mut inlines = vec![
            Inline::Link {
                content: vec![Inline::text("one")],
                target: "https://example.com".into(),
                title: None,
            },
            Inline::Link {
                content: vec![Inline::text("two")],
                target: "https://example.com".into(),
                title: None,
            },
            Inline::Link {
                content: vec![Inline::text("other")],
                target: "https://other.example".into(),
                title: None,
            },
        ];
        merge_adjacent_runs(&mut inlines);
        assert_eq!(inlines.len(), 2);
    }

    #[test]
    fn plain_text_of_a_table_is_tab_separated() {
        let table = TableBlock {
            caption: None,
            columns: vec![],
            borders: BorderStyle::Visible,
            rows: vec![
                TableRow::new([TableCell::text("Quarter"), TableCell::text("Revenue")]),
                TableRow::new([TableCell::text("Q1"), TableCell::text("120")]),
            ],
        };
        let mut doc = Document::new();
        doc.blocks.push(Block::Table(table));
        assert_eq!(doc.plain_text(), "Quarter\tRevenue\nQ1\t120");
    }

    #[test]
    fn ordered_list_numbering_follows_start() {
        let list = ListBlock {
            ordered: true,
            start: Some(3),
            marker: None,
            items: vec![
                ListItem {
                    level: 0,
                    blocks: vec![Block::Paragraph {
                        content: vec![Inline::text("third")],
                        align: Align::Start,
                        style: None,
                    }],
                },
                ListItem {
                    level: 0,
                    blocks: vec![Block::Paragraph {
                        content: vec![Inline::text("fourth")],
                        align: Align::Start,
                        style: None,
                    }],
                },
            ],
        };
        let mut doc = Document::new();
        doc.blocks.push(Block::List(list));
        assert_eq!(doc.plain_text(), "3. third\n4. fourth");
    }

    #[test]
    fn padding_makes_ragged_tables_rectangular() {
        let mut table = TableBlock {
            caption: None,
            columns: vec![],
            borders: BorderStyle::Default,
            rows: vec![
                TableRow::new([
                    TableCell::text("a"),
                    TableCell::text("b"),
                    TableCell::text("c"),
                ]),
                TableRow::new([TableCell::text("d")]),
            ],
        };
        assert_eq!(table.column_count(), 3);
        table.pad_to_rectangle();
        assert_eq!(table.rows[1].cells.len(), 3);
    }

    #[test]
    fn column_count_accounts_for_horizontal_spans() {
        let table = TableBlock {
            caption: None,
            columns: vec![],
            borders: BorderStyle::Default,
            rows: vec![TableRow::new([
                TableCell {
                    blocks: vec![],
                    col_span: 2,
                    ..Default::default()
                },
                TableCell::text("b"),
            ])],
        };
        assert_eq!(table.column_count(), 3);
    }

    #[test]
    fn empty_tables_are_detectable() {
        let table = TableBlock {
            caption: None,
            columns: vec![],
            borders: BorderStyle::None,
            rows: vec![TableRow::new([TableCell::default(), TableCell::default()])],
        };
        assert!(table.is_empty());
    }

    #[test]
    fn block_count_recurses_into_containers() {
        let mut doc = Document::new();
        doc.blocks.push(Block::Paragraph {
            content: vec![Inline::text("a")],
            align: Align::Start,
            style: None,
        });
        doc.blocks.push(Block::Quote {
            blocks: vec![Block::Paragraph {
                content: vec![Inline::text("b")],
                align: Align::Start,
                style: None,
            }],
        });
        // 1 paragraph + 1 quote + 1 nested paragraph
        assert_eq!(doc.block_count(), 3);
    }

    #[test]
    fn normalise_is_idempotent() {
        let mut doc = Document::new();
        doc.blocks.push(Block::Paragraph {
            content: vec![Inline::text("a"), Inline::text("b")],
            align: Align::Start,
            style: None,
        });
        doc.normalise();
        let once = doc.clone();
        doc.normalise();
        assert_eq!(doc, once);
    }

    #[test]
    fn serialises_without_noise_fields() {
        let mut doc = Document::new();
        doc.blocks.push(Block::Paragraph {
            content: vec![Inline::text("hi")],
            align: Align::Start,
            style: None,
        });
        let json = serde_json::to_string(&doc).unwrap();
        // Default-valued style fields must not appear, or golden files become
        // unreadable noise.
        assert!(!json.contains("bold"), "got: {json}");
        assert!(!json.contains("vertical"), "got: {json}");
        assert!(json.contains("\"hi\""), "got: {json}");
    }

    #[test]
    fn a4_content_width_matches_expectation() {
        let setup = PageSetup::default();
        // 210mm - 20mm - 20mm = 170mm
        assert!((setup.content_width().as_mm() - 170.0).abs() < 1e-6);
    }

    #[test]
    fn non_breaking_space_survives_plain_text() {
        let inlines = vec![Inline::text("10"), Inline::NonBreakingSpace, Inline::text("kg")];
        assert_eq!(inlines_to_plain_text(&inlines), "10\u{00A0}kg");
    }

    #[test]
    fn empty_inline_detection() {
        assert!(Inline::text("").is_empty());
        assert!(!Inline::LineBreak.is_empty());
        assert!(
            Inline::Link {
                content: vec![Inline::text("")],
                target: "x".into(),
                title: None,
            }
            .is_empty()
        );
    }
}
