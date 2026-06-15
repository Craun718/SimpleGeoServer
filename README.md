# SimpleGeoServer

English | [中文](README.zh.md)

A simple HTTP static file server with geospatial tile serving, built in Rust. Supports raster (GeoTIFF) and vector (GeoJSON) map tiles.

## Features

- Static file server with automatic `index.html` support
- GeoTIFF raster tile rendering (PNG)
- GeoJSON vector tile serving
- Auto-scan and list geospatial files
- Customizable cache, CORS, and Gzip compression
- Dotfile filtering
- Dynamic OpenAPI documentation + Swagger UI
- Multi-threaded async runtime

## Installation

```bash
git clone https://github.com/Craun718/SimpleGeoServer.git
cd SimpleGeoServer
cargo build --release
```

## Usage

```bash
simple-geo-server [options] [directory]
```

`[directory]` is the root directory to serve (defaults to current directory, like `http-server .`).

### Options

| Option | Default | Description |
|--------|---------|-------------|
| `-p, --port <PORT>` | `8080` | Port to listen on |
| `-a, --address <ADDRESS>` | `0.0.0.0` | Address to bind to |
| `-d, --root <ROOT>` | `.` | Root directory (same as positional arg `[directory]`) |
| `-t, --threads <THREADS>` | `4` | Number of worker threads |
| `--cache <CACHE>` | `3600` | Cache-Control max-age in seconds; negative to disable |
| `--cors` | - | Enable permissive CORS |
| `-g, --gzip` | - | Enable Gzip compression |
| `--no-dotfiles` | - | Reject dotfiles / paths starting with `.` |
| `-f, --full-data` | - | Full data mode |
| `--log-format <FORMAT>` | `default` | Log format |
| `-h, --help` | - | Print help |

### Examples

```bash
# Serve current directory
simple-geo-server

# Serve a specific directory
simple-geo-server /path/to/geodata

# Specify port and root
simple-geo-server -p 3000 -d ./maps

# Enable CORS and Gzip
simple-geo-server --cors -g
```

## API Endpoints

### List files

```
GET /api/geo-files
```

Returns all supported geospatial files in the root directory with metadata.

### File info

```
GET /api/tiles/{filename}/info
```

Returns tile metadata (CRS, extent, bands, etc.) for a given file.

### Raster tile

```
GET /api/tiles/{filename}/png/{z}/{x}/{y}
```

Renders and returns a GeoTIFF raster tile as PNG. Supports `.tif` / `.tiff` files only.

### Vector tile

```
GET /api/tiles/{filename}/geojson/{z}/{x}/{y}
```

Returns a vector tile as GeoJSON FeatureCollection. Supports `.geojson` / `.json` files only.

### Interactive docs

Visit `http://localhost:8080/docs` after starting the server to use Swagger UI.

## Coordinate System

- The tile grid is always **Web Mercator (EPSG:3857)** — all tiles are rendered and served in this projection.
- Source data with a different CRS is **automatically reprojected** to EPSG:3857 on the fly (no manual conversion needed).
- Supported source CRS: WGS84 (EPSG:4326), Web Mercator (EPSG:3857), UTM WGS84 (zones 1–60, N/S).
- Vector tiles: GeoJSON features are reprojected to WGS84 for bounding-box filtering, then served in their original CRS.
- The `/api/tiles/{filename}/info` endpoint returns the original CRS and WGS84 extent of the source file.

## Supported Tile Protocols

| Protocol | Endpoint | Tile Scheme | Output Format |
|----------|----------|-------------|---------------|
| Internal REST API (XYZ) | `/api/tiles/{filename}/png/{z}/{x}/{y}` | Slippy Map (XYZ) | PNG / GeoJSON |
| OGC WMS 1.3.0 | `/ogc/wms?SERVICE=WMS&...` | Arbitrary BBOX | PNG / GeoJSON |
| OGC WMTS 1.0.0 | `/ogc/wmts/1.0.0/{layer}/default/GoogleMapsCompatible/{z}/{x}/{y}` | GoogleMapsCompatible (XYZ) | PNG |
| OGC TMS 1.0.0 | `/ogc/tms/1.0.0/{layer}/{z}/{x}/{y}` | TMS (Y flipped) | PNG |
| TileJSON 3.0.0 | `/ogc/tilejson/{filename}` | XYZ metadata | JSON |

All OGC tile protocols are implemented in src/protocols.rs. WMS and WMTS follow the OGC 1.3.0 and 1.0.0 standards respectively. TMS follows the OSGeo TMS 1.0.0 specification. TileJSON provides tile metadata in the standard 3.0.0 format.

## Supported File Formats

| Format | Type | Description |
|--------|------|-------------|
| `.tif` / `.tiff` | Raster | GeoTIFF remote sensing imagery, multi-band support |
| `.geojson` / `.json` | Vector | GeoJSON feature collections |

## License

Apache License 2.0
