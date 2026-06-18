use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::SystemTime;

use crate::resample::ResamplingMode;

// ─── Content Hash ───

const CONTENT_HASH_READ_SIZE: u64 = 4096;

pub fn get_content_hash(path: &str) -> String {
    {
        let cache = CONTENT_HASH_CACHE.lock().unwrap();
        if let Some(hash) = cache.get(path) {
            return hash.clone();
        }
    }
    match file_content_hash(path) {
        Ok(hash) => {
            let mut cache = CONTENT_HASH_CACHE.lock().unwrap();
            cache.insert(path.to_string(), hash.clone());
            hash
        }
        Err(_) => {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            path.hash(&mut hasher);
            format!("{:016x}", hasher.finish())
        }
    }
}

pub(crate) static CONTENT_HASH_CACHE: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn file_content_hash(path: &str) -> Result<String, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("Failed to open for hash: {e}"))?;
    let metadata = file
        .metadata()
        .map_err(|e| format!("Failed to stat for hash: {e}"))?;
    let file_size = metadata.len();

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    file_size.hash(&mut hasher);

    let mut reader = std::io::BufReader::new(file);
    let mut buf = [0u8; CONTENT_HASH_READ_SIZE as usize];

    let head_len = CONTENT_HASH_READ_SIZE.min(file_size);
    if head_len > 0 {
        let mut chunk = &mut buf[..head_len as usize];
        reader
            .read_exact(&mut chunk)
            .map_err(|e| format!("Hash read head: {e}"))?;
        chunk.hash(&mut hasher);
    }

    if file_size > CONTENT_HASH_READ_SIZE {
        let tail_start = file_size - CONTENT_HASH_READ_SIZE;
        reader
            .seek(SeekFrom::Start(tail_start))
            .map_err(|e| format!("Hash seek tail: {e}"))?;
        let mut chunk = &mut buf[..CONTENT_HASH_READ_SIZE as usize];
        reader
            .read_exact(&mut chunk)
            .map_err(|e| format!("Hash read tail: {e}"))?;
        chunk.hash(&mut hasher);
    }

    Ok(format!("{:016x}", hasher.finish()))
}

// ─── Raster Cache (L0: metadata cache, already in tile.rs) ───

// ─── Tile Cache Key ───

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct TileCacheKey {
    pub layer_hash: String,
    pub z: u32,
    pub x: u32,
    pub y: u32,
    pub resampling: u8,
    /// 0 = PNG, 1 = WebP
    pub format: u8,
}

impl TileCacheKey {
    pub fn new(path: &str, z: u32, x: u32, y: u32, resampling: ResamplingMode) -> Self {
        Self {
            layer_hash: get_content_hash(path),
            z,
            x,
            y,
            resampling: resampling as u8,
            format: 0,
        }
    }

    pub fn new_webp(path: &str, z: u32, x: u32, y: u32, resampling: ResamplingMode) -> Self {
        Self {
            layer_hash: get_content_hash(path),
            z,
            x,
            y,
            resampling: resampling as u8,
            format: 1,
        }
    }
}

// ─── L2 Memory Cache ───

struct MemCacheEntry {
    data: Vec<u8>,
    size: u64,
    last_access: u64,
}

pub struct MemCache {
    entries: HashMap<TileCacheKey, MemCacheEntry>,
    max_bytes: u64,
    current_bytes: u64,
    access_counter: u64,
}

impl MemCache {
    pub fn new(max_mb: u64) -> Self {
        Self {
            entries: HashMap::new(),
            max_bytes: max_mb * 1024 * 1024,
            current_bytes: 0,
            access_counter: 0,
        }
    }

    pub fn get(&mut self, key: &TileCacheKey) -> Option<&Vec<u8>> {
        let entry = self.entries.get_mut(key)?;
        self.access_counter += 1;
        entry.last_access = self.access_counter;
        Some(&entry.data)
    }

    pub fn insert(&mut self, key: TileCacheKey, data: Vec<u8>) {
        let size = data.len() as u64 + std::mem::size_of::<TileCacheKey>() as u64;
        self.access_counter += 1;

        while self.current_bytes + size > self.max_bytes && !self.entries.is_empty() {
            let oldest_key = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.last_access)
                .map(|(k, _)| k.clone());
            if let Some(k) = oldest_key {
                if let Some(removed) = self.entries.remove(&k) {
                    self.current_bytes = self.current_bytes.saturating_sub(removed.size);
                }
            }
        }

        self.current_bytes += size;
        self.entries.insert(
            key,
            MemCacheEntry {
                data,
                size,
                last_access: self.access_counter,
            },
        );
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.current_bytes = 0;
    }

    pub fn size_bytes(&self) -> u64 {
        self.current_bytes
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

// ─── L3 Disk Cache ───

static DISK_CACHE_DIR: OnceLock<PathBuf> = OnceLock::new();

pub fn set_disk_cache_dir(path: &str) {
    if let Err(existing) = DISK_CACHE_DIR.set(PathBuf::from(path)) {
        tracing::warn!("Disk cache dir already set to {:?}, ignoring", existing);
    }
}

#[allow(deprecated)]
fn get_disk_cache_dir() -> &'static Path {
    DISK_CACHE_DIR.get_or_init(|| {
        let tmp = tempfile::tempdir().expect("Failed to create temp dir for tile cache");
        tracing::info!("Using temp dir for disk cache: {:?}", tmp.path());
        tmp.into_path()
    })
}

fn tile_cache_path(
    path: &str,
    z: u32,
    x: u32,
    y: u32,
    resampling: ResamplingMode,
    is_webp: bool,
) -> PathBuf {
    let hash = get_content_hash(path);
    let res_str = resampling as u8;
    let ext = if is_webp { "webp" } else { "png" };
    get_disk_cache_dir()
        .join(format!("{}_{}", hash, res_str))
        .join(z.to_string())
        .join(x.to_string())
        .join(format!("{}.{}", y, ext))
}

pub fn disk_cache_get(
    path: &str,
    z: u32,
    x: u32,
    y: u32,
    resampling: ResamplingMode,
    is_webp: bool,
) -> Option<Vec<u8>> {
    let p = tile_cache_path(path, z, x, y, resampling, is_webp);
    if p.exists() {
        std::fs::read(p).ok()
    } else {
        None
    }
}

pub fn disk_cache_set(
    path: &str,
    z: u32,
    x: u32,
    y: u32,
    resampling: ResamplingMode,
    is_webp: bool,
    data: &[u8],
) {
    let p = tile_cache_path(path, z, x, y, resampling, is_webp);
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&p, data);
    evict_disk_cache_if_needed();
}

pub fn disk_cache_size_bytes() -> u64 {
    crate::directory_size_bytes(get_disk_cache_dir()).unwrap_or(0)
}

pub fn clear_disk_cache() {
    let dir = get_disk_cache_dir();
    let _ = std::fs::remove_dir_all(dir);
    let _ = std::fs::create_dir_all(dir);
}

// ─── Cache Stats ───

static CACHE_HITS_L2: AtomicU64 = AtomicU64::new(0);
static CACHE_HITS_L3: AtomicU64 = AtomicU64::new(0);
static CACHE_MISSES: AtomicU64 = AtomicU64::new(0);

pub fn record_l2_hit() {
    CACHE_HITS_L2.fetch_add(1, Ordering::Relaxed);
}
pub fn record_l3_hit() {
    CACHE_HITS_L3.fetch_add(1, Ordering::Relaxed);
}
pub fn record_miss() {
    CACHE_MISSES.fetch_add(1, Ordering::Relaxed);
}

pub fn cache_stats() -> (u64, u64, u64) {
    (
        CACHE_HITS_L2.load(Ordering::Relaxed),
        CACHE_HITS_L3.load(Ordering::Relaxed),
        CACHE_MISSES.load(Ordering::Relaxed),
    )
}

// ─── Global L2 Cache ───

static L2_CACHE_MAX_MB: AtomicU64 = AtomicU64::new(512);

pub fn set_l2_cache_size_mb(mb: u64) {
    L2_CACHE_MAX_MB.store(mb, Ordering::Relaxed);
}

pub static L2_CACHE: LazyLock<Mutex<MemCache>> =
    LazyLock::new(|| Mutex::new(MemCache::new(L2_CACHE_MAX_MB.load(Ordering::Relaxed))));

// ─── L3 Disk Cache Eviction ───

const DISK_CACHE_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024; // 2 GB
const EVICTION_CHECK_INTERVAL: u64 = 10;

static DISK_WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);
static DISK_CACHE_MAX: AtomicU64 = AtomicU64::new(DISK_CACHE_MAX_BYTES);

pub fn set_disk_cache_max_bytes(max_bytes: u64) {
    DISK_CACHE_MAX.store(max_bytes, Ordering::Relaxed);
}

pub fn evict_disk_cache_if_needed() {
    let count = DISK_WRITE_COUNTER.fetch_add(1, Ordering::Relaxed);
    if count % EVICTION_CHECK_INTERVAL != 0 {
        return;
    }
    let max_bytes = DISK_CACHE_MAX.load(Ordering::Relaxed);
    let dir = get_disk_cache_dir();
    let current_size = crate::directory_size_bytes(dir).unwrap_or(0);
    if current_size <= max_bytes {
        return;
    }
    let target = (max_bytes as f64 * 0.8) as u64;
    let excess = current_size.saturating_sub(target);
    let mut files: Vec<(PathBuf, SystemTime)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                if let Ok(metadata) = path.metadata() {
                    if let Ok(mtime) = metadata.modified() {
                        files.push((path, mtime));
                    }
                }
            }
        }
    }
    files.sort_by_key(|(_, mtime)| *mtime);

    let mut removed = 0u64;
    for (path, _) in &files {
        if removed >= excess {
            break;
        }
        if let Ok(metadata) = path.metadata() {
            let size = metadata.len();
            let _ = std::fs::remove_file(path);
            removed = removed.saturating_add(size);
        }
    }
    if removed > 0 {
        tracing::info!(
            "Evicted {:.1} MB from disk tile cache",
            removed as f64 / 1_048_576.0
        );
    }
}
