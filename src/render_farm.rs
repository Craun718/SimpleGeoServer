use std::collections::BinaryHeap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use once_cell::sync::Lazy;
use tokio::sync::oneshot;

use crate::gpu_renderer::GPU_RENDERER;
use crate::resample::{compute_stretch_bounds, ResamplingMode, StretchConfig};
use crate::tile;

#[derive(Debug, Clone)]
struct TileJob {
    path: String,
    z: u32,
    x: u32,
    y: u32,
    bands: Vec<u32>,
    stretch: Option<StretchConfig>,
    resampling: ResamplingMode,
    priority: u64,
    _submit_time: Instant,
}

impl PartialEq for TileJob {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority
    }
}

impl Eq for TileJob {}

impl PartialOrd for TileJob {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TileJob {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other.priority.cmp(&self.priority)
    }
}

struct PendingResponse {
    sender: oneshot::Sender<Result<Vec<u8>, String>>,
    _submit_time: Instant,
}

pub(crate) struct RenderFarm {
    queue: Mutex<BinaryHeap<TileJob>>,
    pending: Mutex<std::collections::HashMap<(u32, u32, u32), PendingResponse>>,
    _next_job_id: AtomicU64,
}

impl RenderFarm {
    fn new() -> Self {
        Self {
            queue: Mutex::new(BinaryHeap::new()),
            pending: Mutex::new(std::collections::HashMap::new()),
            _next_job_id: AtomicU64::new(1),
        }
    }

    pub async fn render_tile(
        self: &Arc<Self>,
        path: String,
        z: u32,
        x: u32,
        y: u32,
        bands: Vec<u32>,
        stretch: Option<StretchConfig>,
        resampling: ResamplingMode,
    ) -> Result<Vec<u8>, String> {
        let (sender, receiver) = oneshot::channel();
        let submit_time = Instant::now();

        let priority = (z as u64) << 40 | (x.abs_diff(0) as u64) << 20 | (y.abs_diff(0) as u64);

        let job = TileJob {
            path,
            z,
            x,
            y,
            bands,
            stretch,
            resampling,
            priority,
            _submit_time: submit_time,
        };

        {
            let mut queue = self.queue.lock().unwrap();
            queue.push(job);
        }

        {
            let mut pending = self.pending.lock().unwrap();
            pending.insert(
                (z, x, y),
                PendingResponse {
                    sender,
                    _submit_time: submit_time,
                },
            );
        }

        receiver
            .await
            .map_err(|_| "Render farm: response channel closed".to_string())?
    }

    fn process_job(&self, job: TileJob) -> Option<(TileJob, Result<Vec<u8>, String>)> {
        let result = render_tile_sync(
            &job.path,
            job.z,
            job.x,
            job.y,
            &job.bands,
            job.stretch.as_ref(),
            job.resampling,
        );
        Some((job, result))
    }
}

pub(crate) static RENDER_FARM: Lazy<Arc<RenderFarm>> = Lazy::new(|| {
    let farm = Arc::new(RenderFarm::new());

    let num_workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .max(2);
    for _ in 0..num_workers {
        let f = farm.clone();
        std::thread::spawn(move || loop {
            let job = {
                let mut queue = f.queue.lock().unwrap();
                queue.pop()
            };
            if let Some(job) = job {
                let result = f.process_job(job);
                if let Some((job, result)) = result {
                    let mut pending = f.pending.lock().unwrap();
                    let key = (job.z, job.x, job.y);
                    if let Some(response) = pending.remove(&key) {
                        let _ = response.sender.send(result);
                    }
                }
            } else {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        });
    }

    farm
});

fn render_tile_sync(
    path: &str,
    z: u32,
    x: u32,
    y: u32,
    bands: &[u32],
    stretch: Option<&StretchConfig>,
    resampling: ResamplingMode,
) -> Result<Vec<u8>, String> {
    let raster = tile::get_raster(path)?;
    let size = 256u32;

    let (min_x, min_y, max_x, max_y) = tile::tile_bounds_epsg3857(z, x, y, size);
    let range_x = max_x - min_x;
    let is_geographic = raster.crs_type == "Geographic" || raster.crs_type == "Unknown";
    let base_m_per_px = if is_geographic {
        raster.geo_transform[1].abs() * 111320.0
    } else {
        raster.geo_transform[1].abs()
    };
    let tile_res_meters = range_x / size as f64;
    let ifd_idx = select_best_ifd(&raster.ifds, raster.width, base_m_per_px, tile_res_meters);
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
        let lng = tile::mercator_to_lng(wx);
        let lat = tile::mercator_to_lat(wy);
        let (col_i, row_i) = if use_native {
            if let Some((nx, ny)) = crate::reproject::wgs84_to_native_crs(lng, lat, &raster.geo_key) {
                if nc_span_x.abs() <= f64::EPSILON || nc_span_y.abs() <= f64::EPSILON {
                    continue;
                }
                let u = (nx - nc_sw.0) / nc_span_x;
                let v = (ny - nc_sw.1) / nc_span_y;
                let col = u * (ov_width as f64 - 1.0);
                let row = (1.0 - v) * (ov_height as f64 - 1.0);
                (col.round() as i64, row.round() as i64)
            } else {
                continue;
            }
        } else {
            if lng_span.abs() <= f64::EPSILON || lat_span.abs() <= f64::EPSILON {
                continue;
            }
            let u = (lng - sw.0) / lng_span;
            let v = (lat - sw.1) / lat_span;
            let col = u * (ov_width as f64 - 1.0);
            let row = (1.0 - v) * (ov_height as f64 - 1.0);
            (col.round() as i64, row.round() as i64)
        };
        pixel_coords.push((col_i, row_i));
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
    let read_w = (src_w + step - 1) / step;
    let read_h = (src_h + step - 1) / step;

    let (u0, u1, v0, v1) = if use_native {
        let nw_pt = crate::reproject::wgs84_to_native_crs(
            tile::mercator_to_lng(min_x),
            tile::mercator_to_lat(max_y),
            &raster.geo_key,
        )
        .unwrap_or((nc_sw.0, nc_nw.1));
        let ne_pt = crate::reproject::wgs84_to_native_crs(
            tile::mercator_to_lng(max_x),
            tile::mercator_to_lat(max_y),
            &raster.geo_key,
        )
        .unwrap_or((nc_se.0, nc_nw.1));
        let sw_pt = crate::reproject::wgs84_to_native_crs(
            tile::mercator_to_lng(min_x),
            tile::mercator_to_lat(min_y),
            &raster.geo_key,
        )
        .unwrap_or((nc_sw.0, nc_sw.1));
        let u0 = (nw_pt.0 - nc_sw.0) / nc_span_x;
        let u1 = (ne_pt.0 - nc_sw.0) / nc_span_x;
        let v0 = (nw_pt.1 - nc_sw.1) / nc_span_y;
        let v1 = (sw_pt.1 - nc_sw.1) / nc_span_y;
        (u0, u1, v0, v1)
    } else {
        let u0 = (tile::mercator_to_lng(min_x) - sw.0) / lng_span;
        let u1 = (tile::mercator_to_lng(max_x) - sw.0) / lng_span;
        let v0 = (tile::mercator_to_lat(max_y) - sw.1) / lat_span;
        let v1 = (tile::mercator_to_lat(min_y) - sw.1) / lat_span;
        (u0, u1, v0, v1)
    };

    let ov_width_m1 = (active_ifd.width as f64 - 1.0).max(1.0);
    let ov_height_m1 = (active_ifd.height as f64 - 1.0).max(1.0);
    let buf_u0 = col_off as f64 / ov_width_m1;
    let buf_u1 = (col_off + src_w.saturating_sub(1)) as f64 / ov_width_m1;
    let buf_v0 = row_off as f64 / ov_height_m1;
    let buf_v1 = (row_off + src_h.saturating_sub(1)) as f64 / ov_height_m1;

    let uvs = [
        u0 as f32, u1 as f32, v0 as f32, v1 as f32,
        buf_u0 as f32, buf_u1 as f32, buf_v0 as f32, buf_v1 as f32,
    ];

    #[cfg(feature = "gpu")]
    if let Some(gpu) = GPU_RENDERER.as_ref() {
        if read_w <= 1024 && read_h <= 1024 {
            let region_data = if src_w >= 1 && src_h >= 1 {
                tile::read_raster_region(
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

            if !region_data.is_empty() {
                let src_f32: Vec<f32> = region_data.iter().map(|&v| v as f32).collect();
                match gpu.render_tile(
                    &raster, z, x, y, size, bands, stretch, resampling, &src_f32, read_w, read_h,
                    uvs,
                ) {
                    Ok(result) if result.rendered_pixels > 0 => {
                        return encode_webp(&result.rgba, size as usize);
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!("GPU tile render failed, falling back to CPU: {e}"),
                }
            }
        }
    }

    let (rgba, _rendered) = render_raster_tile_cpu_rgba(
        &raster, z, x, y, size, bands, stretch, resampling,
        col_off, row_off, src_w, src_h, step, ifd_idx,
    )?;

    encode_webp(&rgba, size as usize)
}

fn render_raster_tile_cpu_rgba(
    raster: &tile::CachedRaster,
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

    let (min_x, min_y, max_x, max_y) = tile::tile_bounds_epsg3857(z, x, y, size);
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

    let read_w_u = ((src_w + step - 1) / step) as usize;
    let read_h_u = ((src_h + step - 1) / step) as usize;

    let region_data = if src_w >= 1 && src_h >= 1 {
        tile::read_raster_region(
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
    let stretch_bounds = compute_stretch_bounds(raster, stretch);
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

                let lng = tile::mercator_to_lng(world_x);
                let lat = tile::mercator_to_lat(world_y);

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
                    ResamplingMode::Nearest => {
                        let ds_col = buf_col.round() as i64;
                        let ds_row = buf_row.round() as i64;
                        if ds_col >= 0 && ds_col < read_w_u as i64 && ds_row >= 0 && ds_row < read_h_u as i64 {
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
                        } else {
                            pixel_is_nodata = true;
                        }
                    }
                    _ => {
                        if buf_col >= 0.0 && buf_col < read_w_u as f64 && buf_row >= 0.0 && buf_row < read_h_u as f64 {
                            let sample_fn: fn(&[f64], f64, f64, usize, usize, usize, usize) -> f64 =
                                match resampling {
                                    ResamplingMode::Bilinear => crate::resample::sample_bilinear,
                                    ResamplingMode::Bicubic => crate::resample::sample_bicubic,
                                    ResamplingMode::Lanczos3 => crate::resample::sample_lanczos3,
                                    _ => unreachable!(),
                                };
                            if use_grayscale {
                                let bi = 0usize;
                                let val = sample_fn(&region_data, buf_col, buf_row, read_w_u, read_h_u, raster.bands, bi);
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
                                    let val = sample_fn(&region_data, buf_col, buf_row, read_w_u, read_h_u, raster.bands, bi);
                                    if val.is_nan() || crate::raster::is_nodata(val, raster.no_data) {
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

fn encode_webp(rgba: &[u8], size: usize) -> Result<Vec<u8>, String> {
    let encoder = webp::Encoder::from_rgba(rgba, size as u32, size as u32);
    let webp = encoder.encode(90.0);
    Ok(webp.to_vec())
}

fn select_best_ifd(
    ifds: &[tile::IfdInfo],
    base_width: u32,
    base_m_per_px: f64,
    tile_res_meters: f64,
) -> usize {
    if ifds.is_empty() || base_m_per_px <= 0.0 || tile_res_meters <= 0.0 {
        return 0;
    }
    let mut best_idx = 0usize;
    let mut best_diff = f64::INFINITY;
    for (i, ifd) in ifds.iter().enumerate() {
        let scale = base_width as f64 / ifd.width as f64;
        let ifd_res = base_m_per_px * scale;
        let diff = (ifd_res - tile_res_meters).abs();
        if diff < best_diff {
            best_diff = diff;
            best_idx = i;
        }
    }
    best_idx
}

fn approximate_tile_affine(
    raster: &tile::CachedRaster,
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
        let lng = tile::mercator_to_lng(wx);
        let lat = tile::mercator_to_lat(wy);
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
