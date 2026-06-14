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

async fn get_tile_info(
    root: Arc<String>,
    Path(filename): Path<String>,
) -> Result<Json<tile::TileInfo>, (StatusCode, String)> {
    let filepath = std::path::Path::new(root.as_str()).join(&filename);
    if !filepath.exists() {
        return Err((StatusCode::NOT_FOUND, format!("File not found: {}", filename)));
    }
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

async fn get_raster_tile(
    root: Arc<String>,
    Path((filename, z, x, y)): Path<(String, u32, u32, u32)>,
) -> Result<Response, (StatusCode, String)> {
    let filepath = std::path::Path::new(root.as_str()).join(&filename);
    if !filepath.exists() {
        return Err((StatusCode::NOT_FOUND, format!("File not found: {}", filename)));
    }
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

async fn get_vector_tile(
    root: Arc<String>,
    Path((filename, z, x, y)): Path<(String, u32, u32, u32)>,
) -> Result<Response, (StatusCode, String)> {
    let filepath = std::path::Path::new(root.as_str()).join(&filename);
    if !filepath.exists() {
        return Err((StatusCode::NOT_FOUND, format!("File not found: {}", filename)));
    }
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

// ─── 服务器启动 ───

pub fn run() {
    let cli = Cli::parse();

    tracing_subscriber::fmt::init();

    let root = cli.root.clone();
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

        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        axum::serve(listener, app).await.unwrap();
    });
}
