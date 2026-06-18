use std::fs::File;
use std::io::BufReader;

use tiff::decoder::{ChunkType, Decoder, Limits};

pub fn select_ifd_for_zoom(raster: &super::types::CachedRaster, z: u32) -> usize {
    if raster.ifds.len() <= 1 {
        return 0;
    }
    let size = 256u32;
    let (min_x, _min_y, max_x, _max_y) = super::tile_math::tile_bounds_epsg3857(z, 0, 0, size);
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

pub fn read_raster_region(
    path: &str,
    ovr_path: Option<&str>,
    ifd: &super::types::IfdInfo,
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

    read_raster_region_from_decoder(
        &mut decoder,
        ifd,
        col_off,
        row_off,
        width,
        height,
        bands,
        step,
    )
}

pub fn read_raster_region_from_decoder(
    decoder: &mut Decoder<BufReader<File>>,
    ifd: &super::types::IfdInfo,
    col_off: u32,
    row_off: u32,
    width: u32,
    height: u32,
    bands: usize,
    step: u32,
) -> Result<Vec<f64>, String> {
    let out_h = (height + step - 1) / step;
    let out_w = (width + step - 1) / step;
    let total_pixels = out_w as usize * out_h as usize;
    let mut result = vec![0.0f64; total_pixels * bands];

    let image_w = ifd.width;
    let col_end = col_off + width;
    let row_end = row_off + height;

    match ifd.chunk_type {
        ChunkType::Strip => {
            let rows_per_strip = ifd.chunk_length;
            let strip_start = row_off / rows_per_strip;
            let strip_end = (row_end - 1) / rows_per_strip;

            for si in strip_start..=strip_end {
                let chunk_data = decoder
                    .read_chunk(si)
                    .map_err(|e| format!("Failed to read strip {si}: {e}"))?;
                let data_f64 = crate::raster::decode_result_to_f64_vec(&chunk_data);
                let strip_y = si * rows_per_strip;
                let strip_h = rows_per_strip.min(ifd.height - strip_y);
                let overlap_top = row_off.max(strip_y);
                let overlap_bot = row_end.min(strip_y + strip_h);

                let mut out_row = overlap_top;
                while out_row < overlap_bot {
                    let local_y = out_row - strip_y;
                    let dst_row = (out_row - row_off) / step;
                    let mut out_col = col_off;
                    while out_col < col_end {
                        let src_idx =
                            (local_y as usize * image_w as usize + out_col as usize) * bands;
                        let dst_col = (out_col - col_off) / step;
                        let dst_idx =
                            (dst_row as usize * out_w as usize + dst_col as usize) * bands;
                        let end = (src_idx + bands).min(data_f64.len());
                        if end > src_idx {
                            let src_slice = &data_f64[src_idx..end];
                            let dst_slice = &mut result[dst_idx..dst_idx + src_slice.len()];
                            dst_slice.copy_from_slice(src_slice);
                        }
                        out_col += step;
                    }
                    out_row += step;
                }
            }
        }
        ChunkType::Tile => {
            let tile_w = ifd.chunk_width;
            let tile_h = ifd.chunk_length;
            let tiles_per_row = ifd.chunks_per_row;
            let tx_start = col_off / tile_w;
            let tx_end = (col_end - 1) / tile_w;
            let ty_start = row_off / tile_h;
            let ty_end = (row_end - 1) / tile_h;

            for ty in ty_start..=ty_end {
                for tx in tx_start..=tx_end {
                    let tile_idx = ty * tiles_per_row + tx;
                    let chunk_data = decoder
                        .read_chunk(tile_idx)
                        .map_err(|e| format!("Failed to read tile {tile_idx}: {e}"))?;
                    let (cdw, _cdh) = decoder.chunk_data_dimensions(tile_idx);
                    let data_f64 = crate::raster::decode_result_to_f64_vec(&chunk_data);
                    let tile_x = tx * tile_w;
                    let tile_y = ty * tile_h;

                    let overlap_l = col_off.max(tile_x);
                    let overlap_r = col_end.min(tile_x + tile_w);
                    let overlap_t = row_off.max(tile_y);
                    let overlap_b = row_end.min(tile_y + tile_h);

                    let mut out_row = overlap_t;
                    while out_row < overlap_b {
                        let local_tile_row = out_row - tile_y;
                        let dst_row = (out_row - row_off) / step;
                        let mut out_col = overlap_l;
                        while out_col < overlap_r {
                            let local_tile_col = out_col - tile_x;
                            let src_idx = (local_tile_row as usize * cdw as usize
                                + local_tile_col as usize)
                                * bands;
                            let dst_col = (out_col - col_off) / step;
                            let dst_idx =
                                (dst_row as usize * out_w as usize + dst_col as usize) * bands;
                            let end = (src_idx + bands).min(data_f64.len());
                            if end > src_idx {
                                let src_slice = &data_f64[src_idx..end];
                                let dst_slice = &mut result[dst_idx..dst_idx + src_slice.len()];
                                dst_slice.copy_from_slice(src_slice);
                            }
                            out_col += step;
                        }
                        out_row += step;
                    }
                }
            }
        }
    }

    Ok(result)
}
