#![cfg(feature = "gpu")]
#![allow(dead_code)]

use std::sync::LazyLock;

use crate::resample::{ResamplingMode, StretchConfig};
use crate::tile::CachedRaster;

const TILE_PX: u32 = 256;

const TILE_SHADER_SRC: &str = wgsl::TILE_RENDER_COMPUTE;

mod wgsl {
    #[rustfmt::skip]
    pub const TILE_RENDER_COMPUTE: &str = "
struct RenderParams {
    tile_x: u32,
    tile_y: u32,
    tile_z: u32,
    num_bands: u32,
    band_r: u32,
    band_g: u32,
    band_b: u32,
    resampling: u32,
    has_nodata: u32,
    _pad0: u32,
    _pad1: u32,
    stretch_lo_r: f32,
    stretch_hi_r: f32,
    stretch_lo_g: f32,
    stretch_hi_g: f32,
    stretch_lo_b: f32,
    stretch_hi_b: f32,
    nodata: f32,
    u0: f32,
    u1: f32,
    v0: f32,
    v1: f32,
    buf_u0: f32,
    buf_u1: f32,
    buf_v0: f32,
    buf_v1: f32,
    read_w: u32,
    read_h: u32,
};

@group(0) @binding(0) var<uniform> params: RenderParams;
@group(0) @binding(1) var<storage, read> src_data: array<f32>;
@group(0) @binding(2) var<storage, read_write> out_data: array<u32>;

fn sample_nearest(col: f32, row: f32, w: u32, h: u32, bands: u32, band: u32) -> f32 {
    let ix = i32(col + 0.5);
    let iy = i32(row + 0.5);
    if (ix < 0 || iy < 0 || ix >= i32(w) || iy >= i32(h)) {
        return bitcast<f32>(0x7fc00000u);
    }
    let idx = (iy * i32(w) + ix) * i32(bands) + i32(band);
    return src_data[u32(idx)];
}

fn sample_bilinear(col: f32, row: f32, w: u32, h: u32, bands: u32, band: u32) -> f32 {
    if (col < 0.0 || row < 0.0 || col >= f32(w - 1) || row >= f32(h - 1)) {
        return bitcast<f32>(0x7fc00000u);
    }
    let x0 = u32(col);
    let y0 = u32(row);
    let x1 = min(x0 + 1, w - 1);
    let y1 = min(y0 + 1, h - 1);
    let fx = col - f32(x0);
    let fy = row - f32(y0);

    let b0 = band;
    let stride = i32(bands);
    let v00 = src_data[(i32(y0) * i32(w) + i32(x0)) * stride + i32(b0)];
    let v10 = src_data[(i32(y0) * i32(w) + i32(x1)) * stride + i32(b0)];
    let v01 = src_data[(i32(y1) * i32(w) + i32(x0)) * stride + i32(b0)];
    let v11 = src_data[(i32(y1) * i32(w) + i32(x1)) * stride + i32(b0)];

    let v0 = v00 * (1.0 - fx) + v10 * fx;
    let v1 = v01 * (1.0 - fx) + v11 * fx;
    return v0 * (1.0 - fy) + v1 * fy;
}

fn sample_bicubic(col: f32, row: f32, w: u32, h: u32, bands: u32, band: u32) -> f32 {
    if (col < 0.0 || row < 0.0 || col >= f32(w) || row >= f32(h)) {
        return bitcast<f32>(0x7fc00000u);
    }
    let ix = i32(col);
    let iy = i32(row);
    let fx = col - f32(ix);
    let fy = row - f32(iy);

    var cols: array<f32, 4>;
    let iw = i32(w);
    let ih = i32(h);
    let stride = i32(bands);
    for (var cy: u32 = 0u; cy < 4u; cy = cy + 1u) {
        let py = clamp(iy + i32(cy) - 1, 0, ih - 1);
        let p0 = src_data[(py * iw + clamp(ix - 1, 0, iw - 1)) * stride + i32(band)];
        let p1 = src_data[(py * iw + clamp(ix, 0, iw - 1)) * stride + i32(band)];
        let p2 = src_data[(py * iw + clamp(ix + 1, 0, iw - 1)) * stride + i32(band)];
        let p3 = src_data[(py * iw + clamp(ix + 2, 0, iw - 1)) * stride + i32(band)];
        cols[cy] = cubic_hermite(p0, p1, p2, p3, fx);
    }
    return cubic_hermite(cols[0], cols[1], cols[2], cols[3], fy);
}

fn cubic_hermite(p0: f32, p1: f32, p2: f32, p3: f32, t: f32) -> f32 {
    return 0.5 * ((2.0 * p1) + (-p0 + p2) * t + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * t * t + (-p0 + 3.0 * p1 - 3.0 * p2 + p3) * t * t * t);
}

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let tx = gid.x;
    let ty = gid.y;
    if (tx >= 256u || ty >= 256u) {
        out_data[ty * 256u + tx] = 0u;
        return;
    }

    let tx_ratio = (f32(tx) + 0.5) / 256.0;
    let ty_ratio = (f32(ty) + 0.5) / 256.0;
    let u = params.u0 + (params.u1 - params.u0) * tx_ratio;
    let v = params.v0 + (params.v1 - params.v0) * ty_ratio;

    let du = max(params.buf_u1 - params.buf_u0, 1e-10);
    let dv = max(params.buf_v1 - params.buf_v0, 1e-10);
    let buf_col = (u - params.buf_u0) / du * f32(params.read_w - 1u);
    let buf_row = (v - params.buf_v0) / dv * f32(params.read_h - 1u);

    let bands = params.num_bands;
    let use_grayscale = bands < 3u;

    var rgba: vec4<u32> = vec4<u32>(0u, 0u, 0u, 0u);
    var is_nodata: bool = false;

    if (buf_col >= 0.0 && buf_row >= 0.0 && buf_col < f32(params.read_w) && buf_row < f32(params.read_h)) {
        if (params.resampling == 0u) {
            let ds_col = i32(buf_col + 0.5);
            let ds_row = i32(buf_row + 0.5);
            if (ds_col >= 0 && ds_row >= 0 && ds_col < i32(params.read_w) && ds_row < i32(params.read_h)) {
                let idx = (ds_row * i32(params.read_w) + ds_col) * i32(bands);
                if (use_grayscale) {
                    let val = src_data[u32(idx)];
                    if (params.has_nodata == 1u && val == params.nodata) { is_nodata = true; }
                    else if (val != val) { is_nodata = true; }
                    else {
                        let lo = params.stretch_lo_r;
                        let hi = params.stretch_hi_r;
                        let stretched = clamp((val - lo) / max(hi - lo, 1e-10) * 255.0, 0.0, 255.0);
                        let gray = u32(stretched);
                        rgba = vec4<u32>(gray, gray, gray, 255u);
                    }
                } else {
                    let v_r = src_data[u32(idx + i32(params.band_r))];
                    let v_g = src_data[u32(idx + i32(params.band_g))];
                    let v_b = src_data[u32(idx + i32(params.band_b))];
                    if ((params.has_nodata == 1u && (v_r == params.nodata || v_g == params.nodata || v_b == params.nodata)) || v_r != v_r || v_g != v_g || v_b != v_b) {
                        is_nodata = true;
                    } else {
                        let stretched_r = clamp((v_r - params.stretch_lo_r) / max(params.stretch_hi_r - params.stretch_lo_r, 1e-10) * 255.0, 0.0, 255.0);
                        let stretched_g = clamp((v_g - params.stretch_lo_g) / max(params.stretch_hi_g - params.stretch_lo_g, 1e-10) * 255.0, 0.0, 255.0);
                        let stretched_b = clamp((v_b - params.stretch_lo_b) / max(params.stretch_hi_b - params.stretch_lo_b, 1e-10) * 255.0, 0.0, 255.0);
                        rgba = vec4<u32>(u32(stretched_r), u32(stretched_g), u32(stretched_b), 255u);
                    }
                }
            } else {
                is_nodata = true;
            }
        } else {
            if (params.resampling == 1u) {
                if (use_grayscale) {
                    let val = sample_bilinear(buf_col, buf_row, params.read_w, params.read_h, bands, 0u);
                    if (val != val || (params.has_nodata == 1u && val == params.nodata)) {
                        is_nodata = true;
                    } else {
                        let lo = params.stretch_lo_r;
                        let hi = params.stretch_hi_r;
                        let stretched = clamp((val - lo) / max(hi - lo, 1e-10) * 255.0, 0.0, 255.0);
                        let gray = u32(stretched);
                        rgba = vec4<u32>(gray, gray, gray, 255u);
                    }
                } else {
                    let val_r = sample_bilinear(buf_col, buf_row, params.read_w, params.read_h, bands, params.band_r);
                    let val_g = sample_bilinear(buf_col, buf_row, params.read_w, params.read_h, bands, params.band_g);
                    let val_b = sample_bilinear(buf_col, buf_row, params.read_w, params.read_h, bands, params.band_b);
                    if (val_r != val_r || val_g != val_g || val_b != val_b ||
                        (params.has_nodata == 1u && (val_r == params.nodata || val_g == params.nodata || val_b == params.nodata))) {
                        is_nodata = true;
                    } else {
                        let sr = clamp((val_r - params.stretch_lo_r) / max(params.stretch_hi_r - params.stretch_lo_r, 1e-10) * 255.0, 0.0, 255.0);
                        let sg = clamp((val_g - params.stretch_lo_g) / max(params.stretch_hi_g - params.stretch_lo_g, 1e-10) * 255.0, 0.0, 255.0);
                        let sb = clamp((val_b - params.stretch_lo_b) / max(params.stretch_hi_b - params.stretch_lo_b, 1e-10) * 255.0, 0.0, 255.0);
                        rgba = vec4<u32>(u32(sr), u32(sg), u32(sb), 255u);
                    }
                }
            } else {
                if (use_grayscale) {
                    let val = sample_bicubic(buf_col, buf_row, params.read_w, params.read_h, bands, 0u);
                    if (val != val || (params.has_nodata == 1u && val == params.nodata)) {
                        is_nodata = true;
                    } else {
                        let lo = params.stretch_lo_r;
                        let hi = params.stretch_hi_r;
                        let stretched = clamp((val - lo) / max(hi - lo, 1e-10) * 255.0, 0.0, 255.0);
                        let gray = u32(stretched);
                        rgba = vec4<u32>(gray, gray, gray, 255u);
                    }
                } else {
                    let val_r = sample_bicubic(buf_col, buf_row, params.read_w, params.read_h, bands, params.band_r);
                    let val_g = sample_bicubic(buf_col, buf_row, params.read_w, params.read_h, bands, params.band_g);
                    let val_b = sample_bicubic(buf_col, buf_row, params.read_w, params.read_h, bands, params.band_b);
                    if (val_r != val_r || val_g != val_g || val_b != val_b ||
                        (params.has_nodata == 1u && (val_r == params.nodata || val_g == params.nodata || val_b == params.nodata))) {
                        is_nodata = true;
                    } else {
                        let sr = clamp((val_r - params.stretch_lo_r) / max(params.stretch_hi_r - params.stretch_lo_r, 1e-10) * 255.0, 0.0, 255.0);
                        let sg = clamp((val_g - params.stretch_lo_g) / max(params.stretch_hi_g - params.stretch_lo_g, 1e-10) * 255.0, 0.0, 255.0);
                        let sb = clamp((val_b - params.stretch_lo_b) / max(params.stretch_hi_b - params.stretch_lo_b, 1e-10) * 255.0, 0.0, 255.0);
                        rgba = vec4<u32>(u32(sr), u32(sg), u32(sb), 255u);
                    }
                }
            }
        }
    } else {
        is_nodata = true;
    }

    if (is_nodata) {
        rgba = vec4<u32>(0u, 0u, 0u, 0u);
    }

    out_data[ty * 256u + tx] = rgba.r | (rgba.g << 8u) | (rgba.b << 16u) | (rgba.a << 24u);
}
";
}

#[derive(Debug, Clone, Copy)]
#[repr(C, align(16))]
struct ShaderParams {
    tile_x: u32,
    tile_y: u32,
    tile_z: u32,
    num_bands: u32,
    band_r: u32,
    band_g: u32,
    band_b: u32,
    resampling: u32,
    has_nodata: u32,
    _pad0: u32,
    _pad1: u32,
    stretch_lo_r: f32,
    stretch_hi_r: f32,
    stretch_lo_g: f32,
    stretch_hi_g: f32,
    stretch_lo_b: f32,
    stretch_hi_b: f32,
    nodata: f32,
    u0: f32,
    u1: f32,
    v0: f32,
    v1: f32,
    buf_u0: f32,
    buf_u1: f32,
    buf_v0: f32,
    buf_v1: f32,
    read_w: u32,
    read_h: u32,
}

pub struct TileRenderResult {
    pub rgba: Vec<u8>,
    pub rendered_pixels: u32,
}

pub static GPU_RENDERER: LazyLock<Option<GpuRenderer>> = LazyLock::new(
    || match pollster::block_on(GpuRenderer::new()) {
        Ok(renderer) => {
            tracing::info!("GPU renderer initialized successfully");
            Some(renderer)
        }
        Err(e) => {
            tracing::info!(
                "GPU renderer not available (this is normal if no GPU detected), using CPU fallback: {e}"
            );
            None
        }
    },
);

pub fn is_gpu_available() -> bool {
    GPU_RENDERER.is_some()
}

pub struct GpuRenderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    compute_pipeline: wgpu::ComputePipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    params_buffer: wgpu::Buffer,
    src_buffer: wgpu::Buffer,
    out_buffer: wgpu::Buffer,
    params_size: u64,
    max_src_size: u64,
}

impl GpuRenderer {
    async fn new() -> Result<Self, String> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            flags: wgpu::InstanceFlags::default(),
            backend_options: Default::default(),
            memory_budget_thresholds: Default::default(),
            display: None,
        });

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: None,
                force_fallback_adapter: false,
            })
            .await
            .map_err(|_| "No suitable GPU adapter found")?;

        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("Tile Render GPU Device"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::default(),
                    memory_hints: wgpu::MemoryHints::default(),
                    experimental_features: wgpu::ExperimentalFeatures::disabled(),
                    trace: wgpu::Trace::Off,
                },
            )
            .await
            .map_err(|e| format!("Failed to create GPU device: {e}"))?;

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Tile Render Shader"),
            source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(TILE_SHADER_SRC)),
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("Tile Render Bind Group Layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: false },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("Tile Render Pipeline Layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let compute_pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("Tile Render Compute Pipeline"),
            layout: Some(&pipeline_layout),
            module: &shader,
            entry_point: Some("main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            cache: None,
        });

        let params_size = std::mem::size_of::<ShaderParams>() as u64;
        let params_size_aligned = (params_size + 255) & !255;
        let max_src_size = (1024 * 1024 * 8 * 4) as u64;
        let out_size = (TILE_PX * TILE_PX * 4) as u64;

        let params_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Params Buffer"),
            size: params_size_aligned,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let src_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Source Data Buffer"),
            size: max_src_size,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let out_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Output Buffer"),
            size: out_size,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });

        Ok(Self {
            device,
            queue,
            compute_pipeline,
            bind_group_layout,
            params_buffer,
            src_buffer,
            out_buffer,
            params_size: params_size_aligned,
            max_src_size,
        })
    }

    pub fn render_tile(
        &self,
        raster: &CachedRaster,
        z: u32,
        x: u32,
        y: u32,
        _size: u32,
        bands: &[u32],
        stretch: Option<&StretchConfig>,
        resampling: ResamplingMode,
        src_data: &[f32],
        read_w: u32,
        read_h: u32,
        uvs: [f32; 8],
    ) -> Result<TileRenderResult, String> {
        let needed_size = (src_data.len() * std::mem::size_of::<f32>()) as u64;
        if needed_size > self.max_src_size {
            return Err(format!(
                "Source data too large for GPU buffer: {} bytes, max is {} bytes",
                needed_size, self.max_src_size
            ));
        }

        let stretch_bounds = crate::resample::compute_stretch_bounds(raster, stretch);
        let band_indices: Vec<usize> = bands.iter().map(|b| *b as usize - 1).collect();

        let has_nodata: u32 = if raster.no_data.is_some() { 1 } else { 0 };
        let nodata = raster.no_data.unwrap_or(f64::NAN);

        let params = ShaderParams {
            tile_x: x,
            tile_y: y,
            tile_z: z,
            num_bands: raster.bands as u32,
            band_r: band_indices.get(0).copied().unwrap_or(0) as u32,
            band_g: band_indices.get(1).copied().unwrap_or(0) as u32,
            band_b: band_indices.get(2).copied().unwrap_or(0) as u32,
            resampling: match resampling {
                ResamplingMode::NearestNeighbor => 0,
                ResamplingMode::Bilinear => 1,
                ResamplingMode::Bicubic => 2,
                ResamplingMode::Lanczos3 => 1,
            },
            has_nodata,
            _pad0: 0,
            _pad1: 0,
            stretch_lo_r: stretch_bounds.get(0).map(|b| b.0 as f32).unwrap_or(0.0),
            stretch_hi_r: stretch_bounds.get(0).map(|b| b.1 as f32).unwrap_or(255.0),
            stretch_lo_g: stretch_bounds.get(1).map(|b| b.0 as f32).unwrap_or(0.0),
            stretch_hi_g: stretch_bounds.get(1).map(|b| b.1 as f32).unwrap_or(255.0),
            stretch_lo_b: stretch_bounds.get(2).map(|b| b.0 as f32).unwrap_or(0.0),
            stretch_hi_b: stretch_bounds.get(2).map(|b| b.1 as f32).unwrap_or(255.0),
            nodata: nodata as f32,
            u0: uvs[0],
            u1: uvs[1],
            v0: uvs[2],
            v1: uvs[3],
            buf_u0: uvs[4],
            buf_u1: uvs[5],
            buf_v0: uvs[6],
            buf_v1: uvs[7],
            read_w,
            read_h,
        };

        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("Tile Render Bind Group"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.params_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: self.src_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: self.out_buffer.as_entire_binding(),
                },
            ],
        });

        let params_bytes = unsafe {
            std::slice::from_raw_parts(
                &params as *const ShaderParams as *const u8,
                std::mem::size_of::<ShaderParams>(),
            )
        };
        self.queue
            .write_buffer(&self.params_buffer, 0, params_bytes);

        let src_bytes = unsafe {
            std::slice::from_raw_parts(
                src_data.as_ptr() as *const u8,
                src_data.len() * std::mem::size_of::<f32>(),
            )
        };
        self.queue.write_buffer(&self.src_buffer, 0, src_bytes);

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("Tile Render Encoder"),
            });

        {
            let mut cpass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("Tile Render Pass"),
                timestamp_writes: None,
            });
            cpass.set_pipeline(&self.compute_pipeline);
            cpass.set_bind_group(0, &bind_group, &[]);
            cpass.dispatch_workgroups(16, 16, 1);
        }

        let out_size = (TILE_PX * TILE_PX * 4) as u64;
        let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Output Readback Buffer"),
            size: out_size,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        encoder.copy_buffer_to_buffer(&self.out_buffer, 0, &readback, 0, out_size);

        self.queue.submit(std::iter::once(encoder.finish()));

        let slice = readback.slice(..);
        let (sender, receiver) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());

        match receiver.recv().map_err(|_| "Channel closed")? {
            Ok(()) => {
                let mapped = slice.get_mapped_range();
                let rgba: Vec<u8> = mapped.to_vec();
                drop(mapped);
                readback.unmap();

                let rendered_pixels = rgba.chunks(4).filter(|p| p[3] > 0).count() as u32;

                Ok(TileRenderResult {
                    rgba,
                    rendered_pixels,
                })
            }
            Err(e) => Err(format!("GPU readback error: {e:?}")),
        }
    }
}

mod pollster {
    use std::future::Future;

    pub fn block_on<F: Future>(future: F) -> F::Output {
        let waker = noop_waker();
        let mut context = std::task::Context::from_waker(&waker);
        let mut pinned = std::pin::pin!(future);
        loop {
            match pinned.as_mut().poll(&mut context) {
                std::task::Poll::Ready(val) => return val,
                std::task::Poll::Pending => {
                    std::thread::yield_now();
                }
            }
        }
    }

    fn noop_waker() -> std::task::Waker {
        let raw = std::task::RawWaker::new(
            std::ptr::null(),
            &std::task::RawWakerVTable::new(
                |_| std::task::RawWaker::new(std::ptr::null(), &VTABLE),
                |_| {},
                |_| {},
                |_| {},
            ),
        );
        unsafe { std::task::Waker::from_raw(raw) }
    }

    const VTABLE: std::task::RawWakerVTable = std::task::RawWakerVTable::new(
        |_| std::task::RawWaker::new(std::ptr::null(), &VTABLE),
        |_| {},
        |_| {},
        |_| {},
    );
}
