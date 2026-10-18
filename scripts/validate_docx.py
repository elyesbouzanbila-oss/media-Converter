"""Validate a .docx the way a real consumer would, not the way we intended it.

Our own tests assert what we meant to write. This checks what we actually wrote,
against readers that did not grow up alongside our code. Three layers:

  1. Every part is well-formed XML, and the archive is a valid zip.
  2. Every part is reachable from [Content_Types].xml. Word rejects a package
     outright when it is not, with no indication of which part was at fault.
  3. python-docx, an independent implementation, parses it. This is the layer
     that catches style IDs we invented rather than ones Word recognises.

The checks are split deliberately. Structural ones hold for *any* .docx we
produce and always run. Sample-specific ones describe what
`engine/docx/examples/handwritten.rs` puts in the file and run only under
`--sample`, because a validator that fails on a perfectly valid document teaches
you to ignore it — which is exactly what happened when "expected 3 columns" and
"expected a hyperlink" were applied to a real converted file that legitimately
had neither.

Usage:
    python scripts/validate_docx.py out.docx
    python scripts/validate_docx.py target/sample.docx --sample
"""

import sys
import zipfile
from xml.dom import minidom

try:
    import docx

    HAVE_PYTHON_DOCX = True
except ImportError:
    HAVE_PYTHON_DOCX = False

failures = []


def check(condition, message):
    if condition:
        print(f"  ok    {message}")
    else:
        print(f"  FAIL  {message}")
        failures.append(message)


def finish():
    print()
    if failures:
        print(f"FAILED: {len(failures)} check(s)")
        return 1
    print("all checks passed")
    return 0


def structural(document):
    """Checks that hold for any document we produce, whatever its content.

    Rectangularity is here rather than with the sample checks on purpose: Word
    treats a row whose cell count differs from the table's grid as a corrupt
    table, so every table we write has to satisfy it. The *number* of columns is
    a fact about a particular document and is not checked here.
    """
    check(
        bool(document.paragraphs),
        f"python-docx found {len(document.paragraphs)} paragraph(s)",
    )
    print(f"  info  python-docx found {len(document.tables)} table(s)")

    for index, table in enumerate(document.tables):
        columns = len(table.columns)
        widths = {len(row.cells) for row in table.rows}
        check(
            widths == {columns},
            f"table {index} is rectangular ({columns} columns, rows have {sorted(widths)})",
        )

    styles = [p.style.name for p in document.paragraphs]
    heading_styles = [s for s in styles if s.startswith("Heading")]
    print(f"  info  {len(heading_styles)} heading paragraphs ({sorted(set(heading_styles))})")


def sample_expectations(document):
    """What the hand-written sample specifically contains.

    Each of these is evidence about one part of the writer that the sample was
    built to exercise. They are kept apart from the structural checks so that
    converting a real document can be validated without them.
    """
    check(
        len(document.tables) == 1,
        f"the sample has exactly 1 table ({len(document.tables)})",
    )
    if document.tables:
        columns = len(document.tables[0].columns)
        check(columns == 3, f"the sample's table has 3 columns ({columns})")

    # Word's *own* heading styles, not styles we invented with the right names.
    heading_styles = {
        p.style.name for p in document.paragraphs if p.style.name.startswith("Heading")
    }
    check(
        {"Heading 1", "Heading 2", "Heading 3"} <= heading_styles,
        f"the sample uses Word's own heading styles ({sorted(heading_styles)})",
    )

    bold_runs = [
        run.text
        for p in document.paragraphs
        for run in p.runs
        if run.bold and run.text.strip()
    ]
    check(bool(bold_runs), f"bold runs survive ({bold_runs[:4]})")

    # Hyperlinks live in the relationships, not in the run text, so this is
    # evidence the relationship graph is intact rather than merely present.
    rel_targets = [
        rel.target_ref
        for rel in document.part.rels.values()
        if "hyperlink" in rel.reltype
    ]
    check(bool(rel_targets), f"hyperlink relationships present ({rel_targets[:2]})")

    # A marker separated from its item by anything other than a tab reads as one
    # word. The tab is the whole reason a list item's text does not run into its
    # marker.
    marked = [p.text for p in document.paragraphs if "\t" in p.text]
    check(bool(marked), f"list markers are tab-separated ({marked[:2]})")

    text = "\n".join(p.text for p in document.paragraphs)
    check("\u2022" in text, "the bullet character survives as U+2022")
    check("\u00e9" in text, "accented text survives as UTF-8")


def main():
    path = sys.argv[1]
    sample = "--sample" in sys.argv[2:]
    print(f"=== {path}")

    with zipfile.ZipFile(path) as archive:
        bad = archive.testzip()
        check(bad is None, f"archive integrity (first bad entry: {bad})")

        names = [n for n in archive.namelist() if not n.endswith("/")]
        print(f"  parts {len(names)}")

        # Layer 1: well-formedness.
        for name in names:
            if not name.endswith(".xml") and not name.endswith(".rels"):
                continue
            try:
                minidom.parseString(archive.read(name))
            except Exception as error:  # noqa: BLE001 - reporting, not handling
                check(False, f"{name} is well-formed XML ({error})")
        check(True, "all XML parts parse")

        # Layer 2: content-type coverage, both directions.
        types = archive.read("[Content_Types].xml").decode("utf-8")
        for name in names:
            overridden = f'PartName="/{name}"' in types
            extension = name.rsplit(".", 1)[-1]
            defaulted = f'Extension="{extension}"' in types
            if not (overridden or defaulted):
                check(False, f"{name} has no content type")
        check(True, "every part is covered by [Content_Types].xml")

        for chunk in types.split('PartName="/')[1:]:
            part = chunk.split('"')[0]
            if part not in names:
                check(False, f"content types name /{part}, which is not in the archive")
        check(True, "no content type override points at a missing part")

    # Layer 3: an independent implementation.
    if not HAVE_PYTHON_DOCX:
        print("  skip  python-docx not installed; layer 3 not run")
        return finish()

    document = docx.Document(path)
    structural(document)
    if sample:
        sample_expectations(document)

    print()
    print("  first paragraphs as python-docx sees them:")
    for p in document.paragraphs[:10]:
        text = p.text if len(p.text) <= 58 else p.text[:55] + "..."
        print(f"    {p.style.name:<12} {text!r}")

    return finish()


if __name__ == "__main__":
    sys.exit(main())
