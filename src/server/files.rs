use std::collections::HashMap;

use reqwest::Url;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::JavaMemoryConfig;
use crate::dependency::DependencyType;
use crate::path_safety::{portable_path_identity, validate_portable_relative_path};
use crate::project::jvm_args::split_jvm_argument_entries;
use crate::server::loader::{LoaderOutput, ServerInstallLoader, ServerLoaderKind};

const MAX_MANIFEST_BYTES: usize = 8 * 1024 * 1024;
const MAX_FILES: usize = 20_000;
const MAX_ARGUMENTS: usize = 512;
const MAX_ARGUMENT_BYTES: usize = 8 * 1024;
const MAX_NAME_BYTES: usize = 512;
const MAX_VERSION_BYTES: usize = 128;
const MAX_PATH_BYTES: usize = 1024;
const MAX_URL_BYTES: usize = 4096;
const MAX_API_KEY_BYTES: usize = 4096;
const MAX_MANIFEST_FILE_BYTES: u64 = 64 * 1024 * 1024 * 1024;

/// Complete `server-install.json` schema version one consumed by MCServerDownloadTool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerInstallManifest {
    /// Schema discriminator required by MCServerDownloadTool schema version one.
    pub schema_version: u32,
    /// Exact Minecraft version to install.
    pub minecraft: ServerMinecraftConfig,
    /// Java runtime and launch settings.
    pub java: ServerJavaConfig,
    /// Official loader installer and exact expected output.
    pub loader: ServerInstallLoader,
    /// CurseForge key, present only when an automatic ForgeCDN download needs it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub curseforge_api_key: Option<String>,
    /// Files downloaded or manually supplied beneath the server root.
    pub files: Vec<ServerFile>,
}

impl ServerInstallManifest {
    /// Validates the semantic invariants enforced by MCServerDownloadTool schema version one.
    pub fn validate(&self) -> Result<(), ServerManifestError> {
        if self.schema_version != 1 {
            return Err(ServerManifestError::InvalidField {
                field: "schema_version".to_string(),
                reason: format!("expected 1, found {}", self.schema_version),
            });
        }
        validate_minecraft_version(&self.minecraft.version)?;
        self.java.validate()?;
        validate_loader(&self.loader, &self.minecraft.version)?;
        if self.files.len() > MAX_FILES {
            return invalid("files", format!("must contain at most {MAX_FILES} entries"));
        }
        let mut targets = HashMap::<String, String>::with_capacity(self.files.len() + 2);
        for (field, path) in loader_output_paths(&self.loader.output) {
            register_target(&mut targets, field, path)?;
        }
        for (index, file) in self.files.iter().enumerate() {
            file.validate(index)?;
            register_target(
                &mut targets,
                format!("files[{index}].path"),
                file.path.clone(),
            )?;
        }
        let requires_key = self
            .files
            .iter()
            .any(|file| file.download.requires_curseforge_key());
        match (requires_key, self.curseforge_api_key.as_deref()) {
            (true, None) => Err(ServerManifestError::CurseForgeKeyRequired),
            (true, Some(key)) => validate_secret(key),
            (false, Some(_)) => Err(ServerManifestError::UnusedCurseForgeKey),
            (false, None) => Ok(()),
        }?;
        let serialized =
            serde_json::to_vec(self).map_err(|source| ServerManifestError::InvalidField {
                field: "manifest".to_string(),
                reason: source.to_string(),
            })?;
        if serialized.len() > MAX_MANIFEST_BYTES {
            return invalid(
                "manifest",
                format!("must not exceed {MAX_MANIFEST_BYTES} bytes"),
            );
        }
        Ok(())
    }
}

/// Exact Minecraft release selected for installation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerMinecraftConfig {
    /// Minecraft version from the pack configuration.
    pub version: String,
}

/// Java runtime, heap, and argument contract for the generated server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerJavaConfig {
    /// Required Java feature-release number.
    pub major: u16,
    /// Initial heap size in exact mebibytes.
    pub min_memory_mb: u32,
    /// Maximum heap size in exact mebibytes.
    pub max_memory_mb: u32,
    /// JVM arguments placed before the server launch target.
    pub jvm_args: Vec<String>,
    /// Arguments passed to the Minecraft server.
    pub server_args: Vec<String>,
}

impl ServerJavaConfig {
    /// Converts project Java settings into the installer's strict numeric representation.
    pub fn from_memory_config(
        major: u16,
        memory: &JavaMemoryConfig,
    ) -> Result<Self, ServerMemoryError> {
        if major == 0 || major > 255 {
            return Err(ServerMemoryError::InvalidJavaMajor { major });
        }
        let min_memory_mb = memory_mebibytes("java.server.min_memory", &memory.min_memory)?;
        let max_memory_mb = memory_mebibytes("java.server.max_memory", &memory.max_memory)?;
        if min_memory_mb > max_memory_mb {
            return Err(ServerMemoryError::InvertedRange {
                min_memory_mb,
                max_memory_mb,
            });
        }
        Ok(Self {
            major,
            min_memory_mb,
            max_memory_mb,
            jvm_args: split_jvm_argument_entries(&memory.extra_args),
            server_args: vec!["nogui".to_string()],
        })
    }

    fn validate(&self) -> Result<(), ServerManifestError> {
        if self.major == 0 || self.major > 255 {
            return invalid("java.major", "must be between 1 and 255");
        }
        if self.min_memory_mb == 0 || self.max_memory_mb == 0 {
            return invalid("java", "memory values must be greater than zero");
        }
        if self.min_memory_mb > self.max_memory_mb {
            return invalid("java", "minimum memory must not exceed maximum memory");
        }
        validate_arguments("java.jvm_args", &self.jvm_args)?;
        validate_arguments("java.server_args", &self.server_args)
    }
}

/// One file materialized by MCServerDownloadTool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerFile {
    /// Human-readable project name used in manual-download reports.
    pub name: String,
    /// Installer file category.
    #[serde(rename = "type")]
    pub dependency_type: DependencyType,
    /// Slash-separated target path below the server root.
    pub path: String,
    /// Automatic URL or explicit manual acquisition mode.
    pub download: ServerFileDownload,
    /// CurseForge project file page shown for manual acquisition.
    pub project_page: String,
    /// Exact SHA-1 digest of the required file.
    pub sha1: String,
    /// Exact non-zero file size.
    pub size: u64,
}

impl ServerFile {
    fn validate(&self, index: usize) -> Result<(), ServerManifestError> {
        validate_bounded_text(&format!("files[{index}].name"), &self.name, MAX_NAME_BYTES)?;
        validate_relative_path(&format!("files[{index}].path"), &self.path)?;
        self.download.validate(index)?;
        validate_project_page(
            &format!("files[{index}].project_page"),
            &self.project_page,
            &self.dependency_type,
        )?;
        if self.sha1.len() != 40 || !self.sha1.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(ServerManifestError::InvalidField {
                field: format!("files[{index}].sha1"),
                reason: "SHA-1 must contain exactly 40 hexadecimal characters".to_string(),
            });
        }
        if self.size == 0 || self.size > MAX_MANIFEST_FILE_BYTES {
            return Err(ServerManifestError::InvalidField {
                field: format!("files[{index}].size"),
                reason: format!("must be between 1 and {MAX_MANIFEST_FILE_BYTES} bytes"),
            });
        }
        Ok(())
    }
}

/// Acquisition mode understood by MCServerDownloadTool schema v1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase", deny_unknown_fields)]
pub enum ServerFileDownload {
    /// Download the file from the validated URL.
    Automatic {
        /// Exact artifact URL supplied by CurseForge.
        url: String,
    },
    /// Require the user to provide the file and verify it locally.
    Manual,
}

impl ServerFileDownload {
    /// Reports whether this download requires the CurseForge API key.
    pub fn requires_curseforge_key(&self) -> bool {
        let Self::Automatic { url } = self else {
            return false;
        };
        Url::parse(url)
            .ok()
            .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
            .is_some_and(|host| host == "edge.forgecdn.net")
    }

    fn validate(&self, index: usize) -> Result<(), ServerManifestError> {
        if let Self::Automatic { url } = self {
            validate_forgecdn_url(&format!("files[{index}].download.url"), url)?;
        }
        Ok(())
    }
}

/// Fail-fast semantic errors for the generated server installer manifest.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ServerManifestError {
    /// A field would be rejected by MCServerDownloadTool schema version one.
    #[error("invalid server-install.json field '{field}': {reason}")]
    InvalidField { field: String, reason: String },
    /// Two declarations would materialize the same cross-platform target.
    #[error("server-install.json target '{path}' is declared by both {first} and {second}")]
    DuplicateTarget {
        first: String,
        second: String,
        path: String,
    },
    /// ForgeCDN authentication is mandatory for at least one automatic file.
    #[error("server-install.json requires a CurseForge API key for an automatic ForgeCDN file")]
    CurseForgeKeyRequired,
    /// Schema version one rejects a key when no automatic ForgeCDN file consumes it.
    #[error("server-install.json contains an unused CurseForge API key")]
    UnusedCurseForgeKey,
}

fn validate_non_blank(field: &str, value: &str) -> Result<(), ServerManifestError> {
    if value.trim().is_empty() {
        return Err(ServerManifestError::InvalidField {
            field: field.to_string(),
            reason: "must not be blank".to_string(),
        });
    }
    Ok(())
}

fn invalid(field: impl Into<String>, reason: impl Into<String>) -> Result<(), ServerManifestError> {
    Err(ServerManifestError::InvalidField {
        field: field.into(),
        reason: reason.into(),
    })
}

fn validate_bounded_text(
    field: &str,
    value: &str,
    max_bytes: usize,
) -> Result<(), ServerManifestError> {
    validate_non_blank(field, value)?;
    if value.len() > max_bytes {
        return invalid(field, format!("must not exceed {max_bytes} UTF-8 bytes"));
    }
    if value.chars().any(char::is_control) {
        return invalid(field, "control characters are forbidden");
    }
    Ok(())
}

fn validate_secret(value: &str) -> Result<(), ServerManifestError> {
    validate_non_blank("curseforge_api_key", value)?;
    if value.len() > MAX_API_KEY_BYTES {
        return invalid(
            "curseforge_api_key",
            format!("must not exceed {MAX_API_KEY_BYTES} UTF-8 bytes"),
        );
    }
    if value.contains(['\r', '\n', '\0']) {
        return Err(ServerManifestError::InvalidField {
            field: "curseforge_api_key".to_string(),
            reason: "NUL and line breaks are forbidden".to_string(),
        });
    }
    Ok(())
}

fn validate_https_url(field: &str, value: &str) -> Result<Url, ServerManifestError> {
    if value.len() > MAX_URL_BYTES {
        return Err(ServerManifestError::InvalidField {
            field: field.to_string(),
            reason: format!("must not exceed {MAX_URL_BYTES} UTF-8 bytes"),
        });
    }
    let url = Url::parse(value).map_err(|source| ServerManifestError::InvalidField {
        field: field.to_string(),
        reason: source.to_string(),
    })?;
    if url.scheme() != "https" || url.host_str().is_none() {
        return Err(ServerManifestError::InvalidField {
            field: field.to_string(),
            reason: "must be an absolute HTTPS URL".to_string(),
        });
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ServerManifestError::InvalidField {
            field: field.to_string(),
            reason: "credentials, explicit ports, queries, and fragments are forbidden".to_string(),
        });
    }
    Ok(url)
}

fn validate_relative_path(field: &str, value: &str) -> Result<(), ServerManifestError> {
    if value.len() > MAX_PATH_BYTES {
        return invalid(
            field,
            format!("must not exceed {MAX_PATH_BYTES} UTF-8 bytes"),
        );
    }
    validate_portable_relative_path(value).map_err(|error| ServerManifestError::InvalidField {
        field: field.to_string(),
        reason: error.to_string(),
    })
}

fn register_target(
    targets: &mut HashMap<String, String>,
    field: impl Into<String>,
    path: String,
) -> Result<(), ServerManifestError> {
    let field = field.into();
    validate_relative_path(&field, &path)?;
    let identity =
        portable_path_identity(&path).map_err(|error| ServerManifestError::InvalidField {
            field: field.clone(),
            reason: error.to_string(),
        })?;
    if let Some(existing) = targets.insert(identity, field.clone()) {
        return Err(ServerManifestError::DuplicateTarget {
            first: existing,
            second: field,
            path,
        });
    }
    Ok(())
}

fn validate_arguments(field: &str, values: &[String]) -> Result<(), ServerManifestError> {
    if values.len() > MAX_ARGUMENTS {
        return invalid(
            field,
            format!("must contain at most {MAX_ARGUMENTS} entries"),
        );
    }
    for (index, value) in values.iter().enumerate() {
        let item = format!("{field}[{index}]");
        validate_bounded_text(&item, value, MAX_ARGUMENT_BYTES)?;
        if value.contains(['\r', '\n', '\0']) {
            return invalid(item, "NUL and line breaks are forbidden");
        }
    }
    Ok(())
}

fn validate_minecraft_version(value: &str) -> Result<(), ServerManifestError> {
    if value.len() > MAX_VERSION_BYTES {
        return invalid(
            "minecraft.version",
            format!("must not exceed {MAX_VERSION_BYTES} UTF-8 bytes"),
        );
    }
    let components = value.split('.').collect::<Vec<_>>();
    if !(2..=3).contains(&components.len())
        || components[0] != "1"
        || components.iter().any(|component| {
            component.is_empty()
                || !component.bytes().all(|byte| byte.is_ascii_digit())
                || (component.len() > 1 && component.starts_with('0'))
        })
    {
        return invalid(
            "minecraft.version",
            "must use numeric form 1.<minor> or 1.<minor>.<patch>",
        );
    }
    Ok(())
}

fn validate_maven_segment(field: &str, value: &str) -> Result<(), ServerManifestError> {
    if value.is_empty()
        || value.len() > MAX_VERSION_BYTES
        || value == "."
        || value == ".."
        || value.contains("..")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'+'))
    {
        return invalid(
            field,
            "must be one safe Maven segment containing only ASCII letters, digits, '.', '-', '_', or '+'",
        );
    }
    Ok(())
}

fn validate_forgecdn_url(field: &str, value: &str) -> Result<(), ServerManifestError> {
    let url = validate_https_url(field, value)?;
    if !exact_origin(&url, "edge.forgecdn.net") {
        return invalid(field, "must use exact https://edge.forgecdn.net origin");
    }
    let segments = url
        .path_segments()
        .map(Iterator::collect::<Vec<_>>)
        .unwrap_or_default();
    if segments.len() != 4
        || segments[0] != "files"
        || !decimal_id(segments[1])
        || !decimal_id(segments[2])
        || segments[3].is_empty()
        || segments[3] == "."
        || segments[3] == ".."
        || segments[3].chars().any(char::is_control)
    {
        return invalid(field, "must match /files/<id>/<subId>/<filename>");
    }
    Ok(())
}

fn validate_project_page(
    field: &str,
    value: &str,
    dependency_type: &DependencyType,
) -> Result<(), ServerManifestError> {
    let url = validate_https_url(field, value)?;
    if !exact_origin(&url, "www.curseforge.com") {
        return invalid(field, "must use exact https://www.curseforge.com origin");
    }
    let segments = url
        .path_segments()
        .map(Iterator::collect::<Vec<_>>)
        .unwrap_or_default();
    let category = dependency_type.curseforge_web_category();
    if segments.len() != 5
        || segments[0] != "minecraft"
        || segments[1] != category
        || !curseforge_slug(segments[2])
        || segments[3] != "files"
        || !decimal_id(segments[4])
    {
        return invalid(
            field,
            format!("must match /minecraft/{category}/<slug>/files/<fileId>"),
        );
    }
    Ok(())
}

fn validate_loader(
    loader: &ServerInstallLoader,
    minecraft_version: &str,
) -> Result<(), ServerManifestError> {
    validate_maven_segment("loader.version", &loader.version)?;
    let installer = validate_https_url("loader.installer.url", &loader.installer.url)?;
    let sidecar = validate_https_url(
        "loader.installer.sha1_sidecar",
        &loader.installer.sha1_sidecar,
    )?;
    if origin(&installer) != origin(&sidecar)
        || sidecar.as_str() != format!("{}.sha1", installer.as_str())
    {
        return invalid(
            "loader.installer.sha1_sidecar",
            "must be the exact same-origin installer URL with a .sha1 suffix",
        );
    }
    let expected_url = match loader.kind {
        ServerLoaderKind::Forge => format!(
            "https://maven.minecraftforge.net/net/minecraftforge/forge/{minecraft_version}-{0}/forge-{minecraft_version}-{0}-installer.jar",
            loader.version
        ),
        ServerLoaderKind::Fabric => loader.installer.url.clone(),
        ServerLoaderKind::NeoForge => format!(
            "https://maven.neoforged.net/releases/net/neoforged/neoforge/{0}/neoforge-{0}-installer.jar",
            loader.version
        ),
        ServerLoaderKind::Cleanroom => {
            let repository = if loader.version.to_ascii_lowercase().contains("snapshot") {
                "snapshots"
            } else {
                "releases"
            };
            format!(
                "https://repo.cleanroommc.com/{repository}/com/cleanroommc/cleanroom/{0}/cleanroom-{0}-installer.jar",
                loader.version
            )
        }
    };
    if loader.kind == ServerLoaderKind::Fabric {
        let segments = installer
            .path_segments()
            .map(Iterator::collect::<Vec<_>>)
            .unwrap_or_default();
        if !exact_origin(&installer, "maven.fabricmc.net")
            || segments.len() != 5
            || segments[..3] != ["net", "fabricmc", "fabric-installer"]
            || validate_maven_segment("loader.installer.version", segments[3]).is_err()
            || segments[4] != format!("fabric-installer-{}.jar", segments[3])
        {
            return invalid(
                "loader.installer.url",
                "must use the official Fabric installer Maven coordinate",
            );
        }
    } else if loader.installer.url != expected_url {
        return invalid(
            "loader.installer.url",
            "does not match the official loader Maven coordinate",
        );
    }
    for (field, path) in loader_output_paths(&loader.output) {
        validate_relative_path(&field, &path)?;
    }
    let expected_output = match loader.kind {
        ServerLoaderKind::Fabric => LoaderOutput::ExactJar {
            path: "fabric-server-launch.jar".to_string(),
            main_class: Some(
                "net.fabricmc.loader.impl.launch.server.FabricServerLauncher".to_string(),
            ),
        },
        ServerLoaderKind::NeoForge => {
            let base = format!("libraries/net/neoforged/neoforge/{}", loader.version);
            LoaderOutput::ModernArgs {
                windows: format!("{base}/win_args.txt"),
                unix: format!("{base}/unix_args.txt"),
            }
        }
        ServerLoaderKind::Cleanroom => LoaderOutput::ExactJar {
            // Match the filename emitted by Cleanroom's installServer task.
            path: format!("cleanroom-{}.jar", loader.version),
            main_class: None,
        },
        ServerLoaderKind::Forge if modern_minecraft(minecraft_version)? => {
            let base = format!(
                "libraries/net/minecraftforge/forge/{minecraft_version}-{}",
                loader.version
            );
            LoaderOutput::ModernArgs {
                windows: format!("{base}/win_args.txt"),
                unix: format!("{base}/unix_args.txt"),
            }
        }
        ServerLoaderKind::Forge => LoaderOutput::ExactJar {
            path: format!("forge-{minecraft_version}-{}.jar", loader.version),
            main_class: None,
        },
    };
    if loader.output != expected_output {
        return invalid(
            "loader.output",
            "does not match the exact output for the selected loader and versions",
        );
    }
    Ok(())
}

fn loader_output_paths(output: &LoaderOutput) -> Vec<(String, String)> {
    match output {
        LoaderOutput::ModernArgs { windows, unix } => vec![
            ("loader.output.windows".to_string(), windows.clone()),
            ("loader.output.unix".to_string(), unix.clone()),
        ],
        LoaderOutput::ExactJar { path, .. } => {
            vec![("loader.output.path".to_string(), path.clone())]
        }
    }
}

fn exact_origin(url: &Url, host: &str) -> bool {
    url.scheme() == "https"
        && url
            .host_str()
            .is_some_and(|value| value.eq_ignore_ascii_case(host))
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
}

fn origin(url: &Url) -> (String, String, Option<u16>) {
    (
        url.scheme().to_string(),
        url.host_str().unwrap_or_default().to_ascii_lowercase(),
        url.port(),
    )
}

fn decimal_id(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn curseforge_slug(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_NAME_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn modern_minecraft(value: &str) -> Result<bool, ServerManifestError> {
    validate_minecraft_version(value)?;
    let mut parts = value.split('.');
    let major = parts
        .next()
        .unwrap_or_default()
        .parse::<u32>()
        .map_err(|source| ServerManifestError::InvalidField {
            field: "minecraft.version".to_string(),
            reason: source.to_string(),
        })?;
    let minor = parts
        .next()
        .unwrap_or_default()
        .parse::<u32>()
        .map_err(|source| ServerManifestError::InvalidField {
            field: "minecraft.version".to_string(),
            reason: source.to_string(),
        })?;
    Ok((major, minor) >= (1, 17))
}

/// Fail-fast Java heap conversion errors.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ServerMemoryError {
    /// Java feature releases supported by the installer are 1 through 255.
    #[error("java.version_major must be between 1 and 255, found {major}")]
    InvalidJavaMajor { major: u16 },
    /// Memory values must be a positive integer followed by K, M, or G.
    #[error("{field} must be a positive integer followed by K, M, or G, found '{value}'")]
    InvalidFormat { field: &'static str, value: String },
    /// Kibibytes that do not form a whole mebibyte cannot be represented by schema v1.
    #[error("{field} value '{value}' is not an exact whole number of MiB")]
    FractionalMebibyte { field: &'static str, value: String },
    /// The converted number cannot fit the schema's u32 field.
    #[error("{field} value '{value}' exceeds the server installer MiB limit")]
    Overflow { field: &'static str, value: String },
    /// Initial heap must not exceed maximum heap.
    #[error("server minimum memory {min_memory_mb} MiB exceeds maximum memory {max_memory_mb} MiB")]
    InvertedRange {
        min_memory_mb: u32,
        max_memory_mb: u32,
    },
}

/// Parses a Java-style integer K/M/G heap value into exact non-zero MiB.
pub fn memory_mebibytes(field: &'static str, value: &str) -> Result<u32, ServerMemoryError> {
    let (digits, suffix) = value.split_at(value.len().checked_sub(1).ok_or_else(|| {
        ServerMemoryError::InvalidFormat {
            field,
            value: value.to_string(),
        }
    })?);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ServerMemoryError::InvalidFormat {
            field,
            value: value.to_string(),
        });
    }
    let amount = digits
        .parse::<u64>()
        .map_err(|_| ServerMemoryError::Overflow {
            field,
            value: value.to_string(),
        })?;
    if amount == 0 {
        return Err(ServerMemoryError::InvalidFormat {
            field,
            value: value.to_string(),
        });
    }
    let mebibytes = match suffix.to_ascii_uppercase().as_str() {
        "K" if amount % 1024 == 0 => amount / 1024,
        "K" => {
            return Err(ServerMemoryError::FractionalMebibyte {
                field,
                value: value.to_string(),
            });
        }
        "M" => amount,
        "G" => amount
            .checked_mul(1024)
            .ok_or_else(|| ServerMemoryError::Overflow {
                field,
                value: value.to_string(),
            })?,
        _ => {
            return Err(ServerMemoryError::InvalidFormat {
                field,
                value: value.to_string(),
            });
        }
    };
    u32::try_from(mebibytes).map_err(|_| ServerMemoryError::Overflow {
        field,
        value: value.to_string(),
    })
}
