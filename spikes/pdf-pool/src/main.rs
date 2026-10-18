//! Phase 0 spike C — does the worker-process pool actually scale?
//!
//! The plan rests on a claim: PDFium is a process-global singleton (verified in
//! spike A), so real parallelism requires a pool of *worker processes*, each
//! owning its own PDFium instance. That claim has a corollary that is easy to
//! assume and wrong to assume: spawning N processes gives you N-fold
//! throughput.
//!
//! It might not. Process creation is not free, PDFium takes time to bind and
//! lazy-initialise per process, and the workload is memory-bandwidth sensitive,
//! so cores can contend. This spike measures the real number instead of
//! trusting the intuition.
//!
//! The workload is the spike A extractor, unchanged. Only the concurrency model
//! differs between the two modes, so any difference is attributable to it.
//!
//! Run:
//!   cargo run -p spike-pdf-pool --profile spike -- \
//!     --worker target/spike/spike-pdf-extract.exe --workers 8 --jobs 8 \
//!     -- tests/fixtures/generated/perf.pdf

use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn main() -> Result<()> {
    let argv: Vec<String> = std::env::args().skip(1).collect();

    let worker = string_arg(&argv, "--worker")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("--worker <path to spike-pdf-extract> is required"))?;
    if !worker.exists() {
        bail!("worker binary not found: {}", worker.display());
    }

    let workers = usize_arg(&argv, "--workers")
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
        })
        .max(1);
    let rounds = usize_arg(&argv, "--rounds").unwrap_or(3).max(1);

    let base = files_after_dashdash(&argv);
    if base.is_empty() {
        bail!("pass one or more input files after `--`");
    }

    // Repeat the given files round-robin to reach the requested job count, so a
    // single fixture can drive an N-way scaling test without generating N files.
    let jobs = usize_arg(&argv, "--jobs").unwrap_or(base.len()).max(1);
    let tasks: Vec<String> = (0..jobs).map(|i| base[i % base.len()].clone()).collect();

    println!("worker    {}", worker.display());
    println!("workers   {workers}");
    println!("jobs      {}  ({jobs} documents)", tasks.len());
    println!("rounds    {rounds}\n");

    // --- Sequential baseline -------------------------------------------------
    // Same total work, one process at a time. This is the honest denominator:
    // comparing against a single *document* would fold in per-file variance.
    let mut seq = Vec::new();
    for round in 1..=rounds {
        let (wall, times) = run_sequential(&worker, &tasks)?;
        println!(
            "  seq  round {round}/{rounds}: wall {:>8.1} ms   (sum of workers {:>8.1} ms)",
            ms(wall),
            times.iter().sum::<f64>()
        );
        seq.push((wall, times));
    }

    // --- Parallel ------------------------------------------------------------
    let mut par = Vec::new();
    for round in 1..=rounds {
        let (wall, times) = run_parallel(&worker, &tasks, workers)?;
        println!(
            "  par  round {round}/{rounds}: wall {:>8.1} ms   (sum of workers {:>8.1} ms)",
            ms(wall),
            times.iter().sum::<f64>()
        );
        par.push((wall, times));
    }

    // --- Report --------------------------------------------------------------
    let seq_best = seq
        .iter()
        .map(|(w, _)| *w)
        .min()
        .expect("at least one round");
    let par_best = par
        .iter()
        .map(|(w, _)| *w)
        .min()
        .expect("at least one round");

    let seq_work: f64 = seq
        .iter()
        .map(|(_, t)| t.iter().sum::<f64>())
        .fold(f64::INFINITY, f64::min);
    let par_work: f64 = par
        .iter()
        .map(|(_, t)| t.iter().sum::<f64>())
        .fold(f64::INFINITY, f64::min);

    let speedup = seq_best.as_secs_f64() / par_best.as_secs_f64();
    let efficiency = speedup / workers as f64;

    println!("\n=== process-pool scaling ===");
    println!("  sequential wall       {:>10.1} ms", ms(seq_best));
    println!("  parallel wall         {:>10.1} ms", ms(par_best));
    println!("  speedup               {:>10.2}x  (ideal {workers}.00x)", speedup);
    println!(
        "  parallel efficiency   {:>10.1} %  of {workers} workers",
        efficiency * 100.0
    );
    println!();
    println!(
        "  in-worker total, seq  {:>10.1} ms  (actual extraction work)",
        seq_work
    );
    println!(
        "  in-worker total, par  {:>10.1} ms  (should match the line above)",
        par_work
    );

    // Whatever wall time is not accounted for by measured extraction is process
    // lifecycle plus document loading: creation, PDFium binding, PDF parsing,
    // teardown. Sequential mode is the reliable place to measure it, because
    // each job's full process lifetime is serialised into the wall clock.
    //
    // This number decides the pool's shape. If startup is a large fraction of a
    // job, a one-process-per-file pool burns most of its budget on startup and
    // the right design is a persistent worker that accepts many jobs.
    let seq_overhead_per_job = (ms(seq_best) - seq_work) / tasks.len() as f64;
    println!();
    println!(
        "  per-job overhead      {:>10.1} ms  sequential (spawn + bind + load)",
        seq_overhead_per_job
    );
    println!(
        "  parallel CPU budget   {:>10.1} ms  (wall x workers, upper bound)",
        ms(par_best) * workers as f64
    );

    Ok(())
}

/// Run every task to completion, one child process at a time.
///
/// Returns the wall time and each worker's self-reported extraction time.
fn run_sequential(worker: &Path, tasks: &[String]) -> Result<(Duration, Vec<f64>)> {
    let start = Instant::now();
    let mut times = Vec::with_capacity(tasks.len());
    for task in tasks {
        let child = spawn(worker, task)?;
        let output = child.wait_with_output().context("waiting for worker")?;
        times.push(parse_extract_ms(&output.stdout)?);
    }
    Ok((start.elapsed(), times))
}

/// Run tasks in concurrent batches of `workers`.
///
/// Every child in a batch is spawned before any is awaited, which is what makes
/// the batch genuinely concurrent rather than merely pipelined.
fn run_parallel(
    worker: &Path,
    tasks: &[String],
    workers: usize,
) -> Result<(Duration, Vec<f64>)> {
    let start = Instant::now();
    let mut times = Vec::with_capacity(tasks.len());

    for batch in tasks.chunks(workers) {
        let mut children: Vec<Child> = Vec::with_capacity(batch.len());
        for task in batch {
            children.push(spawn(worker, task)?);
        }
        for child in children {
            let output = child.wait_with_output().context("waiting for worker")?;
            times.push(parse_extract_ms(&output.stdout)?);
        }
    }

    Ok((start.elapsed(), times))
}

fn spawn(worker: &Path, task: &str) -> Result<Child> {
    Command::new(worker)
        .arg(task)
        .arg("--quiet")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning worker for {task}"))
}

/// Pull `extract_ms=<float>` out of the worker's single output line.
fn parse_extract_ms(stdout: &[u8]) -> Result<f64> {
    let text = String::from_utf8_lossy(stdout);
    let line = text
        .lines()
        .find(|l| l.starts_with("RESULT "))
        .ok_or_else(|| anyhow!("worker produced no RESULT line; got: {text:?}"))?;

    line.split_whitespace()
        .find_map(|field| field.strip_prefix("extract_ms="))
        .and_then(|v| v.parse::<f64>().ok())
        .ok_or_else(|| anyhow!("could not parse extract_ms from: {line:?}"))
}

fn files_after_dashdash(argv: &[String]) -> Vec<String> {
    match argv.iter().position(|a| a == "--") {
        Some(index) => argv[index + 1..].to_vec(),
        None => Vec::new(),
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn usize_arg(args: &[String], name: &str) -> Option<usize> {
    let idx = args.iter().position(|a| a == name)?;
    args.get(idx + 1)?.parse().ok()
}

fn string_arg(args: &[String], name: &str) -> Option<String> {
    let idx = args.iter().position(|a| a == name)?;
    args.get(idx + 1).cloned()
}
