use std::fs::File;
use std::io::BufReader;

use serde::{Deserialize, Serialize};
use tiff::decoder::{ChunkType, Decoder, Limits};
use utoipa::ToSchema;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InterleaveType {
    Chunky,
    Planar,
}

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
    decoder: Option<Decoder<BufReader<File>>>,
}

#[allow(dead_code)]
impl RasterBlockIterator {
    pub fn new(ifd: IfdInfo, config: RasterBlockConfig, bands: usize) -> Self {
        let blocks_x = (ifd.width + config.block_width - 1) / config.block_width;
        let blocks_y = (ifd.height + config.block_height - 1) / config.block_height;
        Self {
            ifd,
            config,
            blocks_x,
            blocks_y,
            current_block: 0,
            bands,
            decoder: None,
        }
    }

    pub fn next_block(&mut self) -> Option<Result<RasterBlock, String>> {
        if self.current_block >= self.blocks_x * self.blocks_y {
            return None;
        }

        if self.decoder.is_none() {
            let file = match File::open(&self.ifd.file_path) {
                Ok(f) => f,
                Err(e) => return Some(Err(format!("Failed to open {}: {e}", self.ifd.file_path))),
            };
            let mut decoder = match Decoder::new(BufReader::new(file))
                .map_err(|e| format!("Failed to create decoder: {e}"))
                .and_then(|d| Ok(d.with_limits(Limits::unlimited())))
            {
                Ok(d) => d,
                Err(e) => return Some(Err(e)),
            };

            if self.ifd.external {
                if let Some(ptr) = self.ifd.ifd_ptr {
                    if let Ok(dir) = decoder.read_directory(tiff::tags::IfdPointer(ptr)) {
                        decoder.read_directory_tags(&dir);
                    }
                }
            } else if self.ifd.index > 0 {
                if let Ok(sub_val) = decoder.get_tag(tiff::tags::Tag::SubIfd) {
                    if let Ok(ifd_ptrs) = sub_val.into_ifd_vec() {
                        let sub_idx = self.ifd.index - 1;
                        if sub_idx < ifd_ptrs.len() {
                            let ptr = ifd_ptrs[sub_idx];
                            if let Ok(dir) = decoder.read_directory(ptr) {
                                decoder.read_directory_tags(&dir);
                            }
                        }
                    }
                }
            }

            self.decoder = Some(decoder);
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

        let data = match super::raster_load::read_raster_region_from_decoder(
            self.decoder.as_mut().unwrap(),
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
pub struct IfdInfo {
    pub index: usize,
    pub width: u32,
    pub height: u32,
    pub chunk_type: ChunkType,
    pub chunk_width: u32,
    pub chunk_length: u32,
    pub chunks_per_row: u32,
    pub external: bool,
    pub ifd_ptr: Option<u64>,
    pub interleave: InterleaveType,
    pub file_path: String,
}

#[allow(dead_code)]
pub struct CachedRaster {
    pub file_path: String,
    pub width: u32,
    pub height: u32,
    pub bands: usize,
    pub geo_transform: [f64; 6],
    pub no_data: Option<f64>,
    pub min_values: Vec<f64>,
    pub max_values: Vec<f64>,
    pub mean_values: Vec<f64>,
    pub std_dev_values: Vec<f64>,
    pub crs_type: String,
    pub data_type: String,
    pub geo_key: crate::reproject::GeoKeyInfo,
    pub wgs84_corners: [(f64, f64); 4],
    pub native_corners: [(f64, f64); 4],
    pub ifds: Vec<IfdInfo>,
    pub percentile_bounds: std::sync::Mutex<Option<Vec<(f64, f64)>>>,
    pub ovr_path: Option<String>,
    pub max_zoom: u32,
}
