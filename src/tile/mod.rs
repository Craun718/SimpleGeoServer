pub(crate) mod raster_cache;
pub(crate) mod raster_load;
pub(crate) mod raster_ovr;
pub(crate) mod raster_render;
pub(crate) mod tile_math;
pub(crate) mod types;
pub(crate) mod vector_tile;

pub use raster_cache::{
    RasterFileInfo, RasterLoadProgress, clear_raster_memory_cache, get_raster,
    load_and_cache_raster_with_progress, open_raster_metadata, raster_memory_cache_size_bytes,
};
pub use raster_load::{read_raster_region, read_raster_region_from_decoder, select_ifd_for_zoom};
pub use raster_ovr::{generate_ovr, parse_ovr_ifd_offsets};
pub(crate) use raster_render::render_raster_tile_cpu_rgba;
pub use raster_render::{
    approximate_tile_affine, render_map_bbox, render_raster_tile, render_raster_tile_cpu,
    render_raster_tile_ex, render_raster_tile_webp, render_single_tile,
};
pub use tile_math::{
    C, R, clamp_lat, mercator_to_lat, mercator_to_lng, tile_bounds_epsg3857, wgs84_tile_rect,
};
pub use types::{
    CachedRaster, GeoFileInfo, IfdInfo, InterleaveType, RasterBlock, RasterBlockConfig,
    RasterBlockIterator, TileInfo, TileRequest, VectorTileRequest,
};
pub use vector_tile::{
    get_kml_tile_geojson, get_raster_tile_info, get_shapefile_tile_geojson,
    get_vector_tile_geojson, get_vector_tile_info, get_wkt_tile_geojson,
};

pub use crate::raster::decode_result_to_f64_vec;
pub use crate::resample::{ResamplingMode, StretchConfig};
