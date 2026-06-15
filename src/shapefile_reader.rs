use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use geo::BoundingRect;
use once_cell::sync::Lazy;

#[allow(dead_code)]
pub(crate) struct CachedShapefile {
    pub(crate) file_path: String,
    pub(crate) geometries: Vec<geo_types::Geometry<f64>>,
    pub(crate) attributes: Vec<Option<serde_json::Map<String, serde_json::Value>>>,
    pub(crate) crs: crate::reproject::KnownCrs,
    pub(crate) feature_count: u32,
    pub(crate) extent: [f64; 4],
}

static SHAPEFILE_CACHE: Lazy<RwLock<HashMap<String, Arc<CachedShapefile>>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

fn convert_shapes(
    shapes: Vec<shapefile::Shape>,
    crs: crate::reproject::KnownCrs,
) -> Vec<geo_types::Geometry<f64>> {
    let mut geometries = Vec::new();
    for shape in shapes {
        let geom = match geo_types::Geometry::<f64>::try_from(shape) {
            Ok(g) => g,
            Err(_) => continue,
        };
        let geom_wgs84 = if crs != crate::reproject::KnownCrs::Wgs84 {
            crate::reproject::known_crs_geometry_to_wgs84(&geom, crs).unwrap_or(geom)
        } else {
            geom
        };
        geometries.push(geom_wgs84);
    }
    geometries
}

fn load_and_cache_shapefile(path: &str) -> Result<Arc<CachedShapefile>, String> {
    let prj_path = std::path::Path::new(path).with_extension("prj");
    let crs = if prj_path.exists() {
        parse_prj_file(&prj_path).unwrap_or(crate::reproject::KnownCrs::Wgs84)
    } else {
        crate::reproject::KnownCrs::Wgs84
    };

    let shapes = shapefile::read_shapes(path)
        .map_err(|e| format!("Failed to read shapefile: {}", e))?;

    let geometries = convert_shapes(shapes, crs);
    let feature_count = geometries.len() as u32;
    let no_attrs = vec![None; feature_count as usize];

    let extent = geometries
        .iter()
        .filter_map(|g| g.bounding_rect())
        .fold(None, |acc: Option<geo_types::Rect<f64>>, r| {
            Some(match acc {
                Some(e) => geo_types::Rect::new(
                    geo_types::coord! {
                        x: e.min().x.min(r.min().x),
                        y: e.min().y.min(r.min().y),
                    },
                    geo_types::coord! {
                        x: e.max().x.max(r.max().x),
                        y: e.max().y.max(r.max().y),
                    },
                ),
                None => r,
            })
        });
    let extent_arr = match extent {
        Some(r) => [r.min().x, r.min().y, r.max().x, r.max().y],
        None => [0.0, 0.0, 0.0, 0.0],
    };

    Ok(Arc::new(CachedShapefile {
        file_path: path.to_string(),
        geometries,
        attributes: no_attrs,
        crs,
        feature_count,
        extent: extent_arr,
    }))
}

fn parse_prj_file(path: &std::path::Path) -> Option<crate::reproject::KnownCrs> {
    let content = std::fs::read_to_string(path).ok()?;

    if let Some(cap) = content.split("AUTHORITY").nth(1) {
        if let Some(epsg_str) = cap.split('"').nth(3) {
            if epsg_str == "900913" {
                return Some(crate::reproject::KnownCrs::WebMercator);
            }
            if let Ok(code) = epsg_str.parse::<u16>() {
                return parse_epsg_code(code);
            }
        }
    }
    if content.contains("4326") || content.contains("WGS 84") || content.contains("GCS_WGS_1984")
    {
        return Some(crate::reproject::KnownCrs::Wgs84);
    }
    if content.contains("3857") || content.contains("900913") || content.contains("Mercator") {
        return Some(crate::reproject::KnownCrs::WebMercator);
    }
    None
}

fn parse_epsg_code(code: u16) -> Option<crate::reproject::KnownCrs> {
    match code {
        4326 => Some(crate::reproject::KnownCrs::Wgs84),
        3857 => Some(crate::reproject::KnownCrs::WebMercator),
        32601..=32660 => Some(crate::reproject::KnownCrs::UtmWgs84 {
            zone: (code - 32600) as u8,
            northern: true,
        }),
        32701..=32760 => Some(crate::reproject::KnownCrs::UtmWgs84 {
            zone: (code - 32700) as u8,
            northern: false,
        }),
        _ => None,
    }
}

pub(crate) fn get_shapefile(path: &str) -> Result<Arc<CachedShapefile>, String> {
    {
        let cache = SHAPEFILE_CACHE
            .read()
            .map_err(|e| format!("Cache lock error: {}", e))?;
        if let Some(sf) = cache.get(path) {
            return Ok(Arc::clone(sf));
        }
    }
    let sf = load_and_cache_shapefile(path)?;
    let arc = Arc::clone(&sf);
    {
        let mut cache = SHAPEFILE_CACHE
            .write()
            .map_err(|e| format!("Cache lock error: {}", e))?;
        cache.insert(path.to_string(), sf);
    }
    Ok(arc)
}

pub(crate) fn get_shapefile_info(path: &str) -> Result<crate::tile::TileInfo, String> {
    let sf = get_shapefile(path)?;
    Ok(crate::tile::TileInfo {
        data_type: "vector".to_string(),
        min_zoom: 0,
        max_zoom: 22,
        crs: "EPSG:4326".to_string(),
        extent: sf.extent,
        native_crs: "EPSG:4326".to_string(),
        native_extent: sf.extent,
    })
}
