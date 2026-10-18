"""Shared text-comparison primitives.

The comparison *is* the measurement, so it lives in exactly one place. Two
harnesses with their own copy of the normalisation rules would eventually
disagree, and a fidelity number that changes without a code change is worse than
no number at all.

Two signals matter and they are not the same signal:

* The word error rate is the headline, but it is *order-sensitive*, and its
  reference (MuPDF's own block order) is not guaranteed to be reading order. So
  part of the error rate is the reference's fault.
* The missing/extra token multisets are order-*independent*, and therefore the
  honest measure of extraction: a word that appears nowhere is lost, a word that
  appears twice was invented. Those two demand completely different fixes from a
  reading-order problem, which is why they are reported separately.
"""

from collections import Counter
import difflib
import unicodedata


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


def report(a, b, show=12, label_a="ours", label_b="reference"):
    """Print the comparison and return the word error rate."""
    matcher = difflib.SequenceMatcher(a=a, b=b, autojunk=False)

    print(f"{label_a:<10} {len(a):>7} tokens")
    print(f"{label_b:<10} {len(b):>7} tokens")
    print()

    errors = 0
    shown = 0
    for op, i1, i2, j1, j2 in matcher.get_opcodes():
        if op == "equal":
            continue

        errors += max(i2 - i1, j2 - j1)

        if shown < show:
            context_a = " ".join(a[max(0, i1 - 5):i2 + 5])
            context_b = " ".join(b[max(0, j1 - 5):j2 + 5])
            print(f"  {op:8}  {label_a}: {context_a!r}")
            print(f"  {'':8}  {label_b}: {context_b!r}")
            shown += 1

    if errors == 0:
        print("  no differences")

    wer = errors / max(1, len(b))
    print()
    print(f"word error rate  {wer:>7.2%}   ({errors} of {len(b)} {label_b} tokens)")
    print(f"similarity       {matcher.ratio():>7.2%}")

    missing = Counter(b) - Counter(a)
    extra = Counter(a) - Counter(b)

    print()
    print(f"missing tokens   {sum(missing.values()):>7}  ({len(missing)} distinct)")
    print(f"extra tokens     {sum(extra.values()):>7}  ({len(extra)} distinct)")

    for label, bucket in (("missing", missing), ("extra", extra)):
        for token, count in list(bucket.items())[:8]:
            print(f"    {label} x{count:<3} {token!r}")

    return wer
