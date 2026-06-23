use std::collections::HashMap;
use std::io::{Read, Seek};
use std::sync::Mutex;

use geo::{Coord, Geometry, MapCoords};
use geodesy::prelude::*;
use std::sync::LazyLock;
use tiff::TiffResult;
use tiff::decoder::Decoder;
use tiff::tags::Tag;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnownCrs {
    Wgs84,
    WebMercator,
    Epsg(u16),
}

impl KnownCrs {
    #[allow(dead_code)]
    pub fn epsg_code(&self) -> Option<u16> {
        match self {
            KnownCrs::Wgs84 => Some(4326),
            KnownCrs::WebMercator => Some(3857),
            KnownCrs::Epsg(code) => Some(*code),
        }
    }
}

#[derive(Debug, Clone, Default)]
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

// ─── Geodesy Engine ───

struct GeodesyEngine {
    ctx: Minimal,
    cache: HashMap<String, OpHandle>,
}

impl GeodesyEngine {
    fn new() -> Self {
        Self {
            ctx: Minimal::new(),
            cache: HashMap::new(),
        }
    }

    fn apply(
        &mut self,
        op_str: &str,
        dir: Direction,
        coords: &mut dyn CoordinateSet,
    ) -> Result<(), Error> {
        let handle = self.cache.get(op_str).copied().unwrap_or_else(|| {
            let h = self.ctx.op(op_str).expect("failed to register geodesy op");
            self.cache.insert(op_str.to_string(), h);
            h
        });
        self.ctx.apply(handle, dir, coords).map(|_| ())
    }
}

static GEODESY: LazyLock<Mutex<GeodesyEngine>> = LazyLock::new(|| Mutex::new(GeodesyEngine::new()));

fn wgs84_to_crs(lng_deg: f64, lat_deg: f64, epsg: u16) -> Option<(f64, f64)> {
    if epsg == 4326 {
        return Some((lng_deg, lat_deg));
    }
    if epsg == 3857 {
        let mut coords = [Coor2D::raw(lng_deg.to_radians(), lat_deg.to_radians())];
        GEODESY
            .lock()
            .unwrap()
            .apply("webmerc", Fwd, &mut coords)
            .ok()?;
        return Some(coords[0].xy());
    }
    let def = crs_definitions::from_code(epsg)?;
    let op_str = geodesy::authoring::parse_proj(def.proj4).ok()?;
    let mut coords = [Coor2D::raw(lng_deg.to_radians(), lat_deg.to_radians())];
    GEODESY
        .lock()
        .unwrap()
        .apply(&op_str, Fwd, &mut coords)
        .ok()?;
    Some(coords[0].xy())
}

fn crs_to_wgs84(x: f64, y: f64, epsg: u16) -> Option<(f64, f64)> {
    if epsg == 4326 {
        return Some((x, y));
    }
    if epsg == 3857 {
        let mut coords = [Coor2D::raw(x, y)];
        GEODESY
            .lock()
            .unwrap()
            .apply("webmerc", Inv, &mut coords)
            .ok()?;
        let (lng_rad, lat_rad) = coords[0].xy();
        return Some((lng_rad.to_degrees(), lat_rad.to_degrees()));
    }
    let def = crs_definitions::from_code(epsg)?;
    let op_str = geodesy::authoring::parse_proj(def.proj4).ok()?;
    let mut coords = [Coor2D::raw(x, y)];
    GEODESY
        .lock()
        .unwrap()
        .apply(&op_str, Inv, &mut coords)
        .ok()?;
    let (lng_rad, lat_rad) = coords[0].xy();
    Some((lng_rad.to_degrees(), lat_rad.to_degrees()))
}

// ─── GeoKey Parsing ───

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

    let double_data: Vec<f64> = match decoder.find_tag(Tag::GeoDoubleParamsTag)? {
        Some(v) => v.into_f64_vec()?,
        None => Vec::new(),
    };

    let mut info = GeoKeyInfo::default();
    for i in 0..num_keys {
        let base = 4 + 4 * i;
        let key_id = dir_data[base];
        let tiff_tag_loc = dir_data[base + 1];
        let count = dir_data[base + 2];
        let value_or_offset = dir_data[base + 3];

        match key_id {
            1024 if tiff_tag_loc == 0 && count == 1 => info.model_type = Some(value_or_offset),
            2048 if tiff_tag_loc == 0 && count == 1 => info.geographic_type = Some(value_or_offset),
            3072 if tiff_tag_loc == 0 && count == 1 => info.projected_type = Some(value_or_offset),
            3075 if tiff_tag_loc == 0 && count == 1 => {
                info.proj_coord_trans = Some(value_or_offset)
            }
            3080 | 3081 | 3082 | 3083 | 3092 => {
                let val = if tiff_tag_loc == 0 && count == 1 {
                    Some(value_or_offset as f64)
                } else if tiff_tag_loc == 34736 {
                    let idx = value_or_offset as usize;
                    if idx < double_data.len() {
                        Some(double_data[idx])
                    } else {
                        None
                    }
                } else {
                    None
                };
                if let Some(v) = val {
                    match key_id {
                        3080 => info.proj_nat_origin_long = Some(v),
                        3081 => info.proj_nat_origin_lat = Some(v),
                        3082 => info.proj_false_easting = Some(v),
                        3083 => info.proj_false_northing = Some(v),
                        3092 => info.proj_scale_at_nat_origin = Some(v),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    Ok(info)
}

// ─── CRS Parsing ───

pub fn parse_known_crs(name: &str) -> Option<KnownCrs> {
    let upper = name.to_uppercase();
    let compact: String = upper
        .chars()
        .filter(|ch| !matches!(ch, ' ' | '_' | '-'))
        .collect();
    let is_wgs84_family = compact.contains("WGS84") || compact.contains("WGS1984");

    if compact.contains("EPSG:3857")
        || compact.contains("EPSG3857")
        || compact.contains("WEBMERCATOR")
        || compact.contains("PSEUDOMERCATOR")
        || compact.contains("AUXILIARYSPHERE")
    {
        return Some(KnownCrs::WebMercator);
    }

    if let Some((zone, northern)) = parse_utm_zone(&compact) {
        let epsg: u16 = if northern {
            32600 + zone as u16
        } else {
            32700 + zone as u16
        };
        return Some(KnownCrs::Epsg(epsg));
    }

    if compact.contains("EPSG:4326")
        || compact.contains("EPSG4326")
        || compact.contains("CRS84")
        || compact.contains("OGC:CRS84")
        || is_wgs84_family
    {
        return Some(KnownCrs::Wgs84);
    }

    if compact.starts_with("EPSG:") || compact.starts_with("EPSG") {
        let num_part = compact
            .trim_start_matches("EPSG:")
            .trim_start_matches("EPSG");
        if let Ok(code) = num_part.parse::<u16>()
            && crs_definitions::from_code(code).is_some()
        {
            return Some(KnownCrs::Epsg(code));
        }
    }

    None
}

fn parse_utm_zone(compact: &str) -> Option<(u8, bool)> {
    let chars: Vec<char> = compact.chars().collect();
    for start in 0..chars.len() {
        if !compact[start..].starts_with("UTM") {
            continue;
        }
        let mut idx = start + 3;
        let scan_limit = (start + 20).min(chars.len());
        while idx < scan_limit && !chars[idx].is_ascii_digit() {
            idx += 1;
        }
        if idx >= scan_limit {
            continue;
        }
        let digit_start = idx;
        while idx < scan_limit && chars[idx].is_ascii_digit() {
            idx += 1;
        }
        if digit_start == idx {
            continue;
        }
        let zone: u8 = chars[digit_start..idx]
            .iter()
            .collect::<String>()
            .parse()
            .ok()?;
        if !(1..=60).contains(&zone) {
            continue;
        }
        while idx < scan_limit && chars[idx] != 'N' && chars[idx] != 'S' {
            idx += 1;
        }
        if idx < scan_limit {
            return Some((zone, chars[idx] == 'N'));
        }
    }
    None
}

// ─── Coordinate Transformations ───

pub fn known_crs_coord_to_wgs84(x: f64, y: f64, crs: KnownCrs) -> Option<(f64, f64)> {
    match crs {
        KnownCrs::Wgs84 => Some((x, y)),
        KnownCrs::WebMercator => crs_to_wgs84(x, y, 3857),
        KnownCrs::Epsg(epsg) => crs_to_wgs84(x, y, epsg),
    }
}

pub fn known_crs_geometry_to_wgs84(
    geometry: &Geometry<f64>,
    crs: KnownCrs,
) -> Option<Geometry<f64>> {
    geometry
        .try_map_coords(|Coord { x, y }| {
            known_crs_coord_to_wgs84(x, y, crs)
                .map(|(lng, lat)| Coord { x: lng, y: lat })
                .ok_or(())
        })
        .ok()
}

pub fn wgs84_to_native_crs(lng: f64, lat: f64, geo_key: &GeoKeyInfo) -> Option<(f64, f64)> {
    if geo_key.model_type == Some(2) {
        return Some((lng, lat));
    }
    if let Some(epsg) = epsg_from_geo_key(geo_key) {
        return wgs84_to_crs(lng, lat, epsg);
    }
    if geo_key.proj_coord_trans == Some(1) {
        let proj4 = proj4_from_tm_params(geo_key)?;
        let op_str = geodesy::authoring::parse_proj(&proj4).ok()?;
        let mut coords = [Coor2D::raw(lng.to_radians(), lat.to_radians())];
        GEODESY
            .lock()
            .unwrap()
            .apply(&op_str, Fwd, &mut coords)
            .ok()?;
        return Some(coords[0].xy());
    }
    None
}

#[allow(dead_code)]
pub fn known_crs_corners_to_wgs84(corners: &[(f64, f64); 4], crs: KnownCrs) -> Option<[f64; 4]> {
    match crs {
        KnownCrs::Wgs84 => Some(sort_extent(corners)),
        _ => {
            let mut wgs = Vec::new();
            for &(x, y) in corners {
                wgs.push(known_crs_coord_to_wgs84(x, y, crs)?);
            }
            Some(sort_extent(&[wgs[0], wgs[1], wgs[2], wgs[3]]))
        }
    }
}

pub fn known_crs_extent_to_wgs84(extent: [f64; 4], crs: KnownCrs) -> Option<[f64; 4]> {
    let corners = [
        (extent[0], extent[1]),
        (extent[2], extent[1]),
        (extent[0], extent[3]),
        (extent[2], extent[3]),
    ];
    known_crs_corners_to_wgs84(&corners, crs)
}

pub fn extent_to_wgs84(
    geo_transform: &[f64],
    width: u32,
    height: u32,
    geo_key: &GeoKeyInfo,
) -> Option<[f64; 4]> {
    if geo_transform.len() < 6 {
        return None;
    }
    let corners = model_corners(geo_transform, width, height);

    if geo_key.model_type == Some(2) {
        return Some(sort_extent(&corners));
    }

    if let Some(epsg) = epsg_from_geo_key(geo_key) {
        let mut wgs_corners = Vec::new();
        for &(x, y) in &corners {
            wgs_corners.push(crs_to_wgs84(x, y, epsg)?);
        }
        if wgs_corners.len() == 4 {
            return Some(sort_extent(&[
                wgs_corners[0],
                wgs_corners[1],
                wgs_corners[2],
                wgs_corners[3],
            ]));
        }
    }

    if geo_key.proj_coord_trans == Some(1)
        && let Some(proj4_str) = proj4_from_tm_params(geo_key)
        && let Ok(op_str) = geodesy::authoring::parse_proj(&proj4_str)
    {
        let mut engine = GEODESY.lock().unwrap();
        let mut wgs_corners = Vec::new();
        for &(x, y) in &corners {
            let mut coords = [Coor2D::raw(x, y)];
            engine.apply(&op_str, Inv, &mut coords).ok()?;
            let (lng_rad, lat_rad) = coords[0].xy();
            wgs_corners.push((lng_rad.to_degrees(), lat_rad.to_degrees()));
        }
        if wgs_corners.len() == 4 {
            return Some(sort_extent(&[
                wgs_corners[0],
                wgs_corners[1],
                wgs_corners[2],
                wgs_corners[3],
            ]));
        }
    }

    if geographic_range(&corners) {
        return Some(sort_extent(&corners));
    }
    if is_web_mercator_range(&corners) {
        let mut wgs_corners = Vec::new();
        for &(x, y) in &corners {
            wgs_corners.push(crs_to_wgs84(x, y, 3857)?);
        }
        if wgs_corners.len() == 4 {
            return Some(sort_extent(&[
                wgs_corners[0],
                wgs_corners[1],
                wgs_corners[2],
                wgs_corners[3],
            ]));
        }
    }

    None
}

// ─── Helpers ───

fn epsg_from_geo_key(geo_key: &GeoKeyInfo) -> Option<u16> {
    if let Some(epsg) = geo_key.projected_type {
        return Some(epsg);
    }
    if geo_key.model_type == Some(2) {
        return geo_key.geographic_type.or(Some(4326));
    }
    if geo_key.proj_coord_trans == Some(1) {
        return None;
    }
    geo_key.geographic_type
}

fn proj4_from_tm_params(geo_key: &GeoKeyInfo) -> Option<String> {
    let lon0 = geo_key.proj_nat_origin_long?;
    let lat0 = geo_key.proj_nat_origin_lat?;
    let k0 = geo_key.proj_scale_at_nat_origin.unwrap_or(1.0);
    let fe = geo_key.proj_false_easting.unwrap_or(0.0);
    let fn_ = geo_key.proj_false_northing.unwrap_or(0.0);
    Some(format!(
        "+proj=tmerc +lat_0={lat0} +lon_0={lon0} +k={k0} +x_0={fe} +y_0={fn_} +ellps=WGS84 +units=m +no_defs"
    ))
}

fn model_corners(gt: &[f64], width: u32, height: u32) -> [(f64, f64); 4] {
    let (xo, xr, _, yo, _, yr) = (gt[0], gt[1], gt[2], gt[3], gt[4], gt[5]);
    [
        (xo, yo),
        (xo + width as f64 * xr, yo),
        (xo, yo + height as f64 * yr),
        (xo + width as f64 * xr, yo + height as f64 * yr),
    ]
}

fn sort_extent(corners: &[(f64, f64); 4]) -> [f64; 4] {
    let mut xs: Vec<f64> = corners.iter().map(|c| c.0).collect();
    let mut ys: Vec<f64> = corners.iter().map(|c| c.1).collect();
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    ys.sort_by(|a, b| a.partial_cmp(b).unwrap());
    [xs[0], ys[0], xs[3], ys[3]]
}

fn is_web_mercator_range(corners: &[(f64, f64); 4]) -> bool {
    const WM_BOUND: f64 = 20037508.34;
    corners
        .iter()
        .all(|&(x, y)| x.abs() <= WM_BOUND && y.abs() <= WM_BOUND && x.abs() > 180.0)
}

fn geographic_range(corners: &[(f64, f64); 4]) -> bool {
    corners
        .iter()
        .all(|&(x, y)| x.abs() <= 180.0 && y.abs() <= 90.0)
}
