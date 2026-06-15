use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::sync::{Arc, RwLock};

use geo::Intersects;
use image::{ImageEncoder, RgbaImage};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use tiff::decoder::{ChunkType, Decoder, Limits};

pub(crate) const R: f64 = 6378137.0;
pub(crate) const C: f64 = R * std::f64::consts::PI;

// ─── 数据结构 ───

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TileRequest {
    pub path: String,
    pub z: u32,
    pub x: u32,
    pub y: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorTileRequest {
    pub path: String,
    pub z: u32,
    pub x: u32,
    pub y: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TileInfo {
    pub data_type: String,
    pub min_zoom: u32,
    pub max_zoom: u32,
    pub crs: String,
    pub extent: [f64; 4],
    pub native_crs: String,
    pub native_extent: [f64; 4],
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct GeoFileInfo {
    pub name: String,
    pub path: String,
    pub data_type: String,
    pub info: TileInfo,
}

#[derive(Debug, Clone, Copy)]
struct IfdInfo {
    index: usize,
    width: u32,
    height: u32,
    chunk_type: ChunkType,
    chunk_width: u32,
    chunk_length: u32,
    chunks_per_row: u32,
}

#[allow(dead_code)]
pub(crate) struct CachedRaster {
    file_path: String,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) bands: usize,
    geo_transform: [f64; 6],
    no_data: Option<f64>,
    min_values: Vec<f64>,
    max_values: Vec<f64>,
    mean_values: Vec<f64>,
    std_dev_values: Vec<f64>,
    crs_type: String,
    geo_key: crate::reproject::GeoKeyInfo,
    wgs84_corners: [(f64, f64); 4],
    native_corners: [(f64, f64); 4],
    ifds: Vec<IfdInfo>,
    percentile_bounds: std::sync::Mutex<Option<Vec<(f64, f64)>>>,
}

static RASTER_CACHE: Lazy<RwLock<HashMap<String, Arc<CachedRaster>>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

// ─── 栅格加载与缓存 ───

pub(crate) fn get_raster(path: &str) -> Result<Arc<CachedRaster>, String> {
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

fn load_and_cache_raster(path: &str) -> Result<Arc<CachedRaster>, String> {
    let file = File::open(path).map_err(|e| format!("Failed to open raster: {}", e))?;
    let mut decoder = Decoder::new(BufReader::new(file))
        .map_err(|e| format!("Failed to create TIFF decoder: {}", e))?
        .with_limits(Limits::unlimited());

    let (width, height) = decoder
        .dimensions()
        .map_err(|e| format!("Failed to read dimensions: {}", e))?;

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

    let bands = {
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
    };

    let base_ifd = IfdInfo {
        index: 0,
        width,
        height,
        chunk_type,
        chunk_width: chunk_w,
        chunk_length: chunk_h,
        chunks_per_row: cpr,
    };

    let mut min_values = vec![f64::INFINITY; bands];
    let mut max_values = vec![f64::NEG_INFINITY; bands];
    let mut valid_counts = vec![0u64; bands];
    let mut means = vec![0.0f64; bands];
    let mut m2 = vec![0.0f64; bands];
    let mut percentile_samples: Vec<Vec<f64>> = (0..bands).map(|_| Vec::new()).collect();

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
                percentile_samples[b].push(val);
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
            for (_i, ptr) in ifd_ptrs.iter().enumerate() {
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
                        all_ifds.push(IfdInfo {
                            index: all_ifds.len(),
                            width: sub_w,
                            height: sub_h,
                            chunk_type,
                            chunk_width: chunk_w,
                            chunk_length: chunk_h,
                            chunks_per_row: sub_cpr,
                        });
                    }
                }
            }
        }
    }
    std::mem::drop(decoder2);

    Ok(Arc::new(CachedRaster {
        file_path: path.to_string(),
        width,
        height,
        bands,
        geo_transform,
        no_data,
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
    }))
}

fn read_geo_transform_tile(path: &str) -> [f64; 6] {
    if let Ok(file) = File::open(path) {
        if let Ok(mut decoder) = Decoder::new(BufReader::new(file)) {
            if let Ok(tiepoint) = decoder.get_tag_f64_vec(tiff::tags::Tag::ModelTiepointTag) {
                if let Ok(scale) = decoder.get_tag_f64_vec(tiff::tags::Tag::ModelPixelScaleTag) {
                    if tiepoint.len() >= 6 && scale.len() >= 3 {
                        return [
                            tiepoint[3],
                            scale[0],
                            0.0,
                            tiepoint[4],
                            0.0,
                            -scale[1],
                        ];
                    }
                }
            }
        }
    }

    [0.0, 1.0, 0.0, 0.0, 0.0, -1.0]
}

// ─── 栅格区域读取 ───

fn read_raster_region(
    path: &str,
    ifd: &IfdInfo,
    col_off: u32,
    row_off: u32,
    width: u32,
    height: u32,
    bands: usize,
    step: u32,
) -> Result<Vec<f64>, String> {
    let file = File::open(path).map_err(|e| format!("Failed to open for region read: {}", e))?;
    let mut decoder = Decoder::new(BufReader::new(file))
        .map_err(|e| format!("Failed to create decoder for region: {}", e))?
        .with_limits(Limits::unlimited());

    if ifd.index > 0 {
        if let Ok(sub_val) = decoder.get_tag(tiff::tags::Tag::SubIfd) {
            if let Ok(ifd_ptrs) = sub_val.into_ifd_vec() {
                let sub_idx = ifd.index - 1;
                if sub_idx < ifd_ptrs.len() {
                    let ptr = ifd_ptrs[sub_idx];
                    if let Ok(dir) = decoder.read_directory(ptr) {
                        decoder.read_directory_tags(&dir);
                    }
                }
            }
        }
    }

    let out_h = (height + step - 1) / step;
    let out_w = (width + step - 1) / step;
    let total_pixels = out_w as usize * out_h as usize;
    let mut result = vec![0.0f64; total_pixels * bands];

    match ifd.chunk_type {
        ChunkType::Strip => {
            let strip_len = ifd.chunk_length;
            for s in 0..ifd.height {
                let row_in_raster = row_off + s;
                if row_in_raster >= ifd.height {
                    break;
                }
                let strip_idx = (row_in_raster / strip_len) as u32;
                if let Ok(data) = decoder.read_chunk(strip_idx) {
                    let f64_data = crate::raster::decode_result_to_f64_vec(&data);
                    let ch = decoder.chunk_data_dimensions(strip_idx);
                    let strip_row_offset = row_in_raster % strip_len;
                    let _cols = ifd.width as usize;

                    if s % step == 0 {
                        let out_row = (s / step) as usize;
                        for c in 0..width {
                            if c % step == 0 {
                                let col_in_raster = col_off + c;
                                let out_col = (c / step) as usize;
                                let in_idx = (strip_row_offset * ch.0 as u32 + col_in_raster) as usize;
                                let out_idx = (out_row * out_w as usize + out_col) * bands;
                                for b in 0..bands {
                                    let src_idx = in_idx * bands + b;
                                    if src_idx < f64_data.len() {
                                        result[out_idx + b] = f64_data[src_idx];
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        ChunkType::Tile => {
            let tile_w = ifd.chunk_width;
            let tile_h = ifd.chunk_length;
            for t in 0.. {
                let tile_x = t % ifd.chunks_per_row;
                let tile_y = t / ifd.chunks_per_row;
                let tile_col = tile_x * tile_w;
                let tile_row = tile_y * tile_h;

                if tile_col >= col_off + width && tile_row >= row_off + height {
                    break;
                }
                if tile_col > col_off + width || tile_row > row_off + height {
                    if tile_y > row_off / tile_h + height / tile_h + 1 {
                        break;
                    }
                    continue;
                }
                if t >= decoder.tile_count().unwrap_or(u32::MAX) {
                    break;
                }

                if let Ok(data) = decoder.read_chunk(t) {
                    let f64_data = crate::raster::decode_result_to_f64_vec(&data);

                    let over_x = if tile_col >= col_off { 0u32 } else { col_off - tile_col };
                    let over_y = if tile_row >= row_off { 0u32 } else { row_off - tile_row };

                    let read_w = tile_w.saturating_sub(over_x).min(width.saturating_sub(tile_col.saturating_sub(col_off)));
                    let read_h = tile_h.saturating_sub(over_y).min(height.saturating_sub(tile_row.saturating_sub(row_off)));

                    for ly in 0..read_h {
                        if ly % step != 0 {
                            continue;
                        }
                        for lx in 0..read_w {
                            if lx % step != 0 {
                                continue;
                            }
                            let raster_col = tile_col + over_x + lx;
                            let raster_row = tile_row + over_y + ly;
                            if raster_col >= col_off + width || raster_row >= row_off + height {
                                continue;
                            }

                            let out_col = ((raster_col - col_off) / step) as usize;
                            let out_row = ((raster_row - row_off) / step) as usize;

                            let in_col = over_x + lx;
                            let in_row = over_y + ly;
                            let in_idx = (in_row * tile_w + in_col) as usize;
                            let out_idx = (out_row * out_w as usize + out_col) * bands;

                            for b in 0..bands {
                                let src_idx = in_idx * bands + b;
                                if src_idx < f64_data.len() {
                                    result[out_idx + b] = f64_data[src_idx];
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(result)
}

// ─── 栅格瓦片渲染 ───

pub fn render_raster_tile(
    raster: &CachedRaster,
    z: u32,
    x: u32,
    y: u32,
    size: u32,
    bands: &[u32],
) -> Result<(Vec<u8>, u32), String> {
    let (min_x, min_y, max_x, max_y) = tile_bounds_epsg3857(z, x, y, size);
    let range_x = max_x - min_x;
    let range_y = max_y - min_y;

    let sw = raster.wgs84_corners[0];
    let se = raster.wgs84_corners[1];
    let nw = raster.wgs84_corners[2];

    let lng_span = se.0 - sw.0;
    let lat_span = nw.1 - sw.1;

    let use_native = raster.crs_type == "Projected";
    let nc_sw = raster.native_corners[0];
    let nc_se = raster.native_corners[1];
    let nc_nw = raster.native_corners[2];
    let nc_span_x = nc_se.0 - nc_sw.0;
    let nc_span_y = nc_nw.1 - nc_sw.1;

    let corners_3857 = [
        (min_x, max_y),
        (max_x, max_y),
        (min_x, min_y),
        (max_x, min_y),
    ];
    let mut pixel_coords: Vec<(i64, i64)> = Vec::with_capacity(4);
    for &(wx, wy) in &corners_3857 {
        let lng = mercator_to_lng(wx);
        let lat = mercator_to_lat(wy);
        if let Some((col_i, row_i)) = if use_native {
            if let Some((nx, ny)) = crate::reproject::wgs84_to_native_crs(lng, lat, &raster.geo_key)
            {
                if nc_span_x.abs() <= f64::EPSILON || nc_span_y.abs() <= f64::EPSILON {
                    None
                } else {
                    let u = (nx - nc_sw.0) / nc_span_x;
                    let v = (ny - nc_sw.1) / nc_span_y;
                    let col = u * (raster.width as f64 - 1.0);
                    let row = (1.0 - v) * (raster.height as f64 - 1.0);
                    Some((col.round() as i64, row.round() as i64))
                }
            } else {
                None
            }
        } else {
            if lng_span.abs() <= f64::EPSILON || lat_span.abs() <= f64::EPSILON {
                None
            } else {
                let u = (lng - sw.0) / lng_span;
                let v = (lat - sw.1) / lat_span;
                let col = u * (raster.width as f64 - 1.0);
                let row = (1.0 - v) * (raster.height as f64 - 1.0);
                Some((col.round() as i64, row.round() as i64))
            }
        } {
            pixel_coords.push((col_i, row_i));
        }
    }

    let safe_padding = 10i64;
    let min_col = pixel_coords
        .iter()
        .map(|(c, _)| c)
        .min()
        .copied()
        .unwrap_or(0)
        .max(0);
    let max_col = pixel_coords
        .iter()
        .map(|(c, _)| c)
        .max()
        .copied()
        .unwrap_or(0);
    let min_row = pixel_coords
        .iter()
        .map(|(_, r)| r)
        .min()
        .copied()
        .unwrap_or(0)
        .max(0);
    let max_row = pixel_coords
        .iter()
        .map(|(_, r)| r)
        .max()
        .copied()
        .unwrap_or(0);

    let col_off = (min_col - safe_padding).max(0) as u32;
    let row_off = (min_row - safe_padding).max(0) as u32;
    let src_w = ((max_col - min_col + 2 * safe_padding).max(1)) as u32;
    let src_h = ((max_row - min_row + 2 * safe_padding).max(1)) as u32;

    let col_off = col_off.min(raster.width - 1);
    let row_off = row_off.min(raster.height - 1);
    let src_w = src_w.min(raster.width - col_off);
    let src_h = src_h.min(raster.height - row_off);

    let max_read_pixels = 1024u64 * 1024u64;
    let raw_pixels = src_w as u64 * src_h as u64;
    let step = if raw_pixels > max_read_pixels {
        let ratio = (raw_pixels as f64 / max_read_pixels as f64).sqrt();
        (ratio.ceil() as u32).max(1)
    } else {
        1
    };
    let read_w = (src_w + step - 1) / step;
    let read_h = (src_h + step - 1) / step;

    let region_data = if src_w >= 1 && src_h >= 1 {
        read_raster_region(
            &raster.file_path,
            &raster.ifds[0],
            col_off,
            row_off,
            src_w,
            src_h,
            raster.bands,
            step,
        )?
    } else {
        Vec::new()
    };

    let band_indices: Vec<usize> = bands.iter().map(|b| *b as usize - 1).collect();
    let use_grayscale = raster.bands < 3;

    let step_f = step as f64;
    let col_off_f = col_off as f64;
    let row_off_f = row_off as f64;
    let read_w_u = read_w as usize;

    let mut img = RgbaImage::new(size, size);
    let mut rendered: u32 = 0;

    for ty in 0..size {
        for tx in 0..size {
            let world_x = min_x + (tx as f64 + 0.5) / size as f64 * range_x;
            let world_y = max_y - (ty as f64 + 0.5) / size as f64 * range_y;

            let lng = mercator_to_lng(world_x);
            let lat = mercator_to_lat(world_y);

            let (u, v) = if use_native {
                if let Some((nx, ny)) =
                    crate::reproject::wgs84_to_native_crs(lng, lat, &raster.geo_key)
                {
                    let u = if nc_span_x.abs() > f64::EPSILON {
                        (nx - nc_sw.0) / nc_span_x
                    } else {
                        continue;
                    };
                    let v = if nc_span_y.abs() > f64::EPSILON {
                        (ny - nc_sw.1) / nc_span_y
                    } else {
                        continue;
                    };
                    (u, v)
                } else {
                    continue;
                }
            } else {
                let u = if lng_span.abs() > f64::EPSILON {
                    (lng - sw.0) / lng_span
                } else {
                    continue;
                };
                let v = if lat_span.abs() > f64::EPSILON {
                    (lat - sw.1) / lat_span
                } else {
                    continue;
                };
                (u, v)
            };

            let col = u * (raster.width as f64 - 1.0);
            let row = (1.0 - v) * (raster.height as f64 - 1.0);

            let col_i = col.round() as i64;
            let row_i = row.round() as i64;

            if col_i >= 0
                && col_i < raster.width as i64
                && row_i >= 0
                && row_i < raster.height as i64
            {
                let local_col_f = col_i as f64 - col_off_f;
                let local_row_f = row_i as f64 - row_off_f;
                let ds_col = (local_col_f / step_f) as usize;
                let ds_row = (local_row_f / step_f) as usize;

                if ds_col < read_w_u && ds_row < read_h as usize {
                    let idx = (ds_row * read_w_u + ds_col) * raster.bands;

                    let mut rgba = [0u8; 4];
                    rgba[3] = 255;
                    let mut pixel_is_nodata = false;

                    if use_grayscale {
                        let bi = 0usize;
                        if idx + bi < region_data.len() {
                            let val = region_data[idx + bi];
                            if crate::raster::is_nodata(val, raster.no_data) {
                                pixel_is_nodata = true;
                            } else {
                                let (min_v, max_v) = (raster.min_values[bi], raster.max_values[bi]);
                                let stretched = if (max_v - min_v).abs() > f64::EPSILON {
                                    ((val - min_v) / (max_v - min_v) * 255.0).clamp(0.0, 255.0)
                                } else {
                                    0.0
                                };
                                let gray = stretched as u8;
                                rgba[0] = gray;
                                rgba[1] = gray;
                                rgba[2] = gray;
                            }
                        } else {
                            pixel_is_nodata = true;
                        }
                    } else {
                        for (out_idx, &bi) in band_indices.iter().enumerate().take(3) {
                            if bi < raster.bands && idx + bi < region_data.len() {
                                let val = region_data[idx + bi];
                                if crate::raster::is_nodata(val, raster.no_data) {
                                    pixel_is_nodata = true;
                                    break;
                                }
                                let (min_v, max_v) = (raster.min_values[bi], raster.max_values[bi]);
                                let stretched = if (max_v - min_v).abs() > f64::EPSILON {
                                    ((val - min_v) / (max_v - min_v) * 255.0).clamp(0.0, 255.0)
                                } else {
                                    0.0
                                };
                                rgba[out_idx] = stretched as u8;
                            } else {
                                pixel_is_nodata = true;
                                break;
                            }
                        }
                    }

                    if !pixel_is_nodata {
                        img.put_pixel(tx, ty, image::Rgba(rgba));
                        rendered += 1;
                    }
                }
            }
        }
    }

    let mut png_bytes = Vec::new();
    {
        let encoder = image::codecs::png::PngEncoder::new(&mut png_bytes);
        encoder
            .write_image(&img, size, size, image::ExtendedColorType::Rgba8)
            .map_err(|e| format!("PNG encode error: {}", e))?;
    }

    Ok((png_bytes, rendered))
}

// ─── WMS 通用地图渲染（任意 BBOX） ───

pub fn render_map_bbox(
    raster: &CachedRaster,
    bbox: [f64; 4],
    width: u32,
    height: u32,
    bands: &[u32],
    transparent: bool,
) -> Result<Vec<u8>, String> {
    let (min_x, min_y, max_x, max_y) = (bbox[0], bbox[1], bbox[2], bbox[3]);
    let range_x = max_x - min_x;
    let range_y = max_y - min_y;

    let sw = raster.wgs84_corners[0];
    let se = raster.wgs84_corners[1];
    let nw = raster.wgs84_corners[2];

    let lng_span = se.0 - sw.0;
    let lat_span = nw.1 - sw.1;

    let use_native = raster.crs_type == "Projected";
    let nc_sw = raster.native_corners[0];
    let nc_se = raster.native_corners[1];
    let nc_nw = raster.native_corners[2];
    let nc_span_x = nc_se.0 - nc_sw.0;
    let nc_span_y = nc_nw.1 - nc_sw.1;

    let corners_3857 = [
        (min_x, max_y),
        (max_x, max_y),
        (min_x, min_y),
        (max_x, min_y),
    ];
    let mut pixel_coords: Vec<(i64, i64)> = Vec::with_capacity(4);
    for &(wx, wy) in &corners_3857 {
        let lng = mercator_to_lng(wx);
        let lat = mercator_to_lat(wy);
        if let Some((col_i, row_i)) = if use_native {
            if let Some((nx, ny)) = crate::reproject::wgs84_to_native_crs(lng, lat, &raster.geo_key)
            {
                if nc_span_x.abs() <= f64::EPSILON || nc_span_y.abs() <= f64::EPSILON {
                    None
                } else {
                    let u = (nx - nc_sw.0) / nc_span_x;
                    let v = (ny - nc_sw.1) / nc_span_y;
                    let col = u * (raster.width as f64 - 1.0);
                    let row = (1.0 - v) * (raster.height as f64 - 1.0);
                    Some((col.round() as i64, row.round() as i64))
                }
            } else {
                None
            }
        } else {
            if lng_span.abs() <= f64::EPSILON || lat_span.abs() <= f64::EPSILON {
                None
            } else {
                let u = (lng - sw.0) / lng_span;
                let v = (lat - sw.1) / lat_span;
                let col = u * (raster.width as f64 - 1.0);
                let row = (1.0 - v) * (raster.height as f64 - 1.0);
                Some((col.round() as i64, row.round() as i64))
            }
        } {
            pixel_coords.push((col_i, row_i));
        }
    }

    let safe_padding = 10i64;
    let min_col = pixel_coords
        .iter()
        .map(|(c, _)| c)
        .min()
        .copied()
        .unwrap_or(0)
        .max(0);
    let max_col = pixel_coords
        .iter()
        .map(|(c, _)| c)
        .max()
        .copied()
        .unwrap_or(0);
    let min_row = pixel_coords
        .iter()
        .map(|(_, r)| r)
        .min()
        .copied()
        .unwrap_or(0)
        .max(0);
    let max_row = pixel_coords
        .iter()
        .map(|(_, r)| r)
        .max()
        .copied()
        .unwrap_or(0);

    let col_off = (min_col - safe_padding).max(0) as u32;
    let row_off = (min_row - safe_padding).max(0) as u32;
    let src_w = ((max_col - min_col + 2 * safe_padding).max(1)) as u32;
    let src_h = ((max_row - min_row + 2 * safe_padding).max(1)) as u32;

    let col_off = col_off.min(raster.width - 1);
    let row_off = row_off.min(raster.height - 1);
    let src_w = src_w.min(raster.width - col_off);
    let src_h = src_h.min(raster.height - row_off);

    let max_read_pixels = 1024u64 * 1024u64;
    let raw_pixels = src_w as u64 * src_h as u64;
    let step = if raw_pixels > max_read_pixels {
        let ratio = (raw_pixels as f64 / max_read_pixels as f64).sqrt();
        (ratio.ceil() as u32).max(1)
    } else {
        1
    };
    let read_w = (src_w + step - 1) / step;
    let read_h = (src_h + step - 1) / step;

    let region_data = if src_w >= 1 && src_h >= 1 {
        read_raster_region(
            &raster.file_path,
            &raster.ifds[0],
            col_off,
            row_off,
            src_w,
            src_h,
            raster.bands,
            step,
        )?
    } else {
        Vec::new()
    };

    let band_indices: Vec<usize> = bands.iter().map(|b| *b as usize - 1).collect();
    let use_grayscale = raster.bands < 3;

    let step_f = step as f64;
    let col_off_f = col_off as f64;
    let row_off_f = row_off as f64;
    let read_w_u = read_w as usize;

    let mut img = image::RgbaImage::new(width, height);
    if !transparent {
        for pixel in img.pixels_mut() {
            *pixel = image::Rgba([255, 255, 255, 255]);
        }
    }
    let mut _rendered: u32 = 0;

    for ty in 0..height {
        for tx in 0..width {
            let world_x = min_x + (tx as f64 + 0.5) / width as f64 * range_x;
            let world_y = max_y - (ty as f64 + 0.5) / height as f64 * range_y;

            let lng = mercator_to_lng(world_x);
            let lat = mercator_to_lat(world_y);

            let (u, v) = if use_native {
                if let Some((nx, ny)) =
                    crate::reproject::wgs84_to_native_crs(lng, lat, &raster.geo_key)
                {
                    let u = if nc_span_x.abs() > f64::EPSILON {
                        (nx - nc_sw.0) / nc_span_x
                    } else {
                        continue;
                    };
                    let v = if nc_span_y.abs() > f64::EPSILON {
                        (ny - nc_sw.1) / nc_span_y
                    } else {
                        continue;
                    };
                    (u, v)
                } else {
                    continue;
                }
            } else {
                let u = if lng_span.abs() > f64::EPSILON {
                    (lng - sw.0) / lng_span
                } else {
                    continue;
                };
                let v = if lat_span.abs() > f64::EPSILON {
                    (lat - sw.1) / lat_span
                } else {
                    continue;
                };
                (u, v)
            };

            let col = u * (raster.width as f64 - 1.0);
            let row = (1.0 - v) * (raster.height as f64 - 1.0);

            let col_i = col.round() as i64;
            let row_i = row.round() as i64;

            if col_i >= 0
                && col_i < raster.width as i64
                && row_i >= 0
                && row_i < raster.height as i64
            {
                let local_col_f = col_i as f64 - col_off_f;
                let local_row_f = row_i as f64 - row_off_f;
                let ds_col = (local_col_f / step_f) as usize;
                let ds_row = (local_row_f / step_f) as usize;

                if ds_col < read_w_u && ds_row < read_h as usize {
                    let idx = (ds_row * read_w_u + ds_col) * raster.bands;

                    let mut rgba = [0u8; 4];
                    rgba[3] = 255;
                    let mut pixel_is_nodata = false;

                    if use_grayscale {
                        let bi = 0usize;
                        if idx + bi < region_data.len() {
                            let val = region_data[idx + bi];
                            if crate::raster::is_nodata(val, raster.no_data) {
                                pixel_is_nodata = true;
                            } else {
                                let (min_v, max_v) = (raster.min_values[bi], raster.max_values[bi]);
                                let stretched = if (max_v - min_v).abs() > f64::EPSILON {
                                    ((val - min_v) / (max_v - min_v) * 255.0).clamp(0.0, 255.0)
                                } else {
                                    0.0
                                };
                                let gray = stretched as u8;
                                rgba[0] = gray;
                                rgba[1] = gray;
                                rgba[2] = gray;
                            }
                        } else {
                            pixel_is_nodata = true;
                        }
                    } else {
                        for (out_idx, &bi) in band_indices.iter().enumerate().take(3) {
                            if bi < raster.bands && idx + bi < region_data.len() {
                                let val = region_data[idx + bi];
                                if crate::raster::is_nodata(val, raster.no_data) {
                                    pixel_is_nodata = true;
                                    break;
                                }
                                let (min_v, max_v) = (raster.min_values[bi], raster.max_values[bi]);
                                let stretched = if (max_v - min_v).abs() > f64::EPSILON {
                                    ((val - min_v) / (max_v - min_v) * 255.0).clamp(0.0, 255.0)
                                } else {
                                    0.0
                                };
                                rgba[out_idx] = stretched as u8;
                            } else {
                                pixel_is_nodata = true;
                                break;
                            }
                        }
                    }

                    if !pixel_is_nodata {
                        img.put_pixel(tx, ty, image::Rgba(rgba));
                        _rendered += 1;
                    }
                }
            }
        }
    }

    let mut png_bytes = Vec::new();
    {
        let encoder = image::codecs::png::PngEncoder::new(&mut png_bytes);
        encoder
            .write_image(&img, width, height, image::ExtendedColorType::Rgba8)
            .map_err(|e| format!("PNG encode error: {}", e))?;
    }

    Ok(png_bytes)
}

// ─── 瓦片坐标数学 ───

fn tile_bounds_epsg3857(z: u32, x: u32, y: u32, tile_size: u32) -> (f64, f64, f64, f64) {
    let n = (1u64 << z) as f64;
    let res = 2.0 * C / (tile_size as f64 * n);
    let min_x = -C + x as f64 * tile_size as f64 * res;
    let max_x = -C + (x as f64 + 1.0) * tile_size as f64 * res;
    let max_y = C - y as f64 * tile_size as f64 * res;
    let min_y = C - (y as f64 + 1.0) * tile_size as f64 * res;
    (min_x, min_y, max_x, max_y)
}

fn mercator_to_lng(merc_x: f64) -> f64 {
    merc_x * 180.0 / C
}

fn mercator_to_lat(merc_y: f64) -> f64 {
    let val = (merc_y / R).exp();
    let lat_rad = 2.0 * (val.atan() - std::f64::consts::FRAC_PI_4);
    lat_rad.to_degrees()
}

fn clamp_lat(lat: f64) -> f64 {
    const MAX_LAT: f64 = 85.051129;
    lat.clamp(-MAX_LAT, MAX_LAT)
}

pub fn wgs84_tile_rect(z: u32, x: u32, y: u32) -> geo_types::Rect<f64> {
    let size = 256;
    let (min_x, min_y, max_x, max_y) = tile_bounds_epsg3857(z, x, y, size);

    let min_lng = mercator_to_lng(min_x);
    let max_lng = mercator_to_lng(max_x);
    let min_lat = clamp_lat(mercator_to_lat(min_y));
    let max_lat = clamp_lat(mercator_to_lat(max_y));

    geo_types::Rect::new(
        geo_types::coord! { x: min_lng, y: min_lat },
        geo_types::coord! { x: max_lng, y: max_lat },
    )
}

// ─── 矢量瓦片 ───

pub fn get_vector_tile_geojson(req: &VectorTileRequest) -> Result<String, String> {
    let tile_rect = wgs84_tile_rect(req.z, req.x, req.y);

    let content =
        std::fs::read_to_string(&req.path).map_err(|e| format!("Failed to read file: {}", e))?;
    let geojson: geojson::GeoJson = content
        .parse()
        .map_err(|e| format!("Invalid GeoJSON: {}", e))?;

    let source_crs = resolve_geojson_source_crs(&geojson)?;
    let features = collect_geojson_features(&geojson, source_crs, &tile_rect)?;

    let fc = geojson::FeatureCollection {
        bbox: None,
        features,
        foreign_members: None,
    };
    serde_json::to_string(&fc).map_err(|e| format!("Serialization error: {}", e))
}

// ─── Shapefile 瓦片 ───

pub fn get_shapefile_tile_geojson(req: &VectorTileRequest) -> Result<String, String> {
    let tile_rect = wgs84_tile_rect(req.z, req.x, req.y);
    let sf = crate::shapefile_reader::get_shapefile(&req.path)?;

    let mut features = Vec::new();
    for (i, geom) in sf.geometries.iter().enumerate() {
        if !geom.intersects(&tile_rect) {
            continue;
        }
        let props = sf.attributes.get(i).and_then(|a| a.clone());
        let gj_geom = geojson::Geometry::try_from(geom)
            .map_err(|e| format!("Geometry conversion error: {}", e))?;
        features.push(geojson::Feature {
            bbox: None,
            geometry: Some(gj_geom),
            id: None,
            properties: props,
            foreign_members: None,
        });
    }

    let fc = geojson::FeatureCollection {
        bbox: None,
        features,
        foreign_members: None,
    };
    serde_json::to_string(&fc).map_err(|e| format!("Serialization error: {}", e))
}

fn resolve_geojson_source_crs(geojson: &geojson::GeoJson) -> Result<crate::reproject::KnownCrs, String> {
    let crs_name = match geojson {
        geojson::GeoJson::FeatureCollection(fc) => fc
            .foreign_members
            .as_ref()
            .and_then(|members| members.get("crs"))
            .and_then(extract_geojson_crs_name),
        geojson::GeoJson::Feature(f) => f
            .foreign_members
            .as_ref()
            .and_then(|members| members.get("crs"))
            .and_then(extract_geojson_crs_name),
        geojson::GeoJson::Geometry(_) => None,
    };

    match crs_name {
        Some(name) => crate::reproject::parse_known_crs(&name)
            .ok_or_else(|| format!("Unsupported GeoJSON CRS: {name}")),
        None => Ok(crate::reproject::KnownCrs::Wgs84),
    }
}

fn extract_geojson_crs_name(value: &serde_json::Value) -> Option<String> {
    value
        .as_object()?
        .get("properties")?
        .as_object()?
        .get("name")?
        .as_str()
        .map(|value| value.to_string())
}

fn collect_geojson_features(
    geojson: &geojson::GeoJson,
    source_crs: crate::reproject::KnownCrs,
    tile_rect: &geo_types::Rect<f64>,
) -> Result<Vec<geojson::Feature>, String> {
    match geojson {
        geojson::GeoJson::FeatureCollection(fc) => fc
            .features
            .iter()
            .filter_map(|feature| {
                transform_geojson_feature(feature, source_crs, tile_rect).transpose()
            })
            .collect(),
        geojson::GeoJson::Feature(feature) => {
            transform_geojson_feature(feature, source_crs, tile_rect)
                .map(|feature| feature.into_iter().collect())
        }
        geojson::GeoJson::Geometry(geometry) => {
            let feature = geojson::Feature {
                bbox: None,
                geometry: Some(geometry.clone()),
                id: None,
                properties: None,
                foreign_members: None,
            };
            transform_geojson_feature(&feature, source_crs, tile_rect)
                .map(|feature| feature.into_iter().collect())
        }
    }
}

fn transform_geojson_feature(
    feature: &geojson::Feature,
    source_crs: crate::reproject::KnownCrs,
    tile_rect: &geo_types::Rect<f64>,
) -> Result<Option<geojson::Feature>, String> {
    let Some(geometry) = feature.geometry.as_ref() else {
        return Ok(None);
    };

    let geometry = geo_types::Geometry::<f64>::try_from(geometry)
        .map_err(|e| format!("Failed to convert GeoJSON geometry: {e}"))?;
    let geometry = crate::reproject::known_crs_geometry_to_wgs84(&geometry, source_crs)
        .ok_or_else(|| "Failed to reproject GeoJSON geometry to WGS84".to_string())?;

    if !geometry.intersects(tile_rect) {
        return Ok(None);
    }

    Ok(Some(geojson::Feature {
        bbox: None,
        geometry: Some(
            geojson::Geometry::try_from(&geometry)
                .map_err(|e| format!("Failed to convert back to GeoJSON: {e}"))?,
        ),
        id: feature.id.clone(),
        properties: feature.properties.clone(),
        foreign_members: feature.foreign_members.clone(),
    }))
}

// ─── 元数据 ───

pub fn get_raster_tile_info(path: &str) -> Result<TileInfo, String> {
    let raster = get_raster(path)?;
    let gt = raster.geo_transform;

    let is_geographic = raster.crs_type == "Geographic" || raster.crs_type == "Unknown";

    let (extent_wgs84, raster_res_3857) = if is_geographic {
        let min_lng = gt[0];
        let max_lng = gt[0] + raster.width as f64 * gt[1];
        let min_lat = gt[3] + raster.height as f64 * gt[5];
        let max_lat = gt[3];

        let res_degree = gt[1].abs();
        let res_meters = res_degree * 111320.0;

        ([min_lng, min_lat, max_lng, max_lat], res_meters)
    } else {
        if let Some(extent_wgs84) =
            crate::reproject::extent_to_wgs84(&gt, raster.width, raster.height, &raster.geo_key)
        {
            let res_meters = gt[1].abs();
            (extent_wgs84, res_meters)
        } else {
            ([0.0, 0.0, 0.0, 0.0], gt[1].abs())
        }
    };

    let max_zoom = if raster_res_3857 > 0.0 {
        let ratio = (2.0 * C) / (256.0 * raster_res_3857);
        if ratio > 1.0 {
            ratio.log2().ceil() as u32
        } else {
            0
        }
    } else {
        22
    };

    let native_crs = crate::raster::crs_string_from_geo_key(&raster.geo_key);

    let nc = &raster.native_corners;
    let native_extent = [
        nc[0].0.min(nc[1].0).min(nc[2].0).min(nc[3].0),
        nc[0].1.min(nc[1].1).min(nc[2].1).min(nc[3].1),
        nc[0].0.max(nc[1].0).max(nc[2].0).max(nc[3].0),
        nc[0].1.max(nc[1].1).max(nc[2].1).max(nc[3].1),
    ];

    Ok(TileInfo {
        data_type: "raster".to_string(),
        min_zoom: 0,
        max_zoom: max_zoom.min(22),
        crs: "EPSG:4326".to_string(),
        extent: extent_wgs84,
        native_crs,
        native_extent,
    })
}

pub fn get_vector_tile_info() -> TileInfo {
    TileInfo {
        data_type: "vector".to_string(),
        min_zoom: 0,
        max_zoom: 22,
        crs: "EPSG:4326".to_string(),
        extent: [-C, -C, C, C],
        native_crs: "EPSG:4326".to_string(),
        native_extent: [-C, -C, C, C],
    }
}
