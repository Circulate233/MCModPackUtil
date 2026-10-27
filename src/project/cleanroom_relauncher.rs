use std::fs;
use std::io::{Cursor, ErrorKind, Read};
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use thiserror::Error;
use zip::ZipArchive;

use crate::config::{Config, JavaMemoryConfig, LoaderKind};
use crate::dependency::DependencyEntry;
use crate::net::{
    ArtifactRequest, ArtifactTransfer, HttpRequest, HttpTransport, NetworkConfig, NetworkEngine,
};
use crate::project::jvm_args::{JvmArgumentError, java_memory_arguments};

const ORIGINAL_PROJECT_ID: u64 = 1_214_490;
const ORIGINAL_SLUG: &str = "cleanroom-relauncher";
const IMPROVED_PROJECT_ID: u64 = 1_336_201;
const IMPROVED_SLUG: &str = "improved-cleanroom-relauncher";
const UNOFFICIAL_PROJECT_ID: u64 = 1_214_906;
const UNOFFICIAL_SLUG: &str = "cleanroom-relauncher-unofficial";
const MMC_SHA1_SIDECAR_LIMIT: usize = 4 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelauncherVariant {
    Original,
    Improved,
    Unofficial,
}

impl RelauncherVariant {
    fn project_id(self) -> u64 {
        match self {
            Self::Original => ORIGINAL_PROJECT_ID,
            Self::Improved => IMPROVED_PROJECT_ID,
            Self::Unofficial => UNOFFICIAL_PROJECT_ID,
        }
    }

    fn slug(self) -> &'static str {
        match self {
            Self::Original => ORIGINAL_SLUG,
            Self::Improved => IMPROVED_SLUG,
            Self::Unofficial => UNOFFICIAL_SLUG,
        }
    }

    fn config_path(self) -> &'static str {
        match self {
            Self::Original | Self::Improved => "overrides/config/relauncher.json",
            Self::Unofficial => "overrides/config/cleanroom_relauncher.cfg",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedConfig {
    pub variant: RelauncherVariant,
    pub archive_path: &'static str,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedClientFile {
    pub archive_path: String,
    pub bytes: Vec<u8>,
    pub store_uncompressed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedClientFiles {
    pub variant: RelauncherVariant,
    pub files: Vec<GeneratedClientFile>,
}

pub trait MmcPackSource {
    type Error: std::error::Error;

    /// Fetches bounded metadata such as the SHA1 sidecar.
    fn fetch(&self, url: &str) -> Result<Vec<u8>, Self::Error>;

    /// Publishes a checksum-verified MMC archive at `target`.
    ///
    /// The default supports deterministic in-memory test sources. Network-backed
    /// implementations must override this method with streaming transfer.
    fn download_archive(
        &self,
        url: &str,
        target: &Path,
        expected_sha1: &str,
    ) -> Result<(), RelauncherConfigError> {
        let bytes = self
            .fetch(url)
            .map_err(|source| RelauncherConfigError::DownloadMmcPack {
                url: url.to_string(),
                reason: source.to_string(),
            })?;
        verify_mmc_sha1(&bytes, expected_sha1)?;
        write_cache_atomically(target, &bytes)
    }
}

#[derive(Clone)]
pub struct HttpMmcPackSource {
    engine: NetworkEngine,
}

impl HttpMmcPackSource {
    pub fn with_proxy(proxy: Option<&str>) -> Result<Self, RelauncherConfigError> {
        let config = NetworkConfig {
            proxy: proxy.map(str::to_string),
            ..NetworkConfig::default()
        };
        let engine = NetworkEngine::new(config).map_err(|source| {
            RelauncherConfigError::CreateHttpClient {
                reason: source.to_string(),
            }
        })?;
        Ok(Self { engine })
    }
}

impl MmcPackSource for HttpMmcPackSource {
    type Error = crate::net::NetworkError;

    fn fetch(&self, url: &str) -> Result<Vec<u8>, Self::Error> {
        let request = HttpRequest::get(url, MMC_SHA1_SIDECAR_LIMIT).build()?;
        self.engine.get_bytes(request).map(|response| response.body)
    }

    fn download_archive(
        &self,
        url: &str,
        target: &Path,
        expected_sha1: &str,
    ) -> Result<(), RelauncherConfigError> {
        let request = ArtifactRequest::builder("cleanroom-mmc-pack", target, url)
            .expected_sha1(expected_sha1)
            .build()
            .map_err(|source| RelauncherConfigError::DownloadMmcPack {
                url: url.to_string(),
                reason: source.to_string(),
            })?;
        self.engine
            .transfer_one(request, Arc::new(|_| {}))
            .map(|_| ())
            .map_err(|source| RelauncherConfigError::DownloadMmcPack {
                url: url.to_string(),
                reason: source.to_string(),
            })
    }
}

#[derive(Debug, Error)]
pub enum RelauncherConfigError {
    #[error(
        "Cleanroom client dependencies must contain exactly one relauncher variant; found {count}: {variants:?}"
    )]
    InvalidVariantCount { count: usize, variants: Vec<String> },
    #[error(
        "Cleanroom relauncher config target '{path}' already exists in overrides/{side}; remove or rename it before building"
    )]
    ConfigCollision { path: String, side: &'static str },
    #[error("Java memory setting '{field}' must not be empty")]
    EmptyMemorySetting { field: &'static str },
    #[error(
        "Java memory setting '{field}' must be a non-empty whitespace-free JVM size, got '{value}'"
    )]
    InvalidMemorySetting { field: &'static str, value: String },
    #[error(transparent)]
    JvmArguments(#[from] JvmArgumentError),
    #[error("failed to serialize Cleanroom relauncher JSON: {0}")]
    SerializeJson(#[from] serde_json::Error),
    #[error("failed to inspect Cleanroom relauncher config target '{path}': {source}")]
    InspectConfigTarget {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to create Cleanroom MMC HTTP client: {reason}")]
    CreateHttpClient { reason: String },
    #[error("Cleanroom version '{version}' cannot be used in a Maven or archive path")]
    InvalidMmcVersion { version: String },
    #[error("failed to download Cleanroom MMC pack resource '{url}': {reason}")]
    DownloadMmcPack { url: String, reason: String },
    #[error("Cleanroom MMC SHA1 sidecar '{url}' is invalid: {reason}")]
    InvalidMmcSha1 { url: String, reason: String },
    #[error("Cleanroom MMC pack SHA1 mismatch: expected {expected}, got {actual}")]
    MmcSha1Mismatch { expected: String, actual: String },
    #[error("failed to read Cleanroom MMC cache '{path}': {source}")]
    ReadMmcCache {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write Cleanroom MMC cache '{path}': {source}")]
    WriteMmcCache {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("Cleanroom MMC pack is not a valid ZIP: {reason}")]
    InvalidMmcZip { reason: String },
    #[error("Cleanroom MMC pack is missing required root entry '{entry}'")]
    MissingMmcEntry { entry: &'static str },
    #[error("Cleanroom MMC pack contains duplicate root entry '{entry}'")]
    DuplicateMmcEntry { entry: &'static str },
    #[error("Cleanroom MMC pack entry '{entry}' is not valid JSON: {reason}")]
    InvalidMmcJson { entry: &'static str, reason: String },
    #[error(
        "Cleanroom MMC pack must contain exactly one net.minecraftforge component, found {count}"
    )]
    InvalidForgeComponentCount { count: usize },
    #[error("Cleanroom MMC pack component version is '{actual}', expected '{expected}'")]
    MmcVersionMismatch { expected: String, actual: String },
    #[error(
        "Cleanroom MMC pack must contain exactly one com.cleanroommc:cleanroom library, found {count}"
    )]
    InvalidCleanroomLibraryCount { count: usize },
    #[error("Cleanroom MMC pack library version is '{actual}', expected '{expected}'")]
    MmcLibraryVersionMismatch { expected: String, actual: String },
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct OriginalConfig<'a> {
    selected_version: &'a str,
    latest_version: &'a str,
    target_java_version: u16,
    target_vendor: &'static str,
    args: String,
    auto_setup: bool,
    enable_relauncher: bool,
    #[serde(rename = "FetchUpdates")]
    fetch_updates: bool,
    clear_cleanroom_folder: bool,
    clear_java_provision_folder: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ImprovedConfig<'a> {
    selected_version: &'a str,
    latest_version: &'a str,
    java_vendor: &'static str,
    java_version: u16,
    dark_mode: bool,
    auto_update: bool,
    #[serde(rename = "disableCacheTTL")]
    disable_cache_ttl: bool,
    java_path: Option<&'static str>,
    args: String,
}

pub fn prepare_client_config(
    project_dir: &Path,
    config: &Config,
    client_entries: &[DependencyEntry],
) -> Result<Option<GeneratedConfig>, RelauncherConfigError> {
    if config.pack.loader != LoaderKind::Cleanroom {
        return Ok(None);
    }

    let variants = client_entries
        .iter()
        .flat_map(matching_relauncher_variants)
        .collect::<Vec<_>>();
    if variants.len() != 1 {
        return Err(RelauncherConfigError::InvalidVariantCount {
            count: variants.len(),
            variants: variants
                .iter()
                .map(|variant| format!("{:?}", variant))
                .collect(),
        });
    }

    let variant = variants[0];
    let args = render_jvm_args(&config.java.client)?;
    let bytes = match variant {
        RelauncherVariant::Original => serde_json::to_vec_pretty(&OriginalConfig {
            selected_version: &config.pack.loader_version,
            latest_version: &config.pack.loader_version,
            target_java_version: 25,
            target_vendor: "ZULU",
            args,
            auto_setup: true,
            enable_relauncher: true,
            fetch_updates: false,
            clear_cleanroom_folder: false,
            clear_java_provision_folder: false,
        })?,
        RelauncherVariant::Improved => serde_json::to_vec_pretty(&ImprovedConfig {
            selected_version: &config.pack.loader_version,
            latest_version: &config.pack.loader_version,
            java_vendor: "adoptium",
            java_version: 25,
            dark_mode: true,
            auto_update: false,
            disable_cache_ttl: false,
            java_path: None,
            args,
        })?,
        RelauncherVariant::Unofficial => {
            let local_path = mmc_pack_relative_path(&config.pack.loader_version)?;
            ensure_no_collision(project_dir, &format!("overrides/{local_path}"))?;
            render_unofficial_cfg(&config.java.client, &local_path)?
        }
    };

    ensure_no_collision(project_dir, variant.config_path())?;
    Ok(Some(GeneratedConfig {
        variant,
        archive_path: variant.config_path(),
        bytes,
    }))
}

pub fn prepare_client_files<S: MmcPackSource>(
    project_dir: &Path,
    config: &Config,
    client_entries: &[DependencyEntry],
    source: &S,
) -> Result<Option<GeneratedClientFiles>, RelauncherConfigError> {
    let Some(generated_config) = prepare_client_config(project_dir, config, client_entries)? else {
        return Ok(None);
    };
    let variant = generated_config.variant;
    let mut files = vec![GeneratedClientFile {
        archive_path: generated_config.archive_path.to_string(),
        bytes: generated_config.bytes,
        store_uncompressed: false,
    }];

    if variant == RelauncherVariant::Unofficial {
        let archive_path = format!(
            "overrides/{}",
            mmc_pack_relative_path(&config.pack.loader_version)?
        );
        let bytes = load_mmc_pack(project_dir, &config.pack.loader_version, source)?;
        files.push(GeneratedClientFile {
            archive_path,
            bytes,
            store_uncompressed: true,
        });
    }

    Ok(Some(GeneratedClientFiles { variant, files }))
}

pub fn mmc_pack_release_url(version: &str) -> Result<String, RelauncherConfigError> {
    validate_mmc_version(version)?;
    Ok(format!(
        "https://repo.cleanroommc.com/releases/com/cleanroommc/cleanroom/{version}/cleanroom-{version}.zip"
    ))
}

fn mmc_pack_relative_path(version: &str) -> Result<String, RelauncherConfigError> {
    validate_mmc_version(version)?;
    Ok(format!("cleanroom/cleanroom-{version}.zip"))
}

fn validate_mmc_version(version: &str) -> Result<(), RelauncherConfigError> {
    if version.is_empty()
        || !version
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_'))
    {
        return Err(RelauncherConfigError::InvalidMmcVersion {
            version: version.to_string(),
        });
    }
    Ok(())
}

fn load_mmc_pack<S: MmcPackSource>(
    project_dir: &Path,
    version: &str,
    source: &S,
) -> Result<Vec<u8>, RelauncherConfigError> {
    let url = mmc_pack_release_url(version)?;
    let sha1_url = format!("{url}.sha1");
    let sha1_bytes = fetch(source, &sha1_url)?;
    let expected_sha1 = parse_sha1_sidecar(&sha1_url, &sha1_bytes)?;
    let cache_path = project_dir
        .join(".mcmpu")
        .join("cache")
        .join("cleanroom")
        .join(format!("cleanroom-{version}.zip"));

    match fs::read(&cache_path) {
        Ok(bytes) => match verify_mmc_pack(&bytes, version, &expected_sha1) {
            Ok(()) => return Ok(bytes),
            Err(error) => eprintln!(
                "[build] cached Cleanroom MMC pack '{}' is invalid: {error}; downloading again",
                cache_path.display()
            ),
        },
        Err(source) if source.kind() == ErrorKind::NotFound => {}
        Err(source) => {
            return Err(RelauncherConfigError::ReadMmcCache {
                path: cache_path.display().to_string(),
                source,
            });
        }
    }

    source.download_archive(&url, &cache_path, &expected_sha1)?;
    let bytes = fs::read(&cache_path).map_err(|source| RelauncherConfigError::ReadMmcCache {
        path: cache_path.display().to_string(),
        source,
    })?;
    verify_mmc_pack(&bytes, version, &expected_sha1)?;
    Ok(bytes)
}

fn fetch<S: MmcPackSource>(source: &S, url: &str) -> Result<Vec<u8>, RelauncherConfigError> {
    source
        .fetch(url)
        .map_err(|source| RelauncherConfigError::DownloadMmcPack {
            url: url.to_string(),
            reason: source.to_string(),
        })
}

fn parse_sha1_sidecar(url: &str, bytes: &[u8]) -> Result<String, RelauncherConfigError> {
    let text =
        std::str::from_utf8(bytes).map_err(|source| RelauncherConfigError::InvalidMmcSha1 {
            url: url.to_string(),
            reason: source.to_string(),
        })?;
    let sha1 = text.split_whitespace().next().unwrap_or_default();
    if sha1.len() != 40 || !sha1.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(RelauncherConfigError::InvalidMmcSha1 {
            url: url.to_string(),
            reason: "expected a 40-character hexadecimal SHA1".to_string(),
        });
    }
    Ok(sha1.to_ascii_lowercase())
}

fn verify_mmc_pack(
    bytes: &[u8],
    expected_version: &str,
    expected_sha1: &str,
) -> Result<(), RelauncherConfigError> {
    verify_mmc_sha1(bytes, expected_sha1)?;
    validate_mmc_zip(bytes, expected_version)
}

fn verify_mmc_sha1(bytes: &[u8], expected_sha1: &str) -> Result<(), RelauncherConfigError> {
    let actual_sha1 = format!("{:x}", Sha1::digest(bytes));
    if actual_sha1 != expected_sha1 {
        return Err(RelauncherConfigError::MmcSha1Mismatch {
            expected: expected_sha1.to_string(),
            actual: actual_sha1,
        });
    }
    Ok(())
}

fn validate_mmc_zip(bytes: &[u8], expected_version: &str) -> Result<(), RelauncherConfigError> {
    const REQUIRED_ENTRIES: &[&str] = &[
        "mmc-pack.json",
        "patches/net.minecraft.json",
        "patches/net.minecraftforge.json",
        "patches/org.lwjgl3.json",
    ];
    let mut archive = ZipArchive::new(Cursor::new(bytes)).map_err(|source| {
        RelauncherConfigError::InvalidMmcZip {
            reason: source.to_string(),
        }
    })?;
    let mut counts = [0_usize; REQUIRED_ENTRIES.len()];
    for index in 0..archive.len() {
        let file =
            archive
                .by_index(index)
                .map_err(|source| RelauncherConfigError::InvalidMmcZip {
                    reason: source.to_string(),
                })?;
        if file.enclosed_name().is_none() {
            return Err(RelauncherConfigError::InvalidMmcZip {
                reason: format!("entry '{}' escapes the ZIP root", file.name()),
            });
        }
        if let Some(required_index) = REQUIRED_ENTRIES
            .iter()
            .position(|required| file.name() == *required)
        {
            counts[required_index] += 1;
        }
    }
    for (index, entry) in REQUIRED_ENTRIES.iter().enumerate() {
        match counts[index] {
            0 => return Err(RelauncherConfigError::MissingMmcEntry { entry }),
            1 => {}
            _ => return Err(RelauncherConfigError::DuplicateMmcEntry { entry }),
        }
    }

    let manifest: MmcPackManifest = read_json_entry(&mut archive, "mmc-pack.json")?;
    let forge_versions = manifest
        .components
        .iter()
        .filter(|component| component.uid == "net.minecraftforge")
        .map(|component| component.version.as_str())
        .collect::<Vec<_>>();
    if forge_versions.len() != 1 {
        return Err(RelauncherConfigError::InvalidForgeComponentCount {
            count: forge_versions.len(),
        });
    }
    if forge_versions[0] != expected_version {
        return Err(RelauncherConfigError::MmcVersionMismatch {
            expected: expected_version.to_string(),
            actual: forge_versions[0].to_string(),
        });
    }

    let _: ForgePatch = read_json_entry(&mut archive, "patches/net.minecraft.json")?;
    let forge_patch: ForgePatch = read_json_entry(&mut archive, "patches/net.minecraftforge.json")?;
    let _: ForgePatch = read_json_entry(&mut archive, "patches/org.lwjgl3.json")?;
    let cleanroom_versions = forge_patch
        .libraries
        .iter()
        .filter_map(|library| {
            let mut parts = library.name.split(':');
            match (parts.next(), parts.next(), parts.next()) {
                (Some("com.cleanroommc"), Some("cleanroom"), Some(version)) => {
                    Some(version.strip_suffix("-universal").unwrap_or(version))
                }
                _ => None,
            }
        })
        .collect::<Vec<_>>();
    if cleanroom_versions.len() != 1 {
        return Err(RelauncherConfigError::InvalidCleanroomLibraryCount {
            count: cleanroom_versions.len(),
        });
    }
    if cleanroom_versions[0] != expected_version {
        return Err(RelauncherConfigError::MmcLibraryVersionMismatch {
            expected: expected_version.to_string(),
            actual: cleanroom_versions[0].to_string(),
        });
    }
    Ok(())
}

fn read_json_entry<T: for<'de> Deserialize<'de>>(
    archive: &mut ZipArchive<Cursor<&[u8]>>,
    entry: &'static str,
) -> Result<T, RelauncherConfigError> {
    let mut file = archive
        .by_name(entry)
        .map_err(|_| RelauncherConfigError::MissingMmcEntry { entry })?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|source| RelauncherConfigError::InvalidMmcZip {
            reason: format!("failed to read '{entry}': {source}"),
        })?;
    serde_json::from_slice(&bytes).map_err(|source| RelauncherConfigError::InvalidMmcJson {
        entry,
        reason: source.to_string(),
    })
}

fn write_cache_atomically(path: &Path, bytes: &[u8]) -> Result<(), RelauncherConfigError> {
    let parent = path.parent().expect("Cleanroom cache path has parent");
    fs::create_dir_all(parent).map_err(|source| RelauncherConfigError::WriteMmcCache {
        path: parent.display().to_string(),
        source,
    })?;
    let temp_path = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .expect("Cleanroom cache path has file name")
            .to_string_lossy(),
        std::process::id()
    ));
    fs::write(&temp_path, bytes).map_err(|source| RelauncherConfigError::WriteMmcCache {
        path: temp_path.display().to_string(),
        source,
    })?;
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(source) if source.kind() == ErrorKind::NotFound => {}
        Err(source) => {
            return Err(RelauncherConfigError::WriteMmcCache {
                path: path.display().to_string(),
                source,
            });
        }
    }
    fs::rename(&temp_path, path).map_err(|source| RelauncherConfigError::WriteMmcCache {
        path: path.display().to_string(),
        source,
    })
}

#[derive(Debug, Deserialize)]
struct MmcPackManifest {
    components: Vec<MmcComponent>,
}

#[derive(Debug, Deserialize)]
struct MmcComponent {
    uid: String,
    version: String,
}

#[derive(Debug, Deserialize)]
struct ForgePatch {
    libraries: Vec<MmcLibrary>,
}

#[derive(Debug, Deserialize)]
struct MmcLibrary {
    name: String,
}

fn matching_relauncher_variants(
    entry: &DependencyEntry,
) -> impl Iterator<Item = RelauncherVariant> + '_ {
    [
        RelauncherVariant::Original,
        RelauncherVariant::Improved,
        RelauncherVariant::Unofficial,
    ]
    .into_iter()
    .filter(|variant| {
        entry.project_id == Some(variant.project_id())
            || entry
                .slug
                .as_deref()
                .is_some_and(|slug| slug.eq_ignore_ascii_case(variant.slug()))
    })
}

fn render_jvm_args(java: &JavaMemoryConfig) -> Result<String, RelauncherConfigError> {
    validate_memory_setting("java.client.min_memory", &java.min_memory)?;
    validate_memory_setting("java.client.max_memory", &java.max_memory)?;
    Ok(java_memory_arguments(java)?.join(" "))
}

fn validate_memory_setting(field: &'static str, value: &str) -> Result<(), RelauncherConfigError> {
    if value.is_empty() {
        return Err(RelauncherConfigError::EmptyMemorySetting { field });
    }
    if value.starts_with('-') || value.chars().any(char::is_whitespace) {
        return Err(RelauncherConfigError::InvalidMemorySetting {
            field,
            value: value.to_string(),
        });
    }
    Ok(())
}

fn render_unofficial_cfg(
    java: &JavaMemoryConfig,
    local_pack_path: &str,
) -> Result<Vec<u8>, RelauncherConfigError> {
    let jvm_args = render_jvm_args(java)?;
    let fields = [
        // The generated package cannot know the Java executable selected by the
        // user's launcher. Let Cleanroom Relauncher open its Java download and
        // selection UI before it constructs the second JVM command.
        ("B", "Show Config", "true"),
        ("B", "Use Local MMC Pack", "true"),
        ("S", "Proxy Address", ""),
        ("I", "Proxy Port", "0"),
        ("S", "Java Path", ""),
        ("S", "JVM Arguments", jvm_args.as_str()),
        ("B", "Place in Group Name", "true"),
        ("S", "Library Path", ""),
        ("I", "Maximum Retry", "5"),
        ("I", "Maximum Session", "5"),
        ("B", "Use Mirror", "false"),
        ("B", "Always Show Config", "false"),
        ("S", "Local MMC Pack Path", local_pack_path),
        ("S", "Class Path", ""),
        ("B", "Disable Update Toast", "true"),
    ];
    let mut output = String::from("# Configuration file\n\ngeneral {\n");
    for (kind, name, value) in fields {
        output.push_str(&format!("    {kind}:\"{name}\"={value}\n"));
    }
    output.push_str("}\n");
    Ok(output.into_bytes())
}

fn ensure_no_collision(
    project_dir: &Path,
    archive_path: &str,
) -> Result<(), RelauncherConfigError> {
    let relative = archive_path
        .strip_prefix("overrides/")
        .expect("generated relauncher path must be under overrides");
    for side in ["common", "client"] {
        let path = project_dir.join("overrides").join(side).join(relative);
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                return Err(RelauncherConfigError::ConfigCollision {
                    path: archive_path.to_string(),
                    side,
                });
            }
            Err(source) if source.kind() == ErrorKind::NotFound => {}
            Err(source) => {
                return Err(RelauncherConfigError::InspectConfigTarget {
                    path: path.display().to_string(),
                    source,
                });
            }
        }
    }
    Ok(())
}
