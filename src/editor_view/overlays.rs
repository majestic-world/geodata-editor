use std::ops::Range;

use bytemuck::Pod;
use wgpu::util::DeviceExt;

use crate::{
    l2j::{BLOCKS_PER_AXIS, CELLS_PER_BLOCK_AXIS, Document, MAP_CELLS},
    unreal::SourceMap,
};

use super::{
    CpuGeodata, CpuNsweIcons, EditorOverlayOptions, EditorSelectionLookup, EditorUi, QuadVertex,
    editor_geodata_instances_in_blocks, editor_nswe_icon_instances_in_blocks, map_origin,
};

const CHUNK_BLOCKS: usize = 16;
const CHUNKS_PER_AXIS: usize = BLOCKS_PER_AXIS.div_ceil(CHUNK_BLOCKS);
const CHUNK_COUNT: usize = CHUNKS_PER_AXIS * CHUNKS_PER_AXIS;
const CHUNK_CELLS: usize = CHUNK_BLOCKS * CELLS_PER_BLOCK_AXIS;
const UPLOAD_BATCH_BYTES: u64 = 8 * 1024 * 1024;

/// One shared quad and at most 256 instance draws per overlay pass.
pub(super) struct OverlayMeshes {
    quad_vertices: wgpu::Buffer,
    triangles: wgpu::Buffer,
    lines: wgpu::Buffer,
    chunks: Vec<ChunkMeshes>,
    updates: ChunkUpdates,
}

impl OverlayMeshes {
    pub(super) fn new(device: &wgpu::Device) -> Self {
        let vertices = [
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
        Self {
            quad_vertices: device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("editor-overlay-quad"),
                contents: bytemuck::cast_slice(&vertices),
                usage: wgpu::BufferUsages::VERTEX,
            }),
            triangles: device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("editor-overlay-triangles"),
                contents: bytemuck::cast_slice(&[0_u16, 2, 1, 1, 2, 3]),
                usage: wgpu::BufferUsages::INDEX,
            }),
            lines: device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("editor-overlay-lines"),
                contents: bytemuck::cast_slice(&[0_u16, 1, 1, 3, 3, 2, 2, 0]),
                usage: wgpu::BufferUsages::INDEX,
            }),
            chunks: (0..CHUNK_COUNT).map(|_| ChunkMeshes::default()).collect(),
            updates: ChunkUpdates::default(),
        }
    }

    pub(super) fn refresh(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        map: &SourceMap,
        document: &mut Document,
        ui: &EditorUi,
    ) {
        let mut pending_bytes = 0;
        let chunks = &mut self.chunks;
        self.updates
            .refresh(map, document, ui, |index, geodata, icons| {
                let chunk = &mut chunks[index];
                if let Some(mesh) = geodata {
                    pending_bytes += chunk.geodata.upload(
                        device,
                        queue,
                        &mesh.instances,
                        "editor-geodata-chunk",
                    );
                }
                if let Some(mesh) = icons {
                    pending_bytes +=
                        chunk
                            .icons
                            .upload(device, queue, &mesh.instances, "editor-nswe-chunk");
                }
                // Full rebuilds also run in the loading worker. Submit batches so
                // queue.write_buffer staging does not retain the entire region.
                if pending_bytes >= UPLOAD_BATCH_BYTES {
                    queue.submit([]);
                    pending_bytes = 0;
                }
            });
    }

    pub(super) fn needs_icons(&self, ui: &EditorUi) -> bool {
        icons_visible(ui) && self.updates.icons_dirty.iter().any(|dirty| *dirty)
    }

    pub(super) fn draw_geodata<'a>(
        &'a self,
        pass: &mut wgpu::RenderPass<'a>,
        pipeline: &'a wgpu::RenderPipeline,
        camera: &'a wgpu::BindGroup,
        lines: bool,
    ) {
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, camera, &[]);
        pass.set_vertex_buffer(0, self.quad_vertices.slice(..));
        pass.set_index_buffer(
            if lines {
                self.lines.slice(..)
            } else {
                self.triangles.slice(..)
            },
            wgpu::IndexFormat::Uint16,
        );
        for chunk in &self.chunks {
            chunk.geodata.draw(pass, if lines { 0..8 } else { 0..6 });
        }
    }

    pub(super) fn draw_icons<'a>(
        &'a self,
        pass: &mut wgpu::RenderPass<'a>,
        pipeline: &'a wgpu::RenderPipeline,
        camera: &'a wgpu::BindGroup,
        atlas: &'a wgpu::BindGroup,
    ) {
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, camera, &[]);
        pass.set_bind_group(1, atlas, &[]);
        pass.set_vertex_buffer(0, self.quad_vertices.slice(..));
        pass.set_index_buffer(self.triangles.slice(..), wgpu::IndexFormat::Uint16);
        for chunk in &self.chunks {
            chunk.icons.draw(pass, 0..6);
        }
    }
}

#[derive(Default)]
struct ChunkMeshes {
    geodata: InstanceBuffer,
    icons: InstanceBuffer,
}

#[derive(Default)]
struct InstanceBuffer {
    buffer: Option<wgpu::Buffer>,
    capacity: u64,
    count: u32,
}

impl InstanceBuffer {
    fn upload<T: Pod>(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        instances: &[T],
        label: &'static str,
    ) -> u64 {
        self.count = instances.len() as u32;
        if instances.is_empty() {
            return 0;
        }
        let bytes = bytemuck::cast_slice(instances);
        let size = bytes.len() as u64;
        if size > self.capacity {
            self.capacity = size
                .next_power_of_two()
                .min(device.limits().max_buffer_size);
            self.buffer = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: self.capacity,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
        }
        queue.write_buffer(
            self.buffer.as_ref().expect("allocated instance buffer"),
            0,
            bytes,
        );
        size
    }

    fn draw<'a>(&'a self, pass: &mut wgpu::RenderPass<'a>, indices: Range<u32>) {
        if self.count == 0 {
            return;
        }
        if let Some(buffer) = &self.buffer {
            pass.set_vertex_buffer(1, buffer.slice(..));
            pass.draw_indexed(indices, 0, 0..self.count);
        }
    }
}

struct OverlaySnapshot {
    stride: usize,
    visibility: EditorOverlayOptions,
    selection: EditorSelectionLookup,
}

impl OverlaySnapshot {
    fn new(document: &Document, ui: &EditorUi) -> Self {
        let selection = if ui.selection_hidden {
            &[]
        } else if ui.selection.is_empty() {
            std::slice::from_ref(&ui.selected)
        } else {
            ui.selection.as_slice()
        };
        let mut visibility = EditorOverlayOptions::from_ui(ui);
        visibility.selected.layer = ui.visible_layer;
        Self {
            stride: ui.visual_stride.max(1),
            visibility,
            selection: EditorSelectionLookup::new(document, selection),
        }
    }

    fn global_filters_changed(&self, next: &Self) -> bool {
        self.stride != next.stride
            || self.visibility.show_all_open_cells != next.visibility.show_all_open_cells
            || self.visibility.hide_fully_open_blocks != next.visibility.hide_fully_open_blocks
            || self.visibility.show_selected_layer_only != next.visibility.show_selected_layer_only
            || (self.visibility.show_selected_layer_only
                && self.visibility.selected.layer != next.visibility.selected.layer)
    }

    fn context(&self) -> Option<(usize, usize, usize)> {
        if self.visibility.show_all_open_cells || self.visibility.hide_fully_open_blocks {
            None
        } else {
            Some((
                self.visibility.selected.x,
                self.visibility.selected.y,
                self.visibility.open_context_radius,
            ))
        }
    }
}

/// CPU scheduling and generation stay independent of GPU ownership so cache
/// transitions can be checked against a full-region render without a device.
struct ChunkUpdates {
    snapshot: Option<OverlaySnapshot>,
    geodata_dirty: [bool; CHUNK_COUNT],
    icons_dirty: [bool; CHUNK_COUNT],
}

impl Default for ChunkUpdates {
    fn default() -> Self {
        Self {
            snapshot: None,
            geodata_dirty: [true; CHUNK_COUNT],
            icons_dirty: [true; CHUNK_COUNT],
        }
    }
}

impl ChunkUpdates {
    fn refresh(
        &mut self,
        map: &SourceMap,
        document: &mut Document,
        ui: &EditorUi,
        mut replace: impl FnMut(usize, Option<CpuGeodata>, Option<CpuNsweIcons>),
    ) {
        let next = OverlaySnapshot::new(document, ui);
        let mut changed = [false; CHUNK_COUNT];
        for (block_x, block_y) in document.take_render_changes() {
            mark_block(&mut changed, block_x, block_y);
        }
        if let Some(previous) = &self.snapshot {
            if previous.global_filters_changed(&next) {
                changed.fill(true);
            } else {
                if previous.context() != next.context() {
                    for (x, y, radius) in previous.context().into_iter().chain(next.context()) {
                        mark_cells(
                            &mut changed,
                            x.saturating_sub(radius),
                            y.saturating_sub(radius),
                            x.saturating_add(radius),
                            y.saturating_add(radius),
                        );
                    }
                }
                for &(x, y, _) in previous
                    .selection
                    .cells
                    .symmetric_difference(&next.selection.cells)
                {
                    // A selected cell suppresses any coarse open sample whose
                    // footprint contains it, including samples in earlier chunks.
                    let halo = next.stride - 1;
                    mark_cells(
                        &mut changed,
                        x.saturating_sub(halo),
                        y.saturating_sub(halo),
                        x,
                        y,
                    );
                }
                for &(block_x, block_y) in previous
                    .selection
                    .simple_blocks
                    .symmetric_difference(&next.selection.simple_blocks)
                {
                    mark_block(&mut changed, block_x, block_y);
                }
            }
        } else {
            changed.fill(true);
        }
        for (index, changed) in changed.into_iter().enumerate() {
            self.geodata_dirty[index] |= changed;
            self.icons_dirty[index] |= changed;
        }
        self.snapshot = Some(next);
        let snapshot = self
            .snapshot
            .as_ref()
            .expect("initialized overlay snapshot");
        let origin = map_origin(map.bounds);
        let show_icons = icons_visible(ui);
        for index in 0..CHUNK_COUNT {
            let (block_x, block_y) = chunk_blocks(index);
            let geodata = self.geodata_dirty[index].then(|| {
                editor_geodata_instances_in_blocks(
                    map,
                    document,
                    origin,
                    snapshot.stride,
                    snapshot.visibility,
                    &snapshot.selection,
                    block_x.clone(),
                    block_y.clone(),
                )
            });
            let icons = (show_icons && self.icons_dirty[index]).then(|| {
                editor_nswe_icon_instances_in_blocks(
                    map,
                    document,
                    origin,
                    snapshot.stride,
                    snapshot.visibility,
                    &snapshot.selection,
                    block_x,
                    block_y,
                )
            });
            if geodata.is_some() || icons.is_some() {
                self.geodata_dirty[index] = false;
                if show_icons {
                    self.icons_dirty[index] = false;
                }
                replace(index, geodata, icons);
            }
        }
    }
}

fn icons_visible(ui: &EditorUi) -> bool {
    ui.show_nswe_icons && !ui.hide_all_blocks
}

fn chunk_blocks(index: usize) -> (Range<usize>, Range<usize>) {
    let x = index / CHUNKS_PER_AXIS * CHUNK_BLOCKS;
    let y = index % CHUNKS_PER_AXIS * CHUNK_BLOCKS;
    (
        x..(x + CHUNK_BLOCKS).min(BLOCKS_PER_AXIS),
        y..(y + CHUNK_BLOCKS).min(BLOCKS_PER_AXIS),
    )
}

fn mark_block(changed: &mut [bool; CHUNK_COUNT], x: usize, y: usize) {
    if x < BLOCKS_PER_AXIS && y < BLOCKS_PER_AXIS {
        changed[x / CHUNK_BLOCKS * CHUNKS_PER_AXIS + y / CHUNK_BLOCKS] = true;
    }
}

fn mark_cells(
    changed: &mut [bool; CHUNK_COUNT],
    min_x: usize,
    min_y: usize,
    max_x: usize,
    max_y: usize,
) {
    if min_x >= MAP_CELLS || min_y >= MAP_CELLS {
        return;
    }
    for x in min_x / CHUNK_CELLS..=max_x.min(MAP_CELLS - 1) / CHUNK_CELLS {
        for y in min_y / CHUNK_CELLS..=max_y.min(MAP_CELLS - 1) / CHUNK_CELLS {
            changed[x * CHUNKS_PER_AXIS + y] = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use bytemuck::Pod;

    use crate::{editor::EditorOptions, l2j::LayerAddress};

    use super::super::welcome_source_map;
    use super::{
        BLOCKS_PER_AXIS, CHUNK_COUNT, ChunkUpdates, CpuGeodata, CpuNsweIcons, Document, EditorUi,
        OverlaySnapshot, SourceMap, editor_geodata_instances_in_blocks,
        editor_nswe_icon_instances_in_blocks, map_origin,
    };

    struct CpuCache {
        updates: ChunkUpdates,
        geodata: Vec<CpuGeodata>,
        icons: Vec<CpuNsweIcons>,
    }

    impl CpuCache {
        fn new() -> Self {
            Self {
                updates: ChunkUpdates::default(),
                geodata: (0..CHUNK_COUNT).map(|_| CpuGeodata::default()).collect(),
                icons: (0..CHUNK_COUNT).map(|_| CpuNsweIcons::default()).collect(),
            }
        }

        fn refresh(
            &mut self,
            map: &SourceMap,
            document: &mut Document,
            ui: &EditorUi,
        ) -> (Vec<usize>, Vec<usize>) {
            let mut geodata_chunks = Vec::new();
            let mut icon_chunks = Vec::new();
            self.updates
                .refresh(map, document, ui, |index, geodata, icons| {
                    if let Some(mesh) = geodata {
                        self.geodata[index] = mesh;
                        geodata_chunks.push(index);
                    }
                    if let Some(mesh) = icons {
                        self.icons[index] = mesh;
                        icon_chunks.push(index);
                    }
                });
            (geodata_chunks, icon_chunks)
        }

        fn assert_matches_full(&self, map: &SourceMap, document: &Document, ui: &EditorUi) {
            let snapshot = OverlaySnapshot::new(document, ui);
            let full = editor_geodata_instances_in_blocks(
                map,
                document,
                map_origin(map.bounds),
                snapshot.stride,
                snapshot.visibility,
                &snapshot.selection,
                0..BLOCKS_PER_AXIS,
                0..BLOCKS_PER_AXIS,
            );
            assert_eq!(
                canonical_instances(self.geodata.iter().flat_map(|mesh| mesh.instances.iter())),
                canonical_instances(full.instances.iter()),
            );
            if ui.show_nswe_icons && !ui.hide_all_blocks {
                let full = editor_nswe_icon_instances_in_blocks(
                    map,
                    document,
                    map_origin(map.bounds),
                    snapshot.stride,
                    snapshot.visibility,
                    &snapshot.selection,
                    0..BLOCKS_PER_AXIS,
                    0..BLOCKS_PER_AXIS,
                );
                assert_eq!(
                    canonical_instances(self.icons.iter().flat_map(|mesh| mesh.instances.iter())),
                    canonical_instances(full.instances.iter()),
                );
            }
        }
    }

    fn canonical_instances<'a, T: Pod + 'a>(
        instances: impl Iterator<Item = &'a T>,
    ) -> Vec<Vec<u8>> {
        let mut bytes = instances
            .map(|instance| bytemuck::bytes_of(instance).to_vec())
            .collect::<Vec<_>>();
        bytes.sort_unstable();
        bytes
    }

    #[test]
    fn selected_sample_crossing_a_chunk_boundary_matches_full_generation() {
        let map = welcome_source_map(&EditorOptions::default());
        let mut document = Document::blank();
        document.convert_simple_to_complex(15, 8).unwrap();
        document.convert_simple_to_complex(16, 8).unwrap();
        let mut ui = EditorUi {
            selected: LayerAddress::new(128, 66, 0),
            selection_hidden: true,
            show_all_open_cells: true,
            show_nswe_icons: true,
            visual_stride: 3,
            ..EditorUi::default()
        };
        let mut cache = CpuCache::new();
        cache.refresh(&map, &mut document, &ui);
        ui.selection_hidden = false;
        let (geodata, icons) = cache.refresh(&map, &mut document, &ui);
        assert_eq!(geodata, [0, 16]);
        assert_eq!(icons, [0, 16]);
        cache.assert_matches_full(&map, &document, &ui);

        ui.selection_hidden = true;
        cache.refresh(&map, &mut document, &ui);
        cache.assert_matches_full(&map, &document, &ui);
    }

    #[test]
    fn hidden_icons_defer_edits_and_restore_the_current_document() {
        let map = welcome_source_map(&EditorOptions::default());
        let mut document = Document::blank();
        let mut ui = EditorUi {
            selected: LayerAddress::new(130, 130, 0),
            show_nswe_icons: true,
            open_context_radius: 8,
            visual_stride: 1,
            ..EditorUi::default()
        };
        let mut cache = CpuCache::new();
        cache.refresh(&map, &mut document, &ui);
        ui.show_nswe_icons = false;
        document.force_set_nswe([ui.selected], 0, "Block selected cell");
        let (geodata, icons) = cache.refresh(&map, &mut document, &ui);
        assert_eq!(geodata, [17]);
        assert!(icons.is_empty(), "hidden glyphs must not be generated");
        cache.assert_matches_full(&map, &document, &ui);

        ui.show_nswe_icons = true;
        ui.hide_all_blocks = true;
        document
            .set_height([ui.selected], 64, "Raise cell")
            .unwrap();
        let (_, icons) = cache.refresh(&map, &mut document, &ui);
        assert!(
            icons.is_empty(),
            "the master hide filter also defers glyphs"
        );
        ui.hide_all_blocks = false;
        let (geodata, icons) = cache.refresh(&map, &mut document, &ui);
        assert!(geodata.is_empty());
        assert_eq!(icons, [17]);
        cache.assert_matches_full(&map, &document, &ui);

        assert!(document.undo());
        cache.refresh(&map, &mut document, &ui);
        cache.assert_matches_full(&map, &document, &ui);
        assert!(document.redo());
        cache.refresh(&map, &mut document, &ui);
        cache.assert_matches_full(&map, &document, &ui);
        document.restore_block(16, 16).unwrap();
        cache.refresh(&map, &mut document, &ui);
        cache.assert_matches_full(&map, &document, &ui);
    }

    #[test]
    fn moving_local_context_rebuilds_only_old_and_new_chunks() {
        let map = welcome_source_map(&EditorOptions::default());
        let mut document = Document::blank();
        let mut ui = EditorUi {
            selected: LayerAddress::new(127, 127, 0),
            selection_hidden: true,
            show_nswe_icons: true,
            open_context_radius: 2,
            visual_stride: 1,
            ..EditorUi::default()
        };
        let mut cache = CpuCache::new();
        cache.refresh(&map, &mut document, &ui);
        ui.selected = LayerAddress::new(257, 257, 0);
        let (geodata, _) = cache.refresh(&map, &mut document, &ui);
        assert_eq!(geodata, [0, 1, 16, 17, 18, 33, 34]);
        cache.assert_matches_full(&map, &document, &ui);

        ui.open_context_radius = 0;
        cache.refresh(&map, &mut document, &ui);
        cache.assert_matches_full(&map, &document, &ui);
        let (geodata, icons) = cache.refresh(&map, &mut document, &ui);
        assert!(geodata.is_empty() && icons.is_empty());
    }

    #[test]
    fn global_filters_replace_all_cached_geometry() {
        let map = welcome_source_map(&EditorOptions::default());
        let mut document = Document::blank();
        document.convert_simple_to_complex(0, 0).unwrap();
        let mut ui = EditorUi {
            selection_hidden: true,
            show_nswe_icons: true,
            open_context_radius: 8,
            visual_stride: 1,
            ..EditorUi::default()
        };
        let mut cache = CpuCache::new();
        cache.refresh(&map, &mut document, &ui);
        let changes: [fn(&mut EditorUi); 5] = [
            |ui| ui.visual_stride = 3,
            |ui| ui.show_selected_layer_only = true,
            |ui| ui.visible_layer = 1,
            |ui| ui.show_all_open_cells = true,
            |ui| ui.hide_fully_open_blocks = true,
        ];
        for change in changes {
            change(&mut ui);
            let (geodata, icons) = cache.refresh(&map, &mut document, &ui);
            assert_eq!(geodata.len(), CHUNK_COUNT);
            assert_eq!(icons.len(), CHUNK_COUNT);
            cache.assert_matches_full(&map, &document, &ui);
        }
        assert!(cache.geodata.iter().all(|mesh| mesh.instances.is_empty()));
    }
}
