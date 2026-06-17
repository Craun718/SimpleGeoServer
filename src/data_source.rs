use std::sync::Arc;

use axum::http::StatusCode;
use serde::Serialize;

use crate::tile;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum DataType {
    #[serde(rename = "raster")]
    Raster,
    #[serde(rename = "vector")]
    Vector,
}

impl DataType {
    pub fn as_str(&self) -> &'static str {
        match self {
            DataType::Raster => "raster",
            DataType::Vector => "vector",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DataSourceInfo {
    pub name: String,
    pub data_type: DataType,
    pub tile_info: tile::TileInfo,
}

pub type DataSourceResult<T> = Result<T, (StatusCode, String)>;

pub trait DataSource: Send + Sync {
    fn info(&self) -> DataSourceInfo;

    fn render_raster_tile(
        &self,
        z: u32,
        x: u32,
        y: u32,
        params: &crate::TileQueryParams,
    ) -> DataSourceResult<Vec<u8>>;

    fn render_raster_tile_webp(
        &self,
        z: u32,
        x: u32,
        y: u32,
        params: &crate::TileQueryParams,
    ) -> DataSourceResult<Vec<u8>>;

    fn render_vector_tile(&self, z: u32, x: u32, y: u32) -> DataSourceResult<Vec<u8>>;

    fn render_map_bbox(
        &self,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        bands: &[u32],
        transparent: bool,
    ) -> DataSourceResult<Vec<u8>>;
}

// ─── Raster Data Source ───

pub struct RasterDataSource {
    name: String,
    path: String,
}

impl RasterDataSource {
    pub fn new(name: String, path: String) -> Self {
        Self { name, path }
    }
}

impl DataSource for RasterDataSource {
    fn info(&self) -> DataSourceInfo {
        let tile_info = tile::get_raster_tile_info(&self.path).unwrap_or_else(|_| tile::TileInfo {
            data_type: "raster".to_string(),
            min_zoom: 0,
            max_zoom: 0,
            crs: String::new(),
            extent: [0.0; 4],
            native_crs: String::new(),
            native_extent: [0.0; 4],
        });
        DataSourceInfo {
            name: self.name.clone(),
            data_type: DataType::Raster,
            tile_info,
        }
    }

    fn render_raster_tile(
        &self,
        z: u32,
        x: u32,
        y: u32,
        params: &crate::TileQueryParams,
    ) -> DataSourceResult<Vec<u8>> {
        let resampling = params
            .resampling
            .as_deref()
            .map(crate::resample::ResamplingMode::from_str)
            .unwrap_or(crate::resample::ResamplingMode::NearestNeighbor);
        let bands = params.bands.clone().unwrap_or_else(|| vec![1, 2, 3]);

        let cache_key = crate::tile_cache::TileCacheKey::new(&self.path, z, x, y, resampling);
        {
            let mut l2 = crate::tile_cache::L2_CACHE.lock().unwrap();
            if let Some(cached) = l2.get(&cache_key) {
                crate::tile_cache::record_l2_hit();
                return Ok(cached.clone());
            }
        }
        if let Some(cached) =
            crate::tile_cache::disk_cache_get(&self.path, z, x, y, resampling, false)
        {
            crate::tile_cache::record_l3_hit();
            let mut l2 = crate::tile_cache::L2_CACHE.lock().unwrap();
            l2.insert(cache_key, cached.clone());
            return Ok(cached);
        }
        crate::tile_cache::record_miss();

        let raster =
            tile::get_raster(&self.path).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

        let stretch = params
            .stretch
            .as_deref()
            .map(|s| crate::resample::StretchConfig {
                method: crate::resample::StretchMethod::from_str(s),
                min_percent: None,
                max_percent: None,
                std_dev_factor: params.std_dev_factor,
            });

        let (png_data, _rendered) = tile::render_raster_tile_ex(
            &raster,
            z,
            x,
            y,
            256,
            &bands,
            Some(resampling),
            stretch.as_ref(),
            false,
        )
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

        crate::tile_cache::disk_cache_set(&self.path, z, x, y, resampling, false, &png_data);
        {
            let mut l2 = crate::tile_cache::L2_CACHE.lock().unwrap();
            l2.insert(cache_key, png_data.clone());
        }

        Ok(png_data)
    }
    fn render_raster_tile_webp(
        &self,
        z: u32,
        x: u32,
        y: u32,
        params: &crate::TileQueryParams,
    ) -> DataSourceResult<Vec<u8>> {
        let resampling = params
            .resampling
            .as_deref()
            .map(crate::resample::ResamplingMode::from_str)
            .unwrap_or(crate::resample::ResamplingMode::NearestNeighbor);
        let bands = params.bands.clone().unwrap_or_else(|| vec![1, 2, 3]);
        let _stretch = params.stretch.as_deref().map(|s| crate::resample::StretchConfig {
            method: crate::resample::StretchMethod::from_str(s),
            min_percent: params.min_percent,
            max_percent: params.max_percent,
            std_dev_factor: params.std_dev_factor,
        });

        let cache_key =
            crate::tile_cache::TileCacheKey::new_webp(&self.path, z, x, y, resampling);

        {
            let mut l2 = crate::tile_cache::L2_CACHE.lock().unwrap();
            if let Some(cached) = l2.get(&cache_key) {
                crate::tile_cache::record_l2_hit();
                return Ok(cached.clone());
            }
        }
        if let Some(cached) =
            crate::tile_cache::disk_cache_get(&self.path, z, x, y, resampling, true)
        {
            crate::tile_cache::record_l3_hit();
            let mut l2 = crate::tile_cache::L2_CACHE.lock().unwrap();
            l2.insert(cache_key, cached.clone());
            return Ok(cached);
        }
        crate::tile_cache::record_miss();

        let raster =
            tile::get_raster(&self.path).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
        let (webp_data, _rendered) = tile::render_raster_tile_webp(&raster, z, x, y, 256, &bands)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

        crate::tile_cache::disk_cache_set(&self.path, z, x, y, resampling, true, &webp_data);
        {
            let mut l2 = crate::tile_cache::L2_CACHE.lock().unwrap();
            l2.insert(cache_key, webp_data.clone());
        }

        Ok(webp_data)
    }

    fn render_vector_tile(&self, _z: u32, _x: u32, _y: u32) -> DataSourceResult<Vec<u8>> {
        Err((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Raster source does not support vector tiles".to_string(),
        ))
    }

    fn render_map_bbox(
        &self,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        bands: &[u32],
        transparent: bool,
    ) -> DataSourceResult<Vec<u8>> {
        let raster =
            tile::get_raster(&self.path).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
        tile::render_map_bbox(&raster, bbox, width, height, bands, transparent)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))
    }
}

// ─── Vector Data Source ───

pub struct VectorDataSource {
    name: String,
    path: String,
    ext: String,
}

impl VectorDataSource {
    pub fn new(name: String, path: String, ext: String) -> Self {
        Self { name, path, ext }
    }
}

impl DataSource for VectorDataSource {
    fn info(&self) -> DataSourceInfo {
        let tile_info = if self.ext == "shp" {
            crate::shapefile_reader::get_shapefile_info(&self.path)
                .unwrap_or_else(|_| tile::get_vector_tile_info())
        } else {
            tile::get_vector_tile_info()
        };
        DataSourceInfo {
            name: self.name.clone(),
            data_type: DataType::Vector,
            tile_info,
        }
    }

    fn render_raster_tile(
        &self,
        _z: u32,
        _x: u32,
        _y: u32,
        _params: &crate::TileQueryParams,
    ) -> DataSourceResult<Vec<u8>> {
        Err((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Vector source does not support raster tiles".to_string(),
        ))
    }

    fn render_raster_tile_webp(
        &self,
        _z: u32,
        _x: u32,
        _y: u32,
        _params: &crate::TileQueryParams,
    ) -> DataSourceResult<Vec<u8>> {
        Err((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Vector source does not support raster tiles".to_string(),
        ))
    }

    fn render_map_bbox(
        &self,
        _bbox: [f64; 4],
        _width: u32,
        _height: u32,
        _bands: &[u32],
        _transparent: bool,
    ) -> DataSourceResult<Vec<u8>> {
        Err((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Vector source does not support WMS rendering".to_string(),
        ))
    }

    fn render_vector_tile(&self, z: u32, x: u32, y: u32) -> DataSourceResult<Vec<u8>> {
        let req = tile::VectorTileRequest {
            path: self.path.clone(),
            z,
            x,
            y,
        };
        let geojson = match self.ext.as_str() {
            "shp" => tile::get_shapefile_tile_geojson(&req),
            "wkt" => tile::get_wkt_tile_geojson(&req),
            "kml" | "kmz" => tile::get_kml_tile_geojson(&req),
            _ => tile::get_vector_tile_geojson(&req),
        }
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
        Ok(geojson.into_bytes())
    }
}

// ─── Helpers ───

pub fn create_file_source(name: String, path: String, ext: &str) -> Option<Arc<dyn DataSource>> {
    match ext {
        "tif" | "tiff" => Some(Arc::new(RasterDataSource::new(name, path))),
        "geojson" | "json" | "wkt" | "kml" | "kmz" => {
            Some(Arc::new(VectorDataSource::new(name, path, ext.to_string())))
        }
        "shp" => Some(Arc::new(VectorDataSource::new(name, path, ext.to_string()))),
        _ => None,
    }
}
