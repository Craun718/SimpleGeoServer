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

## Supported File Formats

| Format | Type | Description |
|--------|------|-------------|
| `.tif` / `.tiff` | Raster | GeoTIFF remote sensing imagery, multi-band support |
| `.geojson` / `.json` | Vector | GeoJSON feature collections |

## License

Apache License 2.0
