# SimpleGeoServer

[English](README.md) | 中文

SimpleGeoServer 是一个基于 Rust 的 HTTP 服务器，用于同时提供静态文件和地理空间数据服务。它支持栅格与矢量瓦片、OGC 兼容接口，也可以既作为 CLI 程序运行，也可作为 crate 嵌入使用。

## 特性

- 静态文件服务，自动返回 `index.html`
- 从 GeoTIFF 渲染 PNG 栅格瓦片
- 以 GeoJSON 形式提供矢量瓦片
- OGC 接口支持：WMS 1.3.0、WMTS 1.0.0、TMS 1.0.0、TileJSON 3.0.0
- 自动扫描地理空间文件，并提供数据源挂载注册表接口
- 提供批量瓦片渲染接口，适合离线或多瓦片任务
- 内置 Swagger UI 和动态 OpenAPI 文档
- 支持 YAML 配置，并允许 CLI 参数覆盖
- 提供内存 L2 瓦片缓存，并可选开启磁盘缓存目录

## 支持格式

| 格式 | 类型 | 说明 |
|------|------|------|
| `.tif` / `.tiff` | 栅格 | GeoTIFF，支持多波段影像 |
| `.geojson` / `.json` | 矢量 | GeoJSON 要素集合 |
| `.shp` | 矢量 | Shapefile 几何数据 |
| `.wkt` | 矢量 | WKT 文本几何 |
| `.kml` / `.kmz` | 矢量 | KML 与压缩后的 KMZ |

## 构建

```bash
cargo build --release
```

## CLI 用法

```bash
simple-geo-server [OPTIONS] [DIR]
simple-geo-server init
simple-geo-server export-openapi --output openapi.json
```

`[DIR]` 表示服务根目录。如果同时提供 `[DIR]` 和 `--root`，则以 `[DIR]` 为准。如果使用 `--config`，则 CLI 参数会覆盖 YAML 文件中的对应配置。

### 选项

| 选项 | 默认值 | 说明 |
|------|--------|------|
| `-c, --config <PATH>` | - | 从 YAML 配置文件加载设置 |
| `-t, --threads <THREADS>` | `4` | 工作线程数 |
| `-f, --full-data <BOOL>` | - | CLI 暴露的完整数据模式开关 |
| `-p, --port <PORT>` | `8080` | 监听端口 |
| `-a, --address <ADDRESS>` | `0.0.0.0` | 绑定地址 |
| `-d, --root <ROOT>` | `.` | 服务根目录 |
| `[DIR]` | `.` | 位置参数形式的根目录 |
| `--cache-max-age <SECONDS>` | `3600` | `Cache-Control` 的 `max-age` 秒数；负数表示不发送缓存头 |
| `--cors <BOOL>` | `false` | 是否启用宽松 CORS |
| `-g, --gzip <BOOL>` | `false` | 是否启用 Gzip 压缩 |
| `--no-dotfiles <BOOL>` | `false` | 是否拒绝访问点文件和以 `.` 开头的路径 |
| `--log-format <FORMAT>` | `default` | 日志输出格式 |
| `--l2-cache-mb <SIZE_MB>` | `512` | 内存 L2 瓦片缓存大小（MB） |

### 子命令

| 子命令 | 说明 |
|--------|------|
| `init` | 在当前目录生成默认 `config.yaml` |
| `export-openapi` | 将当前动态 OpenAPI 文档导出为 JSON |

### 示例

```bash
# 服务当前目录
simple-geo-server

# 在自定义端口上服务指定目录
simple-geo-server -p 3000 ./data

# 使用 YAML 配置文件
simple-geo-server --config ./config.yaml

# 生成默认配置模板
simple-geo-server init

# 导出解析后的 OpenAPI 文档
simple-geo-server export-openapi --output ./openapi.json
```

## 配置文件

可以通过 `simple-geo-server init` 生成初始配置。当前默认模板如下：

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

`sources` 用于在启动时预挂载具名数据集。`cache.disk_dir` 会在内存缓存之外启用磁盘瓦片缓存目录。

## 核心 API

### 注册表接口

| 方法 | 路径 | 说明 |
|------|------|------|
| `GET` | `/api/sources` | 列出当前已挂载的数据源 |
| `POST` | `/api/mount` | 动态挂载一个数据源 |
| `DELETE` | `/api/unmount/{name}` | 卸载指定数据源 |

`POST /api/mount` 请求体：

```json
{
  "name": "example-raster",
  "path": "./data/raster.tif"
}
```

### 文件发现接口

| 方法 | 路径 | 说明 |
|------|------|------|
| `GET` | `/api/geo-files` | 返回所有受支持的地理空间文件及其元数据 |
| `GET` | `/api/tiles/{filename}/info` | 返回数据源元信息，例如 CRS 和范围 |

### 瓦片接口

| 方法 | 路径 | 输出 |
|------|------|------|
| `GET` | `/api/tiles/{filename}/png/{z}/{x}/{y}` | PNG 栅格瓦片 |
| `GET` | `/api/tiles/{filename}/geojson/{z}/{x}/{y}` | GeoJSON 矢量瓦片 |
| `POST` | `/api/batch-tiles` | 批量渲染结果的 JSON 数组 |

`POST /api/batch-tiles` 请求体：

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

`resampling` 目前支持 `nearest`、`bilinear`、`bicubic`、`lanczos`。`stretch` 目前支持 `min-max`、`percentile`、`standard-deviation`。

### API 文档

- Swagger UI: `http://localhost:8080/docs`
- OpenAPI JSON: `http://localhost:8080/api-docs/openapi.json`

生成的 OpenAPI 文档是动态的，会按当前可用数据源展开具体的文件级瓦片路径。

## OGC 与瓦片元数据接口

| 协议 | 路径 | 说明 |
|------|------|------|
| WMS 1.3.0 | `/ogc/wms` | 支持标准 `GetCapabilities`、`GetMap` 等请求 |
| WMTS 1.0.0 | `/ogc/wmts/1.0.0/WMTSCapabilities.xml` | 能力描述文档 |
| WMTS 瓦片 | `/ogc/wmts/1.0.0/{layer}/default/GoogleMapsCompatible/{z}/{x}/{y}` | GoogleMapsCompatible 矩阵集 |
| TMS 1.0.0 根接口 | `/ogc/tms/1.0.0/` | 服务列表 |
| TMS 1.0.0 图层接口 | `/ogc/tms/1.0.0/{layer}` | 图层元信息 |
| TMS 瓦片 | `/ogc/tms/1.0.0/{layer}/{z}/{x}/{y}` | TMS 规范的 Y 轴翻转路径 |
| TileJSON 3.0.0 | `/ogc/tilejson/{filename}` | 提供客户端可消费的瓦片元数据 |

## 坐标参考系

- 服务器瓦片网格使用 Web Mercator（`EPSG:3857`）。
- 渲染时会按需对源数据进行重投影。
- `/api/tiles/{filename}/info` 会返回原始 CRS 和范围等元数据。
- 矢量瓦片接口输出 GeoJSON，OGC 接口则以各自协议要求的形式暴露相同数据。

## 许可证

Apache License 2.0
