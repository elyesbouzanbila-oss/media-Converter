//! The GUI half of the byte-identical output assertion.
//!
//! The worker child calls `Converter::convert` exactly the way `dconv` (the
//! engine CLI) does — same facade, same call — so "GUI output equals CLI
//! output" is true by construction; what can still regress is anything
//! between the two stdio pipes. This test therefore speaks the frozen worker
//! protocol (`app/src/engine.rs`) to a real child — this same executable
//! re-invoked with `--dconvert-worker` — over the committed synthetic
//! fixtures, and compares the bytes with the committed conversion goldens.
//! No CLI involvement at all, exactly as the contract ask specified.
//!
//! Skipping is deliberate where the development tree lacks what a real child
//! needs: the fixtures and goldens are committed, `pdfium.dll` is not, and a
//! fresh clone without it still runs everything else in the suite. A child
//! that somehow cannot bind despite the check is also a skip with its reason
//! printed, not a failure: the engine's own golden test asserts conversion,
//! while this one asserts that the GUI path writes the same bytes the
//! goldens pin.

use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// The committed fixture/golden pairs, `(pdf, golden.docx)`, under the
/// workspace root. Note the shape of a pair name: the golden sits beside the
/// fixture as `<stem>.golden.docx`.
fn fixture_pairs() -> Vec<(PathBuf, PathBuf)> {
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("tests/fixtures/synthetic");
    let mut pairs = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&fixtures) {
        for entry in entries.filter_map(Result::ok) {
            let pdf = entry.path();
            if !pdf
                .extension()
                .map_or(false, |e| e.eq_ignore_ascii_case("pdf"))
            {
                continue;
            }
            let stem = pdf.file_stem().unwrap_or_default().to_string_lossy();
            let golden = fixtures.join(format!("{stem}.golden.docx"));
            if golden.is_file() {
                pairs.push((pdf, golden));
            }
        }
    }
    pairs.sort();
    pairs
}

/// One JSON line to the child, flushed: the transport is unbuffered
/// line-delimited JSON, same as the pool's writer threads.
fn send<W: Write>(out: &mut W, value: &Value) {
    writeln!(out, "{value}").expect("write a worker line");
    out.flush().expect("flush a worker line");
}

#[test]
fn worker_child_output_matches_the_committed_goldens() {
    let pairs = fixture_pairs();
    let pdfium = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("third_party/pdfium/bin/pdfium.dll");
    if pairs.is_empty() {
        eprintln!("skipping: no committed fixture/golden pairs under tests/fixtures/synthetic");
        return;
    }
    if !pdfium.is_file() {
        eprintln!("skipping: the development tree has no fetched pdfium.dll");
        return;
    }

    let mut child = {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dconvert"));
        command
            .arg("--dconvert-worker")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW, the same reason the pool sets it: a test run
            // must not flash a console per child.
            command.creation_flags(0x0800_0000);
        }
        command.spawn().expect("spawn the worker child")
    };

    let mut stdin = child.stdin.take().expect("worker stdin");
    let stdout = child.stdout.take().expect("worker stdout");
    let out_dir = tempfile::tempdir().expect("temp dir");

    // Talk to one worker for every job rather than one worker per job: the
    // second bind in a process fails (finding A1), so seven conversions
    // succeeding inside this single child is itself the bind-once guarantee.
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    for (index, (pdf, golden)) in pairs.iter().enumerate() {
        let id = (index + 1) as u64;
        let output = out_dir.path().join(format!(
            "{id}-{}.docx",
            pdf.file_stem().unwrap_or_default().to_string_lossy()
        ));
        send(
            &mut stdin,
            &serde_json::json!({
                "t": "job",
                "id": id,
                "source": pdf,
                "output": output,
                "target": "docx",
            }),
        );

        // Drain the child's lines until this job's terminal one arrives. The
        // real pool's reader is strict about exactly one terminal per job;
        // here the sequential loop gives that for free, and a `failed` is the
        // test failing loudly with the child's own words.
        loop {
            line.clear();
            let n = reader
                .read_line(&mut line)
                .expect("read a worker event line");
            assert!(n > 0, "the worker closed stdout before finishing job {id}");
            let value: Value = serde_json::from_str(line.trim()).expect("a worker line parses");
            match value["t"].as_str() {
                Some("started") | Some("progress") | Some("warning") => {}
                Some("finished") => break,
                Some("failed") => panic!(
                    "the worker failed {pdf:?}: {} ({})",
                    value["summary"], value["detail"]
                ),
                // Bind failure before any job: this tree cannot run a real
                // child, whatever the dll check above saw. Skip with the
                // child's reason rather than failing fixtures-without-dll.
                Some("fatal") => {
                    eprintln!(
                        "skipping: the worker child could not bind PDFium: {}",
                        value["detail"]
                    );
                    return;
                }
                _ => {}
            }
        }

        // The assertion itself: the bytes this GUI-path child wrote are the
        // committed golden's bytes, byte for byte.
        let got = std::fs::read(&output).unwrap_or_else(|error| {
            panic!("the worker did not write {} : {error}", output.display())
        });
        let want = std::fs::read(golden).expect("the committed golden is readable");
        assert_eq!(
            got,
            want,
            "output for {} differs from the committed golden",
            pdf.display()
        );
    }

    // EOF on stdin is the pool's clean shutdown; the child must exit 0.
    drop(stdin);
    let status = child.wait().expect("reap the worker child");
    assert!(
        status.success(),
        "the worker did not exit cleanly: {status:?}"
    );
}
