//! Build a representative document in memory and write it out.
//!
//! This exists to be inspected by something other than this crate's own tests.
//! Our assertions check what we *intended* to write; a real DOCX reader checks
//! what we actually wrote, and the two disagree in precisely the places that
//! matter — package structure, style IDs, and element ordering.
//!
//!     cargo run -p dconv-docx --profile spike --example handwritten -- out.docx
//!     python scripts/validate_docx.py out.docx

use dconv_ir::*;

fn bold(text: &str) -> Inline {
    Inline::Text(TextRun::styled(
        text,
        TextStyle {
            bold: true,
            ..Default::default()
        },
    ))
}

fn italic(text: &str) -> Inline {
    Inline::Text(TextRun::styled(
        text,
        TextStyle {
            italic: true,
            ..Default::default()
        },
    ))
}

fn paragraph(content: Vec<Inline>) -> Block {
    Block::Paragraph {
        content,
        align: Align::Start,
        style: None,
    }
}

fn heading(level: u8, text: &str) -> Block {
    Block::Heading {
        level,
        content: vec![Inline::text(text)],
        style: None,
    }
}

fn cell(text: &str, header: bool) -> TableCell {
    TableCell {
        blocks: vec![paragraph(vec![Inline::text(text)])],
        is_header: header,
        ..Default::default()
    }
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "target/sample.docx".to_string());

    let mut table = TableBlock {
        caption: None,
        columns: vec![],
        borders: BorderStyle::Visible,
        rows: vec![
            TableRow::new([
                cell("Propriété", true),
                cell("Description", true),
                cell("Niveau", true),
            ]),
            TableRow::new([
                cell("Atomicité", false),
                cell(
                    "La transaction s'exécute entièrement ou pas du tout ; \
                     aucune étape intermédiaire n'est visible.",
                    false,
                ),
                cell("Obligatoire", false),
            ]),
            // Ragged on purpose: the writer has to pad it, because OOXML
            // rejects a table whose rows have differing cell counts.
            TableRow::new([cell("Cohérence", false)]),
        ],
    };
    table.pad_to_rectangle();

    let document = Document {
        page: PageSetup::default(),
        metadata: Metadata {
            title: Some("Validation de la conversion".into()),
            author: Some("docconvert".into()),
            subject: Some("Contrôle de rendu".into()),
            description: Some("Document témoin pour la validation du writer DOCX".into()),
            keywords: vec!["docx".into(), "conversion".into()],
            created: Some("2026-10-18T00:00:00Z".into()),
            language: Some("fr".into()),
        },
        footnotes: Vec::new(),
        blocks: vec![
            heading(1, "Rappel de cours"),
            heading(2, "Définition"),
            paragraph(vec![
                Inline::text("Une transaction est "),
                bold("indivisible"),
                Inline::text(", c'est-à-dire "),
                italic("tout ou rien"),
                Inline::text(". Elle respecte quatre propriétés, souvent résumées par l'acronyme "),
                bold("ACID"),
                Inline::text(" selon la documentation "),
                Inline::Link {
                    content: vec![Inline::text("officielle")],
                    target: "https://example.invalid/docs".into(),
                    title: Some("Documentation".into()),
                },
                Inline::text("."),
            ]),
            heading(3, "Les quatre propriétés"),
            Block::List(ListBlock {
                ordered: false,
                start: None,
                marker: None,
                items: vec![
                    ListItem {
                        level: 0,
                        blocks: vec![paragraph(vec![
                            bold("Atomicité"),
                            Inline::text(" : exécution complète ou nulle."),
                        ])],
                    },
                    ListItem {
                        level: 0,
                        blocks: vec![paragraph(vec![
                            bold("Cohérence"),
                            Inline::text(" : l'état reste valide."),
                        ])],
                    },
                    ListItem {
                        level: 1,
                        blocks: vec![paragraph(vec![Inline::text("Sous-point pour vérifier l'indentation imbriquée.")])],
                    },
                ],
            }),
            Block::List(ListBlock {
                ordered: true,
                start: Some(1),
                marker: None,
                items: vec![
                    ListItem {
                        level: 0,
                        blocks: vec![paragraph(vec![Inline::text("Commencer la transaction.")])],
                    },
                    ListItem {
                        level: 0,
                        blocks: vec![paragraph(vec![Inline::text("Valider ou annuler.")])],
                    },
                ],
            }),
            paragraph(vec![Inline::text(
                "Le tableau suivant doit être un véritable tableau Word, et non du texte encadré.",
            )]),
            Block::Table(table),
            Block::Caption {
                content: vec![Inline::text("Tableau 1 — Propriétés ACID")],
            },
            heading(2, "Exemple"),
            Block::CodeBlock {
                language: Some("sql".into()),
                text: "BEGIN TRANSACTION;\n  UPDATE compte SET solde = solde - 100 WHERE id = 1;\nCOMMIT;".into(),
            },
            Block::Quote {
                blocks: vec![paragraph(vec![Inline::text(
                    "Une transaction relit une même ligne et obtient une valeur différente car une autre transaction l'a modifiée et validée entre-temps.",
                )])],
            },
            Block::Rule,
            paragraph(vec![Inline::text("Fin du document témoin.")]),
            Block::PageBreak,
            heading(1, "Seconde page"),
            paragraph(vec![
                Inline::text("Le saut de page ci-dessus doit produire une nouvelle page. Exposant : x"),
                Inline::Text(TextRun::styled(
                    "2",
                    TextStyle {
                        vertical: VerticalShift::Superscript,
                        ..Default::default()
                    },
                )),
                Inline::text(" et indice : H"),
                Inline::Text(TextRun::styled(
                    "2",
                    TextStyle {
                        vertical: VerticalShift::Subscript,
                        ..Default::default()
                    },
                )),
                Inline::text("O."),
            ]),
        ],
    };

    match dconv_docx::write_to_path(&document, std::path::Path::new(&path)) {
        Ok(()) => {
            let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            println!("wrote {path} ({bytes} bytes)");
        }
        Err(error) => {
            eprintln!("failed: {error}");
            std::process::exit(1);
        }
    }
}
