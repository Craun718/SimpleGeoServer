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

pub use crate::resample::{ResamplingMode, StretchConfig};

// ─── Raster Block Iterator ───

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct RasterBlockConfig {
    pub block_width: u32,
    pub block_height: u32,
    pub overlap: u32,
    pub step: u32,
}

impl Default for RasterBlockConfig {
    fn default() -> Self {
        Self { block_width: 512, block_height: 512, overlap: 0, step: 1 }
    }
}

#[allow(dead_code)]
pub struct RasterBlock {
    pub block_col: u32,
    pub block_row: u32,
    pub col_off: u32,
    pub row_off: u32,
    pub width: u32,
    pub height: u32,
    pub col_off_raw: u32,
    pub row_off_raw: u32,
    pub width_raw: u32,
    pub height_raw: u32,
    pub data: Vec<f64>,
}

#[allow(dead_code)]
pub struct RasterBlockIterator {
    ifd: IfdInfo,
    config: RasterBlockConfig,
    blocks_x: u32,
    blocks_y: u32,
    current_block: u32,
    bands: usize,
    file_path: String,
}

#[allow(dead_code)]
impl RasterBlockIterator {
    pub fn new(path: &str, ifd: &IfdInfo, config: RasterBlockConfig, bands: usize) -> Self {
        let blocks_x = (ifd.width + config.block_width - 1) / config.block_width;
        let blocks_y = (ifd.height + config.block_height - 1) / config.block_height;
        Self {
            ifd: ifd.clone(),
            config,
            blocks_x,
            blocks_y,
            current_block: 0,
            bands,
            file_path: path.to_string(),
        }
    }

    pub fn next_block(&mut self) -> Option<Result<RasterBlock, String>> {
        if self.current_block >= self.blocks_x * self.blocks_y {
            return None;
        }

        let block_col = self.current_block % self.blocks_x;
        let block_row = self.current_block / self.blocks_x;
        let col_off = block_col * self.config.block_width;
        let row_off = block_row * self.config.block_height;
        let bw = self.config.block_width.min(self.ifd.width - col_off);
        let bh = self.config.block_height.min(self.ifd.height - row_off);

        let overlap = self.config.overlap;
        let col_off_raw = col_off.saturating_sub(overlap);
        let row_off_raw = row_off.saturating_sub(overlap);
        let width_raw = (bw + overlap * 2).min(self.ifd.width - col_off_raw);
        let height_raw = (bh + overlap * 2).min(self.ifd.height - row_off_raw);

        let data = match read_raster_region(
            &self.file_path,
            None,
            &self.ifd,
            col_off_raw,
            row_off_raw,
            width_raw,
            height_raw,
            self.bands,
            self.config.step,
        ) {
            Ok(d) => d,
            Err(e) => return Some(Err(e)),
        };

        self.current_block += 1;

        Some(Ok(RasterBlock {
            block_col,
            block_row,
            col_off,
            row_off,
            width: bw,
            height: bh,
            col_off_raw,
            row_off_raw,
            width_raw,
            height_raw,
            data,
        }))
    }
}

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

#[derive(Debug, Clone)]
pub(crate) struct IfdInfo {
    index: usize,
    width: u32,
    height: u32,
    chunk_type: ChunkType,
    chunk_width: u32,
    chunk_length: u32,
    chunks_per_row: u32,
    external: bool,
    ifd_ptr: Option<u64>,
}

#[allow(dead_code)]
pub(crate) struct CachedRaster {
    pub(crate) file_path: String,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) bands: usize,
    pub(crate) geo_transform: [f64; 6],
    pub(crate) no_data: Option<f64>,
    pub(crate) min_values: Vec<f64>,
    pub(crate) max_values: Vec<f64>,
    pub(crate) mean_values: Vec<f64>,
    pub(crate) std_dev_values: Vec<f64>,
    pub(crate) crs_type: String,
    pub(crate) geo_key: crate::reproject::GeoKeyInfo,
    pub(crate) wgs84_corners: [(f64, f64); 4],
    pub(crate) native_corners: [(f64, f64); 4],
    pub(crate) ifds: Vec<IfdInfo>,
    pub(crate) percentile_bounds: std::sync::Mutex<Option<Vec<(f64, f64)>>>,
    pub(crate) ovr_path: Option<String>,
    pub(crate) max_zoom: u32,
}

static RASTER_CACHE: Lazy<RwLock<HashMap<String, Arc<CachedRaster>>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

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

fn read_u16_ifd_entry(
    entries: &[(u16, u32)],
    tag_id: u16,
) -> Option<u32> {
    entries.iter().find(|(t, _)| *t == tag_id).map(|(_, v)| *v)
}

fn parse_ovr_ifd_offsets(
    ovr_path: &str,
    _base_chunk_type: ChunkType,
    _base_chunk_w: u32,
    _base_chunk_h: u32,
    base_index: usize,
) -> Result<Vec<IfdInfo>, String> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(ovr_path)
        .map_err(|e| format!("Failed to open .ovr: {}", e))?;

    let mut header = [0u8; 8];
    file.read_exact(&mut header)
        .map_err(|e| format!("Failed to read TIFF header from .ovr: {}", e))?;

    let byte_order = u16::from_le_bytes([header[0], header[1]]);
    let first_ifd_offset = if byte_order == 0x4949 {
        u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as u64
    } else if byte_order == 0x4D4D {
        u32::from_be_bytes([header[4], header[5], header[6], header[7]]) as u64
    } else {
        return Err("Invalid TIFF byte order in .ovr".to_string());
    };

    let read_entry = |file: &mut std::fs::File, off: u64| -> Option<(u16, u32)> {
        let mut entry = [0u8; 12];
        file.seek(SeekFrom::Start(off)).ok()?;
        file.read_exact(&mut entry).ok()?;
        let tag = if byte_order == 0x4949 {
            u16::from_le_bytes([entry[0], entry[1]])
        } else {
            u16::from_be_bytes([entry[0], entry[1]])
        };
        let val = if byte_order == 0x4949 {
            u32::from_le_bytes([entry[8], entry[9], entry[10], entry[11]])
        } else {
            u32::from_be_bytes([entry[8], entry[9], entry[10], entry[11]])
        };
        Some((tag, val))
    };

    let mut ifds = Vec::new();
    let mut current_offset = Some(first_ifd_offset);

    while let Some(ifd_off) = current_offset {
        file.seek(SeekFrom::Start(ifd_off))
            .map_err(|e| format!("Failed to seek in .ovr: {}", e))?;

        let mut count_buf = [0u8; 2];
        if file.read_exact(&mut count_buf).is_err() {
            break;
        }
        let entry_count = if byte_order == 0x4949 {
            u16::from_le_bytes(count_buf)
        } else {
            u16::from_be_bytes(count_buf)
        } as usize;

        let mut entries: Vec<(u16, u32)> = Vec::new();
        let mut entry_base = ifd_off + 2;
        for _ in 0..entry_count {
            if let Some(e) = read_entry(&mut file, entry_base) {
                entries.push(e);
            }
            entry_base += 12;
        }

        let width = read_u16_ifd_entry(&entries, 0x0100).unwrap_or(0);
        let height = read_u16_ifd_entry(&entries, 0x0101).unwrap_or(0);

        if width == 0 || height == 0 {
            // Read next IFD offset
            let next_pos = entry_base;
            if file.seek(SeekFrom::Start(next_pos)).is_err() {
                break;
            }
            let mut next_buf = [0u8; 4];
            if file.read_exact(&mut next_buf).is_err() {
                break;
            }
            let next_off = if byte_order == 0x4949 {
                u32::from_le_bytes(next_buf)
            } else {
                u32::from_be_bytes(next_buf)
            };
            current_offset = if next_off != 0 { Some(next_off as u64) } else { None };
            continue;
        }

        // Detect chunk structure from the .ovr IFD itself
        let (chunk_type, chunk_w, chunk_h) =
            if let Some(tw) = read_u16_ifd_entry(&entries, 0x0142) {
                let tl = read_u16_ifd_entry(&entries, 0x0143).unwrap_or(tw);
                (ChunkType::Tile, tw, tl)
            } else {
                let rows_per_strip = read_u16_ifd_entry(&entries, 0x0117).unwrap_or(height);
                (ChunkType::Strip, width, rows_per_strip)
            };

        let cpr = match chunk_type {
            ChunkType::Strip => 1u32,
            ChunkType::Tile => (width + chunk_w - 1) / chunk_w,
        };

        ifds.push(IfdInfo {
            index: base_index + ifds.len(),
            width,
            height,
            chunk_type,
            chunk_width: chunk_w,
            chunk_length: chunk_h,
            chunks_per_row: cpr,
            external: true,
            ifd_ptr: Some(ifd_off),
        });

        // Read next IFD offset
        let next_pos = entry_base;
        if file.seek(SeekFrom::Start(next_pos)).is_err() {
            break;
        }
        let mut next_buf = [0u8; 4];
        if file.read_exact(&mut next_buf).is_err() {
            break;
        }
        let next_off = if byte_order == 0x4949 {
            u32::from_le_bytes(next_buf)
        } else {
            u32::from_be_bytes(next_buf)
        };
        current_offset = if next_off != 0 { Some(next_off as u64) } else { None };
    }

    Ok(ifds)
}

#[allow(dead_code)]
fn bilinear_downsample_f64(
    data: &[f64],
    src_w: u32,
    src_h: u32,
    bands: usize,
    dst_w: u32,
    dst_h: u32,
) -> Vec<f64> {
    let mut result = vec![0.0; dst_w as usize * dst_h as usize * bands];
    let x_ratio = src_w as f64 / dst_w as f64;
    let y_ratio = src_h as f64 / dst_h as f64;

    for dy in 0..dst_h {
        for dx in 0..dst_w {
            let sx = (dx as f64 + 0.5) * x_ratio - 0.5;
            let sy = (dy as f64 + 0.5) * y_ratio - 0.5;

            let sx0 = if sx <= 0.0 { 0 } else { sx.floor() as u32 };
            let sy0 = if sy <= 0.0 { 0 } else { sy.floor() as u32 };
            let sx1 = (sx0 + 1).min(src_w - 1);
            let sy1 = (sy0 + 1).min(src_h - 1);

            let fx = sx - sx0 as f64;
            let fy = sy - sy0 as f64;

            for b in 0..bands {
                let idx00 = (sy0 as usize * src_w as usize + sx0 as usize) * bands + b;
                let idx10 = (sy0 as usize * src_w as usize + sx1 as usize) * bands + b;
                let idx01 = (sy1 as usize * src_w as usize + sx0 as usize) * bands + b;
                let idx11 = (sy1 as usize * src_w as usize + sx1 as usize) * bands + b;

                let v00 = data[idx00];
                let v10 = data[idx10];
                let v01 = data[idx01];
                let v11 = data[idx11];

                let v = v00 * (1.0 - fx) * (1.0 - fy)
                    + v10 * fx * (1.0 - fy)
                    + v01 * (1.0 - fx) * fy
                    + v11 * fx * fy;

                let out_idx = (dy as usize * dst_w as usize + dx as usize) * bands + b;
                result[out_idx] = v;
            }
        }
    }

    result
}

#[allow(dead_code)]
pub fn generate_ovr(path: &str) -> Result<(), String> {
    use tiff::encoder::{TiffEncoder, colortype};
    use tiff::tags::ExtraSamples;

    let file = std::fs::File::open(path)
        .map_err(|e| format!("Failed to open {}: {}", path, e))?;
    let mut decoder = Decoder::new(std::io::BufReader::new(file))
        .map_err(|e| format!("Failed to create decoder: {}", e))?
        .with_limits(Limits::unlimited());

    let (width, height) = decoder
        .dimensions()
        .map_err(|e| format!("Failed to read dimensions: {}", e))?;

    let decoded = decoder
        .read_image()
        .map_err(|e| format!("Failed to read image: {}", e))?;

    let f64_data = crate::raster::decode_result_to_f64_vec(&decoded);
    let total_pixels = width as usize * height as usize;
    if total_pixels == 0 {
        return Err("Image has zero pixels".to_string());
    }
    let bands = if f64_data.len() >= total_pixels && total_pixels > 0 {
        f64_data.len() / total_pixels
    } else {
        1
    };

    let ovr_path = format!("{}.ovr", path);
    let file_out = std::fs::File::create(&ovr_path)
        .map_err(|e| format!("Failed to create .ovr: {}", e))?;
    let mut tiff = TiffEncoder::new(file_out)
        .map_err(|e| format!("Failed to create TIFF encoder: {}", e))?;

    let min_size = 256u32;
    let max_levels = 8usize;
    let mut level_count = 0usize;
    let mut prev_w = width;
    let mut prev_h = height;
    let all_bands = bands;

    // First check if bands > 4 - we can't write these with standard ColorType
    if bands > 4 {
        return Err(format!(
            "Cannot generate .ovr for {} bands (max supported: 4)",
            bands
        ));
    }

    loop {
        let new_w = (prev_w / 2).max(1);
        let new_h = (prev_h / 2).max(1);

        if new_w == prev_w && new_h == prev_h {
            break;
        }
        if prev_w <= min_size && prev_h <= min_size {
            break;
        }
        if level_count >= max_levels {
            break;
        }

        let downsampled = bilinear_downsample_f64(
            &f64_data,
            prev_w,
            prev_h,
            all_bands,
            new_w,
            new_h,
        );

        // Map to ColorType and write
        macro_rules! write_level {
            ($colortype:ty, $inner_type:ty, $max_val:expr) => {{
                let converted: Vec<$inner_type> = downsampled
                    .iter()
                    .map(|v| {
                        let clamped = v.clamp(0.0, $max_val);
                        clamped.round() as $inner_type
                    })
                    .collect();
                tiff.write_image::<$colortype>(new_w, new_h, &converted)
                    .map_err(|e| format!("Failed to write .ovr level {}: {}", level_count, e))?;
            }};
            ($colortype:ty, $inner_type:ty) => {{
                let converted: Vec<$inner_type> = downsampled
                    .iter()
                    .map(|v| *v as $inner_type)
                    .collect();
                tiff.write_image::<$colortype>(new_w, new_h, &converted)
                    .map_err(|e| format!("Failed to write .ovr level {}: {}", level_count, e))?;
            }};
        }

        match &decoded {
            tiff::decoder::DecodingResult::U8(_) => match all_bands {
                1 => write_level!(colortype::Gray8, u8, 255.0),
                2 => {
                    let gray: Vec<u8> = downsampled.iter().step_by(2).map(|v| v.round().clamp(0.0, 255.0) as u8).collect();
                    let extra: Vec<u8> = downsampled.iter().skip(1).step_by(2).map(|v| v.round().clamp(0.0, 255.0) as u8).collect();
                    let mut interleaved = Vec::with_capacity(gray.len() + extra.len());
                    for i in 0..gray.len() {
                        interleaved.push(gray[i]);
                        interleaved.push(extra[i]);
                    }
                    let mut image = tiff.new_image::<colortype::Gray8>(new_w, new_h)
                        .map_err(|e| format!("Failed to create image encoder: {}", e))?;
                    image.extra_samples(&[ExtraSamples::Unspecified])
                        .map_err(|e| format!("Failed to set extra samples: {}", e))?;
                    image.write_data(&interleaved)
                        .map_err(|e| format!("Failed to write .ovr level {}: {}", level_count, e))?;
                }
                3 => write_level!(colortype::RGB8, u8, 255.0),
                4 => write_level!(colortype::RGBA8, u8, 255.0),
                _ => unreachable!(),
            },
            tiff::decoder::DecodingResult::U16(_) => match all_bands {
                1 => write_level!(colortype::Gray16, u16, 65535.0),
                2 => {
                    let gray: Vec<u16> = downsampled.iter().step_by(2).map(|v| v.round().clamp(0.0, 65535.0) as u16).collect();
                    let extra: Vec<u16> = downsampled.iter().skip(1).step_by(2).map(|v| v.round().clamp(0.0, 65535.0) as u16).collect();
                    let mut interleaved = Vec::with_capacity(gray.len() + extra.len());
                    for i in 0..gray.len() {
                        interleaved.push(gray[i]);
                        interleaved.push(extra[i]);
                    }
                    let mut image = tiff.new_image::<colortype::Gray16>(new_w, new_h)
                        .map_err(|e| format!("Failed to create image encoder: {}", e))?;
                    image.extra_samples(&[ExtraSamples::Unspecified])
                        .map_err(|e| format!("Failed to set extra samples: {}", e))?;
                    image.write_data(&interleaved)
                        .map_err(|e| format!("Failed to write .ovr level {}: {}", level_count, e))?;
                }
                3 => write_level!(colortype::RGB16, u16, 65535.0),
                4 => write_level!(colortype::RGBA16, u16, 65535.0),
                _ => unreachable!(),
            },
            tiff::decoder::DecodingResult::F32(_) => match all_bands {
                1 => write_level!(colortype::Gray32Float, f32),
                2 => {
                    let gray: Vec<f32> = downsampled.iter().step_by(2).map(|v| *v as f32).collect();
                    let extra: Vec<f32> = downsampled.iter().skip(1).step_by(2).map(|v| *v as f32).collect();
                    let mut interleaved = Vec::with_capacity(gray.len() + extra.len());
                    for i in 0..gray.len() {
                        interleaved.push(gray[i]);
                        interleaved.push(extra[i]);
                    }
                    let mut image = tiff.new_image::<colortype::Gray32Float>(new_w, new_h)
                        .map_err(|e| format!("Failed to create image encoder: {}", e))?;
                    image.extra_samples(&[ExtraSamples::Unspecified])
                        .map_err(|e| format!("Failed to set extra samples: {}", e))?;
                    image.write_data(&interleaved)
                        .map_err(|e| format!("Failed to write .ovr level {}: {}", level_count, e))?;
                }
                3 => write_level!(colortype::RGB32Float, f32),
                4 => write_level!(colortype::RGBA32Float, f32),
                _ => unreachable!(),
            },
            tiff::decoder::DecodingResult::F64(_) => match all_bands {
                1 => write_level!(colortype::Gray64Float, f64),
                3 => write_level!(colortype::RGB64Float, f64),
                4 => write_level!(colortype::RGBA64Float, f64),
                _ => {
                    return Err(format!(
                        "Unsupported band count {} for f64 .ovr",
                        all_bands
                    ));
                }
            },
            _ => {
                return Err("Unsupported data type for .ovr generation".to_string());
            }
        }

        prev_w = new_w;
        prev_h = new_h;
        level_count += 1;
    }

    tracing::info!("Generated .ovr with {} levels: {}", level_count, ovr_path);
    Ok(())
}

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
        external: false,
        ifd_ptr: None,
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
                            external: false,
                            ifd_ptr: None,
                        });
                    }
                }
            }
        }
    }

    let ovr_path = find_ovr_file(path);
    if let Some(ref ovr_path) = ovr_path {
        match parse_ovr_ifd_offsets(ovr_path, chunk_type, chunk_w, chunk_h, all_ifds.len()) {
            Ok(ovr_ifds) => {
                tracing::info!("Loaded {} overview(s) from .ovr file", ovr_ifds.len());
                all_ifds.extend(ovr_ifds);
            }
            Err(e) => {
                tracing::warn!("Failed to parse .ovr file '{}': {}", ovr_path, e);
            }
        }
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
            let ratio = (2.0 * C) / (256.0 * raster_res_3857);
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

fn select_ifd_for_zoom(raster: &CachedRaster, z: u32) -> usize {
    if raster.ifds.len() <= 1 {
        return 0;
    }
    let size = 256u32;
    let (min_x, _min_y, max_x, _max_y) = tile_bounds_epsg3857(z, 0, 0, size);
    let tile_width_m = (max_x - min_x).abs();
    let tile_res_meters = tile_width_m / size as f64;

    let is_geographic = raster.crs_type == "Geographic" || raster.crs_type == "Unknown";
    let base_m_per_px = if is_geographic {
        raster.geo_transform[1].abs() * 111320.0
    } else {
        raster.geo_transform[1].abs()
    };

    if base_m_per_px <= 0.0 || tile_res_meters <= 0.0 {
        return 0;
    }

    let mut best_idx = 0usize;
    let mut best_diff = f64::INFINITY;
    for (i, ifd) in raster.ifds.iter().enumerate() {
        let scale = raster.width as f64 / ifd.width as f64;
        let ifd_res = base_m_per_px * scale;
        let diff = (ifd_res - tile_res_meters).abs();
        if diff < best_diff {
            best_diff = diff;
            best_idx = i;
        }
    }
    best_idx
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
    ovr_path: Option<&str>,
    ifd: &IfdInfo,
    col_off: u32,
    row_off: u32,
    width: u32,
    height: u32,
    bands: usize,
    step: u32,
) -> Result<Vec<f64>, String> {
    let file = if ifd.external {
        let ovr = ovr_path.ok_or_else(|| "External IFD but no .ovr path provided".to_string())?;
        File::open(ovr).map_err(|e| format!("Failed to open .ovr for region read: {}", e))?
    } else {
        File::open(path).map_err(|e| format!("Failed to open for region read: {}", e))?
    };
    let mut decoder = Decoder::new(BufReader::new(file))
        .map_err(|e| format!("Failed to create decoder for region: {}", e))?
        .with_limits(Limits::unlimited());

    if ifd.external {
        if let Some(ptr) = ifd.ifd_ptr {
            if let Ok(dir) = decoder.read_directory(tiff::tags::IfdPointer(ptr)) {
                decoder.read_directory_tags(&dir);
            }
        }
    } else if ifd.index > 0 {
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
    render_raster_tile_ex(raster, z, x, y, size, bands, None, None)
}

pub fn render_raster_tile_ex(
    raster: &CachedRaster,
    z: u32,
    x: u32,
    y: u32,
    size: u32,
    bands: &[u32],
    resampling: Option<ResamplingMode>,
    stretch: Option<&StretchConfig>,
) -> Result<(Vec<u8>, u32), String> {
    let ifd_idx = select_ifd_for_zoom(raster, z);
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

    let resampling_mode = resampling.unwrap_or(ResamplingMode::Nearest);
    let needs_full_res = resampling_mode != ResamplingMode::Nearest;

    let max_read_pixels = 1024u64 * 1024u64;
    let raw_pixels = src_w as u64 * src_h as u64;
    let step = if needs_full_res {
        1u32
    } else if raw_pixels > max_read_pixels {
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
            raster.ovr_path.as_deref(),
            &raster.ifds[ifd_idx],
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

    let stretch_bounds = crate::resample::compute_stretch_bounds(raster, stretch);

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

            if col < 0.0 || row < 0.0 || col >= raster.width as f64 || row >= raster.height as f64 {
                continue;
            }

            let local_col = col - col_off_f;
            let local_row = row - row_off_f;

            if local_col < 0.0 || local_row < 0.0 {
                continue;
            }

            let mut rgba = [0u8; 4];
            rgba[3] = 255;
            let mut pixel_is_nodata = false;

            let sample_fn = |band: usize| -> f64 {
                match resampling_mode {
                    ResamplingMode::Nearest => {
                        let dc = (local_col / step_f) as usize;
                        let dr = (local_row / step_f) as usize;
                        if dc < read_w_u && dr < read_h as usize {
                            let idx = (dr * read_w_u + dc) * raster.bands + band;
                            if idx < region_data.len() {
                                region_data[idx]
                            } else {
                                f64::NAN
                            }
                        } else {
                            f64::NAN
                        }
                    }
                    ResamplingMode::Bilinear => crate::resample::sample_bilinear(
                        &region_data, local_col, local_row,
                        read_w_u, read_h as usize, raster.bands, band,
                    ),
                    ResamplingMode::Bicubic => crate::resample::sample_bicubic(
                        &region_data, local_col, local_row,
                        read_w_u, read_h as usize, raster.bands, band,
                    ),
                    ResamplingMode::Lanczos3 => crate::resample::sample_lanczos3(
                        &region_data, local_col, local_row,
                        read_w_u, read_h as usize, raster.bands, band,
                    ),
                }
            };

            macro_rules! stretch_band {
                ($val:expr, $bi:ident) => {{
                    if crate::raster::is_nodata($val, raster.no_data) {
                        pixel_is_nodata = true;
                        0u8
                    } else {
                        let (min_v, max_v) = stretch_bounds[$bi];
                        if (max_v - min_v).abs() > f64::EPSILON {
                            ((($val - min_v) / (max_v - min_v)) * 255.0).clamp(0.0, 255.0) as u8
                        } else {
                            0
                        }
                    }
                }};
            }

            if use_grayscale {
                let bi = 0usize;
                let val = sample_fn(bi);
                if !val.is_finite() {
                    pixel_is_nodata = true;
                } else {
                    let gray = stretch_band!(val, bi);
                    if !pixel_is_nodata {
                        rgba[0] = gray;
                        rgba[1] = gray;
                        rgba[2] = gray;
                    }
                }
            } else {
                for (out_idx, &bi) in band_indices.iter().enumerate().take(3) {
                    if bi >= raster.bands {
                        pixel_is_nodata = true;
                        break;
                    }
                    let val = sample_fn(bi);
                    if !val.is_finite() {
                        pixel_is_nodata = true;
                        break;
                    }
                    rgba[out_idx] = stretch_band!(val, bi);
                    if pixel_is_nodata {
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

    let mut png_bytes = Vec::new();
    {
        let encoder = image::codecs::png::PngEncoder::new(&mut png_bytes);
        encoder
            .write_image(&img, size, size, image::ExtendedColorType::Rgba8)
            .map_err(|e| format!("PNG encode error: {}", e))?;
    }

    Ok((png_bytes, rendered))
}

pub fn render_raster_tile_webp(
    raster: &CachedRaster,
    z: u32,
    x: u32,
    y: u32,
    size: u32,
    bands: &[u32],
) -> Result<(Vec<u8>, u32), String> {
    let (rgba_data, rendered) = render_raster_tile(raster, z, x, y, size, bands)?;

    let img = image::load_from_memory(&rgba_data)
        .map_err(|e| format!("Failed to reload PNG: {}", e))?;
    let mut webp_bytes = Vec::new();
    let encoder = image::codecs::webp::WebPEncoder::new_lossless(&mut webp_bytes);
    encoder
        .encode(img.as_rgba8().unwrap(), size, size, image::ExtendedColorType::Rgba8)
        .map_err(|e| format!("WebP encode error: {}", e))?;

    Ok((webp_bytes, rendered))
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

    // Estimate zoom level from bbox extent for overview selection
    let estimated_z = if range_x > 0.0 {
        let z_f = (width as f64 * (2.0 * C) / (range_x * 256.0)).log2();
        if z_f.is_finite() && z_f > 0.0 {
            (z_f.round() as u32).min(22)
        } else {
            0
        }
    } else {
        0
    };
    let ifd_idx = select_ifd_for_zoom(raster, estimated_z);

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
            raster.ovr_path.as_deref(),
            &raster.ifds[ifd_idx],
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

    let stretch_bounds = crate::resample::compute_stretch_bounds(raster, None);

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
                                let (min_v, max_v) = stretch_bounds[bi];
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
                                let (min_v, max_v) = stretch_bounds[bi];
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

// ─── WKT 瓦片 ───

pub fn get_wkt_tile_geojson(req: &VectorTileRequest) -> Result<String, String> {
    let tile_rect = wgs84_tile_rect(req.z, req.x, req.y);
    let content =
        std::fs::read_to_string(&req.path).map_err(|e| format!("Failed to read WKT file: {}", e))?;
    let wkt: wkt::Wkt<f64> = content.parse().map_err(|e| format!("Invalid WKT: {}", e))?;
    let geometry = geo_types::Geometry::<f64>::try_from(wkt)
        .map_err(|e| format!("Failed to convert WKT geometry: {e:?}"))?;

    let features = if geometry.intersects(&tile_rect) {
        let geojson_geom = geojson::Geometry::try_from(&geometry)
            .map_err(|e| format!("Failed to convert to GeoJSON: {e}"))?;
        vec![geojson::Feature {
            bbox: None,
            geometry: Some(geojson_geom),
            id: None,
            properties: None,
            foreign_members: None,
        }]
    } else {
        vec![]
    };

    let fc = geojson::FeatureCollection {
        bbox: None,
        features,
        foreign_members: None,
    };
    serde_json::to_string(&fc).map_err(|e| format!("Serialization error: {}", e))
}

// ─── KML/KMZ 瓦片 ───

pub fn get_kml_tile_geojson(req: &VectorTileRequest) -> Result<String, String> {
    let tile_rect = wgs84_tile_rect(req.z, req.x, req.y);
    let ext = std::path::Path::new(&req.path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default();

    use kml::Kml;
    let kml_doc: Kml<f64> = if ext == "kmz" {
        let mut reader = kml::KmlReader::from_kmz_path(&req.path)
            .map_err(|e| format!("Failed to open KMZ: {}", e))?;
        reader.read().map_err(|e| format!("Failed to parse KMZ: {}", e))?
    } else {
        let content = std::fs::read_to_string(&req.path)
            .map_err(|e| format!("Failed to read KML: {}", e))?;
        content.parse::<Kml<f64>>()
            .map_err(|e| format!("Invalid KML: {}", e))?
    };

    let mut features = Vec::new();
    collect_kml_placemarks(&kml_doc, &tile_rect, &mut features);

    let fc = geojson::FeatureCollection {
        bbox: None,
        features,
        foreign_members: None,
    };
    serde_json::to_string(&fc).map_err(|e| format!("Serialization error: {}", e))
}

fn collect_kml_placemarks(
    kml: &kml::Kml<f64>,
    tile_rect: &geo_types::Rect<f64>,
    features: &mut Vec<geojson::Feature>,
) {
    use kml::Kml;
    match kml {
        Kml::KmlDocument(doc) => {
            for element in &doc.elements {
                collect_kml_placemarks(element, tile_rect, features);
            }
        }
        Kml::Document { elements, .. } => {
            for element in elements {
                collect_kml_placemarks(element, tile_rect, features);
            }
        }
        Kml::Folder { elements, .. } => {
            for element in elements {
                collect_kml_placemarks(element, tile_rect, features);
            }
        }
        Kml::Placemark(placemark) => {
            if let Some(ref geometry) = placemark.geometry {
                if let Ok(geo_geom) = geo_types::Geometry::<f64>::try_from(geometry.clone()) {
                    if geo_geom.intersects(tile_rect) {
                        let geom = geojson::Geometry::try_from(&geo_geom).unwrap();
                        let mut props = serde_json::Map::new();
                        if let Some(ref name) = placemark.name {
                            props.insert("name".to_string(), serde_json::Value::String(name.clone()));
                        }
                        if let Some(ref desc) = placemark.description {
                            props.insert("description".to_string(), serde_json::Value::String(desc.clone()));
                        }
                        features.push(geojson::Feature {
                            bbox: None,
                            geometry: Some(geom),
                            id: None,
                            properties: Some(props),
                            foreign_members: None,
                        });
                    }
                }
            }
        }
        _ => {}
    }
}

// ─── 元数据 ───

pub fn get_raster_tile_info(path: &str) -> Result<TileInfo, String> {
    let raster = get_raster(path)?;
    let gt = raster.geo_transform;

    let is_geographic = raster.crs_type == "Geographic" || raster.crs_type == "Unknown";

    let extent_wgs84 = if is_geographic {
        let min_lng = gt[0];
        let max_lng = gt[0] + raster.width as f64 * gt[1];
        let min_lat = gt[3] + raster.height as f64 * gt[5];
        let max_lat = gt[3];

        [min_lng, min_lat, max_lng, max_lat]
    } else {
        if let Some(extent_wgs84) =
            crate::reproject::extent_to_wgs84(&gt, raster.width, raster.height, &raster.geo_key)
        {
            extent_wgs84
        } else {
            [0.0, 0.0, 0.0, 0.0]
        }
    };

    let max_zoom = raster.max_zoom;

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
