use std::f64::consts::PI;

pub const R: f64 = 6378137.0;
pub const C: f64 = R * PI;

pub fn tile_bounds_epsg3857(z: u32, x: u32, y: u32, tile_size: u32) -> (f64, f64, f64, f64) {
    let n = (1u64 << z) as f64;
    let res = 2.0 * C / (tile_size as f64 * n);
    let min_x = -C + x as f64 * tile_size as f64 * res;
    let max_x = -C + (x as f64 + 1.0) * tile_size as f64 * res;
    let max_y = C - y as f64 * tile_size as f64 * res;
    let min_y = C - (y as f64 + 1.0) * tile_size as f64 * res;
    (min_x, min_y, max_x, max_y)
}

pub fn mercator_to_lng(merc_x: f64) -> f64 {
    merc_x * 180.0 / C
}

pub fn mercator_to_lat(merc_y: f64) -> f64 {
    let val = (merc_y / R).exp();
    let lat_rad = 2.0 * (val.atan() - std::f64::consts::FRAC_PI_4);
    lat_rad.to_degrees()
}

pub fn clamp_lat(lat: f64) -> f64 {
    const MAX_LAT: f64 = 85.051129;
    lat.clamp(-MAX_LAT, MAX_LAT)
}

pub fn wgs84_tile_rect(z: u32, x: u32, y: u32) -> geo_types::Rect<f64> {
    let size = 256;
    let (min_x, min_y, max_x, max_y) = tile_bounds_epsg3857(z, x, y, size);

    let min_lng = mercator_to_lng(min_x);
    let max_lng = mercator_to_lng(max_x);
    let min_lat = clamp_lat(mercator_to_lat(min_y));
    let max_lat = clamp_lat(mercator_to_lat(max_y));

    geo_types::Rect::new(
        geo_types::coord! { x: min_lng, y: min_lat },
        geo_types::coord! { x: max_lng, y: max_lat },
    )
}
