//! Unreal package reader used to recover collision triangles for the editor.

use std::{
    cell::RefCell,
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    rc::Rc,
};

use crate::{
    error::{AppError, Result},
    geometry::{Box3, Transform, Triangle, Vec3},
};

const PACKAGE_MAGIC: i32 = -1_641_380_927; // 0x9e2a83c1
const RF_HAS_STACK: u32 = 0x0200_0000;
const RF_NATIVE: u32 = 0x0400_0000;
const NF_PASSABLE: u8 = 0x01;
const PF_PASSABLE: u32 = 0x0400_00df;

#[derive(Clone, Debug)]
pub struct SourceMap {
    pub name: String,
    /// Recast's X/Y/Z coordinates: Lineage X/Z/Y.
    pub bounds: Box3,
    pub triangles: Vec<Triangle>,
    pub geometry: GeometryStats,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct GeometryStats {
    pub terrain_triangles: usize,
    pub static_mesh_triangles: usize,
    pub bsp_triangles: usize,
    pub blocking_volume_triangles: usize,
}

#[derive(Clone, Copy)]
enum GeometrySource {
    Terrain,
    StaticMesh,
    Bsp,
    BlockingVolume,
}

impl SourceMap {
    fn new(name: &str, world_bounds: Box3) -> Self {
        Self {
            name: name.to_owned(),
            bounds: world_bounds.swap_yz(),
            triangles: Vec::new(),
            geometry: GeometryStats::default(),
        }
    }

    fn add_mesh(&mut self, mesh: &Mesh, transform: Transform, source: GeometrySource) {
        let before = self.triangles.len();
        for indices in mesh.indices.chunks_exact(3) {
            let a = mesh.vertices[indices[0] as usize];
            let b = mesh.vertices[indices[1] as usize];
            let c = mesh.vertices[indices[2] as usize];
            let a_position = transform.point(a.position).swap_yz();
            let b_position = transform.point(b.position).swap_yz();
            let c_position = transform.point(c.position).swap_yz();
            let average_normal = (transform.normal(a.normal)
                + transform.normal(b.normal)
                + transform.normal(c.normal))
            .normalize_or_zero()
            .swap_yz();
            // This is GLM's triangleNormal(v2, v1, v0), used by Current to
            // canonicalise winding before handing the triangles to Recast.
            let face_normal = (b_position - c_position)
                .cross(a_position - c_position)
                .normalize_or_zero();
            if average_normal.dot(face_normal) >= 0.0 {
                self.triangles.push(Triangle {
                    a: c_position,
                    b: b_position,
                    c: a_position,
                });
            } else {
                self.triangles.push(Triangle {
                    a: a_position,
                    b: b_position,
                    c: c_position,
                });
            }
        }
        let added = self.triangles.len() - before;
        match source {
            GeometrySource::Terrain => self.geometry.terrain_triangles += added,
            GeometrySource::StaticMesh => self.geometry.static_mesh_triangles += added,
            GeometrySource::Bsp => self.geometry.bsp_triangles += added,
            GeometrySource::BlockingVolume => self.geometry.blocking_volume_triangles += added,
        }
    }
}

pub struct PackageLoader {
    root: PathBuf,
    archives: RefCell<HashMap<String, Rc<Archive>>>,
    loaded_package_count: RefCell<usize>,
    log_level: u8,
    verbose: bool,
}

impl PackageLoader {
    pub fn new(root: PathBuf, log_level: u8, verbose: bool) -> Self {
        Self {
            root,
            archives: RefCell::new(HashMap::new()),
            loaded_package_count: RefCell::new(0),
            log_level,
            verbose,
        }
    }

    pub fn info(&self, component: &str, message: impl AsRef<str>) {
        if self.verbose && self.log_level >= 4 {
            println!("[{component}] {}", message.as_ref());
        }
    }

    pub fn loaded_package_count(&self) -> usize {
        *self.loaded_package_count.borrow()
    }

    fn warn(&self, component: &str, message: impl AsRef<str>) {
        if self.verbose && self.log_level >= 3 {
            eprintln!("[{component}] {}", message.as_ref());
        }
    }

    fn archive(&self, name: &str) -> Result<Rc<Archive>> {
        if let Some(archive) = self.archives.borrow().get(name) {
            return Ok(Rc::clone(archive));
        }
        let candidates = [
            ("Maps", "unr"),
            ("StaticMeshes", "usx"),
            ("Textures", "utx"),
            ("SysTextures", "utx"),
        ];
        let path = candidates
            .iter()
            .map(|(directory, extension)| {
                self.root
                    .join(directory)
                    .join(format!("{name}.{extension}"))
            })
            .find(|path| path.is_file())
            .ok_or_else(|| AppError::Missing(format!("can't find package: {name}")))?;
        self.info("Unreal", format!("Loading package: {name}"));
        let data = decrypt(&path)?;
        let archive = Rc::new(Archive::parse(name, data)?);
        self.info(
            "Unreal",
            format!(
                "Package loaded: {name} (file: {}, license: {})",
                archive.header.file_version, archive.header.license_version
            ),
        );
        self.archives
            .borrow_mut()
            .insert(name.to_owned(), Rc::clone(&archive));
        *self.loaded_package_count.borrow_mut() += 1;
        Ok(archive)
    }

    fn try_archive(&self, name: &str) -> Result<Option<Rc<Archive>>> {
        match self.archive(name) {
            Ok(archive) => Ok(Some(archive)),
            Err(AppError::Missing(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub fn load_map(&self, name: &str) -> Result<SourceMap> {
        let archive = self.archive(name)?;
        let terrain = archive
            .objects_named("TerrainInfo", self)?
            .into_iter()
            .find_map(|object| match object.as_ref() {
                Object::Terrain(value) => Some(value.clone()),
                _ => None,
            })
            .ok_or_else(|| AppError::InvalidData(format!("no TerrainInfo in package: {name}")))?;
        let terrain_texture = self.texture_ref(&archive, terrain.map_index, "TerrainMap")?;
        if terrain_texture.mips.is_empty() {
            return Err(AppError::InvalidData(format!(
                "can't load terrain heightmap in package: {name}"
            )));
        }
        let terrain_scale = terrain.scale();
        let terrain_position = terrain.position(&terrain_texture);
        let world_bounds = terrain.bounds(&terrain_texture);
        let mut map = SourceMap::new(name, world_bounds);

        if !terrain.broken_scale() {
            let south = self.side_terrain(terrain.map_x, terrain.map_y + 1)?;
            let east = self.side_terrain(terrain.map_x + 1, terrain.map_y)?;
            let southeast = self.side_terrain(terrain.map_x + 1, terrain.map_y + 1)?;
            let mesh = terrain_mesh(
                &terrain,
                &terrain_texture,
                terrain_position,
                terrain_scale,
                south,
                east,
                southeast,
            )?;
            map.add_mesh(&mesh, Transform::default(), GeometrySource::Terrain);
        }

        for class_name in [
            "StaticMeshActor",
            "MovableStaticMeshActor",
            "L2MovableStaticMeshActor",
        ] {
            for object in archive.objects_named(class_name, self)? {
                let Object::Actor(actor) = object.as_ref() else {
                    continue;
                };
                if actor.delete_me || actor.hidden {
                    continue;
                }
                if !actor.collide_actors || !actor.block_actors || !actor.block_players {
                    continue;
                }
                let Some(mesh_index) = actor.static_mesh else {
                    self.warn("App", format!("No static mesh for actor: {name}"));
                    continue;
                };
                let mesh = match self.static_mesh_ref(&archive, mesh_index) {
                    Ok(mesh) => mesh,
                    Err(AppError::Missing(_)) => {
                        self.warn("App", format!("No static mesh for actor: {name}"));
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                let collision_mesh = mesh.collision_mesh()?;
                map.add_mesh(
                    &collision_mesh,
                    actor.transform(),
                    GeometrySource::StaticMesh,
                );
            }
        }

        for object in archive.objects_named("Level", self)? {
            let Object::Level(level) = object.as_ref() else {
                continue;
            };
            let model = self.model_ref(&archive, level.model_index, "Level.Model")?;
            if let Some(mesh) = model.mesh(Some(world_bounds))? {
                map.add_mesh(&mesh, Transform::default(), GeometrySource::Bsp);
            }
        }

        for object in archive.objects_named("BlockingVolume", self)? {
            let Object::Brush(actor) = object.as_ref() else {
                continue;
            };
            let Some(brush_index) = actor.brush else {
                continue;
            };
            let model = self.model_ref(&archive, brush_index, "BlockingVolume.Brush")?;
            if let Some(mesh) = model.mesh(None)? {
                map.add_mesh(&mesh, actor.transform(), GeometrySource::BlockingVolume);
            }
        }
        Ok(map)
    }

    fn side_terrain(&self, x: i32, y: i32) -> Result<Option<(Terrain, Texture)>> {
        let Some(archive) = self.try_archive(&format!("{x}_{y}"))? else {
            return Ok(None);
        };
        let Some(terrain) = archive
            .objects_named("TerrainInfo", self)?
            .into_iter()
            .find_map(|object| match object.as_ref() {
                Object::Terrain(value) => Some(value.clone()),
                _ => None,
            })
        else {
            return Ok(None);
        };
        if terrain.broken_scale() {
            return Ok(None);
        }
        let texture = self.texture_ref(&archive, terrain.map_index, "TerrainMap")?;
        Ok(Some((terrain, texture)))
    }

    fn object_ref(&self, archive: &Rc<Archive>, index: i32, context: &str) -> Result<Rc<Object>> {
        if index == 0 {
            return Err(AppError::Missing(format!(
                "required object reference is empty: {context}"
            )));
        }
        archive.object_at(index, self)
    }

    fn texture_ref(&self, archive: &Rc<Archive>, index: i32, context: &str) -> Result<Texture> {
        match self.object_ref(archive, index, context)?.as_ref() {
            Object::Texture(texture) => Ok(texture.clone()),
            other => Err(AppError::InvalidData(format!(
                "{context} is {}, not Texture",
                other.kind()
            ))),
        }
    }
    fn static_mesh_ref(&self, archive: &Rc<Archive>, index: i32) -> Result<StaticMesh> {
        match self
            .object_ref(archive, index, "Actor.StaticMesh")?
            .as_ref()
        {
            Object::StaticMesh(mesh) => Ok(mesh.clone()),
            other => Err(AppError::InvalidData(format!(
                "Actor.StaticMesh is {}, not StaticMesh",
                other.kind()
            ))),
        }
    }
    fn model_ref(&self, archive: &Rc<Archive>, index: i32, context: &str) -> Result<Model> {
        match self.object_ref(archive, index, context)?.as_ref() {
            Object::Model(model) => Ok(model.clone()),
            other => Err(AppError::InvalidData(format!(
                "{context} is {}, not Model",
                other.kind()
            ))),
        }
    }
}

fn decrypt(path: &Path) -> Result<Vec<u8>> {
    let bytes = fs::read(path)?;
    const HEADER: &[u8] = b"L\x00i\x00n\x00e\x00a\x00g\x00e\x002\x00V\x00e\x00r\x00";
    if bytes.len() < 28 || &bytes[..22] != HEADER {
        return Err(AppError::InvalidData(format!(
            "can't detect Lineage 2 encryption version: {}",
            path.display()
        )));
    }
    let version = std::str::from_utf8(&[bytes[22], bytes[24], bytes[26]])
        .map_err(|_| {
            AppError::InvalidData(format!("invalid encryption version: {}", path.display()))
        })?
        .parse::<u16>()
        .map_err(|_| {
            AppError::InvalidData(format!("invalid encryption version: {}", path.display()))
        })?;
    let key = match version {
        111 => 0xac,
        121 => path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                AppError::InvalidData(format!("non-Unicode package filename: {}", path.display()))
            })?
            .bytes()
            .map(|byte| byte.to_ascii_lowercase() as u32)
            .sum::<u32>() as u8,
        _ => {
            return Err(AppError::Unsupported(format!(
                "unsupported Lineage 2 encryption version: {version}"
            )));
        }
    };
    Ok(bytes[28..].iter().map(|byte| byte ^ key).collect())
}

#[derive(Clone, Debug)]
struct Header {
    file_version: i16,
    license_version: i16,
}
#[derive(Clone, Debug)]
struct Import {
    class_name: String,
    package_index: i32,
    object_name: String,
}
#[derive(Clone, Debug)]
struct Export {
    class_name: String,
    object_name: String,
    flags: u32,
    serial_size: i32,
    serial_offset: i32,
}

struct Archive {
    name: String,
    data: Vec<u8>,
    header: Header,
    names: Vec<String>,
    imports: Vec<Import>,
    exports: Vec<Export>,
    objects: RefCell<Vec<Option<Rc<Object>>>>,
}

impl Archive {
    fn parse(name: &str, data: Vec<u8>) -> Result<Self> {
        let mut reader = Reader::new(&data, 0);
        if reader.i32()? != PACKAGE_MAGIC {
            return Err(AppError::InvalidData(format!(
                "invalid Lineage II package magic: {name}"
            )));
        }
        let header = Header {
            file_version: reader.i16()?,
            license_version: reader.i16()?,
        };
        let _flags = reader.u32()?;
        let name_count = reader.i32()?;
        let name_offset = reader.i32()?;
        let export_count = reader.i32()?;
        let export_offset = reader.i32()?;
        let import_count = reader.i32()?;
        let import_offset = reader.i32()?;
        reader.skip(16)?;
        let generations = reader.i32()?;
        if generations < 0 {
            return Err(AppError::InvalidData(
                "negative package generation count".into(),
            ));
        }
        reader.skip(generations as usize * 8)?;
        check_count("name", name_count)?;
        check_count("import", import_count)?;
        check_count("export", export_count)?;
        let mut name_reader = Reader::new(&data, checked_offset(name_offset, data.len())?);
        let mut names = Vec::with_capacity(name_count as usize);
        for _ in 0..name_count {
            names.push(name_reader.string()?);
            name_reader.u32()?;
        }
        let mut import_reader = Reader::new(&data, checked_offset(import_offset, data.len())?);
        let mut imports = Vec::with_capacity(import_count as usize);
        for _ in 0..import_count {
            let _class_package = name_at(&names, import_reader.index()?)?.to_owned();
            let class_name = name_at(&names, import_reader.index()?)?.to_owned();
            let package_index = import_reader.i32()?;
            let object_name = name_at(&names, import_reader.index()?)?.to_owned();
            imports.push(Import {
                class_name,
                package_index,
                object_name,
            });
        }
        let mut export_reader = Reader::new(&data, checked_offset(export_offset, data.len())?);
        let mut exports = Vec::with_capacity(export_count as usize);
        for _ in 0..export_count {
            let class_index = export_reader.index()?;
            let _super_index = export_reader.index()?;
            let _package_index = export_reader.i32()?;
            let object_name = name_at(&names, export_reader.index()?)?.to_owned();
            let flags = export_reader.u32()?;
            let serial_size = export_reader.index()?;
            let serial_offset = if serial_size > 0 {
                export_reader.index()?
            } else {
                0
            };
            let class_name = object_name_for(&names, &imports, &exports, class_index)?.to_owned();
            exports.push(Export {
                class_name,
                object_name,
                flags,
                serial_size,
                serial_offset,
            });
        }
        Ok(Self {
            name: name.to_owned(),
            data,
            header,
            names,
            imports,
            exports,
            objects: RefCell::new(vec![None; export_count as usize]),
        })
    }

    fn objects_named(&self, class_name: &str, loader: &PackageLoader) -> Result<Vec<Rc<Object>>> {
        self.exports
            .iter()
            .enumerate()
            .filter(|(_, export)| export.class_name == class_name)
            .map(|(index, _)| self.load_export(index, loader))
            .collect()
    }

    fn object_at(&self, index: i32, loader: &PackageLoader) -> Result<Rc<Object>> {
        if index > 0 {
            return self.load_export((index - 1) as usize, loader);
        }
        let import_index = (-index - 1) as usize;
        let import = self.imports.get(import_index).ok_or_else(|| {
            AppError::InvalidData(format!("import index out of bounds in {}", self.name))
        })?;
        let mut root = import;
        while root.package_index != 0 {
            let parent = (-root.package_index - 1) as usize;
            root = self.imports.get(parent).ok_or_else(|| {
                AppError::InvalidData(format!(
                    "package import index out of bounds in {}",
                    self.name
                ))
            })?;
        }
        let archive = loader.archive(&root.object_name)?;
        archive
            .exports
            .iter()
            .enumerate()
            .find(|(_, export)| {
                export.object_name == import.object_name
                    && export.class_name == import.class_name
                    && export.class_name != "Package"
            })
            .map(|(index, _)| archive.load_export(index, loader))
            .transpose()?
            .ok_or_else(|| {
                AppError::Missing(format!(
                    "can't find object {}.{}",
                    root.object_name, import.object_name
                ))
            })
    }

    fn load_export(&self, index: usize, loader: &PackageLoader) -> Result<Rc<Object>> {
        if let Some(object) = self.objects.borrow().get(index).and_then(Clone::clone) {
            return Ok(object);
        }
        let export = self
            .exports
            .get(index)
            .ok_or_else(|| {
                AppError::InvalidData(format!("export index out of bounds in {}", self.name))
            })?
            .clone();
        if export.serial_size <= 0 {
            return Ok(Rc::new(Object::Unknown));
        }
        let start = checked_offset(export.serial_offset, self.data.len())?;
        let end = start
            .checked_add(export.serial_size as usize)
            .filter(|end| *end <= self.data.len())
            .ok_or_else(|| {
                AppError::InvalidData(format!(
                    "invalid serial range for {}.{}",
                    self.name, export.object_name
                ))
            })?;
        let mut reader = Reader::new(&self.data[..end], start);
        let object = Rc::new(self.parse_object(&export, &mut reader, loader)?);
        self.objects.borrow_mut()[index] = Some(Rc::clone(&object));
        Ok(object)
    }

    fn parse_object(
        &self,
        export: &Export,
        reader: &mut Reader<'_>,
        _loader: &PackageLoader,
    ) -> Result<Object> {
        match export.class_name.as_str() {
            "TerrainInfo" => Ok(Object::Terrain(Terrain::read(self, reader, export.flags)?)),
            "Texture" => Ok(Object::Texture(Texture::read(self, reader, export.flags)?)),
            "StaticMesh" => Ok(Object::StaticMesh(StaticMesh::read(
                self,
                reader,
                export.flags,
            )?)),
            "StaticMeshActor" | "MovableStaticMeshActor" | "L2MovableStaticMeshActor" => {
                Ok(Object::Actor(Actor::read(self, reader, export.flags)?))
            }
            "Level" => Ok(Object::Level(Level::read(self, reader, export.flags)?)),
            "Model" => Ok(Object::Model(Model::read(self, reader, export.flags)?)),
            "Brush" | "BlockingVolume" => {
                Ok(Object::Brush(Brush::read(self, reader, export.flags)?))
            }
            _ => Ok(Object::Unknown),
        }
    }
}

fn check_count(label: &str, count: i32) -> Result<()> {
    if !(0..=10_000_000).contains(&count) {
        Err(AppError::InvalidData(format!(
            "invalid {label} count: {count}"
        )))
    } else {
        Ok(())
    }
}
fn checked_offset(offset: i32, limit: usize) -> Result<usize> {
    let offset = usize::try_from(offset)
        .map_err(|_| AppError::InvalidData("negative package offset".into()))?;
    if offset > limit {
        Err(AppError::InvalidData("package offset beyond EOF".into()))
    } else {
        Ok(offset)
    }
}
fn name_at(names: &[String], index: i32) -> Result<&str> {
    names
        .get(
            usize::try_from(index)
                .map_err(|_| AppError::InvalidData("negative name index".into()))?,
        )
        .map(String::as_str)
        .ok_or_else(|| AppError::InvalidData(format!("name index out of bounds: {index}")))
}
fn object_name_for<'a>(
    names: &'a [String],
    imports: &'a [Import],
    exports: &'a [Export],
    index: i32,
) -> Result<&'a str> {
    if index < 0 {
        return imports
            .get((-index - 1) as usize)
            .map(|item| item.object_name.as_str())
            .ok_or_else(|| AppError::InvalidData("class import index out of bounds".into()));
    }
    if index > 0 {
        return exports
            .get((index - 1) as usize)
            .map(|item| item.object_name.as_str())
            .ok_or_else(|| AppError::InvalidData("class export index out of bounds".into()));
    }
    let _ = names;
    Ok("None")
}

#[derive(Clone)]
enum Object {
    Unknown,
    Terrain(Terrain),
    Texture(Texture),
    StaticMesh(StaticMesh),
    Actor(Actor),
    Level(Level),
    Model(Model),
    Brush(Brush),
}
impl Object {
    fn kind(&self) -> &'static str {
        match self {
            Self::Unknown => "unknown object",
            Self::Terrain(_) => "TerrainInfo",
            Self::Texture(_) => "Texture",
            Self::StaticMesh(_) => "StaticMesh",
            Self::Actor(_) => "StaticMeshActor",
            Self::Level(_) => "Level",
            Self::Model(_) => "Model",
            Self::Brush(_) => "Brush",
        }
    }
}

#[derive(Clone, Copy)]
struct Vertex {
    position: Vec3,
    normal: Vec3,
}
#[derive(Clone, Default)]
struct Mesh {
    vertices: Vec<Vertex>,
    indices: Vec<u32>,
}

#[derive(Clone, Copy, Default)]
struct Rotator {
    pitch: i32,
    yaw: i32,
    roll: i32,
}
impl Rotator {
    fn vector(self) -> Vec3 {
        Vec3::new(
            -(self.roll as f32) * std::f32::consts::PI / 32768.0,
            -(self.pitch as f32) * std::f32::consts::PI / 32768.0,
            (self.yaw as f32) * std::f32::consts::PI / 32768.0,
        )
    }
}

#[derive(Clone)]
struct Actor {
    location: Vec3,
    rotation: Rotator,
    draw_scale: f32,
    draw_scale_3d: Vec3,
    pre_pivot: Vec3,
    static_mesh: Option<i32>,
    delete_me: bool,
    hidden: bool,
    collide_actors: bool,
    block_actors: bool,
    block_players: bool,
}
impl Actor {
    fn read(archive: &Archive, reader: &mut Reader<'_>, flags: u32) -> Result<Self> {
        let props = read_properties(archive, reader, flags)?;
        Ok(Self::from_properties(&props))
    }
    fn from_properties(props: &Properties) -> Self {
        Self {
            location: props.vector("Location").unwrap_or(Vec3::ZERO),
            rotation: props.rotator("Rotation").unwrap_or_default(),
            draw_scale: props.float("DrawScale").unwrap_or(1.0),
            draw_scale_3d: props
                .vector("DrawScale3D")
                .unwrap_or(Vec3::new(1.0, 1.0, 1.0)),
            pre_pivot: props.vector("PrePivot").unwrap_or(Vec3::ZERO),
            static_mesh: props.index("StaticMesh"),
            delete_me: props.boolean("bDeleteMe"),
            hidden: props.boolean("bHidden"),
            collide_actors: props.boolean_or("bCollideActors", true),
            block_actors: props.boolean_or("bBlockActors", true),
            block_players: props.boolean_or("bBlockPlayers", true),
        }
    }
    fn transform(&self) -> Transform {
        Transform {
            position: self.location - self.pre_pivot,
            rotation: self.rotation.vector(),
            scale: self.draw_scale_3d * self.draw_scale,
        }
    }
}

#[derive(Clone)]
struct Brush {
    actor: Actor,
    brush: Option<i32>,
}
impl Brush {
    fn read(archive: &Archive, reader: &mut Reader<'_>, flags: u32) -> Result<Self> {
        let props = read_properties(archive, reader, flags)?;
        Ok(Self {
            actor: Actor::from_properties(&props),
            brush: props.index("Brush"),
        })
    }
    fn transform(&self) -> Transform {
        self.actor.transform()
    }
}

#[derive(Clone)]
struct Terrain {
    actor: Actor,
    map_index: i32,
    terrain_scale: Vec3,
    quad_visibility: Vec<u8>,
    edge_turn: Vec<u8>,
    map_x: i32,
    map_y: i32,
}
impl Terrain {
    fn read(archive: &Archive, reader: &mut Reader<'_>, flags: u32) -> Result<Self> {
        let props = read_properties(archive, reader, flags)?;
        let map_index = props
            .index("TerrainMap")
            .ok_or_else(|| AppError::InvalidData("TerrainInfo without TerrainMap".into()))?;
        Ok(Self {
            actor: Actor::from_properties(&props),
            map_index,
            terrain_scale: props.vector("TerrainScale").unwrap_or(Vec3::ZERO),
            quad_visibility: props.bytes("QuadVisibilityBitmap").unwrap_or_default(),
            edge_turn: props.bytes("EdgeTurnBitmap").unwrap_or_default(),
            map_x: props.integer("MapX").unwrap_or(0),
            map_y: props.integer("MapY").unwrap_or(0),
        })
    }
    fn broken_scale(&self) -> bool {
        self.terrain_scale.x == 0.0 || self.terrain_scale.y == 0.0 || self.terrain_scale.z == 0.0
    }
    fn scale(&self) -> Vec3 {
        if self.broken_scale() {
            Vec3::new(128.0, 128.0, 76.0 / 256.0)
        } else {
            Vec3::new(
                self.terrain_scale.x,
                self.terrain_scale.y,
                self.terrain_scale.z / 256.0,
            )
        }
    }
    fn position(&self, texture: &Texture) -> Vec3 {
        let scale = self.scale();
        if self.broken_scale() {
            Vec3::new(
                (self.map_x - 20) as f32 * texture.u_size as f32 * 128.0,
                (self.map_y - 18) as f32 * texture.v_size as f32 * 128.0,
                0.0,
            )
        } else {
            Vec3::new(
                self.actor.location.x - texture.u_size as f32 / 2.0 * scale.x,
                self.actor.location.y - texture.v_size as f32 / 2.0 * scale.y,
                self.actor.location.z - 32768.0 * scale.z,
            )
        }
    }
    fn bounds(&self, texture: &Texture) -> Box3 {
        let position = self.position(texture);
        let scale = self.scale();
        Box3::new(
            Vec3::new(0.0, 0.0, -(16384.0 + position.z) / scale.z) * scale + position,
            Vec3::new(
                texture.u_size as f32,
                texture.v_size as f32,
                (16384.0 - position.z) / scale.z,
            ) * scale
                + position,
        )
    }
}

#[derive(Clone)]
struct Texture {
    format: u8,
    u_size: i32,
    v_size: i32,
    mips: Vec<Vec<u8>>,
}
impl Texture {
    fn read(archive: &Archive, reader: &mut Reader<'_>, flags: u32) -> Result<Self> {
        let props = read_properties(archive, reader, flags)?;
        let format = props.byte("Format").unwrap_or(0);
        let u_size = props.integer("USize").unwrap_or(0);
        let v_size = props.integer("VSize").unwrap_or(0);
        skip_material_data(
            reader,
            archive.header.file_version,
            archive.header.license_version,
        )?;
        let mips = if matches!(format, 3 | 5 | 7 | 8 | 10) {
            let block_size = match format {
                3 => 8,
                7 | 8 => 16,
                10 => 32,
                5 => 64,
                _ => unreachable!(),
            };
            let expected = (u_size / 4)
                .checked_mul(v_size / 4)
                .and_then(|size| size.checked_mul(block_size))
                .ok_or_else(|| AppError::InvalidData("invalid texture size".into()))?;
            let mut size = -1;
            while size != expected {
                size = reader.index()?;
                if reader.remaining() == 0 {
                    return Err(AppError::InvalidData(
                        "unexpected EOF while deserializing texture".into(),
                    ));
                }
            }
            vec![
                reader.bytes(
                    usize::try_from(size)
                        .map_err(|_| AppError::InvalidData("negative mip size".into()))?,
                )?,
            ]
        } else {
            // Non-heightmap texture payloads are irrelevant to build mode, but
            // their serialized array still has to be consumed precisely.
            let _discarded = read_array(reader, |reader| reader.u8())?;
            Vec::new()
        };
        Ok(Self {
            format,
            u_size,
            v_size,
            mips,
        })
    }
    fn heights(&self) -> Result<Vec<u16>> {
        if self.format != 10 {
            return Err(AppError::InvalidData(format!(
                "TerrainMap has unsupported format {} (expected G16)",
                self.format
            )));
        }
        let bytes = self
            .mips
            .first()
            .ok_or_else(|| AppError::InvalidData("TerrainMap has no mip".into()))?;
        let expected = usize::try_from(self.u_size)
            .ok()
            .and_then(|u| {
                usize::try_from(self.v_size)
                    .ok()
                    .and_then(|v| u.checked_mul(v))
            })
            .and_then(|n| n.checked_mul(2))
            .ok_or_else(|| AppError::InvalidData("invalid TerrainMap dimensions".into()))?;
        if bytes.len() < expected {
            return Err(AppError::InvalidData("TerrainMap mip is truncated".into()));
        }
        Ok(bytes[..expected]
            .chunks_exact(2)
            .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
            .collect())
    }
}

#[derive(Clone)]
struct StaticMesh {
    vertices: Vec<Vertex>,
    surfaces: Vec<StaticSurface>,
    indices: Vec<u16>,
    materials: Vec<bool>,
}
#[derive(Clone)]
struct StaticSurface {
    first_index: u16,
    triangle_max: u16,
}
impl StaticMesh {
    fn read(archive: &Archive, reader: &mut Reader<'_>, flags: u32) -> Result<Self> {
        let props = read_properties(archive, reader, flags)?;
        primitive_tail(reader)?;
        let surfaces = read_array(reader, |reader| {
            Ok(StaticSurface {
                first_index: {
                    reader.u32()?;
                    reader.u16()?
                },
                triangle_max: {
                    reader.u16()?;
                    reader.u16()?;
                    reader.u16()?;
                    reader.u16()?
                },
            })
        })?;
        let _bounding_box = read_box(reader)?;
        let vertices = read_array(reader, |reader| {
            Ok(Vertex {
                position: reader.vector()?,
                normal: reader.vector()?,
            })
        })?;
        let _vertex_revision = reader.u32()?;
        let _colors = read_array(reader, |reader| {
            reader.skip(4)?;
            Ok(())
        })?;
        reader.u32()?;
        let _alpha = read_array(reader, |reader| {
            reader.skip(4)?;
            Ok(())
        })?;
        reader.u32()?;
        let uv_stream_count = reader.index()?;
        check_count("UV stream", uv_stream_count)?;
        for _ in 0..uv_stream_count {
            let _uvs = read_array(reader, |reader| {
                reader.f32()?;
                reader.f32()?;
                Ok(())
            })?;
            reader.u32()?;
            reader.u32()?;
        }
        let indices = read_array(reader, |reader| reader.u16())?;
        reader.u32()?;
        let _wireframe = read_array(reader, |reader| reader.u16())?;
        reader.u32()?;
        let _collision_model = reader.index()?;
        let materials = props
            .array_maps("Materials")
            .iter()
            .map(|map| map.boolean("EnableCollision"))
            .collect();
        Ok(Self {
            vertices,
            surfaces,
            indices,
            materials,
        })
    }
    fn collision_mesh(&self) -> Result<Mesh> {
        let mut mesh = Mesh {
            vertices: self.vertices.clone(),
            indices: Vec::new(),
        };
        for (surface_index, surface) in self.surfaces.iter().enumerate() {
            if !self.materials.get(surface_index).copied().unwrap_or(false) {
                continue;
            }
            for triangle in 0..surface.triangle_max as usize {
                let start = surface.first_index as usize + triangle * 3;
                let values = self.indices.get(start..start + 3).ok_or_else(|| {
                    AppError::InvalidData("StaticMesh surface index range is invalid".into())
                })?;
                mesh.indices
                    .extend([values[2] as u32, values[1] as u32, values[0] as u32]);
            }
        }
        Ok(mesh)
    }
}

#[derive(Clone)]
struct Level {
    model_index: i32,
}
impl Level {
    fn read(archive: &Archive, reader: &mut Reader<'_>, flags: u32) -> Result<Self> {
        let _props = read_properties(archive, reader, flags)?;
        let count1 = reader.i32()?;
        let _duplicate1 = reader.i32()?;
        check_count("Level objects", count1)?;
        for _ in 0..count1 {
            reader.index()?;
        }
        if archive.header.license_version > 20 {
            let count2 = reader.i32()?;
            let _duplicate2 = reader.i32()?;
            check_count("Level objects", count2)?;
            for _ in 0..count2 {
                reader.index()?;
            }
        }
        reader.string()?;
        reader.string()?;
        reader.string()?;
        reader.string()?;
        let options = reader.index()?;
        check_count("URL options", options)?;
        for _ in 0..options {
            reader.string()?;
        }
        reader.i32()?;
        reader.u8()?;
        reader.skip(2)?;
        let reach_specs = reader.index()?;
        check_count("reach specs", reach_specs)?;
        for _ in 0..reach_specs {
            reader.i32()?;
            reader.index()?;
            reader.index()?;
            reader.i32()?;
            reader.i32()?;
            reader.i32()?;
            reader.u8()?;
        }
        Ok(Self {
            model_index: reader.index()?,
        })
    }
}

#[derive(Clone)]
struct Model {
    vectors: Vec<Vec3>,
    points: Vec<Vec3>,
    nodes: Vec<BspNode>,
    surfaces: Vec<BspSurface>,
    vertices: Vec<BspVertex>,
}
#[derive(Clone)]
struct BspNode {
    flags: u8,
    vertex_pool_index: i32,
    surface_index: i32,
    vertex_count: u8,
}
#[derive(Clone)]
struct BspSurface {
    polygon_flags: u32,
    base_index: i32,
    normal_index: i32,
    u_index: i32,
    v_index: i32,
}
#[derive(Clone)]
struct BspVertex {
    vertex_index: i32,
}
impl Model {
    fn read(archive: &Archive, reader: &mut Reader<'_>, flags: u32) -> Result<Self> {
        let _props = read_properties(archive, reader, flags)?;
        primitive_tail(reader)?;
        let vectors = read_array(reader, |reader| reader.vector())?;
        let points = read_array(reader, |reader| reader.vector())?;
        let nodes = read_array(reader, |reader| {
            reader.skip(16)?;
            reader.u64()?;
            let flags = reader.u8()?;
            let vertex_pool_index = reader.index()?;
            let surface_index = reader.index()?;
            for _ in 0..5 {
                reader.index()?;
            }
            reader.vector()?;
            reader.i32()?;
            reader.u64()?;
            reader.u64()?;
            reader.index()?;
            reader.index()?;
            let vertex_count = reader.u8()?;
            reader.i32()?;
            reader.i32()?;
            reader.skip(12)?;
            Ok(BspNode {
                flags,
                vertex_pool_index,
                surface_index,
                vertex_count,
            })
        })?;
        let license = archive.header.license_version;
        let surfaces = read_array(reader, |reader| {
            reader.index()?;
            let polygon_flags = reader.u32()?;
            let base_index = reader.index()?;
            let normal_index = reader.index()?;
            let u_index = reader.index()?;
            let v_index = reader.index()?;
            reader.index()?;
            reader.index()?;
            reader.skip(16)?;
            reader.u32()?;
            if license > 20 {
                reader.u32()?;
            }
            Ok(BspSurface {
                polygon_flags,
                base_index,
                normal_index,
                u_index,
                v_index,
            })
        })?;
        let vertices = read_array(reader, |reader| {
            let vertex_index = reader.index()?;
            reader.index()?;
            Ok(BspVertex { vertex_index })
        })?;
        Ok(Self {
            vectors,
            points,
            nodes,
            surfaces,
            vertices,
        })
    }
    fn mesh(&self, bounds: Option<Box3>) -> Result<Option<Mesh>> {
        let mut mesh = Mesh::default();
        for node in &self.nodes {
            if node.flags & NF_PASSABLE != 0 {
                continue;
            }
            let surface = self
                .surfaces
                .get(
                    usize::try_from(node.surface_index)
                        .map_err(|_| AppError::InvalidData("negative BSP surface index".into()))?,
                )
                .ok_or_else(|| AppError::InvalidData("BSP surface index out of bounds".into()))?;
            if surface.polygon_flags & PF_PASSABLE != 0 {
                continue;
            }
            let start = usize::try_from(node.vertex_pool_index)
                .map_err(|_| AppError::InvalidData("negative BSP vertex pool index".into()))?;
            let count = node.vertex_count as usize;
            let references = self.vertices.get(start..start + count).ok_or_else(|| {
                AppError::InvalidData(format!(
                    "BSP vertex pool range is invalid (start={start}, count={count}, pool={})",
                    self.vertices.len()
                ))
            })?;
            let positions = references
                .iter()
                .map(|vertex| {
                    self.points
                        .get(usize::try_from(vertex.vertex_index).unwrap_or(usize::MAX))
                        .copied()
                        .ok_or_else(|| {
                            AppError::InvalidData("BSP point index out of bounds".into())
                        })
                })
                .collect::<Result<Vec<_>>>()?;
            if let Some(bounds) = bounds {
                if positions
                    .iter()
                    .any(|point| !bounds.contains_strict(*point))
                {
                    continue;
                }
            }
            if positions.len() < 3 {
                continue;
            }
            let normal = *self
                .vectors
                .get(
                    usize::try_from(surface.normal_index)
                        .map_err(|_| AppError::InvalidData("negative BSP normal index".into()))?,
                )
                .ok_or_else(|| AppError::InvalidData("BSP normal index out of bounds".into()))?;
            let offset = mesh.vertices.len() as u32;
            mesh.vertices.extend(
                positions
                    .iter()
                    .copied()
                    .map(|position| Vertex { position, normal }),
            );
            for index in 2..positions.len() {
                mesh.indices
                    .extend([offset, offset + index as u32 - 1, offset + index as u32]);
            }
            if surface.polygon_flags & 0x0000_0100 != 0 {
                let reverse_offset = mesh.vertices.len() as u32;
                mesh.vertices
                    .extend(positions.iter().copied().map(|position| Vertex {
                        position,
                        normal: Vec3::new(normal.x, normal.y, -normal.z),
                    }));
                for index in 2..positions.len() {
                    mesh.indices.extend([
                        reverse_offset,
                        reverse_offset + index as u32,
                        reverse_offset + index as u32 - 1,
                    ]);
                }
            }
            let _ = (surface.base_index, surface.u_index, surface.v_index);
        }
        if mesh.vertices.is_empty() {
            Ok(None)
        } else {
            Ok(Some(mesh))
        }
    }
}

fn terrain_mesh(
    terrain: &Terrain,
    texture: &Texture,
    position: Vec3,
    scale: Vec3,
    south: Option<(Terrain, Texture)>,
    east: Option<(Terrain, Texture)>,
    southeast: Option<(Terrain, Texture)>,
) -> Result<Mesh> {
    let width = usize::try_from(texture.u_size)
        .map_err(|_| AppError::InvalidData("negative terrain width".into()))?;
    let height = usize::try_from(texture.v_size)
        .map_err(|_| AppError::InvalidData("negative terrain height".into()))?;
    if width < 2 || height < 2 {
        return Err(AppError::InvalidData("terrain must be at least 2x2".into()));
    }
    let main_heights = texture.heights()?;
    // The Current loader stores the main tile plus one south/east border in a
    // (width + 1) × (height + 1) height grid before deriving vertex normals.
    // Keep the same layout and vertex insertion order; the border is vital for
    // seamless geodata between adjacent maps.
    let mut heights = vec![0u16; (width + 1) * (height + 1)];
    let mut mesh = Mesh::default();
    for y in 0..height {
        for x in 0..width {
            let value = main_heights[y * width + x];
            mesh.vertices.push(Vertex {
                position: Vec3::new(x as f32, y as f32, value as f32) * scale + position,
                normal: Vec3::ZERO,
            });
            heights[y * (width + 1) + x] = value;
        }
    }
    for y in 0..height - 1 {
        for x in 0..width - 1 {
            if !bit(&terrain.quad_visibility, x + y * width) {
                continue;
            }
            if !bit(&terrain.edge_turn, x + y * width) {
                mesh.indices.extend([
                    (x + y * width) as u32,
                    (x + 1 + y * width) as u32,
                    (x + 1 + (y + 1) * width) as u32,
                    (x + y * width) as u32,
                    (x + 1 + (y + 1) * width) as u32,
                    (x + (y + 1) * width) as u32,
                ]);
            } else {
                mesh.indices.extend([
                    (x + (y + 1) * width) as u32,
                    (x + y * width) as u32,
                    (x + 1 + y * width) as u32,
                    (x + (y + 1) * width) as u32,
                    (x + 1 + y * width) as u32,
                    (x + 1 + (y + 1) * width) as u32,
                ]);
            }
        }
    }

    let has_south = south.is_some();
    if let Some((south_terrain, south_texture)) = south {
        if south_texture.u_size != texture.u_size {
            return Err(AppError::InvalidData(
                "south terrain width differs from map terrain".into(),
            ));
        }
        let side_heights = south_texture.heights()?;
        let side_position = Vec3::new(
            position.x,
            position.y,
            south_terrain.position(&south_texture).z,
        );
        let side_scale = Vec3::new(scale.x, scale.y, south_terrain.scale().z);
        let y = height;
        for x in 0..width {
            let value = side_heights[x];
            mesh.vertices.push(Vertex {
                position: Vec3::new(x as f32, y as f32, value as f32) * side_scale + side_position,
                normal: Vec3::ZERO,
            });
            heights[y * (width + 1) + x] = value;
            let last = mesh.vertices.len() as u32 - 1;
            if x != width - 1 {
                mesh.indices.extend([
                    (x + (y - 1) * width) as u32,
                    (x + 1 + (y - 1) * width) as u32,
                    last,
                ]);
            }
            if x != 0 {
                mesh.indices
                    .extend([(x + (y - 1) * width) as u32, last, last - 1]);
            }
        }
    }

    let has_east = east.is_some();
    if let Some((east_terrain, east_texture)) = east {
        if east_texture.v_size != texture.v_size {
            return Err(AppError::InvalidData(
                "east terrain height differs from map terrain".into(),
            ));
        }
        let side_heights = east_texture.heights()?;
        let side_position = Vec3::new(
            position.x,
            position.y,
            east_terrain.position(&east_texture).z,
        );
        let side_scale = Vec3::new(scale.x, scale.y, east_terrain.scale().z);
        let x = width;
        for y in 0..height {
            let value = side_heights[y * width];
            mesh.vertices.push(Vertex {
                position: Vec3::new(x as f32, y as f32, value as f32) * side_scale + side_position,
                normal: Vec3::ZERO,
            });
            heights[y * (width + 1) + x] = value;
            let last = mesh.vertices.len() as u32 - 1;
            if y != height - 1 {
                mesh.indices.extend([
                    (x - 1 + y * width) as u32,
                    last,
                    (x - 1 + (y + 1) * width) as u32,
                ]);
            }
            if y != 0 {
                mesh.indices
                    .extend([(x - 1 + y * width) as u32, last - 1, last]);
            }
        }
    }

    let has_southeast = southeast.is_some();
    if let Some((southeast_terrain, southeast_texture)) = southeast {
        let side_heights = southeast_texture.heights()?;
        let side_position = Vec3::new(
            position.x,
            position.y,
            southeast_terrain.position(&southeast_texture).z,
        );
        let side_scale = Vec3::new(scale.x, scale.y, southeast_terrain.scale().z);
        let x = width;
        let y = height;
        let value = side_heights[0];
        mesh.vertices.push(Vertex {
            position: Vec3::new(x as f32, y as f32, value as f32) * side_scale + side_position,
            normal: Vec3::ZERO,
        });
        heights[y * (width + 1) + x] = value;
        let last = mesh.vertices.len() as u32 - 1;
        mesh.indices
            .extend([(x - 1 + (y - 1) * width) as u32, last - 1, last]);
        mesh.indices.extend([
            (x - 1 + (y - 1) * width) as u32,
            last,
            last - (width as u32 + 2),
        ]);
    }

    // Match the historical finite-difference normal calculation and its mesh
    // indexing. The normals decide winding in SourceMap::add_mesh.
    for y in 0..=height {
        for x in 0..=width {
            let z = heights[y * (width + 1) + x] as f32;
            let top = if y > 0 {
                heights[(y - 1) * (width + 1) + x] as f32
            } else {
                z
            };
            let bottom = if y < height - usize::from(!has_south) {
                heights[(y + 1) * (width + 1) + x] as f32
            } else {
                z
            };
            let left = if x > 0 {
                heights[y * (width + 1) + x - 1] as f32
            } else {
                z
            };
            let right = if x < width - usize::from(!has_east) {
                heights[y * (width + 1) + x + 1] as f32
            } else {
                z
            };
            let normal = Vec3::new(
                (left - right) / ((width + 1) as f32 * 2.0),
                (top - bottom) / ((height + 1) as f32 * 2.0),
                4.0,
            )
            .normalize_or_zero();
            let vertex_index = if x < width && y < height {
                Some(y * width + x)
            } else if x == width && y == height && has_southeast {
                Some(y * (width + 1) + x)
            } else if y == height && has_south {
                Some(y * width + x)
            } else if x == width && has_east {
                Some((height + 1) * width + y)
            } else {
                None
            };
            if let Some(vertex_index) = vertex_index {
                if let Some(vertex) = mesh.vertices.get_mut(vertex_index) {
                    vertex.normal = normal;
                }
            }
        }
    }
    Ok(mesh)
}

fn bit(bytes: &[u8], index: usize) -> bool {
    bytes
        .get(index / 8)
        .map(|byte| byte & (1 << (index % 8)) != 0)
        .unwrap_or(false)
}

#[derive(Clone, Default)]
struct Properties(HashMap<String, Vec<Property>>);
impl Properties {
    fn get(&self, name: &str) -> Option<&Property> {
        self.0.get(name).and_then(|values| values.last())
    }
    fn boolean(&self, name: &str) -> bool {
        self.get(name)
            .is_some_and(|property| matches!(property.value, Value::Bool(true)))
    }
    fn boolean_or(&self, name: &str, default: bool) -> bool {
        self.get(name).map_or(default, |property| {
            matches!(property.value, Value::Bool(true))
        })
    }
    fn byte(&self, name: &str) -> Option<u8> {
        match self.get(name)?.value {
            Value::Byte(value) => Some(value),
            _ => None,
        }
    }
    fn integer(&self, name: &str) -> Option<i32> {
        match self.get(name)?.value {
            Value::Int(value) => Some(value),
            _ => None,
        }
    }
    fn float(&self, name: &str) -> Option<f32> {
        match self.get(name)?.value {
            Value::Float(value) => Some(value),
            _ => None,
        }
    }
    fn index(&self, name: &str) -> Option<i32> {
        match self.get(name)?.value {
            Value::Index(value) => Some(value),
            _ => None,
        }
    }
    fn vector(&self, name: &str) -> Option<Vec3> {
        match self.get(name)?.value {
            Value::Vector(value) => Some(value),
            _ => None,
        }
    }
    fn rotator(&self, name: &str) -> Option<Rotator> {
        match self.get(name)?.value {
            Value::Rotator(value) => Some(value),
            _ => None,
        }
    }
    fn bytes(&self, name: &str) -> Option<Vec<u8>> {
        match &self.get(name)?.value {
            Value::Bytes(value) => Some(value.clone()),
            _ => None,
        }
    }
    fn array_maps(&self, name: &str) -> &[Properties] {
        match self.get(name).map(|property| &property.value) {
            Some(Value::Maps(values)) => values,
            _ => &[],
        }
    }
}
#[derive(Clone)]
struct Property {
    value: Value,
}
#[derive(Clone)]
enum Value {
    Byte(u8),
    Int(i32),
    Bool(bool),
    Float(f32),
    Index(i32),
    Vector(Vec3),
    Rotator(Rotator),
    Bytes(Vec<u8>),
    Maps(Vec<Properties>),
    None,
}

fn read_properties(archive: &Archive, reader: &mut Reader<'_>, flags: u32) -> Result<Properties> {
    if flags & RF_HAS_STACK != 0 {
        let node = reader.index()?;
        reader.index()?;
        reader.u64()?;
        reader.i32()?;
        if node != 0 {
            reader.index()?;
        }
    }
    if flags & RF_NATIVE != 0 {
        return Ok(Properties::default());
    }
    let mut properties = Properties::default();
    loop {
        let name = name_at(&archive.names, reader.index()?)?.to_owned();
        if name == "None" {
            break;
        }
        let info = reader.u8()?;
        let type_id = info & 0x0f;
        let size_type = (info >> 4) & 0x07;
        let is_array = info & 0x80 != 0;
        let struct_name = if type_id == 10 {
            Some(name_at(&archive.names, reader.index()?)?.to_owned())
        } else {
            None
        };
        let size = property_size(reader, size_type)?;
        let value = if type_id == 3 {
            Value::Bool(is_array)
        } else {
            if is_array {
                let _array_index = reader.array_index()?;
            }
            match type_id {
                1 => Value::Byte(reader.u8()?),
                2 => Value::Int(reader.i32()?),
                4 => Value::Float(reader.f32()?),
                5 | 6 => Value::Index(reader.index()?),
                9 => {
                    let start = reader.pos;
                    let count = reader.index()?;
                    check_count("property array", count)?;
                    let prefix = reader.pos - start;
                    let remaining = size.checked_sub(prefix).ok_or_else(|| {
                        AppError::InvalidData(
                            "property array size is smaller than its length".into(),
                        )
                    })?;
                    if name == "Materials" {
                        let mut maps = Vec::with_capacity(count as usize);
                        let end = reader.pos.checked_add(remaining).ok_or_else(|| {
                            AppError::InvalidData("property array size overflow".into())
                        })?;
                        for _ in 0..count {
                            maps.push(read_properties_without_state(archive, reader)?);
                        }
                        if reader.pos != end {
                            return Err(AppError::InvalidData(
                                "invalid Materials property array".into(),
                            ));
                        }
                        Value::Maps(maps)
                    } else {
                        Value::Bytes(reader.bytes(remaining)?)
                    }
                }
                10 if struct_name.as_deref() == Some("Vector") => Value::Vector(reader.vector()?),
                10 if struct_name.as_deref() == Some("Rotator") => {
                    Value::Rotator(reader.rotator()?)
                }
                10 if struct_name.as_deref() == Some("TerrainLayer") => {
                    Value::Maps(vec![read_properties_without_state(archive, reader)?])
                }
                11 => Value::Vector(reader.vector()?),
                12 => Value::Rotator(reader.rotator()?),
                _ => {
                    reader.skip(size)?;
                    Value::None
                }
            }
        };
        properties
            .0
            .entry(name)
            .or_default()
            .push(Property { value });
    }
    Ok(properties)
}
fn read_properties_without_state(archive: &Archive, reader: &mut Reader<'_>) -> Result<Properties> {
    read_properties(archive, reader, 0)
}
fn property_size(reader: &mut Reader<'_>, size_type: u8) -> Result<usize> {
    Ok(match size_type {
        0 => 1,
        1 => 2,
        2 => 4,
        3 => 12,
        4 => 16,
        5 => reader.u8()? as usize,
        6 => reader.u16()? as usize,
        7 => reader.u32()? as usize,
        _ => unreachable!(),
    })
}

fn primitive_tail(reader: &mut Reader<'_>) -> Result<()> {
    let _box = read_box(reader)?;
    reader.vector()?;
    reader.f32()?;
    Ok(())
}
fn read_box(reader: &mut Reader<'_>) -> Result<Box3> {
    let min = reader.vector()?;
    let max = reader.vector()?;
    let _is_valid = reader.u8()?;
    Ok(Box3::new(min, max))
}

fn read_array<T>(
    reader: &mut Reader<'_>,
    mut read: impl FnMut(&mut Reader<'_>) -> Result<T>,
) -> Result<Vec<T>> {
    let count = reader.index()?;
    check_count("array", count)?;
    (0..count).map(|_| read(reader)).collect()
}

fn skip_material_data(
    reader: &mut Reader<'_>,
    file_version: i16,
    license_version: i16,
) -> Result<()> {
    if file_version >= 123 && (16..37).contains(&license_version) {
        reader.u32()?;
    }
    if file_version >= 123 && (30..37).contains(&license_version) {
        if (33..36).contains(&license_version) {
            reader.u8()?;
        }
        for _ in 0..6 {
            reader.u8()?;
        }
        reader.u32()?;
        reader.u32()?;
        reader.u32()?;
        for _ in 0..8 {
            reader.u8()?;
            if license_version < 36 {
                reader.u8()?;
            }
            reader.skip(126)?;
        }
        reader.skip(8)?;
        reader.u32()?;
        reader.u32()?;
        reader.u32()?;
        for _ in 0..16 {
            reader.string()?;
        }
        reader.string()?;
    }
    if file_version >= 123 && license_version >= 37 {
        reader.u8()?;
        reader.u8()?;
        if file_version < 129 {
            reader.skip(2 + 12)?;
        } else {
            reader.skip(5 * (6 + 8))?;
        }
        reader.skip(8 + 12)?;
        let stages = reader.index()?;
        check_count("material stages", stages)?;
        for _ in 0..stages {
            reader.string()?;
            let values = reader.index()?;
            check_count("material stage strings", values)?;
            for _ in 0..values {
                reader.string()?;
            }
        }
        reader.string()?;
    }
    if file_version >= 123 && license_version >= 31 {
        reader.u16()?;
        reader.u16()?;
    }
    Ok(())
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}
impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8], pos: usize) -> Self {
        Self { bytes, pos }
    }
    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.pos)
    }
    fn take<const N: usize>(&mut self) -> Result<[u8; N]> {
        let end = self
            .pos
            .checked_add(N)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| AppError::InvalidData("unexpected EOF in Unreal package".into()))?;
        let mut out = [0; N];
        out.copy_from_slice(&self.bytes[self.pos..end]);
        self.pos = end;
        Ok(out)
    }
    fn bytes(&mut self, size: usize) -> Result<Vec<u8>> {
        let end = self
            .pos
            .checked_add(size)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| AppError::InvalidData("unexpected EOF in Unreal package".into()))?;
        let result = self.bytes[self.pos..end].to_vec();
        self.pos = end;
        Ok(result)
    }
    fn skip(&mut self, size: usize) -> Result<()> {
        self.bytes(size).map(|_| ())
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take::<1>()?[0])
    }
    fn i16(&mut self) -> Result<i16> {
        Ok(i16::from_le_bytes(self.take()?))
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take()?))
    }
    fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.take()?))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take()?))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take()?))
    }
    fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_le_bytes(self.take()?))
    }
    fn vector(&mut self) -> Result<Vec3> {
        Ok(Vec3::new(self.f32()?, self.f32()?, self.f32()?))
    }
    fn rotator(&mut self) -> Result<Rotator> {
        Ok(Rotator {
            pitch: self.i32()?,
            yaw: self.i32()?,
            roll: self.i32()?,
        })
    }
    fn array_index(&mut self) -> Result<u32> {
        let byte = self.u8()?;
        if byte < 128 {
            return Ok(byte as u32);
        }
        // This mirrors Current's ArrayIndex reader, including its two-byte
        // representation for all high-bit values.
        Ok(byte as u32 | ((self.u8()? as u32) << 8))
    }
    fn index(&mut self) -> Result<i32> {
        let mut byte = self.u8()?;
        let negative = byte & 0x80 != 0;
        let mut value = (byte & 0x3f) as i32;
        if byte & 0x40 != 0 {
            let mut shift = 6;
            loop {
                byte = self.u8()?;
                value |= ((byte & 0x7f) as i32) << shift;
                shift += 7;
                if byte & 0x80 == 0 || shift >= 32 {
                    break;
                }
            }
        }
        Ok(if negative { -value } else { value })
    }
    fn string(&mut self) -> Result<String> {
        let length = self.index()?;
        if length < 0 {
            return Err(AppError::Unsupported(
                "UTF-16 Unreal strings are not supported by the current C++ reader either".into(),
            ));
        }
        let mut bytes = self.bytes(length as usize)?;
        if bytes.last() == Some(&0) {
            bytes.pop();
        }
        String::from_utf8(bytes)
            .map_err(|_| AppError::InvalidData("invalid ANSI string in Unreal package".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reads_compact_indices() {
        let mut reader = Reader::new(&[0x3f, 0x41, 0x01, 0xc1, 0x01], 0);
        assert_eq!(reader.index().unwrap(), 63);
        assert_eq!(reader.index().unwrap(), 65);
        assert_eq!(reader.index().unwrap(), -65);
    }
    #[test]
    fn decrypts_v111_payload() {
        let mut encrypted =
            b"L\x00i\x00n\x00e\x00a\x00g\x00e\x002\x00V\x00e\x00r\x001\x001\x001\x00".to_vec();
        encrypted.extend([0xac ^ b'a', 0xac ^ b'b']);
        let path = std::env::temp_dir().join("geodata-editor-v111.unr");
        fs::write(&path, encrypted).unwrap();
        assert_eq!(decrypt(&path).unwrap(), b"ab");
        let _ = fs::remove_file(path);
    }
}
