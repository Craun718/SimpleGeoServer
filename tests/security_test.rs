use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use simple_geo_server::{build_router, init_registry, ServerConfig};
use std::sync::Arc;
use tempfile::TempDir;
use tower::ServiceExt;

fn setup_server(with_dotfile_filter: bool) -> (TempDir, axum::Router) {
    let dir = TempDir::new().unwrap();

    let allowed = dir.path().join("welcome.txt");
    std::fs::write(&allowed, b"hello").unwrap();

    let dotfile = dir.path().join(".env");
    std::fs::write(&dotfile, b"secret").unwrap();

    let sub = dir.path().join("subdir");
    std::fs::create_dir(&sub).unwrap();
    std::fs::write(sub.join("data.txt"), b"data").unwrap();

    let registry = init_registry();
    let root = Arc::new(dir.path().to_str().unwrap().to_string());
    let config = ServerConfig {
        no_dotfiles: with_dotfile_filter,
        ..Default::default()
    };
    let app = build_router(registry, root, &config);
    (dir, app)
}

// ─── ServeDir: normal file access ───

#[tokio::test]
async fn test_serve_dir_serves_normal_file() {
    let (_dir, app) = setup_server(false);
    let res = app
        .oneshot(
            Request::builder()
                .uri("/welcome.txt")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

// ─── ServeDir: path traversal blocking ───

#[tokio::test]
async fn test_serve_dir_rejects_parent_dir_traversal() {
    let (_dir, app) = setup_server(false);
    let res = app
        .oneshot(
            Request::builder()
                .uri("/../etc/passwd")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    // ServeDir in tower-http returns 400 for paths with `..` segments
    assert!(res.status().is_client_error());
}

#[tokio::test]
async fn test_serve_dir_rejects_url_encoded_traversal() {
    let (_dir, app) = setup_server(false);
    let res = app
        .oneshot(
            Request::builder()
                .uri("/%2e%2e/etc/passwd")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(res.status().is_client_error());
}

#[tokio::test]
async fn test_serve_dir_rejects_deep_traversal() {
    let (_dir, app) = setup_server(false);
    let res = app
        .oneshot(
            Request::builder()
                .uri("/subdir/../../etc/passwd")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(res.status().is_client_error());
}

// ─── Dotfile filter (no_dotfiles = true) ───

#[tokio::test]
async fn test_dotfile_filter_blocks_dotfiles() {
    let (_dir, app) = setup_server(true);
    let res = app
        .oneshot(
            Request::builder()
                .uri("/.env")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_dotfile_filter_allows_normal_files() {
    let (_dir, app) = setup_server(true);
    let res = app
        .oneshot(
            Request::builder()
                .uri("/welcome.txt")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_dotfile_filter_disabled_serves_dotfiles() {
    let (_dir, app) = setup_server(false);
    let res = app
        .oneshot(
            Request::builder()
                .uri("/.env")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    // `.env` exists in the temp dir; without the filter it should be served
    assert_eq!(res.status(), StatusCode::OK);
}

// ─── Mount API: direct validate_path call ───

#[tokio::test]
async fn test_mount_rejects_path_traversal() {
    let (_dir, app) = setup_server(false);
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/mount")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "name": "evil",
                        "path": "../../etc/passwd"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_mount_rejects_absolute_path() {
    let (_dir, app) = setup_server(false);
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/mount")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "name": "evil",
                        "path": "/etc/passwd"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

// ─── Tile API: path traversal in URI falls to ServeDir ───
//   `../../etc/passwd` has extra segments so the route doesn't match;
//   the request falls through to ServeDir which blocks `..`.

#[tokio::test]
async fn test_tile_info_falls_to_serve_dir_on_traversal() {
    let (_dir, app) = setup_server(false);
    let res = app
        .oneshot(
            Request::builder()
                .uri("/api/tiles/../../etc/passwd/info")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(res.status().is_client_error());
}

#[tokio::test]
async fn test_tile_png_falls_to_serve_dir_on_traversal() {
    let (_dir, app) = setup_server(false);
    let res = app
        .oneshot(
            Request::builder()
                .uri("/api/tiles/../../etc/passwd/png/0/0/0")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(res.status().is_client_error());
}

// ─── Tile API: URL-encoded traversal hits the route, rejected by registry ───
//   `%2e%2e` stays within one segment so the route matches with filename="..";
//   registry.get("..") returns None → 404.

#[tokio::test]
async fn test_tile_info_encoded_traversal_rejected_by_registry() {
    let (_dir, app) = setup_server(false);
    let res = app
        .oneshot(
            Request::builder()
                .uri("/api/tiles/%2e%2e/info")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_tile_png_encoded_traversal_rejected_by_registry() {
    let (_dir, app) = setup_server(false);
    let res = app
        .oneshot(
            Request::builder()
                .uri("/api/tiles/%2e%2e/png/0/0/0")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

// ─── OGC routes: same pattern — raw traversal falls to ServeDir ───

#[tokio::test]
async fn test_ogc_wmts_traversal_falls_to_serve_dir() {
    let (_dir, app) = setup_server(false);
    let res = app
        .oneshot(
            Request::builder()
                .uri("/ogc/wmts/1.0.0/../../etc/passwd/default/GoogleMapsCompatible/0/0/0")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(res.status().is_client_error());
}

#[tokio::test]
async fn test_ogc_tms_traversal_falls_to_serve_dir() {
    let (_dir, app) = setup_server(false);
    let res = app
        .oneshot(
            Request::builder()
                .uri("/ogc/tms/1.0.0/../../etc/passwd/0/0/0")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(res.status().is_client_error());
}

#[tokio::test]
async fn test_tilejson_traversal_falls_to_serve_dir() {
    let (_dir, app) = setup_server(false);
    let res = app
        .oneshot(
            Request::builder()
                .uri("/ogc/tilejson/../../etc/passwd")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(res.status().is_client_error());
}

// ─── OGC routes: URL-encoded traversal hits the route, rejected by registry ───

#[tokio::test]
async fn test_ogc_tilejson_encoded_traversal_rejected_by_registry() {
    let (_dir, app) = setup_server(false);
    let res = app
        .oneshot(
            Request::builder()
                .uri("/ogc/tilejson/%2e%2e")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}
