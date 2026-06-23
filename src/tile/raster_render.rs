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
    render_raster_tile_ex(raster, z, x, y, size, bands, None, None, false)
}

#[allow(clippy::too_many_arguments)]
pub fn render_raster_tile_ex(
    raster: &CachedRaster,
    z: u32,
    x: u32,
    y: u32,
    size: u32,
    bands: &[u32],
    resampling: Option<ResamplingMode>,
    stretch: Option<&StretchConfig>,
    return_rgba: bool,
) -> Result<(Vec<u8>, u32), String> {
    let ifd_idx = select_ifd_for_zoom(raster, z);
    let active_ifd = &raster.ifds[ifd_idx];
    let ov_width = active_ifd.width;
    let ov_height = active_ifd.height;
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
                    let col = u * (ov_width as f64 - 1.0);
                    let row = (1.0 - v) * (ov_height as f64 - 1.0);
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
                let col = u * (ov_width as f64 - 1.0);
                let row = (1.0 - v) * (ov_height as f64 - 1.0);
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

    let col_off = col_off.min(ov_width - 1);
    let row_off = row_off.min(ov_height - 1);
    let src_w = src_w.min(ov_width - col_off);
    let src_h = src_h.min(ov_height - row_off);

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
    let read_w = src_w.div_ceil(step);
    let read_h = src_h.div_ceil(step);

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

    let affine_approx = approximate_tile_affine(raster, min_x, min_y, max_x, max_y, size);
    let use_affine = affine_approx.is_some();

    let mut img = RgbaImage::new(size, size);
    let mut rendered: u32 = 0;

    for ty in 0..size {
        for tx in 0..size {
            let (u, v) = if use_affine {
                let aff = affine_approx.as_ref().unwrap().0;
                let nx = aff[0] * tx as f64 + aff[1] * ty as f64 + aff[2];
                let ny = aff[3] * tx as f64 + aff[4] * ty as f64 + aff[5];
                if nc_span_x.abs() > f64::EPSILON && nc_span_y.abs() > f64::EPSILON {
                    let u = (nx - nc_sw.0) / nc_span_x;
                    let v = (ny - nc_sw.1) / nc_span_y;
                    (u, v)
                } else {
                    continue;
                }
            } else {
                let world_x = min_x + (tx as f64 + 0.5) / size as f64 * range_x;
                let world_y = max_y - (ty as f64 + 0.5) / size as f64 * range_y;

                let lng = mercator_to_lng(world_x);
                let lat = mercator_to_lat(world_y);

                if use_native {
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
                }
            };

            let col = u * (ov_width as f64 - 1.0);
            let row = (1.0 - v) * (ov_height as f64 - 1.0);

            if col < 0.0 || row < 0.0 || col >= ov_width as f64 || row >= ov_height as f64 {
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
                        &region_data,
                        local_col,
                        local_row,
                        read_w_u,
                        read_h as usize,
                        raster.bands,
                        band,
                    ),
                    ResamplingMode::Bicubic => crate::resample::sample_bicubic(
                        &region_data,
                        local_col,
                        local_row,
                        read_w_u,
                        read_h as usize,
                        raster.bands,
                        band,
                    ),
                    ResamplingMode::Lanczos3 => crate::resample::sample_lanczos3(
                        &region_data,
                        local_col,
                        local_row,
                        read_w_u,
                        read_h as usize,
                        raster.bands,
                        band,
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

    if return_rgba {
        Ok((img.into_raw(), rendered))
    } else {
        let mut png_bytes = Vec::new();
        {
            let encoder = image::codecs::png::PngEncoder::new(&mut png_bytes);
            encoder
                .write_image(&img, size, size, image::ExtendedColorType::Rgba8)
                .map_err(|e| format!("PNG encode error: {}", e))?;
        }
        Ok((png_bytes, rendered))
    }
}

pub fn render_raster_tile_webp(
    raster: &CachedRaster,
    z: u32,
    x: u32,
    y: u32,
    size: u32,
    bands: &[u32],
) -> Result<(Vec<u8>, u32), String> {
    let (rgba, rendered) = render_raster_tile_ex(raster, z, x, y, size, bands, None, None, true)?;
    let encoder = webp::Encoder::from_rgba(&rgba, size, size);
    let webp = encoder.encode(90.0);
    Ok((webp.to_vec(), rendered))
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
    let active_ifd = &raster.ifds[ifd_idx];
    let ov_width = active_ifd.width;
    let ov_height = active_ifd.height;

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
                    let col = u * (ov_width as f64 - 1.0);
                    let row = (1.0 - v) * (ov_height as f64 - 1.0);
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
                let col = u * (ov_width as f64 - 1.0);
                let row = (1.0 - v) * (ov_height as f64 - 1.0);
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

    let col_off = col_off.min(ov_width - 1);
    let row_off = row_off.min(ov_height - 1);
    let src_w = src_w.min(ov_width - col_off);
    let src_h = src_h.min(ov_height - row_off);

    let max_read_pixels = 1024u64 * 1024u64;
    let raw_pixels = src_w as u64 * src_h as u64;
    let step = if raw_pixels > max_read_pixels {
        let ratio = (raw_pixels as f64 / max_read_pixels as f64).sqrt();
        (ratio.ceil() as u32).max(1)
    } else {
        1
    };
    let read_w = src_w.div_ceil(step);
    let read_h = src_h.div_ceil(step);

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

            let col = u * (ov_width as f64 - 1.0);
            let row = (1.0 - v) * (ov_height as f64 - 1.0);

            let col_i = col.round() as i64;
            let row_i = row.round() as i64;

            if col_i >= 0 && col_i < ov_width as i64 && row_i >= 0 && row_i < ov_height as i64 {
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

#[allow(clippy::too_many_arguments)]
pub fn render_raster_tile_cpu(
    raster: &CachedRaster,
    z: u32,
    x: u32,
    y: u32,
    size: u32,
    bands: &[u32],
    stretch: Option<&StretchConfig>,
    resampling: ResamplingMode,
    col_off: u32,
    row_off: u32,
    src_w: u32,
    src_h: u32,
    step: u32,
    ifd_idx: usize,
) -> Result<(Vec<u8>, u32), String> {
    let (rgba, rendered) = render_raster_tile_cpu_rgba(
        raster, z, x, y, size, bands, stretch, resampling, col_off, row_off, src_w, src_h, step,
        ifd_idx,
    )?;

    let mut png_bytes = Vec::new();
    {
        let encoder = image::codecs::png::PngEncoder::new(&mut png_bytes);
        encoder
            .write_image(&rgba, size, size, image::ExtendedColorType::Rgba8)
            .map_err(|e| format!("PNG encode error: {}", e))?;
    }

    Ok((png_bytes, rendered))
}

#[allow(clippy::too_many_arguments)]
pub fn render_single_tile(
    raster: &CachedRaster,
    z: u32,
    x: u32,
    y: u32,
    size: u32,
    bands: &[u32],
    stretch: Option<&StretchConfig>,
    resampling: ResamplingMode,
    format: &str,
) -> Result<(Vec<u8>, u32, String), String> {
    let (data, rendered) = match format {
        "raw-rgba" => render_raster_tile_ex(
            raster,
            z,
            x,
            y,
            size,
            bands,
            Some(resampling),
            stretch,
            true,
        )?,
        _ => render_raster_tile_ex(
            raster,
            z,
            x,
            y,
            size,
            bands,
            Some(resampling),
            stretch,
            false,
        )?,
    };
    Ok((data, rendered, format.to_string()))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn render_raster_tile_cpu_rgba(
    raster: &CachedRaster,
    z: u32,
    x: u32,
    y: u32,
    size: u32,
    bands: &[u32],
    stretch: Option<&StretchConfig>,
    resampling: ResamplingMode,
    col_off: u32,
    row_off: u32,
    src_w: u32,
    src_h: u32,
    step: u32,
    ifd_idx: usize,
) -> Result<(Vec<u8>, u32), String> {
    let active_ifd = &raster.ifds[ifd_idx];
    let ov_width = active_ifd.width;
    let ov_height = active_ifd.height;

    let (min_x, min_y, max_x, max_y) = tile_bounds_epsg3857(z, x, y, size);
    let range_x = max_x - min_x;
    let range_y = max_y - min_y;

    let use_native = raster.crs_type == "Projected";
    let sw = raster.wgs84_corners[0];
    let se = raster.wgs84_corners[1];
    let nw = raster.wgs84_corners[2];
    let lng_span = se.0 - sw.0;
    let lat_span = nw.1 - sw.1;
    let nc_sw = raster.native_corners[0];
    let nc_se = raster.native_corners[1];
    let nc_nw = raster.native_corners[2];
    let nc_span_x = nc_se.0 - nc_sw.0;
    let nc_span_y = nc_nw.1 - nc_sw.1;

    let read_w_u = src_w.div_ceil(step) as usize;
    let read_h_u = src_h.div_ceil(step) as usize;

    let region_data = if src_w >= 1 && src_h >= 1 {
        read_raster_region(
            &raster.file_path,
            raster.ovr_path.as_deref(),
            active_ifd,
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
    let stretch_bounds = crate::resample::compute_stretch_bounds(raster, stretch);
    let step_f = step as f64;
    let col_off_f = col_off as f64;
    let row_off_f = row_off as f64;

    let affine_approx = approximate_tile_affine(raster, min_x, min_y, max_x, max_y, size);
    let use_affine = affine_approx.is_some();

    let mut img = image::RgbaImage::new(size, size);
    let mut rendered: u32 = 0;

    for ty in 0..size {
        for tx in 0..size {
            let (u, v) = if use_affine {
                let aff = affine_approx.as_ref().unwrap().0;
                let nx = aff[0] * tx as f64 + aff[1] * ty as f64 + aff[2];
                let ny = aff[3] * tx as f64 + aff[4] * ty as f64 + aff[5];
                if nc_span_x.abs() > f64::EPSILON && nc_span_y.abs() > f64::EPSILON {
                    let u = (nx - nc_sw.0) / nc_span_x;
                    let v = (ny - nc_sw.1) / nc_span_y;
                    (u, v)
                } else {
                    continue;
                }
            } else {
                let world_x = min_x + (tx as f64 + 0.5) / size as f64 * range_x;
                let world_y = max_y - (ty as f64 + 0.5) / size as f64 * range_y;

                let lng = mercator_to_lng(world_x);
                let lat = mercator_to_lat(world_y);

                if use_native {
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
                }
            };

            let col = u * (ov_width as f64 - 1.0);
            let row = (1.0 - v) * (ov_height as f64 - 1.0);

            if col >= 0.0 && col < ov_width as f64 && row >= 0.0 && row < ov_height as f64 {
                let buf_col = (col - col_off_f) / step_f;
                let buf_row = (row - row_off_f) / step_f;

                let mut rgba = [0u8; 4];
                rgba[3] = 255;
                let mut pixel_is_nodata = false;

                match resampling {
                    ResamplingMode::NearestNeighbor => {
                        let ds_col = buf_col.round() as i64;
                        let ds_row = buf_row.round() as i64;
                        if ds_col >= 0
                            && ds_col < read_w_u as i64
                            && ds_row >= 0
                            && ds_row < read_h_u as i64
                        {
                            let idx = (ds_row as usize * read_w_u + ds_col as usize) * raster.bands;
                            if use_grayscale {
                                let bi = 0usize;
                                if idx + bi < region_data.len() {
                                    let val = region_data[idx + bi];
                                    if crate::raster::is_nodata(val, raster.no_data) {
                                        pixel_is_nodata = true;
                                    } else {
                                        let (min_v, max_v) = stretch_bounds[bi];
                                        let stretched = if (max_v - min_v).abs() > f64::EPSILON {
                                            ((val - min_v) / (max_v - min_v) * 255.0)
                                                .clamp(0.0, 255.0)
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
                                            ((val - min_v) / (max_v - min_v) * 255.0)
                                                .clamp(0.0, 255.0)
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
                        } else {
                            pixel_is_nodata = true;
                        }
                    }
                    _ => {
                        if buf_col >= 0.0
                            && buf_col < read_w_u as f64
                            && buf_row >= 0.0
                            && buf_row < read_h_u as f64
                        {
                            let sample_fn: fn(&[f64], f64, f64, usize, usize, usize, usize) -> f64 =
                                match resampling {
                                    ResamplingMode::Bilinear => crate::resample::sample_bilinear,
                                    ResamplingMode::Bicubic => crate::resample::sample_bicubic,
                                    ResamplingMode::Lanczos3 => crate::resample::sample_lanczos3,
                                    _ => unreachable!(),
                                };
                            if use_grayscale {
                                let bi = 0usize;
                                let val = sample_fn(
                                    &region_data,
                                    buf_col,
                                    buf_row,
                                    read_w_u,
                                    read_h_u,
                                    raster.bands,
                                    bi,
                                );
                                if val.is_nan() || crate::raster::is_nodata(val, raster.no_data) {
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
                                for (out_idx, &bi) in band_indices.iter().enumerate().take(3) {
                                    let val = sample_fn(
                                        &region_data,
                                        buf_col,
                                        buf_row,
                                        read_w_u,
                                        read_h_u,
                                        raster.bands,
                                        bi,
                                    );
                                    if val.is_nan() || crate::raster::is_nodata(val, raster.no_data)
                                    {
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
                                }
                            }
                        } else {
                            pixel_is_nodata = true;
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

    Ok((img.into_raw(), rendered))
}

pub fn approximate_tile_affine(
    raster: &CachedRaster,
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
    size: u32,
) -> Option<([f64; 6], bool)> {
    if raster.crs_type != "Projected" {
        return None;
    }
    let corners = [
        (min_x, max_y),
        (max_x, max_y),
        (min_x, min_y),
        (max_x, min_y),
    ];
    let mut native_pts = Vec::with_capacity(4);
    for &(wx, wy) in &corners {
        let lng = mercator_to_lng(wx);
        let lat = mercator_to_lat(wy);
        if let Some((nx, ny)) = crate::reproject::wgs84_to_native_crs(lng, lat, &raster.geo_key) {
            native_pts.push((nx, ny));
        } else {
            return None;
        }
    }
    if native_pts.len() < 4 {
        return None;
    }
    let s = (size - 1) as f64;
    let (nw_nx, nw_ny) = native_pts[0];
    let (ne_nx, ne_ny) = native_pts[1];
    let (sw_nx, sw_ny) = native_pts[2];
    let (_se_nx, _se_ny) = native_pts[3];

    let dx_nx = (ne_nx - nw_nx) / s;
    let dx_ny = (ne_ny - nw_ny) / s;
    let dy_nx = (sw_nx - nw_nx) / s;
    let dy_ny = (sw_ny - nw_ny) / s;

    Some(([dx_nx, dy_nx, nw_nx, dx_ny, dy_ny, nw_ny], true))
}
