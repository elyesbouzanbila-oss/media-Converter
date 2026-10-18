"""Diff a converted .docx against MuPDF's reading of the PDF it came from.

This is the end-to-end fidelity measure, and it answers a different question from
`compare_text.py`. That one asks whether our *extractor* agrees with an
independent extractor. This one asks whether the document a user actually opens
contains the right words — so the text is read back out of Word's own model via
python-docx, not out of our code. A bug in the DOCX writer is invisible to the
extractor comparison and shows up only here.

Text is collected by walking every `w:t` element under each paragraph in
document order, rather than through `Paragraph.text`. That is deliberate: runs
inside a `w:hyperlink` are nested one level deeper than ordinary runs, and
whether a given python-docx version includes them in `.text` has changed between
releases. Walking the XML reads what Word reads, and cannot silently start
skipping hyperlinked words.

Usage:
    python scripts/compare_docx.py out/report.docx tests/fixtures/private/report.pdf
"""

import argparse
import sys

import docx
import pymupdf

from textdiff import report, tokens

WORD = "{http://schemas.openxmlformats.org/wordprocessingml/2006/main}"


def paragraph_text(element):
    """Every text node under a paragraph, in document order.

    A `w:tab` or `w:br` is a word separator in Word's model but carries no text
    node, so it becomes a space here. Without that, a list marker emitted as
    `<w:t>•</w:t><w:tab/>` runs into its own item text and reads as `•Tester` —
    a measurement error that looks exactly like a missing space in the
    conversion, which is the kind of false positive this harness must not
    produce.
    """
    parts = []
    for node in element.iter():
        if node.tag == f"{WORD}t":
            parts.append(node.text or "")
        elif node.tag in (f"{WORD}tab", f"{WORD}br", f"{WORD}cr"):
            parts.append(" ")
    return "".join(parts)


def body_in_order(document):
    """Yield each body item's text, in the order a reader would meet it.

    Tables are walked row by row because reading a table through
    `document.tables` would move it to the end of the document, which is exactly
    the kind of silent reordering this harness exists to detect.
    """
    for child in document.element.body.iterchildren():
        if child.tag == f"{WORD}p":
            yield paragraph_text(child)
        elif child.tag == f"{WORD}tbl":
            for row in child.iter(f"{WORD}tr"):
                for cell in row.iter(f"{WORD}tc"):
                    yield paragraph_text(cell)


def docx_text(path):
    document = docx.Document(path)
    return " ".join(part for part in body_in_order(document) if part.strip())


def pdf_text(path):
    document = pymupdf.open(path)
    try:
        return " ".join(document[i].get_text() for i in range(document.page_count))
    finally:
        document.close()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("docx")
    parser.add_argument("pdf")
    parser.add_argument("--show", type=int, default=12, help="differences to print")
    args = parser.parse_args()

    ours = tokens(docx_text(args.docx))
    reference = tokens(pdf_text(args.pdf))

    print(f"=== {args.docx}")
    print(f"    against {args.pdf}")
    print()

    return 0 if report(ours, reference, show=args.show) < 0.02 else 1


if __name__ == "__main__":
    sys.exit(main())
