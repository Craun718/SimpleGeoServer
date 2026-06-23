use std::fs::File;
use std::io::BufReader;
use tiff::decoder::Decoder;
use tiff::tags::Tag;
use utoipa::ToSchema;

#[allow(dead_code)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, ToSchema)]
pub struct BandInfo {
    pub index: u32,
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    pub std_dev: f64,
    pub no_data: Option<f64>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RasterLoadResult {
    pub path: String,
    pub width: u32,
    pub height: u32,
    pub bands: u32,
    pub data_type: String,
    pub crs: Option<String>,
    pub geo_transform: Option<Vec<f64>>,
    pub band_info: Vec<BandInfo>,
    pub extent: Option<[f64; 4]>,
}

pub fn read_nodata_value(decoder: &mut Decoder<BufReader<File>>) -> Option<f64> {
    let raw = decoder.get_tag_ascii_string(Tag::GdalNodata).ok()?;
    parse_nodata_string(&raw)
}

pub fn parse_nodata_string(raw: &str) -> Option<f64> {
    let cleaned = raw.trim().trim_end_matches('\0').trim();
    if cleaned.is_empty() {
        return None;
    }
    let first = cleaned.split_whitespace().next()?;
    match first.to_ascii_lowercase().as_str() {
        "nan" => Some(f64::NAN),
        "inf" | "+inf" | "infinity" | "+infinity" => Some(f64::INFINITY),
        "-inf" | "-infinity" => Some(f64::NEG_INFINITY),
        _ => first.parse::<f64>().ok(),
    }
}

pub fn is_nodata(val: f64, nodata: Option<f64>) -> bool {
    if !val.is_finite() {
        return true;
    }
    match nodata {
        Some(nd) if nd.is_finite() => val == nd,
        _ => false,
    }
}

include!(concat!(env!("OUT_DIR"), "/crs_names.rs"));

pub fn crs_string_from_geo_key(geo_key: &crate::reproject::GeoKeyInfo) -> String {
    let projected_cs = 1u16;
    let geographic_cs = 2u16;

    // Try to resolve a human-readable name via the CRS name table first
    if let Some(name) = crs_name_from_geo_key(geo_key) {
        return name;
    }

    // Fallback to EPSG:XXXX format
    match geo_key.model_type {
        Some(t) if t == projected_cs => {
            if let Some(proj) = geo_key.projected_type {
                format!("EPSG:{}", proj)
            } else {
                "Projected CRS".to_string()
            }
        }
        Some(t) if t == geographic_cs => {
            if let Some(geo) = geo_key.geographic_type {
                format!("EPSG:{}", geo)
            } else {
                "Geographic CRS".to_string()
            }
        }
        _ => "Unknown".to_string(),
    }
}

fn crs_name_from_geo_key(geo_key: &crate::reproject::GeoKeyInfo) -> Option<String> {
    match geo_key.model_type? {
        1 => {
            let epsg = geo_key.projected_type?;
            crs_name_from_epsg(epsg).map(|s| s.to_string())
        }
        2 => {
            let epsg = geo_key.geographic_type?;
            crs_name_from_epsg(epsg).map(|s| s.to_string())
        }
        _ => None,
    }
}

fn crs_name_from_epsg(epsg: u16) -> Option<&'static str> {
    match CRS_NAME_TABLE.binary_search_by_key(&epsg, |(code, _)| *code) {
        Ok(idx) => Some(CRS_NAME_TABLE[idx].1),
        Err(_) => None,
    }
}

pub fn decode_result_to_f64_vec(result: &tiff::decoder::DecodingResult) -> Vec<f64> {
    match result {
        tiff::decoder::DecodingResult::U8(v) => v.iter().map(|&x| x as f64).collect(),
        tiff::decoder::DecodingResult::U16(v) => v.iter().map(|&x| x as f64).collect(),
        tiff::decoder::DecodingResult::U32(v) => v.iter().map(|&x| x as f64).collect(),
        tiff::decoder::DecodingResult::U64(v) => v.iter().map(|&x| x as f64).collect(),
        tiff::decoder::DecodingResult::F32(v) => v.iter().map(|&x| x as f64).collect(),
        tiff::decoder::DecodingResult::F64(v) => v.to_vec(),
        tiff::decoder::DecodingResult::I8(v) => v.iter().map(|&x| x as f64).collect(),
        tiff::decoder::DecodingResult::I16(v) => v.iter().map(|&x| x as f64).collect(),
        tiff::decoder::DecodingResult::I32(v) => v.iter().map(|&x| x as f64).collect(),
        tiff::decoder::DecodingResult::I64(v) => v.iter().map(|&x| x as f64).collect(),
        tiff::decoder::DecodingResult::F16(v) => v.iter().map(|&x| f64::from(x)).collect(),
    }
}
