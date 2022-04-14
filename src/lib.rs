mod utils;

use wasm_bindgen::prelude::*;
use naga::front::wgsl;
use naga::front::wgsl::ParseError;
use num::Integer;
use bitvec::prelude::*;
use std::mem::{size_of, take};
use std::sync::atomic::{AtomicBool, Ordering};

// When the `wee_alloc` feature is enabled, use `wee_alloc` as the global
// allocator.
#[cfg(feature = "wee_alloc")]
#[global_allocator]
static ALLOC: wee_alloc::WeeAlloc = wee_alloc::WeeAlloc::INIT;

#[wasm_bindgen]
pub struct WgpuContext {
    window: winit::window::Window,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface,
    surface_format: wgpu::TextureFormat,
}

#[derive(Clone, Copy, bytemuck::Zeroable, bytemuck::Pod)]
#[repr(C)]
struct Time {
    frame: u32,
    elapsed: f32,
}

#[derive(Clone, Copy, bytemuck::Zeroable, bytemuck::Pod)]
#[repr(C)]
struct Mouse {
    pos: [u32; 2],
    click: i32,
}

struct Uniforms {
    time: wgpu::Buffer,
    mouse: wgpu::Buffer,
    keys: wgpu::Buffer,
    custom: wgpu::Buffer,
    storage_buffer: wgpu::Buffer,
    tex_read: wgpu::Texture,
    tex_write: wgpu::Texture,
    tex_screen: wgpu::Texture,
}

#[derive(Clone)]
struct ErrorCallback(Option<js_sys::Function>);

impl ErrorCallback {
    fn call(&self, summary: &str, row: usize, col: usize) {
        match self.0 {
            None => log::error!("No error callback registered"),
            Some(ref callback) => {
                let res = callback.call3(
                    &JsValue::NULL,
                    &JsValue::from(summary),
                    &JsValue::from(row),
                    &JsValue::from(col)
                );
                match res {
                    Err(error) => log::error!("Error calling registered error callback: {error:?}"),
                    _ => ()
                };
            }
        }
    }
}

// safe because wasm is single-threaded: https://github.com/rustwasm/wasm-bindgen/issues/1505
unsafe impl Send for ErrorCallback {}
unsafe impl Sync for ErrorCallback {}

const NUM_KEYCODES: usize = 256;
const MAX_CUSTOM_PARAMS: usize = 16;

#[wasm_bindgen]
pub struct WgpuToyRenderer {
    wgpu: WgpuContext,
    screen_width: u32,
    screen_height: u32,
    time: Time,
    mouse: Mouse,
    keys: BitArr!(for NUM_KEYCODES, in u8, Lsb0),
    custom: std::collections::BTreeMap<String, f32>,
    uniforms: Uniforms,
    compute_bind_group_layout: wgpu::BindGroupLayout,
    compute_pipeline_layout: wgpu::PipelineLayout,
    last_compute_pipelines: Option<Vec<(wgpu::ComputePipeline, [u32; 3])>>,
    compute_pipelines: Vec<(wgpu::ComputePipeline, [u32; 3])>,
    compute_bind_group: wgpu::BindGroup,
    staging_belt: wgpu::util::StagingBelt,
    on_error_cb: ErrorCallback,
    channels: [wgpu::Texture; 2],
    pass_f32: bool,
    screen_blitter: Blitter,
}

static SHADER_ERROR: AtomicBool = AtomicBool::new(false);

fn compute_bind_group_layout_entries(pass_f32: bool) -> [wgpu::BindGroupLayoutEntry; 12] {
    [
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
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        },
        wgpu::BindGroupLayoutEntry {
            binding: 2,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        },
        wgpu::BindGroupLayoutEntry {
            binding: 3,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        },
        wgpu::BindGroupLayoutEntry {
            binding: 4,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::StorageTexture {
                access: wgpu::StorageTextureAccess::WriteOnly,
                format: wgpu::TextureFormat::Rgba16Float,
                view_dimension: wgpu::TextureViewDimension::D2,
            },
            count: None,
        },
        wgpu::BindGroupLayoutEntry {
            binding: 5,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage {
                    read_only: false
                },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        },
        wgpu::BindGroupLayoutEntry {
            binding: 6,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Texture {
                multisampled: false,
                sample_type: wgpu::TextureSampleType::Float { filterable: !pass_f32 },
                view_dimension: wgpu::TextureViewDimension::D2Array,
            },
            count: None,
        },
        wgpu::BindGroupLayoutEntry {
            binding: 7,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::StorageTexture {
                access: wgpu::StorageTextureAccess::WriteOnly,
                format: if pass_f32 { wgpu::TextureFormat::Rgba32Float } else { wgpu::TextureFormat::Rgba16Float },
                view_dimension: wgpu::TextureViewDimension::D2Array,
            },
            count: None,
        },
        wgpu::BindGroupLayoutEntry {
            binding: 8,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::NonFiltering),
            count: None,
        },
        wgpu::BindGroupLayoutEntry {
            binding: 9,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
            count: None,
        },
        wgpu::BindGroupLayoutEntry {
            binding: 10,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Texture {
                multisampled: false,
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
            },
            count: None,
        },
        wgpu::BindGroupLayoutEntry {
            binding: 11,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Texture {
                multisampled: false,
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
            },
            count: None,
        },
    ]
}

const RENDER_BIND_GROUP_LAYOUT_DESCRIPTOR: wgpu::BindGroupLayoutDescriptor = wgpu::BindGroupLayoutDescriptor {
    label: None,
    entries: &[
        wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                multisampled: false,
                sample_type: wgpu::TextureSampleType::Float { filterable: false },
                view_dimension: wgpu::TextureViewDimension::D2,
            },
            count: None,
        },
        wgpu::BindGroupLayoutEntry {
            binding: 1,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::NonFiltering),
            count: None,
        },
    ],
};

#[cfg(target_arch = "wasm32")]
fn init_window(bind_id: String) -> Result<winit::window::Window, Box<dyn std::error::Error>> {
    console_log::init(); // FIXME only do this once
    utils::set_panic_hook();
    let event_loop = winit::event_loop::EventLoop::new();
    let win = web_sys::window().ok_or("window is None")?;
    let doc = win.document().ok_or("document is None")?;
    let element = doc.get_element_by_id(&bind_id).ok_or(format!("cannot find element {bind_id}"))?;
    use wasm_bindgen::JsCast;
    let canvas = element.dyn_into::<web_sys::HtmlCanvasElement>().or(Err("cannot cast to canvas"))?;
    use winit::platform::web::WindowBuilderExtWebSys;
    let window = winit::window::WindowBuilder::new()
        .with_canvas(Some(canvas))
        .build(&event_loop)?;
    Ok(window)
}

#[cfg(not(target_arch = "wasm32"))]
fn init_window(_: String) -> Result<winit::window::Window, Box<dyn std::error::Error>> {
    env_logger::init();
    let event_loop = winit::event_loop::EventLoop::new();
    winit::window::Window::new(&event_loop).map_err(Box::from)
}

// FIXME: async fn(&str) doesn't currently work with wasm_bindgen: https://stackoverflow.com/a/63655324/78204
#[wasm_bindgen]
pub async fn init_wgpu(bind_id: String) -> Result<WgpuContext, String> {
    let window = init_window(bind_id).map_err(|e| e.to_string())?;
    let instance = wgpu::Instance::new(wgpu::Backends::PRIMARY);
    let surface = unsafe { instance.create_surface(&window) };
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: Default::default(),
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
        })
        .await.ok_or("unable to create adapter")?;
    let (device, queue) = adapter
        .request_device(&Default::default(), None)
        .await.map_err(|e| e.to_string())?;
    let size = window.inner_size();
    let surface_format = surface.get_preferred_format(&adapter).unwrap_or(wgpu::TextureFormat::Bgra8UnormSrgb);
    surface.configure(&device, &wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format: surface_format,
        width: size.width,
        height: size.height,
        present_mode: wgpu::PresentMode::Fifo, // vsync
    });
    Ok(WgpuContext {
        window,
        device,
        queue,
        surface,
        surface_format,
    })
}

fn create_uniforms(wgpu: &WgpuContext, width: u32, height: u32, pass_f32: bool) -> Uniforms {
    Uniforms {
        time: wgpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: size_of::<Time>() as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::UNIFORM,
            mapped_at_creation: false,
        }),
        mouse: wgpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: size_of::<Mouse>() as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::UNIFORM,
            mapped_at_creation: false,
        }),
        keys: wgpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: (NUM_KEYCODES / 8) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::UNIFORM,
            mapped_at_creation: false,
        }),
        custom: wgpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: (MAX_CUSTOM_PARAMS * size_of::<f32>()) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::UNIFORM,
            mapped_at_creation: false,
        }),
        storage_buffer: wgpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: (4 * 4 * width * height).into(),
            usage: wgpu::BufferUsages::STORAGE,
            mapped_at_creation: false,
        }),
        tex_read: wgpu.device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 4,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: if pass_f32 { wgpu::TextureFormat::Rgba32Float } else { wgpu::TextureFormat::Rgba16Float },
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
        }),
        tex_write: wgpu.device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 4,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: if pass_f32 { wgpu::TextureFormat::Rgba32Float } else { wgpu::TextureFormat::Rgba16Float },
            usage: wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::STORAGE_BINDING,
        }),
        tex_screen: wgpu.device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba16Float,
            usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING,
        }),
    }
}

fn create_compute_bind_group(wgpu: &WgpuContext, layout: &wgpu::BindGroupLayout, uniforms: &Uniforms, channels: &[wgpu::Texture]) -> wgpu::BindGroup {
    wgpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: None,
        layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: uniforms.custom.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 1, resource: uniforms.time.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 2, resource: uniforms.mouse.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 3, resource: uniforms.keys.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::TextureView(&uniforms.tex_screen.create_view(&Default::default())) },
            wgpu::BindGroupEntry { binding: 5, resource: uniforms.storage_buffer.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 6, resource: wgpu::BindingResource::TextureView(&uniforms.tex_read.create_view(&wgpu::TextureViewDescriptor {
                dimension: Some(wgpu::TextureViewDimension::D2Array),
                ..Default::default()
            })) },
            wgpu::BindGroupEntry { binding: 7, resource: wgpu::BindingResource::TextureView(&uniforms.tex_write.create_view(&wgpu::TextureViewDescriptor {
                dimension: Some(wgpu::TextureViewDimension::D2Array),
                ..Default::default()
            })) },
            wgpu::BindGroupEntry { binding: 8, resource: wgpu::BindingResource::Sampler(&wgpu.device.create_sampler(&Default::default())) },
            wgpu::BindGroupEntry { binding: 9, resource: wgpu::BindingResource::Sampler(&wgpu.device.create_sampler(&wgpu::SamplerDescriptor {
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                ..Default::default()
            })) },
            wgpu::BindGroupEntry { binding: 10, resource: wgpu::BindingResource::TextureView(&channels[0].create_view(&Default::default())) },
            wgpu::BindGroupEntry { binding: 11, resource: wgpu::BindingResource::TextureView(&channels[1].create_view(&Default::default())) },
        ],
    })
}

enum ColourSpace {
    Linear,
    Rgbe,
}

struct Blitter {
    render_pipeline: wgpu::RenderPipeline,
    render_bind_group: wgpu::BindGroup,
    dest_format: wgpu::TextureFormat,
}

impl Blitter {
    fn new(wgpu: &WgpuContext, src: &wgpu::Texture, src_space: ColourSpace, dest_format: wgpu::TextureFormat) -> Self {
        let render_shader = wgpu.device.create_shader_module(&wgpu::ShaderModuleDescriptor {
            label: None,
            source: wgpu::ShaderSource::Wgsl(include_str!("blit.wgsl").into()),
        });
        let render_bind_group_layout = wgpu.device.create_bind_group_layout(&RENDER_BIND_GROUP_LAYOUT_DESCRIPTOR);
        Blitter {
            render_bind_group: wgpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &render_bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&src.create_view(&Default::default())) },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&wgpu.device.create_sampler(&Default::default())) },
                ],
            }),
            render_pipeline: wgpu.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: None,
                layout: Some(&wgpu.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: None,
                    bind_group_layouts: &[&render_bind_group_layout],
                    push_constant_ranges: &[],
                })),
                vertex: wgpu::VertexState {
                    module: &render_shader,
                    entry_point: "vs_main",
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &render_shader,
                    entry_point: match (src_space, dest_format) {
                        // FIXME use sRGB viewFormats instead once the API stabilises
                        (ColourSpace::Linear, wgpu::TextureFormat::Bgra8Unorm) => "fs_main_linear_to_srgb",
                        (ColourSpace::Linear, wgpu::TextureFormat::Bgra8UnormSrgb) => "fs_main", // format automatically performs sRGB encoding
                        (ColourSpace::Rgbe, wgpu::TextureFormat::Rgba16Float) => "fs_main_rgbe_to_linear",
                        _ => panic!("Blitter: unrecognised conversion")
                    },
                    targets: &[dest_format.into()],
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview: None,
            }),
            dest_format,
        }
    }

    fn blit(&self, encoder: &mut wgpu::CommandEncoder, dest: &wgpu::Texture) {
        let view = &dest.create_view(&Default::default());
        let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: None,
            color_attachments: &[wgpu::RenderPassColorAttachment {
                view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::GREEN),
                    store: true,
                },
            }],
            depth_stencil_attachment: None,
        });
        render_pass.set_pipeline(&self.render_pipeline);
        render_pass.set_bind_group(0, &self.render_bind_group, &[]);
        render_pass.draw(0..3, 0..1);
    }

    fn create_texture(&self, wgpu: &WgpuContext, width: u32, height: u32) -> wgpu::Texture {
        let texture = wgpu.device.create_texture(
            &wgpu::TextureDescriptor {
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: self.dest_format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT,
                label: None,
            }
        );
        let mut encoder = wgpu.device.create_command_encoder(&Default::default());
        self.blit(&mut encoder, &texture);
        wgpu.queue.submit(Some(encoder.finish()));
        texture
    }
}

fn stage(staging_belt: &mut wgpu::util::StagingBelt, device: &wgpu::Device, encoder: &mut wgpu::CommandEncoder, data: &[u8], buffer: &wgpu::Buffer) {
    match wgpu::BufferSize::new(data.len() as u64) {
        None => log::warn!("no data to stage"),
        Some(size) => staging_belt.write_buffer(encoder, buffer, 0, size, device)
                                  .copy_from_slice(data)
    }
}

// https://llogiq.github.io/2016/09/24/newline.html
fn count_newlines(s: &str) -> usize {
    s.as_bytes().iter().filter(|&&c| c == b'\n').count()
}

#[wasm_bindgen]
impl WgpuToyRenderer {
    #[wasm_bindgen(constructor)]
    pub fn new(wgpu: WgpuContext) -> WgpuToyRenderer {
        let size = wgpu.window.inner_size();
        let uniforms = create_uniforms(&wgpu, size.width, size.height, false);
        let compute_bind_group_layout = wgpu.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &compute_bind_group_layout_entries(false),
        });

        let blank = wgpu::TextureDescriptor {
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            label: None,
        };
        let channels = [
            wgpu.device.create_texture(&blank),
            wgpu.device.create_texture(&blank),
        ];

        let mut custom = std::collections::BTreeMap::new();
        custom.insert("_dummy".into(), 0.); // just to avoid creating an empty struct in wgsl

        WgpuToyRenderer {
            compute_pipeline_layout: wgpu.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: None,
                bind_group_layouts: &[&compute_bind_group_layout],
                push_constant_ranges: &[],
            }),
            compute_bind_group: create_compute_bind_group(&wgpu, &compute_bind_group_layout, &uniforms, &channels),
            last_compute_pipelines: None,
            compute_pipelines: vec![],
            screen_width: size.width,
            screen_height: size.height,
            time: Time {
                frame: 0,
                elapsed: 0.,
            },
            mouse: Mouse {
                pos: [0, 0],
                click: 0,
            },
            keys: bitarr![u8, Lsb0; 0; 256],
            staging_belt: wgpu::util::StagingBelt::new(4096),
            screen_blitter: Blitter::new(&wgpu, &uniforms.tex_screen, ColourSpace::Linear, wgpu.surface_format),
            wgpu,
            uniforms,
            compute_bind_group_layout,
            on_error_cb: ErrorCallback(None),
            channels,
            custom,
            pass_f32: false,
        }
    }

    pub fn render(&mut self) {
        match self.wgpu.surface.get_current_texture() {
            Err(e) => log::error!("Unable to get framebuffer: {e}"),
            Ok(f) => self.render_to(f)
        }
    }

    fn render_to(&mut self, frame: wgpu::SurfaceTexture) {
        let mut encoder = self.wgpu.device.create_command_encoder(&Default::default());
        let custom_bytes: Vec<u8> = self.custom.values().flat_map(|x| bytemuck::bytes_of(x).iter().copied()).collect();
        stage(&mut self.staging_belt, &self.wgpu.device, &mut encoder, &custom_bytes, &self.uniforms.custom);
        stage(&mut self.staging_belt, &self.wgpu.device, &mut encoder, bytemuck::bytes_of(&self.time), &self.uniforms.time);
        stage(&mut self.staging_belt, &self.wgpu.device, &mut encoder, bytemuck::bytes_of(&self.mouse), &self.uniforms.mouse);
        stage(&mut self.staging_belt, &self.wgpu.device, &mut encoder, &self.keys.as_raw_slice(), &self.uniforms.keys);
        self.staging_belt.finish();
        if SHADER_ERROR.swap(false, Ordering::SeqCst) {
            match take(&mut self.last_compute_pipelines) {
                None => log::warn!("unable to rollback shader after error"),
                Some(vec) => {
                    self.compute_pipelines = vec;
                }
            }
        }
        for (pipeline, workgroup_size) in &self.compute_pipelines {
            {
                let mut compute_pass = encoder.begin_compute_pass(&Default::default());
                compute_pass.set_pipeline(pipeline);
                compute_pass.set_bind_group(0, &self.compute_bind_group, &[]);
                compute_pass.dispatch(self.screen_width.div_ceil(&workgroup_size[0]), self.screen_height.div_ceil(&workgroup_size[1]), 1);
            }
            encoder.copy_texture_to_texture(
                wgpu::ImageCopyTexture {
                    texture: &self.uniforms.tex_write,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::ImageCopyTexture {
                    texture: &self.uniforms.tex_read,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::Extent3d {
                    width: self.screen_width,
                    height: self.screen_height,
                    depth_or_array_layers: 4,
                });
        }
        self.time.frame += 1;
        self.screen_blitter.blit(&mut encoder, &frame.texture);
        self.wgpu.queue.submit(Some(encoder.finish()));
        wasm_bindgen_futures::spawn_local(self.staging_belt.recall());
        frame.present();
    }

    fn prelude(&self) -> String {
        let mut s = String::new();
        s.push_str(r#"
            type int = i32;
            type uint = u32;
            type float = f32;

            type int2 = vec2<i32>;
            type int3 = vec3<i32>;
            type int4 = vec4<i32>;
            type uint2 = vec2<u32>;
            type uint3 = vec3<u32>;
            type uint4 = vec4<u32>;
            type float2 = vec2<f32>;
            type float3 = vec3<f32>;
            type float4 = vec4<f32>;

            struct Time { frame: uint, elapsed: float };
            struct Mouse { pos: uint2, click: int };
        "#);
        s.push_str("struct Custom {");
        for name in self.custom.keys() {
            s.push_str(&name);
            s.push_str(": float,");
        }
        s.push_str("};");
        s.push_str("@group(0) @binding(0) var<uniform> custom: Custom;");
        let pass_format = if self.pass_f32 { "rgba32float" } else { "rgba16float" };
        s.push_str(&format!(r#"
            @group(0) @binding(1) var<uniform> time: Time;
            @group(0) @binding(2) var<uniform> mouse: Mouse;
            @group(0) @binding(3) var<uniform> _keyboard: array<vec4<u32>,2>;
            @group(0) @binding(4) var screen: texture_storage_2d<rgba16float,write>;
            @group(0) @binding(5) var<storage,read_write> atomic_storage: array<atomic<i32>>;
            @group(0) @binding(6) var pass_in: texture_2d_array<f32>;
            @group(0) @binding(7) var pass_out: texture_storage_2d_array<{pass_format},write>;
            @group(0) @binding(8) var nearest: sampler;
            @group(0) @binding(9) var bilinear: sampler;
            @group(0) @binding(10) var channel0: texture_2d<f32>;
            @group(0) @binding(11) var channel1: texture_2d<f32>;
        "#));
        s.push_str(r#"
            fn keyDown(keycode: uint) -> bool {
                return ((_keyboard[keycode / 128u][(keycode % 128u) / 32u] >> (keycode % 32u)) & 1u) == 1u;
            }
        "#);
        return s;
    }

    fn handle_error(&self, e: ParseError, wgsl: &str) {
        let prelude_len = count_newlines(&self.prelude()); // in case we need to report errors
        let (row, col) = e.location(&wgsl);
        let summary = e.emit_to_string(&wgsl);
        self.on_error_cb.call(&summary, if row >= prelude_len { row - prelude_len } else { 0 }, col);
    }

    pub fn set_shader(&mut self, shader: &str) {
        let mut wgsl: String = self.prelude();
        let shader: String = shader.into();
        wgsl.push_str(&shader);
        match wgsl::parse_str(&wgsl) {
            Ok(module) => {
                let entry_points: Vec<_> = module.entry_points.iter()
                    .filter(|f| f.stage == naga::ShaderStage::Compute).collect();
                let compute_shader = self.wgpu.device.create_shader_module(&wgpu::ShaderModuleDescriptor {
                    label: None,
                    source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(&wgsl)),
                });
                self.last_compute_pipelines = Some(take(&mut self.compute_pipelines));
                self.compute_pipelines = entry_points.iter().map(|entry_point| {
                    (self.wgpu.device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                        label: None,
                        layout: Some(&self.compute_pipeline_layout),
                        module: &compute_shader,
                        entry_point: &entry_point.name,
                    }), entry_point.workgroup_size)
                }).collect();
            },
            Err(e) => {
                log::error!("Error parsing WGSL: {e}");
                self.handle_error(e, &wgsl);
            },
        }
    }

    pub fn set_time_elapsed(&mut self, t: f32) {
        self.time.elapsed = t;
    }

    pub fn set_mouse_pos(&mut self, x: u32, y: u32) {
        self.mouse.pos = [x, y];
    }

    pub fn set_mouse_click(&mut self, click: bool) {
        self.mouse.click = if click {1} else {0};
    }

    pub fn set_keydown(&mut self, keycode: usize, keydown: bool) {
        self.keys.set(keycode, keydown);
    }

    pub fn set_custom_float(&mut self, name: &str, value: f32) {
        self.custom.insert(name.into(), value);
    }

    pub fn set_pass_f32(&mut self, pass_f32: bool) {
        self.pass_f32 = pass_f32;
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.screen_width = width;
        self.screen_height = height;
        self.time.frame = 0;
        self.uniforms = create_uniforms(&self.wgpu, width, height, self.pass_f32);
        self.compute_bind_group_layout = self.wgpu.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &compute_bind_group_layout_entries(self.pass_f32),
        });
        self.compute_pipeline_layout = self.wgpu.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[&self.compute_bind_group_layout],
            push_constant_ranges: &[],
        });
        self.compute_bind_group = create_compute_bind_group(&self.wgpu, &self.compute_bind_group_layout, &self.uniforms, &self.channels);
        self.screen_blitter = Blitter::new(&self.wgpu, &self.uniforms.tex_screen, ColourSpace::Linear, self.wgpu.surface_format);
        self.wgpu.window.set_inner_size(winit::dpi::LogicalSize::new(width, height));
    }

    pub fn on_error(&mut self, callback: js_sys::Function) {
        self.on_error_cb = ErrorCallback(Some(callback));

        // FIXME: remove pending resolution of this issue: https://github.com/gfx-rs/wgpu/issues/2130
        let prelude_len = count_newlines(&self.prelude());
        let re = lazy_regex::regex!(r"Parser:\s:(\d+):(\d+)\s([\s\S]*?)\s+Shader");
        let on_error_cb = self.on_error_cb.clone();
        self.wgpu.device.on_uncaptured_error(move |e: wgpu::Error| {
            let err = &e.to_string();
            match re.captures(err) {
                None =>  log::error!("{e}"),
                Some(cap) => {
                    let row = cap[1].parse().unwrap_or(prelude_len);
                    let col = cap[2].parse().unwrap_or(0);
                    let summary = &cap[3];
                    on_error_cb.call(summary, if row >= prelude_len { row - prelude_len } else { 0 }, col);
                    SHADER_ERROR.store(true, Ordering::SeqCst);
                }
            }
        });
    }

    pub fn load_channel(&mut self, index: usize, bytes: &[u8]) {
        match image::load_from_memory(bytes) {
            Err(e) => log::error!("load_channel: {e}"),
            Ok(im) => {
                self.channels[index] = create_texture_from_image(&self.wgpu, &im, wgpu::TextureFormat::Rgba8UnormSrgb);
                self.compute_bind_group = create_compute_bind_group(&self.wgpu, &self.compute_bind_group_layout, &self.uniforms, &self.channels);
            }
        }
    }

    pub fn load_channel_rgbe(&mut self, index: usize, bytes: &[u8]) {
        match image::load_from_memory(bytes) {
            Err(e) => log::error!("load_channel_rgbe: {e}"),
            Ok(im) => {
                use image::GenericImageView;
                let (width, height) = im.dimensions();
                self.channels[index] = Blitter::new(
                    &self.wgpu,
                    &create_texture_from_image(&self.wgpu, &im, wgpu::TextureFormat::Rgba8Unorm),
                    ColourSpace::Rgbe,
                    wgpu::TextureFormat::Rgba16Float,
                ).create_texture(&self.wgpu, width, height);
                self.compute_bind_group = create_compute_bind_group(&self.wgpu, &self.compute_bind_group_layout, &self.uniforms, &self.channels);
            }
        }
    }
}

fn create_texture_from_image(wgpu: &WgpuContext, im: &image::DynamicImage, format: wgpu::TextureFormat) -> wgpu::Texture {
    use image::GenericImageView;
    let (width, height) = im.dimensions();
    let texture = wgpu.device.create_texture(
        &wgpu::TextureDescriptor {
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            label: None,
        }
    );
    wgpu.queue.write_texture(
        texture.as_image_copy(),
        &im.to_rgba8(),
        wgpu::ImageDataLayout {
            offset: 0,
            bytes_per_row: std::num::NonZeroU32::new(4 * width),
            rows_per_image: std::num::NonZeroU32::new(height),
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    texture
}
