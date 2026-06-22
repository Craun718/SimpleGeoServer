use axum::{
    Extension, Json, Router,
    body::Body,
    extract::{Path, Query},
    http::{HeaderMap, Request, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use clap::{Parser, Subcommand};
use serde::Deserialize;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use tower_http::{
    compression::CompressionLayer, cors::CorsLayer, trace::TraceLayer,
};
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;
use utoipauto::utoipauto;

use crate::registry::DataSourceRegistry;

fn deserialize_bands<'de, D>(deserializer: D) -> Result<Option<Vec<u32>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s: Option<String> = Option::deserialize(deserializer)?;
    Ok(s.map(|s| s.split(',').filter_map(|v| v.trim().parse().ok()).collect()))
}

#[derive(Deserialize)]
pub struct TileQueryParams {
    resampling: Option<String>,
    stretch: Option<String>,
    std_dev_factor: Option<f64>,
    min_percent: Option<f64>,
    max_percent: Option<f64>,
    #[serde(default, deserialize_with = "deserialize_bands")]
    bands: Option<Vec<u32>>,
}

impl Default for TileQueryParams {
    fn default() -> Self {
        Self {
            resampling: None,
            stretch: None,
            std_dev_factor: None,
            min_percent: None,
            max_percent: None,
            bands: None,
        }
    }
}

pub mod batch_render;
pub mod config;
pub mod data_source;
#[cfg(feature = "gpu")]
pub mod gpu_renderer;
pub mod protocols;
pub mod raster;
pub mod registry;
pub mod render_farm;
pub mod reproject;
pub mod resample;
pub(crate) mod shapefile_reader;
pub mod tile;
pub mod tile_cache;

#[derive(Parser)]
#[command(
    name = "SimpleGeoServer",
    about = "A simple HTTP static file server with tile serving"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,

    #[arg(short, long)]
    pub config: Option<PathBuf>,

    #[arg(short, long)]
    pub threads: Option<u32>,

    #[arg(short = 'f', long)]
    pub full_data: Option<bool>,

    #[arg(short, long)]
    pub port: Option<u16>,

    #[arg(short, long)]
    pub address: Option<String>,

    #[arg(short = 'd', long)]
    pub root: Option<String>,

    #[arg()]
    pub dir: Option<String>,

    #[arg(long)]
    pub cache_max_age: Option<i32>,

    #[arg(long)]
    pub cors: Option<bool>,

    #[arg(short, long)]
    pub gzip: Option<bool>,

    #[arg(long)]
    pub no_dotfiles: Option<bool>,

    #[arg(long)]
    pub log_format: Option<String>,

    #[arg(long)]
    pub l2_cache_mb: Option<u64>,

    #[arg(long = "allow-path", value_name = "PATH", action = clap::ArgAction::Append)]
    pub allow_path: Option<Vec<String>>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Generate a default config.yaml in the current directory
    Init,
    /// Export OpenAPI spec to a JSON file
    ExportOpenapi {
        #[arg(short, long, default_value = "openapi.json")]
        output: PathBuf,
    },
}

/// Programmatic server configuration (replaces CLI args for embedded use)
pub struct ServerConfig {
    pub threads: u32,
    pub cache_max_age: i32,
    pub cors: bool,
    pub gzip: bool,
    pub no_dotfiles: bool,
    pub l2_cache_mb: u64,
    pub disk_cache_dir: Option<String>,
    pub allowed_paths: Vec<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            threads: 4,
            cache_max_age: 3600,
            cors: true,
            gzip: false,
            no_dotfiles: false,
            l2_cache_mb: 512,
            disk_cache_dir: None,
            allowed_paths: vec![],
        }
    }
}

async fn set_cache_header(
    Extension(cache): Extension<Arc<i64>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let mut res = next.run(req).await;
    if *cache >= 0 && (res.status().is_success() || res.status() == StatusCode::NOT_MODIFIED) {
        res.headers_mut().insert(
            header::CACHE_CONTROL,
            format!("public, max-age={}", cache).parse().unwrap(),
        );
    }
    res
}

async fn filter_dotfiles(req: Request<Body>, next: Next) -> Response {
    let path = req.uri().path();
    if path.split('/').any(|s| !s.is_empty() && s.starts_with('.')) {
        return StatusCode::NOT_FOUND.into_response();
    }
    next.run(req).await
}

// ─── 健康检查 ───

async fn health() -> &'static str {
    "OK"
}

// ─── 切片服务处理器 ───

#[utoipa::path(
    get,
    path = "/api/geo-files",
    responses(
        (status = 200, description = "List of available geo files", body = Vec<crate::tile::GeoFileInfo>),
    ),
    tag = "Files",
)]
fn source_to_geo_file(info: &data_source::DataSourceInfo) -> tile::GeoFileInfo {
    tile::GeoFileInfo {
        name: info.name.clone(),
        path: info.name.clone(),
        data_type: info.data_type.as_str().to_string(),
        info: info.tile_info.clone(),
    }
}

async fn list_geo_files(registry: Arc<DataSourceRegistry>) -> Json<Vec<tile::GeoFileInfo>> {
    let files: Vec<tile::GeoFileInfo> = registry.list().iter().map(source_to_geo_file).collect();
    Json(files)
}

async fn list_sources(registry: Arc<DataSourceRegistry>) -> Json<Vec<data_source::DataSourceInfo>> {
    Json(registry.list())
}

#[utoipa::path(
    get,
    path = "/api/tiles/{filename}/info",
    params(
        ("filename" = String, Path, description = "File name"),
    ),
    responses(
        (status = 200, description = "Tile metadata", body = crate::tile::TileInfo),
        (status = 404, description = "File not found"),
        (status = 415, description = "Unsupported format"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "Files",
)]
async fn get_tile_info(
    registry: Arc<DataSourceRegistry>,
    Path(filename): Path<String>,
) -> Result<Json<tile::TileInfo>, (StatusCode, String)> {
    let source = registry.get(&filename).ok_or((
        StatusCode::NOT_FOUND,
        format!("Source not found: {}", filename),
    ))?;
    Ok(Json(source.info().tile_info))
}

#[utoipa::path(
    get,
    path = "/api/tiles/{filename}/png/{z}/{x}/{y}",
    params(
        ("filename" = String, Path, description = "File name"),
        ("z" = u32, Path, description = "Zoom level"),
        ("x" = u32, Path, description = "Tile X coordinate"),
        ("y" = u32, Path, description = "Tile Y coordinate"),
    ),
    responses(
        (status = 200, description = "PNG tile image", content_type = "image/png"),
        (status = 404, description = "File not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "Tiles",
)]
async fn get_raster_tile(
    registry: Arc<DataSourceRegistry>,
    Path((filename, z, x, y)): Path<(String, u32, u32, u32)>,
    Query(params): Query<TileQueryParams>,
) -> Result<Response, (StatusCode, String)> {
    let source = registry.get(&filename).ok_or((
        StatusCode::NOT_FOUND,
        format!("Source not found: {}", filename),
    ))?;
    let png = source.render_raster_tile(z, x, y, &params)?;
    Ok(([(header::CONTENT_TYPE, "image/png")], png).into_response())
}

#[utoipa::path(
    get,
    path = "/api/tiles/{filename}/geojson/{z}/{x}/{y}",
    params(
        ("filename" = String, Path, description = "File name"),
        ("z" = u32, Path, description = "Zoom level"),
        ("x" = u32, Path, description = "Tile X coordinate"),
        ("y" = u32, Path, description = "Tile Y coordinate"),
    ),
    responses(
        (status = 200, description = "GeoJSON FeatureCollection", content_type = "application/geo+json"),
        (status = 404, description = "File not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "Tiles",
)]
async fn get_vector_tile(
    registry: Arc<DataSourceRegistry>,
    Path((filename, z, x, y)): Path<(String, u32, u32, u32)>,
) -> Result<Response, (StatusCode, String)> {
    let source = registry.get(&filename).ok_or((
        StatusCode::NOT_FOUND,
        format!("Source not found: {}", filename),
    ))?;
    let geojson = source.render_vector_tile(z, x, y)?;
    Ok(([(header::CONTENT_TYPE, "application/geo+json")], geojson).into_response())
}

// ─── WebP 瓦片 ───

async fn get_raster_tile_webp(
    registry: Arc<DataSourceRegistry>,
    Path((filename, z, x, y)): Path<(String, u32, u32, u32)>,
    Query(params): Query<TileQueryParams>,
) -> Result<Response, (StatusCode, String)> {
    let source = registry.get(&filename).ok_or((
        StatusCode::NOT_FOUND,
        format!("Source not found: {}", filename),
    ))?;
    let webp = source.render_raster_tile_webp(z, x, y, &params)?;
    Ok(([(header::CONTENT_TYPE, "image/webp")], webp).into_response())
}

// ─── 动态挂载/卸载 ───

#[derive(Deserialize)]
pub struct MountRequest {
    pub name: String,
    pub path: String,
}

async fn mount_source(
    registry: Arc<DataSourceRegistry>,
    root: Arc<String>,
    allowed_paths: Arc<Vec<String>>,
    Json(req): Json<MountRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let filepath = validate_path(root.as_str(), &allowed_paths, &req.path)?;
    let path_str = filepath.to_string_lossy().to_string();
    let ext = filepath
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default();

    let source = data_source::create_file_source(req.name.clone(), path_str, &ext).ok_or((
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        format!("Unsupported file type: .{}", ext),
    ))?;

    registry
        .mount(req.name.clone(), source)
        .map_err(|e| (StatusCode::CONFLICT, e))?;

    Ok(Json(serde_json::json!({"status": "ok", "name": req.name})))
}

async fn unmount_source(
    registry: Arc<DataSourceRegistry>,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    registry
        .unmount(&name)
        .map_err(|e| (StatusCode::NOT_FOUND, e))?;
    Ok(Json(serde_json::json!({"status": "ok", "name": name})))
}

// ─── 批量渲染 ───

#[derive(Deserialize)]
pub struct BatchTileReq {
    pub z: u32,
    pub x: u32,
    pub y: u32,
}

#[derive(Deserialize)]
pub struct BatchRequest {
    pub filename: String,
    pub tiles: Vec<BatchTileReq>,
    pub resampling: Option<String>,
    pub stretch: Option<String>,
    pub bands: Option<Vec<u32>>,
}

async fn batch_tiles(
    root: Arc<String>,
    allowed_paths: Arc<Vec<String>>,
    Json(req): Json<BatchRequest>,
) -> Result<Json<Vec<serde_json::Value>>, (StatusCode, String)> {
    let filepath = validate_path(root.as_str(), &allowed_paths, &req.filename)?;
    let path_str = filepath.to_string_lossy().to_string();
    let resampling = req
        .resampling
        .as_deref()
        .map(resample::ResamplingMode::from_str)
        .unwrap_or(resample::ResamplingMode::NearestNeighbor);
    let bands = req.bands.clone().unwrap_or_else(|| vec![1, 2, 3]);
    let stretch = req.stretch.as_deref().map(|s| resample::StretchConfig {
        method: resample::StretchMethod::from_str(s),
        min_percent: None,
        max_percent: None,
        std_dev_factor: None,
    });

    let jobs: Vec<batch_render::BatchTileJob> = req
        .tiles
        .iter()
        .map(|t| batch_render::BatchTileJob {
            path: path_str.clone(),
            z: t.z,
            x: t.x,
            y: t.y,
            bands: bands.clone(),
            resampling,
            stretch: stretch.clone(),
            priority: t.z,
        })
        .collect();

    let results = batch_render::render_tiles_parallel(jobs, 4);
    let json_results: Vec<serde_json::Value> = results
        .into_iter()
        .map(|(_, r)| match r {
            Ok(tile) => {
                serde_json::json!({
                    "status": "ok",
                    "z": tile.z,
                    "x": tile.x,
                    "y": tile.y,
                    "bytes": tile.data.len(),
                    "rendered": tile.rendered,
                })
            }
            Err(e) => serde_json::json!({"status": "error", "error": e}),
        })
        .collect();

    Ok(Json(json_results))
}

// ─── OpenAPI 文档 ───

#[utoipauto]
#[derive(OpenApi)]
#[openapi(info(
    title = "SimpleGeoServer API",
    description = "Geospatial file server with raster and vector tile serving",
    version = "0.1.0",
))]
struct ApiDoc;

pub fn directory_size_bytes(path: &std::path::Path) -> Result<u64, String> {
    let mut total = 0u64;
    if path.is_dir() {
        for entry in std::fs::read_dir(path).map_err(|e| format!("Read dir: {e}"))? {
            let entry = entry.map_err(|e| format!("Entry: {e}"))?;
            let ft = entry.file_type().map_err(|e| format!("FileType: {e}"))?;
            if ft.is_dir() {
                total += directory_size_bytes(&entry.path())?;
            } else {
                total += entry
                    .metadata()
                    .map_err(|e| format!("Metadata: {e}"))?
                    .len();
            }
        }
    }
    Ok(total)
}

pub fn validate_path(
    root: &str,
    allowed_paths: &[String],
    filename: &str,
) -> Result<std::path::PathBuf, (StatusCode, String)> {
    let p = std::path::Path::new(filename);
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                return Err((StatusCode::BAD_REQUEST, "Path traversal detected".into()));
            }
            std::path::Component::RootDir => {
                return Err((StatusCode::BAD_REQUEST, "Absolute path not allowed".into()));
            }
            _ => {}
        }
    }

    let bases = std::iter::once(root.to_string()).chain(allowed_paths.iter().cloned());
    for base in bases {
        let filepath = std::path::Path::new(&base).join(filename);
        if filepath.exists() {
            let base_canonical = std::path::Path::new(&base).canonicalize().map_err(|_| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Invalid base directory".into(),
                )
            })?;
            let file_canonical = filepath.canonicalize().map_err(|_| {
                (
                    StatusCode::NOT_FOUND,
                    format!("File not found: {}", filename),
                )
            })?;
            if file_canonical.starts_with(&base_canonical) {
                return Ok(file_canonical);
            }
        }
    }

    Err((
        StatusCode::NOT_FOUND,
        format!("File not found: {}", filename),
    ))
}

pub fn is_raster_ext(ext: &str) -> bool {
    matches!(ext, "tif" | "tiff")
}

pub fn is_vector_ext(ext: &str) -> bool {
    matches!(ext, "geojson" | "json" | "shp" | "wkt" | "kml" | "kmz")
}

fn make_operation_id(base: &str, filename: &str) -> String {
    let safe = filename.replace(|c: char| !c.is_alphanumeric() && c != '_', "_");
    format!("{}_{}", base, safe)
}

fn build_dynamic_spec(geo_files: &[String]) -> serde_json::Value {
    let mut spec: serde_json::Value =
        serde_json::to_value(ApiDoc::openapi()).expect("Failed to serialize ApiDoc");

    let file_list_md = geo_files
        .iter()
        .map(|f| format!("- `{}`", f))
        .collect::<Vec<_>>()
        .join("\n");
    let description = format!(
        "Geospatial file server with raster and vector tile serving.\n\n\
         ### Available Geo Files ({})\n\n{}",
        geo_files.len(),
        file_list_md,
    );

    if let Some(info) = spec.get_mut("info") {
        info["description"] = serde_json::Value::String(description);
        info["x-geo-files"] = serde_json::Value::Array(
            geo_files
                .iter()
                .map(|f| serde_json::Value::String(f.clone()))
                .collect(),
        );
    }

    if geo_files.is_empty() {
        return spec;
    }

    // Expand {filename} template paths into concrete per-file paths
    let Some(paths_obj) = spec.get_mut("paths").and_then(|p| p.as_object_mut()) else {
        return spec;
    };

    let template_paths: Vec<String> = paths_obj
        .keys()
        .filter(|k| k.contains("{filename}"))
        .cloned()
        .collect();

    let mut concrete_paths: Vec<(String, serde_json::Value)> = Vec::new();

    for template_path in &template_paths {
        let Some(path_item) = paths_obj.get(template_path).cloned() else {
            continue;
        };

        for filename in geo_files {
            let ext = std::path::Path::new(filename)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();

            // Skip filetype-specific endpoints
            if template_path.contains("/png/") && !is_raster_ext(&ext) {
                continue;
            }
            if template_path.contains("/geojson/") && !is_vector_ext(&ext) {
                continue;
            }

            let concrete_path = template_path.replace("{filename}", filename);
            let mut item = path_item.clone();

            // Customize the operation for this concrete file
            if let Some(get_op) = item.get_mut("get") {
                // Remove filename from parameters
                if let Some(params) = get_op.get_mut("parameters") {
                    if let Some(arr) = params.as_array_mut() {
                        arr.retain(|p| {
                            p.get("name")
                                != Some(&serde_json::Value::String("filename".to_string()))
                        });
                    }
                }

                // Set unique operationId
                let base_op_id = get_op
                    .get("operationId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("operation");
                get_op["operationId"] =
                    serde_json::Value::String(make_operation_id(base_op_id, filename));

                // Set meaningful summary
                let tag = if template_path.contains("/info") {
                    "info"
                } else if template_path.contains("/png/") {
                    "raster tile (PNG)"
                } else {
                    "vector tile (GeoJSON)"
                };
                get_op["summary"] =
                    serde_json::Value::String(format!("Get {} for {}", tag, filename));
            }

            concrete_paths.push((concrete_path, item));
        }

        // Remove the template path
        paths_obj.remove(template_path);
    }

    // Add all concrete paths (sorted by filename)
    concrete_paths.sort_by(|a, b| a.0.cmp(&b.0));
    for (path, item) in concrete_paths {
        paths_obj.insert(path, item);
    }

    spec
}

fn scan_and_auto_mount(paths: &[String], registry: &DataSourceRegistry) {
    for dir_path in paths {
        let dir = std::path::Path::new(dir_path);
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) if n != "openapi.json" => n.to_string(),
                _ => continue,
            };
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_lowercase())
                .unwrap_or_default();
            let path_str = path.to_string_lossy().to_string();
            if let Some(source) = data_source::create_file_source(name.clone(), path_str, &ext) {
                if let Err(e) = registry.mount(name, source) {
                    log::warn!("Failed to mount '{}': {}", path.display(), e);
                }
            }
        }
    }
}

fn build_registry_from_config(
    file_config: &config::AppConfig,
    scan_paths: &[String],
) -> Arc<DataSourceRegistry> {
    let registry = Arc::new(DataSourceRegistry::new());

    if let Some(sources) = &file_config.sources {
        for src in sources {
            let ext = std::path::Path::new(&src.path)
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_lowercase())
                .unwrap_or_default();
            if let Some(source) =
                data_source::create_file_source(src.name.clone(), src.path.clone(), &ext)
            {
                if let Err(e) = registry.mount(src.name.clone(), source) {
                    log::warn!("Failed to mount '{}' from config: {}", src.name, e);
                }
            } else {
                log::warn!("Unsupported file type for source '{}': .{}", src.name, ext);
            }
        }
    }

    scan_and_auto_mount(scan_paths, &registry);
    registry
}

fn load_config_file(path: &Option<std::path::PathBuf>) -> Option<config::AppConfig> {
    let p = path.as_ref()?;
    match config::load_config(p) {
        Ok(cfg) => {
            log::info!("Loaded config from {}", p.display());
            Some(cfg)
        }
        Err(e) => {
            log::warn!("Failed to load config: {e}");
            None
        }
    }
}

macro_rules! merge_opt {
    ($cli:expr, $cfg:expr, $default:expr) => {
        $cli.clone().or_else(|| $cfg.clone()).unwrap_or($default)
    };
}

fn mime_type(path: &str) -> &str {
    let ext = path.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "html" => "text/html",
        "js" => "application/javascript",
        "css" => "text/css",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "json" => "application/json",
        "xml" => "application/xml",
        "tif" | "tiff" => "image/tiff",
        "geojson" => "application/geo+json",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

async fn fallback_handler(
    Extension(dirs): Extension<Arc<Vec<String>>>,
    req: Request<Body>,
) -> Response {
    let path = req.uri().path();
    let clean = path.trim_start_matches('/');

    if clean.split('/').any(|s| s == "..") {
        return StatusCode::BAD_REQUEST.into_response();
    }

    for base in dirs.iter() {
        let full = std::path::Path::new(base).join(clean);
        if full.is_file() {
            match tokio::fs::read(&full).await {
                Ok(data) => {
                    return Response::builder()
                        .header(header::CONTENT_TYPE, mime_type(path))
                        .status(StatusCode::OK)
                        .body(Body::from(data))
                        .expect("valid response");
                }
                Err(_) => continue,
            }
        } else if full.is_dir() {
            let index = full.join("index.html");
            if index.is_file() {
                match tokio::fs::read(&index).await {
                    Ok(data) => {
                        return Response::builder()
                            .header(header::CONTENT_TYPE, "text/html")
                            .status(StatusCode::OK)
                            .body(Body::from(data))
                            .expect("valid response");
                    }
                    Err(_) => continue,
                }
            }
        }
    }

    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Body::from("Not Found"))
        .expect("valid response")
}

// ─── 服务器启动 ───

/// Create a new empty DataSourceRegistry for programmatic use
pub fn init_registry() -> Arc<DataSourceRegistry> {
    Arc::new(DataSourceRegistry::new())
}

/// Build the full axum Router (without binding or serving).
/// Useful for embedding in another application (e.g. Tauri).
pub fn build_router(
    registry: Arc<DataSourceRegistry>,
    root: Arc<String>,
    config: &ServerConfig,
) -> Router {
    let cache_arc = Arc::new(config.cache_max_age as i64);
    let allowed_paths = Arc::new(config.allowed_paths.clone());

    let all_dirs: Arc<Vec<String>> = Arc::new(
        std::iter::once(root.as_str().to_string())
            .chain(config.allowed_paths.clone())
            .collect(),
    );

    let mut app = Router::new()
        .fallback(fallback_handler)
        .layer(Extension(all_dirs));

    // ─── 健康检查 ───

    app = app.route("/health", get(health));

    // ─── Registry API ───

    app = app.route(
        "/api/sources",
        get({
            let reg = registry.clone();
            move || list_sources(reg.clone())
        }),
    );

    app = app.route(
        "/api/geo-files",
        get({
            let reg = registry.clone();
            move || list_geo_files(reg.clone())
        }),
    );

    app = app.route(
        "/api/mount",
        post({
            let reg = registry.clone();
            let root = root.clone();
            let ap = allowed_paths.clone();
            move |body| mount_source(reg.clone(), root.clone(), ap.clone(), body)
        }),
    );

    app = app.route(
        "/api/unmount/{name}",
        delete({
            let reg = registry.clone();
            move |path| unmount_source(reg.clone(), path)
        }),
    );

    // ─── 切片服务路由 ───

    app = app.route(
        "/api/tiles/{filename}/info",
        get({
            let reg = registry.clone();
            move |path| get_tile_info(reg.clone(), path)
        }),
    );

    app = app.route(
        "/api/tiles/{filename}/png/{z}/{x}/{y}",
        get({
            let reg = registry.clone();
            move |path, query| get_raster_tile(reg.clone(), path, query)
        }),
    );

    app = app.route(
        "/api/tiles/{filename}/geojson/{z}/{x}/{y}",
        get({
            let reg = registry.clone();
            move |path| get_vector_tile(reg.clone(), path)
        }),
    );

    app = app.route(
        "/api/tiles/{filename}/webp/{z}/{x}/{y}",
        get({
            let reg = registry.clone();
            move |path, query| get_raster_tile_webp(reg.clone(), path, query)
        }),
    );

    app = app.route(
        "/api/batch-tiles",
        post({
            let root = root.clone();
            let ap = allowed_paths.clone();
            move |body| batch_tiles(root.clone(), ap.clone(), body)
        }),
    );

    // ─── OGC 协议路由 ───

    app = app.route(
        "/ogc/wms",
        get({
            let reg = registry.clone();
            move |headers, query| protocols::wms_handler(reg.clone(), headers, query)
        }),
    );

    app = app.route(
        "/ogc/wmts/1.0.0/WMTSCapabilities.xml",
        get({
            let reg = registry.clone();
            move |headers: HeaderMap| protocols::wmts_capabilities(reg.clone(), headers)
        }),
    );

    app = app.route(
        "/ogc/wmts/1.0.0/{layer}/default/GoogleMapsCompatible/{z}/{x}/{y}",
        get({
            let reg = registry.clone();
            move |path| protocols::wmts_get_tile(reg.clone(), path)
        }),
    );

    app = app.route(
        "/ogc/tms/1.0.0/",
        get({
            let reg = registry.clone();
            move |headers: HeaderMap| protocols::tms_root(reg.clone(), headers)
        }),
    );

    app = app.route(
        "/ogc/tms/1.0.0/{layer}",
        get({
            let reg = registry.clone();
            move |headers: HeaderMap, path: Path<String>| {
                protocols::tms_layer(reg.clone(), headers, path)
            }
        }),
    );

    app = app.route(
        "/ogc/tms/1.0.0/{layer}/{z}/{x}/{y}",
        get({
            let reg = registry.clone();
            move |path| protocols::tms_get_tile(reg.clone(), path)
        }),
    );

    app = app.route(
        "/ogc/tilejson/{filename}",
        get({
            let reg = registry.clone();
            move |headers: HeaderMap, path: Path<String>| {
                protocols::tilejson(reg.clone(), headers, path)
            }
        }),
    );

    // ─── OpenAPI 文档 ───

    let filenames: Vec<String> = registry.list_names();
    let spec_value = build_dynamic_spec(&filenames);

    if let Some(paths) = spec_value.get("paths").and_then(|p| p.as_object()) {
        let mut file_routes: Vec<&str> = paths.keys().map(|k| k.as_str()).collect();
        file_routes.sort();
        for route in &file_routes {
            if *route != "/api/geo-files" {
                log::info!("  {}", route);
            }
        }
    }

    app = app.merge(
        SwaggerUi::new("/docs").external_url_unchecked("/api-docs/openapi.json", spec_value),
    );

    app = app.layer(middleware::from_fn(set_cache_header));
    app = app.layer(Extension(cache_arc));

    if config.no_dotfiles {
        app = app.layer(middleware::from_fn(filter_dotfiles));
    }

    if config.cors {
        app = app.layer(CorsLayer::permissive());
    }

    if config.gzip {
        app = app.layer(CompressionLayer::new());
    }

    app = app.layer(TraceLayer::new_for_http());

    app
}

pub fn run() {
    let _ = tracing_log::LogTracer::init();
    let cli = Cli::parse();

    match &cli.command {
        Some(Commands::Init) => {
            let path = PathBuf::from("config.yaml");
            match config::generate_default_config(&path) {
                Ok(()) => {
                    println!("Default config written to {}", path.display());
                }
                Err(e) => {
                    eprintln!("Error: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some(Commands::ExportOpenapi { output }) => {
            tracing_subscriber::fmt::init();

            let file_config = load_config_file(&cli.config).unwrap_or_default();
            let server_cfg = file_config.server.as_ref();
            let root_from_cli = merge_opt!(
                cli.root.clone(),
                server_cfg.and_then(|s| s.root.clone()),
                ".".to_string()
            );
            let root = cli.dir.clone().unwrap_or(root_from_cli);
            let allowed_paths = merge_opt!(
                cli.allow_path.clone(),
                server_cfg.and_then(|s| s.allowed_paths.clone()),
                vec![]
            );
            let mut all_paths = vec![root.clone()];
            all_paths.extend(allowed_paths);
            let registry = build_registry_from_config(&file_config, &all_paths);
            let filenames = registry.list_names();
            let spec_value = build_dynamic_spec(&filenames);

            match serde_json::to_string_pretty(&spec_value) {
                Ok(json) => match std::fs::write(output, json) {
                    Ok(()) => {
                        println!("OpenAPI spec written to {}", output.display());
                    }
                    Err(e) => {
                        eprintln!("Error writing {}: {}", output.display(), e);
                        std::process::exit(1);
                    }
                },
                Err(e) => {
                    eprintln!("Error serializing OpenAPI spec: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        None => {}
    }

    tracing_subscriber::fmt::init();

    let file_config = load_config_file(&cli.config).unwrap_or_default();
    let server_cfg = file_config.server.as_ref();
    let cache_cfg = file_config.cache.as_ref();

    let threads = merge_opt!(cli.threads, server_cfg.and_then(|s| s.threads), 4u32);
    let port = merge_opt!(cli.port, server_cfg.and_then(|s| s.port), 8080u16);
    let address = merge_opt!(
        cli.address,
        server_cfg.and_then(|s| s.address.clone()),
        "0.0.0.0".to_string()
    );
    let root_from_cli = merge_opt!(
        cli.root,
        server_cfg.and_then(|s| s.root.clone()),
        ".".to_string()
    );
    let root = cli.dir.clone().unwrap_or(root_from_cli);
    let cache_max_age = merge_opt!(
        cli.cache_max_age,
        server_cfg.and_then(|s| s.cache_max_age),
        3600i32
    );
    let cors = merge_opt!(cli.cors, server_cfg.and_then(|s| s.cors), false);
    let gzip = merge_opt!(cli.gzip, server_cfg.and_then(|s| s.gzip), false);
    let no_dotfiles = merge_opt!(
        cli.no_dotfiles,
        server_cfg.and_then(|s| s.no_dotfiles),
        false
    );
    let l2_cache_mb = merge_opt!(
        cli.l2_cache_mb,
        cache_cfg.and_then(|s| s.l2_size_mb),
        512u64
    );
    let allowed_paths = merge_opt!(
        cli.allow_path,
        server_cfg.and_then(|s| s.allowed_paths.clone()),
        vec![]
    );

    let server_config = ServerConfig {
        threads,
        cache_max_age,
        cors,
        gzip,
        no_dotfiles,
        l2_cache_mb,
        disk_cache_dir: cache_cfg.and_then(|s| s.disk_dir.clone()),
        allowed_paths: allowed_paths.clone(),
    };

    // Apply cache config before any tile operations
    tile_cache::set_l2_cache_size_mb(l2_cache_mb);
    if let Some(dir) = cache_cfg.and_then(|s| s.disk_dir.clone()) {
        tile_cache::set_disk_cache_dir(&dir);
    }

    let mut all_paths = vec![root.clone()];
    all_paths.extend(allowed_paths);
    let registry = build_registry_from_config(&file_config, &all_paths);

    let root_arc = Arc::new(root.clone());

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(threads.max(1) as usize)
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async move {
        let app = build_router(registry, root_arc, &server_config);

        let addr: SocketAddr = format!("{}:{}", address, port)
            .parse()
            .expect("Invalid address or port");

        let access_urls: Vec<String> = match addr.ip() {
            IpAddr::V4(ip) if ip == Ipv4Addr::UNSPECIFIED => vec![
                format!("http://127.0.0.1:{}", addr.port()),
                format!("http://localhost:{}", addr.port()),
            ],
            IpAddr::V6(ip) if ip == Ipv6Addr::UNSPECIFIED => vec![
                format!("http://127.0.0.1:{}", addr.port()),
                format!("http://localhost:{}", addr.port()),
                format!("http://[::1]:{}", addr.port()),
            ],
            IpAddr::V4(ip) if ip == Ipv4Addr::LOCALHOST => {
                vec![format!("http://127.0.0.1:{}", addr.port())]
            }
            IpAddr::V6(ip) if ip == Ipv6Addr::LOCALHOST => {
                vec![format!("http://[::1]:{}", addr.port())]
            }
            IpAddr::V4(ip) => vec![format!("http://{}:{}", ip, addr.port())],
            IpAddr::V6(ip) => vec![format!("http://[{}]:{}", ip, addr.port())],
        };

        log::info!("SimpleGeoServer listening on {}", addr);
        log::info!("Serving files from {}", root);
        for url in &access_urls {
            log::info!("Open in browser: {}", url);
            log::info!("API documentation: {}/docs", url);
        }

        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        axum::serve(listener, app).await.unwrap();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup() -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("allowed.tif");
        std::fs::write(&file_path, b"dummy").unwrap();
        (dir, file_path)
    }

    #[test]
    fn test_validate_path_rejects_parent_dir() {
        let (dir, _) = setup();
        let root = dir.path().to_str().unwrap();
        let result = validate_path(root, &[], "../etc/passwd");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(err.1, "Path traversal detected");
    }

    #[test]
    fn test_validate_path_rejects_absolute_path() {
        let (dir, _) = setup();
        let root = dir.path().to_str().unwrap();
        let result = validate_path(root, &[], "/etc/passwd");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(err.1, "Absolute path not allowed");
    }

    #[test]
    fn test_validate_path_rejects_nonexistent_file() {
        let (dir, _) = setup();
        let root = dir.path().to_str().unwrap();
        let result = validate_path(root, &[], "nonexistent.tif");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.0, StatusCode::NOT_FOUND);
    }

    #[test]
    fn test_validate_path_accepts_valid_file() {
        let (dir, file_path) = setup();
        let root = dir.path().to_str().unwrap();
        let filename = file_path.file_name().unwrap().to_str().unwrap();
        let result = validate_path(root, &[], filename);
        assert!(result.is_ok());
        let canonical = result.unwrap();
        assert!(canonical.starts_with(dir.path().canonicalize().unwrap()));
    }

    #[test]
    fn test_validate_path_rejects_deep_traversal() {
        let (dir, _) = setup();
        let root = dir.path().to_str().unwrap();
        let result = validate_path(root, &[], "subdir/../../etc/passwd");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(err.1, "Path traversal detected");
    }

    #[test]
    fn test_validate_path_accepts_subdir_valid_file() {
        let (dir, _) = setup();
        let subdir = dir.path().join("nested");
        std::fs::create_dir(&subdir).unwrap();
        let file_path = subdir.join("data.tif");
        std::fs::write(&file_path, b"dummy").unwrap();

        let root = dir.path().to_str().unwrap();
        let result = validate_path(root, &[], "nested/data.tif");
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_path_accepts_allowed_path_file() {
        let (dir, _) = setup();
        let outer_dir = TempDir::new().unwrap();
        let outer_file = outer_dir.path().join("external.tif");
        std::fs::write(&outer_file, b"dummy").unwrap();

        let root = dir.path().to_str().unwrap();
        let allowed = vec![outer_dir.path().to_string_lossy().to_string()];
        let filename = outer_file.file_name().unwrap().to_str().unwrap();
        let result = validate_path(root, &allowed, filename);
        assert!(result.is_ok());
        let canonical = result.unwrap();
        assert!(canonical.starts_with(&outer_dir.path().canonicalize().unwrap()));
    }

    #[test]
    fn test_validate_path_rejects_outside_allowed_paths() {
        let (dir, _) = setup();
        let outer_dir = TempDir::new().unwrap();
        let outer_file = outer_dir.path().join("external.tif");
        std::fs::write(&outer_file, b"dummy").unwrap();

        let root = dir.path().to_str().unwrap();
        // empty allowed_paths — should not find the outer file
        let filename = outer_file.file_name().unwrap().to_str().unwrap();
        let result = validate_path(root, &[], filename);
        assert!(result.is_err());
    }


}
