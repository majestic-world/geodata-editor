//! Command line entry point and configuration for the standalone editor.

use std::{env, ffi::OsString, fs, path::PathBuf};

use crate::{
    editor_view,
    error::{AppError, Result},
};

#[derive(Clone, Debug, Default)]
pub struct EditorOptions {
    pub input: Option<PathBuf>,
    pub client_root: Option<PathBuf>,
    pub map: Option<String>,
}

/// Values restored into the editor welcome screen. They are stored only in the
/// local Windows user profile, not beside the executable or in the repository.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EditorMemory {
    pub client_root: String,
    pub geodata_path: String,
    pub map_name: String,
}

const MEMORY_FILE: &str = "editor-history.ini";

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
        "version=2\nclient_root={}\ngeodata_path={}\nmap_name={}\n",
        escape(&memory.client_root),
        escape(&memory.geodata_path),
        escape(&memory.map_name),
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
            "map_name" => memory.map_name = value,
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
        println!("GeodataEditor [--input <arquivo.l2j>] [--client-root <cliente> --map <mapa>]");
        return Ok(());
    }
    let options = parse(arguments)?;
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
        println!("GeodataEditor [--input <arquivo.l2j>] [--client-root <cliente> --map <mapa>]");
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
            "--map" => options.map = Some(next("--map")?),
            unknown => {
                return Err(AppError::InvalidArgument(format!(
                    "unknown option: {unknown}"
                )));
            }
        }
        index += 1;
    }
    if options.client_root.is_some() != options.map.is_some() {
        return Err(AppError::InvalidArgument(
            "--client-root and --map must be used together".into(),
        ));
    }
    if options.input.is_some() && options.client_root.is_none() {
        return Err(AppError::InvalidArgument(
            "GeodataEditor requires --client-root and --map when --input is provided".into(),
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
                "invalid L2J input: {}",
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
    fn parses_client_map_context() {
        let options = parse([
            "GeodataEditor".into(),
            "--client-root".into(),
            ".".into(),
            "--map".into(),
            "22_22".into(),
        ])
        .unwrap();
        assert_eq!(options.input, None);
        assert_eq!(options.map.as_deref(), Some("22_22"));
    }

    #[test]
    fn rejects_an_l2j_without_client_map_context() {
        let error = parse(["GeodataEditor".into(), "--input".into(), "x.l2j".into()]).unwrap_err();
        assert!(error.to_string().contains("--client-root"));
    }

    #[test]
    fn memory_round_trip_preserves_windows_paths() {
        let memory = EditorMemory {
            client_root: r"C:\Lineage II\Client".into(),
            geodata_path: r"D:\Geodata\22_22.l2j".into(),
            map_name: "22_22_Classic".into(),
        };
        assert_eq!(parse_memory(&format_memory(&memory)), memory);
    }
}
