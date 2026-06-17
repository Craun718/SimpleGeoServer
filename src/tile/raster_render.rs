use image::{ImageEncoder, RgbaImage};

use super::raster_load::{read_raster_region, select_ifd_for_zoom};
use super::tile_math::{mercator_to_lat, mercator_to_lng, tile_bounds_epsg3857};
use super::types::CachedRaster;
use crate::resample::{ResamplingMode, StretchConfig};

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

    let resampling_mode = resampling.unwrap_or(ResamplingMode::NearestNeighbor);
    let needs_full_res = resampling_mode != ResamplingMode::NearestNeighbor;

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
                    ResamplingMode::NearestNeighbor => {
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

    let estimated_z = if range_x > 0.0 {
        let z_f = (width as f64 * (2.0 * super::tile_math::C) / (range_x * 256.0)).log2();
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
