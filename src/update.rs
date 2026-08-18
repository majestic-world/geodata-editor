use std::{
    env, fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use rfd::{MessageButtons, MessageDialog, MessageDialogResult, MessageLevel};
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
const LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/majestic-world/geodata-editor/releases/latest";
const RELEASE_ASSET_NAME: &str = "GeodataEditor.exe";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
    size: u64,
    digest: Option<String>,
}

struct AvailableUpdate {
    version: Version,
    download_url: String,
    size: u64,
    sha256: [u8; 32],
}

/// Checks the public release channel before the editor window is created.
/// Every network or filesystem failure deliberately resolves to "no update" so
/// offline launches remain indistinguishable from normal launches.
pub fn check_and_apply() -> bool {
    let Some(executable) = env::current_exe().ok() else {
        return false;
    };
    let Some(update) = latest_update() else {
        return false;
    };
    let accepted = MessageDialog::new()
        .set_level(MessageLevel::Info)
        .set_title("Atualização disponível")
        .set_description(format!(
            "A versão {} está disponível.\n\nDeseja baixar e reiniciar o Geodata Editor?",
            update.version
        ))
        .set_buttons(MessageButtons::YesNo)
        .show();
    if accepted != MessageDialogResult::Yes {
        return false;
    }

    let Some(downloaded) = download_update(&executable, &update) else {
        return false;
    };
    schedule_install(&executable, &downloaded)
}

fn latest_update() -> Option<AvailableUpdate> {
    let release = request_release()?;
    select_update(&release, &Version::parse(env!("CARGO_PKG_VERSION")).ok()?)
}

fn request_release() -> Option<Release> {
    let response = agent(REQUEST_TIMEOUT)
        .get(LATEST_RELEASE_URL)
        .set("Accept", "application/vnd.github+json")
        .set(
            "User-Agent",
            concat!("geodata-editor/", env!("CARGO_PKG_VERSION")),
        )
        .call()
        .ok()?;
    serde_json::from_reader(response.into_reader()).ok()
}

fn select_update(release: &Release, current: &Version) -> Option<AvailableUpdate> {
    let version = parse_release_version(&release.tag_name)?;
    if version <= *current {
        return None;
    }
    let asset = release
        .assets
        .iter()
        .find(|asset| asset.name.eq_ignore_ascii_case(RELEASE_ASSET_NAME))?;
    let sha256 = parse_sha256(asset.digest.as_deref()?)?;
    (asset.size > 0).then(|| AvailableUpdate {
        version,
        download_url: asset.browser_download_url.clone(),
        size: asset.size,
        sha256,
    })
}

/// GitHub tags commonly omit trailing zero components (`1.1` instead of
/// `1.1.0`), while `semver` intentionally requires all three components.
fn parse_release_version(tag: &str) -> Option<Version> {
    let tag = tag.trim().trim_start_matches('v');
    if let Ok(version) = Version::parse(tag) {
        return Some(version);
    }
    let suffix_start = tag.find(['-', '+']).unwrap_or(tag.len());
    let (core, suffix) = tag.split_at(suffix_start);
    let missing_components = 3_usize.checked_sub(core.split('.').count())?;
    if missing_components == 0 || core.split('.').any(|part| part.is_empty()) {
        return None;
    }
    Version::parse(&format!(
        "{core}{}{}",
        ".0".repeat(missing_components),
        suffix
    ))
    .ok()
}

fn parse_sha256(value: &str) -> Option<[u8; 32]> {
    let encoded = value.strip_prefix("sha256:")?;
    if encoded.len() != 64 {
        return None;
    }
    let mut digest = [0_u8; 32];
    for (index, pair) in encoded.as_bytes().chunks_exact(2).enumerate() {
        digest[index] = (hex_digit(pair[0])? << 4) | hex_digit(pair[1])?;
    }
    Some(digest)
}

fn hex_digit(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn download_update(executable: &Path, update: &AvailableUpdate) -> Option<PathBuf> {
    let parent = executable.parent()?;
    let app_name = executable.file_stem()?.to_str()?;
    let staged = parent.join(format!("{app_name}.exe.new"));
    let _ = fs::remove_file(&staged);

    let response = agent(DOWNLOAD_TIMEOUT)
        .get(&update.download_url)
        .set(
            "User-Agent",
            concat!("geodata-editor/", env!("CARGO_PKG_VERSION")),
        )
        .call()
        .ok()?;
    let mut input = response.into_reader();
    let mut output = fs::File::create(&staged).ok()?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 65_536];
    loop {
        let count = input.read(&mut buffer).ok()?;
        if count == 0 {
            break;
        }
        output.write_all(&buffer[..count]).ok()?;
        hasher.update(&buffer[..count]);
    }
    if output.sync_all().is_err() {
        let _ = fs::remove_file(&staged);
        return None;
    }
    if fs::metadata(&staged).ok()?.len() != update.size
        || hasher.finalize().as_slice() != update.sha256.as_slice()
    {
        let _ = fs::remove_file(&staged);
        return None;
    }
    Some(staged)
}

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new().timeout(timeout).build()
}

/// Windows cannot reliably replace the currently running executable. The
/// detached PowerShell helper waits for this process to exit, moves the active
/// executable to `{appName}.exe.old`, activates the downloaded file, and starts
/// it. If activation fails, it restores and restarts the prior executable.
fn schedule_install(executable: &Path, downloaded: &Path) -> bool {
    let Some(parent) = executable.parent() else {
        return false;
    };
    let Some(app_name) = executable.file_stem().and_then(|value| value.to_str()) else {
        return false;
    };
    let old = parent.join(format!("{app_name}.exe.old"));
    let script = parent.join(format!(".{app_name}.update.ps1"));
    if fs::write(&script, POWERSHELL_INSTALLER).is_err() {
        return false;
    }

    let spawned = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-WindowStyle",
            "Hidden",
            "-File",
        ])
        .arg(&script)
        .arg(executable)
        .arg(downloaded)
        .arg(&old)
        .spawn()
        .is_ok();
    if !spawned {
        let _ = fs::remove_file(script);
    }
    spawned
}

const POWERSHELL_INSTALLER: &str = r#"
param(
    [string]$CurrentExe,
    [string]$DownloadedExe,
    [string]$OldExe
)

$ErrorActionPreference = 'Stop'
$moved = $false
for ($attempt = 0; $attempt -lt 50; $attempt++) {
    try {
        Remove-Item -LiteralPath $OldExe -Force -ErrorAction SilentlyContinue
        Move-Item -LiteralPath $CurrentExe -Destination $OldExe -Force -ErrorAction Stop
        $moved = $true
        break
    } catch {
        Start-Sleep -Milliseconds 100
    }
}

if (-not $moved) {
    Start-Process -FilePath $CurrentExe
    Remove-Item -LiteralPath $PSCommandPath -Force -ErrorAction SilentlyContinue
    exit 1
}

try {
    Move-Item -LiteralPath $DownloadedExe -Destination $CurrentExe -Force -ErrorAction Stop
    Start-Process -FilePath $CurrentExe
} catch {
    Move-Item -LiteralPath $OldExe -Destination $CurrentExe -Force -ErrorAction SilentlyContinue
    Start-Process -FilePath $CurrentExe -ErrorAction SilentlyContinue
    exit 1
} finally {
    Remove-Item -LiteralPath $PSCommandPath -Force -ErrorAction SilentlyContinue
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str, assets: &[(&str, u64)]) -> Release {
        Release {
            tag_name: tag.into(),
            assets: assets
                .iter()
                .map(|(name, size)| ReleaseAsset {
                    name: (*name).into(),
                    browser_download_url: format!("https://example.invalid/{name}"),
                    size: *size,
                    digest: Some(format!("sha256:{}", "00".repeat(32))),
                })
                .collect(),
        }
    }

    #[test]
    fn selects_only_newer_release_with_editor_asset() {
        let current = Version::parse("1.2.3").unwrap();
        let update =
            select_update(&release("v1.2.4", &[(RELEASE_ASSET_NAME, 1024)]), &current).unwrap();
        assert_eq!(update.version, Version::parse("1.2.4").unwrap());
        assert_eq!(update.size, 1024);
    }

    #[test]
    fn accepts_github_tags_without_patch_component() {
        let current = Version::parse("0.1.0").unwrap();
        let update = select_update(&release("1.1", &[(RELEASE_ASSET_NAME, 1)]), &current).unwrap();
        assert_eq!(update.version, Version::parse("1.1.0").unwrap());
        assert_eq!(
            parse_release_version("v2.0-rc.1").unwrap(),
            Version::parse("2.0.0-rc.1").unwrap()
        );
    }

    #[test]
    fn ignores_equal_older_or_incomplete_releases() {
        let current = Version::parse("1.2.3").unwrap();
        assert!(select_update(&release("v1.2.3", &[(RELEASE_ASSET_NAME, 1)]), &current).is_none());
        assert!(select_update(&release("v1.2.2", &[(RELEASE_ASSET_NAME, 1)]), &current).is_none());
        assert!(select_update(&release("v1.2.4", &[("source.zip", 1)]), &current).is_none());
        assert!(select_update(&release("v1.2.4", &[(RELEASE_ASSET_NAME, 0)]), &current).is_none());
    }

    #[test]
    fn requires_a_valid_sha256_release_digest() {
        let current = Version::parse("1.2.3").unwrap();
        let mut release = release("v1.2.4", &[(RELEASE_ASSET_NAME, 1)]);
        release.assets[0].digest = None;
        assert!(select_update(&release, &current).is_none());
        assert!(parse_sha256("sha256:not-hex").is_none());
        assert_eq!(
            parse_sha256(&format!("sha256:{}", "ab".repeat(32))).unwrap(),
            [0xab; 32]
        );
    }
}
