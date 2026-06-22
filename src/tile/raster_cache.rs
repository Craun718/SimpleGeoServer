use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::BufReader;
use std::mem::size_of;
use std::sync::{Arc, RwLock};

use std::sync::LazyLock;
use tiff::decoder::{ChunkType, Decoder, Limits};

use super::types::{CachedRaster, IfdInfo, InterleaveType};

const RASTER_CACHE_MAX_ENTRIES: usize = 50;

struct RasterCacheInner {
    map: HashMap<String, Arc<CachedRaster>>,
    order: VecDeque<String>,
    max_entries: usize,
}

impl RasterCacheInner {
    fn new(max_entries: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::with_capacity(max_entries),
            max_entries,
        }
    }

    fn get(&mut self, path: &str) -> Option<&Arc<CachedRaster>> {
        if self.map.contains_key(path) {
            let _ = self.touch(path);
            self.map.get(path)
        } else {
            None
        }
    }

    fn insert(&mut self, path: String, raster: Arc<CachedRaster>) {
        if self.map.contains_key(&path) {
            return;
        }
        while self.map.len() >= self.max_entries {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            } else {
                break;
            }
        }
        self.map.insert(path.clone(), raster);
        self.order.push_back(path);
    }

    fn touch(&mut self, path: &str) {
        if let Some(pos) = self.order.iter().position(|p| p == path) {
            self.order.remove(pos);
            self.order.push_back(path.to_string());
        }
    }
}

static RASTER_CACHE: LazyLock<RwLock<RasterCacheInner>> =
    LazyLock::new(|| RwLock::new(RasterCacheInner::new(RASTER_CACHE_MAX_ENTRIES)));

fn find_ovr_file(tif_path: &str) -> Option<String> {
    let p = std::path::Path::new(tif_path);
    let candidates = [
        format!("{}.ovr", tif_path),
        p.with_extension("ovr").to_string_lossy().to_string(),
    ];
    for path in &candidates {
        if std::path::Path::new(path).exists() {
            return Some(path.clone());
        }
    }
    None
}

fn read_geo_transform_tile(path: &str) -> [f64; 6] {
    let tfw_path = std::path::Path::new(path).with_extension("tfw");
    if tfw_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&tfw_path) {
            let values: Vec<f64> = content
                .lines()
                .filter_map(|l| l.trim().parse::<f64>().ok())
                .collect();
            if values.len() == 6 {
                return [
                    values[4], values[0], values[1], values[5], values[2], values[3],
                ];
            }
        }
    }

    if let Ok(file) = File::open(path) {
        if let Ok(mut decoder) = Decoder::new(BufReader::new(file)) {
            if let Ok(tiepoint) = decoder.get_tag_f64_vec(tiff::tags::Tag::ModelTiepointTag) {
                if let Ok(scale) = decoder.get_tag_f64_vec(tiff::tags::Tag::ModelPixelScaleTag) {
                    if tiepoint.len() >= 6 && scale.len() >= 3 {
                        return [tiepoint[3], scale[0], 0.0, tiepoint[4], 0.0, -scale[1]];
                    }
                }
            }
        }
    }

    [0.0, 1.0, 0.0, 0.0, 0.0, -1.0]
}

pub fn get_raster(path: &str) -> Result<Arc<CachedRaster>, String> {
    {
        let mut cache = RASTER_CACHE
            .write()
            .map_err(|e| format!("Cache lock error: {}", e))?;
        if let Some(raster) = cache.get(path) {
            return Ok(Arc::clone(raster));
        }
    }
    let raster = load_and_cache_raster(path)?;
    let arc = Arc::clone(&raster);
    {
        let mut cache = RASTER_CACHE
            .write()
            .map_err(|e| format!("Cache lock error: {}", e))?;
        cache.insert(path.to_string(), raster);
    }
    Ok(arc)
}

fn estimate_raster_size_bytes(raster: &CachedRaster) -> u64 {
    let vec_bytes = |len: usize| -> u64 { (len as u64).saturating_mul(size_of::<f64>() as u64) };

    let mut total = size_of::<CachedRaster>() as u64;
    total = total.saturating_add(raster.file_path.capacity() as u64);
    total = total.saturating_add(vec_bytes(raster.min_values.capacity()));
    total = total.saturating_add(vec_bytes(raster.max_values.capacity()));
    total = total.saturating_add(vec_bytes(raster.mean_values.capacity()));
    total = total.saturating_add(vec_bytes(raster.std_dev_values.capacity()));
    total = total.saturating_add((size_of::<IfdInfo>() * raster.ifds.capacity()) as u64);
    total = total.saturating_add(raster.crs_type.capacity() as u64);
    total = total.saturating_add((size_of::<usize>() as u64).saturating_mul(2));
    total
}

pub fn raster_memory_cache_size_bytes() -> Result<u64, String> {
    let cache = RASTER_CACHE
        .write()
        .map_err(|e| format!("Cache lock error: {}", e))?;

    Ok(cache.map.iter().fold(0u64, |total, (path, raster)| {
        total
            .saturating_add(path.capacity() as u64)
            .saturating_add(estimate_raster_size_bytes(raster.as_ref()))
    }))
}

#[allow(dead_code)]
pub fn clear_raster_memory_cache() -> Result<(), String> {
    let mut cache = RASTER_CACHE
        .write()
        .map_err(|e| format!("Cache lock error: {}", e))?;
    cache.map.clear();
    cache.order.clear();
    Ok(())
}

pub struct RasterFileInfo {
    pub width: u32,
    pub height: u32,
    pub bands: usize,
    pub data_type: String,
    pub no_data: Option<f64>,
    pub interleave: InterleaveType,
    pub chunk_count: u32,
    pub chunk_type: ChunkType,
    pub chunk_width: u32,
    pub chunk_length: u32,
    pub chunks_per_row: u32,
    pub geo_transform: [f64; 6],
    pub crs_type: String,
    pub extent_wgs84: Option<[f64; 4]>,
    pub max_zoom: u32,
    pub wgs84_corners: [(f64, f64); 4],
    pub native_corners: [(f64, f64); 4],
}

pub enum RasterLoadProgress {
    FileOpened {
        width: u32,
        height: u32,
        bands: usize,
        data_type: String,
        no_data: Option<f64>,
        interleave: String,
        chunk_count: u32,
    },
    GeoInfoReady {
        crs_type: String,
        geo_transform: [f64; 6],
        extent_wgs84: Option<[f64; 4]>,
        max_zoom: u32,
        wgs84_corners: [(f64, f64); 4],
        native_corners: [(f64, f64); 4],
    },
    StatsProgress {
        chunks_done: u32,
        chunk_count: u32,
    },
    StatsComplete {
        min_values: Vec<f64>,
        max_values: Vec<f64>,
        mean_values: Vec<f64>,
        std_dev_values: Vec<f64>,
        percentile_bounds: Vec<(f64, f64)>,
    },
    OverviewsReady {
        total_ifds: usize,
        ovr_path: Option<String>,
    },
    Ready,
}

pub fn open_raster_metadata(path: &str) -> Result<RasterFileInfo, String> {
    let file = File::open(path).map_err(|e| format!("Failed to open raster: {}", e))?;
    let mut decoder = Decoder::new(BufReader::new(file))
        .map_err(|e| format!("Failed to create TIFF decoder: {}", e))?
        .with_limits(Limits::unlimited());

    let (width, height) = decoder
        .dimensions()
        .map_err(|e| format!("Failed to read dimensions: {}", e))?;

    let data_type = decoder
        .colortype()
        .map(|ct| format!("{:?}", ct))
        .unwrap_or_else(|_| "Unknown".to_string());

    let no_data = crate::raster::read_nodata_value(&mut decoder);

    let chunk_type = decoder.get_chunk_type();
    let (chunk_w, chunk_h) = decoder.chunk_dimensions();
    let total_chunks = match chunk_type {
        ChunkType::Strip => decoder.strip_count().unwrap_or(1),
        ChunkType::Tile => decoder.tile_count().unwrap_or(1),
    };
    let cpr = match chunk_type {
        ChunkType::Strip => 1u32,
        ChunkType::Tile => (width + chunk_w - 1) / chunk_w,
    };

    let interleave = {
        let planar = decoder
            .find_tag_unsigned::<u16>(tiff::tags::Tag::PlanarConfiguration)
            .ok()
            .flatten()
            .unwrap_or(1);
        if planar == 2 {
            InterleaveType::Planar
        } else {
            InterleaveType::Chunky
        }
    };

    let bands = {
        let tag_samples = decoder
            .find_tag_unsigned::<u16>(tiff::tags::Tag::SamplesPerPixel)
            .ok()
            .flatten()
            .map(|s| s as usize);
        if let Some(spp) = tag_samples {
            spp
        } else {
            let first_chunk = decoder
                .read_chunk(0)
                .map_err(|e| format!("Failed to read first chunk: {}", e))?;
            let chunk_f64 = crate::raster::decode_result_to_f64_vec(&first_chunk);
            let (w, h) = decoder.chunk_data_dimensions(0);
            let chunk_pixels = (w * h) as usize;
            if chunk_pixels > 0 && chunk_f64.len() >= chunk_pixels {
                chunk_f64.len() / chunk_pixels
            } else {
                1
            }
        }
    };

    let geo_key = crate::reproject::read_geo_key_info(&mut decoder).unwrap_or_default();
    let crs_type = match geo_key.model_type {
        Some(2) => "Geographic".to_string(),
        Some(1) => "Projected".to_string(),
        _ => "Unknown".to_string(),
    };
    let geo_transform = read_geo_transform_tile(path);

    let extent_wgs84 = crate::reproject::extent_to_wgs84(&geo_transform, width, height, &geo_key);
    let wgs84_corners = if let Some([min_lng, min_lat, max_lng, max_lat]) = extent_wgs84 {
        [
            (min_lng, min_lat),
            (max_lng, min_lat),
            (min_lng, max_lat),
            (max_lng, max_lat),
        ]
    } else {
        let gt = geo_transform;
        let c0 = (gt[0], gt[3]);
        let c1 = (gt[0] + width as f64 * gt[1], gt[3]);
        let c2 = (gt[0], gt[3] + height as f64 * gt[5]);
        let c3 = (gt[0] + width as f64 * gt[1], gt[3] + height as f64 * gt[5]);
        let min_lng = c0.0.min(c1.0).min(c2.0).min(c3.0);
        let max_lng = c0.0.max(c1.0).max(c2.0).max(c3.0);
        let min_lat = c0.1.min(c1.1).min(c2.1).min(c3.1);
        let max_lat = c0.1.max(c1.1).max(c2.1).max(c3.1);
        [
            (min_lng, min_lat),
            (max_lng, min_lat),
            (min_lng, max_lat),
            (max_lng, max_lat),
        ]
    };

    let native_corners = if crs_type == "Geographic" || crs_type == "Unknown" {
        wgs84_corners
    } else {
        let converted: Option<[(f64, f64); 4]> = (|| {
            let mut corners = [(0.0f64, 0.0f64); 4];
            for (i, &(lng, lat)) in wgs84_corners.iter().enumerate() {
                corners[i] = crate::reproject::wgs84_to_native_crs(lng, lat, &geo_key)?;
            }
            Some(corners)
        })();
        converted.unwrap_or_else(|| {
            let gt = geo_transform;
            [
                (gt[0], gt[3] + height as f64 * gt[5]),
                (gt[0] + width as f64 * gt[1], gt[3] + height as f64 * gt[5]),
                (gt[0], gt[3]),
                (gt[0] + width as f64 * gt[1], gt[3]),
            ]
        })
    };

    let max_zoom = {
        let is_geographic = crs_type == "Geographic" || crs_type == "Unknown";
        let raster_res_3857 = if is_geographic {
            let res_degree = geo_transform[1].abs();
            res_degree * 111320.0
        } else {
            geo_transform[1].abs()
        };
        if raster_res_3857 > 0.0 {
            let ratio = (2.0 * super::tile_math::C) / (256.0 * raster_res_3857);
            if ratio > 1.0 {
                ratio.log2().ceil() as u32
            } else {
                0
            }
        } else {
            22
        }
        .min(22)
    };

    Ok(RasterFileInfo {
        width,
        height,
        bands,
        data_type,
        no_data,
        interleave,
        chunk_count: total_chunks,
        chunk_type,
        chunk_width: chunk_w,
        chunk_length: chunk_h,
        chunks_per_row: cpr,
        geo_transform,
        crs_type,
        extent_wgs84,
        max_zoom,
        wgs84_corners,
        native_corners,
    })
}

pub fn load_and_cache_raster_with_progress(
    path: &str,
    on_progress: impl Fn(RasterLoadProgress),
) -> Result<Arc<CachedRaster>, String> {
    let file = File::open(path).map_err(|e| format!("Failed to open raster: {}", e))?;
    let mut decoder = Decoder::new(BufReader::new(file))
        .map_err(|e| format!("Failed to create TIFF decoder: {}", e))?
        .with_limits(Limits::unlimited());

    let (width, height) = decoder
        .dimensions()
        .map_err(|e| format!("Failed to read dimensions: {}", e))?;

    let data_type = decoder
        .colortype()
        .map(|ct| format!("{:?}", ct))
        .unwrap_or_else(|_| "Unknown".to_string());

    let no_data = crate::raster::read_nodata_value(&mut decoder);

    let chunk_type = decoder.get_chunk_type();
    let (chunk_w, chunk_h) = decoder.chunk_dimensions();
    let total_chunks = match chunk_type {
        ChunkType::Strip => decoder.strip_count().unwrap_or(1),
        ChunkType::Tile => decoder.tile_count().unwrap_or(1),
    };
    let cpr = match chunk_type {
        ChunkType::Strip => 1u32,
        ChunkType::Tile => (width + chunk_w - 1) / chunk_w,
    };

    let interleave = {
        let planar = decoder
            .find_tag_unsigned::<u16>(tiff::tags::Tag::PlanarConfiguration)
            .ok()
            .flatten()
            .unwrap_or(1);
        if planar == 2 {
            InterleaveType::Planar
        } else {
            InterleaveType::Chunky
        }
    };

    let bands = {
        let tag_samples = decoder
            .find_tag_unsigned::<u16>(tiff::tags::Tag::SamplesPerPixel)
            .ok()
            .flatten()
            .map(|s| s as usize);
        if let Some(spp) = tag_samples {
            spp
        } else {
            let first_chunk = decoder
                .read_chunk(0)
                .map_err(|e| format!("Failed to read first chunk: {}", e))?;
            let chunk_f64 = crate::raster::decode_result_to_f64_vec(&first_chunk);
            let (w, h) = decoder.chunk_data_dimensions(0);
            let chunk_pixels = (w * h) as usize;
            if chunk_pixels > 0 && chunk_f64.len() >= chunk_pixels {
                chunk_f64.len() / chunk_pixels
            } else {
                1
            }
        }
    };

    // Phase 1: file opened, basic dimensions available
    on_progress(RasterLoadProgress::FileOpened {
        width,
        height,
        bands,
        data_type: data_type.clone(),
        no_data,
        interleave: format!("{:?}", interleave),
        chunk_count: total_chunks,
    });

    // Phase 2: read GeoInfo BEFORE stats loop (reordered from original)
    let geo_key = crate::reproject::read_geo_key_info(&mut decoder).unwrap_or_default();
    let crs_type = match geo_key.model_type {
        Some(2) => "Geographic".to_string(),
        Some(1) => "Projected".to_string(),
        _ => "Unknown".to_string(),
    };
    let geo_transform = read_geo_transform_tile(path);

    let extent_wgs84 = crate::reproject::extent_to_wgs84(&geo_transform, width, height, &geo_key);
    let wgs84_corners = if let Some([min_lng, min_lat, max_lng, max_lat]) = extent_wgs84 {
        [
            (min_lng, min_lat),
            (max_lng, min_lat),
            (min_lng, max_lat),
            (max_lng, max_lat),
        ]
    } else {
        let gt = geo_transform;
        let c0 = (gt[0], gt[3]);
        let c1 = (gt[0] + width as f64 * gt[1], gt[3]);
        let c2 = (gt[0], gt[3] + height as f64 * gt[5]);
        let c3 = (gt[0] + width as f64 * gt[1], gt[3] + height as f64 * gt[5]);
        let min_lng = c0.0.min(c1.0).min(c2.0).min(c3.0);
        let max_lng = c0.0.max(c1.0).max(c2.0).max(c3.0);
        let min_lat = c0.1.min(c1.1).min(c2.1).min(c3.1);
        let max_lat = c0.1.max(c1.1).max(c2.1).max(c3.1);
        [
            (min_lng, min_lat),
            (max_lng, min_lat),
            (min_lng, max_lat),
            (max_lng, max_lat),
        ]
    };

    let native_corners = if crs_type == "Geographic" || crs_type == "Unknown" {
        wgs84_corners
    } else {
        let converted: Option<[(f64, f64); 4]> = (|| {
            let mut corners = [(0.0f64, 0.0f64); 4];
            for (i, &(lng, lat)) in wgs84_corners.iter().enumerate() {
                corners[i] = crate::reproject::wgs84_to_native_crs(lng, lat, &geo_key)?;
            }
            Some(corners)
        })();
        converted.unwrap_or_else(|| {
            let gt = geo_transform;
            [
                (gt[0], gt[3] + height as f64 * gt[5]),
                (gt[0] + width as f64 * gt[1], gt[3] + height as f64 * gt[5]),
                (gt[0], gt[3]),
                (gt[0] + width as f64 * gt[1], gt[3]),
            ]
        })
    };

    let max_zoom = {
        let is_geographic = crs_type == "Geographic" || crs_type == "Unknown";
        let raster_res_3857 = if is_geographic {
            let res_degree = geo_transform[1].abs();
            res_degree * 111320.0
        } else {
            geo_transform[1].abs()
        };
        if raster_res_3857 > 0.0 {
            let ratio = (2.0 * super::tile_math::C) / (256.0 * raster_res_3857);
            if ratio > 1.0 {
                ratio.log2().ceil() as u32
            } else {
                0
            }
        } else {
            22
        }
        .min(22)
    };

    on_progress(RasterLoadProgress::GeoInfoReady {
        crs_type: crs_type.clone(),
        geo_transform,
        extent_wgs84,
        max_zoom,
        wgs84_corners,
        native_corners,
    });

    // Phase 3: compute band statistics with progress
    let base_ifd = IfdInfo {
        index: 0,
        width,
        height,
        chunk_type,
        chunk_width: chunk_w,
        chunk_length: chunk_h,
        chunks_per_row: cpr,
        external: false,
        ifd_ptr: None,
        interleave,
        file_path: path.to_string(),
    };

    let mut min_values = vec![f64::INFINITY; bands];
    let mut max_values = vec![f64::NEG_INFINITY; bands];
    let mut valid_counts = vec![0u64; bands];
    let mut means = vec![0.0f64; bands];
    let mut m2 = vec![0.0f64; bands];
    const MAX_PERCENTILE_SAMPLES: usize = 50000;
    let mut percentile_samples: Vec<Vec<f64>> = (0..bands)
        .map(|_| Vec::with_capacity(MAX_PERCENTILE_SAMPLES))
        .collect();

    if interleave == InterleaveType::Planar {
        let chunks_per_band = total_chunks / bands as u32;
        for chunk_idx in 0..total_chunks {
            let band = (chunk_idx / chunks_per_band) as usize;
            if band >= bands {
                break;
            }
            let chunk_data = decoder
                .read_chunk(chunk_idx)
                .map_err(|e| format!("Failed to read chunk {chunk_idx}: {e}"))?;
            let chunk_f64 = crate::raster::decode_result_to_f64_vec(&chunk_data);
            let (cdw, cdh) = decoder.chunk_data_dimensions(chunk_idx);
            let pixels_in_chunk = cdw as usize * cdh as usize;
            for p in 0..pixels_in_chunk {
                if p >= chunk_f64.len() {
                    break;
                }
                let val = chunk_f64[p];
                if crate::raster::is_nodata(val, no_data) {
                    continue;
                }
                valid_counts[band] += 1;
                let n = valid_counts[band] as f64;
                let delta = val - means[band];
                means[band] += delta / n;
                let delta2 = val - means[band];
                m2[band] += delta * delta2;
                if val < min_values[band] {
                    min_values[band] = val;
                }
                if val > max_values[band] {
                    max_values[band] = val;
                }
                if percentile_samples[band].len() < MAX_PERCENTILE_SAMPLES {
                    percentile_samples[band].push(val);
                }
            }
            on_progress(RasterLoadProgress::StatsProgress {
                chunks_done: chunk_idx + 1,
                chunk_count: total_chunks,
            });
        }
    } else {
        for chunk_idx in 0..total_chunks {
            let chunk_data = decoder
                .read_chunk(chunk_idx)
                .map_err(|e| format!("Failed to read chunk {chunk_idx}: {e}"))?;
            let chunk_f64 = crate::raster::decode_result_to_f64_vec(&chunk_data);
            let (cdw, cdh) = decoder.chunk_data_dimensions(chunk_idx);
            let pixels_in_chunk = cdw as usize * cdh as usize;

            for p in 0..pixels_in_chunk {
                for b in 0..bands {
                    let idx = p * bands + b;
                    if idx >= chunk_f64.len() {
                        break;
                    }
                    let val = chunk_f64[idx];
                    if crate::raster::is_nodata(val, no_data) {
                        continue;
                    }
                    valid_counts[b] += 1;
                    let n = valid_counts[b] as f64;
                    let delta = val - means[b];
                    means[b] += delta / n;
                    let delta2 = val - means[b];
                    m2[b] += delta * delta2;
                    if val < min_values[b] {
                        min_values[b] = val;
                    }
                    if val > max_values[b] {
                        max_values[b] = val;
                    }
                    if percentile_samples[b].len() < MAX_PERCENTILE_SAMPLES {
                        percentile_samples[b].push(val);
                    }
                }
            }
            on_progress(RasterLoadProgress::StatsProgress {
                chunks_done: chunk_idx + 1,
                chunk_count: total_chunks,
            });
        }
    }

    for b in 0..bands {
        if valid_counts[b] == 0 {
            min_values[b] = 0.0;
            max_values[b] = 0.0;
        }
    }

    let mut percentile_lo = vec![0.0f64; bands];
    let mut percentile_hi = vec![0.0f64; bands];
    for b in 0..bands {
        if percentile_samples[b].is_empty() {
            percentile_lo[b] = min_values[b];
            percentile_hi[b] = max_values[b];
        } else {
            percentile_samples[b].sort_unstable_by(|a, b| a.partial_cmp(b).unwrap());
            let len = percentile_samples[b].len();
            let lo_idx = ((2.0 / 100.0) * (len - 1) as f64) as usize;
            let hi_idx = ((98.0 / 100.0) * (len - 1) as f64) as usize;
            percentile_lo[b] = percentile_samples[b][lo_idx];
            percentile_hi[b] = percentile_samples[b][hi_idx];
        }
    }
    drop(percentile_samples);

    let std_dev_values: Vec<f64> = (0..bands)
        .map(|b| {
            if valid_counts[b] > 1 {
                (m2[b] / valid_counts[b] as f64).sqrt()
            } else {
                0.0
            }
        })
        .collect();

    let percentile_bounds: Vec<(f64, f64)> = percentile_lo.into_iter().zip(percentile_hi).collect();

    on_progress(RasterLoadProgress::StatsComplete {
        min_values: min_values.clone(),
        max_values: max_values.clone(),
        mean_values: means.clone(),
        std_dev_values: std_dev_values.clone(),
        percentile_bounds: percentile_bounds.clone(),
    });

    std::mem::drop(decoder);

    // Phase 4: read Sub-IFDs and OVR from a fresh decoder
    let file2 = File::open(path).map_err(|e| format!("Failed to open file: {}", e))?;
    let mut decoder2 = Decoder::new(BufReader::new(file2))
        .map_err(|e| format!("Failed to create TIFF decoder: {}", e))?;

    let mut all_ifds = vec![base_ifd];
    if let Ok(sub_val) = decoder2.get_tag(tiff::tags::Tag::SubIfd) {
        if let Ok(ifd_ptrs) = sub_val.into_ifd_vec() {
            for (i, ptr) in ifd_ptrs.iter().enumerate() {
                if let Ok(dir) = decoder2.read_directory(*ptr) {
                    let mut sub_reader = decoder2.read_directory_tags(&dir);
                    if let (Ok(Some(sub_w)), Ok(Some(sub_h))) = (
                        sub_reader.find_tag_unsigned::<u32>(tiff::tags::Tag::ImageWidth),
                        sub_reader.find_tag_unsigned::<u32>(tiff::tags::Tag::ImageLength),
                    ) {
                        let sub_cpr = match chunk_type {
                            ChunkType::Strip => 1u32,
                            ChunkType::Tile => (sub_w + chunk_w - 1) / chunk_w,
                        };
                        log::info!("Found sub-IFD {}: {}x{}", i, sub_w, sub_h);
                        all_ifds.push(IfdInfo {
                            index: all_ifds.len(),
                            width: sub_w,
                            height: sub_h,
                            chunk_type,
                            chunk_width: chunk_w,
                            chunk_length: chunk_h,
                            chunks_per_row: sub_cpr,
                            external: false,
                            ifd_ptr: None,
                            interleave,
                            file_path: path.to_string(),
                        });
                    }
                }
            }
        }
    }

    let ovr_path = find_ovr_file(path);
    if let Some(ref ovr_path) = ovr_path {
        match super::raster_ovr::parse_ovr_ifd_offsets(
            ovr_path,
            chunk_type,
            chunk_w,
            chunk_h,
            all_ifds.len(),
        ) {
            Ok(ovr_ifds) => {
                log::info!("Loaded {} overview(s) from .ovr file", ovr_ifds.len());
                all_ifds.extend(ovr_ifds);
            }
            Err(e) => {
                log::warn!("Failed to parse .ovr file '{}': {}", ovr_path, e);
            }
        }
    } else {
        // Auto-generate .ovr in background when none exists
        let path_clone = path.to_string();
        std::thread::spawn(move || {
            if let Err(e) = super::raster_ovr::generate_ovr(&path_clone) {
                log::warn!("generate_ovr failed for {}: {}", path_clone, e);
            } else {
                log::info!("Auto-generated .ovr for {}", path_clone);
            }
        });
    }

    std::mem::drop(decoder2);

    on_progress(RasterLoadProgress::OverviewsReady {
        total_ifds: all_ifds.len(),
        ovr_path: ovr_path.clone(),
    });

    // Phase 5: build and return
    let raster = Arc::new(CachedRaster {
        file_path: path.to_string(),
        width,
        height,
        bands,
        geo_transform,
        no_data,
        data_type,
        min_values,
        max_values,
        mean_values: means,
        std_dev_values,
        crs_type,
        geo_key,
        wgs84_corners,
        native_corners,
        ifds: all_ifds,
        percentile_bounds: std::sync::Mutex::new(Some(percentile_bounds)),
        ovr_path,
        max_zoom,
    });

    on_progress(RasterLoadProgress::Ready);

    Ok(raster)
}

fn load_and_cache_raster(path: &str) -> Result<Arc<CachedRaster>, String> {
    load_and_cache_raster_with_progress(path, |_| {})
}
