use std::fs;
use std::path::Path;

use thiserror::Error;

use crate::config::{
    BuildConfig, CleanroomConfig, CleanroomRelauncher, Config, DownloadSource, JavaConfig,
    JavaMemoryConfig, LoaderKind, NetworkConfig, PackConfig, TestRunConfig,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsForm {
    pub language: String,
    pub pack_name: String,
    pub pack_version: String,
    pub authors: String,
    pub minecraft_version: String,
    pub loader: LoaderKind,
    pub loader_version: String,
    pub forge_compat_version: String,
    pub proxy_ip: String,
    pub proxy_port: String,
    pub output_dir: String,
    pub client_min_memory: String,
    pub client_max_memory: String,
    pub client_extra_args: String,
    pub server_min_memory: String,
    pub server_max_memory: String,
    pub server_extra_args: String,
    pub test_player_name: String,
    pub test_download_source: DownloadSource,
    pub test_java_version_major: String,
    pub test_sync_interval_seconds: String,
    pub test_debug_extra_jvm_args: String,
    pub test_java_path: String,
    pub cleanroom_enabled: bool,
    pub cleanroom_use_scalar: bool,
    pub cleanroom_scalar_version: String,
    pub cleanroom_relauncher: CleanroomRelauncher,
}

#[derive(Debug, Error)]
pub enum SettingsError {
    #[error(transparent)]
    Config(#[from] crate::config::ConfigError),
    #[error("failed to serialize config.toml: {0}")]
    SerializeToml(#[from] toml::ser::Error),
    #[error("failed to write config file '{path}': {source}")]
    Write {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("[network].proxy_port must be empty or between 1 and 65535")]
    InvalidProxyPort,
    #[error("[testRun].java_version_major must be between 1 and 255")]
    InvalidJavaVersion,
    #[error("[testRun].sync_interval_seconds must be greater than 0")]
    InvalidSyncInterval,
    #[error("[cleanroom].scalar_version only supports 2 or 3")]
    InvalidScalarVersion,
}

impl SettingsForm {
    pub fn load(project_dir: impl AsRef<Path>) -> Result<Self, SettingsError> {
        let config = Config::from_path(project_dir.as_ref().join("config.toml"))?;
        Ok(Self::from_config(config))
    }

    pub fn save(&self, project_dir: impl AsRef<Path>) -> Result<(), SettingsError> {
        let config = self.to_config()?;
        let path = project_dir.as_ref().join("config.toml");
        let text = toml::to_string_pretty(&config)?;
        fs::write(&path, text).map_err(|source| SettingsError::Write {
            path: path.display().to_string(),
            source,
        })
    }

    pub fn from_config(config: Config) -> Self {
        Self {
            language: config.language,
            pack_name: config.pack.name,
            pack_version: config.pack.version,
            authors: config.pack.authors.join(", "),
            minecraft_version: config.pack.minecraft_version,
            loader: config.pack.loader,
            loader_version: config.pack.loader_version,
            forge_compat_version: config.pack.forge_compat_version.unwrap_or_default(),
            proxy_ip: config.network.proxy_ip,
            proxy_port: if config.network.proxy_port == 0 {
                String::new()
            } else {
                config.network.proxy_port.to_string()
            },
            output_dir: config.build.output_dir,
            client_min_memory: config.java.client.min_memory,
            client_max_memory: config.java.client.max_memory,
            client_extra_args: join_lines(&config.java.client.extra_args),
            server_min_memory: config.java.server.min_memory,
            server_max_memory: config.java.server.max_memory,
            server_extra_args: join_lines(&config.java.server.extra_args),
            test_player_name: config.test_run.player_name,
            test_download_source: config.test_run.download_source,
            test_java_version_major: config.test_run.java_version_major.to_string(),
            test_sync_interval_seconds: config.test_run.sync_interval_seconds.to_string(),
            test_debug_extra_jvm_args: join_lines(&config.test_run.debug_extra_jvm_args),
            test_java_path: config.test_run.java_path,
            cleanroom_enabled: config.cleanroom.is_some(),
            cleanroom_use_scalar: config
                .cleanroom
                .as_ref()
                .is_some_and(|cleanroom| cleanroom.use_scalar),
            cleanroom_scalar_version: config
                .cleanroom
                .as_ref()
                .map(|cleanroom| cleanroom.scalar_version.to_string())
                .unwrap_or_else(|| "2".to_string()),
            cleanroom_relauncher: config
                .cleanroom
                .as_ref()
                .map(|cleanroom| cleanroom.relauncher)
                .unwrap_or_default(),
        }
    }

    pub fn to_config(&self) -> Result<Config, SettingsError> {
        let proxy_port = parse_optional_port(&self.proxy_port)?;
        let java_version_major = parse_java_version(&self.test_java_version_major)?;
        let sync_interval_seconds = parse_sync_interval(&self.test_sync_interval_seconds)?;
        let scalar_version = parse_scalar_version(&self.cleanroom_scalar_version)?;
        Ok(Config {
            language: self.language.trim().to_string(),
            pack: PackConfig {
                name: self.pack_name.trim().to_string(),
                version: self.pack_version.trim().to_string(),
                authors: split_list(&self.authors),
                minecraft_version: self.minecraft_version.trim().to_string(),
                loader: self.loader.clone(),
                loader_version: self.loader_version.trim().to_string(),
                forge_compat_version: optional_string(&self.forge_compat_version),
            },
            network: NetworkConfig {
                proxy_ip: self.proxy_ip.trim().to_string(),
                proxy_port,
            },
            build: BuildConfig {
                output_dir: self.output_dir.trim().to_string(),
            },
            java: JavaConfig {
                client: JavaMemoryConfig {
                    min_memory: self.client_min_memory.trim().to_string(),
                    max_memory: self.client_max_memory.trim().to_string(),
                    extra_args: split_lines(&self.client_extra_args),
                },
                server: JavaMemoryConfig {
                    min_memory: self.server_min_memory.trim().to_string(),
                    max_memory: self.server_max_memory.trim().to_string(),
                    extra_args: split_lines(&self.server_extra_args),
                },
            },
            test_run: TestRunConfig {
                player_name: self.test_player_name.trim().to_string(),
                download_source: self.test_download_source,
                java_version_major,
                sync_interval_seconds,
                debug_extra_jvm_args: split_lines(&self.test_debug_extra_jvm_args),
                java_path: self.test_java_path.trim().to_string(),
            },
            cleanroom: self.cleanroom_enabled.then_some(CleanroomConfig {
                use_scalar: self.cleanroom_use_scalar,
                scalar_version,
                relauncher: self.cleanroom_relauncher,
            }),
        })
    }
}

fn parse_optional_port(raw: &str) -> Result<u16, SettingsError> {
    let value = raw.trim();
    if value.is_empty() {
        return Ok(0);
    }
    value
        .parse::<u16>()
        .ok()
        .filter(|port| *port > 0)
        .ok_or(SettingsError::InvalidProxyPort)
}

fn parse_java_version(raw: &str) -> Result<u16, SettingsError> {
    raw.trim()
        .parse::<u16>()
        .ok()
        .filter(|version| *version > 0 && *version <= 255)
        .ok_or(SettingsError::InvalidJavaVersion)
}

fn parse_sync_interval(raw: &str) -> Result<u64, SettingsError> {
    raw.trim()
        .parse::<u64>()
        .ok()
        .filter(|seconds| *seconds > 0)
        .ok_or(SettingsError::InvalidSyncInterval)
}

fn parse_scalar_version(raw: &str) -> Result<u8, SettingsError> {
    let value = raw
        .trim()
        .parse::<u8>()
        .map_err(|_| SettingsError::InvalidScalarVersion)?;
    if matches!(value, 2 | 3) {
        Ok(value)
    } else {
        Err(SettingsError::InvalidScalarVersion)
    }
}

fn split_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

fn split_lines(raw: &str) -> Vec<String> {
    raw.lines()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

fn join_lines(values: &[String]) -> String {
    values.join("\n")
}

fn optional_string(raw: &str) -> Option<String> {
    let value = raw.trim();
    (!value.is_empty()).then(|| value.to_string())
}
