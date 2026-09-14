//! Background project decoding and GPU preparation; only completed resources reach the UI.

use std::{
    cell::Cell,
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc,
        mpsc::{self, Receiver, TryRecvError},
    },
    thread,
};

use wgpu::util::DeviceExt;

use super::{
    CollisionMeshes, EditorUi, EditorView, FALLBACK_MATERIAL_RGBA, PendingFlavour, TexturedBatch,
    TexturedPipelineKey, TexturedPipelineResources, TexturedScene, create_material_bind_group,
    create_material_sampler, create_material_texture, create_textured_pipeline, map_origin,
    map_package_or_prompt, overlays::OverlayMeshes, textured_batch_vertices,
};
use crate::{
    editor::{self, EditorMemory, MapType},
    geometry::Vec3,
    l2j::Document,
    unreal::{PackageLoader, SourceMap, VisualBlend, VisualScene, VisualTexture},
};

/// One receiver owns the admission slot until its result is consumed. Repeated
/// clicks cannot create additional workers, even after a worker has finished.
struct Job<T> {
    receiver: Receiver<T>,
}

impl<T: Send + 'static> Job<T> {
    fn spawn(work: impl FnOnce() -> T + Send + 'static) -> Result<Self, String> {
        let (sender, receiver) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name("editor-project-loader".into())
            .spawn(move || {
                let result = work();
                // A closed window simply drops the prepared result here.
                let _ = sender.send(result);
            })
            .map_err(|error| format!("Falha ao iniciar carregamento: {error}"))?;
        Ok(Self { receiver })
    }

    fn poll(&self) -> Option<Result<T, String>> {
        match self.receiver.try_recv() {
            Ok(result) => Some(Ok(result)),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(
                "O carregamento foi interrompido antes de concluir.".into(),
            )),
        }
    }
}

#[derive(Clone)]
struct ProjectSettings {
    client_root: PathBuf,
    path: PathBuf,
    region: String,
    map_type: MapType,
    ui: EditorUi,
}

/// Identity of the installed project, not of the editable welcome form.
struct ActiveProject {
    client_root: PathBuf,
    package: String,
    origin: Vec3,
}

struct GpuContext {
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    material_layout: Arc<wgpu::BindGroupLayout>,
    pipelines: Arc<TexturedPipelineResources>,
}

struct PreparedProject {
    document: Document,
    source_map: SourceMap,
    collision_meshes: CollisionMeshes,
    overlays: OverlayMeshes,
    textured_scene: Option<TexturedScene>,
    active: ActiveProject,
    max_layer_count: usize,
    visible_layer: usize,
    package_count: usize,
    visual_failed: bool,
    status: String,
}

enum ProjectOutcome {
    Ready(Box<PreparedProject>),
    Prompt(ProjectSettings, PendingFlavour),
    Error(String),
}

struct PreparedVisual {
    scene: TexturedScene,
    batch_count: usize,
}

enum ActiveJob {
    Project(Job<ProjectOutcome>),
    Visual(Job<Result<PreparedVisual, String>>),
}

enum Completion {
    Project(ProjectOutcome),
    Visual(Result<PreparedVisual, String>),
    Disconnected(String),
}

#[derive(Default)]
pub(super) struct LoadingState {
    job: Option<ActiveJob>,
    active: Option<ActiveProject>,
}

impl LoadingState {
    pub(super) fn is_busy(&self) -> bool {
        self.job.is_some()
    }

    pub(super) fn is_project_loading(&self) -> bool {
        matches!(self.job, Some(ActiveJob::Project(_)))
    }

    fn poll(&mut self) -> Option<Completion> {
        let result = match self.job.as_ref()? {
            ActiveJob::Project(job) => job.poll()?.map(Completion::Project),
            ActiveJob::Visual(job) => job.poll()?.map(Completion::Visual),
        };
        self.job = None;
        Some(result.unwrap_or_else(Completion::Disconnected))
    }
}

impl EditorView {
    pub(super) fn open_project(&mut self) {
        if self.loading.is_busy() {
            return;
        }
        let client_text = self.ui.client_root.trim();
        if client_text.is_empty() {
            self.ui.status = "Informe a pasta raiz do cliente Lineage II.".into();
            return;
        }
        let geodata_text = self.ui.open_path.trim();
        if geodata_text.is_empty() {
            self.ui.status = "Selecione a geodata que será editada.".into();
            return;
        }
        let path = PathBuf::from(geodata_text);
        let Some(region) = editor::geodata_region(&path) else {
            self.ui.status = format!("Nome de geodata inválido: {}", path.display());
            return;
        };
        let settings = ProjectSettings {
            client_root: PathBuf::from(client_text),
            path,
            region,
            map_type: self.ui.map_type,
            ui: self.ui.clone(),
        };
        let gpu = self.loading_gpu_context();
        match Job::spawn(move || prepare_project(settings, gpu)) {
            Ok(job) => {
                self.loading.job = Some(ActiveJob::Project(job));
                self.ui.pending_flavour = None;
                self.ui.status = "Carregando projeto em segundo plano…".into();
            }
            Err(error) => self.ui.status = error,
        }
    }

    pub(super) fn enable_textured_view(&mut self) {
        if self.loading.is_busy() || self.textured_scene.is_some() {
            return;
        }
        if !self.loaded || !self.has_context {
            self.ui.status =
                "Carregue um projeto antes de ativar a visualização texturizada.".into();
            self.ui.textured_view = false;
            return;
        }
        let Some(active) = self.loading.active.as_ref() else {
            self.ui.status = "Abra novamente o projeto para carregar as texturas.".into();
            self.ui.textured_view = false;
            return;
        };
        let client_root = active.client_root.clone();
        let package = active.package.clone();
        let origin = active.origin;
        let gpu = self.loading_gpu_context();
        match Job::spawn(move || {
            // PackageLoader and all its Rc-backed textures stay on this thread.
            let loader = PackageLoader::new(client_root, 0, false);
            prepare_visual(&loader, &package, origin, &gpu)
        }) {
            Ok(job) => {
                self.loading.job = Some(ActiveJob::Visual(job));
                self.ui.status = "Carregando visualização texturizada em segundo plano…".into();
            }
            Err(error) => {
                self.ui.textured_view = false;
                self.ui.status = error;
            }
        }
    }

    pub(super) fn poll_loading(&mut self) {
        let Some(completion) = self.loading.poll() else {
            return;
        };
        match completion {
            Completion::Project(ProjectOutcome::Ready(project)) => {
                let project = *project;
                self.document = project.document;
                self.preview.source_map = project.source_map;
                self.preview.collision_meshes = project.collision_meshes;
                self.overlays = project.overlays;
                self.textured_scene = project.textured_scene;
                self.loading.active = Some(project.active);
                self.package_count = project.package_count;
                self.max_layer_count = project.max_layer_count;
                self.ui.visible_layer = project.visible_layer;
                self.ui.selected.layer = project.visible_layer;
                self.ui.selection.clear();
                self.ui.selection_hidden = false;
                self.ui.rectangle_start = None;
                self.ui.line_start = None;
                self.ui.pending_plain_selection = false;
                self.ui.height_input_address = None;
                self.ui.pending_flavour = None;
                self.loaded = true;
                self.has_context = true;
                self.preview.camera.reset(self.preview.source_map.bounds);
                self.ui.status = project.status;
                if project.visual_failed {
                    self.ui.textured_view = false;
                }
                // Initial overlays are already GPU-ready. Only changes made to
                // display filters during loading need reconciliation here.
                self.refresh_editor_meshes();
                if self.ui.textured_view && self.textured_scene.is_none() {
                    self.enable_textured_view();
                }
            }
            Completion::Project(ProjectOutcome::Prompt(settings, pending)) => {
                // The existing confirmation changes map_type and reopens. Keep
                // its form tied to the request that produced this question.
                self.ui.client_root = settings.client_root.to_string_lossy().into_owned();
                self.ui.open_path = settings.path.to_string_lossy().into_owned();
                self.ui.map_type = settings.map_type;
                self.ui.status = pending.question();
                self.ui.pending_flavour = Some(pending);
            }
            Completion::Project(ProjectOutcome::Error(error)) => self.ui.status = error,
            Completion::Visual(Ok(visual)) => {
                self.textured_scene = Some(visual.scene);
                // Do not turn the toggle back on if it was disabled while the
                // job ran: the result remains cached for the next activation.
                self.ui.status = format!(
                    "Visualização texturizada carregada: {} lote(s).",
                    visual.batch_count
                );
            }
            Completion::Visual(Err(error)) => {
                self.ui.textured_view = false;
                self.ui.status = error;
            }
            Completion::Disconnected(error) => {
                if self.textured_scene.is_none() {
                    self.ui.textured_view = false;
                }
                self.ui.status = error;
            }
        }
    }

    fn loading_gpu_context(&self) -> GpuContext {
        GpuContext {
            device: Arc::clone(&self.preview.device),
            queue: Arc::clone(&self.preview.queue),
            material_layout: Arc::clone(&self.preview.material_texture_layout),
            pipelines: Arc::clone(&self.preview.textured_pipelines),
        }
    }
}

fn prepare_project(mut settings: ProjectSettings, gpu: GpuContext) -> ProjectOutcome {
    if !settings.client_root.is_dir() {
        return ProjectOutcome::Error(format!(
            "Pasta de cliente inválida: {}",
            settings.client_root.display()
        ));
    }
    let mut document = match Document::open(&settings.path) {
        Ok(document) => document,
        Err(error) => return ProjectOutcome::Error(format!("Falha ao abrir geodata: {error}")),
    };
    let loader = PackageLoader::new(settings.client_root.clone(), 0, false);
    let package = match map_package_or_prompt(&loader, &settings.region, settings.map_type) {
        Ok(package) => package,
        Err(Some(pending)) => return ProjectOutcome::Prompt(settings, pending),
        Err(None) => {
            return ProjectOutcome::Error(format!(
                "O mapa {} não existe no cliente informado.",
                settings.map_type.package_name(&settings.region)
            ));
        }
    };
    let source_map = match loader.load_map(&package) {
        Ok(source_map) => source_map,
        Err(error) => {
            return ProjectOutcome::Error(format!("Falha ao carregar Maps/{package}.unr: {error}"));
        }
    };
    let package_count = loader.loaded_package_count();
    let origin = map_origin(source_map.bounds);
    let collision_meshes = CollisionMeshes::new(&gpu.device, &source_map, origin);
    let max_layer_count = document.max_layer_count().max(1);
    settings.ui.visible_layer = settings.ui.visible_layer.min(max_layer_count - 1);
    settings.ui.selected.layer = settings.ui.visible_layer;
    settings.ui.selection.clear();
    settings.ui.selection_hidden = false;
    settings.ui.rectangle_start = None;
    settings.ui.line_start = None;
    settings.ui.pending_plain_selection = false;
    settings.ui.height_input_address = None;
    let mut overlays = OverlayMeshes::new(&gpu.device);
    overlays.refresh(
        &gpu.device,
        &gpu.queue,
        &source_map,
        &mut document,
        &settings.ui,
    );
    finish_upload_batch(&gpu.device, &gpu.queue);
    let mut status = format!(
        "Projeto carregado: {} com {} pacotes de contexto.",
        source_map.name, package_count
    );
    let mut visual_failed = false;
    let textured_scene = if settings.ui.textured_view {
        match prepare_visual(&loader, &package, origin, &gpu) {
            Ok(visual) => Some(visual.scene),
            Err(error) => {
                visual_failed = true;
                status.push_str(&format!(" {error}"));
                None
            }
        }
    } else {
        None
    };
    if let Err(error) = editor::save_memory(&EditorMemory {
        client_root: settings.client_root.to_string_lossy().into_owned(),
        geodata_path: settings.path.to_string_lossy().into_owned(),
        map_type: settings.map_type,
        theme: settings.ui.theme,
    }) {
        status.push_str(&format!(" Aviso: memória não salva: {error}"));
    }
    ProjectOutcome::Ready(Box::new(PreparedProject {
        document,
        source_map,
        collision_meshes,
        overlays,
        textured_scene,
        active: ActiveProject {
            client_root: settings.client_root,
            package,
            origin,
        },
        max_layer_count,
        visible_layer: settings.ui.visible_layer,
        package_count,
        visual_failed,
        status,
    }))
}

fn prepare_visual(
    loader: &PackageLoader,
    package: &str,
    origin: Vec3,
    gpu: &GpuContext,
) -> Result<PreparedVisual, String> {
    let scene = loader
        .load_visual_scene(package)
        .map_err(|error| format!("Falha ao carregar visualização texturizada: {error}"))?;
    let batch_count = scene.batches.len();
    let scene = TexturedScene::new(
        &gpu.device,
        &gpu.queue,
        &gpu.material_layout,
        &gpu.pipelines,
        &scene,
        origin,
    );
    Ok(PreparedVisual { scene, batch_count })
}

fn finish_upload_batch(device: &wgpu::Device, queue: &wgpu::Queue) {
    let submission = queue.submit([]);
    // Waiting is confined to the worker. Submitting alone would still allow
    // staging allocations to accumulate faster than the GPU consumes them.
    device.poll(wgpu::Maintain::WaitForSubmissionIndex(submission));
}

// 16 MiB amortizes queue fences while keeping pending upload staging small.
// One indivisible texture or buffer may exceed it, but starts in an empty batch.
const UPLOAD_BATCH_BYTES: u64 = 16 * 1024 * 1024;

fn reserve_upload_bytes(pending: &Cell<u64>, bytes: u64, flush: impl FnOnce()) {
    let mut current = pending.get();
    if current > 0 && bytes > UPLOAD_BATCH_BYTES.saturating_sub(current) {
        flush();
        current = 0;
    }
    pending.set(current + bytes);
}

fn material_texture_upload_bytes(mut width: u32, mut height: u32) -> u64 {
    let mut bytes = 0;
    loop {
        // write_texture pads staging rows to the backend's copy pitch. Use the
        // portable 256-byte alignment conservatively, including every small mip.
        let row_bytes = u64::from(width) * 4;
        let alignment = u64::from(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        bytes += row_bytes.div_ceil(alignment) * alignment * u64::from(height);
        if width == 1 && height == 1 {
            return bytes;
        }
        width = (width / 2).max(1);
        height = (height / 2).max(1);
    }
}

impl TexturedScene {
    pub(super) fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        material_layout: &wgpu::BindGroupLayout,
        resources: &TexturedPipelineResources,
        scene: &VisualScene,
        origin: Vec3,
    ) -> Self {
        let pending_upload_bytes = Cell::new(0);
        let mut textures = HashMap::new();
        let sampler = create_material_sampler(device);
        let texture_key = |texture: Option<&VisualTexture>| {
            texture.map(|texture| (texture.width, texture.height, texture.rgba.as_ptr()))
        };
        let mut texture_view = |texture: Option<&VisualTexture>| {
            Arc::clone(textures.entry(texture_key(texture)).or_insert_with(|| {
                let (width, height, rgba) = texture
                    .map_or((1, 1, FALLBACK_MATERIAL_RGBA.as_slice()), |texture| {
                        (texture.width, texture.height, texture.rgba.as_ref())
                    });
                reserve_upload_bytes(
                    &pending_upload_bytes,
                    material_texture_upload_bytes(width, height),
                    || finish_upload_batch(device, queue),
                );
                Arc::new(create_material_texture(device, queue, width, height, rgba))
            }))
        };
        let mut materials = HashMap::new();
        let mut pipeline_indices = HashMap::new();
        let mut prepared = Self {
            batches: Vec::with_capacity(scene.batches.len()),
            pipelines: Vec::new(),
            visible_order: Vec::with_capacity(scene.batches.len()),
            view_projection: None,
            opaque_order: Vec::new(),
            blended_order: Vec::new(),
            sort_direction: None,
        };
        for batch in &scene.batches {
            let state = batch.material.state;
            if batch.indices.is_empty() || state.blend == VisualBlend::Invisible {
                continue;
            }
            let pipeline_key = TexturedPipelineKey::from(state);
            let pipeline = *pipeline_indices.entry(pipeline_key).or_insert_with(|| {
                let index = prepared.pipelines.len();
                prepared
                    .pipelines
                    .push(create_textured_pipeline(device, resources, pipeline_key));
                index
            });
            let diffuse = batch
                .material
                .texture
                .as_ref()
                .filter(|texture| texture.width > 0 && texture.height > 0);
            let opacity = batch
                .material
                .opacity
                .as_ref()
                .filter(|texture| texture.width > 0 && texture.height > 0);
            let material_key = (state, texture_key(diffuse), texture_key(opacity));
            let material = Arc::clone(materials.entry(material_key).or_insert_with(|| {
                let diffuse = texture_view(diffuse);
                let opacity = opacity.map(|texture| texture_view(Some(texture)));
                Arc::new(create_material_bind_group(
                    device,
                    material_layout,
                    &sampler,
                    &diffuse,
                    opacity.as_deref(),
                    state,
                ))
            }));
            let vertices = textured_batch_vertices(batch, origin);
            let index = prepared.batches.len();
            if state.blend == VisualBlend::Opaque {
                prepared.opaque_order.push(index);
            } else {
                prepared.blended_order.push((index, 0.0));
            }
            let mut min = [f32::INFINITY; 3];
            let mut max = [f32::NEG_INFINITY; 3];
            for vertex in &vertices {
                for axis in 0..3 {
                    min[axis] = min[axis].min(vertex.position[axis]);
                    max[axis] = max[axis].max(vertex.position[axis]);
                }
            }
            let center = std::array::from_fn(|axis| (min[axis] + max[axis]) * 0.5);
            let vertex_bytes = bytemuck::cast_slice(&vertices);
            reserve_upload_bytes(&pending_upload_bytes, vertex_bytes.len() as u64, || {
                finish_upload_batch(device, queue);
            });
            let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("editor-textured-batch-vertices"),
                contents: vertex_bytes,
                usage: wgpu::BufferUsages::VERTEX,
            });
            let index_bytes = bytemuck::cast_slice(&batch.indices);
            reserve_upload_bytes(&pending_upload_bytes, index_bytes.len() as u64, || {
                finish_upload_batch(device, queue);
            });
            let index_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("editor-textured-batch-indices"),
                contents: index_bytes,
                usage: wgpu::BufferUsages::INDEX,
            });
            prepared.batches.push(TexturedBatch {
                vertices: vertex_buffer,
                indices: index_buffer,
                index_count: batch.indices.len() as u32,
                material,
                pipeline,
                center,
                bounds: [min, max],
            });
        }
        finish_upload_batch(device, queue);
        prepared
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::Cell,
        path::PathBuf,
        sync::{Arc, mpsc},
        thread,
        time::{Duration, Instant},
    };

    use super::{
        ActiveJob, Completion, GpuContext, Job, LoadingState, ProjectOutcome, ProjectSettings,
        UPLOAD_BATCH_BYTES, material_texture_upload_bytes, prepare_project, reserve_upload_bytes,
    };
    use crate::{
        editor::{self, MapType},
        editor_view::{
            EditorUi, TexturedPipelineResources, create_camera_layout,
            create_material_texture_layout,
        },
        l2j::Document,
    };

    #[test]
    fn upload_admission_flushes_before_overflow_and_isolates_oversized_uploads() {
        let pending = Cell::new(0);
        let mut flushed = Vec::new();
        for bytes in [UPLOAD_BATCH_BYTES - 4, 4, 4, UPLOAD_BATCH_BYTES + 4, 4] {
            reserve_upload_bytes(&pending, bytes, || flushed.push(pending.get()));
            assert!(pending.get() <= UPLOAD_BATCH_BYTES.max(bytes));
        }
        assert_eq!(flushed, [UPLOAD_BATCH_BYTES, 4, UPLOAD_BATCH_BYTES + 4]);
        assert_eq!(pending.get(), 4);
    }

    #[test]
    fn texture_upload_budget_includes_small_and_non_square_mips() {
        // The fallback still needs one staging row; a 17x3 texture has mip
        // heights 3, 1, 1, 1, 1, all narrower than one portable copy pitch.
        assert_eq!(material_texture_upload_bytes(1, 1), 256);
        assert_eq!(material_texture_upload_bytes(17, 3), 7 * 256);
    }

    fn wait_for_completion(state: &mut LoadingState) -> Completion {
        let started = Instant::now();
        loop {
            if let Some(result) = state.poll() {
                return result;
            }
            assert!(
                started.elapsed() < Duration::from_secs(600),
                "loader timed out"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn project_slot_stays_busy_until_its_failure_is_consumed() {
        let (release, gate) = mpsc::sync_channel(1);
        let job = Job::spawn(move || {
            gate.recv().expect("release worker");
            ProjectOutcome::Error("Unreadable geodata".into())
        })
        .expect("spawn worker");
        let mut state = LoadingState {
            job: Some(ActiveJob::Project(job)),
            active: None,
        };
        assert!(state.is_busy());
        assert!(state.is_project_loading());
        // The worker cannot finish until the caller releases it; polling must
        // nevertheless return so the UI can keep processing input and drawing.
        assert!(state.poll().is_none());
        assert!(state.is_project_loading());
        release.send(()).expect("release worker");
        match wait_for_completion(&mut state) {
            Completion::Project(ProjectOutcome::Error(error)) => {
                assert_eq!(error, "Unreadable geodata");
            }
            _ => panic!("project failure must survive delivery"),
        }
        assert!(!state.is_busy());
        assert!(!state.is_project_loading());
        assert!(state.poll().is_none());
    }

    #[test]
    fn worker_panic_releases_the_loading_slot() {
        let job = Job::spawn(|| panic!("decode failed unexpectedly")).expect("spawn worker");
        let mut state = LoadingState {
            job: Some(ActiveJob::Project(job)),
            active: None,
        };
        assert!(matches!(
            wait_for_completion(&mut state),
            Completion::Disconnected(_)
        ));
        assert!(!state.is_busy());
        assert!(!state.is_project_loading());
    }

    #[test]
    #[ignore = "requires GEODATA_EDITOR_CLIENT, GEODATA_EDITOR_L2J and a GPU"]
    fn real_worker_prepares_matching_document_collision_overlays_and_textures() {
        let client_root = PathBuf::from(
            std::env::var("GEODATA_EDITOR_CLIENT").expect("set GEODATA_EDITOR_CLIENT"),
        );
        let path =
            PathBuf::from(std::env::var("GEODATA_EDITOR_L2J").expect("set GEODATA_EDITOR_L2J"));
        let reference = Document::open(&path).expect("read reference geodata");
        let region = editor::geodata_region(&path).expect("region in geodata filename");
        let instance = wgpu::Instance::default();
        let adapter =
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
                .expect("GPU adapter");
        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("background-loading-smoke"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
            },
            None,
        ))
        .expect("GPU device");
        let material_layout = Arc::new(create_material_texture_layout(&device));
        let camera_layout = create_camera_layout(&device);
        let pipelines = Arc::new(TexturedPipelineResources::new(
            &device,
            &camera_layout,
            &material_layout,
            wgpu::TextureFormat::Rgba8UnormSrgb,
        ));
        let device = Arc::new(device);
        let queue = Arc::new(queue);
        let gpu_context = || GpuContext {
            device: Arc::clone(&device),
            queue: Arc::clone(&queue),
            material_layout: Arc::clone(&material_layout),
            pipelines: Arc::clone(&pipelines),
        };
        let mut settings = ProjectSettings {
            client_root,
            path,
            region,
            map_type: MapType::Classic,
            ui: EditorUi {
                visual_stride: 1,
                open_context_radius: 16,
                show_nswe_icons: true,
                textured_view: true,
                ..Default::default()
            },
        };
        device.push_error_scope(wgpu::ErrorFilter::Validation);
        let mut state = LoadingState::default();
        let mut accepted_flavour = false;
        let project = loop {
            let request = settings.clone();
            let gpu = gpu_context();
            state.job = Some(ActiveJob::Project(
                Job::spawn(move || prepare_project(request, gpu)).expect("start real worker"),
            ));
            match wait_for_completion(&mut state) {
                Completion::Project(ProjectOutcome::Ready(project)) => break project,
                Completion::Project(ProjectOutcome::Prompt(_, pending)) => {
                    assert!(!accepted_flavour, "accepting the offer must resolve it");
                    accepted_flavour = true;
                    settings.map_type = pending.available_type;
                }
                Completion::Project(ProjectOutcome::Error(error))
                | Completion::Disconnected(error) => panic!("{error}"),
                Completion::Visual(_) => panic!("project request returned a visual-only result"),
            }
        };
        assert!(!state.is_busy());
        assert_eq!(
            project.source_map.name,
            settings.map_type.package_name(&settings.region)
        );
        for block_y in 0..256 {
            for block_x in 0..256 {
                assert_eq!(
                    project.document.block(block_x, block_y),
                    reference.block(block_x, block_y),
                    "background loading must preserve the editable file contents"
                );
            }
        }
        assert!(!project.visual_failed, "{}", project.status);
        let visual = project.textured_scene.expect("ready textured scene");
        assert!(visual.batches.iter().any(|batch| batch.index_count >= 3));
        assert_eq!(
            project.collision_meshes.terrain.triangle_count as usize,
            project.source_map.geometry.terrain_triangles * 3,
        );
        assert!(!project.overlays.needs_icons(&settings.ui));
        assert!(pollster::block_on(device.pop_error_scope()).is_none());
        println!(
            "Loaded {}: {} collision triangles, {} visual batches; document unchanged, icons ready",
            project.source_map.name,
            project.source_map.triangles.len(),
            visual.batches.len()
        );
    }
}
