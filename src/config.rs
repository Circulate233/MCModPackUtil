use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::manifest::curseforge_pack::ImportedManifest;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file '{path}': {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse config file '{path}': {source}")]
    Parse {
        path: String,
        #[source]
        source: toml::de::Error,
    },
    #[error("[network].proxy_ip and [network].proxy_port must be configured together")]
    IncompleteProxy,
    #[error("[network].proxy_port must be between 1 and 65535 when proxy_ip is set")]
    InvalidProxyPort,
    #[error(
        "{field} is no longer supported in config.toml. Move CurseForge API keys to --curseforge-api-key, runtime environment variable MCMODPACKUTIL_CURSEFORGE_API_KEY, CURSEFORGE_API_KEY, CF_API_KEY, or an official build with an embedded key."
    )]
    StoredCurseForgeApiKey { field: &'static str },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LoaderKind {
    Forge,
    Fabric,
    NeoForge,
    Cleanroom,
}

impl LoaderKind {
    pub fn manifest_name(&self) -> &'static str {
        match self {
            Self::Forge | Self::Cleanroom => "forge",
            Self::Fabric => "fabric",
            Self::NeoForge => "neoforge",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub language: String,
    pub pack: PackConfig,
    #[serde(default)]
    pub network: NetworkConfig,
    pub build: BuildConfig,
    #[serde(default)]
    pub java: JavaConfig,
    #[serde(default, rename = "testRun")]
    pub test_run: TestRunConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cleanroom: Option<CleanroomConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackConfig {
    pub name: String,
    pub version: String,
    pub authors: Vec<String>,
    pub minecraft_version: String,
    pub loader: LoaderKind,
    pub loader_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forge_compat_version: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkConfig {
    #[serde(default)]
    pub proxy_ip: String,
    #[serde(default)]
    pub proxy_port: u16,
}

impl NetworkConfig {
    pub fn proxy_url(&self) -> Result<Option<String>, ConfigError> {
        let proxy_ip = self.proxy_ip.trim();
        let has_ip = !proxy_ip.is_empty();
        let has_port = self.proxy_port != 0;
        match (has_ip, has_port) {
            (false, false) => Ok(None),
            (true, true) => Ok(Some(format!("http://{proxy_ip}:{}", self.proxy_port))),
            (true, false) => Err(ConfigError::InvalidProxyPort),
            (false, true) => Err(ConfigError::IncompleteProxy),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildConfig {
    pub output_dir: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JavaConfig {
    pub client: JavaMemoryConfig,
    pub server: JavaMemoryConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JavaMemoryConfig {
    pub min_memory: String,
    pub max_memory: String,
    #[serde(default)]
    pub extra_args: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DownloadSource {
    #[default]
    Auto,
    Official,
    Bmclapi,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestRunConfig {
    pub player_name: String,
    pub download_source: DownloadSource,
    pub java_version_major: u16,
    pub sync_interval_seconds: u64,
    pub debug_extra_jvm_args: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub java_path: String,
}

impl Default for TestRunConfig {
    fn default() -> Self {
        Self {
            player_name: "Player".to_string(),
            download_source: DownloadSource::Auto,
            java_version_major: 8,
            sync_interval_seconds: 30,
            debug_extra_jvm_args: Vec::new(),
            java_path: String::new(),
        }
    }
}

impl Default for JavaConfig {
    fn default() -> Self {
        Self {
            client: JavaMemoryConfig {
                min_memory: "2G".to_string(),
                max_memory: "4G".to_string(),
                extra_args: Vec::new(),
            },
            server: JavaMemoryConfig {
                min_memory: "2G".to_string(),
                max_memory: "4G".to_string(),
                extra_args: Vec::new(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CleanroomConfig {
    pub use_scalar: bool,
    pub scalar_version: u8,
    #[serde(default)]
    pub relauncher: CleanroomRelauncher,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CleanroomRelauncher {
    #[default]
    Unofficial,
    Original,
    Improved,
}

impl Config {
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let text = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;
        let text = strip_empty_legacy_curseforge_section(&text)
            .map_err(|field| ConfigError::StoredCurseForgeApiKey { field })?;
        toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.display().to_string(),
            source,
        })
    }

    pub fn from_imported_manifest(manifest: &ImportedManifest, language: String) -> Self {
        let (loader_name, loader_version) = manifest.primary_loader_parts();
        let inferred_loader = if manifest.name.to_ascii_lowercase().contains("cleanroom") {
            LoaderKind::Cleanroom
        } else {
            match loader_name.as_str() {
                "forge" => LoaderKind::Forge,
                "fabric" => LoaderKind::Fabric,
                "neoforge" => LoaderKind::NeoForge,
                other => panic!("unsupported mod loader '{other}'"),
            }
        };
        let forge_compat_version =
            (inferred_loader == LoaderKind::Cleanroom).then(|| loader_version.clone());

        let is_cleanroom = inferred_loader == LoaderKind::Cleanroom;

        Self {
            language,
            pack: PackConfig {
                name: manifest.name.clone(),
                version: manifest.version.clone(),
                authors: manifest.authors(),
                minecraft_version: manifest.minecraft.version.clone(),
                loader: inferred_loader,
                loader_version,
                forge_compat_version,
            },
            network: NetworkConfig::default(),
            build: BuildConfig {
                output_dir: "dist".to_string(),
            },
            java: JavaConfig::default(),
            test_run: TestRunConfig::default(),
            cleanroom: is_cleanroom.then_some(CleanroomConfig {
                use_scalar: false,
                scalar_version: 2,
                relauncher: CleanroomRelauncher::Unofficial,
            }),
        }
    }
}

fn strip_empty_legacy_curseforge_section(text: &str) -> Result<String, &'static str> {
    let mut output = Vec::new();
    let mut in_curseforge = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if is_table_header(trimmed) {
            in_curseforge = trimmed == "[curseforge]";
            if in_curseforge {
                continue;
            }
        }
        if in_curseforge {
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if is_empty_legacy_curseforge_key(trimmed) {
                continue;
            }
            return Err("[curseforge]");
        }
        if starts_key(trimmed, "curseforge_api_key") {
            return Err("curseforge_api_key");
        }
        output.push(line);
    }
    Ok(output.join("\n"))
}

fn is_table_header(trimmed: &str) -> bool {
    trimmed.starts_with('[') && trimmed.ends_with(']')
}

fn starts_key(trimmed: &str, key: &str) -> bool {
    trimmed
        .strip_prefix(key)
        .is_some_and(|rest| rest.trim_start().starts_with('='))
}

fn is_empty_legacy_curseforge_key(trimmed: &str) -> bool {
    legacy_key_value(trimmed, "api_key").is_some_and(str::is_empty)
        || legacy_key_value(trimmed, "curseforge_api_key").is_some_and(str::is_empty)
}

fn legacy_key_value<'a>(trimmed: &'a str, key: &str) -> Option<&'a str> {
    let raw = trimmed.strip_prefix(key)?;
    let value = raw.trim_start().strip_prefix('=')?.trim();
    let value = value.split('#').next().unwrap_or_default().trim();
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
}
