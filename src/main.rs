use std::fs::File;
use std::io::{Write, sink};
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Result, bail};
use clap::{Args, Parser, Subcommand};
use nas_cache::cache::{CacheConfig, CacheIoStats, ReadThroughCache, prune_config};
use nas_cache::pathing::relative_input;

#[derive(Debug, Parser)]
#[command(name = "nas-fast-cache")]
#[command(about = "Fast read-through local cache for NAS-backed Windows paths.")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Bench {
        #[arg(long)]
        source_root: PathBuf,
        #[arg(long)]
        cache_root: PathBuf,
        #[arg(long)]
        path: PathBuf,
        #[arg(long, default_value_t = 2)]
        passes: u32,
        #[arg(long, default_value_t = 8)]
        chunk_size_mib: u64,
        #[arg(long)]
        limit_mib: Option<u64>,
        #[arg(long, default_value_t = 0)]
        offset_mib: u64,
        #[arg(long)]
        disable_cache_writes: bool,
        #[arg(long)]
        enable_sequential_conveyor: bool,
        #[command(flatten)]
        cache_policy: CachePolicyArgs,
    },
    Read {
        #[arg(long)]
        source_root: PathBuf,
        #[arg(long)]
        cache_root: PathBuf,
        #[arg(long)]
        path: PathBuf,
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long, default_value_t = 8)]
        chunk_size_mib: u64,
        #[arg(long)]
        limit_mib: Option<u64>,
        #[arg(long, default_value_t = 0)]
        offset_mib: u64,
        #[arg(long)]
        disable_cache_writes: bool,
        #[arg(long)]
        enable_sequential_conveyor: bool,
        #[command(flatten)]
        cache_policy: CachePolicyArgs,
    },
    Stat {
        #[arg(long)]
        source_root: PathBuf,
        #[arg(long)]
        cache_root: PathBuf,
        #[arg(long)]
        path: PathBuf,
        #[arg(long, default_value_t = 8)]
        chunk_size_mib: u64,
    },
    EvictFile {
        #[arg(long)]
        source_root: PathBuf,
        #[arg(long)]
        cache_root: PathBuf,
        #[arg(long)]
        path: PathBuf,
        #[arg(long, default_value_t = 8)]
        chunk_size_mib: u64,
    },
    Mount {
        #[arg(long)]
        source_root: PathBuf,
        #[arg(long)]
        cache_root: PathBuf,
        #[arg(long)]
        mount: String,
        #[arg(long, default_value_t = 8)]
        chunk_size_mib: u64,
        #[arg(long, default_value_t = 0)]
        threads: u32,
        #[arg(long)]
        disable_cache_writes: bool,
        #[arg(long)]
        enable_sequential_conveyor: bool,
        #[arg(long)]
        enable_writes: bool,
        #[arg(long)]
        write_prefix: Option<PathBuf>,
        #[arg(long)]
        reuse_write_handles: bool,
        #[command(flatten)]
        cache_policy: CachePolicyArgs,
    },
    Prune {
        #[arg(long)]
        cache_root: PathBuf,
        #[command(flatten)]
        cache_policy: CachePolicyArgs,
    },
}

#[derive(Clone, Copy, Debug, Default, Args)]
struct CachePolicyArgs {
    #[arg(long)]
    max_cache_gb: Option<f64>,
    #[arg(long)]
    max_age_hours: Option<f64>,
    #[arg(long)]
    min_free_gb: Option<f64>,
    #[arg(long, default_value_t = 0.0)]
    min_eviction_age_hours: f64,
    #[arg(long, default_value_t = 300)]
    prune_interval_seconds: u64,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Bench {
            source_root,
            cache_root,
            path,
            passes,
            chunk_size_mib,
            limit_mib,
            offset_mib,
            disable_cache_writes,
            enable_sequential_conveyor,
            cache_policy,
        } => {
            let cache = cache(
                source_root,
                cache_root,
                chunk_size_mib,
                !disable_cache_writes,
                enable_sequential_conveyor,
                cache_policy,
            )?;
            let rel = relative_input(&cache.config().source_root, path)?;
            let limit = limit_mib.map(mib);
            let offset = mib(offset_mib);
            for pass in 1..=passes {
                let mut out = sink();
                let started = Instant::now();
                let stats = cache.read_range_to_writer(&rel, offset, limit, &mut out)?;
                let elapsed = started.elapsed().as_secs_f64();
                println!("pass={pass} {}", format_stats(&stats, elapsed));
                let flush_started = Instant::now();
                cache.flush_pending()?;
                let flush_elapsed = flush_started.elapsed().as_secs_f64();
                println!("pass={pass} cache_flush_seconds={flush_elapsed:.3}");
            }
        }
        Command::Read {
            source_root,
            cache_root,
            path,
            out,
            chunk_size_mib,
            limit_mib,
            offset_mib,
            disable_cache_writes,
            enable_sequential_conveyor,
            cache_policy,
        } => {
            let cache = cache(
                source_root,
                cache_root,
                chunk_size_mib,
                !disable_cache_writes,
                enable_sequential_conveyor,
                cache_policy,
            )?;
            let rel = relative_input(&cache.config().source_root, path)?;
            let mut writer: Box<dyn Write> = match out {
                Some(path) => Box::new(File::create(path)?),
                None => Box::new(sink()),
            };
            let started = Instant::now();
            let stats = cache.read_range_to_writer(
                &rel,
                mib(offset_mib),
                limit_mib.map(mib),
                &mut writer,
            )?;
            let elapsed = started.elapsed().as_secs_f64();
            println!("{}", format_stats(&stats, elapsed));
            cache.flush_pending()?;
        }
        Command::Stat {
            source_root,
            cache_root,
            path,
            chunk_size_mib,
        } => {
            let cache = cache(
                source_root,
                cache_root,
                chunk_size_mib,
                true,
                false,
                CachePolicyArgs::default(),
            )?;
            let rel = relative_input(&cache.config().source_root, path)?;
            let stat = cache.stat(&rel)?;
            println!("{}", serde_json::to_string_pretty(&stat)?);
        }
        Command::EvictFile {
            source_root,
            cache_root,
            path,
            chunk_size_mib,
        } => {
            let cache = cache(
                source_root,
                cache_root,
                chunk_size_mib,
                true,
                false,
                CachePolicyArgs::default(),
            )?;
            let rel = relative_input(&cache.config().source_root, path)?;
            let removed = cache.evict_file(&rel)?;
            println!("removed_bytes={removed}");
        }
        Command::Mount {
            source_root,
            cache_root,
            mount: mount_point,
            chunk_size_mib,
            threads,
            disable_cache_writes,
            enable_sequential_conveyor,
            enable_writes,
            write_prefix,
            reuse_write_handles,
            cache_policy,
        } => mount(
            source_root,
            cache_root,
            mount_point,
            chunk_size_mib,
            threads,
            !disable_cache_writes,
            enable_sequential_conveyor,
            enable_writes,
            write_prefix,
            reuse_write_handles,
            cache_policy,
        )?,
        Command::Prune {
            cache_root,
            cache_policy,
        } => {
            let mut config = CacheConfig::new(PathBuf::new(), cache_root);
            apply_cache_policy(&mut config, cache_policy)?;
            let summary = prune_config(&config)?;
            println!("{}", serde_json::to_string_pretty(&summary_json(summary))?);
        }
    }
    Ok(())
}

fn cache(
    source_root: PathBuf,
    cache_root: PathBuf,
    chunk_size_mib: u64,
    write_cache: bool,
    enable_sequential_conveyor: bool,
    cache_policy: CachePolicyArgs,
) -> Result<ReadThroughCache> {
    if chunk_size_mib == 0 {
        bail!("chunk size must be greater than zero");
    }
    let mut config = CacheConfig::new(source_root, cache_root);
    config.chunk_size = mib(chunk_size_mib);
    config.write_cache = write_cache;
    config.enable_sequential_conveyor = enable_sequential_conveyor;
    apply_cache_policy(&mut config, cache_policy)?;
    Ok(ReadThroughCache::new(config))
}

fn apply_cache_policy(config: &mut CacheConfig, policy: CachePolicyArgs) -> Result<()> {
    config.max_cache_bytes = policy
        .max_cache_gb
        .map(|value| bytes_from_gb(value, "max-cache-gb"))
        .transpose()?;
    config.max_cache_age = policy
        .max_age_hours
        .map(|value| duration_from_hours(value, "max-age-hours"))
        .transpose()?;
    config.min_free_bytes = policy
        .min_free_gb
        .map(|value| bytes_from_gb(value, "min-free-gb"))
        .transpose()?;
    config.min_eviction_age =
        duration_from_hours(policy.min_eviction_age_hours, "min-eviction-age-hours")?;
    if policy.prune_interval_seconds == 0 {
        bail!("prune interval must be greater than zero");
    }
    config.prune_interval = std::time::Duration::from_secs(policy.prune_interval_seconds);
    Ok(())
}

fn bytes_from_gb(value: f64, name: &str) -> Result<u64> {
    if !value.is_finite() || value <= 0.0 {
        bail!("{name} must be greater than zero");
    }
    Ok((value * 1024.0 * 1024.0 * 1024.0).round() as u64)
}

fn duration_from_hours(value: f64, name: &str) -> Result<std::time::Duration> {
    if !value.is_finite() || value < 0.0 {
        bail!("{name} must be zero or greater");
    }
    Ok(std::time::Duration::from_secs_f64(value * 3600.0))
}

fn mib(value: u64) -> u64 {
    value * 1024 * 1024
}

fn format_stats(stats: &CacheIoStats, elapsed: f64) -> String {
    let app_mib = stats.bytes as f64 / 1024.0 / 1024.0;
    let cache_mib = stats.cache_bytes as f64 / 1024.0 / 1024.0;
    let source_app_mib = stats.source_bytes as f64 / 1024.0 / 1024.0;
    let source_fetch_mib = stats.source_fetch_bytes as f64 / 1024.0 / 1024.0;
    let source_seconds = stats.source_read_ns as f64 / 1_000_000_000.0;
    let cache_seconds = stats.cache_read_ns as f64 / 1_000_000_000.0;
    let write_enqueue_seconds = stats.cache_write_enqueue_ns as f64 / 1_000_000_000.0;
    let source_fetch_ratio = if stats.bytes == 0 {
        0.0
    } else {
        stats.source_fetch_bytes as f64 / stats.bytes as f64
    };
    let source_fetch_mib_per_sec = if source_seconds <= 0.0 {
        0.0
    } else {
        source_fetch_mib / source_seconds
    };
    format!(
        "bytes={} mib={app_mib:.2} seconds={elapsed:.3} mib_per_sec={:.2} cache_hits={} cache_misses={} cache_mib={cache_mib:.2} source_app_mib={source_app_mib:.2} source_fetch_mib={source_fetch_mib:.2} source_fetch_per_app={source_fetch_ratio:.3} cache_read_ops={} source_read_ops={} cache_read_seconds={cache_seconds:.3} cache_read_max_ms={:.3} source_read_seconds={source_seconds:.3} source_read_max_ms={:.3} source_fetch_mib_per_sec={source_fetch_mib_per_sec:.2} cache_write_jobs={} cache_write_mib={:.2} cache_write_enqueue_seconds={write_enqueue_seconds:.3} window_hits={} window_fills={} prefetch_requests={} prefetch_hits={} prefetch_waits={} prefetch_hit_mib={:.2} demand_wait_seconds={:.3} heap_alloc_mib={:.2}",
        stats.bytes,
        app_mib / elapsed.max(0.000_001),
        stats.cache_hits,
        stats.cache_misses,
        stats.cache_read_ops,
        stats.source_read_ops,
        stats.cache_read_max_ns as f64 / 1_000_000.0,
        stats.source_read_max_ns as f64 / 1_000_000.0,
        stats.cache_write_jobs,
        stats.cache_write_bytes as f64 / 1024.0 / 1024.0,
        stats.window_hits,
        stats.window_fills,
        stats.prefetch_requests,
        stats.prefetch_hits,
        stats.prefetch_waits,
        stats.prefetch_hit_bytes as f64 / 1024.0 / 1024.0,
        stats.demand_wait_ns as f64 / 1_000_000_000.0,
        stats.heap_allocated_bytes as f64 / 1024.0 / 1024.0,
    )
}

#[cfg(all(windows, feature = "mount"))]
fn mount(
    source_root: PathBuf,
    cache_root: PathBuf,
    mount: String,
    chunk_size_mib: u64,
    threads: u32,
    write_cache: bool,
    enable_sequential_conveyor: bool,
    enable_writes: bool,
    write_prefix: Option<PathBuf>,
    reuse_write_handles: bool,
    cache_policy: CachePolicyArgs,
) -> Result<()> {
    use nas_cache::pathing::normalize_relative_path;
    use nas_cache::winfsp_mount::{MountOptions, mount_with_options};

    let write_prefix = match (enable_writes, write_prefix) {
        (true, Some(prefix)) => Some(normalize_relative_path(prefix)?),
        (true, None) => bail!("--enable-writes requires --write-prefix"),
        (false, Some(_)) => bail!("--write-prefix requires --enable-writes"),
        (false, None) => None,
    };

    let cache = cache(
        source_root,
        cache_root,
        chunk_size_mib,
        write_cache,
        enable_sequential_conveyor,
        cache_policy,
    )?;
    mount_with_options(
        cache,
        &mount,
        MountOptions {
            threads,
            enable_writes,
            write_prefix,
            reuse_write_handles,
        },
    )
    .map_err(|err| anyhow::anyhow!("failed to mount WinFsp filesystem: {err:?}"))
}

#[cfg(not(all(windows, feature = "mount")))]
fn mount(
    _source_root: PathBuf,
    _cache_root: PathBuf,
    _mount: String,
    _chunk_size_mib: u64,
    _threads: u32,
    _write_cache: bool,
    _enable_sequential_conveyor: bool,
    _enable_writes: bool,
    _write_prefix: Option<PathBuf>,
    _reuse_write_handles: bool,
    _cache_policy: CachePolicyArgs,
) -> Result<()> {
    bail!("mount support is only available on Windows with the `mount` feature enabled")
}

fn summary_json(summary: nas_cache::cache::PruneSummary) -> serde_json::Value {
    serde_json::json!({
        "removed_entries": summary.removed_entries,
        "removed_bytes": summary.removed_bytes,
        "removed_gb": summary.removed_bytes as f64 / 1024.0 / 1024.0 / 1024.0,
        "cache_bytes_before": summary.cache_bytes_before,
        "cache_bytes_after": summary.cache_bytes_after,
        "cache_gb_before": summary.cache_bytes_before as f64 / 1024.0 / 1024.0 / 1024.0,
        "cache_gb_after": summary.cache_bytes_after as f64 / 1024.0 / 1024.0 / 1024.0,
        "free_bytes_before": summary.free_bytes_before,
        "free_bytes_after": summary.free_bytes_after,
    })
}
