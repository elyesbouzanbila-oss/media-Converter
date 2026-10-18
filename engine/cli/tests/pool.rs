//! The parallel path has to write the same bytes, and report the same facts in
//! the same order, as the serial one.
//!
//! This is the CLI half of "byte-identical output between front ends". The GUI
//! has a worker pool of its own, and the only way to know neither front end has
//! grown conversion logic of its own is to convert the same documents through
//! both and compare. Serial is the reference here because it is the path
//! `dconv-engine`'s golden test already pins to the committed `.docx` files —
//! and these tests check the goldens directly as well, because two paths sharing
//! one mistake would otherwise agree with each other and pass.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The workspace root. Relative components are deliberate: this is passed to a
/// child process whose working directory is the package root, and a path with
/// `..` in it resolves there the same way for both runs being compared.
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn fixtures() -> Vec<PathBuf> {
    let directory = root().join("tests/fixtures/synthetic");
    let mut documents: Vec<PathBuf> = std::fs::read_dir(&directory)
        .unwrap_or_else(|error| panic!("reading {}: {error}", directory.display()))
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|extension| extension == "pdf"))
        .collect();
    // `read_dir` order is arbitrary, and a test that depends on it would pin the
    // input order to whatever the filesystem felt like today.
    documents.sort();
    assert!(
        documents.len() >= 4,
        "a pool needs several documents to be a pool; found {}",
        documents.len()
    );
    documents
}

struct Run {
    status: i32,
    stdout: String,
    stderr: String,
}

/// Convert `inputs` into `out`, in one command, and keep what it said.
fn run(inputs: &[PathBuf], out: &Path, jobs: usize) -> Run {
    let output: Output = Command::new(env!("CARGO_BIN_EXE_dconv"))
        .args(inputs)
        .arg("--jobs")
        .arg(jobs.to_string())
        .arg("--output")
        .arg(out)
        // Progress is the one thing the two paths report differently — page 2/5
        // of one document against "3 of 7 files" across a batch — so asking for
        // it is also how these tests tell which path ran.
        .arg("--verbose")
        .output()
        .expect("dconv runs");

    Run {
        status: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// The bytes written for each input, in input order.
fn written(inputs: &[PathBuf], out: &Path) -> Vec<Vec<u8>> {
    inputs
        .iter()
        .map(|input| {
            let stem = input.file_stem().expect("a fixture has a name");
            let path = out.join(stem).with_extension("docx");
            std::fs::read(&path).unwrap_or_else(|error| panic!("reading {}: {error}", path.display()))
        })
        .collect()
}

fn scratch(label: &str) -> PathBuf {
    let directory = root().join("out/cli-pool-test").join(label);
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory)
        .unwrap_or_else(|error| panic!("creating {}: {error}", directory.display()));
    directory
}

/// A run's report with the measurements taken out.
///
/// Every number in the output that is a *duration* differs between two runs of
/// the same path, so comparing the two paths means ignoring them. Nothing else
/// is ignored: which file, in which order, with which page and block counts is
/// exactly what is being checked, and none of that is a timing.
fn without_timings(stdout: &str) -> String {
    const UNITS: [&str; 3] = ["µs", "ms", "s"];
    let mut kept: Vec<&str> = Vec::new();
    for token in stdout.split_whitespace() {
        // The unit can pick up the punctuation that follows it — `write 3 ms)`
        // is three tokens, the last of which is `ms)`. Missing that leaves the
        // number behind, and the comparison fails on a timing it meant to
        // ignore.
        if UNITS.contains(&token.trim_end_matches([',', ')'])) {
            // The unit follows its number, so the number is the token just kept.
            if kept
                .last()
                .is_some_and(|last| last.chars().all(|c| c.is_ascii_digit() || c == '.'))
            {
                kept.pop();
            }
            continue;
        }
        kept.push(token);
    }
    kept.join(" ")
}

/// Where two reports stop agreeing, line by line.
fn lines_differing(expected: &str, actual: &str) -> String {
    let mut report = String::new();
    for (index, (expected, actual)) in expected.lines().zip(actual.lines()).enumerate() {
        if expected != actual {
            report.push_str(&format!(
                "  line {}:\n    serial: {expected}\n    pooled: {actual}\n",
                index + 1
            ));
        }
    }
    let (expected_lines, actual_lines) = (expected.lines().count(), actual.lines().count());
    if expected_lines != actual_lines {
        report.push_str(&format!(
            "  the serial run reported {expected_lines} lines, the pooled run {actual_lines}\n"
        ));
    }
    if report.is_empty() {
        report.push_str("  the lines agree, so the difference is whitespace\n");
    }
    report
}

#[test]
fn a_pooled_run_writes_and_reports_what_a_serial_run_does() {
    let inputs = fixtures();
    // Both runs write to the same directory and it is emptied between them.
    // The report names the file it wrote, so two reports can only be compared
    // if both runs were given the same place to write — and emptying it is what
    // stops the second run from passing on the first run's output.
    let directory = scratch("one-directory");

    let serial = run(&inputs, &directory, 1);
    let serial_bytes = written(&inputs, &directory);
    std::fs::remove_dir_all(&directory).expect("clearing the scratch directory");
    std::fs::create_dir_all(&directory).expect("recreating the scratch directory");

    let pooled = run(&inputs, &directory, 4);
    let pooled_bytes = written(&inputs, &directory);

    assert_eq!(serial.status, 0, "the serial run failed: {}", serial.stderr);
    assert_eq!(pooled.status, 0, "the pooled run failed: {}", pooled.stderr);

    // The pool really ran. Without this the rest of the test would pass just as
    // happily if `--jobs` were ignored: the two paths agree, which is the point,
    // but they agree trivially if only one of them ever ran.
    assert!(
        pooled.stderr.contains("files,"),
        "a pooled run has no single document to be on page 3 of, so it reports \
         files and workers instead: {:?}",
        pooled.stderr
    );
    assert!(
        serial.stderr.contains("page ") || !serial.stderr.contains("files,"),
        "the serial path reports per page, not per file: {:?}",
        serial.stderr
    );

    // Same facts, same order: the timings are the only thing that may differ.
    let expected = without_timings(&serial.stdout);
    let actual = without_timings(&pooled.stdout);
    if actual != expected {
        panic!(
            "a pooled run must report the same files in the same order as a serial one\n{}",
            lines_differing(&expected, &actual)
        );
    }

    let golden_dir = root().join("tests/fixtures/synthetic");
    for (index, input) in inputs.iter().enumerate() {
        let name = input.file_name().expect("a fixture has a name").to_string_lossy();
        assert_eq!(
            pooled_bytes[index], serial_bytes[index],
            "{name}: the pool wrote different bytes from the serial path"
        );

        let golden = golden_dir.join(format!(
            "{}.golden.docx",
            input.file_stem().expect("a fixture has a stem").to_string_lossy()
        ));
        let expected = std::fs::read(&golden)
            .unwrap_or_else(|error| panic!("reading {}: {error}", golden.display()));
        assert_eq!(
            pooled_bytes[index], expected,
            "{name}: the pool wrote bytes that differ from the committed golden file"
        );
    }
}

#[test]
fn a_pool_carries_on_when_one_job_cannot_be_read() {
    // The contract every front end's pool has to honour: a job that fails is
    // still a job that is accounted for, exactly once, and it does not strand
    // the jobs beside it.
    let inputs = fixtures();
    let directory = scratch("failure");
    std::fs::create_dir_all(directory.join("nested")).expect("scratch directory");

    let mut batch = vec![inputs[0].clone(), directory.join("nested/missing.pdf")];
    batch.extend(inputs[1..3].iter().cloned());

    let run = run(&batch, &directory, 4);

    assert_ne!(run.status, 0, "a missing input has to fail the command");
    // One line *attributes* the failure — `path: message` — and the message
    // itself is several lines long and repeats the path, so counting mentions
    // of the name would count the same report twice.
    let missing = directory.join("nested/missing.pdf");
    let attributed = run
        .stderr
        .lines()
        .filter(|line| line.starts_with(&*missing.to_string_lossy()))
        .count();
    assert_eq!(
        attributed, 1,
        "the missing input should be accounted for once, not once per worker: {}",
        run.stderr
    );
    assert!(
        run.stdout.matches(" -> ").count() == 3,
        "the three readable documents still have to be reported: {}",
        run.stdout
    );

    let readable = vec![inputs[0].clone(), inputs[1].clone(), inputs[2].clone()];
    let bytes = written(&readable, &directory);
    let golden_dir = root().join("tests/fixtures/synthetic");
    for (index, input) in readable.iter().enumerate() {
        let golden = golden_dir.join(format!(
            "{}.golden.docx",
            input.file_stem().expect("a fixture has a stem").to_string_lossy()
        ));
        let expected = std::fs::read(&golden).expect("the golden file is committed");
        assert_eq!(
            bytes[index], expected,
            "{} was converted anyway, and correctly, beside a failed job",
            input.display()
        );
    }
}
