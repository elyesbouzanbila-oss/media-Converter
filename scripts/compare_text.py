"""Diff our PDF text reconstruction against MuPDF, as an independent reference.

Text quality is not something to assess by eyeballing a few pages. Two
implementations disagreeing is normal and expected; what carries information is
the magnitude and the *shape* of the disagreement. A word error rate near zero
means the geometry work is sound. A handful of differences all clustered on one
pattern means one specific bug.

MuPDF (via PyMuPDF) is used as the reference rather than PDFium deliberately:
PDFium is the extractor under test here, so sharing an engine would hide
engine-specific mistakes behind a shared opinion.

Usage:
    python scripts/compare_text.py <spike-binary> <file.pdf>
    python scripts/compare_text.py <spike-binary> <file.pdf> \
        --spike-arg "--space-em 0.25"

The codebase names this a word error rate loosely: it counts substituted,
deleted and inserted tokens, which is the useful quantity even though it is not
a rigorous WER.
"""

import argparse
import difflib
import subprocess
import unicodedata

import pymupdf


def normalise(text):
    """Fold the differences that are not ours to get right.

    Compatibility forms turn ligatures into their component letters. Non-breaking
    and narrow spaces become ordinary spaces, because whether a producer used
    U+00A0 and whether we preserved that distinction is a separate question from
    whether words are separated at all.
    """
    text = unicodedata.normalize("NFKC", text)
    for space in ("\u00a0", "\u202f", "\u2009", "\u2007", "\u2004"):
        text = text.replace(space, " ")
    return text


def tokens(text):
    return normalise(text).split()


def ours(binary, pdf, extra):
    result = subprocess.run(
        [binary, pdf, "--text", *extra],
        capture_output=True,
        text=True,
        encoding="utf-8",
        check=True,
    )
    return tokens(result.stdout)


def reference(pdf):
    doc = pymupdf.open(pdf)
    try:
        return tokens(" ".join(doc[i].get_text() for i in range(doc.page_count)))
    finally:
        doc.close()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("binary", help="path to spike-pdf-extract")
    parser.add_argument("pdf")
    parser.add_argument("--show", type=int, default=12, help="differences to print")
    parser.add_argument(
        "--spike-arg",
        action="append",
        default=[],
        help='extra args for the spike, quoted: --spike-arg "--space-em 0.25"',
    )
    args = parser.parse_args()

    extra = [part for group in args.spike_arg for part in group.split()]

    a = ours(args.binary, args.pdf, extra)
    b = reference(args.pdf)

    matcher = difflib.SequenceMatcher(a=a, b=b, autojunk=False)

    print(f"ours       {len(a):>7} tokens")
    print(f"reference  {len(b):>7} tokens")
    print()

    errors = 0
    shown = 0
    for op, i1, i2, j1, j2 in matcher.get_opcodes():
        if op == "equal":
            continue

        errors += max(i2 - i1, j2 - j1)

        if shown < args.show:
            context_a = " ".join(a[max(0, i1 - 5):i2 + 5])
            context_b = " ".join(b[max(0, j1 - 5):j2 + 5])
            print(f"  {op:8}  ours: {context_a!r}")
            print(f"  {'':8}  ref : {context_b!r}")
            shown += 1

    if shown < args.show and errors == 0:
        print("  no differences")

    wer = errors / max(1, len(b))
    print()
    print(f"word error rate  {wer:>7.2%}   ({errors} of {len(b)} reference tokens)")
    print(f"similarity       {matcher.ratio():>7.2%}")

    # Distinguish words that are misplaced from words that are lost or invented.
    # The two demand completely different fixes — reading order versus
    # extraction — so a single similarity number hides the actionable part.
    from collections import Counter

    counts_ours = Counter(a)
    counts_ref = Counter(b)
    missing = counts_ref - counts_ours
    extra = counts_ours - counts_ref

    print()
    print(f"missing tokens   {sum(missing.values()):>7}  ({len(missing)} distinct)")
    print(f"extra tokens     {sum(extra.values()):>7}  ({len(extra)} distinct)")

    for label, bucket in (("missing", missing), ("extra", extra)):
        for token, count in list(bucket.items())[:8]:
            print(f"    {label} x{count:<3} {token!r}")


if __name__ == "__main__":
    main()
