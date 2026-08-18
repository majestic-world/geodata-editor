//! Native editor renderer for L2J collision context and editable cells.

use std::{
    cmp::Reverse,
    collections::{BinaryHeap, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
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
    editor::{self, EditorMemory, EditorOptions},
    error::{AppError, Result},
    geometry::{Box3, Triangle, Vec3},
    l2j::{self, Direction, Document, EditableBlockType, Layer, LayerAddress, NULL_HEIGHT},
    unreal::{PackageLoader, SourceMap},
};

/// Opens the standalone editor over the map's collision context.
pub fn run_editor(options: EditorOptions) -> Result<()> {
    let memory = editor::load_memory();
    let (source_map, package_count) = editor_source_map(&options)?;
    let (document, loaded) = match &options.input {
        Some(path) => (Document::open(path)?, true),
        None => (Document::blank(), false),
    };
    let event_loop = EventLoop::new()
        .map_err(|error| AppError::InvalidData(format!("can't start editor window: {error}")))?;
    let mut editor = pollster::block_on(EditorView::new(
        &event_loop,
        source_map,
        document,
        loaded && options.client_root.is_some(),
        package_count,
        options,
        memory,
    ))?;
    println!("Opening GeodataEditor. Close the window to return to the terminal.");
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
                                Err(wgpu::SurfaceError::Timeout) => {
                                    eprintln!("warning: editor frame timed out")
                                }
                            }
                        }
                        event @ WindowEvent::MouseInput {
                            button: MouseButton::Right,
                            ..
                        } => editor.input(event, false),
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

fn editor_source_map(options: &EditorOptions) -> Result<(SourceMap, usize)> {
    if let (Some(root), Some(map)) = (&options.client_root, &options.map) {
        println!("Loading map context: {map}");
        let loader = PackageLoader::new(root.clone(), 0, false);
        let source = loader.load_map(map)?;
        let count = loader.loaded_package_count();
        println!("{count} packages loaded for context.");
        return Ok((source, count));
    }
    let name = options
        .input
        .as_ref()
        .and_then(|path| path.file_stem())
        .and_then(|name| name.to_str())
        .unwrap_or("Selecione cliente, mapa e geodata")
        .to_owned();
    Ok((
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
        },
        0,
    ))
}

const WINDOW_TITLE: &str = "Geodata Editor";
const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth24Plus;
const MOUSE_LOOK_SENSITIVITY: f32 = 0.002;
// Keyboard navigation is intentionally precise; hold Shift to return to the
// original full traversal speed for moving across an entire map.
const NORMAL_MOVE_SPEED: f32 = 0.08;

struct Preview {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    size: PhysicalSize<u32>,
    depth_view: wgpu::TextureView,
    triangle_pipeline: wgpu::RenderPipeline,
    triangle_no_cull_pipeline: wgpu::RenderPipeline,
    line_pipeline: wgpu::RenderPipeline,
    geodata_line_pipeline: wgpu::RenderPipeline,
    geodata_overlay_pipeline: wgpu::RenderPipeline,
    nswe_icon_pipeline: wgpu::RenderPipeline,
    nswe_icon_bind_group: wgpu::BindGroup,
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
    async fn new(event_loop: &EventLoop<()>, source_map: SourceMap) -> Result<Self> {
        let window = Arc::new(
            WindowBuilder::new()
                .with_title(WINDOW_TITLE)
                .with_inner_size(PhysicalSize::new(1440, 1000))
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
        let camera_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
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
        });
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
        let depth_view = create_depth_view(&device, &config);
        let collision_meshes = CollisionMeshes::new(&device, &source_map, origin);

        let egui_context = egui::Context::default();
        let mut visuals = egui::Visuals::dark();
        visuals.selection.bg_fill = egui::Color32::from_rgb(0, 166, 181);
        visuals.widgets.active.bg_fill = egui::Color32::from_rgb(0, 137, 151);
        egui_context.set_visuals(visuals);
        let egui_state = egui_winit::State::new(
            egui_context.clone(),
            egui::ViewportId::ROOT,
            window.as_ref(),
            Some(window.scale_factor() as f32),
            None,
        );
        let egui_renderer = egui_wgpu::Renderer::new(&device, format, Some(DEPTH_FORMAT), 1);

        Ok(Self {
            window,
            surface,
            device,
            queue,
            config,
            size,
            depth_view,
            triangle_pipeline,
            triangle_no_cull_pipeline,
            line_pipeline,
            geodata_line_pipeline,
            geodata_overlay_pipeline,
            nswe_icon_pipeline,
            nswe_icon_bind_group,
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

struct EditorView {
    preview: Preview,
    document: Document,
    loaded: bool,
    has_context: bool,
    package_count: usize,
    max_layer_count: usize,
    ui: EditorUi,
    geodata_mesh: GeodataInstances,
    nswe_icon_mesh: NsweIconInstances,
    selection_mesh: GeodataInstances,
    /// One icon for every NSWE mask. They are embedded in the executable, so
    /// the distributed editor keeps working without an adjacent data folder.
    nswe_icons: [egui::TextureHandle; 16],
}

#[derive(Default)]
struct EditorUi {
    open_path: String,
    client_root: String,
    map_name: String,
    selected: LayerAddress,
    selection: Vec<LayerAddress>,
    rectangle_start: Option<LayerAddress>,
    line_start: Option<LayerAddress>,
    visible_layer: usize,
    brush_radius: usize,
    visual_stride: usize,
    show_nswe_icons: bool,
    show_selected_layer_only: bool,
    show_all_open_cells: bool,
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
    show_selected_layer_only: bool,
}

impl EditorOverlayOptions {
    fn from_ui(ui: &EditorUi) -> Self {
        Self {
            selected: ui.selected,
            open_context_radius: ui.open_context_radius,
            show_all_open_cells: ui.show_all_open_cells,
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

    fn shows_layer(self, layer: usize) -> bool {
        !self.show_selected_layer_only || layer == self.selected.layer
    }
}

fn editor_active_selection(ui: &EditorUi) -> Vec<LayerAddress> {
    if ui.selection.is_empty() {
        vec![ui.selected]
    } else {
        ui.selection.clone()
    }
}

fn load_nswe_icons(context: &egui::Context) -> [egui::TextureHandle; 16] {
    std::array::from_fn(|mask| {
        let pixels = decode_nswe_icon(nswe_icon_bytes(mask as u8));
        context.load_texture(
            format!("geodata-editor-nswe-{mask}"),
            pixels,
            egui::TextureOptions::NEAREST,
        )
    })
}

fn decode_nswe_icon(bytes: &[u8]) -> egui::ColorImage {
    let (size, rgba) = decode_nswe_icon_rgba(bytes);
    egui::ColorImage::from_rgba_unmultiplied(size, &rgba)
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
        let preview = Preview::new(event_loop, source_map).await?;
        preview.window.set_title("Geodata Editor By Mk - v1.1");
        configure_editor_theme(&preview.egui_context);
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
            map_name: options.map.clone().unwrap_or(memory.map_name),
            brush_radius: 1,
            visual_stride: 2,
            show_nswe_icons: true,
            open_context_radius: 16,
            ..Default::default()
        };
        if loaded {
            ui.status = "Projeto carregado com o contexto de colisão do cliente.".into();
        } else {
            ui.status = "Informe cliente, mapa e geodata para carregar o projeto.".into();
        }
        let max_layer_count = document.max_layer_count().max(1);
        let origin = map_origin(preview.source_map.bounds);
        let geodata_mesh = GeodataInstances::new(
            &preview.device,
            &editor_geodata_instances(
                &preview.source_map,
                &document,
                origin,
                ui.visual_stride,
                EditorOverlayOptions::from_ui(&ui),
            ),
        );
        let nswe_icon_mesh = NsweIconInstances::new(
            &preview.device,
            &editor_nswe_icon_instances(
                &preview.source_map,
                &document,
                origin,
                ui.visual_stride,
                EditorOverlayOptions::from_ui(&ui),
            ),
        );
        let selection_mesh = GeodataInstances::new(
            &preview.device,
            &editor_selection_instances(
                &preview.source_map,
                &document,
                origin,
                &editor_active_selection(&ui),
            ),
        );
        let nswe_icons = load_nswe_icons(&preview.egui_context);
        let view = Self {
            preview,
            document,
            loaded,
            has_context: options.client_root.is_some(),
            package_count,
            max_layer_count,
            ui,
            geodata_mesh,
            nswe_icon_mesh,
            selection_mesh,
            nswe_icons,
        };
        if view.loaded && view.has_context {
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
        if canvas_input && self.loaded && self.has_context {
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
                        if let Some(cell) = self.pick() {
                            self.select_line_endpoint(cell);
                        } else {
                            self.ui.status = "Nenhuma célula L2J foi atingida para a linha.".into();
                        }
                    }
                    ElementState::Pressed if shift => {
                        self.ui.line_start = None;
                        self.ui.rectangle_start = self.pick();
                    }
                    ElementState::Pressed => {
                        if let Some(cell) = self.pick() {
                            self.ui.line_start = None;
                            self.ui.selected = cell;
                            self.ui.visible_layer = cell.layer;
                            self.ui.selection.clear();
                            self.refresh_editor_meshes();
                            self.ui.status = format!(
                                "Célula selecionada: Geo {},{} | camada {}.",
                                cell.x, cell.y, cell.layer
                            );
                        } else {
                            self.ui.status = "Nenhuma célula L2J foi atingida. Aponte a câmera para a superfície e tente novamente.".into();
                        }
                    }
                    ElementState::Released => {
                        if let (Some(start), Some(end)) =
                            (self.ui.rectangle_start.take(), self.pick())
                        {
                            if start == end {
                                self.toggle_selection(end);
                            } else {
                                self.select_rectangle(start, end);
                            }
                        }
                    }
                }
            }
        }
        self.preview.camera_input(&event);
    }

    fn render(&mut self) -> std::result::Result<(), wgpu::SurfaceError> {
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
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("editor-render-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
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
            if self.has_context {
                self.preview.draw_collision_meshes(&mut pass, false);
            }
            if self.loaded && self.has_context {
                draw_geodata(
                    &mut pass,
                    &self.preview.geodata_overlay_pipeline,
                    &self.geodata_mesh,
                    &self.preview.camera_bind_group,
                    false,
                );
                if self.ui.show_nswe_icons {
                    draw_nswe_icons(
                        &mut pass,
                        &self.preview.nswe_icon_pipeline,
                        &self.nswe_icon_mesh,
                        &self.preview.camera_bind_group,
                        &self.preview.nswe_icon_bind_group,
                    );
                }
                draw_geodata(
                    &mut pass,
                    &self.preview.geodata_overlay_pipeline,
                    &self.selection_mesh,
                    &self.preview.camera_bind_group,
                    false,
                );
            }
            if self.preview.ui.wireframe {
                if self.has_context {
                    self.preview.draw_collision_meshes(&mut pass, true);
                }
                if self.loaded && self.has_context {
                    draw_geodata(
                        &mut pass,
                        &self.preview.geodata_line_pipeline,
                        &self.geodata_mesh,
                        &self.preview.camera_bind_group,
                        true,
                    );
                }
            }
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
        egui::TopBottomPanel::top("editor_toolbar")
            .exact_height(38.0)
            .show(context, |ui| self.draw_editor_toolbar(ui, &mut action));
        egui::TopBottomPanel::bottom("editor_status")
            .exact_height(82.0)
            .show(context, |ui| self.draw_editor_status(ui));
        egui::SidePanel::right("editor_inspector")
            .default_width(348.0)
            .min_width(300.0)
            .max_width(480.0)
            .resizable(true)
            .show(context, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        self.draw_editor_inspector(ui, &mut action, &mut visual_changed);
                    });
            });
        self.apply(action);
        if visual_changed && self.loaded && self.has_context {
            self.refresh_editor_meshes();
        }
    }

    fn draw_editor_toolbar(&mut self, ui: &mut egui::Ui, action: &mut EditorAction) {
        const ACCENT: egui::Color32 = egui::Color32::from_rgb(42, 202, 219);
        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing = egui::vec2(7.0, 0.0);
            ui.add_sized(
                [138.0, 24.0],
                egui::Label::new(egui::RichText::new("GEODATA EDITOR").strong().color(ACCENT)),
            );
            ui.separator();
            toolbar_section_label(ui, "PROJETO", ACCENT);
            if ui.button("Abrir projeto").clicked() {
                *action = EditorAction::OpenProject;
            }
            if ui
                .add_enabled(self.loaded, egui::Button::new("Salvar"))
                .clicked()
            {
                *action = EditorAction::Save;
            }
            ui.separator();
            toolbar_section_label(ui, "EDIÇÃO", ACCENT);
            if ui
                .add_enabled(self.loaded, egui::Button::new("Desfazer"))
                .clicked()
            {
                *action = EditorAction::Undo;
            }
            if ui
                .add_enabled(self.loaded, egui::Button::new("Refazer"))
                .clicked()
            {
                *action = EditorAction::Redo;
            }
            ui.separator();
            toolbar_section_label(ui, "VISUALIZAÇÃO", ACCENT);
            ui.checkbox(&mut self.preview.ui.wireframe, "Wireframe");
            ui.checkbox(&mut self.preview.ui.culling, "Culling");
            ui.checkbox(&mut self.ui.show_nswe_icons, "NSWE");
        });
    }

    fn draw_editor_inspector(
        &mut self,
        ui: &mut egui::Ui,
        action: &mut EditorAction,
        visual_changed: &mut bool,
    ) {
        const ACCENT: egui::Color32 = egui::Color32::from_rgb(42, 202, 219);
        ui.add_space(4.0);
        ui.heading("Inspetor");
        ui.label(
            egui::RichText::new(if self.loaded && self.has_context {
                "Projeto aberto"
            } else {
                "Aguardando projeto"
            })
            .small()
            .color(if self.loaded && self.has_context {
                egui::Color32::from_rgb(112, 204, 145)
            } else {
                egui::Color32::from_rgb(180, 190, 200)
            }),
        );
        ui.separator();

        egui::CollapsingHeader::new(egui::RichText::new("Projeto").strong().color(ACCENT))
            .default_open(!self.loaded)
            .show(ui, |ui| self.draw_project_section(ui, action));
        egui::CollapsingHeader::new(egui::RichText::new("Seleção").strong().color(ACCENT))
            .default_open(true)
            .show(ui, |ui| self.draw_selection_section(ui, visual_changed));
        egui::CollapsingHeader::new(egui::RichText::new("Passabilidade").strong().color(ACCENT))
            .default_open(true)
            .show(ui, |ui| {
                self.draw_passability_section(ui, action, visual_changed)
            });
        egui::CollapsingHeader::new(egui::RichText::new("Bloco").strong().color(ACCENT))
            .default_open(false)
            .show(ui, |ui| self.draw_block_section(ui, action));
        egui::CollapsingHeader::new(egui::RichText::new("Visualização").strong().color(ACCENT))
            .default_open(false)
            .show(ui, |ui| self.draw_visualization_section(ui, visual_changed));
    }

    fn draw_project_section(&mut self, ui: &mut egui::Ui, action: &mut EditorAction) {
        ui.horizontal(|ui| {
            ui.label("Cliente");
            if ui.button("Escolher pasta...").clicked() {
                if let Some(path) = pick_client_directory(&self.ui.client_root) {
                    self.ui.client_root = path.display().to_string();
                    self.ui.status = "Cliente selecionado. Escolha o mapa e a geodata.".into();
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
            ui.label("Mapa");
            let choose_map = ui
                .add_enabled(
                    !self.ui.client_root.trim().is_empty(),
                    egui::Button::new("Escolher mapa..."),
                )
                .clicked();
            if choose_map {
                if let Some(path) = pick_map_file(&self.ui.client_root) {
                    match path.file_stem().and_then(|name| name.to_str()) {
                        Some(name) => {
                            self.ui.map_name = name.to_owned();
                            self.ui.status = format!("Mapa selecionado: {name}.");
                        }
                        None => self.ui.status = "O nome do arquivo de mapa não é válido.".into(),
                    }
                }
            }
        });
        selected_path_label(
            ui,
            &self.ui.map_name,
            "Selecione a pasta do cliente primeiro.",
        );
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("Geodata");
            if ui.button("Escolher geodata...").clicked() {
                if let Some(path) = pick_geodata_file(&self.ui.open_path) {
                    self.ui.open_path = path.display().to_string();
                    self.ui.status = "Geodata selecionada. Abra o projeto quando terminar.".into();
                }
            }
        });
        selected_path_label(ui, &self.ui.open_path, "Nenhuma geodata selecionada.");
        ui.add_space(6.0);
        if ui.button("Carregar projeto").clicked() {
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
                        .checkbox(
                            &mut self.ui.show_selected_layer_only,
                            "Exibir somente selecionada",
                        )
                        .changed();
                });
                *visual_changed |= self.ui.visible_layer != previous_layer;
            });
        self.ui.selected.layer = self.ui.visible_layer;
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("Pincel");
            for radius in [1, 2, 4, 8] {
                ui.selectable_value(&mut self.ui.brush_radius, radius, radius.to_string());
            }
        });
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
        let selection_count = self.ui.selection.len().max(1);
        ui.label(
            egui::RichText::new(format!("{} célula(s) ativa(s)", selection_count))
                .color(egui::Color32::from_rgb(42, 202, 219)),
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
            ui.label(egui::RichText::new("Layers da célula").strong());
            let mut requested_layer = None;
            egui::Grid::new("editor_cell_layers")
                .num_columns(2)
                .spacing([16.0, 3.0])
                .show(ui, |ui| {
                    ui.small("Layer");
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
                egui::Color32::from_rgb(255, 145, 120),
                "A camada visível não existe nesta coluna.",
            );
            return;
        };
        let bx = self.ui.selected.x / 8;
        let by = self.ui.selected.y / 8;
        ui.horizontal(|ui| {
            let mask = (layer.nswe & 0x0f) as usize;
            let icon = &self.nswe_icons[mask];
            let response = egui::Frame::none()
                .fill(egui::Color32::from_rgb(232, 236, 240))
                .rounding(3.0)
                .inner_margin(egui::Margin::same(2.0))
                .show(ui, |ui| ui.image((icon.id(), egui::vec2(32.0, 32.0))))
                .inner;
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
        ui.add_space(4.0);
        ui.label(format!(
            "{} blocos alterados",
            self.document.changed_blocks()
        ));
        if self.has_context {
            ui.small(format!("{} pacotes de contexto", self.package_count));
        }
    }

    fn draw_editor_status(&self, ui: &mut egui::Ui) {
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
            egui::vec2(summary_width, 24.0),
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
                ui.label(format!("SELEÇÃO: {}", self.ui.selection.len().max(1)));
                ui.separator();
                ui.label(format!("ALTERADOS: {}", self.document.changed_blocks()));
                ui.separator();
                ui.monospace(format!("CÂMERA: {camera_x}, {camera_y}, {camera_z}"));
            },
        );
        ui.separator();
        let status = if self.ui.status.is_empty() {
            "Pronto. Clique para selecionar; Shift adiciona e Ctrl segue uma faixa."
        } else {
            &self.ui.status
        };
        let status_color = editor_status_color(status);
        ui.add_sized(
            [ui.available_width(), 42.0],
            egui::Label::new(egui::RichText::new(status).color(status_color)).wrap(true),
        );
    }

    fn apply(&mut self, action: EditorAction) {
        let bx = self.ui.selected.x / 8;
        let by = self.ui.selected.y / 8;
        if !matches!(action, EditorAction::None | EditorAction::OpenProject)
            && (!self.loaded || !self.has_context)
        {
            self.ui.status = "Carregue cliente, mapa e geodata antes de editar.".into();
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
        let result = self
            .document
            .force_set_nswe(targets, mask, format!("Preset NSWE {mask:04b}"));
        if result.changed_cells == 0 && result.changed_links == 0 {
            self.ui.status = if result.rejected_links.is_empty() {
                "As células selecionadas já usavam esse preset.".into()
            } else {
                result.rejected_links.join(" | ")
            };
        } else {
            self.after_edit(&format!(
                "Preset NSWE {mask:04b} aplicado: {} células, {} ligações.",
                result.changed_cells, result.changed_links,
            ));
        }
    }

    fn apply_height(&mut self, requested_height: i32) {
        let targets = self.edit_targets();
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
        if !self.ui.selection.is_empty() {
            return self.ui.selection.clone();
        }
        let radius = self.ui.brush_radius.max(1) - 1;
        let mut result = Vec::new();
        for x in self.ui.selected.x.saturating_sub(radius)..=(self.ui.selected.x + radius).min(2047)
        {
            for y in
                self.ui.selected.y.saturating_sub(radius)..=(self.ui.selected.y + radius).min(2047)
            {
                if let Some(count) = self.document.layer_count(x, y) {
                    result.push(LayerAddress::new(
                        x,
                        y,
                        self.ui.visible_layer.min(count.saturating_sub(1)),
                    ));
                }
            }
        }
        result
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

    fn open_project(&mut self) {
        let client_text = self.ui.client_root.trim();
        if client_text.is_empty() {
            self.ui.status = "Informe a pasta raiz do cliente Lineage II.".into();
            return;
        }
        let client_root = PathBuf::from(client_text);
        if !client_root.is_dir() {
            self.ui.status = format!("Pasta de cliente inválida: {}", client_root.display());
            return;
        }
        let map = self.ui.map_name.trim();
        if map.is_empty() {
            self.ui.status = "Informe o nome do mapa, por exemplo 22_22_Classic.".into();
            return;
        }
        let path = PathBuf::from(self.ui.open_path.trim());
        if self.ui.open_path.trim().is_empty() {
            self.ui.status = "Selecione a geodata que será editada.".into();
            return;
        }
        let document = match Document::open(&path) {
            Ok(document) => document,
            Err(error) => {
                self.ui.status = format!("Falha ao abrir geodata: {error}");
                return;
            }
        };
        let loader = PackageLoader::new(client_root, 0, false);
        let source_map = match loader.load_map(map) {
            Ok(source_map) => source_map,
            Err(error) => {
                self.ui.status = format!("Falha ao carregar o mapa do cliente: {error}");
                return;
            }
        };
        self.package_count = loader.loaded_package_count();
        self.document = document;
        self.max_layer_count = self.document.max_layer_count().max(1);
        self.ui.visible_layer = self
            .ui
            .visible_layer
            .min(self.max_layer_count.saturating_sub(1));
        self.ui.selected.layer = self.ui.visible_layer;
        self.preview.source_map = source_map;
        self.preview.collision_meshes = CollisionMeshes::new(
            &self.preview.device,
            &self.preview.source_map,
            map_origin(self.preview.source_map.bounds),
        );
        self.preview.camera.reset(self.preview.source_map.bounds);
        self.loaded = true;
        self.has_context = true;

        self.ui.selection.clear();
        self.ui.height_input_address = None;
        self.ui.status = format!(
            "Projeto carregado: {} com {} pacotes de contexto.",
            self.preview.source_map.name, self.package_count
        );
        self.refresh_geodata();
        if let Err(error) = self.persist_memory() {
            self.ui
                .status
                .push_str(&format!(" Aviso: memória não salva: {error}"));
        }
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
            map_name: self.ui.map_name.trim().to_owned(),
        })
    }

    fn refresh_geodata(&mut self) {
        self.refresh_editor_meshes();
    }

    fn active_selection(&self) -> Vec<LayerAddress> {
        editor_active_selection(&self.ui)
    }

    fn refresh_editor_meshes(&mut self) {
        let origin = map_origin(self.preview.source_map.bounds);
        let mesh = editor_geodata_instances(
            &self.preview.source_map,
            &self.document,
            origin,
            self.ui.visual_stride,
            EditorOverlayOptions::from_ui(&self.ui),
        );
        self.geodata_mesh = GeodataInstances::new(&self.preview.device, &mesh);
        let icons = editor_nswe_icon_instances(
            &self.preview.source_map,
            &self.document,
            origin,
            self.ui.visual_stride,
            EditorOverlayOptions::from_ui(&self.ui),
        );
        self.nswe_icon_mesh = NsweIconInstances::new(&self.preview.device, &icons);
        self.refresh_selection_mesh();
    }

    fn refresh_selection_mesh(&mut self) {
        let origin = map_origin(self.preview.source_map.bounds);
        let mesh = editor_selection_instances(
            &self.preview.source_map,
            &self.document,
            origin,
            &self.active_selection(),
        );
        self.selection_mesh = GeodataInstances::new(&self.preview.device, &mesh);
    }

    fn pick(&self) -> Option<LayerAddress> {
        let position = self.ui.last_cursor?;
        let width = self.preview.config.width.max(1) as f32;
        let height = self.preview.config.height.max(1) as f32;
        let x = position.x as f32 / width * 2.0 - 1.0;
        let y = 1.0 - position.y as f32 / height * 2.0;
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
        )
    }

    fn select_rectangle(&mut self, start: LayerAddress, end: LayerAddress) {
        const MAX_SELECTION: usize = 65_536;
        let (min_x, max_x) = (start.x.min(end.x), start.x.max(end.x));
        let (min_y, max_y) = (start.y.min(end.y), start.y.max(end.y));
        let mut selection = std::mem::take(&mut self.ui.selection);
        if selection.is_empty() {
            selection.push(self.ui.selected);
        }
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
        if self.ui.selection.is_empty() {
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

fn configure_editor_theme(context: &egui::Context) {
    let mut style = (*context.style()).clone();
    style.visuals = egui::Visuals::dark();
    style.visuals.panel_fill = egui::Color32::from_rgb(21, 27, 34);
    style.visuals.window_fill = egui::Color32::from_rgb(25, 32, 40);
    style.visuals.faint_bg_color = egui::Color32::from_rgb(31, 40, 49);
    style.visuals.extreme_bg_color = egui::Color32::from_rgb(14, 19, 24);
    style.visuals.selection.bg_fill = egui::Color32::from_rgb(20, 125, 143);
    style.visuals.selection.stroke =
        egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(94, 226, 237));
    style.visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(38, 48, 58);
    style.visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(41, 92, 102);
    style.visuals.widgets.active.bg_fill = egui::Color32::from_rgb(24, 138, 153);
    style.spacing.item_spacing = egui::vec2(7.0, 6.0);
    style.spacing.button_padding = egui::vec2(8.0, 4.0);
    context.set_style(style);
}

fn toolbar_section_label(ui: &mut egui::Ui, text: &str, accent: egui::Color32) {
    ui.allocate_ui_with_layout(
        egui::vec2(82.0, 24.0),
        egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
        |ui| {
            ui.label(egui::RichText::new(text).size(11.0).strong().color(accent));
        },
    );
}

fn editor_status_color(status: &str) -> egui::Color32 {
    if status.starts_with("Falha") || status.contains("recusada") || status.contains("inválid") {
        egui::Color32::from_rgb(255, 145, 120)
    } else if status.starts_with("Salvo")
        || status.starts_with("Projeto carregado")
        || status.starts_with("NSWE alterado")
        || status.starts_with("NSWE liberado")
    {
        egui::Color32::from_rgb(112, 204, 145)
    } else {
        egui::Color32::from_rgb(190, 202, 212)
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

/// Draws a crisp, scale-independent version of the legacy NSWE icon. The PNG
/// assets remain in use on the 3D overlay, but buttons need vector strokes so
/// they stay clear on high-DPI monitors and never inherit the dark texture
/// background from the inspector theme.
fn draw_nswe_preset_button(ui: &mut egui::Ui, mask: u8, selected: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(46.0, 46.0), egui::Sense::click());
    let background = if selected {
        egui::Color32::from_rgb(33, 187, 205)
    } else if response.hovered() {
        egui::Color32::from_rgb(249, 251, 252)
    } else {
        egui::Color32::from_rgb(224, 229, 233)
    };
    let border = if selected {
        egui::Color32::from_rgb(127, 244, 255)
    } else {
        egui::Color32::from_rgb(157, 170, 178)
    };
    let painter = ui.painter();
    painter.rect_filled(rect, 4.0, background);
    painter.rect_stroke(rect, 4.0, egui::Stroke::new(1.0_f32, border));

    let center = rect.center();
    let tip = 7.5;
    let offset = 11.0;
    let arrow_stroke = egui::Stroke::new(1.35_f32, egui::Color32::from_rgb(28, 36, 42));
    let draw_arrow = |points: [egui::Pos2; 3], open: bool| {
        if open {
            // Keep only the outside sides of an open arrow. Drawing its base
            // would join the four arrows into an unwanted square at center.
            painter.line_segment([points[0], points[1]], arrow_stroke);
            painter.line_segment([points[0], points[2]], arrow_stroke);
        } else {
            painter.add(egui::Shape::convex_polygon(
                points.to_vec(),
                egui::Color32::from_rgb(28, 36, 42),
                arrow_stroke,
            ));
        }
    };
    draw_arrow(
        [
            egui::pos2(center.x, center.y - offset - tip),
            egui::pos2(center.x - tip, center.y - offset + tip),
            egui::pos2(center.x + tip, center.y - offset + tip),
        ],
        mask & Direction::North.bit() != 0,
    );
    draw_arrow(
        [
            egui::pos2(center.x, center.y + offset + tip),
            egui::pos2(center.x - tip, center.y + offset - tip),
            egui::pos2(center.x + tip, center.y + offset - tip),
        ],
        mask & Direction::South.bit() != 0,
    );
    draw_arrow(
        [
            egui::pos2(center.x - offset - tip, center.y),
            egui::pos2(center.x - offset + tip, center.y - tip),
            egui::pos2(center.x - offset + tip, center.y + tip),
        ],
        mask & Direction::West.bit() != 0,
    );
    draw_arrow(
        [
            egui::pos2(center.x + offset + tip, center.y),
            egui::pos2(center.x + offset - tip, center.y - tip),
            egui::pos2(center.x + offset - tip, center.y + tip),
        ],
        mask & Direction::East.bit() != 0,
    );
    response
}

fn selected_path_label(ui: &mut egui::Ui, value: &str, empty_message: &str) {
    if value.trim().is_empty() {
        ui.small(egui::RichText::new(empty_message).weak());
    } else {
        ui.small(value);
    }
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

fn pick_map_file(client_root: &str) -> Option<PathBuf> {
    let mut dialog = FileDialog::new()
        .set_title("Selecionar mapa Lineage II")
        .add_filter("Mapas Unreal", &["unr"]);
    let maps_directory = PathBuf::from(client_root.trim()).join("Maps");
    if maps_directory.is_dir() {
        dialog = dialog.set_directory(maps_directory);
    } else if let Some(directory) = dialog_directory(client_root) {
        dialog = dialog.set_directory(directory);
    }
    dialog.pick_file()
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

fn editor_selection_instances(
    map: &SourceMap,
    document: &Document,
    origin: Vec3,
    selection: &[LayerAddress],
) -> CpuGeodata {
    let mut mesh = CpuGeodata::default();
    for address in selection {
        let Some(cell) = document.cell(*address) else {
            continue;
        };
        if cell.height == NULL_HEIGHT {
            continue;
        }
        mesh.instances.push(GeodataInstance {
            position: [
                map.bounds.min.x + address.x as f32 * 16.0 + 7.5 - origin.x,
                // It is deliberately above both source geometry and the L2J
                // overlay: a picked cell must remain unmistakable from any
                // camera angle.
                cell.height as f32 - origin.y + 6.0,
                map.bounds.min.z + address.y as f32 * 16.0 + 7.5 - origin.z,
            ],
            scale: 7.85,
            color: [255, 235, 0, 255],
        });
    }
    mesh
}

fn editor_geodata_instances(
    map: &SourceMap,
    document: &Document,
    origin: Vec3,
    stride: usize,
    visibility: EditorOverlayOptions,
) -> CpuGeodata {
    let mut mesh = CpuGeodata::default();
    let stride = stride.max(1);
    for block_x in 0..256 {
        for block_y in 0..256 {
            let start_x = block_x * 8;
            let start_y = block_y * 8;
            match document.block_type(block_x, block_y) {
                Some(EditableBlockType::Simple) => {
                    if !visibility.shows_layer(0) || !visibility.shows_open_block(start_x, start_y)
                    {
                        continue;
                    }
                    if let Some(layer) = document.cell(LayerAddress::new(start_x, start_y, 0)) {
                        append_editor_geodata_cell(
                            &mut mesh, map, origin, start_x, start_y, layer, 63.4,
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
                                if let Some(layer) = document.cell(LayerAddress::new(x, y, layer)) {
                                    // Sampling may simplify large open areas,
                                    // but never hide a partial or blocked
                                    // location.  Those are exactly the cells
                                    // an editor user needs to find and fix.
                                    let is_open = layer.nswe & 0x0f == Layer::OPEN;
                                    if is_open
                                        && (!visibility.shows_open_cell(x, y)
                                            || x % stride != 0
                                            || y % stride != 0)
                                    {
                                        continue;
                                    }
                                    append_editor_geodata_cell(
                                        &mut mesh,
                                        map,
                                        origin,
                                        x,
                                        y,
                                        layer,
                                        if is_open { sampled_scale } else { 7.35 },
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
fn editor_nswe_icon_instances(
    map: &SourceMap,
    document: &Document,
    origin: Vec3,
    stride: usize,
    visibility: EditorOverlayOptions,
) -> CpuNsweIcons {
    let mut mesh = CpuNsweIcons::default();
    let stride = stride.max(1);
    for block_x in 0..256 {
        for block_y in 0..256 {
            let start_x = block_x * 8;
            let start_y = block_y * 8;
            match document.block_type(block_x, block_y) {
                // A simple block is semantically an 8×8 area with all four
                // directions open.  Its cyan surface is already the status
                // indicator; stretching a 32 px glyph across 128 world units
                // only produces a large, pixelated label.
                Some(EditableBlockType::Simple) => {}
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
                                let is_open = layer.nswe & 0x0f == Layer::OPEN;
                                if is_open
                                    && (!visibility.shows_open_cell(x, y)
                                        || x % stride != 0
                                        || y % stride != 0)
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
                                    if is_open { sampled_scale } else { 7.35 },
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
        // Legacy glyphs are 32×32 pixels.  Keep them near one L2J cell wide
        // instead of stretching the bitmap over a sampled group of cells.
        // This also keeps the scene readable when the user zooms in.
        scale: (cell_scale * 0.80).min(7.4),
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
                if cell.height == NULL_HEIGHT {
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
        color: editor_cell_color(cell),
    });
}

fn editor_cell_color(cell: Layer) -> [u8; 4] {
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

#[derive(Default)]
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

struct GeodataInstances {
    quad_vertices: wgpu::Buffer,
    triangles: wgpu::Buffer,
    lines: wgpu::Buffer,
    instances: wgpu::Buffer,
    count: u32,
}

impl GeodataInstances {
    fn new(device: &wgpu::Device, mesh: &CpuGeodata) -> Self {
        const QUAD_VERTICES: [QuadVertex; 4] = [
            QuadVertex {
                offset: [-1.0, -1.0],
            },
            QuadVertex {
                offset: [1.0, -1.0],
            },
            QuadVertex {
                offset: [-1.0, 1.0],
            },
            QuadVertex { offset: [1.0, 1.0] },
        ];
        const TRIANGLES: [u16; 6] = [0, 2, 1, 1, 2, 3];
        const LINES: [u16; 8] = [0, 1, 1, 3, 3, 2, 2, 0];
        let instances = if mesh.instances.is_empty() {
            vec![GeodataInstance {
                position: [0.0; 3],
                scale: 0.0,
                color: [0; 4],
            }]
        } else {
            mesh.instances.clone()
        };
        Self {
            quad_vertices: device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("geodata-quad-vertices"),
                contents: bytemuck::cast_slice(&QUAD_VERTICES),
                usage: wgpu::BufferUsages::VERTEX,
            }),
            triangles: device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("geodata-quad-triangles"),
                contents: bytemuck::cast_slice(&TRIANGLES),
                usage: wgpu::BufferUsages::INDEX,
            }),
            lines: device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("geodata-quad-lines"),
                contents: bytemuck::cast_slice(&LINES),
                usage: wgpu::BufferUsages::INDEX,
            }),
            instances: device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("geodata-instances"),
                contents: bytemuck::cast_slice(&instances),
                usage: wgpu::BufferUsages::VERTEX,
            }),
            count: mesh.instances.len() as u32,
        }
    }
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

struct NsweIconInstances {
    quad_vertices: wgpu::Buffer,
    triangles: wgpu::Buffer,
    instances: wgpu::Buffer,
    count: u32,
}

impl NsweIconInstances {
    fn new(device: &wgpu::Device, mesh: &CpuNsweIcons) -> Self {
        const QUAD_VERTICES: [QuadVertex; 4] = [
            QuadVertex {
                offset: [-1.0, -1.0],
            },
            QuadVertex {
                offset: [1.0, -1.0],
            },
            QuadVertex {
                offset: [-1.0, 1.0],
            },
            QuadVertex { offset: [1.0, 1.0] },
        ];
        const TRIANGLES: [u16; 6] = [0, 2, 1, 1, 2, 3];
        let instances = if mesh.instances.is_empty() {
            vec![NsweIconInstance {
                position: [0.0; 3],
                scale: 0.0,
                mask: 0.0,
            }]
        } else {
            mesh.instances.clone()
        };
        Self {
            quad_vertices: device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("editor-nswe-icon-quad-vertices"),
                contents: bytemuck::cast_slice(&QUAD_VERTICES),
                usage: wgpu::BufferUsages::VERTEX,
            }),
            triangles: device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("editor-nswe-icon-quad-triangles"),
                contents: bytemuck::cast_slice(&TRIANGLES),
                usage: wgpu::BufferUsages::INDEX,
            }),
            instances: device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("editor-nswe-icon-instances"),
                contents: bytemuck::cast_slice(&instances),
                usage: wgpu::BufferUsages::VERTEX,
            }),
            count: mesh.instances.len() as u32,
        }
    }
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

fn draw_geodata<'a>(
    pass: &mut wgpu::RenderPass<'a>,
    pipeline: &'a wgpu::RenderPipeline,
    mesh: &'a GeodataInstances,
    camera: &'a wgpu::BindGroup,
    lines: bool,
) {
    if mesh.count == 0 {
        return;
    }
    pass.set_pipeline(pipeline);
    pass.set_bind_group(0, camera, &[]);
    pass.set_vertex_buffer(0, mesh.quad_vertices.slice(..));
    pass.set_vertex_buffer(1, mesh.instances.slice(..));
    pass.set_index_buffer(
        if lines {
            mesh.lines.slice(..)
        } else {
            mesh.triangles.slice(..)
        },
        wgpu::IndexFormat::Uint16,
    );
    pass.draw_indexed(if lines { 0..8 } else { 0..6 }, 0, 0..mesh.count);
}

fn draw_nswe_icons<'a>(
    pass: &mut wgpu::RenderPass<'a>,
    pipeline: &'a wgpu::RenderPipeline,
    mesh: &'a NsweIconInstances,
    camera: &'a wgpu::BindGroup,
    atlas: &'a wgpu::BindGroup,
) {
    if mesh.count == 0 {
        return;
    }
    pass.set_pipeline(pipeline);
    pass.set_bind_group(0, camera, &[]);
    pass.set_bind_group(1, atlas, &[]);
    pass.set_vertex_buffer(0, mesh.quad_vertices.slice(..));
    pass.set_vertex_buffer(1, mesh.instances.slice(..));
    pass.set_index_buffer(mesh.triangles.slice(..), wgpu::IndexFormat::Uint16);
    pass.draw_indexed(0..6, 0, 0..mesh.count);
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
                if *state == ElementState::Pressed {
                    self.rotating = true;
                    self.raw_mouse = capture_cursor(window);
                } else {
                    self.stop_rotating(window);
                }
                self.cursor = None;
            }
            WindowEvent::Focused(false) => self.stop_rotating(window),
            WindowEvent::CursorMoved { position, .. } if self.rotating && !self.raw_mouse => {
                if let Some(previous) = self.cursor.replace(*position) {
                    rotate_camera(camera, position.x - previous.x, position.y - previous.y);
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
            rotate_camera(camera, delta.0, delta.1);
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
            multisample: wgpu::MultisampleState::default(),
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
            multisample: wgpu::MultisampleState::default(),
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

/// Editor-only L2J overlay. It writes depth so the editor shows the first
/// surface facing the camera instead of blending every floor and ceiling in a
/// multilayer area into the same view.
fn create_geodata_overlay_pipeline(
    device: &wgpu::Device,
    camera_layout: &wgpu::BindGroupLayout,
    format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("geodata-editor-overlay-shader"),
        source: wgpu::ShaderSource::Wgsl(GEODATA_SHADER.into()),
    });
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("geodata-editor-overlay-layout"),
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
        multisample: wgpu::MultisampleState::default(),
        multiview: None,
    })
}

fn create_nswe_icon_resources(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    camera_layout: &wgpu::BindGroupLayout,
    format: wgpu::TextureFormat,
) -> (wgpu::RenderPipeline, wgpu::BindGroup) {
    const ICON_SIDE: u32 = 32;
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
        multisample: wgpu::MultisampleState::default(),
        multiview: None,
    });
    (pipeline, bind_group)
}

fn nswe_icon_atlas() -> Vec<u8> {
    const ICON_SIDE: usize = 32;
    const ATLAS_SIDE: usize = ICON_SIDE * 4;
    let mut atlas = vec![0; ATLAS_SIDE * ATLAS_SIDE * 4];
    for mask in 0..16_usize {
        let (size, icon) = decode_nswe_icon_rgba(nswe_icon_bytes(mask as u8));
        assert_eq!(size, [ICON_SIDE, ICON_SIDE], "NSWE icon must be 32×32");
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
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: DEPTH_FORMAT,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_cells_and_selection_use_high_visibility_colours() {
        assert_eq!(
            editor_cell_color(Layer {
                height: 0,
                nswe: 15
            }),
            [0, 205, 230, 165]
        );
        assert_eq!(
            editor_cell_color(Layer { height: 0, nswe: 0 }),
            [235, 45, 45, 230]
        );
        assert!(
            editor_cell_color(Layer {
                height: 0,
                nswe: 15
            })[3]
                > 100
        );
    }

    #[test]
    fn nswe_icon_atlas_contains_all_sixteen_status_glyphs() {
        let atlas = nswe_icon_atlas();
        assert_eq!(atlas.len(), 128 * 128 * 4);
        for mask in 0..16_usize {
            let origin_x = (mask % 4) * 32;
            let origin_y = (mask / 4) * 32;
            let contains_opaque_pixel = (0..32).any(|row| {
                (0..32)
                    .any(|column| atlas[((origin_y + row) * 128 + origin_x + column) * 4 + 3] > 0)
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
            pick_l2j_ray(&document, bounds, [8.0, 200.0, 8.0], [1.0, -1.0, 0.0], None,),
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
            pick_l2j_ray(&document, bounds, [8.0, 200.0, 8.0], [0.0, -1.0, 0.0], None,),
            Some(LayerAddress::new(0, 0, 1))
        );
        assert_eq!(
            pick_l2j_ray(
                &document,
                bounds,
                [8.0, 200.0, 8.0],
                [0.0, -1.0, 0.0],
                Some(0),
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
}
