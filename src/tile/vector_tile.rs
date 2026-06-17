use geo::Intersects;

use super::tile_math::{wgs84_tile_rect, C};
use super::types::{TileInfo, VectorTileRequest};

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

pub fn get_raster_tile_info(path: &str) -> Result<TileInfo, String> {
    let raster = super::raster_cache::get_raster(path)?;
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
