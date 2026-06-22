use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use simple_geo_server::{ServerConfig, build_router, init_registry, validate_path};
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
        .oneshot(Request::builder().uri("/.env").body(Body::empty()).unwrap())
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
        .oneshot(Request::builder().uri("/.env").body(Body::empty()).unwrap())
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

// ─── Allowed Paths: static file serving ───

#[tokio::test]
async fn test_serve_dir_serves_file_from_allowed_path() {
    let root = TempDir::new().unwrap();
    let ext = TempDir::new().unwrap();
    std::fs::write(ext.path().join("ext.txt"), b"external-content").unwrap();

    let config = ServerConfig {
        allowed_paths: vec![ext.path().to_str().unwrap().to_string()],
        ..Default::default()
    };
    let app = build_router(
        init_registry(),
        Arc::new(root.path().to_str().unwrap().to_string()),
        &config,
    );

    let res = app
        .oneshot(
            Request::builder()
                .uri("/ext.txt")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_serve_dir_root_takes_priority() {
    let root = TempDir::new().unwrap();
    let ext = TempDir::new().unwrap();
    std::fs::write(root.path().join("conflict.txt"), b"root-content").unwrap();
    std::fs::write(ext.path().join("conflict.txt"), b"ext-content").unwrap();

    let config = ServerConfig {
        allowed_paths: vec![ext.path().to_str().unwrap().to_string()],
        ..Default::default()
    };
    let app = build_router(
        init_registry(),
        Arc::new(root.path().to_str().unwrap().to_string()),
        &config,
    );

    let res = app
        .oneshot(
            Request::builder()
                .uri("/conflict.txt")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    assert!(
        body.starts_with(b"root-"),
        "expected root priority, got: {body:?}"
    );
}

#[tokio::test]
async fn test_serve_dir_returns_404_for_missing_file() {
    let root = TempDir::new().unwrap();
    let ext = TempDir::new().unwrap();
    std::fs::write(ext.path().join("ext.txt"), b"ext").unwrap();

    let config = ServerConfig {
        allowed_paths: vec![ext.path().to_str().unwrap().to_string()],
        ..Default::default()
    };
    let app = build_router(
        init_registry(),
        Arc::new(root.path().to_str().unwrap().to_string()),
        &config,
    );

    let res = app
        .oneshot(
            Request::builder()
                .uri("/nonexistent.txt")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_serve_dir_traversal_still_blocked_with_allowed_paths() {
    let root = TempDir::new().unwrap();
    let ext = TempDir::new().unwrap();

    let config = ServerConfig {
        allowed_paths: vec![ext.path().to_str().unwrap().to_string()],
        ..Default::default()
    };
    let app = build_router(
        init_registry(),
        Arc::new(root.path().to_str().unwrap().to_string()),
        &config,
    );

    let res = app
        .oneshot(
            Request::builder()
                .uri("/../welcome.txt")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(res.status().is_client_error());
}

// ─── Allowed Paths: mount API ───

#[tokio::test]
async fn test_mount_accepts_file_in_allowed_path() {
    let root = TempDir::new().unwrap();
    let ext = TempDir::new().unwrap();
    std::fs::write(ext.path().join("ext.tif"), b"dummy-tif").unwrap();

    let config = ServerConfig {
        allowed_paths: vec![ext.path().to_str().unwrap().to_string()],
        ..Default::default()
    };
    let app = build_router(
        init_registry(),
        Arc::new(root.path().to_str().unwrap().to_string()),
        &config,
    );

    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/mount")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "name": "ext",
                        "path": "ext.tif"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_mount_rejects_file_outside_allowed_paths() {
    let root = TempDir::new().unwrap();
    let ext = TempDir::new().unwrap();
    std::fs::write(ext.path().join("ext.tif"), b"dummy-tif").unwrap();

    let app = build_router(
        init_registry(),
        Arc::new(root.path().to_str().unwrap().to_string()),
        &ServerConfig::default(),
    );

    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/mount")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "name": "ext",
                        "path": "ext.tif"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_allowed_path_traversal_mount_still_rejected() {
    let root = TempDir::new().unwrap();
    let ext = TempDir::new().unwrap();
    let sub = ext.path().join("nested");
    std::fs::create_dir(&sub).unwrap();
    std::fs::write(sub.join("ext.tif"), b"dummy-tif").unwrap();

    let config = ServerConfig {
        allowed_paths: vec![ext.path().to_str().unwrap().to_string()],
        ..Default::default()
    };
    let app = build_router(
        init_registry(),
        Arc::new(root.path().to_str().unwrap().to_string()),
        &config,
    );

    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/mount")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "name": "ext",
                        "path": "nested/../ext.tif"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

// ─── validate_path unit tests ───

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
