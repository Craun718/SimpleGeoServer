# SimpleGeoServer

[English](README.md) | 中文

一个基于 Rust 的简单 HTTP 静态文件服务器，内置地理空间切片服务，支持栅格（GeoTIFF）和矢量（GeoJSON）地图瓦片。

## 特性

- 静态文件服务，自动显示 `index.html`
- GeoTIFF 栅格瓦片渲染（PNG）
- GeoJSON 矢量瓦片服务
- 自动扫描并列出地理空间文件
- 可自定义缓存、CORS、Gzip 压缩
- 可过滤点文件（dotfiles）
- 动态 OpenAPI 文档 + Swagger UI
- 多线程异步运行时

## 安装

```bash
git clone https://github.com/Craun718/SimpleGeoServer.git
cd SimpleGeoServer
cargo build --release
```

## 用法

```bash
simple-geo-server [选项] [目录]
```

`[目录]` 为要服务的根目录，默认为当前目录（等价于 `http-server .`）。

### 选项

| 选项 | 默认值 | 说明 |
|------|--------|------|
| `-p, --port <PORT>` | `8080` | 监听端口 |
| `-a, --address <ADDRESS>` | `0.0.0.0` | 监听地址 |
| `-d, --root <ROOT>` | `.` | 服务根目录（同位置参数 `[目录]`） |
| `-t, --threads <THREADS>` | `4` | 工作线程数 |
| `--cache <CACHE>` | `3600` | Cache-Control max-age（秒），负数禁用缓存 |
| `--cors` | - | 启用宽松 CORS |
| `-g, --gzip` | - | 启用 Gzip 压缩 |
| `--no-dotfiles` | - | 拒绝以点开头的文件/路径 |
| `-f, --full-data` | - | 完整数据模式 |
| `--log-format <FORMAT>` | `default` | 日志格式 |
| `-h, --help` | - | 打印帮助信息 |

### 示例

```bash
# 服务当前目录
simple-geo-server

# 服务指定目录
simple-geo-server /path/to/geodata

# 指定端口和根目录
simple-geo-server -p 3000 -d ./maps

# 启用 CORS 和 Gzip
simple-geo-server --cors -g
```

## API 接口

### 文件列表

```
GET /api/geo-files
```

返回根目录下所有支持的地理空间文件列表（含元数据）。

### 文件信息

```
GET /api/tiles/{filename}/info
```

返回指定文件的瓦片信息（坐标系、范围、波段等）。

### 栅格瓦片

```
GET /api/tiles/{filename}/png/{z}/{x}/{y}
```

渲染并返回 GeoTIFF 的 PNG 瓦片。仅支持 `.tif` / `.tiff` 文件。

### 矢量瓦片

```
GET /api/tiles/{filename}/geojson/{z}/{x}/{y}
```

返回 GeoJSON FeatureCollection 格式的矢量瓦片。仅支持 `.geojson` / `.json` 文件。

### 交互文档

启动后访问 `http://localhost:8080/docs` 查看 Swagger UI。

## 支持的文件格式

| 格式 | 类型 | 说明 |
|------|------|------|
| `.tif` / `.tiff` | 栅格 | GeoTIFF 遥感影像，支持多波段 |
| `.geojson` / `.json` | 矢量 | GeoJSON 要素集合 |

## 许可证

Apache License 2.0
