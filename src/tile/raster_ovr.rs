use std::io::{Read, Seek, SeekFrom};

use tiff::decoder::{ChunkType, Decoder, Limits};

fn read_u16_ifd_entry(entries: &[(u16, u32)], tag_id: u16) -> Option<u32> {
    entries.iter().find(|(t, _)| *t == tag_id).map(|(_, v)| *v)
}

pub fn parse_ovr_ifd_offsets(
    ovr_path: &str,
    _base_chunk_type: ChunkType,
    _base_chunk_w: u32,
    _base_chunk_h: u32,
    base_index: usize,
) -> Result<Vec<super::types::IfdInfo>, String> {
    let mut file =
        std::fs::File::open(ovr_path).map_err(|e| format!("Failed to open .ovr: {}", e))?;

    let mut header = [0u8; 8];
    file.read_exact(&mut header)
        .map_err(|e| format!("Failed to read TIFF header from .ovr: {}", e))?;

    let byte_order = u16::from_le_bytes([header[0], header[1]]);
    let first_ifd_offset = if byte_order == 0x4949 {
        u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as u64
    } else if byte_order == 0x4D4D {
        u32::from_be_bytes([header[4], header[5], header[6], header[7]]) as u64
    } else {
        return Err("Invalid TIFF byte order in .ovr".to_string());
    };

    let read_entry = |file: &mut std::fs::File, off: u64| -> Option<(u16, u32)> {
        let mut entry = [0u8; 12];
        file.seek(SeekFrom::Start(off)).ok()?;
        file.read_exact(&mut entry).ok()?;
        let tag = if byte_order == 0x4949 {
            u16::from_le_bytes([entry[0], entry[1]])
        } else {
            u16::from_be_bytes([entry[0], entry[1]])
        };
        let val = if byte_order == 0x4949 {
            u32::from_le_bytes([entry[8], entry[9], entry[10], entry[11]])
        } else {
            u32::from_be_bytes([entry[8], entry[9], entry[10], entry[11]])
        };
        Some((tag, val))
    };

    let mut ifds = Vec::new();
    let mut current_offset = Some(first_ifd_offset);

    while let Some(ifd_off) = current_offset {
        file.seek(SeekFrom::Start(ifd_off))
            .map_err(|e| format!("Failed to seek in .ovr: {}", e))?;

        let mut count_buf = [0u8; 2];
        if file.read_exact(&mut count_buf).is_err() {
            break;
        }
        let entry_count = if byte_order == 0x4949 {
            u16::from_le_bytes(count_buf)
        } else {
            u16::from_be_bytes(count_buf)
        } as usize;

        let mut entries: Vec<(u16, u32)> = Vec::new();
        let mut entry_base = ifd_off + 2;
        for _ in 0..entry_count {
            if let Some(e) = read_entry(&mut file, entry_base) {
                entries.push(e);
            }
            entry_base += 12;
        }

        let width = read_u16_ifd_entry(&entries, 0x0100).unwrap_or(0);
        let height = read_u16_ifd_entry(&entries, 0x0101).unwrap_or(0);

        if width == 0 || height == 0 {
            let next_pos = entry_base;
            if file.seek(SeekFrom::Start(next_pos)).is_err() {
                break;
            }
            let mut next_buf = [0u8; 4];
            if file.read_exact(&mut next_buf).is_err() {
                break;
            }
            let next_off = if byte_order == 0x4949 {
                u32::from_le_bytes(next_buf)
            } else {
                u32::from_be_bytes(next_buf)
            };
            current_offset = if next_off != 0 {
                Some(next_off as u64)
            } else {
                None
            };
            continue;
        }

        let (chunk_type, chunk_w, chunk_h) = if let Some(tw) = read_u16_ifd_entry(&entries, 0x0142)
        {
            let tl = read_u16_ifd_entry(&entries, 0x0143).unwrap_or(tw);
            (ChunkType::Tile, tw, tl)
        } else {
            let rows_per_strip = read_u16_ifd_entry(&entries, 0x0117).unwrap_or(height);
            (ChunkType::Strip, width, rows_per_strip)
        };

        let cpr = match chunk_type {
            ChunkType::Strip => 1u32,
            ChunkType::Tile => (width + chunk_w - 1) / chunk_w,
        };

        ifds.push(super::types::IfdInfo {
            index: base_index + ifds.len(),
            width,
            height,
            chunk_type,
            chunk_width: chunk_w,
            chunk_length: chunk_h,
            chunks_per_row: cpr,
            external: true,
            ifd_ptr: Some(ifd_off),
            interleave: super::types::InterleaveType::Chunky,
            file_path: ovr_path.to_string(),
        });

        let next_pos = entry_base;
        if file.seek(SeekFrom::Start(next_pos)).is_err() {
            break;
        }
        let mut next_buf = [0u8; 4];
        if file.read_exact(&mut next_buf).is_err() {
            break;
        }
        let next_off = if byte_order == 0x4949 {
            u32::from_le_bytes(next_buf)
        } else {
            u32::from_be_bytes(next_buf)
        };
        current_offset = if next_off != 0 {
            Some(next_off as u64)
        } else {
            None
        };
    }

    Ok(ifds)
}

#[allow(dead_code)]
fn bilinear_downsample_f64(
    data: &[f64],
    src_w: u32,
    src_h: u32,
    bands: usize,
    dst_w: u32,
    dst_h: u32,
) -> Vec<f64> {
    let mut result = vec![0.0; dst_w as usize * dst_h as usize * bands];
    let x_ratio = src_w as f64 / dst_w as f64;
    let y_ratio = src_h as f64 / dst_h as f64;

    for dy in 0..dst_h {
        for dx in 0..dst_w {
            let sx = (dx as f64 + 0.5) * x_ratio - 0.5;
            let sy = (dy as f64 + 0.5) * y_ratio - 0.5;

            let sx0 = if sx <= 0.0 { 0 } else { sx.floor() as u32 };
            let sy0 = if sy <= 0.0 { 0 } else { sy.floor() as u32 };
            let sx1 = (sx0 + 1).min(src_w - 1);
            let sy1 = (sy0 + 1).min(src_h - 1);

            let fx = sx - sx0 as f64;
            let fy = sy - sy0 as f64;

            for b in 0..bands {
                let idx00 = (sy0 as usize * src_w as usize + sx0 as usize) * bands + b;
                let idx10 = (sy0 as usize * src_w as usize + sx1 as usize) * bands + b;
                let idx01 = (sy1 as usize * src_w as usize + sx0 as usize) * bands + b;
                let idx11 = (sy1 as usize * src_w as usize + sx1 as usize) * bands + b;

                let v00 = data[idx00];
                let v10 = data[idx10];
                let v01 = data[idx01];
                let v11 = data[idx11];

                let v = v00 * (1.0 - fx) * (1.0 - fy)
                    + v10 * fx * (1.0 - fy)
                    + v01 * (1.0 - fx) * fy
                    + v11 * fx * fy;

                let out_idx = (dy as usize * dst_w as usize + dx as usize) * bands + b;
                result[out_idx] = v;
            }
        }
    }

    result
}

#[allow(dead_code)]
pub fn generate_ovr(path: &str) -> Result<(), String> {
    use tiff::encoder::{TiffEncoder, colortype};
    use tiff::tags::ExtraSamples;

    let file = std::fs::File::open(path).map_err(|e| format!("Failed to open {}: {}", path, e))?;
    let mut decoder = Decoder::new(std::io::BufReader::new(file))
        .map_err(|e| format!("Failed to create decoder: {}", e))?
        .with_limits(Limits::unlimited());

    let (width, height) = decoder
        .dimensions()
        .map_err(|e| format!("Failed to read dimensions: {}", e))?;

    let decoded = decoder
        .read_image()
        .map_err(|e| format!("Failed to read image: {}", e))?;

    let f64_data = crate::raster::decode_result_to_f64_vec(&decoded);
    let total_pixels = width as usize * height as usize;
    if total_pixels == 0 {
        return Err("Image has zero pixels".to_string());
    }
    let bands = if f64_data.len() >= total_pixels && total_pixels > 0 {
        f64_data.len() / total_pixels
    } else {
        1
    };

    let ovr_path = format!("{}.ovr", path);
    let file_out =
        std::fs::File::create(&ovr_path).map_err(|e| format!("Failed to create .ovr: {}", e))?;
    let mut tiff =
        TiffEncoder::new(file_out).map_err(|e| format!("Failed to create TIFF encoder: {}", e))?;

    let min_size = 256u32;
    let max_levels = 8usize;
    let mut level_count = 0usize;
    let mut prev_w = width;
    let mut prev_h = height;
    let all_bands = bands;

    if bands > 4 {
        return Err(format!(
            "Cannot generate .ovr for {} bands (max supported: 4)",
            bands
        ));
    }

    loop {
        let new_w = (prev_w / 2).max(1);
        let new_h = (prev_h / 2).max(1);

        if new_w == prev_w && new_h == prev_h {
            break;
        }
        if prev_w <= min_size && prev_h <= min_size {
            break;
        }
        if level_count >= max_levels {
            break;
        }

        let downsampled =
            bilinear_downsample_f64(&f64_data, prev_w, prev_h, all_bands, new_w, new_h);

        macro_rules! write_level {
            ($colortype:ty, $inner_type:ty, $max_val:expr) => {{
                let converted: Vec<$inner_type> = downsampled
                    .iter()
                    .map(|v| {
                        let clamped = v.clamp(0.0, $max_val);
                        clamped.round() as $inner_type
                    })
                    .collect();
                tiff.write_image::<$colortype>(new_w, new_h, &converted)
                    .map_err(|e| format!("Failed to write .ovr level {}: {}", level_count, e))?;
            }};
            ($colortype:ty, $inner_type:ty) => {{
                let converted: Vec<$inner_type> =
                    downsampled.iter().map(|v| *v as $inner_type).collect();
                tiff.write_image::<$colortype>(new_w, new_h, &converted)
                    .map_err(|e| format!("Failed to write .ovr level {}: {}", level_count, e))?;
            }};
        }

        match &decoded {
            tiff::decoder::DecodingResult::U8(_) => match all_bands {
                1 => write_level!(colortype::Gray8, u8, 255.0),
                2 => {
                    let gray: Vec<u8> = downsampled
                        .iter()
                        .step_by(2)
                        .map(|v| v.round().clamp(0.0, 255.0) as u8)
                        .collect();
                    let extra: Vec<u8> = downsampled
                        .iter()
                        .skip(1)
                        .step_by(2)
                        .map(|v| v.round().clamp(0.0, 255.0) as u8)
                        .collect();
                    let mut interleaved = Vec::with_capacity(gray.len() + extra.len());
                    for i in 0..gray.len() {
                        interleaved.push(gray[i]);
                        interleaved.push(extra[i]);
                    }
                    let mut image = tiff
                        .new_image::<colortype::Gray8>(new_w, new_h)
                        .map_err(|e| format!("Failed to create image encoder: {}", e))?;
                    image
                        .extra_samples(&[ExtraSamples::Unspecified])
                        .map_err(|e| format!("Failed to set extra samples: {}", e))?;
                    image.write_data(&interleaved).map_err(|e| {
                        format!("Failed to write .ovr level {}: {}", level_count, e)
                    })?;
                }
                3 => write_level!(colortype::RGB8, u8, 255.0),
                4 => write_level!(colortype::RGBA8, u8, 255.0),
                _ => unreachable!(),
            },
            tiff::decoder::DecodingResult::U16(_) => match all_bands {
                1 => write_level!(colortype::Gray16, u16, 65535.0),
                2 => {
                    let gray: Vec<u16> = downsampled
                        .iter()
                        .step_by(2)
                        .map(|v| v.round().clamp(0.0, 65535.0) as u16)
                        .collect();
                    let extra: Vec<u16> = downsampled
                        .iter()
                        .skip(1)
                        .step_by(2)
                        .map(|v| v.round().clamp(0.0, 65535.0) as u16)
                        .collect();
                    let mut interleaved = Vec::with_capacity(gray.len() + extra.len());
                    for i in 0..gray.len() {
                        interleaved.push(gray[i]);
                        interleaved.push(extra[i]);
                    }
                    let mut image = tiff
                        .new_image::<colortype::Gray16>(new_w, new_h)
                        .map_err(|e| format!("Failed to create image encoder: {}", e))?;
                    image
                        .extra_samples(&[ExtraSamples::Unspecified])
                        .map_err(|e| format!("Failed to set extra samples: {}", e))?;
                    image.write_data(&interleaved).map_err(|e| {
                        format!("Failed to write .ovr level {}: {}", level_count, e)
                    })?;
                }
                3 => write_level!(colortype::RGB16, u16, 65535.0),
                4 => write_level!(colortype::RGBA16, u16, 65535.0),
                _ => unreachable!(),
            },
            tiff::decoder::DecodingResult::F32(_) => match all_bands {
                1 => write_level!(colortype::Gray32Float, f32),
                2 => {
                    let gray: Vec<f32> = downsampled.iter().step_by(2).map(|v| *v as f32).collect();
                    let extra: Vec<f32> = downsampled
                        .iter()
                        .skip(1)
                        .step_by(2)
                        .map(|v| *v as f32)
                        .collect();
                    let mut interleaved = Vec::with_capacity(gray.len() + extra.len());
                    for i in 0..gray.len() {
                        interleaved.push(gray[i]);
                        interleaved.push(extra[i]);
                    }
                    let mut image = tiff
                        .new_image::<colortype::Gray32Float>(new_w, new_h)
                        .map_err(|e| format!("Failed to create image encoder: {}", e))?;
                    image
                        .extra_samples(&[ExtraSamples::Unspecified])
                        .map_err(|e| format!("Failed to set extra samples: {}", e))?;
                    image.write_data(&interleaved).map_err(|e| {
                        format!("Failed to write .ovr level {}: {}", level_count, e)
                    })?;
                }
                3 => write_level!(colortype::RGB32Float, f32),
                4 => write_level!(colortype::RGBA32Float, f32),
                _ => unreachable!(),
            },
            tiff::decoder::DecodingResult::F64(_) => match all_bands {
                1 => write_level!(colortype::Gray64Float, f64),
                3 => write_level!(colortype::RGB64Float, f64),
                4 => write_level!(colortype::RGBA64Float, f64),
                _ => {
                    return Err(format!("Unsupported band count {} for f64 .ovr", all_bands));
                }
            },
            _ => {
                return Err("Unsupported data type for .ovr generation".to_string());
            }
        }

        prev_w = new_w;
        prev_h = new_h;
        level_count += 1;
    }

    log::info!("Generated .ovr with {} levels: {}", level_count, ovr_path);
    Ok(())
}
