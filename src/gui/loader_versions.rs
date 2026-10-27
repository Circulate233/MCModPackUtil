use std::cmp::Ordering;
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use thiserror::Error;

use crate::config::LoaderKind;
use crate::net::{HttpRequest, HttpTransport, NetworkConfig, NetworkEngine, NetworkError};

const FORGE_METADATA: &str =
    "https://maven.minecraftforge.net/net/minecraftforge/forge/maven-metadata.xml";
const NEOFORGE_METADATA: &str =
    "https://maven.neoforged.net/releases/net/neoforged/neoforge/maven-metadata.xml";
const CLEANROOM_RELEASES_METADATA: &str =
    "https://repo.cleanroommc.com/releases/com/cleanroommc/cleanroom/maven-metadata.xml";
const LOADER_METADATA_RESPONSE_LIMIT: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LoaderVersionQuery {
    pub minecraft_version: String,
    pub loader: LoaderKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoaderVersionCandidates {
    pub query: LoaderVersionQuery,
    pub loader_versions: Vec<String>,
    pub forge_compat_versions: Vec<String>,
}

/// UI-facing lifecycle for one loader-version candidate query.
///
/// Candidate metadata is deliberately fetched only after the user opens a
/// version selector. Keeping this state separate from the candidates avoids a
/// settings page load or a loader change implicitly starting network work.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum LoaderVersionCandidateState {
    /// The current loader/Minecraft pair has not been queried yet.
    #[default]
    Idle,
    /// A user-requested query is running in a background thread.
    Loading,
    /// Candidates for the current query are available.
    Ready,
    /// The latest user-requested query failed and can be retried.
    Failed(String),
}

impl LoaderVersionCandidateState {
    /// Starts a new user-requested query. Requests are ignored while one is
    /// already active, preventing duplicate clicks from spawning duplicates.
    pub fn begin_request(&mut self) -> bool {
        if !self.can_request() {
            return false;
        }
        *self = Self::Loading;
        true
    }

    /// Returns whether a selector click should start or retry a query.
    pub fn can_request(&self) -> bool {
        matches!(self, Self::Idle | Self::Failed(_))
    }

    /// Invalidates candidates after their loader or Minecraft version changes.
    pub fn invalidate(&mut self) {
        *self = Self::Idle;
    }

    /// Reduces a completed background request into a renderable UI state.
    pub fn finish_request(&mut self, result: Result<(), String>) {
        *self = match result {
            Ok(()) => Self::Ready,
            Err(error) => Self::Failed(error),
        };
    }

    pub fn is_loading(&self) -> bool {
        matches!(self, Self::Loading)
    }

    pub fn error(&self) -> Option<&str> {
        match self {
            Self::Failed(error) => Some(error),
            Self::Idle | Self::Loading | Self::Ready => None,
        }
    }
}

#[derive(Debug, Error)]
pub enum LoaderVersionError {
    #[error("Minecraft version is required before querying loader versions")]
    MissingMinecraftVersion,
    #[error("shared network engine error: {0}")]
    Network(#[from] NetworkError),
    #[error("GUI network engine cache lock was poisoned")]
    EngineCachePoisoned,
    #[error("loader metadata '{url}' is not valid UTF-8: {source}")]
    MetadataEncoding {
        url: String,
        #[source]
        source: std::string::FromUtf8Error,
    },
    #[error("failed to parse Maven metadata '{url}': {source}")]
    MavenMetadata {
        url: String,
        #[source]
        source: quick_xml::DeError,
    },
    #[error("no {loader} versions support Minecraft {minecraft_version}")]
    NoCompatibleVersions {
        loader: &'static str,
        minecraft_version: String,
    },
}

#[derive(Clone)]
pub struct LoaderVersionClient {
    network: NetworkEngine,
}

impl LoaderVersionClient {
    pub fn new(proxy: Option<&str>) -> Result<Self, LoaderVersionError> {
        let network = NetworkEngine::new(network_config(proxy))?;
        Ok(Self::with_network_engine(network))
    }

    /// Creates a metadata client on an existing shared network session.
    pub fn with_network_engine(network: NetworkEngine) -> Self {
        Self { network }
    }

    pub fn query(
        &self,
        query: LoaderVersionQuery,
    ) -> Result<LoaderVersionCandidates, LoaderVersionError> {
        let minecraft_version = query.minecraft_version.trim();
        if minecraft_version.is_empty() {
            return Err(LoaderVersionError::MissingMinecraftVersion);
        }
        let (loader_versions, forge_compat_versions) = match query.loader {
            LoaderKind::Forge => (
                forge_versions_for_minecraft(
                    minecraft_version,
                    self.maven_versions(FORGE_METADATA)?,
                ),
                Vec::new(),
            ),
            LoaderKind::Fabric => (self.fabric_versions(minecraft_version)?, Vec::new()),
            LoaderKind::NeoForge => (
                neoforge_versions_for_minecraft(
                    minecraft_version,
                    self.maven_versions(NEOFORGE_METADATA)?,
                ),
                Vec::new(),
            ),
            LoaderKind::Cleanroom => {
                let cleanroom = self.maven_versions(CLEANROOM_RELEASES_METADATA)?;
                let cleanroom = cleanroom_versions_for_minecraft(minecraft_version, cleanroom);
                let forge = forge_versions_for_minecraft(
                    minecraft_version,
                    self.maven_versions(FORGE_METADATA)?,
                );
                (cleanroom, forge)
            }
        };
        if loader_versions.is_empty() {
            return Err(LoaderVersionError::NoCompatibleVersions {
                loader: loader_name(&query.loader),
                minecraft_version: minecraft_version.to_string(),
            });
        }
        Ok(LoaderVersionCandidates {
            query,
            loader_versions,
            forge_compat_versions,
        })
    }

    fn maven_versions(&self, url: &str) -> Result<Vec<String>, LoaderVersionError> {
        let request = HttpRequest::get(url, LOADER_METADATA_RESPONSE_LIMIT).build()?;
        let response = self.network.get_bytes(request)?;
        let text = String::from_utf8(response.body).map_err(|source| {
            LoaderVersionError::MetadataEncoding {
                url: url.to_string(),
                source,
            }
        })?;
        parse_maven_versions(&text).map_err(|source| LoaderVersionError::MavenMetadata {
            url: url.to_string(),
            source,
        })
    }

    fn fabric_versions(&self, minecraft_version: &str) -> Result<Vec<String>, LoaderVersionError> {
        let url = format!("https://meta.fabricmc.net/v2/versions/loader/{minecraft_version}");
        let request = HttpRequest::get(&url, LOADER_METADATA_RESPONSE_LIMIT).build()?;
        let entries: Vec<FabricLoaderEntry> = self.network.get_json(request)?;
        Ok(sorted_unique(
            entries
                .into_iter()
                .map(|entry| entry.loader.version)
                .collect(),
        ))
    }
}

/// Thread-safe cache of shared network sessions keyed by normalized proxy URL.
///
/// GUI background jobs clone an engine from this cache. A proxy change creates
/// one new session, while repeated queries reuse the existing connection pool.
#[derive(Clone, Default)]
pub struct GuiNetworkEngineCache {
    engine: Arc<Mutex<Option<CachedNetworkEngine>>>,
}

struct CachedNetworkEngine {
    proxy: Option<String>,
    network: NetworkEngine,
}

impl std::fmt::Debug for GuiNetworkEngineCache {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuiNetworkEngineCache")
            .finish_non_exhaustive()
    }
}

impl GuiNetworkEngineCache {
    /// Returns the shared engine for `proxy`.
    pub fn network_engine(&self, proxy: Option<&str>) -> Result<NetworkEngine, LoaderVersionError> {
        let key = normalized_proxy(proxy);
        let mut cached = self
            .engine
            .lock()
            .map_err(|_| LoaderVersionError::EngineCachePoisoned)?;
        if let Some(entry) = cached.as_ref()
            && entry.proxy == key
        {
            return Ok(entry.network.clone());
        }
        let network = NetworkEngine::new(network_config(key.as_deref()))?;
        *cached = Some(CachedNetworkEngine {
            proxy: key,
            network: network.clone(),
        });
        Ok(network)
    }

    /// Returns a loader metadata client backed by the shared engine for `proxy`.
    pub fn loader_version_client(
        &self,
        proxy: Option<&str>,
    ) -> Result<LoaderVersionClient, LoaderVersionError> {
        self.network_engine(proxy)
            .map(LoaderVersionClient::with_network_engine)
    }

    /// Returns the number of proxy-specific sessions retained by this cache.
    pub fn cached_engine_count(&self) -> Result<usize, LoaderVersionError> {
        self.engine
            .lock()
            .map(|engine| usize::from(engine.is_some()))
            .map_err(|_| LoaderVersionError::EngineCachePoisoned)
    }
}

fn network_config(proxy: Option<&str>) -> NetworkConfig {
    NetworkConfig {
        proxy: normalized_proxy(proxy),
        ..NetworkConfig::default()
    }
}

fn normalized_proxy(proxy: Option<&str>) -> Option<String> {
    proxy
        .map(str::trim)
        .filter(|proxy| !proxy.is_empty())
        .map(ToOwned::to_owned)
}

#[derive(Debug, Deserialize)]
struct MavenMetadata {
    versioning: MavenVersioning,
}

#[derive(Debug, Deserialize)]
struct MavenVersioning {
    versions: MavenVersions,
}

#[derive(Debug, Deserialize)]
struct MavenVersions {
    #[serde(rename = "version", default)]
    values: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct FabricLoaderEntry {
    loader: FabricLoader,
}

#[derive(Debug, Deserialize)]
struct FabricLoader {
    version: String,
}

pub fn parse_maven_versions(xml: &str) -> Result<Vec<String>, quick_xml::DeError> {
    let metadata: MavenMetadata = quick_xml::de::from_str(xml)?;
    Ok(metadata.versioning.versions.values)
}

pub fn forge_versions_for_minecraft(minecraft_version: &str, versions: Vec<String>) -> Vec<String> {
    let prefix = format!("{minecraft_version}-");
    sorted_unique(
        versions
            .into_iter()
            .filter_map(|version| version.strip_prefix(&prefix).map(str::to_string))
            .collect(),
    )
}

pub fn neoforge_versions_for_minecraft(
    minecraft_version: &str,
    versions: Vec<String>,
) -> Vec<String> {
    let mut parts = minecraft_version.split('.');
    if parts.next() != Some("1") {
        return Vec::new();
    }
    let Some(minor) = parts.next() else {
        return Vec::new();
    };
    let Some(patch) = parts.next() else {
        return Vec::new();
    };
    let prefix = format!("{minor}.{patch}.");
    sorted_unique(
        versions
            .into_iter()
            .filter(|version| version.starts_with(&prefix))
            .collect(),
    )
}

pub fn cleanroom_versions_for_minecraft(
    minecraft_version: &str,
    versions: Vec<String>,
) -> Vec<String> {
    if minecraft_version != "1.12.2" {
        return Vec::new();
    }
    sorted_unique(
        versions
            .into_iter()
            .filter(|version| !version.to_ascii_lowercase().contains("build"))
            .collect(),
    )
}

fn sorted_unique(mut versions: Vec<String>) -> Vec<String> {
    versions.sort_by(|left, right| version_cmp(right, left));
    versions.dedup();
    versions
}

fn version_cmp(left: &str, right: &str) -> Ordering {
    let left = version_parts(left);
    let right = version_parts(right);
    left.cmp(&right)
}

fn version_parts(version: &str) -> Vec<(u8, String)> {
    version
        .split(['.', '-', '+'])
        .map(|part| match part.parse::<u64>() {
            Ok(number) => (1, format!("{number:020}")),
            Err(_) => (0, part.to_ascii_lowercase()),
        })
        .collect()
}

fn loader_name(loader: &LoaderKind) -> &'static str {
    match loader {
        LoaderKind::Forge => "Forge",
        LoaderKind::Fabric => "Fabric",
        LoaderKind::NeoForge => "NeoForge",
        LoaderKind::Cleanroom => "Cleanroom",
    }
}
