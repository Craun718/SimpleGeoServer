use serde::{Deserialize, Serialize};

use crate::tile::CachedRaster;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[repr(u8)]
pub enum ResamplingMode {
    NearestNeighbor,
    Bilinear,
    Bicubic,
    Lanczos3,
}

impl ResamplingMode {
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "nearest" | "nearest-neighbor" => ResamplingMode::NearestNeighbor,
            "bilinear" => ResamplingMode::Bilinear,
            "bicubic" => ResamplingMode::Bicubic,
            "lanczos" | "lanczos3" => ResamplingMode::Lanczos3,
            _ => ResamplingMode::NearestNeighbor,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StretchConfig {
    pub method: StretchMethod,
    #[serde(default)]
    pub min_percent: Option<f64>,
    #[serde(default)]
    pub max_percent: Option<f64>,
    #[serde(default)]
    pub std_dev_factor: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StretchMethod {
    MinMax,
    Percentile,
    #[serde(rename = "standard-deviation")]
    StdDev,
}

impl StretchMethod {
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "percentile" => StretchMethod::Percentile,
            "stddev" | "standard-deviation" => StretchMethod::StdDev,
            _ => StretchMethod::MinMax,
        }
    }
}

pub fn compute_stretch_bounds(
    raster: &CachedRaster,
    stretch: Option<&StretchConfig>,
) -> Vec<(f64, f64)> {
    let bands = raster.bands;

    match stretch.map(|s| s.method) {
        Some(StretchMethod::Percentile) => {
            let cached = raster.percentile_bounds.lock().unwrap().clone();
            cached.unwrap_or_else(|| {
                (0..bands)
                    .map(|b| (raster.min_values[b], raster.max_values[b]))
                    .collect()
            })
        }
        Some(StretchMethod::StdDev) => {
            let factor = stretch.and_then(|s| s.std_dev_factor).unwrap_or(2.0).abs();
            (0..bands)
                .map(|b| {
                    let mean = raster.mean_values[b];
                    let std = raster.std_dev_values[b];
                    (mean - factor * std, mean + factor * std)
                })
                .collect()
        }
        _ => (0..bands)
            .map(|b| (raster.min_values[b], raster.max_values[b]))
            .collect(),
    }
}

fn cubic_hermite(p0: f64, p1: f64, p2: f64, p3: f64, t: f64) -> f64 {
    0.5 * ((2.0 * p1)
        + (-p0 + p2) * t
        + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * t * t
        + (-p0 + 3.0 * p1 - 3.0 * p2 + p3) * t * t * t)
}

pub fn sample_bilinear(
    data: &[f64],
    col: f64,
    row: f64,
    w: usize,
    h: usize,
    bands: usize,
    band: usize,
) -> f64 {
    if col < 0.0 || row < 0.0 || col >= w as f64 || row >= h as f64 {
        return f64::NAN;
    }
    let x0 = col.floor().clamp(0.0, (w - 1) as f64) as usize;
    let y0 = row.floor().clamp(0.0, (h - 1) as f64) as usize;
    let x1 = (x0 + 1).min(w - 1);
    let y1 = (y0 + 1).min(h - 1);
    let fx = col - x0 as f64;
    let fy = row - y0 as f64;

    let v00 = data[(y0 * w + x0) * bands + band];
    let v10 = data[(y0 * w + x1) * bands + band];
    let v01 = data[(y1 * w + x0) * bands + band];
    let v11 = data[(y1 * w + x1) * bands + band];

    let v0 = v00 * (1.0 - fx) + v10 * fx;
    let v1 = v01 * (1.0 - fx) + v11 * fx;
    v0 * (1.0 - fy) + v1 * fy
}

pub fn sample_bicubic(
    data: &[f64],
    col: f64,
    row: f64,
    w: usize,
    h: usize,
    bands: usize,
    band: usize,
) -> f64 {
    if col < 0.0 || row < 0.0 || col >= w as f64 || row >= h as f64 {
        return f64::NAN;
    }
    let ix = col.floor() as i64;
    let iy = row.floor() as i64;
    let fx = col - ix as f64;
    let fy = row - iy as f64;

    let mut cols = [0f64; 4];
    #[allow(clippy::needless_range_loop)]
    for cy in 0..4 {
        let py = (iy + cy as i64 - 1).clamp(0, h as i64 - 1) as usize;
        let p0 = data[(py * w + (ix - 1).clamp(0, w as i64 - 1) as usize) * bands + band];
        let p1 = data[(py * w + ix.clamp(0, w as i64 - 1) as usize) * bands + band];
        let p2 = data[(py * w + (ix + 1).clamp(0, w as i64 - 1) as usize) * bands + band];
        let p3 = data[(py * w + (ix + 2).clamp(0, w as i64 - 1) as usize) * bands + band];
        cols[cy] = cubic_hermite(p0, p1, p2, p3, fx);
    }
    cubic_hermite(cols[0], cols[1], cols[2], cols[3], fy)
}

fn lanczos_kernel(x: f64, a: f64) -> f64 {
    if x == 0.0 {
        return 1.0;
    }
    if x.abs() >= a {
        return 0.0;
    }
    let px = std::f64::consts::PI * x;
    (px.sin() / px) * ((std::f64::consts::PI * x / a).sin() / (std::f64::consts::PI * x / a))
}

pub fn sample_lanczos3(
    data: &[f64],
    col: f64,
    row: f64,
    width: usize,
    height: usize,
    bands: usize,
    band: usize,
) -> f64 {
    if col < 0.0 || row < 0.0 || col >= width as f64 || row >= height as f64 {
        return f64::NAN;
    }
    let cx = col.floor() as i64;
    let cy = row.floor() as i64;
    let fx = col - cx as f64;
    let fy = row - cy as f64;
    let a = 3.0;

    let mut sum = 0.0f64;
    let mut weight_sum = 0.0f64;
    for dy in -2..=3 {
        let py = cy + dy;
        if py < 0 || py >= height as i64 {
            continue;
        }
        let wy = lanczos_kernel(dy as f64 - fy, a);
        for dx in -2..=3 {
            let px = cx + dx;
            if px < 0 || px >= width as i64 {
                continue;
            }
            let wx = lanczos_kernel(dx as f64 - fx, a);
            let weight = wx * wy;
            weight_sum += weight;
            sum += weight * data[(py as usize * width + px as usize) * bands + band];
        }
    }
    if weight_sum.abs() > f64::EPSILON {
        sum / weight_sum
    } else {
        f64::NAN
    }
}
