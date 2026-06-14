use axum::{
    Extension,
    body::Body,
    http::{header, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    Router,
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

#[derive(Parser)]
#[command(name = "SimpleGeoServer", about = "A simple HTTP static file server")]
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

async fn filter_dotfiles(req: Request<Body>, next: Next) -> Response {
    let path = req.uri().path();
    if path.split('/').any(|s| !s.is_empty() && s.starts_with('.')) {
        return StatusCode::NOT_FOUND.into_response();
    }
    next.run(req).await
}

pub fn run() {
    let cli = Cli::parse();

    tracing_subscriber::fmt::init();

    let root = cli.root.clone();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(cli.threads.max(1) as usize)
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async move {
        let svc = ServeDir::new(&root).append_index_html_on_directories(true);

        let mut app = Router::new().fallback_service(svc);

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

        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        axum::serve(listener, app).await.unwrap();
    });
}
