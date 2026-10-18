"""Generate the synthetic test documents under tests/fixtures/synthetic/.

The project's real documents live in `tests/fixtures/private/`, which is
gitignored, so a fresh clone has no end-to-end test input at all: every layout
test passes with no PDF anywhere, and every fidelity number in AGENTS.md depends
on files that are not in the repository. These fixtures exist to close that gap.

They are generated rather than hand-drawn so the geometry is explicit and
reviewable — the whole difficulty of this reader is that a PDF records glyph
positions and not structure, so a fixture is most useful when it states the
positions it is asserting about.

Each document targets one behaviour that has a rule or a finding behind it:

  headings      heading levels from size and spacing (analyze.rs)
  table         a clean two-column region recovered as a real Word table
  table-wrapped a cell that wraps — the known-bad case, locked in as a baseline
  list          bullet and ordered markers, including `1.` `1)` and `(1)`
  script        a superscript, which is why bands re-anchor (finding D7)
  columns       a two-column page, against COLUMN_LAYOUT_HEIGHT_FRACTION
  multipage     explicit page breaks, and the missing/extra token count

The text is French because the real documents are, and accented characters are
therefore load-bearing: they have to survive glyph extraction, the IR, XML
escaping and the zip. A fixture set in ASCII would not notice a regression in
any of those four.

Positions are computed from the font's own metrics via `get_text_length` rather
than estimated by hand. Where two runs are meant to abut, a hand-computed width
that is 1pt short opens a gap larger than WORD_GAP_FRACTION and the reader
correctly inserts a space — which looks like a reader bug in the dump and is
actually a generator bug.

Run:  python scripts/make_fixtures.py
"""

from pathlib import Path

import pymupdf

A4 = pymupdf.paper_rect("a4")
BODY = 11.0
# Not Arial: the base-14 fonts need no embedding, so the fixtures stay a few KB
# and the reader has no subsetted font program to disagree with.
FONT = "helv"

# `pymupdf.get_text_length` must not be used for this: it mis-measures any string
# containing a non-ASCII character, returning 7.74pt for `ème` at 6.96pt where
# the font's own metric is 13.54pt — it appears to drop a character somewhere in
# the decoding. Everything placed after an accented run then lands 5.8pt to the
# left of where this script says it is, which is how the superscript fixture
# first came out reading `3èmeniveau`. `Font.text_length` agrees with the
# per-character metrics and with the advances PDFium reads back, so it is used
# throughout.
_FONT = pymupdf.Font(FONT)

OUT = Path(__file__).resolve().parent.parent / "tests" / "fixtures" / "synthetic"


def width(text: str, size: float) -> float:
    """Advance width of `text`, from the font rather than from arithmetic."""
    return _FONT.text_length(text, fontsize=size)


class Page:
    """A page plus the running cursor used by `para`, in PDF points."""

    def __init__(self, doc: pymupdf.Document):
        self.page = doc.new_page(width=A4.width, height=A4.height)
        self.y = 72.0

    def at(self, x: float, y: float, text: str, size: float = BODY) -> float:
        """One line by its baseline — what the reader groups by.

        Returns the x just past the end of the text, so the next run can be
        placed against it exactly.
        """
        self.page.insert_text((x, y), text, fontsize=size, fontname=FONT)
        return x + width(text, size)

    def para(self, text: str, size: float = BODY, x: float = 72.0, lead: float = 14.0) -> None:
        """A line of body text, advancing the cursor."""
        self.at(x, self.y, text, size)
        self.y += lead

    def gap(self, points: float) -> None:
        self.y += points


def new_document(title: str) -> pymupdf.Document:
    doc = pymupdf.open()
    # Fixed metadata, so regenerating a fixture produces the same file rather
    # than a diff that is entirely a timestamp.
    doc.set_metadata({"title": title, "author": "", "producer": "make_fixtures.py"})
    return doc


def save(doc: pymupdf.Document, name: str) -> None:
    path = OUT / f"{name}.pdf"
    doc.save(path, deflate=True, garbage=4)
    print(f"  {path.relative_to(OUT.parent.parent.parent)}  {path.stat().st_size} bytes")


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


def headings() -> None:
    """Three sizes, well separated vertically.

    The real signal for a heading is not only size: `TP2 Admin BD` has a 12 pt
    note box in an 11 pt document that produced thirteen spurious H3s, because
    size alone cannot tell a heading from emphasis set larger. Genuine headings
    there sit 18-30 pt below the preceding line against a 14 pt body pitch, so
    this fixture spaces them the same way (finding D13).
    """
    doc = new_document("Headings")
    page = Page(doc)
    page.para("Rappel de cours", size=18.0)
    page.gap(24.0)
    page.para("Une transaction est une suite d'opérations qui doit vérifier")
    page.para("quatre propriétés, dites ACID, pour être considérée valide.")
    page.gap(10.0)
    page.para("Atomicité", size=14.0)
    page.gap(20.0)
    page.para("L'atomicité garantit que la transaction s'exécute entièrement")
    page.para("ou pas du tout. Aucun état intermédiaire n'est visible.")
    page.gap(10.0)
    page.para("Propriétés ACID", size=12.5)
    page.gap(18.0)
    page.para("La cohérence assure le passage d'un état valide à un autre.")
    page.para("L'isolation garantit que les transactions concurrentes ne se")
    page.para("perturbent pas mutuellement pendant leur exécution.")
    save(doc, "headings")


def table() -> None:
    """A clean table: two columns, one visual line per cell, five rows.

    One line per cell is the case the region classifier already handles. The
    wrapped variant below is the one it does not.
    """
    doc = new_document("Table")
    page = Page(doc)
    page.para("Propriétés attendues", size=14.0)
    page.gap(26.0)

    col1, col2 = 76.6, 240.0
    rows = [
        ("Propriété", "Définition"),
        ("Atomicité", "Tout ou rien"),
        ("Cohérence", "État valide vers état valide"),
        ("Isolation", "Transactions indépendantes"),
        ("Durabilité", "Effet persistant après validation"),
    ]
    for index, (left, right) in enumerate(rows):
        y = page.y + index * 16.0
        page.at(col1, y, left)
        page.at(col2, y, right)

    page.y += len(rows) * 16.0 + 14.0
    page.para("Le tableau ci-dessus résume les quatre propriétés.")
    save(doc, "table")


def table_wrapped() -> None:
    """The glossary that defeats the current classifier, reduced to one page.

    `TP2 Admin BD 2026_2027.pdf` loses 137 of its 140 differing tokens to this
    shape. Columns separate cleanly and simple rows are recovered; what fails is
    a cell that wraps, because then a visual line is not a row: the band holding
    one cell's tail has fewer segments than its neighbours, so the region stops
    looking like a table and falls back to flow.

    This fixture exists so that the regression is visible the moment the fix
    lands, and so a rewrite of the region classifier can be judged against a
    file that is checked in rather than one that only exists on one machine.
    """
    doc = new_document("TableWrapped")
    page = Page(doc)
    page.para("Glossaire", size=14.0)
    page.gap(26.0)

    col1, col2 = 76.6, 206.7
    # A row whose left cell wraps to two lines and whose right cell wraps to
    # three: the tail-only band carries one segment where its neighbours carry
    # two, which is what breaks the region.
    page.at(col1, page.y, "Connecté en")
    page.at(col1, page.y + 14.0, "session")
    page.at(col2, page.y, "Utilisateur ayant ouvert")
    page.at(col2, page.y + 14.0, "une session sur le")
    page.at(col2, page.y + 28.0, "serveur applicatif.")
    page.y += 42.0

    page.at(col1, page.y, "Droits")
    page.at(col2, page.y, "Permissions accordées au profil.")
    page.y += 16.0

    page.at(col1, page.y, "Profil")
    page.at(col2, page.y, "Ensemble de droits partagés.")
    page.y += 30.0

    page.para("Ces définitions sont indicatives.")
    save(doc, "table-wrapped")


def list_markers() -> None:
    """Three lists, one per marker style the `Numbering` enum can carry.

    The marker is punctuation the source chose, and the IR keeps it rather than
    normalising: a document that writes `(1)` must not come back as `1.`.

    The lists are separated by a paragraph of body text, not just by blank
    space. Two ordered lists set back to back merge into one whatever the gap
    between them, because the classifier only ends a list at a band that is not
    a list item — so a `(1)` list below a `1.` list continues the first list and
    its markers are lost. `ListBlock` carries one `numbering` for the whole
    list, so that is closer to a limitation of the IR's shape than of the
    classifier, and neither real document pays for it: both documents' tokens
    are fully accounted for. A paragraph is what actually separates lists in the
    documents this reader was built for, so that is what this fixture does.
    """
    doc = new_document("Lists")
    page = Page(doc)
    page.para("Étapes à suivre", size=14.0)
    page.gap(22.0)

    page.para("\u2022 Vérifier les prérequis")
    page.para("\u2022 Installer l'application")
    page.para("\u2022 Redémarrer le service")
    page.gap(12.0)

    page.para("1. Ouvrir la session")
    page.para("2. Saisir les identifiants")
    page.para("3. Valider le formulaire")
    page.gap(20.0)

    page.para("Contrôler ensuite les propriétés suivantes :")
    page.gap(20.0)

    page.para("(1) Contrôler la cohérence")
    page.para("(2) Contrôler l'atomicité")
    page.para("(3) Contrôler la durabilité")
    save(doc, "list")


def superscript() -> None:
    """`3ème` with a raised, smaller `ème` — the case that split bands.

    A superscript has a *higher* baseline than the text it annotates, so it is
    seen first when glyphs are offered to bands in baseline-descending order.
    The band must therefore accept a base glyph arriving later and re-anchor
    (findings D7 and D8). Getting this wrong puts `ème` in a band of its own and
    reorders the line.
    """
    doc = new_document("Script")
    page = Page(doc)
    page.para("Notation", size=14.0)
    page.gap(22.0)

    # The geometry of `Groupes : 3ème` in 3IRS_TP2.pdf: a 6.96pt script whose
    # loose box bottom sits 5.69pt above its 11.04pt base's. That offset is the
    # quantity the bander measures, and it is *not* the baseline rise: the
    # smaller glyph has the shallower box, so 0.86pt of the 5.69 is nothing but
    # the difference in descent. Placing the baselines 5.69 apart instead — the
    # obvious misreading, and the one the constant's old comment invited —
    # puts 6.55pt between bottoms, which the 0.9 bound rejects and the script
    # orphans. Hence the subtraction.
    base_size, script_size, descent_em = 11.04, 6.96, 0.2107
    rise = 5.69 - (base_size - script_size) * descent_em

    prefix = "Le 3"
    page.at(72.0, page.y, prefix, base_size)
    # Abutting exactly, as the real `3` and `ème` do (134.41 and 134.42): a gap
    # wider than roughly a third of a space is a word break, and the reader
    # would be right to insert one.
    after_script = page.at(72.0 + width(prefix, base_size), page.y - rise, "ème", script_size)
    # The space is left to the reader's gap rule rather than written as a
    # leading space in the string: PyMuPDF places a string that begins with one
    # about half an em to the left of the x asked for, which opens a spurious
    # gap before it and lands a real space in the middle of `ème`.
    page.at(after_script + width(" ", base_size), page.y, "niveau décrit la deuxième forme normale.", base_size)
    page.y += 14.0

    page.para("Le 2")
    page.para("Chaque forme normale élimine une classe d'anomalie.")
    page.para("La normalisation réduit la redondance des données.")
    save(doc, "script")


def columns() -> None:
    """Two columns of body text with a page-sized gap between them.

    The columns are deliberately *not* on a shared baseline grid. Set on one —
    which is how a typeset page actually looks, and how this fixture first came
    out — the region is indistinguishable from a table: the same number of
    segments per band, at the same two x positions, for six consecutive bands.
    The classifier called it a table and was not wrong to. Two columns and two
    table columns are the same geometry, and nothing in a PDF says which one the
    author meant, so this fixture covers the case that can be told apart and the
    shared-grid case stays the ambiguity it is.

    The expected output is *wrong*, and is pinned as wrong: reading order runs
    across the two columns rather than down each one, so the first paragraph
    interleaves `Le mode écriture verrouille` into the middle of a sentence from
    the left column, and the rows that do align are taken for a table. Multi-
    column reading order is not on the gaps list because no document here has
    needed it; this fixture exists so that the day one does, the change shows up
    as a deliberate diff rather than as a surprise. Do not read its golden file
    as an example of correct output.
    """
    doc = new_document("Columns")
    page = Page(doc)
    page.para("Comparaison", size=14.0)
    page.gap(24.0)

    top = page.y
    left = [
        "Le mode lecture permet",
        "de consulter les données",
        "sans verrouiller les lignes",
        "utilisées par les autres",
        "transactions en cours.",
        "Il améliore le débit.",
    ]
    right = [
        "Le mode écriture verrouille",
        "les lignes concernées jusqu'à",
        "la validation ou l'annulation",
        "de la transaction, ce qui peut",
        "provoquer des attentes.",
    ]
    for index, line in enumerate(left):
        page.at(72.0, top + index * 14.0, line)
    # Offset by half a body line and on a different pitch, so no two baselines
    # in the two columns coincide.
    for index, line in enumerate(right):
        page.at(310.0, top + 7.0 + index * 15.5, line)

    page.y = top + len(left) * 14.0 + 16.0
    page.para("Les deux modes répondent à des besoins différents.")
    save(doc, "columns")


def multipage() -> None:
    """Three pages, to pin the explicit page break between them."""
    doc = new_document("Multipage")
    for number in range(1, 4):
        page = Page(doc)
        page.para(f"Chapitre {number}", size=16.0)
        page.gap(24.0)
        page.para(f"Ceci est le paragraphe d'ouverture du chapitre {number}.")
        page.para("Il contient plusieurs lignes de texte courant, afin que le")
        page.para("lecteur ait de quoi mesurer le corps du document avant de")
        page.para("classer les titres rencontrés plus loin dans la page.")
        page.gap(10.0)
        page.para("Remarque", size=12.5)
        page.gap(18.0)
        page.para("Les sauts de page sont explicites dans l'IR, car une page")
        page.para("Word ne peut pas être déduite de la suite des paragraphes.")
    save(doc, "multipage")


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    print(f"writing to {OUT}")
    for make in (
        headings,
        table,
        table_wrapped,
        list_markers,
        superscript,
        columns,
        multipage,
    ):
        make()


if __name__ == "__main__":
    main()
