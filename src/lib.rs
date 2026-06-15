use axum::{
    body::Body,
    extract::Path,
    http::{header, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Extension, Json, Router,
};
use clap::Parser;
use std::net::SocketAddr;
use std::sync::Arc;
use tower_http::{
    compression::CompressionLayer,
    cors::CorsLayer,
    services::ServeDir,
    trace::TraceLayer,
};
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;
use utoipauto::utoipauto;

mod raster;
mod reproject;
mod tile;

#[derive(Parser)]
#[command(name = "SimpleGeoServer", about = "A simple HTTP static file server with tile serving")]
pub struct Cli {
    #[arg(short, long, default_value_t = 4)]
    pub threads: u32,

    #[arg(short = 'f', long, default_value_t = false)]
    pub full_data: bool,

    #[arg(short, long, default_value_t = 8080)]
    pub port: u16,

    #[arg(short, long, default_value = "0.0.0.0")]
    pub address: String,

    #[arg(short = 'd', long, default_value = ".")]
    pub root: String,

    #[arg()]
    pub dir: Option<String>,

    #[arg(long, default_value_t = 3600)]
    pub cache: i32,

    #[arg(long, default_value_t = false)]
    pub cors: bool,

    #[arg(short, long, default_value_t = false)]
    pub gzip: bool,

    #[arg(long, default_value_t = false)]
    pub no_dotfiles: bool,

    #[arg(long, default_value = "default")]
    pub log_format: String,
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

async fn filter_dotfiles(
    req: Request<Body>,
    next: Next,
) -> Response {
    let path = req.uri().path();
    if path.split('/').any(|s| !s.is_empty() && s.starts_with('.')) {
        return StatusCode::NOT_FOUND.into_response();
    }
    next.run(req).await
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
async fn list_geo_files(root: Arc<String>) -> Json<Vec<tile::GeoFileInfo>> {
    let mut files = Vec::new();
    let dir = std::path::Path::new(root.as_str());
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.to_string())
                .unwrap_or_default();
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_lowercase())
                .unwrap_or_default();

            let info = match ext.as_str() {
                "tif" | "tiff" => {
                    let path_str = path.to_string_lossy().to_string();
                    match tile::get_raster_tile_info(&path_str) {
                        Ok(info) => Some(tile::GeoFileInfo {
                            name: name.clone(),
                            path: name.clone(),
                            data_type: "raster".to_string(),
                            info,
                        }),
                        Err(e) => {
                            tracing::warn!("Failed to read raster {}: {}", name, e);
                            None
                        }
                    }
                }
                "geojson" | "json" => {
                    Some(tile::GeoFileInfo {
                        name: name.clone(),
                        path: name.clone(),
                        data_type: "vector".to_string(),
                        info: tile::get_vector_tile_info(),
                    })
                }
                _ => None,
            };
            if let Some(f) = info {
                files.push(f);
            }
        }
    }
    Json(files)
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
    root: Arc<String>,
    Path(filename): Path<String>,
) -> Result<Json<tile::TileInfo>, (StatusCode, String)> {
    let filepath = validate_path(root.as_str(), &filename)?;
    let path_str = filepath.to_string_lossy().to_string();
    let ext = filepath
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default();

    match ext.as_str() {
        "tif" | "tiff" => {
            tile::get_raster_tile_info(&path_str)
                .map(Json)
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))
        }
        "geojson" | "json" => Ok(Json(tile::get_vector_tile_info())),
        _ => Err((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            format!("Unsupported format: .{}", ext),
        )),
    }
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
    root: Arc<String>,
    Path((filename, z, x, y)): Path<(String, u32, u32, u32)>,
) -> Result<Response, (StatusCode, String)> {
    let filepath = validate_path(root.as_str(), &filename)?;
    let path_str = filepath.to_string_lossy().to_string();

    let raster = tile::get_raster(&path_str)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    let (png_data, _rendered) = tile::render_raster_tile(&raster, z, x, y, 256, &[1, 2, 3])
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    Ok((
        [(header::CONTENT_TYPE, "image/png")],
        png_data,
    )
        .into_response())
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
    root: Arc<String>,
    Path((filename, z, x, y)): Path<(String, u32, u32, u32)>,
) -> Result<Response, (StatusCode, String)> {
    let filepath = validate_path(root.as_str(), &filename)?;
    let path_str = filepath.to_string_lossy().to_string();

    let req = tile::VectorTileRequest {
        path: path_str,
        z,
        x,
        y,
    };

    let geojson = tile::get_vector_tile_geojson(&req)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    Ok((
        [(header::CONTENT_TYPE, "application/geo+json")],
        geojson,
    )
        .into_response())
}

// ─── OpenAPI 文档 ───

#[utoipauto]
#[derive(OpenApi)]
#[openapi(
    info(
        title = "SimpleGeoServer API",
        description = "Geospatial file server with raster and vector tile serving",
        version = "0.1.0",
    ),
)]
struct ApiDoc;

fn validate_path(root: &str, filename: &str) -> Result<std::path::PathBuf, (StatusCode, String)> {
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

    let filepath = std::path::Path::new(root).join(filename);
    if !filepath.exists() {
        return Err((StatusCode::NOT_FOUND, format!("File not found: {}", filename)));
    }

    let root_canonical = std::path::Path::new(root)
        .canonicalize()
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Invalid root directory".into()))?;
    let file_canonical = filepath
        .canonicalize()
        .map_err(|_| (StatusCode::NOT_FOUND, format!("File not found: {}", filename)))?;
    if !file_canonical.starts_with(&root_canonical) {
        return Err((StatusCode::BAD_REQUEST, "Path traversal detected".into()));
    }

    Ok(file_canonical)
}

fn scan_geo_files(root: &str) -> Vec<String> {
    let dir = std::path::Path::new(root);
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_lowercase())
                .unwrap_or_default();
            if matches!(ext.as_str(), "tif" | "tiff" | "geojson" | "json") {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if name != "openapi.json" {
                        files.push(name.to_string());
                    }
                }
            }
        }
    }
    files.sort();
    files
}

fn is_raster_ext(ext: &str) -> bool {
    matches!(ext, "tif" | "tiff")
}

fn is_vector_ext(ext: &str) -> bool {
    matches!(ext, "geojson" | "json")
}

fn make_operation_id(base: &str, filename: &str) -> String {
    let safe = filename.replace(|c: char| !c.is_alphanumeric() && c != '_', "_");
    format!("{}_{}", base, safe)
}

fn build_dynamic_spec(root: &str) -> serde_json::Value {
    let geo_files = scan_geo_files(root);

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
            geo_files.iter().map(|f| serde_json::Value::String(f.clone())).collect(),
        );
    }

    if geo_files.is_empty() {
        return spec;
    }

    // Expand {filename} template paths into concrete per-file paths
    let Some(paths_obj) = spec.get_mut("paths").and_then(|p| p.as_object_mut()) else {
        return spec;
    };

    let template_paths: Vec<String> = paths_obj.keys().filter(|k| k.contains("{filename}")).cloned().collect();

    let mut concrete_paths: Vec<(String, serde_json::Value)> = Vec::new();

    for template_path in &template_paths {
        let Some(path_item) = paths_obj.get(template_path).cloned() else {
            continue;
        };

        for filename in &geo_files {
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
                        arr.retain(|p| p.get("name") != Some(&serde_json::Value::String("filename".to_string())));
                    }
                }

                // Set unique operationId
                let base_op_id = get_op.get("operationId").and_then(|v| v.as_str()).unwrap_or("operation");
                get_op["operationId"] = serde_json::Value::String(make_operation_id(base_op_id, filename));

                // Set meaningful summary
                let tag = if template_path.contains("/info") {
                    "info"
                } else if template_path.contains("/png/") {
                    "raster tile (PNG)"
                } else {
                    "vector tile (GeoJSON)"
                };
                get_op["summary"] = serde_json::Value::String(format!("Get {} for {}", tag, filename));
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

// ─── 服务器启动 ───

pub fn run() {
    let cli = Cli::parse();

    tracing_subscriber::fmt::init();

    let root = cli.dir.clone().unwrap_or(cli.root);
    let root_arc = Arc::new(root.clone());

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(cli.threads.max(1) as usize)
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async move {
        let svc = ServeDir::new(&root).append_index_html_on_directories(true);

        let mut app = Router::new()
            .fallback_service(svc);

        // 切片服务路由
        app = app.route("/api/geo-files", get({
            let root = root_arc.clone();
            move || list_geo_files(root.clone())
        }));

        app = app.route(
            "/api/tiles/{filename}/info",
            get({
                let root = root_arc.clone();
                move |path| get_tile_info(root.clone(), path)
            }),
        );

        app = app.route(
            "/api/tiles/{filename}/png/{z}/{x}/{y}",
            get({
                let root = root_arc.clone();
                move |path| get_raster_tile(root.clone(), path)
            }),
        );

        app = app.route(
            "/api/tiles/{filename}/geojson/{z}/{x}/{y}",
            get({
                let root = root_arc.clone();
                move |path| get_vector_tile(root.clone(), path)
            }),
        );

        // OpenAPI 文档（动态生成，包含当前目录文件列表）
        let spec_value = build_dynamic_spec(&root);
        if let Ok(json) = serde_json::to_string_pretty(&spec_value) {
            if let Err(e) = std::fs::write("openapi.json", &json) {
                tracing::warn!("Failed to write openapi.json: {}", e);
            }
        }

        // 打印所有展开的文件路由
        if let Some(paths) = spec_value.get("paths").and_then(|p| p.as_object()) {
            let mut file_routes: Vec<&str> = paths.keys().map(|k| k.as_str()).collect();
            file_routes.sort();
            for route in &file_routes {
                if *route != "/api/geo-files" {
                    tracing::info!("  {}", route);
                }
            }
        }

        app = app.merge(
            SwaggerUi::new("/docs").external_url_unchecked("/api-docs/openapi.json", spec_value),
        );

        let cache_arc = Arc::new(cli.cache as i64);
        app = app.layer(middleware::from_fn(set_cache_header));
        app = app.layer(Extension(cache_arc));

        if cli.no_dotfiles {
            app = app.layer(middleware::from_fn(filter_dotfiles));
        }

        if cli.cors {
            app = app.layer(CorsLayer::permissive());
        }

        if cli.gzip {
            app = app.layer(CompressionLayer::new());
        }

        app = app.layer(TraceLayer::new_for_http());

        let addr: SocketAddr = format!("{}:{}", cli.address, cli.port)
            .parse()
            .expect("Invalid address or port");

        tracing::info!("SimpleGeoServer started on http://{}", addr);
        tracing::info!("Serving files from {}", root);
        tracing::info!("Geo tile API available at http://{}/api/geo-files", addr);
        tracing::info!("API documentation at http://{}/docs", addr);

        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        axum::serve(listener, app).await.unwrap();
    });
}
