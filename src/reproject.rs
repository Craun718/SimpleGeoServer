use geo::{Coord, Geometry, MapCoords};
use std::io::{Read, Seek};
use tiff::decoder::Decoder;
use tiff::tags::Tag;
use tiff::TiffResult;

const WGS84_A: f64 = 6378137.0;
const WGS84_F: f64 = 1.0 / 298.257223563;
const WGS84_E2: f64 = 2.0 * WGS84_F - WGS84_F * WGS84_F;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnownCrs {
    Wgs84,
    WebMercator,
    UtmWgs84 { zone: u8, northern: bool },
}

#[derive(Debug, Clone)]
pub struct GeoKeyInfo {
    pub model_type: Option<u16>,
    pub projected_type: Option<u16>,
    pub geographic_type: Option<u16>,
    pub proj_nat_origin_long: Option<f64>,
    pub proj_nat_origin_lat: Option<f64>,
    pub proj_false_easting: Option<f64>,
    pub proj_false_northing: Option<f64>,
    pub proj_scale_at_nat_origin: Option<f64>,
    pub proj_coord_trans: Option<u16>,
}

impl Default for GeoKeyInfo {
    fn default() -> Self {
        Self {
            model_type: None,
            projected_type: None,
            geographic_type: None,
            proj_nat_origin_long: None,
            proj_nat_origin_lat: None,
            proj_false_easting: None,
            proj_false_northing: None,
            proj_scale_at_nat_origin: None,
            proj_coord_trans: None,
        }
    }
}

pub fn read_geo_key_info<R: Read + Seek>(decoder: &mut Decoder<R>) -> TiffResult<GeoKeyInfo> {
    let dir_data: Vec<u16> = match decoder.find_tag(Tag::GeoKeyDirectoryTag)? {
        Some(v) => v.into_u16_vec()?,
        None => return Ok(GeoKeyInfo::default()),
    };
    if dir_data.len() < 4 {
        return Ok(GeoKeyInfo::default());
    }
    let num_keys = dir_data[3] as usize;
    let expected = 4 + 4 * num_keys;
    if dir_data.len() < expected {
        return Ok(GeoKeyInfo::default());
    }

    let mut info = GeoKeyInfo::default();
    for i in 0..num_keys {
        let offset = 4 + 4 * i;
        let key_id = dir_data[offset];
        let tiff_tag_location = dir_data[offset + 1];
        let count = dir_data[offset + 2] as usize;
        let value_or_offset = dir_data[offset + 3];

        match key_id {
            1024 => info.model_type = Some(value_or_offset),
            3072 => info.projected_type = Some(value_or_offset),
            2048 => info.geographic_type = Some(value_or_offset),
            3076 => {
                if count > 0 && tiff_tag_location == 0 {
                    info.proj_nat_origin_long =
                        Some(value_or_offset as i16 as f64 * 1e-6);
                } else if tiff_tag_location != 0 {
                    info.proj_nat_origin_long =
                        Some(value_or_offset as i16 as f64 * 1e-6);
                }
            }
            3075 => {
                if count > 0 && tiff_tag_location == 0 {
                    info.proj_nat_origin_lat =
                        Some(value_or_offset as i16 as f64 * 1e-6);
                }
            }
            3082 => {
                info.proj_false_easting = Some(value_or_offset as i16 as f64);
            }
            3083 => {
                info.proj_false_northing = Some(value_or_offset as i16 as f64);
            }
            3092 => {
                info.proj_scale_at_nat_origin =
                    Some(value_or_offset as i16 as f64 * 1e-6);
            }
            3079 => info.proj_coord_trans = Some(value_or_offset),
            _ => {}
        }
    }
    Ok(info)
}

pub fn parse_known_crs(name: &str) -> Option<KnownCrs> {
    let upper = name.to_uppercase();

    if upper.contains("WGS 84")
        || upper.contains("WGS84")
        || upper.contains("GCS_WGS_84")
        || upper.contains("EPSG:4326")
        || upper.contains("EPSG:4269")
        || upper.contains("NAD83")
        || upper.contains("NAD 83")
    {
        if upper.contains("UTM") || upper.contains("ZONE") {
            return None;
        } else {
            return Some(KnownCrs::Wgs84);
        }
    }

    if upper.contains("WEB MERCATOR")
        || upper.contains("WEBMERCATOR")
        || upper.contains("MERCATOR")
        || upper.contains("EPSG:3857")
        || upper.contains("EPSG:900913")
        || upper.contains("EPSG:102100")
    {
        return Some(KnownCrs::WebMercator);
    }

    if upper.contains("UTM") || upper.contains("ZONE") {
        let zone = extract_utm_zone(&upper)?;
        let northern = !upper.contains("SOUTH");
        return Some(KnownCrs::UtmWgs84 { zone, northern });
    }

    if upper.contains("EPSG:326") {
        let code_str = upper
            .split(|c: char| !c.is_ascii_digit())
            .filter(|s| !s.is_empty())
            .next()?;
        let code: u16 = code_str.parse().ok()?;
        let zone = (code - 32600) as u8;
        if (1..=60).contains(&zone) {
            return Some(KnownCrs::UtmWgs84 {
                zone,
                northern: true,
            });
        }
    }
    if upper.contains("EPSG:327") {
        let code_str = upper
            .split(|c: char| !c.is_ascii_digit())
            .filter(|s| !s.is_empty())
            .next()?;
        let code: u16 = code_str.parse().ok()?;
        let zone = (code - 32700) as u8;
        if (1..=60).contains(&zone) {
            return Some(KnownCrs::UtmWgs84 {
                zone,
                northern: false,
            });
        }
    }

    None
}

fn extract_utm_zone(upper: &str) -> Option<u8> {
    let zone_keywords = ["ZONE", "UTM ZONE", "UTM_ZONE", "ZONE "];
    for kw in &zone_keywords {
        if let Some(pos) = upper.find(kw) {
            let after = &upper[pos + kw.len()..];
            let num_str: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(zone) = num_str.parse::<u8>() {
                if (1..=60).contains(&zone) {
                    return Some(zone);
                }
            }
        }
    }
    None
}

pub fn known_crs_coord_to_wgs84(x: f64, y: f64, crs: KnownCrs) -> Option<(f64, f64)> {
    match crs {
        KnownCrs::Wgs84 => Some((x, y)),
        KnownCrs::WebMercator => {
            let lng = x * 180.0 / (WGS84_A * std::f64::consts::PI);
            let val = (y / WGS84_A).exp();
            let lat_rad = 2.0 * val.atan() - std::f64::consts::FRAC_PI_2;
            let lat = lat_rad.to_degrees();
            Some((lng, lat))
        }
        KnownCrs::UtmWgs84 { zone, northern } => {
            utm_to_wgs84(x, y, zone, northern)
        }
    }
}

pub fn known_crs_geometry_to_wgs84(
    geometry: &Geometry<f64>,
    crs: KnownCrs,
) -> Option<Geometry<f64>> {
    match crs {
        KnownCrs::Wgs84 => Some(geometry.clone()),
        _ => Some(geometry.map_coords(|coord| {
            let (lng, lat) =
                known_crs_coord_to_wgs84(coord.x, coord.y, crs).unwrap_or((coord.x, coord.y));
            Coord { x: lng, y: lat }
        })),
    }
}

fn utm_to_wgs84(
    easting: f64,
    northing: f64,
    zone: u8,
    northern: bool,
) -> Option<(f64, f64)> {
    let k0 = 0.9996;
    let a = WGS84_A;
    let e2 = WGS84_E2;
    let _e = e2.sqrt();
    let e1 = (1.0 - (1.0 - e2).sqrt()) / (1.0 + (1.0 - e2).sqrt());

    let x = easting - 500000.0;
    let y = if northern {
        northing
    } else {
        northing - 10_000_000.0
    };

    let m = y / k0;
    let mu = m / (a * (1.0 - e2 / 4.0 - 3.0 * e2 * e2 / 64.0 - 5.0 * e2 * e2 * e2 / 256.0));

    let phi1 = mu
        + (3.0 * e1 / 2.0 - 27.0 * e1 * e1 * e1 / 32.0) * (2.0 * mu).sin()
        + (21.0 * e1 * e1 / 16.0 - 55.0 * e1 * e1 * e1 * e1 / 32.0) * (4.0 * mu).sin()
        + (151.0 * e1 * e1 * e1 / 96.0) * (6.0 * mu).sin()
        + (1097.0 * e1 * e1 * e1 * e1 / 512.0) * (8.0 * mu).sin();

    let sin1 = phi1.sin();
    let cos1 = phi1.cos();
    let tan1 = sin1 / cos1;
    let t1 = tan1 * tan1;
    let c1 = e2 * cos1 * cos1 / (1.0 - e2);
    let r1 = a * (1.0 - e2) / ((1.0 - e2 * sin1 * sin1).powf(1.5));
    let n1 = a / (1.0 - e2 * sin1 * sin1).sqrt();

    let d = x / (n1 * k0);

    let lat = phi1
        - (n1 * tan1 / r1)
            * (d * d / 2.0
                - (5.0 + 3.0 * t1 + 10.0 * c1 - 4.0 * c1 * c1 - 9.0 * e2) * d * d * d * d / 24.0
                + (61.0 + 90.0 * t1 + 298.0 * c1 + 45.0 * t1 * t1 - 252.0 * e2 - 3.0 * c1 * c1)
                    * d * d * d * d * d * d
                    / 720.0);

    let lng_origin = ((zone as i32) - 1) * 6 - 180 + 3;
    let lng = lng_origin as f64
        + (d
            - (1.0 + 2.0 * t1 + c1) * d * d * d / 6.0
            + (5.0 - 2.0 * c1 + 28.0 * t1 - 3.0 * c1 * c1 + 8.0 * e2 + 24.0 * t1 * t1)
                * d * d * d * d * d
                / 120.0)
            / cos1;

    Some((lng, lat.to_degrees()))
}

pub fn wgs84_to_native_crs(
    lng: f64,
    lat: f64,
    geo_key: &GeoKeyInfo,
) -> Option<(f64, f64)> {
    if geo_key.model_type == Some(2) {
        return Some((lng, lat));
    }

    let origin_lng = geo_key.proj_nat_origin_long.unwrap_or(0.0);
    let origin_lat = geo_key.proj_nat_origin_lat.unwrap_or(0.0);
    let false_easting = geo_key.proj_false_easting.unwrap_or(0.0);
    let false_northing = geo_key.proj_false_northing.unwrap_or(0.0);
    let scale = geo_key.proj_scale_at_nat_origin.unwrap_or(1.0);

    let (x, y) = transverse_mercator_forward(lng, lat, origin_lng, origin_lat, scale);
    Some((x + false_easting, y + false_northing))
}

pub fn transverse_mercator_forward(
    lng: f64,
    lat: f64,
    origin_lng: f64,
    origin_lat: f64,
    k0: f64,
) -> (f64, f64) {
    let a = WGS84_A;
    let e2 = WGS84_E2;
    let _e = e2.sqrt();

    let lat_rad = lat.to_radians();
    let lng_rad = lng.to_radians();
    let origin_lng_rad = origin_lng.to_radians();
    let origin_lat_rad = origin_lat.to_radians();

    let d_lng = lng_rad - origin_lng_rad;
    let sin_lat = lat_rad.sin();
    let cos_lat = lat_rad.cos();
    let tan_lat = sin_lat / cos_lat;
    let t = tan_lat * tan_lat;
    let n = a / (1.0 - e2 * sin_lat * sin_lat).sqrt();
    let c = e2 * cos_lat * cos_lat / (1.0 - e2);
    let a0 = (1.0 - e2 / 4.0 - 3.0 * e2 * e2 / 64.0 - 5.0 * e2 * e2 * e2 / 256.0) * a;
    let a2 = (3.0 * e2 / 8.0 + 3.0 * e2 * e2 / 32.0 + 45.0 * e2 * e2 * e2 / 1024.0) * a;
    let a4 = (15.0 * e2 * e2 / 256.0 + 45.0 * e2 * e2 * e2 / 1024.0) * a;
    let a6 = 35.0 * e2 * e2 * e2 / 3072.0 * a;

    let m = a0 * origin_lat_rad
        - a2 * (2.0 * origin_lat_rad).sin()
        + a4 * (4.0 * origin_lat_rad).sin()
        - a6 * (6.0 * origin_lat_rad).sin();

    let m_south = m;
    let m_north = a0 * lat_rad
        - a2 * (2.0 * lat_rad).sin()
        + a4 * (4.0 * lat_rad).sin()
        - a6 * (6.0 * lat_rad).sin();

    let northing = k0
        * (m_north - m_south
            + n * tan_lat * d_lng * d_lng * cos_lat / 2.0
            + n * tan_lat * (5.0 - t + 9.0 * c + 4.0 * c * c) * d_lng * d_lng * d_lng * d_lng
                * cos_lat
                / 24.0);

    let easting = k0
        * (n * d_lng * cos_lat
            + n * (1.0 - t + c) * d_lng * d_lng * d_lng * cos_lat / 6.0
            + n
                * (5.0 - 18.0 * t + t * t + 72.0 * c - 58.0 * e2)
                * d_lng * d_lng * d_lng * d_lng * d_lng
                * cos_lat
                / 120.0);

    (easting, northing)
}

pub fn extent_to_wgs84(
    geo_transform: &[f64; 6],
    width: u32,
    height: u32,
    geo_key: &GeoKeyInfo,
) -> Option<[f64; 4]> {
    let gt = *geo_transform;

    if geo_key.model_type == Some(2) {
        let min_lng = gt[0];
        let max_lng = gt[0] + width as f64 * gt[1];
        let min_lat = gt[3] + height as f64 * gt[5];
        let max_lat = gt[3];
        return Some([min_lng, min_lat, max_lng, max_lat]);
    }

    if geo_key.model_type == Some(1) {
        let corners = [
            (gt[0], gt[3]),
            (gt[0] + width as f64 * gt[1], gt[3]),
            (gt[0], gt[3] + height as f64 * gt[5]),
            (gt[0] + width as f64 * gt[1], gt[3] + height as f64 * gt[5]),
        ];

        let known_crs = determine_crs_from_geo_key(geo_key);
        if let Some(crs) = known_crs {
            let wgs84_corners: Vec<(f64, f64)> = corners
                .iter()
                .filter_map(|&(x, y)| known_crs_coord_to_wgs84(x, y, crs))
                .collect();

            if wgs84_corners.len() == 4 {
                let min_lng = wgs84_corners
                    .iter()
                    .map(|(x, _)| x)
                    .cloned()
                    .fold(f64::INFINITY, f64::min);
                let max_lng = wgs84_corners
                    .iter()
                    .map(|(x, _)| x)
                    .cloned()
                    .fold(f64::NEG_INFINITY, f64::max);
                let min_lat = wgs84_corners
                    .iter()
                    .map(|(_, y)| y)
                    .cloned()
                    .fold(f64::INFINITY, f64::min);
                let max_lat = wgs84_corners
                    .iter()
                    .map(|(_, y)| y)
                    .cloned()
                    .fold(f64::NEG_INFINITY, f64::max);
                return Some([min_lng, min_lat, max_lng, max_lat]);
            }
        }

        let origin_lng = geo_key.proj_nat_origin_long.unwrap_or(0.0);
        let origin_lat = geo_key.proj_nat_origin_lat.unwrap_or(0.0);
        let wgs84_corners: Vec<(f64, f64)> = corners
            .iter()
            .map(|&(x, y)| {
                let d_lng = (x - origin_lng) / (WGS84_A * origin_lat.to_radians().cos());
                let lat = y / 111320.0 + origin_lat;
                let lng = d_lng.to_degrees() + origin_lng;
                (lng, lat)
            })
            .collect();

        let min_lng = wgs84_corners
            .iter()
            .map(|(x, _)| x)
            .cloned()
            .fold(f64::INFINITY, f64::min);
        let max_lng = wgs84_corners
            .iter()
            .map(|(x, _)| x)
            .cloned()
            .fold(f64::NEG_INFINITY, f64::max);
        let min_lat = wgs84_corners
            .iter()
            .map(|(_, y)| y)
            .cloned()
            .fold(f64::INFINITY, f64::min);
        let max_lat = wgs84_corners
            .iter()
            .map(|(_, y)| y)
            .cloned()
            .fold(f64::NEG_INFINITY, f64::max);
        return Some([min_lng, min_lat, max_lng, max_lat]);
    }

    None
}

fn determine_crs_from_geo_key(geo_key: &GeoKeyInfo) -> Option<KnownCrs> {
    if let Some(proj_type) = geo_key.projected_type {
        if (32601..=32660).contains(&proj_type) {
            let zone = (proj_type - 32600) as u8;
            return Some(KnownCrs::UtmWgs84 {
                zone,
                northern: true,
            });
        }
        if (32701..=32760).contains(&proj_type) {
            let zone = (proj_type - 32700) as u8;
            return Some(KnownCrs::UtmWgs84 {
                zone,
                northern: false,
            });
        }
    }
    if let Some(geo_type) = geo_key.geographic_type {
        if geo_type == 4326 || geo_type == 4269 {
            return Some(KnownCrs::Wgs84);
        }
    }

    let origin_lng = geo_key.proj_nat_origin_long.unwrap_or(0.0);
    let _origin_lat = geo_key.proj_nat_origin_lat.unwrap_or(0.0);

    let zone = ((origin_lng + 180.0) / 6.0).ceil() as u8;
    if (1..=60).contains(&zone) && origin_lng.abs() <= 180.0 {
        return Some(KnownCrs::UtmWgs84 { zone, northern: true });
    }

    None
}
