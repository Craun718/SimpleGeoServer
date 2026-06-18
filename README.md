# SimpleGeoServer

English | [中文](README.zh.md)

SimpleGeoServer is a Rust HTTP server for static files and geospatial data. It serves raster and vector tiles, exposes OGC-compatible endpoints, and can be used either as a CLI app or embedded as a crate.

## Features

- Static file serving with automatic `index.html`
- Raster tile rendering from GeoTIFF as PNG
- Vector tile serving as GeoJSON
- OGC endpoints: WMS 1.3.0, WMTS 1.0.0, TMS 1.0.0, TileJSON 3.0.0
- Auto-scan and registry APIs for mounting geospatial sources
- Batch tile rendering API for offline or multi-tile workflows
- Swagger UI and dynamic OpenAPI output
- YAML configuration plus CLI overrides
- In-memory L2 tile cache with optional disk cache directory

## Supported Formats

| Format | Type | Notes |
|--------|------|-------|
| `.tif` / `.tiff` | Raster | GeoTIFF, including multi-band imagery |
| `.geojson` / `.json` | Vector | GeoJSON feature collections |
| `.shp` | Vector | Shapefile geometry source |
| `.wkt` | Vector | Well-known text geometry |
| `.kml` / `.kmz` | Vector | KML and zipped KMZ |

## Build

```bash
cargo build --release
```

## CLI Usage

```bash
simple-geo-server [OPTIONS] [DIR]
simple-geo-server init
simple-geo-server export-openapi --output openapi.json
```

`[DIR]` is the served root directory. If both `[DIR]` and `--root` are provided, `[DIR]` wins. If `--config` is used, CLI flags override values from the YAML file.

### Options

| Option | Default | Description |
|--------|---------|-------------|
| `-c, --config <PATH>` | - | Load settings from a YAML config file |
| `-t, --threads <THREADS>` | `4` | Worker thread count |
| `-f, --full-data <BOOL>` | - | Full-data mode flag exposed by the CLI |
| `-p, --port <PORT>` | `8080` | Listening port |
| `-a, --address <ADDRESS>` | `0.0.0.0` | Bind address |
| `-d, --root <ROOT>` | `.` | Root directory to serve |
| `[DIR]` | `.` | Positional root directory |
| `--cache-max-age <SECONDS>` | `3600` | `Cache-Control` max-age; negative disables cache headers |
| `--cors <BOOL>` | `false` | Enable permissive CORS |
| `-g, --gzip <BOOL>` | `false` | Enable Gzip compression |
| `--no-dotfiles <BOOL>` | `false` | Reject dotfiles and paths starting with `.` |
| `--log-format <FORMAT>` | `default` | Log output format |
| `--l2-cache-mb <SIZE_MB>` | `512` | In-memory L2 tile cache size in MB |

### Subcommands

| Subcommand | Description |
|------------|-------------|
| `init` | Generate a default `config.yaml` in the current directory |
| `export-openapi` | Export the current dynamic OpenAPI spec to JSON |

### Examples

```bash
# Serve the current directory
simple-geo-server

# Serve a specific folder on a custom port
simple-geo-server -p 3000 ./data

# Use a YAML config file
simple-geo-server --config ./config.yaml

# Generate the default config template
simple-geo-server init

# Export the resolved OpenAPI document
simple-geo-server export-openapi --output ./openapi.json
```

## Configuration File

Use `simple-geo-server init` to generate a starter file. The current template looks like this:

```yaml
server:
  port: 8080
  address: 0.0.0.0
  threads: 4
  root: .
  cache_max_age: 3600
  cors: false
  gzip: false
  no_dotfiles: false
  log_format: default
sources:
  - name: example-raster
    path: ./data/raster.tif
  - name: example-vector
    path: ./data/vector.geojson
cache:
  l2_size_mb: 512
  disk_dir: ./cache/tiles
```

`sources` pre-mounts named datasets into the registry. `cache.disk_dir` enables a disk-backed tile cache directory in addition to the in-memory cache.

## Core API

### Registry

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/api/sources` | List mounted data sources |
| `POST` | `/api/mount` | Mount a source dynamically |
| `DELETE` | `/api/unmount/{name}` | Unmount a source |

`POST /api/mount` request body:

```json
{
  "name": "example-raster",
  "path": "./data/raster.tif"
}
```

### File Discovery

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/api/geo-files` | List supported geospatial files with metadata |
| `GET` | `/api/tiles/{filename}/info` | Return source metadata such as CRS and extent |

### Tile Endpoints

| Method | Path | Output |
|--------|------|--------|
| `GET` | `/api/tiles/{filename}/png/{z}/{x}/{y}` | PNG raster tile |
| `GET` | `/api/tiles/{filename}/geojson/{z}/{x}/{y}` | GeoJSON vector tile |
| `POST` | `/api/batch-tiles` | JSON array of batch tile render results |

`POST /api/batch-tiles` request body:

```json
{
  "filename": "./data/raster.tif",
  "tiles": [
    { "z": 4, "x": 12, "y": 6 },
    { "z": 4, "x": 13, "y": 6 }
  ],
  "resampling": "bilinear",
  "stretch": "percentile",
  "bands": [1, 2, 3]
}
```

Supported `resampling` values include `nearest`, `bilinear`, `bicubic`, and `lanczos`. Supported `stretch` values include `min-max`, `percentile`, and `standard-deviation`.

### API Docs

- Swagger UI: `http://localhost:8080/docs`
- OpenAPI JSON: `http://localhost:8080/api-docs/openapi.json`

The generated OpenAPI document is dynamic: it expands file-based tile routes for currently available sources.

## OGC and Tile Metadata Endpoints

| Protocol | Endpoint | Notes |
|----------|----------|-------|
| WMS 1.3.0 | `/ogc/wms` | Supports standard `GetCapabilities` and `GetMap` style requests |
| WMTS 1.0.0 | `/ogc/wmts/1.0.0/WMTSCapabilities.xml` | Capability document |
| WMTS tile | `/ogc/wmts/1.0.0/{layer}/default/GoogleMapsCompatible/{z}/{x}/{y}` | GoogleMapsCompatible matrix |
| TMS 1.0.0 root | `/ogc/tms/1.0.0/` | Service listing |
| TMS 1.0.0 layer | `/ogc/tms/1.0.0/{layer}` | Layer metadata |
| TMS tile | `/ogc/tms/1.0.0/{layer}/{z}/{x}/{y}` | TMS Y-flipped tile path |
| TileJSON 3.0.0 | `/ogc/tilejson/{filename}` | Tile metadata for clients |

## Coordinate Reference Systems

- The server tile grid is Web Mercator (`EPSG:3857`).
- Source datasets are reprojected as needed during rendering.
- The `/api/tiles/{filename}/info` endpoint exposes original CRS and extent metadata.
- Vector tile responses are GeoJSON, while OGC routes expose the same data through protocol-specific shapes.

## License

Apache License 2.0
