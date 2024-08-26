use camera::{Camera, OrthographicProjection};
use cmap::LinearSegmentedColorMap;
use controller::CameraController;
use renderer::{RenderSettings, VolumeRenderer};
use ssoa::SSAOTextures;
use std::{path::PathBuf, sync::Arc};
use volume::VolumeGPU;

#[cfg(target_arch = "wasm32")]
use instant::{Duration, Instant};
#[cfg(not(target_arch = "wasm32"))]
use std::time::{Duration, Instant};

use wgpu::Backends;

#[cfg(target_arch = "wasm32")]
mod web;
#[cfg(target_arch = "wasm32")]
pub use web::*;

use cgmath::Vector2;
use winit::{
    dpi::PhysicalSize, event::{DeviceEvent, ElementState, Event, WindowEvent}, event_loop::{ControlFlow, EventLoop}, keyboard::{KeyCode, PhysicalKey},  window::{Window, WindowBuilder}
};

use crate::{
    cmap::{ColorMapGPU, COLORMAP_RESOLUTION},
    volume::Volume,
};

pub mod camera;
pub mod cmap;
mod controller;
pub mod offline;
#[cfg(feature = "python")]
pub mod py;
pub mod renderer;
mod ssoa;
mod ui;
mod ui_renderer;
mod viewer;
pub mod volume;
pub use viewer::viewer;
#[cfg(feature = "python")]
pub mod py;
// pub mod image;

#[derive(Debug)]
pub struct RenderConfig {
    pub no_vsync: bool,
    pub background_color: wgpu::Color,
    pub show_colormap_editor: bool,
    pub show_volume_info: bool,
    pub vmin: Option<f32>,
    pub vmax: Option<f32>,
    pub distance_scale: f32,
    #[cfg(feature = "colormaps")]
    pub show_cmap_select: bool,
    pub duration: Option<Duration>,
}

pub struct WGPUContext {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub adapter: wgpu::Adapter,
}

impl WGPUContext {
    pub async fn new(instance: &wgpu::Instance, surface: Option<&wgpu::Surface<'static>>) -> Self {
        let adapter = wgpu::util::initialize_adapter_from_env_or_default(instance, surface)
            .await
            .unwrap();

        let required_features = wgpu::Features::default();

        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    required_features,
                    required_limits: adapter.limits(),
                    label: None,
                },
                None,
            )
            .await
            .unwrap();

        Self {
            device,
            queue,
            adapter,
        }
    }
}

pub struct WindowContext {
    wgpu_context: WGPUContext,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    window: Arc<Window>,
    scale_factor: f32,

    controller: CameraController,
    camera: Camera<OrthographicProjection>,
    ui_renderer: ui_renderer::EguiWGPU,
    ui_visible: bool,

    background_color: wgpu::Color,

    volume: VolumeGPU,
    renderer: VolumeRenderer,

    render_settings: RenderSettings,
    cmap_gpu: cmap::ColorMapGPU,
    cmap: LinearSegmentedColorMap,

    playing: bool,
    animation_duration: Duration,

    colormap_editor_visible: bool,
    volume_info_visible: bool,
    #[cfg(feature = "colormaps")]
    cmap_select_visible: bool,

    ssao: ssoa::SSAO,
    ssao_textures: ssoa::SSAOTextures,

    show_box:bool
}

impl WindowContext {
    // Creating some of the wgpu types requires async code
    async fn new(
        window: Window,
        volume: Volume,
        cmap: LinearSegmentedColorMap,
        render_config: &RenderConfig,
    ) -> anyhow::Result<Self> {
        let mut size = window.inner_size();
        if size.width == 0 || size.height == 0 {
            size = PhysicalSize::new(800, 600);
        }
        let window = Arc::new(window);

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: Backends::all().symmetric_difference(Backends::BROWSER_WEBGPU),
            ..Default::default()
        });

        let surface: wgpu::Surface = instance.create_surface(window.clone())?;

        let wgpu_context = WGPUContext::new(&instance, Some(&surface)).await;

        log::info!("device: {:?}", wgpu_context.adapter.get_info().name);

        let device = &wgpu_context.device;
        let queue = &wgpu_context.queue;

        let max_size = device.limits().max_texture_dimension_2d;
        window.set_max_inner_size(Some(PhysicalSize::new(max_size, max_size)));

        let surface_caps = surface.get_capabilities(&wgpu_context.adapter);

        let surface_format = surface_caps
            .formats
            .iter()
            .filter(|f| f.is_srgb())
            .next()
            .unwrap_or(&surface_caps.formats[0])
            .clone();
        let surface_format = surface_format;

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            width: size.width,
            height: size.height,
            desired_maximum_frame_latency: 2,
            present_mode: if render_config.no_vsync {
                wgpu::PresentMode::AutoNoVsync
            } else {
                wgpu::PresentMode::AutoVsync
            },
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            view_formats: vec![],
        };
        surface.configure(&device, &config);

        let ui_renderer = ui_renderer::EguiWGPU::new(device, surface_format, &window);

        let render_format = surface_format;
        let renderer = VolumeRenderer::new(device, render_format);

        let render_settings = RenderSettings {
            clipping_aabb: None,
            time: 0.,
            step_size: 2. / 1000.,
            spatial_filter: wgpu::FilterMode::Nearest,
            temporal_filter: wgpu::FilterMode::Linear,
            distance_scale: render_config.distance_scale,
            vmin: render_config.vmin,
            vmax: render_config.vmax,
            gamma_correction: !surface_format.is_srgb(),
            ..Default::default()
        };

        let mut controller = CameraController::new(0.1, 0.05);
        controller.center = volume.aabb.center();

        let radius = volume.aabb.radius();
        let ratio = size.width as f32 / size.height as f32;
        let camera = Camera::new_aabb_iso(
            volume.aabb.clone(),
            OrthographicProjection::new(Vector2::new(ratio, 1.) * 2. * radius, 1e-4, 100.),
        );

        let animation_duration = render_config
            .duration
            .unwrap_or(Duration::from_secs_f32(10.));

        let volumes_gpu = VolumeGPU::new(device, queue, volume);

        let cmap_gpu = ColorMapGPU::new(&cmap, device, queue, COLORMAP_RESOLUTION);

        let ssao = ssoa::SSAO::new(device, render_format);
        let ssao_textures = SSAOTextures::new(device, config.width, config.height);

        Ok(Self {
            wgpu_context,
            scale_factor: window.scale_factor() as f32,
            window,
            surface,
            config,
            controller,
            ui_renderer,
            ui_visible: true,
            background_color: render_config.background_color,
            camera,

            volume: volumes_gpu,
            renderer,

            render_settings,
            cmap_gpu,
            cmap,
            animation_duration,
            playing: true,
            colormap_editor_visible: render_config.show_colormap_editor,
            volume_info_visible: render_config.show_volume_info,
            #[cfg(feature = "colormaps")]
            cmap_select_visible: render_config.show_cmap_select,
            ssao,
            ssao_textures,
            show_box:false
        })
    }

    fn load_file(&mut self, path: &PathBuf) -> anyhow::Result<()> {
        let reader = std::fs::File::open(path)?;
        let volume = Volume::load(reader)?;
        let volume_gpu =
            VolumeGPU::new(&self.wgpu_context.device, &self.wgpu_context.queue, volume);
        self.volume = volume_gpu;
        // self.controller.center = volume.aabb.center();
        self.camera
            .projection
            .resize(self.config.width, self.config.height);
        Ok(())
    }

    fn resize(&mut self, new_size: winit::dpi::PhysicalSize<u32>, scale_factor: Option<f32>) {
        if new_size.width > 0 && new_size.height > 0 {
            let new_width = new_size.width;
            let new_height = new_size.height;
            self.config.width = new_width;
            self.config.height = new_height;
            self.camera.projection.resize(new_width, new_height);
            self.surface
                .configure(&self.wgpu_context.device, &self.config);
            self.ssao_textures =
                SSAOTextures::new(&self.wgpu_context.device, new_width, new_height);
        }
        if let Some(scale_factor) = scale_factor {
            if scale_factor > 0. {
                self.scale_factor = scale_factor;
            }
        }
    }

    fn update(&mut self, dt: Duration) {
        let old_camera = self.camera.clone();
        self.controller.update_camera(&mut self.camera, dt);
        if !self.camera.visible(self.volume.volume.aabb){
            self.camera = old_camera;
        }

        if self.playing && self.volume.volume.timesteps > 1 {
            self.render_settings.time += dt.as_secs_f32() / self.animation_duration.as_secs_f32();
            self.render_settings.time = self.render_settings.time.fract();
        }
    }

    fn render(&mut self,window:Arc<Window>) -> Result<(), wgpu::SurfaceError> {
        let window_size = self.window.inner_size();
        if window_size.width != self.config.width || window_size.height != self.config.height {
            self.resize(window_size, None);
        }

        let output = self.surface.get_current_texture()?;
        let view_rgb = output.texture.create_view(&wgpu::TextureViewDescriptor {
            format: Some(self.config.format),
            ..Default::default()
        });

        // do prepare stuff

        let mut encoder =
            self.wgpu_context
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("render command encoder"),
                });

        let ui_state = if self.ui_visible {
            self.ui_renderer.begin_frame(&self.window);
            ui::ui(self);

            let shapes = self.ui_renderer.end_frame(&self.window);
            Some(self.ui_renderer.prepare(
                PhysicalSize {
                    width: output.texture.size().width,
                    height: output.texture.size().height,
                },
                self.scale_factor,
                &self.wgpu_context.device,
                &self.wgpu_context.queue,
                &mut encoder,
                shapes,
            ))
        } else {
            None
        };

        let camera = self.camera.clone();
        let frame_data = self.renderer.prepare(
            &self.wgpu_context.device,
            &self.volume,
            &camera,
            &self.render_settings,
            &self.cmap_gpu,
        );

        {
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("render pass"),
                color_attachments: &[
                    Some(wgpu::RenderPassColorAttachment {
                        view: &view_rgb,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(self.background_color),
                            store: wgpu::StoreOp::Store,
                        },
                    }),
                    Some(wgpu::RenderPassColorAttachment {
                        view: &self.ssao_textures.normal_depth,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        },
                    }),
                ],
                ..Default::default()
            });
            self.renderer.render(&mut render_pass, &frame_data);
        }

        if self.render_settings.render_iso && self.render_settings.ssao {
            self.ssao.render(
                &mut encoder,
                &self.wgpu_context.device,
                &self.ssao_textures,
                &self.camera,
                &view_rgb,
            );
        }

        if let Some(state) = &ui_state {
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("render pass ui"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view_rgb,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            self.ui_renderer.render(&mut render_pass, state);
        }

        if let Some(ui_state) = ui_state {
            self.ui_renderer.cleanup(ui_state)
        }
        self.wgpu_context
            .queue
            .submit(std::iter::once(encoder.finish()));
        window.pre_present_notify();
        output.present();
        Ok(())
    }
}

pub async fn open_window(
    window_builder: WindowBuilder,
    volume: Volume,
    cmap: LinearSegmentedColorMap,
    config: RenderConfig,
) {
    let event_loop = EventLoop::new().unwrap();

    let version = env!("CARGO_PKG_VERSION");
    let name = env!("CARGO_PKG_NAME");

    let window = window_builder
        .with_title(format!("{name} {version}"))
        .build(&event_loop)
        .unwrap();

        let min_wait = window.current_monitor().map(|m|{
            let hz = m.refresh_rate_millihertz().unwrap_or(60_000);
            Duration::from_millis(1000000/hz as u64)
        }).unwrap_or(Duration::from_millis(17));
    

    let mut state = WindowContext::new(window, volume, cmap, &config)
        .await
        .unwrap();


    let mut last = Instant::now();



    event_loop.run(move |event,target|
        
        match event {
            Event::NewEvents(e) =>  match e{
                winit::event::StartCause::ResumeTimeReached { .. }=>{
                    state.window.request_redraw();
                }
                _=>{}
            }
        Event::WindowEvent {
            ref event,
            window_id,
        } if window_id == state.window.id() && !state.ui_renderer.on_event(&state.window,event) => match event {
            WindowEvent::Resized(physical_size) => {
                state.resize(*physical_size, None);
            }
            WindowEvent::ScaleFactorChanged {
                scale_factor,
                ..
            } => {
                state.scale_factor = *scale_factor as f32;
            }
            WindowEvent::CloseRequested => {log::info!("close!");target.exit()},
            WindowEvent::ModifiersChanged(m)=>{
                state.controller.alt_pressed = m.state().alt_key();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if let PhysicalKey::Code(key) = event.physical_key{
                    state
                        .controller
                        .process_keyboard(key, event.state == ElementState::Pressed);
                    if key == KeyCode::KeyU && event.state == ElementState::Released{
                        state.ui_visible = !state.ui_visible;
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } => match delta {
                winit::event::MouseScrollDelta::LineDelta(_, dy) => {
                    state.controller.process_scroll(*dy )
                }
                winit::event::MouseScrollDelta::PixelDelta(p) => {
                    state.controller.process_scroll(p.y as f32 / 100.)
                }
            },
            WindowEvent::MouseInput { state:button_state, button, .. }=>{
                match button {
                    winit::event::MouseButton::Left =>                         state.controller.left_mouse_pressed = *button_state == ElementState::Pressed,
                    winit::event::MouseButton::Right => state.controller.right_mouse_pressed = *button_state == ElementState::Pressed,
                    _=>{}
                }
            },
            WindowEvent::DroppedFile(file) => {
                if let Err(e) = state.load_file(file){
                    log::error!("failed to load file: {:?}", e)
                }
            }
            WindowEvent::RedrawRequested => {
                if !config.no_vsync{
                    // make sure the next redraw is called with a small delay
                    target.set_control_flow(ControlFlow::wait_duration(min_wait));
                }
                
                let now = Instant::now();
                let dt = now-last;
                last = now;
                state.update(dt);
    
                match state.render(state.window.clone()) {
                    Ok(_) => {}
                    // Reconfigure the surface if lost
                    Err(wgpu::SurfaceError::Lost) =>{
                        log::error!("lost surface!");
                         state.resize(state.window.inner_size(), None)

                        },
                    // The system is out of memory, we should probably quit
                    Err(wgpu::SurfaceError::OutOfMemory) =>target.exit(),
                    // All other errors (Outdated, Timeout) should be resolved by the next frame
                    Err(e) => println!("error: {:?}", e),
                }
                if config.no_vsync{
                    state.window.request_redraw();
                }
            }
            _ => {}
        },
        Event::DeviceEvent {
            event: DeviceEvent::MouseMotion{ delta, },
            .. // We're not using device_id currently
        } => {
            state.controller.process_mouse(delta.0 as f32, delta.1 as f32)
        }
        Event::AboutToWait => {
            #[cfg(target_arch = "wasm32")]
            use winit::platform::web::WindowExtWebSys;
            #[cfg(target_arch = "wasm32")]
            if let Some(canvas) = state.window.canvas() {
                if canvas.parent_node().is_none() {
                    // The canvas has been removed from the DOM, we should exit
                    target.exit();
                    return;
                }
            }
        }
        _ => {},
    }).unwrap();
    log::info!("exit!");
}
