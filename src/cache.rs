use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::pathing::normalize_relative_path;

const DEFAULT_CHUNK_SIZE: u64 = 8 * 1024 * 1024;
#[cfg(windows)]
const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;
#[cfg(windows)]
const FILE_SHARE_READ_WRITE_DELETE: u32 = 0x0000_0001 | 0x0000_0002 | 0x0000_0004;
const PREFETCH_LOOKAHEAD_CHUNKS: u64 = 2;
const MAX_PREFETCH_IN_FLIGHT: usize = 2;
const MAX_PREFETCH_READY_CHUNKS: usize = 4;

#[derive(Debug, Error)]
pub enum CacheError {
    #[error("path error: {0}")]
    Path(#[from] crate::pathing::PathError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("offset {offset} is beyond file length {len}")]
    OffsetBeyondEof { offset: u64, len: u64 },
}

pub type Result<T> = std::result::Result<T, CacheError>;

#[derive(Clone, Debug)]
pub struct CacheConfig {
    pub source_root: PathBuf,
    pub cache_root: PathBuf,
    pub chunk_size: u64,
    pub write_cache: bool,
    pub enable_sequential_conveyor: bool,
}

impl CacheConfig {
    pub fn new(source_root: PathBuf, cache_root: PathBuf) -> Self {
        Self {
            source_root,
            cache_root,
            chunk_size: DEFAULT_CHUNK_SIZE,
            write_cache: true,
            enable_sequential_conveyor: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SourceFileMeta {
    pub rel_path: String,
    pub source_path: String,
    pub len: u64,
    pub modified_ns: u128,
    pub chunk_size: u64,
    pub cache_key: String,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CacheIoStats {
    pub bytes: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub cache_bytes: u64,
    pub source_bytes: u64,
    pub source_fetch_bytes: u64,
    pub cache_read_ops: u64,
    pub source_read_ops: u64,
    pub cache_read_ns: u64,
    pub cache_read_max_ns: u64,
    pub source_read_ns: u64,
    pub source_read_max_ns: u64,
    pub cache_write_jobs: u64,
    pub cache_write_bytes: u64,
    pub cache_write_enqueue_ns: u64,
    pub heap_allocated_bytes: u64,
    pub window_hits: u64,
    pub window_fills: u64,
    pub prefetch_requests: u64,
    pub prefetch_hits: u64,
    pub prefetch_waits: u64,
    pub prefetch_hit_bytes: u64,
    pub demand_wait_ns: u64,
}

impl CacheIoStats {
    pub fn merge(&mut self, other: CacheIoStats) {
        self.bytes += other.bytes;
        self.cache_hits += other.cache_hits;
        self.cache_misses += other.cache_misses;
        self.cache_bytes += other.cache_bytes;
        self.source_bytes += other.source_bytes;
        self.source_fetch_bytes += other.source_fetch_bytes;
        self.cache_read_ops += other.cache_read_ops;
        self.source_read_ops += other.source_read_ops;
        self.cache_read_ns += other.cache_read_ns;
        self.cache_read_max_ns = self.cache_read_max_ns.max(other.cache_read_max_ns);
        self.source_read_ns += other.source_read_ns;
        self.source_read_max_ns = self.source_read_max_ns.max(other.source_read_max_ns);
        self.cache_write_jobs += other.cache_write_jobs;
        self.cache_write_bytes += other.cache_write_bytes;
        self.cache_write_enqueue_ns += other.cache_write_enqueue_ns;
        self.heap_allocated_bytes += other.heap_allocated_bytes;
        self.window_hits += other.window_hits;
        self.window_fills += other.window_fills;
        self.prefetch_requests += other.prefetch_requests;
        self.prefetch_hits += other.prefetch_hits;
        self.prefetch_waits += other.prefetch_waits;
        self.prefetch_hit_bytes += other.prefetch_hit_bytes;
        self.demand_wait_ns += other.demand_wait_ns;
    }
}

pub struct CacheReadWindow {
    chunk_index: Option<u64>,
    bytes: Option<Arc<[u8]>>,
    from_cache: bool,
    last_read_end: Option<u64>,
    sequential_reads: u32,
    conveyor: SequentialConveyor,
    source_file: Option<WindowSourceFile>,
}

impl CacheReadWindow {
    pub fn new() -> Self {
        Self {
            chunk_index: None,
            bytes: None,
            from_cache: false,
            last_read_end: None,
            sequential_reads: 0,
            conveyor: SequentialConveyor::new(),
            source_file: None,
        }
    }

    fn observe_read(&mut self, offset: u64, len: u64) -> bool {
        let sequential = self.last_read_end.map(|end| offset == end).unwrap_or(true);
        if sequential {
            self.sequential_reads = self.sequential_reads.saturating_add(1);
        } else {
            self.sequential_reads = 0;
            self.conveyor.reset();
        }
        self.last_read_end = Some(offset.saturating_add(len));
        sequential && self.sequential_reads >= 1
    }

    fn source_file(
        &mut self,
        cache: &ReadThroughCache,
        rel_path: &Path,
        meta: &SourceFileMeta,
    ) -> Result<&mut File> {
        let needs_open = self
            .source_file
            .as_ref()
            .map(|source| {
                source.rel_path.as_path() != rel_path || source.cache_key != meta.cache_key
            })
            .unwrap_or(true);
        if needs_open {
            let source_path = cache.config.source_root.join(rel_path);
            self.source_file = Some(WindowSourceFile {
                rel_path: rel_path.to_path_buf(),
                cache_key: meta.cache_key.clone(),
                file: open_source_sequential(&source_path)?,
            });
        }
        Ok(&mut self
            .source_file
            .as_mut()
            .expect("window source file was just opened")
            .file)
    }
}

struct WindowSourceFile {
    rel_path: PathBuf,
    cache_key: String,
    file: File,
}

struct SequentialConveyor {
    tx: Option<Sender<PrefetchJob>>,
    rx: Option<Receiver<PrefetchResult>>,
    rel_path: Option<PathBuf>,
    cache_key: Option<String>,
    scheduled: HashSet<u64>,
    ready: HashMap<u64, CachedChunk>,
}

impl SequentialConveyor {
    fn new() -> Self {
        Self {
            tx: None,
            rx: None,
            rel_path: None,
            cache_key: None,
            scheduled: HashSet::new(),
            ready: HashMap::new(),
        }
    }

    fn reset(&mut self) {
        self.tx = None;
        self.rx = None;
        self.rel_path = None;
        self.cache_key = None;
        self.scheduled.clear();
        self.ready.clear();
    }

    fn ensure_started(&mut self, cache: &ReadThroughCache, rel_path: &Path, meta: &SourceFileMeta) {
        if self.tx.is_some()
            && self.rel_path.as_deref() == Some(rel_path)
            && self.cache_key.as_deref() == Some(meta.cache_key.as_str())
        {
            return;
        }

        self.reset();
        let (job_tx, job_rx) = mpsc::channel::<PrefetchJob>();
        let (result_tx, result_rx) = mpsc::channel::<PrefetchResult>();
        let worker_cache = cache.clone();
        let worker_rel = rel_path.to_path_buf();
        let worker_meta = meta.clone();
        thread_spawn_prefetch_worker(
            worker_cache,
            worker_rel.clone(),
            worker_meta,
            job_rx,
            result_tx,
        );
        self.tx = Some(job_tx);
        self.rx = Some(result_rx);
        self.rel_path = Some(worker_rel);
        self.cache_key = Some(meta.cache_key.clone());
    }

    fn drain_ready(&mut self) {
        let Some(rx) = &self.rx else {
            return;
        };
        loop {
            match rx.try_recv() {
                Ok(result) => {
                    self.scheduled.remove(&result.chunk_index);
                    if let Ok(chunk) = result.result {
                        if self.ready.len() < MAX_PREFETCH_READY_CHUNKS {
                            self.ready.insert(result.chunk_index, chunk);
                        }
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.tx = None;
                    self.rx = None;
                    self.scheduled.clear();
                    break;
                }
            }
        }
    }

    fn take_ready(&mut self, chunk_index: u64) -> Option<CachedChunk> {
        self.drain_ready();
        self.ready.remove(&chunk_index)
    }

    fn wait_for(&mut self, chunk_index: u64) -> Option<CachedChunk> {
        let Some(rx) = &self.rx else {
            return None;
        };
        while self.scheduled.contains(&chunk_index) {
            match rx.recv() {
                Ok(result) => {
                    self.scheduled.remove(&result.chunk_index);
                    match result.result {
                        Ok(chunk) if result.chunk_index == chunk_index => return Some(chunk),
                        Ok(chunk) => {
                            if self.ready.len() < MAX_PREFETCH_READY_CHUNKS {
                                self.ready.insert(result.chunk_index, chunk);
                            }
                        }
                        Err(_) => {
                            if result.chunk_index == chunk_index {
                                return None;
                            }
                        }
                    }
                }
                Err(_) => {
                    self.tx = None;
                    self.rx = None;
                    self.scheduled.clear();
                    return None;
                }
            }
        }
        None
    }

    fn is_scheduled(&self, chunk_index: u64) -> bool {
        self.scheduled.contains(&chunk_index)
    }

    fn schedule_lookahead(
        &mut self,
        cache: &ReadThroughCache,
        rel_path: &Path,
        meta: &SourceFileMeta,
        current_chunk: u64,
    ) -> u64 {
        if !cache.config.enable_sequential_conveyor {
            return 0;
        }
        self.ensure_started(cache, rel_path, meta);
        self.drain_ready();

        let mut scheduled = 0u64;
        for delta in 1..=PREFETCH_LOOKAHEAD_CHUNKS {
            if self.scheduled.len() >= MAX_PREFETCH_IN_FLIGHT {
                break;
            }
            if self.ready.len() >= MAX_PREFETCH_READY_CHUNKS {
                break;
            }
            let chunk_index = current_chunk + delta;
            if chunk_index * meta.chunk_size >= meta.len {
                break;
            }
            if self.ready.contains_key(&chunk_index) || self.scheduled.contains(&chunk_index) {
                continue;
            }
            if cache.chunk_path(meta, chunk_index).exists() {
                continue;
            }
            let Some(tx) = &self.tx else {
                break;
            };
            if tx.send(PrefetchJob { chunk_index }).is_err() {
                self.tx = None;
                self.rx = None;
                self.scheduled.clear();
                break;
            }
            self.scheduled.insert(chunk_index);
            scheduled += 1;
        }
        scheduled
    }
}

struct PrefetchJob {
    chunk_index: u64,
}

struct PrefetchResult {
    chunk_index: u64,
    result: Result<CachedChunk>,
}

fn thread_spawn_prefetch_worker(
    cache: ReadThroughCache,
    rel_path: PathBuf,
    meta: SourceFileMeta,
    job_rx: Receiver<PrefetchJob>,
    result_tx: Sender<PrefetchResult>,
) {
    std::thread::Builder::new()
        .name("nas-fast-cache-prefetch".to_string())
        .spawn(move || {
            let source_path = cache.config.source_root.join(&rel_path);
            let mut source = match open_source_sequential(&source_path) {
                Ok(source) => source,
                Err(err) => {
                    for job in job_rx {
                        let _ = result_tx.send(PrefetchResult {
                            chunk_index: job.chunk_index,
                            result: Err(CacheError::Io(std::io::Error::new(
                                err.kind(),
                                err.to_string(),
                            ))),
                        });
                    }
                    return;
                }
            };
            for job in job_rx {
                let result = if cache.chunk_path(&meta, job.chunk_index).exists() {
                    cache.load_chunk(&rel_path, &meta, job.chunk_index)
                } else {
                    cache.load_source_chunk_from_file(&mut source, &meta, job.chunk_index)
                };
                if result_tx
                    .send(PrefetchResult {
                        chunk_index: job.chunk_index,
                        result,
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .expect("failed to spawn nas-fast-cache prefetch worker");
}

#[derive(Clone)]
pub struct ReadThroughCache {
    config: CacheConfig,
    writer: CacheWriter,
}

impl ReadThroughCache {
    pub fn new(config: CacheConfig) -> Self {
        let chunk_size = if config.chunk_size == 0 {
            DEFAULT_CHUNK_SIZE
        } else {
            config.chunk_size
        };
        Self {
            config: CacheConfig {
                chunk_size,
                ..config
            },
            writer: CacheWriter::new(),
        }
    }

    pub fn config(&self) -> &CacheConfig {
        &self.config
    }

    pub fn source_path(&self, rel_path: impl AsRef<Path>) -> Result<PathBuf> {
        let rel_path = normalize_relative_path(rel_path)?;
        Ok(self.config.source_root.join(rel_path))
    }

    pub fn stat(&self, rel_path: impl AsRef<Path>) -> Result<SourceFileMeta> {
        let rel_path = normalize_relative_path(rel_path)?;
        let source_path = self.config.source_root.join(&rel_path);
        let metadata = fs::metadata(&source_path)?;
        let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
        let modified_ns = system_time_ns(modified);
        let source_path_str = source_path.to_string_lossy().to_string();
        let rel_path_str = rel_path.to_string_lossy().replace('\\', "/");
        let mut hasher = blake3::Hasher::new();
        hasher.update(source_path_str.to_ascii_lowercase().as_bytes());
        hasher.update(&metadata.len().to_le_bytes());
        hasher.update(&modified_ns.to_le_bytes());
        hasher.update(&self.config.chunk_size.to_le_bytes());
        let cache_key = hasher.finalize().to_hex().to_string();
        Ok(SourceFileMeta {
            rel_path: rel_path_str,
            source_path: source_path_str,
            len: metadata.len(),
            modified_ns,
            chunk_size: self.config.chunk_size,
            cache_key,
        })
    }

    pub fn list_dir(&self, rel_path: impl AsRef<Path>) -> Result<Vec<DirEntryMeta>> {
        let rel_path = normalize_relative_path(rel_path)?;
        let source_dir = self.config.source_root.join(rel_path);
        let mut entries = Vec::new();
        for entry in fs::read_dir(source_dir)? {
            let entry = entry?;
            let metadata = entry.metadata()?;
            entries.push(DirEntryMeta {
                name: entry.file_name().to_string_lossy().to_string(),
                is_dir: metadata.is_dir(),
                len: metadata.len(),
                modified: metadata.modified().unwrap_or(UNIX_EPOCH),
                created: metadata
                    .created()
                    .unwrap_or_else(|_| metadata.modified().unwrap_or(UNIX_EPOCH)),
            });
        }
        entries.sort_by(|a, b| {
            b.is_dir.cmp(&a.is_dir).then_with(|| {
                a.name
                    .to_ascii_lowercase()
                    .cmp(&b.name.to_ascii_lowercase())
            })
        });
        Ok(entries)
    }

    pub fn read_at(
        &self,
        rel_path: impl AsRef<Path>,
        offset: u64,
        buffer: &mut [u8],
    ) -> Result<CacheIoStats> {
        let rel_path = normalize_relative_path(rel_path)?;
        let meta = self.stat(&rel_path)?;
        self.read_at_meta(&rel_path, &meta, offset, buffer)
    }

    pub fn read_at_meta(
        &self,
        rel_path: impl AsRef<Path>,
        meta: &SourceFileMeta,
        offset: u64,
        buffer: &mut [u8],
    ) -> Result<CacheIoStats> {
        self.read_at_meta_inner(rel_path, meta, offset, buffer, None)
    }

    pub fn read_at_meta_windowed(
        &self,
        rel_path: impl AsRef<Path>,
        meta: &SourceFileMeta,
        offset: u64,
        buffer: &mut [u8],
        window: &Mutex<CacheReadWindow>,
    ) -> Result<CacheIoStats> {
        self.read_at_meta_inner(rel_path, meta, offset, buffer, Some(window))
    }

    fn read_at_meta_inner(
        &self,
        rel_path: impl AsRef<Path>,
        meta: &SourceFileMeta,
        offset: u64,
        buffer: &mut [u8],
        window: Option<&Mutex<CacheReadWindow>>,
    ) -> Result<CacheIoStats> {
        if buffer.is_empty() {
            return Ok(CacheIoStats::default());
        }
        let rel_path = normalize_relative_path(rel_path)?;
        if offset >= meta.len {
            return Ok(CacheIoStats::default());
        }

        let mut copied = 0usize;
        let mut cursor = offset;
        let mut stats = CacheIoStats::default();
        let requested = std::cmp::min(buffer.len() as u64, meta.len - offset) as usize;

        while copied < requested {
            let chunk_index = cursor / meta.chunk_size;
            let chunk_offset = (cursor % meta.chunk_size) as usize;
            let chunk_start = chunk_index * meta.chunk_size;
            let chunk_len = std::cmp::min(meta.chunk_size, meta.len - chunk_start) as usize;
            let available = chunk_len.saturating_sub(chunk_offset);
            if available == 0 {
                break;
            }
            let take = std::cmp::min(available, requested - copied);
            if let Some(window) = window {
                let mut guard = window
                    .lock()
                    .map_err(|_| std::io::Error::other("cache read window poisoned"))?;
                let sequential_candidate = if self.config.enable_sequential_conveyor {
                    guard.observe_read(cursor, take as u64)
                } else {
                    false
                };
                if guard.chunk_index == Some(chunk_index) {
                    let bytes = guard
                        .bytes
                        .as_ref()
                        .ok_or_else(|| std::io::Error::other("cache read window missing bytes"))?;
                    stats.window_hits += 1;
                    buffer[copied..copied + take]
                        .copy_from_slice(&bytes[chunk_offset..chunk_offset + take]);
                    stats.source_bytes += take as u64;
                    stats.cache_misses += 1;
                } else {
                    let chunk_path = self.chunk_path(meta, chunk_index);
                    if chunk_path.exists() {
                        let read_ns = read_file_range_exact_timed(
                            &chunk_path,
                            chunk_offset as u64,
                            &mut buffer[copied..copied + take],
                        )?;
                        guard.chunk_index = None;
                        guard.bytes = None;
                        guard.from_cache = false;
                        stats.cache_bytes += take as u64;
                        stats.cache_hits += 1;
                        stats.cache_read_ops += 1;
                        stats.cache_read_ns += read_ns;
                        stats.cache_read_max_ns = stats.cache_read_max_ns.max(read_ns);
                    } else {
                        let mut chunk = None;
                        if sequential_candidate {
                            guard.conveyor.ensure_started(self, &rel_path, meta);
                            stats.prefetch_requests += guard.conveyor.schedule_lookahead(
                                self,
                                &rel_path,
                                meta,
                                chunk_index,
                            );
                            if let Some(prefetched) = guard.conveyor.take_ready(chunk_index) {
                                chunk = Some(prefetched);
                                stats.prefetch_hits += 1;
                            } else if guard.conveyor.is_scheduled(chunk_index) {
                                let wait_started = Instant::now();
                                stats.prefetch_waits += 1;
                                chunk = guard.conveyor.wait_for(chunk_index);
                                stats.demand_wait_ns += elapsed_ns(wait_started);
                                if chunk.is_some() {
                                    stats.prefetch_hits += 1;
                                }
                            }
                        }
                        let chunk = match chunk {
                            Some(chunk) => {
                                stats.prefetch_hit_bytes += chunk.bytes.len() as u64;
                                chunk
                            }
                            None => {
                                let source = guard.source_file(self, &rel_path, meta)?;
                                self.load_source_chunk_from_file(source, meta, chunk_index)?
                            }
                        };
                        stats.merge(chunk.stats);
                        guard.bytes = Some(Arc::clone(&chunk.bytes));
                        guard.from_cache = false;
                        guard.chunk_index = Some(chunk_index);
                        stats.window_fills += 1;
                        let bytes = guard
                            .bytes
                            .as_ref()
                            .expect("cache read window was just filled");
                        buffer[copied..copied + take]
                            .copy_from_slice(&bytes[chunk_offset..chunk_offset + take]);
                        stats.source_bytes += take as u64;
                        stats.cache_misses += 1;
                        if sequential_candidate {
                            stats.prefetch_requests += guard.conveyor.schedule_lookahead(
                                self,
                                &rel_path,
                                meta,
                                chunk_index,
                            );
                        }
                    }
                }
            } else {
                let chunk_path = self.chunk_path(meta, chunk_index);
                if chunk_path.exists() {
                    let read_ns = read_file_range_exact_timed(
                        &chunk_path,
                        chunk_offset as u64,
                        &mut buffer[copied..copied + take],
                    )?;
                    stats.cache_bytes += take as u64;
                    stats.cache_hits += 1;
                    stats.cache_read_ops += 1;
                    stats.cache_read_ns += read_ns;
                    stats.cache_read_max_ns = stats.cache_read_max_ns.max(read_ns);
                } else {
                    let chunk = self.load_chunk(&rel_path, meta, chunk_index)?;
                    buffer[copied..copied + take]
                        .copy_from_slice(&chunk.bytes[chunk_offset..chunk_offset + take]);
                    stats.merge(chunk.stats);
                    stats.source_bytes += take as u64;
                    stats.cache_misses += 1;
                }
            }
            copied += take;
            cursor += take as u64;
            stats.bytes += take as u64;
        }

        Ok(stats)
    }

    pub fn read_to_writer(
        &self,
        rel_path: impl AsRef<Path>,
        mut limit_bytes: Option<u64>,
        writer: &mut impl Write,
    ) -> Result<CacheIoStats> {
        self.read_range_to_writer(rel_path, 0, limit_bytes.take(), writer)
    }

    pub fn read_range_to_writer(
        &self,
        rel_path: impl AsRef<Path>,
        offset: u64,
        mut limit_bytes: Option<u64>,
        writer: &mut impl Write,
    ) -> Result<CacheIoStats> {
        let rel_path = normalize_relative_path(rel_path)?;
        let meta = self.stat(&rel_path)?;
        if offset >= meta.len {
            return Ok(CacheIoStats::default());
        }
        let mut remaining = limit_bytes
            .take()
            .unwrap_or(meta.len - offset)
            .min(meta.len - offset);
        let mut offset = offset;
        let mut buffer = vec![0u8; self.config.chunk_size as usize];
        let mut stats = CacheIoStats::default();

        while remaining > 0 {
            let want = std::cmp::min(buffer.len() as u64, remaining) as usize;
            let read_stats = self.read_at_meta(&rel_path, &meta, offset, &mut buffer[..want])?;
            if read_stats.bytes == 0 {
                break;
            }
            writer.write_all(&buffer[..read_stats.bytes as usize])?;
            remaining -= read_stats.bytes;
            offset += read_stats.bytes;
            stats.merge(read_stats);
        }

        Ok(stats)
    }

    pub fn flush_pending(&self) -> Result<()> {
        self.writer.flush().map_err(CacheError::Io)
    }

    pub fn evict_file(&self, rel_path: impl AsRef<Path>) -> Result<u64> {
        let meta = self.stat(rel_path)?;
        let dir = self.file_cache_dir(&meta);
        remove_dir_if_exists(&dir)
    }

    pub fn invalidate_cached_meta(&self, meta: &SourceFileMeta) -> Result<u64> {
        let mut removed = remove_dir_if_exists(&self.file_cache_dir(meta))?;
        removed += remove_file_if_exists(&self.meta_path(meta))?;
        Ok(removed)
    }

    pub fn evict_all(&self) -> Result<u64> {
        remove_dir_if_exists(&self.config.cache_root)
    }

    fn load_chunk(
        &self,
        rel_path: &Path,
        meta: &SourceFileMeta,
        chunk_index: u64,
    ) -> Result<CachedChunk> {
        let chunk_path = self.chunk_path(meta, chunk_index);
        if chunk_path.exists() {
            let started = Instant::now();
            let bytes: Arc<[u8]> = Arc::from(fs::read(chunk_path)?);
            let read_ns = elapsed_ns(started);
            return Ok(CachedChunk {
                bytes,
                stats: CacheIoStats {
                    cache_read_ops: 1,
                    cache_read_ns: read_ns,
                    cache_read_max_ns: read_ns,
                    ..CacheIoStats::default()
                },
            });
        }

        let chunk_start = chunk_index * meta.chunk_size;
        if chunk_start >= meta.len {
            return Err(CacheError::OffsetBeyondEof {
                offset: chunk_start,
                len: meta.len,
            });
        }
        let source_path = self.config.source_root.join(rel_path);
        let mut source = open_source_sequential(&source_path)?;
        self.load_source_chunk_from_file(&mut source, meta, chunk_index)
    }

    fn load_source_chunk_from_file(
        &self,
        source: &mut File,
        meta: &SourceFileMeta,
        chunk_index: u64,
    ) -> Result<CachedChunk> {
        let chunk_start = chunk_index * meta.chunk_size;
        if chunk_start >= meta.len {
            return Err(CacheError::OffsetBeyondEof {
                offset: chunk_start,
                len: meta.len,
            });
        }
        let chunk_len = std::cmp::min(meta.chunk_size, meta.len - chunk_start) as usize;
        let source_started = Instant::now();
        source.seek(SeekFrom::Start(chunk_start))?;
        let mut bytes = vec![0u8; chunk_len];
        source.read_exact(&mut bytes)?;
        let source_read_ns = elapsed_ns(source_started);
        let bytes: Arc<[u8]> = Arc::from(bytes);

        let mut stats = CacheIoStats {
            source_fetch_bytes: chunk_len as u64,
            source_read_ops: 1,
            source_read_ns,
            source_read_max_ns: source_read_ns,
            heap_allocated_bytes: chunk_len as u64,
            ..CacheIoStats::default()
        };
        if self.config.write_cache {
            let enqueue_started = Instant::now();
            if let Some(bytes_queued) =
                self.write_chunk_async(meta, chunk_index, Arc::clone(&bytes))?
            {
                stats.cache_write_jobs += 1;
                stats.cache_write_bytes += bytes_queued;
            }
            if let Some(bytes_queued) = self.write_meta_async(meta)? {
                stats.cache_write_jobs += 1;
                stats.cache_write_bytes += bytes_queued;
            }
            stats.cache_write_enqueue_ns += elapsed_ns(enqueue_started);
        }
        Ok(CachedChunk { bytes, stats })
    }

    fn file_cache_dir(&self, meta: &SourceFileMeta) -> PathBuf {
        self.config
            .cache_root
            .join("chunks")
            .join(&meta.cache_key[0..2])
            .join(&meta.cache_key)
    }

    fn chunk_path(&self, meta: &SourceFileMeta, chunk_index: u64) -> PathBuf {
        self.file_cache_dir(meta)
            .join(format!("{chunk_index:016x}.chunk"))
    }

    fn meta_path(&self, meta: &SourceFileMeta) -> PathBuf {
        self.config
            .cache_root
            .join("meta")
            .join(&meta.cache_key[0..2])
            .join(format!("{}.json", meta.cache_key))
    }

    fn write_chunk_async(
        &self,
        meta: &SourceFileMeta,
        chunk_index: u64,
        bytes: Arc<[u8]>,
    ) -> Result<Option<u64>> {
        let final_path = self.chunk_path(meta, chunk_index);
        if final_path.exists() {
            return Ok(None);
        }
        let parent = final_path.parent().expect("chunk path has parent");
        fs::create_dir_all(parent)?;
        let len = bytes.len() as u64;
        self.writer
            .write_atomic(final_path, bytes)
            .map_err(CacheError::Io)?;
        Ok(Some(len))
    }

    fn write_meta_async(&self, meta: &SourceFileMeta) -> Result<Option<u64>> {
        let final_path = self.meta_path(meta);
        if final_path.exists() {
            return Ok(None);
        }
        if let Some(parent) = final_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(meta)?;
        let len = json.len() as u64;
        self.writer
            .write_atomic(final_path, Arc::from(json))
            .map_err(CacheError::Io)?;
        Ok(Some(len))
    }
}

#[derive(Clone, Debug)]
pub struct DirEntryMeta {
    pub name: String,
    pub is_dir: bool,
    pub len: u64,
    pub modified: SystemTime,
    pub created: SystemTime,
}

struct CachedChunk {
    bytes: Arc<[u8]>,
    stats: CacheIoStats,
}

#[derive(Clone)]
struct CacheWriter {
    tx: Sender<WriteJob>,
}

impl CacheWriter {
    fn new() -> Self {
        let (tx, rx) = mpsc::channel::<WriteJob>();
        std::thread::Builder::new()
            .name("nas-fast-cache-writer".to_string())
            .spawn(move || {
                let mut nonce = 0u64;
                let mut last_error: Option<String> = None;
                for job in rx {
                    match job {
                        WriteJob::Write { final_path, bytes } => {
                            if let Err(err) = write_file_atomic(&final_path, &bytes, &mut nonce) {
                                last_error = Some(err.to_string());
                            }
                        }
                        WriteJob::Flush { done } => {
                            let result = match &last_error {
                                Some(message) => Err(std::io::Error::other(message.clone())),
                                None => Ok(()),
                            };
                            let _ = done.send(result);
                        }
                    }
                }
            })
            .expect("failed to spawn nas-fast-cache writer");
        Self { tx }
    }

    fn write_atomic(&self, final_path: PathBuf, bytes: Arc<[u8]>) -> std::io::Result<()> {
        self.tx
            .send(WriteJob::Write { final_path, bytes })
            .map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::BrokenPipe, "cache writer stopped")
            })
    }

    fn flush(&self) -> std::io::Result<()> {
        let (tx, rx) = mpsc::channel();
        self.tx.send(WriteJob::Flush { done: tx }).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "cache writer stopped")
        })?;
        rx.recv().map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "cache writer stopped")
        })?
    }
}

enum WriteJob {
    Write {
        final_path: PathBuf,
        bytes: Arc<[u8]>,
    },
    Flush {
        done: Sender<std::io::Result<()>>,
    },
}

fn write_file_atomic(final_path: &Path, bytes: &[u8], nonce: &mut u64) -> std::io::Result<()> {
    if final_path.exists() {
        return Ok(());
    }
    let parent = final_path.parent().expect("cache file has parent");
    fs::create_dir_all(parent)?;
    let tmp_path = parent.join(format!(".{}.{}.tmp", std::process::id(), *nonce));
    *nonce += 1;
    {
        let mut tmp = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)?;
        tmp.write_all(bytes)?;
        tmp.sync_data()?;
    }
    match fs::rename(&tmp_path, final_path) {
        Ok(()) => Ok(()),
        Err(err) if final_path.exists() => {
            let _ = fs::remove_file(&tmp_path);
            let _ = err;
            Ok(())
        }
        Err(err) => Err(err),
    }
}

fn read_file_range_exact(path: &Path, offset: u64, buffer: &mut [u8]) -> std::io::Result<()> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(buffer)
}

fn read_file_range_exact_timed(
    path: &Path,
    offset: u64,
    buffer: &mut [u8],
) -> std::io::Result<u64> {
    let started = Instant::now();
    read_file_range_exact(path, offset, buffer)?;
    Ok(elapsed_ns(started))
}

#[cfg(windows)]
fn open_source_sequential(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ_WRITE_DELETE)
        .custom_flags(FILE_FLAG_SEQUENTIAL_SCAN)
        .open(path)
}

#[cfg(not(windows))]
fn open_source_sequential(path: &Path) -> std::io::Result<File> {
    File::open(path)
}

fn elapsed_ns(started: Instant) -> u64 {
    started.elapsed().as_nanos().min(u64::MAX as u128) as u64
}

fn system_time_ns(time: SystemTime) -> u128 {
    time.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

fn remove_dir_if_exists(path: &Path) -> Result<u64> {
    if !path.exists() {
        return Ok(0);
    }
    let bytes = dir_size(path)?;
    fs::remove_dir_all(path)?;
    Ok(bytes)
}

fn remove_file_if_exists(path: &Path) -> Result<u64> {
    if !path.exists() {
        return Ok(0);
    }
    let bytes = fs::metadata(path)?.len();
    fs::remove_file(path)?;
    Ok(bytes)
}

fn dir_size(path: &Path) -> Result<u64> {
    let mut total = 0u64;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            total += dir_size(&entry.path())?;
        } else {
            total += metadata.len();
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hot_read_serves_from_cache_chunks() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let cache = tmp.path().join("cache");
        fs::create_dir_all(source.join("movies")).unwrap();
        fs::write(
            source.join("movies").join("a.bin"),
            vec![7u8; 10 * 1024 * 1024],
        )
        .unwrap();

        let mut config = CacheConfig::new(source, cache);
        config.chunk_size = 1024 * 1024;
        let cache = ReadThroughCache::new(config);
        let mut sink = Vec::new();
        let first = cache
            .read_to_writer("movies/a.bin", Some(2 * 1024 * 1024), &mut sink)
            .unwrap();
        assert_eq!(first.bytes, 2 * 1024 * 1024);
        assert!(first.cache_misses > 0);
        cache.flush_pending().unwrap();

        sink.clear();
        let second = cache
            .read_to_writer("movies/a.bin", Some(2 * 1024 * 1024), &mut sink)
            .unwrap();
        assert_eq!(second.bytes, 2 * 1024 * 1024);
        assert_eq!(second.source_bytes, 0);
        assert!(second.cache_hits > 0);
    }

    #[test]
    fn disabled_cache_writes_do_not_persist_chunks() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let cache = tmp.path().join("cache");
        fs::create_dir_all(source.join("movies")).unwrap();
        fs::write(
            source.join("movies").join("a.bin"),
            vec![7u8; 2 * 1024 * 1024],
        )
        .unwrap();

        let mut config = CacheConfig::new(source, cache.clone());
        config.chunk_size = 1024 * 1024;
        config.write_cache = false;
        let cache_engine = ReadThroughCache::new(config);
        let mut sink = Vec::new();
        let first = cache_engine
            .read_to_writer("movies/a.bin", Some(1024 * 1024), &mut sink)
            .unwrap();
        cache_engine.flush_pending().unwrap();

        assert_eq!(first.cache_write_jobs, 0);
        assert_eq!(first.source_fetch_bytes, 1024 * 1024);
        assert!(!cache.join("chunks").exists());
        assert!(!cache.join("meta").exists());
    }
}
