// Host-cache backend benchmark harness.
//
// Compares persistent-cache implementation strategies under workloads
// derived from the omnifs FUSE host's expected traffic shape.
//
// See bench/host-cache/README.md for results and conclusions.

mod backends;
mod workload;

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use std::time::Instant;

use backends::{Backend, BackendKind};
use workload::{Workload, WorkloadKind};

#[derive(Parser, Debug)]
#[command(about = "omnifs host-cache backend benchmark")]
struct Args {
    #[arg(long, value_enum, default_values_t = BackendKind::all())]
    backends: Vec<BackendKind>,

    #[arg(long, value_enum, default_values_t = WorkloadKind::all())]
    workloads: Vec<WorkloadKind>,

    /// Records per workload. Each workload scales differently from this.
    #[arg(long, default_value_t = 20_000)]
    n: usize,

    /// Seed for deterministic synthetic data.
    #[arg(long, default_value_t = 0xC0FFEE)]
    seed: u64,

    /// Where to put per-backend scratch databases. Default: temp dir.
    #[arg(long)]
    workdir: Option<PathBuf>,

    /// Verbose: print per-step timings.
    #[arg(short, long)]
    verbose: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();

    let workdir = match &args.workdir {
        Some(p) => {
            std::fs::create_dir_all(p)?;
            p.clone()
        },
        None => tempfile::tempdir()?.keep(),
    };
    eprintln!("scratch dir: {}", workdir.display());

    let mut report = Report::new();

    for &backend in &args.backends {
        for &workload in &args.workloads {
            let dir = workdir.join(format!("{}_{}", backend.slug(), workload.slug()));
            std::fs::create_dir_all(&dir)?;

            let mut be: Box<dyn Backend> = backend
                .open(&dir)
                .with_context(|| format!("opening backend {backend:?}"))?;
            let wl = Workload::generate(workload, args.n, args.seed);

            let t = Instant::now();
            let mut stats = wl.run(be.as_mut(), args.verbose)?;
            let elapsed = t.elapsed();

            be.flush()?;
            let footprint = dir_size(&dir).unwrap_or(0);

            if matches!(workload, workload::WorkloadKind::HotRead) {
                drop(be);
                let t_open = Instant::now();
                let mut be2: Box<dyn Backend> = backend.open(&dir)?;
                let open_us = t_open.elapsed().as_micros();
                let cold = wl.cold_read(be2.as_mut(), 2_000)?;
                stats.extra = Some(format!(
                    "{}  reopen {}µs cold(2k): p50 {}µs p99 {}µs hit% {:.1}",
                    stats.extra.unwrap_or_default(),
                    open_us,
                    cold.p50_us,
                    cold.p99_us,
                    cold.hit_pct,
                ));
                be2.flush()?;
                drop(be2);
            }
            let footprint_after_flush = dir_size(&dir).unwrap_or(footprint);

            report.push(RunResult {
                backend,
                workload,
                elapsed_ms: elapsed.as_secs_f64() * 1000.0,
                stats,
                footprint_bytes: footprint.max(footprint_after_flush),
            });
            eprintln!(
                "{:<24} {:<24} {:>10.2} ms  {}",
                backend.slug(),
                workload.slug(),
                elapsed.as_secs_f64() * 1000.0,
                humansize::format_size(footprint.max(footprint_after_flush), humansize::BINARY),
            );
        }
    }

    println!();
    report.print();
    Ok(())
}

#[derive(Debug)]
pub struct RunResult {
    pub backend: BackendKind,
    pub workload: WorkloadKind,
    pub elapsed_ms: f64,
    pub stats: workload::Stats,
    pub footprint_bytes: u64,
}

struct Report {
    rows: Vec<RunResult>,
}

impl Report {
    fn new() -> Self {
        Self { rows: Vec::new() }
    }
    fn push(&mut self, r: RunResult) {
        self.rows.push(r);
    }
    fn print(&self) {
        // Group by workload, print backends side-by-side.
        let workloads: Vec<_> = {
            let mut seen = Vec::new();
            for r in &self.rows {
                if !seen.contains(&r.workload) {
                    seen.push(r.workload);
                }
            }
            seen
        };
        for wl in workloads {
            println!("=== workload: {} ===", wl.slug());
            println!(
                "{:<22} {:>12} {:>14} {:>14} {:>10}",
                "backend", "elapsed", "ops/s", "footprint", "extra"
            );
            for r in self.rows.iter().filter(|r| r.workload == wl) {
                let ops_per_s = r.stats.ops as f64 / (r.elapsed_ms / 1000.0);
                let footprint =
                    humansize::format_size(r.footprint_bytes, humansize::BINARY);
                let extra = match &r.stats.extra {
                    Some(s) => s.clone(),
                    None => String::new(),
                };
                println!(
                    "{:<22} {:>10.2}ms {:>14.0} {:>14} {:>10}",
                    r.backend.slug(),
                    r.elapsed_ms,
                    ops_per_s,
                    footprint,
                    extra,
                );
            }
            println!();
        }
    }
}

fn dir_size(p: &std::path::Path) -> Result<u64> {
    let mut total = 0u64;
    for entry in walkdir::WalkDir::new(p) {
        let entry = entry?;
        if entry.file_type().is_file() {
            total += entry.metadata()?.len();
        }
    }
    Ok(total)
}
