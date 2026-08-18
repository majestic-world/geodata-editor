//! Lossless editable document for L2J, encrypted L2G, and `_conv.dat`.
//!
//! Clean blocks are copied straight from the opened file. This preserves old
//! simple-block low nibbles that GeoEngine ignores, making an unedited export
//! byte-for-byte identical to its input.

use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::Write,
    ops::Range,
    path::{Path, PathBuf},
};

use crate::error::{AppError, Result};

pub const BLOCKS_PER_AXIS: usize = 256;
pub const BLOCK_COUNT: usize = BLOCKS_PER_AXIS * BLOCKS_PER_AXIS;
pub const CELLS_PER_BLOCK_AXIS: usize = 8;
pub const MAP_CELLS: usize = BLOCKS_PER_AXIS * CELLS_PER_BLOCK_AXIS;
pub const COLUMNS_PER_BLOCK: usize = 64;
pub const NULL_HEIGHT: i16 = -16_384;
pub const MAX_WALKABLE_CLIMB: i16 = 16;
/// L2J reserves the four least-significant packed bits for NSWE. Heights are
/// consequently stored in eight-unit increments.
pub const HEIGHT_STEP: i16 = 8;
/// `NULL_HEIGHT` is the -16384 sentinel, so the first editable value is one
/// L2J increment above it.
pub const MIN_EDITABLE_HEIGHT: i16 = NULL_HEIGHT + HEIGHT_STEP;
pub const MAX_EDITABLE_HEIGHT: i16 = 16_376;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Layer {
    pub height: i16,
    /// Bits follow the Lineage convention: N=8, S=4, W=2, E=1.
    pub nswe: u8,
}

impl Layer {
    pub const OPEN: u8 = 0x0f;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    North,
    South,
    West,
    East,
}

impl Direction {
    pub const ALL: [Self; 4] = [Self::North, Self::South, Self::West, Self::East];

    pub const fn bit(self) -> u8 {
        match self {
            Self::North => 8,
            Self::South => 4,
            Self::West => 2,
            Self::East => 1,
        }
    }

    pub const fn opposite(self) -> Self {
        match self {
            Self::North => Self::South,
            Self::South => Self::North,
            Self::West => Self::East,
            Self::East => Self::West,
        }
    }

    const fn offset(self) -> (isize, isize) {
        match self {
            Self::North => (0, -1),
            Self::South => (0, 1),
            Self::West => (-1, 0),
            Self::East => (1, 0),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LayerAddress {
    pub x: usize,
    pub y: usize,
    pub layer: usize,
}

impl LayerAddress {
    pub const fn new(x: usize, y: usize, layer: usize) -> Self {
        Self { x, y, layer }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditableBlockType {
    Simple,
    Complex,
    Multilayer,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditableBlock {
    Simple(Layer),
    Complex([Layer; COLUMNS_PER_BLOCK]),
    Multilayer {
        offsets: [u16; COLUMNS_PER_BLOCK + 1],
        layers: Vec<Layer>,
    },
}

impl EditableBlock {
    pub const fn kind(&self) -> EditableBlockType {
        match self {
            Self::Simple(_) => EditableBlockType::Simple,
            Self::Complex(_) => EditableBlockType::Complex,
            Self::Multilayer { .. } => EditableBlockType::Multilayer,
        }
    }

    pub fn layers(&self, column: usize) -> &[Layer] {
        match self {
            Self::Simple(layer) => std::slice::from_ref(layer),
            Self::Complex(cells) => &cells[column..=column],
            Self::Multilayer { offsets, layers } => {
                &layers[offsets[column] as usize..offsets[column + 1] as usize]
            }
        }
    }

    fn layer(&self, column: usize, layer: usize) -> Option<Layer> {
        self.layers(column).get(layer).copied()
    }

    fn layer_count(&self, column: usize) -> usize {
        self.layers(column).len()
    }

    fn max_layer_count(&self) -> usize {
        match self {
            Self::Simple(_) | Self::Complex(_) => 1,
            Self::Multilayer { offsets, .. } => offsets
                .windows(2)
                .map(|window| usize::from(window[1] - window[0]))
                .max()
                .unwrap_or(0),
        }
    }

    fn promote_to_complex(&mut self) -> bool {
        let Self::Simple(layer) = self else {
            return false;
        };
        *self = Self::Complex([*layer; COLUMNS_PER_BLOCK]);
        true
    }

    fn set_layer_nswe(&mut self, column: usize, layer: usize, nswe: u8) -> Result<()> {
        match self {
            Self::Simple(_) => Err(AppError::InvalidData(
                "simple block must be promoted before a cell edit".into(),
            )),
            Self::Complex(cells) => {
                if layer != 0 || column >= COLUMNS_PER_BLOCK {
                    return Err(AppError::InvalidArgument(
                        "invalid complex cell layer".into(),
                    ));
                }
                cells[column].nswe = nswe & 0x0f;
                Ok(())
            }
            Self::Multilayer { offsets, layers } => {
                let start = offsets[column] as usize;
                let end = offsets[column + 1] as usize;
                let cell = layers
                    .get_mut(start + layer)
                    .filter(|_| start + layer < end)
                    .ok_or_else(|| {
                        AppError::InvalidArgument("invalid multilayer cell layer".into())
                    })?;
                cell.nswe = nswe & 0x0f;
                Ok(())
            }
        }
    }

    fn set_layer_height(&mut self, column: usize, layer: usize, height: i16) -> Result<()> {
        match self {
            Self::Simple(_) => Err(AppError::InvalidData(
                "simple block must be promoted before a cell height edit".into(),
            )),
            Self::Complex(cells) => {
                if layer != 0 || column >= COLUMNS_PER_BLOCK {
                    return Err(AppError::InvalidArgument(
                        "invalid complex cell layer".into(),
                    ));
                }
                cells[column].height = height;
                Ok(())
            }
            Self::Multilayer { offsets, layers } => {
                let start = offsets[column] as usize;
                let end = offsets[column + 1] as usize;
                let cell = layers
                    .get_mut(start + layer)
                    .filter(|_| start + layer < end)
                    .ok_or_else(|| {
                        AppError::InvalidArgument("invalid multilayer cell layer".into())
                    })?;
                cell.height = height;
                // The L2J layer order is height order.  Keep it canonical
                // after a manual edit so readers consistently find the floor
                // before higher platforms in a multilayer column.
                layers[start..end].sort_by_key(|entry| entry.height);
                Ok(())
            }
        }
    }

    fn to_multilayer(&mut self) {
        if matches!(self, Self::Multilayer { .. }) {
            return;
        }
        let mut offsets = [0_u16; COLUMNS_PER_BLOCK + 1];
        let mut layers = Vec::with_capacity(COLUMNS_PER_BLOCK);
        for column in 0..COLUMNS_PER_BLOCK {
            layers.push(self.layers(column)[0]);
            offsets[column + 1] = layers.len() as u16;
        }
        *self = Self::Multilayer { offsets, layers };
    }

    fn collapse_to_complex(&mut self) -> Result<()> {
        if !matches!(self, Self::Multilayer { .. }) {
            return Err(AppError::InvalidArgument(
                "only a multilayer block can become complex".into(),
            ));
        }
        if (0..COLUMNS_PER_BLOCK).any(|column| self.layer_count(column) != 1) {
            return Err(AppError::InvalidArgument(
                "multilayer needs one layer in every column to become complex".into(),
            ));
        }
        let cells = std::array::from_fn(|column| self.layers(column)[0]);
        *self = Self::Complex(cells);
        Ok(())
    }

    fn collapse_to_simple(&mut self) -> Result<()> {
        let Some(first) = self.layers(0).first().copied() else {
            return Err(AppError::InvalidData(
                "L2J block starts with an empty column".into(),
            ));
        };
        if first.nswe != Layer::OPEN
            || (0..COLUMNS_PER_BLOCK)
                .any(|column| self.layer_count(column) != 1 || self.layers(column)[0] != first)
        {
            return Err(AppError::InvalidArgument(
                "simple needs 64 equal one-layer cells with NSWE aberto".into(),
            ));
        }
        *self = Self::Simple(first);
        Ok(())
    }

    fn validate(&self, index: usize) -> Result<()> {
        if let Self::Multilayer { offsets, layers } = self {
            if offsets[0] != 0
                || offsets[COLUMNS_PER_BLOCK] as usize != layers.len()
                || offsets.windows(2).any(|pair| pair[0] >= pair[1])
            {
                return Err(AppError::InvalidData(format!(
                    "invalid multilayer block {index}"
                )));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct BlockState {
    original: EditableBlock,
    current: EditableBlock,
    bytes: Range<usize>,
}

impl BlockState {
    fn dirty(&self) -> bool {
        self.original != self.current
    }
}

#[derive(Clone, Debug)]
struct EditOperation {
    label: String,
    before: Vec<(usize, EditableBlock)>,
    after: Vec<(usize, EditableBlock)>,
}

#[derive(Clone, Debug, Default)]
pub struct EditResult {
    pub changed_cells: usize,
    pub changed_links: usize,
    pub promoted_blocks: usize,
    pub rejected_links: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct HeightEditResult {
    pub changed_cells: usize,
    pub promoted_blocks: usize,
    pub height: i16,
    pub rejected_cells: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct SaveSummary {
    pub changed_blocks: usize,
    pub conversion_blocks: usize,
    pub changed_cells: usize,
    pub changed_directions: usize,
    pub bytes: u64,
    pub path: PathBuf,
}

#[derive(Clone, Debug)]
enum StorageFormat {
    L2j,
    L2g { header: [u8; 4] },
    ConvDat { header: [u8; 18] },
}

/// Editable, validated L2J, L2G, or `_conv.dat` file. Saving over the opened
/// source is allowed; the writer creates a matching `.bak` copy first.
#[derive(Clone, Debug)]
pub struct Document {
    original_bytes: Vec<u8>,
    format: StorageFormat,
    blocks: Vec<BlockState>,
    original_path: Option<PathBuf>,
    undo: Vec<EditOperation>,
    redo: Vec<EditOperation>,
}

impl Document {
    /// A non-persistent empty canvas used only by the editor welcome screen.
    /// It is never saved automatically and is replaced when the user opens a
    /// real geodata file.
    pub fn blank() -> Self {
        let mut bytes = Vec::with_capacity(BLOCK_COUNT * 3);
        for _ in 0..BLOCK_COUNT {
            bytes.push(0);
            bytes.extend_from_slice(&0_i16.to_le_bytes());
        }
        Self::from_bytes(bytes).expect("in-memory L2J canvas is valid")
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let (format, bytes) = decode_storage(path, fs::read(path)?)?;
        let mut document = Self::from_storage_bytes(bytes, format)?;
        document.original_path = Some(path.to_path_buf());
        Ok(document)
    }

    pub fn from_bytes(original_bytes: Vec<u8>) -> Result<Self> {
        Self::from_storage_bytes(original_bytes, StorageFormat::L2j)
    }

    fn from_storage_bytes(original_bytes: Vec<u8>, format: StorageFormat) -> Result<Self> {
        let mut cursor = Cursor::new(&original_bytes);
        let mut blocks = Vec::with_capacity(BLOCK_COUNT);
        for index in 0..BLOCK_COUNT {
            let start = cursor.position;
            let original = parse_block(&mut cursor, index, &format)?;
            blocks.push(BlockState {
                current: original.clone(),
                original,
                bytes: start..cursor.position,
            });
        }
        if cursor.position != original_bytes.len() {
            return Err(AppError::InvalidData(format!(
                "{} bytes remain after the 65,536 {} blocks",
                original_bytes.len() - cursor.position,
                format_name(&format),
            )));
        }
        Ok(Self {
            original_bytes,
            format,
            blocks,
            original_path: None,
            undo: Vec::new(),
            redo: Vec::new(),
        })
    }

    pub fn original_path(&self) -> Option<&Path> {
        self.original_path.as_deref()
    }

    pub const fn dimensions() -> (usize, usize) {
        (MAP_CELLS, MAP_CELLS)
    }

    pub fn block_type(&self, block_x: usize, block_y: usize) -> Option<EditableBlockType> {
        self.blocks
            .get(block_index(block_x, block_y)?)
            .map(|state| state.current.kind())
    }

    pub fn is_block_dirty(&self, block_x: usize, block_y: usize) -> bool {
        block_index(block_x, block_y)
            .and_then(|index| self.blocks.get(index))
            .is_some_and(BlockState::dirty)
    }

    pub fn changed_blocks(&self) -> usize {
        self.blocks.iter().filter(|state| state.dirty()).count()
    }

    pub fn cell(&self, address: LayerAddress) -> Option<Layer> {
        let (block, column) = cell_location(address.x, address.y)?;
        self.blocks.get(block)?.current.layer(column, address.layer)
    }

    pub fn layer_count(&self, x: usize, y: usize) -> Option<usize> {
        let (block, column) = cell_location(x, y)?;
        Some(self.blocks.get(block)?.current.layer_count(column))
    }

    /// Number of indexed layers exposed by the document. It is calculated
    /// from the actual columns rather than assuming the legacy 10-layer cap.
    pub fn max_layer_count(&self) -> usize {
        self.blocks
            .iter()
            .map(|state| state.current.max_layer_count())
            .max()
            .unwrap_or(0)
    }

    pub fn block(&self, block_x: usize, block_y: usize) -> Option<&EditableBlock> {
        Some(&self.blocks.get(block_index(block_x, block_y)?)?.current)
    }

    pub fn set_nswe(
        &mut self,
        targets: impl IntoIterator<Item = LayerAddress>,
        requested_mask: u8,
        label: impl Into<String>,
    ) -> EditResult {
        let mut before = BTreeMap::new();
        let mut result = EditResult::default();
        for target in targets {
            self.set_one_nswe(target, requested_mask & 0x0f, &mut before, &mut result);
        }
        self.commit(label.into(), before);
        result
    }

    /// Applies an explicit editor override. Unlike [`Self::set_nswe`], an open
    /// bit is kept even when there is no neighbour within the normal climb
    /// threshold (or at the edge of a map). When a neighbouring layer exists,
    /// its opposite bit is mirrored using the closest height. This is intended
    /// for manual corrections imported from existing geodata editors.
    pub fn force_set_nswe(
        &mut self,
        targets: impl IntoIterator<Item = LayerAddress>,
        requested_mask: u8,
        label: impl Into<String>,
    ) -> EditResult {
        let mut before = BTreeMap::new();
        let mut result = EditResult::default();
        for target in targets {
            self.force_one_nswe(target, requested_mask & 0x0f, &mut before, &mut result);
        }
        self.commit(label.into(), before);
        result
    }

    /// Changes the floor height of the explicitly addressed L2J layer.  The
    /// format only represents heights in eight-unit increments, so values
    /// entered between increments are normalized toward zero.  A single-cell
    /// edit of a simple block promotes it to complex, preserving the remaining
    /// 63 cells for later independent edits.
    pub fn set_height(
        &mut self,
        targets: impl IntoIterator<Item = LayerAddress>,
        requested_height: i32,
        label: impl Into<String>,
    ) -> Result<HeightEditResult> {
        let height = normalize_editable_height(requested_height)?;
        let mut before = BTreeMap::new();
        let mut result = HeightEditResult {
            height,
            ..HeightEditResult::default()
        };
        for target in targets {
            let Some((block, column)) = cell_location(target.x, target.y) else {
                result
                    .rejected_cells
                    .push(format!("outside grid: {},{}", target.x, target.y));
                continue;
            };
            let Some(previous) = self.blocks[block].current.layer(column, target.layer) else {
                result.rejected_cells.push(format!(
                    "layer {} does not exist at {},{}",
                    target.layer, target.x, target.y
                ));
                continue;
            };
            if previous.height == NULL_HEIGHT {
                result.rejected_cells.push(format!(
                    "layer {} at {},{} is a null-height sentinel",
                    target.layer, target.x, target.y
                ));
                continue;
            }
            if previous.height == height {
                continue;
            }
            self.snapshot(&mut before, block);
            if self.blocks[block].current.promote_to_complex() {
                result.promoted_blocks += 1;
            }
            if self.blocks[block]
                .current
                .set_layer_height(column, target.layer, height)
                .is_ok()
            {
                result.changed_cells += 1;
            }
        }
        self.commit(label.into(), before);
        Ok(result)
    }

    pub fn restore_block(&mut self, block_x: usize, block_y: usize) -> Result<bool> {
        let index = checked_block_index(block_x, block_y)?;
        if !self.blocks[index].dirty() {
            return Ok(false);
        }
        let mut before = BTreeMap::new();
        self.snapshot(&mut before, index);
        self.blocks[index].current = self.blocks[index].original.clone();
        self.commit("Restaurar bloco".into(), before);
        Ok(true)
    }

    pub fn convert_simple_to_complex(&mut self, x: usize, y: usize) -> Result<bool> {
        self.convert(x, y, "Simple → complex", |block| {
            Ok(block.promote_to_complex())
        })
    }

    pub fn convert_complex_to_multilayer(&mut self, x: usize, y: usize) -> Result<bool> {
        self.convert(x, y, "Complex → multilayer", |block| {
            if !matches!(block, EditableBlock::Complex(_)) {
                return Err(AppError::InvalidArgument(
                    "select a complex block to convert to multilayer".into(),
                ));
            }
            block.to_multilayer();
            Ok(true)
        })
    }

    pub fn convert_multilayer_to_complex(&mut self, x: usize, y: usize) -> Result<bool> {
        self.convert(x, y, "Multilayer → complex", |block| {
            block.collapse_to_complex()?;
            Ok(true)
        })
    }

    pub fn convert_to_simple(&mut self, x: usize, y: usize) -> Result<bool> {
        self.convert(x, y, "Converter em simple", |block| {
            block.collapse_to_simple()?;
            Ok(true)
        })
    }

    /// Converts a block directly to the type chosen by the editor. Each
    /// successful conversion is one undoable operation, even when a simple
    /// block needs to be promoted before becoming multilayer.
    pub fn convert_to_type(
        &mut self,
        x: usize,
        y: usize,
        target: EditableBlockType,
    ) -> Result<bool> {
        let label = match target {
            EditableBlockType::Simple => "Converter em simple",
            EditableBlockType::Complex => "Converter em complex",
            EditableBlockType::Multilayer => "Converter em multilayer",
        };
        self.convert(x, y, label, |block| {
            if block.kind() == target {
                return Ok(false);
            }
            match target {
                EditableBlockType::Simple => {
                    block.collapse_to_simple()?;
                }
                EditableBlockType::Complex => match block {
                    EditableBlock::Simple(_) => {
                        block.promote_to_complex();
                    }
                    EditableBlock::Multilayer { .. } => {
                        block.collapse_to_complex()?;
                    }
                    EditableBlock::Complex(_) => unreachable!("matching type returned above"),
                },
                EditableBlockType::Multilayer => {
                    block.promote_to_complex();
                    block.to_multilayer();
                }
            }
            Ok(true)
        })
    }

    pub fn undo(&mut self) -> bool {
        let Some(operation) = self.undo.pop() else {
            return false;
        };
        for (index, block) in &operation.before {
            self.blocks[*index].current = block.clone();
        }
        self.redo.push(operation);
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(operation) = self.redo.pop() else {
            return false;
        };
        for (index, block) in &operation.after {
            self.blocks[*index].current = block.clone();
        }
        self.undo.push(operation);
        true
    }

    pub fn undo_label(&self) -> Option<&str> {
        self.undo.last().map(|entry| entry.label.as_str())
    }

    pub fn redo_label(&self) -> Option<&str> {
        self.redo.last().map(|entry| entry.label.as_str())
    }

    pub fn validate(&self) -> Result<()> {
        if self.blocks.len() != BLOCK_COUNT {
            return Err(AppError::InvalidData("invalid L2J block count".into()));
        }
        for (index, state) in self.blocks.iter().enumerate() {
            state.current.validate(index)?;
        }
        Ok(())
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let body = self.block_bytes()?;
        encode_storage(&self.format, body, &self.blocks)
    }

    fn block_bytes(&self) -> Result<Vec<u8>> {
        let mut result = Vec::with_capacity(self.original_bytes.len());
        for state in &self.blocks {
            if state.dirty() {
                write_editable_block(&mut result, &state.current, &self.format)?;
            } else {
                result.extend_from_slice(&self.original_bytes[state.bytes.clone()]);
            }
        }
        Ok(result)
    }

    pub fn save_as(&self, destination: impl AsRef<Path>) -> Result<SaveSummary> {
        let destination = destination.as_ref();
        if !matches_destination(&self.format, destination) {
            return Err(AppError::InvalidArgument(format!(
                "Salvar como precisa manter o formato {}",
                format_name(&self.format),
            )));
        }
        let parent = destination.parent().unwrap_or_else(|| Path::new("."));
        if !parent.is_dir() {
            return Err(AppError::InvalidArgument(format!(
                "diretório de saída não existe: {}",
                parent.display()
            )));
        }
        let bytes = self.to_bytes()?;
        let extension = destination
            .extension()
            .and_then(|value| value.to_str())
            .ok_or_else(|| AppError::InvalidArgument("arquivo de destino sem extensão".into()))?;
        let temporary = destination.with_extension(format!("{extension}.editor.tmp"));
        let backup = destination.with_extension(format!("{extension}.bak"));
        let write_result = (|| -> Result<()> {
            let mut file = File::create(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            if destination.exists() {
                fs::copy(destination, &backup)?;
            }
            // `rename` cannot replace a file on Windows. The complete temp
            // and backup exist before `copy` replaces the final path.
            fs::copy(&temporary, destination)?;
            fs::remove_file(&temporary)?;
            Ok(())
        })();
        if let Err(error) = write_result {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        let (changed_cells, changed_directions) =
            self.blocks
                .iter()
                .fold((0, 0), |(cells, directions), state| {
                    let (next_cells, next_directions) =
                        block_change_stats(&state.original, &state.current);
                    (cells + next_cells, directions + next_directions)
                });
        Ok(SaveSummary {
            changed_blocks: self.changed_blocks(),
            conversion_blocks: self
                .blocks
                .iter()
                .filter(|state| state.dirty() && state.current.kind() != state.original.kind())
                .count(),
            changed_cells,
            changed_directions,
            bytes: bytes.len() as u64,
            path: destination.to_path_buf(),
        })
    }

    fn convert<F>(&mut self, x: usize, y: usize, label: &str, operation: F) -> Result<bool>
    where
        F: FnOnce(&mut EditableBlock) -> Result<bool>,
    {
        let index = checked_block_index(x, y)?;
        let mut before = BTreeMap::new();
        self.snapshot(&mut before, index);
        let changed = operation(&mut self.blocks[index].current)?;
        if changed {
            self.commit(label.into(), before);
        }
        Ok(changed)
    }

    fn set_one_nswe(
        &mut self,
        target: LayerAddress,
        requested: u8,
        before: &mut BTreeMap<usize, EditableBlock>,
        result: &mut EditResult,
    ) {
        let Some((target_block, target_column)) = cell_location(target.x, target.y) else {
            result
                .rejected_links
                .push(format!("outside grid: {},{}", target.x, target.y));
            return;
        };
        let Some(layer) = self.blocks[target_block]
            .current
            .layer(target_column, target.layer)
        else {
            result.rejected_links.push(format!(
                "layer {} does not exist at {},{}",
                target.layer, target.x, target.y
            ));
            return;
        };
        self.snapshot(before, target_block);
        if self.blocks[target_block].current.promote_to_complex() {
            result.promoted_blocks += 1;
        }
        let mut final_mask = requested;
        for direction in Direction::ALL {
            let wants_open = requested & direction.bit() != 0;
            let Some((nx, ny)) = neighbour(target.x, target.y, direction) else {
                final_mask &= !direction.bit();
                if wants_open {
                    result
                        .rejected_links
                        .push(format!("{} leaves the map", direction_name(direction)));
                }
                continue;
            };
            let (neighbour_block, neighbour_column) =
                cell_location(nx, ny).expect("neighbour was bounds checked");
            let compatible =
                self.find_compatible_layer(neighbour_block, neighbour_column, layer.height);
            if wants_open && compatible.is_none() {
                final_mask &= !direction.bit();
                result.rejected_links.push(format!(
                    "{} at {},{} has no neighbour within climb {}",
                    direction_name(direction),
                    target.x,
                    target.y,
                    MAX_WALKABLE_CLIMB
                ));
                continue;
            }
            let neighbour_layers = if wants_open {
                compatible.into_iter().collect::<Vec<_>>()
            } else if let Some(layer) = compatible {
                vec![layer]
            } else {
                // A malformed/old file can contain a one-way link to a layer
                // outside the modern climb threshold. Closing must still not
                // leave that opposite bit behind.
                (0..self.blocks[neighbour_block]
                    .current
                    .layer_count(neighbour_column))
                    .collect()
            };
            for neighbour_layer in neighbour_layers {
                let old = self.blocks[neighbour_block]
                    .current
                    .layer(neighbour_column, neighbour_layer)
                    .map(|layer| layer.nswe)
                    .unwrap_or(0);
                let new = if wants_open {
                    old | direction.opposite().bit()
                } else {
                    old & !direction.opposite().bit()
                };
                if old != new {
                    self.snapshot(before, neighbour_block);
                    if self.blocks[neighbour_block].current.promote_to_complex() {
                        result.promoted_blocks += 1;
                    }
                    if self.blocks[neighbour_block]
                        .current
                        .set_layer_nswe(neighbour_column, neighbour_layer, new)
                        .is_ok()
                    {
                        result.changed_links += 1;
                    }
                }
            }
        }
        let old = self.blocks[target_block]
            .current
            .layer(target_column, target.layer)
            .map(|layer| layer.nswe)
            .unwrap_or(0);
        if old != final_mask
            && self.blocks[target_block]
                .current
                .set_layer_nswe(target_column, target.layer, final_mask)
                .is_ok()
        {
            result.changed_cells += 1;
        }
    }

    fn force_one_nswe(
        &mut self,
        target: LayerAddress,
        requested: u8,
        before: &mut BTreeMap<usize, EditableBlock>,
        result: &mut EditResult,
    ) {
        let Some((target_block, target_column)) = cell_location(target.x, target.y) else {
            result
                .rejected_links
                .push(format!("outside grid: {},{}", target.x, target.y));
            return;
        };
        let Some(layer) = self.blocks[target_block]
            .current
            .layer(target_column, target.layer)
        else {
            result.rejected_links.push(format!(
                "layer {} does not exist at {},{}",
                target.layer, target.x, target.y
            ));
            return;
        };
        if layer.height == NULL_HEIGHT {
            result.rejected_links.push(format!(
                "layer {} at {},{} is a null-height sentinel",
                target.layer, target.x, target.y
            ));
            return;
        }

        let old = layer.nswe;
        if old != requested {
            self.snapshot(before, target_block);
            if self.blocks[target_block].current.promote_to_complex() {
                result.promoted_blocks += 1;
            }
            if self.blocks[target_block]
                .current
                .set_layer_nswe(target_column, target.layer, requested)
                .is_ok()
            {
                result.changed_cells += 1;
            }
        }

        for direction in Direction::ALL {
            let Some((nx, ny)) = neighbour(target.x, target.y, direction) else {
                // The explicit mask is intentionally preserved at map edges.
                continue;
            };
            let (neighbour_block, neighbour_column) =
                cell_location(nx, ny).expect("neighbour was bounds checked");
            let Some(neighbour_layer) =
                self.find_nearest_layer(neighbour_block, neighbour_column, layer.height)
            else {
                continue;
            };
            let old = self.blocks[neighbour_block]
                .current
                .layer(neighbour_column, neighbour_layer)
                .map(|layer| layer.nswe)
                .unwrap_or(0);
            let new = if requested & direction.bit() != 0 {
                old | direction.opposite().bit()
            } else {
                old & !direction.opposite().bit()
            };
            if old != new {
                self.snapshot(before, neighbour_block);
                if self.blocks[neighbour_block].current.promote_to_complex() {
                    result.promoted_blocks += 1;
                }
                if self.blocks[neighbour_block]
                    .current
                    .set_layer_nswe(neighbour_column, neighbour_layer, new)
                    .is_ok()
                {
                    result.changed_links += 1;
                }
            }
        }
    }

    fn find_compatible_layer(&self, block: usize, column: usize, height: i16) -> Option<usize> {
        self.find_nearest_layer(block, column, height)
            .filter(|index| {
                self.blocks[block]
                    .current
                    .layer(column, *index)
                    .is_some_and(|layer| {
                        i32::from(layer.height).abs_diff(i32::from(height))
                            <= MAX_WALKABLE_CLIMB as u32
                    })
            })
    }

    fn find_nearest_layer(&self, block: usize, column: usize, height: i16) -> Option<usize> {
        self.blocks[block]
            .current
            .layers(column)
            .iter()
            .enumerate()
            .filter(|(_, layer)| layer.height != NULL_HEIGHT)
            .map(|(index, layer)| {
                let delta = i32::from(layer.height).abs_diff(i32::from(height));
                (index, delta)
            })
            .min_by_key(|(_, delta)| *delta)
            .map(|(index, _)| index)
    }

    fn snapshot(&self, before: &mut BTreeMap<usize, EditableBlock>, index: usize) {
        before
            .entry(index)
            .or_insert_with(|| self.blocks[index].current.clone());
    }

    fn commit(&mut self, label: String, before: BTreeMap<usize, EditableBlock>) {
        let mut before = before.into_iter().collect::<Vec<_>>();
        before.retain(|(index, block)| self.blocks[*index].current != *block);
        if before.is_empty() {
            return;
        }
        let after = before
            .iter()
            .map(|(index, _)| (*index, self.blocks[*index].current.clone()))
            .collect();
        self.undo.push(EditOperation {
            label,
            before,
            after,
        });
        self.redo.clear();
    }
}

const L2G_CHECKSUM: i32 = -2_126_429_781;

fn decode_storage(path: &Path, bytes: Vec<u8>) -> Result<(StorageFormat, Vec<u8>)> {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| AppError::InvalidArgument("geodata file name is not valid Unicode".into()))?
        .to_ascii_lowercase();
    if name.ends_with("_conv.dat") {
        let header: [u8; 18] = bytes
            .get(..18)
            .ok_or_else(|| {
                AppError::InvalidData("Conv DAT header is shorter than 18 bytes".into())
            })?
            .try_into()
            .expect("18-byte slice has a fixed length");
        return Ok((StorageFormat::ConvDat { header }, bytes[18..].to_vec()));
    }
    match path.extension().and_then(|value| value.to_str()) {
        Some(extension) if extension.eq_ignore_ascii_case("l2j") => Ok((StorageFormat::L2j, bytes)),
        Some(extension) if extension.eq_ignore_ascii_case("l2g") => {
            let header: [u8; 4] = bytes
                .get(..4)
                .ok_or_else(|| AppError::InvalidData("L2G header is shorter than 4 bytes".into()))?
                .try_into()
                .expect("4-byte slice has a fixed length");
            let mut body = bytes[4..].to_vec();
            decrypt_l2g_body(header, &mut body);
            Ok((StorageFormat::L2g { header }, body))
        }
        _ => Err(AppError::InvalidArgument(
            "unsupported geodata format; expected .l2j, .l2g, or _conv.dat".into(),
        )),
    }
}

fn encode_storage(
    format: &StorageFormat,
    mut body: Vec<u8>,
    blocks: &[BlockState],
) -> Result<Vec<u8>> {
    match format {
        StorageFormat::L2j => Ok(body),
        StorageFormat::L2g { header } => {
            encrypt_l2g_body(*header, &mut body);
            let mut output = Vec::with_capacity(header.len() + body.len());
            output.extend_from_slice(header);
            output.extend_from_slice(&body);
            Ok(output)
        }
        StorageFormat::ConvDat { header } => {
            let mut output = if blocks.iter().any(BlockState::dirty) {
                conv_dat_header(*header, blocks)?
            } else {
                header.to_vec()
            };
            output.extend_from_slice(&body);
            Ok(output)
        }
    }
}

fn format_name(format: &StorageFormat) -> &'static str {
    match format {
        StorageFormat::L2j => ".l2j",
        StorageFormat::L2g { .. } => ".l2g",
        StorageFormat::ConvDat { .. } => "_conv.dat",
    }
}

fn matches_destination(format: &StorageFormat, path: &Path) -> bool {
    match format {
        StorageFormat::L2j => path
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.eq_ignore_ascii_case("l2j")),
        StorageFormat::L2g { .. } => path
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.eq_ignore_ascii_case("l2g")),
        StorageFormat::ConvDat { .. } => path
            .file_name()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.to_ascii_lowercase().ends_with("_conv.dat")),
    }
}

fn decrypt_l2g_body(header: [u8; 4], body: &mut [u8]) {
    let checksum = L2G_CHECKSUM ^ i32::from_be_bytes(header);
    if checksum == 0 {
        return;
    }
    let mut key = checksum
        .to_be_bytes()
        .into_iter()
        .fold(0, |key, byte| key ^ byte);
    for byte in body {
        let decrypted = *byte ^ key;
        *byte = decrypted;
        key = decrypted;
    }
}

fn encrypt_l2g_body(header: [u8; 4], body: &mut [u8]) {
    let checksum = L2G_CHECKSUM ^ i32::from_be_bytes(header);
    if checksum == 0 {
        return;
    }
    let mut key = checksum
        .to_be_bytes()
        .into_iter()
        .fold(0, |key, byte| key ^ byte);
    for byte in body {
        let plain = *byte;
        *byte = plain ^ key;
        key = plain;
    }
}

fn conv_dat_header(original: [u8; 18], blocks: &[BlockState]) -> Result<Vec<u8>> {
    let mut flat_blocks = 0_i32;
    let mut flat_and_complex_blocks = 0_i32;
    let mut cell_count = 0_i32;
    for state in blocks {
        match &state.current {
            EditableBlock::Simple(_) => {
                flat_blocks += 1;
                flat_and_complex_blocks += 1;
            }
            EditableBlock::Complex(_) => {
                flat_and_complex_blocks += 1;
                cell_count += COLUMNS_PER_BLOCK as i32;
            }
            EditableBlock::Multilayer { offsets, .. } => {
                cell_count += i32::from(offsets[COLUMNS_PER_BLOCK]);
            }
        }
    }
    let mut header = Vec::with_capacity(18);
    header.extend_from_slice(&original[..2]);
    write_i16(&mut header, 128);
    write_i16(&mut header, 16);
    write_i32(&mut header, cell_count);
    write_i32(&mut header, flat_and_complex_blocks);
    write_i32(&mut header, flat_blocks);
    Ok(header)
}
struct Cursor<'a> {
    data: &'a [u8],
    position: usize,
}
impl<'a> Cursor<'a> {
    const fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }
    fn read_u8(&mut self, block: usize) -> Result<u8> {
        let value = self.data.get(self.position).copied().ok_or_else(|| {
            AppError::InvalidData(format!(
                "unexpected end of L2J at block {block}, offset {}",
                self.position
            ))
        })?;
        self.position += 1;
        Ok(value)
    }
    fn read_i16(&mut self, block: usize) -> Result<i16> {
        Ok(i16::from_le_bytes([
            self.read_u8(block)?,
            self.read_u8(block)?,
        ]))
    }
}

fn parse_block(
    cursor: &mut Cursor<'_>,
    index: usize,
    format: &StorageFormat,
) -> Result<EditableBlock> {
    match format {
        StorageFormat::L2j | StorageFormat::L2g { .. } => parse_standard_block(cursor, index),
        StorageFormat::ConvDat { .. } => parse_conv_dat_block(cursor, index),
    }
}

fn parse_standard_block(cursor: &mut Cursor<'_>, index: usize) -> Result<EditableBlock> {
    match cursor.read_u8(index)? {
        0 => Ok(EditableBlock::Simple(Layer {
            height: cursor.read_i16(index)? & !0x000f,
            nswe: Layer::OPEN,
        })),
        1 => {
            let mut cells = [Layer::default(); COLUMNS_PER_BLOCK];
            for cell in &mut cells {
                *cell = unpack(cursor.read_i16(index)?);
            }
            Ok(EditableBlock::Complex(cells))
        }
        2 => read_multilayer_block(cursor, index, false),
        kind => Err(AppError::InvalidData(format!(
            "invalid L2J/L2G block type {kind} at block {index}"
        ))),
    }
}

fn parse_conv_dat_block(cursor: &mut Cursor<'_>, index: usize) -> Result<EditableBlock> {
    match cursor.read_i16(index)? {
        0 => {
            let height = cursor.read_i16(index)? & !0x000f;
            let _minimum_height = cursor.read_i16(index)?;
            Ok(EditableBlock::Simple(Layer {
                height,
                nswe: Layer::OPEN,
            }))
        }
        64 => {
            let mut cells = [Layer::default(); COLUMNS_PER_BLOCK];
            for cell in &mut cells {
                *cell = unpack(cursor.read_i16(index)?);
            }
            Ok(EditableBlock::Complex(cells))
        }
        _multilayer_type => read_multilayer_block(cursor, index, true),
    }
}

fn read_multilayer_block(
    cursor: &mut Cursor<'_>,
    index: usize,
    wide_counts: bool,
) -> Result<EditableBlock> {
    let mut offsets = [0u16; COLUMNS_PER_BLOCK + 1];
    let mut layers = Vec::with_capacity(COLUMNS_PER_BLOCK);
    for column in 0..COLUMNS_PER_BLOCK {
        let count = if wide_counts {
            usize::try_from(cursor.read_i16(index)?).map_err(|_| {
                AppError::InvalidData(format!(
                    "negative Conv DAT layer count in column {column} of block {index}"
                ))
            })?
        } else {
            cursor.read_u8(index)? as usize
        };
        if count == 0 {
            return Err(AppError::InvalidData(format!(
                "empty multilayer column {column} in block {index}"
            )));
        }
        for _ in 0..count {
            layers.push(unpack(cursor.read_i16(index)?));
        }
        offsets[column + 1] = u16::try_from(layers.len())
            .map_err(|_| AppError::InvalidData(format!("too many layers in block {index}")))?;
    }
    Ok(EditableBlock::Multilayer { offsets, layers })
}

fn unpack(raw: i16) -> Layer {
    Layer {
        height: (raw & !0x000f) >> 1,
        nswe: (raw & 0x000f) as u8,
    }
}

fn normalize_editable_height(value: i32) -> Result<i16> {
    let normalized = (value / i32::from(HEIGHT_STEP)) * i32::from(HEIGHT_STEP);
    if !(i32::from(MIN_EDITABLE_HEIGHT)..=i32::from(MAX_EDITABLE_HEIGHT)).contains(&normalized) {
        return Err(AppError::InvalidArgument(format!(
            "height must be between {MIN_EDITABLE_HEIGHT} and {MAX_EDITABLE_HEIGHT}"
        )));
    }
    Ok(normalized as i16)
}

fn write_editable_block(
    output: &mut Vec<u8>,
    block: &EditableBlock,
    format: &StorageFormat,
) -> Result<()> {
    block.validate(0)?;
    match format {
        StorageFormat::L2j | StorageFormat::L2g { .. } => write_standard_block(output, block),
        StorageFormat::ConvDat { .. } => write_conv_dat_block(output, block),
    }
}

fn write_standard_block(output: &mut Vec<u8>, block: &EditableBlock) -> Result<()> {
    match block {
        EditableBlock::Simple(layer) => {
            output.push(0);
            write_i16(output, layer.height & !0x000f);
        }
        EditableBlock::Complex(cells) => {
            output.push(1);
            for layer in cells {
                write_complex(output, *layer);
            }
        }
        EditableBlock::Multilayer { offsets, layers } => {
            output.push(2);
            for column in 0..COLUMNS_PER_BLOCK {
                let column = &layers[offsets[column] as usize..offsets[column + 1] as usize];
                output.push(u8::try_from(column.len()).map_err(|_| {
                    AppError::InvalidData("L2J/L2G column has more than 255 layers".into())
                })?);
                for layer in column {
                    write_complex(output, *layer);
                }
            }
        }
    }
    Ok(())
}

fn write_conv_dat_block(output: &mut Vec<u8>, block: &EditableBlock) -> Result<()> {
    match block {
        EditableBlock::Simple(layer) => {
            write_i16(output, 0);
            write_i16(output, layer.height & !0x000f);
            write_i16(output, layer.height & !0x000f);
        }
        EditableBlock::Complex(cells) => {
            write_i16(output, 64);
            for layer in cells {
                write_complex(output, *layer);
            }
        }
        EditableBlock::Multilayer { offsets, layers } => {
            let payload_start = output.len();
            output.extend_from_slice(&[0, 0]);
            for column in 0..COLUMNS_PER_BLOCK {
                let column = &layers[offsets[column] as usize..offsets[column + 1] as usize];

                write_i16(
                    output,
                    i16::try_from(column.len()).map_err(|_| {
                        AppError::InvalidData("Conv DAT column has more than 32,767 layers".into())
                    })?,
                );
                for layer in column {
                    write_complex(output, *layer);
                }
            }
            let payload_size = output.len() - payload_start - 2;
            let kind = 64
                + payload_size.checked_sub(128).ok_or_else(|| {
                    AppError::InvalidData("Conv DAT multilayer payload is too short".into())
                })?;
            let kind = i16::try_from(kind).map_err(|_| {
                AppError::InvalidData("Conv DAT multilayer payload is too large".into())
            })?;
            output[payload_start..payload_start + 2].copy_from_slice(&kind.to_le_bytes());
        }
    }
    Ok(())
}

fn block_index(x: usize, y: usize) -> Option<usize> {
    (x < BLOCKS_PER_AXIS && y < BLOCKS_PER_AXIS).then_some(y + x * BLOCKS_PER_AXIS)
}
fn checked_block_index(x: usize, y: usize) -> Result<usize> {
    block_index(x, y).ok_or_else(|| AppError::InvalidArgument(format!("invalid L2J block {x},{y}")))
}
fn cell_location(x: usize, y: usize) -> Option<(usize, usize)> {
    if x >= MAP_CELLS || y >= MAP_CELLS {
        return None;
    }
    Some((y / 8 + (x / 8) * BLOCKS_PER_AXIS, y % 8 + (x % 8) * 8))
}
fn neighbour(x: usize, y: usize, direction: Direction) -> Option<(usize, usize)> {
    let (dx, dy) = direction.offset();
    let (x, y) = (x.checked_add_signed(dx)?, y.checked_add_signed(dy)?);
    (x < MAP_CELLS && y < MAP_CELLS).then_some((x, y))
}
fn direction_name(direction: Direction) -> &'static str {
    match direction {
        Direction::North => "N",
        Direction::South => "S",
        Direction::West => "W",
        Direction::East => "E",
    }
}
fn write_i16(output: &mut Vec<u8>, value: i16) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn write_i32(output: &mut Vec<u8>, value: i32) {
    output.extend_from_slice(&value.to_le_bytes());
}
fn write_complex(output: &mut Vec<u8>, layer: Layer) {
    write_i16(output, (layer.height << 1) | i16::from(layer.nswe & 15));
}

fn block_change_stats(original: &EditableBlock, current: &EditableBlock) -> (usize, usize) {
    let mut changed_cells = 0;
    let mut changed_directions = 0;
    for column in 0..COLUMNS_PER_BLOCK {
        let before = original.layers(column);
        let after = current.layers(column);
        if before.len() != after.len() {
            changed_cells += before.len().max(after.len());
            changed_directions += before
                .iter()
                .map(|layer| layer.nswe.count_ones() as usize)
                .sum::<usize>();
            changed_directions += after
                .iter()
                .map(|layer| layer.nswe.count_ones() as usize)
                .sum::<usize>();
            continue;
        }
        for (before, after) in before.iter().zip(after) {
            if before != after {
                changed_cells += 1;
                changed_directions += (before.nswe ^ after.nswe).count_ones() as usize;
            }
        }
    }
    (changed_cells, changed_directions)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn simple_file(raw: i16) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(BLOCK_COUNT * 3);
        for _ in 0..BLOCK_COUNT {
            bytes.push(0);
            bytes.extend_from_slice(&raw.to_le_bytes());
        }
        bytes
    }
    fn first_complex_then_simple() -> Vec<u8> {
        let mut bytes = Vec::with_capacity(BLOCK_COUNT * 3 + 126);
        bytes.push(1);
        for column in 0..COLUMNS_PER_BLOCK {
            let height: i16 = if column == 8 { 100 } else { 0 };
            bytes.extend_from_slice(&((height << 1) | 15).to_le_bytes());
        }
        for _ in 1..BLOCK_COUNT {
            bytes.push(0);
            bytes.extend_from_slice(&0_i16.to_le_bytes());
        }
        bytes
    }
    #[test]
    fn simple_semantics_and_unedited_round_trip_are_lossless() {
        let bytes = simple_file(0x012f);
        let document = Document::from_bytes(bytes.clone()).unwrap();
        assert_eq!(
            document.cell(LayerAddress::new(0, 0, 0)),
            Some(Layer {
                height: 0x0120,
                nswe: 15
            })
        );
        assert_eq!(document.to_bytes().unwrap(), bytes);
    }
    #[test]
    fn rejects_truncated_invalid_and_empty_multilayer() {
        assert!(Document::from_bytes(vec![]).is_err());
        let mut invalid = simple_file(0);
        invalid[0] = 3;
        assert!(Document::from_bytes(invalid).is_err());
        let mut empty = simple_file(0);
        empty[0] = 2;
        empty[1] = 0;
        assert!(Document::from_bytes(empty).is_err());
    }
    #[test]
    fn editing_promotes_simple_and_undo_restores_original_bytes() {
        let bytes = simple_file(32);
        let mut document = Document::from_bytes(bytes.clone()).unwrap();
        let result = document.set_nswe([LayerAddress::new(0, 0, 0)], 0, "bloquear");
        assert_eq!(result.promoted_blocks, 1);
        assert_eq!(document.block_type(0, 0), Some(EditableBlockType::Complex));
        assert!(document.undo());
        assert_eq!(document.to_bytes().unwrap(), bytes);
        assert!(document.redo());
    }
    #[test]
    fn editing_height_promotes_simple_and_normalizes_to_l2j_steps() {
        let bytes = simple_file(32);
        let mut document = Document::from_bytes(bytes.clone()).unwrap();
        let target = LayerAddress::new(3, 4, 0);

        let result = document.set_height([target], -3315, "altura").unwrap();

        assert_eq!(result.height, -3312);
        assert_eq!(result.changed_cells, 1);
        assert_eq!(result.promoted_blocks, 1);
        assert_eq!(document.block_type(0, 0), Some(EditableBlockType::Complex));
        assert_eq!(document.cell(target).unwrap().height, -3312);
        assert_eq!(
            document.cell(LayerAddress::new(0, 0, 0)).unwrap().height,
            32
        );
        assert!(document.undo());
        assert_eq!(document.to_bytes().unwrap(), bytes);
        assert!(
            document
                .set_height([target], NULL_HEIGHT as i32, "sentinela")
                .is_err()
        );
    }
    #[test]
    fn nswe_is_symmetric_across_blocks() {
        let mut document = Document::from_bytes(simple_file(32)).unwrap();
        document.set_nswe([LayerAddress::new(7, 0, 0)], Layer::OPEN, "abrir");
        assert_ne!(
            document.cell(LayerAddress::new(7, 0, 0)).unwrap().nswe & 1,
            0
        );
        assert_ne!(
            document.cell(LayerAddress::new(8, 0, 0)).unwrap().nswe & 2,
            0
        );
    }
    #[test]
    fn opening_a_link_rejects_a_neighbour_above_the_climb_limit() {
        let mut document = Document::from_bytes(first_complex_then_simple()).unwrap();
        let result = document.set_nswe(
            [LayerAddress::new(0, 0, 0)],
            Direction::East.bit(),
            "abrir leste",
        );
        assert!(!result.rejected_links.is_empty());
        assert_eq!(
            document.cell(LayerAddress::new(0, 0, 0)).unwrap().nswe & Direction::East.bit(),
            0
        );
    }
    #[test]
    fn explicit_open_override_keeps_all_four_sides_and_mirrors_nearest_layer() {
        let mut document = Document::from_bytes(first_complex_then_simple()).unwrap();
        let target = LayerAddress::new(0, 0, 0);
        document.set_nswe([target], 0, "bloquear");
        let result = document.force_set_nswe([target], Layer::OPEN, "liberar 100%");

        assert_eq!(document.cell(target).unwrap().nswe, Layer::OPEN);
        assert_ne!(result.changed_cells, 0);
        // The east neighbour is 100 units above and is deliberately outside
        // the normal climb threshold. The explicit editor action still
        // mirrors it, making the authored override symmetric.
        assert_ne!(
            document.cell(LayerAddress::new(1, 0, 0)).unwrap().nswe & Direction::West.bit(),
            0
        );
    }
    #[test]
    fn save_as_can_replace_the_opened_file_after_creating_a_backup() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock is after Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "geodata-editor-overwrite-{}-{nonce}.l2j",
            std::process::id()
        ));
        let backup = path.with_extension("l2j.bak");
        let original = simple_file(32);
        fs::write(&path, &original).expect("write source L2J");

        let mut document = Document::open(&path).expect("open source L2J");
        document.set_nswe([LayerAddress::new(0, 0, 0)], 0, "bloquear");
        document.save_as(&path).expect("replace opened L2J");

        assert_eq!(fs::read(&backup).expect("read backup"), original);
        assert_eq!(
            Document::open(&path)
                .expect("reopen replacement")
                .cell(LayerAddress::new(0, 0, 0))
                .unwrap()
                .nswe,
            0
        );
        fs::remove_file(path).expect("remove temporary replacement");
        fs::remove_file(backup).expect("remove temporary backup");
    }
    #[test]
    fn conversions_and_restore_are_checked() {
        let mut document = Document::from_bytes(simple_file(32)).unwrap();
        assert!(document.convert_simple_to_complex(0, 0).unwrap());
        assert!(document.convert_complex_to_multilayer(0, 0).unwrap());
        assert!(document.convert_multilayer_to_complex(0, 0).unwrap());
        assert!(document.convert_to_simple(0, 0).unwrap());
        document.set_nswe([LayerAddress::new(0, 0, 0)], 0, "bloquear");
        assert!(document.restore_block(0, 0).unwrap());
        assert_eq!(document.changed_blocks(), 0);

        assert!(
            document
                .convert_to_type(0, 0, EditableBlockType::Multilayer)
                .unwrap()
        );
        assert_eq!(
            document.block_type(0, 0),
            Some(EditableBlockType::Multilayer)
        );
        assert!(document.undo());
        assert_eq!(document.block_type(0, 0), Some(EditableBlockType::Simple));
    }
    fn l2g_file(body: Vec<u8>) -> Vec<u8> {
        let header = [0x12, 0x34, 0x56, 0x78];
        let mut encrypted = body;
        encrypt_l2g_body(header, &mut encrypted);
        let mut file = Vec::with_capacity(header.len() + encrypted.len());
        file.extend_from_slice(&header);
        file.extend_from_slice(&encrypted);
        file
    }

    fn conv_dat_file(height: i16) -> Vec<u8> {
        let mut file = Vec::with_capacity(18 + BLOCK_COUNT * 6);
        file.extend_from_slice(&[20, 20]);
        write_i16(&mut file, 128);
        write_i16(&mut file, 16);
        write_i32(&mut file, 0);
        write_i32(&mut file, BLOCK_COUNT as i32);
        write_i32(&mut file, BLOCK_COUNT as i32);
        for _ in 0..BLOCK_COUNT {
            write_i16(&mut file, 0);
            write_i16(&mut file, height);
            write_i16(&mut file, height);
        }
        file
    }

    fn temporary_geodata_path(name: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock is after Unix epoch")
            .as_nanos();
        let source = Path::new(name);
        if let Some(prefix) = name.strip_suffix("_conv.dat") {
            return std::env::temp_dir()
                .join(format!("{prefix}-{}-{nonce}_conv.dat", std::process::id()));
        }
        let stem = source
            .file_stem()
            .and_then(|value| value.to_str())
            .expect("test file name has a valid stem");
        let extension = source
            .extension()
            .and_then(|value| value.to_str())
            .expect("test file name has an extension");
        std::env::temp_dir().join(format!("{stem}-{}-{nonce}.{extension}", std::process::id()))
    }

    #[test]
    fn opens_edits_and_reencrypts_l2g() {
        let path = temporary_geodata_path("geodata-editor.l2g");
        let backup = path.with_extension("l2g.bak");
        let original = l2g_file(simple_file(32));
        fs::write(&path, &original).expect("write L2G");

        let mut document = Document::open(&path).expect("open L2G");
        assert_eq!(
            document.cell(LayerAddress::new(0, 0, 0)).unwrap().height,
            32
        );
        assert_eq!(document.to_bytes().expect("round-trip L2G"), original);
        document.set_nswe([LayerAddress::new(0, 0, 0)], 0, "bloquear");
        document.save_as(&path).expect("save L2G");

        assert_eq!(fs::read(&backup).expect("read L2G backup"), original);
        assert_eq!(
            Document::open(&path)
                .expect("reopen saved L2G")
                .cell(LayerAddress::new(0, 0, 0))
                .unwrap()
                .nswe,
            0
        );
        fs::remove_file(path).expect("remove temporary L2G");
        fs::remove_file(backup).expect("remove L2G backup");
    }

    #[test]
    fn opens_edits_and_rewrites_conv_dat_header() {
        let path = temporary_geodata_path("20_20_conv.dat");
        let backup = path.with_extension("dat.bak");
        let original = conv_dat_file(32);
        fs::write(&path, &original).expect("write Conv DAT");

        let mut document = Document::open(&path).expect("open Conv DAT");
        assert_eq!(
            document.cell(LayerAddress::new(0, 0, 0)).unwrap().height,
            32
        );
        assert_eq!(document.to_bytes().expect("round-trip Conv DAT"), original);
        document.set_nswe([LayerAddress::new(0, 0, 0)], 0, "bloquear");
        document.save_as(&path).expect("save Conv DAT");

        let saved = fs::read(&path).expect("read saved Conv DAT");
        assert_eq!(fs::read(&backup).expect("read Conv DAT backup"), original);
        assert_eq!(i32::from_le_bytes(saved[6..10].try_into().unwrap()), 64);
        assert_eq!(
            i32::from_le_bytes(saved[10..14].try_into().unwrap()),
            BLOCK_COUNT as i32
        );
        assert_eq!(
            i32::from_le_bytes(saved[14..18].try_into().unwrap()),
            BLOCK_COUNT as i32 - 1
        );
        assert_eq!(
            Document::open(&path)
                .expect("reopen saved Conv DAT")
                .cell(LayerAddress::new(0, 0, 0))
                .unwrap()
                .nswe,
            0
        );
        fs::remove_file(path).expect("remove temporary Conv DAT");
        fs::remove_file(backup).expect("remove Conv DAT backup");
    }
}
