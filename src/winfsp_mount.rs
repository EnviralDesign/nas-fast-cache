use std::collections::HashSet;
use std::ffi::c_void;
use std::fs::{self, File, OpenOptions};
use std::io::{Error, ErrorKind};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use winfsp::filesystem::{
    DirBuffer, DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext, OpenFileInfo,
    VolumeInfo, WideNameInfo,
};
use winfsp::host::{FileSystemHost, FileSystemParams, FineGuard, VolumeParams};
use winfsp::{Result as FspResult, U16CStr};
use winfsp_sys::FILE_ACCESS_RIGHTS;

use crate::cache::{
    CacheError, CacheIoStats, CacheReadWindow, DirEntryMeta, ReadThroughCache, SourceFileMeta,
};
use crate::pathing::normalize_relative_path;

const FILE_ATTRIBUTE_READONLY: u32 = 0x0000_0001;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;
const INVALID_FILE_ATTRIBUTES: u32 = 0xFFFF_FFFF;
const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
const FSP_CLEANUP_DELETE: u32 = 0x01;
const FILE_SHARE_READ_WRITE_DELETE: u32 = 0x0000_0001 | 0x0000_0002 | 0x0000_0004;

pub fn mount_readonly(cache: ReadThroughCache, mount: &str, threads: u32) -> anyhow::Result<()> {
    mount_with_options(
        cache,
        mount,
        MountOptions {
            threads,
            enable_writes: false,
            write_prefix: None,
            reuse_write_handles: false,
        },
    )
}

pub struct MountOptions {
    pub threads: u32,
    pub enable_writes: bool,
    pub write_prefix: Option<PathBuf>,
    pub reuse_write_handles: bool,
}

pub fn mount_with_options(
    cache: ReadThroughCache,
    mount: &str,
    mount_options: MountOptions,
) -> anyhow::Result<()> {
    winfsp::winfsp_init_or_die();
    let mut volume = VolumeParams::new();
    volume
        .filesystem_name("NasCache")
        .sector_size(4096)
        .sectors_per_allocation_unit(1)
        .max_component_length(255)
        .case_preserved_names(true)
        .unicode_on_disk(true)
        .read_only_volume(!mount_options.enable_writes)
        .file_info_timeout(u32::MAX)
        .dir_info_timeout(u32::MAX)
        .volume_info_timeout(u32::MAX)
        .security_timeout(u32::MAX)
        .post_cleanup_when_modified_only(true)
        .flush_and_purge_on_cleanup(true);

    let threads = mount_options.threads;
    let enable_writes = mount_options.enable_writes;
    let write_prefix = mount_options.write_prefix;
    let reuse_write_handles = mount_options.reuse_write_handles;
    let options = FileSystemParams::default_params(volume);
    let stats = Arc::new(MountStats::default());
    maybe_spawn_stats_reporter(Arc::clone(&stats));
    let context = ReadThroughFs {
        cache,
        stats,
        enable_writes,
        write_prefix,
        reuse_write_handles,
        dirty_paths: Mutex::new(HashSet::new()),
    };
    let mut host: FileSystemHost<ReadThroughFs, FineGuard> =
        FileSystemHost::new_with_options(options, context)?;
    host.mount(mount.to_string())?;
    eprintln!("nas-fast-cache mounted at {mount}; press Ctrl+C or stop the process to unmount");
    host.start_with_threads(threads)?;
    let running = Arc::new(AtomicBool::new(true));
    let handler_running = Arc::clone(&running);
    ctrlc::set_handler(move || {
        handler_running.store(false, Ordering::SeqCst);
    })?;
    while running.load(Ordering::SeqCst) {
        thread::sleep(std::time::Duration::from_secs(1));
    }
    host.stop();
    host.unmount();
    Ok(())
}

struct ReadThroughFs {
    cache: ReadThroughCache,
    stats: Arc<MountStats>,
    enable_writes: bool,
    write_prefix: Option<PathBuf>,
    reuse_write_handles: bool,
    dirty_paths: Mutex<HashSet<String>>,
}

impl ReadThroughFs {
    fn ensure_write_allowed(&self, rel: &Path) -> FspResult<()> {
        if !self.enable_writes {
            return Err(Error::from(ErrorKind::PermissionDenied).into());
        }
        if let Some(prefix) = &self.write_prefix {
            if !path_starts_with_case_insensitive(rel, prefix) {
                return Err(Error::from(ErrorKind::PermissionDenied).into());
            }
        }
        Ok(())
    }

    fn source_path_for_rel(&self, rel: &Path) -> FspResult<PathBuf> {
        fsp(self.cache.source_path(rel))
    }

    fn open_backing_for_write(&self, rel: &Path) -> FspResult<File> {
        Ok(OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(FILE_SHARE_READ_WRITE_DELETE)
            .open(self.source_path_for_rel(rel)?)?)
    }

    fn refresh_file_handle_info(
        &self,
        rel: &Mutex<PathBuf>,
        meta: &Mutex<SourceFileMeta>,
        file_info: &mut FileInfo,
    ) -> FspResult<()> {
        let rel = rel
            .lock()
            .map_err(|_| Error::other("file path lock poisoned"))?
            .clone();
        let new_meta = fsp(self.cache.stat(&rel))?;
        fill_info(
            file_info,
            false,
            new_meta.len,
            Some(ns_to_system_time(new_meta.modified_ns)),
            Some(ns_to_system_time(new_meta.modified_ns)),
            !self.enable_writes,
        );
        *meta
            .lock()
            .map_err(|_| Error::other("file metadata lock poisoned"))? = new_meta;
        Ok(())
    }

    fn fill_info_for_rel(
        &self,
        rel: &Mutex<PathBuf>,
        is_dir: bool,
        file_info: &mut FileInfo,
    ) -> FspResult<()> {
        let rel = rel
            .lock()
            .map_err(|_| Error::other("path lock poisoned"))?
            .clone();
        let metadata = fs::metadata(self.source_path_for_rel(&rel)?)?;
        fill_info(
            file_info,
            is_dir,
            metadata.len(),
            metadata.created().ok(),
            metadata.modified().ok(),
            !self.enable_writes,
        );
        Ok(())
    }

    fn invalidate_meta_best_effort(&self, meta: &SourceFileMeta) {
        if let Err(err) = self.cache.invalidate_cached_meta(meta) {
            eprintln!("nas-fast-cache cache invalidation failed: {err}");
        }
    }

    fn mark_dirty_path(&self, rel: &Path) -> FspResult<()> {
        self.dirty_paths
            .lock()
            .map_err(|_| Error::other("dirty path lock poisoned"))?
            .insert(path_key(rel));
        Ok(())
    }

    fn clear_dirty_path(&self, rel: &Path) {
        if let Ok(mut dirty_paths) = self.dirty_paths.lock() {
            dirty_paths.remove(&path_key(rel));
        }
    }

    fn is_dirty_path(&self, rel: &Path) -> bool {
        self.dirty_paths
            .lock()
            .map(|dirty_paths| dirty_paths.contains(&path_key(rel)))
            .unwrap_or(false)
    }

    fn begin_file_mutation(
        &self,
        rel: &Path,
        meta: &Mutex<SourceFileMeta>,
        dirty_original_meta: &Mutex<Option<SourceFileMeta>>,
    ) -> FspResult<()> {
        self.mark_dirty_path(rel)?;
        let mut dirty_original_meta = dirty_original_meta
            .lock()
            .map_err(|_| Error::other("dirty metadata lock poisoned"))?;
        if dirty_original_meta.is_none() {
            let original = meta
                .lock()
                .map_err(|_| Error::other("file metadata lock poisoned"))?
                .clone();
            self.invalidate_meta_best_effort(&original);
            *dirty_original_meta = Some(original);
        }
        Ok(())
    }

    fn finish_file_mutation(
        &self,
        rel: &Mutex<PathBuf>,
        meta: &Mutex<SourceFileMeta>,
        dirty_original_meta: &Mutex<Option<SourceFileMeta>>,
        file_info: Option<&mut FileInfo>,
    ) -> FspResult<()> {
        let rel = rel
            .lock()
            .map_err(|_| Error::other("file path lock poisoned"))?
            .clone();
        if let Some(original) = dirty_original_meta
            .lock()
            .map_err(|_| Error::other("dirty metadata lock poisoned"))?
            .take()
        {
            self.invalidate_meta_best_effort(&original);
        }
        let new_meta = fsp(self.cache.stat(&rel))?;
        if let Some(file_info) = file_info {
            fill_info_from_meta(file_info, &new_meta, !self.enable_writes);
        }
        *meta
            .lock()
            .map_err(|_| Error::other("file metadata lock poisoned"))? = new_meta;
        self.clear_dirty_path(&rel);
        Ok(())
    }

    fn finish_file_mutation_best_effort(
        &self,
        rel: &Mutex<PathBuf>,
        meta: &Mutex<SourceFileMeta>,
        dirty_original_meta: &Mutex<Option<SourceFileMeta>>,
    ) {
        let _ = self.finish_file_mutation(rel, meta, dirty_original_meta, None);
    }

    fn clear_deleted_file_dirty_state(
        &self,
        rel: &Mutex<PathBuf>,
        meta: &Mutex<SourceFileMeta>,
        dirty_original_meta: &Mutex<Option<SourceFileMeta>>,
    ) {
        if let Ok(rel) = rel.lock() {
            self.clear_dirty_path(&rel);
        }
        if let Ok(mut dirty_original_meta) = dirty_original_meta.lock() {
            if let Some(original) = dirty_original_meta.take() {
                self.invalidate_meta_best_effort(&original);
            }
        }
        if let Ok(meta) = meta.lock() {
            self.invalidate_meta_best_effort(&meta);
        }
    }

    fn read_source_direct(&self, rel: &Path, buffer: &mut [u8], offset: u64) -> FspResult<u32> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let mut file = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ_WRITE_DELETE)
            .open(self.source_path_for_rel(rel)?)?;
        file.seek(SeekFrom::Start(offset))?;
        Ok(file.read(buffer)? as u32)
    }
}

#[derive(Default)]
struct MountStats {
    read_calls: AtomicU64,
    read_bytes: AtomicU64,
    read_requested_bytes: AtomicU64,
    read_ns: AtomicU64,
    read_max_ns: AtomicU64,
    cache_hits: AtomicU64,
    cache_misses: AtomicU64,
    cache_bytes: AtomicU64,
    source_bytes: AtomicU64,
    source_fetch_bytes: AtomicU64,
    cache_read_ops: AtomicU64,
    source_read_ops: AtomicU64,
    cache_read_ns: AtomicU64,
    cache_read_max_ns: AtomicU64,
    source_read_ns: AtomicU64,
    source_read_max_ns: AtomicU64,
    cache_write_jobs: AtomicU64,
    cache_write_bytes: AtomicU64,
    cache_write_enqueue_ns: AtomicU64,
    heap_allocated_bytes: AtomicU64,
    window_hits: AtomicU64,
    window_fills: AtomicU64,
    prefetch_requests: AtomicU64,
    prefetch_hits: AtomicU64,
    prefetch_waits: AtomicU64,
    prefetch_hit_bytes: AtomicU64,
    demand_wait_ns: AtomicU64,
    callbacks_le_64k: AtomicU64,
    callbacks_le_256k: AtomicU64,
    callbacks_le_1m: AtomicU64,
    callbacks_le_4m: AtomicU64,
    callbacks_gt_4m: AtomicU64,
    write_calls: AtomicU64,
    write_bytes: AtomicU64,
    write_ns: AtomicU64,
    write_max_ns: AtomicU64,
}

impl MountStats {
    fn record_read(&self, requested: u64, stats: &CacheIoStats, elapsed: Duration) {
        self.read_calls.fetch_add(1, Ordering::Relaxed);
        self.read_bytes.fetch_add(stats.bytes, Ordering::Relaxed);
        self.read_requested_bytes
            .fetch_add(requested, Ordering::Relaxed);
        self.read_ns
            .fetch_add(elapsed.as_nanos() as u64, Ordering::Relaxed);
        update_max(&self.read_max_ns, elapsed.as_nanos() as u64);
        self.cache_hits
            .fetch_add(stats.cache_hits, Ordering::Relaxed);
        self.cache_misses
            .fetch_add(stats.cache_misses, Ordering::Relaxed);
        self.cache_bytes
            .fetch_add(stats.cache_bytes, Ordering::Relaxed);
        self.source_bytes
            .fetch_add(stats.source_bytes, Ordering::Relaxed);
        self.source_fetch_bytes
            .fetch_add(stats.source_fetch_bytes, Ordering::Relaxed);
        self.cache_read_ops
            .fetch_add(stats.cache_read_ops, Ordering::Relaxed);
        self.source_read_ops
            .fetch_add(stats.source_read_ops, Ordering::Relaxed);
        self.cache_read_ns
            .fetch_add(stats.cache_read_ns, Ordering::Relaxed);
        update_max(&self.cache_read_max_ns, stats.cache_read_max_ns);
        self.source_read_ns
            .fetch_add(stats.source_read_ns, Ordering::Relaxed);
        update_max(&self.source_read_max_ns, stats.source_read_max_ns);
        self.cache_write_jobs
            .fetch_add(stats.cache_write_jobs, Ordering::Relaxed);
        self.cache_write_bytes
            .fetch_add(stats.cache_write_bytes, Ordering::Relaxed);
        self.cache_write_enqueue_ns
            .fetch_add(stats.cache_write_enqueue_ns, Ordering::Relaxed);
        self.heap_allocated_bytes
            .fetch_add(stats.heap_allocated_bytes, Ordering::Relaxed);
        self.window_hits
            .fetch_add(stats.window_hits, Ordering::Relaxed);
        self.window_fills
            .fetch_add(stats.window_fills, Ordering::Relaxed);
        self.prefetch_requests
            .fetch_add(stats.prefetch_requests, Ordering::Relaxed);
        self.prefetch_hits
            .fetch_add(stats.prefetch_hits, Ordering::Relaxed);
        self.prefetch_waits
            .fetch_add(stats.prefetch_waits, Ordering::Relaxed);
        self.prefetch_hit_bytes
            .fetch_add(stats.prefetch_hit_bytes, Ordering::Relaxed);
        self.demand_wait_ns
            .fetch_add(stats.demand_wait_ns, Ordering::Relaxed);

        match requested {
            0..=65_536 => self.callbacks_le_64k.fetch_add(1, Ordering::Relaxed),
            65_537..=262_144 => self.callbacks_le_256k.fetch_add(1, Ordering::Relaxed),
            262_145..=1_048_576 => self.callbacks_le_1m.fetch_add(1, Ordering::Relaxed),
            1_048_577..=4_194_304 => self.callbacks_le_4m.fetch_add(1, Ordering::Relaxed),
            _ => self.callbacks_gt_4m.fetch_add(1, Ordering::Relaxed),
        };
    }

    fn record_write(&self, bytes: u64, elapsed: Duration) {
        self.write_calls.fetch_add(1, Ordering::Relaxed);
        self.write_bytes.fetch_add(bytes, Ordering::Relaxed);
        self.write_ns
            .fetch_add(elapsed.as_nanos() as u64, Ordering::Relaxed);
        update_max(&self.write_max_ns, elapsed.as_nanos() as u64);
    }

    fn snapshot(&self) -> MountStatsSnapshot {
        MountStatsSnapshot {
            read_calls: self.read_calls.load(Ordering::Relaxed),
            read_bytes: self.read_bytes.load(Ordering::Relaxed),
            read_requested_bytes: self.read_requested_bytes.load(Ordering::Relaxed),
            read_ns: self.read_ns.load(Ordering::Relaxed),
            read_max_ns: self.read_max_ns.load(Ordering::Relaxed),
            cache_hits: self.cache_hits.load(Ordering::Relaxed),
            cache_misses: self.cache_misses.load(Ordering::Relaxed),
            cache_bytes: self.cache_bytes.load(Ordering::Relaxed),
            source_bytes: self.source_bytes.load(Ordering::Relaxed),
            source_fetch_bytes: self.source_fetch_bytes.load(Ordering::Relaxed),
            cache_read_ops: self.cache_read_ops.load(Ordering::Relaxed),
            source_read_ops: self.source_read_ops.load(Ordering::Relaxed),
            cache_read_ns: self.cache_read_ns.load(Ordering::Relaxed),
            cache_read_max_ns: self.cache_read_max_ns.load(Ordering::Relaxed),
            source_read_ns: self.source_read_ns.load(Ordering::Relaxed),
            source_read_max_ns: self.source_read_max_ns.load(Ordering::Relaxed),
            cache_write_jobs: self.cache_write_jobs.load(Ordering::Relaxed),
            cache_write_bytes: self.cache_write_bytes.load(Ordering::Relaxed),
            cache_write_enqueue_ns: self.cache_write_enqueue_ns.load(Ordering::Relaxed),
            heap_allocated_bytes: self.heap_allocated_bytes.load(Ordering::Relaxed),
            window_hits: self.window_hits.load(Ordering::Relaxed),
            window_fills: self.window_fills.load(Ordering::Relaxed),
            prefetch_requests: self.prefetch_requests.load(Ordering::Relaxed),
            prefetch_hits: self.prefetch_hits.load(Ordering::Relaxed),
            prefetch_waits: self.prefetch_waits.load(Ordering::Relaxed),
            prefetch_hit_bytes: self.prefetch_hit_bytes.load(Ordering::Relaxed),
            demand_wait_ns: self.demand_wait_ns.load(Ordering::Relaxed),
            callbacks_le_64k: self.callbacks_le_64k.load(Ordering::Relaxed),
            callbacks_le_256k: self.callbacks_le_256k.load(Ordering::Relaxed),
            callbacks_le_1m: self.callbacks_le_1m.load(Ordering::Relaxed),
            callbacks_le_4m: self.callbacks_le_4m.load(Ordering::Relaxed),
            callbacks_gt_4m: self.callbacks_gt_4m.load(Ordering::Relaxed),
            write_calls: self.write_calls.load(Ordering::Relaxed),
            write_bytes: self.write_bytes.load(Ordering::Relaxed),
            write_ns: self.write_ns.load(Ordering::Relaxed),
            write_max_ns: self.write_max_ns.load(Ordering::Relaxed),
        }
    }
}

struct MountStatsSnapshot {
    read_calls: u64,
    read_bytes: u64,
    read_requested_bytes: u64,
    read_ns: u64,
    read_max_ns: u64,
    cache_hits: u64,
    cache_misses: u64,
    cache_bytes: u64,
    source_bytes: u64,
    source_fetch_bytes: u64,
    cache_read_ops: u64,
    source_read_ops: u64,
    cache_read_ns: u64,
    cache_read_max_ns: u64,
    source_read_ns: u64,
    source_read_max_ns: u64,
    cache_write_jobs: u64,
    cache_write_bytes: u64,
    cache_write_enqueue_ns: u64,
    heap_allocated_bytes: u64,
    window_hits: u64,
    window_fills: u64,
    prefetch_requests: u64,
    prefetch_hits: u64,
    prefetch_waits: u64,
    prefetch_hit_bytes: u64,
    demand_wait_ns: u64,
    callbacks_le_64k: u64,
    callbacks_le_256k: u64,
    callbacks_le_1m: u64,
    callbacks_le_4m: u64,
    callbacks_gt_4m: u64,
    write_calls: u64,
    write_bytes: u64,
    write_ns: u64,
    write_max_ns: u64,
}

impl MountStatsSnapshot {
    fn describe_delta(&self, previous: &Self) -> String {
        let calls = self.read_calls.saturating_sub(previous.read_calls);
        let bytes = self.read_bytes.saturating_sub(previous.read_bytes);
        let requested = self
            .read_requested_bytes
            .saturating_sub(previous.read_requested_bytes);
        let ns = self.read_ns.saturating_sub(previous.read_ns);
        let seconds = (ns as f64 / 1_000_000_000.0).max(0.000_001);
        let mib = bytes as f64 / 1024.0 / 1024.0;
        let avg_kib = if calls == 0 {
            0.0
        } else {
            requested as f64 / calls as f64 / 1024.0
        };
        let source_fetch_bytes = self
            .source_fetch_bytes
            .saturating_sub(previous.source_fetch_bytes);
        let source_fetch_mib = source_fetch_bytes as f64 / 1024.0 / 1024.0;
        let source_fetch_per_app = if bytes == 0 {
            0.0
        } else {
            source_fetch_bytes as f64 / bytes as f64
        };
        let source_read_ns = self.source_read_ns.saturating_sub(previous.source_read_ns);
        let source_read_seconds = source_read_ns as f64 / 1_000_000_000.0;
        let source_fetch_mib_per_sec = if source_read_seconds <= 0.0 {
            0.0
        } else {
            source_fetch_mib / source_read_seconds
        };
        let cache_read_seconds =
            self.cache_read_ns.saturating_sub(previous.cache_read_ns) as f64 / 1_000_000_000.0;
        let write_enqueue_seconds =
            self.cache_write_enqueue_ns
                .saturating_sub(previous.cache_write_enqueue_ns) as f64
                / 1_000_000_000.0;
        let source_busy_ratio = source_read_seconds / seconds;
        let prefetch_hit_mib = self
            .prefetch_hit_bytes
            .saturating_sub(previous.prefetch_hit_bytes) as f64
            / 1024.0
            / 1024.0;
        let demand_wait_seconds =
            self.demand_wait_ns.saturating_sub(previous.demand_wait_ns) as f64 / 1_000_000_000.0;
        let write_calls = self.write_calls.saturating_sub(previous.write_calls);
        let write_mib =
            self.write_bytes.saturating_sub(previous.write_bytes) as f64 / 1024.0 / 1024.0;
        let write_seconds =
            self.write_ns.saturating_sub(previous.write_ns) as f64 / 1_000_000_000.0;
        format!(
            "nas-fast-cache stats read_calls={calls} mib={mib:.2} mib_per_sec={:.2} avg_request_kib={avg_kib:.1} max_callback_ms={:.3} cache_hits={} cache_misses={} cache_mib={:.2} source_app_mib={:.2} source_fetch_mib={source_fetch_mib:.2} source_fetch_per_app={source_fetch_per_app:.3} cache_read_ops={} source_read_ops={} cache_read_seconds={cache_read_seconds:.3} cache_read_max_ms={:.3} source_read_seconds={source_read_seconds:.3} source_read_max_ms={:.3} source_fetch_mib_per_sec={source_fetch_mib_per_sec:.2} source_busy_ratio={source_busy_ratio:.2} cache_write_jobs={} cache_write_mib={:.2} cache_write_enqueue_seconds={write_enqueue_seconds:.3} fs_write_calls={write_calls} fs_write_mib={write_mib:.2} fs_write_seconds={write_seconds:.3} fs_write_mib_per_sec={:.2} fs_write_max_ms={:.3} window_hits={} window_fills={} prefetch_requests={} prefetch_hits={} prefetch_waits={} prefetch_hit_mib={prefetch_hit_mib:.2} demand_wait_seconds={demand_wait_seconds:.3} heap_alloc_mib={:.2} buckets_le64k={} le256k={} le1m={} le4m={} gt4m={}",
            mib / seconds,
            self.read_max_ns as f64 / 1_000_000.0,
            self.cache_hits.saturating_sub(previous.cache_hits),
            self.cache_misses.saturating_sub(previous.cache_misses),
            self.cache_bytes.saturating_sub(previous.cache_bytes) as f64 / 1024.0 / 1024.0,
            self.source_bytes.saturating_sub(previous.source_bytes) as f64 / 1024.0 / 1024.0,
            self.cache_read_ops.saturating_sub(previous.cache_read_ops),
            self.source_read_ops
                .saturating_sub(previous.source_read_ops),
            self.cache_read_max_ns as f64 / 1_000_000.0,
            self.source_read_max_ns as f64 / 1_000_000.0,
            self.cache_write_jobs
                .saturating_sub(previous.cache_write_jobs),
            self.cache_write_bytes
                .saturating_sub(previous.cache_write_bytes) as f64
                / 1024.0
                / 1024.0,
            write_mib / write_seconds.max(0.000_001),
            self.write_max_ns as f64 / 1_000_000.0,
            self.window_hits.saturating_sub(previous.window_hits),
            self.window_fills.saturating_sub(previous.window_fills),
            self.prefetch_requests
                .saturating_sub(previous.prefetch_requests),
            self.prefetch_hits.saturating_sub(previous.prefetch_hits),
            self.prefetch_waits.saturating_sub(previous.prefetch_waits),
            self.heap_allocated_bytes
                .saturating_sub(previous.heap_allocated_bytes) as f64
                / 1024.0
                / 1024.0,
            self.callbacks_le_64k
                .saturating_sub(previous.callbacks_le_64k),
            self.callbacks_le_256k
                .saturating_sub(previous.callbacks_le_256k),
            self.callbacks_le_1m
                .saturating_sub(previous.callbacks_le_1m),
            self.callbacks_le_4m
                .saturating_sub(previous.callbacks_le_4m),
            self.callbacks_gt_4m
                .saturating_sub(previous.callbacks_gt_4m),
        )
    }
}

enum Handle {
    File {
        rel: Mutex<PathBuf>,
        meta: Mutex<SourceFileMeta>,
        window: Mutex<CacheReadWindow>,
        write_file: Mutex<Option<File>>,
        dirty_original_meta: Mutex<Option<SourceFileMeta>>,
        delete_pending: AtomicBool,
    },
    Directory {
        rel: Mutex<PathBuf>,
        buffer: DirBuffer,
        delete_pending: AtomicBool,
    },
}

impl FileSystemContext for ReadThroughFs {
    type FileContext = Handle;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        _security_descriptor: Option<&mut [c_void]>,
        _reparse_point_resolver: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> FspResult<FileSecurity> {
        let rel = path_from_winfsp(file_name)?;
        let source = fsp(self.cache.source_path(&rel))?;
        match std::fs::metadata(source) {
            Ok(metadata) => Ok(FileSecurity {
                reparse: false,
                sz_security_descriptor: 0,
                attributes: attrs_for(metadata.is_dir(), !self.enable_writes),
            }),
            Err(err) if err.kind() == ErrorKind::NotFound && self.enable_writes => {
                self.ensure_write_allowed(&rel)?;
                let parent = rel.parent().unwrap_or_else(|| Path::new(""));
                let parent_source = self.source_path_for_rel(parent)?;
                let parent_metadata = std::fs::metadata(parent_source)?;
                if !parent_metadata.is_dir() {
                    return Err(Error::from(ErrorKind::NotFound).into());
                }
                Ok(FileSecurity {
                    reparse: false,
                    sz_security_descriptor: 0,
                    attributes: FILE_ATTRIBUTE_NORMAL,
                })
            }
            Err(err) => Err(err.into()),
        }
    }

    fn open(
        &self,
        file_name: &U16CStr,
        _create_options: u32,
        _granted_access: FILE_ACCESS_RIGHTS,
        file_info: &mut OpenFileInfo,
    ) -> FspResult<Self::FileContext> {
        let rel = path_from_winfsp(file_name)?;
        let source = fsp(self.cache.source_path(&rel))?;
        let metadata = std::fs::metadata(source)?;
        fill_info(
            file_info.as_mut(),
            metadata.is_dir(),
            metadata.len(),
            metadata.created().ok(),
            metadata.modified().ok(),
            !self.enable_writes,
        );
        if metadata.is_dir() {
            Ok(Handle::Directory {
                rel: Mutex::new(rel),
                buffer: DirBuffer::new(),
                delete_pending: AtomicBool::new(false),
            })
        } else {
            let meta = fsp(self.cache.stat(&rel))?;
            Ok(Handle::File {
                rel: Mutex::new(rel),
                meta: Mutex::new(meta),
                window: Mutex::new(CacheReadWindow::new()),
                write_file: Mutex::new(None),
                dirty_original_meta: Mutex::new(None),
                delete_pending: AtomicBool::new(false),
            })
        }
    }

    fn close(&self, context: Self::FileContext) {
        if let Handle::File {
            rel,
            meta,
            dirty_original_meta,
            ..
        } = context
        {
            self.finish_file_mutation_best_effort(&rel, &meta, &dirty_original_meta);
        }
    }

    fn create(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        _granted_access: FILE_ACCESS_RIGHTS,
        file_attributes: winfsp_sys::FILE_FLAGS_AND_ATTRIBUTES,
        _security_descriptor: Option<&[c_void]>,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        _extra_buffer_is_reparse_point: bool,
        file_info: &mut OpenFileInfo,
    ) -> FspResult<Self::FileContext> {
        let rel = path_from_winfsp(file_name)?;
        self.ensure_write_allowed(&rel)?;
        let source = self.source_path_for_rel(&rel)?;
        let is_dir = create_options & FILE_DIRECTORY_FILE != 0
            || file_attributes & FILE_ATTRIBUTE_DIRECTORY != 0;

        if is_dir {
            fs::create_dir(&source)?;
            let metadata = fs::metadata(&source)?;
            fill_info(
                file_info.as_mut(),
                true,
                0,
                metadata.created().ok(),
                metadata.modified().ok(),
                false,
            );
            Ok(Handle::Directory {
                rel: Mutex::new(rel),
                buffer: DirBuffer::new(),
                delete_pending: AtomicBool::new(false),
            })
        } else {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .share_mode(FILE_SHARE_READ_WRITE_DELETE)
                .open(&source)?;
            let meta = fsp(self.cache.stat(&rel))?;
            fill_info(
                file_info.as_mut(),
                false,
                meta.len,
                Some(ns_to_system_time(meta.modified_ns)),
                Some(ns_to_system_time(meta.modified_ns)),
                false,
            );
            Ok(Handle::File {
                rel: Mutex::new(rel),
                meta: Mutex::new(meta),
                window: Mutex::new(CacheReadWindow::new()),
                write_file: Mutex::new(if self.reuse_write_handles {
                    Some(file)
                } else {
                    drop(file);
                    None
                }),
                dirty_original_meta: Mutex::new(None),
                delete_pending: AtomicBool::new(false),
            })
        }
    }

    fn cleanup(&self, context: &Self::FileContext, _file_name: Option<&U16CStr>, flags: u32) {
        let delete_requested = flags & FSP_CLEANUP_DELETE != 0;
        match context {
            Handle::File {
                rel,
                meta,
                write_file,
                dirty_original_meta,
                delete_pending,
                ..
            } => {
                if let Ok(mut file) = write_file.lock() {
                    if let Some(file) = file.as_mut() {
                        let _ = file.flush();
                    }
                    *file = None;
                }
                let pending = delete_requested || delete_pending.load(Ordering::SeqCst);
                let old_meta = meta.lock().ok().map(|meta| meta.clone());
                if pending {
                    if let Ok(rel) = rel.lock() {
                        if self.ensure_write_allowed(&rel).is_ok() {
                            if let Ok(source) = self.source_path_for_rel(&rel) {
                                if let Err(err) = fs::remove_file(source) {
                                    eprintln!("nas-fast-cache cleanup remove file failed: {err}");
                                }
                            }
                        }
                    }
                }
                if pending {
                    self.clear_deleted_file_dirty_state(rel, meta, dirty_original_meta);
                } else {
                    self.finish_file_mutation_best_effort(rel, meta, dirty_original_meta);
                    if let Some(old_meta) = old_meta {
                        self.invalidate_meta_best_effort(&old_meta);
                    }
                }
            }
            Handle::Directory {
                rel,
                delete_pending,
                ..
            } => {
                let pending = delete_requested || delete_pending.load(Ordering::SeqCst);
                if pending {
                    if let Ok(rel) = rel.lock() {
                        if self.ensure_write_allowed(&rel).is_ok() {
                            if let Ok(source) = self.source_path_for_rel(&rel) {
                                if let Err(err) = remove_empty_dir_with_retry(&source) {
                                    eprintln!(
                                        "nas-fast-cache cleanup remove directory failed: {err}"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    fn flush(
        &self,
        context: Option<&Self::FileContext>,
        file_info: &mut FileInfo,
    ) -> FspResult<()> {
        match context {
            Some(Handle::File {
                rel,
                meta,
                write_file,
                dirty_original_meta,
                ..
            }) => {
                let mut file = write_file
                    .lock()
                    .map_err(|_| Error::other("write file lock poisoned"))?;
                if let Some(file) = file.as_mut() {
                    file.sync_all()?;
                }
                if !self.reuse_write_handles {
                    *file = None;
                }
                self.finish_file_mutation(rel, meta, dirty_original_meta, Some(file_info))?;
            }
            Some(Handle::Directory { rel, .. }) => {
                self.fill_info_for_rel(rel, true, file_info)?;
            }
            None => {
                let metadata = fs::metadata(self.cache.config().source_root.clone())?;
                fill_info(
                    file_info,
                    true,
                    0,
                    metadata.created().ok(),
                    metadata.modified().ok(),
                    !self.enable_writes,
                );
            }
        }
        Ok(())
    }

    fn get_file_info(
        &self,
        context: &Self::FileContext,
        file_info: &mut FileInfo,
    ) -> FspResult<()> {
        match context {
            Handle::File { meta, .. } => {
                let meta = meta
                    .lock()
                    .map_err(|_| Error::other("file metadata lock poisoned"))?;
                fill_info(
                    file_info,
                    false,
                    meta.len,
                    Some(ns_to_system_time(meta.modified_ns)),
                    Some(ns_to_system_time(meta.modified_ns)),
                    !self.enable_writes,
                );
            }
            Handle::Directory { rel, .. } => {
                let rel = rel
                    .lock()
                    .map_err(|_| Error::other("directory path lock poisoned"))?;
                let source = fsp(self.cache.source_path(&*rel))?;
                let metadata = std::fs::metadata(source)?;
                fill_info(
                    file_info,
                    true,
                    metadata.len(),
                    metadata.created().ok(),
                    metadata.modified().ok(),
                    !self.enable_writes,
                );
            }
        }
        Ok(())
    }

    fn overwrite(
        &self,
        context: &Self::FileContext,
        _file_attributes: winfsp_sys::FILE_FLAGS_AND_ATTRIBUTES,
        _replace_file_attributes: bool,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        file_info: &mut FileInfo,
    ) -> FspResult<()> {
        let Handle::File {
            rel,
            meta,
            write_file,
            dirty_original_meta,
            ..
        } = context
        else {
            return Err(Error::from(ErrorKind::IsADirectory).into());
        };
        let rel_guard = rel
            .lock()
            .map_err(|_| Error::other("file path lock poisoned"))?;
        self.ensure_write_allowed(&rel_guard)?;
        self.begin_file_mutation(&rel_guard, meta, dirty_original_meta)?;
        let mut file = write_file
            .lock()
            .map_err(|_| Error::other("write file lock poisoned"))?;
        if file.is_none() {
            *file = Some(self.open_backing_for_write(&rel_guard)?);
        }
        let backing = file
            .as_mut()
            .ok_or_else(|| Error::other("write file missing"))?;
        backing.set_len(0)?;
        backing.seek(SeekFrom::Start(0))?;
        if !self.reuse_write_handles {
            *file = None;
        }
        drop(file);
        update_meta_after_write(meta, 0, file_info, !self.enable_writes)?;
        drop(rel_guard);
        Ok(())
    }

    fn read_directory(
        &self,
        context: &Self::FileContext,
        _pattern: Option<&U16CStr>,
        marker: DirMarker,
        out: &mut [u8],
    ) -> FspResult<u32> {
        let Handle::Directory { rel, buffer, .. } = context else {
            return Err(Error::from(ErrorKind::NotADirectory).into());
        };

        if marker.is_none() {
            let rel = rel
                .lock()
                .map_err(|_| Error::other("directory path lock poisoned"))?;
            let entries = fsp(self.cache.list_dir(&*rel))?;
            let lock = buffer.acquire(true, Some(entries.len() as u32 + 2))?;
            write_dir_entry(&lock, ".", true, 0, None, None, !self.enable_writes)?;
            write_dir_entry(&lock, "..", true, 0, None, None, !self.enable_writes)?;
            for entry in entries {
                write_entry_meta(&lock, entry, !self.enable_writes)?;
            }
        }

        Ok(buffer.read(marker, out))
    }

    fn read(&self, context: &Self::FileContext, buffer: &mut [u8], offset: u64) -> FspResult<u32> {
        let Handle::File {
            rel, meta, window, ..
        } = context
        else {
            return Err(Error::from(ErrorKind::IsADirectory).into());
        };
        let started = Instant::now();
        let requested = buffer.len() as u64;
        let rel = rel
            .lock()
            .map_err(|_| Error::other("file path lock poisoned"))?
            .clone();
        if self.is_dirty_path(&rel) {
            return self.read_source_direct(&rel, buffer, offset);
        }
        let meta = meta
            .lock()
            .map_err(|_| Error::other("file metadata lock poisoned"))?
            .clone();
        let stats = if buffer.len() <= 4 * 1024 * 1024 {
            fsp(self
                .cache
                .read_at_meta_windowed(&rel, &meta, offset, buffer, window))?
        } else {
            fsp(self.cache.read_at_meta(&rel, &meta, offset, buffer))?
        };
        self.stats.record_read(requested, &stats, started.elapsed());
        Ok(stats.bytes as u32)
    }

    fn rename(
        &self,
        context: &Self::FileContext,
        _file_name: &U16CStr,
        new_file_name: &U16CStr,
        replace_if_exists: bool,
    ) -> FspResult<()> {
        let new_rel = path_from_winfsp(new_file_name)?;
        self.ensure_write_allowed(&new_rel)?;

        match context {
            Handle::File {
                rel,
                meta,
                write_file,
                ..
            } => {
                let mut rel_guard = rel
                    .lock()
                    .map_err(|_| Error::other("file path lock poisoned"))?;
                self.ensure_write_allowed(&rel_guard)?;
                let old_source = self.source_path_for_rel(&rel_guard)?;
                let new_source = self.source_path_for_rel(&new_rel)?;
                let old_meta = meta
                    .lock()
                    .map_err(|_| Error::other("file metadata lock poisoned"))?
                    .clone();
                *write_file
                    .lock()
                    .map_err(|_| Error::other("write file lock poisoned"))? = None;
                prepare_rename_target(&new_source, replace_if_exists)?;
                fs::rename(old_source, new_source)?;
                *rel_guard = new_rel.clone();
                self.invalidate_meta_best_effort(&old_meta);
                let new_meta = fsp(self.cache.stat(&new_rel))?;
                *meta
                    .lock()
                    .map_err(|_| Error::other("file metadata lock poisoned"))? = new_meta;
            }
            Handle::Directory { rel, .. } => {
                let mut rel_guard = rel
                    .lock()
                    .map_err(|_| Error::other("directory path lock poisoned"))?;
                self.ensure_write_allowed(&rel_guard)?;
                let old_source = self.source_path_for_rel(&rel_guard)?;
                let new_source = self.source_path_for_rel(&new_rel)?;
                prepare_rename_target(&new_source, replace_if_exists)?;
                fs::rename(old_source, new_source)?;
                *rel_guard = new_rel;
            }
        }
        Ok(())
    }

    fn set_basic_info(
        &self,
        context: &Self::FileContext,
        file_attributes: u32,
        _creation_time: u64,
        _last_access_time: u64,
        _last_write_time: u64,
        _last_change_time: u64,
        file_info: &mut FileInfo,
    ) -> FspResult<()> {
        if file_attributes != INVALID_FILE_ATTRIBUTES
            && file_attributes & FILE_ATTRIBUTE_READONLY != 0
        {
            return Err(Error::from(ErrorKind::PermissionDenied).into());
        }
        match context {
            Handle::File { rel, meta, .. } => {
                self.refresh_file_handle_info(rel, meta, file_info)?
            }
            Handle::Directory { rel, .. } => self.fill_info_for_rel(rel, true, file_info)?,
        }
        Ok(())
    }

    fn set_delete(
        &self,
        context: &Self::FileContext,
        _file_name: &U16CStr,
        delete_file: bool,
    ) -> FspResult<()> {
        match context {
            Handle::File {
                rel,
                delete_pending,
                ..
            } => {
                let rel = rel
                    .lock()
                    .map_err(|_| Error::other("file path lock poisoned"))?;
                self.ensure_write_allowed(&rel)?;
                delete_pending.store(delete_file, Ordering::SeqCst);
            }
            Handle::Directory {
                rel,
                delete_pending,
                ..
            } => {
                let rel = rel
                    .lock()
                    .map_err(|_| Error::other("directory path lock poisoned"))?;
                self.ensure_write_allowed(&rel)?;
                if delete_file {
                    let source = self.source_path_for_rel(&rel)?;
                    if fs::read_dir(source)?.next().is_some() {
                        return Err(Error::from(ErrorKind::DirectoryNotEmpty).into());
                    }
                }
                delete_pending.store(delete_file, Ordering::SeqCst);
            }
        }
        Ok(())
    }

    fn set_file_size(
        &self,
        context: &Self::FileContext,
        new_size: u64,
        _set_allocation_size: bool,
        file_info: &mut FileInfo,
    ) -> FspResult<()> {
        let Handle::File {
            rel,
            meta,
            write_file,
            dirty_original_meta,
            ..
        } = context
        else {
            return Err(Error::from(ErrorKind::IsADirectory).into());
        };
        let rel_guard = rel
            .lock()
            .map_err(|_| Error::other("file path lock poisoned"))?;
        self.ensure_write_allowed(&rel_guard)?;
        self.begin_file_mutation(&rel_guard, meta, dirty_original_meta)?;
        let mut file = write_file
            .lock()
            .map_err(|_| Error::other("write file lock poisoned"))?;
        if file.is_none() {
            *file = Some(self.open_backing_for_write(&rel_guard)?);
        }
        file.as_ref()
            .ok_or_else(|| Error::other("write file missing"))?
            .set_len(new_size)?;
        if !self.reuse_write_handles {
            *file = None;
        }
        drop(file);
        update_meta_after_write(meta, new_size, file_info, !self.enable_writes)?;
        drop(rel_guard);
        Ok(())
    }

    fn write(
        &self,
        context: &Self::FileContext,
        buffer: &[u8],
        offset: u64,
        write_to_eof: bool,
        constrained_io: bool,
        file_info: &mut FileInfo,
    ) -> FspResult<u32> {
        let Handle::File {
            rel,
            meta,
            write_file,
            dirty_original_meta,
            ..
        } = context
        else {
            return Err(Error::from(ErrorKind::IsADirectory).into());
        };
        let rel_guard = rel
            .lock()
            .map_err(|_| Error::other("file path lock poisoned"))?;
        self.ensure_write_allowed(&rel_guard)?;
        self.begin_file_mutation(&rel_guard, meta, dirty_original_meta)?;
        let current_len = meta
            .lock()
            .map_err(|_| Error::other("file metadata lock poisoned"))?
            .len;
        let mut file = write_file
            .lock()
            .map_err(|_| Error::other("write file lock poisoned"))?;
        if file.is_none() {
            *file = Some(self.open_backing_for_write(&rel_guard)?);
        }
        let backing = file
            .as_mut()
            .ok_or_else(|| Error::other("write file missing"))?;
        let write_offset = if write_to_eof { current_len } else { offset };
        let writable_len = if constrained_io {
            if write_offset >= current_len {
                0
            } else {
                std::cmp::min(buffer.len() as u64, current_len - write_offset) as usize
            }
        } else {
            buffer.len()
        };
        if writable_len > 0 {
            let started = Instant::now();
            backing.seek(SeekFrom::Start(write_offset))?;
            backing.write_all(&buffer[..writable_len])?;
            self.stats
                .record_write(writable_len as u64, started.elapsed());
        }
        let new_len = if constrained_io {
            current_len
        } else {
            current_len.max(write_offset.saturating_add(writable_len as u64))
        };
        if !self.reuse_write_handles {
            *file = None;
        }
        drop(file);
        update_meta_after_write(meta, new_len, file_info, !self.enable_writes)?;
        drop(rel_guard);
        Ok(writable_len as u32)
    }

    fn get_volume_info(&self, out_volume_info: &mut VolumeInfo) -> FspResult<()> {
        out_volume_info.total_size = 16 * 1024_u64.pow(4);
        out_volume_info.free_size = 8 * 1024_u64.pow(4);
        out_volume_info.set_volume_label("NasCache");
        Ok(())
    }
}

fn path_from_winfsp(file_name: &U16CStr) -> FspResult<PathBuf> {
    let text = file_name.to_string_lossy();
    let trimmed = text.trim_start_matches(['\\', '/']);
    normalize_relative_path(Path::new(trimmed))
        .map_err(|_| Error::from(ErrorKind::InvalidInput).into())
}

fn fsp<T>(result: crate::cache::Result<T>) -> FspResult<T> {
    result.map_err(|err| match err {
        CacheError::Io(err) => err.into(),
        CacheError::Path(_) => Error::from(ErrorKind::InvalidInput).into(),
        CacheError::Json(_) => Error::from(ErrorKind::InvalidData).into(),
        CacheError::OffsetBeyondEof { .. } => Error::from(ErrorKind::InvalidInput).into(),
    })
}

fn maybe_spawn_stats_reporter(stats: Arc<MountStats>) {
    let Ok(raw) = std::env::var("NAS_CACHE_STATS_SECONDS") else {
        return;
    };
    let Ok(seconds) = raw.parse::<u64>() else {
        return;
    };
    if seconds == 0 {
        return;
    }

    thread::Builder::new()
        .name("nas-fast-cache-stats".to_string())
        .spawn(move || {
            let interval = Duration::from_secs(seconds);
            let mut previous = stats.snapshot();
            loop {
                thread::sleep(interval);
                let current = stats.snapshot();
                eprintln!("{}", current.describe_delta(&previous));
                previous = current;
            }
        })
        .expect("failed to spawn nas-fast-cache stats reporter");
}

fn update_max(atom: &AtomicU64, value: u64) {
    let mut current = atom.load(Ordering::Relaxed);
    while value > current {
        match atom.compare_exchange_weak(current, value, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(next) => current = next,
        }
    }
}

fn ns_to_system_time(ns: u128) -> SystemTime {
    UNIX_EPOCH + Duration::from_nanos(ns.min(u64::MAX as u128) as u64)
}

fn write_entry_meta(
    lock: &winfsp::filesystem::DirBufferLock<'_>,
    entry: DirEntryMeta,
    read_only: bool,
) -> FspResult<()> {
    write_dir_entry(
        lock,
        entry.name,
        entry.is_dir,
        entry.len,
        Some(entry.created),
        Some(entry.modified),
        read_only,
    )
}

fn write_dir_entry(
    lock: &winfsp::filesystem::DirBufferLock<'_>,
    name: impl AsRef<std::ffi::OsStr>,
    is_dir: bool,
    len: u64,
    created: Option<SystemTime>,
    modified: Option<SystemTime>,
    read_only: bool,
) -> FspResult<()> {
    let mut info: DirInfo<512> = DirInfo::new();
    fill_info(
        info.file_info_mut(),
        is_dir,
        len,
        created,
        modified,
        read_only,
    );
    let name = name.as_ref().encode_wide().collect::<Vec<_>>();
    info.set_name_raw(name.as_slice())?;
    lock.write(&mut info)
}

fn fill_info(
    info: &mut FileInfo,
    is_dir: bool,
    len: u64,
    created: Option<SystemTime>,
    modified: Option<SystemTime>,
    read_only: bool,
) {
    info.file_attributes = attrs_for(is_dir, read_only);
    info.reparse_tag = 0;
    info.file_size = if is_dir { 0 } else { len };
    info.allocation_size = allocation_size(info.file_size);
    let modified = modified.unwrap_or_else(SystemTime::now);
    let created = created.unwrap_or(modified);
    let modified_filetime = system_time_to_filetime(modified);
    info.creation_time = system_time_to_filetime(created);
    info.last_access_time = modified_filetime;
    info.last_write_time = modified_filetime;
    info.change_time = modified_filetime;
    info.index_number = 0;
    info.hard_links = 0;
    info.ea_size = 0;
}

fn fill_info_from_meta(info: &mut FileInfo, meta: &SourceFileMeta, read_only: bool) {
    fill_info(
        info,
        false,
        meta.len,
        Some(ns_to_system_time(meta.modified_ns)),
        Some(ns_to_system_time(meta.modified_ns)),
        read_only,
    );
}

fn update_meta_after_write(
    meta: &Mutex<SourceFileMeta>,
    len: u64,
    file_info: &mut FileInfo,
    read_only: bool,
) -> FspResult<()> {
    let mut meta = meta
        .lock()
        .map_err(|_| Error::other("file metadata lock poisoned"))?;
    meta.len = len;
    meta.modified_ns = system_time_ns(SystemTime::now());
    fill_info_from_meta(file_info, &meta, read_only);
    Ok(())
}

fn attrs_for(is_dir: bool, read_only: bool) -> u32 {
    let attrs = if is_dir {
        FILE_ATTRIBUTE_DIRECTORY
    } else {
        FILE_ATTRIBUTE_NORMAL
    };
    if read_only {
        attrs | FILE_ATTRIBUTE_READONLY
    } else {
        attrs
    }
}

fn prepare_rename_target(path: &Path, replace_if_exists: bool) -> FspResult<()> {
    if !path.exists() {
        return Ok(());
    }
    if !replace_if_exists {
        return Err(Error::from(ErrorKind::AlreadyExists).into());
    }
    let metadata = fs::metadata(path)?;
    if metadata.is_dir() {
        fs::remove_dir(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn remove_empty_dir_with_retry(path: &Path) -> std::io::Result<()> {
    let mut last_err = None;
    for _ in 0..10 {
        match fs::remove_dir(path) {
            Ok(()) => return Ok(()),
            Err(err) if err.kind() == ErrorKind::DirectoryNotEmpty => {
                last_err = Some(err);
                thread::sleep(Duration::from_millis(50));
            }
            Err(err) => return Err(err),
        }
    }
    Err(last_err.unwrap_or_else(|| Error::from(ErrorKind::DirectoryNotEmpty)))
}

fn path_key(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase()
}

fn path_starts_with_case_insensitive(path: &Path, prefix: &Path) -> bool {
    let mut path_components = path.components();
    for prefix_component in prefix.components() {
        let Some(path_component) = path_components.next() else {
            return false;
        };
        let (std::path::Component::Normal(path_part), std::path::Component::Normal(prefix_part)) =
            (path_component, prefix_component)
        else {
            return false;
        };
        if !path_part
            .to_string_lossy()
            .eq_ignore_ascii_case(&prefix_part.to_string_lossy())
        {
            return false;
        }
    }
    true
}

fn allocation_size(len: u64) -> u64 {
    if len == 0 {
        0
    } else {
        ((len + 4095) / 4096) * 4096
    }
}

fn system_time_to_filetime(time: SystemTime) -> u64 {
    const WINDOWS_TICK: u64 = 10_000_000;
    const SEC_TO_UNIX_EPOCH: u64 = 11_644_473_600;
    let duration = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    (duration.as_secs() + SEC_TO_UNIX_EPOCH) * WINDOWS_TICK
        + u64::from(duration.subsec_nanos() / 100)
}

fn system_time_ns(time: SystemTime) -> u128 {
    time.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}
