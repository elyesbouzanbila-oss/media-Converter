#!/usr/bin/env python3
"""Generate synthetic PDF fixtures for docconvert.

Deliberately dependency-free: writes PDF primitives by hand so it runs on a
bare Python install with no pip. These are *baseline* fixtures only -- the
layout analyser is ultimately tuned against real-world documents, so drop those
into tests/fixtures/private/ (gitignored) and the test harness will pick them up
too.

Covered here:
  single_column.pdf   headings + body paragraphs, 6 pages
  two_column.pdf      two-column body text, 4 pages
  table.pdf           ruled grid tables with aligned cell text, 2 pages
  perf.pdf            100 dense pages, for the throughput benchmark

Note these use the base-14 standard fonts (Helvetica / Times), which are *not*
embedded. That is intentional: it exercises the worst case for style recovery,
where PDFium reports unreliable font weights and we have to fall back to
name-based heuristics. Documents with embedded fonts are easier.

Usage:
    python tests/fixtures/generate_fixtures.py [--out DIR]
"""

from __future__ import annotations

import argparse
import random
from pathlib import Path

# Base-14 font resource names, registered in every page's resource dictionary.
FONTS = {
    "F1": "Helvetica",
    "F2": "Helvetica-Bold",
    "F3": "Times-Roman",
    "F4": "Helvetica-Oblique",
}

A4 = (595.0, 842.0)
MARGIN = 72.0


# --------------------------------------------------------------------------
# Minimal PDF writer
# --------------------------------------------------------------------------


class PdfBuilder:
    """Assembles a single-PDF from raw object bodies."""

    def __init__(self) -> None:
        self._objects: dict[int, bytes] = {}
        self._next = 1
        self._page_refs: list[int] = []
        self._root = 0

    def add(self, body: bytes) -> int:
        num = self._next
        self._next += 1
        self._objects[num] = body
        return num

    def add_page(self, width: float, height: float, content: bytes) -> None:
        content_ref = self.add(
            b"<< /Length %d >>\nstream\n%s\nendstream" % (len(content), content)
        )
        font_entries = " ".join(f"/{n} << /Type /Font /Subtype /Type1 /BaseFont /{f} >>"
                               for n, f in FONTS.items())
        page = (
            f"<< /Type /Page /Parent 0 0 R"
            f" /MediaBox [0 0 {width:.4f} {height:.4f}]"
            f" /Resources << /Font << {font_entries} >> >>"
            f" /Contents {content_ref} 0 R >>"
        ).encode()
        self._page_refs.append(self.add(page))

    def finish(self, replace_parent: bool = True) -> bytes:
        pages_ref = self.add(
            (
                "<< /Type /Pages /Kids ["
                + " ".join(f"{r} 0 R" for r in self._page_refs)
                + f"] /Count {len(self._page_refs)} >>"
            ).encode()
        )
        if replace_parent:
            for r in self._page_refs:
                self._objects[r] = self._objects[r].replace(b"/Parent 0 0 R",
                                                            f"/Parent {pages_ref} 0 R".encode())
        self._root = self.add(f"<< /Type /Catalog /Pages {pages_ref} 0 R >>".encode())
        return self._serialise()

    def _serialise(self) -> bytes:
        out = bytearray(b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n")
        offsets: dict[int, int] = {}
        for num in sorted(self._objects):
            offsets[num] = len(out)
            out += f"{num} 0 obj\n".encode()
            out += self._objects[num]
            out += b"\nendobj\n"

        xref_pos = len(out)
        count = max(self._objects) + 1
        out += f"xref\n0 {count}\n".encode()
        out += b"0000000000 65535 f \n"
        for num in range(1, count):
            if num in offsets:
                out += f"{offsets[num]:010d} 00000 n \n".encode()
            else:
                # Never emitted in practice, but the table must stay dense.
                out += b"0000000000 65535 f \n"
        out += (
            f"trailer\n<< /Size {count} /Root {self._root} 0 R >>\n"
            f"startxref\n{xref_pos}\n%%EOF\n"
        ).encode()
        return bytes(out)


def esc(text: str) -> str:
    """Escape a string for a PDF literal string, and drop non-Latin chars.

    Base-14 fonts with the default encoding are Latin-1 at best, so any
    non-ASCII would render as mojibake and make the fixtures misleading.
    """
    text = text.encode("latin-1", "replace").decode("latin-1")
    return text.replace("\\", r"\\").replace("(", r"\(").replace(")", r"\)")


class Content:
    """Accumulates PDF content-stream operators."""

    def __init__(self) -> None:
        self._ops: list[str] = []

    def text(self, x: float, y: float, s: str, font: str = "F1", size: float = 10.0) -> None:
        self._ops.append(
            f"BT /{font} {size:.2f} Tf 1 0 0 1 {x:.2f} {y:.2f} Tm ({esc(s)}) Tj ET"
        )

    def paragraph(
        self,
        x: float,
        y_top: float,
        lines: list[str],
        font: str = "F1",
        size: float = 10.0,
        leading: float | None = None,
    ) -> float:
        """Write lines of a wrapped paragraph. Returns the y below the block."""
        leading = leading if leading is not None else size * 1.35
        body = [f"BT /{font} {size:.2f} Tf {leading:.2f} TL "
                f"1 0 0 1 {x:.2f} {y_top:.2f} Tm"]
        for i, line in enumerate(lines):
            body.append(f"({esc(line)}) Tj")
            if i != len(lines) - 1:
                body.append("T*")
        body.append("ET")
        self._ops.append(" ".join(body))
        return y_top - leading * (len(lines) - 1)

    def line(self, x1: float, y1: float, x2: float, y2: float, width: float = 0.6) -> None:
        self._ops.append(f"{width:.2f} w {x1:.2f} {y1:.2f} m {x2:.2f} {y2:.2f} l S")

    def rect(self, x: float, y: float, w: float, h: float, width: float = 0.6) -> None:
        self._ops.append(f"{width:.2f} w {x:.2f} {y:.2f} {w:.2f} {h:.2f} re S")

    def build(self) -> bytes:
        return "\n".join(self._ops).encode("latin-1")


# --------------------------------------------------------------------------
# Prose
# --------------------------------------------------------------------------

WORDS = (
    "the quick brown fox jumps over a lazy dog while seventeen engineers debate "
    "whether layout recovery from fixed coordinates is a tractable problem at all "
    "given that glyph positions carry no semantic markers of any kind whatsoever "
    "and typography conventions vary enormously between publishers and decades"
).split()

HEADINGS = [
    "Introduction",
    "Method and Approach",
    "Experimental Results",
    "Discussion of Findings",
    "Limitations",
    "Related Work",
    "Conclusion",
]


def sentences(rng: random.Random, count: int) -> list[str]:
    out = []
    for _ in range(count):
        n = rng.randint(8, 22)
        words = [rng.choice(WORDS) for _ in range(n)]
        words[0] = words[0].capitalize()
        out.append(" ".join(words) + ".")
    return out


def wrap(text: str, width_chars: int) -> list[str]:
    """Greedy wrap, approximating Helvetica 10pt at the given column width."""
    lines, current = [], ""
    for word in text.split():
        candidate = f"{current} {word}".strip()
        if len(candidate) <= width_chars:
            current = candidate
        else:
            if current:
                lines.append(current)
            current = word
    if current:
        lines.append(current)
    return lines


def write_single_column(path: Path, pages: int, seed: int = 1) -> None:
    rng = random.Random(seed)
    b = PdfBuilder()
    for p in range(pages):
        c = Content()
        y = A4[1] - MARGIN
        if p == 0:
            c.text(MARGIN, y, "Recovering Document Structure from PDF", "F2", 20.0)
            y -= 34
            c.text(MARGIN, y, "A Study in Geometric Inference", "F4", 12.0)
            y -= 42
        c.text(MARGIN, y, f"{p + 1}. {HEADINGS[p % len(HEADINGS)]}", "F2", 14.0)
        y -= 26

        # Aim to fill the page, then stop. Each paragraph costs roughly
        # (lines * leading) of vertical space.
        while y > MARGIN + 20:
            para = " ".join(sentences(rng, rng.randint(2, 4)))
            lines = wrap(para, 88)
            if y - len(lines) * 13.5 < MARGIN:
                break
            c.paragraph(MARGIN, y, lines, "F1", 10.0)
            y -= len(lines) * 13.5 + 9

        b.add_page(*A4, c.build())
    path.write_bytes(b.finish())


def write_two_column(path: Path, pages: int, seed: int = 2) -> None:
    rng = random.Random(seed)
    b = PdfBuilder()
    gutter = 24.0
    col_w = (A4[0] - 2 * MARGIN - gutter) / 2
    chars_per_line = int(col_w / 10.0 * 1.75)

    for p in range(pages):
        c = Content()
        y_top = A4[1] - MARGIN
        if p == 0:
            c.text(MARGIN, y_top, "Two Column Layout Sample", "F2", 18.0)
            y_top -= 36
        for col in range(2):
            x = MARGIN + col * (col_w + gutter)
            y = y_top
            if col == 1:
                c.text(x, y, f"{p * 2 + 2}. {HEADINGS[(p + 1) % len(HEADINGS)]}", "F2", 12.0)
                y -= 22
            while y > MARGIN + 20:
                para = " ".join(sentences(rng, 3))
                lines = wrap(para, chars_per_line)
                if y - len(lines) * 12.5 < MARGIN:
                    break
                c.paragraph(x, y, lines, "F1", 9.5)
                y -= len(lines) * 12.5 + 8
        b.add_page(*A4, c.build())
    path.write_bytes(b.finish())


def write_table(path: Path, pages: int, seed: int = 3) -> None:
    rng = random.Random(seed)
    b = PdfBuilder()
    headers = ["Quarter", "Revenue", "Cost", "Margin"]
    col_x = [MARGIN, MARGIN + 120, MARGIN + 220, MARGIN + 300]
    row_h = 22.0

    for p in range(pages):
        c = Content()
        y = A4[1] - MARGIN
        c.text(MARGIN, y, f"Table Fixture {p + 1}", "F2", 16.0)
        y -= 20
        c.text(MARGIN, y, "Consolidated financial summary by period.", "F3", 10.0)
        y -= 30

        # Header occupies its own cell band, like the data rows do. Drawing it
        # at an ad-hoc offset put it on the same baseline as the first data row,
        # which is genuinely ambiguous input for a line grouper.
        rows = 14
        bands = rows + 1
        row_h = 22.0
        table_top = y
        table_bottom = table_top - bands * row_h

        for r in range(bands + 1):
            ry = table_top - r * row_h
            c.line(MARGIN, ry, col_x[-1] + 80, ry)

        for x in col_x + [col_x[-1] + 80]:
            c.line(x, table_top, x, table_bottom)

        # Baseline sits 7pt above each band's lower rule.
        def baseline(band: int) -> float:
            return table_top - (band + 1) * row_h + 7

        for i, h in enumerate(headers):
            c.text(col_x[i] + 5, baseline(0), h, "F2", 10.0)

        for r in range(1, rows + 1):
            y_b = baseline(r)
            c.text(col_x[0] + 5, y_b, f"FY{rng.randint(10, 40)} Q{rng.randint(1, 4)}", "F1", 10.0)
            c.text(col_x[1] + 5, y_b, f"{rng.randint(10, 999)}.{rng.randint(0, 99):02d}", "F1", 10.0)
            c.text(col_x[2] + 5, y_b, f"{rng.randint(10, 400)}.{rng.randint(0, 99):02d}", "F1", 10.0)
            c.text(col_x[3] + 5, y_b, f"{rng.randint(1, 60)}%", "F1", 10.0)

        b.add_page(*A4, c.build())
    path.write_bytes(b.finish())


def write_perf(path: Path, pages: int, seed: int = 4) -> None:
    """Dense single-column text, for the throughput benchmark."""
    rng = random.Random(seed)
    b = PdfBuilder()
    for p in range(pages):
        c = Content()
        y = A4[1] - MARGIN
        c.text(MARGIN, y, f"{p + 1}", "F1", 9.0)
        c.text(A4[0] / 2 - 40, y, f"Chapter {p // 10 + 1}", "F2", 13.0)
        y -= 30
        while y > MARGIN + 20:
            lines = wrap(" ".join(sentences(rng, rng.randint(3, 5))), 92)
            if y - len(lines) * 12.0 < MARGIN:
                break
            c.paragraph(MARGIN, y, lines, "F1", 9.5)
            y -= len(lines) * 12.0 + 7
        b.add_page(*A4, c.build())
    path.write_bytes(b.finish())


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--out", type=Path, default=None,
                    help="output directory (default: tests/fixtures/generated)")
    args = ap.parse_args()

    out = args.out or (Path(__file__).resolve().parent / "generated")
    out.mkdir(parents=True, exist_ok=True)

    specs = [
        ("single_column.pdf", lambda p: write_single_column(p, 6)),
        ("two_column.pdf", lambda p: write_two_column(p, 4)),
        ("table.pdf", lambda p: write_table(p, 2)),
        ("perf.pdf", lambda p: write_perf(p, 100)),
    ]
    for name, fn in specs:
        target = out / name
        fn(target)
        print(f"  {name:<20} {target.stat().st_size:>10,} bytes")
    print(f"\nWrote {len(specs)} fixtures to {out}")


if __name__ == "__main__":
    main()
