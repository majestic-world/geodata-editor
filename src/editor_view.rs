//! Native editor renderer for L2J collision context and editable cells.

use std::{
    borrow::Cow,
    cmp::Reverse,
    collections::{BinaryHeap, HashMap, HashSet},
    ops::Range,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
    time::Instant,
};

use bytemuck::{Pod, Zeroable};
use rfd::FileDialog;
use wgpu::util::DeviceExt;
use winit::{
    dpi::{PhysicalPosition, PhysicalSize},
    event::{DeviceEvent, ElementState, Event, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ControlFlow, EventLoop},
    keyboard::{KeyCode, PhysicalKey},
    window::{CursorGrabMode, Window, WindowBuilder},
};

use crate::{
    editor::{self, EditorMemory, EditorOptions, EditorTheme, MapType},
    editor_chrome::{self as chrome, Icon},
    error::{AppError, Result},
    geometry::{Box3, Triangle, Vec3},
    l2j::{self, Direction, Document, EditableBlockType, Layer, LayerAddress, NULL_HEIGHT},
    unreal::{PackageLoader, SourceMap, VisualBatch, VisualBlend, VisualMaterialState},
};

mod loading;
mod overlays;

/// Opens the standalone editor over the map's collision context.
pub fn run_editor(options: EditorOptions) -> Result<()> {
    let memory = editor::load_memory();
    let open_requested = options.input.is_some();
    let source_map = welcome_source_map(&options);
    let document = Document::blank();
    let event_loop = EventLoop::new()
        .map_err(|error| AppError::InvalidData(format!("can't start editor window: {error}")))?;
    let mut editor = pollster::block_on(EditorView::new(
        &event_loop,
        source_map,
        document,
        false,
        0,
        options,
        memory,
    ))?;
    if open_requested {
        editor.open_project();
    }
    event_loop
        .run(move |event, target| {
            target.set_control_flow(ControlFlow::Poll);
            match event {
                Event::WindowEvent { window_id, event }
                    if window_id == editor.preview.window.id() =>
                {
                    let egui_response = editor
                        .preview
                        .egui_state
                        .on_window_event(editor.preview.window.as_ref(), &event);
                    if egui_response.repaint {
                        editor.preview.window.request_redraw();
                    }
                    match event {
                        WindowEvent::CloseRequested => target.exit(),
                        WindowEvent::Resized(size) => editor.preview.resize(size),
                        WindowEvent::ScaleFactorChanged { .. } => {
                            editor.preview.resize(editor.preview.window.inner_size())
                        }
                        WindowEvent::RedrawRequested => {
                            editor.preview.update();
                            match editor.render() {
                                Ok(()) => {}
                                Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                                    editor.preview.resize(editor.preview.size)
                                }
                                Err(wgpu::SurfaceError::OutOfMemory) => target.exit(),
                                Err(wgpu::SurfaceError::Timeout) => {}
                            }
                        }
                        event @ WindowEvent::MouseInput {
                            button: MouseButton::Right,
                            ..
                        } => editor.input(event, false),
                        event @ WindowEvent::MouseInput {
                            button: MouseButton::Left,
                            ..
                        } => editor.input(event, !egui_response.consumed),
                        event if escape_pressed(&event) => editor.input(event, false),
                        event if !egui_response.consumed => editor.input(event, true),
                        _ => {}
                    }
                }
                Event::DeviceEvent { event, .. } => editor.preview.device_input(&event),
                Event::AboutToWait => editor.preview.window.request_redraw(),
                _ => {}
            }
        })
        .map_err(|error| AppError::InvalidData(format!("editor event loop failed: {error}")))
}

fn escape_pressed(event: &WindowEvent) -> bool {
    matches!(
        event,
        WindowEvent::KeyboardInput { event, .. }
            if event.state == ElementState::Pressed
                && !event.repeat
                && matches!(event.physical_key, PhysicalKey::Code(KeyCode::Escape))
    )
}

/// A confirmation the editor is waiting on: the selected client flavour has
/// no package for the region, but the other one does.
///
/// The prompt is drawn by egui rather than by a native dialog on purpose. A
/// blocking `MessageBox` invoked from inside winit's event-loop callback is
/// created with the right owner and geometry but never becomes visible on
/// Windows, so the app just freezes with no prompt in sight.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingFlavour {
    /// Package the user's current selection asked for, and which is absent.
    missing: String,
    /// Package that does exist, and the flavour it belongs to.
    available: String,
    available_type: MapType,
}

impl PendingFlavour {
    fn question(&self) -> String {
        format!(
            "O mapa {} não existe, mas o {} existe. Quer usar ele?",
            self.missing, self.available
        )
    }
}

/// What the client actually ships for a region, before asking anything.
#[derive(Debug, PartialEq, Eq)]
enum PackageAvailability {
    /// The selected map type's package is there.
    Selected(String),
    /// Only the other flavour is there.
    OnlyOther(MapType, String),
    Neither,
}

/// A classic client names a region `25_25_Classic` and a normal one `25_25`,
/// and plenty of clients ship only one of the two for a given region.
fn map_package_availability(
    loader: &PackageLoader,
    region: &str,
    map_type: MapType,
) -> PackageAvailability {
    let selected = map_type.package_name(region);
    if loader.has_package(&selected) {
        return PackageAvailability::Selected(selected);
    }
    let other_type = map_type.other();
    let other = other_type.package_name(region);
    if loader.has_package(&other) {
        return PackageAvailability::OnlyOther(other_type, other);
    }
    PackageAvailability::Neither
}

/// Turns availability into either a package to load right away or a
/// confirmation to put in front of the user.
fn map_package_or_prompt(
    loader: &PackageLoader,
    region: &str,
    map_type: MapType,
) -> std::result::Result<String, Option<PendingFlavour>> {
    match map_package_availability(loader, region, map_type) {
        PackageAvailability::Selected(package) => Ok(package),
        PackageAvailability::Neither => Err(None),
        PackageAvailability::OnlyOther(available_type, available) => Err(Some(PendingFlavour {
            missing: map_type.package_name(region),
            available,
            available_type,
        })),
    }
}

/// Placeholder map shown while no project is loaded.
fn welcome_source_map(options: &EditorOptions) -> SourceMap {
    let name = options
        .input
        .as_ref()
        .and_then(|path| path.file_stem())
        .and_then(|name| name.to_str())
        .unwrap_or("Selecione o cliente e a geodata")
        .to_owned();
    SourceMap {
        name,
        // Only used while the welcome screen is visible. A real map is
        // required before the document can be shown or edited.
        bounds: Box3::new(
            Vec3::new(0.0, -32_768.0, 0.0),
            Vec3::new(32_768.0, 32_768.0, 32_768.0),
        ),
        triangles: Vec::new(),
        geometry: Default::default(),
    }
}

const WINDOW_TITLE: &str = concat!("Geodata Editor By Mk — v", env!("CARGO_PKG_VERSION"));
const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth24Plus;
/// 4x is the universally supported multisample count and already removes
/// nearly all of the aliasing on this scene's long thin silhouettes.
const MSAA_SAMPLE_COUNT: u32 = 4;
const MOUSE_LOOK_SENSITIVITY: f32 = 0.002;
const MOUSE_ELEVATION_SENSITIVITY: f32 = 0.0012;
// Keyboard navigation is intentionally precise; hold Shift to return to the
// original full traversal speed for moving across an entire map.
const NORMAL_MOVE_SPEED: f32 = 0.08;

struct Preview {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    config: wgpu::SurfaceConfiguration,
    size: PhysicalSize<u32>,
    depth_view: wgpu::TextureView,
    msaa_view: wgpu::TextureView,
    triangle_pipeline: wgpu::RenderPipeline,
    triangle_no_cull_pipeline: wgpu::RenderPipeline,
    line_pipeline: wgpu::RenderPipeline,
    geodata_line_pipeline: wgpu::RenderPipeline,
    geodata_overlay_pipeline: wgpu::RenderPipeline,
    nswe_icon_pipeline: wgpu::RenderPipeline,
    nswe_icon_bind_group: wgpu::BindGroup,
    textured_pipelines: Arc<TexturedPipelineResources>,
    material_texture_layout: Arc<wgpu::BindGroupLayout>,
    camera_buffer: wgpu::Buffer,
    camera_bind_group: wgpu::BindGroup,
    camera: Camera,
    input: CameraInput,
    last_frame: Instant,
    source_map: SourceMap,
    collision_meshes: CollisionMeshes,
    ui: PreviewUi,
    egui_context: egui::Context,
    egui_state: egui_winit::State,
    egui_renderer: egui_wgpu::Renderer,
}

impl Preview {
    async fn new(
        event_loop: &EventLoop<()>,
        source_map: SourceMap,
        theme: EditorTheme,
    ) -> Result<Self> {
        let window = Arc::new(
            WindowBuilder::new()
                .with_title(WINDOW_TITLE)
                .with_inner_size(PhysicalSize::new(1440, 1000))
                .with_visible(false)
                .build(event_loop)
                .map_err(|error| {
                    AppError::InvalidData(format!("can't create editor window: {error}"))
                })?,
        );
        let size = window.inner_size();
        let instance = wgpu::Instance::default();
        let surface = instance
            .create_surface(Arc::clone(&window))
            .map_err(|error| AppError::InvalidData(format!("can't create GPU surface: {error}")))?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .ok_or_else(|| AppError::Missing("no compatible graphics adapter found".into()))?;
        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("geodata-editor-device"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::default(),
                },
                None,
            )
            .await
            .map_err(|error| AppError::InvalidData(format!("can't create GPU device: {error}")))?;
        let capabilities = surface.get_capabilities(&adapter);
        let format = capabilities
            .formats
            .iter()
            .copied()
            .find(wgpu::TextureFormat::is_srgb)
            .or_else(|| capabilities.formats.first().copied())
            .ok_or_else(|| AppError::Missing("graphics adapter has no surface format".into()))?;
        let present_mode = capabilities
            .present_modes
            .iter()
            .copied()
            .find(|mode| *mode == wgpu::PresentMode::Fifo)
            .or_else(|| capabilities.present_modes.first().copied())
            .ok_or_else(|| AppError::Missing("graphics adapter has no presentation mode".into()))?;
        let alpha_mode = capabilities
            .alpha_modes
            .first()
            .copied()
            .ok_or_else(|| AppError::Missing("graphics adapter has no alpha mode".into()))?;
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode,
            alpha_mode,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);
        window.set_visible(true);
        let _ = present_loading_screen(&window, &surface, &device, &queue, &config, theme);

        let origin = map_origin(source_map.bounds);
        let camera = Camera::for_bounds(source_map.bounds);
        let camera_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("editor-camera"),
            contents: bytemuck::bytes_of(&CameraUniform::new(
                camera.matrix(config.width, config.height),
                camera.position,
            )),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let camera_layout = create_camera_layout(&device);
        let camera_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("editor-camera-bind-group"),
            layout: &camera_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: camera_buffer.as_entire_binding(),
            }],
        });
        let (triangle_pipeline, triangle_no_cull_pipeline, line_pipeline) =
            create_pipelines(&device, &camera_layout, format);
        let (_, _, geodata_line_pipeline) =
            create_geodata_pipelines(&device, &camera_layout, format);
        let geodata_overlay_pipeline =
            create_geodata_overlay_pipeline(&device, &camera_layout, format);
        let (nswe_icon_pipeline, nswe_icon_bind_group) =
            create_nswe_icon_resources(&device, &queue, &camera_layout, format);
        let material_texture_layout = create_material_texture_layout(&device);
        let textured_pipelines = Arc::new(TexturedPipelineResources::new(
            &device,
            &camera_layout,
            &material_texture_layout,
            format,
        ));
        let depth_view = create_depth_view(&device, &config);
        let msaa_view = create_msaa_view(&device, &config);
        let collision_meshes = CollisionMeshes::new(&device, &source_map, origin);

        let egui_context = egui::Context::default();
        egui_context.set_visuals(egui::Visuals::dark());
        let egui_state = egui_winit::State::new(
            egui_context.clone(),
            egui::ViewportId::ROOT,
            window.as_ref(),
            Some(window.scale_factor() as f32),
            None,
        );
        let egui_renderer =
            egui_wgpu::Renderer::new(&device, format, Some(DEPTH_FORMAT), MSAA_SAMPLE_COUNT);

        Ok(Self {
            window,
            surface,
            device: Arc::new(device),
            queue: Arc::new(queue),
            config,
            size,
            depth_view,
            msaa_view,
            triangle_pipeline,
            triangle_no_cull_pipeline,
            line_pipeline,
            geodata_line_pipeline,
            geodata_overlay_pipeline,
            nswe_icon_pipeline,
            nswe_icon_bind_group,
            textured_pipelines,
            material_texture_layout: Arc::new(material_texture_layout),
            camera_buffer,
            camera_bind_group,
            camera,
            input: CameraInput::default(),
            last_frame: Instant::now(),
            source_map,
            collision_meshes,
            ui: PreviewUi::default(),
            egui_context,
            egui_state,
            egui_renderer,
        })
    }

    fn resize(&mut self, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.size = size;
        self.config.width = size.width;
        self.config.height = size.height;
        self.surface.configure(&self.device, &self.config);
        self.depth_view = create_depth_view(&self.device, &self.config);
        self.msaa_view = create_msaa_view(&self.device, &self.config);
    }

    fn camera_input(&mut self, event: &WindowEvent) {
        if let WindowEvent::KeyboardInput { event, .. } = event {
            if event.state == ElementState::Pressed
                && !event.repeat
                && matches!(event.physical_key, PhysicalKey::Code(KeyCode::KeyM))
            {
                self.ui.wireframe = !self.ui.wireframe;
            }
        }
        self.input.handle(
            event,
            &mut self.camera,
            self.source_map.bounds,
            self.window.as_ref(),
        );
    }

    fn device_input(&mut self, event: &DeviceEvent) {
        self.input.handle_device(event, &mut self.camera);
    }

    fn update(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_frame).as_secs_f32().min(0.1);
        self.last_frame = now;
        self.input.update_camera(&mut self.camera, elapsed);
        let uniform = CameraUniform::new(
            self.camera.matrix(self.config.width, self.config.height),
            self.camera.position,
        );
        self.queue
            .write_buffer(&self.camera_buffer, 0, bytemuck::bytes_of(&uniform));
    }

    fn draw_collision_meshes<'a>(&'a self, pass: &mut wgpu::RenderPass<'a>, lines: bool) {
        let pipeline = if lines {
            &self.line_pipeline
        } else if self.ui.culling {
            &self.triangle_pipeline
        } else {
            &self.triangle_no_cull_pipeline
        };
        for mesh in [
            &self.collision_meshes.terrain,
            &self.collision_meshes.static_meshes,
            &self.collision_meshes.bsp,
            &self.collision_meshes.blocking_volumes,
        ] {
            draw_mesh(pass, pipeline, mesh, &self.camera_bind_group, lines);
        }
    }
}

fn present_loading_screen(
    window: &Window,
    surface: &wgpu::Surface<'_>,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    config: &wgpu::SurfaceConfiguration,
    theme: EditorTheme,
) -> std::result::Result<(), wgpu::SurfaceError> {
    let output = surface.get_current_texture()?;
    let view = output
        .texture
        .create_view(&wgpu::TextureViewDescriptor::default());
    let pixels_per_point = window.scale_factor() as f32;
    let screen_size = egui::vec2(
        config.width as f32 / pixels_per_point,
        config.height as f32 / pixels_per_point,
    );
    let context = egui::Context::default();
    let mut raw_input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, screen_size)),
        ..Default::default()
    };
    if let Some(viewport) = raw_input.viewports.get_mut(&egui::ViewportId::ROOT) {
        viewport.native_pixels_per_point = Some(pixels_per_point);
        viewport.inner_rect = raw_input.screen_rect;
    }
    let full_output = context.run(raw_input, |context| {
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(theme_extreme_bg(theme)))
            .show(context, |ui| {
                ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                    ui.add_space(((ui.available_height() - 110.0) * 0.5).max(0.0));
                    ui.label(
                        egui::RichText::new("GEODATA EDITOR")
                            .size(28.0)
                            .strong()
                            .color(theme_accent(theme)),
                    );
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new("Carregando editor...")
                            .size(15.0)
                            .color(theme_muted_color(theme)),
                    );
                    ui.add_space(18.0);
                    let (bar, _) =
                        ui.allocate_exact_size(egui::vec2(240.0, 3.0), egui::Sense::hover());
                    ui.painter().rect_filled(bar, 0.0, theme_accent(theme));
                });
            });
    });
    let paint_jobs = context.tessellate(full_output.shapes, full_output.pixels_per_point);
    let screen = egui_wgpu::ScreenDescriptor {
        size_in_pixels: [config.width, config.height],
        pixels_per_point,
    };
    let mut renderer = egui_wgpu::Renderer::new(device, config.format, None, 1);
    for (id, delta) in &full_output.textures_delta.set {
        renderer.update_texture(device, queue, *id, delta);
    }
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("editor-loading-frame"),
    });
    let user_commands = renderer.update_buffers(device, queue, &mut encoder, &paint_jobs, &screen);
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("editor-loading-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: 0.0044,
                        g: 0.0065,
                        b: 0.0091,
                        a: 1.0,
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        renderer.render(&mut pass, &paint_jobs, &screen);
    }
    queue.submit(
        user_commands
            .into_iter()
            .chain(std::iter::once(encoder.finish())),
    );
    output.present();
    for id in &full_output.textures_delta.free {
        renderer.free_texture(id);
    }
    Ok(())
}

struct EditorView {
    preview: Preview,
    document: Document,
    loaded: bool,
    has_context: bool,
    package_count: usize,
    max_layer_count: usize,
    /// Logical viewport bounds; rendering and picking use the same rectangle.
    viewport: egui::Rect,
    ui: EditorUi,
    overlays: overlays::OverlayMeshes,
    loading: loading::LoadingState,
    /// Cached GPU form of the textured visualization, built lazily the
    /// first time `ui.textured_view` is enabled for the current project.
    textured_scene: Option<TexturedScene>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum BrushAnchor {
    Left,
    #[default]
    Center,
    Right,
}

impl BrushAnchor {
    const fn label(self) -> &'static str {
        match self {
            Self::Left => "Esquerda",
            Self::Center => "Centro",
            Self::Right => "Direita",
        }
    }

    const fn description(self) -> &'static str {
        match self {
            Self::Left => "O clique inicia à esquerda; a área cresce para a direita e para cima.",
            Self::Center => "A área é distribuída ao redor da célula clicada.",
            Self::Right => "O clique termina à direita; a área cresce para a esquerda e para cima.",
        }
    }
}

#[derive(Clone, Default)]
struct EditorUi {
    open_path: String,
    selection_hidden: bool,
    client_root: String,
    map_type: MapType,
    theme: EditorTheme,
    selected: LayerAddress,
    selection: Vec<LayerAddress>,
    rectangle_start: Option<LayerAddress>,
    line_start: Option<LayerAddress>,
    pending_plain_selection: bool,
    visible_layer: usize,
    brush_width: usize,
    brush_height: usize,
    brush_anchor: BrushAnchor,
    visual_stride: usize,
    show_nswe_icons: bool,
    textured_view: bool,
    show_selected_layer_only: bool,
    show_all_open_cells: bool,
    hide_fully_open_blocks: bool,
    /// Hides every geodata overlay so only the map itself is visible.
    /// Purely a draw-time filter, so toggling it never rebuilds a mesh.
    hide_all_blocks: bool,
    /// Waiting on the user to accept the other client flavour for the
    /// selected region, drawn as an in-app prompt.
    pending_flavour: Option<PendingFlavour>,
    open_context_radius: usize,
    height_input: i32,
    height_input_address: Option<LayerAddress>,
    keyboard_height_adjust: bool,
    last_cursor: Option<PhysicalPosition<f64>>,
    status: String,
}

#[derive(Clone, Copy)]
enum EditorAction {
    None,
    OpenProject,
    Save,
    ApplyPreset(u8),
    SetHeight(i32),
    Convert(EditableBlockType),
    Undo,
    Redo,
    RestoreBlock,
}

#[derive(Clone, Copy)]
struct EditorOverlayOptions {
    selected: LayerAddress,
    open_context_radius: usize,
    show_all_open_cells: bool,
    hide_fully_open_blocks: bool,
    show_selected_layer_only: bool,
}

impl EditorOverlayOptions {
    fn from_ui(ui: &EditorUi) -> Self {
        Self {
            selected: ui.selected,
            open_context_radius: ui.open_context_radius,
            show_all_open_cells: ui.show_all_open_cells,
            hide_fully_open_blocks: ui.hide_fully_open_blocks,
            show_selected_layer_only: ui.show_selected_layer_only,
        }
    }

    fn shows_open_cell(self, x: usize, y: usize) -> bool {
        self.show_all_open_cells
            || (x.abs_diff(self.selected.x) <= self.open_context_radius
                && y.abs_diff(self.selected.y) <= self.open_context_radius)
    }

    fn shows_open_block(self, start_x: usize, start_y: usize) -> bool {
        self.show_all_open_cells
            || start_x <= self.selected.x.saturating_add(self.open_context_radius)
                && start_x + 7 >= self.selected.x.saturating_sub(self.open_context_radius)
                && start_y <= self.selected.y.saturating_add(self.open_context_radius)
                && start_y + 7 >= self.selected.y.saturating_sub(self.open_context_radius)
    }

    fn hides_block(self, document: &Document, block_x: usize, block_y: usize) -> bool {
        self.hide_fully_open_blocks && block_is_fully_open(document, block_x, block_y)
    }

    fn shows_layer(self, layer: usize) -> bool {
        !self.show_selected_layer_only || layer == self.selected.layer
    }
}

#[derive(Default)]
struct EditorSelectionLookup {
    cells: HashSet<(usize, usize, usize)>,
    simple_blocks: HashSet<(usize, usize)>,
}

impl EditorSelectionLookup {
    fn new(document: &Document, selection: &[LayerAddress]) -> Self {
        let mut lookup = Self::default();
        for address in selection {
            let block = (address.x / 8, address.y / 8);
            if document.block_type(block.0, block.1) == Some(EditableBlockType::Simple) {
                lookup.simple_blocks.insert(block);
            } else {
                lookup.cells.insert((address.x, address.y, address.layer));
            }
        }
        lookup
    }

    fn contains_cell(&self, x: usize, y: usize, layer: usize) -> bool {
        self.cells.contains(&(x, y, layer))
    }

    fn contains_simple_block(&self, block_x: usize, block_y: usize) -> bool {
        self.simple_blocks.contains(&(block_x, block_y))
    }

    fn sampled_area_contains_cell(
        &self,
        start_x: usize,
        start_y: usize,
        layer: usize,
        stride: usize,
    ) -> bool {
        let end_x = start_x.saturating_add(stride).min(l2j::MAP_CELLS);
        let end_y = start_y.saturating_add(stride).min(l2j::MAP_CELLS);
        (start_x..end_x).any(|x| (start_y..end_y).any(|y| self.cells.contains(&(x, y, layer))))
    }
}

fn editor_active_selection_count(ui: &EditorUi) -> usize {
    if ui.selection_hidden {
        0
    } else {
        ui.selection.len().max(1)
    }
}

fn decode_nswe_icon_rgba(bytes: &[u8]) -> ([usize; 2], Vec<u8>) {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder
        .read_info()
        .expect("embedded NSWE icon must be a valid PNG");
    let mut decoded = vec![
        0;
        reader
            .output_buffer_size()
            .expect("embedded NSWE icon needs a bounded decode buffer")
    ];
    let info = reader
        .next_frame(&mut decoded)
        .expect("embedded NSWE icon must decode");
    let input = &decoded[..info.buffer_size()];
    let rgba = match info.color_type {
        png::ColorType::Rgba => input.to_vec(),
        png::ColorType::Rgb => input
            .chunks_exact(3)
            .flat_map(|pixel| [pixel[0], pixel[1], pixel[2], 255])
            .collect(),
        png::ColorType::Grayscale => input
            .iter()
            .flat_map(|value| [*value, *value, *value, 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => input
            .chunks_exact(2)
            .flat_map(|pixel| [pixel[0], pixel[0], pixel[0], pixel[1]])
            .collect(),
        png::ColorType::Indexed => {
            panic!("embedded NSWE icon should expand palette data during decoding")
        }
    };
    ([info.width as usize, info.height as usize], rgba)
}

fn nswe_icon_bytes(mask: u8) -> &'static [u8] {
    match mask & 0x0f {
        0 => include_bytes!("../assets/editor/nswe/nswe-0.png"),
        1 => include_bytes!("../assets/editor/nswe/nswe-1.png"),
        2 => include_bytes!("../assets/editor/nswe/nswe-2.png"),
        3 => include_bytes!("../assets/editor/nswe/nswe-3.png"),
        4 => include_bytes!("../assets/editor/nswe/nswe-4.png"),
        5 => include_bytes!("../assets/editor/nswe/nswe-5.png"),
        6 => include_bytes!("../assets/editor/nswe/nswe-6.png"),
        7 => include_bytes!("../assets/editor/nswe/nswe-7.png"),
        8 => include_bytes!("../assets/editor/nswe/nswe-8.png"),
        9 => include_bytes!("../assets/editor/nswe/nswe-9.png"),
        10 => include_bytes!("../assets/editor/nswe/nswe-10.png"),
        11 => include_bytes!("../assets/editor/nswe/nswe-11.png"),
        12 => include_bytes!("../assets/editor/nswe/nswe-12.png"),
        13 => include_bytes!("../assets/editor/nswe/nswe-13.png"),
        14 => include_bytes!("../assets/editor/nswe/nswe-14.png"),
        _ => include_bytes!("../assets/editor/nswe/nswe-15.png"),
    }
}

impl EditorView {
    async fn new(
        event_loop: &EventLoop<()>,
        source_map: SourceMap,
        document: Document,
        loaded: bool,
        package_count: usize,
        options: EditorOptions,
        memory: EditorMemory,
    ) -> Result<Self> {
        let theme = memory.theme;
        let preview = Preview::new(event_loop, source_map, theme).await?;
        apply_editor_theme(&preview.egui_context, theme);
        let mut ui = EditorUi {
            open_path: options
                .input
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or(memory.geodata_path),
            client_root: options
                .client_root
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or(memory.client_root),
            map_type: options.map_type.unwrap_or(memory.map_type),
            theme,
            brush_width: 1,
            brush_height: 1,
            visual_stride: 1,
            show_nswe_icons: true,
            open_context_radius: 16,
            ..Default::default()
        };
        if loaded {
            ui.status = "Projeto carregado com o contexto de colisão do cliente.".into();
        } else {
            ui.status = "Informe o cliente e a geodata para carregar o projeto.".into();
        }
        let max_layer_count = document.max_layer_count().max(1);
        let overlays = overlays::OverlayMeshes::new(&preview.device);
        let mut view = Self {
            preview,
            document,
            loaded,
            has_context: loaded,
            package_count,
            max_layer_count,
            viewport: egui::Rect::NOTHING,
            ui,
            overlays,
            loading: loading::LoadingState::default(),
            textured_scene: None,
        };
        if view.loaded && view.has_context {
            view.refresh_editor_meshes();
            let _ = view.persist_memory();
        }
        Ok(view)
    }

    fn input(&mut self, event: WindowEvent, canvas_input: bool) {
        if let WindowEvent::CursorMoved { position, .. } = &event {
            self.ui.last_cursor = Some(*position);
        }
        if let WindowEvent::KeyboardInput { event: key, .. } = &event {
            if key.state == ElementState::Pressed {
                if let PhysicalKey::Code(code) = key.physical_key {
                    if canvas_input
                        && self.ui.keyboard_height_adjust
                        && self.loaded
                        && self.has_context
                        && !self.loading.is_project_loading()
                    {
                        let delta = match code {
                            KeyCode::ArrowUp => Some(i32::from(l2j::HEIGHT_STEP)),
                            KeyCode::ArrowDown => Some(-i32::from(l2j::HEIGHT_STEP)),
                            _ => None,
                        };
                        if let Some(delta) = delta {
                            self.sync_height_input();
                            self.apply_height(self.ui.height_input.saturating_add(delta));
                        }
                    }
                    if code == KeyCode::Escape && !key.repeat {
                        self.clear_active_selection();
                    }
                    if !key.repeat {
                        let ctrl = self.preview.input.pressed.contains(&KeyCode::ControlLeft)
                            || self.preview.input.pressed.contains(&KeyCode::ControlRight);
                        let shift = self.preview.input.pressed.contains(&KeyCode::ShiftLeft)
                            || self.preview.input.pressed.contains(&KeyCode::ShiftRight);
                        match (ctrl, shift, code) {
                            (true, false, KeyCode::KeyZ) => self.apply(EditorAction::Undo),
                            (true, false, KeyCode::KeyY) => self.apply(EditorAction::Redo),
                            (true, false, KeyCode::KeyO) => self.apply(EditorAction::OpenProject),
                            (true, _, KeyCode::KeyS) => self.apply(EditorAction::Save),
                            _ => {}
                        }
                    }
                }
            }
        }
        if matches!(
            &event,
            WindowEvent::MouseInput {
                button: MouseButton::Right,
                state: ElementState::Pressed,
                ..
            }
        ) && self.preview.input.left_pressed
        {
            self.ui.pending_plain_selection = false;
            self.ui.rectangle_start = None;
        }
        if !canvas_input
            && matches!(
                &event,
                WindowEvent::MouseInput {
                    button: MouseButton::Left,
                    state: ElementState::Released,
                    ..
                }
            )
        {
            self.ui.pending_plain_selection = false;
            self.ui.rectangle_start = None;
        }
        if canvas_input
            && self.loaded
            && self.has_context
            && !self.loading.is_project_loading()
            && !self.preview.input.uses_left_for_vertical_navigation()
        {
            if let WindowEvent::MouseInput {
                button: MouseButton::Left,
                state,
                ..
            } = &event
            {
                let shift = self.preview.input.pressed.contains(&KeyCode::ShiftLeft)
                    || self.preview.input.pressed.contains(&KeyCode::ShiftRight);
                let ctrl = self.preview.input.pressed.contains(&KeyCode::ControlLeft)
                    || self.preview.input.pressed.contains(&KeyCode::ControlRight);
                match state {
                    ElementState::Pressed if ctrl => {
                        self.ui.pending_plain_selection = false;
                        if let Some(cell) = self.pick() {
                            self.select_line_endpoint(cell);
                        } else {
                            self.ui.status = "Nenhuma célula L2J foi atingida para a linha.".into();
                        }
                    }
                    ElementState::Pressed if shift => {
                        self.ui.pending_plain_selection = false;
                        self.ui.line_start = None;
                        self.ui.rectangle_start = self.pick();
                    }
                    ElementState::Pressed => {
                        // Resolve a normal click on release. If the right
                        // button joins first, the pending selection is
                        // cancelled and the same drag becomes vertical camera
                        // navigation without changing the edited cell.
                        self.ui.pending_plain_selection = true;
                    }
                    ElementState::Released => {
                        let pending_plain_selection =
                            std::mem::take(&mut self.ui.pending_plain_selection);
                        if let (Some(start), Some(end)) =
                            (self.ui.rectangle_start.take(), self.pick())
                        {
                            if start == end {
                                self.toggle_selection(end);
                            } else {
                                self.select_rectangle(start, end);
                            }
                        } else if pending_plain_selection {
                            if let Some(cell) = self.pick() {
                                self.select_brush_area(cell);
                            } else {
                                self.ui.status = "Nenhuma célula L2J foi atingida. Aponte a câmera para a superfície e tente novamente.".into();
                            }
                        }
                    }
                }
            }
        }
        self.preview.camera_input(&event);
    }

    fn render(&mut self) -> std::result::Result<(), wgpu::SurfaceError> {
        self.poll_loading();
        let output = self.preview.surface.get_current_texture()?;
        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let raw_input = self
            .preview
            .egui_state
            .take_egui_input(self.preview.window.as_ref());
        let context = self.preview.egui_context.clone();
        let full_output = context.run(raw_input, |context| self.draw_ui(context));
        if self.loaded && self.overlays.needs_icons(&self.ui) {
            self.refresh_editor_meshes();
        }
        let viewport = viewport_pixels(
            self.viewport,
            self.preview.window.scale_factor() as f32,
            [self.preview.config.width, self.preview.config.height],
        );
        let uniform = CameraUniform::new(
            self.preview
                .camera
                .matrix(viewport.width() as u32, viewport.height() as u32),
            self.preview.camera.position,
        );
        self.preview.queue.write_buffer(
            &self.preview.camera_buffer,
            0,
            bytemuck::bytes_of(&uniform),
        );
        self.preview
            .egui_state
            .handle_platform_output(self.preview.window.as_ref(), full_output.platform_output);
        for (id, delta) in &full_output.textures_delta.set {
            self.preview.egui_renderer.update_texture(
                &self.preview.device,
                &self.preview.queue,
                *id,
                delta,
            );
        }
        let paint_jobs = context.tessellate(full_output.shapes, full_output.pixels_per_point);
        let screen = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [self.preview.config.width, self.preview.config.height],
            pixels_per_point: self.preview.window.scale_factor() as f32,
        };
        let mut encoder =
            self.preview
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("editor-frame"),
                });
        let user_commands = self.preview.egui_renderer.update_buffers(
            &self.preview.device,
            &self.preview.queue,
            &mut encoder,
            &paint_jobs,
            &screen,
        );
        if self.ui.textured_view {
            if let Some(scene) = &mut self.textured_scene {
                scene.update_view(&uniform.view_projection, self.preview.camera.forward());
            }
        }
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("editor-render-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.preview.msaa_view,
                    resolve_target: Some(&view),
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        // The multisampled samples are only needed for the
                        // resolve, never read back afterwards.
                        store: wgpu::StoreOp::Discard,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.preview.depth_view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_viewport(
                viewport.left(),
                viewport.top(),
                viewport.width(),
                viewport.height(),
                0.0,
                1.0,
            );
            if self.has_context {
                match (self.ui.textured_view, &self.textured_scene) {
                    (true, Some(scene)) => {
                        scene.draw(&mut pass, &self.preview.camera_bind_group);
                    }
                    _ => self.preview.draw_collision_meshes(&mut pass, false),
                }
            }
            let blocks_visible = self.loaded && self.has_context && !self.ui.hide_all_blocks;
            if blocks_visible {
                self.overlays.draw_geodata(
                    &mut pass,
                    &self.preview.geodata_overlay_pipeline,
                    &self.preview.camera_bind_group,
                    false,
                );
                // The selected cells already use yellow in the base geodata
                // mesh. Draw only their NSWE glyphs over that same surface.
                if self.ui.show_nswe_icons {
                    self.overlays.draw_icons(
                        &mut pass,
                        &self.preview.nswe_icon_pipeline,
                        &self.preview.camera_bind_group,
                        &self.preview.nswe_icon_bind_group,
                    );
                }
            }
            if self.preview.ui.wireframe {
                if self.has_context {
                    self.preview.draw_collision_meshes(&mut pass, true);
                }
                if blocks_visible {
                    self.overlays.draw_geodata(
                        &mut pass,
                        &self.preview.geodata_line_pipeline,
                        &self.preview.camera_bind_group,
                        true,
                    );
                }
            }
            pass.set_viewport(
                0.0,
                0.0,
                self.preview.config.width as f32,
                self.preview.config.height as f32,
                0.0,
                1.0,
            );
            self.preview
                .egui_renderer
                .render(&mut pass, &paint_jobs, &screen);
        }
        self.preview.queue.submit(
            user_commands
                .into_iter()
                .chain(std::iter::once(encoder.finish())),
        );
        output.present();
        for id in &full_output.textures_delta.free {
            self.preview.egui_renderer.free_texture(id);
        }
        Ok(())
    }

    fn draw_ui(&mut self, context: &egui::Context) {
        let mut action = EditorAction::None;
        let mut visual_changed = false;
        egui::TopBottomPanel::top("editor_menubar")
            .exact_height(30.0)
            .frame(
                egui::Frame::none()
                    .fill(theme_extreme_bg(self.ui.theme))
                    .inner_margin(egui::Margin::symmetric(12.0, 2.0)),
            )
            .show(context, |ui| self.draw_editor_menu(ui, &mut action));
        egui::TopBottomPanel::top("editor_toolbar")
            .exact_height(46.0)
            .frame(
                egui::Frame::side_top_panel(&context.style())
                    .fill(chrome::toolbar_background(self.ui.theme)),
            )
            .show(context, |ui| self.draw_editor_toolbar(ui, &mut action));
        egui::TopBottomPanel::bottom("editor_status")
            .exact_height(56.0)
            .show(context, |ui| self.draw_editor_status(ui));
        egui::SidePanel::right("editor_inspector")
            .default_width(352.0)
            .min_width(320.0)
            .max_width(480.0)
            .resizable(true)
            .frame(
                egui::Frame::side_top_panel(&context.style())
                    .inner_margin(egui::Margin::symmetric(12.0, 8.0)),
            )
            .show(context, |ui| {
                ui.horizontal(|ui| {
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::hover());
                    chrome::paint_icon(
                        ui.painter(),
                        rect,
                        Icon::Layers,
                        theme_accent(self.ui.theme),
                    );
                    ui.label(egui::RichText::new("Detalhes").strong());
                });
                let line = ui.available_rect_before_wrap();
                ui.painter().line_segment(
                    [line.left_top(), line.left_top() + egui::vec2(88.0, 0.0)],
                    egui::Stroke::new(2.0_f32, theme_accent(self.ui.theme)),
                );
                ui.add_space(8.0);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        self.draw_editor_inspector(ui, &mut action, &mut visual_changed)
                    });
            });
        egui::TopBottomPanel::top("editor_viewport_toolbar")
            .exact_height(36.0)
            .frame(
                egui::Frame::side_top_panel(&context.style())
                    .fill(chrome::toolbar_background(self.ui.theme)),
            )
            .show(context, |ui| {
                egui::ScrollArea::horizontal()
                    .id_source("viewport_controls")
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let (rect, _) = ui
                                .allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
                            chrome::paint_icon(
                                ui.painter(),
                                rect,
                                Icon::Cube,
                                ui.visuals().text_color(),
                            );
                            ui.label("Perspectiva");
                            ui.separator();
                            chrome::toggle(
                                ui,
                                Icon::Cube,
                                "Wireframe",
                                &mut self.preview.ui.wireframe,
                            )
                            .on_hover_text("Exibir as arestas da geometria");
                            chrome::toggle(ui, Icon::Eye, "Culling", &mut self.preview.ui.culling)
                                .on_hover_text("Ocultar faces voltadas para trás");
                            chrome::toggle(ui, Icon::Compass, "NSWE", &mut self.ui.show_nswe_icons)
                                .on_hover_text("Exibir direções de passagem nas células");
                            if chrome::toggle(ui, Icon::Grid, "Textura", &mut self.ui.textured_view)
                                .changed()
                                && self.ui.textured_view
                                && self.textured_scene.is_none()
                            {
                                self.enable_textured_view();
                            }
                        });
                    });
            });
        self.viewport = context.available_rect();
        if !self.loaded {
            egui::CentralPanel::default()
                .frame(egui::Frame::none())
                .show(context, |ui| {
                    if chrome::empty_viewport(ui, self.ui.theme) {
                        action = EditorAction::OpenProject;
                    }
                });
        }
        self.draw_flavour_prompt(context, &mut action);
        self.apply(action);
        if visual_changed && self.loaded && self.has_context {
            self.refresh_editor_meshes();
        }
    }

    fn draw_editor_menu(&mut self, ui: &mut egui::Ui, action: &mut EditorAction) {
        egui::menu::bar(ui, |ui| {
            ui.label(
                egui::RichText::new("GE")
                    .strong()
                    .color(theme_accent(self.ui.theme)),
            );
            ui.separator();
            ui.menu_button("Projeto", |ui| {
                if ui.button("Abrir projeto     Ctrl+O").clicked() {
                    *action = EditorAction::OpenProject;
                    ui.close_menu();
                }
                if ui
                    .add_enabled(
                        self.loaded,
                        egui::Button::new("Salvar               Ctrl+S"),
                    )
                    .clicked()
                {
                    *action = EditorAction::Save;
                    ui.close_menu();
                }
            });
            ui.menu_button("Edição", |ui| {
                if ui
                    .add_enabled(self.loaded, egui::Button::new("Desfazer     Ctrl+Z"))
                    .clicked()
                {
                    *action = EditorAction::Undo;
                    ui.close_menu();
                }
                if ui
                    .add_enabled(self.loaded, egui::Button::new("Refazer       Ctrl+Y"))
                    .clicked()
                {
                    *action = EditorAction::Redo;
                    ui.close_menu();
                }
            });
            ui.menu_button("Visualização", |ui| {
                ui.checkbox(&mut self.preview.ui.wireframe, "Wireframe");
                ui.checkbox(&mut self.preview.ui.culling, "Culling");
                ui.checkbox(&mut self.ui.show_nswe_icons, "Direções NSWE");
                if ui
                    .checkbox(&mut self.ui.textured_view, "Texturas do cliente")
                    .changed()
                    && self.ui.textured_view
                    && self.textured_scene.is_none()
                {
                    self.enable_textured_view();
                }
            });
            ui.menu_button("Ajuda", |ui| {
                ui.set_max_width(340.0);
                ui.label(egui::RichText::new("Navegação no mapa").strong());
                ui.label("W A S D · mover / Q E · descer e subir");
                ui.label("Botão direito + arrastar · olhar");
                ui.label("Roda do mouse · aproximar / Shift · acelerar");
                ui.label("Home · restaurar câmera");
                ui.separator();
                ui.label("Clique · selecionar / Shift · adicionar");
                ui.label("Ctrl · seguir faixa / Esc · limpar seleção");
                ui.separator();
                ui.small(concat!(
                    "Geodata Editor by Mk · v",
                    env!("CARGO_PKG_VERSION")
                ));
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    egui::RichText::new("GEODATA EDITOR  /  LINEAGE II")
                        .small()
                        .weak(),
                );
            });
        });
    }

    /// In-app confirmation for the missing map flavour.
    ///
    /// Drawn with egui instead of a native message box: a blocking
    /// `MessageBox` called from inside winit's event-loop callback never
    /// becomes visible on Windows, which freezes the editor behind a prompt
    /// nobody can see.
    fn draw_flavour_prompt(&mut self, context: &egui::Context, action: &mut EditorAction) {
        let Some(pending) = self.ui.pending_flavour.clone() else {
            return;
        };
        egui::Window::new("Mapa não encontrado")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(context, |ui| {
                ui.set_max_width(420.0);
                ui.label(pending.question());
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui
                        .button(format!("Usar {}", pending.available))
                        .on_hover_text("Troca o tipo de mapa e carrega o projeto.")
                        .clicked()
                    {
                        self.ui.map_type = pending.available_type;
                        self.ui.pending_flavour = None;
                        *action = EditorAction::OpenProject;
                    }
                    if ui.button("Cancelar").clicked() {
                        self.ui.pending_flavour = None;
                        self.ui.status = format!(
                            "O mapa {} não existe no cliente informado.",
                            pending.missing
                        );
                    }
                });
            });
    }

    fn draw_editor_toolbar(&mut self, ui: &mut egui::Ui, action: &mut EditorAction) {
        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
            if chrome::icon_button(ui, Icon::Folder, "Abrir projeto", false)
                .on_hover_text("Abrir projeto · Ctrl+O")
                .clicked()
            {
                *action = EditorAction::OpenProject;
            }
            ui.add_enabled_ui(self.loaded && !self.loading.is_project_loading(), |ui| {
                if chrome::icon_button(ui, Icon::Save, "Salvar", false)
                    .on_hover_text("Salvar geodata · Ctrl+S")
                    .clicked()
                {
                    *action = EditorAction::Save;
                }
            });
            ui.separator();
            ui.add_enabled_ui(self.loaded && !self.loading.is_project_loading(), |ui| {
                if chrome::icon_button(ui, Icon::Undo, "", false)
                    .on_hover_text("Desfazer · Ctrl+Z")
                    .clicked()
                {
                    *action = EditorAction::Undo;
                }
                if chrome::icon_button(ui, Icon::Redo, "", false)
                    .on_hover_text("Refazer · Ctrl+Y")
                    .clicked()
                {
                    *action = EditorAction::Redo;
                }
            });
            ui.separator();
            if self.loading.is_busy() {
                ui.spinner();
                ui.label("Carregando...");
            }
            let name = if self.loaded {
                self.preview.source_map.name.as_str()
            } else {
                "Nenhum projeto aberto"
            };
            ui.add(egui::Label::new(egui::RichText::new(name).weak()).truncate(true));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_enabled_ui(!self.loading.is_busy(), |ui| {
                    if chrome::icon_button(ui, Icon::Sun, self.ui.theme.toggled().label(), false)
                        .on_hover_text("Alternar tema do editor")
                        .clicked()
                    {
                        self.ui.theme = self.ui.theme.toggled();
                        apply_editor_theme(ui.ctx(), self.ui.theme);
                        if let Err(error) = self.persist_memory() {
                            self.ui
                                .status
                                .push_str(&format!(" Aviso: memória não salva: {error}"));
                        }
                    }
                });
            });
        });
    }

    fn draw_editor_inspector(
        &mut self,
        ui: &mut egui::Ui,
        action: &mut EditorAction,
        visual_changed: &mut bool,
    ) {
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new(if self.loaded && self.has_context {
                "Projeto aberto"
            } else {
                "Aguardando projeto"
            })
            .small()
            .color(if self.loaded && self.has_context {
                theme_success_color(self.ui.theme)
            } else {
                theme_muted_color(self.ui.theme)
            }),
        );
        ui.separator();

        ui.add_enabled_ui(
            !self.loading.is_busy() && self.ui.pending_flavour.is_none(),
            |ui| {
                inspector_section(ui, "Projeto", !self.loaded, |ui| {
                    self.draw_project_section(ui, action)
                });
            },
        );
        ui.add_enabled_ui(self.loaded && !self.loading.is_project_loading(), |ui| {
            inspector_section(ui, "Seleção", true, |ui| {
                self.draw_selection_section(ui, visual_changed)
            });
            inspector_section(ui, "Passabilidade", true, |ui| {
                self.draw_passability_section(ui, action, visual_changed)
            });
            inspector_section(ui, "Bloco", false, |ui| self.draw_block_section(ui, action));
            inspector_section(ui, "Visualização", false, |ui| {
                self.draw_visualization_section(ui, visual_changed)
            });
        });
    }

    fn draw_project_section(&mut self, ui: &mut egui::Ui, action: &mut EditorAction) {
        ui.horizontal(|ui| {
            ui.label("Cliente");
            if chrome::icon_button(ui, Icon::Folder, "Escolher pasta...", false).clicked() {
                if let Some(path) = pick_client_directory(&self.ui.client_root) {
                    self.ui.client_root = path.display().to_string();
                    self.ui.status = "Cliente selecionado. Escolha o tipo e a geodata.".into();
                }
            }
        });
        selected_path_label(
            ui,
            &self.ui.client_root,
            "Nenhuma pasta de cliente selecionada.",
        );
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("Tipo");
            egui::ComboBox::from_id_source("editor_map_type")
                .selected_text(self.ui.map_type.label())
                .show_ui(ui, |ui| {
                    for map_type in MapType::ALL {
                        ui.selectable_value(&mut self.ui.map_type, map_type, map_type.label());
                    }
                });
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("Geodata");
            if chrome::icon_button(ui, Icon::Folder, "Escolher geodata...", false).clicked() {
                if let Some(path) = pick_geodata_file(&self.ui.open_path) {
                    self.ui.open_path = path.display().to_string();
                    self.ui.status = "Geodata selecionada. Abra o projeto quando terminar.".into();
                }
            }
        });
        selected_path_label(ui, &self.ui.open_path, "Nenhuma geodata selecionada.");
        ui.add_space(4.0);
        match self.map_package() {
            Some(package) => {
                ui.small(format!("Mapa do cliente: Maps/{package}.unr"));
            }
            None => {
                ui.small(egui::RichText::new("O mapa do cliente vem do nome da geodata.").weak());
            }
        }
        ui.add_space(6.0);
        if chrome::primary_button(ui, Icon::Layers, "Carregar projeto").clicked() {
            *action = EditorAction::OpenProject;
        }
        ui.label(
            egui::RichText::new(
                "Salvar substitui a geodata aberta após criar uma cópia .<ext>.bak.",
            )
            .size(egui::TextStyle::Small.resolve(ui.style()).size + 2.0),
        );
    }

    fn draw_selection_section(&mut self, ui: &mut egui::Ui, visual_changed: &mut bool) {
        egui::Grid::new("editor_selection_coordinates")
            .num_columns(2)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                ui.label("Geo X");
                *visual_changed |= ui
                    .add(egui::DragValue::new(&mut self.ui.selected.x).clamp_range(0..=2047))
                    .changed();
                ui.end_row();
                ui.label("Geo Y");
                *visual_changed |= ui
                    .add(egui::DragValue::new(&mut self.ui.selected.y).clamp_range(0..=2047))
                    .changed();
                ui.end_row();
                ui.label("Camada");
                self.ui.visible_layer = self
                    .ui
                    .visible_layer
                    .min(self.max_layer_count.saturating_sub(1));
                let previous_layer = self.ui.visible_layer;
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_source("editor_layer_picker")
                        .selected_text(format!("L{}", self.ui.visible_layer))
                        .show_ui(ui, |ui| {
                            for layer in 0..self.max_layer_count {
                                ui.selectable_value(
                                    &mut self.ui.visible_layer,
                                    layer,
                                    format!("L{layer}"),
                                );
                            }
                        });
                    *visual_changed |= ui
                        .checkbox(&mut self.ui.show_selected_layer_only, "Só selecionada")
                        .changed();
                });
                *visual_changed |= self.ui.visible_layer != previous_layer;
            });
        self.ui.selected.layer = self.ui.visible_layer;
        ui.add_space(4.0);
        egui::Grid::new("editor_selection_brush")
            .num_columns(2)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                ui.label("Pincel X");
                ui.add(
                    egui::DragValue::new(&mut self.ui.brush_width)
                        .clamp_range(1..=256)
                        .speed(1),
                );
                ui.end_row();
                ui.label("Pincel Y");
                ui.add(
                    egui::DragValue::new(&mut self.ui.brush_height)
                        .clamp_range(1..=256)
                        .speed(1),
                );
                ui.end_row();
                ui.label("Origem");
                egui::ComboBox::from_id_source("editor_brush_anchor")
                    .selected_text(self.ui.brush_anchor.label())
                    .show_ui(ui, |ui| {
                        for anchor in [BrushAnchor::Left, BrushAnchor::Center, BrushAnchor::Right] {
                            ui.selectable_value(&mut self.ui.brush_anchor, anchor, anchor.label());
                        }
                    });
                ui.end_row();
            });
        ui.small(format!(
            "{} × {} = {} células. {}",
            self.ui.brush_width,
            self.ui.brush_height,
            self.ui.brush_width * self.ui.brush_height,
            self.ui.brush_anchor.description(),
        ));
        ui.horizontal(|ui| {
            ui.label("Detalhe L2J");
            for stride in [1, 2, 4, 8] {
                *visual_changed |= ui
                    .selectable_value(&mut self.ui.visual_stride, stride, format!("1:{stride}"))
                    .changed();
            }
        });
        ui.add_space(3.0);
        let block_x = self.ui.selected.x / 8;
        let block_y = self.ui.selected.y / 8;
        let block_kind = self
            .document
            .block_type(block_x, block_y)
            .unwrap_or(EditableBlockType::Simple);
        let label = if block_kind == EditableBlockType::Simple {
            "Expandir bloco Simple em 64 células"
        } else {
            "Selecionar as 64 células do bloco"
        };
        let response = ui.add_enabled(self.loaded && self.has_context, egui::Button::new(label));
        if response.clicked() {
            let selected = self.select_current_block_cells();
            if selected > 0 {
                *visual_changed = true;
            }
        }
        response.on_hover_text(
            "Seleciona a grade 8×8 do bloco atual na camada visível. Ao aplicar um preset, um bloco Simple é promovido automaticamente para Complex, permitindo alterar as células individualmente.",
        );
        let selection_count = editor_active_selection_count(&self.ui);
        ui.label(
            egui::RichText::new(format!("{} célula(s) ativa(s)", selection_count))
                .color(theme_accent(self.ui.theme)),
        );
    }

    fn draw_passability_section(
        &mut self,
        ui: &mut egui::Ui,
        action: &mut EditorAction,
        visual_changed: &mut bool,
    ) {
        let x = self.ui.selected.x;
        let y = self.ui.selected.y;
        let layer_count = self.document.layer_count(x, y).unwrap_or(0);
        if layer_count > 0 {
            ui.label(egui::RichText::new("Camadas da célula").strong());
            let mut requested_layer = None;
            egui::Grid::new("editor_cell_layers")
                .num_columns(2)
                .spacing([16.0, 3.0])
                .show(ui, |ui| {
                    ui.small("Camada");
                    ui.small("Altura");
                    ui.end_row();
                    for layer_index in 0..layer_count {
                        let address = LayerAddress::new(x, y, layer_index);
                        let active = layer_index == self.ui.visible_layer;
                        if ui
                            .selectable_label(active, format!("L{layer_index}"))
                            .clicked()
                        {
                            requested_layer = Some(layer_index);
                        }
                        ui.monospace(
                            self.document
                                .cell(address)
                                .map(|entry| entry.height.to_string())
                                .unwrap_or_else(|| "—".into()),
                        );
                        ui.end_row();
                    }
                });
            if let Some(layer_index) = requested_layer {
                self.ui.visible_layer = layer_index;
                self.ui.selected.layer = layer_index;
                self.ui.height_input_address = None;
                *visual_changed = true;
            }
            ui.add_space(4.0);
        }
        self.sync_height_input();
        let Some(layer) = self.document.cell(self.ui.selected) else {
            ui.colored_label(
                theme_error_color(self.ui.theme),
                "A camada visível não existe nesta coluna.",
            );
            return;
        };
        let bx = self.ui.selected.x / 8;
        let by = self.ui.selected.y / 8;
        ui.horizontal(|ui| {
            let (rect, response) =
                ui.allocate_exact_size(egui::vec2(60.0, 60.0), egui::Sense::hover());
            ui.painter()
                .rect_filled(rect, 3.0, ui.visuals().extreme_bg_color);
            chrome::paint_nswe(
                ui.painter(),
                rect.shrink(5.0),
                layer.nswe,
                theme_accent(self.ui.theme),
                theme_muted_color(self.ui.theme),
            );
            response.on_hover_text(format!(
                "NSWE {:04b} — N {}  S {}  W {}  E {}",
                layer.nswe & 0x0f,
                if layer.nswe & Direction::North.bit() != 0 {
                    "aberto"
                } else {
                    "bloqueado"
                },
                if layer.nswe & Direction::South.bit() != 0 {
                    "aberto"
                } else {
                    "bloqueado"
                },
                if layer.nswe & Direction::West.bit() != 0 {
                    "aberto"
                } else {
                    "bloqueado"
                },
                if layer.nswe & Direction::East.bit() != 0 {
                    "aberto"
                } else {
                    "bloqueado"
                },
            ));
            ui.vertical(|ui| {
                ui.label(format!("Altura: {}", layer.height));
                ui.label(format!("NSWE: {:04b}", layer.nswe));
                ui.label(format!(
                    "Camadas: {}",
                    self.document
                        .layer_count(self.ui.selected.x, self.ui.selected.y)
                        .unwrap_or(0)
                ));
            });
        });
        ui.add_space(4.0);
        ui.label(egui::RichText::new("Editar altura").strong());
        ui.horizontal(|ui| {
            ui.label("Nova altura");
            ui.add(
                egui::DragValue::new(&mut self.ui.height_input)
                    .speed(f64::from(l2j::HEIGHT_STEP))
                    .clamp_range(
                        i32::from(l2j::MIN_EDITABLE_HEIGHT)..=i32::from(l2j::MAX_EDITABLE_HEIGHT),
                    ),
            );
            if ui.button("Aplicar").clicked() {
                *action = EditorAction::SetHeight(self.ui.height_input);
            }
        });
        ui.checkbox(
            &mut self.ui.keyboard_height_adjust,
            "Ajustar altura com as setas Cima/Baixo",
        )
        .on_hover_text(
            "Com o canvas do mapa em foco, a seta Cima sobe e a seta Baixo desce em 8 unidades. A alteração é aplicada à célula, pincel ou seleção ativa.",
        );
        ui.small("Valores são ajustados para múltiplos de 8 e aplicados às células ativas.");
        ui.label(
            egui::RichText::new(format!(
                "Bloco {bx},{by} · {:?}{}",
                self.document
                    .block_type(bx, by)
                    .unwrap_or(EditableBlockType::Simple),
                if self.document.is_block_dirty(bx, by) {
                    " · alterado"
                } else {
                    ""
                }
            ))
            .size(egui::TextStyle::Small.resolve(ui.style()).size + 2.0),
        );
        ui.add_space(4.0);
        ui.label(egui::RichText::new("Preset de passabilidade").strong());
        ui.label(
            egui::RichText::new("Selecione as células no mapa e clique no padrão NSWE a aplicar.")
                .size(egui::TextStyle::Small.resolve(ui.style()).size + 2.0),
        );
        let selected_mask = layer.nswe & 0x0f;
        ui.horizontal_wrapped(|ui| {
            for mask in NSWE_PRESET_MASKS {
                let response = draw_nswe_preset_button(ui, mask, mask == selected_mask);
                if response.clicked() {
                    *action = EditorAction::ApplyPreset(mask);
                }
                response.on_hover_text(nswe_preset_tooltip(mask));
            }
        });
    }

    fn draw_block_section(&mut self, ui: &mut egui::Ui, action: &mut EditorAction) {
        let bx = self.ui.selected.x / 8;
        let by = self.ui.selected.y / 8;
        ui.label(format!(
            "Tipo atual: {:?}",
            self.document
                .block_type(bx, by)
                .unwrap_or(EditableBlockType::Simple)
        ));
        let current = self
            .document
            .block_type(bx, by)
            .unwrap_or(EditableBlockType::Simple);
        ui.horizontal(|ui| {
            for (label, kind) in [
                ("Simple", EditableBlockType::Simple),
                ("Complex", EditableBlockType::Complex),
                ("Multiple", EditableBlockType::Multilayer),
            ] {
                if ui.selectable_label(current == kind, label).clicked() {
                    *action = EditorAction::Convert(kind);
                }
            }
        });
        ui.add_space(4.0);
        if ui.button("Restaurar bloco-base").clicked() {
            *action = EditorAction::RestoreBlock;
        }
        ui.small("A restauração descarta somente as alterações do bloco atual.");
    }

    fn draw_visualization_section(&mut self, ui: &mut egui::Ui, visual_changed: &mut bool) {
        // Draw-time only: no mesh rebuild, so this is not a `visual_changed`.
        ui.checkbox(&mut self.ui.hide_all_blocks, "Ocultar todos os blocos")
            .on_hover_text("Exibe apenas o mapa, sem nenhum bloco de geodata.");
        ui.add_enabled_ui(!self.ui.hide_all_blocks, |ui| {
            self.draw_block_visibility_options(ui, visual_changed);
        });
        ui.small("Mouse direito: olhar · esquerdo + direito: subir/descer.");
        ui.add_space(4.0);
        ui.label(format!(
            "{} blocos alterados",
            self.document.changed_blocks()
        ));
        if self.has_context {
            ui.small(format!("{} pacotes de contexto", self.package_count));
        }
    }

    fn draw_block_visibility_options(&mut self, ui: &mut egui::Ui, visual_changed: &mut bool) {
        *visual_changed |= ui
            .checkbox(
                &mut self.ui.hide_fully_open_blocks,
                "Ocultar blocos 100% livres",
            )
            .on_hover_text(
                "Oculta blocos Simple e todas as células sem qualquer direção bloqueada.",
            )
            .changed();
        ui.add_enabled_ui(!self.ui.hide_fully_open_blocks, |ui| {
            *visual_changed |= ui
                .checkbox(
                    &mut self.ui.show_all_open_cells,
                    "Mostrar toda a geodata aberta",
                )
                .changed();
            if !self.ui.show_all_open_cells {
                ui.horizontal(|ui| {
                    ui.label("Contexto aberto");
                    for radius in [8, 16, 32, 64] {
                        *visual_changed |= ui
                            .selectable_value(
                                &mut self.ui.open_context_radius,
                                radius,
                                radius.to_string(),
                            )
                            .changed();
                    }
                });
            }
        });
        if self.ui.hide_fully_open_blocks {
            ui.small("Somente células com algum bloqueio permanecem visíveis.");
        }
    }

    fn draw_editor_status(&self, ui: &mut egui::Ui) {
        ui.spacing_mut().item_spacing.y = 2.0;
        let [camera_x, camera_y, camera_z] =
            camera_location(self.preview.source_map.bounds, self.preview.camera.position);
        let bx = self.ui.selected.x / 8;
        let by = self.ui.selected.y / 8;
        let map_name = if self.loaded {
            self.preview.source_map.name.as_str()
        } else {
            "Sem projeto"
        };
        let summary_width = ui.available_width();
        ui.allocate_ui_with_layout(
            egui::vec2(summary_width, 20.0),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.label(egui::RichText::new(format!("MAPA: {map_name}")).strong());
                ui.separator();
                ui.label(format!(
                    "GEO: {},{} L{}",
                    self.ui.selected.x, self.ui.selected.y, self.ui.visible_layer
                ));
                ui.separator();
                ui.label(format!("BLOCO: {bx},{by}"));
                ui.separator();
                ui.label(format!(
                    "SELEÇÃO: {}",
                    editor_active_selection_count(&self.ui)
                ));
                ui.separator();
                ui.label(format!("ALTERADOS: {}", self.document.changed_blocks()));
                ui.separator();
                ui.monospace(format!("CÂMERA: {camera_x} {camera_y} {camera_z}"));
            },
        );
        ui.separator();
        let status = if self.ui.status.is_empty() {
            "Pronto. Clique para selecionar; Shift adiciona; Ctrl segue uma faixa; Esc limpa."
        } else {
            &self.ui.status
        };
        let status_color = editor_status_color(status, self.ui.theme);
        ui.add_sized(
            [ui.available_width(), 18.0],
            egui::Label::new(egui::RichText::new(status).small().color(status_color))
                .truncate(true),
        )
        .on_hover_text(status);
    }

    fn apply(&mut self, action: EditorAction) {
        if self.loading.is_project_loading()
            && !matches!(action, EditorAction::None | EditorAction::OpenProject)
        {
            return;
        }
        let bx = self.ui.selected.x / 8;
        let by = self.ui.selected.y / 8;
        if !matches!(action, EditorAction::None | EditorAction::OpenProject)
            && (!self.loaded || !self.has_context)
        {
            self.ui.status = "Carregue o cliente e a geodata antes de editar.".into();
            return;
        }
        if self.ui.selection_hidden
            && matches!(
                action,
                EditorAction::RestoreBlock
                    | EditorAction::Convert(_)
                    | EditorAction::ApplyPreset(_)
                    | EditorAction::SetHeight(_)
            )
        {
            self.ui.status = "Selecione uma célula antes de editar.".into();
            return;
        }
        match action {
            EditorAction::None => {}
            EditorAction::OpenProject => self.open_project(),
            EditorAction::Save => self.save_opened_file(),
            EditorAction::Undo => {
                if self.document.undo() {
                    self.after_edit("Operação desfeita.");
                }
            }
            EditorAction::Redo => {
                if self.document.redo() {
                    self.after_edit("Operação refeita.");
                }
            }
            EditorAction::RestoreBlock => match self.document.restore_block(bx, by) {
                Ok(true) => self.after_edit("Bloco restaurado a partir do arquivo-base."),
                Ok(false) => self.ui.status = "O bloco já corresponde ao arquivo-base.".into(),
                Err(error) => self.ui.status = format!("Falha ao restaurar: {error}"),
            },
            EditorAction::Convert(target) => {
                let result = self.document.convert_to_type(bx, by, target);
                self.convert(result);
            }
            EditorAction::ApplyPreset(mask) => self.apply_preset(mask),
            EditorAction::SetHeight(height) => self.apply_height(height),
        }
    }

    fn convert(&mut self, result: Result<bool>) {
        match result {
            Ok(true) => self.after_edit("Conversão aplicada."),
            Ok(false) => self.ui.status = "O bloco já está nesse formato.".into(),
            Err(error) => self.ui.status = format!("Conversão recusada: {error}"),
        }
    }

    fn apply_preset(&mut self, mask: u8) {
        let targets = self.edit_targets();
        if targets.is_empty() {
            self.ui.status = "Selecione uma célula antes de aplicar um preset.".into();
            return;
        }
        let result = self
            .document
            .force_set_nswe(targets, mask, format!("Preset NSWE {mask:04b}"));
        if result.changed_cells == 0 {
            self.ui.status = if result.rejected_links.is_empty() {
                "As células selecionadas já usavam esse preset.".into()
            } else {
                result.rejected_links.join(" | ")
            };
        } else {
            self.after_edit(&format!(
                "Preset NSWE {mask:04b} aplicado em {} células selecionadas.",
                result.changed_cells,
            ));
        }
    }

    fn apply_height(&mut self, requested_height: i32) {
        let targets = self.edit_targets();
        if targets.is_empty() {
            self.ui.status = "Selecione uma célula antes de alterar a altura.".into();
            return;
        }
        match self.document.set_height(
            targets,
            requested_height,
            format!("Altura {requested_height}"),
        ) {
            Ok(result) => {
                self.ui.height_input = i32::from(result.height);
                self.ui.height_input_address = Some(self.ui.selected);
                if result.changed_cells == 0 {
                    self.ui.status = if result.rejected_cells.is_empty() {
                        format!("As células ativas já estão na altura {}.", result.height)
                    } else {
                        result.rejected_cells.join(" | ")
                    };
                } else {
                    self.after_edit(&format!(
                        "Altura {} aplicada: {} células{}.",
                        result.height,
                        result.changed_cells,
                        if result.promoted_blocks == 0 {
                            String::new()
                        } else {
                            format!(
                                ", {} bloco(s) Simple convertido(s) em Complex",
                                result.promoted_blocks
                            )
                        }
                    ));
                }
            }
            Err(error) => self.ui.status = format!("Altura inválida: {error}"),
        }
    }

    fn edit_targets(&self) -> Vec<LayerAddress> {
        if self.ui.selection_hidden {
            return Vec::new();
        }
        if !self.ui.selection.is_empty() {
            return self.ui.selection.clone();
        }
        vec![LayerAddress::new(
            self.ui.selected.x,
            self.ui.selected.y,
            self.ui.visible_layer,
        )]
    }

    fn select_brush_area(&mut self, center: LayerAddress) {
        let selection = brush_area_selection(
            &self.document,
            center,
            self.ui.brush_width,
            self.ui.brush_height,
            self.ui.brush_anchor,
            self.ui.hide_fully_open_blocks,
        );
        if selection.is_empty() {
            self.ui.status = "O pincel não encontrou células editáveis nessa área.".into();
            return;
        }
        self.ui.selected = center;
        self.ui.visible_layer = center.layer;
        self.ui.selection = selection;
        self.ui.selection_hidden = false;
        self.ui.rectangle_start = None;
        self.ui.line_start = None;
        self.ui.height_input_address = None;
        self.ui.status = format!(
            "Área {} × {} ({}) selecionada: {} células.",
            self.ui.brush_width,
            self.ui.brush_height,
            self.ui.brush_anchor.label(),
            self.ui.selection.len(),
        );
        self.refresh_editor_meshes();
    }

    fn select_current_block_cells(&mut self) -> usize {
        let block_x = self.ui.selected.x / 8;
        let block_y = self.ui.selected.y / 8;
        let layer = self.ui.visible_layer;
        let selection = block_layer_selection(&self.document, block_x, block_y, layer);
        if selection.is_empty() {
            self.ui.status = format!(
                "O bloco {block_x},{block_y} não possui a camada L{layer} para selecionar."
            );
            return 0;
        }
        self.ui.selected = selection[0];
        self.ui.height_input_address = None;
        self.ui.selection = selection;
        self.ui.selection_hidden = false;
        self.ui.rectangle_start = None;
        self.ui.line_start = None;
        self.ui.status = format!(
            "Bloco {block_x},{block_y} expandido: {} células selecionadas na camada L{layer}. Escolha um preset de passabilidade para aplicar.",
            self.ui.selection.len()
        );
        self.ui.selection.len()
    }

    fn sync_height_input(&mut self) {
        if self.ui.height_input_address == Some(self.ui.selected) {
            return;
        }
        if let Some(layer) = self.document.cell(self.ui.selected) {
            self.ui.height_input = i32::from(layer.height);
            self.ui.height_input_address = Some(self.ui.selected);
        } else {
            self.ui.height_input_address = None;
        }
    }

    /// Unreal package that backs the selected geodata for the current map type.
    fn map_package(&self) -> Option<String> {
        editor::geodata_region(Path::new(self.ui.open_path.trim()))
            .map(|region| self.ui.map_type.package_name(&region))
    }

    fn save_opened_file(&mut self) {
        if !self.loaded || !self.has_context {
            self.ui.status = "Carregue um projeto antes de salvar.".into();
            return;
        }
        let Some(path) = self.document.original_path().map(Path::to_path_buf) else {
            self.ui.status = "A geodata aberta não possui um arquivo de origem para salvar.".into();
            return;
        };
        match self.document.save_as(&path) {
            Ok(summary) => {
                let file_name = summary
                    .path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("geodata.l2j");
                self.ui.status = format!(
                    "Salvo com sucesso: {file_name}\n{} blocos, {} conversões, {} células, {} direções | {} bytes",
                    summary.changed_blocks,
                    summary.conversion_blocks,
                    summary.changed_cells,
                    summary.changed_directions,
                    summary.bytes,
                );
                if let Err(error) = self.persist_memory() {
                    self.ui
                        .status
                        .push_str(&format!(" Aviso: memória não salva: {error}"));
                }
            }
            Err(error) => self.ui.status = format!("Falha ao salvar: {error}"),
        }
    }

    fn after_edit(&mut self, status: &str) {
        self.ui.status = status.into();
        self.refresh_geodata();
    }

    fn persist_memory(&self) -> Result<()> {
        editor::save_memory(&EditorMemory {
            client_root: self.ui.client_root.trim().to_owned(),
            geodata_path: self.ui.open_path.trim().to_owned(),
            map_type: self.ui.map_type,
            theme: self.ui.theme,
        })
    }

    fn refresh_geodata(&mut self) {
        self.refresh_editor_meshes();
    }

    fn clear_active_selection(&mut self) {
        if self.ui.selection_hidden || !self.loaded || !self.has_context {
            return;
        }
        self.ui.selection_hidden = true;
        self.ui.selection.clear();
        self.ui.rectangle_start = None;
        self.ui.line_start = None;
        self.ui.pending_plain_selection = false;
        self.refresh_editor_meshes();
        self.ui.status = "Seleção removida.".into();
    }

    fn refresh_editor_meshes(&mut self) {
        self.overlays.refresh(
            &self.preview.device,
            &self.preview.queue,
            &self.preview.source_map,
            &mut self.document,
            &self.ui,
        );
    }

    fn pick(&self) -> Option<LayerAddress> {
        let position = self.ui.last_cursor?;
        let viewport = viewport_pixels(
            self.viewport,
            self.preview.window.scale_factor() as f32,
            [self.preview.config.width, self.preview.config.height],
        );
        let [x, y] = viewport_ndc(viewport, egui::pos2(position.x as f32, position.y as f32))?;
        let width = viewport.width();
        let height = viewport.height();
        let forward = self.preview.camera.forward();
        let right = normalize(cross(forward, [0.0, 1.0, 0.0]));
        let up = cross(right, forward);
        let aspect = width / height;
        let tangent = (55.0_f32.to_radians() * 0.5).tan();
        let ray = normalize(add(
            forward,
            add(scale(right, x * tangent * aspect), scale(up, y * tangent)),
        ));
        if ray[1].abs() < 0.000_1 {
            return None;
        }
        let origin = map_origin(self.preview.source_map.bounds);
        // The preview camera is centred around the map origin, while L2J
        // heights and grid coordinates are in world space.  Walk the ray one
        // L2J cell at a time and test its actual layer height.  The previous
        // two-pass height estimate worked on flat terrain, but could land on a
        // different step (or miss it completely) as soon as the cursor was
        // over stairs, bridges or a multilayer block.
        let ray_origin = [
            self.preview.camera.position[0] + origin.x,
            self.preview.camera.position[1] + origin.y,
            self.preview.camera.position[2] + origin.z,
        ];
        pick_l2j_ray(
            &self.document,
            self.preview.source_map.bounds,
            ray_origin,
            ray,
            self.ui
                .show_selected_layer_only
                .then_some(self.ui.visible_layer),
            self.ui.hide_fully_open_blocks,
        )
    }

    fn select_rectangle(&mut self, start: LayerAddress, end: LayerAddress) {
        const MAX_SELECTION: usize = 65_536;
        let (min_x, max_x) = (start.x.min(end.x), start.x.max(end.x));
        let (min_y, max_y) = (start.y.min(end.y), start.y.max(end.y));
        let mut selection = std::mem::take(&mut self.ui.selection);
        if selection.is_empty() && !self.ui.selection_hidden {
            selection.push(self.ui.selected);
        }
        self.ui.selection_hidden = false;
        let mut selected_cells = selection
            .iter()
            .map(|cell| (cell.x, cell.y, cell.layer))
            .collect::<HashSet<_>>();
        let mut limited = false;
        'rows: for x in min_x..=max_x {
            for y in min_y..=max_y {
                if let Some(count) = self.document.layer_count(x, y) {
                    let cell = LayerAddress::new(x, y, end.layer.min(count.saturating_sub(1)));
                    if selected_cells.insert((cell.x, cell.y, cell.layer)) {
                        if selection.len() == MAX_SELECTION {
                            limited = true;
                            break 'rows;
                        }
                        selection.push(cell);
                    }
                }
            }
        }
        self.ui.selected = end;
        self.ui.visible_layer = end.layer;
        self.ui.selection = selection;
        self.ui.status = if limited {
            format!(
                "Seleção limitada a 65.536 células ({} selecionadas).",
                self.ui.selection.len()
            )
        } else {
            format!("{} células selecionadas.", self.ui.selection.len())
        };
        self.refresh_editor_meshes();
    }

    fn toggle_selection(&mut self, cell: LayerAddress) {
        if self.ui.selection_hidden {
            self.ui.selection.clear();
            self.ui.selection_hidden = false;
        } else if self.ui.selection.is_empty() {
            self.ui.selection.push(self.ui.selected);
        }
        self.ui.selected = cell;
        self.ui.visible_layer = cell.layer;
        if let Some(index) = self.ui.selection.iter().position(|entry| *entry == cell) {
            if self.ui.selection.len() > 1 {
                self.ui.selection.remove(index);
            }
        } else {
            self.ui.selection.push(cell);
        }
        self.ui.status = format!("{} células selecionadas.", self.ui.selection.len());
        self.refresh_editor_meshes();
    }

    fn select_line_endpoint(&mut self, cell: LayerAddress) {
        self.ui.selection_hidden = false;
        let Some(start) = self.ui.line_start.take() else {
            self.ui.line_start = Some(cell);
            self.ui.selected = cell;
            self.ui.visible_layer = cell.layer;
            self.ui.selection = vec![cell];
            self.ui.status = format!(
                "Início da linha: Geo {},{}. Com Ctrl pressionado, clique no fim.",
                cell.x, cell.y
            );
            self.refresh_editor_meshes();
            return;
        };

        // A straight Bresenham range is a useful fallback, but authored
        // walkable strips commonly bend around scenery or climb a short
        // stair. Prefer the connected strip that has the same visual NSWE
        // class as both endpoints; it still stays in a bounded area around
        // the requested range so it cannot wander through the whole map.
        let (selection, followed_strip) = flexible_line_selection(&self.document, start, cell)
            .map(|selection| (selection, true))
            .unwrap_or_else(|| (straight_line_selection(&self.document, start, cell), false));
        self.ui.selection_hidden = false;
        self.ui.selected = cell;
        self.ui.visible_layer = cell.layer;
        self.ui.selection = selection;
        self.ui.status = format!(
            "{}: Geo {},{} → {},{} ({} células).",
            if followed_strip {
                "Faixa contínua selecionada"
            } else {
                "Linha direta selecionada"
            },
            start.x,
            start.y,
            cell.x,
            cell.y,
            self.ui.selection.len()
        );
        self.refresh_editor_meshes();
    }
}
fn brush_size(requested_size: usize) -> usize {
    requested_size.clamp(1, 256).min(l2j::MAP_CELLS)
}

fn centered_axis_bounds(center: usize, requested_size: usize) -> (usize, usize) {
    let size = brush_size(requested_size);
    let start = center.saturating_sub(size / 2).min(l2j::MAP_CELLS - size);
    (start, start + size)
}

fn forward_axis_bounds(start_pointer: usize, requested_size: usize) -> (usize, usize) {
    let size = brush_size(requested_size);
    let start = start_pointer.min(l2j::MAP_CELLS - size);
    (start, start + size)
}

fn backward_axis_bounds(end_pointer: usize, requested_size: usize) -> (usize, usize) {
    let size = brush_size(requested_size);
    let end = end_pointer.saturating_add(1).max(size).min(l2j::MAP_CELLS);
    (end - size, end)
}

fn brush_area_selection(
    document: &Document,
    center: LayerAddress,
    width: usize,
    height: usize,
    anchor: BrushAnchor,
    hide_fully_open_cells: bool,
) -> Vec<LayerAddress> {
    let ((start_x, end_x), (start_y, end_y)) = match anchor {
        BrushAnchor::Left => (
            forward_axis_bounds(center.x, width),
            backward_axis_bounds(center.y, height),
        ),
        BrushAnchor::Center => (
            centered_axis_bounds(center.x, width),
            centered_axis_bounds(center.y, height),
        ),
        BrushAnchor::Right => (
            backward_axis_bounds(center.x, width),
            backward_axis_bounds(center.y, height),
        ),
    };
    let mut selection = Vec::with_capacity((end_x - start_x) * (end_y - start_y));
    for y in start_y..end_y {
        for x in start_x..end_x {
            let Some(layer_count) = document.layer_count(x, y) else {
                continue;
            };
            if layer_count == 0 {
                continue;
            }
            let address = LayerAddress::new(x, y, center.layer.min(layer_count - 1));
            let Some(layer) = document.cell(address) else {
                continue;
            };
            if layer.height == NULL_HEIGHT || hide_fully_open_cells && layer_is_fully_open(layer) {
                continue;
            }
            selection.push(address);
        }
    }
    selection
}

/// Bresenham over the L2J grid, inclusive at both ends. It is used when no
/// continuous authored strip can be found between the two Ctrl endpoints.
fn rasterized_line(start: LayerAddress, end: LayerAddress) -> Vec<(usize, usize)> {
    let (mut x, mut y) = (start.x as isize, start.y as isize);
    let (end_x, end_y) = (end.x as isize, end.y as isize);
    let delta_x = (end_x - x).abs();
    let delta_y = -(end_y - y).abs();
    let step_x = if x < end_x { 1 } else { -1 };
    let step_y = if y < end_y { 1 } else { -1 };
    let mut error = delta_x + delta_y;
    let mut cells = Vec::with_capacity(delta_x.max((-delta_y) as isize) as usize + 1);
    loop {
        cells.push((x as usize, y as usize));
        if x == end_x && y == end_y {
            break;
        }
        let doubled = error * 2;
        if doubled >= delta_y {
            error += delta_y;
            x += step_x;
        }
        if doubled <= delta_x {
            error += delta_x;
            y += step_y;
        }
    }
    cells
}

fn straight_line_selection(
    document: &Document,
    start: LayerAddress,
    end: LayerAddress,
) -> Vec<LayerAddress> {
    rasterized_line(start, end)
        .into_iter()
        .filter_map(|(x, y)| {
            let layers = document.layer_count(x, y)?;
            Some(LayerAddress::new(
                x,
                y,
                end.layer.min(layers.saturating_sub(1)),
            ))
        })
        .collect()
}

/// Returns the individual L2J cells in one 8×8 block for the requested
/// visible layer.  Simple blocks intentionally expand here even though the
/// renderer shows them as one large quad: an NSWE preset can then promote the
/// block and edit any of its 64 cells independently.
fn block_layer_selection(
    document: &Document,
    block_x: usize,
    block_y: usize,
    layer: usize,
) -> Vec<LayerAddress> {
    if block_x >= 256 || block_y >= 256 {
        return Vec::new();
    }
    let start_x = block_x * 8;
    let start_y = block_y * 8;
    let mut cells = Vec::with_capacity(64);
    for x in start_x..start_x + 8 {
        for y in start_y..start_y + 8 {
            let address = LayerAddress::new(x, y, layer);
            if document.cell(address).is_some() {
                cells.push(address);
            }
        }
    }
    cells
}

/// Coarse state used only for range selection. It matches the three colors in
/// the editor, without requiring every cell in an authored strip to have the
/// exact same directional mask.
fn route_class(layer: Layer) -> u8 {
    match layer.nswe & 0x0f {
        0 => 0,           // blocked / red
        Layer::OPEN => 2, // open / cyan
        _ => 1,           // partially open / orange
    }
}

const FLEX_ROUTE_MARGIN: usize = 64;
const FLEX_ROUTE_MAX_VISITED: usize = 65_536;
const FLEX_ROUTE_MAX_STEP: u16 = 64;
const FLEX_ROUTE_CLASS_CHANGE_COST: u32 = 48;

/// Finds a short cardinal route through one continuous passability family.
/// Open and partial cells may be connected (with a substantial cost) because
/// real routes often switch mask while turning a corner. Blocked cells remain
/// isolated from a walkable route. Cardinal steps deliberately keep a curved
/// row as real L2J cells instead of cutting diagonally across a bend. The
/// state also includes the layer index, so a multilayer cell never silently
/// changes an unrelated ceiling/floor while following a route.
fn flexible_line_selection(
    document: &Document,
    start: LayerAddress,
    end: LayerAddress,
) -> Option<Vec<LayerAddress>> {
    let start_layer = document.cell(start)?;
    let end_layer = document.cell(end)?;
    if start_layer.height == NULL_HEIGHT
        || end_layer.height == NULL_HEIGHT
        || !same_route_family(route_class(start_layer), route_class(end_layer))
    {
        return None;
    }
    if start == end {
        return Some(vec![start]);
    }

    let (map_width, map_height) = Document::dimensions();
    let min_x = start.x.min(end.x).saturating_sub(FLEX_ROUTE_MARGIN);
    let min_y = start.y.min(end.y).saturating_sub(FLEX_ROUTE_MARGIN);
    let max_x = start
        .x
        .max(end.x)
        .saturating_add(FLEX_ROUTE_MARGIN)
        .min(map_width.saturating_sub(1));
    let max_y = start
        .y
        .max(end.y)
        .saturating_add(FLEX_ROUTE_MARGIN)
        .min(map_height.saturating_sub(1));
    let wanted_class = route_class(start_layer);
    let start_key = (start.x, start.y, start.layer);
    let end_key = (end.x, end.y, end.layer);

    // (estimated total, travelled cost, x, y, layer). Reverse turns the
    // standard max heap into a stable min heap without floating point costs.
    let mut frontier = BinaryHeap::new();
    let mut costs = HashMap::new();
    let mut previous = HashMap::new();
    frontier.push(Reverse((
        route_heuristic(start.x, start.y, end.x, end.y),
        0_u32,
        start.x,
        start.y,
        start.layer,
    )));
    costs.insert(start_key, 0_u32);
    let mut visited = 0_usize;

    while let Some(Reverse((_, cost, x, y, layer_index))) = frontier.pop() {
        let key = (x, y, layer_index);
        if costs.get(&key).copied() != Some(cost) {
            continue;
        }
        if key == end_key {
            return reconstruct_route(previous, key);
        }
        visited += 1;
        if visited > FLEX_ROUTE_MAX_VISITED {
            return None;
        }
        let current = document.cell(LayerAddress::new(x, y, layer_index))?;

        for (offset_x, offset_y) in [(0_isize, -1_isize), (0, 1), (-1, 0), (1, 0)] {
            let Some(next_x) = x.checked_add_signed(offset_x) else {
                continue;
            };
            let Some(next_y) = y.checked_add_signed(offset_y) else {
                continue;
            };
            if next_x < min_x || next_x > max_x || next_y < min_y || next_y > max_y {
                continue;
            }
            let Some(layer_count) = document.layer_count(next_x, next_y) else {
                continue;
            };
            for next_layer_index in 0..layer_count {
                let next_address = LayerAddress::new(next_x, next_y, next_layer_index);
                let Some(next) = document.cell(next_address) else {
                    continue;
                };
                let Some(class_cost) = route_class_cost(wanted_class, route_class(next)) else {
                    continue;
                };
                if next.height == NULL_HEIGHT
                    || current.height.abs_diff(next.height) > FLEX_ROUTE_MAX_STEP
                {
                    continue;
                }

                let next_key = (next_x, next_y, next_layer_index);
                let height_cost = u32::from(current.height.abs_diff(next.height)) / 8;
                // Prefer the same mask when both alternatives are present,
                // while permitting turns whose NSWE direction naturally
                // changes along an authored path.
                let mask_cost = u32::from((current.nswe & 0x0f) != (next.nswe & 0x0f));
                let next_cost = cost + 10 + height_cost + mask_cost + class_cost;
                if costs
                    .get(&next_key)
                    .is_some_and(|known| *known <= next_cost)
                {
                    continue;
                }
                costs.insert(next_key, next_cost);
                previous.insert(next_key, key);
                let estimate = next_cost + route_heuristic(next_x, next_y, end.x, end.y);
                frontier.push(Reverse((
                    estimate,
                    next_cost,
                    next_x,
                    next_y,
                    next_layer_index,
                )));
            }
        }
    }
    None
}

fn same_route_family(left: u8, right: u8) -> bool {
    left == right || (left != 0 && right != 0)
}

fn route_class_cost(wanted: u8, candidate: u8) -> Option<u32> {
    if wanted == candidate {
        Some(0)
    } else if wanted != 0 && candidate != 0 {
        // It is still a walkable strip, but preserve a strong preference for
        // the color/mask class selected at the first endpoint.
        Some(FLEX_ROUTE_CLASS_CHANGE_COST)
    } else {
        None
    }
}

fn route_heuristic(x: usize, y: usize, end_x: usize, end_y: usize) -> u32 {
    // The search moves orthogonally, therefore Manhattan distance is an exact
    // admissible lower bound and keeps long rows responsive.
    u32::try_from(x.abs_diff(end_x).saturating_add(y.abs_diff(end_y))).unwrap_or(u32::MAX) * 10
}

fn reconstruct_route(
    previous: HashMap<(usize, usize, usize), (usize, usize, usize)>,
    mut current: (usize, usize, usize),
) -> Option<Vec<LayerAddress>> {
    let mut route = vec![LayerAddress::new(current.0, current.1, current.2)];
    while let Some(&parent) = previous.get(&current) {
        current = parent;
        route.push(LayerAddress::new(current.0, current.1, current.2));
    }
    route.reverse();
    (!route.is_empty()).then_some(route)
}

/// Clamp to the surface even while panels resize or the window is minimized.
fn viewport_pixels(rect: egui::Rect, scale: f32, surface: [u32; 2]) -> egui::Rect {
    let size = egui::vec2(surface[0].max(1) as f32, surface[1].max(1) as f32);
    let min = egui::pos2(
        (rect.left() * scale).floor().clamp(0.0, size.x - 1.0),
        (rect.top() * scale).floor().clamp(0.0, size.y - 1.0),
    );
    let max = egui::pos2(
        (rect.right() * scale).ceil().clamp(min.x + 1.0, size.x),
        (rect.bottom() * scale).ceil().clamp(min.y + 1.0, size.y),
    );
    egui::Rect::from_min_max(min, max)
}

fn viewport_ndc(rect: egui::Rect, point: egui::Pos2) -> Option<[f32; 2]> {
    if !rect.contains(point) {
        return None;
    }
    Some([
        (point.x - rect.left()) / rect.width() * 2.0 - 1.0,
        1.0 - (point.y - rect.top()) / rect.height() * 2.0,
    ])
}

fn theme_extreme_bg(theme: EditorTheme) -> egui::Color32 {
    chrome::background(theme)
}

fn theme_accent(theme: EditorTheme) -> egui::Color32 {
    chrome::accent(theme)
}

/// Secondary text for status messages and supporting details.
fn theme_muted_color(theme: EditorTheme) -> egui::Color32 {
    match theme {
        EditorTheme::Dark => egui::Color32::from_gray(160),
        EditorTheme::Light => egui::Color32::from_rgb(95, 100, 108),
    }
}

/// Success text: "project open", saved/loaded status messages.
fn theme_success_color(theme: EditorTheme) -> egui::Color32 {
    match theme {
        EditorTheme::Dark => egui::Color32::from_rgb(112, 204, 145),
        EditorTheme::Light => egui::Color32::from_rgb(24, 128, 68),
    }
}

/// Error/warning text: failed operations, rejected input, missing layers.
fn theme_error_color(theme: EditorTheme) -> egui::Color32 {
    match theme {
        EditorTheme::Dark => egui::Color32::from_rgb(255, 145, 120),
        EditorTheme::Light => egui::Color32::from_rgb(190, 55, 35),
    }
}

fn apply_editor_theme(context: &egui::Context, theme: EditorTheme) {
    chrome::apply_theme(context, theme);
}

fn inspector_section(
    ui: &mut egui::Ui,
    title: &str,
    open: bool,
    content: impl FnOnce(&mut egui::Ui),
) {
    egui::Frame::none()
        .fill(ui.visuals().faint_bg_color)
        .inner_margin(egui::Margin::symmetric(6.0, 4.0))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            egui::CollapsingHeader::new(egui::RichText::new(title).strong())
                .default_open(open)
                .show(ui, |ui| {
                    ui.add_space(6.0);
                    content(ui);
                    ui.add_space(6.0);
                });
        });
    ui.add_space(2.0);
}

fn editor_status_color(status: &str, theme: EditorTheme) -> egui::Color32 {
    if status.starts_with("Falha") || status.contains("recusada") || status.contains("inválid") {
        theme_error_color(theme)
    } else if status.starts_with("Salvo")
        || status.starts_with("Projeto carregado")
        || status.starts_with("NSWE alterado")
        || status.starts_with("NSWE liberado")
    {
        theme_success_color(theme)
    } else {
        theme_muted_color(theme)
    }
}

// The legacy editors present every NSWE combination as a direct preset, with
// the fully open cell first. Keeping this order makes the common correction
// quick to reach while still exposing all four-direction combinations.
const NSWE_PRESET_MASKS: [u8; 16] = [15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0];

fn nswe_preset_tooltip(mask: u8) -> String {
    let state = |direction: Direction| {
        if mask & direction.bit() != 0 {
            "aberto"
        } else {
            "bloqueado"
        }
    };
    format!(
        "Aplicar preset NSWE {mask:04b}\nN: {} · S: {} · W: {} · E: {}",
        state(Direction::North),
        state(Direction::South),
        state(Direction::West),
        state(Direction::East),
    )
}

/// Vector presets share their direction grammar with the cell summary.
fn draw_nswe_preset_button(ui: &mut egui::Ui, mask: u8, selected: bool) -> egui::Response {
    let response = ui.add(
        egui::Button::new("")
            .min_size(egui::vec2(42.0, 42.0))
            .selected(selected),
    );
    chrome::paint_nswe(
        ui.painter(),
        response.rect.shrink(4.0),
        mask,
        ui.visuals().selection.stroke.color,
        ui.visuals().weak_text_color(),
    );
    response
}

fn selected_path_label(ui: &mut egui::Ui, value: &str, empty_message: &str) {
    let text = if value.trim().is_empty() {
        empty_message
    } else {
        value
    };
    egui::Frame::none()
        .fill(ui.visuals().extreme_bg_color)
        .rounding(3.0)
        .inner_margin(egui::Margin::symmetric(8.0, 5.0))
        .show(ui, |ui| {
            ui.set_min_width((ui.available_width() - 1.0).max(0.0));
            ui.add(egui::Label::new(egui::RichText::new(text).small().weak()).truncate(true))
                .on_hover_text(text);
        });
}

fn dialog_directory(value: &str) -> Option<PathBuf> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let path = PathBuf::from(value);
    if path.is_dir() {
        Some(path)
    } else {
        path.parent()
            .filter(|parent| parent.is_dir())
            .map(Path::to_path_buf)
    }
}

fn pick_client_directory(current: &str) -> Option<PathBuf> {
    let mut dialog = FileDialog::new().set_title("Selecionar cliente Lineage II");
    if let Some(directory) = dialog_directory(current) {
        dialog = dialog.set_directory(directory);
    }
    dialog.pick_folder()
}

fn pick_geodata_file(current: &str) -> Option<PathBuf> {
    let mut dialog = FileDialog::new()
        .set_title("Selecionar geodata")
        .add_filter("Geodata (.l2j, .l2g, _conv.dat)", &["l2j", "l2g", "dat"]);
    if let Some(directory) = dialog_directory(current) {
        dialog = dialog.set_directory(directory);
    }
    dialog.pick_file()
}

fn layer_is_fully_open(layer: Layer) -> bool {
    layer.height != NULL_HEIGHT && layer.nswe & 0x0f == Layer::OPEN
}

fn block_is_fully_open(document: &Document, block_x: usize, block_y: usize) -> bool {
    let Some(block) = document.block(block_x, block_y) else {
        return false;
    };
    if block.kind() == EditableBlockType::Simple {
        return block
            .layers(0)
            .first()
            .copied()
            .is_some_and(layer_is_fully_open);
    }

    let mut has_surface = false;
    for column in 0..l2j::COLUMNS_PER_BLOCK {
        for layer in block.layers(column) {
            if layer.height == NULL_HEIGHT {
                continue;
            }
            has_surface = true;
            if !layer_is_fully_open(*layer) {
                return false;
            }
        }
    }
    has_surface
}

fn editor_geodata_instances_in_blocks(
    map: &SourceMap,
    document: &Document,
    origin: Vec3,
    stride: usize,
    visibility: EditorOverlayOptions,
    selection: &EditorSelectionLookup,
    block_x: Range<usize>,
    block_y: Range<usize>,
) -> CpuGeodata {
    let mut mesh = CpuGeodata::default();
    let stride = stride.max(1);
    for block_x in block_x {
        for block_y in block_y.clone() {
            let start_x = block_x * 8;
            let start_y = block_y * 8;
            if visibility.hides_block(document, block_x, block_y) {
                continue;
            }
            match document.block_type(block_x, block_y) {
                Some(EditableBlockType::Simple) => {
                    let selected = selection.contains_simple_block(block_x, block_y);
                    if !visibility.shows_layer(0)
                        || !selected && !visibility.shows_open_block(start_x, start_y)
                    {
                        continue;
                    }
                    if let Some(layer) = document.cell(LayerAddress::new(start_x, start_y, 0)) {
                        append_editor_geodata_cell(
                            &mut mesh, map, origin, start_x, start_y, layer, 63.4, selected,
                        );
                    }
                }
                Some(EditableBlockType::Complex | EditableBlockType::Multilayer) => {
                    let sampled_scale = 8.0 * stride as f32 - 0.6;
                    for local_x in 0..8 {
                        for local_y in 0..8 {
                            let x = start_x + local_x;
                            let y = start_y + local_y;
                            let layers = document.layer_count(x, y).unwrap_or(0);
                            for layer in 0..layers {
                                if !visibility.shows_layer(layer) {
                                    continue;
                                }
                                if let Some(cell) = document.cell(LayerAddress::new(x, y, layer)) {
                                    let selected = selection.contains_cell(x, y, layer);
                                    let is_open = layer_is_fully_open(cell);
                                    let sampled_area_selected =
                                        selection.sampled_area_contains_cell(x, y, layer, stride);
                                    if is_open
                                        && (visibility.hide_fully_open_blocks
                                            || !selected
                                                && (!visibility.shows_open_cell(x, y)
                                                    || x % stride != 0
                                                    || y % stride != 0
                                                    || sampled_area_selected))
                                    {
                                        continue;
                                    }
                                    append_editor_geodata_cell(
                                        &mut mesh,
                                        map,
                                        origin,
                                        x,
                                        y,
                                        cell,
                                        if is_open && !selected {
                                            sampled_scale
                                        } else {
                                            7.35
                                        },
                                        selected,
                                    );
                                }
                            }
                        }
                    }
                }
                None => {}
            }
        }
    }
    mesh
}

/// Builds the status-glyph layer that sits directly over the editor's L2J
/// quads.  It intentionally follows the same sampling rules as the coloured
/// cells, so an icon always describes the quad below it.  Partial and blocked
/// columns are never sampled away.
fn editor_nswe_icon_instances_in_blocks(
    map: &SourceMap,
    document: &Document,
    origin: Vec3,
    stride: usize,
    visibility: EditorOverlayOptions,
    selection: &EditorSelectionLookup,
    block_x: Range<usize>,
    block_y: Range<usize>,
) -> CpuNsweIcons {
    let mut mesh = CpuNsweIcons::default();
    let stride = stride.max(1);
    for block_x in block_x {
        for block_y in block_y.clone() {
            let start_x = block_x * 8;
            let start_y = block_y * 8;
            if visibility.hides_block(document, block_x, block_y) {
                continue;
            }
            match document.block_type(block_x, block_y) {
                // Simple is one authored 8×8 surface, so it receives one
                // equally unified glyph instead of 64 repeated cell icons.
                Some(EditableBlockType::Simple) => {
                    let selected = selection.contains_simple_block(block_x, block_y);
                    if visibility.shows_layer(0)
                        && (selected || visibility.shows_open_block(start_x, start_y))
                    {
                        if let Some(layer) = document.cell(LayerAddress::new(start_x, start_y, 0)) {
                            append_editor_nswe_icon(
                                &mut mesh, map, origin, start_x, start_y, layer, 63.4,
                            );
                        }
                    }
                }
                Some(EditableBlockType::Complex | EditableBlockType::Multilayer) => {
                    let sampled_scale = 8.0 * stride as f32 - 0.6;
                    for local_x in 0..8 {
                        for local_y in 0..8 {
                            let x = start_x + local_x;
                            let y = start_y + local_y;
                            let layers = document.layer_count(x, y).unwrap_or(0);
                            for layer_index in 0..layers {
                                if !visibility.shows_layer(layer_index) {
                                    continue;
                                }
                                let Some(layer) =
                                    document.cell(LayerAddress::new(x, y, layer_index))
                                else {
                                    continue;
                                };
                                let selected = selection.contains_cell(x, y, layer_index);
                                let is_open = layer_is_fully_open(layer);
                                let sampled_area_selected =
                                    selection.sampled_area_contains_cell(x, y, layer_index, stride);
                                if is_open
                                    && (visibility.hide_fully_open_blocks
                                        || !selected
                                            && (!visibility.shows_open_cell(x, y)
                                                || x % stride != 0
                                                || y % stride != 0
                                                || sampled_area_selected))
                                {
                                    continue;
                                }
                                append_editor_nswe_icon(
                                    &mut mesh,
                                    map,
                                    origin,
                                    x,
                                    y,
                                    layer,
                                    if is_open && !selected {
                                        sampled_scale
                                    } else {
                                        7.35
                                    },
                                );
                            }
                        }
                    }
                }
                None => {}
            }
        }
    }
    mesh
}

fn append_editor_nswe_icon(
    mesh: &mut CpuNsweIcons,
    map: &SourceMap,
    origin: Vec3,
    x: usize,
    y: usize,
    cell: Layer,
    cell_scale: f32,
) {
    if cell.height == NULL_HEIGHT {
        return;
    }
    mesh.instances.push(NsweIconInstance {
        position: [
            map.bounds.min.x + x as f32 * 16.0 + cell_scale - origin.x,
            cell.height as f32 - origin.y + 2.5,
            map.bounds.min.z + y as f32 * 16.0 + cell_scale - origin.z,
        ],
        // The 256 px glyph remains sharp when enlarged over a Simple block.
        // Complex cells use almost their full surface for legibility at zoom.
        scale: if cell_scale > 8.0 {
            cell_scale * 0.78
        } else {
            cell_scale * 0.92
        },
        mask: (cell.nswe & 0x0f) as f32,
    });
}

/// Finds the first L2J layer actually hit by a view ray. It uses a 2D DDA walk
/// through the fixed 16-unit grid, so a pick is evaluated against each
/// individual stair cell instead of a guessed terrain height or a global layer
/// number that may be hidden below the clicked surface.
fn pick_l2j_ray(
    document: &Document,
    bounds: Box3,
    ray_origin: [f32; 3],
    ray: [f32; 3],
    layer_filter: Option<usize>,
    hide_fully_open_blocks: bool,
) -> Option<LayerAddress> {
    let (mut current_t, end_t) = ray_grid_interval(ray_origin, ray, bounds)?;
    current_t = current_t.max(0.0) + 0.000_1;
    if current_t > end_t {
        return None;
    }

    const CELL_SIZE: f32 = 16.0;
    const GRID_SIZE: isize = 2048;
    let point_x = ray_origin[0] + ray[0] * current_t;
    let point_z = ray_origin[2] + ray[2] * current_t;
    let mut cell_x = ((point_x - bounds.min.x) / CELL_SIZE).floor() as isize;
    let mut cell_y = ((point_z - bounds.min.z) / CELL_SIZE).floor() as isize;
    cell_x = cell_x.clamp(0, GRID_SIZE - 1);
    cell_y = cell_y.clamp(0, GRID_SIZE - 1);

    let step_x = if ray[0] >= 0.0 { 1 } else { -1 };
    let step_y = if ray[2] >= 0.0 { 1 } else { -1 };
    let delta_x = if ray[0].abs() < 0.000_001 {
        f32::INFINITY
    } else {
        CELL_SIZE / ray[0].abs()
    };
    let delta_y = if ray[2].abs() < 0.000_001 {
        f32::INFINITY
    } else {
        CELL_SIZE / ray[2].abs()
    };
    let next_x = bounds.min.x
        + if step_x > 0 {
            (cell_x + 1) as f32 * CELL_SIZE
        } else {
            cell_x as f32 * CELL_SIZE
        };
    let next_y = bounds.min.z
        + if step_y > 0 {
            (cell_y + 1) as f32 * CELL_SIZE
        } else {
            cell_y as f32 * CELL_SIZE
        };
    let mut edge_x = if delta_x.is_finite() {
        (next_x - ray_origin[0]) / ray[0]
    } else {
        f32::INFINITY
    };
    let mut edge_y = if delta_y.is_finite() {
        (next_y - ray_origin[2]) / ray[2]
    } else {
        f32::INFINITY
    };

    // A diagonal ray can cross at most 4,096 cells.  The extra allowance
    // covers exact boundary crossings without risking an unbounded loop.
    for _ in 0..=4_098 {
        if !(0..GRID_SIZE).contains(&cell_x) || !(0..GRID_SIZE).contains(&cell_y) {
            return None;
        }
        let cell_end = edge_x.min(edge_y).min(end_t);
        let x = cell_x as usize;
        let y = cell_y as usize;
        let layers = document.layer_count(x, y)?;
        let hit = (0..layers)
            .filter(|layer| layer_filter.map_or(true, |wanted| *layer == wanted))
            .filter_map(|layer| {
                let cell = document.cell(LayerAddress::new(x, y, layer))?;
                if cell.height == NULL_HEIGHT || hide_fully_open_blocks && layer_is_fully_open(cell)
                {
                    return None;
                }
                let height_t = (cell.height as f32 - ray_origin[1]) / ray[1];
                (height_t >= current_t - 0.001 && height_t <= cell_end + 0.001)
                    .then_some((layer, height_t))
            })
            .min_by(|(_, left), (_, right)| left.total_cmp(right));
        if let Some((layer, _)) = hit {
            return Some(LayerAddress::new(x, y, layer));
        }

        if cell_end >= end_t {
            return None;
        }
        if edge_x < edge_y {
            cell_x += step_x;
            current_t = edge_x;
            edge_x += delta_x;
        } else if edge_y < edge_x {
            cell_y += step_y;
            current_t = edge_y;
            edge_y += delta_y;
        } else {
            // Crossing exactly through a cell corner: advance both axes,
            // otherwise the same corner would be evaluated twice.
            cell_x += step_x;
            cell_y += step_y;
            current_t = edge_x;
            edge_x += delta_x;
            edge_y += delta_y;
        }
    }
    None
}

fn ray_grid_interval(ray_origin: [f32; 3], ray: [f32; 3], bounds: Box3) -> Option<(f32, f32)> {
    let mut enter = f32::NEG_INFINITY;
    let mut exit = f32::INFINITY;
    for (origin, direction, minimum, maximum) in [
        (ray_origin[0], ray[0], bounds.min.x, bounds.max.x),
        (ray_origin[2], ray[2], bounds.min.z, bounds.max.z),
    ] {
        if direction.abs() < 0.000_001 {
            if origin < minimum || origin > maximum {
                return None;
            }
            continue;
        }
        let first = (minimum - origin) / direction;
        let last = (maximum - origin) / direction;
        enter = enter.max(first.min(last));
        exit = exit.min(first.max(last));
        if enter > exit {
            return None;
        }
    }
    Some((enter, exit))
}

fn append_editor_geodata_cell(
    mesh: &mut CpuGeodata,
    map: &SourceMap,
    origin: Vec3,
    x: usize,
    y: usize,
    cell: Layer,
    scale: f32,
    selected: bool,
) {
    if cell.height == NULL_HEIGHT {
        return;
    }
    mesh.instances.push(GeodataInstance {
        position: [
            map.bounds.min.x + x as f32 * 16.0 + scale - origin.x,
            cell.height as f32 - origin.y + 1.25,
            map.bounds.min.z + y as f32 * 16.0 + scale - origin.z,
        ],
        scale,
        color: editor_cell_color(cell, selected),
    });
}

fn editor_cell_color(cell: Layer, selected: bool) -> [u8; 4] {
    if selected {
        return [255, 235, 0, 255];
    }
    match (cell.nswe & 0x0f).count_ones() {
        4 => [0, 205, 230, 165],
        0 => [235, 45, 45, 230],
        _ => [255, 150, 0, 220],
    }
}

struct PreviewUi {
    wireframe: bool,
    culling: bool,
}

impl Default for PreviewUi {
    fn default() -> Self {
        Self {
            wireframe: false,
            culling: true,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Vertex {
    position: [f32; 3],
    color: [f32; 4],
    normal: [f32; 3],
}

impl Vertex {
    const ATTRIBUTES: [wgpu::VertexAttribute; 3] =
        wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x4, 2 => Float32x3];

    fn layout() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Self>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &Self::ATTRIBUTES,
        }
    }
}

#[derive(Clone, Default)]
struct CpuMesh {
    vertices: Vec<Vertex>,
    triangles: Vec<u32>,
    lines: Vec<u32>,
}

struct GpuMesh {
    vertices: wgpu::Buffer,
    triangles: wgpu::Buffer,
    lines: wgpu::Buffer,
    triangle_count: u32,
    line_count: u32,
}
impl GpuMesh {
    fn new(device: &wgpu::Device, mesh: &CpuMesh) -> Self {
        let cpu_vertices = if mesh.vertices.is_empty() {
            vec![Vertex {
                position: [0.0; 3],
                color: [0.0; 4],
                normal: [0.0, 1.0, 0.0],
            }]
        } else {
            mesh.vertices.clone()
        };
        let cpu_triangles = if mesh.triangles.is_empty() {
            vec![0u32]
        } else {
            mesh.triangles.clone()
        };
        let cpu_lines = if mesh.lines.is_empty() {
            vec![0u32]
        } else {
            mesh.lines.clone()
        };
        let vertices = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("preview-mesh-vertices"),
            contents: bytemuck::cast_slice(&cpu_vertices),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let triangles = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("preview-mesh-triangles"),
            contents: bytemuck::cast_slice(&cpu_triangles),
            usage: wgpu::BufferUsages::INDEX,
        });
        let lines = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("preview-mesh-lines"),
            contents: bytemuck::cast_slice(&cpu_lines),
            usage: wgpu::BufferUsages::INDEX,
        });
        Self {
            vertices,
            triangles,
            lines,
            triangle_count: mesh.triangles.len() as u32,
            line_count: mesh.lines.len() as u32,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct QuadVertex {
    offset: [f32; 2],
}

impl QuadVertex {
    const ATTRIBUTES: [wgpu::VertexAttribute; 1] = wgpu::vertex_attr_array![0 => Float32x2];

    fn layout() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Self>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &Self::ATTRIBUTES,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GeodataInstance {
    position: [f32; 3],
    scale: f32,
    color: [u8; 4],
}

impl GeodataInstance {
    const ATTRIBUTES: [wgpu::VertexAttribute; 3] = wgpu::vertex_attr_array![
        1 => Float32x3,
        2 => Float32,
        3 => Unorm8x4
    ];

    fn layout() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Self>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &Self::ATTRIBUTES,
        }
    }
}

#[derive(Default)]
struct CpuGeodata {
    instances: Vec<GeodataInstance>,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct NsweIconInstance {
    position: [f32; 3],
    scale: f32,
    mask: f32,
}

impl NsweIconInstance {
    const ATTRIBUTES: [wgpu::VertexAttribute; 3] = wgpu::vertex_attr_array![
        1 => Float32x3,
        2 => Float32,
        3 => Float32
    ];

    fn layout() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Self>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &Self::ATTRIBUTES,
        }
    }
}

#[derive(Default)]
struct CpuNsweIcons {
    instances: Vec<NsweIconInstance>,
}

struct CollisionMeshes {
    terrain: GpuMesh,
    static_meshes: GpuMesh,
    bsp: GpuMesh,
    blocking_volumes: GpuMesh,
}

impl CollisionMeshes {
    fn new(device: &wgpu::Device, map: &SourceMap, origin: Vec3) -> Self {
        let terrain_end = map.geometry.terrain_triangles;
        let meshes_end = terrain_end + map.geometry.static_mesh_triangles;
        let bsp_end = meshes_end + map.geometry.bsp_triangles;
        debug_assert_eq!(
            map.triangles.len(),
            bsp_end + map.geometry.blocking_volume_triangles
        );
        // Do not decimate collision triangles here.  The terrain has two
        // triangles per quad; stepping through that stream to keep a global
        // triangle budget removes one half of many quads and produces the
        // black-and-white checkerboard seen on large underground maps.
        let make = |triangles: &[Triangle], color| {
            GpuMesh::new(device, &source_collision_mesh(triangles, origin, color))
        };
        Self {
            terrain: make(&map.triangles[..terrain_end], [0.85, 0.85, 0.85, 1.0]),
            static_meshes: make(
                &map.triangles[terrain_end..meshes_end],
                [1.0, 0.6, 0.6, 1.0],
            ),
            bsp: make(&map.triangles[meshes_end..bsp_end], [1.0, 1.0, 0.7, 1.0]),
            blocking_volumes: make(&map.triangles[bsp_end..], [1.0, 0.75, 0.35, 1.0]),
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct TexturedVertex {
    position: [f32; 3],
    normal: [f32; 3],
    uv: [f32; 2],
}

impl TexturedVertex {
    const ATTRIBUTES: [wgpu::VertexAttribute; 3] =
        wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3, 2 => Float32x2];

    fn layout() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Self>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &Self::ATTRIBUTES,
        }
    }
}

/// One independently sortable surface or merged opaque material group.
struct TexturedBatch {
    vertices: wgpu::Buffer,
    indices: wgpu::Buffer,
    index_count: u32,
    material: Arc<wgpu::BindGroup>,
    pipeline: usize,
    center: [f32; 3],
    bounds: [[f32; 3]; 2],
}

/// GPU-resident form of `unreal::VisualScene`, built once when the textured
/// view is enabled (or a new project loads while it's already enabled).
struct TexturedScene {
    batches: Vec<TexturedBatch>,
    pipelines: Vec<wgpu::RenderPipeline>,
    visible_order: Vec<usize>,
    view_projection: Option<[[f32; 4]; 4]>,
    opaque_order: Vec<usize>,
    blended_order: Vec<(usize, f32)>,
    sort_direction: Option<[f32; 3]>,
}

impl TexturedScene {
    fn update_view(&mut self, matrix: &[[f32; 4]; 4], forward: [f32; 3]) {
        if self.view_projection == Some(*matrix) && self.sort_direction == Some(forward) {
            return;
        }
        self.sort_blended(forward);
        self.view_projection = Some(*matrix);
        let frustum = ViewFrustum::new(matrix);
        self.visible_order.clear();
        self.visible_order.extend(
            self.opaque_order
                .iter()
                .copied()
                .chain(self.blended_order.iter().map(|(index, _)| *index))
                .filter(|index| frustum.intersects(self.batches[*index].bounds)),
        );
    }

    fn sort_blended(&mut self, forward: [f32; 3]) {
        if self.sort_direction == Some(forward) {
            return;
        }
        self.sort_direction = Some(forward);
        for (index, depth) in &mut self.blended_order {
            // Camera translation subtracts the same value from every depth,
            // so only a changed viewing direction can change this ordering.
            *depth = dot(self.batches[*index].center, forward);
        }
        self.blended_order
            .sort_unstable_by(|left, right| right.1.total_cmp(&left.1).then(left.0.cmp(&right.0)));
    }

    fn draw<'a>(&'a self, pass: &mut wgpu::RenderPass<'a>, camera: &'a wgpu::BindGroup) {
        pass.set_bind_group(0, camera, &[]);
        let mut pipeline = None;
        let mut material: Option<&Arc<wgpu::BindGroup>> = None;
        for index in &self.visible_order {
            let batch = &self.batches[*index];
            if pipeline != Some(batch.pipeline) {
                pass.set_pipeline(&self.pipelines[batch.pipeline]);
                pipeline = Some(batch.pipeline);
            }
            if material.is_none_or(|previous| !Arc::ptr_eq(previous, &batch.material)) {
                pass.set_bind_group(1, &batch.material, &[]);
                material = Some(&batch.material);
            }
            pass.set_vertex_buffer(0, batch.vertices.slice(..));
            pass.set_index_buffer(batch.indices.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..batch.index_count, 0, 0..1);
        }
    }
}

struct ViewFrustum {
    planes: [[f32; 4]; 6],
}

impl ViewFrustum {
    fn new(matrix: &[[f32; 4]; 4]) -> Self {
        let row = |r: usize| std::array::from_fn::<_, 4, _>(|c| matrix[c][r]);
        let x = row(0);
        let y = row(1);
        let z = row(2);
        let w = row(3);
        Self {
            planes: [
                std::array::from_fn(|i| w[i] + x[i]),
                std::array::from_fn(|i| w[i] - x[i]),
                std::array::from_fn(|i| w[i] + y[i]),
                std::array::from_fn(|i| w[i] - y[i]),
                z, // wgpu's near clip plane is z >= 0, not z >= -w.
                std::array::from_fn(|i| w[i] - z[i]),
            ],
        }
    }

    fn intersects(&self, bounds: [[f32; 3]; 2]) -> bool {
        self.planes.iter().all(|plane| {
            let mut distance = plane[3];
            for axis in 0..3 {
                let corner = if plane[axis] >= 0.0 {
                    bounds[1][axis]
                } else {
                    bounds[0][axis]
                };
                distance += plane[axis] * corner;
            }
            // Reject only a box fully outside. Keep plane crossings, boundary
            // contacts and non-finite data conservatively visible.
            !(distance < -0.001)
        })
    }
}

/// Rebases a visual batch onto the preview's origin-relative space.
///
/// `VisualScene` carries absolute Lineage II world coordinates, but every
/// mesh the preview renders (collision geometry, geodata cells, NSWE icons)
/// is expressed relative to `map_origin` so the camera can work near zero
/// and keep f32 precision. Skipping this rebase puts the textured scene
/// tens of thousands of units away from the geodata it is meant to overlay.
fn textured_batch_vertices(batch: &VisualBatch, origin: Vec3) -> Vec<TexturedVertex> {
    batch
        .vertices
        .iter()
        .map(|(position, normal, uv)| TexturedVertex {
            position: [
                position.x - origin.x,
                position.y - origin.y,
                position.z - origin.z,
            ],
            normal: [normal.x, normal.y, normal.z],
            uv: *uv,
        })
        .collect()
}

fn source_collision_mesh(triangles: &[Triangle], origin: Vec3, color: [f32; 4]) -> CpuMesh {
    let mut mesh = CpuMesh::default();
    for triangle in triangles {
        append_triangle(&mut mesh, *triangle, origin, color);
    }
    mesh
}

fn append_triangle(mesh: &mut CpuMesh, triangle: Triangle, origin: Vec3, color: [f32; 4]) {
    let base = mesh.vertices.len() as u32;
    let normal = (triangle.b - triangle.a)
        .cross(triangle.c - triangle.a)
        .normalize_or_zero();
    for point in [triangle.a, triangle.b, triangle.c] {
        mesh.vertices.push(Vertex {
            position: [point.x - origin.x, point.y - origin.y, point.z - origin.z],
            color,
            normal: [normal.x, normal.y, normal.z],
        });
    }
    mesh.triangles
        .extend_from_slice(&[base, base + 1, base + 2]);
    mesh.lines
        .extend_from_slice(&[base, base + 1, base + 1, base + 2, base + 2, base]);
}

fn draw_mesh<'a>(
    pass: &mut wgpu::RenderPass<'a>,
    pipeline: &'a wgpu::RenderPipeline,
    mesh: &'a GpuMesh,
    camera: &'a wgpu::BindGroup,
    lines: bool,
) {
    let count = if lines {
        mesh.line_count
    } else {
        mesh.triangle_count
    };
    if count == 0 {
        return;
    }
    pass.set_pipeline(pipeline);
    pass.set_bind_group(0, camera, &[]);
    pass.set_vertex_buffer(0, mesh.vertices.slice(..));
    pass.set_index_buffer(
        if lines {
            mesh.lines.slice(..)
        } else {
            mesh.triangles.slice(..)
        },
        wgpu::IndexFormat::Uint32,
    );
    pass.draw_indexed(0..count, 0, 0..1);
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct CameraUniform {
    view_projection: [[f32; 4]; 4],
    position: [f32; 3],
    _padding: f32,
}

impl CameraUniform {
    fn new(view_projection: [[f32; 4]; 4], position: [f32; 3]) -> Self {
        Self {
            view_projection,
            position,
            _padding: 0.0,
        }
    }
}

fn create_camera_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("editor-camera-layout"),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        }],
    })
}

struct Camera {
    position: [f32; 3],
    yaw: f32,
    pitch: f32,
    speed: f32,
}

impl Camera {
    fn for_bounds(bounds: Box3) -> Self {
        let width = bounds.max.x - bounds.min.x;
        let depth = bounds.max.z - bounds.min.z;
        let extent = width.max(depth).max(2_000.0);
        let position = [extent * 0.72, extent * 0.65, extent * 0.72];
        let horizontal = (position[0] * position[0] + position[2] * position[2]).sqrt();
        Self {
            position,
            yaw: (-position[2]).atan2(-position[0]),
            pitch: (-position[1]).atan2(horizontal),
            speed: extent * 0.7,
        }
    }

    fn reset(&mut self, bounds: Box3) {
        *self = Self::for_bounds(bounds);
    }

    fn matrix(&self, width: u32, height: u32) -> [[f32; 4]; 4] {
        let forward = self.forward();
        let target = add(self.position, forward);
        let aspect = width.max(1) as f32 / height.max(1) as f32;
        multiply(
            perspective_rh(55.0_f32.to_radians(), aspect, 1.0, 250_000.0),
            look_at_rh(self.position, target, [0.0, 1.0, 0.0]),
        )
    }

    fn forward(&self) -> [f32; 3] {
        [
            self.yaw.cos() * self.pitch.cos(),
            self.pitch.sin(),
            self.yaw.sin() * self.pitch.cos(),
        ]
    }
}

#[derive(Default)]
struct CameraInput {
    pressed: HashSet<KeyCode>,
    rotating: bool,
    raw_mouse: bool,
    cursor: Option<PhysicalPosition<f64>>,
    left_pressed: bool,
    right_pressed: bool,
    dual_button_navigation: bool,
}

impl CameraInput {
    fn handle(&mut self, event: &WindowEvent, camera: &mut Camera, bounds: Box3, window: &Window) {
        match event {
            WindowEvent::KeyboardInput { event, .. } => {
                let PhysicalKey::Code(code) = event.physical_key else {
                    return;
                };
                match event.state {
                    ElementState::Pressed => {
                        if code == KeyCode::Home && !event.repeat {
                            camera.reset(bounds);
                        }
                        self.pressed.insert(code);
                    }
                    ElementState::Released => {
                        self.pressed.remove(&code);
                    }
                }
            }
            WindowEvent::MouseInput {
                button: MouseButton::Right,
                state,
                ..
            } => {
                self.right_pressed = *state == ElementState::Pressed;
                self.update_dual_button_navigation();
                if self.right_pressed {
                    self.rotating = true;
                    self.raw_mouse = capture_cursor(window);
                } else {
                    self.stop_rotating(window);
                }
                self.cursor = None;
            }
            WindowEvent::MouseInput {
                button: MouseButton::Left,
                state,
                ..
            } => {
                self.left_pressed = *state == ElementState::Pressed;
                self.update_dual_button_navigation();
                self.cursor = None;
            }
            WindowEvent::Focused(false) => {
                self.left_pressed = false;
                self.right_pressed = false;
                self.dual_button_navigation = false;
                self.stop_rotating(window);
            }
            WindowEvent::CursorMoved { position, .. } if self.rotating && !self.raw_mouse => {
                if let Some(previous) = self.cursor.replace(*position) {
                    self.apply_mouse_motion(
                        camera,
                        position.x - previous.x,
                        position.y - previous.y,
                    );
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let amount = match delta {
                    MouseScrollDelta::LineDelta(_, y) => *y,
                    MouseScrollDelta::PixelDelta(position) => position.y as f32 / 50.0,
                };
                camera.position = add(
                    camera.position,
                    scale(camera.forward(), amount * camera.speed * 0.12),
                );
            }
            _ => {}
        }
    }

    fn handle_device(&mut self, event: &DeviceEvent, camera: &mut Camera) {
        if !self.rotating || !self.raw_mouse {
            return;
        }
        if let DeviceEvent::MouseMotion { delta } = event {
            self.apply_mouse_motion(camera, delta.0, delta.1);
        }
    }

    fn update_dual_button_navigation(&mut self) {
        if self.left_pressed && self.right_pressed {
            self.dual_button_navigation = true;
        } else if !self.left_pressed && !self.right_pressed {
            self.dual_button_navigation = false;
        }
    }

    fn uses_left_for_vertical_navigation(&self) -> bool {
        self.right_pressed || self.dual_button_navigation
    }

    fn apply_mouse_motion(&self, camera: &mut Camera, delta_x: f64, delta_y: f64) {
        if self.left_pressed && self.right_pressed {
            elevate_camera(camera, delta_y);
        } else {
            rotate_camera(camera, delta_x, delta_y);
        }
    }

    fn stop_rotating(&mut self, window: &Window) {
        self.rotating = false;
        self.raw_mouse = false;
        self.cursor = None;
        let _ = window.set_cursor_grab(CursorGrabMode::None);
        window.set_cursor_visible(true);
    }

    fn update_camera(&self, camera: &mut Camera, elapsed: f32) {
        let forward = camera.forward();
        let horizontal_forward = normalize([forward[0], 0.0, forward[2]]);
        let right = normalize([-horizontal_forward[2], 0.0, horizontal_forward[0]]);
        let moving_fast = self.pressed.contains(&KeyCode::ShiftLeft)
            || self.pressed.contains(&KeyCode::ShiftRight);
        let speed = if moving_fast { 1.0 } else { NORMAL_MOVE_SPEED };
        let distance = camera.speed * elapsed * speed;
        if self.pressed.contains(&KeyCode::KeyW) {
            camera.position = add(camera.position, scale(forward, distance));
        }
        if self.pressed.contains(&KeyCode::KeyS) {
            camera.position = add(camera.position, scale(forward, -distance));
        }
        if self.pressed.contains(&KeyCode::KeyD) {
            camera.position = add(camera.position, scale(right, distance));
        }
        if self.pressed.contains(&KeyCode::KeyA) {
            camera.position = add(camera.position, scale(right, -distance));
        }
        if self.pressed.contains(&KeyCode::KeyE) {
            camera.position[1] += distance;
        }
        if self.pressed.contains(&KeyCode::KeyQ) {
            camera.position[1] -= distance;
        }
    }
}

/// Mirrors GLFW_CURSOR_DISABLED from the old viewer. Locked gives raw deltas
/// on Windows; confined keeps a usable fallback for adapters that reject it.
fn capture_cursor(window: &Window) -> bool {
    let grabbed = window.set_cursor_grab(CursorGrabMode::Locked).is_ok()
        || window.set_cursor_grab(CursorGrabMode::Confined).is_ok();
    window.set_cursor_visible(!grabbed);
    grabbed
}

fn rotate_camera(camera: &mut Camera, delta_x: f64, delta_y: f64) {
    camera.yaw -= delta_x as f32 * MOUSE_LOOK_SENSITIVITY;
    camera.pitch = (camera.pitch - delta_y as f32 * MOUSE_LOOK_SENSITIVITY).clamp(-1.52, 1.52);
}

fn elevate_camera(camera: &mut Camera, delta_y: f64) {
    camera.position[1] -= delta_y as f32 * camera.speed * MOUSE_ELEVATION_SENSITIVITY;
}

fn create_pipelines(
    device: &wgpu::Device,
    camera_layout: &wgpu::BindGroupLayout,
    format: wgpu::TextureFormat,
) -> (
    wgpu::RenderPipeline,
    wgpu::RenderPipeline,
    wgpu::RenderPipeline,
) {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("preview-shader"),
        source: wgpu::ShaderSource::Wgsl(PREVIEW_SHADER.into()),
    });
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("preview-pipeline-layout"),
        bind_group_layouts: &[camera_layout],
        push_constant_ranges: &[],
    });
    let make_pipeline = |label, topology, cull_mode| {
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(label),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs_main",
                buffers: &[Vertex::layout()],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs_main",
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode,
                polygon_mode: wgpu::PolygonMode::Fill,
                unclipped_depth: false,
                conservative: false,
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: true,
                depth_compare: wgpu::CompareFunction::Less,
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: msaa_state(),
            multiview: None,
        })
    };
    (
        make_pipeline(
            "preview-triangles-cull",
            wgpu::PrimitiveTopology::TriangleList,
            Some(wgpu::Face::Back),
        ),
        make_pipeline(
            "preview-triangles-no-cull",
            wgpu::PrimitiveTopology::TriangleList,
            None,
        ),
        make_pipeline("preview-lines", wgpu::PrimitiveTopology::LineList, None),
    )
}

fn create_geodata_pipelines(
    device: &wgpu::Device,
    camera_layout: &wgpu::BindGroupLayout,
    format: wgpu::TextureFormat,
) -> (
    wgpu::RenderPipeline,
    wgpu::RenderPipeline,
    wgpu::RenderPipeline,
) {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("geodata-preview-shader"),
        source: wgpu::ShaderSource::Wgsl(GEODATA_SHADER.into()),
    });
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("geodata-preview-pipeline-layout"),
        bind_group_layouts: &[camera_layout],
        push_constant_ranges: &[],
    });
    let make_pipeline = |label, topology, cull_mode| {
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(label),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs_main",
                buffers: &[QuadVertex::layout(), GeodataInstance::layout()],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs_main",
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode,
                polygon_mode: wgpu::PolygonMode::Fill,
                unclipped_depth: false,
                conservative: false,
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                // A multilayer block can contain floors, ceilings and
                // platforms in the same X/Z column.  Blending every one of
                // them makes the preview look broken, particularly on maps
                // with dense architecture such as 22_23_Classic.  Writing
                // depth makes the GPU retain only the surface facing the
                // camera, exactly as it already does for the collision mesh.
                depth_write_enabled: true,
                depth_compare: wgpu::CompareFunction::LessEqual,
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: msaa_state(),
            multiview: None,
        })
    };
    (
        make_pipeline(
            "geodata-preview-cull",
            wgpu::PrimitiveTopology::TriangleList,
            Some(wgpu::Face::Back),
        ),
        make_pipeline(
            "geodata-preview-no-cull",
            wgpu::PrimitiveTopology::TriangleList,
            None,
        ),
        make_pipeline(
            "geodata-preview-lines",
            wgpu::PrimitiveTopology::LineList,
            None,
        ),
    )
}

/// Editor-only L2J overlay. Selected cells use a different instance colour in
/// this same pipeline, so no competing surface or depth offset is required.
fn create_geodata_overlay_pipeline(
    device: &wgpu::Device,
    camera_layout: &wgpu::BindGroupLayout,
    format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("geodata-editor-overlay"),
        source: wgpu::ShaderSource::Wgsl(GEODATA_SHADER.into()),
    });
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("geodata-editor-overlay"),
        bind_group_layouts: &[camera_layout],
        push_constant_ranges: &[],
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("geodata-editor-overlay"),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: "vs_main",
            buffers: &[QuadVertex::layout(), GeodataInstance::layout()],
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: "fs_main",
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            strip_index_format: None,
            front_face: wgpu::FrontFace::Ccw,
            cull_mode: None,
            polygon_mode: wgpu::PolygonMode::Fill,
            unclipped_depth: false,
            conservative: false,
        },
        depth_stencil: Some(wgpu::DepthStencilState {
            format: DEPTH_FORMAT,
            depth_write_enabled: true,
            depth_compare: wgpu::CompareFunction::LessEqual,
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default(),
        }),
        multisample: msaa_state(),
        multiview: None,
    })
}

fn create_nswe_icon_resources(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    camera_layout: &wgpu::BindGroupLayout,
    format: wgpu::TextureFormat,
) -> (wgpu::RenderPipeline, wgpu::BindGroup) {
    const ICON_SIDE: u32 = 256;
    const ATLAS_SIDE: u32 = ICON_SIDE * 4;
    let pixels = nswe_icon_atlas();
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("editor-nswe-icon-atlas"),
        size: wgpu::Extent3d {
            width: ATLAS_SIDE,
            height: ATLAS_SIDE,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        wgpu::ImageCopyTexture {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &pixels,
        wgpu::ImageDataLayout {
            offset: 0,
            bytes_per_row: Some(4 * ATLAS_SIDE),
            rows_per_image: Some(ATLAS_SIDE),
        },
        wgpu::Extent3d {
            width: ATLAS_SIDE,
            height: ATLAS_SIDE,
            depth_or_array_layers: 1,
        },
    );
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("editor-nswe-icon-sampler"),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        address_mode_w: wgpu::AddressMode::ClampToEdge,
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        mipmap_filter: wgpu::FilterMode::Nearest,
        ..Default::default()
    });
    let atlas_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("editor-nswe-icon-atlas-layout"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    });
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("editor-nswe-icon-atlas-bind-group"),
        layout: &atlas_layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(&sampler),
            },
        ],
    });
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("editor-nswe-icon-shader"),
        source: wgpu::ShaderSource::Wgsl(NSWE_ICON_SHADER.into()),
    });
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("editor-nswe-icon-pipeline-layout"),
        bind_group_layouts: &[camera_layout, &atlas_layout],
        push_constant_ranges: &[],
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("editor-nswe-icon-pipeline"),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: "vs_main",
            buffers: &[QuadVertex::layout(), NsweIconInstance::layout()],
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: "fs_main",
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            strip_index_format: None,
            front_face: wgpu::FrontFace::Ccw,
            cull_mode: None,
            polygon_mode: wgpu::PolygonMode::Fill,
            unclipped_depth: false,
            conservative: false,
        },
        depth_stencil: Some(wgpu::DepthStencilState {
            format: DEPTH_FORMAT,
            depth_write_enabled: false,
            // Status glyphs must obey the same surface depth as their cell;
            // otherwise icons from floors below leak through a platform.
            depth_compare: wgpu::CompareFunction::LessEqual,
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default(),
        }),
        multisample: msaa_state(),
        multiview: None,
    });
    (pipeline, bind_group)
}

/// Bind group layout shared by every material texture in the textured
/// visualization. Kept separate from the NSWE icon atlas layout because
/// material textures need `Repeat` addressing (terrain/wall tiling) instead
/// of the icon atlas's `ClampToEdge`.
fn create_material_texture_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("editor-material-texture-layout"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 3,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
        ],
    })
}

/// sRGB byte -> linear float, for the 256 possible channel values.
///
/// The client's bitmaps are sRGB-encoded and the GPU samples them through
/// `Rgba8UnormSrgb`, so any CPU-side filtering has to leave gamma space too:
/// averaging the raw bytes darkens every mip level (a 0/255 checkerboard
/// averages to 128, which is ~22% light instead of 50%), which is exactly
/// what makes a distant tiled wall read as a dark smear.
static SRGB_TO_LINEAR: LazyLock<[f32; 256]> = LazyLock::new(|| {
    let mut table = [0.0; 256];
    for (value, slot) in table.iter_mut().enumerate() {
        let channel = value as f32 / 255.0;
        *slot = if channel <= 0.040_45 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        };
    }
    table
});

fn linear_to_srgb(channel: f32) -> u8 {
    let encoded = if channel <= 0.003_130_8 {
        channel * 12.92
    } else {
        1.055 * channel.powf(1.0 / 2.4) - 0.055
    };
    (encoded * 255.0 + 0.5).clamp(0.0, 255.0) as u8
}

/// Downsamples one RGBA8 level to half its size using a 2×2 box filter in
/// linear light (clamping at odd edges). Used to build a full mip chain on
/// the CPU, since decoded client textures arrive as a single top-level
/// bitmap. Alpha is linear already, so it is averaged directly.
fn downsample_rgba(width: u32, height: u32, rgba: &[u8]) -> (u32, u32, Vec<u8>) {
    let next_width = (width / 2).max(1);
    let next_height = (height / 2).max(1);
    let to_linear = &*SRGB_TO_LINEAR;
    let at = |x: u32, y: u32| -> usize {
        let x = x.min(width - 1);
        let y = y.min(height - 1);
        ((y * width + x) * 4) as usize
    };
    let mut next = vec![0u8; (next_width * next_height * 4) as usize];
    for y in 0..next_height {
        for x in 0..next_width {
            let corners = [
                at(x * 2, y * 2),
                at(x * 2 + 1, y * 2),
                at(x * 2, y * 2 + 1),
                at(x * 2 + 1, y * 2 + 1),
            ];
            let target = ((y * next_width + x) * 4) as usize;
            for channel in 0..3 {
                let sum: f32 = corners
                    .iter()
                    .map(|corner| to_linear[rgba[corner + channel] as usize])
                    .sum();
                next[target + channel] = linear_to_srgb(sum * 0.25);
            }
            let alpha: u32 = corners
                .iter()
                .map(|corner| rgba[corner + 3] as u32)
                .sum::<u32>();
            next[target + 3] = (alpha / 4) as u8;
        }
    }
    (next_width, next_height, next)
}

/// Uploads the borrowed base level plus a full generated mip chain. The
/// worker caches views so independently sortable surfaces sharing a bitmap
/// do not each allocate another copy of that texture on the GPU.
fn create_material_texture(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    width: u32,
    height: u32,
    rgba: &[u8],
) -> wgpu::TextureView {
    let mip_level_count = u32::BITS - width.max(height).leading_zeros();
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("editor-material-texture"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let (mut level_width, mut level_height) = (width, height);
    let mut level_rgba = Cow::Borrowed(rgba);
    for level in 0..mip_level_count {
        queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &texture,
                mip_level: level,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &level_rgba,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(4 * level_width),
                rows_per_image: Some(level_height),
            },
            wgpu::Extent3d {
                width: level_width,
                height: level_height,
                depth_or_array_layers: 1,
            },
        );
        if level + 1 < mip_level_count {
            let (next_width, next_height, next_rgba) =
                downsample_rgba(level_width, level_height, &level_rgba);
            level_width = next_width;
            level_height = next_height;
            level_rgba = Cow::Owned(next_rgba);
        }
    }
    texture.create_view(&wgpu::TextureViewDescriptor::default())
}

fn create_material_sampler(device: &wgpu::Device) -> wgpu::Sampler {
    device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("editor-material-sampler"),
        address_mode_u: wgpu::AddressMode::Repeat,
        address_mode_v: wgpu::AddressMode::Repeat,
        address_mode_w: wgpu::AddressMode::Repeat,
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        mipmap_filter: wgpu::FilterMode::Linear,
        // Terrain and wall textures are almost always seen at a grazing
        // angle, where isotropic mip selection blurs along the wrong axis
        // and smears the tiling into vertical streaks. All three filters
        // are Linear, which is what wgpu requires to accept this.
        anisotropy_clamp: 8,
        ..Default::default()
    })
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct MaterialUniform {
    alpha_cutoff: f32,
    alpha_source: u32,
    _padding: [u32; 2],
}

fn create_material_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    texture: &wgpu::TextureView,
    opacity: Option<&wgpu::TextureView>,
    state: VisualMaterialState,
) -> wgpu::BindGroup {
    let uniform = MaterialUniform {
        alpha_cutoff: state
            .alpha_cutoff
            .map_or(-1.0, |value| f32::from(value) / 255.0),
        alpha_source: if opacity.is_some() {
            2
        } else {
            u32::from(state.use_texture_alpha)
        },
        _padding: [0; 2],
    };
    let buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("editor-material-uniform"),
        contents: bytemuck::bytes_of(&uniform),
        usage: wgpu::BufferUsages::UNIFORM,
    });
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("editor-material-bind-group"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(texture),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::TextureView(opacity.unwrap_or(texture)),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: buffer.as_entire_binding(),
            },
        ],
    })
}

/// Flat neutral gray used for surfaces whose material didn't resolve to a
/// decodable texture (unsupported bitmap format or unresolved material graph).
const FALLBACK_MATERIAL_RGBA: [u8; 4] = [170, 170, 170, 255];

/// Only blend and depth affect pipeline compilation; authored alpha source
/// and cutoff are material uniforms and must not multiply pipeline variants.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct TexturedPipelineKey {
    blend: VisualBlend,
    depth_write: bool,
    depth_test: bool,
}

impl From<VisualMaterialState> for TexturedPipelineKey {
    fn from(state: VisualMaterialState) -> Self {
        Self {
            blend: state.blend,
            depth_write: state.depth_write,
            depth_test: state.depth_test,
        }
    }
}

/// Shared immutable shader/layout; scene-specific variants compile on the
/// loader worker, never during drawing or before they are actually needed.
struct TexturedPipelineResources {
    shader: wgpu::ShaderModule,
    layout: wgpu::PipelineLayout,
    format: wgpu::TextureFormat,
}

impl TexturedPipelineResources {
    fn new(
        device: &wgpu::Device,
        camera_layout: &wgpu::BindGroupLayout,
        material_layout: &wgpu::BindGroupLayout,
        format: wgpu::TextureFormat,
    ) -> Self {
        Self {
            shader: device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("editor-textured-shader"),
                source: wgpu::ShaderSource::Wgsl(TEXTURED_SHADER.into()),
            }),
            layout: device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("editor-textured-pipeline-layout"),
                bind_group_layouts: &[camera_layout, material_layout],
                push_constant_ranges: &[],
            }),
            format,
        }
    }
}

fn material_blend(blend: VisualBlend) -> Option<wgpu::BlendState> {
    let (src_factor, dst_factor) = match blend {
        VisualBlend::Opaque | VisualBlend::Invisible => return None,
        VisualBlend::Alpha => (
            wgpu::BlendFactor::SrcAlpha,
            wgpu::BlendFactor::OneMinusSrcAlpha,
        ),
        VisualBlend::AlphaModulate => (wgpu::BlendFactor::Dst, wgpu::BlendFactor::OneMinusSrcAlpha),
        VisualBlend::Translucent => (wgpu::BlendFactor::One, wgpu::BlendFactor::OneMinusSrc),
        VisualBlend::Modulate => (wgpu::BlendFactor::Dst, wgpu::BlendFactor::Src),
        VisualBlend::Brighten => (wgpu::BlendFactor::One, wgpu::BlendFactor::One),
        VisualBlend::Darken => (wgpu::BlendFactor::Zero, wgpu::BlendFactor::OneMinusSrc),
    };
    Some(wgpu::BlendState {
        color: wgpu::BlendComponent {
            src_factor,
            dst_factor,
            operation: wgpu::BlendOperation::Add,
        },
        alpha: wgpu::BlendComponent::OVER,
    })
}

/// No culling: the visual mesh/BSP winding is not normalized.
fn create_textured_pipeline(
    device: &wgpu::Device,
    resources: &TexturedPipelineResources,
    key: TexturedPipelineKey,
) -> wgpu::RenderPipeline {
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("editor-textured-pipeline"),
        layout: Some(&resources.layout),
        vertex: wgpu::VertexState {
            module: &resources.shader,
            entry_point: "vs_main",
            buffers: &[TexturedVertex::layout()],
        },
        fragment: Some(wgpu::FragmentState {
            module: &resources.shader,
            entry_point: "fs_main",
            targets: &[Some(wgpu::ColorTargetState {
                format: resources.format,
                blend: material_blend(key.blend),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            strip_index_format: None,
            front_face: wgpu::FrontFace::Ccw,
            cull_mode: None,
            polygon_mode: wgpu::PolygonMode::Fill,
            unclipped_depth: false,
            conservative: false,
        },
        depth_stencil: Some(wgpu::DepthStencilState {
            format: DEPTH_FORMAT,
            depth_write_enabled: key.depth_write,
            depth_compare: if key.depth_test {
                wgpu::CompareFunction::Less
            } else {
                wgpu::CompareFunction::Always
            },
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default(),
        }),
        multisample: msaa_state(),
        multiview: None,
    })
}

fn nswe_icon_atlas() -> Vec<u8> {
    const ICON_SIDE: usize = 256;
    const ATLAS_SIDE: usize = ICON_SIDE * 4;
    let mut atlas = vec![0; ATLAS_SIDE * ATLAS_SIDE * 4];
    for mask in 0..16_usize {
        let (size, icon) = decode_nswe_icon_rgba(nswe_icon_bytes(mask as u8));
        assert_eq!(size, [ICON_SIDE, ICON_SIDE], "NSWE icon must be 256×256");
        let origin_x = (mask % 4) * ICON_SIDE;
        let origin_y = (mask / 4) * ICON_SIDE;
        for row in 0..ICON_SIDE {
            let source = &icon[row * ICON_SIDE * 4..(row + 1) * ICON_SIDE * 4];
            let start = ((origin_y + row) * ATLAS_SIDE + origin_x) * 4;
            atlas[start..start + ICON_SIDE * 4].copy_from_slice(source);
        }
    }
    atlas
}

/// Multisample state shared by every pipeline drawing into the preview's
/// render pass. All of them must agree with the attachments' sample count.
fn msaa_state() -> wgpu::MultisampleState {
    wgpu::MultisampleState {
        count: MSAA_SAMPLE_COUNT,
        ..Default::default()
    }
}

fn create_depth_view(
    device: &wgpu::Device,
    config: &wgpu::SurfaceConfiguration,
) -> wgpu::TextureView {
    device
        .create_texture(&wgpu::TextureDescriptor {
            label: Some("preview-depth"),
            size: wgpu::Extent3d {
                width: config.width,
                height: config.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: MSAA_SAMPLE_COUNT,
            dimension: wgpu::TextureDimension::D2,
            format: DEPTH_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        })
        .create_view(&wgpu::TextureViewDescriptor::default())
}

/// Offscreen multisampled colour target. The pass renders here and resolves
/// into the swapchain texture, which is what removes the stair-stepping on
/// mesh silhouettes and on the geodata cell quads.
fn create_msaa_view(
    device: &wgpu::Device,
    config: &wgpu::SurfaceConfiguration,
) -> wgpu::TextureView {
    device
        .create_texture(&wgpu::TextureDescriptor {
            label: Some("preview-msaa-color"),
            size: wgpu::Extent3d {
                width: config.width,
                height: config.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: MSAA_SAMPLE_COUNT,
            dimension: wgpu::TextureDimension::D2,
            format: config.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        })
        .create_view(&wgpu::TextureViewDescriptor::default())
}

fn map_origin(bounds: Box3) -> Vec3 {
    Vec3::new(
        (bounds.min.x + bounds.max.x) * 0.5,
        (bounds.min.y + bounds.max.y) * 0.5,
        (bounds.min.z + bounds.max.z) * 0.5,
    )
}

/// Reports the preview camera in the Lineage/Unreal order used by the old UI:
/// X, Y, Z. Preview rendering uses Recast's swapped X, Z, Y basis and recenters
/// map meshes around their bounds, so both transforms are reversed here.
fn camera_location(bounds: Box3, position: [f32; 3]) -> [i32; 3] {
    let origin = map_origin(bounds);
    [
        (position[0] + origin.x) as i32,
        (position[2] + origin.z) as i32,
        (position[1] + origin.y) as i32,
    ]
}

fn add(lhs: [f32; 3], rhs: [f32; 3]) -> [f32; 3] {
    [lhs[0] + rhs[0], lhs[1] + rhs[1], lhs[2] + rhs[2]]
}

fn subtract(lhs: [f32; 3], rhs: [f32; 3]) -> [f32; 3] {
    [lhs[0] - rhs[0], lhs[1] - rhs[1], lhs[2] - rhs[2]]
}

fn scale(value: [f32; 3], amount: f32) -> [f32; 3] {
    [value[0] * amount, value[1] * amount, value[2] * amount]
}

fn dot(lhs: [f32; 3], rhs: [f32; 3]) -> f32 {
    lhs[0] * rhs[0] + lhs[1] * rhs[1] + lhs[2] * rhs[2]
}

fn cross(lhs: [f32; 3], rhs: [f32; 3]) -> [f32; 3] {
    [
        lhs[1] * rhs[2] - lhs[2] * rhs[1],
        lhs[2] * rhs[0] - lhs[0] * rhs[2],
        lhs[0] * rhs[1] - lhs[1] * rhs[0],
    ]
}

fn normalize(value: [f32; 3]) -> [f32; 3] {
    let length = dot(value, value).sqrt();
    if length > 0.000_001 {
        scale(value, 1.0 / length)
    } else {
        [0.0, 0.0, 0.0]
    }
}

fn multiply(lhs: [[f32; 4]; 4], rhs: [[f32; 4]; 4]) -> [[f32; 4]; 4] {
    let mut result = [[0.0; 4]; 4];
    for column in 0..4 {
        for row in 0..4 {
            result[column][row] = (0..4)
                .map(|index| lhs[index][row] * rhs[column][index])
                .sum();
        }
    }
    result
}

fn perspective_rh(fov_y: f32, aspect: f32, near: f32, far: f32) -> [[f32; 4]; 4] {
    let f = 1.0 / (fov_y * 0.5).tan();
    [
        [f / aspect, 0.0, 0.0, 0.0],
        [0.0, f, 0.0, 0.0],
        [0.0, 0.0, far / (near - far), -1.0],
        [0.0, 0.0, near * far / (near - far), 0.0],
    ]
}

fn look_at_rh(eye: [f32; 3], target: [f32; 3], up: [f32; 3]) -> [[f32; 4]; 4] {
    let forward = normalize(subtract(target, eye));
    let side = normalize(cross(forward, up));
    let up = cross(side, forward);
    [
        [side[0], up[0], -forward[0], 0.0],
        [side[1], up[1], -forward[1], 0.0],
        [side[2], up[2], -forward[2], 0.0],
        [-dot(side, eye), -dot(up, eye), dot(forward, eye), 1.0],
    ]
}

const PREVIEW_SHADER: &str = r#"
struct Camera {
    view_projection: mat4x4<f32>,
    position: vec3<f32>,
    _padding: f32,
};

@group(0) @binding(0)
var<uniform> camera: Camera;

struct VertexInput {
    @location(0) position: vec3<f32>,
    @location(1) color: vec4<f32>,
    @location(2) normal: vec3<f32>,
};

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) world_position: vec3<f32>,
    @location(2) normal: vec3<f32>,
};

@vertex
fn vs_main(input: VertexInput) -> VertexOutput {
    var output: VertexOutput;
    output.position = camera.view_projection * vec4<f32>(input.position, 1.0);
    output.color = input.color;
    output.world_position = input.position;
    output.normal = input.normal;
    return output;
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    let base_color = input.color.rgb;
    let light = normalize(camera.position - input.world_position);
    let front_face = dot(input.normal, light) >= 0.0;
    let normal = select(-input.normal, input.normal, front_face);
    let shaded_color = select(base_color * 0.5, base_color, front_face);
    let diffuse = shaded_color * max(dot(normal, light), 0.0);
    let display_color = clamp(shaded_color * (base_color * 0.25 + diffuse), vec3<f32>(0.0), vec3<f32>(1.0));
    return vec4<f32>(srgb_to_linear(display_color), input.color.a);
}

fn srgb_to_linear(color: vec3<f32>) -> vec3<f32> {
    let lower = color / vec3<f32>(12.92);
    let upper = pow((color + vec3<f32>(0.055)) / vec3<f32>(1.055), vec3<f32>(2.4));
    return select(lower, upper, color > vec3<f32>(0.04045));
}
"#;

const GEODATA_SHADER: &str = r#"
struct Camera {
    view_projection: mat4x4<f32>,
    position: vec3<f32>,
    _padding: f32,
};

@group(0) @binding(0)
var<uniform> camera: Camera;

struct VertexInput {
    @location(0) offset: vec2<f32>,
    @location(1) position: vec3<f32>,
    @location(2) scale: f32,
    @location(3) color: vec4<f32>,
};

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs_main(input: VertexInput) -> VertexOutput {
    var output: VertexOutput;
    let point = input.position + vec3<f32>(input.offset.x * input.scale, 0.0, input.offset.y * input.scale);
    output.position = camera.view_projection * vec4<f32>(point, 1.0);
    output.color = input.color;
    return output;
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    return vec4<f32>(srgb_to_linear(input.color.rgb), input.color.a);
}


fn srgb_to_linear(color: vec3<f32>) -> vec3<f32> {
    let lower = color / vec3<f32>(12.92);
    let upper = pow((color + vec3<f32>(0.055)) / vec3<f32>(1.055), vec3<f32>(2.4));
    return select(lower, upper, color > vec3<f32>(0.04045));

}
"#;

const NSWE_ICON_SHADER: &str = r#"
struct Camera {
    view_projection: mat4x4<f32>,
    position: vec3<f32>,
    _padding: f32,
};

@group(0) @binding(0)
var<uniform> camera: Camera;

@group(1) @binding(0)
var icon_atlas: texture_2d<f32>;
@group(1) @binding(1)
var icon_sampler: sampler;

struct VertexInput {
    @location(0) offset: vec2<f32>,
    @location(1) position: vec3<f32>,
    @location(2) scale: f32,
    @location(3) mask: f32,
};

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(input: VertexInput) -> VertexOutput {
    var output: VertexOutput;
    let point = input.position + vec3<f32>(input.offset.x * input.scale, 0.0, input.offset.y * input.scale);
    output.position = camera.view_projection * vec4<f32>(point, 1.0);
    let local_uv = input.offset * 0.5 + vec2<f32>(0.5, 0.5);
    let column = input.mask - 4.0 * floor(input.mask / 4.0);
    let row = floor(input.mask / 4.0);
    output.uv = (vec2<f32>(column, row) + local_uv) * 0.25;
    return output;
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    let icon = textureSample(icon_atlas, icon_sampler, input.uv);
    if icon.a < 0.02 {
        discard;
    }
    return icon;
}
"#;

const TEXTURED_SHADER: &str = r#"
struct Camera {
    view_projection: mat4x4<f32>,
    position: vec3<f32>,
    _padding: f32,
};

@group(0) @binding(0)
var<uniform> camera: Camera;

@group(1) @binding(0)
var material_texture: texture_2d<f32>;
@group(1) @binding(1)
var material_sampler: sampler;
@group(1) @binding(2)
var opacity_texture: texture_2d<f32>;

struct Material {
    alpha_cutoff: f32,
    alpha_source: u32,
    _padding: vec2<u32>,
};
@group(1) @binding(3)
var<uniform> material: Material;

struct VertexInput {
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
};

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) world_position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
};

// Stand-in for the client's baked lightmaps, which this viewer cannot read.
// The three terms are budgeted so a surface facing both the sun and the
// camera lands just under 1.0: any more and light-coloured stone clips to
// flat white and loses all its texture detail.
const SKY_COLOR: vec3<f32> = vec3<f32>(0.36, 0.39, 0.45);
const GROUND_COLOR: vec3<f32> = vec3<f32>(0.20, 0.18, 0.16);
const SUN_DIRECTION: vec3<f32> = vec3<f32>(0.45, 0.80, 0.40);
const SUN_COLOR: vec3<f32> = vec3<f32>(1.05, 0.98, 0.88);
const SUN_INTENSITY: f32 = 0.50;
const HEADLAMP_INTENSITY: f32 = 0.12;

@vertex
fn vs_main(input: VertexInput) -> VertexOutput {
    var output: VertexOutput;
    output.position = camera.view_projection * vec4<f32>(input.position, 1.0);
    output.world_position = input.position;
    output.normal = input.normal;
    output.uv = input.uv;
    return output;
}

// Lighting has to stand in for the client's baked lightmaps, which this
// viewer has no access to. A camera headlamp alone leaves every surface
// facing away from the viewer flat black, so the shading is built from
// three cheap terms that keep detail readable from any angle:
//   * a hemisphere ambient (sky above, bounce below) that never reaches 0,
//   * a fixed world sun, so geometry keeps stable orientation cues while
//     the camera moves,
//   * a weak headlamp, which keeps caves and interiors legible.
// All of it runs in linear light: the texture is sampled through an sRGB
// view and the surface re-encodes on write.
@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    let sample = textureSample(material_texture, material_sampler, input.uv);
    // Raw diffuse alpha can be specularity, not coverage. Only the resolved
    // material graph may opt into it or into a distinct opacity bitmap.
    var alpha = 1.0;
    if material.alpha_source == 1u {
        alpha = sample.a;
    } else if material.alpha_source == 2u {
        alpha = textureSample(opacity_texture, material_sampler, input.uv).a;
    }
    if alpha < material.alpha_cutoff {
        discard;
    }

    let normal = normalize(input.normal);
    let view_direction = normalize(camera.position - input.world_position);

    // Everything below uses the stored normal as-is, and every two-sided
    // term uses the magnitude of its angle.
    //
    // Flipping the normal to face the viewer (`dot(normal, view) < 0`) is
    // the obvious way to light single-sided walls and leaf cards, and it is
    // a trap: for any up-facing surface that dot changes sign exactly at
    // the camera's altitude, and the points at the camera's altitude
    // project to a perfectly straight horizontal line. The result is a
    // razor-sharp seam across the screen at eye level, with all the terrain
    // beyond it flipped to lit-from-below. The rasterizer's `front_facing`
    // is no help either: this scene's winding does not agree with its
    // normals (hence `cull_mode: None`), so it darkens the terrain wholesale.
    let hemisphere = mix(GROUND_COLOR, SKY_COLOR, normal.y * 0.5 + 0.5);
    // Two-sided wrapped diffuse: the magnitude keeps a card lit from either
    // side, and the wrap softens the terminator so a surface just past
    // grazing incidence still shows its texture instead of going black.
    let sun = max((abs(dot(normal, normalize(SUN_DIRECTION))) + 0.3) / 1.3, 0.0);
    let headlamp = HEADLAMP_INTENSITY * abs(dot(normal, view_direction));
    let light = hemisphere + SUN_COLOR * (sun * SUN_INTENSITY) + vec3<f32>(headlamp);

    return vec4<f32>(sample.rgb * light, alpha);
}
"#;

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use crate::unreal::{VisualMaterial, VisualScene, VisualTexture};

    use super::*;

    #[test]
    fn view_frustum_preserves_crossings_and_rejects_fully_outside_bounds() {
        let frustum = ViewFrustum::new(&[
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ]);
        assert!(frustum.intersects([[-0.5, -0.5, 0.0], [0.5, 0.5, 1.0]]));
        // A visible box may have its centre outside the view.
        assert!(frustum.intersects([[-20.0, -0.5, 0.2], [0.1, 0.5, 0.8]]));
        for axis in 0..3 {
            let lower = if axis == 2 { 0.0 } else { -1.0 };
            let mut min = [-0.5, -0.5, 0.2];
            let mut max = [0.5, 0.5, 0.8];
            min[axis] = lower - 2.0;
            max[axis] = lower;
            assert!(
                frustum.intersects([min, max]),
                "touching lower plane {axis}"
            );
            max[axis] = lower - 0.1;
            assert!(
                !frustum.intersects([min, max]),
                "outside lower plane {axis}"
            );
            min[axis] = 1.0;
            max[axis] = 3.0;
            assert!(
                frustum.intersects([min, max]),
                "touching upper plane {axis}"
            );
            min[axis] = 1.1;
            assert!(
                !frustum.intersects([min, max]),
                "outside upper plane {axis}"
            );
        }
    }

    #[test]
    fn viewport_picking_tracks_panel_bounds_and_display_scale() {
        let logical = egui::Rect::from_min_max(egui::pos2(0.0, 112.0), egui::pos2(1088.0, 944.0));
        for scale in [1.0, 1.25, 2.0] {
            let viewport = viewport_pixels(
                logical,
                scale,
                [(1440.0 * scale) as u32, (1000.0 * scale) as u32],
            );
            assert_eq!(viewport_ndc(viewport, viewport.center()), Some([0.0, 0.0]));
            assert_eq!(
                viewport_ndc(viewport, viewport.left_top()),
                Some([-1.0, 1.0])
            );
            assert_eq!(
                viewport_ndc(viewport, viewport.right_bottom()),
                Some([1.0, -1.0])
            );
            assert_eq!(
                viewport_ndc(viewport, viewport.right_center() + egui::vec2(1.0, 0.0)),
                None
            );
            assert_eq!(viewport_ndc(viewport, egui::pos2(100.0, 20.0)), None);
        }
    }

    #[test]
    fn viewport_remains_valid_when_panels_exceed_a_small_surface() {
        let rect = viewport_pixels(
            egui::Rect::from_min_max(egui::pos2(0.0, 112.0), egui::pos2(-200.0, -30.0)),
            1.5,
            [120, 90],
        );
        assert!(rect.width() >= 1.0 && rect.height() >= 1.0);
        assert!(rect.left() >= 0.0 && rect.top() >= 0.0);
        assert!(rect.right() <= 120.0 && rect.bottom() <= 90.0);
    }

    #[test]
    #[ignore = "requires a working GPU adapter"]
    fn opaque_material_keeps_pixels_with_specular_alpha() {
        let opaque = render_textured_pixel([190, 150, 110, 255]).expect("GPU adapter");
        let specular_alpha = render_textured_pixel([190, 150, 110, 0]).expect("GPU adapter");
        assert_eq!(
            specular_alpha, opaque,
            "an opaque material must not turn its specularity mask into transparency"
        );
    }

    #[test]
    #[ignore = "requires a working GPU adapter"]
    fn textured_scene_uses_updated_camera_uniform() {
        let mut triangle = material_triangle([255; 4], 0.5, Default::default(), None);
        for (position, _, _) in &mut triangle.vertices {
            position.x = position.x * 0.25 - 0.5;
            position.y *= 0.25;
        }
        let camera = CameraUniform::new(
            [
                [1.0, 0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
                [0.0, 0.0, 0.0, 1.0],
            ],
            [0.0, 1000.0, 0.0],
        );
        let mut shifted = camera;
        shifted.view_projection[3][0] = 1.0;
        let (side, frames) = render_visual_frames(
            &[VisualScene {
                batches: vec![triangle],
            }],
            &[camera, shifted],
        )
        .expect("GPU adapter");
        let left = ((side / 2 * side + side / 4) * 4) as usize;
        let right = ((side / 2 * side + side * 3 / 4) * 4) as usize;
        assert!(frames[0][left] > 100 && frames[0][right] == 0);
        assert!(
            frames[1][left] == 0 && frames[1][right] > 100,
            "the existing scene must move the triangle after a camera uniform write"
        );
    }

    /// Renders one full-viewport, up-facing triangle through the real
    /// textured pipeline and returns the resolved frame. Exercises the WGSL,
    /// the MSAA attachments and the resolve in one shot.
    ///
    /// The triangle's world positions are its NDC positions (the view
    /// projection is identity), so `camera_position[1]` decides where the
    /// camera's eye level falls inside the frame.
    fn render_textured_frame(texel: [u8; 4], camera_position: [f32; 3]) -> Option<(u32, Vec<u8>)> {
        render_visual_frame(
            &[VisualScene {
                batches: vec![material_triangle(texel, 0.0, Default::default(), None)],
            }],
            camera_position,
        )
    }

    fn material_triangle(
        texel: [u8; 4],
        depth: f32,
        state: VisualMaterialState,
        opacity: Option<[u8; 4]>,
    ) -> VisualBatch {
        let texture = |rgba: [u8; 4]| VisualTexture {
            width: 1,
            height: 1,
            rgba: Rc::from(rgba.as_slice()),
        };
        VisualBatch {
            material: VisualMaterial {
                texture: Some(texture(texel)),
                opacity: opacity.map(texture),
                state,
            },
            vertices: [
                Vec3::new(-1.0, -1.0, depth),
                Vec3::new(3.0, -1.0, depth),
                Vec3::new(-1.0, 3.0, depth),
            ]
            .into_iter()
            .map(|position| (position, Vec3::new(0.0, 1.0, 0.0), [0.0, 0.0]))
            .collect(),
            indices: vec![0, 1, 2],
        }
    }

    fn render_visual_frame(
        scenes: &[VisualScene],
        camera_position: [f32; 3],
    ) -> Option<(u32, Vec<u8>)> {
        // Identity view projection: vertices are given directly in NDC.
        let camera = CameraUniform::new(
            [
                [1.0, 0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
                [0.0, 0.0, 0.0, 1.0],
            ],
            camera_position,
        );
        let (side, mut frames) = render_visual_frames(scenes, &[camera])?;
        Some((side, frames.pop()?))
    }

    fn render_visual_frames(
        scenes: &[VisualScene],
        cameras: &[CameraUniform],
    ) -> Option<(u32, Vec<Vec<u8>>)> {
        const SIDE: u32 = 64;

        let instance = wgpu::Instance::default();
        let adapter =
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))?;
        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("shading-test-device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
            },
            None,
        ))
        .ok()?;

        let format = wgpu::TextureFormat::Rgba8UnormSrgb;
        let camera_layout = create_camera_layout(&device);
        let camera_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: None,
            contents: bytemuck::bytes_of(cameras.first()?),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let camera_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &camera_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: camera_buffer.as_entire_binding(),
            }],
        });

        let material_layout = create_material_texture_layout(&device);
        let resources =
            TexturedPipelineResources::new(&device, &camera_layout, &material_layout, format);
        let mut scenes: Vec<_> = scenes
            .iter()
            .map(|scene| {
                let mut scene = TexturedScene::new(
                    &device,
                    &queue,
                    &material_layout,
                    &resources,
                    scene,
                    Vec3::new(0.0, 0.0, 0.0),
                );
                // Identity projection uses increasing Z for farther surfaces.
                scene.sort_blended([0.0, 0.0, 1.0]);
                scene
            })
            .collect();

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: SIDE,
            height: SIDE,
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        let depth_view = create_depth_view(&device, &config);
        let msaa_view = create_msaa_view(&device, &config);
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width: SIDE,
                height: SIDE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: (SIDE * SIDE * 4) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        // Scene construction happens once; camera and visibility remain live.
        let mut frames = Vec::with_capacity(cameras.len());
        for camera in cameras {
            queue.write_buffer(&camera_buffer, 0, bytemuck::bytes_of(camera));
            for scene in &mut scenes {
                scene.update_view(&camera.view_projection, [0.0, 0.0, 1.0]);
            }
            let mut encoder =
                device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: None,
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &msaa_view,
                        resolve_target: Some(&target_view),
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Discard,
                        },
                    })],
                    depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                        view: &depth_view,
                        depth_ops: Some(wgpu::Operations {
                            load: wgpu::LoadOp::Clear(1.0),
                            store: wgpu::StoreOp::Discard,
                        }),
                        stencil_ops: None,
                    }),
                    timestamp_writes: None,
                    occlusion_query_set: None,
                });
                for scene in &scenes {
                    scene.draw(&mut pass, &camera_bind_group);
                }
            }
            encoder.copy_texture_to_buffer(
                wgpu::ImageCopyTexture {
                    texture: &target,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::ImageCopyBuffer {
                    buffer: &readback,
                    layout: wgpu::ImageDataLayout {
                        offset: 0,
                        bytes_per_row: Some(SIDE * 4),
                        rows_per_image: Some(SIDE),
                    },
                },
                wgpu::Extent3d {
                    width: SIDE,
                    height: SIDE,
                    depth_or_array_layers: 1,
                },
            );
            queue.submit([encoder.finish()]);

            let slice = readback.slice(..);
            slice.map_async(wgpu::MapMode::Read, |_| {});
            device.poll(wgpu::Maintain::Wait);
            let data = slice.get_mapped_range();
            frames.push(data.to_vec());
            drop(data);
            readback.unmap();
        }
        Some((SIDE, frames))
    }

    /// Centre pixel of the default overhead-camera frame.
    fn render_textured_pixel(texel: [u8; 4]) -> Option<[u8; 4]> {
        render_material_pixel(texel, Default::default(), None)
    }

    fn render_material_pixel(
        texel: [u8; 4],
        state: VisualMaterialState,
        opacity: Option<[u8; 4]>,
    ) -> Option<[u8; 4]> {
        render_scene_pixel(&[VisualScene {
            batches: vec![material_triangle(texel, 0.0, state, opacity)],
        }])
    }

    fn render_scene_pixel(scenes: &[VisualScene]) -> Option<[u8; 4]> {
        let (side, pixels) = render_visual_frame(scenes, [0.0, 1000.0, 0.0])?;
        let middle = ((side / 2 * side + side / 2) * 4) as usize;
        Some([
            pixels[middle],
            pixels[middle + 1],
            pixels[middle + 2],
            pixels[middle + 3],
        ])
    }

    /// Renders a client map's textured scene offscreen and writes it to a
    /// PNG, through the same pipeline, MSAA attachments and resolve the
    /// editor uses. This is how a render artefact gets inspected without
    /// driving the GUI window.
    ///
    /// `GEODATA_EDITOR_MAP` picks the package (default `17_22_Classic`) and
    /// `GEODATA_EDITOR_CAM` ("x,y,z,yaw_deg,pitch_deg") replaces the
    /// overview shot with a specific viewpoint.
    #[test]
    #[ignore = "renders a PNG for human inspection; requires GEODATA_EDITOR_CLIENT and a GPU"]
    fn renders_client_map_to_png() {
        const WIDTH: u32 = 1280;
        const HEIGHT: u32 = 720;

        let client = std::env::var("GEODATA_EDITOR_CLIENT").expect("set GEODATA_EDITOR_CLIENT");
        let package = std::env::var("GEODATA_EDITOR_MAP").unwrap_or("17_22_Classic".into());
        let loader = PackageLoader::new(PathBuf::from(&client), 0, false);
        let source_map = loader.load_map(&package).expect("load map");
        let scene = loader
            .load_visual_scene(&package)
            .expect("load visual scene");
        let origin = map_origin(source_map.bounds);

        let instance = wgpu::Instance::default();
        let adapter =
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
                .expect("no GPU adapter");
        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("offscreen-device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
            },
            None,
        ))
        .expect("create device");

        let format = wgpu::TextureFormat::Rgba8UnormSrgb;
        let camera_layout = create_camera_layout(&device);

        let mut camera = Camera::for_bounds(source_map.bounds);
        // "x,y,z,yaw_deg,pitch_deg" overrides the overview shot.
        if let Ok(values) = std::env::var("GEODATA_EDITOR_CAM") {
            let parsed: Vec<f32> = values
                .split(',')
                .filter_map(|value| value.trim().parse().ok())
                .collect();
            if parsed.len() == 5 {
                camera.position = [parsed[0], parsed[1], parsed[2]];
                camera.yaw = parsed[3].to_radians();
                camera.pitch = parsed[4].to_radians();
            }
        }
        let uniform = CameraUniform::new(camera.matrix(WIDTH, HEIGHT), camera.position);
        let camera_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: None,
            contents: bytemuck::bytes_of(&uniform),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let camera_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &camera_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: camera_buffer.as_entire_binding(),
            }],
        });

        let material_layout = create_material_texture_layout(&device);
        let resources =
            TexturedPipelineResources::new(&device, &camera_layout, &material_layout, format);
        let mut textured = TexturedScene::new(
            &device,
            &queue,
            &material_layout,
            &resources,
            &scene,
            origin,
        );
        // Keep batch isolation while exercising the same sorted, material-aware
        // drawing path as the interactive editor.
        let only = std::env::var("GEODATA_EDITOR_ONLY_BATCH")
            .ok()
            .and_then(|value| value.parse::<usize>().ok());
        let skip = std::env::var("GEODATA_EDITOR_SKIP_BATCH")
            .ok()
            .and_then(|value| value.parse::<usize>().ok());
        let visible =
            |index: usize| only.is_none_or(|wanted| wanted == index) && skip != Some(index);
        textured.opaque_order.retain(|index| visible(*index));
        textured.blended_order.retain(|(index, _)| visible(*index));
        textured.update_view(&uniform.view_projection, camera.forward());

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: WIDTH,
            height: HEIGHT,
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        let depth_view = create_depth_view(&device, &config);
        let msaa_view = create_msaa_view(&device, &config);
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width: WIDTH,
                height: HEIGHT,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: (WIDTH * HEIGHT * 4) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &msaa_view,
                    resolve_target: Some(&target_view),
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Discard,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &depth_view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Discard,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            textured.draw(&mut pass, &camera_bind_group);
        }
        encoder.copy_texture_to_buffer(
            wgpu::ImageCopyTexture {
                texture: &target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::ImageCopyBuffer {
                buffer: &readback,
                layout: wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(WIDTH * 4),
                    rows_per_image: Some(HEIGHT),
                },
            },
            wgpu::Extent3d {
                width: WIDTH,
                height: HEIGHT,
                depth_or_array_layers: 1,
            },
        );
        queue.submit([encoder.finish()]);

        let slice = readback.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        device.poll(wgpu::Maintain::Wait);
        let pixels = slice.get_mapped_range().to_vec();

        let path = std::env::temp_dir().join(format!("geodata_editor_{package}.png"));
        let file = std::fs::File::create(&path).expect("create png");
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), WIDTH, HEIGHT);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .expect("png header")
            .write_image_data(&pixels)
            .expect("png data");
        println!(
            "camera position {:?} yaw {:.1} pitch {:.1}\nwrote {}",
            camera.position,
            camera.yaw.to_degrees(),
            camera.pitch.to_degrees(),
            path.display()
        );
    }

    #[test]
    #[ignore = "requires a working GPU adapter"]
    fn textured_surfaces_are_lit_without_clipping_to_white() {
        let Some(pixel) = render_textured_pixel([255, 255, 255, 255]) else {
            panic!("no GPU adapter available");
        };

        // A white surface facing both the sun and the camera is the
        // brightest case in the scene. It must stay short of 255: once it
        // clips, light stone loses every bit of texture detail.
        for (channel, value) in pixel[..3].iter().enumerate() {
            assert!(
                (200..255).contains(value),
                "channel {channel} rendered {value}, expected a bright but unclipped value"
            );
        }

        // Mid-grey must land clearly below the white case, otherwise the
        // lighting is washing the albedo out.
        let grey = render_textured_pixel([128, 128, 128, 255]).expect("second render");
        assert!(
            grey[0] < pixel[0] - 30,
            "mid-grey ({}) is too close to white ({})",
            grey[0],
            pixel[0]
        );
        assert!(grey[0] > 60, "mid-grey rendered too dark: {}", grey[0]);
    }

    #[test]
    #[ignore = "requires a working GPU adapter"]
    fn masked_texels_are_cut_out_instead_of_blended() {
        // Foliage cards rely on this: an almost transparent texel has to
        // leave the background untouched and write no depth.
        let pixel = render_material_pixel(
            [255, 255, 255, 25],
            VisualMaterialState {
                alpha_cutoff: Some(128),
                use_texture_alpha: true,
                ..Default::default()
            },
            None,
        )
        .expect("GPU adapter");
        assert_eq!(
            pixel[..3],
            [0, 0, 0],
            "masked texel was drawn instead of discarded"
        );
    }

    #[test]
    #[ignore = "requires a working GPU adapter"]
    fn masked_material_honors_authored_alpha_threshold_boundary() {
        let state = VisualMaterialState {
            alpha_cutoff: Some(20),
            use_texture_alpha: true,
            ..Default::default()
        };
        let below = render_material_pixel([190, 150, 110, 19], state, None).expect("GPU adapter");
        let equal = render_material_pixel([190, 150, 110, 20], state, None).expect("GPU adapter");
        let opaque = render_textured_pixel([190, 150, 110, 255]).expect("GPU adapter");
        assert_eq!(below[..3], [0, 0, 0]);
        assert_eq!(equal[..3], opaque[..3], "the authored cutoff is inclusive");

        let zero = render_material_pixel(
            [190, 150, 110, 0],
            VisualMaterialState {
                alpha_cutoff: Some(0),
                ..state
            },
            None,
        )
        .expect("GPU adapter");
        assert_eq!(
            zero[..3],
            opaque[..3],
            "AlphaRef=0 must not invent a cutoff"
        );
    }

    #[test]
    #[ignore = "requires a working GPU adapter"]
    fn separate_opacity_controls_coverage_without_replacing_diffuse_color() {
        let state = VisualMaterialState {
            alpha_cutoff: Some(20),
            ..Default::default()
        };
        let hidden = render_material_pixel([190, 150, 110, 255], state, Some([255, 255, 255, 19]))
            .expect("GPU adapter");
        let visible = render_material_pixel([190, 150, 110, 0], state, Some([0, 255, 0, 20]))
            .expect("GPU adapter");
        let opaque = render_textured_pixel([190, 150, 110, 255]).expect("GPU adapter");
        assert_eq!(hidden[..3], [0, 0, 0]);
        assert_eq!(
            visible[..3],
            opaque[..3],
            "opacity RGB must not tint diffuse"
        );
    }

    fn assert_linear_pixel(pixel: [u8; 4], expected: [f32; 3]) {
        for channel in 0..3 {
            let expected = linear_to_srgb(expected[channel]);
            assert!(
                pixel[channel].abs_diff(expected) <= 2,
                "channel {channel}: rendered {}, expected {expected} (pixel {pixel:?})",
                pixel[channel],
            );
        }
    }

    #[test]
    #[ignore = "requires a working GPU adapter"]
    fn material_blend_modes_composite_with_unreal_framebuffer_factors() {
        let source_texel = [180, 110, 60, 128];
        let background_texel = [70, 100, 160, 255];
        let source = render_textured_pixel(source_texel).expect("GPU adapter");
        let background = render_textured_pixel(background_texel).expect("GPU adapter");
        let alpha = 128.0 / 255.0;
        for blend in [
            VisualBlend::Alpha,
            VisualBlend::AlphaModulate,
            VisualBlend::Translucent,
            VisualBlend::Modulate,
            VisualBlend::Brighten,
            VisualBlend::Darken,
        ] {
            let pixel = render_scene_pixel(&[VisualScene {
                batches: vec![
                    material_triangle(
                        source_texel,
                        0.2,
                        VisualMaterialState {
                            blend,
                            use_texture_alpha: true,
                            depth_write: false,
                            ..Default::default()
                        },
                        None,
                    ),
                    material_triangle(background_texel, 0.8, Default::default(), None),
                ],
            }])
            .expect("GPU adapter");
            let expected = std::array::from_fn(|channel| {
                let src = SRGB_TO_LINEAR[source[channel] as usize];
                let dst = SRGB_TO_LINEAR[background[channel] as usize];
                match blend {
                    VisualBlend::Alpha => src * alpha + dst * (1.0 - alpha),
                    VisualBlend::AlphaModulate => src * dst + dst * (1.0 - alpha),
                    VisualBlend::Translucent => src + dst * (1.0 - src),
                    VisualBlend::Modulate => 2.0 * src * dst,
                    VisualBlend::Brighten => src + dst,
                    VisualBlend::Darken => dst * (1.0 - src),
                    VisualBlend::Opaque | VisualBlend::Invisible => unreachable!(),
                }
            });
            assert_linear_pixel(pixel, expected);
        }
    }

    #[test]
    #[ignore = "requires a working GPU adapter"]
    fn alpha_surfaces_sort_back_to_front_after_opaque_geometry() {
        let state = VisualMaterialState {
            blend: VisualBlend::Alpha,
            use_texture_alpha: true,
            depth_write: false,
            ..Default::default()
        };
        let near = material_triangle([255, 0, 0, 128], 0.2, state, None);
        let middle = material_triangle([0, 255, 0, 128], 0.5, state, None);
        let far = material_triangle([0, 0, 255, 255], 0.8, Default::default(), None);
        let pixel = render_scene_pixel(&[VisualScene {
            batches: vec![near, far, middle],
        }])
        .expect("GPU adapter");
        let white = render_textured_pixel([255, 255, 255, 255]).expect("GPU adapter");
        let alpha = 128.0 / 255.0;
        assert_linear_pixel(
            pixel,
            [
                SRGB_TO_LINEAR[white[0] as usize] * alpha,
                SRGB_TO_LINEAR[white[1] as usize] * alpha * (1.0 - alpha),
                SRGB_TO_LINEAR[white[2] as usize] * (1.0 - alpha) * (1.0 - alpha),
            ],
        );
    }

    #[test]
    #[ignore = "requires a working GPU adapter"]
    fn blended_material_depth_flags_control_later_fragments() {
        let background = || material_triangle([0, 0, 255, 255], 0.8, Default::default(), None);
        let later = || material_triangle([0, 255, 0, 255], 0.5, Default::default(), None);
        let green = render_scene_pixel(&[VisualScene {
            batches: vec![later()],
        }])
        .expect("GPU adapter");
        let state = VisualMaterialState {
            blend: VisualBlend::Alpha,
            use_texture_alpha: true,
            depth_write: false,
            ..Default::default()
        };
        let near = || material_triangle([255, 0, 0, 128], 0.2, state, None);
        let composed = render_scene_pixel(&[VisualScene {
            batches: vec![background(), near()],
        }])
        .expect("GPU adapter");
        for depth_write in [false, true] {
            let mut near = near();
            near.material.state.depth_write = depth_write;
            let pixel = render_scene_pixel(&[
                VisualScene {
                    batches: vec![background(), near],
                },
                VisualScene {
                    batches: vec![later()],
                },
            ])
            .expect("GPU adapter");
            assert_eq!(pixel, if depth_write { composed } else { green });
        }

        // Disabling depth testing must also let a blended surface behind an
        // opaque wall contribute; enabling it must leave the wall untouched.
        let wall = || material_triangle([0, 255, 0, 255], 0.1, Default::default(), None);
        let wall_pixel = render_scene_pixel(&[VisualScene {
            batches: vec![wall()],
        }])
        .expect("GPU adapter");
        for depth_test in [false, true] {
            let mut behind = near();
            behind.material.state.depth_test = depth_test;
            let pixel = render_scene_pixel(&[VisualScene {
                batches: vec![wall(), behind],
            }])
            .expect("GPU adapter");
            if depth_test {
                assert_eq!(pixel, wall_pixel);
            } else {
                let alpha = 128.0 / 255.0;
                assert_linear_pixel(
                    pixel,
                    [
                        SRGB_TO_LINEAR[composed[0] as usize],
                        SRGB_TO_LINEAR[wall_pixel[1] as usize] * (1.0 - alpha),
                        0.0,
                    ],
                );
            }
        }
    }

    #[test]
    #[ignore = "requires a working GPU adapter"]
    fn shading_has_no_seam_at_the_camera_eye_level() {
        // One flat, up-facing surface straddling the camera's altitude. Any
        // view-dependent normal flip splits it exactly at eye level, which
        // in a real map reads as a straight horizontal line across the
        // screen with all the terrain beyond it darkened.
        let Some((side, pixels)) = render_textured_frame([200, 200, 200, 255], [0.0, 0.0, 2.0])
        else {
            panic!("no GPU adapter available");
        };

        let sample = |row: u32| -> [u8; 3] {
            let offset = ((row * side + side / 2) * 4) as usize;
            [pixels[offset], pixels[offset + 1], pixels[offset + 2]]
        };
        let above = sample(side / 4); // above eye level
        let below = sample(side * 3 / 4); // below eye level

        for channel in 0..3 {
            let difference = above[channel].abs_diff(below[channel]);
            assert!(
                difference <= 4,
                "channel {channel} differs by {difference} across the camera's eye level \
                 ({} above vs {} below)",
                above[channel],
                below[channel]
            );
        }
    }
    #[test]
    #[ignore = "requires GEODATA_EDITOR_CLIENT and GEODATA_EDITOR_L2J pointing to a local client"]
    fn boots_the_client_package_of_each_map_type() {
        let root = std::env::var("GEODATA_EDITOR_CLIENT").expect("set GEODATA_EDITOR_CLIENT");
        let input = std::env::var("GEODATA_EDITOR_L2J").expect("set GEODATA_EDITOR_L2J");
        let region = editor::geodata_region(Path::new(&input)).expect("region of the geodata");
        for map_type in MapType::ALL {
            let loader = PackageLoader::new(PathBuf::from(&root), 0, false);
            let package = map_package_or_prompt(&loader, &region, map_type)
                .expect("both map flavours exist for this region");
            let source_map = loader.load_map(&package).expect("load the client package");
            assert_eq!(source_map.name, map_type.package_name(&region));
        }
    }

    #[test]
    #[ignore = "requires GEODATA_EDITOR_CLIENT pointing to a local real Lineage II client"]
    fn a_real_client_ships_regions_in_only_one_flavour() {
        // Real clients are not symmetric: this is what makes the fallback
        // necessary rather than defensive. Whichever side is missing, the
        // other one has to be the offer.
        let root = std::env::var("GEODATA_EDITOR_CLIENT").expect("set GEODATA_EDITOR_CLIENT");
        let root = PathBuf::from(root);
        let loader = PackageLoader::new(root.clone(), 0, false);

        let mut regions: Vec<String> = std::fs::read_dir(root.join("Maps"))
            .expect("read Maps")
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                let name = entry.file_name().to_str()?.to_owned();
                let stem = name.strip_suffix(".unr")?;
                Some(stem.strip_suffix("_Classic").unwrap_or(stem).to_owned())
            })
            .collect();
        regions.sort();
        regions.dedup();

        let mut only_classic = 0;
        let mut only_normal = 0;
        let mut offered: Option<String> = None;
        for region in &regions {
            match map_package_availability(&loader, region, MapType::Normal) {
                PackageAvailability::OnlyOther(MapType::Classic, package) => {
                    assert_eq!(package, format!("{region}_Classic"));
                    only_classic += 1;
                }
                PackageAvailability::Selected(package) => assert_eq!(&package, region),
                other => panic!("unexpected availability for {region}: {other:?}"),
            }
            match map_package_availability(&loader, region, MapType::Classic) {
                PackageAvailability::OnlyOther(MapType::Normal, package) => {
                    assert_eq!(&package, region);
                    offered.get_or_insert(package);
                    only_normal += 1;
                }
                PackageAvailability::Selected(package) => {
                    assert_eq!(package, format!("{region}_Classic"))
                }
                other => panic!("unexpected availability for {region}: {other:?}"),
            }
        }
        println!(
            "{} regions: {only_classic} only classic, {only_normal} only normal",
            regions.len()
        );
        assert!(
            only_classic + only_normal > 0,
            "expected at least one region shipped in a single flavour"
        );

        // Accepting the offer has to lead somewhere: the package that was
        // proposed must parse, not merely exist on disk.
        let offered = offered.expect("a region shipped only under the normal name");
        let source_map = loader
            .load_map(&offered)
            .expect("the offered package must load");
        assert_eq!(source_map.name, offered);
    }

    #[test]
    fn a_missing_map_flavour_falls_back_to_the_one_the_client_ships() {
        // A classic client names the region `25_25_Classic`, a normal one
        // `25_25`. This client only ships the normal name.
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock is after the epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("geodata-editor-flavour-{nonce}"));
        std::fs::create_dir_all(root.join("Maps")).expect("create fake client");
        std::fs::write(root.join("Maps").join("25_25.unr"), b"not a real package")
            .expect("create fake package");
        let loader = PackageLoader::new(root.clone(), 0, false);

        assert_eq!(
            map_package_availability(&loader, "25_25", MapType::Classic),
            PackageAvailability::OnlyOther(MapType::Normal, "25_25".into()),
            "the classic name is absent, so the normal one must be offered"
        );
        assert_eq!(
            map_package_availability(&loader, "25_25", MapType::Normal),
            PackageAvailability::Selected("25_25".into()),
            "the selected flavour exists and must be used as is"
        );
        assert_eq!(
            map_package_availability(&loader, "26_26", MapType::Classic),
            PackageAvailability::Neither
        );

        // And the reverse direction, on a client that only ships classic.
        std::fs::rename(
            root.join("Maps").join("25_25.unr"),
            root.join("Maps").join("30_30_Classic.unr"),
        )
        .expect("rename fake package");
        assert_eq!(
            map_package_availability(&loader, "30_30", MapType::Normal),
            PackageAvailability::OnlyOther(MapType::Classic, "30_30_Classic".into())
        );

        // The prompt the UI shows carries both names and the flavour to
        // switch to, so accepting it can load without asking again.
        let pending = map_package_or_prompt(&loader, "30_30", MapType::Normal)
            .expect_err("the normal name is absent")
            .expect("the classic name exists, so a prompt is expected");
        assert_eq!(pending.missing, "30_30");
        assert_eq!(pending.available, "30_30_Classic");
        assert_eq!(pending.available_type, MapType::Classic);
        assert_eq!(
            pending.question(),
            "O mapa 30_30 não existe, mas o 30_30_Classic existe. Quer usar ele?"
        );
        assert_eq!(
            map_package_or_prompt(&loader, "26_26", MapType::Classic).expect_err("absent"),
            None,
            "neither flavour exists, so there is nothing to offer"
        );

        // What the "Usar ..." button does: switch the map type, then reopen.
        // The second pass must resolve straight to a package.
        assert_eq!(
            map_package_or_prompt(&loader, "30_30", pending.available_type),
            Ok("30_30_Classic".into()),
            "accepting the offer has to load instead of asking again"
        );

        std::fs::remove_dir_all(&root).expect("remove fake client");
    }

    #[test]
    fn editor_cells_and_selection_use_high_visibility_colours() {
        let open = Layer {
            height: 0,
            nswe: Layer::OPEN,
        };
        assert_eq!(editor_cell_color(open, false), [0, 205, 230, 165]);
        assert_eq!(
            editor_cell_color(Layer { height: 0, nswe: 0 }, false),
            [235, 45, 45, 230]
        );
        assert_eq!(editor_cell_color(open, true), [255, 235, 0, 255]);
        assert_eq!(
            editor_cell_color(Layer { height: 0, nswe: 0 }, true),
            [255, 235, 0, 255]
        );
    }

    #[test]
    fn rectangular_brush_selects_exact_configured_area_around_click() {
        let document = Document::blank();
        let center = LayerAddress::new(100, 200, 0);

        assert_eq!(centered_axis_bounds(center.x, 10), (95, 105));
        assert_eq!(centered_axis_bounds(center.y, 4), (198, 202));

        let selection = brush_area_selection(&document, center, 10, 4, BrushAnchor::Center, false);
        assert_eq!(selection.len(), 40);
        assert!(
            selection
                .iter()
                .all(|cell| { (95..105).contains(&cell.x) && (198..202).contains(&cell.y) })
        );

        assert_eq!(centered_axis_bounds(0, 10), (0, 10));
        assert_eq!(centered_axis_bounds(2047, 4), (2044, 2048));
    }

    #[test]
    fn directional_brush_uses_clicked_cell_as_requested_pointer() {
        let document = Document::blank();
        let pointer = LayerAddress::new(100, 200, 0);

        let from_left = brush_area_selection(&document, pointer, 20, 10, BrushAnchor::Left, false);
        assert_eq!(from_left.len(), 200);
        assert!(
            from_left
                .iter()
                .all(|cell| { (100..120).contains(&cell.x) && (191..201).contains(&cell.y) })
        );

        let from_right =
            brush_area_selection(&document, pointer, 20, 10, BrushAnchor::Right, false);
        assert_eq!(from_right.len(), 200);
        assert!(
            from_right
                .iter()
                .all(|cell| { (81..101).contains(&cell.x) && (191..201).contains(&cell.y) })
        );
    }

    #[test]
    fn simple_selection_colours_the_whole_authored_block() {
        let mut document = Document::blank();
        let address = LayerAddress::new(13, 22, 0);
        let lookup = EditorSelectionLookup::new(&document, &[address]);

        assert!(lookup.contains_simple_block(1, 2));
        assert!(!lookup.contains_cell(13, 22, 0));

        document.convert_simple_to_complex(1, 2).unwrap();
        let lookup = EditorSelectionLookup::new(&document, &[address]);

        assert!(!lookup.contains_simple_block(1, 2));
        assert!(lookup.contains_cell(13, 22, 0));
    }
    #[test]
    fn fully_open_filter_hides_only_blocks_without_restrictions() {
        let mut document = Document::blank();
        assert!(block_is_fully_open(&document, 0, 0));

        document.force_set_nswe([LayerAddress::new(0, 0, 0)], 0, "bloquear");

        assert!(!block_is_fully_open(&document, 0, 0));
        assert!(block_is_fully_open(&document, 1, 0));
    }

    #[test]
    fn hidden_fully_open_blocks_cannot_be_picked() {
        let document = Document::blank();
        let bounds = Box3::new(
            Vec3::new(0.0, -100.0, 0.0),
            Vec3::new(32_768.0, 100.0, 32_768.0),
        );

        assert_eq!(
            pick_l2j_ray(
                &document,
                bounds,
                [8.0, 50.0, 8.0],
                [0.0, -1.0, 0.0],
                None,
                false,
            ),
            Some(LayerAddress::new(0, 0, 0))
        );
        assert_eq!(
            pick_l2j_ray(
                &document,
                bounds,
                [8.0, 50.0, 8.0],
                [0.0, -1.0, 0.0],
                None,
                true,
            ),
            None
        );
    }

    #[test]
    fn hidden_filter_skips_open_cells_inside_mixed_blocks() {
        let mut document = Document::blank();
        document.force_set_nswe([LayerAddress::new(0, 0, 0)], 0, "bloquear");
        let bounds = Box3::new(
            Vec3::new(0.0, -100.0, 0.0),
            Vec3::new(32_768.0, 100.0, 32_768.0),
        );

        assert_eq!(
            pick_l2j_ray(
                &document,
                bounds,
                [8.0, 50.0, 8.0],
                [0.0, -1.0, 0.0],
                None,
                true,
            ),
            Some(LayerAddress::new(0, 0, 0))
        );
        assert_eq!(
            pick_l2j_ray(
                &document,
                bounds,
                [24.0, 50.0, 8.0],
                [0.0, -1.0, 0.0],
                None,
                true,
            ),
            None
        );
    }

    #[test]
    fn nswe_icon_atlas_contains_all_sixteen_status_glyphs() {
        let atlas = nswe_icon_atlas();
        assert_eq!(atlas.len(), 1024 * 1024 * 4);
        for mask in 0..16_usize {
            let origin_x = (mask % 4) * 256;
            let origin_y = (mask / 4) * 256;
            let contains_opaque_pixel = (0..256).any(|row| {
                (0..256)
                    .any(|column| atlas[((origin_y + row) * 1024 + origin_x + column) * 4 + 3] > 0)
            });
            assert!(contains_opaque_pixel, "mask {mask} has no visible glyph");
        }
    }

    #[test]
    fn ray_grid_interval_clips_a_view_ray_to_the_map() {
        let bounds = Box3::new(
            Vec3::new(100.0, -100.0, 200.0),
            Vec3::new(200.0, 100.0, 300.0),
        );
        let (enter, exit) = ray_grid_interval([50.0, 40.0, 250.0], [1.0, -1.0, 0.0], bounds)
            .expect("the ray crosses the map");

        assert!((enter - 50.0).abs() < f32::EPSILON);
        assert!((exit - 150.0).abs() < f32::EPSILON);
    }

    #[test]
    fn ray_grid_interval_rejects_a_parallel_ray_outside_the_map() {
        let bounds = Box3::new(Vec3::new(0.0, 0.0, 0.0), Vec3::new(100.0, 100.0, 100.0));
        assert!(ray_grid_interval([20.0, 0.0, 120.0], [1.0, -1.0, 0.0], bounds).is_none());
    }

    #[test]
    fn l2j_pick_hits_the_real_step_instead_of_a_guessed_height() {
        // The first block is complex.  Its cell (3, 0) is a high step; every
        // other cell and every later block is a flat simple cell at height 0.
        let mut bytes = Vec::with_capacity(crate::l2j::BLOCK_COUNT * 3 + 126);
        bytes.push(1);
        for column in 0..64 {
            let height = if column == 3 * 8 { 160_i16 } else { 0 };
            bytes.extend_from_slice(&((height << 1) | 15).to_le_bytes());
        }
        for _ in 1..crate::l2j::BLOCK_COUNT {
            bytes.push(0);
            bytes.extend_from_slice(&0_i16.to_le_bytes());
        }
        let document = Document::from_bytes(bytes).expect("synthetic L2J is valid");
        let bounds = Box3::new(
            Vec3::new(0.0, -100.0, 0.0),
            Vec3::new(32_768.0, 1_000.0, 32_768.0),
        );

        // At y=160 this ray reaches x=48, exactly inside geo cell (3, 0).
        // A height=0 first-pass estimate would instead jump to x=208.
        assert_eq!(
            pick_l2j_ray(
                &document,
                bounds,
                [8.0, 200.0, 8.0],
                [1.0, -1.0, 0.0],
                None,
                false,
            ),
            Some(LayerAddress::new(3, 0, 0))
        );
    }

    #[test]
    fn l2j_pick_selects_the_frontmost_multilayer_surface() {
        let mut bytes = Vec::with_capacity(crate::l2j::BLOCK_COUNT * 3 + 130);
        bytes.push(2);
        for column in 0..64 {
            if column == 0 {
                bytes.push(2);
                bytes.extend_from_slice(&15_i16.to_le_bytes());
                bytes.extend_from_slice(&((100_i16 << 1) | 15).to_le_bytes());
            } else {
                bytes.push(1);
                bytes.extend_from_slice(&15_i16.to_le_bytes());
            }
        }
        for _ in 1..crate::l2j::BLOCK_COUNT {
            bytes.push(0);
            bytes.extend_from_slice(&0_i16.to_le_bytes());
        }
        let document = Document::from_bytes(bytes).expect("synthetic multilayer L2J is valid");
        let bounds = Box3::new(
            Vec3::new(0.0, -100.0, 0.0),
            Vec3::new(32_768.0, 1_000.0, 32_768.0),
        );

        assert_eq!(
            pick_l2j_ray(
                &document,
                bounds,
                [8.0, 200.0, 8.0],
                [0.0, -1.0, 0.0],
                None,
                false,
            ),
            Some(LayerAddress::new(0, 0, 1))
        );
        assert_eq!(
            pick_l2j_ray(
                &document,
                bounds,
                [8.0, 200.0, 8.0],
                [0.0, -1.0, 0.0],
                Some(0),
                false,
            ),
            Some(LayerAddress::new(0, 0, 0))
        );
    }

    #[test]
    fn ctrl_line_selection_includes_every_cell_between_two_endpoints() {
        let cells = rasterized_line(LayerAddress::new(1, 9, 0), LayerAddress::new(10, 9, 0));
        assert_eq!(cells.len(), 10);
        assert_eq!(cells.first(), Some(&(1, 9)));
        assert_eq!(cells.last(), Some(&(10, 9)));
        assert!(
            cells
                .iter()
                .enumerate()
                .all(|(index, cell)| *cell == (index + 1, 9))
        );
    }

    #[test]
    fn simple_block_expands_to_its_sixty_four_editable_cells() {
        let document = Document::blank();
        let cells = block_layer_selection(&document, 12, 34, 0);

        assert_eq!(cells.len(), 64);
        assert_eq!(cells.first(), Some(&LayerAddress::new(96, 272, 0)));
        assert_eq!(cells.last(), Some(&LayerAddress::new(103, 279, 0)));
        assert!(
            cells
                .iter()
                .all(|address| document.cell(*address).is_some())
        );
        assert!(block_layer_selection(&document, 12, 34, 1).is_empty());
    }

    #[test]
    fn ctrl_flexible_selection_follows_a_curved_partial_strip() {
        // The only partial cells in the first complex block form an L-shaped
        // strip. A direct range would cut through open cells, while the
        // flexible Ctrl selection must stay on the authored orange strip.
        let curved_strip = [(0, 0), (1, 0), (2, 0), (2, 1), (2, 2), (3, 2)];
        let mut bytes = Vec::with_capacity(crate::l2j::BLOCK_COUNT * 3 + 126);
        bytes.push(1);
        for local_x in 0..8 {
            for local_y in 0..8 {
                let mask = curved_strip
                    .contains(&(local_x, local_y))
                    .then_some(1_i16)
                    .unwrap_or(15);
                bytes.extend_from_slice(&mask.to_le_bytes());
            }
        }
        for _ in 1..crate::l2j::BLOCK_COUNT {
            bytes.push(0);
            bytes.extend_from_slice(&0_i16.to_le_bytes());
        }
        let document = Document::from_bytes(bytes).expect("synthetic curved L2J is valid");

        let route = flexible_line_selection(
            &document,
            LayerAddress::new(0, 0, 0),
            LayerAddress::new(3, 2, 0),
        )
        .expect("the connected partial strip should be selected");
        let route_cells = route
            .into_iter()
            .map(|address| (address.x, address.y))
            .collect::<Vec<_>>();
        assert_eq!(route_cells, curved_strip);
    }

    #[test]
    fn look_at_keeps_the_camera_origin_finite() {
        let matrix = look_at_rh([0.0, 10.0, 10.0], [0.0, 0.0, 0.0], [0.0, 1.0, 0.0]);
        assert!(matrix.into_iter().flatten().all(f32::is_finite));
    }

    #[test]
    fn textured_vertices_share_the_collision_mesh_space() {
        // A geodata region sits at absolute Lineage II coordinates, so the
        // preview rebases every mesh onto `map_origin`. A textured batch
        // left in absolute space renders tens of thousands of units away
        // from the collision geometry and geodata cells it overlays.
        let bounds = Box3::new(
            Vec3::new(-98_304.0, -3_205.0, 131_072.0),
            Vec3::new(-65_536.0, 1_773.0, 163_840.0),
        );
        let origin = map_origin(bounds);
        let corner = Vec3::new(-98_304.0, -3_205.0, 131_072.0);
        let batch = VisualBatch {
            material: Default::default(),
            vertices: vec![(corner, Vec3::new(0.0, 1.0, 0.0), [0.25, 0.75])],
            indices: vec![0],
        };

        let textured = textured_batch_vertices(&batch, origin);
        let collision = source_collision_mesh(
            &[Triangle {
                a: corner,
                b: corner,
                c: corner,
            }],
            origin,
            [1.0; 4],
        );

        assert_eq!(textured.len(), 1);
        assert_eq!(textured[0].position, collision.vertices[0].position);
        assert_eq!(textured[0].uv, [0.25, 0.75]);
    }

    #[test]
    fn mip_downsampling_averages_in_linear_light() {
        // A black/white checkerboard is half the light, and half the light
        // is sRGB ~188, not the byte midpoint 128. Averaging the encoded
        // bytes is what made distant tiled surfaces read as dark smears.
        let checkerboard = [
            0, 0, 0, 255, 255, 255, 255, 255, // row 0: black, white
            255, 255, 255, 255, 0, 0, 0, 255, // row 1: white, black
        ];

        let (width, height, mip) = downsample_rgba(2, 2, &checkerboard);

        assert_eq!((width, height), (1, 1));
        assert_eq!(mip[3], 255, "opaque texels must stay opaque");
        for channel in 0..3 {
            assert!(
                (180..=190).contains(&mip[channel]),
                "channel {channel} averaged to {} in gamma space, expected ~182 (50% linear)",
                mip[channel]
            );
        }
    }

    #[test]
    fn srgb_round_trip_preserves_every_channel_value() {
        // The mip chain rides on this pair being each other's inverse; a
        // drift here would tint every generated level.
        for value in 0..=255_u8 {
            let round_tripped = linear_to_srgb(SRGB_TO_LINEAR[value as usize]);
            assert_eq!(round_tripped, value, "sRGB round trip drifted at {value}");
        }
    }

    #[test]
    fn forward_movement_follows_camera_pitch() {
        let mut input = CameraInput::default();
        input.pressed.insert(KeyCode::KeyW);
        let mut camera = Camera {
            position: [0.0, 0.0, 0.0],
            yaw: 0.0,
            pitch: std::f32::consts::FRAC_PI_4,
            speed: 100.0,
        };

        input.update_camera(&mut camera, 0.5);

        assert!(camera.position[0] > 0.0);
        assert!(camera.position[1] > 0.0);
        assert_eq!(camera.position[2], 0.0);
    }

    #[test]
    fn shift_restores_the_original_keyboard_speed() {
        let mut slow_input = CameraInput::default();
        slow_input.pressed.insert(KeyCode::KeyW);
        let mut slow_camera = Camera {
            position: [0.0, 0.0, 0.0],
            yaw: 0.0,
            pitch: 0.0,
            speed: 100.0,
        };
        slow_input.update_camera(&mut slow_camera, 1.0);

        let mut fast_input = CameraInput::default();
        fast_input.pressed.insert(KeyCode::KeyW);
        fast_input.pressed.insert(KeyCode::ShiftLeft);
        let mut fast_camera = Camera {
            position: [0.0, 0.0, 0.0],
            yaw: 0.0,
            pitch: 0.0,
            speed: 100.0,
        };
        fast_input.update_camera(&mut fast_camera, 1.0);

        assert_eq!(slow_camera.position[0], 8.0);
        assert_eq!(fast_camera.position[0], 100.0);
    }

    #[test]
    fn camera_location_reverses_the_preview_transform() {
        let bounds = Box3::new(
            Vec3::new(100.0, -30.0, 200.0),
            Vec3::new(300.0, 70.0, 500.0),
        );

        assert_eq!(camera_location(bounds, [5.0, 6.0, 7.0]), [205, 357, 26]);
    }

    #[test]
    fn raw_mouse_delta_rotates_at_the_current_sensitivity() {
        let mut camera = Camera {
            position: [0.0, 0.0, 0.0],
            yaw: 0.0,
            pitch: 0.0,
            speed: 1.0,
        };

        rotate_camera(&mut camera, 100.0, -50.0);

        assert!((camera.yaw + 0.2).abs() < f32::EPSILON);
        assert!((camera.pitch - 0.1).abs() < f32::EPSILON);
    }
    #[test]
    fn dual_mouse_drag_moves_vertically_without_rotating() {
        let input = CameraInput {
            left_pressed: true,
            right_pressed: true,
            ..Default::default()
        };
        let mut camera = Camera {
            position: [0.0, 25.0, 0.0],
            yaw: 0.4,
            pitch: -0.3,
            speed: 100.0,
        };

        input.apply_mouse_motion(&mut camera, 80.0, -50.0);

        assert_eq!(camera.position[1], 31.0);
        assert_eq!(camera.yaw, 0.4);
        assert_eq!(camera.pitch, -0.3);
    }

    #[test]
    fn right_mouse_drag_still_rotates_the_camera() {
        let input = CameraInput {
            right_pressed: true,
            ..Default::default()
        };
        let mut camera = Camera {
            position: [0.0, 25.0, 0.0],
            yaw: 0.0,
            pitch: 0.0,
            speed: 100.0,
        };

        input.apply_mouse_motion(&mut camera, 100.0, -50.0);

        assert_eq!(camera.position[1], 25.0);
        assert!((camera.yaw + 0.2).abs() < f32::EPSILON);
        assert!((camera.pitch - 0.1).abs() < f32::EPSILON);
    }
}
