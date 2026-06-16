use std::sync::Arc;

use crate::resample::{ResamplingMode, StretchConfig};
use crate::tile::CachedRaster;

#[derive(Clone)]
pub struct BatchTileJob {
    pub path: String,
    pub z: u32,
    pub x: u32,
    pub y: u32,
    pub bands: Vec<u32>,
    pub resampling: ResamplingMode,
    pub stretch: Option<StretchConfig>,
    #[allow(dead_code)]
    pub priority: u32,
}

pub struct BatchTileResult {
    pub x: u32,
    pub y: u32,
    pub z: u32,
    pub data: Vec<u8>,
    pub rendered: u32,
}

pub fn render_tiles_parallel(jobs: Vec<BatchTileJob>, max_workers: usize) -> Vec<(usize, Result<BatchTileResult, String>)> {
    let n_results = jobs.len();
    let n_workers = max_workers.max(1).min(jobs.len());
    if n_results == 0 {
        return Vec::new();
    }

    let results: std::sync::Mutex<Vec<Option<Result<BatchTileResult, String>>>> =
        std::sync::Mutex::new((0..n_results).map(|_| None).collect());

    let chunk_size = (n_results + n_workers - 1) / n_workers;

    std::thread::scope(|s| {
        for chunk_idx in 0..n_workers {
            let start = chunk_idx * chunk_size;
            let end = (start + chunk_size).min(n_results);
            if start >= n_results {
                break;
            }
            let chunk_jobs: Vec<BatchTileJob> = jobs[start..end].to_vec();
            let results = &results;

            s.spawn(move || {
                for (offset, job) in chunk_jobs.iter().enumerate() {
                    let global_idx = start + offset;
                    let result = match crate::tile::get_raster(&job.path) {
                        Ok(raster) => render_single(&raster, job),
                        Err(e) => Err(e),
                    };
                    let mut res = results.lock().unwrap();
                    res[global_idx] = Some(result);
                }
            });
        }
    });

    let res = results.lock().unwrap();
    let mut final_results = Vec::with_capacity(n_results);
    for (i, r) in res.iter().enumerate() {
        match r {
            Some(Ok(tile)) => {
                let owned = BatchTileResult {
                    x: tile.x,
                    y: tile.y,
                    z: tile.z,
                    data: tile.data.clone(),
                    rendered: tile.rendered,
                };
                final_results.push((i, Ok(owned)));
            }
            Some(Err(e)) => final_results.push((i, Err(e.clone()))),
            None => final_results.push((i, Err("No result".to_string()))),
        }
    }
    final_results
}

fn render_single(raster: &Arc<CachedRaster>, job: &BatchTileJob) -> Result<BatchTileResult, String> {
    let (png_data, rendered) = crate::tile::render_raster_tile_ex(
        raster, job.z, job.x, job.y, 256, &job.bands,
        Some(job.resampling), job.stretch.as_ref(),
    )?;

    Ok(BatchTileResult {
        x: job.x,
        y: job.y,
        z: job.z,
        data: png_data,
        rendered,
    })
}
