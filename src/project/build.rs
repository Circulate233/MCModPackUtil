use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, mpsc};

use crate::config::{Config, ConfigError};
use crate::curseforge::cache::PersistentCurseForgeApi;
use crate::curseforge::client::CurseForgeClient;
use crate::curseforge::model::CfFile;
use crate::curseforge::resolver::{
    ApiKeyResolver, CachedCurseForgeApi, CandidateSelector, CurseForgeApi, ResolveError,
};
use crate::dependency::{DependencyEntry, DependencyError, DependencySide, DependencyType};
use crate::manifest::generated::{
    GeneratedManifestError, generate_modlist_html, generate_pack_manifest,
};
use crate::project::archive::{
    ArchiveEntry, ArchiveError, ArchivePlan, Compression, DEFAULT_LIMITS, collect_layer,
    collect_root_docs, publish_pair, publish_single, unique_temp_path,
};
use crate::project::cleanroom::{CleanroomRuleError, cleanroom_dependency_supplements_for};
use crate::project::cleanroom_relauncher::{
    GeneratedClientFiles, HttpMmcPackSource, MmcPackSource, RelauncherConfigError,
    prepare_client_config, prepare_client_files,
};
use crate::project::jvm_args::java_memory_arguments;
use crate::server::files::{
    ServerFile, ServerFileDownload, ServerInstallManifest, ServerJavaConfig, ServerMinecraftConfig,
};
use crate::server::installer::{
    HttpServerInstallerSource, ServerInstallerAsset, ServerInstallerSource,
};
use crate::server::loader::{LoaderPlanError, plan_server_install_loader};
use rayon::prelude::*;
use thiserror::Error;

const TEST_BUILD_API_KEY: &str = "test-build-api-key";

pub use crate::server::installer::ServerPlatform;

/// Selects which distributable archives a build invocation creates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BuildTarget {
    /// Create only the CurseForge client package.
    Client,
    /// Create only one platform-specific server package.
    Server,
    /// Create the client package and one platform-specific server package atomically.
    #[default]
    Both,
}

impl BuildTarget {
    fn includes_client(self) -> bool {
        matches!(self, Self::Client | Self::Both)
    }

    fn includes_server(self) -> bool {
        matches!(self, Self::Server | Self::Both)
    }
}

/// Chooses how the target platform for a server archive is resolved.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ServerPlatformChoice {
    /// Use the current supported compilation target.
    #[default]
    Current,
    /// Build for one explicitly selected supported target.
    Explicit(ServerPlatform),
}

/// Immutable options for one build invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildOptions {
    /// Archives to create.
    pub target: BuildTarget,
    /// Server platform selection, used only when `target` includes the server.
    pub server_platform: ServerPlatformChoice,
    /// Optional output directory replacing `[build].output_dir` for this invocation.
    pub output_dir_override: Option<PathBuf>,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            target: BuildTarget::Both,
            server_platform: ServerPlatformChoice::Current,
            output_dir_override: None,
        }
    }
}

/// Archives created by a target-aware build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildArtifacts {
    /// Client ZIP path when the target includes the client.
    pub client_zip: Option<PathBuf>,
    /// Platform-specific server ZIP path when the target includes the server.
    pub server_zip: Option<PathBuf>,
}

/// Pair of archives returned by legacy build wrappers, which always build both targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildOutputs {
    /// CurseForge client ZIP.
    pub client_zip: PathBuf,
    /// Server ZIP for the current host platform.
    pub server_zip: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildValidationReport {
    pub client_manifest_entries: usize,
    pub server_downloads: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildProgress {
    ReadingProject {
        project_dir: String,
    },
    DependencyListsLoaded {
        common: usize,
        client: usize,
        server: usize,
        resourcepacks: usize,
    },
    CleanroomSupplementsApplied {
        added: usize,
    },
    ResolvingDependencies {
        total: usize,
    },
    ResolvedDependency {
        completed: usize,
        total: usize,
        project_id: Option<u64>,
        file_id: Option<u64>,
        slug: Option<String>,
    },
    PreparingOutput {
        output_dir: String,
    },
    PreparingServerInstallerAssets,
    ServerInstallerAssetsReady,
    WritingClientZip {
        path: String,
    },
    WritingServerZip {
        path: String,
    },
    Finished {
        client_zip: String,
        server_zip: String,
    },
}

#[derive(Debug, Error)]
pub enum BuildError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("failed to read file '{path}': {source}")]
    ReadFile {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to create directory '{path}': {source}")]
    CreateDir {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write file '{path}': {source}")]
    WriteFile {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid configured build output directory: {reason}")]
    InvalidOutputDirectory { reason: String },
    #[error("failed to read dependency list '{path}': {source}")]
    ReadDependencyList {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse dependency list '{path}': {source}")]
    ParseDependencyList {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to serialize JSON: {0}")]
    SerializeJson(#[from] serde_json::Error),
    #[error(transparent)]
    GeneratedManifest(#[from] GeneratedManifestError),
    #[error(transparent)]
    Resolve(#[from] ResolveError),
    #[error(transparent)]
    Dependency(#[from] DependencyError),
    #[error(transparent)]
    Cleanroom(#[from] CleanroomRuleError),
    #[error(transparent)]
    CleanroomRelauncherConfig(#[from] RelauncherConfigError),
    #[error(transparent)]
    Loader(#[from] LoaderPlanError),
    #[error(transparent)]
    ServerMemory(#[from] crate::server::files::ServerMemoryError),
    #[error(transparent)]
    ServerManifest(#[from] crate::server::files::ServerManifestError),
    #[error(transparent)]
    ServerInstallerNetwork(#[from] crate::net::NetworkError),
    #[error("failed to prepare server installer assets: {0}")]
    ServerInstaller(String),
    #[error("invalid build options: {reason}")]
    InvalidOptions { reason: String },
    #[error(transparent)]
    ServerPlatform(#[from] crate::server::installer::ServerPlatformError),
    #[error("build did not produce required {target} output")]
    MissingOutput { target: &'static str },
    #[error(transparent)]
    JvmArguments(#[from] crate::project::jvm_args::JvmArgumentError),
    #[error("failed to build archive pair: {reason}")]
    Archive { reason: String },
    #[error("invalid server override layout: {reason}")]
    ServerOverrideLayout { reason: String },
    #[error("internal dependency resolution bucket '{bucket}' missed index {index}")]
    ResolutionBucket { bucket: &'static str, index: usize },
    #[error("CurseForge file {project_id}/{file_id} has no SHA1 hash")]
    MissingSha1 { project_id: u64, file_id: u64 },
}

impl From<ArchiveError> for BuildError {
    fn from(error: ArchiveError) -> Self {
        Self::Archive {
            reason: error.to_string(),
        }
    }
}

#[derive(Debug, Clone)]
struct ResolvedEntry {
    entry: DependencyEntry,
    file: CfFile,
}

#[derive(Debug, Clone)]
pub struct RuntimeResolvedEntry {
    pub entry: DependencyEntry,
    pub file: CfFile,
}

#[derive(Debug, Clone)]
pub struct RuntimeResolvedLists {
    pub common: Vec<RuntimeResolvedEntry>,
    pub client: Vec<RuntimeResolvedEntry>,
    pub server: Vec<RuntimeResolvedEntry>,
    pub resourcepacks: Vec<RuntimeResolvedEntry>,
}

#[derive(Debug, Clone, Copy)]
enum ListBucket {
    Common,
    Client,
    Server,
    Resourcepacks,
}

#[derive(Debug, Clone)]
struct QueuedEntry {
    bucket: ListBucket,
    index: usize,
    entry: DependencyEntry,
}

#[derive(Debug, Clone)]
struct ResolvedQueuedEntry {
    bucket: ListBucket,
    index: usize,
    resolved: ResolvedEntry,
}

pub fn build_project(project_dir: impl AsRef<Path>) -> Result<BuildOutputs, BuildError> {
    require_both_outputs(build_project_with_options(
        project_dir,
        BuildOptions::default(),
    )?)
}

/// Builds the selected targets using the configured CurseForge credentials and network settings.
pub fn build_project_with_options(
    project_dir: impl AsRef<Path>,
    options: BuildOptions,
) -> Result<BuildArtifacts, BuildError> {
    let project_dir = project_dir.as_ref();
    let config = Config::from_path(project_dir.join("config.toml"))?;
    let proxy = resolve_build_proxy(&config, None)?;
    let env_api_key = ApiKeyResolver::env_key();
    let builtin_api_key = ApiKeyResolver::builtin_key();
    let api_key = ApiKeyResolver::resolve(
        None,
        env_api_key.as_deref(),
        builtin_api_key.as_deref().unwrap_or(""),
    )?;
    let api = CurseForgeClient::with_proxy(api_key.clone(), proxy.as_deref())?;
    let mmc_source = HttpMmcPackSource::with_proxy(proxy.as_deref())?;
    let server_installer_source = HttpServerInstallerSource::with_proxy(proxy.as_deref())?;
    build_project_with_config_and_api(
        BuildInvocation {
            project_dir,
            config,
            server_api_key: &api_key,
            options,
        },
        BuildSources {
            api: &api,
            mmc: &mmc_source,
            server_installer: &server_installer_source,
        },
        &mut |_| {},
    )
}

pub fn build_project_with_api_key(
    project_dir: impl AsRef<Path>,
    api_key: String,
    proxy: Option<&str>,
) -> Result<BuildOutputs, BuildError> {
    require_both_outputs(build_project_with_api_key_and_options(
        project_dir,
        api_key,
        proxy,
        BuildOptions::default(),
    )?)
}

/// Builds the selected targets with an explicit CurseForge API key.
pub fn build_project_with_api_key_and_options(
    project_dir: impl AsRef<Path>,
    api_key: String,
    proxy: Option<&str>,
    options: BuildOptions,
) -> Result<BuildArtifacts, BuildError> {
    let project_dir = project_dir.as_ref();
    let config = Config::from_path(project_dir.join("config.toml"))?;
    let proxy = resolve_build_proxy(&config, proxy)?;
    let api = CurseForgeClient::with_proxy(api_key.clone(), proxy.as_deref())?;
    let mmc_source = HttpMmcPackSource::with_proxy(proxy.as_deref())?;
    let server_installer_source = HttpServerInstallerSource::with_proxy(proxy.as_deref())?;
    build_project_with_config_and_api(
        BuildInvocation {
            project_dir,
            config,
            server_api_key: &api_key,
            options,
        },
        BuildSources {
            api: &api,
            mmc: &mmc_source,
            server_installer: &server_installer_source,
        },
        &mut |_| {},
    )
}

pub fn build_project_with_api_key_and_progress<F>(
    project_dir: impl AsRef<Path>,
    api_key: String,
    proxy: Option<&str>,
    progress: &mut F,
) -> Result<BuildOutputs, BuildError>
where
    F: FnMut(&BuildProgress) + Send,
{
    build_project_with_api_key_and_progress_to(project_dir, api_key, proxy, None, progress)
}

pub fn build_project_with_api_key_and_progress_to<F>(
    project_dir: impl AsRef<Path>,
    api_key: String,
    proxy: Option<&str>,
    output_dir: Option<&Path>,
    progress: &mut F,
) -> Result<BuildOutputs, BuildError>
where
    F: FnMut(&BuildProgress) + Send,
{
    let options = BuildOptions {
        output_dir_override: output_dir.map(Path::to_path_buf),
        ..BuildOptions::default()
    };
    require_both_outputs(build_project_with_api_key_and_options_and_progress(
        project_dir,
        api_key,
        proxy,
        options,
        progress,
    )?)
}

/// Builds selected targets with explicit credentials and progress reporting.
pub fn build_project_with_api_key_and_options_and_progress<F>(
    project_dir: impl AsRef<Path>,
    api_key: String,
    proxy: Option<&str>,
    options: BuildOptions,
    progress: &mut F,
) -> Result<BuildArtifacts, BuildError>
where
    F: FnMut(&BuildProgress) + Send,
{
    let project_dir = project_dir.as_ref();
    let config = Config::from_path(project_dir.join("config.toml"))?;
    let proxy = resolve_build_proxy(&config, proxy)?;
    let api = CurseForgeClient::with_proxy(api_key.clone(), proxy.as_deref())?;
    let mmc_source = HttpMmcPackSource::with_proxy(proxy.as_deref())?;
    let server_installer_source = HttpServerInstallerSource::with_proxy(proxy.as_deref())?;
    build_project_with_config_and_api(
        BuildInvocation {
            project_dir,
            config,
            server_api_key: &api_key,
            options,
        },
        BuildSources {
            api: &api,
            mmc: &mmc_source,
            server_installer: &server_installer_source,
        },
        progress,
    )
}

pub fn build_project_with_api<A: CurseForgeApi + Sync>(
    project_dir: impl AsRef<Path>,
    api: &A,
) -> Result<BuildOutputs, BuildError> {
    require_both_outputs(build_project_with_api_and_options(
        project_dir,
        api,
        BuildOptions::default(),
    )?)
}

/// Builds selected targets using a caller-provided CurseForge API implementation.
pub fn build_project_with_api_and_options<A: CurseForgeApi + Sync>(
    project_dir: impl AsRef<Path>,
    api: &A,
    options: BuildOptions,
) -> Result<BuildArtifacts, BuildError> {
    let project_dir = project_dir.as_ref();
    let config = Config::from_path(project_dir.join("config.toml"))?;
    let proxy = resolve_build_proxy(&config, None)?;
    let mmc_source = HttpMmcPackSource::with_proxy(proxy.as_deref())?;
    let server_installer_source = HttpServerInstallerSource::with_proxy(proxy.as_deref())?;
    build_project_with_config_and_api(
        BuildInvocation {
            project_dir,
            config,
            server_api_key: TEST_BUILD_API_KEY,
            options,
        },
        BuildSources {
            api,
            mmc: &mmc_source,
            server_installer: &server_installer_source,
        },
        &mut |_| {},
    )
}

pub fn build_project_with_sources<A, S, I>(
    project_dir: impl AsRef<Path>,
    api: &A,
    mmc_source: &S,
    server_installer_source: &I,
) -> Result<BuildOutputs, BuildError>
where
    A: CurseForgeApi + Sync,
    S: MmcPackSource,
    I: ServerInstallerSource + Sync,
{
    build_project_with_sources_to(project_dir, api, mmc_source, server_installer_source, None)
}

pub fn build_project_with_sources_to<A, S, I>(
    project_dir: impl AsRef<Path>,
    api: &A,
    mmc_source: &S,
    server_installer_source: &I,
    output_dir: Option<&Path>,
) -> Result<BuildOutputs, BuildError>
where
    A: CurseForgeApi + Sync,
    S: MmcPackSource,
    I: ServerInstallerSource + Sync,
{
    let options = BuildOptions {
        output_dir_override: output_dir.map(Path::to_path_buf),
        ..BuildOptions::default()
    };
    require_both_outputs(build_project_with_options_and_sources(
        project_dir,
        api,
        mmc_source,
        server_installer_source,
        options,
    )?)
}

/// Builds selected targets from caller-provided API, MMC, and server-installer sources.
pub fn build_project_with_options_and_sources<A, S, I>(
    project_dir: impl AsRef<Path>,
    api: &A,
    mmc_source: &S,
    server_installer_source: &I,
    options: BuildOptions,
) -> Result<BuildArtifacts, BuildError>
where
    A: CurseForgeApi + Sync,
    S: MmcPackSource,
    I: ServerInstallerSource + Sync,
{
    let project_dir = project_dir.as_ref();
    let config = Config::from_path(project_dir.join("config.toml"))?;
    build_project_with_config_and_api(
        BuildInvocation {
            project_dir,
            config,
            server_api_key: TEST_BUILD_API_KEY,
            options,
        },
        BuildSources {
            api,
            mmc: mmc_source,
            server_installer: server_installer_source,
        },
        &mut |_| {},
    )
}

pub fn build_project_with_api_and_progress<A, F>(
    project_dir: impl AsRef<Path>,
    api: &A,
    progress: &mut F,
) -> Result<BuildOutputs, BuildError>
where
    A: CurseForgeApi + Sync,
    F: FnMut(&BuildProgress) + Send,
{
    require_both_outputs(build_project_with_api_and_options_and_progress(
        project_dir,
        api,
        BuildOptions::default(),
        progress,
    )?)
}

/// Builds selected targets with a caller-provided API and progress reporting.
pub fn build_project_with_api_and_options_and_progress<A, F>(
    project_dir: impl AsRef<Path>,
    api: &A,
    options: BuildOptions,
    progress: &mut F,
) -> Result<BuildArtifacts, BuildError>
where
    A: CurseForgeApi + Sync,
    F: FnMut(&BuildProgress) + Send,
{
    let project_dir = project_dir.as_ref();
    let config = Config::from_path(project_dir.join("config.toml"))?;
    let proxy = resolve_build_proxy(&config, None)?;
    let mmc_source = HttpMmcPackSource::with_proxy(proxy.as_deref())?;
    let server_installer_source = HttpServerInstallerSource::with_proxy(proxy.as_deref())?;
    build_project_with_config_and_api(
        BuildInvocation {
            project_dir,
            config,
            server_api_key: TEST_BUILD_API_KEY,
            options,
        },
        BuildSources {
            api,
            mmc: &mmc_source,
            server_installer: &server_installer_source,
        },
        progress,
    )
}

pub fn build_project_with_sources_and_progress<A, S, I, F>(
    project_dir: impl AsRef<Path>,
    api: &A,
    mmc_source: &S,
    server_installer_source: &I,
    progress: &mut F,
) -> Result<BuildOutputs, BuildError>
where
    A: CurseForgeApi + Sync,
    S: MmcPackSource,
    I: ServerInstallerSource + Sync,
    F: FnMut(&BuildProgress) + Send,
{
    require_both_outputs(build_project_with_options_and_sources_and_progress(
        project_dir,
        api,
        mmc_source,
        server_installer_source,
        BuildOptions::default(),
        progress,
    )?)
}

/// Builds selected targets from caller-provided sources with progress reporting.
pub fn build_project_with_options_and_sources_and_progress<A, S, I, F>(
    project_dir: impl AsRef<Path>,
    api: &A,
    mmc_source: &S,
    server_installer_source: &I,
    options: BuildOptions,
    progress: &mut F,
) -> Result<BuildArtifacts, BuildError>
where
    A: CurseForgeApi + Sync,
    S: MmcPackSource,
    I: ServerInstallerSource + Sync,
    F: FnMut(&BuildProgress) + Send,
{
    let project_dir = project_dir.as_ref();
    let config = Config::from_path(project_dir.join("config.toml"))?;
    build_project_with_config_and_api(
        BuildInvocation {
            project_dir,
            config,
            server_api_key: TEST_BUILD_API_KEY,
            options,
        },
        BuildSources {
            api,
            mmc: mmc_source,
            server_installer: server_installer_source,
        },
        progress,
    )
}

fn require_both_outputs(artifacts: BuildArtifacts) -> Result<BuildOutputs, BuildError> {
    Ok(BuildOutputs {
        client_zip: artifacts
            .client_zip
            .ok_or(BuildError::MissingOutput { target: "client" })?,
        server_zip: artifacts
            .server_zip
            .ok_or(BuildError::MissingOutput { target: "server" })?,
    })
}

fn resolve_build_proxy(
    config: &Config,
    explicit_proxy: Option<&str>,
) -> Result<Option<String>, ConfigError> {
    match explicit_proxy {
        Some(proxy) => Ok(Some(proxy.to_string())),
        None => config.network.proxy_url(),
    }
}

pub fn validate_project_with_api<A: CurseForgeApi + Sync>(
    project_dir: impl AsRef<Path>,
    api: &A,
) -> Result<BuildValidationReport, BuildError> {
    validate_project_with_api_and_progress(project_dir, api, &mut |_| {})
}

pub fn validate_project_with_api_and_progress<A, F>(
    project_dir: impl AsRef<Path>,
    api: &A,
    progress: &mut F,
) -> Result<BuildValidationReport, BuildError>
where
    A: CurseForgeApi + Sync,
    F: FnMut(&BuildProgress) + Send,
{
    let project_dir = project_dir.as_ref();
    progress(&BuildProgress::ReadingProject {
        project_dir: project_dir.display().to_string(),
    });
    let config = Config::from_path(project_dir.join("config.toml"))?;
    let lists = ProjectLists::read(project_dir)?;
    let before_supplements = lists.total_len();
    progress(&BuildProgress::DependencyListsLoaded {
        common: lists.common.len(),
        client: lists.client.len(),
        server: lists.server.len(),
        resourcepacks: lists.resourcepacks.len(),
    });
    let lists = lists.with_cleanroom_supplements(&config)?;
    progress(&BuildProgress::CleanroomSupplementsApplied {
        added: lists.total_len().saturating_sub(before_supplements),
    });
    let resolved = ResolvedLists::resolve(project_dir, &config, api, &lists, progress)?;
    prepare_client_config(project_dir, &config, &resolved.client_manifest_entries())?;
    let manifest = server_files_manifest(&config, &resolved, TEST_BUILD_API_KEY)?;
    Ok(BuildValidationReport {
        client_manifest_entries: resolved.client_manifest_entries().len(),
        server_downloads: manifest.files.len(),
    })
}

pub fn resolve_project_for_runtime_with_api<A: CurseForgeApi + Sync>(
    project_dir: impl AsRef<Path>,
    api: &A,
) -> Result<(Config, RuntimeResolvedLists), BuildError> {
    let project_dir = project_dir.as_ref();
    let config = Config::from_path(project_dir.join("config.toml"))?;
    let lists = ProjectLists::read(project_dir)?;
    let lists = lists.with_cleanroom_runtime_supplements(&config)?;
    let resolved =
        ResolvedLists::resolve_without_download_url_check(project_dir, &config, api, &lists)?;
    Ok((config, resolved.into_runtime()))
}

struct BuildInvocation<'a> {
    project_dir: &'a Path,
    config: Config,
    server_api_key: &'a str,
    options: BuildOptions,
}

struct BuildSources<'a, A, S, I> {
    api: &'a A,
    mmc: &'a S,
    server_installer: &'a I,
}

fn build_project_with_config_and_api<A, S, I>(
    invocation: BuildInvocation<'_>,
    sources: BuildSources<'_, A, S, I>,
    progress: &mut (impl FnMut(&BuildProgress) + Send),
) -> Result<BuildArtifacts, BuildError>
where
    A: CurseForgeApi + Sync,
    S: MmcPackSource,
    I: ServerInstallerSource + Sync,
{
    let BuildInvocation {
        project_dir,
        config,
        server_api_key,
        options,
    } = invocation;
    let target = options.target;
    let server_platform = match (target.includes_server(), options.server_platform) {
        (true, ServerPlatformChoice::Current) => Some(ServerPlatform::current()?),
        (true, ServerPlatformChoice::Explicit(platform)) => Some(platform),
        (false, ServerPlatformChoice::Current) => None,
        (false, ServerPlatformChoice::Explicit(platform)) => {
            return Err(BuildError::InvalidOptions {
                reason: format!(
                    "server platform '{}' cannot be used with a client-only build",
                    platform.as_str()
                ),
            });
        }
    };
    if target.includes_server() {
        java_memory_arguments(&config.java.server)?;
    }
    let output_dir = match options.output_dir_override {
        Some(path) => path,
        None => configured_output_dir(project_dir, &config.build.output_dir)?,
    };
    progress(&BuildProgress::ReadingProject {
        project_dir: project_dir.display().to_string(),
    });
    let lists = ProjectLists::read_for_target(project_dir, target)?;
    let before_supplements = lists.total_len();
    progress(&BuildProgress::DependencyListsLoaded {
        common: lists.common.len(),
        client: lists.client.len(),
        server: lists.server.len(),
        resourcepacks: lists.resourcepacks.len(),
    });
    let lists = lists.with_cleanroom_supplements_for_target(&config, target)?;
    progress(&BuildProgress::CleanroomSupplementsApplied {
        added: lists.total_len().saturating_sub(before_supplements),
    });
    let resolved = ResolvedLists::resolve(project_dir, &config, sources.api, &lists, progress)?;
    progress(&BuildProgress::PreparingOutput {
        output_dir: output_dir.display().to_string(),
    });
    create_dir(&output_dir)?;

    let artifacts = match target {
        BuildTarget::Client => {
            let (client_zip, client_temp) = prepare_client_archive(
                project_dir,
                &output_dir,
                &config,
                &resolved,
                sources.mmc,
                progress,
            )?;
            if let Err(error) = publish_single(&client_temp, &client_zip) {
                let _ = fs::remove_file(&client_temp);
                return Err(error.into());
            }
            BuildArtifacts {
                client_zip: Some(client_zip),
                server_zip: None,
            }
        }
        BuildTarget::Server => {
            let platform = server_platform.expect("server target resolved its platform");
            progress(&BuildProgress::PreparingServerInstallerAssets);
            let installer =
                prepare_server_installer(sources.server_installer, project_dir, platform)?;
            progress(&BuildProgress::ServerInstallerAssetsReady);
            let (server_zip, server_temp) = prepare_server_archive(
                project_dir,
                &output_dir,
                &config,
                &resolved,
                server_api_key,
                platform,
                &installer,
                progress,
            )?;
            if let Err(error) = publish_single(&server_temp, &server_zip) {
                let _ = fs::remove_file(&server_temp);
                return Err(error.into());
            }
            BuildArtifacts {
                client_zip: None,
                server_zip: Some(server_zip),
            }
        }
        BuildTarget::Both => {
            let platform = server_platform.expect("both target resolved its server platform");
            progress(&BuildProgress::PreparingServerInstallerAssets);
            let (client_zip, server_zip) = std::thread::scope(|scope| {
                let (request_started_tx, request_started_rx) = mpsc::sync_channel(1);
                let installer_request_task = scope.spawn(move || {
                    let request_started = std::cell::Cell::new(false);
                    let duplicate_signal = std::cell::Cell::new(false);
                    let signal_failed = std::cell::Cell::new(false);
                    let request = sources.server_installer.request(platform, &mut || {
                        if request_started.replace(true) {
                            duplicate_signal.set(true);
                        } else if request_started_tx.send(()).is_err() {
                            signal_failed.set(true);
                        }
                    });
                    if !request_started.get() {
                        return Err(BuildError::ServerInstaller(
                            "server installer source returned without initiating its metadata request"
                                .to_string(),
                        ));
                    }
                    if duplicate_signal.get() {
                        return Err(BuildError::ServerInstaller(
                            "server installer source initiated its metadata request more than once"
                                .to_string(),
                        ));
                    }
                    if signal_failed.get() {
                        return Err(BuildError::ServerInstaller(
                            "server installer metadata request start receiver was dropped"
                                .to_string(),
                        ));
                    }
                    request.map_err(|source| BuildError::ServerInstaller(source.to_string()))
                });

                request_started_rx.recv().map_err(|_| {
                    BuildError::ServerInstaller(
                        "server installer metadata request worker stopped before initiation"
                            .to_string(),
                    )
                })?;
                let client_result = prepare_client_archive(
                    project_dir,
                    &output_dir,
                    &config,
                    &resolved,
                    sources.mmc,
                    progress,
                );
                let installer_request_result = match installer_request_task.join() {
                    Ok(result) => result,
                    Err(_) => {
                        if let Err(client_error) = client_result {
                            eprintln!(
                                "server installer metadata request worker panicked after the client build failed: {client_error}"
                            );
                            return Err(client_error);
                        }
                        remove_client_temp(&client_result);
                        return Err(BuildError::ServerInstaller(
                            "server installer metadata request worker panicked".to_string(),
                        ));
                    }
                };
                let (client_zip, client_temp) = client_result?;
                let installer_request = match installer_request_result {
                    Ok(request) => request,
                    Err(error) => {
                        let _ = fs::remove_file(&client_temp);
                        return Err(error);
                    }
                };
                let installer =
                    match catch_unwind(AssertUnwindSafe(|| installer_request.prepare(project_dir)))
                    {
                        Ok(Ok(installer)) => installer,
                        Ok(Err(source)) => {
                            let _ = fs::remove_file(&client_temp);
                            return Err(BuildError::ServerInstaller(source.to_string()));
                        }
                        Err(_) => {
                            let _ = fs::remove_file(&client_temp);
                            return Err(BuildError::ServerInstaller(
                                "server installer payload preparation panicked".to_string(),
                            ));
                        }
                    };
                if let Err(source) = installer.validate_for(platform) {
                    let _ = fs::remove_file(&client_temp);
                    return Err(BuildError::ServerInstaller(source.to_string()));
                }
                progress(&BuildProgress::ServerInstallerAssetsReady);
                let (server_zip, server_temp) = match prepare_server_archive(
                    project_dir,
                    &output_dir,
                    &config,
                    &resolved,
                    server_api_key,
                    platform,
                    &installer,
                    progress,
                ) {
                    Ok(output) => output,
                    Err(error) => {
                        let _ = fs::remove_file(&client_temp);
                        return Err(error);
                    }
                };
                if let Err(error) =
                    publish_pair(&client_temp, &client_zip, &server_temp, &server_zip)
                {
                    let _ = fs::remove_file(&client_temp);
                    let _ = fs::remove_file(&server_temp);
                    return Err(error.into());
                }
                Ok((client_zip, server_zip))
            })?;
            BuildArtifacts {
                client_zip: Some(client_zip),
                server_zip: Some(server_zip),
            }
        }
    };

    progress(&BuildProgress::Finished {
        client_zip: artifacts
            .client_zip
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
        server_zip: artifacts
            .server_zip
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
    });
    Ok(artifacts)
}

fn remove_client_temp(result: &Result<(PathBuf, PathBuf), BuildError>) {
    if let Ok((_, client_temp)) = result {
        let _ = fs::remove_file(client_temp);
    }
}

fn prepare_server_installer<I: ServerInstallerSource>(
    source: &I,
    project_dir: &Path,
    platform: ServerPlatform,
) -> Result<ServerInstallerAsset, BuildError> {
    let asset = source
        .prepare(project_dir, platform)
        .map_err(|source| BuildError::ServerInstaller(source.to_string()))?;
    asset
        .validate_for(platform)
        .map_err(|source| BuildError::ServerInstaller(source.to_string()))?;
    Ok(asset)
}

fn prepare_client_archive<S: MmcPackSource>(
    project_dir: &Path,
    output_dir: &Path,
    config: &Config,
    resolved: &ResolvedLists,
    mmc_source: &S,
    progress: &mut (impl FnMut(&BuildProgress) + Send),
) -> Result<(PathBuf, PathBuf), BuildError> {
    let cleanroom_files = prepare_client_files(
        project_dir,
        config,
        &resolved.client_manifest_entries(),
        mmc_source,
    )?;
    let client_zip = output_dir.join(format!("{}.zip", config.pack.name));
    let client_plan = plan_client_zip(project_dir, config, resolved, cleanroom_files.as_ref())?;
    let client_temp = unique_temp_path(&client_zip, "client")?;
    progress(&BuildProgress::WritingClientZip {
        path: client_zip.display().to_string(),
    });
    if let Err(error) = client_plan.write_to(&client_temp) {
        let _ = fs::remove_file(&client_temp);
        return Err(error.into());
    }
    Ok((client_zip, client_temp))
}

#[allow(clippy::too_many_arguments)]
fn prepare_server_archive(
    project_dir: &Path,
    output_dir: &Path,
    config: &Config,
    resolved: &ResolvedLists,
    server_api_key: &str,
    platform: ServerPlatform,
    installer: &ServerInstallerAsset,
    progress: &mut (impl FnMut(&BuildProgress) + Send),
) -> Result<(PathBuf, PathBuf), BuildError> {
    let base_name = format!("{}-{}", config.pack.name, config.pack.version);
    let server_zip = output_dir.join(format!("{base_name}-server-{}.zip", platform.as_str()));
    let server_plan = plan_server_zip(project_dir, config, resolved, server_api_key, installer)?;
    let server_temp = unique_temp_path(&server_zip, "server")?;
    progress(&BuildProgress::WritingServerZip {
        path: server_zip.display().to_string(),
    });
    if let Err(error) = server_plan.write_to(&server_temp) {
        let _ = fs::remove_file(&server_temp);
        return Err(error.into());
    }
    Ok((server_zip, server_temp))
}

#[derive(Debug, Clone)]
struct ProjectLists {
    common: Vec<DependencyEntry>,
    client: Vec<DependencyEntry>,
    server: Vec<DependencyEntry>,
    resourcepacks: Vec<DependencyEntry>,
}

impl ProjectLists {
    fn read(project_dir: &Path) -> Result<Self, BuildError> {
        Ok(Self {
            common: read_dependency_list(project_dir.join("common.json"), DependencySide::Common)?,
            client: read_dependency_list(project_dir.join("client.json"), DependencySide::Client)?,
            server: read_dependency_list(project_dir.join("server.json"), DependencySide::Server)?,
            resourcepacks: read_dependency_list(
                project_dir.join("resourcepacks.json"),
                DependencySide::Resourcepack,
            )?,
        })
    }

    fn read_for_target(project_dir: &Path, target: BuildTarget) -> Result<Self, BuildError> {
        let mut resourcepacks = read_dependency_list(
            project_dir.join("resourcepacks.json"),
            DependencySide::Resourcepack,
        )?;
        if target == BuildTarget::Server {
            resourcepacks.retain(|entry| entry.dependency_type != DependencyType::ShaderPack);
        }
        Ok(Self {
            common: read_dependency_list(project_dir.join("common.json"), DependencySide::Common)?,
            client: if target.includes_client() {
                read_dependency_list(project_dir.join("client.json"), DependencySide::Client)?
            } else {
                Vec::new()
            },
            server: if target.includes_server() {
                read_dependency_list(project_dir.join("server.json"), DependencySide::Server)?
            } else {
                Vec::new()
            },
            resourcepacks,
        })
    }

    fn all_entries(&self) -> Vec<DependencyEntry> {
        self.common
            .iter()
            .chain(&self.client)
            .chain(&self.server)
            .chain(&self.resourcepacks)
            .cloned()
            .collect()
    }

    fn total_len(&self) -> usize {
        self.common.len() + self.client.len() + self.server.len() + self.resourcepacks.len()
    }

    fn with_cleanroom_supplements(mut self, config: &Config) -> Result<Self, BuildError> {
        for entry in cleanroom_dependency_supplements_for(config, &self.all_entries())? {
            match entry.side {
                DependencySide::Common => self.common.push(entry),
                DependencySide::Client => self.client.push(entry),
                DependencySide::Server => self.server.push(entry),
                DependencySide::Resourcepack => self.resourcepacks.push(entry),
            }
        }
        Ok(self)
    }

    fn with_cleanroom_supplements_for_target(
        mut self,
        config: &Config,
        target: BuildTarget,
    ) -> Result<Self, BuildError> {
        for entry in cleanroom_dependency_supplements_for(config, &self.all_entries())? {
            match entry.side {
                DependencySide::Common => self.common.push(entry),
                DependencySide::Client if target.includes_client() => self.client.push(entry),
                DependencySide::Server if target.includes_server() => self.server.push(entry),
                DependencySide::Resourcepack => self.resourcepacks.push(entry),
                DependencySide::Client | DependencySide::Server => {}
            }
        }
        Ok(self)
    }

    fn with_cleanroom_runtime_supplements(mut self, config: &Config) -> Result<Self, BuildError> {
        for entry in cleanroom_dependency_supplements_for(config, &self.all_entries())?
            .into_iter()
            .filter(|entry| !is_cleanroom_relauncher_entry(entry))
        {
            match entry.side {
                DependencySide::Common => self.common.push(entry),
                DependencySide::Client => self.client.push(entry),
                DependencySide::Server => self.server.push(entry),
                DependencySide::Resourcepack => self.resourcepacks.push(entry),
            }
        }
        Ok(self)
    }
}

#[derive(Debug, Clone)]
struct ResolvedLists {
    common: Vec<ResolvedEntry>,
    client: Vec<ResolvedEntry>,
    server: Vec<ResolvedEntry>,
    resourcepacks: Vec<ResolvedEntry>,
}

impl ResolvedLists {
    fn resolve<A: CurseForgeApi + Sync>(
        project_dir: &Path,
        config: &Config,
        api: &A,
        lists: &ProjectLists,
        progress: &mut (impl FnMut(&BuildProgress) + Send),
    ) -> Result<Self, BuildError> {
        Self::resolve_with_options(project_dir, config, api, lists, progress)
    }

    fn resolve_without_download_url_check<A: CurseForgeApi + Sync>(
        project_dir: &Path,
        config: &Config,
        api: &A,
        lists: &ProjectLists,
    ) -> Result<Self, BuildError> {
        Self::resolve_with_options(project_dir, config, api, lists, &mut |_| {})
    }

    fn resolve_with_options<A: CurseForgeApi + Sync>(
        project_dir: &Path,
        config: &Config,
        api: &A,
        lists: &ProjectLists,
        progress: &mut (impl FnMut(&BuildProgress) + Send),
    ) -> Result<Self, BuildError> {
        let persistent_api = PersistentCurseForgeApi::new(api, Some(project_dir));
        let api = CachedCurseForgeApi::new(&persistent_api);
        let queued_entries = queued_entries(lists);
        let total = queued_entries.len();
        progress(&BuildProgress::ResolvingDependencies { total });
        let completed = Mutex::new(0_usize);
        let progress = Mutex::new(progress);
        let resolved_entries = queued_entries
            .par_iter()
            .map(|queued| {
                let resolved = resolve_entry(config, &api, &queued.entry)?;
                let completed = {
                    let mut completed = completed
                        .lock()
                        .expect("build completed counter mutex poisoned");
                    *completed += 1;
                    *completed
                };
                {
                    let mut progress = progress.lock().expect("build progress mutex poisoned");
                    progress(&BuildProgress::ResolvedDependency {
                        completed,
                        total,
                        project_id: resolved.entry.project_id,
                        file_id: resolved.entry.file_id,
                        slug: resolved.entry.slug.clone(),
                    });
                }
                Ok(ResolvedQueuedEntry {
                    bucket: queued.bucket,
                    index: queued.index,
                    resolved,
                })
            })
            .collect::<Result<Vec<_>, BuildError>>()?;

        let mut common = vec![None; lists.common.len()];
        let mut client = vec![None; lists.client.len()];
        let mut server = vec![None; lists.server.len()];
        let mut resourcepacks = vec![None; lists.resourcepacks.len()];

        for resolved in resolved_entries {
            match resolved.bucket {
                ListBucket::Common => common[resolved.index] = Some(resolved.resolved),
                ListBucket::Client => client[resolved.index] = Some(resolved.resolved),
                ListBucket::Server => server[resolved.index] = Some(resolved.resolved),
                ListBucket::Resourcepacks => {
                    resourcepacks[resolved.index] = Some(resolved.resolved)
                }
            }
        }

        Ok(Self {
            common: unwrap_resolved_bucket("common", common)?,
            client: unwrap_resolved_bucket("client", client)?,
            server: unwrap_resolved_bucket("server", server)?,
            resourcepacks: unwrap_resolved_bucket("resourcepacks", resourcepacks)?,
        })
    }

    fn client_manifest_entries(&self) -> Vec<DependencyEntry> {
        self.common
            .iter()
            .chain(&self.client)
            .chain(&self.resourcepacks)
            .map(|resolved| resolved.entry.clone())
            .collect()
    }

    fn server_downloads(&self) -> Vec<&ResolvedEntry> {
        self.common
            .iter()
            .chain(&self.server)
            .chain(
                self.resourcepacks.iter().filter(|resolved| {
                    resolved.entry.dependency_type == DependencyType::ResourcePack
                }),
            )
            .collect()
    }

    fn into_runtime(self) -> RuntimeResolvedLists {
        RuntimeResolvedLists {
            common: self.common.into_iter().map(Into::into).collect(),
            client: self.client.into_iter().map(Into::into).collect(),
            server: self.server.into_iter().map(Into::into).collect(),
            resourcepacks: self.resourcepacks.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<ResolvedEntry> for RuntimeResolvedEntry {
    fn from(value: ResolvedEntry) -> Self {
        Self {
            entry: value.entry,
            file: value.file,
        }
    }
}

fn is_cleanroom_relauncher_entry(entry: &DependencyEntry) -> bool {
    const RELAUNCHER_PROJECTS: &[u64] = &[1_214_490, 1_214_906, 1_336_201];
    const RELAUNCHER_SLUGS: &[&str] = &[
        "cleanroom-relauncher",
        "cleanroom-relauncher-unofficial",
        "improved-cleanroom-relauncher",
    ];
    entry
        .project_id
        .is_some_and(|project_id| RELAUNCHER_PROJECTS.contains(&project_id))
        || entry.slug.as_deref().is_some_and(|slug| {
            RELAUNCHER_SLUGS
                .iter()
                .any(|candidate| slug.eq_ignore_ascii_case(candidate))
        })
}

fn queued_entries(lists: &ProjectLists) -> Vec<QueuedEntry> {
    lists
        .common
        .iter()
        .cloned()
        .enumerate()
        .map(|(index, entry)| QueuedEntry {
            bucket: ListBucket::Common,
            index,
            entry,
        })
        .chain(
            lists
                .client
                .iter()
                .cloned()
                .enumerate()
                .map(|(index, entry)| QueuedEntry {
                    bucket: ListBucket::Client,
                    index,
                    entry,
                }),
        )
        .chain(
            lists
                .server
                .iter()
                .cloned()
                .enumerate()
                .map(|(index, entry)| QueuedEntry {
                    bucket: ListBucket::Server,
                    index,
                    entry,
                }),
        )
        .chain(
            lists
                .resourcepacks
                .iter()
                .cloned()
                .enumerate()
                .map(|(index, entry)| QueuedEntry {
                    bucket: ListBucket::Resourcepacks,
                    index,
                    entry,
                }),
        )
        .collect()
}

fn unwrap_resolved_bucket(
    bucket: &'static str,
    entries: Vec<Option<ResolvedEntry>>,
) -> Result<Vec<ResolvedEntry>, BuildError> {
    entries
        .into_iter()
        .enumerate()
        .map(|(index, entry)| entry.ok_or(BuildError::ResolutionBucket { bucket, index }))
        .collect()
}

fn resolve_entry<A: CurseForgeApi + Sync>(
    config: &Config,
    api: &A,
    entry: &DependencyEntry,
) -> Result<ResolvedEntry, BuildError> {
    let loader = entry.loader.as_deref().unwrap_or("");
    let (project, file) = match (entry.project_id, entry.file_id) {
        (Some(project_id), Some(file_id)) => {
            let (project, file) = rayon::join(
                || api.get_project(project_id),
                || api.get_file(project_id, file_id),
            );
            let project = project?;
            let file = file?;
            (project, file)
        }
        (Some(project_id), None) => {
            let project = api.get_project(project_id)?;
            let files = api.list_files(project_id, &config.pack.minecraft_version, loader)?;
            let file = CandidateSelector::select_latest(&files)?;
            (project, file)
        }
        (None, _) => {
            let slug = entry
                .slug
                .as_deref()
                .ok_or_else(|| ResolveError::MissingSlug(entry.name.clone()))?;
            let project = api.find_project_by_slug(slug)?;
            let files = api.list_files(project.id, &config.pack.minecraft_version, loader)?;
            let file = CandidateSelector::select_latest(&files)?;
            (project, file)
        }
    };

    let mut locked = match DependencyEntry::from_curseforge(&project, &file) {
        Ok(entry) => entry,
        Err(DependencyError::MissingSide { .. }) => {
            DependencyEntry::from_curseforge_with_side(&project, &file, entry.side.clone())?
        }
        Err(err) => return Err(err.into()),
    };
    locked.required = entry.required;
    Ok(ResolvedEntry {
        entry: locked,
        file,
    })
}

fn plan_client_zip(
    project_dir: &Path,
    config: &Config,
    resolved: &ResolvedLists,
    cleanroom_files: Option<&GeneratedClientFiles>,
) -> Result<ArchivePlan, BuildError> {
    let mut plan = ArchivePlan::new(DEFAULT_LIMITS);
    let entries = resolved.client_manifest_entries();
    let manifest = generate_pack_manifest(config, &entries, "overrides")?;
    plan.insert(ArchiveEntry::bytes(
        "manifest.json",
        serde_json::to_vec(&manifest)?,
        Compression::Deflated,
        "generated client manifest",
    ))?;
    plan.insert(ArchiveEntry::bytes(
        "modlist.html",
        generate_modlist_html(&entries).into_bytes(),
        Compression::Deflated,
        "generated mod list",
    ))?;
    collect_layer(
        &mut plan,
        &project_dir.join("overrides/common"),
        Some("overrides"),
        "overrides/common",
        None,
        true,
    )?;
    collect_layer(
        &mut plan,
        &project_dir.join("overrides/client"),
        Some("overrides"),
        "overrides/client",
        Some("overrides/common"),
        true,
    )?;
    if let Some(cleanroom_files) = cleanroom_files {
        for generated in &cleanroom_files.files {
            let compression = if generated.store_uncompressed {
                Compression::Stored
            } else {
                Compression::Deflated
            };
            plan.insert(ArchiveEntry::bytes(
                generated.archive_path.clone(),
                generated.bytes.clone(),
                compression,
                "generated Cleanroom client file",
            ))?;
        }
    }
    collect_root_docs(&mut plan, project_dir)?;
    Ok(plan)
}

fn server_override_error(error: ArchiveError) -> BuildError {
    BuildError::ServerOverrideLayout {
        reason: error.to_string(),
    }
}

fn plan_server_zip(
    project_dir: &Path,
    config: &Config,
    resolved: &ResolvedLists,
    server_api_key: &str,
    server_installer_asset: &ServerInstallerAsset,
) -> Result<ArchivePlan, BuildError> {
    let mut plan = ArchivePlan::new(DEFAULT_LIMITS);
    collect_layer(
        &mut plan,
        &project_dir.join("overrides/common"),
        None,
        "overrides/common",
        None,
        true,
    )
    .map_err(server_override_error)?;
    collect_layer(
        &mut plan,
        &project_dir.join("overrides/server"),
        None,
        "overrides/server",
        Some("overrides/common"),
        true,
    )
    .map_err(server_override_error)?;
    let manifest = server_files_manifest(config, resolved, server_api_key)?;
    plan.insert(ArchiveEntry::bytes(
        "server-install.json",
        serde_json::to_vec(&manifest)?,
        Compression::Deflated,
        "generated server installer manifest",
    ))?;
    plan.insert(ArchiveEntry::verified_file(
        server_installer_asset.name.clone(),
        server_installer_asset.path.clone(),
        server_installer_asset.size,
        server_installer_asset.sha256.clone(),
        server_installer_asset.unix_executable,
        "MCServerDownloadTool release asset",
    ))?;
    if !plan.contains("resourcepacks/.mcmpu-keep")? {
        plan.insert(ArchiveEntry::bytes(
            "resourcepacks/.mcmpu-keep",
            Vec::new(),
            Compression::Deflated,
            "generated resource pack directory marker",
        ))?;
    }
    collect_root_docs(&mut plan, project_dir)?;
    Ok(plan)
}

fn server_files_manifest(
    config: &Config,
    resolved: &ResolvedLists,
    server_api_key: &str,
) -> Result<ServerInstallManifest, BuildError> {
    let files = resolved
        .server_downloads()
        .into_iter()
        .map(server_file)
        .collect::<Result<Vec<_>, _>>()?;
    let loader = plan_server_install_loader(
        &config.pack.loader,
        &config.pack.minecraft_version,
        &config.pack.loader_version,
    )?;
    let curseforge_api_key = files
        .iter()
        .any(|file| file.download.requires_curseforge_key())
        .then(|| server_api_key.to_string());
    let manifest = ServerInstallManifest {
        schema_version: 1,
        minecraft: ServerMinecraftConfig {
            version: config.pack.minecraft_version.clone(),
        },
        java: ServerJavaConfig::from_memory_config(config.java.version_major, &config.java.server)?,
        files,
        loader,
        curseforge_api_key,
    };
    manifest.validate()?;
    Ok(manifest)
}

fn server_file(resolved: &ResolvedEntry) -> Result<ServerFile, BuildError> {
    let project_id = resolved.entry.project_id.unwrap_or_default();
    let file_id = resolved.entry.file_id.unwrap_or_default();
    let slug = resolved
        .entry
        .slug
        .as_deref()
        .ok_or_else(|| ResolveError::MissingSlug(resolved.entry.name.clone()))?;
    // Keep the exact filename supplied by CurseForge. The server installer writes directly to
    // this manifest path, so adding IDs to the name would change the modpack's shipped filenames.
    let file_name = &resolved.file.file_name;
    let path = match resolved.entry.dependency_type {
        DependencyType::Mod => format!("mods/{file_name}"),
        DependencyType::ResourcePack => format!("resourcepacks/{file_name}"),
        DependencyType::ShaderPack => format!("shaderpacks/{file_name}"),
    };
    let sha1 = resolved
        .file
        .hashes
        .iter()
        .find(|hash| hash.algo == 1)
        .map(|hash| hash.value.clone())
        .ok_or(BuildError::MissingSha1 {
            project_id,
            file_id,
        })?;
    let download = resolved
        .file
        .download_url
        .as_deref()
        .filter(|url| resolved.file.is_available && !url.trim().is_empty())
        .map(|url| ServerFileDownload::Automatic {
            url: url.to_string(),
        })
        .unwrap_or(ServerFileDownload::Manual);
    Ok(ServerFile {
        path,
        name: resolved.entry.name.clone(),
        dependency_type: resolved.entry.dependency_type.clone(),
        download,
        project_page: resolved
            .entry
            .dependency_type
            .curseforge_file_page_url(slug, file_id),
        sha1,
        size: resolved.file.file_length,
    })
}

fn read_dependency_list(
    path: impl AsRef<Path>,
    side: DependencySide,
) -> Result<Vec<DependencyEntry>, BuildError> {
    let path = path.as_ref();
    let text = fs::read_to_string(path).map_err(|source| BuildError::ReadDependencyList {
        path: path.display().to_string(),
        source,
    })?;
    let mut entries: Vec<DependencyEntry> =
        serde_json::from_str(&text).map_err(|source| BuildError::ParseDependencyList {
            path: path.display().to_string(),
            source,
        })?;
    for entry in &mut entries {
        entry.side = side.clone();
    }
    Ok(entries)
}

fn create_dir(path: impl AsRef<Path>) -> Result<(), BuildError> {
    let path = path.as_ref();
    fs::create_dir_all(path).map_err(|source| BuildError::CreateDir {
        path: path.display().to_string(),
        source,
    })
}

fn configured_output_dir(project_dir: &Path, configured: &str) -> Result<PathBuf, BuildError> {
    let project_root =
        fs::canonicalize(project_dir).map_err(|source| BuildError::InvalidOutputDirectory {
            reason: format!(
                "failed to resolve project root '{}': {source}",
                project_dir.display()
            ),
        })?;
    let mut current = project_root.clone();
    for component in Path::new(configured).components() {
        match component {
            std::path::Component::CurDir => continue,
            std::path::Component::Normal(value) => current.push(value),
            _ => {
                return Err(BuildError::InvalidOutputDirectory {
                    reason: format!("'{configured}' is not a project-relative directory"),
                });
            }
        }
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(BuildError::InvalidOutputDirectory {
                    reason: format!("failed to inspect '{}': {source}", current.display()),
                });
            }
        };
        if metadata.file_type().is_symlink() || metadata_is_reparse_point(&metadata) {
            return Err(BuildError::InvalidOutputDirectory {
                reason: format!(
                    "configured path component '{}' is a symbolic link or reparse point",
                    current.display()
                ),
            });
        }
        if !metadata.is_dir() {
            return Err(BuildError::InvalidOutputDirectory {
                reason: format!(
                    "configured path component '{}' is not a directory",
                    current.display()
                ),
            });
        }
    }
    if !current.starts_with(&project_root) {
        return Err(BuildError::InvalidOutputDirectory {
            reason: format!(
                "'{configured}' escapes project root '{}'",
                project_root.display()
            ),
        });
    }
    Ok(current)
}

fn metadata_is_reparse_point(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        let _ = metadata;
        false
    }
}
