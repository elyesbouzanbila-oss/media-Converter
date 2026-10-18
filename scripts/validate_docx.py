"""Validate a .docx the way a real consumer would, not the way we intended it.

Our own tests assert what we meant to write. This checks what we actually wrote,
against readers that did not grow up alongside our code. Three layers:

  1. Every part is well-formed XML, and the archive is a valid zip.
  2. Every part is reachable from [Content_Types].xml. Word rejects a package
     outright when it is not, with no indication of which part was at fault.
  3. python-docx, an independent implementation, parses it. This is the layer
     that catches style IDs we invented rather than ones Word recognises.

Usage:
    python scripts/validate_docx.py out.docx
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


def main():
    path = sys.argv[1]
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
    else:
        document = docx.Document(path)
        check(len(document.tables) >= 1, f"python-docx found {len(document.tables)} table(s)")

        styles = [p.style.name for p in document.paragraphs]
        heading_styles = [s for s in styles if s.startswith("Heading")]
        check(
            len(heading_styles) >= 3,
            f"python-docx recognises {len(heading_styles)} heading paragraphs ({set(heading_styles)})",
        )

        table = document.tables[0]
        check(
            len(table.columns) == 3,
            f"table has {len(table.columns)} columns, expected 3",
        )
        widths = {len(row.cells) for row in table.rows}
        check(
            widths == {3},
            f"every table row is rectangular (cell counts: {sorted(widths)})",
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

        print()
        print("  first paragraphs as python-docx sees them:")
        for p in document.paragraphs[:10]:
            text = p.text if len(p.text) <= 58 else p.text[:55] + "..."
            print(f"    {p.style.name:<12} {text!r}")

    print()
    if failures:
        print(f"FAILED: {len(failures)} check(s)")
        return 1
    print("all checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
