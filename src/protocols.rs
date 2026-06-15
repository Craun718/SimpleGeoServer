use axum::{
    extract::{Path, Query},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use quick_xml::{
    events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event},
    Writer,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

// ─── 查询参数结构体 ───

#[derive(Deserialize)]
#[allow(dead_code)]
#[serde(rename_all = "UPPERCASE")]
pub(crate) struct WmsQuery {
    #[serde(alias = "service")]
    service: Option<String>,
    #[serde(alias = "request")]
    request: Option<String>,
    #[serde(alias = "version")]
    version: Option<String>,
    #[serde(alias = "layers")]
    layers: Option<String>,
    #[serde(alias = "styles")]
    styles: Option<String>,
    #[serde(alias = "crs")]
    crs: Option<String>,
    #[serde(alias = "bbox")]
    bbox: Option<String>,
    #[serde(alias = "width")]
    width: Option<u32>,
    #[serde(alias = "height")]
    height: Option<u32>,
    #[serde(alias = "format")]
    format: Option<String>,
    #[serde(alias = "transparent")]
    transparent: Option<String>,
    #[serde(alias = "bgcolor")]
    bgcolor: Option<String>,
    #[serde(alias = "exceptions")]
    exceptions: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct WmtsTileParams {
    layer: String,
    z: u32,
    x: u32,
    y: u32,
}

#[derive(Deserialize)]
pub(crate) struct TmsTileParams {
    layer: String,
    z: u32,
    x: u32,
    y: u32,
}

// ─── TileJSON 数据结构 ───

#[derive(Serialize)]
struct TileJson {
    tilejson: String,
    name: String,
    description: String,
    version: String,
    attribution: String,
    scheme: String,
    tiles: Vec<String>,
    minzoom: u32,
    maxzoom: u32,
    bounds: [f64; 4],
    center: [f64; 3],
    format: String,
    r#type: String,
}

// ─── 辅助函数 ───

fn base_url(headers: &HeaderMap) -> String {
    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost:8080");
    format!("http://{}", host)
}

fn file_ext(filename: &str) -> &str {
    std::path::Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
}

fn ows_exception_xml(code: &str, text: &str) -> Response {
    let mut writer = Writer::new_with_indent(Vec::new(), b' ', 2);
    let _ = writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)));
    let root = BytesStart::new("ServiceExceptionReport")
        .with_attributes(vec![
            ("version", "1.3.0"),
            ("xmlns", "http://www.opengis.net/ogc"),
        ]);
    let _ = writer.write_event(Event::Start(root));
    let se = BytesStart::new("ServiceException")
        .with_attributes(vec![("code", code)]);
    let _ = writer.write_event(Event::Start(se));
    let _ = writer.write_event(Event::Text(BytesText::new(text)));
    let _ = writer.write_event(Event::End(BytesEnd::new("ServiceException")));
    let _ = writer.write_event(Event::End(BytesEnd::new("ServiceExceptionReport")));
    let xml = writer.into_inner();
    (
        StatusCode::BAD_REQUEST,
        [(header::CONTENT_TYPE, "text/xml; charset=utf-8")],
        xml,
    )
        .into_response()
}

fn file_info(root: &str, filename: &str) -> Result<(String, crate::tile::TileInfo), Response> {
    let filepath = crate::validate_path(root, filename).map_err(|_e| {
        (StatusCode::NOT_FOUND, format!("Layer not found: {}", filename)).into_response()
    })?;
    let path_str = filepath.to_string_lossy().to_string();
    let ext = file_ext(filename).to_lowercase();

    if crate::is_raster_ext(&ext) {
        let info = crate::tile::get_raster_tile_info(&path_str).map_err(|e| {
            (StatusCode::INTERNAL_SERVER_ERROR, e).into_response()
        })?;
        Ok(("raster".to_string(), info))
    } else if crate::is_vector_ext(&ext) {
        if ext == "shp" {
            let info = crate::shapefile_reader::get_shapefile_info(&path_str).map_err(|e| {
                (StatusCode::INTERNAL_SERVER_ERROR, e).into_response()
            })?;
            Ok(("vector".to_string(), info))
        } else {
            Ok(("vector".to_string(), crate::tile::get_vector_tile_info()))
        }
    } else {
        Err((StatusCode::UNSUPPORTED_MEDIA_TYPE, format!("Unsupported format: .{}", ext)).into_response())
    }
}

fn extent_epsg3857(info: &crate::tile::TileInfo) -> [f64; 4] {
    let merc_y = |lat: f64| {
        let lat = lat.clamp(-85.0511, 85.0511);
        let lat_rad = lat.to_radians();
        crate::tile::R * (std::f64::consts::FRAC_PI_4 + lat_rad / 2.0).tan().ln()
    };
    let merc_x = |lng: f64| lng * crate::tile::C / 180.0;
    let (min_lng, min_lat, max_lng, max_lat) = (info.extent[0], info.extent[1], info.extent[2], info.extent[3]);
    [
        merc_x(min_lng),
        merc_y(min_lat),
        merc_x(max_lng),
        merc_y(max_lat),
    ]
}

fn write_empty(writer: &mut Writer<Vec<u8>>, name: &str, at: &[(&str, &str)]) {
    let mut e = BytesStart::new(name);
    for &(k, v) in at {
        e.push_attribute((k, v));
    }
    writer.write_event(Event::Empty(e)).unwrap();
}

fn get_bands(raster: &crate::tile::CachedRaster) -> Vec<u32> {
    if raster.bands >= 3 { vec![1, 2, 3] } else { vec![1] }
}

// ─── WMS ───

pub(crate) async fn wms_handler(
    root: Arc<String>,
    headers: HeaderMap,
    Query(params): Query<WmsQuery>,
) -> Response {
    let svc = params.service.as_deref().unwrap_or("");
    if svc.to_uppercase() != "WMS" {
        return ows_exception_xml("InvalidParameterValue", "SERVICE must be 'WMS'");
    }

    let req = params.request.as_deref().unwrap_or("");
    match req.to_uppercase().as_str() {
        "GETCAPABILITIES" => wms_capabilities_xml(&root, &headers),
        "GETMAP" => wms_get_map(&root, &headers, &params),
        _ => ows_exception_xml("OperationNotSupported", &format!("Unknown request: {}", req)),
    }
}

fn wms_capabilities_xml(root: &str, headers: &HeaderMap) -> Response {
    let base = base_url(headers);
    let wms_url = format!("{}/ogc/wms", base);
    let geo_files = crate::scan_geo_files(root);

    let mut writer = Writer::new_with_indent(Vec::new(), b' ', 2);
    macro_rules! start {
        ($name:expr) => { writer.write_event(Event::Start(BytesStart::new($name))).unwrap() };
    }
    macro_rules! end {
        ($name:expr) => { writer.write_event(Event::End(BytesEnd::new($name))).unwrap() };
    }
    macro_rules! text {
        ($text:expr) => { writer.write_event(Event::Text(BytesText::new($text))).unwrap() };
    }
    macro_rules! leaf {
        ($name:expr, $value:expr) => {
            start!($name);
            text!($value);
            end!($name);
        };
    }

    writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None))).unwrap();

    let wms_caps = BytesStart::new("WMS_Capabilities")
        .with_attributes(vec![
            ("version", "1.3.0"),
            ("xmlns", "http://www.opengis.net/wms"),
            ("xmlns:sld", "http://www.opengis.net/sld"),
            ("xmlns:xsi", "http://www.w3.org/2001/XMLSchema-instance"),
            ("xmlns:xlink", "http://www.w3.org/1999/xlink"),
        ]);
    writer.write_event(Event::Start(wms_caps)).unwrap();

    start!("Service");
    leaf!("Name", "WMS");
    leaf!("Title", "SimpleGeoServer WMS");
    write_empty(&mut writer, "OnlineResource", &[
        ("xmlns:xlink", "http://www.w3.org/1999/xlink"),
        ("xlink:href", &wms_url),
    ]);
    end!("Service");

    start!("Capability");
    start!("Request");
    start!("GetCapabilities");
    leaf!("Format", "application/vnd.ogc.wms_xml");
    start!("DCPType");
    start!("HTTP");
    start!("Get");
    write_empty(&mut writer, "OnlineResource", &[
        ("xmlns:xlink", "http://www.w3.org/1999/xlink"),
        ("xlink:href", &wms_url),
    ]);
    end!("Get");
    end!("HTTP");
    end!("DCPType");
    end!("GetCapabilities");

    start!("GetMap");
    leaf!("Format", "image/png");
    start!("DCPType");
    start!("HTTP");
    start!("Get");
    write_empty(&mut writer, "OnlineResource", &[
        ("xmlns:xlink", "http://www.w3.org/1999/xlink"),
        ("xlink:href", &wms_url),
    ]);
    end!("Get");
    end!("HTTP");
    end!("DCPType");
    start!("DCPType");
    start!("HTTP");
    start!("Post");
    write_empty(&mut writer, "OnlineResource", &[
        ("xmlns:xlink", "http://www.w3.org/1999/xlink"),
        ("xlink:href", &wms_url),
    ]);
    end!("Post");
    end!("HTTP");
    end!("DCPType");
    end!("GetMap");
    end!("Request");

    start!("Exception");
    leaf!("Format", "XML");
    leaf!("Format", "INIMAGE");
    leaf!("Format", "BLANK");
    end!("Exception");

    start!("Layer");
    leaf!("Title", "SimpleGeoServer WMS");
    leaf!("CRS", "EPSG:3857");

    let mut min_lng = f64::INFINITY;
    let mut max_lng = f64::NEG_INFINITY;
    let mut min_lat = f64::INFINITY;
    let mut max_lat = f64::NEG_INFINITY;
    for fname in &geo_files {
        let ext = file_ext(fname).to_lowercase();
        if !crate::is_raster_ext(&ext) { continue; }
        if let Ok((_, info)) = file_info(root, fname) {
            let e = info.extent;
            if e[0] < min_lng { min_lng = e[0]; }
            if e[2] > max_lng { max_lng = e[2]; }
            if e[1] < min_lat { min_lat = e[1]; }
            if e[3] > max_lat { max_lat = e[3]; }
        }
    }
    if min_lng.is_finite() {
        start!("EX_GeographicBoundingBox");
        leaf!("westBoundLongitude", &format!("{:.10}", min_lng));
        leaf!("eastBoundLongitude", &format!("{:.10}", max_lng));
        leaf!("southBoundLatitude", &format!("{:.10}", min_lat));
        leaf!("northBoundLatitude", &format!("{:.10}", max_lat));
        end!("EX_GeographicBoundingBox");
    }

    for fname in &geo_files {
        let ext = file_ext(fname).to_lowercase();
        if !crate::is_raster_ext(&ext) { continue; }
        if let Ok((_dtype, info)) = file_info(root, fname) {
            let e = info.extent;
            let em = extent_epsg3857(&info);
            let em0s = format!("{:.5}", em[0]);
            let em1s = format!("{:.5}", em[1]);
            let em2s = format!("{:.5}", em[2]);
            let em3s = format!("{:.5}", em[3]);

            start!("Layer");
            leaf!("Name", fname);
            leaf!("Title", fname);
            leaf!("CRS", "EPSG:3857");
            start!("EX_GeographicBoundingBox");
            leaf!("westBoundLongitude", &format!("{:.10}", e[0]));
            leaf!("eastBoundLongitude", &format!("{:.10}", e[2]));
            leaf!("southBoundLatitude", &format!("{:.10}", e[1]));
            leaf!("northBoundLatitude", &format!("{:.10}", e[3]));
            end!("EX_GeographicBoundingBox");
            write_empty(&mut writer, "BoundingBox", &[
                ("CRS", "EPSG:3857"),
                ("minx", &em0s),
                ("miny", &em1s),
                ("maxx", &em2s),
                ("maxy", &em3s),
            ]);
            leaf!("MinScaleDenominator", "0.0");
            leaf!("MaxScaleDenominator", "1e12");
            end!("Layer");
        }
    }
    end!("Layer");
    end!("Capability");
    end!("WMS_Capabilities");

    let xml = writer.into_inner();
    (
        [(header::CONTENT_TYPE, "application/vnd.ogc.wms_xml; charset=utf-8")],
        xml,
    )
        .into_response()
}

fn wms_get_map(root: &str, _headers: &HeaderMap, params: &WmsQuery) -> Response {
    let layers = params.layers.as_deref().unwrap_or("");
    if layers.is_empty() {
        return ows_exception_xml("MissingParameterValue", "LAYERS is required");
    }

    let layer_name = layers.split(',').next().unwrap_or(layers);
    let filepath = match crate::validate_path(root, layer_name) {
        Ok(p) => p,
        Err(e) => return ows_exception_xml("LayerNotDefined", &format!("Layer '{}' not found: {}", layer_name, e.1)),
    };
    let path_str = filepath.to_string_lossy().to_string();
    let ext = file_ext(layer_name).to_lowercase();

    if ext == "shp" {
        return wms_get_map_shapefile(layer_name, &path_str, params, "");
    }

    if !crate::is_raster_ext(&ext) {
        return ows_exception_xml("LayerNotQueryable", "WMS GetMap only supports raster layers (GeoTIFF)");
    }

    let crs = params.crs.as_deref().unwrap_or("EPSG:3857");
    if crs.to_uppercase() != "EPSG:3857" && crs.to_uppercase() != "EPSG:900913" {
        return ows_exception_xml("InvalidCRS", &format!("Unsupported CRS: {} (only EPSG:3857 is supported)", crs));
    }

    let bbox_str_parsed = params.bbox.as_deref().unwrap_or("");
    let bbox_parts: Vec<f64> = bbox_str_parsed
        .split(',')
        .filter_map(|s| s.trim().parse::<f64>().ok())
        .collect();
    if bbox_parts.len() != 4 {
        return ows_exception_xml("MissingParameterValue", "BBOX is required and must be minx,miny,maxx,maxy");
    }
    let bbox = [bbox_parts[0], bbox_parts[1], bbox_parts[2], bbox_parts[3]];

    let width = params.width.unwrap_or(256);
    let height = params.height.unwrap_or(256);
    if width == 0 || height == 0 || width > 4096 || height > 4096 {
        return ows_exception_xml("InvalidParameterValue", "WIDTH and HEIGHT must be between 1 and 4096");
    }

    let transparent = params
        .transparent
        .as_deref()
        .map(|v| v.to_uppercase() == "TRUE")
        .unwrap_or(false);

    let raster = match crate::tile::get_raster(&path_str) {
        Ok(r) => r,
        Err(e) => return ows_exception_xml("InternalError", &e),
    };

    let bands = get_bands(&raster);
    let png_data = match crate::tile::render_map_bbox(&raster, bbox, width, height, &bands, transparent) {
        Ok(d) => d,
        Err(e) => return ows_exception_xml("InternalError", &e),
    };

    (
        [(header::CONTENT_TYPE, "image/png")],
        png_data,
    )
        .into_response()
}

fn wms_get_map_shapefile(
    _layer_name: &str,
    path_str: &str,
    _params: &WmsQuery,
    _bbox_str: &str,
) -> Response {
    let sf = match crate::shapefile_reader::get_shapefile(path_str) {
        Ok(s) => s,
        Err(e) => return ows_exception_xml("InternalError", &e),
    };

    let mut features = Vec::new();
    for (i, geom) in sf.geometries.iter().enumerate() {
        let props = sf.attributes.get(i).and_then(|a| a.clone());
        let gj_geom = match geojson::Geometry::try_from(geom) {
            Ok(g) => g,
            Err(_) => continue,
        };
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
    match serde_json::to_string(&fc) {
        Ok(json) => (
            [(header::CONTENT_TYPE, "application/geo+json")],
            json,
        )
            .into_response(),
        Err(e) => ows_exception_xml("InternalError", &format!("{}", e)),
    }
}

// ─── WMTS ───

pub(crate) async fn wmts_capabilities(
    root: Arc<String>,
    headers: HeaderMap,
) -> Response {
    let base = base_url(&headers);
    let wmts_url = format!("{}/ogc/wmts/1.0.0", base);
    let geo_files = crate::scan_geo_files(&root);

    let mut writer = Writer::new_with_indent(Vec::new(), b' ', 2);
    macro_rules! start {
        ($name:expr) => { writer.write_event(Event::Start(BytesStart::new($name))).unwrap() };
    }
    macro_rules! end {
        ($name:expr) => { writer.write_event(Event::End(BytesEnd::new($name))).unwrap() };
    }
    macro_rules! leaf {
        ($name:expr, $value:expr) => {
            start!($name);
            writer.write_event(Event::Text(BytesText::new($value))).unwrap();
            end!($name);
        };
    }

    writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None))).unwrap();

    let caps = BytesStart::new("Capabilities")
        .with_attributes(vec![
            ("xmlns", "http://www.opengis.net/wmts/1.0"),
            ("xmlns:ows", "http://www.opengis.net/ows/1.1"),
            ("xmlns:xlink", "http://www.w3.org/1999/xlink"),
            ("xmlns:xsi", "http://www.w3.org/2001/XMLSchema-instance"),
            ("version", "1.0.0"),
        ]);
    writer.write_event(Event::Start(caps)).unwrap();

    start!("ows:ServiceIdentification");
    leaf!("ows:Title", "SimpleGeoServer WMTS");
    leaf!("ows:ServiceType", "OGC WMTS");
    leaf!("ows:ServiceTypeVersion", "1.0.0");
    end!("ows:ServiceIdentification");

    start!("ows:ServiceProvider");
    leaf!("ows:ProviderName", "SimpleGeoServer");
    end!("ows:ServiceProvider");

    start!("Contents");

    for fname in &geo_files {
        let ext = file_ext(fname).to_lowercase();
        if !crate::is_raster_ext(&ext) { continue; }
        if let Ok((_, info)) = file_info(&root, fname) {
            let em = extent_epsg3857(&info);
            let em0s = format!("{:.5}", em[0]);
            let em1s = format!("{:.5}", em[1]);
            let em2s = format!("{:.5}", em[2]);
            let em3s = format!("{:.5}", em[3]);

            start!("Layer");
            leaf!("ows:Title", fname);
            leaf!("ows:Identifier", fname);
            write_empty(&mut writer, "ows:BoundingBox", &[
                ("CRS", "EPSG:3857"),
                ("minx", &em0s),
                ("miny", &em1s),
                ("maxx", &em2s),
                ("maxy", &em3s),
            ]);

            start!("Style");
            leaf!("ows:Identifier", "default");
            leaf!("ows:Title", "Default Style");
            end!("Style");

            leaf!("Format", "image/png");

            start!("TileMatrixSetLink");
            leaf!("TileMatrixSet", "GoogleMapsCompatible");
            end!("TileMatrixSetLink");

            let tmpl = format!("{}/{{layer}}/default/GoogleMapsCompatible/{{TileMatrix}}/{{TileCol}}/{{TileRow}}.png", wmts_url);
            write_empty(&mut writer, "ResourceURL", &[
                ("format", "image/png"),
                ("resourceType", "tile"),
                ("template", &tmpl),
            ]);

            end!("Layer");
        }
    }

    start!("TileMatrixSet");
    leaf!("ows:Identifier", "GoogleMapsCompatible");
    leaf!("ows:SupportedCRS", "EPSG:3857");

    let tile_size = 256u64;
    let max_zoom = 22;
    for z in 0..=max_zoom {
        let n = 1u64 << z;
        let scale_denom = (2.0 * crate::tile::C) / (tile_size as f64 * n as f64 * 0.00028);
        let sd_str = format!("{:.10}", scale_denom);
        let tc_str = format!("{:.10} {}", -crate::tile::C, crate::tile::C);

        start!("TileMatrix");
        leaf!("ows:Identifier", &z.to_string());
        leaf!("ScaleDenominator", &sd_str);
        leaf!("TopLeftCorner", &tc_str);
        leaf!("TileWidth", &tile_size.to_string());
        leaf!("TileHeight", &tile_size.to_string());
        leaf!("MatrixWidth", &n.to_string());
        leaf!("MatrixHeight", &n.to_string());
        end!("TileMatrix");
    }
    end!("TileMatrixSet");
    end!("Contents");
    end!("Capabilities");

    let xml = writer.into_inner();
    (
        [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
        xml,
    )
        .into_response()
}

pub(crate) async fn wmts_get_tile(
    root: Arc<String>,
    Path(params): Path<WmtsTileParams>,
) -> Response {
    let filename = &params.layer;
    let filepath = match crate::validate_path(&root, filename) {
        Ok(p) => p,
        Err(_) => return (StatusCode::NOT_FOUND, "Layer not found").into_response(),
    };
    let path_str = filepath.to_string_lossy().to_string();
    let ext = file_ext(filename).to_lowercase();

    if !crate::is_raster_ext(&ext) {
        return (StatusCode::UNSUPPORTED_MEDIA_TYPE, "WMTS only supports raster layers").into_response();
    }

    let raster = match crate::tile::get_raster(&path_str) {
        Ok(r) => r,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };

    let bands = get_bands(&raster);
    match crate::tile::render_raster_tile(&raster, params.z, params.x, params.y, 256, &bands) {
        Ok((png_data, _)) => (
            [(header::CONTENT_TYPE, "image/png")],
            png_data,
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

// ─── TMS ───

pub(crate) async fn tms_root(
    root: Arc<String>,
    headers: HeaderMap,
) -> Response {
    let base = base_url(&headers);
    let tms_url = format!("{}/ogc/tms/1.0.0", base);
    let geo_files = crate::scan_geo_files(&root);

    let mut writer = Writer::new_with_indent(Vec::new(), b' ', 2);

    writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None))).unwrap();

    let root_elem = BytesStart::new("TileMaps")
        .with_attributes(vec![
            ("version", "1.0.0"),
            ("xmlns", "http://tms.osgeo.org/1.0.0"),
        ]);
    writer.write_event(Event::Start(root_elem)).unwrap();

    for fname in &geo_files {
        let ext = file_ext(fname).to_lowercase();
        if !crate::is_raster_ext(&ext) { continue; }
        let layer_url = format!("{}/{}", tms_url, fname);
        write_empty(&mut writer, "TileMap", &[
            ("title", fname),
            ("srs", "EPSG:3857"),
            ("profile", "global-geodetic"),
            ("href", &layer_url),
        ]);
    }

    writer.write_event(Event::End(BytesEnd::new("TileMaps"))).unwrap();
    let xml = writer.into_inner();

    (
        [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
        xml,
    )
        .into_response()
}

pub(crate) async fn tms_layer(
    root: Arc<String>,
    headers: HeaderMap,
    Path(layer): Path<String>,
) -> Response {
    let base = base_url(&headers);
    let tms_url = format!("{}/ogc/tms/1.0.0", base);
    let layer_url = format!("{}/{}", tms_url, layer);

    let (dtype, info) = match file_info(&root, &layer) {
        Ok(v) => v,
        Err(r) => return r,
    };

    if dtype != "raster" {
        return (StatusCode::UNSUPPORTED_MEDIA_TYPE, "TMS only supports raster layers").into_response();
    }

    let em = extent_epsg3857(&info);
    let em0s = format!("{:.5}", em[0]);
    let em1s = format!("{:.5}", em[1]);
    let em2s = format!("{:.5}", em[2]);
    let em3s = format!("{:.5}", em[3]);

    let mut writer = Writer::new_with_indent(Vec::new(), b' ', 2);
    macro_rules! start {
        ($name:expr) => { writer.write_event(Event::Start(BytesStart::new($name))).unwrap() };
    }
    macro_rules! end {
        ($name:expr) => { writer.write_event(Event::End(BytesEnd::new($name))).unwrap() };
    }
    macro_rules! leaf {
        ($name:expr, $value:expr) => {
            start!($name);
            writer.write_event(Event::Text(BytesText::new($value))).unwrap();
            end!($name);
        };
    }

    writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None))).unwrap();

    let tm = BytesStart::new("TileMap")
        .with_attributes(vec![
            ("version", "1.0.0"),
            ("xmlns", "http://tms.osgeo.org/1.0.0"),
        ]);
    writer.write_event(Event::Start(tm)).unwrap();
    leaf!("Title", &layer);
    leaf!("Abstract", &format!("TMS layer for {}", layer));
    leaf!("SRS", "EPSG:3857");

    write_empty(&mut writer, "BoundingBox", &[
        ("minx", &em0s),
        ("miny", &em1s),
        ("maxx", &em2s),
        ("maxy", &em3s),
    ]);

    let ox = (-crate::tile::C).to_string();
    let oy = (-crate::tile::C).to_string();
    write_empty(&mut writer, "Origin", &[
        ("x", &ox),
        ("y", &oy),
    ]);

    write_empty(&mut writer, "TileFormat", &[
        ("width", "256"),
        ("height", "256"),
        ("mime-type", "image/png"),
        ("extension", "png"),
    ]);

    start!("TileSets");
    let max_zoom = info.max_zoom.min(22);
    for z in 0..=max_zoom {
        let n = 1u64 << z;
        let res = 2.0 * crate::tile::C / (256.0 * n as f64);
        let href = format!("{}/{}", layer_url, z);
        let res_str = format!("{:.10}", res);
        let order_str = z.to_string();
        write_empty(&mut writer, "TileSet", &[
            ("href", &href),
            ("units-per-pixel", &res_str),
            ("order", &order_str),
        ]);
    }
    end!("TileSets");
    end!("TileMap");

    let xml = writer.into_inner();
    (
        [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
        xml,
    )
        .into_response()
}

pub(crate) async fn tms_get_tile(
    root: Arc<String>,
    Path(params): Path<TmsTileParams>,
) -> Response {
    let filename = &params.layer;
    let filepath = match crate::validate_path(&root, filename) {
        Ok(p) => p,
        Err(_) => return (StatusCode::NOT_FOUND, "Layer not found").into_response(),
    };
    let path_str = filepath.to_string_lossy().to_string();
    let ext = file_ext(filename).to_lowercase();

    if !crate::is_raster_ext(&ext) {
        return (StatusCode::UNSUPPORTED_MEDIA_TYPE, "TMS only supports raster layers").into_response();
    }

    let raster = match crate::tile::get_raster(&path_str) {
        Ok(r) => r,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };

    let max_y = (1u64 << params.z) - 1;
    let xyz_y = if params.y as u64 <= max_y {
        (max_y - params.y as u64) as u32
    } else {
        return (StatusCode::BAD_REQUEST, "Tile Y out of range").into_response();
    };

    let bands = get_bands(&raster);
    match crate::tile::render_raster_tile(&raster, params.z, params.x, xyz_y, 256, &bands) {
        Ok((png_data, _)) => (
            [(header::CONTENT_TYPE, "image/png")],
            png_data,
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

// ─── TileJSON ───

pub(crate) async fn tilejson(
    root: Arc<String>,
    headers: HeaderMap,
    Path(filename): Path<String>,
) -> Response {
    let (dtype, info) = match file_info(&root, &filename) {
        Ok(v) => v,
        Err(r) => return r,
    };

    let base = base_url(&headers);
    let tile_url = if dtype == "raster" {
        format!("{}/api/tiles/{}/png/{{z}}/{{x}}/{{y}}", base, filename)
    } else {
        format!("{}/api/tiles/{}/geojson/{{z}}/{{x}}/{{y}}", base, filename)
    };

    let fmt = if dtype == "raster" { "png" } else { "geojson" };
    let tile_type = if dtype == "raster" { "raster" } else { "vector" };

    let e = info.extent;
    let center_lng = (e[0] + e[2]) / 2.0;
    let center_lat = (e[1] + e[3]) / 2.0;
    let center_zoom = ((info.max_zoom + info.min_zoom) / 2).max(info.min_zoom);

    let tj = TileJson {
        tilejson: "3.0.0".to_string(),
        name: filename.clone(),
        description: format!("{} tile layer for {}", tile_type, filename),
        version: "1.0.0".to_string(),
        attribution: String::new(),
        scheme: "xyz".to_string(),
        tiles: vec![tile_url],
        minzoom: info.min_zoom,
        maxzoom: info.max_zoom,
        bounds: e,
        center: [center_lng, center_lat, center_zoom as f64],
        format: fmt.to_string(),
        r#type: tile_type.to_string(),
    };

    Json(tj).into_response()
}
