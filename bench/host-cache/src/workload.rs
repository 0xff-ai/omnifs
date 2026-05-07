// Synthetic workload generator + driver.
//
// Models the omnifs host's expected cache traffic: bursts of dirent +
// lookup writes from `list_children` terminals interleaved with point
// reads driven by FUSE getattr/lookup/readdir, file reads with a heavy
// duplicate-content tail, and occasional prefix invalidations from
// webhook-driven event-outcomes.

use crate::backends::{Backend, RecordKind};
use anyhow::Result;
use clap::ValueEnum;
use hdrhistogram::Histogram;
use rand::distributions::{Alphanumeric, Distribution, WeightedIndex};
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha20Rng;
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum WorkloadKind {
    /// Bulk preload: write throughput for terminal-application bursts.
    BulkPreload,
    /// Hot-read steady state: point reads against a populated cache.
    HotRead,
    /// Mixed: 70% read, 25% write, 5% delete (small subtrees only).
    Mixed,
    /// Prefix invalidation cost on a populated cache.
    PrefixInvalidate,
    /// File dedup: insert content with realistic duplication, measure
    /// resulting on-disk footprint.
    FileDedup,
    /// Large blob writes — exercises KV-separation strategies.
    LargeBlob,
}

impl WorkloadKind {
    pub fn all() -> Vec<WorkloadKind> {
        vec![
            Self::BulkPreload,
            Self::HotRead,
            Self::Mixed,
            Self::PrefixInvalidate,
            Self::FileDedup,
            Self::LargeBlob,
        ]
    }
    pub fn slug(self) -> &'static str {
        match self {
            Self::BulkPreload => "bulk_preload",
            Self::HotRead => "hot_read",
            Self::Mixed => "mixed",
            Self::PrefixInvalidate => "prefix_invalidate",
            Self::FileDedup => "file_dedup",
            Self::LargeBlob => "large_blob",
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub ops: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub extra: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct ColdReadResult {
    pub p50_us: u64,
    pub p99_us: u64,
    pub hit_pct: f64,
}

pub struct Workload {
    kind: WorkloadKind,
    n: usize,
    seed: u64,
    paths: Vec<String>,
}

impl Workload {
    pub fn generate(kind: WorkloadKind, n: usize, seed: u64) -> Self {
        let paths = generate_paths(n, seed);
        Self {
            kind,
            n,
            seed,
            paths,
        }
    }

    /// Independently measures cold-cache read latency on a fresh re-open.
    /// Reuses the same key reconstruction as `hot_read`.
    pub fn cold_read(&self, be: &mut dyn Backend, n_reads: usize) -> Result<ColdReadResult> {
        let dir_paths = self.paths.iter().filter(|p| !p.contains('.')).collect::<Vec<_>>();
        let mut keys: Vec<(String, RecordKind)> = Vec::new();
        let mut rng2 = ChaCha20Rng::seed_from_u64(self.seed ^ 0xA1);
        let target_terminals = (self.n / 10).max(50);
        for _ in 0..target_terminals {
            let parent = dir_paths[rng2.gen_range(0..dir_paths.len())].clone();
            let n_children = rng2.gen_range(3..40);
            let _ = synth_dirents_payload(&mut rng2, n_children);
            keys.push((parent.clone(), RecordKind::Dirents));
            for c in 0..n_children {
                let child_path = format!("{parent}/c{c:03}");
                let _ = synth_lookup_payload(&mut rng2);
                keys.push((child_path.clone(), RecordKind::Lookup));
                let _ = synth_attr_payload(&mut rng2);
                keys.push((child_path.clone(), RecordKind::Attr));
                if rng2.gen_bool(0.20) {
                    let _ = synth_file_small(&mut rng2);
                    keys.push((child_path, RecordKind::File));
                }
            }
        }
        let mut rng = ChaCha20Rng::seed_from_u64(self.seed ^ 0xCC);
        let mut hist = Histogram::<u64>::new_with_bounds(1, 60_000_000, 3).unwrap();
        let mut hits = 0u64;
        let n = n_reads.min(keys.len());
        for _ in 0..n {
            let (path, kind) = &keys[rng.gen_range(0..keys.len())];
            let t = Instant::now();
            let v = be.get(path, *kind)?;
            hist.record(t.elapsed().as_nanos() as u64).ok();
            if v.is_some() {
                hits += 1;
            }
        }
        Ok(ColdReadResult {
            p50_us: hist.value_at_quantile(0.50) / 1000,
            p99_us: hist.value_at_quantile(0.99) / 1000,
            hit_pct: hits as f64 / n.max(1) as f64 * 100.0,
        })
    }

    pub fn run(&self, be: &mut dyn Backend, verbose: bool) -> Result<Stats> {
        match self.kind {
            WorkloadKind::BulkPreload => self.bulk_preload(be, verbose),
            WorkloadKind::HotRead => self.hot_read(be, verbose),
            WorkloadKind::Mixed => self.mixed(be, verbose),
            WorkloadKind::PrefixInvalidate => self.prefix_invalidate(be, verbose),
            WorkloadKind::FileDedup => self.file_dedup(be, verbose),
            WorkloadKind::LargeBlob => self.large_blob(be, verbose),
        }
    }

    fn bulk_preload(&self, be: &mut dyn Backend, verbose: bool) -> Result<Stats> {
        // Simulate: each "terminal" produces a `list_children` for one dir
        // plus one Lookup + one Attr for each child plus a few small File
        // bodies as sibling-files. We batch each terminal as one write.
        let mut rng = ChaCha20Rng::seed_from_u64(self.seed ^ 0xA1);
        let mut stats = Stats::default();
        let mut t_last = Instant::now();
        let mut last_ops = 0u64;
        let dir_paths = self.paths.iter().filter(|p| !p.contains('.')).collect::<Vec<_>>();
        let target_terminals = (self.n / 10).max(50);
        for term_i in 0..target_terminals {
            let parent = dir_paths[rng.gen_range(0..dir_paths.len())].clone();
            let n_children = rng.gen_range(3..40);
            let mut batch = Vec::with_capacity(2 + n_children * 3);
            // dirents
            let dirents_payload = synth_dirents_payload(&mut rng, n_children);
            batch.push((parent.clone(), RecordKind::Dirents, dirents_payload.clone()));
            for c in 0..n_children {
                let child_name = format!("c{c:03}");
                let child_path = format!("{parent}/{child_name}");
                let lp = synth_lookup_payload(&mut rng);
                batch.push((child_path.clone(), RecordKind::Lookup, lp));
                let ap = synth_attr_payload(&mut rng);
                batch.push((child_path.clone(), RecordKind::Attr, ap));
                if rng.gen_bool(0.20) {
                    // ~20% of children get a small file body inlined
                    let f = synth_file_small(&mut rng);
                    batch.push((child_path, RecordKind::File, f));
                }
            }
            for (_, _, p) in &batch {
                stats.bytes_in += p.len() as u64;
            }
            stats.ops += batch.len() as u64;
            be.put_batch(&batch)?;
            if verbose && t_last.elapsed().as_secs() >= 1 {
                let dt = t_last.elapsed().as_secs_f64();
                eprintln!(
                    "  term {term_i}/{target_terminals}: {:.0} writes/s",
                    (stats.ops - last_ops) as f64 / dt
                );
                t_last = Instant::now();
                last_ops = stats.ops;
            }
        }
        Ok(stats)
    }

    fn hot_read(&self, be: &mut dyn Backend, _verbose: bool) -> Result<Stats> {
        // First populate, then do hot reads against a Zipfian-ish hot subset.
        let _populate_stats = self.bulk_preload(be, false)?;
        // Reseed for reads.
        let mut rng = ChaCha20Rng::seed_from_u64(self.seed ^ 0xB2);

        // Build the read-key set deterministically by replaying the same
        // RNG sequence as bulk_preload, but only computing keys.
        let dir_paths = self.paths.iter().filter(|p| !p.contains('.')).collect::<Vec<_>>();
        let mut keys: Vec<(String, RecordKind)> = Vec::new();
        let mut rng2 = ChaCha20Rng::seed_from_u64(self.seed ^ 0xA1);
        let target_terminals = (self.n / 10).max(50);
        for _ in 0..target_terminals {
            let parent = dir_paths[rng2.gen_range(0..dir_paths.len())].clone();
            let n_children = rng2.gen_range(3..40);
            let _ = synth_dirents_payload(&mut rng2, n_children);
            keys.push((parent.clone(), RecordKind::Dirents));
            for c in 0..n_children {
                let child_path = format!("{parent}/c{c:03}");
                let _ = synth_lookup_payload(&mut rng2);
                keys.push((child_path.clone(), RecordKind::Lookup));
                let _ = synth_attr_payload(&mut rng2);
                keys.push((child_path.clone(), RecordKind::Attr));
                if rng2.gen_bool(0.20) {
                    let _ = synth_file_small(&mut rng2);
                    keys.push((child_path, RecordKind::File));
                }
            }
        }
        let hot_keys: Vec<_> = keys.iter().take(keys.len() / 10).cloned().collect();
        let cold_keys = &keys[keys.len() / 10..];
        let target_reads = self.n * 5;
        let mut stats = Stats::default();
        let mut hits = 0u64;
        let mut hist = Histogram::<u64>::new_with_bounds(1, 60_000_000, 3).unwrap();
        for _ in 0..target_reads {
            let (path, kind) = if !hot_keys.is_empty() && rng.gen_bool(0.80) {
                hot_keys[rng.gen_range(0..hot_keys.len())].clone()
            } else if !cold_keys.is_empty() {
                cold_keys[rng.gen_range(0..cold_keys.len())].clone()
            } else {
                continue;
            };
            let t = Instant::now();
            let v = be.get(&path, kind)?;
            hist.record(t.elapsed().as_nanos() as u64).ok();
            if let Some(v) = v {
                hits += 1;
                stats.bytes_out += v.len() as u64;
            }
            stats.ops += 1;
        }
        stats.extra = Some(format!(
            "hit% {:.1}  p50 {}µs  p99 {}µs",
            (hits as f64 / stats.ops as f64) * 100.0,
            hist.value_at_quantile(0.50) / 1000,
            hist.value_at_quantile(0.99) / 1000,
        ));
        Ok(stats)
    }

    fn mixed(&self, be: &mut dyn Backend, _verbose: bool) -> Result<Stats> {
        // Pre-warm with a small bulk burst.
        self.bulk_preload(be, false)?;
        let mut rng = ChaCha20Rng::seed_from_u64(self.seed ^ 0xC3);
        let dir_paths = self.paths.iter().filter(|p| !p.contains('.')).collect::<Vec<_>>();
        let mut stats = Stats::default();
        let weights = WeightedIndex::new([70u32, 25, 5]).unwrap(); // read/write/del
        for _ in 0..self.n {
            match weights.sample(&mut rng) {
                0 => {
                    // read
                    let parent = dir_paths[rng.gen_range(0..dir_paths.len())].clone();
                    let kind = match rng.gen_range(0..3) {
                        0 => RecordKind::Lookup,
                        1 => RecordKind::Attr,
                        _ => RecordKind::Dirents,
                    };
                    let _ = be.get(&parent, kind)?;
                    stats.ops += 1;
                },
                1 => {
                    // write: a small Lookup or Attr or File
                    let parent = dir_paths[rng.gen_range(0..dir_paths.len())].clone();
                    let child = format!("{parent}/m{:04}", rng.gen_range(0..9999));
                    let (kind, payload) = match rng.gen_range(0..4) {
                        0 => (RecordKind::Lookup, synth_lookup_payload(&mut rng)),
                        1 => (RecordKind::Attr, synth_attr_payload(&mut rng)),
                        2 => (RecordKind::File, synth_file_small(&mut rng)),
                        _ => (RecordKind::File, synth_file_medium(&mut rng)),
                    };
                    stats.bytes_in += payload.len() as u64;
                    be.put_batch(&[(child, kind, payload)])?;
                    stats.ops += 1;
                },
                2 => {
                    // delete: pick a deeper path and exact-delete it; rare
                    // prefix-deletes scoped to a small leaf dir.
                    let deep_paths: Vec<&String> = dir_paths
                        .iter()
                        .copied()
                        .filter(|p| p.matches('/').count() >= 4)
                        .collect();
                    let parent: String = if !deep_paths.is_empty() {
                        deep_paths[rng.gen_range(0..deep_paths.len())].clone()
                    } else {
                        dir_paths[rng.gen_range(0..dir_paths.len())].clone()
                    };
                    if rng.gen_bool(0.9) {
                        be.delete_exact(&parent)?;
                    } else {
                        be.delete_prefix(&parent)?;
                    }
                    stats.ops += 1;
                },
                _ => unreachable!(),
            }
        }
        Ok(stats)
    }

    fn prefix_invalidate(&self, be: &mut dyn Backend, _verbose: bool) -> Result<Stats> {
        self.bulk_preload(be, false)?;
        let mut rng = ChaCha20Rng::seed_from_u64(self.seed ^ 0xD4);
        let dir_paths: Vec<_> = self
            .paths
            .iter()
            .filter(|p| !p.contains('.') && p.matches('/').count() <= 4)
            .cloned()
            .collect();
        let n_invalidations = (self.n / 100).max(20).min(2000);
        let mut stats = Stats::default();
        for _ in 0..n_invalidations {
            let prefix = &dir_paths[rng.gen_range(0..dir_paths.len())];
            let deleted = be.delete_prefix(prefix)?;
            stats.ops += 1;
            stats.bytes_out += deleted as u64;
        }
        stats.extra = Some(format!("rows_evicted {}", stats.bytes_out));
        Ok(stats)
    }

    fn file_dedup(&self, be: &mut dyn Backend, _verbose: bool) -> Result<Stats> {
        // Generate a corpus of distinct file payloads, then write them at
        // many paths with realistic duplication: 50% of paths share content
        // with another path in the corpus.
        let mut rng = ChaCha20Rng::seed_from_u64(self.seed ^ 0xE5);
        let n_files = self.n;
        let n_distinct = (n_files / 2).max(10);
        let corpus: Vec<Vec<u8>> = (0..n_distinct)
            .map(|i| {
                if i % 8 == 0 {
                    synth_file_medium(&mut rng)
                } else if i % 32 == 0 {
                    synth_file_large(&mut rng)
                } else {
                    synth_file_small(&mut rng)
                }
            })
            .collect();
        let mut stats = Stats::default();
        let mut written = 0u64;
        let mut batch = Vec::with_capacity(1000);
        for i in 0..n_files {
            let path = self.paths[i % self.paths.len()].clone();
            let payload = corpus[rng.gen_range(0..corpus.len())].clone();
            written += payload.len() as u64;
            batch.push((path, RecordKind::File, payload));
            if batch.len() >= 1000 {
                for (_, _, p) in &batch {
                    stats.bytes_in += p.len() as u64;
                }
                stats.ops += batch.len() as u64;
                be.put_batch(&batch)?;
                batch.clear();
            }
        }
        if !batch.is_empty() {
            for (_, _, p) in &batch {
                stats.bytes_in += p.len() as u64;
            }
            stats.ops += batch.len() as u64;
            be.put_batch(&batch)?;
        }
        stats.extra = Some(format!(
            "logical_bytes {}",
            humansize::format_size(written, humansize::BINARY)
        ));
        Ok(stats)
    }

    fn large_blob(&self, be: &mut dyn Backend, _verbose: bool) -> Result<Stats> {
        // Many medium and large file payloads. No dedup. Tests how each
        // backend handles big values: write-amp on B-trees vs LSM blob log.
        let mut rng = ChaCha20Rng::seed_from_u64(self.seed ^ 0xF6);
        let n_files = self.n / 4;
        let mut stats = Stats::default();
        let mut batch = Vec::with_capacity(64);
        for i in 0..n_files {
            let path = format!("/large/n{i:06}.bin");
            let payload = if i % 4 == 0 {
                synth_file_large(&mut rng)
            } else {
                synth_file_medium(&mut rng)
            };
            stats.bytes_in += payload.len() as u64;
            batch.push((path, RecordKind::File, payload));
            if batch.len() >= 64 {
                stats.ops += batch.len() as u64;
                be.put_batch(&batch)?;
                batch.clear();
            }
        }
        if !batch.is_empty() {
            stats.ops += batch.len() as u64;
            be.put_batch(&batch)?;
        }
        // Then read the whole set sequentially.
        let mut hits = 0u64;
        let mut bytes_out = 0u64;
        let mut hist = Histogram::<u64>::new_with_bounds(1, 60_000_000, 3).unwrap();
        for i in 0..n_files {
            let path = format!("/large/n{i:06}.bin");
            let t = Instant::now();
            let v = be.get(&path, RecordKind::File)?;
            hist.record(t.elapsed().as_nanos() as u64).ok();
            if let Some(v) = v {
                hits += 1;
                bytes_out += v.len() as u64;
            }
        }
        stats.bytes_out = bytes_out;
        stats.extra = Some(format!(
            "logical_bytes {} read p50 {}µs p99 {}µs hit% {:.1}",
            humansize::format_size(stats.bytes_in, humansize::BINARY),
            hist.value_at_quantile(0.50) / 1000,
            hist.value_at_quantile(0.99) / 1000,
            (hits as f64 / n_files as f64) * 100.0,
        ));
        Ok(stats)
    }
} // impl Workload

// --- Synthetic data shapes ---

fn generate_paths(n: usize, seed: u64) -> Vec<String> {
    let mut rng = ChaCha20Rng::seed_from_u64(seed);
    // Provider-flavored: /repos/{org}/{name}/issues/{n}/comments/{n}/file
    let n_orgs = ((n as f64).sqrt() as usize / 4).max(8);
    let n_repos_per_org = 8;
    let n_dirs_per_repo = 16;
    let mut out = Vec::with_capacity(n);
    out.push("/".to_string());
    out.push("/repos".to_string());
    for org_i in 0..n_orgs {
        let org = format!("org{org_i:03}");
        out.push(format!("/repos/{org}"));
        for repo_i in 0..n_repos_per_org {
            let repo = format!("repo{repo_i:02}");
            out.push(format!("/repos/{org}/{repo}"));
            for d in &["issues", "pulls", "branches", "tags"] {
                out.push(format!("/repos/{org}/{repo}/{d}"));
                for k in 0..n_dirs_per_repo {
                    out.push(format!("/repos/{org}/{repo}/{d}/n{k:04}"));
                    if rng.gen_bool(0.3) {
                        out.push(format!("/repos/{org}/{repo}/{d}/n{k:04}/title.txt"));
                        out.push(format!("/repos/{org}/{repo}/{d}/n{k:04}/body.md"));
                    }
                }
            }
            if out.len() >= n {
                return out;
            }
        }
    }
    out
}

fn synth_lookup_payload(rng: &mut ChaCha20Rng) -> Vec<u8> {
    // (kind: 1, size: 8, name_len: 1, name: ~4-12) postcard-flavored.
    let mut v = Vec::with_capacity(24);
    v.push(rng.gen_range(0..2)); // dir / file
    v.extend_from_slice(&rng.r#gen::<u64>().to_le_bytes());
    let name_len = rng.gen_range(4..12);
    v.push(name_len as u8);
    v.extend((0..name_len).map(|_| rng.sample(Alphanumeric)));
    v
}

fn synth_attr_payload(rng: &mut ChaCha20Rng) -> Vec<u8> {
    let mut v = Vec::with_capacity(16);
    v.push(rng.gen_range(0..2));
    v.extend_from_slice(&rng.r#gen::<u64>().to_le_bytes());
    v
}

fn synth_dirents_payload(rng: &mut ChaCha20Rng, n: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + n * 24);
    v.extend_from_slice(&(n as u32).to_le_bytes());
    v.push(1); // exhaustive
    for _ in 0..n {
        v.push(rng.gen_range(0..2));
        v.extend_from_slice(&rng.r#gen::<u64>().to_le_bytes());
        let name_len = rng.gen_range(4..12);
        v.push(name_len as u8);
        v.extend((0..name_len).map(|_| rng.sample(Alphanumeric)));
    }
    v
}

fn synth_file_small(rng: &mut ChaCha20Rng) -> Vec<u8> {
    // 64 B - 4 KB
    let len = rng.gen_range(64..4096);
    fill_compressible_bytes(rng, len)
}

fn synth_file_medium(rng: &mut ChaCha20Rng) -> Vec<u8> {
    // 4 KB - 64 KB
    let len = rng.gen_range(4096..64 * 1024);
    fill_compressible_bytes(rng, len)
}

fn synth_file_large(rng: &mut ChaCha20Rng) -> Vec<u8> {
    // 64 KB - 512 KB (kept off the 1 MiB+ tail to keep bench runtime
    // sane; eviction behavior is independent of single-blob size beyond
    // a threshold)
    let len = rng.gen_range(64 * 1024..512 * 1024);
    fill_compressible_bytes(rng, len)
}

fn fill_compressible_bytes(rng: &mut ChaCha20Rng, len: usize) -> Vec<u8> {
    // Approximate text/code: roughly 2x compressible by zstd in practice.
    // Mix a small dictionary of "tokens" with random separators and a
    // sprinkling of noise so the result isn't pathologically compressible.
    let dict = [
        "function", "return", "const", "let", "if", "else", "the", "of",
        "and", "user", "data", "id", "path", "name", "value", "type",
        "true", "false", "null", "object", "list", "string", "number",
    ];
    let mut v = Vec::with_capacity(len);
    while v.len() < len {
        let tok = dict[rng.gen_range(0..dict.len())];
        v.extend_from_slice(tok.as_bytes());
        v.push(if rng.gen_bool(0.85) { b' ' } else { rng.sample(Alphanumeric) });
        if rng.gen_bool(0.05) {
            // sprinkle of noise
            v.push(rng.r#gen());
            v.push(rng.r#gen());
        }
    }
    v.truncate(len);
    v
}
