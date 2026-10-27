use std::fs;
use std::path::{Path, PathBuf};

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
    #[error("[java].version_major must be between 1 and 255, got {value}")]
    InvalidJavaVersion { value: u16 },
    #[error("missing required [java].version_major in config.toml")]
    MissingJavaVersion,
    #[error("local test-run sync_interval_seconds must be greater than 0")]
    InvalidTestRunSyncInterval,
    #[error("[pack].loader_version must not be empty")]
    MissingLoaderVersion,
    #[error("[pack].minecraft_version must not be empty")]
    MissingMinecraftVersion,
    #[error("[pack].{field} is not a safe cross-platform archive filename component: {reason}")]
    InvalidPackFilename { field: &'static str, reason: String },
    #[error("[build].output_dir must remain inside the project directory: {reason}")]
    InvalidBuildOutputDir { reason: String },
    #[error("Cleanroom requires [pack].forge_compat_version")]
    MissingCleanroomForgeCompatVersion,
    #[error("[pack].forge_compat_version is only valid when [pack].loader = 'cleanroom'")]
    UnexpectedForgeCompatVersion,
    #[error("failed to write local test-run config '{path}': {source}")]
    WriteLocalRun {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to serialize local test-run config: {0}")]
    SerializeLocalRun(#[from] toml::ser::Error),
    #[error("failed to write config file '{path}': {source}")]
    WriteConfig {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "{field} is no longer supported in config.toml. Move CurseForge API keys to --curseforge-api-key, runtime environment variable MCMODPACKUTIL_CURSEFORGE_API_KEY, CURSEFORGE_API_KEY, CF_API_KEY, or an official build with an embedded key."
    )]
    StoredCurseForgeApiKey { field: &'static str },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
    pub java: JavaConfig,
    #[serde(skip)]
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
    pub version_major: u16,
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
            sync_interval_seconds: 30,
            debug_extra_jvm_args: Vec::new(),
            java_path: String::new(),
        }
    }
}

impl TestRunConfig {
    pub fn path(project_dir: impl AsRef<Path>) -> PathBuf {
        project_dir.as_ref().join("test-run.toml")
    }

    pub fn from_project(project_dir: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = Self::path(project_dir);
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(&path).map_err(|source| ConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;
        let config: Self = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.display().to_string(),
            source,
        })?;
        config.validate()?;
        Ok(config)
    }

    pub fn write_to_project(&self, project_dir: impl AsRef<Path>) -> Result<(), ConfigError> {
        self.validate()?;
        let path = Self::path(project_dir);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| ConfigError::WriteLocalRun {
                path: parent.display().to_string(),
                source,
            })?;
        }
        let text = toml::to_string_pretty(self)?;
        fs::write(&path, text).map_err(|source| ConfigError::WriteLocalRun {
            path: path.display().to_string(),
            source,
        })
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.sync_interval_seconds == 0 {
            return Err(ConfigError::InvalidTestRunSyncInterval);
        }
        Ok(())
    }
}

impl Default for JavaConfig {
    fn default() -> Self {
        Self {
            version_major: 8,
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
        let mut value =
            toml::from_str::<toml::Value>(&text).map_err(|source| ConfigError::Parse {
                path: path.display().to_string(),
                source,
            })?;
        let migration = migrate_legacy_java_version(path, &mut value)?;
        let config: Self = value
            .clone()
            .try_into()
            .map_err(|source| ConfigError::Parse {
                path: path.display().to_string(),
                source,
            })?;
        config.validate()?;
        if let Some(migration) = migration {
            let config_text = toml::to_string_pretty(&value)?;
            fs::write(path, config_text).map_err(|source| ConfigError::WriteConfig {
                path: path.display().to_string(),
                source,
            })?;
            fs::write(&migration.test_run_path, migration.test_run_text).map_err(|source| {
                ConfigError::WriteLocalRun {
                    path: migration.test_run_path.display().to_string(),
                    source,
                }
            })?;
        }
        Ok(config)
    }

    /// Loads recognized configuration values for the settings editor.
    /// Missing, malformed, unknown, or incorrectly typed values leave the
    /// corresponding editable default unchanged. CLI operations must use
    /// [`Self::from_path`] and keep their strict validation behavior.
    pub fn from_path_for_settings(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let text = match fs::read_to_string(path) {
            Ok(text) => Some(text),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => None,
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.display().to_string(),
                    source,
                });
            }
        };
        let mut config = settings_defaults();
        let canonical_java_version_present = text
            .as_ref()
            .and_then(|text| toml::from_str::<toml::Value>(text).ok())
            .as_ref()
            .and_then(toml::Value::as_table)
            .map(|root| apply_settings_fields(&mut config, root))
            .unwrap_or(false);
        if !canonical_java_version_present
            && let Some(version) = legacy_settings_java_version(path)?
        {
            config.java.version_major = version;
        }
        Ok(config)
    }

    /// Validates configuration invariants that cannot be represented by TOML types.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !(1..=255).contains(&self.java.version_major) {
            return Err(ConfigError::InvalidJavaVersion {
                value: self.java.version_major,
            });
        }
        if self.pack.minecraft_version.trim().is_empty() {
            return Err(ConfigError::MissingMinecraftVersion);
        }
        if self.pack.loader_version.trim().is_empty() {
            return Err(ConfigError::MissingLoaderVersion);
        }
        for (field, value) in [("name", &self.pack.name), ("version", &self.pack.version)] {
            crate::path_safety::validate_portable_component(value).map_err(|error| {
                ConfigError::InvalidPackFilename {
                    field,
                    reason: error.to_string(),
                }
            })?;
        }
        crate::path_safety::validate_project_relative_directory(&self.build.output_dir).map_err(
            |error| ConfigError::InvalidBuildOutputDir {
                reason: error.to_string(),
            },
        )?;
        match self.pack.loader {
            LoaderKind::Cleanroom => {
                if self
                    .pack
                    .forge_compat_version
                    .as_deref()
                    .is_none_or(|version| version.trim().is_empty())
                {
                    return Err(ConfigError::MissingCleanroomForgeCompatVersion);
                }
            }
            _ if self.pack.forge_compat_version.is_some() => {
                return Err(ConfigError::UnexpectedForgeCompatVersion);
            }
            _ => {}
        }
        Ok(())
    }

    pub fn from_imported_manifest(
        manifest: &ImportedManifest,
        language: String,
    ) -> Result<Self, ConfigError> {
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
        let config = Self {
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
        };
        config.validate()?;
        Ok(config)
    }
}

fn settings_defaults() -> Config {
    Config {
        language: String::new(),
        pack: PackConfig {
            name: String::new(),
            version: String::new(),
            authors: Vec::new(),
            minecraft_version: String::new(),
            loader: LoaderKind::Forge,
            loader_version: String::new(),
            forge_compat_version: None,
        },
        network: NetworkConfig::default(),
        build: BuildConfig {
            output_dir: "dist".to_string(),
        },
        java: JavaConfig::default(),
        test_run: TestRunConfig::default(),
        cleanroom: None,
    }
}

fn apply_settings_fields(config: &mut Config, root: &toml::map::Map<String, toml::Value>) -> bool {
    if let Some(language) = string_field(root, "language")
        .filter(|language| language.is_empty() || matches!(*language, "zh-CN" | "en-US"))
    {
        config.language = language.to_string();
    }

    if let Some(pack) = table_field(root, "pack") {
        apply_string(pack, "name", &mut config.pack.name);
        apply_string(pack, "version", &mut config.pack.version);
        if let Some(authors) = string_array_field(pack, "authors") {
            config.pack.authors = authors;
        }
        apply_string(
            pack,
            "minecraft_version",
            &mut config.pack.minecraft_version,
        );
        if let Some(loader) = loader_field(pack, "loader") {
            config.pack.loader = loader;
        }
        apply_string(pack, "loader_version", &mut config.pack.loader_version);
        if let Some(forge_compat_version) = string_field(pack, "forge_compat_version") {
            config.pack.forge_compat_version = Some(forge_compat_version.to_string());
        }
    }

    if let Some(network) = table_field(root, "network") {
        apply_string(network, "proxy_ip", &mut config.network.proxy_ip);
        if let Some(port) = integer_field(network, "proxy_port").and_then(valid_u16) {
            config.network.proxy_port = port;
        }
        if config.network.proxy_url().is_err() {
            config.network = NetworkConfig::default();
        }
    }

    if let Some(build) = table_field(root, "build")
        && let Some(output_dir) = string_field(build, "output_dir").filter(|output_dir| {
            crate::path_safety::validate_project_relative_directory(output_dir).is_ok()
        })
    {
        config.build.output_dir = output_dir.to_string();
    }

    let mut canonical_java_version_present = false;
    if let Some(java) = table_field(root, "java") {
        canonical_java_version_present = java.contains_key("version_major");
        if let Some(version) = integer_field(java, "version_major")
            .and_then(valid_u16)
            .filter(|version| *version > 0)
        {
            config.java.version_major = version;
        }
        if let Some(client) = table_field(java, "client") {
            apply_java_memory_fields(&mut config.java.client, client);
        }
        if let Some(server) = table_field(java, "server") {
            apply_java_memory_fields(&mut config.java.server, server);
        }
    }

    if let Some(cleanroom) = table_field(root, "cleanroom") {
        let mut supplied = CleanroomConfig {
            use_scalar: false,
            scalar_version: 2,
            relauncher: CleanroomRelauncher::default(),
        };
        if let Some(use_scalar) = bool_field(cleanroom, "use_scalar") {
            supplied.use_scalar = use_scalar;
        }
        if let Some(scalar_version) = integer_field(cleanroom, "scalar_version")
            .and_then(valid_u8)
            .filter(|version| matches!(version, 2 | 3))
        {
            supplied.scalar_version = scalar_version;
        }
        if let Some(relauncher) = relauncher_field(cleanroom, "relauncher") {
            supplied.relauncher = relauncher;
        }
        config.cleanroom = Some(supplied);
    }

    canonical_java_version_present
}

fn apply_java_memory_fields(
    memory: &mut JavaMemoryConfig,
    supplied: &toml::map::Map<String, toml::Value>,
) {
    apply_string(supplied, "min_memory", &mut memory.min_memory);
    apply_string(supplied, "max_memory", &mut memory.max_memory);
    if let Some(extra_args) = string_array_field(supplied, "extra_args") {
        memory.extra_args = extra_args;
    }
}

fn apply_string(table: &toml::map::Map<String, toml::Value>, key: &str, destination: &mut String) {
    if let Some(value) = string_field(table, key) {
        *destination = value.to_string();
    }
}

fn table_field<'a>(
    table: &'a toml::map::Map<String, toml::Value>,
    key: &str,
) -> Option<&'a toml::map::Map<String, toml::Value>> {
    table.get(key).and_then(toml::Value::as_table)
}

fn string_field<'a>(table: &'a toml::map::Map<String, toml::Value>, key: &str) -> Option<&'a str> {
    table.get(key).and_then(toml::Value::as_str)
}

fn integer_field(table: &toml::map::Map<String, toml::Value>, key: &str) -> Option<i64> {
    table.get(key).and_then(toml::Value::as_integer)
}

fn bool_field(table: &toml::map::Map<String, toml::Value>, key: &str) -> Option<bool> {
    table.get(key).and_then(toml::Value::as_bool)
}

fn string_array_field(
    table: &toml::map::Map<String, toml::Value>,
    key: &str,
) -> Option<Vec<String>> {
    table
        .get(key)?
        .as_array()?
        .iter()
        .map(|value| value.as_str().map(str::to_string))
        .collect()
}

fn loader_field(table: &toml::map::Map<String, toml::Value>, key: &str) -> Option<LoaderKind> {
    match string_field(table, key)? {
        "forge" => Some(LoaderKind::Forge),
        "fabric" => Some(LoaderKind::Fabric),
        "neo-forge" => Some(LoaderKind::NeoForge),
        "cleanroom" => Some(LoaderKind::Cleanroom),
        _ => None,
    }
}

fn relauncher_field(
    table: &toml::map::Map<String, toml::Value>,
    key: &str,
) -> Option<CleanroomRelauncher> {
    match string_field(table, key)? {
        "unofficial" => Some(CleanroomRelauncher::Unofficial),
        "original" => Some(CleanroomRelauncher::Original),
        "improved" => Some(CleanroomRelauncher::Improved),
        _ => None,
    }
}

fn valid_u16(value: i64) -> Option<u16> {
    u16::try_from(value).ok()
}

fn valid_u8(value: i64) -> Option<u8> {
    u8::try_from(value).ok()
}

struct LegacyJavaMigration {
    test_run_path: PathBuf,
    test_run_text: String,
}

fn migrate_legacy_java_version(
    config_path: &Path,
    config: &mut toml::Value,
) -> Result<Option<LegacyJavaMigration>, ConfigError> {
    let java = config
        .get_mut("java")
        .and_then(toml::Value::as_table_mut)
        .ok_or(ConfigError::MissingJavaVersion)?;
    java.remove("server_version_major");
    if java.contains_key("version_major") {
        return Ok(None);
    }

    let project_dir = config_path
        .parent()
        .ok_or(ConfigError::MissingJavaVersion)?;
    let test_run_path = TestRunConfig::path(project_dir);
    let text = fs::read_to_string(&test_run_path).map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            ConfigError::MissingJavaVersion
        } else {
            ConfigError::Read {
                path: test_run_path.display().to_string(),
                source,
            }
        }
    })?;
    let mut test_run =
        toml::from_str::<toml::Value>(&text).map_err(|source| ConfigError::Parse {
            path: test_run_path.display().to_string(),
            source,
        })?;
    let value = test_run
        .as_table_mut()
        .and_then(|table| table.remove("java_version_major"))
        .and_then(|value| value.as_integer())
        .and_then(|value| u16::try_from(value).ok())
        .filter(|value| (1..=255).contains(value))
        .ok_or(ConfigError::MissingJavaVersion)?;
    java.insert(
        "version_major".to_string(),
        toml::Value::Integer(i64::from(value)),
    );
    let test_run_text = toml::to_string_pretty(&test_run)?;
    Ok(Some(LegacyJavaMigration {
        test_run_path,
        test_run_text,
    }))
}

/// Reads the retired local Java field for display without mutating either file.
fn legacy_settings_java_version(config_path: &Path) -> Result<Option<u16>, ConfigError> {
    let Some(project_dir) = config_path.parent() else {
        return Ok(None);
    };
    let test_run_path = TestRunConfig::path(project_dir);
    let text = match fs::read_to_string(&test_run_path) {
        Ok(text) => text,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ConfigError::Read {
                path: test_run_path.display().to_string(),
                source,
            });
        }
    };
    let Ok(value) = toml::from_str::<toml::Value>(&text) else {
        return Ok(None);
    };
    Ok(value
        .as_table()
        .and_then(|table| table.get("java_version_major"))
        .and_then(toml::Value::as_integer)
        .and_then(|value| u16::try_from(value).ok())
        .filter(|value| (1..=255).contains(value)))
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
