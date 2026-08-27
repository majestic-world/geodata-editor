//! Command line entry point and configuration for the standalone editor.

use std::{
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

use crate::{
    editor_view,
    error::{AppError, Result},
};

#[derive(Clone, Debug, Default)]
pub struct EditorOptions {
    pub input: Option<PathBuf>,
    pub client_root: Option<PathBuf>,
    pub map_type: Option<MapType>,
}

/// Map flavour of the Lineage II client. It decides which Unreal package under
/// `<client>/Maps` backs a geodata region: `22_22.unr` for normal clients and
/// `22_22_Classic.unr` for classic ones.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MapType {
    #[default]
    Classic,
    Normal,
}

impl MapType {
    pub const ALL: [Self; 2] = [Self::Classic, Self::Normal];

    pub fn label(self) -> &'static str {
        match self {
            Self::Classic => "Classic",
            Self::Normal => "Normal",
        }
    }

    /// Unreal package name for a geodata region, for example `22_22_Classic`.
    pub fn package_name(self, region: &str) -> String {
        match self {
            Self::Classic => format!("{region}_Classic"),
            Self::Normal => region.to_owned(),
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::Classic => "classic",
            Self::Normal => "normal",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "classic" => Some(Self::Classic),
            "normal" => Some(Self::Normal),
            _ => None,
        }
    }
}

/// Color scheme for the editor UI. Dark is the default and the theme the
/// editor has always shipped with; light is opt-in via the toolbar toggle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EditorTheme {
    #[default]
    Dark,
    Light,
}

impl EditorTheme {
    /// Returns the other theme, used to implement a single toggle control.
    pub fn toggled(self) -> Self {
        match self {
            Self::Dark => Self::Light,
            Self::Light => Self::Dark,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Dark => "Escuro",
            Self::Light => "Claro",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "dark" => Some(Self::Dark),
            "light" => Some(Self::Light),
            _ => None,
        }
    }
}

/// Region shared by a geodata file and its Unreal package: `22_22` for
/// `22_22.l2j`, `22_22.l2g`, and `22_22_conv.dat`.
pub fn geodata_region(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let region = match name.to_ascii_lowercase().strip_suffix("_conv.dat") {
        Some(prefix) => &name[..prefix.len()],
        None => path.file_stem()?.to_str()?,
    };
    (!region.is_empty()).then(|| region.to_owned())
}

/// Values restored into the editor welcome screen. They are stored only in the
/// local Windows user profile, not beside the executable or in the repository.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EditorMemory {
    pub client_root: String,
    pub geodata_path: String,
    pub map_type: MapType,
    pub theme: EditorTheme,
}

const MEMORY_FILE: &str = "editor-history.ini";

const USAGE: &str = "GeodataEditor [--input <arquivo.l2j|arquivo.l2g|mapa_conv.dat>] [--client-root <cliente>] [--type classic|normal]";

pub fn load_memory() -> EditorMemory {
    let Ok(contents) = fs::read_to_string(memory_path()) else {
        return EditorMemory::default();
    };
    parse_memory(&contents)
}

pub fn save_memory(memory: &EditorMemory) -> Result<()> {
    let path = memory_path();
    let parent = path.parent().ok_or_else(|| {
        AppError::InvalidData("editor memory file has no parent directory".into())
    })?;
    fs::create_dir_all(parent)?;
    fs::write(path, format_memory(memory))?;
    Ok(())
}

fn memory_path() -> PathBuf {
    let base = env::var_os("APPDATA")
        .or_else(|| env::var_os("LOCALAPPDATA"))
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir);
    base.join("GeodataEditor").join(MEMORY_FILE)
}

fn format_memory(memory: &EditorMemory) -> String {
    format!(
        "version=4\nclient_root={}\ngeodata_path={}\nmap_type={}\ntheme={}\n",
        escape(&memory.client_root),
        escape(&memory.geodata_path),
        memory.map_type.key(),
        memory.theme.key(),
    )
}

fn parse_memory(contents: &str) -> EditorMemory {
    let mut memory = EditorMemory::default();
    for line in contents.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = unescape(value);
        match key {
            "client_root" => memory.client_root = value,
            "geodata_path" => memory.geodata_path = value,
            "map_type" => memory.map_type = MapType::parse(&value).unwrap_or_default(),
            "theme" => memory.theme = EditorTheme::parse(&value).unwrap_or_default(),
            _ => {}
        }
    }
    memory
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn unescape(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut escaped = false;
    for character in value.chars() {
        if escaped {
            result.push(match character {
                'n' => '\n',
                'r' => '\r',
                '\\' => '\\',
                other => other,
            });
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else {
            result.push(character);
        }
    }
    if escaped {
        result.push('\\');
    }
    result
}

pub fn run_cli(arguments: impl IntoIterator<Item = OsString>) -> Result<()> {
    let arguments = arguments.into_iter().collect::<Vec<_>>();
    if arguments
        .iter()
        .skip(1)
        .any(|value| value == "--help" || value == "-h")
    {
        println!("{USAGE}");
        return Ok(());
    }
    let options = parse(arguments)?;
    if crate::update::check_and_apply() {
        return Ok(());
    }
    editor_view::run_editor(options)
}

pub fn parse(arguments: impl IntoIterator<Item = OsString>) -> Result<EditorOptions> {
    let mut values = arguments.into_iter();
    let program = values.next().unwrap_or_default();
    let values = values
        .map(|value| {
            value
                .into_string()
                .map_err(|_| AppError::InvalidArgument("arguments must be valid Unicode".into()))
        })
        .collect::<Result<Vec<_>>>()?;
    if values
        .iter()
        .any(|value| value == "--help" || value == "-h")
    {
        println!("{USAGE}");
        return Err(AppError::InvalidArgument(String::new()));
    }
    let mut options = EditorOptions::default();
    let mut index = 0;
    while index < values.len() {
        let option = &values[index];
        let mut next = |flag: &str| -> Result<String> {
            index += 1;
            values
                .get(index)
                .cloned()
                .ok_or_else(|| AppError::InvalidArgument(format!("missing value for {flag}")))
        };
        match option.as_str() {
            "--input" => options.input = Some(PathBuf::from(next("--input")?)),
            "--client-root" => options.client_root = Some(PathBuf::from(next("--client-root")?)),
            "--type" => {
                let value = next("--type")?;
                options.map_type = Some(MapType::parse(&value).ok_or_else(|| {
                    AppError::InvalidArgument(format!(
                        "invalid map type: {value}; expected classic or normal"
                    ))
                })?);
            }
            unknown => {
                return Err(AppError::InvalidArgument(format!(
                    "unknown option: {unknown}"
                )));
            }
        }
        index += 1;
    }
    if options.input.is_some() && options.client_root.is_none() {
        return Err(AppError::InvalidArgument(
            "GeodataEditor requires --client-root when --input is provided".into(),
        ));
    }
    if let Some(root) = &options.client_root {
        if !root.is_dir() {
            return Err(AppError::InvalidArgument(format!(
                "invalid Lineage II client path: {}",
                root.display()
            )));
        }
    }
    if let Some(input) = &options.input {
        if !input.is_file() {
            return Err(AppError::InvalidArgument(format!(
                "invalid geodata input: {}",
                input.display()
            )));
        }
    }
    let _ = program;
    Ok(options)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_client_context_with_a_map_type() {
        let options = parse([
            "GeodataEditor".into(),
            "--client-root".into(),
            ".".into(),
            "--type".into(),
            "Normal".into(),
        ])
        .unwrap();
        assert_eq!(options.input, None);
        assert_eq!(options.map_type, Some(MapType::Normal));
    }

    #[test]
    fn rejects_an_unknown_map_type() {
        let error =
            parse(["GeodataEditor".into(), "--type".into(), "chronicle".into()]).unwrap_err();
        assert!(error.to_string().contains("invalid map type"));
    }

    #[test]
    fn rejects_an_l2j_without_a_client_root() {
        let error = parse(["GeodataEditor".into(), "--input".into(), "x.l2j".into()]).unwrap_err();
        assert!(error.to_string().contains("--client-root"));
    }

    #[test]
    fn reads_the_region_of_every_geodata_extension() {
        assert_eq!(
            geodata_region(Path::new(r"D:\Geodata\22_22.l2j")).as_deref(),
            Some("22_22")
        );
        assert_eq!(
            geodata_region(Path::new("22_22.l2g")).as_deref(),
            Some("22_22")
        );
        assert_eq!(
            geodata_region(Path::new("22_22_conv.dat")).as_deref(),
            Some("22_22")
        );
        assert_eq!(geodata_region(Path::new("_conv.dat")), None);
    }

    #[test]
    fn names_the_unreal_package_of_each_map_type() {
        assert_eq!(MapType::Classic.package_name("22_22"), "22_22_Classic");
        assert_eq!(MapType::Normal.package_name("22_22"), "22_22");
    }

    #[test]
    fn memory_round_trip_preserves_windows_paths() {
        let memory = EditorMemory {
            client_root: r"C:\Lineage II\Client".into(),
            geodata_path: r"D:\Geodata\22_22.l2j".into(),
            map_type: MapType::Normal,
            theme: EditorTheme::Light,
        };
        assert_eq!(parse_memory(&format_memory(&memory)), memory);
    }
}
