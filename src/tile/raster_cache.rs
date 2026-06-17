use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::mem::size_of;
use std::sync::{Arc, RwLock};

use std::sync::LazyLock;
use tiff::decoder::{ChunkType, Decoder, Limits};

use super::types::{CachedRaster, IfdInfo, InterleaveType};

static RASTER_CACHE: LazyLock<RwLock<HashMap<String, Arc<CachedRaster>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

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
        let cache = RASTER_CACHE
            .read()
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
        .read()
        .map_err(|e| format!("Cache lock error: {}", e))?;

    Ok(cache.iter().fold(0u64, |total, (path, raster)| {
        total
            .saturating_add(path.capacity() as u64)
            .saturating_add(estimate_raster_size_bytes(raster.as_ref()))
    }))
}

fn load_and_cache_raster(path: &str) -> Result<Arc<CachedRaster>, String> {
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
        }
    }

    for b in 0..bands {
        if valid_counts[b] == 0 {
            min_values[b] = 0.0;
            max_values[b] = 0.0;
        }
    }

    std::mem::drop(decoder);

    let file2 = File::open(path).map_err(|e| format!("Failed to open file: {}", e))?;
    let mut decoder2 = Decoder::new(BufReader::new(file2))
        .map_err(|e| format!("Failed to create TIFF decoder: {}", e))?;

    let geo_key = crate::reproject::read_geo_key_info(&mut decoder2).unwrap_or_default();
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
                        tracing::info!("Found sub-IFD {}: {}x{}", i, sub_w, sub_h);
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
                tracing::info!("Loaded {} overview(s) from .ovr file", ovr_ifds.len());
                all_ifds.extend(ovr_ifds);
            }
            Err(e) => {
                tracing::warn!("Failed to parse .ovr file '{}': {}", ovr_path, e);
            }
        }
    } else {
        // Auto-generate .ovr in background when none exists
        let path_clone = path.to_string();
        std::thread::spawn(move || {
            if let Err(e) = super::raster_ovr::generate_ovr(&path_clone) {
                tracing::warn!("generate_ovr failed for {}: {}", path_clone, e);
            } else {
                tracing::info!("Auto-generated .ovr for {}", path_clone);
            }
        });
    }

    std::mem::drop(decoder2);

    let max_zoom = {
        let gt = geo_transform;
        let is_geographic = crs_type == "Geographic" || crs_type == "Unknown";
        let raster_res_3857 = if is_geographic {
            let res_degree = gt[1].abs();
            res_degree * 111320.0
        } else {
            gt[1].abs()
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

    Ok(Arc::new(CachedRaster {
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
        std_dev_values: {
            let mut sd = vec![0.0f64; bands];
            for b in 0..bands {
                sd[b] = if valid_counts[b] > 1 {
                    (m2[b] / valid_counts[b] as f64).sqrt()
                } else {
                    0.0
                };
            }
            sd
        },
        crs_type,
        geo_key,
        wgs84_corners,
        native_corners,
        ifds: all_ifds,
        percentile_bounds: std::sync::Mutex::new(Some(
            percentile_lo.into_iter().zip(percentile_hi).collect(),
        )),
        ovr_path,
        max_zoom,
    }))
}
